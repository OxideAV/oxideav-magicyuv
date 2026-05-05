//! MagicYUV `Packet`-to-`Frame` decoder.
//!
//! Pipeline (per trace doc §8 minimal sketch):
//!
//! 1. Parse the 32-byte file header (`header::FileHeader::parse`).
//! 2. Parse the slice-offset table (`header::SliceOffsetTable::parse`),
//!    deriving plane-major absolute byte offsets and the start of the
//!    Huffman block.
//! 3. Parse one Huffman length descriptor per plane and build a
//!    canonical-Huffman decoder.
//! 4. For each slice, for each plane: read the (flag, predictor) prefix
//!    pair, decode `slice_h * slice_plane_w` residuals (Huffman if flag
//!    bit 0 clear, raw bytes/uint16 if set), apply the per-row predictor
//!    on the slice rectangle.
//! 5. If the format is RGB-decorrelated (`M8RG` / `M8RA`), invert the
//!    `B' = B - G; R' = R - G` transform pixel-by-pixel and pack the
//!    result into the chosen output `PixelFormat` (RGB24 / RGBA).
//!
//! Slices form a `nb_slices_x × nb_slices_y` rectangular grid. The
//! 8-bit FFmpeg-only encoder always emits `nb_slices_x = 1` (full-width
//! row bands), but the bitstream permits `nb_slices_x ≥ 1` and the
//! decoder handles arbitrary tilings.
//!
//! For 10/12/14-bit content (`bps > 8`) the predictor and Huffman
//! buffers widen to `u16`; the wire still carries the per-symbol
//! Huffman index (alphabet `1 << bps`), and the predictor's `mask` is
//! `(1 << bps) - 1`. Raw mode reads `bps`-bit literals as **little-
//! endian 2-byte words** (the only on-wire form the trace doc and the
//! upstream decoder agree on).

use oxideav_core::frame::VideoPlane;
use oxideav_core::Decoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

use crate::bitstream::BitReader;
use crate::header::{FileHeader, FormatFamily, SliceOffsetTable};
use crate::huffman::{CanonicalHuffman, LengthTable};
use crate::predictor::{
    apply_gradient_u16, apply_gradient_u8, apply_left_u16, apply_left_u8, apply_median_u16,
    apply_median_u8, Predictor,
};

/// Cap on a single-frame plane allocation, matched to the workspace
/// convention (32k × 32k) — adversarial headers with billion-pixel
/// dimensions must not OOM the decoder.
const MAX_DECODED_PIXELS: usize = 32_768 * 32_768;

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(MagicYuvDecoder {
        codec_id: params.codec_id.clone(),
        pending: None,
        eof: false,
    }))
}

struct MagicYuvDecoder {
    codec_id: CodecId,
    pending: Option<Packet>,
    eof: bool,
}

impl Decoder for MagicYuvDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other(
                "magicyuv decoder: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        };
        let vf = decode_packet(&pkt.data, pkt.pts)?;
        Ok(Frame::Video(vf))
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
}

/// Decode one MagicYUV packet (the AVI `movi` frame body, magic `MAGY...`)
/// into a single `VideoFrame`.
pub fn decode_packet(bytes: &[u8], pts: Option<i64>) -> Result<VideoFrame> {
    let header = FileHeader::parse(bytes)?;
    let pixels = (header.width as usize).saturating_mul(header.height as usize);
    if pixels > MAX_DECODED_PIXELS {
        return Err(Error::invalid(format!(
            "magicyuv: declared dimensions {}x{} exceed sanity cap",
            header.width, header.height
        )));
    }
    let table = SliceOffsetTable::parse(bytes, &header, bytes.len())?;
    let nb_planes = header.format.planes();

    // Plane dimensions (pre-decorrelation, pre-output packing).
    let plane_widths = plane_widths(&header);
    let plane_heights = plane_heights(&header);

    let alphabet = 1usize << header.format.bps();

    if header.format.is_high_bit_depth() {
        decode_packet_u16(
            bytes,
            pts,
            header,
            table,
            plane_widths,
            plane_heights,
            alphabet,
        )
    } else {
        decode_packet_u8(
            bytes,
            pts,
            header,
            table,
            nb_planes,
            plane_widths,
            plane_heights,
            alphabet,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_packet_u8(
    bytes: &[u8],
    pts: Option<i64>,
    header: FileHeader,
    table: SliceOffsetTable,
    nb_planes: usize,
    plane_widths: Vec<usize>,
    plane_heights: Vec<usize>,
    alphabet: usize,
) -> Result<VideoFrame> {
    // Allocate per-plane working buffers.
    let mut planes: Vec<Vec<u8>> = (0..nb_planes)
        .map(|p| vec![0u8; plane_widths[p] * plane_heights[p]])
        .collect();

    // Decode the per-plane Huffman length descriptors.
    let mut huffmans: Vec<CanonicalHuffman> = Vec::with_capacity(nb_planes);
    {
        let mut p = table.huffman_start;
        for _ in 0..nb_planes {
            let (lt, np) = LengthTable::parse(bytes, p, alphabet)?;
            p = np;
            huffmans.push(CanonicalHuffman::build(&lt.lengths)?);
        }
    }

    let nb_x = header.nb_slices_x();
    let nb_y = header.nb_slices_y();

    for sy in 0..nb_y {
        for sx in 0..nb_x {
            let slice_idx = header.slice_index(sx, sy);
            let (row_start, row_end) = header.slice_row_range(sy);
            let (col_start, col_end) = header.slice_col_range(sx);
            for plane in 0..nb_planes {
                let pw = plane_widths[plane];
                let v_sub = chroma_v_sub(&header, plane);
                let h_sub = chroma_h_sub(&header, plane);
                let plane_row_start = row_start >> v_sub;
                let plane_row_end = subsample_end(row_end, header.height as usize, v_sub);
                let plane_h = plane_row_end - plane_row_start;
                let plane_col_start = col_start >> h_sub;
                let plane_col_end = subsample_end(col_end, header.width as usize, h_sub);
                let plane_w = plane_col_end - plane_col_start;

                let pstart = table.starts[plane][slice_idx];
                let pend = table.ends[plane][slice_idx];
                if pstart + 2 > pend || pend > bytes.len() {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice {slice_idx} plane {plane} bad range \
                         [{pstart}..{pend}) of {})",
                        bytes.len(),
                    )));
                }
                let flag = bytes[pstart];
                let pred = Predictor::from_byte(bytes[pstart + 1])?;
                let payload = &bytes[pstart + 2..pend];

                let needed = plane_h * plane_w;
                let mut tile = vec![0u8; needed];

                if flag & 1 != 0 {
                    // Raw mode: literal bytes (8-bit only here).
                    let n = tile.len();
                    if payload.len() < n {
                        return Err(Error::invalid(format!(
                            "magicyuv: raw slice payload too short ({} < {n})",
                            payload.len(),
                        )));
                    }
                    tile.copy_from_slice(&payload[..n]);
                } else {
                    // Huffman residuals.
                    let huff = &huffmans[plane];
                    let mut br = BitReader::new(payload);
                    for px in tile.iter_mut() {
                        *px = huff.decode(&mut br)? as u8;
                    }
                }

                // Apply the per-slice predictor on the tile (slice-local
                // coordinates: stride == plane_w, height == plane_h).
                match pred {
                    Predictor::Left => apply_left_u8(&mut tile, plane_w, plane_w, plane_h),
                    Predictor::Gradient => apply_gradient_u8(&mut tile, plane_w, plane_w, plane_h),
                    Predictor::Median => apply_median_u8(&mut tile, plane_w, plane_w, plane_h),
                }

                // Blit tile into the plane buffer.
                for r in 0..plane_h {
                    let dst_off = (plane_row_start + r) * pw + plane_col_start;
                    planes[plane][dst_off..dst_off + plane_w]
                        .copy_from_slice(&tile[r * plane_w..(r + 1) * plane_w]);
                }
            }
        }
    }

    // RGB decorrelation pass. Wire plane order for GBRP/GBRAP family is
    // empirically B', G, R' (see crate-level note in the original
    // decoder). Apply across the entire plane after all slices land.
    if header.format.rgb_decorrelated() {
        let total = plane_widths[1] * plane_heights[1];
        debug_assert_eq!(total, planes[1].len());
        for i in 0..total {
            let g = planes[1][i];
            planes[0][i] = planes[0][i].wrapping_add(g);
            planes[2][i] = planes[2][i].wrapping_add(g);
        }
    }

    let out = pack_output_u8(&header, planes, plane_widths, plane_heights);
    Ok(VideoFrame { pts, planes: out })
}

#[allow(clippy::too_many_arguments)]
fn decode_packet_u16(
    bytes: &[u8],
    pts: Option<i64>,
    header: FileHeader,
    table: SliceOffsetTable,
    plane_widths: Vec<usize>,
    plane_heights: Vec<usize>,
    alphabet: usize,
) -> Result<VideoFrame> {
    let nb_planes = header.format.planes();
    let bps = header.format.bps();
    let mask: u16 = if bps >= 16 {
        0xFFFF
    } else {
        ((1u32 << bps) - 1) as u16
    };

    let mut planes: Vec<Vec<u16>> = (0..nb_planes)
        .map(|p| vec![0u16; plane_widths[p] * plane_heights[p]])
        .collect();

    let mut huffmans: Vec<CanonicalHuffman> = Vec::with_capacity(nb_planes);
    {
        let mut p = table.huffman_start;
        for _ in 0..nb_planes {
            let (lt, np) = LengthTable::parse(bytes, p, alphabet)?;
            p = np;
            huffmans.push(CanonicalHuffman::build(&lt.lengths)?);
        }
    }

    let nb_x = header.nb_slices_x();
    let nb_y = header.nb_slices_y();

    for sy in 0..nb_y {
        for sx in 0..nb_x {
            let slice_idx = header.slice_index(sx, sy);
            let (row_start, row_end) = header.slice_row_range(sy);
            let (col_start, col_end) = header.slice_col_range(sx);
            for plane in 0..nb_planes {
                let pw = plane_widths[plane];
                let v_sub = chroma_v_sub(&header, plane);
                let h_sub = chroma_h_sub(&header, plane);
                let plane_row_start = row_start >> v_sub;
                let plane_row_end = subsample_end(row_end, header.height as usize, v_sub);
                let plane_h = plane_row_end - plane_row_start;
                let plane_col_start = col_start >> h_sub;
                let plane_col_end = subsample_end(col_end, header.width as usize, h_sub);
                let plane_w = plane_col_end - plane_col_start;

                let pstart = table.starts[plane][slice_idx];
                let pend = table.ends[plane][slice_idx];
                if pstart + 2 > pend || pend > bytes.len() {
                    return Err(Error::invalid(format!(
                        "magicyuv: slice {slice_idx} plane {plane} bad range \
                         [{pstart}..{pend}) of {})",
                        bytes.len(),
                    )));
                }
                let flag = bytes[pstart];
                let pred = Predictor::from_byte(bytes[pstart + 1])?;
                let payload = &bytes[pstart + 2..pend];

                let needed = plane_h * plane_w;
                let mut tile = vec![0u16; needed];

                if flag & 1 != 0 {
                    // Raw mode: 16-bit-packed little-endian samples
                    // (one u16 per sample, bps bits used; high bits 0).
                    let need_bytes = needed * 2;
                    if payload.len() < need_bytes {
                        return Err(Error::invalid(format!(
                            "magicyuv: raw slice payload too short ({} < {need_bytes})",
                            payload.len(),
                        )));
                    }
                    for (i, px) in tile.iter_mut().enumerate() {
                        let lo = payload[2 * i] as u16;
                        let hi = payload[2 * i + 1] as u16;
                        *px = (lo | (hi << 8)) & mask;
                    }
                } else {
                    let huff = &huffmans[plane];
                    let mut br = BitReader::new(payload);
                    for px in tile.iter_mut() {
                        *px = huff.decode(&mut br)? as u16;
                    }
                }

                match pred {
                    Predictor::Left => apply_left_u16(&mut tile, plane_w, plane_w, plane_h, mask),
                    Predictor::Gradient => {
                        apply_gradient_u16(&mut tile, plane_w, plane_w, plane_h, mask)
                    }
                    Predictor::Median => {
                        apply_median_u16(&mut tile, plane_w, plane_w, plane_h, mask)
                    }
                }

                for r in 0..plane_h {
                    let dst_off = (plane_row_start + r) * pw + plane_col_start;
                    planes[plane][dst_off..dst_off + plane_w]
                        .copy_from_slice(&tile[r * plane_w..(r + 1) * plane_w]);
                }
            }
        }
    }

    if header.format.rgb_decorrelated() {
        let total = plane_widths[1] * plane_heights[1];
        for i in 0..total {
            let g = planes[1][i];
            planes[0][i] = ((planes[0][i] as u32).wrapping_add(g as u32) as u16) & mask;
            planes[2][i] = ((planes[2][i] as u32).wrapping_add(g as u32) as u16) & mask;
        }
    }

    let out = pack_output_u16(&header, planes, plane_widths, plane_heights);
    Ok(VideoFrame { pts, planes: out })
}

fn plane_widths(h: &FileHeader) -> Vec<usize> {
    let w = h.width as usize;
    let h_sub = h.format.h_subsample() as usize;
    match h.format.family() {
        FormatFamily::Gray => vec![w],
        FormatFamily::Gbrp => vec![w; 3],
        FormatFamily::Gbrap => vec![w; 4],
        FormatFamily::Yuva444P => vec![w; 4],
        FormatFamily::Yuv444P => vec![w; 3],
        FormatFamily::Yuv422P | FormatFamily::Yuv420P => {
            let cw = (w + (1 << h_sub) - 1) >> h_sub;
            vec![w, cw, cw]
        }
    }
}

fn plane_heights(h: &FileHeader) -> Vec<usize> {
    let height = h.height as usize;
    let v_sub = h.format.v_subsample() as usize;
    match h.format.family() {
        FormatFamily::Gray => vec![height],
        FormatFamily::Gbrp => vec![height; 3],
        FormatFamily::Gbrap => vec![height; 4],
        FormatFamily::Yuva444P => vec![height; 4],
        FormatFamily::Yuv444P | FormatFamily::Yuv422P => vec![height; 3],
        FormatFamily::Yuv420P => {
            let ch = (height + (1 << v_sub) - 1) >> v_sub;
            vec![height, ch, ch]
        }
    }
}

fn chroma_v_sub(h: &FileHeader, plane: usize) -> usize {
    if plane == 0 {
        return 0;
    }
    if plane >= 3 {
        return 0;
    }
    h.format.v_subsample() as usize
}

fn chroma_h_sub(h: &FileHeader, plane: usize) -> usize {
    if plane == 0 {
        return 0;
    }
    if plane >= 3 {
        return 0;
    }
    h.format.h_subsample() as usize
}

/// Convert a luma boundary into a chroma boundary, rounding **up** for
/// the trailing edge so the boundary still covers the full luma extent
/// when the frame is not divisible by the chroma subsampling factor.
fn subsample_end(luma_end: usize, total: usize, sub: usize) -> usize {
    if sub == 0 {
        return luma_end;
    }
    let chroma_total = (total + (1 << sub) - 1) >> sub;
    let r = luma_end >> sub;
    r.min(chroma_total)
}

fn pack_output_u8(
    header: &FileHeader,
    planes: Vec<Vec<u8>>,
    pw: Vec<usize>,
    ph: Vec<usize>,
) -> Vec<VideoPlane> {
    let pf = header.format.output_pixel_format();
    let w = header.width as usize;
    let h = header.height as usize;
    match (header.format.family(), pf) {
        (FormatFamily::Gray, PixelFormat::Gray8) => vec![VideoPlane {
            stride: w,
            data: planes.into_iter().next().unwrap(),
        }],
        (FormatFamily::Yuv422P, _)
        | (FormatFamily::Yuv420P, _)
        | (FormatFamily::Yuv444P, _)
        | (FormatFamily::Yuva444P, _) => {
            let mut out = Vec::with_capacity(planes.len());
            for (i, data) in planes.into_iter().enumerate() {
                out.push(VideoPlane {
                    stride: pw[i],
                    data,
                });
            }
            for i in 0..out.len() {
                debug_assert_eq!(out[i].data.len(), pw[i] * ph[i]);
            }
            out
        }
        (FormatFamily::Gbrp, PixelFormat::Rgb24) => {
            let mut packed = vec![0u8; w * h * 3];
            for i in 0..(w * h) {
                packed[i * 3] = planes[2][i]; // R
                packed[i * 3 + 1] = planes[1][i]; // G
                packed[i * 3 + 2] = planes[0][i]; // B
            }
            vec![VideoPlane {
                stride: w * 3,
                data: packed,
            }]
        }
        (FormatFamily::Gbrap, PixelFormat::Rgba) => {
            let mut packed = vec![0u8; w * h * 4];
            for i in 0..(w * h) {
                packed[i * 4] = planes[2][i]; // R
                packed[i * 4 + 1] = planes[1][i]; // G
                packed[i * 4 + 2] = planes[0][i]; // B
                packed[i * 4 + 3] = planes[3][i]; // A
            }
            vec![VideoPlane {
                stride: w * 4,
                data: packed,
            }]
        }
        _ => unreachable!("unhandled (family, pixel_format) pair for u8 path"),
    }
}

fn pack_output_u16(
    header: &FileHeader,
    planes: Vec<Vec<u16>>,
    pw: Vec<usize>,
    ph: Vec<usize>,
) -> Vec<VideoPlane> {
    let pf = header.format.output_pixel_format();
    match (header.format.family(), pf) {
        (FormatFamily::Gray, PixelFormat::Gray10Le) => {
            let p = planes.into_iter().next().unwrap();
            vec![VideoPlane {
                stride: pw[0] * 2,
                data: u16_to_le_bytes(p),
            }]
        }
        (FormatFamily::Yuv422P, _) | (FormatFamily::Yuv420P, _) | (FormatFamily::Yuv444P, _) => {
            let mut out = Vec::with_capacity(planes.len());
            for (i, data) in planes.into_iter().enumerate() {
                let stride = pw[i] * 2;
                out.push(VideoPlane {
                    stride,
                    data: u16_to_le_bytes(data),
                });
            }
            for i in 0..out.len() {
                debug_assert_eq!(out[i].data.len(), pw[i] * ph[i] * 2);
            }
            out
        }
        _ => unreachable!("unhandled (family, pixel_format) pair for u16 path"),
    }
}

fn u16_to_le_bytes(v: Vec<u16>) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for s in v {
        out.push((s & 0xFF) as u8);
        out.push(((s >> 8) & 0xFF) as u8);
    }
    out
}
