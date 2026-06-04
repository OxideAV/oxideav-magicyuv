# oxideav-magicyuv

Pure-Rust MagicYUV lossless video codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Codec-only.** Decodes the full native FOURCC set: 8-bit (M8RG, M8RA,
M8Y4, M8Y2, M8Y0, M8YA, M8G0) and 10/12/14-bit (M0RG, M0RA, M2RG,
M2RA, M4RG, M4RA, M0Y2, M0Y4, M0Y0, M0G0). Honours
`flags & FLAG_INTERLACED` for field-stride=2 prediction (`spec/04`
§5.1). The public encoder API (`encode_frame`) emits wire-format
frames the decoder round-trips byte-for-byte. Encoder strategies:
**fixed** Left / Gradient / Median (`PredictorStrategy::Fixed`),
**Dynamic** per-slice predictor selection by minimum residual L1
norm (`PredictorStrategy::Dynamic`, spec/04 §3), and per-slice
Huffman / raw fallback (`SliceMode::Auto`, spec/05 §6.2 —
`(pixels * bits + 7) / 8` byte-budget). `EncodeOptions::dynamic_auto()`
combines both for the spec/04 §3 + spec/05 §6.2 always-on
configuration the v2.4.2 encoder ships with. A `trace` Cargo feature
surfaces a JSONL trace tape for the Auditor's lockstep harness; the
`huff.used` field is a per-symbol `{length, code}` map per the
audit/02 §4.2 forward spec, and `preamble_trailing.extra_bytes` is
emitted as a JSON integer count per the spec/05 §10 Q6 +
audit/00 §8.8 canonical schema (matching the Python ref's
`frame.py:514`).

The implementation is built against the strict-isolation clean-room
workspace at
[`docs/video/magicyuv/`](https://github.com/OxideAV/docs/tree/master/video/magicyuv).
That workspace completed six Specifier rounds, two Auditor rounds,
an Implementer-Python reference round, and three Validator rounds
before this orphan reset. The Implementer in this repo reads only
`spec/00..05` plus `tables/00-fourcc-table.csv` and
`tables/01-predictor-table.csv` — no FFmpeg source, no proprietary
binary, no Python reference source, no `old` branch.

AVI is a container, not a codec — its demux/mux (single-RIFF AVI 1.0
+ OpenDML 2.0 multi-RIFF per `spec/06`) lives in
[`oxideav-avi`](https://github.com/OxideAV/oxideav-avi), which uses
`oxideav-magicyuv` as a dev-dep for end-to-end roundtrip coverage.

## Pipeline

| Stage                  | Source                                |
| ---------------------- | ------------------------------------- |
| 32-byte v7 header      | spec/01 §3 (audit-corrected aux_byte / slice_height) |
| Slice table + preamble | spec/02 §5..§7                        |
| Plane-major plane order| spec/03 §4..§6 (RGB wire order audit-corrected) |
| Per-slice predictors   | spec/04 §4 (Left, Gradient, Median; modular 8-bit Median + standard JPEG-LS at 10/12/14-bit) |
| Interlaced field-stride| spec/04 §5.1 round-2 (top neighbour = row r-2; first 2 rows raw) |
| Per-plane Huffman      | spec/05 §1.1 (RLE descriptor)         |
| Canonical-code build   | spec/05 §2.0 (longest-length-first cumulative — **NOT** RFC 1951; auditor round 2 correction) |
| Raw-mode fallback      | spec/05 §4.1                           |

## Public API

- [`decode_frame`] — decode a single MAGY-prefixed frame's bytes.
  Returns one [`DecodedPlane`] per native plane; sample storage is
  `u8` for 8-bit FOURCCs and `u16` for 10/12/14-bit FOURCCs.
- [`decode_into`] — streaming variant. Decodes into a caller-owned
  `DecodedFrame`, re-using the per-plane `Vec` storage from the
  previous call when geometry matches. Skips 4-7 `Vec` allocations
  per frame (one per plane + the prior working copy of the G plane
  used in RGB inter-plane decorrelation reversal — that working
  copy is also gone from `decode_frame` itself now).
- [`encode_frame`] — encode one frame from per-plane pixel buffers.
- [`header::parse`] — standalone v7 header parser.
- [`Error`], [`Result`] — crate-local error type.
- `register(ctx)` (default-on `registry` feature) — wire the
  decoder into `oxideav-core`'s codec registry.
- `output_params(rec, w, h)` — `CodecParameters` (with
  `tag = Some(CodecTag::fourcc(rec.fourcc))`) the encoder produces;
  muxers consume this to write the right wire FourCC.

## Cargo features

- **`registry`** (default): wire the crate into `oxideav-core`'s
  codec registry. Standalone builds (`--no-default-features`)
  drop the `oxideav-core` dependency entirely.
- **`trace`** (off): emit JSONL trace events to the path in
  `OXIDEAV_MAGICYUV_TRACE_FILE` during decode. Used by the round-2
  Auditor's `jq`-line-diff lockstep harness against the cleanroom
  Python reference codec's `--trace` output.

## Fuzzing

Three [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) harnesses
live under [`fuzz/`](fuzz/), all three wired into the daily `fuzz.yml`
workflow that splits a 1800-s total budget evenly across them.

**`decode_magicyuv`** drives `decode_frame` on an arbitrary byte
buffer, exercising the whole header → slice-table → preamble →
per-plane Huffman → raw / Huffman slice payload → Left / Gradient /
Median predictor inverse → RGB-decorrelation-reversal chain; the
contract under test is that decode always *returns* a `Result` and
never panics / overflows / indexes OOB. A header pre-screen skips
declared rasters above a 16 MiB cap so a valid-but-enormous frame (a
resource request, not a logic bug) doesn't register as an OOM false
positive. Seed corpus spans every FOURCC family / bit-depth tier ×
encode mode (Huffman / raw / Dynamic+Auto / interlaced). Latest local
baseline: ~980 k exec in 60 s, zero crashes.

**`encode_magicyuv`** drives `encode_frame(rec, w, h, slice_height,
planes, options)` across the full parameter cube — 17 native v7
FOURCCs (8 + 10/12/14-bit RGB / RGBA / YUV / YUVA / Gray) × 4
predictor strategies (`Fixed{Left,Gradient,Median}` + Dynamic) × 3
per-slice modes (Huffman / Raw / Auto) × interlaced on/off. Two
contracts are checked: (a) the encoder never panics on hostile inputs,
(b) every `Ok(bytes)` round-trips through `decode_frame` byte-for-byte
(the encoder is forbidden from emitting wire bytes its own decoder
rejects). Dimensions capped at 32×32 so the budget lands on encode
logic — canonical-Huffman builder + length-limited Package-Merge
fallback, slice-range arithmetic, RGB decorrelate, bit-pack/unpack
symmetry, Dynamic per-slice predictor selection, Auto per-slice mode
comparison — rather than allocator branches. Latest local baseline:
~210 k exec / 60 s, ~418 k / 180 s, zero crashes.

**`huffman_descriptor`** pushes arbitrary bytes straight into the
`huffman::parse_lengths` + `HuffmanTable::build` pair, bypassing the
32-byte header / slice-table / preamble framing the full-frame target
walks first. Concentrates fuzz pressure on `spec/05` §1.1 run-length
descriptor decode, `spec/05` §2.0 canonical-Huffman code construction
(the audit-corrected longest-length-first cumulative accumulator +
Kraft check, with `1u64 << len` at `len = max_length = 18` for the
14-bit tier), and the two-level primary/secondary table arithmetic
(`REDIRECT_MARKER`, per-prefix subtable allocation, residual-bit
spread). Successful builds then drive `decode_into_u{8,16}` on the
trailing fuzz bytes so the post-build BitReader peek/consume hot loop
sees pressure too. Input layout: byte 0 = bit-depth tier selector
(mod 4 → `n_symbols ∈ {256, 1024, 4096, 16384}`, `max_length ∈ {12,
14, 16, 18}`), bytes 1-2 = descriptor cap (LE u16, capped at 16 KiB),
bytes 3.. = descriptor + trailing decode payload. Latest local
baseline: ~830 k exec / 16 s (~51 k exec/s), zero crashes.

```sh
cd fuzz && cargo +nightly fuzz run decode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run encode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run huffman_descriptor -- -max_total_time=60
```

## Profiling

[`examples/profile_magicyuv.rs`](examples/profile_magicyuv.rs) is a
flat sampling-profiler driver. The Criterion benches and
`examples/quick_bench.rs` are timing-oriented (Criterion's warm-up +
estimator math show up in the profile; `quick_bench` runs each
scenario for 10-30 iterations, too short for a sampling profiler to
settle on the codec body). `profile_magicyuv` runs each scenario in a
single flat loop with one `Instant`-pair around it, so `samply` /
`cargo flamegraph` / `perf record` see the codec hot paths directly.

Modes: `encode`, `decode`, `roundtrip`, `dynamic` (the
`EncodeOptions::dynamic_auto()` v2.4.2 always-on combination per
spec/04 §3 + spec/05 §6.2), `interlaced` (spec/04 §5.1 field-stride=2
prediction), `all`.

Scenarios cover the dominant cost-axes the workspace README rows
track: the 8-bit primary-Huffman path (M8RG / M8Y0 1280×720 +
M8G0 1920×1080), the two-level 10-bit Huffman path (M0RG 1280×720),
and the modular-Median 8-bit path (M8RG 256×256). Inputs are the
same `quick_bench` gradient + 3-bit xorshift noise so profile output
and bench numbers reference the same residual histogram.

```sh
cargo build --release --example profile_magicyuv
samply record -- ./target/release/examples/profile_magicyuv encode 500
samply record -- ./target/release/examples/profile_magicyuv decode 2000
samply record -- ./target/release/examples/profile_magicyuv dynamic 500
cargo flamegraph --example profile_magicyuv -- decode 2000
```

## Why clean-room

MagicYUV is a closed-source commercial codec by Pavel Zlatev / "ignus"
(`magicyuv.com`). Reverse-engineering it from the proprietary binary
is permitted under 17 U.S.C. §1201(f) (DMCA interoperability
exemption), *Sega v. Accolade* / *Sony v. Connectix*, and EU Directive
2009/24/EC Articles 5(3) + 6. The cleanroom workspace at
`docs/video/magicyuv/` reverse-engineered the v7 layout from the
proprietary `magicyuv.dll` directly; the Implementer in this repo is
wall-isolated from any FFmpeg-derived material.

## Implementer allow-list

- `docs/video/magicyuv/spec/00..05`
- `docs/video/magicyuv/tables/00-fourcc-table.csv`
- `docs/video/magicyuv/tables/01-predictor-table.csv`
- `oxideav-core`'s public API

## Implementer forbidden-input list

- FFmpeg `libavcodec/magicyuv*.c` and any FFmpeg-derived material.
- The retired `old` branch of this repository.
- `docs/video/magicyuv/reference/binaries/` (proprietary binary).
- `docs/video/magicyuv/reference-impl/python/` (cleanroom Python
  reference codec).
- Any third-party MagicYUV decoder source.
