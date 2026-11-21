//! Pins for the interoperability fixes found by the live black-box
//! matrix (`tests/blackbox_matrix.rs`):
//!
//! - multi-slice frames whose preamble index bytes hold row-major ranks
//!   and whose slice table is plane-major but not monotonic (the shape a
//!   black-box encoder writes for every multi-slice frame);
//! - odd dimensions on subsampled FOURCCs (chroma rounds up);
//! - the encoder never writes a zero-length Huffman symbol.
//!
//! The `tests/data/bb_*.magy` files are single frames produced by the
//! `ffmpeg` executable's MagicYUV encoder from the neighbouring `.raw`
//! planes (fixture provenance only).

use oxideav_magicyuv::tables::{FourccRecord, PredictorKind, fourcc_table};
use oxideav_magicyuv::{
    EncodeOptions, Error, PlaneInput, PredictorStrategy, Samples, SliceMode, decode_frame,
    encode_frame,
};

fn rec(fourcc: &[u8; 4]) -> FourccRecord {
    fourcc_table()
        .iter()
        .copied()
        .find(|r| &r.fourcc == fourcc)
        .unwrap()
}

fn decoded_bytes(frame: &[u8]) -> Vec<u8> {
    let f = decode_frame(frame).expect("decode");
    let mut out = Vec::new();
    for p in &f.planes {
        match &p.samples {
            Samples::U8(v) => out.extend_from_slice(v),
            Samples::U16(v) => out.extend(v.iter().flat_map(|s| s.to_le_bytes())),
        }
    }
    out
}

#[test]
fn foreign_multi_slice_rgb_frame_decodes() {
    let frame = include_bytes!("data/bb_gbrp_4x4_s2_median.magy");
    let want = include_bytes!("data/bb_gbrp_4x4_s2_median.raw");
    // 3 planes × 2 slices: index bytes are the row-major ranks.
    assert_eq!(&frame[32 + 4 * 7..32 + 4 * 7 + 7], &[3, 0, 3, 1, 4, 2, 5]);
    assert_eq!(decoded_bytes(frame), want);
}

#[test]
fn foreign_odd_sized_420_frame_decodes() {
    let frame = include_bytes!("data/bb_yuv420p_3x5_s2_gradient.magy");
    let want = include_bytes!("data/bb_yuv420p_3x5_s2_gradient.raw");
    let f = decode_frame(frame).unwrap();
    assert_eq!((f.planes[0].width, f.planes[0].height), (3, 5));
    assert_eq!((f.planes[1].width, f.planes[1].height), (2, 3));
    assert_eq!((f.planes[2].width, f.planes[2].height), (2, 3));
    assert_eq!(decoded_bytes(frame), want);
}

#[test]
fn foreign_one_row_slices_gray_frame_decodes() {
    let frame = include_bytes!("data/bb_gray_5x7_s7_left.magy");
    let want = include_bytes!("data/bb_gray_5x7_s7_left.raw");
    assert_eq!(decoded_bytes(frame), want);
}

/// A parsed frame: header, slice starts (relative to the header end),
/// preamble and payloads in table order.
struct Parts {
    header: Vec<u8>,
    index: Vec<u8>,
    descriptors: Vec<u8>,
    payloads: Vec<Vec<u8>>,
}

fn split(frame: &[u8], total_slices: usize) -> Parts {
    let entry = |i: usize| {
        u32::from_le_bytes(frame[32 + 4 * i..36 + 4 * i].try_into().unwrap()) as usize + 32
    };
    let pre = 32 + 4 * (total_slices + 1);
    let mut payloads = Vec::new();
    for k in 0..total_slices {
        let end = if k + 1 < total_slices {
            entry(k + 2)
        } else {
            frame.len()
        };
        payloads.push(frame[entry(k + 1)..end].to_vec());
    }
    Parts {
        header: frame[..32].to_vec(),
        index: frame[pre + 1..pre + 1 + total_slices].to_vec(),
        descriptors: frame[pre + 1 + total_slices..entry(1)].to_vec(),
        payloads,
    }
}

/// Re-emit `parts` with the payloads stored in `physical` order (a
/// permutation of table positions), the table still indexed by table
/// position, and `index` as the preamble bytes.
fn emit(parts: &Parts, planes: u8, index: &[u8], physical: &[usize]) -> Vec<u8> {
    let n = parts.payloads.len();
    let base = 4 * (n + 1) + 1 + n + parts.descriptors.len();
    let mut start = vec![0usize; n];
    let mut pos = base;
    for &k in physical {
        start[k] = pos;
        pos += parts.payloads[k].len();
    }
    let mut out = parts.header.clone();
    out.extend((start[0] as u32).to_le_bytes());
    for s in &start {
        out.extend((*s as u32).to_le_bytes());
    }
    out.push(planes);
    out.extend_from_slice(index);
    out.extend_from_slice(&parts.descriptors);
    for &k in physical {
        out.extend_from_slice(&parts.payloads[k]);
    }
    out
}

fn sample_planes(r: FourccRecord, w: usize, h: usize) -> Vec<PlaneInput> {
    (0..r.planes as usize)
        .map(|p| {
            let chroma = (p == 1 || p == 2) && (r.sub_x > 1 || r.sub_y > 1);
            let (pw, ph) = if chroma {
                (w.div_ceil(r.sub_x as usize), h.div_ceil(r.sub_y as usize))
            } else {
                (w, h)
            };
            let v = (0..pw * ph).map(|i| (i * 7 + p * 31 + (i * i) % 13) as u32);
            if r.is_high_bit_depth() {
                PlaneInput::U16(v.map(|x| (x & r.sample_mask()) as u16).collect())
            } else {
                PlaneInput::U8(v.map(|x| x as u8).collect())
            }
        })
        .collect()
}

#[test]
fn row_major_rank_layout_decodes_like_the_plane_index_layout() {
    for fourcc in [b"M8RG", b"M8RA", b"M8Y0", b"M8G0", b"M0RG", b"M0Y0"] {
        let r = rec(fourcc);
        let (w, h, slice_height) = (12usize, 20usize, 6u32);
        let planes = r.planes as usize;
        let per_plane = h.div_ceil(slice_height as usize);
        let total = planes * per_plane;
        let ours = encode_frame(
            r,
            w as u32,
            h as u32,
            slice_height,
            sample_planes(r, w, h),
            EncodeOptions::default()
                .with_strategy(PredictorStrategy::Dynamic)
                .with_mode(SliceMode::Auto),
        )
        .unwrap();
        let want = decoded_bytes(&ours);
        let parts = split(&ours, total);
        assert!(parts.index.iter().all(|&b| (b as usize) < planes));

        // Table position p·S + s keeps slice s of plane p; the bytes
        // become s·P + p and the payloads are stored slice-row by
        // slice-row.
        let rank: Vec<u8> = (0..total)
            .map(|k| ((k % per_plane) * planes + k / per_plane) as u8)
            .collect();
        let mut physical: Vec<usize> = (0..total).collect();
        physical.sort_by_key(|&k| rank[k]);
        let scattered = emit(&parts, r.planes, &rank, &physical);
        assert_eq!(decoded_bytes(&scattered), want, "{:?}", r.fourcc);

        // The same bytes over a monotonic table decode too (the slice
        // extents are then the plain `spec/02` §5.1 ones).
        let in_order: Vec<usize> = (0..total).collect();
        let ranked_in_order = emit(&parts, r.planes, &rank, &in_order);
        assert_eq!(decoded_bytes(&ranked_in_order), want);

        // Anything that is neither plane indices nor exactly the
        // row-major ranks is refused.
        let mut other = rank.clone();
        other.swap(0, total - 1);
        assert!(matches!(
            decode_frame(&emit(&parts, r.planes, &other, &physical)),
            Err(Error::BadPlaneIndex { .. })
        ));

        // Two slices may not share a start in the scattered layout.
        let mut dup = scattered.clone();
        let (a, b) = (32 + 4 * 2, 32 + 4 * 3);
        let first = dup[a..a + 4].to_vec();
        dup[b..b + 4].copy_from_slice(&first);
        assert!(decode_frame(&dup).is_err());
    }
}

#[test]
fn encoder_never_emits_a_zero_length_symbol() {
    // Every descriptor byte is either a literal length 1..=max or a
    // run `0x80 | length` with length 1..=max (`spec/05` §1.1); a
    // length of 0 would be `0x00` or `0x80`.
    for r in fourcc_table() {
        for (w, h) in [(1usize, 1usize), (8, 8), (33, 17)] {
            for strategy in [
                PredictorStrategy::Fixed(PredictorKind::Left),
                PredictorStrategy::Dynamic,
            ] {
                let frame = encode_frame(
                    *r,
                    w as u32,
                    h as u32,
                    28,
                    sample_planes(*r, w, h),
                    EncodeOptions::default().with_strategy(strategy),
                )
                .unwrap();
                let planes = r.planes as usize;
                let pre = 32 + 4 * (planes + 1);
                let end = u32::from_le_bytes(frame[36..40].try_into().unwrap()) as usize + 32;
                let mut pos = pre + 1 + planes;
                let mut symbols = 0usize;
                while pos < end {
                    let b = frame[pos];
                    assert_ne!(b & 0x7f, 0, "{:?}: zero-length symbol", r.fourcc);
                    assert!(b & 0x7f <= r.max_huffman_length());
                    if b & 0x80 != 0 {
                        symbols += 1 + frame[pos + 1] as usize;
                        pos += 2;
                    } else {
                        symbols += 1;
                        pos += 1;
                    }
                }
                assert_eq!(symbols, planes << r.bit_depth);
            }
        }
    }
}

/// Planes with enough samples that a thread grant is honoured.
fn large_planes(r: FourccRecord, w: usize, h: usize) -> Vec<PlaneInput> {
    let mut state = 0x9e37_79b9u32;
    (0..r.planes as usize)
        .map(|p| {
            let chroma = (p == 1 || p == 2) && (r.sub_x > 1 || r.sub_y > 1);
            let (pw, ph) = if chroma {
                (w.div_ceil(r.sub_x as usize), h.div_ceil(r.sub_y as usize))
            } else {
                (w, h)
            };
            let v = (0..pw * ph).map(|i| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                // smooth in the top half, noise below: raw and Huffman
                // slices in one frame under `SliceMode::Auto`
                if i < pw * ph / 2 {
                    (i % pw + i / pw + p * 9) as u32 + (state & 3)
                } else {
                    state >> 8
                }
            });
            if r.is_high_bit_depth() {
                PlaneInput::U16(v.map(|x| (x & r.sample_mask()) as u16).collect())
            } else {
                PlaneInput::U8(v.map(|x| x as u8).collect())
            }
        })
        .collect()
}

#[test]
fn thread_grant_changes_neither_the_bytes_nor_the_samples() {
    use oxideav_magicyuv::{DecodeOptions, decode_frame_with};
    let (w, h) = (331usize, 257usize);
    for fourcc in [
        b"M8RG", b"M8RA", b"M8Y0", b"M8Y2", b"M8G0", b"M0RG", b"M0Y0", b"M4RA",
    ] {
        let r = rec(fourcc);
        for (strategy, mode, slice_height, interlaced) in [
            (PredictorStrategy::Dynamic, SliceMode::Auto, 28u32, false),
            (PredictorStrategy::Dynamic, SliceMode::Auto, 6, true),
            (
                PredictorStrategy::Fixed(PredictorKind::Median),
                SliceMode::Huffman,
                258,
                false,
            ),
            (
                PredictorStrategy::Fixed(PredictorKind::Gradient),
                SliceMode::Raw,
                2,
                false,
            ),
        ] {
            let opts = EncodeOptions::default()
                .with_strategy(strategy)
                .with_mode(mode)
                .with_interlaced(interlaced);
            let encode = |threads: usize| {
                encode_frame(
                    r,
                    w as u32,
                    h as u32,
                    slice_height,
                    large_planes(r, w, h),
                    opts.with_threads(threads),
                )
                .unwrap()
            };
            let serial = encode(1);
            for threads in [0usize, 2, 3, 8, 64] {
                assert!(
                    encode(threads) == serial,
                    "{:?} sh={slice_height}: encode differs at threads={threads}",
                    r.fourcc
                );
            }
            let want = decoded_bytes(&serial);
            for threads in [0usize, 2, 3, 8, 64] {
                let f =
                    decode_frame_with(&serial, DecodeOptions::new().with_threads(threads)).unwrap();
                let mut got = Vec::new();
                for p in &f.planes {
                    match &p.samples {
                        Samples::U8(v) => got.extend_from_slice(v),
                        Samples::U16(v) => got.extend(v.iter().flat_map(|s| s.to_le_bytes())),
                    }
                }
                assert!(
                    got == want,
                    "{:?} sh={slice_height}: decode differs at threads={threads}",
                    r.fourcc
                );
            }
        }
    }
}

#[test]
fn threaded_decode_reports_the_first_bad_slice() {
    use oxideav_magicyuv::{DecodeOptions, decode_frame_with};
    let r = rec(b"M8RG");
    let (w, h, slice_height) = (331usize, 257usize, 16u32);
    let per_plane = h.div_ceil(slice_height as usize);
    let total = 3 * per_plane;
    let mut frame = encode_frame(
        r,
        w as u32,
        h as u32,
        slice_height,
        large_planes(r, w, h),
        EncodeOptions::default(),
    )
    .unwrap();
    // Give slices 7 and 30 an unknown predictor id.
    for k in [30usize, 7] {
        let start =
            u32::from_le_bytes(frame[36 + 4 * k..40 + 4 * k].try_into().unwrap()) as usize + 32;
        frame[start + 1] = 0x09;
    }
    assert!(total > 30);
    for threads in [1usize, 2, 8] {
        let got = decode_frame_with(&frame, DecodeOptions::new().with_threads(threads));
        assert!(
            matches!(got, Err(Error::BadPredictorId(0x09))),
            "threads={threads}"
        );
    }
}
