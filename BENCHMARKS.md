# oxideav-magicyuv benchmarks

Hardware: Apple M-series, macOS 26.1.

Run with `cargo bench` (criterion harness). For fast measure-tweak-remeasure
iterations during the optimization round, the `examples/quick_bench.rs`
helper times the same scenarios with a flat 10-iteration loop.

## Scenarios

| Scenario                       | FOURCC | Predictor | Resolution    | Notes                                                    |
| ------------------------------ | ------ | --------- | ------------- | -------------------------------------------------------- |
| `decode_m8rg_720p`             | M8RG   | Gradient  | 1280×720      | 8-bit RGB, primary-Huffman path                          |
| `decode_m8y0_720p`             | M8Y0   | Gradient  | 1280×720      | 8-bit YUV 4:2:0, chroma-subsampled plane geom            |
| `decode_m8g0_1080p`            | M8G0   | Left      | 1920×1080     | 8-bit Gray, simplest predictor                           |
| `decode_m0rg_720p_10bit`       | M0RG   | Gradient  | 1280×720      | 10-bit RGB, two-level Huffman lookup                     |
| `decode_m8rg_256_median`       | M8RG   | Median    | 256×256       | 8-bit RGB, modular-Median path                           |

## Hot-path attribution (sampled with `sample(1)`, decode side)

- ~70-75 % `huffman::HuffmanTable::decode` — per-pixel two-level lookup +
  `BitReader::peek_bits` + `BitReader::consume` + `refill`.
- ~25 % `predict::apply_u{8,16}_with_stride` — per-pixel Left / Gradient /
  Median (modular 8-bit / JPEG-LS 10/12/14-bit).
- < 5 % header parse, slice-table walk, RGB inter-plane decorrelation
  reverse, per-plane buffer allocation.

## Baseline (round-3, before optimization)

Numbers from `examples/quick_bench all` on the host above. Criterion
gives equivalent figures (within 1 %).

| Scenario                          | Decode (ms) | Encode (ms) |
| --------------------------------- | ----------: | ----------: |
| M8RG / gradient / 1280×720        |       20.09 |       20.65 |
| M8Y0 / gradient / 1280×720        |        9.92 |       11.79 |
| M8G0 / left   / 1920×1080         |       13.12 |       14.41 |
| M0RG / gradient / 1280×720 / 10b  |       19.62 |       26.87 |
| M8RG / median   / 256×256         |        1.46 |        n/a  |

## Optimizations landed

### 1. `BitReader::refill` 8-byte fast path

Single u64 big-endian load + OR-merge replaces the byte-by-byte loop
when ≥ 8 bytes are still ahead. Same observable bit stream
(verified against the existing 53 unit tests + roundtrip suite).

| Scenario                          | Before  | After   | Δ        |
| --------------------------------- | ------: | ------: | -------: |
| dec M8RG / gradient / 1280×720    | 20.09 ms | 19.73 ms | -1.8 % |
| dec M8Y0 / gradient / 1280×720    |  9.92 ms |  9.23 ms | -7.0 % |
| dec M8G0 / left   / 1920×1080     | 13.12 ms | 13.48 ms | +2.7 % |
| dec M0RG / gradient / 1280×720    | 19.62 ms | 19.24 ms | -1.9 % |
| dec M8RG / median   / 256×256     |  1.46 ms |  1.47 ms |  0.0 % |

Modest. The compiler already pipelines the byte loop reasonably; the
big win lands together with optimization #2 below (batched decode).

### 2. `HuffmanTable::decode_into_u{8,16}` batched decode

Folds the per-symbol `peek_bits` + table lookup + `consume` into a
single tight loop so the BitReader state (`acc`, `fill`, `pos`) stays
in registers across the whole slice. The 8-bit path additionally
short-circuits the primary-table-only case (`max_len ≤ 12`) so the
two-level dispatch + `secondary` indirection vanishes from the hot
loop on every native 8-bit FOURCC.

Numbers below are cumulative on top of optimization #1 (i.e. they
compare to the round-3 baseline, not to opt-1-only).

| Scenario                          | Baseline | After 1+2 | Δ        |
| --------------------------------- | -------: | --------: | -------: |
| dec M8RG / gradient / 1280×720    | 20.09 ms | 16.73 ms  | -16.7 % |
| dec M8Y0 / gradient / 1280×720    |  9.92 ms |  8.19 ms  | -17.4 % |
| dec M8G0 / left   / 1920×1080     | 13.12 ms | 11.54 ms  | -12.0 % |
| dec M0RG / gradient / 1280×720    | 19.62 ms | 17.82 ms  |  -9.2 % |
| dec M8RG / median   / 256×256     |  1.46 ms |  1.34 ms  |  -8.2 % |

### 3. Predictor inner loops via `split_at_mut` row pair

`apply_u{8,16}_with_stride` now splits `data` into the previous-row
`&[u_]` slice and the current-row `&mut [u_]` slice once per row. The
optimiser can then elide the bounds check on every `cur[c]` /
`prev[c]` access in the c-loop — `c` is provably `< width =
slice.len()`. Same arithmetic, same observable output.

Cumulative on top of opt 1+2 (compared to round-3 baseline):

| Scenario                          | Baseline  | After 1+2+3 | Δ        |
| --------------------------------- | --------: | ----------: | -------: |
| dec M8RG / gradient / 1280×720    | 20.09 ms  |   10.97 ms  | -45.4 % |
| dec M8Y0 / gradient / 1280×720    |  9.92 ms  |    4.85 ms  | -51.1 % |
| dec M8G0 / left   / 1920×1080     | 13.12 ms  |    6.82 ms  | -48.0 % |
| dec M0RG / gradient / 1280×720    | 19.62 ms  |   11.14 ms  | -43.2 % |
| dec M8RG / median   / 256×256     |  1.46 ms  |    0.84 ms  | -42.5 % |

### 4. Encoder predictor row-pair + `peek_bits` inline-always

Mirrors opt 3 on the encoder side (`encode_predictor_u{8,16}` use
the same `split_at_mut` shape). Also marks
`bitreader::BitReader::peek_bits` `#[inline(always)]` so the
Huffman batch decoder body stays a flat tight loop after inlining.

| Scenario                          | Baseline  | After 1-4 | Δ        |
| --------------------------------- | --------: | --------: | -------: |
| enc M8RG / gradient / 1280×720    | 20.65 ms  | 19.14 ms  |  -7.3 % |
| enc M8Y0 / gradient / 1280×720    | 11.79 ms  | 11.19 ms  |  -5.1 % |
| enc M8G0 / left   / 1920×1080     | 14.41 ms  | 14.37 ms  |  -0.3 % |
| enc M0RG / gradient / 1280×720    | 26.87 ms  | 25.94 ms  |  -3.5 % |

Decoder figures are unchanged from opt 3; the encoder picks up
3-7 % on the predictor-bound scenarios. The remaining encoder cost
is dominated by the per-bit `BitWriter::write` + `canonical_huffman_lengths`
heap-build, which are deferred to the next round (see "Round-N+1
candidates" below).

### 5. `BitWriter` 64-bit accumulator with whole-byte drain

The encoder's `BitWriter::write(code, len)` was a `for i in
(0..len).rev() { … }` per-bit loop. Replaces it with a single
shift + OR into a 64-bit accumulator, then drains whole bytes
while the accumulator holds ≥ 8 bits. Same observable byte stream
(verified by the 53 unit tests + the encode→decode round-trip).

Encoder figures (cumulative, vs round-3 baseline):

| Scenario                          | Baseline  | After 1-5 | Δ        |
| --------------------------------- | --------: | --------: | -------: |
| enc M8RG / gradient / 1280×720    | 20.65 ms  | 11.73 ms  | -43.2 % |
| enc M8Y0 / gradient / 1280×720    | 11.79 ms  |  6.76 ms  | -42.7 % |
| enc M8G0 / left   / 1920×1080     | 14.41 ms  |  8.11 ms  | -43.7 % |
| enc M0RG / gradient / 1280×720    | 26.87 ms  | 16.37 ms  | -39.1 % |

## Final cumulative deltas

| Scenario                             | Decode Δ | Encode Δ |
| ------------------------------------ | -------: | -------: |
| M8RG / gradient / 1280×720           |  -42.2 % |  -43.2 % |
| M8Y0 / gradient / 1280×720           |  -47.4 % |  -42.7 % |
| M8G0 / left   / 1920×1080            |  -44.4 % |  -43.7 % |
| M0RG / gradient / 1280×720 / 10-bit  |  -41.3 % |  -39.1 % |
| M8RG / median   / 256×256            |  -39.7 % |    n/a   |

## Round-N+1 candidates

- **Encoder `canonical_huffman_lengths` → `enforce_length_cap`** —
  has degenerate behaviour on highly-skewed histograms (e.g. Median
  predictor at ≥ 1024×1024 with smooth-gradient inputs). Per-frame
  encode wall-time blows up from low ms to many minutes. The decoder
  is unaffected; spec-compliant streams produced by Gradient/Left at
  the same sizes decode identically. Fix needs the length-limited
  Package-Merge algorithm.
- **Decoder primary-table layout** — `Vec<(u32, u8)>` packs each
  entry into 8 bytes (5 used, 3 padding). Switching to
  `Vec<u16>` (6 bits length, 10 bits symbol — fits 8-bit alphabet)
  halves the working-set and may move the table into L1 hot for
  smaller frames.
- **Predictor SIMD (Left only)** — Left has the simplest dependency
  chain (only `cur[c-1]`); a `wrapping_add` prefix-scan over u8x16
  using `core::simd` could give another 2-3× on Gray (Left at
  1080p is the simplest predictor in our coverage).
- **`vec![0u8; w*h]` per decode** — every `decode_frame` call
  allocates fresh plane buffers. A `decode_into(&mut DecodedFrame)`
  variant that re-uses caller-owned `Vec`s would avoid the per-frame
  malloc on streaming scenarios.
- **`HuffmanTable::build`** itself is on the cold path but takes
  ~2-5 % of `decode_frame` for small frames (256×256 / smaller). A
  one-shot `(symbol, length, code)` construction that skips the
  intermediate `code` and `start` Vecs would help that quantile.
