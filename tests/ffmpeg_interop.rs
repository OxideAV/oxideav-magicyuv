//! End-to-end interop test: drive FFmpeg as a black-box encoder, then
//! decode the resulting MagicYUV packet with this crate, and assert
//! bit-exact pixel match against FFmpeg's own raw decode.
//!
//! Skipped (with a `println!`) if `ffmpeg` is not on PATH.

// Pixel-by-pixel index loops are idiomatic for the test-fixture builders.
#![allow(clippy::needless_range_loop)]

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

// ─────────────────────── encoder ↔ ffmpeg cross tests ───────────────────────
//
// Produce a packet from our encoder, wrap it in a minimal AVI container,
// then ask ffmpeg's `magicyuv` decoder to read it back as raw planes.
// The two outputs must match bit-for-bit. This validates that our
// encoder writes a wire format ffmpeg understands.

/// Build a minimal AVI containing one MagicYUV keyframe. The
/// generated container has only the chunks the FFmpeg AVI demuxer
/// needs to identify the codec, size, and locate the single frame:
///
/// - `RIFF` ('AVI ') header
/// - `LIST hdrl` ( `avih` + `LIST strl` ( `strh` 'vids' + `strf` ) )
/// - `LIST movi` ( one `00db` chunk holding the `MAGY` packet )
/// - `idx1` index referencing the chunk
///
/// `extradata` is the 32-byte file-header copy ffmpeg expects in
/// `strf` for AVI-muxed MagicYUV (see crate trace doc §1).
fn wrap_packet_in_avi(
    packet: &[u8],
    width: u32,
    height: u32,
    fourcc: &[u8; 4],
    extradata: &[u8],
) -> Vec<u8> {
    fn pad2(v: &mut Vec<u8>) {
        if v.len() % 2 != 0 {
            v.push(0);
        }
    }
    fn write_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(tag);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 != 0 {
            out.push(0);
        }
    }
    fn write_list(out: &mut Vec<u8>, list_type: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
        out.extend_from_slice(list_type);
        out.extend_from_slice(body);
        if body.len() % 2 != 0 {
            out.push(0);
        }
    }

    // ---- avih (56 bytes) ----
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&40000u32.to_le_bytes()); // micro_sec_per_frame (25 fps)
    avih.extend_from_slice(&0u32.to_le_bytes()); // max bytes/sec
    avih.extend_from_slice(&0u32.to_le_bytes()); // padding granularity
    avih.extend_from_slice(&0x10u32.to_le_bytes()); // flags = AVIF_HASINDEX
    avih.extend_from_slice(&1u32.to_le_bytes()); // total frames
    avih.extend_from_slice(&0u32.to_le_bytes()); // initial frames
    avih.extend_from_slice(&1u32.to_le_bytes()); // streams
    avih.extend_from_slice(&0u32.to_le_bytes()); // suggested buffer size
    avih.extend_from_slice(&width.to_le_bytes()); // width
    avih.extend_from_slice(&height.to_le_bytes()); // height
    avih.extend_from_slice(&0u32.to_le_bytes()); // reserved 1
    avih.extend_from_slice(&0u32.to_le_bytes()); // reserved 2
    avih.extend_from_slice(&0u32.to_le_bytes()); // reserved 3
    avih.extend_from_slice(&0u32.to_le_bytes()); // reserved 4
    debug_assert_eq!(avih.len(), 56);

    // ---- strh (56 bytes, AVIStreamHeader) ----
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(fourcc); // handler / fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // flags
    strh.extend_from_slice(&0u16.to_le_bytes()); // priority
    strh.extend_from_slice(&0u16.to_le_bytes()); // language
    strh.extend_from_slice(&0u32.to_le_bytes()); // initial frames
    strh.extend_from_slice(&1u32.to_le_bytes()); // scale
    strh.extend_from_slice(&25u32.to_le_bytes()); // rate (25 fps)
    strh.extend_from_slice(&0u32.to_le_bytes()); // start
    strh.extend_from_slice(&1u32.to_le_bytes()); // length (frames)
    strh.extend_from_slice(&0u32.to_le_bytes()); // suggested buffer size
    strh.extend_from_slice(&((-1i32) as u32).to_le_bytes()); // quality
    strh.extend_from_slice(&0u32.to_le_bytes()); // sample size
                                                 // rcFrame (4 i16: left, top, right, bottom)
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&0i16.to_le_bytes());
    strh.extend_from_slice(&(width as i16).to_le_bytes());
    strh.extend_from_slice(&(height as i16).to_le_bytes());
    debug_assert_eq!(strh.len(), 56);

    // ---- strf: BITMAPINFOHEADER + extradata ----
    let mut strf = Vec::with_capacity(40 + extradata.len());
    let bmi_size = 40u32 + extradata.len() as u32;
    strf.extend_from_slice(&bmi_size.to_le_bytes()); // size
    strf.extend_from_slice(&width.to_le_bytes()); // width
    strf.extend_from_slice(&height.to_le_bytes()); // height
    strf.extend_from_slice(&1u16.to_le_bytes()); // planes
    strf.extend_from_slice(&24u16.to_le_bytes()); // bit_count (informational)
    strf.extend_from_slice(fourcc); // compression
    strf.extend_from_slice(&(width * height * 3).to_le_bytes()); // size_image
    strf.extend_from_slice(&0u32.to_le_bytes()); // x pels per meter
    strf.extend_from_slice(&0u32.to_le_bytes()); // y pels per meter
    strf.extend_from_slice(&0u32.to_le_bytes()); // colours used
    strf.extend_from_slice(&0u32.to_le_bytes()); // colours important
    strf.extend_from_slice(extradata);

    // ---- LIST strl ( strh + strf ) ----
    let mut strl_body = Vec::new();
    write_chunk(&mut strl_body, b"strh", &strh);
    write_chunk(&mut strl_body, b"strf", &strf);

    // ---- LIST hdrl ( avih + LIST strl ) ----
    let mut hdrl_body = Vec::new();
    write_chunk(&mut hdrl_body, b"avih", &avih);
    write_list(&mut hdrl_body, b"strl", &strl_body);

    // ---- LIST movi ( 00db chunk ) ----
    let mut movi_body = Vec::new();
    write_chunk(&mut movi_body, b"00db", packet);

    // ---- idx1 — one entry pointing at the 00db chunk ----
    // Offset is relative to start of `movi` payload (just after the
    // `LIST <size> movi` 12-byte preamble): so the `00db` chunk header
    // sits at offset 4 bytes into movi_body? No — the AVI convention
    // is that the offset is from the start of the `LIST` four bytes
    // OR the start of `movi`. Most muxers put it relative to the
    // `movi` four-byte tag (so first chunk is at offset 4). FFmpeg's
    // demuxer accepts both with `oavi_idx_offset` heuristic; pick
    // 4 for clarity.
    let mut idx1 = Vec::new();
    idx1.extend_from_slice(b"00db");
    idx1.extend_from_slice(&0x10u32.to_le_bytes()); // flags = AVIIF_KEYFRAME
    idx1.extend_from_slice(&4u32.to_le_bytes()); // offset (rel to `movi` tag)
    idx1.extend_from_slice(&(packet.len() as u32).to_le_bytes()); // size

    // ---- RIFF body ----
    let mut riff_body: Vec<u8> = Vec::new();
    riff_body.extend_from_slice(b"AVI ");
    write_list(&mut riff_body, b"hdrl", &hdrl_body);
    write_list(&mut riff_body, b"movi", &movi_body);
    write_chunk(&mut riff_body, b"idx1", &idx1);
    pad2(&mut riff_body);

    // ---- RIFF wrapper ----
    let mut out = Vec::with_capacity(8 + riff_body.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
    out.extend_from_slice(&riff_body);
    out
}

/// Extradata for AVI-muxed MagicYUV: ffmpeg expects the 32-byte
/// file header copy in `strf`. Use the first 32 bytes of our packet
/// (which IS the file header).
fn extradata_for_packet(packet: &[u8]) -> &[u8] {
    &packet[..32]
}

fn fourcc_for_format(format: oxideav_magicyuv::header::FormatCode) -> &'static [u8; 4] {
    use oxideav_magicyuv::header::FormatCode::*;
    match format {
        Gbrp => b"M8RG",
        Gbrap => b"M8RA",
        Yuv444P => b"M8Y4",
        Yuv422P => b"M8Y2",
        Yuv420P => b"M8Y0",
        Yuva444P => b"M8YA",
        Gray8 => b"M8G0",
        Yuv422P10 => b"M0Y2",
        Gray10 => b"M0G0",
        Yuv444P10 => b"M0Y4",
        Yuv420P10 => b"M0Y0",
    }
}

/// Encode `frame` with our encoder, wrap in AVI, decode via ffmpeg's
/// `magicyuv` decoder, return the raw plane bytes ffmpeg produces.
#[allow(clippy::too_many_arguments)]
fn ffmpeg_decode_our_packet(
    format: oxideav_magicyuv::header::FormatCode,
    predictor: oxideav_magicyuv::predictor::Predictor,
    width: u32,
    height: u32,
    slice_width: u32,
    slice_height: u32,
    frame: &oxideav_core::VideoFrame,
    out_pix_fmt: &str,
) -> Vec<u8> {
    let pkt = oxideav_magicyuv::encoder::encode_frame(
        format,
        predictor,
        width,
        height,
        slice_width,
        slice_height,
        frame,
    )
    .expect("our encoder failed");
    let fourcc = fourcc_for_format(format);
    let extradata = extradata_for_packet(&pkt);
    let avi = wrap_packet_in_avi(&pkt, width, height, fourcc, extradata);
    decode_raw(&avi, out_pix_fmt)
}

fn build_yuv422p(w: u32, h: u32, seed: u32) -> oxideav_core::VideoFrame {
    use oxideav_core::frame::VideoPlane;
    let cw = (w / 2) as usize;
    let mut y = vec![0u8; (w * h) as usize];
    let mut u = vec![0u8; cw * h as usize];
    let mut v = vec![0u8; cw * h as usize];
    for i in 0..y.len() {
        y[i] = ((seed.wrapping_mul(31).wrapping_add(i as u32)) & 0xFF) as u8;
    }
    for i in 0..u.len() {
        u[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
        v[i] = ((seed.wrapping_mul(19).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
    }
    oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![
            VideoPlane {
                stride: w as usize,
                data: y,
            },
            VideoPlane {
                stride: cw,
                data: u,
            },
            VideoPlane {
                stride: cw,
                data: v,
            },
        ],
    }
}

fn build_yuv420p(w: u32, h: u32, seed: u32) -> oxideav_core::VideoFrame {
    use oxideav_core::frame::VideoPlane;
    let cw = (w / 2) as usize;
    let ch = (h / 2) as usize;
    let mut y = vec![0u8; (w * h) as usize];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for i in 0..y.len() {
        y[i] = ((seed.wrapping_mul(31).wrapping_add(i as u32 * 7)) & 0xFF) as u8;
    }
    for i in 0..u.len() {
        u[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
        v[i] = ((seed.wrapping_mul(19).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
    }
    oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![
            VideoPlane {
                stride: w as usize,
                data: y,
            },
            VideoPlane {
                stride: cw,
                data: u,
            },
            VideoPlane {
                stride: cw,
                data: v,
            },
        ],
    }
}

fn build_gray8(w: u32, h: u32, seed: u32) -> oxideav_core::VideoFrame {
    use oxideav_core::frame::VideoPlane;
    let mut data = vec![0u8; (w * h) as usize];
    for i in 0..data.len() {
        data[i] = ((seed.wrapping_mul(7).wrapping_add(i as u32 * 13)) & 0xFF) as u8;
    }
    oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![VideoPlane {
            stride: w as usize,
            data,
        }],
    }
}

fn frame_planes_concat_local(frame: &oxideav_core::VideoFrame) -> Vec<u8> {
    let mut out = Vec::new();
    for p in &frame.planes {
        out.extend_from_slice(&p.data);
    }
    out
}

#[test]
fn enc_yuv422p_left_64x48_ffmpeg_decodes() {
    let label = "enc yuv422p/left/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv422p(64, 48, 0xC0FFEE);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Left,
        64,
        48,
        64,
        48,
        &f,
        "yuv422p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_yuv422p_gradient_64x48_ffmpeg_decodes() {
    let label = "enc yuv422p/gradient/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv422p(64, 48, 0xBEEF);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Gradient,
        64,
        48,
        64,
        48,
        &f,
        "yuv422p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_yuv422p_median_64x48_ffmpeg_decodes() {
    let label = "enc yuv422p/median/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv422p(64, 48, 0xABCD);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Median,
        64,
        48,
        64,
        48,
        &f,
        "yuv422p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_yuv420p_median_64x48_ffmpeg_decodes() {
    let label = "enc yuv420p/median/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv420p(64, 48, 0x1234);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv420P,
        oxideav_magicyuv::predictor::Predictor::Median,
        64,
        48,
        64,
        48,
        &f,
        "yuv420p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_gray8_left_64x48_ffmpeg_decodes() {
    let label = "enc gray8/left/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_gray8(64, 48, 0x5A5A);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Gray8,
        oxideav_magicyuv::predictor::Predictor::Left,
        64,
        48,
        64,
        48,
        &f,
        "gray",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

/// Multi-slice encoded packet (vertical row bands) — ffmpeg cross-decode.
#[test]
fn enc_yuv422p_multi_slice_ffmpeg_decodes() {
    let label = "enc yuv422p/left/64x48 -slices 4 (vertical) → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv422p(64, 48, 0xCAFE);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Left,
        64,
        48,
        64,
        12,
        &f,
        "yuv422p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

fn build_yuv444p(w: u32, h: u32, seed: u32) -> oxideav_core::VideoFrame {
    use oxideav_core::frame::VideoPlane;
    let mut y = vec![0u8; (w * h) as usize];
    let mut u = vec![0u8; (w * h) as usize];
    let mut v = vec![0u8; (w * h) as usize];
    for i in 0..y.len() {
        y[i] = ((seed.wrapping_mul(11).wrapping_add(i as u32 * 7)) & 0xFF) as u8;
        u[i] = ((seed.wrapping_mul(13).wrapping_add(i as u32 * 5)) & 0xFF) as u8;
        v[i] = ((seed.wrapping_mul(17).wrapping_add(i as u32 * 3)) & 0xFF) as u8;
    }
    oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![
            VideoPlane {
                stride: w as usize,
                data: y,
            },
            VideoPlane {
                stride: w as usize,
                data: u,
            },
            VideoPlane {
                stride: w as usize,
                data: v,
            },
        ],
    }
}

fn build_rgb24(w: u32, h: u32, seed: u32) -> oxideav_core::VideoFrame {
    use oxideav_core::frame::VideoPlane;
    let n = (w * h) as usize;
    let mut data = vec![0u8; n * 3];
    for i in 0..n {
        let i32_ = i as u32;
        data[i * 3] = ((seed.wrapping_mul(11).wrapping_add(i32_ * 31)) & 0xFF) as u8;
        data[i * 3 + 1] = ((seed.wrapping_mul(13).wrapping_add(i32_ * 17)) & 0xFF) as u8;
        data[i * 3 + 2] = ((seed.wrapping_mul(17).wrapping_add(i32_ * 19)) & 0xFF) as u8;
    }
    oxideav_core::VideoFrame {
        pts: Some(0),
        planes: vec![VideoPlane {
            stride: (w * 3) as usize,
            data,
        }],
    }
}

#[test]
fn enc_yuv444p_left_64x48_ffmpeg_decodes() {
    let label = "enc yuv444p/left/64x48 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv444p(64, 48, 0xDEAD);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv444P,
        oxideav_magicyuv::predictor::Predictor::Left,
        64,
        48,
        64,
        48,
        &f,
        "yuv444p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_gbrp_left_64x48_ffmpeg_decodes() {
    let label = "enc gbrp/left/64x48 → ffmpeg decode (rgb24)";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_rgb24(64, 48, 0xBEEF);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Gbrp,
        oxideav_magicyuv::predictor::Predictor::Left,
        64,
        48,
        64,
        48,
        &f,
        "rgb24",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_gbrp_gradient_64x48_ffmpeg_decodes() {
    let label = "enc gbrp/gradient/64x48 → ffmpeg decode (rgb24)";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_rgb24(64, 48, 0xCAFE);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Gbrp,
        oxideav_magicyuv::predictor::Predictor::Gradient,
        64,
        48,
        64,
        48,
        &f,
        "rgb24",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

#[test]
fn enc_yuv422p_320x240_ffmpeg_decodes() {
    let label = "enc yuv422p/median/320x240 → ffmpeg decode";
    if !ffmpeg_available() {
        eprintln!("[SKIP] {label}: ffmpeg not on PATH");
        return;
    }
    let f = build_yuv422p(320, 240, 0x4242);
    let theirs = ffmpeg_decode_our_packet(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Median,
        320,
        240,
        320,
        240,
        &f,
        "yuv422p",
    );
    let ours = frame_planes_concat_local(&f);
    assert_bit_exact(label, &ours, &theirs);
}

/// Horizontally-tiled encoded packet — ffmpeg's reference decoder
/// rejects horizontal tilings (`AVERROR_PATCHWELCOME`), so we instead
/// validate the path through our own decoder. A separate
/// `decoder` test below exercises the same wire on the read side.
#[test]
fn enc_yuv422p_horizontal_tile_self_roundtrip() {
    // Self-roundtrip lives in `encoder::tests`; this test exercises
    // the public encoder + decoder API together at the package
    // boundary.
    let f = build_yuv422p(64, 48, 0xDEAD);
    let pkt = oxideav_magicyuv::encoder::encode_frame(
        oxideav_magicyuv::header::FormatCode::Yuv422P,
        oxideav_magicyuv::predictor::Predictor::Median,
        64,
        48,
        32,
        24,
        &f,
    )
    .expect("encode");
    let got = oxideav_magicyuv::decoder::decode_packet(&pkt, None).expect("decode");
    assert_eq!(
        frame_planes_concat_local(&got),
        frame_planes_concat_local(&f)
    );
}
