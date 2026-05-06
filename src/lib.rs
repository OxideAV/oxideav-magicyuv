//! Pure-Rust MagicYUV v7 lossless decoder.
//!
//! **Round 1 — clean-room rebuild.** The crate decodes the 8-bit
//! native FOURCC family (M8RG, M8RA, M8Y4, M8Y2, M8Y0, M8YA, M8G0)
//! per the strict-isolation clean-room workspace at
//! `docs/video/magicyuv/spec/00..06`. Higher bit-depth FOURCCs
//! (M0/M2/M4) are spec-feasible — `spec/03` covers the 2-byte sample
//! container — but the cleanroom Validator only certified 8-bit
//! byte-exactness, so round 1 stays in that lane and rejects them
//! with [`Error::UnsupportedFormatByte`].
//!
//! ## Pipeline
//!
//! 1. **Header parse** ([`header::parse`]): the 32-byte v7 frame
//!    header per [`spec/01` §3], with all five Auditor-round-1
//!    inline corrections honoured (`+0x1c` is `slice_height`, the
//!    audit-corrected `aux_byte` cross-check, …).
//! 2. **Slice table** (`decoder.rs`): the `(N+1)` u32 LE entries
//!    after the header per [`spec/02` §5].
//! 3. **Preamble**: plane count + per-slice plane-index bytes per
//!    [`spec/02` §7]; per-plane Huffman descriptors per
//!    [`spec/05` §1.1].
//! 4. **Per-plane Huffman** ([`huffman::HuffmanTable::build`]): the
//!    audit-corrected longest-length-first cumulative algorithm of
//!    [`spec/05` §2.0] — **NOT** RFC 1951.
//! 5. **Per-slice predictor** ([`predict::apply_u8`]): Left /
//!    Gradient / Median per [`spec/04` §4]. Median is the modular
//!    8-bit variant from the round-1 validation correction.
//! 6. **RGB inter-plane decorrelation reversal**: `spec/03` §4
//!    audit-corrected wire order is `(B', G, R')[, A]`; we recover
//!    `(G, B, R)[, A]` by inverting `B' = B - G; R' = R - G`.
//! 7. **AVI demux** ([`avi::AviReader`]): walks RIFF / `strf` /
//!    `00dc` chunks per [`spec/06`] for end-to-end file decode.
//!
//! ## Public API
//!
//! - [`decode_frame`] — decode one MAGY-prefixed frame's bytes into
//!   per-plane buffers.
//! - [`avi::AviReader`] — walk an AVI file and pull each frame out.
//! - [`Error`] / [`Result`] — crate-local error type.
//! - [`header::parse`] — standalone header parser (also re-used for
//!   AVI `strf` extradata, which is byte-identical per spec/06 §4.1).
//!
//! ## Cargo features
//!
//! - **`registry`** (default): wire the crate into `oxideav-core`'s
//!   codec registry. Disable for standalone builds that don't need
//!   the framework.
//!
//! [`spec/01` §3]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/01-file-header-and-fourccs.md
//! [`spec/02` §5]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/02-slice-table.md
//! [`spec/02` §7]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/02-slice-table.md
//! [`spec/04` §4]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/04-prediction-modes.md
//! [`spec/05` §1.1]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/05-entropy-coding.md
//! [`spec/05` §2.0]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/05-entropy-coding.md
//! [`spec/06`]: https://github.com/OxideAV/docs/blob/master/video/magicyuv/spec/06-avi-carriage.md

#![forbid(unsafe_code)]

pub mod avi;
pub mod bitreader;
pub mod decoder;
#[cfg(test)]
mod encoder;
pub mod error;
pub mod header;
pub mod huffman;
pub mod predict;
#[cfg(feature = "registry")]
pub mod registry;
#[cfg(test)]
mod roundtrip_tests;
pub mod tables;

pub use crate::decoder::{decode_frame, DecodedFrame, DecodedPlane};
pub use crate::error::{Error, Result};

// Framework integration — only when the `registry` feature is on.
#[cfg(feature = "registry")]
pub use crate::registry::{register, register_codecs, CODEC_ID_STR};

#[cfg(feature = "registry")]
oxideav_core::register!("oxideav-magicyuv", register);
