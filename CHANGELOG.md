# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
