# oxideav-magicyuv

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
`interlaced`); defaults reproduce `EncodeOptions::dynamic_auto()`.

Both sides reject odd dimensions that don't divide a subsampled
FOURCC's chroma factor with the same `OddDimensionForSubsampling`
error, so they accept exactly the same dimension set. The
flags-dword knobs `interlaced` (bit 1), `full_range` (bit 2), and the
4-bit `color_matrix` nibble (bits 20..23) are surfaced as
[`EncodeOptions`] fields and recovered via typed `FrameHeader`
accessors.

## Pipeline

| Stage                  | Notes                                         |
| ---------------------- | --------------------------------------------- |
| 32-byte v7 header      | parse + emit                                  |
| Slice table + preamble | honours on-wire `per_slice_plane_index`       |
| Chroma slice partition | per-plane slice count from luma row count     |
| Per-slice predictors   | Left / Gradient / Median (modular 8-bit, JPEG-LS at 10/12/14-bit) |
| Interlaced field-stride| top neighbour = row r-2; first 2 rows raw     |
| Per-plane Huffman      | RLE descriptor                                |
| Canonical-code build   | longest-length-first cumulative; over-full books rejected at build, under-full (Kraft < 1) books accepted (encoder emits them for single-symbol planes) |
| Under-full hardening   | a malformed slice peeking unused codespace → `HuffmanIncomplete` (spec/05 §2.1), not a silent mis-decode |
| Raw-mode fallback      | per-slice                                     |

## Conformance

Beyond the encode→decode self-roundtrip suite, the decoder is pinned
against **proprietary-encoder ground-truth bytes** reconstructed
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

## Public API

- [`decode_frame`] — decode one MAGY-prefixed frame; returns one
  [`DecodedPlane`] per native plane (`u8` for 8-bit, `u16` for
  10/12/14-bit FOURCCs).
- [`decode_into`] — streaming variant reusing caller-owned per-plane
  storage when geometry matches.
- [`encode_frame`] — encode one frame from per-plane pixel buffers,
  driven by [`EncodeOptions`].
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
  parameter cube; asserts no panic plus byte-exact round-trip
  through `decode_frame`.
- **`huffman_descriptor`** — pushes arbitrary bytes into the
  length-descriptor parse + canonical-Huffman build + two-level
  table arithmetic.

```sh
cd fuzz && cargo +nightly fuzz run decode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run encode_magicyuv -- -max_total_time=60
cd fuzz && cargo +nightly fuzz run huffman_descriptor -- -max_total_time=60
```

## Profiling

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
