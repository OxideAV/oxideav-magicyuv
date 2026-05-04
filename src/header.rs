//! MagicYUV file-header + slice-offset table parser (trace doc §3.1-§3.4).
//!
//! Layout:
//!
//! ```text
//! [0..4]    'MAGY' magic
//! [4..8]    header_size_field (le32, encoder writes 32; decoder uses as
//!            base offset for subsequent slice-offset le32s)
//! [8]       version (= 7 today)
//! [9]       format byte (§3.2)
//! [10]      max_huff_length (informational; decoder caps at 32)
//! [11..13]  reserved
//! [13]      color_matrix
//! [14]      flags (bit 1 = interlaced; bit 2 = full-range)
//! [15]      reserved
//! [16..20]  width  (le32)
//! [20..24]  height (le32)
//! [24..28]  slice_width  (le32, must equal width per FFmpeg decoder)
//! [28..32]  slice_height (le32)
//! [32..36]  leading le32 (skipped by decoder)
//! [36..36+4*planes*nb_slices] slice-offset table, plane-major le32s
//! [next byte] sanity == nb_planes
//! [next planes*nb_slices bytes] permutation (skipped)
//! ```

use oxideav_core::{Error, PixelFormat, Result};

pub const MAGY_MAGIC: [u8; 4] = *b"MAGY";

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FormatCode {
    /// `M8RG` — GBRP, 8-bit, 3 planes, RGB-decorrelated.
    Gbrp = 0x65,
    /// `M8RA` — GBRAP, 8-bit, 4 planes, RGB-decorrelated.
    Gbrap = 0x66,
    /// `M8Y4` — YUV 4:4:4, 8-bit, 3 planes.
    Yuv444P = 0x67,
    /// `M8Y2` — YUV 4:2:2, 8-bit, 3 planes.
    Yuv422P = 0x68,
    /// `M8Y0` — YUV 4:2:0, 8-bit, 3 planes.
    Yuv420P = 0x69,
    /// `M8YA` — YUVA 4:4:4, 8-bit, 4 planes.
    Yuva444P = 0x6a,
    /// `M8G0` — GRAY8.
    Gray8 = 0x6b,
}

impl FormatCode {
    pub fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0x65 => Self::Gbrp,
            0x66 => Self::Gbrap,
            0x67 => Self::Yuv444P,
            0x68 => Self::Yuv422P,
            0x69 => Self::Yuv420P,
            0x6a => Self::Yuva444P,
            0x6b => Self::Gray8,
            // Higher bit-depth codes (0x6c..) are spec'd but not
            // implemented in this version; decode returns Unsupported
            // before reaching the predictor stage.
            _ => {
                return Err(Error::unsupported(format!(
                    "magicyuv: format byte 0x{b:02x} not supported (8-bit only today)"
                )))
            }
        })
    }

    /// Number of planes for this format.
    pub fn planes(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Gbrp | Self::Yuv444P | Self::Yuv422P | Self::Yuv420P => 3,
            Self::Gbrap | Self::Yuva444P => 4,
        }
    }

    /// Bits per sample (always 8 for the formats we decode today).
    pub fn bps(self) -> u8 {
        8
    }

    /// Whether the encoder applied the implicit GBR↔RGB decorrelation
    /// `B' = B - G; R' = R - G; G' = G` before prediction.
    pub fn rgb_decorrelated(self) -> bool {
        matches!(self, Self::Gbrp | Self::Gbrap)
    }

    /// Horizontal chroma subsampling shift (0 for full, 1 for half).
    pub fn h_subsample(self) -> u32 {
        match self {
            Self::Yuv422P | Self::Yuv420P => 1,
            _ => 0,
        }
    }

    /// Vertical chroma subsampling shift.
    pub fn v_subsample(self) -> u32 {
        match self {
            Self::Yuv420P => 1,
            _ => 0,
        }
    }

    /// The PixelFormat the decoder emits for this format. We decode the
    /// GBRP / GBRAP wire formats to packed RGB24 (after inverse
    /// decorrelation) since `oxideav-core::PixelFormat` doesn't (yet)
    /// have a `Gbrp` planar variant.
    pub fn output_pixel_format(self) -> PixelFormat {
        match self {
            Self::Gbrp => PixelFormat::Rgb24,
            Self::Gbrap => PixelFormat::Rgba,
            Self::Yuv444P => PixelFormat::Yuv444P,
            Self::Yuv422P => PixelFormat::Yuv422P,
            Self::Yuv420P => PixelFormat::Yuv420P,
            // No native Yuva444P in core — fall back to Yuv444P; alpha
            // becomes a 4th plane by convention.
            Self::Yuva444P => PixelFormat::Yuv444P,
            Self::Gray8 => PixelFormat::Gray8,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileHeader {
    pub format: FormatCode,
    pub version: u8,
    pub max_huff_length: u8,
    pub color_matrix: u8,
    pub flags: u8,
    pub width: u32,
    pub height: u32,
    pub slice_width: u32,
    pub slice_height: u32,
}

impl FileHeader {
    /// Parse the 32-byte fixed file header. Returns the parsed header.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 32 {
            return Err(Error::invalid(format!(
                "magicyuv: packet too short for header ({} < 32)",
                bytes.len()
            )));
        }
        if bytes[0..4] != MAGY_MAGIC {
            return Err(Error::invalid(
                "magicyuv: missing 'MAGY' magic at start of packet",
            ));
        }
        let _header_size_field = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let version = bytes[8];
        if version != 7 {
            return Err(Error::unsupported(format!(
                "magicyuv: unsupported version {version} (only v7 implemented)"
            )));
        }
        let format = FormatCode::from_byte(bytes[9])?;
        let max_huff_length = bytes[10];
        if max_huff_length > 32 {
            return Err(Error::invalid(format!(
                "magicyuv: max_huff_length {max_huff_length} > 32 cap"
            )));
        }
        let color_matrix = bytes[13];
        let flags = bytes[14];
        if flags & 0x02 != 0 {
            return Err(Error::unsupported(
                "magicyuv: interlaced streams not yet implemented",
            ));
        }
        let width = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let slice_width = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let slice_height = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
        if width == 0 || height == 0 {
            return Err(Error::invalid("magicyuv: zero-sized frame"));
        }
        if slice_width != width {
            return Err(Error::unsupported(format!(
                "magicyuv: slice_width ({slice_width}) != width ({width}) — \
                 horizontal tiling not implemented (matches FFmpeg's PATCHWELCOME)"
            )));
        }
        if slice_height == 0 {
            return Err(Error::invalid("magicyuv: zero slice_height"));
        }
        Ok(Self {
            format,
            version,
            max_huff_length,
            color_matrix,
            flags,
            width,
            height,
            slice_width,
            slice_height,
        })
    }

    /// Number of slices, derived per §3.3.
    pub fn nb_slices(&self) -> usize {
        (self.height as usize).div_ceil(self.slice_height as usize)
    }

    /// Pixel-row range covered by slice `s` (start, end_exclusive).
    pub fn slice_row_range(&self, s: usize) -> (usize, usize) {
        let start = s * self.slice_height as usize;
        let end = ((s + 1) * self.slice_height as usize).min(self.height as usize);
        (start, end)
    }

    /// Whether this stream sets the full-color-range flag (bit 2).
    pub fn full_range(&self) -> bool {
        self.flags & 0x04 != 0
    }
}

/// Slice-offset table parsed from bytes `[32..]` of a packet.
///
/// The wire stores `planes * nb_slices` plane-major le32 starts (each
/// relative to byte 32). The slice payloads themselves are written
/// **slice-major** in the file: (plane 0 slice 0), (plane 1 slice 0),
/// ..., (plane P-1 slice 0), (plane 0 slice 1), ... — so the
/// per-plane starts are not monotone in the packet, and a slice's end
/// is the **file-order successor's** start, not the same plane's next
/// slice's start. We expose `starts` + `ends` separately to keep this
/// distinction explicit.
#[derive(Clone, Debug)]
pub struct SliceOffsetTable {
    /// `starts[plane][slice]` = absolute byte offset of the slice's
    /// first byte (length `nb_slices`).
    pub starts: Vec<Vec<usize>>,
    /// `ends[plane][slice]` = absolute byte offset one past the slice's
    /// last byte (= start of whatever slice comes next in file order,
    /// or `packet_len` for the file's last slice).
    pub ends: Vec<Vec<usize>>,
    /// Absolute byte offset where the per-plane Huffman descriptors begin.
    pub huffman_start: usize,
}

impl SliceOffsetTable {
    /// Parse the offset table starting at byte 32 (right after the file
    /// header). `packet_len` bounds end-of-plane fill-in for the last
    /// slice.
    pub fn parse(bytes: &[u8], header: &FileHeader, packet_len: usize) -> Result<Self> {
        let planes = header.format.planes();
        let nb_slices = header.nb_slices();
        let mut p = 32usize;
        if bytes.len() < p + 4 {
            return Err(Error::invalid(
                "magicyuv: packet too short for offset-leading",
            ));
        }
        // §3.4: skip the leading le32 immediately after the header.
        p += 4;
        let needed = 4 * planes * nb_slices;
        if bytes.len() < p + needed {
            return Err(Error::invalid(format!(
                "magicyuv: packet too short for slice-offset table ({} bytes need at least {})",
                bytes.len() - p,
                needed
            )));
        }
        let mut wire: Vec<u32> = Vec::with_capacity(planes * nb_slices);
        for _ in 0..(planes * nb_slices) {
            let v = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap());
            wire.push(v);
            p += 4;
        }
        // Sanity byte == nb_planes.
        if bytes.len() < p + 1 {
            return Err(Error::invalid(
                "magicyuv: packet too short for sanity-byte after offsets",
            ));
        }
        let sanity = bytes[p];
        if sanity as usize != planes {
            return Err(Error::invalid(format!(
                "magicyuv: sanity byte ({sanity}) != nb_planes ({planes})"
            )));
        }
        p += 1;
        // Skip the permutation array.
        let perm_len = planes * nb_slices;
        if bytes.len() < p + perm_len {
            return Err(Error::invalid(
                "magicyuv: packet too short for permutation array",
            ));
        }
        p += perm_len;
        let huffman_start = p;

        // Compute per-(plane, slice) absolute start; then derive each
        // slice's end as the start of its **file-order** successor
        // (NOT the same plane's next slice — see the struct docstring
        // for why).
        let abs_start = |plane: usize, slice: usize| -> usize {
            wire[plane * nb_slices + slice] as usize + 32
        };
        let abs_end = |plane: usize, slice: usize| -> usize {
            // Slice-major file order: after (p, s) comes (p+1, s),
            // wrapping to (0, s+1) at the last plane.
            if plane + 1 < planes {
                abs_start(plane + 1, slice)
            } else if slice + 1 < nb_slices {
                abs_start(0, slice + 1)
            } else {
                packet_len
            }
        };
        let mut starts: Vec<Vec<usize>> = Vec::with_capacity(planes);
        let mut ends: Vec<Vec<usize>> = Vec::with_capacity(planes);
        for plane in 0..planes {
            let mut s_v: Vec<usize> = Vec::with_capacity(nb_slices);
            let mut e_v: Vec<usize> = Vec::with_capacity(nb_slices);
            for slice in 0..nb_slices {
                let st = abs_start(plane, slice);
                let en = abs_end(plane, slice);
                if st > packet_len || en > packet_len {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice {slice} plane {plane} byte range \
                         [{st}..{en}) exceeds packet length {packet_len}"
                    )));
                }
                if en < st {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice {slice} plane {plane} end {en} \
                         precedes start {st}"
                    )));
                }
                s_v.push(st);
                e_v.push(en);
            }
            starts.push(s_v);
            ends.push(e_v);
        }
        Ok(Self {
            starts,
            ends,
            huffman_start,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_code_planes_and_subsampling() {
        assert_eq!(FormatCode::Gbrp.planes(), 3);
        assert!(FormatCode::Gbrp.rgb_decorrelated());
        assert_eq!(FormatCode::Yuv422P.planes(), 3);
        assert_eq!(FormatCode::Yuv422P.h_subsample(), 1);
        assert_eq!(FormatCode::Yuv420P.v_subsample(), 1);
        assert_eq!(FormatCode::Gray8.planes(), 1);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(b"XXXX");
        assert!(FileHeader::parse(&bytes).is_err());
    }
}
