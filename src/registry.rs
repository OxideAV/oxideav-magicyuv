//! `oxideav-core` framework integration: codec registration plus the
//! [`oxideav_core::Decoder`] implementation wrapping the crate's
//! `decode_frame`.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) skip
//! this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder,
    Error as CoreError, Frame, MediaType, Packet, PixelFormat, Result as CoreResult,
    RuntimeContext, VideoFrame, VideoPlane,
};

use crate::decoder::{decode_frame, DecodedFrame, Samples};
use crate::tables::Family;

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
        .with_lossless(true)
        .with_intra_only(true);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder)
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
}
