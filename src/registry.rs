//! `oxideav-core` framework integration: codec registration plus the
//! [`oxideav_core::Decoder`] implementation wrapping the crate's
//! `decode_frame`.
//!
//! Compiled only when the default-on `registry` Cargo feature is
//! enabled. Standalone consumers (`default-features = false`) skip
//! this module entirely.

#![cfg(feature = "registry")]

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, Decoder,
    Error as CoreError, Frame, MediaType, Packet, PixelFormat, Result as CoreResult,
    RuntimeContext, VideoFrame, VideoPlane,
};

use crate::decoder::{decode_frame, DecodedFrame, Samples};
use crate::tables::Family;

/// Canonical codec id. `oxideav-meta::register_all` calls
/// `crate::__oxideav_entry`, which delegates here.
pub const CODEC_ID_STR: &str = "magicyuv";

/// Register the MagicYUV codec with `reg`.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("magicyuv_sw")
        .with_decode()
        .with_lossless(true)
        .with_intra_only(true);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder),
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

    use crate::encoder::{encode_frame, EncodeOptions, PlaneInput, SliceMode};
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
    fn end_to_end_decode_via_registry_m8g0() {
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
        let bytes = encode_frame(
            rec,
            16,
            16,
            28,
            vec![PlaneInput::U8(pixels.clone())],
            EncodeOptions {
                predictor: PredictorKind::Left,
                mode: SliceMode::Huffman,
                interlaced: false,
            },
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
