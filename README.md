# oxideav-magicyuv

[![CI](https://github.com/OxideAV/oxideav-magicyuv/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-magicyuv/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-magicyuv.svg)](https://crates.io/crates/oxideav-magicyuv) [![docs.rs](https://docs.rs/oxideav-magicyuv/badge.svg)](https://docs.rs/oxideav-magicyuv) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust MagicYUV lossless video codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Codec-only** (MagicYUV is a video codec; AVI/MOV containers live in
their own crates). Decoder + encoder, with byte-for-byte round-trip.

Decodes the full native FOURCC set: 8-bit (M8RG, M8RA, M8Y4, M8Y2,
M8Y0, M8YA, M8G0) and 10/12/14-bit (M0RG, M0RA, M2RG, M2RA, M4RG,
M4RA, M0Y2, M0Y4, M0Y0, M0G0). Honours `FLAG_INTERLACED` for
field-stride prediction.

The encoder emits wire-format frames the decoder round-trips
byte-for-byte. Strategies:

- **Fixed** Left / Gradient / Median predictors
  (`PredictorStrategy::Fixed`).
- **Dynamic** per-slice predictor selection by minimum residual L1
  norm (`PredictorStrategy::Dynamic`).
- Per-slice Huffman / raw fallback (`SliceMode::Auto`, by byte
  budget). `EncodeOptions::dynamic_auto()` combines both.

Both directions are wired into `oxideav-core`'s codec registry (the
default-on `registry` feature): the codec entry declares
`with_decode()` **and** `with_encode()`, registering a `Decoder` and
an `Encoder` factory plus all 17 native v7 FourCC tags. The registry
`Encoder` recovers the output FourCC from `CodecParameters::tag` and
consumes planar `Frame::Video`s (one plane per codec plane, in the
family order the decoder emits), so a framework-level encode→decode
loop round-trips bit-exact for every native FourCC. Encode strategy is
configurable through `CodecParameters::options` (`predictor` ∈
left/gradient/median/dynamic, `slice_mode` ∈ huffman/raw/auto,
`interlaced`, and `slice_height` for the `spec/02` §4 slice partition);
defaults reproduce `EncodeOptions::dynamic_auto()` with a single
full-frame slice. `output_params()` also reports the exact
`PixelFormat` for each FourCC (interleaved `Rgb24`/`Rgba` at 8-bit,
planar `Gbrp*Le`/`Yuv*P*Le`/`Gray*Le` otherwise; `None` for the one
FourCC with no exact core variant). A content probe validates a FourCC
claim against the `MAGY` magic (`spec/01` §1): decisive `1.0`/`0.0` when
a packet/header is peekable, `0.9` on the FourCC alone otherwise.

Odd widths and heights are accepted for every FOURCC; on 4:2:2 /
4:2:0 the chroma planes are `ceil(width / sub_x) × ceil(height /
sub_y)` samples (`spec/03` §8.2). On a 4:2:0 family a `slice_height`
that is not a multiple of the vertical chroma factor is refused by
encoder, decoder and the registry encoder alike
(`SliceHeightNotDivisibleBySubsampling`, `spec/02` §6: the chroma
partition would not tile). The encoder also refuses an interlaced frame
whose slice partition leaves a slice shorter than two rows per plane
(`InterlacedSliceTooShort`, see *Interoperability*). The flags-dword
knobs `interlaced` (bit 1), `full_range` (bit 2), and the 4-bit
`color_matrix` nibble (bits 20..23) are surfaced as [`EncodeOptions`]
fields and recovered via typed `FrameHeader` accessors.

## Pipeline

| Stage                  | Notes                                         |
| ---------------------- | --------------------------------------------- |
| 32-byte v7 header      | parse + emit                                  |
| Slice table + preamble | honours on-wire `per_slice_plane_index`; also the row-major-rank layout (see *Interoperability*) |
| Chroma planes          | `ceil(dim / sub)` samples; per-plane slice count from luma row count; `slice_height` indivisible by `sub_y` on 4:2:0 rejected (`spec/02` §6) |
| Per-slice predictors   | Left / Gradient / Median (modular 8-bit, JPEG-LS at 10/12/14-bit); §4.4 formula divergence + §4 column-0 top-fallback pinned against spec worked examples |
| Interlaced field-stride| top neighbour = row r-2; first 2 rows raw     |
| Per-plane Huffman      | RLE descriptor                                |
| Canonical-code build   | longest-length-first cumulative; over-full books rejected at build, under-full (Kraft < 1) books accepted on decode. The encoder always writes a complete book over the whole alphabet (optimal length-limited, package-merge) |
| Under-full hardening   | a malformed slice peeking unused codespace → `HuffmanIncomplete` (spec/05 §2.1), not a silent mis-decode |
| Raw-mode fallback      | per-slice                                     |

## Interoperability

`tests/blackbox_matrix.rs` runs a live matrix against a black-box
MagicYUV encoder and decoder (the `ffmpeg` executable, output only; the
test skips at runtime when it is absent). Current result with
`cargo test --release`: **30 402 frames, 30 402 pass.**

- **Foreign → this decoder — 12 474 / 12 474 bit-exact.** Every pixel
  format the black-box encoder accepts (planar RGB, RGBA, YUV 4:4:4 /
  4:2:2 / 4:2:0, YUVA 4:4:4:4, gray — all 8-bit; it writes no deeper
  format) × 3 predictors × slice counts 1…33 × 13 sizes from 1×1 through
  odd sizes to 641×361 × 6 kinds of content (noise, flat, zero,
  two-valued, noisy ramp, clean ramp). The black-box decoder is run on
  the same frames and agrees.
- **This encoder → foreign decoder — 17 928 / 17 928 accepted
  bit-exact with no decoder diagnostics.** All 17 native FOURCCs
  (8/10/12/14-bit) × Left / Gradient / Median / Dynamic × Huffman / raw /
  auto × slice heights 1…1024 × progressive / interlaced × 8 sizes
  (1×1 … 321×243). This covers the 10/12/14-bit bit-packed raw slices
  of `spec/05` §4.1, which the vendor fixtures behind the spec never
  exercised.
- **Size.** On the foreign → ours frames, `EncodeOptions::dynamic_auto()`
  at the black-box encoder's slice height needs 91.7 % (4:2:0) to
  95.8 % of the bytes of the *best of the black-box encoder's three
  predictors*, per pixel format (run-length descriptors, per-slice
  predictor choice and raw fallback); the test asserts it never needs
  more.

What the first run of this matrix found (all fixed, each pinned in
`tests/foreign_layout.rs`):

- **Every frame this encoder wrote was refused** by the black-box
  decoder: the Huffman books marked unused symbols with length 0. The
  books in `spec/05` §1.2 / §1.3 always code the whole alphabet, and so
  does the encoder now.
- **Every multi-slice foreign frame was refused** here. Such frames
  use a layout `spec/02` does not describe: the slice payloads are
  stored slice-row by slice-row, the slice table is still indexed
  plane-major (so it is not monotonic), and the preamble index byte at
  position `p·S + s` holds `s·P + p` (`P` planes, `S` slices per plane)
  rather than the plane index of `spec/02` §7.3. The decoder accepts
  exactly this shape next to the documented one — the two cannot be
  confused, a rank byte exceeds the plane count as soon as a plane has
  two slices — and refuses any other permutation; what a general
  permutation would mean is an open question for the spec.
- **Odd sizes on 4:2:2 / 4:2:0 were refused** in both directions.
- **Interlaced frames with a slice shorter than two rows per plane**
  (measured as the slice's luma rows divided by `sub_y`, rounded down)
  are refused by the black-box decoder, so the encoder returns
  `InterlacedSliceTooShort` instead of writing one. The decoder here
  still reads such frames.

Not verified against any outside stream: frames written by the vendor
codec itself beyond the worked examples transcribed from the spec
(below); foreign 10/12/14-bit frames (the black-box encoder writes
none); foreign interlaced frames (the black-box encoder never sets the
flag).

## Conformance

Beyond the live matrix and the encode→decode self-roundtrip suite, the
decoder is pinned against **proprietary-encoder ground-truth bytes** reconstructed
byte-for-byte from the clean-room spec's worked-example hex (no
reference binary in-tree):

- `m8rg_64x64_zero.bin` — the canonical all-zero 64×64 RGB frame
  whose complete 1670-byte layout is documented in spec/02 §5.2 +
  spec/05 §7. Decodes to three all-zero GBR planes, exercising the
  full Huffman path (descriptor parse, length-descending canonical
  code build, MSB-first read, Left predictor, RGB decorrelation).
- The exact §2.0.1 canonical code-assignment table (symbol 0 → code
  `1`; symbol 95 → code `127`; 254 length-9 symbols → codes
  `0..253`) is asserted directly, locking the construction against
  RFC-1951 ordering.
- `4×8 M8RG interlaced raw` — the spec/04 §5.1.1 doubled-row-stride
  residual stream, decoding to the documented field-interleaved
  vertical ramp (ground-truth coverage of the interlaced predictor).

The encode→decode self-roundtrip suite is driven by two complementary
property sweeps proving bit-exact lossless recovery:

- **Cartesian sweep** — all 17 native FOURCCs × all 3 predictors ×
  both slice modes × single-slice / multi-slice / partial-chroma
  geometry × interlaced on/off, seeded so the Huffman descriptor and
  predictor paths see high-entropy residuals.
- **Minimal-geometry sweep** — the boundary complement, with every
  dimension the smallest the FOURCC's chroma subsampling admits (down
  to `1×1` for RGB / Gray and `sub_x × sub_y` for the 4:2:x families).
  Single-column (first-column-only predictor arm), single-row (one
  header row, no top neighbour), `1×1` (single-symbol under-full
  Huffman book), and interlaced `≤ field_stride`-row (header-rows
  early-return) boundaries are each pinned, for both the fixed and
  the `dynamic_auto()` strategies. The same boundary shapes seed the
  decode fuzz corpus.

### Forward-compat decode tolerances

The v2.4.2 encoder emits a narrow subset of the format's legal shapes,
so the round-trip suite never exercises the parts the spec marks as
reserved / implementation-latitude. A dedicated `forward_compat` module
rewrites a valid frame to the edge of each documented open question and
asserts the decoded pixels are unchanged — locking the decoder's
robustness against a regression that tightened a tolerance:

- **Reserved `slice_flags` bits** (`spec/04` §8 Q3) — only bit 0 selects
  Huffman vs raw; bits 1..7 are ignored (`& 0x01`) in both the 8-bit and
  high-bit-depth slice loops.
- **Trailing preamble bytes** (`spec/05` §10 Q5) — padding between the
  last Huffman descriptor and the first slice is skipped; pinned in the
  default feature set (the `trace` build additionally emits a
  `preamble_trailing` diagnostic).
- **Header `codec_variant` byte `+0x0b`** (`spec/04` §8 Q4) — stored for
  diagnostics but never consulted by the decode dispatch (the predictor
  is the per-slice `predictor_id`); decode is invariant to its value.
- **Slice-table `entry[0]`** (`spec/02` §10 Q4) — the decoder derives
  the preamble end and every slice start from `entry[1..]`, so `entry[0]`
  can hold anything.
- **Interleaved `per_slice_plane_index`** (`spec/02` §7.3 / §10 Q8) — a
  non-plane-major slice order decodes identically (RGB/RGBA/YUV-4:2:0 at
  8-bit and 10-bit).

## Public API

- [`decode_frame`] — decode one MAGY-prefixed frame; returns one
  [`DecodedPlane`] per native plane (`u8` for 8-bit, `u16` for
  10/12/14-bit FOURCCs).
- [`decode_into`] — streaming variant reusing caller-owned per-plane
  storage when geometry matches.
- [`decode_frame_with`] / [`decode_into_with`] — the same with
  [`DecodeOptions`] (`with_threads(n)`).
- [`encode_frame`] — encode one frame from per-plane pixel buffers,
  driven by [`EncodeOptions`] (predictor strategy, slice mode,
  interlacing, header flags, `with_threads(n)`).
- [`header::parse`] — standalone v7 header parser, plus
  `is_interlaced` / `is_full_range` / `color_matrix_nibble` typed
  accessors and the `FLAG_*` constants.
- [`Error`], [`Result`] — crate-local error type.
- `register(ctx)` (default-on `registry` feature) — wire into
  `oxideav-core`'s codec registry.
- `output_params(rec, w, h)` — `CodecParameters` the encoder
  produces, for muxers writing the wire FourCC.

## Cargo features

- **`registry`** (default): wire into `oxideav-core`'s codec
  registry. `--no-default-features` drops the `oxideav-core`
  dependency entirely.
- **`trace`** (off): emit JSONL trace events to the path in
  `OXIDEAV_MAGICYUV_TRACE_FILE` during decode.

## Fuzzing

Three [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
harnesses under [`fuzz/`](fuzz/), wired into the daily fuzz workflow:

- **`decode_magicyuv`** — drives `decode_frame` on arbitrary bytes;
  contract is decode always returns a `Result` and never
  panics / overflows / indexes OOB.
- **`encode_magicyuv`** — drives `encode_frame` across the full
  parameter cube (odd sizes included); asserts no panic plus
  byte-exact round-trip through `decode_frame`.
- **`huffman_descriptor`** — pushes arbitrary bytes into the
  length-descriptor parse + canonical-Huffman build + two-level
  table arithmetic.

```sh
cd fuzz && cargo +nightly fuzz run decode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run encode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run huffman_descriptor -- -max_total_time=60
```

## Threads and throughput

Every entry point is serial unless the caller grants threads:
`DecodeOptions::with_threads(n)`, `EncodeOptions::with_threads(n)`, or
`set_execution_context` on the registry decoder / encoder. Slices are
independent (`spec/04` §5.2), so with a grant they are predicted and
entropy-coded on up to `n` scoped `std::thread` workers (no
dependency, blocking hand-off). Decoded samples and encoded bytes are
identical at every grant. The grant only helps when a plane has
several slices: the registry encoder's default is one slice per plane
(best compression); set its `slice_height` option (28 is what the
vendor encoder writes) to make frames that encode and decode in
parallel.

MiB of raw samples per second, `examples/throughput.rs`, slice height
28, a real image crop with added noise (compresses to 0.52 at 8-bit).
18-core arm64 host shared with other build jobs, so every figure is
the median of 3 rounds of a median-of-7 and still moves by ±15 %
between sessions. "before" is the serial code at the start of this
work; the serial and the 8-thread columns are the current code.

| Frame, operation                     | before | serial | 8 threads |
| ------------------------------------ | -----: | -----: | --------: |
| RGB 8-bit 2160p, encode Median       |    312 |    458 |      2363 |
| RGB 8-bit 2160p, encode Dynamic+Auto |    216 |    273 |      1649 |
| RGB 8-bit 2160p, decode              |    351 |    596 |      3768 |
| RGB 8-bit 1080p, encode Median       |    275 |    444 |      2414 |
| RGB 8-bit 1080p, encode Dynamic+Auto |    198 |    280 |      1637 |
| RGB 8-bit 1080p, decode              |    314 |    470 |      2916 |
| 4:2:0 8-bit 2160p, encode Median     |    266 |    378 |      2398 |
| 4:2:0 8-bit 2160p, encode Dyn+Auto   |    201 |    250 |      1602 |
| 4:2:0 8-bit 2160p, decode            |    329 |    524 |      3588 |
| 4:2:0 8-bit 1080p, decode            |    308 |    511 |      2985 |
| RGB 10-bit 2160p, encode Median      |    393 |    458 |      3834 |
| RGB 10-bit 2160p, encode Dyn+Auto    |    315 |    315 |      1836 |
| RGB 10-bit 2160p, decode             |    329 |    347 |      2534 |

Where the serial gains come from: the 8-bit Huffman decoder reads two
symbols per table lookup whenever both codes fit the 12-bit window and
refills its bit buffer once per four lookups; the encoder gathers
three or four codes per 64-bit store instead of emitting byte by byte,
computes residuals in place, and counts symbols in four interleaved
histograms. The 10/12/14-bit decoder (two-level table) is unchanged,
its serial figures above are noise. The registry decoder no longer
copies 8-bit planes into the output frame.

## Profiling

[`examples/magy_cli.rs`](examples/magy_cli.rs) decodes / encodes single
frame files (planes back to back), handy next to a black-box tool.
[`examples/profile_magicyuv.rs`](examples/profile_magicyuv.rs) is a
flat sampling-profiler driver (modes: `encode`, `decode`,
`roundtrip`, `dynamic`, `interlaced`, `all`). Criterion benches under
[`benches/`](benches/) cover the timing-oriented hot paths.

```sh
cargo build --release --example profile_magicyuv
samply record -- ./target/release/examples/profile_magicyuv decode 2000
```

## Why clean-room

MagicYUV is a closed-source commercial codec. The implementation is
built solely against the strict-isolation clean-room workspace at
[`docs/video/magicyuv/`](https://github.com/OxideAV/docs/tree/master/video/magicyuv)
(spec + FOURCC / predictor tables) — no proprietary binary, no
reference-encoder source, no third-party decoder source. Reverse
engineering for interoperability is permitted under 17 U.S.C.
§1201(f), *Sega v. Accolade* / *Sony v. Connectix*, and EU Directive
2009/24/EC Articles 5(3) + 6.

## License

MIT.
