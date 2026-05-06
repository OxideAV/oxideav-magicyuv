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
    #[inline]
    pub fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        (self.acc >> (64 - n)) as u32
    }

    /// Drop `n` bits from the top of the accumulator and refill from
    /// `data`.
    #[inline]
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
    #[inline]
    fn refill(&mut self) {
        while self.fill <= 56 {
            let byte = if self.pos < self.data.len() {
                self.data[self.pos]
            } else {
                0
            };
            self.acc |= (byte as u64) << (56 - self.fill);
            self.fill += 8;
            // pos advances even past EOF so position() reflects the
            // last in-bounds index plus one.
            if self.pos < self.data.len() {
                self.pos += 1;
            } else {
                // Stop trying to refill once we're past the end; we
                // don't want pos to grow unboundedly.
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
