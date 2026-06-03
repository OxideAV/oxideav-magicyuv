//! Standalone profiling driver for the MagicYUV encoder and decoder.
//!
//! Round 206 (depth-mode profiling slot). The crate already ships
//! Criterion benches (`benches/{decode,encode,roundtrip,decode_all_fourccs,
//! encode_strategy_matrix}.rs`) and a tight `examples/quick_bench.rs`
//! timing helper, but neither is a clean target for `samply` /
//! `perf record` / `cargo flamegraph`:
//!
//! * Criterion's warm-up + sampling layers + estimator math show up in
//!   the profile and bury the real codec hot paths.
//! * `quick_bench` runs each scenario for 10-30 iterations, too short
//!   for a sampling profiler to settle on the codec body before the
//!   loop exits.
//!
//! This example is a flat, steady-state driver: it builds a
//! deterministic synthesised input once per scenario, then runs a
//! configurable number of iterations of whichever path was requested
//! with a single `Instant::now()` / `elapsed()` pair around the whole
//! loop. Throughput is printed at the end so it doubles as a quick
//! A/B harness for the inner tweak-remeasure loop when Criterion's
//! per-run overhead is too coarse.
//!
//! Usage:
//!
//!     cargo run --example profile_magicyuv --release -- <mode> [<iters>]
//!
//! Modes:
//!
//!     decode      — encode each scenario once outside the loop, decode
//!                   N times against the cached bytes via `decode_into`
//!                   (the streaming entry point that re-uses the
//!                   per-plane storage across iterations)
//!     encode      — synth pixels once, encode N times with the fixed
//!                   Huffman strategy used in the per-FOURCC bench
//!                   matrix
//!     roundtrip   — synth pixels once, encode + `decode_into` every
//!                   iteration
//!     dynamic     — encode N times with `EncodeOptions::dynamic_auto()`
//!                   — spec/04 §3 + spec/05 §6.2, the v2.4.2 always-on
//!                   per-slice predictor + Huffman/raw selection (the
//!                   most expensive encode configuration)
//!     interlaced  — decode N times against a `flags & FLAG_INTERLACED`
//!                   frame, exercising the spec/04 §5.1 field-stride=2
//!                   predictor path
//!     raw         — decode N times against a high-bit-depth raw-mode
//!                   slice payload (`slice_flags & 0x01 == 1`), the
//!                   spec/05 §4.1 bit-packed bytes-to-u16 unpacker —
//!                   per-pixel `BitReader::read_bits` is replaced by
//!                   `bitreader::unpack_raw_bits_to_u16`, a single
//!                   refill check every floor(56 / bits) pixels rather
//!                   than once per pixel
//!     all         — run every mode in order (default)
//!
//! With `samply`:
//!
//!     samply record -- ./target/release/examples/profile_magicyuv encode 500
//!     samply record -- ./target/release/examples/profile_magicyuv decode 2000
//!     samply record -- ./target/release/examples/profile_magicyuv dynamic 500
//!
//! With `cargo flamegraph` (needs `cargo install flamegraph`):
//!
//!     cargo flamegraph --example profile_magicyuv -- decode 2000
//!
//! No external files are read — every input is synthesised in-driver
//! from a deterministic gradient + xorshift-noise pattern matching the
//! Criterion bench harnesses byte-for-byte, so profile output and
//! bench numbers reference the same residual distribution. Scenarios
//! cover the dominant cost-axes the workspace README rows track: the
//! 8-bit primary-Huffman path (M8RG / M8Y0 / M8G0), the two-level
//! 10-bit Huffman path (M0RG), and the modular-Median 8-bit path
//! (M8RG small frame).
//!
//! Allow-list: `oxideav-magicyuv`'s own public API only — no
//! `oxideav-core`, no `docs/`, no external libraries.

use std::env;
use std::io::Write;
use std::time::Instant;

use oxideav_magicyuv::tables::{Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{
    decode_into, encode_frame, tables, DecodedFrame, EncodeOptions, PlaneInput, SliceMode,
};

// ───────────────────── synthesised input planes ─────────────────────
//
// Deterministic gradient + 3-bit xorshift noise. Same shape the
// `quick_bench` helper uses so profile and bench inputs are
// byte-identical and the residual histogram lines up.

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

// ───────────────────── scenarios ─────────────────────

/// Per-scenario knobs. The picks match the `quick_bench` and Criterion
/// bench archetypes so profile and bench numbers reference identical
/// inputs.
struct Scenario {
    name: &'static str,
    /// `format_byte` per `tables/00-fourcc-table.csv` — the byte the
    /// MAGY header carries at offset +0x09.
    format_byte: u8,
    width: u32,
    height: u32,
    /// Fixed predictor used for the per-FOURCC encode + decode cells.
    /// Dynamic mode overrides this; interlaced mode picks Gradient.
    predictor: PredictorKind,
    /// Default iteration count for the encode path. The decode path
    /// scales this up internally (decode is much cheaper per iter).
    encode_iters_default: u32,
}

fn scenarios() -> &'static [Scenario] {
    &[
        Scenario {
            name: "M8RG/gradient/1280x720",
            format_byte: 0x65,
            width: 1280,
            height: 720,
            predictor: PredictorKind::Gradient,
            encode_iters_default: 60,
        },
        Scenario {
            name: "M8Y0/gradient/1280x720",
            format_byte: 0x69,
            width: 1280,
            height: 720,
            predictor: PredictorKind::Gradient,
            encode_iters_default: 80,
        },
        Scenario {
            name: "M8G0/left/1920x1080",
            format_byte: 0x6b,
            width: 1920,
            height: 1080,
            predictor: PredictorKind::Left,
            encode_iters_default: 60,
        },
        Scenario {
            name: "M0RG/gradient/1280x720/10bit",
            format_byte: 0x6d,
            width: 1280,
            height: 720,
            predictor: PredictorKind::Gradient,
            encode_iters_default: 40,
        },
        Scenario {
            name: "M8RG/median/256x256",
            format_byte: 0x65,
            width: 256,
            height: 256,
            predictor: PredictorKind::Median,
            encode_iters_default: 500,
        },
    ]
}

/// Raw uncompressed plane-bytes per iteration. The throughput print
/// divides this by elapsed time to give a MiB/s figure that's directly
/// comparable across bit-depths and subsampling.
fn raw_bytes_per_iter(rec: FourccRecord, w: u32, h: u32) -> usize {
    let mut total = 0usize;
    let bytes_per_sample = if rec.is_high_bit_depth() { 2 } else { 1 };
    for p in 0..rec.planes as usize {
        let (sub_x, sub_y) = match rec.family {
            Family::Yuv if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
            Family::Yuva if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
            _ => (1usize, 1usize),
        };
        let pw = (w as usize) / sub_x;
        let ph = (h as usize) / sub_y;
        total += pw * ph * bytes_per_sample;
    }
    total
}

fn print_throughput_line(
    label: &str,
    name: &str,
    iters: u32,
    elapsed_secs: f64,
    raw_bytes_total: u64,
    extra: &str,
) {
    let per_iter_ms = elapsed_secs * 1000.0 / iters as f64;
    let mib_per_s = (raw_bytes_total as f64) / elapsed_secs / (1024.0 * 1024.0);
    println!(
        "  {label:9} {name:34} iters={iters:>5} {per_iter_ms:9.3} ms/iter  {mib_per_s:9.2} MiB/s (raw){extra}",
    );
}

// ───────────────────── modes ─────────────────────

fn profile_encode(iters_override: Option<u32>) {
    println!("== encode (fixed Huffman, predictor per scenario) ==");
    for scen in scenarios() {
        let iters = iters_override.unwrap_or(scen.encode_iters_default);
        let rec = tables::lookup(scen.format_byte).expect("format_byte in fourcc table");
        let opts = EncodeOptions {
            mode: SliceMode::Huffman,
            ..EncodeOptions::fixed(scen.predictor)
        };
        let raw = raw_bytes_per_iter(rec, scen.width, scen.height) as u64;

        // Warm-up — first encode pays any lazy table-parse cost.
        {
            let p = make_planes(rec, scen.width, scen.height);
            let _ = encode_frame(rec, scen.width, scen.height, 28, p, opts).expect("warmup encode");
        }

        let t = Instant::now();
        let mut total_out_bytes = 0u64;
        for _ in 0..iters {
            let p = make_planes(rec, scen.width, scen.height);
            let bytes = std::hint::black_box(
                encode_frame(
                    rec,
                    scen.width,
                    scen.height,
                    28,
                    std::hint::black_box(p),
                    opts,
                )
                .expect("encode_frame"),
            );
            total_out_bytes += bytes.len() as u64;
            std::hint::black_box(bytes);
        }
        let elapsed = t.elapsed().as_secs_f64();
        let avg = total_out_bytes / iters.max(1) as u64;
        let ratio = avg as f64 / raw as f64;
        let extra = format!("  out={avg}B/iter ({ratio:.3} of input)");
        print_throughput_line(
            "encode",
            scen.name,
            iters,
            elapsed,
            raw * iters as u64,
            &extra,
        );
        std::io::stdout().flush().ok();
    }
}

fn profile_decode(iters_override: Option<u32>) {
    println!("== decode (decode_into, plane storage re-used) ==");
    for scen in scenarios() {
        // Decode is the cheaper side — scale the iteration count up
        // when the caller didn't pick one explicitly.
        let iters = iters_override.unwrap_or(scen.encode_iters_default * 50);
        let rec = tables::lookup(scen.format_byte).expect("format_byte in fourcc table");
        let opts = EncodeOptions {
            mode: SliceMode::Huffman,
            ..EncodeOptions::fixed(scen.predictor)
        };
        let raw = raw_bytes_per_iter(rec, scen.width, scen.height) as u64;

        // Encode once OUTSIDE the timed region — the decode driver
        // only times the decode path.
        let p = make_planes(rec, scen.width, scen.height);
        let bytes = encode_frame(rec, scen.width, scen.height, 28, p, opts).expect("setup encode");

        // Caller-owned `DecodedFrame`. `decode_into` re-uses its
        // per-plane storage across iterations after the first, so the
        // hot loop is allocator-quiet.
        let mut dst = DecodedFrame::empty();
        decode_into(&bytes, &mut dst).expect("warmup decode_into");

        let t = Instant::now();
        for _ in 0..iters {
            decode_into(std::hint::black_box(&bytes), &mut dst).expect("decode_into");
            std::hint::black_box(&dst);
        }
        let elapsed = t.elapsed().as_secs_f64();
        print_throughput_line("decode", scen.name, iters, elapsed, raw * iters as u64, "");
        std::io::stdout().flush().ok();
    }
}

fn profile_roundtrip(iters_override: Option<u32>) {
    println!("== roundtrip (encode + decode_into per iter) ==");
    for scen in scenarios() {
        let iters = iters_override.unwrap_or(scen.encode_iters_default);
        let rec = tables::lookup(scen.format_byte).expect("format_byte in fourcc table");
        let opts = EncodeOptions {
            mode: SliceMode::Huffman,
            ..EncodeOptions::fixed(scen.predictor)
        };
        let raw = raw_bytes_per_iter(rec, scen.width, scen.height) as u64;

        // Warm-up — one full encode + decode pair.
        {
            let p = make_planes(rec, scen.width, scen.height);
            let bytes =
                encode_frame(rec, scen.width, scen.height, 28, p, opts).expect("warmup encode");
            let mut dst = DecodedFrame::empty();
            decode_into(&bytes, &mut dst).expect("warmup decode_into");
        }

        let mut dst = DecodedFrame::empty();
        let t = Instant::now();
        for _ in 0..iters {
            let p = make_planes(rec, scen.width, scen.height);
            let bytes = std::hint::black_box(
                encode_frame(rec, scen.width, scen.height, 28, p, opts).expect("encode_frame"),
            );
            decode_into(std::hint::black_box(&bytes), &mut dst).expect("decode_into");
            std::hint::black_box(&dst);
        }
        let elapsed = t.elapsed().as_secs_f64();
        print_throughput_line(
            "roundtrip",
            scen.name,
            iters,
            elapsed,
            raw * iters as u64,
            "",
        );
        std::io::stdout().flush().ok();
    }
}

fn profile_dynamic(iters_override: Option<u32>) {
    println!("== dynamic_auto (spec/04 §3 + spec/05 §6.2, v2.4.2 always-on) ==");
    for scen in scenarios() {
        let iters = iters_override.unwrap_or(scen.encode_iters_default);
        let rec = tables::lookup(scen.format_byte).expect("format_byte in fourcc table");
        let opts = EncodeOptions::dynamic_auto();
        let raw = raw_bytes_per_iter(rec, scen.width, scen.height) as u64;

        // Warm-up.
        {
            let p = make_planes(rec, scen.width, scen.height);
            let _ = encode_frame(rec, scen.width, scen.height, 28, p, opts)
                .expect("warmup dynamic encode");
        }

        let t = Instant::now();
        let mut total_out_bytes = 0u64;
        for _ in 0..iters {
            let p = make_planes(rec, scen.width, scen.height);
            let bytes = std::hint::black_box(
                encode_frame(
                    rec,
                    scen.width,
                    scen.height,
                    28,
                    std::hint::black_box(p),
                    opts,
                )
                .expect("encode_frame"),
            );
            total_out_bytes += bytes.len() as u64;
            std::hint::black_box(bytes);
        }
        let elapsed = t.elapsed().as_secs_f64();
        let avg = total_out_bytes / iters.max(1) as u64;
        let ratio = avg as f64 / raw as f64;
        let extra = format!("  out={avg}B/iter ({ratio:.3} of input)");
        print_throughput_line(
            "dynamic",
            scen.name,
            iters,
            elapsed,
            raw * iters as u64,
            &extra,
        );
        std::io::stdout().flush().ok();
    }
}

/// Interlaced 8-bit YUV 4:2:0 decode. `flags & FLAG_INTERLACED == 0x02`
/// puts the predictor on field-stride=2 (top neighbour = row r-2,
/// first two rows raw). Exercises the spec/04 §5.1 field-split path
/// that the per-FOURCC scenarios above all leave on the progressive
/// side. The encoder is fed the same synthesised planes; only the
/// `flags` byte differs. We pick a small Gradient frame so the
/// scenario fits in a single profile run without dominating the wall
/// time of `all` mode.
fn profile_interlaced(iters_override: Option<u32>) {
    println!("== interlaced (M8Y0 4:2:0 gradient 640x480 flags & 0x02) ==");
    let iters = iters_override.unwrap_or(2000);
    let rec = tables::lookup(0x69).expect("M8Y0 in fourcc table");
    let width: u32 = 640;
    let height: u32 = 480;
    let opts = EncodeOptions {
        mode: SliceMode::Huffman,
        interlaced: true,
        ..EncodeOptions::fixed(PredictorKind::Gradient)
    };
    let raw = raw_bytes_per_iter(rec, width, height) as u64;

    // Encode once outside the timed region.
    let planes = make_planes(rec, width, height);
    let bytes =
        encode_frame(rec, width, height, 28, planes, opts).expect("interlaced setup encode");

    // Warm-up.
    let mut dst = DecodedFrame::empty();
    decode_into(&bytes, &mut dst).expect("warmup interlaced decode_into");

    let t = Instant::now();
    for _ in 0..iters {
        decode_into(std::hint::black_box(&bytes), &mut dst).expect("interlaced decode_into");
        std::hint::black_box(&dst);
    }
    let elapsed = t.elapsed().as_secs_f64();
    print_throughput_line(
        "interlaced",
        "M8Y0/gradient/640x480/i",
        iters,
        elapsed,
        raw * iters as u64,
        "",
    );
    std::io::stdout().flush().ok();
}

/// High-bit-depth raw-mode decode (`slice_flags & 0x01`). Targets the
/// batched `unpack_raw_bits_to_u16` hot loop introduced in round 212 —
/// the rest of the high-bit-depth pipeline (slice walking, predictor
/// inverse, RGB decorrelation) is shared with the Huffman variant
/// already covered by `profile_decode`, so any delta between this
/// scenario's MiB/s and `decode`'s M0RG row is attributable to the
/// unpacker. Three bit-depth tiers (10/12/14) so the per-tier refill
/// cadence (≈ 5 / ≈ 4 / ≈ 4 pixels between refills) is exercised end
/// to end.
fn profile_raw(iters_override: Option<u32>) {
    println!("== raw (high-bit-depth bit-packed slice payload, decode_into) ==");
    // (format_byte, label, width, height, iters_default)
    let rows: &[(u8, &'static str, u32, u32, u32)] = &[
        (0x6d, "M0RG/10bit/640x480/raw", 640, 480, 4000),
        (0x6f, "M2RG/12bit/640x480/raw", 640, 480, 4000),
        (0x71, "M4RG/14bit/640x480/raw", 640, 480, 4000),
    ];
    for (fb, label, w, h, iters_default) in rows {
        let iters = iters_override.unwrap_or(*iters_default);
        let rec = tables::lookup(*fb).expect("format_byte in fourcc table");
        let opts = EncodeOptions {
            mode: SliceMode::Raw,
            ..EncodeOptions::fixed(PredictorKind::Left)
        };
        let raw = raw_bytes_per_iter(rec, *w, *h) as u64;
        // Encode once outside the timed region.
        let planes = make_planes(rec, *w, *h);
        let bytes = encode_frame(rec, *w, *h, 28, planes, opts).expect("raw setup encode");
        let mut dst = DecodedFrame::empty();
        decode_into(&bytes, &mut dst).expect("warmup raw decode_into");
        let t = Instant::now();
        for _ in 0..iters {
            decode_into(std::hint::black_box(&bytes), &mut dst).expect("raw decode_into");
            std::hint::black_box(&dst);
        }
        let elapsed = t.elapsed().as_secs_f64();
        print_throughput_line("raw", label, iters, elapsed, raw * iters as u64, "");
        std::io::stdout().flush().ok();
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "all".to_string());
    let iters_override: Option<u32> = args.next().and_then(|s| s.parse().ok());

    println!(
        "=== oxideav-magicyuv profile (mode={mode}, iters={}) ===",
        iters_override
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".to_string()),
    );
    println!();

    match mode.as_str() {
        "encode" => profile_encode(iters_override),
        "decode" => profile_decode(iters_override),
        "roundtrip" => profile_roundtrip(iters_override),
        "dynamic" | "dynamic_auto" => profile_dynamic(iters_override),
        "interlaced" => profile_interlaced(iters_override),
        "raw" => profile_raw(iters_override),
        "all" => {
            profile_encode(iters_override);
            println!();
            profile_decode(iters_override);
            println!();
            profile_roundtrip(iters_override);
            println!();
            profile_dynamic(iters_override);
            println!();
            profile_interlaced(iters_override);
            println!();
            profile_raw(iters_override);
        }
        other => {
            eprintln!("unknown mode: {other:?}");
            eprintln!(
                "usage: profile_magicyuv [encode|decode|roundtrip|dynamic|interlaced|raw|all] [<iters>]"
            );
            std::process::exit(2);
        }
    }
}
