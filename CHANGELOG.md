# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.7](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.6...v0.0.7) - 2026-09-01

### Fixed

- *(clippy)* use byte-string literal for byte_char_slices lint (test-only)

### Other

- hide internal pub surface from rustdoc/semver (fleet rule 2026-09-01)
- document forward-compat decode tolerances (spec open questions)
- *(decode)* pin invariance to codec_variant byte + slice-table entry[0] (spec/04 §8 Q4, spec/02 §10 Q4)
- *(decode)* tolerate trailing preamble bytes in default features (spec/05 §10 Q5)
- *(decode)* prove slice_flags reserved bits 1..7 are ignored (spec/04 §8 Q3)
- add CI / crates.io / docs.rs / MIT-license badges
- typed width_extra accessor + decode robustness to +0x18 mismatch
- pin raw-slice on-wire byte sizes to spec/05 §4.1 (open Q2)
- refresh stale round-1-era doc comments to match complete impl
- document the §6 slice-height divisibility guard + §4.4 formula pins
- *(registry)* cover §6 slice-height guard at the registry encoder tier
- *(predict)* pin spec/04 §4.4 bit-depth-conditional Median formula
- reject slice_height indivisible by sub_y on 4:2:0 (spec/02 §6)
- attach a MAGY content probe to disambiguate FourCC claims
- report exact PixelFormat in output_params (spec/03 §4/§7)
- expose slice_height as a registry encoder option (spec/02 §4)
- accept interleaved 8-bit RGB/RGBA at the registry encoder
- typed encoder options via CodecParameters::options (spec/04 + spec/05)
- register a framework-level Encoder (spec/01 §4.1 + spec/03 §7)
- document the cartesian + minimal-geometry property sweeps
- seed decode corpus with 55 minimal-geometry frames
- *(roundtrip)* minimal-geometry dynamic_auto sweep across all 17 FOURCCs
- *(roundtrip)* minimal-geometry property sweep across all 17 FOURCCs
- pin HBD-path malformed-input rejection guards (10/12/14-bit)
- document proprietary-ground-truth conformance fixtures
- *(decode)* ground-truth interlaced field-stride decode from spec §5.1.1
- *(huffman)* pin exact §2.0.1 canonical code-assignment table
- *(decode)* decode proprietary-ground-truth m8rg_64x64_zero from spec hex
- *(roundtrip)* full cartesian encode→decode property sweep across the advertised input space
- surface HuffmanIncomplete on under-full codespace hit (spec/05 §2.1)
- *(huffman)* pin two-level high-bit-depth decode EOF/truncation robustness (spec/05 §3.3)
- fix EOF-refill subtract-overflow in single-level Huffman decode (spec/05 §3)
- *(decode)* pin reserved predictor_id=0x00 rejection (spec/04 §1.2 + §7.3c)
- refresh to current status, drop per-round changelog cruft

### Other

- silence the newly-promoted `clippy::byte_char_slices` lint in a registry probe test (`[b'M', b'A']` → `b"MA"`); mechanical, test-only, no behaviour change
- **pin decode-invariance to the header `codec_variant` byte `+0x0b`** (`spec/04` §8 open-question 4). The byte is always `0x02` in v2.4.2 streams and is *not* the on-wire predictor (that is the per-slice `predictor_id`, `spec/04` §1.2); the parser stores it for diagnostics but the decode dispatch never consults it. New `forward_compat::decoder_ignores_codec_variant_byte` rewrites `+0x0b` to `{0x00, 0x01, 0x03, 0x04, 0x7f, 0xff}` (including the reserved `>2` range) on an 8-bit M8RG and a high-bit-depth M0Y0 frame, confirming `FrameHeader::codec_variant` faithfully reports the mutated byte while the decoded pixels stay bit-exact — a decoder targeting v2.4.2 output can ignore the field entirely
- **pin decode-invariance to slice-table `entry[0]`** (`spec/02` §10 open-question 4). Every v2.4.2 fixture has `entry[0] == entry[1]`; whether the duplication is a meaningful "preamble-end" boundary or a vestigial v6 artifact is unresolved. A §5.1-following decoder derives the preamble end and every slice start from `entry[1..]` and never reads `entry[0]`, so it is robust to either interpretation. New `forward_compat::decoder_ignores_slice_table_entry_zero` corrupts `entry[0]` to `{0, 1, 0x2000, 0xdeadbeef}` on a six-slice M8RG frame and asserts bit-exact recovery
- **pin decode-tolerance of trailing preamble bytes in the default feature set** (`spec/05` §10 open-question 5). The per-plane Huffman-descriptor parser consumes exactly the bytes needed for N lengths per plane and then advances to the slice payloads, without checking that consumption reaches `entry[1]` (the preamble end); a future/third-party encoder that padded the preamble would still decode. The existing `trace_preamble_trailing_emits_integer_extra_bytes` test proves the `trace` build emits a `preamble_trailing` diagnostic for this branch, but it is `#[cfg(feature = "trace")]` and compiled out of the default CI build, so nothing pinned the *decoded pixels* in the default configuration. New `forward_compat::decoder_tolerates_trailing_preamble_bytes` inserts `pad_len ∈ {1,3,8}` padding bytes between the last descriptor and the first slice (bumping every slice offset) on single-plane M8G0 and three-plane M8RG 16×16 frames and asserts bit-exact recovery — the first default-feature coverage of open-question 5's forward-compat path
- **prove the decoder ignores the reserved bits 1..7 of a slice's `slice_flags` byte** (`spec/04` §8 open-question 3). Only bit 0 selects Huffman (`0`) vs raw (`1`); bits 1..7 are reserved forward-compat padding a future/third-party encoder may set, and both slice loops mask `& 0x01`. The v2.4.2 encoder only ever writes `0x00`/`0x01`, so every self-roundtrip test sees a clear high nibble — a regression that compared the whole byte (`== 0x01` / `!= 0`) would slip past them all. New `forward_compat::decoder_ignores_reserved_slice_flag_bits` ORs `0xfe` (all reserved bits set, bit 0 preserved) into every slice's flags byte on a 32×32 two-slices-per-plane M8RG (8-bit `decode_eight_bit` path) and M0RG (high-bit-depth `decode_high_bit_depth` path) frame, in both Huffman and Raw slice modes, and asserts bit-exact recovery against the unmodified baseline
- **expose the redundant `+0x18` width copy as a typed accessor + harden the decoder against a `width != width_extra` header** (`spec/01` §3.2 / §6 open-question 1). The encoder writes the ICCompress width into *both* `+0x10` and `+0x18`; a stream where they disagree is one the surveyed v2.4.2 encoder never produces, but its semantic (likely a coded-vs-display-width distinction) is an unresolved open question — so the decoder must stay robust rather than reject or mis-size. New `FrameHeader::coded_width()` returns the `+0x18` field and `FrameHeader::width_fields_agree()` reports whether it matches the authoritative `+0x10` width; both are diagnostic, never rejection triggers (geometry is derived from `+0x10` alone). New `decoder_tolerates_width_extra_mismatch` mutates a valid M8RG / M0RG / M8Y0 frame's `+0x18` field and proves decode still reconstructs bit-exact while the accessor flags the divergence; two header-unit tests pin the accessor on the canonical fixture (agree) and on a synthesised mismatch (disagree, still parses)
- **pin the on-wire raw-slice byte sizes to the `spec/05` §4.1 packing formula** at every bit-depth tier — the formula `05` §10 open-question 2 flags as "not behaviourally exercised by the fixtures surveyed". The existing raw-mode round-trips prove the packer and unpacker are mutual inverses, but a pair that agreed on a *wrong* field width (e.g. byte-aligning each 10-bit sample to 16 bits) would still round-trip; the absolute on-wire byte count is the only invariant that ties the layout to the documented `bits`-bit MSB-first packing. Two new tests read the slice-offset table directly and assert each raw slice payload spans exactly `2 + (rows·w·bits + 7) / 8` bytes: `high_bit_depth_raw_slice_byte_sizes_match_spec_4_1` reproduces the §4.1 table (M0RG 28-row = 2242 B / 8-row tail = 642 B; M2RG 2690 / 770; M4RG 3138 / 898) on 64×64 `slice_height=28` frames, and `eight_bit_raw_slice_byte_sizes_match_spec_4_1` reproduces the §4.1 *behavioural confirmation* (M8RG 1794 B full / 514 B tail). Both exclude the whole-frame final slice (it carries `spec/02` §8 even-byte padding) and still assert the bit-exact round-trip. This is the first verification of open-question 2's HBD bit-packing against a documented byte count rather than self-consistency alone
- **refresh stale module/field doc comments** left over from the staged round-1→round-2 bit-depth rollout. `header.rs`'s module preamble + `FLAG_INTERLACED` doc claimed the interlaced field-stride path was "deferred beyond round 1" / "only the progressive path is implemented" and `error.rs`'s `UnsupportedFormatByte` + `tables.rs`'s `Family` doc described the 10/12/14-bit families as "deferred to a later round" / "gated at the dispatch site" — all three contradict the now-complete implementation (interlaced field-stride=2 decode+encode and the full 8/10/12/14-bit native FOURCC set both ship and round-trip bit-exact). Doc-only; no behaviour change.
- **pin the `spec/04` §4.4 bit-depth-conditional Median formula directly against the spec's worked examples**. The Median predictor is the subtlest rule in the codec: the 8-bit path uses the **modular** gradient `(left + top - top_left) & 0xff` and the 10/12/14-bit path uses the **full-precision JPEG-LS** clip `clip(left + top - top_left, min, max)`. The two diverge only when the raw gradient falls outside `[0, 2^bits)`. The existing round-trip sweeps prove self-consistency but **cannot distinguish the two formulas** — an encoder/decoder pair round-trips with *either* rule — so a regression that swapped the 8-bit path to standard JPEG-LS (or the HBD path to modular) would slip past every round-trip test. New `median_formula_spec_4_4` unit module asserts absolute reconstructed pixel values: the §4.4 8-bit worked example (`left=10,top=20,top_left=200` ⇒ modular `86` clipped to `20`, vs JPEG-LS `10` — a divergence point); a genuine 14-bit modular-vs-JPEG-LS divergence (`left=2,top=3,top_left=16380` ⇒ JPEG-LS `2`, modular would give `3`); the §4.4 14-bit clip-on-underflow rule (scaled in-range since the spec's illustrative `24576/16384` values exceed `0x3fff`); all three JPEG-LS branches at every HBD mask; and the §4.2–4.4 **column-0 fallback** (`spec/04` §8 open-question 1's least-observed path) showing a non-zero column-0 residual reconstructs `(top + res)` identically for Left / Gradient / Median at both 8-bit and HBD. 5 new tests
- **cover the §6 slice-height guard at the registry encoder tier**. The registry `Encoder` resolves the `slice_height` option (`0` → full frame, otherwise clamped to the frame height) and forwards it to `encode_frame`; the new guard therefore reaches a registry caller too. A new `registry_encoder_rejects_odd_slice_height_on_420` test proves the registry encoder surfaces the rejection *cleanly* (a `CoreError` from `send_frame`, never a panic) for odd `slice_height ∈ {7,9,13}` on M8Y0, and still round-trips every even value `{0,2,6,14}` bit-exact — closing the guard's coverage across all three API tiers (direct `encode_frame`, `decode_frame`, and the registry `Encoder`)
- **reject a `slice_height` indivisible by `sub_y` on 4:2:0 families, in both directions** (`spec/02` §6). The §6 chroma row-partition reuses the **luma** slice count `slices_per_plane = ceil(height / slice_height)` and tiles the chroma plane with `chroma_psh = slice_height / sub_y` (floor). When `sub_y` does not divide `slice_height` cleanly, `slices_per_plane × chroma_psh` floors *below* the chroma height — the bottom chroma rows fall outside every slice and were silently emitted zero-padded (encoder accepted the frame, decoder reconstructed a partly-zero chroma plane: a genuine round-trip-breaking bug, e.g. `slice_height = 7` on a 14-row M8Y0 dropped chroma row 6). The v2.4.2 encoder only ever writes `slice_height = 28` (divisible by every native `sub_y ∈ {1, 2}`), so the indivisible case is out-of-spec / malformed. Both the direct `encode_frame` API and `decode_frame` now reject it with the new `Error::SliceHeightNotDivisibleBySubsampling`, the same defensive posture as the existing `OddDimensionForSubsampling` guard, so the encoder never emits a stream it cannot itself round-trip and the decoder never silently drops chroma rows from a hostile header. Inert for `sub_y == 1` (RGB / Gray / 4:4:4 / 4:2:2 — odd `slice_height` stays legal there and still round-trips). Four new tests: encoder rejection across M8Y0 + M0Y0 × {7,9,13}; an `odd_slice_height_ok_when_sub_y_is_one` positive sweep proving the guard targets only vertical-subsampled families; a decoder-side header-mutation rejection (`+0x1c` slice_height field set to 7); and `even_slice_height_420_tiles_chroma_completely` proving every permitted even value 2..=14 on a 14-row 4:2:0 frame still tiles the chroma plane and round-trips bit-exact
- **attach a content probe to the codec registration** (`CodecInfo::probe`) so a FourCC claim can be validated against the actual bitstream (`spec/01` §1 — every v7 frame opens with the 4-byte `MAGY` magic). With no peeked bytes (the common case: demuxers resolve at stream-discovery time before any packet exists) the probe returns a high `0.9` so the FourCC match still wins. When a first packet (or container header blob) *is* available the magic is decisive: `MAGY` → `1.0`, anything else → `0.0`, so a mis-tagged foreign stream carried under one of our 17 FourCCs is no longer claimed. Two new tests: per-input confidence values and an end-to-end resolve (valid MAGY stream → magicyuv; foreign packet → unresolved)
- **set `CodecParameters::pixel_format` in `output_params()`** so a muxer/pipeline knows the encoder's output plane layout without re-deriving it from the FourCC. The new `pixel_format_for` helper maps each native FOURCC to the exact `oxideav-core` `PixelFormat` the registry decoder *emits*: 8-bit RGB/RGBA → interleaved `Rgb24`/`Rgba`; 10/12/14-bit RGB → planar `Gbrp*Le`/`Gbrap*Le` (`spec/03` §4 GBR wire order); YUV → `Yuv{444,422,420}P{,10Le,12Le}`; Gray → `Gray{8,10Le,12Le}`. Returns `None` for the one FOURCC with no exact core variant (`M8YA`, YUVA 4:4:4:4 8-bit — there is no `Yuva444P`), leaving `pixel_format` unset rather than mislabelling it. New `output_params_set_exact_pixel_format` pins all 17 mappings
- **expose `slice_height` as a registry encoder option** (`spec/02` §4 slice-table partition). The registry encoder previously hard-coded one full-frame slice per plane; the new `slice_height` u32 option (default `0` = single slice) partitions each plane into `ceil(height / N)` slices, clamped to the frame height (a value taller than the frame degenerates to one slice, and `0` avoids the `div_ceil(0)` trap). The decoder reconstructs identically regardless of the partition, so this only varies the wire slice layout. New `registry_encoder_multi_slice_round_trips` proves bit-exact recovery on M8Y4 32-row frames across `slice_height ∈ {0,1,8,12,32,64}` (single-slice, per-row, multi-slice with a partial tail, exact-fit, and over-height clamp)
- **accept interleaved 8-bit RGB / RGBA at the registry encoder**, closing the decode→re-encode loop for these families. The registry decoder emits 8-bit RGB / RGBA as a *single interleaved* `R,G,B[,A]` plane (a display convenience); the encoder previously only accepted the planar `G,B,R[,A]` layout, so a registry decode→re-encode of an RGB stream failed on plane count. The encoder now detects a single interleaved plane for the 8-bit RGB/RGBA families and de-interleaves it back to planar (the exact inverse of `map_to_video_frame`'s interleaving), while still accepting planar input as before. New `registry_8bit_rgb_interleaved_reencode_round_trip` proves a full decode→re-encode→decode fixed point for M8RG + M8RA (planar input still covered by the all-FourCC sweep)
- **surface typed encoder options through `CodecParameters::options`** — `MagicYuvEncoderOptions` (a `CodecOptionsStruct` with `Enum`-validated SCHEMA, registered via `CodecInfo::encoder_options`) lets a pipeline pick the encode strategy by string key: `predictor` ∈ {`left`,`gradient`,`median`,`dynamic`} (`spec/04` §3 / §4.2–4.4), `slice_mode` ∈ {`huffman`,`raw`,`auto`} (`spec/05` §6.2), and `interlaced` (bool, `spec/04` §5.1). Defaults reproduce the previous hard-coded `EncodeOptions::dynamic_auto()` (dynamic predictor + auto slice mode + progressive), so the zero-config path is unchanged. `make_encoder` parses the bag once at construction (strict: unknown keys / bad enum values rejected). Six new tests: per-predictor + per-slice-mode mapping, default equivalence, schema rejection, a 4×3×2 framework-encoder round-trip honouring every option combination on M8Y0 (4:2:0, exercising the field-stride path on a subsampled family), and the registered-schema surface check
- **register a framework-level `Encoder`** (`oxideav_core::Encoder`) alongside the existing decoder, so a pipeline can drive MagicYUV *encoding* through the registry — not just decoding. The crate already shipped a full clean-room `encode_frame`; this wires it to `CodecInfo::encoder` + `CodecCapabilities::with_encode()`. The factory recovers the native FourCC (one of the 17 v7 codes, `spec/01` §4.1) from `CodecParameters::tag` (rejecting input-only `§4.2` FourCCs / `WaveFormat` tags / missing geometry), and the `MagicYuvEncoder` consumes planar `Frame::Video`s — one `VideoPlane` per codec plane in the family order the decoder *emits* (`G,B,R[,A]` / `Y,U,V[,A]` / single `Y`), unpacking LE-`u16` bytes for the 10/12/14-bit families (`spec/03` §7.3). Output packets are flagged keyframes (intra-only) and propagate the source `pts`. Emits one slice per plane (full-height `slice_height`, always valid per `spec/02` §4). New `registry_encode_decode_round_trip_all_fourccs` test proves the framework `Encoder` → framework `Decoder` path reconstructs bit-exact for **every** native FourCC across all families / bit-depths / subsamplings; plus factory-rejection, output-params, plane-count/size-mismatch, and send-before-receive guards (11 registry tests total, up from 5)
- seed the **`decode_magicyuv` fuzz corpus with 55 minimal-geometry frames** (11 representative FOURCCs across every family / bit-depth tier × 5 degenerate variants: `1×1` Huffman-Left, single-column Huffman-Median, single-row raw-Gradient, 2-row interlaced, and odd `3×5` Dynamic+Auto). The existing curated corpus is uniformly `12×12`; these boundary shapes give the daily fuzz workflow (cron `fuzz.yml`) degenerate starting points to mutate — the narrowest raw slice (one bit-packed sample), the single-symbol under-full Huffman book (`spec/05` §2.1), the interlaced `rows ≤ field_stride` header-rows early-return (`spec/04` §5.1), and the single-column first-column-only predictor path. All 55 are emitted by the crate's own `encode_frame` (no external tooling) and verified to decode cleanly. Complements the deterministic `minimal_geometry_*` roundtrip sweeps with mutation-based boundary coverage
- add a **minimal-geometry `dynamic_auto()`** sweep (`minimal_geometry_dynamic_auto_all_fourccs`) — the combined-strategy complement of the fixed-predictor minimal sweep below. `EncodeOptions::dynamic_auto()` runs per-slice predictor selection by minimum residual L1 norm *and* per-slice raw/Huffman byte-budget selection, both of which degenerate at tiny sizes the 64×64 `dynamic_plus_auto_round_trips_combined` never reaches: the L1-score scan over a 1×1 / single-column / single-row slice (all candidate predictors tie or collapse to the same first-column term, so the tie-break path is exercised) and the Auto byte-budget at a tiny payload (a 1×1 plane's Huffman descriptor overhead dwarfs its single-pixel payload, so Auto falls back to raw — the narrowest possible raw slice, bit-packed at `bits` bits/sample for HBD per `spec/05` §4.1). All 17 FOURCCs × {1×1, 1-col, 1-row, 2×2, 3×5 minimal-lattice} × interlaced on/off = 170 cells, each filled by the seeded scrambler, round-trip bit-exact. Verified empirically clean first; the sweep locks the dynamic/auto decision arithmetic at degenerate sizes against regression
- add a **minimal-geometry** property sweep (`minimal_geometry_property_sweep_all_fourccs`) — the boundary complement of the 16×16-and-up `cartesian_property_sweep_all_fourccs_predictors_modes`. Every dimension is the smallest the FOURCC's chroma subsampling admits (`spec/03` §8.2: subsampled planes need `width % sub_x == 0` / `height % sub_y == 0`), so the cases reach down to `1×1` for RGB/Gray and `sub_x × sub_y` for the 4:2:x families. These geometries exercise predictor / Huffman boundaries the larger sweep never hits: a **single column** (`width == sub_x`) where every data-row pixel has only the top neighbour so the `c == 0` first-column predictor arm is the only one that runs; a **single row** (`height == sub_y`) which is one header row and short-circuits `apply_*_with_stride` before any top-neighbour read; a **`1×1`** lone sample where both predictor loop bodies are skipped and the Huffman descriptor degrades to a single-symbol (under-full, Kraft=1/2) book (`spec/05` §2.1); and **interlaced on a `≤ field_stride`-row plane** where `rows ≤ header_rows` makes `apply_*_with_stride` take the header-rows early-return (`spec/04` §5.1 "first two rows have no top neighbour") rather than underflow `r - 2`. All 17 FOURCCs × {1×1, 1-col, 1-row, 2×2, 3×5} × 3 predictors × 2 modes × interlaced on/off × 2 seeds = 2040 cells, each filled by the seeded `(seed, plane, r, c)` scrambler so multi-pixel cases see a non-trivial Huffman histogram. Empirically these all already round-trip bit-exact; the sweep locks that boundary behaviour against any future regression in predictor first-column / first-row / interlaced-header arithmetic. Failures print the FOURCC + geometry + seed for replay
- pin the malformed-input rejection guards on the **high-bit-depth** (10/12/14-bit) decode slice loop directly (`high_bit_depth_malformed_input` test module, all 10 HBD FOURCCs). The pre-existing decoder-error tests (`out_of_range_plane_index_rejected`, `over_quota_plane_index_rejected`, `rejects_*_predictor_id`) all ran against 8-bit (`0x65`) fixtures, exercising the guards inside `decode_eight_bit`. The `decode_high_bit_depth` slice loop has its **own** copies of those guards with **different arithmetic** — most importantly the raw-mode payload-length check is `2 + (pixels * bits).div_ceil(8)` (bit-packed at `bits` bits/sample, spec/05 §4.1) versus the 8-bit path's `2 + pixels` (one byte/sample) — so a regression loosening the HBD guard would slip past every 8-bit test. New cases trip, on the HBD branch: raw-mode `SliceTruncated` (final-slice payload short of the bit-packed byte count), `SlicePrefixMissing` (final slice cut to one byte), `BadPredictorId(0x7f)` (out-of-range predictor id), and `BadPredictorId(0x00)` (reserved id, spec/04 §1.2 + §7.3c). Each derives the final on-wire slice offset from the v7 slice table and asserts the exact error variant per FOURCC; the HBD `HuffmanIncomplete` / unused-codespace path stays covered by the `decode_into_u16_underfull_unused_slot_flags_invalid` unit test
- add a full cartesian encode→decode self-roundtrip property sweep (`cartesian_property_sweep_all_fourccs_predictors_modes`) proving bit-exact lossless recovery across the **entire** advertised input space: all 17 native FOURCCs (7 8-bit + 10 high-bit-depth families) × all 3 predictors (Left / Gradient / Median) × both slice modes (Huffman / Raw) × 3 dimension/slice-height triples (single-slice, 3-slice with partial last slice, 4:2:0 partial-chroma) × 4 distinct seeds, with interlaced field-stride=2 prediction alternated across seeds — 1224 cells total. Each cell's source planes are filled by a seeded SplitMix64 scrambler over `(seed, plane, r, c)` so the sweep exercises the Huffman descriptor / canonical-code build over high-entropy near-flat codebooks and the predictor LSB/sign handling over arbitrary residuals, complementing the existing per-feature tests that each pin one dimension+pattern combo. Failures are bit-for-bit replayable from the printed seed
- detect under-full (Kraft < 1) Huffman descriptors at decode time and reject a slice whose bitstream peeks into the unused codespace with the new `Error::HuffmanIncomplete` (spec/05 §2.1 + §10 Q1). The table constructor keeps *accepting* under-full books — the encoder legitimately produces them for single-symbol planes (e.g. an all-zero-residual plane) and the vendor binary's constructor accepts them too, so conformant streams (which only ever peek the assigned codes' prefixes) round-trip unchanged. A malformed slice that lands on a zero-init `(symbol 0, length 0)` flat-table slot previously consumed no bits and silently mis-decoded; the batched `decode_single_level_into_u8` / `decode_into_u8` / `decode_into_u16` paths now fold a per-symbol `len != 0` check and signal the hit so the decoder surfaces it, complementing the build-time `HuffmanOverfull` guard
- pin EOF/truncation robustness of the two-level (10/12/14-bit) `decode_into_u16` Huffman hot loop — sibling of the single-level truncation pin from the previous round: a truncated high-bit-depth Huffman slice must decode the full geometry-derived symbol count without panicking and zero-pad past EOF (spec/05 §3.3), with the batched two-level loop agreeing byte-for-byte with the per-pixel `decode()` reference on the same short buffer
- fix subtract-with-overflow panic in the single-level Huffman decode hot loop on truncated slices: the slow-path EOF refill stopped after one zero pad byte (`fill` as low as 8), violating the ≥57-bit post-refill invariant a 12-bit code's `fill -= len` relies on; now zero-pads `fill` above 56 past EOF without advancing the cursor (spec/05 §3 zero-pad semantics unchanged)
- extend the `decode_magicyuv` fuzz harness to also drive the streaming `decode_into` buffer-reuse path across two sub-frames through one reused `DecodedFrame` (geometry-match short-circuit, in-place `Vec::resize`, partial-decode-after-`Err` reuse)
- pin end-to-end rejection of reserved `predictor_id = 0x00` (spec/04 §1.2 + §7.3c malformed-prefix range), complementing the existing ≥0x04 case

## [0.0.6](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.5...v0.0.6) - 2026-06-15

### Other

- lock in spec/02 §4+§6 chroma slice partition (even non-28 slice_height, partial last chroma slice)
- reject odd subsampled dimensions, symmetric with decoder (spec/03 §8.2)
- honour on-wire per_slice_plane_index ordering (spec/02 §7.3)
- batched single-level Huffman decode hoists per-symbol refill out of 8-bit hot loop
- cap Huffman descriptor RLE runs at 255 reps (spec/05 §1.5 / §10 Q3)
- spec/01 §3.1 RGB-family flags override (keep-mask 0xf1903f clears FLAG_FULL_RANGE)
- batched Huffman-mode bit packer + packed (code,length) table
- drop release-plz.toml — use release-plz defaults across the workspace
- full_range knob (spec/01 §3.1 FLAG_FULL_RANGE / registry +0x78)
- ColorMatrix nibble knob (EncodeOptions::color_matrix → flags bits 20..23)
- typed ColorMatrix nibble accessor + FLAG_COLOR_MATRIX_MASK/SHIFT constants
- add huffman_descriptor target driving parse_lengths + HuffmanTable::build directly
- batched raw-mode bit packer (one drain per pixel, hoisted state)
- per-plane scratch reuse in build_slice_residuals_u{8,16}_into
- BitWriter direct parity tests + r217 to_be_bytes drain candidate closed
- batched raw-mode high-bit-depth unpacker (one refill per ~floor(56/bits) pixels)
- profile_magicyuv samply-friendly flat-loop driver
- encoder strategy × mode × interlaced matrix (Dynamic + Auto coverage)

### Tests

- **Lock in the `spec/02` §4 + §6 chroma slice-partition rule for an
  even, non-28 `slice_height` with a partial last chroma slice.** The
  existing 4:2:0 roundtrip used the v2.4.2-constant `slice_height = 28`
  (chroma per-slice height `28/2 = 14`, tiling cleanly); the new
  `yuv_4_2_0_partial_chroma_last_slice_even_non28_slice_height` case
  (M8Y0 64×54, `slice_height = 10`) makes the documented last-slice
  `.min(chroma_height)` clamp load-bearing: the luma row count drives
  `slices_per_plane = ceil(54/10) = 6`, while the 27-row chroma plane
  is partitioned into rows [0,5) [5,10) [10,15) [15,20) [20,25)
  [25,30→27) — six slices summing to the full chroma height with a
  partial final slice. Covered across all three predictors (Left /
  Gradient / Median) and both per-slice modes (Huffman / Raw); decoder
  and encoder derive the partition identically through
  `plane_geom` / `plane_dims_for`.

### Fixed

- **`encode_frame` rejects odd dimensions that don't divide the
  FOURCC's chroma subsampling factor (`spec/03` §8.2), symmetric with
  the decoder.** The decoder has refused odd subsampled dimensions
  since round 2 (the ceiling-vs-floor chroma rounding rule at odd
  resolutions is an unverified open question in the spec), returning
  `OddDimensionForSubsampling`. The encoder, however, silently floored
  `width / sub_x` (resp. `height / sub_y`) in `plane_dims_for`, so an
  odd-width 4:2:2 / odd-height 4:2:0 request either dropped the last
  chroma column/row or — if the caller supplied a ceil-sized chroma
  plane — failed with a confusing `EncoderInputMismatch`. Worse, when
  it did succeed it produced a stream the decoder then rejected. The
  encoder now performs the same `OddDimensionForSubsampling` check at
  the top of `encode_frame`, before any plane-length validation, so
  the two sides accept exactly the same dimension set. Even dimensions
  (`ceil == floor`) are unaffected. The `OddDimensionForSubsampling`
  doc comment, which claimed the encoder already disallowed odd
  dimensions, is now accurate. Four tests cover odd-width / odd-height
  rejection, the even-dimension round-trip, and encoder↔decoder
  rejection symmetry across the subsampled FOURCCs.
- **`FrameHeader::is_interlaced` doc comment corrected.** It claimed
  interlaced prediction was deferred and that callers touching it
  would get a rejection error; interlaced field-stride=2 prediction
  has shipped on both the decoder and encoder for several rounds and
  round-trips byte-for-byte. The comment now describes the actual
  behaviour.
- **Decoder honours the on-wire `per_slice_plane_index` mapping
  instead of assuming plane-major order (`spec/02` §7.3).** The
  preamble carries one `per_slice_plane_index` byte per slice naming
  the plane that slice belongs to; `spec/02` §7.3 states a
  spec-compliant decoder MUST read this mapping and NOT assume the
  plane-major ordering the v2.4.2 encoder happens to emit ("the
  encoder's freedom to interleave is preserved by the table format").
  The decoder previously parsed the bytes only to *reject* any frame
  whose mapping deviated from the computed `s / slices_per_plane`,
  returning a `Truncated{"… not plane-major"}` error — so a valid
  interleaved stream (e.g. round-robin slice ordering across planes
  for parallel emit) was refused. The slice loop now derives each
  slice's `(plane, in_plane_idx)` from the mapping byte plus a running
  per-plane appearance counter: the k-th slice naming plane `p` covers
  plane `p`'s row block `[k·plane_slice_height, …)` regardless of its
  position in the global slice order. The file-offset bounds still
  come from the slice table in global order (`entry[s+1]..entry[s+2]`),
  so payload extraction is unchanged. A mapping byte outside
  `[0, num_planes)` or one that over-fills a plane's
  `slices_per_plane` quota is rejected via the new
  `Error::BadPlaneIndex` variant (replacing the prior plane-major
  rejection). Plane-major streams — every frame the bundled encoder
  produces — decode byte-identically to before (the running counter
  reproduces `s / slices_per_plane` exactly for plane-major input).
  Six new lib tests (`per_slice_plane_index_ordering` module) re-emit
  encoder-produced frames with permuted global slice orders that keep
  each plane's within-plane slice order — round-robin RGB-8bit /
  YUV-4:2:0-8bit / RGB-10bit, reverse-plane-order RGBA-8bit — and
  assert pixel-exact decode parity with the plane-major source, plus
  out-of-range and over-quota mapping bytes both surfacing
  `BadPlaneIndex`. Lib test count 127 → 133 (128 under
  `--no-default-features`, 137 under `--all-features`); clippy clean;
  fmt clean.

- **Huffman descriptor run-length writer caps runs at 255 reps
  (count byte `0xfe`) per `spec/05` §1.5 / §10 Q3.** The encoder's
  `encode_descriptor` previously allowed a single two-byte run to
  carry up to 256 repetitions, emitting the reserved count byte
  `0xff`. The v2.4.2 encoder caps each run at 255 reps — count byte
  `0xfe` (the run-count cap branch at `magicyuv.dll!0x69b94600`,
  two-byte run-emit at `0x69b946d6`), reserving `0xff` with one unit
  of headroom. A length value that repeats `≥ 256` times now splits
  into successive `(0x80|v, 0xfe)` pairs followed by the remainder,
  matching the vendor's descriptor bytes byte-for-byte. Most visible
  on the high-bit-depth (10/12/14-bit, N ∈ {1024, 4096, 16384})
  plane descriptors where a single length value commonly repeats
  thousands of times (`spec/05` §10 Q3's `01 89 fe 89 fe …`
  10-bit-sparse shape). Round-trip is unchanged — the decoder's
  `parse_lengths` already accepts both forms — but vendor-byte
  parity on RLE-heavy descriptors is now exact.

- **Encoder applies the `spec/01` §3.1 RGB-family flags override
  (keep-mask `0xf1903f`).** The v2.4.2 encoder's post-accumulation
  override at `magicyuv.dll!0x69b9769c`–`0x69b976bb` computes the
  biased index `format_byte - 0x67`, reaches the override block
  unconditionally when it exceeds `0x17` (the unsigned-compare
  fallthrough that routes 8-bit RGB `0x65` / `0x66` there), and
  otherwise tests `1 << biased` against the mask `0xf1903f` — the
  YUV/Gray-family format bytes `{0x67, 0x68, 0x69, 0x6a, 0x6b,
  0x6c, 0x73, 0x76, 0x77, 0x7b, 0x7c, 0x7d, 0x7e}` for which the
  override does NOT fire. For every other format byte (the RGB
  family at every bit depth) the override clears flags bit 2
  (Full-range YUV, `FLAG_FULL_RANGE`) and forces the codec_variant
  byte at header `+0x0b` to `0x02`. Our `write_header` previously
  OR-ed `FLAG_FULL_RANGE` for any FOURCC when
  `EncodeOptions::full_range` was set — a wire deviation from the
  v2.4.2 encoder on RGB-family streams. The override is now
  materialised after the OR-accumulation (the codec_variant force
  is already unconditional per the `spec/01` §3.0 v2.4.2 clamp at
  `magicyuv.dll!0x69ba9060`, so only the flags-bit-2 clear is new);
  it touches bit 2 only — Interlaced (bit 1) and the ColorMatrix
  nibble (bits 20..23) survive untouched, and the pixel bytes are
  unaffected (the override is header-only; the lossless residual
  path never reads the bit). Default-options output is
  byte-identical to the prior round for every FOURCC
  (`full_range` defaults to `false`, making the clear a no-op).
  Two new family-sweep round-trip tests pin both sides: the
  RGB-family sweep ({`0x65`, `0x66`, `0x6d`, `0x6e`, `0x6f`,
  `0x70`, `0x71`, `0x72`} — all eight published RGB/RGBA FOURCCs)
  authors all three flags knobs and asserts bit 2 is clear on the
  wire while Interlaced + the ColorMatrix nibble survive and the
  samples round-trip; the YUV/Gray sweep ({`0x67`, `0x68`, `0x69`,
  `0x6a`, `0x6b`, `0x6c`, `0x73`, `0x76`, `0x7b`} — all nine
  published keep-mask members) asserts the authored bit reaches
  the wire and reads back via `is_full_range()`. The two existing
  `full_range` knob tests moved from M8RG (an override-path FOURCC,
  where asserting bit-2-set contradicted the spec) to M8Y0 (a
  keep-mask member) in the same commit. Lib test count grew from
  122 to 124 (119 under `--no-default-features`, 128 under
  `--all-features`); clippy clean; fmt clean.

### Changed

- **Batched single-level Huffman decode — per-symbol refill branch
  hoisted out of the 8-bit hot loop (`BitReader::decode_single_level_into_u8`).**
  The 8-bit decode hot loop in `HuffmanTable::decode_into_u8` ran
  `peek_bits` + `consume` per symbol, and `consume` ends with an
  unconditional `refill()` that re-evaluates its guard
  (`fill <= 56 && pos + 8 <= len`) on *every* symbol — even though a
  productive refill only happens once every `floor(56 / primary_bits)
  ≈ 4-5` symbols (each native 8-bit code is ≤ `primary_bits ≤ 12`
  bits, and a topped-up 64-bit accumulator holds ≥ 56 valid bits).
  The new `BitReader::decode_single_level_into_u8` keeps `acc`/`fill`
  in stack-local registers and only calls `refill` when
  `fill < primary_bits`; the inner per-symbol step is a pure
  shift-pair plus one table fetch with no function call, no `pos`
  arithmetic, and no refill branch on the ~4-of-5 symbols that don't
  need one. The byte cursor + EOF zero-pad behaviour is delegated to
  the existing `refill` (via the spilled field state), so the
  observable bit stream is byte-identical. Every native 8-bit FOURCC
  (`max_len ≤ 12 = PRIMARY_BITS`) routes here; the 10/12/14-bit
  two-level `decode_into_u16` path is untouched. Decode wall time
  improves ~24-29 % on the three large 8-bit scenarios (M8RG/M8Y0
  gradient 1280×720, M8G0 left 1920×1080) and ~13 % on the small-frame
  8-bit Median scenario (`examples/profile_magicyuv decode`, 5-pass
  interleaved A/B medians); the 10-bit M0RG scenario is flat (the
  change does not touch its path). Bit-identity is pinned by the
  existing `decode_into_u8_matches_per_pixel_decode` parity test plus
  all 60 round-trip tests exercising 8-bit Huffman decode end-to-end,
  and verified by a whole-corpus FNV-1a digest over the 102
  `fuzz/corpus/decode_magicyuv/*.magy` fixtures' reconstructed plane
  samples (`47e29dec388ea4ed` identical before and after, 102/102
  decoded). Lib test count unchanged at 127 (default features), 131
  under `--all-features`; clippy clean; fmt clean.

- **Batched Huffman-mode bit packer
  (`encoder::pack_huffman_residuals_u{8,16}`) + packed
  `(code, length)` table (`PlaneHuff::packed: Vec<u32>`,
  low 8 b length + high 24 b code).** The four per-symbol
  Huffman emit hot loops (u8 + u16 × Auto-probe + Huffman-emit)
  previously walked `for &sym in res_block { bw.write(huff.codes[sym
  as usize], huff.lengths[sym as usize]); }` — two `Vec` table
  fetches per symbol + a `&mut BitWriter` per-call reload that
  blocked the optimiser from keeping accumulator state in
  registers. The new batched packer mirrors the prior round's
  raw-mode `pack_raw_bits_from_u16` shape: stack-local
  `acc: u64` + `bits_used: u32` across the whole slice, one
  packed-`u32` fetch per symbol (length in low 8 b, code in high
  24 b — safe because `max_huff_len ≤ 18 < 24`), and the per-slice
  `BitWriter::with_capacity + finish + payload.extend(...)`
  byte-copy hop is gone (the Huffman-emit packer writes directly
  into the pre-sized `payload: Vec<u8>`; the Auto-probe site keeps
  a fresh `Vec` so it can compare the would-be Huffman size
  against `raw_size` without committing). 4-6 % win on the
  Dynamic-Auto encode scenarios (per-symbol Huffman fires twice
  per Huffman-winning slice — once for the size probe, once for
  the emit) and 2-4 % on the Fixed-Huffman scenarios on the Apple
  M-series host (`examples/quick_bench encode` + `dynamic`, 5-run
  medians, full table in `BENCHMARKS.md` §13). Same observable
  byte stream as the prior per-symbol shape — pinned by seven new
  `pack_huffman_residuals_tests` (`empty_input_emits_no_bytes`,
  `appends_to_existing_buffer`, `matches_per_pixel_u8_xorshift`,
  `matches_per_pixel_u16_at_10_12_14`,
  `skewed_histogram_matches_reference`,
  `packed_layout_matches_codes_and_lengths`,
  `unused_symbols_are_skipped`) against a per-pixel
  `BitWriter::write(huff.codes[sym], huff.lengths[sym])` reference
  oracle, and anchored end-to-end by the existing round-trip suite
  (every FOURCC × predictor × Huffman / Auto configuration). Lib
  test count grew from 115 to 122 (117 under
  `--no-default-features`, 126 under `--all-features`); clippy
  clean; fmt clean. Raw-mode payload bytes are unchanged (the
  `SliceMode::Raw` site keeps routing through `pack_raw_bits_*`).

### Added

- **Encoder-side `EncodeOptions::full_range` knob (`spec/01`
  §3.1).** The encoder's `write_header` now accepts a boolean
  `full_range` mirroring the v2.4.2 encoder's `FullRangeYUV`
  registry value at context offset `+0x78`, OR-accumulated as
  `FLAG_FULL_RANGE` (bit 2, mask `0x00000004`) of the flags dword
  per the documented OR-accumulator sequence at
  `magicyuv.dll!0x69b97647`–`0x69b9767a`. The decoder pickup at
  `magicyuv.dll!0x69bae311` (file `@0x2d311`) shifts the dword
  right by 2 and isolates the low bit, exposing the boolean to the
  application/conversion layer; the round-1
  `FrameHeader::is_full_range()` accessor surfaces the same value
  on the decode side. The codec layer's pixel residuals are
  independent of the bit, so the wire pixel bytes returned by
  `decode_frame` round-trip byte-exact regardless of the authored
  value. `EncodeOptions::default()` / `EncodeOptions::fixed(_)` /
  `EncodeOptions::dynamic_auto()` all initialise the field to
  `false`, matching the r245-era encoder behaviour byte-for-byte so
  callers using struct-update syntax keep producing identical
  output. The `encode_magicyuv` fuzz target gains coverage of the
  new knob via bit 1 of header byte 8 (bit 0 stays `interlaced`,
  high nibble stays `color_matrix`), exercising both values at
  every libfuzzer iteration alongside the existing two flags-dword
  knobs. Three new lib tests cover the knob: a true/false sweep
  asserting (a) the bit round-trips via `is_full_range()`, (b) the
  on-wire flags dword carries exactly `FLAG_FULL_RANGE` when set,
  and (c) the pixel bytes round-trip unchanged; a three-way
  composition test setting `interlaced = true` + `full_range =
  true` + `color_matrix = 0xa` simultaneously and asserting all
  three flag groups survive the OR-accumulation; and a defaults
  invariant test asserting each of the three constructor entry
  points clears `full_range` and that a default-options encode
  emits a clear `FLAG_FULL_RANGE` bit. Lib test count grew from
  112 to 115; clippy clean; fmt clean.
- **Encoder-side `EncodeOptions::color_matrix` knob (`spec/01`
  §3.1).** The encoder's `write_header` now accepts a 4-bit
  ColorMatrix nibble mirroring the v2.4.2 encoder's `ColorMatrix`
  registry value at context offset `+0x68`, OR-accumulated into
  the flags dword at bits 20..23 (mask `0x00f00000`) per the
  documented OR-accumulator sequence at
  `magicyuv.dll!0x69b97647`–`0x69b9767a`. The implementation
  honours the spec's matrix-skip sentinel: when
  `EncodeOptions::color_matrix == 1` the OR step is bypassed and
  flags bits 20..23 stay clear, matching the r242-era encoder
  behaviour byte-for-byte. Any other 0..=15 value (the field is
  masked low-nibble before the shift, so 16..=255 wraps without
  affecting other bits) lands the nibble in flags bits 20..23 for
  recovery via the round-1
  `FrameHeader::color_matrix_nibble()` accessor. The matrix knob
  is orthogonal to the lossless residual path: across the full
  0..=15 sweep on M8RG 32×16 the pixel bytes round-trip byte-exact
  to the decoder, so the codec layer carries the nibble strictly
  as a header-level annotation for downstream colour conversion
  (the GUI in `reference/vendor/changelog.md` v0.9.2-beta exposes
  Rec.601 and Rec.709). `EncodeOptions::default()` /
  `EncodeOptions::fixed(_)` / `EncodeOptions::dynamic_auto()` all
  carry the matrix-skip sentinel so r242-era callers using struct-
  update syntax (`..EncodeOptions::default()`) keep producing
  byte-identical headers. The `encode_magicyuv` fuzz target gains
  coverage of the new knob via the high nibble of header byte 8
  (low bit kept for `interlaced`), so the full 0..=15 range of
  `color_matrix` is now exercised at every encoder iteration of
  the libfuzzer run. Three new lib tests cover the knob: a 0..=15
  sweep asserting (a) the nibble round-trips, (b) the on-wire
  flags dword equals `(nibble & 0xf) << 20`, and (c) the pixel
  bytes round-trip unchanged; a composition test setting
  `interlaced = true` + `color_matrix = 0xa` together and
  asserting both bit 1 and bits 20..23 survive the OR-
  accumulation; and a defaults invariant test asserting each of
  the three constructor entry points carries the matrix-skip
  sentinel and that a default-options encode emits a zero
  ColorMatrix nibble. Lib test count grew from 109 to 112; clippy
  clean; fmt clean.
- **Typed `FrameHeader::color_matrix_nibble()` accessor + paired
  `FLAG_COLOR_MATRIX_MASK` / `FLAG_COLOR_MATRIX_SHIFT` public
  constants (`spec/01` §3.1).** The v7 header's `flags` dword
  carries three documented bit groups: bit 1 (Interlaced,
  `FLAG_INTERLACED`), bit 2 (Full-range YUV, `FLAG_FULL_RANGE`),
  and bits 20..23 (ColorMatrix nibble, mask `0x00f00000`). The
  first two have been surfaced as typed `is_interlaced()` /
  `is_full_range()` accessors since round 1; the four-bit
  ColorMatrix nibble at bits 20..23 was only callable by hand-
  shifting `header.flags` against an ad-hoc literal. The new
  `color_matrix_nibble()` accessor extracts the raw 0..=15 value
  by masking with the new `FLAG_COLOR_MATRIX_MASK` public
  constant (`0x00f00000`) and shifting right by the new
  `FLAG_COLOR_MATRIX_SHIFT` public constant (20). The nibble is
  informational at the lossless codec layer — the wire bytes
  returned by `decode_frame` / `decode_into` are unchanged
  regardless — and is consumed by downstream colour-conversion
  (the GUI in `reference/vendor/changelog.md` v0.9.2-beta
  exposes Rec.601 and Rec.709; the wire layout reserves 16
  entries). The accessor returns 0 when the encoder's OR-
  accumulator at `spec/01` §3.1 skipped the matrix contribution
  (the encoder treats `ColorMatrix == 1` as the skip case), so a
  0 nibble cannot disambiguate "Rec.601" from "encoder
  matrix-skip path" — that distinction lives above the codec
  layer. Three new unit tests cover the accessor: a 16-way sweep
  shifting every 0..=15 value into bits 20..23 and confirming
  the accessor recovers it, an independence test setting
  Interlaced + Full-range + nibble = 0xa simultaneously to catch
  future mask drift between the three flag groups, and a
  constants invariant test asserting
  `FLAG_COLOR_MATRIX_MASK == 0xf << FLAG_COLOR_MATRIX_SHIFT`
  and that the nibble mask has zero overlap with
  `FLAG_INTERLACED` / `FLAG_FULL_RANGE`. The existing
  `parses_canonical_header_from_spec02_5_2` test gains an extra
  `color_matrix_nibble() == 2` assertion against the fixture's
  `flags = 0x00200000` value (the GUI's "Rec.709" registry value
  per `reference/vendor/changelog.md` v0.9.2-beta), plus a
  paired assertion that `is_interlaced() == false` and
  `is_full_range() == false`, so the canonical-header round-trip
  now exercises all three documented flag accessors instead of
  just the bit-by-bit `flags == 0x00200000` byte-pattern check.
  Public API addition only; no change to encoder / decoder /
  Huffman / predictor wire behaviour. Lib test count grew from
  106 to 109; clippy clean; fmt clean.
- **Third cargo-fuzz target `huffman_descriptor`.** Pushes arbitrary
  fuzz-supplied bytes straight into the public Huffman sub-surface
  (`huffman::parse_lengths` + `HuffmanTable::build` +
  `decode_into_u{8,16}`), bypassing the 32-byte v7 header / slice-table
  / preamble framing the full-frame `decode_magicyuv` target walks
  first. The full-frame harness only reaches `huffman::*` after a
  valid header has been parsed — most random byte sequences are
  rejected before they ever touch the canonical-Huffman code-builder.
  The new target concentrates fuzz pressure on `spec/05` §1.1
  run-length descriptor decode (literal / run-form alternation +
  `HuffmanLengthExceedsMax` rejection + `Truncated` rejection),
  `spec/05` §2.0 canonical-Huffman code construction (the
  audit-corrected longest-length-first cumulative accumulator + the
  `(1 << len) <= acc` Kraft check, with the `1u64 << len` cast at
  `len = max_length = 18` that the 14-bit tier exercises), and the
  two-level primary/secondary table arithmetic (`REDIRECT_MARKER`
  sentinel, per-prefix subtable allocation keyed by
  `code[s] >> (l - primary_bits)`, residual-bit spread). Successful
  builds drive `decode_into_u{8,16}` on the trailing bytes so the
  post-build `BitReader` peek/consume hot loop sees pressure too.
  Input layout: byte 0 = bit-depth tier selector (mod 4), bytes 1-2 =
  descriptor cap (LE u16, capped at 16 KiB), bytes 3.. = descriptor +
  trailing decode payload. Allocation cap stays tight (≤ 16 KiB
  primary table + ≤ 4096 secondary subtables × 64 entries + ≤ 8 KiB
  `out` for the decode-loop exercise) so no resource-request false
  positives compete with the logic-bug signal the harness is built to
  find. Local baseline ~830 k exec / 16 s = ~51 k exec/s, zero
  crashes. Auto-discovered by the `fuzz.yml` reusable workflow under
  `OxideAV/.github/.github/workflows/crate-fuzz.yml@master` (no
  workflow edit needed — the 1800-s daily budget now splits three
  ways across `decode_magicyuv` + `encode_magicyuv` +
  `huffman_descriptor`).
- **Batched raw-mode bit packer (`encoder::pack_raw_bits_from_u16`).**
  Encoder symmetry for the high-bit-depth raw-mode slice payload
  (`spec/05` §4.1: a continuous MSB-first bit-stream of
  `bits ∈ {10, 12, 14}`-wide samples packed across
  `(pixels * bits + 7) / 8` bytes). The encoder's raw-mode call site
  previously walked `for &sym in res_block { bw.write(sym as u32,
  bits); }` on a per-pixel `BitWriter`; each `bw.write` is a
  `&mut self` call so the optimiser couldn't prove `self.bytes` /
  `self.acc` / `self.bits_used` survived across the loop without a
  reload, and every iteration paid an early-return-on-`len == 0`
  branch that is dead in this context (raw mode always emits a
  fixed-width sample). The new function hoists the same 64-bit
  accumulator + whole-byte drain shape into stack locals across the
  whole slice, drops the dead early-return, and appends directly into
  the existing pre-sized `payload: Vec<u8>` (saving the
  `bw.finish() + payload.extend(...)` memcpy that the prior shape did
  per slice). The drain loop's iteration count is bounded by
  `floor(bits / 8) + 1` ≤ 2 at every native bit-depth and is easily
  unrolled by LLVM. Mirrors the decoder-side
  `bitreader::unpack_raw_bits_to_u16` added in the prior round.
  Seven new `pack_raw_bits_tests` (`empty_input_emits_no_bytes`,
  `out_of_range_bits_is_noop`, `single_aligned_sample`,
  `appends_to_existing_buffer`, `matches_per_pixel_at_10_12_14`,
  `unaligned_tail_zero_pads`, `long_payload_crosses_multiple_drains`)
  pin the batched implementation against a per-pixel
  `BitWriter::write(sym as u32, bits)` reference oracle defined
  inside the test module — the same shape the decoder-side tests use.
  The bit-depth-bounded test also round-trips packed bytes through
  the decoder's `unpack_raw_bits_to_u16` so the symmetry is anchored
  at both ends. Same observable wire-byte stream — all 110 unit +
  round-trip tests pass under `--all-features` (was 103; the 7 new
  packer parity tests) and 101 under `--no-default-features` (was
  94). Measured improvement on `examples/quick_bench raw`
  (10-run medians on the Apple M-series host):

  | Scenario                          | Baseline   | Post       | Δ        |
  | --------------------------------- | ---------: | ---------: | -------: |
  | enc M0RG / gradient / 1280×720    | 16.92 ms   | 14.48 ms   | -14.4 %  |
  | enc M2RG / gradient / 1280×720    | 18.17 ms   | 14.25 ms   | -21.5 %  |
  | enc M4RG / gradient / 1280×720    | 19.50 ms   | 14.79 ms   | -24.2 %  |

  The 8-bit M8RG raw scenario is unchanged (its raw path is the
  `payload.extend_from_slice(res_block)` byte-copy memcpy, not the
  bit-packer). Fixed-Huffman + Dynamic-Auto encode timings on the
  existing `quick_bench encode` / `dynamic` scenarios are flat to
  within ±1 % run-to-run noise: the batched packer fires only on
  `SliceMode::Raw` and on the Auto-loses-to-raw fallback (a rare
  natural-image case). Decode is unchanged.

- **`examples/quick_bench raw` scenario.** Fourth selector in the
  micro-bench driver alongside `decode`, `encode`, `dynamic`: walks
  `SliceMode::Raw` encode for one 8-bit FOURCC (the
  `extend_from_slice` control) and the 10/12/14-bit RGB family at
  1280×720. The 10-bit, 12-bit, and 14-bit cells are the exact call
  sites for the new `pack_raw_bits_from_u16` packer; the 8-bit cell
  rules out any change to the byte-copy path. `quick_bench raw` is
  the focused A/B harness for any future raw-pack hot-loop tweak
  while Criterion's `encode_strategy_matrix` bench remains the
  fixture-driven regression oracle.

### Changed

- **Per-plane scratch reuse in `build_slice_residuals_u{8,16}_into`.**
  The encoder's per-slice predictor-builder previously allocated a
  fresh `Vec<u8>` (Fixed mode) or three fresh `Vec<u8>`s (Dynamic
  mode) via `src.to_vec()` on every slice, then dropped two of the
  three Dynamic Vecs after L1-score selection and copied the
  winning one into the per-plane residual accumulator via
  `residuals.extend_from_slice(&block)`. The new shape pre-sizes
  two scratch `Vec<u8>` / `Vec<u16>` buffers at the per-plane scope
  (lifetime spans every slice and every kind), and:
  - Fixed mode writes `src` directly into the tail of `residuals`
    (which is already pre-sized to `plane_w × plane_h`), then runs
    `encode_predictor_u{8,16}` in-place on that tail. Per-slice
    `Vec` allocations: 1 → 0; per-slice memcpys: 2 → 1.
  - Dynamic mode runs the three candidate predictors through
    `trial_a` (kind 1, Left) / `trial_b` (kind 2, Gradient) / the
    loser's slot (kind 3, Median), tracks `best_in_a`, and
    `extend_from_slice`-copies the winning scratch into
    `residuals` once. Per-slice `Vec` allocations: 3 → 0;
    per-slice memcpys: 4 → 4 (same — heap pressure is the only
    component eliminated).

  Five new `build_slice_residuals_scratch_tests` (`u8_into_matches
  _reference_across_three_consecutive_slices`, `u16_into_matches
  _reference_across_consecutive_slices`, `u8_into_interlaced_
  matches_reference`, `u8_into_dynamic_picks_lowest_l1_with_ties_
  broken_left_first`, `u8_into_fixed_writes_into_dst_tail_without_
  touching_scratch`) pin the new shape against a fresh-Vec-per-call
  reference oracle defined inside the test module, exercising
  consecutive-slice scratch reuse, interlaced field-stride=2 (where
  the header-row block is processed before the strided body), the
  tie-break order on a uniform plane (Left wins, the strict `<`
  comparison preserves the predictor-id-ascending tie-break the
  prior shape used), and the Fixed-mode no-touch-scratch contract
  (a Fixed call may not clobber scratch buffers the caller may
  legitimately leave un-allocated until the first Dynamic invocation).

  Wall-time impact: timings on the Apple M-series host
  (`examples/quick_bench encode|dynamic`, 5-run medians per side
  under matched system load) are flat to within ±1.3 % across both
  Fixed and Dynamic — the per-slice heap pressure was already
  amortised by the platform allocator's per-thread arena fast
  paths. The change ships as an allocation-discipline improvement
  (peak heap and `Vec` alloc counter per encode drop measurably:
  for a 1280×720 M8RG/dynamic frame at 30 slices × 3 planes, the
  per-slice 3 Dynamic mallocs become 2 plane-lifetime trial mallocs,
  i.e. 90 mallocs → 2 across the residual-build phase) and a
  shape that any future predictor SIMD / batched-L1-score work
  has a cleaner buffer to attach to. All 103 unit + round-trip
  tests pass under `--all-features` (was 98; the 5 new scratch
  parity tests) and 94 under `--no-default-features` (was 89).

### Added

- **Direct `BitWriter` parity tests (`encoder::bit_writer_tests`).**
  Eight new unit tests pin the `BitWriter::write` whole-byte drain
  shape against a deliberately trivial bit-by-bit reference (one
  MSB bit per loop iteration, partial-byte tail zero-padded per
  `spec/05` §1.1's Huffman-payload wire convention). Coverage:
  empty input, zero-length writes interleaved with payload writes,
  byte-aligned single writes, unaligned tail zero-padding, hand-
  crafted short-code sequences crossing multiple byte boundaries,
  back-to-back 32-bit writes that force ≥ 4-byte drains, the
  `bits_used + len == 64` accumulator-full boundary case, and
  eight 512-write xorshift streams with `len ∈ [1, 32]` to walk
  every drain-shape the production hot loop sees in practice. The
  round-trip suite (84 scenarios across every FOURCC × predictor ×
  mode × interlaced combination) was already the integration-level
  oracle for any byte drift in the encoder's bitstream; these
  direct tests pin the `BitWriter` body at the unit level so a
  future hot-loop reshape lands as a focused failure before the
  larger integration tests reach it. Closes a coverage gap noted
  during the round-217 `to_be_bytes` drain-shape candidate (see
  `BENCHMARKS.md` "Round 217 closed candidates" — implemented,
  measured a +7-9 % encode regression on the Apple M-series host,
  rolled back; the tests stay because they exercise the existing
  shape just as well).

- **Batched raw-mode bit unpacker (`bitreader::unpack_raw_bits_to_u16`).**
  The high-bit-depth raw-mode slice payload (`spec/05` §4.1: a
  continuous MSB-first bit-stream of `bits ∈ {10, 12, 14}`-wide
  residuals packed across `(pixels * bits + 7) / 8` bytes) was being
  decoded by a per-pixel `BitReader::read_bits(bits)` loop in
  `decoder::decode_high_bit_depth`. Each `read_bits` call performs a
  refill check against the 64-bit accumulator, so a 640×480 10-bit
  raw slice fires ≈ 300 k refill branches even though only
  `ceil(640·480·10 / 56)` ≈ 55 k of them actually pull fresh bytes
  (one in ~5). The new unpacker writes `dst.len()` samples into a
  pre-sized `&mut [u16]` slice using the same 64-bit MSB-aligned
  accumulator + 8-byte big-endian refill the generic `BitReader`
  uses, but the refill check fires at most once per `floor(56 / bits)`
  pixels (≈ 5 / ≈ 4 / ≈ 4 at 10/12/14-bit) rather than once per
  pixel. End-of-data is implicitly zero-padded, matching the
  per-pixel path's `BitReader::refill` semantics. Four unit tests
  pin the batched implementation against the per-pixel reference
  (random 200-sample stream at each of 10/12/14-bit, a 5-sample
  short-payload zero-pad edge case, an empty-destination no-op, and
  a 4096-sample 14-bit stream that crosses multiple refill cycles).
  No observable bitstream change — the existing
  `high_bit_depth_raw_mode` + `all_fourccs_left_raw_random` round-trip
  tests continue to pass through the new path. Coverage source: the
  spec/05 §4.1 byte-layout block and the §3.3 64-bit accumulator
  refill rule shared with the Huffman bitstream.

- **`profile_magicyuv raw` mode (`examples/profile_magicyuv.rs`).** A
  third scenario in the sampling-profiler driver alongside `decode`
  and `roundtrip`: walks `M0RG/10bit/640×480`, `M2RG/12bit/640×480`,
  and `M4RG/14bit/640×480` raw-mode (`slice_flags & 0x01 == 1`)
  payloads through `decode_into` in a flat loop, so a `samply` /
  `cargo flamegraph` / `perf record` profile resolves the
  `unpack_raw_bits_to_u16` hot loop without Criterion's warm-up +
  estimator math diluting the per-iteration sample. Since the rest
  of the high-bit-depth pipeline (slice walking, predictor inverse,
  RGB decorrelation reversal) is shared with the Huffman variant
  already covered by `profile_decode`, any delta between this
  scenario's MiB/s and `decode`'s M0RG row is attributable to the
  unpacker. The three bit-depth tiers exercise the per-tier refill
  cadence end to end.

- **Sampling-profiler driver (`examples/profile_magicyuv.rs`).** The
  existing Criterion benches (`benches/{decode,encode,roundtrip,
  decode_all_fourccs,encode_strategy_matrix}.rs`) measure steady-state
  throughput under Criterion's sampling framework, but their warm-up +
  estimator math show up in a `samply` / `cargo flamegraph` /
  `perf record` profile and bury the codec hot paths. `quick_bench`
  is a timing helper (10-30 iters per scenario) — too short for a
  sampling profiler to settle on the codec body before the loop
  exits. The new `profile_magicyuv` example runs each scenario in a
  single flat loop with one `Instant`-pair around it, so the codec
  hot paths (Huffman batch decode, modular / JPEG-LS Median, RGB
  decorrelation reversal, Package-Merge length cap, BitWriter
  drain) are what the profiler sees. Five modes — `encode`,
  `decode`, `roundtrip`, `dynamic` (the `EncodeOptions::dynamic_auto()`
  v2.4.2 always-on combination per spec/04 §3 + spec/05 §6.2),
  `interlaced` (spec/04 §5.1 field-stride=2 prediction) — across the
  five `quick_bench` archetypes (M8RG/M8Y0/M8G0 8-bit primary-Huffman
  + M0RG 10-bit two-level + M8RG 256×256 modular-Median). Inputs are
  the same `quick_bench` gradient + 3-bit xorshift noise so profile
  output and bench numbers reference the same residual histogram.
  Throughput print (per-iter ms + MiB/s of raw uncompressed plane
  bytes + per-iter encoded-byte ratio for encode modes) makes the
  driver double as a quick A/B harness when Criterion's per-run
  overhead is too coarse. README has the `samply` / `cargo flamegraph`
  invocations under the new "Profiling" section. No `src/` changes —
  the driver consumes the existing `decode_into` /
  `encode_frame` / `EncodeOptions::{dynamic_auto,fixed}` / `tables::lookup`
  public API.

- **Encoder strategy × mode × interlaced Criterion bench
  (`benches/encode_strategy_matrix.rs`).** The per-FOURCC `encode`
  bench and the `decode_all_fourccs` breadth sweep both fix
  `strategy = Fixed(_)` + `mode = Huffman` + `interlaced = false`,
  leaving the encoder's `PredictorStrategy::Dynamic` (spec/04 §3,
  per-slice min-residual selection across Left + Gradient + Median)
  and `SliceMode::Auto` (spec/05 §6.2, per-slice Huffman vs
  bit-packed-raw size comparison) and the `flags & FLAG_INTERLACED`
  field-stride=2 prediction path (spec/04 §5.1) without any Criterion
  coverage. The new bench walks all 24 cells of the
  `(strategy ∈ {Fixed{Left,Gradient,Median}, Dynamic}) × (mode ∈
  {Huffman, Raw, Auto}) × (interlaced ∈ {off, on})` cube at M8Y0
  640×480 (8-bit YUV 4:2:0 — exercises the cross-plane-size dispatch
  the RGB-only matrix would miss). Captured baseline numbers + the
  per-axis reading (Raw cells ~1.5× faster than Huffman across every
  strategy; Dynamic ~1.3-1.4× the cost of any Fixed Huffman cell
  matching the 3× predictor work × < 30 % predictor share of total
  encode time; Auto matches Huffman cell-for-cell for the Gradient
  and Median strategies; interlaced 1-3 % slower than progressive
  uniformly) added to `BENCHMARKS.md` under "Round-200: encoder
  strategy × mode × interlaced matrix". Together with the existing
  benches this finally lights up every orthogonal encoder axis the
  public `EncodeOptions` exposes; the `Dynamic + Auto`
  (`EncodeOptions::dynamic_auto()`) shipping configuration now has
  Criterion regression coverage.

- **Cross-FOURCC decode-throughput Criterion bench
  (`benches/decode_all_fourccs.rs`).** Covers every native MagicYUV v7
  FOURCC defined in `tables/00-fourcc-table.csv` (17 entries: 8-bit
  RGB / RGBA / YUV 4:4:4 / 4:2:2 / 4:2:0 / YUVA / Gray, plus the
  10/12/14-bit RGB / RGBA / YUV / Gray variants) at the same 640×480
  Gradient + Huffman configuration so per-format decode throughput
  can be compared at a glance. Throughput is reported in raw
  uncompressed plane bytes so the MiB/s figures across formats line
  up as "decoded pixel volume per second". Captured baseline numbers
  + per-tier reading (8-bit clusters at 237-266 MiB/s, 10/12/14-bit
  at 379-395 MiB/s — the expected ~50 % MiB/s gain from the
  per-sample arithmetic carrying 2 bytes instead of 1 for the same
  per-sample pipeline cost) added to `BENCHMARKS.md` under
  "Round-194: cross-FOURCC decode-throughput sweep". The breadth
  complement to the existing five-scenario optimisation-round
  picks; future regressions that drift one FOURCC relative to its
  bit-depth tier-mates will surface immediately on
  `cargo bench -p oxideav-magicyuv --bench decode_all_fourccs`.

## [0.0.5](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.4...v0.0.5) - 2026-05-30

### Other

- inline two-level hot loop for decode_into_u16
- drop one HashMap + one Vec clone from HuffmanTable::build
- BitWriter::with_capacity for slice-payload hot path
- anchor Cargo.lock to crate root so fuzz/Cargo.lock isn't dual-state
- refresh fuzz.yml preamble for the two-target setup
- add encode_magicyuv target driving the full 17-FOURCC × strategy × mode × interlaced cube
- add decode_magicyuv cargo-fuzz harness
- decode_into(&mut DecodedFrame) streaming entry point
- pack decoder primary table into Vec<u32> (low-8 length, high-24 symbol)
- length-limited Package-Merge for Huffman code lengths (spec/05 §1.3)
- preamble_trailing.extra_bytes is an integer count (spec/05 §10 Q6)
- PredictorStrategy::Dynamic + SliceMode::Auto (spec/04 §3 + spec/05 §6.2)

### Changed

- **Decoder `HuffmanTable::decode_into_u16` inlined two-level hot loop.**
  The 10/12/14-bit Huffman batch decoder used to walk
  `self.decode(br)` once per pixel — `#[inline(always)]` on `decode`
  let the function body fold in, but the loop body still re-loaded
  `self.max_len`, `self.primary_bits`, the `primary` slice base, the
  `secondary` slice base, and the `REDIRECT_MARKER` comparand from
  `&self` on every iteration (the compiler couldn't prove `&self`
  immutable across the inner `br.consume` mutation). The new shape
  mirrors `decode_into_u8` (opt-2 in `BENCHMARKS.md`): hoist all five
  to local bindings once at function entry, split single-level vs
  two-level into the loop selector (the single-level branch covers
  well-formed-but-shallow descriptors whose realised `max_len_used`
  lands at ≤ `PRIMARY_BITS = 12`), and run a flat peek/consume +
  table lookup body so the BitReader's `acc` / `fill` / `pos` stay
  in registers across the whole slice. Two new
  `decode_into_u16_matches_per_pixel_decode_{two_level,single_level}`
  parity tests pin the batch helper against the per-pixel `decode()`
  reference on hand-assembled MSB-first bit streams that bounce
  across multiple primary-prefix buckets (two-level) and across a
  shallow 10-bit alphabet (single-level), so any future hot-loop
  tweak that drifts symbol output gets caught the same way the
  round-2 `decode_into_u8_matches_per_pixel_decode` test caught
  off-by-ones on the 8-bit batch path. Same observable wire-byte
  stream — all 86 unit + round-trip tests pass under
  `--all-features` (was 84; the two new parity tests) and 77 under
  `--no-default-features` (was 75). End-to-end decode timings on
  `examples/quick_bench decode` for `M0RG/gradient/1280×720` (the
  10-bit two-level scenario) move from a 13.62-13.93 ms baseline
  median of ~13.78 ms to a 13.12-13.91 ms post-opt median of
  ~13.27 ms — a ~3-4 % saving on the two-level path. The 8-bit
  M8RG/gradient and M8Y0/gradient scenarios are unchanged within
  run-to-run noise (their `decode_into_u8` path was already
  inlined-loop-shaped from opt-2). Trace JSONL emitter is
  unaffected — it lives in `decoder.rs` and consumes the same
  per-symbol stream the batch helper produces.

- **Decoder `HuffmanTable::build` allocation cleanups.** Two
  build-path tweaks that drop one heap allocation per plane built
  (`HashMap<u32, usize>` → `Vec<i32>; primary_size` direct-index)
  and one intermediate `Vec` copy (`start.clone()` → `mem::take`
  hand-off into the code-assignment loop). The `HashMap` was used to
  associate a primary prefix value with the per-prefix subtable
  index in the two-level path (`max_len > PRIMARY_BITS = 12`, i.e.
  every 10 / 12 / 14-bit alphabet). Since prefix values are bounded
  by `1 << primary_bits ≤ 4096`, a direct-indexed `Vec<i32>` with
  the sentinel `-1` for "no subtable yet" is denser than the
  SipHash-keyed map: the lookup is one indexed load, the
  miss-then-insert is an indexed compare-and-store, and the build
  loop's per-symbol branch carries no hash-or-allocate cost. The
  Vec is up to 16 KB at the 14-bit alphabet (4096 × 4 B), zeroed
  once at construction. Same observable `HuffmanTable` (the
  per-symbol `lengths`, `codes`, and lookup-table contents are
  byte-identical to the prior path; verified by all 84 unit +
  round-trip tests under `--all-features` and 75 under
  `--no-default-features`, plus a new
  `build_two_level_uses_per_prefix_subtables` test that exercises
  the two-level build path with an alphabet that places symbols in
  every distinct primary prefix bucket). End-to-end decode timings
  are within run-to-run noise on the in-tree Apple M-series host —
  the build cost is a tiny fraction of total decode work even on
  the smallest scenario (256×256, ~0.9 ms decode of which the build
  is well under 50 µs) — but the allocator-pressure reduction is
  real and replaces a SipHash probe per deferred-symbol with an
  indexed load. Wire bytes the decoder consumes are unchanged
  (symbol-order walk of `lengths` is identical, so each prefix's
  subtable lands at the same `secondary[]` index either way).

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
