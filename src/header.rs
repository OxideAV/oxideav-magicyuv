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
//! [24..28]  slice_width  (le32; may equal width or a smaller tile width)
//! [28..32]  slice_height (le32)
//! [32..36]  leading le32 (skipped by decoder)
//! [36..36+4*planes*nb_slices_total] slice-offset table, plane-major le32s
//! [next byte] sanity == nb_planes
//! [next planes*nb_slices_total bytes] permutation (skipped)
//! ```
//!
//! `nb_slices_total = nb_slices_x * nb_slices_y` (the grid product). The
//! 8-bit-only FFmpeg encoder always emits `nb_slices_x = 1`; the
//! horizontal partition is allowed by the bitstream and is implemented
//! both on encode and decode.

use oxideav_core::{Error, PixelFormat, Result};

pub const MAGY_MAGIC: [u8; 4] = *b"MAGY";

/// Family of the format byte's pixel layout.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FormatFamily {
    /// GBRP (3 planes, RGB-decorrelated).
    Gbrp,
    /// GBRAP (4 planes, RGB-decorrelated).
    Gbrap,
    /// YUV 4:4:4 (3 planes, no subsampling).
    Yuv444P,
    /// YUV 4:2:2 (3 planes, h_sub = 1).
    Yuv422P,
    /// YUV 4:2:0 (3 planes, h_sub = 1, v_sub = 1).
    Yuv420P,
    /// YUVA 4:4:4 (4 planes, no subsampling).
    Yuva444P,
    /// Single-plane grayscale.
    Gray,
}

impl FormatFamily {
    pub fn planes(self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Gbrp | Self::Yuv444P | Self::Yuv422P | Self::Yuv420P => 3,
            Self::Gbrap | Self::Yuva444P => 4,
        }
    }

    pub fn rgb_decorrelated(self) -> bool {
        matches!(self, Self::Gbrp | Self::Gbrap)
    }

    pub fn h_subsample(self) -> u32 {
        match self {
            Self::Yuv422P | Self::Yuv420P => 1,
            _ => 0,
        }
    }

    pub fn v_subsample(self) -> u32 {
        match self {
            Self::Yuv420P => 1,
            _ => 0,
        }
    }
}

/// One specific (family × bit-depth) pair, identified by the file-header
/// format byte. The decoder's main dispatch uses this; the encoder picks
/// one when constructing a packet.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FormatCode {
    /// `M8RG` 0x65 — GBRP, 8-bit.
    Gbrp,
    /// `M8RA` 0x66 — GBRAP, 8-bit.
    Gbrap,
    /// `M8Y4` 0x67 — YUV 4:4:4, 8-bit.
    Yuv444P,
    /// `M8Y2` 0x68 — YUV 4:2:2, 8-bit.
    Yuv422P,
    /// `M8Y0` 0x69 — YUV 4:2:0, 8-bit.
    Yuv420P,
    /// `M8YA` 0x6a — YUVA 4:4:4, 8-bit.
    Yuva444P,
    /// `M8G0` 0x6b — GRAY8.
    Gray8,
    /// 0x6c — YUV 4:2:2, 10-bit.
    Yuv422P10,
    /// 0x73 — GRAY10.
    Gray10,
    /// 0x76 — YUV 4:4:4, 10-bit.
    Yuv444P10,
    /// 0x7b — YUV 4:2:0, 10-bit.
    Yuv420P10,
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
            0x6c => Self::Yuv422P10,
            0x73 => Self::Gray10,
            0x76 => Self::Yuv444P10,
            0x7b => Self::Yuv420P10,
            // Codes 0x6d/0x6e (GBRP10/GBRAP10), 0x6f/0x70 (GBRP12/
            // GBRAP12), 0x71/0x72 (GBRP14/GBRAP14) are recognised by
            // the FFmpeg decoder but the workspace `oxideav-core`
            // PixelFormat enum has no GBRP10/12/14 / GBRAP10/12/14
            // variants today — surface them as `Unsupported` until
            // core grows the variants. 0x74/0x75/0x77/0x78/0x79/0x7a
            // are reserved gaps.
            _ => {
                return Err(Error::unsupported(format!(
                    "magicyuv: format byte 0x{b:02x} not supported by this decoder"
                )))
            }
        })
    }

    pub fn as_byte(self) -> u8 {
        match self {
            Self::Gbrp => 0x65,
            Self::Gbrap => 0x66,
            Self::Yuv444P => 0x67,
            Self::Yuv422P => 0x68,
            Self::Yuv420P => 0x69,
            Self::Yuva444P => 0x6a,
            Self::Gray8 => 0x6b,
            Self::Yuv422P10 => 0x6c,
            Self::Gray10 => 0x73,
            Self::Yuv444P10 => 0x76,
            Self::Yuv420P10 => 0x7b,
        }
    }

    pub fn family(self) -> FormatFamily {
        match self {
            Self::Gbrp => FormatFamily::Gbrp,
            Self::Gbrap => FormatFamily::Gbrap,
            Self::Yuv444P | Self::Yuv444P10 => FormatFamily::Yuv444P,
            Self::Yuv422P | Self::Yuv422P10 => FormatFamily::Yuv422P,
            Self::Yuv420P | Self::Yuv420P10 => FormatFamily::Yuv420P,
            Self::Yuva444P => FormatFamily::Yuva444P,
            Self::Gray8 | Self::Gray10 => FormatFamily::Gray,
        }
    }

    pub fn planes(self) -> usize {
        self.family().planes()
    }

    /// Bits per sample (8 / 10 / 12 / 14).
    pub fn bps(self) -> u8 {
        match self {
            Self::Gbrp
            | Self::Gbrap
            | Self::Yuv444P
            | Self::Yuv422P
            | Self::Yuv420P
            | Self::Yuva444P
            | Self::Gray8 => 8,
            Self::Yuv422P10 | Self::Gray10 | Self::Yuv444P10 | Self::Yuv420P10 => 10,
        }
    }

    /// True when each sample occupies 2 bytes on disk (raw mode) and the
    /// in-memory predictor uses `u16` arithmetic.
    pub fn is_high_bit_depth(self) -> bool {
        self.bps() > 8
    }

    pub fn rgb_decorrelated(self) -> bool {
        self.family().rgb_decorrelated()
    }

    pub fn h_subsample(self) -> u32 {
        self.family().h_subsample()
    }

    pub fn v_subsample(self) -> u32 {
        self.family().v_subsample()
    }

    /// The PixelFormat the decoder emits for this format. We decode the
    /// GBRP / GBRAP wire formats to packed RGB24 / RGBA (after inverse
    /// decorrelation) since `oxideav-core::PixelFormat` doesn't expose a
    /// `Gbrp` planar variant.
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
            Self::Yuv422P10 => PixelFormat::Yuv422P10Le,
            Self::Yuv444P10 => PixelFormat::Yuv444P10Le,
            Self::Yuv420P10 => PixelFormat::Yuv420P10Le,
            Self::Gray10 => PixelFormat::Gray10Le,
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
        if slice_width == 0 || slice_height == 0 {
            return Err(Error::invalid("magicyuv: zero slice_width or slice_height"));
        }
        if slice_width > width {
            return Err(Error::invalid(format!(
                "magicyuv: slice_width ({slice_width}) > width ({width})"
            )));
        }
        // Horizontal alignment to the chroma grid: every slice column
        // must cover an integer number of chroma samples. (FFmpeg's
        // decoder enforces this implicitly because it derives chroma
        // tile widths from a left-shift.)
        let h_sub = format.h_subsample();
        if h_sub > 0 && (slice_width % (1u32 << h_sub) != 0) {
            return Err(Error::invalid(format!(
                "magicyuv: slice_width {slice_width} not aligned to chroma h-subsampling {h_sub}",
            )));
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

    /// Number of vertical slice rows (= what the original FFmpeg-only
    /// implementation called `nb_slices`).
    pub fn nb_slices_y(&self) -> usize {
        (self.height as usize).div_ceil(self.slice_height as usize)
    }

    /// Number of horizontal slice columns.
    pub fn nb_slices_x(&self) -> usize {
        (self.width as usize).div_ceil(self.slice_width as usize)
    }

    /// Total number of slices on the wire (`nb_slices_x * nb_slices_y`).
    pub fn nb_slices(&self) -> usize {
        self.nb_slices_x() * self.nb_slices_y()
    }

    /// Pixel-row range covered by row-band `s_y` (start, end_exclusive).
    pub fn slice_row_range(&self, s_y: usize) -> (usize, usize) {
        let start = s_y * self.slice_height as usize;
        let end = ((s_y + 1) * self.slice_height as usize).min(self.height as usize);
        (start, end)
    }

    /// Pixel-column range covered by column-band `s_x`
    /// (start, end_exclusive).
    pub fn slice_col_range(&self, s_x: usize) -> (usize, usize) {
        let start = s_x * self.slice_width as usize;
        let end = ((s_x + 1) * self.slice_width as usize).min(self.width as usize);
        (start, end)
    }

    /// Map a (row-band, col-band) pair to the linear slice index that
    /// indexes into [`SliceOffsetTable::starts`]. Slice major-order is
    /// row-major across columns: `index = s_y * nb_slices_x + s_x`.
    pub fn slice_index(&self, s_x: usize, s_y: usize) -> usize {
        s_y * self.nb_slices_x() + s_x
    }

    /// Whether this stream sets the full-color-range flag (bit 2).
    pub fn full_range(&self) -> bool {
        self.flags & 0x04 != 0
    }
}

/// Slice-offset table parsed from bytes `[32..]` of a packet.
///
/// The wire stores `planes * nb_slices_total` plane-major le32 starts
/// (each relative to byte 32). The slice payloads themselves are
/// written **slice-major** in the file: (plane 0 slice 0), (plane 1
/// slice 0), ..., (plane P-1 slice 0), (plane 0 slice 1), ... — so
/// the per-plane starts are not monotone in the packet, and a slice's
/// end is the **file-order successor's** start, not the same plane's
/// next slice's start. We expose `starts` + `ends` separately to keep
/// this distinction explicit.
#[derive(Clone, Debug)]
pub struct SliceOffsetTable {
    /// `starts[plane][slice]` = absolute byte offset of the slice's
    /// first byte (length `nb_slices_total`).
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
        let abs_start =
            |plane: usize, slice: usize| -> usize { wire[plane * nb_slices + slice] as usize + 32 };
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
        assert_eq!(FormatCode::Yuv422P10.bps(), 10);
        assert!(FormatCode::Gray10.is_high_bit_depth());
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(b"XXXX");
        assert!(FileHeader::parse(&bytes).is_err());
    }

    /// Build a synthetic 32-byte file header for a YUV422P frame with
    /// the given dimensions and slice height, then append a
    /// plane-major slice-offset table populated from the supplied
    /// `wire_starts[plane][slice]` (relative to byte 32). Returns the
    /// concatenated bytes plus the absolute offset where slice
    /// payloads should begin (== huffman_start in our parser, which
    /// the synthesised "Huffman descriptor" then occupies; tests can
    /// pad past `huffman_start` to whatever `packet_len` they want).
    fn build_synthetic_packet(
        format: FormatCode,
        width: u32,
        height: u32,
        slice_width: u32,
        slice_height: u32,
        wire_starts: &[Vec<u32>],
        packet_len: usize,
    ) -> Vec<u8> {
        let planes = format.planes();
        let nb_slices_x = (width as usize).div_ceil(slice_width as usize);
        let nb_slices_y = (height as usize).div_ceil(slice_height as usize);
        let nb_slices = nb_slices_x * nb_slices_y;
        assert_eq!(wire_starts.len(), planes);
        for v in wire_starts {
            assert_eq!(v.len(), nb_slices);
        }
        let mut out = vec![0u8; packet_len];
        out[0..4].copy_from_slice(b"MAGY");
        out[4..8].copy_from_slice(&32u32.to_le_bytes());
        out[8] = 7;
        out[9] = format.as_byte();
        out[10] = 12;
        out[14] = 0;
        out[16..20].copy_from_slice(&width.to_le_bytes());
        out[20..24].copy_from_slice(&height.to_le_bytes());
        out[24..28].copy_from_slice(&slice_width.to_le_bytes());
        out[28..32].copy_from_slice(&slice_height.to_le_bytes());
        out[32..36].copy_from_slice(&0u32.to_le_bytes());
        let mut p = 36;
        for plane in 0..planes {
            for slice in 0..nb_slices {
                out[p..p + 4].copy_from_slice(&wire_starts[plane][slice].to_le_bytes());
                p += 4;
            }
        }
        out[p] = planes as u8;
        p += 1;
        p += planes * nb_slices;
        let _ = p;
        out
    }

    #[test]
    fn slice_offset_table_single_slice_yuv422p() {
        let wire = vec![vec![800u32], vec![1000u32], vec![1100u32]];
        let pkt = build_synthetic_packet(FormatCode::Yuv422P, 64, 48, 64, 48, &wire, 1234);
        let hdr = FileHeader::parse(&pkt).unwrap();
        let table = SliceOffsetTable::parse(&pkt, &hdr, 1234).unwrap();
        assert_eq!(table.starts[0], vec![832]);
        assert_eq!(table.starts[1], vec![1032]);
        assert_eq!(table.starts[2], vec![1132]);
        assert_eq!(table.ends[0], vec![1032]);
        assert_eq!(table.ends[1], vec![1132]);
        assert_eq!(table.ends[2], vec![1234]);
    }

    #[test]
    fn slice_offset_table_multi_slice_interleaved() {
        let wire = vec![
            vec![833u32, 1105, 1329, 1553],
            vec![957u32, 1217, 1441, 1733],
            vec![1029u32, 1273, 1497, 1833],
        ];
        let pkt = build_synthetic_packet(FormatCode::Yuv422P, 64, 48, 64, 12, &wire, 1970);
        let hdr = FileHeader::parse(&pkt).unwrap();
        let table = SliceOffsetTable::parse(&pkt, &hdr, 1970).unwrap();

        assert_eq!(table.starts[0], vec![865, 1137, 1361, 1585]);
        assert_eq!(table.starts[1], vec![989, 1249, 1473, 1765]);
        assert_eq!(table.starts[2], vec![1061, 1305, 1529, 1865]);

        assert_eq!(table.ends[0], vec![989, 1249, 1473, 1765]);
        assert_eq!(table.ends[1], vec![1061, 1305, 1529, 1865]);
        assert_eq!(table.ends[2], vec![1137, 1361, 1585, 1970]);

        for plane in 0..3 {
            for slice in 0..4 {
                let st = table.starts[plane][slice];
                let en = table.ends[plane][slice];
                assert!(en >= st + 2);
            }
        }
    }

    #[test]
    fn slice_offset_table_rejects_oob_offset() {
        let wire = vec![vec![5000u32], vec![1000u32], vec![1100u32]];
        let pkt = build_synthetic_packet(FormatCode::Yuv422P, 64, 48, 64, 48, &wire, 1234);
        let hdr = FileHeader::parse(&pkt).unwrap();
        let err = SliceOffsetTable::parse(&pkt, &hdr, 1234).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("exceeds packet length"));
    }

    #[test]
    fn slice_offset_table_rejects_descending_offset() {
        let wire = vec![vec![800u32, 100], vec![1000u32, 50], vec![1100u32, 200]];
        let pkt = build_synthetic_packet(FormatCode::Yuv422P, 64, 48, 64, 24, &wire, 1234);
        let hdr = FileHeader::parse(&pkt).unwrap();
        let err = SliceOffsetTable::parse(&pkt, &hdr, 1234).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("precedes start") || msg.contains("not monotone"));
    }

    /// 64×48 YUV422P split into 2 horizontal columns × 1 row =
    /// `nb_slices_x = 2`, `nb_slices_y = 1`. Wire stores 2 slices per
    /// plane. File-order is (p0,s0)(p1,s0)(p2,s0)(p0,s1)(p1,s1)(p2,s1)
    /// so each successor's start ≥ previous start.
    #[test]
    fn slice_offset_table_horizontal_tiles() {
        let wire = vec![vec![400u32, 1500], vec![700u32, 1700], vec![1000u32, 2000]];
        let pkt = build_synthetic_packet(FormatCode::Yuv422P, 64, 48, 32, 48, &wire, 2400);
        let hdr = FileHeader::parse(&pkt).unwrap();
        assert_eq!(hdr.nb_slices_x(), 2);
        assert_eq!(hdr.nb_slices_y(), 1);
        let t = SliceOffsetTable::parse(&pkt, &hdr, 2400).unwrap();
        assert_eq!(t.starts[0], vec![432, 1532]);
        assert_eq!(t.starts[1], vec![732, 1732]);
        assert_eq!(t.starts[2], vec![1032, 2032]);
        // Ends in file order: (p0,s0)→p1,s0; (p1,s0)→p2,s0; (p2,s0)→p0,s1;
        // (p0,s1)→p1,s1; (p1,s1)→p2,s1; (p2,s1)→packet_len.
        assert_eq!(t.ends[0], vec![732, 1732]);
        assert_eq!(t.ends[1], vec![1032, 2032]);
        assert_eq!(t.ends[2], vec![1532, 2400]);
    }
}
