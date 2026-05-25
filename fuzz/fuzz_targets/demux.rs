#![no_main]

//! Demux arbitrary fuzz-supplied bytes through the MP4 / ISO Base
//! Media File Format demuxer.
//!
//! The contract under test is purely that the calls *return*: a
//! malformed stream yields `Err(Error::…)`, a well-formed one yields
//! `Ok(_)` packets until `Error::Eof`, and neither path may panic,
//! abort, integer-overflow (in a debug build), index out of bounds,
//! or attempt an attacker-controlled `Vec::with_capacity` /
//! `vec![0; n]` allocation that exceeds what the input could
//! possibly back. Return values are intentionally discarded.
//!
//! The ISO BMFF attack surface this exercises:
//!   * The box-tree walker, which descends `moov > trak > mdia >
//!     minf > stbl > ...` and the parallel `moof > traf > trun`
//!     fragmented-MP4 tree, where every level is a
//!     `size:u32 / type:FourCC [/ largesize:u64]` length-prefixed
//!     container (ISO/IEC 14496-12 §4.2). `size:u32 = 0` ("to EOF")
//!     and `size:u32 = 1` ("largesize follows") are the two box
//!     sentinels that have historically defeated naive parsers.
//!   * Sample-table expansion — `stts`, `stsc`, `stsz`/`stz2`,
//!     `stco`/`co64`, `stss`, `ctts`, `sdtp`, `sbgp`/`sgpd` all
//!     have attacker-controlled entry counts that drive
//!     allocations and per-sample arithmetic.
//!   * Fragmented MP4 — `tfhd` per-track defaults, `tfdt` base
//!     media decode time, `trun` per-sample overrides, all of
//!     which compose into the absolute file-offset arithmetic that
//!     locates each fragment's payload bytes.
//!   * Edit list (`edts/elst`) — signed `media_time` plus
//!     fixed-point `media_rate`, with segment durations in the
//!     (possibly zero) movie timescale.
//!   * Sample-entry inner parsers — `avcC`, `hvcC`, `av1C`, `vpcC`,
//!     `dfLa`, `dOps`, `dac3`, `dec3`, and the BER-encoded `esds`
//!     descriptor chain, all walked under an outer
//!     `data_reference_index` the input chooses.
//!   * Metadata — 3GPP `udta` boxes (`titl`/`auth`/…) and
//!     iTunes-style `meta`/`ilst` (whose `item > data` inner shape
//!     is itself a recursive box tree).
//!   * `seek_to(0, 0)` re-exercises the sample-table walker from a
//!     random offset.
//!
//! Open is the only entry point: a successful open hands back a
//! demuxer whose `next_packet` then walks every sample / fragment.
//! We cap the per-input packet count so a pathological valid stream
//! can't dominate fuzz time.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{NullCodecResolver, ReadSeek};

/// Bound on how many packets we drain per fuzz input. A pathological
/// but legitimate stream (e.g. a one-sample-per-chunk track with a
/// long `stsz` table) could otherwise spin the fuzzer on a single
/// many-packet track instead of exploring the input space.
const MAX_PACKETS_PER_INPUT: usize = 256;

fuzz_target!(|data: &[u8]| {
    // Skip trivially-short inputs — the smallest legal MP4 has at
    // least an 8-byte `ftyp` box header, so anything shorter can't
    // even pass the outermost box read.
    if data.len() < 8 {
        return;
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let Ok(mut dmx) = oxideav_mp4::demux::open(rs, &NullCodecResolver) else {
        return;
    };

    // Touch the metadata + streams slices once. These are populated
    // entirely by the open() path but exercising the accessors
    // catches any post-open invariant the parser might have left in
    // an inconsistent state.
    let _ = dmx.streams().len();
    let _ = dmx.metadata().len();
    let _ = dmx.duration_micros();

    // Drain packets up to MAX_PACKETS_PER_INPUT. The loop terminates
    // on the first error (Eof, invalid, ...) — fuzz inputs are
    // expected to crash the sample-table walker more often than they
    // demux cleanly, so a bounded loop is plenty.
    for _ in 0..MAX_PACKETS_PER_INPUT {
        if dmx.next_packet().is_err() {
            break;
        }
    }

    // Re-exercise the seek path. seek_to(0, 0) is the cheapest
    // possible call — it lands on the first sync sample of stream 0
    // (if any) — and runs the `stss` / sample-offset machinery from
    // a random offset. If the file had no streams this returns Err;
    // that's fine.
    let _ = dmx.seek_to(0, 0);
});
