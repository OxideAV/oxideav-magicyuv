//! Public MagicYUV v7 frame encoder.
//!
//! This is a **clean-room** encoder written from `spec/01..05`. Every
//! emitted frame is a well-formed v7 stream that round-trips through
//! [`crate::decode_frame`] byte-for-byte.
//!
//! Codec-only: AVI envelope/demux (single-RIFF AVI 1.0 + OpenDML 2.0
//! multi-RIFF per `spec/06`) lives in `oxideav-avi`, not here.
//!
//! Coverage:
//! - Header per spec/01 §3 (audit-corrected aux_byte values).
//! - Slice table + plane-major preamble per spec/02 §5..§7.
//! - Per-plane Huffman descriptors (run-length encoded) per spec/05
//!   §1.1, with codes assigned via the longest-length-first
//!   cumulative algorithm of spec/05 §2.0 — same algorithm the
//!   decoder uses, so the resulting (sym, code, length) triples are
//!   self-consistent.
//! - Per-slice predictor selection. Caller picks one of:
//!   - **Fixed** Left / Gradient / Median (spec/04 §1.2, §4).
//!   - **Dynamic** (spec/04 §3): the encoder evaluates all three
//!     predictors per slice, sums absolute (signed) residuals, and
//!     writes the minimiser into the slice's `predictor_id` byte. On
//!     the wire `predictor_id ∈ {0x01, 0x02, 0x03}` only; `0x04` never
//!     appears (spec/04 §3.1 and the v2.4.2 encoder dispatch evidence
//!     at `magicyuv.dll!0x69b96970..0x69b96ac9`).
//! - Per-slice Huffman / Raw mode selection. Caller picks one of:
//!   - **Huffman** (`slice_flags = 0x00`) — always Huffman.
//!   - **Raw** (`slice_flags = 0x01`) — always raw.
//!   - **Auto** (spec/05 §6.2): the encoder compares each slice's
//!     Huffman size to its raw size (`(pixels * bits + 7) / 8` per
//!     spec/05 §4.1) and picks the smaller, emitting `slice_flags`
//!     `0x00` or `0x01` per slice.
//! - Huffman OR raw-mode payloads. Raw mode at 8-bit is byte-per-
//!   sample; at 10/12/14-bit it is bit-packed at `bits` bits per
//!   sample (`spec/05` §4.1).
//! - RGB inter-plane decorrelation (B' = B - G, R' = R - G) when the
//!   family is RGB / RGBA per spec/03 §4 audit-corrected note.
//! - Interlaced field-stride=2 prediction per spec/04 §5.1.
//!
//! Out of scope: cross-plane optimisations and the proprietary
//! v2.4.2 encoder's exact byte-for-byte output (e.g. the residual-
//! evaluation cost function may differ in the byte count it rounds
//! against — the spec only fixes "minimum residual sum" as the
//! selection criterion, not which norm). What is guaranteed is
//! spec-conformance: Dynamic emits per-slice `predictor_id ∈ {1,2,3}`
//! by minimum residual; Auto emits per-slice `slice_flags ∈ {0,1}` by
//! smaller-payload comparison; both produce frames the decoder
//! round-trips byte-for-byte.

use crate::error::{Error, Result};
use crate::header::{
    FLAG_COLOR_MATRIX_SHIFT, FLAG_FULL_RANGE, FLAG_INTERLACED, HEADER_SIZE, MAGY_MAGIC,
};
use crate::predict::FieldStride;
use crate::tables::{Family, FourccRecord, PredictorKind};

/// Per-slice Huffman / raw mode the encoder offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SliceMode {
    /// Huffman-coded residuals (`slice_flags = 0x00`) for every slice.
    Huffman,
    /// Raw post-prediction residuals (`slice_flags = 0x01`) for every slice.
    Raw,
    /// **Auto** per-slice Huffman / raw selection (spec/05 §6.2).
    ///
    /// The encoder builds the per-plane Huffman table once (from the
    /// residual histogram across every slice of the plane), then for
    /// each slice independently chooses whichever of
    /// `(huffman_size, raw_size)` is smaller, writing the corresponding
    /// `slice_flags` byte (`0x00` or `0x01`). The raw size is
    /// `(slice_pixels * bits + 7) / 8` per spec/05 §4.1 (1 byte per
    /// sample at 8-bit; bit-packed at 10/12/14-bit). The proprietary
    /// v2.4.2 encoder uses the same per-slice fallback (its `Adaptive
    /// coding` toggle became always-on in v1.2 per spec/05 §6).
    Auto,
}

/// Per-slice predictor strategy the encoder offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PredictorStrategy {
    /// Use the named predictor on every slice
    /// (spec/04 §1.2 — fixed `predictor_id`).
    Fixed(PredictorKind),
    /// **Dynamic** per-slice predictor selection (spec/04 §3).
    ///
    /// For each slice the encoder evaluates all three predictors
    /// (Left, Gradient, Median), sums the residuals per the spec's
    /// "smallest residual sum" criterion (here: sum of absolute
    /// signed residuals, which is monotone with subsequent Huffman
    /// cost), and writes whichever predictor minimised the sum into
    /// that slice's `predictor_id` byte. On the wire,
    /// `predictor_id ∈ {0x01, 0x02, 0x03}` — `0x04` never appears
    /// (spec/04 §3.1, §7 open-question 5).
    Dynamic,
}

/// High-level encode options.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct EncodeOptions {
    /// Per-slice predictor strategy.
    ///
    /// Default: [`PredictorStrategy::Fixed`] [`PredictorKind::Gradient`]
    /// (the v2.4.2 encoder's `CompMethod=2` setting per spec/04 §3.3).
    pub strategy: PredictorStrategy,
    /// Per-slice Huffman / raw mode.
    pub mode: SliceMode,
    /// Set the header `flags & FLAG_INTERLACED` bit and emit
    /// field-stride=2 prediction per `spec/04` §5.1.
    pub interlaced: bool,
    /// 4-bit ColorMatrix nibble mirroring the v2.4.2 encoder's
    /// `ColorMatrix` registry value (`spec/01` §3.1 — context
    /// offset `+0x68`, OR-accumulated into the flags dword at bits
    /// 20..23 with mask `0x00f00000`). The encoder masks the low
    /// nibble before the shift, so the on-wire flags dword carries
    /// `(color_matrix & 0xf) << 20`. The spec's encoder OR-
    /// accumulator treats registry value `1` as the matrix-skip
    /// sentinel — the OR step is bypassed and flags bits 20..23
    /// stay clear; the matching default-`color_matrix = 1` value
    /// keeps a freshly-constructed `EncodeOptions::default()`
    /// emitting the same flags dword the r242-era encoder did,
    /// agreeing with the zero returned by
    /// [`crate::header::FrameHeader::color_matrix_nibble`] for the
    /// matrix-skip wire shape. The ColorMatrix nibble is
    /// informational at the lossless codec layer — the wire pixel
    /// bytes round-trip unchanged for every value 0..=15 — and is
    /// consumed by downstream colour-conversion (the GUI in
    /// `reference/vendor/changelog.md` v0.9.2-beta exposes Rec.601
    /// and Rec.709; the wire layout reserves 16 entries).
    pub color_matrix: u8,
    /// Set the header `flags & FLAG_FULL_RANGE` bit, mirroring the
    /// v2.4.2 encoder's `FullRangeYUV` registry value (`spec/01`
    /// §3.1 — context offset `+0x78`, OR-accumulated as bit 2 of
    /// the flags dword, mask `0x00000004`). The spec's encoder
    /// OR-accumulator at `magicyuv.dll!0x69b97647`–`0x69b9767a`
    /// reads the registry value and ORs `0x4` into the flags
    /// dword when non-zero; the decoder pickup at
    /// `magicyuv.dll!0x69bae311` (file `@0x2d311`) shifts the
    /// flags dword right by 2 and isolates the low bit, exposing
    /// the same boolean to the application/conversion layer. The
    /// codec layer's pixel residuals are independent of this
    /// signal — the wire pixel bytes returned by
    /// [`crate::decode_frame`] round-trip byte-exact regardless of
    /// the bit's value — so this field is strictly a header-level
    /// annotation routed to downstream YUV ↔ RGB conversion (the
    /// `is_full_range()` accessor on the parsed
    /// [`crate::header::FrameHeader`] recovers the same boolean).
    /// Pairs with [`Self::interlaced`] (bit 1) and
    /// [`Self::color_matrix`] (bits 20..23) — the three knobs ride
    /// the same flags dword without interference.
    ///
    /// **RGB-family override** (`spec/01` §3.1,
    /// `magicyuv.dll!0x69b9769c`–`0x69b976bb`): after the
    /// OR-accumulation, the encoder tests
    /// `1 << (format_byte - 0x67)` against the keep-mask
    /// `0xf1903f` (the YUV/Gray-family format bytes) and, for any
    /// format byte outside that set — every native RGB / RGBA
    /// FOURCC at every bit depth, with `0x65` / `0x66` reaching
    /// the override via the out-of-range fallthrough — clears
    /// flags bit 2 again. An authored `full_range = true` is
    /// therefore only observable on the wire for YUV / Gray
    /// FOURCCs; RGB-family headers always carry a clear bit 2,
    /// matching the v2.4.2 encoder byte-for-byte.
    pub full_range: bool,
    /// **Deprecated, kept for source compatibility.** When `strategy`
    /// is `Fixed(_)`, this field is ignored. Callers should set
    /// `strategy = PredictorStrategy::Fixed(predictor)` instead.
    pub predictor: PredictorKind,
}

impl EncodeOptions {
    /// Set `strategy`.
    #[must_use]
    pub fn with_strategy(mut self, strategy: PredictorStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Set `mode`.
    #[must_use]
    pub fn with_mode(mut self, mode: SliceMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set `interlaced`.
    #[must_use]
    pub fn with_interlaced(mut self, interlaced: bool) -> Self {
        self.interlaced = interlaced;
        self
    }

    /// Set `color_matrix`.
    #[must_use]
    pub fn with_color_matrix(mut self, color_matrix: u8) -> Self {
        self.color_matrix = color_matrix;
        self
    }

    /// Set `full_range`.
    #[must_use]
    pub fn with_full_range(mut self, full_range: bool) -> Self {
        self.full_range = full_range;
        self
    }

    /// Set `predictor`.
    #[must_use]
    pub fn with_predictor(mut self, predictor: PredictorKind) -> Self {
        self.predictor = predictor;
        self
    }
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Huffman,
            interlaced: false,
            color_matrix: 1,
            full_range: false,
            predictor: PredictorKind::Gradient,
        }
    }
}

impl EncodeOptions {
    /// Helper: build an options value driving the Dynamic predictor
    /// strategy + Auto Huffman/raw fallback — the spec/04 §3 + spec/05
    /// §6.2 always-on adaptive combination.
    pub fn dynamic_auto() -> Self {
        Self {
            strategy: PredictorStrategy::Dynamic,
            mode: SliceMode::Auto,
            interlaced: false,
            color_matrix: 1,
            full_range: false,
            predictor: PredictorKind::Gradient,
        }
    }

    /// Helper: shorthand for `Fixed(p)` with the given predictor.
    pub fn fixed(p: PredictorKind) -> Self {
        Self {
            strategy: PredictorStrategy::Fixed(p),
            mode: SliceMode::Huffman,
            interlaced: false,
            color_matrix: 1,
            full_range: false,
            predictor: p,
        }
    }
}

/// Encode a single MAGY v7 frame.
///
/// `planes` is given in the format-byte's family order (G/B/R[/A] for
/// RGB; Y/U/V[/A] for YUV; Y for Gray). Sample type matches the
/// FOURCC's bit-depth: [`PlaneInput::U8`] for 8-bit FOURCCs,
/// [`PlaneInput::U16`] for 10/12/14-bit FOURCCs (with values in the
/// low `bit_depth` bits — the encoder masks before encoding).
///
/// Per-plane geometry is derived from `(width, height,
/// rec.{family,sub_x,sub_y})`.  The caller's plane buffers MUST match
/// the per-plane element counts exactly.
pub fn encode_frame(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes: Vec<PlaneInput>,
    options: EncodeOptions,
) -> Result<Vec<u8>> {
    encode_frame_checked(rec, width, height, slice_height, planes, options, true)
}

/// [`encode_frame`] without the interlaced slice-length guard, so the
/// in-crate tests can still build the short interlaced slices the
/// decoder accepts (`spec/04` §5.1.1) but the encoder no longer writes.
#[cfg(test)]
pub(crate) fn encode_frame_any_geometry(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes: Vec<PlaneInput>,
    options: EncodeOptions,
) -> Result<Vec<u8>> {
    encode_frame_checked(rec, width, height, slice_height, planes, options, false)
}

fn encode_frame_checked(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes: Vec<PlaneInput>,
    options: EncodeOptions,
    guard_interlaced_slices: bool,
) -> Result<Vec<u8>> {
    for (what, v) in [
        ("width", width),
        ("height", height),
        ("slice_height", slice_height),
    ] {
        if v == 0 {
            return Err(Error::ZeroDimension { what });
        }
    }
    for (what, got) in [
        ("width", width),
        ("height", height),
        ("slice_height", slice_height),
    ] {
        if got > crate::header::MAX_DIMENSION {
            return Err(Error::DimensionTooLarge { what, got });
        }
    }
    // Interlaced: every slice of every plane needs its two field-first
    // rows (`spec/04` §5.1.1). A black-box decoder measures this on the
    // luma rows of a slice divided (rounding down) by the vertical
    // chroma factor, so a 4:2:0 slice needs four luma rows even where
    // the rounded-up chroma plane would have two rows left.
    if options.interlaced && guard_interlaced_slices {
        let sub_y = rec.sub_y.max(1) as u32;
        let slices = height.div_ceil(slice_height);
        let last_rows = height - (slices - 1) * slice_height;
        if (slices > 1 && slice_height / sub_y < 2) || last_rows / sub_y < 2 {
            return Err(Error::InterlacedSliceTooShort {
                slice_height,
                height,
            });
        }
    }
    // Reject a `slice_height` the §6 chroma partition can't tile (an
    // indivisible-by-`sub_y` value floors `slice_height / sub_y` and
    // drops the bottom chroma rows). Symmetric with the decoder guard;
    // inert for `sub_y == 1`.
    if rec.sub_y as u32 > 1 && !slice_height.is_multiple_of(rec.sub_y as u32) {
        return Err(Error::SliceHeightNotDivisibleBySubsampling {
            slice_height,
            factor: rec.sub_y as u32,
        });
    }
    if rec.is_high_bit_depth() {
        encode_frame_u16(rec, width, height, slice_height, planes, options)
    } else {
        encode_frame_u8(rec, width, height, slice_height, planes, options)
    }
}

/// Build the [`oxideav_core::CodecParameters`] this encoder produces for
/// a given (FOURCC, width, height) configuration.
///
/// Equivalent to what an `Encoder::output_params()` impl would return.
/// In particular, the returned params carry **`tag = Some(CodecTag::fourcc(rec.fourcc))`**
/// so muxers writing the encoder's packets emit the right FourCC on the wire
/// (one of the 17 native v7 codes — `M8RG`, `M8RA`, `M8Y4`, `M8Y2`, `M8Y0`,
/// `M8YA`, `M8G0` for 8-bit; `M0RG`, `M0RA`, `M0Y4`, `M0Y2`, `M0Y0`, `M0G0`
/// for 10-bit; `M2RG`, `M2RA` for 12-bit; `M4RG`, `M4RA` for 14-bit).
///
/// Compiled only when the default-on `registry` feature is enabled (the
/// `oxideav-core` framework integration). Standalone consumers
/// (`default-features = false`) build pure-Rust frame bytes via
/// [`encode_frame`] and don't need this helper.
#[cfg(feature = "registry")]
pub fn output_params(rec: FourccRecord, width: u32, height: u32) -> oxideav_core::CodecParameters {
    use oxideav_core::{CodecId, CodecParameters, CodecTag};
    let mut p = CodecParameters::video(CodecId::new(crate::registry::CODEC_ID_STR))
        .with_tag(CodecTag::fourcc(&rec.fourcc));
    p.width = Some(width);
    p.height = Some(height);
    p.pixel_format = pixel_format_for(rec);
    p
}

/// Map a native MagicYUV FOURCC to the `oxideav-core` [`PixelFormat`]
/// the registry decoder *emits* for it, when an exact variant exists.
///
/// Reflects the decoder's plane layout (see `registry::map_to_video_frame`):
/// 8-bit RGB / RGBA decode to **interleaved** `Rgb24` / `Rgba`; the
/// 10/12/14-bit RGB families decode to **planar GBR** (`Gbrp*Le` /
/// `Gbrap*Le`, `spec/03` §4 wire order); YUV / Gray families decode to
/// the matching planar `Yuv*P*` / `Gray*` formats.
///
/// Returns `None` for FOURCCs with no exact core variant — currently only
/// `M8YA` (YUVA 4:4:4:4 8-bit, no `Yuva444P` in core). Muxers that need a
/// label for these fall back to their own synthesis; the planes are still
/// carried verbatim.
#[cfg(feature = "registry")]
pub(crate) fn pixel_format_for(rec: FourccRecord) -> Option<oxideav_core::PixelFormat> {
    use oxideav_core::PixelFormat as Pf;
    let (sub_x, sub_y) = (rec.sub_x, rec.sub_y);
    Some(match (rec.family, rec.bit_depth) {
        (Family::Gray, 8) => Pf::Gray8,
        (Family::Gray, 10) => Pf::Gray10Le,
        (Family::Gray, 12) => Pf::Gray12Le,
        (Family::Gray, 14) => Pf::Gray16Le, // 14-bit gray has no exact variant; nearest 16-bit word
        (Family::Rgb, 8) => Pf::Rgb24,
        (Family::Rgba, 8) => Pf::Rgba,
        (Family::Rgb, 10) => Pf::Gbrp10Le,
        (Family::Rgba, 10) => Pf::Gbrap10Le,
        (Family::Rgb, 12) => Pf::Gbrp12Le,
        (Family::Rgba, 12) => Pf::Gbrap12Le,
        (Family::Rgb, 14) => Pf::Gbrp14Le,
        (Family::Rgba, 14) => Pf::Gbrap14Le,
        (Family::Yuv, 8) => match (sub_x, sub_y) {
            (1, 1) => Pf::Yuv444P,
            (2, 1) => Pf::Yuv422P,
            (2, 2) => Pf::Yuv420P,
            _ => return None,
        },
        (Family::Yuv, 10) => match (sub_x, sub_y) {
            (1, 1) => Pf::Yuv444P10Le,
            (2, 1) => Pf::Yuv422P10Le,
            (2, 2) => Pf::Yuv420P10Le,
            _ => return None,
        },
        (Family::Yuv, 12) => match (sub_x, sub_y) {
            (1, 1) => Pf::Yuv444P12Le,
            (2, 1) => Pf::Yuv422P12Le,
            (2, 2) => Pf::Yuv420P12Le,
            _ => return None,
        },
        // M8YA (YUVA 4:4:4:4 8-bit) — no exact core variant.
        _ => return None,
    })
}

/// Per-plane input buffer for the encoder.
#[derive(Debug, Clone)]
pub enum PlaneInput {
    /// 8-bit family.
    U8(Vec<u8>),
    /// 10/12/14-bit family (low `bit_depth` bits significant).
    U16(Vec<u16>),
}

impl PlaneInput {
    pub fn len(&self) -> usize {
        match self {
            PlaneInput::U8(v) => v.len(),
            PlaneInput::U16(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

const MAX_HUFF_LEN_8BIT: u8 = 12;
const MAX_HUFF_LEN_10BIT: u8 = 14;
const MAX_HUFF_LEN_12BIT: u8 = 16;
const MAX_HUFF_LEN_14BIT: u8 = 18;

fn max_huff_len_for(bit_depth: u8) -> u8 {
    match bit_depth {
        8 => MAX_HUFF_LEN_8BIT,
        10 => MAX_HUFF_LEN_10BIT,
        12 => MAX_HUFF_LEN_12BIT,
        14 => MAX_HUFF_LEN_14BIT,
        _ => MAX_HUFF_LEN_8BIT,
    }
}

/// Encoder-side per-plane Huffman.
struct PlaneHuff {
    lengths: Vec<u8>,
    /// Per-symbol prefix-free code. Kept on the struct so the
    /// `huffman_limit_tests::codes_are_prefix_free` audit can verify
    /// the on-wire decodability guarantee (spec/05 §2.0.3) against
    /// the production build path; the per-symbol encoder emit hot
    /// loop now fetches `packed` (below) instead so this field is
    /// not read on the production code path.
    #[cfg_attr(not(test), allow(dead_code))]
    codes: Vec<u32>,
    /// Per-symbol `(code, length)` packed into a single `u32`: low 8 bits
    /// hold `length`, high 24 bits hold `code`. The encoder's per-symbol
    /// emit hot loop fetches **one** u32 per residual symbol instead of
    /// two separate `lengths[sym]` + `codes[sym]` indexes — halving the
    /// table-fetch cost in the inner loop the way the decoder's
    /// `packed: Vec<u32>` primary table (BENCHMARKS opt-6) did for the
    /// decode side. Safe at every native bit-depth: `max_huff_len = 18`
    /// at 14-bit (`MAX_HUFF_LEN_14BIT`), so `code < 2^18 < 2^24` fits.
    /// Cheap to derive (one allocation + one pass) and amortised across
    /// every slice of the plane's encode loop.
    packed: Vec<u32>,
}

/// Pack `(code, length)` into the `PlaneHuff::packed` u32 layout: low 8
/// bits hold the length, high 24 bits hold the code. Caller must ensure
/// `code < 2^24` and `length ≤ 24`; in this crate both invariants hold
/// because `length ≤ MAX_HUFF_LEN_14BIT = 18 < 24` and
/// `code < 2^length ≤ 2^18 < 2^24`.
#[inline(always)]
const fn pack_huff_entry(code: u32, length: u8) -> u32 {
    (code << 8) | (length as u32)
}

impl PlaneHuff {
    fn build_from_histogram(hist: &[u32], max_len: u8) -> Self {
        let lengths = complete_code_lengths(hist, max_len);
        let n = lengths.len();
        let max_observed = *lengths.iter().max().unwrap_or(&0);
        let mut codes = vec![0u32; n];
        if max_observed > 0 {
            let mut bl_count = vec![0u32; (max_observed as usize) + 2];
            for &l in &lengths {
                bl_count[l as usize] += 1;
            }
            let mut start = vec![0u32; (max_observed as usize) + 2];
            let mut acc: u32 = 0xffff_ffff;
            for len in (1..=(max_observed as u32)).rev() {
                let n_at = bl_count[len as usize];
                start[len as usize] = acc.wrapping_add(1);
                acc = acc.wrapping_add(n_at);
                acc >>= 1;
            }
            let mut cur = start.clone();
            for (s, &l) in lengths.iter().enumerate() {
                if l == 0 {
                    continue;
                }
                let li = l as usize;
                codes[s] = cur[li];
                cur[li] = cur[li].wrapping_add(1);
            }
        }
        // Precompute the per-symbol packed `(code, length)` table for the
        // batched emit hot loop. One pass + one allocation; amortised
        // across every slice that emits this plane's residuals.
        let mut packed = Vec::with_capacity(n);
        for (s, &l) in lengths.iter().enumerate() {
            packed.push(pack_huff_entry(codes[s], l));
        }
        Self {
            lengths,
            codes,
            packed,
        }
    }
}

/// Optimal length-limited prefix-code lengths over the **whole**
/// alphabet: every symbol — including the ones the plane never uses —
/// receives a length in `1..=cap`, and the lengths satisfy the Kraft
/// equality `Σ 2^-L = 1` exactly.
///
/// This is the shape every descriptor in `spec/05` §1.2 / §1.3 has
/// (the all-zero plane's book is `1, 94 × 9, 8, 160 × 9`: 256 coded
/// symbols although one is used). A book that marks unused symbols
/// with length 0 is the `spec/05` §10 open question 1 territory, and a
/// black-box decoder refuses it outright, so the encoder never emits
/// one. Unused symbols carry weight 0 and therefore sink to the
/// longest lengths; with `cap = bit_depth + 4` they occupy at most
/// 1/16 of the codespace.
///
/// The lengths are computed with the package-merge construction
/// (coin-collector formulation): level `cap` holds the leaves sorted
/// by weight, every shallower level holds the leaves merged with the
/// pairwise packages of the level below, and the cheapest `2n - 2`
/// items of the top level are selected. A leaf's code length is the
/// number of levels on which it is selected. Only one flag per item
/// (leaf or package) is kept per level, so the cost is
/// `O(cap · n)` time and memory for any alphabet size.
fn complete_code_lengths(hist: &[u32], cap: u8) -> Vec<u8> {
    let n = hist.len();
    if n < 2 {
        return vec![1u8; n];
    }
    debug_assert!((cap as u32) < 32 && (1usize << cap) >= n);

    // Leaves in ascending weight order, ties by symbol index.
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by_key(|&s| (hist[s as usize], s));
    let leaf_w: Vec<u64> = order.iter().map(|&s| hist[s as usize] as u64).collect();

    // `is_package[k]` describes the merged list at depth `cap - k`.
    let mut is_package: Vec<Vec<bool>> = Vec::with_capacity(cap as usize);
    is_package.push(vec![false; n]);
    let mut prev: Vec<u64> = leaf_w.clone();
    for _ in 1..cap {
        let packages = prev.len() / 2;
        let mut merged: Vec<u64> = Vec::with_capacity(n + packages);
        let mut flags: Vec<bool> = Vec::with_capacity(n + packages);
        let (mut a, mut b) = (0usize, 0usize);
        while a < n || b < packages {
            let take_leaf = if b == packages {
                true
            } else {
                a < n && leaf_w[a] <= prev[2 * b] + prev[2 * b + 1]
            };
            if take_leaf {
                merged.push(leaf_w[a]);
                flags.push(false);
                a += 1;
            } else {
                merged.push(prev[2 * b] + prev[2 * b + 1]);
                flags.push(true);
                b += 1;
            }
        }
        prev = merged;
        is_package.push(flags);
    }

    // Walk back from the top level: of the `take` cheapest items, the
    // leaves each add one to their own length and every package
    // demands its two constituents on the level below.
    let mut bump = vec![0i32; n + 1];
    let mut take = 2 * n - 2;
    for flags in is_package.iter().rev() {
        let take_here = take.min(flags.len());
        let packages = flags[..take_here].iter().filter(|&&p| p).count();
        let leaves = take_here - packages;
        bump[0] += 1;
        bump[leaves] -= 1;
        take = 2 * packages;
        if take == 0 {
            break;
        }
    }
    let mut lengths = vec![0u8; n];
    let mut depth = 0i32;
    for (&sym, &d) in order.iter().zip(bump.iter()) {
        depth += d;
        lengths[sym as usize] = depth as u8;
    }
    lengths
}

/// Encode a sequence of code lengths using the run-length scheme of
/// `spec/05` §1.1.
///
/// The v2.4.2 encoder caps each emitted run at **255** repetitions —
/// count byte `0xfe` (254), one unit of headroom below the `0xff`
/// the decoder would also accept (`spec/05` §1.5: run-count cap
/// branch at `0x69b94600`, two-byte run-emit at `0x69b946d6`). The
/// reserved `0xff` count byte is never produced by the vendor, so a
/// length value that repeats `≥ 256` times is split into successive
/// `(0x80|v, 0xfe)` pairs (a run of 255) followed by the remainder
/// (`spec/05` §10 Q3: a 10-bit sparse distribution emits
/// `01 89 fe 89 fe …`, not a single `0xff`-counted run). Matching the
/// cap keeps the descriptor bytes byte-for-byte identical to the
/// vendor encoder, including for the high-bit-depth (10/12/14-bit,
/// N ∈ {1024, 4096, 16384}) plane descriptors where a single length
/// value commonly repeats thousands of times.
fn encode_descriptor(lengths: &[u8]) -> Vec<u8> {
    /// Maximum repetitions a single two-byte run may carry. The count
    /// byte is `run - 1`, capped at `0xfe` per `spec/05` §1.5.
    const MAX_RUN: usize = 255;
    let mut out = Vec::new();
    let mut i = 0;
    while i < lengths.len() {
        let v = lengths[i];
        let mut run = 1usize;
        while i + run < lengths.len() && lengths[i + run] == v && run < MAX_RUN {
            run += 1;
        }
        if run >= 2 {
            let count_byte = (run - 1) as u8;
            out.push(0x80 | v);
            out.push(count_byte);
            i += run;
        } else {
            out.push(v);
            i += 1;
        }
    }
    out
}

/// MSB-first bit writer (mirrors decoder's BitReader).
///
/// Round-3 perf: keeps a 64-bit accumulator with the next-to-emit
/// bits at the high end. Each `write(code, len)` shifts the code
/// into the empty low bits of the accumulator; whenever ≥ 8 bits
/// fill the high end we drain whole bytes (the typical Huffman
/// case at 8-bit gives `len ≤ 12`, so 1-2 bytes drain per call).
/// Same observable byte stream as the per-bit loop, but with the
/// inner `for i in (0..len).rev()` replaced by a couple of shifts.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Bit accumulator; valid bits are the high `bits_used` bits.
    acc: u64,
    bits_used: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BitWriter {
    /// Allocate the output buffer up front. Saves the geometric `Vec`
    /// reallocations on the slice-payload hot path, where the caller
    /// already knows a tight upper bound on the encoded byte count:
    ///
    /// - For 8-bit Auto / Huffman slice payloads the cap is `pixels`
    ///   bytes (Auto's Huffman branch fails over to raw the moment the
    ///   encoded size would exceed it; pure Huffman mode is bounded by
    ///   `(pixels * max_huff_len + 7) / 8` ≤ 1.5 * pixels at
    ///   max_huff_len = 12 for the 8-bit alphabet).
    /// - For 10/12/14-bit raw payloads the cap is exactly
    ///   `(pixels * bits + 7) / 8`.
    /// - For Auto-comparison Huffman buffers the same `pixels` /
    ///   `(pixels * bits + 7) / 8` ceiling applies.
    ///
    /// Slightly over-provisioning is harmless — the worst-case
    /// difference is a single allocator slop block which `Vec::shrink_to`
    /// could reclaim post-hoc but we don't because the resulting buffer
    /// is short-lived (assembled into the frame and dropped).
    pub(crate) fn with_capacity(byte_cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(byte_cap),
            acc: 0,
            bits_used: 0,
        }
    }
    /// Append `len` bits (low-aligned in `code`, MSB-first on the wire)
    /// to the bitstream. `len ≤ 32`. Caller must ensure
    /// `code < (1 << len)` (not asserted in release).
    #[inline(always)]
    pub(crate) fn write(&mut self, code: u32, len: u8) {
        if len == 0 {
            return;
        }
        debug_assert!(len <= 32);
        debug_assert!(self.bits_used <= 32);
        // Place `code` so its top bit lines up at acc[63 - bits_used].
        // Shift amount is `64 - bits_used - len`, which is in [0, 64].
        // (When `bits_used + len == 64` we fully fill the accumulator
        // and must drain immediately; a shift by 0 is the no-op case.)
        let shift = 64 - self.bits_used - (len as u32);
        self.acc |= (code as u64) << shift;
        self.bits_used += len as u32;
        // Drain whole bytes from the high end while we have ≥ 8.
        //
        // A r217 candidate replaced this byte-by-byte drain with a
        // single `acc.to_be_bytes()` + `extend_from_slice(&buf[..N])`
        // — the hypothesis was that one slice memcpy beats up to seven
        // `Vec::push`es per call. The measurement on the Apple M-series
        // host disagreed: a +6 to +9 % encode regression across every
        // `quick_bench encode` scenario (M8RG/M8Y0/M8G0/M0RG, 3-run
        // medians). LLVM already pipelines the `push(byte) + acc <<= 8
        // + bits_used -= 8` loop with the pre-allocated `Vec`'s
        // capacity check elided, so the `to_be_bytes` shape adds a
        // variable-length slice copy and a branch on `bytes_to_drain
        // == 8` that the scalar form already amortises. See the
        // "Round 217 closed candidates" section in `BENCHMARKS.md`.
        while self.bits_used >= 8 {
            let byte = (self.acc >> 56) as u8;
            self.bytes.push(byte);
            self.acc <<= 8;
            self.bits_used -= 8;
        }
    }
    pub(crate) fn finish(mut self) -> Vec<u8> {
        if self.bits_used > 0 {
            // `bits_used` is in [1, 7] here; the remaining bits are
            // the high `bits_used` bits of `acc`. Same MSB-first
            // wire convention as the per-bit code: shift down by 56
            // so they land in the low byte's top bits.
            let byte = (self.acc >> 56) as u8;
            self.bytes.push(byte);
        }
        self.bytes
    }
}

/// Batched packer for high-bit-depth raw-mode slice payloads
/// (`spec/05` §4.1: `(pixels * bits + 7) / 8` MSB-first bits packed
/// in the output). Appends `ceil(src.len() * bits / 8)` bytes to
/// `dst`. `bits` is taken at runtime; if it is 0, > 16, or `src` is
/// empty the buffer is left untouched.
///
/// Mirrors `bitreader::unpack_raw_bits_to_u16` (the decoder-side
/// counterpart added in the prior round): the same MSB-first 64-bit
/// accumulator + whole-byte drain shape used by `BitWriter::write`,
/// but with the state hoisted into stack locals across the whole
/// slice. The generic `BitWriter::write` path took `&mut self`, so
/// the optimiser cannot prove `self.bytes` / `self.acc` /
/// `self.bits_used` survive across the per-symbol call without a
/// reload — this entry point keeps all three in registers across the
/// inner loop. `len` here is the constant per-call `bits`, so the
/// drain loop's iteration count is bounded by `floor(bits / 8) + 1`
/// (≤ 2 at 10/12/14-bit) and easily unrolled by LLVM.
///
/// Same observable byte stream as a per-pixel
/// `BitWriter::write(sym as u32, bits)` loop; the slice-payload Vec
/// receives identical bytes byte-for-byte. Used by the encoder's
/// `SliceMode::Raw` (and Auto-loser raw-fallback) at 10/12/14-bit.
#[inline]
pub(crate) fn pack_raw_bits_from_u16(dst: &mut Vec<u8>, src: &[u16], bits: u8) {
    if bits == 0 || bits > 16 || src.is_empty() {
        return;
    }
    let nbits = bits as u32;
    let mask: u64 = if nbits == 64 {
        u64::MAX
    } else {
        (1u64 << nbits) - 1
    };
    // Reserve up front so the per-symbol drain can rely on
    // amortised-O(1) `push`.
    dst.reserve((src.len() * bits as usize).div_ceil(8));
    let mut acc: u64 = 0;
    let mut bits_used: u32 = 0;
    for &sym in src {
        // Same `BitWriter::write` arithmetic: place `sym` MSB-first
        // at acc[(63-bits_used) .. (64-bits_used-nbits)].
        let shift = 64 - bits_used - nbits;
        acc |= ((sym as u64) & mask) << shift;
        bits_used += nbits;
        // Drain whole bytes from the high end. With `nbits ≤ 16` and
        // entry `bits_used ≤ 7`, the post-merge `bits_used ≤ 23`, so
        // the loop runs ≤ 2 times.
        while bits_used >= 8 {
            dst.push((acc >> 56) as u8);
            acc <<= 8;
            bits_used -= 8;
        }
    }
    if bits_used > 0 {
        // Same partial-byte zero-pad as `BitWriter::finish`.
        dst.push((acc >> 56) as u8);
    }
}

/// Batched packer for the encoder's per-plane Huffman emit hot loop
/// (8-bit residuals). Mirrors `pack_raw_bits_from_u16` (and the
/// decoder's `decode_into_u8` batched hot loop, BENCHMARKS opt-2): the
/// 64-bit accumulator + whole-byte drain state lives in stack locals
/// across the whole slice, so the optimiser keeps `acc` / `bits_used`
/// / the `Vec::push` length in registers across every symbol — the
/// generic `BitWriter::write` path took `&mut self`, which forced a
/// reload of `self.bytes` / `self.acc` / `self.bits_used` after every
/// call and carried the `if len == 0 { return; }` early-return branch
/// at every call site.
///
/// Per-symbol table fetch is **one** u32 from `packed` (low 8 b length,
/// high 24 b code) instead of two separate `lengths[sym]` + `codes[sym]`
/// indexes — matches the encoder-side reshape of the decoder's
/// `Vec<u32>` packed primary table (opt-6).
///
/// Same observable byte stream as the per-pixel
/// `for &sym in src { bw.write(huff.codes[sym], huff.lengths[sym]); }`
/// loop the call sites previously walked. Pinned against that
/// reference by the `pack_huffman_residuals_tests` module + verified
/// end-to-end by the existing round-trip suite (every FOURCC ×
/// predictor × Huffman / Auto configuration).
#[inline]
pub(crate) fn pack_huffman_residuals_u8(dst: &mut Vec<u8>, src: &[u8], packed: &[u32]) {
    if src.is_empty() {
        return;
    }
    // Reserve a conservative upper bound: the per-symbol code length is
    // at most `MAX_HUFF_LEN_8BIT = 12`, so `(src.len() * 12 + 7) / 8`.
    // Falls short of the cap an over-bit-depth-prefixed `packed` could
    // request (e.g. if the caller passed a 10/12/14-bit table by
    // mistake), but the call sites already pre-size the `dst` Vec with
    // a tight slice-payload cap, so this reservation is a hint at most.
    dst.reserve((src.len() * 12).div_ceil(8));
    let mut acc: u64 = 0;
    let mut bits_used: u32 = 0;
    for &sym in src {
        // Single table fetch per symbol — splits the packed u32 into
        // `(code, length)` once. `length ≤ 18 ≤ 32` so the `BitWriter`
        // arithmetic below stays in bounds.
        let entry = packed[sym as usize];
        let length = (entry & 0xff) as u8;
        let code = entry >> 8;
        if length == 0 {
            // Same as `BitWriter::write`'s `if len == 0 { return; }`
            // early-return: an unused symbol carries no bits. Keeping
            // this branch here (rather than at the per-call-site loop
            // body) keeps the per-symbol cost paid once per *unused*
            // symbol the encoder hands us — which shouldn't happen
            // because the histogram is built from this very residual
            // stream, but the build path tolerates the all-unused
            // descriptor so we must too.
            continue;
        }
        // Same MSB-first placement as `BitWriter::write`. With
        // `length ≤ 12` (8-bit max code length) and entry-condition
        // `bits_used ≤ 7`, the post-merge `bits_used ≤ 19`, so the
        // drain loop runs ≤ 2 times.
        let shift = 64 - bits_used - (length as u32);
        acc |= (code as u64) << shift;
        bits_used += length as u32;
        while bits_used >= 8 {
            dst.push((acc >> 56) as u8);
            acc <<= 8;
            bits_used -= 8;
        }
    }
    if bits_used > 0 {
        // Same partial-byte zero-pad as `BitWriter::finish`.
        dst.push((acc >> 56) as u8);
    }
}

/// 10/12/14-bit analogue of `pack_huffman_residuals_u8`. Symbol index
/// type widens to `u16` (the residual alphabet is up to 16384 entries
/// at 14-bit), but the accumulator / drain logic is identical. Per-
/// symbol code length is at most `MAX_HUFF_LEN_14BIT = 18`, so the
/// post-merge `bits_used ≤ 25` and the drain loop still runs ≤ 3 times.
#[inline]
pub(crate) fn pack_huffman_residuals_u16(dst: &mut Vec<u8>, src: &[u16], packed: &[u32]) {
    if src.is_empty() {
        return;
    }
    // Reserve a conservative upper bound: `MAX_HUFF_LEN_14BIT = 18`,
    // so `(src.len() * 18 + 7) / 8`. As with the u8 variant this is a
    // hint — the call sites pre-size `dst` to `2 * raw_size + 2`.
    dst.reserve((src.len() * 18).div_ceil(8));
    let mut acc: u64 = 0;
    let mut bits_used: u32 = 0;
    for &sym in src {
        let entry = packed[sym as usize];
        let length = (entry & 0xff) as u8;
        let code = entry >> 8;
        if length == 0 {
            continue;
        }
        let shift = 64 - bits_used - (length as u32);
        acc |= (code as u64) << shift;
        bits_used += length as u32;
        while bits_used >= 8 {
            dst.push((acc >> 56) as u8);
            acc <<= 8;
            bits_used -= 8;
        }
    }
    if bits_used > 0 {
        dst.push((acc >> 56) as u8);
    }
}

// ─────────────────────────── 8-bit encode ───────────────────────────

fn encode_predictor_u8(
    kind: PredictorKind,
    data: &mut [u8],
    rows: usize,
    width: usize,
    field_stride: FieldStride,
) {
    if rows == 0 || width == 0 {
        return;
    }
    let fs = field_stride.0 as usize;
    let header_rows = fs.min(rows);
    // Round-3 perf: same row-pair `split_at_mut` shape as the
    // decoder's `predict::apply_u8_with_stride` — let LLVM elide the
    // bounds check on every `cur[c]` / `prev[c]` access. Iterate
    // bottom-up because each row's residual depends on the
    // already-still-pixel-valued previous row.
    match kind {
        PredictorKind::Left => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]);
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]);
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]);
                    }
                }
            }
        }
        PredictorKind::Gradient => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        let left = cur[c - 1];
                        let top = prev[c];
                        let top_left = prev[c - 1];
                        let pred = left.wrapping_add(top).wrapping_sub(top_left);
                        cur[c] = cur[c].wrapping_sub(pred);
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]);
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]);
                    }
                }
            }
        }
        PredictorKind::Median => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        let left = cur[c - 1];
                        let top = prev[c];
                        let top_left = prev[c - 1];
                        let gradient = left.wrapping_add(top).wrapping_sub(top_left);
                        let lo = left.min(top);
                        let hi = left.max(top);
                        let pred = if gradient < lo {
                            lo
                        } else if gradient > hi {
                            hi
                        } else {
                            gradient
                        };
                        cur[c] = cur[c].wrapping_sub(pred);
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]);
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]);
                    }
                }
            }
        }
    }
}

fn encode_frame_u8(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes_in: Vec<PlaneInput>,
    options: EncodeOptions,
) -> Result<Vec<u8>> {
    let num_planes = rec.planes as usize;
    if planes_in.len() != num_planes {
        return Err(Error::EncoderInputMismatch {
            plane: planes_in.len(),
            expected: num_planes,
            got: planes_in.len(),
        });
    }
    let w = width as usize;
    let h = height as usize;
    let sh = slice_height as usize;
    let slices_per_plane = h.div_ceil(sh);
    let total_slices = num_planes * slices_per_plane;
    let field_stride = if options.interlaced {
        FieldStride::INTERLACED
    } else {
        FieldStride::PROGRESSIVE
    };

    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(num_planes);
    for (i, p) in planes_in.into_iter().enumerate() {
        match p {
            PlaneInput::U8(v) => planes.push(v),
            PlaneInput::U16(_) => {
                return Err(Error::EncoderInputMismatch {
                    plane: i,
                    expected: 0,
                    got: 1,
                });
            }
        }
    }

    // Per-plane geometry.
    let plane_dims: Vec<(usize, usize, usize)> = (0..num_planes)
        .map(|p| plane_dims_for(rec, p, w, h, sh))
        .collect();
    for (p, (pw, ph, _)) in plane_dims.iter().enumerate() {
        let expected = pw * ph;
        if planes[p].len() != expected {
            return Err(Error::EncoderInputMismatch {
                plane: p,
                expected,
                got: planes[p].len(),
            });
        }
    }

    let mut wire_planes: Vec<Vec<u8>> = match rec.family {
        Family::Rgb | Family::Rgba => apply_rgb_decorrelation_u8(planes),
        _ => planes,
    };

    // Per-slice predictor decision and residual production. Under
    // `PredictorStrategy::Fixed(p)` every slice uses `p`; under
    // `PredictorStrategy::Dynamic` the encoder evaluates all three
    // predictors per slice and writes whichever produced the smallest
    // residual sum (per spec/04 §3.1 algorithm).
    //
    // `plane_resid[p]` holds the concatenated residuals for every slice
    // of plane `p` (in slice order), produced with that slice's chosen
    // predictor. `slice_predictors[s]` records the predictor that wrote
    // the residuals for global slice `s = plane * slices_per_plane +
    // in_plane_idx`.
    let mut plane_resid: Vec<Vec<u8>> = Vec::with_capacity(num_planes);
    let mut slice_predictors: Vec<PredictorKind> = vec![PredictorKind::Left; total_slices];
    let strategy = options.strategy;
    // Reusable per-plane trial scratch buffers for Dynamic strategy.
    // Capped at one slice's worth of bytes (max plane_slice_height *
    // pw across planes); reused across every slice and every kind so
    // the per-slice allocation count drops from 3 (Dynamic) / 1
    // (Fixed) Vec<u8> mallocs to 0 after the first slice of the
    // largest plane. The previous shape paid an
    // `slice_rows * pw`-byte malloc on every `src.to_vec()` call (3
    // per slice for Dynamic, 1 per slice for Fixed); see
    // `build_slice_residuals_u8_into` below.
    let mut trial_a: Vec<u8> = Vec::new();
    let mut trial_b: Vec<u8> = Vec::new();
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u8> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let src = &plane_data[row_start * pw..row_end * pw];
            let chosen = build_slice_residuals_u8_into(
                strategy,
                src,
                slice_rows,
                pw,
                field_stride,
                &mut residuals,
                &mut trial_a,
                &mut trial_b,
            );
            slice_predictors[p * slices_per_plane + s] = chosen;
        }
        plane_resid.push(residuals);
    }

    let max_len = max_huff_len_for(rec.bit_depth);
    let plane_huffs: Vec<PlaneHuff> = plane_resid
        .iter()
        .map(|res| {
            let mut hist = vec![0u32; 1 << rec.bit_depth];
            for &b in res {
                hist[b as usize] += 1;
            }
            if res.is_empty() {
                hist[0] = 1;
            }
            PlaneHuff::build_from_histogram(&hist, max_len)
        })
        .collect();

    let mut slice_payloads: Vec<Vec<u8>> = Vec::with_capacity(total_slices);
    for (s, &pred_kind) in slice_predictors.iter().enumerate() {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;
        let (pw, ph, plane_slice_height) = plane_dims[plane];
        let row_start = in_plane_idx * plane_slice_height;
        let row_end = ((in_plane_idx + 1) * plane_slice_height).min(ph);
        let _ = row_end;

        let res_block = &plane_resid[plane][row_start * pw..row_end * pw];
        let pred_id: u8 = predictor_id_byte(pred_kind);

        // Choose per-slice Huffman vs raw flags.
        let mut huff_buf: Option<Vec<u8>> = None;
        let raw_size = res_block.len(); // 1 byte per sample at 8-bit
        let flags: u8 = match options.mode {
            SliceMode::Huffman => 0x00,
            SliceMode::Raw => 0x01,
            SliceMode::Auto => {
                let huff = &plane_huffs[plane];
                // Pre-size to the raw-size upper bound: under Auto, any
                // Huffman encoding exceeding `raw_size` would lose to
                // raw, so the actual emitted byte count is ≤ `raw_size`
                // when Huffman wins. Add 1 for the partial-byte tail
                // the packer's final `dst.push((acc >> 56) as u8)`
                // flushes (`bits_used ∈ [1,7]` ⇒ one more byte).
                // Eliminates ~17 geometric reallocations per slice at
                // 1280×28 = 35840 input bytes.
                let mut bytes = Vec::with_capacity(raw_size + 1);
                pack_huffman_residuals_u8(&mut bytes, res_block, &huff.packed);
                if bytes.len() <= raw_size {
                    huff_buf = Some(bytes);
                    0x00
                } else {
                    0x01
                }
            }
        };

        // Pre-size the per-slice payload: flags + pred_id + body. The
        // body is `raw_size` for raw, `huff_buf.len()` (≤ raw_size + 1)
        // for cached-Auto, and bounded by the 8-bit max-Huffman-length
        // `(pixels * 12 + 7) / 8` for fresh-Huffman. Use `raw_size + 2`
        // as a tight upper bound for the common path; oversized cases
        // will just realloc once at the end.
        let mut payload = Vec::with_capacity(raw_size + 2);
        payload.push(flags);
        payload.push(pred_id);

        if flags & 0x01 != 0 {
            payload.extend_from_slice(res_block);
        } else if let Some(buf) = huff_buf {
            payload.extend(buf);
        } else {
            let huff = &plane_huffs[plane];
            // Pure-Huffman mode (SliceMode::Huffman): output bounded by
            // `(pixels * max_huff_len + 7) / 8` ≤ 1.5 * raw_size at the
            // 8-bit alphabet's `max_huff_len = 12`. Reserve
            // `raw_size + raw_size / 2 + 1` on the existing `payload`
            // Vec so the batched packer's `dst.push((acc >> 56) as u8)`
            // hot loop stays branchless on the capacity check;
            // pathological skewed slices will still grow gracefully.
            payload.reserve(raw_size + raw_size / 2 + 1);
            pack_huffman_residuals_u8(&mut payload, res_block, &huff.packed);
        }
        slice_payloads.push(payload);
    }

    Ok(assemble_frame(
        rec,
        width,
        height,
        slice_height,
        options.interlaced,
        options.color_matrix,
        options.full_range,
        total_slices,
        slices_per_plane,
        &plane_huffs,
        &slice_payloads,
    ))
}

fn predictor_id_byte(k: PredictorKind) -> u8 {
    match k {
        PredictorKind::Left => 0x01,
        PredictorKind::Gradient => 0x02,
        PredictorKind::Median => 0x03,
    }
}

/// Compute the residual block for one slice of a `u8` plane, picking
/// the predictor per `strategy`. Returns `(chosen_predictor, residuals)`.
///
/// For `Fixed(p)` we just apply `p`. For `Dynamic` we evaluate Left,
/// Gradient, and Median, sum the absolute (signed) residual byte
/// values, and pick the minimiser (ties broken by predictor-id
/// ascending — i.e. Left, then Gradient, then Median — matching
/// spec/04 §3.1's "lowest residual sum" criterion).
/// In-place residual-build with reusable scratch buffers.
///
/// Fixed: writes the slice's `src` copy directly into the tail of
/// `dst` (the per-plane residual accumulator, pre-sized to
/// `plane_w × plane_h` so the extend is allocation-free) and runs the
/// predictor in-place on that tail. No scratch use.
///
/// Dynamic: runs the three candidate predictors through `trial_a` /
/// `trial_b` (reused across every slice of every plane — alloc-free
/// after the first slice of the largest plane), tracks the lowest L1
/// score so far, and on completion copies the winning scratch into
/// `dst`'s tail with a single `extend_from_slice`. Net per-slice
/// allocation count is 0 (vs. 3 `src.to_vec()` mallocs in the prior
/// shape); per-slice copy count is 4 (3 `src→scratch` + 1
/// `scratch→dst`) — same as before (3 `src.to_vec()` + 1
/// `extend_from_slice`), but with the heap-pressure component
/// removed.
///
/// Tie-break is the same as the prior shape: predictor-id ascending
/// (Left, Gradient, Median) — `<` for "strictly better" preserves the
/// first-runner-up on ties.
#[allow(clippy::too_many_arguments)]
fn build_slice_residuals_u8_into(
    strategy: PredictorStrategy,
    src: &[u8],
    rows: usize,
    width: usize,
    field_stride: FieldStride,
    dst: &mut Vec<u8>,
    trial_a: &mut Vec<u8>,
    trial_b: &mut Vec<u8>,
) -> PredictorKind {
    match strategy {
        PredictorStrategy::Fixed(p) => {
            let tail_start = dst.len();
            dst.extend_from_slice(src);
            encode_predictor_u8(p, &mut dst[tail_start..], rows, width, field_stride);
            p
        }
        PredictorStrategy::Dynamic => {
            // Kind 1 (Left) → trial_a. Becomes "best so far".
            trial_a.clear();
            trial_a.extend_from_slice(src);
            encode_predictor_u8(
                PredictorKind::Left,
                &mut trial_a[..],
                rows,
                width,
                field_stride,
            );
            let mut best_kind = PredictorKind::Left;
            let mut best_in_a = true;
            let mut best_score = abs_signed_sum_u8(trial_a);

            // Kind 2 (Gradient) → trial_b.
            trial_b.clear();
            trial_b.extend_from_slice(src);
            encode_predictor_u8(
                PredictorKind::Gradient,
                &mut trial_b[..],
                rows,
                width,
                field_stride,
            );
            let score_b = abs_signed_sum_u8(trial_b);
            if score_b < best_score {
                best_score = score_b;
                best_kind = PredictorKind::Gradient;
                best_in_a = false;
            }

            // Kind 3 (Median) → the current loser's scratch so the
            // winner survives untouched.
            {
                let loser = if best_in_a {
                    &mut *trial_b
                } else {
                    &mut *trial_a
                };
                loser.clear();
                loser.extend_from_slice(src);
                encode_predictor_u8(
                    PredictorKind::Median,
                    &mut loser[..],
                    rows,
                    width,
                    field_stride,
                );
                let score_m = abs_signed_sum_u8(loser);
                if score_m < best_score {
                    best_kind = PredictorKind::Median;
                    best_in_a = !best_in_a;
                }
            }

            // Append the winning scratch into the residual accumulator.
            if best_in_a {
                dst.extend_from_slice(trial_a);
            } else {
                dst.extend_from_slice(trial_b);
            }
            best_kind
        }
    }
}

/// Sum of |signed| residuals interpreting each byte as `i8`. Equivalent
/// to `sum of min(b, 256 - b)` over the unsigned bytes. Spec/04 §3.1
/// fixes the selector as "smallest residual sum"; the L1 norm over the
/// signed residuals (rather than the raw bytes) is the only sum that
/// preserves the natural "near-zero residual is good" ordering.
fn abs_signed_sum_u8(block: &[u8]) -> u64 {
    let mut s: u64 = 0;
    for &b in block {
        let signed = b as i8;
        s += signed.unsigned_abs() as u64;
    }
    s
}

fn apply_rgb_decorrelation_u8(planes: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut iter = planes.into_iter();
    let g = iter.next().unwrap();
    let mut b = iter.next().unwrap();
    let mut r = iter.next().unwrap();
    let a_opt = iter.next();
    for (bv, &gv) in b.iter_mut().zip(g.iter()) {
        *bv = bv.wrapping_sub(gv);
    }
    for (rv, &gv) in r.iter_mut().zip(g.iter()) {
        *rv = rv.wrapping_sub(gv);
    }
    let mut out = vec![b, g, r];
    if let Some(a) = a_opt {
        out.push(a);
    }
    out
}

// ─────────────────────────── 16-bit encode ──────────────────────────

fn encode_predictor_u16(
    kind: PredictorKind,
    data: &mut [u16],
    rows: usize,
    width: usize,
    mask: u16,
    field_stride: FieldStride,
) {
    if rows == 0 || width == 0 {
        return;
    }
    let fs = field_stride.0 as usize;
    let header_rows = fs.min(rows);
    // See `encode_predictor_u8` for the row-pair split rationale.
    match kind {
        PredictorKind::Left => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]) & mask;
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]) & mask;
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]) & mask;
                    }
                }
            }
        }
        PredictorKind::Gradient => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        let left = cur[c - 1];
                        let top = prev[c];
                        let top_left = prev[c - 1];
                        let pred = left.wrapping_add(top).wrapping_sub(top_left);
                        cur[c] = cur[c].wrapping_sub(pred) & mask;
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]) & mask;
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]) & mask;
                    }
                }
            }
        }
        PredictorKind::Median => {
            for r in (0..rows).rev() {
                if r >= header_rows {
                    let (head, tail) = data.split_at_mut(r * width);
                    let prev = &head[(r - fs) * width..(r - fs) * width + width];
                    let cur = &mut tail[..width];
                    for c in (1..width).rev() {
                        let left = cur[c - 1];
                        let top = prev[c];
                        let top_left = prev[c - 1];
                        // Standard JPEG-LS Median (10/12/14-bit).
                        let lo = left.min(top);
                        let hi = left.max(top);
                        let pred = if top_left >= hi {
                            lo
                        } else if top_left <= lo {
                            hi
                        } else {
                            let r32 = (left as i32)
                                .wrapping_add(top as i32)
                                .wrapping_sub(top_left as i32);
                            (r32 as u16) & mask
                        };
                        cur[c] = cur[c].wrapping_sub(pred) & mask;
                    }
                    cur[0] = cur[0].wrapping_sub(prev[0]) & mask;
                } else {
                    let cur = &mut data[r * width..(r + 1) * width];
                    for c in (1..width).rev() {
                        cur[c] = cur[c].wrapping_sub(cur[c - 1]) & mask;
                    }
                }
            }
        }
    }
}

fn encode_frame_u16(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes_in: Vec<PlaneInput>,
    options: EncodeOptions,
) -> Result<Vec<u8>> {
    let num_planes = rec.planes as usize;
    if planes_in.len() != num_planes {
        return Err(Error::EncoderInputMismatch {
            plane: planes_in.len(),
            expected: num_planes,
            got: planes_in.len(),
        });
    }
    let w = width as usize;
    let h = height as usize;
    let sh = slice_height as usize;
    let slices_per_plane = h.div_ceil(sh);
    let total_slices = num_planes * slices_per_plane;
    let bits = rec.bit_depth;
    let mask = rec.sample_mask() as u16;
    let field_stride = if options.interlaced {
        FieldStride::INTERLACED
    } else {
        FieldStride::PROGRESSIVE
    };

    let mut planes: Vec<Vec<u16>> = Vec::with_capacity(num_planes);
    for (i, p) in planes_in.into_iter().enumerate() {
        match p {
            PlaneInput::U16(mut v) => {
                for x in v.iter_mut() {
                    *x &= mask;
                }
                planes.push(v);
            }
            PlaneInput::U8(_) => {
                return Err(Error::EncoderInputMismatch {
                    plane: i,
                    expected: 1,
                    got: 0,
                });
            }
        }
    }

    let plane_dims: Vec<(usize, usize, usize)> = (0..num_planes)
        .map(|p| plane_dims_for(rec, p, w, h, sh))
        .collect();
    for (p, (pw, ph, _)) in plane_dims.iter().enumerate() {
        let expected = pw * ph;
        if planes[p].len() != expected {
            return Err(Error::EncoderInputMismatch {
                plane: p,
                expected,
                got: planes[p].len(),
            });
        }
    }

    let mut wire_planes: Vec<Vec<u16>> = match rec.family {
        Family::Rgb | Family::Rgba => apply_rgb_decorrelation_u16(planes, mask),
        _ => planes,
    };

    let mut plane_resid: Vec<Vec<u16>> = Vec::with_capacity(num_planes);
    let mut slice_predictors: Vec<PredictorKind> = vec![PredictorKind::Left; total_slices];
    let strategy = options.strategy;
    // See `encode_frame_u8` for the per-plane scratch-reuse rationale.
    let mut trial_a: Vec<u16> = Vec::new();
    let mut trial_b: Vec<u16> = Vec::new();
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u16> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let src = &plane_data[row_start * pw..row_end * pw];
            let chosen = build_slice_residuals_u16_into(
                strategy,
                src,
                slice_rows,
                pw,
                mask,
                bits,
                field_stride,
                &mut residuals,
                &mut trial_a,
                &mut trial_b,
            );
            slice_predictors[p * slices_per_plane + s] = chosen;
        }
        plane_resid.push(residuals);
    }

    let max_len = max_huff_len_for(bits);
    let plane_huffs: Vec<PlaneHuff> = plane_resid
        .iter()
        .map(|res| {
            let mut hist = vec![0u32; 1usize << bits];
            for &b in res {
                hist[b as usize] += 1;
            }
            if res.is_empty() {
                hist[0] = 1;
            }
            PlaneHuff::build_from_histogram(&hist, max_len)
        })
        .collect();

    let mut slice_payloads: Vec<Vec<u8>> = Vec::with_capacity(total_slices);
    for (s, &pred_kind) in slice_predictors.iter().enumerate() {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;
        let (pw, ph, plane_slice_height) = plane_dims[plane];
        let _ = ph;
        let row_start = in_plane_idx * plane_slice_height;
        let row_end = ((in_plane_idx + 1) * plane_slice_height).min(plane_dims[plane].1);

        let res_block = &plane_resid[plane][row_start * pw..row_end * pw];
        let pred_id: u8 = predictor_id_byte(pred_kind);

        // Per-slice mode selection. Raw size at bit-depth `bits` is
        // `(pixels * bits + 7) / 8` bytes (spec/05 §4.1).
        let raw_bits = res_block.len() * bits as usize;
        let raw_size = raw_bits.div_ceil(8);
        let mut huff_buf: Option<Vec<u8>> = None;
        let flags: u8 = match options.mode {
            SliceMode::Huffman => 0x00,
            SliceMode::Raw => 0x01,
            SliceMode::Auto => {
                let huff = &plane_huffs[plane];
                // See the u8 Auto branch for the cap reasoning. At
                // 10/12/14-bit `raw_size = (pixels * bits + 7) / 8`.
                let mut bytes = Vec::with_capacity(raw_size + 1);
                pack_huffman_residuals_u16(&mut bytes, res_block, &huff.packed);
                if bytes.len() <= raw_size {
                    huff_buf = Some(bytes);
                    0x00
                } else {
                    0x01
                }
            }
        };

        // Pre-size the per-slice payload. For the 10/12/14-bit path,
        // pure-Huffman mode is capped by `(pixels * max_huff_len + 7)
        // / 8` with `max_huff_len ∈ {14,16,18}` (spec/05 §1) — the
        // worst-case ratio over `raw_size = (pixels * bits + 7) / 8` is
        // `max_huff_len / bits ≤ 18/10 = 1.8`. We use the same `2 *
        // raw_size + 2` cap as a comfortable margin.
        let mut payload = Vec::with_capacity(2 * raw_size + 2);
        payload.push(flags);
        payload.push(pred_id);

        if flags & 0x01 != 0 {
            // Bit-pack at `bits` bits MSB-first. Output is exactly
            // `raw_size` bytes (final partial byte already accounted
            // for by `div_ceil`). The batched packer hoists the
            // 64-bit accumulator + drain state into stack locals
            // across the slice, eliminating the per-symbol
            // `&mut BitWriter` reload + early-return that the
            // generic `BitWriter::write` carries.
            pack_raw_bits_from_u16(&mut payload, res_block, bits);
        } else if let Some(buf) = huff_buf {
            payload.extend(buf);
        } else {
            let huff = &plane_huffs[plane];
            // See the u8 fresh-Huffman comment for the cap rationale;
            // 1.8 × raw_size at 10-bit is the worst case. Reserve on
            // the existing `payload` so the batched packer's drain
            // hot loop is branchless on the capacity check.
            payload.reserve(2 * raw_size + 1);
            pack_huffman_residuals_u16(&mut payload, res_block, &huff.packed);
        }
        slice_payloads.push(payload);
    }

    Ok(assemble_frame(
        rec,
        width,
        height,
        slice_height,
        options.interlaced,
        options.color_matrix,
        options.full_range,
        total_slices,
        slices_per_plane,
        &plane_huffs,
        &slice_payloads,
    ))
}

/// `u16` analogue of [`build_slice_residuals_u8`]: pick a predictor
/// (per `strategy`) and emit its residuals. For Dynamic, the score is
/// `u16` analogue of [`build_slice_residuals_u8_into`]: same scratch
/// reuse + in-place-into-`dst`-tail shape. Fixed walks the predictor
/// in `dst`'s tail; Dynamic shuffles the three trial residuals
/// through `trial_a` / `trial_b` (reused across every slice + every
/// kind) and `extend_from_slice`-copies the winning scratch into
/// `dst` once.
#[allow(clippy::too_many_arguments)]
fn build_slice_residuals_u16_into(
    strategy: PredictorStrategy,
    src: &[u16],
    rows: usize,
    width: usize,
    mask: u16,
    bits: u8,
    field_stride: FieldStride,
    dst: &mut Vec<u16>,
    trial_a: &mut Vec<u16>,
    trial_b: &mut Vec<u16>,
) -> PredictorKind {
    match strategy {
        PredictorStrategy::Fixed(p) => {
            let tail_start = dst.len();
            dst.extend_from_slice(src);
            encode_predictor_u16(p, &mut dst[tail_start..], rows, width, mask, field_stride);
            p
        }
        PredictorStrategy::Dynamic => {
            let half = 1u32 << (bits - 1);

            // Kind 1 (Left) → trial_a.
            trial_a.clear();
            trial_a.extend_from_slice(src);
            encode_predictor_u16(
                PredictorKind::Left,
                &mut trial_a[..],
                rows,
                width,
                mask,
                field_stride,
            );
            let mut best_kind = PredictorKind::Left;
            let mut best_in_a = true;
            let mut best_score = abs_signed_sum_u16(trial_a, half);

            // Kind 2 (Gradient) → trial_b.
            trial_b.clear();
            trial_b.extend_from_slice(src);
            encode_predictor_u16(
                PredictorKind::Gradient,
                &mut trial_b[..],
                rows,
                width,
                mask,
                field_stride,
            );
            let score_b = abs_signed_sum_u16(trial_b, half);
            if score_b < best_score {
                best_score = score_b;
                best_kind = PredictorKind::Gradient;
                best_in_a = false;
            }

            // Kind 3 (Median) → loser's scratch.
            {
                let loser = if best_in_a {
                    &mut *trial_b
                } else {
                    &mut *trial_a
                };
                loser.clear();
                loser.extend_from_slice(src);
                encode_predictor_u16(
                    PredictorKind::Median,
                    &mut loser[..],
                    rows,
                    width,
                    mask,
                    field_stride,
                );
                let score_m = abs_signed_sum_u16(loser, half);
                if score_m < best_score {
                    best_kind = PredictorKind::Median;
                    best_in_a = !best_in_a;
                }
            }

            if best_in_a {
                dst.extend_from_slice(trial_a);
            } else {
                dst.extend_from_slice(trial_b);
            }
            best_kind
        }
    }
}

/// `sum_b min(b, (1 << bits) - b)` — the abs-signed L1 norm of the
/// residual interpreted as a signed value in the `bits`-bit window.
/// `half = 1 << (bits-1)`; a residual ≥ half wraps to the negative
/// side, where its magnitude is `(1 << bits) - r`.
fn abs_signed_sum_u16(block: &[u16], half: u32) -> u64 {
    let two_bits = 2u32 * half;
    let mut s: u64 = 0;
    for &b in block {
        let v = b as u32;
        let mag = if v >= half { two_bits - v } else { v };
        s += mag as u64;
    }
    s
}

fn apply_rgb_decorrelation_u16(planes: Vec<Vec<u16>>, mask: u16) -> Vec<Vec<u16>> {
    let mut iter = planes.into_iter();
    let g = iter.next().unwrap();
    let mut b = iter.next().unwrap();
    let mut r = iter.next().unwrap();
    let a_opt = iter.next();
    for (bv, &gv) in b.iter_mut().zip(g.iter()) {
        *bv = bv.wrapping_sub(gv) & mask;
    }
    for (rv, &gv) in r.iter_mut().zip(g.iter()) {
        *rv = rv.wrapping_sub(gv) & mask;
    }
    let mut out = vec![b, g, r];
    if let Some(a) = a_opt {
        out.push(a);
    }
    out
}

// ─────────────────────────── shared assembly ──────────────────────────

fn plane_dims_for(
    rec: FourccRecord,
    plane: usize,
    width: usize,
    height: usize,
    slice_height: usize,
) -> (usize, usize, usize) {
    let (sub_x, sub_y) = match rec.family {
        Family::Yuv if plane == 1 || plane == 2 => (rec.sub_x as usize, rec.sub_y as usize),
        Family::Yuva if plane == 1 || plane == 2 => (rec.sub_x as usize, rec.sub_y as usize),
        _ => (1usize, 1usize),
    };
    // Chroma planes round up at odd sizes (`spec/03` §8.2).
    let pw = width.div_ceil(sub_x);
    let ph = height.div_ceil(sub_y);
    let psh = slice_height / sub_y;
    (pw, ph, psh)
}

#[allow(clippy::too_many_arguments)]
fn assemble_frame(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    interlaced: bool,
    color_matrix: u8,
    full_range: bool,
    total_slices: usize,
    slices_per_plane: usize,
    plane_huffs: &[PlaneHuff],
    slice_payloads: &[Vec<u8>],
) -> Vec<u8> {
    let num_planes = rec.planes as usize;

    // Preamble: plane_count + per_slice_plane_index + huff descriptors.
    let mut preamble = Vec::new();
    preamble.push(num_planes as u8);
    for s in 0..total_slices {
        preamble.push((s / slices_per_plane) as u8);
    }
    for huff in plane_huffs {
        preamble.extend(encode_descriptor(&huff.lengths));
    }

    // Slice table: (total_slices + 1) u32 LE entries.
    let table_bytes = 4 * (total_slices + 1);
    let mut entries = vec![0u32; total_slices + 1];
    let preamble_off = table_bytes;
    let mut off = preamble_off + preamble.len();
    entries[0] = off as u32;
    entries[1] = off as u32;
    for s in 0..total_slices {
        entries[s + 1] = off as u32;
        off += slice_payloads[s].len();
    }

    // Build the final frame.
    let mut out = Vec::new();
    write_header(
        rec,
        width,
        height,
        slice_height,
        interlaced,
        color_matrix,
        full_range,
        &mut out,
    );
    debug_assert_eq!(out.len(), HEADER_SIZE);
    for &e in &entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out.extend_from_slice(&preamble);
    for p in slice_payloads {
        out.extend_from_slice(p);
    }
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// `spec/01` §3.1 flags-override keep-mask. The v2.4.2 encoder's
/// post-accumulation override at
/// `magicyuv.dll!0x69b9769c`–`0x69b976bb` builds the bit
/// `1 << (format_byte - 0x67)` and tests it against this mask.
/// Decoded per the spec's `bit b ↔ format byte 0x67 + b` formula,
/// the mask names the YUV/Gray-family format bytes
/// `{0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x73, 0x76, 0x77, 0x7b,
/// 0x7c, 0x7d, 0x7e}` — the set for which the override does **not**
/// fire. Every other format byte — the RGB family inside
/// `[0x67, 0x7e]`, plus `0x65` / `0x66` (8-bit RGB / RGBA) via the
/// out-of-range fallthrough — takes the override path: flags bit 2
/// (Full-range YUV, [`FLAG_FULL_RANGE`]) is cleared and the
/// codec_variant byte at header `+0x0b` is forced to `0x02`.
const FULL_RANGE_KEEP_MASK: u32 = 0x00f1_903f;

#[allow(clippy::too_many_arguments)]
fn write_header(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    interlaced: bool,
    color_matrix: u8,
    full_range: bool,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&MAGY_MAGIC);
    out.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    out.push(7); // version
    out.push(rec.format_byte);
    out.push(rec.aux_byte);
    out.push(0x02); // codec_variant — v2.4.2 always 0x02 (spec/04 §2).
    // Encoder OR-accumulator from spec/01 §3.1 (the v2.4.2 encoder's
    // `magicyuv.dll!0x69b97647`–`0x69b9767a` sequence): the
    // `ColorMatrix == 1` registry value is the matrix-skip
    // sentinel — the encoder branches over the OR step, leaving
    // flags bits 20..23 clear; any other low-nibble value
    // (0..=15 modulo `& 0xf`) shifts into bits 20..23 with mask
    // `0x00f00000`. Interlaced (bit 1) accumulates from the
    // separate `Interlaced` registry value at context `+0x70`.
    // Full-range YUV (bit 2, registry `+0x78`) — when the
    // `FullRangeYUV` registry value is non-zero, the OR step
    // contributes `0x4` to the flags dword (mask
    // `0x00000004`); the decoder pickup at
    // `magicyuv.dll!0x69bae311` (file `@0x2d311`) shifts the
    // dword right by 2 and isolates the low bit, routing the
    // boolean to the application/conversion layer.
    let mut flags: u32 = 0;
    if interlaced {
        flags |= FLAG_INTERLACED;
    }
    if full_range {
        flags |= FLAG_FULL_RANGE;
    }
    if color_matrix != 1 {
        flags |= u32::from(color_matrix & 0x0f) << FLAG_COLOR_MATRIX_SHIFT;
    }
    // spec/01 §3.1 post-accumulation override
    // (`magicyuv.dll!0x69b9769c`–`0x69b976bb`): compute the biased
    // index `format_byte - 0x67`; when it exceeds `0x17` the
    // override block is reached unconditionally (the unsigned
    // compare's fallthrough is how 8-bit RGB `0x65` / `0x66` land
    // there), otherwise the bit `1 << biased` is tested against
    // `FULL_RANGE_KEEP_MASK` and the override fires when the bit is
    // NOT in the mask (the RGB family). The override clears flags
    // bit 2 (Full-range YUV) and forces the codec_variant byte at
    // `+0x0b` to `0x02` — the latter is already unconditional above
    // per the spec/01 §3.0 v2.4.2 clamp at
    // `magicyuv.dll!0x69ba9060`, so only the flags-bit-2 clear is
    // materialised here. YUV/Gray-family streams retain whatever
    // bit 2 the OR-accumulation produced.
    let biased = u32::from(rec.format_byte).wrapping_sub(0x67);
    if biased > 0x17 || (1u32 << biased) & FULL_RANGE_KEEP_MASK == 0 {
        flags &= !FLAG_FULL_RANGE;
    }
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes()); // width_extra
    out.extend_from_slice(&slice_height.to_le_bytes());
}

#[cfg(test)]
mod huffman_limit_tests {
    use super::*;

    /// Kraft sum `Σ 2^-L` over the non-zero lengths, computed in exact
    /// rational form (numerator over `2^maxlen`) so the comparison to
    /// the complete-code target is integer-exact rather than float.
    fn kraft_is_one(lengths: &[u8]) -> bool {
        let max = lengths.iter().copied().max().unwrap_or(0);
        if max == 0 {
            return false;
        }
        let mut num: u128 = 0;
        for &l in lengths {
            if l > 0 {
                num += 1u128 << (max - l);
            }
        }
        num == (1u128 << max)
    }

    /// Validate that the production code-assignment path
    /// (`PlaneHuff::build_from_histogram`, lines ~242) produces a
    /// genuine *prefix-free* code from these lengths: no shorter code
    /// is a prefix of a longer one. This is the on-wire decodability
    /// guarantee — the decoder's flat lookup table can only be built
    /// from a prefix-free assignment (spec/05 §2.0.3).
    fn codes_are_prefix_free(hist: &[u32], lengths: &[u8]) -> bool {
        let ph = PlaneHuff::build_from_histogram(hist, *lengths.iter().max().unwrap_or(&1));
        let codes: Vec<(u32, u8)> = lengths
            .iter()
            .enumerate()
            .filter(|&(_, &l)| l > 0)
            .map(|(s, &l)| (ph.codes[s], l))
            .collect();
        // Pairwise: code A is a prefix of code B (A no longer than B)
        // iff B's top `len_a` MSB-aligned bits equal A. Compare every
        // pair MSB-aligned to a common 32-bit field.
        for i in 0..codes.len() {
            let (ca, la) = codes[i];
            let a_top = (ca as u64) << (32 - la as u64); // MSB-aligned A
            for &(cb, lb) in codes.iter().skip(i + 1) {
                let (short_top, short_len, long_top) = if la <= lb {
                    (a_top, la, (cb as u64) << (32 - lb as u64))
                } else {
                    ((cb as u64) << (32 - lb as u64), lb, a_top)
                };
                let mask = (!0u64) << (32 - short_len as u64);
                if (long_top & mask) == short_top {
                    return false; // prefix collision
                }
            }
        }
        true
    }

    /// Asserts the lengths are a length-capped complete prefix code
    /// over the whole alphabet (no symbol left without a code) that the
    /// production code-assignment path turns into a prefix-free code.
    fn assert_valid_capped(hist: &[u32], cap: u8) -> Vec<u8> {
        let lengths = complete_code_lengths(hist, cap);
        assert_eq!(lengths.len(), hist.len());
        let max = lengths.iter().copied().max().unwrap_or(0);
        assert!(max <= cap, "max length {max} exceeds cap {cap}");
        assert!(
            lengths.iter().all(|&l| l >= 1),
            "a symbol was left without a code"
        );
        assert!(kraft_is_one(&lengths), "Kraft sum != 1 for cap {cap}");
        assert!(
            codes_are_prefix_free(hist, &lengths),
            "code assignment not prefix-free for cap {cap}"
        );
        lengths
    }

    /// Total coded bits of `hist` under `lengths`.
    fn cost(hist: &[u32], lengths: &[u8]) -> u64 {
        hist.iter()
            .zip(lengths)
            .map(|(&h, &l)| h as u64 * l as u64)
            .sum()
    }

    /// Cost of the unconstrained optimal prefix code over the whole
    /// alphabet (sum of the merged weights of a two-smallest merge).
    fn unlimited_cost(hist: &[u32]) -> u64 {
        let mut w: Vec<u64> = hist.iter().map(|&h| h as u64).collect();
        let mut total = 0u64;
        while w.len() > 1 {
            w.sort_unstable_by(|a, b| b.cmp(a));
            let a = w.pop().unwrap();
            let b = w.pop().unwrap();
            total += a + b;
            w.push(a + b);
        }
        total
    }

    #[test]
    fn fibonacci_histogram_is_capped_and_complete() {
        // A Fibonacci frequency profile has an unconstrained-optimal
        // depth of ~N-1 (47 for N=64) — far past the 8-bit cap of 12.
        let mut hist = vec![0u32; 64];
        let (mut a, mut b) = (1u32, 1u32);
        for h in hist.iter_mut() {
            *h = a;
            let c = a.wrapping_add(b);
            a = b;
            b = c;
        }
        assert_valid_capped(&hist, 12);
    }

    #[test]
    fn geometric_skew_8bit_capped() {
        let mut hist = vec![0u32; 256];
        for (i, h) in hist.iter_mut().enumerate() {
            let shift = 30u64.saturating_sub(i as u64 / 4);
            *h = (1u64 << shift) as u32 + 1;
        }
        assert_valid_capped(&hist, 12);
    }

    #[test]
    fn geometric_skew_10bit_capped() {
        let mut hist = vec![0u32; 1024];
        for (i, h) in hist.iter_mut().enumerate() {
            let shift = 30u64.saturating_sub(i as u64 / 8);
            *h = (1u64 << shift) as u32 + 1;
        }
        assert_valid_capped(&hist, 14);
    }

    #[test]
    fn non_binding_cap_matches_the_unconstrained_optimum() {
        // When the unconstrained optimum already fits the cap, the
        // length-limited construction must cost exactly the same.
        let mut hist = vec![1u32; 256];
        hist[0] = 1_000_000;
        let lengths = assert_valid_capped(&hist, 12);
        assert_eq!(cost(&hist, &lengths), unlimited_cost(&hist));

        let mut state = 0x1234_5678u32;
        let mut noisy = vec![0u32; 256];
        for h in noisy.iter_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *h = 900 + state % 200;
        }
        let lengths = assert_valid_capped(&noisy, 12);
        assert_eq!(cost(&noisy, &lengths), unlimited_cost(&noisy));
    }

    #[test]
    fn uniform_alphabet_is_fixed_length() {
        for (n, cap) in [
            (16usize, 12u8),
            (256, 12),
            (1024, 14),
            (4096, 16),
            (16384, 18),
        ] {
            let hist = vec![1u32; n];
            let lengths = assert_valid_capped_fast(&hist, cap);
            let expect = (usize::BITS - 1 - (n.leading_zeros())) as u8; // log2(n)
            assert!(lengths.iter().all(|&l| l == expect));
        }
    }

    /// As [`assert_valid_capped`] minus the quadratic prefix-free scan,
    /// for the 12/14-bit alphabets.
    fn assert_valid_capped_fast(hist: &[u32], cap: u8) -> Vec<u8> {
        let lengths = complete_code_lengths(hist, cap);
        assert!(lengths.iter().all(|&l| l >= 1 && l <= cap));
        assert!(kraft_is_one(&lengths));
        lengths
    }

    #[test]
    fn two_used_symbols_still_code_the_whole_alphabet() {
        let hist = [5u32, 3, 0, 0];
        let lengths = assert_valid_capped(&hist, 12);
        assert_eq!(lengths[0], 1);
        assert_eq!(lengths[1], 2);
        assert_eq!((lengths[2], lengths[3]), (3, 3));
    }

    #[test]
    fn single_used_symbol_matches_the_spec_all_zero_book_shape() {
        // spec/05 §1.2: the all-zero 8-bit plane's book is one
        // length-1 code, one length-8 code and 254 length-9 codes.
        let mut hist = vec![0u32; 256];
        hist[0] = 4096;
        let lengths = assert_valid_capped(&hist, 12);
        assert_eq!(lengths[0], 1);
        assert_eq!(lengths.iter().filter(|&&l| l == 8).count(), 1);
        assert_eq!(lengths.iter().filter(|&&l| l == 9).count(), 254);
    }

    #[test]
    fn sparse_high_depth_alphabets_stay_complete() {
        // A handful of used symbols in the 10/12/14-bit alphabets: the
        // unused symbols must fit under the cap without starving the
        // used ones (they take at most 1/16 of the codespace).
        for (bits, cap) in [(10u32, 14u8), (12, 16), (14, 18)] {
            let n = 1usize << bits;
            let mut hist = vec![0u32; n];
            hist[0] = 50_000;
            hist[1] = 20_000;
            hist[n - 1] = 20_000;
            hist[n / 2] = 7;
            let lengths = assert_valid_capped_fast(&hist, cap);
            assert_eq!(lengths[0], 1);
            assert!(lengths[1] <= 3 && lengths[n - 1] <= 3);
            let unused_max = lengths[2];
            assert_eq!(unused_max, cap);
        }
    }

    // --- Descriptor run-length cap (spec/05 §1.5 / §10 Q3) ---------------

    #[test]
    fn descriptor_run_caps_at_255_count_byte_0xfe() {
        // A length value repeated exactly 256 times must NOT collapse
        // into a single `(0x80|v, 0xff)` pair — the vendor encoder caps
        // a run at 255 reps (count byte 0xfe), reserving 0xff. The
        // remaining single repetition spills into a literal.
        let lengths = vec![9u8; 256];
        let desc = super::encode_descriptor(&lengths);
        // Expect: run of 255 (89 fe) + 1 literal (09).
        assert_eq!(desc, vec![0x80 | 9, 0xfe, 9]);
        // No 0xff count byte anywhere in the descriptor.
        assert!(
            !desc.windows(2).any(|w| (w[0] & 0x80) != 0 && w[1] == 0xff),
            "descriptor emitted reserved 0xff count byte: {desc:02x?}"
        );
    }

    #[test]
    fn descriptor_sparse_highdepth_splits_into_capped_runs() {
        // spec/05 §10 Q3 worked shape: a 10-bit (N=1024) descriptor
        // whose tail is one length value repeated 1023 times. The tail
        // must split into successive 255-rep runs, never a single
        // out-of-range run.
        let mut lengths = vec![11u8; 1024];
        lengths[0] = 1;
        let desc = super::encode_descriptor(&lengths);
        // First byte is the length-1 literal; the rest encode 1023
        // zero-lengths as 4×(80 fe) [=1020] + a tail run of 3 (80 02).
        assert_eq!(desc[0], 1);
        assert!(
            !desc.windows(2).any(|w| (w[0] & 0x80) != 0 && w[1] == 0xff),
            "high-depth sparse descriptor emitted 0xff count: {desc:02x?}"
        );
        // Round-trip through the decoder's parser reproduces the lengths.
        let (parsed, consumed) =
            crate::huffman::parse_lengths(&desc, 1024, 14, 0).expect("descriptor must re-parse");
        assert_eq!(parsed, lengths);
        assert_eq!(consumed, desc.len());
    }

    #[test]
    fn descriptor_run_of_255_stays_single_pair() {
        // Exactly 255 reps is the largest single run: count byte 0xfe.
        let lengths = vec![7u8; 255];
        let desc = super::encode_descriptor(&lengths);
        assert_eq!(desc, vec![0x80 | 7, 0xfe]);
    }
}

#[cfg(test)]
mod bit_writer_tests {
    //! Direct parity tests for [`BitWriter`]'s whole-byte-drain shape.
    //!
    //! The round-trip suite (`roundtrip_tests.rs`, 84+ scenarios) is
    //! the primary oracle — it feeds every encoded byte back through
    //! `decode_frame` and asserts an exact pixel-equal round-trip,
    //! which catches any wire-byte drift in the `BitWriter` immediately.
    //!
    //! These tests pin the writer's emit shape directly at the unit
    //! level so a future hot-loop tweak shows up as a unit failure
    //! before the integration tests reach it. The reference is a
    //! deliberately trivial bit-by-bit shape that walks each `(code,
    //! len)` MSB-first into a `Vec<u8>`, padding the final partial
    //! byte with zero bits — exactly the wire convention `spec/05` §1.1
    //! sets out for the Huffman payload.
    use super::BitWriter;

    /// Reference encoder: writes `len` MSB-aligned bits into the
    /// growing byte vector one bit at a time. Matches the
    /// `BitWriter::finish()` zero-pad-tail convention.
    fn reference(write_ops: &[(u32, u8)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut acc: u8 = 0;
        let mut bits: u8 = 0;
        for &(code, len) in write_ops {
            for i in (0..len).rev() {
                let bit = ((code >> i) & 1) as u8;
                acc = (acc << 1) | bit;
                bits += 1;
                if bits == 8 {
                    out.push(acc);
                    acc = 0;
                    bits = 0;
                }
            }
        }
        if bits != 0 {
            out.push(acc << (8 - bits));
        }
        out
    }

    fn drive(write_ops: &[(u32, u8)]) -> Vec<u8> {
        let mut bw = BitWriter::with_capacity(write_ops.len());
        for &(code, len) in write_ops {
            bw.write(code, len);
        }
        bw.finish()
    }

    #[test]
    fn empty_writer_emits_no_bytes() {
        assert!(drive(&[]).is_empty());
    }

    #[test]
    fn zero_length_writes_are_noops() {
        assert_eq!(drive(&[(0, 0), (0, 0), (0xff, 0)]), Vec::<u8>::new());
        // Mixed with non-zero writes.
        let ops = [(0, 0), (0b1011, 4), (0, 0), (0b0110, 4)];
        assert_eq!(drive(&ops), reference(&ops));
    }

    #[test]
    fn single_byte_aligned_write() {
        assert_eq!(drive(&[(0xa5, 8)]), vec![0xa5]);
    }

    #[test]
    fn unaligned_tail_zero_pads() {
        // 4 bits 0b1011 → wire byte 0b1011_0000.
        assert_eq!(drive(&[(0b1011, 4)]), vec![0b1011_0000]);
    }

    #[test]
    fn short_codes_match_reference_fixed() {
        // Hand-crafted sequence that crosses several byte boundaries
        // without any single write filling the accumulator. Exercises
        // the bytes_to_drain ∈ {0, 1, 2} drain paths.
        let ops = [
            (0b1, 1),
            (0b01, 2),
            (0b110, 3),
            (0b0101, 4),
            (0b10110, 5),
            (0b001100, 6),
            (0b1010101, 7),
            (0b10101010, 8),
            (0b110, 3),
        ];
        assert_eq!(drive(&ops), reference(&ops));
    }

    #[test]
    fn long_codes_drain_multiple_bytes() {
        // Each 32-bit write forces a 4-byte drain. Crossing 8-byte
        // boundaries exercises the larger bytes_to_drain values.
        let ops = [
            (0xdead_beef_u32, 32),
            (0xcafe_babe, 32),
            (0xfeed_face, 32),
            (0x1234_5678, 32),
        ];
        assert_eq!(drive(&ops), reference(&ops));
    }

    #[test]
    fn bits_used_plus_len_equals_64_full_drain() {
        // Pre-fill the accumulator to bits_used = 32, then issue a
        // len-32 write so bits_used + len == 64. The drain path then
        // computes bytes_to_drain = 8 and must zero `acc` rather than
        // shift by 64 (a u64 << 64 is undefined behaviour the
        // implementation special-cases). Wire output must still match
        // the bit-by-bit reference.
        let ops = [(0x1234_5678_u32, 32), (0xfedc_ba98, 32)];
        assert_eq!(drive(&ops), reference(&ops));
        // Same boundary after a tiny prefix so the alignment differs.
        let ops2 = [(0b1, 1), (0x7fff_ffff, 31), (0x1234_5678, 32)];
        assert_eq!(drive(&ops2), reference(&ops2));
    }

    #[test]
    fn xorshift_random_streams_match_reference() {
        // 8 independent xorshift streams, each ~512 (code, len) writes
        // with `len ∈ [1, 32]`. Walks every drain shape the production
        // hot loop emits in practice (the per-symbol Huffman pack
        // produces 1-12 bit codes at 8-bit depth, 1-18 at 14-bit
        // depth — well inside the 32-bit cap).
        for seed in 0u32..8 {
            let mut state = 0x9e37_79b9_u32 ^ seed.wrapping_mul(0xdead_beef);
            let mut ops: Vec<(u32, u8)> = Vec::with_capacity(512);
            for _ in 0..512 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let len = ((state >> 8) % 32) as u8 + 1; // [1, 32]
                let mask: u32 = if len == 32 { !0 } else { (1u32 << len) - 1 };
                let code = (state.wrapping_mul(0x85eb_ca6b)) & mask;
                ops.push((code, len));
            }
            assert_eq!(drive(&ops), reference(&ops), "seed {seed}");
        }
    }
}

#[cfg(test)]
mod pack_raw_bits_tests {
    //! Pin the batched `pack_raw_bits_from_u16` against a per-pixel
    //! `BitWriter::write(sym as u32, bits)` reference oracle.
    //!
    //! The round-trip suite covers raw-mode bytes end-to-end (encode
    //! → decode round-trip on every FOURCC × predictor × Raw
    //! configuration). These unit tests pin the packer body in
    //! isolation so any future hot-loop reshape lands as a focused
    //! failure before the larger integration tests reach it,
    //! mirroring the `bit_writer_tests` shape from a prior round.
    use super::*;

    fn reference(samples: &[u16], bits: u8) -> Vec<u8> {
        // Per-symbol drive through the production `BitWriter` — same
        // observable byte stream we already verify via the round-trip
        // tests. The reference walks one `bw.write(sym as u32, bits)`
        // call per sample, exactly the shape the prior raw-mode
        // emitter used at every call site.
        let cap = (samples.len() * bits as usize).div_ceil(8);
        let mut bw = BitWriter::with_capacity(cap);
        for &s in samples {
            bw.write(s as u32, bits);
        }
        bw.finish()
    }

    fn drive(samples: &[u16], bits: u8) -> Vec<u8> {
        let mut out = Vec::new();
        pack_raw_bits_from_u16(&mut out, samples, bits);
        out
    }

    #[test]
    fn empty_input_emits_no_bytes() {
        assert!(drive(&[], 10).is_empty());
        assert!(drive(&[], 12).is_empty());
        assert!(drive(&[], 14).is_empty());
    }

    #[test]
    fn out_of_range_bits_is_noop() {
        assert!(drive(&[0x123, 0x456], 0).is_empty());
        assert!(drive(&[0x1, 0x2], 17).is_empty());
    }

    #[test]
    fn single_aligned_sample() {
        // 16-bit width aligns to two whole bytes.
        assert_eq!(drive(&[0xabcd], 16), vec![0xab, 0xcd]);
    }

    #[test]
    fn appends_to_existing_buffer() {
        // The packer must `extend` an existing `payload` Vec — verify
        // the prefix bytes survive intact.
        let mut out = vec![0xff_u8, 0xee, 0xdd];
        pack_raw_bits_from_u16(&mut out, &[0xabcd], 16);
        assert_eq!(out, vec![0xff, 0xee, 0xdd, 0xab, 0xcd]);
    }

    #[test]
    fn matches_per_pixel_at_10_12_14() {
        // For each bit-depth, walk a 200-sample deterministic stream
        // covering edge values (0, mask, mid-range) plus xorshift
        // chaos. Assert the batched and per-pixel outputs are
        // byte-identical, and that the result re-unpacks to the
        // original samples.
        use crate::bitreader::unpack_raw_bits_to_u16;
        for &bits in &[10u8, 12, 14] {
            let mask = ((1u32 << bits) - 1) as u16;
            let n = 200usize;
            let mut s: u32 = 0xdead_beef ^ (bits as u32).wrapping_mul(0x9e37_79b9);
            let mut samples = Vec::with_capacity(n);
            for i in 0..n {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let v = if i == 0 {
                    0
                } else if i == 1 {
                    mask
                } else {
                    (s as u16) & mask
                };
                samples.push(v);
            }
            let fast = drive(&samples, bits);
            let slow = reference(&samples, bits);
            assert_eq!(fast, slow, "fast/slow mismatch at bits={bits}");

            // Round-trip through the decoder's batched unpacker.
            let mut recovered = vec![0u16; n];
            unpack_raw_bits_to_u16(&fast, &mut recovered, bits, mask);
            assert_eq!(recovered, samples, "round-trip mismatch at bits={bits}");
        }
    }

    #[test]
    fn unaligned_tail_zero_pads() {
        // 3 samples × 10 bits = 30 bits → 4 bytes (last byte's low 2
        // bits are zero). Walk the per-pixel reference for the
        // canonical layout.
        let samples = [0x3ff_u16, 0x000, 0x2aa];
        let out = drive(&samples, 10);
        assert_eq!(out.len(), (3 * 10_usize).div_ceil(8));
        assert_eq!(out, reference(&samples, 10));
    }

    #[test]
    fn long_payload_crosses_multiple_drains() {
        // 4096 14-bit samples = 7168 bytes, well past any single-
        // drain boundary. Pins the per-symbol drain loop's
        // post-merge state across many cycles.
        let bits = 14u8;
        let mask = 0x3fff_u16;
        let n = 4096usize;
        let mut s: u32 = 0x1234_5678;
        let samples: Vec<u16> = (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as u16) & mask
            })
            .collect();
        let fast = drive(&samples, bits);
        let slow = reference(&samples, bits);
        assert_eq!(fast, slow);
        assert_eq!(fast.len(), (n * bits as usize).div_ceil(8));
    }
}

#[cfg(test)]
mod pack_huffman_residuals_tests {
    //! Pin the batched `pack_huffman_residuals_u{8,16}` against a
    //! per-pixel `BitWriter::write(huff.codes[sym], huff.lengths[sym])`
    //! reference oracle.
    //!
    //! The round-trip suite covers Huffman-mode bytes end-to-end
    //! (every FOURCC × predictor × Huffman / Auto configuration). The
    //! parity tests here pin the packer body in isolation, mirroring
    //! `pack_raw_bits_tests` so any future hot-loop reshape (e.g. a
    //! SIMDified inner loop, or a packed-table layout change) lands as
    //! a focused failure before the broader integration suite reaches
    //! it.
    use super::*;

    fn reference_u8(src: &[u8], huff: &PlaneHuff) -> Vec<u8> {
        // The shape the encoder walked at every call site before
        // opt-13 — two table fetches per symbol + one `BitWriter::write`
        // per symbol. The accumulated drain shape is `BitWriter`'s.
        let cap = src.len();
        let mut bw = BitWriter::with_capacity(cap);
        for &sym in src {
            let len = huff.lengths[sym as usize];
            let code = huff.codes[sym as usize];
            bw.write(code, len);
        }
        bw.finish()
    }

    fn reference_u16(src: &[u16], huff: &PlaneHuff) -> Vec<u8> {
        let cap = 2 * src.len();
        let mut bw = BitWriter::with_capacity(cap);
        for &sym in src {
            let len = huff.lengths[sym as usize];
            let code = huff.codes[sym as usize];
            bw.write(code, len);
        }
        bw.finish()
    }

    fn drive_u8(src: &[u8], huff: &PlaneHuff) -> Vec<u8> {
        let mut out = Vec::new();
        pack_huffman_residuals_u8(&mut out, src, &huff.packed);
        out
    }

    fn drive_u16(src: &[u16], huff: &PlaneHuff) -> Vec<u8> {
        let mut out = Vec::new();
        pack_huffman_residuals_u16(&mut out, src, &huff.packed);
        out
    }

    /// Build a PlaneHuff from a histogram covering every symbol in
    /// `src` — guarantees all symbols are reachable from a non-zero
    /// length, so the per-symbol drive never hits the unused-symbol
    /// `length == 0` early-continue branch.
    fn build_huff_u8(src: &[u8], max_len: u8) -> PlaneHuff {
        let mut hist = vec![0u32; 256];
        for &s in src {
            hist[s as usize] += 1;
        }
        // Guarantee at least one symbol so `build_from_histogram`
        // takes the non-trivial path.
        if src.is_empty() {
            hist[0] = 1;
        }
        PlaneHuff::build_from_histogram(&hist, max_len)
    }

    fn build_huff_u16(src: &[u16], alphabet: usize, max_len: u8) -> PlaneHuff {
        let mut hist = vec![0u32; alphabet];
        for &s in src {
            hist[s as usize] += 1;
        }
        if src.is_empty() {
            hist[0] = 1;
        }
        PlaneHuff::build_from_histogram(&hist, max_len)
    }

    #[test]
    fn empty_input_emits_no_bytes() {
        let huff = build_huff_u8(&[], 12);
        assert!(drive_u8(&[], &huff).is_empty());
        let huff16 = build_huff_u16(&[], 1024, 14);
        assert!(drive_u16(&[], &huff16).is_empty());
    }

    #[test]
    fn appends_to_existing_buffer() {
        // Mirrors the call-site shape: the packer is called on a
        // payload Vec that already carries `flags + pred_id`. Verify
        // the prefix bytes survive intact.
        let src = [0u8, 1, 2, 1, 0, 1, 2, 1, 0];
        let huff = build_huff_u8(&src, 12);
        let mut out = vec![0xff_u8, 0xee, 0xdd];
        pack_huffman_residuals_u8(&mut out, &src, &huff.packed);
        assert_eq!(&out[..3], &[0xff, 0xee, 0xdd]);
        assert_eq!(&out[3..], reference_u8(&src, &huff).as_slice());
    }

    #[test]
    fn matches_per_pixel_u8_xorshift() {
        // 8-bit alphabet, xorshift fan-out across the whole 256-symbol
        // space. Stress every drain-loop cycle the per-call-site walk
        // would hit; assert byte-for-byte equivalence to the
        // `BitWriter::write`-per-symbol reference.
        let n = 4096usize;
        let mut s: u32 = 0xc0ff_ee01;
        let src: Vec<u8> = (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect();
        let huff = build_huff_u8(&src, 12);
        let fast = drive_u8(&src, &huff);
        let slow = reference_u8(&src, &huff);
        assert_eq!(fast, slow);
    }

    #[test]
    fn matches_per_pixel_u16_at_10_12_14() {
        // High-bit-depth tiers. Each tier walks an alphabet capped at
        // `1 << bits` with the per-tier `max_len`; the residual stream
        // is xorshift-shaped + masked into the alphabet so every
        // symbol class shows up.
        for &(bits, max_len) in &[(10u8, 14u8), (12, 16), (14, 18)] {
            let alphabet = 1usize << bits;
            let mask = (alphabet - 1) as u16;
            let n = 2048usize;
            let mut s: u32 = 0xfeed_d00d ^ (bits as u32).wrapping_mul(0x9e37_79b9);
            let src: Vec<u16> = (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    (s as u16) & mask
                })
                .collect();
            let huff = build_huff_u16(&src, alphabet, max_len);
            let fast = drive_u16(&src, &huff);
            let slow = reference_u16(&src, &huff);
            assert_eq!(fast, slow, "fast/slow mismatch at bits={bits}");
        }
    }

    #[test]
    fn skewed_histogram_matches_reference() {
        // Skewed Fibonacci-like histogram drives `enforce_length_cap`
        // (the length-limited Package-Merge path). Verifies the packer
        // is correct even at the cap-bound `max_huff_len = 12` boundary
        // where the codes hit the 12-bit ceiling.
        let mut hist = vec![0u32; 256];
        let mut a: u32 = 1;
        let mut b: u32 = 1;
        for h in hist.iter_mut().take(40) {
            *h = a;
            let next = a + b;
            a = b;
            b = next;
        }
        let huff = PlaneHuff::build_from_histogram(&hist, 12);
        // Drive 1024 symbols sampled from the skewed alphabet.
        let mut s: u32 = 0x1357_9bdf;
        let src: Vec<u8> = (0..1024)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                // Stay inside the first 40 active symbols.
                (s as usize % 40) as u8
            })
            .collect();
        let fast = drive_u8(&src, &huff);
        let slow = reference_u8(&src, &huff);
        assert_eq!(fast, slow);
    }

    #[test]
    fn packed_layout_matches_codes_and_lengths() {
        // Sanity-check the packed encoding is `(code << 8) | length`.
        // Take an arbitrary histogram, build, then verify every symbol's
        // packed entry recovers both halves.
        let mut hist = vec![0u32; 256];
        for (i, h) in hist.iter_mut().enumerate().take(64) {
            *h = (i + 1) as u32;
        }
        let huff = PlaneHuff::build_from_histogram(&hist, 12);
        assert_eq!(huff.packed.len(), 256);
        for (s, &entry) in huff.packed.iter().enumerate() {
            let length = (entry & 0xff) as u8;
            let code = entry >> 8;
            assert_eq!(length, huff.lengths[s]);
            assert_eq!(code, huff.codes[s]);
        }
    }

    #[test]
    fn sparse_histogram_stream_matches_reference() {
        // A histogram that leaves most symbols unused (they still get
        // codes, just long ones): the batched packer's bytes equal what
        // the per-pixel reference emits.
        let mut hist = vec![0u32; 256];
        hist[10] = 3;
        hist[20] = 2;
        hist[30] = 1;
        let huff = PlaneHuff::build_from_histogram(&hist, 12);
        let src = [10u8, 20, 30, 10, 20, 10];
        let fast = drive_u8(&src, &huff);
        let slow = reference_u8(&src, &huff);
        assert_eq!(fast, slow);
    }
}

#[cfg(test)]
mod build_slice_residuals_scratch_tests {
    //! Pin the `build_slice_residuals_u{8,16}_into` contract: the
    //! scratch buffers (`trial_a` / `trial_b`) are reused across
    //! multiple slices + multiple kinds, but each call's output into
    //! `dst` must be identical to the result of a fresh-Vec per-trial
    //! reference shape. These tests catch any future scratch-state
    //! leak (e.g. forgetting `clear()` before `extend_from_slice`).
    //!
    //! The round-trip suite (`roundtrip_tests`) is the integration-
    //! level oracle for bitstream correctness; these are unit-level
    //! parity tests on the residual builder alone.
    use super::*;
    use crate::predict::FieldStride;

    fn reference_u8(
        strategy: PredictorStrategy,
        src: &[u8],
        rows: usize,
        width: usize,
        field_stride: FieldStride,
    ) -> (PredictorKind, Vec<u8>) {
        match strategy {
            PredictorStrategy::Fixed(p) => {
                let mut block = src.to_vec();
                encode_predictor_u8(p, &mut block, rows, width, field_stride);
                (p, block)
            }
            PredictorStrategy::Dynamic => {
                let mut best_kind = PredictorKind::Left;
                let mut best_block: Vec<u8> = Vec::new();
                let mut best_score: u64 = u64::MAX;
                for &kind in &[
                    PredictorKind::Left,
                    PredictorKind::Gradient,
                    PredictorKind::Median,
                ] {
                    let mut block = src.to_vec();
                    encode_predictor_u8(kind, &mut block, rows, width, field_stride);
                    let score = abs_signed_sum_u8(&block);
                    if score < best_score {
                        best_score = score;
                        best_kind = kind;
                        best_block = block;
                    }
                }
                (best_kind, best_block)
            }
        }
    }

    fn reference_u16(
        strategy: PredictorStrategy,
        src: &[u16],
        rows: usize,
        width: usize,
        mask: u16,
        bits: u8,
        field_stride: FieldStride,
    ) -> (PredictorKind, Vec<u16>) {
        match strategy {
            PredictorStrategy::Fixed(p) => {
                let mut block = src.to_vec();
                encode_predictor_u16(p, &mut block, rows, width, mask, field_stride);
                (p, block)
            }
            PredictorStrategy::Dynamic => {
                let mut best_kind = PredictorKind::Left;
                let mut best_block: Vec<u16> = Vec::new();
                let mut best_score: u64 = u64::MAX;
                let half = 1u32 << (bits - 1);
                for &kind in &[
                    PredictorKind::Left,
                    PredictorKind::Gradient,
                    PredictorKind::Median,
                ] {
                    let mut block = src.to_vec();
                    encode_predictor_u16(kind, &mut block, rows, width, mask, field_stride);
                    let score = abs_signed_sum_u16(&block, half);
                    if score < best_score {
                        best_score = score;
                        best_kind = kind;
                        best_block = block;
                    }
                }
                (best_kind, best_block)
            }
        }
    }

    fn xorshift(seed: u32, n: usize) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            out.push((s & 0xff) as u8);
        }
        out
    }

    fn xorshift_u16(seed: u32, n: usize, mask: u16) -> Vec<u16> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            out.push((s as u16) & mask);
        }
        out
    }

    #[test]
    fn u8_into_matches_reference_across_three_consecutive_slices() {
        // 16-row plane in three 5/5/6-row slices — exercises scratch
        // reuse across slices of varying length.
        let width = 24;
        let plane = xorshift(0xC0FFEE, 16 * width);
        let strategies = [
            PredictorStrategy::Fixed(PredictorKind::Left),
            PredictorStrategy::Fixed(PredictorKind::Gradient),
            PredictorStrategy::Fixed(PredictorKind::Median),
            PredictorStrategy::Dynamic,
        ];
        for &strategy in &strategies {
            let mut dst = Vec::new();
            let mut ta = Vec::new();
            let mut tb = Vec::new();
            let mut row_ofs = 0usize;
            let row_runs = [5usize, 5, 6];
            let mut got_kinds = Vec::new();
            for &rows in &row_runs {
                let src = &plane[row_ofs * width..(row_ofs + rows) * width];
                let k = build_slice_residuals_u8_into(
                    strategy,
                    src,
                    rows,
                    width,
                    FieldStride::PROGRESSIVE,
                    &mut dst,
                    &mut ta,
                    &mut tb,
                );
                got_kinds.push(k);
                row_ofs += rows;
            }
            // Reference: independent fresh-Vec per slice.
            let mut want: Vec<u8> = Vec::new();
            let mut want_kinds = Vec::new();
            row_ofs = 0;
            for &rows in &row_runs {
                let src = &plane[row_ofs * width..(row_ofs + rows) * width];
                let (k, block) = reference_u8(strategy, src, rows, width, FieldStride::PROGRESSIVE);
                want_kinds.push(k);
                want.extend_from_slice(&block);
                row_ofs += rows;
            }
            assert_eq!(got_kinds, want_kinds, "strategy {strategy:?}");
            assert_eq!(dst, want, "strategy {strategy:?}");
        }
    }

    #[test]
    fn u16_into_matches_reference_across_consecutive_slices() {
        let width = 32;
        let bits = 10u8;
        let mask: u16 = (1 << bits) - 1;
        let plane = xorshift_u16(0xBADBEEF, 24 * width, mask);
        let strategies = [
            PredictorStrategy::Fixed(PredictorKind::Left),
            PredictorStrategy::Fixed(PredictorKind::Gradient),
            PredictorStrategy::Fixed(PredictorKind::Median),
            PredictorStrategy::Dynamic,
        ];
        for &strategy in &strategies {
            let mut dst = Vec::new();
            let mut ta = Vec::new();
            let mut tb = Vec::new();
            let mut row_ofs = 0usize;
            let row_runs = [6usize, 8, 10];
            let mut got_kinds = Vec::new();
            for &rows in &row_runs {
                let src = &plane[row_ofs * width..(row_ofs + rows) * width];
                let k = build_slice_residuals_u16_into(
                    strategy,
                    src,
                    rows,
                    width,
                    mask,
                    bits,
                    FieldStride::PROGRESSIVE,
                    &mut dst,
                    &mut ta,
                    &mut tb,
                );
                got_kinds.push(k);
                row_ofs += rows;
            }
            let mut want: Vec<u16> = Vec::new();
            let mut want_kinds = Vec::new();
            row_ofs = 0;
            for &rows in &row_runs {
                let src = &plane[row_ofs * width..(row_ofs + rows) * width];
                let (k, block) = reference_u16(
                    strategy,
                    src,
                    rows,
                    width,
                    mask,
                    bits,
                    FieldStride::PROGRESSIVE,
                );
                want_kinds.push(k);
                want.extend_from_slice(&block);
                row_ofs += rows;
            }
            assert_eq!(got_kinds, want_kinds, "strategy {strategy:?}");
            assert_eq!(dst, want, "strategy {strategy:?}");
        }
    }

    #[test]
    fn u8_into_interlaced_matches_reference() {
        // field_stride = 2 path: at least 4 rows so the predictor
        // walks beyond the header-row block.
        let width = 16;
        let plane = xorshift(0xDECADE, 8 * width);
        let mut dst = Vec::new();
        let mut ta = Vec::new();
        let mut tb = Vec::new();
        let k = build_slice_residuals_u8_into(
            PredictorStrategy::Dynamic,
            &plane,
            8,
            width,
            FieldStride::INTERLACED,
            &mut dst,
            &mut ta,
            &mut tb,
        );
        let (rk, rblock) = reference_u8(
            PredictorStrategy::Dynamic,
            &plane,
            8,
            width,
            FieldStride::INTERLACED,
        );
        assert_eq!(k, rk);
        assert_eq!(dst, rblock);
    }

    #[test]
    fn u8_into_dynamic_picks_lowest_l1_with_ties_broken_left_first() {
        // Uniform plane: every predictor produces all-zero residuals,
        // so all three score 0. Tie-break must pick Left (predictor-id
        // ascending), matching the prior shape's `<` comparison.
        let width = 8;
        let rows = 4;
        let plane = vec![0x42u8; rows * width];
        let mut dst = Vec::new();
        let mut ta = Vec::new();
        let mut tb = Vec::new();
        let k = build_slice_residuals_u8_into(
            PredictorStrategy::Dynamic,
            &plane,
            rows,
            width,
            FieldStride::PROGRESSIVE,
            &mut dst,
            &mut ta,
            &mut tb,
        );
        assert_eq!(k, PredictorKind::Left, "tie-break is Left-first");
        // Residual is `plane - predicted` reduced modulo 256. For a
        // uniform 0x42 plane under Left, every cell after column 0 of
        // row 0 (= 0x42 itself) is `cur - prev = 0`. The (0,0) cell
        // has no neighbour to subtract.
        assert_eq!(dst[0], 0x42);
        assert!(dst[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn u8_into_fixed_writes_into_dst_tail_without_touching_scratch() {
        // Fixed mode must leave the scratch buffers alone — proves
        // the call doesn't gratuitously clobber state callers may
        // legitimately not have allocated yet.
        let width = 8;
        let rows = 3;
        let plane = xorshift(7, rows * width);
        let mut dst = Vec::new();
        let mut ta = vec![0xAAu8; 16]; // sentinel
        let mut tb = vec![0xBBu8; 16];
        let _ = build_slice_residuals_u8_into(
            PredictorStrategy::Fixed(PredictorKind::Gradient),
            &plane,
            rows,
            width,
            FieldStride::PROGRESSIVE,
            &mut dst,
            &mut ta,
            &mut tb,
        );
        assert_eq!(ta, vec![0xAAu8; 16], "Fixed mode must not touch trial_a");
        assert_eq!(tb, vec![0xBBu8; 16], "Fixed mode must not touch trial_b");
    }
}
