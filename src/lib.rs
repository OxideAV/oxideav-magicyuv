//! Pure-Rust MagicYUV lossless video codec.
//!
//! **Round 0 — clean-room rebuild scaffold.** This is a fresh orphan
//! `master`; the previous implementation was retired alongside issue
//! [#3](https://github.com/OxideAV/oxideav-magicyuv/issues/3). The new
//! implementation is being built against the strict-isolation
//! clean-room workspace at
//! `https://github.com/OxideAV/docs/tree/master/video/magicyuv`. Until
//! the Implementer round lands, this crate exposes nothing beyond the
//! crate-local `Error` type below.
//!
//! See `README.md` for the rebuild scope, the v7 wire-format coverage
//! the spec in `docs/video/magicyuv/spec/00..06` claims, and the
//! Implementer's allow-list / forbidden-input list.

#![forbid(unsafe_code)]

/// Crate-local error type. Concrete variants are added as the
/// Implementer round populates each pipeline stage (file header,
/// slice table, predictor, Huffman entropy decode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Reserved placeholder. Will be replaced by real variants
    /// (InvalidHeader / Truncated / UnsupportedFourcc / HuffmanError /
    /// …) in the Implementer round.
    NotImplemented,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotImplemented => f.write_str(
                "oxideav-magicyuv: clean-room rebuild in progress — see crates/oxideav-magicyuv/README.md",
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Crate-local Result alias.
pub type Result<T> = core::result::Result<T, Error>;
