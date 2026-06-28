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

    /// Direct assertions of the `spec/04` §4.4 bit-depth-conditional
    /// Median formula — the subtlest rule in the codec, where the
    /// 8-bit path uses the **modular** gradient `(left + top - top_left)
    /// & 0xff` and the 10/12/14-bit path uses the **full-precision**
    /// JPEG-LS gradient `left + top - top_left`. The two diverge only
    /// when the raw gradient falls outside `[0, 2^bits)`; the round-trip
    /// sweeps prove self-consistency but cannot distinguish the two
    /// formulas (an encoder/decoder pair would round-trip with *either*
    /// rule). These tests pin the absolute reconstructed value against
    /// the spec's own worked examples, so a regression that swapped the
    /// 8-bit path to standard JPEG-LS (or the HBD path to modular) would
    /// fail here even though every round-trip test still passed.
    mod median_formula_spec_4_4 {
        use super::*;

        /// Drive a 2×2 plane so that decoding row 1 column 1 sees the
        /// exact `(left, top, top_left)` triple, with a zero residual at
        /// `[1,1]` so the reconstructed pixel *is* the predictor.
        /// Returns `px[1,1]`. `MAX = (1<<bits)-1`.
        fn med_pred_u8(left: u8, top: u8, top_left: u8) -> u8 {
            // px[0,0] = res[0,0]                 = top_left
            // px[0,1] = px[0,0] + res[0,1]       = top
            // px[1,0] = px[0,0] + res[1,0]       = left
            // px[1,1] = MED(left, top, top_left) + res[1,1] (=0)
            let mut buf = vec![
                top_left,
                top.wrapping_sub(top_left),
                left.wrapping_sub(top_left),
                0,
            ];
            apply_u8(PredictorKind::Median, &mut buf, 2, 2);
            assert_eq!(buf[0], top_left);
            assert_eq!(buf[1], top);
            assert_eq!(buf[2], left);
            buf[3]
        }

        fn med_pred_u16(left: u16, top: u16, top_left: u16, mask: u16) -> u16 {
            let mut buf = vec![
                top_left & mask,
                top.wrapping_sub(top_left) & mask,
                left.wrapping_sub(top_left) & mask,
                0,
            ];
            apply_u16(PredictorKind::Median, &mut buf, 2, 2, mask);
            assert_eq!(buf[0], top_left & mask);
            assert_eq!(buf[1], top & mask);
            assert_eq!(buf[2], left & mask);
            buf[3]
        }

        /// `spec/04` §4.4 8-bit worked example:
        /// `left=10, top=20, top_left=200`. Modular gradient
        /// `(10 + 20 - 200) & 0xff = 86`; `86 > max(10,20)=20` ⇒ clip
        /// to 20. (Standard JPEG-LS would give `clip(-170, 10, 20) = 10`,
        /// so this value distinguishes the two formulas.)
        #[test]
        fn modular_8bit_worked_example() {
            assert_eq!(
                med_pred_u8(10, 20, 200),
                20,
                "8-bit Median must use the modular gradient (spec/04 §4.4)"
            );
        }

        /// `spec/04` §4.4 14-bit worked example, *scaled into range*.
        /// The spec illustrates the JPEG-LS clip with the triple
        /// `a=24576, b=16384, c=12288` (values chosen for arithmetic
        /// clarity; they exceed the 14-bit max `0x3fff = 16383`, so they
        /// are not valid in-range samples and cannot be reproduced as
        /// literal decode neighbours). The *rule* the example
        /// demonstrates is "`top_left ≤ min(left, top)` ⇒ return
        /// `max(left, top)`". We pin that exact branch with an in-range
        /// 14-bit triple that triggers it: `left=12000, top=8000,
        /// top_left=4000` — `top_left=4000 ≤ min=8000` ⇒ `max=12000`.
        /// (The modular rule would compute `(12000+8000-4000) & 0x3fff =
        /// 16000`, then `16000 > max=12000` ⇒ clip to 12000 — agreeing
        /// here — so for a divergence point see `hbd_diverges_from_modular`.)
        #[test]
        fn jpegls_14bit_clip_on_underflow() {
            assert_eq!(
                med_pred_u16(12000, 8000, 4000, 0x3fff),
                12000,
                "14-bit Median: top_left ≤ min ⇒ return max (spec/04 §4.4)"
            );
        }

        /// A triple where the modular and JPEG-LS rules genuinely
        /// diverge at high bit depth, proving the impl uses JPEG-LS.
        /// `left=2, top=3, top_left=16380` at mask `0x3fff`:
        /// - JPEG-LS: `top_left=16380 ≥ max(2,3)=3` ⇒ return `min=2`.
        /// - Modular: `(2 + 3 - 16380) & 0x3fff = (-16375) & 0x3fff =
        ///   9` (`-16375 + 2·16384 = 16393`; `16393 & 0x3fff = 9`),
        ///   then `9 > max=3` ⇒ clip to `3`.
        ///
        /// The impl must return `2` (JPEG-LS), not `3` (modular).
        #[test]
        fn hbd_diverges_from_modular() {
            assert_eq!(
                med_pred_u16(2, 3, 16380, 0x3fff),
                2,
                "14-bit Median must follow JPEG-LS (return 2), not modular (3)"
            );
        }

        /// All three JPEG-LS branches at every HBD mask.
        #[test]
        fn hbd_all_three_branches() {
            for &mask in &[0x3ffu16, 0xfff, 0x3fff] {
                // top_left ≥ max ⇒ return min. left=5,top=7,top_left=10.
                assert_eq!(
                    med_pred_u16(5, 7, 10, mask),
                    5,
                    "JPEG-LS top_left≥max branch (mask {mask:#x}) ⇒ min(left,top)"
                );
                // top_left ≤ min ⇒ return max. left=mask,top=mask-1,tl=0.
                assert_eq!(
                    med_pred_u16(mask, mask - 1, 0, mask),
                    mask,
                    "JPEG-LS top_left≤min branch (mask {mask:#x}) ⇒ max(left,top)"
                );
                // In-range ⇒ return left+top-top_left.
                // left=100,top=120,top_left=110 ⇒ 110 ∈ (100,120) ⇒ 110.
                assert_eq!(
                    med_pred_u16(100, 120, 110, mask),
                    110,
                    "JPEG-LS in-range branch (mask {mask:#x}) ⇒ left+top-top_left"
                );
            }
        }

        /// Column 0 of every subsequent row uses the **top** neighbour
        /// (`prev[0]`) as the predictor for all three predictors
        /// (`spec/04` §4.2–4.4 column-0 fallback) — `spec/04` §8 open
        /// question 1 flags this as the least-observed path because every
        /// vendor fixture's column 0 sits in a zero-residual region. A
        /// non-zero column-0 residual makes the rule observable: with
        /// `res[0,0]=100, res[1,0]=7` the reconstruction is
        /// `px[1,0] = (px[0,0] + 7) & MAX = 107`, identical for Left,
        /// Gradient and Median (none has a left or top-left neighbour at
        /// column 0).
        #[test]
        fn column0_subsequent_row_uses_top_all_predictors() {
            for kind in [
                PredictorKind::Left,
                PredictorKind::Gradient,
                PredictorKind::Median,
            ] {
                // 1-wide, 2-row plane isolates the column-0 path.
                let mut buf = vec![100u8, 7u8];
                apply_u8(kind, &mut buf, 2, 1);
                assert_eq!(buf[0], 100, "{kind:?} row 0 col 0 = res");
                assert_eq!(
                    buf[1], 107,
                    "{kind:?} row 1 col 0 must be (top + res) = 107 (spec/04 §4 col-0 fallback)"
                );
            }
            // Same rule at high bit depth (mask path).
            for kind in [
                PredictorKind::Left,
                PredictorKind::Gradient,
                PredictorKind::Median,
            ] {
                let mut buf = vec![1000u16, 50u16];
                apply_u16(kind, &mut buf, 2, 1, 0x3fff);
                assert_eq!(buf[0], 1000);
                assert_eq!(buf[1], 1050, "{kind:?} HBD col-0 fallback = top + res");
            }
        }
    }
}
