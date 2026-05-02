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
//!    pair, decode `slice_h * plane_w` residuals (Huffman if flag bit 0
//!    clear, raw bytes if set), apply the per-row predictor.
//! 5. If the format is RGB-decorrelated (`M8RG` / `M8RA`), invert the
//!    `B' = B - G; R' = R - G` transform pixel-by-pixel and pack the
//!    result into the chosen output `PixelFormat` (RGB24 / RGBA).

use oxideav_core::frame::VideoPlane;
use oxideav_core::Decoder;
use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

use crate::bitstream::BitReader;
use crate::header::{FileHeader, FormatCode, SliceOffsetTable};
use crate::huffman::{CanonicalHuffman, LengthTable};
use crate::predictor::{apply_gradient_u8, apply_left_u8, apply_median_u8, Predictor};

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
    let nb_slices = header.nb_slices();
    let nb_planes = header.format.planes();

    // Plane dimensions (pre-decorrelation, pre-output packing).
    let plane_widths = plane_widths(&header);
    let plane_heights = plane_heights(&header);

    // Allocate per-plane working buffers, contiguous & tightly packed
    // (stride == width). These hold the raw decoded samples in plane
    // order: for GBRP that's G/B/R; for YUV that's Y/U/V; for GRAY8
    // it's the single luma plane.
    let mut planes: Vec<Vec<u8>> = (0..nb_planes)
        .map(|p| vec![0u8; plane_widths[p] * plane_heights[p]])
        .collect();

    // Decode the per-plane Huffman length descriptors.
    let alphabet = 1usize << header.format.bps();
    let mut huffmans: Vec<CanonicalHuffman> = Vec::with_capacity(nb_planes);
    {
        let mut p = table.huffman_start;
        for _ in 0..nb_planes {
            let (lt, np) = LengthTable::parse(bytes, p, alphabet)?;
            p = np;
            huffmans.push(CanonicalHuffman::build(&lt.lengths)?);
        }
    }

    // Decode each slice, plane by plane.
    for slice in 0..nb_slices {
        let (row_start, row_end) = header.slice_row_range(slice);
        for plane in 0..nb_planes {
            let pw = plane_widths[plane];
            // Subsample the slice-row range vertically for chroma planes.
            let v_sub = chroma_v_sub(&header, plane);
            let h_sub = chroma_h_sub(&header, plane);
            let plane_row_start = row_start >> v_sub;
            let plane_row_end = row_end_for_plane(row_end, header.height as usize, v_sub);
            let plane_h = plane_row_end - plane_row_start;
            let _ = h_sub;

            let pstart = table.offsets[plane][slice];
            let pend = table.offsets[plane][slice + 1];
            if pstart + 2 > pend || pend > bytes.len() {
                return Err(Error::invalid(format!(
                    "magicyuv: slice {slice} plane {plane} bad range [{pstart}..{pend}) of {})",
                    bytes.len(),
                )));
            }
            let flag = bytes[pstart];
            let pred = Predictor::from_byte(bytes[pstart + 1])?;
            let payload = &bytes[pstart + 2..pend];

            // Slice destination view inside the plane buffer.
            let dst_off = plane_row_start * pw;
            let dst = &mut planes[plane][dst_off..dst_off + plane_h * pw];

            if flag & 1 != 0 {
                // Raw mode: literal bytes (8-bit only here).
                if payload.len() < dst.len() {
                    return Err(Error::invalid(format!(
                        "magicyuv: raw slice payload too short ({} < {})",
                        payload.len(),
                        dst.len()
                    )));
                }
                dst.copy_from_slice(&payload[..dst.len()]);
            } else {
                // Huffman residuals.
                let huff = &huffmans[plane];
                let mut br = BitReader::new(payload);
                for px in dst.iter_mut() {
                    *px = huff.decode(&mut br)? as u8;
                }
            }

            // Apply the per-slice predictor.
            match pred {
                Predictor::Left => apply_left_u8(dst, pw, pw, plane_h),
                Predictor::Gradient => apply_gradient_u8(dst, pw, pw, plane_h),
                Predictor::Median => apply_median_u8(dst, pw, pw, plane_h),
            }

            // RGB decorrelation: applied per-slice after predictor,
            // before storing pixels (trace doc §3.7). For the MagicYUV
            // decorrelation: G is plane 0, B is plane 1, R is plane 2.
            // We add plane 0 (G) to planes 1 (B) and 2 (R) at the same
            // pixel index. Done after both planes' predictors run, so
            // we defer this to the post-loop pass below.
        }
    }

    // RGB decorrelation pass — invert the encoder's B' = B - G,
    // R' = R - G transform. Empirically the wire plane order for the
    // GBRP / GBRAP family (`0x65` / `0x66`) is **B', G, R'** — NOT
    // G, B, R as the trace document's prose implies. Verified with a
    // pure-green (R=0, G=255, B=1) constant-color fixture: the
    // second-most-common residual sym in the wire's first descriptor
    // is sym 2 (= B' for green = 1 − 255 = 2 mod 256), sym 255 in
    // the second descriptor (= G), and sym 1 in the third (= R' =
    // 0 − 255 = 1 mod 256).
    if header.format.rgb_decorrelated() {
        let total = plane_widths[1] * plane_heights[1];
        debug_assert_eq!(total, planes[1].len());
        for i in 0..total {
            let g = planes[1][i];
            planes[0][i] = planes[0][i].wrapping_add(g);
            planes[2][i] = planes[2][i].wrapping_add(g);
        }
        // Alpha (plane 3) for GBRAP passes through.
    }

    // Pack into the output VideoFrame.
    let out = pack_output(&header, planes, plane_widths, plane_heights);
    Ok(VideoFrame { pts, planes: out })
}

fn plane_widths(h: &FileHeader) -> Vec<usize> {
    let w = h.width as usize;
    let h_sub = h.format.h_subsample() as usize;
    match h.format {
        FormatCode::Gray8 => vec![w],
        FormatCode::Gbrp => vec![w; 3],
        FormatCode::Gbrap => vec![w; 4],
        FormatCode::Yuva444P => vec![w; 4],
        FormatCode::Yuv444P => vec![w; 3],
        FormatCode::Yuv422P | FormatCode::Yuv420P => {
            let cw = (w + (1 << h_sub) - 1) >> h_sub;
            vec![w, cw, cw]
        }
    }
}

fn plane_heights(h: &FileHeader) -> Vec<usize> {
    let height = h.height as usize;
    let v_sub = h.format.v_subsample() as usize;
    match h.format {
        FormatCode::Gray8 => vec![height],
        FormatCode::Gbrp => vec![height; 3],
        FormatCode::Gbrap => vec![height; 4],
        FormatCode::Yuva444P => vec![height; 4],
        FormatCode::Yuv444P | FormatCode::Yuv422P => vec![height; 3],
        FormatCode::Yuv420P => {
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
        // Alpha plane — full resolution.
        return 0;
    }
    h.format.v_subsample() as usize
}

fn chroma_h_sub(h: &FileHeader, plane: usize) -> usize {
    if plane == 0 || plane >= 3 {
        return 0;
    }
    h.format.h_subsample() as usize
}

/// Convert a luma row-end into a chroma row-end with vertical subsampling.
fn row_end_for_plane(luma_row_end: usize, total_height: usize, v_sub: usize) -> usize {
    if v_sub == 0 {
        return luma_row_end;
    }
    // For YUV 4:2:0, the chroma plane is exactly height/2 rows tall;
    // each chroma row covers two luma rows. The slice row-end is always
    // even per §3.3, so no rounding needed.
    let chroma_total = (total_height + (1 << v_sub) - 1) >> v_sub;
    let r = luma_row_end >> v_sub;
    r.min(chroma_total)
}

fn pack_output(
    header: &FileHeader,
    planes: Vec<Vec<u8>>,
    pw: Vec<usize>,
    ph: Vec<usize>,
) -> Vec<VideoPlane> {
    let pf = header.format.output_pixel_format();
    let w = header.width as usize;
    let h = header.height as usize;
    match (header.format, pf) {
        (FormatCode::Gray8, PixelFormat::Gray8) => vec![VideoPlane {
            stride: w,
            data: planes.into_iter().next().unwrap(),
        }],
        (FormatCode::Yuv422P, _)
        | (FormatCode::Yuv420P, _)
        | (FormatCode::Yuv444P, _)
        | (FormatCode::Yuva444P, _) => {
            // Already planar in our buffers — emit as-is, in plane order.
            // For YUV the plane order is Y/U/V which is what core expects.
            let mut out = Vec::with_capacity(planes.len());
            for (i, data) in planes.into_iter().enumerate() {
                out.push(VideoPlane {
                    stride: pw[i],
                    data,
                });
            }
            // Sanity: planes[i] length == pw[i] * ph[i].
            for i in 0..out.len() {
                debug_assert_eq!(out[i].data.len(), pw[i] * ph[i]);
            }
            out
        }
        (FormatCode::Gbrp, PixelFormat::Rgb24) => {
            // Wire plane 0 = B, plane 1 = G, plane 2 = R (after the
            // GBR decorrelation pass that ran above). Pack R, G, B.
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
        (FormatCode::Gbrap, PixelFormat::Rgba) => {
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
        _ => unreachable!("unhandled (format, pixel_format) pair"),
    }
}
