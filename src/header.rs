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
/// `offsets[plane]` is a vector of length `nb_slices + 1`; entry `s` is
/// the absolute byte offset (in the packet) of the start of slice `s`,
/// entry `nb_slices` is the absolute end-of-plane byte offset (the start
/// of the next plane's slice 0, or the packet length for the last
/// plane). The decoder converts the plane-major le32 wire entries into
/// this slightly more convenient form.
#[derive(Clone, Debug)]
pub struct SliceOffsetTable {
    /// `offsets[plane][slice]` = absolute start byte; trailing entry =
    /// absolute end byte of last slice in that plane.
    pub offsets: Vec<Vec<usize>>,
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

        // Convert wire entries to absolute byte offsets per (plane, slice).
        let mut offsets: Vec<Vec<usize>> = Vec::with_capacity(planes);
        for plane in 0..planes {
            let mut v: Vec<usize> = Vec::with_capacity(nb_slices + 1);
            for slice in 0..nb_slices {
                let e = wire[plane * nb_slices + slice] as usize;
                v.push(e + 32);
            }
            // Terminator: the start of plane (plane+1) slice 0, or packet end.
            let term = if plane + 1 < planes {
                wire[(plane + 1) * nb_slices] as usize + 32
            } else {
                packet_len
            };
            v.push(term);
            // Sanity: monotone non-decreasing within the plane.
            for w in v.windows(2) {
                if w[1] < w[0] {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice offsets not monotone in plane {plane}"
                    )));
                }
                if w[1] > packet_len {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice offset {} exceeds packet length {}",
                        w[1], packet_len
                    )));
                }
            }
            offsets.push(v);
        }
        Ok(Self {
            offsets,
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
