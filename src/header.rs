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
//!    layer consumes that flag and applies the `spec/04` §5.1
//!    field-stride=2 row stride (top of row `r` is row `r-2`; the
//!    first two rows of each slice have no top neighbour). Both
//!    decode and encode honour it, so interlaced streams round-trip
//!    byte-for-byte across every FOURCC / bit-depth tier.

use crate::error::{Error, Result};
use crate::tables;

/// MAGY magic 4-byte sequence at offset 0 (`spec/01` §1).
pub const MAGY_MAGIC: [u8; 4] = *b"MAGY";

/// v7 header is exactly 32 bytes (`spec/01` §2 + decoder buffer-len
/// rejection at VMA `0x69badfcc`).
pub const HEADER_SIZE: usize = 32;

/// `flags & FLAG_INTERLACED` set ⇒ field-stride=2 prediction
/// (`spec/04` §5.1). The decoder doubles the predictor's
/// top-neighbour row stride (top of row `r` is row `r-2`; the first
/// two rows of each slice have no top neighbour) and the encoder
/// emits the matching field-stride residuals, so interlaced streams
/// round-trip byte-for-byte. The bit is also written by the encoder
/// per `spec/01` §3.1.
pub const FLAG_INTERLACED: u32 = 0x0000_0002;

/// `flags & FLAG_FULL_RANGE` set ⇒ full-range YUV (`spec/01` §3.1
/// "FullRangeYUV" registry). The decoder pipeline doesn't apply YUV
/// conversion at this layer — the lossless wire bytes are returned
/// unchanged in either case — but downstream colour-conversion needs
/// the flag. Plumbed through into the public `FrameHeader` struct.
pub const FLAG_FULL_RANGE: u32 = 0x0000_0004;

/// Bit mask isolating the 4-bit ColorMatrix nibble inside the
/// `flags` dword (`spec/01` §3.1 — encoder OR-accumulates the
/// `ColorMatrix` registry's low nibble at bits 20..23 of the flags
/// word; mask `0x00f00000`). Pair with [`FLAG_COLOR_MATRIX_SHIFT`]
/// to extract the raw nibble (0..=15).
pub const FLAG_COLOR_MATRIX_MASK: u32 = 0x00f0_0000;

/// Right shift to align the masked ColorMatrix nibble down to a
/// 0..=15 integer (`spec/01` §3.1 — the encoder writes the value
/// shifted left by 20 before OR-ing into the flags accumulator).
pub const FLAG_COLOR_MATRIX_SHIFT: u32 = 20;

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
    /// A zero-initialised header, used as a placeholder slot in
    /// [`crate::decoder::DecodedFrame::empty`] before the first decode
    /// populates it.
    pub fn placeholder() -> Self {
        Self {
            format_byte: 0,
            aux_byte: 0,
            codec_variant: 0,
            flags: 0,
            width: 0,
            height: 0,
            width_extra: 0,
            slice_height: 0,
        }
    }
}

impl FrameHeader {
    /// `true` if the header advertises interlaced field-stride
    /// prediction (`spec/04` §5.1 round-2 note). The decoder honours
    /// the bit by doubling the predictor's top-neighbour row stride
    /// (top of row `r` is row `r-2`; the first two rows of each slice
    /// have no top neighbour), and the encoder emits the matching
    /// field-stride=2 residuals when [`crate::EncodeOptions::interlaced`]
    /// is set, so interlaced streams round-trip byte-for-byte across
    /// every FOURCC / bit-depth tier.
    pub fn is_interlaced(&self) -> bool {
        (self.flags & FLAG_INTERLACED) != 0
    }

    /// `true` if the full-range YUV flag bit is set.
    pub fn is_full_range(&self) -> bool {
        (self.flags & FLAG_FULL_RANGE) != 0
    }

    /// Raw 4-bit ColorMatrix nibble carried in the `flags` dword
    /// (`spec/01` §3.1 — bits 20..23, mask `0x00f00000`). The value
    /// is informational at the lossless codec layer: the wire bytes
    /// returned by [`crate::decode_frame`] are unchanged either
    /// way, and downstream colour-conversion (Rec.601 / Rec.709 /
    /// future entries the GUI does not expose) is the consumer of
    /// this signal per the spec's "application/conversion layer"
    /// callout. Returns 0..=15; the vendor encoder writes
    /// `ColorMatrix == 1` as 0 because the encoder's OR-accumulator
    /// at `spec/01` §3.1 skips the matrix contribution in that
    /// case, so a 0 nibble means either "Rec.601" or "encoder
    /// matrix-skip path"; the bit pattern alone cannot disambiguate.
    pub fn color_matrix_nibble(&self) -> u8 {
        ((self.flags & FLAG_COLOR_MATRIX_MASK) >> FLAG_COLOR_MATRIX_SHIFT) as u8
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
        // spec/01 §3.1: the ColorMatrix nibble lives at bits 20..23
        // of `flags`. The §5.2 fixture has `flags = 0x00200000`, so
        // the nibble is 2 (the GUI's "Rec.709" registry value per
        // `reference/vendor/changelog.md` v0.9.2-beta).
        assert_eq!(h.color_matrix_nibble(), 2);
        // Bit 1 (Interlaced) and bit 2 (Full-range YUV) are both
        // clear in this fixture.
        assert!(!h.is_interlaced());
        assert!(!h.is_full_range());
    }

    /// Sweep every 0..=15 value the encoder's `ColorMatrix` register
    /// can fit into `spec/01` §3.1's bits 20..23 nibble and confirm
    /// [`FrameHeader::color_matrix_nibble`] recovers it from the raw
    /// `flags` dword. Uses a directly-constructed `FrameHeader` so
    /// the test isolates the accessor from the parser; the parser
    /// happy path is covered by `parses_canonical_header_from_spec02_5_2`.
    #[test]
    fn color_matrix_nibble_recovers_every_4bit_value() {
        for nibble in 0u32..=0xf {
            let h = FrameHeader {
                format_byte: 0x65,
                aux_byte: 0x0c,
                codec_variant: 0x02,
                flags: nibble << FLAG_COLOR_MATRIX_SHIFT,
                width: 64,
                height: 64,
                width_extra: 64,
                slice_height: 28,
            };
            assert_eq!(
                h.color_matrix_nibble(),
                nibble as u8,
                "nibble {nibble:#x} must round-trip through flags bits 20..23"
            );
            // The other documented bits stay isolated from the nibble.
            assert!(
                !h.is_interlaced(),
                "matrix nibble {nibble:#x} must not bleed into bit 1"
            );
            assert!(
                !h.is_full_range(),
                "matrix nibble {nibble:#x} must not bleed into bit 2"
            );
        }
    }

    /// Cross-bit isolation: setting Interlaced + Full-range
    /// simultaneously with a non-zero ColorMatrix nibble must
    /// leave each accessor returning exactly the field it owns.
    /// Guards against future regressions where someone widens
    /// [`FLAG_COLOR_MATRIX_MASK`] or shifts a constant by mistake.
    #[test]
    fn flag_accessors_are_independent_of_each_other() {
        let h = FrameHeader {
            format_byte: 0x65,
            aux_byte: 0x0c,
            codec_variant: 0x02,
            // Interlaced (bit 1) + Full-range (bit 2) + nibble = 0xa
            // (a value the GUI does not expose, intentionally chosen
            // to be outside the {0, 2} pair the encoder typically
            // writes).
            flags: FLAG_INTERLACED | FLAG_FULL_RANGE | (0xa_u32 << FLAG_COLOR_MATRIX_SHIFT),
            width: 64,
            height: 64,
            width_extra: 64,
            slice_height: 28,
        };
        assert!(h.is_interlaced());
        assert!(h.is_full_range());
        assert_eq!(h.color_matrix_nibble(), 0xa);
    }

    /// The mask and shift constants must satisfy the relation
    /// documented in `spec/01` §3.1: `mask == 0xf << shift`. This
    /// catches a constant drift before the run-time accessor would
    /// silently return wrong values.
    #[test]
    fn color_matrix_constants_match_spec01_3_1() {
        assert_eq!(FLAG_COLOR_MATRIX_MASK, 0xf_u32 << FLAG_COLOR_MATRIX_SHIFT);
        assert_eq!(FLAG_COLOR_MATRIX_SHIFT, 20);
        assert_eq!(FLAG_COLOR_MATRIX_MASK, 0x00f0_0000);
        // No overlap with the two other documented flag bits.
        assert_eq!(FLAG_COLOR_MATRIX_MASK & FLAG_INTERLACED, 0);
        assert_eq!(FLAG_COLOR_MATRIX_MASK & FLAG_FULL_RANGE, 0);
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
