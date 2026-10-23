//! Slice-offset table hardening (FFmpeg CVE-2026-8461 / PixelSmash class).

use crate::error::{Error, Result};

/// Cap on `planes * ceil(height / slice_height)`.
pub const MAX_SLICES: usize = 16_384;

/// Resolve byte range of slice `s`. Offsets are relative to `table_off`.
pub fn slice_byte_range(
    entries: &[u32],
    s: usize,
    total_slices: usize,
    table_off: usize,
    preamble_end: usize,
    packet_len: usize,
) -> Result<(usize, usize)> {
    let start = (entries[s + 1] as usize)
        .checked_add(table_off)
        .ok_or(Error::BadSliceTable { slice_index: s })?;
    let end = if s + 1 < total_slices {
        (entries[s + 2] as usize)
            .checked_add(table_off)
            .ok_or(Error::BadSliceTable { slice_index: s })?
    } else {
        packet_len
    };
    if start < preamble_end || end < start || packet_len < end {
        return Err(Error::BadSliceTable { slice_index: s });
    }
    Ok((start, end))
}

/// Require non-decreasing slice start offsets.
pub fn reject_nonmonotonic_table(entries: &[u32], total_slices: usize) -> Result<()> {
    for s in 0..total_slices.saturating_sub(1) {
        if entries[s + 1] > entries[s + 2] {
            return Err(Error::BadSliceTable { slice_index: s });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_backwards_table() {
        let entries = vec![0, 10, 4];
        assert!(matches!(
            reject_nonmonotonic_table(&entries, 2),
            Err(Error::BadSliceTable { slice_index: 0 })
        ));
    }

    #[test]
    fn accepts_monotonic_table() {
        let entries = vec![0, 10, 20, 30];
        assert!(reject_nonmonotonic_table(&entries, 3).is_ok());
    }

    #[test]
    fn range_rejects_offset_into_header() {
        let entries = vec![0, 0, 8];
        let err = slice_byte_range(&entries, 0, 1, 32, 48, 100).unwrap_err();
        assert!(matches!(err, Error::BadSliceTable { slice_index: 0 }));
    }

    #[test]
    fn range_accepts_payload_after_preamble() {
        let entries = vec![0, 20, 40];
        let (start, end) = slice_byte_range(&entries, 0, 2, 32, 48, 200).unwrap();
        assert_eq!(start, 52);
        assert_eq!(end, 72);
    }
}
