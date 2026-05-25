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
use oxideav_magicyuv::header;
use oxideav_magicyuv::tables;

/// Upper bound on the declared output raster across all planes
/// (16 MiB). Anything larger is a resource request, not a logic path,
/// so the harness skips it.
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    // Pre-screen the header (no allocation) so we can bound the
    // decoder's per-plane allocation before it runs. A header that
    // doesn't even parse is still a perfectly good exercise of the
    // parse-rejection paths, so fall through to `decode_frame` in that
    // case — it will return the same `Err` cheaply.
    if let Ok(hdr) = header::parse(data) {
        if let Some(rec) = tables::lookup(hdr.format_byte) {
            // Worst-case container size per sample: 2 bytes for the
            // high-bit-depth families, 1 for 8-bit. Plane subsampling
            // only ever *shrinks* a plane, so width*height*planes is a
            // safe upper bound on the total decoded raster.
            let bytes_per_sample: u64 = if rec.is_high_bit_depth() { 2 } else { 1 };
            let total = (hdr.width as u64)
                .checked_mul(hdr.height as u64)
                .and_then(|wh| wh.checked_mul(rec.planes as u64))
                .and_then(|s| s.checked_mul(bytes_per_sample));
            match total {
                Some(n) if n <= MAX_OUTPUT_BYTES => {}
                // Over the cap (or overflowed): a legitimate resource
                // request, not a bug. Skip the allocation-heavy decode.
                _ => return,
            }
        }
    }

    // The whole point: decode must never panic / overflow / OOB on a
    // body of arbitrary bytes.
    let _ = decode_frame(data);
});
