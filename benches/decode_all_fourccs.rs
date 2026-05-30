//! Criterion benchmark covering decode throughput across **every**
//! native MagicYUV v7 FOURCC defined in
//! `tables/00-fourcc-table.csv`.
//!
//! The existing `decode` bench picks five hand-selected scenarios
//! aimed at the optimisation rounds (one per hot-path archetype).
//! This bench is the breadth complement: it runs every FOURCC the
//! crate claims to support at the same resolution + predictor so
//! per-format decode throughput can be compared at a glance.
//!
//! Resolution: 640×480 — divisible by 4 in both dimensions so the
//! YUV 4:2:0 / 4:2:2 chroma planes are whole-byte; small enough to
//! finish the whole sweep in well under a minute even on the
//! Criterion default 5 s measurement window.
//!
//! Predictor: Gradient. The Gradient predictor is available for
//! every bit-depth (modular 8-bit + JPEG-LS 10/12/14-bit), produces
//! residuals that exercise the Huffman path (Left+smooth-gradient
//! input collapses to a single-symbol Huffman tree which is not
//! representative of natural-image work), and dodges the encoder's
//! known Median-at-large-resolution slow path documented in the
//! `decode` bench.
//!
//! Slice mode: Huffman (the default observed in reference-encoder
//! captures per `provenance/`).
//!
//! Throughput is reported in raw pixel **bytes** (uncompressed
//! plane bytes — `width * height * sum(plane_bytes_per_sample) /
//! product(subsampling)`), so the GB/s figures across formats are
//! directly comparable as "decoded pixel volume per second".
//!
//! Run with:
//!     cargo bench -p oxideav-magicyuv --bench decode_all_fourccs

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_magicyuv::tables::{fourcc_table, Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{decode_frame, encode_frame, EncodeOptions, PlaneInput, SliceMode};

const BENCH_WIDTH: u32 = 640;
const BENCH_HEIGHT: u32 = 480;
const SLICE_HEIGHT: u32 = 28;

/// Cheap deterministic xorshift32 — mirrors the existing `decode`
/// bench so per-pixel residual statistics line up across both
/// harnesses.
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

fn build_plane_u16(width: usize, height: usize, plane: usize, mask: u16) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    let mut state: u32 = 0xdead_beef ^ (plane as u32).wrapping_mul(0x9e37_79b9);
    for r in 0..height {
        for c in 0..width {
            let base = ((r as u32).wrapping_add(c as u32) >> 1) & (mask as u32);
            let n1 = xorshift_byte(&mut state) as u32;
            let n2 = xorshift_byte(&mut state) as u32;
            let noise = ((n1 << 8) | n2) & 0x1f;
            out[r * width + c] = (base.wrapping_add(noise) as u16) & mask;
        }
    }
    out
}

fn make_planes(rec: FourccRecord, width: u32, height: u32) -> Vec<PlaneInput> {
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
            if rec.is_high_bit_depth() {
                PlaneInput::U16(build_plane_u16(pw, ph, p, mask))
            } else {
                PlaneInput::U8(build_plane_u8(pw, ph, p))
            }
        })
        .collect()
}

/// Raw uncompressed plane bytes for the given FOURCC + image
/// dimensions. Used as the Criterion throughput numerator so the
/// per-format GB/s figures are comparable as "pixel data decoded
/// per second".
fn raw_plane_bytes(rec: FourccRecord, width: u32, height: u32) -> u64 {
    let w = width as u64;
    let h = height as u64;
    let bytes_per_sample = if rec.is_high_bit_depth() { 2 } else { 1 };
    let mut total: u64 = 0;
    for p in 0..rec.planes as usize {
        let (sub_x, sub_y) = match rec.family {
            Family::Yuv if p == 1 || p == 2 => (rec.sub_x as u64, rec.sub_y as u64),
            Family::Yuva if p == 1 || p == 2 => (rec.sub_x as u64, rec.sub_y as u64),
            _ => (1u64, 1u64),
        };
        total += (w / sub_x) * (h / sub_y) * bytes_per_sample;
    }
    total
}

/// One Criterion group per FOURCC. Each group has a single bench
/// function named after the four-character FOURCC tag so the
/// `cargo bench` output is naturally sorted alphabetically by tag.
fn bench_all_fourccs(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_all_fourccs_640x480_gradient");
    for rec in fourcc_table() {
        let tag = std::str::from_utf8(&rec.fourcc).expect("ascii fourcc tag");
        // Encode one frame outside the timing loop. The bench
        // measures decode only — encode timings are covered by the
        // `encode` bench harness.
        let planes = make_planes(*rec, BENCH_WIDTH, BENCH_HEIGHT);
        let opts = EncodeOptions {
            mode: SliceMode::Huffman,
            ..EncodeOptions::fixed(PredictorKind::Gradient)
        };
        let frame = encode_frame(*rec, BENCH_WIDTH, BENCH_HEIGHT, SLICE_HEIGHT, planes, opts)
            .expect("encode for bench input");

        group.throughput(Throughput::Bytes(raw_plane_bytes(
            *rec,
            BENCH_WIDTH,
            BENCH_HEIGHT,
        )));
        group.bench_function(BenchmarkId::from_parameter(tag), |b| {
            b.iter(|| decode_frame(criterion::black_box(&frame)).expect("decode"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_all_fourccs);
criterion_main!(benches);
