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
//! Round 1 covers 8-bit native FOURCCs only. The 10/12/14-bit dispatch
//! returns `Error::UnsupportedFormatByte` (gated at header parse).

use crate::bitreader::BitReader;
use crate::error::{Error, Result};
use crate::header::{self, FrameHeader};
use crate::huffman::{self, HuffmanTable};
use crate::predict;
use crate::tables::{self, Family, FourccRecord};

/// Maximum Huffman code length for an 8-bit plane (`spec/05` §1.1).
const MAX_HUFF_LEN_8BIT: u8 = 12;

/// Per-plane geometry computed from `(width, height, slice_height,
/// FourccRecord)`. Round 1 only emits 8-bit planes; the per-sample
/// container size is always 1 byte.
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

/// One reconstructed plane.
pub struct DecodedPlane {
    pub width: usize,
    pub height: usize,
    /// Row-major sample bytes. For 8-bit planes this is one byte per
    /// pixel (`width * height` bytes total).
    pub data: Vec<u8>,
}

/// Decode a complete v7 MAGY frame from `bytes`.
pub fn decode_frame(bytes: &[u8]) -> Result<DecodedFrame> {
    if bytes.len() < header::HEADER_SIZE {
        return Err(Error::Truncated {
            what: "MAGY frame",
            needed: header::HEADER_SIZE,
            have: bytes.len(),
        });
    }
    let hdr = header::parse(bytes)?;
    let rec = tables::lookup_round1(hdr.format_byte)?;

    let width = hdr.width as usize;
    let height = hdr.height as usize;
    let slice_height = hdr.slice_height as usize;
    let slices_per_plane = height.div_ceil(slice_height);
    let num_planes = rec.planes as usize;
    let total_slices = num_planes * slices_per_plane;

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
        // The wire's plane_count does not match the format byte's
        // family-implied count. spec/02 §7.2 notes this is
        // unverified-divergent-from-format_byte; we reject it because
        // in v2.4.2 they're always equal and we have no way to honour
        // a different count.
        return Err(Error::Truncated {
            what: "preamble plane_count != format-byte planes",
            needed: num_planes,
            have: plane_count,
        });
    }
    // Per-slice plane index bytes at preamble[1 .. 1+total_slices].
    if preamble.len() < 1 + total_slices {
        return Err(Error::Truncated {
            what: "preamble per-slice plane-index",
            needed: 1 + total_slices,
            have: preamble.len(),
        });
    }
    let per_slice_plane = &preamble[1..1 + total_slices];
    // The plane-major ordering is what v2.4.2 emits and what the
    // walker assumes; reject anything else for round 1.
    for (s, &p) in per_slice_plane.iter().enumerate() {
        let expected = (s / slices_per_plane) as u8;
        if p != expected {
            // Round-1 conservative rejection — the spec leaves this
            // open (spec/02 §7.3 + §10 question 8) but no tested
            // fixture deviates. We could honour arbitrary orderings
            // by sorting; not needed for round 1.
            return Err(Error::Truncated {
                what: "preamble per_slice_plane_index not plane-major",
                needed: expected as usize,
                have: p as usize,
            });
        }
    }

    // Per-plane Huffman descriptors at preamble[1 + total_slices ..].
    let mut huff_tables: Vec<HuffmanTable> = Vec::with_capacity(num_planes);
    let mut desc_pos = 1 + total_slices;
    for plane in 0..num_planes {
        let n_symbols = 1usize << 8; // 8-bit family: 256 symbols.
        let (lens, used) =
            huffman::parse_lengths(&preamble[desc_pos..], n_symbols, MAX_HUFF_LEN_8BIT, plane)?;
        desc_pos += used;
        let table = HuffmanTable::build(lens, plane)?;
        huff_tables.push(table);
    }

    // Per-plane geometry. RGB/RGBA/YUVA + alpha plane = full res; YUV
    // chroma planes are subsampled by (sub_x, sub_y).
    let plane_geoms: Vec<PlaneGeom> = (0..num_planes)
        .map(|p| plane_geom(rec, p, width, height, slice_height))
        .collect();

    // Allocate per-plane output buffers.
    let mut plane_bufs: Vec<Vec<u8>> = plane_geoms
        .iter()
        .map(|g| vec![0u8; g.width * g.height])
        .collect();

    // Walk slices. Plane-major ordering: slices [0..slices_per_plane)
    // belong to plane 0, [slices_per_plane..2*slices_per_plane) to
    // plane 1, etc. Within a plane the slices stack vertically.
    for s in 0..total_slices {
        let plane = s / slices_per_plane;
        let in_plane_idx = s % slices_per_plane;

        let g = plane_geoms[plane];
        let row_start = in_plane_idx * g.plane_slice_height;
        let row_end = ((in_plane_idx + 1) * g.plane_slice_height).min(g.height);
        let slice_rows = row_end - row_start;

        // Slice payload is bytes [entry[s+1] + 0x20 .. (next or
        // end)] per spec/02 §5.1.
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

        if (slice_flags & 0x01) != 0 {
            // Raw mode (`spec/05` §4.1). 8-bit family: one byte per
            // residual.
            let needed = pixels_in_slice;
            if payload.len() < 2 + needed {
                return Err(Error::SliceTruncated { slice_index: s });
            }
            buf.copy_from_slice(&payload[2..2 + needed]);
        } else {
            // Huffman mode. Decode pixels_in_slice symbols from the
            // bitstream starting at payload+2, MSB-first (spec/05
            // §2.2 / §3.2).
            let bits = &payload[2..];
            let mut br = BitReader::new(bits);
            let table = &huff_tables[plane];
            for px in buf.iter_mut() {
                *px = table.decode(&mut br) as u8;
            }
        }

        // Apply the slice's predictor in-place.
        predict::apply_u8(pred_kind, buf, slice_rows, g.width);
    }

    // For RGB-family streams, reverse the (B', G, R') decorrelation
    // per spec/03 §4 audit-corrected wire-order:
    //   B = (B' + G) mod 256, R = (R' + G) mod 256.
    // The wire stores planes in order (B', G, R')[, A] but spec/03
    // §4 specifies the user-facing output order is (G, B, R)[, A].
    // We reorder while reversing.
    let final_planes = match rec.family {
        Family::Rgb | Family::Rgba => reverse_rgb_decorrelation(plane_bufs, &plane_geoms, rec),
        Family::Yuv | Family::Yuva | Family::Gray => plane_bufs
            .into_iter()
            .zip(plane_geoms.iter())
            .map(|(data, g)| DecodedPlane {
                width: g.width,
                height: g.height,
                data,
            })
            .collect(),
    };

    Ok(DecodedFrame {
        width: hdr.width,
        height: hdr.height,
        planes: final_planes,
        header: hdr,
        record: rec,
    })
}

/// Compute per-plane (width, height, plane_slice_height) for the
/// `plane`-th plane in the format byte's family order.
fn plane_geom(
    rec: FourccRecord,
    plane: usize,
    width: usize,
    height: usize,
    slice_height: usize,
) -> PlaneGeom {
    let (sub_x, sub_y) = match rec.family {
        Family::Yuv if plane == 1 || plane == 2 => (rec.sub_x as usize, rec.sub_y as usize),
        // YUVA: planes 1, 2 chroma; plane 3 alpha is full res.
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

/// Reverse the RGB inter-plane decorrelation per `spec/03` §4
/// validation-corrected note. The wire stores planes in order
/// `(B', G, R')[, A]` (the round-1 supported set has 8-bit family
/// only); we reorder to the user-facing GBR(A) plane order while
/// undoing `B = (B' + G) mod 256`, `R = (R' + G) mod 256`.
fn reverse_rgb_decorrelation(
    mut wire_planes: Vec<Vec<u8>>,
    geoms: &[PlaneGeom],
    rec: FourccRecord,
) -> Vec<DecodedPlane> {
    debug_assert!(matches!(rec.family, Family::Rgb | Family::Rgba));
    debug_assert!(wire_planes.len() == geoms.len());
    debug_assert!(wire_planes.len() >= 3);

    // Wire indices: 0 = B', 1 = G, 2 = R', 3 = A (if alpha).
    // Recover B and R per pixel by adding G mod 256.
    let g = wire_planes[1].clone();
    let b_prime = &mut wire_planes[0];
    for (b, &gv) in b_prime.iter_mut().zip(g.iter()) {
        *b = b.wrapping_add(gv);
    }
    let r_prime = &mut wire_planes[2];
    for (r, &gv) in r_prime.iter_mut().zip(g.iter()) {
        *r = r.wrapping_add(gv);
    }
    // wire_planes is now (B, G, R)[, A]. User-facing output order is
    // (G, B, R)[, A] per spec/03 §4 ("RGB family wire is (B', G, R')
    // but user-facing plane order is (G, B, R)").
    let mut iter = wire_planes.into_iter();
    let b = iter.next().unwrap();
    let g_plane = iter.next().unwrap();
    let r = iter.next().unwrap();
    let a_opt = iter.next();
    let mut out = Vec::with_capacity(geoms.len());
    out.push(DecodedPlane {
        width: geoms[0].width,
        height: geoms[0].height,
        data: g_plane,
    });
    out.push(DecodedPlane {
        width: geoms[1].width,
        height: geoms[1].height,
        data: b,
    });
    out.push(DecodedPlane {
        width: geoms[2].width,
        height: geoms[2].height,
        data: r,
    });
    if let Some(a) = a_opt {
        out.push(DecodedPlane {
            width: geoms[3].width,
            height: geoms[3].height,
            data: a,
        });
    }
    out
}
