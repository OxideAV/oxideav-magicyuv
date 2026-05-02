# oxideav-magicyuv

Pure-Rust **MagicYUV** lossless intra-only video decoder. Clean-room
implementation built from the behavioural trace at
`docs/video/magicyuv/magicyuv-trace-reverse-engineering.md`. Zero C
dependencies; no third-party source consulted.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-magicyuv = "0.0"
```

## What works today

| Area                            | State                                        |
|---------------------------------|----------------------------------------------|
| Bitstream version               | v7 (the only version FFmpeg recognises)      |
| 8-bit format codes              | `M8RG`, `M8RA`, `M8Y4`, `M8Y2`, `M8Y0`, `M8YA`, `M8G0` — every code the FFmpeg encoder emits |
| Predictors                      | LEFT, GRADIENT, MEDIAN — all three          |
| Raw-mode slice fallback         | Yes (slice flag bit 0)                       |
| Multi-slice frames              | Yes; per-plane Huffman shared across slices  |
| Huffman length descriptor       | Both forms — single-byte and run-length      |
| GBR↔RGB decorrelation           | Yes — applied implicitly for `M8RG` / `M8RA` |
| Decoded output formats          | `Gray8`, `Yuv420P`, `Yuv422P`, `Yuv444P`, `Rgb24`, `Rgba` |
| AVI tag registration            | All seven 8-bit FOURCCs claim `magicyuv` id  |
| Interop (FFmpeg → us)           | YUV422P / GBRP / GRAY8 from `ffmpeg -c:v magicyuv` decoded bit-exactly |

## Not yet supported

- **10/12/14-bit content** (format codes ≥ `0x6c`). Wire syntax differs
  only in the per-symbol byte-pair widening; deferred until we have a
  10-bit fixture from the proprietary encoder.
- **Interlaced frames** (file-header `flags` bit 1).
- **Horizontally-tiled slices** (`slice_width != width`). Matches
  FFmpeg's `AVERROR_PATCHWELCOME`.
- **Encoding.** Decode-only today.
- **Versions other than 7.**

## Layout

* [`bitstream`] — MSB-first bit reader for slice payloads.
* [`huffman`]   — Length-descriptor parser + canonical-Huffman decoder.
* [`predictor`] — LEFT / GRADIENT / MEDIAN spatial predictors.
* [`header`]    — File header + slice-offset table parsing.
* [`decoder`]   — `Packet -> VideoFrame` glue.

## License

MIT.
