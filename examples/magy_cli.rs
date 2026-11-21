//! Minimal file-level driver for the frame codec, used by the
//! black-box interoperability matrix and for ad-hoc checks.
//!
//! ```text
//! magy_cli decode <in.magy> <out.raw>
//! magy_cli encode <FOURCC> <w> <h> <slice_height> <left|gradient|median|dynamic>
//!                 <huffman|raw|auto> <interlaced 0|1> <in.raw> <out.magy>
//! ```
//!
//! Raw files hold the planes back to back in the FOURCC's family order
//! (G,B,R[,A] / Y,U,V[,A] / Y), one byte per sample at 8-bit and one
//! little-endian 16-bit word per sample above.

use std::process::ExitCode;

use oxideav_magicyuv::tables::{PredictorKind, fourcc_table};
use oxideav_magicyuv::{
    EncodeOptions, PlaneInput, PredictorStrategy, Samples, SliceMode, decode_frame, encode_frame,
};

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("decode") if args.len() == 3 => {
            let bytes = std::fs::read(&args[1]).map_err(|e| e.to_string())?;
            let frame = decode_frame(&bytes).map_err(|e| format!("decode: {e}"))?;
            let mut out = Vec::new();
            for p in &frame.planes {
                match &p.samples {
                    Samples::U8(v) => out.extend_from_slice(v),
                    Samples::U16(v) => out.extend(v.iter().flat_map(|s| s.to_le_bytes())),
                }
            }
            println!(
                "fourcc={} width={} height={} planes={} depth={} interlaced={} slice_height={}",
                String::from_utf8_lossy(&frame.record.fourcc),
                frame.width,
                frame.height,
                frame.planes.len(),
                frame.record.bit_depth,
                frame.header.is_interlaced(),
                frame.header.slice_height,
            );
            std::fs::write(&args[2], out).map_err(|e| e.to_string())
        }
        Some("encode") if args.len() == 10 => {
            let rec = fourcc_table()
                .iter()
                .copied()
                .find(|r| r.fourcc == args[1].as_bytes())
                .ok_or("unknown FOURCC")?;
            let w: u32 = args[2].parse().map_err(|_| "width")?;
            let h: u32 = args[3].parse().map_err(|_| "height")?;
            let sh: u32 = args[4].parse().map_err(|_| "slice_height")?;
            let strategy = match args[5].as_str() {
                "left" => PredictorStrategy::Fixed(PredictorKind::Left),
                "gradient" => PredictorStrategy::Fixed(PredictorKind::Gradient),
                "median" => PredictorStrategy::Fixed(PredictorKind::Median),
                "dynamic" => PredictorStrategy::Dynamic,
                _ => return Err("predictor".into()),
            };
            let mode = match args[6].as_str() {
                "huffman" => SliceMode::Huffman,
                "raw" => SliceMode::Raw,
                "auto" => SliceMode::Auto,
                _ => return Err("slice mode".into()),
            };
            let interlaced = args[7] == "1";
            let raw = std::fs::read(&args[8]).map_err(|e| e.to_string())?;
            let mut planes = Vec::new();
            let mut pos = 0usize;
            for i in 0..rec.planes as usize {
                let chroma = i == 1 || i == 2;
                let (pw, ph) = if chroma {
                    (w.div_ceil(rec.sub_x as u32), h.div_ceil(rec.sub_y as u32))
                } else {
                    (w, h)
                };
                let n = pw as usize * ph as usize;
                if rec.is_high_bit_depth() {
                    let b = raw.get(pos..pos + 2 * n).ok_or("raw input too short")?;
                    pos += 2 * n;
                    let (words, _) = b.as_chunks::<2>();
                    planes.push(PlaneInput::U16(
                        words.iter().map(|c| u16::from_le_bytes(*c)).collect(),
                    ));
                } else {
                    let b = raw.get(pos..pos + n).ok_or("raw input too short")?;
                    pos += n;
                    planes.push(PlaneInput::U8(b.to_vec()));
                }
            }
            let opts = EncodeOptions::default()
                .with_strategy(strategy)
                .with_mode(mode)
                .with_interlaced(interlaced);
            let bytes =
                encode_frame(rec, w, h, sh, planes, opts).map_err(|e| format!("encode: {e}"))?;
            std::fs::write(&args[9], bytes).map_err(|e| e.to_string())
        }
        _ => Err("usage: magy_cli decode <in> <out> | encode <FOURCC> <w> <h> <slice_height> <pred> <mode> <interlaced> <in> <out>".into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("magy_cli: {e}");
            ExitCode::FAILURE
        }
    }
}
