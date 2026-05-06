//! In-place predictor reconstruction per `spec/04` §4.
//!
//! Each predictor takes a slice's residual buffer (already filled by
//! Huffman / raw decode) and rewrites it in-place to reconstructed
//! pixel values. The buffer layout is plane row-major: `data[r * w +
//! c]` is the residual at `(r, c)` on entry, the reconstructed pixel
//! on exit.
//!
//! Round-1 implementation only handles 8-bit samples (`u8`). The
//! 10/12/14-bit variants are spec-feasible (the formulas just take a
//! `MAX = (1 << bits) - 1` mask per `spec/04` §4.5) but are deferred
//! since the round-1 supported set is 8-bit native. The 8-bit
//! Median formula is the **modular** variant per `spec/04` §4.4
//! Round-1 validation-corrected note (the standard JPEG-LS Median is
//! used at 10/12/14-bit, but that's deferred here).
//!
//! Slice independence: each call to `apply_*` is given the slice's
//! own residual block. There is no carry-over from prior slices —
//! `spec/04` §5.2 establishes that each slice starts fresh, so
//! predictor state initialises at row 0 column 0 of `data` and
//! propagates within the slice.

use crate::tables::PredictorKind;

/// Apply the slice's predictor `kind` in-place to a row-major 8-bit
/// residual buffer of size `(rows × width)`. After the call the
/// buffer holds reconstructed pixel values modulo 256.
pub fn apply_u8(kind: PredictorKind, data: &mut [u8], rows: usize, width: usize) {
    debug_assert_eq!(data.len(), rows * width);
    if rows == 0 || width == 0 {
        return;
    }
    match kind {
        PredictorKind::Left => apply_left_u8(data, rows, width),
        PredictorKind::Gradient => apply_gradient_u8(data, rows, width),
        PredictorKind::Median => apply_median_u8(data, rows, width),
    }
}

fn apply_left_u8(data: &mut [u8], rows: usize, width: usize) {
    // Row 0: running sum across the row (column 0 = residual itself).
    {
        let row = &mut data[0..width];
        for c in 1..width {
            row[c] = row[c].wrapping_add(row[c - 1]);
        }
    }
    // Rows ≥ 1: column 0 falls back to top (previous row's column 0).
    for r in 1..rows {
        let prev0 = data[(r - 1) * width];
        let row_off = r * width;
        data[row_off] = data[row_off].wrapping_add(prev0);
        for c in 1..width {
            data[row_off + c] = data[row_off + c].wrapping_add(data[row_off + c - 1]);
        }
    }
}

fn apply_gradient_u8(data: &mut [u8], rows: usize, width: usize) {
    // Row 0 falls back to Left predictor across the row (top
    // neighbours don't exist).
    {
        let row = &mut data[0..width];
        for c in 1..width {
            row[c] = row[c].wrapping_add(row[c - 1]);
        }
    }
    for r in 1..rows {
        let prev_off = (r - 1) * width;
        let row_off = r * width;
        // Column 0: pred = top → result = top + residual.
        data[row_off] = data[row_off].wrapping_add(data[prev_off]);
        for c in 1..width {
            // pred = left + top - top_left, all mod 256.
            let left = data[row_off + c - 1];
            let top = data[prev_off + c];
            let top_left = data[prev_off + c - 1];
            let pred = left.wrapping_add(top).wrapping_sub(top_left);
            data[row_off + c] = data[row_off + c].wrapping_add(pred);
        }
    }
}

fn apply_median_u8(data: &mut [u8], rows: usize, width: usize) {
    // Row 0 falls back to Left.
    {
        let row = &mut data[0..width];
        for c in 1..width {
            row[c] = row[c].wrapping_add(row[c - 1]);
        }
    }
    for r in 1..rows {
        let prev_off = (r - 1) * width;
        let row_off = r * width;
        // Column 0: pred = top.
        data[row_off] = data[row_off].wrapping_add(data[prev_off]);
        for c in 1..width {
            let left = data[row_off + c - 1];
            let top = data[prev_off + c];
            let top_left = data[prev_off + c - 1];
            // Modular Median (spec/04 §4.4 Round-1 validation-
            // corrected note for 8-bit):
            //   gradient = (left + top - top_left) & 0xff
            //   if gradient < min(left, top): pred = min
            //   else if gradient > max(left, top): pred = max
            //   else: pred = gradient
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
            data[row_off + c] = data[row_off + c].wrapping_add(pred);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoder-side counterpart of `apply_left_u8`. Used by the
    /// roundtrip tests below.
    pub(crate) fn encode_left_u8(data: &mut [u8], rows: usize, width: usize) {
        if rows == 0 || width == 0 {
            return;
        }
        // Bottom-up & right-to-left within each row, so we don't
        // overwrite needed neighbours.
        for r in (0..rows).rev() {
            let row_off = r * width;
            for c in (1..width).rev() {
                data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
            }
            if r >= 1 {
                let prev0 = data[(r - 1) * width];
                data[row_off] = data[row_off].wrapping_sub(prev0);
            }
        }
    }

    pub(crate) fn encode_gradient_u8(data: &mut [u8], rows: usize, width: usize) {
        if rows == 0 || width == 0 {
            return;
        }
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r == 0 {
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
            } else {
                let prev_off = (r - 1) * width;
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
        if rows == 0 || width == 0 {
            return;
        }
        for r in (0..rows).rev() {
            let row_off = r * width;
            if r == 0 {
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
            } else {
                let prev_off = (r - 1) * width;
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
}
