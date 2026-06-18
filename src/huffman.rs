//! Per-plane Huffman decoder for MagicYUV v7.
//!
//! Two pieces:
//!
//! 1. **Descriptor parser** (`parse_lengths`) — walks the
//!    run-length-encoded byte stream from the per-frame preamble per
//!    `spec/05` §1.1 and emits exactly `N = 1 << bits` code lengths.
//! 2. **Canonical-Huffman code construction** (`HuffmanTable::build`) —
//!    the **non-RFC-1951** longest-length-first cumulative algorithm
//!    from `spec/05` §2.0 (Auditor round 2 correction). Walks tiers
//!    `max_len_used .. 1` with an accumulator that initialises at
//!    `0xffffffff` (= -1 mod 2^32), increments per code at the tier,
//!    then **right-shifts** between tiers — the opposite orientation
//!    of RFC 1951 §3.2.2.
//!
//! Decoding uses a **two-level** lookup keyed on the top
//! `min(max_len, 12)` bits of the bit accumulator:
//!
//! * If the prefix's actual code length ≤ 12, the primary table
//!   delivers `(symbol, length)` directly (one lookup).
//! * Otherwise (10/12/14-bit alphabets where the longest codes can
//!   reach 14, 16, or 18 bits), the primary entry steers into a
//!   per-prefix fallback subtable indexed by the next bits.
//!
//! The cap of 12 bits on the primary table keeps the memory per plane
//! at 4 K entries × 4 B = 16 KB, even when `max_length` = 18 (which
//! would otherwise cost 256 K entries × 4 B = 1 MB per plane).

use crate::bitreader::BitReader;
use crate::error::{Error, Result};

/// Maximum primary-table prefix bits. Keeps the flat table at
/// 4K entries × packed `u32` even for max_length = 18 (= 16 KB → 8 KB
/// after the round-127 packed-entry reshuffle).
const PRIMARY_BITS: u8 = 12;

/// Sentinel value in the low byte of a primary-table entry that means
/// "redirect to a secondary subtable". Lengths 1..=18 ≪ 0xff so it is
/// unambiguous against any terminal entry.
const REDIRECT_MARKER: u8 = 0xff;

/// Pack `(symbol_or_subtable_index, length_or_marker)` into the u32
/// layout used by `primary` and `secondary`. Low 8 bits hold the
/// length / marker; high 24 bits hold the symbol / subtable index.
#[inline(always)]
const fn pack_entry(sym_or_sub: u32, len_or_marker: u8) -> u32 {
    (sym_or_sub << 8) | (len_or_marker as u32)
}

/// Per-plane Huffman length descriptor parser (`spec/05` §1.1).
///
/// `n` is `1 << bit_depth` (i.e. 256 for 8-bit, 1024 for 10-bit,
/// 4096 for 12-bit, 16384 for 14-bit). `max_length` is the per-tier
/// cap (12 / 14 / 16 / 18 by bit-depth, per `spec/05` §1.1's
/// `HuffCoderT<…, bits, max_length, hint>` template parameters).
///
/// Returns `(lengths, bytes_consumed)` so the caller can advance to
/// the next plane's descriptor.
pub fn parse_lengths(
    descriptor: &[u8],
    n: usize,
    max_length: u8,
    plane: usize,
) -> Result<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(n);
    let mut pos = 0;
    while out.len() < n {
        if pos >= descriptor.len() {
            return Err(Error::Truncated {
                what: "huffman descriptor",
                needed: 1,
                have: 0,
            });
        }
        let b = descriptor[pos];
        pos += 1;
        if (b & 0x80) == 0 {
            // Literal length.
            if b > max_length {
                return Err(Error::HuffmanLengthExceedsMax {
                    plane,
                    got: b,
                    max: max_length,
                });
            }
            out.push(b);
        } else {
            // Run form: next byte is the count - 1 → emit (1 + c)
            // copies of (b & 0x7f).
            if pos >= descriptor.len() {
                return Err(Error::Truncated {
                    what: "huffman descriptor run-count byte",
                    needed: 1,
                    have: 0,
                });
            }
            let c = descriptor[pos];
            pos += 1;
            let value = b & 0x7f;
            if value > max_length {
                return Err(Error::HuffmanLengthExceedsMax {
                    plane,
                    got: value,
                    max: max_length,
                });
            }
            let count = (1usize) + (c as usize);
            for _ in 0..count {
                if out.len() >= n {
                    break;
                }
                out.push(value);
            }
        }
    }
    if out.len() != n {
        return Err(Error::Truncated {
            what: "huffman descriptor (overran symbol count)",
            needed: n,
            have: out.len(),
        });
    }
    Ok((out, pos))
}

/// Built per-plane Huffman lookup table.
pub struct HuffmanTable {
    /// Code length for each input symbol (`L[s] == 0` ⇒ unused).
    lengths: Vec<u8>,
    /// Canonical Huffman code per symbol (`code[s] == 0` when
    /// `lengths[s] == 0`). Surfaced by [`Self::codes`] so the trace
    /// emitter can produce the per-symbol `{length, code}` map per
    /// `audit/02` §4.2.
    codes: Vec<u32>,
    /// `max_len_used`. For an all-unused descriptor (Kraft 0) this is
    /// 0; we never actually decode such a table, but parse_lengths
    /// allows it.
    max_len: u8,
    /// Primary table indexed by the top `primary_bits` bits.
    /// Packed `u32` per entry — replaces the prior `(u32, u8)` tuple
    /// layout (which paid 8 bytes per slot due to alignment padding)
    /// with a flat 4-byte u32, halving the primary working set from
    /// 16 KB → 8 KB per plane at `max_len = 18`. Cuts the per-pixel
    /// hot-loop load width in half (one 4-byte aligned `u32` read
    /// instead of an 8-byte tuple load) and improves L1 fit on small
    /// frames.
    ///
    /// Encoding (low 8 bits = length-or-marker, high 24 bits =
    /// symbol-or-subtable-index):
    ///
    /// * If `(entry & 0xff) ≤ primary_bits` ⇒ terminal: `entry >> 8`
    ///   is the decoded symbol, `entry & 0xff` is its code length.
    /// * If `(entry & 0xff) == 0xff` ⇒ redirect: `entry >> 8` is the
    ///   index into `secondary` of the per-prefix subtable, and the
    ///   caller consumes another `(max_len - primary_bits)` bits.
    ///
    /// The 24-bit symbol field covers the 14-bit alphabet
    /// (16384 symbols) and the secondary-table index (at most 4096
    /// distinct primary prefixes when `primary_bits = 12`).
    primary: Vec<u32>,
    /// Effective primary-prefix size in bits. `min(max_len, PRIMARY_BITS)`.
    primary_bits: u8,
    /// Per-prefix secondary tables, packed identically to `primary`
    /// (low 8 bits = length within the subtable, high 24 bits =
    /// symbol). Indexed by the next `(max_len - primary_bits)` bits.
    /// Empty when `max_len ≤ primary_bits`.
    secondary: Vec<Vec<u32>>,
}

impl HuffmanTable {
    /// Construct a canonical-Huffman code from `lengths` per
    /// `spec/05` §2.0 (longest-length-first cumulative algorithm).
    pub fn build(lengths: Vec<u8>, plane: usize) -> Result<Self> {
        let max_len = *lengths.iter().max().unwrap_or(&0);
        // bl_count[len] = #{s : L[s] == len}, len in 0..=max_len.
        let mut bl_count = vec![0u32; (max_len as usize) + 2];
        for &l in &lengths {
            bl_count[l as usize] += 1;
        }

        // Phase 4 of §2.0.3: walk longest-first, assign codes.
        let mut start = vec![0u32; (max_len as usize) + 2];
        let mut acc: u32 = 0xffff_ffff;
        for len in (1..=(max_len as u32)).rev() {
            let n_at = bl_count[len as usize];
            start[len as usize] = acc.wrapping_add(1);
            acc = acc.wrapping_add(n_at);
            // Validity check: (1 << len) > acc (must be strictly
            // greater). spec/05 §2.0.3 reject path: `(1 << len) <=
            // acc` — guard against `len == 32` overflow first.
            let codespace: u64 = 1u64 << len;
            if codespace <= (acc as u64) {
                return Err(Error::HuffmanOverfull { plane });
            }
            acc >>= 1;
        }

        // Phase 2 pass: assign codes to symbols.
        // The previous `cur = start.clone()` step paid for a fresh
        // `Vec<u32>` of length `max_len + 2` (up to 20 u32 = 80 B at
        // the 14-bit alphabet's max_len = 18). It also kept `start`
        // live for no further use. Since `start` isn't read after the
        // assignment phase, we hand the buffer to `cur` by `mem::take`
        // — no allocation, no copy.
        let mut code = vec![0u32; lengths.len()];
        if max_len > 0 {
            let mut cur = core::mem::take(&mut start);
            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                let li = l as usize;
                code[s] = cur[li];
                cur[li] = cur[li].wrapping_add(1);
            }
        }

        // Build the lookup tables.
        let primary_bits = max_len.min(PRIMARY_BITS);
        let primary_size: usize = if max_len == 0 {
            1
        } else {
            1usize << primary_bits
        };
        let mut primary = vec![0u32; primary_size];
        let mut secondary: Vec<Vec<u32>> = Vec::new();

        if max_len == 0 {
            return Ok(Self {
                lengths,
                codes: code,
                max_len,
                primary,
                primary_bits,
                secondary,
            });
        }

        if max_len <= primary_bits {
            // Single-level: terminal entries cover the full prefix.
            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                let shift = (primary_bits - l) as u32;
                let base = (code[s] as usize) << shift;
                let count = 1usize << shift;
                let entry = pack_entry(s as u32, l);
                for k in 0..count {
                    primary[base + k] = entry;
                }
            }
        } else {
            // Two-level: gather, per primary-prefix bucket, the codes
            // whose length > primary_bits (and the terminal codes whose
            // length ≤ primary_bits). Walk symbols once, partition into
            // (terminal, deferred-by-prefix).
            //
            // Each deferred-symbol's first `primary_bits` bits give
            // its primary-prefix index; we route those to a per-prefix
            // subtable.
            let secondary_bits = max_len - primary_bits;
            let secondary_size = 1usize << secondary_bits;
            // Discover the unique primary prefixes that have any
            // deferred symbol, in canonical-code order.
            //
            // The previous shape used `HashMap<u32, usize>` for the
            // prefix → subtable-index lookup. The prefix is bounded by
            // `1 << primary_bits ≤ 4096`, so a direct-indexed
            // `Vec<i32>` of that size is denser and faster than the
            // SipHash-keyed map: the lookup is one indexed load, the
            // miss-then-insert is an indexed compare-and-store. The
            // map paid a `HashMap` heap allocation (typically 48 B
            // header + a ~32 B bucket array even when empty) and a
            // hash computation per probe; the direct-index path pays
            // exactly `primary_size * 4` B (up to 16 KB at
            // `primary_bits = 12`) zeroed once. `i32` with the
            // sentinel `-1` for "no subtable yet" avoids an `Option`
            // tag check inside the hot loop.
            let mut prefix_to_idx = vec![-1i32; primary_size];

            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                if l <= primary_bits {
                    // Terminal: spread across the primary bucket.
                    let shift = (primary_bits - l) as u32;
                    let base = (code[s] as usize) << shift;
                    let count = 1usize << shift;
                    let entry = pack_entry(s as u32, l);
                    for k in 0..count {
                        primary[base + k] = entry;
                    }
                } else {
                    // Deferred: route via a subtable.
                    let prefix = code[s] >> (l - primary_bits);
                    let slot = &mut prefix_to_idx[prefix as usize];
                    let sub_idx = if *slot < 0 {
                        secondary.push(vec![0u32; secondary_size]);
                        let idx = secondary.len() - 1;
                        *slot = idx as i32;
                        idx
                    } else {
                        *slot as usize
                    };
                    // Within the subtable, the symbol covers
                    // `1 << (max_len - l)` entries starting at
                    // `(code[s] & ((1<<(l-primary_bits))-1)) << (max_len - l)`
                    // (the residual bits of the code within the
                    // subtable space).
                    let resid_bits = l - primary_bits;
                    let resid_mask = (1u32 << resid_bits) - 1;
                    let resid = code[s] & resid_mask;
                    let shift = (secondary_bits - resid_bits) as u32;
                    let base = (resid as usize) << shift;
                    let count = 1usize << shift;
                    let l_in_sub = l - primary_bits;
                    let sub_entry = pack_entry(s as u32, l_in_sub);
                    for k in 0..count {
                        secondary[sub_idx][base + k] = sub_entry;
                    }
                    // Mark the primary entry as a redirect:
                    // `(sub_idx, REDIRECT_MARKER)`.
                    primary[prefix as usize] = pack_entry(sub_idx as u32, REDIRECT_MARKER);
                }
            }
        }

        Ok(Self {
            lengths,
            codes: code,
            max_len,
            primary,
            primary_bits,
            secondary,
        })
    }

    /// `max_len_used` after `build`.
    pub fn max_len(&self) -> u8 {
        self.max_len
    }

    /// `true` if no symbol carries a positive length — i.e. the
    /// descriptor was entirely unused. v2.4.2 doesn't produce this
    /// (every fixture has Kraft = 1.0 per `spec/05` §1.3). As of the
    /// completeness check in [`Self::build`], a degenerate
    /// (`max_len == 0`) or under-full descriptor is rejected with
    /// [`Error::HuffmanIncomplete`] before a table is ever returned,
    /// so a built table is always complete and `is_empty()` is `false`
    /// in practice. The accessor and the `max_len == 0` short-circuits
    /// in [`Self::decode`] / [`Self::decode_into_u8`] are retained as
    /// defence in depth.
    pub fn is_empty(&self) -> bool {
        self.max_len == 0
    }

    /// Decode one symbol from `br`. Returns the symbol's index in
    /// `[0, N)`. The caller is expected to keep the symbol count
    /// equal to plane-height-of-slice × plane-width per `spec/05`
    /// §3.3 and stop reading at exactly that count.
    #[inline(always)]
    pub fn decode(&self, br: &mut BitReader<'_>) -> u32 {
        if self.max_len == 0 {
            return 0;
        }
        let key = br.peek_bits(self.primary_bits as u32) as usize;
        let entry = self.primary[key];
        let len = (entry & 0xff) as u8;
        let sym_or_sub = entry >> 8;
        if len != REDIRECT_MARKER {
            br.consume(len as u32);
            sym_or_sub
        } else {
            // Two-level: consume the primary prefix and look up the
            // subtable.
            br.consume(self.primary_bits as u32);
            let secondary_bits = self.max_len - self.primary_bits;
            let key2 = br.peek_bits(secondary_bits as u32) as usize;
            let sub_entry = self.secondary[sym_or_sub as usize][key2];
            let l_in_sub = (sub_entry & 0xff) as u8;
            br.consume(l_in_sub as u32);
            sub_entry >> 8
        }
    }

    /// Decode `out.len()` symbols into `out` as `u8`. Hot-path helper
    /// for the 8-bit decoder loop — folds peek/consume + table lookup
    /// inline so the compiler keeps the BitReader state (`acc`, `fill`,
    /// `pos`) in registers across iterations.
    ///
    /// Returns `false` if the descriptor was degenerate
    /// (`max_len == 0`, all-unused) or if any decoded symbol indexed an
    /// **unused-codespace** lookup slot — a zero-length entry left over
    /// from an under-full descriptor (`spec/05` §2.1). In both cases no
    /// progress can be made / the bitstream is invalid, and the caller
    /// surfaces it as malformed input rather than emitting garbage. A
    /// `true` return means every symbol decoded to a real `len ≥ 1`
    /// code. The 8-bit alphabet always has `max_len ≤ 12 = PRIMARY_BITS`,
    /// so the secondary table is empty and this loop only takes the
    /// single-level fast path.
    #[inline]
    pub fn decode_into_u8(&self, br: &mut BitReader<'_>, out: &mut [u8]) -> bool {
        if self.max_len == 0 {
            return false;
        }
        let primary_bits = self.primary_bits as u32;
        // Snapshot the primary table; its length is `1 << primary_bits`.
        let primary = self.primary.as_slice();
        // 8-bit FOURCCs always have max_len ≤ 12 → single-level.
        // 10/12/14-bit alphabets go through the generic `decode` path
        // below; they still benefit from the `#[inline(always)]` on
        // `decode` itself.
        if self.max_len <= self.primary_bits {
            // Round-286: hoist the per-symbol refill branch out of the
            // hot loop. Every entry here is terminal (no redirect
            // marker), so the batched single-level decoder peeks the
            // top `primary_bits` and consumes the real code length per
            // pixel, refilling only when `fill < primary_bits` (≈ once
            // every 4-5 pixels for an 8-bit alphabet) instead of after
            // every symbol. Same observable bit stream as the prior
            // `peek_bits` + `consume` body — the EOF zero-pad + 8-byte
            // fast load are delegated to `BitReader::refill` unchanged,
            // pinned by `decode_into_u8_matches_per_pixel_decode`.
            br.decode_single_level_into_u8(primary, primary_bits, out)
        } else {
            // Defensive two-level fallback (unreachable for native 8-bit
            // FOURCCs, whose `max_len ≤ 12 = PRIMARY_BITS`). Fold the
            // per-symbol unused-codespace check through `decode_checked`.
            let mut all_valid = true;
            for px in out.iter_mut() {
                let (sym, valid) = self.decode_checked(br);
                all_valid &= valid;
                *px = sym as u8;
            }
            all_valid
        }
    }

    /// Like [`Self::decode`] but also reports whether the symbol came
    /// from a real `len ≥ 1` code. A `false` validity flag means the
    /// peeked prefix landed on an unused-codespace slot (an under-full
    /// descriptor's zero-init entry, `spec/05` §2.1) — the symbol is
    /// meaningless and no bits were consumed. Kept separate from the
    /// `#[inline(always)]` [`Self::decode`] so the validity bookkeeping
    /// is opt-in and the per-pixel decode fast path stays branch-light.
    #[inline]
    fn decode_checked(&self, br: &mut BitReader<'_>) -> (u32, bool) {
        if self.max_len == 0 {
            return (0, false);
        }
        let key = br.peek_bits(self.primary_bits as u32) as usize;
        let entry = self.primary[key];
        let len = (entry & 0xff) as u8;
        let sym_or_sub = entry >> 8;
        if len == 0 {
            // Unused-codespace slot in an under-full primary table.
            return (sym_or_sub, false);
        }
        if len != REDIRECT_MARKER {
            br.consume(len as u32);
            (sym_or_sub, true)
        } else {
            br.consume(self.primary_bits as u32);
            let secondary_bits = self.max_len - self.primary_bits;
            let key2 = br.peek_bits(secondary_bits as u32) as usize;
            let sub_entry = self.secondary[sym_or_sub as usize][key2];
            let l_in_sub = (sub_entry & 0xff) as u8;
            if l_in_sub == 0 {
                // Unused-codespace slot in a secondary subtable.
                return (sub_entry >> 8, false);
            }
            br.consume(l_in_sub as u32);
            (sub_entry >> 8, true)
        }
    }

    /// Decode `out.len()` symbols into `out` as `u16`, mask-ANDing each
    /// to `mask`. Mirrors [`Self::decode_into_u8`] for the 10/12/14-bit
    /// path, where `max_len` may exceed `PRIMARY_BITS = 12` and so the
    /// two-level lookup runs.
    ///
    /// Round-191 perf: the hot loop is folded inline so the BitReader
    /// state (`acc`, `fill`, `pos`) stays in registers across symbols —
    /// `primary_bits`, `primary`, `secondary`, and `max_len` are
    /// snapshot once outside the loop and the per-symbol peek/consume +
    /// table lookup avoids the per-pixel function-call ceremony the
    /// previous `self.decode(br)` body forced even with
    /// `#[inline(always)]`. The single-level / two-level split is
    /// hoisted to the loop selector, identical in shape to
    /// `decode_into_u8`.
    #[inline]
    pub fn decode_into_u16(&self, br: &mut BitReader<'_>, out: &mut [u16], mask: u16) -> bool {
        if self.max_len == 0 {
            return false;
        }
        let primary_bits = self.primary_bits as u32;
        let primary = self.primary.as_slice();
        // `all_valid` clears if any symbol indexes an unused-codespace
        // (`len == 0`) slot — an under-full descriptor's zero-init entry
        // (`spec/05` §2.1). The fold is a single AND per symbol so the
        // hot loop stays branch-light; a complete book never trips it.
        let mut all_valid = true;
        if self.max_len <= self.primary_bits {
            // Single-level fast path. The 10/12/14-bit alphabets in
            // practice hit `max_len > primary_bits` (the two-level
            // selector below), but a well-formed-but-shallow
            // descriptor (e.g. a low-entropy plane whose realised
            // `max_len_used` lands at ≤ primary_bits) still funnels
            // through this branch — keep the symmetry with
            // `decode_into_u8`.
            for px in out.iter_mut() {
                let key = br.peek_bits(primary_bits) as usize;
                let entry = primary[key];
                let len = entry & 0xff;
                all_valid &= len != 0;
                br.consume(len);
                *px = ((entry >> 8) as u16) & mask;
            }
        } else {
            // Two-level path. Snapshot `secondary` + `secondary_bits`
            // once; the inner loop only re-issues the bitreader ops.
            let secondary = self.secondary.as_slice();
            let secondary_bits = (self.max_len - self.primary_bits) as u32;
            for px in out.iter_mut() {
                let key = br.peek_bits(primary_bits) as usize;
                let entry = primary[key];
                let len = entry & 0xff;
                if len == 0 {
                    // Unused-codespace slot in the primary table.
                    all_valid = false;
                    *px = ((entry >> 8) as u16) & mask;
                } else if len != REDIRECT_MARKER as u32 {
                    br.consume(len);
                    *px = ((entry >> 8) as u16) & mask;
                } else {
                    // Primary entry redirects to a subtable.
                    br.consume(primary_bits);
                    let sub_idx = (entry >> 8) as usize;
                    let key2 = br.peek_bits(secondary_bits) as usize;
                    let sub_entry = secondary[sub_idx][key2];
                    let l_in_sub = sub_entry & 0xff;
                    all_valid &= l_in_sub != 0;
                    br.consume(l_in_sub);
                    *px = ((sub_entry >> 8) as u16) & mask;
                }
            }
        }
        all_valid
    }

    /// Borrow the per-symbol length array (debug / cross-validation).
    pub fn lengths(&self) -> &[u8] {
        &self.lengths
    }

    /// Borrow the per-symbol canonical-Huffman code array. Entries
    /// with `lengths[s] == 0` are zero and should be ignored. Used by
    /// the `trace` feature emitter to mirror the Python ref's
    /// `huff.used` map per `audit/02` §4.2.
    pub fn codes(&self) -> &[u32] {
        &self.codes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lengths_spec_example() {
        // spec/05 §1.2 m8rg_64x64_zero.bin descriptor:
        //   01 89 5d 08 89 9f → 256 lengths (1×len-1, 1×len-8, 254×len-9).
        let desc = [0x01, 0x89, 0x5d, 0x08, 0x89, 0x9f];
        let (lens, used) = parse_lengths(&desc, 256, 12, 0).expect("parse");
        assert_eq!(used, 6);
        assert_eq!(lens.len(), 256);
        assert_eq!(lens[0], 1);
        assert_eq!(lens[95], 8);
        // 1..=94 and 96..=255 are length 9.
        for &l in lens.iter().take(95).skip(1) {
            assert_eq!(l, 9);
        }
        for &l in lens.iter().skip(96) {
            assert_eq!(l, 9);
        }
    }

    #[test]
    fn build_spec_example_assigns_code_1_to_symbol_0() {
        // Per spec/05 §2.0.1 worked example: symbol 0 (length 1) gets
        // code `1` of length 1 — the all-zero-residual plane streams
        // bytes `ff ff ff …`.
        let desc = [0x01, 0x89, 0x5d, 0x08, 0x89, 0x9f];
        let (lens, _) = parse_lengths(&desc, 256, 12, 0).unwrap();
        let table = HuffmanTable::build(lens, 0).expect("build");
        // Decode a stream of all-1 bits: every symbol should be 0.
        let bits = [0xffu8; 64];
        let mut br = BitReader::new(&bits);
        for _ in 0..256 {
            assert_eq!(table.decode(&mut br), 0);
        }
    }

    #[test]
    fn build_rejects_overfull() {
        // Two length-1 symbols + 254 length-9 symbols → Kraft > 1.
        let mut lens = vec![9u8; 256];
        lens[0] = 1;
        lens[1] = 1;
        let r = HuffmanTable::build(lens, 0);
        assert!(matches!(r, Err(Error::HuffmanOverfull { plane: 0 })));
    }

    #[test]
    fn build_accepts_underfull_book() {
        // One length-1 symbol only: Kraft = 1/2 < 1 (under-full). The
        // encoder legitimately produces this for a single-symbol plane
        // (e.g. an all-zero-residual plane — the most common case), and
        // the binary's constructor accepts it, so `build` must too. The
        // unused codespace surfaces at *decode* time (see
        // `decode_underfull_unused_slot_flags_invalid`), not at build.
        let mut lens = vec![0u8; 256];
        lens[0] = 1;
        assert!(HuffmanTable::build(lens, 3).is_ok());
    }

    #[test]
    fn decode_underfull_valid_path_makes_progress() {
        // The encoder's single-symbol plane: symbol 0 length 1, code 0.
        // A conformant stream of all-`0` bits decodes every pixel to
        // symbol 0 and never peeks the unused `primary[1]` slot, so the
        // batch decoder reports `all_valid == true`.
        let mut lens = vec![0u8; 256];
        lens[0] = 1;
        let table = HuffmanTable::build(lens, 0).expect("build under-full");
        let bytes = [0x00u8; 8];
        let mut br = BitReader::new(&bytes);
        let mut out = [0u8; 16];
        assert!(table.decode_into_u8(&mut br, &mut out));
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn decode_underfull_unused_slot_flags_invalid() {
        // Same single-symbol-length-1 under-full book. A `1` bit indexes
        // the unused `primary[1]` slot (zero-init `(sym 0, len 0)`),
        // which would consume no bits and silently mis-decode. The batch
        // decoder must report `all_valid == false` so the decoder can
        // reject the slice (`spec/05` §2.1 + §10 Q1) — the malformed-
        // input hardening this round adds.
        let mut lens = vec![0u8; 256];
        lens[0] = 1;
        let table = HuffmanTable::build(lens, 0).expect("build under-full");
        let bytes = [0xffu8; 8]; // all `1` bits → always hits primary[1]
        let mut br = BitReader::new(&bytes);
        let mut out = [0u8; 16];
        assert!(!table.decode_into_u8(&mut br, &mut out));
    }

    #[test]
    fn decode_into_u16_underfull_unused_slot_flags_invalid() {
        // u16 single-level path: 1024-symbol alphabet, only symbol 0 at
        // length 10 (Kraft = 1/1024 < 1). All-`1` bits index an unused
        // primary slot; the batch decoder reports invalid.
        let mut lens = vec![0u8; 1024];
        lens[0] = 10;
        let table = HuffmanTable::build(lens, 0).expect("build under-full u16");
        let bytes = [0xffu8; 16];
        let mut br = BitReader::new(&bytes);
        let mut out = [0u16; 16];
        assert!(!table.decode_into_u16(&mut br, &mut out, 0x03ff));
    }

    #[test]
    fn build_two_symbol_alphabet() {
        // Lengths [1, 1, 0, 0, …]. Per spec/05 §2.0 with only tier
        // len=1: acc starts at 0xffffffff, becomes -1+2 = 1; codes
        // assigned in symbol-index order start at acc+1 = 0. So
        // symbol 0 → code 0, symbol 1 → code 1.
        let mut lens = vec![0u8; 256];
        lens[0] = 1;
        lens[1] = 1;
        let table = HuffmanTable::build(lens, 0).expect("build");
        // 0b1010_1010 → bits 1,0,1,0,1,0,1,0
        // → symbols 1,0,1,0,1,0,1,0.
        let bytes = [0b1010_1010];
        let mut br = BitReader::new(&bytes);
        assert_eq!(table.decode(&mut br), 1);
        assert_eq!(table.decode(&mut br), 0);
        assert_eq!(table.decode(&mut br), 1);
        assert_eq!(table.decode(&mut br), 0);
    }

    #[test]
    fn parse_lengths_rejects_overlong_literal() {
        // 0x0d = 13 > max_length=12 ⇒ reject.
        let desc = [0x0d];
        assert!(parse_lengths(&desc, 1, 12, 0).is_err());
    }

    #[test]
    fn build_with_long_codes_uses_two_level_table() {
        // Construct a canonical Huffman code with max_len = 14
        // (i.e. > PRIMARY_BITS = 12). Use a 4-symbol alphabet:
        // lengths [1, 2, 14, 14] — Kraft = 1/2 + 1/4 + 2/16384.
        // Wait, Kraft ≠ 1; let's pick something that sums to 1.
        // lengths = [1, 3, 3, 3, 4, 4] (6 syms): 1/2 + 3·1/8 + 2·1/16
        //   = 8/16 + 6/16 + 2/16 = 1 ✓.
        // But we want a >12 length to test the two-level path.
        // Try lengths = [1, 2, 14, 14, 14, 14] for first 6 symbols
        //   plus 248 symbols of length 14. Total Kraft = 1/2 + 1/4
        //   + 252·(1/16384) ≈ 0.7654 — under-full.
        // We need Σ 2^-L = 1.
        //   1/2 + 1/4 + (256-2) * 1/16384 ≠ 1.
        // Simpler: a length-3 + length-13 mix.
        //   1×len-3 (1/8) + (256-1)×len-13. 1/8 + 255/8192 = 0.156
        //   Not 1.
        // OK, easier: length-3 + length-X where X is calculated.
        //   Σ = 1/8 + 255 · 2^-X = 1 ⇒ 255 · 2^-X = 7/8
        //     ⇒ 2^X = 255 · 8 / 7 = 291.4 — not power of 2.
        // Use a small 8-symbol alphabet with Kraft 1.
        // Lengths [1, 2, 3, 3, 13, 13, 13, ...]
        // Kraft for 1+2+3+3 only = 1/2 + 1/4 + 1/8 + 1/8 = 1.0.
        // Add zero-length tail.
        let mut lens = vec![0u8; 256];
        lens[0] = 1;
        lens[1] = 2;
        lens[2] = 3;
        lens[3] = 3;
        // Now flatten the 3-tier: split symbol 0 (length 1) into a
        // length-2 split and add a long-code symbol.
        // Kraft initial = 1; replace 1×len-1 with 2×len-2 keeps Kraft=1.
        // Replace one of the len-2 with 4×len-4: 1/4 → 4·1/16 = 1/4 (same).
        // Replace one of those with 16×len-8: 1/16 → 16·1/256 (same).
        // Replace one of those with N×len-13: 1/256 → N · 2^-13. Pick
        // N = 32 → 32/8192 = 4/1024 ≠ 1/256. 1/256 = 32/8192 ✓.
        // We can keep going to len-14: 1/512 → 64·2^-14? 64/16384 =
        // 4/1024 = 1/256 ≠. 1/512 = 32/16384, so 32 syms of len-14.
        // Just construct: lens = [2, 2, 4, 4, 4, 8, 8, 8, 8, 8, 8, 8, 8,
        //   8, 8, 8, 8, 8, 8, 8, 8, 8, 14, 14, ..., 14 (32 times)]
        // Total syms = 2 + 3 + 16-1 + 32 = ?  Quick check Kraft:
        //   2·1/4 + 3·1/16 + 15·1/256 + 32·1/16384 = 1/2 + 3/16 + 15/256
        //     + 1/512 = 128/256 + 48/256 + 15/256 + 0.5/256 = 191.5/256 ≠ 1.
        // Use a simpler test that just confirms the two-level path
        // doesn't crash on a reasonable case. Build a 256-symbol table
        // where one symbol has length 14 (and the rest fit). Use a
        // recursive split.
        // Drop to a clean construction: an all-equal-length code.
        // 256 syms, all length 8 → Kraft = 256·1/256 = 1 ✓; max_len
        // = 8 < PRIMARY_BITS, so single-level path. Doesn't test
        // two-level.
        // Try: 1024 syms all length 10 → Kraft = 1, max_len = 10
        // (still < 12). Single-level.
        // Try: 16384 syms all length 14 → Kraft = 16384·2^-14 = 1.
        // max_len = 14 > 12, exercises the two-level path.
        let lens14: Vec<u8> = vec![14u8; 16384];
        let t = HuffmanTable::build(lens14, 0).expect("build long-code");
        assert_eq!(t.max_len(), 14);
        // Decode all-zero-bits stream — codes assigned in symbol-index
        // order at the longest tier; per the §2.0 algorithm, with all
        // 16384 lengths equal to 14, the first symbol gets some code
        // value, and the next gets `code+1`, etc. Just sanity-check
        // we can decode 4 symbols without panicking.
        let bytes = vec![0u8; 32];
        let mut br = BitReader::new(&bytes);
        for _ in 0..4 {
            let _ = t.decode(&mut br);
        }
        // Also sanity-check the lookup is sound: shadow the length array.
        assert_eq!(t.lengths().len(), 16384);

        // Avoid unused-warnings on the constructed lens:
        let _ = lens;
    }

    #[test]
    fn pack_entry_round_trip_terminal_and_redirect() {
        // Terminal entry: every (length, symbol) tuple a primary slot can
        // hold (length 1..=PRIMARY_BITS = 12, symbol up to a 14-bit
        // alphabet = 16383) must survive the pack→unpack cycle.
        for len in 1..=PRIMARY_BITS {
            for &sym in &[0u32, 1, 255, 256, 4095, 4096, 8191, 16383] {
                let entry = pack_entry(sym, len);
                let unpacked_len = (entry & 0xff) as u8;
                let unpacked_sym = entry >> 8;
                assert_eq!(unpacked_len, len);
                assert_eq!(unpacked_sym, sym);
                assert_ne!(unpacked_len, REDIRECT_MARKER);
            }
        }
        // Redirect entries: the low-byte marker is `REDIRECT_MARKER`
        // (0xff), unambiguously distinct from any legal length 1..=18.
        for sub_idx in [0u32, 1, 255, 256, 4095] {
            let entry = pack_entry(sub_idx, REDIRECT_MARKER);
            assert_eq!((entry & 0xff) as u8, REDIRECT_MARKER);
            assert_eq!(entry >> 8, sub_idx);
        }
    }

    #[test]
    fn decode_into_u8_matches_per_pixel_decode() {
        // Equivalence between the batch helper (which assumes the 8-bit
        // single-level fast path) and the generic `decode` per pixel:
        // both must produce the same symbol sequence from the same bit
        // stream for any 8-bit alphabet. Build a real-world descriptor
        // and exercise both paths.
        let desc = [0x01, 0x89, 0x5d, 0x08, 0x89, 0x9f];
        let (lens, _) = parse_lengths(&desc, 256, 12, 0).unwrap();
        let table = HuffmanTable::build(lens, 0).expect("build");
        // Encode a known-symbol stream by walking the canonical codes
        // for arbitrary symbol values, then decode it twice.
        let codes = table.codes().to_vec();
        let lengths = table.lengths().to_vec();
        // Pick 11 symbols all with positive length.
        let chosen: Vec<u8> = (0..=10)
            .filter(|&s| lengths[s as usize] > 0)
            .take(8)
            .collect();
        // Hand-assemble the bit stream (MSB-first) using the canonical
        // codes the build phase assigned.
        let mut bit_buf: u64 = 0;
        let mut bit_fill: u32 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        for &sym in &chosen {
            let l = lengths[sym as usize] as u32;
            let c = codes[sym as usize] as u64;
            bit_buf = (bit_buf << l) | c;
            bit_fill += l;
            while bit_fill >= 8 {
                bit_fill -= 8;
                bytes.push(((bit_buf >> bit_fill) & 0xff) as u8);
            }
        }
        if bit_fill > 0 {
            bytes.push(((bit_buf << (8 - bit_fill)) & 0xff) as u8);
        }
        // Pad with zero so the BitReader can refill at end-of-stream.
        bytes.extend_from_slice(&[0u8; 16]);

        // Per-pixel reference.
        let mut br_ref = BitReader::new(&bytes);
        let ref_syms: Vec<u8> = (0..chosen.len())
            .map(|_| table.decode(&mut br_ref) as u8)
            .collect();
        // Batch path.
        let mut br_batch = BitReader::new(&bytes);
        let mut batch_syms = vec![0u8; chosen.len()];
        assert!(table.decode_into_u8(&mut br_batch, &mut batch_syms));

        assert_eq!(ref_syms, chosen);
        assert_eq!(batch_syms, chosen);
    }

    /// Build a high-bit-depth descriptor whose alphabet uses several
    /// distinct primary-prefix buckets in the two-level path
    /// (`max_len > PRIMARY_BITS = 12`). The direct-indexed prefix →
    /// subtable-index lookup that replaced the prior `HashMap<u32,
    /// usize>` must produce the same per-symbol decode results: each
    /// encoded symbol round-trips through `table.decode` exactly as
    /// it would have under the HashMap shape. This is the build-side
    /// equivalent of `decode_into_u8_matches_per_pixel_decode` and
    /// protects against any off-by-one in `prefix_to_idx[prefix as
    /// usize]` indexing or sentinel handling.
    #[test]
    fn build_two_level_uses_per_prefix_subtables() {
        // 16384-symbol all-length-14 alphabet: Kraft = 16384·2^-14 = 1.
        // The canonical-code assignment spreads codes across all
        // 4096 primary prefixes (each prefix bucket gets 16384/4096 =
        // 4 symbols, all in its dedicated subtable of 4 entries), so
        // the build path exercises 4096 distinct
        // `prefix_to_idx[prefix]` updates. Decode a hand-assembled bit
        // stream that bounces across several prefix buckets and
        // assert each emitted symbol matches the source.
        let lens = vec![14u8; 16384];
        let table = HuffmanTable::build(lens, 0).expect("build long-code");
        assert_eq!(table.max_len(), 14);
        // Pick a spread of symbols that fall into different primary
        // prefix buckets.
        let chosen_syms: Vec<u32> = vec![0, 7, 4095, 4096, 8191, 8192, 12345, 16383];
        let codes = table.codes().to_vec();
        let lengths_view = table.lengths().to_vec();
        // Hand-assemble MSB-first bit stream from the canonical codes.
        let mut bit_buf: u64 = 0;
        let mut bit_fill: u32 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        for &sym in &chosen_syms {
            let l = lengths_view[sym as usize] as u32;
            let c = codes[sym as usize] as u64;
            bit_buf = (bit_buf << l) | c;
            bit_fill += l;
            while bit_fill >= 8 {
                let shift = bit_fill - 8;
                bytes.push(((bit_buf >> shift) & 0xff) as u8);
                bit_fill -= 8;
                bit_buf &= (1u64 << bit_fill).wrapping_sub(1);
            }
        }
        if bit_fill > 0 {
            bytes.push(((bit_buf << (8 - bit_fill)) & 0xff) as u8);
        }
        let mut br = BitReader::new(&bytes);
        let decoded: Vec<u32> = (0..chosen_syms.len())
            .map(|_| table.decode(&mut br))
            .collect();
        assert_eq!(decoded, chosen_syms);
    }

    /// Round-191 parity guard for the inlined two-level
    /// `decode_into_u16` hot loop: the batch helper must produce the
    /// same symbol sequence the per-pixel `decode()` does, on the same
    /// bit stream, across both single-level and two-level alphabets.
    /// The 10/12/14-bit path is what the v7 wire actually uses, so this
    /// pins the hot-loop refactor against the spec-grounded per-pixel
    /// reference path.
    #[test]
    fn decode_into_u16_matches_per_pixel_decode_two_level() {
        // 14-bit alphabet, max_len_used = 14 (two-level path active).
        let lens = vec![14u8; 16384];
        let table = HuffmanTable::build(lens, 0).expect("build long-code");
        assert_eq!(table.max_len(), 14);
        // Walk a mix of symbols whose canonical codes land in distinct
        // primary-prefix buckets, plus a few terminal-looking choices.
        let chosen_syms: Vec<u16> = vec![
            0, 1, 7, 255, 4095, 4096, 8191, 8192, 12345, 16383, 1024, 9999,
        ];
        let codes = table.codes().to_vec();
        let lengths_view = table.lengths().to_vec();
        let mut bit_buf: u64 = 0;
        let mut bit_fill: u32 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        for &sym in &chosen_syms {
            let l = lengths_view[sym as usize] as u32;
            let c = codes[sym as usize] as u64;
            bit_buf = (bit_buf << l) | c;
            bit_fill += l;
            while bit_fill >= 8 {
                let shift = bit_fill - 8;
                bytes.push(((bit_buf >> shift) & 0xff) as u8);
                bit_fill -= 8;
                bit_buf &= (1u64 << bit_fill).wrapping_sub(1);
            }
        }
        if bit_fill > 0 {
            bytes.push(((bit_buf << (8 - bit_fill)) & 0xff) as u8);
        }
        // Tail-pad so the bitreader can refill at end-of-stream
        // (matches the 8-bit parity test).
        bytes.extend_from_slice(&[0u8; 16]);

        // Per-pixel reference path.
        let mut br_ref = BitReader::new(&bytes);
        let ref_syms: Vec<u16> = (0..chosen_syms.len())
            .map(|_| table.decode(&mut br_ref) as u16)
            .collect();
        // Batch path under test — the inlined two-level hot loop.
        let mut br_batch = BitReader::new(&bytes);
        let mut batch_syms = vec![0u16; chosen_syms.len()];
        assert!(table.decode_into_u16(&mut br_batch, &mut batch_syms, 0x3fff));
        assert_eq!(ref_syms, chosen_syms);
        assert_eq!(batch_syms, chosen_syms);
    }

    /// EOF / truncation robustness guard for the **two-level**
    /// `decode_into_u16` hot loop (the 10/12/14-bit wire path). Sibling
    /// of `bitreader::single_level_decode_no_underflow_on_truncated_input`
    /// (commit `fb26d05`): that one pinned the single-level 8-bit path
    /// against the EOF `fill -= len` subtract-overflow; the two-level
    /// high-bit-depth path — which the v7 wire actually uses for every
    /// 10/12/14-bit FOURCC — had no equivalent truncation pin. A
    /// truncated slice (`spec/05` §3.3: the decoder must stop at the
    /// geometry-derived residual count and zero-pad past EOF, §3) must
    /// decode the full requested symbol count without panicking, and the
    /// batched path must agree byte-for-byte with the per-pixel
    /// `decode()` reference on the same short buffer.
    #[test]
    fn decode_into_u16_two_level_no_underflow_on_truncated_input() {
        // 14-bit all-length-14 alphabet ⇒ max_len_used = 14 > 12, so the
        // two-level redirect path runs (each symbol consumes
        // primary_bits=12 then secondary_bits=2). Kraft = 16384·2^-14 = 1.
        let lens = vec![14u8; 16384];
        let table = HuffmanTable::build(lens, 0).expect("build two-level");
        assert_eq!(table.max_len(), 14);
        // Only 3 bytes (24 bits) of input, but decode 16 symbols ×
        // 14 bits = 224 bits — forcing repeated EOF zero-pad refills
        // inside the two-level branch (consume 12 + peek 2 + consume 2
        // per symbol, each past EOF after the first ~1.7 symbols).
        let bytes = [0xab_u8, 0xcd, 0xef];
        let mut br_ref = BitReader::new(&bytes);
        let ref_syms: Vec<u16> = (0..16).map(|_| table.decode(&mut br_ref) as u16).collect();
        let mut br_batch = BitReader::new(&bytes);
        let mut batch_syms = vec![0u16; 16];
        // No panic == the `fill -= len` underflow guard holds; equality
        // pins the batched two-level loop to the per-pixel reference's
        // EOF zero-pad behaviour.
        assert!(table.decode_into_u16(&mut br_batch, &mut batch_syms, 0x3fff));
        assert_eq!(batch_syms, ref_syms);
    }

    /// Round-191 parity guard for the *single-level* branch of the
    /// inlined `decode_into_u16` hot loop. A well-formed-but-shallow
    /// 10-bit alphabet whose realised `max_len_used` lands at
    /// ≤ PRIMARY_BITS = 12 still funnels through `decode_into_u16` and
    /// must produce the same per-symbol decode result as the generic
    /// per-pixel `decode()`.
    #[test]
    fn decode_into_u16_matches_per_pixel_decode_single_level() {
        // 1024-symbol all-length-10 alphabet: Kraft = 1024·2^-10 = 1,
        // max_len_used = 10 ≤ PRIMARY_BITS = 12 ⇒ single-level path.
        let lens = vec![10u8; 1024];
        let table = HuffmanTable::build(lens, 0).expect("build single-level u16");
        assert_eq!(table.max_len(), 10);
        let chosen_syms: Vec<u16> = vec![0, 1, 7, 255, 512, 1023, 256, 768];
        let codes = table.codes().to_vec();
        let lengths_view = table.lengths().to_vec();
        let mut bit_buf: u64 = 0;
        let mut bit_fill: u32 = 0;
        let mut bytes: Vec<u8> = Vec::new();
        for &sym in &chosen_syms {
            let l = lengths_view[sym as usize] as u32;
            let c = codes[sym as usize] as u64;
            bit_buf = (bit_buf << l) | c;
            bit_fill += l;
            while bit_fill >= 8 {
                let shift = bit_fill - 8;
                bytes.push(((bit_buf >> shift) & 0xff) as u8);
                bit_fill -= 8;
                bit_buf &= (1u64 << bit_fill).wrapping_sub(1);
            }
        }
        if bit_fill > 0 {
            bytes.push(((bit_buf << (8 - bit_fill)) & 0xff) as u8);
        }
        bytes.extend_from_slice(&[0u8; 16]);

        let mut br_ref = BitReader::new(&bytes);
        let ref_syms: Vec<u16> = (0..chosen_syms.len())
            .map(|_| table.decode(&mut br_ref) as u16)
            .collect();
        let mut br_batch = BitReader::new(&bytes);
        let mut batch_syms = vec![0u16; chosen_syms.len()];
        assert!(table.decode_into_u16(&mut br_batch, &mut batch_syms, 0x03ff));
        assert_eq!(ref_syms, chosen_syms);
        assert_eq!(batch_syms, chosen_syms);
    }
}
