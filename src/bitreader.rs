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
}
