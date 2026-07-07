//! Fragmented-MP4 muxer (DASH / HLS / Smooth-Streaming / CMAF output).
//!
//! Layout produced (ISO/IEC 14496-12 §8.8 + DASH-IF Interop):
//!
//! ```text
//! ftyp                       (init segment header)
//! moov
//!   mvhd
//!   trak ... (per stream, with empty stbl sample tables)
//!   mvex
//!     trex (per track — default sample size/duration/flags)
//! styp? + moof + mdat        (one segment per fragment cadence boundary)
//! styp? + moof + mdat
//! ...
//! ```
//!
//! No `mdat` is written before the first fragment cadence boundary; the
//! `moov` advertises empty `stts/stsc/stsz/stco` per the §8.8.1 note that
//! "the sample table boxes specify zero samples for these tracks."
//!
//! Per-fragment, each track's accumulated samples become one `traf` whose
//! `tfhd` carries the `default-base-is-moof` flag (0x020000) so per-sample
//! offsets are relative to the `moof` start — we don't need to know where
//! the `mdat` lands in the file.
//!
//! `trex` defaults are derived per-track from the first packet's duration
//! and (for video) the keyframe-status bit; `tfhd` then uses
//! `default_sample_size` / `default_sample_flags` only when overrides are
//! homogeneous across the whole fragment, falling back to per-sample
//! `trun` entries otherwise.

use std::io::{Seek, SeekFrom, Write};

use oxideav_core::{Error, Muxer, Packet, Result, StreamInfo, WriteSeek};

use crate::muxer::{
    build_mdia, build_mvhd, build_tkhd, default_samples_per_chunk, rescale_to_media_ts, wrap_box,
    TrackState,
};
use crate::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};
use crate::sample_entries::sample_entry_for;

/// A queued `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12 §8.16.5)
/// to emit immediately before the next fragment's `moof`.
///
/// Set via [`FragmentedMuxer::set_next_segment_prft`]; consumed (cleared)
/// when that fragment is flushed, so each call annotates exactly one
/// `moof`. The box relates UTC wall-clock time (`ntp_timestamp`, NTP
/// 64-bit format) to a point on the reference track's media clock
/// (`media_time`), letting a DASH/CMAF client correlate live-edge media
/// time to absolute time without parsing sample data.
#[derive(Clone, Copy, Debug)]
pub struct PrftRequest {
    /// `track_ID` of the reference track this box annotates.
    pub reference_track_id: u32,
    /// UTC time in NTP 64-bit format (high 32 bits = seconds since
    /// 1900-01-01, low 32 bits = fractional seconds).
    pub ntp_timestamp: u64,
    /// The same instant on the reference track's media clock (decode
    /// time, in the track's media timescale). Written as `u32` when it
    /// fits and `force_v1` is false, else as `u64` (box version 1).
    pub media_time: u64,
    /// 24-bit FullBox `flags` (2022-edition annotation bits; `0` for the
    /// 2015 edition). Only the low three bytes are written.
    pub flags: u32,
    /// Force box version 1 (64-bit `media_time`) even when `media_time`
    /// would fit in `u32`. Lets a producer keep a uniform v1 layout
    /// across a stream whose early `media_time`s happen to be small.
    pub force_v1: bool,
}

/// One sample queued in a track's pending fragment.
#[derive(Clone, Debug)]
struct PendingSample {
    data: Vec<u8>,
    duration: u32,
    flags: u32,
    composition_time_offset: i32,
}

/// Per-track running state for the fragmented muxer.
struct FragTrackState {
    /// Underlying `TrackState` from the non-fragmented path. We re-use its
    /// builders for moov subboxes and bookkeeping fields, but the sample
    /// tables stay empty — fragmented files put all samples in moof+mdat.
    base: TrackState,
    /// Track-ID (1-based) used in `tfhd.track_ID`.
    track_id: u32,
    /// CENC routing decision built from the matching
    /// `Mp4MuxerOptions::track_protection` entry at open (ISO/IEC
    /// 23001-7 §4.1). `None` for unprotected tracks. Used to validate
    /// the per-sample `senc` entries handed to
    /// [`FragmentedMuxer::write_protected_packet`] (§9.2 IV-supply
    /// discipline) and to size the per-fragment `saiz` table.
    protection: Option<crate::cenc::CencSchemeDecision>,
    /// Per-sample CENC auxiliary information (§7.1) queued for the next
    /// fragment, parallel to `pending`: either empty (no protected
    /// writes this fragment) or exactly `pending.len()` entries (every
    /// sample of the fragment came through
    /// [`FragmentedMuxer::write_protected_packet`]). Mixing the two
    /// write paths within one fragment is rejected at queue time.
    pending_senc: Vec<crate::cenc::SencSample>,
    /// Per-sample CENC `seig` overrides (ISO/IEC 23001-7 §6) queued for
    /// the next fragment, parallel to `pending_senc`: `Some(entry)` for
    /// a sample whose encryption parameters (typically the KID — key
    /// rotation) override the `tenc` defaults, `None` for a sample on
    /// the defaults. At flush, the distinct entries become one
    /// fragment-local `sgpd` (`grouping_type = 'seig'`, §8.9.3) and the
    /// per-sample mapping one `sbgp` whose fragment-local
    /// group-description indices start at 0x10001 (§8.9.4).
    pending_seig: Vec<Option<crate::cenc::SeigEntry>>,
    /// trex defaults: derived from the first packet's metadata.
    trex_default_sample_duration: u32,
    trex_default_sample_size: u32,
    trex_default_sample_flags: u32,
    /// Set after we've seen the first packet, freezing the trex defaults.
    trex_locked: bool,
    /// Samples queued for the next fragment.
    pending: Vec<PendingSample>,
    /// Cumulative media-timescale ticks across emitted fragments for
    /// `tfdt.base_media_decode_time` of the *next* fragment.
    next_bmdt: u64,
    /// Number of packets seen so far (across all fragments) — used for the
    /// `EveryNPackets` cadence policy.
    packets_total: u64,
    /// Random-access entries collected for this track's `tfra`. One entry
    /// per emitted fragment in which this track contributed at least one
    /// sync sample. All `*_number` fields are 1-based per ISO/IEC 14496-12
    /// §8.8.11.
    tfra_entries: Vec<TfraEmitEntry>,
}

/// One row of an emitted `tfra` table — recorded per fragment as the
/// muxer walks tracks, used at `write_trailer` to build the mfra.
#[derive(Clone, Copy, Debug)]
struct TfraEmitEntry {
    time: u64,
    moof_offset: u64,
    traf_number: u32,
    trun_number: u32,
    sample_number: u32,
}

impl FragTrackState {
    fn new(
        base: TrackState,
        track_id: u32,
        protection: Option<crate::cenc::CencSchemeDecision>,
    ) -> Self {
        Self {
            base,
            track_id,
            protection,
            pending_senc: Vec::new(),
            pending_seig: Vec::new(),
            trex_default_sample_duration: 0,
            trex_default_sample_size: 0,
            trex_default_sample_flags: 0,
            trex_locked: false,
            pending: Vec::new(),
            next_bmdt: 0,
            packets_total: 0,
            tfra_entries: Vec::new(),
        }
    }

    /// Lock in the trex defaults from the first packet seen on this track.
    /// `flags` reflects the keyframe status (sample_is_non_sync bit set
    /// when this packet is a non-key frame).
    fn lock_trex(&mut self, duration: u32, size: u32, flags: u32) {
        if self.trex_locked {
            return;
        }
        self.trex_default_sample_duration = duration;
        self.trex_default_sample_size = size;
        self.trex_default_sample_flags = flags;
        self.trex_locked = true;
    }
}

/// `sample_flags.sample_is_non_sync_sample` — ISO/IEC 14496-12 §8.8.3.1.
const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;
/// `sample_flags.sample_depends_on = 2` (this sample doesn't depend on
/// others) — emitted on keyframes for parser compatibility.
const SAMPLE_DEPENDS_ON_NONE: u32 = 0x0200_0000;

/// Compute the standard `sample_flags` value for one sample. Non-key →
/// sets the non-sync bit; key → clears it and signals "no dependency".
fn sample_flags_for(keyframe: bool) -> u32 {
    if keyframe {
        SAMPLE_DEPENDS_ON_NONE
    } else {
        SAMPLE_IS_NON_SYNC
    }
}

// --- Public entry point --------------------------------------------------

pub(crate) fn open_fragmented(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
    frag_options: FragmentedOptions,
) -> Result<Box<dyn Muxer>> {
    let m = open_fragmented_typed(output, streams, options, frag_options)?;
    Ok(Box::new(m))
}

/// Build a [`FragmentedMuxer`] directly — same construction as
/// [`open_fragmented`] but returns the concrete type so callers can
/// reach the inherent methods (notably
/// [`FragmentedMuxer::write_fragmented_segment_with_styp`]) that aren't
/// part of the [`Muxer`] trait.
///
/// The `Box<dyn Muxer>` form returned by [`open_fragmented`] is what the
/// registry uses; this typed form is for callers driving CMAF / DASH
/// segment emission themselves.
pub fn open_fragmented_typed(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
    frag_options: FragmentedOptions,
) -> Result<FragmentedMuxer> {
    if streams.is_empty() {
        return Err(Error::invalid("mp4 muxer: need at least one stream"));
    }
    let mut tracks = Vec::with_capacity(streams.len());
    for (i, s) in streams.iter().enumerate() {
        let mut entry = sample_entry_for(&s.params)?;
        // ISO/IEC 14496-12 §8.12: wrap the entry into its protected
        // enc* form when a protection directive targets this stream.
        // Keep the typed scheme decision around: write_protected_packet
        // validates each sample's senc entry against it (§9.2) and the
        // flush path sizes the per-fragment saiz from it.
        let mut protection = None;
        if let Some(prot) = options
            .track_protection
            .iter()
            .find(|p| p.stream_index == i)
        {
            entry = crate::sample_entries::apply_protection(entry, s.params.media_type, prot)?;
            protection = Some(crate::cenc::CencSchemeDecision::new(
                crate::cenc::CencScheme::from_fourcc(&prot.scheme_type),
                prot.tenc.clone(),
            )?);
        }
        let mut base = TrackState::new(s.clone(), entry);
        // For fragmented mode every fragment is its own chunk, so the
        // chunking-target field is irrelevant — we only carry it to keep
        // TrackState happy.
        base.samples_per_chunk_target = default_samples_per_chunk(&base.stream);
        tracks.push(FragTrackState::new(base, (i as u32) + 1, protection));
    }
    Ok(FragmentedMuxer {
        output,
        tracks,
        options,
        frag_options,
        sequence_number: 0,
        header_written: false,
        trailer_written: false,
        styp_override: None,
        pending_prft: None,
        pending_moof_pssh: Vec::new(),
    })
}

/// Fragmented-MP4 (DASH / CMAF / HLS-fMP4 / Smooth-Streaming) muxer.
///
/// Constructed via [`open_fragmented_typed`]. Exposes the `Muxer` trait
/// (`write_header` / `write_packet` / `write_trailer`) plus an inherent
/// [`Self::write_fragmented_segment_with_styp`] method for write-side
/// control over the per-segment `styp` brand list (CMAF / DASH segment
/// type marker — ISO/IEC 14496-12 §8.16.2).
pub struct FragmentedMuxer {
    output: Box<dyn WriteSeek>,
    tracks: Vec<FragTrackState>,
    options: Mp4MuxerOptions,
    frag_options: FragmentedOptions,
    /// `mfhd.sequence_number` of the *next* fragment (1-based per spec).
    sequence_number: u32,
    header_written: bool,
    trailer_written: bool,
    /// When `Some`, the next emitted segment's `styp` uses these explicit
    /// `(major_brand, compatible_brands)` values; consumed (cleared) on
    /// the next `flush_fragment`. Set via
    /// [`Self::write_fragmented_segment_with_styp`].
    styp_override: Option<([u8; 4], Vec<[u8; 4]>)>,
    /// When `Some`, the next emitted fragment gets a `prft`
    /// (ProducerReferenceTimeBox) written after any `sidx`/`styp` and
    /// immediately before the `moof`; consumed (cleared) on the next
    /// `flush_fragment`. Set via [`Self::set_next_segment_prft`].
    pending_prft: Option<PrftRequest>,
    /// `pssh` (ProtectionSystemSpecificHeaderBox, ISO/IEC 23001-7
    /// §8.1) boxes to write *inside* the next fragment's `moof`
    /// (§8.1.1 permits `pssh` in `moov` or `moof`); consumed (cleared)
    /// on the next `flush_fragment`. Set via
    /// [`Self::set_next_segment_pssh`]. Used for per-fragment key
    /// rotation where a new licence blob applies only to the samples
    /// in the associated fragment.
    pending_moof_pssh: Vec<crate::cenc::PsshBox>,
}

impl Muxer for FragmentedMuxer {
    fn format_name(&self) -> &str {
        match (&self.options.brand, self.options.fragmented.is_some()) {
            (BrandPreset::Ismv, true) => "ismv",
            (BrandPreset::Mov, _) => "mov",
            _ => "mp4",
        }
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("mp4 muxer: write_header called twice"));
        }
        // ftyp first.
        let ftyp = build_ftyp(&self.options.brand);
        self.output.write_all(&ftyp)?;

        // Then moov with mvex+trex but no media samples (empty stbl
        // tables). trex defaults are zero until the first packet locks
        // them; this is legal — the per-sample fields in `trun` then
        // override the zeroes.
        let moov = build_init_moov(
            &self.tracks,
            &self.frag_options.levels,
            &self.frag_options.treps,
            &self.options.pssh,
        )?;
        self.output.write_all(&moov)?;

        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.queue_packet(packet, None)
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(Error::other("mp4 muxer: write_trailer before write_header"));
        }
        if self.tracks.iter().any(|t| !t.pending.is_empty()) {
            // Final flush: keep the trailing keyframe in THIS fragment.
            // The EveryKeyframe detach exists to make a keyframe open
            // the *next* fragment — at end of stream there is no next
            // fragment, and detaching here would silently drop the
            // sample (it would be replayed into a pending queue nobody
            // flushes again).
            self.flush_fragment_inner(false)?;
        }
        // Random-access trailer (§8.8.10 mfra + §8.8.11 tfra + §8.8.13 mfro).
        // Only emit when at least one fragment carried a sync sample on at
        // least one track; an mfra with all-empty tfras would be useless
        // and wastes ~24 bytes for nothing.
        if self.frag_options.emit_random_access_indexes
            && self.tracks.iter().any(|t| !t.tfra_entries.is_empty())
        {
            self.write_mfra()?;
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

impl FragmentedMuxer {
    /// Write one **protected** sample: the packet's (already-encrypted)
    /// payload plus its ISO/IEC 23001-7 §7.1 sample auxiliary
    /// information — the per-sample `InitializationVector` and optional
    /// subsample map destined for this fragment's `senc`
    /// SampleEncryptionBox (§7.2).
    ///
    /// The stream targeted by `packet.stream_index` must carry a
    /// [`crate::TrackProtection`] directive (the `sinf`-wrapped sample
    /// entry written at `open`); the `senc` entry is validated against
    /// that directive's `tenc`:
    ///
    /// * `initialization_vector.len()` must equal
    ///   `tenc.default_Per_Sample_IV_Size` (§9.2) — or be empty when
    ///   the track uses a constant IV (`Per_Sample_IV_Size == 0`,
    ///   §9.1);
    /// * when a subsample map is supplied, its
    ///   `BytesOfClearData + BytesOfProtectedData` total must equal the
    ///   packet payload length (§9.5.1);
    /// * every sample of a fragment must come through this method, or
    ///   none — mixing with plain [`Muxer::write_packet`] on the same
    ///   track within one fragment would leave `senc.sample_count`
    ///   disagreeing with the §7.2.3 rule ("either zero or the total
    ///   number of samples in the track fragment") and is rejected.
    ///
    /// At each fragment flush the queued entries become one `senc` box
    /// in the track's `traf` plus the matching `saiz` /
    /// `saio` pair (§8.7.8–9; inside a `traf` the single `saio` offset
    /// is relative to the moof start since the muxer always sets
    /// `default-base-is-moof`, per §8.8.14), making every fragment
    /// independently decryptable (the §7.2.1 DASH Media Segment
    /// posture). When the track uses a constant IV **and** no sample
    /// carries a subsample map, the auxiliary information is empty and
    /// all three boxes are omitted per §7.1 ("the sample auxiliary
    /// information would then be empty and should be omitted").
    ///
    /// Encryption itself is the caller's job (e.g. via
    /// [`crate::cenc_cipher::encrypt_sample_in_place`] with the same
    /// IV / subsample map handed here).
    pub fn write_protected_packet(
        &mut self,
        packet: &Packet,
        senc: crate::cenc::SencSample,
    ) -> Result<()> {
        self.queue_packet_protected(packet, senc, None)
    }

    /// Like [`Self::write_protected_packet`] but additionally maps the
    /// sample to a CENC sample group override (ISO/IEC 23001-7 §6 —
    /// `CencSampleEncryptionInformationGroupEntry`, `seig`), the
    /// key-rotation channel: samples whose `seig` names a different
    /// `KID` than `tenc.default_KID` are decrypted with a different
    /// key.
    ///
    /// Pass `Some(entry)` for a sample whose encryption parameters
    /// override the track defaults, `None` for a sample on the
    /// defaults (equivalent to [`Self::write_protected_packet`] —
    /// `sbgp.group_description_index = 0`, "member of no group").
    ///
    /// At each fragment flush, the distinct entries used by the
    /// fragment's samples are deduplicated (in first-use order) into
    /// one **fragment-local** `sgpd` with `grouping_type = 'seig'`
    /// (§8.9.3 — `sgpd` in `traf`) and the per-sample mapping becomes
    /// one `sbgp` whose group-description indices use the §8.9.4
    /// fragment-local numbering (`0x10001` = first local description).
    /// Both boxes are written into the track's `traf`, so each
    /// fragment remains independently decryptable — the same posture
    /// as the per-fragment `senc` (ISO/IEC 23001-7 §6: "For fragmented
    /// files, it may be necessary to store both the Sample To Group
    /// Box and Sample Group Description Box in each track fragment").
    ///
    /// Restrictions, enforced at queue time:
    ///
    /// * the entry itself must satisfy the §6 round-trip rules (see
    ///   [`crate::cenc::build_seig_entry`]);
    /// * a **protected** override (`isProtected == 1`) must keep
    ///   `Per_Sample_IV_Size` equal to the track default — the `senc`
    ///   box stores every sample's IV at one width (§7.2.3), so an
    ///   override changing the width would make the fragment's `senc`
    ///   unparseable.
    pub fn write_protected_packet_grouped(
        &mut self,
        packet: &Packet,
        senc: crate::cenc::SencSample,
        seig: Option<crate::cenc::SeigEntry>,
    ) -> Result<()> {
        self.queue_packet_protected(packet, senc, seig)
    }

    /// Protected-path front half: validates the optional `seig`
    /// override, then joins the shared queue path.
    fn queue_packet_protected(
        &mut self,
        packet: &Packet,
        senc: crate::cenc::SencSample,
        seig: Option<crate::cenc::SeigEntry>,
    ) -> Result<()> {
        if let Some(entry) = &seig {
            let idx = packet.stream_index as usize;
            // §6 round-trip validation (pattern nibbles, IV-size set,
            // constant-IV coherence) — fail here, not at flush.
            crate::cenc::build_seig_entry(entry)?;
            if let Some(track) = self.tracks.get(idx) {
                if let Some(decision) = &track.protection {
                    if entry.is_protected == 1
                        && entry.per_sample_iv_size != decision.tenc.default_per_sample_iv_size
                    {
                        return Err(Error::invalid(format!(
                            "mp4 muxer: stream {idx} seig override changes \
                             Per_Sample_IV_Size from {} to {} — the fragment's senc stores \
                             one IV width for all samples (§7.2.3)",
                            decision.tenc.default_per_sample_iv_size, entry.per_sample_iv_size
                        )));
                    }
                }
            }
        }
        self.queue_packet_impl(packet, Some(senc), seig)
    }

    /// Shared queue path behind [`Muxer::write_packet`] (no CENC
    /// auxiliary info) and the protected-write entry points.
    fn queue_packet(
        &mut self,
        packet: &Packet,
        senc: Option<crate::cenc::SencSample>,
    ) -> Result<()> {
        self.queue_packet_impl(packet, senc, None)
    }

    fn queue_packet_impl(
        &mut self,
        packet: &Packet,
        senc: Option<crate::cenc::SencSample>,
        seig: Option<crate::cenc::SeigEntry>,
    ) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("mp4 muxer: write_header not called"));
        }
        let idx = packet.stream_index as usize;
        if idx >= self.tracks.len() {
            return Err(Error::invalid(format!(
                "mp4 muxer: unknown stream index {idx}"
            )));
        }

        // CENC queue discipline (ISO/IEC 23001-7 §7.2.3 + §9.2).
        match &senc {
            Some(entry) => {
                let track = &self.tracks[idx];
                let decision = track.protection.as_ref().ok_or_else(|| {
                    Error::invalid(format!(
                        "mp4 muxer: write_protected_packet on stream {idx} without a \
                         track_protection directive"
                    ))
                })?;
                // §7.2.3: senc covers all samples of the fragment or
                // none — every previously queued sample of this
                // fragment must have carried an entry too.
                if track.pending_senc.len() != track.pending.len() {
                    return Err(Error::invalid(format!(
                        "mp4 muxer: stream {idx} mixes write_protected_packet with plain \
                         write_packet within one fragment (senc covers all samples or none, \
                         ISO/IEC 23001-7 §7.2.3)"
                    )));
                }
                // §9.2 IV-supply discipline against the track tenc.
                match decision.iv_supply() {
                    crate::cenc::IvSupply::PerSample { size } => {
                        if entry.initialization_vector.len() != size as usize {
                            return Err(Error::invalid(format!(
                                "mp4 muxer: stream {idx} per-sample IV is {} bytes but \
                                 tenc.default_Per_Sample_IV_Size is {size} (§9.2)",
                                entry.initialization_vector.len()
                            )));
                        }
                    }
                    crate::cenc::IvSupply::Constant => {
                        if !entry.initialization_vector.is_empty() {
                            return Err(Error::invalid(format!(
                                "mp4 muxer: stream {idx} uses a constant IV — senc entries \
                                 must not carry per-sample IV bytes (§9.2)"
                            )));
                        }
                    }
                    crate::cenc::IvSupply::None => {
                        return Err(Error::invalid(format!(
                            "mp4 muxer: stream {idx} tenc default is unprotected \
                             (isProtected == 0) — write_protected_packet has no IV context"
                        )));
                    }
                }
                // §9.5.1: subsample totals must cover the sample exactly.
                if !entry.subsamples.is_empty() {
                    let mut total: u64 = 0;
                    for s in &entry.subsamples {
                        total += s.bytes_of_clear_data as u64 + s.bytes_of_protected_data as u64;
                    }
                    if total != packet.data.len() as u64 {
                        return Err(Error::invalid(format!(
                            "mp4 muxer: stream {idx} subsample map covers {total} bytes but \
                             the sample is {} bytes (§9.5.1)",
                            packet.data.len()
                        )));
                    }
                }
            }
            None => {
                // A protected track that already queued senc entries in
                // this fragment cannot fall back to the plain path.
                if !self.tracks[idx].pending_senc.is_empty() {
                    return Err(Error::invalid(format!(
                        "mp4 muxer: stream {idx} mixes plain write_packet with \
                         write_protected_packet within one fragment (senc covers all samples \
                         or none, ISO/IEC 23001-7 §7.2.3)"
                    )));
                }
            }
        }

        // Convert pts/duration to the media timescale.
        let media_ts = self.tracks[idx].base.media_time_scale;
        let dur = if let Some(d) = packet.duration {
            let v = rescale_to_media_ts(d, packet.time_base, media_ts);
            if v > 0 {
                v as u32
            } else {
                1
            }
        } else if let (Some(prev), Some(cur)) = (
            self.tracks[idx].base.prev_pts_in_ts,
            packet
                .pts
                .map(|v| rescale_to_media_ts(v, packet.time_base, media_ts)),
        ) {
            ((cur - prev).max(0) as u32).max(1)
        } else {
            1
        };

        // Composition-time offset = pts - dts (in media timescale). When
        // either is missing, default to 0.
        let cts_off = match (packet.pts, packet.dts) {
            (Some(p), Some(d)) => {
                let pp = rescale_to_media_ts(p, packet.time_base, media_ts);
                let dd = rescale_to_media_ts(d, packet.time_base, media_ts);
                (pp - dd) as i32
            }
            _ => 0,
        };

        let flags = sample_flags_for(packet.flags.keyframe);
        let size = packet.data.len() as u32;
        let track = &mut self.tracks[idx];

        // Lock trex defaults from first packet (any track).
        track.lock_trex(dur, size, flags);

        track.pending.push(PendingSample {
            data: packet.data.clone(),
            duration: dur,
            flags,
            composition_time_offset: cts_off,
        });
        if let Some(entry) = senc {
            track.pending_senc.push(entry);
            // pending_seig stays parallel to pending_senc: every
            // protected sample records its (possibly absent) group
            // override so the flush can run-length-encode the sbgp.
            track.pending_seig.push(seig);
        }

        // Update bookkeeping for delta computation on subsequent packets.
        let pts_in_ts = packet
            .pts
            .map(|v| rescale_to_media_ts(v, packet.time_base, media_ts));
        if let Some(p) = pts_in_ts {
            if track.base.first_pts_in_ts.is_none() {
                track.base.first_pts_in_ts = Some(p);
            }
            track.base.prev_pts_in_ts = Some(p);
        } else {
            let base = track.base.prev_pts_in_ts.unwrap_or(0);
            track.base.prev_pts_in_ts = Some(base + dur as i64);
            if track.base.first_pts_in_ts.is_none() {
                track.base.first_pts_in_ts = Some(0);
            }
        }
        track.base.cumulative_duration += dur as u64;
        track.packets_total += 1;

        // Maybe emit a fragment now.
        if self.should_flush(idx, packet.flags.keyframe) {
            self.flush_fragment()?;
        }
        Ok(())
    }

    /// Mark the next emitted fragment's `styp` (ISO/IEC 14496-12 §8.16.2
    /// Segment Type Box) to use the given `(major_brand, compat_brands)`,
    /// overriding the `Mp4MuxerOptions::fragmented::styp` preset for one
    /// segment.
    ///
    /// Use this when driving DASH/CMAF segment emission segment-by-segment
    /// from your own code — e.g. to switch from an init-segment-style
    /// `styp(msdh)` to an intermediate `styp(msix)` (indexed media
    /// segment) or `styp(cmfs)` (CMAF segment) without rebuilding the
    /// muxer. The override is consumed (cleared) when the next fragment
    /// is flushed; subsequent fragments fall back to the configured
    /// preset.
    ///
    /// Per §8.16.2 the box is structurally identical to `ftyp` (§4.3):
    /// 4-byte `major_brand`, 4-byte `minor_version` (always 0 here),
    /// then a run of 4-byte `compatible_brands` to end of box. Empty
    /// `compat_brands` is legal — §4.3 (inherited by §8.16.2) permits a
    /// zero-length list. See [`crate::styp::build_styp`] for the
    /// stateless byte-builder this method threads through.
    ///
    /// Errors only when [`Self::has_random_access_indexes_enabled`] is
    /// false **and** the configured `frag_options.styp` is also `None`
    /// **and** the writer hasn't been told to emit a styp — i.e. when
    /// emitting a styp would still leave the segment without one of the
    /// surrounding boxes the override implies. Today the method never
    /// errors: it merely records the override and the next flush emits
    /// the styp unconditionally.
    pub fn write_fragmented_segment_with_styp(
        &mut self,
        major_brand: [u8; 4],
        compat_brands: &[[u8; 4]],
    ) {
        self.styp_override = Some((major_brand, compat_brands.to_vec()));
    }

    /// Queue a `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12
    /// §8.16.5) to be written immediately before the *next* fragment's
    /// `moof`.
    ///
    /// `prft` relates UTC wall-clock time (`ntp_timestamp`, in NTP
    /// 64-bit 32.32 fixed-point format — high 32 bits seconds since
    /// 1900-01-01, low 32 bits fractional seconds) to a point on the
    /// reference track's media clock (`media_time`, in that track's media
    /// timescale, decode time). A DASH/CMAF client uses it to map the
    /// live-edge media time back to absolute time without parsing sample
    /// data.
    ///
    /// Per §8.16.5 the box relates to the next `moof` in bitstream order
    /// and must follow any `styp`/`sidx`; this method records the request
    /// and the next `flush_fragment` emits it in that position. The
    /// request is consumed (cleared) once that fragment is flushed, so
    /// each call annotates exactly one fragment. Calling it again before
    /// the next flush replaces the pending request.
    ///
    /// The box version is chosen automatically: version 0 (`u32`
    /// `media_time`) when `media_time` fits in 32 bits, version 1 (`u64`)
    /// otherwise. `flags` carries the 2022-edition annotation bits (pass
    /// `0` for the 2015-edition layout); only the low three bytes are
    /// written into the 24-bit FullBox `flags` field.
    pub fn set_next_segment_prft(
        &mut self,
        reference_track_id: u32,
        ntp_timestamp: u64,
        media_time: u64,
        flags: u32,
    ) {
        self.pending_prft = Some(PrftRequest {
            reference_track_id,
            ntp_timestamp,
            media_time,
            flags,
            force_v1: false,
        });
    }

    /// Like [`Self::set_next_segment_prft`] but forces box version 1
    /// (64-bit `media_time`) regardless of the magnitude of
    /// `media_time`. Use this to keep a uniform v1 layout across a stream
    /// whose early `media_time` values happen to fit in `u32`.
    pub fn set_next_segment_prft_v1(
        &mut self,
        reference_track_id: u32,
        ntp_timestamp: u64,
        media_time: u64,
        flags: u32,
    ) {
        self.pending_prft = Some(PrftRequest {
            reference_track_id,
            ntp_timestamp,
            media_time,
            flags,
            force_v1: true,
        });
    }

    /// Queue one or more `pssh` (ProtectionSystemSpecificHeaderBox,
    /// ISO/IEC 23001-7 §8.1) boxes to be written *inside* the next
    /// fragment's `moof` (§8.1.1 explicitly permits `pssh` in a `moov`
    /// **or** a `moof`).
    ///
    /// This is the movie-fragment counterpart to the moov-level
    /// `Mp4MuxerOptions::pssh` init-segment boxes: use it for
    /// per-fragment key rotation, where a fresh licence blob (or a new
    /// KID set) applies only to the samples in the associated fragment.
    /// A §8.1.1-conformant reader examines the `pssh` boxes in the
    /// `moov` and in the `moof` associated with a sample (but not those
    /// in other fragments), so a box queued here scopes exactly to the
    /// next fragment.
    ///
    /// The boxes are written as the first children of the `moof` body
    /// (before `mfhd`? — no: after `mfhd`, before the `traf` boxes, so
    /// the fragment header still leads), serialised through
    /// `cenc::build_pssh_box`. The request is consumed (cleared) once
    /// that fragment is flushed; calling it again before the next flush
    /// appends to the pending set. A record that would not round-trip
    /// (a v0 box carrying KIDs, oversize counts) surfaces its error at
    /// the next `write_packet` / `write_trailer` that triggers the
    /// flush, not here.
    pub fn set_next_segment_pssh(&mut self, pssh: impl IntoIterator<Item = crate::cenc::PsshBox>) {
        self.pending_moof_pssh.extend(pssh);
    }

    /// Return true when the cadence policy says it's time to emit a
    /// fragment after the current packet.
    ///
    /// `current_track_idx` is the track that just received a packet;
    /// `is_keyframe` is its keyframe flag. The cadence is anchored to the
    /// first track (index 0) — typically video — except `EveryNPackets`
    /// and `EveryKeyframe` which fire on the matching event of any track.
    fn should_flush(&self, current_track_idx: usize, is_keyframe: bool) -> bool {
        match self.frag_options.cadence {
            FragmentCadence::EverySeconds(secs) => {
                // Anchor on track 0 (video) or single track. Fire when the
                // running pending duration of the anchor track reaches
                // `secs` seconds in its own media timescale.
                let anchor = 0usize;
                if self.tracks[anchor].pending.is_empty() {
                    return false;
                }
                let media_ts = self.tracks[anchor].base.media_time_scale as f64;
                let pending_ticks: u64 = self.tracks[anchor]
                    .pending
                    .iter()
                    .map(|s| s.duration as u64)
                    .sum();
                (pending_ticks as f64 / media_ts) >= secs
            }
            FragmentCadence::EveryKeyframe => {
                // Fire on the *next* keyframe AFTER we already have at
                // least one sample on the anchor track. The new keyframe
                // becomes the first sample of the next fragment, so we
                // flush BEFORE pushing it. (This method is consulted
                // AFTER the push, so we instead flush when we have ≥ 2
                // samples and the just-pushed one is a keyframe.)
                let anchor = 0usize;
                if current_track_idx != anchor {
                    return false;
                }
                if !is_keyframe {
                    return false;
                }
                self.tracks[anchor].pending.len() >= 2
            }
            FragmentCadence::EveryNPackets(n) => {
                if n == 0 {
                    return false;
                }
                let anchor = 0usize;
                if current_track_idx != anchor {
                    return false;
                }
                self.tracks[anchor].pending.len() as u32 >= n
            }
        }
    }

    /// Emit one `styp? + moof + mdat` triple. For `EveryKeyframe` the
    /// just-pushed keyframe is detached from `pending` and re-pushed after
    /// the flush — see `should_flush` doc.
    fn flush_fragment(&mut self) -> Result<()> {
        self.flush_fragment_inner(true)
    }

    /// `flush_fragment` body. `detach_trailing_keyframe` is `true` on
    /// cadence-driven flushes (the just-pushed keyframe opens the next
    /// fragment) and `false` on the final `write_trailer` flush, where
    /// detaching would drop the sample — there is no next fragment to
    /// replay it into.
    fn flush_fragment_inner(&mut self, detach_trailing_keyframe: bool) -> Result<()> {
        // For EveryKeyframe semantics: the just-pushed keyframe needs to
        // be the *first* sample of the *next* fragment, not the last of
        // the current one. Detach and replay after. A CENC senc entry
        // (and its seig override) queued alongside the sample travels
        // with it.
        type Detached = (
            usize,
            PendingSample,
            Option<(crate::cenc::SencSample, Option<crate::cenc::SeigEntry>)>,
        );
        let mut detached: Vec<Detached> = Vec::new();
        if detach_trailing_keyframe
            && matches!(self.frag_options.cadence, FragmentCadence::EveryKeyframe)
        {
            let anchor = 0usize;
            if let Some(last) = self.tracks[anchor].pending.last() {
                if last.flags & SAMPLE_IS_NON_SYNC == 0 {
                    let s = self.tracks[anchor].pending.pop().unwrap();
                    let cenc = if self.tracks[anchor].pending_senc.len()
                        > self.tracks[anchor].pending.len()
                    {
                        let senc = self.tracks[anchor].pending_senc.pop().unwrap();
                        let seig = self.tracks[anchor].pending_seig.pop().flatten();
                        Some((senc, seig))
                    } else {
                        None
                    };
                    // Roll back cumulative_duration so the next fragment
                    // re-accounts for this keyframe.
                    self.tracks[anchor].base.cumulative_duration = self.tracks[anchor]
                        .base
                        .cumulative_duration
                        .saturating_sub(s.duration as u64);
                    detached.push((anchor, s, cenc));
                }
            }
        }

        // If after detaching everything is empty, nothing to flush.
        if self.tracks.iter().all(|t| t.pending.is_empty()) {
            // Replay any detached samples into the next fragment.
            for (idx, s, cenc) in detached {
                self.tracks[idx].base.cumulative_duration += s.duration as u64;
                self.tracks[idx].pending.push(s);
                if let Some((senc, seig)) = cenc {
                    self.tracks[idx].pending_senc.push(senc);
                    self.tracks[idx].pending_seig.push(seig);
                }
            }
            return Ok(());
        }

        self.sequence_number += 1;
        let seq = self.sequence_number;

        // Build moof (with any per-fragment moof-level pssh boxes, §8.1.1).
        let moof_pssh = std::mem::take(&mut self.pending_moof_pssh);
        let moof = build_moof(seq, &self.tracks, &moof_pssh)?;
        let moof_size = moof.len() as u64;

        // Build mdat: concatenate per-track sample bytes in the same
        // order trun walks them (track-by-track).
        let mut mdat_payload: Vec<u8> = Vec::new();
        for t in &self.tracks {
            for s in &t.pending {
                mdat_payload.extend_from_slice(&s.data);
            }
        }
        let mdat = wrap_box(b"mdat", &mdat_payload);
        let mdat_size = mdat.len() as u64;

        // Optional sidx (one-reference, covers this fragment only). Per
        // ISO/IEC 14496-12 §8.16.3 the `first_offset` is relative to the
        // first byte AFTER the sidx box; we write the sidx immediately
        // before styp+moof+mdat so the next-byte anchor is exactly where
        // the optional styp lands. The single referenced subsegment is
        // `styp? + moof + mdat`, sized accordingly. `first_offset = 0`.
        if self.frag_options.emit_random_access_indexes {
            let styp_size: u64 = if let Some((major, compat)) = &self.styp_override {
                crate::styp::build_styp(*major, compat).len() as u64
            } else {
                self.frag_options
                    .styp
                    .as_ref()
                    .map(|b| build_styp(b).len() as u64)
                    .unwrap_or(0)
            };
            // A queued `prft` is written inside this subsegment (between
            // styp and moof), so its size counts toward the sidx
            // referenced_size — otherwise a byte-range fetch driven by the
            // sidx would fall short of the moof.
            let prft_size: u64 = self
                .pending_prft
                .as_ref()
                .map(|p| build_prft(p).len() as u64)
                .unwrap_or(0);
            let subsegment_size = styp_size + prft_size + moof_size + mdat_size;
            // EPT for this fragment on the anchor (first contributing) track
            // is the bmdt the track entered the fragment with.
            let anchor_idx = self
                .tracks
                .iter()
                .position(|t| !t.pending.is_empty())
                .unwrap_or(0);
            let ept = self.tracks[anchor_idx].next_bmdt;
            let timescale = self.tracks[anchor_idx].base.media_time_scale;
            let frag_dur_anchor: u64 = self.tracks[anchor_idx]
                .pending
                .iter()
                .map(|s| s.duration as u64)
                .sum();
            // Per §8.16.3 subsegment_duration is u32; clamp very long
            // fragments (defensive — single fragments are typically <30 s).
            let subseg_dur_u32 = frag_dur_anchor.min(u32::MAX as u64) as u32;
            // starts_with_SAP iff the anchor track's first pending sample
            // is a sync sample (SAP type 1 = closed-GOP IDR).
            let starts_sap = self.tracks[anchor_idx]
                .pending
                .first()
                .map(|s| s.flags & SAMPLE_IS_NON_SYNC == 0)
                .unwrap_or(false);
            let sidx = build_sidx(
                self.tracks[anchor_idx].track_id,
                timescale,
                ept,
                subsegment_size,
                subseg_dur_u32,
                starts_sap,
            );
            self.output.write_all(&sidx)?;

            // Optional `ssix` (SubsegmentIndexBox §8.16.4) documenting the
            // `sidx` just written. It must immediately follow that `sidx`
            // (§8.16.4.1) and carry `subsegment_count == reference_count`
            // (1 here). The single subsegment is partitioned into two
            // contiguous level ranges that together cover every byte:
            //   range 1 = styp? + prft? + moof  (metadata level),
            //   range 2 = mdat                  (media level).
            // `range_size` is a 24-bit field; an mdat (or metadata run)
            // larger than 16 MiB is rejected by `build_ssix_box` rather
            // than silently truncated.
            if self.frag_options.emit_ssix {
                let (meta_level, media_level) = self.frag_options.ssix_levels;
                let meta_size = styp_size + prft_size + moof_size;
                // Guard the u64 → u32 narrowing before build_ssix_box's
                // 24-bit check, so a >4 GiB range surfaces an error rather
                // than wrapping. (Fragment subsegments are far smaller in
                // practice; this is purely defensive.)
                if meta_size > u32::MAX as u64 || mdat_size > u32::MAX as u64 {
                    return Err(Error::invalid("MP4: ssix subsegment range exceeds 32 bits"));
                }
                let record = crate::demux::SsixRecord {
                    subsegments: vec![crate::demux::SsixSubsegment {
                        ranges: vec![
                            crate::demux::SsixRange {
                                level: meta_level,
                                range_size: meta_size as u32,
                            },
                            crate::demux::SsixRange {
                                level: media_level,
                                range_size: mdat_size as u32,
                            },
                        ],
                    }],
                };
                let ssix = crate::demux::build_ssix_box(&record)?;
                self.output.write_all(&ssix)?;
            }
        }

        // Optional styp. A per-segment override set via
        // `write_fragmented_segment_with_styp` wins over `frag_options.styp`
        // for this single segment, then is cleared so subsequent segments
        // fall back to the configured preset (or no styp at all).
        if let Some((major, compat)) = self.styp_override.take() {
            crate::styp::write_styp(&mut self.output, major, &compat)?;
        } else if let Some(brand) = &self.frag_options.styp {
            let styp = build_styp(brand);
            self.output.write_all(&styp)?;
        }

        // Optional `prft` (ProducerReferenceTimeBox §8.16.5). Per spec it
        // relates to the *next* moof and must follow any styp/sidx, so it
        // is written here, immediately before the moof. Consumed (cleared)
        // so each request annotates exactly one fragment.
        if let Some(prft) = self.pending_prft.take() {
            let bytes = build_prft(&prft);
            self.output.write_all(&bytes)?;
        }

        // Record the absolute byte offset of the moof BEFORE writing it —
        // this is what `tfra.moof_offset` and DASH random-access seekers
        // need. `stream_position` is reliable here because all writes so
        // far have been forward-streaming.
        let moof_offset = self.output.stream_position()?;

        // Patch the trun.data_offset values inside `moof`. We computed
        // offsets relative to moof start during build_moof; now the moof
        // bytes are final, so the offsets simply need (moof_size + 8) added
        // (8 = mdat header). That's already what build_moof does — see
        // its TrackFragData::trun_data_offset_in_moof field — so we just
        // verify the moof-local layout produces correct offsets.
        self.output.write_all(&moof)?;
        self.output.write_all(&mdat)?;

        // For each track, record one tfra entry per sync sample in this
        // fragment. With one traf per track per moof and one trun per
        // traf, traf/trun numbers are always 1. Most fragments have
        // exactly one keyframe (the first sample) so this is usually
        // a single entry per track.
        for t in self.tracks.iter_mut() {
            if t.pending.is_empty() {
                continue;
            }
            let mut dts_in_frag: u64 = 0;
            for (k, s) in t.pending.iter().enumerate() {
                let is_sync = s.flags & SAMPLE_IS_NON_SYNC == 0;
                if is_sync {
                    t.tfra_entries.push(TfraEmitEntry {
                        time: t.next_bmdt + dts_in_frag,
                        moof_offset,
                        traf_number: 1,
                        trun_number: 1,
                        sample_number: (k as u32) + 1,
                    });
                }
                dts_in_frag += s.duration as u64;
            }
        }

        // Advance bmdt for next fragment + drain pending + replay
        // detached.
        for t in self.tracks.iter_mut() {
            let frag_dur: u64 = t.pending.iter().map(|s| s.duration as u64).sum();
            t.next_bmdt += frag_dur;
            t.pending.clear();
            t.pending_senc.clear();
            t.pending_seig.clear();
        }
        for (idx, s, cenc) in detached {
            self.tracks[idx].base.cumulative_duration += s.duration as u64;
            self.tracks[idx].pending.push(s);
            if let Some((senc, seig)) = cenc {
                self.tracks[idx].pending_senc.push(senc);
                self.tracks[idx].pending_seig.push(seig);
            }
        }

        let _ = moof_size;
        Ok(())
    }

    /// Write the random-access trailer: one `mfra` containing per-track
    /// `tfra` tables + an `mfro` size trailer (ISO/IEC 14496-12 §8.8.10–13).
    /// `mfro.size` is the total size of the `mfra` box including its own
    /// 8-byte header — players read mfro by reading the last 16 bytes of
    /// the file (8-byte mfro header + 4 ver/flags + 4 size) to learn
    /// where to seek for the random-access index.
    fn write_mfra(&mut self) -> Result<()> {
        let mut mfra_body: Vec<u8> = Vec::new();
        for t in &self.tracks {
            if t.tfra_entries.is_empty() {
                continue;
            }
            mfra_body.extend_from_slice(&build_tfra(t.track_id, &t.tfra_entries));
        }
        // mfro is part of the mfra body (it's a child box of mfra). Per
        // §8.8.13 its `size` field equals the total size of the enclosing
        // mfra box (header + body + mfro itself). mfro itself is 16 bytes:
        // 8 box header + 4 version/flags + 4 size.
        let mfra_total_size: u64 = 8 + mfra_body.len() as u64 + 16;
        let mfro = build_mfro(mfra_total_size as u32);
        mfra_body.extend_from_slice(&mfro);
        let mfra = wrap_box(b"mfra", &mfra_body);
        debug_assert_eq!(mfra.len() as u64, mfra_total_size);
        self.output.seek(SeekFrom::End(0))?;
        self.output.write_all(&mfra)?;
        Ok(())
    }
}

// --- Init segment builders ------------------------------------------------

fn build_ftyp(brand: &BrandPreset) -> Vec<u8> {
    let major = brand.major_brand();
    let compat = brand.compatible_brands();
    let mut body = Vec::with_capacity(8 + 4 * compat.len());
    body.extend_from_slice(&major);
    let minor: u32 = match brand {
        BrandPreset::Mp4 => 0x0000_0200,
        _ => 0,
    };
    body.extend_from_slice(&minor.to_be_bytes());
    for b in &compat {
        body.extend_from_slice(b);
    }
    wrap_box(b"ftyp", &body)
}

fn build_styp(brand: &BrandPreset) -> Vec<u8> {
    // styp has the same shape as ftyp (ISO/IEC 14496-12 §8.16.2). Major
    // brand carries a CMAF / DASH segment-type code (msdh / msix / cmfs / …).
    let major = brand.major_brand();
    let compat = brand.compatible_brands();
    let mut body = Vec::with_capacity(8 + 4 * compat.len());
    body.extend_from_slice(&major);
    body.extend_from_slice(&0u32.to_be_bytes()); // minor_version
    for b in &compat {
        body.extend_from_slice(b);
    }
    wrap_box(b"styp", &body)
}

fn build_init_moov(
    tracks: &[FragTrackState],
    levels: &[crate::demux::LevaEntry],
    treps: &[crate::demux::TrepRecord],
    pssh: &[crate::cenc::PsshBox],
) -> Result<Vec<u8>> {
    // Movie timescale: pick 1000 (matches the non-fragmented path).
    let movie_timescale: u32 = 1000;

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&build_mvhd(movie_timescale, 0, (tracks.len() as u32) + 1));
    for t in tracks {
        moov_body.extend_from_slice(&build_trak_init(t.track_id, &t.base, movie_timescale)?);
    }
    moov_body.extend_from_slice(&build_mvex(tracks, levels, treps)?);
    // ISO/IEC 23001-7 §8.1: moov-level pssh boxes, one per DRM system,
    // after the trak boxes + mvex.
    for record in pssh {
        moov_body.extend_from_slice(&crate::cenc::build_pssh_box(record)?);
    }
    Ok(wrap_box(b"moov", &moov_body))
}

fn build_trak_init(track_id: u32, t: &TrackState, movie_timescale: u32) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    // Duration is unknown at init time — use 0 so players read tfhd/trun
    // for actual timing. Per §8.2.2 a zero duration means "indefinite".
    body.extend_from_slice(&build_tkhd(track_id, 0, &t.stream));
    // mdia uses the same builder as the non-fragmented path; sample
    // tables will be empty (the moov has no samples).
    body.extend_from_slice(&build_mdia(t)?);
    let _ = movie_timescale;
    Ok(wrap_box(b"trak", &body))
}

/// `mvex` (§8.8.1) container holding `trex` per track + an optional
/// `mehd` (movie-extends header) carrying overall fragment duration —
/// we omit `mehd` since fragment durations are unknown at init time.
///
/// When `levels` is non-empty, a `leva` (LevelAssignmentBox, §8.8.13) is
/// appended after the `trex` boxes to declare how the file is partitioned
/// into levels for partial-subsegment fetch (the level numbers a sibling
/// `ssix` refers to). The `leva` is omitted entirely when no levels are
/// configured, keeping the byte-identical default `mvex`.
///
/// When `treps` is non-empty, one `trep` (TrackExtensionPropertiesBox,
/// §8.8.15) per record is appended after the `leva` (in slice order),
/// each documenting characteristics of one track in the subsequent
/// fragments (e.g. an `assp` AlternativeStartupSequencePropertiesBox,
/// §8.8.16). The `trep` boxes are omitted entirely when `treps` is empty.
fn build_mvex(
    tracks: &[FragTrackState],
    levels: &[crate::demux::LevaEntry],
    treps: &[crate::demux::TrepRecord],
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    for t in tracks {
        body.extend_from_slice(&build_trex(
            t.track_id,
            t.trex_default_sample_duration,
            t.trex_default_sample_size,
            t.trex_default_sample_flags,
        ));
    }
    if !levels.is_empty() {
        let record = crate::demux::LevaRecord {
            entries: levels.to_vec(),
        };
        body.extend_from_slice(&crate::demux::build_leva_box(&record)?);
    }
    for trep in treps {
        body.extend_from_slice(&crate::demux::build_trep_box(trep)?);
    }
    Ok(wrap_box(b"mvex", &body))
}

/// §8.8.3 `trex` — TrackExtendsBox.
/// FullBox(version=0, flags=0) + track_ID(u32) + DSDI(u32) +
/// default_sample_duration(u32) + default_sample_size(u32) +
/// default_sample_flags(u32). 24-byte payload.
fn build_trex(track_id: u32, ddur: u32, dsiz: u32, dflg: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(24);
    body.extend_from_slice(&[0u8; 4]); // version + flags
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    body.extend_from_slice(&ddur.to_be_bytes());
    body.extend_from_slice(&dsiz.to_be_bytes());
    body.extend_from_slice(&dflg.to_be_bytes());
    wrap_box(b"trex", &body)
}

// --- Per-fragment moof builder -------------------------------------------

/// `moof` for one fragment. Order: `mfhd` then one `traf` per track that
/// has pending samples.
///
/// The `trun.data_offset` is the offset of the *first* sample of that
/// trun, relative to the *start of the enclosing moof box* (because
/// `tfhd.default-base-is-moof` is set). We compute it as
/// `moof_size + 8 + cumulative_byte_offset_in_mdat` once the moof's own
/// size is known. To break the circular dependency we build the moof
/// twice: first with placeholder offsets to size it, then with real
/// offsets. Since the moof's size doesn't depend on the offset *values*
/// (they're fixed-width i32), the two passes always produce the same
/// total length.
fn build_moof(
    seq: u32,
    tracks: &[FragTrackState],
    pssh: &[crate::cenc::PsshBox],
) -> Result<Vec<u8>> {
    // Pass 1: build with data_offset = 0 to learn the moof size.
    let placeholder = build_moof_inner(seq, tracks, pssh, |_track_idx, _byte_in_mdat| 0)?;
    let moof_size = placeholder.len() as u64;
    let mdat_header_size: u64 = 8;
    // Pass 2: real offsets relative to start of moof.
    let final_moof = build_moof_inner(seq, tracks, pssh, |_track_idx, byte_in_mdat| {
        (moof_size + mdat_header_size + byte_in_mdat) as i32
    })?;
    debug_assert_eq!(final_moof.len() as u64, moof_size, "moof size shifted");
    Ok(final_moof)
}

fn build_moof_inner<F>(
    seq: u32,
    tracks: &[FragTrackState],
    pssh: &[crate::cenc::PsshBox],
    offset_fn: F,
) -> Result<Vec<u8>>
where
    F: Fn(usize, u64) -> i32,
{
    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&build_mfhd(seq));
    // ISO/IEC 23001-7 §8.1.1: moof-level pssh boxes sit after the
    // fragment header and before the traf boxes, scoping their DRM
    // header to this fragment's samples only.
    for record in pssh {
        moof_body.extend_from_slice(&crate::cenc::build_pssh_box(record)?);
    }

    // Walk tracks, accumulating per-track byte offsets within mdat.
    let mut byte_in_mdat: u64 = 0;
    for (i, t) in tracks.iter().enumerate() {
        if t.pending.is_empty() {
            continue;
        }
        let track_first_byte = byte_in_mdat;
        let trun_data_offset = offset_fn(i, track_first_byte);
        let traf = build_traf(t, trun_data_offset)?;
        let traf_pos_in_body = moof_body.len();
        moof_body.extend_from_slice(&traf.bytes);
        // Patch this traf's single `saio` offset now that the traf's
        // position within the moof is known. With
        // `default-base-is-moof` set, the offset is relative to the
        // first byte of the enclosing moof box (§8.8.14), i.e. the
        // 8-byte moof header plus the traf's position in the body plus
        // the aux-data position inside the traf. Identical in both
        // build passes (the moof layout doesn't depend on the trun
        // data-offset values), so patching per pass is safe.
        if let Some(patch) = traf.saio_patch {
            let absolute = 8u64 + traf_pos_in_body as u64 + patch.aux_data_pos as u64;
            let value = u32::try_from(absolute).map_err(|_| {
                Error::invalid("MP4: saio offset exceeds u32 (moof too large for v0 saio)")
            })?;
            let field = traf_pos_in_body + patch.field_pos;
            moof_body[field..field + 4].copy_from_slice(&value.to_be_bytes());
        }
        // Advance byte pointer past this track's samples.
        for s in &t.pending {
            byte_in_mdat += s.data.len() as u64;
        }
    }
    Ok(wrap_box(b"moof", &moof_body))
}

/// Byte positions (relative to the start of the traf box, i.e.
/// including its 8-byte header) that [`build_moof_inner`] needs to
/// finalise a traf's `saio` once the traf's position within the moof
/// is known.
struct SaioPatch {
    /// Position of the 4-byte v0 `saio` offset field.
    field_pos: usize,
    /// Position of the first CENC auxiliary-information byte — the
    /// first `senc` entry, right after the senc box's `sample_count`.
    aux_data_pos: usize,
}

/// A built `traf` plus the optional `saio` patch request.
struct TrafBuild {
    bytes: Vec<u8>,
    saio_patch: Option<SaioPatch>,
}

/// §8.8.5 `mfhd` — MovieFragmentHeaderBox.
/// FullBox(0,0) + sequence_number(u32). 8-byte payload.
fn build_mfhd(seq: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&[0u8; 4]); // version + flags
    body.extend_from_slice(&seq.to_be_bytes());
    wrap_box(b"mfhd", &body)
}

/// `traf` for one track: tfhd + tfdt + [senc + saiz + saio] + trun.
///
/// The CENC triple is present only when the fragment's samples were
/// queued through [`FragmentedMuxer::write_protected_packet`] and the
/// per-sample auxiliary information is non-empty (ISO/IEC 23001-7
/// §7.1 — a constant-IV track with no subsample maps has empty aux
/// info, and all three boxes are omitted).
fn build_traf(t: &FragTrackState, trun_data_offset: i32) -> Result<TrafBuild> {
    let defaults = FragmentDefaults::for_track(t);
    let mut body = Vec::new();
    body.extend_from_slice(&build_tfhd(t, defaults));
    body.extend_from_slice(&build_tfdt(t.next_bmdt));
    let saio_patch = append_traf_cenc_boxes(t, &mut body)?;
    append_traf_seig_groups(t, &mut body)?;
    body.extend_from_slice(&build_trun(t, trun_data_offset, defaults));
    Ok(TrafBuild {
        bytes: wrap_box(b"traf", &body),
        // `body` positions shift by the 8-byte traf box header once
        // wrapped.
        saio_patch: saio_patch.map(|p| SaioPatch {
            field_pos: p.field_pos + 8,
            aux_data_pos: p.aux_data_pos + 8,
        }),
    })
}

/// Append the per-fragment CENC boxes — `senc` (ISO/IEC 23001-7 §7.2)
/// plus the matching `saiz` / `saio` pair (ISO/IEC 14496-12 §8.7.8–9)
/// — to a traf body under construction. Returns the [`SaioPatch`]
/// positions (relative to the traf *body*) when the boxes were
/// emitted.
///
/// Layout decisions, per spec:
///
/// * `senc.flags` gets the `UseSubSampleEncryption` bit (0x2) iff any
///   sample carries a subsample map (§7.2.3);
/// * `saiz` / `saio` omit the optional `aux_info_type` /
///   `aux_info_type_parameter` pair — §7.1: for CENC-protected tracks
///   the defaults are the scheme FourCC and 0, "so content SHOULD be
///   created omitting these optional fields";
/// * `saiz.default_sample_info_size` is used when every sample's aux
///   info has one size (the §7.1 NOTE), else the per-sample table;
/// * `saio` carries a single offset (the aux info for all the track's
///   runs is contiguous inside `senc`, §8.8.14) pointing at the first
///   senc entry; the field is patched to its moof-relative value by
///   [`build_moof_inner`];
/// * empty aux info (constant IV + no subsamples) emits nothing (§7.1).
fn append_traf_cenc_boxes(t: &FragTrackState, body: &mut Vec<u8>) -> Result<Option<SaioPatch>> {
    if t.pending_senc.is_empty() {
        return Ok(None);
    }
    if t.pending_senc.len() != t.pending.len() {
        // queue_packet enforces this; re-checked so a future internal
        // caller can't desynchronise the two queues silently.
        return Err(Error::invalid(
            "MP4: pending senc entries out of step with pending samples",
        ));
    }
    let any_subsamples = t.pending_senc.iter().any(|s| !s.subsamples.is_empty());
    let iv_size = t
        .pending_senc
        .first()
        .map(|s| s.initialization_vector.len())
        .unwrap_or(0);
    if iv_size == 0 && !any_subsamples {
        // §7.1: constant-IV track without subsample maps — the sample
        // auxiliary information is empty "and should be omitted".
        return Ok(None);
    }

    // Per-sample auxiliary-information sizes (§7.1
    // CencSampleAuxiliaryDataFormat): IV bytes, plus — only under
    // subsample encryption — the u16 subsample_count and 6 bytes per
    // (clear, protected) pair.
    let mut sizes: Vec<u64> = Vec::with_capacity(t.pending_senc.len());
    for s in &t.pending_senc {
        let mut sz = s.initialization_vector.len() as u64;
        if any_subsamples {
            sz += 2 + 6 * s.subsamples.len() as u64;
        }
        sizes.push(sz);
    }
    for (i, &sz) in sizes.iter().enumerate() {
        if sz > u8::MAX as u64 {
            return Err(Error::invalid(format!(
                "MP4: sample {i} CENC auxiliary info is {sz} bytes — exceeds the 8-bit \
                 saiz sample_info_size field (§8.7.8.3)"
            )));
        }
    }

    let senc = crate::cenc::SencBox {
        flags: if any_subsamples { 0x0000_0002 } else { 0 },
        samples: t.pending_senc.clone(),
    };
    let senc_bytes = crate::cenc::build_senc_box(&senc)?;
    let senc_pos = body.len();
    body.extend_from_slice(&senc_bytes);
    // First aux-info byte: senc box header (8) + FullBox (4) +
    // sample_count (4).
    let aux_data_pos = senc_pos + 16;

    // saiz — constant-size shortcut when all samples agree.
    let all_same = sizes.windows(2).all(|w| w[0] == w[1]);
    let saiz_record = crate::demux::SaizBox {
        aux_info_type: None,
        aux_info_type_parameter: None,
        default_sample_info_size: if all_same { sizes[0] as u8 } else { 0 },
        sample_count: sizes.len() as u32,
        per_sample: if all_same {
            Vec::new()
        } else {
            sizes.iter().map(|&s| s as u8).collect()
        },
    };
    let saiz_bytes = crate::demux::build_saiz_box(&saiz_record)
        .ok_or_else(|| Error::invalid("MP4: saiz record failed to serialise"))?;
    body.extend_from_slice(&saiz_bytes);

    // saio — v0, one placeholder offset patched by build_moof_inner.
    let saio_record = crate::demux::SaioBox {
        version: 0,
        aux_info_type: None,
        aux_info_type_parameter: None,
        offsets: vec![0],
    };
    let saio_bytes = crate::demux::build_saio_box(&saio_record)
        .ok_or_else(|| Error::invalid("MP4: saio record failed to serialise"))?;
    let saio_pos = body.len();
    body.extend_from_slice(&saio_bytes);
    // v0 saio layout: box header (8) + FullBox (4) + entry_count (4),
    // then the 4-byte offset field.
    let field_pos = saio_pos + 16;

    Ok(Some(SaioPatch {
        field_pos,
        aux_data_pos,
    }))
}

/// Append the fragment-local CENC `seig` sample-group boxes — one
/// `sgpd` (`grouping_type = 'seig'`, ISO/IEC 14496-12 §8.9.3 in its
/// `traf` container) plus one `sbgp` (§8.9.2) — to a traf body under
/// construction, when any of the fragment's samples carries a
/// [`crate::cenc::SeigEntry`] override (ISO/IEC 23001-7 §6).
///
/// The distinct entries are deduplicated in first-use order into the
/// `sgpd`; the `sbgp` run-length-encodes the per-sample mapping with
/// the §8.9.4 fragment-local index numbering — "the group description
/// indexes for groups defined within the same fragment start at
/// 0x10001, i.e. the index value 1, with the value 1 in the top 16
/// bits" — and index 0 ("member of no group") for samples on the
/// `tenc` defaults. The §8.9.4 constraint that the sbgp's sample total
/// equal the fragment's sample count holds by construction.
fn append_traf_seig_groups(t: &FragTrackState, body: &mut Vec<u8>) -> Result<()> {
    if t.pending_seig.iter().all(|s| s.is_none()) {
        return Ok(());
    }
    if t.pending_seig.len() != t.pending.len() {
        return Err(Error::invalid(
            "MP4: pending seig overrides out of step with pending samples",
        ));
    }

    // Dedupe entries in first-use order; map each sample to its sbgp
    // group_description_index.
    let mut uniques: Vec<&crate::cenc::SeigEntry> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(t.pending_seig.len());
    for seig in &t.pending_seig {
        match seig {
            None => indices.push(0),
            Some(entry) => {
                let k = match uniques.iter().position(|u| *u == entry) {
                    Some(k) => k,
                    None => {
                        uniques.push(entry);
                        uniques.len() - 1
                    }
                };
                indices.push(0x10001 + k as u32);
            }
        }
    }

    let mut sgpd_entries: Vec<Vec<u8>> = Vec::with_capacity(uniques.len());
    for entry in &uniques {
        sgpd_entries.push(crate::cenc::build_seig_entry(entry)?);
    }
    let sgpd = crate::sample_groups::SampleGroupDescription {
        grouping_type: *b"seig",
        default_sample_description_index: None,
        entries: sgpd_entries,
    };
    body.extend_from_slice(&crate::sample_groups::build_sgpd(&sgpd));

    // Run-length encode the per-sample indices.
    let mut entries: Vec<(u32, u32)> = Vec::new();
    for &idx in &indices {
        match entries.last_mut() {
            Some((count, last)) if *last == idx => *count += 1,
            _ => entries.push((1, idx)),
        }
    }
    let sbgp = crate::sample_groups::SampleToGroup {
        grouping_type: *b"seig",
        grouping_type_parameter: None,
        entries,
    };
    body.extend_from_slice(&crate::sample_groups::build_sbgp(&sbgp));
    Ok(())
}

/// `tfhd` flag bits (ISO/IEC 14496-12 §8.8.7.1).
const TFHD_BASE_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x000002;
const TFHD_DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x000008;
const TFHD_DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x000010;
const TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x000020;
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x020000;

/// Effective per-fragment defaults (size / duration / flags) that the
/// muxer will publish via `tfhd` — chosen so the trun stays minimal when
/// samples agree. Computed once per fragment + reused by both
/// [`build_tfhd`] and [`build_trun`] to keep them in sync.
#[derive(Clone, Copy)]
struct FragmentDefaults {
    /// Per-fragment default size, when all pending samples share one
    /// size. `None` → trun must carry per-sample sizes.
    homogeneous_size: Option<u32>,
    /// Same for duration.
    homogeneous_duration: Option<u32>,
    /// Per-fragment default flags. Set to `Some(f)` when all-except-the-
    /// first sample have flags `f`, and the first sample either matches
    /// or differs (in which case `first_sample_distinct = true`). When
    /// not even the tail agrees, `None` — trun then carries per-sample
    /// flags.
    homogeneous_flags: Option<u32>,
    /// `true` when the first sample's flags differ from the rest (typical
    /// video pattern: keyframe + P-frames). When set, `homogeneous_flags`
    /// reflects samples[1..] and `trun.first_sample_flags` carries
    /// samples[0]'s flags.
    first_sample_distinct: bool,
    /// First-sample flags value, populated when `first_sample_distinct`.
    first_sample_flags: u32,
}

impl FragmentDefaults {
    fn for_track(t: &FragTrackState) -> Self {
        let homogeneous_size = t
            .pending
            .first()
            .map(|s| s.data.len() as u32)
            .filter(|&sz| t.pending.iter().all(|s| s.data.len() as u32 == sz));
        let homogeneous_duration = t
            .pending
            .first()
            .map(|s| s.duration)
            .filter(|&d| t.pending.iter().all(|s| s.duration == d));
        // Flags: prefer "all samples agree" first; else "samples[1..]
        // agree and sample[0] differs"; else None.
        let (homogeneous_flags, first_sample_distinct, first_sample_flags) = match t.pending.len() {
            0 => (None, false, 0),
            1 => (t.pending.first().map(|s| s.flags), false, 0),
            _ => {
                let all_same = t
                    .pending
                    .first()
                    .map(|s| s.flags)
                    .filter(|&f| t.pending.iter().all(|s| s.flags == f));
                if all_same.is_some() {
                    (all_same, false, 0)
                } else {
                    let tail_first = t.pending[1].flags;
                    let tail_same = t.pending[1..].iter().all(|s| s.flags == tail_first);
                    if tail_same {
                        (Some(tail_first), true, t.pending[0].flags)
                    } else {
                        (None, false, 0)
                    }
                }
            }
        };
        Self {
            homogeneous_size,
            homogeneous_duration,
            homogeneous_flags,
            first_sample_distinct,
            first_sample_flags,
        }
    }
}

/// §8.8.7 `tfhd` — TrackFragmentHeaderBox.
///
/// Always sets `default-base-is-moof` (0x020000) so per-sample data
/// offsets in `trun` are relative to the moof box's first byte. Per-track
/// sample defaults are emitted when all samples in the run agree on a
/// value — the trun then omits the field. trex defaults aren't useful
/// here because the moov was written before any packet arrived, so all
/// trex values are zero (CMAF / DASH-IF norm: per-fragment overrides).
fn build_tfhd(t: &FragTrackState, defaults: FragmentDefaults) -> Vec<u8> {
    let mut flags = TFHD_DEFAULT_BASE_IS_MOOF;
    if defaults.homogeneous_duration.is_some() {
        flags |= TFHD_DEFAULT_SAMPLE_DURATION_PRESENT;
    }
    if defaults.homogeneous_size.is_some() {
        flags |= TFHD_DEFAULT_SAMPLE_SIZE_PRESENT;
    }
    if defaults.homogeneous_flags.is_some() {
        flags |= TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT;
    }

    // Body: version(0) + 3-byte flags + track_ID + optional fields in
    // the order declared by the spec table.
    let mut body = Vec::new();
    body.push(0); // version
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&t.track_id.to_be_bytes());
    let _ = TFHD_BASE_DATA_OFFSET_PRESENT;
    let _ = TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT;
    if let Some(d) = defaults.homogeneous_duration {
        body.extend_from_slice(&d.to_be_bytes());
    }
    if let Some(sz) = defaults.homogeneous_size {
        body.extend_from_slice(&sz.to_be_bytes());
    }
    if let Some(f) = defaults.homogeneous_flags {
        body.extend_from_slice(&f.to_be_bytes());
    }
    wrap_box(b"tfhd", &body)
}

/// §8.16.5 `prft` — ProducerReferenceTimeBox.
///
/// `FullBox('prft', version, flags)` whose body is `reference_track_ID`
/// (`u32`), `ntp_timestamp` (`u64`, NTP 64-bit 32.32 fixed-point), then
/// `media_time` (`u32` when `version == 0`, `u64` when `version == 1`).
/// `flags` is `0` in the 2015 edition; the 2022 edition defines named
/// annotation bits (encoder-input/output, finalization, file-write,
/// arbitrary-association) that we pass through verbatim without changing
/// the body layout. The box relates to the *next* `moof` in bitstream
/// order, so the muxer emits it after any `sidx`/`styp` and immediately
/// before the `moof`.
fn build_prft(req: &PrftRequest) -> Vec<u8> {
    // version 1 iff media_time doesn't fit in u32; the caller can also
    // force v1 by setting `force_v1`.
    let version: u8 = if req.force_v1 || req.media_time > u32::MAX as u64 {
        1
    } else {
        0
    };
    let mut body = Vec::with_capacity(if version == 0 { 20 } else { 24 });
    body.push(version);
    // 24-bit flags, big-endian (low three bytes of req.flags).
    let f = req.flags.to_be_bytes();
    body.extend_from_slice(&[f[1], f[2], f[3]]);
    body.extend_from_slice(&req.reference_track_id.to_be_bytes());
    body.extend_from_slice(&req.ntp_timestamp.to_be_bytes());
    if version == 0 {
        body.extend_from_slice(&(req.media_time as u32).to_be_bytes());
    } else {
        body.extend_from_slice(&req.media_time.to_be_bytes());
    }
    wrap_box(b"prft", &body)
}

/// §8.8.12 `tfdt` — TrackFragmentDecodeTimeBox. Always v1 (u64 bmdt) so
/// long streams (> ~22 hours at 48 kHz) don't overflow.
fn build_tfdt(bmdt: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.push(1); // version 1 — 64-bit bmdt
    body.extend_from_slice(&[0u8; 3]);
    body.extend_from_slice(&bmdt.to_be_bytes());
    wrap_box(b"tfdt", &body)
}

/// `trun` flag bits (ISO/IEC 14496-12 §8.8.8.1).
const TRUN_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TRUN_FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x000004;
const TRUN_SAMPLE_DURATION_PRESENT: u32 = 0x000100;
const TRUN_SAMPLE_SIZE_PRESENT: u32 = 0x000200;
const TRUN_SAMPLE_FLAGS_PRESENT: u32 = 0x000400;
const TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT: u32 = 0x000800;

/// §8.8.8 `trun` — TrackRunBox.
///
/// We always emit `data_offset_present` (so the demuxer doesn't need to
/// guess where mdat starts within the moof), then per-sample
/// duration/size/flags/cts only when NOT covered by the per-fragment
/// `tfhd` defaults (homogeneous run). trex defaults aren't useful here
/// because the moov was written before any packet arrived (zero
/// defaults).
fn build_trun(t: &FragTrackState, data_offset: i32, defaults: FragmentDefaults) -> Vec<u8> {
    // Per-sample fields are needed iff the corresponding tfhd default
    // wasn't emitted (i.e. samples disagree on this value).
    let need_per_sample_dur = defaults.homogeneous_duration.is_none();
    let need_per_sample_size = defaults.homogeneous_size.is_none();
    let need_per_sample_flags = defaults.homogeneous_flags.is_none();
    // CTS offset present if any sample has non-zero offset.
    let need_cts = t.pending.iter().any(|s| s.composition_time_offset != 0);

    let mut flags = TRUN_DATA_OFFSET_PRESENT;
    if defaults.first_sample_distinct {
        flags |= TRUN_FIRST_SAMPLE_FLAGS_PRESENT;
    }
    if need_per_sample_dur {
        flags |= TRUN_SAMPLE_DURATION_PRESENT;
    }
    if need_per_sample_size {
        flags |= TRUN_SAMPLE_SIZE_PRESENT;
    }
    if need_per_sample_flags {
        flags |= TRUN_SAMPLE_FLAGS_PRESENT;
    }
    if need_cts {
        flags |= TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT;
    }

    let mut body = Vec::new();
    // version 1 lets composition_time_offset be signed (i32). v0 is
    // unsigned and is what most legacy boxes use; we pick v1 because the
    // demuxer accepts both.
    body.push(1);
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&(t.pending.len() as u32).to_be_bytes());
    body.extend_from_slice(&data_offset.to_be_bytes());
    if defaults.first_sample_distinct {
        body.extend_from_slice(&defaults.first_sample_flags.to_be_bytes());
    }
    for s in &t.pending {
        if need_per_sample_dur {
            body.extend_from_slice(&s.duration.to_be_bytes());
        }
        if need_per_sample_size {
            body.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        }
        if need_per_sample_flags {
            body.extend_from_slice(&s.flags.to_be_bytes());
        }
        if need_cts {
            body.extend_from_slice(&s.composition_time_offset.to_be_bytes());
        }
    }
    wrap_box(b"trun", &body)
}

// --- Random-access boxes (§8.16.3 sidx, §8.8.10–13 mfra/tfra/mfro) -------

/// §8.16.3 — `sidx` SegmentIndexBox.
///
/// We always emit a one-reference sidx covering the immediately-following
/// styp+moof+mdat: this is the simplest legal form per §8.16.3. The
/// `first_offset` is 0 — the next byte after the sidx box is the first
/// byte of the referenced subsegment (per §8.16.3.1: "the offset of the
/// first byte of the first referenced material from the first byte
/// following this `SegmentIndexBox`").
///
/// We pick version=1 (u64 EPT + u64 first_offset) so long-running streams
/// (DASH live with multi-day decode time at 90 kHz timescale) don't
/// truncate the earliest-presentation-time field.
fn build_sidx(
    reference_id: u32,
    timescale: u32,
    earliest_presentation_time: u64,
    referenced_size: u64,
    subsegment_duration: u32,
    starts_with_sap: bool,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(28 + 12);
    body.push(1); // version 1 — u64 EPT + first_offset
    body.extend_from_slice(&[0u8; 3]); // flags
    body.extend_from_slice(&reference_id.to_be_bytes());
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&earliest_presentation_time.to_be_bytes());
    body.extend_from_slice(&0u64.to_be_bytes()); // first_offset = 0
    body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    body.extend_from_slice(&1u16.to_be_bytes()); // reference_count = 1
                                                 // Per-reference 12-byte record:
                                                 //   [bit 31] reference_type (0 = media), [bits 30..0] referenced_size
                                                 //   subsegment_duration (u32)
                                                 //   [bit 31] starts_with_SAP, [bits 30..28] SAP_type, [bits 27..0] SAP_delta_time
    let r0 = (referenced_size.min(0x7FFF_FFFF) as u32) & 0x7FFF_FFFF; // bit 31 = 0 (not sidx)
    body.extend_from_slice(&r0.to_be_bytes());
    body.extend_from_slice(&subsegment_duration.to_be_bytes());
    let sap_type: u32 = if starts_with_sap { 1 } else { 0 }; // SAP type 1 = closed-GOP IDR
    let r2 = (if starts_with_sap { 0x8000_0000u32 } else { 0 }) | (sap_type << 28);
    body.extend_from_slice(&r2.to_be_bytes());
    wrap_box(b"sidx", &body)
}

/// §8.8.11 `tfra` — TrackFragmentRandomAccessBox.
///
/// We always emit version=1 (u64 time + u64 moof_offset) so the index
/// remains valid for files larger than 4 GiB and decode-time fields
/// outside u32 range. `length_size_of_*` fields are all zero (1-byte each
/// for traf/trun/sample numbers), since the muxer always emits exactly
/// one traf per moof and one trun per traf — those numbers are always 1.
fn build_tfra(track_id: u32, entries: &[TfraEmitEntry]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + entries.len() * 19);
    body.push(1); // version 1 — u64 time + moof_offset
    body.extend_from_slice(&[0u8; 3]); // flags
    body.extend_from_slice(&track_id.to_be_bytes());
    // length_size encoding: each 2-bit field encodes (byte_length - 1).
    // We pick 1-byte for traf/trun/sample numbers → all bits zero.
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        body.extend_from_slice(&e.time.to_be_bytes());
        body.extend_from_slice(&e.moof_offset.to_be_bytes());
        // length_size=0 → each *_number is one byte. Clamp to u8::MAX —
        // these are always 1 in our writer so no real clamping happens.
        body.push(e.traf_number.min(u8::MAX as u32) as u8);
        body.push(e.trun_number.min(u8::MAX as u32) as u8);
        body.push(e.sample_number.min(u8::MAX as u32) as u8);
    }
    wrap_box(b"tfra", &body)
}

/// §8.8.13 `mfro` — MovieFragmentRandomAccessOffsetBox.
///
/// FullBox(version=0, flags=0) + size (u32) where `size` is the total
/// size in bytes of the enclosing `mfra` box (so a player can find the
/// mfra by reading the last 16 bytes of the file: 8-byte mfro header +
/// 4-byte version/flags + 4-byte size).
fn build_mfro(mfra_total_size: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&[0u8; 4]); // version + flags
    body.extend_from_slice(&mfra_total_size.to_be_bytes());
    wrap_box(b"mfro", &body)
}
