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

### 6. Decoder primary-table packed `u32` layout

`HuffmanTable::primary` was `Vec<(u32, u8)>` — each slot paid 8 B due
to alignment padding (5 B used, 3 B wasted). Replaced with `Vec<u32>`
that packs `(symbol_or_subtable_index, length_or_marker)` into a single
4-B word (low 8 b = length, high 24 b = symbol). Same applies to the
per-prefix secondary subtables. Halves the primary working set from
16 KB → 8 KB per plane at `max_len = 18` (the 10/12/14-bit alphabets),
and cuts the 8-bit hot loop's per-pixel load from an 8-B tuple to a
single 4-B `u32` read. A new unit test
(`pack_entry_round_trip_terminal_and_redirect`) asserts every legal
`(length, symbol)` pair in the primary's range survives the
pack→unpack cycle and that `REDIRECT_MARKER = 0xff` is unambiguous
against any terminal length (1..=18); another
(`decode_into_u8_matches_per_pixel_decode`) asserts the batch helper
and per-pixel `decode` produce the same symbol stream from the same
bit input on a real-world descriptor.

Decoder figures (this-round only, opt-5 → opt-6 on the same host
within the same boot — back-to-back pre/post measurements so the
delta is not contaminated by thermal / scheduler drift). Encoder is
unchanged because it builds canonical lengths via Package-Merge
directly and never constructs a `HuffmanTable`:

| Scenario                          | Pre opt-6 | After opt-6 | Δ        |
| --------------------------------- | --------: | ----------: | -------: |
| dec M8RG / gradient / 1280×720    | 12.10 ms  | 11.00 ms    |  -9.1 % |
| dec M8Y0 / gradient / 1280×720    |  5.65 ms  |  5.06 ms    | -10.5 % |
| dec M8G0 / left   / 1920×1080     |  7.42 ms  |  6.52 ms    | -12.1 % |
| dec M0RG / gradient / 1280×720    | 14.22 ms  | 13.44 ms    |  -5.5 % |
| dec M8RG / median   / 256×256     |  0.95 ms  |  0.82 ms    | -13.4 % |

### 7. `decode_into(&mut DecodedFrame)` streaming entry point

`decode_frame` was a fresh-allocate path: every call allocated 3-4
per-plane `Vec<u8>` / `Vec<u16>` (one per plane), plus a working
clone of the G plane inside the RGB inter-plane decorrelation
reversal (the prior code couldn't borrow B'/R' mutably *and* G
immutably from the same `Vec<Vec<u8>>` so it cloned G). The new
`decode_into(&[u8], &mut DecodedFrame)` decodes into a caller-owned
`DecodedFrame` and reuses its per-plane `Samples::U8` / `Samples::U16`
storage in place when geometry matches — `Vec::clear` + `resize`
keeps the existing allocation, and `as_ptr()` + `capacity()` are
stable across iterations (verified by the
`decode_into_reuses_plane_storage_when_geometry_matches` unit test).

The RGB decorrelation reversal (8-bit and high-bit-depth) is
rewritten to use disjoint `split_at_mut` borrows of `[B', G, R']`,
so the prior `wire_planes[1].clone()` working copy is gone —
`decode_frame` itself picks up the same saving since it now wraps
`decode_into`.

`examples/quick_bench`'s decode helper now times both paths
back-to-back. Decode-only deltas (`into=` column shows the
`decode_into` ms; the percentage in parens is `(decode_frame -
decode_into) / decode_frame`):

| Scenario                          | decode_frame | decode_into | Δ        |
| --------------------------------- | -----------: | ----------: | -------: |
| M8RG / gradient / 1280×720        |    10.20 ms  |   10.00 ms  |  -2.0 % |
| M8Y0 / gradient / 1280×720        |     4.54 ms  |    4.54 ms  |  -0.1 % |
| M8G0 / left   / 1920×1080         |     6.03 ms  |    5.97 ms  |  -0.9 % |
| M0RG / gradient / 1280×720 / 10b  |    12.39 ms  |   12.29 ms  |  -0.8 % |

The RGB-family path benefits the most because it eliminates four
`Vec` allocations per frame (three wire planes + the G clone) versus
one (for Gray) or three (for YUV-4:2:0); chroma planes are also
much smaller. Pure-malloc savings are a larger fraction of total
work at smaller frame sizes — e.g. a 128×128 M8RG decode is a few
hundred microseconds, mostly malloc — so streaming consumers that
iterate over thumbnails or tile decodes will see a bigger win than
the 1080p timings above. The trace JSONL emitter is unchanged
(`decode_into` re-uses the storage but emits the same event sequence
in the same order); all 83 tests under `--all-features` continue to
pass.

## Final cumulative deltas

| Scenario                             | Decode Δ | Encode Δ |
| ------------------------------------ | -------: | -------: |
| M8RG / gradient / 1280×720           |  -47.4 % |  -43.2 % |
| M8Y0 / gradient / 1280×720           |  -52.9 % |  -42.7 % |
| M8G0 / left   / 1920×1080            |  -54.5 % |  -43.7 % |
| M0RG / gradient / 1280×720 / 10-bit  |  -44.6 % |  -39.1 % |
| M8RG / median   / 256×256            |  -47.7 % |    n/a   |

## Trace-feature lockstep

The `--features trace` JSONL emitter is bit-identical before and
after this round (verified by MD5 of the tape on a 32×16 M8RG
gradient frame: `5744c8060e6a3bccd53e1abf05ad6846` on both
`f8624ad` baseline and `d98b090` post-opt-5). The opt-6 packed
primary-table change does not touch the trace emitter
(`HuffmanTable::codes()` still surfaces the per-symbol canonical
codes from a separate `codes: Vec<u32>` field, which the packed
layout doesn't change). All 78 tests pass under `--features trace`.

## Round-N+1 candidates

- ~~**Encoder `canonical_huffman_lengths` → `enforce_length_cap`**~~
  *(resolved — package-merge limiter)* — the prior `enforce_length_cap`
  "steal-a-bit" heuristic both spun for millions of iterations on
  highly-skewed (Fibonacci / near-geometric) residual histograms and
  left an *invalid* over-long code (Kraft sum ≪ 1). It is replaced by
  the length-limited **Package-Merge** algorithm (Larmore & Hirschberg
  1990), which is only invoked when the unbounded-optimal tree exceeds
  the per-bit-depth cap (12 / 14 / 16 / 18, spec/05 §1) and produces an
  optimal *length-limited* prefix code with Kraft sum exactly 1.0
  (spec/05 §1.3). The common (non-binding) path keeps the plain
  canonical lengths byte-for-byte, so the trace lockstep is unchanged.
- ~~**Decoder primary-table layout**~~ *(resolved — packed `u32`
  layout)* — the `Vec<(u32, u8)>` 8-B-per-slot layout is now a flat
  `Vec<u32>` packing length into the low 8 b and symbol /
  subtable-index into the high 24 b. Halves the primary working set
  (16 KB → 8 KB per plane at `max_len = 18`), and the 8-bit hot loop
  now does a single 4-B load per pixel instead of an 8-B tuple
  fetch. The `Vec<u16>` candidate alternative was discarded: the
  10/12/14-bit alphabets exceed the 10-bit symbol field and the
  redirect marker can't be encoded in 6-bit length, so the 4-B
  packed `u32` is the right balance of generality and memory.
- **Predictor SIMD (Left only)** — Left has the simplest dependency
  chain (only `cur[c-1]`); a `wrapping_add` prefix-scan over u8x16
  using `core::simd` could give another 2-3× on Gray (Left at
  1080p is the simplest predictor in our coverage).
- ~~**`vec![0u8; w*h]` per decode**~~ *(resolved — `decode_into`)* —
  the per-frame `Vec` allocations (one per plane in `plane_bufs` +
  one per output `DecodedPlane` + the working clone of the G plane
  in RGB decorrelation reversal) are eliminated for streaming
  consumers by the new `decode_into(&mut DecodedFrame)` entry point.
  `decode_frame` is now a wrapper around `decode_into` so it also
  picks up the G-clone removal even on the fresh-allocate path.
- **`HuffmanTable::build`** itself is on the cold path but takes
  ~2-5 % of `decode_frame` for small frames (256×256 / smaller). A
  one-shot `(symbol, length, code)` construction that skips the
  intermediate `code` and `start` Vecs would help that quantile.
