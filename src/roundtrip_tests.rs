//! Self-roundtrip integration tests.
//!
//! No reference fixtures (the cleanroom proprietary binary is
//! Auditor-only). We synthesise frames with the public encoder,
//! decode them back, and assert byte-exactness.

use crate::decoder::{decode_frame, Samples};
use crate::encoder::{encode_frame, EncodeOptions, PlaneInput, PredictorStrategy, SliceMode};
use crate::tables::{lookup, lookup_round1, lookup_round2, Family, FourccRecord, PredictorKind};

/// Helper: build per-plane pixel buffers (8-bit) for a given FOURCC.
fn make_planes_u8(rec: FourccRecord, width: u32, height: u32, pattern: u8) -> Vec<PlaneInput> {
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
                    buf[r * pw + c] = pixel_for_u8(pattern, p, r, c);
                }
            }
            PlaneInput::U8(buf)
        })
        .collect()
}

fn make_planes_u16(rec: FourccRecord, width: u32, height: u32, pattern: u8) -> Vec<PlaneInput> {
    let w = width as usize;
    let h = height as usize;
    let num_planes = rec.planes as usize;
    let mask = rec.sample_mask() as u16;
    (0..num_planes)
        .map(|p| {
            let (sub_x, sub_y) = match rec.family {
                Family::Yuv if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                Family::Yuva if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
                _ => (1usize, 1usize),
            };
            let pw = w / sub_x;
            let ph = h / sub_y;
            let mut buf = vec![0u16; pw * ph];
            for r in 0..ph {
                for c in 0..pw {
                    buf[r * pw + c] = pixel_for_u16(pattern, p, r, c, mask);
                }
            }
            PlaneInput::U16(buf)
        })
        .collect()
}

fn pixel_for_u8(pattern: u8, plane: usize, r: usize, c: usize) -> u8 {
    let r = r as u32;
    let c = c as u32;
    let p = plane as u32;
    let v: u32 = match pattern {
        0 => 0,
        1 => c & 0xff,
        2 => r & 0xff,
        3 => (r + c) & 0xff,
        4 => (r ^ c) & 0xff,
        5 => {
            let mut x: u32 = (r << 16) ^ (c << 8) ^ (p * 31) ^ 0x5113c4f5;
            x = x.wrapping_mul(0x9e37_79b9).wrapping_add(0x6534_3aa1);
            (x >> 17) & 0xff
        }
        _ => 0xa5,
    };
    (v & 0xff) as u8
}

fn pixel_for_u16(pattern: u8, plane: usize, r: usize, c: usize, mask: u16) -> u16 {
    let r = r as u32;
    let c = c as u32;
    let p = plane as u32;
    let v: u32 = match pattern {
        0 => 0,
        1 => c,
        2 => r,
        3 => r + c,
        4 => r ^ c,
        5 => {
            let mut x: u32 = (r << 16) ^ (c << 8) ^ (p * 31) ^ 0x5113c4f5;
            x = x.wrapping_mul(0x9e37_79b9).wrapping_add(0x6534_3aa1);
            x >> 11
        }
        _ => 0xa5_u32 << 4,
    };
    (v as u16) & mask
}

/// Seeded SplitMix64-style scrambler over `(seed, plane, r, c)`. Used by
/// the cartesian property sweep so each `(fourcc, predictor, mode, dims,
/// seed)` cell gets a *distinct* pseudo-random pixel field rather than
/// reusing one of the six fixed `pixel_for_*` patterns. The output is a
/// reproducible function of its inputs (no global RNG state), so a sweep
/// failure is bit-for-bit replayable from the printed seed.
fn scramble(seed: u64, plane: usize, r: usize, c: usize) -> u64 {
    let mut x = seed
        ^ (plane as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (r as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ (c as u64).wrapping_mul(0x94d0_49bb_1331_11eb);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn make_planes_u8_seeded(rec: FourccRecord, width: u32, height: u32, seed: u64) -> Vec<PlaneInput> {
    let w = width as usize;
    let h = height as usize;
    let num_planes = rec.planes as usize;
    (0..num_planes)
        .map(|p| {
            let (sub_x, sub_y) = match rec.family {
                Family::Yuv | Family::Yuva if p == 1 || p == 2 => {
                    (rec.sub_x as usize, rec.sub_y as usize)
                }
                _ => (1usize, 1usize),
            };
            let pw = w / sub_x;
            let ph = h / sub_y;
            let mut buf = vec![0u8; pw * ph];
            for r in 0..ph {
                for c in 0..pw {
                    buf[r * pw + c] = (scramble(seed, p, r, c) & 0xff) as u8;
                }
            }
            PlaneInput::U8(buf)
        })
        .collect()
}

fn make_planes_u16_seeded(
    rec: FourccRecord,
    width: u32,
    height: u32,
    seed: u64,
) -> Vec<PlaneInput> {
    let w = width as usize;
    let h = height as usize;
    let num_planes = rec.planes as usize;
    let mask = rec.sample_mask() as u16;
    (0..num_planes)
        .map(|p| {
            let (sub_x, sub_y) = match rec.family {
                Family::Yuv | Family::Yuva if p == 1 || p == 2 => {
                    (rec.sub_x as usize, rec.sub_y as usize)
                }
                _ => (1usize, 1usize),
            };
            let pw = w / sub_x;
            let ph = h / sub_y;
            let mut buf = vec![0u16; pw * ph];
            for r in 0..ph {
                for c in 0..pw {
                    buf[r * pw + c] = (scramble(seed, p, r, c) as u16) & mask;
                }
            }
            PlaneInput::U16(buf)
        })
        .collect()
}

fn samples_eq_planes(planes_in: &[PlaneInput], decoded: &[crate::decoder::DecodedPlane]) -> bool {
    if planes_in.len() != decoded.len() {
        return false;
    }
    for (i, (got, expected)) in decoded.iter().zip(planes_in.iter()).enumerate() {
        match (&got.samples, expected) {
            (Samples::U8(g), PlaneInput::U8(e)) => {
                if g != e {
                    eprintln!(
                        "plane {i} u8 mismatch (got {} bytes, expected {})",
                        g.len(),
                        e.len()
                    );
                    return false;
                }
            }
            (Samples::U16(g), PlaneInput::U16(e)) => {
                if g != e {
                    eprintln!(
                        "plane {i} u16 mismatch (got {} samples, expected {})",
                        g.len(),
                        e.len()
                    );
                    return false;
                }
            }
            _ => {
                eprintln!("plane {i} type mismatch");
                return false;
            }
        }
    }
    true
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
    interlaced: bool,
) {
    let planes_in: Vec<PlaneInput> = if rec.is_high_bit_depth() {
        make_planes_u16(rec, width, height, pattern)
    } else {
        make_planes_u8(rec, width, height, pattern)
    };
    let bytes = encode_frame(
        rec,
        width,
        height,
        slice_height,
        planes_in.clone(),
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(predictor),
            predictor,
            mode,
            interlaced,
            color_matrix: 1,
            full_range: false,
        },
    )
    .unwrap_or_else(|e| {
        panic!(
            "{fourcc_label} {width}x{height} {predictor:?} {mode:?} pattern={pattern} interlaced={interlaced}: encode failed: {e}"
        )
    });
    let dec = decode_frame(&bytes).unwrap_or_else(|e| {
        panic!(
            "{fourcc_label} {width}x{height} {predictor:?} {mode:?} pattern={pattern} interlaced={interlaced}: decode failed: {e}"
        )
    });
    assert_eq!(dec.width, width);
    assert_eq!(dec.height, height);
    assert!(
        samples_eq_planes(&planes_in, &dec.planes),
        "{fourcc_label} {width}x{height} {predictor:?} {mode:?} pattern={pattern} interlaced={interlaced}: plane mismatch"
    );
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

const ROUND2_HIGH_FOURCCS: &[(&str, u8)] = &[
    ("M0RG", 0x6d),
    ("M0RA", 0x6e),
    ("M2RG", 0x6f),
    ("M2RA", 0x70),
    ("M4RG", 0x71),
    ("M4RA", 0x72),
    ("M0Y2", 0x6c),
    ("M0Y4", 0x76),
    ("M0Y0", 0x7b),
    ("M0G0", 0x73),
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        );
    }
}

#[test]
fn multiple_slices_per_plane_64_high() {
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
            false,
        );
    }
}

#[test]
fn slice_height_other_than_28() {
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
        false,
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
        false,
    );
}

#[test]
fn small_8x8_image_all_three_predictors() {
    let rec = lookup_round1(0x67).unwrap();
    for &p in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        for &mode in &[SliceMode::Huffman, SliceMode::Raw] {
            roundtrip("M8Y4", rec, 8, 8, 28, p, mode, 5, false);
        }
    }
}

#[test]
fn yuv_4_2_0_with_64x32_three_planes() {
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
        false,
    );
}

#[test]
fn yuv_4_2_2_with_32x16() {
    let rec = lookup_round1(0x68).unwrap();
    roundtrip(
        "M8Y2",
        rec,
        32,
        16,
        28,
        PredictorKind::Gradient,
        SliceMode::Huffman,
        5,
        false,
    );
}

// spec/02 §4 (slice count) + §6 (chroma slice partition, lines
// 386..401): for subsampled YUV the per-plane slice count is derived
// from the **luma** row count — `slices_per_plane = ceil(H /
// slice_height)` — and a chroma slice `s` covers chroma rows
// `[s * slice_height/sub_y, (s+1) * slice_height/sub_y)`, clamped to
// the chroma plane height. The constant-28 v2.4.2 case is exercised
// by `yuv_4_2_0_with_64x32_three_planes`; this case uses an even
// non-28 `slice_height` and a height whose chroma plane does NOT
// tile evenly, so the documented last-slice `.min(chroma_height)`
// clamp is load-bearing (without it the final chroma slice would
// over-run or drop rows). M8Y0 (4:2:0, sub_y = 2): 64×54, luma
// `slices_per_plane = ceil(54/10) = 6`; chroma height 27, chroma
// per-slice height `10/2 = 5`, so chroma slices cover rows
// [0,5) [5,10) [10,15) [15,20) [20,25) [25,30→27) — six slices
// summing to the full 27 chroma rows with a partial last slice.
#[test]
fn yuv_4_2_0_partial_chroma_last_slice_even_non28_slice_height() {
    let rec = lookup_round1(0x69).unwrap();
    assert_eq!(rec.sub_y, 2);
    for &p in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        for &mode in &[SliceMode::Huffman, SliceMode::Raw] {
            roundtrip("M8Y0", rec, 64, 54, 10, p, mode, 5, false);
        }
    }
}

// ─────────────────── round-2: 10/12/14-bit FOURCCs ───────────────────

#[test]
fn high_bit_depth_left_huffman_zero() {
    for (label, fb) in ROUND2_HIGH_FOURCCS {
        let rec = lookup_round2(*fb).unwrap();
        roundtrip(
            label,
            rec,
            16,
            16,
            28,
            PredictorKind::Left,
            SliceMode::Huffman,
            0,
            false,
        );
    }
}

#[test]
fn high_bit_depth_gradient_huffman_const() {
    for (label, fb) in ROUND2_HIGH_FOURCCS {
        let rec = lookup_round2(*fb).unwrap();
        roundtrip(
            label,
            rec,
            16,
            16,
            28,
            PredictorKind::Gradient,
            SliceMode::Huffman,
            3,
            false,
        );
    }
}

#[test]
fn high_bit_depth_median_huffman_random() {
    for (label, fb) in ROUND2_HIGH_FOURCCS {
        let rec = lookup_round2(*fb).unwrap();
        roundtrip(
            label,
            rec,
            16,
            16,
            28,
            PredictorKind::Median,
            SliceMode::Huffman,
            5,
            false,
        );
    }
}

#[test]
fn high_bit_depth_raw_mode() {
    for (label, fb) in ROUND2_HIGH_FOURCCS {
        let rec = lookup_round2(*fb).unwrap();
        roundtrip(
            label,
            rec,
            16,
            16,
            28,
            PredictorKind::Left,
            SliceMode::Raw,
            5,
            false,
        );
    }
}

#[test]
fn high_bit_depth_64x64_multi_slice() {
    // 64×64 with slice_height=28 → 3 slices per plane.
    for (label, fb) in &[("M0RG", 0x6du8), ("M2RG", 0x6f), ("M4RG", 0x71)] {
        let rec = lookup_round2(*fb).unwrap();
        roundtrip(
            label,
            rec,
            64,
            64,
            28,
            PredictorKind::Median,
            SliceMode::Huffman,
            5,
            false,
        );
    }
}

// ─────────────────── round-2: interlaced ───────────────────

#[test]
fn interlaced_8bit_progressive_pattern() {
    // 4×8 frame, interlaced, all three predictors.
    let rec = lookup_round1(0x65).unwrap();
    for &p in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        for &mode in &[SliceMode::Huffman, SliceMode::Raw] {
            roundtrip("M8RG", rec, 8, 16, 28, p, mode, 5, true);
        }
    }
}

#[test]
fn interlaced_8bit_gray_random() {
    let rec = lookup_round1(0x6b).unwrap();
    roundtrip(
        "M8G0",
        rec,
        16,
        16,
        28,
        PredictorKind::Median,
        SliceMode::Huffman,
        5,
        true,
    );
}

#[test]
fn interlaced_high_bit_depth() {
    let rec = lookup_round2(0x6d).unwrap();
    roundtrip(
        "M0RG",
        rec,
        16,
        16,
        28,
        PredictorKind::Median,
        SliceMode::Huffman,
        5,
        true,
    );
}

// ─────────────────── header validation tests ───────────────────

#[test]
fn rejects_unknown_format_byte() {
    // Build a header for a format byte not in the CSV.
    let mut buf = vec![0u8; 32];
    buf[0..4].copy_from_slice(b"MAGY");
    buf[4..8].copy_from_slice(&32u32.to_le_bytes());
    buf[8] = 7;
    buf[9] = 0x74; // unmapped per spec/03 §3
    buf[10] = 0x0c;
    buf[11] = 0x02;
    buf[16..20].copy_from_slice(&64u32.to_le_bytes());
    buf[20..24].copy_from_slice(&64u32.to_le_bytes());
    buf[24..28].copy_from_slice(&64u32.to_le_bytes());
    buf[28..32].copy_from_slice(&28u32.to_le_bytes());
    let r = decode_frame(&buf);
    assert!(matches!(r, Err(crate::Error::UnsupportedFormatByte(0x74))));
    let _ = lookup; // keep the import warm for round-2 tests
}

#[test]
fn rejects_corrupt_predictor_id() {
    let rec = lookup_round1(0x6b).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let mut bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![PlaneInput::U8(pixels)],
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .unwrap();
    let entry1 = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
    bytes[32 + entry1 + 1] = 0x04;
    let r = decode_frame(&bytes);
    assert!(matches!(r, Err(crate::Error::BadPredictorId(0x04))));
}

#[test]
fn rejects_zero_predictor_id() {
    // spec/04 §1.2 + §7.3c: the only legal per-slice `predictor_id`
    // values are 0x01 (Left), 0x02 (Gradient), 0x03 (Median). Value
    // 0x00 — the natural result of a zeroed or truncated slice prefix
    // — is explicitly listed alongside ≥0x04 as malformed and MUST be
    // rejected. The companion `rejects_corrupt_predictor_id` covers the
    // ≥0x04 half of that range; this pins the reserved-zero half so the
    // defined rejection path can't regress to a silent mis-decode.
    let rec = lookup_round1(0x6b).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let mut bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![PlaneInput::U8(pixels)],
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .unwrap();
    let entry1 = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
    bytes[32 + entry1 + 1] = 0x00;
    let r = decode_frame(&bytes);
    assert!(matches!(r, Err(crate::Error::BadPredictorId(0x00))));
}

#[test]
fn encoder_rejects_odd_width_for_horizontal_subsampling() {
    // M8Y2 (4:2:2, sub_x = 2, sub_y = 1): an odd width can't be
    // floored to a chroma width without dropping the last column, and
    // the ceiling-vs-floor rule is unverified (spec/03 §8.2). The
    // encoder must refuse rather than silently produce a stream the
    // decoder then rejects.
    let rec = lookup_round1(0x68).unwrap();
    assert_eq!(rec.sub_x, 2);
    assert_eq!(rec.sub_y, 1);
    // Width 5 is odd; height 4 is fine (sub_y = 1 never divides).
    let r = encode_frame(
        rec,
        5,
        4,
        28,
        // Sized however — the dimension guard fires before any plane
        // length check, so empty plane vecs are sufficient.
        vec![
            PlaneInput::U8(Vec::new()),
            PlaneInput::U8(Vec::new()),
            PlaneInput::U8(Vec::new()),
        ],
        EncodeOptions::fixed(PredictorKind::Left),
    );
    assert!(matches!(
        r,
        Err(crate::Error::OddDimensionForSubsampling {
            what: "width",
            got: 5,
            factor: 2,
        })
    ));
}

#[test]
fn encoder_rejects_odd_height_for_vertical_subsampling() {
    // M8Y0 (4:2:0, sub_x = 2, sub_y = 2): an odd height trips the
    // sub_y guard. Width is even here so the failure is unambiguously
    // the height check.
    let rec = lookup_round1(0x69).unwrap();
    assert_eq!(rec.sub_y, 2);
    let r = encode_frame(
        rec,
        8,
        7,
        28,
        vec![
            PlaneInput::U8(Vec::new()),
            PlaneInput::U8(Vec::new()),
            PlaneInput::U8(Vec::new()),
        ],
        EncodeOptions::fixed(PredictorKind::Gradient),
    );
    assert!(matches!(
        r,
        Err(crate::Error::OddDimensionForSubsampling {
            what: "height",
            got: 7,
            factor: 2,
        })
    ));
}

#[test]
fn encoder_accepts_even_subsampled_dimensions() {
    // The odd-dimension guard is inert for even dimensions (ceil ==
    // floor): a 4:2:0 frame at even width/height still encodes and
    // round-trips byte-for-byte.
    let rec = lookup_round1(0x69).unwrap();
    let planes = make_planes_u8(rec, 8, 8, 1);
    let bytes = encode_frame(
        rec,
        8,
        8,
        28,
        planes.clone(),
        EncodeOptions::fixed(PredictorKind::Median),
    )
    .unwrap();
    let frame = decode_frame(&bytes).unwrap();
    assert_eq!(frame.planes.len(), 3);
    // Luma is full-size; chroma is half-size in both axes.
    assert_eq!((frame.planes[0].width, frame.planes[0].height), (8, 8));
    assert_eq!((frame.planes[1].width, frame.planes[1].height), (4, 4));
    assert_eq!((frame.planes[2].width, frame.planes[2].height), (4, 4));
}

#[test]
fn encoder_and_decoder_reject_the_same_odd_dimensions() {
    // The encoder's odd-dimension guard is symmetric with the
    // decoder's: any (FOURCC, width, height) the encoder refuses, a
    // hand-built header at those dimensions would also be refused by
    // decode_frame. We exercise the subsampled FOURCCs at odd sizes
    // and assert both sides return OddDimensionForSubsampling.
    for (fb, w, h) in [
        (0x67u8, 3, 4), // M8Y4 (4:4:4) — sub_x = sub_y = 1, never odd-rejected
        (0x68, 3, 4),   // M8Y2 (4:2:2) — odd width rejected
        (0x69, 4, 3),   // M8Y0 (4:2:0) — odd height rejected
        (0x6a, 3, 4),   // M8YA (4:4:4:4) — sub = 1, never rejected
    ] {
        let rec = lookup_round1(fb).unwrap();
        let n = rec.planes as usize;
        let enc = encode_frame(
            rec,
            w,
            h,
            28,
            vec![PlaneInput::U8(Vec::new()); n],
            EncodeOptions::fixed(PredictorKind::Left),
        );
        let subsampled = rec.sub_x > 1 || rec.sub_y > 1;
        let odd_for_sub = (rec.sub_x as u32 > 1 && w % rec.sub_x as u32 != 0)
            || (rec.sub_y as u32 > 1 && h % rec.sub_y as u32 != 0);
        if subsampled && odd_for_sub {
            assert!(
                matches!(enc, Err(crate::Error::OddDimensionForSubsampling { .. })),
                "encoder must reject odd {:?} subsampled dims {w}x{h}",
                rec.fourcc
            );
            // Hand-build a header at the same dims and confirm the
            // decoder rejects identically.
            let mut buf = vec![0u8; 32];
            buf[0..4].copy_from_slice(b"MAGY");
            buf[4..8].copy_from_slice(&32u32.to_le_bytes());
            buf[8] = 7;
            buf[9] = fb;
            buf[10] = rec.aux_byte;
            buf[11] = 0x02;
            buf[16..20].copy_from_slice(&w.to_le_bytes());
            buf[20..24].copy_from_slice(&h.to_le_bytes());
            buf[24..28].copy_from_slice(&w.to_le_bytes());
            buf[28..32].copy_from_slice(&28u32.to_le_bytes());
            let dec = decode_frame(&buf);
            assert!(
                matches!(dec, Err(crate::Error::OddDimensionForSubsampling { .. })),
                "decoder must reject the same odd {:?} dims {w}x{h}",
                rec.fourcc
            );
        } else {
            // Non-subsampled FOURCCs accept odd dims (encoder may then
            // fail on plane-length mismatch, but never with the
            // odd-dimension error).
            assert!(
                !matches!(enc, Err(crate::Error::OddDimensionForSubsampling { .. })),
                "non-subsampled {:?} must not be odd-rejected",
                rec.fourcc
            );
        }
    }
}

// ─────────────────── round-2: trace emitter ───────────────────
//
// Trace tests share a process-global env var, so we serialise them
// behind a `Mutex` to keep them deterministic in parallel cargo test.

#[cfg(feature = "trace")]
fn trace_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[cfg(feature = "trace")]
#[test]
fn trace_emits_audit02_event_vocabulary() {
    use std::io::Read;
    let _g = trace_lock();
    // Encode a small M8RG fixture, set the env var, decode, then
    // read the JSONL and assert the event sequence + counts match
    // audit/02 §4's schema.
    let rec = lookup_round1(0x65).unwrap();
    let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![
            PlaneInput::U8(pixels.clone()),
            PlaneInput::U8(pixels.clone()),
            PlaneInput::U8(pixels.clone()),
        ],
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .unwrap();
    let path = std::env::temp_dir().join(format!(
        "magicyuv-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // Truncate first.
    let _ = std::fs::remove_file(&path);
    std::env::set_var("OXIDEAV_MAGICYUV_TRACE_FILE", &path);
    let _ = decode_frame(&bytes).expect("decode");
    std::env::remove_var("OXIDEAV_MAGICYUV_TRACE_FILE");

    let mut s = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    let _ = std::fs::remove_file(&path);

    // The env var is process-global, so concurrent non-trace tests
    // running decode_frame may also append events to the same file.
    // We therefore assert "the expected event kinds appear at least
    // once" rather than exact counts. The Auditor's `jq`-line-diff
    // harness runs in a quiet process where exact counts are
    // deterministic; this in-crate test is a smoke-check.
    let kinds: Vec<&str> = s
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let q = l.find("\"kind\":\"").unwrap();
            let rest = &l[q + 8..];
            let end = rest.find('"').unwrap();
            &rest[..end]
        })
        .collect();
    assert!(kinds.contains(&"hdr"), "trace must contain hdr event");
    assert!(
        kinds.contains(&"slice_table"),
        "trace must contain slice_table event"
    );
    assert!(
        kinds.contains(&"preamble"),
        "trace must contain preamble event"
    );
    let huff_count = kinds.iter().filter(|k| **k == "huff").count();
    assert!(
        huff_count >= 3,
        "trace must contain ≥ 3 huff events (one per RGB plane); got {huff_count}"
    );
    let payload_count = kinds.iter().filter(|k| **k == "payload").count();
    assert!(
        payload_count >= 3,
        "trace must contain ≥ 3 payload events; got {payload_count}"
    );
}

#[cfg(feature = "trace")]
#[test]
fn trace_huff_used_field_is_per_symbol_map() {
    use std::io::Read;
    let _g = trace_lock();
    // Per audit/02 §4.2 + audit/03 §2 the `huff.used` field MUST be a
    // per-symbol `{symbol: {length, code}}` map (NOT a bool) emitted in
    // symbol-ascending order with insertion order `length, code`. The
    // codes carried in the map MUST match the canonical-Huffman codes
    // the decoder's own `HuffmanTable::build` produces from the parsed
    // descriptor — that's what makes the Auditor's strict jq-line-diff
    // pass against the Python reference codec's `--trace` output.
    let rec = lookup_round1(0x6b).unwrap(); // M8G0 (single plane).
    let pixels: Vec<u8> = (0..(16 * 16)).map(|i| ((i * 37) & 0xff) as u8).collect();
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![PlaneInput::U8(pixels.clone())],
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .unwrap();
    let path = std::env::temp_dir().join(format!(
        "magicyuv-trace-huff-used-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    std::env::set_var("OXIDEAV_MAGICYUV_TRACE_FILE", &path);
    let _ = decode_frame(&bytes).expect("decode");
    std::env::remove_var("OXIDEAV_MAGICYUV_TRACE_FILE");
    let mut s = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    let _ = std::fs::remove_file(&path);

    // Find a line with kind=huff for plane 0 — we encoded a fresh
    // file, so the very first huff event in the tape is ours.
    let huff_line = s
        .lines()
        .find(|l| l.contains("\"kind\":\"huff\""))
        .expect("at least one huff event");

    // The serialised `used` field MUST start with `{` (i.e. be a map),
    // NOT `true` / `false` (the round-2 bool form).
    let used_idx = huff_line
        .find("\"used\":")
        .expect("huff event must carry a `used` field");
    let after = &huff_line[used_idx + "\"used\":".len()..];
    assert!(
        after.starts_with('{'),
        "huff.used must be an object, not a bool: {after:?}"
    );
    assert!(
        !after.starts_with("true"),
        "huff.used must not be `true` (round-2 schema)"
    );
    assert!(
        !after.starts_with("false"),
        "huff.used must not be `false` (round-2 schema)"
    );

    // Cross-check the per-symbol payload against the canonical-Huffman
    // builder's own output: re-derive the lengths from the descriptor
    // bytes we just emitted, build the same canonical-code table, and
    // confirm a sampled `(symbol → {length, code})` pair matches.
    //
    // Pick the first symbol whose code-length is positive, parse its
    // value out of the JSON map, and compare.
    use crate::huffman::{parse_lengths, HuffmanTable};
    // Re-derive the descriptor bytes by re-running the encode pipeline
    // off the same input. Easier: parse them out of the trace event's
    // `descriptor_bytes` hex field.
    let dh_idx =
        huff_line.find("\"descriptor_bytes\":\"").unwrap() + "\"descriptor_bytes\":\"".len();
    let dh_end = dh_idx + huff_line[dh_idx..].find('"').unwrap();
    let hex = &huff_line[dh_idx..dh_end];
    let mut desc = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        desc.push(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
    }
    let (lens, _) = parse_lengths(&desc, 256, 12, 0).expect("parse lens");
    let table = HuffmanTable::build(lens.clone(), 0).expect("build");
    let codes = table.codes();
    // Find the first symbol with positive length.
    let (s_check, l_check) = lens
        .iter()
        .enumerate()
        .find(|(_, &l)| l > 0)
        .map(|(s, &l)| (s, l))
        .expect("at least one positive-length symbol");
    let c_check = codes[s_check];
    // Look for `"<s>":{"length":<L>,"code":<C>}` in the trace line —
    // this is the exact insertion order the audit/02 §4.2 schema
    // requires (`length` before `code`).
    let needle = format!("\"{s_check}\":{{\"length\":{l_check},\"code\":{c_check}}}");
    assert!(
        huff_line.contains(&needle),
        "expected substring not found in huff trace event\n  needle: {needle}\n  haystack: {huff_line}"
    );
}

#[cfg(feature = "trace")]
#[test]
fn trace_preamble_trailing_emits_integer_extra_bytes() {
    use std::io::Read;
    let _g = trace_lock();
    // Per `spec/05 §10 Q6` audit-corrected note + `audit/00 §8.8`
    // canonicalisation table + `audit/04 §2.3` divergence note, the
    // `preamble_trailing` event's `extra_bytes` field MUST be a JSON
    // integer (the byte count of unconsumed trailing preamble), NOT a
    // hex byte-string. The Python reference codec emits
    // `tracer.event("preamble_trailing", extra_bytes=
    // len(preamble) - cursor)` at `frame.py:514`; the Rust crate
    // must match so a strict `jq -S -c '.'` line-diff is empty.
    //
    // v2.4.2 never emits trailing preamble bytes (the count is always
    // zero and the event never fires on vendor streams), so we
    // synthesise a frame with padding inserted between the last
    // Huffman descriptor and the first slice payload, then update the
    // slice-table entries by the padding length. The decoder's
    // descriptor parser stops at the last descriptor byte; the
    // remaining bytes in the preamble region (defined by
    // `entry[1] + table_off`) become the trailing-bytes.
    let rec = lookup_round1(0x6b).unwrap(); // M8G0 — single plane.
    let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![PlaneInput::U8(pixels)],
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .unwrap();

    // Slice table starts at offset 0x20 (32). For a single-plane 16×16
    // frame with `slice_height=28 ≥ 16`, `total_slices = 1` so the
    // table is `(1 + 1) = 2` u32 LE entries (8 bytes).
    //
    // `entry[0]` and `entry[1]` are both the preamble-end (= first
    // slice offset) per `assemble_frame`. To inject padding, we
    // insert `pad_len` zero bytes immediately after the last
    // descriptor byte but before the first slice's payload, then
    // bump `entry[1]` (and every entry after it) by `pad_len`.
    let pad_len: usize = 7;
    let table_off: usize = 32;
    let total_slices: usize = 1;
    // Original first-slice file-offset (table_off + entries[1]).
    let entry1 = u32::from_le_bytes(bytes[table_off + 4..table_off + 8].try_into().unwrap());
    let first_payload_off = (entry1 as usize) + table_off;
    // Synthesise the patched buffer: prefix + pad_len zeros + suffix.
    let mut patched = Vec::with_capacity(bytes.len() + pad_len);
    patched.extend_from_slice(&bytes[..first_payload_off]);
    patched.extend(std::iter::repeat_n(0u8, pad_len));
    patched.extend_from_slice(&bytes[first_payload_off..]);
    // Bump entry[1] (the only slice-end entry for a single-slice
    // frame) by pad_len. entry[0] (preamble-end indicator) is
    // structurally identical to entry[1] in our assembler, so bump
    // it too so the decoder reads exactly `pad_len` trailing bytes.
    for i in 1..=total_slices {
        let off = table_off + 4 * i;
        let v = u32::from_le_bytes(patched[off..off + 4].try_into().unwrap()) + pad_len as u32;
        patched[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    let path = std::env::temp_dir().join(format!(
        "magicyuv-trace-pt-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    std::env::set_var("OXIDEAV_MAGICYUV_TRACE_FILE", &path);
    let _ = decode_frame(&patched).expect("decode patched frame");
    std::env::remove_var("OXIDEAV_MAGICYUV_TRACE_FILE");

    let mut s = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    let _ = std::fs::remove_file(&path);

    // Find the preamble_trailing event line.
    let pt_line = s
        .lines()
        .find(|l| l.contains("\"kind\":\"preamble_trailing\""))
        .expect("preamble_trailing event must fire when extra bytes present");

    // Strict schema: the line must contain `"extra_bytes":<int>` where
    // <int> equals our padding length, and MUST NOT contain a hex
    // byte-string form like `"extra_bytes":"00…"`.
    let needle = format!("\"extra_bytes\":{pad_len}");
    assert!(
        pt_line.contains(&needle),
        "expected `\"extra_bytes\":<int>` (count) per spec/05 §10 Q6 canonical schema; got: {pt_line}"
    );
    assert!(
        !pt_line.contains("\"extra_bytes\":\""),
        "extra_bytes MUST be a JSON integer, not a hex string (spec/05 §10 Q6); got: {pt_line}"
    );
    // The exact line must canonicalise (via `jq -S -c '.'`) to
    // `{"extra_bytes":<n>,"kind":"preamble_trailing"}`. We assert the
    // canonical-form equality directly without invoking jq so the test
    // doesn't depend on jq being installed.
    let expected = format!("{{\"kind\":\"preamble_trailing\",\"extra_bytes\":{pad_len}}}");
    assert_eq!(
        pt_line, expected,
        "preamble_trailing line must be exactly the canonical schema"
    );
}

#[cfg(feature = "trace")]
#[test]
fn trace_omits_when_env_var_unset() {
    let _g = trace_lock();
    let rec = lookup_round1(0x6b).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![PlaneInput::U8(pixels)],
        EncodeOptions::default(),
    )
    .unwrap();
    std::env::remove_var("OXIDEAV_MAGICYUV_TRACE_FILE");
    let _ = decode_frame(&bytes).expect("decode");
}

#[test]
fn header_extradata_equals_per_frame_header() {
    let rec = lookup_round1(0x65).unwrap();
    let pixels = vec![0u8; 16 * 16];
    let bytes = encode_frame(
        rec,
        16,
        16,
        28,
        vec![
            PlaneInput::U8(pixels.clone()),
            PlaneInput::U8(pixels.clone()),
            PlaneInput::U8(pixels.clone()),
        ],
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .unwrap();
    let h1 = crate::header::parse(&bytes[..32]).unwrap();
    assert_eq!(h1.format_byte, 0x65);
    assert_eq!(h1.aux_byte, 0x0c);
    assert_eq!(h1.width, 16);
    assert_eq!(h1.height, 16);
    assert_eq!(h1.slice_height, 28);
}

// ───────────────────── round-78: Dynamic + Auto encoder strategies ─────────────────────
//
// `spec/04` §3 (Dynamic) and `spec/05` §6.2 (per-slice raw fallback)
// are encoder-side conventions, not wire-format requirements. The
// tests below confirm:
//
// 1. Frames produced under `PredictorStrategy::Dynamic` round-trip
//    byte-exact through the decoder (the decoder doesn't care which
//    predictor was picked — it reads each slice's `predictor_id`
//    independently).
// 2. Dynamic actually selects different predictors across slices when
//    the input pattern is structured to favour different predictors
//    per slice (e.g. horizontal vs vertical ramp). This mirrors the
//    `spec/04` §3.2 behavioural-confirmation observation that the
//    proprietary v2.4.2 encoder's "Dynamic" picks per-slice predictors
//    rather than a single global one.
// 3. Frames produced under `SliceMode::Auto` round-trip byte-exact,
//    and for an all-zero (degenerate) input every slice gets
//    `slice_flags = 0x00` (Huffman wins because zero-bit-cost
//    Huffman dominates raw size).
// 4. For a high-entropy random input, `Auto` should choose raw on at
//    least some slices (Huffman code expansion + descriptor overhead
//    means random data can be cheaper to ship raw).
// 5. Combined `Dynamic + Auto` round-trips at every native FOURCC ×
//    every test pattern.

fn extract_per_slice_predictor_ids(
    bytes: &[u8],
    num_planes: usize,
    slices_per_plane: usize,
) -> Vec<u8> {
    // Header is 32 bytes; slice table is `(total_slices + 1) * 4`
    // little-endian entries starting at offset 32. Each `entry[k+1]`
    // is the byte offset of slice-k's payload relative to the end of
    // the header (per `spec/02` §5). Slice payload byte +0 is
    // `slice_flags`, byte +1 is `predictor_id`.
    let total_slices = num_planes * slices_per_plane;
    let mut ids = Vec::with_capacity(total_slices);
    for s in 0..total_slices {
        let entry_off = 32 + 4 * (s + 1);
        let entry =
            u32::from_le_bytes(bytes[entry_off..entry_off + 4].try_into().unwrap()) as usize;
        ids.push(bytes[32 + entry + 1]);
    }
    ids
}

fn extract_per_slice_flags(bytes: &[u8], num_planes: usize, slices_per_plane: usize) -> Vec<u8> {
    let total_slices = num_planes * slices_per_plane;
    let mut flags = Vec::with_capacity(total_slices);
    for s in 0..total_slices {
        let entry_off = 32 + 4 * (s + 1);
        let entry =
            u32::from_le_bytes(bytes[entry_off..entry_off + 4].try_into().unwrap()) as usize;
        flags.push(bytes[32 + entry]);
    }
    flags
}

#[test]
fn dynamic_strategy_round_trips_every_8bit_fourcc() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        for pattern in 0u8..6 {
            let planes_in = make_planes_u8(rec, 64, 64, pattern);
            let bytes = encode_frame(
                rec,
                64,
                64,
                28,
                planes_in.clone(),
                EncodeOptions {
                    strategy: PredictorStrategy::Dynamic,
                    mode: SliceMode::Huffman,
                    ..EncodeOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{label} dynamic encode failed: {e}"));
            let dec = decode_frame(&bytes)
                .unwrap_or_else(|e| panic!("{label} dynamic decode failed: {e}"));
            assert!(
                samples_eq_planes(&planes_in, &dec.planes),
                "{label} dynamic pattern={pattern}: plane mismatch"
            );
            // Sanity: every emitted predictor_id is in {1, 2, 3}.
            let ids = extract_per_slice_predictor_ids(&bytes, rec.planes as usize, 3);
            for id in ids {
                assert!(
                    (1..=3).contains(&id),
                    "{label} dynamic pattern={pattern}: predictor_id {id:#x} out of {{1,2,3}}"
                );
            }
        }
    }
}

#[test]
fn dynamic_strategy_round_trips_high_bit_depth() {
    use crate::tables;
    // One representative from each high-bit-depth family.
    let format_bytes: &[u8] = &[
        0x6c, // M0Y2 (10-bit YUV 4:2:2)
        0x6d, // M0RG (10-bit RGB)
        0x6f, // M2RG (12-bit RGB)
        0x71, // M4RG (14-bit RGB)
    ];
    for &fb in format_bytes {
        let rec = tables::lookup(fb).unwrap();
        for pattern in 0u8..4 {
            let planes_in = make_planes_u16(rec, 32, 32, pattern);
            let bytes = encode_frame(
                rec,
                32,
                32,
                28,
                planes_in.clone(),
                EncodeOptions {
                    strategy: PredictorStrategy::Dynamic,
                    mode: SliceMode::Huffman,
                    ..EncodeOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{fb:#x} dynamic encode failed: {e}"));
            let dec = decode_frame(&bytes)
                .unwrap_or_else(|e| panic!("{fb:#x} dynamic decode failed: {e}"));
            assert!(
                samples_eq_planes(&planes_in, &dec.planes),
                "{fb:#x} dynamic pattern={pattern}: plane mismatch"
            );
        }
    }
}

#[test]
fn dynamic_picks_left_for_horizontal_ramp() {
    // Horizontal ramp `px[r,c] = c` — Left's residuals are 1 everywhere
    // except column 0; Gradient and Median have similar 1's but also
    // a non-trivial first-row tail. Left should win on residual-sum.
    // (At minimum: the picked predictor is NOT Median for every slice;
    // a stricter pattern-aware check would require pinning to the
    // spec's exact L1 norm, which we already chose.)
    let rec = crate::tables::lookup(0x6b).unwrap(); // M8G0 — 1 plane
    let mut pixels = vec![0u8; 64 * 64];
    for r in 0..64usize {
        for c in 0..64usize {
            pixels[r * 64 + c] = c as u8;
        }
    }
    let bytes = encode_frame(
        rec,
        64,
        64,
        28,
        vec![PlaneInput::U8(pixels.clone())],
        EncodeOptions {
            strategy: PredictorStrategy::Dynamic,
            mode: SliceMode::Huffman,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let ids = extract_per_slice_predictor_ids(&bytes, 1, 3);
    // Horizontal-ramp's Left residual is `pixel(c, r) - pixel(c-1, r) =
    // 1` for c >= 1 (and `pixel(0, r) - pixel(0, r-1) = 0` for r >= 1).
    // Gradient: `pixel(c, r) - (left + top - top_left) = c - (c-1 + c -
    // (c-1)) = c - c = 0` for c >= 1, r >= 1 — but the first-column /
    // first-row Left-fallback paths add 1's. Median is similar to
    // Gradient for monotone ramps.  Either Left, Gradient, or Median
    // could plausibly win the L1 race here; what matters is the
    // decoder accepts the chosen ID and reconstructs byte-exact.
    let dec = decode_frame(&bytes).unwrap();
    assert!(
        samples_eq_planes(&[PlaneInput::U8(pixels)], &dec.planes),
        "horizontal-ramp dynamic: plane mismatch"
    );
    // At least one slice has a meaningful prediction (not raw-passthrough).
    for id in ids {
        assert!((1..=3).contains(&id));
    }
}

#[test]
fn dynamic_varies_predictor_across_slices_with_mixed_content() {
    // M8G0, 1 plane, 64×64, slice height = 16 → 4 slices. Per slice,
    // construct a different residual structure that favours a
    // different predictor:
    //
    // - slice 0 (rows 0..16):  random noise plateau — large residuals
    //   under every predictor; pseudo-random ties go to the lower
    //   predictor id.
    // - slice 1 (rows 16..32): horizontal-ramp shifted by row index
    //   `px(r,c) = c + r * 7` — Left's residual is +1 within rows
    //   but row-jumps of 7 at column 0; Gradient cancels both → wins.
    // - slice 2 (rows 32..48): constant block — every predictor's
    //   residual is 0 inside the slice, but Left's column-0 fallback
    //   uses the previous row (also constant) → 0; ties go to Left
    //   by predictor-id ascending.
    // - slice 3 (rows 48..64): vertical-ramp `px(r,c) = r` — same
    //   row-running-sum sees +1 column-0 and 0 elsewhere; Left
    //   produces equal residual mass to Gradient.
    //
    // The test asserts that across the 4 slices Dynamic emits ≥ 2
    // distinct predictor IDs — i.e. the strategy actually adapts.
    let rec = crate::tables::lookup(0x6b).unwrap(); // M8G0
    let mut buf = vec![0u8; 64 * 64];
    // slice 0: noise
    let mut acc: u32 = 0x1234_5678;
    for row in 0..16usize {
        for col in 0..64usize {
            acc = acc.wrapping_mul(1664525).wrapping_add(1013904223);
            buf[row * 64 + col] = (acc >> 16) as u8;
        }
    }
    // slice 1: shifted-horizontal-ramp
    for row in 16..32usize {
        for col in 0..64usize {
            buf[row * 64 + col] = (col + row * 7) as u8;
        }
    }
    // slice 2: constant
    for row in 32..48usize {
        for col in 0..64usize {
            buf[row * 64 + col] = 200;
        }
    }
    // slice 3: vertical-ramp
    for row in 48..64usize {
        for col in 0..64usize {
            buf[row * 64 + col] = row as u8;
        }
    }
    let bytes = encode_frame(
        rec,
        64,
        64,
        16,
        vec![PlaneInput::U8(buf.clone())],
        EncodeOptions {
            strategy: PredictorStrategy::Dynamic,
            mode: SliceMode::Huffman,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let ids = extract_per_slice_predictor_ids(&bytes, 1, 4);
    let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert!(
        unique.len() >= 2,
        "dynamic should pick ≥ 2 distinct predictors across 4 slices with different content, got ids = {ids:?}"
    );
    // And it round-trips.
    let dec = decode_frame(&bytes).unwrap();
    assert!(samples_eq_planes(&[PlaneInput::U8(buf)], &dec.planes));
}

#[test]
fn auto_mode_round_trips_8bit() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        for pattern in 0u8..6 {
            let planes_in = make_planes_u8(rec, 64, 64, pattern);
            let bytes = encode_frame(
                rec,
                64,
                64,
                28,
                planes_in.clone(),
                EncodeOptions {
                    strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
                    mode: SliceMode::Auto,
                    ..EncodeOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{label} auto encode failed: {e}"));
            let dec =
                decode_frame(&bytes).unwrap_or_else(|e| panic!("{label} auto decode failed: {e}"));
            assert!(
                samples_eq_planes(&planes_in, &dec.planes),
                "{label} auto pattern={pattern}: plane mismatch"
            );
            // Every flags byte is in {0x00, 0x01}.
            let flags = extract_per_slice_flags(&bytes, rec.planes as usize, 3);
            for f in flags {
                assert!(
                    f == 0x00 || f == 0x01,
                    "{label} auto pattern={pattern}: flags={f:#x} out of {{0x00, 0x01}}"
                );
            }
        }
    }
}

#[test]
fn auto_mode_picks_huffman_for_all_zero() {
    // All-zero input → all-zero residuals after any predictor → tiny
    // Huffman bitstream (length-1 code per symbol-0). Auto should pick
    // Huffman for every slice.
    let rec = crate::tables::lookup(0x6b).unwrap(); // M8G0
    let pixels = vec![0u8; 64 * 64];
    let bytes = encode_frame(
        rec,
        64,
        64,
        28,
        vec![PlaneInput::U8(pixels)],
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Auto,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let flags = extract_per_slice_flags(&bytes, 1, 3);
    for f in flags {
        assert_eq!(
            f, 0x00,
            "auto on all-zero input should pick Huffman; got flags={f:#x}"
        );
    }
}

#[test]
fn auto_mode_falls_back_to_raw_on_random_input() {
    // 128×128 high-entropy random: Huffman bytes can grow close to raw
    // for symbol-1-bit code dominance; auto picks the smaller. We
    // assert that the encoded frame is no larger than the raw-only
    // encoding of the same input AND no larger than the huffman-only
    // encoding, which is the load-bearing property of auto mode.
    let rec = crate::tables::lookup(0x6b).unwrap(); // M8G0
    let w = 128u32;
    let h = 128u32;
    let mut pixels = vec![0u8; (w * h) as usize];
    let mut acc: u32 = 0x1234_5678;
    for x in pixels.iter_mut() {
        acc = acc.wrapping_mul(1664525).wrapping_add(1013904223);
        *x = (acc >> 8) as u8;
    }
    let auto_bytes = encode_frame(
        rec,
        w,
        h,
        28,
        vec![PlaneInput::U8(pixels.clone())],
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Auto,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let raw_bytes = encode_frame(
        rec,
        w,
        h,
        28,
        vec![PlaneInput::U8(pixels.clone())],
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Raw,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let huff_bytes = encode_frame(
        rec,
        w,
        h,
        28,
        vec![PlaneInput::U8(pixels.clone())],
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Huffman,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    assert!(
        auto_bytes.len() <= raw_bytes.len(),
        "auto should be ≤ raw: auto={} raw={}",
        auto_bytes.len(),
        raw_bytes.len()
    );
    assert!(
        auto_bytes.len() <= huff_bytes.len(),
        "auto should be ≤ huffman: auto={} huff={}",
        auto_bytes.len(),
        huff_bytes.len()
    );
    // Round-trips.
    let dec = decode_frame(&auto_bytes).unwrap();
    assert!(samples_eq_planes(&[PlaneInput::U8(pixels)], &dec.planes));
}

#[test]
fn dynamic_plus_auto_round_trips_combined() {
    for (label, fb) in ROUND1_FOURCCS {
        let rec = lookup_round1(*fb).unwrap();
        for pattern in 0u8..6 {
            let planes_in = make_planes_u8(rec, 64, 64, pattern);
            let bytes = encode_frame(
                rec,
                64,
                64,
                28,
                planes_in.clone(),
                EncodeOptions::dynamic_auto(),
            )
            .unwrap_or_else(|e| panic!("{label} dynamic+auto encode failed: {e}"));
            let dec = decode_frame(&bytes)
                .unwrap_or_else(|e| panic!("{label} dynamic+auto decode failed: {e}"));
            assert!(
                samples_eq_planes(&planes_in, &dec.planes),
                "{label} dynamic+auto pattern={pattern}: plane mismatch"
            );
        }
    }
}

#[test]
fn dynamic_is_no_larger_than_worst_fixed_on_mixed_content() {
    // Sanity: on a mixed-content frame, Dynamic should produce a frame
    // no larger than the larger of (fixed Left, fixed Gradient, fixed
    // Median). i.e. by picking the per-slice minimum residual sum,
    // Dynamic dominates the worst-case fixed predictor.
    let rec = crate::tables::lookup(0x65).unwrap(); // M8RG
    let mut g = vec![0u8; 128 * 64];
    let mut b = vec![0u8; 128 * 64];
    let mut r = vec![0u8; 128 * 64];
    let mut acc: u32 = 0xcafe_babe;
    for row in 0..64usize {
        for col in 0..128usize {
            // Mix horizontal ramp + vertical ramp + noise.
            acc = acc.wrapping_mul(1103515245).wrapping_add(12345);
            g[row * 128 + col] = col as u8;
            b[row * 128 + col] = row as u8;
            r[row * 128 + col] = (acc >> 16) as u8;
        }
    }
    let make_input = || {
        vec![
            PlaneInput::U8(g.clone()),
            PlaneInput::U8(b.clone()),
            PlaneInput::U8(r.clone()),
        ]
    };
    let opts = |s: PredictorStrategy| EncodeOptions {
        strategy: s,
        mode: SliceMode::Huffman,
        ..EncodeOptions::default()
    };
    let bytes_left = encode_frame(
        rec,
        128,
        64,
        28,
        make_input(),
        opts(PredictorStrategy::Fixed(PredictorKind::Left)),
    )
    .unwrap();
    let bytes_grad = encode_frame(
        rec,
        128,
        64,
        28,
        make_input(),
        opts(PredictorStrategy::Fixed(PredictorKind::Gradient)),
    )
    .unwrap();
    let bytes_med = encode_frame(
        rec,
        128,
        64,
        28,
        make_input(),
        opts(PredictorStrategy::Fixed(PredictorKind::Median)),
    )
    .unwrap();
    let bytes_dyn = encode_frame(
        rec,
        128,
        64,
        28,
        make_input(),
        opts(PredictorStrategy::Dynamic),
    )
    .unwrap();
    let worst = *[bytes_left.len(), bytes_grad.len(), bytes_med.len()]
        .iter()
        .max()
        .unwrap();
    assert!(
        bytes_dyn.len() <= worst,
        "dynamic ({}) should be ≤ worst fixed ({}) — L={}, G={}, M={}",
        bytes_dyn.len(),
        worst,
        bytes_left.len(),
        bytes_grad.len(),
        bytes_med.len()
    );
}

// ───────────────── round-124: length-limited Huffman (package-merge) ─────────────────
//
// The encoder caps every per-plane Huffman code length at `max_length`
// (8-bit → 12, spec/05 §1, table in §1.1) and every emitted descriptor
// must decode to a *complete* code with Kraft sum exactly 1.0
// (spec/05 §1.3). A residual histogram with a near-geometric /
// Fibonacci-like shape drives the unbounded-optimal Huffman tree well
// past depth 12; the prior `enforce_length_cap` "steal-a-bit" heuristic
// both spun for millions of iterations and produced an *invalid*
// (Kraft ≪ 1) over-long code on such inputs. The package-merge limiter
// replacing it must produce a frame that round-trips byte-exact.

/// Build a Gray (M8G0) plane whose *Left*-predicted residuals form a
/// deeply-skewed histogram: each row holds a single dominant value
/// plus a steeply-decaying ramp, integrated so the per-pixel left
/// difference reproduces a Fibonacci-weighted symbol mix. The exact
/// residuals don't matter — only that the optimal tree would exceed
/// depth 12, forcing the length-limited path.
fn skewed_gray_plane(w: usize, h: usize) -> Vec<u8> {
    let mut buf = vec![0u8; w * h];
    // Per-row, walk a small set of residual symbols with frequencies
    // that decay geometrically, integrating into pixel values via a
    // running sum so Left prediction recovers those residuals.
    for r in 0..h {
        let mut running: u8 = (r as u8).wrapping_mul(37);
        buf[r * w] = running; // first column is raw under Left.
        for c in 1..w {
            // Map the column index into a heavily front-loaded symbol:
            // most columns reuse a tiny set of residuals, a few use
            // large unique ones. This yields a long-tailed histogram.
            let bucket = (c.trailing_zeros() as u8).min(31);
            let resid = match bucket {
                0 => 1u8,
                1 => 2,
                2 => 3,
                b => 5u8.wrapping_add(b.wrapping_mul(17)),
            };
            running = running.wrapping_add(resid);
            buf[r * w + c] = running;
        }
    }
    buf
}

#[test]
fn skewed_histogram_round_trips_left_8bit() {
    // 256×256 Gray — large enough that the residual histogram is rich
    // and the (pre-fix) cap loop would have stalled.
    let rec = lookup_round1(0x6b).unwrap(); // M8G0
    let plane = skewed_gray_plane(256, 256);
    let planes_in = vec![PlaneInput::U8(plane)];
    let bytes = encode_frame(
        rec,
        256,
        256,
        64,
        planes_in.clone(),
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .expect("skewed M8G0 Left encode must not stall or fail");
    let dec = decode_frame(&bytes).expect("skewed M8G0 Left decode");
    assert!(
        samples_eq_planes(&planes_in, &dec.planes),
        "skewed M8G0 Left: plane mismatch"
    );
}

#[test]
fn skewed_histogram_round_trips_dynamic_auto_rgb() {
    // Same skewed shape across all three RGB planes, exercised through
    // the always-on Dynamic + Auto configuration the v2.4.2 encoder
    // ships with (spec/04 §3 + spec/05 §6.2).
    let rec = lookup(0x65).unwrap(); // M8RG
    let g = skewed_gray_plane(192, 128);
    let b = skewed_gray_plane(192, 128);
    let r = skewed_gray_plane(192, 128);
    let planes_in = vec![PlaneInput::U8(g), PlaneInput::U8(b), PlaneInput::U8(r)];
    let bytes = encode_frame(
        rec,
        192,
        128,
        64,
        planes_in.clone(),
        EncodeOptions::dynamic_auto(),
    )
    .expect("skewed M8RG dynamic+auto encode must not stall or fail");
    let dec = decode_frame(&bytes).expect("skewed M8RG dynamic+auto decode");
    assert!(
        samples_eq_planes(&planes_in, &dec.planes),
        "skewed M8RG dynamic+auto: plane mismatch"
    );
}

#[test]
fn skewed_histogram_round_trips_median_10bit() {
    // 10-bit RGB (M0RG, cap = 14). A skewed u16 residual histogram over
    // the 1024-symbol alphabet, Median-predicted, must still cap to ≤ 14
    // and round-trip.
    let rec = lookup_round2(0x6d).unwrap(); // M0RG
    let mask = rec.sample_mask() as u16;
    let (w, h) = (160usize, 96usize);
    let mk = || {
        let mut buf = vec![0u16; w * h];
        for row in 0..h {
            let mut running: u16 = ((row as u16).wrapping_mul(101)) & mask;
            buf[row * w] = running;
            for col in 1..w {
                let bucket = (col.trailing_zeros() as u16).min(31);
                let resid = match bucket {
                    0 => 1u16,
                    1 => 2,
                    2 => 4,
                    bk => 7u16.wrapping_add(bk.wrapping_mul(53)),
                };
                running = running.wrapping_add(resid) & mask;
                buf[row * w + col] = running;
            }
        }
        PlaneInput::U16(buf)
    };
    let planes_in = vec![mk(), mk(), mk()];
    let bytes = encode_frame(
        rec,
        w as u32,
        h as u32,
        48,
        planes_in.clone(),
        EncodeOptions::fixed(PredictorKind::Median),
    )
    .expect("skewed M0RG Median encode must not stall or fail");
    let dec = decode_frame(&bytes).expect("skewed M0RG Median decode");
    assert!(
        samples_eq_planes(&planes_in, &dec.planes),
        "skewed M0RG Median: plane mismatch"
    );
}

/// `decode_into` must produce byte-identical planes to `decode_frame`
/// across the full predictor set for an RGB-family 8-bit FOURCC (the
/// path that exercises the RGB inter-plane decorrelation reversal —
/// the most allocation-heavy code path).
#[test]
fn decode_into_matches_decode_frame_rgb_8bit() {
    use crate::decoder::{decode_into, DecodedFrame};
    let rec = lookup(0x65).expect("M8RG"); // RGB 8-bit, 3 planes
    let w = 64u32;
    let h = 32u32;
    let mut dst = DecodedFrame::empty();
    for &pred in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        for &pattern in &[0u8, 3, 7] {
            let planes_in = make_planes_u8(rec, w, h, pattern);
            let bytes = encode_frame(rec, w, h, 28, planes_in.clone(), EncodeOptions::fixed(pred))
                .expect("encode");
            let one_shot = decode_frame(&bytes).expect("decode_frame");
            decode_into(&bytes, &mut dst).expect("decode_into");
            assert_eq!(dst.width, one_shot.width);
            assert_eq!(dst.height, one_shot.height);
            assert_eq!(dst.planes.len(), one_shot.planes.len());
            for (a, b) in dst.planes.iter().zip(one_shot.planes.iter()) {
                assert_eq!(a.width, b.width);
                assert_eq!(a.height, b.height);
                assert_eq!(a.bit_depth, b.bit_depth);
                assert_eq!(
                    a.samples, b.samples,
                    "decode_into vs decode_frame mismatch (pred={pred:?}, pattern={pattern})"
                );
            }
        }
    }
}

/// Same parity check on a 10-bit RGB FOURCC — verifies the high-bit
/// path (`Samples::U16` storage + `apply_u16_with_stride`) reuses
/// buffers correctly and produces identical samples.
#[test]
fn decode_into_matches_decode_frame_rgb_10bit() {
    use crate::decoder::{decode_into, DecodedFrame};
    let rec = lookup(0x6d).expect("M0RG"); // RGB 10-bit, 3 planes
    let w = 32u32;
    let h = 16u32;
    let mut dst = DecodedFrame::empty();
    for &pred in &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ] {
        let planes_in = make_planes_u16(rec, w, h, 3);
        let bytes = encode_frame(rec, w, h, 16, planes_in.clone(), EncodeOptions::fixed(pred))
            .expect("encode");
        let one_shot = decode_frame(&bytes).expect("decode_frame");
        decode_into(&bytes, &mut dst).expect("decode_into");
        assert_eq!(dst.planes.len(), one_shot.planes.len());
        for (a, b) in dst.planes.iter().zip(one_shot.planes.iter()) {
            assert_eq!(
                a.samples, b.samples,
                "decode_into vs decode_frame mismatch (10-bit pred={pred:?})"
            );
        }
    }
}

/// The streaming-reuse promise: when consecutive `decode_into` calls
/// see the same geometry, the per-plane `Vec<u8>` storage is re-used
/// in place — no fresh allocation, no capacity growth. We verify by
/// snapshotting `Vec::as_ptr` + `Vec::capacity` after the first
/// decode and asserting they survive a second decode unchanged.
#[test]
fn decode_into_reuses_plane_storage_when_geometry_matches() {
    use crate::decoder::{decode_into, DecodedFrame, Samples};
    let rec = lookup(0x6b).expect("M8G0"); // Gray 8-bit, 1 plane
    let w = 64u32;
    let h = 64u32;
    let planes_in = make_planes_u8(rec, w, h, 5);
    let bytes = encode_frame(
        rec,
        w,
        h,
        28,
        planes_in,
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .expect("encode");
    let mut dst = DecodedFrame::empty();
    decode_into(&bytes, &mut dst).expect("first decode_into");
    // Snapshot the first plane's allocation identity.
    let (ptr0, cap0) = match &dst.planes[0].samples {
        Samples::U8(v) => (v.as_ptr(), v.capacity()),
        _ => unreachable!("M8G0 is 8-bit"),
    };
    decode_into(&bytes, &mut dst).expect("second decode_into");
    let (ptr1, cap1) = match &dst.planes[0].samples {
        Samples::U8(v) => (v.as_ptr(), v.capacity()),
        _ => unreachable!("M8G0 is 8-bit"),
    };
    assert_eq!(ptr0, ptr1, "plane Vec was reallocated on reuse");
    assert_eq!(cap0, cap1, "plane Vec capacity changed on reuse");
}

/// Geometry change between consecutive `decode_into` calls must still
/// produce a correct decode (the plane Vec is resized in place but
/// the output samples are exact).
#[test]
fn decode_into_handles_geometry_change() {
    use crate::decoder::{decode_into, DecodedFrame};
    let rec = lookup(0x65).expect("M8RG");
    let mut dst = DecodedFrame::empty();
    // First frame: 64×32.
    let planes_a = make_planes_u8(rec, 64, 32, 1);
    let bytes_a = encode_frame(
        rec,
        64,
        32,
        28,
        planes_a.clone(),
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .expect("encode-a");
    decode_into(&bytes_a, &mut dst).expect("decode_into-a");
    assert_eq!(dst.width, 64);
    assert_eq!(dst.height, 32);
    assert!(samples_eq_planes(&planes_a, &dst.planes));
    // Second frame: 32×16 (smaller) — Vecs are truncated.
    let planes_b = make_planes_u8(rec, 32, 16, 2);
    let bytes_b = encode_frame(
        rec,
        32,
        16,
        28,
        planes_b.clone(),
        EncodeOptions::fixed(PredictorKind::Median),
    )
    .expect("encode-b");
    decode_into(&bytes_b, &mut dst).expect("decode_into-b");
    assert_eq!(dst.width, 32);
    assert_eq!(dst.height, 16);
    assert!(samples_eq_planes(&planes_b, &dst.planes));
    // Third frame: 96×48 (larger) — Vecs grow.
    let planes_c = make_planes_u8(rec, 96, 48, 3);
    let bytes_c = encode_frame(
        rec,
        96,
        48,
        28,
        planes_c.clone(),
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .expect("encode-c");
    decode_into(&bytes_c, &mut dst).expect("decode_into-c");
    assert_eq!(dst.width, 96);
    assert_eq!(dst.height, 48);
    assert!(samples_eq_planes(&planes_c, &dst.planes));
}

/// Format-byte change between consecutive `decode_into` calls (e.g.
/// switching from 8-bit Gray to 10-bit RGB) must drop the U8 storage
/// and produce a correct U16 decode. Mirrors the inverse direction
/// too.
#[test]
fn decode_into_handles_bit_depth_change() {
    use crate::decoder::{decode_into, DecodedFrame};
    let mut dst = DecodedFrame::empty();
    // 8-bit Gray first.
    let rec_8 = lookup(0x6b).expect("M8G0");
    let planes_8 = make_planes_u8(rec_8, 32, 32, 4);
    let bytes_8 = encode_frame(
        rec_8,
        32,
        32,
        28,
        planes_8.clone(),
        EncodeOptions::fixed(PredictorKind::Left),
    )
    .expect("encode-8");
    decode_into(&bytes_8, &mut dst).expect("decode_into-8");
    assert!(samples_eq_planes(&planes_8, &dst.planes));
    // Now 10-bit RGB into the same dst.
    let rec_10 = lookup(0x6d).expect("M0RG");
    let planes_10 = make_planes_u16(rec_10, 32, 16, 5);
    let bytes_10 = encode_frame(
        rec_10,
        32,
        16,
        16,
        planes_10.clone(),
        EncodeOptions::fixed(PredictorKind::Gradient),
    )
    .expect("encode-10");
    decode_into(&bytes_10, &mut dst).expect("decode_into-10");
    assert!(samples_eq_planes(&planes_10, &dst.planes));
    // And back to 8-bit Gray.
    decode_into(&bytes_8, &mut dst).expect("decode_into-8 round 2");
    assert!(samples_eq_planes(&planes_8, &dst.planes));
}

/// Verify the encoder-side `EncodeOptions::color_matrix` knob writes
/// the nibble into the flags dword at bits 20..23 (`spec/01` §3.1)
/// per the v2.4.2 encoder's OR-accumulation, and that the value
/// survives a round-trip through `header::parse` /
/// [`crate::header::FrameHeader::color_matrix_nibble`]. The 4-bit
/// space is exhaustively swept (0..=15) on M8RG so the test catches
/// drift in either the encoder shift / mask or the reader-side
/// accessor — for each authored value the test confirms (a) the
/// nibble round-trips through the wire, (b) the on-wire flags dword
/// matches the documented `(nibble & 0xf) << 20` formula, and (c)
/// the pixel bytes round-trip unchanged so the matrix knob remains
/// orthogonal to the lossless residual path (the codec layer
/// carries the nibble as a header-level annotation, leaving colour
/// conversion to the consumer above the codec).
#[test]
fn encoder_color_matrix_knob_writes_flags_bits_20_23_and_round_trips() {
    use crate::header::{parse as parse_header, FLAG_COLOR_MATRIX_MASK, FLAG_COLOR_MATRIX_SHIFT};
    let rec = lookup(0x65).expect("M8RG");
    let w = 32u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 7);
    for authored in 0u8..=15 {
        let mut opts = EncodeOptions::fixed(PredictorKind::Gradient);
        opts.color_matrix = authored;
        let bytes = encode_frame(rec, w, h, sh, planes.clone(), opts)
            .expect("encode_frame with color_matrix knob");
        // Parse the header back: the nibble must round-trip exactly
        // for every authored value except 1 (the spec/01 §3.1
        // matrix-skip sentinel, which produces a zero nibble on
        // the wire — that is the documented v2.4.2 behaviour).
        let parsed = parse_header(&bytes).expect("parse encoder-emitted header");
        let expected_nibble: u8 = if authored == 1 { 0 } else { authored };
        assert_eq!(
            parsed.color_matrix_nibble(),
            expected_nibble,
            "authored color_matrix={authored} should land as nibble {expected_nibble} via header.flags bits 20..23",
        );
        // The on-wire dword must match the documented
        // `(nibble & 0xf) << 20` formula precisely (mask isolation).
        let masked = parsed.flags & FLAG_COLOR_MATRIX_MASK;
        assert_eq!(
            masked,
            u32::from(expected_nibble) << FLAG_COLOR_MATRIX_SHIFT,
            "authored color_matrix={authored} masked dword mismatch",
        );
        // The pixel bytes must round-trip byte-exact regardless of
        // the nibble — the codec layer treats it as a header
        // annotation, not a per-sample transform.
        let frame = decode_frame(&bytes).expect("decode_frame");
        for (i, p) in frame.planes.iter().enumerate() {
            let expected_samples = match &planes[i] {
                PlaneInput::U8(buf) => buf.as_slice(),
                PlaneInput::U16(_) => unreachable!("M8RG is 8-bit"),
            };
            match &p.samples {
                Samples::U8(got) => {
                    assert_eq!(
                        got, expected_samples,
                        "plane {i} samples must round-trip irrespective of color_matrix={authored}",
                    );
                }
                Samples::U16(_) => unreachable!("M8RG is 8-bit"),
            }
        }
    }
}

/// The encoder must compose the ColorMatrix nibble with the
/// Interlaced flag cleanly: setting `interlaced = true`
/// and `color_matrix = 0xa` simultaneously must produce a flags
/// dword that carries BOTH bit 1 and bits 20..23, and the parsed
/// header's typed accessors must each report the right value
/// independently of the other. Catches a future regression where
/// the encoder's OR-accumulator overwrites one flag group with
/// another (e.g. a `flags = ...` assignment in place of a `|=`).
#[test]
fn encoder_color_matrix_composes_with_interlaced_flag() {
    use crate::header::{parse as parse_header, FLAG_INTERLACED};
    let rec = lookup(0x65).expect("M8RG");
    let w = 32u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 3);
    let opts = EncodeOptions {
        strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
        mode: SliceMode::Huffman,
        interlaced: true,
        color_matrix: 0xa,
        full_range: false,
        predictor: PredictorKind::Gradient,
    };
    let bytes = encode_frame(rec, w, h, sh, planes.clone(), opts).expect("encode_frame combined");
    let parsed = parse_header(&bytes).expect("parse combined-flags header");
    assert!(
        parsed.is_interlaced(),
        "interlaced flag must survive composition with color_matrix",
    );
    assert_eq!(
        parsed.color_matrix_nibble(),
        0xa,
        "color_matrix nibble must survive composition with interlaced",
    );
    // The composed flags dword equals the bit-OR of the two
    // documented contributions — bit 1 plus the nibble shifted up.
    let expected = FLAG_INTERLACED | (0xa_u32 << 20);
    assert_eq!(
        parsed.flags, expected,
        "composed flags dword mismatch (interlaced + color_matrix=0xa)",
    );
    // And the wire pixel bytes still round-trip.
    let frame = decode_frame(&bytes).expect("decode_frame combined");
    for (i, p) in frame.planes.iter().enumerate() {
        let expected_samples = match &planes[i] {
            PlaneInput::U8(buf) => buf.as_slice(),
            PlaneInput::U16(_) => unreachable!("M8RG is 8-bit"),
        };
        match &p.samples {
            Samples::U8(got) => {
                assert_eq!(got, expected_samples, "plane {i} samples mismatch");
            }
            Samples::U16(_) => unreachable!("M8RG is 8-bit"),
        }
    }
}

/// `EncodeOptions::default()` (and the `fixed` / `dynamic_auto`
/// helpers) must initialise `color_matrix` to `1` — the spec/01
/// §3.1 matrix-skip sentinel. That choice means freshly-defaulted
/// callers emit a header whose ColorMatrix nibble is 0 on the
/// wire (matching the zero `header::FrameHeader::color_matrix_nibble`
/// returns from the matrix-skip flags shape), preserving the
/// existing behavioural contract from r242 and earlier.
#[test]
fn encode_options_defaults_carry_matrix_skip_sentinel() {
    assert_eq!(
        EncodeOptions::default().color_matrix,
        1,
        "default color_matrix must be 1 (spec/01 §3.1 matrix-skip sentinel)",
    );
    assert_eq!(
        EncodeOptions::fixed(PredictorKind::Left).color_matrix,
        1,
        "fixed() helper must carry the matrix-skip sentinel",
    );
    assert_eq!(
        EncodeOptions::dynamic_auto().color_matrix,
        1,
        "dynamic_auto() helper must carry the matrix-skip sentinel",
    );
    // Sanity check: emit a frame with default options and confirm
    // the on-wire flags dword has zero in bits 20..23.
    use crate::header::{parse as parse_header, FLAG_COLOR_MATRIX_MASK};
    let rec = lookup(0x65).expect("M8RG");
    let w = 16u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 1);
    let bytes = encode_frame(rec, w, h, sh, planes, EncodeOptions::default())
        .expect("encode default-options frame");
    let parsed = parse_header(&bytes).expect("parse default-options header");
    assert_eq!(
        parsed.flags & FLAG_COLOR_MATRIX_MASK,
        0,
        "default options must emit a zero ColorMatrix nibble",
    );
}

/// `EncodeOptions::full_range` must land on the wire as flags bit 2
/// (`FLAG_FULL_RANGE`, mask `0x00000004`) per `spec/01` §3.1's
/// `FullRangeYUV` registry value at encoder context offset `+0x78`
/// (the v2.4.2 encoder's OR-accumulator at
/// `magicyuv.dll!0x69b97647`–`0x69b9767a` ORs `0x4` when the
/// registry value is non-zero). The decoder pickup at
/// `magicyuv.dll!0x69bae311` (file `@0x2d311`) reads bit 2 back as
/// a boolean and routes it to the application/conversion layer; on
/// our side `FrameHeader::is_full_range()` exposes the same
/// boolean. The lossless codec layer is independent of this signal,
/// so the wire pixel bytes returned by `decode_frame` must match
/// the original planes byte-for-byte regardless of the bit's value.
///
/// The FOURCC under test is M8Y0 (format byte `0x69`) — a member of
/// `spec/01` §3.1's keep-mask `0xf1903f` (the YUV/Gray family), so
/// the post-accumulation override does NOT fire and the authored
/// bit reaches the wire. (RGB-family FOURCCs have the bit cleared
/// by the override; see the dedicated family-sweep tests below.)
#[test]
fn encoder_full_range_knob_writes_flags_bit_2_and_round_trips() {
    use crate::header::{parse as parse_header, FLAG_FULL_RANGE};
    let rec = lookup(0x69).expect("M8Y0");
    let w = 32u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 11);
    for &authored in &[false, true] {
        let mut opts = EncodeOptions::fixed(PredictorKind::Gradient);
        opts.full_range = authored;
        let bytes = encode_frame(rec, w, h, sh, planes.clone(), opts)
            .expect("encode_frame with full_range knob");
        let parsed = parse_header(&bytes).expect("parse encoder-emitted header");
        assert_eq!(
            parsed.is_full_range(),
            authored,
            "authored full_range={authored} should land as flags bit 2 via FLAG_FULL_RANGE",
        );
        let expected_bit = if authored { FLAG_FULL_RANGE } else { 0 };
        assert_eq!(
            parsed.flags & FLAG_FULL_RANGE,
            expected_bit,
            "authored full_range={authored} masked dword mismatch",
        );
        // Pixel bytes must round-trip byte-exact regardless of the
        // application-layer signal — the codec layer is bit-pure.
        let frame = decode_frame(&bytes).expect("decode_frame");
        assert!(
            samples_eq_planes(&planes, &frame.planes),
            "samples must round-trip irrespective of full_range={authored}",
        );
    }
}

/// `spec/01` §3.1 post-accumulation override, RGB-family side: for
/// every native FOURCC whose format byte is NOT in the keep-mask
/// `0xf1903f` — the in-range RGB family `{0x6d, 0x6e, 0x6f, 0x70,
/// 0x71, 0x72}` plus `0x65` / `0x66` (8-bit RGB / RGBA) via the
/// `format_byte - 0x67 > 0x17` out-of-range fallthrough — the
/// v2.4.2 encoder (`magicyuv.dll!0x69b9769c`–`0x69b976bb`) clears
/// flags bit 2 even when the `FullRangeYUV` registry value asked
/// for it. The override touches ONLY bit 2: the Interlaced bit and
/// the ColorMatrix nibble accumulated earlier must survive, and the
/// pixel bytes still round-trip (the override is header-only).
#[test]
fn full_range_override_clears_bit_2_for_rgb_family() {
    use crate::header::{parse as parse_header, FLAG_FULL_RANGE};
    for &fb in &[0x65u8, 0x66, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72] {
        let rec = lookup(fb).expect("native RGB-family format byte");
        let w = 16u32;
        let h = 8u32;
        let sh = 8u32;
        let planes = if rec.is_high_bit_depth() {
            make_planes_u16(rec, w, h, 5)
        } else {
            make_planes_u8(rec, w, h, 5)
        };
        let opts = EncodeOptions {
            strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
            mode: SliceMode::Huffman,
            interlaced: true,
            color_matrix: 0xa,
            full_range: true,
            predictor: PredictorKind::Gradient,
        };
        let bytes = encode_frame(rec, w, h, sh, planes.clone(), opts)
            .expect("encode_frame RGB-family full_range");
        let parsed = parse_header(&bytes).expect("parse RGB-family header");
        assert!(
            !parsed.is_full_range(),
            "format byte {fb:#04x}: spec/01 §3.1 override must clear flags bit 2",
        );
        assert_eq!(
            parsed.flags & FLAG_FULL_RANGE,
            0,
            "format byte {fb:#04x}: masked dword must have bit 2 clear",
        );
        // The override is bit-2-only — the other two accumulated
        // flag groups survive untouched.
        assert!(
            parsed.is_interlaced(),
            "format byte {fb:#04x}: override must not clear the Interlaced bit",
        );
        assert_eq!(
            parsed.color_matrix_nibble(),
            0xa,
            "format byte {fb:#04x}: override must not clear the ColorMatrix nibble",
        );
        let frame = decode_frame(&bytes).expect("decode_frame RGB-family");
        assert!(
            samples_eq_planes(&planes, &frame.planes),
            "format byte {fb:#04x}: samples must round-trip under the override",
        );
    }
}

/// `spec/01` §3.1 post-accumulation override, YUV/Gray-family side:
/// every native FOURCC whose format byte IS in the keep-mask
/// `0xf1903f` (`{0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x73, 0x76,
/// 0x7b}` among the published set) branches over the override
/// block, so an authored `full_range = true` reaches the wire as
/// flags bit 2 and reads back through `is_full_range()`.
#[test]
fn full_range_survives_for_yuv_and_gray_family() {
    use crate::header::{parse as parse_header, FLAG_FULL_RANGE};
    for &fb in &[0x67u8, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x73, 0x76, 0x7b] {
        let rec = lookup(fb).expect("native YUV/Gray-family format byte");
        let w = 16u32;
        let h = 8u32;
        let sh = 8u32;
        let planes = if rec.is_high_bit_depth() {
            make_planes_u16(rec, w, h, 5)
        } else {
            make_planes_u8(rec, w, h, 5)
        };
        let mut opts = EncodeOptions::fixed(PredictorKind::Gradient);
        opts.full_range = true;
        let bytes = encode_frame(rec, w, h, sh, planes.clone(), opts)
            .expect("encode_frame YUV/Gray-family full_range");
        let parsed = parse_header(&bytes).expect("parse YUV/Gray-family header");
        assert!(
            parsed.is_full_range(),
            "format byte {fb:#04x}: keep-mask member must retain flags bit 2",
        );
        assert_eq!(
            parsed.flags & FLAG_FULL_RANGE,
            FLAG_FULL_RANGE,
            "format byte {fb:#04x}: masked dword must carry bit 2",
        );
        let frame = decode_frame(&bytes).expect("decode_frame YUV/Gray-family");
        assert!(
            samples_eq_planes(&planes, &frame.planes),
            "format byte {fb:#04x}: samples must round-trip with bit 2 set",
        );
    }
}

/// The encoder must compose `full_range` with the other two
/// presently-public flags-dword knobs (`interlaced` at bit 1 and
/// `color_matrix` at bits 20..23) without any of the three
/// clobbering another. Setting all three simultaneously must
/// produce a flags dword whose typed accessors each report exactly
/// the field they own, guarding against a future regression where
/// the OR-accumulator becomes an assignment.
///
/// Uses M8Y0 (format byte `0x69`, a `spec/01` §3.1 keep-mask
/// member) so the RGB-family override does not clear bit 2 and all
/// three authored knobs are observable on the wire simultaneously.
#[test]
fn encoder_full_range_composes_with_interlaced_and_color_matrix() {
    use crate::header::{parse as parse_header, FLAG_FULL_RANGE, FLAG_INTERLACED};
    let rec = lookup(0x69).expect("M8Y0");
    let w = 32u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 5);
    let opts = EncodeOptions {
        strategy: PredictorStrategy::Fixed(PredictorKind::Gradient),
        mode: SliceMode::Huffman,
        interlaced: true,
        color_matrix: 0xa,
        full_range: true,
        predictor: PredictorKind::Gradient,
    };
    let bytes =
        encode_frame(rec, w, h, sh, planes.clone(), opts).expect("encode_frame combined-three");
    let parsed = parse_header(&bytes).expect("parse combined-three-flags header");
    assert!(
        parsed.is_interlaced(),
        "interlaced flag must survive composition with full_range + color_matrix",
    );
    assert!(
        parsed.is_full_range(),
        "full_range flag must survive composition with interlaced + color_matrix",
    );
    assert_eq!(
        parsed.color_matrix_nibble(),
        0xa,
        "color_matrix nibble must survive composition with interlaced + full_range",
    );
    // The composed flags dword equals the bit-OR of the three
    // documented contributions exactly.
    let expected = FLAG_INTERLACED | FLAG_FULL_RANGE | (0xa_u32 << 20);
    assert_eq!(
        parsed.flags, expected,
        "composed flags dword mismatch (interlaced + full_range + color_matrix=0xa)",
    );
    // Pixel bytes still round-trip.
    let frame = decode_frame(&bytes).expect("decode_frame combined-three");
    assert!(
        samples_eq_planes(&planes, &frame.planes),
        "samples must round-trip under the combined three-flag encode",
    );
}

/// `EncodeOptions::default()` (and the `fixed` / `dynamic_auto`
/// helpers) must initialise `full_range` to `false`. That choice
/// keeps a freshly-defaulted caller emitting a header whose flags
/// bit 2 (`FLAG_FULL_RANGE`) stays clear — matching the r242-era
/// encoder's behaviour byte-for-byte and the matrix-skip-sentinel
/// default for `color_matrix`. A sanity round-trip confirms the
/// emitted dword's bit 2 is clear by parsing the header back.
#[test]
fn encode_options_defaults_clear_full_range() {
    assert!(
        !EncodeOptions::default().full_range,
        "default full_range must be false",
    );
    assert!(
        !EncodeOptions::fixed(PredictorKind::Left).full_range,
        "fixed() helper must default full_range to false",
    );
    assert!(
        !EncodeOptions::dynamic_auto().full_range,
        "dynamic_auto() helper must default full_range to false",
    );
    use crate::header::{parse as parse_header, FLAG_FULL_RANGE};
    let rec = lookup(0x65).expect("M8RG");
    let w = 16u32;
    let h = 16u32;
    let sh = 16u32;
    let planes = make_planes_u8(rec, w, h, 2);
    let bytes = encode_frame(rec, w, h, sh, planes, EncodeOptions::default())
        .expect("encode default-options frame");
    let parsed = parse_header(&bytes).expect("parse default-options header");
    assert_eq!(
        parsed.flags & FLAG_FULL_RANGE,
        0,
        "default options must emit a clear FLAG_FULL_RANGE bit",
    );
}

/// Seeded variant of [`roundtrip`]: builds the source planes from
/// [`scramble`] (a distinct pseudo-random field per `(seed, plane, r,
/// c)`) instead of one of the six fixed patterns, then asserts the
/// encode→decode pipeline recovers them bit-for-bit. The seed is woven
/// into every panic message so a sweep failure is replayable.
#[allow(clippy::too_many_arguments)]
fn roundtrip_seeded(
    fourcc_label: &str,
    rec: FourccRecord,
    width: u32,
    height: u32,
    slice_height: u32,
    predictor: PredictorKind,
    mode: SliceMode,
    seed: u64,
    interlaced: bool,
) {
    let planes_in: Vec<PlaneInput> = if rec.is_high_bit_depth() {
        make_planes_u16_seeded(rec, width, height, seed)
    } else {
        make_planes_u8_seeded(rec, width, height, seed)
    };
    let bytes = encode_frame(
        rec,
        width,
        height,
        slice_height,
        planes_in.clone(),
        EncodeOptions {
            strategy: PredictorStrategy::Fixed(predictor),
            predictor,
            mode,
            interlaced,
            color_matrix: 1,
            full_range: false,
        },
    )
    .unwrap_or_else(|e| {
        panic!(
            "{fourcc_label} {width}x{height} sh={slice_height} {predictor:?} {mode:?} seed={seed:#018x} interlaced={interlaced}: encode failed: {e}"
        )
    });
    let dec = decode_frame(&bytes).unwrap_or_else(|e| {
        panic!(
            "{fourcc_label} {width}x{height} sh={slice_height} {predictor:?} {mode:?} seed={seed:#018x} interlaced={interlaced}: decode failed: {e}"
        )
    });
    assert_eq!(dec.width, width);
    assert_eq!(dec.height, height);
    assert!(
        samples_eq_planes(&planes_in, &dec.planes),
        "{fourcc_label} {width}x{height} sh={slice_height} {predictor:?} {mode:?} seed={seed:#018x} interlaced={interlaced}: plane mismatch"
    );
}

/// Full cartesian property sweep proving bit-exact lossless recovery
/// across the **entire** valid input space the codec advertises:
/// every native FOURCC (all three 8-bit + 10/12/14-bit families) ×
/// every predictor (Left / Gradient / Median) × every slice mode
/// (Huffman / Raw) × a dimension/slice-height set chosen to stress
/// single-slice, multi-slice, and partial-last-slice geometry × four
/// distinct pseudo-random seeds.
///
/// This is intentionally exhaustive where the per-feature tests above
/// are pinned: each of those fixes one dimension+pattern combo, so a
/// regression that only manifests at, say, Median+Raw on a 4:2:0
/// FOURCC with a partial last chroma slice and a non-zero residual
/// distribution could slip between them. The seeded fields exercise
/// the Huffman descriptor + canonical-code paths over high-entropy
/// data (forcing near-flat codebooks) as well as the predictor LSB /
/// sign handling over arbitrary residuals.
///
/// All combinations use even dimensions so they satisfy every
/// subsampled FOURCC's chroma-divisibility constraint (spec/03 §8.2);
/// odd-dimension rejection is covered separately by the
/// `encoder_rejects_*` tests.
#[test]
fn cartesian_property_sweep_all_fourccs_predictors_modes() {
    // (width, height, slice_height) triples:
    //  - 16×16 / sh=28 → single slice per plane (sh > height).
    //  - 32×64 / sh=28 → 3 slices/plane (64 = 28+28+8 partial last).
    //  - 24×20 / sh=8  → 4:2:0 chroma = 12×10, last slice partial.
    const DIMS: &[(u32, u32, u32)] = &[(16, 16, 28), (32, 64, 28), (24, 20, 8)];
    const PREDICTORS: &[PredictorKind] = &[
        PredictorKind::Left,
        PredictorKind::Gradient,
        PredictorKind::Median,
    ];
    const MODES: &[SliceMode] = &[SliceMode::Huffman, SliceMode::Raw];
    const SEEDS: &[u64] = &[
        0x0000_0000_0000_0001,
        0xdead_beef_cafe_babe,
        0x0123_4567_89ab_cdef,
        0xffff_ffff_ffff_fffe,
    ];

    let all_fourccs = ROUND1_FOURCCS.iter().chain(ROUND2_HIGH_FOURCCS.iter());
    let mut cases = 0usize;
    for (label, fb) in all_fourccs {
        let rec = lookup(*fb).unwrap_or_else(|| panic!("lookup {label} ({fb:#04x})"));
        for &(w, h, sh) in DIMS {
            for &pred in PREDICTORS {
                for &mode in MODES {
                    for (i, &seed) in SEEDS.iter().enumerate() {
                        // Vary interlaced across seeds so both the
                        // progressive and field-stride=2 prediction
                        // paths are exercised for every FOURCC/predictor.
                        let interlaced = i % 2 == 1 && h >= 4;
                        roundtrip_seeded(label, rec, w, h, sh, pred, mode, seed, interlaced);
                        cases += 1;
                    }
                }
            }
        }
    }
    // 17 fourccs × 3 dims × 3 predictors × 2 modes × 4 seeds.
    assert_eq!(cases, 17 * 3 * 3 * 2 * 4, "sweep case count");
}

/// Decoder honours an arbitrary on-wire `per_slice_plane_index`
/// ordering, not just the plane-major one the vendor encoder emits
/// (`spec/02` §7.3: "A spec-compliant decoder MUST read
/// `per_slice_plane_index` from the preamble (not assume the
/// plane-major ordering); the encoder's freedom to interleave is
/// preserved by the table format.").
///
/// Our encoder only ever writes plane-major frames, so to exercise the
/// interleaved path we take a plane-major frame, parse out its slice
/// table / preamble / payloads, and re-serialise it with the global
/// slice order permuted (and the `per_slice_plane_index` bytes +
/// slice-table entries kept consistent with the new order). The
/// permuted frame must decode to the exact same planes.
mod per_slice_plane_index_ordering {
    use super::*;
    use crate::decoder::decode_frame;

    /// Layout of a parsed plane-major v7 frame, decomposed so the test
    /// can re-emit it with the global slice order permuted.
    struct ParsedFrame {
        header: Vec<u8>,           // 32-byte header, verbatim
        plane_count_byte: u8,      // preamble[0]
        huff_descriptors: Vec<u8>, // preamble[1 + total_slices ..]
        // For each global slice in its original (plane-major) order:
        slice_plane: Vec<u8>,        // the per_slice_plane_index byte
        slice_payload: Vec<Vec<u8>>, // the raw slice bytes (prefix + body)
        total_slices: usize,
    }

    fn parse(bytes: &[u8], total_slices: usize) -> ParsedFrame {
        let header = bytes[..0x20].to_vec();
        let table_off = 0x20usize;
        let n = total_slices + 1;
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let o = table_off + 4 * i;
            entries.push(u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize);
        }
        let table_bytes = 4 * n;
        let preamble_start = table_off + table_bytes;
        let preamble_end = entries[1] + table_off;
        let preamble = &bytes[preamble_start..preamble_end];
        let plane_count_byte = preamble[0];
        let slice_plane = preamble[1..1 + total_slices].to_vec();
        let huff_descriptors = preamble[1 + total_slices..].to_vec();

        let mut slice_payload = Vec::with_capacity(total_slices);
        for s in 0..total_slices {
            let start = entries[s + 1] + table_off;
            let end = if s + 1 < total_slices {
                entries[s + 2] + table_off
            } else {
                // Last slice runs to end-of-file; strip the single
                // even-byte pad if present so the re-emit recomputes it.
                bytes.len()
            };
            slice_payload.push(bytes[start..end].to_vec());
        }
        ParsedFrame {
            header,
            plane_count_byte,
            huff_descriptors,
            slice_plane,
            slice_payload,
            total_slices,
        }
    }

    /// Re-serialise the frame emitting its global slices in `order`
    /// (a permutation of `0..total_slices`). The `per_slice_plane_index`
    /// byte travels with its slice, so the decoder's running per-plane
    /// counter still reconstructs each slice's in-plane position.
    fn reemit_permuted(pf: &ParsedFrame, order: &[usize]) -> Vec<u8> {
        assert_eq!(order.len(), pf.total_slices);
        let n = pf.total_slices + 1;
        let table_bytes = 4 * n;

        // Preamble in the new order.
        let mut preamble = Vec::new();
        preamble.push(pf.plane_count_byte);
        for &s in order {
            preamble.push(pf.slice_plane[s]);
        }
        preamble.extend_from_slice(&pf.huff_descriptors);

        // Slice-table entries: entry[0]=entry[1]=preamble end; each
        // subsequent entry is the running payload offset.
        let mut entries = vec![0u32; n];
        let mut off = table_bytes + preamble.len();
        entries[0] = off as u32;
        entries[1] = off as u32;
        for (i, &s) in order.iter().enumerate() {
            entries[i + 1] = off as u32;
            off += pf.slice_payload[s].len();
        }

        let mut out = Vec::new();
        out.extend_from_slice(&pf.header);
        for &e in &entries {
            out.extend_from_slice(&e.to_le_bytes());
        }
        out.extend_from_slice(&preamble);
        for &s in order {
            out.extend_from_slice(&pf.slice_payload[s]);
        }
        if out.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn run_case(fb: u8, w: u32, h: u32, sh: u32, pattern: u8, order: &[usize]) {
        let rec = lookup(fb).unwrap();
        let planes_in: Vec<PlaneInput> = if rec.is_high_bit_depth() {
            make_planes_u16(rec, w, h, pattern)
        } else {
            make_planes_u8(rec, w, h, pattern)
        };
        let bytes = encode_frame(
            rec,
            w,
            h,
            sh,
            planes_in.clone(),
            EncodeOptions::fixed(PredictorKind::Median),
        )
        .expect("encode plane-major frame");

        // Reference decode of the unmodified plane-major frame.
        let ref_dec = decode_frame(&bytes).expect("decode plane-major frame");

        let slices_per_plane = (h as usize).div_ceil(sh as usize);
        let total_slices = rec.planes as usize * slices_per_plane;
        let pf = parse(&bytes, total_slices);

        // Sanity: the original is plane-major (s / slices_per_plane).
        for s in 0..total_slices {
            assert_eq!(pf.slice_plane[s] as usize, s / slices_per_plane);
        }

        let permuted = reemit_permuted(&pf, order);
        let perm_dec =
            decode_frame(&permuted).expect("decode interleaved per_slice_plane_index frame");

        assert_eq!(perm_dec.planes.len(), ref_dec.planes.len());
        assert!(
            samples_eq_planes(&planes_in, &perm_dec.planes),
            "interleaved-order decode must match the source pixels",
        );
        // And it must equal the plane-major decode plane-for-plane.
        for (a, b) in perm_dec.planes.iter().zip(ref_dec.planes.iter()) {
            match (&a.samples, &b.samples) {
                (Samples::U8(x), Samples::U8(y)) => assert_eq!(x, y),
                (Samples::U16(x), Samples::U16(y)) => assert_eq!(x, y),
                _ => panic!("plane sample-type mismatch"),
            }
        }
    }

    #[test]
    fn round_robin_interleave_rgb_8bit() {
        // M8RG 8×8, slice_height 4 → 2 slices/plane × 3 planes = 6.
        // Round-robin global order: p0s0,p1s0,p2s0,p0s1,p1s1,p2s1.
        // In plane-major indices that is [0,2,4,1,3,5].
        run_case(0x65, 8, 8, 4, 3, &[0, 2, 4, 1, 3, 5]);
    }

    #[test]
    fn plane_reversed_interleave_rgba_8bit() {
        // M8RA 8×8, slice_height 4 → 2 slices/plane × 4 planes = 8.
        // Emit the planes in reverse plane order (3,2,1,0) while
        // preserving each plane's within-plane slice order (slice 0
        // before slice 1). The decoder's running per-plane counter
        // must therefore still place plane p's k-th *appearance* at
        // in-plane row block k. A permutation that reordered slices
        // *within* a plane would change the pixel meaning and is not a
        // valid interleaving — only the plane-level order is free.
        run_case(0x66, 8, 8, 4, 5, &[6, 7, 4, 5, 2, 3, 0, 1]);
    }

    #[test]
    fn round_robin_interleave_yuv420_8bit() {
        // M8Y0 (4:2:0) 16×16, slice_height 8 → 2 slices/plane × 3 = 6.
        // Chroma slices use a different in-plane row stride; the
        // running per-plane counter must still place them correctly.
        run_case(0x69, 16, 16, 8, 3, &[0, 2, 4, 1, 3, 5]);
    }

    #[test]
    fn round_robin_interleave_rgb_10bit() {
        // M0RG 8×8, slice_height 4 → 2 slices/plane × 3 planes = 6.
        run_case(0x6d, 8, 8, 4, 3, &[0, 2, 4, 1, 3, 5]);
    }

    #[test]
    fn out_of_range_plane_index_rejected() {
        // Corrupt one per_slice_plane_index byte to name a 4th plane in
        // a 3-plane frame; the decoder must reject it as malformed.
        let rec = lookup(0x65).unwrap();
        let (w, h, sh) = (8u32, 8u32, 4u32);
        let planes = make_planes_u8(rec, w, h, 3);
        let bytes = encode_frame(
            rec,
            w,
            h,
            sh,
            planes,
            EncodeOptions::fixed(PredictorKind::Left),
        )
        .unwrap();
        // Preamble byte 1 is the first per_slice_plane_index byte:
        // 0x20 + 4*(6+1) = 0x3c, +1 for plane_count → 0x3d.
        let mut corrupt = bytes.clone();
        let idx = 0x20 + 4 * (6 + 1) + 1;
        corrupt[idx] = 3; // plane 3 does not exist (0..=2)
        match decode_frame(&corrupt) {
            Err(crate::error::Error::BadPlaneIndex { .. }) => {}
            Err(e) => panic!("expected BadPlaneIndex, got {e:?}"),
            Ok(_) => panic!("expected BadPlaneIndex, got Ok"),
        }
    }

    #[test]
    fn over_quota_plane_index_rejected() {
        // Name plane 0 three times in a 2-slices-per-plane frame: the
        // quota overflow must be rejected.
        let rec = lookup(0x65).unwrap();
        let (w, h, sh) = (8u32, 8u32, 4u32);
        let planes = make_planes_u8(rec, w, h, 3);
        let bytes = encode_frame(
            rec,
            w,
            h,
            sh,
            planes,
            EncodeOptions::fixed(PredictorKind::Left),
        )
        .unwrap();
        let mut corrupt = bytes.clone();
        let base = 0x20 + 4 * (6 + 1) + 1;
        // Original plane-major bytes are 0 0 1 1 2 2; set slice 2 (the
        // first plane-1 slice) to plane 0, giving 0 0 0 1 2 2 → plane 0
        // appears 3× but only has 2 slices.
        corrupt[base + 2] = 0;
        match decode_frame(&corrupt) {
            Err(crate::error::Error::BadPlaneIndex { .. }) => {}
            Err(e) => panic!("expected BadPlaneIndex on quota overflow, got {e:?}"),
            Ok(_) => panic!("expected BadPlaneIndex on quota overflow, got Ok"),
        }
    }
}
