# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Encoder slice-payload `BitWriter` pre-allocation.** The
  `encode_frame_u8` / `encode_frame_u16` slice-emit hot path
  constructed `BitWriter::new()` (zero-capacity `Vec<u8>`) at five
  sites per Auto-mode slice (Auto Huffman trial, fresh-Huffman
  re-emit, u16 raw bit-pack, u16 Auto trial, u16 fresh-Huffman emit).
  Each `bw.write` grew its backing buffer geometrically — ~17
  reallocations for a 1280×28 8-bit slice (35840 bytes), each
  copying the prefix. Every site now uses
  `BitWriter::with_capacity(byte_cap)` with a known upper bound
  (raw_size + 1 for Auto comparison; `raw_size + raw_size / 2 + 1`
  for 8-bit fresh-Huffman at `max_huff_len = 12`; `2 * raw_size + 1`
  for the 10/12/14-bit fresh-Huffman at `max_huff_len ∈ {14,16,18}`).
  The per-slice `payload` `Vec` is similarly pre-sized so the final
  `payload.extend(bw.finish())` doesn't pay one more allocation. The
  unused `BitWriter::new()` is removed in the same commit. Same
  observable byte stream — all 83 unit + round-trip tests pass
  under `--all-features` and 74 under `--no-default-features`.
  Measured improvement on the new
  `examples/quick_bench dynamic` scenario
  (`EncodeOptions::dynamic_auto()` = `PredictorStrategy::Dynamic` +
  `SliceMode::Auto`, the v2.4.2 always-on adaptive combination per
  spec/04 §3 + spec/05 §6.2): -2.2 % to -3.4 % across the four
  scenarios on the in-tree Apple M-series host. Fixed-strategy
  encode + the Huffman-only Fixed `time_encode` scenarios are
  within noise; decode is unchanged.

### Added

- **`examples/quick_bench dynamic` scenario.** Times the
  `EncodeOptions::dynamic_auto()` configuration (spec/04 §3 +
  spec/05 §6.2 — the v2.4.2 encoder's always-on adaptive
  combination, three predictor candidates evaluated per slice + per
  slice Huffman/raw size comparison). Joins the existing Fixed-
  strategy `encode` selector under the same `quick_bench all` driver.
  This is the production-relevant encode workload; the prior
  `time_encode` scenarios pin a single fixed predictor for
  hot-path attribution only.

- **`cargo-fuzz` encode harness (`fuzz/fuzz_targets/encode_magicyuv.rs`).**
  A second target drives `encode_frame(rec, w, h, slice_height, planes,
  options)` across the full parameter cube — 17 FOURCCs (8 + 10/12/14-bit
  RGB / RGBA / YUV / YUVA / Gray) × 4 predictor strategies
  (`Fixed{Left,Gradient,Median}` + Dynamic) × 3 per-slice modes
  (Huffman / Raw / Auto) × interlaced on/off — and asserts (a) the
  encoder never panics on hostile inputs, (b) every `Ok(bytes)` round-
  trips through `decode_frame` byte-for-byte (the encoder is forbidden
  from emitting wire bytes its own decoder rejects). The harness
  enforces the implicit encoder precondition `slice_height % rec.sub_y
  == 0` (the spec's v2.4.2 default `slice_height = 28` satisfies it
  trivially at every native subsampling); a smaller seed would round
  chroma `plane_slice_height` to 0 and silently drop the chroma planes
  from the wire — out of scope for a fuzz harness driving legal
  encoder inputs. Local baseline: ~210 k exec / 60 s, ~418 k / 180 s,
  zero crashes. Dimensions capped at 32×32 to keep the budget on
  logic (canonical-Huffman builder + length-limited Package-Merge
  fallback, slice-range arithmetic, RGB decorrelate, bit-pack/unpack
  symmetry, Dynamic per-slice predictor selection, Auto per-slice mode
  comparison) rather than allocator branches. The existing daily
  `fuzz.yml` workflow's reusable `crate-fuzz.yml` auto-discovers the
  new target and splits the 1800-s total budget evenly across both
  `decode_magicyuv` and `encode_magicyuv`. No `src/` changes.

- **`cargo-fuzz` decode harness (`fuzz/`).** A `decode_magicyuv` target
  drives `decode_frame` on arbitrary bytes, exercising the full header →
  slice-table → preamble → per-plane Huffman → raw / Huffman payload →
  Left / Gradient / Median predictor inverse → RGB-decorrelation-reversal
  chain and asserting decode always returns a `Result` (never panics /
  overflows / indexes OOB). A header pre-screen skips declared rasters
  above a 16 MiB cap to avoid OOM false positives on valid-but-enormous
  frames. Seed corpus spans every FOURCC family / bit-depth tier × encode
  mode. Local baseline: ~980 k exec in 60 s, zero crashes. A daily
  `fuzz.yml` workflow runs it in CI. No `src/` changes — the existing
  decoder was already panic-free across the fuzzed input space.

- **`decode_into(&[u8], &mut DecodedFrame)` streaming entry point.**
  The existing [`decode_frame`] always allocates fresh per-plane
  `Vec`s (one per plane in `plane_bufs`, one per output `DecodedPlane`,
  plus a working copy of the G plane inside the RGB inter-plane
  decorrelation reversal — 4-7 `Vec` allocations per frame). The new
  `decode_into` decodes into a caller-owned `DecodedFrame`, re-using
  the per-plane `Samples::U8` / `Samples::U16` inner-`Vec` storage
  when the frame geometry matches the previous call. Buffer life-cycle:
    - First call (or geometry change): plane Vecs are resized to fit
      (or re-allocated when previously of the wrong `Samples` variant).
    - Subsequent calls (same geometry): `Vec::clear` + `resize` keeps
      the existing allocation — `as_ptr()` + `capacity()` are stable
      across iterations.
  RGB inter-plane decorrelation reversal (both 8-bit and high-bit
  paths) is rewritten to use disjoint `split_at_mut` borrows of
  `[B', G, R']`, so the prior `wire_planes[1].clone()` working copy of
  the G plane is gone — `decode_frame` itself picks up the same
  allocation reduction. `decode_frame` is now a one-line wrapper
  around `decode_into(bytes, &mut DecodedFrame::empty())`. New unit
  tests `decode_into_matches_decode_frame_rgb_8bit`,
  `decode_into_matches_decode_frame_rgb_10bit`,
  `decode_into_reuses_plane_storage_when_geometry_matches` (asserts
  `Vec::as_ptr` + `Vec::capacity` survive a second decode unchanged),
  `decode_into_handles_geometry_change`, and
  `decode_into_handles_bit_depth_change` cover the new API. Public
  helpers `FrameHeader::placeholder()` and `FourccRecord::placeholder()`
  are added to seed the `DecodedFrame::empty()` slot before the first
  decode populates it. Measured win on
  `examples/quick_bench`'s RGB-family 1280×720 gradient scenario is
  -2 % … -9 % decode-side (varies with thermal / page-cache state;
  pure-malloc savings are larger as a fraction at smaller frame
  sizes); other family scenarios are within ±2 % (allocation-bound is
  a small fraction of their total decode work).

### Changed

- **Decoder Huffman primary table is now a packed `Vec<u32>`** (low 8
  bits = code length or `REDIRECT_MARKER = 0xff`, high 24 bits =
  symbol or secondary-subtable index). Replaces the prior
  `Vec<(u32, u8)>` layout that paid 8 B per slot due to alignment
  padding (5 B used, 3 B wasted). Same change applies to the
  per-prefix secondary subtables. The primary working set drops
  16 KB → 8 KB per plane at `max_len = 18`, and the 8-bit
  single-level hot loop in `HuffmanTable::decode_into_u8` does one
  4-B aligned `u32` load per pixel instead of an 8-B tuple fetch.
  Measured decode-side win across `examples/quick_bench` scenarios
  is -5 % … -13 % per FOURCC (1080p Gray Left -8 % on top of the
  round-4 baseline; 256×256 Median -13 %). The encoder side does
  not construct a `HuffmanTable` (it builds canonical lengths via
  Package-Merge directly) so encoder timings are unchanged. New
  unit tests `pack_entry_round_trip_terminal_and_redirect` (asserts
  every legal `(length, symbol)` pair in the primary's range
  survives the pack→unpack cycle and that `REDIRECT_MARKER` is
  unambiguous against any terminal length 1..=18) and
  `decode_into_u8_matches_per_pixel_decode` (asserts the batch
  helper and per-pixel `decode` produce the same symbol stream from
  the same bit input on a real-world `spec/05 §1.2` descriptor)
  cover the new layout. The trace JSONL emitter is unaffected
  (`HuffmanTable::codes()` still surfaces the per-symbol canonical
  codes for `audit/02 §4.2`'s `huff.used` map).

### Fixed

- **Encoder Huffman length cap now uses length-limited Package-Merge.**
  The per-plane code-length builder capped lengths at `max_length`
  (8-bit → 12, 10 → 14, 12 → 16, 14 → 18; spec/05 §1 table) with a
  naive `enforce_length_cap` "steal-a-bit" heuristic. On a deeply
  skewed residual histogram (a Fibonacci / near-geometric shape, e.g.
  a smooth-gradient plane after Median prediction) that heuristic both
  looped for millions of iterations *and* produced an **invalid**
  over-long code whose Kraft sum was far below 1 — a stream the
  decoder's canonical-code constructor (spec/05 §2.0.3) would reject.
  It is replaced by the **Package-Merge** algorithm (Larmore &
  Hirschberg, 1990), which produces an *optimal length-limited* prefix
  code with Kraft sum exactly 1.0 (spec/05 §1.3) and runs in
  milliseconds. The limiter is only invoked when the unbounded-optimal
  tree exceeds the cap; the common (non-binding) path keeps the plain
  canonical lengths byte-for-byte, so existing encoded streams and the
  `trace` lockstep tape are unchanged. New unit tests assert the
  capped code is complete (Kraft = 1) and prefix-free for Fibonacci /
  geometric / dominant / uniform histograms at 8- and 10-bit; new
  roundtrip tests encode→decode skewed M8G0 (Left), M8RG
  (Dynamic+Auto), and M0RG (Median) frames byte-exact.

### Changed

- **`trace`: `preamble_trailing.extra_bytes` is now a JSON integer.**
  The event's `extra_bytes` field is emitted as an integer count
  (`len(preamble) - cursor`) rather than a hex byte-string, matching
  the Python reference codec at `frame.py:514` per the
  `spec/05 §10 Q6` audit-corrected canonical schema +
  `audit/00 §8.8` resolution table (latent observation noted at
  `audit/04 §2.3`). v2.4.2 streams never produce trailing preamble
  bytes so the event remains zero-impact on the existing 4-fixture
  strict `jq -S -c '.'` trace lockstep. A new lib test
  (`trace_preamble_trailing_emits_integer_extra_bytes`) constructs
  a synthesised frame with 7 trailing bytes inserted into the
  preamble region (adjusting the slice-table entries accordingly)
  and asserts the canonical-form emission
  `{"kind":"preamble_trailing","extra_bytes":7}`.

## [0.0.4](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.3...v0.0.4) - 2026-05-07

### Other

- note trace lockstep MD5 unchanged
- round-N+1 candidates list
- u64-accumulator BitWriter, whole-byte drain
- encoder + bitreader: row-pair predictor split + peek_bits inline-always
- row-pair split_at_mut for bounds-check elimination
- batch decode_into_u{8,16} for the slice hot loop
- 8-byte fast-path refill for the Huffman hot loop
- add criterion harness + baseline doc

### Added

- **`PredictorStrategy::Dynamic`** — encoder spec/04 §3 strategy.
  When set on `EncodeOptions.strategy`, the encoder evaluates all
  three predictors (Left, Gradient, Median) on every slice, sums the
  signed-L1-norm of the post-prediction residuals, and writes
  whichever predictor produced the smaller sum into that slice's
  `predictor_id` byte. The wire format is unchanged
  (`predictor_id ∈ {0x01, 0x02, 0x03}` per slice); only the encoder's
  selection logic differs. Matches the v2.4.2 encoder dispatch at
  `magicyuv.dll!0x69b96970..0x69b96ac9` (spec/04 §3.1 evidence).
- **`SliceMode::Auto`** — per-slice raw fallback per spec/05 §6.2.
  When set on `EncodeOptions.mode`, the encoder builds the per-plane
  Huffman table once over all of the plane's residuals, then for each
  slice independently picks whichever of `(huffman_size, raw_size)`
  is smaller and writes the corresponding `slice_flags` byte
  (`0x00` or `0x01`). Raw size is `(slice_pixels * bits + 7) / 8`
  bytes per spec/05 §4.1. Matches the v2.4.2 "Adaptive coding"
  toggle that became always-on in v1.2.
- **`EncodeOptions::dynamic_auto()`** and
  **`EncodeOptions::fixed(p)`** builder helpers for the two common
  configurations (the spec/04 §3 + spec/05 §6.2 always-on combination
  and the fixed-predictor / fixed-Huffman case respectively).
- New `PredictorStrategy` enum (`Fixed(PredictorKind)` + `Dynamic`)
  re-exported at the crate root.
- 8 new lib tests covering the round-78 surface:
  `dynamic_strategy_round_trips_every_8bit_fourcc` (Dynamic across
  the seven 8-bit FOURCCs × 6 patterns = 42 byte-exact frames),
  `dynamic_strategy_round_trips_high_bit_depth` (4 high-bit-depth
  FOURCCs × 4 patterns = 16 byte-exact frames),
  `dynamic_picks_left_for_horizontal_ramp` (predictor-ID sanity
  check), `dynamic_varies_predictor_across_slices_with_mixed_content`
  (asserts Dynamic picks ≥ 2 distinct predictor IDs for an M8RG
  frame whose planes favour different predictors — mirrors the
  spec/04 §3.2 behavioural-confirmation pattern), `auto_mode_round_trips_8bit`
  (Auto across all 8-bit FOURCCs × 6 patterns), `auto_mode_picks_huffman_for_all_zero`
  (degenerate all-zero input gets `slice_flags = 0x00` everywhere),
  `auto_mode_falls_back_to_raw_on_random_input` (Auto is ≤ both
  fixed Raw and fixed Huffman on high-entropy data),
  `dynamic_plus_auto_round_trips_combined` (the always-on
  combination per `EncodeOptions::dynamic_auto()`), and
  `dynamic_is_no_larger_than_worst_fixed_on_mixed_content`
  (Dynamic dominates the worst fixed-predictor frame size).
- Criterion bench harness (`benches/decode.rs`, `benches/encode.rs`,
  `benches/roundtrip.rs`) covering the dominant FOURCCs (M8RG, M8Y0,
  M8G0, M0RG) at 720p / 1080p plus a 256×256 Median scenario.
  Inputs are synthesised on-the-fly via `encode_frame` so the benches
  ship without binary fixtures.
- `BENCHMARKS.md` baseline document with hot-path attribution
  (Huffman decode ~70-75 %, predictor ~25 %).
- `examples/quick_bench.rs` flat-loop helper for the
  measure-tweak-remeasure inner loop during optimization rounds.

### Changed

- `bitreader::BitReader::refill` grew an 8-byte fast path that issues a
  single big-endian u64 load when the cursor has at least 8 bytes
  ahead, then OR-merges the next bytes into the accumulator in one
  shift. Same observable bit stream as the per-byte loop (verified by
  the existing tests + the trace lockstep), with ≈ 4× fewer per-symbol
  loads in the Huffman hot path. Slow path (near-EOF, < 8 bytes left)
  retains the byte-loop with the documented zero-pad-past-end
  behaviour.
- `huffman::HuffmanTable::decode_into_u8` / `decode_into_u16` batch
  helpers — fold the per-symbol `peek_bits` + `consume` calls inline
  so the BitReader state (`acc`, `fill`, `pos`) stays in registers
  across the whole slice. The 8-bit path also short-circuits the
  primary-table-only case (`max_len ≤ PRIMARY_BITS = 12`) to skip
  the two-level dispatch entirely. The decoder's two slice loops
  (`decoder::decode_eight_bit`, `decoder::decode_high_bit_depth`)
  call the batch helper instead of the per-pixel `decode`.
- `predict::apply_u{8,16}_with_stride` inner loops use
  `data.split_at_mut(r * width)` once per row to expose the previous
  row as an immutable `&[u8]` / `&[u16]` slice and the current row as
  a mutable `&mut [u8]` / `&mut [u16]` of fixed length `width`.
  This lets the optimiser elide per-element bounds checks (the
  index `c` is provably `< width = slice.len()`), nearly halving
  decoder wall-time on every native FOURCC. The arithmetic — Left,
  Gradient, modular-Median, JPEG-LS-Median — is byte-identical.
- `encoder::encode_predictor_u{8,16}` mirror the decoder's row-pair
  `split_at_mut` shape so the encoder side picks up the same bounds-
  check elimination. Encoder wall-time drops 3-7 % on Gradient /
  10-bit; the encoder hot-path moves to the bit-writer + Huffman
  tree builder.
- `bitreader::BitReader::peek_bits` becomes `#[inline(always)]` so the
  Huffman batch decoder body stays a flat tight loop after inlining.
- `encoder::BitWriter::write` rewritten to use a 64-bit accumulator
  with a whole-byte drain. Replaces the `for i in (0..len).rev()`
  per-bit loop with a single shift + OR. Drains 1-2 whole bytes per
  call on typical Huffman alphabets. Identical observable byte
  stream (verified by the 53 unit tests + the encode→decode
  round-trip suite).

## [0.0.3](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.2...v0.0.3) - 2026-05-06

### Other

- remove AVI carriage from oxideav-magicyuv
- fill output_params().tag with the active FourCC
- declare 17 native v7 FourCCs via CodecInfo::tags
- Round 3 — OpenDML 2.0 super-index + huff.used schema fix
- Round 2 — high-bit-depth + interlaced + encoder + trace tape
- Round 1 — 8-bit MagicYUV v7 decoder
- Round 0 — clean-room rebuild scaffold (orphan master)

### Removed

- `avi::AviReader`, `avi::AviKind`, `avi::RiffSegmentLimit`,
  `encode_avi`, `encode_avi_opendml`, and the `src/avi.rs` module.
  AVI is a container; its decode + encode (including OpenDML 2.0
  multi-RIFF support) live in `oxideav-avi` (round trip tests there
  reference `oxideav-magicyuv` as a dev-dep). The codec crate now
  exposes only raw MAGY-frame encode/decode + the framework
  `Decoder` impl + `output_params()`.

### Added

- Declare native v7 FourCCs (`M8RG`, `M8RA`, `M8Y4`, `M8Y2`, `M8Y0`,
  `M8YA`, `M8G0`, `M0RG`, `M0RA`, `M0Y4`, `M0Y2`, `M0Y0`, `M0G0`,
  `M2RG`, `M2RA`, `M4RG`, `M4RA` — 17 total, per spec/01 §4.1) via
  `CodecInfo::tags([CodecTag::fourcc(…)])` so `oxideav-avi` can
  resolve them through `CodecResolver` without a hand-maintained
  codec_map.
- **`encoder::output_params(rec, width, height) -> CodecParameters`**
  helper (gated on the default-on `registry` feature). Returns the
  `CodecParameters` value that an `Encoder::output_params()` impl
  would surface — in particular `params.tag = Some(CodecTag::fourcc(rec.fourcc))`
  so `oxideav-avi`'s muxer writes the configured wire FourCC
  (one of the 17 native v7 codes) without needing the previous
  `extradata[0..4]` printable-FourCC hint hack. The tag flows from
  the encoder's `FourccRecord` directly to the muxer via
  `CodecParameters::tag` — the architectural correction that
  replaces the never-published 0.1.25 `CodecResolver::tag_for_codec`
  inverse-lookup path.

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
