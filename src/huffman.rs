//! Canonical Huffman support for MagicYUV.
//!
//! The trace document (§3.5, §5.1) describes:
//!
//! * Per-plane "length descriptor" — one byte per symbol with the MSB
//!   clear, OR a `0x80 | LEN`, `RUN-1` byte pair to declare RUN
//!   consecutive symbols of length LEN. Lengths are at most 32; length 0
//!   means "absent".
//! * Canonical codes are reconstructed in length-ascending, symbol-
//!   ascending order, **with each code bit-complemented** so that the
//!   most common (shortest) symbols start with leading `1` bits rather
//!   than leading `0`s. Empirically MagicYUV uses this "JPEG-style"
//!   convention (first code at length L is `(1 << L) - 1`, decremented
//!   per subsequent symbol; each length transition right-shifts the
//!   running code) — verified by decoding a constant-grey 64×48 frame
//!   where sym 0's code is observed as `1` (1 bit) rather than `0`.
//! * Symbols are read MSB-first from the slice bitstream.

use crate::bitstream::BitReader;
use oxideav_core::{Error, Result};

/// Hard cap on Huffman code lengths the decoder accepts; matches the
/// trace document's "decoder enforces a hard cap of 32".
pub const MAX_HUFF_LENGTH: u8 = 32;

/// Decoded length descriptor: `lengths[symbol]` = code length in bits
/// (0 means the symbol is absent from the alphabet).
pub struct LengthTable {
    pub lengths: Vec<u8>,
}

impl LengthTable {
    /// Read one descriptor from `bytes` starting at `pos`. Returns the
    /// table plus the new byte offset. `alphabet` is the expected number
    /// of symbols (e.g. 256 for an 8-bit plane, 1024 for 10-bit).
    pub fn parse(bytes: &[u8], pos: usize, alphabet: usize) -> Result<(Self, usize)> {
        let mut lengths = Vec::with_capacity(alphabet);
        let mut p = pos;
        while lengths.len() < alphabet {
            if p >= bytes.len() {
                return Err(Error::invalid(
                    "magicyuv: huffman length descriptor truncated",
                ));
            }
            let b = bytes[p];
            p += 1;
            if b & 0x80 == 0 {
                // Single-symbol form. b is the literal length.
                if b > MAX_HUFF_LENGTH {
                    return Err(Error::invalid(format!(
                        "magicyuv: huffman length {b} exceeds hard cap {MAX_HUFF_LENGTH}"
                    )));
                }
                lengths.push(b);
            } else {
                // Run form: this byte is `0x80 | LEN`; the next byte is RUN-1.
                let len = b & 0x7F;
                if len > MAX_HUFF_LENGTH {
                    return Err(Error::invalid(format!(
                        "magicyuv: huffman length {len} exceeds hard cap {MAX_HUFF_LENGTH}"
                    )));
                }
                if p >= bytes.len() {
                    return Err(Error::invalid(
                        "magicyuv: huffman run-form truncated (missing run length byte)",
                    ));
                }
                let run = bytes[p] as usize + 1;
                p += 1;
                if lengths.len() + run > alphabet {
                    return Err(Error::invalid(format!(
                        "magicyuv: huffman run overshoots alphabet (have {} need {} cap {})",
                        lengths.len(),
                        run,
                        alphabet
                    )));
                }
                for _ in 0..run {
                    lengths.push(len);
                }
            }
        }
        Ok((Self { lengths }, p))
    }
}

/// Symbol-decode table. We use a single-level lookup at `MAX_HUFF_LENGTH`
/// bits — small enough to fit comfortably (4 KiB for 8-bit alphabets,
/// 4 GiB if naïvely sized for 10-bit) so we cap the lookup at 12 bits
/// (the encoder's natural cap) and fall back to a linear walk for the
/// few longer codes when they arise. In practice MagicYUV's encoder
/// caps at 12 (max_huff_length byte = 12), and the decoder's hard cap
/// is 32; we accept both.
pub struct CanonicalHuffman {
    /// Direct lookup table indexed by the next `lookup_bits` of the
    /// stream. Each entry is `(symbol, code_length)`. A `code_length`
    /// of 0 means "no fast match — fall back to slow walk".
    lookup: Vec<(u32, u8)>,
    lookup_bits: u32,
    /// (length, base code, base symbol-index in `symbols`) per nonzero
    /// length, sorted ascending. Used by the slow fallback when the
    /// cached lookup misses.
    levels: Vec<(u8, u32, usize)>,
    /// Symbols sorted by (length asc, symbol-index asc).
    symbols: Vec<u32>,
}

impl CanonicalHuffman {
    /// Build a canonical Huffman from a length descriptor.
    pub fn build(lengths: &[u8]) -> Result<Self> {
        // Sanity: enforce ≤ MAX_HUFF_LENGTH.
        for (sym, &l) in lengths.iter().enumerate() {
            if l > MAX_HUFF_LENGTH {
                return Err(Error::invalid(format!(
                    "magicyuv: symbol {sym} length {l} exceeds cap {MAX_HUFF_LENGTH}"
                )));
            }
        }

        // Sort symbol indices by (length asc, symbol DESC), skipping
        // length 0. The descending tiebreak is the empirically-required
        // convention for MagicYUV — verified against an FFmpeg-encoded
        // 64×48 YUV422P testsrc fixture where symbols 16 and 25 share
        // length 10 and the encoder maps the higher-indexed sym to the
        // *higher* canonical code (= lower wire value after the
        // bit-complement step below). Sorting ascending gave a
        // consistent off-by-one swap inside each multi-symbol length.
        let mut order: Vec<u32> = (0..lengths.len() as u32)
            .filter(|&s| lengths[s as usize] != 0)
            .collect();
        order.sort_by_key(|&s| (lengths[s as usize], std::cmp::Reverse(s)));

        if order.is_empty() {
            return Err(Error::invalid(
                "magicyuv: huffman alphabet is empty (all lengths == 0)",
            ));
        }

        // Single-symbol case: assign it code 0 of length max(1, declared).
        // The trace doc shows length-1 codes in real files, never length-0 codes.
        // Build the canonical-code stream the standard way.
        let max_len = order
            .iter()
            .map(|&s| lengths[s as usize])
            .max()
            .unwrap_or(0);
        if max_len == 0 {
            return Err(Error::invalid("magicyuv: max huffman length is zero"));
        }

        // Verify the Kraft inequality holds (== 1) for a complete tree.
        // Allow the special single-symbol case where length is 1 and only
        // one symbol exists (Kraft = 0.5) — tolerated by the FFmpeg decoder
        // and sometimes seen for collapsed alpha planes.
        let mut kraft_num: u64 = 0;
        let max_l_u32 = max_len as u32;
        for &sym in &order {
            let l = lengths[sym as usize] as u32;
            kraft_num += 1u64 << (max_l_u32 - l);
        }
        let kraft_one = 1u64 << max_l_u32;
        if kraft_num > kraft_one {
            return Err(Error::invalid(format!(
                "magicyuv: huffman over-subscribed (Kraft sum {} > {})",
                kraft_num, kraft_one
            )));
        }
        // Under-subscription is tolerated.

        // Assign canonical codes per the JPEG/MagicYUV "high-first"
        // convention: at each length L the codes are assigned starting
        // from the largest available value and decremented per symbol.
        // Equivalently, run the standard ascending canonical algorithm
        // and bit-complement each code (`canon = canon ^ ((1<<len) - 1)`),
        // which is what the wire format expects when read MSB-first.
        //
        // We materialise (length, base_canonical_code, base_symbol_index)
        // per length level; `base_canonical_code` is the *standard*
        // ascending canonical code of the first symbol at that length —
        // the actual wire code is `base_canonical_code ^ ((1<<len) - 1)`,
        // and decreases by 1 per subsequent symbol at the same length.
        let mut levels: Vec<(u8, u32, usize)> = Vec::new();
        let mut code: u32 = 0;
        let mut prev_len: u8 = 0;
        let mut idx: usize = 0;
        while idx < order.len() {
            let len = lengths[order[idx] as usize];
            if prev_len != 0 {
                code <<= len - prev_len;
            }
            levels.push((len, code, idx));
            let mut j = idx + 1;
            while j < order.len() && lengths[order[j] as usize] == len {
                j += 1;
            }
            code = code
                .checked_add((j - idx) as u32)
                .ok_or_else(|| Error::invalid("magicyuv: huffman code overflow"))?;
            prev_len = len;
            idx = j;
        }

        // Build a single-level lookup at min(max_len, 12) bits.
        let lookup_bits: u32 = max_l_u32.min(12);
        let lookup_size = 1usize << lookup_bits;
        let mut lookup: Vec<(u32, u8)> = vec![(0, 0); lookup_size];

        if lookup_bits > 0 {
            for (li, &(len, base_code_canon, base_sym_idx)) in levels.iter().enumerate() {
                if (len as u32) > lookup_bits {
                    continue; // too long to fit in the fast table
                }
                let count = if li + 1 < levels.len() {
                    levels[li + 1].2 - base_sym_idx
                } else {
                    order.len() - base_sym_idx
                };
                let shift = lookup_bits - len as u32;
                let len_mask = (1u32 << len) - 1;
                for k in 0..count {
                    let canon = base_code_canon + k as u32;
                    // Wire code is the bit-complement of the standard canonical.
                    let wire = canon ^ len_mask;
                    let entry = (wire << shift) as usize;
                    let span = 1usize << shift;
                    let sym = order[base_sym_idx + k];
                    for s in 0..span {
                        lookup[entry + s] = (sym, len);
                    }
                }
            }
        }

        Ok(Self {
            lookup,
            lookup_bits,
            levels,
            symbols: order,
        })
    }

    /// Decode one symbol from `br`. Returns `(symbol, bits_consumed)`.
    /// A successful decode also consumes the bits via `read_bits`.
    pub fn decode<'a>(&self, br: &mut BitReader<'a>) -> Result<u32> {
        // Fast path: peek lookup_bits, look up; if hit, consume that many.
        if self.lookup_bits > 0 {
            let key = br.peek_bits(self.lookup_bits) as usize;
            let (sym, len) = self.lookup[key];
            if len != 0 {
                let _ = br.read_bits(len as u32);
                return Ok(sym);
            }
        }
        // Slow path: walk levels in length order, accumulating bits.
        // The bits we already peeked at lookup_bits constitute the prefix;
        // we need to find a matching code at length > lookup_bits.
        let mut accum: u32 = if self.lookup_bits > 0 {
            br.peek_bits(self.lookup_bits)
        } else {
            0
        };
        let mut have: u32 = self.lookup_bits;

        for (li, &(len, base_code_canon, base_sym_idx)) in self.levels.iter().enumerate() {
            let len_u = len as u32;
            if len_u <= self.lookup_bits {
                continue;
            }
            // Extend accum to len bits.
            while have < len_u {
                let want = len_u - have;
                let peek = br.peek_bits(have + want);
                accum = peek;
                have += want;
            }
            let wire = accum;
            let len_mask = (1u32 << len_u) - 1;
            let canon = wire ^ len_mask;
            let count = if li + 1 < self.levels.len() {
                self.levels[li + 1].2 - base_sym_idx
            } else {
                self.symbols.len() - base_sym_idx
            };
            if canon >= base_code_canon && canon < base_code_canon + count as u32 {
                let sym = self.symbols[base_sym_idx + (canon - base_code_canon) as usize];
                let _ = br.read_bits(len_u);
                return Ok(sym);
            }
        }
        Err(Error::invalid(
            "magicyuv: huffman code did not match any symbol",
        ))
    }

    /// Number of distinct symbols (length > 0). Useful for sanity checks.
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_byte_form() {
        // 4-symbol alphabet, lengths [2, 2, 1, 0].
        let bytes = [2u8, 2, 1, 0];
        let (t, end) = LengthTable::parse(&bytes, 0, 4).unwrap();
        assert_eq!(t.lengths, vec![2, 2, 1, 0]);
        assert_eq!(end, 4);
    }

    #[test]
    fn parse_run_form() {
        // 6-symbol alphabet: first 4 of length 2, then 2 of length 3.
        let bytes = [0x82u8, 0x03, 0x83, 0x01];
        let (t, end) = LengthTable::parse(&bytes, 0, 6).unwrap();
        assert_eq!(t.lengths, vec![2, 2, 2, 2, 3, 3]);
        assert_eq!(end, 4);
    }

    #[test]
    fn canonical_codes_high_first_with_desc_tiebreak() {
        // Lengths [3, 3, 3, 3, 3, 2, 4, 4]. Within a length, MagicYUV
        // assigns canonical-ascending codes to syms in DESCENDING
        // order, so sym F (5, len 2) gets the highest priority code,
        // then within len 3 syms 4, 3, 2, 1, 0, and within len 4 syms
        // 7, 6. After bit-complement (high-first wire convention):
        //   F=11   sym 4=101  sym 3=100  sym 2=011  sym 1=010  sym 0=001
        //   sym 7=0001  sym 6=0000
        // Decode stream "F A G H" = sym 5, sym 0, sym 6, sym 7
        //   = 11 + 001 + 0000 + 0001 = 1100100000001 (13 bits) →
        //   byte 0 = 11001000 (0xC8), byte 1 = 00001000 (0x08, padded
        //   with 3 trailing zero bits).
        let lengths = vec![3, 3, 3, 3, 3, 2, 4, 4];
        let h = CanonicalHuffman::build(&lengths).unwrap();
        let stream = [0b1100_1000u8, 0b0000_1000u8];
        let mut br = BitReader::new(&stream);
        assert_eq!(h.decode(&mut br).unwrap(), 5); // F
        assert_eq!(h.decode(&mut br).unwrap(), 0); // A (sym 0, last in desc order at len 3)
        assert_eq!(h.decode(&mut br).unwrap(), 6); // G (sym 6, last in desc order at len 4)
        assert_eq!(h.decode(&mut br).unwrap(), 7); // H (sym 7, first in desc order at len 4)
    }

    #[test]
    fn single_symbol_alphabet() {
        // Only symbol 5, length 1. High-first: code = `1` (1 bit).
        let mut lengths = vec![0u8; 8];
        lengths[5] = 1;
        let h = CanonicalHuffman::build(&lengths).unwrap();
        let stream = [0b1111_1111u8];
        let mut br = BitReader::new(&stream);
        assert_eq!(h.decode(&mut br).unwrap(), 5);
        assert_eq!(h.decode(&mut br).unwrap(), 5);
    }
}
