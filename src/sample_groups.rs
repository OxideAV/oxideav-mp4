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

use crate::boxes::{CSGP, SBGP, SGPD};

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

/// One pattern of a [`CompactSampleToGroup`] (`csgp`, §8.9.5): a run of
/// `indices.len()` per-sample `sample_group_description_index` values
/// (the *pattern*), replicated across `sample_count` consecutive groups
/// of that length. The pattern therefore covers `sample_count *
/// indices.len()` samples in total.
///
/// `pattern_length` is implicit from `indices.len()` (the builder writes
/// it), matching the read side which reconstructs it the same way.
#[derive(Clone, Debug, Default)]
pub struct CompactSampleToGroupPattern {
    /// `sample_count[i]` — number of consecutive groups (each
    /// `indices.len()` samples long) that replay this pattern.
    pub sample_count: u32,
    /// `sample_group_description_index[i][1..=pattern_length]` — one
    /// index per sample of the pattern. `0` means "member of no group of
    /// this type"; in a `traf` the index's most-significant bit (for the
    /// chosen field width) distinguishes a fragment-local description
    /// (set) from a global one (clear). The raw value is written
    /// verbatim — the builder does not synthesise the fragment-local bit.
    pub indices: Vec<u32>,
}

/// One `csgp` (CompactSampleToGroupBox, ISO/IEC 14496-12:2020 §8.9.5) to
/// emit on a track — the compact alternative to [`SampleToGroup`].
///
/// Where `sbgp` emits one `(sample_count, group_description_index)` pair
/// per run, `csgp` groups samples into a small set of **patterns** that
/// are each replicated across the track: pattern `i` replays its
/// `indices` for `sample_count[i]` consecutive groups. A track whose
/// per-sample group membership is periodic (the common reason to pick the
/// compact form) shrinks dramatically — the index run is bit-packed at
/// the narrowest width that fits.
///
/// Pairs with an `sgpd` of the same `grouping_type` exactly like `sbgp`.
#[derive(Clone, Debug, Default)]
pub struct CompactSampleToGroup {
    /// Four-byte grouping type matching the paired `sgpd`.
    pub grouping_type: [u8; 4],
    /// `grouping_type_parameter` — selects an alternative grouping of the
    /// same type. `Some(_)` sets the flag-layout presence bit and emits
    /// the optional `u32` field; `None` omits it.
    pub grouping_type_parameter: Option<u32>,
    /// `index_msb_indicates_fragment_local_description` — flag-layout
    /// **bit 7** (§8.9.5). Set it (only legal when emitting into a `traf`)
    /// to declare that the most-significant bit of each index is a
    /// fragment-local-vs-global `sgpd` source selector. The builder writes
    /// each index value verbatim; it does not synthesise the selector bit,
    /// so a caller that sets this must pre-set the high bit on indices that
    /// should reference the fragment-local `sgpd`. Defaults to `false`
    /// (`stbl` form / no MSB special-casing).
    pub index_msb_indicates_fragment_local_description: bool,
    /// The repeating index patterns, in emit order.
    pub patterns: Vec<CompactSampleToGroupPattern>,
}

/// Map a maximum field value to the narrowest §8.9.5 2-bit size code.
///
/// The width function is `width = 4 << code` (code 0→4, 1→8, 2→16,
/// 3→32 bits). The smallest code whose width holds `max` is chosen so a
/// `csgp` is as compact as the data allows; an all-zero column still
/// picks code 0 (4 bits), the spec's minimum width.
fn size_code_for(max: u32) -> u8 {
    if max <= 0xF {
        0
    } else if max <= 0xFF {
        1
    } else if max <= 0xFFFF {
        2
    } else {
        3
    }
}

/// MSB-first big-endian bit writer — the inverse of the demuxer's
/// `BitCursor`. Accumulates bits into a byte buffer; the final partial
/// byte is zero-padded on `finish` so the box body stays byte-aligned
/// (§8.9.5 leaves no defined meaning for trailing pad bits, matching the
/// read side which simply stops once every declared field is consumed).
struct BitWriter {
    out: Vec<u8>,
    /// Bits already filled in the in-progress final byte (0..=7); `0`
    /// means the buffer is byte-aligned and a fresh byte starts the next
    /// write.
    bits_filled: u8,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            out: Vec::new(),
            bits_filled: 0,
        }
    }

    /// Write the low `n` bits (0..=32) of `value`, MSB-first. `n == 0`
    /// writes nothing.
    fn write(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            if self.bits_filled == 0 {
                self.out.push(0);
            }
            let last = self.out.len() - 1;
            self.out[last] |= bit << (7 - self.bits_filled);
            self.bits_filled = (self.bits_filled + 1) & 7;
        }
    }

    /// Zero-pad the final partial byte and return the buffer.
    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Serialise a [`CompactSampleToGroup`] into a complete `csgp` box ready
/// to append to a track's `stbl` (or a `traf`) body.
///
/// The three bit-field width codes (for `sample_group_description_index`,
/// `sample_count`, and `pattern_length`) are chosen automatically as the
/// narrowest §8.9.5 widths that hold every value present, and packed into
/// the `FullBox.flags` field together with the
/// `grouping_type_parameter_present` bit:
///
/// ```text
///     index_size_code                              = flags[0..1] (2 bits)
///     count_size_code                              = flags[2..3] (2 bits)
///     pattern_size_code                            = flags[4..5] (2 bits)
///     grouping_type_parameter_present              = flags[6]     (1 bit)
///     index_msb_indicates_fragment_local_description = flags[7]   (1 bit)
/// ```
///
/// The fixed-width header fields (`grouping_type`, optional
/// `grouping_type_parameter`, `pattern_count`) are byte-aligned `u32`s;
/// from `pattern_count` onward the `(pattern_length, sample_count)` array
/// and then the flattened index run are bit-packed MSB-first at the
/// chosen widths (no byte alignment between fields), the exact inverse of
/// `demux::parse_csgp`. The returned slice includes the 8-byte ISO BMFF
/// box header (`size:u32 + type='csgp'`).
pub fn build_csgp(c: &CompactSampleToGroup) -> Vec<u8> {
    let max_pattern_length = c
        .patterns
        .iter()
        .map(|p| p.indices.len() as u32)
        .max()
        .unwrap_or(0);
    let max_sample_count = c.patterns.iter().map(|p| p.sample_count).max().unwrap_or(0);
    let max_index = c
        .patterns
        .iter()
        .flat_map(|p| p.indices.iter().copied())
        .max()
        .unwrap_or(0);

    let pattern_size_code = size_code_for(max_pattern_length);
    let count_size_code = size_code_for(max_sample_count);
    let index_size_code = size_code_for(max_index);
    let pattern_w = 4u32 << pattern_size_code;
    let count_w = 4u32 << count_size_code;
    let index_w = 4u32 << index_size_code;

    let gtpp = c.grouping_type_parameter.is_some();
    // FullBox flags: index[0..1], count[2..3], pattern[4..5], gtpp[6],
    // index_msb_indicates_fragment_local_description[7] (§8.9.5).
    let flags: u32 = (index_size_code as u32)
        | ((count_size_code as u32) << 2)
        | ((pattern_size_code as u32) << 4)
        | (if gtpp { 1 } else { 0 } << 6)
        | (if c.index_msb_indicates_fragment_local_description {
            1
        } else {
            0
        } << 7);

    let mut body = Vec::new();
    body.push(0); // version 0
    body.extend_from_slice(&flags.to_be_bytes()[1..]); // 24-bit flags
    body.extend_from_slice(&c.grouping_type);
    if let Some(p) = c.grouping_type_parameter {
        body.extend_from_slice(&p.to_be_bytes());
    }
    body.extend_from_slice(&(c.patterns.len() as u32).to_be_bytes());

    // Bit-packed region: all (pattern_length, sample_count) pairs first,
    // then every pattern's index run flattened in order.
    let mut bits = BitWriter::new();
    for p in &c.patterns {
        bits.write(p.indices.len() as u32, pattern_w);
        bits.write(p.sample_count, count_w);
    }
    for p in &c.patterns {
        for &idx in &p.indices {
            bits.write(idx, index_w);
        }
    }
    body.extend_from_slice(&bits.finish());

    wrap(&CSGP, &body)
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

    #[test]
    fn size_code_for_picks_narrowest_width() {
        assert_eq!(size_code_for(0), 0); // all-zero still 4 bits
        assert_eq!(size_code_for(0xF), 0); // 4-bit boundary
        assert_eq!(size_code_for(0x10), 1); // needs 8 bits
        assert_eq!(size_code_for(0xFF), 1);
        assert_eq!(size_code_for(0x100), 2); // needs 16 bits
        assert_eq!(size_code_for(0xFFFF), 2);
        assert_eq!(size_code_for(0x1_0000), 3); // needs 32 bits
        assert_eq!(size_code_for(u32::MAX), 3);
    }

    #[test]
    fn bit_writer_msb_first() {
        let mut w = BitWriter::new();
        // 0b101 then 0b01 → 0b10101 padded to 0b1010_1000 = 0xA8.
        w.write(0b101, 3);
        w.write(0b01, 2);
        let out = w.finish();
        assert_eq!(out, vec![0xA8]);
    }

    #[test]
    fn csgp_4bit_widths_byte_exact() {
        // One pattern: pattern_length = 2, sample_count = 3, indices
        // [1, 2]. All values ≤ 0xF → every size code is 0 (4-bit width),
        // so flags = 0 and the bit-packed region is:
        //   pattern_length=2 (4b) sample_count=3 (4b) → 0x23
        //   idx[0]=1 (4b) idx[1]=2 (4b)               → 0x12
        let c = CompactSampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 3,
                indices: vec![1, 2],
            }],
        };
        let b = build_csgp(&c);
        // size = 8 (hdr) + 4 (full) + 4 (gt) + 4 (pattern_count) + 2 (bits)
        assert_eq!(b.len(), 22);
        assert_eq!(&b[0..4], &22u32.to_be_bytes());
        assert_eq!(&b[4..8], b"csgp");
        assert_eq!(&b[8..12], &[0, 0, 0, 0]); // version 0 + flags 0
        assert_eq!(&b[12..16], b"roll");
        assert_eq!(&b[16..20], &1u32.to_be_bytes()); // pattern_count
        assert_eq!(b[20], 0x23); // pattern_length=2 | sample_count=3
        assert_eq!(b[21], 0x12); // idx 1 | idx 2
    }

    #[test]
    fn csgp_flags_encode_size_codes() {
        // Force distinct codes: index needs 8 bits (0x10), count needs
        // 16 bits (0x100), pattern_length is 1 → 4 bits.
        let c = CompactSampleToGroup {
            grouping_type: *b"sync",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 0x100,
                indices: vec![0x10],
            }],
        };
        let b = build_csgp(&c);
        // flags: index_size_code=1 (bits0..1), count_size_code=2
        // (bits2..3), pattern_size_code=0 (bits4..5), gtpp=0.
        let flags = u32::from_be_bytes([0, b[9], b[10], b[11]]);
        assert_eq!(flags & 0x3, 1); // index_size_code
        assert_eq!((flags >> 2) & 0x3, 2); // count_size_code
        assert_eq!((flags >> 4) & 0x3, 0); // pattern_size_code
        assert_eq!((flags >> 6) & 0x1, 0); // gtpp
    }

    #[test]
    fn csgp_with_grouping_type_parameter_sets_presence_bit() {
        let c = CompactSampleToGroup {
            grouping_type: *b"rap ",
            grouping_type_parameter: Some(7),
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 1,
                indices: vec![1],
            }],
        };
        let b = build_csgp(&c);
        let flags = u32::from_be_bytes([0, b[9], b[10], b[11]]);
        assert_eq!((flags >> 6) & 0x1, 1); // gtpp bit set
        assert_eq!(&b[12..16], b"rap ");
        assert_eq!(&b[16..20], &7u32.to_be_bytes()); // grouping_type_parameter
        assert_eq!(&b[20..24], &1u32.to_be_bytes()); // pattern_count
    }

    #[test]
    fn csgp_empty_patterns_legal() {
        let c = CompactSampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![],
        };
        let b = build_csgp(&c);
        // size = 8 + 4 + 4 + 4 = 20, pattern_count = 0, no bit region.
        assert_eq!(b.len(), 20);
        assert_eq!(&b[16..20], &0u32.to_be_bytes());
    }

    /// Build → parse round-trip through the canonical demuxer reader for
    /// a spread of widths, the grouping_type_parameter, multiple
    /// patterns, and the fragment-local high bit.
    #[test]
    fn csgp_roundtrip_through_parser() {
        let cases = vec![
            CompactSampleToGroup {
                grouping_type: *b"roll",
                grouping_type_parameter: None,
                index_msb_indicates_fragment_local_description: false,
                patterns: vec![CompactSampleToGroupPattern {
                    sample_count: 3,
                    indices: vec![1, 2],
                }],
            },
            CompactSampleToGroup {
                grouping_type: *b"rap ",
                grouping_type_parameter: Some(42),
                index_msb_indicates_fragment_local_description: false,
                patterns: vec![
                    CompactSampleToGroupPattern {
                        sample_count: 0x100,
                        indices: vec![0x10, 0, 0xFF],
                    },
                    CompactSampleToGroupPattern {
                        sample_count: 1,
                        indices: vec![0x1_0000],
                    },
                ],
            },
            CompactSampleToGroup {
                grouping_type: *b"sync",
                grouping_type_parameter: None,
                index_msb_indicates_fragment_local_description: false,
                // fragment-local high bit set on an 8-bit-wide index.
                patterns: vec![CompactSampleToGroupPattern {
                    sample_count: 5,
                    indices: vec![0x8000_0001],
                }],
            },
        ];
        for c in &cases {
            let bytes = build_csgp(c);
            // Strip the 8-byte box header — parse_csgp_box takes the body.
            let parsed = crate::demux::parse_csgp_box(&bytes[8..]).unwrap();
            assert_eq!(parsed.grouping_type, c.grouping_type);
            assert_eq!(parsed.grouping_type_parameter, c.grouping_type_parameter);
            assert_eq!(parsed.patterns.len(), c.patterns.len());
            for (pp, cp) in parsed.patterns.iter().zip(&c.patterns) {
                assert_eq!(pp.sample_count, cp.sample_count);
                assert_eq!(pp.indices, cp.indices);
            }
        }
    }
}
