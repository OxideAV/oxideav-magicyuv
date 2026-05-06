# oxideav-magicyuv

Pure-Rust MagicYUV lossless video codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 3 — clean-room rebuild.** Decodes the full native FOURCC
set: 8-bit (M8RG, M8RA, M8Y4, M8Y2, M8Y0, M8YA, M8G0) and 10/12/14-bit
(M0RG, M0RA, M2RG, M2RA, M4RG, M4RA, M0Y2, M0Y4, M0Y0, M0G0).
Honours `flags & FLAG_INTERLACED` for field-stride=2 prediction
(`spec/04` §5.1). A public encoder API (`encode_frame`, `encode_avi`,
`encode_avi_opendml`) emits wire-format frames + AVI envelopes
(single-RIFF AVI 1.0 and multi-RIFF OpenDML 2.0 per `spec/06` §6.1)
the decoder round-trips byte-for-byte. A `trace` Cargo feature
surfaces a JSONL trace tape for the Auditor's lockstep harness; round
3 fixed the `huff.used` field schema to the per-symbol `{length,
code}` map the audit/02 §4.2 forward spec required.

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
| Per-slice predictors   | spec/04 §4 (Left, Gradient, Median; modular 8-bit Median + standard JPEG-LS at 10/12/14-bit) |
| Interlaced field-stride| spec/04 §5.1 round-2 (top neighbour = row r-2; first 2 rows raw) |
| Per-plane Huffman      | spec/05 §1.1 (RLE descriptor)         |
| Canonical-code build   | spec/05 §2.0 (longest-length-first cumulative — **NOT** RFC 1951; auditor round 2 correction) |
| Raw-mode fallback      | spec/05 §4.1                           |
| AVI demuxer            | spec/06 (RIFF / strf / 00dc; OpenDML 2.0 multi-RIFF supported on both decode + encode in round 3) |

## Public API

- [`decode_frame`] — decode a single MAGY-prefixed frame's bytes.
  Returns one [`DecodedPlane`] per native plane; sample storage is
  `u8` for 8-bit FOURCCs and `u16` for 10/12/14-bit FOURCCs.
- [`encode_frame`] / [`encode_avi`] / [`encode_avi_opendml`] — public
  encoder API. The OpenDML variant takes a `RiffSegmentLimit` and
  emits a multi-RIFF (`AVI ` + `AVIX` continuations + `indx` super-index)
  envelope per `spec/06` §6.1.
- [`avi::AviReader`] — walk an AVI file's `00dc` chunks for end-to-end
  decode. Aggregates frames across every top-level RIFF in the file,
  so OpenDML 2.0 multi-RIFF inputs are demuxed transparently.
- [`header::parse`] — standalone v7 header parser, also re-used for
  the AVI `strf` extradata (byte-identical per spec/06 §4.1).
- [`Error`], [`Result`] — crate-local error type.
- `register(ctx)` (default-on `registry` feature) — wire the
  decoder into `oxideav-core`'s codec registry.

## Cargo features

- **`registry`** (default): wire the crate into `oxideav-core`'s
  codec registry. Standalone builds (`--no-default-features`)
  drop the `oxideav-core` dependency entirely.
- **`trace`** (off): emit JSONL trace events to the path in
  `OXIDEAV_MAGICYUV_TRACE_FILE` during decode. Used by the round-2
  Auditor's `jq`-line-diff lockstep harness against the cleanroom
  Python reference codec's `--trace` output.

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
