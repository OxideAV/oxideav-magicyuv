//! v7 frame header (32 bytes) parser per `spec/01` §3, with the five
//! audit-corrected interpretations from `audit/00-report.md` baked in:
//!
//! 1. `+0x0a aux_byte` is the format-byte's `max_huffman_code_length`
//!    (12/14/16/18 for 8/10/12/14-bit) — we cross-check it against
//!    `tables/00-fourcc-table.csv` rather than hard-coding 12.
//! 2. `+0x0b codec_variant` is **not** the on-wire predictor in v2.4.2
//!    streams — the actual mode is the per-slice `predictor_id` byte
//!    (`spec/04` §1.2). We keep the byte for diagnostics but do not
//!    use it for dispatch.
//! 3. `+0x1c` is `slice_height` (was misnamed `height_extra` in
//!    Specifier round 1; corrected by Specifier round 3 / Auditor
//!    round 1).
//! 4. The encoder allowlist mask `0xf1903f` polarity correction in
//!    `spec/02` §intro is purely encoder-side and does not affect the
//!    decoder; we just note it.
//! 5. The `flags` dword carries `Interlaced` at bit 1 — the predictor
//!    layer consumes that flag (currently informational; `spec/04`
//!    §5.1 round-2 corrected note's interlaced row stride is deferred
//!    beyond round 1, see README "deferred").

use crate::error::{Error, Result};
use crate::tables;

/// MAGY magic 4-byte sequence at offset 0 (`spec/01` §1).
pub const MAGY_MAGIC: [u8; 4] = *b"MAGY";

/// v7 header is exactly 32 bytes (`spec/01` §2 + decoder buffer-len
/// rejection at VMA `0x69badfcc`).
pub const HEADER_SIZE: usize = 32;

/// `flags & FLAG_INTERLACED` set ⇒ field-stride=2 prediction
/// (`spec/04` §5.1 round-2 note). Round 1 informs the decoder of the
/// flag but only the progressive path is implemented. The bit is
/// also written by the encoder per `spec/01` §3.1.
pub const FLAG_INTERLACED: u32 = 0x0000_0002;

/// `flags & FLAG_FULL_RANGE` set ⇒ full-range YUV (`spec/01` §3.1
/// "FullRangeYUV" registry). The decoder pipeline doesn't apply YUV
/// conversion at this layer — the lossless wire bytes are returned
/// unchanged in either case — but downstream colour-conversion needs
/// the flag. Plumbed through into the public `FrameHeader` struct.
pub const FLAG_FULL_RANGE: u32 = 0x0000_0004;

/// Defensive cap on width/height. `32 768` exceeds any v2.4.2
/// fixture by a wide margin; a hostile header with a billion-pixel
/// width would otherwise allocate gigabytes.
pub const MAX_DIMENSION: u32 = 32_768;

/// Parsed v7 frame header / `strf` extradata. Both are byte-identical
/// per `spec/06` §4.1.
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    /// Header byte `+0x09`.
    pub format_byte: u8,
    /// Header byte `+0x0a` (audit-corrected: `max_huffman_code_length`).
    pub aux_byte: u8,
    /// Header byte `+0x0b`. Always `0x02` in v2.4.2 streams; not used
    /// for dispatch. Stored for diagnostics + cross-validation.
    pub codec_variant: u8,
    /// Header bytes `+0x0c..+0x0f` little-endian.
    pub flags: u32,
    /// Header bytes `+0x10..+0x13` little-endian.
    pub width: u32,
    /// Header bytes `+0x14..+0x17` little-endian.
    pub height: u32,
    /// Header bytes `+0x18..+0x1b` little-endian. Usually equal to
    /// `width` per `spec/01` §3.2; behaviour when it differs (Round-1
    /// open question 2) is informational here.
    pub width_extra: u32,
    /// Header bytes `+0x1c..+0x1f` little-endian — the audit-corrected
    /// `slice_height` field (`spec/02` §3 — 28 in v2.4.2; the spec
    /// requires decoders to read this rather than assume 28).
    pub slice_height: u32,
}

impl FrameHeader {
    /// `true` if the header advertises interlaced field-stride
    /// prediction (`spec/04` §5.1 round-2 note). Round 1 surfaces
    /// the bit but the predictor's interlaced row-stride is
    /// deferred — calls touching it return `Error::UnsupportedFormatByte`
    /// equivalent (the decoder rejects rather than silently
    /// mis-reconstructing).
    pub fn is_interlaced(&self) -> bool {
        (self.flags & FLAG_INTERLACED) != 0
    }

    /// `true` if the full-range YUV flag bit is set.
    pub fn is_full_range(&self) -> bool {
        (self.flags & FLAG_FULL_RANGE) != 0
    }
}

/// Parse the 32-byte v7 frame header. The same parser is reused for
/// the `strf` extradata (`spec/06` §4) — it's byte-identical.
pub fn parse(buf: &[u8]) -> Result<FrameHeader> {
    if buf.len() < HEADER_SIZE {
        return Err(Error::Truncated {
            what: "frame header",
            needed: HEADER_SIZE,
            have: buf.len(),
        });
    }
    if buf[0..4] != MAGY_MAGIC {
        return Err(Error::BadMagic);
    }
    let header_size = read_u32_le(&buf[4..8]);
    if header_size != HEADER_SIZE as u32 {
        return Err(Error::BadHeaderSize(header_size));
    }
    let version = buf[8];
    if version > 7 {
        return Err(Error::BadVersion(version));
    }
    let format_byte = buf[9];
    let aux_byte = buf[10];
    let codec_variant = buf[11];
    let flags = read_u32_le(&buf[12..16]);
    let width = read_u32_le(&buf[16..20]);
    let height = read_u32_le(&buf[20..24]);
    let width_extra = read_u32_le(&buf[24..28]);
    let slice_height = read_u32_le(&buf[28..32]);

    // Format byte must be in the round-2 supported set (CSV).
    let rec = tables::lookup_round2(format_byte)?;
    // Audit-corrected aux_byte cross-check (spec/01 §3.0). For 8-bit
    // formats this is 0x0c.
    if aux_byte != rec.aux_byte {
        return Err(Error::AuxByteMismatch {
            got: aux_byte,
            expected: rec.aux_byte,
        });
    }

    if width == 0 {
        return Err(Error::ZeroDimension { what: "width" });
    }
    if height == 0 {
        return Err(Error::ZeroDimension { what: "height" });
    }
    if slice_height == 0 {
        return Err(Error::ZeroDimension {
            what: "slice_height",
        });
    }
    if width > MAX_DIMENSION {
        return Err(Error::DimensionTooLarge {
            what: "width",
            got: width,
        });
    }
    if height > MAX_DIMENSION {
        return Err(Error::DimensionTooLarge {
            what: "height",
            got: height,
        });
    }
    if slice_height > MAX_DIMENSION {
        return Err(Error::DimensionTooLarge {
            what: "slice_height",
            got: slice_height,
        });
    }

    Ok(FrameHeader {
        format_byte,
        aux_byte,
        codec_variant,
        flags,
        width,
        height,
        width_extra,
        slice_height,
    })
}

fn read_u32_le(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[0..4]);
    u32::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated() {
        assert!(matches!(
            parse(&[]),
            Err(Error::Truncated {
                what: "frame header",
                ..
            })
        ));
        assert!(matches!(parse(&[0u8; 16]), Err(Error::Truncated { .. })));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(b"NOPE");
        assert!(matches!(parse(&buf), Err(Error::BadMagic)));
    }

    #[test]
    fn rejects_bad_header_size() {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGY_MAGIC);
        buf[4..8].copy_from_slice(&21u32.to_le_bytes());
        assert!(matches!(parse(&buf), Err(Error::BadHeaderSize(21))));
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGY_MAGIC);
        buf[4..8].copy_from_slice(&32u32.to_le_bytes());
        buf[8] = 8;
        assert!(matches!(parse(&buf), Err(Error::BadVersion(8))));
    }

    #[test]
    fn parses_canonical_header_from_spec02_5_2() {
        // Per spec/02 §5.2 m8rg_64x64_zero.bin first 32 bytes:
        //   4d 41 47 59 20 00 00 00 07 65 0c 02 00 00 20 00
        //   40 00 00 00 40 00 00 00 40 00 00 00 1c 00 00 00
        // Bytes 0x0c..0x0f are `00 00 20 00` ⇒ flags = 0x00200000
        // (the ColorMatrix nibble = 2 in bits 20..23 per spec/01
        // §3.1).
        let buf = [
            0x4d, 0x41, 0x47, 0x59, 0x20, 0x00, 0x00, 0x00, 0x07, 0x65, 0x0c, 0x02, 0x00, 0x00,
            0x20, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00,
            0x1c, 0x00, 0x00, 0x00,
        ];
        let h = parse(&buf).expect("spec/02 §5.2 must parse cleanly");
        assert_eq!(h.format_byte, 0x65);
        assert_eq!(h.aux_byte, 0x0c);
        assert_eq!(h.flags, 0x0020_0000);
        assert_eq!(h.width, 64);
        assert_eq!(h.height, 64);
        assert_eq!(h.width_extra, 64);
        assert_eq!(h.slice_height, 28);
    }

    #[test]
    fn rejects_aux_byte_mismatch() {
        // Build a valid header but corrupt aux_byte to 0x0e — that's
        // legal for 10-bit formats but illegal for format_byte=0x65
        // (M8RG, 8-bit, expected 0x0c).
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGY_MAGIC);
        buf[4..8].copy_from_slice(&32u32.to_le_bytes());
        buf[8] = 7;
        buf[9] = 0x65;
        buf[10] = 0x0e;
        buf[11] = 0x02;
        buf[16..20].copy_from_slice(&64u32.to_le_bytes());
        buf[20..24].copy_from_slice(&64u32.to_le_bytes());
        buf[24..28].copy_from_slice(&64u32.to_le_bytes());
        buf[28..32].copy_from_slice(&28u32.to_le_bytes());
        match parse(&buf) {
            Err(Error::AuxByteMismatch {
                got: 0x0e,
                expected: 0x0c,
            }) => {}
            other => panic!("expected AuxByteMismatch, got {other:?}"),
        }
    }
}
