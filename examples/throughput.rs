//! Encode / decode throughput on a raw frame file.
//!
//! ```text
//! throughput <FOURCC> <width> <height> <frame.raw> [slice_height] [runs] [threads]
//! ```
//!
//! `frame.raw` holds one frame's planes back to back in the FOURCC's
//! family order (one byte per sample at 8-bit, little-endian 16-bit
//! words above). Each figure is the median of `runs` timings (default
//! 9) in MiB of raw samples per second. `threads` (default 1) is the
//! grant handed to both the encoder and the decoder.

use std::time::Instant;

use oxideav_magicyuv::tables::{PredictorKind, fourcc_table};
use oxideav_magicyuv::{
    DecodeOptions, DecodedFrame, EncodeOptions, PlaneInput, PredictorStrategy, SliceMode,
    decode_into_with, encode_frame,
};

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!(
            "usage: throughput <FOURCC> <width> <height> <frame.raw> [slice_height] [runs] [threads]"
        );
        std::process::exit(2);
    }
    let rec = fourcc_table()
        .iter()
        .copied()
        .find(|r| r.fourcc == args[0].as_bytes())
        .expect("unknown FOURCC");
    let w: u32 = args[1].parse().expect("width");
    let h: u32 = args[2].parse().expect("height");
    let raw = std::fs::read(&args[3]).expect("read frame");
    let slice_height: u32 = args.get(4).map_or(28, |s| s.parse().expect("slice_height"));
    let runs: usize = args.get(5).map_or(9, |s| s.parse().expect("runs"));
    let threads: usize = args.get(6).map_or(1, |s| s.parse().expect("threads"));

    let mut planes = Vec::new();
    let mut pos = 0usize;
    for i in 0..rec.planes as usize {
        let chroma = (i == 1 || i == 2) && (rec.sub_x > 1 || rec.sub_y > 1);
        let (pw, ph) = if chroma {
            (w.div_ceil(rec.sub_x as u32), h.div_ceil(rec.sub_y as u32))
        } else {
            (w, h)
        };
        let n = pw as usize * ph as usize;
        if rec.is_high_bit_depth() {
            let (words, _) = raw[pos..pos + 2 * n].as_chunks::<2>();
            pos += 2 * n;
            planes.push(PlaneInput::U16(
                words.iter().map(|c| u16::from_le_bytes(*c)).collect(),
            ));
        } else {
            planes.push(PlaneInput::U8(raw[pos..pos + n].to_vec()));
            pos += n;
        }
    }
    let mib = pos as f64 / (1024.0 * 1024.0);

    let configs: [(&str, EncodeOptions); 4] = [
        ("left", EncodeOptions::fixed(PredictorKind::Left)),
        ("gradient", EncodeOptions::fixed(PredictorKind::Gradient)),
        ("median", EncodeOptions::fixed(PredictorKind::Median)),
        (
            "dynamic+auto",
            EncodeOptions::default()
                .with_strategy(PredictorStrategy::Dynamic)
                .with_mode(SliceMode::Auto),
        ),
    ];
    println!(
        "{} {w}x{h} slice_height={slice_height} threads={threads} ({mib:.1} MiB raw), median of {runs}",
        args[0]
    );
    for (name, opts) in configs {
        let opts = opts.with_threads(threads);
        let mut frame = Vec::new();
        let mut enc = Vec::new();
        for _ in 0..runs {
            let input = planes.clone();
            let t = Instant::now();
            frame = encode_frame(rec, w, h, slice_height, input, opts).expect("encode");
            enc.push(t.elapsed().as_secs_f64());
        }
        let mut dst = DecodedFrame::empty();
        let mut dec = Vec::new();
        for _ in 0..runs {
            let t = Instant::now();
            decode_into_with(&frame, &mut dst, DecodeOptions::new().with_threads(threads))
                .expect("decode");
            dec.push(t.elapsed().as_secs_f64());
        }
        println!(
            "  {name:13} encode {:7.1} MiB/s   decode {:7.1} MiB/s   ratio {:.3}",
            mib / median(enc),
            mib / median(dec),
            frame.len() as f64 / pos as f64
        );
    }
}
