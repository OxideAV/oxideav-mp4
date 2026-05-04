//! Muxer configuration for the MP4 / ISOBMFF writer.
//!
//! The default [`Mp4MuxerOptions`] matches what `muxer::open` has always done:
//! major brand `mp42`, no faststart, no fragmentation. Three convenience
//! presets are provided via [`BrandPreset`] for the common `mp4`, `mov`, and
//! `ismv` registry entries; a `Custom` variant lets callers supply any
//! major + compatible brand list directly.
//!
//! Setting [`Mp4MuxerOptions::fragmented`] to `Some(...)` switches the muxer
//! into fragmented-MP4 mode (DASH / HLS / Smooth-Streaming / CMAF output).

/// Brand preset controlling the `ftyp` box written at the start of the file.
///
/// The four-byte codes follow ISO/IEC 14496-12 and the de-facto QuickTime /
/// Smooth Streaming conventions:
///
/// * [`Mp4`](BrandPreset::Mp4): `mp42` / `isom mp42 mp41 iso2`
/// * [`Mov`](BrandPreset::Mov): `qt  ` / `qt  `
/// * [`Ismv`](BrandPreset::Ismv): `iso4` / `iso4 piff iso6 isml`
/// * [`Custom`](BrandPreset::Custom): caller-supplied major + compatible list
#[derive(Clone, Debug)]
pub enum BrandPreset {
    /// Standard MP4 — `major=mp42`, compatible=`isom mp42 mp41 iso2`.
    Mp4,
    /// Apple QuickTime — `major=qt  `, compatible=`qt  `.
    Mov,
    /// Microsoft Smooth Streaming / ISMV — `major=iso4`, compatible=`iso4 piff iso6 isml`.
    Ismv,
    /// Custom brand with an explicit major + compatible list.
    Custom {
        major: [u8; 4],
        compatible: Vec<[u8; 4]>,
    },
}

impl BrandPreset {
    /// Return the major brand for this preset.
    pub fn major_brand(&self) -> [u8; 4] {
        match self {
            BrandPreset::Mp4 => *b"mp42",
            BrandPreset::Mov => *b"qt  ",
            BrandPreset::Ismv => *b"iso4",
            BrandPreset::Custom { major, .. } => *major,
        }
    }

    /// Return the list of compatible brands for this preset.
    pub fn compatible_brands(&self) -> Vec<[u8; 4]> {
        match self {
            BrandPreset::Mp4 => vec![*b"isom", *b"mp42", *b"mp41", *b"iso2"],
            BrandPreset::Mov => vec![*b"qt  "],
            BrandPreset::Ismv => vec![*b"iso4", *b"piff", *b"iso6", *b"isml"],
            BrandPreset::Custom { compatible, .. } => compatible.clone(),
        }
    }
}

/// Cadence policy controlling when the fragmented muxer emits a `moof+mdat`
/// pair (one segment / fragment per flush).
///
/// In a true CMAF / DASH `init+seg*` workflow each `moof+mdat` becomes one
/// addressable HTTP range; the cadence picks how big each one is.
#[derive(Clone, Copy, Debug)]
pub enum FragmentCadence {
    /// Flush whenever the running fragment duration of the *first* track
    /// (typically video) reaches `seconds`. Falls back to per-track total
    /// when there is no first track. Compressed audio samples are tiny
    /// (~20 ms each) so picking 2..6 s yields reasonable fragment sizes.
    EverySeconds(f64),
    /// Flush at every keyframe of the *first* track (typically video). The
    /// run before the first keyframe is held until one arrives. Audio-only
    /// inputs (every audio sample is a keyframe) effectively get one
    /// fragment per audio sample with this — pair with seconds/N for
    /// audio-only output.
    EveryKeyframe,
    /// Flush every `n` packets of the first track. Useful for testing
    /// (predictable cadence without timing dependence).
    EveryNPackets(u32),
}

/// Fragmented-MP4 muxer options.
///
/// When [`Mp4MuxerOptions::fragmented`] is `Some(FragmentedOptions { .. })`,
/// the muxer writes the file as
///
/// ```text
/// ftyp
/// moov  (mvex+trex; no media samples in moov)
/// styp? + moof + mdat   (per fragment, repeated)
/// styp? + moof + mdat
/// ...
/// ```
///
/// matching ISO/IEC 14496-12 §8.8 (Movie Fragments) + DASH-IF Interop
/// guidelines for `styp` brands.
#[derive(Clone, Debug)]
pub struct FragmentedOptions {
    /// When to flush a fragment; see [`FragmentCadence`].
    pub cadence: FragmentCadence,
    /// Emit a `styp` SegmentTypeBox before each `moof+mdat` pair (CMAF
    /// segment marker). When `None`, no `styp` is written and the file is
    /// a plain fragmented ISOBMFF (still valid for any DASH parser, but
    /// not a CMAF-conformant addressable segment).
    ///
    /// DASH-IF Interop §6.2 recommends `styp(major=msdh, compat=msdh msix)`
    /// for an indexed media segment, or `cmfs` / `cmff` for CMAF brand
    /// signalling. The default `Some(BrandPreset::Custom { major: msdh,
    /// compatible: [msdh, msix] })` is the broadly-interop choice.
    pub styp: Option<BrandPreset>,
}

impl Default for FragmentedOptions {
    fn default() -> Self {
        Self {
            cadence: FragmentCadence::EverySeconds(2.0),
            styp: Some(BrandPreset::Custom {
                major: *b"msdh",
                compatible: vec![*b"msdh", *b"msix"],
            }),
        }
    }
}

/// Runtime options controlling how the MP4 muxer shapes its output.
///
/// Call [`Mp4MuxerOptions::default`] for the historical behavior of the
/// plain `"mp4"` registry entry (major=`mp42`, no faststart, no fragmentation).
#[derive(Clone, Debug)]
pub struct Mp4MuxerOptions {
    /// `ftyp` brand preset written at the beginning of the file.
    pub brand: BrandPreset,
    /// If `true`, rewrite the file at `write_trailer` time so `moov` precedes
    /// `mdat` ("faststart" / "web-optimized" layout). Requires a seekable
    /// output (which `WriteSeek` already provides). Mutually exclusive with
    /// `fragmented`.
    pub faststart: bool,
    /// If `Some(...)`, switch the muxer to fragmented-MP4 mode (DASH / HLS /
    /// Smooth-Streaming / CMAF). The first call to `write_header` emits
    /// `ftyp + moov` (with `mvex+trex` defaults, no media samples); each
    /// fragment cadence boundary emits `styp? + moof + mdat`. Mutually
    /// exclusive with `faststart`.
    pub fragmented: Option<FragmentedOptions>,
}

impl Default for Mp4MuxerOptions {
    fn default() -> Self {
        Self {
            brand: BrandPreset::Mp4,
            faststart: false,
            fragmented: None,
        }
    }
}
