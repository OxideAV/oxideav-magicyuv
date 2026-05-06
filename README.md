# oxideav-magicyuv

Pure-Rust MagicYUV lossless video codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 0 — clean-room rebuild scaffold.** This `master` branch is a
fresh orphan. The previous implementation was retired alongside the
methodology challenge filed as
[issue #3](https://github.com/OxideAV/oxideav-magicyuv/issues/3) (the
prior trace document cited FFmpeg `libavcodec/magicyuv*.c` as the
writeup's basis); the prior history is preserved on the `old` branch
for archival.

The new implementation will be built against the strict-isolation
clean-room workspace at
[`docs/video/magicyuv/`](https://github.com/OxideAV/docs/tree/master/video/magicyuv),
which has completed the Specifier (six rounds), Auditor (two rounds),
Implementer-Python (round 9), and Validator (rounds 11–13)
clean-room phases. The full v7 wire-format specification — covering
the file header, slice table, plane mapping, three predictors,
non-RFC-1951 longest-length-first canonical Huffman entropy coding,
and AVI carriage — is consumable from `spec/00..06` and `tables/`.

Scope (per the cleanroom v7 lower-bound table):

| Family | Bit depths | Subsamplings | Alpha | Interlace |
| ------ | ---------- | ------------ | ----- | --------- |
| YUV    | 8/10/12/14 | 4:0:0 / 4:2:0 / 4:2:2 / 4:4:4 | 4:4:4 | yes |
| RGB    | 8/10/12/14 | 4:4:4 (planar GBR) | yes | n/a |
| Gray   | 8/10/12/14 | n/a | n/a | n/a |

## Why clean-room

MagicYUV is a closed-source commercial codec by Pavel Zlatev / "ignus"
(`magicyuv.com`). Reverse-engineering it from the proprietary binary
is permitted under 17 U.S.C. §1201(f) (DMCA interoperability
exemption) + *Sega v. Accolade* / *Sony v. Connectix*, and
EU Directive 2009/24/EC Articles 5(3) + 6. Reading FFmpeg's
LGPL `libavcodec/magicyuv*.c` to understand the format is **not**
the path the project chose: the cleanroom workspace at
`docs/video/magicyuv/` reverse-engineered the v7 layout from the
proprietary `magicyuv.dll` directly, and the Implementer in this repo
is wall-isolated from any FFmpeg-derived material.

## Implementer allow-list

- `docs/video/magicyuv/spec/00..06` — natural-language wire-format
  description authored across six Specifier rounds + two Auditor
  rounds.
- `docs/video/magicyuv/tables/00-fourcc-table.csv` and
  `01-predictor-table.csv` — extraction artifacts; load via
  `include_str!` / `include_bytes!`, do not retype.
- `oxideav-core`'s public API (decoder/encoder traits,
  `RuntimeContext`, `VideoFrame`, `PixelFormat`, `Error`, etc.).

## Implementer forbidden-input list

- FFmpeg `libavcodec/magicyuv*.c`, `libavformat/riff.c` — taints
  the wall.
- The retired `audio/tta/`-style `magicyuv-trace-reverse-engineering.md`
  doc that was deleted from `OxideAV/docs` in commit `937a346`.
- The `old` branch of this repository — that's the retired
  contaminated implementation.
- `docs/video/magicyuv/reference/binaries/` (proprietary binary;
  reserved for the Auditor).
- `docs/video/magicyuv/reference-impl/python/` (the cleanroom Python
  reference codec; the Implementer reads spec + tables only, not
  another implementation's source).
- Any third-party MagicYUV decoder source (multimedia.cx walkthroughs,
  other Rust crates, etc.).
