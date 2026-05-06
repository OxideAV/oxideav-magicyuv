# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — round 3

- **OpenDML 2.0 super-index** (`spec/06` §6.1) on both decode and
  encode sides. The decoder's `AviReader` now walks every top-level
  `RIFF` chunk in the file: the first carries `AVI ` form (with
  `hdrl` + `movi` + optional `indx` super-index), every subsequent
  one carries `AVIX` form with a `movi` LIST. `00dc` chunks across
  all such RIFFs are concatenated into a single contiguous frame
  stream. The decoder is fully backward-compatible with single-RIFF
  AVI 1.0 files (the round 1 / 2 path).
- **Public OpenDML encoder** `encode_avi_opendml(rec, w, h, frames,
  segment_limit)` plus `RiffSegmentLimit::{OneGiB, Bytes(u64)}` and
  the `AviKind` enum. The first RIFF segment carries the `hdrl`
  with an `indx` super-index chunk in `strl`; subsequent segments
  are `RIFF AVIX` continuations. Each `indx` super-index entry's
  `qwOffset` / `dwSize` / `dwDuration` is back-patched after each
  RIFF's file offset is known.
- **`huff.used` trace schema fix** (`audit/02` §4.2 + `audit/03` §2).
  The `Event::Huff.used` field is now a per-symbol
  `(symbol, length, code)` triple slice serialised as
  `"used":{"<sym>":{"length":<L>,"code":<C>}, …}` in symbol-ascending
  order with insertion order `length, code` — exactly the shape the
  Python reference codec emits. `HuffmanTable::codes()` is the new
  accessor that surfaces the canonical-Huffman codes the decoder
  builds from the parsed descriptor; the trace emitter walks the
  parallel `(lengths, codes)` arrays to build the per-event map.
  After this fix, the Auditor's strict `jq -S -c` line-diff against
  the Python ref is empty across all 4 round-2 trace fixtures.
- 4 new in-crate tests: `trace_huff_used_field_is_per_symbol_map`
  (asserts the new map shape contains the canonical-code triple
  the encoder produced), `opendml_avi_round_trips_multi_riff`
  (8 frames forced into ≥ 2 RIFF segments, decode aggregates them
  back into one stream), `opendml_single_segment_when_limit_large_enough`
  (back-compat: large segment limit → exactly one RIFF), and
  `opendml_indx_entries_point_to_riff_offsets` (each indx super-index
  entry's `qwOffset` / `dwSize` matches the corresponding RIFF chunk).

### Notes — round 3

- The MagicYUV v7 wire-format coverage is **complete** modulo the
  proprietary v2.4.2 encoder's per-slice "Dynamic" predictor strategy
  and its byte-budget raw-fallback heuristic. Both are encoder-side
  conventions per `spec/04` §3 and `spec/05` §10 question 5; they do
  NOT affect decoder conformance. The round-2 Auditor's pass matrix
  (10 high-bit-depth FOURCCs × 2 sizes × 4 patterns = 80/80
  byte-exact, 2/2 interlaced byte-exact, 4/4 encoder roundtrip) plus
  round-3's trace-tape strict-diff fix completes the published spec
  coverage. The `ix00` per-RIFF index chunks of OpenDML 2.0 are not
  emitted by the Rust encoder — `spec/06` §6.1 explicitly notes that
  `ix00` is muxer territory, not codec territory; the decoder
  recovers all `00dc` chunks by walking the `movi` LIST directly,
  without consulting any index.

### Added — round 2

- **10/12/14-bit native FOURCC family**: M0RG, M0RA, M2RG, M2RA,
  M4RG, M4RA (RGB / RGBA), M0Y2, M0Y4, M0Y0 (YUV), M0G0 (Gray) decode
  via a u16 storage path. Per-bit-depth wrap mask `(1 << bits) - 1`
  applied after every predictor add. **Median at 10/12/14-bit is
  standard JPEG-LS** per `spec/04` §4.4 round-2 corrected note (NOT
  the modular formula used at 8-bit). Self-roundtrip for the four
  synthetic patterns (zero / const / ramp / random) at 16×16 + 64×64
  passes for every high-bit-depth FOURCC × every predictor × Huffman
  + raw mode.
- **Interlaced field-stride=2 prediction** (`spec/04` §5.1 round-2):
  when `flags & FLAG_INTERLACED == 0x02`, the predictor's top
  neighbour is row `r - 2`, and the first **two** rows of each slice
  have no top neighbour (Left across both, like progressive row 0).
  Self-roundtrip tests for 8-bit and high-bit-depth interlaced
  fixtures pass.
- **Public encoder API** (`encode_frame`, `encode_avi`,
  `EncodeOptions`, `PlaneInput`, `SliceMode`). The encoder is a
  clean-room implementation that produces well-formed v7 frames the
  decoder round-trips byte-for-byte. It does NOT chase the
  proprietary v2.4.2 encoder's "Dynamic" predictor strategy or its
  byte-budget raw-fallback heuristic — those are encoder-side
  conventions, not wire-format requirements.
- **JSON-Lines trace emitter** behind the `trace` Cargo feature.
  When the feature is on AND `OXIDEAV_MAGICYUV_TRACE_FILE` is set,
  the decoder writes one event per state transition: `hdr`,
  `slice_table`, `preamble`, `huff` (one per plane), `payload` (one
  per slice), `preamble_trailing`, `avi`. Field schema mirrors the
  round-1 Auditor review's §4 forward spec
  (`docs/video/magicyuv/audit/02-implementer-rust-round-1-review.md`)
  byte-for-byte so the Auditor's `jq`-line-diff harness can lockstep
  the Rust output against the cleanroom Python reference codec's
  `--trace` output.
- **Two-level Huffman lookup table** (primary 12-bit + per-prefix
  secondary subtables) keeps the per-plane lookup memory at 16 KB
  even for the 14-bit `max_length=18` tier (vs. the 1 MB a flat 18-bit
  table would use).
- High-bit-depth raw mode (bit-packed at `bits` bits per sample,
  MSB-first) — `spec/05` §4.1.

### Notes

- `Cargo.toml` adds the `trace` feature and the `tables/`
  artefacts continue to be loaded via `include_str!`.  The default
  `registry` feature is unchanged.
- The proprietary binary's exact encoder output (per-slice "Dynamic"
  strategy, 64×64 random raw-mode flag pattern) is not reproduced;
  spec/04 §3 + §4 specify it as encoder-side, and the v2.4.2-Auditor
  byte-exact lockstep stays a decode-side guarantee.

### Added — round 1

- Round-1 clean-room MagicYUV v7 decoder for the 8-bit native
  FOURCC family: **M8RG, M8RA, M8Y4, M8Y2, M8Y0, M8YA, M8G0**.
  - 32-byte v7 frame-header parser (`spec/01` §3) honouring all
    five Auditor-round-1 inline corrections (audit-corrected
    `aux_byte = max_huffman_code_length`, `+0x1c =
    slice_height`, encoder allowlist mask polarity correction,
    on-wire `predictor_id` is per-slice rather than the
    `+0x0b codec_variant` byte, RGB-family wire order is `(B', G,
    R')` after `(B − G, R − G)` decorrelation).
  - Slice-table walker (`spec/02` §5) supporting plane-major
    preamble layout and arbitrary `slice_height` (not assumed = 28).
  - Per-slice prediction (`spec/04` §4): Left (modular `+`),
    Gradient (`left + top - top_left` mod 256), and the
    audit-corrected modular 8-bit Median formula. The 10/12/14-bit
    Medians (standard JPEG-LS per round-2 validation note) are
    deferred along with their FOURCCs.
  - Per-plane canonical Huffman built via the audit-corrected
    longest-length-first cumulative algorithm of `spec/05` §2.0
    (Auditor round 2 correction; **not** RFC 1951 §3.2.2).
    Run-length-encoded length descriptor parsing per
    `spec/05` §1.1.
  - Per-slice raw-mode fallback (`spec/05` §4.1).
  - RGB inter-plane decorrelation reversal: `B = (B' + G) mod 256`,
    `R = (R' + G) mod 256`, with output in the user-facing
    `(G, B, R)[, A]` plane order (`spec/03` §4 audit-corrected).
  - Minimal RIFF/AVI demuxer (`spec/06`): walks `RIFF AVI` ↦
    `LIST hdrl` ↦ `strl` ↦ `strf` to extract the 32-byte MAGY
    extradata (validated against the per-frame header) and emits
    `00dc` chunk payloads to `decode_frame`. OpenDML 2.0
    super-index support is out of scope for round 1.
  - `oxideav-core` framework integration behind the default-on
    `registry` Cargo feature: `register(ctx)` installs a
    `Decoder` factory under codec id `"magicyuv"` mapping each
    decoded frame into a `VideoFrame` with planes packed into
    `Rgb24` / `Rgba` for RGB families, `Gray8` for M8G0, planar
    Y/U/V for the YUV families, and Y/U/V/A for M8YA.
  - 37 lib tests across the seven 8-bit FOURCCs × three
    predictors × Huffman / raw modes × multiple patterns plus
    bit-reader, predictor-roundtrip, header-rejection, AVI
    end-to-end, and registry round-trip checks. All green; no
    `#[ignore]`.

### Notes

- The `tables/00-fourcc-table.csv` and `tables/01-predictor-table.csv`
  artefacts are loaded once at startup via `include_str!` and parsed
  lazily; values are never retyped from spec into Rust source.
- The implementation reads `slice_height` from the header rather
  than assuming `28`, satisfying `spec/02` §10 question 1.
- Standalone (`--no-default-features`) builds drop the `oxideav-core`
  dependency entirely. Standalone test suite is 35/35 green.
