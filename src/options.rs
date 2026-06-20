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
/// moov                    (mvex+trex; no media samples in moov)
/// sidx?                   (one per fragment, references the next moof+mdat)
/// styp? + moof + mdat     (per fragment, repeated)
/// sidx? + styp? + moof + mdat
/// ...
/// mfra?                   (at end: per-track tfra + mfro size trailer)
/// ```
///
/// matching ISO/IEC 14496-12 §8.8 (Movie Fragments) + §8.16 (sidx) + §8.8.10
/// (mfra) + DASH-IF Interop guidelines for `styp` brands.
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
    /// Emit `sidx` (SegmentIndexBox §8.16.3) before each `moof+mdat` and
    /// an `mfra` (MovieFragmentRandomAccessBox §8.8.10) trailer with
    /// per-track `tfra` random-access tables + the size-of-mfra `mfro`
    /// at end of file. Required for the DASH on-demand profile (single
    /// file with embedded byte-range index) and for fast random-access
    /// without scanning every moof. Default `true`.
    ///
    /// The emitted `sidx` is a single-entry index covering the immediately-
    /// following moof+mdat (the simplest legal form per §8.16.3); a
    /// multi-segment top-level sidx can be layered on by an outer
    /// segmenter if needed.
    pub emit_random_access_indexes: bool,
    /// Per-level assignment entries for a `leva` (LevelAssignmentBox,
    /// ISO/IEC 14496-12 §8.8.13) emitted inside the init `mvex`.
    ///
    /// When non-empty, the muxer writes one `leva` after the `trex` boxes
    /// in `mvex`, advertising how the file's content is partitioned into
    /// **levels** for partial-subsegment fetch. Each entry is a
    /// [`demux::LevaEntry`](crate::demux::LevaEntry) (`track_id` +
    /// `padding_flag` + `assignment_type` + the type-specific tail). The
    /// level *order* in this slice is the level *number* a sibling `ssix`
    /// SubsegmentIndexBox refers to (§8.16.4.2).
    ///
    /// The §8.8.13.3 conformance constraints (`level_count ≥ 2`, the
    /// "zero or more of type 2/3 then zero or more of exactly one type"
    /// ordering rule) are the caller's responsibility; the muxer
    /// serialises whatever is supplied verbatim. Empty by default — most
    /// fragmented files don't declare levels.
    pub levels: Vec<crate::demux::LevaEntry>,
}

impl Default for FragmentedOptions {
    fn default() -> Self {
        Self {
            cadence: FragmentCadence::EverySeconds(2.0),
            styp: Some(BrandPreset::Custom {
                major: *b"msdh",
                compatible: vec![*b"msdh", *b"msix"],
            }),
            emit_random_access_indexes: true,
            levels: Vec::new(),
        }
    }
}

/// Per-track sample-group emission request.
///
/// Each entry attaches an `sbgp` (SampleToGroupBox) and / or `sgpd`
/// (SampleGroupDescriptionBox) pair into one track's `stbl`. The two
/// halves share `grouping_type`; the writer simply serialises whatever
/// the caller supplies — content interpretation belongs to a layer
/// that knows the grouping-type semantics (per ISO/IEC 14496-12 §8.9).
///
/// Multiple `TrackSampleGroups` entries may target the same
/// `stream_index`; they accumulate in encounter order. The muxer
/// emits all `sgpd` boxes first, then all `sbgp` boxes, after the
/// chunk-offset table inside each track's `stbl`.
#[derive(Clone, Debug, Default)]
pub struct TrackSampleGroups {
    /// Index into the muxer's `streams` slice (the stream slot that
    /// owns these groups).
    pub stream_index: usize,
    /// `sbgp` boxes to emit for this track. Order is preserved.
    pub sbgp: Vec<crate::sample_groups::SampleToGroup>,
    /// `sgpd` boxes to emit for this track. Order is preserved.
    pub sgpd: Vec<crate::sample_groups::SampleGroupDescription>,
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
    /// If `true` (the default), the muxer emits a per-track `edts/elst`
    /// (EditBox/EditListBox, ISO/IEC 14496-12 §8.6.5–6) whenever a track's
    /// first packet has a positive presentation timestamp. The edit list
    /// carries a leading **empty edit** (`media_time = -1`) of that start
    /// delay followed by a normal `media_time = 0` segment for the track's
    /// duration, so a player offsets the track start instead of beginning
    /// at presentation time 0 (the §8.6.5 "An empty edit is used to offset
    /// the start time of a track" idiom).
    ///
    /// Tracks whose first PTS is zero (or absent) get no `edts` — the
    /// implicit one-to-one timeline mapping applies. Set this to `false`
    /// to suppress edit-list emission entirely.
    pub write_edit_list: bool,
    /// Per-track sample-group declarations (`sbgp` + `sgpd`, ISO/IEC
    /// 14496-12 §8.9.2 / §8.9.3). Empty by default — most muxed files
    /// don't need sample groups. When non-empty, each
    /// [`TrackSampleGroups`] entry's `sbgp` / `sgpd` boxes are emitted
    /// into the target track's `stbl` after the chunk-offset table.
    pub track_sample_groups: Vec<TrackSampleGroups>,
    /// Reserve a 64-bit `largesize` header for the `mdat` box so the
    /// media payload may exceed 4 GiB (ISO/IEC 14496-12 §4.2 extended
    /// size form: `size == 1` then an `unsigned int(64) largesize`).
    ///
    /// The plain 32-bit `mdat` header can only describe a box up to
    /// `u32::MAX` bytes; without this flag the muxer errors at
    /// `write_trailer` if the accumulated payload would overflow that.
    /// Because the direct-write path streams `mdat` to the output before
    /// the final size is known, the header form has to be chosen *up
    /// front* — so a producer that expects a >4 GiB `mdat` (long
    /// uncompressed captures, multi-hour high-bitrate masters) sets this
    /// to `true` to reserve the 16-byte largesize header. The 8 extra
    /// bytes are the only cost for files that stay under 4 GiB, so the
    /// default is `false` (compact 32-bit header, byte-identical to the
    /// historical output). `co64` chunk offsets are still chosen
    /// automatically when any chunk offset itself exceeds `u32::MAX`,
    /// independent of this flag.
    pub large_mdat: bool,
}

impl Default for Mp4MuxerOptions {
    fn default() -> Self {
        Self {
            brand: BrandPreset::Mp4,
            faststart: false,
            fragmented: None,
            write_edit_list: true,
            track_sample_groups: Vec::new(),
            large_mdat: false,
        }
    }
}
