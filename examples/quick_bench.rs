//! Standalone quick bench used during the optimization round to get a
//! fast turnaround on micro-tweaks. Criterion is the canonical harness
//! (`cargo bench`), but for the inner measure-tweak-remeasure loop a
//! flat 30-iteration time-this-thing tool moves faster.

use std::time::Instant;

use oxideav_magicyuv::tables::{Family, FourccRecord, PredictorKind};
use oxideav_magicyuv::{
    decode_frame, decode_into, encode_frame, tables, DecodedFrame, EncodeOptions, PlaneInput,
    SliceMode,
};

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

fn time_decode(name: &str, format_byte: u8, w: u32, h: u32, predictor: PredictorKind, iters: u32) {
    use std::io::Write;
    print!("  decode {:38} ", name);
    std::io::stdout().flush().ok();
    let rec = tables::lookup(format_byte).unwrap();
    let planes = make_planes(rec, w, h);
    let opts = EncodeOptions {
        mode: SliceMode::Huffman,
        ..EncodeOptions::fixed(predictor)
    };
    let t_enc = Instant::now();
    let frame = encode_frame(rec, w, h, 28, planes, opts).unwrap();
    let enc_ms = t_enc.elapsed().as_secs_f64() * 1000.0;

    // 1 warmup
    let _ = decode_frame(&frame).unwrap();

    let t = Instant::now();
    for _ in 0..iters {
        let _ = std::hint::black_box(decode_frame(std::hint::black_box(&frame)).unwrap());
    }
    let elapsed = t.elapsed().as_secs_f64();
    let per_iter_ms = elapsed * 1000.0 / iters as f64;

    // Streaming-reuse variant: one DecodedFrame, N decode_into calls.
    let mut dst = DecodedFrame::empty();
    decode_into(&frame, &mut dst).unwrap(); // warmup + alloc plane buffers
    let t2 = Instant::now();
    for _ in 0..iters {
        decode_into(std::hint::black_box(&frame), &mut dst).unwrap();
        std::hint::black_box(&dst);
    }
    let per_iter_ms_into = t2.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    let delta = (per_iter_ms - per_iter_ms_into) / per_iter_ms * 100.0;
    println!(
        "{:7.2} ms/iter  into={:7.2} ({:+5.1}%)  frame_bytes={}  (one-shot encode {:.1}ms)",
        per_iter_ms,
        per_iter_ms_into,
        -delta,
        frame.len(),
        enc_ms
    );
}

fn time_encode(name: &str, format_byte: u8, w: u32, h: u32, predictor: PredictorKind, iters: u32) {
    use std::io::Write;
    print!("  encode {:38} ", name);
    std::io::stdout().flush().ok();
    let rec = tables::lookup(format_byte).unwrap();
    let opts = EncodeOptions {
        mode: SliceMode::Huffman,
        ..EncodeOptions::fixed(predictor)
    };
    let _ = encode_frame(rec, w, h, 28, make_planes(rec, w, h), opts).unwrap();

    let t = Instant::now();
    for _ in 0..iters {
        let p = make_planes(rec, w, h);
        let _ = std::hint::black_box(encode_frame(rec, w, h, 28, p, opts).unwrap());
    }
    let elapsed = t.elapsed().as_secs_f64();
    let per_iter_ms = elapsed * 1000.0 / iters as f64;
    println!("{:7.2} ms/iter", per_iter_ms);
}

fn main() {
    use std::env;
    let pick = env::args().nth(1).unwrap_or_else(|| "all".into());
    println!("=== oxideav-magicyuv quick-bench ({pick}) ===");
    println!();
    if pick == "all" || pick == "decode" {
        println!("Decode (per-iteration ms):");
        time_decode(
            "M8RG/gradient/1280x720",
            0x65,
            1280,
            720,
            PredictorKind::Gradient,
            10,
        );
        time_decode(
            "M8Y0/gradient/1280x720",
            0x69,
            1280,
            720,
            PredictorKind::Gradient,
            10,
        );
        time_decode(
            "M8G0/left/1920x1080",
            0x6b,
            1920,
            1080,
            PredictorKind::Left,
            10,
        );
        time_decode(
            "M0RG/gradient/1280x720/10bit",
            0x6d,
            1280,
            720,
            PredictorKind::Gradient,
            10,
        );
        time_decode(
            "M8RG/median/256x256",
            0x65,
            256,
            256,
            PredictorKind::Median,
            30,
        );
        // Small-frame streaming scenario — malloc overhead is a
        // larger fraction of total work here, so `decode_into`
        // shines.
        time_decode(
            "M8RG/gradient/128x128",
            0x65,
            128,
            128,
            PredictorKind::Gradient,
            200,
        );
        time_decode(
            "M0RG/gradient/128x128/10bit",
            0x6d,
            128,
            128,
            PredictorKind::Gradient,
            200,
        );
    }
    if pick == "all" || pick == "encode" {
        println!();
        println!("Encode (per-iteration ms, plane-build included):");
        time_encode(
            "M8RG/gradient/1280x720",
            0x65,
            1280,
            720,
            PredictorKind::Gradient,
            5,
        );
        time_encode(
            "M8Y0/gradient/1280x720",
            0x69,
            1280,
            720,
            PredictorKind::Gradient,
            5,
        );
        time_encode(
            "M8G0/left/1920x1080",
            0x6b,
            1920,
            1080,
            PredictorKind::Left,
            5,
        );
        time_encode(
            "M0RG/gradient/1280x720/10bit",
            0x6d,
            1280,
            720,
            PredictorKind::Gradient,
            5,
        );
    }
}
