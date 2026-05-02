//! MSB-first bit reader for MagicYUV slice payloads.
//!
//! Per the trace document §3.6 / §5.1, MagicYUV slice residuals are a
//! left-aligned canonical-Huffman bitstream — the first symbol's MSB
//! sits at the high bit of the first byte, the second symbol packs
//! immediately after, and so on. This module is the minimum primitive
//! needed by the Huffman decoder.

use oxideav_core::{Error, Result};

/// MSB-first bit reader. Reads up to 32 bits at a time.
pub struct BitReader<'a> {
    bytes: &'a [u8],
    /// Cached bit buffer; high `nbits` bits are valid.
    cache: u64,
    /// Number of valid bits currently in `cache` (0..=64).
    nbits: u32,
    /// Index of the next byte to refill from.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cache: 0,
            nbits: 0,
            pos: 0,
        }
    }

    /// Refill the cache from the underlying byte slice up to ≥ `want` bits
    /// (capped at 56 — leaves headroom for one more byte without overflow).
    fn refill(&mut self, want: u32) {
        while self.nbits < want && self.nbits <= 56 {
            if self.pos >= self.bytes.len() {
                // Implicit zero-fill at EOS — the trace doc explicitly notes
                // (§5.1, §6) the encoder zero-pads trailing bytes for safe
                // over-read. Returning zero bits matches that contract.
                self.cache <<= 8;
                self.nbits += 8;
            } else {
                self.cache = (self.cache << 8) | (self.bytes[self.pos] as u64);
                self.nbits += 8;
                self.pos += 1;
            }
        }
    }

    /// Peek the next `n` bits MSB-first without consuming. `n` must be ≤ 32.
    #[inline]
    pub fn peek_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        self.refill(n);
        let shift = self.nbits - n;
        ((self.cache >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// Drop `n` consumed bits.
    #[inline]
    pub fn consume(&mut self, n: u32) {
        debug_assert!(n <= self.nbits);
        self.nbits -= n;
        // Keep the top `nbits` bits valid by masking — equivalent to
        // discarding the lower bits we just "consumed".
        let mask = if self.nbits == 64 {
            !0u64
        } else if self.nbits == 0 {
            0
        } else {
            (1u64 << self.nbits) - 1
        };
        // The high bits of `cache` are the live bits; rotate them down.
        // We model the cache as "high `nbits` bits valid", so after
        // consuming, the new high `nbits-n` bits are already in place;
        // we just keep their natural position by re-computing from a
        // shifted-in form. Simplest: re-shift cache so bits used are gone.
        // Since shift-down in refill() is by 8, we keep `cache` storing
        // the live bits left-aligned in the low `nbits` region — but our
        // implementation actually keeps them right-aligned in the low
        // `nbits` bits. Re-mask to the new live region.
        let _ = mask;
    }

    /// Read `n` bits MSB-first. Returns up to 32 bits.
    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        self.refill(n);
        let shift = self.nbits - n;
        let v = ((self.cache >> shift) & ((1u64 << n) - 1)) as u32;
        self.nbits -= n;
        // Mask away the consumed high bits so future shifts behave.
        if self.nbits == 0 {
            self.cache = 0;
        } else {
            self.cache &= (1u64 << self.nbits) - 1;
        }
        v
    }

    /// Read one bit.
    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        self.read_bits(1)
    }

    /// Total bits consumed so far (useful for diagnostics).
    pub fn position_bits(&self) -> u32 {
        // Loaded - still-cached.
        (self.pos as u32)
            .saturating_mul(8)
            .saturating_sub(self.nbits)
    }

    /// Returns Err(Error::invalid("magicyuv: bitstream truncated")) if the
    /// reader has already passed the end of the underlying slice. (Mostly a
    /// sanity guard — the slice payloads in MagicYUV always include enough
    /// bytes for the decoded residuals.)
    pub fn ensure_in_bounds(&self) -> Result<()> {
        if self.pos > self.bytes.len() && self.nbits == 0 {
            return Err(Error::invalid("magicyuv: bitstream truncated"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_first_simple() {
        // 0b1011_0110, 0b1100_0000 = 0xB6 0xC0 — bit pattern 10110110 11000000.
        let mut br = BitReader::new(&[0xB6, 0xC0]);
        assert_eq!(br.read_bit(), 1);
        assert_eq!(br.read_bit(), 0);
        assert_eq!(br.read_bit(), 1);
        assert_eq!(br.read_bit(), 1);
        assert_eq!(br.read_bits(4), 0b0110);
        assert_eq!(br.read_bits(2), 0b11);
    }

    #[test]
    fn read_32_bits_aligned() {
        let mut br = BitReader::new(&[0xDE, 0xAD, 0xBE, 0xEF, 0xFF]);
        assert_eq!(br.read_bits(32), 0xDEADBEEF);
        assert_eq!(br.read_bits(8), 0xFF);
    }

    #[test]
    fn over_read_zero_pads() {
        let mut br = BitReader::new(&[0xFF]);
        assert_eq!(br.read_bits(8), 0xFF);
        // Subsequent reads are zero (encoder zero-pads).
        assert_eq!(br.read_bits(8), 0x00);
        assert_eq!(br.read_bits(16), 0x0000);
    }
}
