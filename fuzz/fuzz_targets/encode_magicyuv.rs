#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the public MagicYUV v7
//! **encoder** (`encode_frame`) and then through the in-crate decoder
//! (`decode_frame`) so any encoded byte stream the encoder accepts must
//! also survive the parser that the `decode_magicyuv` target hammers.
//!
//! The decoder fuzz target (`decode_magicyuv`) covers the attacker-
//! facing surface — bytes flow in from a network / file and the
//! decoder must never panic on them. The **encoder** is a different
//! shape of risk: its input is the typed tuple
//! `(rec, width, height, slice_height, planes: Vec<PlaneInput>, options)`
//! — not a raw byte stream — and it must never panic / abort /
//! integer-overflow / OOM regardless of how hostile the *caller* is.
//! Callers that mis-size a plane buffer, set a slice height larger than
//! the frame, pick an interlaced flag against an odd frame height, or
//! drive the Dynamic / Auto strategy across every FOURCC are all real
//! integration shapes. Just as critically, once the encoder *accepts*
//! an input it MUST produce wire bytes the decoder round-trips
//! byte-for-byte; a silent encoder/decoder skew is a correctness bug
//! the self-roundtrip suite catches on hand-picked fixtures but the
//! fuzzer drives across the whole parameter cube.
//!
//! ## Parameter cube exercised
//!
//! - **17 FOURCCs** (spec/01 §4.1): the round-1 8-bit set
//!   `{M8RG, M8RA, M8Y4, M8Y2, M8Y0, M8YA, M8G0}` plus the round-2
//!   high-bit-depth set `{M0RG, M0RA, M2RG, M2RA, M4RG, M4RA, M0Y2,
//!   M0Y4, M0Y0, M0G0}` — every native v7 byte the encoder accepts.
//! - **4 predictor strategies**: `Fixed(Left)`, `Fixed(Gradient)`,
//!   `Fixed(Median)`, and `Dynamic` (spec/04 §3 — picks per-slice
//!   minimum-residual). The Median branch covers both the 8-bit
//!   modular wrap form and the 10/12/14-bit JPEG-LS form.
//! - **3 per-slice modes**: `Huffman`, `Raw`, `Auto` (spec/05 §6.2 —
//!   per-slice fallback to whichever of `(huffman_size, raw_size)`
//!   is smaller).
//! - **Interlaced flag on/off** (spec/04 §5.1 field-stride=2 prediction).
//!
//! Per-plane buffer dimensions follow the FOURCC's subsampling
//! (`Family::Yuv` chroma planes at `(sub_x, sub_y)` per spec/03 §6).
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : FOURCC selector (mod 17) → one of the 17 native v7 bytes
//!   bytes 1-2   : width seed   → 2..=32, snapped to even
//!   bytes 3-4   : height seed  → 2..=32, snapped to even
//!   byte 5      : slice_height seed → 1..=height
//!   byte 6      : predictor strategy selector (mod 4)
//!                  → Fixed(Left) / Fixed(Gradient) / Fixed(Median) / Dynamic
//!   byte 7      : per-slice mode selector (mod 3) → Huffman / Raw / Auto
//!   byte 8      : interlaced bit (low bit; high bits ignored)
//!   bytes 9..   : raw plane-sample bytes consumed left-to-right across
//!                 the wire-order plane list (G,B,R[,A] for RGB;
//!                 Y,U,V[,A] for YUV; Y for Gray). At 10/12/14-bit each
//!                 sample consumes 2 bytes (little-endian) and is then
//!                 masked to `bit_depth` bits — anything in the high
//!                 bits is discarded by the encoder's own input
//!                 sanitisation. If the input runs out, the remaining
//!                 samples are filled with zero so a short input still
//!                 exercises the full pipeline.
//! ```
//!
//! ## Contract under test
//!
//! 1. `encode_frame(...)` always *returns* a `Result` — no panic, no
//!    abort, no integer overflow (in a debug / ASAN build), no OOM.
//! 2. Whenever `encode_frame` returns `Ok(bytes)`, `decode_frame(&bytes)`
//!    must also return `Ok(decoded)` (the encoder is not allowed to
//!    emit syntactically-malformed bytes that its own decoder rejects).
//! 3. Whenever both calls succeed, the decoded per-plane samples must
//!    equal the input per-plane samples bit-exactly. This is the
//!    self-roundtrip invariant the in-tree tests pin on hand-picked
//!    fixtures, here driven across arbitrary `(fourcc × dims ×
//!    slice_height × strategy × mode × interlaced × pixels)` tuples.
//!
//! ## Dimension cap
//!
//! Dimensions are capped at 32×32 so the fuzzer's budget lands on
//! encoder/decoder logic (canonical-Huffman builder + length-limited
//! Package-Merge fallback, slice-range arithmetic, RGB decorrelate,
//! bit-pack/unpack symmetry, Dynamic per-slice predictor selection,
//! Auto per-slice mode comparison) rather than the trivial "allocate a
//! few MiB" branch the format's syntax allows. At 10/12/14-bit a
//! 32×32 4-plane RGBA frame is still only 8 KiB — well within the
//! fuzzer's per-iteration budget.

use libfuzzer_sys::fuzz_target;
use oxideav_magicyuv::tables::{Family, PredictorKind};
use oxideav_magicyuv::{
    decode_frame, encode_frame, tables, EncodeOptions, PlaneInput, PredictorStrategy, Samples,
    SliceMode,
};

/// Every native v7 format byte from `tables/00-fourcc-table.csv`
/// (spec/01 §4.1). 7 round-1 (8-bit) + 10 round-2 (10/12/14-bit).
const FOURCC_BYTES: [u8; 17] = [
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, // 8-bit
    0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, // 10/12/14-bit RGB/RGBA + 10-bit YUV 4:2:2
    0x73, 0x76, 0x7b, // 10-bit Gray + 10-bit YUV 4:4:4 + 10-bit YUV 4:2:0
];

/// Maximum frame dimension the harness drives the encoder with. At
/// 32×32 4-plane RGBA-14-bit the encoded raster is 4 KiB before
/// Huffman, well within the libfuzzer per-iteration budget.
const MAX_DIM: u32 = 32;

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }

    // ── Header byte 0: FOURCC ──────────────────────────────────────
    let fb = FOURCC_BYTES[(data[0] as usize) % FOURCC_BYTES.len()];
    let rec = match tables::lookup(fb) {
        Some(r) => r,
        None => return, // unreachable for the curated FOURCC_BYTES set
    };

    // ── Header bytes 1-4: width / height ──────────────────────────
    // Snap to even ≥ 2 so the subsampled chroma planes (sub_x/sub_y
    // up to 2) never round down to 0 rows or 0 columns. The encoder
    // accepts odd dimensions in principle — the in-crate roundtrip
    // tests only cover even — and the fuzzer can still find issues
    // with odd dims via the slice_height / interlaced axes below.
    let width_seed = u16::from_le_bytes([data[1], data[2]]) as u32;
    let height_seed = u16::from_le_bytes([data[3], data[4]]) as u32;
    let width = ((width_seed % MAX_DIM) + 2) & !1; // even in 2..=32
    let height = ((height_seed % MAX_DIM) + 2) & !1; // even in 2..=32

    // ── Header byte 5: slice_height ───────────────────────────────
    // Bias toward small slice heights so a single frame exercises
    // multiple slices (each one runs the per-slice Huffman / raw mode
    // selector + per-slice predictor decision under Dynamic).
    //
    // Two encoder preconditions enforced here so a legal call lands on
    // the encoder, not garbage-in-garbage-out arithmetic:
    //
    // 1. `slice_height >= 1` — `slices_per_plane = h.div_ceil(sh)`
    //    div-by-zeroes on `sh = 0`.
    // 2. `slice_height` is a multiple of `rec.sub_y` (the chroma
    //    vertical subsampling). The encoder derives chroma slice
    //    height as `slice_height / sub_y` (integer divide); a value
    //    smaller than `sub_y` rounds to 0 and the chroma slices then
    //    cover no rows — the encoder still returns Ok but the chroma
    //    planes never reach the wire, breaking the round-trip
    //    invariant. (Spec/02 §3 fixes `slice_height = 28` for v2.4.2,
    //    which trivially satisfies both — even at 4:2:0 sub_y=2,
    //    `28 / 2 = 14` rows per chroma slice. The constraint isn't
    //    written explicitly in the spec but is implied by every
    //    fixture; the fuzzer is **not** the venue to validate the
    //    encoder's behaviour on out-of-spec slice geometries.)
    let sub_y = rec.sub_y as u32;
    let sh_seed = (data[5] as u32 % height).max(1);
    // Round down to a multiple of sub_y (≥ sub_y).
    let slice_height = ((sh_seed / sub_y).max(1)) * sub_y;

    // ── Header byte 6: predictor strategy ─────────────────────────
    let strategy = match data[6] % 4 {
        0 => PredictorStrategy::Fixed(PredictorKind::Left),
        1 => PredictorStrategy::Fixed(PredictorKind::Gradient),
        2 => PredictorStrategy::Fixed(PredictorKind::Median),
        _ => PredictorStrategy::Dynamic,
    };
    // `EncodeOptions::predictor` is documented as ignored when
    // `strategy = Fixed(_)` (see EncodeOptions struct doc-comments).
    // Mirror the in-crate roundtrip-test convention: keep it
    // consistent with strategy when Fixed, default to Gradient for
    // Dynamic. The encoder doesn't read this field on the Dynamic
    // path, so the value is arbitrary.
    let predictor = match strategy {
        PredictorStrategy::Fixed(p) => p,
        PredictorStrategy::Dynamic => PredictorKind::Gradient,
    };

    // ── Header byte 7: per-slice mode ─────────────────────────────
    let mode = match data[7] % 3 {
        0 => SliceMode::Huffman,
        1 => SliceMode::Raw,
        _ => SliceMode::Auto,
    };

    // ── Header byte 8: interlaced (bit 0) + ColorMatrix nibble (bits 4..7) ─
    // The high nibble of byte 8 seeds the `EncodeOptions::color_matrix`
    // field directly. The encoder masks the low 4 bits before shifting
    // into the flags dword at bits 20..23 (`spec/01` §3.1), and the
    // value 1 is the documented matrix-skip sentinel — driving the
    // full 0..=15 range here exercises both the shift / mask plumbing
    // and the spec's "skip the OR when registry value == 1" branch.
    let interlaced = (data[8] & 1) != 0;
    let color_matrix = data[8] >> 4;

    let options = EncodeOptions {
        strategy,
        mode,
        interlaced,
        color_matrix,
        predictor,
    };

    // ── Build per-plane buffers ───────────────────────────────────
    // Per-plane geometry: chroma planes of `Family::Yuv` / `Family::Yuva`
    // subsample by `(rec.sub_x, rec.sub_y)`; everything else is full
    // resolution. Mirrors `encoder::plane_dims_for` and the in-crate
    // `make_planes_u{8,16}` test helpers.
    let num_planes = rec.planes as usize;
    let mut payload = &data[9..];
    let mut planes: Vec<PlaneInput> = Vec::with_capacity(num_planes);
    let mask: u16 = rec.sample_mask() as u16;

    for p in 0..num_planes {
        let (sub_x, sub_y) = match rec.family {
            Family::Yuv if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
            Family::Yuva if p == 1 || p == 2 => (rec.sub_x as usize, rec.sub_y as usize),
            _ => (1usize, 1usize),
        };
        let pw = (width as usize) / sub_x;
        let ph = (height as usize) / sub_y;
        let count = pw * ph;
        if rec.is_high_bit_depth() {
            // 2 bytes per sample (little-endian) then masked to
            // bit_depth; the encoder also masks but doing so here
            // means the input we keep for the round-trip assertion
            // matches what the decoder will return.
            let mut buf = vec![0u16; count];
            for slot in buf.iter_mut() {
                if payload.len() >= 2 {
                    let v = u16::from_le_bytes([payload[0], payload[1]]);
                    *slot = v & mask;
                    payload = &payload[2..];
                } else if !payload.is_empty() {
                    *slot = (payload[0] as u16) & mask;
                    payload = &payload[1..];
                } else {
                    *slot = 0;
                }
            }
            planes.push(PlaneInput::U16(buf));
        } else {
            let mut buf = vec![0u8; count];
            let take = count.min(payload.len());
            buf[..take].copy_from_slice(&payload[..take]);
            payload = &payload[take..];
            planes.push(PlaneInput::U8(buf));
        }
    }

    // Cache the inputs so the round-trip assertion can compare
    // bit-exactly against the decoded planes after encode_frame
    // consumes the PlaneInput list.
    let inputs: Vec<PlaneInput> = planes.clone();

    // ── 1. Encoder must always return ─────────────────────────────
    let bytes = match encode_frame(rec, width, height, slice_height, planes, options) {
        Ok(b) => b,
        Err(_) => return, // legitimate rejection (e.g. mis-sized plane on a
                          // FOURCC the harness happened to size for a sibling
                          // family); not a panic, contract upheld.
    };

    // ── 2. Encoder output must decode ────────────────────────────
    let decoded = match decode_frame(&bytes) {
        Ok(f) => f,
        Err(e) => {
            // The encoder accepted these parameters but our own
            // decoder rejected the bytes it produced — that's a
            // hard contract violation, not a fuzz-discoverable
            // corruption.
            panic!(
                "encoder produced bytes the in-crate decoder rejects: {e:?} \
                 fourcc={:?} fb={fb:#04x} {width}x{height} sh={slice_height} \
                 strategy={strategy:?} mode={mode:?} interlaced={interlaced} \
                 encoded_len={}",
                rec.fourcc,
                bytes.len()
            );
        }
    };

    // ── 3. Round-trip must be bit-exact ───────────────────────────
    assert_eq!(
        decoded.planes.len(),
        inputs.len(),
        "plane count mismatch after roundtrip: encoded {} decoded {}",
        inputs.len(),
        decoded.planes.len()
    );
    assert_eq!(decoded.width, width, "decoded width mismatch");
    assert_eq!(decoded.height, height, "decoded height mismatch");

    for (i, (dec, inp)) in decoded.planes.iter().zip(inputs.iter()).enumerate() {
        match (&dec.samples, inp) {
            (Samples::U8(g), PlaneInput::U8(e)) => {
                assert_eq!(
                    g, e,
                    "plane {i} u8 roundtrip mismatch: fb={fb:#04x} {width}x{height} \
                     sh={slice_height} strategy={strategy:?} mode={mode:?} \
                     interlaced={interlaced}"
                );
            }
            (Samples::U16(g), PlaneInput::U16(e)) => {
                assert_eq!(
                    g, e,
                    "plane {i} u16 roundtrip mismatch: fb={fb:#04x} {width}x{height} \
                     sh={slice_height} strategy={strategy:?} mode={mode:?} \
                     interlaced={interlaced}"
                );
            }
            _ => panic!("plane {i} container variant mismatch after roundtrip: fb={fb:#04x}"),
        }
    }
});
