//! Pure-Rust **MagicYUV** lossless video decoder.
//!
//! MagicYUV is a closed-source proprietary codec by ignus / Pavel Zlatev
//! (`magicyuv.com`); the bitstream layout this crate implements is
//! reverse-engineered from a behavioural trace of FFmpeg's reference
//! decoder/encoder, captured under
//! `docs/video/magicyuv/magicyuv-trace-reverse-engineering.md`. No
//! third-party source was consulted: this is a clean-room implementation
//! built from the trace document alone.
//!
//! ## Supported (today)
//!
//! All seven 8-bit format codes the FFmpeg encoder emits:
//!
//! | Code   | FOURCC | Pixel layout (wire) | Output `PixelFormat`    |
//! |--------|--------|---------------------|-------------------------|
//! | `0x65` | `M8RG` | GBRP                | `Rgb24`                 |
//! | `0x66` | `M8RA` | GBRAP               | `Rgba`                  |
//! | `0x67` | `M8Y4` | YUV 4:4:4           | `Yuv444P`               |
//! | `0x68` | `M8Y2` | YUV 4:2:2           | `Yuv422P`               |
//! | `0x69` | `M8Y0` | YUV 4:2:0           | `Yuv420P`               |
//! | `0x6a` | `M8YA` | YUVA 4:4:4          | `Yuv444P` (alpha = plane 3) |
//! | `0x6b` | `M8G0` | GRAY8               | `Gray8`                 |
//!
//! All three spatial predictors (LEFT, GRADIENT, MEDIAN), the per-slice
//! raw-mode fallback (slice flag bit 0), implicit GBR↔RGB
//! decorrelation, and multi-slice frames.
//!
//! ## Not yet supported
//!
//! - **10/12/14-bit content** (format codes ≥ `0x6c`). The wire syntax is
//!   identical except for the per-symbol byte-pair widening; deferred
//!   pending a 10-bit fixture from the proprietary encoder.
//! - **Interlaced frames** (file-header `flags` bit 1). The decoder
//!   returns `Error::Unsupported` on encounter.
//! - **Horizontally-tiled slices** (`slice_width != width`). Matches
//!   FFmpeg's `AVERROR_PATCHWELCOME`.
//! - **Encoding.** This crate is decode-only; the trace document
//!   sketches encoder semantics but no encoder is implemented yet.
//! - **Versions other than 7.**

// Pixel-by-pixel index loops are idiomatic for predictor / pack-output
// inner kernels and read more clearly with explicit indices than with
// chained iterator adapters.
#![allow(clippy::needless_range_loop)]

pub mod bitstream;
pub mod decoder;
pub mod header;
pub mod huffman;
pub mod predictor;

use oxideav_core::{CodecCapabilities, CodecId, CodecTag, PixelFormat};
use oxideav_core::{CodecInfo, CodecRegistry};

/// Public codec id string.
pub const CODEC_ID_STR: &str = "magicyuv";

/// All seven AVI FOURCCs that MagicYUV's encoder writes today, one per
/// 8-bit pixel layout (trace doc §2). Higher-bit-depth codes (`M0…`,
/// `M2…`, etc.) are reserved by the `magicyuv.com` SDK but not emitted
/// by the FFmpeg encoder we used as a reference.
pub const MAGICYUV_FOURCCS: [&[u8; 4]; 7] = [
    b"M8RG", // GBRP
    b"M8RA", // GBRAP
    b"M8Y4", // YUV 4:4:4
    b"M8Y2", // YUV 4:2:2
    b"M8Y0", // YUV 4:2:0
    b"M8YA", // YUVA 4:4:4
    b"M8G0", // GRAY8
];

/// Returns `Some(CodecId::new("magicyuv"))` if `fourcc` (case-insensitive)
/// is one of the seven 8-bit MagicYUV FOURCCs.
pub fn codec_id_for_fourcc(fourcc: &[u8; 4]) -> Option<CodecId> {
    let mut upper = [0u8; 4];
    for i in 0..4 {
        upper[i] = fourcc[i].to_ascii_uppercase();
    }
    matches_known(&upper).then(|| CodecId::new(CODEC_ID_STR))
}

fn matches_known(fourcc: &[u8; 4]) -> bool {
    matches!(
        fourcc,
        b"M8RG" | b"M8RA" | b"M8Y4" | b"M8Y2" | b"M8Y0" | b"M8YA" | b"M8G0"
    )
}

/// Register the MagicYUV decoder for every supported FOURCC.
pub fn register(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("magicyuv_sw")
        .with_lossless(true)
        .with_intra_only(true)
        .with_pixel_format(PixelFormat::Yuv420P)
        .with_pixel_format(PixelFormat::Yuv422P)
        .with_pixel_format(PixelFormat::Yuv444P)
        .with_pixel_format(PixelFormat::Gray8)
        .with_pixel_format(PixelFormat::Rgb24)
        .with_pixel_format(PixelFormat::Rgba)
        .with_max_size(65535, 65535);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(decoder::make_decoder)
            .tags([
                CodecTag::fourcc(b"M8RG"),
                CodecTag::fourcc(b"M8RA"),
                CodecTag::fourcc(b"M8Y4"),
                CodecTag::fourcc(b"M8Y2"),
                CodecTag::fourcc(b"M8Y0"),
                CodecTag::fourcc(b"M8YA"),
                CodecTag::fourcc(b"M8G0"),
            ]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CodecRegistry;

    #[test]
    fn fourcc_round_trip() {
        for fc in MAGICYUV_FOURCCS {
            assert_eq!(
                codec_id_for_fourcc(fc),
                Some(CodecId::new(CODEC_ID_STR)),
                "fourcc {fc:?} should be recognised"
            );
        }
        assert_eq!(codec_id_for_fourcc(b"avc1"), None);
        assert_eq!(codec_id_for_fourcc(b"FFV1"), None);
    }

    #[test]
    fn register_advertises_decoder() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        assert!(reg.has_decoder(&CodecId::new(CODEC_ID_STR)));
    }
}
