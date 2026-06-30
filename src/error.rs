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
    /// The `format_byte` at `+0x09` is not one of the 17 native v7
    /// FOURCCs enumerated in `tables/00-fourcc-table.csv` (`spec/01`
    /// §4.1). The full 8-bit and 10/12/14-bit native set is supported;
    /// this error is for bytes outside that enumeration (reserved /
    /// VFW-negotiation-only IDs and corrupt headers).
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
    /// A per-plane Huffman slice's bitstream indexed an
    /// **unused-codespace** slot of an *under-full* descriptor
    /// (Σ 2⁻ᴸ < 1 — the assigned codes leave part of the codespace
    /// unused), or the descriptor was the degenerate all-unused book
    /// (`max_len == 0`). The decoder *accepts* under-full descriptors
    /// at table-build time (the encoder legitimately produces them for
    /// single-symbol planes, e.g. an all-zero-residual plane, and the
    /// vendor binary's constructor accepts them too) — a conformant
    /// stream only ever peeks the assigned codes' prefixes and never
    /// trips this. But a malformed slice can peek into the unused
    /// codespace, whose flat-table slot is the zero-init
    /// `(symbol 0, length 0)` entry: decoding it would consume no bits
    /// and silently mis-decode (`spec/05` §2.1 + §10 Q1). The decoder
    /// surfaces that as this error rather than emitting garbage,
    /// complementing the build-time [`Self::HuffmanOverfull`] guard.
    HuffmanIncomplete {
        /// 0-based plane index inside the frame.
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
    /// Header (decode) or requested (encode) dimensions don't divide
    /// evenly by the FOURCC's chroma subsampling factor (`spec/03`
    /// §8.2). The ceiling-vs-floor rounding rule for chroma planes at
    /// odd resolutions is an unverified open question, so both the
    /// decoder and [`crate::encode_frame`] conservatively reject the
    /// ambiguous case rather than silently flooring `dim / factor`.
    OddDimensionForSubsampling {
        /// `width` or `height`.
        what: &'static str,
        /// Header value.
        got: u32,
        /// Subsampling factor (2 for 4:2:x, 4:2:0).
        factor: u32,
    },
    /// `slice_height` does not divide cleanly by the chroma vertical
    /// subsampling factor on a subsampled YUV family (`spec/02` §6).
    ///
    /// The §6 chroma row-partition rule maps a chroma slice to rows
    /// `[s × slice_height / sub_y, (s + 1) × slice_height / sub_y)`
    /// of the chroma plane, reusing the **luma** slice count
    /// `slices_per_plane = ceil(height / slice_height)`. That tiling
    /// only covers the whole chroma plane when `sub_y` divides
    /// `slice_height` — otherwise `slices_per_plane × (slice_height /
    /// sub_y)` floors below the chroma height and the bottom chroma
    /// rows are never assigned to any slice (the decoder would emit a
    /// silently-truncated, partly-zero chroma plane). The v2.4.2
    /// encoder only ever writes `slice_height = 28`, which divides
    /// every native `sub_y ∈ {1, 2}` cleanly, so the spec leaves the
    /// indivisible case unverified. Both directions reject it — the
    /// same defensive posture as
    /// [`Self::OddDimensionForSubsampling`] — so the encoder never
    /// emits a stream it cannot itself round-trip and the decoder
    /// never silently drops chroma rows from a hostile header. For
    /// `sub_y == 1` (RGB / Gray / 4:4:4 / 4:2:2) the guard is inert.
    SliceHeightNotDivisibleBySubsampling {
        /// The `slice_height` header / argument value.
        slice_height: u32,
        /// The vertical subsampling factor (`sub_y`, 2 for 4:2:0).
        factor: u32,
    },
    /// The encoder API was called with a planes vector whose lengths
    /// don't match the per-FOURCC plane geometry.
    EncoderInputMismatch {
        plane: usize,
        expected: usize,
        got: usize,
    },
    /// A `per_slice_plane_index` byte in the preamble named a plane
    /// outside `[0, num_planes)`, or named one plane more times than
    /// the plane has slices (`spec/02` §7.3). The on-wire slice→plane
    /// mapping must address each plane exactly `slices_per_plane`
    /// times; the decoder honours an arbitrary (e.g. interleaved)
    /// ordering but rejects a mapping that over- or under-fills a
    /// plane's slice quota.
    BadPlaneIndex {
        /// Slice index inside the frame whose mapping byte was bad.
        slice_index: usize,
        /// The plane index the preamble named for this slice.
        got: usize,
        /// Number of planes the format byte implies.
        num_planes: usize,
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
            Self::HuffmanIncomplete { plane } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman descriptor incomplete / under-full (spec/05 §2.1)"
            ),
            Self::HuffmanLengthExceedsMax { plane, got, max } => write!(
                f,
                "oxideav-magicyuv: plane {plane} Huffman length {got} > {max} (spec/05 §1.1)"
            ),
            Self::SlicePrefixMissing { slice_index } => write!(
                f,
                "oxideav-magicyuv: slice {slice_index} has no 2-byte prefix (spec/04 §1)"
            ),
            Self::OddDimensionForSubsampling { what, got, factor } => write!(
                f,
                "oxideav-magicyuv: {what} {got} is not divisible by chroma subsampling factor {factor} (spec/03 §8.2)"
            ),
            Self::SliceHeightNotDivisibleBySubsampling {
                slice_height,
                factor,
            } => write!(
                f,
                "oxideav-magicyuv: slice_height {slice_height} is not divisible by chroma vertical subsampling factor {factor} — the spec/02 §6 chroma partition would drop the bottom chroma rows"
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
                "oxideav-magicyuv: slice {slice_index} per_slice_plane_index {got} invalid for {num_planes}-plane frame (spec/02 §7.3)"
            ),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
