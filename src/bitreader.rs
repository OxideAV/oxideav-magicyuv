//! MSB-first big-endian byte bit reader (`spec/05` §2.2 / §3.3).
//!
//! Each byte's bit 7 is consumed first, then bit 6, …, bit 0; then
//! the next byte is loaded MSB-first. This matches the decoder kernel
//! at `magicyuv.dll!0x69d44bc0` (`shlx %ecx, %r10d, %r14`,
//! consume-direction left-aligned).
//!
//! The reader keeps a 64-bit accumulator and a count of valid bits in
//! the high end (`fill`). `peek(n)` returns the top `n` bits as the
//! low bits of a `u32`; `consume(n)` shifts them out. The reader is
//! tolerant of running off the end of `data` — it pretends the
//! trailing bits are zero, and the caller is expected to know the
//! exact symbol count from plane geometry per `spec/05` §3.3.

#[derive(Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Top `fill` bits of `acc` are valid (left-aligned, i.e. bit 63
    /// is the next bit to deliver). Lower bits are zero.
    acc: u64,
    fill: u32,
    /// Cursor into `data` for the next byte to fetch.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut r = Self {
            data,
            acc: 0,
            fill: 0,
            pos: 0,
        };
        r.refill();
        r
    }

    /// Bytes consumed from the underlying slice. Used for diagnostics
    /// (e.g. matching the spec/02 §5.1 slice-byte arithmetic).
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Top `n` bits of the accumulator as the low `n` bits of a u32.
    /// `n` must be ≤ 32.
    #[inline(always)]
    pub fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        (self.acc >> (64 - n)) as u32
    }

    /// Drop `n` bits from the top of the accumulator and refill from
    /// `data`.
    #[inline(always)]
    pub fn consume(&mut self, n: u32) {
        debug_assert!(n <= 32);
        self.acc <<= n;
        self.fill = self.fill.saturating_sub(n);
        self.refill();
    }

    /// Read `n` bits as an unsigned integer in `[0, 2^n)`. `n` ≤ 32.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek_bits(n);
        self.consume(n);
        v
    }

    /// Make sure ≥ 32 bits are buffered, advancing the byte cursor.
    /// At EOF the buffer is implicitly extended with zero bytes.
    ///
    /// Round-3 perf: the hot Huffman-decode path calls this on every
    /// symbol. The fast path `pos + 8 ≤ data.len()` reads 8 bytes
    /// big-endian into a u64 in one go and OR-merges the unread bits
    /// into the accumulator — same observable bit stream as the
    /// byte-by-byte loop, but ≈ 4× fewer loads + branches per call.
    /// The slow path falls through to the original byte-loop logic
    /// near EOF, where misaligned reads aren't safe.
    #[inline(always)]
    fn refill(&mut self) {
        // Fast path: at least 8 unread bytes ahead AND room for ≥ 8
        // bits in `acc` (`self.fill <= 56`). We pull 8 bytes
        // big-endian as a single u64 load, shift it right by `fill`
        // so its top byte lines up at the first empty bit of `acc`,
        // and OR it in. Number of bytes successfully merged is
        // `(64 - fill) / 8` — the rest are kept for the next refill
        // by advancing `pos` by exactly that many.
        //
        // OR-overlap correctness: when `fill` isn't a multiple of 8,
        // some leftover bits of the partially-merged byte land at the
        // bottom of `acc`. On the next refill those same bits at the
        // same data offset are re-OR-ed in — bit-identical, so the
        // OR is idempotent and the observable stream is unchanged.
        if self.fill <= 56 && self.pos + 8 <= self.data.len() {
            let arr: [u8; 8] = self.data[self.pos..self.pos + 8]
                .try_into()
                .expect("8-byte window bounds-checked above");
            let next = u64::from_be_bytes(arr);
            // `next >> fill` lines up `next[63..56]` at
            // `acc[(63-fill)..(56-fill)]` — the top of the empty
            // region. The shift is in [0, 56], well within u64's
            // shift range so no overflow check fires.
            self.acc |= next >> self.fill;
            let bytes = (64 - self.fill) / 8;
            self.pos += bytes as usize;
            self.fill += bytes * 8;
            return;
        }
        // Slow path: ≤ 7 bytes left, or we already have > 56 bits.
        // Same byte-by-byte loop as before — preserves the exact
        // EOF-pad-with-zero semantics that integration tests
        // (and the trace lockstep) rely on.
        while self.fill <= 56 {
            let byte = if self.pos < self.data.len() {
                self.data[self.pos]
            } else {
                0
            };
            self.acc |= (byte as u64) << (56 - self.fill);
            self.fill += 8;
            if self.pos < self.data.len() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }
}

/// Batched unpacker for high-bit-depth raw-mode slice payloads
/// (`spec/05` §4.1: `(pixels * bits + 7) / 8` MSB-first bits packed in
/// `data`). Writes `dst.len()` samples of width `bits` ∈ {10, 12, 14}
/// into `dst`, masking each to the low `bits` bits.
///
/// Same observable bit stream as a per-pixel
/// `BitReader::read_bits(bits)` loop, but folds the bit-bucket into a
/// 64-bit accumulator that is refilled from 8-byte big-endian loads.
/// The per-pixel cost is one `peek` (a shift + cast) plus a refill
/// branch that fires once every `floor(56 / bits)` pixels (≈ 5 at
/// 10-bit, ≈ 4 at 12/14-bit) — versus the generic
/// `BitReader::read_bits` path which performs the same refill check
/// on every call.
///
/// `bits` is taken at runtime; if it is 0 or > 16 the buffer is left
/// untouched. End-of-data is implicitly zero-padded to match the
/// generic `BitReader` semantics.
///
/// Used by the decoder's raw-mode high-bit-depth slice payload
/// dispatch (`decoder.rs::decode_high_bit_depth`).
#[inline]
pub fn unpack_raw_bits_to_u16(data: &[u8], dst: &mut [u16], bits: u8, mask: u16) {
    if bits == 0 || bits > 16 || dst.is_empty() {
        return;
    }
    let nbits = bits as u32;
    let mut acc: u64 = 0;
    let mut fill: u32 = 0;
    let mut pos: usize = 0;
    let data_len = data.len();
    // Initial refill — mirror `BitReader::new`'s behaviour.
    if pos + 8 <= data_len {
        let arr: [u8; 8] = data[pos..pos + 8].try_into().expect("8-byte window");
        acc = u64::from_be_bytes(arr);
        pos += 8;
        fill = 64;
    } else {
        // Slow scalar fill for short payloads.
        while fill <= 56 {
            let byte = if pos < data_len { data[pos] } else { 0 };
            acc |= (byte as u64) << (56 - fill);
            fill += 8;
            if pos < data_len {
                pos += 1;
            } else {
                break;
            }
        }
    }
    for px in dst.iter_mut() {
        // Refill if fewer than `nbits` valid bits remain.
        if fill < nbits {
            // Pull up to 8 fresh bytes into the empty high region.
            if pos + 8 <= data_len {
                let arr: [u8; 8] = data[pos..pos + 8].try_into().expect("8-byte window");
                let next = u64::from_be_bytes(arr);
                acc |= next >> fill;
                let bytes = (64 - fill) / 8;
                pos += bytes as usize;
                fill += bytes * 8;
            } else {
                while fill < nbits && fill <= 56 {
                    let byte = if pos < data_len { data[pos] } else { 0 };
                    acc |= (byte as u64) << (56 - fill);
                    fill += 8;
                    if pos < data_len {
                        pos += 1;
                    } else {
                        // EOF — treat the rest as implicit-zero per
                        // `BitReader::refill`'s pad-with-zero rule.
                        break;
                    }
                }
            }
        }
        // Peek `nbits` from the top of `acc`, consume, mask.
        let v = (acc >> (64 - nbits)) as u16;
        acc <<= nbits;
        fill = fill.saturating_sub(nbits);
        *px = v & mask;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        // `0xa5` = 10100101. Consuming 4 bits should yield 0b1010 = 10.
        let mut br = BitReader::new(&[0xa5, 0x3c]);
        assert_eq!(br.read_bits(4), 0b1010);
        assert_eq!(br.read_bits(4), 0b0101);
        assert_eq!(br.read_bits(8), 0x3c);
    }

    #[test]
    fn reads_across_byte_boundary() {
        // 0xff 0x80 = 11111111 10000000.
        // Read 9 bits → 0b111111111 = 0x1ff; next 7 bits all zero.
        let mut br = BitReader::new(&[0xff, 0x80]);
        assert_eq!(br.read_bits(9), 0x1ff);
        assert_eq!(br.read_bits(7), 0);
    }

    #[test]
    fn pads_with_zero_past_end() {
        let mut br = BitReader::new(&[0x80]);
        // First bit = 1.
        assert_eq!(br.read_bits(1), 1);
        // Next 7 bits in the byte are zero.
        assert_eq!(br.read_bits(7), 0);
        // Past-EOF reads keep returning zero.
        assert_eq!(br.read_bits(16), 0);
    }

    /// Reference implementation of `unpack_raw_bits_to_u16` built on
    /// top of the generic per-pixel `BitReader::read_bits`, used to
    /// pin the batched unpacker against the same observable bit
    /// stream the decoder used to emit.
    fn unpack_ref(data: &[u8], dst: &mut [u16], bits: u8, mask: u16) {
        let mut br = BitReader::new(data);
        for px in dst.iter_mut() {
            *px = (br.read_bits(bits as u32) as u16) & mask;
        }
    }

    fn pack_msb_first(samples: &[u16], bits: u8) -> Vec<u8> {
        // Same MSB-first big-endian wire convention as the encoder's
        // `BitWriter`. Use a 64-bit accumulator with `bits_used` valid
        // bits in the top, draining whole bytes from the high end.
        let mut out: Vec<u8> = Vec::with_capacity((samples.len() * bits as usize).div_ceil(8));
        let mut acc: u64 = 0;
        let mut bits_used: u32 = 0;
        for &s in samples {
            let shift = 64 - bits_used - (bits as u32);
            acc |= (s as u64) << shift;
            bits_used += bits as u32;
            while bits_used >= 8 {
                let byte = (acc >> 56) as u8;
                out.push(byte);
                acc <<= 8;
                bits_used -= 8;
            }
        }
        if bits_used > 0 {
            out.push((acc >> 56) as u8);
        }
        out
    }

    #[test]
    fn unpack_raw_bits_to_u16_matches_per_pixel_at_10_12_14() {
        // For each bit-depth pack a deterministic 200-sample stream,
        // unpack with both paths, assert byte-for-byte agreement.
        for &bits in &[10u8, 12, 14] {
            let mask = ((1u32 << bits) - 1) as u16;
            let n = 200usize;
            // Deterministic input (xorshift32) covering edge values
            // (0, mask, mid-range).
            let mut s: u32 = 0xdead_beef ^ (bits as u32).wrapping_mul(0x9e37_79b9);
            let mut samples = Vec::with_capacity(n);
            for i in 0..n {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let v = if i == 0 {
                    0
                } else if i == 1 {
                    mask
                } else {
                    (s as u16) & mask
                };
                samples.push(v);
            }
            let packed = pack_msb_first(&samples, bits);

            let mut fast = vec![0u16; n];
            unpack_raw_bits_to_u16(&packed, &mut fast, bits, mask);

            let mut slow = vec![0u16; n];
            unpack_ref(&packed, &mut slow, bits, mask);

            assert_eq!(fast, slow, "fast/slow mismatch at bits={bits}");
            assert_eq!(fast, samples, "round-trip mismatch at bits={bits}");
        }
    }

    #[test]
    fn unpack_raw_bits_to_u16_zero_pads_past_eof() {
        // Provide a payload shorter than the destination demands; both
        // paths should zero-fill the tail.
        let bits = 12u8;
        let mask = 0x0fff_u16;
        // 3 bytes = 2 full 12-bit samples + 0 leftover bits = 24
        // unread bits → fast path covers first 2 samples; the next
        // sample(s) should be implicit zeros.
        let data = [0xab_u8, 0xcd, 0xef];
        let mut fast = vec![0u16; 5];
        unpack_raw_bits_to_u16(&data, &mut fast, bits, mask);
        let mut slow = vec![0u16; 5];
        unpack_ref(&data, &mut slow, bits, mask);
        assert_eq!(fast, slow);
        // First sample is the top 12 bits of the 24-bit stream:
        // 0xabcdef >> 12 = 0xabc.
        assert_eq!(fast[0], 0xabc);
        // Second sample is the low 12 bits: 0xdef.
        assert_eq!(fast[1], 0xdef);
        assert_eq!(fast[2], 0);
        assert_eq!(fast[3], 0);
        assert_eq!(fast[4], 0);
    }

    #[test]
    fn unpack_raw_bits_to_u16_handles_empty_dst() {
        let mut dst: [u16; 0] = [];
        unpack_raw_bits_to_u16(&[0xff_u8; 4], &mut dst, 10, 0x3ff);
        // Just exercising the early-return.
    }

    #[test]
    fn unpack_raw_bits_to_u16_long_payload_crosses_refills() {
        // 4096 samples × 14 bits = 7168 bytes → multiple refill cycles.
        let bits = 14u8;
        let mask = 0x3fff_u16;
        let n = 4096usize;
        let mut s: u32 = 0x1234_5678;
        let samples: Vec<u16> = (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as u16) & mask
            })
            .collect();
        let packed = pack_msb_first(&samples, bits);
        let mut fast = vec![0u16; n];
        unpack_raw_bits_to_u16(&packed, &mut fast, bits, mask);
        assert_eq!(fast, samples);
    }
}
