//! Criterion benchmark covering the **encoder's predictor-strategy ×
//! per-slice-mode × interlaced** axis at a single representative
//! FOURCC. Together with the per-FOURCC `encode` and
//! `decode_all_fourccs` benches, this finally lights up every
//! orthogonal encoder axis the public `EncodeOptions` exposes:
//!
//! | Bench                       | FOURCC axis | Strategy axis        | Mode axis            | Interlaced |
//! | --------------------------- | ----------- | -------------------- | -------------------- | ---------- |
//! | `encode`                    | 4 picks     | `Fixed{Lt,Gd,Md}`    | `Huffman` only       | off only   |
//! | `decode_all_fourccs`        | all 17      | `Fixed(Gradient)`    | `Huffman` only       | off only   |
//! | **`encode_strategy_matrix`**| 1 pick      | **all 4 strategies** | **all 3 modes**      | **on+off** |
//!
//! Why this matters: `PredictorStrategy::Dynamic` (spec/04 §3) runs
//! Left + Gradient + Median against every slice and keeps the smallest
//! residual sum, so it has roughly **3× the prediction-side cost** of
//! any `Fixed(_)` strategy. `SliceMode::Auto` (spec/05 §6.2) then
//! sizes both the Huffman pack and the bit-packed raw payload per
//! slice and picks the smaller, doubling the bit-pack work on slices
//! where Huffman wins. Neither path was being timed before this bench,
//! so a Dynamic + Auto regression (the `EncodeOptions::dynamic_auto()`
//! shipping configuration) would have been invisible. The `interlaced`
//! axis (`spec/04` §5.1 field-stride=2 prediction) shares its predictor
//! kernels with the progressive path but uses a different neighbour
//! geometry — so a regression there shows up as an "interlaced-on row
//! is suddenly 30 % slower than the matching interlaced-off row"
//! anomaly in the matrix readout.
//!
//! FOURCC pick: **M8Y0** (0x69, 8-bit YUV 4:2:0). Subsampled chroma
//! exercises the cross-plane-size dispatch (a single-FOURCC matrix
//! covering only RGB would miss it), and the modular-Median 8-bit path
//! is faster than the 10/12/14-bit JPEG-LS Median form, so the
//! Median + Dynamic + Auto cells finish in a reasonable wall time at
//! the bench resolution. The complementary `encode_all_fourccs`-style
//! breadth sweep at one fixed strategy is already covered by the
//! companion `decode_all_fourccs` bench's encode-side feed loop.
//!
//! Resolution: 640×480 — same as `decode_all_fourccs` so per-FOURCC
//! timings remain comparable across benches. Small enough that the
//! 24-cell matrix (4 strategies × 3 modes × 2 interlaced) finishes
//! inside the default Criterion measurement window.
//!
//! Slice height: **28**, mirroring the v2.4.2 encoder's default
//! (`spec/02` §3) and matching every other bench in the crate. At
//! sub_y = 2 (4:2:0 chroma) the chroma slice height is `28 / 2 = 14`,
//! which is valid per the `encode_magicyuv` fuzz target's documented
//! `slice_height % sub_y == 0` precondition.
//!
//! Throughput is reported in raw uncompressed plane bytes
//! (`w * h * 3 / 2` for 4:2:0) so the per-strategy MiB/s figures are
//! directly comparable as "encoded pixel volume per second".
//!
//! Run with:
//!     cargo bench -p oxideav-magicyuv --bench encode_strategy_matrix

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_magicyuv::tables::{self, Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{encode_frame, EncodeOptions, PlaneInput, PredictorStrategy, SliceMode};

const BENCH_WIDTH: u32 = 640;
const BENCH_HEIGHT: u32 = 480;
const SLICE_HEIGHT: u32 = 28;
/// 0x69 = M8Y0 (8-bit YUV 4:2:0). See module docstring for the pick.
const BENCH_FORMAT_BYTE: u8 = 0x69;

/// Cheap deterministic xorshift32 — mirrors every other bench in the
/// crate so per-pixel residual statistics line up across all harnesses.
fn xorshift_byte(state: &mut u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    (*state & 0xff) as u8
}

fn build_plane_u8(width: usize, height: usize, plane: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let mut state: u32 = 0xdead_beef ^ (plane as u32).wrapping_mul(0x9e37_79b9);
    for r in 0..height {
        for c in 0..width {
            let base = ((r as u32).wrapping_add(c as u32) >> 1) & 0xff;
            let noise = xorshift_byte(&mut state) as u32 & 0x07;
            out[r * width + c] = (base.wrapping_add(noise) & 0xff) as u8;
        }
    }
    out
}

fn make_planes(rec: FourccRecord, width: u32, height: u32) -> Vec<PlaneInput> {
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
            PlaneInput::U8(build_plane_u8(pw, ph, p))
        })
        .collect()
}

/// Raw uncompressed plane bytes for the M8Y0 4:2:0 input.
fn raw_plane_bytes(rec: FourccRecord, width: u32, height: u32) -> u64 {
    let w = width as u64;
    let h = height as u64;
    let mut total: u64 = 0;
    for p in 0..rec.planes as usize {
        let (sub_x, sub_y) = match rec.family {
            Family::Yuv if p == 1 || p == 2 => (rec.sub_x as u64, rec.sub_y as u64),
            Family::Yuva if p == 1 || p == 2 => (rec.sub_x as u64, rec.sub_y as u64),
            _ => (1u64, 1u64),
        };
        total += (w / sub_x) * (h / sub_y);
    }
    total
}

/// Short human-readable tag for the BenchmarkId axis.
fn strategy_tag(s: PredictorStrategy) -> &'static str {
    match s {
        PredictorStrategy::Fixed(PredictorKind::Left) => "fixed-left",
        PredictorStrategy::Fixed(PredictorKind::Gradient) => "fixed-gradient",
        PredictorStrategy::Fixed(PredictorKind::Median) => "fixed-median",
        PredictorStrategy::Dynamic => "dynamic",
    }
}

fn mode_tag(m: SliceMode) -> &'static str {
    match m {
        SliceMode::Huffman => "huffman",
        SliceMode::Raw => "raw",
        SliceMode::Auto => "auto",
    }
}

fn interlaced_tag(i: bool) -> &'static str {
    if i {
        "interlaced"
    } else {
        "progressive"
    }
}

fn bench_encode_strategy_matrix(c: &mut Criterion) {
    let rec = tables::lookup(BENCH_FORMAT_BYTE).expect("M8Y0 in FOURCC table");
    let mut group = c.benchmark_group("encode_strategy_matrix_m8y0_640x480");
    group.throughput(Throughput::Bytes(raw_plane_bytes(
        rec,
        BENCH_WIDTH,
        BENCH_HEIGHT,
    )));

    let strategies = [
        PredictorStrategy::Fixed(PredictorKind::Left),
        PredictorStrategy::Fixed(PredictorKind::Gradient),
        PredictorStrategy::Fixed(PredictorKind::Median),
        PredictorStrategy::Dynamic,
    ];
    let modes = [SliceMode::Huffman, SliceMode::Raw, SliceMode::Auto];
    let interlaced_axis = [false, true];

    for &strategy in &strategies {
        // `EncodeOptions::predictor` is documented as ignored when
        // `strategy = Fixed(_)`. Mirror the in-crate convention (and
        // the fuzz target's): keep it consistent with `strategy` when
        // Fixed, default to Gradient under Dynamic.
        let predictor = match strategy {
            PredictorStrategy::Fixed(p) => p,
            PredictorStrategy::Dynamic => PredictorKind::Gradient,
        };
        for &mode in &modes {
            for &interlaced in &interlaced_axis {
                let opts = EncodeOptions {
                    strategy,
                    mode,
                    interlaced,
                    predictor,
                };
                let label = format!(
                    "{}/{}/{}",
                    strategy_tag(strategy),
                    mode_tag(mode),
                    interlaced_tag(interlaced),
                );
                group.bench_function(BenchmarkId::from_parameter(label), |b| {
                    b.iter(|| {
                        // Per-iteration plane rebuild: the encoder
                        // consumes `Vec<PlaneInput>` by value, so the
                        // bench has to refresh the input. Mirrors the
                        // existing `encode` bench shape (per-iter
                        // memcpy is < 5 % of any cell here at
                        // 640×480/8-bit/4:2:0 = 460 800 sample bytes).
                        let planes = make_planes(rec, BENCH_WIDTH, BENCH_HEIGHT);
                        let _ = encode_frame(
                            rec,
                            BENCH_WIDTH,
                            BENCH_HEIGHT,
                            SLICE_HEIGHT,
                            planes,
                            opts,
                        )
                        .expect("encode");
                    });
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_encode_strategy_matrix);
criterion_main!(benches);
