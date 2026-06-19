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
}
