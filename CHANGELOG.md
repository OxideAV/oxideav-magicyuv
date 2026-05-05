# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.2](https://github.com/OxideAV/oxideav-magicyuv/compare/v0.0.1...v0.0.2) - 2026-05-04

### Other

- remove dead BitReader::consume + unused chroma_h_sub helper
- add unit tests for SliceOffsetTable parser + apply rustfmt
- fix multi-slice (>1) decode by tracking per-slice ends explicitly

### Fixed

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

### Added

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
