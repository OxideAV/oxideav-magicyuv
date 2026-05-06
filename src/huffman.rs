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
//! Decoding uses a flat lookup table keyed on the top
//! `max_huffman_code_length` bits of the bit accumulator. Spec/05
//! §2.0.3 says the proprietary decoder uses a flat-table lookup of
//! the same size; we mirror it.

use crate::bitreader::BitReader;
use crate::error::{Error, Result};

/// Per-plane Huffman length descriptor parser (`spec/05` §1.1).
///
/// `n` is `1 << bit_depth` (i.e. 256 for 8-bit). `max_length` is the
/// per-tier cap (12 for 8-bit, per the `HuffCoderT<…, 8, 12, 12>`
/// template parameter in `spec/05` §1.5).
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
    /// Flat lookup table indexed by the top `max_len` bits of the bit
    /// accumulator. Each entry is `(symbol, code_length)`. For an
    /// in-table prefix of length `L`, every entry whose key starts
    /// with that prefix's bits (followed by all 2^(max_len-L)
    /// possibilities) holds the same `(symbol, L)`.
    flat: Vec<(u16, u8)>,
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
        // We want a per-symbol `code` table; the algorithm produces
        // codes in symbol-index order *within each length tier*. The
        // simplest implementation is two passes:
        //   1. Compute `start[len]` = first code value for length tier
        //      `len`, by simulating the accumulator.
        //   2. Walk `s = 0..N`, assign `code[s] = start[L[s]]++`.
        let mut start = vec![0u32; (max_len as usize) + 2];
        let mut acc: u32 = 0xffff_ffff;
        for len in (1..=(max_len as u32)).rev() {
            let n_at = bl_count[len as usize];
            // Within the tier, codes are `acc+1, acc+2, …, acc+n_at`.
            // Save the smallest as `start[len]`.
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

        // Build flat lookup table indexed by top `max_len` bits.
        let table_size: usize = if max_len == 0 { 1 } else { 1usize << max_len };
        let mut flat = vec![(0u16, 0u8); table_size];
        if max_len > 0 {
            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                let shift = (max_len - l) as u32;
                let base = (code[s] as usize) << shift;
                let count = 1usize << shift;
                for k in 0..count {
                    flat[base + k] = (s as u16, l);
                }
            }
        }

        Ok(Self {
            lengths,
            max_len,
            flat,
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
    pub fn decode(&self, br: &mut BitReader<'_>) -> u16 {
        if self.max_len == 0 {
            // Pathological — caller protected against this (see
            // is_empty); if it slips through we return symbol 0 to
            // avoid an unconditional panic in tight loops.
            return 0;
        }
        let key = br.peek_bits(self.max_len as u32) as usize;
        let (sym, len) = self.flat[key];
        br.consume(len as u32);
        sym
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
}
