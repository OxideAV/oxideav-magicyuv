//! Criterion benchmarks for the MagicYUV encoder hot paths.
//!
//! Mirrors the `decode` bench coverage. Each scenario rebuilds the
//! per-plane input buffers fresh per iteration (the encoder consumes
//! by value), which adds a tiny constant memcpy overhead but keeps
//! the bench self-consistent.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_magicyuv::tables::{Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{encode_frame, tables, EncodeOptions, PlaneInput, SliceMode};

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

fn run_encode(format_byte: u8, width: u32, height: u32, predictor: PredictorKind) {
    let rec = tables::lookup(format_byte).expect("FOURCC in CSV");
    let planes = make_planes(rec, width, height);
    let opts = EncodeOptions {
        mode: SliceMode::Huffman,
        ..EncodeOptions::fixed(predictor)
    };
    let _ = encode_frame(rec, width, height, 28, planes, opts).expect("encode");
}

fn bench_encode_m8rg_720p(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_m8rg_720p");
    g.throughput(Throughput::Bytes((1280 * 720 * 3) as u64));
    g.bench_function(BenchmarkId::from_parameter("M8RG/gradient/1280x720"), |b| {
        b.iter(|| run_encode(0x65, 1280, 720, PredictorKind::Gradient));
    });
    g.finish();
}

fn bench_encode_m8y0_720p(c: &mut Criterion) {
    // Median + 720p hits the encoder's known Huffman-builder slow-path;
    // benchmark with Gradient (the dominant predictor in practice).
    let mut g = c.benchmark_group("encode_m8y0_720p");
    g.throughput(Throughput::Bytes((1280 * 720 * 3 / 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("M8Y0/gradient/1280x720"), |b| {
        b.iter(|| run_encode(0x69, 1280, 720, PredictorKind::Gradient));
    });
    g.finish();
}

fn bench_encode_m8g0_1080p(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_m8g0_1080p");
    g.throughput(Throughput::Bytes((1920 * 1080) as u64));
    g.bench_function(BenchmarkId::from_parameter("M8G0/left/1920x1080"), |b| {
        b.iter(|| run_encode(0x6b, 1920, 1080, PredictorKind::Left));
    });
    g.finish();
}

fn bench_encode_m0rg_720p_10bit(c: &mut Criterion) {
    let mut g = c.benchmark_group("encode_m0rg_720p_10bit");
    g.throughput(Throughput::Bytes((1280 * 720 * 3 * 2) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("M0RG/gradient/1280x720/10bit"),
        |b| {
            b.iter(|| run_encode(0x6d, 1280, 720, PredictorKind::Gradient));
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_encode_m8rg_720p,
    bench_encode_m8y0_720p,
    bench_encode_m8g0_1080p,
    bench_encode_m0rg_720p_10bit,
);
criterion_main!(benches);
