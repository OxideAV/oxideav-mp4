//! Sample group muxing — write side of `sbgp` (SampleToGroupBox,
//! ISO/IEC 14496-12 §8.9.2) and `sgpd` (SampleGroupDescriptionBox,
//! §8.9.3).
//!
//! These two boxes always travel as a pair: an `sgpd` declares a table
//! of per-group descriptive entries for a given four-byte
//! `grouping_type` (e.g. `roll`, `rap `, `sync`, `alst`, `prol`), and
//! an `sbgp` of the same `grouping_type` maps a track's samples into
//! those groups via a run-length `(sample_count,
//! group_description_index)` table. An index of `0` means "sample is a
//! member of no group of this type"; an index ≥ `0x10001` is a
//! movie-fragment-local reference into a fragment's own `sgpd`
//! (§8.9.4) and is preserved verbatim — this builder does not resolve
//! fragment-local groups (consistent with the demuxer side).
//!
//! The per-group entry payload inside `sgpd` is **grouping-type-specific
//! and opaque to the container**: this crate carries it as `Vec<u8>`
//! and never inspects it, mirroring the demuxer behaviour. A layer
//! that knows the `grouping_type` semantics (for example a `roll`-aware
//! codec consumer that wants to encode a signed `roll_distance`)
//! supplies the bytes pre-serialised.
//!
//! # Layouts
//!
//! ## `sbgp` (SampleToGroupBox, §8.9.2)
//!
//! ```text
//! FullBox('sbgp', version, 0)
//! unsigned int(32) grouping_type
//! if (version == 1)
//!     unsigned int(32) grouping_type_parameter
//! unsigned int(32) entry_count
//! entry_count × {
//!     unsigned int(32) sample_count
//!     unsigned int(32) group_description_index
//! }
//! ```
//!
//! Version is `1` iff `grouping_type_parameter` is `Some(_)`, else `0`.
//!
//! ## `sgpd` (SampleGroupDescriptionBox, §8.9.3)
//!
//! ```text
//! FullBox('sgpd', version, 0)
//! unsigned int(32) grouping_type
//! if (version == 1) unsigned int(32) default_length
//! if (version >= 2) unsigned int(32) default_sample_description_index
//! unsigned int(32) entry_count
//! entry_count × {
//!     if (version == 1 && default_length == 0)
//!         unsigned int(32) description_length
//!     SampleGroupEntry(grouping_type)   // grouping-type-specific blob
//! }
//! ```
//!
//! The builder picks the version automatically per §8.9.3.2:
//!
//! * If `default_sample_description_index` is `Some(_)` and all entries
//!   share a common non-zero byte length → **version 2** with the
//!   `default_sample_description_index` field; the entries are emitted
//!   back-to-back without per-entry length prefixes (callers must know
//!   the entry size from the `grouping_type` semantics on the read
//!   side).
//! * If all entries share a common non-zero byte length → **version 1**
//!   with `default_length = <that length>`; entries are packed
//!   back-to-back.
//! * Otherwise → **version 1** with `default_length = 0`; each entry is
//!   preceded by its own `u32 description_length`.
//!
//! The version-0 "no per-entry length signalling" form (§8.9.3.3 NOTE,
//! deprecated) is intentionally not emitted; the spec recommends
//! against it precisely because it cannot be scanned, and round-tripping
//! it against the demuxer side reduces to "one combined blob" which is
//! not a faithful reconstruction.
//!
//! # Pairing
//!
//! For a sample-group pair to be meaningful on read, the writer should
//! ensure the `sbgp.grouping_type` matches some `sgpd.grouping_type` in
//! the same track's `stbl` and that the cumulative
//! `sum(sample_count)` matches the track's sample count (§8.9.2.1).
//! Mismatches are not rejected — a producer that intentionally writes a
//! partial map (e.g. a stream-friendly format that only labels the
//! first few seconds) is free to do so. The demuxer just surfaces the
//! pair verbatim.

use crate::boxes::{SBGP, SGPD};

/// One `sbgp` (SampleToGroupBox) to emit on a track.
///
/// `grouping_type` is the four-byte selector that pairs this box with
/// an `sgpd` of the same type. The common types in ISO/IEC 14496-12
/// are `roll` (audio roll-back distance), `rap ` (random-access
/// points), `sync` (sync samples), `alst` (alternate startup
/// sequence), `prol` (audio preroll); other groupings are defined in
/// codec-binding specs.
///
/// `grouping_type_parameter` selects an alternative grouping of the
/// same type when `Some(_)` (version 1 of `sbgp`); `None` emits
/// version 0.
///
/// `entries` is the run-length table — each pair `(sample_count,
/// group_description_index)` says "the next `sample_count` samples
/// belong to group `group_description_index`". `group_description_index
/// = 0` means "no group of this type" for the run; an index ≥
/// `0x10001` is a movie-fragment-local reference and is written
/// verbatim (the muxer does not resolve fragment-local groups). The
/// cumulative `sum(sample_count)` should match the track's total
/// sample count per §8.9.2.1 — the builder does not validate this.
#[derive(Clone, Debug, Default)]
pub struct SampleToGroup {
    /// Four-byte grouping type (e.g. `*b"roll"`, `*b"rap "`).
    pub grouping_type: [u8; 4],
    /// `grouping_type_parameter` — selects an alternative grouping of
    /// the same type. `Some(_)` emits version 1; `None` emits version 0.
    pub grouping_type_parameter: Option<u32>,
    /// Run-length entries `(sample_count, group_description_index)`.
    /// Empty is a legal "zero-entry" `sbgp`.
    pub entries: Vec<(u32, u32)>,
}

/// One `sgpd` (SampleGroupDescriptionBox) to emit on a track.
///
/// Pairs with an `sbgp` of the same `grouping_type`. The per-group
/// entry payload is grouping-type-specific and opaque to the container —
/// callers supply `entries` as already-serialised `Vec<u8>`s.
///
/// `default_sample_description_index` (if `Some(_)`) requests the
/// version-2 layout: the box header carries a
/// `default_sample_description_index` to apply to samples not mapped
/// by any `sbgp` of this type, and entries are emitted back-to-back
/// without per-entry length prefixes (all entries must share a common
/// non-zero length).
#[derive(Clone, Debug, Default)]
pub struct SampleGroupDescription {
    /// Four-byte grouping type matching the paired `sbgp`.
    pub grouping_type: [u8; 4],
    /// `default_sample_description_index` (version 2). `Some(0)` means
    /// "no group of this type" per §8.9.3 (the default value).
    pub default_sample_description_index: Option<u32>,
    /// Per-group entry payloads, grouping-type-specific and opaque.
    /// Empty is a legal "zero-entry" `sgpd`.
    pub entries: Vec<Vec<u8>>,
}

/// Serialise an [`SampleToGroup`] into a complete `sbgp` box ready to
/// append to a track's `stbl` body.
///
/// Picks version 0 if `grouping_type_parameter` is `None`, version 1
/// otherwise. The returned slice includes the 8-byte ISO BMFF box
/// header (`size:u32 + type='sbgp'`).
pub fn build_sbgp(s: &SampleToGroup) -> Vec<u8> {
    let version: u8 = if s.grouping_type_parameter.is_some() {
        1
    } else {
        0
    };
    // header (FullBox: 1B version + 3B flags) + grouping_type(4) +
    //   (v1: grouping_type_parameter(4)) + entry_count(4) + entries*8
    let mut body =
        Vec::with_capacity(4 + 4 + if version == 1 { 4 } else { 0 } + 4 + s.entries.len() * 8);
    body.push(version);
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(&s.grouping_type);
    if let Some(p) = s.grouping_type_parameter {
        body.extend_from_slice(&p.to_be_bytes());
    }
    body.extend_from_slice(&(s.entries.len() as u32).to_be_bytes());
    for (count, idx) in &s.entries {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&idx.to_be_bytes());
    }
    wrap(&SBGP, &body)
}

/// Serialise a [`SampleGroupDescription`] into a complete `sgpd` box.
///
/// Version is chosen automatically per §8.9.3.2:
///
/// * `default_sample_description_index = Some(_)` and entries share a
///   common non-zero length → **version 2**.
/// * Entries share a common non-zero length → **version 1** with
///   `default_length = <that length>`.
/// * Otherwise → **version 1** with `default_length = 0` (each entry
///   carries its own `u32 description_length`).
///
/// Empty entry list → version 1 with `default_length = 0` and
/// `entry_count = 0`.
pub fn build_sgpd(s: &SampleGroupDescription) -> Vec<u8> {
    // Decide common-length / per-entry-length / v2 layout.
    let common_len = entries_common_length(&s.entries);
    let want_v2 = s.default_sample_description_index.is_some() && common_len.is_some();
    let (version, default_length): (u8, u32) = if want_v2 {
        (2, 0)
    } else if let Some(cl) = common_len {
        (1, cl as u32)
    } else {
        (1, 0)
    };

    let entries_payload_len = match (version, default_length) {
        (1, 0) => s.entries.iter().map(|e| 4 + e.len()).sum::<usize>(),
        (1, dl) => s.entries.len() * dl as usize,
        (2, _) => s.entries.iter().map(|e| e.len()).sum::<usize>(),
        _ => unreachable!(),
    };

    // header sizing: 4 (full) + 4 (grouping_type)
    //   + 4 (default_length if v1)
    //   + 4 (default_sample_description_index if v2)
    //   + 4 (entry_count)
    //   + entries
    let extra = match version {
        1 => 4,
        2 => 4,
        _ => unreachable!(),
    };
    let mut body = Vec::with_capacity(4 + 4 + extra + 4 + entries_payload_len);
    body.push(version);
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(&s.grouping_type);
    match version {
        1 => body.extend_from_slice(&default_length.to_be_bytes()),
        2 => body.extend_from_slice(
            &s.default_sample_description_index
                .unwrap_or(0)
                .to_be_bytes(),
        ),
        _ => unreachable!(),
    }
    body.extend_from_slice(&(s.entries.len() as u32).to_be_bytes());

    match (version, default_length) {
        (1, 0) => {
            for e in &s.entries {
                body.extend_from_slice(&(e.len() as u32).to_be_bytes());
                body.extend_from_slice(e);
            }
        }
        (1, _) => {
            for e in &s.entries {
                body.extend_from_slice(e);
            }
        }
        (2, _) => {
            for e in &s.entries {
                body.extend_from_slice(e);
            }
        }
        _ => unreachable!(),
    }
    wrap(&SGPD, &body)
}

/// `Some(len)` iff all entries share the same non-zero byte length.
/// `None` for an empty list (no fixed length to claim) or for a list
/// with heterogeneous lengths.
fn entries_common_length(entries: &[Vec<u8>]) -> Option<usize> {
    let first = entries.first()?;
    if first.is_empty() {
        return None;
    }
    let len = first.len();
    if entries.iter().all(|e| e.len() == len) {
        Some(len)
    } else {
        None
    }
}

fn wrap(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbgp_v0_two_runs_byte_exact() {
        let s = SampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            entries: vec![(10, 1), (5, 0)],
        };
        let b = build_sbgp(&s);
        // header: size = 8 + 4 (full) + 4 (gt) + 4 (count) + 2*8 = 36
        assert_eq!(b.len(), 36);
        assert_eq!(&b[0..4], &36u32.to_be_bytes());
        assert_eq!(&b[4..8], b"sbgp");
        assert_eq!(&b[8..12], &[0, 0, 0, 0]); // version 0 + flags
        assert_eq!(&b[12..16], b"roll");
        assert_eq!(&b[16..20], &2u32.to_be_bytes()); // entry_count
        assert_eq!(&b[20..24], &10u32.to_be_bytes());
        assert_eq!(&b[24..28], &1u32.to_be_bytes());
        assert_eq!(&b[28..32], &5u32.to_be_bytes());
        assert_eq!(&b[32..36], &0u32.to_be_bytes());
    }

    #[test]
    fn sbgp_v1_with_parameter_byte_exact() {
        let s = SampleToGroup {
            grouping_type: *b"rap ",
            grouping_type_parameter: Some(7),
            entries: vec![(3, 2)],
        };
        let b = build_sbgp(&s);
        // size = 8 + 4 + 4 + 4 + 4 + 8 = 32
        assert_eq!(b.len(), 32);
        assert_eq!(&b[8..12], &[1, 0, 0, 0]); // version 1 + flags
        assert_eq!(&b[12..16], b"rap ");
        assert_eq!(&b[16..20], &7u32.to_be_bytes()); // grouping_type_parameter
        assert_eq!(&b[20..24], &1u32.to_be_bytes()); // entry_count
        assert_eq!(&b[24..28], &3u32.to_be_bytes());
        assert_eq!(&b[28..32], &2u32.to_be_bytes());
    }

    #[test]
    fn sbgp_zero_entries_legal() {
        let s = SampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            entries: vec![],
        };
        let b = build_sbgp(&s);
        // size = 8 + 4 + 4 + 4 = 20
        assert_eq!(b.len(), 20);
        assert_eq!(&b[16..20], &0u32.to_be_bytes()); // entry_count = 0
    }

    #[test]
    fn sbgp_fragment_local_index_preserved() {
        let s = SampleToGroup {
            grouping_type: *b"sync",
            grouping_type_parameter: None,
            entries: vec![(1, 0x1_0001)],
        };
        let b = build_sbgp(&s);
        assert_eq!(&b[24..28], &0x1_0001u32.to_be_bytes());
    }

    #[test]
    fn sgpd_v1_fixed_length_when_entries_share_size() {
        let s = SampleGroupDescription {
            grouping_type: *b"roll",
            default_sample_description_index: None,
            entries: vec![vec![0xFF, 0xFB], vec![0x00, 0x05]],
        };
        let b = build_sgpd(&s);
        // size = 8 + 4 + 4 + 4 (default_length) + 4 (entry_count) + 2*2 = 28
        assert_eq!(b.len(), 28);
        assert_eq!(&b[8..12], &[1, 0, 0, 0]); // version 1
        assert_eq!(&b[12..16], b"roll");
        assert_eq!(&b[16..20], &2u32.to_be_bytes()); // default_length = 2
        assert_eq!(&b[20..24], &2u32.to_be_bytes()); // entry_count
        assert_eq!(&b[24..26], &[0xFF, 0xFB]);
        assert_eq!(&b[26..28], &[0x00, 0x05]);
    }

    #[test]
    fn sgpd_v1_variable_length_when_entries_differ() {
        let s = SampleGroupDescription {
            grouping_type: *b"prol",
            default_sample_description_index: None,
            entries: vec![vec![0xAA, 0xBB, 0xCC], vec![0xDD]],
        };
        let b = build_sgpd(&s);
        // size = 8 + 4 + 4 + 4 (default_length = 0) + 4 (entry_count)
        //   + (4 + 3) + (4 + 1) = 36
        assert_eq!(b.len(), 36);
        assert_eq!(&b[8..12], &[1, 0, 0, 0]); // version 1
        assert_eq!(&b[16..20], &0u32.to_be_bytes()); // default_length = 0
        assert_eq!(&b[20..24], &2u32.to_be_bytes()); // entry_count
        assert_eq!(&b[24..28], &3u32.to_be_bytes()); // description_length 0
        assert_eq!(&b[28..31], &[0xAA, 0xBB, 0xCC]);
        assert_eq!(&b[31..35], &1u32.to_be_bytes()); // description_length 1
        assert_eq!(&b[35..36], &[0xDD]);
    }

    #[test]
    fn sgpd_v2_when_default_sample_description_index_set() {
        let s = SampleGroupDescription {
            grouping_type: *b"alst",
            default_sample_description_index: Some(3),
            entries: vec![vec![0x01, 0x02], vec![0x03, 0x04]],
        };
        let b = build_sgpd(&s);
        // size = 8 + 4 + 4 + 4 (default_sample_description_index) + 4 (entry_count) + 4 (entries) = 28
        assert_eq!(b.len(), 28);
        assert_eq!(&b[8..12], &[2, 0, 0, 0]); // version 2
        assert_eq!(&b[12..16], b"alst");
        assert_eq!(&b[16..20], &3u32.to_be_bytes()); // default_sample_description_index
        assert_eq!(&b[20..24], &2u32.to_be_bytes()); // entry_count
        assert_eq!(&b[24..26], &[0x01, 0x02]);
        assert_eq!(&b[26..28], &[0x03, 0x04]);
    }

    #[test]
    fn sgpd_v2_falls_back_to_v1_when_entries_differ() {
        // default_sample_description_index is set, but entries don't
        // share a length → can't use v2's no-length form. Falls back
        // to v1 with per-entry length.
        let s = SampleGroupDescription {
            grouping_type: *b"alst",
            default_sample_description_index: Some(3),
            entries: vec![vec![0x01], vec![0x02, 0x03]],
        };
        let b = build_sgpd(&s);
        assert_eq!(b[8], 1); // version 1, not 2
        assert_eq!(&b[16..20], &0u32.to_be_bytes()); // default_length = 0
    }

    #[test]
    fn sgpd_empty_entries_legal() {
        let s = SampleGroupDescription {
            grouping_type: *b"roll",
            default_sample_description_index: None,
            entries: vec![],
        };
        let b = build_sgpd(&s);
        // size = 8 + 4 + 4 + 4 (default_length=0) + 4 (entry_count=0) = 24
        assert_eq!(b.len(), 24);
        assert_eq!(&b[8..12], &[1, 0, 0, 0]); // version 1
        assert_eq!(&b[20..24], &0u32.to_be_bytes()); // entry_count = 0
    }

    #[test]
    fn entries_common_length_helper() {
        assert_eq!(entries_common_length(&[]), None);
        assert_eq!(entries_common_length(&[vec![]]), None);
        assert_eq!(entries_common_length(&[vec![1, 2]]), Some(2));
        assert_eq!(entries_common_length(&[vec![1, 2], vec![3, 4]]), Some(2));
        assert_eq!(entries_common_length(&[vec![1], vec![2, 3]]), None);
    }
}
