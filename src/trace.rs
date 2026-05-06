//! JSON-Lines trace emitter for the round-2 Auditor's lockstep harness.
//!
//! Compiled in only when the `trace` Cargo feature is enabled. When
//! the feature is on AND the env var `OXIDEAV_MAGICYUV_TRACE_FILE` is
//! set, the decoder emits one JSONL event per state transition to
//! that path.
//!
//! Event vocabulary + per-event field schema is the round-1 Auditor
//! review's §4 forward spec
//! (`docs/video/magicyuv/audit/02-implementer-rust-round-1-review.md`).
//! This file is the canonical Rust mirror of that schema — the Auditor's
//! `jq`-line-diff harness is the cross-checker that finalises lockstep.
//!
//! The emitter writes a tiny in-house JSON serialiser (no external
//! `serde_json` dep) so the standalone build doesn't gain a dep just
//! because of the trace feature.

#![cfg(feature = "trace")]

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

/// Per-thread (per-decode-call really) trace tape. Each `decode_frame`
/// call constructs a `Tracer`, threads it through, and drops it at the
/// end of decode. Emission is best-effort; I/O errors are ignored.
pub(crate) struct Tracer {
    writer: RefCell<BufWriter<File>>,
}

impl Tracer {
    /// Open the path in `OXIDEAV_MAGICYUV_TRACE_FILE` for append. If the
    /// env var is unset / unreadable, returns `None` and the decoder's
    /// trace points become no-ops.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("OXIDEAV_MAGICYUV_TRACE_FILE")?;
        let path = PathBuf::from(path);
        let file = File::options().create(true).append(true).open(path).ok()?;
        Some(Self {
            writer: RefCell::new(BufWriter::new(file)),
        })
    }

    /// Write one JSONL event line.
    pub fn emit(&self, ev: Event) {
        let mut s = String::with_capacity(256);
        ev.write_to(&mut s);
        s.push('\n');
        let mut w = self.writer.borrow_mut();
        let _ = w.write_all(s.as_bytes());
    }
}

impl Drop for Tracer {
    fn drop(&mut self) {
        let _ = self.writer.borrow_mut().flush();
    }
}

/// A trace event — one JSON object per emit. Field order in the
/// output mirrors the Python ref's `dict` insertion order from the
/// audit/02 §4 schema so a `jq`-line-diff is character-equal apart
/// from per-encoder Huffman descriptor differences.
pub(crate) enum Event<'a> {
    Hdr {
        magic: &'a str,
        version: u8,
        format_byte: u8,
        fourcc: &'a str,
        aux_byte: u8,
        codec_variant: u8,
        flags: u32,
        width: u32,
        height: u32,
        width_extra: u32,
        slice_height: u32,
        bit_depth: u8,
        family: &'a str,
        subsampling: &'a str,
        planes: u8,
    },
    SliceTable {
        entries: &'a [u32],
        total_slices: usize,
        slices_per_plane: usize,
        num_planes: usize,
    },
    Preamble {
        plane_count: u8,
        per_slice_plane_index: &'a [u8],
    },
    Huff {
        plane: usize,
        descriptor_bytes: &'a [u8],
        descriptor_length: usize,
        n_symbols: usize,
        max_length: u8,
        used: bool,
    },
    Payload {
        slice: usize,
        plane: usize,
        slice_in_plane: usize,
        row_start: usize,
        row_end: usize,
        cols: usize,
        file_offset: usize,
        payload_size: usize,
        slice_flags: u8,
        predictor_id: u8,
        mode: &'a str,
        n_pixels: usize,
    },
    PreambleTrailing {
        extra_bytes: &'a [u8],
    },
    Avi {
        chunk: &'a str,
        size: usize,
        extra: &'a [(&'a str, AviField<'a>)],
    },
}

/// Lightweight tagged value for Avi event extras (avoids serde).
#[allow(dead_code)]
pub(crate) enum AviField<'a> {
    U32(u32),
    USize(usize),
    Str(&'a str),
    Bytes(&'a [u8]),
}

impl<'a> Event<'a> {
    fn write_to(&self, out: &mut String) {
        match self {
            Event::Hdr {
                magic,
                version,
                format_byte,
                fourcc,
                aux_byte,
                codec_variant,
                flags,
                width,
                height,
                width_extra,
                slice_height,
                bit_depth,
                family,
                subsampling,
                planes,
            } => {
                out.push_str("{\"kind\":\"hdr\"");
                push_str_field(out, "magic", magic);
                push_int_field(out, "version", *version as u64);
                push_int_field(out, "format_byte", *format_byte as u64);
                push_str_field(out, "fourcc", fourcc);
                push_int_field(out, "aux_byte", *aux_byte as u64);
                push_int_field(out, "codec_variant", *codec_variant as u64);
                push_int_field(out, "flags", *flags as u64);
                push_int_field(out, "width", *width as u64);
                push_int_field(out, "height", *height as u64);
                push_int_field(out, "width_extra", *width_extra as u64);
                push_int_field(out, "slice_height", *slice_height as u64);
                push_int_field(out, "bit_depth", *bit_depth as u64);
                push_str_field(out, "family", family);
                push_str_field(out, "subsampling", subsampling);
                push_int_field(out, "planes", *planes as u64);
                out.push('}');
            }
            Event::SliceTable {
                entries,
                total_slices,
                slices_per_plane,
                num_planes,
            } => {
                out.push_str("{\"kind\":\"slice_table\"");
                out.push_str(",\"entries\":[");
                for (i, e) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("{}", e));
                }
                out.push(']');
                push_int_field(out, "total_slices", *total_slices as u64);
                push_int_field(out, "slices_per_plane", *slices_per_plane as u64);
                push_int_field(out, "num_planes", *num_planes as u64);
                out.push('}');
            }
            Event::Preamble {
                plane_count,
                per_slice_plane_index,
            } => {
                out.push_str("{\"kind\":\"preamble\"");
                push_int_field(out, "plane_count", *plane_count as u64);
                out.push_str(",\"per_slice_plane_index\":[");
                for (i, p) in per_slice_plane_index.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("{}", p));
                }
                out.push(']');
                out.push('}');
            }
            Event::Huff {
                plane,
                descriptor_bytes,
                descriptor_length,
                n_symbols,
                max_length,
                used,
            } => {
                out.push_str("{\"kind\":\"huff\"");
                push_int_field(out, "plane", *plane as u64);
                push_hex_field(out, "descriptor_bytes", descriptor_bytes);
                push_int_field(out, "descriptor_length", *descriptor_length as u64);
                push_int_field(out, "n_symbols", *n_symbols as u64);
                push_int_field(out, "max_length", *max_length as u64);
                out.push_str(",\"used\":");
                out.push_str(if *used { "true" } else { "false" });
                out.push('}');
            }
            Event::Payload {
                slice,
                plane,
                slice_in_plane,
                row_start,
                row_end,
                cols,
                file_offset,
                payload_size,
                slice_flags,
                predictor_id,
                mode,
                n_pixels,
            } => {
                out.push_str("{\"kind\":\"payload\"");
                push_int_field(out, "slice", *slice as u64);
                push_int_field(out, "plane", *plane as u64);
                push_int_field(out, "slice_in_plane", *slice_in_plane as u64);
                push_int_field(out, "row_start", *row_start as u64);
                push_int_field(out, "row_end", *row_end as u64);
                push_int_field(out, "cols", *cols as u64);
                push_int_field(out, "file_offset", *file_offset as u64);
                push_int_field(out, "payload_size", *payload_size as u64);
                push_int_field(out, "slice_flags", *slice_flags as u64);
                push_int_field(out, "predictor_id", *predictor_id as u64);
                push_str_field(out, "mode", mode);
                push_int_field(out, "n_pixels", *n_pixels as u64);
                out.push('}');
            }
            Event::PreambleTrailing { extra_bytes } => {
                out.push_str("{\"kind\":\"preamble_trailing\"");
                push_hex_field(out, "extra_bytes", extra_bytes);
                out.push('}');
            }
            Event::Avi { chunk, size, extra } => {
                out.push_str("{\"kind\":\"avi\"");
                push_str_field(out, "chunk", chunk);
                push_int_field(out, "size", *size as u64);
                for (k, v) in extra.iter() {
                    out.push(',');
                    out.push('"');
                    out.push_str(k);
                    out.push_str("\":");
                    match v {
                        AviField::U32(n) => out.push_str(&format!("{}", n)),
                        AviField::USize(n) => out.push_str(&format!("{}", n)),
                        AviField::Str(s) => {
                            out.push('"');
                            out.push_str(s);
                            out.push('"');
                        }
                        AviField::Bytes(b) => {
                            out.push('"');
                            for byte in b.iter() {
                                out.push_str(&format!("{:02x}", byte));
                            }
                            out.push('"');
                        }
                    }
                }
                out.push('}');
            }
        }
    }
}

fn push_str_field(out: &mut String, key: &str, val: &str) {
    out.push(',');
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    out.push_str(val);
    out.push('"');
}

fn push_int_field(out: &mut String, key: &str, val: u64) {
    out.push(',');
    out.push('"');
    out.push_str(key);
    out.push_str("\":");
    out.push_str(&format!("{}", val));
}

fn push_hex_field(out: &mut String, key: &str, bytes: &[u8]) {
    out.push(',');
    out.push('"');
    out.push_str(key);
    out.push_str("\":\"");
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out.push('"');
}
