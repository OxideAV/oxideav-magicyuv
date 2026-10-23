//! Crate-local error type.

use core::fmt;

/// Crate-local error type for the MagicYUV v7 decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated {
        what: &'static str,
        needed: usize,
        have: usize,
    },
    BadMagic,
    BadHeaderSize(u32),
    BadVersion(u8),
    UnsupportedFormatByte(u8),
    AuxByteMismatch {
        got: u8,
        expected: u8,
    },
    ZeroDimension {
        what: &'static str,
    },
    DimensionTooLarge {
        what: &'static str,
        got: u32,
    },
    BadPredictorId(u8),
    SliceTruncated {
        slice_index: usize,
    },
    HuffmanOverfull {
        plane: usize,
    },
    HuffmanIncomplete {
        plane: usize,
    },
    HuffmanLengthExceedsMax {
        plane: usize,
        got: u8,
        max: u8,
    },
    SlicePrefixMissing {
        slice_index: usize,
    },
    OddDimensionForSubsampling {
        what: &'static str,
        got: u32,
        factor: u32,
    },
    SliceHeightNotDivisibleBySubsampling {
        slice_height: u32,
        factor: u32,
    },
    EncoderInputMismatch {
        plane: usize,
        expected: usize,
        got: usize,
    },
    BadPlaneIndex {
        slice_index: usize,
        got: usize,
        num_planes: usize,
    },
    TooManySlices {
        got: usize,
        max: usize,
    },
    BadSliceTable {
        slice_index: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { what, needed, have } => write!(
                f,
                "oxideav-magicyuv: truncated {what} (need {needed} bytes, have {have})"
            ),
            Self::BadMagic => f.write_str(
                "oxideav-magicyuv: bad MAGY magic at frame header offset 0",
            ),
            Self::BadHeaderSize(n) => write!(
                f,
                "oxideav-magicyuv: header_size {n:#x} != 0x20"
            ),
            Self::BadVersion(v) => write!(
                f,
                "oxideav-magicyuv: version {v} > 7"
            ),
            Self::UnsupportedFormatByte(b) => write!(
                f,
                "oxideav-magicyuv: format_byte {b:#04x} not in the supported native set"
            ),
            Self::AuxByteMismatch { got, expected } => write!(
                f,
                "oxideav-magicyuv: aux_byte {got:#04x} mismatches expected {expected:#04x}"
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
                "oxideav-magicyuv: predictor_id {b:#04x} not in {{0x01, 0x02, 0x03}}"
            ),
            Self::SliceTruncated { slice_index } => {
                write!(f, "oxideav-magicyuv: slice {slice_index} payload truncated")
            }
            Self::HuffmanOverfull { plane } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman descriptor over-full"
            ),
            Self::HuffmanIncomplete { plane } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman descriptor incomplete"
            ),
            Self::HuffmanLengthExceedsMax { plane, got, max } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman length {got} > {max}"
            ),
            Self::SlicePrefixMissing { slice_index } => write!(
                f,
                "oxideav-magicyuv: slice {slice_index} has no 2-byte prefix"
            ),
            Self::OddDimensionForSubsampling { what, got, factor } => write!(
                f,
                "oxideav-magicyuv: {what} {got} is not divisible by chroma factor {factor}"
            ),
            Self::SliceHeightNotDivisibleBySubsampling {
                slice_height,
                factor,
            } => write!(
                f,
                "oxideav-magicyuv: slice_height {slice_height} not divisible by chroma factor {factor}"
            ),
            Self::EncoderInputMismatch {
                plane,
                expected,
                got,
            } => write!(
                f,
                "oxideav-magicyuv: encoder plane {plane} length {got} != expected {expected}"
            ),
            Self::BadPlaneIndex {
                slice_index,
                got,
                num_planes,
            } => write!(
                f,
                "oxideav-magicyuv: slice {slice_index} plane index {got} invalid for {num_planes} planes"
            ),
            Self::TooManySlices { got, max } => write!(
                f,
                "oxideav-magicyuv: slice count {got} exceeds cap {max}"
            ),
            Self::BadSliceTable { slice_index } => write!(
                f,
                "oxideav-magicyuv: slice table entry {slice_index} is non-monotonic, wrapped, or points before preamble end"
            ),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
