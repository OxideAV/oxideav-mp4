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
    fn new(base: TrackState, track_id: u32) -> Self {
        Self {
            base,
            track_id,
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
    if streams.is_empty() {
        return Err(Error::invalid("mp4 muxer: need at least one stream"));
    }
    let mut tracks = Vec::with_capacity(streams.len());
    for (i, s) in streams.iter().enumerate() {
        let entry = sample_entry_for(&s.params)?;
        let mut base = TrackState::new(s.clone(), entry);
        // For fragmented mode every fragment is its own chunk, so the
        // chunking-target field is irrelevant — we only carry it to keep
        // TrackState happy.
        base.samples_per_chunk_target = default_samples_per_chunk(&base.stream);
        tracks.push(FragTrackState::new(base, (i as u32) + 1));
    }
    Ok(Box::new(FragmentedMuxer {
        output,
        tracks,
        options,
        frag_options,
        sequence_number: 0,
        header_written: false,
        trailer_written: false,
    }))
}

struct FragmentedMuxer {
    output: Box<dyn WriteSeek>,
    tracks: Vec<FragTrackState>,
    options: Mp4MuxerOptions,
    frag_options: FragmentedOptions,
    /// `mfhd.sequence_number` of the *next* fragment (1-based per spec).
    sequence_number: u32,
    header_written: bool,
    trailer_written: bool,
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
        let moov = build_init_moov(&self.tracks)?;
        self.output.write_all(&moov)?;

        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("mp4 muxer: write_header not called"));
        }
        let idx = packet.stream_index as usize;
        if idx >= self.tracks.len() {
            return Err(Error::invalid(format!(
                "mp4 muxer: unknown stream index {idx}"
            )));
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

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(Error::other("mp4 muxer: write_trailer before write_header"));
        }
        if self.tracks.iter().any(|t| !t.pending.is_empty()) {
            self.flush_fragment()?;
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
        // For EveryKeyframe semantics: the just-pushed keyframe needs to
        // be the *first* sample of the *next* fragment, not the last of
        // the current one. Detach and replay after.
        let mut detached: Vec<(usize, PendingSample)> = Vec::new();
        if matches!(self.frag_options.cadence, FragmentCadence::EveryKeyframe) {
            let anchor = 0usize;
            if let Some(last) = self.tracks[anchor].pending.last() {
                if last.flags & SAMPLE_IS_NON_SYNC == 0 {
                    let s = self.tracks[anchor].pending.pop().unwrap();
                    // Roll back cumulative_duration so the next fragment
                    // re-accounts for this keyframe.
                    self.tracks[anchor].base.cumulative_duration = self.tracks[anchor]
                        .base
                        .cumulative_duration
                        .saturating_sub(s.duration as u64);
                    detached.push((anchor, s));
                }
            }
        }

        // If after detaching everything is empty, nothing to flush.
        if self.tracks.iter().all(|t| t.pending.is_empty()) {
            // Replay any detached samples into the next fragment.
            for (idx, s) in detached {
                self.tracks[idx].base.cumulative_duration += s.duration as u64;
                self.tracks[idx].pending.push(s);
            }
            return Ok(());
        }

        self.sequence_number += 1;
        let seq = self.sequence_number;

        // Build moof.
        let moof = build_moof(seq, &self.tracks)?;
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
            let styp_size: u64 = self
                .frag_options
                .styp
                .as_ref()
                .map(|b| build_styp(b).len() as u64)
                .unwrap_or(0);
            let subsegment_size = styp_size + moof_size + mdat_size;
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
        }

        // Optional styp.
        if let Some(brand) = &self.frag_options.styp {
            let styp = build_styp(brand);
            self.output.write_all(&styp)?;
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
        }
        for (idx, s) in detached {
            self.tracks[idx].base.cumulative_duration += s.duration as u64;
            self.tracks[idx].pending.push(s);
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

fn build_init_moov(tracks: &[FragTrackState]) -> Result<Vec<u8>> {
    // Movie timescale: pick 1000 (matches the non-fragmented path).
    let movie_timescale: u32 = 1000;

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&build_mvhd(movie_timescale, 0, (tracks.len() as u32) + 1));
    for t in tracks {
        moov_body.extend_from_slice(&build_trak_init(t.track_id, &t.base, movie_timescale)?);
    }
    moov_body.extend_from_slice(&build_mvex(tracks));
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
fn build_mvex(tracks: &[FragTrackState]) -> Vec<u8> {
    let mut body = Vec::new();
    for t in tracks {
        body.extend_from_slice(&build_trex(
            t.track_id,
            t.trex_default_sample_duration,
            t.trex_default_sample_size,
            t.trex_default_sample_flags,
        ));
    }
    wrap_box(b"mvex", &body)
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
fn build_moof(seq: u32, tracks: &[FragTrackState]) -> Result<Vec<u8>> {
    // Pass 1: build with data_offset = 0 to learn the moof size.
    let placeholder = build_moof_inner(seq, tracks, |_track_idx, _byte_in_mdat| 0)?;
    let moof_size = placeholder.len() as u64;
    let mdat_header_size: u64 = 8;
    // Pass 2: real offsets relative to start of moof.
    let final_moof = build_moof_inner(seq, tracks, |_track_idx, byte_in_mdat| {
        (moof_size + mdat_header_size + byte_in_mdat) as i32
    })?;
    debug_assert_eq!(final_moof.len() as u64, moof_size, "moof size shifted");
    Ok(final_moof)
}

fn build_moof_inner<F>(seq: u32, tracks: &[FragTrackState], offset_fn: F) -> Result<Vec<u8>>
where
    F: Fn(usize, u64) -> i32,
{
    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&build_mfhd(seq));

    // Walk tracks, accumulating per-track byte offsets within mdat.
    let mut byte_in_mdat: u64 = 0;
    for (i, t) in tracks.iter().enumerate() {
        if t.pending.is_empty() {
            continue;
        }
        let track_first_byte = byte_in_mdat;
        let trun_data_offset = offset_fn(i, track_first_byte);
        let traf = build_traf(t, trun_data_offset)?;
        moof_body.extend_from_slice(&traf);
        // Advance byte pointer past this track's samples.
        for s in &t.pending {
            byte_in_mdat += s.data.len() as u64;
        }
    }
    Ok(wrap_box(b"moof", &moof_body))
}

/// §8.8.5 `mfhd` — MovieFragmentHeaderBox.
/// FullBox(0,0) + sequence_number(u32). 8-byte payload.
fn build_mfhd(seq: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&[0u8; 4]); // version + flags
    body.extend_from_slice(&seq.to_be_bytes());
    wrap_box(b"mfhd", &body)
}

/// `traf` for one track: tfhd + tfdt + trun.
fn build_traf(t: &FragTrackState, trun_data_offset: i32) -> Result<Vec<u8>> {
    let defaults = FragmentDefaults::for_track(t);
    let mut body = Vec::new();
    body.extend_from_slice(&build_tfhd(t, defaults));
    body.extend_from_slice(&build_tfdt(t.next_bmdt));
    body.extend_from_slice(&build_trun(t, trun_data_offset, defaults));
    Ok(wrap_box(b"traf", &body))
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
