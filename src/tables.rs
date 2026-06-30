//! Parse `docs/video/magicyuv/tables/00-fourcc-table.csv` and
//! `01-predictor-table.csv` at startup. The dispatch policy in the
//! Implementer round prompt forbids retyping any of those values; we
//! only embed the CSV bytes via `include_str!` and do a one-time
//! linear parse into a `static` lookup.
//!
//! The CSVs are part of the `oxideav-workspace` repository
//! (`docs/video/magicyuv/tables/`). They're kept out of the crate's
//! published source tree because the workspace would have to ship
//! them as `package.include = [...]`; instead we copy them into the
//! crate at this module's compile time via the absolute build-host
//! path `include_str!` resolves relative to. This crate ships a
//! verbatim `tables/` directory next to `src/` so the `include_str!`
//! is path-portable on consumers' machines too.

use crate::error::{Error, Result};

const FOURCC_CSV: &str = include_str!("../tables/00-fourcc-table.csv");
const PREDICTOR_CSV: &str = include_str!("../tables/01-predictor-table.csv");

/// Pixel-family classification, derived from `tables/00-fourcc-table.csv`
/// column "family". All five families (`Rgb`, `Rgba`, `Yuv`, `Yuva`,
/// `Gray`) are decoded and encoded across every bit-depth tier the
/// CSV enumerates (8 / 10 / 12 / 14-bit); the storage container width
/// is selected from [`FourccRecord::is_high_bit_depth`], not the
/// family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Three planes: G, B, R (per spec/03 §4 audit-corrected wire
    /// order is `(B', G, R')` after `(B-G, R-G)` decorrelation).
    Rgb,
    /// Four planes: G, B, R, A.
    Rgba,
    /// Three planes: Y, U, V (subsampling per `subsampling` field).
    Yuv,
    /// Four planes: Y, U, V, A. Always 4:4:4:4 (`spec/03` §5).
    Yuva,
    /// Single luma plane.
    Gray,
}

/// Per-FOURCC record from `tables/00-fourcc-table.csv`. Round-1 dispatch
/// uses `format_byte`, `bit_depth`, `planes`, `aux_byte`, `family`,
/// and `(sub_x, sub_y)` — derived from the CSV's `subsampling` column.
#[derive(Debug, Clone, Copy)]
pub struct FourccRecord {
    pub format_byte: u8,
    pub fourcc: [u8; 4],
    pub bit_depth: u8,
    pub planes: u8,
    pub alpha: bool,
    pub aux_byte: u8,
    pub family: Family,
    pub sub_x: u8,
    pub sub_y: u8,
}

impl FourccRecord {
    /// `true` iff samples sit in a 1-byte container (8-bit family).
    /// 10/12/14-bit live in 2-byte containers per spec/03 §7.
    pub fn is_8bit(&self) -> bool {
        self.bit_depth == 8
    }

    /// A zero-initialised record, used as a placeholder slot in
    /// [`crate::decoder::DecodedFrame::empty`] before the first decode
    /// populates it. The first [`crate::decoder::decode_into`] call
    /// overwrites it with the parsed value.
    pub fn placeholder() -> Self {
        Self {
            format_byte: 0,
            fourcc: *b"????",
            bit_depth: 0,
            planes: 0,
            alpha: false,
            aux_byte: 0,
            family: Family::Gray,
            sub_x: 1,
            sub_y: 1,
        }
    }
}

/// Look up a record by `format_byte`. Returns `None` if no
/// corresponding row exists in the CSV.
pub fn lookup(format_byte: u8) -> Option<FourccRecord> {
    fourcc_table()
        .iter()
        .find(|r| r.format_byte == format_byte)
        .copied()
}

/// Look up a record and ensure it is part of the round-1
/// 8-bit-native supported set (`{0x65, 0x66, 0x67, 0x68, 0x69, 0x6a,
/// 0x6b}`). Higher bit-depths return `Error::UnsupportedFormatByte`.
pub fn lookup_round1(format_byte: u8) -> Result<FourccRecord> {
    let rec = lookup(format_byte).ok_or(Error::UnsupportedFormatByte(format_byte))?;
    if !rec.is_8bit() {
        return Err(Error::UnsupportedFormatByte(format_byte));
    }
    Ok(rec)
}

/// Look up a record across the round-2 supported set (8-bit + 10/12/14
/// bit native FOURCCs from `tables/00-fourcc-table.csv`). Returns
/// `Error::UnsupportedFormatByte` for bytes not present in the CSV.
pub fn lookup_round2(format_byte: u8) -> Result<FourccRecord> {
    lookup(format_byte).ok_or(Error::UnsupportedFormatByte(format_byte))
}

impl FourccRecord {
    /// `max_huffman_code_length` for this FOURCC's bit-depth tier
    /// (`spec/05` §1.1: 12/14/16/18 for 8/10/12/14-bit).
    pub fn max_huffman_length(&self) -> u8 {
        match self.bit_depth {
            8 => 12,
            10 => 14,
            12 => 16,
            14 => 18,
            _ => 12,
        }
    }

    /// Per-bit-depth wrap mask. `(1 << bit_depth) - 1`.
    pub fn sample_mask(&self) -> u32 {
        (1u32 << self.bit_depth) - 1
    }

    /// `true` if this FOURCC stores samples in a 2-byte container
    /// (10/12/14-bit). The 8-bit family is 1-byte container.
    pub fn is_high_bit_depth(&self) -> bool {
        self.bit_depth > 8
    }

    /// Subsampling label string (`"4:4:4"`, `"4:2:2"`, `"4:2:0"`,
    /// `"4:0:0"`, `"n/a"`) for trace events.
    pub fn subsampling_label(&self) -> &'static str {
        match (self.family, self.sub_x, self.sub_y) {
            (Family::Gray, _, _) => "4:0:0",
            (Family::Yuv, 1, 1) => "4:4:4",
            (Family::Yuv, 2, 1) => "4:2:2",
            (Family::Yuv, 2, 2) => "4:2:0",
            (Family::Yuva, _, _) => "4:4:4:4",
            _ => "n/a",
        }
    }

    /// Family label string for trace events.
    pub fn family_label(&self) -> &'static str {
        match self.family {
            Family::Rgb => "RGB",
            Family::Rgba => "RGBA",
            Family::Yuv => "YUV",
            Family::Yuva => "YUVA",
            Family::Gray => "Gray",
        }
    }
}

/// Borrow the parsed-once FOURCC table.
pub fn fourcc_table() -> &'static [FourccRecord] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<FourccRecord>> = OnceLock::new();
    TABLE.get_or_init(parse_fourcc_csv)
}

fn parse_fourcc_csv() -> Vec<FourccRecord> {
    let mut out = Vec::new();
    for raw in FOURCC_CSV.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("format_byte") {
            continue;
        }
        // Columns:
        // format_byte,fourcc,bit_depth,planes,bpp,alpha,aux_byte,family,subsampling,description
        let mut cols = line.split(',');
        let format_byte = parse_hex_or_dec(cols.next().expect("fourcc csv col 0"));
        let fourcc_str = cols.next().expect("fourcc csv col 1").trim();
        let bit_depth: u8 = cols
            .next()
            .expect("col 2")
            .trim()
            .parse()
            .expect("bit_depth");
        let planes: u8 = cols.next().expect("col 3").trim().parse().expect("planes");
        let _bpp: u32 = cols.next().expect("col 4").trim().parse().expect("bpp");
        let alpha_v: u8 = cols.next().expect("col 5").trim().parse().expect("alpha");
        let aux_byte = parse_hex_or_dec(cols.next().expect("col 6")) as u8;
        let family_str = cols.next().expect("col 7").trim();
        let subsampling = cols.next().expect("col 8").trim();

        let family = match family_str {
            "RGB" => Family::Rgb,
            "RGBA" => Family::Rgba,
            "YUV" => Family::Yuv,
            "YUVA" => Family::Yuva,
            "Gray" => Family::Gray,
            other => panic!("oxideav-magicyuv: unknown family in fourcc csv: {other}"),
        };

        let (sub_x, sub_y) = subsampling_xy(subsampling, family);

        let mut fourcc = [0u8; 4];
        let bytes = fourcc_str.as_bytes();
        assert!(bytes.len() == 4, "fourcc must be 4 chars: {fourcc_str:?}");
        fourcc.copy_from_slice(bytes);

        out.push(FourccRecord {
            format_byte: format_byte as u8,
            fourcc,
            bit_depth,
            planes,
            alpha: alpha_v != 0,
            aux_byte,
            family,
            sub_x,
            sub_y,
        });
    }
    out
}

fn parse_hex_or_dec(s: &str) -> u32 {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(rest, 16).expect("hex parse")
    } else {
        s.parse().expect("dec parse")
    }
}

fn subsampling_xy(s: &str, family: Family) -> (u8, u8) {
    match (family, s) {
        (Family::Rgb | Family::Rgba | Family::Gray, _) => (1, 1),
        (Family::Yuva, _) => (1, 1),
        (Family::Yuv, "4:4:4") => (1, 1),
        (Family::Yuv, "4:2:2") => (2, 1),
        (Family::Yuv, "4:2:0") => (2, 2),
        (Family::Yuv, other) => panic!("oxideav-magicyuv: unknown YUV subsampling {other:?}"),
    }
}

// ───────────────────── predictors ─────────────────────

/// Per-row record from `tables/01-predictor-table.csv`. The
/// implementation only consumes `predictor_id` (we already know the
/// formulas from `spec/04` §4.2..§4.4); the table is parsed to
/// guarantee the IDs we accept exactly match the documented dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredictorRecord {
    pub predictor_id: u8,
    pub kind: PredictorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictorKind {
    Left,
    Gradient,
    Median,
}

/// Borrow the parsed-once predictor table.
pub fn predictor_table() -> &'static [PredictorRecord] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<Vec<PredictorRecord>> = OnceLock::new();
    TABLE.get_or_init(parse_predictor_csv)
}

fn parse_predictor_csv() -> Vec<PredictorRecord> {
    let mut out = Vec::new();
    for raw in PREDICTOR_CSV.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("predictor_id") {
            continue;
        }
        let mut cols = line.split(',');
        let predictor_id = parse_hex_or_dec(cols.next().expect("col 0")) as u8;
        let name = cols.next().expect("col 1").trim();
        let kind = match name {
            "Left" => PredictorKind::Left,
            "Gradient" => PredictorKind::Gradient,
            "Median" => PredictorKind::Median,
            other => panic!("oxideav-magicyuv: unknown predictor name {other:?}"),
        };
        out.push(PredictorRecord { predictor_id, kind });
    }
    out
}

/// Resolve a predictor ID byte to its kind, rejecting values not in
/// the CSV.
pub fn lookup_predictor(predictor_id: u8) -> Result<PredictorKind> {
    predictor_table()
        .iter()
        .find(|r| r.predictor_id == predictor_id)
        .map(|r| r.kind)
        .ok_or(Error::BadPredictorId(predictor_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_table_includes_round1_set() {
        let need: &[(u8, &[u8; 4])] = &[
            (0x65, b"M8RG"),
            (0x66, b"M8RA"),
            (0x67, b"M8Y4"),
            (0x68, b"M8Y2"),
            (0x69, b"M8Y0"),
            (0x6a, b"M8YA"),
            (0x6b, b"M8G0"),
        ];
        for (fb, fc) in need {
            let r = lookup(*fb).expect("round-1 fourcc must be in table");
            assert_eq!(&r.fourcc, *fc, "fourcc mismatch for {fb:#04x}");
            assert!(r.is_8bit(), "round-1 fourcc {fb:#04x} should be 8-bit");
        }
    }

    #[test]
    fn predictor_table_three_kinds() {
        let t = predictor_table();
        assert_eq!(t.len(), 3);
        assert_eq!(lookup_predictor(0x01).unwrap(), PredictorKind::Left);
        assert_eq!(lookup_predictor(0x02).unwrap(), PredictorKind::Gradient);
        assert_eq!(lookup_predictor(0x03).unwrap(), PredictorKind::Median);
        assert!(lookup_predictor(0x00).is_err());
        assert!(lookup_predictor(0x04).is_err());
    }

    #[test]
    fn round1_lookup_rejects_higher_bit_depths() {
        // M0Y2 (10-bit) is in the CSV but not round-1.
        assert!(lookup_round1(0x6c).is_err());
        // M8Y0 (8-bit) is round-1.
        assert!(lookup_round1(0x69).is_ok());
    }
}
