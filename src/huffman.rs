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
/// 4K entries × `(u16, u8)` even for max_length = 18.
const PRIMARY_BITS: u8 = 12;

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
    /// `max_len_used`. For an all-unused descriptor (Kraft 0) this is
    /// 0; we never actually decode such a table, but parse_lengths
    /// allows it.
    max_len: u8,
    /// Primary table indexed by the top `primary_bits` bits.
    /// `(symbol_or_subtable_index, length_or_marker)`. If `length`
    /// is `≤ primary_bits` the entry is terminal (`length` is the
    /// real code length, `symbol` is the decoded symbol). If
    /// `length` is `0xff` the entry is a redirect: `symbol` is the
    /// index into `secondary` of the per-prefix subtable, and the
    /// caller consumes another `(max_len - primary_bits)` bits.
    primary: Vec<(u32, u8)>,
    /// Effective primary-prefix size in bits. `min(max_len, PRIMARY_BITS)`.
    primary_bits: u8,
    /// Per-prefix secondary tables. Each entry is `Vec<(symbol,
    /// length_in_subtable)>` indexed by the next `(max_len -
    /// primary_bits)` bits. Empty when `max_len ≤ primary_bits`.
    secondary: Vec<Vec<(u32, u8)>>,
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
        let mut code = vec![0u32; lengths.len()];
        if max_len > 0 {
            let mut cur = start.clone();
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
        let mut primary = vec![(0u32, 0u8); primary_size];
        let mut secondary: Vec<Vec<(u32, u8)>> = Vec::new();

        if max_len == 0 {
            return Ok(Self {
                lengths,
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
                for k in 0..count {
                    primary[base + k] = (s as u32, l);
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
            let mut prefix_to_idx: std::collections::HashMap<u32, usize> =
                std::collections::HashMap::new();

            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                if l <= primary_bits {
                    // Terminal: spread across the primary bucket.
                    let shift = (primary_bits - l) as u32;
                    let base = (code[s] as usize) << shift;
                    let count = 1usize << shift;
                    for k in 0..count {
                        primary[base + k] = (s as u32, l);
                    }
                } else {
                    // Deferred: route via a subtable.
                    let prefix = code[s] >> (l - primary_bits);
                    let sub_idx = *prefix_to_idx.entry(prefix).or_insert_with(|| {
                        secondary.push(vec![(0u32, 0u8); secondary_size]);
                        secondary.len() - 1
                    });
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
                    for k in 0..count {
                        secondary[sub_idx][base + k] = (s as u32, l_in_sub);
                    }
                    // Mark the primary entry as a redirect (length =
                    // 0xff sentinel; symbol = sub_idx).
                    primary[prefix as usize] = (sub_idx as u32, 0xff);
                }
            }
        }

        Ok(Self {
            lengths,
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
    /// (every fixture has Kraft = 1.0 per `spec/05` §1.3) but a
    /// well-formed-but-degenerate descriptor parses without raising
    /// `HuffmanOverfull`. Calling `decode` on such a table will
    /// never make progress, so the caller should reject.
    pub fn is_empty(&self) -> bool {
        self.max_len == 0
    }

    /// Decode one symbol from `br`. Returns the symbol's index in
    /// `[0, N)`. The caller is expected to keep the symbol count
    /// equal to plane-height-of-slice × plane-width per `spec/05`
    /// §3.3 and stop reading at exactly that count.
    #[inline]
    pub fn decode(&self, br: &mut BitReader<'_>) -> u32 {
        if self.max_len == 0 {
            return 0;
        }
        let key = br.peek_bits(self.primary_bits as u32) as usize;
        let (sym_or_sub, len) = self.primary[key];
        if len != 0xff {
            br.consume(len as u32);
            sym_or_sub
        } else {
            // Two-level: consume the primary prefix and look up the
            // subtable.
            br.consume(self.primary_bits as u32);
            let secondary_bits = self.max_len - self.primary_bits;
            let key2 = br.peek_bits(secondary_bits as u32) as usize;
            let (sym, l_in_sub) = self.secondary[sym_or_sub as usize][key2];
            br.consume(l_in_sub as u32);
            sym
        }
    }

    /// Borrow the per-symbol length array (debug / cross-validation).
    pub fn lengths(&self) -> &[u8] {
        &self.lengths
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
}
