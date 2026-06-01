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
    let mut tfras: Vec<TfraRecord> = Vec::new();
    let mut prfts: Vec<PrftRecord> = Vec::new();
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
    for moof in &moofs {
        parse_moof(
            moof,
            &parsed.tracks,
            &mut samples,
            &mut next_dts,
            &mut senc_records,
            &mut sai_records,
        )?;
    }

    samples.sort_by_key(|s| s.offset);

    // Movie duration from mvhd, translated into microseconds.
    let duration_micros: i64 = if parsed.movie_timescale > 0 && parsed.movie_duration > 0 {
        (parsed.movie_duration as i128 * 1_000_000 / parsed.movie_timescale as i128) as i64
    } else {
        0
    };

    // Surface any parsed `prft` ProducerReferenceTimeBoxes through the
    // container metadata channel as `prft_<n>` (0-based file order)
    // with value "reference_track_ID ntp_timestamp media_time" — three
    // decimal integers, space-separated, mirroring the
    // `tref_<type>` / `sgpd_<n>` conventions used elsewhere in this
    // crate. Callers wanting the structured record can use the public
    // `parse_prft_box` entry point. Absent prft, no keys are emitted.
    let mut metadata = parsed.metadata;
    for (n, p) in prfts.iter().enumerate() {
        metadata.push((
            format!("prft_{n}"),
            format!(
                "{} {} {}",
                p.reference_track_id, p.ntp_timestamp, p.media_time
            ),
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

    Ok(Box::new(Mp4Demuxer {
        input,
        streams,
        samples,
        cursor: 0,
        metadata,
        duration_micros,
        sidxes,
        tfras,
        prfts,
        psshes: parsed.psshes,
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

#[derive(Default)]
struct ParsedMoov {
    tracks: Vec<Track>,
    movie_timescale: u32,
    movie_duration: u64,
    metadata: Vec<(String, String)>,
    /// §8.1 (ISO/IEC 23001-7) — `pssh`
    /// ProtectionSystemSpecificHeaderBox entries collected at moov
    /// level. Each entry corresponds to one DRM system signalled in
    /// the file by SystemID UUID. moof-level `pssh` instances are
    /// also permitted by the spec but the surface here is moov-only;
    /// fragment-level pssh can be added without changing the demuxer
    /// API.
    psshes: Vec<PsshBox>,
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
    /// §8.6.1.4 — `cslg` (CompositionToDecodeBox). Present when signed
    /// composition offsets (a v1 `ctts`) are in use and the producer
    /// chose to document the composition↔decode timeline relationship.
    /// `None` when the track has no `cslg` (the common case for files
    /// without B-frame reordering, or files that simply omit the box).
    cslg: Option<CslgBox>,
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
    /// §8.6.4 — `sdtp` (SampleDependencyTypeBox). Per-sample dependency
    /// hints in decode order: one entry per sample, four 2-bit fields
    /// `(is_leading, sample_depends_on, sample_is_depended_on,
    /// sample_has_redundancy)` packed into a single byte each on disk.
    /// Empty when the track has no `sdtp` (the common case); a present
    /// table whose length is shorter than the track's sample_count
    /// is preserved verbatim (truncation is the producer's bug, not
    /// ours to invent zeros for).
    sdtp: Vec<SdtpEntry>,
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
            _ => skip_cursor_bytes(&mut cur, psz),
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
        protection_scheme: None,
        tenc: None,
        tref: Vec::new(),
        elng: None,
        kinds: Vec::new(),
        cslg: None,
        stsh: Vec::new(),
        sbgp: Vec::new(),
        sgpd: Vec::new(),
        sdtp: Vec::new(),
        subs: Vec::new(),
        saiz: Vec::new(),
        saio: Vec::new(),
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
/// track-level container additionally hosts `kind` (§8.10.4) plus —
/// historically — per-track copyright / title overrides. We only
/// implement `kind` here because that is the box whose presence has a
/// well-defined effect on downstream stream routing; other children are
/// skipped so a future round can add them without changing the entry
/// point.
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
        if hdr.fourcc == KIND {
            parse_kind(&body[start..start + psz], t);
        }
        // Other track-udta children (e.g. legacy `titl` overrides) are
        // skipped — file-wide metadata is collected from the moov-level
        // udta by `parse_udta`.
    }
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
            _ => skip_cursor_bytes(&mut cur, psz),
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
            STSH => t.stsh = parse_stsh(&b)?,
            SDTP => t.sdtp = parse_sdtp(&b)?,
            CTTS => t.ctts = parse_ctts(&b)?,
            CSLG => t.cslg = Some(parse_cslg(&b)?),
            SBGP => t.sbgp.push(parse_sbgp(&b)?),
            SGPD => t.sgpd.push(parse_sgpd(&b)?),
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
    let mut cur = std::io::Cursor::new(&body[8..]);
    let hdr = match read_box_header(&mut cur)? {
        Some(h) => h,
        None => return Err(Error::invalid("MP4: stsd first entry missing")),
    };
    let psz = hdr.payload_size().unwrap_or(0) as usize;
    let entry = read_bytes_vec(&mut cur, psz)?;
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
fn parse_moof(
    moof: &MoofRecord,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
    senc_records: &mut Vec<SencRecord>,
    sai_records: &mut Vec<SaiRecord>,
) -> Result<()> {
    let mut cur = std::io::Cursor::new(&moof.body);
    let end = moof.body.len() as u64;
    let mut moof_sequence: u32 = 0;
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
            _ => skip_cursor_bytes(&mut cur, psz),
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
    /// ISO/IEC 23001-7 §8.1 pssh entries collected from moov. One per
    /// DRM system signalled in the file; the demuxer does not consume
    /// them but a downstream decryption layer can look up its
    /// SystemID match here.
    #[allow(dead_code)]
    psshes: Vec<PsshBox>,
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
    /// `pssh` entries inside individual `moof` boxes are also permitted
    /// by the spec — they are not yet collected here; an r197 follow-up
    /// can extend `SencRecord` / `parse_moof` to surface them.
    #[allow(dead_code)]
    pub fn psshes(&self) -> &[PsshBox] {
        &self.psshes
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
            elng: None,
            kinds: Vec::new(),
            cslg: None,
            stsh: Vec::new(),
            sbgp: Vec::new(),
            sgpd: Vec::new(),
            sdtp: Vec::new(),
            subs: Vec::new(),
            saiz: Vec::new(),
            saio: Vec::new(),
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
}
