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
use crate::header::{FLAG_INTERLACED, HEADER_SIZE, MAGY_MAGIC};
use crate::predict::FieldStride;
use crate::tables::{Family, FourccRecord, PredictorKind};

/// Per-slice Huffman / raw mode the encoder offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// **Deprecated, kept for source compatibility.** When `strategy`
    /// is `Fixed(_)`, this field is ignored. Callers should set
    /// `strategy = PredictorStrategy::Fixed(predictor)` instead.
    pub predictor: PredictorKind,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Huffman,
            interlaced: false,
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
            predictor: PredictorKind::Gradient,
        }
    }

    /// Helper: shorthand for `Fixed(p)` with the given predictor.
    pub fn fixed(p: PredictorKind) -> Self {
        Self {
            strategy: PredictorStrategy::Fixed(p),
            mode: SliceMode::Huffman,
            interlaced: false,
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
    p
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
    codes: Vec<u32>,
}

impl PlaneHuff {
    fn build_from_histogram(hist: &[u32], max_len: u8) -> Self {
        let lengths = canonical_huffman_lengths(hist, max_len);
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
        Self { lengths, codes }
    }
}

fn canonical_huffman_lengths(hist: &[u32], max_len: u8) -> Vec<u8> {
    #[derive(Clone, Copy)]
    struct Node {
        freq: u64,
        tag: u32,
        left: i32,
        right: i32,
        symbol: i32,
    }
    let n = hist.len();
    let mut nodes: Vec<Node> = Vec::new();
    let mut heap: Vec<u32> = Vec::new();
    for (s, &c) in hist.iter().enumerate() {
        if c == 0 {
            continue;
        }
        nodes.push(Node {
            freq: c as u64,
            tag: s as u32,
            left: -1,
            right: -1,
            symbol: s as i32,
        });
        heap.push((nodes.len() - 1) as u32);
    }
    if nodes.is_empty() {
        return vec![0u8; n];
    }
    if nodes.len() == 1 {
        let mut out = vec![0u8; n];
        let only = nodes[0].symbol as usize;
        out[only] = 1;
        return out;
    }
    let lt = |a: &Node, b: &Node| a.freq < b.freq || (a.freq == b.freq && a.tag < b.tag);
    let mut next_tag: u32 = 0x1000_0000;
    while heap.len() > 1 {
        heap.sort_by(|&i, &j| {
            let na = &nodes[i as usize];
            let nb = &nodes[j as usize];
            if lt(na, nb) {
                std::cmp::Ordering::Less
            } else if lt(nb, na) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let a = heap.remove(0);
        let b = heap.remove(0);
        let an = nodes[a as usize];
        let bn = nodes[b as usize];
        nodes.push(Node {
            freq: an.freq + bn.freq,
            tag: next_tag,
            left: a as i32,
            right: b as i32,
            symbol: -1,
        });
        next_tag += 1;
        heap.push((nodes.len() - 1) as u32);
    }
    let root = heap[0] as usize;
    let mut lengths = vec![0u8; n];
    fn walk(nodes: &[Node], idx: usize, depth: u8, lengths: &mut [u8]) {
        let node = &nodes[idx];
        if node.symbol >= 0 {
            lengths[node.symbol as usize] = depth.max(1);
            return;
        }
        if node.left >= 0 {
            walk(nodes, node.left as usize, depth + 1, lengths);
        }
        if node.right >= 0 {
            walk(nodes, node.right as usize, depth + 1, lengths);
        }
    }
    walk(&nodes, root, 0, &mut lengths);
    enforce_length_cap(&mut lengths, max_len);
    lengths
}

fn enforce_length_cap(lengths: &mut [u8], cap: u8) {
    while let Some(over) = lengths.iter().copied().max() {
        if over <= cap {
            break;
        }
        let (i_long, _) = lengths
            .iter()
            .enumerate()
            .filter(|(_, &l)| l > 0)
            .max_by_key(|(_, &l)| l)
            .unwrap();
        let (i_short, _) = lengths
            .iter()
            .enumerate()
            .filter(|(j, &l)| l > 0 && *j != i_long)
            .min_by_key(|(_, &l)| l)
            .unwrap();
        lengths[i_long] -= 1;
        lengths[i_short] += 1;
    }
}

/// Encode a sequence of code lengths using the run-length scheme of
/// `spec/05` §1.1.
fn encode_descriptor(lengths: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lengths.len() {
        let v = lengths[i];
        let mut run = 1usize;
        while i + run < lengths.len() && lengths[i + run] == v && run < 256 {
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
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Bit accumulator; valid bits are the high `bits_used` bits.
    acc: u64,
    bits_used: u32,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
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
                })
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
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u8> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let src = &plane_data[row_start * pw..row_end * pw];
            let (chosen, block) =
                build_slice_residuals_u8(strategy, src, slice_rows, pw, field_stride);
            slice_predictors[p * slices_per_plane + s] = chosen;
            residuals.extend_from_slice(&block);
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
                let mut bw = BitWriter::new();
                for &sym in res_block {
                    let len = huff.lengths[sym as usize];
                    let code = huff.codes[sym as usize];
                    bw.write(code, len);
                }
                let bytes = bw.finish();
                if bytes.len() <= raw_size {
                    huff_buf = Some(bytes);
                    0x00
                } else {
                    0x01
                }
            }
        };

        let mut payload = Vec::new();
        payload.push(flags);
        payload.push(pred_id);

        if flags & 0x01 != 0 {
            payload.extend_from_slice(res_block);
        } else if let Some(buf) = huff_buf {
            payload.extend(buf);
        } else {
            let huff = &plane_huffs[plane];
            let mut bw = BitWriter::new();
            for &sym in res_block {
                let len = huff.lengths[sym as usize];
                let code = huff.codes[sym as usize];
                bw.write(code, len);
            }
            payload.extend(bw.finish());
        }
        slice_payloads.push(payload);
    }

    Ok(assemble_frame(
        rec,
        width,
        height,
        slice_height,
        options.interlaced,
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
fn build_slice_residuals_u8(
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
                })
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
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u16> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let src = &plane_data[row_start * pw..row_end * pw];
            let (chosen, block) =
                build_slice_residuals_u16(strategy, src, slice_rows, pw, mask, bits, field_stride);
            slice_predictors[p * slices_per_plane + s] = chosen;
            residuals.extend_from_slice(&block);
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
                let mut bw = BitWriter::new();
                for &sym in res_block {
                    let len = huff.lengths[sym as usize];
                    let code = huff.codes[sym as usize];
                    bw.write(code, len);
                }
                let bytes = bw.finish();
                if bytes.len() <= raw_size {
                    huff_buf = Some(bytes);
                    0x00
                } else {
                    0x01
                }
            }
        };

        let mut payload = Vec::new();
        payload.push(flags);
        payload.push(pred_id);

        if flags & 0x01 != 0 {
            // Bit-pack at `bits` bits MSB-first.
            let mut bw = BitWriter::new();
            for &sym in res_block {
                bw.write(sym as u32, bits);
            }
            payload.extend(bw.finish());
        } else if let Some(buf) = huff_buf {
            payload.extend(buf);
        } else {
            let huff = &plane_huffs[plane];
            let mut bw = BitWriter::new();
            for &sym in res_block {
                let len = huff.lengths[sym as usize];
                let code = huff.codes[sym as usize];
                bw.write(code, len);
            }
            payload.extend(bw.finish());
        }
        slice_payloads.push(payload);
    }

    Ok(assemble_frame(
        rec,
        width,
        height,
        slice_height,
        options.interlaced,
        total_slices,
        slices_per_plane,
        &plane_huffs,
        &slice_payloads,
    ))
}

/// `u16` analogue of [`build_slice_residuals_u8`]: pick a predictor
/// (per `strategy`) and emit its residuals. For Dynamic, the score is
/// `sum |signed(residual)|` interpreting each residual as a signed
/// value in the `bits`-bit window — equivalent to
/// `min(r, (1 << bits) - r)`. Same monotone-with-Huffman-cost
/// rationale as the u8 case.
#[allow(clippy::too_many_arguments)]
fn build_slice_residuals_u16(
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
    let pw = width / sub_x;
    let ph = height / sub_y;
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
    write_header(rec, width, height, slice_height, interlaced, &mut out);
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

fn write_header(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    interlaced: bool,
    out: &mut Vec<u8>,
) {
    out.extend_from_slice(&MAGY_MAGIC);
    out.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    out.push(7); // version
    out.push(rec.format_byte);
    out.push(rec.aux_byte);
    out.push(0x02); // codec_variant — v2.4.2 always 0x02 (spec/04 §2).
    let flags: u32 = if interlaced { FLAG_INTERLACED } else { 0 };
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes()); // width_extra
    out.extend_from_slice(&slice_height.to_le_bytes());
}
