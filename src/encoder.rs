//! MagicYUV `Frame`-to-`Packet` encoder (8-bit).
//!
//! Mirror of [`crate::decoder`]. For every input video frame the
//! encoder produces one self-contained intra packet whose layout
//! matches what FFmpeg's `magicyuv` decoder accepts:
//!
//! 1. 32-byte file header (`MAGY` magic, version 7, format byte,
//!    width / height / slice geometry).
//! 2. Slice-offset table (plane-major le32 starts relative to byte 32),
//!    `nb_planes` sanity byte, permutation array.
//! 3. Per-plane Huffman length descriptors (single-byte form, one byte
//!    per symbol of the alphabet).
//! 4. Slice payloads in **slice-major** physical order:
//!    `(p0,s0)(p1,s0)…(pP-1,s0)(p0,s1)…(pP-1,sN-1)`. Each payload is a
//!    2-byte prefix `(flag, predictor)` followed by the MSB-first
//!    canonical-Huffman residual stream.
//!
//! The trace doc (§3.7) documents the slice-major physical layout with
//! per-plane plane-major offsets — so the offset table is plane-major
//! but the actual payload order in the byte stream is slice-major. We
//! mirror this exactly.
//!
//! Today the encoder supports the same seven 8-bit format codes the
//! existing FFmpeg encoder emits (`M8RG`, `M8RA`, `M8Y4`, `M8Y2`,
//! `M8Y0`, `M8YA`, `M8G0`), all three predictors (`left`, `gradient`,
//! `median`), and an arbitrary `nb_slices_y × nb_slices_x` slice grid
//! (controlled via `slice_height` / `slice_width` options). 10-bit
//! encoding is wired through the same path but currently only exposed
//! to internal tests — the workspace's downstream encoders all use
//! 8-bit today.

use oxideav_core::frame::VideoPlane;
use oxideav_core::time::TimeBase;
use oxideav_core::{
    CodecId, CodecParameters, Encoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

use crate::bitstream::BitWriter;
use crate::header::{FormatCode, FormatFamily};
use crate::huffman::{build_canonical_codes, build_lengths_from_histogram};
use crate::predictor::{
    encode_gradient_u16, encode_gradient_u8, encode_left_u16, encode_left_u8, encode_median_u16,
    encode_median_u8, Predictor,
};

/// Default Huffman length cap (matches FFmpeg's `magicyuvenc.c`
/// `MAX_HUFF_LEN = 12`).
const DEFAULT_HUFF_MAX_LEN: u8 = 12;

pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    let width = params
        .width
        .ok_or_else(|| Error::invalid("magicyuv encoder: missing width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("magicyuv encoder: missing height"))?;
    let pixel_format = params
        .pixel_format
        .ok_or_else(|| Error::invalid("magicyuv encoder: missing pixel_format"))?;

    let format = format_from_pixel_format(pixel_format)?;
    if width == 0 || height == 0 {
        return Err(Error::invalid("magicyuv encoder: zero-sized frame"));
    }
    let predictor = predictor_from_options(params)?;

    let h_sub = format.h_subsample();
    let v_sub = format.v_subsample();
    // slice_height defaults to height (single row band); rounded up to
    // chroma vertical subsampling. slice_width defaults to width.
    let mut slice_height = parse_u32_option(params, "slice_height").unwrap_or(height);
    let mut slice_width = parse_u32_option(params, "slice_width").unwrap_or(width);
    if slice_height == 0 {
        slice_height = height;
    }
    if slice_width == 0 {
        slice_width = width;
    }
    if slice_width > width {
        slice_width = width;
    }
    if slice_height > height {
        slice_height = height;
    }
    // Align slice_width to chroma h_sub. The trace doc requires
    // boundary alignment.
    if h_sub > 0 {
        let chunk = 1u32 << h_sub;
        if slice_width % chunk != 0 {
            slice_width = slice_width.div_ceil(chunk) * chunk;
            if slice_width > width {
                slice_width = width;
            }
        }
    }
    if v_sub > 0 {
        let chunk = 1u32 << v_sub;
        if slice_height % chunk != 0 {
            slice_height = slice_height.div_ceil(chunk) * chunk;
            if slice_height > height {
                slice_height = height;
            }
        }
    }

    let mut output_params = params.clone();
    output_params.width = Some(width);
    output_params.height = Some(height);
    output_params.pixel_format = Some(pixel_format);

    Ok(Box::new(MagicYuvEncoder {
        codec_id: params.codec_id.clone(),
        format,
        predictor,
        width,
        height,
        slice_width,
        slice_height,
        output_params,
        pending: None,
    }))
}

fn parse_u32_option(params: &CodecParameters, key: &str) -> Option<u32> {
    params.options.get(key).and_then(|s| s.parse::<u32>().ok())
}

fn predictor_from_options(params: &CodecParameters) -> Result<Predictor> {
    if let Some(v) = params.options.get("predictor") {
        match v.to_ascii_lowercase().as_str() {
            "left" => Ok(Predictor::Left),
            "gradient" => Ok(Predictor::Gradient),
            "median" => Ok(Predictor::Median),
            other => Err(Error::invalid(format!(
                "magicyuv encoder: unknown predictor option `{other}`"
            ))),
        }
    } else {
        Ok(Predictor::Left)
    }
}

/// Map a `PixelFormat` to the MagicYUV format byte the encoder will emit.
pub fn format_from_pixel_format(pf: PixelFormat) -> Result<FormatCode> {
    Ok(match pf {
        PixelFormat::Rgb24 => FormatCode::Gbrp,
        PixelFormat::Rgba => FormatCode::Gbrap,
        PixelFormat::Yuv444P => FormatCode::Yuv444P,
        PixelFormat::Yuv422P => FormatCode::Yuv422P,
        PixelFormat::Yuv420P => FormatCode::Yuv420P,
        PixelFormat::Gray8 => FormatCode::Gray8,
        PixelFormat::Yuv422P10Le => FormatCode::Yuv422P10,
        PixelFormat::Yuv444P10Le => FormatCode::Yuv444P10,
        PixelFormat::Yuv420P10Le => FormatCode::Yuv420P10,
        PixelFormat::Gray10Le => FormatCode::Gray10,
        other => {
            return Err(Error::unsupported(format!(
                "magicyuv encoder: pixel format {other:?} not supported"
            )));
        }
    })
}

struct MagicYuvEncoder {
    codec_id: CodecId,
    format: FormatCode,
    predictor: Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    output_params: CodecParameters,
    pending: Option<Packet>,
}

impl Encoder for MagicYuvEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let v = match frame {
            Frame::Video(v) => v,
            _ => return Err(Error::invalid("magicyuv encoder: only video frames")),
        };
        if self.pending.is_some() {
            return Err(Error::other(
                "magicyuv encoder: receive_packet must be called between frames",
            ));
        }
        let payload = encode_frame(
            self.format,
            self.predictor,
            self.width,
            self.height,
            self.slice_width,
            self.slice_height,
            v,
        )?;
        let mut pkt = Packet::new(0, TimeBase::new(1, 25), payload).with_keyframe(true);
        if let Some(pts) = v.pts {
            pkt = pkt.with_pts(pts);
        }
        self.pending = Some(pkt);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        self.pending.take().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Build one MagicYUV packet for `frame`. Public so tests can drive the
/// encoder without going through the trait.
pub fn encode_frame(
    format: FormatCode,
    predictor: Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    frame: &VideoFrame,
) -> Result<Vec<u8>> {
    if format.is_high_bit_depth() {
        encode_frame_u16(
            format,
            predictor,
            width,
            height,
            slice_width,
            slice_height,
            frame,
        )
    } else {
        encode_frame_u8(
            format,
            predictor,
            width,
            height,
            slice_width,
            slice_height,
            frame,
        )
    }
}

fn encode_frame_u8(
    format: FormatCode,
    predictor: Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    frame: &VideoFrame,
) -> Result<Vec<u8>> {
    let nb_planes = format.planes();
    let plane_widths = plane_widths(format, width);
    let plane_heights = plane_heights(format, height);
    let alphabet: usize = 256;

    // Step 1: lift the input frame into the encoder's per-plane working
    // buffers (tightly packed, stride == plane_width). For RGB-decorrelated
    // formats we also apply the encoder-side B' = B-G, R' = R-G transform
    // here — the wire's plane order is empirically (B', G, R'[, A]).
    let planes_u8 = pack_planes_u8(format, frame, &plane_widths, &plane_heights, width, height)?;

    // Step 2: per-slice, per-plane: compute residuals on the slice
    // rectangle, accumulate per-plane symbol histograms.
    let nb_x = (width as usize).div_ceil(slice_width as usize);
    let nb_y = (height as usize).div_ceil(slice_height as usize);
    let nb_slices = nb_x * nb_y;

    let mut hists: Vec<Vec<u64>> = vec![vec![0u64; alphabet]; nb_planes];
    // residuals[plane][slice] — we allocate up-front so we can write to
    // them in plane-major order (the encoder's natural inner loop) and
    // emit them in slice-major order (the wire requirement).
    let mut residuals: Vec<Vec<Vec<u8>>> = vec![Vec::with_capacity(nb_slices); nb_planes];
    for plane in 0..nb_planes {
        for _ in 0..nb_slices {
            residuals[plane].push(Vec::new());
        }
    }

    for sy in 0..nb_y {
        let row_start = sy * slice_height as usize;
        let row_end = ((sy + 1) * slice_height as usize).min(height as usize);
        for sx in 0..nb_x {
            let col_start = sx * slice_width as usize;
            let col_end = ((sx + 1) * slice_width as usize).min(width as usize);
            let slice_idx = sy * nb_x + sx;
            for plane in 0..nb_planes {
                let pw = plane_widths[plane];
                let v_sub = chroma_v_sub(format, plane);
                let h_sub = chroma_h_sub(format, plane);
                let plane_row_start = row_start >> v_sub;
                let plane_row_end = subsample_end(row_end, height as usize, v_sub);
                let plane_h = plane_row_end - plane_row_start;
                let plane_col_start = col_start >> h_sub;
                let plane_col_end = subsample_end(col_end, width as usize, h_sub);
                let plane_w = plane_col_end - plane_col_start;
                let needed = plane_h * plane_w;

                // Copy the tile into a contiguous buffer so the
                // predictor / encoder-residual functions see
                // stride == plane_w.
                let mut tile = vec![0u8; needed];
                for r in 0..plane_h {
                    let src_off = (plane_row_start + r) * pw + plane_col_start;
                    tile[r * plane_w..(r + 1) * plane_w]
                        .copy_from_slice(&planes_u8[plane][src_off..src_off + plane_w]);
                }

                let mut tile_res = vec![0u8; needed];
                match predictor {
                    Predictor::Left => {
                        encode_left_u8(&tile, plane_w, plane_w, plane_h, &mut tile_res);
                    }
                    Predictor::Gradient => {
                        encode_gradient_u8(&tile, plane_w, plane_w, plane_h, &mut tile_res);
                    }
                    Predictor::Median => {
                        encode_median_u8(&tile, plane_w, plane_w, plane_h, &mut tile_res);
                    }
                }
                for &b in tile_res.iter() {
                    hists[plane][b as usize] += 1;
                }
                residuals[plane][slice_idx] = tile_res;
            }
        }
    }

    // Step 3: build per-plane Huffman length tables + canonical wire codes.
    let mut lengths: Vec<Vec<u8>> = Vec::with_capacity(nb_planes);
    let mut codes: Vec<Vec<(u32, u8)>> = Vec::with_capacity(nb_planes);
    for hist in hists.iter() {
        let lens = build_lengths_from_histogram(hist, DEFAULT_HUFF_MAX_LEN)?;
        let cd = build_canonical_codes(&lens)?;
        lengths.push(lens);
        codes.push(cd);
    }

    // Step 4: assemble. Header → offset-table-skeleton → permutation →
    // Huffman descriptors → slice payloads. The slice offsets are
    // patched in once we know the actual byte positions.
    assemble_packet(
        format,
        predictor,
        width,
        height,
        slice_width,
        slice_height,
        nb_x,
        nb_y,
        nb_planes,
        &lengths,
        &codes,
        &residuals,
        false,
        &[],
    )
}

fn encode_frame_u16(
    format: FormatCode,
    predictor: Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    frame: &VideoFrame,
) -> Result<Vec<u8>> {
    let nb_planes = format.planes();
    let bps = format.bps();
    let mask: u16 = if bps >= 16 {
        0xFFFF
    } else {
        ((1u32 << bps) - 1) as u16
    };
    let plane_widths = plane_widths(format, width);
    let plane_heights = plane_heights(format, height);
    let alphabet: usize = 1 << bps;

    let planes_u16 = pack_planes_u16(
        format,
        frame,
        &plane_widths,
        &plane_heights,
        width,
        height,
        mask,
    )?;

    let nb_x = (width as usize).div_ceil(slice_width as usize);
    let nb_y = (height as usize).div_ceil(slice_height as usize);
    let nb_slices = nb_x * nb_y;

    let mut hists: Vec<Vec<u64>> = vec![vec![0u64; alphabet]; nb_planes];
    let mut residuals: Vec<Vec<Vec<u16>>> = vec![Vec::with_capacity(nb_slices); nb_planes];
    for plane in 0..nb_planes {
        for _ in 0..nb_slices {
            residuals[plane].push(Vec::new());
        }
    }

    for sy in 0..nb_y {
        let row_start = sy * slice_height as usize;
        let row_end = ((sy + 1) * slice_height as usize).min(height as usize);
        for sx in 0..nb_x {
            let col_start = sx * slice_width as usize;
            let col_end = ((sx + 1) * slice_width as usize).min(width as usize);
            let slice_idx = sy * nb_x + sx;
            for plane in 0..nb_planes {
                let pw = plane_widths[plane];
                let v_sub = chroma_v_sub(format, plane);
                let h_sub = chroma_h_sub(format, plane);
                let plane_row_start = row_start >> v_sub;
                let plane_row_end = subsample_end(row_end, height as usize, v_sub);
                let plane_h = plane_row_end - plane_row_start;
                let plane_col_start = col_start >> h_sub;
                let plane_col_end = subsample_end(col_end, width as usize, h_sub);
                let plane_w = plane_col_end - plane_col_start;
                let needed = plane_h * plane_w;

                let mut tile = vec![0u16; needed];
                for r in 0..plane_h {
                    let src_off = (plane_row_start + r) * pw + plane_col_start;
                    tile[r * plane_w..(r + 1) * plane_w]
                        .copy_from_slice(&planes_u16[plane][src_off..src_off + plane_w]);
                }

                let mut tile_res = vec![0u16; needed];
                match predictor {
                    Predictor::Left => {
                        encode_left_u16(&tile, plane_w, plane_w, plane_h, mask, &mut tile_res)
                    }
                    Predictor::Gradient => {
                        encode_gradient_u16(&tile, plane_w, plane_w, plane_h, mask, &mut tile_res)
                    }
                    Predictor::Median => {
                        encode_median_u16(&tile, plane_w, plane_w, plane_h, mask, &mut tile_res)
                    }
                }
                for &s in tile_res.iter() {
                    hists[plane][(s & mask) as usize] += 1;
                }
                residuals[plane][slice_idx] = tile_res;
            }
        }
    }

    let mut lengths: Vec<Vec<u8>> = Vec::with_capacity(nb_planes);
    let mut codes: Vec<Vec<(u32, u8)>> = Vec::with_capacity(nb_planes);
    for hist in hists.iter() {
        // For 10/12/14-bit the deepest code can need >12 bits because
        // the alphabet is up to 16384 wide. Bump the cap up to the
        // canonical-builder hard limit (32) — the decoder reads MSB-
        // first so accepting longer codes here is safe.
        let lens = build_lengths_from_histogram(hist, crate::huffman::MAX_HUFF_LENGTH)?;
        let cd = build_canonical_codes(&lens)?;
        lengths.push(lens);
        codes.push(cd);
    }

    // Convert u16 residuals to a flat u32 per slice for the writer
    // helper — we only call this side from the same module so it's
    // OK to keep the signature wide.
    let res32: Vec<Vec<Vec<u32>>> = residuals
        .iter()
        .map(|plane_slices| {
            plane_slices
                .iter()
                .map(|tile| tile.iter().map(|&v| (v & mask) as u32).collect())
                .collect()
        })
        .collect();

    assemble_packet(
        format,
        predictor,
        width,
        height,
        slice_width,
        slice_height,
        nb_x,
        nb_y,
        nb_planes,
        &lengths,
        &codes,
        &Vec::new(), // u8 path unused
        true,
        &res32,
    )
}

#[allow(clippy::too_many_arguments)]
fn assemble_packet(
    format: FormatCode,
    _predictor: Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    nb_x: usize,
    nb_y: usize,
    nb_planes: usize,
    lengths: &[Vec<u8>],
    codes: &[Vec<(u32, u8)>],
    residuals_u8: &[Vec<Vec<u8>>],
    high_bit_depth: bool,
    residuals_u32: &[Vec<Vec<u32>>],
) -> Result<Vec<u8>> {
    let nb_slices = nb_x * nb_y;
    let alphabet: usize = if high_bit_depth {
        1 << format.bps()
    } else {
        256
    };
    let huff_table_bytes = alphabet; // single-byte form
    let mut bytes: Vec<u8> = Vec::with_capacity(2048);

    // ── Fixed header (32 bytes) ──
    bytes.extend_from_slice(b"MAGY");
    bytes.extend_from_slice(&32u32.to_le_bytes()); // header_size_field
    bytes.push(7); // version
    bytes.push(format.as_byte()); // format
    bytes.push(DEFAULT_HUFF_MAX_LEN); // max_huff_length (advisory)
    bytes.push(0); // reserved
    bytes.push(0); // reserved
    bytes.push(0); // color_matrix
    bytes.push(0); // flags
    bytes.push(0); // reserved
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&slice_width.to_le_bytes());
    bytes.extend_from_slice(&slice_height.to_le_bytes());

    // ── Leading le32 (skipped by decoder; FFmpeg encoder writes
    //    `start_of_first_slice - 32`). Filled in once we know the value.
    let leading_pos = bytes.len();
    bytes.extend_from_slice(&0u32.to_le_bytes());

    // ── Slice-offset table: planes × nb_slices le32s, plane-major. We
    //    write zero placeholders here and patch them after we know each
    //    slice's actual start byte.
    let offset_table_pos = bytes.len();
    for _ in 0..(nb_planes * nb_slices) {
        bytes.extend_from_slice(&0u32.to_le_bytes());
    }

    // Sanity byte (== nb_planes).
    bytes.push(nb_planes as u8);

    // Permutation array: deterministic `slice * planes + plane` index
    // (matches FFmpeg's encoder output exactly so the round-trip via
    // FFmpeg's decoder is unambiguous).
    for slice in 0..nb_slices {
        for plane in 0..nb_planes {
            bytes.push(((slice * nb_planes + plane) & 0xFF) as u8);
        }
    }

    // ── Per-plane Huffman length descriptors (single-byte form, MSB
    //    clear). FFmpeg's encoder never emits the run form; we follow
    //    the same convention.
    for lens in lengths.iter() {
        debug_assert_eq!(lens.len(), huff_table_bytes);
        for &l in lens.iter() {
            // Lengths are clamped to ≤ MAX_HUFF_LENGTH by the builder.
            // The single-byte form is `0LLLLLLL` with L ≤ 32.
            // Length 0 (absent) is allowed in our wire convention by
            // way of the canonical-Huffman length-zero rule, but the
            // FFmpeg decoder's `ff_vlc_init_multi_from_lengths` accepts
            // 0 only as "absent". Pass through verbatim.
            bytes.push(l & 0x7F);
        }
    }

    // ── Slice payloads in slice-major file order. Patch the offset
    //    table as we go so the decoder can find each slice.
    //
    // For each (slice, plane): write 2 prefix bytes (flag=0, predictor)
    // followed by the MSB-first Huffman-coded residual stream.
    let pred_byte = match _predictor {
        Predictor::Left => 1u8,
        Predictor::Gradient => 2u8,
        Predictor::Median => 3u8,
    };
    let first_slice_pos = bytes.len();
    for slice in 0..nb_slices {
        for plane in 0..nb_planes {
            let slice_start = bytes.len();
            // Patch wire offset (relative to byte 32).
            let off_in_table = offset_table_pos + (plane * nb_slices + slice) * 4;
            let wire_off = (slice_start as i64 - 32) as u32;
            bytes[off_in_table..off_in_table + 4].copy_from_slice(&wire_off.to_le_bytes());

            // 2-byte prefix: flag = 0 (Huffman, no raw fallback),
            // predictor byte.
            bytes.push(0);
            bytes.push(pred_byte);

            // Huffman-coded residual stream.
            let plane_codes = &codes[plane];
            let mut bw = BitWriter::with_capacity(1024);
            if high_bit_depth {
                for &sym in residuals_u32[plane][slice].iter() {
                    let (wire, len) = plane_codes[sym as usize];
                    if len == 0 {
                        return Err(Error::invalid(format!(
                            "magicyuv encoder: histogram missed sym {sym} on plane {plane}"
                        )));
                    }
                    bw.write_bits(len as u32, wire);
                }
            } else {
                for &sym in residuals_u8[plane][slice].iter() {
                    let (wire, len) = plane_codes[sym as usize];
                    if len == 0 {
                        return Err(Error::invalid(format!(
                            "magicyuv encoder: histogram missed sym {sym} on plane {plane}"
                        )));
                    }
                    bw.write_bits(len as u32, wire);
                }
            }
            let mut payload = bw.finish();
            // Pad each slice to a 4-byte boundary so the next slice
            // starts well-aligned. The trace doc (§6 last bullet)
            // notes the encoder rounds slice allocations to 4-byte
            // multiples and zero-fills trailing bytes for safe over-
            // read; the decoder doesn't care but FFmpeg's offset
            // arithmetic assumes strictly increasing wire offsets so
            // zero padding to 4-byte alignment keeps the slice ends
            // simple.
            while payload.len() % 4 != 0 {
                payload.push(0);
            }
            bytes.append(&mut payload);
        }
    }

    // Patch leading le32 = (first_slice_pos - 32) to match FFmpeg's
    // encoder convention.
    let leading_val = (first_slice_pos as i64 - 32) as u32;
    bytes[leading_pos..leading_pos + 4].copy_from_slice(&leading_val.to_le_bytes());

    Ok(bytes)
}

// ───────────────────── plane geometry helpers ─────────────────────

fn plane_widths(format: FormatCode, width: u32) -> Vec<usize> {
    let w = width as usize;
    let h_sub = format.h_subsample() as usize;
    match format.family() {
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

fn plane_heights(format: FormatCode, height: u32) -> Vec<usize> {
    let h = height as usize;
    let v_sub = format.v_subsample() as usize;
    match format.family() {
        FormatFamily::Gray => vec![h],
        FormatFamily::Gbrp => vec![h; 3],
        FormatFamily::Gbrap => vec![h; 4],
        FormatFamily::Yuva444P => vec![h; 4],
        FormatFamily::Yuv444P | FormatFamily::Yuv422P => vec![h; 3],
        FormatFamily::Yuv420P => {
            let ch = (h + (1 << v_sub) - 1) >> v_sub;
            vec![h, ch, ch]
        }
    }
}

fn chroma_v_sub(format: FormatCode, plane: usize) -> usize {
    if plane == 0 || plane >= 3 {
        return 0;
    }
    format.v_subsample() as usize
}

fn chroma_h_sub(format: FormatCode, plane: usize) -> usize {
    if plane == 0 || plane >= 3 {
        return 0;
    }
    format.h_subsample() as usize
}

fn subsample_end(luma_end: usize, total: usize, sub: usize) -> usize {
    if sub == 0 {
        return luma_end;
    }
    let chroma_total = (total + (1 << sub) - 1) >> sub;
    let r = luma_end >> sub;
    r.min(chroma_total)
}

// ───────────────────── input → working buffer ─────────────────────

/// Lift `frame` into per-plane working buffers, applying the encoder-
/// side decorrelation transform for RGB-decorrelated formats.
fn pack_planes_u8(
    format: FormatCode,
    frame: &VideoFrame,
    plane_widths: &[usize],
    plane_heights: &[usize],
    width: u32,
    height: u32,
) -> Result<Vec<Vec<u8>>> {
    let nb_planes = format.planes();
    let mut out: Vec<Vec<u8>> = (0..nb_planes)
        .map(|p| vec![0u8; plane_widths[p] * plane_heights[p]])
        .collect();

    match format.family() {
        FormatFamily::Gray => {
            if frame.planes.is_empty() {
                return Err(Error::invalid("magicyuv encoder: gray needs 1 plane"));
            }
            blit_plane(
                &frame.planes[0],
                &mut out[0],
                plane_widths[0],
                plane_heights[0],
            )?;
        }
        FormatFamily::Yuv420P | FormatFamily::Yuv422P | FormatFamily::Yuv444P => {
            if frame.planes.len() < 3 {
                return Err(Error::invalid("magicyuv encoder: yuv needs 3 planes"));
            }
            for p in 0..3 {
                blit_plane(
                    &frame.planes[p],
                    &mut out[p],
                    plane_widths[p],
                    plane_heights[p],
                )?;
            }
        }
        FormatFamily::Yuva444P => {
            if frame.planes.len() < 4 {
                return Err(Error::invalid("magicyuv encoder: yuva needs 4 planes"));
            }
            for p in 0..4 {
                blit_plane(
                    &frame.planes[p],
                    &mut out[p],
                    plane_widths[p],
                    plane_heights[p],
                )?;
            }
        }
        FormatFamily::Gbrp => {
            // Input is packed Rgb24 (R, G, B per pixel). Convert to
            // wire plane order (B', G, R') with B' = B - G, R' = R - G.
            if frame.planes.is_empty() {
                return Err(Error::invalid("magicyuv encoder: rgb needs 1 packed plane"));
            }
            let plane = &frame.planes[0];
            let stride = plane.stride;
            let w = width as usize;
            let h = height as usize;
            if stride < w * 3 {
                return Err(Error::invalid("magicyuv encoder: rgb stride < width*3"));
            }
            if plane.data.len() < stride * h {
                return Err(Error::invalid("magicyuv encoder: rgb plane too small"));
            }
            for y in 0..h {
                for x in 0..w {
                    let r = plane.data[y * stride + x * 3];
                    let g = plane.data[y * stride + x * 3 + 1];
                    let b = plane.data[y * stride + x * 3 + 2];
                    let bp = b.wrapping_sub(g);
                    let rp = r.wrapping_sub(g);
                    let i = y * w + x;
                    out[0][i] = bp;
                    out[1][i] = g;
                    out[2][i] = rp;
                }
            }
        }
        FormatFamily::Gbrap => {
            if frame.planes.is_empty() {
                return Err(Error::invalid(
                    "magicyuv encoder: rgba needs 1 packed plane",
                ));
            }
            let plane = &frame.planes[0];
            let stride = plane.stride;
            let w = width as usize;
            let h = height as usize;
            if stride < w * 4 {
                return Err(Error::invalid("magicyuv encoder: rgba stride < width*4"));
            }
            if plane.data.len() < stride * h {
                return Err(Error::invalid("magicyuv encoder: rgba plane too small"));
            }
            for y in 0..h {
                for x in 0..w {
                    let r = plane.data[y * stride + x * 4];
                    let g = plane.data[y * stride + x * 4 + 1];
                    let b = plane.data[y * stride + x * 4 + 2];
                    let a = plane.data[y * stride + x * 4 + 3];
                    let bp = b.wrapping_sub(g);
                    let rp = r.wrapping_sub(g);
                    let i = y * w + x;
                    out[0][i] = bp;
                    out[1][i] = g;
                    out[2][i] = rp;
                    out[3][i] = a;
                }
            }
        }
    }
    Ok(out)
}

fn pack_planes_u16(
    format: FormatCode,
    frame: &VideoFrame,
    plane_widths: &[usize],
    plane_heights: &[usize],
    _width: u32,
    _height: u32,
    mask: u16,
) -> Result<Vec<Vec<u16>>> {
    let nb_planes = format.planes();
    let mut out: Vec<Vec<u16>> = (0..nb_planes)
        .map(|p| vec![0u16; plane_widths[p] * plane_heights[p]])
        .collect();

    match format.family() {
        FormatFamily::Gray => {
            if frame.planes.is_empty() {
                return Err(Error::invalid("magicyuv encoder: gray needs 1 plane"));
            }
            blit_plane_u16(
                &frame.planes[0],
                &mut out[0],
                plane_widths[0],
                plane_heights[0],
                mask,
            )?;
        }
        FormatFamily::Yuv420P | FormatFamily::Yuv422P | FormatFamily::Yuv444P => {
            if frame.planes.len() < 3 {
                return Err(Error::invalid("magicyuv encoder: yuv needs 3 planes"));
            }
            for p in 0..3 {
                blit_plane_u16(
                    &frame.planes[p],
                    &mut out[p],
                    plane_widths[p],
                    plane_heights[p],
                    mask,
                )?;
            }
        }
        _ => {
            return Err(Error::unsupported(
                "magicyuv encoder: 10-bit RGB / GBRAP / YUVA encoding not yet supported",
            ));
        }
    }
    Ok(out)
}

fn blit_plane(plane: &VideoPlane, dst: &mut [u8], width: usize, height: usize) -> Result<()> {
    let stride = plane.stride;
    if stride < width {
        return Err(Error::invalid(format!(
            "magicyuv encoder: plane stride {stride} < width {width}"
        )));
    }
    if plane.data.len() < stride * height {
        return Err(Error::invalid(format!(
            "magicyuv encoder: plane too small ({} < {})",
            plane.data.len(),
            stride * height
        )));
    }
    for y in 0..height {
        let off = y * stride;
        dst[y * width..(y + 1) * width].copy_from_slice(&plane.data[off..off + width]);
    }
    Ok(())
}

fn blit_plane_u16(
    plane: &VideoPlane,
    dst: &mut [u16],
    width: usize,
    height: usize,
    mask: u16,
) -> Result<()> {
    // 16-bit planes are stored as u8 little-endian pairs.
    let need_bytes = width * 2;
    if plane.stride < need_bytes {
        return Err(Error::invalid(format!(
            "magicyuv encoder: 16-bit plane stride {} < {need_bytes}",
            plane.stride
        )));
    }
    if plane.data.len() < plane.stride * height {
        return Err(Error::invalid(
            "magicyuv encoder: 16-bit plane data too small",
        ));
    }
    for y in 0..height {
        let off = y * plane.stride;
        for x in 0..width {
            let lo = plane.data[off + x * 2] as u16;
            let hi = plane.data[off + x * 2 + 1] as u16;
            dst[y * width + x] = (lo | (hi << 8)) & mask;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::decode_packet;
    use oxideav_core::frame::VideoPlane;

    fn frame_yuv422p_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let cw = w / 2;
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * h];
        let mut v = vec![0u8; cw * h];
        for i in 0..(w * h) {
            y[i] = ((seed.wrapping_mul(31).wrapping_add(i as u32)) & 0xFF) as u8;
        }
        for i in 0..(cw * h) {
            u[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
            v[i] = ((seed.wrapping_mul(19).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane { stride: w, data: y },
                VideoPlane {
                    stride: cw,
                    data: u,
                },
                VideoPlane {
                    stride: cw,
                    data: v,
                },
            ],
        }
    }

    fn frame_yuv420p_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let cw = w / 2;
        let ch = h / 2;
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; cw * ch];
        let mut v = vec![0u8; cw * ch];
        for i in 0..(w * h) {
            y[i] = ((seed.wrapping_mul(31).wrapping_add(i as u32 * 7)) & 0xFF) as u8;
        }
        for i in 0..(cw * ch) {
            u[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
            v[i] = ((seed.wrapping_mul(19).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane { stride: w, data: y },
                VideoPlane {
                    stride: cw,
                    data: u,
                },
                VideoPlane {
                    stride: cw,
                    data: v,
                },
            ],
        }
    }

    fn frame_yuv444p_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let mut y = vec![0u8; w * h];
        let mut u = vec![0u8; w * h];
        let mut v = vec![0u8; w * h];
        for i in 0..(w * h) {
            y[i] = ((seed.wrapping_mul(11).wrapping_add(i as u32 * 7)) & 0xFF) as u8;
            u[i] = ((seed.wrapping_mul(13).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
            v[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane { stride: w, data: y },
                VideoPlane { stride: w, data: u },
                VideoPlane { stride: w, data: v },
            ],
        }
    }

    fn frame_gray_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let mut data = vec![0u8; w * h];
        for i in 0..data.len() {
            data[i] = ((seed.wrapping_mul(7).wrapping_add(i as u32 * 13)) & 0xFF) as u8;
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane { stride: w, data }],
        }
    }

    fn frame_rgb_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let mut data = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as u32;
                data[(y * w + x) * 3] = ((seed.wrapping_mul(11).wrapping_add(i * 31)) & 0xFF) as u8;
                data[(y * w + x) * 3 + 1] =
                    ((seed.wrapping_mul(13).wrapping_add(i * 17)) & 0xFF) as u8;
                data[(y * w + x) * 3 + 2] =
                    ((seed.wrapping_mul(17).wrapping_add(i * 19)) & 0xFF) as u8;
            }
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w * 3,
                data,
            }],
        }
    }

    fn frame_rgba_8(w: usize, h: usize, seed: u32) -> VideoFrame {
        let mut data = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as u32;
                let off = (y * w + x) * 4;
                data[off] = ((seed.wrapping_mul(11).wrapping_add(i * 31)) & 0xFF) as u8;
                data[off + 1] = ((seed.wrapping_mul(13).wrapping_add(i * 17)) & 0xFF) as u8;
                data[off + 2] = ((seed.wrapping_mul(17).wrapping_add(i * 19)) & 0xFF) as u8;
                data[off + 3] = ((seed.wrapping_mul(19).wrapping_add(i * 23)) & 0xFF) as u8;
            }
        }
        VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w * 4,
                data,
            }],
        }
    }

    fn assert_frames_eq(a: &VideoFrame, b: &VideoFrame) {
        assert_eq!(a.planes.len(), b.planes.len(), "plane count");
        for (i, (pa, pb)) in a.planes.iter().zip(b.planes.iter()).enumerate() {
            assert_eq!(pa.stride, pb.stride, "plane {i} stride");
            assert_eq!(pa.data, pb.data, "plane {i} data");
        }
    }

    /// Self-roundtrip every (format × predictor) on an 8-bit YUV422P
    /// fixture: encode, then re-decode through our own decoder, then
    /// compare planes byte-for-byte.
    #[test]
    fn yuv422p_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_yuv422p_8(64, 48, 0xC0FFEE);
            let bytes = encode_frame(FormatCode::Yuv422P, pred, 64, 48, 64, 48, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    #[test]
    fn yuv420p_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_yuv420p_8(64, 48, 0xBEAD);
            let bytes = encode_frame(FormatCode::Yuv420P, pred, 64, 48, 64, 48, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    #[test]
    fn yuv444p_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_yuv444p_8(48, 32, 0xABCD);
            let bytes = encode_frame(FormatCode::Yuv444P, pred, 48, 32, 48, 32, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    #[test]
    fn gray8_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_gray_8(64, 48, 0xF00D);
            let bytes = encode_frame(FormatCode::Gray8, pred, 64, 48, 64, 48, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    #[test]
    fn gbrp_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_rgb_8(48, 32, 0x1234);
            let bytes = encode_frame(FormatCode::Gbrp, pred, 48, 32, 48, 32, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    #[test]
    fn gbrap_self_roundtrip_all_predictors() {
        for &pred in &[Predictor::Left, Predictor::Gradient, Predictor::Median] {
            let f = frame_rgba_8(48, 32, 0x5678);
            let bytes = encode_frame(FormatCode::Gbrap, pred, 48, 32, 48, 32, &f).unwrap();
            let got = decode_packet(&bytes, None).unwrap();
            assert_frames_eq(&got, &f);
        }
    }

    /// Multi-slice encoding — vertical row bands.
    #[test]
    fn yuv422p_multi_slice_vertical() {
        let f = frame_yuv422p_8(64, 48, 0x9999);
        let bytes = encode_frame(FormatCode::Yuv422P, Predictor::Left, 64, 48, 64, 12, &f).unwrap();
        let got = decode_packet(&bytes, None).unwrap();
        assert_frames_eq(&got, &f);
    }

    /// Multi-slice encoding — horizontal columns × row bands.
    #[test]
    fn yuv422p_multi_slice_horizontal_grid() {
        let f = frame_yuv422p_8(64, 48, 0xAAAA);
        let bytes =
            encode_frame(FormatCode::Yuv422P, Predictor::Gradient, 64, 48, 32, 24, &f).unwrap();
        let got = decode_packet(&bytes, None).unwrap();
        assert_frames_eq(&got, &f);
    }

    /// 10-bit YUV422P self roundtrip.
    #[test]
    fn yuv422p10_self_roundtrip() {
        let w = 32usize;
        let h = 24usize;
        let cw = w / 2;
        let mask: u16 = 0x3FF;
        let mut y = vec![0u16; w * h];
        let mut u = vec![0u16; cw * h];
        let mut v = vec![0u16; cw * h];
        for i in 0..(w * h) {
            y[i] = ((i as u32 * 37 + 11) & mask as u32) as u16;
        }
        for i in 0..(cw * h) {
            u[i] = ((i as u32 * 41 + 17) & mask as u32) as u16;
            v[i] = ((i as u32 * 43 + 23) & mask as u32) as u16;
        }
        let to_le = |v: &[u16]| -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 2);
            for &s in v {
                out.push((s & 0xFF) as u8);
                out.push(((s >> 8) & 0xFF) as u8);
            }
            out
        };
        let f = VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane {
                    stride: w * 2,
                    data: to_le(&y),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: to_le(&u),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: to_le(&v),
                },
            ],
        };
        let bytes = encode_frame(
            FormatCode::Yuv422P10,
            Predictor::Median,
            w as u32,
            h as u32,
            w as u32,
            h as u32,
            &f,
        )
        .unwrap();
        let got = decode_packet(&bytes, None).unwrap();
        assert_frames_eq(&got, &f);
    }

    #[test]
    fn yuv420p10_self_roundtrip() {
        let w = 32usize;
        let h = 24usize;
        let cw = w / 2;
        let ch = h / 2;
        let mask: u16 = 0x3FF;
        let mut y = vec![0u16; w * h];
        let mut u = vec![0u16; cw * ch];
        let mut v = vec![0u16; cw * ch];
        for i in 0..(w * h) {
            y[i] = ((i as u32 * 47 + 5) & mask as u32) as u16;
        }
        for i in 0..(cw * ch) {
            u[i] = ((i as u32 * 53 + 9) & mask as u32) as u16;
            v[i] = ((i as u32 * 59 + 13) & mask as u32) as u16;
        }
        let to_le = |v: &[u16]| -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 2);
            for &s in v {
                out.push((s & 0xFF) as u8);
                out.push(((s >> 8) & 0xFF) as u8);
            }
            out
        };
        let f = VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane {
                    stride: w * 2,
                    data: to_le(&y),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: to_le(&u),
                },
                VideoPlane {
                    stride: cw * 2,
                    data: to_le(&v),
                },
            ],
        };
        let bytes = encode_frame(
            FormatCode::Yuv420P10,
            Predictor::Gradient,
            w as u32,
            h as u32,
            w as u32,
            h as u32,
            &f,
        )
        .unwrap();
        let got = decode_packet(&bytes, None).unwrap();
        assert_frames_eq(&got, &f);
    }

    #[test]
    fn gray10_self_roundtrip() {
        let w = 32usize;
        let h = 24usize;
        let mask: u16 = 0x3FF;
        let mut data = vec![0u16; w * h];
        for i in 0..data.len() {
            data[i] = ((i as u32 * 71 + 19) & mask as u32) as u16;
        }
        let to_le = |v: &[u16]| -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 2);
            for &s in v {
                out.push((s & 0xFF) as u8);
                out.push(((s >> 8) & 0xFF) as u8);
            }
            out
        };
        let f = VideoFrame {
            pts: Some(0),
            planes: vec![VideoPlane {
                stride: w * 2,
                data: to_le(&data),
            }],
        };
        let bytes = encode_frame(
            FormatCode::Gray10,
            Predictor::Left,
            w as u32,
            h as u32,
            w as u32,
            h as u32,
            &f,
        )
        .unwrap();
        let got = decode_packet(&bytes, None).unwrap();
        assert_frames_eq(&got, &f);
    }
}
