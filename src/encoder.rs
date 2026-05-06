//! Test-only MagicYUV v7 frame synthesiser.
//!
//! This module exists so the round-1 decoder can self-roundtrip
//! against bytes our own encoder writes — the cleanroom Auditor will
//! cross-validate against the proprietary binary in round 2 once the
//! trace contract is wired. Until then, self-roundtrip catches every
//! decoder ↔ encoder symmetry bug.
//!
//! The encoder writes every byte from spec, **not** by reading any
//! reference codec source. The implementation here mirrors the same
//! `spec/01..05` chapters the decoder consumes — wall-isolation
//! preserved.
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
//! - Huffman OR raw-mode payloads.
//! - RGB inter-plane decorrelation (B' = B - G, R' = R - G) when the
//!   family is RGB / RGBA per spec/03 §4 audit-corrected note.
//!
//! Out of scope: Dynamic encoder strategy, interlaced fields, cross-
//! plane optimisations, byte-budget raw-fallback heuristic. The
//! encoder accepts a `slice_mode` parameter and honours it
//! unconditionally; round-1 tests run all-three-predictors and both
//! modes (Huffman / raw) explicitly to exercise the decoder paths.

#![cfg(test)]

use crate::header::{HEADER_SIZE, MAGY_MAGIC};
use crate::tables::{Family, FourccRecord, PredictorKind};

/// Build-length-limited Huffman; we cap at 12 to mirror the
/// `HuffCoderT<…, 8, 12, 12>` constraint (`spec/05` §1.5). This is
/// just an internal cap — the decoder also rejects anything beyond
/// 12.
const MAX_HUFF_LEN_8BIT: u8 = 12;

/// Encoder-side per-plane Huffman: builds canonical lengths from a
/// histogram with the same length-descending cumulative algorithm
/// the decoder uses, then assigns codes via the same procedure.
struct PlaneHuff {
    lengths: Vec<u8>,
    codes: Vec<u32>,
}

impl PlaneHuff {
    fn build_from_histogram(hist: &[u32; 256]) -> Self {
        // Build a length-limited canonical Huffman code from the
        // histogram. Algorithm: package-merge would be ideal; we
        // start with frequency-sorted Huffman, then if the maximum
        // length exceeds 12 we cheaply enforce the cap by clamping
        // the longest lengths down and assigning the remainder
        // (length-12) to the rest — sufficient for the small test
        // alphabets we generate (each test image's residual
        // distribution has ≤ 256 distinct symbols, plenty of headroom
        // before length-12 saturates).
        //
        // For round-1 fixtures that are all-zero or simple ramps the
        // natural lengths are well under 12; we don't need a
        // length-limiter.
        let lengths = canonical_huffman_lengths(hist);
        // Sanity-check Kraft inequality — we treat overshoots by
        // forcing a fall-back assignment that the decoder accepts
        // (length-12 for the longest tail; rare in test fixtures).
        let kraft_ok = kraft_sum_le_one(&lengths);
        debug_assert!(kraft_ok, "encoder built non-canonical Huffman lengths");

        let max_len = *lengths.iter().max().unwrap_or(&0);
        debug_assert!(
            max_len <= MAX_HUFF_LEN_8BIT,
            "encoder produced length > 12 (got {max_len})"
        );

        let mut codes = vec![0u32; 256];
        if max_len > 0 {
            // Mirror the decoder's spec/05 §2.0 algorithm exactly so
            // that decoded-symbol bit patterns match what the encoder
            // writes.
            let mut bl_count = vec![0u32; (max_len as usize) + 2];
            for &l in &lengths {
                bl_count[l as usize] += 1;
            }
            let mut start = vec![0u32; (max_len as usize) + 2];
            let mut acc: u32 = 0xffff_ffff;
            for len in (1..=(max_len as u32)).rev() {
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
        let _ = max_len;
        Self { lengths, codes }
    }
}

fn kraft_sum_le_one(lengths: &[u8]) -> bool {
    // Sum 2^-L; allow tiny floating-point drift.
    let mut s: f64 = 0.0;
    for &l in lengths {
        if l > 0 {
            s += 2f64.powi(-(l as i32));
        }
    }
    s <= 1.000_000_001
}

/// Build canonical-Huffman lengths from a symbol histogram via
/// Huffman's tree-construction algorithm + canonical conversion.
/// Length-limiting (cap at 12) is done by a simple "pull lengths
/// down" pass when needed. For simple test inputs the natural
/// lengths are already ≤ 12 by construction.
fn canonical_huffman_lengths(hist: &[u32; 256]) -> Vec<u8> {
    // Standard Huffman: build a tree of (frequency, node) pairs,
    // repeatedly fuse the two smallest frequencies. The implementation
    // is a small priority queue — sufficient for 256-symbol alphabets.
    #[derive(Clone, Copy)]
    struct Node {
        freq: u64,
        // For tie-breaks: smaller `tag` wins. Symbols come first (by
        // index), then merged nodes (by creation order).
        tag: u32,
        // Left/right child indices into `nodes`. -1 for leaves.
        left: i32,
        right: i32,
        symbol: i16,
    }
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
            symbol: s as i16,
        });
        heap.push((nodes.len() - 1) as u32);
    }
    if nodes.is_empty() {
        return vec![0u8; 256];
    }
    if nodes.len() == 1 {
        // Single-symbol alphabet — give it length 1 anyway. The
        // decoder accepts a length-1 unused codespace; the resulting
        // Kraft is 1/2, but the empty bitstream never references it.
        let mut out = vec![0u8; 256];
        let only = nodes[0].symbol as usize;
        out[only] = 1;
        return out;
    }
    let lt = |a: &Node, b: &Node| a.freq < b.freq || (a.freq == b.freq && a.tag < b.tag);
    let mut next_tag: u32 = 0x1000_0000;
    while heap.len() > 1 {
        // Find the two smallest entries.
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
    let mut lengths = vec![0u8; 256];
    fn walk(nodes: &[Node], idx: usize, depth: u8, lengths: &mut [u8]) {
        let n = &nodes[idx];
        if n.symbol >= 0 {
            lengths[n.symbol as usize] = depth.max(1);
            return;
        }
        if n.left >= 0 {
            walk(nodes, n.left as usize, depth + 1, lengths);
        }
        if n.right >= 0 {
            walk(nodes, n.right as usize, depth + 1, lengths);
        }
    }
    walk(&nodes, root, 0, &mut lengths);
    // Length-limit to ≤ 12. Simple iterative repair: while any length
    // > 12, find the longest and a partner length-> 12 ↔ swap a
    // length-1-deeper unused codespace. For simple test fixtures this
    // never fires.
    enforce_length_cap(&mut lengths, MAX_HUFF_LEN_8BIT);
    lengths
}

/// Hard cap: if any code exceeds `cap`, re-derive lengths by
/// distributing the unused codespace. Cheap repair sufficient for
/// the round-1 test fixtures (random small images).
fn enforce_length_cap(lengths: &mut [u8], cap: u8) {
    while let Some(over) = lengths.iter().copied().max() {
        if over <= cap {
            break;
        }
        // Find the symbol with the longest length and drop it by 1;
        // find the symbol with the smallest non-zero length (other
        // than that one) and increase it by 1. This is the classic
        // length-limited Huffman repair step.
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
/// `spec/05` §1.1. The encoder uses runs ≥ 2; runs of 1 are emitted
/// as literals. Run counts cap at 255 (we use 254 to leave headroom
/// matching the proprietary encoder's `cmp $0xfe` cap; spec/05 §1.5).
fn encode_descriptor(lengths: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lengths.len() {
        let v = lengths[i];
        // Find the run of equal values.
        let mut run = 1;
        while i + run < lengths.len() && lengths[i + run] == v && run < 255 {
            run += 1;
        }
        if run >= 2 {
            // 2..256 repetitions encoded as one byte (0x80|v) plus a
            // count byte (run - 1), capped at 255.
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

/// Predictor encoder: subtract the predictor in-place to produce
/// residuals (the inverse of `predict::apply_*`). Used by the test
/// encoder before Huffman / raw packing.
fn encode_predictor_u8(kind: PredictorKind, data: &mut [u8], rows: usize, width: usize) {
    if rows == 0 || width == 0 {
        return;
    }
    match kind {
        PredictorKind::Left => {
            // Bottom-up & right-to-left so we don't overwrite needed
            // neighbours.
            for r in (0..rows).rev() {
                let row_off = r * width;
                for c in (1..width).rev() {
                    data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                }
                if r >= 1 {
                    let prev0 = data[(r - 1) * width];
                    data[row_off] = data[row_off].wrapping_sub(prev0);
                }
            }
        }
        PredictorKind::Gradient => {
            for r in (0..rows).rev() {
                let row_off = r * width;
                if r == 0 {
                    for c in (1..width).rev() {
                        data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                    }
                } else {
                    let prev_off = (r - 1) * width;
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
                if r == 0 {
                    for c in (1..width).rev() {
                        data[row_off + c] = data[row_off + c].wrapping_sub(data[row_off + c - 1]);
                    }
                } else {
                    let prev_off = (r - 1) * width;
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

/// MSB-first bit writer mirroring the decoder's BitReader convention.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits buffered in the current byte's high bits (`bits_used` from
    /// 0..7).
    cur: u8,
    bits_used: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cur: 0,
            bits_used: 0,
        }
    }
    fn write(&mut self, code: u32, len: u8) {
        // Bits are written MSB-first: bit (len - 1) of `code` goes in
        // first.
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
    fn finish(mut self) -> Vec<u8> {
        if self.bits_used > 0 {
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

/// Per-slice mode the test encoder offers. Round-1 tests cover both.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SliceMode {
    Huffman,
    Raw,
}

/// Top-level: synthesise a v7 MAGY frame from per-plane pixel buffers.
///
/// `planes` is given in the format-byte's family order (G/B/R[/A] for
/// RGB-family; Y/U/V[/A] for YUV; Y for Gray). The encoder applies
/// the spec/03 §4 RGB decorrelation transform internally for RGB
/// families, so callers pass user-facing pixel bytes.
pub(crate) fn encode_frame(
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    planes: Vec<Vec<u8>>,
    predictor: PredictorKind,
    slice_mode: SliceMode,
) -> Vec<u8> {
    let num_planes = rec.planes as usize;
    debug_assert_eq!(planes.len(), num_planes);

    let w = width as usize;
    let h = height as usize;
    let sh = slice_height as usize;
    let slices_per_plane = h.div_ceil(sh);
    let total_slices = num_planes * slices_per_plane;

    // Apply RGB decorrelation if needed to produce wire-order planes.
    let mut wire_planes: Vec<Vec<u8>> = match rec.family {
        Family::Rgb | Family::Rgba => apply_rgb_decorrelation(planes),
        Family::Yuv | Family::Yuva | Family::Gray => planes,
    };

    // Per-plane geometry.
    let plane_dims: Vec<(usize, usize, usize)> = (0..num_planes)
        .map(|p| {
            let (sub_x, sub_y) = match rec.family {
                Family::Yuv if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                Family::Yuva if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                _ => (1usize, 1usize),
            };
            let pw = w / sub_x;
            let ph = h / sub_y;
            let psh = sh / sub_y;
            (pw, ph, psh)
        })
        .collect();

    // Build per-plane residuals (predictor-applied) and histograms.
    // We also slice them up so we can emit per-slice bytes later.
    let mut plane_resid: Vec<Vec<u8>> = Vec::with_capacity(num_planes);
    for (p, plane_data) in wire_planes.drain(..).enumerate() {
        let (pw, ph, _psh) = plane_dims[p];
        let slices_pp_p = slices_per_plane;
        let plane_slice_height = plane_dims[p].2;

        let mut residuals: Vec<u8> = Vec::with_capacity(pw * ph);
        for s in 0..slices_pp_p {
            let row_start = s * plane_slice_height;
            let row_end = ((s + 1) * plane_slice_height).min(ph);
            let slice_rows = row_end - row_start;
            let mut block: Vec<u8> = plane_data[row_start * pw..row_end * pw].to_vec();
            encode_predictor_u8(predictor, &mut block, slice_rows, pw);
            residuals.extend_from_slice(&block);
        }
        plane_resid.push(residuals);
    }

    // Per-plane Huffman tables for Huffman mode. (Even in Raw mode we
    // still write a Huffman descriptor so the decoder's preamble walk
    // succeeds; the descriptor will be unused when slice_flags & 0x01.)
    let plane_huffs: Vec<PlaneHuff> = plane_resid
        .iter()
        .map(|res| {
            let mut hist = [0u32; 256];
            for &b in res {
                hist[b as usize] += 1;
            }
            // Ensure at least one symbol has a positive length even if
            // the residual stream is empty (0-pixel slice).
            if res.is_empty() {
                hist[0] = 1;
            }
            PlaneHuff::build_from_histogram(&hist)
        })
        .collect();

    // Build payload buffers for each slice.
    let mut slice_payloads: Vec<Vec<u8>> = Vec::with_capacity(total_slices);
    for s in 0..total_slices {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;
        let (pw, ph, plane_slice_height) = plane_dims[plane];
        let row_start = in_plane_idx * plane_slice_height;
        let row_end = ((in_plane_idx + 1) * plane_slice_height).min(ph);
        let slice_rows = row_end - row_start;

        let res_block = &plane_resid[plane][row_start * pw..row_end * pw];

        let mut payload = Vec::new();
        // Slice prefix bytes (spec/04 §1).
        let flags: u8 = match slice_mode {
            SliceMode::Huffman => 0x00,
            SliceMode::Raw => 0x01,
        };
        let pred_id: u8 = match predictor {
            PredictorKind::Left => 0x01,
            PredictorKind::Gradient => 0x02,
            PredictorKind::Median => 0x03,
        };
        payload.push(flags);
        payload.push(pred_id);

        match slice_mode {
            SliceMode::Raw => {
                // Raw bytes — one byte per residual (8-bit family).
                payload.extend_from_slice(res_block);
                let _ = slice_rows; // shadow unused-variable for raw branch
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

    // Assemble preamble: plane_count + per_slice_plane_index + huff
    // descriptors.
    let mut preamble = Vec::new();
    preamble.push(num_planes as u8);
    for s in 0..total_slices {
        preamble.push((s / slices_per_plane) as u8);
    }
    for huff in &plane_huffs {
        preamble.extend(encode_descriptor(&huff.lengths));
    }

    // Slice table: (total_slices + 1) u32 LE entries. entry[k] = byte
    // offset of slice (k-1)'s start, relative to byte after header.
    // In v2.4.2 entry[0] == entry[1] (per spec/02 §5.3); we do the
    // same.
    let table_bytes = 4 * (total_slices + 1);
    let mut entries = vec![0u32; total_slices + 1];
    let preamble_off = table_bytes; // bytes from start-of-table.
    let mut running = preamble_off + preamble.len();
    entries[1] = preamble_off as u32 + preamble.len() as u32;
    entries[0] = entries[1];
    // entries[2..=total_slices] = end of each slice (== start of next)
    for s in 0..total_slices {
        running += slice_payloads[s].len();
        if s + 1 < total_slices {
            entries[s + 2] =
                (running - slice_payloads[s].len()) as u32 + slice_payloads[s].len() as u32;
            // entry[s+2] is the byte offset of slice s+1, which is
            // exactly `running` after adding slice s's length.
            entries[s + 2] = running as u32;
        }
    }
    // Note: entries[total_slices] = byte offset of last slice's start,
    // computed = preamble_off + preamble.len() + sum of slices[0..N-1].
    // The last slice extends to file_end.
    // Recompute cleanly:
    let mut off = preamble_off + preamble.len();
    entries[1] = off as u32;
    entries[0] = entries[1];
    for s in 0..total_slices {
        // entry[s+1] = start of slice s.
        entries[s + 1] = off as u32;
        off += slice_payloads[s].len();
    }

    // Build the final frame: header || slice_table || preamble ||
    // payloads.
    let mut out = Vec::new();
    write_header(rec, width, height, slice_height, &mut out);
    debug_assert_eq!(out.len(), HEADER_SIZE);
    for &e in &entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out.extend_from_slice(&preamble);
    for p in &slice_payloads {
        out.extend_from_slice(p);
    }
    // Even-byte padding (spec/02 §8).
    if out.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn write_header(rec: FourccRecord, width: u32, height: u32, slice_height: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&MAGY_MAGIC);
    out.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    out.push(7); // version
    out.push(rec.format_byte);
    out.push(rec.aux_byte);
    out.push(0x02); // codec_variant — v2.4.2 always emits 0x02.
    out.extend_from_slice(&0u32.to_le_bytes()); // flags = 0
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes()); // width_extra
    out.extend_from_slice(&slice_height.to_le_bytes());
}

/// Apply spec/03 §4 audit-corrected wire decorrelation: input is
/// user-facing G/B/R[/A] order; output is wire G/B/R[/A] but with
/// `B' = B - G mod 256, R' = R - G mod 256` and the wire-order swap
/// to `(B', G, R')[, A]`. We then write planes in that wire order.
fn apply_rgb_decorrelation(planes: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
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
    // Wire order is (B', G, R')[, A].
    let mut out = vec![b, g, r];
    if let Some(a) = a_opt {
        out.push(a);
    }
    out
}
