//! Minimal RIFF/AVI demuxer for MagicYUV per `spec/06`.
//!
//! Round 1 scope: walk the standard `RIFF AVI` envelope, locate the
//! `strf` chunk's BITMAPINFOHEADER + extradata, validate the
//! extradata as a v7 MAGY header (it's byte-identical to each
//! per-frame `00dc` MAGY header per `spec/06` §4.1), and emit the
//! `00dc` chunk payloads as decode-ready frames.
//!
//! The parser is iterator-style: build it from a byte slice, then
//! call `.next_frame()` to get each frame's bytes (the unpadded
//! `00dc` chunk payload). The payload is exactly what
//! [`crate::decode_frame`] consumes.
//!
//! OpenDML 2.0 (`indx` super-index, `RIFF AVIX` continuations) is
//! out of scope for round 1 per `spec/06` §6.1 — single-RIFF AVI
//! files (≤ 1 GB) are handled.

use crate::error::{Error, Result};
use crate::header;

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
        let (info, frames) = parse_riff(buf)?;
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

fn parse_riff(buf: &[u8]) -> Result<(AviInfo, Vec<&[u8]>)> {
    // RIFF(<size>) "AVI " ...
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
    let riff_end = (8 + riff_size).min(buf.len());
    let body = &buf[12..riff_end];

    let mut info: Option<AviInfo> = None;
    let mut frames: Vec<&[u8]> = Vec::new();
    walk_chunks(body, &mut info, &mut frames)?;

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
            if &list_type == b"hdrl" || &list_type == b"strl" {
                walk_chunks(list_body, info, frames)?;
            } else if &list_type == b"movi" {
                walk_movi(list_body, frames)?;
            }
            // Other LIST types (e.g. 'INFO') are ignored.
        } else if &id == b"strf" {
            // Stream format chunk: BITMAPINFOHEADER (40 B) + extradata
            // (32 B for v7 MAGY).
            if payload.len() >= 40 + header::HEADER_SIZE {
                let bih = &payload[..40];
                let extra = &payload[40..40 + header::HEADER_SIZE];
                // Validate the MAGY extradata.
                let _ = header::parse(extra)?;
                let bi_width = read_u32_le(&bih[4..8]);
                let bi_height = read_u32_le(&bih[8..12]).abs_signed();
                let fourcc: [u8; 4] = bih[16..20].try_into().unwrap();
                *info = Some(AviInfo {
                    width: bi_width,
                    height: bi_height,
                    fourcc,
                });
            }
        }

        // Word-align to even byte boundary.
        let advance = payload_end + (size & 1);
        if advance > buf.len() {
            break;
        }
        buf = &buf[advance..];
    }
    Ok(())
}

fn walk_movi<'a>(mut buf: &'a [u8], frames: &mut Vec<&'a [u8]>) -> Result<()> {
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
        // Per spec/06 §2.1, video frame chunks have FOURCC
        // <sn>dc — for stream 0 that's "00dc". We accept any
        // <NN>dc.
        if id[2] == b'd' && id[3] == b'c' {
            frames.push(payload);
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
        // Treat self as i32 and return its absolute value (cap at
        // i32::MAX → u32). biHeight is documented signed; positive
        // for compressed video per spec/06 §3.4 but we tolerate
        // negative.
        let s = self as i32;
        s.unsigned_abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::decode_frame;
    use crate::encoder::{encode_frame, SliceMode};
    use crate::tables::{lookup_round1, PredictorKind};

    /// Synthesise a minimal RIFF AVI file containing one MAGY frame.
    /// Used to exercise [`AviReader::open`] end-to-end.
    fn synth_avi(frame_bytes: &[u8], fourcc: [u8; 4], width: u32, height: u32) -> Vec<u8> {
        // Build strf payload: 40 B BITMAPINFOHEADER + 32 B extradata
        // (the extradata is the first 32 bytes of frame_bytes per
        // spec/06 §4.1).
        let mut bih = Vec::new();
        bih.extend_from_slice(&72u32.to_le_bytes()); // biSize = 72
        bih.extend_from_slice(&width.to_le_bytes());
        bih.extend_from_slice(&height.to_le_bytes());
        bih.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        bih.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
        bih.extend_from_slice(&fourcc); // biCompression
        bih.extend_from_slice(&(frame_bytes.len() as u32).to_le_bytes()); // biSizeImage
        bih.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
        bih.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
        bih.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        bih.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        let mut strf_payload = bih;
        strf_payload.extend_from_slice(&frame_bytes[..32]);

        // Build the "strl" LIST: type tag + strf chunk.
        let mut strl_payload = Vec::new();
        strl_payload.extend_from_slice(b"strl");
        strl_payload.extend_from_slice(b"strf");
        strl_payload.extend_from_slice(&(strf_payload.len() as u32).to_le_bytes());
        strl_payload.extend_from_slice(&strf_payload);
        if strf_payload.len() % 2 == 1 {
            strl_payload.push(0);
        }

        // Build the "hdrl" LIST: type tag + strl LIST chunk.
        let mut hdrl_payload = Vec::new();
        hdrl_payload.extend_from_slice(b"hdrl");
        hdrl_payload.extend_from_slice(b"LIST");
        hdrl_payload.extend_from_slice(&(strl_payload.len() as u32).to_le_bytes());
        hdrl_payload.extend_from_slice(&strl_payload);

        // Build the "movi" LIST: type tag + 00dc chunk.
        let mut movi_payload = Vec::new();
        movi_payload.extend_from_slice(b"movi");
        movi_payload.extend_from_slice(b"00dc");
        movi_payload.extend_from_slice(&(frame_bytes.len() as u32).to_le_bytes());
        movi_payload.extend_from_slice(frame_bytes);
        if frame_bytes.len() % 2 == 1 {
            movi_payload.push(0);
        }

        // RIFF body: "AVI " + LIST(hdrl) + LIST(movi).
        let mut riff_body = Vec::new();
        riff_body.extend_from_slice(b"AVI ");
        riff_body.extend_from_slice(b"LIST");
        riff_body.extend_from_slice(&(hdrl_payload.len() as u32).to_le_bytes());
        riff_body.extend_from_slice(&hdrl_payload);
        riff_body.extend_from_slice(b"LIST");
        riff_body.extend_from_slice(&(movi_payload.len() as u32).to_le_bytes());
        riff_body.extend_from_slice(&movi_payload);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
        out.extend_from_slice(&riff_body);
        out
    }

    #[test]
    fn avi_walks_one_frame_m8g0() {
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16)).map(|i| (i & 0xff) as u8).collect();
        let frame_bytes = encode_frame(
            rec,
            16,
            16,
            28,
            vec![pixels.clone()],
            PredictorKind::Gradient,
            SliceMode::Huffman,
        );
        let avi = synth_avi(&frame_bytes, *b"M8G0", 16, 16);
        let mut reader = AviReader::open(&avi).expect("avi parse");
        assert_eq!(reader.info.width, 16);
        assert_eq!(reader.info.height, 16);
        assert_eq!(&reader.info.fourcc, b"M8G0");
        assert_eq!(reader.frame_count(), 1);
        let frame = reader.next_frame().expect("frame 0");
        let dec = decode_frame(frame).expect("decode frame 0");
        assert_eq!(dec.planes[0].data, pixels);
        assert!(reader.next_frame().is_none());
    }
}
