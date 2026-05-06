# oxideav-magicyuv

Pure-Rust MagicYUV lossless video codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 1 — clean-room rebuild.** Decodes the 8-bit native
FOURCC family of MagicYUV v7 streams: **M8RG, M8RA, M8Y4, M8Y2,
M8Y0, M8YA, M8G0**. The 10-/12-/14-bit FOURCCs (M0/M2/M4) are
spec-feasible but Validator-unverified at byte-exactness — they're
deferred to a later round and currently surface as
`Error::UnsupportedFormatByte`.

The implementation is built against the strict-isolation clean-room
workspace at
[`docs/video/magicyuv/`](https://github.com/OxideAV/docs/tree/master/video/magicyuv).
That workspace completed six Specifier rounds, two Auditor rounds,
an Implementer-Python reference round, and three Validator rounds
before this orphan reset. The Implementer in this repo reads only
`spec/00..06` plus `tables/00-fourcc-table.csv` and
`tables/01-predictor-table.csv` — no FFmpeg source, no proprietary
binary, no Python reference source, no `old` branch.

## Pipeline

| Stage                  | Source                                |
| ---------------------- | ------------------------------------- |
| 32-byte v7 header      | spec/01 §3 (audit-corrected aux_byte / slice_height) |
| Slice table + preamble | spec/02 §5..§7                        |
| Plane-major plane order| spec/03 §4..§6 (RGB wire order audit-corrected) |
| Per-slice predictors   | spec/04 §4 (Left, Gradient, Median; modular 8-bit Median) |
| Per-plane Huffman      | spec/05 §1.1 (RLE descriptor)         |
| Canonical-code build   | spec/05 §2.0 (longest-length-first cumulative — **NOT** RFC 1951; auditor round 2 correction) |
| Raw-mode fallback      | spec/05 §4.1                           |
| AVI demuxer            | spec/06 (RIFF / strf / 00dc; OpenDML out of scope for round 1) |

## Public API

- [`decode_frame`] — decode a single MAGY-prefixed frame's bytes.
- [`avi::AviReader`] — walk an AVI file's `00dc` chunks for end-to-end
  decode.
- [`header::parse`] — standalone v7 header parser, also re-used for
  the AVI `strf` extradata (byte-identical per spec/06 §4.1).
- [`Error`], [`Result`] — crate-local error type.
- `register(ctx)` (default-on `registry` feature) — wire the
  decoder into `oxideav-core`'s codec registry.

## Cargo features

- **`registry`** (default): wire the crate into `oxideav-core`'s
  codec registry. Standalone builds (`--no-default-features`)
  drop the `oxideav-core` dependency entirely and expose only the
  pure-Rust decoder API.

## Why clean-room

MagicYUV is a closed-source commercial codec by Pavel Zlatev / "ignus"
(`magicyuv.com`). Reverse-engineering it from the proprietary binary
is permitted under 17 U.S.C. §1201(f) (DMCA interoperability
exemption), *Sega v. Accolade* / *Sony v. Connectix*, and EU Directive
2009/24/EC Articles 5(3) + 6. The cleanroom workspace at
`docs/video/magicyuv/` reverse-engineered the v7 layout from the
proprietary `magicyuv.dll` directly; the Implementer in this repo is
wall-isolated from any FFmpeg-derived material.

## Implementer allow-list

- `docs/video/magicyuv/spec/00..06`
- `docs/video/magicyuv/tables/00-fourcc-table.csv`
- `docs/video/magicyuv/tables/01-predictor-table.csv`
- `oxideav-core`'s public API

## Implementer forbidden-input list

- FFmpeg `libavcodec/magicyuv*.c` and any FFmpeg-derived material.
- The retired `old` branch of this repository.
- `docs/video/magicyuv/reference/binaries/` (proprietary binary).
- `docs/video/magicyuv/reference-impl/python/` (cleanroom Python
  reference codec).
- Any third-party MagicYUV decoder source.
