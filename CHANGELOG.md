# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Clean-room rebuild from a fresh orphan `master`. The previous
  implementation was retired alongside issue
  [#3](https://github.com/OxideAV/oxideav-magicyuv/issues/3) (the
  prior trace document cited FFmpeg `libavcodec/magicyuv*.c` as the
  writeup's basis, which does not satisfy clean-room separation); the
  prior history is preserved on the `old` branch.
- The new code is being written against the strict-isolation
  clean-room workspace at
  [`docs/video/magicyuv/`](https://github.com/OxideAV/docs/tree/master/video/magicyuv),
  which completed six Specifier rounds, two Auditor rounds, an
  Implementer-Python reference round (round 9), and three Validator
  rounds (11–13) before this orphan reset. The Implementer in this
  repo reads only `spec/00..06` and `tables/` — no FFmpeg source, no
  proprietary binary, no Python reference source, no `old` branch.
