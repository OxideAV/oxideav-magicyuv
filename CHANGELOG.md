# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.2...v0.0.3) - 2026-05-06

### Other

- prepend contestation notice (issue #3) + clean-room rebuild plan
- drop committed Cargo.lock + relax oxideav-core to "0.1"
- pure-Rust 8/10-bit MagicYUV encoder + decoder horizontal tiles + 10-bit decode

### Added

- **Encoder.** Pure-Rust `Frame -> Packet` encoder mirroring the
  decoder. Supports all seven 8-bit format codes the FFmpeg encoder
  emits (`M8RG`/`M8RA`/`M8Y4`/`M8Y2`/`M8Y0`/`M8YA`/`M8G0`), all three
  predictors (LEFT/GRADIENT/MEDIAN), arbitrary `nb_slices_x ×
  nb_slices_y` slice grids, plus the four 10-bit codes the decoder
  understands (`M0Y2`/`M0Y4`/`M0Y0`/`M0G0`).
  - Histogram-driven canonical-Huffman length builder
    (`huffman::build_lengths_from_histogram`) that pads zero-frequency
    symbols to a placeholder length so the resulting code is a complete
    canonical prefix code (Σ 2^-len = 1) — required by FFmpeg's
    `ff_vlc_init_multi_from_lengths` decoder-side validator.
  - Wire-code synthesiser (`huffman::build_canonical_codes`) that
    inverts the decoder's "high-first with descending-symbol tiebreak"
    canonical convention.
  - MSB-first `bitstream::BitWriter` mirror of the existing
    `BitReader`, plus encoder-side predictor functions
    (`predictor::encode_{left,gradient,median}_{u8,u16}`).
- **Horizontally-tiled slices** decoded. The decoder previously
  rejected `slice_width != width` (matching FFmpeg's `PATCHWELCOME`);
  it now handles arbitrary `nb_slices_x × nb_slices_y` rectangular
  grids. Each (slice_x, slice_y) tile carries its own
  (flag, predictor) prefix and Huffman-coded residual stream;
  predictors operate on the tile rectangle (LEFT for the tile's first
  row, predictor's "top" neighbour from the tile's previous row, etc.).
- **10-bit decode** for the four format bytes the FFmpeg decoder
  recognises that have a corresponding `oxideav-core::PixelFormat`
  variant: `0x6c` (YUV422P10 → `Yuv422P10Le`), `0x73` (GRAY10 →
  `Gray10Le`), `0x76` (YUV444P10 → `Yuv444P10Le`), `0x7b` (YUV420P10
  → `Yuv420P10Le`). Predictor + Huffman + raw paths all use `u16`
  arithmetic with `mask = (1 << bps) - 1`. Higher-bit-depth GBRP /
  GBRAP / 12-bit / 14-bit codes remain `Unsupported` pending core
  pixel-format additions.
- **FFmpeg cross-decode tests.** Eleven new integration tests build a
  minimal AVI container around the encoder output and feed it back to
  `ffmpeg -c:v magicyuv`. All eleven (yuv422p/yuv420p/yuv444p/gray8/
  gbrp × predictors, plus a multi-slice case and a 320×240 case)
  round-trip bit-exactly through ffmpeg's reference decoder.

## [0.0.2](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.1...v0.0.2) - 2026-05-04

### Other

- remove dead BitReader::consume + unused chroma_h_sub helper
- add unit tests for SliceOffsetTable parser + apply rustfmt
- fix multi-slice (>1) decode by tracking per-slice ends explicitly

### Fixed (pre-0.0.2)

- Multi-slice (nb_slices > 1) frames now decode bit-exactly. The wire
  layout's "plane-major slice starts" is **not** monotone within a
  single plane: the encoder writes slice payloads slice-major
  (plane 0 slice 0, plane 1 slice 0, ..., plane 0 slice 1, ...), so a
  slice's end byte is the start of its **file-order** successor, not
  the same plane's next slice. Replaced the previous
  `offsets[plane][slice + 1]`-as-end heuristic (which corrupted the
  end-byte of every non-last slice for multi-slice frames) with
  explicit `starts` + `ends` vectors derived from the file-order
  successor relation. New 4-slice forced-via-`-slices N` interop tests
  cover the regression.

### Added (pre-0.0.2)

- Interop tests for the previously-untested `M8RA` (GBRAP → packed
  Rgba) and `M8YA` (YUVA 4:4:4 → 4-plane Y/U/V/A) format codes,
  matching the existing per-predictor coverage of the YUV/GBRP paths.
- Forced multi-slice interop tests (`-slices 4` on 64×48) for
  `yuv422p`/`median` and `gbrp`/`left`, exercising the slice-major
  file-order interleaving on small frames.


## [0.0.1] - 2026-05-02

### Added

- Initial scaffold: clean-room MagicYUV decoder built from the
  behavioural trace under `docs/video/magicyuv/`. No third-party
  source consulted.
- 8-bit decode for `M8RG` (GBRP), `M8RA` (GBRAP), `M8Y4` (YUV444),
  `M8Y2` (YUV422), `M8Y0` (YUV420), `M8YA` (YUVA444), `M8G0`
  (GRAY8) — every format the FFmpeg encoder emits.
- All three spatial predictors (LEFT, GRADIENT, MEDIAN) plus
  raw-mode fallback (slice flag bit 0).
- Multi-slice frames; canonical-Huffman length-descriptor decoder
  with both the single-byte and the run-length form (§3.5).
- Implicit GBR↔RGB decorrelation for the `M8RG` / `M8RA` formats,
  emitted to packed `Rgb24`.
- AVI-tag registration: `M8RG`, `M8RA`, `M8Y4`, `M8Y2`, `M8Y0`,
  `M8YA`, `M8G0` map to the magicyuv codec id.
- Unit tests for canonical Huffman, predictors, length-descriptor
  parser. Integration test driving FFmpeg as a black-box encoder
  and asserting bit-exact decode for YUV422P / GBRP / GRAY8.
