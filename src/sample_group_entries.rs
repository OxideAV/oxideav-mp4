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
}
