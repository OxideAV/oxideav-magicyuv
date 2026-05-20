//! Pure-Rust MagicYUV v7 lossless decoder + encoder.
//!
//! **Codec-only crate.** AVI is a container, not a codec, and its
//! demux + mux (including OpenDML 2.0 multi-RIFF) live in
//! `oxideav-avi`. This crate handles only the MAGY frame wire format.
//! It decodes the full native FOURCC set: 8-bit (M8RG, M8RA, M8Y4,
//! M8Y2, M8Y0, M8YA, M8G0) and 10/12/14-bit (M0RG, M0RA, M2RG, M2RA,
//! M4RG, M4RA, M0Y2, M0Y4, M0Y0, M0G0). Interlaced field-stride=2
//! prediction (`flags & 0x02`) is honoured per `spec/04` §5.1. The
//! public encoder API ([`encode_frame`]) produces wire-format frames
//! that round-trip through [`decode_frame`] byte-for-byte.
//!
//! ## Pipeline
//!
//! 1. **Header parse** ([`header::parse`]): the 32-byte v7 frame
//!    header per [`spec/01` §3].
//! 2. **Slice table** (`decoder.rs`): the `(N+1)` u32 LE entries
//!    after the header per [`spec/02` §5].
//! 3. **Preamble**: plane count + per-slice plane-index bytes per
//!    [`spec/02` §7]; per-plane Huffman descriptors per
//!    [`spec/05` §1.1].
//! 4. **Per-plane Huffman** ([`huffman::HuffmanTable::build`]): the
//!    audit-corrected longest-length-first cumulative algorithm of
//!    [`spec/05` §2.0] — **NOT** RFC 1951.
//! 5. **Per-slice predictor** ([`predict::apply_u8`] /
//!    [`predict::apply_u16`]): Left / Gradient / Median per
//!    [`spec/04` §4]. Median is **modular** at 8-bit and **standard
//!    JPEG-LS** at 10/12/14-bit.
//! 6. **RGB inter-plane decorrelation reversal**: `spec/03` §4
//!    audit-corrected wire order is `(B', G, R')[, A]`.
//!
//! ## Public API
//!
//! - [`decode_frame`] — decode one MAGY-prefixed frame's bytes into
//!   per-plane buffers.
//! - [`encode_frame`] — encode one frame from per-plane pixel buffers.
//! - [`Error`] / [`Result`] — crate-local error type.
//!
//! ## Cargo features
//!
//! - **`registry`** (default): wire the crate into `oxideav-core`'s
//!   codec registry.
//! - **`trace`**: emit JSON-Lines trace events to the path in
//!   `OXIDEAV_MAGICYUV_TRACE_FILE` during decode. Used by the
//!   round-2 Auditor's `jq`-line-diff lockstep harness.
//!
//! [`spec/01` §3]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/01-file-header-and-fourccs.md
//! [`spec/02` §5]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/02-slice-table.md
//! [`spec/02` §7]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/02-slice-table.md
//! [`spec/04` §4]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/04-prediction-modes.md
//! [`spec/05` §1.1]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/05-entropy-coding.md
//! [`spec/05` §2.0]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/05-entropy-coding.md

#![forbid(unsafe_code)]

pub mod bitreader;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod header;
pub mod huffman;
pub mod predict;
#[cfg(feature = "registry")]
pub mod registry;
#[cfg(test)]
mod roundtrip_tests;
pub mod tables;
#[cfg(feature = "trace")]
pub(crate) mod trace;

pub use crate::decoder::{decode_frame, DecodedFrame, DecodedPlane, Samples};
#[cfg(feature = "registry")]
pub use crate::encoder::output_params;
pub use crate::encoder::{encode_frame, EncodeOptions, PlaneInput, PredictorStrategy, SliceMode};
pub use crate::error::{Error, Result};

// Framework integration — only when the `registry` feature is on.
#[cfg(feature = "registry")]
pub use crate::registry::{register, register_codecs, CODEC_ID_STR};

#[cfg(feature = "registry")]
oxideav_core::register!("oxideav-magicyuv", register);
