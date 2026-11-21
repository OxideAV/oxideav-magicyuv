//! Live interoperability matrix against a black-box MagicYUV encoder
//! and decoder (the `ffmpeg` executable, used for its output only).
//!
//! - **Foreign → ours**: the black-box encoder writes every pixel
//!   format it accepts × every predictor × slice counts 1…64 ×
//!   tiny / odd / large sizes × several kinds of content; each frame
//!   must decode here to exactly the source samples, and the black-box
//!   decoder must agree.
//! - **Ours → foreign**: this encoder writes every native FOURCC ×
//!   predictor strategy × slice mode × slice height × interlacing; the
//!   black-box decoder must accept every frame silently and return
//!   exactly the source samples.
//!
//! Everything is generated in a temporary directory. The tests skip at
//! runtime (and say so) when the executable is missing or lacks the
//! codec.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_magicyuv::tables::{FourccRecord, PredictorKind, fourcc_table};
use oxideav_magicyuv::{
    EncodeOptions, Error, PlaneInput, PredictorStrategy, Samples, SliceMode, decode_frame,
    encode_frame,
};

const TOOL: &str = "ffmpeg";

/// `true` when the black-box tool runs and has both codec directions.
fn tool_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if cfg!(miri) {
            return false;
        }
        let has = |kind: &str| {
            Command::new(TOOL)
                .args(["-hide_banner", "-h", &format!("{kind}=magicyuv")])
                .output()
                .map(|o| {
                    o.status.success() && String::from_utf8_lossy(&o.stdout).contains("magicyuv [")
                })
                .unwrap_or(false)
        };
        has("encoder") && has("decoder")
    })
}

fn skip(test: &str) -> bool {
    if tool_available() {
        return false;
    }
    eprintln!("{test}: skipped — `{TOOL}` with a MagicYUV encoder + decoder is not available");
    true
}

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "oxideav-magicyuv-matrix-{}-{}-{tag}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// xorshift64* stream.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Content {
    /// Uniform noise: Huffman cannot win, raw slices where allowed.
    Noise,
    /// One value per plane: a single used symbol.
    Flat,
    /// All zero.
    Zero,
    /// Two values at random: two used symbols.
    TwoValue,
    /// A ramp with ±2 noise: the photographic-like case.
    Smooth,
    /// A noiseless diagonal ramp.
    Ramp,
}

const CONTENTS: [Content; 6] = [
    Content::Noise,
    Content::Flat,
    Content::Zero,
    Content::TwoValue,
    Content::Smooth,
    Content::Ramp,
];

fn record(fourcc: &str) -> FourccRecord {
    fourcc_table()
        .iter()
        .copied()
        .find(|r| r.fourcc == fourcc.as_bytes())
        .expect("native FOURCC")
}

fn plane_dims(rec: FourccRecord, plane: usize, w: usize, h: usize) -> (usize, usize) {
    if (plane == 1 || plane == 2) && (rec.sub_x > 1 || rec.sub_y > 1) {
        (
            w.div_ceil(rec.sub_x as usize),
            h.div_ceil(rec.sub_y as usize),
        )
    } else {
        (w, h)
    }
}

/// One frame's planes as sample values (masked to the bit depth).
fn make_planes(
    rec: FourccRecord,
    w: usize,
    h: usize,
    content: Content,
    seed: u64,
) -> Vec<Vec<u16>> {
    let mask = rec.sample_mask();
    let scale = (mask + 1) / 256;
    let mut rng = Rng::new(seed);
    (0..rec.planes as usize)
        .map(|p| {
            let (pw, ph) = plane_dims(rec, p, w, h);
            let mut out = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                for x in 0..pw {
                    let v = match content {
                        Content::Noise => rng.next(),
                        Content::Flat => (37 * p as u32 + 128) * scale,
                        Content::Zero => 0,
                        Content::TwoValue => {
                            if rng.next() & 1 == 0 {
                                10 * scale
                            } else {
                                200 * scale + 3
                            }
                        }
                        Content::Smooth => {
                            ((2 * x + y + 40 * p) as u32 * scale).wrapping_add(rng.next() % 5)
                        }
                        Content::Ramp => (3 * x + 5 * y + 40 * p) as u32 * scale / 2,
                    };
                    out.push((v & mask) as u16);
                }
            }
            out
        })
        .collect()
}

/// Planes back to back: one byte per sample at 8-bit, LE words above.
fn serialise(rec: FourccRecord, planes: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in planes {
        if rec.is_high_bit_depth() {
            out.extend(p.iter().flat_map(|s| s.to_le_bytes()));
        } else {
            out.extend(p.iter().map(|&s| s as u8));
        }
    }
    out
}

fn plane_inputs(rec: FourccRecord, planes: &[Vec<u16>]) -> Vec<PlaneInput> {
    planes
        .iter()
        .map(|p| {
            if rec.is_high_bit_depth() {
                PlaneInput::U16(p.clone())
            } else {
                PlaneInput::U8(p.iter().map(|&s| s as u8).collect())
            }
        })
        .collect()
}

fn our_decode(frame: &[u8]) -> Result<Vec<u8>, String> {
    let f = decode_frame(frame).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for p in &f.planes {
        match &p.samples {
            Samples::U8(v) => out.extend_from_slice(v),
            Samples::U16(v) => out.extend(v.iter().flat_map(|s| s.to_le_bytes())),
        }
    }
    Ok(out)
}

/// Run the tool, returning its stderr; panics when it fails.
fn run(args: &[&str], what: &str) -> String {
    let out = Command::new(TOOL)
        .args(["-hide_banner", "-nostdin", "-loglevel", "warning", "-y"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{what}: cannot run `{TOOL}`: {e}"));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{what}: `{TOOL}` failed: {stderr}");
    stderr
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 temp path")
}

/// Black-box decode of the numbered frame files `<stem>%d.magy`
/// (`count` of them from 0) into one raw file; returns its bytes and
/// the tool's stderr.
fn blackbox_decode(dir: &Scratch, stem: &str, what: &str) -> (Vec<u8>, String) {
    let pattern = dir.path(&format!("{stem}%d.magy"));
    let out = dir.path(&format!("{stem}.out"));
    let stderr = run(
        &[
            "-f",
            "image2",
            "-start_number",
            "0",
            "-c:v",
            "magicyuv",
            "-i",
            s(&pattern),
            "-f",
            "rawvideo",
            s(&out),
        ],
        what,
    );
    (std::fs::read(&out).expect("black-box output"), stderr)
}

/// Unoptimised builds run a reduced matrix (sizes up to 17×33, three
/// slice counts, two slice heights, two predictor strategies) so a
/// plain `cargo test` stays quick; `cargo test --release` runs all of
/// it.
const REDUCED: bool = cfg!(debug_assertions);

fn thinned(w: usize, h: usize) -> bool {
    REDUCED && w * h > 1_200
}

// ───────────────────────── foreign → ours ─────────────────────────

const SIZES: [(usize, usize); 13] = [
    (1, 1),
    (2, 2),
    (1, 8),
    (8, 1),
    (3, 5),
    (7, 9),
    (16, 16),
    (17, 33),
    (64, 48),
    (65, 49),
    (130, 66),
    (320, 240),
    (641, 361),
];

/// Slice counts asked of the black-box encoder. It writes at most 61
/// slices per plane for these sizes; past 255 slices in a frame its own
/// decoder refuses its output, so the sweep stops short of that.
const SLICE_COUNTS: [u32; 9] = [1, 2, 3, 4, 5, 8, 16, 33, 64];

/// Statistics of one foreign → ours sweep.
#[derive(Default)]
struct Tally {
    frames: usize,
    /// Bytes the black-box encoder needed, best predictor per frame.
    foreign_bytes: u64,
    /// Bytes this encoder needed for the same frames and slice height.
    our_bytes: u64,
}

/// Sweep one black-box input pixel format.
fn foreign_to_ours(pix_fmt: &str, fourcc: &str) -> Tally {
    let rec = record(fourcc);
    let dir = Scratch::new(pix_fmt);
    let mut tally = Tally::default();
    for (w, h) in SIZES {
        if thinned(w, h) {
            continue;
        }
        let big = w * h > 100_000;
        let slice_counts: &[u32] = if REDUCED {
            &[1, 5, 33]
        } else if big {
            &[1, 5, 33, 64]
        } else {
            &SLICE_COUNTS
        };
        let mut expected: Vec<Vec<u8>> = Vec::new();
        for &slices in slice_counts {
            // Smallest foreign frame per content over the three predictors.
            let mut best = [u64::MAX; CONTENTS.len()];
            let mut slice_height = 0u32;
            for pred in 1..=3u32 {
                let seed = (w * 7919 + h * 104_729) as u64 * 64 + (slices * 4 + pred) as u64;
                let frames: Vec<Vec<Vec<u16>>> = CONTENTS
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| make_planes(rec, w, h, c, seed * 8 + i as u64))
                    .collect();
                let input = dir.path("in.raw");
                let mut raw = Vec::new();
                for f in &frames {
                    raw.extend(serialise(rec, f));
                }
                std::fs::write(&input, &raw).unwrap();
                let first = expected.len();
                let pattern = dir.path("f%d.magy");
                let what = format!("{pix_fmt} {w}x{h} pred={pred} slices={slices}");
                run(
                    &[
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        pix_fmt,
                        "-s",
                        &format!("{w}x{h}"),
                        "-i",
                        s(&input),
                        "-c:v",
                        "magicyuv",
                        "-pred",
                        &pred.to_string(),
                        "-slices",
                        &slices.to_string(),
                        "-f",
                        "image2",
                        "-start_number",
                        &first.to_string(),
                        s(&pattern),
                    ],
                    &what,
                );
                for (i, f) in frames.iter().enumerate() {
                    let want = serialise(rec, f);
                    let bytes = std::fs::read(dir.path(&format!("f{}.magy", first + i)))
                        .unwrap_or_else(|e| panic!("{what}: frame {i} missing: {e}"));
                    let got = our_decode(&bytes)
                        .unwrap_or_else(|e| panic!("{what} {:?}: decode failed: {e}", CONTENTS[i]));
                    assert!(
                        got == want,
                        "{what} {:?}: decoded samples differ",
                        CONTENTS[i]
                    );
                    best[i] = best[i].min(bytes.len() as u64);
                    slice_height = u32::from_le_bytes(bytes[28..32].try_into().unwrap());
                    expected.push(want);
                    tally.frames += 1;
                }
                if pred == 3 {
                    // This encoder, same slice height, adaptive settings.
                    for (i, f) in frames.iter().enumerate() {
                        let ours = encode_frame(
                            rec,
                            w as u32,
                            h as u32,
                            slice_height,
                            plane_inputs(rec, f),
                            EncodeOptions::dynamic_auto(),
                        )
                        .unwrap_or_else(|e| panic!("{what}: our encode failed: {e}"));
                        tally.our_bytes += ours.len() as u64;
                        tally.foreign_bytes += best[i];
                    }
                }
            }
        }
        // The black-box decoder agrees on every frame of this size.
        let what = format!("{pix_fmt} {w}x{h} black-box decode");
        let (out, _) = blackbox_decode(&dir, "f", &what);
        assert!(out == expected.concat(), "{what}: differs from the source");
        for i in 0..expected.len() {
            let _ = std::fs::remove_file(dir.path(&format!("f{i}.magy")));
        }
    }
    eprintln!(
        "foreign→ours {pix_fmt}: {} frames bit-exact; adaptive encode {} B vs black-box best-of-3 {} B ({:.1} %)",
        tally.frames,
        tally.our_bytes,
        tally.foreign_bytes,
        100.0 * tally.our_bytes as f64 / tally.foreign_bytes as f64
    );
    tally
}

macro_rules! foreign_test {
    ($name:ident, $pix:literal, $fourcc:literal) => {
        #[test]
        fn $name() {
            if skip(stringify!($name)) {
                return;
            }
            let tally = foreign_to_ours($pix, $fourcc);
            let expected = if REDUCED { 432 } else { 2016 };
            assert_eq!(tally.frames, expected);
            assert!(
                tally.our_bytes <= tally.foreign_bytes,
                "adaptive encode is larger than the black-box encoder's best"
            );
        }
    };
}

foreign_test!(foreign_gbrp, "gbrp", "M8RG");
foreign_test!(foreign_gbrap, "gbrap", "M8RA");
foreign_test!(foreign_yuv444p, "yuv444p", "M8Y4");
foreign_test!(foreign_yuv422p, "yuv422p", "M8Y2");
foreign_test!(foreign_yuv420p, "yuv420p", "M8Y0");
foreign_test!(foreign_yuva444p, "yuva444p", "M8YA");
foreign_test!(foreign_gray, "gray", "M8G0");

// ───────────────────────── ours → foreign ─────────────────────────

const OUR_SIZES: [(usize, usize); 8] = [
    (1, 1),
    (2, 2),
    (3, 5),
    (16, 16),
    (17, 33),
    (64, 48),
    (130, 66),
    (321, 243),
];

const STRATEGIES: [PredictorStrategy; 4] = [
    PredictorStrategy::Fixed(PredictorKind::Left),
    PredictorStrategy::Fixed(PredictorKind::Gradient),
    PredictorStrategy::Fixed(PredictorKind::Median),
    PredictorStrategy::Dynamic,
];

const MODES: [SliceMode; 3] = [SliceMode::Huffman, SliceMode::Raw, SliceMode::Auto];

/// Sweep this encoder's options for one FOURCC; returns the frame count.
fn ours_to_foreign(fourcc: &str) -> usize {
    let rec = record(fourcc);
    let dir = Scratch::new(fourcc);
    let mut total = 0usize;
    let mut refused = 0usize;
    let mut pick = 0usize;
    for (w, h) in OUR_SIZES {
        if thinned(w, h) {
            continue;
        }
        let mut expected: Vec<u8> = Vec::new();
        let mut count = 0usize;
        // 1-row slices, odd heights, the vendor default 28, one slice.
        for slice_height in [1u32, 2, 4, 7, 16, 28, 1024] {
            if rec.sub_y > 1 && !slice_height.is_multiple_of(rec.sub_y as u32) {
                continue;
            }
            if REDUCED && slice_height != 2 && slice_height != 28 {
                continue;
            }
            for strategy in STRATEGIES {
                if REDUCED
                    && matches!(strategy, PredictorStrategy::Fixed(k) if k != PredictorKind::Left)
                {
                    continue;
                }
                for mode in MODES {
                    for interlaced in [false, true] {
                        let content = CONTENTS[pick % CONTENTS.len()];
                        pick += 1;
                        let planes = make_planes(rec, w, h, content, (pick * 31 + w * h) as u64);
                        let frame = match encode_frame(
                            rec,
                            w as u32,
                            h as u32,
                            slice_height,
                            plane_inputs(rec, &planes),
                            EncodeOptions::default()
                                .with_strategy(strategy)
                                .with_mode(mode)
                                .with_interlaced(interlaced),
                        ) {
                            Ok(frame) => frame,
                            // Too-short interlaced slices are not written
                            // (the black-box decoder refuses them).
                            Err(Error::InterlacedSliceTooShort { .. }) if interlaced => {
                                refused += 1;
                                continue;
                            }
                            Err(e) => panic!(
                                "{fourcc} {w}x{h} sh={slice_height} {strategy:?} {mode:?}: {e}"
                            ),
                        };
                        let want = serialise(rec, &planes);
                        assert!(our_decode(&frame).as_deref() == Ok(&want[..]));
                        std::fs::write(dir.path(&format!("o{count}.magy")), &frame).unwrap();
                        expected.extend(want);
                        count += 1;
                    }
                }
            }
        }
        let what = format!("{fourcc} {w}x{h}: black-box decode of {count} frames");
        if count == 0 {
            continue;
        }
        let (out, stderr) = blackbox_decode(&dir, "o", &what);
        // Only the decoder's own diagnostics count (the raw-video
        // output stage complains about tiny frame sizes on its own).
        let complaints: Vec<&str> = stderr.lines().filter(|l| l.contains("magicyuv")).collect();
        assert!(complaints.is_empty(), "{what}: warnings: {complaints:?}");
        assert!(out == expected, "{what}: differs from the source");
        for i in 0..count {
            let _ = std::fs::remove_file(dir.path(&format!("o{i}.magy")));
        }
        total += count;
    }
    eprintln!(
        "ours→foreign {fourcc}: {total} frames accepted bit-exact, no warnings ({refused} too-short interlaced slice geometries not written)"
    );
    total
}

macro_rules! ours_test {
    ($name:ident, [$($fourcc:literal),+]) => {
        #[test]
        fn $name() {
            if skip(stringify!($name)) {
                return;
            }
            $( ours_to_foreign($fourcc); )+
        }
    };
}

ours_test!(ours_rgb_8bit, ["M8RG", "M8RA"]);
ours_test!(ours_yuv_8bit, ["M8Y4", "M8Y2", "M8Y0", "M8YA", "M8G0"]);
ours_test!(
    ours_rgb_high_depth,
    ["M0RG", "M0RA", "M2RG", "M2RA", "M4RG", "M4RA"]
);
ours_test!(ours_yuv_10bit, ["M0Y4", "M0Y2", "M0Y0", "M0G0"]);
