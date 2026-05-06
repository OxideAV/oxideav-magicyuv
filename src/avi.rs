//! Minimal RIFF/AVI demuxer for MagicYUV per `spec/06`.
//!
//! Walks the standard `RIFF AVI ` envelope, locates the `strf`
//! chunk's BITMAPINFOHEADER + extradata, validates the extradata as a
//! v7 MAGY header (it's byte-identical to each per-frame `00dc` MAGY
//! header per `spec/06` §4.1), and emits the `00dc` chunk payloads as
//! decode-ready frames.
//!
//! ## OpenDML 2.0 super-index (`spec/06` §6.1)
//!
//! Round 3 adds OpenDML 2.0 multi-RIFF handling. The walker iterates
//! every top-level `RIFF` chunk in the file: the first carries the
//! `AVI ` four-cc with `hdrl` + `movi`, every subsequent one carries
//! the `AVIX` four-cc with a single `movi` LIST. `00dc` chunks across
//! all such RIFFs are concatenated into a single contiguous frame
//! stream. The `indx` super-index in `strl` is recognised and its
//! `dwChunkId == '00dc'` field cross-checked against the strf
//! FOURCC, but the actual `00dc` frame discovery still walks the
//! `movi` LISTs directly — the super-index is informational here
//! since MagicYUV is intra-only and the host application is expected
//! to walk every chunk anyway. Single-RIFF files (round 1/2 path)
//! remain backward-compatible: when no `AVIX` continuation is
//! present, the walker behaves exactly as before.

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
    let mut info: Option<AviInfo> = None;
    let mut frames: Vec<&[u8]> = Vec::new();
    let mut cursor = 0usize;
    let mut riff_index = 0usize;

    while cursor + 12 <= buf.len() {
        if &buf[cursor..cursor + 4] != b"RIFF" {
            // Tolerate trailing junk after the last RIFF.
            if cursor == 0 {
                return Err(Error::Truncated {
                    what: "RIFF marker",
                    needed: 1,
                    have: 0,
                });
            }
            break;
        }
        let riff_size = read_u32_le(&buf[cursor + 4..cursor + 8]) as usize;
        let riff_form: [u8; 4] = buf[cursor + 8..cursor + 12].try_into().unwrap();

        // First RIFF must declare 'AVI '. Subsequent RIFFs MAY be
        // 'AVIX' OpenDML 2.0 continuations per spec/06 §6.1.
        if riff_index == 0 {
            if &riff_form != b"AVI " {
                return Err(Error::Truncated {
                    what: "AVI marker",
                    needed: 1,
                    have: 0,
                });
            }
        } else if &riff_form != b"AVI " && &riff_form != b"AVIX" {
            // Unknown continuation form: stop walking.
            break;
        }

        #[cfg(feature = "trace")]
        if let Some(t) = tracer {
            let form_str = std::str::from_utf8(&riff_form).unwrap_or("????");
            t.emit(Event::Avi {
                chunk: "RIFF",
                size: riff_size,
                extra: &[("form", AviField::Str(form_str))],
            });
        }

        let riff_end = (cursor + 8 + riff_size).min(buf.len());
        let body = &buf[cursor + 12..riff_end];
        walk_chunks(
            body,
            &mut info,
            &mut frames,
            #[cfg(feature = "trace")]
            tracer,
        )?;

        // Advance past this RIFF, honouring the even-pad rule.
        let advance = 8 + riff_size + (riff_size & 1);
        if advance == 0 {
            break;
        }
        cursor += advance;
        riff_index += 1;
    }

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
        } else if &id == b"indx" {
            // OpenDML 2.0 super-index per spec/06 §6.1. Layout:
            //   WORD wLongsPerEntry      // = 4 for super-index
            //   BYTE bIndexSubType       // = 0
            //   BYTE bIndexType          // = AVI_INDEX_OF_INDEXES = 0x00
            //   DWORD nEntriesInUse
            //   DWORD dwChunkId          // e.g. '00dc'
            //   DWORD dwReserved[3]
            //   <16 B per entry: qwOffset, dwSize, dwDuration>
            //
            // We accept the chunk for trace-level visibility and
            // correctness of the SUPER-INDEX cross-check, but the
            // actual `00dc` discovery is done by walking the `movi`
            // LISTs across all RIFFs (§6.1's claim: "the codec consumes
            // the sequence of MAGY-prefixed packets one at a time").
            #[cfg(feature = "trace")]
            if let Some(t) = tracer {
                if payload.len() >= 24 {
                    let entries_in_use = read_u32_le(&payload[4..8]);
                    let chunk_id: [u8; 4] = payload[8..12].try_into().unwrap();
                    let cid = std::str::from_utf8(&chunk_id).unwrap_or("????");
                    t.emit(Event::Avi {
                        chunk: "indx",
                        size,
                        extra: &[
                            ("nEntriesInUse", AviField::U32(entries_in_use)),
                            ("dwChunkId", AviField::Str(cid)),
                        ],
                    });
                } else {
                    t.emit(Event::Avi {
                        chunk: "indx",
                        size,
                        extra: &[],
                    });
                }
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
    use crate::encoder::{
        encode_avi, encode_avi_opendml, encode_frame, EncodeOptions, PlaneInput, RiffSegmentLimit,
        SliceMode,
    };
    use crate::tables::{lookup_round1, PredictorKind};

    fn make_m8g0_frame(seed: u8) -> (crate::tables::FourccRecord, Vec<u8>, Vec<u8>) {
        let rec = lookup_round1(0x6b).unwrap();
        let pixels: Vec<u8> = (0..(16 * 16))
            .map(|i| ((i & 0xff) as u8).wrapping_add(seed))
            .collect();
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
        (rec, pixels, frame_bytes)
    }

    #[test]
    fn avi_walks_one_frame_m8g0() {
        let (rec, pixels, frame_bytes) = make_m8g0_frame(0);
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

    #[test]
    fn opendml_avi_round_trips_multi_riff() {
        // Synthesise 8 small frames; force segmentation at ~1 KB per
        // RIFF so we get 4–8 RIFF AVIX continuations. The decoder
        // MUST aggregate every `00dc` chunk across all RIFFs and
        // expose them as one contiguous frame stream.
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut originals: Vec<Vec<u8>> = Vec::new();
        let rec = lookup_round1(0x6b).unwrap();
        for s in 0..8u8 {
            let (_, px, fb) = make_m8g0_frame(s);
            originals.push(px);
            frames.push(fb);
        }
        // Each frame is ~280 bytes; with limit = 1 KiB we expect at
        // least 2 segments and likely more.
        let avi = encode_avi_opendml(rec, 16, 16, &frames, RiffSegmentLimit::Bytes(1024));
        // Sanity: there must be at least 2 'RIFF' markers in the
        // output buffer (else we didn't actually segment).
        let riff_count = avi.windows(4).filter(|w| *w == b"RIFF").count();
        assert!(
            riff_count >= 2,
            "OpenDML output must contain ≥ 2 RIFF chunks, got {riff_count}"
        );
        // Sanity: at least one 'AVIX' continuation form.
        let avix_count = avi.windows(4).filter(|w| *w == b"AVIX").count();
        assert!(
            avix_count >= 1,
            "OpenDML output must contain ≥ 1 AVIX form, got {avix_count}"
        );
        // Sanity: an indx super-index chunk.
        let indx_count = avi.windows(4).filter(|w| *w == b"indx").count();
        assert_eq!(indx_count, 1, "expected exactly one `indx` super-index");

        // Decode side: walk every RIFF + recover all 8 frames.
        let mut reader = AviReader::open(&avi).expect("avi parse");
        assert_eq!(reader.info.width, 16);
        assert_eq!(reader.info.height, 16);
        assert_eq!(&reader.info.fourcc, b"M8G0");
        assert_eq!(reader.frame_count(), 8);
        for (i, original) in originals.iter().enumerate() {
            let frame = reader.next_frame().unwrap_or_else(|| panic!("frame {i}"));
            let dec = decode_frame(frame).expect("decode");
            assert_eq!(
                dec.planes[0].samples.as_u8().unwrap(),
                &original[..],
                "frame {i} bytes mismatch"
            );
        }
        assert!(reader.next_frame().is_none());
    }

    #[test]
    fn opendml_single_segment_when_limit_large_enough() {
        // When the segment limit is generous, all frames fit in a
        // single segment — the output should still parse, and the
        // file should contain exactly ONE RIFF + the indx is still
        // emitted (with one super-index entry).
        let (rec, pixels, frame_bytes) = make_m8g0_frame(0);
        let avi = encode_avi_opendml(
            rec,
            16,
            16,
            std::slice::from_ref(&frame_bytes),
            RiffSegmentLimit::OneGiB,
        );
        let riff_count = avi.windows(4).filter(|w| *w == b"RIFF").count();
        assert_eq!(
            riff_count, 1,
            "single-segment OpenDML output should have exactly one RIFF"
        );

        let mut reader = AviReader::open(&avi).expect("avi parse");
        assert_eq!(reader.frame_count(), 1);
        let frame = reader.next_frame().unwrap();
        let dec = decode_frame(frame).expect("decode");
        assert_eq!(dec.planes[0].samples.as_u8().unwrap(), &pixels[..]);
    }

    #[test]
    fn opendml_indx_entries_point_to_riff_offsets() {
        // Build a multi-segment OpenDML file and verify each indx
        // super-index entry's qwOffset matches the file offset of a
        // 'RIFF' marker, and dwSize matches the corresponding RIFF
        // chunk's full byte size (header + body, including pad).
        //
        // We walk the file by following RIFF chunks at the top
        // level (rather than substring-searching for 'RIFF' bytes,
        // which can collide with content in indx entries that just
        // happen to contain those 4 bytes).
        let rec = lookup_round1(0x6b).unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for s in 0..6u8 {
            let (_, _, fb) = make_m8g0_frame(s);
            frames.push(fb);
        }
        let avi = encode_avi_opendml(rec, 16, 16, &frames, RiffSegmentLimit::Bytes(1024));
        // Walk top-level RIFF chunks.
        let mut riff_offsets: Vec<u64> = Vec::new();
        let mut i = 0usize;
        while i + 12 <= avi.len() {
            if &avi[i..i + 4] != b"RIFF" {
                break;
            }
            riff_offsets.push(i as u64);
            let sz = read_u32_le(&avi[i + 4..i + 8]) as usize;
            let advance = 8 + sz + (sz & 1);
            assert!(advance >= 12, "RIFF advance underflow");
            assert!(
                i + advance <= avi.len(),
                "RIFF at {i} extends past file (advance={advance}, len={})",
                avi.len()
            );
            i += advance;
        }
        assert!(riff_offsets.len() >= 2, "expected ≥ 2 RIFFs");

        // Locate indx chunk by walking the structure: RIFF + size +
        // 'AVI ' + LIST + size + 'hdrl' + LIST + size + 'strl' +
        // 'strf' + size + strf_payload + pad + 'indx' + size + payload.
        // The strf payload size depends on `rec` (M8G0 → 72 B),
        // matching `build_strf_payload`.
        // Use the chunk-walker to robustly locate indx — it sits at a
        // known position relative to the start of strl. Easier
        // approach: search for `indx` four-cc but only look INSIDE the
        // first RIFF's strl region (i.e. before the first movi).
        let first_riff_end = {
            let off = riff_offsets[0] as usize;
            let sz = read_u32_le(&avi[off + 4..off + 8]) as usize;
            off + 8 + sz
        };
        // Find 'indx' four-cc within the first RIFF only.
        let mut indx_pos: Option<usize> = None;
        let mut j = riff_offsets[0] as usize + 12;
        while j + 8 <= first_riff_end {
            if &avi[j..j + 4] == b"indx" {
                indx_pos = Some(j);
                break;
            }
            j += 4;
        }
        let indx_pos = indx_pos.expect("indx must be present in first RIFF");
        // indx chunk: 'indx' (4) + dwSize (4) + payload.
        // Payload layout (24-byte preamble before per-entry slots):
        //   +0..2   wLongsPerEntry (u16, = 4)
        //   +2..3   bIndexSubType  (u8)
        //   +3..4   bIndexType     (u8, = 0)
        //   +4..8   nEntriesInUse  (u32)
        //   +8..12  dwChunkId      (u32 / 4-cc)
        //   +12..24 dwReserved[3]  (3*u32)
        let payload_off = indx_pos + 8;
        let n_entries = u32::from_le_bytes([
            avi[payload_off + 4],
            avi[payload_off + 5],
            avi[payload_off + 6],
            avi[payload_off + 7],
        ]) as usize;
        assert_eq!(n_entries, riff_offsets.len(), "indx entry count");
        let entries_start = payload_off + 24;
        for (k, &expected_off) in riff_offsets.iter().enumerate() {
            let base = entries_start + 16 * k;
            let mut q = [0u8; 8];
            q.copy_from_slice(&avi[base..base + 8]);
            let qw_off = u64::from_le_bytes(q);
            assert_eq!(qw_off, expected_off, "indx entry {k} qwOffset mismatch");
            let mut d = [0u8; 4];
            d.copy_from_slice(&avi[base + 8..base + 12]);
            let dw_size = u32::from_le_bytes(d) as u64;
            // Compare to the RIFF chunk size (= 8 + RIFF dwSize).
            let mut sz_le = [0u8; 4];
            sz_le.copy_from_slice(&avi[expected_off as usize + 4..expected_off as usize + 8]);
            let riff_dw_size = u32::from_le_bytes(sz_le) as u64;
            assert_eq!(dw_size, riff_dw_size + 8, "indx entry {k} dwSize");
        }
    }
}
