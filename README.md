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
| Canonical-code build   | longest-length-first cumulative               |
| Raw-mode fallback      | per-slice                                     |

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
