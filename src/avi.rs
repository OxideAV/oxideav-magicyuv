//! Minimal RIFF/AVI demuxer for MagicYUV per `spec/06`.
//!
//! Walks the standard `RIFF AVI` envelope, locates the
//! `strf` chunk's BITMAPINFOHEADER + extradata, validates the
//! extradata as a v7 MAGY header (it's byte-identical to each
//! per-frame `00dc` MAGY header per `spec/06` §4.1), and emits the
//! `00dc` chunk payloads as decode-ready frames.
//!
//! OpenDML 2.0 (`indx` super-index, `RIFF AVIX` continuations) is
//! out of scope for round 1/2 per `spec/06` §6.1 — single-RIFF AVI
//! files (≤ 1 GB) are handled.

use crate::error::{Error, Result};
use crate::header;

#[cfg(feature = "trace")]
use crate::trace::{AviField, Event, Tracer};

/// Parsed header info from a MagicYUV-bearing AVI file.
pub struct AviInfo {
    /// Width from the BITMAPINFOHEADER.
    pub width: u32,
    /// Height from the BITMAPINFOHEADER (the codec writes positive
    /// values per `spec/06` §3.4).
    pub height: u32,
    /// FOURCC from `biCompression` (e.g. `b"M8RG"`).
    pub fourcc: [u8; 4],
}

/// Minimal AVI walker for MagicYUV.
pub struct AviReader<'a> {
    /// Slice of `00dc` chunk payloads, in file order.
    frames: Vec<&'a [u8]>,
    /// Header info from the strf extradata.
    pub info: AviInfo,
    pos: usize,
}

impl<'a> AviReader<'a> {
    /// Parse an AVI file's bytes into a frame iterator.
    pub fn open(buf: &'a [u8]) -> Result<Self> {
        #[cfg(feature = "trace")]
        let tracer = Tracer::from_env();
        #[cfg(not(feature = "trace"))]
        let tracer: Option<()> = None;
        let (info, frames) = parse_riff(
            buf,
            #[cfg(feature = "trace")]
            tracer.as_ref(),
        )?;
        let _ = tracer;
        Ok(Self {
            frames,
            info,
            pos: 0,
        })
    }

    /// Number of `00dc` frames in the file.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Pull the next frame's payload. Returns `None` when exhausted.
    pub fn next_frame(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.frames.len() {
            return None;
        }
        let f = self.frames[self.pos];
        self.pos += 1;
        Some(f)
    }
}

#[allow(clippy::needless_lifetimes)]
fn parse_riff<'a>(
    buf: &'a [u8],
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
) -> Result<(AviInfo, Vec<&'a [u8]>)> {
    if buf.len() < 12 {
        return Err(Error::Truncated {
            what: "RIFF AVI envelope",
            needed: 12,
            have: buf.len(),
        });
    }
    if &buf[0..4] != b"RIFF" {
        return Err(Error::Truncated {
            what: "RIFF marker",
            needed: 1,
            have: 0,
        });
    }
    if &buf[8..12] != b"AVI " {
        return Err(Error::Truncated {
            what: "AVI marker",
            needed: 1,
            have: 0,
        });
    }
    let riff_size = read_u32_le(&buf[4..8]) as usize;
    #[cfg(feature = "trace")]
    if let Some(t) = tracer {
        t.emit(Event::Avi {
            chunk: "RIFF",
            size: riff_size,
            extra: &[],
        });
    }
    let riff_end = (8 + riff_size).min(buf.len());
    let body = &buf[12..riff_end];

    let mut info: Option<AviInfo> = None;
    let mut frames: Vec<&[u8]> = Vec::new();
    walk_chunks(
        body,
        &mut info,
        &mut frames,
        #[cfg(feature = "trace")]
        tracer,
    )?;

    let info = info.ok_or(Error::Truncated {
        what: "strf chunk (no MAGY extradata)",
        needed: 1,
        have: 0,
    })?;
    Ok((info, frames))
}

fn walk_chunks<'a>(
    mut buf: &'a [u8],
    info: &mut Option<AviInfo>,
    frames: &mut Vec<&'a [u8]>,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
) -> Result<()> {
    while buf.len() >= 8 {
        let id: [u8; 4] = buf[0..4].try_into().unwrap();
        let size = read_u32_le(&buf[4..8]) as usize;
        let payload_start = 8;
        let payload_end = payload_start + size;
        if buf.len() < payload_end {
            return Err(Error::Truncated {
                what: "RIFF chunk payload",
                needed: payload_end,
                have: buf.len(),
            });
        }
        let payload = &buf[payload_start..payload_end];

        if &id == b"LIST" {
            if payload.len() < 4 {
                return Err(Error::Truncated {
                    what: "LIST type",
                    needed: 4,
                    have: payload.len(),
                });
            }
            let list_type: [u8; 4] = payload[0..4].try_into().unwrap();
            let list_body = &payload[4..];
            #[cfg(feature = "trace")]
            if let Some(t) = tracer {
                let lt = std::str::from_utf8(&list_type).unwrap_or("????");
                t.emit(Event::Avi {
                    chunk: "LIST",
                    size,
                    extra: &[("list_type", AviField::Str(lt))],
                });
            }
            if &list_type == b"hdrl" || &list_type == b"strl" {
                walk_chunks(
                    list_body,
                    info,
                    frames,
                    #[cfg(feature = "trace")]
                    tracer,
                )?;
            } else if &list_type == b"movi" {
                walk_movi(
                    list_body,
                    frames,
                    #[cfg(feature = "trace")]
                    tracer,
                )?;
            }
        } else if &id == b"strf" && payload.len() >= 40 + header::HEADER_SIZE {
            let bih = &payload[..40];
            let extra = &payload[40..40 + header::HEADER_SIZE];
            let _ = header::parse(extra)?;
            let bi_width = read_u32_le(&bih[4..8]);
            let bi_height = read_u32_le(&bih[8..12]).abs_signed();
            let fourcc: [u8; 4] = bih[16..20].try_into().unwrap();
            #[cfg(feature = "trace")]
            if let Some(t) = tracer {
                let fc = std::str::from_utf8(&fourcc).unwrap_or("????");
                t.emit(Event::Avi {
                    chunk: "strf",
                    size,
                    extra: &[
                        ("width", AviField::U32(bi_width)),
                        ("height", AviField::U32(bi_height)),
                        ("fourcc", AviField::Str(fc)),
                    ],
                });
            }
            *info = Some(AviInfo {
                width: bi_width,
                height: bi_height,
                fourcc,
            });
        }

        let advance = payload_end + (size & 1);
        if advance > buf.len() {
            break;
        }
        buf = &buf[advance..];
    }
    Ok(())
}

fn walk_movi<'a>(
    mut buf: &'a [u8],
    frames: &mut Vec<&'a [u8]>,
    #[cfg(feature = "trace")] tracer: Option<&Tracer>,
) -> Result<()> {
    #[cfg(feature = "trace")]
    let mut frame_index = 0usize;
    while buf.len() >= 8 {
        let id: [u8; 4] = buf[0..4].try_into().unwrap();
        let size = read_u32_le(&buf[4..8]) as usize;
        if buf.len() < 8 + size {
            return Err(Error::Truncated {
                what: "movi chunk",
                needed: 8 + size,
                have: buf.len(),
            });
        }
        let payload = &buf[8..8 + size];
        if id[2] == b'd' && id[3] == b'c' {
            #[cfg(feature = "trace")]
            if let Some(t) = tracer {
                t.emit(Event::Avi {
                    chunk: "00dc",
                    size,
                    extra: &[
                        ("frame_index", AviField::USize(frame_index)),
                        ("payload_size", AviField::USize(payload.len())),
                    ],
                });
            }
            frames.push(payload);
            #[cfg(feature = "trace")]
            {
                frame_index += 1;
            }
        }
        let advance = 8 + size + (size & 1);
        if advance > buf.len() {
            break;
        }
        buf = &buf[advance..];
    }
    Ok(())
}

fn read_u32_le(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[0..4]);
    u32::from_le_bytes(a)
}

trait AbsSigned {
    fn abs_signed(self) -> u32;
}
impl AbsSigned for u32 {
    fn abs_signed(self) -> u32 {
        let s = self as i32;
        s.unsigned_abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::decode_frame;
    use crate::encoder::{encode_avi, encode_frame, EncodeOptions, PlaneInput, SliceMode};
    use crate::tables::{lookup_round1, PredictorKind};

    #[test]
    fn avi_walks_one_frame_m8g0() {
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
        let frame_bytes = encode_frame(
            rec,
            16,
            16,
            28,
            vec![PlaneInput::U8(pixels.clone())],
            EncodeOptions {
                predictor: PredictorKind::Gradient,
                mode: SliceMode::Huffman,
                interlaced: false,
            },
        )
        .unwrap();
        let avi = encode_avi(rec, 16, 16, std::slice::from_ref(&frame_bytes));
        let mut reader = AviReader::open(&avi).expect("avi parse");
        assert_eq!(reader.info.width, 16);
        assert_eq!(reader.info.height, 16);
        assert_eq!(&reader.info.fourcc, b"M8G0");
        assert_eq!(reader.frame_count(), 1);
        let frame = reader.next_frame().expect("frame 0");
        let dec = decode_frame(frame).expect("decode frame 0");
        assert_eq!(dec.planes[0].samples.as_u8().unwrap(), &pixels[..]);
        assert!(reader.next_frame().is_none());
    }
}
