//! Public MagicYUV v7 frame encoder.
//!
//! This is a **clean-room** encoder written from `spec/01..06`.  It
//! does NOT chase the proprietary v2.4.2 encoder's exact byte output
//! (notably its "Dynamic" predictor strategy and its byte-budget raw
//! fallback heuristic) — those are encoder-side conventions, not
//! wire-format requirements.  What it does guarantee is that every
//! emitted frame is a well-formed v7 stream that round-trips through
//! [`crate::decode_frame`] byte-for-byte.
//!
//! Coverage:
//! - Header per spec/01 §3 (audit-corrected aux_byte values).
//! - Slice table + plane-major preamble per spec/02 §5..§7.
//! - Per-plane Huffman descriptors (run-length encoded) per spec/05
//!   §1.1, with codes assigned via the longest-length-first
//!   cumulative algorithm of spec/05 §2.0 — same algorithm the
//!   decoder uses, so the resulting (sym, code, length) triples are
//!   self-consistent.
//! - One predictor per slice (caller picks Left / Gradient / Median).
//! - Huffman OR raw-mode payloads. Raw mode at 8-bit is byte-per-
//!   sample; at 10/12/14-bit it is bit-packed at `bits` bits per
//!   sample (`spec/05` §4.1).
//! - RGB inter-plane decorrelation (B' = B - G, R' = R - G) when the
//!   family is RGB / RGBA per spec/03 §4 audit-corrected note.
//! - Interlaced field-stride=2 prediction per spec/04 §5.1.
//!
//! Out of scope: the proprietary "Dynamic" strategy (a strict
//! min-residual-sum picker), byte-budget raw-fallback heuristics,
//! cross-plane optimisations.  The caller passes the predictor and
//! mode explicitly.

use crate::error::{Error, Result};
use crate::header::{FLAG_INTERLACED, HEADER_SIZE, MAGY_MAGIC};
use crate::predict::FieldStride;
use crate::tables::{Family, FourccRecord, PredictorKind};

/// Per-slice mode the encoder offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceMode {
    /// Huffman-coded residuals (`slice_flags = 0x00`).
    Huffman,
    /// Raw post-prediction residuals (`slice_flags = 0x01`).
    Raw,
}

/// High-level encode options.
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    pub predictor: PredictorKind,
    pub mode: SliceMode,
    /// Set the header `flags & FLAG_INTERLACED` bit and emit
    /// field-stride=2 prediction per `spec/04` §5.1.
    pub interlaced: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            predictor: PredictorKind::Gradient,
            mode: SliceMode::Huffman,
            interlaced: false,
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
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    bits_used: u32,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cur: 0,
            bits_used: 0,
        }
    }
    pub(crate) fn write(&mut self, code: u32, len: u8) {
        for i in (0..len).rev() {
            let bit = ((code >> i) & 1) as u8;
            self.cur |= bit << (7 - self.bits_used);
            self.bits_used += 1;
            if self.bits_used == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.bits_used = 0;
            }
        }
    }
    pub(crate) fn finish(mut self) -> Vec<u8> {
        if self.bits_used > 0 {
            self.bytes.push(self.cur);
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
    match kind {
        PredictorKind::Left => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
                if r >= header_rows {
                    let prev0 = data[(r - fs) * width];
                    data[row_off] = data[row_off].wrapping_sub(prev0);
                }
            }
        }
        PredictorKind::Gradient => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                if r < header_rows {
                    for c in (1..width).rev() {
                        data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                    }
                } else {
                    let prev_off = (r - fs) * width;
                    for c in (1..width).rev() {
                        let left = data[row_off + c - 1];
                        let top = data[prev_off + c];
                        let top_left = data[prev_off + c - 1];
                        let pred = left.wrapping_add(top).wrapping_sub(top_left);
                        data[row_off + c] = data[row_off + c].wrapping_sub(pred);
                    }
                    data[row_off] = data[row_off].wrapping_sub(data[prev_off]);
                }
            }
        }
        PredictorKind::Median => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                if r < header_rows {
                    for c in (1..width).rev() {
                        data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                    }
                } else {
                    let prev_off = (r - fs) * width;
                    for c in (1..width).rev() {
                        let left = data[row_off + c - 1];
                        let top = data[prev_off + c];
                        let top_left = data[prev_off + c - 1];
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
                        data[row_off + c] = data[row_off + c].wrapping_sub(pred);
                    }
                    data[row_off] = data[row_off].wrapping_sub(data[prev_off]);
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

    // Build per-plane residuals via predictor-encode.
    let mut plane_resid: Vec<Vec<u8>> = Vec::with_capacity(num_planes);
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u8> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let mut block: Vec<u8> = plane_data[row_start * pw..row_end * pw].to_vec();
            encode_predictor_u8(options.predictor, &mut block, slice_rows, pw, field_stride);
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
    for s in 0..total_slices {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;
        let (pw, ph, plane_slice_height) = plane_dims[plane];
        let row_start = in_plane_idx * plane_slice_height;
        let row_end = ((in_plane_idx + 1) * plane_slice_height).min(ph);
        let _ = row_end;

        let res_block = &plane_resid[plane][row_start * pw..row_end * pw];

        let mut payload = Vec::new();
        let flags: u8 = match options.mode {
            SliceMode::Huffman => 0x00,
            SliceMode::Raw => 0x01,
        };
        let pred_id: u8 = match options.predictor {
            PredictorKind::Left => 0x01,
            PredictorKind::Gradient => 0x02,
            PredictorKind::Median => 0x03,
        };
        payload.push(flags);
        payload.push(pred_id);

        match options.mode {
            SliceMode::Raw => {
                payload.extend_from_slice(res_block);
            }
            SliceMode::Huffman => {
                let huff = &plane_huffs[plane];
                let mut bw = BitWriter::new();
                for &sym in res_block {
                    let len = huff.lengths[sym as usize];
                    let code = huff.codes[sym as usize];
                    bw.write(code, len);
                }
                payload.extend(bw.finish());
            }
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
    match kind {
        PredictorKind::Left => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                for c in (1..width).rev() {
                    data[row_off + c] =
                        (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
                }
                if r >= header_rows {
                    let prev0 = data[(r - fs) * width];
                    data[row_off] = (data[row_off].wrapping_sub(prev0)) & mask;
                }
            }
        }
        PredictorKind::Gradient => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                if r < header_rows {
                    for c in (1..width).rev() {
                        data[row_off + c] =
                            (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
                    }
                } else {
                    let prev_off = (r - fs) * width;
                    for c in (1..width).rev() {
                        let left = data[row_off + c - 1];
                        let top = data[prev_off + c];
                        let top_left = data[prev_off + c - 1];
                        let pred = left.wrapping_add(top).wrapping_sub(top_left);
                        data[row_off + c] = (data[row_off + c].wrapping_sub(pred)) & mask;
                    }
                    data[row_off] = (data[row_off].wrapping_sub(data[prev_off])) & mask;
                }
            }
        }
        PredictorKind::Median => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                if r < header_rows {
                    for c in (1..width).rev() {
                        data[row_off + c] =
                            (data[row_off + c].wrapping_sub(data[row_off + c - 1])) & mask;
                    }
                } else {
                    let prev_off = (r - fs) * width;
                    for c in (1..width).rev() {
                        let left = data[row_off + c - 1];
                        let top = data[prev_off + c];
                        let top_left = data[prev_off + c - 1];
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
                        data[row_off + c] = (data[row_off + c].wrapping_sub(pred)) & mask;
                    }
                    data[row_off] = (data[row_off].wrapping_sub(data[prev_off])) & mask;
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
    for p in 0..num_planes {
        let (pw, ph, plane_slice_height) = plane_dims[p];
        let mut residuals: Vec<u16> = Vec::with_capacity(pw * ph);
        let plane_data = std::mem::take(&mut wire_planes[p]);
        for s in 0..slices_per_plane {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let mut block: Vec<u16> = plane_data[row_start * pw..row_end * pw].to_vec();
            encode_predictor_u16(
                options.predictor,
                &mut block,
                slice_rows,
                pw,
                mask,
                field_stride,
            );
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
    for s in 0..total_slices {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;
        let (pw, ph, plane_slice_height) = plane_dims[plane];
        let _ = ph;
        let row_start = in_plane_idx * plane_slice_height;
        let row_end = ((in_plane_idx + 1) * plane_slice_height).min(plane_dims[plane].1);

        let res_block = &plane_resid[plane][row_start * pw..row_end * pw];

        let mut payload = Vec::new();
        let flags: u8 = match options.mode {
            SliceMode::Huffman => 0x00,
            SliceMode::Raw => 0x01,
        };
        let pred_id: u8 = match options.predictor {
            PredictorKind::Left => 0x01,
            PredictorKind::Gradient => 0x02,
            PredictorKind::Median => 0x03,
        };
        payload.push(flags);
        payload.push(pred_id);

        match options.mode {
            SliceMode::Raw => {
                // Bit-pack at `bits` bits MSB-first.
                let mut bw = BitWriter::new();
                for &sym in res_block {
                    bw.write(sym as u32, bits);
                }
                payload.extend(bw.finish());
            }
            SliceMode::Huffman => {
                let huff = &plane_huffs[plane];
                let mut bw = BitWriter::new();
                for &sym in res_block {
                    let len = huff.lengths[sym as usize];
                    let code = huff.codes[sym as usize];
                    bw.write(code, len);
                }
                payload.extend(bw.finish());
            }
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

// ─────────────────────────── AVI envelope ──────────────────────────

/// Wrap one or more pre-encoded MAGY frames into a single-RIFF AVI
/// container per `spec/06`. The function writes a minimal `hdrl` /
/// `strl` / `strf` (with the first frame's MAGY header as `strf`
/// extradata, byte-identical per spec/06 §4.1) and a `movi` LIST
/// containing each frame as a `00dc` chunk.
pub fn encode_avi(rec: FourccRecord, width: u32, height: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    assert!(!frames.is_empty(), "encode_avi: at least one frame needed");

    let mut bih = Vec::new();
    bih.extend_from_slice(&72u32.to_le_bytes()); // biSize
    bih.extend_from_slice(&width.to_le_bytes());
    bih.extend_from_slice(&height.to_le_bytes());
    bih.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    bih.extend_from_slice(&((rec.bit_depth as u16) * 3).to_le_bytes()); // biBitCount approx
    bih.extend_from_slice(&rec.fourcc); // biCompression
    bih.extend_from_slice(&(frames[0].len() as u32).to_le_bytes()); // biSizeImage
    bih.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
    bih.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
    bih.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    bih.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    let mut strf_payload = bih;
    strf_payload.extend_from_slice(&frames[0][..HEADER_SIZE]);

    let mut strl_payload = Vec::new();
    strl_payload.extend_from_slice(b"strl");
    strl_payload.extend_from_slice(b"strf");
    strl_payload.extend_from_slice(&(strf_payload.len() as u32).to_le_bytes());
    strl_payload.extend_from_slice(&strf_payload);
    if strf_payload.len() % 2 == 1 {
        strl_payload.push(0);
    }

    let mut hdrl_payload = Vec::new();
    hdrl_payload.extend_from_slice(b"hdrl");
    hdrl_payload.extend_from_slice(b"LIST");
    hdrl_payload.extend_from_slice(&(strl_payload.len() as u32).to_le_bytes());
    hdrl_payload.extend_from_slice(&strl_payload);

    let mut movi_payload = Vec::new();
    movi_payload.extend_from_slice(b"movi");
    for f in frames {
        movi_payload.extend_from_slice(b"00dc");
        movi_payload.extend_from_slice(&(f.len() as u32).to_le_bytes());
        movi_payload.extend_from_slice(f);
        if f.len() % 2 == 1 {
            movi_payload.push(0);
        }
    }

    let mut riff_body = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(hdrl_payload.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&hdrl_payload);
    riff_body.extend_from_slice(b"LIST");
    riff_body.extend_from_slice(&(movi_payload.len() as u32).to_le_bytes());
    riff_body.extend_from_slice(&movi_payload);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}
