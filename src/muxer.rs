//! MP4 / ISOBMFF muxer — moov-at-end ("shape A") with optional faststart.
//!
//! # Default layout (moov-at-end)
//!
//! ```text
//! ftyp
//! mdat (streaming — packets concatenated as they arrive)
//! moov (written on close, with full sample tables)
//! ```
//!
//! # Faststart layout (`Mp4MuxerOptions::faststart = true`)
//!
//! ```text
//! ftyp
//! moov (chunk offsets pre-patched)
//! mdat
//! ```
//!
//! To produce the faststart layout without a `Read` on the output we buffer
//! mdat in memory during `write_packet`, then emit the file in a single
//! `[ftyp][moov][mdat]` sequence at `write_trailer`. This trades memory for
//! simplicity and pure-Rust + no-extra-crate constraints; see
//! [`Mp4MuxerOptions::faststart`] docs for the tradeoff.
//!
//! # Codec-agnostic
//!
//! The muxer API is codec-agnostic: `write_packet` only appends bytes and
//! updates bookkeeping. The only place codec knowledge enters is
//! `sample_entries::sample_entry_for` which maps each stream's `CodecId` to
//! its `stsd` sample-entry bytes. If a codec isn't in that table, `open`
//! returns `Error::Unsupported` — never at `write_packet` time.

use std::io::{Cursor, Seek, SeekFrom, Write};

use oxideav_core::{Error, MediaType, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::options::{BrandPreset, FragmentedOptions, Mp4MuxerOptions, TrackSampleGroups};
use crate::sample_entries::{
    apply_protection, sample_entry_for, subtitle_handler_for, subtitle_uses_sthd, SampleEntry,
};
use crate::sample_groups::{
    build_csgp, build_sbgp, build_sgpd, CompactSampleToGroup, SampleGroupDescription, SampleToGroup,
};

/// Per-track state kept between `write_packet` calls.
pub(crate) struct TrackState {
    /// Cloned stream info (for handler, time base, etc.).
    pub(crate) stream: StreamInfo,
    /// Media time scale (ticks/second in the track's own time base).
    pub(crate) media_time_scale: u32,
    /// Sample entry (built at open, written in `moov`).
    pub(crate) sample_entry: SampleEntry,

    // Sample tables.
    /// `stsz`: one entry per sample (u32 byte size).
    pub(crate) sample_sizes: Vec<u32>,
    /// `stts`: run-length `(sample_count, sample_delta)` pairs.
    pub(crate) stts: Vec<(u32, u32)>,
    /// `stss`: 1-based sample indices of keyframes.
    pub(crate) keyframes: Vec<u32>,
    /// `stco`/`co64`: absolute file offsets of chunks.
    pub(crate) chunk_offsets: Vec<u64>,
    /// `stsc`: run-length `(first_chunk, samples_per_chunk, sample_desc_idx)`.
    pub(crate) stsc: Vec<(u32, u32, u32)>,

    // Chunking state.
    current_chunk_samples: u32,
    current_chunk_start_offset: u64,
    /// How many samples we want to batch into one chunk (~1 sec worth).
    pub(crate) samples_per_chunk_target: u32,
    /// Number of chunks emitted so far.
    chunk_count: u32,
    /// Used to track stsc runs: samples-per-chunk of the previous chunk.
    last_samples_per_chunk: u32,

    // Running counters.
    /// Sample index of the next packet (0-based).
    next_sample_index: u32,
    /// Cumulative media-time-scale ticks written so far.
    pub(crate) cumulative_duration: u64,
    /// PTS of the previous packet in media time scale, for delta calculation.
    pub(crate) prev_pts_in_ts: Option<i64>,
    /// First PTS in media time scale (for duration calculation + elst).
    pub(crate) first_pts_in_ts: Option<i64>,
}

impl TrackState {
    pub(crate) fn new(stream: StreamInfo, sample_entry: SampleEntry) -> Self {
        // Media time scale: for audio prefer sample_rate, for video use a
        // reasonable default of 1000 (so video durations are in ms).
        let media_time_scale = match stream.params.media_type {
            MediaType::Audio => stream.params.sample_rate.unwrap_or(1000),
            _ => 1000,
        };
        Self {
            stream,
            media_time_scale,
            sample_entry,
            sample_sizes: Vec::new(),
            stts: Vec::new(),
            keyframes: Vec::new(),
            chunk_offsets: Vec::new(),
            stsc: Vec::new(),
            current_chunk_samples: 0,
            current_chunk_start_offset: 0,
            samples_per_chunk_target: 1,
            chunk_count: 0,
            last_samples_per_chunk: 0,
            next_sample_index: 0,
            cumulative_duration: 0,
            prev_pts_in_ts: None,
            first_pts_in_ts: None,
        }
    }

    /// Finalise any open chunk's stsc bookkeeping.
    fn close_current_chunk(&mut self) {
        if self.current_chunk_samples == 0 {
            return;
        }
        let spc = self.current_chunk_samples;
        self.chunk_count += 1;
        // stsc: if this chunk's samples_per_chunk differs from the previous
        // run, start a new run.
        if spc != self.last_samples_per_chunk {
            self.stsc.push((self.chunk_count, spc, 1));
            self.last_samples_per_chunk = spc;
        }
        self.current_chunk_samples = 0;
    }
}

/// Default entry point: matches the historical `mp4` muxer (major=`mp42`,
/// no faststart, no fragmentation). Use [`open_with_options`] for explicit
/// control.
pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    open_with_options(output, streams, Mp4MuxerOptions::default())
}

/// Open a `mov` muxer: identical to [`open`] but emits a QuickTime `ftyp`
/// brand (major=`qt  `). Registered under the `"mov"` name.
pub fn open_mov(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Mov,
        ..Mp4MuxerOptions::default()
    };
    open_with_options(output, streams, opts)
}

/// Open an `ismv` muxer: emits an ISMV / Smooth Streaming `ftyp` brand
/// (major=`iso4`, compatible=`iso4 piff iso6 isml`) AND switches to
/// fragmented output (moof+mdat after each fragment cadence boundary).
/// Registered under the `"ismv"` name.
pub fn open_ismv(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Ismv,
        fragmented: Some(FragmentedOptions::default()),
        ..Mp4MuxerOptions::default()
    };
    open_with_options(output, streams, opts)
}

/// Open a DASH / CMAF fragmented muxer: emits `iso6 / dash` brand, with
/// `mvex+trex` in moov and `styp+moof+mdat` per fragment. Registered
/// under the `"dash"` and `"cmaf"` names.
pub fn open_dash(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"mp41", *b"dash", *b"cmfc"],
        },
        fragmented: Some(FragmentedOptions::default()),
        ..Mp4MuxerOptions::default()
    };
    open_with_options(output, streams, opts)
}

/// Programmatic entry point with explicit options.
pub fn open_with_options(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
) -> Result<Box<dyn Muxer>> {
    if streams.is_empty() {
        return Err(Error::invalid("mp4 muxer: need at least one stream"));
    }
    if options.faststart && options.fragmented.is_some() {
        return Err(Error::invalid(
            "mp4 muxer: faststart and fragmented are mutually exclusive",
        ));
    }
    if let Some(frag_opts) = options.fragmented.clone() {
        return crate::frag::open_fragmented(output, streams, options, frag_opts);
    }
    let mut tracks = Vec::with_capacity(streams.len());
    for (i, s) in streams.iter().enumerate() {
        let mut entry = sample_entry_for(&s.params)?;
        // ISO/IEC 14496-12 §8.12: wrap the entry into its protected
        // enc* form when a protection directive targets this stream.
        if let Some(prot) = options
            .track_protection
            .iter()
            .find(|p| p.stream_index == i)
        {
            entry = apply_protection(entry, s.params.media_type, prot)?;
        }
        tracks.push(TrackState::new(s.clone(), entry));
    }
    Ok(Box::new(Mp4Muxer {
        output,
        tracks,
        options,
        ftyp_bytes: Vec::new(),
        mdat_size_offset: 0,
        mdat_start_offset: 0,
        mdat_bytes: 0,
        mdat_buffer: None,
        header_written: false,
        trailer_written: false,
    }))
}

struct Mp4Muxer {
    output: Box<dyn WriteSeek>,
    tracks: Vec<TrackState>,
    options: Mp4MuxerOptions,
    /// Serialized `ftyp` box bytes (kept for faststart rewrite).
    ftyp_bytes: Vec<u8>,
    /// Byte offset of the mdat `size` field (direct-write mode only).
    mdat_size_offset: u64,
    /// Byte offset just after the mdat header (direct-write mode: in the
    /// real output; faststart mode: virtual — the intended position in the
    /// final file).
    mdat_start_offset: u64,
    /// Running count of mdat payload bytes.
    mdat_bytes: u64,
    /// In-memory mdat payload. `Some` iff `options.faststart` is `true`.
    mdat_buffer: Option<Cursor<Vec<u8>>>,
    header_written: bool,
    trailer_written: bool,
}

impl Muxer for Mp4Muxer {
    fn format_name(&self) -> &str {
        match self.options.brand {
            BrandPreset::Mov => "mov",
            BrandPreset::Ismv => "ismv",
            _ => "mp4",
        }
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("mp4 muxer: write_header called twice"));
        }

        // Build the ftyp body from the configured brand preset.
        let major = self.options.brand.major_brand();
        let compat = self.options.brand.compatible_brands();
        let mut ftyp_body = Vec::with_capacity(8 + 4 * compat.len());
        ftyp_body.extend_from_slice(&major);
        // minor_version: 0x200 is conventional for mp4/isom; 0x0 for qt;
        // 0x0 for iso4/ismv. All readers ignore this in practice.
        let minor_version: u32 = match self.options.brand {
            BrandPreset::Mp4 => 0x0000_0200,
            _ => 0,
        };
        ftyp_body.extend_from_slice(&minor_version.to_be_bytes());
        for b in &compat {
            ftyp_body.extend_from_slice(b);
        }
        // Serialize the whole ftyp into memory (we need its bytes either way;
        // faststart mode reuses them at trailer time).
        let ftyp = wrap_box(b"ftyp", &ftyp_body);
        self.ftyp_bytes = ftyp.clone();

        if self.options.faststart {
            // Write ftyp to the real output now. mdat payload goes to an
            // in-memory buffer; moov is emitted at trailer time before mdat.
            self.output.write_all(&ftyp)?;
            // Virtual offsets: the final layout is [ftyp][moov][mdat]. We
            // don't know moov_size yet, so we leave mdat_start_offset at 0
            // and compute final chunk offsets at trailer time by adding
            // `ftyp_size + moov_size + 8` (mdat header) to each stored
            // relative offset.
            self.mdat_start_offset = 0;
            self.mdat_size_offset = 0;
            self.mdat_buffer = Some(Cursor::new(Vec::new()));
        } else {
            // Direct-write mode: write ftyp + mdat header placeholder, stream
            // mdat payload to the output.
            self.output.write_all(&ftyp)?;

            // Start mdat as a streaming box. The header form is chosen up
            // front because the payload streams to the output before its
            // final size is known. Default: a 32-bit `size` placeholder
            // (8-byte header) — patched at trailer time, errors out if the
            // payload exceeds 4 GiB. With `large_mdat`: the §4.2 extended
            // form `[size=1]["mdat"][largesize:u64]` (16-byte header), so
            // the payload may grow past 4 GiB; the u64 `largesize` is
            // patched at trailer time.
            let pos = self.output.stream_position()?;
            self.mdat_size_offset = pos;
            if self.options.large_mdat {
                // size == 1 signals "read the 64-bit largesize after the
                // type" (§4.2). The largesize placeholder is zeroed now and
                // backfilled in write_trailer_direct.
                self.output.write_all(&1u32.to_be_bytes())?;
                self.output.write_all(b"mdat")?;
                self.output.write_all(&[0u8; 8])?;
            } else {
                self.output.write_all(&[0, 0, 0, 0])?;
                self.output.write_all(b"mdat")?;
            }
            self.mdat_start_offset = self.output.stream_position()?;
        }

        // Compute per-track samples_per_chunk_target (≈ 1 sec of samples).
        for t in &mut self.tracks {
            t.samples_per_chunk_target = default_samples_per_chunk(&t.stream);
            t.current_chunk_start_offset = self.mdat_start_offset;
        }

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

        // Bytes first: capture offset, append payload, update mdat counter.
        // `cur_offset` is absolute in direct-write mode and relative-to-mdat
        // in faststart mode (patched up at trailer time).
        let cur_offset = self.mdat_start_offset + self.mdat_bytes;
        if let Some(buf) = self.mdat_buffer.as_mut() {
            buf.get_mut().extend_from_slice(&packet.data);
        } else {
            self.output.write_all(&packet.data)?;
        }
        self.mdat_bytes += packet.data.len() as u64;

        // Now update bookkeeping on the track (released borrow above).
        let t = &mut self.tracks[idx];

        // Convert packet pts (in its own time_base) to track's media time scale.
        let pts_in_ts = packet
            .pts
            .map(|v| rescale_to_media_ts(v, packet.time_base, t.media_time_scale));
        // Decode delta: if pts is available, use difference from previous; else
        // fall back to packet.duration rescaled.
        let delta = compute_delta(t, packet, pts_in_ts);

        t.sample_sizes.push(packet.data.len() as u32);
        // stts RLE: append a new (1, delta) or extend the last run.
        match t.stts.last_mut() {
            Some((count, d)) if *d == delta => *count += 1,
            _ => t.stts.push((1, delta)),
        }
        t.cumulative_duration += delta as u64;
        if packet.flags.keyframe {
            t.keyframes.push(t.next_sample_index + 1);
        }

        // Chunking.
        if t.current_chunk_samples == 0 {
            // First sample of a new chunk — record its offset.
            t.chunk_offsets.push(cur_offset);
            t.current_chunk_start_offset = cur_offset;
        }
        t.current_chunk_samples += 1;
        if t.current_chunk_samples >= t.samples_per_chunk_target {
            t.close_current_chunk();
        }

        if let Some(p) = pts_in_ts {
            if t.first_pts_in_ts.is_none() {
                t.first_pts_in_ts = Some(p);
            }
            t.prev_pts_in_ts = Some(p);
        } else {
            // Without pts, accumulate via deltas.
            let base = t.prev_pts_in_ts.unwrap_or(0);
            t.prev_pts_in_ts = Some(base + delta as i64);
            if t.first_pts_in_ts.is_none() {
                t.first_pts_in_ts = Some(0);
            }
        }
        t.next_sample_index += 1;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(Error::other("mp4 muxer: write_trailer before write_header"));
        }

        // Close any open chunks.
        for t in &mut self.tracks {
            t.close_current_chunk();
        }

        if self.options.faststart {
            self.write_trailer_faststart()?;
        } else {
            self.write_trailer_direct()?;
        }

        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

impl Mp4Muxer {
    /// Finalise in the default `[ftyp][mdat][moov]` layout.
    fn write_trailer_direct(&mut self) -> Result<()> {
        // Patch mdat size header. Current position == end of mdat payload.
        let end_pos = self.output.stream_position()?;
        let mdat_total = end_pos - self.mdat_size_offset;
        if self.options.large_mdat {
            // Extended form: the 32-bit `size` field already holds `1`; the
            // real size is the 8-byte `largesize` immediately after the
            // 4-byte type (i.e. at `mdat_size_offset + 8`). `mdat_total`
            // here counts the full 16-byte header + payload, which is what
            // §4.2 says `largesize` reports (the box's total size).
            self.output
                .seek(SeekFrom::Start(self.mdat_size_offset + 8))?;
            self.output.write_all(&mdat_total.to_be_bytes())?;
            self.output.seek(SeekFrom::Start(end_pos))?;
        } else if mdat_total <= u32::MAX as u64 {
            self.output.seek(SeekFrom::Start(self.mdat_size_offset))?;
            self.output.write_all(&(mdat_total as u32).to_be_bytes())?;
            self.output.seek(SeekFrom::Start(end_pos))?;
        } else {
            return Err(Error::unsupported(
                "mp4 muxer: mdat > 4 GiB requires largesize header; \
                 set Mp4MuxerOptions::large_mdat = true",
            ));
        }

        // Write moov at the end.
        let moov = build_moov(
            &self.tracks,
            self.options.write_edit_list,
            &self.options.track_sample_groups,
            &self.options.pssh,
        )?;
        self.output.write_all(&moov)?;
        Ok(())
    }

    /// Finalise in the faststart `[ftyp][moov][mdat]` layout.
    ///
    /// ftyp was already written at `write_header` time; mdat payload has been
    /// buffered in memory. We need to (a) determine the final moov size, (b)
    /// patch every chunk offset by `ftyp_size + moov_size + 8` (mdat header),
    /// (c) emit moov, then (d) emit the mdat header + buffered payload.
    ///
    /// moov size depends on whether chunk offsets fit in 32 bits, which in
    /// turn depends on the final mdat position (which depends on moov size).
    /// We break the cycle by computing a "relative moov" first, then using
    /// its size to pick `stco` vs `co64`, then adding the base offset.
    fn write_trailer_faststart(&mut self) -> Result<()> {
        let ftyp_size = self.ftyp_bytes.len() as u64;

        let mdat_payload = self
            .mdat_buffer
            .take()
            .map(|c| c.into_inner())
            .unwrap_or_default();
        // faststart buffers the whole payload, so the size is known here:
        // pick the §4.2 extended (16-byte) header when the caller asked for
        // it OR when the compact 32-bit `size` would overflow. Otherwise the
        // compact 8-byte header (byte-identical to the historical output).
        let payload_len = mdat_payload.len() as u64;
        let needs_large = self.options.large_mdat || (8 + payload_len) > u32::MAX as u64;
        let mdat_header_size: u64 = if needs_large { 16 } else { 8 };
        let mdat_total = mdat_header_size + payload_len;

        // Iteratively converge on moov_size. Because chunk-offset width (stco
        // 32-bit vs co64 64-bit) is chosen per build_moov call and depends on
        // whether any patched offset exceeds u32::MAX, we must loop until
        // stable. In practice this is ≤ 2 iterations.
        let orig_offsets: Vec<Vec<u64>> = self
            .tracks
            .iter()
            .map(|t| t.chunk_offsets.clone())
            .collect();
        // Start with moov_size=0 as a lower-bound guess.
        let mut moov_size: u64 = 0;
        let mut moov_bytes: Vec<u8> = Vec::new();
        for attempt in 0..4 {
            let base = ftyp_size + moov_size + mdat_header_size;
            // Apply the base to stored (mdat-relative) offsets.
            for (t, orig) in self.tracks.iter_mut().zip(orig_offsets.iter()) {
                t.chunk_offsets = orig.iter().map(|o| *o + base).collect();
            }
            let candidate = build_moov(
                &self.tracks,
                self.options.write_edit_list,
                &self.options.track_sample_groups,
                &self.options.pssh,
            )?;
            let candidate_size = candidate.len() as u64;
            let converged = candidate_size == moov_size;
            moov_size = candidate_size;
            moov_bytes = candidate;
            if converged {
                break;
            }
            if attempt == 3 {
                return Err(Error::other(
                    "mp4 muxer: faststart moov size did not converge",
                ));
            }
        }

        // ftyp is already at the start of the output. Seek past it and write
        // moov followed by mdat (header + payload).
        self.output.seek(SeekFrom::Start(ftyp_size))?;
        self.output.write_all(&moov_bytes)?;
        // mdat box header: compact 32-bit `size` or §4.2 extended
        // `[size=1]["mdat"][largesize:u64]`, matching the size baked into
        // the chunk offsets above (`mdat_header_size`).
        if needs_large {
            self.output.write_all(&1u32.to_be_bytes())?;
            self.output.write_all(b"mdat")?;
            self.output.write_all(&mdat_total.to_be_bytes())?;
        } else {
            self.output.write_all(&(mdat_total as u32).to_be_bytes())?;
            self.output.write_all(b"mdat")?;
        }
        self.output.write_all(&mdat_payload)?;
        Ok(())
    }
}

// --- Moov builders --------------------------------------------------------

fn build_moov(
    tracks: &[TrackState],
    write_edit_list: bool,
    track_sample_groups: &[TrackSampleGroups],
    pssh: &[crate::cenc::PsshBox],
) -> Result<Vec<u8>> {
    // mvhd: use the largest media-time-scale duration as a rough movie
    // duration at timescale 1000.
    let movie_timescale: u32 = 1000;
    let mut movie_duration: u64 = 0;
    for (i, t) in tracks.iter().enumerate() {
        let track_duration_movie =
            rescale_u64(t.cumulative_duration, t.media_time_scale, movie_timescale);
        if track_duration_movie > movie_duration {
            movie_duration = track_duration_movie;
        }
        // next_track_id: we use i+2 (ids are 1-based and we want to reserve
        // space for future tracks).
        let _ = i;
    }
    let next_track_id = (tracks.len() as u32) + 1;

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&build_mvhd(movie_timescale, movie_duration, next_track_id));
    for (i, t) in tracks.iter().enumerate() {
        // Gather all sample-group directives that target this stream index.
        // Multiple directives for the same stream accumulate in encounter
        // order.
        let mut track_sbgp: Vec<&SampleToGroup> = Vec::new();
        let mut track_sgpd: Vec<&SampleGroupDescription> = Vec::new();
        let mut track_csgp: Vec<&CompactSampleToGroup> = Vec::new();
        for tsg in track_sample_groups.iter().filter(|g| g.stream_index == i) {
            track_sbgp.extend(tsg.sbgp.iter());
            track_sgpd.extend(tsg.sgpd.iter());
            track_csgp.extend(tsg.csgp.iter());
        }
        moov_body.extend_from_slice(&build_trak(
            i as u32 + 1,
            t,
            movie_timescale,
            write_edit_list,
            &track_sbgp,
            &track_sgpd,
            &track_csgp,
        )?);
    }
    // ISO/IEC 23001-7 §8.1: moov-level pssh boxes, one per DRM
    // system, after the trak boxes.
    for record in pssh {
        moov_body.extend_from_slice(&crate::cenc::build_pssh_box(record)?);
    }
    Ok(wrap_box(b"moov", &moov_body))
}

pub(crate) fn build_mvhd(timescale: u32, duration: u64, next_track_id: u32) -> Vec<u8> {
    // Choose version 0 if duration fits in u32, else version 1.
    let use_v1 = duration > u32::MAX as u64;
    let mut body = Vec::with_capacity(120);
    if use_v1 {
        body.push(1); // version
        body.extend_from_slice(&[0, 0, 0]); // flags
        body.extend_from_slice(&0u64.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u64.to_be_bytes()); // modification_time
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.push(0); // version
        body.extend_from_slice(&[0, 0, 0]); // flags
        body.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    // rate 1.0 (16.16), volume 1.0 (8.8), reserved
    body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
    body.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    body.extend_from_slice(&[0, 0]); // reserved u16
    body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // reserved 2x u32
                                                       // identity matrix (9 × 32-bit fixed-point, 3x3)
    let identity: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in identity {
        body.extend_from_slice(&v.to_be_bytes());
    }
    // pre_defined (6 × u32)
    body.extend_from_slice(&[0u8; 24]);
    body.extend_from_slice(&next_track_id.to_be_bytes());
    wrap_box(b"mvhd", &body)
}

#[allow(clippy::too_many_arguments)]
fn build_trak(
    track_id: u32,
    t: &TrackState,
    movie_timescale: u32,
    write_edit_list: bool,
    sbgp: &[&SampleToGroup],
    sgpd: &[&SampleGroupDescription],
    csgp: &[&CompactSampleToGroup],
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let track_duration_movie =
        rescale_u64(t.cumulative_duration, t.media_time_scale, movie_timescale);
    body.extend_from_slice(&build_tkhd(track_id, track_duration_movie, &t.stream));
    // edts/elst (ISO/IEC 14496-12 §8.6.5–6) goes between tkhd and mdia. Emit
    // it only when the track has a positive start delay to offset.
    if write_edit_list {
        if let Some(edts) = build_edts(t, movie_timescale, track_duration_movie) {
            body.extend_from_slice(&edts);
        }
    }
    body.extend_from_slice(&build_mdia_with_sample_groups(t, sbgp, sgpd, csgp)?);
    Ok(wrap_box(b"trak", &body))
}

/// Build an `edts/elst` (EditBox containing an EditListBox) for a track whose
/// first presented sample has a positive PTS, per ISO/IEC 14496-12 §8.6.5–6.
///
/// "An empty edit is used to offset the start time of a track" (§8.6.5.1). We
/// therefore emit a two-entry list:
///
/// 1. an **empty edit** (`media_time = -1`) whose `segment_duration` is the
///    start delay expressed in the movie timescale, and
/// 2. a normal edit (`media_time = 0`, `media_rate = 1.0`) covering the rest
///    of the track's media duration,
///
/// so a player holds the timeline blank for the delay and then plays the
/// track from its first media sample. Returns `None` when the first PTS is
/// zero/absent or the rescaled delay is zero (no offset needed → implicit
/// 1:1 mapping applies and no box is written).
///
/// `version` is chosen per §8.6.6.2: version 1 (64-bit fields) when any
/// duration exceeds the 32-bit range, else version 0.
fn build_edts(t: &TrackState, movie_timescale: u32, track_duration_movie: u64) -> Option<Vec<u8>> {
    let first_pts = t.first_pts_in_ts?;
    if first_pts <= 0 {
        return None;
    }
    // Start delay in the movie timescale (elst segment_duration units).
    let delay_movie = rescale_u64(first_pts as u64, t.media_time_scale, movie_timescale);
    if delay_movie == 0 {
        return None;
    }

    // version 1 if any field needs more than 32 bits.
    let use_v1 = delay_movie > u32::MAX as u64 || track_duration_movie > u32::MAX as u64;
    let media_rate: u32 = 0x0001_0000; // 1.0 in 16.16 (rate_integer=1, frac=0).

    let mut body = Vec::new();
    body.push(if use_v1 { 1 } else { 0 }); // version
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(&2u32.to_be_bytes()); // entry_count

    // Entry 1: empty edit (media_time = -1) for the start delay.
    push_elst_entry(&mut body, use_v1, delay_movie, -1, media_rate);
    // Entry 2: normal edit (media_time = 0) covering the track media duration.
    push_elst_entry(&mut body, use_v1, track_duration_movie, 0, media_rate);

    let elst = wrap_box(b"elst", &body);
    Some(wrap_box(b"edts", &elst))
}

/// Append one EditListBox entry (`segment_duration`, `media_time`,
/// `media_rate`) using v0 (32-bit) or v1 (64-bit) field widths per §8.6.6.2.
fn push_elst_entry(
    out: &mut Vec<u8>,
    use_v1: bool,
    segment_duration: u64,
    media_time: i64,
    media_rate: u32,
) {
    if use_v1 {
        out.extend_from_slice(&segment_duration.to_be_bytes());
        out.extend_from_slice(&media_time.to_be_bytes());
    } else {
        out.extend_from_slice(&(segment_duration as u32).to_be_bytes());
        out.extend_from_slice(&(media_time as i32).to_be_bytes());
    }
    out.extend_from_slice(&media_rate.to_be_bytes());
}

pub(crate) fn build_tkhd(track_id: u32, duration: u64, stream: &StreamInfo) -> Vec<u8> {
    let use_v1 = duration > u32::MAX as u64;
    let mut body = Vec::new();
    let flags: u32 = 0x0000_0007; // track_enabled | track_in_movie | track_in_preview
    if use_v1 {
        body.push(1);
        body.extend_from_slice(&flags.to_be_bytes()[1..4]);
        body.extend_from_slice(&0u64.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u64.to_be_bytes()); // modification_time
        body.extend_from_slice(&track_id.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // reserved
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.push(0);
        body.extend_from_slice(&flags.to_be_bytes()[1..4]);
        body.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        body.extend_from_slice(&track_id.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // reserved
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 8]); // reserved 2x u32
    body.extend_from_slice(&0i16.to_be_bytes()); // layer
    body.extend_from_slice(&0i16.to_be_bytes()); // alternate_group
                                                 // volume: 1.0 for audio, 0 for non-audio
    let volume: u16 = if stream.params.media_type == MediaType::Audio {
        0x0100
    } else {
        0
    };
    body.extend_from_slice(&volume.to_be_bytes());
    body.extend_from_slice(&[0, 0]); // reserved u16
                                     // identity matrix
    let identity: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in identity {
        body.extend_from_slice(&v.to_be_bytes());
    }
    // width/height in 16.16 fixed (video only; audio has zeros).
    let (w, h) = match stream.params.media_type {
        MediaType::Video => (
            (stream.params.width.unwrap_or(0)) << 16,
            (stream.params.height.unwrap_or(0)) << 16,
        ),
        _ => (0, 0),
    };
    body.extend_from_slice(&w.to_be_bytes());
    body.extend_from_slice(&h.to_be_bytes());
    wrap_box(b"tkhd", &body)
}

pub(crate) fn build_mdia(t: &TrackState) -> Result<Vec<u8>> {
    build_mdia_with_sample_groups(t, &[], &[], &[])
}

fn build_mdia_with_sample_groups(
    t: &TrackState,
    sbgp: &[&SampleToGroup],
    sgpd: &[&SampleGroupDescription],
    csgp: &[&CompactSampleToGroup],
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&build_mdhd(t));
    body.extend_from_slice(&build_hdlr(&t.stream));
    body.extend_from_slice(&build_minf_with_sample_groups(t, sbgp, sgpd, csgp)?);
    Ok(wrap_box(b"mdia", &body))
}

fn build_mdhd(t: &TrackState) -> Vec<u8> {
    let duration = t.cumulative_duration;
    let use_v1 = duration > u32::MAX as u64;
    let mut body = Vec::new();
    if use_v1 {
        body.push(1);
        body.extend_from_slice(&[0, 0, 0]); // flags
        body.extend_from_slice(&0u64.to_be_bytes()); // creation
        body.extend_from_slice(&0u64.to_be_bytes()); // modification
        body.extend_from_slice(&t.media_time_scale.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
    } else {
        body.push(0);
        body.extend_from_slice(&[0, 0, 0]); // flags
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&t.media_time_scale.to_be_bytes());
        body.extend_from_slice(&(duration as u32).to_be_bytes());
    }
    // language: ISO-639-2/T packed ("und") + pre_defined u16
    let lang = pack_language(b"und");
    body.extend_from_slice(&lang.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    wrap_box(b"mdhd", &body)
}

fn pack_language(code: &[u8; 3]) -> u16 {
    let a = (code[0].saturating_sub(0x60) & 0x1F) as u16;
    let b = (code[1].saturating_sub(0x60) & 0x1F) as u16;
    let c = (code[2].saturating_sub(0x60) & 0x1F) as u16;
    (a << 10) | (b << 5) | c
}

fn build_hdlr(stream: &StreamInfo) -> Vec<u8> {
    // ISO/IEC 14496-12 §8.4.3 handler-type four-char-codes:
    // `soun` (Audio), `vide` (Video), `subt` (BMFF §12.6.1 subtitle),
    // `text` (BMFF §12.5.1 timed text — `tx3g`/`mov_text`), `data`
    // (everything else). The handler-type drives the demuxer's
    // MediaType mapping; the muxer mirrors that here so a
    // mov_text/webvtt/ttml/sbtt/stxt round-trip ends up on the same
    // MediaType::Subtitle stream the demuxer surfaced.
    let (handler_type, name): ([u8; 4], &str) = match stream.params.media_type {
        MediaType::Audio => (*b"soun", "SoundHandler"),
        MediaType::Video => (*b"vide", "VideoHandler"),
        MediaType::Subtitle => (
            subtitle_handler_for(stream.params.codec_id.as_str()),
            "SubtitleHandler",
        ),
        _ => (*b"data", "DataHandler"),
    };
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    body.extend_from_slice(&handler_type);
    body.extend_from_slice(&[0u8; 12]); // 3x reserved u32
    body.extend_from_slice(name.as_bytes());
    body.push(0); // NUL terminator
    wrap_box(b"hdlr", &body)
}

fn build_minf_with_sample_groups(
    t: &TrackState,
    sbgp: &[&SampleToGroup],
    sgpd: &[&SampleGroupDescription],
    csgp: &[&CompactSampleToGroup],
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    match t.stream.params.media_type {
        MediaType::Audio => body.extend_from_slice(&build_smhd()),
        MediaType::Video => body.extend_from_slice(&build_vmhd()),
        MediaType::Subtitle => {
            // BMFF §12.6.2 SubtitleMediaHeader (`sthd`) for the `subt`
            // handler (wvtt/stpp/sbtt/stxt); BMFF §12.5.2 null-media
            // header (`nmhd`) for the `text` handler (tx3g/mov_text).
            if subtitle_uses_sthd(t.stream.params.codec_id.as_str()) {
                body.extend_from_slice(&build_sthd());
            } else {
                body.extend_from_slice(&build_nmhd());
            }
        }
        _ => body.extend_from_slice(&build_nmhd()),
    }
    body.extend_from_slice(&build_dinf());
    body.extend_from_slice(&build_stbl_with_sample_groups(t, sbgp, sgpd, csgp)?);
    Ok(wrap_box(b"minf", &body))
}

/// SubtitleMediaHeader box — FullBox with no payload fields.
/// ISO/IEC 14496-12 §12.6.2.2.
fn build_sthd() -> Vec<u8> {
    let body = [0u8; 4]; // version + 24-bit flags, all zero.
    wrap_box(b"sthd", &body)
}

fn build_smhd() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&0i16.to_be_bytes()); // balance
    body.extend_from_slice(&[0, 0]); // reserved
    wrap_box(b"smhd", &body)
}

fn build_vmhd() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 1]); // version + flags (flags=1 required)
    body.extend_from_slice(&0u16.to_be_bytes()); // graphicsmode
    body.extend_from_slice(&[0u8; 6]); // opcolor (3x u16)
    wrap_box(b"vmhd", &body)
}

fn build_nmhd() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    wrap_box(b"nmhd", &body)
}

fn build_dinf() -> Vec<u8> {
    // dref: FullBox + entry_count(u32=1) + url (self-referring, flags=1)
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    dref_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
                                                      // "url " box, FullBox, flags=1 means "self-contained, no location".
    let mut url_body = Vec::new();
    url_body.extend_from_slice(&[0, 0, 0, 1]);
    dref_body.extend_from_slice(&wrap_box(b"url ", &url_body));
    let dref = wrap_box(b"dref", &dref_body);
    wrap_box(b"dinf", &dref)
}

/// `build_stbl` variant that appends optional `sbgp` / `sgpd` / `csgp`
/// boxes (ISO/IEC 14496-12 §8.9.2 / §8.9.3 / §8.9.5) after the
/// chunk-offset table.
///
/// Order inside `stbl` follows the §8.5.1 "These tables… should
/// appear in the order shown" recommendation: media metadata first
/// (`stsd`, `stts`, `stss`, `stsc`, `stsz`, `stco`/`co64`), then the
/// optional sample-group boxes. We emit all `sgpd` boxes before any
/// `sbgp` / `csgp` so the description tables that an `sbgp` / `csgp`
/// references are declared first; this is not mandated by §8.9 but is
/// the recommended order in §8.5.1 and what every spec example uses.
/// `csgp` follows `sbgp`; the two are alternative encodings of the same
/// per-sample → group mapping, so for any one `grouping_type` a caller
/// supplies exactly one form (§8.9.5).
fn build_stbl_with_sample_groups(
    t: &TrackState,
    sbgp: &[&SampleToGroup],
    sgpd: &[&SampleGroupDescription],
    csgp: &[&CompactSampleToGroup],
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&build_stsd(t));
    body.extend_from_slice(&build_stts(&t.stts));
    if !t.keyframes.is_empty() {
        body.extend_from_slice(&build_stss(&t.keyframes));
    }
    body.extend_from_slice(&build_stsc(&t.stsc));
    body.extend_from_slice(&build_stsz(&t.sample_sizes));
    body.extend_from_slice(&build_chunk_offset_box(&t.chunk_offsets));
    // ISO/IEC 14496-12 §8.9: sample groups. `sgpd` first (description
    // table), then `sbgp` (per-sample index into the description), then
    // `csgp` (the compact bit-packed alternative to `sbgp`).
    for sg in sgpd {
        body.extend_from_slice(&build_sgpd(sg));
    }
    for sb in sbgp {
        body.extend_from_slice(&build_sbgp(sb));
    }
    for cg in csgp {
        body.extend_from_slice(&build_csgp(cg));
    }
    Ok(wrap_box(b"stbl", &body))
}

fn build_stsd(t: &TrackState) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
                                                 // The sample entry is itself a box of type = `fourcc`, size = 8 + body.len().
    body.extend_from_slice(&wrap_box(&t.sample_entry.fourcc, &t.sample_entry.body));
    wrap_box(b"stsd", &body)
}

fn build_stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + runs.len() * 8);
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&delta.to_be_bytes());
    }
    wrap_box(b"stts", &body)
}

fn build_stss(keyframes: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + keyframes.len() * 4);
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&(keyframes.len() as u32).to_be_bytes());
    for k in keyframes {
        body.extend_from_slice(&k.to_be_bytes());
    }
    wrap_box(b"stss", &body)
}

fn build_stsc(runs: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + runs.len() * 12);
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (fc, spc, sdi) in runs {
        body.extend_from_slice(&fc.to_be_bytes());
        body.extend_from_slice(&spc.to_be_bytes());
        body.extend_from_slice(&sdi.to_be_bytes());
    }
    wrap_box(b"stsc", &body)
}

fn build_stsz(sizes: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(12 + sizes.len() * 4);
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
                                           // If all sizes are the same, use uniform. Otherwise 0 + per-sample table.
    let uniform = if !sizes.is_empty() && sizes.iter().all(|&s| s == sizes[0]) {
        sizes[0]
    } else {
        0
    };
    body.extend_from_slice(&uniform.to_be_bytes());
    body.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    if uniform == 0 {
        for s in sizes {
            body.extend_from_slice(&s.to_be_bytes());
        }
    }
    wrap_box(b"stsz", &body)
}

fn build_chunk_offset_box(offsets: &[u64]) -> Vec<u8> {
    // Auto-promote to co64 if any offset exceeds u32 range.
    let needs_64 = offsets.iter().any(|&o| o > u32::MAX as u64);
    if needs_64 {
        let mut body = Vec::with_capacity(8 + offsets.len() * 8);
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            body.extend_from_slice(&o.to_be_bytes());
        }
        wrap_box(b"co64", &body)
    } else {
        let mut body = Vec::with_capacity(8 + offsets.len() * 4);
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            body.extend_from_slice(&(*o as u32).to_be_bytes());
        }
        wrap_box(b"stco", &body)
    }
}

// --- Box utilities --------------------------------------------------------

/// Wrap a body into a box with a 32-bit size header. Returns the full box bytes.
pub(crate) fn wrap_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

// --- Helpers --------------------------------------------------------------

pub(crate) fn default_samples_per_chunk(stream: &StreamInfo) -> u32 {
    // Target roughly 1 second per chunk. For PCM we emit large packets; for
    // compressed codecs samples are ~20ms each.
    match stream.params.media_type {
        MediaType::Audio => {
            // 1 chunk per ~50 samples is a common ffmpeg-ish default for
            // compressed audio. For PCM (single huge packet), 1 sample/chunk
            // is fine.
            if stream.params.codec_id.as_str().starts_with("pcm_") {
                1
            } else {
                50
            }
        }
        MediaType::Video => 1,
        _ => 1,
    }
}

fn compute_delta(t: &TrackState, packet: &Packet, pts_in_ts: Option<i64>) -> u32 {
    // Preferred: use packet.duration if present, rescaled to media timescale.
    if let Some(d) = packet.duration {
        if d > 0 {
            let v = rescale_to_media_ts(d, packet.time_base, t.media_time_scale);
            if v > 0 {
                return v as u32;
            }
        }
    }
    // Otherwise: diff against previous packet's PTS.
    if let (Some(prev), Some(cur)) = (t.prev_pts_in_ts, pts_in_ts) {
        let d = (cur - prev).max(0);
        if d > 0 {
            return d as u32;
        }
    }
    // Fallback: a sensible default based on codec. For audio, use 1 sample
    // (1 tick at sample_rate time scale, which is wrong but better than 0).
    1
}

pub(crate) fn rescale_to_media_ts(value: i64, from_tb: oxideav_core::TimeBase, to_ts: u32) -> i64 {
    let from_r = from_tb.as_rational();
    if from_r.den == 0 || to_ts == 0 {
        return value;
    }
    // value * (from.num/from.den) in seconds, * to_ts in ticks.
    let num = from_r.num as i128 * to_ts as i128;
    let den = from_r.den as i128;
    let prod = value as i128 * num;
    (prod / den) as i64
}

pub(crate) fn rescale_u64(value: u64, from_ts: u32, to_ts: u32) -> u64 {
    if from_ts == 0 {
        return value;
    }
    let prod = value as u128 * to_ts as u128;
    (prod / from_ts as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters};

    #[test]
    fn wrap_box_header_layout() {
        let b = wrap_box(b"test", &[1, 2, 3]);
        assert_eq!(&b[0..4], &[0, 0, 0, 11]);
        assert_eq!(&b[4..8], b"test");
        assert_eq!(&b[8..], &[1, 2, 3]);
    }

    #[test]
    fn chunk_offset_auto_promotes_to_co64() {
        let offsets = [(u32::MAX as u64) + 1];
        let b = build_chunk_offset_box(&offsets);
        assert_eq!(&b[4..8], b"co64");
    }

    #[test]
    fn chunk_offset_stays_stco_when_small() {
        let offsets = [42u64, 1234, 999_999];
        let b = build_chunk_offset_box(&offsets);
        assert_eq!(&b[4..8], b"stco");
    }

    #[test]
    fn pack_language_und() {
        assert_eq!(pack_language(b"und"), ((21u16) << 10) | ((14u16) << 5) | 4);
    }

    fn track_with_first_pts(first_pts: Option<i64>, media_time_scale: u32) -> TrackState {
        let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
        params.sample_rate = Some(media_time_scale);
        params.channels = Some(2);
        let stream = StreamInfo {
            index: 0,
            time_base: oxideav_core::TimeBase::new(1, media_time_scale as i64),
            duration: None,
            start_time: None,
            params,
        };
        let entry = SampleEntry {
            fourcc: *b"sowt",
            body: Vec::new(),
        };
        let mut t = TrackState::new(stream, entry);
        t.media_time_scale = media_time_scale;
        t.first_pts_in_ts = first_pts;
        t.cumulative_duration = media_time_scale as u64; // 1 second of media.
        t
    }

    #[test]
    fn edts_absent_when_first_pts_zero() {
        let t = track_with_first_pts(Some(0), 48_000);
        assert!(build_edts(&t, 1000, 1000).is_none());
        let none = track_with_first_pts(None, 48_000);
        assert!(build_edts(&none, 1000, 1000).is_none());
    }

    #[test]
    fn edts_v0_layout_for_positive_delay() {
        // first_pts = 24_000 ticks at 48 kHz = 0.5 s → 500 ticks at movie ts 1000.
        let t = track_with_first_pts(Some(24_000), 48_000);
        let track_dur_movie = 1000u64;
        let edts = build_edts(&t, 1000, track_dur_movie).expect("edts emitted");
        // edts wraps elst.
        assert_eq!(&edts[4..8], b"edts");
        let elst = &edts[8..];
        assert_eq!(&elst[4..8], b"elst");
        let body = &elst[8..];
        assert_eq!(body[0], 0, "version 0 for sub-32-bit fields");
        let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        assert_eq!(entry_count, 2);
        // Entry 1: empty edit. v0 entry = 4+4+4 bytes.
        let e1 = &body[8..20];
        let seg1 = u32::from_be_bytes([e1[0], e1[1], e1[2], e1[3]]);
        let mt1 = i32::from_be_bytes([e1[4], e1[5], e1[6], e1[7]]);
        let rate1 = u32::from_be_bytes([e1[8], e1[9], e1[10], e1[11]]);
        assert_eq!(seg1, 500, "0.5 s delay at movie timescale 1000");
        assert_eq!(mt1, -1, "empty edit");
        assert_eq!(rate1, 0x0001_0000, "media_rate 1.0");
        // Entry 2: normal edit covering the track duration.
        let e2 = &body[20..32];
        let seg2 = u32::from_be_bytes([e2[0], e2[1], e2[2], e2[3]]);
        let mt2 = i32::from_be_bytes([e2[4], e2[5], e2[6], e2[7]]);
        assert_eq!(seg2, track_dur_movie as u32);
        assert_eq!(mt2, 0, "non-empty edit starts at media time 0");
    }

    #[test]
    fn edts_v1_layout_for_large_duration() {
        // A track duration beyond u32 forces version 1 (64-bit fields).
        let mut t = track_with_first_pts(Some(1000), 1000);
        t.media_time_scale = 1000;
        let big_dur = (u32::MAX as u64) + 10;
        let edts = build_edts(&t, 1000, big_dur).expect("edts emitted");
        let body = &edts[8..][8..];
        assert_eq!(body[0], 1, "version 1 for >32-bit duration");
        // v1 entry = 8+8+4 bytes; entry 2 segment_duration sits at offset 8+20.
        let off = 8 + 20;
        let seg2 = u64::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
            body[off + 4],
            body[off + 5],
            body[off + 6],
            body[off + 7],
        ]);
        assert_eq!(seg2, big_dur);
    }
}
