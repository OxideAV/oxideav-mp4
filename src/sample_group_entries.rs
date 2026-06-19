//! Typed decoders + builders for the standard ISO/IEC 14496-12 §10
//! **sample-group description entries**.
//!
//! The demuxer surfaces every `sgpd` (SampleGroupDescriptionBox, §8.9.3)
//! entry as an opaque `Vec<u8>` blob — the container deliberately does
//! not interpret a `grouping_type`'s payload, because the meaning belongs
//! to the layer that knows the type (see `demux.rs`'s `sgpd_<n>` metadata
//! surface). This module is that interpretation layer for the
//! grouping types the **base** specification itself defines in §10:
//!
//! | `grouping_type` | §      | Entry class                | This module |
//! |-----------------|--------|----------------------------|-------------|
//! | `roll`          | 10.1   | Visual/AudioRollRecoveryEntry | [`RollRecoveryEntry`] |
//! | `prol`          | 10.1   | AudioPreRollEntry          | [`RollRecoveryEntry`] |
//! | `rash`          | 10.2   | RateShareEntry             | [`RateShareEntry`] |
//! | `alst`          | 10.3   | AlternativeStartupEntry    | [`AlternativeStartupEntry`] |
//! | `rap `          | 10.4   | VisualRandomAccessEntry    | [`VisualRandomAccessEntry`] |
//! | `tele`          | 10.5   | TemporalLevelEntry         | [`TemporalLevelEntry`] |
//! | `sap `          | 10.6   | SAPEntry                   | [`SapEntry`] |
//!
//! The CENC `seig` grouping type (ISO/IEC 23001-7 §6) has its own typed
//! parser in [`crate::cenc::parse_seig`]; this module covers the boxes
//! defined by the base 14496-12 standard.
//!
//! Each `parse_*` consumes one entry's blob (exactly the bytes the
//! demuxer stored per entry) and each `build_*` produces the same blob.
//! `parse(build(e)) == e` round-trips for every type. Parsers tolerate
//! trailing bytes from a future edition (mirroring the §6 "clients SHALL
//! ignore additional bytes" posture `parse_seig` already follows) except
//! where the trailing bytes carry meaning (`alst`'s optional output-rate
//! tail, `rash`'s multi-operation-point list).

use oxideav_core::{Error, Result};

/// The four-byte `grouping_type` selecting one of the §10 entry layouts.
///
/// `from_blob` / `to_blob` route to the matching typed parser/builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleGroupGroupingType {
    /// `roll` — VisualRollRecoveryEntry / AudioRollRecoveryEntry (§10.1).
    Roll,
    /// `prol` — AudioPreRollEntry (§10.1).
    Prol,
    /// `rash` — RateShareEntry (§10.2).
    RateShare,
    /// `alst` — AlternativeStartupEntry (§10.3).
    AlternativeStartup,
    /// `rap ` — VisualRandomAccessEntry (§10.4).
    RandomAccessPoint,
    /// `tele` — TemporalLevelEntry (§10.5).
    TemporalLevel,
    /// `sap ` — SAPEntry (§10.6).
    Sap,
}

impl SampleGroupGroupingType {
    /// Map a four-byte grouping type to its §10 variant, or `None` for a
    /// type not defined by the base specification (e.g. a codec-binding
    /// grouping type like `sync`, or the CENC `seig`).
    pub fn from_fourcc(fourcc: &[u8; 4]) -> Option<Self> {
        match fourcc {
            b"roll" => Some(Self::Roll),
            b"prol" => Some(Self::Prol),
            b"rash" => Some(Self::RateShare),
            b"alst" => Some(Self::AlternativeStartup),
            b"rap " => Some(Self::RandomAccessPoint),
            b"tele" => Some(Self::TemporalLevel),
            b"sap " => Some(Self::Sap),
            _ => None,
        }
    }

    /// The on-wire four-byte `grouping_type` FourCC.
    pub fn fourcc(self) -> [u8; 4] {
        match self {
            Self::Roll => *b"roll",
            Self::Prol => *b"prol",
            Self::RateShare => *b"rash",
            Self::AlternativeStartup => *b"alst",
            Self::RandomAccessPoint => *b"rap ",
            Self::TemporalLevel => *b"tele",
            Self::Sap => *b"sap ",
        }
    }
}

// ---------------------------------------------------------------------------
// §10.1 Roll / Pre-roll
// ---------------------------------------------------------------------------

/// `roll` / `prol` entry (§10.1.1.2): a single signed 16-bit
/// `roll_distance`.
///
/// Both `VisualRollRecoveryEntry` / `AudioRollRecoveryEntry` (`roll`) and
/// `AudioPreRollEntry` (`prol`) share the same one-field layout — the
/// `grouping_type` distinguishes recovery-roll from pre-roll semantics,
/// not the entry shape. Parse with [`parse_roll`], build with
/// [`build_roll`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RollRecoveryEntry {
    /// `roll_distance` (§10.1.1.3): the number of samples that must be
    /// decoded for a sample to be decoded correctly. Positive = samples
    /// *after* the group member; negative = samples *before*. The value
    /// zero "must not be used" per §10.1.1.3 (the sync sample table
    /// documents the no-roll random-access points) — the parser does not
    /// reject it (a malformed producer slip should not abort the open;
    /// callers validating conformance can check `roll_distance != 0`).
    pub roll_distance: i16,
}

/// Parse a `roll` / `prol` entry blob into a [`RollRecoveryEntry`].
///
/// Trailing bytes after the 2-byte `roll_distance` are ignored (future
/// edition tolerance).
pub fn parse_roll(blob: &[u8]) -> Result<RollRecoveryEntry> {
    let b = blob
        .get(0..2)
        .ok_or_else(|| Error::invalid("sgpd roll/prol entry: need 2 bytes for roll_distance"))?;
    Ok(RollRecoveryEntry {
        roll_distance: i16::from_be_bytes([b[0], b[1]]),
    })
}

/// Serialise a [`RollRecoveryEntry`] into the 2-byte entry blob.
pub fn build_roll(e: &RollRecoveryEntry) -> Vec<u8> {
    e.roll_distance.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// §10.2 Rate share (`rash`)
// ---------------------------------------------------------------------------

/// One operation point of a multi-point [`RateShareEntry`] (§10.2.2.2):
/// an `(available_bitrate, target_rate_share)` pair.
///
/// Present only when `operation_point_count > 1` (the single-point form
/// stores its `target_rate_share` directly and carries no
/// `available_bitrate`). `available_bitrate` defines the operation point
/// in kilobits/second; entries shall be strictly increasing (§10.2.2.3),
/// though the parser does not enforce monotonicity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateShareOperationPoint {
    /// `available_bitrate` (§10.2.2.3): the total available bitrate (kbps)
    /// that defines this operation point.
    pub available_bitrate: u32,
    /// `target_rate_share` (§10.2.2.3): the percentage of available
    /// bandwidth to allocate to the media at this operation point. Zero
    /// means "no information on the preferred rate share is provided".
    pub target_rate_share: u16,
}

/// `rash` entry (§10.2.2.2): `RateShareEntry`.
///
/// Aids a server / player allocating bitrate across streams sharing a
/// bandwidth resource (§10.2.1). The on-wire form has two shapes keyed by
/// `operation_point_count`: a single operation point stores one bare
/// `target_rate_share`; multiple points store an `(available_bitrate,
/// target_rate_share)` pair each. Both shapes share the trailing
/// `maximum_bitrate` / `minimum_bitrate` / `discard_priority` fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RateShareEntry {
    /// The single-point `target_rate_share` when
    /// `operation_point_count == 1`. `Some(_)` selects the single-point
    /// wire shape; `None` selects the multi-point shape from
    /// `operation_points`. Exactly one of `single_target_rate_share` /
    /// a non-empty `operation_points` is meaningful — [`build_rash`]
    /// prefers `single_target_rate_share` when set.
    pub single_target_rate_share: Option<u16>,
    /// The `(available_bitrate, target_rate_share)` operation points when
    /// `operation_point_count > 1`. Empty for the single-point form.
    pub operation_points: Vec<RateShareOperationPoint>,
    /// `maximum_bitrate` (§10.2.2.3): a non-zero kbps upper threshold for
    /// bandwidth allocation; 0 = "no information provided".
    pub maximum_bitrate: u32,
    /// `minimum_bitrate` (§10.2.2.3): a non-zero kbps lower threshold; 0 =
    /// "no information provided".
    pub minimum_bitrate: u32,
    /// `discard_priority` (§10.2.2.3): the priority of the track when
    /// tracks are discarded to meet rate-share constraints — the highest
    /// value is discarded first.
    pub discard_priority: u8,
}

/// Parse a `rash` entry blob into a [`RateShareEntry`].
///
/// The `operation_point_count` discriminator selects the single-point
/// (one bare `target_rate_share`) or multi-point
/// (`available_bitrate` + `target_rate_share` per point) wire shape per
/// §10.2.2.2. A `operation_point_count == 0` is rejected (§10.2.2.3 fixes
/// it non-zero); trailing bytes after `discard_priority` are ignored.
pub fn parse_rash(blob: &[u8]) -> Result<RateShareEntry> {
    let read_u16 =
        |o: usize| -> Option<u16> { blob.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]])) };
    let read_u32 = |o: usize| -> Option<u32> {
        blob.get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let op_count = read_u16(0)
        .ok_or_else(|| Error::invalid("sgpd rash entry: operation_point_count truncated"))?;
    if op_count == 0 {
        return Err(Error::invalid(
            "sgpd rash entry: operation_point_count must be non-zero (§10.2.2.3)",
        ));
    }
    let mut off = 2;
    let (single_target_rate_share, operation_points) = if op_count == 1 {
        let t = read_u16(off)
            .ok_or_else(|| Error::invalid("sgpd rash entry: target_rate_share truncated"))?;
        off += 2;
        (Some(t), Vec::new())
    } else {
        let mut pts = Vec::with_capacity(op_count as usize);
        for _ in 0..op_count {
            let ab = read_u32(off)
                .ok_or_else(|| Error::invalid("sgpd rash entry: available_bitrate truncated"))?;
            off += 4;
            let t = read_u16(off)
                .ok_or_else(|| Error::invalid("sgpd rash entry: target_rate_share truncated"))?;
            off += 2;
            pts.push(RateShareOperationPoint {
                available_bitrate: ab,
                target_rate_share: t,
            });
        }
        (None, pts)
    };
    let maximum_bitrate = read_u32(off)
        .ok_or_else(|| Error::invalid("sgpd rash entry: maximum_bitrate truncated"))?;
    off += 4;
    let minimum_bitrate = read_u32(off)
        .ok_or_else(|| Error::invalid("sgpd rash entry: minimum_bitrate truncated"))?;
    off += 4;
    let discard_priority = *blob
        .get(off)
        .ok_or_else(|| Error::invalid("sgpd rash entry: discard_priority truncated"))?;
    Ok(RateShareEntry {
        single_target_rate_share,
        operation_points,
        maximum_bitrate,
        minimum_bitrate,
        discard_priority,
    })
}

/// Serialise a [`RateShareEntry`] into its entry blob.
///
/// When `single_target_rate_share` is `Some(_)` the single-point wire
/// shape is emitted (`operation_point_count = 1`); otherwise the
/// multi-point shape is emitted with `operation_point_count =
/// operation_points.len()` (at least 1 — an empty `operation_points` with
/// no `single_target_rate_share` would be an invalid count-0 box, so the
/// builder writes a single zero-share point to keep the box valid).
pub fn build_rash(e: &RateShareEntry) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(t) = e.single_target_rate_share {
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&t.to_be_bytes());
    } else if e.operation_points.is_empty() {
        // Avoid emitting the §10.2.2.3-invalid count-0 box: degrade to a
        // single-point form with a zero (no-information) target share.
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
    } else {
        let count = e.operation_points.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&count.to_be_bytes());
        for p in e.operation_points.iter().take(count as usize) {
            out.extend_from_slice(&p.available_bitrate.to_be_bytes());
            out.extend_from_slice(&p.target_rate_share.to_be_bytes());
        }
    }
    out.extend_from_slice(&e.maximum_bitrate.to_be_bytes());
    out.extend_from_slice(&e.minimum_bitrate.to_be_bytes());
    out.push(e.discard_priority);
    out
}

// ---------------------------------------------------------------------------
// §10.3 Alternative startup sequence (`alst`)
// ---------------------------------------------------------------------------

/// One `(num_output_samples, num_total_samples)` piece of an
/// [`AlternativeStartupEntry`]'s optional output-rate tail (§10.3.2 do-loop).
///
/// The alternative startup sequence is divided into consecutive pieces,
/// each with a constant sample output rate (§10.3.3). `num_output_samples`
/// is the number of output samples of the piece; `num_total_samples` is
/// the total number of samples (including those not in the alternative
/// startup sequence) spanned by the piece.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlstOutputRatePiece {
    /// `num_output_samples[j]` (§10.3.3).
    pub num_output_samples: u16,
    /// `num_total_samples[j]` (§10.3.3).
    pub num_total_samples: u16,
}

/// `alst` entry (§10.3.2): `AlternativeStartupEntry`.
///
/// Documents an alternative startup sequence enabling faster start-up
/// for hierarchically-scalable streams (§10.3.4). The entry carries a
/// `roll_count`-long table of decoding-time deltas plus an optional
/// trailing run of output-rate pieces read "until the end of the
/// structure" (§10.3.2 do-loop), so this entry is **length-sensitive**:
/// the parser needs the exact entry blob (it consumes the output-rate
/// pieces from whatever bytes remain after the fixed header + offset
/// array).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlternativeStartupEntry {
    /// `first_output_sample` (§10.3.3): the 1-based index of the first
    /// sample intended for output among the samples in the alternative
    /// startup sequence (the sync initial sample is index 1). When
    /// `sample_offsets` is empty (`roll_count == 0`) the associated
    /// sample does not belong to any alternative startup sequence and
    /// the semantics of this field are unspecified (§10.3.3).
    pub first_output_sample: u16,
    /// `sample_offset[1..=roll_count]` (§10.3.3): the decoding-time delta
    /// of the i-th sample in the alternative startup sequence relative to
    /// its regular decoding time. `roll_count` is implicit from the
    /// length of this vector. An empty vector means `roll_count == 0`.
    pub sample_offsets: Vec<u32>,
    /// The optional output-rate tail (§10.3.2 do-loop): zero or more
    /// `(num_output_samples, num_total_samples)` pieces read until the end
    /// of the entry. Empty when the producer wrote no tail.
    pub output_rate_pieces: Vec<AlstOutputRatePiece>,
}

/// Parse an `alst` entry blob into an [`AlternativeStartupEntry`].
///
/// The optional output-rate tail is consumed from the bytes that remain
/// after the fixed `roll_count` + `first_output_sample` header and the
/// `roll_count`-long `sample_offset` array (§10.3.2 "until the end of the
/// structure") — so the caller must pass exactly one entry's blob. A
/// trailing partial piece (fewer than 4 bytes) is rejected: §10.3.2's
/// do-loop pairs `num_output_samples` with `num_total_samples`, so an odd
/// tail length is a framing error, not a future-edition extension.
pub fn parse_alst(blob: &[u8]) -> Result<AlternativeStartupEntry> {
    let read_u16 =
        |o: usize| -> Option<u16> { blob.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]])) };
    let read_u32 = |o: usize| -> Option<u32> {
        blob.get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let roll_count = read_u16(0)
        .ok_or_else(|| Error::invalid("sgpd alst entry: roll_count truncated"))?
        as usize;
    let first_output_sample = read_u16(2)
        .ok_or_else(|| Error::invalid("sgpd alst entry: first_output_sample truncated"))?;
    let mut off = 4;
    let mut sample_offsets = Vec::with_capacity(roll_count);
    for _ in 0..roll_count {
        let v = read_u32(off)
            .ok_or_else(|| Error::invalid("sgpd alst entry: sample_offset truncated"))?;
        sample_offsets.push(v);
        off += 4;
    }
    // Optional output-rate tail: read 4-byte pieces until the entry ends.
    let tail = &blob[off..];
    if tail.len() % 4 != 0 {
        return Err(Error::invalid(
            "sgpd alst entry: output-rate tail not a whole number of (out,total) pieces",
        ));
    }
    let mut output_rate_pieces = Vec::with_capacity(tail.len() / 4);
    for chunk in tail.chunks_exact(4) {
        output_rate_pieces.push(AlstOutputRatePiece {
            num_output_samples: u16::from_be_bytes([chunk[0], chunk[1]]),
            num_total_samples: u16::from_be_bytes([chunk[2], chunk[3]]),
        });
    }
    Ok(AlternativeStartupEntry {
        first_output_sample,
        sample_offsets,
        output_rate_pieces,
    })
}

/// Serialise an [`AlternativeStartupEntry`] into its variable-length entry
/// blob. `roll_count` is written from `sample_offsets.len()` (capped at
/// `u16::MAX`).
pub fn build_alst(e: &AlternativeStartupEntry) -> Vec<u8> {
    let roll_count = e.sample_offsets.len().min(u16::MAX as usize) as u16;
    let mut out =
        Vec::with_capacity(4 + e.sample_offsets.len() * 4 + e.output_rate_pieces.len() * 4);
    out.extend_from_slice(&roll_count.to_be_bytes());
    out.extend_from_slice(&e.first_output_sample.to_be_bytes());
    for &o in e.sample_offsets.iter().take(roll_count as usize) {
        out.extend_from_slice(&o.to_be_bytes());
    }
    for p in &e.output_rate_pieces {
        out.extend_from_slice(&p.num_output_samples.to_be_bytes());
        out.extend_from_slice(&p.num_total_samples.to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// §10.4 Random access points (`rap `)
// ---------------------------------------------------------------------------

/// `rap ` entry (§10.4.2): `VisualRandomAccessEntry`.
///
/// A single byte packs a 1-bit `num_leading_samples_known` flag plus a
/// 7-bit `num_leading_samples` count. Members of this group are random
/// access points (§10.4.1) — and may, but need not, also be sync samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisualRandomAccessEntry {
    /// `num_leading_samples_known` (§10.4.3): `true` when the number of
    /// leading samples is known for each sample in this group (and given
    /// by `num_leading_samples`). A leading sample is one associated with
    /// an "open" RAP that precedes the RAP in presentation order but
    /// cannot be correctly decoded when decoding starts at the RAP.
    pub num_leading_samples_known: bool,
    /// `num_leading_samples` (§10.4.3): the number of leading samples for
    /// each sample in this group. When `num_leading_samples_known` is
    /// `false` this field "should be ignored" — the parser still surfaces
    /// the on-wire 7-bit value verbatim so a round-trip is byte-exact.
    /// Range `0..=127` (7 bits); [`build_rap`] masks to 7 bits.
    pub num_leading_samples: u8,
}

/// Parse a `rap ` entry blob into a [`VisualRandomAccessEntry`].
///
/// Trailing bytes after the single packed byte are ignored.
pub fn parse_rap(blob: &[u8]) -> Result<VisualRandomAccessEntry> {
    let b = *blob
        .first()
        .ok_or_else(|| Error::invalid("sgpd rap entry: need 1 byte"))?;
    Ok(VisualRandomAccessEntry {
        num_leading_samples_known: (b & 0x80) != 0,
        num_leading_samples: b & 0x7F,
    })
}

/// Serialise a [`VisualRandomAccessEntry`] into the 1-byte entry blob.
///
/// `num_leading_samples` is masked to its 7-bit field width.
pub fn build_rap(e: &VisualRandomAccessEntry) -> Vec<u8> {
    let known = if e.num_leading_samples_known { 0x80 } else { 0 };
    vec![known | (e.num_leading_samples & 0x7F)]
}

// ---------------------------------------------------------------------------
// §10.5 Temporal level (`tele`)
// ---------------------------------------------------------------------------

/// `tele` entry (§10.5.2): `TemporalLevelEntry`.
///
/// The temporal level of samples in the group equals the sample-group
/// description index (§10.5.3); a single byte packs a 1-bit
/// `level_independently_decodable` flag plus 7 reserved bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TemporalLevelEntry {
    /// `level_independently_decodable` (§10.5.3): `true` (1) indicates all
    /// samples of this level have no coding dependencies on samples of
    /// other levels; `false` (0) indicates no information is provided.
    pub level_independently_decodable: bool,
}

/// Parse a `tele` entry blob into a [`TemporalLevelEntry`].
///
/// The trailing 7 reserved bits (§10.5.2, `reserved=0`) are ignored, as
/// are any bytes past the first.
pub fn parse_tele(blob: &[u8]) -> Result<TemporalLevelEntry> {
    let b = *blob
        .first()
        .ok_or_else(|| Error::invalid("sgpd tele entry: need 1 byte"))?;
    Ok(TemporalLevelEntry {
        level_independently_decodable: (b & 0x80) != 0,
    })
}

/// Serialise a [`TemporalLevelEntry`] into the 1-byte entry blob (the 7
/// reserved bits are written as zero per §10.5.2).
pub fn build_tele(e: &TemporalLevelEntry) -> Vec<u8> {
    vec![if e.level_independently_decodable {
        0x80
    } else {
        0
    }]
}

// ---------------------------------------------------------------------------
// §10.6 Stream access point (`sap `)
// ---------------------------------------------------------------------------

/// `sap ` entry (§10.6.2): `SAPEntry`.
///
/// A single byte packs `dependent_flag` (1 bit), 3 reserved bits, and a
/// 4-bit `SAP_type`. Identifies samples whose first byte is the position
/// `ISAU` for a Stream Access Point (Annex I) of the indicated type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SapEntry {
    /// `dependent_flag` (§10.6.3): `false` (0) for non-layered media.
    /// `true` (1) specifies that the reference layers (if any) for
    /// predicting the target layers may have to be decoded to access a
    /// sample of this group; `false` specifies they need not be.
    pub dependent_flag: bool,
    /// `SAP_type` (§10.6.3): the SAP type (Annex I) of the associated
    /// samples. Values 0 and 7 are reserved; 1..=6 specify the type.
    /// Range `0..=15` (4 bits); [`build_sap`] masks to 4 bits.
    pub sap_type: u8,
}

/// Parse a `sap ` entry blob into a [`SapEntry`].
///
/// The 3 reserved bits (§10.6.3 — "Parsers shall allow and ignore all
/// values of reserved") are not validated; bytes past the first are
/// ignored.
pub fn parse_sap(blob: &[u8]) -> Result<SapEntry> {
    let b = *blob
        .first()
        .ok_or_else(|| Error::invalid("sgpd sap entry: need 1 byte"))?;
    Ok(SapEntry {
        dependent_flag: (b & 0x80) != 0,
        sap_type: b & 0x0F,
    })
}

/// Serialise a [`SapEntry`] into the 1-byte entry blob (reserved bits
/// written as zero, `SAP_type` masked to 4 bits).
pub fn build_sap(e: &SapEntry) -> Vec<u8> {
    let dep = if e.dependent_flag { 0x80 } else { 0 };
    vec![dep | (e.sap_type & 0x0F)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roll_roundtrip_positive() {
        let e = RollRecoveryEntry { roll_distance: 4 };
        let blob = build_roll(&e);
        assert_eq!(blob, vec![0x00, 0x04]);
        assert_eq!(parse_roll(&blob).unwrap(), e);
    }

    #[test]
    fn roll_roundtrip_negative() {
        let e = RollRecoveryEntry { roll_distance: -3 };
        let blob = build_roll(&e);
        assert_eq!(blob, (-3i16).to_be_bytes());
        assert_eq!(parse_roll(&blob).unwrap(), e);
    }

    #[test]
    fn roll_tolerates_trailing_bytes() {
        let blob = [0x00, 0x05, 0xDE, 0xAD];
        assert_eq!(parse_roll(&blob).unwrap().roll_distance, 5);
    }

    #[test]
    fn roll_rejects_short() {
        assert!(parse_roll(&[0x00]).is_err());
        assert!(parse_roll(&[]).is_err());
    }

    #[test]
    fn grouping_type_fourcc_roundtrip() {
        for gt in [
            SampleGroupGroupingType::Roll,
            SampleGroupGroupingType::Prol,
            SampleGroupGroupingType::RateShare,
            SampleGroupGroupingType::AlternativeStartup,
            SampleGroupGroupingType::RandomAccessPoint,
            SampleGroupGroupingType::TemporalLevel,
            SampleGroupGroupingType::Sap,
        ] {
            assert_eq!(SampleGroupGroupingType::from_fourcc(&gt.fourcc()), Some(gt));
        }
        assert_eq!(SampleGroupGroupingType::from_fourcc(b"sync"), None);
        assert_eq!(SampleGroupGroupingType::from_fourcc(b"seig"), None);
    }

    #[test]
    fn rap_roundtrip_known() {
        let e = VisualRandomAccessEntry {
            num_leading_samples_known: true,
            num_leading_samples: 3,
        };
        let blob = build_rap(&e);
        assert_eq!(blob, vec![0x83]); // 1<<7 | 3
        assert_eq!(parse_rap(&blob).unwrap(), e);
    }

    #[test]
    fn rap_roundtrip_unknown() {
        // num_leading_samples_known = 0; the 7-bit count is still carried
        // verbatim (§10.4.3 says "should be ignored", not "must be zero").
        let e = VisualRandomAccessEntry {
            num_leading_samples_known: false,
            num_leading_samples: 0x7F,
        };
        let blob = build_rap(&e);
        assert_eq!(blob, vec![0x7F]);
        assert_eq!(parse_rap(&blob).unwrap(), e);
    }

    #[test]
    fn rap_masks_7bit() {
        // A caller-supplied count that overflows 7 bits is masked, not
        // bled into the known flag.
        let e = VisualRandomAccessEntry {
            num_leading_samples_known: false,
            num_leading_samples: 0xFF,
        };
        assert_eq!(build_rap(&e), vec![0x7F]);
    }

    #[test]
    fn rap_rejects_empty() {
        assert!(parse_rap(&[]).is_err());
    }

    #[test]
    fn tele_roundtrip() {
        for indep in [false, true] {
            let e = TemporalLevelEntry {
                level_independently_decodable: indep,
            };
            let blob = build_tele(&e);
            assert_eq!(blob, vec![if indep { 0x80 } else { 0 }]);
            assert_eq!(parse_tele(&blob).unwrap(), e);
        }
    }

    #[test]
    fn tele_ignores_reserved_bits() {
        // Reserved low 7 bits set by a producer slip — parse still reads
        // only the top bit.
        assert!(parse_tele(&[0xFF]).unwrap().level_independently_decodable);
        assert!(!parse_tele(&[0x7F]).unwrap().level_independently_decodable);
    }

    #[test]
    fn sap_roundtrip() {
        let e = SapEntry {
            dependent_flag: true,
            sap_type: 6,
        };
        let blob = build_sap(&e);
        assert_eq!(blob, vec![0x86]); // 1<<7 | 6
        assert_eq!(parse_sap(&blob).unwrap(), e);
    }

    #[test]
    fn sap_ignores_reserved_and_masks() {
        // Reserved bits (0x70) ignored on parse; sap_type masked on build.
        let parsed = parse_sap(&[0xF5]).unwrap(); // dep=1, reserved=111, type=0101
        assert!(parsed.dependent_flag);
        assert_eq!(parsed.sap_type, 5);
        let e = SapEntry {
            dependent_flag: false,
            sap_type: 0xFF,
        };
        assert_eq!(build_sap(&e), vec![0x0F]);
    }

    #[test]
    fn sap_rejects_empty() {
        assert!(parse_sap(&[]).is_err());
    }

    #[test]
    fn alst_roundtrip_no_tail() {
        let e = AlternativeStartupEntry {
            first_output_sample: 2,
            sample_offsets: vec![0, 10, 20],
            output_rate_pieces: vec![],
        };
        let blob = build_alst(&e);
        // roll_count=3, first_output_sample=2, then 3 u32 offsets = 4 + 12.
        assert_eq!(blob.len(), 16);
        assert_eq!(&blob[0..2], &3u16.to_be_bytes());
        assert_eq!(&blob[2..4], &2u16.to_be_bytes());
        assert_eq!(parse_alst(&blob).unwrap(), e);
    }

    #[test]
    fn alst_roundtrip_with_tail() {
        let e = AlternativeStartupEntry {
            first_output_sample: 1,
            sample_offsets: vec![0, 5],
            output_rate_pieces: vec![
                AlstOutputRatePiece {
                    num_output_samples: 4,
                    num_total_samples: 8,
                },
                AlstOutputRatePiece {
                    num_output_samples: 2,
                    num_total_samples: 3,
                },
            ],
        };
        let blob = build_alst(&e);
        // 4 (hdr) + 2*4 (offsets) + 2*4 (pieces) = 20.
        assert_eq!(blob.len(), 20);
        assert_eq!(parse_alst(&blob).unwrap(), e);
    }

    #[test]
    fn alst_roundtrip_roll_count_zero() {
        // roll_count == 0: no alternative startup sequence; first_output
        // semantics unspecified but the field still round-trips.
        let e = AlternativeStartupEntry {
            first_output_sample: 0,
            sample_offsets: vec![],
            output_rate_pieces: vec![],
        };
        let blob = build_alst(&e);
        assert_eq!(blob.len(), 4);
        assert_eq!(parse_alst(&blob).unwrap(), e);
    }

    #[test]
    fn alst_rejects_truncated_offset_array() {
        // roll_count says 2 but only one u32 of offsets present.
        let mut blob = Vec::new();
        blob.extend_from_slice(&2u16.to_be_bytes());
        blob.extend_from_slice(&0u16.to_be_bytes());
        blob.extend_from_slice(&7u32.to_be_bytes());
        assert!(parse_alst(&blob).is_err());
    }

    #[test]
    fn alst_rejects_odd_tail() {
        // Header + one offset + a 2-byte dangling half-piece.
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u16.to_be_bytes());
        blob.extend_from_slice(&1u16.to_be_bytes());
        blob.extend_from_slice(&0u32.to_be_bytes());
        blob.extend_from_slice(&9u16.to_be_bytes());
        assert!(parse_alst(&blob).is_err());
    }

    #[test]
    fn rash_roundtrip_single_point() {
        let e = RateShareEntry {
            single_target_rate_share: Some(60),
            operation_points: vec![],
            maximum_bitrate: 2000,
            minimum_bitrate: 100,
            discard_priority: 128,
        };
        let blob = build_rash(&e);
        // 2 (count) + 2 (target) + 4 (max) + 4 (min) + 1 (prio) = 13.
        assert_eq!(blob.len(), 13);
        assert_eq!(&blob[0..2], &1u16.to_be_bytes()); // operation_point_count
        assert_eq!(parse_rash(&blob).unwrap(), e);
    }

    #[test]
    fn rash_roundtrip_multi_point() {
        let e = RateShareEntry {
            single_target_rate_share: None,
            operation_points: vec![
                RateShareOperationPoint {
                    available_bitrate: 500,
                    target_rate_share: 40,
                },
                RateShareOperationPoint {
                    available_bitrate: 1500,
                    target_rate_share: 70,
                },
            ],
            maximum_bitrate: 3000,
            minimum_bitrate: 200,
            discard_priority: 64,
        };
        let blob = build_rash(&e);
        // 2 (count) + 2*(4+2) (points) + 4 + 4 + 1 = 23.
        assert_eq!(blob.len(), 23);
        assert_eq!(&blob[0..2], &2u16.to_be_bytes());
        assert_eq!(parse_rash(&blob).unwrap(), e);
    }

    #[test]
    fn rash_rejects_count_zero() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&0u16.to_be_bytes()); // operation_point_count = 0
        blob.extend_from_slice(&[0u8; 9]); // max + min + prio
        assert!(parse_rash(&blob).is_err());
    }

    #[test]
    fn rash_rejects_truncated_points() {
        // count = 2 but only one full point present.
        let mut blob = Vec::new();
        blob.extend_from_slice(&2u16.to_be_bytes());
        blob.extend_from_slice(&500u32.to_be_bytes());
        blob.extend_from_slice(&40u16.to_be_bytes());
        assert!(parse_rash(&blob).is_err());
    }

    #[test]
    fn rash_build_empty_degrades_to_single_zero() {
        // No single share and no points → builder writes a valid count-1
        // box with a zero (no-information) target share rather than an
        // invalid count-0 box.
        let e = RateShareEntry {
            single_target_rate_share: None,
            operation_points: vec![],
            maximum_bitrate: 0,
            minimum_bitrate: 0,
            discard_priority: 128,
        };
        let blob = build_rash(&e);
        assert_eq!(&blob[0..2], &1u16.to_be_bytes());
        let parsed = parse_rash(&blob).unwrap();
        assert_eq!(parsed.single_target_rate_share, Some(0));
    }
}
