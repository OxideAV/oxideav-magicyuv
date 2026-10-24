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
//! # No raster cap
//!
//! Header dimensions come straight off the wire (capped at
//! `header::MAX_DIMENSION` = 32 768 by the parser), so a valid header
//! can declare a multi-gigabyte raster. The decoder itself rejects any
//! slice whose payload is shorter than its residual count can fit
//! (`spec/05` §3.4: at least one bit per Huffman residual, `bits` bits
//! per raw residual) *before* it sizes the plane buffers, so the
//! decoded raster is bounded by 8 samples per input byte. The harness
//! therefore feeds every input straight in: an OOM here is a decoder
//! bug, not a resource request.

use libfuzzer_sys::fuzz_target;
use oxideav_magicyuv::decode_frame;
use oxideav_magicyuv::decoder::{DecodedFrame, decode_into};

fuzz_target!(|data: &[u8]| {
    // (1) One-shot path. The whole point: decode must never panic /
    // overflow / OOB on a body of arbitrary bytes.
    let _ = decode_frame(data);

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
        // Result intentionally discarded; on `Err`, `dst` is left in an
        // unspecified-but-valid state and is fed straight back into the
        // next iteration — exactly the reuse-after-failure shape we want
        // to stress.
        let _ = decode_into(sub, &mut dst);
    }
});
