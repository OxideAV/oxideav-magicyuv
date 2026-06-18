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

use crate::bitreader::{unpack_raw_bits_to_u16, BitReader};
use crate::error::{Error, Result};
use crate::header::{self, FrameHeader, FLAG_INTERLACED};
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
pub struct DecodedPlane {
    pub width: usize,
    pub height: usize,
    /// Significant bits per sample (8, 10, 12, or 14).
    pub bit_depth: u8,
    /// Storage. 8-bit FOURCCs use [`Samples::U8`]; 10/12/14-bit use
    /// [`Samples::U16`] (with values masked to `(1 << bit_depth) - 1`).
    pub samples: Samples,
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
    let mut dst = DecodedFrame::empty();
    decode_into(bytes, &mut dst)?;
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

    // Reject odd dims that don't divide chroma subsampling cleanly —
    // the rounding rule is unverified at odd sizes (spec/03 §8.2).
    if rec.sub_x as u32 > 1 && (hdr.width % rec.sub_x as u32) != 0 {
        return Err(Error::OddDimensionForSubsampling {
            what: "width",
            got: hdr.width,
            factor: rec.sub_x as u32,
        });
    }
    if rec.sub_y as u32 > 1 && (hdr.height % rec.sub_y as u32) != 0 {
        return Err(Error::OddDimensionForSubsampling {
            what: "height",
            got: hdr.height,
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
    // Honour the on-wire slice→plane mapping rather than assuming the
    // plane-major ordering (`spec/02` §7.3: "A spec-compliant decoder
    // MUST read `per_slice_plane_index` from the preamble (not assume
    // the plane-major ordering); the encoder's freedom to interleave
    // is preserved by the table format."). Each `per_slice_plane_index`
    // byte names the plane its slice belongs to; the slice's
    // within-plane index is the running count of slices already seen
    // for that plane (so the k-th slice of plane p covers chroma /
    // luma rows `[k*plane_slice_height, …)` per §6 regardless of where
    // it sits in the global slice order). A plane index outside
    // `[0, num_planes)`, or one that over-fills a plane's
    // `slices_per_plane` quota, is rejected as malformed.
    let mut slice_plane_map: Vec<(usize, usize)> = Vec::with_capacity(total_slices);
    let mut plane_fill = vec![0usize; num_planes];
    for (s, &p) in per_slice_plane.iter().enumerate() {
        let plane = p as usize;
        if plane >= num_planes || plane_fill[plane] >= slices_per_plane {
            return Err(Error::BadPlaneIndex {
                slice_index: s,
                got: plane,
                num_planes,
            });
        }
        let in_plane_idx = plane_fill[plane];
        plane_fill[plane] += 1;
        slice_plane_map.push((plane, in_plane_idx));
    }

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
    if let Some(t) = &tracer {
        if desc_pos < preamble.len() {
            t.emit(Event::PreambleTrailing {
                extra_bytes: preamble.len() - desc_pos,
            });
        }
    }
    let _ = desc_pos;

    // Per-plane geometry.
    let plane_geoms: Vec<PlaneGeom> = (0..num_planes)
        .map(|p| plane_geom(rec, p, width, height, slice_height))
        .collect();

    let interlaced = (hdr.flags & FLAG_INTERLACED) != 0;
    let field_stride = if interlaced {
        FieldStride::INTERLACED
    } else {
        FieldStride::PROGRESSIVE
    };

    if rec.is_high_bit_depth() {
        decode_high_bit_depth(
            bytes,
            &entries,
            table_off,
            total_slices,
            &slice_plane_map,
            &plane_geoms,
            &huff_tables,
            rec,
            field_stride,
            #[cfg(feature = "trace")]
            tracer.as_ref(),
            hdr,
            dst,
        )
    } else {
        decode_eight_bit(
            bytes,
            &entries,
            table_off,
            total_slices,
            &slice_plane_map,
            &plane_geoms,
            &huff_tables,
            rec,
            field_stride,
            #[cfg(feature = "trace")]
            tracer.as_ref(),
            hdr,
            dst,
        )
    }
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
    entries: &[u32],
    table_off: usize,
    total_slices: usize,
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    huff_tables: &[HuffmanTable],
    rec: FourccRecord,
    field_stride: FieldStride,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
    hdr: FrameHeader,
    dst: &mut DecodedFrame,
) -> Result<()> {
    let _num_planes = rec.planes as usize;
    // Re-use existing per-plane storage when geometry matches; only
    // size-mismatched (or previously-U16) slots allocate.
    let mut plane_bufs: Vec<Vec<u8>> = take_plane_bufs_u8(dst, plane_geoms);

    for s in 0..total_slices {
        // `(plane, in_plane_idx)` comes from the on-wire
        // `per_slice_plane_index` mapping (spec/02 §7.3), not the
        // assumed plane-major `s / slices_per_plane`.
        let (plane, in_plane_idx) = slice_plane_map[s];
        let g = plane_geoms[plane];
        let row_start = in_plane_idx * g.plane_slice_height;
        let row_end = ((in_plane_idx + 1) * g.plane_slice_height).min(g.height);
        let slice_rows = row_end - row_start;

        let slice_start = (entries[s + 1] as usize) + table_off;
        let slice_end = if s + 1 < total_slices {
            (entries[s + 2] as usize) + table_off
        } else {
            bytes.len()
        };
        if slice_end < slice_start || bytes.len() < slice_end {
            return Err(Error::SliceTruncated { slice_index: s });
        }
        let payload = &bytes[slice_start..slice_end];
        if payload.len() < 2 {
            return Err(Error::SlicePrefixMissing { slice_index: s });
        }
        let slice_flags = payload[0];
        let predictor_id = payload[1];
        let pred_kind = tables::lookup_predictor(predictor_id)?;

        let pixels_in_slice = slice_rows * g.width;
        let buf = &mut plane_bufs[plane][row_start * g.width..row_end * g.width];

        let mode_str: &str;
        if (slice_flags & 0x01) != 0 {
            mode_str = "raw";
            let needed = pixels_in_slice;
            if payload.len() < 2 + needed {
                return Err(Error::SliceTruncated { slice_index: s });
            }
            buf.copy_from_slice(&payload[2..2 + needed]);
        } else {
            mode_str = "huffman";
            let bits = &payload[2..];
            let mut br = BitReader::new(bits);
            let table = &huff_tables[plane];
            // Inline batch helper — folds peek+consume into a single
            // tight loop so the compiler keeps BitReader state in
            // registers across iterations. Same observable bit stream
            // as the per-symbol `decode` call (kept as the slow
            // fallback inside `decode_into_u8`). A `false` return means
            // a symbol indexed an unused-codespace slot of an under-full
            // descriptor (`spec/05` §2.1) — reject the slice as
            // malformed rather than emit garbage.
            if !table.decode_into_u8(&mut br, buf) {
                return Err(Error::HuffmanIncomplete { plane });
            }
        }

        #[cfg(feature = "trace")]
        if let Some(t) = tracer {
            t.emit(Event::Payload {
                slice: s,
                plane,
                slice_in_plane: in_plane_idx,
                row_start,
                row_end,
                cols: g.width,
                file_offset: slice_start,
                payload_size: payload.len(),
                slice_flags,
                predictor_id,
                mode: mode_str,
                n_pixels: pixels_in_slice,
            });
        }
        let _ = mode_str;

        predict::apply_u8_with_stride(pred_kind, buf, slice_rows, g.width, field_stride);
    }

    let final_planes = match rec.family {
        Family::Rgb | Family::Rgba => reverse_rgb_decorrelation_u8(plane_bufs, plane_geoms, rec),
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
    entries: &[u32],
    table_off: usize,
    total_slices: usize,
    slice_plane_map: &[(usize, usize)],
    plane_geoms: &[PlaneGeom],
    huff_tables: &[HuffmanTable],
    rec: FourccRecord,
    field_stride: FieldStride,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
    hdr: FrameHeader,
    dst: &mut DecodedFrame,
) -> Result<()> {
    let _num_planes = rec.planes as usize;
    let bits = rec.bit_depth;
    let mask = rec.sample_mask() as u16;

    let mut plane_bufs: Vec<Vec<u16>> = take_plane_bufs_u16(dst, plane_geoms);

    for s in 0..total_slices {
        // On-wire slice→plane mapping (spec/02 §7.3); see
        // `decode_eight_bit` for the rationale.
        let (plane, in_plane_idx) = slice_plane_map[s];
        let g = plane_geoms[plane];
        let row_start = in_plane_idx * g.plane_slice_height;
        let row_end = ((in_plane_idx + 1) * g.plane_slice_height).min(g.height);
        let slice_rows = row_end - row_start;

        let slice_start = (entries[s + 1] as usize) + table_off;
        let slice_end = if s + 1 < total_slices {
            (entries[s + 2] as usize) + table_off
        } else {
            bytes.len()
        };
        if slice_end < slice_start || bytes.len() < slice_end {
            return Err(Error::SliceTruncated { slice_index: s });
        }
        let payload = &bytes[slice_start..slice_end];
        if payload.len() < 2 {
            return Err(Error::SlicePrefixMissing { slice_index: s });
        }
        let slice_flags = payload[0];
        let predictor_id = payload[1];
        let pred_kind = tables::lookup_predictor(predictor_id)?;

        let pixels_in_slice = slice_rows * g.width;
        let buf = &mut plane_bufs[plane][row_start * g.width..row_end * g.width];

        let mode_str: &str;
        if (slice_flags & 0x01) != 0 {
            mode_str = "raw";
            // High-bit-depth raw mode: residuals are bit-packed at
            // `bits` bits per sample, MSB-first, total
            // `pixels_in_slice * bits` bits (`spec/05` §4.1).
            let needed_bits = pixels_in_slice * bits as usize;
            let needed_bytes = needed_bits.div_ceil(8);
            if payload.len() < 2 + needed_bytes {
                return Err(Error::SliceTruncated { slice_index: s });
            }
            // Batched unpacker — same observable bit stream as a
            // per-pixel `BitReader::read_bits(bits)` loop but folds
            // the refill check into a per-`floor(56/bits)` cadence
            // rather than firing once per pixel.
            unpack_raw_bits_to_u16(&payload[2..2 + needed_bytes], buf, bits, mask);
        } else {
            mode_str = "huffman";
            let mut br = BitReader::new(&payload[2..]);
            let table = &huff_tables[plane];
            // `false` ⇒ a symbol hit an unused-codespace slot of an
            // under-full descriptor (`spec/05` §2.1); reject as malformed.
            if !table.decode_into_u16(&mut br, buf, mask) {
                return Err(Error::HuffmanIncomplete { plane });
            }
        }

        #[cfg(feature = "trace")]
        if let Some(t) = tracer {
            t.emit(Event::Payload {
                slice: s,
                plane,
                slice_in_plane: in_plane_idx,
                row_start,
                row_end,
                cols: g.width,
                file_offset: slice_start,
                payload_size: payload.len(),
                slice_flags,
                predictor_id,
                mode: mode_str,
                n_pixels: pixels_in_slice,
            });
        }
        let _ = mode_str;

        predict::apply_u16_with_stride(pred_kind, buf, slice_rows, g.width, mask, field_stride);
    }

    let final_planes = match rec.family {
        Family::Rgb | Family::Rgba => {
            reverse_rgb_decorrelation_u16(plane_bufs, plane_geoms, rec, mask)
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
    let pw = width / sub_x;
    let ph = height / sub_y;
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
        for (b, &gv) in b_prime.iter_mut().zip(g_plane.iter()) {
            *b = b.wrapping_add(gv);
        }
        for (r, &gv) in r_prime.iter_mut().zip(g_plane.iter()) {
            *r = r.wrapping_add(gv);
        }
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
        for (b, &gv) in b_prime.iter_mut().zip(g_plane.iter()) {
            *b = b.wrapping_add(gv) & mask;
        }
        for (r, &gv) in r_prime.iter_mut().zip(g_plane.iter()) {
            *r = r.wrapping_add(gv) & mask;
        }
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
