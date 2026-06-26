//! `oxideav-core` framework integration: codec registration plus the
//! [`oxideav_core::Decoder`] implementation wrapping the crate's
//! `decode_frame`.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) skip
//! this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    parse_options, CodecCapabilities, CodecId, CodecInfo, CodecOptionsStruct, CodecParameters,
    CodecRegistry, CodecTag, Decoder, Encoder, Error as CoreError, Frame, MediaType, OptionField,
    OptionKind, OptionValue, Packet, PixelFormat, Result as CoreResult, RuntimeContext, TimeBase,
    VideoFrame, VideoPlane,
};

use crate::decoder::{decode_frame, DecodedFrame, Samples};
use crate::encoder::{
    encode_frame, output_params, EncodeOptions, PlaneInput, PredictorStrategy, SliceMode,
};
use crate::tables::{lookup, Family, FourccRecord, PredictorKind};

/// Canonical codec id. `oxideav-meta::register_all` calls
/// `crate::__oxideav_entry`, which delegates here.
pub const CODEC_ID_STR: &str = "magicyuv";

/// Register the MagicYUV codec with `reg`.
///
/// Claims the 17 native v7 FourCCs from `spec/01` §4.1 + the
/// `tables/00-fourcc-table.csv` ordering: 7 8-bit families
/// (`M8RG` / `M8RA` / `M8Y4` / `M8Y2` / `M8Y0` / `M8YA` / `M8G0`),
/// 6 10-bit (`M0RG` / `M0RA` / `M0Y4` / `M0Y2` / `M0Y0` / `M0G0`),
/// 2 12-bit (`M2RG` / `M2RA`), and 2 14-bit (`M4RG` / `M4RA`). These
/// declarations let `oxideav-avi`'s demuxer resolve a MagicYUV
/// stream's `biCompression` straight through `CodecResolver` without
/// a hand-maintained FourCC table on the container side.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("magicyuv_sw")
        .with_decode()
        .with_encode()
        .with_lossless(true)
        .with_intra_only(true);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
            .encoder(make_encoder)
            .encoder_options::<MagicYuvEncoderOptions>()
            .tags([
                // 8-bit families (spec/01 §4.1)
                CodecTag::fourcc(b"M8RG"),
                CodecTag::fourcc(b"M8RA"),
                CodecTag::fourcc(b"M8Y4"),
                CodecTag::fourcc(b"M8Y2"),
                CodecTag::fourcc(b"M8Y0"),
                CodecTag::fourcc(b"M8YA"),
                CodecTag::fourcc(b"M8G0"),
                // 10-bit families
                CodecTag::fourcc(b"M0RG"),
                CodecTag::fourcc(b"M0RA"),
                CodecTag::fourcc(b"M0Y4"),
                CodecTag::fourcc(b"M0Y2"),
                CodecTag::fourcc(b"M0Y0"),
                CodecTag::fourcc(b"M0G0"),
                // 12-bit families
                CodecTag::fourcc(b"M2RG"),
                CodecTag::fourcc(b"M2RA"),
                // 14-bit families
                CodecTag::fourcc(b"M4RG"),
                CodecTag::fourcc(b"M4RA"),
            ]),
    );
}

/// Unified entry point invoked by the macro-generated wrapper.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

// ──────────────────────── Decoder impl ────────────────────────

fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
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

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if self.pending.is_some() {
            return Err(CoreError::other(
                "oxideav-magicyuv: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(CoreError::Eof)
            } else {
                Err(CoreError::NeedMore)
            };
        };
        let frame = decode_frame(&pkt.data)
            .map_err(|e| CoreError::invalid(format!("oxideav-magicyuv: {e}")))?;
        Ok(Frame::Video(map_to_video_frame(frame, pkt.pts)))
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.eof = true;
        Ok(())
    }
}

fn samples_to_bytes(s: &Samples) -> Vec<u8> {
    match s {
        Samples::U8(v) => v.clone(),
        Samples::U16(v) => {
            // LE 16-bit container per spec/03 §7.3.
            let mut out = Vec::with_capacity(v.len() * 2);
            for &x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
            out
        }
    }
}

fn samples_byte_stride(plane_width: usize, samples: &Samples) -> usize {
    match samples {
        Samples::U8(_) => plane_width,
        Samples::U16(_) => plane_width * 2,
    }
}

fn map_to_video_frame(frame: DecodedFrame, pts: Option<i64>) -> VideoFrame {
    let _ = frame.record;
    let planes = match frame.record.family {
        Family::Gray => {
            let p = &frame.planes[0];
            vec![VideoPlane {
                stride: samples_byte_stride(p.width, &p.samples),
                data: samples_to_bytes(&p.samples),
            }]
        }
        Family::Yuv | Family::Yuva => frame
            .planes
            .iter()
            .map(|p| VideoPlane {
                stride: samples_byte_stride(p.width, &p.samples),
                data: samples_to_bytes(&p.samples),
            })
            .collect(),
        Family::Rgb => {
            // For 8-bit pack to interleaved RGB; for high-bit-depth,
            // emit per-plane GBR planes verbatim (LE-16-bit) so the
            // caller has the raw decoded values.
            if frame.planes[0].samples.is_u8() {
                let g = frame.planes[0].samples.as_u8().unwrap();
                let b = frame.planes[1].samples.as_u8().unwrap();
                let r = frame.planes[2].samples.as_u8().unwrap();
                let n = g.len();
                let mut out = Vec::with_capacity(n * 3);
                for i in 0..n {
                    out.push(r[i]);
                    out.push(g[i]);
                    out.push(b[i]);
                }
                vec![VideoPlane {
                    stride: frame.width as usize * 3,
                    data: out,
                }]
            } else {
                frame
                    .planes
                    .iter()
                    .map(|p| VideoPlane {
                        stride: samples_byte_stride(p.width, &p.samples),
                        data: samples_to_bytes(&p.samples),
                    })
                    .collect()
            }
        }
        Family::Rgba => {
            if frame.planes[0].samples.is_u8() {
                let g = frame.planes[0].samples.as_u8().unwrap();
                let b = frame.planes[1].samples.as_u8().unwrap();
                let r = frame.planes[2].samples.as_u8().unwrap();
                let a = frame.planes[3].samples.as_u8().unwrap();
                let n = g.len();
                let mut out = Vec::with_capacity(n * 4);
                for i in 0..n {
                    out.push(r[i]);
                    out.push(g[i]);
                    out.push(b[i]);
                    out.push(a[i]);
                }
                vec![VideoPlane {
                    stride: frame.width as usize * 4,
                    data: out,
                }]
            } else {
                frame
                    .planes
                    .iter()
                    .map(|p| VideoPlane {
                        stride: samples_byte_stride(p.width, &p.samples),
                        data: samples_to_bytes(&p.samples),
                    })
                    .collect()
            }
        }
    };

    let _ = MediaType::Video;
    let _ = PixelFormat::Yuv420P;
    VideoFrame { pts, planes }
}

// ──────────────────────── Encoder impl ────────────────────────

/// Build an encoder for a given parameter set.
///
/// The output FourCC is taken from `params.tag` (one of the 17 native
/// v7 codes — `spec/01` §4.1). When the tag is absent or not a native
/// MagicYUV FourCC the factory fails; muxers/pipelines that build an
/// encoder via `output_params()` always carry the tag, so the common
/// path is infallible. `width`/`height` come from `params` and pin the
/// per-plane geometry the [`Encoder::send_frame`] path validates each
/// frame against.
fn make_encoder(params: &CodecParameters) -> CoreResult<Box<dyn Encoder>> {
    let rec = fourcc_record_for_params(params)?;
    let width = params.width.ok_or_else(|| {
        CoreError::invalid("oxideav-magicyuv: encoder requires CodecParameters::width")
    })?;
    let height = params.height.ok_or_else(|| {
        CoreError::invalid("oxideav-magicyuv: encoder requires CodecParameters::height")
    })?;
    if width == 0 || height == 0 {
        return Err(CoreError::invalid(
            "oxideav-magicyuv: encoder requires non-zero width and height",
        ));
    }
    let opts: MagicYuvEncoderOptions = parse_options(&params.options)?;
    Ok(Box::new(MagicYuvEncoder {
        rec,
        width,
        height,
        out_params: output_params(rec, width, height),
        options: opts.to_encode_options(),
        pending: None,
        next_pts: 0,
    }))
}

/// Typed encoder options surfaced through `CodecParameters::options`
/// (`spec/04` predictor strategies + `spec/05` §6.2 slice modes).
///
/// Defaults match [`EncodeOptions::dynamic_auto`] — per-slice predictor
/// selection by minimum residual (`predictor = "dynamic"`) and per-slice
/// Huffman/raw byte-budget selection (`slice_mode = "auto"`),
/// progressive (`interlaced = false`). Every combination produces a
/// stream the decoder round-trips bit-exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagicYuvEncoderOptions {
    /// `"left"` / `"gradient"` / `"median"` (fixed predictor) or
    /// `"dynamic"` (per-slice minimum-residual selection, `spec/04` §3).
    pub predictor: String,
    /// `"huffman"` / `"raw"` / `"auto"` (per-slice byte-budget fallback,
    /// `spec/05` §6.2).
    pub slice_mode: String,
    /// Emit interlaced field-stride=2 prediction (`spec/04` §5.1).
    pub interlaced: bool,
}

impl Default for MagicYuvEncoderOptions {
    fn default() -> Self {
        Self {
            predictor: "dynamic".to_owned(),
            slice_mode: "auto".to_owned(),
            interlaced: false,
        }
    }
}

impl MagicYuvEncoderOptions {
    fn to_encode_options(&self) -> EncodeOptions {
        let strategy = match self.predictor.as_str() {
            "left" => PredictorStrategy::Fixed(PredictorKind::Left),
            "gradient" => PredictorStrategy::Fixed(PredictorKind::Gradient),
            "median" => PredictorStrategy::Fixed(PredictorKind::Median),
            // "dynamic" (the default) and any value the SCHEMA already
            // validated against the allow-list.
            _ => PredictorStrategy::Dynamic,
        };
        let mode = match self.slice_mode.as_str() {
            "huffman" => SliceMode::Huffman,
            "raw" => SliceMode::Raw,
            _ => SliceMode::Auto,
        };
        // Seed from dynamic_auto() (carries the spec-default color_matrix
        // / full_range knobs) then override the three configurable axes.
        let mut e = EncodeOptions::dynamic_auto();
        e.strategy = strategy;
        e.mode = mode;
        e.interlaced = self.interlaced;
        if let PredictorStrategy::Fixed(k) = strategy {
            e.predictor = k;
        }
        e
    }
}

impl CodecOptionsStruct for MagicYuvEncoderOptions {
    const SCHEMA: &'static [OptionField] = &[
        OptionField {
            name: "predictor",
            kind: OptionKind::Enum(&["left", "gradient", "median", "dynamic"]),
            default: OptionValue::String(String::new()),
            help: "per-slice predictor: left/gradient/median (fixed) or dynamic (min-residual)",
        },
        OptionField {
            name: "slice_mode",
            kind: OptionKind::Enum(&["huffman", "raw", "auto"]),
            default: OptionValue::String(String::new()),
            help: "per-slice entropy mode: huffman/raw (fixed) or auto (byte-budget fallback)",
        },
        OptionField {
            name: "interlaced",
            kind: OptionKind::Bool,
            default: OptionValue::Bool(false),
            help: "emit interlaced field-stride=2 prediction (spec/04 §5.1)",
        },
    ];

    fn apply(&mut self, key: &str, value: &OptionValue) -> CoreResult<()> {
        match key {
            "predictor" => self.predictor = value.as_str()?.to_owned(),
            "slice_mode" => self.slice_mode = value.as_str()?.to_owned(),
            "interlaced" => self.interlaced = value.as_bool()?,
            _ => unreachable!("guarded by SCHEMA"),
        }
        Ok(())
    }
}

/// Resolve the native-FourCC [`FourccRecord`] from a parameter set's
/// `tag`. Only the 17 native v7 FourCCs (`spec/01` §4.1) resolve; any
/// other tag (input-only `§4.2` FourCCs, `WaveFormat`, …) is rejected
/// — the encoder writes one of the native codes on the wire.
fn fourcc_record_for_params(params: &CodecParameters) -> CoreResult<FourccRecord> {
    let tag = params.tag.as_ref().ok_or_else(|| {
        CoreError::invalid(
            "oxideav-magicyuv: encoder requires a native FourCC in CodecParameters::tag",
        )
    })?;
    let CodecTag::Fourcc(raw) = tag else {
        return Err(CoreError::invalid(
            "oxideav-magicyuv: encoder tag must be a FourCC (one of the 17 native v7 codes)",
        ));
    };
    // CodecTag::fourcc upper-cases alphabetic bytes; the native codes
    // (`M8RG`, …) are already upper-case so a direct match succeeds.
    fourcc_table_lookup_by_fourcc(raw).ok_or_else(|| {
        CoreError::invalid(format!(
            "oxideav-magicyuv: FourCC {:?} is not a native MagicYUV v7 code",
            std::str::from_utf8(raw).unwrap_or("????"),
        ))
    })
}

/// Linear scan of the native FourCC table for a 4-byte code.
fn fourcc_table_lookup_by_fourcc(raw: &[u8; 4]) -> Option<FourccRecord> {
    for fb in 0x00u8..=0xFF {
        if let Some(rec) = lookup(fb) {
            if &rec.fourcc == raw {
                return Some(rec);
            }
        }
    }
    None
}

/// A frame-to-packet MagicYUV v7 encoder.
///
/// Consumes [`Frame::Video`]s in **planar** layout: one [`VideoPlane`]
/// per codec plane, in the family order [`encode_frame`] expects
/// (`G,B,R[,A]` for RGB; `Y,U,V[,A]` for YUV; single `Y` for Gray —
/// the exact order [`crate::decode_frame`] produces in
/// [`DecodedFrame::planes`]). This makes a registry-level
/// decode→re-encode lossless and is the natural inverse of the decoder.
///
/// High-bit-depth (10/12/14-bit) planes carry their samples as
/// little-endian `u16` bytes (`spec/03` §7.3); the encoder unpacks them
/// back to `u16` before encoding. 8-bit planes are one byte per sample.
struct MagicYuvEncoder {
    rec: FourccRecord,
    width: u32,
    height: u32,
    out_params: CodecParameters,
    options: EncodeOptions,
    pending: Option<Packet>,
    next_pts: i64,
}

impl MagicYuvEncoder {
    /// Per-plane element count for plane index `p` at this FOURCC's
    /// geometry (`spec/03` §8 chroma rounding — exact division because
    /// `encode_frame` rejects odd subsampled dimensions).
    fn plane_pixel_count(&self, plane_index: usize) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        // Plane 0 (luma / G) and any alpha plane are full-resolution;
        // the two chroma planes (indices 1,2 of a YUV family) are
        // subsampled. RGB / Gray / YUVA use (1,1) so every plane is
        // full-resolution.
        let is_chroma =
            matches!(self.rec.family, Family::Yuv) && (plane_index == 1 || plane_index == 2);
        if is_chroma {
            (w / self.rec.sub_x as usize) * (h / self.rec.sub_y as usize)
        } else {
            w * h
        }
    }

    /// Convert one [`VideoPlane`]'s bytes into a [`PlaneInput`] matching
    /// the FOURCC's container width (1 byte/sample at 8-bit, LE-`u16` at
    /// 10/12/14-bit per `spec/03` §7.3).
    fn plane_to_input(&self, plane_index: usize, plane: &VideoPlane) -> CoreResult<PlaneInput> {
        let want = self.plane_pixel_count(plane_index);
        if self.rec.is_8bit() {
            if plane.data.len() != want {
                return Err(CoreError::invalid(format!(
                    "oxideav-magicyuv: plane {plane_index} expected {want} bytes, got {}",
                    plane.data.len(),
                )));
            }
            Ok(PlaneInput::U8(plane.data.clone()))
        } else {
            if plane.data.len() != want * 2 {
                return Err(CoreError::invalid(format!(
                    "oxideav-magicyuv: high-bit-depth plane {plane_index} expected {} bytes (LE u16), got {}",
                    want * 2,
                    plane.data.len(),
                )));
            }
            let mut samples = Vec::with_capacity(want);
            for chunk in plane.data.chunks_exact(2) {
                samples.push(u16::from_le_bytes([chunk[0], chunk[1]]));
            }
            Ok(PlaneInput::U16(samples))
        }
    }

    /// De-interleave a single 8-bit RGB / RGBA [`VideoPlane`]
    /// (`R,G,B[,A]` byte order — the exact layout the registry decoder
    /// emits) into the planar `G,B,R[,A]` [`PlaneInput`]s the encoder
    /// consumes. Inverse of the interleaving in `map_to_video_frame`,
    /// closing the registry decode→re-encode loop for these families.
    fn deinterleave_rgb8(&self, plane: &VideoPlane) -> CoreResult<Vec<PlaneInput>> {
        let pixels = (self.width as usize) * (self.height as usize);
        let nch = self.rec.planes as usize; // 3 (RGB) or 4 (RGBA)
        if plane.data.len() != pixels * nch {
            return Err(CoreError::invalid(format!(
                "oxideav-magicyuv: interleaved {}-channel plane expected {} bytes, got {}",
                nch,
                pixels * nch,
                plane.data.len(),
            )));
        }
        let mut r = Vec::with_capacity(pixels);
        let mut g = Vec::with_capacity(pixels);
        let mut b = Vec::with_capacity(pixels);
        let mut a = Vec::with_capacity(pixels);
        for px in plane.data.chunks_exact(nch) {
            // Interleaved order is R,G,B[,A] (see map_to_video_frame).
            r.push(px[0]);
            g.push(px[1]);
            b.push(px[2]);
            if nch == 4 {
                a.push(px[3]);
            }
        }
        // Encoder plane order is G,B,R[,A] (spec/03 §4 user-facing order).
        let mut out = vec![PlaneInput::U8(g), PlaneInput::U8(b), PlaneInput::U8(r)];
        if nch == 4 {
            out.push(PlaneInput::U8(a));
        }
        Ok(out)
    }
}

impl Encoder for MagicYuvEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.out_params.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.out_params
    }

    fn send_frame(&mut self, frame: &Frame) -> CoreResult<()> {
        if self.pending.is_some() {
            return Err(CoreError::other(
                "oxideav-magicyuv: receive_packet must be called before sending another frame",
            ));
        }
        let Frame::Video(v) = frame else {
            return Err(CoreError::invalid(
                "oxideav-magicyuv: encoder accepts only video frames",
            ));
        };
        let want_planes = self.rec.planes as usize;
        // 8-bit RGB / RGBA may arrive as a single **interleaved** plane —
        // exactly the layout the registry decoder emits for these
        // families (R,G,B[,A] bytes). De-interleave it back to planar
        // G,B,R[,A] so a registry decode→re-encode round-trips. Planar
        // input (one plane per channel) is still accepted as-is.
        let interleaved = self.rec.is_8bit()
            && matches!(self.rec.family, Family::Rgb | Family::Rgba)
            && v.planes.len() == 1;
        let inputs = if interleaved {
            self.deinterleave_rgb8(&v.planes[0])?
        } else {
            if v.planes.len() != want_planes {
                return Err(CoreError::invalid(format!(
                    "oxideav-magicyuv: FourCC {:?} needs {want_planes} planar planes, got {}",
                    std::str::from_utf8(&self.rec.fourcc).unwrap_or("????"),
                    v.planes.len(),
                )));
            }
            let mut inputs: Vec<PlaneInput> = Vec::with_capacity(want_planes);
            for (i, plane) in v.planes.iter().enumerate() {
                inputs.push(self.plane_to_input(i, plane)?);
            }
            inputs
        };
        // slice_height = full image height → a single slice per plane.
        // The decoder reconstructs the same pixels regardless of the
        // slice partition; one slice keeps the wire bytes minimal and is
        // always valid (`spec/02` §4).
        let bytes = encode_frame(
            self.rec,
            self.width,
            self.height,
            self.height,
            inputs,
            self.options,
        )
        .map_err(|e| CoreError::invalid(format!("oxideav-magicyuv: encode failed: {e}")))?;
        let pts = v.pts.unwrap_or(self.next_pts);
        self.next_pts = pts + 1;
        // MagicYUV is intra-only (every frame is a keyframe).
        self.pending = Some(
            Packet::new(0, TimeBase::new(1, 30), bytes)
                .with_pts(pts)
                .with_keyframe(true),
        );
        Ok(())
    }

    fn receive_packet(&mut self) -> CoreResult<Packet> {
        self.pending.take().ok_or(CoreError::NeedMore)
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

// ──────────────────────── tests ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase};

    use crate::encoder::{encode_frame, EncodeOptions, PlaneInput};
    use crate::tables::{lookup_round1, PredictorKind};

    #[test]
    fn register_via_runtime_context_installs_codec() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let codec_id = CodecId::new(CODEC_ID_STR);
        assert!(
            ctx.codecs.has_decoder(&codec_id),
            "codec registration should install a decoder factory"
        );
    }

    #[test]
    fn register_claims_all_17_native_fourccs() {
        // spec/01 §4.1 + tables/00-fourcc-table.csv. After
        // registration the resolver must surface "magicyuv" for every
        // one of these — that's how `oxideav-avi`'s demuxer plumbs
        // FourCC → codec_id without its own codec table.
        use oxideav_core::ProbeContext;
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let fourccs: [&[u8; 4]; 17] = [
            b"M8RG", b"M8RA", b"M8Y4", b"M8Y2", b"M8Y0", b"M8YA", b"M8G0", b"M0RG", b"M0RA",
            b"M0Y4", b"M0Y2", b"M0Y0", b"M0G0", b"M2RG", b"M2RA", b"M4RG", b"M4RA",
        ];
        for fc in fourccs {
            let tag = CodecTag::fourcc(fc);
            let resolved = reg
                .resolve_tag_ref(&ProbeContext::new(&tag))
                .map(|c| c.as_str());
            assert_eq!(
                resolved,
                Some(CODEC_ID_STR),
                "FourCC {:?} did not resolve to magicyuv",
                std::str::from_utf8(fc).unwrap_or("????"),
            );
        }
        // Case-insensitive lookup also lands on magicyuv.
        let lower = CodecTag::fourcc(b"m8rg");
        assert_eq!(
            reg.resolve_tag_ref(&ProbeContext::new(&lower))
                .map(|c| c.as_str()),
            Some(CODEC_ID_STR),
        );
    }

    #[test]
    fn output_params_carry_configured_fourcc_tag() {
        // The encoder-side `output_params()` helper plumbs each FOURCC
        // straight into `CodecParameters::tag`. The 17 native v7
        // FourCCs (spec/01 §4.1) all round-trip through it, which is
        // what muxers consume to emit the right wire FourCC.
        use crate::encoder::output_params;
        use crate::tables::{lookup, lookup_round2};
        use oxideav_core::CodecTag;

        // 8-bit families.
        for &fb in &[0x65u8, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b] {
            let rec = lookup(fb).unwrap_or_else(|| panic!("missing record 0x{fb:02x}"));
            let p = output_params(rec, 64, 64);
            assert_eq!(
                p.tag,
                Some(CodecTag::fourcc(&rec.fourcc)),
                "FourCC {:?} (format byte 0x{:02x}) did not round-trip via output_params().tag",
                std::str::from_utf8(&rec.fourcc).unwrap_or("????"),
                fb,
            );
            assert_eq!(p.codec_id.as_str(), CODEC_ID_STR);
            assert_eq!(p.width, Some(64));
            assert_eq!(p.height, Some(64));
        }
        // High-bit-depth families. Use the round-2 lookup which
        // accepts every native FOURCC byte.
        for fb in 0x00..=0xFFu8 {
            if let Ok(rec) = lookup_round2(fb) {
                let p = output_params(rec, 32, 32);
                assert_eq!(
                    p.tag,
                    Some(CodecTag::fourcc(&rec.fourcc)),
                    "FourCC {:?} (format byte 0x{:02x}) did not round-trip via output_params().tag",
                    std::str::from_utf8(&rec.fourcc).unwrap_or("????"),
                    fb,
                );
            }
        }
    }

    #[test]
    fn output_params_tag_resolves_back_to_magicyuv_via_registry() {
        // Forward direction: the FourCC the encoder writes via
        // `output_params().tag` must resolve to "magicyuv" through
        // the registry's `resolve_tag` — closing the round-trip loop
        // that `oxideav-avi`'s muxer + demuxer rely on.
        use crate::encoder::output_params;
        use crate::tables::lookup;
        use oxideav_core::ProbeContext;

        let rec = lookup(0x66).expect("M8RA");
        let p = output_params(rec, 64, 64);
        let tag = p.tag.as_ref().expect("tag set by output_params");

        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let resolved = reg.resolve_tag_ref(&ProbeContext::new(tag));
        assert_eq!(
            resolved.map(|c| c.as_str()),
            Some(CODEC_ID_STR),
            "encoder's output FourCC must resolve back to magicyuv",
        );
    }

    #[test]
    fn end_to_end_decode_via_registry_m8g0() {
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
        let bytes = encode_frame(
            rec,
            16,
            16,
            28,
            vec![PlaneInput::U8(pixels.clone())],
            EncodeOptions::fixed(PredictorKind::Left),
        )
        .expect("encode");

        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let mut params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        params.media_type = MediaType::Video;
        let mut dec = ctx.codecs.first_decoder(&params).expect("first_decoder");
        let pkt = Packet::new(0, TimeBase::new(1, 30), bytes);
        dec.send_packet(&pkt).expect("send_packet");
        let frame = dec.receive_frame().expect("receive_frame");
        match frame {
            Frame::Video(v) => {
                assert_eq!(v.planes.len(), 1);
                assert_eq!(v.planes[0].data, pixels);
            }
            other => panic!("expected video frame, got {other:?}"),
        }
    }

    // ─────────────────── registry Encoder impl ───────────────────

    use crate::tables::{lookup, lookup_round2, Family};
    use oxideav_core::{ProbeContext, VideoFrame, VideoPlane};

    /// Build a `CodecParameters` carrying the native FourCC + geometry
    /// the encoder factory consumes.
    fn enc_params(rec: crate::tables::FourccRecord, w: u32, h: u32) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        p.media_type = MediaType::Video;
        p.tag = Some(CodecTag::fourcc(&rec.fourcc));
        p.width = Some(w);
        p.height = Some(h);
        p
    }

    /// Per-plane element count for a FOURCC at a given geometry.
    fn plane_counts(rec: crate::tables::FourccRecord, w: usize, h: usize) -> Vec<usize> {
        (0..rec.planes as usize)
            .map(|i| {
                let chroma = matches!(rec.family, Family::Yuv) && (i == 1 || i == 2);
                if chroma {
                    (w / rec.sub_x as usize) * (h / rec.sub_y as usize)
                } else {
                    w * h
                }
            })
            .collect()
    }

    /// Build a planar `VideoFrame` whose plane bytes are produced by a
    /// deterministic scrambler, matching the layout the encoder expects.
    fn make_video_frame(rec: crate::tables::FourccRecord, w: usize, h: usize) -> VideoFrame {
        let counts = plane_counts(rec, w, h);
        let mask = rec.sample_mask() as u64;
        let mut planes = Vec::with_capacity(counts.len());
        for (p, &n) in counts.iter().enumerate() {
            if rec.is_8bit() {
                let data: Vec<u8> = (0..n)
                    .map(|i| (((p as u64 * 131 + i as u64 * 17) & mask) & 0xff) as u8)
                    .collect();
                let stride = if p == 0
                    || matches!(
                        rec.family,
                        Family::Rgb | Family::Rgba | Family::Yuva | Family::Gray
                    ) {
                    w
                } else {
                    w / rec.sub_x as usize
                };
                planes.push(VideoPlane { stride, data });
            } else {
                let mut data = Vec::with_capacity(n * 2);
                for i in 0..n {
                    let v = ((p as u64 * 131 + i as u64 * 17) & mask) as u16;
                    data.extend_from_slice(&v.to_le_bytes());
                }
                let pw = if p == 0
                    || matches!(
                        rec.family,
                        Family::Rgb | Family::Rgba | Family::Yuva | Family::Gray
                    ) {
                    w
                } else {
                    w / rec.sub_x as usize
                };
                planes.push(VideoPlane {
                    stride: pw * 2,
                    data,
                });
            }
        }
        VideoFrame {
            pts: Some(7),
            planes,
        }
    }

    #[test]
    fn register_installs_encoder_factory() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert!(
            ctx.codecs.has_encoder(&CodecId::new(CODEC_ID_STR)),
            "encoder factory must be installed alongside the decoder",
        );
    }

    #[test]
    fn encoder_factory_rejects_missing_or_foreign_tag() {
        // No tag at all.
        let mut p = CodecParameters::video(CodecId::new(CODEC_ID_STR));
        p.width = Some(16);
        p.height = Some(16);
        assert!(make_encoder(&p).is_err(), "missing tag must fail");

        // A non-native FourCC (input-only YV12 from spec/01 §4.2).
        p.tag = Some(CodecTag::fourcc(b"YV12"));
        assert!(make_encoder(&p).is_err(), "foreign FourCC must fail");

        // A WaveFormat tag (wrong kind).
        p.tag = Some(CodecTag::wave_format(0x0001));
        assert!(make_encoder(&p).is_err(), "non-FourCC tag must fail");

        // Native tag but zero geometry.
        let rec = lookup(0x65).unwrap();
        let mut p2 = enc_params(rec, 0, 16);
        assert!(make_encoder(&p2).is_err(), "zero width must fail");
        p2.width = Some(16);
        p2.height = None;
        assert!(make_encoder(&p2).is_err(), "missing height must fail");
    }

    #[test]
    fn encoder_output_params_carry_fourcc_and_geometry() {
        let rec = lookup(0x66).unwrap(); // M8RA
        let params = enc_params(rec, 48, 32);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let enc = ctx.codecs.first_encoder(&params).expect("first_encoder");
        let out = enc.output_params();
        assert_eq!(out.codec_id.as_str(), CODEC_ID_STR);
        assert_eq!(out.tag, Some(CodecTag::fourcc(b"M8RA")));
        assert_eq!(out.width, Some(48));
        assert_eq!(out.height, Some(32));
        // The output FourCC resolves back to magicyuv via the registry.
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let tag = out.tag.as_ref().unwrap();
        assert_eq!(
            reg.resolve_tag_ref(&ProbeContext::new(tag))
                .map(|c| c.as_str()),
            Some(CODEC_ID_STR),
        );
    }

    #[test]
    fn encoder_rejects_wrong_plane_count_and_size() {
        let rec = lookup(0x67).unwrap(); // M8Y4 — 3 planes
        let params = enc_params(rec, 16, 16);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let mut enc = ctx.codecs.first_encoder(&params).expect("first_encoder");

        // Two planes for a 3-plane FourCC.
        let bad = VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane {
                    stride: 16,
                    data: vec![0u8; 256],
                },
                VideoPlane {
                    stride: 16,
                    data: vec![0u8; 256],
                },
            ],
        };
        assert!(
            enc.send_frame(&Frame::Video(bad)).is_err(),
            "plane count mismatch must fail"
        );

        // Right count, wrong size on plane 0.
        let bad2 = VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane {
                    stride: 16,
                    data: vec![0u8; 100],
                },
                VideoPlane {
                    stride: 16,
                    data: vec![0u8; 256],
                },
                VideoPlane {
                    stride: 16,
                    data: vec![0u8; 256],
                },
            ],
        };
        assert!(
            enc.send_frame(&Frame::Video(bad2)).is_err(),
            "plane size mismatch must fail"
        );

        // Non-video frame.
        use oxideav_core::AudioFrame;
        let af = AudioFrame {
            samples: 0,
            pts: None,
            data: vec![],
        };
        assert!(
            enc.send_frame(&Frame::Audio(af)).is_err(),
            "audio frame must fail"
        );
    }

    #[test]
    fn encoder_send_before_receive_errors() {
        let rec = lookup(0x6b).unwrap(); // M8G0
        let params = enc_params(rec, 8, 8);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let mut enc = ctx.codecs.first_encoder(&params).expect("first_encoder");
        let f = make_video_frame(rec, 8, 8);
        enc.send_frame(&Frame::Video(f.clone()))
            .expect("first send");
        // Second send before receive_packet must fail.
        assert!(enc.send_frame(&Frame::Video(f)).is_err());
        // receive_packet on empty (after draining) yields NeedMore.
        let _ = enc.receive_packet().expect("packet ready");
        assert!(matches!(enc.receive_packet(), Err(CoreError::NeedMore)));
    }

    /// The headline milestone: the framework-level `Encoder` produces a
    /// packet the framework-level `Decoder` reconstructs bit-exact, for
    /// every native FourCC across every family / bit-depth / subsampling.
    #[test]
    fn registry_encode_decode_round_trip_all_fourccs() {
        // 16×16 divides every native subsampling factor (spec/03 §8.2).
        let (w, h) = (16usize, 16usize);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);

        for fb in 0x00u8..=0xFF {
            let Ok(rec) = lookup_round2(fb) else { continue };
            let src = make_video_frame(rec, w, h);

            // Encode through the framework Encoder.
            let params = enc_params(rec, w as u32, h as u32);
            let mut enc = ctx.codecs.first_encoder(&params).expect("first_encoder");
            enc.send_frame(&Frame::Video(src.clone()))
                .expect("send_frame");
            let pkt = enc.receive_packet().expect("receive_packet");
            assert!(
                pkt.is_keyframe(),
                "{:?}: intra-only packet must be a keyframe",
                rec.fourcc
            );
            assert_eq!(
                pkt.pts,
                Some(7),
                "{:?}: pts must propagate from the frame",
                rec.fourcc
            );

            // The encoder's output FourCC must resolve to this codec.
            assert_eq!(enc.output_params().tag, Some(CodecTag::fourcc(&rec.fourcc)));

            // Decode through the framework Decoder.
            let mut dec_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
            dec_params.media_type = MediaType::Video;
            let mut dec = ctx
                .codecs
                .first_decoder(&dec_params)
                .expect("first_decoder");
            dec.send_packet(&pkt).expect("send_packet");
            let frame = dec.receive_frame().expect("receive_frame");
            let Frame::Video(out) = frame else {
                panic!("expected video frame")
            };

            // The decoder emits planar planes for YUV / Gray / HBD-RGB
            // and interleaved bytes for 8-bit RGB / RGBA; compare against
            // the source by re-deriving the expected output. For planar
            // families a direct per-plane compare is exact.
            match rec.family {
                Family::Gray | Family::Yuv | Family::Yuva => {
                    assert_eq!(out.planes.len(), src.planes.len(), "{:?}", rec.fourcc);
                    for (o, s) in out.planes.iter().zip(src.planes.iter()) {
                        assert_eq!(o.data, s.data, "{:?}: plane bytes differ", rec.fourcc);
                    }
                }
                Family::Rgb | Family::Rgba if !rec.is_8bit() => {
                    // HBD RGB emits per-plane GBR verbatim.
                    assert_eq!(out.planes.len(), src.planes.len(), "{:?}", rec.fourcc);
                    for (o, s) in out.planes.iter().zip(src.planes.iter()) {
                        assert_eq!(o.data, s.data, "{:?}: HBD GBR plane differ", rec.fourcc);
                    }
                }
                Family::Rgb => {
                    // 8-bit RGB → interleaved R,G,B. Rebuild expected.
                    let g = &src.planes[0].data;
                    let b = &src.planes[1].data;
                    let r = &src.planes[2].data;
                    let mut want = Vec::with_capacity(g.len() * 3);
                    for i in 0..g.len() {
                        want.push(r[i]);
                        want.push(g[i]);
                        want.push(b[i]);
                    }
                    assert_eq!(out.planes.len(), 1, "{:?}", rec.fourcc);
                    assert_eq!(
                        out.planes[0].data, want,
                        "{:?}: interleaved RGB",
                        rec.fourcc
                    );
                }
                Family::Rgba => {
                    let g = &src.planes[0].data;
                    let b = &src.planes[1].data;
                    let r = &src.planes[2].data;
                    let a = &src.planes[3].data;
                    let mut want = Vec::with_capacity(g.len() * 4);
                    for i in 0..g.len() {
                        want.push(r[i]);
                        want.push(g[i]);
                        want.push(b[i]);
                        want.push(a[i]);
                    }
                    assert_eq!(out.planes.len(), 1, "{:?}", rec.fourcc);
                    assert_eq!(
                        out.planes[0].data, want,
                        "{:?}: interleaved RGBA",
                        rec.fourcc
                    );
                }
            }
        }
    }

    // ─────────────────── typed encoder options ───────────────────

    use oxideav_core::CodecOptions;

    #[test]
    fn options_default_is_dynamic_auto_progressive() {
        let o = MagicYuvEncoderOptions::default();
        let e = o.to_encode_options();
        let baseline = EncodeOptions::dynamic_auto();
        assert_eq!(e.strategy, baseline.strategy);
        assert_eq!(e.mode, baseline.mode);
        assert!(!e.interlaced);
    }

    #[test]
    fn options_map_each_predictor_and_slice_mode() {
        use crate::encoder::{PredictorStrategy, SliceMode};
        let cases: &[(&str, PredictorStrategy)] = &[
            ("left", PredictorStrategy::Fixed(PredictorKind::Left)),
            (
                "gradient",
                PredictorStrategy::Fixed(PredictorKind::Gradient),
            ),
            ("median", PredictorStrategy::Fixed(PredictorKind::Median)),
            ("dynamic", PredictorStrategy::Dynamic),
        ];
        for (s, want) in cases {
            let o = MagicYuvEncoderOptions {
                predictor: (*s).to_owned(),
                ..Default::default()
            };
            assert_eq!(o.to_encode_options().strategy, *want, "predictor {s}");
        }
        let modes: &[(&str, SliceMode)] = &[
            ("huffman", SliceMode::Huffman),
            ("raw", SliceMode::Raw),
            ("auto", SliceMode::Auto),
        ];
        for (s, want) in modes {
            let o = MagicYuvEncoderOptions {
                slice_mode: (*s).to_owned(),
                ..Default::default()
            };
            assert_eq!(o.to_encode_options().mode, *want, "slice_mode {s}");
        }
    }

    #[test]
    fn options_schema_rejects_unknown_key_and_bad_value() {
        // Unknown key.
        let bag = CodecOptions::new().set("nonsense", "1");
        assert!(parse_options::<MagicYuvEncoderOptions>(&bag).is_err());
        // Bad enum value.
        let bag = CodecOptions::new().set("predictor", "paeth");
        assert!(parse_options::<MagicYuvEncoderOptions>(&bag).is_err());
        let bag = CodecOptions::new().set("slice_mode", "arith");
        assert!(parse_options::<MagicYuvEncoderOptions>(&bag).is_err());
        // Valid combination parses.
        let bag = CodecOptions::new()
            .set("predictor", "median")
            .set("slice_mode", "raw")
            .set("interlaced", "true");
        let o = parse_options::<MagicYuvEncoderOptions>(&bag).expect("valid options");
        assert_eq!(o.predictor, "median");
        assert_eq!(o.slice_mode, "raw");
        assert!(o.interlaced);
    }

    #[test]
    fn registry_encoder_honours_options_across_modes() {
        // Drive the framework encoder with every (predictor, slice_mode,
        // interlaced) combination through CodecParameters::options and
        // confirm each still round-trips bit-exact through the decoder.
        // Use M8Y0 (4:2:0 8-bit, 3 planes) — interlaced exercises the
        // field-stride path on a subsampled family.
        let rec = lookup(0x69).unwrap(); // M8Y0
        let (w, h) = (16usize, 16usize);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        let src = make_video_frame(rec, w, h);

        for predictor in ["left", "gradient", "median", "dynamic"] {
            for slice_mode in ["huffman", "raw", "auto"] {
                for interlaced in ["false", "true"] {
                    let mut params = enc_params(rec, w as u32, h as u32);
                    params.options = CodecOptions::new()
                        .set("predictor", predictor)
                        .set("slice_mode", slice_mode)
                        .set("interlaced", interlaced);
                    let mut enc = ctx.codecs.first_encoder(&params).expect("first_encoder");
                    enc.send_frame(&Frame::Video(src.clone()))
                        .unwrap_or_else(|e| panic!("{predictor}/{slice_mode}/{interlaced}: {e}"));
                    let pkt = enc.receive_packet().expect("receive_packet");

                    let mut dec_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
                    dec_params.media_type = MediaType::Video;
                    let mut dec = ctx
                        .codecs
                        .first_decoder(&dec_params)
                        .expect("first_decoder");
                    dec.send_packet(&pkt).expect("send_packet");
                    let Frame::Video(out) = dec.receive_frame().expect("receive_frame") else {
                        panic!("expected video");
                    };
                    assert_eq!(out.planes.len(), src.planes.len());
                    for (o, s) in out.planes.iter().zip(src.planes.iter()) {
                        assert_eq!(
                            o.data, s.data,
                            "{predictor}/{slice_mode}/{interlaced}: plane bytes differ",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn registry_exposes_encoder_options_schema() {
        let mut reg = CodecRegistry::new();
        register_codecs(&mut reg);
        let schema = reg
            .encoder_options_schema(&CodecId::new(CODEC_ID_STR))
            .expect("encoder options schema must be registered");
        let names: Vec<&str> = schema.iter().map(|f| f.name).collect();
        assert!(names.contains(&"predictor"));
        assert!(names.contains(&"slice_mode"));
        assert!(names.contains(&"interlaced"));
    }

    /// Close the loop for the 8-bit RGB / RGBA families: the registry
    /// decoder emits a single **interleaved** plane; feeding that exact
    /// plane straight back into the registry encoder (which now
    /// de-interleaves it) must reproduce the same interleaved output on a
    /// second decode — a full decode→re-encode→decode fixed point.
    #[test]
    fn registry_8bit_rgb_interleaved_reencode_round_trip() {
        let (w, h) = (16usize, 16usize);
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);

        for fb in [0x65u8, 0x66] {
            // M8RG (RGB), M8RA (RGBA)
            let rec = lookup(fb).unwrap();
            // Encode a source frame (planar G,B,R[,A]) once.
            let src = make_video_frame(rec, w, h);
            let params = enc_params(rec, w as u32, h as u32);
            let mut enc = ctx.codecs.first_encoder(&params).expect("encoder");
            enc.send_frame(&Frame::Video(src)).expect("send planar");
            let pkt0 = enc.receive_packet().expect("pkt0");

            // Decode → interleaved single plane.
            let mut dec_params = CodecParameters::video(CodecId::new(CODEC_ID_STR));
            dec_params.media_type = MediaType::Video;
            let mut dec = ctx.codecs.first_decoder(&dec_params).expect("dec");
            dec.send_packet(&pkt0).expect("send pkt0");
            let Frame::Video(interleaved) = dec.receive_frame().expect("decode0") else {
                panic!("video");
            };
            assert_eq!(interleaved.planes.len(), 1, "8-bit RGB decodes interleaved");

            // Re-encode the interleaved plane directly (de-interleave path).
            let mut enc2 = ctx.codecs.first_encoder(&params).expect("encoder2");
            enc2.send_frame(&Frame::Video(interleaved.clone()))
                .expect("send interleaved");
            let pkt1 = enc2.receive_packet().expect("pkt1");

            // Decode again → must equal the first interleaved output.
            let mut dec2 = ctx.codecs.first_decoder(&dec_params).expect("dec2");
            dec2.send_packet(&pkt1).expect("send pkt1");
            let Frame::Video(interleaved2) = dec2.receive_frame().expect("decode1") else {
                panic!("video");
            };
            assert_eq!(
                interleaved2.planes[0].data, interleaved.planes[0].data,
                "{:?}: interleaved re-encode is a fixed point",
                rec.fourcc,
            );
        }
    }
}
