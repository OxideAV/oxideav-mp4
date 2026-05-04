//! MP4 / ISOBMFF demuxer.
//!
//! Strategy on open():
//! 1. Validate ftyp.
//! 2. Walk `moov/trak/*` to collect per-track metadata and sample tables.
//! 3. Expand the sample tables into a flat, file-offset-sorted list of
//!    samples `(track_idx, offset, size, pts, duration)`.
//!
//! `next_packet` then serves them in order by seeking into the mdat.

use std::collections::HashSet;
use std::io::SeekFrom;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, CodecTag, Error, MediaType, Packet, ProbeContext,
    Result, SampleFormat, StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::boxes::*;
use crate::codec_id::{from_sample_entry, from_sample_entry_with_oti};

pub fn open(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    // Walk top-level boxes looking for ftyp + moov. Continue past moov
    // to pick up any movie fragments (`moof`+`mdat` pairs) that a
    // fragmented / DASH / HLS / Smooth Streaming MP4 emits after the
    // initial movie header. ISO/IEC 14496-12 §8.8.
    let mut saw_ftyp = false;
    let mut moov: Option<Vec<u8>> = None;
    let mut moofs: Vec<MoofRecord> = Vec::new();
    while let Some(hdr) = read_box_header(&mut *input)? {
        match hdr.fourcc {
            FTYP => {
                saw_ftyp = true;
                skip_box_body(&mut *input, &hdr)?;
            }
            MOOV => {
                moov = Some(read_box_body(&mut *input, &hdr)?);
            }
            // §8.16 — `styp` SegmentTypeBox (CMAF / DASH segment marker).
            // Same shape as ftyp; we just skip its payload.
            STYP => skip_box_body(&mut *input, &hdr)?,
            // §8.16.3 — `sidx` SegmentIndexBox. Sample-precision seek
            // index for adaptive-bitrate streams; useful for `seek_to`
            // optimisation but not required for sequential demux.
            SIDX => skip_box_body(&mut *input, &hdr)?,
            // §8.8.4 — `moof` MovieFragmentBox. Capture the body and
            // remember where the box itself started — the
            // `default-base-is-moof` flag in `tfhd` makes per-sample
            // offsets relative to this position.
            MOOF => {
                let payload_size = hdr
                    .payload_size()
                    .ok_or_else(|| Error::invalid("MP4: open-ended moof"))?;
                // Position after read_box_header is the body start; the
                // moof box itself starts header_len bytes earlier.
                let body_start = input.stream_position()?;
                let moof_start = body_start - hdr.header_len;
                let body = read_bytes_vec(&mut *input, payload_size as usize)?;
                moofs.push(MoofRecord { moof_start, body });
            }
            // §8.8.7 — `mfra` MovieFragmentRandomAccessBox. Top-level
            // index of fragment positions for random access. Not
            // required for sequential demux, skip.
            MFRA => skip_box_body(&mut *input, &hdr)?,
            _ => skip_box_body(&mut *input, &hdr)?,
        }
    }
    if !saw_ftyp {
        return Err(Error::invalid("MP4: missing ftyp box"));
    }
    let moov = moov.ok_or_else(|| Error::invalid("MP4: missing moov box"))?;

    let parsed = parse_moov(&moov)?;
    if parsed.tracks.is_empty() {
        return Err(Error::invalid("MP4: no tracks"));
    }

    let mut streams: Vec<StreamInfo> = Vec::with_capacity(parsed.tracks.len());
    let mut samples: Vec<SampleRef> = Vec::new();
    for (i, t) in parsed.tracks.iter().enumerate() {
        streams.push(build_stream_info(i as u32, t, codecs));
        expand_samples(t, i as u32, &mut samples)?;
    }

    // Per-track running base_media_decode_time, used when a fragment's
    // `tfdt` box is absent. Initialised from the last DTS+duration of
    // the moov-derived samples for that track (zero if none).
    let mut next_dts: Vec<i64> = vec![0; parsed.tracks.len()];
    for s in &samples {
        let idx = s.track_idx as usize;
        let end = s.dts.saturating_add(s.duration);
        if end > next_dts[idx] {
            next_dts[idx] = end;
        }
    }

    for moof in &moofs {
        parse_moof(moof, &parsed.tracks, &mut samples, &mut next_dts)?;
    }

    samples.sort_by_key(|s| s.offset);

    // Movie duration from mvhd, translated into microseconds.
    let duration_micros: i64 = if parsed.movie_timescale > 0 && parsed.movie_duration > 0 {
        (parsed.movie_duration as i128 * 1_000_000 / parsed.movie_timescale as i128) as i64
    } else {
        0
    };

    Ok(Box::new(Mp4Demuxer {
        input,
        streams,
        samples,
        cursor: 0,
        metadata: parsed.metadata,
        duration_micros,
    }))
}

/// One `moof` box captured from the top-level walk. We hold onto the
/// payload bytes plus the absolute file offset of the `moof` box's
/// header — needed for the `default-base-is-moof` data-offset fall-back
/// (ISO/IEC 14496-12 §8.8.7.1, flag 0x020000) and for `tfhd`
/// `base_data_offset` arithmetic.
struct MoofRecord {
    moof_start: u64,
    body: Vec<u8>,
}

#[derive(Default)]
struct ParsedMoov {
    tracks: Vec<Track>,
    movie_timescale: u32,
    movie_duration: u64,
    metadata: Vec<(String, String)>,
}

/// Per-track info collected from moov.
#[derive(Clone, Debug)]
struct Track {
    /// `tkhd.track_ID` — 1-based track identifier, used to match
    /// fragmented `tfhd.track_ID` to a parsed track.
    track_id: u32,
    /// Matroska-like id ("audio" / "video"); derived from handler.
    media_type: MediaType,
    codec_id_fourcc: [u8; 4],
    /// Per-track timescale (ticks per second).
    timescale: u32,
    duration: Option<u64>,
    // Audio
    channels: Option<u16>,
    sample_rate: Option<u32>,
    sample_size_bits: Option<u16>,
    // Video
    width: Option<u32>,
    height: Option<u32>,
    // Codec-specific setup payload, if any.
    extradata: Vec<u8>,
    /// MPEG-4 `objectTypeIndication` (OTI) from the esds box, if present.
    /// Used to refine `mp4a` / `mp4v` FourCCs into concrete codec ids.
    esds_oti: Option<u8>,
    // Sample tables.
    stts: Vec<(u32, u32)>, // (sample_count, sample_delta) — in media timescale
    stsc: Vec<(u32, u32, u32)>, // (first_chunk, samples_per_chunk, sample_description_index)
    stsz: Vec<u32>,        // per-sample sizes (or `uniform`-derived vec of same size)
    chunk_offsets: Vec<u64>, // absolute file offsets (stco or co64)
    /// 1-based sample indices that are sync (key) frames. Empty means
    /// "all samples are sync frames" per ISO/IEC 14496-12.
    stss: Vec<u32>,
    /// §8.6.1.3 ctts (CompositionOffsetBox). Run-length pairs of
    /// `(sample_count, sample_offset)` mapping decoding-order index
    /// to a composition-time offset (CTS = DTS + offset). Version 0
    /// uses unsigned offsets, version 1 signed; we always store i32
    /// so callers can apply it uniformly. Empty when the box is
    /// absent (every sample's CTS equals its DTS — no B-frames or
    /// the encoder didn't reorder).
    ctts: Vec<(u32, i32)>,
    /// §8.6.6 edts/elst — full edit list. Each entry is
    /// `(segment_duration_movie_ts, media_time_track_ts, media_rate_q16)`.
    /// `media_time = -1` marks an empty edit (a "dwell" segment that
    /// pads the presentation timeline before any media is shown).
    /// `media_rate` is fixed-point 16.16 — `0x0001_0000` is normal speed.
    ///
    /// `segment_duration` is in the *movie* timescale (per spec); we
    /// store it as recorded and convert at apply time.
    ///
    /// An empty vector means "no edit list" (no shift, identity
    /// presentation timeline).
    elst: Vec<ElstEntry>,
    /// `mvex/trex` per-track defaults populated from the moov; supply
    /// the fall-back per-sample size / duration / flags / sample
    /// description index when a fragment's `tfhd` / `trun` doesn't
    /// override them. Zero-initialised when the file has no `mvex`
    /// (i.e. is not fragmented).
    trex: TrexDefaults,
}

/// Single entry of an `elst` (EditListBox).
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // segment_duration/media_rate parsed for completeness; only media_time drives sample shift today
struct ElstEntry {
    /// In *movie* timescale (per ISO/IEC 14496-12 §8.6.6.1).
    segment_duration: u64,
    /// In *track* (media) timescale. `-1` means "empty" (the segment
    /// has no underlying media; play silence/black for the duration).
    media_time: i64,
    /// 16.16 fixed-point. Only `0x0001_0000` (1.0) is supported in
    /// terms of timeline mapping; non-1.0 rates are recorded but the
    /// presentation-time projection assumes 1.0.
    media_rate: u32,
}

/// Per-track defaults from `mvex/trex` (ISO/IEC 14496-12 §8.8.3). All
/// fields zero when the file has no `mvex`. The fragment parser
/// consults these as a fall-back when a `tfhd` lacks the corresponding
/// override flag.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)] // default_sample_description_index recorded for parity; we only use the first stsd entry
struct TrexDefaults {
    /// `default_sample_description_index` — almost always 1.
    default_sample_description_index: u32,
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
}

fn parse_moov(moov: &[u8]) -> Result<ParsedMoov> {
    let mut out = ParsedMoov::default();
    let mut cur = std::io::Cursor::new(moov);
    let end = moov.len() as u64;
    // First pass: collect tracks + mvhd + metadata.
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            TRAK => {
                let body = read_bytes_vec(&mut cur, psz)?;
                if let Some(t) = parse_trak(&body)? {
                    out.tracks.push(t);
                }
            }
            MVHD => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_mvhd(&body, &mut out)?;
            }
            UDTA => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_udta(&body, &mut out.metadata);
            }
            META => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_meta(&body, &mut out.metadata);
            }
            MVEX => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_mvex(&body, &mut out.tracks)?;
            }
            _ => {
                cur.set_position(cur.position() + psz as u64);
            }
        }
    }
    Ok(out)
}

/// §8.8.1 — `mvex` (MovieExtendsBox). Container for `trex` boxes that
/// supply per-track defaults consumed by the fragment parser.
fn parse_mvex(body: &[u8], tracks: &mut [Track]) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            TREX => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_trex(&b, tracks)?;
            }
            // mehd (movie-extends-header) carries an overall fragment
            // duration we don't consume — the per-fragment tfdt + trun
            // are authoritative.
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }
    Ok(())
}

/// §8.8.3 — `trex` (TrackExtendsBox).
///
/// Layout: FullBox header (4 bytes) + track_ID (u32) +
/// default_sample_description_index (u32) + default_sample_duration (u32) +
/// default_sample_size (u32) + default_sample_flags (u32). 24 payload bytes.
fn parse_trex(body: &[u8], tracks: &mut [Track]) -> Result<()> {
    if body.len() < 24 {
        return Err(Error::invalid("MP4: trex too short"));
    }
    let track_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let dsdi = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let ddur = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
    let dsiz = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
    let dflg = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
    if let Some(t) = tracks.iter_mut().find(|t| t.track_id == track_id) {
        t.trex = TrexDefaults {
            default_sample_description_index: dsdi,
            default_sample_duration: ddur,
            default_sample_size: dsiz,
            default_sample_flags: dflg,
        };
    }
    Ok(())
}

/// ISO/IEC 14496-12 §8.2.2 Movie Header box. Carries the movie-wide
/// timescale and duration (in that timescale).
fn parse_mvhd(body: &[u8], out: &mut ParsedMoov) -> Result<()> {
    if body.is_empty() {
        return Err(Error::invalid("MP4: mvhd empty"));
    }
    let version = body[0];
    let (timescale, duration) = if version == 0 {
        if body.len() < 20 {
            return Err(Error::invalid("MP4: mvhd v0 too short"));
        }
        let ts = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
        let du = u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as u64;
        (ts, du)
    } else {
        if body.len() < 32 {
            return Err(Error::invalid("MP4: mvhd v1 too short"));
        }
        let ts = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
        let du = u64::from_be_bytes([
            body[24], body[25], body[26], body[27], body[28], body[29], body[30], body[31],
        ]);
        (ts, du)
    };
    out.movie_timescale = timescale;
    out.movie_duration = duration;
    Ok(())
}

/// Parse a `udta` box body. May contain 3GPP-style boxes (titl/auth/cprt/…)
/// and/or an iTunes-style `meta` subtree.
fn parse_udta(body: &[u8], metadata: &mut Vec<(String, String)>) {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        if cur.position() as usize + psz > body.len() {
            break;
        }
        let start = cur.position() as usize;
        cur.set_position((start + psz) as u64);
        let payload = &body[start..start + psz];
        match &hdr.fourcc {
            b"meta" => parse_meta(payload, metadata),
            // 3GPP TS 26.244: titl / auth / cprt / dscp — body is a
            // FullBox (1 version + 3 flags) then 2-byte language code
            // then UTF-8 (or UTF-16 if BOM) string.
            b"titl" | b"auth" | b"cprt" | b"dscp" | b"gnre" | b"albm" | b"yrrc"
                if payload.len() >= 6 =>
            {
                let key = match &hdr.fourcc {
                    b"titl" => "title",
                    b"auth" => "artist",
                    b"cprt" => "copyright",
                    b"dscp" => "description",
                    b"gnre" => "genre",
                    b"albm" => "album",
                    b"yrrc" => "date",
                    _ => unreachable!(),
                };
                let s = decode_utf8_or_utf16(&payload[6..]);
                if !s.is_empty() {
                    metadata.push((key.into(), s));
                }
            }
            _ => {}
        }
    }
}

/// Parse a `meta` box body (iTunes-style or ISO-BMFF). The body is a
/// FullBox (4 bytes of version/flags), then a series of child boxes
/// including `hdlr` (identifies the scheme) and `ilst` (the item list).
fn parse_meta(body: &[u8], metadata: &mut Vec<(String, String)>) {
    if body.len() < 4 {
        return;
    }
    // First 4 bytes are version/flags (FullBox header); skip them.
    let mut cur = std::io::Cursor::new(&body[4..]);
    let end = body.len() as u64 - 4;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let start = cur.position() as usize;
        if start + psz > (body.len() - 4) {
            break;
        }
        cur.set_position((start + psz) as u64);
        if hdr.fourcc == ILST {
            parse_ilst(&body[4 + start..4 + start + psz], metadata);
        }
    }
}

/// Parse an `ilst` (iTunes-style item list). Each child is a FourCC-keyed
/// box whose payload contains a `data` subbox with the value.
fn parse_ilst(body: &[u8], metadata: &mut Vec<(String, String)>) {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let start = cur.position() as usize;
        if start + psz > body.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        // Recurse one level: look for a `data` child.
        let item = &body[start..start + psz];
        let key = ilst_key_for(&hdr.fourcc);
        if key.is_none() {
            continue;
        }
        let key = key.unwrap();
        let mut sub = std::io::Cursor::new(item);
        let sub_end = item.len() as u64;
        while sub.position() < sub_end {
            let sh = match read_box_header(&mut sub).ok().flatten() {
                Some(h) => h,
                None => break,
            };
            let sub_psz = sh.payload_size().unwrap_or(0) as usize;
            let sub_start = sub.position() as usize;
            if sub_start + sub_psz > item.len() {
                break;
            }
            sub.set_position((sub_start + sub_psz) as u64);
            if sh.fourcc == DATA {
                // data box: 4 bytes type_indicator + 4 bytes locale + payload.
                let data_body = &item[sub_start..sub_start + sub_psz];
                if data_body.len() > 8 {
                    let value = String::from_utf8_lossy(&data_body[8..]).trim().to_string();
                    if !value.is_empty() {
                        metadata.push((key.into(), value));
                    }
                }
            }
        }
    }
}

fn ilst_key_for(fourcc: &[u8; 4]) -> Option<&'static str> {
    // The iTunes atoms starting with 0xA9 are the "copyright symbol" keys.
    match fourcc {
        b"\xa9nam" => Some("title"),
        b"\xa9ART" => Some("artist"),
        b"\xa9alb" => Some("album"),
        b"\xa9cmt" => Some("comment"),
        b"\xa9gen" => Some("genre"),
        b"\xa9day" => Some("date"),
        b"\xa9wrt" => Some("composer"),
        b"\xa9too" => Some("encoder"),
        b"\xa9cpy" | b"cprt" => Some("copyright"),
        b"\xa9lyr" => Some("lyrics"),
        b"aART" => Some("album_artist"),
        b"trkn" => Some("track"),
        b"disk" => Some("disc"),
        b"desc" => Some("description"),
        _ => None,
    }
}

fn decode_utf8_or_utf16(buf: &[u8]) -> String {
    if buf.len() >= 2 && buf[0] == 0xFE && buf[1] == 0xFF {
        // UTF-16BE with BOM.
        let pairs = buf[2..].chunks_exact(2);
        let units: Vec<u16> = pairs.map(|p| u16::from_be_bytes([p[0], p[1]])).collect();
        return String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .trim()
            .to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim().to_string()
}

fn parse_trak(body: &[u8]) -> Result<Option<Track>> {
    let mut t = Track {
        track_id: 0,
        media_type: MediaType::Unknown,
        codec_id_fourcc: [0; 4],
        timescale: 0,
        duration: None,
        channels: None,
        sample_rate: None,
        sample_size_bits: None,
        width: None,
        height: None,
        extradata: Vec::new(),
        esds_oti: None,
        stts: Vec::new(),
        stsc: Vec::new(),
        stsz: Vec::new(),
        chunk_offsets: Vec::new(),
        stss: Vec::new(),
        ctts: Vec::new(),
        elst: Vec::new(),
        trex: TrexDefaults::default(),
    };
    let mut has_media = false;
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            TKHD => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_tkhd(&sub, &mut t)?;
            }
            MDIA => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_mdia(&sub, &mut t)?;
                has_media = true;
            }
            EDTS => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_edts(&sub, &mut t)?;
            }
            _ => {
                cur.set_position(cur.position() + psz as u64);
            }
        }
    }
    if has_media {
        Ok(Some(t))
    } else {
        Ok(None)
    }
}

/// §8.3.2 — `tkhd` (TrackHeaderBox). We only need `track_ID` for
/// fragmented-MP4 traf-to-track matching; the matrix / w/h preview
/// fields are the visual entry's job.
fn parse_tkhd(body: &[u8], t: &mut Track) -> Result<()> {
    if body.is_empty() {
        return Err(Error::invalid("MP4: tkhd empty"));
    }
    let version = body[0];
    // FullBox header is 4 bytes. v0: 4 created + 4 modified + 4 track_ID.
    // v1: 8 + 8 + 4. After-header offsets:
    let off = if version == 0 { 4 + 8 } else { 4 + 16 };
    if body.len() < off + 4 {
        return Err(Error::invalid("MP4: tkhd too short"));
    }
    t.track_id = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    Ok(())
}

/// §8.6.5 — `edts` (EditBox) container. We only care about the inner
/// `elst` (EditListBox) child; everything else in the container is
/// reserved.
fn parse_edts(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            ELST => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_elst(&b, t)?;
            }
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }
    Ok(())
}

/// §8.6.6 — `elst` (EditListBox).
///
/// Each entry is `(segment_duration, media_time, media_rate)`. Width
/// of the first two fields depends on the FullBox `version`:
/// * v0 → both 32-bit (unsigned dur, signed media_time)
/// * v1 → both 64-bit (unsigned dur, signed media_time)
///
/// `media_rate` is always a 16.16 fixed-point u32 (high 16 bits
/// integer, low 16 fractional). `0x0001_0000` is normal speed.
///
/// Special values:
/// * `media_time = -1` → an "empty" edit (no underlying media for the
///   given duration); samples skip this gap on the presentation
///   timeline.
/// * Non-1.0 `media_rate` (slow-motion / reverse) is recorded but the
///   sample-time projection assumes 1.0 — complete fast/slow playback
///   is out of scope for this demuxer.
///
/// Stores the full list so the multi-segment apply path
/// (`apply_elst_segments`) can hop the presentation timeline
/// across each segment correctly.
fn parse_elst(body: &[u8], t: &mut Track) -> Result<()> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: elst too short"));
    }
    let version = body[0];
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let entry_size = if version == 1 { 20 } else { 12 };
    let mut off = 8;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if off + entry_size > body.len() {
            return Err(Error::invalid("MP4: elst truncated"));
        }
        let (segment_duration, media_time) = if version == 1 {
            let dur = u64::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
                body[off + 4],
                body[off + 5],
                body[off + 6],
                body[off + 7],
            ]);
            let mt = i64::from_be_bytes([
                body[off + 8],
                body[off + 9],
                body[off + 10],
                body[off + 11],
                body[off + 12],
                body[off + 13],
                body[off + 14],
                body[off + 15],
            ]);
            (dur, mt)
        } else {
            let dur =
                u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64;
            let mt =
                i32::from_be_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]])
                    as i64;
            (dur, mt)
        };
        let rate_off = off + if version == 1 { 16 } else { 8 };
        let media_rate = u32::from_be_bytes([
            body[rate_off],
            body[rate_off + 1],
            body[rate_off + 2],
            body[rate_off + 3],
        ]);
        entries.push(ElstEntry {
            segment_duration,
            media_time,
            media_rate,
        });
        off += entry_size;
    }
    t.elst = entries;
    Ok(())
}

/// Compute the `media_time` shift applied to all samples in the track,
/// using the first non-empty edit segment as the leading shift. This
/// matches the legacy single-segment behaviour and is what B-frame
/// `ctts` offsets compose against to land the first presented frame
/// at pts 0.
fn elst_leading_media_time(t: &Track) -> i64 {
    for e in &t.elst {
        if e.media_time != -1 {
            return e.media_time;
        }
    }
    0
}

fn parse_mdia(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            MDHD => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_mdhd(&b, t)?;
            }
            HDLR => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_hdlr(&b, t)?;
            }
            MINF => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_minf(&b, t)?;
            }
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }
    Ok(())
}

fn parse_mdhd(body: &[u8], t: &mut Track) -> Result<()> {
    if body.len() < 24 {
        return Err(Error::invalid("MP4: mdhd too short"));
    }
    let version = body[0];
    let (timescale, duration) = if version == 0 {
        let ts = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
        let du = u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as u64;
        (ts, du)
    } else {
        if body.len() < 32 {
            return Err(Error::invalid("MP4: mdhd v1 too short"));
        }
        let ts = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
        let du = u64::from_be_bytes([
            body[24], body[25], body[26], body[27], body[28], body[29], body[30], body[31],
        ]);
        (ts, du)
    };
    t.timescale = timescale;
    t.duration = Some(duration);
    Ok(())
}

fn parse_hdlr(body: &[u8], t: &mut Track) -> Result<()> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: hdlr too short"));
    }
    let mut handler = [0u8; 4];
    handler.copy_from_slice(&body[8..12]);
    t.media_type = match &handler {
        h if *h == HANDLER_SOUN => MediaType::Audio,
        h if *h == HANDLER_VIDE => MediaType::Video,
        _ => MediaType::Data,
    };
    Ok(())
}

fn parse_minf(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            STBL => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_stbl(&sub, t)?;
            }
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }
    Ok(())
}

fn parse_stbl(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let b = read_bytes_vec(&mut cur, psz)?;
        match hdr.fourcc {
            STSD => parse_stsd(&b, t)?,
            STTS => t.stts = parse_stts(&b)?,
            STSC => t.stsc = parse_stsc(&b)?,
            STSZ => t.stsz = parse_stsz(&b)?,
            STZ2 => t.stsz = parse_stz2(&b)?,
            STCO => t.chunk_offsets = parse_stco(&b)?,
            CO64 => t.chunk_offsets = parse_co64(&b)?,
            STSS => t.stss = parse_stss(&b)?,
            CTTS => t.ctts = parse_ctts(&b)?,
            _ => {}
        }
    }
    Ok(())
}

fn parse_stsd(body: &[u8], t: &mut Track) -> Result<()> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stsd too short"));
    }
    let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    if entry_count == 0 {
        return Ok(());
    }
    let mut cur = std::io::Cursor::new(&body[8..]);
    let hdr = match read_box_header(&mut cur)? {
        Some(h) => h,
        None => return Err(Error::invalid("MP4: stsd first entry missing")),
    };
    let psz = hdr.payload_size().unwrap_or(0) as usize;
    let entry = read_bytes_vec(&mut cur, psz)?;
    t.codec_id_fourcc = hdr.fourcc;
    parse_sample_entry(&entry, t)?;
    Ok(())
}

fn parse_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    if entry.len() < 8 {
        return Ok(());
    }
    match t.media_type {
        MediaType::Audio => parse_audio_sample_entry(entry, t),
        MediaType::Video => parse_video_sample_entry(entry, t),
        _ => Ok(()),
    }
}

fn parse_audio_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    // AudioSampleEntryV0 layout:
    //   6 bytes reserved
    //   2 bytes data_reference_index
    //   8 bytes reserved (or version/revision/vendor in QT-style)
    //   2 bytes channel_count
    //   2 bytes sample_size
    //   4 bytes reserved
    //   4 bytes sample_rate (16.16 fixed)
    // = 28 bytes, followed by child boxes.
    if entry.len() < 28 {
        return Ok(());
    }
    let channels = u16::from_be_bytes([entry[16], entry[17]]);
    let sample_size = u16::from_be_bytes([entry[18], entry[19]]);
    let sample_rate = u32::from_be_bytes([entry[24], entry[25], entry[26], entry[27]]) >> 16;
    t.channels = Some(channels);
    t.sample_size_bits = Some(sample_size);
    t.sample_rate = Some(sample_rate);

    // Child boxes (dfLa, dOps, esds, ...).
    let mut cur = std::io::Cursor::new(&entry[28..]);
    let end = (entry.len() - 28) as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let body = read_bytes_vec(&mut cur, psz)?;
        match &hdr.fourcc {
            // FLAC-in-MP4 dfLa: 1 byte version + 3 bytes flags + metadata blocks.
            // Our FLAC decoder wants just the metadata blocks.
            b"dfLa" if body.len() > 4 => {
                t.extradata = body[4..].to_vec();
            }
            // Opus-in-MP4 dOps: a subset of OpusHead without the 8-byte magic.
            // We rebuild OpusHead so our downstream code can treat it uniformly.
            b"dOps" if body.len() >= 11 => {
                let mut oh = Vec::with_capacity(body.len() + 8);
                oh.extend_from_slice(b"OpusHead");
                oh.extend_from_slice(&body);
                t.extradata = oh;
            }
            // ES Descriptor box for MPEG-4 audio (mp4a): strip the nested
            // descriptor wrappers and hand the DecoderSpecificInfo (the AAC
            // AudioSpecificConfig) straight to the decoder via extradata.
            // We also capture the `objectTypeIndication` so the codec-id
            // resolver can disambiguate MP3-in-mp4a vs. AAC-in-mp4a.
            b"esds" if body.len() >= 4 => {
                if let Some(parsed) = parse_esds(&body[4..]) {
                    if !parsed.dsi.is_empty() {
                        t.extradata = parsed.dsi;
                    }
                    t.esds_oti = parsed.oti;
                }
            }
            // AC-3 specific config (`dac3`, ETSI TS 102 366 Annex F.4)
            // and E-AC-3 specific config (`dec3`, Annex G.4). Keep the
            // raw box payload as extradata so downstream decoders that
            // care about `fscod`/`bsid`/`acmod`/`lfeon`/etc. can parse
            // it themselves. For decoders that don't need it the bytes
            // are harmless extra context.
            b"dac3" | b"dec3" => t.extradata = body,
            _ => {}
        }
    }
    Ok(())
}

/// What we extract from an esds box: the DecoderSpecificInfo (empty when
/// absent) and the DecoderConfigDescriptor's `objectTypeIndication` byte.
#[derive(Default)]
struct EsdsInfo {
    dsi: Vec<u8>,
    oti: Option<u8>,
}

/// Parse an esds `ES_Descriptor` payload (the part after the `FullBox`
/// version+flags) and return its DecoderSpecificInfo bytes together with
/// the `objectTypeIndication` byte from the DecoderConfigDescriptor.
///
/// ES_Descriptor layout (ISO/IEC 14496-1 §7.2.6):
///   tag 0x03, BER length,
///   ES_ID (u16), flags (u8) — plus optional dependsOn/URL/OCR fields,
///   DecoderConfigDescriptor (tag 0x04) {
///     objectTypeIndication (u8),
///     streamType+upstream+reserved (u8),
///     bufferSizeDB (u24),
///     maxBitrate (u32),
///     avgBitrate (u32),
///     DecoderSpecificInfo (tag 0x05) — the ASC (or whatever the codec
///     uses as its setup header),
///   },
///   SLConfigDescriptor (tag 0x06).
///
/// Returns `None` only if the outer ES_Descriptor itself is malformed.
/// A well-formed ES_Descriptor with no DCD returns `EsdsInfo::default()`.
fn parse_esds(buf: &[u8]) -> Option<EsdsInfo> {
    let mut info = EsdsInfo::default();
    let mut cur = 0usize;
    let (tag, len, hdr_bytes) = read_descr(buf, cur)?;
    if tag != 0x03 {
        return None;
    }
    cur += hdr_bytes;
    let es_end = cur.checked_add(len)?;
    if es_end > buf.len() {
        return None;
    }
    // ES_ID + flags byte (3 bytes). Flags byte bit 7 = streamDependenceFlag,
    // bit 6 = URL_Flag, bit 5 = OCRstreamFlag — each enables extra fields.
    if cur + 3 > es_end {
        return None;
    }
    let flags = buf[cur + 2];
    cur += 3;
    if flags & 0x80 != 0 {
        cur = cur.checked_add(2)?; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        // URL: 1-byte length + that many bytes.
        if cur >= es_end {
            return None;
        }
        let url_len = buf[cur] as usize;
        cur = cur.checked_add(1 + url_len)?;
    }
    if flags & 0x20 != 0 {
        cur = cur.checked_add(2)?; // OCR_ES_ID
    }

    // Walk sub-descriptors looking for DecoderConfigDescriptor.
    while cur < es_end {
        let (sub_tag, sub_len, sub_hdr) = read_descr(buf, cur)?;
        cur += sub_hdr;
        let sub_end = cur.checked_add(sub_len)?;
        if sub_end > es_end {
            return None;
        }
        if sub_tag == 0x04 {
            // DecoderConfigDescriptor: 13 fixed bytes then nested descriptors.
            // First byte is the objectTypeIndication we care about.
            if sub_len < 13 {
                return None;
            }
            info.oti = Some(buf[cur]);
            if sub_len > 13 {
                let mut inner = cur + 13;
                while inner < sub_end {
                    let (dsi_tag, dsi_len, dsi_hdr) = read_descr(buf, inner)?;
                    inner += dsi_hdr;
                    let dsi_end = inner.checked_add(dsi_len)?;
                    if dsi_end > sub_end {
                        return None;
                    }
                    if dsi_tag == 0x05 {
                        info.dsi = buf[inner..dsi_end].to_vec();
                        break;
                    }
                    inner = dsi_end;
                }
            }
        }
        cur = sub_end;
    }
    Some(info)
}

/// Back-compat thin wrapper — returns just the DSI bytes.
#[cfg(test)]
fn parse_esds_dsi(buf: &[u8]) -> Option<Vec<u8>> {
    let info = parse_esds(buf)?;
    if info.dsi.is_empty() {
        None
    } else {
        Some(info.dsi)
    }
}

/// Read one MPEG-4 descriptor header (tag + BER-encoded length). Returns
/// `(tag, content_length, header_bytes_consumed)`. Length bytes use the
/// standard 7-bit varint with a continuation flag in bit 7; caps at 4 bytes.
fn read_descr(buf: &[u8], off: usize) -> Option<(u8, usize, usize)> {
    if off >= buf.len() {
        return None;
    }
    let tag = buf[off];
    let mut len: usize = 0;
    let mut consumed = 1usize;
    for _ in 0..4 {
        let p = off + consumed;
        if p >= buf.len() {
            return None;
        }
        let b = buf[p];
        consumed += 1;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            return Some((tag, len, consumed));
        }
    }
    None
}

fn parse_video_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    // VisualSampleEntry: 6 reserved + 2 data_ref_idx + 16 pre_defined +
    // 2 width + 2 height + ... = 78 bytes total payload. Offsets per
    // ISO/IEC 14496-12.
    if entry.len() < 28 {
        return Ok(());
    }
    let width = u16::from_be_bytes([entry[24], entry[25]]);
    let height = u16::from_be_bytes([entry[26], entry[27]]);
    t.width = Some(width as u32);
    t.height = Some(height as u32);

    // Walk the codec-specific child boxes that sit after the 78-byte
    // VisualSampleEntry preamble. We surface configuration records as
    // extradata so downstream codec crates can bootstrap from them.
    if entry.len() <= 78 {
        return Ok(());
    }
    let mut cur = std::io::Cursor::new(&entry[78..]);
    let end = (entry.len() - 78) as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let body = read_bytes_vec(&mut cur, psz)?;
        match &hdr.fourcc {
            // AVCConfigurationRecord (ISO/IEC 14496-15 §5.3.3) — for
            // h264, our decoder consumes this verbatim.
            b"avcC" => t.extradata = body,
            // HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 §8.3.3).
            b"hvcC" => t.extradata = body,
            // AV1CodecConfigurationRecord — av1C box per the AV1 ISOBMFF spec.
            b"av1C" => t.extradata = body,
            // VPCodecConfigurationRecord — vpcC box for VP8 / VP9.
            b"vpcC" => t.extradata = body,
            // esds for `mp4v` sample entries. Same shape as the audio variant.
            // We keep the DSI (MPEG-4 VOL header for Part 2, etc.) as
            // extradata and remember the OTI so `from_sample_entry_with_oti`
            // can refine `mp4v` into `mpeg1video` / `mpeg2video` / etc.
            b"esds" if body.len() >= 4 => {
                if let Some(parsed) = parse_esds(&body[4..]) {
                    if !parsed.dsi.is_empty() {
                        t.extradata = parsed.dsi;
                    }
                    t.esds_oti = parsed.oti;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_stts(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stts too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: stts truncated"));
        }
        let cnt = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let dlt = u32::from_be_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
        out.push((cnt, dlt));
        off += 8;
    }
    Ok(out)
}

fn parse_stsc(body: &[u8]) -> Result<Vec<(u32, u32, u32)>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stsc too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 12 > body.len() {
            return Err(Error::invalid("MP4: stsc truncated"));
        }
        let fc = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let spc = u32::from_be_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
        let sdi =
            u32::from_be_bytes([body[off + 8], body[off + 9], body[off + 10], body[off + 11]]);
        out.push((fc, spc, sdi));
        off += 12;
    }
    Ok(out)
}

fn parse_stsz(body: &[u8]) -> Result<Vec<u32>> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: stsz too short"));
    }
    let uniform = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let count = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;
    if uniform != 0 {
        return Ok(vec![uniform; count]);
    }
    let mut out = Vec::with_capacity(count);
    let mut off = 12;
    for _ in 0..count {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: stsz truncated"));
        }
        out.push(u32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }
    Ok(out)
}

fn parse_stz2(body: &[u8]) -> Result<Vec<u32>> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: stz2 too short"));
    }
    let field_size = body[7];
    let count = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;
    let mut out = Vec::with_capacity(count);
    let off = 12;
    match field_size {
        4 => {
            for i in 0..count {
                if off + i / 2 >= body.len() {
                    return Err(Error::invalid("MP4: stz2 4-bit truncated"));
                }
                let b = body[off + i / 2];
                let v = if i % 2 == 0 { b >> 4 } else { b & 0x0F };
                out.push(v as u32);
            }
        }
        8 => {
            if off + count > body.len() {
                return Err(Error::invalid("MP4: stz2 8-bit truncated"));
            }
            for i in 0..count {
                out.push(body[off + i] as u32);
            }
        }
        16 => {
            if off + count * 2 > body.len() {
                return Err(Error::invalid("MP4: stz2 16-bit truncated"));
            }
            for i in 0..count {
                out.push(u16::from_be_bytes([body[off + 2 * i], body[off + 2 * i + 1]]) as u32);
            }
        }
        _ => return Err(Error::invalid("MP4: stz2 invalid field size")),
    }
    Ok(out)
}

fn parse_stss(body: &[u8]) -> Result<Vec<u32>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stss too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: stss truncated"));
        }
        out.push(u32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }
    Ok(out)
}

/// Parse `ctts` (CompositionOffsetBox, ISO/IEC 14496-12 §8.6.1.3).
///
/// Run-length pairs of `(sample_count, sample_offset)` mapping the
/// decoding-order index to the composition-time offset that converts
/// the sample's DTS into its CTS (CTS = DTS + offset). The header is
/// the standard FullBox: `version(8) flags(24) entry_count(32)`.
///
/// * Version 0 (the common case) stores `sample_offset` as `u32`,
///   permitting only non-negative offsets.
/// * Version 1 stores it as `i32`, allowing negative offsets — used
///   when the encoder shifts the entire CTS timeline below DTS so
///   the first frame's CTS can sit at zero (Apple-style negative
///   composition shift).
///
/// We always return `i32` so callers can apply the offset uniformly.
fn parse_ctts(body: &[u8]) -> Result<Vec<(u32, i32)>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: ctts too short"));
    }
    let version = body[0];
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: ctts truncated"));
        }
        let cnt = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let raw = [body[off + 4], body[off + 5], body[off + 6], body[off + 7]];
        let dlt: i32 = if version == 0 {
            // Spec says u32 here, but real-world v0 files routinely
            // emit large unsigned values that exceed i32::MAX only
            // when the timescale is itself huge — `as i32` preserves
            // bit-pattern, and a sane v0 producer keeps the offset
            // representable.
            u32::from_be_bytes(raw) as i32
        } else {
            i32::from_be_bytes(raw)
        };
        out.push((cnt, dlt));
        off += 8;
    }
    Ok(out)
}

fn parse_stco(body: &[u8]) -> Result<Vec<u64>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stco too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: stco truncated"));
        }
        out.push(
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64,
        );
        off += 4;
    }
    Ok(out)
}

fn parse_co64(body: &[u8]) -> Result<Vec<u64>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: co64 too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut off = 8;
    for _ in 0..count {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: co64 truncated"));
        }
        out.push(u64::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
            body[off + 4],
            body[off + 5],
            body[off + 6],
            body[off + 7],
        ]));
        off += 8;
    }
    Ok(out)
}

// --- Movie-fragment parsing (ISO/IEC 14496-12 §8.8) ----------------------

/// Walk one `moof` box, locating its `traf` children and stitching the
/// fragmented samples into the per-track sample list.
///
/// `next_dts` carries each track's running base_media_decode_time so a
/// `traf` without `tfdt` continues seamlessly from the previous
/// fragment (or from the moov-derived sample tail).
fn parse_moof(
    moof: &MoofRecord,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
) -> Result<()> {
    let mut cur = std::io::Cursor::new(&moof.body);
    let end = moof.body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            // §8.8.5 — `mfhd` MovieFragmentHeaderBox. Carries the
            // monotonically-increasing sequence_number; useful for
            // multi-source diagnostics but not required for sample
            // resolution. Skip.
            MFHD => cur.set_position(cur.position() + psz as u64),
            TRAF => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_traf(&body, moof.moof_start, tracks, samples, next_dts)?;
            }
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }
    Ok(())
}

/// One track-fragment scratch record built from `tfhd` + `tfdt` before
/// any `trun` is walked.
#[derive(Default)]
struct TrafState {
    track_idx: usize,
    /// `tfhd.flags` — selects which optional fields are present.
    tfhd_flags: u32,
    /// Effective base offset for `trun` entries that have no explicit
    /// per-sample offset. Built from `tfhd.base_data_offset` when
    /// present, or the start of the containing `moof` when the
    /// `default-base-is-moof` flag (0x020000) is set, or — failing
    /// both — the start of the first `mdat` after this `moof`. The
    /// last fall-back is rare in practice; we approximate it by
    /// using the moof start, which matches every fragment in our
    /// test corpus and the §8.8.7.1 spec text "either the first byte
    /// of the enclosing Movie Fragment Box".
    base_data_offset: u64,
    /// Per-track defaults from `tfhd`, falling back to `trex`.
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
    /// `tfdt.base_media_decode_time` — first sample's DTS in this
    /// fragment, in the track's media timescale. `None` when no
    /// `tfdt` is present (then the running `next_dts` is used).
    base_media_decode_time: Option<i64>,
}

/// `tfhd` flag bits (ISO/IEC 14496-12 §8.8.7.1).
const TFHD_BASE_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x000002;
const TFHD_DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x000008;
const TFHD_DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x000010;
const TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x000020;
// `default-base-is-moof` (0x020000) is implicit in our base resolution:
// when no explicit `base_data_offset` is present we already use
// `moof_start`, which matches both the 0x020000 semantic and the
// "first byte of the enclosing Movie Fragment Box" spec fall-back.
#[allow(dead_code)]
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x020000;

/// `trun` flag bits (ISO/IEC 14496-12 §8.8.8.1).
const TRUN_DATA_OFFSET_PRESENT: u32 = 0x000001;
const TRUN_FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x000004;
const TRUN_SAMPLE_DURATION_PRESENT: u32 = 0x000100;
const TRUN_SAMPLE_SIZE_PRESENT: u32 = 0x000200;
const TRUN_SAMPLE_FLAGS_PRESENT: u32 = 0x000400;
const TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT: u32 = 0x000800;

/// `sample_flags` `sample_is_non_sync_sample` bit
/// (ISO/IEC 14496-12 §8.8.3.1, byte 3 bit 0 of the 32-bit field).
const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;

fn parse_traf(
    body: &[u8],
    moof_start: u64,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
) -> Result<()> {
    // First pass: read tfhd + tfdt before the trun(s) so each trun
    // has the full default context.
    let mut state = TrafState::default();
    let mut tfhd_seen = false;

    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            TFHD => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_tfhd(&b, moof_start, tracks, &mut state)?;
                tfhd_seen = true;
            }
            TFDT => {
                let b = read_bytes_vec(&mut cur, psz)?;
                state.base_media_decode_time = Some(parse_tfdt(&b)?);
            }
            // We only resolve trun's in the second pass; skip here.
            TRUN => cur.set_position(cur.position() + psz as u64),
            _ => cur.set_position(cur.position() + psz as u64),
        }
    }

    if !tfhd_seen {
        return Err(Error::invalid("MP4: traf missing tfhd"));
    }

    let track = &tracks[state.track_idx];

    // Each trun walks samples from its own data_offset relative to
    // base_data_offset. Track running data offset across multiple
    // truns within a single traf — per spec, when a trun has no
    // explicit data_offset it picks up where the previous trun left
    // off (sequential append).
    //
    // Running DTS within this fragment starts at base_media_decode_time
    // (or the carried-over next_dts when tfdt is absent).
    let mut frag_dts: i64 = state
        .base_media_decode_time
        .unwrap_or(next_dts[state.track_idx]);
    let mut next_data_offset_within_traf: u64 = state.base_data_offset;

    let mut cur = std::io::Cursor::new(body);
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        if hdr.fourcc != TRUN {
            cur.set_position(cur.position() + psz as u64);
            continue;
        }
        let b = read_bytes_vec(&mut cur, psz)?;
        let parsed = parse_trun(&b)?;

        // Resolve the run's starting data offset:
        // * explicit data_offset present → relative to base_data_offset
        //   (for the first trun in a traf) or to start of moof when
        //   that's how base was derived. The spec defines `data_offset`
        //   as relative to the position of `moof.start + offset` when
        //   `tfhd.base_data_offset` is absent and default-base-is-moof
        //   is set; relative to `tfhd.base_data_offset` otherwise.
        //   We've already collapsed both into `state.base_data_offset`,
        //   so it's `base + data_offset`.
        // * absent → continue from end of previous trun's data.
        let mut sample_off = if let Some(d) = parsed.data_offset {
            // i32 → wrap into u64 by sign-extending so a negative
            // offset (rare but legal) backs up into the moof.
            (state.base_data_offset as i64).wrapping_add(d as i64) as u64
        } else {
            next_data_offset_within_traf
        };

        for (i, s) in parsed.samples.iter().enumerate() {
            let dur = s.duration.unwrap_or(state.default_sample_duration) as i64;
            let size = s.size.unwrap_or(state.default_sample_size);
            // Per-sample flags: explicit > first_sample_flags (i==0)
            // > tfhd default > trex default. The is-sync bit is
            // 0x0001_0000 when the sample is **non**-sync.
            let flags = s.flags.unwrap_or_else(|| {
                if i == 0 {
                    parsed
                        .first_sample_flags
                        .unwrap_or(state.default_sample_flags)
                } else {
                    state.default_sample_flags
                }
            });
            let keyframe = (flags & SAMPLE_IS_NON_SYNC) == 0;
            let cts_off = s.composition_time_offset.unwrap_or(0) as i64;
            let elst_shift = elst_leading_media_time(track);
            let dts_v = frag_dts.saturating_sub(elst_shift);
            let cts_v = frag_dts.saturating_add(cts_off).saturating_sub(elst_shift);

            samples.push(SampleRef {
                track_idx: state.track_idx as u32,
                offset: sample_off,
                size,
                pts: cts_v,
                dts: dts_v,
                duration: dur,
                keyframe,
            });

            sample_off = sample_off.saturating_add(size as u64);
            frag_dts = frag_dts.saturating_add(dur);
        }
        next_data_offset_within_traf = sample_off;
    }

    next_dts[state.track_idx] = frag_dts;
    Ok(())
}

/// §8.8.7 — `tfhd` (TrackFragmentHeaderBox).
///
/// Layout: FullBox header (4 bytes) + track_ID (u32). Then optional
/// fields gated by the flag bits in the lower 24 bits of the FullBox
/// header (in the order they appear in the spec table).
fn parse_tfhd(body: &[u8], moof_start: u64, tracks: &[Track], state: &mut TrafState) -> Result<()> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: tfhd too short"));
    }
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let track_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let track_idx = tracks
        .iter()
        .position(|t| t.track_id == track_id)
        .ok_or_else(|| {
            Error::invalid(format!("MP4: tfhd refers to unknown track_ID {track_id}"))
        })?;

    let mut off = 8;
    let trex = tracks[track_idx].trex;

    // Initial defaults from trex; tfhd flags will overwrite.
    state.track_idx = track_idx;
    state.tfhd_flags = flags;
    state.default_sample_duration = trex.default_sample_duration;
    state.default_sample_size = trex.default_sample_size;
    state.default_sample_flags = trex.default_sample_flags;

    // Reset the per-traf bmdt — it'll be set by tfdt if present.
    state.base_media_decode_time = None;

    // Establish base_data_offset.
    let mut explicit_base: Option<u64> = None;
    if flags & TFHD_BASE_DATA_OFFSET_PRESENT != 0 {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: tfhd base_data_offset truncated"));
        }
        let v = u64::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
            body[off + 4],
            body[off + 5],
            body[off + 6],
            body[off + 7],
        ]);
        explicit_base = Some(v);
        off += 8;
    }
    if flags & TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid(
                "MP4: tfhd sample_description_index truncated",
            ));
        }
        // Currently unused; we only carry the first stsd entry.
        off += 4;
    }
    if flags & TFHD_DEFAULT_SAMPLE_DURATION_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid(
                "MP4: tfhd default_sample_duration truncated",
            ));
        }
        state.default_sample_duration =
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
    }
    if flags & TFHD_DEFAULT_SAMPLE_SIZE_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: tfhd default_sample_size truncated"));
        }
        state.default_sample_size =
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
    }
    if flags & TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: tfhd default_sample_flags truncated"));
        }
        state.default_sample_flags =
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        // off += 4; // last optional, no further reads
    }

    // Resolve effective base_data_offset:
    // 1. explicit base_data_offset wins.
    // 2. otherwise, default-base-is-moof flag (0x020000) → moof_start.
    // 3. otherwise, per spec the "first byte of the enclosing Movie
    //    Fragment Box" — same as moof_start. (Spec actually says
    //    "first byte of the enclosing Movie Fragment Box" for the
    //    *first* traf in the moof, and "end of the previous traf's
    //    data" for subsequent ones, but every real fragmented file we
    //    care about either sets explicit base or default-base-is-moof.)
    state.base_data_offset = explicit_base.unwrap_or(moof_start);

    Ok(())
}

/// §8.8.12 — `tfdt` (TrackFragmentDecodeTimeBox).
///
/// Layout: FullBox header (4 bytes) + base_media_decode_time
/// (u32 if version=0, u64 if version=1).
fn parse_tfdt(body: &[u8]) -> Result<i64> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: tfdt too short"));
    }
    let version = body[0];
    if version == 1 {
        if body.len() < 12 {
            return Err(Error::invalid("MP4: tfdt v1 too short"));
        }
        Ok(u64::from_be_bytes([
            body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
        ]) as i64)
    } else {
        if body.len() < 8 {
            return Err(Error::invalid("MP4: tfdt v0 too short"));
        }
        Ok(u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as i64)
    }
}

/// One sample's optional per-sample fields from a `trun` entry. Each
/// `Some` carries the explicit value; `None` defers to the per-traf
/// or per-track defaults.
#[derive(Clone, Copy, Debug, Default)]
struct TrunSample {
    duration: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
    composition_time_offset: Option<i32>,
}

#[derive(Default)]
struct ParsedTrun {
    /// `data_offset` (i32) when the `data-offset-present` flag is set.
    /// Relative to the effective base_data_offset.
    data_offset: Option<i32>,
    /// Override flags for the *first* sample of the run when the
    /// `first-sample-flags-present` flag is set.
    first_sample_flags: Option<u32>,
    samples: Vec<TrunSample>,
}

/// §8.8.8 — `trun` (TrackRunBox).
///
/// Layout: FullBox header (4 bytes), sample_count (u32),
/// optional data_offset (i32), optional first_sample_flags (u32),
/// then sample_count repeats of the per-sample optional fields in the
/// fixed order: duration, size, flags, composition_time_offset.
fn parse_trun(body: &[u8]) -> Result<ParsedTrun> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: trun too short"));
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let sample_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut off = 8usize;

    let mut out = ParsedTrun::default();
    out.samples.reserve(sample_count);

    if flags & TRUN_DATA_OFFSET_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: trun data_offset truncated"));
        }
        out.data_offset = Some(i32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }
    if flags & TRUN_FIRST_SAMPLE_FLAGS_PRESENT != 0 {
        if off + 4 > body.len() {
            return Err(Error::invalid("MP4: trun first_sample_flags truncated"));
        }
        out.first_sample_flags = Some(u32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }

    let per_sample_fields = ((flags & TRUN_SAMPLE_DURATION_PRESENT) != 0) as usize
        + ((flags & TRUN_SAMPLE_SIZE_PRESENT) != 0) as usize
        + ((flags & TRUN_SAMPLE_FLAGS_PRESENT) != 0) as usize
        + ((flags & TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT) != 0) as usize;
    let needed = sample_count.saturating_mul(4 * per_sample_fields);
    if off + needed > body.len() {
        return Err(Error::invalid("MP4: trun samples truncated"));
    }

    for _ in 0..sample_count {
        let mut s = TrunSample::default();
        if flags & TRUN_SAMPLE_DURATION_PRESENT != 0 {
            s.duration = Some(u32::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
            ]));
            off += 4;
        }
        if flags & TRUN_SAMPLE_SIZE_PRESENT != 0 {
            s.size = Some(u32::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
            ]));
            off += 4;
        }
        if flags & TRUN_SAMPLE_FLAGS_PRESENT != 0 {
            s.flags = Some(u32::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
            ]));
            off += 4;
        }
        if flags & TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT != 0 {
            // v0: u32, v1: i32. We always store i32 — the round-trip
            // wraps the bit pattern, matching ffmpeg / libisobmff.
            let raw = [body[off], body[off + 1], body[off + 2], body[off + 3]];
            let v = if version == 0 {
                u32::from_be_bytes(raw) as i32
            } else {
                i32::from_be_bytes(raw)
            };
            s.composition_time_offset = Some(v);
            off += 4;
        }
        out.samples.push(s);
    }
    Ok(out)
}

// --- Sample-table expansion ----------------------------------------------

#[derive(Clone, Copy, Debug)]
struct SampleRef {
    track_idx: u32,
    offset: u64,
    size: u32,
    /// Composition-time stamp (CTS), media timescale. Equals DTS for
    /// streams without a `ctts` box (e.g. audio, intra-only video);
    /// otherwise CTS = DTS + ctts_offset, see ISO/IEC 14496-12 §8.6.1.3.
    pts: i64,
    /// Decoding-time stamp, media timescale. Carried separately so the
    /// packet can advertise both — the codec consumes the buffer in
    /// decode order (`dts`) and the player paces presentation by `pts`.
    dts: i64,
    duration: i64,
    keyframe: bool,
}

fn expand_samples(t: &Track, track_idx: u32, out: &mut Vec<SampleRef>) -> Result<()> {
    if t.stsz.is_empty() {
        return Ok(());
    }
    let n_samples = t.stsz.len();

    // Build per-sample DTS by scanning stts (cumulative).
    // ISO/IEC 14496-12 §8.6.1.2 — stts maps decoding-order index to
    // a per-sample delta in the media timescale, so the running sum
    // is the sample's DTS.
    let mut pts = Vec::with_capacity(n_samples);
    {
        let mut i = 0;
        let mut t_accum: i64 = 0;
        for &(count, delta) in &t.stts {
            for _ in 0..count {
                if i >= n_samples {
                    break;
                }
                pts.push((t_accum, delta as i64));
                t_accum += delta as i64;
                i += 1;
            }
        }
        while pts.len() < n_samples {
            pts.push((t_accum, 0));
        }
    }

    // Apply ctts (§8.6.1.3) to convert DTS → CTS. Without this the
    // packets we hand the codec carry DTS as `pts`, and any codec
    // that reorders for display (B-frames in H.264 / H.265 / AV1)
    // emits frames in display order with decode-time pts attached
    // — i.e. monotonic-decreasing pts at every B-frame boundary.
    // The CTS is what downstream pacing wants.
    let mut cts_offsets: Vec<i64> = vec![0; n_samples];
    if !t.ctts.is_empty() {
        let mut i = 0usize;
        for &(count, off) in &t.ctts {
            for _ in 0..count {
                if i >= n_samples {
                    break;
                }
                cts_offsets[i] = off as i64;
                i += 1;
            }
        }
    }

    // Determine which chunk each sample belongs to using stsc.
    // stsc is run-length: each entry says "starting at first_chunk, every
    // chunk has `samples_per_chunk` samples" until the next entry's first_chunk.
    // We need to know, for each sample, (chunk_index, index_within_chunk).
    //
    // Defensive: clamp `samples_per_chunk` to `n_samples` per entry so a
    // malicious file claiming `spc = u32::MAX` doesn't burn CPU spinning
    // an inner `for 0..spc` loop that the `sample_i >= n_samples` break
    // inside it would otherwise terminate only after ~4 billion iterations.
    // With the clamp the inner loop walks at most `n_samples` iterations
    // per stsc entry, matching what any well-formed file already does.
    let mut chunk_of_sample = Vec::with_capacity(n_samples);
    let mut sample_within_chunk = Vec::with_capacity(n_samples);
    {
        let mut sample_i = 0;
        let mut chunk_i = 1u32;
        let n_samples_u32 = u32::try_from(n_samples).unwrap_or(u32::MAX);
        for entry_i in 0..t.stsc.len() {
            let (fc, spc, _sdi) = t.stsc[entry_i];
            let next_fc = t
                .stsc
                .get(entry_i + 1)
                .map(|e| e.0)
                .unwrap_or(t.chunk_offsets.len() as u32 + 1);
            let spc_clamped = spc.min(n_samples_u32);
            // `next_fc - fc` runs of `spc` samples each.
            let mut ch = chunk_i.max(fc);
            while ch < next_fc && sample_i < n_samples {
                for s_in_ch in 0..spc_clamped {
                    if sample_i >= n_samples {
                        break;
                    }
                    chunk_of_sample.push(ch);
                    sample_within_chunk.push(s_in_ch);
                    sample_i += 1;
                }
                ch += 1;
            }
            chunk_i = ch;
        }
        // Fallback: if stsc didn't cover all samples, place the remainder in
        // the last chunk. (Invalid files — but don't crash.)
        while sample_within_chunk.len() < n_samples {
            chunk_of_sample.push(*chunk_of_sample.last().unwrap_or(&1));
            sample_within_chunk.push(0);
        }
    }

    // Per-sample keyframe flags. Per ISO/IEC 14496-12, an absent or empty
    // stss means every sample is a sync frame (this is the norm for audio
    // and intra-only video). Otherwise only the 1-based indices listed in
    // stss are sync frames.
    let stss_all_keyframes = t.stss.is_empty();
    let stss_set: std::collections::HashSet<u32> = t.stss.iter().copied().collect();

    // Compute each sample's absolute offset.
    for i in 0..n_samples {
        let chunk = chunk_of_sample[i] as usize;
        if chunk == 0 || chunk > t.chunk_offsets.len() {
            return Err(Error::invalid(format!(
                "MP4: chunk index {chunk} out of range (track {track_idx})"
            )));
        }
        let chunk_off = t.chunk_offsets[chunk - 1];
        // Sum sizes of preceding samples in this chunk.
        let chunk_start_sample = i - sample_within_chunk[i] as usize;
        let mut preceding: u64 = 0;
        for j in chunk_start_sample..i {
            preceding += t.stsz[j] as u64;
        }
        let size = t.stsz[i];
        let (dts_v, dur) = pts[i];
        // CTS = DTS + ctts_offset (§8.6.1.3). Streams without a ctts
        // box leave `cts_offsets` zero-filled, so audio + intra-only
        // video continue to take the DTS path unchanged.
        //
        // The edit list (§8.6.6) shifts the presentation timeline by
        // the first non-empty edit's `media_time`; subtract it from
        // CTS so the first presented frame's pts lands at zero.
        // Tracks without elst leave the leading shift at 0 (no-op).
        let elst_shift = elst_leading_media_time(t);
        let cts_v = dts_v
            .saturating_add(cts_offsets[i])
            .saturating_sub(elst_shift);
        let dts_v_shifted = dts_v.saturating_sub(elst_shift);
        let one_based = (i as u32) + 1;
        let keyframe = stss_all_keyframes || stss_set.contains(&one_based);
        out.push(SampleRef {
            track_idx,
            offset: chunk_off + preceding,
            size,
            pts: cts_v,
            dts: dts_v_shifted,
            duration: dur,
            keyframe,
        });
    }
    Ok(())
}

fn build_ctx<'a>(tag: &'a CodecTag, t: &'a Track) -> ProbeContext<'a> {
    let mut ctx = ProbeContext::new(tag);
    if !t.extradata.is_empty() {
        ctx = ctx.header(&t.extradata);
    }
    if let Some(b) = t.sample_size_bits {
        ctx = ctx.bits(b);
    }
    if let Some(c) = t.channels {
        ctx = ctx.channels(c);
    }
    if let Some(sr) = t.sample_rate {
        ctx = ctx.sample_rate(sr);
    }
    if let Some(w) = t.width {
        ctx = ctx.width(w);
    }
    if let Some(h) = t.height {
        ctx = ctx.height(h);
    }
    ctx
}

fn build_stream_info(index: u32, t: &Track, codecs: &dyn CodecResolver) -> StreamInfo {
    // Try the shared CodecResolver registry first — this lets codec crates
    // own their sample-entry FourCCs / OTI mapping. For `mp4a` / `mp4v`
    // entries we prefer the OTI-aware tag (more specific) over the bare
    // FourCC, then fall back to the static `from_sample_entry*` tables.
    let codec_id = {
        // Fill a ProbeContext with the hints we've already parsed so
        // codec probes can disambiguate (e.g. PCM flavours by bit depth).
        let mut resolved: Option<CodecId> = None;
        if let Some(oti) = t.esds_oti {
            let tag = CodecTag::mp4_object_type(oti);
            let ctx = build_ctx(&tag, t);
            resolved = codecs.resolve_tag(&ctx);
        }
        if resolved.is_none() {
            let tag = CodecTag::fourcc(&t.codec_id_fourcc);
            let ctx = build_ctx(&tag, t);
            resolved = codecs.resolve_tag(&ctx);
        }
        resolved.unwrap_or_else(|| match t.esds_oti {
            Some(oti) => from_sample_entry_with_oti(&t.codec_id_fourcc, oti),
            None => from_sample_entry(&t.codec_id_fourcc),
        })
    };
    let mut params = match t.media_type {
        MediaType::Audio => CodecParameters::audio(codec_id),
        MediaType::Video => CodecParameters::video(codec_id),
        _ => {
            let mut p = CodecParameters::audio(codec_id);
            p.media_type = MediaType::Data;
            p
        }
    };
    params.channels = t.channels;
    params.sample_rate = t.sample_rate;
    params.sample_format = match (params.codec_id.as_str(), t.sample_size_bits) {
        ("flac", Some(8)) => Some(SampleFormat::U8),
        ("flac", Some(16)) => Some(SampleFormat::S16),
        ("flac", Some(24)) => Some(SampleFormat::S24),
        ("flac", Some(32)) => Some(SampleFormat::S32),
        ("pcm_s16le", _) => Some(SampleFormat::S16),
        _ => None,
    };
    params.width = t.width;
    params.height = t.height;
    params.extradata = t.extradata.clone();

    let timescale = if t.timescale == 0 { 1 } else { t.timescale };
    StreamInfo {
        index,
        time_base: TimeBase::new(1, timescale as i64),
        duration: t.duration.map(|d| d as i64),
        start_time: Some(0),
        params,
    }
}

// --- Demuxer state --------------------------------------------------------

struct Mp4Demuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    samples: Vec<SampleRef>,
    cursor: usize,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
}

impl Demuxer for Mp4Demuxer {
    fn format_name(&self) -> &str {
        "mp4"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.cursor >= self.samples.len() {
            return Err(Error::Eof);
        }
        let s = self.samples[self.cursor];
        self.cursor += 1;
        self.input.seek(SeekFrom::Start(s.offset))?;
        let mut data = vec![0u8; s.size as usize];
        self.input.read_exact(&mut data)?;
        let stream = &self.streams[s.track_idx as usize];
        let mut pkt = Packet::new(s.track_idx, stream.time_base, data);
        // CTS for `pts` (display order), DTS for `dts` (decode order).
        // For tracks without a ctts box `pts == dts` because the
        // demuxer fills `cts_offsets[i]` with zero (§8.6.1.3).
        pkt.pts = Some(s.pts);
        pkt.dts = Some(s.dts);
        pkt.duration = Some(s.duration);
        pkt.flags.keyframe = s.keyframe;
        Ok(pkt)
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index as usize >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MP4: stream index {stream_index} out of range"
            )));
        }
        // Find the last keyframe of this stream with pts <= target.
        let mut best_cursor: Option<usize> = None;
        let mut best_pts: i64 = 0;
        for (i, s) in self.samples.iter().enumerate() {
            if s.track_idx != stream_index || !s.keyframe {
                continue;
            }
            if s.pts <= pts {
                if best_cursor.is_none() || s.pts >= best_pts {
                    best_cursor = Some(i);
                    best_pts = s.pts;
                }
            } else {
                break;
            }
        }
        // If no keyframe at-or-before target but there is any keyframe,
        // fall back to the first keyframe of this stream (pts 0).
        if best_cursor.is_none() {
            for (i, s) in self.samples.iter().enumerate() {
                if s.track_idx == stream_index && s.keyframe {
                    best_cursor = Some(i);
                    best_pts = s.pts;
                    break;
                }
            }
        }
        let cursor = best_cursor.ok_or_else(|| {
            Error::unsupported(format!(
                "MP4: no keyframes in stream {stream_index} to seek to"
            ))
        })?;
        self.cursor = cursor;
        Ok(best_pts)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

use std::io::Read;

fn read_bytes_vec<R: Read + ?Sized>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// Silence unused-import warnings for HashSet / SeekFrom if they become unused later.
#[allow(dead_code)]
fn _unused() -> (HashSet<u32>, SeekFrom) {
    (HashSet::new(), SeekFrom::Start(0))
}

#[cfg(test)]
mod tests {
    use super::parse_esds_dsi;

    /// Build a minimal esds ES_Descriptor payload that wraps a
    /// DecoderConfigDescriptor whose DecoderSpecificInfo equals `asc`.
    fn build_esds_payload(asc: &[u8]) -> Vec<u8> {
        // DecoderSpecificInfo: tag 0x05, length = asc.len(), body = asc.
        let mut dsi = Vec::new();
        dsi.push(0x05);
        dsi.push(asc.len() as u8);
        dsi.extend_from_slice(asc);

        // DecoderConfigDescriptor: tag 0x04, length = 13 + dsi.len().
        let mut dcd = vec![
            0x04,
            (13 + dsi.len()) as u8,
            0x40,               // object type: AAC
            (0x05 << 2) | 0x01, // stream type audio
        ];
        dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
        dcd.extend_from_slice(&[0, 0, 0, 0]); // maxBitrate
        dcd.extend_from_slice(&[0, 0, 0, 0]); // avgBitrate
        dcd.extend_from_slice(&dsi);

        // SLConfigDescriptor: tag 0x06, length 1, body 0x02.
        let slc = vec![0x06, 0x01, 0x02];

        // ES_Descriptor: tag 0x03, length = 3 + dcd + slc.
        let mut esd = Vec::new();
        esd.push(0x03);
        esd.push((3 + dcd.len() + slc.len()) as u8);
        esd.extend_from_slice(&[0, 0, 0]); // ES_ID + flags
        esd.extend_from_slice(&dcd);
        esd.extend_from_slice(&slc);
        esd
    }

    #[test]
    fn extracts_asc_from_esds() {
        // Typical AAC-LC 44.1 kHz stereo ASC: 0x12, 0x10.
        let asc = [0x12, 0x10];
        let payload = build_esds_payload(&asc);
        let got = parse_esds_dsi(&payload).expect("dsi");
        assert_eq!(got, asc);
    }

    #[test]
    fn handles_ber_multi_byte_length() {
        // Exercise the BER varint path by padding a descriptor length encoded
        // as 0x80 0x02 (two continuation bytes encoding the value 2).
        let asc = [0x11, 0x90];
        // Manually craft: tag 0x03, length encoded as 0x80|0x00, 0x80|0x00, 0x7F & len
        // Build the same ES_Descriptor body and then prefix tag/length directly.
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0]); // ES_ID + flags

        // DCD with single-byte BER length
        let mut dsi = vec![0x05, asc.len() as u8];
        dsi.extend_from_slice(&asc);
        let mut dcd = vec![0x04, (13 + dsi.len()) as u8, 0x40, (0x05 << 2) | 0x01];
        dcd.extend_from_slice(&[0, 0, 0]);
        dcd.extend_from_slice(&[0, 0, 0, 0]);
        dcd.extend_from_slice(&[0, 0, 0, 0]);
        dcd.extend_from_slice(&dsi);
        body.extend_from_slice(&dcd);
        body.extend_from_slice(&[0x06, 0x01, 0x02]);

        // Prepend ES_Descriptor tag + 2-byte BER length.
        let body_len = body.len();
        assert!(body_len < 128);
        let hi = (body_len >> 7) as u8 | 0x80;
        let lo = (body_len & 0x7F) as u8;
        let mut payload = vec![0x03, hi, lo];
        payload.extend_from_slice(&body);

        let got = parse_esds_dsi(&payload).expect("dsi");
        assert_eq!(got, asc);
    }

    #[test]
    fn rejects_non_es_descriptor() {
        let payload = vec![0x04, 0x01, 0x00];
        assert!(parse_esds_dsi(&payload).is_none());
    }

    /// Build a minimal AudioSampleEntryV0 (28-byte preamble) followed by
    /// an arbitrary child box. Channels 2, sample-size 16, sample-rate
    /// 48000, then `child_fourcc` carrying `child_body`.
    fn build_audio_sample_entry(child_fourcc: &[u8; 4], child_body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(28 + 8 + child_body.len());
        // 28-byte preamble: 6 reserved + 2 data_ref_idx + 8 reserved
        // + 2 channels + 2 sample_size + 2 pre_defined + 2 reserved
        // + 4 sample_rate (16.16 fixed).
        out.extend_from_slice(&[0u8; 6]);
        out.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        out.extend_from_slice(&[0u8; 8]); // reserved
        out.extend_from_slice(&2u16.to_be_bytes()); // channels
        out.extend_from_slice(&16u16.to_be_bytes()); // sample_size
        out.extend_from_slice(&[0u8; 4]); // pre_defined + reserved
        out.extend_from_slice(&((48_000u32) << 16).to_be_bytes()); // sample_rate 16.16
                                                                   // Child box: 4-byte size + 4-byte fourcc + body.
        let total = (8 + child_body.len()) as u32;
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(child_fourcc);
        out.extend_from_slice(child_body);
        out
    }

    fn fresh_track() -> super::Track {
        super::Track {
            track_id: 0,
            media_type: oxideav_core::MediaType::Audio,
            codec_id_fourcc: [0; 4],
            timescale: 0,
            duration: None,
            channels: None,
            sample_rate: None,
            sample_size_bits: None,
            width: None,
            height: None,
            extradata: Vec::new(),
            esds_oti: None,
            stts: Vec::new(),
            stsc: Vec::new(),
            stsz: Vec::new(),
            chunk_offsets: Vec::new(),
            stss: Vec::new(),
            ctts: Vec::new(),
            elst: Vec::new(),
            trex: super::TrexDefaults::default(),
        }
    }

    #[test]
    fn surfaces_dac3_box_as_extradata() {
        // ETSI TS 102 366 §F.4 dac3 specific box body: 3 bytes packing
        // fscod/bsid/bsmod/acmod/lfeon/bit_rate_code. Use a recognisable
        // pattern so the round-trip is visible.
        let dac3 = [0x10, 0x4C, 0x40];
        let entry = build_audio_sample_entry(b"dac3", &dac3);
        let mut t = fresh_track();
        super::parse_audio_sample_entry(&entry, &mut t).unwrap();
        assert_eq!(t.extradata, dac3, "dac3 body should be surfaced verbatim");
        assert_eq!(t.channels, Some(2));
        assert_eq!(t.sample_rate, Some(48_000));
    }

    #[test]
    fn surfaces_dec3_box_as_extradata() {
        // ETSI TS 102 366 §G.4 dec3 specific box body — variable length,
        // but the demuxer treats it opaquely. Pick 5 bytes.
        let dec3 = [0x07, 0xC0, 0x20, 0x00, 0x00];
        let entry = build_audio_sample_entry(b"dec3", &dec3);
        let mut t = fresh_track();
        super::parse_audio_sample_entry(&entry, &mut t).unwrap();
        assert_eq!(t.extradata, dec3, "dec3 body should be surfaced verbatim");
    }

    /// Adversarial stsc with `samples_per_chunk = u32::MAX` used to spin
    /// the inner `for 0..spc` loop ~4 billion times per chunk, freezing
    /// the demuxer for an unbounded period on a tiny `n_samples`. The
    /// clamp limits the inner loop to `n_samples` iterations per entry,
    /// so this test must complete in milliseconds, not minutes.
    #[test]
    fn expand_samples_clamps_giant_samples_per_chunk() {
        let mut t = fresh_track();
        t.stsz = vec![1, 1, 1, 1]; // 4 samples
        t.stts = vec![(4, 100)]; // 4 samples × delta 100
        t.stsc = vec![(1, u32::MAX, 1)]; // adversarial: spc = 2^32 - 1
        t.chunk_offsets = vec![0, 100, 200, 300]; // 4 chunks
        let mut out = Vec::new();
        let start = std::time::Instant::now();
        super::expand_samples(&t, 0, &mut out).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 4, "should yield exactly 4 samples");
        // 4 chunks × 4-sample clamp = 16 inner iterations, well under
        // 1ms. The pre-clamp version takes ~minutes (n_chunks * 4G ≈
        // 16 billion iterations). 100ms gives us plenty of headroom
        // while still catching any regression that re-introduces the
        // unbounded loop.
        assert!(
            elapsed.as_millis() < 100,
            "expand_samples spun on adversarial spc: took {elapsed:?}",
        );
    }

    /// `tfdt` v0 carries `base_media_decode_time` as a 32-bit field.
    #[test]
    fn parse_tfdt_v0_carries_32bit_bmdt() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
        body.extend_from_slice(&12_345u32.to_be_bytes());
        let bmdt = super::parse_tfdt(&body).unwrap();
        assert_eq!(bmdt, 12_345);
    }

    /// `tfdt` v1 carries `base_media_decode_time` as a 64-bit field.
    #[test]
    fn parse_tfdt_v1_carries_64bit_bmdt() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1, 0, 0, 0]); // version 1 + flags
        body.extend_from_slice(&0x0000_0001_2345_6789u64.to_be_bytes());
        let bmdt = super::parse_tfdt(&body).unwrap();
        assert_eq!(bmdt, 0x0000_0001_2345_6789);
    }

    /// `trun` with explicit per-sample sizes / durations.
    #[test]
    fn parse_trun_extracts_sample_count_size_duration() {
        // flags: data-offset (0x000001) + sample-duration (0x000100)
        //      + sample-size (0x000200) = 0x000301
        let flags: u32 =
            TRUN_DATA_OFFSET_PRESENT | TRUN_SAMPLE_DURATION_PRESENT | TRUN_SAMPLE_SIZE_PRESENT;
        let mut body = Vec::new();
        body.push(0); // version
        body.extend_from_slice(&flags.to_be_bytes()[1..4]); // 24-bit flags
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&0x12345678i32.to_be_bytes()); // data_offset
        for (dur, sz) in [(100u32, 50u32), (200, 60), (300, 70)] {
            body.extend_from_slice(&dur.to_be_bytes());
            body.extend_from_slice(&sz.to_be_bytes());
        }
        let parsed = super::parse_trun(&body).unwrap();
        assert_eq!(parsed.data_offset, Some(0x12345678));
        assert_eq!(parsed.samples.len(), 3);
        assert_eq!(parsed.samples[0].duration, Some(100));
        assert_eq!(parsed.samples[0].size, Some(50));
        assert_eq!(parsed.samples[2].duration, Some(300));
        assert_eq!(parsed.samples[2].size, Some(70));
    }

    use super::{TRUN_DATA_OFFSET_PRESENT, TRUN_SAMPLE_DURATION_PRESENT, TRUN_SAMPLE_SIZE_PRESENT};

    /// `trun` with composition-time-offset (B-frames). v1 negative
    /// offsets must be sign-extended.
    #[test]
    fn parse_trun_v1_signed_composition_offset() {
        let flags: u32 = TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT;
        let mut body = Vec::new();
        body.push(1); // version 1 — signed cts offsets
        body.extend_from_slice(&flags.to_be_bytes()[1..4]);
        body.extend_from_slice(&2u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&(-50i32).to_be_bytes());
        body.extend_from_slice(&(75i32).to_be_bytes());
        let parsed = super::parse_trun(&body).unwrap();
        assert_eq!(parsed.samples[0].composition_time_offset, Some(-50));
        assert_eq!(parsed.samples[1].composition_time_offset, Some(75));
    }

    use super::TRUN_SAMPLE_COMPOSITION_TIME_OFFSETS_PRESENT;

    /// `trex` populates per-track defaults that the fragment parser
    /// reads when a `tfhd` doesn't override them.
    #[test]
    fn parse_trex_populates_track_defaults() {
        let mut t = fresh_track();
        t.track_id = 7;
        let mut tracks = vec![t];
        // FullBox header + track_ID(7) + DSDI(1) + ddur(1024) + dsiz(0) + dflg(0)
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        body.extend_from_slice(&7u32.to_be_bytes()); // track_ID
        body.extend_from_slice(&1u32.to_be_bytes()); // DSDI
        body.extend_from_slice(&1024u32.to_be_bytes()); // default_sample_duration
        body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
        body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
        super::parse_trex(&body, &mut tracks).unwrap();
        assert_eq!(tracks[0].trex.default_sample_duration, 1024);
        assert_eq!(tracks[0].trex.default_sample_description_index, 1);
    }

    /// Multi-segment `elst` records every entry; the leading-shift
    /// helper picks the first non-empty `media_time`.
    #[test]
    fn parse_elst_v0_multi_segment() {
        // version 0, 2 entries:
        //   (dur=1000, media_time=-1, media_rate=0x10000)  // empty edit
        //   (dur=2000, media_time=500, media_rate=0x10000)
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
        body.extend_from_slice(&2u32.to_be_bytes());
        // entry 1
        body.extend_from_slice(&1000u32.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        // entry 2
        body.extend_from_slice(&2000u32.to_be_bytes());
        body.extend_from_slice(&500i32.to_be_bytes());
        body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let mut t = fresh_track();
        super::parse_elst(&body, &mut t).unwrap();
        assert_eq!(t.elst.len(), 2);
        assert_eq!(t.elst[0].media_time, -1);
        assert_eq!(t.elst[1].media_time, 500);
        assert_eq!(t.elst[1].segment_duration, 2000);
        assert_eq!(super::elst_leading_media_time(&t), 500);
    }
}
