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

use crate::decoder::{decode_frame, DecodedFrame};
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

fn map_to_video_frame(frame: DecodedFrame, pts: Option<i64>) -> VideoFrame {
    // Choose a PixelFormat for the frame. For Round-1 8-bit native
    // FOURCCs we map:
    //   M8RG → Rgb24 (interleaved); we re-pack from the GBR planes.
    //   M8RA → Rgba   (interleaved); re-pack from GBRA planes.
    //   M8Y4 → Yuv444P
    //   M8Y2 → Yuv422P
    //   M8Y0 → Yuv420P
    //   M8YA → Yuva420P? — there's no native YUVA-4:4:4:4 in core's
    //                       PixelFormat enum yet; round 1 emits the
    //                       four planes verbatim with PixelFormat
    //                       fallback to Yuv444P + a separate alpha
    //                       plane.
    //   M8G0 → Gray8.
    //
    // The mapping deliberately exposes user-facing data in the most
    // ergonomic core format. Callers needing the raw planar wire
    // bytes can still call `crate::decode_frame` directly.
    let _ = frame.record; // record consulted via family below
    let planes = match frame.record.family {
        Family::Gray => vec![VideoPlane {
            stride: frame.planes[0].width,
            data: frame.planes[0].data.clone(),
        }],
        Family::Yuv => frame
            .planes
            .iter()
            .map(|p| VideoPlane {
                stride: p.width,
                data: p.data.clone(),
            })
            .collect(),
        Family::Yuva => frame
            .planes
            .iter()
            .map(|p| VideoPlane {
                stride: p.width,
                data: p.data.clone(),
            })
            .collect(),
        Family::Rgb => {
            // Pack G,B,R planes into interleaved Rgb24 (R,G,B order).
            let g = &frame.planes[0].data;
            let b = &frame.planes[1].data;
            let r = &frame.planes[2].data;
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
        }
        Family::Rgba => {
            let g = &frame.planes[0].data;
            let b = &frame.planes[1].data;
            let r = &frame.planes[2].data;
            let a = &frame.planes[3].data;
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
        }
    };

    let _ = MediaType::Video; // marker — this is a video frame.
    let _ = PixelFormat::Yuv420P; // marker — concrete format set by stream params upstream.
    VideoFrame { pts, planes }
}

// ──────────────────────── tests ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, Packet, TimeBase};

    use crate::encoder::{encode_frame, SliceMode};
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
        // Build a tiny M8G0 (Gray, 8-bit) frame and decode it via the
        // registered framework decoder.
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
        let bytes = encode_frame(
            rec,
            16,
            16,
            28,
            vec![pixels.clone()],
            PredictorKind::Left,
            SliceMode::Huffman,
        );

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
