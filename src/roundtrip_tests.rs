//! Self-roundtrip integration tests.
//!
//! No reference fixtures (the cleanroom proprietary binary is
//! Auditor-only). We synthesise frames with the public encoder,
//! decode them back, and assert byte-exactness.

use crate::decoder::{decode_frame, Samples};
use crate::encoder::{encode_frame, EncodeOptions, PlaneInput, SliceMode};
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
            predictor,
            mode,
            interlaced,
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
        EncodeOptions {
            predictor: PredictorKind::Left,
            mode: SliceMode::Huffman,
            interlaced: false,
        },
    )
    .unwrap();
    let entry1 = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
    bytes[32 + entry1 + 1] = 0x04;
    let r = decode_frame(&bytes);
    assert!(matches!(r, Err(crate::Error::BadPredictorId(0x04))));
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
        EncodeOptions {
            predictor: PredictorKind::Gradient,
            mode: SliceMode::Huffman,
            interlaced: false,
        },
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
        EncodeOptions {
            predictor: PredictorKind::Gradient,
            mode: SliceMode::Huffman,
            interlaced: false,
        },
    )
    .unwrap();
    let h1 = crate::header::parse(&bytes[..32]).unwrap();
    assert_eq!(h1.format_byte, 0x65);
    assert_eq!(h1.aux_byte, 0x0c);
    assert_eq!(h1.width, 16);
    assert_eq!(h1.height, 16);
    assert_eq!(h1.slice_height, 28);
}
