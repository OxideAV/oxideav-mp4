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
use crate::cenc::{parse_pssh, parse_senc, parse_tenc, PsshBox, SencBox, TencBox};
use crate::codec_id::{from_sample_entry, from_sample_entry_with_oti};

pub fn open(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    // Walk top-level boxes looking for ftyp + moov. Continue past moov
    // to pick up any movie fragments (`moof`+`mdat` pairs) that a
    // fragmented / DASH / HLS / Smooth Streaming MP4 emits after the
    // initial movie header. ISO/IEC 14496-12 §8.8.
    let mut saw_ftyp = false;
    let mut moov: Option<Vec<u8>> = None;
    let mut moofs: Vec<MoofRecord> = Vec::new();
    let mut sidxes: Vec<SidxRecord> = Vec::new();
    // §8.16.4 — optional `ssix` SubsegmentIndexBoxes (zero or one per
    // leaf-only sidx, each immediately following its associated sidx).
    // Collected in file order; informational for this demuxer (we seek
    // via sidx / tfra), surfaced for partial-subsegment fetch tooling.
    let mut ssixes: Vec<SsixRecord> = Vec::new();
    let mut tfras: Vec<TfraRecord> = Vec::new();
    let mut prfts: Vec<PrftRecord> = Vec::new();
    // §8.1.3 — optional `pdin` ProgressiveDownloadInfoBox. Quantity is
    // zero or one per file (§8.1.3.1); we keep the first instance seen
    // during the top-level walk and silently ignore any subsequent
    // copies (a malformed file with two pdin boxes is no reason to
    // abort the open — the first is the one the spec endorses).
    let mut pdin: Option<PdinRecord> = None;
    while let Some(hdr) = read_box_header(&mut *input)? {
        match hdr.fourcc {
            FTYP => {
                saw_ftyp = true;
                skip_box_body(&mut *input, &hdr)?;
            }
            // §8.1.3 — `pdin` ProgressiveDownloadInfoBox. Top-level
            // FullBox carrying (rate, initial_delay) u32 pairs that hint
            // the suggested initial playback delay for a given effective
            // download rate. Captured for the structured `pdin_entries`
            // accessor and surfaced as flat `pdin` / `pdin_<n>`
            // metadata keys below.
            PDIN => {
                let body = read_box_body(&mut *input, &hdr)?;
                if pdin.is_none() {
                    pdin = Some(parse_pdin(&body)?);
                }
            }
            MOOV => {
                moov = Some(read_box_body(&mut *input, &hdr)?);
            }
            // §8.16 — `styp` SegmentTypeBox (CMAF / DASH segment marker).
            // Same shape as ftyp; we just skip its payload.
            STYP => skip_box_body(&mut *input, &hdr)?,
            // §8.16.3 — `sidx` SegmentIndexBox. Sample-precision seek
            // index for adaptive-bitrate streams. Capture the absolute
            // file offset of the sidx itself — sidx-internal references
            // are relative to "the first byte after this box" (per
            // spec), so we need that anchor to resolve them.
            SIDX => {
                let body_start = input.stream_position()?;
                let sidx_end_offset = body_start
                    + hdr
                        .payload_size()
                        .ok_or_else(|| Error::invalid("MP4: open-ended sidx"))?;
                let body = read_box_body(&mut *input, &hdr)?;
                if let Some(r) = parse_sidx(&body, sidx_end_offset)? {
                    sidxes.push(r);
                }
            }
            // §8.16.4 — `ssix` SubsegmentIndexBox. Maps levels (per the
            // `leva` LevelAssignmentBox, §8.8.13) to byte ranges of the
            // subsegments indexed by the immediately preceding `sidx`,
            // enabling partial-subsegment byte-range access. The box is
            // informational for this demuxer (seeking uses sidx / tfra),
            // so — like `leva` — a malformed instance is dropped rather
            // than aborting the open.
            SSIX => {
                let body = read_box_body(&mut *input, &hdr)?;
                if let Ok(r) = parse_ssix(&body) {
                    ssixes.push(r);
                }
            }
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
            // §8.8.10 — `mfra` MovieFragmentRandomAccessBox. Container
            // for one `tfra` per track-with-random-access plus the
            // size-of-mfra `mfro` trailer. Parsed for the byte-range
            // seek-table consumed by `seek_to`.
            MFRA => {
                let body = read_box_body(&mut *input, &hdr)?;
                parse_mfra(&body, &mut tfras)?;
            }
            // §8.16.5 — `prft` ProducerReferenceTimeBox. Top-level
            // FullBox correlating wall-clock NTP time with a media
            // time on one reference track. Multiple instances are
            // allowed; each is associated with the following moof in
            // file order. We collect them all; callers walk the
            // returned list / metadata channel.
            PRFT => {
                let body = read_box_body(&mut *input, &hdr)?;
                if let Some(r) = parse_prft(&body)? {
                    prfts.push(r);
                }
            }
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

    let mut senc_records: Vec<SencRecord> = Vec::new();
    let mut sai_records: Vec<SaiRecord> = Vec::new();
    let mut moof_psshes: Vec<MoofPsshRecord> = Vec::new();
    for moof in &moofs {
        parse_moof(
            moof,
            &parsed.tracks,
            &mut samples,
            &mut next_dts,
            &mut senc_records,
            &mut sai_records,
            &mut moof_psshes,
        )?;
    }

    samples.sort_by_key(|s| s.offset);

    // Movie duration from mvhd (§8.2.2), with §8.8.2 `mehd`
    // (MovieExtendsHeaderBox) as fall-back for fragmented files. When
    // a fragmented file is sealed with a known overall duration, the
    // authoring step writes `mehd.fragment_duration` while
    // `mvhd.duration` may legitimately stay zero (the moov has no
    // resident samples that contribute to the duration). In that case
    // we report the mehd value; when mvhd already supplies a non-zero
    // duration and mehd is also present, the spec wording
    // ("provides the overall duration, including fragments") makes
    // mehd the authoritative total, so we still prefer it.
    let effective_duration: u64 = parsed
        .mehd_fragment_duration
        .filter(|&d| d > 0)
        .unwrap_or(parsed.movie_duration);
    let duration_micros: i64 = if parsed.movie_timescale > 0 && effective_duration > 0 {
        (effective_duration as i128 * 1_000_000 / parsed.movie_timescale as i128) as i64
    } else {
        0
    };

    // Surface §8.8.2 mehd fragment_duration as a top-level metadata
    // key when present. Mirrors the rest of this crate's flat metadata
    // surface: the structured value is also reflected in
    // `Demuxer::duration_micros` (see the duration calc above); this
    // key exposes the raw value in the movie timescale for tooling
    // that wants the untranslated number (e.g. CMAF live-edge probes
    // wanting to confirm a producer's sealed total against their own
    // running tally). Absent `mehd`, no key is emitted.
    let mut metadata = parsed.metadata;
    if let Some(d) = parsed.mehd_fragment_duration {
        metadata.push(("mehd_fragment_duration".to_string(), d.to_string()));
    }

    // Surface a parsed `pdin` (ProgressiveDownloadInfoBox, §8.1.3) on
    // the flat metadata channel. Quantity is zero or one per file
    // (§8.1.3.1); we emit:
    //   - `pdin_count` = number of (rate, initial_delay) pairs
    //   - `pdin_<n>` = "rate initial_delay" (decimal, space-separated)
    //     for n in 0..count, mirroring the per-pair surface used for
    //     `prft_<n>`. A consumer that wants the structured record
    //     reaches for `Mp4Demuxer::pdin_entries()` (or the public
    //     `parse_pdin_box`) — the flat surface is for tooling that
    //     prefers a string-keyed metadata bag.
    // Absent `pdin`, no keys are emitted.
    if let Some(p) = pdin.as_ref() {
        metadata.push(("pdin_count".to_string(), p.entries.len().to_string()));
        for (n, e) in p.entries.iter().enumerate() {
            metadata.push((
                format!("pdin_{n}"),
                format!("{} {}", e.rate, e.initial_delay),
            ));
        }
    }

    // Surface a parsed `leva` (LevelAssignmentBox, §8.8.13) on the flat
    // metadata channel. Quantity is zero or one per file (§8.8.13.1);
    // we emit:
    //   - `leva_count` = number of per-level entries
    //   - `leva_<n>` = "<track_id> pad=<0|1> at=<assignment_type>
    //     [grouping_type=<u32>] [grouping_type_parameter=<u32>]
    //     [sub_track_id=<u32>]" — the trailing tokens are emitted only
    //     when their assignment_type variant uses them, mirroring the
    //     variant-specific tails in §8.8.13.2.
    // A consumer that wants the structured record reaches for
    // `Mp4Demuxer::leva_entries()` (or the public `parse_leva_box`).
    // Absent `leva`, no keys are emitted.
    if let Some(l) = parsed.leva.as_ref() {
        metadata.push(("leva_count".to_string(), l.entries.len().to_string()));
        for (n, e) in l.entries.iter().enumerate() {
            let mut s = format!(
                "{} pad={} at={}",
                e.track_id, e.padding_flag as u8, e.assignment_type
            );
            match e.assignment_type {
                0 => {
                    use std::fmt::Write;
                    let _ = write!(s, " grouping_type={}", e.grouping_type);
                }
                1 => {
                    use std::fmt::Write;
                    let _ = write!(
                        s,
                        " grouping_type={} grouping_type_parameter={}",
                        e.grouping_type, e.grouping_type_parameter,
                    );
                }
                4 => {
                    use std::fmt::Write;
                    let _ = write!(s, " sub_track_id={}", e.sub_track_id);
                }
                _ => {}
            }
            metadata.push((format!("leva_{n}"), s));
        }
    }

    // Surface any parsed `trep` TrackExtensionPropertiesBoxes (§8.8.15)
    // on the flat metadata channel. Quantity is zero or more (zero or
    // one per track, §8.8.15.1); we emit one `trep_<n>` key per box, in
    // file order, with value "<track_id> children=<k>[ <fourcc>...]" —
    // the track the box describes, the number of nested child boxes,
    // and each child's four-character type (so a consumer can spot an
    // `assp` without consuming the structured record). Callers wanting
    // the structured form reach for `Mp4Demuxer::treps()` (or the
    // public `parse_trep_box`). Absent `trep`, no keys are emitted.
    for (n, t) in parsed.treps.iter().enumerate() {
        let mut s = format!("{} children={}", t.track_id, t.children.len());
        for c in &t.children {
            use std::fmt::Write;
            let _ = write!(s, " {}", String::from_utf8_lossy(&c.fourcc));
        }
        metadata.push((format!("trep_{n}"), s));
    }

    // Surface any parsed `prft` ProducerReferenceTimeBoxes through the
    // container metadata channel as `prft_<n>` (0-based file order)
    // with value "reference_track_ID ntp_timestamp media_time" — three
    // decimal integers, space-separated, mirroring the
    // `tref_<type>` / `sgpd_<n>` conventions used elsewhere in this
    // crate. Callers wanting the structured record can use the public
    // `parse_prft_box` entry point. Absent prft, no keys are emitted.
    for (n, p) in prfts.iter().enumerate() {
        metadata.push((
            format!("prft_{n}"),
            format!(
                "{} {} {}",
                p.reference_track_id, p.ntp_timestamp, p.media_time
            ),
        ));
    }

    // Surface any parsed `ssix` SubsegmentIndexBoxes (§8.16.4) through
    // the container metadata channel as `ssix_<n>` (0-based file order)
    // with summary value "<subsegment_count> <total_range_count>" —
    // two decimal integers, space-separated. The full per-subsegment
    // (level, range_size) table can be large (one 4-byte record per
    // partial-subsegment range), so the flat channel carries only the
    // shape; callers wanting the structured record reach for the
    // public `parse_ssix_box` entry point (or `Mp4Demuxer::ssixes`).
    // Absent ssix, no keys are emitted.
    for (n, s) in ssixes.iter().enumerate() {
        let total_ranges: usize = s.subsegments.iter().map(|ss| ss.ranges.len()).sum();
        metadata.push((
            format!("ssix_{n}"),
            format!("{} {}", s.subsegments.len(), total_ranges),
        ));
    }

    // Surface ISO/IEC 23001-7 §8.1 pssh boxes as `pssh_<n>` keys with
    // value `"<system_id_hex> <kid_count> <data_len>"`. Callers wanting
    // the structured record use the public `Mp4Demuxer::psshes` accessor.
    for (n, p) in parsed.psshes.iter().enumerate() {
        let sysid_hex: String = p.system_id.iter().map(|b| format!("{b:02x}")).collect();
        metadata.push((
            format!("pssh_{n}"),
            format!("{} {} {}", sysid_hex, p.kids.len(), p.data.len()),
        ));
    }

    // Aggregate counter for §7.2 senc — one key per (track_idx,
    // moof_sequence) record with summary "<sample_count> <flags_hex>".
    for (n, r) in senc_records.iter().enumerate() {
        metadata.push((
            format!("senc_{n}"),
            format!(
                "track={} seq={} samples={} flags=0x{:08x}",
                r.track_idx,
                r.moof_sequence,
                r.senc.samples.len(),
                r.senc.flags,
            ),
        ));
    }

    // §8.7.8–9 — per-fragment `(saiz, saio)` summary, one key per
    // SaiRecord (one per `traf` that carried at least one box).
    // Format: "track=<t> seq=<s> saiz=<n> saio=<m>" so a caller knows
    // the box counts on this fragment without consuming the public
    // SaiRecord vector. Structured records remain accessible via
    // `Mp4Demuxer::sai_records()`.
    for (n, r) in sai_records.iter().enumerate() {
        metadata.push((
            format!("frag_sai_{n}"),
            format!(
                "track={} seq={} saiz={} saio={}",
                r.track_idx,
                r.moof_sequence,
                r.saiz.len(),
                r.saio.len(),
            ),
        ));
    }

    // ISO/IEC 23001-7 §8.1 — pssh boxes that live inside individual
    // `moof` boxes (§8.1.1 permits pssh in either `moov` or `moof`).
    // One `moof_pssh_<n>` key per MoofPsshRecord, in moof-walk order,
    // summarising SystemID + fragment scope. The structured records
    // are accessible via `Mp4Demuxer::moof_psshes()`. Hex encoding
    // mirrors the moov-level `pssh_<n>` SystemID surface.
    for (n, r) in moof_psshes.iter().enumerate() {
        let sysid_hex: String = r
            .pssh
            .system_id
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        metadata.push((
            format!("moof_pssh_{n}"),
            format!(
                "systemid={} seq={} kids={} data={}",
                sysid_hex,
                r.moof_sequence,
                r.pssh.kids.len(),
                r.pssh.data.len(),
            ),
        ));
    }

    Ok(Box::new(Mp4Demuxer {
        input,
        streams,
        samples,
        cursor: 0,
        metadata,
        duration_micros,
        sidxes,
        ssixes,
        tfras,
        prfts,
        pdin,
        leva: parsed.leva,
        treps: parsed.treps,
        psshes: parsed.psshes,
        moof_psshes,
        senc_records,
        sai_records,
        movie_timescale: parsed.movie_timescale,
        track_timescales: parsed.tracks.iter().map(|t| t.timescale).collect(),
        track_ids: parsed.tracks.iter().map(|t| t.track_id).collect(),
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

/// Decoded `sidx` (SegmentIndexBox, §8.16.3) for one track. Each
/// reference describes one subsegment (typically one `moof+mdat` pair):
/// its byte size, its decode-time duration in this track's timescale,
/// and a SAP-type flag (we keep the sync-bit surface but only use the
/// "starts-with-SAP" marker).
#[derive(Clone, Debug)]
pub struct SidxRecord {
    /// `reference_ID` — usually a `tkhd.track_ID` for a single-track
    /// segment, or a hierarchical index ID for nested sidx.
    pub reference_id: u32,
    /// Timescale for `earliest_presentation_time` and per-reference
    /// `subsegment_duration` (per spec, NOT the track's media timescale
    /// — DASH-IF Interop §6.2.3 uses the track timescale here, but the
    /// sidx is allowed to pick its own).
    pub timescale: u32,
    /// Composition timestamp of the first sample referenced.
    pub earliest_presentation_time: u64,
    /// File offset of the first byte of the FIRST referenced
    /// subsegment, computed from `first_offset` + `sidx_end_offset`
    /// (§8.16.3.1: "an unsigned integer that gives the offset relative
    /// to the first byte after this box").
    pub first_byte_offset: u64,
    pub references: Vec<SidxReference>,
}

#[derive(Clone, Copy, Debug)]
pub struct SidxReference {
    /// True when this reference points to another `sidx` (hierarchical
    /// index), false when it points to the actual media subsegment.
    pub is_sidx: bool,
    /// Total byte size of the referenced subsegment / sidx.
    pub referenced_size: u32,
    /// Decode-time duration of the subsegment in `timescale` units.
    pub subsegment_duration: u32,
    /// True when the subsegment starts with a Stream Access Point.
    pub starts_with_sap: bool,
    /// SAP type (0..=7); 1 = Closed-GOP (IDR), 2 = open-GOP IDR-like,
    /// 3 = open-GOP, etc. Per §I.2 of ISO/IEC 14496-12.
    pub sap_type: u8,
}

/// Decoded `ssix` (SubsegmentIndexBox, ISO/IEC 14496-12 §8.16.4).
///
/// Maps levels (as assigned by the `leva` LevelAssignmentBox, §8.8.13)
/// to byte ranges of the indexed subsegments, so a client can fetch a
/// partial subsegment (e.g. only the lower temporal levels) by byte
/// range instead of downloading the whole subsegment. Placement rules
/// (§8.16.4.1): zero or one per `sidx` that indexes only leaf
/// subsegments, immediately following the associated `sidx`;
/// `subsegment_count` shall equal that sidx's `reference_count`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsixRecord {
    /// One entry per subsegment indexed by the associated `sidx`, in
    /// the same order as the sidx references.
    pub subsegments: Vec<SsixSubsegment>,
}

/// Per-subsegment range list inside an `ssix` (§8.16.4.2).
///
/// Every byte of the subsegment is explicitly assigned to a level, so
/// the ranges partition the subsegment: range j starts at the byte
/// immediately after range j−1 ends (the first at the subsegment's
/// first byte) and spans `range_size` bytes. §8.16.4.1 requires at
/// least two ranges per subsegment in a conforming file; we parse a
/// smaller count without error (the constraint binds writers — a
/// reader gains nothing by rejecting a one-range partition).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsixSubsegment {
    pub ranges: Vec<SsixRange>,
}

/// One (level, range_size) record of an `ssix` subsegment (§8.16.4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SsixRange {
    /// Level this partial subsegment is assigned to (§8.16.4.3). For
    /// leaf subsegments, levels appear in increasing order within a
    /// subsegment — samples of a partial subsegment may depend on
    /// preceding partial subsegments of the same subsegment, never on
    /// later ones.
    pub level: u8,
    /// Byte size of the partial subsegment (24-bit field, so at most
    /// 0xFF_FFFF per range; larger spans repeat the level across
    /// consecutive ranges).
    pub range_size: u32,
}

/// Decoded `tfra` (TrackFragmentRandomAccessBox, §8.8.11) — per-track
/// table of moof byte offsets keyed by presentation time. One entry per
/// random-access point in the track.
#[derive(Clone, Debug)]
pub struct TfraRecord {
    pub track_id: u32,
    pub entries: Vec<TfraEntry>,
}

#[derive(Clone, Copy, Debug)]
pub struct TfraEntry {
    /// Decoding time of the random-access sample in the track's media
    /// timescale (NOT the movie timescale).
    pub time: u64,
    /// File offset of the `moof` box containing the sample.
    pub moof_offset: u64,
    /// 1-based traf number within that moof.
    pub traf_number: u32,
    /// 1-based trun number within that traf.
    pub trun_number: u32,
    /// 1-based sample index within that trun.
    pub sample_number: u32,
}

/// Decoded `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12 §8.16.5).
///
/// A producer reference time correlates a UTC wall-clock instant (in
/// NTP 64-bit format, RFC 5905 — high 32 bits = seconds since 1900-01-01,
/// low 32 bits = fractional seconds) with a media time on one reference
/// track. The box appears at file scope and relates to the NEXT `moof`
/// box that follows it in bitstream order (§8.16.5.1 placement rule),
/// so a low-latency DASH/CMAF live consumer can match production
/// wall-clock to media presentation time.
#[derive(Clone, Copy, Debug)]
pub struct PrftRecord {
    /// `track_ID` of the reference track whose `media_time` this box
    /// annotates (§8.16.5.3 reference_track_ID).
    pub reference_track_id: u32,
    /// UTC time in NTP 64-bit format. Caller decomposes into seconds
    /// (`(ntp_timestamp >> 32) - 2_208_988_800` for Unix epoch) and
    /// fractional seconds (`(ntp_timestamp & 0xFFFF_FFFF) / 2^32`).
    pub ntp_timestamp: u64,
    /// Same instant expressed in the reference track's media timescale
    /// (decoding time). Promoted from u32 → u64 for the v0 layout so
    /// callers see one type regardless of box version.
    pub media_time: u64,
    /// Box version (0 → 32-bit `media_time`, 1 → 64-bit `media_time`).
    /// Surfaced so callers can validate against `media_time`'s
    /// representable range when round-tripping.
    pub version: u8,
}

/// One `(rate, initial_delay)` pair from a `pdin`
/// ProgressiveDownloadInfoBox (ISO/IEC 14496-12 §8.1.3).
///
/// `rate` is an effective download rate in bytes per second; for that
/// rate, `initial_delay` (milliseconds) is the suggested initial
/// playback delay that lets the rest of the file arrive ahead of its
/// playback deadline (§8.1.3.3). A receiver picks two adjacent
/// entries whose rates bracket its observed throughput and linearly
/// interpolates the delay, or extrapolates from the first / last entry
/// when its observed rate sits outside the recorded range.
///
/// Both fields are `u32` on the wire (big-endian) and surfaced
/// verbatim — neither is bounded by the spec other than its 32-bit
/// width, so the structure preserves whatever the producer wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdinEntry {
    /// Effective download rate in bytes / second.
    pub rate: u32,
    /// Suggested initial playback delay in milliseconds at `rate`.
    pub initial_delay: u32,
}

/// Decoded `pdin` (ProgressiveDownloadInfoBox, ISO/IEC 14496-12 §8.1.3).
///
/// `FullBox(version = 0, flags = 0)` whose body is `N` consecutive
/// `(rate, initial_delay)` u32 pairs — `N = body_payload_size / 8`.
/// The spec fixes no minimum or maximum count; an empty body (zero
/// pairs) is permitted and yields an empty `entries` vec. The version
/// is pinned to 0 (§8.1.3.2); a non-zero version is tolerated rather
/// than rejected because the on-wire layout is unambiguous.
///
/// Quantity is zero or one per file (§8.1.3.1); the demuxer collects
/// the first instance and ignores any subsequent ones.
#[derive(Clone, Debug)]
pub struct PdinRecord {
    /// Pairs in file order, mirroring §8.1.3.2's loop ordering.
    /// A consumer searching by rate should sort if a non-monotonic
    /// producer is suspected — the spec wording recommends interpolation
    /// but doesn't *mandate* monotonic ordering.
    pub entries: Vec<PdinEntry>,
}

/// Per-level descriptor carried by `leva` (LevelAssignmentBox, ISO/IEC
/// 14496-12 §8.8.13). One entry per `level_count` loop iteration in
/// §8.8.13.2 syntax.
///
/// On the wire each entry is:
/// `unsigned int(32) track_id; unsigned int(1) padding_flag;
/// unsigned int(7) assignment_type; <type-specific tail>`.
///
/// The trailing tail varies with `assignment_type`:
/// * 0 → `unsigned int(32) grouping_type`
/// * 1 → `unsigned int(32) grouping_type` + `unsigned int(32) grouping_type_parameter`
/// * 2 or 3 → empty (track / track-subsegment level assignment)
/// * 4 → `unsigned int(32) sub_track_id`
/// * other values are reserved (§8.8.13.3)
///
/// The §8.8.13.3 sequence rule constrains `assignment_type` ordering
/// across an entire `leva`: "zero or more of type 2 or 3, followed by
/// zero or more of exactly one type." The demuxer preserves the
/// producer's order verbatim and does not enforce the rule — a downstream
/// consumer that cares about it can validate against
/// [`LevaEntry::assignment_type`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevaEntry {
    /// `track_id` field per §8.8.13.3 — identifies the track assigned
    /// to this level. Track 0 is not a legal value (the spec defines
    /// track IDs as ≥ 1 in §8.3.2.3); the demuxer carries whatever the
    /// producer wrote so a validator can flag it.
    pub track_id: u32,
    /// `padding_flag` (1-bit, §8.8.13.3): when `true`, a conforming
    /// fraction may be formed by concatenating any positive integer
    /// number of levels and padding the last `mdat` to its full header
    /// size with zero bytes. When `false`, this padding option is not
    /// assured by the producer.
    pub padding_flag: bool,
    /// `assignment_type` (7-bit) — discriminator for the variant-specific
    /// tail; surfaced verbatim. Values 0..=4 are defined by the spec;
    /// any value > 4 falls in the §8.8.13.3 "reserved" range and the
    /// demuxer carries it without further parsing the tail.
    pub assignment_type: u8,
    /// `grouping_type` (set when `assignment_type` ∈ {0, 1}), otherwise 0.
    pub grouping_type: u32,
    /// `grouping_type_parameter` (set when `assignment_type == 1`),
    /// otherwise 0. §8.9.3 / §10.5 pin its semantics for the matching
    /// sample grouping.
    pub grouping_type_parameter: u32,
    /// `sub_track_id` (set when `assignment_type == 4`), otherwise 0.
    /// §8.14.4 identifies the sub-track that holds this level's samples.
    pub sub_track_id: u32,
}

/// Decoded `leva` (LevelAssignmentBox, ISO/IEC 14496-12 §8.8.13).
///
/// `FullBox(version = 0, flags = 0)` whose body opens with
/// `unsigned int(8) level_count` and is followed by `level_count`
/// per-level entries laid out per [`LevaEntry`]. The spec requires
/// `level_count ≥ 2` (§8.8.13.3 "level_count shall be greater than or
/// equal to 2"); the demuxer carries whatever the producer wrote and
/// surfaces the count via `entries.len()` so a validator can spot a
/// short table.
///
/// Quantity is zero or one per file (§8.8.13.1); the demuxer collects
/// the first instance seen inside `mvex` and ignores any subsequent
/// copies (a malformed file with two leva boxes is no reason to abort
/// the parse — the first is the one the spec endorses).
#[derive(Clone, Debug)]
pub struct LevaRecord {
    /// Entries in file order, mirroring §8.8.13.2's loop ordering.
    /// The §8.8.13.3 sequence rule on `assignment_type` ordering is
    /// not enforced here; a consumer can validate by walking the slice.
    pub entries: Vec<LevaEntry>,
}

/// One child box recorded inside a `trep` (TrackExtensionPropertiesBox,
/// ISO/IEC 14496-12 §8.8.15). The box body is "any number of boxes"
/// (§8.8.15.2) — the demuxer records each child's type and payload
/// length without interpreting it, so a downstream consumer can spot,
/// for example, an `assp` (Alternative Startup Sequence Properties Box,
/// §8.8.16) without this crate having to model every box that might be
/// nested. The structured nesting stays opaque here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrepChild {
    /// The child box's four-character type code, verbatim on the wire.
    pub fourcc: [u8; 4],
    /// Length of the child's payload in bytes (the box size minus its
    /// header). A child whose declared size ran past the end of the
    /// `trep` body is clamped to the bytes actually available, so this
    /// reflects what was readable, not necessarily the producer's
    /// declared length.
    pub payload_len: usize,
}

/// Decoded `trep` (TrackExtensionPropertiesBox, ISO/IEC 14496-12
/// §8.8.15).
///
/// `FullBox(version = 0, flags = 0)` whose body opens with
/// `unsigned int(32) track_id` (the track these extension properties
/// describe, §8.8.15.3) and is followed by "any number of boxes"
/// (§8.8.15.2). The trailing children are recorded by type + length
/// only — see [`TrepChild`].
///
/// Quantity is "zero or more, zero or one per track" (§8.8.15.1); the
/// demuxer collects every `trep` it finds inside `mvex` in file order.
/// A `trep` with a malformed FullBox preamble or a truncated `track_id`
/// is dropped rather than aborting the parse, matching the treatment of
/// the sibling optional `mvex` boxes (`mehd`, `leva`).
#[derive(Clone, Debug)]
pub struct TrepRecord {
    /// `track_id` (§8.8.15.3) — the track for which these extension
    /// properties are provided. Carried verbatim; track 0 is not a
    /// legal value (§8.3.2.3 pins track IDs ≥ 1) but the demuxer does
    /// not reject it so a validator can flag it.
    pub track_id: u32,
    /// Child boxes nested in this `trep`, in file order. Each is
    /// recorded by type + payload length only ([`TrepChild`]); the
    /// list is empty for a `trep` that carries just its `track_id`.
    pub children: Vec<TrepChild>,
}

#[derive(Default)]
struct ParsedMoov {
    tracks: Vec<Track>,
    movie_timescale: u32,
    movie_duration: u64,
    /// §8.8.2 (`mehd` MovieExtendsHeaderBox) — overall presentation
    /// duration of a fragmented movie, including fragments, in the
    /// movie timescale. `None` when the box is absent (per the spec
    /// it is optional). v0 widens to u64 on intake so the rest of the
    /// pipeline sees one type. When present and non-zero, takes
    /// precedence over `mvhd.duration` for the surfaced
    /// `Demuxer::duration_micros` value — a sealed fragmented file
    /// typically has `mvhd.duration = 0` and the only authoritative
    /// total is the `mehd`.
    mehd_fragment_duration: Option<u64>,
    metadata: Vec<(String, String)>,
    /// §8.1 (ISO/IEC 23001-7) — `pssh`
    /// ProtectionSystemSpecificHeaderBox entries collected at moov
    /// level. Each entry corresponds to one DRM system signalled in
    /// the file by SystemID UUID. moof-level `pssh` instances are
    /// also permitted by the spec but the surface here is moov-only;
    /// fragment-level pssh can be added without changing the demuxer
    /// API.
    psshes: Vec<PsshBox>,
    /// §8.8.13 — `leva` LevelAssignmentBox. Optional FullBox inside
    /// `mvex` (quantity zero or one); maps tracks / sample groups /
    /// sub-tracks to "levels" inside subsequent movie fragments. `None`
    /// when the box is absent. Informational only — the demuxer does
    /// not consume the levels itself, but downstream tooling (DASH
    /// subsegment selection, scalable-bitstream layer selection) can
    /// reach them through `Mp4Demuxer::leva_entries()` or the flat
    /// `leva_count` / `leva_<n>` metadata keys.
    leva: Option<LevaRecord>,
    /// §8.8.15 — `trep` TrackExtensionPropertiesBox entries. Optional
    /// FullBoxes inside `mvex` (quantity zero or more, zero or one per
    /// track); each documents characteristics of one track in the
    /// subsequent movie fragments. Empty when the file carries none.
    /// Informational only — the demuxer does not consume them, but
    /// downstream tooling can reach them through
    /// `Mp4Demuxer::treps()` or the flat `trep_<n>` metadata keys.
    treps: Vec<TrepRecord>,
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
    /// Set when the track's sample entry was wrapped as `encv`/`enca`/
    /// `enct`/`encs` (ISO/IEC 14496-12 §8.12). The carried value is the
    /// `scheme_type` four-character code recovered from the inner
    /// `sinf/schm` box — e.g. `cenc` / `cbc1` / `cens` / `cbcs` per
    /// ISO/IEC 23001-7. Callers should treat the per-sample payloads as
    /// encrypted (this crate does not decrypt). The original codec
    /// FourCC is recovered transparently via `sinf/frma` so
    /// `codec_id_fourcc` reflects the un-transformed sample entry and
    /// downstream decoders can be set up as if the track were plain —
    /// they just won't get plaintext bytes until something else
    /// decrypts them.
    protection_scheme: Option<[u8; 4]>,
    /// ISO/IEC 23001-7 §8.2 — `tenc` TrackEncryptionBox payload, when
    /// present. Lives inside `schi` inside `sinf` of an `enc*` sample
    /// entry. Carries the track-level defaults (KID, per-sample IV
    /// size, isProtected flag, plus v1 pattern-encryption block
    /// counts + constant IV) consumed by a downstream CENC decryptor.
    /// `None` when the track is unprotected or when the protected
    /// sample entry omits `tenc` (the spec marks it "Mandatory: No
    /// (Yes, for protected tracks)", so a missing tenc on an `enc*`
    /// entry is still parseable but the file is non-conforming).
    tenc: Option<TencBox>,
    /// §8.3.3 — `tref` (TrackReferenceBox) entries. Each pair is
    /// `(reference_type, track_IDs)` where `reference_type` is the FourCC
    /// of an inner `TrackReferenceTypeBox` (e.g. `chap`, `subt`, `cdsc`,
    /// `hint`, `font`, `hind`, `vdep`, `vplx`) and `track_IDs` is the
    /// raw `unsigned int(32)` array packed in that inner box's body.
    /// Empty when the file has no `tref`. A given `reference_type` may
    /// appear at most once (per spec — track-reference type boxes are
    /// keyed by their FourCC).
    tref: Vec<([u8; 4], Vec<u32>)>,
    /// §8.3.4 — `trgr` (TrackGroupBox) child entries. Each pair is
    /// `(track_group_type, track_group_id)`: the `TrackGroupTypeBox`
    /// FourCC names the grouping (`msrc` is the spec-named example for
    /// multi-source presentations) and the 32-bit `track_group_id` is
    /// the in-file identifier — two tracks sharing the same
    /// `(track_group_type, track_group_id)` pair belong to the same
    /// group. The outer `TrackGroupBox` itself is "Zero or one" per
    /// track but can hold an arbitrary list of typed child boxes; the
    /// spec does not forbid two children with the same
    /// `track_group_type` on the same track, so we preserve encounter
    /// order and surface each entry separately rather than
    /// de-duplicating by type. Track groups are **not** dependency
    /// relationships — that role belongs to `tref` (§8.3.3).
    trgr: Vec<([u8; 4], u32)>,
    /// §8.4.6 — `elng` (ExtendedLanguageBox). The NULL-terminated BCP 47
    /// (RFC 4646) language tag string (e.g. `en-US`, `fr-FR`, `zh-CN`).
    /// `None` when the track has no `elng` box, in which case callers
    /// should fall back to the packed 3-char language code in `mdhd`.
    /// The extended tag overrides the `mdhd` language when present
    /// (§8.4.6.1).
    elng: Option<String>,
    /// §8.10.4 — `kind` (KindBox) entries from the track-level `udta`.
    /// Each pair is `(schemeURI, value)`: a NULL-terminated URI followed
    /// by a NULL-terminated value string. When `value` is empty, the URI
    /// itself defines the kind (e.g. `urn:apple:hap:closed-captions`);
    /// when a value is present, the URI identifies a naming scheme and
    /// `value` is the kind name from that scheme (e.g.
    /// `urn:mpeg:dash:role:2011`, `main`). Zero or more per track —
    /// multiple `kind` boxes from different schemes can co-label the
    /// same track (spec example: two schemes both declaring "subtitles").
    kinds: Vec<(String, String)>,
    /// §8.10.2 — `cprt` (CopyrightBox) entries from the track-level
    /// `udta`. Each record pairs a packed-3-letter ISO 639-2/T language
    /// code with the decoded notice string. Quantity "zero or more":
    /// a producer can attach one box per language for a multi-lingual
    /// per-track copyright. Distinct from the lumped 3GPP-style
    /// metadata in `Mp4Demuxer::metadata` because the typed shape
    /// preserves the language tag. Empty when the track has no `cprt`
    /// (the common case).
    copyrights: Vec<CopyrightRecord>,
    /// §8.10.3 — `tsel` (TrackSelectionBox) from the track-level `udta`.
    /// The `switch_group` (signed; default 0) names a switching set
    /// within the alternate group declared on `tkhd` (§8.3.2): two
    /// tracks carrying the same non-zero `switch_group` are interchangeable
    /// at any point during playback (the consumer may switch between
    /// them on the fly, e.g. for bitrate adaptation). `attribute_list`
    /// is the list of FourCC tags drawn from §8.10.3.5's descriptive
    /// (`tesc`/`fgsc`/`cgsc`/`spsc`/`resc`/`vwsc`) and differentiating
    /// (`bitr` / `cdec` / `lang` / …) sets that characterise what the
    /// track offers. `None` when the track has no `tsel`.
    tsel: Option<TselBox>,
    /// §8.14.3 — `strk` (Sub Track Box) entries from the track-level
    /// `udta`. Each defines a sub track (part of this track assigned to
    /// alternate / switch groups for layered-codec media selection): the
    /// `stri` (§8.14.4) selection metadata plus zero or more `strd/stsg`
    /// (§8.14.6) sample-group definitions. Quantity "zero or more"
    /// (§8.14.3.1); order matches the on-disk order. Empty when the
    /// track defines no sub tracks (the common case).
    sub_tracks: Vec<SubTrack>,
    /// §8.6.1.4 — `cslg` (CompositionToDecodeBox). Present when signed
    /// composition offsets (a v1 `ctts`) are in use and the producer
    /// chose to document the composition↔decode timeline relationship.
    /// `None` when the track has no `cslg` (the common case for files
    /// without B-frame reordering, or files that simply omit the box).
    cslg: Option<CslgBox>,
    /// §12.1.2 / §8.4.5 — `vmhd` (VideoMediaHeaderBox). Present for
    /// video tracks (the spec marks it mandatory inside `minf` for
    /// video media). Carries the `graphicsmode` composition mode and
    /// `opcolor`. `None` for non-video tracks (which use `smhd` /
    /// `nmhd` / `sthd` instead) or when the box is absent.
    vmhd: Option<VmhdBox>,
    /// §8.6.3 — `stsh` (ShadowSyncSampleBox) entries. Each pair is
    /// `(shadowed_sample_number, sync_sample_number)`, both 1-based
    /// sample indices: when seeking to (or before) the non-sync
    /// `shadowed_sample_number`, the named `sync_sample_number` may be
    /// decoded in its place to recover a usable starting point. The
    /// table is sorted by `shadowed_sample_number` per spec and is
    /// purely an optional seek optimisation — it is ignored in normal
    /// forward play and a track decodes correctly without it. Empty
    /// when the box is absent (the common case).
    stsh: Vec<(u32, u32)>,
    /// §8.9.2 — `sbgp` (SampleToGroupBox) instances. A track may carry
    /// several, one per `grouping_type`. Each entry records the grouping
    /// type, the optional v1 `grouping_type_parameter`, and the
    /// run-length `(sample_count, group_description_index)` table that
    /// maps decode-order sample runs to a sample-group description index.
    /// Empty when the track has no `sbgp` (the common case).
    sbgp: Vec<SbgpBox>,
    /// §8.9.3 — `sgpd` (SampleGroupDescriptionBox) instances. One per
    /// `grouping_type`, each holding the per-group descriptive entries
    /// that an `sbgp` of the same type indexes. The entry payloads are
    /// grouping-type-specific blobs that the container surfaces verbatim
    /// (as hex) without interpreting them. Empty when the track has no
    /// `sgpd` (the common case).
    sgpd: Vec<SgpdBox>,
    /// §8.9.5 — `csgp` (CompactSampleToGroupBox) instances. The compact
    /// alternative to `sbgp`: each records the grouping type, the
    /// optional `grouping_type_parameter`, and a small set of repeating
    /// index patterns replicated across the track. Empty when the track
    /// has no `csgp` (the common case — `csgp` is a 2020+ addition).
    csgp: Vec<CsgpBox>,
    /// §8.6.4 — `sdtp` (SampleDependencyTypeBox). Per-sample dependency
    /// hints in decode order: one entry per sample, four 2-bit fields
    /// `(is_leading, sample_depends_on, sample_is_depended_on,
    /// sample_has_redundancy)` packed into a single byte each on disk.
    /// Empty when the track has no `sdtp` (the common case); a present
    /// table whose length is shorter than the track's sample_count
    /// is preserved verbatim (truncation is the producer's bug, not
    /// ours to invent zeros for).
    sdtp: Vec<SdtpEntry>,
    /// §8.5.3 — `stdp` (DegradationPriorityBox). Per-sample 16-bit
    /// `priority` values in decode order — the `sample_count` is
    /// implicit from `stsz` / `stz2` (mirroring `sdtp`). The exact
    /// meaning and acceptable range of `priority` are defined by
    /// specifications derived from BMFF (§8.5.3.1), so the container
    /// preserves the raw u16s without interpreting them. Empty when
    /// the track has no `stdp` (the common case).
    stdp: Vec<u16>,
    /// §8.7.6 — `padb` (PaddingBitsBox). Per-sample 3-bit
    /// `pad` counts in decode order, decoded from the packed
    /// `(reserved:1 + pad:3) × 2` per-byte nibble layout. The on-wire
    /// `sample_count` (declared inside the box, unlike `sdtp` / `stdp`)
    /// fixes the number of valid entries; the unused trailing nibble
    /// when `sample_count` is odd is dropped during parse. Empty when
    /// the track has no `padb` (the common case — only bitstream codecs
    /// whose samples do not end on a byte boundary need this).
    padb: Vec<u8>,
    /// §8.7.7 — `subs` (SubSampleInformationBox) instances. The spec
    /// allows more than one per container provided they differ in `flags`
    /// (the carried codec's semantics for `flags` distinguish them); we
    /// collect them in order of encounter. Empty when the track has no
    /// `subs` (the common case).
    subs: Vec<SubsBox>,
    /// §8.7.8 — `saiz` (SampleAuxiliaryInformationSizesBox) instances
    /// found inside `stbl`. The spec allows multiple, keyed by
    /// `(aux_info_type, aux_info_type_parameter)`. Each `saiz` pairs
    /// with a `saio` of the matching key in `saio`. Empty when the
    /// track has no `saiz` in `stbl` (the common case for
    /// non-protected, non-fragmented content).
    saiz: Vec<SaizBox>,
    /// §8.7.9 — `saio` (SampleAuxiliaryInformationOffsetsBox) instances
    /// found inside `stbl`. One per matching `saiz`; offsets are
    /// absolute file positions when read from `stbl`. Empty in the
    /// common case.
    saio: Vec<SaioBox>,
    /// §8.5.2 — every `stsd` (SampleDescriptionBox) entry, in on-disk
    /// order. Entry `[0]` is the one used for active decode dispatch
    /// (its FourCC is mirrored to `codec_id_fourcc`); entries `[1..]`
    /// are additional sample descriptions the track may switch to via a
    /// per-chunk `stsc.sample_description_index` (≥ 2) or, in fragments,
    /// a `tfhd` / `trex` `sample_description_index`. A track has more
    /// than one entry when the same media changes parameters mid-stream
    /// (e.g. a resolution / codec-config switch) without starting a new
    /// track. Each `StsdEntry` records the entry's FourCC plus the
    /// §8.5.2.2 `data_reference_index`. Never empty for a media track
    /// (a track always has at least one sample description); the vector
    /// is left empty only when the `stsd` was absent or `entry_count`
    /// was zero (both non-conforming).
    stsd_entries: Vec<StsdEntry>,
}

/// One `stsd` (SampleDescriptionBox, §8.5.2) entry header. Every
/// SampleEntry (§8.5.2.2) begins with the same 8-byte preamble — 6
/// `reserved` bytes then the 16-bit `data_reference_index` — regardless
/// of the concrete sample-entry class that follows. We capture that
/// common prefix plus the entry's FourCC; the codec-specific tail
/// (audio / video preamble + child config boxes) is parsed separately
/// for entry `[0]` via `parse_sample_entry`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StsdEntry {
    /// The sample-entry FourCC (`box` type of the SampleEntry). For a
    /// protected entry this is the outer `enc*` placeholder, not the
    /// un-transformed format (§8.12); the active-entry unwrap only
    /// rewrites `codec_id_fourcc`, not this raw record.
    format: [u8; 4],
    /// §8.5.2.2 `data_reference_index` — 1-based index into the track's
    /// `dref` table naming where the samples using this description are
    /// stored. Almost always `1` (samples in this same file).
    data_reference_index: u16,
}

/// One entry of an `sdtp` (SampleDependencyTypeBox, §8.6.4) — decoded
/// from the on-wire `unsigned int(2) is_leading; unsigned int(2)
/// sample_depends_on; unsigned int(2) sample_is_depended_on;
/// unsigned int(2) sample_has_redundancy;` 8-bit packing.
///
/// Each field uses the spec's four-valued enum (0 = "unknown", a
/// reserved value 3 in some fields). We surface the raw small ints
/// rather than a typed enum to stay format-agnostic — callers that
/// care about the named values consult §8.6.4.3 themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SdtpEntry {
    /// 2 bits — see §8.6.4.3. Values:
    /// `0` = leading nature unknown,
    /// `1` = leading with dependency before referenced I-picture (not
    /// decodable),
    /// `2` = not a leading sample,
    /// `3` = leading without dependency before referenced I-picture
    /// (decodable).
    is_leading: u8,
    /// 2 bits — see §8.6.4.3. Values:
    /// `0` = dependency unknown,
    /// `1` = depends on others (not an I-picture),
    /// `2` = does not depend on others (I-picture),
    /// `3` = reserved.
    sample_depends_on: u8,
    /// 2 bits — see §8.6.4.3. Values:
    /// `0` = whether others depend on this sample is unknown,
    /// `1` = other samples may depend on this one (not disposable),
    /// `2` = no other sample depends on this one (disposable),
    /// `3` = reserved.
    sample_is_depended_on: u8,
    /// 2 bits — see §8.6.4.3. Values:
    /// `0` = unknown whether there is redundant coding,
    /// `1` = there is redundant coding,
    /// `2` = there is no redundant coding,
    /// `3` = reserved.
    sample_has_redundancy: u8,
}

/// Parsed `sbgp` (SampleToGroupBox, ISO/IEC 14496-12 §8.9.2).
#[derive(Clone, Debug, Default)]
struct SbgpBox {
    /// Four-byte grouping type identifying which `sgpd` (same type) this
    /// box indexes — e.g. `rap `, `roll`, `sync`, `tele`, `alst`.
    grouping_type: [u8; 4],
    /// `grouping_type_parameter` — present only in version 1, selecting
    /// one of several alternative groupings of the same type. `None` for
    /// version 0.
    grouping_type_parameter: Option<u32>,
    /// Run-length `(sample_count, group_description_index)` pairs. A
    /// `group_description_index` of 0 means "member of no group of this
    /// type"; an index ≥ 0x10001 (movie-fragment-local) is preserved
    /// verbatim — the demuxer does not resolve fragment-local groups.
    entries: Vec<(u32, u32)>,
}

/// Parsed `sgpd` (SampleGroupDescriptionBox, ISO/IEC 14496-12 §8.9.3).
#[derive(Clone, Debug, Default)]
struct SgpdBox {
    /// Four-byte grouping type linking this description table to the
    /// `sbgp` of the same type.
    grouping_type: [u8; 4],
    /// `default_sample_description_index` (version ≥ 2): the group entry
    /// applied to samples not mapped by any `sbgp`. 0 (the default)
    /// means "no group of this type"; absent in versions 0/1.
    default_sample_description_index: Option<u32>,
    /// Per-group descriptive entries, each a grouping-type-specific
    /// opaque payload preserved verbatim. The container does not
    /// interpret them — interpretation belongs to the layer that knows
    /// the `grouping_type` semantics.
    entries: Vec<Vec<u8>>,
}

/// One pattern of a `csgp` (CompactSampleToGroupBox, §8.9.5): a run of
/// `pattern_length` per-sample `sample_group_description_index` values,
/// replicated across `sample_count` consecutive groups of that length.
#[derive(Clone, Debug, Default)]
struct CsgpPattern {
    /// `sample_count[i]` — number of consecutive groups (each
    /// `pattern_length` samples long) that replay this pattern. The
    /// pattern covers `sample_count * pattern_length` samples in total.
    sample_count: u32,
    /// `sample_group_description_index[i][1..=pattern_length]` — one
    /// index per sample of the pattern. Index 0 means "member of no
    /// group of this type"; when the box lives in a `traf`, the index's
    /// most-significant bit (for the field's width) distinguishes a
    /// fragment-local description (set) from a global one (clear). The
    /// raw value is preserved verbatim; the demuxer does not resolve it.
    indices: Vec<u32>,
}

/// Parsed `csgp` (CompactSampleToGroupBox, ISO/IEC 14496-12:2020 §8.9.5).
#[derive(Clone, Debug, Default)]
struct CsgpBox {
    /// Four-byte grouping type linking this box to the `sgpd` of the
    /// same type — identical role to `sbgp.grouping_type`.
    grouping_type: [u8; 4],
    /// `grouping_type_parameter` — present only when the flag layout's
    /// presence bit is set, selecting one of several alternative
    /// groupings of the same type. `None` when the bit is clear.
    grouping_type_parameter: Option<u32>,
    /// The repeating index patterns, in disk order.
    patterns: Vec<CsgpPattern>,
}

/// One sub-sample of a `subs` (SubSampleInformationBox, §8.7.7) entry.
///
/// Fields decoded from the on-wire `unsigned int(16 or 32) subsample_size;
/// unsigned int(8) subsample_priority; unsigned int(8) discardable;
/// unsigned int(32) codec_specific_parameters;` layout. The
/// `subsample_size` field is 16-bit at version 0 and widens to 32-bit at
/// version 1; we always store as `u32` so callers handle one shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SubSampleEntry {
    /// §8.7.7.3 — size in bytes of the sub-sample.
    subsample_size: u32,
    /// §8.7.7.3 — degradation priority. Higher = more important to
    /// decoded quality. Codec-specific scale; the container does not
    /// interpret it.
    subsample_priority: u8,
    /// §8.7.7.3 — `0` = required to decode the current sample; non-zero
    /// = optional (e.g. an SEI message that only enhances output).
    discardable: u8,
    /// §8.7.7.3 — opaque codec-specific blob (4 bytes). For AVC
    /// (ISO/IEC 14496-15) this encodes NAL-unit role / dependency
    /// information; absent a codec-specific binding the field is `0`.
    codec_specific_parameters: u32,
}

/// One entry of a `subs` (SubSampleInformationBox, §8.7.7) — a single
/// sample's sub-sample table together with the sparse delta that
/// addresses it.
#[derive(Clone, Debug, Default)]
struct SubsEntry {
    /// §8.7.7.3 — sample-number delta from the previous entry's sample
    /// (or from sample 0 for the first entry). The decoded absolute
    /// sample numbers are *not* materialised — callers walking the table
    /// can accumulate them themselves.
    sample_delta: u32,
    /// The sub-samples of this entry's sample, in disk order. May be
    /// empty (`subsample_count == 0`) per §8.7.7.1 — in that case the
    /// addressed sample has no sub-sample structure but is still
    /// enumerated by the table (the spec permits this shape so a
    /// producer can document "this sample is monolithic" alongside
    /// genuinely sub-divided neighbours).
    subsamples: Vec<SubSampleEntry>,
}

/// Parsed `subs` (SubSampleInformationBox, ISO/IEC 14496-12 §8.7.7).
///
/// The box has been a `FullBox(version, flags)` since the original 2003
/// edition. `version` selects the on-wire width of `subsample_size`
/// (16-bit at v0, 32-bit at v1); `flags` is owned by the carried codec
/// — when more than one `subs` is present in the same container, the
/// spec mandates that each carry a distinct `flags` value (§8.7.7.1).
/// We preserve both as-recorded so codec-specific consumers can pick
/// the table whose semantics they understand.
#[derive(Clone, Debug, Default)]
struct SubsBox {
    /// FullBox version (0 or 1) — determines `subsample_size` width on
    /// disk. We normalise to `u32` in `SubSampleEntry::subsample_size`.
    version: u8,
    /// FullBox flags — codec-specific. Distinguishes co-resident `subs`
    /// boxes per §8.7.7.1.
    flags: u32,
    /// One entry per *addressed* sample (the table is sparse — samples
    /// with no `subs` row contribute nothing here).
    entries: Vec<SubsEntry>,
}

/// Parsed `saiz` (SampleAuxiliaryInformationSizesBox, ISO/IEC 14496-12
/// §8.7.8).
///
/// The `aux_info_type` / `aux_info_type_parameter` pair is the lookup
/// key that pairs a `saiz` with its matching `saio` of the same key
/// (§8.7.8.1: "there must be a matching SampleAuxiliaryInformationOffsetsBox
/// with the same values of aux_info_type and aux_info_type_parameter").
/// Both fields are present on disk only when `flags & 1` is set; when
/// absent the implied value of `aux_info_type` is (a) the protection
/// scheme type for protected content (transformed sample entries) or
/// (b) the sample entry FourCC otherwise (§8.7.8.3 — that fallback is
/// not materialised here; we surface `None`/`None` so callers can
/// apply the rule themselves with the surrounding sinf/sample-entry
/// context).
///
/// `default_sample_info_size` is the constant-size shortcut: when
/// non-zero, every sample in the table has that size and `per_sample`
/// is empty. When zero, `per_sample[i]` holds the size for the i-th
/// sample. `sample_count` is stored so an over-long table (the spec
/// permits `sample_count` < total stsz/stz2 count — auxiliary info
/// supplied for the initial samples only) is preserved verbatim.
#[derive(Clone, Debug, Default)]
struct SaizBox {
    /// §8.7.8.3 `aux_info_type` — present only when `flags & 1` is set.
    aux_info_type: Option<[u8; 4]>,
    /// §8.7.8.3 `aux_info_type_parameter` — present only when
    /// `flags & 1` is set; defaults to 0 when omitted.
    aux_info_type_parameter: Option<u32>,
    /// §8.7.8.3 `default_sample_info_size`; non-zero means a
    /// constant-size table and `per_sample` is empty.
    default_sample_info_size: u8,
    /// §8.7.8.3 `sample_count` — the declared number of samples this
    /// `saiz` covers (may be smaller than the track's full sample count
    /// per §8.7.8.3).
    sample_count: u32,
    /// §8.7.8.2 `sample_info_size[]` — populated only when
    /// `default_sample_info_size == 0`; otherwise empty.
    per_sample: Vec<u8>,
}

/// Parsed `saio` (SampleAuxiliaryInformationOffsetsBox, ISO/IEC 14496-12
/// §8.7.9).
///
/// Same key shape as `SaizBox`: `(aux_info_type,
/// aux_info_type_parameter)` selects which auxiliary-information stream
/// these offsets belong to. The table itself is `entry_count` chunk
/// (or fragment-run) offsets; per §8.7.9.3 the count must be 1 (a
/// single contiguous chunk) or equal to the count of chunks (in `stbl`)
/// / `trun`s (in `traf`). When the box appears inside `stbl` the
/// offsets are absolute file positions; in `traf` they are relative to
/// the base_data_offset established by the surrounding `tfhd` (or to
/// the `moof` start when `default-base-is-moof` is set, per §8.8.14).
///
/// Both v0 (32-bit offsets) and v1 (64-bit offsets) are read and
/// widened to `u64` so callers handle one shape. `version` is preserved
/// so a producer round-tripping back to disk can re-emit the same
/// width.
#[derive(Clone, Debug, Default)]
struct SaioBox {
    /// FullBox version (0 or 1) — selects 32-bit / 64-bit on-disk
    /// offset width. Preserved so a round-trip can match the original
    /// layout.
    version: u8,
    /// §8.7.9.3 `aux_info_type` — present only when `flags & 1` is set.
    aux_info_type: Option<[u8; 4]>,
    /// §8.7.9.3 `aux_info_type_parameter` — present only when
    /// `flags & 1` is set; defaults to 0 when omitted.
    aux_info_type_parameter: Option<u32>,
    /// §8.7.9.2 `offset[entry_count]` — widened to u64 regardless of
    /// box version. Semantics depend on container: absolute file
    /// position inside `stbl`, base_data_offset-relative inside `traf`.
    offsets: Vec<u64>,
}

/// Parsed `cslg` (CompositionToDecodeBox, ISO/IEC 14496-12 §8.6.1.4).
///
/// All five fields are signed; version 0 stores them as 32-bit, version
/// 1 as 64-bit. We widen v0 to `i64` on read so callers handle one
/// shape. The values are in the track media timescale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CslgBox {
    /// Add this to every computed CTS to guarantee CTS ≥ DTS for all
    /// samples (honouring the profile/level buffer model). When
    /// `leastDecodeToDisplayDelta` is ≥ 0 this may be 0; otherwise it
    /// should be at least `-leastDecodeToDisplayDelta`.
    composition_to_dts_shift: i64,
    /// The smallest composition offset in the track's `ctts`.
    least_decode_to_display_delta: i64,
    /// The largest composition offset in the track's `ctts`.
    greatest_decode_to_display_delta: i64,
    /// The smallest computed CTS for any sample in this track's media.
    composition_start_time: i64,
    /// The CTS-plus-duration of the sample with the largest computed
    /// CTS; `0` means "composition end time unknown" (§8.6.1.4.3).
    composition_end_time: i64,
}

/// Parsed `vmhd` (VideoMediaHeaderBox, ISO/IEC 14496-12 §12.1.2,
/// defined per §8.4.5).
///
/// The media-type-specific header carried inside `minf` for a video
/// track. Its body, after the 4-byte `FullBox(version=0, flags=1)`
/// preamble, is a 16-bit `graphicsmode` and a `[u16; 3]` `opcolor`.
/// `graphicsmode == 0` is `copy` (copy over the existing image);
/// derived specifications may extend the set. `opcolor` is the
/// (red, green, blue) colour available to graphics modes that use one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VmhdBox {
    /// The composition mode for the video track. `0` is `copy`
    /// (§12.1.2.3); other values are defined by derived specifications.
    graphicsmode: u16,
    /// The three (red, green, blue) colour components available for use
    /// by graphics modes that consume an operation colour.
    opcolor: [u16; 3],
}

/// Parsed `tsel` (TrackSelectionBox, ISO/IEC 14496-12 §8.10.3).
///
/// The box carries two pieces of media-selection metadata for the
/// containing track:
///
/// * `switch_group` — a signed 32-bit identifier (§8.10.3.4) used by
///   the consumer to group tracks that are interchangeable *during*
///   playback (e.g. multi-bitrate renditions of the same stream where
///   a player may flip between them on a frame boundary). Zero means
///   "no information"; a non-zero value must match across the entire
///   switch set. Tracks in the same `switch_group` are required by
///   the spec to be in the same alternate group (§8.3.2) too.
/// * `attribute_list` — a list of FourCCs (§8.10.3.5), each one
///   *either* a descriptive tag (`tesc` = temporal scalability,
///   `fgsc` = fine-grain SNR, `cgsc` = coarse-grain SNR, `spsc` =
///   spatial, `resc` = region-of-interest, `vwsc` = view) *or* a
///   differentiating tag (`bitr` = bitrate, `cdec` = codec, `lang` =
///   language, …). The container preserves the on-disk byte order
///   verbatim — interpretation is delegated to the consumer that
///   knows the alternate-group semantics.
///
/// `template int(32)` in the spec syntax means the field has a
/// recommended default of 0; we still parse the value as a signed
/// 32-bit integer per the type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TselBox {
    /// `switch_group` (§8.10.3.4). Signed. 0 means "no information"
    /// (also the spec-recommended default when the box is absent).
    switch_group: i32,
    /// `attribute_list` (§8.10.3.5). Each entry is a 4-byte FourCC.
    /// Order matches the on-disk order. Empty when the box body
    /// carried only the `switch_group`.
    attribute_list: Vec<[u8; 4]>,
}

/// One Sub Track Sample Group (ISO/IEC 14496-12 §8.14.6, `stsg`).
///
/// Defines one slice of a sub track as the union of the sample groups —
/// of a single `grouping_type` — named by the listed `sgpd` (§8.9.3)
/// description indices. The container does not resolve the indices
/// against the matching `sgpd` table; it preserves the `(grouping_type,
/// indices)` pairing verbatim for a consumer that knows the grouping
/// semantics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SubTrackSampleGroup {
    /// `grouping_type` (§8.14.6.3). The same FourCC used by the matching
    /// `sbgp` (§8.9.2) and `sgpd` (§8.9.3) boxes on the parent track.
    grouping_type: [u8; 4],
    /// `group_description_index[]` (§8.14.6.3). Each entry indexes a
    /// `sgpd` entry of `grouping_type` that describes samples belonging
    /// to this sub track. Order matches the on-disk order.
    group_description_indices: Vec<u32>,
}

/// One parsed Sub Track (ISO/IEC 14496-12 §8.14.3, `strk`).
///
/// A sub track assigns *part* of the containing track to alternate /
/// switch groups (§8.14.1), letting a media-selection layer choose among
/// layered-codec alternatives (SVC / MVC temporal, spatial, SNR, or view
/// layers) that don't map onto whole-track boundaries. The mandatory
/// `stri` (§8.14.4) carries the selection metadata; the mandatory `strd`
/// (§8.14.5) holds zero or more `stsg` (§8.14.6) that define which sample
/// groups make up the sub track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SubTrack {
    /// `stri.switch_group` (§8.14.4.3). Signed; 0 (default) means "no
    /// switching information". Shares the global numbering with
    /// `tsel.switch_group` (§8.10.3) so a switch group can span track
    /// and sub-track boundaries. `None` when the `strk` carried no
    /// well-formed `stri` (the box is mandatory per §8.14.4.1, but a
    /// malformed entry is tolerated rather than aborting the demux).
    switch_group: i16,
    /// `stri.alternate_group` (§8.14.4.3). Signed; 0 (default) means "no
    /// information on relations to other tracks/sub-tracks". Shares the
    /// numbering with `tkhd.alternate_group` (§8.3.2).
    alternate_group: i16,
    /// `stri.sub_track_ID` (§8.14.4.3). A non-zero value uniquely
    /// identifies the sub track locally within the parent track; 0
    /// (default) means "not assigned".
    sub_track_id: u32,
    /// `stri.attribute_list[]` (§8.14.4.3). Each entry is a 4-byte
    /// FourCC drawn from the descriptive set (`tesc`, `fgsc`, `cgsc`,
    /// `spsc`, `resc`, `vwsc`) or the differentiating set (`bitr`,
    /// `frar`, `nvws`, …). Order matches the on-disk order. Empty when
    /// the `stri` body carried only the three group/id fields.
    attribute_list: Vec<[u8; 4]>,
    /// `strd/stsg` Sub Track Sample Group boxes (§8.14.6). Zero or more;
    /// order matches the on-disk order.
    sample_groups: Vec<SubTrackSampleGroup>,
}

/// Decoded `cprt` (CopyrightBox, ISO/IEC 14496-12 §8.10.2). Carried in a
/// `udta` container — `moov/udta` for a file-wide notice or `trak/udta`
/// for a per-track notice — and surfaced separately from the lumped
/// 3GPP-style `udta` metadata channel because the typed shape exposes
/// the language code alongside the notice (so a multilingual presentation
/// can be reconstructed without re-parsing the box).
///
/// Spec syntax (§8.10.2.2): `FullBox('cprt', 0, 0) { bit(1) pad = 0;
/// unsigned int(5)[3] language; string notice; }`. The 16-bit language
/// word packs three lower-case ISO 639-2/T characters, each stored as
/// `(ASCII - 0x60)`; the notice is a NULL-terminated UTF-8 (or
/// UTF-16BE-with-BOM) string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CopyrightRecord {
    /// ISO 639-2/T three-letter language code (lower-case ASCII),
    /// decoded from the packed 16-bit `language` word (§8.10.2.3).
    /// Each byte equals the literal character `'a'..='z'`. When the box
    /// carries the packed sentinel `\0\0\0` (i.e. `0x0000`, which decodes
    /// to `0x60 0x60 0x60` = backticks rather than letters), the field
    /// is left as ASCII backticks because the spec wording confines
    /// well-formed values to "lower-case letters" only — a producer
    /// that emitted a zero word is signalling "language unspecified"
    /// per the §8.10.2.3 well-formedness rule, and the reader leaves
    /// the literal decoded bytes for the caller to recognise.
    language: [u8; 3],
    /// The decoded copyright notice. UTF-8 by default; if the on-wire
    /// string opened with the UTF-16 BOM (0xFE 0xFF) the bytes were
    /// re-decoded as UTF-16BE. The trailing NUL terminator (§8.10.2.3)
    /// is stripped before storage.
    notice: String,
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
                parse_mvex(
                    &body,
                    &mut out.tracks,
                    &mut out.mehd_fragment_duration,
                    &mut out.leva,
                    &mut out.treps,
                )?;
            }
            PSSH => {
                // ISO/IEC 23001-7 §8.1 — ProtectionSystemSpecificHeaderBox.
                // Zero or more per moov; each is keyed by a 16-byte
                // SystemID UUID identifying a DRM. Parsed structurally
                // and surfaced through the demuxer's metadata channel
                // (no decryption — this crate only carries the box).
                // A malformed pssh is non-fatal: drop it but keep the
                // file parseable (DRM-free playback may still be
                // intended, and an opaque DRM box should not brick the
                // demuxer).
                let body = read_bytes_vec(&mut cur, psz)?;
                if let Ok(p) = parse_pssh(&body) {
                    out.psshes.push(p);
                }
            }
            _ => {
                skip_cursor_bytes(&mut cur, psz);
            }
        }
    }
    Ok(out)
}

/// §8.8.1 — `mvex` (MovieExtendsBox). Container for `trex` boxes that
/// supply per-track defaults consumed by the fragment parser, an
/// optional `mehd` MovieExtendsHeaderBox carrying the overall
/// presentation duration of a fragmented movie, and (§8.8.13) an
/// optional `leva` LevelAssignmentBox mapping tracks / sample groups /
/// sub-tracks to fragment levels.
fn parse_mvex(
    body: &[u8],
    tracks: &mut [Track],
    mehd_fragment_duration: &mut Option<u64>,
    leva: &mut Option<LevaRecord>,
    treps: &mut Vec<TrepRecord>,
) -> Result<()> {
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
            // §8.8.2 — `mehd` MovieExtendsHeaderBox. Optional FullBox
            // (`Quantity: Zero or one`). v0 → 32-bit fragment_duration;
            // v1 → 64-bit. Widened to u64 either way. The recorded
            // value is in the movie timescale (per §8.8.2.3
            // "indicated in the Movie Header Box"). A malformed mehd
            // is non-fatal: we drop it but keep the file parseable
            // (the demuxer falls back to mvhd.duration in that case).
            MEHD => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Ok(d) = parse_mehd(&b) {
                    // Spec allows only one per mvex; if a second
                    // mehd appears we keep the first (defensive — the
                    // file is malformed and the rest of our parser
                    // also takes the first occurrence of single-shot
                    // boxes).
                    if mehd_fragment_duration.is_none() {
                        *mehd_fragment_duration = Some(d);
                    }
                }
            }
            // §8.8.13 — `leva` LevelAssignmentBox. Optional FullBox
            // (quantity zero or one per file). Maps tracks / sample
            // groups / sub-tracks to "levels" inside subsequent moof
            // fragments so a downstream consumer (DASH subsegment
            // selector, scalable-bitstream layer picker) can skip
            // levels it doesn't need. The demuxer captures the first
            // instance seen and surfaces it through
            // `Mp4Demuxer::leva_entries()` plus the flat `leva_count`
            // / `leva_<n>` metadata channel. A malformed leva is
            // non-fatal: dropped, file remains parseable.
            LEVA => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if leva.is_none() {
                    if let Ok(r) = parse_leva(&b) {
                        *leva = Some(r);
                    }
                }
            }
            // §8.8.15 — `trep` TrackExtensionPropertiesBox. Optional
            // FullBox (quantity zero or more, zero or one per track).
            // Documents/summarises a track's characteristics in the
            // subsequent movie fragments; its body is `track_id`
            // followed by "any number of boxes". The demuxer records
            // each instance (track_id + the type/length of each child
            // box) and surfaces them through `Mp4Demuxer::treps()` plus
            // the flat `trep_<n>` metadata channel. A malformed trep is
            // non-fatal: dropped, file remains parseable.
            TREP => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Ok(r) = parse_trep(&b) {
                    treps.push(r);
                }
            }
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }
    Ok(())
}

/// §8.8.2 — `mehd` MovieExtendsHeaderBox.
///
/// Layout: FullBox preamble (4 bytes: version + flags) followed by a
/// single `fragment_duration` field — 4 bytes (v0) or 8 bytes (v1) —
/// in the movie timescale.
///
/// Returns the duration widened to `u64`. v0 values are zero-extended;
/// v1 values are read big-endian directly. An unknown version is
/// treated as malformed (the spec defines only 0 and 1).
fn parse_mehd(body: &[u8]) -> Result<u64> {
    if body.is_empty() {
        return Err(Error::invalid("MP4: mehd empty"));
    }
    let version = body[0];
    match version {
        0 => {
            if body.len() < 8 {
                return Err(Error::invalid("MP4: mehd v0 too short"));
            }
            Ok(u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as u64)
        }
        1 => {
            if body.len() < 12 {
                return Err(Error::invalid("MP4: mehd v1 too short"));
            }
            Ok(u64::from_be_bytes([
                body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
            ]))
        }
        _ => Err(Error::invalid("MP4: mehd unknown version")),
    }
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
        protection_scheme: None,
        tenc: None,
        tref: Vec::new(),
        trgr: Vec::new(),
        elng: None,
        kinds: Vec::new(),
        copyrights: Vec::new(),
        tsel: None,
        sub_tracks: Vec::new(),
        cslg: None,
        vmhd: None,
        stsh: Vec::new(),
        sbgp: Vec::new(),
        sgpd: Vec::new(),
        csgp: Vec::new(),
        sdtp: Vec::new(),
        stdp: Vec::new(),
        padb: Vec::new(),
        subs: Vec::new(),
        saiz: Vec::new(),
        saio: Vec::new(),
        stsd_entries: Vec::new(),
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
            TREF => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_tref(&sub, &mut t)?;
            }
            TRGR => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_trgr(&sub, &mut t)?;
            }
            UDTA => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_track_udta(&sub, &mut t);
            }
            _ => {
                skip_cursor_bytes(&mut cur, psz);
            }
        }
    }
    if has_media {
        Ok(Some(t))
    } else {
        Ok(None)
    }
}

/// Walk a *track-level* `udta` body and pick up any boxes whose semantics
/// are per-track rather than per-movie. The moov-level `udta` parser
/// (`parse_udta`) handles file-wide 3GPP / iTunes metadata; the
/// track-level container additionally hosts `kind` (§8.10.4), `tsel`
/// (§8.10.3), and `cprt` (§8.10.2 — per-track copyright). The `cprt`
/// handler here is the BMFF typed-accessor path that preserves the
/// 16-bit packed ISO 639-2/T language code alongside the notice — the
/// 3GPP-style `udta` aggregator in `parse_udta` lumps `cprt` into the
/// generic `(key, value)` metadata channel and drops the language tag.
/// Other children (e.g. legacy `titl` overrides) are skipped so a
/// future round can add them without changing the entry point.
fn parse_track_udta(body: &[u8], t: &mut Track) {
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
        match hdr.fourcc {
            KIND => parse_kind(&body[start..start + psz], t),
            TSEL => parse_tsel(&body[start..start + psz], t),
            STRK => parse_strk(&body[start..start + psz], t),
            CPRT => parse_cprt(&body[start..start + psz], t),
            _ => {
                // Other track-udta children (e.g. legacy `titl` overrides)
                // are skipped — file-wide metadata is collected from the
                // moov-level udta by `parse_udta`.
            }
        }
    }
}

/// §8.10.2 — `cprt` (CopyrightBox) typed accessor. FullBox preamble
/// (1 version + 3 flags) followed by a 16-bit packed language word and
/// then the NULL-terminated notice string.
///
/// Language decoding (§8.10.2.3): the 16-bit big-endian word splits as
/// `1 pad bit + 5 * 3` so the three 5-bit fields are read MSB-first; each
/// field equals `(ASCII - 0x60)` for one lower-case ISO 639-2/T character.
/// We reverse the offset to recover the literal character. A value
/// outside `0x01..=0x1A` (i.e. not a valid 'a'..'z' offset) is left in
/// the raw bits — the spec confines well-formed inputs to lowercase
/// letters, so an out-of-range nibble signals a producer that emitted
/// a zero / sentinel word; the reader records the literal decoded bytes
/// for the caller to recognise.
///
/// Notice decoding (§8.10.2.3): the bytes immediately after the language
/// word are the C-string notice. When the first two bytes are the
/// UTF-16 BOM (0xFE 0xFF) the remainder is interpreted as UTF-16BE per
/// the spec; otherwise the bytes are UTF-8. In both cases the trailing
/// NUL terminator is stripped before storage. An empty notice (just a
/// terminator) is preserved as an empty string — that still represents
/// a well-formed declaration of language and is not the same as the
/// absence of a `cprt` box.
///
/// Robustness: a non-zero FullBox version is unknown to this spec
/// revision and the box is dropped; a body too short to hold the
/// FullBox + language word is dropped; a notice that fails UTF-8
/// validation is replaced lossily (matching `decode_utf8_or_utf16`'s
/// posture) so a corrupt notice never aborts the demux.
fn parse_cprt(body: &[u8], t: &mut Track) {
    // FullBox preamble (1 version + 3 flags) + 16-bit packed language.
    if body.len() < 6 {
        return;
    }
    let version = body[0];
    if version != 0 {
        // §8.10.2.2 pins the box to version 0.
        return;
    }
    // 16-bit packed language word: bit 15 is the pad, then three 5-bit
    // characters. Each 5-bit value is (ASCII - 0x60); reverse the offset
    // to recover the literal lowercase ASCII letter (a..z = 0x61..0x7A,
    // offsets 0x01..0x1A).
    let packed = u16::from_be_bytes([body[4], body[5]]);
    let c0 = ((packed >> 10) & 0x1F) as u8;
    let c1 = ((packed >> 5) & 0x1F) as u8;
    let c2 = (packed & 0x1F) as u8;
    let language = [c0 + 0x60, c1 + 0x60, c2 + 0x60];
    // Notice: UTF-16BE if it opens with the BOM (0xFE 0xFF), UTF-8
    // otherwise. Strip the trailing NUL terminator per §8.10.2.3.
    let notice = decode_utf8_or_utf16(&body[6..]);
    t.copyrights.push(CopyrightRecord { language, notice });
}

/// §8.10.4 — `kind` (KindBox). FullBox preamble (version 0, flags 0)
/// followed by two NULL-terminated UTF-8 C strings: `schemeURI` then
/// `value`. Per §8.10.4.3 the URI alone identifies the kind when no
/// value follows; when a value is present the URI identifies the
/// naming scheme and `value` is the name from that scheme.
///
/// Spec quirk: both strings are stored with their terminators, so a
/// well-formed box body is at least `4 (FullBox) + 1 (URI NUL) +
/// 1 (value NUL) = 6` bytes. A box too short to hold both terminators
/// is tolerated by reading whatever fits — the box is informational
/// and a malformed entry should not abort the demux. Multiple `kind`
/// boxes may appear in a single track-level `udta`; each is appended
/// (the spec explicitly allows "More than one of these" with different
/// schemes — e.g. one DASH role plus one iTunes role).
fn parse_kind(body: &[u8], t: &mut Track) {
    // FullBox preamble (1 version + 3 flags).
    if body.len() < 4 {
        return;
    }
    let s = &body[4..];
    // Split at first NUL: that is the schemeURI terminator. If no NUL
    // is found, treat the entire remainder as the URI and `value` as
    // empty (tolerant read; matches `parse_elng`'s posture).
    let uri_end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    let uri = match std::str::from_utf8(&s[..uri_end]) {
        Ok(u) => u.to_string(),
        Err(_) => return,
    };
    // The value string starts immediately after the URI's NUL (if any).
    // Take everything up to the next NUL; an absent NUL means "read to
    // end of box". Both an explicitly-empty value (URI\0\0) and an
    // absent value (URI\0 with nothing after) decode to "".
    let value = if uri_end >= s.len() {
        String::new()
    } else {
        let rest = &s[uri_end + 1..];
        let val_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        match std::str::from_utf8(&rest[..val_end]) {
            Ok(v) => v.to_string(),
            Err(_) => return,
        }
    };
    // §8.10.4.3 — at least the URI must identify *something*. An
    // empty URI yields a useless entry; drop it rather than surfacing
    // a bogus kind on `params.options`.
    if uri.is_empty() {
        return;
    }
    t.kinds.push((uri, value));
}

/// §8.10.3 — `tsel` (TrackSelectionBox).
///
/// Spec syntax (§8.10.3.3):
/// ```text
/// aligned(8) class TrackSelectionBox
/// extends FullBox('tsel', version = 0, 0) {
///   template int(32) switch_group = 0;
///   unsigned int(32) attribute_list[]; // to end of the box
/// }
/// ```
///
/// Body layout after the 4-byte FullBox preamble:
///   * 4-byte signed big-endian `switch_group`.
///   * Zero or more 4-byte FourCCs (the `attribute_list`).
///
/// A box with an unknown FullBox version (the spec pins it to 0) or a
/// body too short to hold the FullBox + `switch_group` is silently
/// dropped — the box is informational and a corrupted entry should
/// never abort the demux (mirroring `parse_kind`'s posture).
///
/// Trailing bytes that don't make up a complete 4-byte FourCC are
/// ignored: the spec wording is `attribute_list[]` "to end of the box",
/// so a producer that miscalculated the box size is tolerated by
/// taking only the complete entries (matches the §8.7.7 `subs` and
/// §8.16.3 `sidx` "trailing partial record" handling elsewhere in this
/// crate).
fn parse_tsel(body: &[u8], t: &mut Track) {
    // FullBox preamble (1 version + 3 flags) plus 4-byte switch_group.
    if body.len() < 8 {
        return;
    }
    let version = body[0];
    if version != 0 {
        // Spec pins it to 0. Drop rather than mis-parse a derived-spec
        // extension we don't recognise.
        return;
    }
    let switch_group = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let mut attribute_list = Vec::new();
    let mut i = 8;
    while i + 4 <= body.len() {
        let attr: [u8; 4] = [body[i], body[i + 1], body[i + 2], body[i + 3]];
        attribute_list.push(attr);
        i += 4;
    }
    // A trailing 1..3-byte fragment is silently dropped — see doc note.
    t.tsel = Some(TselBox {
        switch_group,
        attribute_list,
    });
}

/// Render a 4-byte FourCC for surfacing on `params.options`: the printable
/// ASCII string when every byte is non-control valid UTF-8, otherwise an
/// 8-digit lowercase hex fallback. Mirrors the inline closure used by the
/// `tsel_attributes` / `tref_<type>` surfacing so the sub-track attribute
/// and grouping-type tokens read identically.
fn fourcc_token(a: &[u8; 4]) -> String {
    std::str::from_utf8(a)
        .ok()
        .filter(|s| s.chars().all(|c| !c.is_control()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{:02x}{:02x}{:02x}{:02x}", a[0], a[1], a[2], a[3]))
}

/// §8.14.3 — `strk` (Sub Track Box).
///
/// Spec syntax (§8.14.3.2): `aligned(8) class SubTrack extends
/// Box('strk') {}` — a plain `Box` (no FullBox preamble) whose body is a
/// container. It holds a mandatory `stri` (Sub Track Information,
/// §8.14.4) and a mandatory `strd` (Sub Track Definition, §8.14.5). We
/// walk the child boxes, parse the `stri` selection metadata, and recurse
/// one level into `strd` to collect its `stsg` (Sub Track Sample Group,
/// §8.14.6) children.
///
/// Robustness: `stri` and `strd` are both mandatory (§8.14.4.1 /
/// §8.14.5.1), but a `strk` missing one of them — or carrying a malformed
/// `stri` — is tolerated rather than aborting the demux (the box is
/// informational, mirroring `parse_tsel` / `parse_kind`). A `strk` whose
/// `stri` fails to parse contributes no `SubTrack` (there is nothing
/// useful to surface without the selection fields); a present `stri` with
/// a missing or empty `strd` still surfaces the selection metadata with
/// an empty `sample_groups`.
fn parse_strk(body: &[u8], t: &mut Track) {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    let mut info: Option<SubTrack> = None;
    let mut sample_groups: Vec<SubTrackSampleGroup> = Vec::new();
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
        match hdr.fourcc {
            STRI => info = parse_stri(&body[start..start + psz]),
            STRD => sample_groups = parse_strd(&body[start..start + psz]),
            _ => {
                // Other `strk` children (codec-specific definitions, e.g.
                // SVC/MVC tier boxes from ISO/IEC 14496-15) are skipped —
                // §8.14.1 leaves the door open for media-specific
                // definitions a future round can recognise.
            }
        }
    }
    // `stri` is mandatory (§8.14.4.1); without it we have no selection
    // metadata to surface, so the whole `strk` is dropped. With it, the
    // `strd/stsg` definitions (which may be empty) are attached.
    if let Some(mut st) = info {
        st.sample_groups = sample_groups;
        t.sub_tracks.push(st);
    }
}

/// §8.14.4 — `stri` (Sub Track Information Box).
///
/// Spec syntax (§8.14.4.2):
/// ```text
/// aligned(8) class SubTrackInformation
///   extends FullBox('stri', version = 0, 0) {
///   template int(16) switch_group = 0;
///   template int(16) alternate_group = 0;
///   template unsigned int(32) sub_track_ID = 0;
///   unsigned int(32) attribute_list[]; // to the end of the box
/// }
/// ```
///
/// Body layout after the 4-byte FullBox preamble:
///   * 2-byte signed big-endian `switch_group`.
///   * 2-byte signed big-endian `alternate_group`.
///   * 4-byte unsigned big-endian `sub_track_ID`.
///   * Zero or more 4-byte FourCCs (the `attribute_list`).
///
/// Returns `None` for an unknown FullBox version (the spec pins it to 0)
/// or a body too short to hold the FullBox + the three fixed fields. A
/// trailing 1..3-byte fragment that doesn't complete a 4-byte FourCC is
/// ignored ("attribute_list[] to the end of the box"), matching the
/// `parse_tsel` posture.
fn parse_stri(body: &[u8]) -> Option<SubTrack> {
    // FullBox preamble (1 version + 3 flags) + 2 + 2 + 4 fixed fields.
    if body.len() < 12 {
        return None;
    }
    let version = body[0];
    if version != 0 {
        // §8.14.4.2 pins the box to version 0.
        return None;
    }
    let switch_group = i16::from_be_bytes([body[4], body[5]]);
    let alternate_group = i16::from_be_bytes([body[6], body[7]]);
    let sub_track_id = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let mut attribute_list = Vec::new();
    let mut i = 12;
    while i + 4 <= body.len() {
        let attr: [u8; 4] = [body[i], body[i + 1], body[i + 2], body[i + 3]];
        attribute_list.push(attr);
        i += 4;
    }
    Some(SubTrack {
        switch_group,
        alternate_group,
        sub_track_id,
        attribute_list,
        sample_groups: Vec::new(),
    })
}

/// §8.14.5 — `strd` (Sub Track Definition Box).
///
/// Spec syntax (§8.14.5.2): `aligned(8) class SubTrackDefinition extends
/// Box('strd') {}` — a plain `Box` container holding zero or more `stsg`
/// (§8.14.6) for the generic (non-codec-specific) sub-track definition
/// mechanism. Walks the children and returns the parsed `stsg` records in
/// on-disk order. Children other than `stsg` (codec-specific definitions)
/// are skipped.
fn parse_strd(body: &[u8]) -> Vec<SubTrackSampleGroup> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    let mut out = Vec::new();
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
        if hdr.fourcc == STSG {
            if let Some(sg) = parse_stsg(&body[start..start + psz]) {
                out.push(sg);
            }
        }
    }
    out
}

/// §8.14.6 — `stsg` (Sub Track Sample Group Box).
///
/// Spec syntax (§8.14.6.2):
/// ```text
/// aligned(8) class SubTrackSampleGroupBox extends FullBox('stsg', 0, 0) {
///   unsigned int(32) grouping_type;
///   unsigned int(16) item_count;
///   for (i = 0; i < item_count; i++)
///     unsigned int(32) group_description_index;
/// }
/// ```
///
/// Body layout after the 4-byte FullBox preamble:
///   * 4-byte `grouping_type` FourCC.
///   * 2-byte big-endian `item_count`.
///   * `item_count` 4-byte big-endian `group_description_index` values.
///
/// Returns `None` for an unknown FullBox version, a body too short for
/// the FullBox + `grouping_type` + `item_count`, or a declared
/// `item_count` that overruns the bytes actually present (a truncated
/// index list would lie about which sample groups make up the sub track,
/// so it is rejected rather than silently shortened).
fn parse_stsg(body: &[u8]) -> Option<SubTrackSampleGroup> {
    // FullBox preamble (4) + grouping_type (4) + item_count (2).
    if body.len() < 10 {
        return None;
    }
    let version = body[0];
    if version != 0 {
        // §8.14.6.2 pins the box to version 0.
        return None;
    }
    let grouping_type: [u8; 4] = [body[4], body[5], body[6], body[7]];
    let item_count = u16::from_be_bytes([body[8], body[9]]) as usize;
    // Each entry is a 4-byte index; reject a count that overruns the body.
    if body.len() < 10 + item_count * 4 {
        return None;
    }
    let mut group_description_indices = Vec::with_capacity(item_count);
    let mut i = 10;
    for _ in 0..item_count {
        group_description_indices.push(u32::from_be_bytes([
            body[i],
            body[i + 1],
            body[i + 2],
            body[i + 3],
        ]));
        i += 4;
    }
    Some(SubTrackSampleGroup {
        grouping_type,
        group_description_indices,
    })
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

/// §8.3.3 — `tref` (TrackReferenceBox).
///
/// Spec syntax (§8.3.3.2):
/// ```text
/// aligned(8) class TrackReferenceBox extends Box('tref') {
/// }
/// aligned(8) class TrackReferenceTypeBox (unsigned int(32) reference_type)
///     extends Box(reference_type) {
///   unsigned int(32) track_IDs[];   // array fills the box
/// }
/// ```
///
/// The outer `tref` is a plain container (no FullBox version/flags) whose
/// children are typed reference boxes. Each child's *FourCC* is the
/// `reference_type` (e.g. `hint`, `cdsc`, `font`, `hind`, `vdep`, `vplx`,
/// `subt`, `chap`, `tmcd`) and its body is a packed array of `u32`
/// `track_ID`s (big-endian per §4.2 file-format conventions). The array
/// sizes itself by filling the box payload.
///
/// Per §8.3.3.3 `track_ID` is "never zero". We tolerate (skip) zero
/// entries rather than rejecting the file outright — some hand-rolled
/// muxers pad the array.
fn parse_tref(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let child = read_bytes_vec(&mut cur, psz)?;
        // §8.3.3.2 — child body is "unsigned int(32) track_IDs[]"; size
        // not stored, derived from box length. Reject malformed
        // sub-boxes whose body isn't a whole number of u32s — that's a
        // structural error, not a "skip me" hint.
        if child.len() % 4 != 0 {
            return Err(Error::invalid("MP4: tref child not a multiple of 4 bytes"));
        }
        let mut ids = Vec::with_capacity(child.len() / 4);
        for chunk in child.chunks_exact(4) {
            let id = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if id != 0 {
                ids.push(id);
            }
        }
        // Per spec a given reference_type appears at most once. If a
        // file violates that, keep the first occurrence and discard the
        // rest — silently chaining them together would surface bogus
        // (track,reference_type) pairs.
        if !t.tref.iter().any(|(ty, _)| *ty == hdr.fourcc) {
            t.tref.push((hdr.fourcc, ids));
        }
    }
    Ok(())
}

/// §8.3.4 — `trgr` (TrackGroupBox).
///
/// Spec syntax (§8.3.4.2):
/// ```text
/// aligned(8) class TrackGroupBox('trgr') {
/// }
/// aligned(8) class TrackGroupTypeBox (unsigned int(32) track_group_type)
///     extends FullBox(track_group_type, version = 0, flags = 0) {
///   unsigned int(32) track_group_id;
///   // the remaining data may be specified for a particular
///   // track_group_type
/// }
/// ```
///
/// The outer `trgr` is a plain container whose children are typed
/// FullBoxes — each child's FourCC IS the `track_group_type` (the
/// spec-named example is `msrc`, for multi-source presentations).
/// Each child body is a 4-byte `FullBox` preamble (version + 24-bit
/// flags, both fixed to zero per §8.3.4.2) followed by the 32-bit
/// `track_group_id`. Any trailing bytes are reserved for a
/// per-`track_group_type` extension and silently ignored at this
/// layer — a future derived spec adding fields can land here without
/// changing the parse contract.
///
/// Two tracks sharing the same `(track_group_type, track_group_id)`
/// pair belong to the same group (§8.3.4.3). Per §8.3.4.1 a group is
/// **not** a dependency relationship — that role belongs to `tref`
/// (§8.3.3).
///
/// The spec does not forbid two children of the same `track_group_type`
/// on the same track, so we preserve encounter order in `t.trgr` rather
/// than de-duplicating by type. A child whose body is shorter than the
/// 8-byte preamble + id is a structural error and aborts the parse
/// (the file is malformed); a child whose preamble has non-zero
/// version is silently skipped (the spec pins version = 0, so a
/// non-zero value is a forward-compatible extension we cannot interpret
/// safely).
fn parse_trgr(body: &[u8], t: &mut Track) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let child = read_bytes_vec(&mut cur, psz)?;
        // FullBox preamble (4) + track_group_id (4) = 8 bytes minimum.
        if child.len() < 8 {
            return Err(Error::invalid(
                "MP4: trgr child too short for FullBox + track_group_id",
            ));
        }
        // §8.3.4.2 pins version = 0. A different version means a
        // forward-compatible extension we cannot decode safely; skip
        // the child rather than mis-parsing it.
        let version = child[0];
        if version != 0 {
            continue;
        }
        let track_group_id = u32::from_be_bytes([child[4], child[5], child[6], child[7]]);
        t.trgr.push((hdr.fourcc, track_group_id));
    }
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
            _ => skip_cursor_bytes(&mut cur, psz),
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
            ELNG => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_elng(&b, t);
            }
            HDLR => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_hdlr(&b, t)?;
            }
            MINF => {
                let b = read_bytes_vec(&mut cur, psz)?;
                parse_minf(&b, t)?;
            }
            _ => skip_cursor_bytes(&mut cur, psz),
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

/// Parse an `elng` (ExtendedLanguageBox, ISO/IEC 14496-12 §8.4.6).
///
/// Layout: a `FullBox` preamble (1 version byte + 3 flag bytes, both
/// always zero per §8.4.6.2) followed by a single NULL-terminated UTF-8
/// C string `extended_language` holding a BCP 47 / RFC 4646 tag
/// (`en-US`, `fr-FR`, `zh-CN`, …). The tag is read up to the first NUL
/// (a trailing NUL is required by the spec but tolerated absent); any
/// bytes after the NUL are ignored. A malformed (too-short or non-UTF-8)
/// box is silently skipped rather than failing the demux — the box is
/// optional and a player can always fall back to `mdhd`'s packed code.
fn parse_elng(body: &[u8], t: &mut Track) {
    // FullBox preamble is 4 bytes; anything shorter has no string.
    if body.len() < 4 {
        return;
    }
    let s = &body[4..];
    // Take everything up to the first NUL terminator (or the whole
    // remainder if no NUL is present).
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    if let Ok(tag) = std::str::from_utf8(&s[..end]) {
        if !tag.is_empty() {
            t.elng = Some(tag.to_string());
        }
    }
}

fn parse_hdlr(body: &[u8], t: &mut Track) -> Result<()> {
    // FullBox (4 bytes), pre_defined (4 bytes), handler_type (4 bytes
    // at offset 8). ISO/IEC 14496-12 §8.4.3.2.
    if body.len() < 12 {
        return Err(Error::invalid("MP4: hdlr too short"));
    }
    let mut handler = [0u8; 4];
    handler.copy_from_slice(&body[8..12]);
    t.media_type = match &handler {
        h if *h == HANDLER_SOUN => MediaType::Audio,
        h if *h == HANDLER_VIDE => MediaType::Video,
        // `subt` (BMFF §12.6.1), `sbtl` (QuickTime), `text` (BMFF
        // §12.5.1 timed text — `tx3g` lives here). All three are
        // surfaced as MediaType::Subtitle so callers can route them
        // through their subtitle pipeline.
        h if *h == HANDLER_SUBT || *h == HANDLER_SBTL || *h == HANDLER_TEXT => MediaType::Subtitle,
        // `meta` — timed metadata (BMFF §8.11). Stays as Data; no
        // subtitle dispatch.
        h if *h == HANDLER_META => MediaType::Data,
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
            VMHD => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                // §12.1.2 fixes the quantity at exactly one; keep the
                // first instance seen and ignore any stray duplicates.
                if t.vmhd.is_none() {
                    if let Ok(v) = parse_vmhd(&sub) {
                        t.vmhd = Some(v);
                    }
                }
            }
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }
    Ok(())
}

/// Parse `vmhd` (VideoMediaHeaderBox, ISO/IEC 14496-12 §12.1.2 /
/// §8.4.5).
///
/// The body is a 4-byte `FullBox(version=0, flags=1)` preamble followed
/// by `unsigned int(16) graphicsmode` and `unsigned int(16)[3] opcolor`
/// — eight payload bytes after the preamble, twelve total. The `version`
/// is not enforced (the spec fixes it at 0, but we tolerate a non-zero
/// value rather than dropping a usable header); `flags` is likewise not
/// required to equal 1. A body shorter than the full 12 bytes is
/// rejected — a partial header would surface an `opcolor` component that
/// is really truncation noise.
fn parse_vmhd(body: &[u8]) -> Result<VmhdBox> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: vmhd too short"));
    }
    let graphicsmode = u16::from_be_bytes([body[4], body[5]]);
    let opcolor = [
        u16::from_be_bytes([body[6], body[7]]),
        u16::from_be_bytes([body[8], body[9]]),
        u16::from_be_bytes([body[10], body[11]]),
    ];
    Ok(VmhdBox {
        graphicsmode,
        opcolor,
    })
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
            STSH => t.stsh = parse_stsh(&b)?,
            SDTP => t.sdtp = parse_sdtp(&b)?,
            STDP => t.stdp = parse_stdp(&b)?,
            PADB => t.padb = parse_padb(&b)?,
            CTTS => t.ctts = parse_ctts(&b)?,
            CSLG => t.cslg = Some(parse_cslg(&b)?),
            SBGP => t.sbgp.push(parse_sbgp(&b)?),
            SGPD => t.sgpd.push(parse_sgpd(&b)?),
            CSGP => t.csgp.push(parse_csgp(&b)?),
            SUBS => t.subs.push(parse_subs(&b)?),
            SAIZ => t.saiz.push(parse_saiz(&b)?),
            SAIO => t.saio.push(parse_saio(&b)?),
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
    // ISO/IEC 14496-12 §8.5.2: walk **all** SampleEntry instances, not
    // just the first. The first entry drives active decode dispatch; the
    // rest are recorded so callers can resolve a `stsc` / `tfhd`
    // `sample_description_index` ≥ 2 to the FourCC of the description it
    // selects. Each SampleEntry shares the §8.5.2.2 8-byte preamble
    // (`reserved[6]` + `data_reference_index`), which we capture per
    // entry; the codec-specific tail of entry [0] is parsed below.
    let mut cur = std::io::Cursor::new(&body[8..]);
    let mut first_entry: Option<(BoxHeader, Vec<u8>)> = None;
    // Bound the recorded-entries vector by the byte budget so a forged
    // `entry_count` can't trigger a giant up-front allocation: the
    // smallest possible SampleEntry is its 8-byte box header plus the
    // 8-byte preamble (16 bytes), so the body can hold at most
    // `body.len() / 16` real entries.
    let max_entries = (body.len() / 16).max(1);
    t.stsd_entries
        .reserve(entry_count.min(max_entries as u32) as usize);
    for i in 0..entry_count {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => {
                if i == 0 {
                    return Err(Error::invalid("MP4: stsd first entry missing"));
                }
                // A truncated trailing entry: the declared `entry_count`
                // over-counts the bytes actually present. Stop at what we
                // could read rather than inventing entries.
                break;
            }
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let entry = read_bytes_vec(&mut cur, psz)?;
        // §8.5.2.2 preamble: 6 reserved bytes then a 16-bit
        // data_reference_index. Entries shorter than 8 bytes are
        // malformed; record a zero index rather than failing the whole
        // box (the FourCC alone is still useful metadata).
        let data_reference_index = if entry.len() >= 8 {
            u16::from_be_bytes([entry[6], entry[7]])
        } else {
            0
        };
        t.stsd_entries.push(StsdEntry {
            format: hdr.fourcc,
            data_reference_index,
        });
        if i == 0 {
            first_entry = Some((hdr, entry));
        }
    }
    let (hdr, entry) = match first_entry {
        Some(pair) => pair,
        None => return Err(Error::invalid("MP4: stsd first entry missing")),
    };
    t.codec_id_fourcc = hdr.fourcc;
    // ISO/IEC 14496-12 §8.12 — protected sample entries (`encv`, `enca`,
    // `enct`, `encs`) wrap the original sample description: the original
    // FourCC and the encryption parameters move into a child `sinf` box
    // while the outer FourCC becomes one of the `enc*` placeholders. We
    // peek into the sinf to recover the original FourCC + scheme type
    // so downstream codec dispatch sees the un-transformed type (and
    // callers can read `protection_scheme` on the stream to learn how
    // to decrypt). The bytes inside the sample entry stay laid out as
    // if the original FourCC were present — preamble (28 for audio /
    // 78 for video) then codec config boxes — which is exactly what
    // §8.12.0 mandates ("leaving all other boxes unmodified").
    if matches!(hdr.fourcc, ENCV | ENCA | ENCT | ENCS) {
        let unwrap = parse_sinf_for_original_format(&entry, t.media_type)?;
        if let Some(fourcc) = unwrap.original_format {
            t.codec_id_fourcc = fourcc;
        }
        t.protection_scheme = unwrap.scheme_type;
        t.tenc = unwrap.tenc;
    }
    parse_sample_entry(&entry, t)?;
    Ok(())
}

/// Result of unwrapping an `enc*` sample entry. Each field is
/// independently `None` when the corresponding child box was absent
/// from the sinf:
///
/// * `original_format` — un-transformed sample-entry FourCC from
///   `sinf/frma` (§8.12.2).
/// * `scheme_type` — protection scheme FourCC from `sinf/schm`
///   (§8.12.5). Per spec a `sinf` may carry `frma` without `schm` in
///   IPMP-only signalling.
/// * `tenc` — parsed CENC TrackEncryptionBox payload from
///   `sinf/schi/tenc` (ISO/IEC 23001-7 §8.2). Absent when the file
///   uses a non-CENC scheme or omits the (CENC-mandatory) tenc.
struct SinfUnwrap {
    original_format: Option<[u8; 4]>,
    scheme_type: Option<[u8; 4]>,
    tenc: Option<TencBox>,
}

impl SinfUnwrap {
    fn empty() -> Self {
        SinfUnwrap {
            original_format: None,
            scheme_type: None,
            tenc: None,
        }
    }
}

/// Walk a protected sample entry (`enc*`) to find its `sinf` child
/// container, then pull out the original (un-transformed) FourCC from
/// `frma` and the scheme type from `schm`. Returns
/// `(Some(original_fourcc), Some(scheme_type))` when both are present
/// in well-formed input; either field is `None` when the corresponding
/// child box is absent. The latter is permitted by §8.12: in IPMP-only
/// signalling a `sinf` may carry `frma` without `schm`.
///
/// The preamble length depends on the original media-type (recorded on
/// the track via the `hdlr`): 28 bytes for audio (AudioSampleEntry v0)
/// or 78 bytes for video (VisualSampleEntry). We skip that, then walk
/// child boxes until we hit `sinf`.
fn parse_sinf_for_original_format(entry: &[u8], media_type: MediaType) -> Result<SinfUnwrap> {
    let preamble = match media_type {
        MediaType::Audio => 28,
        MediaType::Video => 78,
        // Other media types we don't currently surface as `enc*`; bail
        // gracefully without claiming anything.
        _ => return Ok(SinfUnwrap::empty()),
    };
    if entry.len() <= preamble {
        return Ok(SinfUnwrap::empty());
    }
    let mut cur = std::io::Cursor::new(&entry[preamble..]);
    let end = (entry.len() - preamble) as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let body = read_bytes_vec(&mut cur, psz)?;
        if hdr.fourcc == SINF {
            return parse_sinf_body(&body);
        }
    }
    Ok(SinfUnwrap::empty())
}

/// Inner sinf walker — at the sinf level we're looking for `frma`
/// (original_format, §8.12.2), `schm` (scheme_type, §8.12.5), and
/// `schi` (SchemeInformationBox, §8.12.6). Per ISO/IEC 23001-7 §4.1
/// the `schi` of a CENC-protected track contains a `tenc`
/// TrackEncryptionBox; we descend one level into `schi` to pick that
/// up alongside the §8.12 fields.
fn parse_sinf_body(body: &[u8]) -> Result<SinfUnwrap> {
    let mut out = SinfUnwrap::empty();
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let inner = read_bytes_vec(&mut cur, psz)?;
        match hdr.fourcc {
            FRMA if inner.len() >= 4 => {
                out.original_format = Some([inner[0], inner[1], inner[2], inner[3]]);
            }
            SCHM if inner.len() >= 8 => {
                // FullBox header (4 bytes) + scheme_type (4 bytes) +
                // scheme_version (4 bytes) + optional URI. We only need
                // scheme_type at offset 4.
                out.scheme_type = Some([inner[4], inner[5], inner[6], inner[7]]);
            }
            SCHI => {
                // ISO/IEC 23001-7 §4.1 — descend into schi to find
                // `tenc`. A malformed tenc inside an otherwise valid
                // schi should not abort sinf parsing (it still has
                // structural meaning via frma + schm), so we swallow
                // a TencBox parse error and leave `out.tenc = None`.
                out.tenc = walk_schi_for_tenc(&inner);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Walk a `schi` (SchemeInformationBox, ISO/IEC 14496-12 §8.12.6)
/// body looking for a `tenc` (TrackEncryptionBox, ISO/IEC 23001-7
/// §8.2) child. Returns the parsed payload, or `None` if no tenc was
/// present (or it was malformed — schi can legitimately carry other
/// scheme-specific children).
fn walk_schi_for_tenc(body: &[u8]) -> Option<TencBox> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = read_box_header(&mut cur).ok().flatten()?;
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let inner = read_bytes_vec(&mut cur, psz).ok()?;
        if hdr.fourcc == TENC {
            return parse_tenc(&inner).ok();
        }
    }
    None
}

fn parse_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    if entry.len() < 8 {
        return Ok(());
    }
    match t.media_type {
        MediaType::Audio => parse_audio_sample_entry(entry, t),
        MediaType::Video => parse_video_sample_entry(entry, t),
        MediaType::Subtitle => parse_subtitle_sample_entry(entry, t),
        _ => Ok(()),
    }
}

/// Parse a subtitle / timed-text sample entry. Layout per
/// ISO/IEC 14496-12 §12.5–6 and 3GPP TS 26.245 (for `tx3g`).
///
/// Common to every subtitle sample entry: 8 reserved bytes (6 + 2
/// data_reference_index). After that, the per-codec body varies:
///
/// * `wvtt` (WebVTT): the spec defines an inner `vttC` box (config) and
///   an optional `vlab` (label). We capture `vttC` as extradata.
/// * `stpp` (XML subtitle / TTML, BMFF §12.6.3.2): null-terminated
///   UTF-8 strings — namespace, schema_location (optional),
///   auxiliary_mime_types (optional) — then optional `btrt`. We store
///   `namespace\0schema_location\0aux_mime_types` as extradata so
///   callers can recover them.
/// * `sbtt`/`stxt` (BMFF §12.5–6): content_encoding\0mime_format\0
///   plus optional `btrt`/`txtC`. Surface the strings as extradata.
/// * `tx3g` (3GPP TS 26.245): 18 bytes of display flags + colour +
///   default text box + style record, then optional `ftab` etc. The
///   3GPP TS 26.245 spec is not in `docs/`; we don't parse the body
///   today, but the FourCC dispatch / codec id (`mov_text`) is enough
///   for round-trip carriage through the demuxer.
/// * `text` (QuickTime plain text): same situation — surfaced as a
///   subtitle stream without decoding.
/// * `c608` / `c708`: CEA-608/708 closed captions. Sample bytes carry
///   the raw caption packets; no extradata to surface.
fn parse_subtitle_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    // 8-byte preamble (6 reserved + 2 data_reference_index).
    if entry.len() < 8 {
        return Ok(());
    }
    // For BMFF text subtitle entries (`stpp`, `sbtt`, `stxt`) the
    // body after the preamble is a series of null-terminated UTF-8
    // strings. Capture them verbatim — the demuxer doesn't interpret
    // their semantics, but consumers (e.g. a TTML renderer) do.
    match &t.codec_id_fourcc {
        b"stpp" | b"sbtt" | b"stxt" => {
            // Strings + optional child boxes. We need to skip the
            // strings before walking child boxes. Find the last null
            // terminator that ends the string-only region: stpp has up
            // to three strings (namespace, schema_location?,
            // auxiliary_mime_types?), sbtt/stxt have up to two
            // (content_encoding?, mime_format). All optional fields
            // are present as bare null-bytes when empty, so we walk to
            // the first byte that *could* be a box header (size+type).
            //
            // The simplest correct behaviour: copy the entire post-
            // preamble payload as extradata, including any trailing
            // `btrt`/`txtC` sub-boxes. Callers that need to extract
            // the strings can parse them with `splitn('\0')`.
            t.extradata = entry[8..].to_vec();
        }
        b"wvtt" => {
            // WebVTT in MP4: walk for the `vttC` config box (and
            // optional `vlab` label). The spec lives in ISO/IEC
            // 14496-30; we treat the entire post-preamble payload as
            // extradata so consumers can find the embedded `vttC`
            // header without re-parsing the BMFF surface.
            t.extradata = entry[8..].to_vec();
        }
        b"tx3g" | b"text" | b"c608" | b"c708" => {
            // `tx3g` carries an 18-byte fixed header (display flags +
            // colours + default text box + default style record) plus
            // optional `ftab` font table. Treat the entire post-
            // preamble payload as extradata so a downstream renderer
            // can pick the colour / style defaults out of it.
            //
            // For `text` / `c608` / `c708` no useful per-track header
            // is defined; the post-preamble bytes are still preserved
            // as extradata for any nonstandard carriage.
            t.extradata = entry[8..].to_vec();
        }
        _ => {}
    }
    Ok(())
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

/// Parse `stsh` (ShadowSyncSampleBox, ISO/IEC 14496-12 §8.6.3).
///
/// `FullBox(version = 0, flags = 0)` whose body is a `u32` `entry_count`
/// followed by that many `(shadowed_sample_number, sync_sample_number)`
/// pairs, each a big-endian `u32`. The first member names a (normally
/// non-sync) sample; the second names a sync sample that can be decoded
/// in its place when seeking to or before the shadowed sample. The
/// table is ordered by `shadowed_sample_number` per §8.6.3.1, but we
/// keep on-wire order so the bytes round-trip without assuming the
/// producer honoured the ordering recommendation.
///
/// `with_capacity` is bounded by the byte budget (`(len - 8) / 8`) so an
/// adversarial `entry_count` can't trigger a giant up-front allocation;
/// the per-entry bounds check rejects a body that is shorter than the
/// claimed count.
fn parse_stsh(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: stsh too short"));
    }
    let count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let max_entries = (body.len() - 8) / 8;
    let mut out = Vec::with_capacity(count.min(max_entries));
    let mut off = 8;
    for _ in 0..count {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: stsh truncated"));
        }
        let shadowed = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let sync = u32::from_be_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
        out.push((shadowed, sync));
        off += 8;
    }
    Ok(out)
}

/// Parse `sdtp` (SampleDependencyTypeBox, ISO/IEC 14496-12 §8.6.4).
///
/// `FullBox(version = 0, flags = 0)` whose body, after the 4-byte
/// FullBox preamble, is one byte per sample. The `sample_count` is
/// taken from `stsz` / `stz2` and is *not* re-stated in the box, so
/// the table's length is simply `body.len() - 4`. Each byte is four
/// 2-bit fields packed MSB-first per §8.6.4.2:
///
/// ```text
///     unsigned int(2) is_leading;
///     unsigned int(2) sample_depends_on;
///     unsigned int(2) sample_is_depended_on;
///     unsigned int(2) sample_has_redundancy;
/// ```
///
/// Each 2-bit value's permitted set is in §8.6.4.3 (e.g.
/// `sample_depends_on = 2` → "I-picture"; `sample_is_depended_on = 2`
/// → "disposable"). The container preserves the raw small ints; a
/// trick-mode renderer or seek heuristic interprets them.
///
/// If the producer wrote a table longer than the track's
/// `sample_count`, the trailing bytes are still parsed — callers
/// that care about alignment between the table and the sample list
/// cross-check the lengths themselves.
fn parse_sdtp(body: &[u8]) -> Result<Vec<SdtpEntry>> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: sdtp too short"));
    }
    // §8.6.4.2 fixes version = 0 and the FullBox preamble carries no
    // useful flags; tolerate both rather than rejecting a wrong-version
    // table, since the byte format is unambiguous.
    let payload = &body[4..];
    let mut out = Vec::with_capacity(payload.len());
    for &byte in payload {
        out.push(SdtpEntry {
            is_leading: (byte >> 6) & 0x03,
            sample_depends_on: (byte >> 4) & 0x03,
            sample_is_depended_on: (byte >> 2) & 0x03,
            sample_has_redundancy: byte & 0x03,
        });
    }
    Ok(out)
}

/// Parse `stdp` (DegradationPriorityBox, ISO/IEC 14496-12 §8.5.3).
///
/// `FullBox(version = 0, flags = 0)` whose body, after the 4-byte
/// FullBox preamble, is a packed array of big-endian `unsigned int(16)
/// priority` entries — one per sample. The `sample_count` is taken
/// from `stsz` / `stz2` and is *not* re-stated in the box (§8.5.3.1),
/// so the table's length is simply `(body.len() - 4) / 2`. A trailing
/// odd byte (the spec never produces one — `priority` is always
/// 16-bit) is silently ignored rather than rejecting the whole box.
///
/// The exact meaning and acceptable range of `priority` are defined
/// by derived specifications (§8.5.3.3 "Specifications derived from
/// this define the exact meaning and acceptable range of the
/// `priority` field"); the container preserves the raw u16 values
/// without interpreting them. A renderer that needs to drop samples
/// under bitrate / CPU pressure consults the carrying spec for the
/// priority ordering.
fn parse_stdp(body: &[u8]) -> Result<Vec<u16>> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: stdp too short"));
    }
    // §8.5.3.2 pins version = 0 and there are no defined flags; the
    // FullBox preamble carries no information beyond that, so tolerate
    // whatever the producer wrote — the per-sample u16 layout is
    // unambiguous.
    let payload = &body[4..];
    let count = payload.len() / 2;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * 2;
        out.push(u16::from_be_bytes([payload[off], payload[off + 1]]));
    }
    Ok(out)
}

/// §8.7.6 — `padb` (PaddingBitsBox) typed accessor.
///
/// `FullBox(version = 0, flags = 0)` whose body is:
///
/// ```text
///     unsigned int(32) sample_count
///     for i in 0 .. (sample_count + 1) / 2 {
///         bit(1) reserved = 0; bit(3) pad1;   // top nibble
///         bit(1) reserved = 0; bit(3) pad2;   // bottom nibble
///     }
/// ```
///
/// Each nibble's reserved bit is required to be 0 per §8.7.6.2 but is
/// not enforced here — the container layer surfaces what the producer
/// wrote rather than rejecting on a reserved-bit slip; a strict
/// validator can re-check by walking the raw box bytes.
///
/// `pad1` is the padding-bit count for sample number `(i * 2) + 1`,
/// `pad2` for sample `(i * 2) + 2` (§8.7.6.3 — sample numbering is
/// 1-based on the wire; the returned vec is 0-indexed so entry `n` is
/// the padding-bit count for the `n+1`-th sample). When `sample_count`
/// is odd the trailing `pad2` nibble is unused and dropped: the
/// returned vec is exactly `sample_count` long.
///
/// `sample_count == 0` is permitted (a zero-sample padb yields an empty
/// vec). A body too short for the declared `sample_count` is rejected
/// rather than silently truncated — the spec mandates the box carry
/// `(sample_count + 1) / 2` bytes of data.
///
/// Unlike `sdtp` / `stdp` (whose sample count is implicit from
/// `stsz` / `stz2`), `padb` re-declares its own `sample_count` on the
/// wire and the table is self-contained; if a producer wrote a
/// `sample_count` larger than the track's actual sample count the
/// trailing entries are still returned — a length mismatch is the
/// producer's bug, not ours to invent zeros for.
fn parse_padb(body: &[u8]) -> Result<Vec<u8>> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: padb too short"));
    }
    // §8.7.6.2 pins version = 0 and there are no defined flags; the
    // FullBox preamble carries no useful information beyond that.
    let sample_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let need = sample_count.div_ceil(2);
    if body.len() < 8 + need {
        return Err(Error::invalid(
            "MP4: padb body shorter than declared sample_count",
        ));
    }
    let payload = &body[8..8 + need];
    let mut out = Vec::with_capacity(sample_count);
    for (i, &byte) in payload.iter().enumerate() {
        // Top nibble = pad1 (sample (i*2)+1); bottom nibble = pad2
        // (sample (i*2)+2). The low 3 bits of each nibble carry the
        // padding-bit count; the high bit is a reserved zero per
        // §8.7.6.2 and is masked off here so a producer slip on the
        // reserved bit does not corrupt the count.
        let pad1 = (byte >> 4) & 0x07;
        out.push(pad1);
        if out.len() == sample_count {
            break;
        }
        let pad2 = byte & 0x07;
        out.push(pad2);
        if out.len() == sample_count {
            break;
        }
        // Suppress the unused-binding warning when no break fires (i
        // is the byte index but we don't need it here — kept for
        // future per-byte diagnostics).
        let _ = i;
    }
    Ok(out)
}

/// Parse `sbgp` (SampleToGroupBox, ISO/IEC 14496-12 §8.9.2).
///
/// `FullBox(version, 0)`:
///
/// ```text
///     unsigned int(32) grouping_type
///     if (version == 1) unsigned int(32) grouping_type_parameter
///     unsigned int(32) entry_count
///     entry_count × {
///         unsigned int(32) sample_count
///         unsigned int(32) group_description_index
///     }
/// ```
///
/// The table is a run-length map: each entry covers `sample_count`
/// consecutive decode-order samples that all share
/// `group_description_index`. An index of 0 means "member of no group of
/// this type" (§8.9.2.3); indices ≥ 0x10001 are movie-fragment-local
/// references (§8.9.4) and are preserved verbatim — the demuxer does not
/// resolve them against a fragment's own `sgpd`.
///
/// `with_capacity` is bounded by the byte budget so an adversarial
/// `entry_count` cannot trigger a giant up-front allocation; the
/// per-entry bounds check rejects a truncated body.
fn parse_sbgp(body: &[u8]) -> Result<SbgpBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: sbgp too short"));
    }
    let version = body[0];
    let mut off = 4;
    let read_u32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let grouping_type_raw =
        read_u32(body, off).ok_or_else(|| Error::invalid("MP4: sbgp grouping_type truncated"))?;
    let grouping_type = grouping_type_raw.to_be_bytes();
    off += 4;
    let grouping_type_parameter = if version == 1 {
        let p = read_u32(body, off)
            .ok_or_else(|| Error::invalid("MP4: sbgp grouping_type_parameter truncated"))?;
        off += 4;
        Some(p)
    } else {
        None
    };
    let count = read_u32(body, off)
        .ok_or_else(|| Error::invalid("MP4: sbgp entry_count truncated"))? as usize;
    off += 4;
    let max_entries = body.len().saturating_sub(off) / 8;
    let mut entries = Vec::with_capacity(count.min(max_entries));
    for _ in 0..count {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: sbgp entries truncated"));
        }
        let sample_count =
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let gdi = u32::from_be_bytes([body[off + 4], body[off + 5], body[off + 6], body[off + 7]]);
        entries.push((sample_count, gdi));
        off += 8;
    }
    Ok(SbgpBox {
        grouping_type,
        grouping_type_parameter,
        entries,
    })
}

/// Parse `sgpd` (SampleGroupDescriptionBox, ISO/IEC 14496-12 §8.9.3).
///
/// `FullBox(version, 0)`:
///
/// ```text
///     unsigned int(32) grouping_type
///     if (version == 1) unsigned int(32) default_length
///     if (version >= 2) unsigned int(32) default_sample_description_index
///     unsigned int(32) entry_count
///     entry_count × {
///         if (version == 1 && default_length == 0)
///             unsigned int(32) description_length
///         SampleGroupEntry(grouping_type)   // grouping-type-specific blob
///     }
/// ```
///
/// The per-entry `SampleGroupEntry` payload is grouping-type-specific and
/// opaque to the container — we capture its raw bytes verbatim and leave
/// interpretation to the layer that knows the `grouping_type` semantics.
///
/// Entry sizing follows §8.9.3.2:
/// * version 1 with `default_length > 0`: every entry is `default_length`
///   bytes.
/// * version 1 with `default_length == 0`: each entry is preceded by a
///   `u32` `description_length`.
/// * version 0 / version ≥ 2 with no per-entry length: the spec notes
///   version-0 entries carry no signalled size (§8.9.3.3 NOTE — their use
///   is deprecated precisely because they cannot be scanned). When the
///   box gives us no length to chunk by, we fall back to treating the
///   remaining body as a single combined entry blob rather than guessing
///   a fixed entry size we cannot know — callers that recognise the
///   `grouping_type` can re-split it.
fn parse_sgpd(body: &[u8]) -> Result<SgpdBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: sgpd too short"));
    }
    let version = body[0];
    let mut off = 4;
    let read_u32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let grouping_type_raw =
        read_u32(body, off).ok_or_else(|| Error::invalid("MP4: sgpd grouping_type truncated"))?;
    let grouping_type = grouping_type_raw.to_be_bytes();
    off += 4;
    let default_length = if version == 1 {
        let dl = read_u32(body, off)
            .ok_or_else(|| Error::invalid("MP4: sgpd default_length truncated"))?;
        off += 4;
        dl
    } else {
        0
    };
    let default_sample_description_index = if version >= 2 {
        let d = read_u32(body, off).ok_or_else(|| {
            Error::invalid("MP4: sgpd default_sample_description_index truncated")
        })?;
        off += 4;
        Some(d)
    } else {
        None
    };
    let count = read_u32(body, off)
        .ok_or_else(|| Error::invalid("MP4: sgpd entry_count truncated"))? as usize;
    off += 4;

    let mut entries: Vec<Vec<u8>> = Vec::new();
    if version == 1 && default_length == 0 {
        // Each entry prefixed with its own u32 description_length.
        for _ in 0..count {
            let len = read_u32(body, off)
                .ok_or_else(|| Error::invalid("MP4: sgpd description_length truncated"))?
                as usize;
            off += 4;
            if off + len > body.len() {
                return Err(Error::invalid("MP4: sgpd variable entry truncated"));
            }
            entries.push(body[off..off + len].to_vec());
            off += len;
        }
    } else if default_length > 0 {
        // Fixed-size entries (version 1 with a non-zero default, or any
        // version that supplied a default_length). Every entry is
        // `default_length` bytes.
        let len = default_length as usize;
        for _ in 0..count {
            if off + len > body.len() {
                return Err(Error::invalid("MP4: sgpd fixed entry truncated"));
            }
            entries.push(body[off..off + len].to_vec());
            off += len;
        }
    } else {
        // No per-entry length available (version 0, or version ≥ 2 with no
        // default_length field): we cannot determine per-entry boundaries
        // from the container alone. Capture the remaining body as one
        // combined blob so no bytes are lost; a grouping-type-aware caller
        // can re-split it. Empty when entry_count is 0.
        if count > 0 && off < body.len() {
            entries.push(body[off..].to_vec());
        }
    }

    Ok(SgpdBox {
        grouping_type,
        default_sample_description_index,
        entries,
    })
}

/// MSB-first big-endian bit cursor over a byte slice, used for the
/// bit-packed variable-width fields of `csgp` (§8.9.5). Reads up to 32
/// bits per call; returns `None` when the request would run past the end
/// of the slice (the caller maps that to a "truncated" parse error).
struct BitCursor<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    bit_pos: usize,
}

impl<'a> BitCursor<'a> {
    /// Anchor the cursor at byte offset `byte_off` (bit `byte_off * 8`).
    fn new(data: &'a [u8], byte_off: usize) -> Self {
        BitCursor {
            data,
            bit_pos: byte_off * 8,
        }
    }

    /// Read `n` bits (0..=32) MSB-first as a big-endian unsigned value.
    /// `n == 0` yields `0` without consuming any bits. Returns `None`
    /// when fewer than `n` bits remain.
    fn read(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Some(0);
        }
        let total_bits = self.data.len() * 8;
        if self.bit_pos + n as usize > total_bits {
            return None;
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte = self.data[self.bit_pos >> 3];
            let bit = (byte >> (7 - (self.bit_pos & 7))) & 1;
            value = (value << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Some(value)
    }
}

/// Parse `csgp` (CompactSampleToGroupBox, ISO/IEC 14496-12:2020 §8.9.5).
///
/// The compact alternative to `sbgp`. `FullBox(version=0, flags)` where
/// the 24-bit `flags` is overloaded to carry four sub-fields (LSB-first
/// bit numbering):
///
/// ```text
///     index_size_code                = flags[0..1]   (2 bits)
///     count_size_code                = flags[2..3]   (2 bits)
///     pattern_size_code              = flags[4..5]   (2 bits)
///     grouping_type_parameter_present = flags[6]     (1 bit)
/// ```
///
/// Each 2-bit size code selects a field width via `width = 4 << code`
/// (code 0→4, 1→8, 2→16, 3→32 bits). The body is then:
///
/// ```text
///     unsigned int(32) grouping_type
///     if (grouping_type_parameter_present)
///         unsigned int(32) grouping_type_parameter
///     unsigned int(32) pattern_count
///     for i in 1..=pattern_count {
///         unsigned int(f(pattern_size_code)) pattern_length[i]
///         unsigned int(f(count_size_code))   sample_count[i]
///     }
///     for j in 1..=pattern_count {
///         for k in 1..=pattern_length[j] {
///             unsigned int(f(index_size_code))
///                 sample_group_description_index[j][k]
///         }
///     }
/// ```
///
/// The `pattern_length`/`sample_count` array and the index array are two
/// separate runs (all lengths first, then all indices) — the index run's
/// total width is `sum(pattern_length[j]) * f(index_size_code)` bits. The
/// 4- and 8-bit field widths mean fields are bit-packed (not byte
/// aligned) across the array; we read MSB-first from a running bit
/// cursor, matching the `unsigned int(N)` big-endian bit-field
/// convention used throughout 14496-12.
///
/// `sample_group_description_index` values are kept verbatim: 0 = "no
/// group of this type"; in a `traf` the field's most-significant bit
/// distinguishes a fragment-local description (set) from a global one
/// (clear). The demuxer does not resolve fragment-local references.
fn parse_csgp(body: &[u8]) -> Result<CsgpBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: csgp too short"));
    }
    // FullBox: version(8) then 24-bit flags carrying the size codes.
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let index_size_code = (flags & 0x3) as u8;
    let count_size_code = ((flags >> 2) & 0x3) as u8;
    let pattern_size_code = ((flags >> 4) & 0x3) as u8;
    let gtpp = (flags >> 6) & 0x1 == 1;
    let width = |code: u8| -> u32 { 4u32 << code };
    let index_w = width(index_size_code);
    let count_w = width(count_size_code);
    let pattern_w = width(pattern_size_code);

    let read_u32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    let mut off = 4usize;
    let grouping_type_raw =
        read_u32(body, off).ok_or_else(|| Error::invalid("MP4: csgp grouping_type truncated"))?;
    let grouping_type = grouping_type_raw.to_be_bytes();
    off += 4;
    let grouping_type_parameter = if gtpp {
        let p = read_u32(body, off)
            .ok_or_else(|| Error::invalid("MP4: csgp grouping_type_parameter truncated"))?;
        off += 4;
        Some(p)
    } else {
        None
    };
    let pattern_count = read_u32(body, off)
        .ok_or_else(|| Error::invalid("MP4: csgp pattern_count truncated"))?
        as usize;
    off += 4;

    // From here on, fields are bit-packed (4-/8-/16-/32-bit widths) with
    // no byte alignment between them. Read MSB-first from a bit cursor
    // anchored at the current byte offset.
    let mut bits = BitCursor::new(body, off);
    // Bound the up-front pattern allocation by the remaining bit budget:
    // every pattern needs at least `pattern_w + count_w` bits here. Both
    // widths are `4 << code`, hence ≥ 4, so the divisor is always ≥ 8.
    let remaining_bits = body.len().saturating_sub(off).saturating_mul(8);
    let min_pattern_bits = (pattern_w + count_w) as usize;
    let cap = pattern_count.min(remaining_bits / min_pattern_bits);
    let mut lengths: Vec<(u32, u32)> = Vec::with_capacity(cap);
    for _ in 0..pattern_count {
        let pattern_length = bits
            .read(pattern_w)
            .ok_or_else(|| Error::invalid("MP4: csgp pattern_length truncated"))?;
        let sample_count = bits
            .read(count_w)
            .ok_or_else(|| Error::invalid("MP4: csgp sample_count truncated"))?;
        lengths.push((pattern_length, sample_count));
    }

    let mut patterns: Vec<CsgpPattern> = Vec::with_capacity(lengths.len());
    for (pattern_length, sample_count) in lengths {
        let mut indices = Vec::with_capacity(pattern_length.min(remaining_bits as u32) as usize);
        for _ in 0..pattern_length {
            let idx = bits
                .read(index_w)
                .ok_or_else(|| Error::invalid("MP4: csgp index truncated"))?;
            indices.push(idx);
        }
        patterns.push(CsgpPattern {
            sample_count,
            indices,
        });
    }

    Ok(CsgpBox {
        grouping_type,
        grouping_type_parameter,
        patterns,
    })
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

/// Parse `subs` (SubSampleInformationBox, ISO/IEC 14496-12 §8.7.7).
///
/// `FullBox(version, flags)`:
///
/// ```text
///     unsigned int(32) entry_count
///     entry_count × {
///         unsigned int(32) sample_delta
///         unsigned int(16) subsample_count
///         subsample_count × {
///             if (version == 1) unsigned int(32) subsample_size
///             else              unsigned int(16) subsample_size
///             unsigned int(8)  subsample_priority
///             unsigned int(8)  discardable
///             unsigned int(32) codec_specific_parameters
///         }
///     }
/// ```
///
/// Per §8.7.7.1 the table is *sparse*: each entry's `sample_delta` is
/// the difference between this entry's target sample and the previous
/// entry's (or sample 0 for the first entry), so a `subs` documents only
/// the samples that genuinely have sub-sample structure. A
/// `subsample_count` of 0 is legal: the entry still consumes one row
/// (advances the delta cursor) but produces no per-sub-sample fields.
///
/// The codec-specific semantics of `subsample_priority`, `discardable`,
/// `codec_specific_parameters`, and `flags` are deliberately preserved
/// verbatim — the container does not interpret them. For H.264
/// (ISO/IEC 14496-15) `codec_specific_parameters` encodes per-NAL-unit
/// dependency information; we surface the raw u32 so a codec-aware
/// layer can decode it.
///
/// `with_capacity` is bounded by the byte budget so an adversarial
/// `entry_count` cannot force a giant up-front allocation; per-entry
/// bounds checks reject a truncated body.
fn parse_subs(body: &[u8]) -> Result<SubsBox> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: subs too short"));
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    // Minimum bytes per row, used as a guard on `with_capacity`:
    //   sample_delta (4) + subsample_count (2) = 6 even for an
    //   empty-subsample entry.
    let max_entries = body.len().saturating_sub(8) / 6;
    let mut entries: Vec<SubsEntry> = Vec::with_capacity(entry_count.min(max_entries));
    let size_width = if version == 1 { 4 } else { 2 };
    let per_sub_min = size_width + 1 + 1 + 4; // size + priority + discardable + csp
    let mut off = 8;
    for _ in 0..entry_count {
        if off + 6 > body.len() {
            return Err(Error::invalid("MP4: subs entry header truncated"));
        }
        let sample_delta =
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        let subsample_count = u16::from_be_bytes([body[off], body[off + 1]]) as usize;
        off += 2;
        // Cap per-entry capacity by remaining byte budget so an inflated
        // `subsample_count` can't allocate ahead of the available bytes.
        let max_subs = (body.len().saturating_sub(off)) / per_sub_min;
        let mut subs_vec: Vec<SubSampleEntry> = Vec::with_capacity(subsample_count.min(max_subs));
        for _ in 0..subsample_count {
            if off + per_sub_min > body.len() {
                return Err(Error::invalid("MP4: subs subsample truncated"));
            }
            let subsample_size = if version == 1 {
                let v =
                    u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
                off += 4;
                v
            } else {
                let v = u16::from_be_bytes([body[off], body[off + 1]]) as u32;
                off += 2;
                v
            };
            let subsample_priority = body[off];
            off += 1;
            let discardable = body[off];
            off += 1;
            let codec_specific_parameters =
                u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
            off += 4;
            subs_vec.push(SubSampleEntry {
                subsample_size,
                subsample_priority,
                discardable,
                codec_specific_parameters,
            });
        }
        entries.push(SubsEntry {
            sample_delta,
            subsamples: subs_vec,
        });
    }
    Ok(SubsBox {
        version,
        flags,
        entries,
    })
}

/// Parse `saiz` (SampleAuxiliaryInformationSizesBox, ISO/IEC 14496-12
/// §8.7.8).
///
/// On-wire layout (§8.7.8.2):
///
/// ```text
/// FullBox header  : 4 bytes  (version + flags)
/// if (flags & 1) {
///     aux_info_type           : u32   (FourCC)
///     aux_info_type_parameter : u32
/// }
/// default_sample_info_size : u8
/// sample_count             : u32
/// if (default_sample_info_size == 0) {
///     sample_info_size[sample_count] : u8 each
/// }
/// ```
///
/// The `aux_info_type` block is gated by the FullBox `flags`'s low bit;
/// when absent the implied value is the protection scheme type (for
/// transformed sample entries) or the sample-entry FourCC (§8.7.8.3).
/// We surface `Option`s rather than materialise the fallback so the
/// caller's higher-level context (sinf/sample-entry FourCC) decides.
fn parse_saiz(body: &[u8]) -> Result<SaizBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: saiz too short"));
    }
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let mut off = 4usize;
    let mut aux_info_type: Option<[u8; 4]> = None;
    let mut aux_info_type_parameter: Option<u32> = None;
    if flags & 1 != 0 {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: saiz aux_info_type truncated"));
        }
        aux_info_type = Some([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        aux_info_type_parameter = Some(u32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }
    if off + 5 > body.len() {
        return Err(Error::invalid("MP4: saiz header truncated"));
    }
    let default_sample_info_size = body[off];
    off += 1;
    let sample_count = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    off += 4;
    let mut per_sample: Vec<u8> = Vec::new();
    if default_sample_info_size == 0 {
        let want = sample_count as usize;
        // Cap allocation by the remaining byte budget so a forged
        // `sample_count` cannot pre-allocate ahead of the truncated body.
        let avail = body.len().saturating_sub(off);
        per_sample = Vec::with_capacity(want.min(avail));
        if off + want > body.len() {
            return Err(Error::invalid("MP4: saiz sample_info_size truncated"));
        }
        per_sample.extend_from_slice(&body[off..off + want]);
    }
    Ok(SaizBox {
        aux_info_type,
        aux_info_type_parameter,
        default_sample_info_size,
        sample_count,
        per_sample,
    })
}

/// Parse `saio` (SampleAuxiliaryInformationOffsetsBox, ISO/IEC 14496-12
/// §8.7.9).
///
/// On-wire layout (§8.7.9.2):
///
/// ```text
/// FullBox header  : 4 bytes  (version + flags)
/// if (flags & 1) {
///     aux_info_type           : u32   (FourCC)
///     aux_info_type_parameter : u32
/// }
/// entry_count : u32
/// if (version == 0) { offset[entry_count] : u32 each }
/// else             { offset[entry_count] : u64 each }
/// ```
///
/// All offsets are widened to `u64` so callers handle one shape.
/// `version` is preserved so a producer can re-emit the original width.
fn parse_saio(body: &[u8]) -> Result<SaioBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: saio too short"));
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let mut off = 4usize;
    let mut aux_info_type: Option<[u8; 4]> = None;
    let mut aux_info_type_parameter: Option<u32> = None;
    if flags & 1 != 0 {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: saio aux_info_type truncated"));
        }
        aux_info_type = Some([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        aux_info_type_parameter = Some(u32::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
        ]));
        off += 4;
    }
    if off + 4 > body.len() {
        return Err(Error::invalid("MP4: saio entry_count truncated"));
    }
    let entry_count =
        u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as usize;
    off += 4;
    let off_width = if version == 1 { 8 } else { 4 };
    // Cap capacity by remaining byte budget so a forged `entry_count`
    // cannot pre-allocate ahead of the truncated body.
    let avail = body.len().saturating_sub(off);
    let max_entries = avail / off_width;
    let mut offsets: Vec<u64> = Vec::with_capacity(entry_count.min(max_entries));
    for _ in 0..entry_count {
        if off + off_width > body.len() {
            return Err(Error::invalid("MP4: saio offset truncated"));
        }
        let v = if version == 1 {
            u64::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
                body[off + 4],
                body[off + 5],
                body[off + 6],
                body[off + 7],
            ])
        } else {
            u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64
        };
        offsets.push(v);
        off += off_width;
    }
    Ok(SaioBox {
        version,
        aux_info_type,
        aux_info_type_parameter,
        offsets,
    })
}

/// Parse `cslg` (CompositionToDecodeBox, ISO/IEC 14496-12 §8.6.1.4).
///
/// `FullBox(version, 0)` whose body is five signed integers. Version 0
/// stores them as 32-bit, version 1 as 64-bit (used only when at least
/// one value exceeds the 32-bit range, per §8.6.1.4.1). The fields are,
/// in order: `compositionToDTSShift`, `leastDecodeToDisplayDelta`,
/// `greatestDecodeToDisplayDelta`, `compositionStartTime`,
/// `compositionEndTime`. All five are widened to `i64` so callers see
/// one shape regardless of the on-wire version.
fn parse_cslg(body: &[u8]) -> Result<CslgBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: cslg too short"));
    }
    let version = body[0];
    let mut off = 4;
    let mut take = |width: usize, b: &[u8]| -> Result<i64> {
        if off + width > b.len() {
            return Err(Error::invalid("MP4: cslg truncated"));
        }
        let v = if width == 4 {
            i32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as i64
        } else {
            i64::from_be_bytes([
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
                b[off + 4],
                b[off + 5],
                b[off + 6],
                b[off + 7],
            ])
        };
        off += width;
        Ok(v)
    };
    let w = if version == 0 { 4 } else { 8 };
    Ok(CslgBox {
        composition_to_dts_shift: take(w, body)?,
        least_decode_to_display_delta: take(w, body)?,
        greatest_decode_to_display_delta: take(w, body)?,
        composition_start_time: take(w, body)?,
        composition_end_time: take(w, body)?,
    })
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

/// One per-fragment CENC SampleEncryptionBox record. Collected during
/// the moof walk so a downstream layer can replay per-sample IVs and
/// subsample maps without re-parsing the file. `track_idx` is the
/// 0-based index into the demuxer's `streams()`; `senc` carries the
/// payload exactly as parsed.
#[derive(Clone, Debug)]
pub struct SencRecord {
    /// `track_idx` of the matching stream in the demuxer's
    /// `streams()` list (0-based).
    pub track_idx: u32,
    /// `mfhd.sequence_number` of the containing `moof`. Surfaced so a
    /// caller that interleaves multiple sources or replays out of
    /// order can re-key.
    pub moof_sequence: u32,
    /// Parsed CENC SampleEncryptionBox.
    pub senc: SencBox,
}

/// One per-fragment ISO/IEC 14496-12 §8.7.8 / §8.7.9 record. Each
/// `traf` may carry zero or more `(saiz, saio)` pairs (keyed by
/// `(aux_info_type, aux_info_type_parameter)`); the demuxer collects
/// them as `SaiRecord` so a downstream layer can replay per-sample
/// auxiliary-info pointers (most commonly CENC IV / subsample-map
/// material when `senc` is absent and the IV stream is carried via
/// auxiliary-info pointers per §8.8.14 + ISO/IEC 23001-7 §7.3) without
/// re-parsing the file. `aux_info_type` semantics determine pairing;
/// the demuxer does not interpret the bytes the offsets point at.
#[derive(Clone, Debug)]
pub struct SaiRecord {
    /// `track_idx` of the matching stream in the demuxer's
    /// `streams()` list (0-based).
    pub track_idx: u32,
    /// `mfhd.sequence_number` of the containing `moof`. Surfaced so a
    /// caller that interleaves multiple sources or replays out of
    /// order can re-key.
    pub moof_sequence: u32,
    /// Parsed `(saiz, saio)` pairs from this `traf`, in disk order.
    /// The spec permits multiple pairs per `traf` keyed by
    /// `(aux_info_type, aux_info_type_parameter)`; a caller pairs
    /// each `saiz` with the `saio` of the matching key. We do not
    /// pre-pair here so an unmatched box (a producer slip) is
    /// preserved verbatim for inspection.
    pub saiz: Vec<TrafSaiz>,
    pub saio: Vec<TrafSaio>,
}

/// One `saiz` (SampleAuxiliaryInformationSizesBox) found in a `traf`.
/// The on-wire layout matches the stbl-level box; the only semantic
/// difference is that the matching `saio.offset[]` is base_data_offset
/// relative (per §8.8.14) rather than absolute.
#[derive(Clone, Debug, Default)]
pub struct TrafSaiz {
    /// §8.7.8.3 — `aux_info_type` (present only when `flags & 1`).
    pub aux_info_type: Option<[u8; 4]>,
    /// §8.7.8.3 — `aux_info_type_parameter` (present only when
    /// `flags & 1`).
    pub aux_info_type_parameter: Option<u32>,
    /// §8.7.8.3 — `default_sample_info_size`.
    pub default_sample_info_size: u8,
    /// §8.7.8.3 — `sample_count`.
    pub sample_count: u32,
    /// §8.7.8.2 — `sample_info_size[]` (populated only when
    /// `default_sample_info_size == 0`).
    pub per_sample: Vec<u8>,
}

/// One `saio` (SampleAuxiliaryInformationOffsetsBox) found in a `traf`.
/// `version` selects 32-bit / 64-bit on-disk offset width. Offsets are
/// widened to `u64` and are *relative* to the `tfhd.base_data_offset`
/// established for this traf (or the `moof` start when
/// `default-base-is-moof` is set, per §8.8.14).
#[derive(Clone, Debug, Default)]
pub struct TrafSaio {
    /// FullBox version.
    pub version: u8,
    /// §8.7.9.3 — `aux_info_type`.
    pub aux_info_type: Option<[u8; 4]>,
    /// §8.7.9.3 — `aux_info_type_parameter`.
    pub aux_info_type_parameter: Option<u32>,
    /// §8.7.9.2 — `offset[]` widened to u64; base_data_offset relative
    /// inside a traf per §8.8.14.
    pub offsets: Vec<u64>,
}

/// One per-fragment ISO/IEC 23001-7 §8.1 `pssh`
/// ProtectionSystemSpecificHeaderBox record collected from inside a
/// `moof`. The spec (§8.1.1) instructs readers to examine pssh boxes
/// "in the Movie Box and in the Movie Fragment Box associated with
/// the sample (but not those in other Movie Fragment Boxes)", so a
/// decrypting layer keys each record by `moof_sequence` to scope
/// SystemID matches to the right fragment.
///
/// `pssh` carries the parsed PsshBox; `moof_sequence` is the
/// `mfhd.sequence_number` of the enclosing `moof`.
#[derive(Clone, Debug)]
pub struct MoofPsshRecord {
    /// `mfhd.sequence_number` of the enclosing `moof`. Surfaced so a
    /// decrypting layer can scope SystemID matches to the right
    /// fragment without re-walking the file.
    pub moof_sequence: u32,
    /// Parsed ProtectionSystemSpecificHeaderBox.
    pub pssh: PsshBox,
}

/// Walk one `moof` box, locating its `traf` children and stitching the
/// fragmented samples into the per-track sample list.
///
/// `next_dts` carries each track's running base_media_decode_time so a
/// `traf` without `tfdt` continues seamlessly from the previous
/// fragment (or from the moov-derived sample tail).
///
/// `senc_records` accumulates per-fragment ISO/IEC 23001-7 §7.2 senc
/// payloads keyed by `(track_idx, moof_sequence)` so a decrypting
/// layer can recover them without re-parsing the file.
///
/// `sai_records` accumulates per-fragment ISO/IEC 14496-12 §8.7.8–9
/// `(saiz, saio)` pairs, keyed the same way. The auxiliary-information
/// bytes themselves remain in the mdat at the offsets the `saio`
/// names; the demuxer doesn't fetch them (their semantics belong to
/// the carried `aux_info_type`, e.g. CENC `cenc`/`cbcs`).
///
/// `moof_psshes` accumulates ISO/IEC 23001-7 §8.1 `pssh` boxes that
/// live directly inside this `moof` (§8.1.1 permits pssh in either
/// `moov` or `moof`). Each captured PsshBox is keyed by the enclosing
/// `mfhd.sequence_number` so a downstream DRM layer can scope SystemID
/// matches per §8.1.1 ("readers SHALL examine all Protection System
/// Specific Header boxes in the Movie Box and in the Movie Fragment
/// Box associated with the sample (but not those in other Movie
/// Fragment Boxes)"). A malformed pssh inside a moof is dropped
/// without failing the fragment, mirroring the moov-level recovery
/// policy.
#[allow(clippy::too_many_arguments)] // per-fragment record sinks are independent vectors threaded through one moof walk; bundling them obscures which box populates which sink
fn parse_moof(
    moof: &MoofRecord,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
    senc_records: &mut Vec<SencRecord>,
    sai_records: &mut Vec<SaiRecord>,
    moof_psshes: &mut Vec<MoofPsshRecord>,
) -> Result<()> {
    let mut cur = std::io::Cursor::new(&moof.body);
    let end = moof.body.len() as u64;
    let mut moof_sequence: u32 = 0;
    // §8.1.1 allows pssh boxes at moof level; collect their bodies as
    // we walk so a pssh that precedes `mfhd` (rare but spec-legal —
    // §8.8 doesn't fix the relative order between `mfhd` and `pssh`)
    // still binds to the correct sequence_number once we've seen
    // mfhd. Finalised below.
    let mut pending_pssh_bodies: Vec<Vec<u8>> = Vec::new();
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            // §8.8.5 — `mfhd` MovieFragmentHeaderBox. Carries the
            // monotonically-increasing sequence_number. Capture it
            // so per-traf SencRecords get an ordering key.
            MFHD => {
                let body = read_bytes_vec(&mut cur, psz)?;
                // FullBox header (4 bytes) + sequence_number (u32).
                if body.len() >= 8 {
                    moof_sequence = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                }
            }
            TRAF => {
                let body = read_bytes_vec(&mut cur, psz)?;
                parse_traf(
                    &body,
                    moof.moof_start,
                    tracks,
                    samples,
                    next_dts,
                    moof_sequence,
                    senc_records,
                    sai_records,
                )?;
            }
            PSSH => {
                // ISO/IEC 23001-7 §8.1 — ProtectionSystemSpecificHeaderBox
                // at moof level. Buffer the body until the whole moof has
                // been walked (we need the captured `moof_sequence` from
                // `mfhd` to key the record). A malformed pssh body is
                // dropped without failing the fragment — mirrors the
                // moov-level recovery policy and matches the spec's
                // opaque-SystemID treatment.
                let body = read_bytes_vec(&mut cur, psz)?;
                pending_pssh_bodies.push(body);
            }
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }
    for body in pending_pssh_bodies {
        if let Ok(pssh) = parse_pssh(&body) {
            moof_psshes.push(MoofPsshRecord {
                moof_sequence,
                pssh,
            });
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

/// Decoded `sample_flags` (ISO/IEC 14496-12 §8.8.3.1) — the 32-bit
/// field that appears as `default_sample_flags` in `trex` (§8.8.3) and
/// `tfhd` (§8.8.7), and as `first_sample_flags` / per-sample
/// `sample_flags` in `trun` (§8.8.8). All four sites share one packing.
///
/// On-wire layout, MSB to LSB (§8.8.3.1):
///
/// ```text
/// bit(4)            reserved = 0;
/// unsigned int(2)   is_leading;                  // bits 27..26
/// unsigned int(2)   sample_depends_on;           // bits 25..24
/// unsigned int(2)   sample_is_depended_on;       // bits 23..22
/// unsigned int(2)   sample_has_redundancy;       // bits 21..20
/// bit(3)            sample_padding_value;        // bits 19..17
/// bit(1)            sample_is_non_sync_sample;   // bit  16
/// unsigned int(16)  sample_degradation_priority; // bits 15..0
/// ```
///
/// The four 2-bit fields (`is_leading`, `sample_depends_on`,
/// `sample_is_depended_on`, `sample_has_redundancy`) share their
/// value semantics with the `sdtp` (SampleDependencyTypeBox) entries —
/// §8.8.3.1 says they are "defined as documented in the Independent
/// and Disposable Samples Box" (§8.6.4). `sample_is_non_sync_sample`
/// inverts the §8.6.2 sync-sample table semantics: when the flag is 0
/// the sample is a sync (key) sample. `sample_padding_value` and
/// `sample_degradation_priority` carry the same meanings as the
/// `padb` / `stdp` (Padding / DegradationPriority) box entries.
///
/// The fields preserve the on-wire small-integer encoding rather than
/// being mapped to typed enums: §8.6.4.3's value tables use `0` as
/// "unknown" with format-specific overrides, and surfacing the raw
/// values keeps callers in charge of those overrides (matching the
/// approach taken for the private `SdtpEntry` already in this module).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleFlags {
    /// 2 bits (bits 27..26). §8.6.4.3 / §8.8.3.1.
    /// `0` = leading nature unknown, `1` = leading with a dependency
    /// before the referenced I-picture (not decodable),
    /// `2` = not a leading sample, `3` = leading without that
    /// dependency (decodable).
    pub is_leading: u8,
    /// 2 bits (bits 25..24). §8.6.4.3 / §8.8.3.1.
    /// `0` = dependency unknown, `1` = depends on others (not an
    /// I-picture), `2` = does not depend on others (I-picture),
    /// `3` = reserved.
    pub sample_depends_on: u8,
    /// 2 bits (bits 23..22). §8.6.4.3 / §8.8.3.1.
    /// `0` = unknown, `1` = other samples may depend on this one
    /// (not disposable), `2` = no other sample depends on this one
    /// (disposable), `3` = reserved.
    pub sample_is_depended_on: u8,
    /// 2 bits (bits 21..20). §8.6.4.3 / §8.8.3.1.
    /// `0` = redundancy unknown, `1` = redundant coding present,
    /// `2` = no redundant coding, `3` = reserved.
    pub sample_has_redundancy: u8,
    /// 3 bits (bits 19..17). §8.8.3.1 — "defined as for the padding
    /// bits". Carried verbatim without further interpretation.
    pub sample_padding_value: u8,
    /// 1 bit (bit 16). §8.8.3.1 — when `false` the sample is a sync
    /// (key) sample, mirroring the §8.6.2 sync-sample table absence
    /// semantics ("as if … all samples are sync samples, the sync
    /// sample table were absent").
    pub sample_is_non_sync_sample: bool,
    /// 16 bits (bits 15..0). §8.8.3.1 — "defined as for the
    /// degradation priority table" (§8.5.3). Producers that don't
    /// carry per-sample priority leave this zero.
    pub sample_degradation_priority: u16,
}

impl SampleFlags {
    /// Decode the 32-bit packed `sample_flags` field per §8.8.3.1.
    ///
    /// The top 4 reserved bits are not surfaced — §8.8.3.1 fixes them
    /// to zero, but we accept non-zero reserved bits without rejecting
    /// (the spec is silent on producer-side enforcement, and a
    /// well-formed reader should tolerate them rather than refuse the
    /// fragment).
    pub fn from_u32(raw: u32) -> Self {
        Self {
            is_leading: ((raw >> 26) & 0x03) as u8,
            sample_depends_on: ((raw >> 24) & 0x03) as u8,
            sample_is_depended_on: ((raw >> 22) & 0x03) as u8,
            sample_has_redundancy: ((raw >> 20) & 0x03) as u8,
            sample_padding_value: ((raw >> 17) & 0x07) as u8,
            sample_is_non_sync_sample: (raw & SAMPLE_IS_NON_SYNC) != 0,
            sample_degradation_priority: (raw & 0xFFFF) as u16,
        }
    }

    /// Round-trip helper — pack the typed fields back into the on-wire
    /// 32-bit form. Reserved bits 31..28 are emitted as zero per
    /// §8.8.3.1. Field widths are masked so out-of-range values do not
    /// bleed into neighbouring fields.
    pub fn to_u32(self) -> u32 {
        ((self.is_leading as u32 & 0x03) << 26)
            | ((self.sample_depends_on as u32 & 0x03) << 24)
            | ((self.sample_is_depended_on as u32 & 0x03) << 22)
            | ((self.sample_has_redundancy as u32 & 0x03) << 20)
            | ((self.sample_padding_value as u32 & 0x07) << 17)
            | (if self.sample_is_non_sync_sample {
                SAMPLE_IS_NON_SYNC
            } else {
                0
            })
            | self.sample_degradation_priority as u32
    }

    /// `true` when this is a sync (key) sample — the inverse of
    /// `sample_is_non_sync_sample` per §8.8.3.1. Provided as a
    /// convenience because §8.6.2 / §8.8.3.1 / §8.8.8 all use the
    /// "absence = sync" convention and downstream code typically
    /// wants the positive predicate.
    pub fn is_sync_sample(self) -> bool {
        !self.sample_is_non_sync_sample
    }
}

/// Public §8.8.3.1 typed-accessor convenience — `pub` re-export of
/// [`SampleFlags::from_u32`] for callers that have the raw on-wire
/// `u32` from a `trex` / `tfhd` / `trun` parse.
pub fn parse_sample_flags(raw: u32) -> SampleFlags {
    SampleFlags::from_u32(raw)
}

#[allow(clippy::too_many_arguments)] // tfhd / tfdt / senc / saiz / saio inputs are independent state carried through the same walk; bundling them into a struct hides the per-box ownership and obscures the moof→traf data flow
fn parse_traf(
    body: &[u8],
    moof_start: u64,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
    moof_sequence: u32,
    senc_records: &mut Vec<SencRecord>,
    sai_records: &mut Vec<SaiRecord>,
) -> Result<()> {
    // First pass: read tfhd + tfdt before the trun(s) so each trun
    // has the full default context. Also pick up senc (ISO/IEC 23001-7
    // §7.2) — it has no on-wire ordering dependency on tfhd/tfdt, but
    // parsing it needs the matching track's tenc.default_Per_Sample_IV_Size
    // to know how many IV bytes to consume per sample.
    let mut state = TrafState::default();
    let mut tfhd_seen = false;
    let mut senc_body: Option<Vec<u8>> = None;
    let mut frag_saiz: Vec<TrafSaiz> = Vec::new();
    let mut frag_saio: Vec<TrafSaio> = Vec::new();

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
            // §7.2 senc — keep the bytes; we need the track's tenc
            // first (via state.track_idx, populated by tfhd) before we
            // can interpret the per-sample IV width.
            SENC => {
                senc_body = Some(read_bytes_vec(&mut cur, psz)?);
            }
            // §8.7.8 / §8.7.9 — `saiz` / `saio` inside `traf`.
            // Promote the parsed `SaizBox` / `SaioBox` to the public
            // fragment-record shape (`TrafSaiz` / `TrafSaio`) so the
            // demuxer surface exposes the per-fragment auxiliary-info
            // table to a downstream CENC layer that needs it as a
            // `senc` alternative (per §8.8.14: in a `traf`, `saio`
            // offsets are relative to `tfhd.base_data_offset`).
            SAIZ => {
                let b = read_bytes_vec(&mut cur, psz)?;
                let sb = parse_saiz(&b)?;
                frag_saiz.push(TrafSaiz {
                    aux_info_type: sb.aux_info_type,
                    aux_info_type_parameter: sb.aux_info_type_parameter,
                    default_sample_info_size: sb.default_sample_info_size,
                    sample_count: sb.sample_count,
                    per_sample: sb.per_sample,
                });
            }
            SAIO => {
                let b = read_bytes_vec(&mut cur, psz)?;
                let sb = parse_saio(&b)?;
                frag_saio.push(TrafSaio {
                    version: sb.version,
                    aux_info_type: sb.aux_info_type,
                    aux_info_type_parameter: sb.aux_info_type_parameter,
                    offsets: sb.offsets,
                });
            }
            // We only resolve trun's in the second pass; skip here.
            TRUN => skip_cursor_bytes(&mut cur, psz),
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }

    if !tfhd_seen {
        return Err(Error::invalid("MP4: traf missing tfhd"));
    }

    let track = &tracks[state.track_idx];

    // ISO/IEC 23001-7 §7.2 — now that we know which track this traf
    // refers to, we can recover the per-sample IV width from the
    // track's tenc default and parse the captured senc body. Tracks
    // without tenc (non-CENC protection scheme, or no protection at
    // all) drop the senc on the floor — a malformed file may carry
    // it without the matching scheme box, but there's nothing the
    // demuxer can do with it without an IV-size key.
    if let Some(body) = senc_body {
        if let Some(tenc) = &track.tenc {
            if let Ok(senc) = parse_senc(&body, tenc.default_per_sample_iv_size) {
                senc_records.push(SencRecord {
                    track_idx: state.track_idx as u32,
                    moof_sequence,
                    senc,
                });
            }
        }
    }

    // §8.7.8–9 — if this `traf` carried any auxiliary-information
    // boxes, record them as one `SaiRecord` keyed by the same
    // `(track_idx, moof_sequence)` pair as the senc record. The
    // demuxer leaves pairing (`saiz` ↔ `saio` by `(aux_info_type,
    // aux_info_type_parameter)`) to the consumer, so even an
    // unmatched box (a producer slip — `saiz` without `saio` or vice
    // versa) is preserved verbatim for inspection.
    if !frag_saiz.is_empty() || !frag_saio.is_empty() {
        sai_records.push(SaiRecord {
            track_idx: state.track_idx as u32,
            moof_sequence,
            saiz: frag_saiz,
            saio: frag_saio,
        });
    }

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
            skip_cursor_bytes(&mut cur, psz);
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

// --- Random-access index parsers (§8.16.3 sidx, §8.8.10–11 mfra/tfra) ----

/// §8.16.3 — `sidx` SegmentIndexBox.
///
/// Layout (FullBox 4 bytes already consumed by caller):
///   reference_ID (u32),
///   timescale (u32),
///   if version==0: earliest_presentation_time (u32) + first_offset (u32)
///   if version==1: earliest_presentation_time (u64) + first_offset (u64)
///   reserved (u16) = 0
///   reference_count (u16)
///   for each ref:
///     [bit 31] reference_type (1 = sidx, 0 = media subseg)
///     [bits 30..0] referenced_size (u31)
///     subsegment_duration (u32)
///     [bit 31] starts_with_SAP (1 bit)
///     [bits 30..28] SAP_type (3 bits)
///     [bits 27..0] SAP_delta_time (u28) — we don't surface
///
/// `sidx_end_offset` is the absolute file offset of the first byte AFTER
/// the sidx box (its body's spec-defined anchor for `first_offset`).
fn parse_sidx(body: &[u8], sidx_end_offset: u64) -> Result<Option<SidxRecord>> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: sidx too short"));
    }
    let version = body[0];
    let mut off = 4usize; // skip version + 3-byte flags
    let reference_id = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    off += 4;
    let timescale = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    off += 4;
    let (ept, first_offset) = if version == 0 {
        if off + 8 > body.len() {
            return Err(Error::invalid("MP4: sidx v0 truncated"));
        }
        let e = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64;
        off += 4;
        let f = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64;
        off += 4;
        (e, f)
    } else {
        if off + 16 > body.len() {
            return Err(Error::invalid("MP4: sidx v1 truncated"));
        }
        let e = u64::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
            body[off + 4],
            body[off + 5],
            body[off + 6],
            body[off + 7],
        ]);
        off += 8;
        let f = u64::from_be_bytes([
            body[off],
            body[off + 1],
            body[off + 2],
            body[off + 3],
            body[off + 4],
            body[off + 5],
            body[off + 6],
            body[off + 7],
        ]);
        off += 8;
        (e, f)
    };
    if off + 4 > body.len() {
        return Err(Error::invalid("MP4: sidx header truncated"));
    }
    // skip 2-byte reserved
    let reference_count = u16::from_be_bytes([body[off + 2], body[off + 3]]) as usize;
    off += 4;
    let needed = reference_count.saturating_mul(12);
    if off + needed > body.len() {
        return Err(Error::invalid("MP4: sidx references truncated"));
    }
    let mut references = Vec::with_capacity(reference_count);
    for _ in 0..reference_count {
        let r0 = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        let r1 = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        let r2 = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        off += 4;
        let is_sidx = (r0 & 0x8000_0000) != 0;
        let referenced_size = r0 & 0x7FFF_FFFF;
        let starts_with_sap = (r2 & 0x8000_0000) != 0;
        let sap_type = ((r2 >> 28) & 0x7) as u8;
        references.push(SidxReference {
            is_sidx,
            referenced_size,
            subsegment_duration: r1,
            starts_with_sap,
            sap_type,
        });
    }
    Ok(Some(SidxRecord {
        reference_id,
        timescale,
        earliest_presentation_time: ept,
        first_byte_offset: sidx_end_offset.saturating_add(first_offset),
        references,
    }))
}

/// §8.16.4 — `ssix` SubsegmentIndexBox.
///
/// Layout (§8.16.4.2):
///
/// ```text
///     FullBox(ssix, version = 0, flags = 0)
///     unsigned int(32) subsegment_count;
///     for (i = 1; i <= subsegment_count; i++) {
///         unsigned int(32) range_count;
///         for (j = 1; j <= range_count; j++) {
///             unsigned int(8)  level;
///             unsigned int(24) range_size;
///         }
///     }
/// }
/// ```
///
/// Only version 0 is defined; a different version byte is rejected
/// rather than mis-read against the v0 layout. `subsegment_count` and
/// each `range_count` are attacker-controlled u32s, so capacity is
/// pre-allocated against the bytes actually remaining (4 per range,
/// ≥ 4 per subsegment), never against the declared counts; any count
/// that outruns the body is a truncation error.
fn parse_ssix(body: &[u8]) -> Result<SsixRecord> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: ssix too short"));
    }
    if body[0] != 0 {
        return Err(Error::invalid("MP4: ssix unsupported version"));
    }
    // bytes 1..4 are flags (always 0 per §8.16.4.2) — not consumed.
    let subsegment_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let mut cursor = 8usize;
    let mut subsegments =
        Vec::with_capacity(subsegment_count.min(body.len().saturating_sub(cursor) / 4));
    for _ in 0..subsegment_count {
        if body.len() - cursor < 4 {
            return Err(Error::invalid("MP4: ssix subsegment truncated"));
        }
        let range_count = u32::from_be_bytes([
            body[cursor],
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
        ]) as usize;
        cursor += 4;
        if (body.len() - cursor) / 4 < range_count {
            return Err(Error::invalid("MP4: ssix range list truncated"));
        }
        let mut ranges = Vec::with_capacity(range_count);
        for _ in 0..range_count {
            let level = body[cursor];
            let range_size =
                u32::from_be_bytes([0, body[cursor + 1], body[cursor + 2], body[cursor + 3]]);
            cursor += 4;
            ranges.push(SsixRange { level, range_size });
        }
        subsegments.push(SsixSubsegment { ranges });
    }
    Ok(SsixRecord { subsegments })
}

/// §8.16.5 — `prft` ProducerReferenceTimeBox.
///
/// Layout (FullBox 4 bytes already consumed by caller):
///   reference_track_ID (u32),
///   ntp_timestamp (u64 — NTP-format UTC time, RFC 5905),
///   if version == 0: media_time (u32)
///   if version == 1: media_time (u64)
///
/// Total body length: 16 bytes (v0) or 20 bytes (v1).
///
/// Per §8.16.5.3, `media_time` is on the reference track's media
/// clock and corresponds to the same instant as `ntp_timestamp`. The
/// box is associated with the NEXT `moof` in file order (§8.16.5.1
/// placement rule); we don't enforce that ordering at parse time —
/// callers correlate by file position.
/// Parse a `pdin` (ProgressiveDownloadInfoBox, ISO/IEC 14496-12 §8.1.3)
/// body. The body is the bytes after the 8/16-byte box header.
///
/// Layout (§8.1.3.2):
///
/// ```text
///     FullBox(pdin, version = 0, flags = 0)
///     for (i = 0; ; i++) {        // to end of box
///         unsigned int(32) rate;
///         unsigned int(32) initial_delay;
///     }
/// ```
///
/// The 4-byte FullBox preamble is consumed and verified to fit. The
/// remaining payload must be a multiple of 8 bytes (one u32 pair per
/// entry); a body whose post-preamble length is not a multiple of 8
/// is rejected as malformed rather than silently truncated to the
/// nearest pair. A zero-pair body is permitted and yields an empty
/// `entries` vec — the box is informational and an empty table is
/// the producer's way of saying "no progressive-download hints
/// available".
///
/// Version is pinned to 0 by §8.1.3.2; a non-zero version is tolerated
/// (the on-wire layout is unambiguous) rather than rejected, mirroring
/// `parse_padb` / `parse_stdp` posture for FullBox version-bit slips.
fn parse_pdin(body: &[u8]) -> Result<PdinRecord> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: pdin too short"));
    }
    // Skip the 4-byte FullBox preamble (version + flags). §8.1.3.2 pins
    // version = 0 and flags = 0; we tolerate anything the producer
    // wrote because the payload layout is unambiguous.
    let payload = &body[4..];
    if payload.len() % 8 != 0 {
        return Err(Error::invalid(
            "MP4: pdin payload not a multiple of 8 bytes (rate + initial_delay pairs)",
        ));
    }
    let count = payload.len() / 8;
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * 8;
        let rate = u32::from_be_bytes([
            payload[off],
            payload[off + 1],
            payload[off + 2],
            payload[off + 3],
        ]);
        let initial_delay = u32::from_be_bytes([
            payload[off + 4],
            payload[off + 5],
            payload[off + 6],
            payload[off + 7],
        ]);
        entries.push(PdinEntry {
            rate,
            initial_delay,
        });
    }
    Ok(PdinRecord { entries })
}

/// Parse a `trep` TrackExtensionPropertiesBox body (ISO/IEC 14496-12
/// §8.8.15).
///
/// Wire layout (§8.8.15.2):
/// ```text
/// class TrackExtensionPropertiesBox extends FullBox('trep', 0, 0) {
///     unsigned int(32) track_id;
///     // Any number of boxes may follow
/// }
/// ```
///
/// The body is a 4-byte FullBox preamble (version + flags), a 4-byte
/// `track_id`, then "any number of boxes" (§8.8.15.2). Those trailing
/// children — e.g. `assp` (§8.8.16) — are recorded by type + payload
/// length only ([`TrepChild`]); this parser does not recurse into them.
///
/// Version is pinned to 0 by §8.8.15.2; a non-zero version is tolerated
/// (the layout up to `track_id` is unambiguous) rather than rejected,
/// matching the `parse_leva` / `parse_pdin` posture for FullBox
/// version-bit slips.
///
/// A child box whose declared size overruns the remaining `trep` body
/// is clamped to the bytes available and recorded with the clamped
/// length; parsing then stops (a child can't legally extend past its
/// parent, and continuing would desynchronise the walk). A child header
/// that itself doesn't fit (fewer than 8 bytes left) ends the child
/// loop. Neither case rejects the box — the `track_id` is still useful.
fn parse_trep(body: &[u8]) -> Result<TrepRecord> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: trep too short for FullBox preamble"));
    }
    // Skip the 4-byte FullBox preamble (version + flags). §8.8.15.2
    // pins version = 0 and flags = 0; tolerated otherwise (the layout
    // up to track_id is fixed regardless).
    let payload = &body[4..];
    if payload.len() < 4 {
        return Err(Error::invalid("MP4: trep truncated track_id"));
    }
    let track_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);

    // Walk the trailing "any number of boxes" recording each child's
    // type + payload length. We parse the child headers inline rather
    // than via `read_box_header` so a clamped/over-long child can't
    // abort the whole parse (the demuxer treats trep as informational).
    let mut children = Vec::new();
    let mut off = 4usize;
    while off + 8 <= payload.len() {
        let size32 = u32::from_be_bytes([
            payload[off],
            payload[off + 1],
            payload[off + 2],
            payload[off + 3],
        ]) as usize;
        let fourcc = [
            payload[off + 4],
            payload[off + 5],
            payload[off + 6],
            payload[off + 7],
        ];
        // §4.2 box-size semantics: size 0 means "to end of enclosing
        // box"; size 1 means a 64-bit largesize follows. trep children
        // are small property boxes, so we read the 64-bit largesize
        // when present and treat size 0 as "rest of the trep body".
        let (header_len, box_size) = match size32 {
            0 => (8usize, payload.len() - off),
            1 => {
                if off + 16 > payload.len() {
                    // largesize header doesn't fit — stop the walk.
                    break;
                }
                let large = u64::from_be_bytes([
                    payload[off + 8],
                    payload[off + 9],
                    payload[off + 10],
                    payload[off + 11],
                    payload[off + 12],
                    payload[off + 13],
                    payload[off + 14],
                    payload[off + 15],
                ]) as usize;
                (16usize, large)
            }
            n => (8usize, n),
        };
        // A box can't be smaller than its own header. A malformed
        // small size ends the walk rather than looping forever.
        if box_size < header_len {
            break;
        }
        // Clamp an over-long child to the bytes that remain in the
        // trep body — a child legally can't extend past its parent.
        let available = payload.len() - off;
        let effective = box_size.min(available);
        let payload_len = effective - header_len;
        children.push(TrepChild {
            fourcc,
            payload_len,
        });
        off += effective;
        // If we clamped (the declared size ran past the parent), there
        // are no further siblings — stop cleanly.
        if effective < box_size {
            break;
        }
    }

    Ok(TrepRecord { track_id, children })
}

/// Parse a `leva` LevelAssignmentBox body (ISO/IEC 14496-12 §8.8.13).
///
/// Wire layout (§8.8.13.2):
/// ```text
/// FullBox('leva', 0, 0) {
///     unsigned int(8)  level_count;
///     for (j = 1; j <= level_count; j++) {
///         unsigned int(32) track_id;
///         unsigned int(1)  padding_flag;
///         unsigned int(7)  assignment_type;
///         if (assignment_type == 0)      { unsigned int(32) grouping_type; }
///         else if (assignment_type == 1) { unsigned int(32) grouping_type;
///                                          unsigned int(32) grouping_type_parameter; }
///         else if (assignment_type == 2) {}
///         else if (assignment_type == 3) {}
///         else if (assignment_type == 4) { unsigned int(32) sub_track_id; }
///     }
/// }
/// ```
///
/// Each entry is 4 (track_id) + 1 (padding_flag + assignment_type) + the
/// type-specific tail (0/4/8/0/0/4 bytes for type 2/0/1/2/3/4
/// respectively); a truncated tail rejects the whole box rather than
/// silently dropping the rest of the table — a partial table would lie
/// about which levels exist.
///
/// Version is pinned to 0 by §8.8.13.2; a non-zero version is tolerated
/// (the on-wire layout is unambiguous) rather than rejected, mirroring
/// the `parse_pdin` / `parse_padb` / `parse_stdp` posture for FullBox
/// version-bit slips.
///
/// The §8.8.13.3 "level_count ≥ 2" rule is **not** enforced — the spec
/// pins a minimum but the demuxer carries whatever the producer wrote
/// so downstream validators can flag it. A `level_count` of 0 yields an
/// empty `entries` vec.
///
/// Reserved `assignment_type` values (> 4) are carried verbatim with
/// the variant-specific fields left at 0; the spec says they're
/// reserved so this parser doesn't attempt to consume an unknown tail
/// (any non-empty tail for a reserved type would desynchronise the
/// loop). A reserved value therefore takes 5 bytes.
fn parse_leva(body: &[u8]) -> Result<LevaRecord> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: leva too short for FullBox preamble"));
    }
    // Skip the 4-byte FullBox preamble (version + flags). §8.8.13.2 pins
    // version = 0 and flags = 0; tolerated otherwise (layout is fixed).
    let payload = &body[4..];
    if payload.is_empty() {
        return Err(Error::invalid("MP4: leva missing level_count byte"));
    }
    let level_count = payload[0] as usize;
    let mut cursor = 1usize;
    let mut entries = Vec::with_capacity(level_count);
    for _ in 0..level_count {
        // Need at least 5 bytes for track_id (4) + padding+assignment (1).
        if cursor + 5 > payload.len() {
            return Err(Error::invalid(
                "MP4: leva truncated entry header (need 5 bytes for track_id+padding+assignment)",
            ));
        }
        let track_id = u32::from_be_bytes([
            payload[cursor],
            payload[cursor + 1],
            payload[cursor + 2],
            payload[cursor + 3],
        ]);
        let packed = payload[cursor + 4];
        // §8.8.13.2: high bit is padding_flag, low 7 bits are
        // assignment_type.
        let padding_flag = (packed & 0x80) != 0;
        let assignment_type = packed & 0x7f;
        cursor += 5;

        let mut grouping_type = 0u32;
        let mut grouping_type_parameter = 0u32;
        let mut sub_track_id = 0u32;
        match assignment_type {
            0 => {
                if cursor + 4 > payload.len() {
                    return Err(Error::invalid(
                        "MP4: leva truncated grouping_type for assignment_type=0",
                    ));
                }
                grouping_type = u32::from_be_bytes([
                    payload[cursor],
                    payload[cursor + 1],
                    payload[cursor + 2],
                    payload[cursor + 3],
                ]);
                cursor += 4;
            }
            1 => {
                if cursor + 8 > payload.len() {
                    return Err(Error::invalid(
                        "MP4: leva truncated grouping_type/parameter for assignment_type=1",
                    ));
                }
                grouping_type = u32::from_be_bytes([
                    payload[cursor],
                    payload[cursor + 1],
                    payload[cursor + 2],
                    payload[cursor + 3],
                ]);
                grouping_type_parameter = u32::from_be_bytes([
                    payload[cursor + 4],
                    payload[cursor + 5],
                    payload[cursor + 6],
                    payload[cursor + 7],
                ]);
                cursor += 8;
            }
            2 | 3 => {
                // §8.8.13.2 explicitly defines no further syntax
                // elements for assignment_type 2 (level assignment by
                // track) and 3 (by track-subsegment). The five-byte
                // header is the whole entry.
            }
            4 => {
                if cursor + 4 > payload.len() {
                    return Err(Error::invalid(
                        "MP4: leva truncated sub_track_id for assignment_type=4",
                    ));
                }
                sub_track_id = u32::from_be_bytes([
                    payload[cursor],
                    payload[cursor + 1],
                    payload[cursor + 2],
                    payload[cursor + 3],
                ]);
                cursor += 4;
            }
            _ => {
                // §8.8.13.3 — values > 4 are reserved. The spec doesn't
                // define a tail length so we can't safely consume any
                // extra bytes; the entry stands as the 5-byte header.
                // A downstream validator can flag the reserved type.
            }
        }
        entries.push(LevaEntry {
            track_id,
            padding_flag,
            assignment_type,
            grouping_type,
            grouping_type_parameter,
            sub_track_id,
        });
    }
    Ok(LevaRecord { entries })
}

fn parse_prft(body: &[u8]) -> Result<Option<PrftRecord>> {
    if body.len() < 16 {
        return Err(Error::invalid("MP4: prft too short"));
    }
    let version = body[0];
    // skip 3-byte flags (always 0 per §8.16.5.2)
    let reference_track_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let ntp_timestamp = u64::from_be_bytes([
        body[8], body[9], body[10], body[11], body[12], body[13], body[14], body[15],
    ]);
    let media_time: u64 = if version == 0 {
        if body.len() < 20 {
            return Err(Error::invalid("MP4: prft v0 truncated"));
        }
        u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as u64
    } else {
        // Per §8.16.5.2, version 1 widens media_time to 64-bit. Any
        // version != 0 we treat as v1 (no other version is defined).
        if body.len() < 24 {
            return Err(Error::invalid("MP4: prft v1 truncated"));
        }
        u64::from_be_bytes([
            body[16], body[17], body[18], body[19], body[20], body[21], body[22], body[23],
        ])
    };
    Ok(Some(PrftRecord {
        reference_track_id,
        ntp_timestamp,
        media_time,
        version,
    }))
}

/// §8.8.10 — `mfra` MovieFragmentRandomAccessBox. Container for one
/// `tfra` per track-with-random-access plus the size-of-mfra `mfro`
/// trailer. We collect the tfra entries; mfro is not consumed (we
/// already know the mfra size from the box header).
fn parse_mfra(body: &[u8], out: &mut Vec<TfraRecord>) -> Result<()> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        match hdr.fourcc {
            TFRA => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Some(r) = parse_tfra(&b)? {
                    out.push(r);
                }
            }
            MFRO => {
                // mfro: FullBox + size (u32). We've already read the
                // outer mfra box header so the size is redundant here.
                skip_cursor_bytes(&mut cur, psz);
            }
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }
    Ok(())
}

/// §8.8.11 — `tfra` TrackFragmentRandomAccessBox.
///
/// Layout (FullBox 4 bytes consumed by caller):
///   track_ID (u32),
///   reserved (28-bit zero) + length_size_of_traf_num (2 bits) +
///     length_size_of_trun_num (2 bits) + length_size_of_sample_num (2 bits)
///   number_of_entry (u32)
///   for each entry:
///     time         — u32 (v0) or u64 (v1)
///     moof_offset  — u32 (v0) or u64 (v1)
///     traf_number  — variable (1..=4 bytes)
///     trun_number  — variable (1..=4 bytes)
///     sample_number — variable (1..=4 bytes)
fn parse_tfra(body: &[u8]) -> Result<Option<TfraRecord>> {
    if body.len() < 12 {
        return Err(Error::invalid("MP4: tfra too short"));
    }
    let version = body[0];
    let mut off = 4usize;
    let track_id = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    off += 4;
    let lengths = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
    off += 4;
    // Per spec: each `length_size_of_*` is 2 bits encoding (0..=3),
    // and the actual byte length is value+1.
    let len_traf = (((lengths >> 4) & 0x3) as usize) + 1;
    let len_trun = (((lengths >> 2) & 0x3) as usize) + 1;
    let len_sample = ((lengths & 0x3) as usize) + 1;
    if off + 4 > body.len() {
        return Err(Error::invalid("MP4: tfra entry_count truncated"));
    }
    let n = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as usize;
    off += 4;
    let mut entries = Vec::with_capacity(n);
    let entry_size = if version == 1 { 16 } else { 8 } + len_traf + len_trun + len_sample;
    if off + n.saturating_mul(entry_size) > body.len() {
        return Err(Error::invalid("MP4: tfra entries truncated"));
    }
    for _ in 0..n {
        let (time, moof_offset) = if version == 1 {
            let t = u64::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
                body[off + 4],
                body[off + 5],
                body[off + 6],
                body[off + 7],
            ]);
            off += 8;
            let m = u64::from_be_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
                body[off + 4],
                body[off + 5],
                body[off + 6],
                body[off + 7],
            ]);
            off += 8;
            (t, m)
        } else {
            let t =
                u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64;
            off += 4;
            let m =
                u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]) as u64;
            off += 4;
            (t, m)
        };
        let traf_number = read_var_u32(&body[off..off + len_traf]);
        off += len_traf;
        let trun_number = read_var_u32(&body[off..off + len_trun]);
        off += len_trun;
        let sample_number = read_var_u32(&body[off..off + len_sample]);
        off += len_sample;
        entries.push(TfraEntry {
            time,
            moof_offset,
            traf_number,
            trun_number,
            sample_number,
        });
    }
    Ok(Some(TfraRecord { track_id, entries }))
}

/// Read a 1..=4 byte big-endian unsigned integer into a u32.
fn read_var_u32(buf: &[u8]) -> u32 {
    let mut v: u32 = 0;
    for &b in buf {
        v = (v << 8) | b as u32;
    }
    v
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
        MediaType::Subtitle => CodecParameters::subtitle(codec_id),
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

    // ISO/IEC 14496-12 §8.12: when the track's sample entry was wrapped
    // as `enc*`, surface the recovered scheme type so callers can
    // detect (and refuse, or hand off to a CENC layer) protected
    // tracks. The codec id has already been remapped to the original
    // un-transformed FourCC via `sinf/frma` so this is the only place
    // the protection is visible on the public stream surface.
    if let Some(scheme) = t.protection_scheme {
        let scheme_str = std::str::from_utf8(&scheme).unwrap_or("????").to_string();
        params.options.insert("protection_scheme", scheme_str);
    }

    // ISO/IEC 23001-7 §8.2: when a CENC TrackEncryptionBox was
    // present inside `sinf/schi`, surface its defaults on
    // `params.options` so a downstream decryption layer can recover
    // them without a second pass. Keys:
    //
    //   cenc_default_kid               16-byte KID, lowercase hex
    //   cenc_default_is_protected      "0" or "1"
    //   cenc_default_iv_size           0 / 8 / 16
    //   cenc_default_crypt_byte_block  v1 only, decimal (omitted on v0)
    //   cenc_default_skip_byte_block   v1 only, decimal (omitted on v0)
    //   cenc_default_constant_iv       lowercase hex, only when
    //                                  isProtected==1 && iv_size==0
    //   cenc_tenc_version              "0" or "1"
    //
    // All keys are absent for unprotected (or non-CENC-protected)
    // tracks. Hex encoding mirrors the `pssh_<n>` SystemID surface.
    if let Some(tenc) = &t.tenc {
        let kid_hex: String = tenc
            .default_kid
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        params.options.insert("cenc_default_kid", kid_hex);
        params.options.insert(
            "cenc_default_is_protected",
            tenc.default_is_protected.to_string(),
        );
        params.options.insert(
            "cenc_default_iv_size",
            tenc.default_per_sample_iv_size.to_string(),
        );
        params
            .options
            .insert("cenc_tenc_version", tenc.version.to_string());
        if tenc.version >= 1 {
            params.options.insert(
                "cenc_default_crypt_byte_block",
                tenc.default_crypt_byte_block.to_string(),
            );
            params.options.insert(
                "cenc_default_skip_byte_block",
                tenc.default_skip_byte_block.to_string(),
            );
        }
        if let Some(civ) = &tenc.default_constant_iv {
            let iv_hex: String = civ.iter().map(|b| format!("{b:02x}")).collect();
            params.options.insert("cenc_default_constant_iv", iv_hex);
        }
    }

    // ISO/IEC 14496-12 §8.4.6: when the track carries an `elng`
    // ExtendedLanguageBox, surface its BCP 47 (RFC 4646) tag on
    // `params.options["language"]` (e.g. `en-US`, `zh-Hant`). This is
    // richer than `mdhd`'s packed 3-character ISO 639-2 code (which
    // can't express region/script/variant subtags) and overrides it
    // per §8.4.6.1 when the two disagree.
    if let Some(tag) = &t.elng {
        params.options.insert("language", tag.clone());
    }

    // ISO/IEC 14496-12 §8.3.3: surface track references as
    // `tref_<type>` → space-separated track IDs. Callers use this to
    // wire up subtitle→video (`subt`), chapter (`chap`), content
    // description (`cdsc`), font (`font`), hint (`hint`), depth/parallax
    // auxiliary video (`vdep` / `vplx`), and dependency (`hind`)
    // relationships. Reference types whose FourCC contains non-printable
    // bytes are still surfaced (the key is utf8-lossy so they don't
    // panic), but downstream callers should treat unknown types as
    // opaque.
    for (ref_type, ids) in &t.tref {
        if ids.is_empty() {
            continue;
        }
        let type_str = std::str::from_utf8(ref_type)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| {
                format!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    ref_type[0], ref_type[1], ref_type[2], ref_type[3]
                )
            });
        let value = ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        params.options.insert(format!("tref_{}", type_str), value);
    }

    // ISO/IEC 14496-12 §8.3.4: surface each `trgr` (TrackGroupBox)
    // child as `trgr_<n>` where `<n>` is the 0-based encounter index.
    // Value is `"<track_group_type> <track_group_id>"` — the FourCC
    // type and the 32-bit identifier, space-separated, mirroring the
    // `kind_<n>` two-field shape. Tracks sharing the same
    // `(type, id)` pair belong to the same group (§8.3.4.3); this is
    // a track-group membership signal, not a dependency relationship
    // (use `tref_<type>` for the latter). Reference types whose
    // FourCC contains non-printable bytes fall back to an 8-digit hex
    // representation, matching the `tref_<type>` convention.
    for (i, (group_type, group_id)) in t.trgr.iter().enumerate() {
        let type_str = std::str::from_utf8(group_type)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| {
                format!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    group_type[0], group_type[1], group_type[2], group_type[3]
                )
            });
        params
            .options
            .insert(format!("trgr_{}", i), format!("{} {}", type_str, group_id));
    }

    // ISO/IEC 14496-12 §8.10.4: surface each track-level `kind`
    // (KindBox) on `params.options` as `kind_<n>` where `<n>` is the
    // 0-based encounter index. Value is the URI alone when the box
    // carried no name (URI uniquely identifies the kind), or
    // "URI value" (space-separated, mirroring the `tref_<type>`
    // surface convention) when both fields are populated. Multiple
    // schemes (e.g. one DASH role + one iTunes role) co-label the
    // same track with distinct keys.
    for (i, (uri, value)) in t.kinds.iter().enumerate() {
        let v = if value.is_empty() {
            uri.clone()
        } else {
            format!("{} {}", uri, value)
        };
        params.options.insert(format!("kind_{}", i), v);
    }

    // ISO/IEC 14496-12 §8.10.2: surface each track-level `cprt`
    // (CopyrightBox) on `params.options` as a pair: `copyright_<n>`
    // carries the decoded notice string and `copyright_<n>_lang` carries
    // the 3-letter ISO 639-2/T language code. The key is omitted when
    // the notice is empty, mirroring the `kind_<n>` posture that a
    // value-less informational box does not pollute the option map; the
    // accompanying `_lang` key is still emitted for a present-but-empty
    // notice when the language tag itself is well-formed ASCII letters,
    // so a consumer can tell "track declares an empty notice in `eng`"
    // from "track has no `cprt` at all". When the decoded language
    // bytes are outside the spec's `'a'..='z'` range (a malformed or
    // zero-packed source word) the `_lang` key falls back to 8-digit
    // hex of the raw 16-bit packed value, matching the `tref_<type>` /
    // `trgr_<n>` non-printable fallback convention.
    for (i, c) in t.copyrights.iter().enumerate() {
        let lang_str = if c.language.iter().all(|&b| b.is_ascii_lowercase()) {
            std::str::from_utf8(&c.language).unwrap().to_string()
        } else {
            // Reverse the packed-language decode to recover the original
            // 16-bit word for a stable surface even when the bytes are
            // out of the spec's lowercase-letter range.
            let raw: u32 = ((c.language[0].wrapping_sub(0x60) as u32) << 10)
                | ((c.language[1].wrapping_sub(0x60) as u32) << 5)
                | (c.language[2].wrapping_sub(0x60) as u32);
            format!("{:08x}", raw)
        };
        if !c.notice.is_empty() {
            params
                .options
                .insert(format!("copyright_{}", i), c.notice.clone());
        }
        params
            .options
            .insert(format!("copyright_{}_lang", i), lang_str);
    }

    // ISO/IEC 14496-12 §8.10.3: surface the optional `tsel`
    // (TrackSelectionBox) — when present, emit two `params.options`
    // keys. `tsel_switch_group` is the signed 32-bit identifier
    // grouping interchangeable-during-playback tracks (§8.10.3.4); a
    // value of 0 still surfaces because it lets a consumer tell a
    // present-but-zero `tsel` from an absent one. `tsel_attributes`
    // is the space-separated list of FourCC tags from
    // `attribute_list[]` (§8.10.3.5: descriptive `tesc` / `fgsc` /
    // `cgsc` / `spsc` / `resc` / `vwsc` and differentiating `bitr` /
    // `cdec` / `lang` / …); the key is omitted when the list is
    // empty. FourCCs whose bytes contain non-printable values fall
    // back to 8-digit hex (matching the `tref_<type>` / `trgr_<n>`
    // convention). Absent `tsel`, neither key is emitted.
    if let Some(tsel) = &t.tsel {
        params
            .options
            .insert("tsel_switch_group", tsel.switch_group.to_string());
        if !tsel.attribute_list.is_empty() {
            let v = tsel
                .attribute_list
                .iter()
                .map(|a| {
                    std::str::from_utf8(a)
                        .ok()
                        .filter(|s| s.chars().all(|c| !c.is_control()))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!("{:02x}{:02x}{:02x}{:02x}", a[0], a[1], a[2], a[3])
                        })
                })
                .collect::<Vec<_>>()
                .join(" ");
            params.options.insert("tsel_attributes", v);
        }
    }

    // ISO/IEC 14496-12 §8.14: surface each Sub Track (`strk`) the track
    // declared. A sub track assigns *part* of a track to alternate /
    // switch groups for layered-codec media selection (§8.14.1). Each is
    // emitted on `params.options` as `subtrack_<n>` (0-based encounter
    // index) carrying the `stri` (§8.14.4) selection fields:
    //   `"id=<sub_track_ID> switch=<switch_group> alt=<alternate_group>
    //     [attrs=<fourcc...>] [stsg=<grouping_type>:<idx>,<idx>;...]"`
    // The `switch` / `alt` fields are always emitted (0 is meaningful —
    // it distinguishes present-but-unassigned from a missing field); the
    // `attrs=` block lists the §8.14.4.3 attribute FourCCs (descriptive
    // `tesc`/`fgsc`/… and differentiating `bitr`/`frar`/`nvws`/…) and is
    // omitted when empty; the `stsg=` block lists each `strd/stsg`
    // (§8.14.6) sub-track sample-group definition as
    // `<grouping_type>:<index>,<index>,…` and is omitted when the sub
    // track declared none. FourCCs whose bytes are non-printable fall
    // back to 8-digit hex (matching the `tsel_attributes` convention).
    // Absent `strk`, no keys are emitted.
    for (n, st) in t.sub_tracks.iter().enumerate() {
        use std::fmt::Write;
        let mut s = format!(
            "id={} switch={} alt={}",
            st.sub_track_id, st.switch_group, st.alternate_group
        );
        if !st.attribute_list.is_empty() {
            let attrs = st
                .attribute_list
                .iter()
                .map(fourcc_token)
                .collect::<Vec<_>>()
                .join(" ");
            let _ = write!(s, " attrs={attrs}");
        }
        if !st.sample_groups.is_empty() {
            let groups = st
                .sample_groups
                .iter()
                .map(|sg| {
                    let idxs = sg
                        .group_description_indices
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{}:{}", fourcc_token(&sg.grouping_type), idxs)
                })
                .collect::<Vec<_>>()
                .join(";");
            let _ = write!(s, " stsg={groups}");
        }
        params.options.insert(format!("subtrack_{n}"), s);
    }

    // ISO/IEC 14496-12 §8.6.1.4: when the track carries a `cslg`
    // (CompositionToDecodeBox), surface its five timeline-relation
    // fields on `params.options` as `cslg_<field>`. These document the
    // composition↔decode relationship implied by a signed (v1) `ctts`:
    // the DTS shift that guarantees CTS ≥ DTS, the least/greatest
    // composition offsets, and the composition start/end times. Callers
    // building an accurate presentation timeline (e.g. to size an output
    // edit or clamp the leading gap) can read these instead of scanning
    // the whole `ctts`. All values are in the track media timescale.
    if let Some(c) = &t.cslg {
        params.options.insert(
            "cslg_composition_to_dts_shift",
            c.composition_to_dts_shift.to_string(),
        );
        params.options.insert(
            "cslg_least_decode_to_display_delta",
            c.least_decode_to_display_delta.to_string(),
        );
        params.options.insert(
            "cslg_greatest_decode_to_display_delta",
            c.greatest_decode_to_display_delta.to_string(),
        );
        params.options.insert(
            "cslg_composition_start_time",
            c.composition_start_time.to_string(),
        );
        params.options.insert(
            "cslg_composition_end_time",
            c.composition_end_time.to_string(),
        );
    }

    // ISO/IEC 14496-12 §12.1.2 / §8.4.5: when the track carries a
    // `vmhd` (VideoMediaHeaderBox) — i.e. a video track — surface its
    // composition mode and operation colour on `params.options`.
    // `vmhd_graphicsmode` is the decimal composition mode (0 = copy);
    // `vmhd_opcolor` is the three (red, green, blue) 16-bit components
    // space-separated, decimal. Callers that compose this track over an
    // existing image read these instead of re-walking `minf`.
    if let Some(v) = &t.vmhd {
        params
            .options
            .insert("vmhd_graphicsmode", v.graphicsmode.to_string());
        params.options.insert(
            "vmhd_opcolor",
            format!("{} {} {}", v.opcolor[0], v.opcolor[1], v.opcolor[2]),
        );
    }

    // ISO/IEC 14496-12 §8.6.3: surface the optional ShadowSyncSampleBox
    // (`stsh`) as `stsh_<n>` options keys (0-based encounter index),
    // each `"shadowed sync"` — the 1-based shadowed sample number and
    // the 1-based sync sample that may replace it when seeking to (or
    // before) that sample. The convention mirrors `tref_<type>` /
    // `kind_<n>`. The table is a seek optimisation only; a track with no
    // `stsh` emits none of the keys.
    for (i, (shadowed, sync)) in t.stsh.iter().enumerate() {
        params
            .options
            .insert(format!("stsh_{}", i), format!("{} {}", shadowed, sync));
    }

    // ISO/IEC 14496-12 §8.5.2: surface the full SampleDescriptionBox
    // entry list when a track carries more than one sample description.
    // Entry [0] always drives active decode (its FourCC is the track's
    // `codec` / `codec_id_fourcc`), so for the overwhelmingly common
    // single-entry case there is nothing extra to say and no keys are
    // emitted. When `entry_count > 1` the track can switch descriptions
    // mid-stream via a per-chunk `stsc.sample_description_index` (≥ 2) or
    // a fragment's `tfhd` / `trex` `sample_description_index`; the
    // alternatives are surfaced so a caller can map such an index to the
    // FourCC it selects:
    //   * `stsd_count` — the total number of sample-description entries.
    //   * `stsd_<n>` (1-based, matching the spec's 1-based
    //     `sample_description_index`) — `<fourcc> dref=<data_reference_index>`
    //     for every entry. Entry 1 is the active one (also reflected in
    //     `codec`); entries ≥ 2 are the alternates. The 1-based key index
    //     lets a `sample_description_index` value be looked up directly as
    //     `stsd_<index>`.
    // Absent multiple entries, none of these keys are emitted.
    if t.stsd_entries.len() > 1 {
        params
            .options
            .insert("stsd_count".to_string(), t.stsd_entries.len().to_string());
        for (i, e) in t.stsd_entries.iter().enumerate() {
            params.options.insert(
                format!("stsd_{}", i + 1),
                format!(
                    "{} dref={}",
                    fourcc_token(&e.format),
                    e.data_reference_index
                ),
            );
        }
    }

    // ISO/IEC 14496-12 §8.9.2/§8.9.3: surface sample groupings. Each
    // `sbgp` (SampleToGroupBox) becomes one `sbgp_<n>` key (0-based
    // encounter index) whose value is the grouping type, an optional
    // `param=<P>` (v1 grouping_type_parameter), then the run-length
    // `count:index` pairs (`group_description_index` 0 = "no group";
    // ≥ 0x10001 = movie-fragment-local index, kept verbatim). Each
    // `sgpd` (SampleGroupDescriptionBox) becomes one `sgpd_<n>` key
    // whose value is the grouping type, an optional `default=<D>`
    // (v2 default_sample_description_index), then the per-group entry
    // payloads rendered as lowercase hex (grouping-type-specific blobs
    // the container does not interpret). The two share `grouping_type`
    // so a caller can pair an `sbgp_<n>` with the `sgpd_<m>` of the
    // same type. Absent both boxes, none of the keys are emitted.
    let render_grouping_type = |gt: &[u8; 4]| -> String {
        std::str::from_utf8(gt)
            .ok()
            .filter(|s| s.chars().all(|c| !c.is_control()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{:02x}{:02x}{:02x}{:02x}", gt[0], gt[1], gt[2], gt[3]))
    };
    for (i, sb) in t.sbgp.iter().enumerate() {
        let mut v = render_grouping_type(&sb.grouping_type);
        if let Some(p) = sb.grouping_type_parameter {
            v.push_str(&format!(" param={}", p));
        }
        for (count, idx) in &sb.entries {
            v.push_str(&format!(" {}:{}", count, idx));
        }
        params.options.insert(format!("sbgp_{}", i), v);
    }
    for (i, sg) in t.sgpd.iter().enumerate() {
        let mut v = render_grouping_type(&sg.grouping_type);
        if let Some(d) = sg.default_sample_description_index {
            v.push_str(&format!(" default={}", d));
        }
        for entry in &sg.entries {
            v.push(' ');
            for byte in entry {
                v.push_str(&format!("{:02x}", byte));
            }
        }
        params.options.insert(format!("sgpd_{}", i), v);
    }

    // ISO/IEC 14496-12:2020 §8.9.5: surface CompactSampleToGroupBox
    // (`csgp`) instances. Each becomes one `csgp_<n>` key whose value is
    // the grouping type, an optional `param=<P>` (when the flag layout's
    // grouping_type_parameter_present bit is set), then one
    // space-separated token per pattern of the form
    // `count*idx0,idx1,...,idxN` — `count` is the pattern's
    // `sample_count` (how many times the index run replays) and the
    // comma-list is the per-sample `sample_group_description_index`
    // values of the pattern (0 = "no group"; fragment-local indices kept
    // verbatim with their high bit set). Shares `grouping_type` with the
    // matching `sgpd_<m>` exactly like `sbgp`. Absent `csgp`, no keys.
    for (i, cg) in t.csgp.iter().enumerate() {
        let mut v = render_grouping_type(&cg.grouping_type);
        if let Some(p) = cg.grouping_type_parameter {
            v.push_str(&format!(" param={}", p));
        }
        for pat in &cg.patterns {
            v.push(' ');
            v.push_str(&format!("{}*", pat.sample_count));
            let idx_list = pat
                .indices
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(",");
            v.push_str(&idx_list);
        }
        params.options.insert(format!("csgp_{}", i), v);
    }

    // ISO/IEC 14496-12 §8.6.4: surface the optional SampleDependencyTypeBox
    // (`sdtp`) as a small summary on `params.options` rather than
    // per-sample (per-sample would flood the map for a typical track).
    // Five keys per track-with-sdtp:
    //   * `sdtp_count` — total per-sample entries.
    //   * `sdtp_leading_count` — samples flagged as a leading sample
    //     (is_leading ∈ {1, 3} — value 1 = leading-with-prior-dependency,
    //     not decodable from this point; value 3 = leading without prior
    //     dependency, decodable).
    //   * `sdtp_independent_count` — samples with sample_depends_on = 2
    //     (an I-picture per dependency hint — does not depend on others).
    //   * `sdtp_disposable_count` — samples with sample_is_depended_on = 2
    //     (no other sample depends on this; safe to drop during fast
    //     forward).
    //   * `sdtp_redundant_count` — samples with sample_has_redundancy = 1
    //     (the sample carries redundant coding).
    // Absent `sdtp`, none of the keys are emitted.
    if !t.sdtp.is_empty() {
        let mut leading = 0u32;
        let mut independent = 0u32;
        let mut disposable = 0u32;
        let mut redundant = 0u32;
        for e in &t.sdtp {
            if e.is_leading == 1 || e.is_leading == 3 {
                leading += 1;
            }
            if e.sample_depends_on == 2 {
                independent += 1;
            }
            if e.sample_is_depended_on == 2 {
                disposable += 1;
            }
            if e.sample_has_redundancy == 1 {
                redundant += 1;
            }
        }
        params
            .options
            .insert("sdtp_count".to_string(), t.sdtp.len().to_string());
        params
            .options
            .insert("sdtp_leading_count".to_string(), leading.to_string());
        params.options.insert(
            "sdtp_independent_count".to_string(),
            independent.to_string(),
        );
        params
            .options
            .insert("sdtp_disposable_count".to_string(), disposable.to_string());
        params
            .options
            .insert("sdtp_redundant_count".to_string(), redundant.to_string());
    }

    // ISO/IEC 14496-12 §8.5.3: surface the optional DegradationPriorityBox
    // (`stdp`) as a small summary on `params.options` rather than the raw
    // per-sample table (a typical track has thousands of samples; the
    // whole `priority` array would dominate the options map). The spec
    // leaves the value semantics to derived specifications — we cannot
    // bucket priorities into named tiers at the container layer, so the
    // summary is the count plus min/max/sum so a consumer can recover the
    // mean and check the value spread without scanning. Four keys per
    // track-with-stdp:
    //   * `stdp_count` — total per-sample priority entries.
    //   * `stdp_min` — minimum `priority` value across the table (decimal).
    //   * `stdp_max` — maximum `priority` value across the table (decimal).
    //   * `stdp_sum` — sum of all `priority` values (u64 decimal — a u16
    //     priority × 2^32 samples still fits comfortably).
    // Absent `stdp`, none of the keys are emitted.
    if !t.stdp.is_empty() {
        let mut lo = u16::MAX;
        let mut hi = 0u16;
        let mut sum: u64 = 0;
        for &p in &t.stdp {
            if p < lo {
                lo = p;
            }
            if p > hi {
                hi = p;
            }
            sum += p as u64;
        }
        params
            .options
            .insert("stdp_count".to_string(), t.stdp.len().to_string());
        params
            .options
            .insert("stdp_min".to_string(), lo.to_string());
        params
            .options
            .insert("stdp_max".to_string(), hi.to_string());
        params
            .options
            .insert("stdp_sum".to_string(), sum.to_string());
    }

    // ISO/IEC 14496-12 §8.7.6: surface the optional PaddingBitsBox
    // (`padb`) as a small summary on `params.options` rather than the
    // raw per-sample table — a track with `padb` typically has every
    // sample's pad count packed densely and the raw nibble stream would
    // dominate the options map. Each pad count is bounded 0..=7 by
    // §8.7.6.3 so a fixed-width histogram fits in one short string;
    // keys emitted per track-with-padb:
    //   * `padb_count` — total sample entries in the parsed table.
    //   * `padb_max` — maximum pad count seen (0..=7).
    //   * `padb_nonzero_count` — number of samples whose pad count is
    //     non-zero (the count a bitstream consumer actually cares about
    //     — a track with all-zero pad counts is byte-aligned and the
    //     `padb` box is informational only).
    //   * `padb_hist` — eight-element histogram `n0:n1:n2:n3:n4:n5:n6:n7`
    //     where `nK` is the count of samples whose pad count is K
    //     (decimal). The colon-separated form keeps the key compact and
    //     lets a consumer recover the full per-bucket count without
    //     parsing variable-length JSON.
    // Absent `padb`, none of the keys are emitted.
    if !t.padb.is_empty() {
        let mut hist = [0u64; 8];
        let mut max_pad = 0u8;
        let mut nonzero: u64 = 0;
        for &p in &t.padb {
            let bucket = (p & 0x07) as usize;
            hist[bucket] = hist[bucket].saturating_add(1);
            if p > max_pad {
                max_pad = p;
            }
            if p != 0 {
                nonzero += 1;
            }
        }
        params
            .options
            .insert("padb_count".to_string(), t.padb.len().to_string());
        params
            .options
            .insert("padb_max".to_string(), max_pad.to_string());
        params
            .options
            .insert("padb_nonzero_count".to_string(), nonzero.to_string());
        params.options.insert(
            "padb_hist".to_string(),
            format!(
                "{}:{}:{}:{}:{}:{}:{}:{}",
                hist[0], hist[1], hist[2], hist[3], hist[4], hist[5], hist[6], hist[7]
            ),
        );
    }

    // ISO/IEC 14496-12 §8.7.7: surface each `subs` (SubSampleInformationBox)
    // present on the track. Each `subs` becomes one `subs_<n>` key whose
    // value starts with `"v<version> flags=<f>"` (the FullBox preamble —
    // version selects `subsample_size` width on disk; `flags` distinguishes
    // co-resident `subs` boxes per §8.7.7.1) followed by space-separated
    // per-sample blocks of the form
    //   `delta=<d>[:size,priority,discardable,csp[;size,priority,discardable,csp ...]]`
    // (the trailing colon and per-sub-sample list are omitted when the
    // entry has zero sub-samples). Decimal for `delta`, `size`,
    // `priority`, `discardable`; lowercase 8-digit hex for
    // `codec_specific_parameters` (it is a codec-defined u32 blob).
    //
    // Sub-sample semantics (e.g. which NAL unit a row indexes for H.264
    // per ISO/IEC 14496-15) belong to the codec layer; the container
    // surfaces the raw rows so a codec-aware consumer can decode them.
    // Absent `subs`, no keys are emitted.
    for (i, sb) in t.subs.iter().enumerate() {
        let mut v = format!("v{} flags={}", sb.version, sb.flags);
        for ent in &sb.entries {
            v.push_str(&format!(" delta={}", ent.sample_delta));
            if !ent.subsamples.is_empty() {
                v.push(':');
                let mut first = true;
                for s in &ent.subsamples {
                    if !first {
                        v.push(';');
                    }
                    first = false;
                    v.push_str(&format!(
                        "{},{},{},{:08x}",
                        s.subsample_size,
                        s.subsample_priority,
                        s.discardable,
                        s.codec_specific_parameters
                    ));
                }
            }
        }
        params.options.insert(format!("subs_{}", i), v);
    }

    // ISO/IEC 14496-12 §8.7.8 / §8.7.9 — surface each `saiz` /
    // `saio` (Sample Auxiliary Information Sizes / Offsets) found
    // inside `stbl` on `params.options`. Each `saiz` becomes one
    // `saiz_<n>` key (0-based encounter index); each `saio` becomes
    // one `saio_<n>` key. The pair is keyed by `(aux_info_type,
    // aux_info_type_parameter)` — a caller pairs an `saiz_<n>` with
    // the `saio_<m>` whose key matches. The most common consumer is
    // CENC: when `senc` is absent, the per-sample IV + subsample map
    // is carried in an auxiliary-information stream of type `cenc` /
    // `cbc1` / `cens` / `cbcs`, and the `saiz`+`saio` pair points to
    // the bytes in the mdat (ISO/IEC 23001-7 §7.3).
    //
    // Format conventions (decimal unless noted, mirroring the
    // sbgp/sgpd/subs surface):
    //
    //   saiz_<n> = "[type=<fourcc>] [param=<P>] default_size=<D> count=<N> [sizes=<s0>,<s1>,…]"
    //   saio_<n> = "v<version> [type=<fourcc>] [param=<P>] offsets=<o0>,<o1>,…"
    //
    // The `type=` / `param=` blocks are omitted when the FullBox
    // `flags & 1` bit was zero on disk (implied type is the scheme
    // type for protected content or the sample-entry FourCC
    // otherwise — see §8.7.8.3). The `sizes=` block is omitted when
    // `default_sample_info_size != 0` (every sample has size
    // `default_size`). Offsets are decimal; in `stbl` they are
    // absolute file positions.
    for (i, sz) in t.saiz.iter().enumerate() {
        let mut v = String::new();
        if let Some(t4) = &sz.aux_info_type {
            v.push_str(&format!(
                "type={} ",
                std::str::from_utf8(t4).unwrap_or("????")
            ));
        }
        if let Some(p) = sz.aux_info_type_parameter {
            v.push_str(&format!("param={p} "));
        }
        v.push_str(&format!(
            "default_size={} count={}",
            sz.default_sample_info_size, sz.sample_count
        ));
        if !sz.per_sample.is_empty() {
            v.push_str(" sizes=");
            let mut first = true;
            for s in &sz.per_sample {
                if !first {
                    v.push(',');
                }
                first = false;
                v.push_str(&format!("{s}"));
            }
        }
        params.options.insert(format!("saiz_{}", i), v);
    }
    for (i, so) in t.saio.iter().enumerate() {
        let mut v = format!("v{}", so.version);
        if let Some(t4) = &so.aux_info_type {
            v.push_str(&format!(
                " type={}",
                std::str::from_utf8(t4).unwrap_or("????")
            ));
        }
        if let Some(p) = so.aux_info_type_parameter {
            v.push_str(&format!(" param={p}"));
        }
        v.push_str(" offsets=");
        let mut first = true;
        for o in &so.offsets {
            if !first {
                v.push(',');
            }
            first = false;
            v.push_str(&format!("{o}"));
        }
        params.options.insert(format!("saio_{}", i), v);
    }

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
    /// Parsed `sidx` SegmentIndexBoxes; one per top-level sidx encountered.
    /// Consulted by `seek_to` as a secondary fast-path when no `tfra`
    /// covers the requested track: a `sidx` carries per-subsegment
    /// `(earliest_presentation_time, subsegment_duration, byte size)`
    /// triples (ISO/IEC 14496-12 §8.16.3), which lets us pick the
    /// subsegment whose decode-time range contains `pts` in O(log N)
    /// without expanding the moofs. The hit is converted to a file
    /// offset; the sample-list scan then snaps to the first keyframe
    /// at or after that offset, bounded by one subsegment.
    sidxes: Vec<SidxRecord>,
    /// Parsed `ssix` SubsegmentIndexBoxes (§8.16.4), in file order.
    /// Each maps levels (per the `leva` LevelAssignmentBox) to byte
    /// ranges of the subsegments indexed by the associated `sidx`.
    /// Surfaced as `ssix_<n>` flat-metadata summaries and through the
    /// public `Mp4Demuxer::ssixes` accessor; not consulted by
    /// `next_packet` / `seek_to` (we seek whole subsegments, not
    /// partial ones).
    #[allow(dead_code)]
    ssixes: Vec<SsixRecord>,
    /// Parsed `tfra` random-access tables, one per track that the file's
    /// `mfra` indexes. Each holds (presentation time, moof offset) pairs
    /// for keyframes — `seek_to` walks these to land directly on a moof
    /// boundary instead of scanning every sample.
    tfras: Vec<TfraRecord>,
    /// Parsed `prft` ProducerReferenceTimeBoxes (§8.16.5), in file
    /// order. Each is also surfaced as a `prft_<n>` flat-metadata entry;
    /// the structured list is kept for downstream tooling (low-latency
    /// DASH live edge tracking, wall-clock-to-pts mapping). Currently
    /// not consulted by `next_packet` / `seek_to`.
    #[allow(dead_code)]
    prfts: Vec<PrftRecord>,
    /// Parsed `pdin` ProgressiveDownloadInfoBox (§8.1.3), if the file
    /// carries one. Quantity is zero or one per file (§8.1.3.1); the
    /// structured record is reachable via the public
    /// `Mp4Demuxer::pdin_entries` accessor (downcast) and the flat
    /// metadata channel surfaces it as `pdin_count` + `pdin_<n>` keys.
    /// Informational only — the demuxer does not consult it.
    #[allow(dead_code)]
    pdin: Option<PdinRecord>,
    /// Parsed `leva` LevelAssignmentBox (§8.8.13), if the file carries
    /// one. Quantity is zero or one per file (§8.8.13.1); the
    /// structured record is reachable via the public
    /// `Mp4Demuxer::leva_entries` accessor (downcast) and the flat
    /// metadata channel surfaces it as `leva_count` + `leva_<n>` keys.
    /// Informational only — the demuxer does not consult it.
    #[allow(dead_code)]
    leva: Option<LevaRecord>,
    /// Parsed `trep` TrackExtensionPropertiesBoxes (§8.8.15), in file
    /// order. Quantity is zero or more (zero or one per track); each
    /// documents one track's characteristics in the subsequent movie
    /// fragments. The structured records are reachable via the public
    /// `Mp4Demuxer::treps` accessor (downcast) and the flat metadata
    /// channel surfaces them as `trep_<n>` keys. Informational only —
    /// the demuxer does not consult them.
    #[allow(dead_code)]
    treps: Vec<TrepRecord>,
    /// ISO/IEC 23001-7 §8.1 pssh entries collected from moov. One per
    /// DRM system signalled in the file; the demuxer does not consume
    /// them but a downstream decryption layer can look up its
    /// SystemID match here.
    #[allow(dead_code)]
    psshes: Vec<PsshBox>,
    /// ISO/IEC 23001-7 §8.1 pssh entries collected from inside
    /// individual `moof` boxes. Each record is keyed by the enclosing
    /// `mfhd.sequence_number` so a decrypting layer can scope SystemID
    /// matches to the right fragment per §8.1.1 ("readers SHALL
    /// examine all Protection System Specific Header boxes in the
    /// Movie Box and in the Movie Fragment Box associated with the
    /// sample (but not those in other Movie Fragment Boxes)"). Empty
    /// in non-fragmented files and in fragmented files that signal
    /// protection only at moov level (the common case).
    #[allow(dead_code)]
    moof_psshes: Vec<MoofPsshRecord>,
    /// ISO/IEC 23001-7 §7.2 senc records, one per `traf` that carried
    /// a SampleEncryptionBox. Each holds the parsed per-sample IVs
    /// and (when `UseSubSampleEncryption` was set) the subsample map,
    /// keyed by `(track_idx, moof_sequence)` for downstream
    /// reassembly. Empty in non-CENC files (the common case).
    #[allow(dead_code)]
    senc_records: Vec<SencRecord>,
    /// ISO/IEC 14496-12 §8.7.8–9 per-fragment auxiliary-information
    /// records. One per `traf` that carried at least one `saiz` or
    /// `saio` box, keyed by `(track_idx, moof_sequence)`. Empty in
    /// files that don't use auxiliary-information offsets (the common
    /// case — most CENC files use `senc` instead, and unprotected
    /// files have no aux-info at all).
    #[allow(dead_code)]
    sai_records: Vec<SaiRecord>,
    /// Per-track media timescale (1-based parallel to `track_ids`),
    /// needed to translate `tfra.time` (track timescale) to the seek-to
    /// caller's pts (also track timescale, but this lets us add unit
    /// asserts later if they diverge).
    #[allow(dead_code)]
    movie_timescale: u32,
    track_timescales: Vec<u32>,
    track_ids: Vec<u32>,
}

impl Mp4Demuxer {
    /// ISO/IEC 23001-7 §8.1 — all `pssh`
    /// ProtectionSystemSpecificHeaderBox entries discovered at moov
    /// level, in file order. Each is keyed by a 16-byte SystemID UUID;
    /// a downstream DRM layer that matches the SystemID consumes the
    /// `data` blob (and v1 `kids` list). Empty for unprotected files.
    ///
    /// `pssh` entries inside individual `moof` boxes are surfaced via
    /// the parallel [`Mp4Demuxer::moof_psshes`] accessor, keyed by the
    /// enclosing `mfhd.sequence_number` per §8.1.1.
    #[allow(dead_code)]
    pub fn psshes(&self) -> &[PsshBox] {
        &self.psshes
    }

    /// ISO/IEC 23001-7 §8.1 — `pssh` entries collected from inside
    /// individual `moof` boxes, in moof-walk order. Each record is
    /// keyed by the enclosing `mfhd.sequence_number` (the
    /// `MoofPsshRecord::moof_sequence` field) so a decrypting layer
    /// can scope SystemID lookups to the right fragment per the
    /// §8.1.1 normative-reader rule: "examine all Protection System
    /// Specific Header boxes in the Movie Box and in the Movie
    /// Fragment Box associated with the sample (but not those in
    /// other Movie Fragment Boxes)". Empty for non-fragmented files
    /// and for fragmented files that signal protection only at moov
    /// level (the common case).
    #[allow(dead_code)]
    pub fn moof_psshes(&self) -> &[MoofPsshRecord] {
        &self.moof_psshes
    }

    /// ISO/IEC 23001-7 §7.2 — per-fragment `senc`
    /// SampleEncryptionBox records collected from every `traf` whose
    /// matching track carried a `tenc` default (so the per-sample IV
    /// width was recoverable). Indexed in moof-encounter order; the
    /// `track_idx` field references this demuxer's `streams()` list,
    /// and `moof_sequence` is the `mfhd.sequence_number` of the
    /// containing movie fragment. Empty for unfragmented files and
    /// for fragmented files without per-fragment encryption metadata.
    #[allow(dead_code)]
    pub fn senc_records(&self) -> &[SencRecord] {
        &self.senc_records
    }

    /// ISO/IEC 14496-12 §8.1.3 — `pdin` ProgressiveDownloadInfoBox
    /// pairs, if the file carries one. Each entry is a
    /// `(rate, initial_delay)` u32 pair recommending an initial
    /// playback delay (milliseconds) for a given effective download
    /// rate (bytes/second); a receiver picks adjacent entries that
    /// bracket its observed throughput and linearly interpolates the
    /// delay (or extrapolates from the first / last entry).
    /// Returns `None` when the file has no `pdin` box; returns
    /// `Some(&[])` for an explicitly empty box (a producer's way of
    /// saying "no hints available"). The structured record is
    /// surfaced separately from the flat `pdin_count` / `pdin_<n>`
    /// metadata keys so tooling can choose the shape it prefers.
    #[allow(dead_code)]
    pub fn pdin_entries(&self) -> Option<&[PdinEntry]> {
        self.pdin.as_ref().map(|p| p.entries.as_slice())
    }

    /// ISO/IEC 14496-12 §8.8.13 — `leva` LevelAssignmentBox entries,
    /// if the file carries one. Each entry maps one fragment level to
    /// a `track_id` (plus a `padding_flag` and an `assignment_type`
    /// that discriminates among sample-group / track / track-subsegment
    /// / sub-track variants per §8.8.13.2). Returns `None` when the
    /// file has no `leva` box; returns `Some(&[])` for an explicit
    /// `level_count = 0` (the spec pins a minimum of 2 in §8.8.13.3
    /// but the demuxer carries whatever the producer wrote so a
    /// validator can flag a short table). The structured record is
    /// surfaced separately from the flat `leva_count` / `leva_<n>`
    /// metadata keys so tooling can choose the shape it prefers.
    #[allow(dead_code)]
    pub fn leva_entries(&self) -> Option<&[LevaEntry]> {
        self.leva.as_ref().map(|l| l.entries.as_slice())
    }

    /// ISO/IEC 14496-12 §8.8.15 — all `trep`
    /// TrackExtensionPropertiesBoxes discovered inside `mvex`, in file
    /// order. Each names the `track_id` it describes and carries the
    /// type + payload length of any child boxes it nests (e.g. an
    /// `assp` Alternative Startup Sequence Properties Box, §8.8.16).
    /// Empty for files without `trep` boxes (which is most of the
    /// corpus — `trep` is a fragmented-movie hint). The flat metadata
    /// channel carries `trep_<n>` summaries; this accessor returns the
    /// structured records.
    #[allow(dead_code)]
    pub fn treps(&self) -> &[TrepRecord] {
        &self.treps
    }

    /// ISO/IEC 14496-12 §8.16.4 — all `ssix` SubsegmentIndexBoxes
    /// discovered during the top-level walk, in file order. Each maps
    /// levels (per the `leva` LevelAssignmentBox, §8.8.13 — see
    /// [`Mp4Demuxer::leva_entries`]) to byte ranges of the subsegments
    /// indexed by the `sidx` the ssix follows. Empty for files without
    /// subsegment indexes (which is most of the corpus). The flat
    /// metadata channel carries `ssix_<n>` shape summaries; this
    /// accessor returns the structured table.
    #[allow(dead_code)]
    pub fn ssixes(&self) -> &[SsixRecord] {
        &self.ssixes
    }

    /// ISO/IEC 14496-12 §8.7.8–9 — per-fragment Sample Auxiliary
    /// Information records. One per `traf` that carried at least one
    /// `saiz` or `saio` box; the `track_idx` references this
    /// demuxer's `streams()` list and `moof_sequence` is the
    /// containing `mfhd.sequence_number`. The auxiliary-information
    /// bytes themselves stay in the mdat at the offsets the `saio`
    /// names (base_data_offset relative inside a traf per §8.8.14) —
    /// the demuxer doesn't fetch or interpret them; their semantics
    /// belong to the carried `aux_info_type` (e.g. CENC `cenc` /
    /// `cbc1` / `cens` / `cbcs` per ISO/IEC 23001-7 §7.3, or any
    /// other registered aux-info type). Empty for files without
    /// auxiliary-info offsets (which is most of the corpus).
    #[allow(dead_code)]
    pub fn sai_records(&self) -> &[SaiRecord] {
        &self.sai_records
    }
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

        // Optional fast-path: if we have a `tfra` random-access index
        // for this track, use it to locate the moof byte offset of the
        // last keyframe at-or-before `pts`. We then convert the
        // `moof_offset` back to a sample index by looking up the first
        // sample of this track whose `offset >= moof_offset`. This is
        // O(log N) on the tfra (binary search by time) + O(N) on the
        // sample list (linear scan from the moof boundary), where the
        // sample list scan is bounded by one fragment's worth of
        // samples — far better than the full O(N) walk below.
        //
        // The tfra entries' `time` field is in this track's media
        // timescale, same as the caller's `pts`.
        if let Some(target) = self.tfra_seek_target(stream_index, pts) {
            // Find the cursor: first sample of this track whose offset
            // matches the moof's first sample. The moof's samples land
            // in mdat, which starts after the moof box; the first one
            // belongs to this track (single-track sidx) or the first
            // traf for this track.
            for (i, s) in self.samples.iter().enumerate() {
                if s.track_idx != stream_index {
                    continue;
                }
                if s.offset >= target.moof_offset && s.keyframe {
                    self.cursor = i;
                    return Ok(s.pts);
                }
            }
            // Fall through to the linear scan if the tfra entry didn't
            // resolve cleanly (e.g. mdat layout the file lied about).
        }

        // Secondary fast-path: if `tfra` didn't apply (no mfra in the
        // file, or none of the file's tfras index this track), but the
        // file carries a `sidx` (typical for DASH on-demand profile,
        // §8.16.3), use it to find the subsegment whose decode-time
        // range contains `pts` and snap to the first keyframe at or
        // after that subsegment's byte offset. The sample-list scan is
        // bounded by one subsegment's samples.
        if let Some(target_offset) = self.sidx_seek_target(stream_index, pts) {
            for (i, s) in self.samples.iter().enumerate() {
                if s.track_idx != stream_index {
                    continue;
                }
                if s.offset >= target_offset && s.keyframe {
                    self.cursor = i;
                    return Ok(s.pts);
                }
            }
            // Fall through to the linear scan if no keyframe lands at
            // or after the sidx-pointed offset (corrupt sidx, etc.).
        }

        // Default linear scan: find the last keyframe of this stream
        // with pts <= target.
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

impl Mp4Demuxer {
    /// Look up the latest tfra entry for `stream_index` whose `time`
    /// is `<= pts`. Returns the `TfraEntry` or `None` when no tfra
    /// covers this track or no entry is at-or-before `pts`.
    fn tfra_seek_target(&self, stream_index: u32, pts: i64) -> Option<TfraEntry> {
        if self.tfras.is_empty() {
            return None;
        }
        let track_id = self.track_ids.get(stream_index as usize)?;
        let tfra = self.tfras.iter().find(|t| t.track_id == *track_id)?;
        if tfra.entries.is_empty() {
            return None;
        }
        let _ts = self.track_timescales.get(stream_index as usize)?;
        // tfra entries are sorted by `time` per spec. Binary search for
        // the largest `time <= pts`. We treat negative pts as "before
        // any keyframe" → return the first entry.
        if pts < 0 {
            return Some(tfra.entries[0]);
        }
        let target = pts as u64;
        match tfra.entries.binary_search_by_key(&target, |e| e.time) {
            Ok(i) => Some(tfra.entries[i]),
            Err(i) => {
                if i == 0 {
                    Some(tfra.entries[0])
                } else {
                    Some(tfra.entries[i - 1])
                }
            }
        }
    }

    /// Look up the byte offset of the subsegment that covers `pts` for
    /// this track, using parsed `sidx` (ISO/IEC 14496-12 §8.16.3) records.
    ///
    /// Two on-the-wire shapes are handled, both spec-conformant:
    ///
    /// 1. **Single `sidx` indexing N subsegments** (DASH on-demand
    ///    profile). One sidx record at the start of the file lists
    ///    every fragment's `(EPT, duration, size)`. We binary-search
    ///    that table by decode time.
    ///
    /// 2. **One `sidx` per fragment, each indexing one subsegment**
    ///    (DASH live profile / our own muxer's output). Each sidx
    ///    immediately precedes a `(styp? + moof + mdat)` and reports
    ///    `EPT` = that fragment's `tfdt`. We pick the latest sidx
    ///    whose EPT ≤ `pts`.
    ///
    /// Both shapes converge on the same return: a byte offset the
    /// `seek_to` caller can match against `SampleRef::offset` to find
    /// the first keyframe at-or-after that fragment's first byte. The
    /// sample-list scan that follows is bounded by one subsegment.
    ///
    /// Hierarchical (nested) sidx references are skipped — we keep
    /// walking media references only. Returns `None` when no sidx
    /// covers this track, or no reference is at or before `pts`.
    fn sidx_seek_target(&self, stream_index: u32, pts: i64) -> Option<u64> {
        if self.sidxes.is_empty() {
            return None;
        }
        let track_id = *self.track_ids.get(stream_index as usize)?;
        let track_ts = *self.track_timescales.get(stream_index as usize)? as u64;
        if track_ts == 0 {
            return None;
        }
        let pts_u = if pts < 0 { 0u64 } else { pts as u64 };

        // Walk every sidx whose reference_id matches the track. For
        // each one, expand its reference list into virtual
        // `(time_in_sidx_scale, byte_offset)` anchors and remember the
        // latest anchor whose time ≤ pts (translated into sidx scale).
        let mut best: Option<u64> = None;
        let mut first_media_offset: Option<u64> = None;
        for sidx in self.sidxes.iter().filter(|s| s.reference_id == track_id) {
            if sidx.references.is_empty() || sidx.timescale == 0 {
                continue;
            }
            let target_sidx_time = if track_ts == sidx.timescale as u64 {
                pts_u
            } else {
                // Multiply with u128 to avoid overflow on long durations.
                ((pts_u as u128) * (sidx.timescale as u128) / (track_ts as u128)) as u64
            };
            let mut cur_time = sidx.earliest_presentation_time;
            let mut cur_offset = sidx.first_byte_offset;
            for r in &sidx.references {
                if r.is_sidx {
                    // Hierarchical sidx: consumes byte range, no
                    // media-time anchor to land on.
                    cur_offset = cur_offset.saturating_add(r.referenced_size as u64);
                    continue;
                }
                if first_media_offset.is_none() {
                    first_media_offset = Some(cur_offset);
                }
                if cur_time <= target_sidx_time {
                    // Later anchors override earlier ones (we're picking
                    // the LAST subsegment whose start ≤ pts).
                    best = Some(cur_offset);
                }
                cur_time = cur_time.saturating_add(r.subsegment_duration as u64);
                cur_offset = cur_offset.saturating_add(r.referenced_size as u64);
            }
        }
        // If pts predates every sidx-indexed subsegment, snap to the
        // first media reference's offset so the caller still lands on
        // a keyframe boundary rather than falling through to the slow
        // path.
        best.or(first_media_offset)
    }
}

// --- Public parser entry points (tests + diagnostic tools) ---------------

/// Parse a standalone `sidx` (SegmentIndexBox, §8.16.3) body from
/// `body` (the bytes after the 8/16-byte box header). `sidx_end_offset`
/// is the absolute file offset of the first byte AFTER the sidx box;
/// it anchors `first_offset` per spec.
///
/// Exposed as a public entry point so tooling can read DASH segment
/// indexes without re-running `open()`.
pub fn parse_sidx_box(body: &[u8], sidx_end_offset: u64) -> Result<Option<SidxRecord>> {
    parse_sidx(body, sidx_end_offset)
}

/// Parse a standalone `ssix` (SubsegmentIndexBox, §8.16.4) body from
/// `body` (the bytes after the 8/16-byte box header).
///
/// Exposed as a public entry point so DASH tooling can map levels to
/// partial-subsegment byte ranges without re-running `open()` — the
/// ssix sits immediately after the `sidx` it documents (§8.16.4.1), so
/// a segment fetcher that already walked to the sidx has the ssix
/// bytes in hand.
///
/// Returns `Err` for a body shorter than the 8-byte floor (4-byte
/// FullBox preamble + `subsegment_count`), for a version byte other
/// than 0 (the only defined layout), and for any subsegment or range
/// list that outruns the body. A `subsegment_count` of zero yields an
/// empty record.
pub fn parse_ssix_box(body: &[u8]) -> Result<SsixRecord> {
    parse_ssix(body)
}

/// Serialise a [`SsixRecord`] into a complete `ssix` box — 8-byte
/// header (`[size:u32]['ssix']`) plus the §8.16.4.2 v0 body — for
/// segment-index emitters that pair it with a `sidx`.
///
/// The caller owns the §8.16.4.1 conformance constraints the record
/// type cannot express: the box must immediately follow the `sidx` it
/// documents, `subsegments.len()` must equal that sidx's
/// `reference_count`, every byte of each subsegment must be covered
/// (ranges partition the subsegment), and each subsegment should carry
/// at least two ranges. `range_size` values are 24-bit on the wire;
/// values above 0xFF_FFFF are rejected rather than silently masked.
pub fn build_ssix_box(record: &SsixRecord) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(
        8 + record
            .subsegments
            .iter()
            .map(|s| 4 + 4 * s.ranges.len())
            .sum::<usize>(),
    );
    body.extend_from_slice(&[0u8; 4]); // FullBox(version = 0, flags = 0)
    body.extend_from_slice(&(record.subsegments.len() as u32).to_be_bytes());
    for s in &record.subsegments {
        body.extend_from_slice(&(s.ranges.len() as u32).to_be_bytes());
        for r in &s.ranges {
            if r.range_size > 0xFF_FFFF {
                return Err(Error::invalid("MP4: ssix range_size exceeds 24 bits"));
            }
            let b = r.range_size.to_be_bytes();
            body.extend_from_slice(&[r.level, b[1], b[2], b[3]]);
        }
    }
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"ssix");
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a standalone `mfra` (MovieFragmentRandomAccessBox, §8.8.10)
/// body, returning the per-track `tfra` tables (one per track that the
/// file's mfra indexes).
pub fn parse_mfra_box(body: &[u8]) -> Result<Vec<TfraRecord>> {
    let mut out = Vec::new();
    parse_mfra(body, &mut out)?;
    Ok(out)
}

/// Parse a standalone `prft` (ProducerReferenceTimeBox, §8.16.5) body
/// from `body` (the bytes after the 8/16-byte box header).
///
/// Exposed as a public entry point so low-latency DASH / CMAF tooling
/// can correlate wall-clock NTP timestamps with media decoding time
/// for one reference track without re-running `open()`.
///
/// Returns `Err` for a truncated body (less than 16 bytes for v0 or
/// 20 bytes for v1). Multiple `prft` boxes may appear in one file
/// (each annotates the following moof per §8.16.5.1), in which case
/// callers should walk the top-level box list themselves and call
/// this entry per occurrence.
pub fn parse_prft_box(body: &[u8]) -> Result<Option<PrftRecord>> {
    parse_prft(body)
}

/// Parse a standalone `pdin` (ProgressiveDownloadInfoBox, ISO/IEC
/// 14496-12 §8.1.3) body from `body` (the bytes after the 8/16-byte
/// box header).
///
/// Exposed as a public entry point so tooling that already has the
/// box's payload bytes in hand (a DASH packager, a manifest emitter,
/// a fixture validator) can recover the `(rate, initial_delay)` pairs
/// without re-running `open()`. Returns `Err` for a body shorter than
/// the 4-byte FullBox preamble or whose post-preamble length is not a
/// multiple of 8 bytes (one `(rate, initial_delay)` u32 pair per
/// entry); returns `Ok(PdinRecord { entries: vec![] })` for an
/// explicitly empty box (preamble only, zero pairs).
pub fn parse_pdin_box(body: &[u8]) -> Result<PdinRecord> {
    parse_pdin(body)
}

/// Parse a standalone `leva` (LevelAssignmentBox, ISO/IEC 14496-12
/// §8.8.13) body from `body` (the bytes after the 8/16-byte box
/// header).
///
/// Exposed as a public entry point so tooling that already has the
/// box's payload bytes in hand (a DASH packager, a manifest emitter, a
/// fragment-level extractor) can recover the per-level table without
/// re-running `open()`. Returns `Err` for a body shorter than the
/// 4-byte FullBox preamble, a body missing its `level_count` byte, or
/// any truncated per-entry tail; returns
/// `Ok(LevaRecord { entries: vec![] })` for a body with the preamble +
/// `level_count = 0`.
pub fn parse_leva_box(body: &[u8]) -> Result<LevaRecord> {
    parse_leva(body)
}

/// Parse a standalone `trep` (TrackExtensionPropertiesBox, ISO/IEC
/// 14496-12 §8.8.15) body from `body` (the bytes after the 8/16-byte
/// box header).
///
/// Exposed as a public entry point so tooling that already has the
/// box's payload bytes in hand can recover the `track_id` and the
/// type/length list of nested child boxes without re-running `open()`.
/// Returns `Err` for a body shorter than the 4-byte FullBox preamble or
/// one missing its 4-byte `track_id`; returns
/// `Ok(TrepRecord { children: vec![], .. })` for a `trep` carrying just
/// its `track_id` with no child boxes.
pub fn parse_trep_box(body: &[u8]) -> Result<TrepRecord> {
    parse_trep(body)
}

use std::io::Read;

fn read_bytes_vec<R: Read + ?Sized>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    // `n` derives from a box size field that is attacker-controlled
    // (`size32` is up to 4 GiB, `largesize` up to 2^64 − 1). Don't
    // pre-allocate against an unverified declared length; let
    // `Read::take` cap the read at whatever the input can deliver and
    // grow the buffer to match, then surface a truncation as
    // `Error::invalid`.
    let mut buf = Vec::new();
    r.take(n as u64).read_to_end(&mut buf)?;
    if buf.len() != n {
        return Err(Error::invalid("MP4: truncated box payload"));
    }
    Ok(buf)
}

/// Advance an in-memory `Cursor` by `psz` bytes, clamped at the
/// cursor's end. The intra-box walkers in this module repeatedly do
/// `cur.set_position(cur.position() + psz)` to skip an unknown box's
/// payload, where `psz` derives from an attacker-controlled box-size
/// field. A raw `+` panics on overflow when `psz` is u32::MAX-class
/// and the cursor is already several gigabytes in; a saturating clamp
/// to the buffer end keeps the loop bounded and lets the surrounding
/// `while cur.position() < end` terminate cleanly on the next
/// iteration without dereferencing past the buffer.
fn skip_cursor_bytes<T: AsRef<[u8]>>(cur: &mut std::io::Cursor<T>, psz: usize) {
    let end = cur.get_ref().as_ref().len() as u64;
    let pos = cur.position();
    let next = pos.saturating_add(psz as u64).min(end);
    cur.set_position(next);
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
            protection_scheme: None,
            tenc: None,
            tref: Vec::new(),
            trgr: Vec::new(),
            elng: None,
            kinds: Vec::new(),
            copyrights: Vec::new(),
            tsel: None,
            sub_tracks: Vec::new(),
            cslg: None,
            vmhd: None,
            stsh: Vec::new(),
            sbgp: Vec::new(),
            sgpd: Vec::new(),
            csgp: Vec::new(),
            sdtp: Vec::new(),
            stdp: Vec::new(),
            padb: Vec::new(),
            subs: Vec::new(),
            saiz: Vec::new(),
            saio: Vec::new(),
            stsd_entries: Vec::new(),
        }
    }

    /// Build an `enca`-wrapped audio sample entry: the 28-byte audio
    /// preamble (channels 2, 16-bit, 48 kHz) followed by a `sinf` box
    /// carrying `frma` (original FourCC) + `schm` (scheme type). This
    /// mirrors what a CENC-protected AAC track looks like on disk
    /// (ISO/IEC 14496-12 §8.12).
    fn build_enca_with_sinf(original: &[u8; 4], scheme: &[u8; 4]) -> Vec<u8> {
        // Preamble: 28 bytes.
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 6]);
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&16u16.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&((48_000u32) << 16).to_be_bytes());

        // Build sinf body: frma + schm.
        let mut sinf_body = Vec::new();
        // frma: 8 byte header + 4 byte data_format.
        sinf_body.extend_from_slice(&12u32.to_be_bytes());
        sinf_body.extend_from_slice(b"frma");
        sinf_body.extend_from_slice(original);
        // schm: 8 byte header + 4 byte FullBox version/flags + 4 byte
        // scheme_type + 4 byte scheme_version.
        sinf_body.extend_from_slice(&20u32.to_be_bytes());
        sinf_body.extend_from_slice(b"schm");
        sinf_body.extend_from_slice(&[0u8; 4]); // version + flags
        sinf_body.extend_from_slice(scheme);
        sinf_body.extend_from_slice(&[0u8; 4]); // scheme_version

        // sinf box header + body.
        let sinf_total = (8 + sinf_body.len()) as u32;
        out.extend_from_slice(&sinf_total.to_be_bytes());
        out.extend_from_slice(b"sinf");
        out.extend_from_slice(&sinf_body);
        out
    }

    #[test]
    fn enca_sample_entry_recovers_original_format() {
        // CENC-style: outer FourCC `enca`, original `mp4a` (AAC), scheme
        // `cenc`. Drive the full parse_stsd pathway: build an stsd
        // payload with entry_count=1 and one `enca` entry containing
        // the audio preamble + sinf.
        let entry_body = build_enca_with_sinf(b"mp4a", b"cenc");
        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]); // FullBox version/flags
        stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        let total = (8 + entry_body.len()) as u32;
        stsd.extend_from_slice(&total.to_be_bytes());
        stsd.extend_from_slice(b"enca");
        stsd.extend_from_slice(&entry_body);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();
        // Original FourCC recovered from sinf/frma.
        assert_eq!(&t.codec_id_fourcc, b"mp4a");
        // Scheme type from sinf/schm.
        assert_eq!(t.protection_scheme, Some(*b"cenc"));
        // Preamble still parsed (channels populated despite the enc wrap).
        assert_eq!(t.channels, Some(2));
    }

    #[test]
    fn encv_sample_entry_recovers_h264_original() {
        // Video variant: 78-byte preamble + sinf with frma=avc1, schm=cbcs.
        let mut entry_body = Vec::new();
        entry_body.extend_from_slice(&[0u8; 6]);
        entry_body.extend_from_slice(&1u16.to_be_bytes());
        entry_body.extend_from_slice(&[0u8; 16]); // pre_defined/reserved
        entry_body.extend_from_slice(&1280u16.to_be_bytes()); // width
        entry_body.extend_from_slice(&720u16.to_be_bytes()); // height
        entry_body.extend_from_slice(&((72u32) << 16).to_be_bytes()); // horizresolution
        entry_body.extend_from_slice(&((72u32) << 16).to_be_bytes()); // vertresolution
        entry_body.extend_from_slice(&[0u8; 4]); // reserved
        entry_body.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        entry_body.extend_from_slice(&[0u8; 32]); // compressorname
        entry_body.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        entry_body.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
                                                              // Total preamble: 78 bytes.
        assert_eq!(entry_body.len(), 78);

        let mut sinf_body = Vec::new();
        sinf_body.extend_from_slice(&12u32.to_be_bytes());
        sinf_body.extend_from_slice(b"frma");
        sinf_body.extend_from_slice(b"avc1");
        sinf_body.extend_from_slice(&20u32.to_be_bytes());
        sinf_body.extend_from_slice(b"schm");
        sinf_body.extend_from_slice(&[0u8; 4]);
        sinf_body.extend_from_slice(b"cbcs");
        sinf_body.extend_from_slice(&[0u8; 4]);

        let sinf_total = (8 + sinf_body.len()) as u32;
        entry_body.extend_from_slice(&sinf_total.to_be_bytes());
        entry_body.extend_from_slice(b"sinf");
        entry_body.extend_from_slice(&sinf_body);

        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        let total = (8 + entry_body.len()) as u32;
        stsd.extend_from_slice(&total.to_be_bytes());
        stsd.extend_from_slice(b"encv");
        stsd.extend_from_slice(&entry_body);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_stsd(&stsd, &mut t).unwrap();
        assert_eq!(&t.codec_id_fourcc, b"avc1");
        assert_eq!(t.protection_scheme, Some(*b"cbcs"));
        assert_eq!(t.width, Some(1280));
        assert_eq!(t.height, Some(720));
    }

    /// Build a minimal audio `SampleEntry` body (no codec config children):
    /// the §8.5.2.2 8-byte preamble (`reserved[6]` + `data_reference_index`)
    /// followed by the 20-byte AudioSampleEntry v0 tail. `dref` sets the
    /// `data_reference_index`. The returned bytes are the *body* (no box
    /// header).
    fn audio_sample_entry_body(channels: u16, dref: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 6]); // reserved
        out.extend_from_slice(&dref.to_be_bytes()); // data_reference_index
        out.extend_from_slice(&[0u8; 8]); // reserved (2 × u32)
        out.extend_from_slice(&channels.to_be_bytes()); // channelcount
        out.extend_from_slice(&16u16.to_be_bytes()); // samplesize
        out.extend_from_slice(&[0u8; 4]); // pre_defined + reserved
        out.extend_from_slice(&((48_000u32) << 16).to_be_bytes()); // samplerate
        out
    }

    /// Wrap a SampleEntry body in its box header (size + FourCC).
    fn boxed_sample_entry(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let total = (8 + body.len()) as u32;
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(body);
        out
    }

    /// Assemble a `stsd` body (FullBox header + entry_count + entries).
    fn build_stsd(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 4]); // version + flags
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for e in entries {
            out.extend_from_slice(e);
        }
        out
    }

    #[test]
    fn stsd_records_all_entries_first_drives_active_codec() {
        // Three sample descriptions: mp4a (active), then ac-3 and a
        // hex-rendered non-printable FourCC. ISO/IEC 14496-12 §8.5.2 —
        // entry [0] is the active description, [1..] are alternates.
        let e0 = boxed_sample_entry(b"mp4a", &audio_sample_entry_body(2, 1));
        let e1 = boxed_sample_entry(b"ac-3", &audio_sample_entry_body(6, 1));
        let e2 = boxed_sample_entry(&[0x00, 0x01, 0x02, 0x03], &audio_sample_entry_body(1, 2));
        let stsd = build_stsd(&[e0, e1, e2]);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();

        // Active codec is entry [0].
        assert_eq!(&t.codec_id_fourcc, b"mp4a");
        // Active entry's preamble still drives the audio fields.
        assert_eq!(t.channels, Some(2));

        // All three entries recorded in order with their FourCC + dref.
        assert_eq!(t.stsd_entries.len(), 3);
        assert_eq!(&t.stsd_entries[0].format, b"mp4a");
        assert_eq!(t.stsd_entries[0].data_reference_index, 1);
        assert_eq!(&t.stsd_entries[1].format, b"ac-3");
        assert_eq!(t.stsd_entries[1].data_reference_index, 1);
        assert_eq!(t.stsd_entries[2].format, [0x00, 0x01, 0x02, 0x03]);
        assert_eq!(t.stsd_entries[2].data_reference_index, 2);
    }

    #[test]
    fn build_stream_info_surfaces_multi_stsd_on_options() {
        let e0 = boxed_sample_entry(b"mp4a", &audio_sample_entry_body(2, 1));
        let e1 = boxed_sample_entry(b"ac-3", &audio_sample_entry_body(6, 3));
        let stsd = build_stsd(&[e0, e1]);

        let mut t = fresh_track();
        t.timescale = 48_000;
        super::parse_stsd(&stsd, &mut t).unwrap();

        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        // entry_count surfaced.
        assert_eq!(info.params.options.get("stsd_count"), Some("2"));
        // 1-based keys matching the spec's sample_description_index, with
        // the data_reference_index per entry.
        assert_eq!(info.params.options.get("stsd_1"), Some("mp4a dref=1"));
        assert_eq!(info.params.options.get("stsd_2"), Some("ac-3 dref=3"));
        // No phantom third key.
        assert_eq!(info.params.options.get("stsd_3"), None);
    }

    #[test]
    fn build_stream_info_single_stsd_emits_no_keys() {
        // The overwhelmingly common single-description track surfaces no
        // stsd_* keys — the active codec already carries that info.
        let e0 = boxed_sample_entry(b"mp4a", &audio_sample_entry_body(2, 1));
        let stsd = build_stsd(&[e0]);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();
        assert_eq!(t.stsd_entries.len(), 1);

        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stsd_count"), None);
        assert_eq!(info.params.options.get("stsd_1"), None);
    }

    #[test]
    fn stsd_overcount_stops_at_present_entries() {
        // A forged entry_count larger than the bytes present: parse stops
        // at what it could read rather than inventing entries or erroring.
        let e0 = boxed_sample_entry(b"mp4a", &audio_sample_entry_body(2, 1));
        let e1 = boxed_sample_entry(b"ac-3", &audio_sample_entry_body(6, 1));
        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]); // version + flags
        stsd.extend_from_slice(&9u32.to_be_bytes()); // lie: claim 9 entries
        stsd.extend_from_slice(&e0);
        stsd.extend_from_slice(&e1);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();
        // Only the two real entries are recorded.
        assert_eq!(t.stsd_entries.len(), 2);
        assert_eq!(&t.codec_id_fourcc, b"mp4a");
    }

    /// Build a `tenc` box (ISO/IEC 23001-7 §8.2) including its 8-byte
    /// box header (size + `tenc` fourcc).
    fn build_tenc_box_v0(kid: &[u8; 16], iv_size: u8) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version=0 + flags
        body.push(0); // reserved
        body.push(0); // reserved
        body.push(1); // default_isProtected
        body.push(iv_size);
        body.extend_from_slice(kid);
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(b"tenc");
        out.extend_from_slice(&body);
        out
    }

    fn build_tenc_box_v1_constant_iv(
        kid: &[u8; 16],
        crypt: u8,
        skip: u8,
        const_iv: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version=1 + flags
        body.push(0); // reserved
        body.push((crypt << 4) | (skip & 0x0F));
        body.push(1); // default_isProtected
        body.push(0); // default_Per_Sample_IV_Size = 0 ⇒ constant IV
        body.extend_from_slice(kid);
        body.push(const_iv.len() as u8);
        body.extend_from_slice(const_iv);
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(b"tenc");
        out.extend_from_slice(&body);
        out
    }

    /// Build a `schi` box containing the given child box bytes
    /// (already framed with their own 8-byte header).
    fn build_schi_box(child: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + child.len());
        out.extend_from_slice(&((8 + child.len()) as u32).to_be_bytes());
        out.extend_from_slice(b"schi");
        out.extend_from_slice(child);
        out
    }

    /// Build a sinf body with frma + schm + schi (where schi contains
    /// `schi_child` — typically a `tenc`). Mirrors the on-disk shape
    /// for a CENC-protected sample entry's protection scheme info.
    fn build_sinf_body_with_schi(
        original: &[u8; 4],
        scheme: &[u8; 4],
        schi_child: &[u8],
    ) -> Vec<u8> {
        let mut sinf_body = Vec::new();
        // frma
        sinf_body.extend_from_slice(&12u32.to_be_bytes());
        sinf_body.extend_from_slice(b"frma");
        sinf_body.extend_from_slice(original);
        // schm
        sinf_body.extend_from_slice(&20u32.to_be_bytes());
        sinf_body.extend_from_slice(b"schm");
        sinf_body.extend_from_slice(&[0u8; 4]);
        sinf_body.extend_from_slice(scheme);
        sinf_body.extend_from_slice(&[0u8; 4]);
        // schi (containing tenc)
        let schi = build_schi_box(schi_child);
        sinf_body.extend_from_slice(&schi);
        sinf_body
    }

    fn build_enca_with_full_sinf(original: &[u8; 4], scheme: &[u8; 4], tenc_box: &[u8]) -> Vec<u8> {
        // 28-byte audio preamble.
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 6]);
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&16u16.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&((48_000u32) << 16).to_be_bytes());
        let sinf_body = build_sinf_body_with_schi(original, scheme, tenc_box);
        let sinf_total = (8 + sinf_body.len()) as u32;
        out.extend_from_slice(&sinf_total.to_be_bytes());
        out.extend_from_slice(b"sinf");
        out.extend_from_slice(&sinf_body);
        out
    }

    #[test]
    fn enca_sample_entry_with_sinf_schi_tenc_populates_track_tenc_v0() {
        // Outer FourCC `enca`, original `mp4a`, scheme `cenc`, with a
        // v0 tenc carrying a 16-byte IV and a synthetic KID.
        let kid: [u8; 16] = [
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ];
        let tenc_box = build_tenc_box_v0(&kid, 16);
        let entry_body = build_enca_with_full_sinf(b"mp4a", b"cenc", &tenc_box);
        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        let total = (8 + entry_body.len()) as u32;
        stsd.extend_from_slice(&total.to_be_bytes());
        stsd.extend_from_slice(b"enca");
        stsd.extend_from_slice(&entry_body);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();
        assert_eq!(&t.codec_id_fourcc, b"mp4a");
        assert_eq!(t.protection_scheme, Some(*b"cenc"));
        let tenc = t.tenc.expect("tenc parsed from sinf/schi/tenc");
        assert_eq!(tenc.version, 0);
        assert_eq!(tenc.default_is_protected, 1);
        assert_eq!(tenc.default_per_sample_iv_size, 16);
        assert_eq!(tenc.default_kid, kid);
        assert!(tenc.default_constant_iv.is_none());
    }

    #[test]
    fn encv_sample_entry_v1_pattern_with_constant_iv_lands_on_track() {
        // Build a v1 tenc with pattern 1:9 and an 8-byte constant IV,
        // wrap it in schi/sinf and an `encv` sample entry with scheme
        // `cbcs` (the typical CBC-S + pattern-encryption configuration).
        let kid = [0x01u8; 16];
        let civ = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        let tenc_box = build_tenc_box_v1_constant_iv(&kid, 1, 9, &civ);

        // 78-byte video preamble.
        let mut entry_body = Vec::new();
        entry_body.extend_from_slice(&[0u8; 6]);
        entry_body.extend_from_slice(&1u16.to_be_bytes());
        entry_body.extend_from_slice(&[0u8; 16]);
        entry_body.extend_from_slice(&1280u16.to_be_bytes());
        entry_body.extend_from_slice(&720u16.to_be_bytes());
        entry_body.extend_from_slice(&((72u32) << 16).to_be_bytes());
        entry_body.extend_from_slice(&((72u32) << 16).to_be_bytes());
        entry_body.extend_from_slice(&[0u8; 4]);
        entry_body.extend_from_slice(&1u16.to_be_bytes());
        entry_body.extend_from_slice(&[0u8; 32]);
        entry_body.extend_from_slice(&0x0018u16.to_be_bytes());
        entry_body.extend_from_slice(&(-1i16).to_be_bytes());
        assert_eq!(entry_body.len(), 78);

        let sinf_body = build_sinf_body_with_schi(b"avc1", b"cbcs", &tenc_box);
        let sinf_total = (8 + sinf_body.len()) as u32;
        entry_body.extend_from_slice(&sinf_total.to_be_bytes());
        entry_body.extend_from_slice(b"sinf");
        entry_body.extend_from_slice(&sinf_body);

        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        let total = (8 + entry_body.len()) as u32;
        stsd.extend_from_slice(&total.to_be_bytes());
        stsd.extend_from_slice(b"encv");
        stsd.extend_from_slice(&entry_body);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_stsd(&stsd, &mut t).unwrap();
        assert_eq!(&t.codec_id_fourcc, b"avc1");
        assert_eq!(t.protection_scheme, Some(*b"cbcs"));
        let tenc = t.tenc.expect("tenc parsed from sinf/schi/tenc");
        assert_eq!(tenc.version, 1);
        assert_eq!(tenc.default_crypt_byte_block, 1);
        assert_eq!(tenc.default_skip_byte_block, 9);
        assert_eq!(tenc.default_per_sample_iv_size, 0);
        assert_eq!(tenc.default_constant_iv.as_deref(), Some(&civ[..]));
    }

    #[test]
    fn enca_sample_entry_without_tenc_leaves_track_tenc_none() {
        // Original frma+schm shape (no schi/tenc) — must still parse,
        // and `track.tenc` stays None.
        let entry_body = build_enca_with_sinf(b"mp4a", b"cenc");
        let mut stsd = Vec::new();
        stsd.extend_from_slice(&[0u8; 4]);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        let total = (8 + entry_body.len()) as u32;
        stsd.extend_from_slice(&total.to_be_bytes());
        stsd.extend_from_slice(b"enca");
        stsd.extend_from_slice(&entry_body);

        let mut t = fresh_track();
        super::parse_stsd(&stsd, &mut t).unwrap();
        assert_eq!(&t.codec_id_fourcc, b"mp4a");
        assert_eq!(t.protection_scheme, Some(*b"cenc"));
        assert!(t.tenc.is_none());
    }

    #[test]
    fn subtitle_handler_dispatches_subtitle_media_type() {
        // hdlr body layout: 4 bytes FullBox + 4 bytes pre_defined +
        // 4 bytes handler_type + 12 bytes reserved + name (null-term).
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&[0u8; 4]); // pre_defined
        body.extend_from_slice(b"subt"); // handler_type
        body.extend_from_slice(&[0u8; 12]); // reserved
        body.extend_from_slice(b"\0");

        let mut t = fresh_track();
        super::parse_hdlr(&body, &mut t).unwrap();
        assert_eq!(t.media_type, oxideav_core::MediaType::Subtitle);

        // And `text` (timed text) also lands on Subtitle.
        let mut body2 = Vec::new();
        body2.extend_from_slice(&[0u8; 4]);
        body2.extend_from_slice(&[0u8; 4]);
        body2.extend_from_slice(b"text");
        body2.extend_from_slice(&[0u8; 12]);
        body2.extend_from_slice(b"\0");

        let mut t2 = fresh_track();
        super::parse_hdlr(&body2, &mut t2).unwrap();
        assert_eq!(t2.media_type, oxideav_core::MediaType::Subtitle);

        // And `sbtl` (QuickTime subtitle handler) also lands on Subtitle.
        let mut body3 = Vec::new();
        body3.extend_from_slice(&[0u8; 4]);
        body3.extend_from_slice(&[0u8; 4]);
        body3.extend_from_slice(b"sbtl");
        body3.extend_from_slice(&[0u8; 12]);
        body3.extend_from_slice(b"\0");

        let mut t3 = fresh_track();
        super::parse_hdlr(&body3, &mut t3).unwrap();
        assert_eq!(t3.media_type, oxideav_core::MediaType::Subtitle);
    }

    #[test]
    fn tx3g_sample_entry_preserves_payload_as_extradata() {
        // tx3g (3GPP TS 26.245) sample entry layout starts with a
        // 6-byte reserved + 2-byte data_reference_index preamble, then
        // 18 bytes of display flags / colours / default text box /
        // default style record. We don't decode the 18 bytes (no
        // 26.245 in docs/) but we preserve them so renderers can.
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0u8; 6]);
        entry.extend_from_slice(&1u16.to_be_bytes());
        // 18 bytes of "tx3g header" — arbitrary recognisable pattern.
        let tx3g_header = [
            0x00, 0x00, 0x00, 0x00, // display_flags
            0x00, 0x00, // horizontal/vertical justification
            0xFF, 0xFF, 0xFF, 0xFF, // background_color_rgba (white)
            0x00, 0x00, 0x00, 0x00, // default text box (top/left)
            0x00, 0x10, 0x00, 0x10, // default text box (bot/right)
        ];
        entry.extend_from_slice(&tx3g_header);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"tx3g";
        super::parse_subtitle_sample_entry(&entry, &mut t).unwrap();
        // Post-preamble bytes preserved verbatim.
        assert_eq!(t.extradata, tx3g_header);
    }

    #[test]
    fn stpp_sample_entry_preserves_xml_namespace_strings() {
        // stpp body: namespace\0schema_location\0auxiliary_mime_types\0
        // — UTF-8 null-terminated strings. We don't split them; we
        // hand the whole post-preamble blob to extradata so the caller
        // can `split('\0')`.
        let mut entry = Vec::new();
        entry.extend_from_slice(&[0u8; 6]);
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(b"http://www.w3.org/ns/ttml\0\0\0");

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"stpp";
        super::parse_subtitle_sample_entry(&entry, &mut t).unwrap();
        assert!(t.extradata.starts_with(b"http://www.w3.org/ns/ttml"));
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

    /// §8.3.3 — `tref` carrying a single `chap` reference resolves to one
    /// child box of body `[u32 track_ID]`. The reference type is the
    /// child's FourCC; the body is a packed big-endian u32 array.
    #[test]
    fn parse_tref_chap_single_id() {
        // Inner TrackReferenceTypeBox: size(12) "chap" track_ID(3)
        let inner = wrap_box_full_size(b"chap", &3u32.to_be_bytes());
        // Outer `tref` body is just the concatenation of inner boxes.
        let mut t = fresh_track();
        super::parse_tref(&inner, &mut t).unwrap();
        assert_eq!(t.tref.len(), 1);
        assert_eq!(&t.tref[0].0, b"chap");
        assert_eq!(t.tref[0].1, vec![3]);
    }

    /// Multiple typed references in one `tref` are surfaced in order.
    #[test]
    fn parse_tref_multiple_types() {
        let mut body = Vec::new();
        // subt → tracks 4, 5
        let mut subt_payload = Vec::new();
        subt_payload.extend_from_slice(&4u32.to_be_bytes());
        subt_payload.extend_from_slice(&5u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"subt", &subt_payload));
        // cdsc → track 2
        body.extend(wrap_box_full_size(b"cdsc", &2u32.to_be_bytes()));
        let mut t = fresh_track();
        super::parse_tref(&body, &mut t).unwrap();
        assert_eq!(t.tref.len(), 2);
        assert_eq!(&t.tref[0].0, b"subt");
        assert_eq!(t.tref[0].1, vec![4, 5]);
        assert_eq!(&t.tref[1].0, b"cdsc");
        assert_eq!(t.tref[1].1, vec![2]);
    }

    /// §8.3.3.3 — `track_ID` is never zero; we silently drop zeros from
    /// the array rather than rejecting the file.
    #[test]
    fn parse_tref_drops_zero_track_ids() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&7u32.to_be_bytes());
        let body = wrap_box_full_size(b"font", &payload);
        let mut t = fresh_track();
        super::parse_tref(&body, &mut t).unwrap();
        assert_eq!(t.tref.len(), 1);
        assert_eq!(t.tref[0].1, vec![7]);
    }

    /// Sub-box body that isn't a multiple of 4 bytes is a structural
    /// error — we surface it as `Error::Invalid` rather than silently
    /// padding or truncating.
    #[test]
    fn parse_tref_misaligned_child_rejected() {
        let body = wrap_box_full_size(b"hint", &[0u8, 0, 0, 1, 0xFF]); // 5 bytes
        let mut t = fresh_track();
        let err = super::parse_tref(&body, &mut t).unwrap_err();
        assert!(matches!(err, oxideav_core::Error::InvalidData(_)));
    }

    /// Spec says at most one TrackReferenceTypeBox per reference_type
    /// inside a `tref`. If a malformed file repeats one, the first wins
    /// and subsequent duplicates are ignored.
    #[test]
    fn parse_tref_duplicate_type_keeps_first() {
        let mut body = Vec::new();
        body.extend(wrap_box_full_size(b"subt", &1u32.to_be_bytes()));
        body.extend(wrap_box_full_size(b"subt", &2u32.to_be_bytes()));
        let mut t = fresh_track();
        super::parse_tref(&body, &mut t).unwrap();
        assert_eq!(t.tref.len(), 1);
        assert_eq!(t.tref[0].1, vec![1]);
    }

    /// Empty outer `tref` (no inner boxes) parses cleanly and yields no
    /// references.
    #[test]
    fn parse_tref_empty_body_is_ok() {
        let mut t = fresh_track();
        super::parse_tref(&[], &mut t).unwrap();
        assert!(t.tref.is_empty());
    }

    /// Track references surface on `StreamInfo.params.options` as
    /// `tref_<type>` keys whose value is a space-separated track-ID
    /// list. Reference types with no IDs (after dropping zeros) are
    /// suppressed.
    #[test]
    fn build_stream_info_surfaces_tref_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"tx3g";
        t.timescale = 1000;
        t.tref.push((*b"subt", vec![10, 11]));
        t.tref.push((*b"font", vec![20]));
        t.tref.push((*b"hint", vec![])); // empty after-zero strip: suppressed.
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tref_subt"), Some("10 11"));
        assert_eq!(info.params.options.get("tref_font"), Some("20"));
        assert_eq!(info.params.options.get("tref_hint"), None);
    }

    /// §8.3.4 — `trgr` carrying a single `msrc` TrackGroupTypeBox child
    /// produces one `(track_group_type, track_group_id)` entry on the
    /// track. The FullBox preamble is 4 bytes of zeros (version + 24-bit
    /// flags, both spec-pinned to 0), followed by the 32-bit
    /// `track_group_id`.
    #[test]
    fn parse_trgr_msrc_single_child() {
        // Inner TrackGroupTypeBox: size(16) "msrc" [v+flags=0] track_group_id=42
        let mut inner_body = Vec::new();
        inner_body.extend_from_slice(&[0u8; 4]); // FullBox v0 + flags=0
        inner_body.extend_from_slice(&42u32.to_be_bytes());
        let body = wrap_box_full_size(b"msrc", &inner_body);
        let mut t = fresh_track();
        super::parse_trgr(&body, &mut t).unwrap();
        assert_eq!(t.trgr.len(), 1);
        assert_eq!(&t.trgr[0].0, b"msrc");
        assert_eq!(t.trgr[0].1, 42);
    }

    /// Multiple typed children inside one `trgr` are surfaced in
    /// encounter order. Different types coexist: `msrc` (spec-named) plus
    /// a hypothetical derived-spec FourCC.
    #[test]
    fn parse_trgr_multiple_types() {
        let mut body = Vec::new();
        let mut msrc_payload = Vec::new();
        msrc_payload.extend_from_slice(&[0u8; 4]); // FullBox
        msrc_payload.extend_from_slice(&7u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"msrc", &msrc_payload));
        let mut foo_payload = Vec::new();
        foo_payload.extend_from_slice(&[0u8; 4]); // FullBox
        foo_payload.extend_from_slice(&99u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"foo ", &foo_payload));
        let mut t = fresh_track();
        super::parse_trgr(&body, &mut t).unwrap();
        assert_eq!(t.trgr.len(), 2);
        assert_eq!(&t.trgr[0].0, b"msrc");
        assert_eq!(t.trgr[0].1, 7);
        assert_eq!(&t.trgr[1].0, b"foo ");
        assert_eq!(t.trgr[1].1, 99);
    }

    /// Spec leaves the door open for two children of the same
    /// `track_group_type` on the same track; we preserve both rather
    /// than de-duplicating (unlike `tref` where the spec explicitly
    /// caps at one entry per type).
    #[test]
    fn parse_trgr_repeated_type_kept_in_order() {
        let mut body = Vec::new();
        let mut p1 = Vec::new();
        p1.extend_from_slice(&[0u8; 4]);
        p1.extend_from_slice(&1u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"msrc", &p1));
        let mut p2 = Vec::new();
        p2.extend_from_slice(&[0u8; 4]);
        p2.extend_from_slice(&2u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"msrc", &p2));
        let mut t = fresh_track();
        super::parse_trgr(&body, &mut t).unwrap();
        assert_eq!(t.trgr.len(), 2);
        assert_eq!(t.trgr[0].1, 1);
        assert_eq!(t.trgr[1].1, 2);
    }

    /// A child whose body is shorter than the 8-byte preamble + id is a
    /// structural error. We surface it as `Error::InvalidData` rather
    /// than silently skipping (which would mask corruption).
    #[test]
    fn parse_trgr_short_child_rejected() {
        // 7-byte body: FullBox preamble + only 3 bytes where the id
        // should go.
        let body = wrap_box_full_size(b"msrc", &[0u8, 0, 0, 0, 0, 0, 0]);
        let mut t = fresh_track();
        let err = super::parse_trgr(&body, &mut t).unwrap_err();
        assert!(matches!(err, oxideav_core::Error::InvalidData(_)));
    }

    /// §8.3.4.2 pins `version = 0`. A non-zero version is a
    /// forward-compatible extension we cannot decode safely — skip the
    /// child rather than mis-parsing it. The remaining children of the
    /// same `trgr` continue to be processed.
    #[test]
    fn parse_trgr_skips_unknown_version_child() {
        let mut body = Vec::new();
        // Child 1: version = 1, skip.
        let mut p1 = Vec::new();
        p1.extend_from_slice(&[1u8, 0, 0, 0]); // version = 1
        p1.extend_from_slice(&5u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"msrc", &p1));
        // Child 2: version = 0, kept.
        let mut p2 = Vec::new();
        p2.extend_from_slice(&[0u8; 4]);
        p2.extend_from_slice(&8u32.to_be_bytes());
        body.extend(wrap_box_full_size(b"msrc", &p2));
        let mut t = fresh_track();
        super::parse_trgr(&body, &mut t).unwrap();
        assert_eq!(t.trgr.len(), 1);
        assert_eq!(t.trgr[0].1, 8);
    }

    /// Trailing bytes beyond `track_group_id` are reserved for derived
    /// specs. We ignore them at this layer — the file is still valid and
    /// the id is still recovered.
    #[test]
    fn parse_trgr_ignores_trailing_extension_bytes() {
        let mut inner_body = Vec::new();
        inner_body.extend_from_slice(&[0u8; 4]); // FullBox
        inner_body.extend_from_slice(&13u32.to_be_bytes());
        inner_body.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // extension
        let body = wrap_box_full_size(b"msrc", &inner_body);
        let mut t = fresh_track();
        super::parse_trgr(&body, &mut t).unwrap();
        assert_eq!(t.trgr.len(), 1);
        assert_eq!(t.trgr[0].1, 13);
    }

    /// Empty outer `trgr` (no inner boxes) parses cleanly and yields no
    /// entries.
    #[test]
    fn parse_trgr_empty_body_is_ok() {
        let mut t = fresh_track();
        super::parse_trgr(&[], &mut t).unwrap();
        assert!(t.trgr.is_empty());
    }

    /// `parse_trak` accepts a `trgr` box nested under `trak` and lands
    /// its child entries on the resulting Track. Confirms the routing
    /// glue inside `parse_trak`'s box match (not just the standalone
    /// parser).
    #[test]
    fn parse_trak_picks_up_nested_trgr() {
        // Build a minimal trak: tkhd + mdia (mdhd + hdlr + minf/stbl
        // pre-baked elsewhere is not needed — parse_trak only requires
        // an mdia to consider the track "has_media", and parse_mdia
        // walks its own children). Mirror the kind-nested test setup.
        let mut tkhd = vec![0u8; 92]; // FullBox v0 + 84 bytes
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes()); // track_id = 1

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // timescale

        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 8]); // FullBox + pre_defined
        hdlr.extend_from_slice(b"vide"); // handler_type
        hdlr.extend_from_slice(&[0u8; 12]); // reserved[3]
        hdlr.extend_from_slice(b"\0"); // name (empty C string)

        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));

        // trgr containing one msrc child with track_group_id = 555.
        let mut trgr_body = Vec::new();
        let mut msrc_body = Vec::new();
        msrc_body.extend_from_slice(&[0u8; 4]); // FullBox
        msrc_body.extend_from_slice(&555u32.to_be_bytes());
        trgr_body.extend(wrap_box_full_size(b"msrc", &msrc_body));

        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        trak.extend(wrap_box_full_size(b"trgr", &trgr_body));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.trgr.len(), 1);
        assert_eq!(&t.trgr[0].0, b"msrc");
        assert_eq!(t.trgr[0].1, 555);
    }

    /// Track groups surface on `StreamInfo.params.options` as
    /// `trgr_<n>` keys whose value is `"<type> <id>"`. Mirrors the
    /// `kind_<n>` two-field convention.
    #[test]
    fn build_stream_info_surfaces_trgr_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90_000;
        t.trgr.push((*b"msrc", 7));
        t.trgr.push((*b"msrc", 42)); // second of same type still surfaced.
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("trgr_0"), Some("msrc 7"));
        assert_eq!(info.params.options.get("trgr_1"), Some("msrc 42"));
    }

    /// `elng` (ExtendedLanguageBox §8.4.6): FullBox preamble + a
    /// NULL-terminated BCP 47 tag. The parsed tag (without the NUL) is
    /// stored on the track.
    #[test]
    fn parse_elng_reads_bcp47_tag() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox version+flags
        body.extend_from_slice(b"en-US\0"); // NULL-terminated tag
        let mut t = fresh_track();
        super::parse_elng(&body, &mut t);
        assert_eq!(t.elng.as_deref(), Some("en-US"));
    }

    /// A tag with region+script subtags (`zh-Hant-HK`) round-trips, and a
    /// tag without an explicit NUL terminator (technically malformed but
    /// tolerated) still parses.
    #[test]
    fn parse_elng_subtags_and_missing_nul() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"zh-Hant-HK\0");
        let mut t = fresh_track();
        super::parse_elng(&body, &mut t);
        assert_eq!(t.elng.as_deref(), Some("zh-Hant-HK"));

        // No trailing NUL: read to end of body.
        let mut body2 = Vec::new();
        body2.extend_from_slice(&[0u8; 4]);
        body2.extend_from_slice(b"fr-FR");
        let mut t2 = fresh_track();
        super::parse_elng(&body2, &mut t2);
        assert_eq!(t2.elng.as_deref(), Some("fr-FR"));
    }

    /// A too-short or empty-string `elng` leaves the track with no
    /// extended language and never panics.
    #[test]
    fn parse_elng_malformed_is_silently_skipped() {
        // Only 3 bytes — shorter than the FullBox preamble.
        let mut t = fresh_track();
        super::parse_elng(&[0, 0, 0], &mut t);
        assert_eq!(t.elng, None);

        // FullBox preamble then an immediate NUL (empty tag).
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.push(0);
        let mut t2 = fresh_track();
        super::parse_elng(&body, &mut t2);
        assert_eq!(t2.elng, None);
    }

    /// The extended language tag surfaces on
    /// `StreamInfo.params.options["language"]`.
    #[test]
    fn build_stream_info_surfaces_elng_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        t.elng = Some("de-DE".to_string());
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("language"), Some("de-DE"));
    }

    /// No `elng` box → no `language` option (caller falls back to mdhd).
    #[test]
    fn build_stream_info_no_elng_no_language_option() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("language"), None);
    }

    /// End-to-end: an `elng` box nested inside `mdia` is picked up by
    /// `parse_mdia` and lands on the track.
    #[test]
    fn parse_mdia_picks_up_nested_elng() {
        // Minimal mdhd (v0, 24 bytes) so timescale is set, then elng.
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 4]); // version+flags
        mdhd.extend_from_slice(&[0u8; 8]); // creation+modification time
        mdhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
        mdhd.extend_from_slice(&[0u8; 4]); // language + pre_defined

        let mut elng = Vec::new();
        elng.extend_from_slice(&[0u8; 4]);
        elng.extend_from_slice(b"es-419\0");

        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"elng", &elng));

        let mut t = fresh_track();
        super::parse_mdia(&mdia, &mut t).unwrap();
        assert_eq!(t.timescale, 1000);
        assert_eq!(t.elng.as_deref(), Some("es-419"));
    }

    /// Build a self-contained Box (size + FourCC + payload, 32-bit
    /// size field). Used as a building block for stuffing typed
    /// reference children into a synthetic `tref` body.
    fn wrap_box_full_size(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    /// §8.10.4 — `kind` (KindBox): FullBox preamble + NULL-terminated
    /// schemeURI + NULL-terminated value. When the value is empty
    /// (URI alone identifies the kind) the body is `<FullBox><URI>\0\0`.
    #[test]
    fn parse_kind_uri_only() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox version+flags
        body.extend_from_slice(b"urn:mpeg:dash:role:2011\0");
        body.extend_from_slice(b"\0"); // empty value
        let mut t = fresh_track();
        super::parse_kind(&body, &mut t);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].0, "urn:mpeg:dash:role:2011");
        assert_eq!(t.kinds[0].1, "");
    }

    /// A kind with both URI and value: the URI identifies the naming
    /// scheme and the value is the kind name within that scheme. The
    /// canonical DASH example: scheme = `urn:mpeg:dash:role:2011`,
    /// value = `main`.
    #[test]
    fn parse_kind_uri_and_value() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"urn:mpeg:dash:role:2011\0");
        body.extend_from_slice(b"main\0");
        let mut t = fresh_track();
        super::parse_kind(&body, &mut t);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].0, "urn:mpeg:dash:role:2011");
        assert_eq!(t.kinds[0].1, "main");
    }

    /// A box that lacks the trailing value NUL is tolerated — the
    /// value string is taken up to the end of the box. Matches the
    /// `parse_elng` posture for optional-string boxes.
    #[test]
    fn parse_kind_missing_value_nul_tolerated() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"urn:example:role\0");
        body.extend_from_slice(b"alt"); // no trailing NUL
        let mut t = fresh_track();
        super::parse_kind(&body, &mut t);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].0, "urn:example:role");
        assert_eq!(t.kinds[0].1, "alt");
    }

    /// Empty URI (just two NULs) yields no kind — a kind with no URI
    /// has nothing to identify.
    #[test]
    fn parse_kind_empty_uri_dropped() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"\0\0");
        let mut t = fresh_track();
        super::parse_kind(&body, &mut t);
        assert!(t.kinds.is_empty());
    }

    /// A too-short body (smaller than the FullBox preamble) is
    /// silently skipped — the box is informational and a malformed
    /// entry should never abort the demux.
    #[test]
    fn parse_kind_too_short_is_silently_skipped() {
        let mut t = fresh_track();
        super::parse_kind(&[0, 0, 0], &mut t);
        assert!(t.kinds.is_empty());
    }

    /// Track-level `udta` may carry multiple `kind` boxes from
    /// different naming schemes — the spec note allows it ("two
    /// schemes that both define a kind that indicates sub-titles").
    /// Each lands as a distinct entry, preserved in encounter order.
    #[test]
    fn parse_track_udta_collects_multiple_kinds() {
        // Two kinds: a DASH role and a custom scheme.
        let mut k1 = Vec::new();
        k1.extend_from_slice(&[0u8; 4]);
        k1.extend_from_slice(b"urn:mpeg:dash:role:2011\0");
        k1.extend_from_slice(b"caption\0");
        let mut k2 = Vec::new();
        k2.extend_from_slice(&[0u8; 4]);
        k2.extend_from_slice(b"urn:apple:hap:subtitles\0\0");

        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"kind", &k1));
        udta.extend(wrap_box_full_size(b"kind", &k2));

        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert_eq!(t.kinds.len(), 2);
        assert_eq!(t.kinds[0].0, "urn:mpeg:dash:role:2011");
        assert_eq!(t.kinds[0].1, "caption");
        assert_eq!(t.kinds[1].0, "urn:apple:hap:subtitles");
        assert_eq!(t.kinds[1].1, "");
    }

    /// Non-`kind` children inside a track-level `udta` are skipped
    /// (file-wide metadata is the moov-level `udta` parser's job).
    #[test]
    fn parse_track_udta_ignores_non_kind_children() {
        // A `titl` (3GPP title) inside a track-level udta: not a kind,
        // shouldn't crash, and shouldn't populate the kinds list.
        let mut titl = Vec::new();
        titl.extend_from_slice(&[0u8; 6]); // FullBox + 2-byte lang
        titl.extend_from_slice(b"My Track\0");
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"titl", &titl));

        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert!(t.kinds.is_empty());
    }

    /// Surfacing: each parsed kind shows up as `kind_<n>` on
    /// `params.options`. URI-only kinds emit just the URI; URI+value
    /// kinds emit `"URI value"` (space-separated, mirroring the
    /// `tref_<type>` value convention).
    #[test]
    fn build_stream_info_surfaces_kinds_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"wvtt";
        t.timescale = 1000;
        t.kinds
            .push(("urn:mpeg:dash:role:2011".to_string(), "caption".to_string()));
        t.kinds
            .push(("urn:apple:hap:subtitles".to_string(), String::new()));
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("kind_0"),
            Some("urn:mpeg:dash:role:2011 caption")
        );
        assert_eq!(
            info.params.options.get("kind_1"),
            Some("urn:apple:hap:subtitles")
        );
        assert_eq!(info.params.options.get("kind_2"), None);
    }

    /// A track with no `kind` boxes surfaces no `kind_*` options.
    #[test]
    fn build_stream_info_no_kind_no_kind_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("kind_0"), None);
    }

    /// End-to-end: a `kind` box nested inside `trak/udta` is picked up
    /// by `parse_trak` and lands on the track.
    #[test]
    fn parse_trak_picks_up_nested_kind() {
        // Minimal tkhd (v0): FullBox (4) + created (4) + modified (4) +
        // track_ID (4) + reserved (4) + duration (4) + reserved (8) +
        // layer (2) + alt_group (2) + volume (2) + reserved (2) +
        // matrix (36) + width (4) + height (4) = 84 bytes.
        let mut tkhd = vec![0u8; 84];
        // Set track_ID at offset 4+4+4 = 12.
        tkhd[12..16].copy_from_slice(&7u32.to_be_bytes());
        // Minimal mdhd (v0, 24 bytes).
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 4]); // version+flags
        mdhd.extend_from_slice(&[0u8; 8]); // created+modified
        mdhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
        mdhd.extend_from_slice(&[0u8; 4]); // language + pre_defined
                                           // Minimal hdlr identifying a video track.
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 4]); // FullBox
        hdlr.extend_from_slice(&[0u8; 4]); // pre_defined
        hdlr.extend_from_slice(b"vide"); // handler_type
        hdlr.extend_from_slice(&[0u8; 12]); // reserved
        hdlr.push(0); // name (empty C string)
                      // Wrap the mdhd + hdlr into a minimal mdia.
        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));
        // Minimal `kind` box: URI = "urn:mpeg:dash:role:2011", value = "main".
        let mut kind = Vec::new();
        kind.extend_from_slice(&[0u8; 4]);
        kind.extend_from_slice(b"urn:mpeg:dash:role:2011\0");
        kind.extend_from_slice(b"main\0");
        // Wrap kind in a track-level udta.
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"kind", &kind));
        // Assemble the trak body.
        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        trak.extend(wrap_box_full_size(b"udta", &udta));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.track_id, 7);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].0, "urn:mpeg:dash:role:2011");
        assert_eq!(t.kinds[0].1, "main");
    }

    /// Builds the 16-bit packed language word of an ISO/IEC 14496-12
    /// §8.10.2.3 `cprt` box from a 3-letter ASCII lowercase tag.
    /// Each character contributes 5 bits of `(c - 0x60)` and the
    /// padding bit at position 15 is 0.
    fn pack_lang(tag: &[u8; 3]) -> [u8; 2] {
        let c0 = (tag[0] - 0x60) as u16;
        let c1 = (tag[1] - 0x60) as u16;
        let c2 = (tag[2] - 0x60) as u16;
        let packed = (c0 << 10) | (c1 << 5) | c2;
        packed.to_be_bytes()
    }

    /// §8.10.2 — `cprt` (CopyrightBox) ASCII case: FullBox preamble +
    /// packed `eng` language + NULL-terminated UTF-8 notice. The decoded
    /// record carries the 3-letter language tag and the notice without
    /// the trailing NUL.
    #[test]
    fn parse_cprt_ascii_eng_notice() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox version 0 + flags 0
        body.extend_from_slice(&pack_lang(b"eng"));
        body.extend_from_slice(b"(c) 2026 Example\0");
        let mut t = fresh_track();
        super::parse_cprt(&body, &mut t);
        assert_eq!(t.copyrights.len(), 1);
        assert_eq!(&t.copyrights[0].language, b"eng");
        assert_eq!(t.copyrights[0].notice, "(c) 2026 Example");
    }

    /// §8.10.2 — `cprt` with a UTF-16BE notice (the spec's
    /// "if UTF-16 is used, the string shall start with the BYTE ORDER
    /// MARK (0xFEFF)" path). The BOM is consumed and the notice is
    /// decoded to a native String — the UTF-16 NUL-terminator pair at
    /// the end is also stripped.
    #[test]
    fn parse_cprt_utf16be_notice_with_bom() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&pack_lang(b"jpn"));
        // BOM + "あ" (U+3042) + NULL terminator (U+0000).
        body.extend_from_slice(&[0xFE, 0xFF]);
        body.extend_from_slice(&[0x30, 0x42]);
        body.extend_from_slice(&[0x00, 0x00]);
        let mut t = fresh_track();
        super::parse_cprt(&body, &mut t);
        assert_eq!(t.copyrights.len(), 1);
        assert_eq!(&t.copyrights[0].language, b"jpn");
        assert_eq!(t.copyrights[0].notice, "あ");
    }

    /// A `cprt` box with a non-zero FullBox version is unknown to the
    /// §8.10.2.2 syntax (which pins it to 0); the box is silently
    /// dropped — a forward-incompatible layout should not abort the
    /// demux nor inject an entry with potentially-wrong byte offsets.
    #[test]
    fn parse_cprt_unknown_version_is_dropped() {
        let mut body = Vec::new();
        body.push(1); // version = 1 (not defined by §8.10.2.2)
        body.extend_from_slice(&[0u8; 3]); // flags
        body.extend_from_slice(&pack_lang(b"eng"));
        body.extend_from_slice(b"ignored\0");
        let mut t = fresh_track();
        super::parse_cprt(&body, &mut t);
        assert!(t.copyrights.is_empty());
    }

    /// A `cprt` body shorter than the FullBox + language word (6 bytes)
    /// is silently dropped — the box is informational and a malformed
    /// entry should never abort the demux (mirrors `parse_kind`'s
    /// posture).
    #[test]
    fn parse_cprt_too_short_is_silently_dropped() {
        let mut t = fresh_track();
        super::parse_cprt(&[0, 0, 0, 0, 0], &mut t);
        assert!(t.copyrights.is_empty());
    }

    /// A `cprt` with an empty notice (just the FullBox + language +
    /// terminator) is preserved as an empty-string entry. The decoded
    /// language tag is still meaningful — "the track declares an empty
    /// notice for this language" is not the same as the absence of
    /// any `cprt` box.
    #[test]
    fn parse_cprt_empty_notice_preserves_language() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&pack_lang(b"fra"));
        body.push(0); // notice is just the terminator
        let mut t = fresh_track();
        super::parse_cprt(&body, &mut t);
        assert_eq!(t.copyrights.len(), 1);
        assert_eq!(&t.copyrights[0].language, b"fra");
        assert_eq!(t.copyrights[0].notice, "");
    }

    /// Multiple `cprt` boxes inside the same track-level `udta` —
    /// §8.10.2.1's "Quantity: Zero or more" — each lands as a distinct
    /// record in encounter order. The canonical use-case is a
    /// multilingual copyright (one box per language).
    #[test]
    fn parse_track_udta_collects_multiple_cprts() {
        let mut c1 = Vec::new();
        c1.extend_from_slice(&[0u8; 4]);
        c1.extend_from_slice(&pack_lang(b"eng"));
        c1.extend_from_slice(b"(c) 2026 Example\0");
        let mut c2 = Vec::new();
        c2.extend_from_slice(&[0u8; 4]);
        c2.extend_from_slice(&pack_lang(b"fra"));
        c2.extend_from_slice(b"(c) 2026 Exemple\0");

        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"cprt", &c1));
        udta.extend(wrap_box_full_size(b"cprt", &c2));

        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert_eq!(t.copyrights.len(), 2);
        assert_eq!(&t.copyrights[0].language, b"eng");
        assert_eq!(t.copyrights[0].notice, "(c) 2026 Example");
        assert_eq!(&t.copyrights[1].language, b"fra");
        assert_eq!(t.copyrights[1].notice, "(c) 2026 Exemple");
    }

    /// Surfacing: each parsed `cprt` record shows up on `params.options`
    /// as the pair `copyright_<n>` (notice) + `copyright_<n>_lang`
    /// (3-letter language tag), keyed by 0-based encounter index. An
    /// empty notice omits the notice key but keeps the `_lang` key so
    /// a consumer can still tell "track declares an empty notice in
    /// `eng`" from "track has no `cprt`".
    #[test]
    fn build_stream_info_surfaces_copyrights_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        t.copyrights.push(super::CopyrightRecord {
            language: *b"eng",
            notice: "(c) 2026 Example".to_string(),
        });
        t.copyrights.push(super::CopyrightRecord {
            language: *b"jpn",
            notice: "".to_string(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("copyright_0"),
            Some("(c) 2026 Example")
        );
        assert_eq!(info.params.options.get("copyright_0_lang"), Some("eng"));
        assert_eq!(info.params.options.get("copyright_1"), None);
        assert_eq!(info.params.options.get("copyright_1_lang"), Some("jpn"));
        assert_eq!(info.params.options.get("copyright_2"), None);
    }

    /// A track with no `cprt` records surfaces no `copyright_*` options.
    #[test]
    fn build_stream_info_no_copyrights_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("copyright_0"), None);
        assert_eq!(info.params.options.get("copyright_0_lang"), None);
    }

    /// End-to-end: a `cprt` box nested inside `trak/udta` is picked up
    /// by `parse_trak` and lands on the track's `copyrights` list.
    #[test]
    fn parse_trak_picks_up_nested_cprt() {
        // Minimal tkhd (v0): 84 bytes; set track_ID at offset 12.
        let mut tkhd = vec![0u8; 84];
        tkhd[12..16].copy_from_slice(&11u32.to_be_bytes());
        // Minimal mdhd (v0, 24 bytes).
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 4]); // version+flags
        mdhd.extend_from_slice(&[0u8; 8]); // created+modified
        mdhd.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
        mdhd.extend_from_slice(&[0u8; 4]); // language + pre_defined
                                           // Minimal hdlr identifying a video track.
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 4]); // FullBox
        hdlr.extend_from_slice(&[0u8; 4]); // pre_defined
        hdlr.extend_from_slice(b"vide"); // handler_type
        hdlr.extend_from_slice(&[0u8; 12]); // reserved
        hdlr.push(0); // name (empty C string)
                      // Wrap mdhd + hdlr into a minimal mdia.
        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));
        // Minimal `cprt` box: language `eng`, notice "(c) 2026 Example".
        let mut cprt = Vec::new();
        cprt.extend_from_slice(&[0u8; 4]);
        cprt.extend_from_slice(&pack_lang(b"eng"));
        cprt.extend_from_slice(b"(c) 2026 Example\0");
        // Wrap cprt in a track-level udta.
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"cprt", &cprt));
        // Assemble the trak body.
        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        trak.extend(wrap_box_full_size(b"udta", &udta));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.track_id, 11);
        assert_eq!(t.copyrights.len(), 1);
        assert_eq!(&t.copyrights[0].language, b"eng");
        assert_eq!(t.copyrights[0].notice, "(c) 2026 Example");
    }

    /// §8.10.3 — `tsel` (TrackSelectionBox): FullBox preamble + signed
    /// 32-bit `switch_group` + zero or more 4-byte FourCC attributes
    /// running to the end of the box. The canonical bitrate-adaptation
    /// rendition: `switch_group = 1` + `bitr` (differentiating attribute
    /// = bitrate).
    #[test]
    fn parse_tsel_switch_group_and_one_attribute() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox version 0 + flags 0
        body.extend_from_slice(&1i32.to_be_bytes()); // switch_group = 1
        body.extend_from_slice(b"bitr"); // attribute_list[0]
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        let tsel = t.tsel.as_ref().unwrap();
        assert_eq!(tsel.switch_group, 1);
        assert_eq!(tsel.attribute_list, vec![*b"bitr"]);
    }

    /// `tsel` with multiple attributes: each 4-byte chunk after the
    /// `switch_group` is one FourCC. Order matches on-disk order
    /// (the spec writes "list" without forcing a particular sort).
    #[test]
    fn parse_tsel_multiple_attributes_preserve_order() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&42i32.to_be_bytes()); // switch_group = 42
        body.extend_from_slice(b"bitr"); // bitrate (differentiating)
        body.extend_from_slice(b"cdec"); // codec (differentiating)
        body.extend_from_slice(b"lang"); // language (differentiating)
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        let tsel = t.tsel.as_ref().unwrap();
        assert_eq!(tsel.switch_group, 42);
        assert_eq!(tsel.attribute_list, vec![*b"bitr", *b"cdec", *b"lang"]);
    }

    /// `tsel` with `switch_group = 0` and an empty attribute list is the
    /// minimal legal body: 4-byte FullBox + 4-byte `switch_group`. The
    /// box must still parse — surfacing a zero switch group lets a
    /// consumer tell "present but no information" from "absent".
    #[test]
    fn parse_tsel_zero_switch_group_no_attributes() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&0i32.to_be_bytes()); // switch_group = 0
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        let tsel = t.tsel.as_ref().unwrap();
        assert_eq!(tsel.switch_group, 0);
        assert!(tsel.attribute_list.is_empty());
    }

    /// `template int(32) switch_group` is signed — a negative value must
    /// be preserved. The spec doesn't reserve negative space, but the
    /// declared type is signed so we read it as signed (a hand-crafted
    /// authoring tool emitting `switch_group = -1` round-trips cleanly).
    #[test]
    fn parse_tsel_negative_switch_group_preserved() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&(-7i32).to_be_bytes()); // switch_group = -7
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        let tsel = t.tsel.as_ref().unwrap();
        assert_eq!(tsel.switch_group, -7);
    }

    /// A body shorter than `FullBox + switch_group` (8 bytes) is silently
    /// dropped — `tsel` is informational and a malformed entry should
    /// never abort the demux (mirroring `parse_kind`).
    #[test]
    fn parse_tsel_too_short_silently_dropped() {
        let mut t = fresh_track();
        super::parse_tsel(&[0u8; 7], &mut t);
        assert!(t.tsel.is_none());
    }

    /// Unknown FullBox `version` (the spec pins it to 0): silently
    /// dropped so a future derived-spec extension never mis-parses on
    /// pre-extension demuxers.
    #[test]
    fn parse_tsel_unknown_version_silently_dropped() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1, 0, 0, 0]); // version 1 + flags 0
        body.extend_from_slice(&5i32.to_be_bytes()); // switch_group = 5
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        assert!(t.tsel.is_none());
    }

    /// Trailing bytes that don't make up a complete 4-byte FourCC
    /// (1–3 bytes after the last full attribute) are ignored —
    /// matches the existing "trailing partial record" handling
    /// elsewhere in the crate (`subs`, `sidx`).
    #[test]
    fn parse_tsel_trailing_partial_fourcc_ignored() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(&3i32.to_be_bytes()); // switch_group
        body.extend_from_slice(b"bitr"); // one complete attribute
        body.extend_from_slice(b"xy"); // 2 trailing bytes — dropped
        let mut t = fresh_track();
        super::parse_tsel(&body, &mut t);
        let tsel = t.tsel.as_ref().unwrap();
        assert_eq!(tsel.switch_group, 3);
        assert_eq!(tsel.attribute_list, vec![*b"bitr"]);
    }

    /// End-to-end: a `tsel` nested inside `trak/udta` is picked up by
    /// `parse_track_udta` (alongside `kind`).
    #[test]
    fn parse_track_udta_picks_up_tsel() {
        let mut tsel = Vec::new();
        tsel.extend_from_slice(&[0u8; 4]); // FullBox
        tsel.extend_from_slice(&9i32.to_be_bytes()); // switch_group = 9
        tsel.extend_from_slice(b"bitr");
        tsel.extend_from_slice(b"lang");
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"tsel", &tsel));

        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        let tb = t.tsel.as_ref().unwrap();
        assert_eq!(tb.switch_group, 9);
        assert_eq!(tb.attribute_list, vec![*b"bitr", *b"lang"]);
    }

    /// `tsel` coexists with `kind` inside the same track-level `udta`.
    /// Both children are picked up and land on their respective fields.
    #[test]
    fn parse_track_udta_collects_tsel_alongside_kind() {
        let mut kind = Vec::new();
        kind.extend_from_slice(&[0u8; 4]);
        kind.extend_from_slice(b"urn:mpeg:dash:role:2011\0");
        kind.extend_from_slice(b"alternate\0");

        let mut tsel = Vec::new();
        tsel.extend_from_slice(&[0u8; 4]);
        tsel.extend_from_slice(&2i32.to_be_bytes());
        tsel.extend_from_slice(b"cdec");

        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"kind", &kind));
        udta.extend(wrap_box_full_size(b"tsel", &tsel));

        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].1, "alternate");
        let tb = t.tsel.as_ref().unwrap();
        assert_eq!(tb.switch_group, 2);
        assert_eq!(tb.attribute_list, vec![*b"cdec"]);
    }

    /// Surfacing: a present `tsel` emits `tsel_switch_group` (always,
    /// even when zero) and `tsel_attributes` (only when the list is
    /// non-empty). The attributes render as space-separated FourCC
    /// strings, matching the `tref_<type>` value convention.
    #[test]
    fn build_stream_info_surfaces_tsel_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        t.tsel = Some(super::TselBox {
            switch_group: 11,
            attribute_list: vec![*b"bitr", *b"cdec"],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tsel_switch_group"), Some("11"));
        assert_eq!(
            info.params.options.get("tsel_attributes"),
            Some("bitr cdec")
        );
    }

    /// A `tsel` with `switch_group = 0` and an empty attribute list
    /// still emits the `tsel_switch_group` key (present-but-zero is
    /// distinguishable from absent at the caller layer) but omits
    /// `tsel_attributes`.
    #[test]
    fn build_stream_info_tsel_zero_no_attributes() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        t.tsel = Some(super::TselBox {
            switch_group: 0,
            attribute_list: Vec::new(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tsel_switch_group"), Some("0"));
        assert_eq!(info.params.options.get("tsel_attributes"), None);
    }

    /// A track with no `tsel` surfaces no `tsel_*` options at all.
    #[test]
    fn build_stream_info_no_tsel_no_tsel_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tsel_switch_group"), None);
        assert_eq!(info.params.options.get("tsel_attributes"), None);
    }

    /// A `tsel` attribute that contains non-printable bytes falls back
    /// to 8-digit hex on the rendered surface, matching the
    /// `tref_<type>` convention for unusual FourCCs.
    #[test]
    fn build_stream_info_tsel_attribute_non_printable_renders_as_hex() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        t.tsel = Some(super::TselBox {
            switch_group: 1,
            attribute_list: vec![[0x00, 0x01, 0x02, 0x03], *b"bitr"],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("tsel_attributes"),
            Some("00010203 bitr")
        );
    }

    /// End-to-end: a `tsel` nested inside `trak/udta` is picked up by
    /// `parse_trak` and lands on the track field.
    #[test]
    fn parse_trak_picks_up_nested_tsel() {
        let mut tkhd = vec![0u8; 84];
        tkhd[12..16].copy_from_slice(&13u32.to_be_bytes());
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 4]);
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&1000u32.to_be_bytes());
        mdhd.extend_from_slice(&0u32.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 4]);
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 4]);
        hdlr.extend_from_slice(&[0u8; 4]);
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));

        let mut tsel = Vec::new();
        tsel.extend_from_slice(&[0u8; 4]);
        tsel.extend_from_slice(&100i32.to_be_bytes());
        tsel.extend_from_slice(b"bitr");
        tsel.extend_from_slice(b"lang");

        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"tsel", &tsel));

        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        trak.extend(wrap_box_full_size(b"udta", &udta));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.track_id, 13);
        let tb = t.tsel.as_ref().unwrap();
        assert_eq!(tb.switch_group, 100);
        assert_eq!(tb.attribute_list, vec![*b"bitr", *b"lang"]);
    }

    // --- §8.14 Sub tracks (strk / stri / strd / stsg) ---

    /// Build a minimal `stri` (§8.14.4) body: FullBox preamble +
    /// `switch_group` (i16) + `alternate_group` (i16) + `sub_track_ID`
    /// (u32) + zero or more attribute FourCCs.
    fn stri_body(switch: i16, alt: i16, id: u32, attrs: &[&[u8; 4]]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 4]); // FullBox version 0 + flags 0
        b.extend_from_slice(&switch.to_be_bytes());
        b.extend_from_slice(&alt.to_be_bytes());
        b.extend_from_slice(&id.to_be_bytes());
        for a in attrs {
            b.extend_from_slice(*a);
        }
        b
    }

    /// Build a `stsg` (§8.14.6) body: FullBox preamble + `grouping_type`
    /// FourCC + `item_count` (u16) + that many `group_description_index`
    /// (u32) values.
    fn stsg_body(grouping_type: &[u8; 4], indices: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0u8; 4]); // FullBox
        b.extend_from_slice(grouping_type);
        b.extend_from_slice(&(indices.len() as u16).to_be_bytes());
        for i in indices {
            b.extend_from_slice(&i.to_be_bytes());
        }
        b
    }

    /// §8.14.4 — `stri`: the three fixed fields plus a two-entry
    /// `attribute_list`. The canonical view-scalable sub track:
    /// `switch_group = 1`, `alternate_group = 1`, `sub_track_ID = 2`,
    /// attributes `vwsc` (view scalability, descriptive) + `nvws`
    /// (number of views, differentiating).
    #[test]
    fn parse_stri_fields_and_attributes() {
        let body = stri_body(1, 1, 2, &[b"vwsc", b"nvws"]);
        let st = super::parse_stri(&body).unwrap();
        assert_eq!(st.switch_group, 1);
        assert_eq!(st.alternate_group, 1);
        assert_eq!(st.sub_track_id, 2);
        assert_eq!(st.attribute_list, vec![*b"vwsc", *b"nvws"]);
        assert!(st.sample_groups.is_empty());
    }

    /// `stri` group fields are signed 16-bit (`template int(16)`); a
    /// negative value must round-trip. The minimal legal body carries the
    /// three fixed fields and an empty attribute list.
    #[test]
    fn parse_stri_negative_groups_no_attributes() {
        let body = stri_body(-3, -5, 0, &[]);
        let st = super::parse_stri(&body).unwrap();
        assert_eq!(st.switch_group, -3);
        assert_eq!(st.alternate_group, -5);
        assert_eq!(st.sub_track_id, 0);
        assert!(st.attribute_list.is_empty());
    }

    /// A `stri` body shorter than `FullBox + switch + alt + id` (12 bytes)
    /// is rejected — without the fixed fields there is no usable selection
    /// metadata.
    #[test]
    fn parse_stri_too_short_rejected() {
        assert!(super::parse_stri(&[0u8; 11]).is_none());
    }

    /// Unknown FullBox `version` (the spec pins `stri` to 0) is rejected
    /// so a future derived-spec extension never mis-parses.
    #[test]
    fn parse_stri_unknown_version_rejected() {
        let mut body = stri_body(1, 1, 1, &[b"bitr"]);
        body[0] = 1; // version 1
        assert!(super::parse_stri(&body).is_none());
    }

    /// A trailing 1–3 byte fragment after the last complete attribute
    /// FourCC is ignored ("attribute_list[] to the end of the box"),
    /// matching the `parse_tsel` posture.
    #[test]
    fn parse_stri_trailing_partial_fourcc_ignored() {
        let mut body = stri_body(7, 0, 9, &[b"frar"]);
        body.extend_from_slice(b"xy"); // 2 trailing bytes — dropped
        let st = super::parse_stri(&body).unwrap();
        assert_eq!(st.attribute_list, vec![*b"frar"]);
    }

    /// §8.14.6 — `stsg`: grouping_type + item_count + index list.
    #[test]
    fn parse_stsg_grouping_and_indices() {
        let body = stsg_body(b"tele", &[1, 3, 7]);
        let sg = super::parse_stsg(&body).unwrap();
        assert_eq!(&sg.grouping_type, b"tele");
        assert_eq!(sg.group_description_indices, vec![1, 3, 7]);
    }

    /// `stsg` with `item_count = 0` is a legal empty definition (the box
    /// names the grouping type but no description indices yet).
    #[test]
    fn parse_stsg_empty_index_list() {
        let body = stsg_body(b"roll", &[]);
        let sg = super::parse_stsg(&body).unwrap();
        assert_eq!(&sg.grouping_type, b"roll");
        assert!(sg.group_description_indices.is_empty());
    }

    /// A declared `item_count` that overruns the bytes present is rejected
    /// — a truncated index list would lie about the sub track's groups.
    #[test]
    fn parse_stsg_overrunning_item_count_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox
        body.extend_from_slice(b"tele");
        body.extend_from_slice(&3u16.to_be_bytes()); // claims 3 indices
        body.extend_from_slice(&1u32.to_be_bytes()); // only 1 present
        assert!(super::parse_stsg(&body).is_none());
    }

    /// Unknown FullBox version on `stsg` is rejected.
    #[test]
    fn parse_stsg_unknown_version_rejected() {
        let mut body = stsg_body(b"tele", &[1]);
        body[0] = 2;
        assert!(super::parse_stsg(&body).is_none());
    }

    /// §8.14.3 — `strk`: full assembly with a `stri` plus a `strd`
    /// holding two `stsg` boxes. The whole sub track lands on
    /// `t.sub_tracks` with its sample-group definitions attached in
    /// on-disk order.
    #[test]
    fn parse_strk_with_stri_and_strd_stsg() {
        let stri = stri_body(2, 2, 5, &[b"tesc"]);
        let stsg0 = stsg_body(b"tele", &[1, 2]);
        let stsg1 = stsg_body(b"roll", &[4]);
        let mut strd = Vec::new();
        strd.extend(wrap_box_full_size(b"stsg", &stsg0));
        strd.extend(wrap_box_full_size(b"stsg", &stsg1));
        let mut strk = Vec::new();
        strk.extend(wrap_box_full_size(b"stri", &stri));
        strk.extend(wrap_box_full_size(b"strd", &strd));

        let mut t = fresh_track();
        super::parse_strk(&strk, &mut t);
        assert_eq!(t.sub_tracks.len(), 1);
        let st = &t.sub_tracks[0];
        assert_eq!(st.switch_group, 2);
        assert_eq!(st.alternate_group, 2);
        assert_eq!(st.sub_track_id, 5);
        assert_eq!(st.attribute_list, vec![*b"tesc"]);
        assert_eq!(st.sample_groups.len(), 2);
        assert_eq!(&st.sample_groups[0].grouping_type, b"tele");
        assert_eq!(st.sample_groups[0].group_description_indices, vec![1, 2]);
        assert_eq!(&st.sample_groups[1].grouping_type, b"roll");
        assert_eq!(st.sample_groups[1].group_description_indices, vec![4]);
    }

    /// A `strk` with a `stri` but no `strd` still surfaces the selection
    /// metadata (with an empty `sample_groups`). `strd` is mandatory per
    /// §8.14.5.1, but a producer slip is tolerated rather than aborting.
    #[test]
    fn parse_strk_stri_only_no_strd() {
        let stri = stri_body(1, 0, 3, &[]);
        let mut strk = Vec::new();
        strk.extend(wrap_box_full_size(b"stri", &stri));
        let mut t = fresh_track();
        super::parse_strk(&strk, &mut t);
        assert_eq!(t.sub_tracks.len(), 1);
        assert_eq!(t.sub_tracks[0].sub_track_id, 3);
        assert!(t.sub_tracks[0].sample_groups.is_empty());
    }

    /// A `strk` missing its mandatory `stri` contributes no sub track —
    /// there is no selection metadata to surface.
    #[test]
    fn parse_strk_without_stri_dropped() {
        let strd = Vec::new();
        let mut strk = Vec::new();
        strk.extend(wrap_box_full_size(b"strd", &strd));
        let mut t = fresh_track();
        super::parse_strk(&strk, &mut t);
        assert!(t.sub_tracks.is_empty());
    }

    /// Multiple `strk` boxes per track (§8.14.3.1 "Zero or more") are all
    /// collected in on-disk order via `parse_track_udta`.
    #[test]
    fn parse_track_udta_collects_multiple_strk() {
        let strk_a = {
            let stri = stri_body(1, 1, 1, &[b"tesc"]);
            wrap_box_full_size(b"stri", &stri)
        };
        let strk_b = {
            let stri = stri_body(2, 1, 2, &[b"spsc"]);
            wrap_box_full_size(b"stri", &stri)
        };
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"strk", &strk_a));
        udta.extend(wrap_box_full_size(b"strk", &strk_b));
        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert_eq!(t.sub_tracks.len(), 2);
        assert_eq!(t.sub_tracks[0].sub_track_id, 1);
        assert_eq!(t.sub_tracks[0].attribute_list, vec![*b"tesc"]);
        assert_eq!(t.sub_tracks[1].sub_track_id, 2);
        assert_eq!(t.sub_tracks[1].attribute_list, vec![*b"spsc"]);
    }

    /// Surfacing: a sub track emits `subtrack_<n>` carrying the three
    /// selection fields plus optional `attrs=` / `stsg=` blocks.
    #[test]
    fn build_stream_info_surfaces_subtrack_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        t.sub_tracks.push(super::SubTrack {
            switch_group: 4,
            alternate_group: 2,
            sub_track_id: 7,
            attribute_list: vec![*b"tesc", *b"bitr"],
            sample_groups: vec![super::SubTrackSampleGroup {
                grouping_type: *b"tele",
                group_description_indices: vec![1, 2],
            }],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("subtrack_0"),
            Some("id=7 switch=4 alt=2 attrs=tesc bitr stsg=tele:1,2")
        );
    }

    /// A sub track with no attributes and no sample groups surfaces only
    /// the three always-present fixed fields.
    #[test]
    fn build_stream_info_subtrack_minimal() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        t.sub_tracks.push(super::SubTrack {
            switch_group: 0,
            alternate_group: 0,
            sub_track_id: 0,
            attribute_list: Vec::new(),
            sample_groups: Vec::new(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("subtrack_0"),
            Some("id=0 switch=0 alt=0")
        );
    }

    /// A track with no `strk` surfaces no `subtrack_*` options.
    #[test]
    fn build_stream_info_no_strk_no_subtrack_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 90000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("subtrack_0"), None);
    }

    /// End-to-end: a `strk` nested inside `trak/udta` is picked up by
    /// `parse_trak` and lands on the track field.
    #[test]
    fn parse_trak_picks_up_nested_strk() {
        let mut tkhd = vec![0u8; 84];
        tkhd[12..16].copy_from_slice(&21u32.to_be_bytes());
        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0u8; 4]);
        mdhd.extend_from_slice(&[0u8; 8]);
        mdhd.extend_from_slice(&1000u32.to_be_bytes());
        mdhd.extend_from_slice(&0u32.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 4]);
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 4]);
        hdlr.extend_from_slice(&[0u8; 4]);
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));

        let stri = stri_body(5, 5, 8, &[b"spsc"]);
        let stsg = stsg_body(b"tele", &[2]);
        let mut strd = Vec::new();
        strd.extend(wrap_box_full_size(b"stsg", &stsg));
        let mut strk = Vec::new();
        strk.extend(wrap_box_full_size(b"stri", &stri));
        strk.extend(wrap_box_full_size(b"strd", &strd));
        let mut udta = Vec::new();
        udta.extend(wrap_box_full_size(b"strk", &strk));

        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        trak.extend(wrap_box_full_size(b"udta", &udta));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.track_id, 21);
        assert_eq!(t.sub_tracks.len(), 1);
        let st = &t.sub_tracks[0];
        assert_eq!(st.switch_group, 5);
        assert_eq!(st.alternate_group, 5);
        assert_eq!(st.sub_track_id, 8);
        assert_eq!(st.attribute_list, vec![*b"spsc"]);
        assert_eq!(st.sample_groups.len(), 1);
        assert_eq!(&st.sample_groups[0].grouping_type, b"tele");
        assert_eq!(st.sample_groups[0].group_description_indices, vec![2]);
    }

    /// §8.6.1.4 — `cslg` version 0: five signed 32-bit fields after the
    /// FullBox preamble. Exercises a typical Apple-style negative
    /// composition shift (`compositionToDTSShift` positive, least delta
    /// negative).
    #[test]
    fn parse_cslg_v0() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        body.extend_from_slice(&512i32.to_be_bytes()); // compositionToDTSShift
        body.extend_from_slice(&(-512i32).to_be_bytes()); // leastDecodeToDisplayDelta
        body.extend_from_slice(&1024i32.to_be_bytes()); // greatestDecodeToDisplayDelta
        body.extend_from_slice(&0i32.to_be_bytes()); // compositionStartTime
        body.extend_from_slice(&48000i32.to_be_bytes()); // compositionEndTime
        let c = super::parse_cslg(&body).unwrap();
        assert_eq!(c.composition_to_dts_shift, 512);
        assert_eq!(c.least_decode_to_display_delta, -512);
        assert_eq!(c.greatest_decode_to_display_delta, 1024);
        assert_eq!(c.composition_start_time, 0);
        assert_eq!(c.composition_end_time, 48000);
    }

    /// §8.6.1.4 — `cslg` version 1: five signed 64-bit fields. Used when
    /// at least one value exceeds the 32-bit range. Verifies a value
    /// past `i32::MAX` round-trips, and that signed 64-bit is honoured.
    #[test]
    fn parse_cslg_v1_64bit() {
        let big = (i32::MAX as i64) + 1_000; // > 2^31, needs 64 bits
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version 1 + flags 0
        body.extend_from_slice(&0i64.to_be_bytes()); // compositionToDTSShift
        body.extend_from_slice(&(-1_000i64).to_be_bytes()); // leastDecodeToDisplayDelta
        body.extend_from_slice(&2_000i64.to_be_bytes()); // greatestDecodeToDisplayDelta
        body.extend_from_slice(&0i64.to_be_bytes()); // compositionStartTime
        body.extend_from_slice(&big.to_be_bytes()); // compositionEndTime
        let c = super::parse_cslg(&body).unwrap();
        assert_eq!(c.composition_to_dts_shift, 0);
        assert_eq!(c.least_decode_to_display_delta, -1_000);
        assert_eq!(c.greatest_decode_to_display_delta, 2_000);
        assert_eq!(c.composition_start_time, 0);
        assert_eq!(c.composition_end_time, big);
    }

    /// A `cslg` with no room for even the FullBox preamble is rejected.
    #[test]
    fn parse_cslg_too_short() {
        assert!(super::parse_cslg(&[0u8, 0, 0]).is_err());
    }

    /// A `cslg` whose body is cut off mid-field (here a v0 box missing
    /// its last 32-bit field) is rejected rather than silently zero-filled.
    #[test]
    fn parse_cslg_truncated() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags
        body.extend_from_slice(&0i32.to_be_bytes()); // 1/5
        body.extend_from_slice(&0i32.to_be_bytes()); // 2/5
        body.extend_from_slice(&0i32.to_be_bytes()); // 3/5
        body.extend_from_slice(&0i32.to_be_bytes()); // 4/5 — 5th missing
        assert!(super::parse_cslg(&body).is_err());
    }

    /// `parse_stbl` lands the `cslg` on the track alongside the other
    /// sample-table boxes.
    #[test]
    fn parse_stbl_picks_up_cslg() {
        let mut cslg = Vec::new();
        cslg.extend_from_slice(&[0u8; 4]); // v0
        cslg.extend_from_slice(&100i32.to_be_bytes());
        cslg.extend_from_slice(&(-100i32).to_be_bytes());
        cslg.extend_from_slice(&200i32.to_be_bytes());
        cslg.extend_from_slice(&0i32.to_be_bytes());
        cslg.extend_from_slice(&5000i32.to_be_bytes());
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"cslg", &cslg));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        let c = t.cslg.expect("cslg should be parsed");
        assert_eq!(c.composition_to_dts_shift, 100);
        assert_eq!(c.least_decode_to_display_delta, -100);
        assert_eq!(c.greatest_decode_to_display_delta, 200);
        assert_eq!(c.composition_start_time, 0);
        assert_eq!(c.composition_end_time, 5000);
    }

    /// §12.1.2 — `vmhd` body after the FullBox preamble is a 16-bit
    /// `graphicsmode` plus three 16-bit `opcolor` components. Decode a
    /// typical `copy`-mode header with a non-zero opcolor.
    #[test]
    fn parse_vmhd_graphicsmode_and_opcolor() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 1]); // version 0, flags 1
        body.extend_from_slice(&0u16.to_be_bytes()); // graphicsmode = copy
        body.extend_from_slice(&0x1234u16.to_be_bytes()); // opcolor red
        body.extend_from_slice(&0x5678u16.to_be_bytes()); // opcolor green
        body.extend_from_slice(&0x9abcu16.to_be_bytes()); // opcolor blue
        let v = super::parse_vmhd(&body).unwrap();
        assert_eq!(v.graphicsmode, 0);
        assert_eq!(v.opcolor, [0x1234, 0x5678, 0x9abc]);
    }

    /// A non-zero `graphicsmode` (a derived-spec composition mode) is
    /// preserved verbatim, not normalised to `copy`.
    #[test]
    fn parse_vmhd_nonzero_graphicsmode_preserved() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 1]);
        body.extend_from_slice(&0x0100u16.to_be_bytes()); // graphicsmode
        body.extend_from_slice(&[0u8; 6]); // opcolor all zero
        let v = super::parse_vmhd(&body).unwrap();
        assert_eq!(v.graphicsmode, 0x0100);
        assert_eq!(v.opcolor, [0, 0, 0]);
    }

    /// The parser does not require `version == 0` — a stray non-zero
    /// version byte still yields a usable header rather than a drop.
    #[test]
    fn parse_vmhd_tolerates_nonzero_version() {
        let mut body = Vec::new();
        body.extend_from_slice(&[3u8, 0, 0, 1]); // version 3
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(&[0u8; 6]);
        let v = super::parse_vmhd(&body).unwrap();
        assert_eq!(v.graphicsmode, 7);
    }

    /// A body shorter than the full 12 bytes (preamble + graphicsmode +
    /// 3×opcolor) is rejected — a truncated tail would surface noise as
    /// an opcolor component.
    #[test]
    fn parse_vmhd_too_short_is_rejected() {
        // 11 bytes: one short of the final opcolor-blue byte.
        let body = vec![0u8; 11];
        assert!(super::parse_vmhd(&body).is_err());
    }

    /// `parse_minf` lands the `vmhd` on the track.
    #[test]
    fn parse_minf_picks_up_vmhd() {
        let mut vmhd = Vec::new();
        vmhd.extend_from_slice(&[0u8, 0, 0, 1]);
        vmhd.extend_from_slice(&0u16.to_be_bytes());
        vmhd.extend_from_slice(&10u16.to_be_bytes());
        vmhd.extend_from_slice(&20u16.to_be_bytes());
        vmhd.extend_from_slice(&30u16.to_be_bytes());
        let mut minf = Vec::new();
        minf.extend(wrap_box_full_size(b"vmhd", &vmhd));

        let mut t = fresh_track();
        super::parse_minf(&minf, &mut t).unwrap();
        let v = t.vmhd.expect("vmhd should be parsed");
        assert_eq!(v.graphicsmode, 0);
        assert_eq!(v.opcolor, [10, 20, 30]);
    }

    /// §12.1.2 fixes the quantity at one — `parse_minf` keeps the first
    /// `vmhd` and ignores a stray duplicate.
    #[test]
    fn parse_minf_keeps_first_vmhd_ignores_duplicate() {
        let mut first = Vec::new();
        first.extend_from_slice(&[0u8, 0, 0, 1]);
        first.extend_from_slice(&1u16.to_be_bytes());
        first.extend_from_slice(&[0u8; 6]);
        let mut second = Vec::new();
        second.extend_from_slice(&[0u8, 0, 0, 1]);
        second.extend_from_slice(&2u16.to_be_bytes());
        second.extend_from_slice(&[0u8; 6]);
        let mut minf = Vec::new();
        minf.extend(wrap_box_full_size(b"vmhd", &first));
        minf.extend(wrap_box_full_size(b"vmhd", &second));

        let mut t = fresh_track();
        super::parse_minf(&minf, &mut t).unwrap();
        assert_eq!(t.vmhd.unwrap().graphicsmode, 1);
    }

    /// A malformed `vmhd` (too short) inside `minf` is dropped silently —
    /// the walker continues and the track simply has no video header,
    /// rather than failing the whole `minf` parse.
    #[test]
    fn parse_minf_drops_malformed_vmhd() {
        let short = vec![0u8; 5]; // far too short
        let mut minf = Vec::new();
        minf.extend(wrap_box_full_size(b"vmhd", &short));

        let mut t = fresh_track();
        super::parse_minf(&minf, &mut t).unwrap();
        assert!(t.vmhd.is_none());
    }

    /// Surfacing: a parsed `cslg` exposes its five fields on
    /// `params.options` as `cslg_<field>` decimal strings.
    #[test]
    fn build_stream_info_surfaces_cslg_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.cslg = Some(super::CslgBox {
            composition_to_dts_shift: 512,
            least_decode_to_display_delta: -512,
            greatest_decode_to_display_delta: 1024,
            composition_start_time: 0,
            composition_end_time: 90000,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("cslg_composition_to_dts_shift"),
            Some("512")
        );
        assert_eq!(
            info.params
                .options
                .get("cslg_least_decode_to_display_delta"),
            Some("-512")
        );
        assert_eq!(
            info.params
                .options
                .get("cslg_greatest_decode_to_display_delta"),
            Some("1024")
        );
        assert_eq!(
            info.params.options.get("cslg_composition_start_time"),
            Some("0")
        );
        assert_eq!(
            info.params.options.get("cslg_composition_end_time"),
            Some("90000")
        );
    }

    /// Absence: a track with no `cslg` emits none of the `cslg_*`
    /// options keys.
    #[test]
    fn build_stream_info_no_cslg_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("cslg_composition_to_dts_shift"),
            None
        );
        assert_eq!(info.params.options.get("cslg_composition_end_time"), None);
    }

    /// Surfacing: a parsed `vmhd` exposes `vmhd_graphicsmode` (decimal)
    /// and `vmhd_opcolor` (three space-separated decimal components) on
    /// `params.options`.
    #[test]
    fn build_stream_info_surfaces_vmhd_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.vmhd = Some(super::VmhdBox {
            graphicsmode: 0,
            opcolor: [10, 20, 30],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("vmhd_graphicsmode"), Some("0"));
        assert_eq!(info.params.options.get("vmhd_opcolor"), Some("10 20 30"));
    }

    /// Absence: a track with no `vmhd` emits neither `vmhd_*` key.
    #[test]
    fn build_stream_info_no_vmhd_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("vmhd_graphicsmode"), None);
        assert_eq!(info.params.options.get("vmhd_opcolor"), None);
    }

    /// §8.6.3 — `stsh` with two `(shadowed, sync)` pairs. Verifies both
    /// big-endian u32 members of each entry are read in order.
    #[test]
    fn parse_stsh_two_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count
        body.extend_from_slice(&7u32.to_be_bytes()); // shadowed_sample_number
        body.extend_from_slice(&1u32.to_be_bytes()); // sync_sample_number
        body.extend_from_slice(&13u32.to_be_bytes());
        body.extend_from_slice(&10u32.to_be_bytes());
        let v = super::parse_stsh(&body).unwrap();
        assert_eq!(v, vec![(7, 1), (13, 10)]);
    }

    /// A zero-entry `stsh` (header only) parses to an empty table rather
    /// than failing.
    #[test]
    fn parse_stsh_empty_table() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        body.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
        let v = super::parse_stsh(&body).unwrap();
        assert!(v.is_empty());
    }

    /// An `stsh` with no room for even the FullBox + count preamble is
    /// rejected.
    #[test]
    fn parse_stsh_too_short() {
        assert!(super::parse_stsh(&[0u8, 0, 0, 0, 0, 0, 0]).is_err());
    }

    /// An `stsh` whose body is cut off mid-entry (here a count of 2 but
    /// only one full pair of bytes) is rejected rather than truncated to
    /// the bytes present.
    #[test]
    fn parse_stsh_truncated() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags
        body.extend_from_slice(&2u32.to_be_bytes()); // claims 2 entries
        body.extend_from_slice(&7u32.to_be_bytes()); // shadowed (entry 1)
        body.extend_from_slice(&1u32.to_be_bytes()); // sync (entry 1)
        body.extend_from_slice(&13u32.to_be_bytes()); // shadowed (entry 2) — sync missing
        assert!(super::parse_stsh(&body).is_err());
    }

    /// An adversarial `entry_count` far larger than the body cannot
    /// trigger a giant up-front allocation: `with_capacity` is clamped to
    /// the byte budget and the per-entry bounds check rejects the box.
    #[test]
    fn parse_stsh_huge_count_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags
        body.extend_from_slice(&u32::MAX.to_be_bytes()); // claims ~4 billion entries
        body.extend_from_slice(&1u32.to_be_bytes()); // only one u32 of payload
        assert!(super::parse_stsh(&body).is_err());
    }

    /// `parse_stbl` lands the `stsh` on the track alongside the other
    /// sample-table boxes.
    #[test]
    fn parse_stbl_picks_up_stsh() {
        let mut stsh = Vec::new();
        stsh.extend_from_slice(&[0u8; 4]); // v0
        stsh.extend_from_slice(&1u32.to_be_bytes()); // one entry
        stsh.extend_from_slice(&5u32.to_be_bytes()); // shadowed
        stsh.extend_from_slice(&2u32.to_be_bytes()); // sync
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"stsh", &stsh));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.stsh, vec![(5, 2)]);
    }

    /// Surfacing: a parsed `stsh` exposes its pairs on `params.options`
    /// as `stsh_<n>` = `"shadowed sync"` decimal strings.
    #[test]
    fn build_stream_info_surfaces_stsh_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.stsh = vec![(7, 1), (13, 10)];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stsh_0"), Some("7 1"));
        assert_eq!(info.params.options.get("stsh_1"), Some("13 10"));
    }

    /// Absence: a track with no `stsh` emits none of the `stsh_*` keys.
    #[test]
    fn build_stream_info_no_stsh_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stsh_0"), None);
    }

    // --- §8.9 sample groups (sbgp / sgpd) ---------------------------------

    /// §8.9.2 — `sbgp` version 0: grouping_type + entry_count + run-length
    /// `(sample_count, group_description_index)` pairs. No
    /// grouping_type_parameter in v0.
    #[test]
    fn parse_sbgp_v0_two_runs() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        body.extend_from_slice(b"roll"); // grouping_type
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count
        body.extend_from_slice(&10u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&1u32.to_be_bytes()); // group_description_index
        body.extend_from_slice(&5u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // index 0 = "no group"
        let sb = super::parse_sbgp(&body).unwrap();
        assert_eq!(&sb.grouping_type, b"roll");
        assert_eq!(sb.grouping_type_parameter, None);
        assert_eq!(sb.entries, vec![(10, 1), (5, 0)]);
    }

    /// §8.9.2 — `sbgp` version 1 carries an extra grouping_type_parameter
    /// between grouping_type and entry_count.
    #[test]
    fn parse_sbgp_v1_with_parameter() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version 1 + flags 0
        body.extend_from_slice(b"rap "); // grouping_type
        body.extend_from_slice(&7u32.to_be_bytes()); // grouping_type_parameter
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&2u32.to_be_bytes()); // group_description_index
        let sb = super::parse_sbgp(&body).unwrap();
        assert_eq!(&sb.grouping_type, b"rap ");
        assert_eq!(sb.grouping_type_parameter, Some(7));
        assert_eq!(sb.entries, vec![(3, 2)]);
    }

    /// A movie-fragment-local group index (≥ 0x10001, §8.9.4) is preserved
    /// verbatim — the demuxer does not resolve it.
    #[test]
    fn parse_sbgp_keeps_fragment_local_index() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"sync");
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&0x1_0001u32.to_be_bytes()); // fragment-local idx
        let sb = super::parse_sbgp(&body).unwrap();
        assert_eq!(sb.entries, vec![(1, 0x1_0001)]);
    }

    /// A zero-entry `sbgp` (header only) parses to an empty table.
    #[test]
    fn parse_sbgp_empty_table() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
        let sb = super::parse_sbgp(&body).unwrap();
        assert!(sb.entries.is_empty());
    }

    /// An `sbgp` cut off mid-entry is rejected rather than truncated.
    #[test]
    fn parse_sbgp_truncated_entry_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&2u32.to_be_bytes()); // claims 2 entries
        body.extend_from_slice(&1u32.to_be_bytes()); // only one full pair
        body.extend_from_slice(&1u32.to_be_bytes());
        assert!(super::parse_sbgp(&body).is_err());
    }

    /// An `sbgp` too short for the grouping_type is rejected.
    #[test]
    fn parse_sbgp_too_short() {
        assert!(super::parse_sbgp(&[0u8, 0, 0, 0, b'r']).is_err());
    }

    /// §8.9.3 — `sgpd` version 1 with a fixed `default_length`: every
    /// entry is `default_length` bytes; no per-entry length prefix.
    #[test]
    fn parse_sgpd_v1_fixed_length() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version 1 + flags 0
        body.extend_from_slice(b"roll"); // grouping_type
        body.extend_from_slice(&2u32.to_be_bytes()); // default_length = 2
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
        body.extend_from_slice(&[0xFF, 0xFB]); // entry 0 (roll_distance)
        body.extend_from_slice(&[0x00, 0x05]); // entry 1
        let sg = super::parse_sgpd(&body).unwrap();
        assert_eq!(&sg.grouping_type, b"roll");
        assert_eq!(sg.default_sample_description_index, None);
        assert_eq!(sg.entries, vec![vec![0xFF, 0xFB], vec![0x00, 0x05]]);
    }

    /// §8.9.3 — `sgpd` version 1 with `default_length == 0`: each entry is
    /// preceded by its own `u32` description_length.
    #[test]
    fn parse_sgpd_v1_variable_length() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version 1 + flags 0
        body.extend_from_slice(b"prol"); // grouping_type
        body.extend_from_slice(&0u32.to_be_bytes()); // default_length = 0
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
        body.extend_from_slice(&3u32.to_be_bytes()); // description_length 0 = 3
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        body.extend_from_slice(&1u32.to_be_bytes()); // description_length 1 = 1
        body.extend_from_slice(&[0xDD]);
        let sg = super::parse_sgpd(&body).unwrap();
        assert_eq!(&sg.grouping_type, b"prol");
        assert_eq!(sg.entries, vec![vec![0xAA, 0xBB, 0xCC], vec![0xDD]]);
    }

    /// §8.9.3 — `sgpd` version 2 supplies a
    /// `default_sample_description_index` before entry_count.
    #[test]
    fn parse_sgpd_v2_default_index() {
        let mut body = Vec::new();
        body.extend_from_slice(&[2u8, 0, 0, 0]); // version 2 + flags 0
        body.extend_from_slice(b"alst"); // grouping_type
        body.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
        body.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
        let sg = super::parse_sgpd(&body).unwrap();
        assert_eq!(&sg.grouping_type, b"alst");
        assert_eq!(sg.default_sample_description_index, Some(1));
        assert!(sg.entries.is_empty());
    }

    /// §8.9.3 — `sgpd` version 0 carries no per-entry length signalling
    /// (§8.9.3.3 NOTE). With no length to chunk by, the remaining body is
    /// captured as a single combined blob so no bytes are lost.
    #[test]
    fn parse_sgpd_v0_combined_blob_fallback() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        body.extend_from_slice(b"roll"); // grouping_type
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
        body.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // opaque tail
        let sg = super::parse_sgpd(&body).unwrap();
        assert_eq!(sg.entries, vec![vec![0x11, 0x22, 0x33, 0x44]]);
    }

    /// A variable-length `sgpd` whose declared description_length runs past
    /// the body is rejected.
    #[test]
    fn parse_sgpd_variable_truncated_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]);
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&0u32.to_be_bytes()); // default_length = 0
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        body.extend_from_slice(&8u32.to_be_bytes()); // claims 8 bytes
        body.extend_from_slice(&[0x01, 0x02]); // only 2 present
        assert!(super::parse_sgpd(&body).is_err());
    }

    /// The MSB-first `BitCursor` reads big-endian bit fields across byte
    /// boundaries, including a zero-width read.
    #[test]
    fn bit_cursor_reads_msb_first() {
        // 0b1010_0110 0b1100_0001
        let data = [0xA6u8, 0xC1];
        let mut c = super::BitCursor::new(&data, 0);
        assert_eq!(c.read(0), Some(0)); // zero width consumes nothing
        assert_eq!(c.read(4), Some(0b1010));
        assert_eq!(c.read(4), Some(0b0110));
        assert_eq!(c.read(8), Some(0b1100_0001));
        assert_eq!(c.read(1), None); // exhausted
    }

    /// The cursor honours a non-zero byte anchor and rejects an
    /// over-long read.
    #[test]
    fn bit_cursor_anchor_and_overrun() {
        let data = [0x00, 0xFF, 0x0F];
        let mut c = super::BitCursor::new(&data, 1); // start at byte 1
        assert_eq!(c.read(12), Some(0xFF0)); // 0xFF then top nibble of 0x0F
        assert_eq!(c.read(8), None); // only 4 bits left
        assert_eq!(c.read(4), Some(0x0F));
    }

    /// §8.9.5 — `csgp` with 16-bit field widths (byte-aligned): two
    /// patterns of one and two samples. flags encode all three size
    /// codes as `2` (16-bit) with no grouping_type_parameter.
    #[test]
    fn parse_csgp_16bit_widths() {
        // index_size_code=2, count_size_code=2, pattern_size_code=2,
        // gtpp=0  →  flags = 2 | (2<<2) | (2<<4) = 0x2A.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x2A]); // version 0 + flags
        body.extend_from_slice(b"roll"); // grouping_type
        body.extend_from_slice(&2u32.to_be_bytes()); // pattern_count = 2
                                                     // pattern 1: length 1, sample_count 3
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&3u16.to_be_bytes());
        // pattern 2: length 2, sample_count 1
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        // indices: pattern 1 → [5]; pattern 2 → [0, 7]
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&7u16.to_be_bytes());
        let cg = super::parse_csgp(&body).unwrap();
        assert_eq!(&cg.grouping_type, b"roll");
        assert_eq!(cg.grouping_type_parameter, None);
        assert_eq!(cg.patterns.len(), 2);
        assert_eq!(cg.patterns[0].sample_count, 3);
        assert_eq!(cg.patterns[0].indices, vec![5]);
        assert_eq!(cg.patterns[1].sample_count, 1);
        assert_eq!(cg.patterns[1].indices, vec![0, 7]);
    }

    /// §8.9.5 — 4-bit bit-packed fields (the densest form). All three
    /// size codes are `0` (width 4). A single pattern of three samples
    /// packs `pattern_length`+`sample_count` into one byte and the three
    /// indices into 12 bits.
    #[test]
    fn parse_csgp_4bit_packed() {
        // size codes all 0 → flags 0; gtpp=0.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]); // version 0 + flags 0
        body.extend_from_slice(b"sync"); // grouping_type
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count = 1
                                                     // pattern_length=3 (0b0011), sample_count=2 (0b0010) → 0x32
                                                     // indices [1, 2, 4] → 0b0001 0010 0100 = 0x12 0x4_ (12 bits,
                                                     // padded to a byte boundary by the box's size).
        body.push(0x32);
        body.push(0x12);
        body.push(0x40); // top nibble = index 4, low nibble padding
        let cg = super::parse_csgp(&body).unwrap();
        assert_eq!(&cg.grouping_type, b"sync");
        assert_eq!(cg.patterns.len(), 1);
        assert_eq!(cg.patterns[0].sample_count, 2);
        assert_eq!(cg.patterns[0].indices, vec![1, 2, 4]);
    }

    /// §8.9.5 — the flag presence bit (bit 6) adds a 32-bit
    /// `grouping_type_parameter` after `grouping_type`.
    #[test]
    fn parse_csgp_with_grouping_type_parameter() {
        // size codes all 2 (16-bit) plus presence bit (1<<6 = 0x40):
        // flags = 0x2A | 0x40 = 0x6A.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x6A]);
        body.extend_from_slice(b"rap ");
        body.extend_from_slice(&9u32.to_be_bytes()); // grouping_type_parameter
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count = 1
        body.extend_from_slice(&1u16.to_be_bytes()); // pattern_length
        body.extend_from_slice(&4u16.to_be_bytes()); // sample_count
        body.extend_from_slice(&2u16.to_be_bytes()); // index
        let cg = super::parse_csgp(&body).unwrap();
        assert_eq!(&cg.grouping_type, b"rap ");
        assert_eq!(cg.grouping_type_parameter, Some(9));
        assert_eq!(cg.patterns[0].sample_count, 4);
        assert_eq!(cg.patterns[0].indices, vec![2]);
    }

    /// §8.9.5 — a fragment-local index keeps its high bit verbatim. With
    /// a 32-bit index width, bit 31 set marks fragment-local.
    #[test]
    fn parse_csgp_fragment_local_index_preserved() {
        // index_size_code=3 (32-bit), count/pattern codes 2 (16-bit):
        // flags = 3 | (2<<2) | (2<<4) = 0x2B.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x2B]);
        body.extend_from_slice(b"seig");
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count
        body.extend_from_slice(&1u16.to_be_bytes()); // pattern_length
        body.extend_from_slice(&1u16.to_be_bytes()); // sample_count
        body.extend_from_slice(&0x8000_0001u32.to_be_bytes()); // frag-local idx
        let cg = super::parse_csgp(&body).unwrap();
        assert_eq!(cg.patterns[0].indices, vec![0x8000_0001]);
    }

    /// A `csgp` truncated mid-index is rejected, not silently shortened.
    #[test]
    fn parse_csgp_truncated_index_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x2A]); // 16-bit widths
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count
        body.extend_from_slice(&2u16.to_be_bytes()); // pattern_length = 2
        body.extend_from_slice(&1u16.to_be_bytes()); // sample_count
        body.extend_from_slice(&5u16.to_be_bytes()); // only one of two indices
        assert!(super::parse_csgp(&body).is_err());
    }

    /// A `csgp` too short even for the FullBox header is rejected.
    #[test]
    fn parse_csgp_too_short() {
        assert!(super::parse_csgp(&[0u8, 0]).is_err());
    }

    /// `parse_stbl` accumulates multiple `sbgp`/`sgpd` instances (one per
    /// grouping type) on the track rather than overwriting.
    #[test]
    fn parse_stbl_accumulates_sample_groups() {
        // sbgp(roll)
        let mut sbgp = Vec::new();
        sbgp.extend_from_slice(&[0u8; 4]);
        sbgp.extend_from_slice(b"roll");
        sbgp.extend_from_slice(&1u32.to_be_bytes());
        sbgp.extend_from_slice(&4u32.to_be_bytes());
        sbgp.extend_from_slice(&1u32.to_be_bytes());
        // sgpd(roll) v1 fixed length 2
        let mut sgpd = Vec::new();
        sgpd.extend_from_slice(&[1u8, 0, 0, 0]);
        sgpd.extend_from_slice(b"roll");
        sgpd.extend_from_slice(&2u32.to_be_bytes()); // default_length
        sgpd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        sgpd.extend_from_slice(&[0xFF, 0xFB]);

        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"sbgp", &sbgp));
        stbl.extend(wrap_box_full_size(b"sgpd", &sgpd));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.sbgp.len(), 1);
        assert_eq!(&t.sbgp[0].grouping_type, b"roll");
        assert_eq!(t.sbgp[0].entries, vec![(4, 1)]);
        assert_eq!(t.sgpd.len(), 1);
        assert_eq!(&t.sgpd[0].grouping_type, b"roll");
        assert_eq!(t.sgpd[0].entries, vec![vec![0xFF, 0xFB]]);
    }

    /// Surfacing: a parsed `sbgp` exposes its run-length map on
    /// `params.options` as `sbgp_<n>` (grouping type, optional `param=`,
    /// then `count:index` pairs); `sgpd` exposes `sgpd_<n>` (grouping
    /// type, optional `default=`, then hex entry payloads).
    #[test]
    fn build_stream_info_surfaces_sample_groups_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48000;
        t.sbgp = vec![
            super::SbgpBox {
                grouping_type: *b"roll",
                grouping_type_parameter: None,
                entries: vec![(10, 1), (5, 0)],
            },
            super::SbgpBox {
                grouping_type: *b"rap ",
                grouping_type_parameter: Some(7),
                entries: vec![(3, 2)],
            },
        ];
        t.sgpd = vec![super::SgpdBox {
            grouping_type: *b"roll",
            default_sample_description_index: Some(1),
            entries: vec![vec![0xFF, 0xFB], vec![0x00, 0x05]],
        }];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("sbgp_0"), Some("roll 10:1 5:0"));
        assert_eq!(info.params.options.get("sbgp_1"), Some("rap  param=7 3:2"));
        assert_eq!(
            info.params.options.get("sgpd_0"),
            Some("roll default=1 fffb 0005")
        );
    }

    /// Absence: a track with no sample groups emits none of the
    /// `sbgp_*` / `sgpd_*` keys.
    #[test]
    fn build_stream_info_no_sample_groups_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("sbgp_0"), None);
        assert_eq!(info.params.options.get("sgpd_0"), None);
    }

    // --- §8.6.4 sdtp (SampleDependencyTypeBox) ------------------------------

    /// Helper: pack the four 2-bit dependency fields into the on-wire
    /// `sdtp` byte layout (MSB-first per §8.6.4.2).
    fn pack_sdtp(is_leading: u8, depends_on: u8, depended_on: u8, redundancy: u8) -> u8 {
        ((is_leading & 0x03) << 6)
            | ((depends_on & 0x03) << 4)
            | ((depended_on & 0x03) << 2)
            | (redundancy & 0x03)
    }

    /// §8.6.4 — `sdtp` round-trip: an I-picture (depends_on=2,
    /// depended_on=1, redundancy=2, not leading) and a disposable
    /// B-frame (depends_on=1, depended_on=2, redundancy=2, leading
    /// with prior dependency=1) both decode to the expected per-field
    /// 2-bit values.
    #[test]
    fn parse_sdtp_two_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble (version 0 + flags 0)
        body.push(pack_sdtp(2, 2, 1, 2)); // I-picture
        body.push(pack_sdtp(1, 1, 2, 2)); // disposable leading
        let v = super::parse_sdtp(&body).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].is_leading, 2);
        assert_eq!(v[0].sample_depends_on, 2);
        assert_eq!(v[0].sample_is_depended_on, 1);
        assert_eq!(v[0].sample_has_redundancy, 2);
        assert_eq!(v[1].is_leading, 1);
        assert_eq!(v[1].sample_depends_on, 1);
        assert_eq!(v[1].sample_is_depended_on, 2);
        assert_eq!(v[1].sample_has_redundancy, 2);
    }

    /// A zero-sample `sdtp` (FullBox preamble only) parses to an empty
    /// table rather than failing — implied `sample_count` from `stsz`
    /// may legitimately be zero (an empty track).
    #[test]
    fn parse_sdtp_empty_table() {
        let body = vec![0u8; 4]; // FullBox preamble only
        let v = super::parse_sdtp(&body).unwrap();
        assert!(v.is_empty());
    }

    /// An `sdtp` too short for even the FullBox preamble is rejected.
    #[test]
    fn parse_sdtp_too_short() {
        assert!(super::parse_sdtp(&[0u8, 0, 0]).is_err());
    }

    /// Bit-packing decode: a single byte `0b11_10_01_00` must unpack
    /// to (3, 2, 1, 0) — MSB pair first per §8.6.4.2. This is the
    /// pivot test for getting the shift order right.
    #[test]
    fn parse_sdtp_field_order() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.push(0b11_10_01_00); // is_leading=3 depends_on=2 depended_on=1 redundancy=0
        let v = super::parse_sdtp(&body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].is_leading, 3);
        assert_eq!(v[0].sample_depends_on, 2);
        assert_eq!(v[0].sample_is_depended_on, 1);
        assert_eq!(v[0].sample_has_redundancy, 0);
    }

    /// Every field in a single byte that's all zeros parses to all-zero
    /// "unknown" entries (the default state when a producer fills the
    /// table but has no real information per §8.6.4.3 value `0`).
    #[test]
    fn parse_sdtp_all_unknowns() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.push(0u8);
        body.push(0u8);
        body.push(0u8);
        let v = super::parse_sdtp(&body).unwrap();
        assert_eq!(v.len(), 3);
        for e in &v {
            assert_eq!(e.is_leading, 0);
            assert_eq!(e.sample_depends_on, 0);
            assert_eq!(e.sample_is_depended_on, 0);
            assert_eq!(e.sample_has_redundancy, 0);
        }
    }

    /// `parse_stbl` lands the `sdtp` on the track alongside the other
    /// sample-table boxes (proves the dispatch arm is wired).
    #[test]
    fn parse_stbl_picks_up_sdtp() {
        let mut sdtp = Vec::new();
        sdtp.extend_from_slice(&[0u8; 4]); // FullBox preamble
        sdtp.push(pack_sdtp(2, 2, 1, 2)); // I-picture
        sdtp.push(pack_sdtp(0, 1, 2, 0)); // disposable
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"sdtp", &sdtp));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.sdtp.len(), 2);
        assert_eq!(t.sdtp[0].sample_depends_on, 2);
        assert_eq!(t.sdtp[1].sample_is_depended_on, 2);
    }

    /// Surfacing: a parsed `sdtp` exposes its summary counts on
    /// `params.options` as the five `sdtp_*_count` keys. We construct
    /// a 4-entry table covering one I-picture (independent), one
    /// disposable B-frame, one leading B-frame, and one sample with
    /// redundant coding so every counter increments at least once.
    #[test]
    fn build_stream_info_surfaces_sdtp_summary_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.sdtp = vec![
            // I-picture: depends_on=2 (independent), depended_on=1 (not
            // disposable), not leading, no redundancy.
            super::SdtpEntry {
                is_leading: 2,
                sample_depends_on: 2,
                sample_is_depended_on: 1,
                sample_has_redundancy: 0,
            },
            // Disposable B-frame: depended_on=2.
            super::SdtpEntry {
                is_leading: 2,
                sample_depends_on: 1,
                sample_is_depended_on: 2,
                sample_has_redundancy: 0,
            },
            // Leading decodable B-frame: is_leading=3.
            super::SdtpEntry {
                is_leading: 3,
                sample_depends_on: 1,
                sample_is_depended_on: 2,
                sample_has_redundancy: 0,
            },
            // Sample with redundant coding: has_redundancy=1.
            super::SdtpEntry {
                is_leading: 2,
                sample_depends_on: 1,
                sample_is_depended_on: 1,
                sample_has_redundancy: 1,
            },
        ];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("sdtp_count"), Some("4"));
        assert_eq!(info.params.options.get("sdtp_leading_count"), Some("1"));
        assert_eq!(info.params.options.get("sdtp_independent_count"), Some("1"));
        assert_eq!(info.params.options.get("sdtp_disposable_count"), Some("2"));
        assert_eq!(info.params.options.get("sdtp_redundant_count"), Some("1"));
    }

    /// Absence: a track with no `sdtp` emits none of the summary keys.
    #[test]
    fn build_stream_info_no_sdtp_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("sdtp_count"), None);
        assert_eq!(info.params.options.get("sdtp_leading_count"), None);
        assert_eq!(info.params.options.get("sdtp_independent_count"), None);
        assert_eq!(info.params.options.get("sdtp_disposable_count"), None);
        assert_eq!(info.params.options.get("sdtp_redundant_count"), None);
    }

    // --- §8.5.3 stdp (DegradationPriorityBox) -------------------------------

    /// §8.5.3 — `stdp` round-trip: a 3-sample priority table laid out
    /// big-endian after the FullBox preamble decodes to the same three
    /// u16 values.
    #[test]
    fn parse_stdp_three_entries() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble (version 0 + flags 0)
        body.extend_from_slice(&0x0001u16.to_be_bytes());
        body.extend_from_slice(&0x0102u16.to_be_bytes());
        body.extend_from_slice(&0xFFFFu16.to_be_bytes());
        let v = super::parse_stdp(&body).unwrap();
        assert_eq!(v, vec![0x0001u16, 0x0102, 0xFFFF]);
    }

    /// A zero-sample `stdp` (FullBox preamble only) parses to an empty
    /// table rather than failing — the implied `sample_count` from
    /// `stsz` may legitimately be zero.
    #[test]
    fn parse_stdp_empty_table() {
        let body = vec![0u8; 4];
        let v = super::parse_stdp(&body).unwrap();
        assert!(v.is_empty());
    }

    /// An `stdp` too short for even the FullBox preamble is rejected.
    #[test]
    fn parse_stdp_too_short() {
        assert!(super::parse_stdp(&[0u8, 0, 0]).is_err());
    }

    /// A trailing odd byte (the spec never produces one — `priority`
    /// is always a 16-bit value) is silently ignored rather than
    /// failing. Two `priority` u16s + one trailing byte yields a
    /// 2-entry table.
    #[test]
    fn parse_stdp_trailing_odd_byte_ignored() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.extend_from_slice(&0x0001u16.to_be_bytes());
        body.extend_from_slice(&0x0002u16.to_be_bytes());
        body.push(0xAA); // stray byte
        let v = super::parse_stdp(&body).unwrap();
        assert_eq!(v, vec![0x0001u16, 0x0002]);
    }

    /// `parse_stbl` lands the `stdp` on the track alongside the other
    /// sample-table boxes (proves the dispatch arm is wired).
    #[test]
    fn parse_stbl_picks_up_stdp() {
        let mut stdp = Vec::new();
        stdp.extend_from_slice(&[0u8; 4]); // FullBox preamble
        stdp.extend_from_slice(&0x0001u16.to_be_bytes());
        stdp.extend_from_slice(&0x0005u16.to_be_bytes());
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"stdp", &stdp));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.stdp, vec![0x0001u16, 0x0005]);
    }

    /// Surfacing: a parsed `stdp` exposes its summary on `params.options`
    /// as four `stdp_*` keys (count + min + max + sum). The spec leaves
    /// the value semantics to derived specifications, so we surface the
    /// raw aggregates rather than a named-bucket breakdown like `sdtp`.
    #[test]
    fn build_stream_info_surfaces_stdp_summary_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        // Four samples with priorities {1, 5, 7, 3}: min=1, max=7, sum=16.
        t.stdp = vec![1u16, 5, 7, 3];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stdp_count"), Some("4"));
        assert_eq!(info.params.options.get("stdp_min"), Some("1"));
        assert_eq!(info.params.options.get("stdp_max"), Some("7"));
        assert_eq!(info.params.options.get("stdp_sum"), Some("16"));
    }

    /// A single-entry `stdp` exposes `min == max == sum == priority`.
    #[test]
    fn build_stream_info_stdp_single_entry_min_eq_max() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.stdp = vec![42u16];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stdp_count"), Some("1"));
        assert_eq!(info.params.options.get("stdp_min"), Some("42"));
        assert_eq!(info.params.options.get("stdp_max"), Some("42"));
        assert_eq!(info.params.options.get("stdp_sum"), Some("42"));
    }

    /// Absence: a track with no `stdp` emits none of the summary keys.
    #[test]
    fn build_stream_info_no_stdp_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stdp_count"), None);
        assert_eq!(info.params.options.get("stdp_min"), None);
        assert_eq!(info.params.options.get("stdp_max"), None);
        assert_eq!(info.params.options.get("stdp_sum"), None);
    }

    /// Pack two §8.7.6 `padb` nibble values into one byte: top nibble is
    /// `pad1` (sample (i*2)+1), bottom nibble is `pad2` (sample (i*2)+2).
    /// Each nibble's high bit is the reserved zero (§8.7.6.2) and the low
    /// three bits carry the pad count in 0..=7.
    fn pack_padb(pad1: u8, pad2: u8) -> u8 {
        ((pad1 & 0x07) << 4) | (pad2 & 0x07)
    }

    /// `padb` with four samples (one byte holding two pairs of pad
    /// counts) round-trips through the typed accessor and yields the
    /// per-sample u8 vec in decode order.
    #[test]
    fn parse_padb_four_samples_two_bytes() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble (v0, flags 0)
        body.extend_from_slice(&4u32.to_be_bytes()); // sample_count = 4
        body.push(pack_padb(1, 2)); // samples 1, 2
        body.push(pack_padb(3, 7)); // samples 3, 4
        let v = super::parse_padb(&body).unwrap();
        assert_eq!(v, vec![1u8, 2, 3, 7]);
    }

    /// `padb` with an odd sample_count discards the unused trailing
    /// nibble (§8.7.6.2 — `(sample_count + 1) / 2` bytes total, last
    /// byte's bottom nibble is unused when sample_count is odd).
    #[test]
    fn parse_padb_odd_sample_count_drops_trailing_nibble() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count = 3
        body.push(pack_padb(2, 4)); // samples 1, 2
        body.push(pack_padb(6, 5)); // sample 3; pad2 unused
        let v = super::parse_padb(&body).unwrap();
        assert_eq!(v, vec![2u8, 4, 6]);
    }

    /// `padb` with `sample_count == 0` (zero-sample table) is permitted
    /// and yields an empty vec.
    #[test]
    fn parse_padb_zero_samples() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.extend_from_slice(&0u32.to_be_bytes()); // sample_count = 0
        let v = super::parse_padb(&body).unwrap();
        assert!(v.is_empty());
    }

    /// A `padb` body too short for even the FullBox + sample_count
    /// preamble (8 bytes) is rejected outright.
    #[test]
    fn parse_padb_too_short_preamble() {
        assert!(super::parse_padb(&[0u8; 4]).is_err());
        assert!(super::parse_padb(&[0u8; 7]).is_err());
    }

    /// A `padb` whose declared `sample_count` would need more bytes than
    /// the body carries is rejected — the spec mandates the box carry
    /// `(sample_count + 1) / 2` bytes of payload data.
    #[test]
    fn parse_padb_truncated_payload() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.extend_from_slice(&10u32.to_be_bytes()); // sample_count = 10
        body.push(0); // only 1 of the 5 needed bytes
        body.push(0);
        assert!(super::parse_padb(&body).is_err());
    }

    /// The reserved high bit of each nibble (§8.7.6.2) is masked off
    /// rather than carried into the returned pad value — a producer
    /// slip on the reserved bit does not corrupt the count.
    #[test]
    fn parse_padb_masks_reserved_bit() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&2u32.to_be_bytes()); // sample_count = 2
                                                     // pad1 = 5 (0b101) with reserved bit set: nibble = 0b1101 = 0xD
                                                     // pad2 = 2 (0b010) with reserved bit set: nibble = 0b1010 = 0xA
        body.push(0xDA);
        let v = super::parse_padb(&body).unwrap();
        assert_eq!(v, vec![5u8, 2]);
    }

    /// `parse_stbl` lands the `padb` on the track alongside the other
    /// sample-table boxes (proves the dispatch arm is wired).
    #[test]
    fn parse_stbl_picks_up_padb() {
        let mut padb = Vec::new();
        padb.extend_from_slice(&[0u8; 4]); // FullBox preamble
        padb.extend_from_slice(&2u32.to_be_bytes()); // sample_count = 2
        padb.push(pack_padb(4, 1));
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"padb", &padb));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.padb, vec![4u8, 1]);
    }

    /// Surfacing: a parsed `padb` exposes its summary on `params.options`
    /// as four `padb_*` keys (count + max + nonzero_count + 8-bucket
    /// histogram). The histogram fully captures the value distribution
    /// in a single key; consumers can recover any aggregate from it.
    #[test]
    fn build_stream_info_surfaces_padb_summary_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48_000;
        // Eight samples with pad counts {0, 0, 1, 2, 2, 7, 7, 7}:
        // count=8, max=7, nonzero=6
        // histogram = [2,1,2,0,0,0,0,3]
        t.padb = vec![0u8, 0, 1, 2, 2, 7, 7, 7];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("padb_count"), Some("8"));
        assert_eq!(info.params.options.get("padb_max"), Some("7"));
        assert_eq!(info.params.options.get("padb_nonzero_count"), Some("6"));
        assert_eq!(
            info.params.options.get("padb_hist"),
            Some("2:1:2:0:0:0:0:3")
        );
    }

    /// A `padb` whose every entry is zero (the byte-aligned-bitstream
    /// case) still emits the summary keys — the box's presence is the
    /// signal that the producer accounted for padding, even if every
    /// sample happens to be byte-aligned.
    #[test]
    fn build_stream_info_padb_all_zero_emits_keys() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48_000;
        t.padb = vec![0u8; 5];
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("padb_count"), Some("5"));
        assert_eq!(info.params.options.get("padb_max"), Some("0"));
        assert_eq!(info.params.options.get("padb_nonzero_count"), Some("0"));
        assert_eq!(
            info.params.options.get("padb_hist"),
            Some("5:0:0:0:0:0:0:0")
        );
    }

    /// Absence: a track with no `padb` emits none of the summary keys.
    #[test]
    fn build_stream_info_no_padb_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48_000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("padb_count"), None);
        assert_eq!(info.params.options.get("padb_max"), None);
        assert_eq!(info.params.options.get("padb_nonzero_count"), None);
        assert_eq!(info.params.options.get("padb_hist"), None);
    }

    /// Build a `pdin` (ISO/IEC 14496-12 §8.1.3) body: 4-byte FullBox
    /// preamble followed by N `(rate, initial_delay)` u32 pairs in big
    /// endian. Helper for the round 259 pdin tests.
    fn build_pdin(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
        for (rate, delay) in entries {
            body.extend_from_slice(&rate.to_be_bytes());
            body.extend_from_slice(&delay.to_be_bytes());
        }
        body
    }

    /// `pdin` with a representative two-entry table round-trips through
    /// the typed accessor in file order, with both u32 fields preserved
    /// verbatim.
    #[test]
    fn parse_pdin_two_entries_preserves_order() {
        let body = build_pdin(&[(125_000, 2_500), (250_000, 1_200)]);
        let r = super::parse_pdin(&body).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(
            r.entries[0],
            super::PdinEntry {
                rate: 125_000,
                initial_delay: 2_500,
            }
        );
        assert_eq!(
            r.entries[1],
            super::PdinEntry {
                rate: 250_000,
                initial_delay: 1_200,
            }
        );
    }

    /// `pdin` with zero pairs (preamble only, 4-byte body) is permitted:
    /// a producer's way of signalling "no progressive-download hints".
    #[test]
    fn parse_pdin_empty_body_yields_empty_entries() {
        let body = build_pdin(&[]);
        let r = super::parse_pdin(&body).unwrap();
        assert!(r.entries.is_empty());
    }

    /// A `pdin` body shorter than the 4-byte FullBox preamble is
    /// rejected outright — `parse_pdin` cannot recover the version bits.
    #[test]
    fn parse_pdin_too_short_preamble() {
        for n in 0..4 {
            let body = vec![0u8; n];
            assert!(super::parse_pdin(&body).is_err(), "len {n} must err");
        }
    }

    /// A `pdin` body whose post-preamble length is not a multiple of 8
    /// (one u32 pair per entry) is rejected — the spec's loop body is
    /// exactly 8 bytes (§8.1.3.2) and a truncated tail is the
    /// producer's bug, not ours to silently round down.
    #[test]
    fn parse_pdin_unaligned_payload_is_rejected() {
        // 4-byte preamble + 12 bytes (1.5 pairs) → unaligned.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&[0u8; 12]);
        assert!(super::parse_pdin(&body).is_err());

        // 4-byte preamble + 9 bytes (just past one pair) → unaligned.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&[0u8; 9]);
        assert!(super::parse_pdin(&body).is_err());
    }

    /// `parse_pdin` tolerates a non-zero version byte: §8.1.3.2 pins
    /// version to 0 but the payload layout is unambiguous (no
    /// version-dependent branching), so we surface whatever the
    /// producer wrote rather than rejecting on a version-bit slip —
    /// matching `parse_padb` / `parse_stdp` posture.
    #[test]
    fn parse_pdin_tolerates_nonzero_version() {
        let mut body = Vec::new();
        body.push(0xAA); // bogus version
        body.extend_from_slice(&[0u8; 3]); // flags
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        let r = super::parse_pdin(&body).unwrap();
        assert_eq!(
            r.entries,
            vec![super::PdinEntry {
                rate: 1,
                initial_delay: 2,
            }]
        );
    }

    /// `parse_pdin` reads u32s in big-endian byte order per the spec
    /// (every multi-byte field in ISOBMFF is big-endian, §4.2 and
    /// §8.1.3.2). A naïve little-endian read would surface
    /// `u32::from_le_bytes` semantics instead, which this test pins.
    #[test]
    fn parse_pdin_big_endian_byte_order() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
                                           // rate = 0x01_02_03_04, initial_delay = 0x05_06_07_08 in big-endian.
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        body.extend_from_slice(&[0x05, 0x06, 0x07, 0x08]);
        let r = super::parse_pdin(&body).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].rate, 0x01_02_03_04);
        assert_eq!(r.entries[0].initial_delay, 0x05_06_07_08);
    }

    /// `parse_pdin_box` (the public wrapper) routes through the same
    /// `parse_pdin` impl as the internal call, so tooling that has the
    /// payload bytes in hand recovers the same `PdinRecord`.
    #[test]
    fn parse_pdin_box_public_entry_matches_internal() {
        let body = build_pdin(&[(64_000, 5_000), (128_000, 2_400), (256_000, 1_000)]);
        let r_public = super::parse_pdin_box(&body).unwrap();
        let r_internal = super::parse_pdin(&body).unwrap();
        assert_eq!(r_public.entries, r_internal.entries);
        assert_eq!(r_public.entries.len(), 3);
    }

    /// One per-level entry to feed `build_leva`. The variant-specific
    /// trailing fields are tagged on the assignment_type discriminant:
    /// 0 → `Some(grouping_type)` only, 1 → both, 4 → sub_track_id, 2/3
    /// → none.
    #[derive(Clone, Copy, Debug)]
    struct LevaBuild {
        track_id: u32,
        padding_flag: bool,
        assignment_type: u8,
        grouping_type: u32,
        grouping_type_parameter: u32,
        sub_track_id: u32,
    }

    /// Build a `leva` (ISO/IEC 14496-12 §8.8.13) body: 4-byte FullBox
    /// preamble + `level_count` u8 + N per-level entries laid out per
    /// §8.8.13.2. Reserved assignment_type values (> 4) emit only the
    /// 5-byte header — the spec doesn't define their tail length.
    fn build_leva(entries: &[LevaBuild]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
        body.push(entries.len() as u8); // level_count
        for e in entries {
            body.extend_from_slice(&e.track_id.to_be_bytes());
            let packed = (if e.padding_flag { 0x80 } else { 0 }) | (e.assignment_type & 0x7f);
            body.push(packed);
            match e.assignment_type {
                0 => body.extend_from_slice(&e.grouping_type.to_be_bytes()),
                1 => {
                    body.extend_from_slice(&e.grouping_type.to_be_bytes());
                    body.extend_from_slice(&e.grouping_type_parameter.to_be_bytes());
                }
                2 | 3 => {}
                4 => body.extend_from_slice(&e.sub_track_id.to_be_bytes()),
                _ => {}
            }
        }
        body
    }

    /// `leva` with a representative two-entry mix (one type-0, one
    /// type-4) round-trips through `parse_leva` in file order, with
    /// all variant-specific fields preserved verbatim.
    #[test]
    fn parse_leva_mixed_assignment_types_preserved() {
        let entries = vec![
            LevaBuild {
                track_id: 1,
                padding_flag: false,
                assignment_type: 0,
                grouping_type: u32::from_be_bytes(*b"roll"),
                grouping_type_parameter: 0,
                sub_track_id: 0,
            },
            LevaBuild {
                track_id: 7,
                padding_flag: true,
                assignment_type: 4,
                grouping_type: 0,
                grouping_type_parameter: 0,
                sub_track_id: 0xAABB_CCDD,
            },
        ];
        let body = build_leva(&entries);
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 2);

        assert_eq!(r.entries[0].track_id, 1);
        assert!(!r.entries[0].padding_flag);
        assert_eq!(r.entries[0].assignment_type, 0);
        assert_eq!(r.entries[0].grouping_type, u32::from_be_bytes(*b"roll"));
        assert_eq!(r.entries[0].grouping_type_parameter, 0);
        assert_eq!(r.entries[0].sub_track_id, 0);

        assert_eq!(r.entries[1].track_id, 7);
        assert!(r.entries[1].padding_flag);
        assert_eq!(r.entries[1].assignment_type, 4);
        assert_eq!(r.entries[1].sub_track_id, 0xAABB_CCDD);
    }

    /// `leva` with assignment_type = 1 carries both `grouping_type`
    /// and `grouping_type_parameter`; the parser must consume both
    /// (8 bytes of tail) per §8.8.13.2.
    #[test]
    fn parse_leva_assignment_type_one_carries_both_grouping_fields() {
        let entries = vec![LevaBuild {
            track_id: 42,
            padding_flag: false,
            assignment_type: 1,
            grouping_type: u32::from_be_bytes(*b"sync"),
            grouping_type_parameter: 0xDEAD_BEEF,
            sub_track_id: 0,
        }];
        let body = build_leva(&entries);
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].assignment_type, 1);
        assert_eq!(r.entries[0].grouping_type, u32::from_be_bytes(*b"sync"));
        assert_eq!(r.entries[0].grouping_type_parameter, 0xDEAD_BEEF);
    }

    /// `leva` with assignment_type 2 or 3 (level by track / by
    /// track-subsegment) is just the 5-byte header — no variant tail.
    /// Two back-to-back type-2 / type-3 entries round-trip cleanly.
    #[test]
    fn parse_leva_assignment_types_two_three_have_no_tail() {
        let entries = vec![
            LevaBuild {
                track_id: 11,
                padding_flag: false,
                assignment_type: 2,
                grouping_type: 0,
                grouping_type_parameter: 0,
                sub_track_id: 0,
            },
            LevaBuild {
                track_id: 12,
                padding_flag: true,
                assignment_type: 3,
                grouping_type: 0,
                grouping_type_parameter: 0,
                sub_track_id: 0,
            },
        ];
        let body = build_leva(&entries);
        // Body: 4 preamble + 1 level_count + 2*5 entry bytes = 15 total.
        assert_eq!(body.len(), 4 + 1 + 2 * 5);
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].assignment_type, 2);
        assert_eq!(r.entries[0].track_id, 11);
        assert_eq!(r.entries[1].assignment_type, 3);
        assert_eq!(r.entries[1].track_id, 12);
        assert!(r.entries[1].padding_flag);
    }

    /// `leva` with `level_count = 0` is permitted by this parser
    /// (the spec pins a minimum of 2 in §8.8.13.3 but we carry whatever
    /// the producer wrote so a validator can flag it). Body is 4-byte
    /// preamble + 1-byte zero level_count.
    #[test]
    fn parse_leva_zero_level_count_yields_empty_entries() {
        let body = build_leva(&[]);
        let r = super::parse_leva(&body).unwrap();
        assert!(r.entries.is_empty());
    }

    /// A `leva` body shorter than the 4-byte FullBox preamble is
    /// rejected — `parse_leva` can't recover the version bits.
    #[test]
    fn parse_leva_too_short_preamble() {
        for n in 0..4 {
            let body = vec![0u8; n];
            assert!(super::parse_leva(&body).is_err(), "len {n} must err");
        }
    }

    /// A `leva` body with the FullBox preamble but no `level_count`
    /// byte is rejected.
    #[test]
    fn parse_leva_missing_level_count_byte() {
        let body = vec![0u8; 4];
        assert!(super::parse_leva(&body).is_err());
    }

    /// A `leva` body that claims `level_count = 1` but stops before
    /// the entry's 5-byte header is rejected as truncated rather than
    /// silently treated as zero entries — a short table would lie
    /// about which levels exist.
    #[test]
    fn parse_leva_truncated_entry_header_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(1); // level_count = 1
                      // Only 4 bytes of the 5-byte header (track_id but no packed byte).
        body.extend_from_slice(&[0u8; 4]);
        assert!(super::parse_leva(&body).is_err());
    }

    /// `leva` assignment_type = 0 with a truncated `grouping_type` (3
    /// bytes instead of 4) is rejected.
    #[test]
    fn parse_leva_truncated_grouping_type_for_at0_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(1); // level_count
        body.extend_from_slice(&5u32.to_be_bytes()); // track_id
        body.push(0); // padding=0 | at=0
                      // Only 3 of 4 grouping_type bytes.
        body.extend_from_slice(&[0u8; 3]);
        assert!(super::parse_leva(&body).is_err());
    }

    /// `leva` assignment_type = 1 with a truncated tail (4 of 8
    /// bytes) is rejected.
    #[test]
    fn parse_leva_truncated_grouping_parameter_for_at1_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(1); // level_count
        body.extend_from_slice(&5u32.to_be_bytes()); // track_id
        body.push(1); // padding=0 | at=1
        body.extend_from_slice(&7u32.to_be_bytes()); // grouping_type
                                                     // Missing the 4-byte grouping_type_parameter.
        assert!(super::parse_leva(&body).is_err());
    }

    /// `leva` assignment_type = 4 with a truncated `sub_track_id` (2
    /// bytes instead of 4) is rejected.
    #[test]
    fn parse_leva_truncated_sub_track_id_for_at4_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(1); // level_count
        body.extend_from_slice(&5u32.to_be_bytes()); // track_id
        body.push(4); // padding=0 | at=4
        body.extend_from_slice(&[0u8; 2]); // only 2 of 4 sub_track_id bytes
        assert!(super::parse_leva(&body).is_err());
    }

    /// `leva` reserved assignment_type values (> 4) are carried as
    /// 5-byte headers with all variant-specific fields zero. The
    /// spec doesn't define their tail, so the parser must not eat
    /// any extra bytes — a downstream entry must still be reachable.
    #[test]
    fn parse_leva_reserved_assignment_type_consumes_no_tail() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(2); // level_count = 2
                      // Entry 0: track_id = 9, padding = 1, at = 5 (reserved).
        body.extend_from_slice(&9u32.to_be_bytes());
        body.push(0x80 | 5);
        // Entry 1: track_id = 10, padding = 0, at = 2 (no tail).
        body.extend_from_slice(&10u32.to_be_bytes());
        body.push(2);
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].track_id, 9);
        assert!(r.entries[0].padding_flag);
        assert_eq!(r.entries[0].assignment_type, 5);
        assert_eq!(r.entries[0].grouping_type, 0);
        assert_eq!(r.entries[0].grouping_type_parameter, 0);
        assert_eq!(r.entries[0].sub_track_id, 0);
        // The second entry must have parsed cleanly, proving the
        // reserved entry didn't consume the next header's bytes.
        assert_eq!(r.entries[1].track_id, 10);
        assert_eq!(r.entries[1].assignment_type, 2);
    }

    /// `parse_leva` tolerates a non-zero FullBox version byte
    /// (§8.8.13.2 pins it to 0). The layout is unambiguous so we
    /// surface whatever the producer wrote — same posture as
    /// `parse_pdin` / `parse_padb` / `parse_stdp`.
    #[test]
    fn parse_leva_tolerates_nonzero_version() {
        let mut body = Vec::new();
        body.push(0xCC); // bogus version
        body.extend_from_slice(&[0u8; 3]); // flags
        body.push(1); // level_count
        body.extend_from_slice(&42u32.to_be_bytes()); // track_id
        body.push(0x80 | 3); // padding=1 | at=3 (no tail)
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].track_id, 42);
        assert!(r.entries[0].padding_flag);
        assert_eq!(r.entries[0].assignment_type, 3);
    }

    /// `parse_leva` reads u32s in big-endian byte order per §4.2 and
    /// §8.8.13.2. A naïve little-endian read would surface
    /// `u32::from_le_bytes` semantics; this test pins the BE choice.
    #[test]
    fn parse_leva_big_endian_byte_order() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // preamble
        body.push(1); // level_count
        body.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // track_id
        body.push(0); // packed byte: padding=0 | at=0
        body.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x01]); // grouping_type
        let r = super::parse_leva(&body).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].track_id, 0x1122_3344);
        assert_eq!(r.entries[0].grouping_type, 0xABCD_EF01);
    }

    /// `parse_leva` masks the 7-bit `assignment_type` against the
    /// `padding_flag` high bit — both halves of the packed byte are
    /// independently recoverable.
    #[test]
    fn parse_leva_packed_byte_splits_padding_and_assignment_type() {
        for padding in [false, true] {
            for at in 0u8..=4 {
                let mut body = Vec::new();
                body.extend_from_slice(&[0u8; 4]);
                body.push(1);
                body.extend_from_slice(&3u32.to_be_bytes());
                body.push((if padding { 0x80 } else { 0 }) | at);
                match at {
                    0 => body.extend_from_slice(&0u32.to_be_bytes()),
                    1 => body.extend_from_slice(&[0u8; 8]),
                    2 | 3 => {}
                    4 => body.extend_from_slice(&0u32.to_be_bytes()),
                    _ => {}
                }
                let r = super::parse_leva(&body).unwrap();
                assert_eq!(r.entries.len(), 1);
                assert_eq!(r.entries[0].padding_flag, padding);
                assert_eq!(r.entries[0].assignment_type, at);
            }
        }
    }

    /// `parse_leva_box` (the public wrapper) routes through the same
    /// `parse_leva` impl as the internal call, so tooling that has
    /// the payload bytes in hand recovers the same `LevaRecord`.
    #[test]
    fn parse_leva_box_public_entry_matches_internal() {
        let entries = vec![
            LevaBuild {
                track_id: 1,
                padding_flag: true,
                assignment_type: 0,
                grouping_type: u32::from_be_bytes(*b"rap "),
                grouping_type_parameter: 0,
                sub_track_id: 0,
            },
            LevaBuild {
                track_id: 2,
                padding_flag: false,
                assignment_type: 1,
                grouping_type: u32::from_be_bytes(*b"alst"),
                grouping_type_parameter: 0x1234_5678,
                sub_track_id: 0,
            },
            LevaBuild {
                track_id: 3,
                padding_flag: false,
                assignment_type: 4,
                grouping_type: 0,
                grouping_type_parameter: 0,
                sub_track_id: 99,
            },
        ];
        let body = build_leva(&entries);
        let r_public = super::parse_leva_box(&body).unwrap();
        let r_internal = super::parse_leva(&body).unwrap();
        assert_eq!(r_public.entries, r_internal.entries);
        assert_eq!(r_public.entries.len(), 3);
    }

    // ---- trep (TrackExtensionPropertiesBox, §8.8.15) -------------------

    /// Build a child box (8-byte header + payload) for embedding in a
    /// `trep` body.
    fn build_child_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = 8 + payload.len();
        let mut v = Vec::with_capacity(size);
        v.extend_from_slice(&(size as u32).to_be_bytes());
        v.extend_from_slice(fourcc);
        v.extend_from_slice(payload);
        v
    }

    /// Build a `trep` body: 4-byte FullBox preamble + 4-byte track_id +
    /// the concatenated child boxes.
    fn build_trep(track_id: u32, children: &[Vec<u8>]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&[0u8; 4]); // version + flags = 0
        v.extend_from_slice(&track_id.to_be_bytes());
        for c in children {
            v.extend_from_slice(c);
        }
        v
    }

    /// A `trep` carrying just its `track_id` (no children) parses to an
    /// empty `children` list. Body is 4 preamble + 4 track_id = 8 bytes.
    #[test]
    fn parse_trep_track_id_only() {
        let body = build_trep(0x0102_0304, &[]);
        assert_eq!(body.len(), 8);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.track_id, 0x0102_0304);
        assert!(r.children.is_empty());
    }

    /// A `trep` with two child boxes records each child's fourcc and
    /// payload length, in file order, without recursing into them.
    #[test]
    fn parse_trep_records_children_in_order() {
        let assp = build_child_box(b"assp", &[0xaa; 12]);
        let unkn = build_child_box(b"xyzw", &[]);
        let body = build_trep(7, &[assp, unkn]);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.track_id, 7);
        assert_eq!(r.children.len(), 2);
        assert_eq!(&r.children[0].fourcc, b"assp");
        assert_eq!(r.children[0].payload_len, 12);
        assert_eq!(&r.children[1].fourcc, b"xyzw");
        assert_eq!(r.children[1].payload_len, 0);
    }

    /// A `trep` body shorter than the 4-byte FullBox preamble is
    /// rejected — there's nothing to recover.
    #[test]
    fn parse_trep_too_short_preamble() {
        for n in 0..4 {
            let body = vec![0u8; n];
            assert!(super::parse_trep(&body).is_err(), "len {n} must err");
        }
    }

    /// A `trep` body with the preamble but a truncated `track_id` (fewer
    /// than 4 bytes after the preamble) is rejected.
    #[test]
    fn parse_trep_truncated_track_id() {
        for extra in 0..4 {
            let mut body = vec![0u8; 4]; // preamble
            body.extend_from_slice(&vec![0u8; extra]);
            assert!(
                super::parse_trep(&body).is_err(),
                "track_id with {extra} bytes must err",
            );
        }
    }

    /// A non-zero FullBox version is tolerated (the layout up to
    /// `track_id` is unambiguous), matching the `parse_leva` posture.
    #[test]
    fn parse_trep_tolerates_nonzero_version() {
        let mut body = build_trep(42, &[build_child_box(b"assp", &[1, 2, 3, 4])]);
        body[0] = 0x01; // flip the version byte
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.track_id, 42);
        assert_eq!(r.children.len(), 1);
        assert_eq!(r.children[0].payload_len, 4);
    }

    /// A child whose declared size overruns the remaining `trep` body is
    /// clamped to the bytes available, recorded with the clamped length,
    /// and ends the child walk (no sibling can follow an over-long box).
    #[test]
    fn parse_trep_clamps_overlong_child() {
        let mut body = build_trep(3, &[]);
        // Child header declaring size 0x40 (64) but only 4 payload
        // bytes actually present after the 8-byte header.
        body.extend_from_slice(&64u32.to_be_bytes());
        body.extend_from_slice(b"assp");
        body.extend_from_slice(&[0xff; 4]);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.children.len(), 1);
        assert_eq!(&r.children[0].fourcc, b"assp");
        // Available after the header = 4 bytes; clamped payload_len = 4.
        assert_eq!(r.children[0].payload_len, 4);
    }

    /// A trailing child header that doesn't fit (fewer than 8 bytes left
    /// after the previous child) ends the walk cleanly rather than
    /// erroring — the `track_id` and the well-formed children survive.
    #[test]
    fn parse_trep_trailing_partial_child_header_ignored() {
        let mut body = build_trep(9, &[build_child_box(b"assp", &[0; 4])]);
        // Append 5 stray bytes — too few for an 8-byte box header.
        body.extend_from_slice(&[0x11; 5]);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.track_id, 9);
        assert_eq!(r.children.len(), 1);
        assert_eq!(&r.children[0].fourcc, b"assp");
    }

    /// A child using the 64-bit `largesize` form (size32 == 1) is read
    /// via its 16-byte header and the largesize field.
    #[test]
    fn parse_trep_child_largesize_form() {
        let mut body = build_trep(5, &[]);
        // size32 = 1 signals a 64-bit largesize follows the fourcc.
        let payload = [0x55u8; 6];
        let total: u64 = (16 + payload.len()) as u64;
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(b"assp");
        body.extend_from_slice(&total.to_be_bytes());
        body.extend_from_slice(&payload);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.children.len(), 1);
        assert_eq!(&r.children[0].fourcc, b"assp");
        assert_eq!(r.children[0].payload_len, 6);
    }

    /// `parse_trep_box` (the public wrapper) routes through the same
    /// internal parser as the `open()` walk.
    #[test]
    fn parse_trep_box_public_entry_matches_internal() {
        let body = build_trep(123, &[build_child_box(b"assp", &[7; 8])]);
        let r_public = super::parse_trep_box(&body).unwrap();
        let r_internal = super::parse_trep(&body).unwrap();
        assert_eq!(r_public.track_id, r_internal.track_id);
        assert_eq!(r_public.children, r_internal.children);
        assert_eq!(r_public.track_id, 123);
    }

    /// `parse_ssix_box` (the public wrapper) routes through the same
    /// internal parser as the `open()` walk, and `build_ssix_box`
    /// round-trips a record through it (modulo the 8-byte box header
    /// the builder prepends).
    #[test]
    fn ssix_public_entries_round_trip() {
        let record = super::SsixRecord {
            subsegments: vec![
                super::SsixSubsegment {
                    ranges: vec![
                        super::SsixRange {
                            level: 0,
                            range_size: 1_024,
                        },
                        super::SsixRange {
                            level: 1,
                            range_size: 0xFF_FFFF,
                        },
                    ],
                },
                super::SsixSubsegment {
                    ranges: vec![
                        super::SsixRange {
                            level: 0,
                            range_size: 512,
                        },
                        super::SsixRange {
                            level: 2,
                            range_size: 7,
                        },
                    ],
                },
            ],
        };
        let boxed = super::build_ssix_box(&record).unwrap();
        assert_eq!(&boxed[4..8], b"ssix");
        assert_eq!(
            boxed.len(),
            u32::from_be_bytes(boxed[..4].try_into().unwrap()) as usize
        );
        let body = &boxed[8..];
        let r_public = super::parse_ssix_box(body).unwrap();
        let r_internal = super::parse_ssix(body).unwrap();
        assert_eq!(r_public, r_internal);
        assert_eq!(r_public, record);
    }

    /// `(subsample_size16, priority, discardable, csp)` — one v0 sub-sample.
    type SubsV0Row = (u16, u8, u8, u32);
    /// `(subsample_size32, priority, discardable, csp)` — one v1 sub-sample.
    type SubsV1Row = (u32, u8, u8, u32);

    /// Build a v0 `subs` body: FullBox preamble + entry_count + N entries.
    /// Each entry: `(sample_delta, [(size16, priority, discardable, csp), ...])`.
    fn build_subs_v0(flags: u32, entries: &[(u32, &[SubsV0Row])]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0u8); // version
        body.extend_from_slice(&flags.to_be_bytes()[1..]); // 24-bit flags
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (sample_delta, subs) in entries {
            body.extend_from_slice(&sample_delta.to_be_bytes());
            body.extend_from_slice(&(subs.len() as u16).to_be_bytes());
            for (size, prio, disc, csp) in subs.iter() {
                body.extend_from_slice(&size.to_be_bytes());
                body.push(*prio);
                body.push(*disc);
                body.extend_from_slice(&csp.to_be_bytes());
            }
        }
        body
    }

    /// Build a v1 `subs` body — `subsample_size` widens to 32-bit.
    fn build_subs_v1(flags: u32, entries: &[(u32, &[SubsV1Row])]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(1u8);
        body.extend_from_slice(&flags.to_be_bytes()[1..]);
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (sample_delta, subs) in entries {
            body.extend_from_slice(&sample_delta.to_be_bytes());
            body.extend_from_slice(&(subs.len() as u16).to_be_bytes());
            for (size, prio, disc, csp) in subs.iter() {
                body.extend_from_slice(&size.to_be_bytes());
                body.push(*prio);
                body.push(*disc);
                body.extend_from_slice(&csp.to_be_bytes());
            }
        }
        body
    }

    /// §8.7.7 — v0 round-trip with two entries: one sample (delta=1) split
    /// into three sub-samples with varied priority/discardable/csp, plus
    /// a second sample (delta=2 → sample 3) with one sub-sample. Verifies
    /// field order, the 16-bit `subsample_size` width at v0, and the
    /// sparse `sample_delta` accumulation contract.
    #[test]
    fn parse_subs_v0_round_trip() {
        let body = build_subs_v0(
            0,
            &[
                (
                    1,
                    &[
                        (100, 5, 0, 0x0000_0001),
                        (50, 4, 1, 0x0000_0002),
                        (25, 3, 0, 0),
                    ],
                ),
                (2, &[(80, 6, 0, 0xdead_beef)]),
            ],
        );
        let s = super::parse_subs(&body).unwrap();
        assert_eq!(s.version, 0);
        assert_eq!(s.flags, 0);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].sample_delta, 1);
        assert_eq!(s.entries[0].subsamples.len(), 3);
        assert_eq!(s.entries[0].subsamples[0].subsample_size, 100);
        assert_eq!(s.entries[0].subsamples[0].subsample_priority, 5);
        assert_eq!(s.entries[0].subsamples[0].discardable, 0);
        assert_eq!(s.entries[0].subsamples[0].codec_specific_parameters, 1);
        assert_eq!(s.entries[0].subsamples[2].subsample_size, 25);
        assert_eq!(s.entries[1].sample_delta, 2);
        assert_eq!(s.entries[1].subsamples.len(), 1);
        assert_eq!(
            s.entries[1].subsamples[0].codec_specific_parameters,
            0xdead_beef
        );
    }

    /// §8.7.7 — v1 widens `subsample_size` to 32-bit. Use a payload above
    /// the 16-bit ceiling (0x0001_0000) to prove the widening took effect
    /// and the parser didn't truncate to u16.
    #[test]
    fn parse_subs_v1_size_is_32bit() {
        let body = build_subs_v1(0, &[(1, &[(0x0001_2345, 0, 0, 0)])]);
        let s = super::parse_subs(&body).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.entries[0].subsamples[0].subsample_size, 0x0001_2345);
    }

    /// §8.7.7.1 — a `subsample_count` of 0 is legal: the entry still
    /// consumes one row (advances the sparse delta cursor) but produces
    /// no per-sub-sample fields. Verifies the parser allows the
    /// degenerate shape rather than rejecting it.
    #[test]
    fn parse_subs_empty_subsample_count() {
        let body = build_subs_v0(0, &[(5, &[])]);
        let s = super::parse_subs(&body).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].sample_delta, 5);
        assert!(s.entries[0].subsamples.is_empty());
    }

    /// `flags` is preserved verbatim (codec-specific per §8.7.7.1). Use a
    /// non-trivial 24-bit value to confirm the FullBox preamble parser
    /// didn't mask the field.
    #[test]
    fn parse_subs_preserves_flags() {
        let body = build_subs_v0(0x00_aa_55, &[(1, &[])]);
        let s = super::parse_subs(&body).unwrap();
        assert_eq!(s.flags, 0x00_aa_55);
    }

    /// A body too short to hold even the FullBox preamble + entry_count
    /// is rejected.
    #[test]
    fn parse_subs_too_short() {
        assert!(super::parse_subs(&[0u8; 7]).is_err());
    }

    /// A truncated sub-sample row (entry_count promises 1 entry, but the
    /// body runs out mid-sub-sample) is rejected rather than silently
    /// returning the partial table.
    #[test]
    fn parse_subs_truncated_subsample() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // FullBox preamble
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        body.extend_from_slice(&1u32.to_be_bytes()); // sample_delta
        body.extend_from_slice(&1u16.to_be_bytes()); // subsample_count = 1
        body.extend_from_slice(&[0u8; 3]); // truncated subsample (need 8)
        assert!(super::parse_subs(&body).is_err());
    }

    /// `parse_stbl` lands the `subs` on the track alongside the other
    /// sample-table boxes (proves the dispatch arm is wired).
    #[test]
    fn parse_stbl_picks_up_subs() {
        let subs_body = build_subs_v0(0, &[(1, &[(42, 0, 0, 0)])]);
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"subs", &subs_body));
        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.subs.len(), 1);
        assert_eq!(t.subs[0].entries[0].subsamples[0].subsample_size, 42);
    }

    /// Multiple `subs` boxes (distinct `flags` per §8.7.7.1) accumulate
    /// in encounter order on the track — we don't fail on, deduplicate,
    /// or otherwise touch the count: the spec explicitly permits several.
    #[test]
    fn parse_stbl_accumulates_multiple_subs() {
        let s1 = build_subs_v0(0, &[(1, &[(10, 0, 0, 0)])]);
        let s2 = build_subs_v0(1, &[(2, &[(20, 0, 0, 0)])]);
        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"subs", &s1));
        stbl.extend(wrap_box_full_size(b"subs", &s2));
        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.subs.len(), 2);
        assert_eq!(t.subs[0].flags, 0);
        assert_eq!(t.subs[1].flags, 1);
    }

    /// Surfacing: a parsed `subs` becomes one `subs_<n>` key on
    /// `params.options`. Verifies the `"v<version> flags=<f>"` prefix,
    /// the `delta=<d>:size,priority,discardable,csp` per-entry shape
    /// (with sub-samples joined by `;` and `csp` rendered as 8-digit
    /// hex), and the omission of the colon when an entry has zero
    /// sub-samples.
    #[test]
    fn build_stream_info_surfaces_subs_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.subs.push(super::SubsBox {
            version: 0,
            flags: 0x42,
            entries: vec![
                super::SubsEntry {
                    sample_delta: 1,
                    subsamples: vec![
                        super::SubSampleEntry {
                            subsample_size: 100,
                            subsample_priority: 5,
                            discardable: 0,
                            codec_specific_parameters: 0x0000_0001,
                        },
                        super::SubSampleEntry {
                            subsample_size: 50,
                            subsample_priority: 0,
                            discardable: 1,
                            codec_specific_parameters: 0x0000_0002,
                        },
                    ],
                },
                super::SubsEntry {
                    sample_delta: 3,
                    subsamples: Vec::new(),
                },
            ],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        let got = info.params.options.get("subs_0").unwrap();
        assert_eq!(
            got,
            "v0 flags=66 delta=1:100,5,0,00000001;50,0,1,00000002 delta=3"
        );
    }

    /// Absence: a track with no `subs` emits no `subs_<n>` keys.
    #[test]
    fn build_stream_info_no_subs_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("subs_0"), None);
    }

    // ISO/IEC 14496-12 §8.7.8 — `saiz` parsing. Body layout (after the
    // 4-byte FullBox header):
    //
    //   if (flags & 1) { u32 aux_info_type; u32 aux_info_type_parameter }
    //   u8  default_sample_info_size
    //   u32 sample_count
    //   if (default_sample_info_size == 0) { u8 sample_info_size[sample_count] }

    /// §8.7.8.2 with `flags & 1 == 0` and a non-zero
    /// `default_sample_info_size`: the per-sample table is omitted on
    /// disk (every sample has the constant size) and the
    /// `aux_info_type` block is absent (the implied value is the
    /// sample-entry FourCC or the protection scheme type — caller's
    /// job to apply that rule).
    #[test]
    fn parse_saiz_constant_size_no_aux_type_key() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version=0, flags=0
        body.push(16u8); // default_sample_info_size = 16
        body.extend_from_slice(&5u32.to_be_bytes()); // sample_count = 5
        let sz = super::parse_saiz(&body).expect("saiz parses");
        assert_eq!(sz.aux_info_type, None);
        assert_eq!(sz.aux_info_type_parameter, None);
        assert_eq!(sz.default_sample_info_size, 16);
        assert_eq!(sz.sample_count, 5);
        assert!(sz.per_sample.is_empty());
    }

    /// §8.7.8.2 with `flags & 1 == 1` (aux_info_type key present) and
    /// `default_sample_info_size == 0` (per-sample table populated).
    /// Matches the CENC case where each sample has a distinct IV +
    /// subsample-map record size.
    #[test]
    fn parse_saiz_variable_size_with_aux_type_key() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 1]); // version=0, flags=1
        body.extend_from_slice(b"cenc"); // aux_info_type
        body.extend_from_slice(&0u32.to_be_bytes()); // aux_info_type_parameter
        body.push(0u8); // default_sample_info_size = 0 → per-sample table
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count = 3
        body.extend_from_slice(&[12u8, 24u8, 36u8]); // sample_info_size[]
        let sz = super::parse_saiz(&body).expect("saiz parses");
        assert_eq!(sz.aux_info_type, Some(*b"cenc"));
        assert_eq!(sz.aux_info_type_parameter, Some(0));
        assert_eq!(sz.default_sample_info_size, 0);
        assert_eq!(sz.sample_count, 3);
        assert_eq!(sz.per_sample, vec![12, 24, 36]);
    }

    /// Truncation: a `sample_count` that names more bytes than the
    /// body actually carries must surface as `Error::invalid` (not a
    /// panic, not a partial vec). This is the same anti-DoS shape the
    /// fuzzer pinned for `subs`.
    #[test]
    fn parse_saiz_truncated_per_sample_returns_err() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version=0, flags=0
        body.push(0u8); // default_sample_info_size = 0 → per-sample table
        body.extend_from_slice(&100u32.to_be_bytes()); // claims 100 samples
        body.extend_from_slice(&[1u8, 2u8, 3u8]); // only 3 bytes follow
        let err = super::parse_saiz(&body).expect_err("truncation must err");
        assert!(format!("{err}").contains("MP4: saiz"));
    }

    // ISO/IEC 14496-12 §8.7.9 — `saio` parsing. Body layout (after the
    // 4-byte FullBox header):
    //
    //   if (flags & 1) { u32 aux_info_type; u32 aux_info_type_parameter }
    //   u32 entry_count
    //   if (version == 0) { u32 offset[entry_count] }
    //   else              { u64 offset[entry_count] }

    /// §8.7.9.2 v0 with `flags & 1 == 0` and a single offset entry —
    /// the common DASH/CMAF shape ("all aux-info for the segment is
    /// contiguous", per §8.7.9.3 + §8.8.14).
    #[test]
    fn parse_saio_v0_single_offset_no_aux_type() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version=0, flags=0
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        body.extend_from_slice(&0xdead_beefu32.to_be_bytes()); // offset
        let so = super::parse_saio(&body).expect("saio v0 parses");
        assert_eq!(so.version, 0);
        assert_eq!(so.aux_info_type, None);
        assert_eq!(so.aux_info_type_parameter, None);
        assert_eq!(so.offsets, vec![0xdead_beef]);
    }

    /// §8.7.9.2 v1 with the aux_info_type key + two 64-bit offsets.
    /// v1 is used when at least one offset exceeds 32 bits (typical
    /// for large mdat content).
    #[test]
    fn parse_saio_v1_two_64bit_offsets_with_aux_type() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 1]); // version=1, flags=1
        body.extend_from_slice(b"cenc");
        body.extend_from_slice(&7u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
        body.extend_from_slice(&0x0000_0001_0000_0000u64.to_be_bytes());
        body.extend_from_slice(&0xffff_ffff_ffff_0000u64.to_be_bytes());
        let so = super::parse_saio(&body).expect("saio v1 parses");
        assert_eq!(so.version, 1);
        assert_eq!(so.aux_info_type, Some(*b"cenc"));
        assert_eq!(so.aux_info_type_parameter, Some(7));
        assert_eq!(
            so.offsets,
            vec![0x0000_0001_0000_0000, 0xffff_ffff_ffff_0000]
        );
    }

    /// Truncation: a forged `entry_count` that exceeds the body size
    /// must err rather than panic / wrap. Confirms the anti-DoS budget
    /// cap (per_entry width × entry_count) holds.
    #[test]
    fn parse_saio_truncated_offsets_returns_err() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // v0, flags=0
        body.extend_from_slice(&100u32.to_be_bytes()); // claims 100 entries
        body.extend_from_slice(&1u32.to_be_bytes()); // only 1 follows
        let err = super::parse_saio(&body).expect_err("truncation must err");
        assert!(format!("{err}").contains("MP4: saio"));
    }

    /// `parse_stbl` dispatch wires both boxes onto the Track. Build a
    /// minimal `stbl` carrying one `saiz` (constant-size) + one
    /// `saio` (single absolute offset) and verify the Track captures
    /// both with their fields intact.
    #[test]
    fn parse_stbl_collects_saiz_and_saio() {
        // saiz body: flags=0, default_sample_info_size=8, sample_count=4.
        let mut saiz_body = Vec::new();
        saiz_body.extend_from_slice(&[0u8; 4]);
        saiz_body.push(8u8);
        saiz_body.extend_from_slice(&4u32.to_be_bytes());

        // saio body: v0, flags=0, entry_count=1, offset=0x1000.
        let mut saio_body = Vec::new();
        saio_body.extend_from_slice(&[0u8; 4]);
        saio_body.extend_from_slice(&1u32.to_be_bytes());
        saio_body.extend_from_slice(&0x1000u32.to_be_bytes());

        let mut stbl = Vec::new();
        stbl.extend(wrap_box_full_size(b"saiz", &saiz_body));
        stbl.extend(wrap_box_full_size(b"saio", &saio_body));

        let mut t = fresh_track();
        super::parse_stbl(&stbl, &mut t).unwrap();
        assert_eq!(t.saiz.len(), 1);
        assert_eq!(t.saiz[0].default_sample_info_size, 8);
        assert_eq!(t.saiz[0].sample_count, 4);
        assert_eq!(t.saio.len(), 1);
        assert_eq!(t.saio[0].version, 0);
        assert_eq!(t.saio[0].offsets, vec![0x1000]);
    }

    /// `build_stream_info` surfaces both boxes on `params.options`.
    /// Pair: a `saiz` carrying a `(cenc, 0)` key + variable per-sample
    /// table; a `saio` with the same key + two 64-bit offsets. The
    /// formatted strings follow the documented format.
    #[test]
    fn build_stream_info_surfaces_saiz_and_saio_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"encv";
        t.timescale = 1000;
        t.saiz.push(super::SaizBox {
            aux_info_type: Some(*b"cenc"),
            aux_info_type_parameter: Some(0),
            default_sample_info_size: 0,
            sample_count: 3,
            per_sample: vec![18, 18, 26],
        });
        t.saio.push(super::SaioBox {
            version: 1,
            aux_info_type: Some(*b"cenc"),
            aux_info_type_parameter: Some(0),
            offsets: vec![0x1_0000, 0x2_0000],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("saiz_0").unwrap(),
            "type=cenc param=0 default_size=0 count=3 sizes=18,18,26"
        );
        assert_eq!(
            info.params.options.get("saio_0").unwrap(),
            "v1 type=cenc param=0 offsets=65536,131072"
        );
    }

    /// Absence: a track with no `saiz` / `saio` emits no `saiz_<n>` or
    /// `saio_<n>` keys (mirrors the `no_subs_no_options` shape).
    #[test]
    fn build_stream_info_no_saiz_saio_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("saiz_0"), None);
        assert_eq!(info.params.options.get("saio_0"), None);
    }

    /// `saiz` with no aux_info_type key + constant size: surfaced
    /// without the `type=` / `param=` prefix and without `sizes=`.
    #[test]
    fn build_stream_info_surfaces_saiz_constant_no_key() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        t.codec_id_fourcc = *b"mp4a";
        t.timescale = 48_000;
        t.saiz.push(super::SaizBox {
            aux_info_type: None,
            aux_info_type_parameter: None,
            default_sample_info_size: 22,
            sample_count: 100,
            per_sample: Vec::new(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("saiz_0").unwrap(),
            "default_size=22 count=100"
        );
    }

    // ----- §8.8.2 mehd MovieExtendsHeaderBox parser ---------------------

    #[test]
    fn parse_mehd_v0_reads_32_bit_fragment_duration() {
        // FullBox(version=0, flags=0) + u32 fragment_duration
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let d = super::parse_mehd(&body).expect("v0 mehd parses");
        assert_eq!(d, 0xDEAD_BEEF);
    }

    #[test]
    fn parse_mehd_v1_reads_64_bit_fragment_duration() {
        // FullBox(version=1, flags=0) + u64 fragment_duration
        let mut body = vec![1u8, 0, 0, 0];
        let dur: u64 = 0x0123_4567_89AB_CDEF;
        body.extend_from_slice(&dur.to_be_bytes());
        let d = super::parse_mehd(&body).expect("v1 mehd parses");
        assert_eq!(d, dur);
    }

    #[test]
    fn parse_mehd_rejects_empty_body() {
        assert!(super::parse_mehd(&[]).is_err());
    }

    #[test]
    fn parse_mehd_rejects_truncated_v0() {
        // version=0, flags=0, only 2 of 4 duration bytes.
        let body = vec![0u8, 0, 0, 0, 0x12, 0x34];
        assert!(super::parse_mehd(&body).is_err());
    }

    #[test]
    fn parse_mehd_rejects_truncated_v1() {
        // version=1, flags=0, only 4 of 8 duration bytes.
        let body = vec![1u8, 0, 0, 0, 0, 0, 0, 0];
        assert!(super::parse_mehd(&body).is_err());
    }

    #[test]
    fn parse_mehd_rejects_unknown_version() {
        // The spec defines only versions 0 and 1; any other value is
        // a forged / corrupt box and must be refused (not best-effort
        // parsed — we don't know what the field layout is).
        let mut body = vec![2u8, 0, 0, 0];
        body.extend_from_slice(&0u64.to_be_bytes());
        assert!(super::parse_mehd(&body).is_err());
    }

    // ----- ISO/IEC 23001-7 §8.1.1 — moof-level pssh ---------------------

    /// Build a `moof` body containing one `mfhd` + N `pssh` children
    /// (no `traf` so we exercise just the pssh collection path).
    fn build_moof_body_with_pssh(seq: u32, psshes: &[Vec<u8>]) -> Vec<u8> {
        // mfhd: FullBox(version=0, flags=0) + u32 sequence_number.
        let mut mfhd_body = vec![0u8; 4];
        mfhd_body.extend_from_slice(&seq.to_be_bytes());
        let mfhd = wrap_box(b"mfhd", &mfhd_body);

        let mut body = Vec::new();
        body.extend_from_slice(&mfhd);
        for pssh_body in psshes {
            body.extend_from_slice(&wrap_box(b"pssh", pssh_body));
        }
        body
    }

    /// Wrap `body` with an 8-byte BoxHeader (size + fourcc).
    fn wrap_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(body);
        out
    }

    /// Construct a v0 `pssh` body: FullBox + 16-byte SystemID +
    /// u32 DataSize + Data.
    fn pssh_v0_body(system_id: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8; 4]; // version=0, flags=0
        body.extend_from_slice(system_id);
        body.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        body
    }

    /// Construct a v1 `pssh` body: FullBox + SystemID + KID_count + KIDs +
    /// DataSize + Data.
    fn pssh_v1_body(system_id: &[u8; 16], kids: &[[u8; 16]], data: &[u8]) -> Vec<u8> {
        let mut body = vec![1u8, 0, 0, 0]; // version=1, flags=0
        body.extend_from_slice(system_id);
        body.extend_from_slice(&(kids.len() as u32).to_be_bytes());
        for k in kids {
            body.extend_from_slice(k);
        }
        body.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        body
    }

    /// A moof containing one v0 pssh after the mfhd is captured into
    /// `moof_psshes` keyed by the mfhd sequence_number. Mirrors §8.1.1
    /// "pssh may be in moov or moof" and §8.1.1's reader rule that
    /// each fragment's pssh applies only to that fragment.
    #[test]
    fn parse_moof_collects_one_pssh_keyed_by_mfhd_sequence() {
        let sysid: [u8; 16] = [
            0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d,
            0x21, 0xed,
        ];
        let data = b"opaque-drm-blob".to_vec();
        let body = build_moof_body_with_pssh(7, &[pssh_v0_body(&sysid, &data)]);
        let moof = super::MoofRecord {
            moof_start: 0,
            body,
        };
        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = Vec::new();
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut moof_psshes: Vec<super::MoofPsshRecord> = Vec::new();
        super::parse_moof(
            &moof,
            &[],
            &mut samples,
            &mut next_dts,
            &mut senc,
            &mut sai,
            &mut moof_psshes,
        )
        .expect("moof walk succeeds");

        assert_eq!(moof_psshes.len(), 1);
        let r = &moof_psshes[0];
        assert_eq!(r.moof_sequence, 7);
        assert_eq!(r.pssh.version, 0);
        assert_eq!(r.pssh.system_id, sysid);
        assert!(r.pssh.kids.is_empty());
        assert_eq!(r.pssh.data, data);
    }

    /// A moof containing two pssh boxes (one v0, one v1 with two
    /// KIDs) — both records share the same `moof_sequence` (= mfhd
    /// sequence_number) and preserve the on-wire walk order. Exercises
    /// the per-fragment "multiple SystemIDs" shape called out in §8.1.1
    /// ("A single file MAY be constructed to be playable by multiple
    /// key and digital rights management systems").
    #[test]
    fn parse_moof_collects_two_psshes_v0_and_v1_in_walk_order() {
        let sysid_a: [u8; 16] = [0x11; 16];
        let sysid_b: [u8; 16] = [0x22; 16];
        let kid_x: [u8; 16] = [0xAA; 16];
        let kid_y: [u8; 16] = [0xBB; 16];
        let data_a = b"sys-a-data".to_vec();
        let data_b = b"sys-b-data-longer".to_vec();
        let body = build_moof_body_with_pssh(
            42,
            &[
                pssh_v0_body(&sysid_a, &data_a),
                pssh_v1_body(&sysid_b, &[kid_x, kid_y], &data_b),
            ],
        );
        let moof = super::MoofRecord {
            moof_start: 0,
            body,
        };
        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = Vec::new();
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut moof_psshes: Vec<super::MoofPsshRecord> = Vec::new();
        super::parse_moof(
            &moof,
            &[],
            &mut samples,
            &mut next_dts,
            &mut senc,
            &mut sai,
            &mut moof_psshes,
        )
        .expect("moof walk succeeds");

        assert_eq!(moof_psshes.len(), 2);
        // Same fragment scope on both.
        assert_eq!(moof_psshes[0].moof_sequence, 42);
        assert_eq!(moof_psshes[1].moof_sequence, 42);
        // Walk order preserved.
        assert_eq!(moof_psshes[0].pssh.version, 0);
        assert_eq!(moof_psshes[0].pssh.system_id, sysid_a);
        assert!(moof_psshes[0].pssh.kids.is_empty());
        assert_eq!(moof_psshes[0].pssh.data, data_a);

        assert_eq!(moof_psshes[1].pssh.version, 1);
        assert_eq!(moof_psshes[1].pssh.system_id, sysid_b);
        assert_eq!(moof_psshes[1].pssh.kids, vec![kid_x, kid_y]);
        assert_eq!(moof_psshes[1].pssh.data, data_b);
    }

    /// A pssh that appears before the mfhd inside a moof still
    /// receives the correct `moof_sequence` once mfhd is encountered.
    /// §8.8 lists `mfhd` as the first child of `moof` but does not
    /// forbid other top-level boxes preceding it in practice; the
    /// buffered-finalize approach (collect pssh bodies, then key by
    /// the post-walk captured sequence_number) handles either order
    /// without losing fidelity.
    #[test]
    fn parse_moof_pssh_before_mfhd_still_binds_to_sequence() {
        let sysid: [u8; 16] = [0xCC; 16];
        let data = b"pre-mfhd-pssh".to_vec();

        // Build moof body with pssh FIRST, then mfhd.
        let mut mfhd_body = vec![0u8; 4];
        mfhd_body.extend_from_slice(&99u32.to_be_bytes());
        let mfhd = wrap_box(b"mfhd", &mfhd_body);
        let pssh = wrap_box(b"pssh", &pssh_v0_body(&sysid, &data));
        let mut body = Vec::new();
        body.extend_from_slice(&pssh);
        body.extend_from_slice(&mfhd);

        let moof = super::MoofRecord {
            moof_start: 0,
            body,
        };
        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = Vec::new();
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut moof_psshes: Vec<super::MoofPsshRecord> = Vec::new();
        super::parse_moof(
            &moof,
            &[],
            &mut samples,
            &mut next_dts,
            &mut senc,
            &mut sai,
            &mut moof_psshes,
        )
        .expect("moof walk succeeds");

        assert_eq!(moof_psshes.len(), 1);
        assert_eq!(moof_psshes[0].moof_sequence, 99);
        assert_eq!(moof_psshes[0].pssh.system_id, sysid);
        assert_eq!(moof_psshes[0].pssh.data, data);
    }

    /// A malformed pssh inside a moof (here a truncated body) is
    /// dropped without aborting the moof walk — mirrors the moov-level
    /// recovery policy: a forged DRM box must not brick the demuxer
    /// for unrelated playback (or for any non-decrypting workflow).
    #[test]
    fn parse_moof_drops_malformed_pssh_silently() {
        // Truncated pssh body: just FullBox header, no SystemID/data.
        let bad = vec![0u8; 4];
        let body = build_moof_body_with_pssh(3, &[bad]);
        let moof = super::MoofRecord {
            moof_start: 0,
            body,
        };
        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = Vec::new();
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut moof_psshes: Vec<super::MoofPsshRecord> = Vec::new();
        super::parse_moof(
            &moof,
            &[],
            &mut samples,
            &mut next_dts,
            &mut senc,
            &mut sai,
            &mut moof_psshes,
        )
        .expect("walk succeeds despite bad pssh");
        assert!(moof_psshes.is_empty(), "malformed pssh dropped silently");
    }

    // ----- §8.8.3.1 sample_flags typed accessor -----------------------------

    #[test]
    fn sample_flags_all_zero_decodes_to_sync_sample() {
        // §8.8.3.1: when the field is 0 every 2-bit/3-bit/16-bit
        // subfield is "unknown" / zero and the sync-sample bit is
        // clear → sample IS a sync sample.
        let f = super::SampleFlags::from_u32(0);
        assert_eq!(f.is_leading, 0);
        assert_eq!(f.sample_depends_on, 0);
        assert_eq!(f.sample_is_depended_on, 0);
        assert_eq!(f.sample_has_redundancy, 0);
        assert_eq!(f.sample_padding_value, 0);
        assert!(!f.sample_is_non_sync_sample);
        assert!(f.is_sync_sample());
        assert_eq!(f.sample_degradation_priority, 0);
    }

    #[test]
    fn sample_flags_non_sync_bit_only() {
        // The widely-used "non-sync sample" marker — every other
        // subfield zero, just bit 16 set. Sentinel: 0x0001_0000.
        let f = super::SampleFlags::from_u32(0x0001_0000);
        assert!(f.sample_is_non_sync_sample);
        assert!(!f.is_sync_sample());
        assert_eq!(f.is_leading, 0);
        assert_eq!(f.sample_depends_on, 0);
        assert_eq!(f.sample_degradation_priority, 0);
    }

    #[test]
    fn sample_flags_i_picture_pattern() {
        // §8.6.4.3 / §8.8.3.1 convention for a typical I-picture
        // (used by every fMP4 producer for the SAP-1 frame of a
        // fragment): is_leading=2 (not leading),
        // sample_depends_on=2 (does not depend on others →
        // I-picture), sample_is_depended_on=1 (others may depend),
        // sample_has_redundancy=0 (unknown), padding=0,
        // non_sync=0, priority=0.
        //
        // Bit packing (§8.8.3.1, per-field shifts):
        //   is_leading           << 26 = 2 << 26 = 0x0800_0000
        //   sample_depends_on    << 24 = 2 << 24 = 0x0200_0000
        //   sample_is_depended_on<< 22 = 1 << 22 = 0x0040_0000
        //   everything else                       = 0
        //   total                                 = 0x0A40_0000
        let raw: u32 = 0x0A40_0000;
        let f = super::SampleFlags::from_u32(raw);
        assert_eq!(f.is_leading, 2);
        assert_eq!(f.sample_depends_on, 2);
        assert_eq!(f.sample_is_depended_on, 1);
        assert_eq!(f.sample_has_redundancy, 0);
        assert!(!f.sample_is_non_sync_sample);
        assert!(f.is_sync_sample());
        assert_eq!(f.sample_padding_value, 0);
        assert_eq!(f.sample_degradation_priority, 0);
        // Round-trip — reserved bits 31..28 stay zero per §8.8.3.1.
        assert_eq!(f.to_u32(), raw);
    }

    #[test]
    fn sample_flags_b_picture_pattern() {
        // Typical B-picture pattern in fMP4:
        // is_leading=0 (unknown), sample_depends_on=1 (depends on
        // others), sample_is_depended_on=2 (no one depends → safely
        // disposable), redundancy=0, padding=0, non_sync=1,
        // priority=0.
        //
        // Bit packing (§8.8.3.1, per-field shifts):
        //   sample_depends_on    << 24 = 1 << 24 = 0x0100_0000
        //   sample_is_depended_on<< 22 = 2 << 22 = 0x0080_0000
        //   sample_is_non_sync_sample  = bit 16   = 0x0001_0000
        //   total                                 = 0x0181_0000
        let raw: u32 = 0x0181_0000;
        let f = super::SampleFlags::from_u32(raw);
        assert_eq!(f.is_leading, 0);
        assert_eq!(f.sample_depends_on, 1);
        assert_eq!(f.sample_is_depended_on, 2);
        assert_eq!(f.sample_has_redundancy, 0);
        assert!(f.sample_is_non_sync_sample);
        assert!(!f.is_sync_sample());
        assert_eq!(f.sample_padding_value, 0);
        assert_eq!(f.sample_degradation_priority, 0);
        assert_eq!(f.to_u32(), raw);
    }

    #[test]
    fn sample_flags_degradation_priority_field_is_low_16_bits() {
        // Carry only the low 16 bits — exercises the degradation
        // priority decode path independently. 0xBEEF is arbitrary
        // and chosen so the byte boundary is visible.
        let f = super::SampleFlags::from_u32(0x0000_BEEF);
        assert_eq!(f.sample_degradation_priority, 0xBEEF);
        assert_eq!(f.is_leading, 0);
        assert_eq!(f.sample_depends_on, 0);
        assert_eq!(f.sample_is_depended_on, 0);
        assert_eq!(f.sample_has_redundancy, 0);
        assert_eq!(f.sample_padding_value, 0);
        assert!(!f.sample_is_non_sync_sample);
    }

    #[test]
    fn sample_flags_padding_field_round_trips() {
        // sample_padding_value is 3 bits — values 0..=7 are legal.
        // Exercise the full range to catch a shift / mask bug in
        // the bits 19..17 window.
        for pad in 0u8..=7 {
            let f = super::SampleFlags {
                sample_padding_value: pad,
                ..Default::default()
            };
            let round = super::SampleFlags::from_u32(f.to_u32());
            assert_eq!(round.sample_padding_value, pad);
            assert_eq!(round, f);
        }
    }

    #[test]
    fn sample_flags_reserved_bits_ignored_on_decode() {
        // §8.8.3.1 fixes bits 31..28 to zero, but we tolerate a
        // producer that sets them (silent-recovery posture matching
        // the rest of the demuxer). A decode then re-encode strips
        // the reserved bits back to zero.
        let raw = 0xF000_0000_u32 | 0x0001_0000_u32; // reserved + non-sync
        let f = super::SampleFlags::from_u32(raw);
        assert!(f.sample_is_non_sync_sample);
        // Re-encoding emits clean reserved=0.
        assert_eq!(f.to_u32(), 0x0001_0000);
    }

    #[test]
    fn sample_flags_all_field_widths_round_trip() {
        // Saturate every field at its maximum legal value to confirm
        // no field bleeds into a neighbour's window.
        let f = super::SampleFlags {
            is_leading: 3,
            sample_depends_on: 3,
            sample_is_depended_on: 3,
            sample_has_redundancy: 3,
            sample_padding_value: 7,
            sample_is_non_sync_sample: true,
            sample_degradation_priority: 0xFFFF,
        };
        let raw = f.to_u32();
        // reserved=0000 then five-pairs-saturated then padding=111
        // then non_sync=1 then priority=0xFFFF
        // = 0000 11 11 11 11 111 1 1111111111111111
        // = 0x0FFF_FFFF
        assert_eq!(raw, 0x0FFF_FFFF);
        assert_eq!(super::SampleFlags::from_u32(raw), f);
    }

    #[test]
    fn parse_sample_flags_public_helper_matches_struct() {
        // Confirms the free-function entry point routes through
        // SampleFlags::from_u32 unchanged.
        let raw = 0x0244_0000_u32;
        assert_eq!(
            super::parse_sample_flags(raw),
            super::SampleFlags::from_u32(raw)
        );
    }
}
