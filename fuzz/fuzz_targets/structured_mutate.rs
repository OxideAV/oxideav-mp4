#![no_main]

//! Structure-aware hostile mutation — start from a writer-shaped file
//! (built by the same valid-by-construction recipe the round-trip
//! target uses: plain / faststart / fragmented layouts, 1–2 tracks,
//! edit lists, empty-time gaps, `styp` + `sidx`/`mfra` indexes) and
//! run a bounded mutation program over it:
//!
//!   * byte flips anywhere (box sizes, FourCCs, table entries);
//!   * truncation at an arbitrary point (mid-box, mid-table);
//!   * targeted overwrites right after a chosen structural FourCC —
//!     the exact fields the samplers and offset walkers arithmetic
//!     on: `stts` / `stsc` / `stsz` / `stz2` / `stco` / `co64` /
//!     `stss` / `ctts` entry counts, `trun` sample counts and flags,
//!     `tfhd` / `tfdt` defaults and decode times, `elst` segment
//!     maps, `iloc` / `iref` / `ipma` item tables, `senc` / `saiz` /
//!     `saio` CENC carriers, `sidx` / `tfra` / `mfro` / `mehd`
//!     random-access and duration fields, and the `mdat` payload
//!     itself.
//!
//! Because the mutants start structurally deep, the fuzzer reaches
//! sample-table and fragment-walk states that raw random bytes almost
//! never assemble. Contract: no panic, no abort, no debug-build
//! overflow, no attacker-proportional allocation — whether a mutant
//! opens or errors is the mutant's business. The full accessor
//! battery + bounded drain + both seek paths run on every mutant.

use libfuzzer_sys::fuzz_target;
use oxideav_mp4_fuzz::{MuxPlan, Recipe, exercise_demux};

/// Structural FourCCs whose trailing bytes get targeted overwrites.
const TARGET_FOURCCS: &[&[u8; 4]] = &[
    b"stts", b"stsc", b"stsz", b"stz2", b"stco", b"co64", b"stss", b"ctts", b"trun", b"tfhd",
    b"tfdt", b"trex", b"mehd", b"elst", b"iloc", b"iref", b"ipma", b"iinf", b"senc", b"saiz",
    b"saio", b"sidx", b"tfra", b"mfro", b"mdat", b"stsd", b"mvhd", b"tkhd", b"mdhd", b"moof",
];

fuzz_target!(|data: &[u8]| {
    let mut r = Recipe::new(data);
    let plan = MuxPlan::decode(&mut r);
    let base = plan.run().bytes;
    if base.is_empty() {
        return;
    }

    // Mutation program from the remaining recipe bytes.
    let mut m = base;
    let mut ops = Recipe::new(r.rest());
    let n_ops = 1 + (ops.u8() % 8) as usize;
    for _ in 0..n_ops {
        if m.is_empty() {
            break;
        }
        match ops.u8() % 4 {
            // Byte flip.
            0 | 1 => {
                let pos = ops.u32() as usize % m.len();
                let val = ops.u8();
                m[pos] ^= val.max(1);
            }
            // Truncate (final op: everything after is meaningless).
            2 => {
                let cut = ops.u32() as usize % (m.len() + 1);
                m.truncate(cut);
                break;
            }
            // Targeted structural corruption: overwrite the bytes
            // right after the k-th occurrence of a chosen FourCC.
            _ => {
                let wanted = TARGET_FOURCCS[ops.u8() as usize % TARGET_FOURCCS.len()];
                let mut hits: Vec<usize> = Vec::new();
                for k in 0..m.len().saturating_sub(4) {
                    if &m[k..k + 4] == wanted.as_slice() {
                        hits.push(k);
                    }
                }
                if hits.is_empty() {
                    continue;
                }
                let at = hits[ops.u8() as usize % hits.len()];
                // Land on the version/flags word, the first count, or
                // deeper into the table body.
                let skip = 4 + 4 * (ops.u8() as usize % 4);
                let start = (at + skip).min(m.len());
                let len = 4 + (ops.u8() as usize % 13);
                let end = (start + len).min(m.len());
                for b in &mut m[start..end] {
                    *b = ops.u8();
                }
            }
        }
    }

    exercise_demux(&m);
});
