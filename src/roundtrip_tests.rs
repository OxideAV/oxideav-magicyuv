//! Self-roundtrip integration tests for the round-1 decoder.
//!
//! No reference fixtures (the cleanroom proprietary binary is
//! Auditor-only). We synthesise frames with the in-tree test
//! encoder, decode them back, and assert byte-exactness across the
//! seven 8-bit native FOURCCs × three predictors × Huffman / raw
//! modes × multiple patterns.

use crate::decoder::decode_frame;
use crate::encoder::{encode_frame, SliceMode};
use crate::tables::{lookup_round1, Family, FourccRecord, PredictorKind};

/// Helper: build per-plane pixel buffers for a given FOURCC at
/// `(width × height)` using a deterministic synthetic pattern.
fn make_planes(rec: FourccRecord, width: u32, height: u32, pattern: u8) -> Vec<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    let num_planes = rec.planes as usize;
    (0..num_planes)
        .map(|p| {
            let (sub_x, sub_y) = match rec.family {
                Family::Yuv if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                Family::Yuva if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                _ => (1usize, 1usize),
            };
            let pw = w / sub_x;
            let ph = h / sub_y;
            let mut buf = vec![0u8; pw * ph];
            for r in 0..ph {
                for c in 0..pw {
                    buf[r * pw + c] = pixel_for(pattern, p, r, c);
                }
            }
            buf
        })
        .collect()
}

fn pixel_for(pattern: u8, plane: usize, r: usize, c: usize) -> u8 {
    let r = r as u32;
    let c = c as u32;
    let p = plane as u32;
    let v: u32 = match pattern {
        0 => 0,              // all zero
        1 => c & 0xff,       // horizontal ramp
        2 => r & 0xff,       // vertical ramp
        3 => (r + c) & 0xff, // diagonal ramp
        4 => (r ^ c) & 0xff, // diagonal XOR
        5 => {
            // pseudo-random per-pixel
            let mut x: u32 = (r << 16) ^ (c << 8) ^ (p * 31) ^ 0x5113c4f5;
            x = x.wrapping_mul(0x9e37_79b9).wrapping_add(0x6534_3aa1);
            (x >> 17) & 0xff
        }
        _ => 0xa5,
    };
    (v & 0xff) as u8
}

#[allow(clippy::too_many_arguments)]
fn roundtrip(
    fourcc_label: &str,
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    predictor: PredictorKind,
    mode: SliceMode,
    pattern: u8,
) {
    let planes_in = make_planes(rec, width, height, pattern);
    let bytes = encode_frame(
        rec,
        width,
        height,
        slice_height,
        planes_in.clone(),
        predictor,
        mode,
    );
    let dec = decode_frame(&bytes).unwrap_or_else(|e| {
        panic!(
            "{fourcc_label} {width}x{height} {predictor:?} {mode:?} pattern={pattern}: decode failed: {e}"
        )
    });
    assert_eq!(dec.width, width);
    assert_eq!(dec.height, height);
    assert_eq!(dec.planes.len(), planes_in.len());
    for (i, (got, expected)) in dec.planes.iter().zip(planes_in.iter()).enumerate() {
        if got.data != *expected {
            // Print a small diff prefix to ease debugging.
            let n = got.data.len().min(expected.len()).min(32);
            panic!(
                "{fourcc_label} {width}x{height} {predictor:?} {mode:?} pattern={pattern}: \
                 plane {i} mismatch (first {n} bytes)\n  got:      {:02x?}\n  expected: {:02x?}",
                &got.data[..n],
                &expected[..n]
            );
        }
    }
}

const ROUND1_FOURCCS: &[(&str, u8)] = &[
    ("M8RG", 0x65),
    ("M8RA", 0x66),
    ("M8Y4", 0x67),
    ("M8Y2", 0x68),
    ("M8Y0", 0x69),
    ("M8YA", 0x6a),
    ("M8G0", 0x6b),
];

#[test]
fn all_fourccs_left_huffman_zero() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            32,
            28,
            PredictorKind::Left,
            SliceMode::Huffman,
            0,
        );
    }
}

#[test]
fn all_fourccs_gradient_huffman_diag_ramp() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            32,
            28,
            PredictorKind::Gradient,
            SliceMode::Huffman,
            3,
        );
    }
}

#[test]
fn all_fourccs_median_huffman_xor_pattern() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            32,
            28,
            PredictorKind::Median,
            SliceMode::Huffman,
            4,
        );
    }
}

#[test]
fn all_fourccs_left_raw_random() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            32,
            28,
            PredictorKind::Left,
            SliceMode::Raw,
            5,
        );
    }
}

#[test]
fn all_fourccs_gradient_raw_horizontal_ramp() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            32,
            28,
            PredictorKind::Gradient,
            SliceMode::Raw,
            1,
        );
    }
}

#[test]
fn multiple_slices_per_plane_64_high() {
    // 64 rows / 28 slice_height = 3 slices per plane.
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        roundtrip(
            label,
            rec,
            32,
            64,
            28,
            PredictorKind::Median,
            SliceMode::Huffman,
            5,
        );
    }
}

#[test]
fn slice_height_other_than_28() {
    // The decoder must read slice_height from the header, not assume
    // 28 (spec/02 §3, §10 question 1).
    let rec = lookup_round1(0x65).unwrap();
    roundtrip(
        "M8RG",
        rec,
        32,
        16,
        8,
        PredictorKind::Left,
        SliceMode::Huffman,
        3,
    );
    roundtrip(
        "M8RG",
        rec,
        32,
        32,
        16,
        PredictorKind::Gradient,
        SliceMode::Raw,
        1,
    );
}

#[test]
fn small_8x8_image_all_three_predictors() {
    let rec = lookup_round1(0x67).unwrap(); // M8Y4
    for &p in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        for &mode in &[SliceMode::Huffman, SliceMode::Raw] {
            roundtrip("M8Y4", rec, 8, 8, 28, p, mode, 5);
        }
    }
}

#[test]
fn yuv_4_2_0_with_64x32_three_planes() {
    // M8Y0: 4:2:0 chroma. Verifies the chroma plane geometry.
    let rec = lookup_round1(0x69).unwrap();
    roundtrip(
        "M8Y0",
        rec,
        64,
        32,
        28,
        PredictorKind::Median,
        SliceMode::Huffman,
        5,
    );
}

#[test]
fn yuv_4_2_2_with_32x16() {
    let rec = lookup_round1(0x68).unwrap(); // M8Y2
    roundtrip(
        "M8Y2",
        rec,
        32,
        16,
        28,
        PredictorKind::Gradient,
        SliceMode::Huffman,
        5,
    );
}

// ─────────────────────── header validation tests ───────────────────────

#[test]
fn rejects_unsupported_format_byte() {
    // Build a header for M0Y2 (10-bit, 0x6c) — outside round-1 set.
    let mut buf = vec![0u8; 32];
    buf[0..4].copy_from_slice(b"MAGY");
    buf[4..8].copy_from_slice(&32u32.to_le_bytes());
    buf[8] = 7;
    buf[9] = 0x6c;
    buf[10] = 0x0e; // legal for 10-bit but we still reject the format byte.
    buf[11] = 0x02;
    buf[16..20].copy_from_slice(&64u32.to_le_bytes());
    buf[20..24].copy_from_slice(&64u32.to_le_bytes());
    buf[24..28].copy_from_slice(&64u32.to_le_bytes());
    buf[28..32].copy_from_slice(&28u32.to_le_bytes());
    let r = decode_frame(&buf);
    assert!(matches!(r, Err(crate::Error::UnsupportedFormatByte(0x6c))));
}

#[test]
fn rejects_corrupt_predictor_id() {
    // Encode legitimately, then corrupt the predictor_id of slice 0
    // (slice +1 byte) to 0x04 (would-be Dynamic — not on the wire
    // per spec/04 §1.2).
    let rec = lookup_round1(0x6b).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let mut bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![pixels],
        PredictorKind::Left,
        SliceMode::Huffman,
    );
    // Find slice 0's start: header (32) + slice table (4 * 2 = 8) +
    // preamble. Easiest is to read the slice table entry[1] from
    // bytes[32+4..32+8] and add 32 + 1 (the predictor_id is at offset
    // +1 of the slice).
    let entry1 = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
    bytes[32 + entry1 + 1] = 0x04;
    let r = decode_frame(&bytes);
    assert!(matches!(r, Err(crate::Error::BadPredictorId(0x04))));
}

#[test]
fn header_extradata_equals_per_frame_header() {
    // spec/06 §4.1: the strf extradata bytes equal the per-frame
    // MAGY header bytes. Construct a frame and confirm bytes 0..32
    // are a valid header that re-parses to the same values.
    let rec = lookup_round1(0x65).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![pixels.clone(), pixels.clone(), pixels.clone()],
        PredictorKind::Gradient,
        SliceMode::Huffman,
    );
    let h1 = crate::header::parse(&bytes[..32]).unwrap();
    assert_eq!(h1.format_byte, 0x65);
    assert_eq!(h1.aux_byte, 0x0c);
    assert_eq!(h1.width, 16);
    assert_eq!(h1.height, 16);
    assert_eq!(h1.slice_height, 28);
}
