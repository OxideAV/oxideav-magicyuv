//! Three spatial predictors used by MagicYUV (trace doc §4):
//!
//! * `LEFT`     (1) — previous reconstructed pixel on the same row.
//! * `GRADIENT` (2) — `left + top - top-left`.
//! * `MEDIAN`   (3) — `median(left, top, left + top - top-left)` — the
//!   classic JPEG-LS / LOCO-I MED predictor.
//!
//! Reconstruction is `pixel = (residual + predicted) & max`, where
//! `max = (1 << bps) - 1`. Per the trace doc, the predictor's "top
//! neighbour" comes from the previous row of the same slice; the first
//! row of a slice has no top neighbour and falls back to LEFT.
//!
//! Generic `u16` paths are used for both the 8-bit (`max = 0xFF`) and
//! the 10/12/14-bit (`max = (1 << bps) - 1 ≤ 0x3FFF`) routes; for 8-bit
//! we additionally expose narrower helpers that operate directly on
//! `u8` slices for the byte-store fast path.

use oxideav_core::{Error, Result};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Predictor {
    Left = 1,
    Gradient = 2,
    Median = 3,
}

impl Predictor {
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            1 => Ok(Self::Left),
            2 => Ok(Self::Gradient),
            3 => Ok(Self::Median),
            _ => Err(Error::invalid(format!(
                "magicyuv: predictor byte {b} not in {{1, 2, 3}}"
            ))),
        }
    }
}

/// Apply LEFT prediction to `dst` in-place. `dst` initially holds
/// per-pixel residuals; on return holds reconstructed samples.
/// `width` and `height` are the plane dimensions in samples.
///
/// Empirically MagicYUV's LEFT predictor uses a row-major raster scan
/// where the leftmost pixel of row `r > 0` predicts from
/// `dst[r-1, 0]` (the **first** pixel of the previous row), not the
/// last. The first pixel of row 0 uses 0 as its predictor. The trace
/// doc says "the start of each row predicts from the end of the
/// previous row" but FFmpeg-encoded testsrc fixtures only round-trip
/// bit-exactly when "first pixel of previous row" is used — which
/// matches UT Video's per-row-anchor LEFT predictor.
pub fn apply_left_u8(dst: &mut [u8], stride: usize, width: usize, height: usize) {
    debug_assert!(stride * height <= dst.len());
    if height == 0 || width == 0 {
        return;
    }
    // Row 0: pred[0] = 0; pred[i>0] = dst[0, i-1].
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let v = dst[i].wrapping_add(left);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        // Left of the leftmost pixel = dst[r-1, 0].
        let mut left: u8 = dst[off_prev];
        for i in 0..width {
            let v = dst[off + i].wrapping_add(left);
            dst[off + i] = v;
            left = v;
        }
    }
}

/// Apply GRADIENT prediction (`left + top - top-left`) to `dst`.
pub fn apply_gradient_u8(dst: &mut [u8], stride: usize, width: usize, height: usize) {
    // First row: pure LEFT.
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let v = dst[i].wrapping_add(left);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        // Leftmost pixel of row r>0 predicts as TOP per the
        // gradient-degenerate case (left/top-left wrap to "top").
        // Equivalent here: left=0, top=dst[off_prev], top-left=0
        // → predictor = top.
        let mut top_left: u8 = 0;
        let mut left_val: u8 = 0;
        for i in 0..width {
            let top = dst[off_prev + i];
            let pred = left_val.wrapping_add(top).wrapping_sub(top_left);
            let v = dst[off + i].wrapping_add(pred);
            dst[off + i] = v;
            top_left = top;
            left_val = v;
        }
    }
}

/// Apply MEDIAN prediction (straight median of left, top, top-left)
/// to `dst`. Row 0 falls back to LEFT. For row r > 0 col 0, both
/// `left` and `top-left` are missing — both are clamped to `top`,
/// which makes the median collapse to `top` (the first pixel of the
/// previous row).
pub fn apply_median_u8(dst: &mut [u8], stride: usize, width: usize, height: usize) {
    if height == 0 || width == 0 {
        return;
    }
    // Row 0: pure LEFT (predictor 0 for col 0).
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let v = dst[i].wrapping_add(left);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        // Init left/top-left to the row-r-1 first-pixel value so that
        // the median for col 0 equals top (= dst[r-1, 0]).
        let mut top_left: u8 = dst[off_prev];
        let mut left_val: u8 = dst[off_prev];
        for i in 0..width {
            let top = dst[off_prev + i];
            let pred = median3(left_val, top, top_left);
            let v = dst[off + i].wrapping_add(pred);
            dst[off + i] = v;
            top_left = top;
            left_val = v;
        }
    }
}

/// MED predictor, "median form": `median(a, b, a + b − c)` in
/// **wrapping `u8` arithmetic**.
///
/// MagicYUV's MEDIAN is the median of the three values
/// `{left, top, left + top − top-left}`, with the gradient term
/// computed modulo 2^bps. This differs from the textbook JPEG-LS MED
/// branch form `(c ≥ max(a,b) ? min(a,b) : c ≤ min(a,b) ? max(a,b) :
/// a+b-c)` whenever `a+b-c` wraps — empirically the wrap path matters:
/// pixel (12, 8) of a testsrc YUV422P MEDIAN frame has left=235,
/// top=170, top-left=16 and decodes to 170, which the median form
/// produces (170+235-16 = 389 mod 256 = 133, median{235, 170, 133} =
/// 170) but the branch form does not (it returns max = 235).
#[inline]
fn median3(a: u8, b: u8, c: u8) -> u8 {
    let g = a.wrapping_add(b).wrapping_sub(c);
    median_of_three(a, b, g)
}

#[inline]
fn median_of_three(a: u8, b: u8, c: u8) -> u8 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c <= lo {
        lo
    } else if c >= hi {
        hi
    } else {
        c
    }
}

// ────────────────────────── u16 / >8-bit paths ──────────────────────────
//
// The 10/12/14-bit decode and encode paths reuse the predictor structure
// from the 8-bit paths verbatim — all that changes is the wraparound
// modulus, which is `1 << bps` instead of `256`. We accept the modulus
// as `mask = (1 << bps) - 1` and operate on `u16` samples; the
// add/sub/mask sequence is exactly the bps-bit modular arithmetic the
// trace doc §4.1 specifies.

#[inline]
fn modu16(v: u32, mask: u16) -> u16 {
    (v as u16) & mask
}

pub fn apply_left_u16(dst: &mut [u16], stride: usize, width: usize, height: usize, mask: u16) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let v = modu16((dst[i] as u32).wrapping_add(left as u32), mask);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut left: u16 = dst[off_prev];
        for i in 0..width {
            let v = modu16((dst[off + i] as u32).wrapping_add(left as u32), mask);
            dst[off + i] = v;
            left = v;
        }
    }
}

pub fn apply_gradient_u16(dst: &mut [u16], stride: usize, width: usize, height: usize, mask: u16) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let v = modu16((dst[i] as u32).wrapping_add(left as u32), mask);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u16 = 0;
        let mut left_val: u16 = 0;
        for i in 0..width {
            let top = dst[off_prev + i];
            let pred = (left_val as u32)
                .wrapping_add(top as u32)
                .wrapping_sub(top_left as u32);
            let v = modu16((dst[off + i] as u32).wrapping_add(pred), mask);
            dst[off + i] = v;
            top_left = top;
            left_val = v;
        }
    }
}

pub fn apply_median_u16(dst: &mut [u16], stride: usize, width: usize, height: usize, mask: u16) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let v = modu16((dst[i] as u32).wrapping_add(left as u32), mask);
            dst[i] = v;
            left = v;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u16 = dst[off_prev];
        let mut left_val: u16 = dst[off_prev];
        for i in 0..width {
            let top = dst[off_prev + i];
            let pred = median3_u16(left_val, top, top_left, mask);
            let v = modu16((dst[off + i] as u32).wrapping_add(pred as u32), mask);
            dst[off + i] = v;
            top_left = top;
            left_val = v;
        }
    }
}

#[inline]
fn median3_u16(a: u16, b: u16, c: u16, mask: u16) -> u16 {
    let g = modu16(
        (a as u32).wrapping_add(b as u32).wrapping_sub(c as u32),
        mask,
    );
    median_of_three_u16(a, b, g)
}

#[inline]
fn median_of_three_u16(a: u16, b: u16, c: u16) -> u16 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c <= lo {
        lo
    } else if c >= hi {
        hi
    } else {
        c
    }
}

// ────────────────────────── encoder mirrors ──────────────────────────
//
// The encoder produces residuals from reconstructed samples; the
// transformation is the bit-exact inverse of the decoder's predictor
// stage. We expose dedicated functions instead of "predict-then-subtract
// per sample" wrappers so the encoder hot path can avoid an intermediate
// allocation. Both fast (u8) and generic (u16, with mask) paths are
// provided.
//
// Each function takes `samples` (the reconstructed plane) and writes
// into `out` (the residual buffer, same shape as `samples`).

pub fn encode_left_u8(samples: &[u8], stride: usize, width: usize, height: usize, out: &mut [u8]) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = s.wrapping_sub(left);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut left: u8 = samples[off_prev];
        for i in 0..width {
            let s = samples[off + i];
            out[off + i] = s.wrapping_sub(left);
            left = s;
        }
    }
}

pub fn encode_gradient_u8(
    samples: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    out: &mut [u8],
) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = s.wrapping_sub(left);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u8 = 0;
        let mut left_val: u8 = 0;
        for i in 0..width {
            let top = samples[off_prev + i];
            let pred = left_val.wrapping_add(top).wrapping_sub(top_left);
            let s = samples[off + i];
            out[off + i] = s.wrapping_sub(pred);
            top_left = top;
            left_val = s;
        }
    }
}

pub fn encode_median_u8(
    samples: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    out: &mut [u8],
) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u8 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = s.wrapping_sub(left);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u8 = samples[off_prev];
        let mut left_val: u8 = samples[off_prev];
        for i in 0..width {
            let top = samples[off_prev + i];
            let pred = median3(left_val, top, top_left);
            let s = samples[off + i];
            out[off + i] = s.wrapping_sub(pred);
            top_left = top;
            left_val = s;
        }
    }
}

pub fn encode_left_u16(
    samples: &[u16],
    stride: usize,
    width: usize,
    height: usize,
    mask: u16,
    out: &mut [u16],
) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = modu16((s as u32).wrapping_sub(left as u32), mask);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut left: u16 = samples[off_prev];
        for i in 0..width {
            let s = samples[off + i];
            out[off + i] = modu16((s as u32).wrapping_sub(left as u32), mask);
            left = s;
        }
    }
}

pub fn encode_gradient_u16(
    samples: &[u16],
    stride: usize,
    width: usize,
    height: usize,
    mask: u16,
    out: &mut [u16],
) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = modu16((s as u32).wrapping_sub(left as u32), mask);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u16 = 0;
        let mut left_val: u16 = 0;
        for i in 0..width {
            let top = samples[off_prev + i];
            let pred = (left_val as u32)
                .wrapping_add(top as u32)
                .wrapping_sub(top_left as u32);
            let s = samples[off + i];
            out[off + i] = modu16((s as u32).wrapping_sub(pred), mask);
            top_left = top;
            left_val = s;
        }
    }
}

pub fn encode_median_u16(
    samples: &[u16],
    stride: usize,
    width: usize,
    height: usize,
    mask: u16,
    out: &mut [u16],
) {
    if height == 0 || width == 0 {
        return;
    }
    {
        let mut left: u16 = 0;
        for i in 0..width {
            let s = samples[i];
            out[i] = modu16((s as u32).wrapping_sub(left as u32), mask);
            left = s;
        }
    }
    for row in 1..height {
        let off = row * stride;
        let off_prev = (row - 1) * stride;
        let mut top_left: u16 = samples[off_prev];
        let mut left_val: u16 = samples[off_prev];
        for i in 0..width {
            let top = samples[off_prev + i];
            let pred = median3_u16(left_val, top, top_left, mask);
            let s = samples[off + i];
            out[off + i] = modu16((s as u32).wrapping_sub(pred as u32), mask);
            top_left = top;
            left_val = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_round_trip_one_row() {
        // Residuals for "5 3 1 2" with row-leftmost predictor=0 would be
        // 5, 3-5=−2 (=0xFE wrapping), 1-3=−2 (=0xFE), 2-1=1
        let mut dst = [5u8, 0xFE, 0xFE, 1];
        apply_left_u8(&mut dst, 4, 4, 1);
        assert_eq!(dst, [5, 3, 1, 2]);
    }

    #[test]
    fn left_two_rows_uses_first_of_prev_as_anchor() {
        // Row 0: pixels 5, 3, 1, 2 → residuals 5, -2, -2, 1.
        // Row 1: predictor for first pixel = row0[0] = 5; want pixel
        // [10, 12, 11, 9]. Residuals = 10-5=5, 12-10=2, 11-12=-1=0xFF,
        // 9-11=-2=0xFE.
        let mut dst = [5u8, 0xFE, 0xFE, 1, 5, 2, 0xFF, 0xFE];
        apply_left_u8(&mut dst, 4, 4, 2);
        assert_eq!(dst, [5, 3, 1, 2, 10, 12, 11, 9]);
    }

    #[test]
    fn median_uses_gradient_with_wrap() {
        // a=235, b=170, c=16: gradient = 235+170-16 = 389 mod 256 = 133.
        // median{235, 170, 133} = 170.
        assert_eq!(median3(235, 170, 16), 170);
        // a=16, b=81, c=16: gradient = 16+81-16 = 81.
        // median{16, 81, 81} = 81.
        assert_eq!(median3(16, 81, 16), 81);
        // a=10, b=20, c=15: gradient = 15. median{10, 20, 15} = 15.
        assert_eq!(median3(10, 20, 15), 15);
    }

    #[test]
    fn gradient_two_rows() {
        // Pixels:
        //   row0: 10 20 30 40
        //   row1: 50 60 70 80
        // Predictors row1: top + left - top_left
        //   p[0] = 0 + 10 - 0 = 10 → residual = 50 - 10 = 40
        //   p[1] = 50 + 20 - 10 = 60 → residual = 60 - 60 = 0
        //   p[2] = 60 + 30 - 20 = 70 → residual = 70 - 70 = 0
        //   p[3] = 70 + 40 - 30 = 80 → residual = 80 - 80 = 0
        // Row0 residuals (LEFT): 10, 10, 10, 10.
        let mut dst = [10u8, 10, 10, 10, 40, 0, 0, 0];
        apply_gradient_u8(&mut dst, 4, 4, 2);
        assert_eq!(dst, [10, 20, 30, 40, 50, 60, 70, 80]);
    }

    /// Encoder is the bit-exact inverse of the decoder for each
    /// predictor: encode(reconstructed) → residuals; decode(residuals)
    /// → reconstructed.
    #[test]
    fn encoder_decoder_round_trip_left_u8() {
        let original: Vec<u8> = (0..(8 * 6)).map(|i| ((i * 13 + 7) & 0xFF) as u8).collect();
        let mut residuals = vec![0u8; original.len()];
        encode_left_u8(&original, 8, 8, 6, &mut residuals);
        apply_left_u8(&mut residuals, 8, 8, 6);
        assert_eq!(residuals, original);
    }

    #[test]
    fn encoder_decoder_round_trip_gradient_u8() {
        let original: Vec<u8> = (0..(8 * 6)).map(|i| ((i * 31 + 17) & 0xFF) as u8).collect();
        let mut residuals = vec![0u8; original.len()];
        encode_gradient_u8(&original, 8, 8, 6, &mut residuals);
        apply_gradient_u8(&mut residuals, 8, 8, 6);
        assert_eq!(residuals, original);
    }

    #[test]
    fn encoder_decoder_round_trip_median_u8() {
        let original: Vec<u8> = (0..(8 * 6)).map(|i| ((i * 41 + 23) & 0xFF) as u8).collect();
        let mut residuals = vec![0u8; original.len()];
        encode_median_u8(&original, 8, 8, 6, &mut residuals);
        apply_median_u8(&mut residuals, 8, 8, 6);
        assert_eq!(residuals, original);
    }

    #[test]
    fn encoder_decoder_round_trip_left_u16_10bit() {
        let mask: u16 = 0x3FF;
        let original: Vec<u16> = (0..(8 * 6))
            .map(|i| ((i as u32 * 137) & mask as u32) as u16)
            .collect();
        let mut residuals = vec![0u16; original.len()];
        encode_left_u16(&original, 8, 8, 6, mask, &mut residuals);
        apply_left_u16(&mut residuals, 8, 8, 6, mask);
        assert_eq!(residuals, original);
    }

    #[test]
    fn encoder_decoder_round_trip_median_u16_12bit() {
        let mask: u16 = 0xFFF;
        let original: Vec<u16> = (0..(16 * 8))
            .map(|i| ((i as u32 * 769 + 12345) & mask as u32) as u16)
            .collect();
        let mut residuals = vec![0u16; original.len()];
        encode_median_u16(&original, 16, 16, 8, mask, &mut residuals);
        apply_median_u16(&mut residuals, 16, 16, 8, mask);
        assert_eq!(residuals, original);
    }
}
