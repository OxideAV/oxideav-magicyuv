//! End-to-end interop test: drive FFmpeg as a black-box encoder, then
//! decode the resulting MagicYUV packet with this crate, and assert
//! bit-exact pixel match against FFmpeg's own raw decode.
//!
//! Skipped (with a `println!`) if `ffmpeg` is not on PATH.

use std::process::{Command, Stdio};

/// Look for the **second** `MAGY` magic in `bytes` (the first lives in
/// the AVI `strf` extradata as a 32-byte header-only packet; the second
/// is the actual frame in the `movi` chunk). Returns the slice from
/// that magic to the start of the `idx1` AVI index chunk (or to EOF if
/// no idx1 marker is present). Matches the trace document's extraction
/// procedure (§1).
fn extract_magy_packet(bytes: &[u8]) -> Option<&[u8]> {
    let mut last_magy: Option<usize> = None;
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"MAGY" {
            last_magy = Some(i);
        }
        i += 1;
    }
    let start = last_magy?;
    // Find the `idx1` four-byte tag after `start`.
    let mut end = bytes.len();
    let mut j = start + 4;
    while j + 4 <= bytes.len() {
        if &bytes[j..j + 4] == b"idx1" {
            end = j;
            break;
        }
        j += 1;
    }
    Some(&bytes[start..end])
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("ffmpeg invocation failed (binary missing?)");
    if !out.status.success() {
        panic!(
            "ffmpeg exited {:?}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.stdout
}

fn encode_magy(width: u32, height: u32, pix_fmt: &str, predictor: &str) -> Vec<u8> {
    encode_magy_slices(width, height, pix_fmt, predictor, None)
}

/// Same as [`encode_magy`] but optionally requests `slices` (FFmpeg's
/// `-slices N`) — useful for forcing multi-slice frames at sizes too
/// small to trigger the encoder's automatic slice subdivision.
fn encode_magy_slices(
    width: u32,
    height: u32,
    pix_fmt: &str,
    predictor: &str,
    slices: Option<u32>,
) -> Vec<u8> {
    let testsrc = format!("testsrc=size={width}x{height}:duration=1:rate=1");
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        testsrc,
        "-frames:v".into(),
        "1".into(),
        "-pix_fmt".into(),
        pix_fmt.into(),
        "-c:v".into(),
        "magicyuv".into(),
        "-pred".into(),
        predictor.into(),
    ];
    if let Some(n) = slices {
        args.push("-slices".into());
        args.push(n.to_string());
    }
    args.push("-f".into());
    args.push("avi".into());
    args.push("-".into());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ffmpeg(&arg_refs)
}

fn decode_raw(avi: &[u8], pix_fmt: &str) -> Vec<u8> {
    // Pipe the AVI back into ffmpeg via stdin and grab the raw plane bytes.
    use std::io::Write;
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-pix_fmt",
            pix_fmt,
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ffmpeg spawn for decode");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(avi).expect("write avi");
    }
    let out = child.wait_with_output().expect("ffmpeg wait");
    if !out.status.success() {
        panic!(
            "ffmpeg decode exited {:?}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.stdout
}

/// Concatenate plane buffers in (Y, U, V) order from a `VideoFrame` into
/// a single `Vec<u8>` matching what `ffmpeg -f rawvideo` produces.
fn frame_planes_concat(frame: &oxideav_core::VideoFrame) -> Vec<u8> {
    let mut out = Vec::new();
    for p in &frame.planes {
        out.extend_from_slice(&p.data);
    }
    out
}

fn assert_bit_exact(test_name: &str, ours: &[u8], theirs: &[u8]) {
    if ours == theirs {
        return;
    }
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{test_name}: length mismatch — ours={}, theirs={}",
        ours.len(),
        theirs.len()
    );
    let mut first_diff = None;
    let mut ndiff = 0usize;
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        if a != b {
            if first_diff.is_none() {
                first_diff = Some((i, *a, *b));
            }
            ndiff += 1;
        }
    }
    panic!(
        "{test_name}: {ndiff} of {} bytes differ; first divergence at offset {:?}",
        ours.len(),
        first_diff
    );
}

fn try_decode_test(pix_fmt: &str, predictor: &str, width: u32, height: u32, label: &str) {
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let avi = encode_magy(width, height, pix_fmt, predictor);
    let theirs = decode_raw(&avi, pix_fmt);

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a real magicyuv packet");
    let ours = frame_planes_concat(&frame);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn yuv422p_left_64x48() {
    try_decode_test("yuv422p", "left", 64, 48, "yuv422p/left/64x48");
}

#[test]
fn yuv422p_gradient_64x48() {
    try_decode_test("yuv422p", "gradient", 64, 48, "yuv422p/gradient/64x48");
}

#[test]
fn yuv422p_median_64x48() {
    try_decode_test("yuv422p", "median", 64, 48, "yuv422p/median/64x48");
}

#[test]
fn yuv420p_left_64x48() {
    try_decode_test("yuv420p", "left", 64, 48, "yuv420p/left/64x48");
}

#[test]
fn yuv420p_median_64x48() {
    try_decode_test("yuv420p", "median", 64, 48, "yuv420p/median/64x48");
}

#[test]
fn yuv444p_gradient_64x48() {
    try_decode_test("yuv444p", "gradient", 64, 48, "yuv444p/gradient/64x48");
}

#[test]
fn gray8_left_64x48() {
    try_decode_test("gray", "left", 64, 48, "gray8/left/64x48");
}

#[test]
fn yuv422p_median_320x240() {
    try_decode_test("yuv422p", "median", 320, 240, "yuv422p/median/320x240");
}

/// GBRP encoded by FFmpeg → decoded by us → packed `Rgb24` plane in
/// `R, G, B` byte order. We compare to FFmpeg's `rgb24` rawvideo dump
/// so the planar→packed unswizzling is exercised as well.
fn try_decode_rgb_test(predictor: &str, width: u32, height: u32, label: &str) {
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    // Encode at gbrp (the only RGB pixel format the FFmpeg magicyuv
    // encoder accepts), but ask FFmpeg to deliver the reference raw
    // as rgb24 — that's what our decoder emits for `M8RG` packets
    // (we don't expose a planar-GBR pixel format from `oxideav-core`).
    let avi = encode_magy(width, height, "gbrp", predictor);
    let theirs = decode_raw(&avi, "rgb24");

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a real magicyuv packet");
    assert_eq!(
        frame.planes.len(),
        1,
        "{label}: GBRP decode should emit one packed RGB plane"
    );
    let ours = &frame.planes[0].data;
    assert_bit_exact(label, ours, &theirs);
}

#[test]
fn gbrp_left_64x48() {
    try_decode_rgb_test("left", 64, 48, "gbrp/left/64x48");
}

#[test]
fn gbrp_gradient_64x48() {
    try_decode_rgb_test("gradient", 64, 48, "gbrp/gradient/64x48");
}

#[test]
fn gbrp_median_64x48() {
    try_decode_rgb_test("median", 64, 48, "gbrp/median/64x48");
}

#[test]
fn yuv420p_median_320x240_multi_slice() {
    // 320×240 with 2 slices is the canonical multi-slice fixture from
    // the trace document's per-corpus table (§6).
    try_decode_test(
        "yuv420p",
        "median",
        320,
        240,
        "yuv420p/median/320x240 (multi-slice)",
    );
}

/// GBRAP (`M8RA`, format 0x66) — 4-plane, RGB-decorrelated. Decoder
/// emits packed `Rgba` (one VideoPlane). Reference is FFmpeg's `rgba`
/// rawvideo dump so we exercise the decorrelation + RGB swizzle on
/// the alpha-bearing path.
fn try_decode_gbrap_test(predictor: &str, width: u32, height: u32, label: &str) {
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let avi = encode_magy(width, height, "gbrap", predictor);
    let theirs = decode_raw(&avi, "rgba");

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a real M8RA packet");
    assert_eq!(
        frame.planes.len(),
        1,
        "{label}: GBRAP decode should emit one packed RGBA plane"
    );
    let ours = &frame.planes[0].data;
    assert_bit_exact(label, ours, &theirs);
}

#[test]
fn gbrap_left_64x48() {
    try_decode_gbrap_test("left", 64, 48, "gbrap/left/64x48");
}

#[test]
fn gbrap_gradient_64x48() {
    try_decode_gbrap_test("gradient", 64, 48, "gbrap/gradient/64x48");
}

#[test]
fn gbrap_median_64x48() {
    try_decode_gbrap_test("median", 64, 48, "gbrap/median/64x48");
}

/// YUVA 4:4:4 (`M8YA`, format 0x6a). Wire layout is Y/U/V/A planar
/// (4 planes, no chroma subsampling, no RGB decorrelation). The
/// decoder emits the four planes as-is in (Y, U, V, A) order and the
/// PixelFormat falls back to `Yuv444P` (no native YUVA in
/// `oxideav-core`). We compare the concatenated planes to FFmpeg's
/// `yuva444p` rawvideo dump.
fn try_decode_yuva444p_test(predictor: &str, width: u32, height: u32, label: &str) {
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let avi = encode_magy(width, height, "yuva444p", predictor);
    let theirs = decode_raw(&avi, "yuva444p");

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a real M8YA packet");
    assert_eq!(
        frame.planes.len(),
        4,
        "{label}: YUVA444P decode should emit four planes (Y, U, V, A)"
    );
    let ours = frame_planes_concat(&frame);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn yuva444p_left_64x48() {
    try_decode_yuva444p_test("left", 64, 48, "yuva444p/left/64x48");
}

#[test]
fn yuva444p_gradient_64x48() {
    try_decode_yuva444p_test("gradient", 64, 48, "yuva444p/gradient/64x48");
}

#[test]
fn yuva444p_median_64x48() {
    try_decode_yuva444p_test("median", 64, 48, "yuva444p/median/64x48");
}

/// Force a multi-slice frame at a small size by passing FFmpeg's
/// `-slices 4`. This exercises the per-plane Huffman-shared,
/// per-slice-prefix-pair decode path on a 4-slice frame whose
/// `slice_height` is 12 luma rows (= 6 chroma rows for 4:2:0). The
/// canonical 320×240 fixture (above) only has 2 slices.
#[test]
fn yuv422p_median_64x48_4slices() {
    let label = "yuv422p/median/64x48 -slices 4";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let avi = encode_magy_slices(64, 48, "yuv422p", "median", Some(4));
    let theirs = decode_raw(&avi, "yuv422p");

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a forced multi-slice packet");
    let ours = frame_planes_concat(&frame);
    assert_bit_exact(label, &ours, &theirs);
}

/// Same as above but on the GBRP path, which additionally exercises
/// the per-slice predictor + multi-slice + post-decorrelation pass.
#[test]
fn gbrp_left_64x48_4slices() {
    let label = "gbrp/left/64x48 -slices 4";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let avi = encode_magy_slices(64, 48, "gbrp", "left", Some(4));
    let theirs = decode_raw(&avi, "rgb24");

    let packet = extract_magy_packet(&avi).expect("MAGY packet not found in AVI");
    let frame = oxideav_magicyuv::decoder::decode_packet(packet, None)
        .expect("our decoder returned an error on a forced multi-slice GBRP packet");
    assert_eq!(frame.planes.len(), 1);
    let ours = &frame.planes[0].data;
    assert_bit_exact(label, ours, &theirs);
}
