# oxideav-magicyuv

Pure-Rust **MagicYUV** lossless intra-only video **decoder + encoder**.
Clean-room implementation built from the behavioural trace at
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
| 10-bit format codes             | `M0Y2` (YUV422P10), `M0Y4` (YUV444P10), `M0Y0` (YUV420P10), `M0G0` (GRAY10) |
| Predictors                      | LEFT, GRADIENT, MEDIAN — all three          |
| Raw-mode slice fallback         | Yes (slice flag bit 0)                       |
| Multi-slice frames              | Vertical row bands **and** horizontal-tile columns (arbitrary `nb_slices_x × nb_slices_y` grid) |
| Huffman length descriptor       | Both forms — single-byte and run-length      |
| GBR↔RGB decorrelation           | Yes — applied implicitly for `M8RG` / `M8RA` |
| Decoded output formats          | `Gray8/Gray10Le`, `Yuv420P/Yuv422P/Yuv444P` (8 + 10), `Rgb24`, `Rgba` |
| AVI tag registration            | All eleven recognised FOURCCs claim `magicyuv` id |
| Interop (FFmpeg → us)           | YUV422P / GBRP / GRAY8 / GBRAP / YUVA444P from `ffmpeg -c:v magicyuv` decoded bit-exactly across LEFT/GRADIENT/MEDIAN, single-slice and multi-slice |
| Interop (us → FFmpeg)           | Encoder output for `M8Y2/M8Y0/M8Y4/M8G0/M8RG` round-trips through FFmpeg's reference decoder bit-exactly across LEFT/GRADIENT/MEDIAN, single + multi-slice |

## Not yet supported

- **12/14-bit content** (codes `0x6f`-`0x72`). The wire syntax widens
  to 16-bit-packed samples; the predictor + Huffman kernels are already
  `u16`-clean, but `oxideav-core::PixelFormat` lacks the GBRP12/14 +
  GBRAP12/14 variants needed to round-trip on the output side.
- **GBRP10 / GBRAP10** (codes `0x6d` / `0x6e`). Same blocker —
  `oxideav-core` has no `Gbrp10Le` / `Gbrap10Le` variants today.
- **Interlaced frames** (file-header `flags` bit 1). The decoder rejects
  packets with the interlace bit set; landing this requires the
  per-field predictor doubling described in trace doc §4.1.
- **Versions other than 7.**

## Layout

* [`bitstream`] — MSB-first bit reader + writer for slice payloads.
* [`huffman`]   — Length-descriptor parser, canonical-Huffman decoder,
                  histogram-driven length builder, wire-code synthesiser.
* [`predictor`] — LEFT / GRADIENT / MEDIAN spatial predictors, both
                  decode (`apply_*`) and encode (`encode_*`) directions,
                  `u8` + `u16` paths.
* [`header`]    — File header + slice-offset table parsing.
* [`decoder`]   — `Packet -> VideoFrame` glue.
* [`encoder`]   — `VideoFrame -> Packet` glue.

## License

MIT.
