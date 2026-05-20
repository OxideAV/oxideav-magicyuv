//! Encode-then-decode roundtrip benchmark. Useful as a single-figure
//! "end-to-end pipeline" health check — adds the encode + decode wall
//! times together so a regression in either side is visible.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_magicyuv::tables::{Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{decode_frame, encode_frame, tables, EncodeOptions, PlaneInput, SliceMode};

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

fn run_roundtrip(format_byte: u8, width: u32, height: u32, predictor: PredictorKind) {
    let rec = tables::lookup(format_byte).expect("FOURCC in CSV");
    let planes = make_planes(rec, width, height);
    let opts = EncodeOptions {
        mode: SliceMode::Huffman,
        ..EncodeOptions::fixed(predictor)
    };
    let frame = encode_frame(rec, width, height, 28, planes, opts).expect("encode");
    let _ = decode_frame(&frame).expect("decode");
}

fn bench_roundtrip_m8rg_720p(c: &mut Criterion) {
    let mut g = c.benchmark_group("roundtrip_m8rg_720p");
    g.throughput(Throughput::Bytes((1280 * 720 * 3) as u64));
    g.bench_function(BenchmarkId::from_parameter("M8RG/gradient/1280x720"), |b| {
        b.iter(|| run_roundtrip(0x65, 1280, 720, PredictorKind::Gradient));
    });
    g.finish();
}

criterion_group!(benches, bench_roundtrip_m8rg_720p);
criterion_main!(benches);
