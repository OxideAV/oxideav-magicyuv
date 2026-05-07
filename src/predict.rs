//! In-place predictor reconstruction per `spec/04` §4.
//!
//! Each predictor takes a slice's residual buffer (already filled by
//! Huffman / raw decode) and rewrites it in-place to reconstructed
//! pixel values. The buffer layout is plane row-major: `data[r * w +
//! c]` is the residual at `(r, c)` on entry, the reconstructed pixel
//! on exit.
//!
//! The 8-bit path uses `u8` directly; the 10/12/14-bit path uses `u16`
//! with an explicit `& MAX` mask after every add (`MAX = (1 << bits) -
//! 1`).
//!
//! The 8-bit Median formula is the **modular** variant per `spec/04`
//! §4.4 round-1 validation-corrected note.  The 10/12/14-bit Medians
//! are **standard JPEG-LS** per the round-2 validation correction
//! ("only the 8-bit Median uses the modular formula"). The two formulas
//! agree when the gradient is naturally inside `[min, max]`; they
//! diverge only on over/underflow.
//!
//! Slice independence: each call to `apply_*` is given the slice's
//! own residual block. There is no carry-over from prior slices —
//! `spec/04` §5.2 establishes that each slice starts fresh, so
//! predictor state initialises at row 0 column 0 of `data` and
//! propagates within the slice.
//!
//! Interlaced (`flags & FLAG_INTERLACED == 2`, `spec/04` §5.1
//! round-2 correction): top neighbour is `r - 2` (not `r - 1`); the
//! first **two** rows of each slice have no top neighbour and behave
//! like row 0 of a progressive slice (column 0 = residual itself,
//! columns 1+ use Left across the row).

use crate::tables::PredictorKind;

/// Field-stride for prediction. Progressive frames use stride 1
/// (top = previous row); interlaced frames use stride 2
/// (top = row r-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldStride(pub u8);

impl FieldStride {
    pub const PROGRESSIVE: Self = Self(1);
    pub const INTERLACED: Self = Self(2);
}

/// Apply the slice's predictor `kind` in-place to a row-major 8-bit
/// residual buffer of size `(rows × width)`. After the call the
/// buffer holds reconstructed pixel values modulo 256.
pub fn apply_u8(kind: PredictorKind, data: &mut [u8], rows: usize, width: usize) {
    apply_u8_with_stride(kind, data, rows, width, FieldStride::PROGRESSIVE);
}

/// Same as [`apply_u8`] but honours `field_stride` for interlaced
/// streams (`spec/04` §5.1 round-2 corrected note).
pub fn apply_u8_with_stride(
    kind: PredictorKind,
    data: &mut [u8],
    rows: usize,
    width: usize,
    field_stride: FieldStride,
) {
    debug_assert_eq!(data.len(), rows * width);
    if rows == 0 || width == 0 {
        return;
    }
    let fs = field_stride.0 as usize;
    let header_rows = fs.min(rows);
    // Header rows: each treated like a progressive row 0 — Left across.
    for hr in 0..header_rows {
        let row = &mut data[hr * width..(hr + 1) * width];
        for c in 1..width {
            row[c] = row[c].wrapping_add(row[c - 1]);
        }
    }
    if rows <= header_rows {
        return;
    }
    // Round-3 perf: split `data` into the previous-row reference + the
    // current-row mutable slice once per row. The compiler can then
    // elide the per-element bounds check on `cur[c]` and `prev[c]`
    // — the slice length is `width`, and `c` is ≤ width-1 in the loop
    // ‹⇒ within bounds›.
    match kind {
        PredictorKind::Left => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]);
                for c in 1..width {
                    cur[c] = cur[c].wrapping_add(cur[c - 1]);
                }
            }
        }
        PredictorKind::Gradient => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]);
                for c in 1..width {
                    let left = cur[c - 1];
                    let top = prev[c];
                    let top_left = prev[c - 1];
                    let pred = left.wrapping_add(top).wrapping_sub(top_left);
                    cur[c] = cur[c].wrapping_add(pred);
                }
            }
        }
        PredictorKind::Median => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]);
                for c in 1..width {
                    let left = cur[c - 1];
                    let top = prev[c];
                    let top_left = prev[c - 1];
                    // Modular 8-bit Median (spec/04 §4.4 round-1 note).
                    let gradient = left.wrapping_add(top).wrapping_sub(top_left);
                    let lo = left.min(top);
                    let hi = left.max(top);
                    let pred = if gradient < lo {
                        lo
                    } else if gradient > hi {
                        hi
                    } else {
                        gradient
                    };
                    cur[c] = cur[c].wrapping_add(pred);
                }
            }
        }
    }
}

/// Apply the slice's predictor `kind` in-place to a row-major
/// 10/12/14-bit residual buffer (samples stored as `u16`, but only the
/// low `bits` are significant — the caller guarantees `mask = (1 <<
/// bits) - 1`).
///
/// 10/12/14-bit Median is the **standard JPEG-LS** clip-on-extremes
/// predictor (`spec/04` §4.4 round-2 correction).
pub fn apply_u16(kind: PredictorKind, data: &mut [u16], rows: usize, width: usize, mask: u16) {
    apply_u16_with_stride(kind, data, rows, width, mask, FieldStride::PROGRESSIVE);
}

/// Same as [`apply_u16`] but honours `field_stride`.
pub fn apply_u16_with_stride(
    kind: PredictorKind,
    data: &mut [u16],
    rows: usize,
    width: usize,
    mask: u16,
    field_stride: FieldStride,
) {
    debug_assert_eq!(data.len(), rows * width);
    if rows == 0 || width == 0 {
        return;
    }
    let fs = field_stride.0 as usize;
    let header_rows = fs.min(rows);
    for hr in 0..header_rows {
        let row = &mut data[hr * width..(hr + 1) * width];
        for c in 1..width {
            row[c] = row[c].wrapping_add(row[c - 1]) & mask;
        }
    }
    if rows <= header_rows {
        return;
    }
    // See `apply_u8_with_stride` for the row-split rationale.
    match kind {
        PredictorKind::Left => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]) & mask;
                for c in 1..width {
                    cur[c] = cur[c].wrapping_add(cur[c - 1]) & mask;
                }
            }
        }
        PredictorKind::Gradient => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]) & mask;
                for c in 1..width {
                    let left = cur[c - 1];
                    let top = prev[c];
                    let top_left = prev[c - 1];
                    let pred = left.wrapping_add(top).wrapping_sub(top_left);
                    cur[c] = cur[c].wrapping_add(pred) & mask;
                }
            }
        }
        PredictorKind::Median => {
            for r in header_rows..rows {
                let (head, tail) = data.split_at_mut(r * width);
                let prev = &head[(r - fs) * width..(r - fs) * width + width];
                let cur = &mut tail[..width];
                cur[0] = cur[0].wrapping_add(prev[0]) & mask;
                for c in 1..width {
                    let left = cur[c - 1];
                    let top = prev[c];
                    let top_left = prev[c - 1];
                    // Standard JPEG-LS Median Edge Detector
                    // (spec/04 §4.4 round-2 corrected note). Use
                    // i32 to compute the un-wrapped gradient.
                    let lo = left.min(top);
                    let hi = left.max(top);
                    let pred = if top_left >= hi {
                        lo
                    } else if top_left <= lo {
                        hi
                    } else {
                        // left + top - top_left, full precision (the
                        // `top_left` strictly between min and max
                        // guarantees the result fits in `[0, 2*MAX]`).
                        let r32 = (left as i32)
                            .wrapping_add(top as i32)
                            .wrapping_sub(top_left as i32);
                        (r32 as u16) & mask
                    };
                    cur[c] = cur[c].wrapping_add(pred) & mask;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoder-side counterpart of `apply_left_u8`. Used by the
    /// roundtrip tests below.
    pub(crate) fn encode_left_u8(data: &mut [u8], rows: usize, width: usize) {
        encode_left_u8_with_stride(data, rows, width, FieldStride::PROGRESSIVE)
    }

    pub(crate) fn encode_left_u8_with_stride(
        data: &mut [u8],
        rows: usize,
        width: usize,
        field_stride: FieldStride,
    ) {
        if rows == 0 || width == 0 {
            return;
        }
        let fs = field_stride.0 as usize;
        let header_rows = fs.min(rows);
        for r in (0..rows).rev() {
            let row_off = r * width;
            for c in (1..width).rev() {
                data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
            }
            if r >= header_rows {
                let prev0 = data[(r - fs) * width];
                data[row_off] = data[row_off].wrapping_sub(prev0);
            }
        }
    }

    pub(crate) fn encode_gradient_u8(data: &mut [u8], rows: usize, width: usize) {
        encode_gradient_u8_with_stride(data, rows, width, FieldStride::PROGRESSIVE)
    }

    pub(crate) fn encode_gradient_u8_with_stride(
        data: &mut [u8],
        rows: usize,
        width: usize,
        field_stride: FieldStride,
    ) {
        if rows == 0 || width == 0 {
            return;
        }
        let fs = field_stride.0 as usize;
        let header_rows = fs.min(rows);
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r < header_rows {
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
            } else {
                let prev_off = (r - fs) * width;
                for c in (1..width).rev() {
                    let left = data[row_off + c - 1];
                    let top = data[prev_off + c];
                    let top_left = data[prev_off + c - 1];
                    let pred = left.wrapping_add(top).wrapping_sub(top_left);
                    data[row_off + c] = data[row_off + c].wrapping_sub(pred);
                }
                data[row_off] = data[row_off].wrapping_sub(data[prev_off]);
            }
        }
    }

    pub(crate) fn encode_median_u8(data: &mut [u8], rows: usize, width: usize) {
        encode_median_u8_with_stride(data, rows, width, FieldStride::PROGRESSIVE)
    }

    pub(crate) fn encode_median_u8_with_stride(
        data: &mut [u8],
        rows: usize,
        width: usize,
        field_stride: FieldStride,
    ) {
        if rows == 0 || width == 0 {
            return;
        }
        let fs = field_stride.0 as usize;
        let header_rows = fs.min(rows);
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r < header_rows {
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
            } else {
                let prev_off = (r - fs) * width;
                for c in (1..width).rev() {
                    let left = data[row_off + c - 1];
                    let top = data[prev_off + c];
                    let top_left = data[prev_off + c - 1];
                    let gradient = left.wrapping_add(top).wrapping_sub(top_left);
                    let lo = left.min(top);
                    let hi = left.max(top);
                    let pred = if gradient < lo {
                        lo
                    } else if gradient > hi {
                        hi
                    } else {
                        gradient
                    };
                    data[row_off + c] = data[row_off + c].wrapping_sub(pred);
                }
                data[row_off] = data[row_off].wrapping_sub(data[prev_off]);
            }
        }
    }

    /// Encoder-side u16 counterparts (used by the roundtrip tests for
    /// the high-bit-depth path).
    pub(crate) fn encode_left_u16(data: &mut [u16], rows: usize, width: usize, mask: u16) {
        if rows == 0 || width == 0 {
            return;
        }
        for r in (0..rows).rev() {
            let row_off = r * width;
            for c in (1..width).rev() {
                data[row_off + c] = (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
            }
            if r >= 1 {
                let prev0 = data[(r - 1) * width];
                data[row_off] = (data[row_off].wrapping_sub(prev0)) & mask;
            }
        }
    }

    pub(crate) fn encode_gradient_u16(data: &mut [u16], rows: usize, width: usize, mask: u16) {
        if rows == 0 || width == 0 {
            return;
        }
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r == 0 {
                for c in (1..width).rev() {
                    data[row_off + c] =
                        (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
                }
            } else {
                let prev_off = (r - 1) * width;
                for c in (1..width).rev() {
                    let left = data[row_off + c - 1];
                    let top = data[prev_off + c];
                    let top_left = data[prev_off + c - 1];
                    let pred = left.wrapping_add(top).wrapping_sub(top_left);
                    data[row_off + c] = (data[row_off + c].wrapping_sub(pred)) & mask;
                }
                data[row_off] = (data[row_off].wrapping_sub(data[prev_off])) & mask;
            }
        }
    }

    pub(crate) fn encode_median_u16_jpegls(data: &mut [u16], rows: usize, width: usize, mask: u16) {
        if rows == 0 || width == 0 {
            return;
        }
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r == 0 {
                for c in (1..width).rev() {
                    data[row_off + c] =
                        (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
                }
            } else {
                let prev_off = (r - 1) * width;
                for c in (1..width).rev() {
                    let left = data[row_off + c - 1];
                    let top = data[prev_off + c];
                    let top_left = data[prev_off + c - 1];
                    let lo = left.min(top);
                    let hi = left.max(top);
                    let pred = if top_left >= hi {
                        lo
                    } else if top_left <= lo {
                        hi
                    } else {
                        let r32 = (left as i32)
                            .wrapping_add(top as i32)
                            .wrapping_sub(top_left as i32);
                        (r32 as u16) & mask
                    };
                    data[row_off + c] = (data[row_off + c].wrapping_sub(pred)) & mask;
                }
                data[row_off] = (data[row_off].wrapping_sub(data[prev_off])) & mask;
            }
        }
    }

    #[test]
    fn left_roundtrips_random_8x8() {
        for seed in 0..16u32 {
            let mut orig = pseudo_random_8x8(seed);
            let snapshot = orig.clone();
            encode_left_u8(&mut orig, 8, 8);
            apply_u8(PredictorKind::Left, &mut orig, 8, 8);
            assert_eq!(orig, snapshot, "left predictor must round-trip");
        }
    }

    #[test]
    fn gradient_roundtrips_random_8x8() {
        for seed in 0..16u32 {
            let mut orig = pseudo_random_8x8(seed);
            let snapshot = orig.clone();
            encode_gradient_u8(&mut orig, 8, 8);
            apply_u8(PredictorKind::Gradient, &mut orig, 8, 8);
            assert_eq!(orig, snapshot, "gradient predictor must round-trip");
        }
    }

    #[test]
    fn median_roundtrips_random_8x8() {
        for seed in 0..16u32 {
            let mut orig = pseudo_random_8x8(seed);
            let snapshot = orig.clone();
            encode_median_u8(&mut orig, 8, 8);
            apply_u8(PredictorKind::Median, &mut orig, 8, 8);
            assert_eq!(orig, snapshot, "median predictor must round-trip");
        }
    }

    fn pseudo_random_8x8(seed: u32) -> Vec<u8> {
        let mut s: u32 = 0x5113c4f5_u32.wrapping_add(seed.wrapping_mul(0x9e37_79b9));
        let mut v = vec![0u8; 64];
        for b in v.iter_mut() {
            // splitmix-ish, output low byte.
            s = s.wrapping_mul(0x9e37_79b9).wrapping_add(0x6534_3aa1);
            *b = (s >> 16) as u8;
        }
        v
    }

    fn pseudo_random_8x8_u16(seed: u32, mask: u16) -> Vec<u16> {
        let mut s: u32 = 0x5113c4f5_u32.wrapping_add(seed.wrapping_mul(0x9e37_79b9));
        let mut v = vec![0u16; 64];
        for b in v.iter_mut() {
            s = s.wrapping_mul(0x9e37_79b9).wrapping_add(0x6534_3aa1);
            *b = ((s >> 11) as u16) & mask;
        }
        v
    }

    #[test]
    fn left_u16_roundtrips_random_8x8() {
        for &mask in &[0x3ffu16, 0xfff, 0x3fff] {
            for seed in 0..16u32 {
                let mut orig = pseudo_random_8x8_u16(seed, mask);
                let snapshot = orig.clone();
                encode_left_u16(&mut orig, 8, 8, mask);
                apply_u16(PredictorKind::Left, &mut orig, 8, 8, mask);
                assert_eq!(orig, snapshot, "left u16 predictor must round-trip");
            }
        }
    }

    #[test]
    fn gradient_u16_roundtrips_random_8x8() {
        for &mask in &[0x3ffu16, 0xfff, 0x3fff] {
            for seed in 0..16u32 {
                let mut orig = pseudo_random_8x8_u16(seed, mask);
                let snapshot = orig.clone();
                encode_gradient_u16(&mut orig, 8, 8, mask);
                apply_u16(PredictorKind::Gradient, &mut orig, 8, 8, mask);
                assert_eq!(orig, snapshot, "gradient u16 predictor must round-trip");
            }
        }
    }

    #[test]
    fn median_u16_jpegls_roundtrips_random_8x8() {
        for &mask in &[0x3ffu16, 0xfff, 0x3fff] {
            for seed in 0..16u32 {
                let mut orig = pseudo_random_8x8_u16(seed, mask);
                let snapshot = orig.clone();
                encode_median_u16_jpegls(&mut orig, 8, 8, mask);
                apply_u16(PredictorKind::Median, &mut orig, 8, 8, mask);
                assert_eq!(orig, snapshot, "median u16 predictor must round-trip");
            }
        }
    }

    #[test]
    fn all_zero_residuals_yield_all_zero_pixels() {
        let mut buf = vec![0u8; 32 * 16];
        apply_u8(PredictorKind::Left, &mut buf, 16, 32);
        assert!(buf.iter().all(|&b| b == 0));

        let mut buf = vec![0u8; 32 * 16];
        apply_u8(PredictorKind::Gradient, &mut buf, 16, 32);
        assert!(buf.iter().all(|&b| b == 0));

        let mut buf = vec![0u8; 32 * 16];
        apply_u8(PredictorKind::Median, &mut buf, 16, 32);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn left_u8_interlaced_roundtrip() {
        // 4×8 frame, interlaced: two field-firsts (rows 0 and 1).
        for seed in 0..16u32 {
            let mut orig: Vec<u8> = (0..(4 * 8))
                .map(|i| ((i as u32).wrapping_mul(0x9e37_79b9 ^ seed) >> 16) as u8)
                .collect();
            let snapshot = orig.clone();
            encode_left_u8_with_stride(&mut orig, 8, 4, FieldStride::INTERLACED);
            apply_u8_with_stride(
                PredictorKind::Left,
                &mut orig,
                8,
                4,
                FieldStride::INTERLACED,
            );
            assert_eq!(orig, snapshot, "interlaced left predictor must round-trip");
        }
    }

    #[test]
    fn median_u8_interlaced_roundtrip() {
        for seed in 0..16u32 {
            let mut orig: Vec<u8> = (0..(4 * 8))
                .map(|i| ((i as u32).wrapping_mul(0x9e37_79b9 ^ seed) >> 16) as u8)
                .collect();
            let snapshot = orig.clone();
            encode_median_u8_with_stride(&mut orig, 8, 4, FieldStride::INTERLACED);
            apply_u8_with_stride(
                PredictorKind::Median,
                &mut orig,
                8,
                4,
                FieldStride::INTERLACED,
            );
            assert_eq!(
                orig, snapshot,
                "interlaced median predictor must round-trip"
            );
        }
    }
}
