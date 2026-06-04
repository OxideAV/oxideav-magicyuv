#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the public Huffman
//! sub-surface (`huffman::parse_lengths` + `HuffmanTable::build` +
//! `HuffmanTable::decode_into_u8` / `decode_into_u16`) directly,
//! bypassing the v7 frame header / slice-table / preamble framing
//! that `decode_magicyuv` walks first.
//!
//! The full-frame target (`decode_magicyuv`) only reaches this code
//! path once it has parsed a 32-byte header (spec/01 §3) and a slice
//! table (spec/02 §5) — that framing rejects most random byte
//! sequences before they reach `huffman::*`. This harness pushes
//! arbitrary bytes straight into the descriptor parser + canonical
//! table builder, so the fuzzer concentrates on:
//!
//! - **spec/05 §1.1** run-length descriptor decode — literal byte
//!   (`b < 0x80`, length value) vs run form (`b ≥ 0x80`, next byte =
//!   run count - 1), including the `max_length` rejection branch
//!   that yields `HuffmanLengthExceedsMax`, and the truncation
//!   rejection that yields `Truncated`.
//! - **spec/05 §2.0** canonical-Huffman code construction — the
//!   audit-corrected longest-length-first cumulative accumulator
//!   (NOT RFC 1951), the `(1 << len) <= acc` over-full Kraft check
//!   that yields `HuffmanOverfull`, and the boundary at
//!   `len = max_length` where the `1u64 << len` shift is computed
//!   in u64 to avoid the u32-overflow trap.
//! - The two-level lookup arithmetic (`primary_bits = 12`, the
//!   `REDIRECT_MARKER` sentinel, the per-prefix subtable allocation
//!   keyed by `code[s] >> (l - primary_bits)`, and the residual-bit
//!   spread inside each subtable).
//! - The `decode_into_u{8,16}` hot loops (post-build) on an
//!   adversarially-chosen payload — the BitReader's MSB-first
//!   left-aligned accumulator (`peek_bits` / `consume`), the
//!   single-level fast path, and the two-level redirect arm.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0   : tier selector (mod 4) → bit_depth ∈ {8, 10, 12, 14}
//!              → n_symbols   ∈ {256, 1024, 4096, 16384}
//!              → max_length  ∈ {12, 14, 16, 18}
//!   bytes 1-2: descriptor cap (LE u16) — number of bytes (capped at
//!              16 KiB) the parser may consume before bailing. The
//!              parser stops emitting at `n_symbols` lengths or
//!              `descriptor.len()`, whichever comes first; the cap
//!              just bounds the slice we hand it so a runaway
//!              fuzz input doesn't tilt the per-iteration cost.
//!   bytes 3..: descriptor bytes for `parse_lengths`. Whatever bytes
//!              remain after `parse_lengths` returns `used` are then
//!              fed to a `BitReader` and driven through
//!              `decode_into_u{8,16}` for a bounded `out` length so
//!              the post-build decode loop is exercised too.
//! ```
//!
//! ## Contracts under test
//!
//! 1. `parse_lengths(...)` always returns. A malformed descriptor
//!    yields `Err(Truncated | HuffmanLengthExceedsMax)`; a
//!    well-formed one yields exactly `n_symbols` lengths. No panic,
//!    no overflow, no OOB.
//! 2. `HuffmanTable::build(lengths, plane)` always returns. An
//!    over-full code book yields `Err(HuffmanOverfull)`; a
//!    well-formed one yields `Ok(table)` whose `lengths().len() ==
//!    n_symbols`. No panic, no overflow, no OOB. In particular the
//!    `1u64 << len` cast at `len = max_length` (18 for the 14-bit
//!    tier) must not overflow the way a `1u32 << 18` cast would on
//!    a strict-overflow build.
//! 3. After a successful `build`, `decode_into_u8` /
//!    `decode_into_u16` always returns (`true` for `max_len > 0`,
//!    `false` for the degenerate all-unused descriptor) regardless
//!    of how short the trailing payload is — the `BitReader` pads
//!    with zero past EOF per `spec/05` §3.3.
//!
//! ## Allocation cap rationale
//!
//! `parse_lengths` allocates `n` u8s (≤ 16384), `build` allocates
//! ≤ 16 KiB of primary table + ≤ 4096 secondary tables × 64 entries
//! each (worst case). The cap on descriptor input bytes (16 KiB) and
//! on `out` length (`n_symbols`) keeps the per-iteration footprint
//! bounded — no resource-request false positives. Compare with
//! `decode_magicyuv`'s 16 MiB raster cap which sizes the full-frame
//! decoder's per-plane allocation; this sub-surface harness's caps
//! are about three orders of magnitude tighter because the surface
//! is itself that much smaller.

use libfuzzer_sys::fuzz_target;
use oxideav_magicyuv::bitreader::BitReader;
use oxideav_magicyuv::huffman::{parse_lengths, HuffmanTable};

/// Upper bound on the descriptor slice fed to `parse_lengths`. The
/// parser stops at the first `n_symbols` emitted lengths anyway, so
/// 16 KiB is generous — it bounds the per-iteration cost when the
/// fuzzer hands us many consecutive run-form bytes that emit no
/// progress per pass.
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024;

/// Map the tier-selector byte to `(n_symbols, max_length)` per
/// `spec/05` §1.1's `HuffCoderT<…, bits, max_length, hint>` template
/// parameters.
fn tier(selector: u8) -> (usize, u8) {
    match selector & 0x03 {
        0 => (1 << 8, 12),  // 8-bit
        1 => (1 << 10, 14), // 10-bit
        2 => (1 << 12, 16), // 12-bit
        _ => (1 << 14, 18), // 14-bit
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let (n_symbols, max_length) = tier(data[0]);
    let cap_raw = u16::from_le_bytes([data[1], data[2]]) as usize;
    let cap = cap_raw.min(MAX_DESCRIPTOR_BYTES);
    let tail = &data[3..];
    let descriptor_end = cap.min(tail.len());
    let descriptor = &tail[..descriptor_end];

    // (1) Descriptor parse — exercises spec/05 §1.1 RLE decode.
    let (lengths, used) = match parse_lengths(descriptor, n_symbols, max_length, 0) {
        Ok(pair) => pair,
        Err(_) => return,
    };
    debug_assert_eq!(lengths.len(), n_symbols);

    // (2) Canonical-Huffman build — exercises spec/05 §2.0 the
    //     longest-length-first cumulative accumulator + Kraft check.
    let table = match HuffmanTable::build(lengths, 0) {
        Ok(t) => t,
        Err(_) => return,
    };

    // The build accepts the degenerate all-unused descriptor
    // (Kraft = 0; v2.4.2 never produces this but the parse path
    // allows it). `decode_into_u*` then returns `false` without
    // touching the BitReader; we skip the decode-loop exercise in
    // that case to keep the harness focused on table-shape paths
    // the rest of the fuzzer can stress.
    if table.is_empty() {
        return;
    }

    // (3) Post-build decode hot loop. Drive the remaining tail through
    //     `decode_into_u{8,16}` for a bounded `out` length so the
    //     BitReader peek/consume + the single-level / two-level
    //     selector arms in `huffman.rs` see fuzz pressure on top of
    //     whatever table shape the build produced.
    let payload = &tail[descriptor_end.min(tail.len())..];
    let payload_after_desc = &payload[used.min(payload.len())..];
    // Bound `out_len` by `n_symbols` so the harness's per-iteration
    // cost is dominated by table-construction (the surface under
    // test) rather than the decode loop. Cap at 4096 so even the
    // 14-bit tier stays under 8 KiB of `out` allocation.
    let out_len = (payload_after_desc.len().min(n_symbols)).min(4096);
    if out_len == 0 {
        return;
    }

    if max_length <= 12 {
        // 8-bit tier path → `decode_into_u8`.
        let mut br = BitReader::new(payload_after_desc);
        let mut out = vec![0u8; out_len];
        let _ = table.decode_into_u8(&mut br, &mut out);
    } else {
        // 10/12/14-bit tier path → `decode_into_u16` with the
        // per-bit-depth mask the decoder applies (`(1 << bit_depth)
        // - 1`). Reconstruct it from `n_symbols`.
        let mask = (n_symbols - 1) as u16;
        let mut br = BitReader::new(payload_after_desc);
        let mut out = vec![0u16; out_len];
        let _ = table.decode_into_u16(&mut br, &mut out, mask);
    }
});
