//! Hostile slice-table / geometry inputs against the public decoder.
//!
//! Every case here is a malformed (or adversarially-shaped) v7 frame
//! built by mutating a well-formed frame from our own encoder. The
//! contract under test (`spec/02` §5.1 decoder rule, `spec/05` §3.3 /
//! §3.4 slice payload size relation) is that `decode_frame` and
//! `decode_into` always *return* — `Err` for anything the spec's
//! layout makes impossible — and never panic, overflow, index out of
//! bounds, or allocate a raster the packet cannot possibly describe.
//!
//! Tests run in the debug profile, so integer overflow panics too.

use oxideav_magicyuv::decoder::DecodedFrame;
use oxideav_magicyuv::header::{HEADER_SIZE, MAX_DIMENSION};
use oxideav_magicyuv::tables::{self, Family, FourccRecord};
use oxideav_magicyuv::{EncodeOptions, PlaneInput, SliceMode, decode_frame, decode_into};

/// Format bytes covering every family / bit-depth tier the slice walk
/// distinguishes: 8-bit RGB, 8-bit RGBA, 8-bit YUV 4:2:0, 8-bit Gray,
/// 10-bit RGB, 10-bit YUV 4:2:2 (the high-bit-depth path).
fn sample_records() -> Vec<FourccRecord> {
    let mut out = Vec::new();
    for rec in tables::fourcc_table() {
        let key = (rec.bit_depth, rec.family, rec.sub_x, rec.sub_y);
        let wanted = [
            (8, Family::Rgb, 1, 1),
            (8, Family::Rgba, 1, 1),
            (8, Family::Yuv, 2, 2),
            (8, Family::Gray, 1, 1),
            (10, Family::Rgb, 1, 1),
            (10, Family::Yuv, 2, 1),
        ];
        if wanted.contains(&key)
            && !out
                .iter()
                .any(|r: &FourccRecord| (r.bit_depth, r.family, r.sub_x, r.sub_y) == key)
        {
            out.push(*rec);
        }
    }
    assert!(out.len() >= 5, "fourcc table missing expected families");
    out
}

fn plane_dims(rec: &FourccRecord, plane: usize, w: u32, h: u32) -> (usize, usize) {
    let chroma = matches!(rec.family, Family::Yuv | Family::Yuva) && (plane == 1 || plane == 2);
    if chroma {
        (
            (w / rec.sub_x as u32) as usize,
            (h / rec.sub_y as u32) as usize,
        )
    } else {
        (w as usize, h as usize)
    }
}

/// Encode a small textured frame with several slices per plane.
fn make_frame(rec: FourccRecord, w: u32, h: u32, slice_height: u32, mode: SliceMode) -> Vec<u8> {
    let mut planes = Vec::new();
    for p in 0..rec.planes as usize {
        let (pw, ph) = plane_dims(&rec, p, w, h);
        let n = pw * ph;
        if rec.bit_depth == 8 {
            planes.push(PlaneInput::U8(
                (0..n)
                    .map(|i| ((i * 7 + p * 31) ^ (i >> 3)) as u8)
                    .collect(),
            ));
        } else {
            let mask = (1u32 << rec.bit_depth) - 1;
            planes.push(PlaneInput::U16(
                (0..n)
                    .map(|i| (((i * 13 + p * 97) ^ (i >> 2)) as u32 & mask) as u16)
                    .collect(),
            ));
        }
    }
    let opts = EncodeOptions {
        mode,
        ..EncodeOptions::default()
    };
    let bytes = oxideav_magicyuv::encode_frame(rec, w, h, slice_height, planes, opts)
        .expect("encode baseline frame");
    decode_frame(&bytes).expect("baseline frame must decode");
    bytes
}

fn baselines() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for rec in sample_records() {
        for mode in [SliceMode::Huffman, SliceMode::Raw] {
            out.push(make_frame(rec, 16, 20, 4, mode));
        }
    }
    out
}

fn total_slices(frame: &[u8]) -> usize {
    let rec = tables::lookup(frame[9]).unwrap();
    let h = u32::from_le_bytes(frame[20..24].try_into().unwrap()) as usize;
    let sh = u32::from_le_bytes(frame[28..32].try_into().unwrap()) as usize;
    rec.planes as usize * h.div_ceil(sh)
}

fn entry(frame: &[u8], i: usize) -> u32 {
    let o = HEADER_SIZE + 4 * i;
    u32::from_le_bytes(frame[o..o + 4].try_into().unwrap())
}

fn set_entry(frame: &mut [u8], i: usize, v: u32) {
    let o = HEADER_SIZE + 4 * i;
    frame[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn set_u32(frame: &mut [u8], off: usize, v: u32) {
    frame[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Drive both public entry points; the streaming one into a frame a
/// previous good decode already populated (stale-geometry reuse).
fn decode_both(bytes: &[u8], good: &[u8]) -> bool {
    let one_shot = decode_frame(bytes).is_ok();
    let mut dst = DecodedFrame::empty();
    decode_into(good, &mut dst).expect("good frame decodes");
    let streamed = decode_into(bytes, &mut dst).is_ok();
    assert_eq!(one_shot, streamed, "one-shot and streaming paths disagree");
    // A failed decode must leave `dst` reusable.
    decode_into(good, &mut dst).expect("dst reusable after hostile frame");
    one_shot
}

#[test]
fn every_truncation_of_a_frame_is_rejected() {
    for good in baselines() {
        let n = total_slices(&good);
        let table_end = HEADER_SIZE + 4 * (n + 1);
        for len in 0..good.len() {
            let cut = &good[..len];
            let ok = decode_both(cut, &good);
            // Anything short of the full slice table, preamble and
            // first slice prefix can never be a frame.
            if len < table_end + 2 {
                assert!(!ok, "truncation to {len} bytes accepted");
            }
        }
    }
}

#[test]
fn slice_offsets_past_packet_end_are_rejected() {
    for good in baselines() {
        let n = total_slices(&good);
        let rel_len = (good.len() - HEADER_SIZE) as u32;
        for i in 1..=n {
            for v in [
                rel_len + 1,
                rel_len + 2,
                rel_len + 1000,
                u32::MAX - 32,
                u32::MAX - 31,
                u32::MAX - 1,
                u32::MAX,
            ] {
                let mut f = good.clone();
                set_entry(&mut f, i, v);
                assert!(!decode_both(&f, &good), "entry[{i}] = {v:#x} accepted");
            }
        }
    }
}

#[test]
fn non_monotonic_and_overlapping_offsets_are_rejected() {
    for good in baselines() {
        let n = total_slices(&good);
        let first = entry(&good, 1);
        for i in 2..=n {
            let prev = entry(&good, i - 1);
            let next = if i < n {
                Some(entry(&good, i + 1))
            } else {
                None
            };
            let mut cases = vec![
                0,                    // into the slice table / header
                first - 1,            // last preamble byte
                prev.wrapping_sub(1), // before the previous slice start
            ];
            if let Some(nx) = next {
                cases.push(nx + 1); // past the following slice start
            }
            for v in cases {
                let mut f = good.clone();
                set_entry(&mut f, i, v);
                assert!(!decode_both(&f, &good), "entry[{i}] = {v} accepted");
            }
            // Zero-length slice (entry[i] == entry[i-1]): the earlier
            // slice loses its 2-byte prefix.
            let mut f = good.clone();
            set_entry(&mut f, i, prev);
            assert!(!decode_both(&f, &good), "zero-length slice {i} accepted");
        }
        // Swap every adjacent pair of slice starts.
        for i in 1..n {
            let mut f = good.clone();
            let (a, b) = (entry(&good, i), entry(&good, i + 1));
            set_entry(&mut f, i, b);
            set_entry(&mut f, i + 1, a);
            assert!(
                !decode_both(&f, &good),
                "swapped entries {i}/{} accepted",
                i + 1
            );
        }
        // entry[1] (= preamble end) shrunk into the slice table, or
        // grown past slice 0's end.
        for v in [0, 4 * n as u32, 4 * (n as u32 + 1)] {
            let mut f = good.clone();
            set_entry(&mut f, 1, v);
            assert!(!decode_both(&f, &good), "entry[1] = {v} accepted");
        }
    }
}

#[test]
fn entry_zero_is_ignored() {
    // spec/02 §5.1: entry[0] is not part of the decoder rule.
    for good in baselines() {
        for v in [0, 1, u32::MAX] {
            let mut f = good.clone();
            set_entry(&mut f, 0, v);
            assert!(decode_both(&f, &good), "entry[0] = {v:#x} rejected");
        }
    }
}

#[test]
fn every_single_byte_corruption_of_table_and_preamble_returns() {
    for good in baselines() {
        let preamble_end = HEADER_SIZE + entry(&good, 1) as usize;
        for pos in HEADER_SIZE..preamble_end.min(good.len()) {
            for v in [0x00u8, 0x01, 0x7f, 0x80, 0xfe, 0xff] {
                let mut f = good.clone();
                f[pos] = v;
                let _ = decode_both(&f, &good);
            }
        }
    }
}

#[test]
fn zero_and_huge_slice_heights_return() {
    for good in baselines() {
        for sh in [
            0u32,
            1,
            2,
            3,
            5,
            19,
            20,
            21,
            4096,
            MAX_DIMENSION,
            MAX_DIMENSION + 1,
            u32::MAX,
        ] {
            let mut f = good.clone();
            set_u32(&mut f, 28, sh);
            let ok = decode_both(&f, &good);
            if sh == 0 || sh > MAX_DIMENSION {
                assert!(!ok, "slice_height {sh} accepted");
            }
        }
    }
}

/// Header dimensions far beyond what the packet can describe. Every
/// Huffman residual costs at least one bit (`spec/05` §3.4: slice size
/// is `2 + ceil(Σ code bits / 8)` and every used code has length ≥ 1),
/// and a raw residual costs `bits` bits (`spec/05` §4.1) — so a frame
/// declaring a gigapixel raster in a few hundred bytes is malformed and
/// must be rejected *before* the decoder sizes its plane buffers.
#[test]
fn oversized_dimensions_are_rejected_without_allocating() {
    for good in baselines() {
        let rec = tables::lookup(good[9]).unwrap();
        for (w, h, sh) in [
            (MAX_DIMENSION, MAX_DIMENSION, MAX_DIMENSION),
            (MAX_DIMENSION, MAX_DIMENSION, 4),
            (MAX_DIMENSION, 20, 4),
            (16, MAX_DIMENSION, MAX_DIMENSION),
            (MAX_DIMENSION, MAX_DIMENSION, 1),
            (u32::MAX, 20, 4),
            (16, u32::MAX, 4),
            (MAX_DIMENSION + 1, 20, 4),
        ] {
            let mut f = good.clone();
            set_u32(&mut f, 16, w);
            set_u32(&mut f, 20, h);
            set_u32(&mut f, 24, w);
            set_u32(&mut f, 28, sh);
            if rec.sub_y > 1 && sh % rec.sub_y as u32 != 0 {
                continue;
            }
            let start = std::time::Instant::now();
            assert!(
                !decode_both(&f, &good),
                "{w}x{h}/{sh} accepted from {} bytes",
                f.len()
            );
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "{w}x{h}/{sh} took {:?} to reject",
                start.elapsed()
            );
        }
    }
}

/// A slice-table that is internally consistent but describes far more
/// slices than the packet holds (tiny `slice_height`, large height).
#[test]
fn slice_count_larger_than_packet_is_rejected() {
    for good in baselines() {
        for (h, sh) in [(MAX_DIMENSION, 2), (MAX_DIMENSION, 1), (4096, 2)] {
            let rec = tables::lookup(good[9]).unwrap();
            if rec.sub_y > 1 && sh % rec.sub_y as u32 != 0 {
                continue;
            }
            let mut f = good.clone();
            set_u32(&mut f, 20, h);
            set_u32(&mut f, 28, sh);
            assert!(!decode_both(&f, &good), "h={h} sh={sh} accepted");
        }
    }
}
