# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round-1 clean-room MagicYUV v7 decoder for the 8-bit native
  FOURCC family: **M8RG, M8RA, M8Y4, M8Y2, M8Y0, M8YA, M8G0**.
  - 32-byte v7 frame-header parser (`spec/01` §3) honouring all
    five Auditor-round-1 inline corrections (audit-corrected
    `aux_byte = max_huffman_code_length`, `+0x1c =
    slice_height`, encoder allowlist mask polarity correction,
    on-wire `predictor_id` is per-slice rather than the
    `+0x0b codec_variant` byte, RGB-family wire order is `(B', G,
    R')` after `(B − G, R − G)` decorrelation).
  - Slice-table walker (`spec/02` §5) supporting plane-major
    preamble layout and arbitrary `slice_height` (not assumed = 28).
  - Per-slice prediction (`spec/04` §4): Left (modular `+`),
    Gradient (`left + top - top_left` mod 256), and the
    audit-corrected modular 8-bit Median formula. The 10/12/14-bit
    Medians (standard JPEG-LS per round-2 validation note) are
    deferred along with their FOURCCs.
  - Per-plane canonical Huffman built via the audit-corrected
    longest-length-first cumulative algorithm of `spec/05` §2.0
    (Auditor round 2 correction; **not** RFC 1951 §3.2.2).
    Run-length-encoded length descriptor parsing per
    `spec/05` §1.1.
  - Per-slice raw-mode fallback (`spec/05` §4.1).
  - RGB inter-plane decorrelation reversal: `B = (B' + G) mod 256`,
    `R = (R' + G) mod 256`, with output in the user-facing
    `(G, B, R)[, A]` plane order (`spec/03` §4 audit-corrected).
  - Minimal RIFF/AVI demuxer (`spec/06`): walks `RIFF AVI` ↦
    `LIST hdrl` ↦ `strl` ↦ `strf` to extract the 32-byte MAGY
    extradata (validated against the per-frame header) and emits
    `00dc` chunk payloads to `decode_frame`. OpenDML 2.0
    super-index support is out of scope for round 1.
  - `oxideav-core` framework integration behind the default-on
    `registry` Cargo feature: `register(ctx)` installs a
    `Decoder` factory under codec id `"magicyuv"` mapping each
    decoded frame into a `VideoFrame` with planes packed into
    `Rgb24` / `Rgba` for RGB families, `Gray8` for M8G0, planar
    Y/U/V for the YUV families, and Y/U/V/A for M8YA.
  - 37 lib tests across the seven 8-bit FOURCCs × three
    predictors × Huffman / raw modes × multiple patterns plus
    bit-reader, predictor-roundtrip, header-rejection, AVI
    end-to-end, and registry round-trip checks. All green; no
    `#[ignore]`.

### Notes

- The `tables/00-fourcc-table.csv` and `tables/01-predictor-table.csv`
  artefacts are loaded once at startup via `include_str!` and parsed
  lazily; values are never retyped from spec into Rust source.
- The implementation reads `slice_height` from the header rather
  than assuming `28`, satisfying `spec/02` §10 question 1.
- Standalone (`--no-default-features`) builds drop the `oxideav-core`
  dependency entirely. Standalone test suite is 35/35 green.
