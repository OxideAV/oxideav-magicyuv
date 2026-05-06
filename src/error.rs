//! Crate-local error type. Variants mirror the failure modes the
//! Implementer round (round 1) actually surfaces; new variants get
//! added as later rounds (encoder, AVI carriage, …) extend coverage.

use core::fmt;

/// Crate-local error type for the MagicYUV v7 decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Buffer was shorter than the v7 header's mandatory 32 bytes
    /// (`spec/01` §1) or shorter than the slice table / preamble it
    /// declares (`spec/02` §5).
    Truncated {
        /// Human-readable site of the truncation.
        what: &'static str,
        /// Number of bytes the parser asked for.
        needed: usize,
        /// Number of bytes left in the input.
        have: usize,
    },
    /// The 4-byte magic at offset 0 was not `'MAGY'` (`spec/01` §1).
    BadMagic,
    /// The `header_size` field at `+0x04` did not equal 0x20
    /// (`spec/01` §2). The v7 header is exactly 32 bytes.
    BadHeaderSize(u32),
    /// The `version` field at `+0x08` was greater than 7 (`spec/01` §2).
    BadVersion(u8),
    /// The `format_byte` at `+0x09` is outside the round-1 supported
    /// 8-bit native FOURCC set (`{0x65, 0x66, 0x67, 0x68, 0x69, 0x6a,
    /// 0x6b}` per `spec/01` §4.1). Higher-bit-depth formats are
    /// spec-feasible but deferred to a later round.
    UnsupportedFormatByte(u8),
    /// The `aux_byte` at `+0x0a` did not match the format-byte's
    /// expected `max_huffman_code_length` per `spec/01` §3.0
    /// audit-corrected note (12 for 8-bit, 14 for 10-bit, etc.). This
    /// is the same check the decoder performs at VMA `0x69bae3a8`.
    AuxByteMismatch {
        /// What we read from header `+0x0a`.
        got: u8,
        /// What `tables/00-fourcc-table.csv` says for the format byte.
        expected: u8,
    },
    /// Width or height was zero, or `slice_height` was zero (`spec/01`
    /// §3 + `spec/02` §3). All three are dimensions; zero would imply
    /// no pixels / no slices.
    ZeroDimension {
        /// Field name, e.g. "width" / "height" / "slice_height".
        what: &'static str,
    },
    /// Width / height exceeded the safety cap. Pure-Rust hardening
    /// against malicious headers — the cap is generous (`32 768`) and
    /// well above any v2.4.2 fixture.
    DimensionTooLarge {
        /// Field name.
        what: &'static str,
        /// Value the header claimed.
        got: u32,
    },
    /// `predictor_id` byte at slice +1 was not in `{0x01, 0x02, 0x03}`
    /// (`spec/04` §1.2).
    BadPredictorId(u8),
    /// Decoded slice payload was shorter than what the slice
    /// header / Huffman codestream demanded.
    SliceTruncated {
        /// Index of the offending slice within the frame.
        slice_index: usize,
    },
    /// Per-plane Huffman descriptor parsed an over-full code book
    /// (Σ 2^-L > 1 at some length tier) — the decoder rejection at
    /// `magicyuv.dll!0x69bb276c..0x69bb276f` (`spec/05` §2.0.3
    /// "Phase 4").
    HuffmanOverfull {
        /// 0-based plane index inside the frame (per spec/03 §4..§6
        /// canonical order).
        plane: usize,
    },
    /// Per-plane Huffman descriptor declared a code length above
    /// `max_length` for the plane's bit-depth tier (`spec/05` §1.1
    /// table). 8-bit caps at 12.
    HuffmanLengthExceedsMax {
        /// 0-based plane index.
        plane: usize,
        /// The over-the-cap length value.
        got: u8,
        /// The cap from `HuffCoderT<…>` (`spec/05` §1.1 / §1.5).
        max: u8,
    },
    /// Slice payload smaller than the 2-byte prefix every slice must
    /// carry (`spec/04` §1).
    SlicePrefixMissing {
        /// Slice index inside the frame.
        slice_index: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                what,
                needed,
                have,
            } => write!(
                f,
                "oxideav-magicyuv: truncated {what} (need {needed} bytes, have {have})"
            ),
            Self::BadMagic => f.write_str(
                "oxideav-magicyuv: bad MAGY magic at frame header offset 0 (spec/01 §1)",
            ),
            Self::BadHeaderSize(n) => write!(
                f,
                "oxideav-magicyuv: header_size {n:#x} != 0x20 — v7 header must be exactly 32 bytes"
            ),
            Self::BadVersion(v) => write!(
                f,
                "oxideav-magicyuv: version {v} > 7 — decoder accepts versions 0..=7 (spec/01 §2)"
            ),
            Self::UnsupportedFormatByte(b) => write!(
                f,
                "oxideav-magicyuv: format_byte {b:#04x} not in the round-1 supported 8-bit native set (spec/01 §4.1)"
            ),
            Self::AuxByteMismatch { got, expected } => write!(
                f,
                "oxideav-magicyuv: aux_byte {got:#04x} mismatches format-byte's expected {expected:#04x} (spec/01 §3.0 audit-corrected)"
            ),
            Self::ZeroDimension { what } => {
                write!(f, "oxideav-magicyuv: {what} field is zero")
            }
            Self::DimensionTooLarge { what, got } => write!(
                f,
                "oxideav-magicyuv: {what} {got} exceeds the implementation cap"
            ),
            Self::BadPredictorId(b) => write!(
                f,
                "oxideav-magicyuv: predictor_id {b:#04x} not in {{0x01, 0x02, 0x03}} (spec/04 §1.2)"
            ),
            Self::SliceTruncated { slice_index } => write!(
                f,
                "oxideav-magicyuv: slice {slice_index} payload truncated"
            ),
            Self::HuffmanOverfull { plane } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman descriptor over-full (spec/05 §2.0.3)"
            ),
            Self::HuffmanLengthExceedsMax { plane, got, max } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman length {got} > {max} (spec/05 §1.1)"
            ),
            Self::SlicePrefixMissing { slice_index } => write!(
                f,
                "oxideav-magicyuv: slice {slice_index} has no 2-byte prefix (spec/04 §1)"
            ),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
