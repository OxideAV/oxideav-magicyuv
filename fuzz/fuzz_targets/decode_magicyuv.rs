#![no_main]

//! Decode arbitrary fuzz-supplied bytes through the full MagicYUV v7
//! decode chain: `header::parse` (the 32-byte frame header of
//! `spec/01` §3 — magic, version, format byte, dimensions, flags),
//! `decode_frame` (the `(N+1)`-entry u32 slice table of `spec/02` §5,
//! the per-slice preamble + per-plane canonical-Huffman length
//! descriptors of `spec/05` §2.0, the per-slice raw / Huffman payloads,
//! the Left / Gradient / Median predictor inverse — Median modular at
//! 8-bit and JPEG-LS at 10/12/14-bit — the optional interlaced
//! field-stride, and the RGB inter-plane decorrelation reversal).
//!
//! The contract under test is purely that every call *returns*: a
//! malformed stream yields `Err(magicyuv::Error::…)`, a well-formed one
//! yields `Ok(DecodedFrame)`, and neither path may panic, abort,
//! integer-overflow (in a debug/ASAN build), or index out of bounds —
//! regardless of how hostile the bytes are. The return value is
//! intentionally discarded (a round-trip oracle would need a trusted
//! encoder of the *same* arbitrary stream, which doesn't exist).
//!
//! # Two entry points: one-shot and streaming
//!
//! `decode_frame` is the allocating one-shot wrapper. The streaming
//! sibling `decode_into` re-uses a caller-owned [`DecodedFrame`]'s
//! per-plane `Vec` storage across many frames, only resizing in place
//! when the on-wire geometry / format byte changes (`decoder.rs`
//! doc-comment on `decode_into`). That buffer-reuse path — the
//! geometry-match short-circuit, the `Vec::resize` grow/shrink, and the
//! documented "on `Err`, `dst` is left partially-decoded" state — is a
//! distinct code path that the one-shot wrapper never exercises. The
//! most hostile shape for it is *decoding a second arbitrary frame into
//! the buffers a first arbitrary frame already populated*, where a
//! stale-geometry or stale-record bug would surface. This harness
//! therefore splits the fuzz input into two sub-frames and drives both
//! through a single reused `DecodedFrame`, in addition to the
//! stand-alone `decode_frame` exercise. `decode_into`'s contract is the
//! same as `decode_frame`'s: it must only ever return, never
//! panic / overflow / OOB / OOM.
//!
//! # Why the raster cap
//!
//! Once the header parses, `decode_frame` allocates one
//! `width * height` (8-bit) or `2 * width * height` (10/12/14-bit)
//! buffer per native plane (up to 4 planes). Those dimensions come
//! straight off the wire — capped at `header::MAX_DIMENSION` (32 768)
//! by the parser — so a valid header declaring, say, 32768 × 32768 RGBA
//! is a legitimate multi-gigabyte *resource* request, not a decoder
//! bug. Letting the allocator OOM on it would be a false positive that
//! masks the real logic bugs this harness is built to find. We
//! therefore reject declared frames whose total raster exceeds a 16 MiB
//! harness cap (mirroring what a real demuxer's sanity limits would do)
//! before driving the decoder, while still exercising every
//! parse/table/Huffman/predictor path on inputs up to the cap. The
//! library itself keeps only the spec's `MAX_DIMENSION` policy.

use libfuzzer_sys::fuzz_target;
use oxideav_magicyuv::decode_frame;
use oxideav_magicyuv::decoder::{decode_into, DecodedFrame};
use oxideav_magicyuv::header;
use oxideav_magicyuv::tables;

/// Upper bound on the declared output raster across all planes
/// (16 MiB). Anything larger is a resource request, not a logic path,
/// so the harness skips it.
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

/// `true` if `data` declares a header whose total raster sits within
/// the harness cap (or doesn't parse at all — in which case the decode
/// call cheaply hits the same `Err`). `false` means "this is a
/// multi-gigabyte resource request, skip the allocation-heavy decode".
fn within_raster_cap(data: &[u8]) -> bool {
    // A header that doesn't even parse is still a perfectly good
    // exercise of the parse-rejection paths — let the decoder run and
    // return the same cheap `Err`.
    let Ok(hdr) = header::parse(data) else {
        return true;
    };
    let Some(rec) = tables::lookup(hdr.format_byte) else {
        return true;
    };
    // Worst-case container size per sample: 2 bytes for the
    // high-bit-depth families, 1 for 8-bit. Plane subsampling only ever
    // *shrinks* a plane, so width*height*planes is a safe upper bound on
    // the total decoded raster.
    let bytes_per_sample: u64 = if rec.is_high_bit_depth() { 2 } else { 1 };
    let total = (hdr.width as u64)
        .checked_mul(hdr.height as u64)
        .and_then(|wh| wh.checked_mul(rec.planes as u64))
        .and_then(|s| s.checked_mul(bytes_per_sample));
    matches!(total, Some(n) if n <= MAX_OUTPUT_BYTES)
}

fuzz_target!(|data: &[u8]| {
    // (1) One-shot path. The whole point: decode must never panic /
    // overflow / OOB on a body of arbitrary bytes.
    if within_raster_cap(data) {
        let _ = decode_frame(data);
    }

    // (2) Streaming path. Split the input into two sub-frames and run
    // both through a *single* reused `DecodedFrame`, so the
    // geometry-match short-circuit, the in-place `Vec::resize`
    // grow/shrink, and the partial-decode-after-`Err` state are all put
    // under the same hostility as the one-shot path. The split point is
    // derived from the input itself so the corpus can steer it; an
    // empty half is fine (it hits the truncated-header `Err`).
    if data.is_empty() {
        return;
    }
    let split = (data[0] as usize) * data.len() / 256;
    let (first, second) = data.split_at(split.min(data.len()));

    let mut dst = DecodedFrame::empty();
    for sub in [first, second] {
        // Re-screen each sub-frame independently against the raster cap
        // — a sub-frame can declare its own dimensions.
        if within_raster_cap(sub) {
            // Result intentionally discarded; on `Err`, `dst` is left in
            // an unspecified-but-valid state and is fed straight back
            // into the next iteration — exactly the reuse-after-failure
            // shape we want to stress.
            let _ = decode_into(sub, &mut dst);
        }
    }
});
