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

## Known follow-ups (not part of this round)

- Encoder Huffman tree builder (`encoder::canonical_huffman_lengths` →
  `enforce_length_cap`) has degenerate behaviour on highly-skewed
  histograms (e.g. Median predictor at ≥ 1024×1024 with smooth-gradient
  inputs). Symptoms: per-frame encode wall-time blows up from low
  milliseconds to many minutes. The decoder is unaffected; the spec-
  compliant streams produced by Gradient/Left at the same sizes decode
  identically. Fix is encoder-side (length-cap algorithm + length-limited
  Package-Merge), tracked separately.
