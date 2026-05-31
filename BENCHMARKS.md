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
| `decode_all_fourccs_640x480`   | all 17 | Gradient  | 640×480       | breadth sweep — one bench per native v7 FOURCC           |
| `encode_strategy_matrix_m8y0_640x480` | M8Y0 | all 4 strategies | 640×480 | encoder strategy × mode × interlaced matrix (24 cells) |

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

### 8. `BitWriter::with_capacity` for slice-payload encode

The per-slice Huffman / raw bit-packing path constructed a `BitWriter`
via `BitWriter::new()` — `Vec::new()` with zero pre-allocation.
`bw.write` then grew the backing `Vec<u8>` geometrically: a 1280×28
Auto-mode slice with raw_size ≈ 35840 paid ~17 `realloc`s walking from
1 → 2 → 4 → … → 65536, all copying the prefix. Same shape for the 8-bit
fresh-Huffman emit, the 10/12/14-bit Auto Huffman trial, the 10/12/14-bit
raw bit-pack, and the 10/12/14-bit fresh-Huffman emit — five sites total.

`BitWriter::with_capacity(byte_cap)` lets each call-site pre-size the
output buffer from a known upper bound (raw_size + 1 for the
size-comparison branches, `raw_size + raw_size / 2 + 1` for 8-bit
fresh-Huffman at `max_huff_len = 12`, `2 * raw_size + 1` for the
10/12/14-bit fresh-Huffman). The per-slice `payload` `Vec` is also
pre-sized to its known cap so the trailing `extend` doesn't pay one
more reallocation. `BitWriter::new()` is removed since every caller
moves to `with_capacity`; the public surface stays `pub(crate)` so no
downstream API is affected.

Encoder figures (`examples/quick_bench dynamic`, new scenario this
round — `EncodeOptions::dynamic_auto()` = `PredictorStrategy::Dynamic`
+ `SliceMode::Auto`, the v2.4.2 always-on adaptive combination per
spec/04 §3 + spec/05 §6.2). Same host, back-to-back pre/post within
a single boot:

| Scenario                            | Pre opt-8 | After opt-8 | Δ        |
| ----------------------------------- | --------: | ----------: | -------: |
| enc M8RG / dynamic / 1280×720       | 14.25 ms  | 13.93 ms    |  -2.2 % |
| enc M8Y0 / dynamic / 1280×720       |  7.45 ms  |  7.28 ms    |  -2.3 % |
| enc M8G0 / dynamic / 1920×1080      | 11.00 ms  | 10.90 ms    |  -1.0 % |
| enc M0RG / dynamic / 1280×720 / 10b | 20.80 ms  | 20.10 ms    |  -3.4 % |

Fixed-strategy encode (`enc … / gradient | left`, the prior
`time_encode` scenarios) is unchanged from opt-7 within run-to-run
noise; the saving lands specifically on the
Dynamic-strategy / Auto-mode path that owns the most BitWriters per
slice (three predictor trials × per-slice Huffman size probe ⇒ four
BitWriters per slice). Decode side is untouched.

All 83 unit + round-trip tests pass under `--all-features` and 74
under `--no-default-features`; trace MD5 is unchanged because the
trace emitter runs in the decoder only.

### 9. `HuffmanTable::build` allocation cleanups

Two micro-cleanups on the decoder build path the prior round flagged
as the "small-frame `~2-5 %` quantile of `decode_frame`":

1. `cur = start.clone()` (a fresh `Vec<u32>` of length `max_len + 2`,
   up to 80 B at the 14-bit alphabet's `max_len = 18`) is replaced by
   `cur = core::mem::take(&mut start)`, since `start` isn't read
   after the code-assignment phase. One allocation gone, no copy.
2. `prefix_to_idx: HashMap<u32, usize>` in the two-level path
   (`max_len > PRIMARY_BITS = 12`, i.e. every 10 / 12 / 14-bit
   alphabet) is replaced by a direct-indexed `Vec<i32>` of length
   `primary_size ≤ 4096`. Prefix values are bounded by `1 <<
   primary_bits ≤ 4096`, so a Vec lookup is one indexed load + one
   sentinel-check (`< 0`) instead of a hash compute + bucket walk.
   The map's heap header (~48 B + bucket array even at size 0) and
   per-probe SipHash cost are gone; the new Vec costs at most 16 KB
   (4096 × 4 B) zeroed once at build time.

Same observable `HuffmanTable`. The per-symbol `lengths`, `codes`,
and lookup-table contents are byte-identical to the prior path —
the symbol-order walk of `lengths` is preserved, so each prefix's
subtable lands at the same `secondary[]` index either way. Verified
by all 84 unit + round-trip tests under `--all-features` and 75
under `--no-default-features` (one new test:
`build_two_level_uses_per_prefix_subtables` exercises an alphabet
that places symbols in every distinct primary prefix bucket); trace
MD5 is unchanged.

End-to-end decode timings on the in-tree Apple M-series host
(`examples/quick_bench decode`, three runs back-to-back per side):

| Scenario                          | Baseline | After opt-9 | Δ        |
| --------------------------------- | -------: | ----------: | -------: |
| dec M8RG / gradient / 1280×720    | 11.37 ms |    11.34 ms | -0.3 %  |
| dec M8Y0 / gradient / 1280×720    |  5.26 ms |     5.21 ms | -0.9 %  |
| dec M8G0 / left   / 1920×1080     |  6.81 ms |     6.76 ms | -0.7 %  |
| dec M0RG / gradient / 1280×720    | 14.00 ms |    14.02 ms |  0.0 %  |
| dec M8RG / median   / 256×256     |  0.88 ms |     0.87 ms | -1.1 %  |
| dec M8RG / gradient / 128×128     |  0.18 ms |     0.18 ms |  0.0 %  |
| dec M0RG / gradient / 128×128     |  0.25 ms |     0.25 ms |  0.0 %  |

All within run-to-run noise. The build path is genuinely cold even
on the smallest 128×128 scenario (build is <50 µs out of ~180 µs
total decode); the saving is allocator-pressure reduction, not a
measurable speedup. The change ships anyway as a code-quality and
allocation-discipline win — the `HuffmanTable::build` flagged
candidate from opt-8 is closed below.

### 10. `HuffmanTable::decode_into_u16` inlined two-level hot loop

The 10/12/14-bit Huffman batch decoder used to walk
`self.decode(br)` once per pixel — `#[inline(always)]` on `decode`
let the body fold in, but the loop still re-loaded `self.max_len`,
`self.primary_bits`, the `primary` slice base, the `secondary`
slice base, and the `REDIRECT_MARKER` comparand from `&self` on
every iteration (the compiler couldn't prove `&self` immutable
across the inner `br.consume` mutation). The new shape mirrors
opt-2's `decode_into_u8`: hoist all five to local bindings once at
function entry, split single-level vs two-level at the loop
selector (the single-level branch covers well-formed-but-shallow
descriptors whose realised `max_len_used` lands at ≤
`PRIMARY_BITS = 12`), and run a flat peek/consume + table lookup
body so the BitReader's `acc` / `fill` / `pos` stay in registers
across the whole slice.

Two new
`decode_into_u16_matches_per_pixel_decode_{two_level,single_level}`
parity tests pin the batch helper against the per-pixel `decode()`
reference on hand-assembled MSB-first bit streams that bounce
across multiple primary-prefix buckets (two-level) and a shallow
10-bit alphabet (single-level), so any future hot-loop drift gets
caught the same way the round-2
`decode_into_u8_matches_per_pixel_decode` test caught off-by-ones
on the 8-bit batch path.

Decoder figures (`examples/quick_bench decode`, same Apple
M-series host within the same boot, 5-run medians per side):

| Scenario                           | Pre opt-10 | After opt-10 | Δ        |
| ---------------------------------- | ---------: | -----------: | -------: |
| dec M0RG / gradient / 1280×720     |   13.78 ms |     13.27 ms |  -3.7 %  |
| dec M0RG / gradient / 128×128 / 10b |    0.25 ms |     0.24 ms |  -4.0 %  |

The 8-bit M8RG / M8Y0 / M8G0 / median scenarios are unchanged
within run-to-run noise — their `decode_into_u8` path was already
inlined-loop-shaped from opt-2. Trace JSONL emitter is unaffected
(it lives in `decoder.rs` and consumes the same per-symbol stream
the batch helper produces; the trace MD5 on a 32×16 M8RG gradient
frame is bit-identical pre/post). All 86 unit + round-trip tests
pass under `--all-features` (was 84; the two new parity tests) and
77 under `--no-default-features` (was 75).

## Round-N+1 candidates

- **Predictor SIMD (Left only)** — Left has the simplest dependency
  chain (only `cur[c-1]`); a `wrapping_add` prefix-scan over u8x16
  using `core::simd` could give another 2-3 % on Gray (Left at
  1080p is the simplest predictor in our coverage). Carried forward
  from opt-7 — `core::simd` is nightly-only, so this needs the MSRV
  bump or a `core::arch::aarch64`/`core::arch::x86_64` fallback.

## Round 186 closed candidates (no-go)

- ~~**Auto-mode size probe without emit (spec/05 §6.2)**~~ —
  implemented as a `PlaneHuff::measure_bits_u{8,16}` precomputed
  sum-of-lengths followed by a conditional emit, then rolled back.
  The candidate was advertised as "net even on the typical case
  (Huffman usually wins so the bitstream is needed anyway)"; actual
  measurement on the Apple M-series host showed a +3 to +5 % encode
  regression across all four `quick_bench dynamic` scenarios when
  Huffman wins on the smooth synthetic plane. The extra read-only
  pass over the per-slice residuals costs more than the tightened
  `BitWriter` allocation saves; the raw-wins case (where the emit
  would have been wasted) is too rare on natural-image residuals
  for the savings to net out. Closing this candidate — a future
  retry would need a cheap proxy for "expect raw wins" so the probe
  only runs when it pays off, not unconditionally.

## Earlier round-N+1 candidates (resolved)

- ~~**`HuffmanTable::build`** itself is on the cold path but takes
  ~2-5 % of `decode_frame` for small frames (256×256 / smaller). A
  one-shot `(symbol, length, code)` construction that skips the
  intermediate `code` and `start` Vecs would help that quantile.~~
  *(partially resolved — opt-9 above)* — the `start` `Vec` is now
  reused as `cur` via `mem::take` (one allocation gone) and the
  two-level path's `HashMap<u32, usize>` is replaced by a
  direct-indexed `Vec<i32>` of length `≤ 4096`. The remaining
  `code: Vec<u32>` is still allocated; folding it directly into
  the primary/secondary writes would need a re-shape that walks
  symbols in tier order rather than symbol order and didn't fit
  this round's scope.

## Round 181 closed candidates (no-go)

- ~~**Fused encode-predictor + L1-score pass**~~ — implemented as
  `encode_predictor_u{8,16}_with_score`, then rolled back. The
  unfused two-pass shape compiles to autovectorized inner loops
  (LLVM SIMDifies the `(b as i8).abs()` scan in
  `abs_signed_sum_u8`); forcing the score into the predictor loop
  body suppressed that vectorization and regressed Dynamic-mode
  encode by ~3 % on every scenario. The reverse hypothesis — fewer
  memory passes wins — was wrong on this codebase at this slice
  size.

## Earlier resolved candidates

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
- ~~**`vec![0u8; w*h]` per decode**~~ *(resolved — `decode_into`)* —
  the per-frame `Vec` allocations (one per plane in `plane_bufs` +
  one per output `DecodedPlane` + the working clone of the G plane
  in RGB decorrelation reversal) are eliminated for streaming
  consumers by the new `decode_into(&mut DecodedFrame)` entry point.
  `decode_frame` is now a wrapper around `decode_into` so it also
  picks up the G-clone removal even on the fresh-allocate path.

## Round-194: cross-FOURCC decode-throughput sweep

The optimisation-round scenarios above measure five hand-picked
hot-path archetypes (one per Huffman / predictor / bit-depth /
subsampling combination). The new `decode_all_fourccs` Criterion
bench (`cargo bench -p oxideav-magicyuv --bench decode_all_fourccs`)
is the breadth complement: it covers **every** native MagicYUV v7
FOURCC (17 entries from `tables/00-fourcc-table.csv`) at a single
small resolution + predictor so per-format decode throughput can be
compared at a glance.

- **Resolution:** 640×480 (divisible by 4 in both dimensions so the
  4:2:0 / 4:2:2 chroma planes are whole-byte).
- **Predictor:** Gradient (available for every bit-depth — modular
  8-bit + JPEG-LS 10/12/14-bit — and produces residuals that exercise
  the Huffman path rather than collapsing to a single-symbol tree).
- **Slice mode:** Huffman.
- **Throughput numerator:** raw uncompressed plane bytes — for
  example M8Y0 (YUV 4:2:0) at 640×480 is 460 800 B and M0RA (10-bit
  RGBA) is 2 457 600 B. So the MiB/s figures are directly comparable
  as "decoded pixel volume per second", not bitstream consumption.

Apple M-series, macOS 26.1, Criterion `--quick` mode, two
back-to-back runs within the same boot; the figures below are
medians (the prior table's 5-run-median methodology).

| FOURCC | Bit | Family | Subsampling | Decode (ms) | Throughput (MiB/s) |
| ------ | --: | ------ | ----------- | ----------: | -----------------: |
| M8RG   |   8 | RGB    | n/a         |       3.697 |              237.7 |
| M8RA   |   8 | RGBA   | n/a         |       4.873 |              240.5 |
| M8Y4   |   8 | YUV    | 4:4:4       |       3.406 |              258.1 |
| M8Y2   |   8 | YUV    | 4:2:2       |       2.256 |              259.7 |
| M8Y0   |   8 | YUV    | 4:2:0       |       1.671 |              263.0 |
| M8YA   |   8 | YUVA   | 4:4:4:4     |       4.539 |              258.2 |
| M8G0   |   8 | Gray   | n/a         |       1.101 |              266.0 |
| M0Y2   |  10 | YUV    | 4:2:2       |       2.983 |              392.8 |
| M0RG   |  10 | RGB    | n/a         |       4.571 |              384.5 |
| M0RA   |  10 | RGBA   | n/a         |       6.045 |              387.7 |
| M2RG   |  12 | RGB    | n/a         |       4.546 |              386.7 |
| M2RA   |  12 | RGBA   | n/a         |       6.082 |              385.4 |
| M4RG   |  14 | RGB    | n/a         |       4.631 |              379.5 |
| M4RA   |  14 | RGBA   | n/a         |       6.183 |              379.0 |
| M0G0   |  10 | Gray   | n/a         |       1.484 |              394.8 |
| M0Y4   |  10 | YUV    | 4:4:4       |       4.476 |              392.7 |
| M0Y0   |  10 | YUV    | 4:2:0       |       2.226 |              394.8 |

### Reading the table

- **The 8-bit family clusters around 237-266 MiB/s.** RGB / RGBA are
  the slowest (237 / 240) — both pay the inter-plane G-B-R
  decorrelation reversal on top of per-plane Huffman + Gradient.
  Subsampled YUV gets cheaper as chroma shrinks (4:4:4 → 4:2:2 →
  4:2:0 ⇒ 258 → 260 → 263) because the chroma planes have fewer
  pixels at the same plane-overhead. Gray is the ceiling (266) —
  one plane, no cross-plane work.
- **The 10/12/14-bit family clusters around 379-395 MiB/s — a
  uniform ~50 % MiB/s improvement over the 8-bit family.** Each
  decoded pixel carries 2 bytes instead of 1 for the same Huffman
  + predictor pipeline cost per sample, so the MiB/s metric
  (which weights bytes) rises even though the per-pixel time is
  longer (e.g. M0RG 4.571 ms vs M8RG 3.697 ms). This is the
  expected shape — the hot loop is per-sample, not per-byte.
- **The packed-`u32` primary-table layout (opt-6) + inlined two-level
  hot loop (opt-10) keep the 10/12/14-bit gap stable across bit-depths.**
  M0RG (10-bit) → M2RG (12-bit) → M4RG (14-bit) walk 385 → 387 → 380
  MiB/s, a ≤ 2 % spread across three different `max_huff_len` values
  (14 / 16 / 18). The per-pixel two-level lookup cost is essentially
  flat now — the residual variation is the per-symbol bit-stream length
  (deeper alphabets carry longer codes on average so the BitReader
  drains faster).
- **Alpha (the `*RA` / `*YA` variants) costs roughly one extra plane
  of decode time** as expected — M8RA - M8RG = 4.873 - 3.697 = 1.18 ms,
  which matches a fourth 640×480 8-bit plane decode at the per-plane
  rate the Gray scenario sets (M8G0 = 1.101 ms for one 640×480 plane).
  10-bit shows the same shape: M0RA - M0RG = 1.47 ms ≈ M0G0 = 1.48 ms.

### Followups

None as actionable optimisations yet — the spread is uniform within
each bit-depth tier and the gap between tiers is the per-sample-vs-
per-byte arithmetic, not a hot-path drift. The bench is now part of
the regression surface so any future change that drifts one FOURCC
relative to its tier-mates will surface immediately.

## Round-200: encoder strategy × mode × interlaced matrix

The per-FOURCC `encode` bench (5 hand-picks) and the
`decode_all_fourccs` breadth sweep both fix
`strategy = Fixed(_)` + `mode = Huffman` + `interlaced = false`,
leaving the encoder's other axes invisible to Criterion. The new
`encode_strategy_matrix` bench
(`cargo bench -p oxideav-magicyuv --bench encode_strategy_matrix`)
walks every combination of the public `EncodeOptions` knobs at a
single representative FOURCC so a regression on any of them lands as
a single anomalous cell in the matrix readout:

- **Strategy axis (4 cells):** `Fixed(Left)`, `Fixed(Gradient)`,
  `Fixed(Median)`, **`Dynamic`** (spec/04 §3 — runs all three
  predictors per slice and keeps the smallest residual sum, ~3× the
  prediction-side work of any Fixed strategy).
- **Mode axis (3 cells):** `Huffman`, `Raw`, **`Auto`** (spec/05 §6.2 —
  sizes both the Huffman pack and the bit-packed raw payload per slice
  and picks the smaller, doubling the bit-pack accounting on slices
  where Huffman wins).
- **Interlaced axis (2 cells):** progressive vs `flags & FLAG_INTERLACED`
  on (`spec/04` §5.1 field-stride=2 prediction — same predictor
  kernels, different neighbour geometry).

24 cells total. FOURCC pick: **M8Y0** (8-bit YUV 4:2:0). Subsampled
chroma exercises the cross-plane-size dispatch the RGB-only Strategy
matrix would miss; modular-Median 8-bit is faster than JPEG-LS Median
10/12/14-bit so the Median × Dynamic × Auto cell finishes in a
reasonable wall time. Resolution: 640×480 (same as
`decode_all_fourccs` for cross-bench comparability). Throughput is
raw uncompressed plane bytes (M8Y0 4:2:0 at 640×480 = 460 800 B).

Apple M-series, macOS 26.1, Criterion default measurement, single
boot. Time figures are medians from 20 samples × 3 s window.

| Strategy        | Mode     | Interlaced  | Encode (ms) | Throughput (MiB/s) |
| --------------- | -------- | ----------- | ----------: | -----------------: |
| Fixed(Left)     | Huffman  | progressive |       1.915 |              229.5 |
| Fixed(Left)     | Huffman  | interlaced  |       1.983 |              221.6 |
| Fixed(Left)     | Raw      | progressive |       1.219 |              360.6 |
| Fixed(Left)     | Raw      | interlaced  |       1.237 |              355.5 |
| Fixed(Left)     | Auto     | progressive |       2.043 |              215.1 |
| Fixed(Left)     | Auto     | interlaced  |       2.069 |              212.4 |
| Fixed(Gradient) | Huffman  | progressive |       1.910 |              230.1 |
| Fixed(Gradient) | Huffman  | interlaced  |       1.940 |              226.5 |
| Fixed(Gradient) | Raw      | progressive |       1.254 |              350.5 |
| Fixed(Gradient) | Raw      | interlaced  |       1.276 |              344.5 |
| Fixed(Gradient) | Auto     | progressive |       1.895 |              231.9 |
| Fixed(Gradient) | Auto     | interlaced  |       1.906 |              230.6 |
| Fixed(Median)   | Huffman  | progressive |       2.060 |              213.4 |
| Fixed(Median)   | Huffman  | interlaced  |       2.074 |              211.9 |
| Fixed(Median)   | Raw      | progressive |       1.314 |              334.5 |
| Fixed(Median)   | Raw      | interlaced  |       1.331 |              330.2 |
| Fixed(Median)   | Auto     | progressive |       2.074 |              211.9 |
| Fixed(Median)   | Auto     | interlaced  |       2.090 |              210.3 |
| Dynamic         | Huffman  | progressive |       2.555 |              172.0 |
| Dynamic         | Huffman  | interlaced  |       2.500 |              175.8 |
| Dynamic         | Raw      | progressive |       1.731 |              253.9 |
| Dynamic         | Raw      | interlaced  |       1.765 |              249.0 |
| Dynamic         | Auto     | progressive |       2.451 |              179.3 |
| Dynamic         | Auto     | interlaced  |       2.484 |              176.9 |

### Reading the matrix

- **Raw cells are uniformly ~1.5× faster than Huffman cells** across
  every strategy (e.g. `Fixed(Gradient)` 1.254 ms raw vs 1.910 ms
  Huffman = 1.52×). Raw skips canonical-Huffman build + per-sample
  Huffman pack and just bit-packs residuals. This is the "encoder
  upper bound" for the chosen predictor — useful when triaging
  encoder regressions: a Huffman-side regression that shows up as a
  proportional slowdown in raw too is a predictor regression, not a
  Huffman one.
- **`Dynamic` is 1.3-1.4× the cost of any `Fixed(_)` Huffman cell**
  (2.555 ms vs 1.91-2.06 ms). That matches the spec: Dynamic evaluates
  Left + Gradient + Median per slice and keeps the smallest residual,
  so the predictor side does roughly 3× the work — but the predictor
  is < 30 % of total encode time (the Huffman pack dominates), so the
  observed factor is well below 3×. The same pattern holds for raw:
  Dynamic + Raw (1.731 ms) is exactly the sum of one Fixed-Raw plus
  two extra predictor passes (no extra bit-pack work, since only the
  winning predictor's residuals get packed).
- **`Auto` matches Huffman cell-for-cell** for the Gradient and Median
  strategies (1.895 ms vs 1.910 ms, 2.074 ms vs 2.060 ms) — the
  per-slice raw-size accounting is cheap and the Auto path keeps
  picking Huffman because the residual is compressible. The only
  outlier is `Fixed(Left)` + Auto (2.043 ms vs 1.915 ms Huffman) —
  Left residuals on this synthetic input are noisier than Gradient's,
  so the Auto path's raw-size comparison shows up. Still well within
  the noise budget for a 24-cell matrix.
- **Interlaced is uniformly 1-3 % slower than progressive**, as
  expected: same predictor kernels, the field-stride=2 dispatch
  splits the inner loop into two half-height passes and the first
  two rows of each field are raw-passed (more bit-stream emission
  per row at the start of every slice). Within noise everywhere
  except the Raw cells where the slower-loop effect lines up cleanly.

### Followups

None as actionable optimisations yet — every cell reads as expected
within the spec's predicted cost shape. The Dynamic + Auto path is
now timed and could be revisited if a future optimisation targets
the per-slice predictor-selection inner loop. The bench is now part
of the regression surface: any future change that drifts one cell
relative to its row-/column-mates will surface immediately on
`cargo bench -p oxideav-magicyuv --bench encode_strategy_matrix`.
