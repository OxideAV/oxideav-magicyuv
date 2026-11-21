//! Top-level v7 frame decoder. Walks header → slice table →
//! preamble → per-slice payloads, then dispatches Huffman / raw +
//! predictor reconstruction per `spec/02..05`.
//!
//! The output is one decoded plane buffer per native plane (in the
//! format-byte's family order: G/B/R[/A] for RGB, Y/U/V[/A] for YUV,
//! Y for Gray). For RGB-family streams we apply the
//! `(B + G, G, R + G)` decorrelation reversal per `spec/03` §4
//! validation-corrected note.
//!
//! Round 2 covers the full 8-bit + 10/12/14-bit native FOURCC set
//! plus interlaced field-stride=2 prediction (`flags & 0x02`).

use crate::bitreader::{BitReader, unpack_raw_bits_to_u16};
use crate::error::{Error, Result};
use crate::header::{self, FLAG_INTERLACED, FrameHeader};
use crate::huffman::{self, HuffmanTable};
use crate::predict::{self, FieldStride};
use crate::tables::{self, Family, FourccRecord};

#[cfg(feature = "trace")]
use crate::trace::{Event, Tracer};

/// Per-plane geometry computed from `(width, height, slice_height,
/// FourccRecord)`.
#[derive(Debug, Clone, Copy)]
struct PlaneGeom {
    width: usize,
    height: usize,
    /// Slice-height in this plane's row count. For chroma planes of
    /// 4:2:0 / 4:2:2 this is `header.slice_height / sub_y` per
    /// `spec/04` §6.
    plane_slice_height: usize,
}

/// Result of decoding one v7 frame.
///
/// On the streaming path callers can re-use a single `DecodedFrame`
/// across frames via [`decode_into`] — the plane buffers (`Samples::U8`
/// / `Samples::U16` inner `Vec`s) are re-used in place when the
/// frame geometry matches the previous call, avoiding 4-7 per-frame
/// `Vec` allocations (one per plane + the RGB-decorrelation working
/// copy). Use [`DecodedFrame::default`] (or
/// [`DecodedFrame::empty`]) to seed an empty target for the first call.
#[non_exhaustive]
pub struct DecodedFrame {
    /// Frame width (luma plane width).
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Per-plane decoded buffers, in the format-byte's family order.
    pub planes: Vec<DecodedPlane>,
    /// Parsed header for the caller's diagnostics.
    pub header: FrameHeader,
    /// FOURCC record (family, subsampling, bit depth, …).
    pub record: FourccRecord,
}

impl DecodedFrame {
    /// Set `width`.
    #[must_use]
    pub fn with_width(mut self, width: u32) -> Self {
        self.width = width;
        self
    }

    /// Set `height`.
    #[must_use]
    pub fn with_height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    /// Set `planes`.
    #[must_use]
    pub fn with_planes(mut self, planes: Vec<DecodedPlane>) -> Self {
        self.planes = planes;
        self
    }

    /// Set `header`.
    #[must_use]
    pub fn with_header(mut self, header: FrameHeader) -> Self {
        self.header = header;
        self
    }

    /// Set `record`.
    #[must_use]
    pub fn with_record(mut self, record: FourccRecord) -> Self {
        self.record = record;
        self
    }
}

impl DecodedFrame {
    /// An empty frame placeholder, valid as the target slot for the
    /// first [`decode_into`] call. The fields are populated on the
    /// first decode; subsequent decodes re-use the per-plane storage
    /// when geometry matches.
    pub fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            planes: Vec::new(),
            header: FrameHeader::placeholder(),
            record: FourccRecord::placeholder(),
        }
    }
}

impl Default for DecodedFrame {
    fn default() -> Self {
        Self::empty()
    }
}

/// One reconstructed plane.
#[non_exhaustive]
pub struct DecodedPlane {
    pub width: usize,
    pub height: usize,
    /// Significant bits per sample (8, 10, 12, or 14).
    pub bit_depth: u8,
    /// Storage. 8-bit FOURCCs use [`Samples::U8`]; 10/12/14-bit use
    /// [`Samples::U16`] (with values masked to `(1 << bit_depth) - 1`).
    pub samples: Samples,
}

impl DecodedPlane {
    /// Build `DecodedPlane` from every field, in declaration order.
    pub fn new(width: usize, height: usize, bit_depth: u8, samples: Samples) -> Self {
        Self {
            width,
            height,
            bit_depth,
            samples,
        }
    }
}

/// Per-plane sample container. 8-bit FOURCCs decode to [`Samples::U8`];
/// 10/12/14-bit FOURCCs decode to [`Samples::U16`] with the unused
/// MSBs zero-filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

impl Samples {
    /// `true` if the variant is `U8`.
    pub fn is_u8(&self) -> bool {
        matches!(self, Samples::U8(_))
    }
    /// Borrow as a `[u8]` slice if the variant is `U8`, else `None`.
    pub fn as_u8(&self) -> Option<&[u8]> {
        match self {
            Samples::U8(v) => Some(v),
            _ => None,
        }
    }
    /// Borrow as a `[u16]` slice if the variant is `U16`, else `None`.
    pub fn as_u16(&self) -> Option<&[u16]> {
        match self {
            Samples::U16(v) => Some(v),
            _ => None,
        }
    }
    /// Number of samples.
    pub fn len(&self) -> usize {
        match self {
            Samples::U8(v) => v.len(),
            Samples::U16(v) => v.len(),
        }
    }
    /// `true` if zero samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl DecodedPlane {
    /// Convenience: borrow the 8-bit plane data, panicking if the
    /// plane was high-bit-depth. Round-1 callers used the old
    /// `data: Vec<u8>` field directly; this preserves the ergonomics.
    pub fn data_u8(&self) -> &[u8] {
        match &self.samples {
            Samples::U8(v) => v,
            Samples::U16(_) => panic!("oxideav-magicyuv: plane is high-bit-depth; use as_u16()"),
        }
    }
}

/// Decode a complete v7 MAGY frame from `bytes`, allocating fresh
/// per-plane buffers. Convenience wrapper around [`decode_into`] for
/// one-shot callers.
pub fn decode_frame(bytes: &[u8]) -> Result<DecodedFrame> {
    decode_frame_with(bytes, DecodeOptions::new())
}

/// Decoder knobs. The default is a serial decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Upper bound on the worker threads one frame may use. Slices are
    /// independent (`spec/04` §5.2), so they are decoded on up to this
    /// many scoped threads, the calling thread included. `0` and `1`
    /// both mean serial. The decoded samples are identical at every
    /// value.
    pub threads: usize,
}

impl DecodeOptions {
    /// Serial decode.
    pub const fn new() -> Self {
        Self { threads: 1 }
    }

    /// Set `threads`.
    #[must_use]
    pub const fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// [`decode_frame`] with explicit [`DecodeOptions`].
pub fn decode_frame_with(bytes: &[u8], options: DecodeOptions) -> Result<DecodedFrame> {
    let mut dst = DecodedFrame::empty();
    decode_into_with(bytes, &mut dst, options)?;
    Ok(dst)
}

/// Decode a complete v7 MAGY frame from `bytes` into `dst`, re-using
/// `dst`'s per-plane buffers (`Samples::U8` / `Samples::U16` inner
/// `Vec`s) when the geometry matches. Designed for the streaming hot
/// path where one [`DecodedFrame`] instance is decoded into across
/// many frames: each call avoids the 4-7 per-frame `Vec` allocations
/// that [`decode_frame`] performs (one per plane in `plane_bufs` +
/// one per output [`DecodedPlane`] + the working copy of the G plane
/// used by RGB inter-plane decorrelation reversal).
///
/// On dimension / format-byte change the affected buffers are
/// resized in place via [`Vec::resize`]; the underlying allocation is
/// kept when the new size fits the existing capacity.
///
/// On error `dst` is left in an unspecified-but-valid state — its
/// plane buffers may carry partially-decoded residuals; callers
/// should treat the value as "needs re-decode" after any `Err`
/// return.
pub fn decode_into(bytes: &[u8], dst: &mut DecodedFrame) -> Result<()> {
    decode_into_with(bytes, dst, DecodeOptions::new())
}

/// [`decode_into`] with explicit [`DecodeOptions`].
pub fn decode_into_with(
    bytes: &[u8],
    dst: &mut DecodedFrame,
    options: DecodeOptions,
) -> Result<()> {
    if bytes.len() < header::HEADER_SIZE {
        return Err(Error::Truncated {
            what: "MAGY frame",
            needed: header::HEADER_SIZE,
            have: bytes.len(),
        });
    }

    #[cfg(feature = "trace")]
    let tracer = Tracer::from_env();

    let hdr = header::parse(bytes)?;
    let rec = tables::lookup_round2(hdr.format_byte)?;

    #[cfg(feature = "trace")]
    if let Some(t) = &tracer {
        let fourcc = std::str::from_utf8(&rec.fourcc).unwrap_or("????");
        t.emit(Event::Hdr {
            magic: "MAGY",
            version: bytes[8],
            format_byte: rec.format_byte,
            fourcc,
            aux_byte: hdr.aux_byte,
            codec_variant: hdr.codec_variant,
            flags: hdr.flags,
            width: hdr.width,
            height: hdr.height,
            width_extra: hdr.width_extra,
            slice_height: hdr.slice_height,
            bit_depth: rec.bit_depth,
            family: rec.family_label(),
            subsampling: rec.subsampling_label(),
            planes: rec.planes,
        });
    }

    let width = hdr.width as usize;
    let height = hdr.height as usize;
    let slice_height = hdr.slice_height as usize;
    let slices_per_plane = height.div_ceil(slice_height);
    let num_planes = rec.planes as usize;
    let total_slices = num_planes * slices_per_plane;

    // Reject a `slice_height` the §6 chroma partition can't tile. With
    // `chroma_psh = slice_height / sub_y` (floor) and the chroma plane
    // reusing the luma `slices_per_plane`, an indivisible `slice_height`
    // leaves `slices_per_plane × chroma_psh < chroma_height`, so the
    // bottom chroma rows are never assigned to any slice and would be
    // emitted silently zero-padded. The v2.4.2 encoder only ever writes
    // `slice_height = 28` (divisible by every native `sub_y`), so the
    // indivisible case is a malformed / out-of-spec header; reject it
    // rather than mis-decode (symmetric with the encoder guard and with
    // `OddDimensionForSubsampling`). Inert for `sub_y == 1`.
    if rec.sub_y as u32 > 1 && (hdr.slice_height % rec.sub_y as u32) != 0 {
        return Err(Error::SliceHeightNotDivisibleBySubsampling {
            slice_height: hdr.slice_height,
            factor: rec.sub_y as u32,
        });
    }

    // Slice table: (total_slices + 1) u32 LE entries at offset 0x20.
    let table_off = header::HEADER_SIZE;
    let table_bytes = 4 * (total_slices + 1);
    if bytes.len() < table_off + table_bytes {
        return Err(Error::Truncated {
            what: "slice table",
            needed: table_off + table_bytes,
            have: bytes.len(),
        });
    }
    let mut entries = Vec::with_capacity(total_slices + 1);
    for i in 0..(total_slices + 1) {
        let off = table_off + 4 * i;
        let mut a = [0u8; 4];
        a.copy_from_slice(&bytes[off..off + 4]);
        entries.push(u32::from_le_bytes(a));
    }

    #[cfg(feature = "trace")]
    if let Some(t) = &tracer {
        t.emit(Event::SliceTable {
            entries: &entries,
            total_slices,
            slices_per_plane,
            num_planes,
        });
    }

    // Preamble: bytes [table_off + table_bytes .. 0x20 + entry[1])
    // (per spec/02 §5.1).
    let preamble_start = table_off + table_bytes;
    let preamble_end = (entries[1] as usize)
        .checked_add(table_off)
        .ok_or(Error::Truncated {
            what: "preamble bounds",
            needed: 1,
            have: 0,
        })?;
    if preamble_end < preamble_start || bytes.len() < preamble_end {
        return Err(Error::Truncated {
            what: "preamble",
            needed: preamble_end,
            have: bytes.len(),
        });
    }
    let preamble = &bytes[preamble_start..preamble_end];

    // Preamble byte 0 = plane_count.
    if preamble.is_empty() {
        return Err(Error::Truncated {
            what: "preamble (plane_count byte)",
            needed: 1,
            have: 0,
        });
    }
    let plane_count = preamble[0] as usize;
    if plane_count != num_planes {
        return Err(Error::Truncated {
            what: "preamble plane_count != format-byte planes",
            needed: num_planes,
            have: plane_count,
        });
    }
    if preamble.len() < 1 + total_slices {
        return Err(Error::Truncated {
            what: "preamble per-slice plane-index",
            needed: 1 + total_slices,
            have: preamble.len(),
        });
    }
    let per_slice_plane = &preamble[1..1 + total_slices];
    let (slice_plane_map, scattered) =
        resolve_slice_map(per_slice_plane, num_planes, slices_per_plane)?;

    #[cfg(feature = "trace")]
    if let Some(t) = &tracer {
        t.emit(Event::Preamble {
            plane_count: plane_count as u8,
            per_slice_plane_index: per_slice_plane,
        });
    }

    // Per-plane Huffman descriptors at preamble[1 + total_slices ..].
    let n_symbols = 1usize << rec.bit_depth;
    let max_huff_len = rec.max_huffman_length();
    let mut huff_tables: Vec<HuffmanTable> = Vec::with_capacity(num_planes);
    let mut desc_pos = 1 + total_slices;
    for plane in 0..num_planes {
        let desc_start = desc_pos;
        let (lens, used) =
            huffman::parse_lengths(&preamble[desc_pos..], n_symbols, max_huff_len, plane)?;
        desc_pos += used;
        let table = HuffmanTable::build(lens, plane)?;
        #[cfg(feature = "trace")]
        if let Some(t) = &tracer {
            // Build per-symbol `(symbol, length, code)` triples in
            // symbol-ascending order, matching `audit/02` §4.2 +
            // `audit/03` §2 (Python ref's `huff.used` is a per-symbol
            // map, NOT a bool).
            let lengths = table.lengths();
            let codes = table.codes();
            let mut used_map: Vec<(u32, u8, u32)> = Vec::new();
            for (s, &l) in lengths.iter().enumerate() {
                if l > 0 {
                    used_map.push((s as u32, l, codes[s]));
                }
            }
            t.emit(Event::Huff {
                plane,
                descriptor_bytes: &preamble[desc_start..desc_start + used],
                descriptor_length: used,
                n_symbols,
                max_length: max_huff_len,
                used: &used_map,
            });
        }
        #[cfg(not(feature = "trace"))]
        let _ = desc_start;
        huff_tables.push(table);
    }

    // Any preamble bytes after the last descriptor: emit a
    // preamble_trailing trace event so the Auditor's jq-line-diff
    // catches encoder/decoder mismatches. Per `spec/05` §10 Q6 +
    // `audit/00` §8.8 the canonical schema carries `extra_bytes` as
    // an integer count, matching the Python ref's
    // `len(preamble) - cursor` emission at `frame.py:514`.
    #[cfg(feature = "trace")]
    if let Some(t) = &tracer
        && desc_pos < preamble.len()
    {
        t.emit(Event::PreambleTrailing {
            extra_bytes: preamble.len() - desc_pos,
        });
    }
    let _ = desc_pos;

    // Per-plane geometry.
    let plane_geoms: Vec<PlaneGeom> = (0..num_planes)
        .map(|p| plane_geom(rec, p, width, height, slice_height))
        .collect();

    // Resolve and size-check every slice *before* the plane buffers are
    // sized from the header geometry. See `validate_slices`.
    let slice_ranges = validate_slices(
        bytes,
        &entries,
        table_off,
        total_slices,
        &slice_plane_map,
        &plane_geoms,
        &huff_tables,
        rec,
        scattered,
    )?;

    let interlaced = (hdr.flags & FLAG_INTERLACED) != 0;
    let field_stride = if interlaced {
        FieldStride::INTERLACED
    } else {
        FieldStride::PROGRESSIVE
    };

    // A grant is only worth its thread start-up on frames of some size.
    let samples: usize = plane_geoms.iter().map(|g| g.width * g.height).sum();
    let threads = if samples < MIN_SAMPLES_FOR_THREADS {
        1
    } else {
        options.threads.max(1)
    };

    if rec.is_high_bit_depth() {
        decode_high_bit_depth(
            bytes,
            &slice_ranges,
            &slice_plane_map,
            &plane_geoms,
            &huff_tables,
            rec,
            field_stride,
            #[cfg(feature = "trace")]
            tracer.as_ref(),
            hdr,
            dst,
            threads,
        )
    } else {
        decode_eight_bit(
            bytes,
            &slice_ranges,
            &slice_plane_map,
            &plane_geoms,
            &huff_tables,
            rec,
            field_stride,
            #[cfg(feature = "trace")]
            tracer.as_ref(),
            hdr,
            dst,
            threads,
        )
    }
}

/// Frames with fewer samples than this are decoded serially whatever
/// the grant: starting threads would cost more than the decode.
const MIN_SAMPLES_FOR_THREADS: usize = 1 << 16;

/// One slice's decode work: its payload and the rows it reconstructs.
struct SliceJob<'a, T> {
    /// Position in the slice table.
    slice: usize,
    plane: usize,
    #[cfg_attr(not(feature = "trace"), allow(dead_code))]
    in_plane_idx: usize,
    #[cfg_attr(not(feature = "trace"), allow(dead_code))]
    row_start: usize,
    rows: usize,
    width: usize,
    #[cfg_attr(not(feature = "trace"), allow(dead_code))]
    file_offset: usize,
    payload: &'a [u8],
    out: &'a mut [T],
}

/// What a decoded slice reports back (for the trace tape).
#[cfg_attr(not(feature = "trace"), allow(dead_code))]
struct SliceDone {
    slice: usize,
    plane: usize,
    in_plane_idx: usize,
    row_start: usize,
    rows: usize,
    width: usize,
    file_offset: usize,
    payload_size: usize,
    slice_flags: u8,
    predictor_id: u8,
}

impl<T> SliceJob<'_, T> {
    fn done(&self, slice_flags: u8, predictor_id: u8) -> SliceDone {
        SliceDone {
            slice: self.slice,
            plane: self.plane,
            in_plane_idx: self.in_plane_idx,
            row_start: self.row_start,
            rows: self.rows,
            width: self.width,
            file_offset: self.file_offset,
            payload_size: self.payload.len(),
            slice_flags,
            predictor_id,
        }
    }
}

/// Carve the plane buffers into one disjoint row block per slice and
/// pair each with its payload, in slice-table order.
fn slice_jobs<'a, T>(
    bytes: &'a [u8],
    slice_ranges: &[(usize, usize)],
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    plane_bufs: &'a mut [Vec<T>],
) -> Vec<SliceJob<'a, T>> {
    let slices_per_plane = slice_plane_map.len() / plane_geoms.len().max(1);
    // table position of the i-th slice of each plane
    let mut table_pos = vec![vec![0usize; slices_per_plane]; plane_geoms.len()];
    for (s, &(plane, in_plane_idx)) in slice_plane_map.iter().enumerate() {
        table_pos[plane][in_plane_idx] = s;
    }
    let mut jobs = Vec::with_capacity(slice_plane_map.len());
    for ((plane, buf), g) in plane_bufs.iter_mut().enumerate().zip(plane_geoms) {
        let mut rest = buf.as_mut_slice();
        for (in_plane_idx, &slice) in table_pos[plane].iter().enumerate() {
            let row_start = (in_plane_idx * g.plane_slice_height).min(g.height);
            let row_end = ((in_plane_idx + 1) * g.plane_slice_height).min(g.height);
            let rows = row_end - row_start;
            let (out, tail) = std::mem::take(&mut rest).split_at_mut(rows * g.width);
            rest = tail;
            let (start, end) = slice_ranges[slice];
            jobs.push(SliceJob {
                slice,
                plane,
                in_plane_idx,
                row_start,
                rows,
                width: g.width,
                file_offset: start,
                payload: &bytes[start..end],
                out,
            });
        }
    }
    jobs.sort_unstable_by_key(|j| j.slice);
    jobs
}

/// Hand the per-slice results to the trace tape (in slice order, up to
/// the first failure) and surface the first failure.
fn finish_slices(
    results: Vec<Result<SliceDone>>,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
) -> Result<()> {
    for r in results {
        let _done = r?;
        #[cfg(feature = "trace")]
        if let Some(t) = tracer {
            t.emit(Event::Payload {
                slice: _done.slice,
                plane: _done.plane,
                slice_in_plane: _done.in_plane_idx,
                row_start: _done.row_start,
                row_end: _done.row_start + _done.rows,
                cols: _done.width,
                file_offset: _done.file_offset,
                payload_size: _done.payload_size,
                slice_flags: _done.slice_flags,
                predictor_id: _done.predictor_id,
                mode: if (_done.slice_flags & 0x01) != 0 {
                    "raw"
                } else {
                    "huffman"
                },
                n_pixels: _done.rows * _done.width,
            });
        }
    }
    Ok(())
}

/// Resolve the preamble's `total_slices` index bytes into, for each
/// slice-table position, the plane it belongs to and its index within
/// that plane. The `bool` is `true` for the second layout below.
///
/// Two layouts are accepted:
///
/// 1. **Plane-index bytes** (`spec/02` §7.3): byte `k` names the plane
///    of slice `k`; a slice's within-plane index is the running count
///    of earlier slices of the same plane. The slice table is walked
///    with the `spec/02` §5.1 rule (slice `k` ends where slice `k + 1`
///    starts).
/// 2. **Row-major rank bytes**: byte `p·S + s` holds `s·P + p` (`P`
///    planes, `S` slices per plane) and the slice table is indexed
///    plane-major — entry `1 + p·S + s` is the start of slice `s` of
///    plane `p` — while the payloads themselves are stored slice-row by
///    slice-row, so the table is not monotonic. This shape is not in
///    `spec/02`; it is what a black-box encoder writes for every
///    multi-slice frame, and it is accepted only in exactly this form.
///    With `S == 1` the two layouts are the same bytes.
///
/// The layouts cannot be confused: with `S > 1` layout 2 always
/// contains a value `≥ P`, which layout 1 rejects.
fn resolve_slice_map(
    index_bytes: &[u8],
    num_planes: usize,
    slices_per_plane: usize,
) -> Result<(Vec<(usize, usize)>, bool)> {
    let mut map: Vec<(usize, usize)> = Vec::with_capacity(index_bytes.len());
    let mut plane_fill = vec![0usize; num_planes];
    let mut bad: Option<(usize, usize)> = None;
    for (s, &p) in index_bytes.iter().enumerate() {
        let plane = p as usize;
        if plane >= num_planes || plane_fill[plane] >= slices_per_plane {
            bad = Some((s, plane));
            break;
        }
        map.push((plane, plane_fill[plane]));
        plane_fill[plane] += 1;
    }
    let Some((slice_index, got)) = bad else {
        return Ok((map, false));
    };
    let row_major_rank = index_bytes.iter().enumerate().all(|(k, &b)| {
        let (plane, in_plane) = (k / slices_per_plane, k % slices_per_plane);
        b as usize == in_plane * num_planes + plane
    });
    if !row_major_rank {
        return Err(Error::BadPlaneIndex {
            slice_index,
            got,
            num_planes,
        });
    }
    map.clear();
    map.extend((0..index_bytes.len()).map(|k| (k / slices_per_plane, k % slices_per_plane)));
    Ok((map, true))
}

/// Minimum number of payload bytes (after the 2-byte slice prefix)
/// that can carry `pixels` residuals at `bits_per_residual` bits each:
/// `ceil(pixels * bits / 8)`. Computed in `u64` so the product cannot
/// wrap on 32-bit targets; `None` if the result does not fit `usize`.
fn min_payload_bytes(pixels: usize, bits_per_residual: u32) -> Option<usize> {
    let bits = (pixels as u64).checked_mul(bits_per_residual as u64)?;
    usize::try_from(bits.div_ceil(8)).ok()
}

/// Resolve every slice's byte range per the `spec/02` §5.1 decoder
/// rule and reject any slice that cannot possibly hold its residuals,
/// *before* the decoder sizes its plane buffers from the header.
///
/// Per slice `s` (`0..total_slices`):
///
/// - start = `entry[s+1] + 0x20`; end = `entry[s+2] + 0x20`, or the
///   packet length for the last slice (`spec/02` §5.1). Both are
///   computed with checked arithmetic; `end < start` (a non-monotonic
///   or overlapping table) or `end > packet length` is
///   [`Error::SliceTruncated`]. Because every slice must satisfy
///   `start <= end` and slice 0 starts at the preamble end
///   (`entry[1]`, `spec/02` §5.1), the chain also guarantees that no
///   slice starts inside the header, slice table or preamble.
/// - the payload carries the 2-byte `slice_flags` / `predictor_id`
///   prefix (`spec/05` §3.1), else [`Error::SlicePrefixMissing`].
/// - the bytes after the prefix are at least the minimum the slice's
///   `rows × plane_width` residuals need: `bits` bits each in raw mode
///   (`spec/05` §4.1 — one byte per sample at 8-bit), and at least the
///   plane's *shortest* code length each in Huffman mode (`spec/05`
///   §3.4: a Huffman slice is `2 + ceil(Σ code bits / 8)` bytes, and
///   every assigned code is ≥ 1 bit). A Huffman slice of a plane whose
///   descriptor assigns no code at all is [`Error::HuffmanIncomplete`].
///
/// The last check is what bounds the decoder's memory: the bit reader
/// zero-pads past the end of a slice (`spec/05` §3.3 — the residual
/// count comes from geometry, not from the bitstream), so without it a
/// few hundred bytes declaring a `32768 × 32768` raster would make the
/// decoder allocate and fill gigabytes of planes. With it, the decoded
/// raster is at most 8 samples per input byte.
#[allow(clippy::too_many_arguments)]
fn validate_slices(
    bytes: &[u8],
    entries: &[u32],
    table_off: usize,
    total_slices: usize,
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    huff_tables: &[HuffmanTable],
    rec: FourccRecord,
    scattered: bool,
) -> Result<Vec<(usize, usize)>> {
    let offset = |e: u32, s: usize| {
        usize::try_from(e)
            .ok()
            .and_then(|e| e.checked_add(table_off))
            .ok_or(Error::SliceTruncated { slice_index: s })
    };
    // Shortest assigned code per plane (0 = no code assigned).
    let min_code_len: Vec<u32> = huff_tables
        .iter()
        .map(|t| {
            t.lengths()
                .iter()
                .copied()
                .filter(|&l| l > 0)
                .min()
                .unwrap_or(0) as u32
        })
        .collect();
    let mut sorted_starts: Vec<usize> = Vec::new();
    let mut first_start = 0usize;
    if scattered {
        for (s, &e) in entries[1..].iter().enumerate() {
            sorted_starts.push(offset(e, s)?);
        }
        first_start = sorted_starts[0];
        sorted_starts.sort_unstable();
    }
    let mut ranges = Vec::with_capacity(total_slices);
    for (s, &(plane, in_plane_idx)) in slice_plane_map.iter().enumerate() {
        let start = offset(entries[s + 1], s)?;
        let end = if scattered {
            // Payloads are not in table order: a slice ends at the
            // nearest later slice start (or the packet end). No two
            // slices may share a start and none may begin before the
            // preamble end (`entry[1]`).
            let at = sorted_starts.partition_point(|&e| e <= start);
            if start < first_start || (at >= 2 && sorted_starts[at - 2] == start) {
                return Err(Error::SliceTruncated { slice_index: s });
            }
            sorted_starts.get(at).copied().unwrap_or(bytes.len())
        } else if s + 1 < total_slices {
            offset(entries[s + 2], s)?
        } else {
            bytes.len()
        };
        if end < start || bytes.len() < end {
            return Err(Error::SliceTruncated { slice_index: s });
        }
        if end - start < 2 {
            return Err(Error::SlicePrefixMissing { slice_index: s });
        }
        let g = plane_geoms[plane];
        let row_start = in_plane_idx * g.plane_slice_height;
        let row_end = ((in_plane_idx + 1) * g.plane_slice_height).min(g.height);
        let pixels = row_end.saturating_sub(row_start) * g.width;
        let bits_per_residual = if (bytes[start] & 0x01) != 0 {
            rec.bit_depth as u32
        } else {
            match min_code_len[plane] {
                0 => return Err(Error::HuffmanIncomplete { plane }),
                l => l,
            }
        };
        let needed = min_payload_bytes(pixels, bits_per_residual)
            .ok_or(Error::SliceTruncated { slice_index: s })?;
        if end - start - 2 < needed {
            return Err(Error::SliceTruncated { slice_index: s });
        }
        ranges.push((start, end));
    }
    Ok(ranges)
}

/// Pull the existing per-plane `Vec<u8>` storage out of `dst` so it
/// can be re-used in place; returns a Vec of (resized-to-fit) buffers
/// matching `plane_geoms`. Any plane that was previously high-bit-depth
/// — or any size mismatch — is replaced with a fresh allocation; the
/// reuse path costs nothing extra for the non-streaming case.
fn take_plane_bufs_u8(dst: &mut DecodedFrame, plane_geoms: &[PlaneGeom]) -> Vec<Vec<u8>> {
    let mut pool: Vec<Vec<u8>> = dst
        .planes
        .drain(..)
        .map(|p| match p.samples {
            Samples::U8(v) => v,
            Samples::U16(_) => Vec::new(),
        })
        .collect();
    pool.resize_with(plane_geoms.len(), Vec::new);
    for (buf, g) in pool.iter_mut().zip(plane_geoms.iter()) {
        let needed = g.width * g.height;
        buf.clear();
        buf.resize(needed, 0u8);
    }
    pool
}

/// 16-bit analogue of [`take_plane_bufs_u8`].
fn take_plane_bufs_u16(dst: &mut DecodedFrame, plane_geoms: &[PlaneGeom]) -> Vec<Vec<u16>> {
    let mut pool: Vec<Vec<u16>> = dst
        .planes
        .drain(..)
        .map(|p| match p.samples {
            Samples::U16(v) => v,
            Samples::U8(_) => Vec::new(),
        })
        .collect();
    pool.resize_with(plane_geoms.len(), Vec::new);
    for (buf, g) in pool.iter_mut().zip(plane_geoms.iter()) {
        let needed = g.width * g.height;
        buf.clear();
        buf.resize(needed, 0u16);
    }
    pool
}

#[allow(clippy::too_many_arguments)]
fn decode_eight_bit(
    bytes: &[u8],
    slice_ranges: &[(usize, usize)],
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    huff_tables: &[HuffmanTable],
    rec: FourccRecord,
    field_stride: FieldStride,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
    hdr: FrameHeader,
    dst: &mut DecodedFrame,
    threads: usize,
) -> Result<()> {
    // Re-use existing per-plane storage when geometry matches; only
    // size-mismatched (or previously-U16) slots allocate.
    let mut plane_bufs: Vec<Vec<u8>> = take_plane_bufs_u8(dst, plane_geoms);

    // `(plane, in_plane_idx)` of every slice comes from the on-wire
    // mapping (spec/02 §7.3). Bounds, prefix and minimum size were
    // checked by `validate_slices`.
    let jobs = slice_jobs(
        bytes,
        slice_ranges,
        slice_plane_map,
        plane_geoms,
        &mut plane_bufs,
    );
    let results = crate::parallel::for_each_job(
        jobs,
        threads,
        || (),
        |_, job| -> Result<SliceDone> {
            let slice_flags = job.payload[0];
            let predictor_id = job.payload[1];
            let pred_kind = tables::lookup_predictor(predictor_id)?;
            let done = job.done(slice_flags, predictor_id);
            if (slice_flags & 0x01) != 0 {
                let needed = job.out.len();
                if job.payload.len() - 2 < needed {
                    return Err(Error::SliceTruncated {
                        slice_index: job.slice,
                    });
                }
                job.out.copy_from_slice(&job.payload[2..2 + needed]);
            } else {
                let mut br = BitReader::new(&job.payload[2..]);
                // A `false` return means a symbol indexed an
                // unused-codespace slot of an under-full descriptor
                // (`spec/05` §2.1) — reject the slice as malformed
                // rather than emit garbage.
                if !huff_tables[job.plane].decode_into_u8(&mut br, job.out) {
                    return Err(Error::HuffmanIncomplete { plane: job.plane });
                }
            }
            predict::apply_u8_with_stride(pred_kind, job.out, job.rows, job.width, field_stride);
            Ok(done)
        },
    );
    finish_slices(
        results,
        #[cfg(feature = "trace")]
        tracer,
    )?;

    let final_planes = match rec.family {
        Family::Rgb | Family::Rgba => {
            reverse_rgb_decorrelation_u8(plane_bufs, plane_geoms, rec, threads)
        }
        Family::Yuv | Family::Yuva | Family::Gray => plane_bufs
            .into_iter()
            .zip(plane_geoms.iter())
            .map(|(data, g)| DecodedPlane {
                width: g.width,
                height: g.height,
                bit_depth: 8,
                samples: Samples::U8(data),
            })
            .collect(),
    };

    dst.width = hdr.width;
    dst.height = hdr.height;
    dst.planes = final_planes;
    dst.header = hdr;
    dst.record = rec;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_high_bit_depth(
    bytes: &[u8],
    slice_ranges: &[(usize, usize)],
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    huff_tables: &[HuffmanTable],
    rec: FourccRecord,
    field_stride: FieldStride,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
    hdr: FrameHeader,
    dst: &mut DecodedFrame,
    threads: usize,
) -> Result<()> {
    let bits = rec.bit_depth;
    let mask = rec.sample_mask() as u16;

    let mut plane_bufs: Vec<Vec<u16>> = take_plane_bufs_u16(dst, plane_geoms);

    // On-wire slice→plane mapping (spec/02 §7.3); see
    // `decode_eight_bit` for the rationale.
    let jobs = slice_jobs(
        bytes,
        slice_ranges,
        slice_plane_map,
        plane_geoms,
        &mut plane_bufs,
    );
    let results = crate::parallel::for_each_job(
        jobs,
        threads,
        || (),
        |_, job| -> Result<SliceDone> {
            let slice_flags = job.payload[0];
            let predictor_id = job.payload[1];
            let pred_kind = tables::lookup_predictor(predictor_id)?;
            let done = job.done(slice_flags, predictor_id);
            if (slice_flags & 0x01) != 0 {
                // High-bit-depth raw mode: residuals are bit-packed at
                // `bits` bits per sample, MSB-first, total
                // `pixels * bits` bits (`spec/05` §4.1).
                let needed_bytes =
                    min_payload_bytes(job.out.len(), bits as u32).ok_or(Error::SliceTruncated {
                        slice_index: job.slice,
                    })?;
                if job.payload.len() - 2 < needed_bytes {
                    return Err(Error::SliceTruncated {
                        slice_index: job.slice,
                    });
                }
                // Batched unpacker — same observable bit stream as a
                // per-pixel `BitReader::read_bits(bits)` loop but folds
                // the refill check into a per-`floor(56/bits)` cadence
                // rather than firing once per pixel.
                unpack_raw_bits_to_u16(&job.payload[2..2 + needed_bytes], job.out, bits, mask);
            } else {
                let mut br = BitReader::new(&job.payload[2..]);
                // `false` ⇒ a symbol hit an unused-codespace slot of an
                // under-full descriptor (`spec/05` §2.1); reject as
                // malformed.
                if !huff_tables[job.plane].decode_into_u16(&mut br, job.out, mask) {
                    return Err(Error::HuffmanIncomplete { plane: job.plane });
                }
            }
            predict::apply_u16_with_stride(
                pred_kind,
                job.out,
                job.rows,
                job.width,
                mask,
                field_stride,
            );
            Ok(done)
        },
    );
    finish_slices(
        results,
        #[cfg(feature = "trace")]
        tracer,
    )?;

    let final_planes = match rec.family {
        Family::Rgb | Family::Rgba => {
            reverse_rgb_decorrelation_u16(plane_bufs, plane_geoms, rec, mask, threads)
        }
        Family::Yuv | Family::Yuva | Family::Gray => plane_bufs
            .into_iter()
            .zip(plane_geoms.iter())
            .map(|(data, g)| DecodedPlane {
                width: g.width,
                height: g.height,
                bit_depth: bits,
                samples: Samples::U16(data),
            })
            .collect(),
    };

    dst.width = hdr.width;
    dst.height = hdr.height;
    dst.planes = final_planes;
    dst.header = hdr;
    dst.record = rec;
    Ok(())
}

fn plane_geom(
    rec: FourccRecord,
    plane: usize,
    width: usize,
    height: usize,
    slice_height: usize,
) -> PlaneGeom {
    let (sub_x, sub_y) = match rec.family {
        Family::Yuv if plane == 1 || plane == 2 => (rec.sub_x as usize, rec.sub_y as usize),
        Family::Yuva if plane == 1 || plane == 2 => (rec.sub_x as usize, rec.sub_y as usize),
        _ => (1, 1),
    };
    // Chroma planes round up at odd sizes (`spec/03` §8.2,
    // `spec/04` §6: `ceil(width / sub_x)`).
    let pw = width.div_ceil(sub_x);
    let ph = height.div_ceil(sub_y);
    PlaneGeom {
        width: pw,
        height: ph,
        plane_slice_height: slice_height / sub_y,
    }
}

fn reverse_rgb_decorrelation_u8(
    mut wire_planes: Vec<Vec<u8>>,
    geoms: &[PlaneGeom],
    rec: FourccRecord,
    threads: usize,
) -> Vec<DecodedPlane> {
    debug_assert!(matches!(rec.family, Family::Rgb | Family::Rgba));
    debug_assert!(wire_planes.len() == geoms.len());
    debug_assert!(wire_planes.len() >= 3);
    // Borrow B', G, R' simultaneously via disjoint slice splits — no
    // intermediate clone of G. `wire_planes` has at least 3 entries;
    // optional A sits in index 3.
    {
        let (b_slice, rest) = wire_planes.split_at_mut(1); // [B'] | [G, R', ...]
        let (g_slice, rest2) = rest.split_at_mut(1); //         [G]  | [R', ...]
        let (r_slice, _) = rest2.split_at_mut(1); //              [R'] | [..]
        let b_prime = &mut b_slice[0];
        let g_plane = &g_slice[0];
        let r_prime = &mut r_slice[0];
        let add_green = |x: &mut [u8], g: &[u8]| {
            for (v, &gv) in x.iter_mut().zip(g) {
                *v = v.wrapping_add(gv);
            }
        };
        crate::parallel::zip_chunks(b_prime, g_plane, threads, add_green);
        crate::parallel::zip_chunks(r_prime, g_plane, threads, add_green);
    }
    let mut iter = wire_planes.into_iter();
    let b = iter.next().unwrap();
    let g_plane = iter.next().unwrap();
    let r = iter.next().unwrap();
    let a_opt = iter.next();
    let mut out = Vec::with_capacity(geoms.len());
    out.push(DecodedPlane {
        width: geoms[0].width,
        height: geoms[0].height,
        bit_depth: 8,
        samples: Samples::U8(g_plane),
    });
    out.push(DecodedPlane {
        width: geoms[1].width,
        height: geoms[1].height,
        bit_depth: 8,
        samples: Samples::U8(b),
    });
    out.push(DecodedPlane {
        width: geoms[2].width,
        height: geoms[2].height,
        bit_depth: 8,
        samples: Samples::U8(r),
    });
    if let Some(a) = a_opt {
        out.push(DecodedPlane {
            width: geoms[3].width,
            height: geoms[3].height,
            bit_depth: 8,
            samples: Samples::U8(a),
        });
    }
    out
}

fn reverse_rgb_decorrelation_u16(
    mut wire_planes: Vec<Vec<u16>>,
    geoms: &[PlaneGeom],
    rec: FourccRecord,
    mask: u16,
    threads: usize,
) -> Vec<DecodedPlane> {
    debug_assert!(matches!(rec.family, Family::Rgb | Family::Rgba));
    debug_assert!(wire_planes.len() == geoms.len());
    debug_assert!(wire_planes.len() >= 3);
    // Same disjoint-split trick as the u8 path — borrow B', G, R' at
    // once and avoid the working copy of G.
    {
        let (b_slice, rest) = wire_planes.split_at_mut(1);
        let (g_slice, rest2) = rest.split_at_mut(1);
        let (r_slice, _) = rest2.split_at_mut(1);
        let b_prime = &mut b_slice[0];
        let g_plane = &g_slice[0];
        let r_prime = &mut r_slice[0];
        let add_green = |x: &mut [u16], g: &[u16]| {
            for (v, &gv) in x.iter_mut().zip(g) {
                *v = v.wrapping_add(gv) & mask;
            }
        };
        crate::parallel::zip_chunks(b_prime, g_plane, threads, add_green);
        crate::parallel::zip_chunks(r_prime, g_plane, threads, add_green);
    }
    let bd = rec.bit_depth;
    let mut iter = wire_planes.into_iter();
    let b = iter.next().unwrap();
    let g_plane = iter.next().unwrap();
    let r = iter.next().unwrap();
    let a_opt = iter.next();
    let mut out = Vec::with_capacity(geoms.len());
    out.push(DecodedPlane {
        width: geoms[0].width,
        height: geoms[0].height,
        bit_depth: bd,
        samples: Samples::U16(g_plane),
    });
    out.push(DecodedPlane {
        width: geoms[1].width,
        height: geoms[1].height,
        bit_depth: bd,
        samples: Samples::U16(b),
    });
    out.push(DecodedPlane {
        width: geoms[2].width,
        height: geoms[2].height,
        bit_depth: bd,
        samples: Samples::U16(r),
    });
    if let Some(a) = a_opt {
        out.push(DecodedPlane {
            width: geoms[3].width,
            height: geoms[3].height,
            bit_depth: bd,
            samples: Samples::U16(a),
        });
    }
    out
}
