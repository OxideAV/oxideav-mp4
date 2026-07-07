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

pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(open_typed(input, codecs)?))
}

/// Open an MP4 / ISOBMFF input and return the concrete [`Mp4Demuxer`] —
/// same parse as [`open`], but the typed form reaches the inherent
/// accessors that aren't part of the [`Demuxer`] trait (the structured
/// `sidx` / `ssix` / `tfra` / `pssh` / CENC `senc` / `saiz`+`saio` /
/// fragment-local sample-group records a decrypting or indexing layer
/// consumes). The `Box<dyn Demuxer>` form returned by [`open`] is what
/// the registry uses.
pub fn open_typed(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Mp4Demuxer> {
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
    // QuickTime Preview Atom (`pnot`) — a top-level atom locating the
    // movie's preview (poster) image. Quantity zero or one; the first
    // instance seen wins.
    let mut pnot: Option<PnotRecord> = None;
    // §8.11 — file-level `meta` box item infrastructure (HEIF / MIAF).
    // Quantity zero or one at the top level; the first instance wins. A
    // `meta` may also appear inside `moov` / `trak` (handled in
    // `parse_moov` / `parse_trak`); the file-level one is the HEIF home.
    let mut meta_items = MetaItems::default();
    // §8.11.7 — file-level `meco` AdditionalMetadataContainerBox. Quantity
    // zero or one at each level (§8.11.7.1); the first instance wins. Holds
    // additional `meta` boxes (distinct handler types) plus `mere`
    // relations. Default-empty when no `meco` is present.
    let mut meco: MecoBox = MecoBox::default();
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
            // QuickTime Preview Atom (`pnot`) — a top-level plain `Box`
            // locating the movie's preview (poster) image. The first
            // parseable instance wins; a malformed one is dropped
            // (informational) rather than aborting the open.
            PNOT => {
                let body = read_box_body(&mut *input, &hdr)?;
                if pnot.is_none() {
                    pnot = parse_pnot(&body);
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
            // §8.11 — file-level `meta` box (HEIF / MIAF item catalogue).
            // Parse the §8.11 item infrastructure (pitm / iloc / iinf /
            // iref) the first time one is seen; subsequent copies (a
            // malformed file with two file-level meta boxes) are skipped.
            META => {
                let body = read_box_body(&mut *input, &hdr)?;
                if meta_items.is_empty() && meta_items.handler_type == [0; 4] {
                    meta_items = parse_meta_items(&body);
                }
            }
            // §8.11.7 — file-level `meco` AdditionalMetadataContainerBox.
            // Parse the first instance's additional `meta` boxes + `mere`
            // relations; subsequent copies are skipped.
            MECO => {
                let body = read_box_body(&mut *input, &hdr)?;
                if meco.is_empty() {
                    meco = parse_meco_box(&body);
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
    let mut traf_sample_groups: Vec<TrafSampleGroupRecord> = Vec::new();
    for moof in &moofs {
        parse_moof(
            moof,
            &parsed.tracks,
            &mut samples,
            &mut next_dts,
            &mut senc_records,
            &mut sai_records,
            &mut moof_psshes,
            &mut traf_sample_groups,
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

    // Surface a QuickTime Preview Atom (`pnot`) on the flat metadata
    // channel as `pnot` with value "<atom_type> <atom_index>
    // mod=<modification_date>" — the FourCC of the atom that holds the
    // preview data (e.g. `PICT`), which instance of that type to use, and
    // the Macintosh-format modification date. The structured record is
    // reachable via `Mp4Demuxer::pnot()` (or the public `parse_pnot_box`).
    // Absent `pnot`, no key is emitted.
    if let Some(p) = pnot.as_ref() {
        metadata.push((
            "pnot".to_string(),
            format!(
                "{} {} mod={}",
                String::from_utf8_lossy(&p.atom_type),
                p.atom_index,
                p.modification_date
            ),
        ));
    }

    // Surface the file-level `meta` box item infrastructure (§8.11) on
    // the flat metadata channel. The structured records are reachable
    // via `Mp4Demuxer::meta_items()`; the flat surface mirrors the rest
    // of this crate's string-keyed bag for tooling that prefers it.
    // Emitted (only when the corresponding box was present):
    //   - `meta_handler` = the `meta`'s hdlr handler_type FourCC
    //   - `meta_primary_item` = the `pitm` primary item_ID
    //   - `meta_item_count` = number of `iinf` `infe` entries
    //   - `meta_item_<n>` = "id=<id> type=<fourcc|v0/1> name=<name>"
    //   - `meta_iloc_count` = number of `iloc` item-location records
    //   - `meta_iref_count` = number of `iref` reference groups
    surface_meta_items(&meta_items, &mut metadata);

    // Surface a file-level `meco` (AdditionalMetadataContainerBox,
    // §8.11.7) on the flat metadata channel. Emitted only when a `meco`
    // was present and non-empty:
    //   - `meco_meta_count` = number of additional `meta` boxes
    //   - `meco_meta_<n>` = the additional `meta`'s handler_type FourCC
    //   - `meco_relation_count` = number of `mere` relation boxes
    //   - `meco_relation_<n>` = "<first>-<second>=<relation>" (the two
    //     handler-type FourCCs and the §8.11.8.3 relation value)
    // Structured records via `Mp4Demuxer::meco()`.
    if !meco.is_empty() {
        if !meco.metas.is_empty() {
            metadata.push(("meco_meta_count".to_string(), meco.metas.len().to_string()));
            for (n, m) in meco.metas.iter().enumerate() {
                metadata.push((
                    format!("meco_meta_{n}"),
                    String::from_utf8_lossy(&m.handler_type).to_string(),
                ));
            }
        }
        if !meco.relations.is_empty() {
            metadata.push((
                "meco_relation_count".to_string(),
                meco.relations.len().to_string(),
            ));
            for (n, r) in meco.relations.iter().enumerate() {
                metadata.push((
                    format!("meco_relation_{n}"),
                    format!(
                        "{}-{}={}",
                        String::from_utf8_lossy(&r.first_metabox_handler_type),
                        String::from_utf8_lossy(&r.second_metabox_handler_type),
                        r.metabox_relation
                    ),
                ));
            }
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
            // The one base-spec-defined child is `assp` (§8.8.16); when
            // it decoded cleanly, append its min_initial_alt_startup
            // offset(s) so a consumer reading the flat channel sees the
            // alternative-startup bound without reaching for the typed
            // record. v0 → "assp(off=<i>)"; v1 → one
            // "<grouping_type_parameter>:<offset>" token per entry,
            // e.g. "assp(0:-5 1:0)".
            if let Some(a) = &c.assp {
                if a.version == 0 {
                    if let Some(e) = a.entries.first() {
                        let _ = write!(s, "(off={})", e.min_initial_alt_startup_offset);
                    }
                } else {
                    let _ = write!(s, "(");
                    for (i, e) in a.entries.iter().enumerate() {
                        if i > 0 {
                            let _ = write!(s, " ");
                        }
                        let _ = write!(
                            s,
                            "{}:{}",
                            e.grouping_type_parameter.unwrap_or(0),
                            e.min_initial_alt_startup_offset
                        );
                    }
                    let _ = write!(s, ")");
                }
            }
        }
        metadata.push((format!("trep_{n}"), s));
    }

    // Surface any parsed `prft` ProducerReferenceTimeBoxes through the
    // container metadata channel as `prft_<n>` (0-based file order)
    // with value "reference_track_ID ntp_timestamp media_time" — three
    // decimal integers, space-separated, mirroring the
    // `tref_<type>` / `sgpd_<n>` conventions used elsewhere in this
    // crate. When the 2022-edition `flags` annotation bits are set, a
    // trailing space-separated token list of the set names
    // (`encoder_input_output`, `finalization_time`, `file_write_time`,
    // `arbitrary_association`, `realtime_offset`) is appended; the
    // three-integer prefix stays backward-compatible. Callers wanting the
    // structured record can use the public `parse_prft_box` entry point.
    // Absent prft, no keys are emitted.
    for (n, p) in prfts.iter().enumerate() {
        let mut value = format!(
            "{} {} {}",
            p.reference_track_id, p.ntp_timestamp, p.media_time
        );
        // `realtime_offset` is the combined 0x18 mask; surface it instead
        // of its two component bits when both are set.
        if p.is_realtime_offset() {
            value.push_str(" realtime_offset");
        } else {
            if p.is_finalization_time() {
                value.push_str(" finalization_time");
            }
            if p.is_arbitrary_association() {
                value.push_str(" arbitrary_association");
            }
        }
        if p.is_encoder_input_output() {
            value.push_str(" encoder_input_output");
        }
        if p.is_file_write_time() {
            value.push_str(" file_write_time");
        }
        metadata.push((format!("prft_{n}"), value));
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

    // §8.9 — per-fragment sample-group summary, one `frag_sample_group_<n>`
    // key per `traf` that carried at least one `sgpd` / `sbgp` / `csgp`.
    // Format: "track=<t> seq=<s> sgpd=<n> sbgp=<m> csgp=<k>" so a caller
    // knows the box counts on this fragment without consuming the public
    // record vector. Structured records are reachable via
    // `Mp4Demuxer::traf_sample_groups()`.
    for (n, r) in traf_sample_groups.iter().enumerate() {
        metadata.push((
            format!("frag_sample_group_{n}"),
            format!(
                "track={} seq={} sgpd={} sbgp={} csgp={}",
                r.track_idx,
                r.moof_sequence,
                r.sgpd.len(),
                r.sbgp.len(),
                r.csgp.len(),
            ),
        ));
    }

    Ok(Mp4Demuxer {
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
        pnot,
        leva: parsed.leva,
        treps: parsed.treps,
        psshes: parsed.psshes,
        moof_psshes,
        senc_records,
        sai_records,
        traf_sample_groups,
        meta_items,
        meco,
        movie_timescale: parsed.movie_timescale,
        track_timescales: parsed.tracks.iter().map(|t| t.timescale).collect(),
        track_ids: parsed.tracks.iter().map(|t| t.track_id).collect(),
    })
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
    /// 24-bit FullBox `flags`. In the 2015 edition this is always `0`; the
    /// 2022 edition defines named bits that annotate what the NTP time
    /// represents (the body layout is unchanged either way). Preserved
    /// verbatim so a 2022-aware consumer can interpret it via
    /// [`PrftRecord::is_encoder_input_output`] / [`PrftRecord::is_finalization_time`]
    /// / [`PrftRecord::is_file_write_time`] / [`PrftRecord::is_arbitrary_association`]
    /// / [`PrftRecord::is_realtime_offset`]; a 2015-era consumer ignores it.
    pub flags: u32,
}

impl PrftRecord {
    /// `encoder_input_output` (`flags & 0x000001`, 2022 edition): the NTP
    /// time is the frame's encoder input/output time.
    pub fn is_encoder_input_output(&self) -> bool {
        self.flags & 0x00_0001 != 0
    }

    /// `finalization_time` (`flags & 0x000002`, 2022 edition): the NTP
    /// time is the finalization (segment-complete) time.
    pub fn is_finalization_time(&self) -> bool {
        self.flags & 0x00_0002 != 0
    }

    /// `file_write_time` (`flags & 0x000004`, 2022 edition): the NTP time
    /// is the file/segment write time.
    pub fn is_file_write_time(&self) -> bool {
        self.flags & 0x00_0004 != 0
    }

    /// `arbitrary_association` (`flags & 0x000008`, 2022 edition): an
    /// arbitrary association rather than the default "next moof".
    pub fn is_arbitrary_association(&self) -> bool {
        self.flags & 0x00_0008 != 0
    }

    /// `realtime_offset` (`flags & 0x000018`, 2022 edition): the combined
    /// value used for real-time-offset signalling. This is a combined
    /// mask, so all of its set bits must be present.
    pub fn is_realtime_offset(&self) -> bool {
        self.flags & 0x00_0018 == 0x00_0018
    }
}

/// Decoded `amve` (AmbientViewingEnvironmentBox, ISO/IEC 14496-12
/// post-2015 addition).
///
/// A plain `Box` (NOT a `FullBox` — there is no `version` / `flags`
/// byte) carried inside a `VisualSampleEntry` (e.g. `avc1` / `hvc1` /
/// `av01`). It signals the nominal ambient viewing environment for the
/// display of the track's video — the same three syntax elements, with
/// the same units and ranges, as the `ambient_viewing_environment` SEI
/// message and the ISO/IEC 23091-3 (CICP) ambient-viewing-environment
/// parameters. The body is a fixed 8 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmveRecord {
    /// Environmental illuminance of the ambient viewing environment, in
    /// units of 0.0001 lux (i.e. `lux = ambient_illuminance / 10000`).
    /// `0` is permitted by the syntax but generally treated as
    /// "unknown" / unspecified by consumers.
    pub ambient_illuminance: u32,
    /// Normalized CIE 1931 *x* chromaticity of the environmental ambient
    /// light, in increments of 0.00002 (`x = ambient_light_x × 0.00002`).
    /// The spec range is 0..=50000 (0.0 .. 1.0); the raw u16 is preserved
    /// verbatim so a producer slip out of range round-trips unchanged.
    pub ambient_light_x: u16,
    /// Normalized CIE 1931 *y* chromaticity of the environmental ambient
    /// light, in increments of 0.00002 (`y = ambient_light_y × 0.00002`).
    /// Range as for `ambient_light_x`.
    pub ambient_light_y: u16,
}

/// Decoded `btrt` (BitRateBox, ISO/IEC 14496-12 §8.5.2).
///
/// A plain `Box` (NOT a `FullBox` — there is no `version` / `flags`
/// byte) that may appear, optionally, at the end of any `SampleEntry`
/// (video / audio / metadata / text). The body is a fixed 12 bytes of
/// three big-endian `u32`s: the decoding buffer size and the elementary
/// stream's maximum / average bit rate. It carries the same three
/// bandwidth quantities that the MPEG-4 `esds`
/// `DecoderConfigDescriptor` carries (`bufferSizeDB` / `maxBitrate` /
/// `avgBitrate`), but in a codec-agnostic box usable by sample entries
/// that do not carry an `esds` (e.g. `avc1` / `hvc1` / `av01` video,
/// `Opus` / `fLaC` audio).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtrtRecord {
    /// Size of the decoding buffer for the elementary stream, in bytes
    /// (§8.5.2.3 `bufferSizeDB`).
    pub buffer_size_db: u32,
    /// Maximum rate, in bits/second, over any window of one second
    /// (§8.5.2.3 `maxBitrate`).
    pub max_bitrate: u32,
    /// Average rate, in bits/second, over the entire presentation
    /// (§8.5.2.3 `avgBitrate`).
    pub avg_bitrate: u32,
}

/// Decoded `pasp` (PixelAspectRatioBox, ISO/IEC 14496-12 §12.1.4). A
/// plain `Box` (no FullBox preamble) inside a `VisualSampleEntry`
/// declaring the relative width / height of a pixel — the ratio a
/// renderer applies to obtain the display aspect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaspRecord {
    /// `hSpacing` — relative width of a pixel (§12.1.4.3).
    pub h_spacing: u32,
    /// `vSpacing` — relative height of a pixel (§12.1.4.3). Same units
    /// as `h_spacing`.
    pub v_spacing: u32,
}

/// Decoded `clap` (CleanApertureBox, ISO/IEC 14496-12 §12.1.4). A plain
/// `Box` inside a `VisualSampleEntry` giving the clean-aperture
/// rectangle as four (numerator, denominator) fractions: the active
/// picture region after overscan / edge artefacts are cropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClapRecord {
    /// `cleanApertureWidthN` / `cleanApertureWidthD` — clean aperture
    /// width as a fraction in counted pixels (§12.1.4.3). `D` is positive.
    pub width_n: u32,
    pub width_d: u32,
    /// `cleanApertureHeightN` / `cleanApertureHeightD` — clean aperture
    /// height fraction (§12.1.4.3). `D` is positive.
    pub height_n: u32,
    pub height_d: u32,
    /// `horizOffN` / `horizOffD` — horizontal offset of the clean
    /// aperture centre minus `(width-1)/2`, as a fraction (typically 0).
    pub horiz_off_n: u32,
    pub horiz_off_d: u32,
    /// `vertOffN` / `vertOffD` — vertical offset of the clean aperture
    /// centre minus `(height-1)/2`, as a fraction (typically 0).
    pub vert_off_n: u32,
    pub vert_off_d: u32,
}

/// Decoded `colr` (ColourInformationBox, ISO/IEC 14496-12 §12.1.5). A
/// plain `Box` inside a `VisualSampleEntry`. The first 32 bits are a
/// `colour_type`; the payload that follows depends on it. The three
/// base-spec types are modelled; an unknown type keeps the raw payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColrRecord {
    /// `nclx` (§12.1.5.2) — on-screen colours: `colour_primaries`,
    /// `transfer_characteristics`, `matrix_coefficients` (each a
    /// 16-bit code from ISO/IEC 23091-2), plus a `full_range_flag`.
    Nclx {
        colour_primaries: u16,
        transfer_characteristics: u16,
        matrix_coefficients: u16,
        full_range: bool,
    },
    /// `rICC` (§12.1.5.2) — a restricted ICC profile (Monochrome or
    /// Three-Component Matrix-Based input profile); raw profile bytes.
    RestrictedIcc(Vec<u8>),
    /// `prof` (§12.1.5.2) — an unrestricted ICC profile; raw bytes.
    UnrestrictedIcc(Vec<u8>),
    /// Any other `colour_type`; the FourCC + the bytes that followed it.
    Other { colour_type: [u8; 4], data: Vec<u8> },
}

/// Decoded `stvi` (StereoVideoBox, ISO/IEC 14496-12 §8.15.4.2).
///
/// A `FullBox(version = 0, flags = 0)` that sits inside the
/// `schi` (SchemeInformationBox) of a sample entry's `sinf` when the
/// restricted-scheme SchemeType is `stvi` (stereoscopic video,
/// §8.15.4.1). It indicates that decoded frames carry either two
/// spatially packed constituent frames forming a stereo pair (frame
/// packing) or one of two views of a stereo pair (left / right in
/// different tracks).
///
/// The body is a packed `template int(30) reserved` plus `int(2)
/// single_view_allowed`, then `int(32) stereo_scheme`, `int(32) length`,
/// the `int(8)[length] stereo_indication_type` array, and optional
/// trailing boxes. The container preserves `single_view_allowed`,
/// `stereo_scheme`, and the raw `stereo_indication_type` bytes verbatim;
/// their detailed interpretation is `stereo_scheme`-specific (§8.15.4.2.3
/// routes to a table in ISO/IEC 14496-10 / 13818-2 / 23000-11) and is
/// left to a downstream consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StviRecord {
    /// `single_view_allowed` (§8.15.4.2.3), the low 2 bits after the
    /// 30-bit reserved field. `0` → content may only be displayed on a
    /// stereoscopic display; bit 0 (`& 1`) set → the right view may be
    /// shown on a monoscopic single-view display; bit 1 (`& 2`) set →
    /// the left view may be shown on a monoscopic single-view display.
    pub single_view_allowed: u8,
    /// `stereo_scheme` (§8.15.4.2.3) — the stereo arrangement scheme.
    /// `1` = the frame-packing-arrangement SEI scheme of ISO/IEC
    /// 14496-10; `2` = the Annex L arrangement-type scheme of ISO/IEC
    /// 13818-2; `3` = the ISO/IEC 23000-11 scheme. Other values are
    /// reserved; the raw integer is preserved verbatim.
    pub stereo_scheme: u32,
    /// The `stereo_indication_type` byte string (`length` bytes), whose
    /// syntax depends on `stereo_scheme` (§8.15.4.2.3). Preserved
    /// verbatim — for scheme 1/2 it is a big-endian `u32`
    /// arrangement-type code, for scheme 3 it is two `u8`s
    /// (composition-type + `is_left_first` in the LSB of the second).
    pub stereo_indication_type: Vec<u8>,
}

impl StviRecord {
    /// `true` when the right view of the stereo pair may be displayed on
    /// a monoscopic single-view display (`single_view_allowed & 1`,
    /// §8.15.4.2.3).
    pub fn right_view_monoscopic_allowed(&self) -> bool {
        self.single_view_allowed & 1 != 0
    }

    /// `true` when the left view of the stereo pair may be displayed on
    /// a monoscopic single-view display (`single_view_allowed & 2`,
    /// §8.15.4.2.3).
    pub fn left_view_monoscopic_allowed(&self) -> bool {
        self.single_view_allowed & 2 != 0
    }
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

/// Decoded QuickTime Preview Atom (`pnot`).
///
/// A top-level atom locating the movie's preview (poster) image — a
/// representative frame suitable for display in an Open dialog. Body
/// layout (a plain `Box`, no FullBox preamble): `unsigned int(32)
/// modification_date` (a Macintosh-format date), `unsigned int(16)
/// version` (0), `unsigned int(32) atom_type` (the FourCC of the atom
/// holding the preview data, typically `PICT`), and `unsigned int(16)
/// atom_index` (which atom of that type to use, typically 1) — twelve
/// bytes. All integers big-endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PnotRecord {
    /// The date the preview was last updated, in Macintosh format
    /// (seconds since 1904-01-01).
    pub modification_date: u32,
    /// The version number (must be 0).
    pub version: u16,
    /// The FourCC of the atom type that holds the preview data (typically
    /// `PICT` for a QuickDraw picture).
    pub atom_type: [u8; 4],
    /// Which atom of the specified type to use as the preview (typically
    /// 1 = the first).
    pub atom_index: u16,
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

/// One `(grouping_type_parameter, min_initial_alt_startup_offset)` entry
/// of a version-1 `assp` (Alternative Startup Sequence Properties Box,
/// ISO/IEC 14496-12 §8.8.16). A version-0 `assp` carries a single
/// implied entry with no `grouping_type_parameter`; see [`AsspRecord`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsspEntry {
    /// §8.8.16.3 `grouping_type_parameter` — selects which one of the
    /// alternative startup-sequence sample groupings this entry applies
    /// to (v1 only; `None` for the single implied v0 entry).
    pub grouping_type_parameter: Option<u32>,
    /// §8.8.16.3 `min_initial_alt_startup_offset` — a signed lower bound
    /// on `sample_offset[1]` of the referred `alst` (§10.3.2) sample
    /// group description entries: no value shall be smaller than this.
    pub min_initial_alt_startup_offset: i32,
}

/// Decoded `assp` (Alternative Startup Sequence Properties Box, ISO/IEC
/// 14496-12 §8.8.16).
///
/// A `FullBox('assp', version, 0)` nested inside a `trep`
/// (TrackExtensionPropertiesBox, §8.8.15) that indicates the properties
/// of the alternative startup sequence (`alst`, §10.3.2) sample groups
/// in the subsequent track fragments of the `trep`'s track. §8.8.16.1
/// pins the version to track the `sbgp` version used for the `alst`
/// grouping: version 0 when the `alst` `sbgp` is v0 (one implied entry,
/// no `grouping_type_parameter`); version 1 when the `alst` `sbgp` is v1
/// (a `num_entries`-long list keyed by `grouping_type_parameter`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsspRecord {
    /// FullBox `version` — 0 (single implied entry) or 1 (keyed list).
    pub version: u8,
    /// The per-grouping entries. Version 0 yields exactly one entry with
    /// `grouping_type_parameter == None`; version 1 yields `num_entries`
    /// entries, each carrying its own `grouping_type_parameter`.
    pub entries: Vec<AsspEntry>,
}

/// One child box recorded inside a `trep` (TrackExtensionPropertiesBox,
/// ISO/IEC 14496-12 §8.8.15). The box body is "any number of boxes"
/// (§8.8.15.2) — the demuxer records each child's type and payload
/// length, and additionally decodes the one child the base spec defines
/// here, `assp` (Alternative Startup Sequence Properties Box, §8.8.16),
/// into a typed [`AsspRecord`] (in [`TrepChild::assp`]). Any other child
/// stays opaque (type + length only) so a downstream consumer can still
/// spot it without this crate modelling every box that might be nested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrepChild {
    /// The child box's four-character type code, verbatim on the wire.
    pub fourcc: [u8; 4],
    /// Length of the child's payload in bytes (the box size minus its
    /// header). A child whose declared size ran past the end of the
    /// `trep` body is clamped to the bytes actually available, so this
    /// reflects what was readable, not necessarily the producer's
    /// declared length.
    pub payload_len: usize,
    /// The typed `assp` record when this child is an `assp`
    /// (Alternative Startup Sequence Properties Box, §8.8.16) whose body
    /// parsed cleanly; `None` for any other child type or a malformed
    /// `assp` (a malformed `assp` still contributes its type + length so
    /// the child list stays faithful to the wire).
    pub assp: Option<AsspRecord>,
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
    /// `amve` (AmbientViewingEnvironmentBox, ISO/IEC 14496-12 post-2015
    /// addition) found inside the track's `VisualSampleEntry`. Carries the
    /// nominal ambient viewing environment (illuminance + CIE 1931
    /// chromaticity) for the display of the video — the file-format
    /// carriage of the `ambient_viewing_environment` SEI / ISO/IEC
    /// 23091-3 parameters. `None` for non-video tracks or when the box is
    /// absent (the common case). When two sample entries each carry an
    /// `amve`, the first encountered wins (a single nominal environment
    /// per track).
    amve: Option<AmveRecord>,
    /// §8.15.4.2 — `stvi` (StereoVideoBox) found inside the track's
    /// sample-entry `sinf/schi` when the restricted-scheme SchemeType is
    /// `stvi` (stereoscopic video, §8.15.4.1). Carries
    /// `single_view_allowed`, `stereo_scheme`, and the raw
    /// `stereo_indication_type` bytes describing the stereo arrangement.
    /// `None` for non-stereo tracks (the common case) or when the box is
    /// absent / malformed. The first `stvi` encountered across the
    /// track's sample entries wins.
    stvi: Option<StviRecord>,
    /// §8.5.2 — `btrt` (BitRateBox) found inside the track's active
    /// `SampleEntry` (`[0]`). Carries `bufferSizeDB` / `maxBitrate` /
    /// `avgBitrate`, the codec-agnostic bandwidth descriptor a player or
    /// ABR packager reads without an `esds`. `None` when the sample
    /// entry omits it (common). When several sample entries each carry a
    /// `btrt`, the first encountered (on the active entry) wins.
    btrt: Option<BtrtRecord>,
    /// §12.1.4 — `pasp` (PixelAspectRatioBox) in the track's active
    /// `VisualSampleEntry`. `None` for non-video tracks or when absent
    /// (square pixels). First one encountered wins.
    pasp: Option<PaspRecord>,
    /// §12.1.4 — `clap` (CleanApertureBox) in the active
    /// `VisualSampleEntry`. `None` when absent. First one wins.
    clap: Option<ClapRecord>,
    /// §12.1.5 — `colr` (ColourInformationBox) in the active
    /// `VisualSampleEntry`. `None` when absent. The spec permits several
    /// `colr` boxes (most-accurate first); the first encountered wins.
    colr: Option<ColrRecord>,
    /// §12.4.2 / §8.4.5 — `hmhd` (HintMediaHeaderBox). Present for hint
    /// tracks (a `hdlr` of type `hint`); the spec marks exactly one
    /// media header mandatory inside `minf`, and a hint track uses this
    /// one. Carries protocol-independent PDU-size + bitrate statistics.
    /// `None` for non-hint tracks (which use `vmhd` / `smhd` / `nmhd` /
    /// `sthd` instead) or when the box is absent.
    hmhd: Option<HmhdBox>,
    /// QuickTime Base Media Info Atom (`gmin`) found inside the track's
    /// `minf/gmhd` (base media information header) atom. QuickTime uses
    /// `gmhd` in place of `vmhd` / `smhd` for media types derived from the
    /// base media handler (text, timecode, music, generic/`gnrc`); its
    /// `gmin` child carries a `graphicsmode` composition mode, a
    /// three-component `opcolor`, and a `balance` (the stereo sound mix).
    /// `None` for tracks whose `minf` uses a typed media header instead, or
    /// when the atom is absent. The first parseable `gmin` seen wins.
    gmin: Option<GminBox>,
    /// QuickTime Timecode Media Information Atom (`tcmi`) found inside the
    /// track's `minf/gmhd` for a timecode track (media type `tmcd`). Carries
    /// the text-rendering parameters (font / face / size / text +
    /// background colour / font name) for the on-screen timecode. `None`
    /// for non-timecode tracks or when the atom is absent. The first
    /// parseable `tcmi` seen wins.
    tcmi: Option<TcmiBox>,
    /// QuickTime Timecode sample description (`tmcd` `stsd` entry) for a
    /// timecode track. Carries the `flags` (drop-frame / 24-hour / …),
    /// `timescale`, `frame_duration`, and `number_of_frames` that define
    /// how the track's timecode samples are interpreted. `None` for
    /// non-timecode tracks or when the entry cannot be parsed.
    tmcd: Option<TmcdSampleEntry>,
    /// QuickTime Text sample description (`text` `stsd` entry) for a
    /// QuickTime text track. Carries the display flags, justification,
    /// colours, default text box, font, and font name that define how the
    /// track's text samples are drawn. `None` for non-`text` entries or
    /// when the entry cannot be parsed (the raw post-preamble bytes remain
    /// available as `extradata`).
    text_entry: Option<TextSampleEntry>,
    /// QuickTime Track Load Settings Atom (`load`) found inside the track's
    /// `trak`. Carries the preload segment (start/duration), preload flags,
    /// and default playback hints. `None` when the atom is absent (the
    /// common case). The first parseable `load` seen wins.
    load_settings: Option<LoadSettingsBox>,
    /// §9.1.2 / §9.4.1.2 — the RTP / SRTP / reception hint sample entry
    /// (`rtp ` / `srtp` / `rrtp`) from the track's active `stsd` entry,
    /// when present. Carries `maxpacketsize` plus the `tims` timescale and
    /// optional `tsro`/`snro` offsets (and an `srpp` SRTPProcessBox for
    /// `srtp`). `None` for non-hint tracks or a hint track whose entry
    /// format is some other protocol (e.g. an MPEG-2 TS hint).
    rtp_hint: Option<crate::hint::RtpHintSampleEntry>,
    /// §9.3.3.2 — the MPEG-2 TS server / reception hint sample entry
    /// (`sm2t` / `rm2t`) from the track's active `stsd` entry, when
    /// present. Carries the per-TS-packet preceding/trailing byte counts
    /// and the precomputed-only flag. `None` for non-MPEG-2-TS-hint
    /// tracks.
    mpeg2ts_hint: Option<crate::hint::Mpeg2TsHintSampleEntry>,
    /// §9.1.5 — the `hinf` Hint Statistics Box from the track's `udta`,
    /// when present. Summarises the packetised stream a server would
    /// generate (total bytes/packets sent, max-rate windows, payload
    /// rtpmap entries, …). Empty ([`crate::hint::HintStatistics::is_empty`])
    /// for non-hint tracks or a hint track that carried no statistics.
    hint_stats: crate::hint::HintStatistics,
    /// §8.7.2 — `dref` (DataReferenceBox) from the track's
    /// `minf/dinf/dref`. The table of media-data locations that every
    /// sample entry's 1-based `data_reference_index` (§8.5.2.2) selects
    /// from: a self-contained entry (`flags & 1`) means the samples live
    /// in this same file, an external `url `/`urn ` entry names another
    /// resource. `None` when the track has no parseable `dref` (the box
    /// is mandatory inside `minf`, but a malformed one is tolerated). The
    /// common single-source case is exactly one self-contained `url `
    /// entry.
    dref: Option<DrefBox>,
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
pub struct SbgpBox {
    /// Four-byte grouping type identifying which `sgpd` (same type) this
    /// box indexes — e.g. `rap `, `roll`, `sync`, `tele`, `alst`.
    pub grouping_type: [u8; 4],
    /// `grouping_type_parameter` — present only in version 1, selecting
    /// one of several alternative groupings of the same type. `None` for
    /// version 0.
    pub grouping_type_parameter: Option<u32>,
    /// Run-length `(sample_count, group_description_index)` pairs. A
    /// `group_description_index` of 0 means "member of no group of this
    /// type"; an index ≥ 0x10001 (movie-fragment-local) is preserved
    /// verbatim — the demuxer does not resolve fragment-local groups.
    pub entries: Vec<(u32, u32)>,
}

/// Parsed `sgpd` (SampleGroupDescriptionBox, ISO/IEC 14496-12 §8.9.3).
#[derive(Clone, Debug, Default)]
pub struct SgpdBox {
    /// Four-byte grouping type linking this description table to the
    /// `sbgp` of the same type.
    pub grouping_type: [u8; 4],
    /// `default_sample_description_index` (version ≥ 2): the group entry
    /// applied to samples not mapped by any `sbgp`. 0 (the default)
    /// means "no group of this type"; absent in versions 0/1.
    pub default_sample_description_index: Option<u32>,
    /// Per-group descriptive entries, each a grouping-type-specific
    /// opaque payload preserved verbatim. The container does not
    /// interpret them — interpretation belongs to the layer that knows
    /// the `grouping_type` semantics.
    pub entries: Vec<Vec<u8>>,
}

/// One pattern of a `csgp` (CompactSampleToGroupBox, §8.9.5): a run of
/// `pattern_length` per-sample `sample_group_description_index` values,
/// replicated across `sample_count` consecutive groups of that length.
#[derive(Clone, Debug, Default)]
pub struct CsgpPattern {
    /// `sample_count[i]` — number of consecutive groups (each
    /// `pattern_length` samples long) that replay this pattern. The
    /// pattern covers `sample_count * pattern_length` samples in total.
    pub sample_count: u32,
    /// `sample_group_description_index[i][1..=pattern_length]` — one
    /// index per sample of the pattern. Index 0 means "member of no
    /// group of this type"; when the box lives in a `traf`, the index's
    /// most-significant bit (for the field's width) distinguishes a
    /// fragment-local description (set) from a global one (clear). The
    /// raw value is preserved verbatim; the demuxer does not resolve it.
    pub indices: Vec<u32>,
}

/// Parsed `csgp` (CompactSampleToGroupBox, ISO/IEC 14496-12:2020 §8.9.5).
#[derive(Clone, Debug, Default)]
pub struct CsgpBox {
    /// Four-byte grouping type linking this box to the `sgpd` of the
    /// same type — identical role to `sbgp.grouping_type`.
    pub grouping_type: [u8; 4],
    /// `grouping_type_parameter` — present only when the flag layout's
    /// presence bit is set, selecting one of several alternative
    /// groupings of the same type. `None` when the bit is clear.
    pub grouping_type_parameter: Option<u32>,
    /// `index_msb_indicates_fragment_local_description` — flag layout
    /// **bit 7** (`flags & 0x80`). When set (legal only inside a `traf`),
    /// the most-significant bit of each `sample_group_description_index`,
    /// at the field's [`index_field_bits`] width, is a *source selector*
    /// rather than part of the index value: MSB = 0 → the index refers to
    /// the `trak`/`moov`-level `sgpd` (global); MSB = 1 → it refers to the
    /// fragment-local `sgpd` inside the enclosing `traf`. When clear, the
    /// index is a plain `sgpd` index with no MSB special-casing. The
    /// indices in [`CsgpPattern::indices`] are always preserved verbatim;
    /// use [`CsgpPattern::resolve_index`] to split a stored index into its
    /// (fragment-local, value) parts according to this flag and width.
    pub index_msb_indicates_fragment_local_description: bool,
    /// Bit width (`4`, `8`, `16`, or `32`) of each
    /// `sample_group_description_index` field on disk — `4 << index_size_code`
    /// where `index_size_code = flags & 0x3`. The MSB convention of
    /// [`index_msb_indicates_fragment_local_description`] locates the
    /// source-selector bit at position `index_field_bits - 1`, so a
    /// resolver needs this to mask it correctly.
    pub index_field_bits: u8,
    /// The repeating index patterns, in disk order.
    pub patterns: Vec<CsgpPattern>,
}

/// One resolved `sample_group_description_index` from a `csgp` whose
/// [`CsgpBox::index_msb_indicates_fragment_local_description`] flag is
/// set: the source-selector MSB has been split off from the index value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CsgpResolvedIndex {
    /// `true` when the field's MSB was set: the index refers to a
    /// description in the **fragment-local** `sgpd` (inside the enclosing
    /// `traf`). `false` → the **global** (`trak`/`moov`-level) `sgpd`.
    /// Always `false` when the box's flag is clear.
    pub fragment_local: bool,
    /// The group-description index with the source-selector MSB stripped
    /// (1-based into the chosen `sgpd`; `0` means "member of no group").
    /// When the box's flag is clear this is the verbatim stored value.
    pub value: u32,
}

impl CsgpPattern {
    /// Resolve the `n`-th stored index of this pattern against the box's
    /// MSB convention.
    ///
    /// When `index_msb_indicates_fragment_local_description` is set (only
    /// legal inside a `traf`), the most-significant bit of the field — at
    /// `index_field_bits` width — selects the `sgpd` source and is
    /// stripped from the value (ISO/IEC 14496-12:2020 §8.9.5). When it is
    /// clear, the index is returned verbatim with `fragment_local =
    /// false`. Returns `None` if `n` is out of range.
    pub fn resolve_index(
        &self,
        n: usize,
        index_msb_indicates_fragment_local_description: bool,
        index_field_bits: u8,
    ) -> Option<CsgpResolvedIndex> {
        let raw = *self.indices.get(n)?;
        Some(resolve_csgp_index(
            raw,
            index_msb_indicates_fragment_local_description,
            index_field_bits,
        ))
    }
}

/// Split a raw `csgp` index into its (fragment-local, value) parts.
///
/// Shared by [`CsgpPattern::resolve_index`]. With the MSB convention
/// active, the selector bit sits at `index_field_bits - 1`; it is read
/// and masked off. A `index_field_bits` of 0 (degenerate) is treated as
/// "no MSB", returning the value verbatim.
fn resolve_csgp_index(
    raw: u32,
    msb_is_fragment_local: bool,
    index_field_bits: u8,
) -> CsgpResolvedIndex {
    if !msb_is_fragment_local || index_field_bits == 0 || index_field_bits > 32 {
        return CsgpResolvedIndex {
            fragment_local: false,
            value: raw,
        };
    }
    let msb_pos = index_field_bits - 1;
    let msb_mask = 1u32 << msb_pos;
    CsgpResolvedIndex {
        fragment_local: raw & msb_mask != 0,
        value: raw & (msb_mask - 1),
    }
}

impl CsgpBox {
    /// Expand the compact patterns into one resolved
    /// `sample_group_description_index` per sample, in decoding order
    /// (ISO/IEC 14496-12:2020 §8.9.5, "Pattern → sample mapping").
    ///
    /// `csgp` stores a small set of index *patterns* that are each cycled
    /// across a run of samples; this method materialises the per-sample
    /// mapping the same way `sbgp` already gives it, so a caller need not
    /// re-derive the cycling rules:
    ///
    /// * Pattern `i` (with `pattern_length = indices.len()`) applies to its
    ///   `sample_count[i]` samples. When `sample_count[i] > pattern_length`
    ///   the index entries are **cycled** to cover all the samples; the
    ///   cycle may end part-way through a pattern (no requirement that it
    ///   divides evenly). When `sample_count[i] == pattern_length` the
    ///   pattern is used once, un-repeated.
    /// * A pattern with an empty index run (`pattern_length == 0`) cannot
    ///   be cycled, so it contributes nothing and its `sample_count` is
    ///   skipped (a malformed-but-bounded input rather than a hard error).
    /// * If the sum of `sample_count[i]` is **less than** `total_samples`,
    ///   the trailing unmapped samples take the `sgpd` default group, which
    ///   this method surfaces as a resolved index with `value == 0`
    ///   (`fragment_local == false`) — "member of no group of this type",
    ///   exactly the same sentinel an explicit `0` index carries. The
    ///   caller substitutes the `sgpd` `default_group_description_index`
    ///   (if any) for those zeros, identically to the `sbgp` default rule.
    /// * If the sum **exceeds** `total_samples` the reader behaviour is
    ///   undefined per the spec; we clamp to `total_samples` so the output
    ///   never exceeds the track's real sample count.
    ///
    /// Each emitted index is run through the box's MSB convention via
    /// [`resolve_csgp_index`], so fragment-local vs global selection is
    /// already split out (see
    /// [`CsgpBox::index_msb_indicates_fragment_local_description`]).
    pub fn resolve_samples(&self, total_samples: usize) -> Vec<CsgpResolvedIndex> {
        let mut out: Vec<CsgpResolvedIndex> = Vec::with_capacity(total_samples);
        'patterns: for pat in &self.patterns {
            let plen = pat.indices.len();
            if plen == 0 {
                // No index entries to cycle — this pattern maps nothing.
                continue;
            }
            for s in 0..pat.sample_count as usize {
                if out.len() >= total_samples {
                    break 'patterns;
                }
                let raw = pat.indices[s % plen];
                out.push(resolve_csgp_index(
                    raw,
                    self.index_msb_indicates_fragment_local_description,
                    self.index_field_bits,
                ));
            }
        }
        // Trailing samples not covered by any pattern take the sgpd default
        // (surfaced as value 0 = "no group"); pad up to total_samples.
        while out.len() < total_samples {
            out.push(CsgpResolvedIndex {
                fragment_local: false,
                value: 0,
            });
        }
        out
    }
}

/// One sub-sample of a `subs` (SubSampleInformationBox, §8.7.7) entry.
///
/// Fields decoded from the on-wire `unsigned int(16 or 32) subsample_size;
/// unsigned int(8) subsample_priority; unsigned int(8) discardable;
/// unsigned int(32) codec_specific_parameters;` layout. The
/// `subsample_size` field is 16-bit at version 0 and widens to 32-bit at
/// version 1; we always store as `u32` so callers handle one shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubSampleEntry {
    /// §8.7.7.3 — size in bytes of the sub-sample.
    pub subsample_size: u32,
    /// §8.7.7.3 — degradation priority. Higher = more important to
    /// decoded quality. Codec-specific scale; the container does not
    /// interpret it.
    pub subsample_priority: u8,
    /// §8.7.7.3 — `0` = required to decode the current sample; non-zero
    /// = optional (e.g. an SEI message that only enhances output).
    pub discardable: u8,
    /// §8.7.7.3 — opaque codec-specific blob (4 bytes). For AVC
    /// (ISO/IEC 14496-15) this encodes NAL-unit role / dependency
    /// information; absent a codec-specific binding the field is `0`.
    pub codec_specific_parameters: u32,
}

/// One entry of a `subs` (SubSampleInformationBox, §8.7.7) — a single
/// sample's sub-sample table together with the sparse delta that
/// addresses it.
#[derive(Clone, Debug, Default)]
pub struct SubsEntry {
    /// §8.7.7.3 — sample-number delta from the previous entry's sample
    /// (or from sample 0 for the first entry). The decoded absolute
    /// sample numbers are *not* materialised — callers walking the table
    /// can accumulate them themselves.
    pub sample_delta: u32,
    /// The sub-samples of this entry's sample, in disk order. May be
    /// empty (`subsample_count == 0`) per §8.7.7.1 — in that case the
    /// addressed sample has no sub-sample structure but is still
    /// enumerated by the table (the spec permits this shape so a
    /// producer can document "this sample is monolithic" alongside
    /// genuinely sub-divided neighbours).
    pub subsamples: Vec<SubSampleEntry>,
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
pub struct SubsBox {
    /// FullBox version (0 or 1) — determines `subsample_size` width on
    /// disk. We normalise to `u32` in `SubSampleEntry::subsample_size`.
    pub version: u8,
    /// FullBox flags — codec-specific. Distinguishes co-resident `subs`
    /// boxes per §8.7.7.1.
    pub flags: u32,
    /// One entry per *addressed* sample (the table is sparse — samples
    /// with no `subs` row contribute nothing here).
    pub entries: Vec<SubsEntry>,
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
pub struct SaizBox {
    /// §8.7.8.3 `aux_info_type` — present only when `flags & 1` is set.
    pub aux_info_type: Option<[u8; 4]>,
    /// §8.7.8.3 `aux_info_type_parameter` — present only when
    /// `flags & 1` is set; defaults to 0 when omitted.
    pub aux_info_type_parameter: Option<u32>,
    /// §8.7.8.3 `default_sample_info_size`; non-zero means a
    /// constant-size table and `per_sample` is empty.
    pub default_sample_info_size: u8,
    /// §8.7.8.3 `sample_count` — the declared number of samples this
    /// `saiz` covers (may be smaller than the track's full sample count
    /// per §8.7.8.3).
    pub sample_count: u32,
    /// §8.7.8.2 `sample_info_size[]` — populated only when
    /// `default_sample_info_size == 0`; otherwise empty.
    pub per_sample: Vec<u8>,
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
pub struct SaioBox {
    /// FullBox version (0 or 1) — selects 32-bit / 64-bit on-disk
    /// offset width. Preserved so a round-trip can match the original
    /// layout.
    pub version: u8,
    /// §8.7.9.3 `aux_info_type` — present only when `flags & 1` is set.
    pub aux_info_type: Option<[u8; 4]>,
    /// §8.7.9.3 `aux_info_type_parameter` — present only when
    /// `flags & 1` is set; defaults to 0 when omitted.
    pub aux_info_type_parameter: Option<u32>,
    /// §8.7.9.2 `offset[entry_count]` — widened to u64 regardless of
    /// box version. Semantics depend on container: absolute file
    /// position inside `stbl`, base_data_offset-relative inside `traf`.
    pub offsets: Vec<u64>,
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

/// Parsed `hmhd` (HintMediaHeaderBox, ISO/IEC 14496-12 §12.4.2,
/// defined per §8.4.5).
///
/// The media-type-specific header carried inside `minf` for a hint
/// track (a `hdlr` of type `hint`). Its body, after the 4-byte
/// `FullBox(version=0, 0)` preamble, is two 16-bit Protocol Data Unit
/// (PDU) byte sizes and two 32-bit bitrates, followed by a reserved
/// 32-bit zero (§12.4.2.2). The statistics are protocol-independent
/// (§12.4.2.1) — they summarise the packetised stream a hint track
/// describes so a streaming server can size buffers / pace delivery
/// without parsing every hint sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HmhdBox {
    /// Size in bytes of the largest PDU in this hint stream (§12.4.2.3).
    max_pdu_size: u16,
    /// Average size of a PDU over the entire presentation (§12.4.2.3).
    avg_pdu_size: u16,
    /// Maximum rate in bits/second over any window of one second
    /// (§12.4.2.3).
    max_bitrate: u32,
    /// Average rate in bits/second over the entire presentation
    /// (§12.4.2.3).
    avg_bitrate: u32,
}

/// Decoded QuickTime Base Media Info Atom (`gmin`).
///
/// QuickTime carries a **Base Media Information Header Atom** (`gmhd`)
/// inside `minf` for media types derived from the base media handler
/// (text, timecode, music, and generic tracks) — the role that `vmhd`
/// fills for video and `smhd` for sound. The `gmhd`'s required child is
/// the **Base Media Info Atom** (`gmin`), whose fixed-size body defines
/// the media's control information: graphics mode, operation colour, and
/// stereo sound balance.
///
/// Body layout after the 4-byte `FullBox(version, flags)` preamble:
/// `unsigned int(16) graphicsmode`, `unsigned int(16)[3] opcolor`,
/// `int(16) balance`, and a reserved `unsigned int(16)` — twelve payload
/// bytes after the preamble, sixteen total. All integers big-endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GminBox {
    /// The transfer (composition) mode for the base media, selecting the
    /// operation performed when drawing or transferring an image from one
    /// location to another. `0` is `copy`.
    pub graphicsmode: u16,
    /// The three (red, green, blue) 16-bit colour components for the
    /// transfer-mode operation indicated by `graphicsmode`.
    pub opcolor: [u16; 3],
    /// The sound balance: the mix of this media's sound between the two
    /// speakers. Normally `0` (centered); negative favours the left
    /// speaker, positive the right (an 8.8 fixed-point value).
    pub balance: i16,
}

/// Decoded QuickTime Timecode Media Information Atom (`tcmi`).
///
/// A timecode track (media type `tmcd`) carries a `tcmi` atom inside its
/// `gmhd` (base media information header). It governs how the timecode
/// text is rendered on screen: the font, style, size, and the text and
/// background colours.
///
/// Body layout after the 4-byte `FullBox(version, flags)` preamble:
/// `unsigned int(16) text_font`, `unsigned int(16) text_face`,
/// `unsigned int(16) text_size`, a 48-bit (`[u16; 3]`) `text_color`, a
/// 48-bit `background_color`, and a Pascal-string `font_name` (a leading
/// length byte followed by that many characters). All integers
/// big-endian.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TcmiBox {
    /// The font id to use for the timecode text. `0` selects the system
    /// font; when `font_name` is a valid name this field is ignored.
    pub text_font: u16,
    /// The font style (face). `0` is normal; the low bits enable Bold
    /// (`0x01`), Italic (`0x02`), Underline (`0x04`), Outline (`0x08`),
    /// Shadow (`0x10`), Condense (`0x20`), and Extend (`0x40`).
    pub text_face: u16,
    /// The point size of the timecode text.
    pub text_size: u16,
    /// The 48-bit RGB colour of the timecode text (red, green, blue).
    pub text_color: [u16; 3],
    /// The 48-bit RGB background colour behind the timecode text.
    pub background_color: [u16; 3],
    /// The name of the timecode text's font (the Pascal string, without
    /// its leading length byte). Empty when the atom carries no name.
    pub font_name: String,
}

/// Decoded QuickTime Timecode sample description (`tmcd` `stsd` entry).
///
/// A timecode track (media type `tmcd`) carries a single `tmcd` sample
/// entry that defines how the track's timecode sample data is
/// interpreted. Body layout after the shared 8-byte sample-entry preamble
/// (`reserved[6]` + `data_reference_index`): `unsigned int(32) reserved`,
/// `unsigned int(32) flags`, `unsigned int(32) timescale`,
/// `unsigned int(32) frame_duration`, `unsigned int(8) number_of_frames`,
/// and a reserved `unsigned int(8)` (the qtff shows the trailing reserved
/// as 24 bits alongside an optional source-reference `udta`; this record
/// captures the fixed numeric fields). All integers big-endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TmcdSampleEntry {
    /// Timecode characteristics: Drop-frame (`0x0001`), 24-hour-max
    /// (`0x0002`), Negative-times-OK (`0x0004`), and Counter (`0x0008`).
    pub flags: u32,
    /// The time scale for interpreting `frame_duration` (units per
    /// second).
    pub timescale: u32,
    /// How long each timecode frame lasts, in `timescale` units.
    pub frame_duration: u32,
    /// The number of frames per second for the timecode format. When the
    /// Counter flag is set, the number of frames per counter tick.
    pub number_of_frames: u8,
}

impl TmcdSampleEntry {
    /// Whether the timecode is drop-frame (`flags & 0x0001`).
    pub fn drop_frame(&self) -> bool {
        self.flags & 0x0001 != 0
    }
    /// Whether the timecode wraps after 24 hours (`flags & 0x0002`).
    pub fn twenty_four_hour_max(&self) -> bool {
        self.flags & 0x0002 != 0
    }
    /// Whether negative time values are allowed (`flags & 0x0004`).
    pub fn negative_times_ok(&self) -> bool {
        self.flags & 0x0004 != 0
    }
    /// Whether the time value is a tape counter value (`flags & 0x0008`).
    pub fn counter(&self) -> bool {
        self.flags & 0x0008 != 0
    }
}

/// Decoded QuickTime Text sample description (`text` `stsd` entry).
///
/// A QuickTime text track (media type `text`) carries a `text` sample
/// entry that defines how the track's text samples are drawn. Body layout
/// after the shared 8-byte sample-entry preamble: `unsigned int(32)
/// display_flags`, `int(32) text_justification`, a 48-bit (`[u16; 3]`)
/// `background_color`, a 64-bit `default_text_box` (`[i16; 4]` = top,
/// left, bottom, right), a reserved `int(64)`, `unsigned int(16)
/// font_number`, `unsigned int(16) font_face`, a reserved `int(8)`, a
/// reserved `int(16)`, a 48-bit `foreground_color`, and a Pascal-string
/// `text_name` (the font name). All integers big-endian.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextSampleEntry {
    /// Flags describing how the text is drawn (Don't-auto-scale `0x0002`,
    /// Use-movie-background-color `0x0008`, Scroll-in `0x0020`,
    /// Scroll-out `0x0040`, Horizontal-scroll `0x0080`, Reverse-scroll
    /// `0x0100`, Continuous-scroll `0x0200`, Drop-shadow `0x1000`,
    /// Anti-alias `0x2000`, Key-text `0x4000`).
    pub display_flags: u32,
    /// Text alignment: `0` left-justified, `1` centered, `-1`
    /// right-justified.
    pub text_justification: i32,
    /// The 48-bit RGB background colour of the text (red, green, blue).
    pub background_color: [u16; 3],
    /// The default text box rectangle (top, left, bottom, right).
    pub default_text_box: [i16; 4],
    /// The font number to use (must be 0 per the spec).
    pub font_number: u16,
    /// The font style (face). `0` is normal; the low bits enable Bold
    /// (`0x01`), Italic (`0x02`), Underline (`0x04`), Outline (`0x08`),
    /// Shadow (`0x10`), Condense (`0x20`), and Extend (`0x40`).
    pub font_face: u16,
    /// The 48-bit RGB foreground colour of the text (red, green, blue).
    pub foreground_color: [u16; 3],
    /// The name of the font to display the text with (the Pascal string,
    /// without its leading length byte). Empty when none is carried.
    pub text_name: String,
}

/// Decoded QuickTime Track Load Settings Atom (`load`).
///
/// A `trak`-level atom indicating how a reader should preload and play the
/// track. Body layout (a plain `Box`, no FullBox preamble): `int(32)
/// preload_start_time`, `int(32) preload_duration`, `int(32)
/// preload_flags`, `int(32) default_hints` — sixteen bytes. Times are in
/// the movie's time coordinate system. All integers big-endian.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadSettingsBox {
    /// Start time, in the movie timescale, of a track segment to preload.
    pub preload_start_time: i32,
    /// Duration, in the movie timescale, of the preload segment. `-1`
    /// means the segment extends to the end of the track.
    pub preload_duration: i32,
    /// Flags governing preload (mutually exclusive): `1` preload the track
    /// regardless of enablement; `2` preload only when the track is
    /// enabled.
    pub preload_flags: i32,
    /// Playback hints (may combine): Double-buffer (`0x0020`),
    /// High-quality (`0x0100`).
    pub default_hints: i32,
}

impl LoadSettingsBox {
    /// Whether the track is preloaded regardless of enablement
    /// (`preload_flags == 1`).
    pub fn preload_always(&self) -> bool {
        self.preload_flags == 1
    }
    /// Whether the track is preloaded only when enabled
    /// (`preload_flags == 2`).
    pub fn preload_if_enabled(&self) -> bool {
        self.preload_flags == 2
    }
    /// Whether the Double-buffer hint is set (`default_hints & 0x0020`).
    pub fn double_buffer(&self) -> bool {
        self.default_hints & 0x0020 != 0
    }
    /// Whether the High-quality hint is set (`default_hints & 0x0100`).
    pub fn high_quality(&self) -> bool {
        self.default_hints & 0x0100 != 0
    }
}

/// One entry of a `dref` (DataReferenceBox, ISO/IEC 14496-12 §8.7.2).
///
/// Each entry is either a `url ` DataEntryUrlBox or a `urn `
/// DataEntryUrnBox. The 24-bit FullBox `flags` field carries the only
/// defined flag: bit 0 (`0x000001`) means the media data is in the same
/// file as the Movie Box that contains this data reference — the
/// *self-contained* case. When self-contained, the URL form is used and
/// no string follows the flags (§8.7.2.3); otherwise `location` names the
/// external resource (a `urn ` entry additionally carries a `name`, the
/// URN proper).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DataEntry {
    /// The entry box type — `url ` or `urn `. Preserved verbatim so a
    /// surfacing layer can distinguish the two forms; any other FourCC is
    /// non-conforming (§8.7.2.1) and dropped during parse.
    kind: [u8; 4],
    /// The full 24-bit FullBox `flags`. Bit 0 set ⇒ self-contained.
    flags: u32,
    /// For a `urn ` entry, the URN `name` (the resource's name); `None`
    /// for a `url ` entry, which has no name field.
    name: Option<String>,
    /// The `location` URL. Absent (`None`) for a self-contained entry
    /// (`flags & 1` set, no string present per §8.7.2.3); present for an
    /// external `url `, and optional for a `urn ` (§8.7.2.3 — "optional
    /// in a URN entry").
    location: Option<String>,
}

impl DataEntry {
    /// §8.7.2.3 — the data is in the same file as the Movie Box when the
    /// low flag bit is set.
    fn is_self_contained(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Parsed `dref` (DataReferenceBox, ISO/IEC 14496-12 §8.7.2), the table
/// of media-data locations that the per-sample-entry
/// `data_reference_index` (§8.5.2.2) indexes into. A track may be split
/// over several sources, so the box holds `entry_count` entries; index
/// `k` (1-based) names where the samples of a sample description carrying
/// `data_reference_index == k` are stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DrefBox {
    /// The data-reference table, in on-disk order. Entry `[i]` is the
    /// 1-based `data_reference_index` `i + 1`.
    entries: Vec<DataEntry>,
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

// ===========================================================================
// ISO/IEC 14496-12 §8.11 — `meta` item infrastructure (HEIF / MIAF family)
// ===========================================================================
//
// A `meta` box describes a set of untimed *items* (still images, EXIF
// blobs, XMP, derived images, …) rather than timed samples. The item
// catalogue is spread across four sibling boxes inside the `meta`:
//
//   * `pitm` (PrimaryItemBox, §8.11.4) — names the primary item;
//   * `iloc` (ItemLocationBox, §8.11.3) — where each item's bytes live;
//   * `iinf` (ItemInfoBox, §8.11.6) — each item's type code + name;
//   * `iref` (ItemReferenceBox, §8.11.12) — typed item→item links.
//
// This crate is a container: it surfaces the catalogue (so a HEIF reader
// can locate, type and relate items) but does not itself decode an
// item's codec payload. The structured records below are reachable via
// `Mp4Demuxer::meta_items()` and the flat `meta_*` metadata keys.

/// One extent of an item, per the `iloc` (ItemLocationBox, ISO/IEC
/// 14496-12 §8.11.3) extent loop. An item's data is the concatenation of
/// its extents in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IlocExtent {
    /// `extent_index` (§8.11.3.3) — only meaningful for
    /// `construction_method == 2` (item_offset); 0 when `index_size` was
    /// 0 or the construction method does not use it.
    pub extent_index: u64,
    /// `extent_offset` — absolute offset of this extent's bytes from the
    /// data origin selected by `construction_method`. 0 when
    /// `offset_size` was 0 (the §8.11.3.3 "beginning of source" implied
    /// value).
    pub extent_offset: u64,
    /// `extent_length` — length of this extent in bytes. 0 when
    /// `length_size` was 0 (the §8.11.3.3 "entire length of source"
    /// implied value).
    pub extent_length: u64,
}

/// One item's location record, per the `iloc` per-item loop (ISO/IEC
/// 14496-12 §8.11.3.2). The on-wire field widths (`offset_size`,
/// `length_size`, `base_offset_size`, `index_size`) are box-global; the
/// decoded values are widened to `u64` here so a consumer sees one type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IlocItem {
    /// `item_ID` — the catalogue key. 16-bit on the wire for versions
    /// 0/1, 32-bit for version 2; widened to `u32`.
    pub item_id: u32,
    /// `construction_method` (versions 1/2 only; 0 for version 0):
    /// 0 = file offset, 1 = idat offset, 2 = item offset (§8.11.3.3).
    pub construction_method: u8,
    /// `data_reference_index` — 0 = this file, else a 1-based index into
    /// the `dref` table (§8.11.3.3).
    pub data_reference_index: u16,
    /// `base_offset` — added to each extent offset (§8.11.3.3). 0 when
    /// `base_offset_size` was 0.
    pub base_offset: u64,
    /// Extents in file order; their lengths sum to the item's total size.
    pub extents: Vec<IlocExtent>,
}

/// Decoded `iloc` (ItemLocationBox, ISO/IEC 14496-12 §8.11.3).
///
/// Carries the box-global field-width selectors verbatim (so a consumer
/// can reason about the on-wire encoding) plus one [`IlocItem`] per
/// declared item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IlocBox {
    /// FullBox version (0, 1, or 2). Versions 1/2 carry
    /// `construction_method`; version 2 widens `item_ID` and `item_count`
    /// to 32-bit.
    pub version: u8,
    /// `offset_size` — byte width of each `extent_offset` ({0, 4, 8}).
    pub offset_size: u8,
    /// `length_size` — byte width of each `extent_length` ({0, 4, 8}).
    pub length_size: u8,
    /// `base_offset_size` — byte width of `base_offset` ({0, 4, 8}).
    pub base_offset_size: u8,
    /// `index_size` — byte width of `extent_index` ({0, 4, 8}); 0 for
    /// version 0 (where the nibble is a reserved field).
    pub index_size: u8,
    /// One record per declared item, in file order.
    pub items: Vec<IlocItem>,
}

/// One item's information entry, decoded from an `infe` (ItemInfoEntry,
/// ISO/IEC 14496-12 §8.11.6.2) inside `iinf`.
///
/// The base spec defines four `infe` versions. Versions 0/1 carry a
/// `(protection_index, name, content_type, content_encoding)` shape with
/// no `item_type` code; versions 2/3 carry a 32-bit `item_type` FourCC
/// (e.g. `hvc1`, `Exif`, `mime`, `uri `) plus a name and — for `mime` /
/// `uri ` types — a content-type or URI string. The fields that don't
/// apply to a given version are left empty / zero.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ItemInfoEntry {
    /// `item_ID` — matches an `iloc` `item_ID`.
    pub item_id: u32,
    /// `item_protection_index` — 0 = unprotected, else a 1-based index
    /// into the `ipro` ItemProtectionBox (§8.11.6.3).
    pub protection_index: u16,
    /// `item_type` — the 32-bit type code (versions ≥ 2). All-zero for
    /// versions 0/1 where the field does not exist on the wire.
    pub item_type: [u8; 4],
    /// `item_name` — a human-readable / symbolic name (may be empty).
    pub item_name: String,
    /// `content_type` — MIME type string (versions 0/1 always; versions
    /// ≥ 2 only when `item_type == "mime"`). Empty when absent.
    pub content_type: String,
    /// `content_encoding` — optional encoding string (e.g. "gzip");
    /// empty when absent. For `uri ` items (versions ≥ 2) this field
    /// instead carries the `item_uri_type` string.
    pub content_encoding: String,
    /// FullBox version of the source `infe` (0, 1, 2, or 3).
    pub version: u8,
}

/// Decoded `iinf` (ItemInfoBox, ISO/IEC 14496-12 §8.11.6).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct IinfBox {
    /// One entry per `infe` child, in file order (the spec sorts by
    /// increasing `item_ID`, but the demuxer preserves wire order).
    pub entries: Vec<ItemInfoEntry>,
}

/// One typed group of item references sharing a `reference_type`, decoded
/// from a `SingleItemTypeReferenceBox` inside `iref` (ISO/IEC 14496-12
/// §8.11.12.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemReference {
    /// The reference type FourCC (e.g. `dimg`, `thmb`, `cdsc`, `auxl`).
    pub reference_type: [u8; 4],
    /// `from_item_ID` — the item that refers to the others.
    pub from_item_id: u32,
    /// `to_item_ID`s — the items referred to, in file order.
    pub to_item_ids: Vec<u32>,
}

/// Decoded `iref` (ItemReferenceBox, ISO/IEC 14496-12 §8.11.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrefBox {
    /// FullBox version: 0 → 16-bit item IDs, 1 → 32-bit item IDs.
    pub version: u8,
    /// One group per `SingleItemTypeReferenceBox`, in file order.
    pub references: Vec<ItemReference>,
}

/// One decoded item property held in an `ipco` (ItemPropertyContainerBox,
/// ISO/IEC 23008-12 §9.3.1). Each property is a `Box` or `FullBox` whose
/// type code names the property; the variants below model the base HEIF
/// property set. Boxes whose syntax is shared with ISO/IEC 14496-12
/// (`pasp` / `clap` / `colr`) reuse this crate's existing typed records.
/// Any property box this layer does not recognise is preserved verbatim as
/// [`ItemProperty::Other`] so its index slot and bytes survive a
/// parse→build round-trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemProperty {
    /// `ispe` ImageSpatialExtentsProperty (§6.5.3) — the reconstructed
    /// image `(width, height)` in pixels.
    Ispe { image_width: u32, image_height: u32 },
    /// `pixi` PixelInformationProperty (§6.5.6) — the per-channel bit depth
    /// of the reconstructed image, one entry per colour channel.
    Pixi { bits_per_channel: Vec<u8> },
    /// `rloc` RelativeLocationProperty (§6.5.7) — an item's
    /// `(horizontal_offset, vertical_offset)` within the image it has a
    /// `tbas` item reference to.
    Rloc {
        horizontal_offset: u32,
        vertical_offset: u32,
    },
    /// `auxC` AuxiliaryTypeProperty (§6.5.8) — a NULL-terminated URN
    /// `aux_type` identifying an auxiliary image (e.g. an alpha plane or
    /// depth map) plus the type-specific `aux_subtype` tail bytes.
    AuxC {
        aux_type: String,
        aux_subtype: Vec<u8>,
    },
    /// `irot` ImageRotation (§6.5.10) — a transformative property: the
    /// reconstructed image is rotated anti-clockwise by `angle * 90`
    /// degrees (`angle` ∈ `0..=3`).
    Irot { angle: u8 },
    /// `imir` ImageMirror (§6.5.12) — a transformative property: the
    /// reconstructed image is mirrored about a vertical (`axis == 0`) or
    /// horizontal (`axis == 1`) axis.
    Imir { axis: u8 },
    /// `lsel` LayerSelectorProperty (§6.5.11) — selects one reconstructed
    /// image (`layer_id`) of a multi-layer coded image item.
    Lsel { layer_id: u16 },
    /// `pasp` PixelAspectRatioBox (§6.5.4 → ISO/IEC 14496-12 §12.1.4).
    Pasp(PaspRecord),
    /// `clap` CleanApertureBox (§6.5.9 → ISO/IEC 14496-12 §12.1.4), a
    /// transformative crop property.
    Clap(ClapRecord),
    /// `colr` ColourInformationBox (§6.5.5 → ISO/IEC 14496-12 §12.1.5).
    Colr(ColrRecord),
    /// `udes` UserDescriptionProperty (§6.5.20) — four NUL-terminated UTF-8
    /// strings: `lang` (RFC 5646 tag), `name`, `description`, and
    /// comma-separated `tags` (any may be empty).
    Udes {
        lang: String,
        name: String,
        description: String,
        tags: String,
    },
    /// `altt` AccessibilityTextProperty (§6.5.21) — an alternate-text
    /// string (HTML `alt`-style) plus its RFC 5646 language tag.
    Altt { alt_text: String, alt_lang: String },
    /// `iscl` ImageScaling (§6.5.13) — a transformative property: the
    /// horizontal / vertical scaling ratios as 16-bit numerator/denominator
    /// pairs (a value of 0 is spec-prohibited but preserved verbatim).
    Iscl {
        target_width_numerator: u16,
        target_width_denominator: u16,
        target_height_numerator: u16,
        target_height_denominator: u16,
    },
    /// `rref` RequiredReferenceTypesProperty (§6.5.17) — the `iref`
    /// reference-type FourCCs a reader must understand to decode the item
    /// (e.g. `pred` for a predictively-coded image item).
    Rref { reference_types: Vec<[u8; 4]> },
    /// `crtt` CreationTimeProperty (§6.5.18) — the item / group creation
    /// time, in microseconds since 1904-01-01 UTC.
    Crtt { creation_time: u64 },
    /// `mdft` ModificationTimeProperty (§6.5.19) — the most recent
    /// modification time, in microseconds since 1904-01-01 UTC.
    Mdft { modification_time: u64 },
    /// Any other property box: its FourCC type plus the raw body bytes
    /// (after any FullBox preamble is *included* — the body is the box
    /// payload verbatim). Preserved so the implicit 1-based index slot is
    /// never silently dropped.
    Other { box_type: [u8; 4], body: Vec<u8> },
}

/// One `(essential, property_index)` association from an `ipma`
/// (ItemPropertyAssociation, ISO/IEC 23008-12 §9.3.1). `property_index` is
/// the 1-based index into the sibling `ipco` (0 means "no property"); when
/// `essential` is set a reader that does not understand the referenced
/// property must not process the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyAssociation {
    /// `essential` (§9.3.3) — true → a reader unable to process the
    /// associated property must reject the item.
    pub essential: bool,
    /// `property_index` (§9.3.3) — 1-based index into `ipco`'s implicit
    /// property list, or 0 for "no property".
    pub property_index: u16,
}

/// One item's association list from an `ipma` box (§9.3.1): the item ID and
/// the ordered set of property indices associated with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemPropertyAssociationEntry {
    /// `item_ID` — matches an `iloc` / `iinf` item ID.
    pub item_id: u32,
    /// The ordered associations (`ipma` preserves writer order, which is
    /// significant: transformative properties apply in sequence — §6.5.1).
    pub associations: Vec<PropertyAssociation>,
}

/// Decoded `iprp` (ItemPropertiesBox, ISO/IEC 23008-12 §9.3.1) — the
/// `ipco` property list plus every `ipma` association box, merged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemProperties {
    /// The `ipco` property list, in on-wire order. The implicit
    /// `property_index` an `ipma` association references is the 1-based
    /// position in this vec (so `properties[index - 1]`).
    pub properties: Vec<ItemProperty>,
    /// Every item→property association, accumulated across all `ipma`
    /// boxes in the `iprp`. The spec permits several `ipma` boxes
    /// distinguished by `(version, flags)`; their entries are concatenated
    /// here in encounter order.
    pub associations: Vec<ItemPropertyAssociationEntry>,
}

impl ItemProperties {
    /// True when neither an `ipco` property nor an `ipma` association was
    /// found (an empty / malformed `iprp`).
    pub fn is_empty(&self) -> bool {
        self.properties.is_empty() && self.associations.is_empty()
    }

    /// Look up the property record referenced by a 1-based `property_index`
    /// (the value carried in an `ipma` association). Returns `None` for
    /// index 0 ("no property") or an index past the end of the `ipco` list.
    pub fn property(&self, property_index: u16) -> Option<&ItemProperty> {
        if property_index == 0 {
            return None;
        }
        self.properties.get((property_index - 1) as usize)
    }

    /// The resolved `(essential, &ItemProperty)` pairs associated with an
    /// item, in `ipma` order. Associations whose `property_index` is 0 or
    /// out of range are skipped (a malformed reference contributes nothing
    /// rather than aborting). Returns an empty vec when the item has no
    /// `ipma` entry.
    pub fn properties_for(&self, item_id: u32) -> Vec<(bool, &ItemProperty)> {
        let mut out = Vec::new();
        for entry in &self.associations {
            if entry.item_id != item_id {
                continue;
            }
            for a in &entry.associations {
                if let Some(p) = self.property(a.property_index) {
                    out.push((a.essential, p));
                }
            }
        }
        out
    }
}

/// One entity group decoded from an `EntityToGroupBox` (ISO/IEC 23008-12
/// §9.4.3) inside a `grpl` GroupsListBox. The box's FourCC names the
/// `grouping_type` (the semantics — `altr` alternatives, `ster` stereo
/// pair, etc.); the body lists the item / track IDs that share the
/// grouping characteristic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityToGroup {
    /// The `grouping_type` FourCC (the `EntityToGroupBox`'s box type) —
    /// e.g. `altr` (alternatives, only one to be played), `ster` (a
    /// two-entity stereo pair, entity 0 = left / entity 1 = right).
    pub grouping_type: [u8; 4],
    /// `group_id` (§9.4.3.3) — the group's unique non-negative ID,
    /// distinct from any other group ID, item ID, or track ID at its
    /// hierarchy level.
    pub group_id: u32,
    /// `entity_id`s (§9.4.3.3) — the item IDs (or, at file level, track
    /// IDs) mapped to this group, in list order. For `ster` the first is
    /// the left view and the second the right.
    pub entity_ids: Vec<u32>,
}

/// Decoded `grpl` (GroupsListBox, ISO/IEC 23008-12 §9.4.2) — every
/// `EntityToGroupBox` it contains, in on-wire order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityGroups {
    /// One record per `EntityToGroupBox` child, in file order.
    pub groups: Vec<EntityToGroup>,
}

impl EntityGroups {
    /// True when the `grpl` carried no `EntityToGroupBox` children.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The entity groups of a given `grouping_type` (e.g. all `altr`
    /// alternative sets), in file order.
    pub fn by_type(&self, grouping_type: [u8; 4]) -> impl Iterator<Item = &EntityToGroup> {
        self.groups
            .iter()
            .filter(move |g| g.grouping_type == grouping_type)
    }

    /// The group with a given `group_id`, if any.
    pub fn by_id(&self, group_id: u32) -> Option<&EntityToGroup> {
        self.groups.iter().find(|g| g.group_id == group_id)
    }
}

/// A `meta` box's item infrastructure (ISO/IEC 14496-12 §8.11),
/// collected together. Any of the four child boxes may be absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaItems {
    /// `pitm` (§8.11.4) — the primary item's ID, if a `pitm` is present.
    pub primary_item_id: Option<u32>,
    /// The handler type from the `meta`'s `hdlr` (§8.4.3) — e.g. `pict`
    /// for HEIF still images, `mdir` for iTunes metadata. All-zero when
    /// no `hdlr` was found.
    pub handler_type: [u8; 4],
    /// `iloc` (§8.11.3), if present.
    pub iloc: Option<IlocBox>,
    /// `iinf` (§8.11.6), if present.
    pub iinf: Option<IinfBox>,
    /// `iref` (§8.11.12), if present.
    pub iref: Option<IrefBox>,
    /// `idat` (ItemDataBox, §8.11.11) bytes, if present. Items whose
    /// `iloc` `construction_method == 1` address their extents relative
    /// to the start of this buffer (the §8.11.3.3 idat data origin).
    /// Empty when no `idat` box is present.
    pub idat: Vec<u8>,
    /// `fiin` (FD Item Information Box, §8.13.2), if present. Carries the
    /// File Delivery partitioning (`paen` → `fpar`/`fecr`/`fire`) plus the
    /// optional FD session-group (`segr`) and group-name (`gitn`) boxes.
    /// `None` for a non-FD `meta` (the common HEIF / iTunes case).
    pub fiin: Option<crate::fd::FiinBox>,
    /// `iprp` (ItemPropertiesBox, ISO/IEC 23008-12 §9.3.1), if present.
    /// The HEIF item-properties family: the `ipco` property list plus the
    /// `ipma` per-item associations. `None` for a `meta` carrying no
    /// `iprp` (every non-HEIF `meta`, and HEIF files written without
    /// properties). Resolve an item's properties via
    /// [`ItemProperties::properties_for`].
    pub iprp: Option<ItemProperties>,
    /// `grpl` (GroupsListBox, ISO/IEC 23008-12 §9.4.2), if present. The
    /// HEIF entity groups (`altr` alternatives, `ster` stereo pairs, …)
    /// mapping item / track IDs into named groups. `None` for a `meta`
    /// carrying no `grpl`.
    pub grpl: Option<EntityGroups>,
}

/// A resolved byte range for one item extent, relative to a data origin
/// determined by the item's `construction_method` (ISO/IEC 14496-12
/// §8.11.3.3). For `construction_method == 0` (file) the range is an
/// absolute file offset; for `construction_method == 1` (idat) it is an
/// offset into the `meta` box's `idat` buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemByteRange {
    /// Start offset (from the data origin selected by the construction
    /// method). `base_offset + extent_offset`.
    pub offset: u64,
    /// Length in bytes.
    pub length: u64,
}

impl MetaItems {
    /// True when this `meta` box carries no item infrastructure at all
    /// (no `pitm` / `iloc` / `iinf` / `iref` / `idat`) — e.g. a plain
    /// iTunes `meta` whose payload is just `hdlr` + `ilst`.
    pub fn is_empty(&self) -> bool {
        self.primary_item_id.is_none()
            && self.iloc.is_none()
            && self.iinf.is_none()
            && self.iref.is_none()
            && self.idat.is_empty()
            && self.fiin.is_none()
            && self.iprp.is_none()
            && self.grpl.is_none()
    }

    /// Look up an item's `iloc` location record by item ID.
    pub fn iloc_item(&self, item_id: u32) -> Option<&IlocItem> {
        self.iloc
            .as_ref()?
            .items
            .iter()
            .find(|it| it.item_id == item_id)
    }

    /// Look up an item's `iinf` info entry by item ID.
    pub fn item_info(&self, item_id: u32) -> Option<&ItemInfoEntry> {
        self.iinf
            .as_ref()?
            .entries
            .iter()
            .find(|e| e.item_id == item_id)
    }

    /// Resolve the byte ranges of an item's extents, applying the item's
    /// `base_offset` (§8.11.3.3) to each extent offset. The returned
    /// ranges are relative to the data origin selected by the item's
    /// `construction_method` (file offset for 0, `idat` offset for 1);
    /// callers using method 0 add the file base, callers using method 1
    /// index into [`MetaItems::idat`].
    ///
    /// Returns `None` when the item ID is not in `iloc`. An empty extent
    /// list yields an empty vec.
    pub fn item_byte_ranges(&self, item_id: u32) -> Option<Vec<ItemByteRange>> {
        let it = self.iloc_item(item_id)?;
        Some(
            it.extents
                .iter()
                .map(|ex| ItemByteRange {
                    offset: it.base_offset.saturating_add(ex.extent_offset),
                    length: ex.extent_length,
                })
                .collect(),
        )
    }

    /// Materialise the bytes of an item whose data is stored inside the
    /// `meta` box's `idat` (ItemDataBox, §8.11.11) — i.e. whose `iloc`
    /// `construction_method == 1`. The item's extents are concatenated
    /// in order. Returns `None` when:
    ///   * the item ID is not in `iloc`,
    ///   * the item's construction method is not 1 (idat), or
    ///   * any extent range overruns the available `idat` bytes.
    ///
    /// Items addressed by file offset (`construction_method == 0`) are
    /// not resolved here — the demuxer does not re-read the input for
    /// untimed items; use [`MetaItems::item_byte_ranges`] to obtain
    /// their absolute file offsets and read them externally.
    pub fn item_data_from_idat(&self, item_id: u32) -> Option<Vec<u8>> {
        let it = self.iloc_item(item_id)?;
        if it.construction_method != 1 {
            return None;
        }
        let mut out = Vec::new();
        for ex in &it.extents {
            let start = it.base_offset.checked_add(ex.extent_offset)? as usize;
            // length 0 means "entire source" for a single-extent item
            // (§8.11.3.3); for idat that is the rest of the buffer.
            let len = if ex.extent_length == 0 && it.extents.len() == 1 {
                self.idat.len().checked_sub(start)?
            } else {
                ex.extent_length as usize
            };
            let end = start.checked_add(len)?;
            if end > self.idat.len() {
                return None;
            }
            out.extend_from_slice(&self.idat[start..end]);
        }
        Some(out)
    }
}

/// Read a big-endian unsigned integer of `n` bytes (n ∈ {0, 4, 8}, the
/// `iloc` field-width set) from `buf` at `pos`, advancing `pos`. A width
/// of 0 yields 0 without consuming bytes (the §8.11.3.3 implied-value
/// convention). Returns `None` if the field would overrun `buf`.
fn read_sized_be(buf: &[u8], pos: &mut usize, n: usize) -> Option<u64> {
    if n == 0 {
        return Some(0);
    }
    if *pos + n > buf.len() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in &buf[*pos..*pos + n] {
        v = (v << 8) | b as u64;
    }
    *pos += n;
    Some(v)
}

/// Parse an `iloc` (ItemLocationBox, ISO/IEC 14496-12 §8.11.3) body
/// (the bytes after the box header). Returns `None` on a truncated or
/// malformed box rather than aborting the enclosing `meta` walk.
pub fn parse_iloc_box(body: &[u8]) -> Option<IlocBox> {
    if body.len() < 6 {
        return None;
    }
    let version = body[0];
    if version > 2 {
        return None;
    }
    // body[1..4] are flags (must be 0 per §8.11.3.2); skipped.
    let mut pos = 4usize;
    let b = body[pos];
    let offset_size = b >> 4;
    let length_size = b & 0x0F;
    pos += 1;
    let b = body[pos];
    let base_offset_size = b >> 4;
    // The low nibble is `index_size` for versions 1/2, reserved (0) for 0.
    let index_size = if version == 1 || version == 2 {
        b & 0x0F
    } else {
        0
    };
    pos += 1;
    // §8.11.3.2: each *_size must be one of {0, 4, 8}.
    for &s in &[offset_size, length_size, base_offset_size, index_size] {
        if s != 0 && s != 4 && s != 8 {
            return None;
        }
    }
    let item_count = if version < 2 {
        read_sized_be(body, &mut pos, 2)?
    } else {
        read_sized_be(body, &mut pos, 4)?
    };
    let mut items = Vec::new();
    for _ in 0..item_count {
        let item_id = if version < 2 {
            read_sized_be(body, &mut pos, 2)? as u32
        } else {
            read_sized_be(body, &mut pos, 4)? as u32
        };
        let construction_method = if version == 1 || version == 2 {
            // unsigned int(12) reserved + unsigned int(4) construction_method
            let w = read_sized_be(body, &mut pos, 2)?;
            (w & 0x0F) as u8
        } else {
            0
        };
        let data_reference_index = read_sized_be(body, &mut pos, 2)? as u16;
        let base_offset = read_sized_be(body, &mut pos, base_offset_size as usize)?;
        let extent_count = read_sized_be(body, &mut pos, 2)?;
        let mut extents = Vec::new();
        for _ in 0..extent_count {
            let extent_index = if (version == 1 || version == 2) && index_size > 0 {
                read_sized_be(body, &mut pos, index_size as usize)?
            } else {
                0
            };
            let extent_offset = read_sized_be(body, &mut pos, offset_size as usize)?;
            let extent_length = read_sized_be(body, &mut pos, length_size as usize)?;
            extents.push(IlocExtent {
                extent_index,
                extent_offset,
                extent_length,
            });
        }
        items.push(IlocItem {
            item_id,
            construction_method,
            data_reference_index,
            base_offset,
            extents,
        });
    }
    Some(IlocBox {
        version,
        offset_size,
        length_size,
        base_offset_size,
        index_size,
        items,
    })
}

/// Parse a `pitm` (PrimaryItemBox, ISO/IEC 14496-12 §8.11.4) body and
/// return the primary `item_ID`. v0 → 16-bit, v≥1 → 32-bit.
pub fn parse_pitm_box(body: &[u8]) -> Option<u32> {
    if body.len() < 4 {
        return None;
    }
    let version = body[0];
    let mut pos = 4usize;
    if version == 0 {
        read_sized_be(body, &mut pos, 2).map(|v| v as u32)
    } else {
        read_sized_be(body, &mut pos, 4).map(|v| v as u32)
    }
}

/// Read a NUL-terminated UTF-8 string starting at `pos`, advancing `pos`
/// past the terminator. If no NUL is found before the end of `buf`, the
/// remaining bytes are taken as the string and `pos` is set to the end.
fn read_c_string_at(buf: &[u8], pos: &mut usize) -> String {
    let start = *pos;
    let mut i = start;
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    let s = String::from_utf8_lossy(&buf[start..i]).into_owned();
    // Advance past the terminator (or to end if none).
    *pos = if i < buf.len() { i + 1 } else { buf.len() };
    s
}

/// Parse a single `infe` (ItemInfoEntry, ISO/IEC 14496-12 §8.11.6.2)
/// body (bytes after the `infe` box header). Returns `None` on a
/// truncated header.
fn parse_infe(body: &[u8]) -> Option<ItemInfoEntry> {
    if body.len() < 4 {
        return None;
    }
    let version = body[0];
    let mut pos = 4usize;
    let mut e = ItemInfoEntry {
        version,
        ..Default::default()
    };
    if version == 0 || version == 1 {
        e.item_id = read_sized_be(body, &mut pos, 2)? as u32;
        e.protection_index = read_sized_be(body, &mut pos, 2)? as u16;
        e.item_name = read_c_string_at(body, &mut pos);
        e.content_type = read_c_string_at(body, &mut pos);
        // content_encoding is optional; present only if bytes remain.
        if pos < body.len() {
            e.content_encoding = read_c_string_at(body, &mut pos);
        }
        // version 1 may carry an extension_type + extension; ignored here.
    } else {
        // version >= 2
        e.item_id = if version == 2 {
            read_sized_be(body, &mut pos, 2)? as u32
        } else {
            read_sized_be(body, &mut pos, 4)? as u32
        };
        e.protection_index = read_sized_be(body, &mut pos, 2)? as u16;
        if pos + 4 > body.len() {
            return None;
        }
        e.item_type.copy_from_slice(&body[pos..pos + 4]);
        pos += 4;
        e.item_name = read_c_string_at(body, &mut pos);
        if &e.item_type == b"mime" {
            e.content_type = read_c_string_at(body, &mut pos);
            if pos < body.len() {
                e.content_encoding = read_c_string_at(body, &mut pos);
            }
        } else if &e.item_type == b"uri " {
            // item_uri_type → carried in content_encoding to avoid a
            // separate field (documented on ItemInfoEntry).
            e.content_encoding = read_c_string_at(body, &mut pos);
        }
    }
    Some(e)
}

/// Parse an `iinf` (ItemInfoBox, ISO/IEC 14496-12 §8.11.6) body. Walks
/// the child `infe` boxes. Malformed children are skipped; a truncated
/// header aborts the walk gracefully (returns whatever parsed cleanly).
pub fn parse_iinf_box(body: &[u8]) -> Option<IinfBox> {
    if body.len() < 4 {
        return None;
    }
    let version = body[0];
    let mut pos = 4usize;
    let entry_count = if version == 0 {
        read_sized_be(body, &mut pos, 2)?
    } else {
        read_sized_be(body, &mut pos, 4)?
    };
    let mut out = IinfBox::default();
    let mut cur = std::io::Cursor::new(&body[pos..]);
    let region = &body[pos..];
    let end = region.len() as u64;
    let mut seen = 0u64;
    while cur.position() < end && seen < entry_count {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let start = cur.position() as usize;
        if start + psz > region.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        if hdr.fourcc == INFE {
            if let Some(e) = parse_infe(&region[start..start + psz]) {
                out.entries.push(e);
            }
        }
        seen += 1;
    }
    Some(out)
}

/// Parse an `iref` (ItemReferenceBox, ISO/IEC 14496-12 §8.11.12) body.
/// Each child is a `SingleItemTypeReferenceBox` whose box type IS the
/// reference type.
pub fn parse_iref_box(body: &[u8]) -> Option<IrefBox> {
    if body.len() < 4 {
        return None;
    }
    let version = body[0];
    if version > 1 {
        return None;
    }
    let id_bytes = if version == 0 { 2usize } else { 4usize };
    let region = &body[4..];
    let mut cur = std::io::Cursor::new(region);
    let end = region.len() as u64;
    let mut references = Vec::new();
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let start = cur.position() as usize;
        if start + psz > region.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        let sub = &region[start..start + psz];
        let mut p = 0usize;
        let from_item_id = match read_sized_be(sub, &mut p, id_bytes) {
            Some(v) => v as u32,
            None => continue,
        };
        let ref_count = match read_sized_be(sub, &mut p, 2) {
            Some(v) => v,
            None => continue,
        };
        let mut to_item_ids = Vec::new();
        for _ in 0..ref_count {
            match read_sized_be(sub, &mut p, id_bytes) {
                Some(v) => to_item_ids.push(v as u32),
                None => break,
            }
        }
        references.push(ItemReference {
            reference_type: hdr.fourcc,
            from_item_id,
            to_item_ids,
        });
    }
    Some(IrefBox {
        version,
        references,
    })
}

/// Walk a `meta` box body (bytes after its FullBox preamble removed by
/// the caller is NOT assumed — this takes the full body and skips the
/// 4-byte version/flags itself) and collect the §8.11 item
/// infrastructure (`pitm` / `iloc` / `iinf` / `iref`) plus the `hdlr`
/// handler type. Returns an all-`None` [`MetaItems`] for a plain iTunes
/// `meta` (which carries only `hdlr` + `ilst`).
pub fn parse_meta_items(body: &[u8]) -> MetaItems {
    let mut out = MetaItems::default();
    if body.len() < 4 {
        return out;
    }
    let region = &body[4..];
    let mut cur = std::io::Cursor::new(region);
    let end = region.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let start = cur.position() as usize;
        if start + psz > region.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        let child = &region[start..start + psz];
        match hdr.fourcc {
            // §8.4.3 hdlr body: version/flags(4) + pre_defined(4) +
            // handler_type(4) + … — capture handler_type.
            HDLR if child.len() >= 12 => {
                out.handler_type.copy_from_slice(&child[8..12]);
            }
            PITM => {
                if let Some(id) = parse_pitm_box(child) {
                    out.primary_item_id = Some(id);
                }
            }
            ILOC => out.iloc = parse_iloc_box(child),
            IINF => out.iinf = parse_iinf_box(child),
            IREF => out.iref = parse_iref_box(child),
            // §8.11.11 ItemDataBox — raw bytes; the data origin for
            // construction_method 1 (idat) items.
            IDAT => out.idat = child.to_vec(),
            // §8.13.2 FD Item Information Box — File Delivery partitioning.
            FIIN => out.fiin = crate::fd::parse_fiin_box(child),
            // ISO/IEC 23008-12 §9.3 ItemPropertiesBox — HEIF item
            // properties (`ipco` list + `ipma` associations).
            IPRP => {
                let p = parse_iprp_box(child);
                if !p.is_empty() {
                    out.iprp = Some(p);
                }
            }
            // ISO/IEC 23008-12 §9.4 GroupsListBox — HEIF entity groups.
            GRPL => {
                let g = parse_grpl_box(child);
                if !g.is_empty() {
                    out.grpl = Some(g);
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HEIF / MIAF item-properties parsers (ISO/IEC 23008-12 §9.3 / §6.5). The
// `iprp` ItemPropertiesBox holds one `ipco` (ItemPropertyContainerBox — an
// implicitly 1-indexed list of property boxes) and one or more `ipma`
// (ItemPropertyAssociation) boxes mapping each item to its property indices.
// ---------------------------------------------------------------------------

/// Parse an `iprp` (ItemPropertiesBox, ISO/IEC 23008-12 §9.3.1) body (the
/// bytes after the box header). Walks the single `ipco` child for the
/// property list and every `ipma` child for the per-item associations.
/// A malformed instance yields an empty [`ItemProperties`] (the caller
/// drops it) rather than aborting the enclosing `meta` walk.
pub fn parse_iprp_box(body: &[u8]) -> ItemProperties {
    let mut out = ItemProperties::default();
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = match hdr.payload_size() {
            Some(p) => p as usize,
            None => break,
        };
        let start = cur.position() as usize;
        if start + psz > body.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        let child = &body[start..start + psz];
        match hdr.fourcc {
            IPCO => out.properties = parse_ipco_box(child),
            IPMA => out.associations.extend(parse_ipma_box(child)),
            _ => {}
        }
    }
    out
}

/// Parse an `ipco` (ItemPropertyContainerBox, ISO/IEC 23008-12 §9.3.1)
/// body into the implicitly-indexed property list. Each child box is
/// decoded into the typed [`ItemProperty`] variant for its FourCC; an
/// unrecognised box type is preserved verbatim as [`ItemProperty::Other`]
/// so its 1-based index slot is never dropped (the index an `ipma`
/// references must stay stable). A `free` / `skip` box (§9.3.1 allows
/// it inside `ipco`, occupying an index value but carrying no meaning) is
/// preserved as `Other` too so subsequent indices stay aligned.
pub fn parse_ipco_box(body: &[u8]) -> Vec<ItemProperty> {
    let mut out = Vec::new();
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = match hdr.payload_size() {
            Some(p) => p as usize,
            None => break,
        };
        let start = cur.position() as usize;
        if start + psz > body.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        let child = &body[start..start + psz];
        out.push(parse_item_property(hdr.fourcc, child));
    }
    out
}

/// Decode one `ipco` child box `(box_type, body)` into a typed
/// [`ItemProperty`]. `body` is the box payload (after the box header). For
/// the `ItemFullProperty` types (`ispe` / `pixi` / `rloc` / `auxC`) the
/// body opens with the 4-byte FullBox version/flags word. A child that is
/// truncated for its declared type falls back to [`ItemProperty::Other`]
/// so the index slot survives.
fn parse_item_property(box_type: [u8; 4], body: &[u8]) -> ItemProperty {
    let other = || ItemProperty::Other {
        box_type,
        body: body.to_vec(),
    };
    match box_type {
        // §6.5.3 ispe: FullBox(0,0) + u32 width + u32 height.
        ISPE => {
            if body.len() < 12 {
                return other();
            }
            ItemProperty::Ispe {
                image_width: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                image_height: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
            }
        }
        // §6.5.6 pixi: FullBox(0,0) + u8 num_channels + num_channels × u8.
        PIXI => {
            if body.len() < 5 {
                return other();
            }
            let num = body[4] as usize;
            if 5 + num > body.len() {
                return other();
            }
            ItemProperty::Pixi {
                bits_per_channel: body[5..5 + num].to_vec(),
            }
        }
        // §6.5.7 rloc: FullBox(0,0) + u32 horizontal + u32 vertical.
        RLOC => {
            if body.len() < 12 {
                return other();
            }
            ItemProperty::Rloc {
                horizontal_offset: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                vertical_offset: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
            }
        }
        // §6.5.8 auxC: FullBox(0,flags) + null-terminated UTF-8 URN +
        // type-specific aux_subtype tail.
        AUXC => {
            if body.len() < 4 {
                return other();
            }
            let rest = &body[4..];
            let nul = rest.iter().position(|&b| b == 0);
            let (type_bytes, sub) = match nul {
                Some(i) => (&rest[..i], &rest[i + 1..]),
                None => (rest, &rest[rest.len()..]),
            };
            ItemProperty::AuxC {
                aux_type: String::from_utf8_lossy(type_bytes).to_string(),
                aux_subtype: sub.to_vec(),
            }
        }
        // §6.5.10 irot: plain Box; 1 byte = 6 reserved + 2-bit angle.
        IROT => {
            if body.is_empty() {
                return other();
            }
            ItemProperty::Irot {
                angle: body[0] & 0x03,
            }
        }
        // §6.5.12 imir: plain Box; 1 byte = 7 reserved + 1-bit axis.
        IMIR => {
            if body.is_empty() {
                return other();
            }
            ItemProperty::Imir {
                axis: body[0] & 0x01,
            }
        }
        // §6.5.11 lsel: plain Box; u16 layer_id.
        LSEL => {
            if body.len() < 2 {
                return other();
            }
            ItemProperty::Lsel {
                layer_id: u16::from_be_bytes([body[0], body[1]]),
            }
        }
        // §6.5.20 udes: FullBox(0,0) + four NUL-terminated UTF-8 strings.
        UDES => {
            if body.len() < 4 {
                return other();
            }
            let mut p = 4usize;
            let lang = read_c_string_at(body, &mut p);
            let name = read_c_string_at(body, &mut p);
            let description = read_c_string_at(body, &mut p);
            let tags = read_c_string_at(body, &mut p);
            ItemProperty::Udes {
                lang,
                name,
                description,
                tags,
            }
        }
        // §6.5.21 altt: FullBox(0,0) + alt_text + alt_lang (NUL-terminated).
        ALTT => {
            if body.len() < 4 {
                return other();
            }
            let mut p = 4usize;
            let alt_text = read_c_string_at(body, &mut p);
            let alt_lang = read_c_string_at(body, &mut p);
            ItemProperty::Altt { alt_text, alt_lang }
        }
        // §6.5.13 iscl: FullBox(0,0) + four u16 numerator/denominator.
        ISCL => {
            if body.len() < 12 {
                return other();
            }
            ItemProperty::Iscl {
                target_width_numerator: u16::from_be_bytes([body[4], body[5]]),
                target_width_denominator: u16::from_be_bytes([body[6], body[7]]),
                target_height_numerator: u16::from_be_bytes([body[8], body[9]]),
                target_height_denominator: u16::from_be_bytes([body[10], body[11]]),
            }
        }
        // §6.5.17 rref: FullBox(0,0) + u8 count + count × u32 FourCC.
        RREF => {
            if body.len() < 5 {
                return other();
            }
            let count = body[4] as usize;
            if 5 + count * 4 > body.len() {
                return other();
            }
            let mut reference_types = Vec::with_capacity(count);
            for i in 0..count {
                let o = 5 + i * 4;
                reference_types.push([body[o], body[o + 1], body[o + 2], body[o + 3]]);
            }
            ItemProperty::Rref { reference_types }
        }
        // §6.5.18 crtt: FullBox(0,0) + u64 creation_time.
        CRTT => {
            if body.len() < 12 {
                return other();
            }
            ItemProperty::Crtt {
                creation_time: u64::from_be_bytes([
                    body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
                ]),
            }
        }
        // §6.5.19 mdft: FullBox(0,0) + u64 modification_time.
        MDFT => {
            if body.len() < 12 {
                return other();
            }
            ItemProperty::Mdft {
                modification_time: u64::from_be_bytes([
                    body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
                ]),
            }
        }
        // §6.5.4 / §6.5.9 / §6.5.5 — same syntax as the 14496-12 boxes.
        _ if &box_type == b"pasp" => parse_pasp(body)
            .map(ItemProperty::Pasp)
            .unwrap_or_else(other),
        _ if &box_type == b"clap" => parse_clap(body)
            .map(ItemProperty::Clap)
            .unwrap_or_else(other),
        _ if &box_type == b"colr" => parse_colr(body)
            .map(ItemProperty::Colr)
            .unwrap_or_else(other),
        _ => other(),
    }
}

/// Parse an `ipma` (ItemPropertyAssociation, ISO/IEC 23008-12 §9.3.1)
/// body into per-item association lists. `version` selects the item-ID
/// width (0 → 16-bit, ≥ 1 → 32-bit); `flags & 1` selects the
/// `property_index` width (0 → 7-bit, 1 → 15-bit), with the top bit of the
/// field carrying `essential`. A truncated entry ends the walk at what was
/// read rather than inventing associations.
pub fn parse_ipma_box(body: &[u8]) -> Vec<ItemPropertyAssociationEntry> {
    let mut out = Vec::new();
    if body.len() < 8 {
        return out;
    }
    let version = body[0];
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let wide_index = flags & 1 == 1;
    let mut pos = 4usize;
    let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    pos += 4;
    for _ in 0..entry_count {
        let item_id = if version == 0 {
            if pos + 2 > body.len() {
                break;
            }
            let v = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            v
        } else {
            if pos + 4 > body.len() {
                break;
            }
            let v = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
            pos += 4;
            v
        };
        if pos >= body.len() {
            break;
        }
        let assoc_count = body[pos] as usize;
        pos += 1;
        let mut associations = Vec::with_capacity(assoc_count);
        let mut truncated = false;
        for _ in 0..assoc_count {
            if wide_index {
                if pos + 2 > body.len() {
                    truncated = true;
                    break;
                }
                let w = u16::from_be_bytes([body[pos], body[pos + 1]]);
                pos += 2;
                associations.push(PropertyAssociation {
                    essential: w & 0x8000 != 0,
                    property_index: w & 0x7FFF,
                });
            } else {
                if pos + 1 > body.len() {
                    truncated = true;
                    break;
                }
                let b = body[pos];
                pos += 1;
                associations.push(PropertyAssociation {
                    essential: b & 0x80 != 0,
                    property_index: (b & 0x7F) as u16,
                });
            }
        }
        out.push(ItemPropertyAssociationEntry {
            item_id,
            associations,
        });
        if truncated {
            break;
        }
    }
    out
}

/// Parse a `grpl` (GroupsListBox, ISO/IEC 23008-12 §9.4.2) body — a plain
/// `Box` whose payload is a sequence of `EntityToGroupBox`es (each a
/// `FullBox(grouping_type, version, flags)`). Each child's FourCC is the
/// `grouping_type`; its body is `group_id` (u32) + `num_entities_in_group`
/// (u32) + that many `entity_id`s (u32). A child whose declared
/// `num_entities_in_group` overruns its body is dropped (it contributes no
/// group) rather than reading past the end; the walk continues with the
/// next child. A truncated child header ends the walk cleanly.
pub fn parse_grpl_box(body: &[u8]) -> EntityGroups {
    let mut out = EntityGroups::default();
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur).ok().flatten() {
            Some(h) => h,
            None => break,
        };
        let psz = match hdr.payload_size() {
            Some(p) => p as usize,
            None => break,
        };
        let start = cur.position() as usize;
        if start + psz > body.len() {
            break;
        }
        cur.set_position((start + psz) as u64);
        let child = &body[start..start + psz];
        if let Some(g) = parse_entity_to_group(hdr.fourcc, child) {
            out.groups.push(g);
        }
    }
    out
}

/// Decode one `EntityToGroupBox` (§9.4.3.2) `(grouping_type, body)` where
/// `body` opens with the 4-byte FullBox version/flags word. Returns `None`
/// for a body too short for the two fixed u32 fields or whose
/// `num_entities_in_group` overruns the available bytes.
fn parse_entity_to_group(grouping_type: [u8; 4], body: &[u8]) -> Option<EntityToGroup> {
    // FullBox preamble (4) + group_id (4) + num_entities_in_group (4).
    if body.len() < 12 {
        return None;
    }
    let group_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let num = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;
    let mut pos = 12usize;
    if pos + num * 4 > body.len() {
        return None;
    }
    let mut entity_ids = Vec::with_capacity(num);
    for _ in 0..num {
        entity_ids.push(u32::from_be_bytes([
            body[pos],
            body[pos + 1],
            body[pos + 2],
            body[pos + 3],
        ]));
        pos += 4;
    }
    Some(EntityToGroup {
        grouping_type,
        group_id,
        entity_ids,
    })
}

/// Append the flat `meta_*` metadata keys for a parsed file-level `meta`
/// box (§8.11). No keys are emitted for an empty / absent `meta`.
fn surface_meta_items(m: &MetaItems, metadata: &mut Vec<(String, String)>) {
    if m.handler_type == [0; 4] && m.is_empty() {
        return;
    }
    if m.handler_type != [0; 4] {
        metadata.push((
            "meta_handler".to_string(),
            String::from_utf8_lossy(&m.handler_type).to_string(),
        ));
    }
    if let Some(id) = m.primary_item_id {
        metadata.push(("meta_primary_item".to_string(), id.to_string()));
    }
    if let Some(iinf) = m.iinf.as_ref() {
        metadata.push((
            "meta_item_count".to_string(),
            iinf.entries.len().to_string(),
        ));
        for (n, e) in iinf.entries.iter().enumerate() {
            let type_tok = if e.version >= 2 {
                String::from_utf8_lossy(&e.item_type).to_string()
            } else {
                format!("v{}", e.version)
            };
            metadata.push((
                format!("meta_item_{n}"),
                format!("id={} type={} name={}", e.item_id, type_tok, e.item_name),
            ));
        }
    }
    if let Some(iloc) = m.iloc.as_ref() {
        metadata.push(("meta_iloc_count".to_string(), iloc.items.len().to_string()));
        // Surface each item's location summary so a HEIF consumer can spot
        // where an item lives (construction method + extent layout +
        // total length) without downcasting to `MetaItems::iloc`. The
        // exact byte ranges remain on the typed record / `item_byte_ranges`.
        for (n, it) in iloc.items.iter().enumerate() {
            let total_len: u64 = it.extents.iter().map(|e| e.extent_length).sum();
            metadata.push((
                format!("meta_iloc_{n}"),
                format!(
                    "id={} method={} extents={} length={}",
                    it.item_id,
                    it.construction_method,
                    it.extents.len(),
                    total_len
                ),
            ));
        }
    }
    if let Some(iref) = m.iref.as_ref() {
        metadata.push((
            "meta_iref_count".to_string(),
            iref.references.len().to_string(),
        ));
        // Surface each typed reference group so the HEIF relationship graph
        // (thumbnail `thmb`, auxiliary `auxl`, derivation `dimg`,
        // description `cdsc`, pre-derived `base`, predictive `pred`,
        // tile-base `tbas`, scalable-base `exbl`, …) is queryable from the
        // flat channel without downcasting to `MetaItems::iref`.
        for (n, r) in iref.references.iter().enumerate() {
            let to: Vec<String> = r.to_item_ids.iter().map(|id| id.to_string()).collect();
            metadata.push((
                format!("meta_iref_{n}"),
                format!(
                    "type={} from={} to={}",
                    String::from_utf8_lossy(&r.reference_type),
                    r.from_item_id,
                    to.join(",")
                ),
            ));
        }
    }
    if !m.idat.is_empty() {
        metadata.push(("meta_idat_len".to_string(), m.idat.len().to_string()));
    }
    if let Some(fiin) = m.fiin.as_ref() {
        // §8.13.2 FD Item Information — surface the partition-entry count
        // plus whether the optional session-group / group-name boxes are
        // present, so an FD-aware consumer can spot the box without
        // reaching into the typed `MetaItems::fiin`.
        metadata.push((
            "meta_fiin_partitions".to_string(),
            fiin.partition_entries.len().to_string(),
        ));
        if let Some(segr) = fiin.session_info.as_ref() {
            metadata.push((
                "meta_fiin_session_groups".to_string(),
                segr.session_groups.len().to_string(),
            ));
        }
        if let Some(gitn) = fiin.group_id_to_name.as_ref() {
            metadata.push((
                "meta_fiin_group_names".to_string(),
                gitn.entries.len().to_string(),
            ));
        }
    }
    if let Some(iprp) = m.iprp.as_ref() {
        // ISO/IEC 23008-12 §9.3 ItemPropertiesBox — surface a compact
        // summary: the property-list size, a per-property type token, and
        // each item's resolved association list. A consumer wanting the
        // typed records reaches `MetaItems::iprp` / `properties_for`.
        metadata.push((
            "meta_iprp_property_count".to_string(),
            iprp.properties.len().to_string(),
        ));
        for (n, p) in iprp.properties.iter().enumerate() {
            metadata.push((format!("meta_iprp_property_{n}"), item_property_token(p)));
        }
        for (n, e) in iprp.associations.iter().enumerate() {
            // Each association rendered as `idx` or `idx*` (the `*` marks
            // an essential property), space-separated, in `ipma` order.
            let assoc: Vec<String> = e
                .associations
                .iter()
                .map(|a| {
                    if a.essential {
                        format!("{}*", a.property_index)
                    } else {
                        a.property_index.to_string()
                    }
                })
                .collect();
            metadata.push((
                format!("meta_iprp_item_{n}"),
                format!("id={} props={}", e.item_id, assoc.join(",")),
            ));
        }
    }
    if let Some(grpl) = m.grpl.as_ref() {
        // ISO/IEC 23008-12 §9.4 GroupsListBox — surface a per-group
        // summary: the group count plus, per group, the grouping type, the
        // group ID, and the mapped entity IDs.
        metadata.push((
            "meta_grpl_group_count".to_string(),
            grpl.groups.len().to_string(),
        ));
        for (n, g) in grpl.groups.iter().enumerate() {
            let ids: Vec<String> = g.entity_ids.iter().map(|e| e.to_string()).collect();
            metadata.push((
                format!("meta_grpl_group_{n}"),
                format!(
                    "type={} id={} entities={}",
                    String::from_utf8_lossy(&g.grouping_type),
                    g.group_id,
                    ids.join(",")
                ),
            ));
        }
    }
}

/// Render one [`ItemProperty`] as a compact metadata token: the property
/// FourCC followed by its salient decoded value(s). Used by the
/// `meta_iprp_property_<n>` summary surface.
fn item_property_token(p: &ItemProperty) -> String {
    match p {
        ItemProperty::Ispe {
            image_width,
            image_height,
        } => format!("ispe {image_width}x{image_height}"),
        ItemProperty::Pixi { bits_per_channel } => {
            let bits: Vec<String> = bits_per_channel.iter().map(|b| b.to_string()).collect();
            format!("pixi {}", bits.join(":"))
        }
        ItemProperty::Rloc {
            horizontal_offset,
            vertical_offset,
        } => format!("rloc {horizontal_offset},{vertical_offset}"),
        ItemProperty::AuxC { aux_type, .. } => format!("auxC {aux_type}"),
        ItemProperty::Irot { angle } => format!("irot {}", *angle as u32 * 90),
        ItemProperty::Imir { axis } => {
            format!("imir {}", if *axis == 0 { "v" } else { "h" })
        }
        ItemProperty::Lsel { layer_id } => format!("lsel {layer_id}"),
        ItemProperty::Pasp(r) => format!("pasp {}:{}", r.h_spacing, r.v_spacing),
        ItemProperty::Clap(r) => format!("clap {}/{}", r.width_n, r.width_d),
        ItemProperty::Colr(c) => match c {
            ColrRecord::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                ..
            } => format!(
                "colr nclx {colour_primaries}/{transfer_characteristics}/{matrix_coefficients}"
            ),
            ColrRecord::RestrictedIcc(d) => format!("colr rICC {}", d.len()),
            ColrRecord::UnrestrictedIcc(d) => format!("colr prof {}", d.len()),
            ColrRecord::Other { colour_type, .. } => {
                format!("colr {}", String::from_utf8_lossy(colour_type))
            }
        },
        ItemProperty::Udes { name, lang, .. } => {
            if lang.is_empty() {
                format!("udes {name}")
            } else {
                format!("udes [{lang}] {name}")
            }
        }
        ItemProperty::Altt { alt_text, .. } => format!("altt {alt_text}"),
        ItemProperty::Iscl {
            target_width_numerator,
            target_width_denominator,
            target_height_numerator,
            target_height_denominator,
        } => format!(
            "iscl {target_width_numerator}/{target_width_denominator}x{target_height_numerator}/{target_height_denominator}"
        ),
        ItemProperty::Rref { reference_types } => {
            let types: Vec<String> = reference_types
                .iter()
                .map(|t| String::from_utf8_lossy(t).to_string())
                .collect();
            format!("rref {}", types.join(","))
        }
        ItemProperty::Crtt { creation_time } => format!("crtt {creation_time}"),
        ItemProperty::Mdft { modification_time } => format!("mdft {modification_time}"),
        ItemProperty::Other { box_type, .. } => String::from_utf8_lossy(box_type).to_string(),
    }
}

// ---------------------------------------------------------------------------
// §8.11 meta-item box builders (write counterparts to the parsers above).
// Each builder is the byte-exact inverse of its parser: a record produced
// by `parse_*` re-serialises identically, and the muxer / a caller
// assembling a HEIF `meta` box can emit valid §8.11 boxes.
// ---------------------------------------------------------------------------

/// Append `value` as a big-endian integer of `n` bytes (n ∈ {0, 4, 8})
/// to `out`. A width of 0 writes nothing (the §8.11.3.3 implied-value
/// convention). The low `n*8` bits of `value` are written.
fn write_sized_be(out: &mut Vec<u8>, value: u64, n: usize) {
    if n == 0 {
        return;
    }
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[8 - n..]);
}

/// Wrap a `body` in a complete `[size:u32][fourcc]` box header (32-bit
/// size form). The §4.2 largesize form is not needed for the small meta
/// boxes this module emits.
fn wrap_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&total.to_be_bytes());
    v.extend_from_slice(fourcc);
    v.extend_from_slice(body);
    v
}

/// Build a complete `iloc` (ItemLocationBox, ISO/IEC 14496-12 §8.11.3)
/// box. Field widths are taken from the record verbatim, so the output
/// round-trips byte-exact through [`parse_iloc_box`].
///
/// Returns `None` when the record is internally inconsistent in a way
/// that would not round-trip: a `*_size` outside {0, 4, 8}, a version 0
/// item carrying a non-zero `construction_method` (the field does not
/// exist on the wire for v0), or an `index_size > 0` on a version 0 box
/// (the index nibble is reserved for v0).
pub fn build_iloc_box(b: &IlocBox) -> Option<Vec<u8>> {
    for &s in &[
        b.offset_size,
        b.length_size,
        b.base_offset_size,
        b.index_size,
    ] {
        if s != 0 && s != 4 && s != 8 {
            return None;
        }
    }
    if b.version > 2 {
        return None;
    }
    if b.version == 0 && b.index_size != 0 {
        return None;
    }
    let mut body = Vec::new();
    body.push(b.version);
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.push((b.offset_size << 4) | (b.length_size & 0x0F));
    let lo = if b.version == 1 || b.version == 2 {
        b.index_size & 0x0F
    } else {
        0
    };
    body.push((b.base_offset_size << 4) | lo);
    if b.version < 2 {
        write_sized_be(&mut body, b.items.len() as u64, 2);
    } else {
        write_sized_be(&mut body, b.items.len() as u64, 4);
    }
    for it in &b.items {
        if b.version < 2 {
            write_sized_be(&mut body, it.item_id as u64, 2);
        } else {
            write_sized_be(&mut body, it.item_id as u64, 4);
        }
        if b.version == 1 || b.version == 2 {
            if it.construction_method > 0x0F {
                return None;
            }
            // 12 reserved bits + 4-bit construction_method.
            write_sized_be(&mut body, it.construction_method as u64, 2);
        } else if it.construction_method != 0 {
            return None;
        }
        write_sized_be(&mut body, it.data_reference_index as u64, 2);
        write_sized_be(&mut body, it.base_offset, b.base_offset_size as usize);
        write_sized_be(&mut body, it.extents.len() as u64, 2);
        for ex in &it.extents {
            if (b.version == 1 || b.version == 2) && b.index_size > 0 {
                write_sized_be(&mut body, ex.extent_index, b.index_size as usize);
            }
            write_sized_be(&mut body, ex.extent_offset, b.offset_size as usize);
            write_sized_be(&mut body, ex.extent_length, b.length_size as usize);
        }
    }
    Some(wrap_box(&ILOC, &body))
}

/// Build a complete `pitm` (PrimaryItemBox, ISO/IEC 14496-12 §8.11.4)
/// box. Uses version 0 (16-bit `item_ID`) when the ID fits in 16 bits,
/// else version 1 (32-bit). Round-trips through [`parse_pitm_box`].
pub fn build_pitm_box(item_id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    if item_id <= u16::MAX as u32 {
        body.push(0); // version 0
        body.extend_from_slice(&[0, 0, 0]);
        write_sized_be(&mut body, item_id as u64, 2);
    } else {
        body.push(1); // version 1
        body.extend_from_slice(&[0, 0, 0]);
        write_sized_be(&mut body, item_id as u64, 4);
    }
    wrap_box(&PITM, &body)
}

/// Build a single `infe` (ItemInfoEntry, ISO/IEC 14496-12 §8.11.6.2)
/// box from an [`ItemInfoEntry`] record. Returns `None` for an
/// unsupported version (only 0/1/2/3 are defined) or a name/type
/// string containing an embedded NUL (which would corrupt the
/// NUL-terminated framing).
fn build_infe(e: &ItemInfoEntry) -> Option<Vec<u8>> {
    if e.version > 3 {
        return None;
    }
    for s in [&e.item_name, &e.content_type, &e.content_encoding] {
        if s.as_bytes().contains(&0) {
            return None;
        }
    }
    let mut body = vec![e.version, 0, 0, 0];
    if e.version == 0 || e.version == 1 {
        write_sized_be(&mut body, e.item_id as u64, 2);
        write_sized_be(&mut body, e.protection_index as u64, 2);
        body.extend_from_slice(e.item_name.as_bytes());
        body.push(0);
        body.extend_from_slice(e.content_type.as_bytes());
        body.push(0);
        if !e.content_encoding.is_empty() {
            body.extend_from_slice(e.content_encoding.as_bytes());
            body.push(0);
        }
    } else {
        if e.version == 2 {
            write_sized_be(&mut body, e.item_id as u64, 2);
        } else {
            write_sized_be(&mut body, e.item_id as u64, 4);
        }
        write_sized_be(&mut body, e.protection_index as u64, 2);
        body.extend_from_slice(&e.item_type);
        body.extend_from_slice(e.item_name.as_bytes());
        body.push(0);
        if &e.item_type == b"mime" {
            body.extend_from_slice(e.content_type.as_bytes());
            body.push(0);
            if !e.content_encoding.is_empty() {
                body.extend_from_slice(e.content_encoding.as_bytes());
                body.push(0);
            }
        } else if &e.item_type == b"uri " {
            body.extend_from_slice(e.content_encoding.as_bytes());
            body.push(0);
        }
    }
    Some(wrap_box(&INFE, &body))
}

/// Build a complete `iinf` (ItemInfoBox, ISO/IEC 14496-12 §8.11.6) box
/// from an [`IinfBox`]. Uses version 0 (16-bit `entry_count`) when the
/// entry count fits in 16 bits, else version 1 (32-bit). Returns `None`
/// if any child `infe` cannot be serialised. Round-trips through
/// [`parse_iinf_box`].
pub fn build_iinf_box(b: &IinfBox) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    if b.entries.len() <= u16::MAX as usize {
        body.push(0); // version 0
        body.extend_from_slice(&[0, 0, 0]);
        write_sized_be(&mut body, b.entries.len() as u64, 2);
    } else {
        body.push(1); // version 1
        body.extend_from_slice(&[0, 0, 0]);
        write_sized_be(&mut body, b.entries.len() as u64, 4);
    }
    for e in &b.entries {
        body.extend_from_slice(&build_infe(e)?);
    }
    Some(wrap_box(&IINF, &body))
}

/// Build a complete `iref` (ItemReferenceBox, ISO/IEC 14496-12 §8.11.12)
/// box from an [`IrefBox`]. The record's `version` selects 16-bit (v0)
/// or 32-bit (v1) item IDs. Returns `None` for an unsupported version
/// or a reference group whose `reference_count` overflows 16 bits.
/// Round-trips through [`parse_iref_box`].
pub fn build_iref_box(b: &IrefBox) -> Option<Vec<u8>> {
    if b.version > 1 {
        return None;
    }
    let id_bytes = if b.version == 0 { 2 } else { 4 };
    let mut body = vec![b.version, 0, 0, 0];
    for r in &b.references {
        if r.to_item_ids.len() > u16::MAX as usize {
            return None;
        }
        let mut sub = Vec::new();
        write_sized_be(&mut sub, r.from_item_id as u64, id_bytes);
        write_sized_be(&mut sub, r.to_item_ids.len() as u64, 2);
        for &to in &r.to_item_ids {
            write_sized_be(&mut sub, to as u64, id_bytes);
        }
        body.extend_from_slice(&wrap_box(&r.reference_type, &sub));
    }
    Some(wrap_box(&IREF, &body))
}

/// Build a complete `idat` (ItemDataBox, ISO/IEC 14496-12 §8.11.11) box
/// wrapping the raw item bytes.
pub fn build_idat_box(data: &[u8]) -> Vec<u8> {
    wrap_box(&IDAT, data)
}

// ---------------------------------------------------------------------------
// HEIF / MIAF item-properties builders (ISO/IEC 23008-12 §9.3 / §6.5). The
// byte-exact inverses of `parse_iprp_box` / `parse_ipco_box` /
// `parse_ipma_box` / `parse_item_property`, so a caller assembling a HEIF
// `meta` from typed records re-emits the same bytes the parsers decode.
// ---------------------------------------------------------------------------

/// Serialise one [`ItemProperty`] into a complete property box (header +
/// body). The inverse of [`parse_item_property`]; the `ItemFullProperty`
/// types (`ispe` / `pixi` / `rloc` / `auxC`) emit the 4-byte FullBox
/// version/flags preamble (version 0, flags 0 — `auxC` likewise writes
/// flags 0). [`ItemProperty::Other`] re-emits its preserved bytes verbatim.
pub fn build_item_property(p: &ItemProperty) -> Vec<u8> {
    match p {
        ItemProperty::Ispe {
            image_width,
            image_height,
        } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(&image_width.to_be_bytes());
            body.extend_from_slice(&image_height.to_be_bytes());
            wrap_box(&ISPE, &body)
        }
        ItemProperty::Pixi { bits_per_channel } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.push(bits_per_channel.len() as u8);
            body.extend_from_slice(bits_per_channel);
            wrap_box(&PIXI, &body)
        }
        ItemProperty::Rloc {
            horizontal_offset,
            vertical_offset,
        } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(&horizontal_offset.to_be_bytes());
            body.extend_from_slice(&vertical_offset.to_be_bytes());
            wrap_box(&RLOC, &body)
        }
        ItemProperty::AuxC {
            aux_type,
            aux_subtype,
        } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(aux_type.as_bytes());
            body.push(0); // NULL terminator
            body.extend_from_slice(aux_subtype);
            wrap_box(&AUXC, &body)
        }
        ItemProperty::Irot { angle } => wrap_box(&IROT, &[angle & 0x03]),
        ItemProperty::Imir { axis } => wrap_box(&IMIR, &[axis & 0x01]),
        ItemProperty::Lsel { layer_id } => wrap_box(&LSEL, &layer_id.to_be_bytes()),
        ItemProperty::Pasp(r) => build_pasp_box(r),
        ItemProperty::Clap(r) => build_clap_box(r),
        ItemProperty::Colr(c) => build_colr_box(c),
        ItemProperty::Udes {
            lang,
            name,
            description,
            tags,
        } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            for s in [lang, name, description, tags] {
                body.extend_from_slice(s.as_bytes());
                body.push(0);
            }
            wrap_box(&UDES, &body)
        }
        ItemProperty::Altt { alt_text, alt_lang } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            for s in [alt_text, alt_lang] {
                body.extend_from_slice(s.as_bytes());
                body.push(0);
            }
            wrap_box(&ALTT, &body)
        }
        ItemProperty::Iscl {
            target_width_numerator,
            target_width_denominator,
            target_height_numerator,
            target_height_denominator,
        } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(&target_width_numerator.to_be_bytes());
            body.extend_from_slice(&target_width_denominator.to_be_bytes());
            body.extend_from_slice(&target_height_numerator.to_be_bytes());
            body.extend_from_slice(&target_height_denominator.to_be_bytes());
            wrap_box(&ISCL, &body)
        }
        ItemProperty::Rref { reference_types } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.push(reference_types.len() as u8);
            for rt in reference_types {
                body.extend_from_slice(rt);
            }
            wrap_box(&RREF, &body)
        }
        ItemProperty::Crtt { creation_time } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(&creation_time.to_be_bytes());
            wrap_box(&CRTT, &body)
        }
        ItemProperty::Mdft { modification_time } => {
            let mut body = vec![0u8; 4]; // FullBox(0,0)
            body.extend_from_slice(&modification_time.to_be_bytes());
            wrap_box(&MDFT, &body)
        }
        ItemProperty::Other { box_type, body } => wrap_box(box_type, body),
    }
}

/// Build an `ipco` (ItemPropertyContainerBox, ISO/IEC 23008-12 §9.3.1)
/// from the implicitly-indexed property list — each property box in order.
/// The inverse of [`parse_ipco_box`].
pub fn build_ipco_box(properties: &[ItemProperty]) -> Vec<u8> {
    let mut body = Vec::new();
    for p in properties {
        body.extend_from_slice(&build_item_property(p));
    }
    wrap_box(&IPCO, &body)
}

/// Build a single `ipma` (ItemPropertyAssociation, ISO/IEC 23008-12
/// §9.3.1) box covering all `entries`. The item-ID width and
/// `property_index` width are chosen as the narrowest that hold every
/// value: version 1 (32-bit item IDs) when any item ID exceeds 0xFFFF;
/// `flags & 1` (15-bit index) when any `property_index` exceeds 0x7F. The
/// inverse of [`parse_ipma_box`]. Per §9.3.1 entries must be ordered by
/// increasing `item_ID`; the builder sorts a copy so a caller need not.
/// Returns `None` if any `property_index` exceeds the 15-bit wire ceiling
/// (0x7FFF) — that would not round-trip.
pub fn build_ipma_box(entries: &[ItemPropertyAssociationEntry]) -> Option<Vec<u8>> {
    let mut sorted: Vec<&ItemPropertyAssociationEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.item_id);
    let need_wide_id = sorted.iter().any(|e| e.item_id > 0xFFFF);
    let mut need_wide_index = false;
    for e in &sorted {
        for a in &e.associations {
            if a.property_index > 0x7FFF {
                return None;
            }
            if a.property_index > 0x7F {
                need_wide_index = true;
            }
        }
    }
    let version: u8 = if need_wide_id { 1 } else { 0 };
    let flags: u32 = if need_wide_index { 1 } else { 0 };
    let mut body = Vec::new();
    body.push(version);
    body.extend_from_slice(&flags.to_be_bytes()[1..]); // 24-bit flags
    body.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
    for e in &sorted {
        if version == 0 {
            body.extend_from_slice(&(e.item_id as u16).to_be_bytes());
        } else {
            body.extend_from_slice(&e.item_id.to_be_bytes());
        }
        body.push(e.associations.len() as u8);
        for a in &e.associations {
            if need_wide_index {
                let w = (if a.essential { 0x8000 } else { 0 }) | (a.property_index & 0x7FFF);
                body.extend_from_slice(&w.to_be_bytes());
            } else {
                let b = (if a.essential { 0x80 } else { 0 }) | (a.property_index as u8 & 0x7F);
                body.push(b);
            }
        }
    }
    Some(wrap_box(&IPMA, &body))
}

/// Build a complete `iprp` (ItemPropertiesBox, ISO/IEC 23008-12 §9.3.1)
/// box from typed [`ItemProperties`] — the `ipco` property list followed
/// by one `ipma` association box. The byte-exact inverse of
/// [`parse_iprp_box`] for the single-`ipma` shape this builder emits.
/// Returns `None` when the association table cannot round-trip (a
/// `property_index` past the 15-bit ceiling).
pub fn build_iprp_box(p: &ItemProperties) -> Option<Vec<u8>> {
    let mut body = build_ipco_box(&p.properties);
    if !p.associations.is_empty() {
        body.extend_from_slice(&build_ipma_box(&p.associations)?);
    }
    Some(wrap_box(&IPRP, &body))
}

/// Build a single `EntityToGroupBox` (ISO/IEC 23008-12 §9.4.3.2) from an
/// [`EntityToGroup`]. The box type is the record's `grouping_type`; the
/// body is the 4-byte FullBox preamble (version 0, flags 0) + `group_id`
/// (u32) + `num_entities_in_group` (u32) + the `entity_id` array. The
/// byte-exact inverse of [`parse_entity_to_group`].
pub fn build_entity_to_group_box(g: &EntityToGroup) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // FullBox(0, 0)
    body.extend_from_slice(&g.group_id.to_be_bytes());
    body.extend_from_slice(&(g.entity_ids.len() as u32).to_be_bytes());
    for &e in &g.entity_ids {
        body.extend_from_slice(&e.to_be_bytes());
    }
    wrap_box(&g.grouping_type, &body)
}

/// Build a complete `grpl` (GroupsListBox, ISO/IEC 23008-12 §9.4.2) box
/// from typed [`EntityGroups`] — one `EntityToGroupBox` per group, in
/// slice order. The byte-exact inverse of [`parse_grpl_box`].
pub fn build_grpl_box(g: &EntityGroups) -> Vec<u8> {
    let mut body = Vec::new();
    for grp in &g.groups {
        body.extend_from_slice(&build_entity_to_group_box(grp));
    }
    wrap_box(&GRPL, &body)
}

// ---------------------------------------------------------------------------
// §8.11.7 / §8.11.8 — Additional Metadata Container Box (`meco`) and
// Metabox Relation Box (`mere`). A `meco` (at File / `moov` / `trak`
// level) holds one or more additional `meta` boxes — each with a distinct
// handler type — that complement the primary `meta`, plus zero or more
// `mere` boxes describing how two same-level `meta` boxes relate.
// ---------------------------------------------------------------------------

/// One `mere` MetaboxRelationBox (ISO/IEC 14496-12 §8.11.8). Names two
/// same-level `meta` boxes by their `hdlr` handler types and records the
/// relation between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MereRelation {
    /// The first related `meta` box's handler type (§8.4.3), verbatim.
    pub first_metabox_handler_type: [u8; 4],
    /// The second related `meta` box's handler type, verbatim.
    pub second_metabox_handler_type: [u8; 4],
    /// Relation enum (§8.11.8.3): 1 unknown, 2 unrelated, 3 complementary,
    /// 4 overlapping (neither preferred), 5 second is a subset of the
    /// first (first preferred). Carried verbatim — values outside 1..=5
    /// are preserved rather than rejected so a validator can flag them.
    pub metabox_relation: u8,
}

/// Decoded `meco` AdditionalMetadataContainerBox (ISO/IEC 14496-12
/// §8.11.7): the additional `meta` boxes plus the `mere` relation boxes
/// that the container holds. The additional `meta` boxes are captured as
/// fully-parsed [`MetaItems`] (handler type + any §8.11 item
/// infrastructure they carry); a `meco`-resident `meta` is required to
/// carry a primary item / primary data box (§8.11.7.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MecoBox {
    /// The additional `meta` boxes, in file order. Each must have a
    /// handler type distinct from the primary `meta` and from each other
    /// (§8.11.7.1), but the parser does not enforce that — it captures
    /// whatever is present.
    pub metas: Vec<MetaItems>,
    /// The `mere` relation boxes, in file order.
    pub relations: Vec<MereRelation>,
}

impl MecoBox {
    /// True when the container holds neither an additional `meta` nor a
    /// `mere` — e.g. an empty / malformed `meco`.
    pub fn is_empty(&self) -> bool {
        self.metas.is_empty() && self.relations.is_empty()
    }
}

/// Parse a `mere` body (the bytes after the `[size][mere]` header).
/// `None` on truncation. The §8.11.8.2 body is two 32-bit handler-type
/// codes plus a 1-byte relation, after the FullBox preamble.
pub fn parse_mere_box(body: &[u8]) -> Option<MereRelation> {
    // FullBox preamble (4) + first(4) + second(4) + relation(1) = 13.
    if body.len() < 13 {
        return None;
    }
    let mut first = [0u8; 4];
    let mut second = [0u8; 4];
    first.copy_from_slice(&body[4..8]);
    second.copy_from_slice(&body[8..12]);
    Some(MereRelation {
        first_metabox_handler_type: first,
        second_metabox_handler_type: second,
        metabox_relation: body[12],
    })
}

/// Serialise a `mere` box. The byte-exact inverse of [`parse_mere_box`]
/// (FullBox version 0, flags 0).
pub fn build_mere_box(r: &MereRelation) -> Vec<u8> {
    let mut body = Vec::with_capacity(13);
    body.extend_from_slice(&[0u8; 4]); // version/flags
    body.extend_from_slice(&r.first_metabox_handler_type);
    body.extend_from_slice(&r.second_metabox_handler_type);
    body.push(r.metabox_relation);
    wrap_box(&MERE, &body)
}

/// Parse a `meco` body. Walks its child boxes, decoding each `meta` into
/// a [`MetaItems`] and each `mere` into a [`MereRelation`]; other boxes
/// are ignored. A `meta` child's body opens with the FullBox preamble,
/// matching the top-level `meta` walk.
pub fn parse_meco_box(body: &[u8]) -> MecoBox {
    let mut out = MecoBox::default();
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
        let child = &body[start..start + psz];
        match hdr.fourcc {
            META => out.metas.push(parse_meta_items(child)),
            MERE => {
                if let Some(r) = parse_mere_box(child) {
                    out.relations.push(r);
                }
            }
            _ => {}
        }
    }
    out
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
        amve: None,
        stvi: None,
        btrt: None,
        pasp: None,
        clap: None,
        colr: None,
        hmhd: None,
        gmin: None,
        tcmi: None,
        tmcd: None,
        text_entry: None,
        load_settings: None,
        rtp_hint: None,
        mpeg2ts_hint: None,
        hint_stats: crate::hint::HintStatistics::default(),
        dref: None,
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
            LOAD => {
                // QuickTime Track Load Settings Atom: keep the first
                // parseable instance; a malformed one is dropped
                // (informational) so the trak walk never aborts.
                let sub = read_bytes_vec(&mut cur, psz)?;
                if t.load_settings.is_none() {
                    t.load_settings = parse_load_settings(&sub);
                }
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
            // §9.1.5 — `hinf` Hint Statistics Box (in a hint track's
            // udta). First one wins.
            HINF if t.hint_stats.is_empty() => {
                t.hint_stats = crate::hint::parse_hinf_box(&body[start..start + psz]);
            }
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

/// Serialise a `cprt` (CopyrightBox, ISO/IEC 14496-12 §8.10.2) — the
/// byte-exact inverse of [`parse_cprt`] (FullBox version 0, flags 0).
///
/// Layout: 4-byte FullBox preamble, then the 16-bit packed `language`
/// word (bit 15 pad, three 5-bit ISO 639-2/T characters each `ASCII -
/// 0x60`, §8.10.2.3), then the NUL-terminated UTF-8 `notice` string.
///
/// `language` must be three lower-case ASCII letters (`'a'..='z'`); a
/// byte outside that range cannot be encoded into the 5-bit field and is
/// rejected. A `notice` containing an embedded NUL is rejected (it would
/// truncate on re-parse). The notice is always written as UTF-8 (the
/// parser also accepts a UTF-16BE BOM form, but the canonical write form
/// is UTF-8).
pub fn build_cprt_box(language: &[u8; 3], notice: &str) -> Option<Vec<u8>> {
    let mut packed: u16 = 0;
    for &ch in language {
        if !ch.is_ascii_lowercase() {
            return None;
        }
        packed = (packed << 5) | u16::from(ch - 0x60);
    }
    if notice.as_bytes().contains(&0) {
        return None;
    }
    let mut body = Vec::with_capacity(4 + 2 + notice.len() + 1);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(&packed.to_be_bytes());
    body.extend_from_slice(notice.as_bytes());
    body.push(0); // NUL terminator (§8.10.2.3)
    Some(wrap_box(&crate::boxes::CPRT, &body))
}

/// Serialise a `kind` (KindBox, ISO/IEC 14496-12 §8.10.4) — the byte-exact
/// inverse of [`parse_kind`] (FullBox version 0, flags 0).
///
/// Layout: 4-byte FullBox preamble, then two NUL-terminated UTF-8 C
/// strings — the `schemeURI` then the `value` (§8.10.4.2). The `value`
/// terminator is always written even when empty (`URI\0\0`), which the
/// parser accepts as an explicitly-empty value.
///
/// Rejects an empty `schemeURI` (§8.10.4.3 requires the URI to identify
/// the kind; the parser drops a URI-less entry) or either string
/// containing an embedded NUL (it would truncate on re-parse).
pub fn build_kind_box(scheme_uri: &str, value: &str) -> Option<Vec<u8>> {
    if scheme_uri.is_empty() || scheme_uri.as_bytes().contains(&0) || value.as_bytes().contains(&0)
    {
        return None;
    }
    let mut body = Vec::with_capacity(4 + scheme_uri.len() + value.len() + 2);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(scheme_uri.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    Some(wrap_box(&crate::boxes::KIND, &body))
}

/// Serialise a `tsel` (TrackSelectionBox, ISO/IEC 14496-12 §8.10.3) — the
/// byte-exact inverse of [`parse_tsel`] (FullBox version 0, flags 0).
///
/// Layout: 4-byte FullBox preamble, then the 32-bit signed `switch_group`
/// (§8.10.3.3), then the `attribute_list` as a sequence of 4-byte FourCCs
/// to the end of the box.
pub fn build_tsel_box(switch_group: i32, attribute_list: &[[u8; 4]]) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + attribute_list.len() * 4);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(&switch_group.to_be_bytes());
    for attr in attribute_list {
        body.extend_from_slice(attr);
    }
    wrap_box(&crate::boxes::TSEL, &body)
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

/// Serialise an `stsg` (SubTrackSampleGroupBox, ISO/IEC 14496-12 §8.14.6)
/// — the byte-exact inverse of [`parse_stsg`] (FullBox version 0, flags 0).
///
/// Layout: 4-byte FullBox preamble, 4-byte `grouping_type` FourCC, 2-byte
/// big-endian `item_count` (= `indices.len()`), then `item_count` 4-byte
/// big-endian `group_description_index` values (§8.14.6.2).
///
/// Returns `None` if `indices.len()` exceeds `u16::MAX` (the on-wire
/// `item_count` is 16-bit).
pub fn build_stsg_box(grouping_type: &[u8; 4], indices: &[u32]) -> Option<Vec<u8>> {
    let item_count: u16 = indices.len().try_into().ok()?;
    let mut body = Vec::with_capacity(6 + indices.len() * 4);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(grouping_type);
    body.extend_from_slice(&item_count.to_be_bytes());
    for &idx in indices {
        body.extend_from_slice(&idx.to_be_bytes());
    }
    Some(wrap_box(&STSG, &body))
}

/// Serialise an `stri` (SubTrackInformationBox, ISO/IEC 14496-12 §8.14.4)
/// — the byte-exact inverse of [`parse_stri`] (FullBox version 0, flags 0).
///
/// Layout: 4-byte FullBox preamble, 2-byte signed `switch_group`, 2-byte
/// signed `alternate_group`, 4-byte unsigned `sub_track_ID`, then the
/// `attribute_list` as a sequence of 4-byte FourCCs to the end of the box
/// (§8.14.4.2).
pub fn build_stri_box(
    switch_group: i16,
    alternate_group: i16,
    sub_track_id: u32,
    attribute_list: &[[u8; 4]],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(12 + attribute_list.len() * 4);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(&switch_group.to_be_bytes());
    body.extend_from_slice(&alternate_group.to_be_bytes());
    body.extend_from_slice(&sub_track_id.to_be_bytes());
    for attr in attribute_list {
        body.extend_from_slice(attr);
    }
    wrap_box(&STRI, &body)
}

/// Serialise a complete `strk` (SubTrackBox, ISO/IEC 14496-12 §8.14.3)
/// from its mandatory `stri` selection fields plus zero or more
/// `(grouping_type, group_description_indices)` sample groups.
///
/// The `strk` is a plain container box (§8.14.3.2). It holds the mandatory
/// `stri` (§8.14.4) followed — when `sample_groups` is non-empty — by a
/// single `strd` (SubTrackDefinitionBox, §8.14.5) wrapping one `stsg`
/// (§8.14.6) per `(grouping_type, indices)` pair, in supplied order. When
/// `sample_groups` is empty no `strd` is emitted (the box is optional and
/// would otherwise be a content-free container). The result re-parses
/// through [`parse_strk`] to the same sub-track record.
///
/// Returns `None` if any sample group's index list exceeds the 16-bit
/// `item_count` cap (propagated from [`build_stsg_box`]).
pub fn build_strk_box(
    switch_group: i16,
    alternate_group: i16,
    sub_track_id: u32,
    attribute_list: &[[u8; 4]],
    sample_groups: &[([u8; 4], Vec<u32>)],
) -> Option<Vec<u8>> {
    let mut body = build_stri_box(switch_group, alternate_group, sub_track_id, attribute_list);
    if !sample_groups.is_empty() {
        let mut strd_body = Vec::new();
        for (grouping_type, indices) in sample_groups {
            strd_body.extend_from_slice(&build_stsg_box(grouping_type, indices)?);
        }
        body.extend_from_slice(&wrap_box(&STRD, &strd_body));
    }
    Some(wrap_box(&STRK, &body))
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

/// Serialise a `tref` (TrackReferenceBox, ISO/IEC 14496-12 §8.3.3) from a
/// list of `(reference_type, track_IDs)` pairs — the byte-exact inverse of
/// [`parse_tref`].
///
/// The outer `tref` is a plain container box (no FullBox version/flags).
/// Each pair becomes one `TrackReferenceTypeBox` whose FourCC is the
/// `reference_type` and whose body is the packed big-endian `track_ID`
/// array (§8.3.3.2). The pairs are emitted in the supplied order; a given
/// `reference_type` should appear at most once (the spec permits one
/// `TrackReferenceTypeBox` per type) but this builder does not enforce
/// uniqueness — that is the caller's responsibility.
///
/// Per §8.3.3.3 a `track_ID` is "never zero"; a pair carrying a zero ID is
/// rejected (a zero would be silently dropped by [`parse_tref`], breaking
/// round-trip). An empty `track_IDs` array for a type is permitted (the
/// box is legal but references nothing). Returns `None` if any pair
/// carries a zero `track_ID`.
pub fn build_tref_box(refs: &[([u8; 4], Vec<u32>)]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    for (ref_type, ids) in refs {
        let mut child = Vec::with_capacity(ids.len() * 4);
        for &id in ids {
            if id == 0 {
                return None;
            }
            child.extend_from_slice(&id.to_be_bytes());
        }
        body.extend_from_slice(&wrap_box(ref_type, &child));
    }
    Some(wrap_box(&crate::boxes::TREF, &body))
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

/// Serialise a `trgr` (TrackGroupBox, ISO/IEC 14496-12 §8.3.4) from a list
/// of `(track_group_type, track_group_id)` pairs — the byte-exact inverse
/// of [`parse_trgr`].
///
/// The outer `trgr` is a plain container box (no FullBox preamble). Each
/// pair becomes one `TrackGroupTypeBox` whose FourCC is the
/// `track_group_type` and whose body is the 4-byte FullBox preamble
/// (version 0, flags 0) followed by the 32-bit `track_group_id`
/// (§8.3.4.2). The per-`track_group_type` extension tail is not emitted
/// (this layer does not model it); pairs are written in supplied order.
pub fn build_trgr_box(groups: &[([u8; 4], u32)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (group_type, group_id) in groups {
        let mut child = Vec::with_capacity(8);
        child.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
        child.extend_from_slice(&group_id.to_be_bytes());
        body.extend_from_slice(&wrap_box(group_type, &child));
    }
    wrap_box(&crate::boxes::TRGR, &body)
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
            HMHD => {
                let sub = read_bytes_vec(&mut cur, psz)?;
                // §12.4.2 marks exactly one media header mandatory inside
                // `minf`; keep the first parseable `hmhd` and ignore stray
                // duplicates. A malformed one is dropped (informational —
                // the track still demuxes), so the walk never aborts.
                if t.hmhd.is_none() {
                    if let Ok(h) = parse_hmhd(&sub) {
                        t.hmhd = Some(h);
                    }
                }
            }
            GMHD => {
                // QuickTime Base Media Information Header Atom: a container
                // used in place of a typed media header for base-media
                // tracks (text / timecode / music / generic). Walk it for
                // its `gmin` child (base media info) and, for a timecode
                // track, its `tcmi` child (timecode media info). Keep the
                // first parseable instance of each; a malformed child is
                // dropped (informational — the track still demuxes) so the
                // walk never aborts.
                let sub = read_bytes_vec(&mut cur, psz)?;
                parse_gmhd(&sub, t);
            }
            DINF => {
                // §8.7.1: DataInformationBox is a plain container whose
                // sole child of interest is the `dref` DataReferenceBox.
                // §8.7.1.1 fixes the quantity at exactly one; keep the
                // first parseable instance and ignore stray duplicates. A
                // malformed `dref` is dropped (informational — the file
                // still demuxes against the single-source default), so the
                // dinf walk never aborts the surrounding `minf` parse.
                let sub = read_bytes_vec(&mut cur, psz)?;
                if t.dref.is_none() {
                    if let Some(d) = parse_dinf(&sub) {
                        t.dref = Some(d);
                    }
                }
            }
            _ => skip_cursor_bytes(&mut cur, psz),
        }
    }
    Ok(())
}

/// Walk a `dinf` (DataInformationBox, §8.7.1) body for its `dref`
/// (DataReferenceBox) child and parse it. Returns `None` when the `dinf`
/// carries no parseable `dref`.
fn parse_dinf(body: &[u8]) -> Option<DrefBox> {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = read_box_header(&mut cur).ok()??;
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let sub = read_bytes_vec(&mut cur, psz).ok()?;
        if hdr.fourcc == DREF {
            return parse_dref(&sub).ok();
        }
    }
    None
}

/// Parse a `dref` (DataReferenceBox, ISO/IEC 14496-12 §8.7.2) body.
///
/// Layout after the box header: a 4-byte `FullBox(version=0, flags=0)`
/// preamble, a `unsigned int(32) entry_count`, then `entry_count`
/// `DataEntryBox` children — each a `url ` DataEntryUrlBox or `urn `
/// DataEntryUrnBox FullBox (§8.7.2.2). Each child's own 24-bit
/// `entry_flags` carries the §8.7.2.3 self-contained bit; when it is set
/// the URL form is used with no string body. A `urn ` entry holds a
/// NULL-terminated `name` followed by an optional NULL-terminated
/// `location`; a `url ` entry holds at most a `location`.
///
/// Robustness mirrors the rest of the demuxer: a forged `entry_count`
/// cannot trigger a large up-front allocation (the smallest possible
/// child is its 8-byte box header), a child whose declared size overruns
/// the remaining body ends the walk at what was read, and an unknown
/// FullBox `version` is tolerated (the spec pins it to 0 but the layout
/// is unambiguous).
fn parse_dref(body: &[u8]) -> Result<DrefBox> {
    if body.len() < 8 {
        return Err(Error::invalid("MP4: dref too short"));
    }
    let entry_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let mut cur = std::io::Cursor::new(&body[8..]);
    let avail = (body.len() - 8) as u64;
    // The smallest DataEntryBox is an 8-byte box header (the
    // self-contained `url ` form, 4-byte preamble = the size+type), so
    // the body can hold at most `avail / 8` real entries.
    let max_entries = (avail / 8).max(1);
    let mut entries = Vec::with_capacity(entry_count.min(max_entries as u32) as usize);
    for _ in 0..entry_count {
        if cur.position() >= avail {
            break;
        }
        let hdr = match read_box_header(&mut cur)? {
            Some(h) => h,
            None => break,
        };
        let psz = match hdr.payload_size() {
            Some(p) => p as usize,
            None => break,
        };
        let child = match read_bytes_vec(&mut cur, psz) {
            Ok(c) => c,
            Err(_) => break,
        };
        // The child body is a FullBox: 1 version byte + 3 flag bytes.
        if child.len() < 4 {
            // A malformed child (no FullBox preamble); skip it rather
            // than aborting the whole table — the rest may be valid.
            continue;
        }
        let flags = u32::from_be_bytes([0, child[1], child[2], child[3]]);
        let payload = &child[4..];
        match hdr.fourcc {
            URL_ => {
                // §8.7.2.3: self-contained ⇒ no string present. Otherwise
                // a single NULL-terminated UTF-8 `location`.
                let location = if flags & 1 != 0 {
                    None
                } else {
                    read_c_string(payload)
                };
                entries.push(DataEntry {
                    kind: URL_,
                    flags,
                    name: None,
                    location,
                });
            }
            URN_ => {
                // §8.7.2.2: a NULL-terminated `name` then an optional
                // NULL-terminated `location`. `name` is required for a
                // URN entry; `location` is optional. Self-contained URN
                // entries are not the spec's intent (it directs the URL
                // form for self-contained data), so we still read the
                // strings present.
                let (name, rest) = read_c_string_split(payload);
                let location = read_c_string(rest);
                entries.push(DataEntry {
                    kind: URN_,
                    flags,
                    name,
                    location,
                });
            }
            // §8.7.2.1: each DataEntryBox shall be a `url ` or `urn `.
            // Anything else is non-conforming — record nothing for it so
            // the entry index alignment with `data_reference_index` is
            // preserved by skipping (rather than inserting a bogus entry).
            _ => {}
        }
    }
    Ok(DrefBox { entries })
}

/// Read a single NULL-terminated UTF-8 C string from `buf`. Returns
/// `None` when the string is empty (no bytes, or an immediate NUL) so a
/// "present but empty" entry collapses to "no string". A buffer with no
/// NUL terminator is read to its end (a producer slip on the terminator
/// should not lose the string).
fn read_c_string(buf: &[u8]) -> Option<String> {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if end == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Split `buf` at the first NUL: returns the leading C string (or `None`
/// when empty) and the bytes after the terminator (for reading a second
/// C string). When no NUL is present the whole buffer is the string and
/// the remainder is empty.
fn read_c_string_split(buf: &[u8]) -> (Option<String>, &[u8]) {
    match buf.iter().position(|&b| b == 0) {
        Some(pos) => {
            let s = if pos == 0 {
                None
            } else {
                Some(String::from_utf8_lossy(&buf[..pos]).into_owned())
            };
            (s, &buf[pos + 1..])
        }
        None => {
            let s = if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(buf).into_owned())
            };
            (s, &[])
        }
    }
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

/// Parse `hmhd` (HintMediaHeaderBox, ISO/IEC 14496-12 §12.4.2 /
/// §8.4.5).
///
/// The body is a 4-byte `FullBox(version=0, 0)` preamble followed by
/// `unsigned int(16) maxPDUsize`, `unsigned int(16) avgPDUsize`,
/// `unsigned int(32) maxbitrate`, `unsigned int(32) avgbitrate`, and a
/// reserved `unsigned int(32)` — sixteen payload bytes after the
/// preamble, twenty total. The `version` is not enforced (the spec
/// fixes it at 0, but a non-zero value is tolerated rather than
/// dropping a usable header, matching the `parse_vmhd` posture); the
/// trailing reserved word (required zero by §12.4.2.2) is read past but
/// not surfaced. A body shorter than the full 20 bytes is rejected — a
/// partial header would surface a bitrate that is really truncation
/// noise.
fn parse_hmhd(body: &[u8]) -> Result<HmhdBox> {
    if body.len() < 20 {
        return Err(Error::invalid("MP4: hmhd too short"));
    }
    let max_pdu_size = u16::from_be_bytes([body[4], body[5]]);
    let avg_pdu_size = u16::from_be_bytes([body[6], body[7]]);
    let max_bitrate = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let avg_bitrate = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
    Ok(HmhdBox {
        max_pdu_size,
        avg_pdu_size,
        max_bitrate,
        avg_bitrate,
    })
}

/// Walk a QuickTime `gmhd` (Base Media Information Header Atom) body,
/// populating `t.gmin` from its `gmin` (Base Media Info Atom) child and,
/// for a timecode track, `t.tcmi` from its `tcmi` (Timecode Media Info
/// Atom) child. The first parseable instance of each wins. A `gmhd` may
/// also carry other media-specific children (e.g. a `text` atom for text
/// media); those are skipped. A malformed child ends the walk at what was
/// read without disturbing the surrounding parse.
fn parse_gmhd(body: &[u8], t: &mut Track) {
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur) {
            Ok(Some(h)) => h,
            _ => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let sub = match read_bytes_vec(&mut cur, psz) {
            Ok(s) => s,
            Err(_) => break,
        };
        match hdr.fourcc {
            GMIN if t.gmin.is_none() => {
                if let Some(g) = parse_gmin(&sub) {
                    t.gmin = Some(g);
                }
            }
            TCMI if t.tcmi.is_none() => {
                if let Some(tc) = parse_tcmi(&sub) {
                    t.tcmi = Some(tc);
                }
            }
            _ => {}
        }
    }
}

/// Parse a QuickTime `gmin` (Base Media Info Atom) body.
///
/// The body is a 4-byte `FullBox(version, flags)` preamble followed by
/// `unsigned int(16) graphicsmode`, `unsigned int(16)[3] opcolor`,
/// `int(16) balance`, and a reserved `unsigned int(16)` — twelve payload
/// bytes after the preamble, sixteen total. The `version` / `flags`
/// (specified as 0) are read past but not enforced (a non-zero value is
/// tolerated rather than dropping a usable atom, matching the `parse_vmhd`
/// posture); the trailing reserved word is read past but not surfaced.
/// A body shorter than the full 16 bytes is rejected — a partial atom
/// would surface a `balance` that is really truncation noise.
fn parse_gmin(body: &[u8]) -> Option<GminBox> {
    if body.len() < 16 {
        return None;
    }
    let graphicsmode = u16::from_be_bytes([body[4], body[5]]);
    let opcolor = [
        u16::from_be_bytes([body[6], body[7]]),
        u16::from_be_bytes([body[8], body[9]]),
        u16::from_be_bytes([body[10], body[11]]),
    ];
    let balance = i16::from_be_bytes([body[12], body[13]]);
    Some(GminBox {
        graphicsmode,
        opcolor,
        balance,
    })
}

/// Parse a standalone QuickTime `gmin` (Base Media Info Atom) body from
/// `body` (the 16 bytes after the plain 8-byte box header). Exposed so
/// tooling holding the atom's payload can recover the
/// `(graphicsmode, opcolor, balance)` triple without re-running `open()`.
/// `Err` for a body shorter than the fixed 16-byte layout; trailing bytes
/// are ignored.
pub fn parse_gmin_box(body: &[u8]) -> Result<GminBox> {
    parse_gmin(body).ok_or_else(|| Error::invalid("MP4: gmin too short"))
}

/// Build a complete QuickTime `gmin` (Base Media Info Atom) box from a
/// [`GminBox`]. The byte-exact inverse of [`parse_gmin_box`]: a 4-byte
/// `FullBox(version=0, flags=0)` preamble, the `graphicsmode`, the three
/// `opcolor` components, the `balance`, and the reserved zero word.
pub fn build_gmin_box(r: &GminBox) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&r.graphicsmode.to_be_bytes());
    for c in r.opcolor {
        body.extend_from_slice(&c.to_be_bytes());
    }
    body.extend_from_slice(&r.balance.to_be_bytes());
    body.extend_from_slice(&[0, 0]); // reserved
    wrap_box(&GMIN, &body)
}

/// Build a complete QuickTime `gmhd` (Base Media Information Header Atom)
/// box wrapping a single `gmin` (Base Media Info Atom) built from `r`.
/// This is the minimal `gmhd` — the `gmin` child alone — sufficient for
/// a base-media (text / timecode / music / generic) track's `minf`.
pub fn build_gmhd_box(r: &GminBox) -> Vec<u8> {
    wrap_box(&GMHD, &build_gmin_box(r))
}

/// Parse a QuickTime `tcmi` (Timecode Media Info Atom) body.
///
/// The body is a 4-byte `FullBox(version, flags)` preamble followed by
/// `unsigned int(16) text_font`, `unsigned int(16) text_face`,
/// `unsigned int(16) text_size`, a 48-bit (`[u16; 3]`) `text_color`, a
/// 48-bit `background_color`, and a Pascal-string `font_name` (a length
/// byte then that many characters) — a fixed 22-byte prefix after the
/// preamble followed by the variable-length name. The `version` / `flags`
/// (specified as 0) are read past but not enforced. A body shorter than
/// the 22-byte fixed prefix is rejected; a `font_name` whose declared
/// length overruns the remaining bytes is clamped to what is present
/// (rather than dropping the whole atom). Non-UTF-8 name bytes are
/// replaced (lossy) so the record always yields a `String`.
fn parse_tcmi(body: &[u8]) -> Option<TcmiBox> {
    // 4 (preamble) + 2 (font) + 2 (face) + 2 (size) + 6 (text_color)
    // + 6 (background_color) = 22 fixed bytes before the Pascal string.
    if body.len() < 22 {
        return None;
    }
    let text_font = u16::from_be_bytes([body[4], body[5]]);
    let text_face = u16::from_be_bytes([body[6], body[7]]);
    let text_size = u16::from_be_bytes([body[8], body[9]]);
    let text_color = [
        u16::from_be_bytes([body[10], body[11]]),
        u16::from_be_bytes([body[12], body[13]]),
        u16::from_be_bytes([body[14], body[15]]),
    ];
    let background_color = [
        u16::from_be_bytes([body[16], body[17]]),
        u16::from_be_bytes([body[18], body[19]]),
        u16::from_be_bytes([body[20], body[21]]),
    ];
    // Pascal string: a leading length byte, then that many characters.
    // A missing length byte (exactly 22 bytes) means an empty name.
    let font_name = if body.len() > 22 {
        let n = body[22] as usize;
        let avail = body.len().saturating_sub(23);
        let take = n.min(avail);
        String::from_utf8_lossy(&body[23..23 + take]).into_owned()
    } else {
        String::new()
    };
    Some(TcmiBox {
        text_font,
        text_face,
        text_size,
        text_color,
        background_color,
        font_name,
    })
}

/// Parse a standalone QuickTime `tcmi` (Timecode Media Info Atom) body
/// from `body` (the bytes after the plain 8-byte box header). Exposed so
/// tooling holding the atom's payload can recover its rendering
/// parameters without re-running `open()`. `Err` for a body shorter than
/// the fixed 22-byte prefix; a `font_name` length that overruns is
/// clamped to the bytes present.
pub fn parse_tcmi_box(body: &[u8]) -> Result<TcmiBox> {
    parse_tcmi(body).ok_or_else(|| Error::invalid("MP4: tcmi too short"))
}

/// Build a complete QuickTime `tcmi` (Timecode Media Info Atom) box from a
/// [`TcmiBox`]. The byte-exact inverse of [`parse_tcmi_box`]: a 4-byte
/// `FullBox(version=0, flags=0)` preamble, the three 16-bit
/// font/face/size fields, the two 48-bit colours, and the Pascal-string
/// `font_name` (a length byte capped at 255 followed by that many
/// characters).
pub fn build_tcmi_box(r: &TcmiBox) -> Vec<u8> {
    let mut body = Vec::with_capacity(23 + r.font_name.len());
    body.extend_from_slice(&[0, 0, 0, 0]); // version + flags
    body.extend_from_slice(&r.text_font.to_be_bytes());
    body.extend_from_slice(&r.text_face.to_be_bytes());
    body.extend_from_slice(&r.text_size.to_be_bytes());
    for c in r.text_color {
        body.extend_from_slice(&c.to_be_bytes());
    }
    for c in r.background_color {
        body.extend_from_slice(&c.to_be_bytes());
    }
    let name = r.font_name.as_bytes();
    let n = name.len().min(255);
    body.push(n as u8);
    body.extend_from_slice(&name[..n]);
    wrap_box(&TCMI, &body)
}

/// Parse a QuickTime `tmcd` (Timecode) sample-description entry from
/// `entry` (the full sample-entry bytes, including the shared 8-byte
/// `reserved[6]` + `data_reference_index` preamble). After the preamble:
/// `unsigned int(32) reserved`, `unsigned int(32) flags`,
/// `unsigned int(32) timescale`, `unsigned int(32) frame_duration`,
/// `unsigned int(8) number_of_frames` — 21 payload bytes after the
/// preamble (29 total up to and including `number_of_frames`). Returns
/// `None` for an entry too short to hold the fixed numeric fields; any
/// trailing reserved / source-reference `udta` bytes are ignored.
fn parse_tmcd_sample_entry(entry: &[u8]) -> Option<TmcdSampleEntry> {
    // 8 (preamble) + 4 (reserved) + 4 (flags) + 4 (timescale)
    // + 4 (frame_duration) + 1 (number_of_frames) = 25 bytes minimum.
    if entry.len() < 25 {
        return None;
    }
    let flags = u32::from_be_bytes([entry[12], entry[13], entry[14], entry[15]]);
    let timescale = u32::from_be_bytes([entry[16], entry[17], entry[18], entry[19]]);
    let frame_duration = u32::from_be_bytes([entry[20], entry[21], entry[22], entry[23]]);
    let number_of_frames = entry[24];
    Some(TmcdSampleEntry {
        flags,
        timescale,
        frame_duration,
        number_of_frames,
    })
}

/// Parse a standalone QuickTime `tmcd` (Timecode) sample-description entry
/// body from `entry` (the full sample-entry bytes including the shared
/// 8-byte preamble). Exposed so tooling holding the entry's bytes can
/// recover the `(flags, timescale, frame_duration, number_of_frames)`
/// fields without re-running `open()`. `Err` for an entry too short to
/// hold the fixed numeric fields.
pub fn parse_tmcd_sample_entry_box(entry: &[u8]) -> Result<TmcdSampleEntry> {
    parse_tmcd_sample_entry(entry).ok_or_else(|| Error::invalid("MP4: tmcd sample entry too short"))
}

/// Build a complete QuickTime `tmcd` (Timecode) sample-description entry
/// box from a [`TmcdSampleEntry`], with `data_reference_index`. The
/// byte-exact inverse of [`parse_tmcd_sample_entry_box`]: the shared
/// 8-byte preamble (`reserved[6]` + `data_reference_index`), a reserved
/// u32, the `flags`, `timescale`, `frame_duration`, `number_of_frames`,
/// and a reserved u8. The optional source-reference `udta` is not emitted.
pub fn build_tmcd_sample_entry(r: &TmcdSampleEntry, data_reference_index: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(26);
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&data_reference_index.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]); // reserved
    body.extend_from_slice(&r.flags.to_be_bytes());
    body.extend_from_slice(&r.timescale.to_be_bytes());
    body.extend_from_slice(&r.frame_duration.to_be_bytes());
    body.push(r.number_of_frames);
    body.push(0); // reserved
    wrap_box(&TMCD, &body)
}

/// Parse a QuickTime `text` (Text) sample-description entry from `entry`
/// (the full sample-entry bytes including the shared 8-byte preamble).
/// Fixed field offsets (from `entry` start): `display_flags` [8..12],
/// `text_justification` [12..16], `background_color` [16..22],
/// `default_text_box` [22..30], reserved [30..38], `font_number`
/// [38..40], `font_face` [40..42], reserved [42..43], reserved [43..45],
/// `foreground_color` [45..51], then a Pascal-string `text_name` at
/// [51..]. Returns `None` for an entry shorter than the 51-byte fixed
/// prefix; a `text_name` length that overruns the body is clamped to the
/// bytes present. Non-UTF-8 name bytes are replaced (lossy).
fn parse_text_sample_entry(entry: &[u8]) -> Option<TextSampleEntry> {
    if entry.len() < 51 {
        return None;
    }
    let display_flags = u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]]);
    let text_justification = i32::from_be_bytes([entry[12], entry[13], entry[14], entry[15]]);
    let background_color = [
        u16::from_be_bytes([entry[16], entry[17]]),
        u16::from_be_bytes([entry[18], entry[19]]),
        u16::from_be_bytes([entry[20], entry[21]]),
    ];
    let default_text_box = [
        i16::from_be_bytes([entry[22], entry[23]]),
        i16::from_be_bytes([entry[24], entry[25]]),
        i16::from_be_bytes([entry[26], entry[27]]),
        i16::from_be_bytes([entry[28], entry[29]]),
    ];
    // reserved int(64) at [30..38]
    let font_number = u16::from_be_bytes([entry[38], entry[39]]);
    let font_face = u16::from_be_bytes([entry[40], entry[41]]);
    // reserved int(8) at [42], reserved int(16) at [43..45]
    let foreground_color = [
        u16::from_be_bytes([entry[45], entry[46]]),
        u16::from_be_bytes([entry[47], entry[48]]),
        u16::from_be_bytes([entry[49], entry[50]]),
    ];
    // Pascal string text_name: a leading length byte then that many
    // characters. Absent (exactly 51 bytes) → empty name.
    let text_name = if entry.len() > 51 {
        let n = entry[51] as usize;
        let avail = entry.len().saturating_sub(52);
        let take = n.min(avail);
        String::from_utf8_lossy(&entry[52..52 + take]).into_owned()
    } else {
        String::new()
    };
    Some(TextSampleEntry {
        display_flags,
        text_justification,
        background_color,
        default_text_box,
        font_number,
        font_face,
        foreground_color,
        text_name,
    })
}

/// Parse a standalone QuickTime `text` (Text) sample-description entry
/// body from `entry` (the full sample-entry bytes including the shared
/// 8-byte preamble). Exposed so tooling holding the entry's bytes can
/// recover the display / colour / font fields without re-running
/// `open()`. `Err` for an entry shorter than the 51-byte fixed prefix.
pub fn parse_text_sample_entry_box(entry: &[u8]) -> Result<TextSampleEntry> {
    parse_text_sample_entry(entry).ok_or_else(|| Error::invalid("MP4: text sample entry too short"))
}

/// Build a complete QuickTime `text` (Text) sample-description entry box
/// from a [`TextSampleEntry`], with `data_reference_index`. The byte-exact
/// inverse of [`parse_text_sample_entry_box`]: the shared 8-byte preamble,
/// the fixed numeric fields (with the spec's reserved zeros), and the
/// Pascal-string `text_name` (a length byte capped at 255 followed by
/// that many characters).
pub fn build_text_sample_entry(r: &TextSampleEntry, data_reference_index: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(52 + r.text_name.len());
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&data_reference_index.to_be_bytes());
    body.extend_from_slice(&r.display_flags.to_be_bytes());
    body.extend_from_slice(&r.text_justification.to_be_bytes());
    for c in r.background_color {
        body.extend_from_slice(&c.to_be_bytes());
    }
    for v in r.default_text_box {
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&[0u8; 8]); // reserved int(64)
    body.extend_from_slice(&r.font_number.to_be_bytes());
    body.extend_from_slice(&r.font_face.to_be_bytes());
    body.push(0); // reserved int(8)
    body.extend_from_slice(&[0u8; 2]); // reserved int(16)
    for c in r.foreground_color {
        body.extend_from_slice(&c.to_be_bytes());
    }
    let name = r.text_name.as_bytes();
    let n = name.len().min(255);
    body.push(n as u8);
    body.extend_from_slice(&name[..n]);
    wrap_box(b"text", &body)
}

/// Parse a QuickTime `load` (Track Load Settings Atom) body from `body`
/// (the 16 bytes after the plain 8-byte box header — `load` is a plain
/// `Box`, no FullBox preamble): `int(32) preload_start_time`, `int(32)
/// preload_duration`, `int(32) preload_flags`, `int(32) default_hints`.
/// Returns `None` for a body shorter than the fixed 16 bytes; trailing
/// bytes are ignored.
fn parse_load_settings(body: &[u8]) -> Option<LoadSettingsBox> {
    if body.len() < 16 {
        return None;
    }
    let preload_start_time = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let preload_duration = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let preload_flags = i32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let default_hints = i32::from_be_bytes([body[12], body[13], body[14], body[15]]);
    Some(LoadSettingsBox {
        preload_start_time,
        preload_duration,
        preload_flags,
        default_hints,
    })
}

/// Parse a standalone QuickTime `load` (Track Load Settings Atom) body
/// from `body` (the 16 bytes after the plain 8-byte box header). Exposed
/// so tooling holding the atom's payload can recover the preload / hint
/// fields without re-running `open()`. `Err` for a body shorter than the
/// fixed 16-byte layout; trailing bytes are ignored.
pub fn parse_load_settings_box(body: &[u8]) -> Result<LoadSettingsBox> {
    parse_load_settings(body).ok_or_else(|| Error::invalid("MP4: load too short"))
}

/// Build a complete QuickTime `load` (Track Load Settings Atom) box from a
/// [`LoadSettingsBox`]. The byte-exact inverse of
/// [`parse_load_settings_box`]: the four 32-bit fields with no FullBox
/// preamble.
pub fn build_load_settings_box(r: &LoadSettingsBox) -> Vec<u8> {
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&r.preload_start_time.to_be_bytes());
    body.extend_from_slice(&r.preload_duration.to_be_bytes());
    body.extend_from_slice(&r.preload_flags.to_be_bytes());
    body.extend_from_slice(&r.default_hints.to_be_bytes());
    wrap_box(&LOAD, &body)
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
        if t.stvi.is_none() {
            t.stvi = unwrap.stvi;
        }
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
    /// `stvi` (StereoVideoBox, §8.15.4.2) recovered from `sinf/schi`
    /// when the SchemeType is `stvi` (stereoscopic video, §8.15.4.1).
    /// `None` for non-stereo schemes (the common case) or a malformed
    /// box.
    stvi: Option<StviRecord>,
}

impl SinfUnwrap {
    fn empty() -> Self {
        SinfUnwrap {
            original_format: None,
            scheme_type: None,
            tenc: None,
            stvi: None,
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
                // `tenc` (CENC), and ISO/IEC 14496-12 §8.15.4.2 — the
                // `stvi` (StereoVideoBox) of a `stvi`-scheme restricted
                // entry. A malformed child inside an otherwise valid
                // schi should not abort sinf parsing (it still has
                // structural meaning via frma + schm), so each is
                // swallowed independently and left `None` on error.
                let children = walk_schi(&inner);
                out.tenc = children.tenc;
                out.stvi = children.stvi;
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Scheme-specific children recovered from a `schi`
/// (SchemeInformationBox, ISO/IEC 14496-12 §8.12.6). Each field is
/// independently `None` when its box was absent or malformed — a `schi`
/// carries whichever children the active SchemeType defines.
#[derive(Default)]
struct SchiChildren {
    /// `tenc` (TrackEncryptionBox, ISO/IEC 23001-7 §8.2) — CENC scheme.
    tenc: Option<TencBox>,
    /// `stvi` (StereoVideoBox, §8.15.4.2) — `stvi` stereoscopic scheme.
    stvi: Option<StviRecord>,
}

/// Walk a `schi` (SchemeInformationBox, ISO/IEC 14496-12 §8.12.6)
/// body collecting the scheme-specific children this crate understands:
/// the CENC `tenc` (TrackEncryptionBox, ISO/IEC 23001-7 §8.2) and the
/// stereoscopic-video `stvi` (StereoVideoBox, §8.15.4.2). A `schi` is
/// scheme-specific so it may legitimately carry neither (or other
/// children we don't model); a malformed child is dropped without
/// aborting the walk. The walk ends cleanly on a truncated header.
fn walk_schi(body: &[u8]) -> SchiChildren {
    let mut out = SchiChildren::default();
    let mut cur = std::io::Cursor::new(body);
    let end = body.len() as u64;
    while cur.position() < end {
        let hdr = match read_box_header(&mut cur) {
            Ok(Some(h)) => h,
            _ => break,
        };
        let psz = hdr.payload_size().unwrap_or(0) as usize;
        let inner = match read_bytes_vec(&mut cur, psz) {
            Ok(b) => b,
            Err(_) => break,
        };
        match hdr.fourcc {
            TENC if out.tenc.is_none() => out.tenc = parse_tenc(&inner).ok(),
            STVI if out.stvi.is_none() => out.stvi = parse_stvi(&inner),
            _ => {}
        }
    }
    out
}

fn parse_sample_entry(entry: &[u8], t: &mut Track) -> Result<()> {
    if entry.len() < 8 {
        return Ok(());
    }
    // §9.1.2 / §9.4.1.2 / §9.4.2.3 — RTP / SRTP / RTP-reception / RTCP-
    // reception hint sample entries. A hint track's handler maps to
    // MediaType::Data, so dispatch on the active stsd FourCC before the
    // media-type switch. A malformed entry leaves `rtp_hint` None rather
    // than aborting the open. `rtcp` shares the `rtp ` body (§9.4.2.3).
    if matches!(
        t.codec_id_fourcc,
        RTP_HINT | SRTP_HINT | RRTP_HINT | RTCP_HINT
    ) {
        t.rtp_hint = crate::hint::parse_rtp_hint_sample_entry(t.codec_id_fourcc, entry);
        return Ok(());
    }
    // §9.3.3.2 — MPEG-2 TS server (`sm2t`) / reception (`rm2t`) hint
    // sample entries.
    if matches!(t.codec_id_fourcc, SM2T | RM2T) {
        t.mpeg2ts_hint = crate::hint::parse_mpeg2ts_hint_sample_entry(t.codec_id_fourcc, entry);
        return Ok(());
    }
    // QuickTime timecode media (`tmcd`): its handler maps to
    // MediaType::Data, so dispatch on the FourCC before the media-type
    // switch. A malformed entry leaves `tmcd` None rather than aborting.
    if t.codec_id_fourcc == TMCD {
        t.tmcd = parse_tmcd_sample_entry(entry);
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
        b"text" => {
            // QuickTime plain-text sample description: a fixed header
            // (display flags + justification + colours + default text box
            // + font) followed by a Pascal-string font name. Parse the
            // fixed numeric fields into a structured record; keep the raw
            // post-preamble bytes as extradata too (a renderer may want
            // the trailing font name / any nonstandard tail verbatim).
            t.text_entry = parse_text_sample_entry(entry);
            t.extradata = entry[8..].to_vec();
        }
        b"tx3g" | b"c608" | b"c708" => {
            // `tx3g` carries an 18-byte fixed header (display flags +
            // colours + default text box + default style record) plus
            // optional `ftab` font table. Treat the entire post-
            // preamble payload as extradata so a downstream renderer
            // can pick the colour / style defaults out of it.
            //
            // For `c608` / `c708` no useful per-track header is defined;
            // the post-preamble bytes are still preserved as extradata
            // for any nonstandard carriage.
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
            // ALAC magic cookie (`alac` config child inside an `alac`
            // entry): a FullBox — 4 bytes version/flags then the cookie.
            // Surface the cookie bytes (mirroring the dfLa convention);
            // the muxer's `alac_entry` re-wraps them byte-exact.
            b"alac" if body.len() > 4 => {
                t.extradata = body[4..].to_vec();
            }
            // AC-3 specific config (`dac3`, ETSI TS 102 366 Annex F.4)
            // and E-AC-3 specific config (`dec3`, Annex G.4). Keep the
            // raw box payload as extradata so downstream decoders that
            // care about `fscod`/`bsid`/`acmod`/`lfeon`/etc. can parse
            // it themselves. For decoders that don't need it the bytes
            // are harmless extra context.
            b"dac3" | b"dec3" => t.extradata = body,
            // BitRateBox (ISO/IEC 14496-12 §8.5.2) — optional, at the end
            // of any sample entry including audio. First wins; short body
            // dropped (informational).
            b"btrt" if t.btrt.is_none() => {
                if let Some(rec) = parse_btrt(&body) {
                    t.btrt = Some(rec);
                }
            }
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
            // H.263 decoder configuration (`d263`, the 3GPP MP4 packaging's
            // config child inside an `s263` entry). Kept verbatim as opaque
            // extradata — the muxer's `h263_entry` re-emits it byte-exact.
            b"d263" => t.extradata = body,
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
            // AmbientViewingEnvironmentBox (ISO/IEC 14496-12 post-2015) —
            // the file-format carriage of the ambient_viewing_environment
            // SEI / ISO/IEC 23091-3 parameters. A plain Box (no FullBox
            // version/flags), fixed 8-byte body. First one on the track
            // wins; a malformed (short) body is dropped (informational —
            // it never aborts the open).
            b"amve" if t.amve.is_none() => {
                if let Some(rec) = parse_amve(&body) {
                    t.amve = Some(rec);
                }
            }
            // BitRateBox (ISO/IEC 14496-12 §8.5.2) — an optional plain
            // Box (no FullBox version/flags) at the end of the sample
            // entry carrying `bufferSizeDB` / `maxBitrate` / `avgBitrate`.
            // First one on the active entry wins; a short body is dropped
            // (informational — it never aborts the open).
            b"btrt" if t.btrt.is_none() => {
                if let Some(rec) = parse_btrt(&body) {
                    t.btrt = Some(rec);
                }
            }
            // PixelAspectRatioBox (§12.1.4) — a plain Box (no FullBox
            // preamble), 8-byte body (hSpacing / vSpacing). First one on
            // the active entry wins; a short body is dropped.
            b"pasp" if t.pasp.is_none() => {
                if let Some(rec) = parse_pasp(&body) {
                    t.pasp = Some(rec);
                }
            }
            // CleanApertureBox (§12.1.4) — a plain Box, 32-byte body of
            // four (N, D) fractions. First one wins; short body dropped.
            b"clap" if t.clap.is_none() => {
                if let Some(rec) = parse_clap(&body) {
                    t.clap = Some(rec);
                }
            }
            // ColourInformationBox (§12.1.5) — a plain Box; the spec
            // allows several (most-accurate first), the first wins here.
            b"colr" if t.colr.is_none() => {
                if let Some(rec) = parse_colr(&body) {
                    t.colr = Some(rec);
                }
            }
            // Restricted-scheme (§8.15) video — a non-encrypted
            // `VisualSampleEntry` (e.g. a `resv` whose original FourCC is
            // already preserved here, or any entry signalling a
            // restricted scheme) carries its `sinf` directly as a child
            // box rather than wrapped by an `enc*` outer FourCC. Descend
            // it to recover an `stvi` (StereoVideoBox, §8.15.4.2) from
            // the `schi` when the SchemeType is `stvi`. The `enc*` path
            // (handled before `parse_sample_entry`) already populated
            // `t.stvi` for protected entries — first one wins.
            b"sinf" if t.stvi.is_none() => {
                if let Ok(unwrap) = parse_sinf_body(&body) {
                    t.stvi = unwrap.stvi;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Parse an `amve` (AmbientViewingEnvironmentBox) body — the 8 bytes
/// after the plain 8-byte box header (`amve` is a `Box`, not a
/// `FullBox`, so there is no version/flags preamble).
///
/// Returns `None` for a body shorter than the fixed 8-byte layout
/// (`ambient_illuminance` u32 + `ambient_light_x` u16 +
/// `ambient_light_y` u16). Trailing bytes beyond byte 8 — reserved for a
/// future edition — are ignored so an extended box still parses.
fn parse_amve(body: &[u8]) -> Option<AmveRecord> {
    if body.len() < 8 {
        return None;
    }
    let ambient_illuminance = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let ambient_light_x = u16::from_be_bytes([body[4], body[5]]);
    let ambient_light_y = u16::from_be_bytes([body[6], body[7]]);
    Some(AmveRecord {
        ambient_illuminance,
        ambient_light_x,
        ambient_light_y,
    })
}

/// Parse a `btrt` (BitRateBox, ISO/IEC 14496-12 §8.5.2) body — the 12
/// bytes after the plain 8-byte box header (`btrt` is a `Box`, not a
/// `FullBox`, so there is no version/flags preamble).
///
/// The body is exactly three big-endian `u32`s: `bufferSizeDB`,
/// `maxBitrate`, `avgBitrate`. Returns `None` for a body shorter than
/// 12 bytes (a truncated box). Trailing bytes beyond byte 12 — none are
/// defined by §8.5.2, but a future edition could append fields — are
/// ignored so an extended box still parses, matching the `parse_amve`
/// posture.
fn parse_btrt(body: &[u8]) -> Option<BtrtRecord> {
    if body.len() < 12 {
        return None;
    }
    let buffer_size_db = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let max_bitrate = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let avg_bitrate = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    Some(BtrtRecord {
        buffer_size_db,
        max_bitrate,
        avg_bitrate,
    })
}

/// Parse a `pasp` (PixelAspectRatioBox, ISO/IEC 14496-12 §12.1.4) body —
/// the 8 bytes after the plain box header (`pasp` is a `Box`, not a
/// `FullBox`). `None` for a body shorter than 8 bytes.
pub fn parse_pasp(body: &[u8]) -> Option<PaspRecord> {
    if body.len() < 8 {
        return None;
    }
    Some(PaspRecord {
        h_spacing: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        v_spacing: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
    })
}

/// Parse a `clap` (CleanApertureBox, ISO/IEC 14496-12 §12.1.4) body —
/// the 32 bytes (eight u32s) after the plain box header. `None` for a
/// body shorter than 32 bytes.
pub fn parse_clap(body: &[u8]) -> Option<ClapRecord> {
    if body.len() < 32 {
        return None;
    }
    let u = |i: usize| u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
    Some(ClapRecord {
        width_n: u(0),
        width_d: u(4),
        height_n: u(8),
        height_d: u(12),
        horiz_off_n: u(16),
        horiz_off_d: u(20),
        vert_off_n: u(24),
        vert_off_d: u(28),
    })
}

/// Parse a `colr` (ColourInformationBox, ISO/IEC 14496-12 §12.1.5) body
/// — the bytes after the plain box header (`colr` is a `Box`). The first
/// four bytes are the `colour_type`; the remaining payload depends on
/// it (§12.1.5.2). `None` for a body shorter than the 4-byte type, or a
/// truncated `nclx` payload.
pub fn parse_colr(body: &[u8]) -> Option<ColrRecord> {
    if body.len() < 4 {
        return None;
    }
    let colour_type = [body[0], body[1], body[2], body[3]];
    let rest = &body[4..];
    match &colour_type {
        b"nclx" => {
            // colour_primaries(16) + transfer(16) + matrix(16) +
            // full_range_flag(1) + reserved(7).
            if rest.len() < 7 {
                return None;
            }
            Some(ColrRecord::Nclx {
                colour_primaries: u16::from_be_bytes([rest[0], rest[1]]),
                transfer_characteristics: u16::from_be_bytes([rest[2], rest[3]]),
                matrix_coefficients: u16::from_be_bytes([rest[4], rest[5]]),
                full_range: rest[6] & 0x80 != 0,
            })
        }
        b"rICC" => Some(ColrRecord::RestrictedIcc(rest.to_vec())),
        b"prof" => Some(ColrRecord::UnrestrictedIcc(rest.to_vec())),
        _ => Some(ColrRecord::Other {
            colour_type,
            data: rest.to_vec(),
        }),
    }
}

/// Parse an `stvi` (StereoVideoBox, ISO/IEC 14496-12 §8.15.4.2) body —
/// the bytes after the 4-byte FullBox version/flags preamble.
///
/// Layout (§8.15.4.2.2):
///
/// ```text
/// template int(30) reserved = 0;
/// int(2)  single_view_allowed;
/// int(32) stereo_scheme;
/// int(32) length;
/// int(8)[length] stereo_indication_type;
/// Box[] any_box;  // optional, ignored at this layer
/// ```
///
/// The first 32-bit word packs the 30-bit reserved field (high bits)
/// and the 2-bit `single_view_allowed` (low bits). The reserved bits
/// are masked off so a producer slip on them does not corrupt the
/// surfaced value. `length` is validated against the bytes actually
/// present; a `length` that overruns the body is rejected (`None`)
/// rather than reading past the end. Trailing `any_box` bytes after the
/// `stereo_indication_type` array are ignored. Returns `None` for a
/// body too short to hold the fixed 12-byte (preamble-relative) header.
fn parse_stvi(body: &[u8]) -> Option<StviRecord> {
    // 4-byte FullBox preamble + 4 (single_view_allowed word) + 4
    // (stereo_scheme) + 4 (length) = 16 bytes minimum.
    if body.len() < 16 {
        return None;
    }
    // FullBox preamble: version (1) + flags (3). §8.15.4.2.2 pins
    // version 0; we tolerate a non-zero version (the layout is
    // unambiguous), matching the `parse_pdin` / `parse_padb` posture.
    let svw = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    // Low 2 bits are `single_view_allowed`; the upper 30 are reserved.
    let single_view_allowed = (svw & 0x3) as u8;
    let stereo_scheme = u32::from_be_bytes([body[8], body[9], body[10], body[11]]);
    let length = u32::from_be_bytes([body[12], body[13], body[14], body[15]]) as usize;
    let sit_start: usize = 16;
    // `length` must not run past the bytes present (a truncated /
    // forged length would otherwise read trailing `any_box` content or
    // off the end).
    let sit_end = sit_start.checked_add(length)?;
    if sit_end > body.len() {
        return None;
    }
    let stereo_indication_type = body[sit_start..sit_end].to_vec();
    Some(StviRecord {
        single_view_allowed,
        stereo_scheme,
        stereo_indication_type,
    })
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
pub fn parse_csgp(body: &[u8]) -> Result<CsgpBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: csgp too short"));
    }
    // FullBox: version(8) then 24-bit flags carrying the size codes.
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let index_size_code = (flags & 0x3) as u8;
    let count_size_code = ((flags >> 2) & 0x3) as u8;
    let pattern_size_code = ((flags >> 4) & 0x3) as u8;
    // §8.9.5 constraint: `pattern_size_code` and `count_size_code` must
    // agree on whether the 4-bit width (code 0) is used — a 4-bit/non-4-bit
    // mix is an invalid file. Reject it rather than mis-pack the interleaved
    // (pattern_length, sample_count) pairs at disagreeing widths.
    if (pattern_size_code == 0) != (count_size_code == 0) {
        return Err(Error::invalid(
            "MP4: csgp pattern_size_code/count_size_code disagree on 4-bit width",
        ));
    }
    let gtpp = (flags >> 6) & 0x1 == 1;
    // Flag layout bit 7 (§8.9.5): when set, the MSB of each index is a
    // fragment-local-vs-global source selector (only legal in a `traf`).
    let index_msb_indicates_fragment_local_description = (flags >> 7) & 0x1 == 1;
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
        index_msb_indicates_fragment_local_description,
        index_field_bits: index_w as u8,
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

/// Parse a standalone `subs` (SubSampleInformationBox, ISO/IEC 14496-12
/// §8.7.7) body (the bytes after the box header) into a [`SubsBox`].
///
/// Exposed as a public entry point so a codec-aware layer (e.g. an AVC
/// NAL-unit splitter reading `codec_specific_parameters`) can decode a
/// `subs` table it already has in hand without re-running `open()`.
/// Returns `Err` for a body shorter than the FullBox preamble +
/// `entry_count`, or a body truncated mid-entry / mid-subsample.
pub fn parse_subs_box(body: &[u8]) -> Result<SubsBox> {
    parse_subs(body)
}

/// Serialise a `subs` (SubSampleInformationBox, ISO/IEC 14496-12 §8.7.7)
/// from a [`SubsBox`] — the byte-exact inverse of [`parse_subs_box`].
///
/// Layout (§8.7.7.2): a FullBox preamble carrying the record's `version`
/// (0 or 1) and its codec-owned 24-bit `flags` verbatim, the 32-bit
/// `entry_count`, then per entry the 32-bit `sample_delta`, the 16-bit
/// `subsample_count`, and per sub-sample the `subsample_size` (16-bit at
/// version 0, 32-bit at version 1), `subsample_priority` (8-bit),
/// `discardable` (8-bit) and `codec_specific_parameters` (32-bit).
///
/// Returns `None` when the record cannot be expressed on the wire and
/// re-parse identically: a `version` other than 0/1; an `entry_count` or
/// any `subsample_count` exceeding its 32-/16-bit field; or — at version
/// 0 — a `subsample_size` exceeding `u16::MAX` (the v0 size field is
/// 16-bit; the caller must use version 1).
pub fn build_subs_box(record: &SubsBox) -> Option<Vec<u8>> {
    if record.version > 1 {
        return None;
    }
    let entry_count: u32 = record.entries.len().try_into().ok()?;
    let mut body = Vec::with_capacity(8 + record.entries.len() * 6);
    body.push(record.version);
    body.extend_from_slice(&record.flags.to_be_bytes()[1..4]); // 24-bit flags
    body.extend_from_slice(&entry_count.to_be_bytes());
    for entry in &record.entries {
        let subsample_count: u16 = entry.subsamples.len().try_into().ok()?;
        body.extend_from_slice(&entry.sample_delta.to_be_bytes());
        body.extend_from_slice(&subsample_count.to_be_bytes());
        for sub in &entry.subsamples {
            if record.version == 0 {
                let size: u16 = sub.subsample_size.try_into().ok()?;
                body.extend_from_slice(&size.to_be_bytes());
            } else {
                body.extend_from_slice(&sub.subsample_size.to_be_bytes());
            }
            body.push(sub.subsample_priority);
            body.push(sub.discardable);
            body.extend_from_slice(&sub.codec_specific_parameters.to_be_bytes());
        }
    }
    Some(wrap_box(&SUBS, &body))
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

/// Parse a standalone `saiz` (SampleAuxiliaryInformationSizesBox, ISO/IEC
/// 14496-12 §8.7.8) body (the bytes after the box header) into a
/// [`SaizBox`]. Public entry point for CENC / DRM tooling that already
/// holds the box payload.
pub fn parse_saiz_box(body: &[u8]) -> Result<SaizBox> {
    parse_saiz(body)
}

/// Parse a standalone `saio` (SampleAuxiliaryInformationOffsetsBox,
/// ISO/IEC 14496-12 §8.7.9) body into a [`SaioBox`]. Public entry point
/// paired with [`build_saio_box`].
pub fn parse_saio_box(body: &[u8]) -> Result<SaioBox> {
    parse_saio(body)
}

/// Serialise a `saiz` (SampleAuxiliaryInformationSizesBox, ISO/IEC
/// 14496-12 §8.7.8) from a [`SaizBox`] — the byte-exact inverse of
/// [`parse_saiz_box`].
///
/// The `aux_info_type` / `aux_info_type_parameter` key is written only
/// when `aux_info_type` is `Some` (which sets `flags & 1`); the parameter
/// defaults to 0 when omitted (§8.7.8.3). The body then carries the 8-bit
/// `default_sample_info_size`, the 32-bit `sample_count`, and — only when
/// `default_sample_info_size == 0` — the `per_sample` size table.
///
/// Returns `None` when the record cannot round-trip: a non-zero
/// `default_sample_info_size` paired with a non-empty `per_sample` (the
/// constant-size shortcut and the per-sample table are mutually
/// exclusive), or — for a variable-size table — a `sample_count` that
/// does not match `per_sample.len()`.
pub fn build_saiz_box(record: &SaizBox) -> Option<Vec<u8>> {
    if record.default_sample_info_size != 0 && !record.per_sample.is_empty() {
        return None;
    }
    if record.default_sample_info_size == 0
        && record.sample_count as usize != record.per_sample.len()
    {
        return None;
    }
    let mut body = Vec::new();
    let flags: u32 = if record.aux_info_type.is_some() { 1 } else { 0 };
    body.push(0); // version 0 (§8.7.8.2 defines only version 0)
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    if let Some(t) = record.aux_info_type {
        body.extend_from_slice(&t);
        body.extend_from_slice(&record.aux_info_type_parameter.unwrap_or(0).to_be_bytes());
    }
    body.push(record.default_sample_info_size);
    body.extend_from_slice(&record.sample_count.to_be_bytes());
    if record.default_sample_info_size == 0 {
        body.extend_from_slice(&record.per_sample);
    }
    Some(wrap_box(&SAIZ, &body))
}

/// Serialise a `saio` (SampleAuxiliaryInformationOffsetsBox, ISO/IEC
/// 14496-12 §8.7.9) from a [`SaioBox`] — the byte-exact inverse of
/// [`parse_saio_box`].
///
/// The `aux_info_type` key is written only when present (setting
/// `flags & 1`), the parameter defaulting to 0 when omitted. The body
/// then carries the 32-bit `entry_count` followed by the offset array,
/// each written as a 32-bit field for version 0 or a 64-bit field for
/// version 1 (§8.7.9.2).
///
/// Returns `None` for a `version` other than 0/1, an `offsets` count
/// exceeding the 32-bit `entry_count`, or — at version 0 — an offset
/// exceeding `u32::MAX` (the caller must use version 1).
pub fn build_saio_box(record: &SaioBox) -> Option<Vec<u8>> {
    if record.version > 1 {
        return None;
    }
    let entry_count: u32 = record.offsets.len().try_into().ok()?;
    let mut body = Vec::new();
    let flags: u32 = if record.aux_info_type.is_some() { 1 } else { 0 };
    body.push(record.version);
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    if let Some(t) = record.aux_info_type {
        body.extend_from_slice(&t);
        body.extend_from_slice(&record.aux_info_type_parameter.unwrap_or(0).to_be_bytes());
    }
    body.extend_from_slice(&entry_count.to_be_bytes());
    for &offset in &record.offsets {
        if record.version == 0 {
            let o: u32 = offset.try_into().ok()?;
            body.extend_from_slice(&o.to_be_bytes());
        } else {
            body.extend_from_slice(&offset.to_be_bytes());
        }
    }
    Some(wrap_box(&SAIO, &body))
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

/// One per-fragment ISO/IEC 14496-12 §8.9 sample-group record collected
/// from inside a `traf` (Track Fragment Box).
///
/// §8.9.2 / §8.9.3 / §8.9.5 permit `sbgp` / `csgp` and `sgpd` to live in
/// a `traf` as well as in `stbl`: a movie fragment can carry its own
/// **fragment-local** group descriptions (its own `sgpd`) and a
/// per-sample mapping (`sbgp` or `csgp`) into either the fragment-local
/// `sgpd` or the `trak`-level (global) one. The `csgp` bit-7
/// `index_msb_indicates_fragment_local_description` flag exists precisely
/// for this `traf` case (§8.9.5: "legal only inside a `traf`"). The
/// demuxer collects each `traf`'s sample-group boxes verbatim, keyed by
/// `(track_idx, moof_sequence)`, so a downstream layer can resolve
/// per-fragment grouping (most commonly CENC `seig` key rotation across
/// a fragment's samples) without re-walking the file. The bytes are not
/// interpreted here — the parsed `SampleToGroup` / `CompactSampleToGroup`
/// / `SampleGroupDescription` records preserve their on-wire values and
/// the grouping-type semantics belong to the caller (mirroring the
/// `stbl`-level surface).
#[derive(Clone, Debug)]
pub struct TrafSampleGroupRecord {
    /// `track_idx` of the matching stream in the demuxer's `streams()`
    /// list (0-based).
    pub track_idx: u32,
    /// `mfhd.sequence_number` of the enclosing `moof`. Surfaced so a
    /// caller replaying out of order can re-key.
    pub moof_sequence: u32,
    /// Fragment-local `sgpd` (SampleGroupDescriptionBox §8.9.3)
    /// descriptions, in `traf` order. These shadow the `trak`-level
    /// `sgpd` of the same `grouping_type` for samples in this fragment
    /// whose mapping selects the fragment-local source.
    pub sgpd: Vec<SgpdBox>,
    /// `sbgp` (SampleToGroupBox §8.9.2) per-sample run-length maps, in
    /// `traf` order.
    pub sbgp: Vec<SbgpBox>,
    /// `csgp` (CompactSampleToGroupBox §8.9.5) compact maps, in `traf`
    /// order. The MSB-fragment-local convention (bit 7) is meaningful
    /// here — see [`CsgpBox::index_msb_indicates_fragment_local_description`].
    pub csgp: Vec<CsgpBox>,
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
    traf_sample_groups: &mut Vec<TrafSampleGroupRecord>,
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
                    traf_sample_groups,
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

#[allow(clippy::too_many_arguments)]
// tfhd / tfdt / senc / saiz / saio / sample-group inputs are independent state carried through the same walk; bundling them into a struct hides the per-box ownership and obscures the moof→traf data flow
fn parse_traf(
    body: &[u8],
    moof_start: u64,
    tracks: &[Track],
    samples: &mut Vec<SampleRef>,
    next_dts: &mut [i64],
    moof_sequence: u32,
    senc_records: &mut Vec<SencRecord>,
    sai_records: &mut Vec<SaiRecord>,
    traf_sample_groups: &mut Vec<TrafSampleGroupRecord>,
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
    // §8.9 — fragment-local sample-group boxes. A `traf` may carry its
    // own `sgpd` descriptions plus `sbgp` / `csgp` per-sample maps (the
    // `csgp` bit-7 fragment-local convention is only legal here). Collect
    // them verbatim; the grouping-type semantics belong to the caller.
    let mut frag_sgpd: Vec<SgpdBox> = Vec::new();
    let mut frag_sbgp: Vec<SbgpBox> = Vec::new();
    let mut frag_csgp: Vec<CsgpBox> = Vec::new();

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
            // §8.9.3 / §8.9.2 / §8.9.5 — fragment-local sample-group
            // boxes. A malformed box is dropped (the fragment's samples
            // are still usable); a well-formed one is preserved verbatim.
            SGPD => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Ok(g) = parse_sgpd(&b) {
                    frag_sgpd.push(g);
                }
            }
            SBGP => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Ok(g) = parse_sbgp(&b) {
                    frag_sbgp.push(g);
                }
            }
            CSGP => {
                let b = read_bytes_vec(&mut cur, psz)?;
                if let Ok(g) = parse_csgp(&b) {
                    frag_csgp.push(g);
                }
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

    // §8.9 — record this traf's fragment-local sample-group boxes (if
    // any) keyed by `(track_idx, moof_sequence)`, so a downstream layer
    // can resolve per-fragment grouping (e.g. CENC `seig` key rotation)
    // without re-walking the file.
    if !frag_sgpd.is_empty() || !frag_sbgp.is_empty() || !frag_csgp.is_empty() {
        traf_sample_groups.push(TrafSampleGroupRecord {
            track_idx: state.track_idx as u32,
            moof_sequence,
            sgpd: frag_sgpd,
            sbgp: frag_sbgp,
            csgp: frag_csgp,
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
        // The one base-spec-defined `trep` child is `assp` (§8.8.16);
        // decode its body into a typed record. A malformed `assp` still
        // contributes its type + length (assp = None) so the child list
        // stays faithful to the wire and never aborts the trep parse.
        let assp = if &fourcc == b"assp" {
            parse_assp(&payload[off + header_len..off + effective]).ok()
        } else {
            None
        };
        children.push(TrepChild {
            fourcc,
            payload_len,
            assp,
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

/// Parse an `assp` (AlternativeStartupSequencePropertiesBox, ISO/IEC
/// 14496-12 §8.8.16) body — the bytes after the 8/16-byte box header.
///
/// Wire layout (§8.8.16.2):
/// ```text
/// FullBox('assp', version, 0) {
///     if (version == 0) {
///         signed int(32) min_initial_alt_startup_offset;
///     } else if (version == 1) {
///         unsigned int(32) num_entries;
///         for (j = 1; j <= num_entries; j++) {
///             unsigned int(32) grouping_type_parameter;
///             signed int(32)   min_initial_alt_startup_offset;
///         }
///     }
/// }
/// ```
///
/// Only versions 0 and 1 are defined (§8.8.16.1 ties the box version to
/// the `alst` `sbgp` version); any other version is rejected rather than
/// mis-read. A truncated body — too short for the v0 fixed offset, the
/// v1 `num_entries`, or a declared entry's 8 bytes — is rejected so a
/// partial table never lies about the recorded bounds. `num_entries` is
/// validated against the bytes actually present before allocation.
fn parse_assp(body: &[u8]) -> Result<AsspRecord> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4: assp too short for FullBox preamble"));
    }
    let version = body[0];
    // flags (body[1..4]) are pinned to 0 by §8.8.16.2 and unused here.
    let payload = &body[4..];
    match version {
        0 => {
            if payload.len() < 4 {
                return Err(Error::invalid("MP4: assp v0 truncated offset"));
            }
            let off = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            Ok(AsspRecord {
                version,
                entries: vec![AsspEntry {
                    grouping_type_parameter: None,
                    min_initial_alt_startup_offset: off,
                }],
            })
        }
        1 => {
            if payload.len() < 4 {
                return Err(Error::invalid("MP4: assp v1 truncated num_entries"));
            }
            let num_entries =
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            // Each entry is two 32-bit words (8 bytes). Reject a count
            // that can't be backed by the remaining bytes before
            // allocating.
            let entries_bytes = &payload[4..];
            if num_entries.saturating_mul(8) > entries_bytes.len() {
                return Err(Error::invalid("MP4: assp v1 num_entries overruns body"));
            }
            let mut entries = Vec::with_capacity(num_entries);
            let mut p = 0usize;
            for _ in 0..num_entries {
                let gtp = u32::from_be_bytes([
                    entries_bytes[p],
                    entries_bytes[p + 1],
                    entries_bytes[p + 2],
                    entries_bytes[p + 3],
                ]);
                let off = i32::from_be_bytes([
                    entries_bytes[p + 4],
                    entries_bytes[p + 5],
                    entries_bytes[p + 6],
                    entries_bytes[p + 7],
                ]);
                entries.push(AsspEntry {
                    grouping_type_parameter: Some(gtp),
                    min_initial_alt_startup_offset: off,
                });
                p += 8;
            }
            Ok(AsspRecord { version, entries })
        }
        v => Err(Error::invalid(format!("MP4: assp unsupported version {v}"))),
    }
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
    // 24-bit flags: 0 in the 2015 edition, named annotation bits in the
    // 2022 edition (the body layout is identical either way). Captured so
    // a 2022-aware consumer can interpret what the NTP time represents.
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
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
        flags,
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

    // QuickTime Base Media Info Atom (`minf/gmhd/gmin`): when the track
    // carries a `gmhd` in place of a typed media header (a base-media
    // track — text / timecode / music / generic), surface its control
    // information on `params.options`. `gmin_graphicsmode` is the decimal
    // composition mode (0 = copy); `gmin_opcolor` is the three
    // (red, green, blue) 16-bit components space-separated, decimal; and
    // `gmin_balance` is the signed stereo sound balance. Absent `gmhd`,
    // none of the keys are emitted.
    if let Some(g) = &t.gmin {
        params
            .options
            .insert("gmin_graphicsmode", g.graphicsmode.to_string());
        params.options.insert(
            "gmin_opcolor",
            format!("{} {} {}", g.opcolor[0], g.opcolor[1], g.opcolor[2]),
        );
        params.options.insert("gmin_balance", g.balance.to_string());
    }

    // QuickTime Timecode Media Info Atom (`minf/gmhd/tcmi`): when a
    // timecode track (media type `tmcd`) carries a `tcmi`, surface its
    // text-rendering parameters on `params.options`. `tcmi_text_font` /
    // `tcmi_text_face` / `tcmi_text_size` are decimal; `tcmi_text_color`
    // and `tcmi_background_color` are the three space-separated 16-bit RGB
    // components; and `tcmi_font_name` is the font name (omitted when
    // empty). Absent `tcmi`, none of the keys are emitted.
    if let Some(tc) = &t.tcmi {
        params
            .options
            .insert("tcmi_text_font", tc.text_font.to_string());
        params
            .options
            .insert("tcmi_text_face", tc.text_face.to_string());
        params
            .options
            .insert("tcmi_text_size", tc.text_size.to_string());
        params.options.insert(
            "tcmi_text_color",
            format!(
                "{} {} {}",
                tc.text_color[0], tc.text_color[1], tc.text_color[2]
            ),
        );
        params.options.insert(
            "tcmi_background_color",
            format!(
                "{} {} {}",
                tc.background_color[0], tc.background_color[1], tc.background_color[2]
            ),
        );
        if !tc.font_name.is_empty() {
            params
                .options
                .insert("tcmi_font_name", tc.font_name.clone());
        }
    }

    // QuickTime Timecode sample description (`stsd` `tmcd` entry): when the
    // track is a timecode track, surface the fields that define how its
    // timecode samples are read. `tmcd_flags` is the decimal flag word;
    // `tmcd_drop_frame` / `tmcd_24hour_max` / `tmcd_negative_ok` /
    // `tmcd_counter` are the decoded booleans; `tmcd_timescale`,
    // `tmcd_frame_duration`, and `tmcd_number_of_frames` are decimal.
    // Absent a `tmcd` entry, none of the keys are emitted.
    if let Some(tm) = &t.tmcd {
        params.options.insert("tmcd_flags", tm.flags.to_string());
        params
            .options
            .insert("tmcd_drop_frame", tm.drop_frame().to_string());
        params
            .options
            .insert("tmcd_24hour_max", tm.twenty_four_hour_max().to_string());
        params
            .options
            .insert("tmcd_negative_ok", tm.negative_times_ok().to_string());
        params
            .options
            .insert("tmcd_counter", tm.counter().to_string());
        params
            .options
            .insert("tmcd_timescale", tm.timescale.to_string());
        params
            .options
            .insert("tmcd_frame_duration", tm.frame_duration.to_string());
        params
            .options
            .insert("tmcd_number_of_frames", tm.number_of_frames.to_string());
    }

    // QuickTime Text sample description (`stsd` `text` entry): when a
    // QuickTime text track carries a structured `text` entry, surface its
    // draw parameters on `params.options`. `text_display_flags` is the
    // decimal flag word; `text_justification` is the signed alignment
    // (0 left / 1 centered / -1 right); `text_background_color` and
    // `text_foreground_color` are the three space-separated 16-bit RGB
    // components; `text_default_text_box` is top/left/bottom/right
    // space-separated; `text_font_number` / `text_font_face` are decimal;
    // and `text_font_name` is the font name (omitted when empty). Absent a
    // structured `text` entry, none of the keys are emitted.
    if let Some(tx) = &t.text_entry {
        params
            .options
            .insert("text_display_flags", tx.display_flags.to_string());
        params
            .options
            .insert("text_justification", tx.text_justification.to_string());
        params.options.insert(
            "text_background_color",
            format!(
                "{} {} {}",
                tx.background_color[0], tx.background_color[1], tx.background_color[2]
            ),
        );
        params.options.insert(
            "text_foreground_color",
            format!(
                "{} {} {}",
                tx.foreground_color[0], tx.foreground_color[1], tx.foreground_color[2]
            ),
        );
        params.options.insert(
            "text_default_text_box",
            format!(
                "{} {} {} {}",
                tx.default_text_box[0],
                tx.default_text_box[1],
                tx.default_text_box[2],
                tx.default_text_box[3]
            ),
        );
        params
            .options
            .insert("text_font_number", tx.font_number.to_string());
        params
            .options
            .insert("text_font_face", tx.font_face.to_string());
        if !tx.text_name.is_empty() {
            params
                .options
                .insert("text_font_name", tx.text_name.clone());
        }
    }

    // QuickTime Track Load Settings Atom (`trak/load`): when the track
    // carries a `load`, surface its preload / hint fields on
    // `params.options`. `load_preload_start_time` / `load_preload_duration`
    // / `load_preload_flags` / `load_default_hints` are decimal; the
    // decoded booleans `load_preload_always` / `load_preload_if_enabled` /
    // `load_double_buffer` / `load_high_quality` expose the flag / hint
    // bits. Absent `load`, none of the keys are emitted.
    if let Some(ls) = &t.load_settings {
        params
            .options
            .insert("load_preload_start_time", ls.preload_start_time.to_string());
        params
            .options
            .insert("load_preload_duration", ls.preload_duration.to_string());
        params
            .options
            .insert("load_preload_flags", ls.preload_flags.to_string());
        params
            .options
            .insert("load_default_hints", ls.default_hints.to_string());
        params
            .options
            .insert("load_preload_always", ls.preload_always().to_string());
        params.options.insert(
            "load_preload_if_enabled",
            ls.preload_if_enabled().to_string(),
        );
        params
            .options
            .insert("load_double_buffer", ls.double_buffer().to_string());
        params
            .options
            .insert("load_high_quality", ls.high_quality().to_string());
    }

    // ISO/IEC 14496-12 (post-2015): when the track's `VisualSampleEntry`
    // carried an `amve` (AmbientViewingEnvironmentBox), surface its three
    // raw fields on `params.options` as `amve_ambient_illuminance`
    // (0.0001 lux per unit), `amve_ambient_light_x`, and
    // `amve_ambient_light_y` (0.00002 CIE 1931 chromaticity per unit),
    // all decimal. The raw integers are preserved verbatim — a downstream
    // HDR pipeline applies the unit scaling and can populate the matching
    // `ambient_viewing_environment` SEI field-for-field with no
    // conversion. Absent `amve`, none of the keys are emitted.
    if let Some(a) = &t.amve {
        params.options.insert(
            "amve_ambient_illuminance",
            a.ambient_illuminance.to_string(),
        );
        params
            .options
            .insert("amve_ambient_light_x", a.ambient_light_x.to_string());
        params
            .options
            .insert("amve_ambient_light_y", a.ambient_light_y.to_string());
    }

    // ISO/IEC 14496-12 §8.15.4.2: when the track's sample-entry
    // `sinf/schi` carried an `stvi` (StereoVideoBox) — i.e. the
    // restricted-scheme SchemeType is `stvi` (stereoscopic video,
    // §8.15.4.1) — surface its three fields on `params.options`:
    // `stvi_single_view_allowed` (the 2-bit monoscopic-display
    // permission), `stvi_stereo_scheme` (1 = 14496-10 frame-packing
    // SEI, 2 = 13818-2 Annex L, 3 = 23000-11), both decimal, and
    // `stvi_indication` (the raw `stereo_indication_type` bytes as
    // lowercase hex — its meaning is `stereo_scheme`-specific, so it is
    // preserved verbatim for a downstream consumer). The
    // `stvi_indication` key is omitted when the indication is empty
    // (`length == 0`). Absent `stvi`, none of the keys are emitted.
    if let Some(s) = &t.stvi {
        params.options.insert(
            "stvi_single_view_allowed",
            s.single_view_allowed.to_string(),
        );
        params
            .options
            .insert("stvi_stereo_scheme", s.stereo_scheme.to_string());
        if !s.stereo_indication_type.is_empty() {
            let mut hex = String::with_capacity(s.stereo_indication_type.len() * 2);
            for b in &s.stereo_indication_type {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
            }
            params.options.insert("stvi_indication", hex);
        }
    }

    // ISO/IEC 14496-12 §8.5.2: when the track's active `SampleEntry`
    // carried a `btrt` (BitRateBox), surface its three bandwidth fields
    // on `params.options` as `btrt_buffer_size_db` (decoding buffer size
    // in bytes), `btrt_max_bitrate`, and `btrt_avg_bitrate` (bits/second,
    // over any one-second window / the whole presentation), all decimal.
    // An ABR packager or buffer-sizing player reads these directly
    // without parsing an `esds` (the box is codec-agnostic and present on
    // sample entries that carry no MPEG-4 descriptor). Absent `btrt`,
    // none of the keys are emitted.
    if let Some(b) = &t.btrt {
        params
            .options
            .insert("btrt_buffer_size_db", b.buffer_size_db.to_string());
        params
            .options
            .insert("btrt_max_bitrate", b.max_bitrate.to_string());
        params
            .options
            .insert("btrt_avg_bitrate", b.avg_bitrate.to_string());
    }

    // ISO/IEC 14496-12 §12.1.4 — `pasp` (PixelAspectRatioBox): surface
    // the pixel aspect ratio on `params.options` as `pasp_h_spacing` /
    // `pasp_v_spacing` (decimal). A renderer multiplies the stored
    // sample dimensions by this ratio to obtain the display aspect.
    // Absent `pasp`, square pixels are implied and no keys are emitted.
    if let Some(p) = &t.pasp {
        params
            .options
            .insert("pasp_h_spacing", p.h_spacing.to_string());
        params
            .options
            .insert("pasp_v_spacing", p.v_spacing.to_string());
    }

    // ISO/IEC 14496-12 §12.1.4 — `clap` (CleanApertureBox): surface the
    // clean-aperture rectangle as four `N/D` fractions on
    // `params.options` (`clap_width` / `clap_height` / `clap_horiz_off`
    // / `clap_vert_off`, each "<N>/<D>"). The active picture region after
    // overscan / edge artefacts are cropped. Absent `clap`, no keys.
    if let Some(c) = &t.clap {
        params
            .options
            .insert("clap_width", format!("{}/{}", c.width_n, c.width_d));
        params
            .options
            .insert("clap_height", format!("{}/{}", c.height_n, c.height_d));
        params.options.insert(
            "clap_horiz_off",
            format!("{}/{}", c.horiz_off_n, c.horiz_off_d),
        );
        params.options.insert(
            "clap_vert_off",
            format!("{}/{}", c.vert_off_n, c.vert_off_d),
        );
    }

    // ISO/IEC 14496-12 §12.1.5 — `colr` (ColourInformationBox): surface
    // the colour description on `params.options`. For an `nclx` box the
    // three 16-bit ISO/IEC 23091-2 codes + the range flag are emitted as
    // `colr_type=nclx`, `colr_primaries`, `colr_transfer`, `colr_matrix`
    // (decimal), and `colr_full_range` ("0"/"1"). For an ICC profile
    // (`rICC` / `prof`) only `colr_type` + `colr_icc_len` (profile byte
    // length) are emitted (the raw profile is reachable via the typed
    // record). An unknown colour type emits `colr_type=<fourcc>`.
    if let Some(c) = &t.colr {
        match c {
            ColrRecord::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            } => {
                params.options.insert("colr_type", "nclx".to_string());
                params
                    .options
                    .insert("colr_primaries", colour_primaries.to_string());
                params
                    .options
                    .insert("colr_transfer", transfer_characteristics.to_string());
                params
                    .options
                    .insert("colr_matrix", matrix_coefficients.to_string());
                params
                    .options
                    .insert("colr_full_range", (*full_range as u8).to_string());
            }
            ColrRecord::RestrictedIcc(d) => {
                params.options.insert("colr_type", "rICC".to_string());
                params.options.insert("colr_icc_len", d.len().to_string());
            }
            ColrRecord::UnrestrictedIcc(d) => {
                params.options.insert("colr_type", "prof".to_string());
                params.options.insert("colr_icc_len", d.len().to_string());
            }
            ColrRecord::Other { colour_type, .. } => {
                params.options.insert(
                    "colr_type",
                    String::from_utf8_lossy(colour_type).to_string(),
                );
            }
        }
    }

    // ISO/IEC 14496-12 §12.4.2 / §8.4.5: when the track carries an
    // `hmhd` (HintMediaHeaderBox) — i.e. a hint track — surface its
    // protocol-independent streaming statistics on `params.options`:
    // `hmhd_max_pdu_size` / `hmhd_avg_pdu_size` (byte sizes of the
    // largest / average Protocol Data Unit) and `hmhd_max_bitrate` /
    // `hmhd_avg_bitrate` (bits/second over any one-second window / the
    // whole presentation), all decimal. A streaming server pacing
    // delivery or sizing buffers reads these instead of re-walking
    // `minf`. Absent `hmhd`, none of the keys are emitted.
    if let Some(h) = &t.hmhd {
        params
            .options
            .insert("hmhd_max_pdu_size", h.max_pdu_size.to_string());
        params
            .options
            .insert("hmhd_avg_pdu_size", h.avg_pdu_size.to_string());
        params
            .options
            .insert("hmhd_max_bitrate", h.max_bitrate.to_string());
        params
            .options
            .insert("hmhd_avg_bitrate", h.avg_bitrate.to_string());
    }

    // ISO/IEC 14496-12 §9.1.2 / §9.4.1.2: when the track's active stsd
    // entry is an RTP / SRTP / reception hint sample entry (`rtp ` /
    // `srtp` / `rrtp`), surface its packetisation parameters on
    // `params.options`: `rtp_hint_format` (the entry FourCC),
    // `rtp_hint_max_packet_size` (the largest packet this track
    // generates), `rtp_hint_timescale` (the `tims` RTP clock), and the
    // optional `rtp_hint_time_offset` / `rtp_hint_sequence_offset` (`tsro`
    // / `snro`, signed, decimal — omitted when the box is absent). An
    // `srtp` entry also emits `rtp_hint_srtp = "true"`. A streaming server
    // reads these to reconstruct the RTP stream without re-walking `stsd`.
    if let Some(r) = &t.rtp_hint {
        params.options.insert(
            "rtp_hint_format",
            String::from_utf8_lossy(&r.format).to_string(),
        );
        params
            .options
            .insert("rtp_hint_max_packet_size", r.max_packet_size.to_string());
        if let Some(ts) = r.timescale {
            params.options.insert("rtp_hint_timescale", ts.to_string());
        }
        if let Some(off) = r.time_offset {
            params
                .options
                .insert("rtp_hint_time_offset", off.to_string());
        }
        if let Some(off) = r.sequence_offset {
            params
                .options
                .insert("rtp_hint_sequence_offset", off.to_string());
        }
        if r.srpp.is_some() {
            params.options.insert("rtp_hint_srtp", "true".to_string());
        }
    }

    // ISO/IEC 14496-12 §9.3.3.2: when the track's active stsd entry is an
    // MPEG-2 TS server / reception hint sample entry (`sm2t` / `rm2t`),
    // surface its per-TS-packet framing on `params.options`:
    // `m2t_hint_format` (the entry FourCC), `m2t_hint_preceding_bytes` /
    // `m2t_hint_trailing_bytes` (recording-device bytes wrapped around
    // each 188-byte TS packet), and `m2t_hint_precomputed` (`"true"` when
    // the precomputed-only flag is set). A de-hinter reads these to strip
    // the wrapping bytes before reassembling the TS.
    if let Some(m) = &t.mpeg2ts_hint {
        params.options.insert(
            "m2t_hint_format",
            String::from_utf8_lossy(&m.format).to_string(),
        );
        params.options.insert(
            "m2t_hint_preceding_bytes",
            m.preceding_bytes_len.to_string(),
        );
        params
            .options
            .insert("m2t_hint_trailing_bytes", m.trailing_bytes_len.to_string());
        if m.precomputed_only {
            params
                .options
                .insert("m2t_hint_precomputed", "true".to_string());
        }
    }

    // ISO/IEC 14496-12 §9.1.5: when the hint track's `udta` carried a
    // `hinf` Hint Statistics Box, surface its present statistics on
    // `params.options` (each key emitted only when the corresponding
    // sub-box was present): `hinf_bytes_sent` (`trpy`, incl. RTP
    // headers), `hinf_packets_sent` (`nump`), `hinf_payload_bytes`
    // (`tpyl`, excl. RTP headers), `hinf_media_bytes` (`dmed`),
    // `hinf_immediate_bytes` (`dimm`), `hinf_repeated_bytes` (`drep`),
    // `hinf_largest_packet` (`pmax`), `hinf_maxr_count` (number of `maxr`
    // windows), and `hinf_payload_count` (number of `payt` entries). A
    // streaming server / analytics tool reads delivery totals without
    // walking `udta`. The u32 variants (`totl`/`npck`/`tpay`) prefer the
    // u64 fields when both forms are present.
    {
        let h = &t.hint_stats;
        if let Some(v) = h
            .bytes_sent_with_rtp_64
            .or(h.bytes_sent_with_rtp_32.map(u64::from))
        {
            params.options.insert("hinf_bytes_sent", v.to_string());
        }
        if let Some(v) = h.packets_sent_64.or(h.packets_sent_32.map(u64::from)) {
            params.options.insert("hinf_packets_sent", v.to_string());
        }
        if let Some(v) = h
            .bytes_sent_no_rtp_64
            .or(h.bytes_sent_no_rtp_32.map(u64::from))
        {
            params.options.insert("hinf_payload_bytes", v.to_string());
        }
        if let Some(v) = h.media_bytes_sent {
            params.options.insert("hinf_media_bytes", v.to_string());
        }
        if let Some(v) = h.immediate_bytes_sent {
            params.options.insert("hinf_immediate_bytes", v.to_string());
        }
        if let Some(v) = h.repeated_bytes_sent {
            params.options.insert("hinf_repeated_bytes", v.to_string());
        }
        if let Some(v) = h.largest_packet {
            params.options.insert("hinf_largest_packet", v.to_string());
        }
        if !h.max_rates.is_empty() {
            params
                .options
                .insert("hinf_maxr_count", h.max_rates.len().to_string());
        }
        if !h.payload_ids.is_empty() {
            params
                .options
                .insert("hinf_payload_count", h.payload_ids.len().to_string());
        }
    }

    // ISO/IEC 14496-12 §8.7.2: surface the DataReferenceBox (`dref`) so a
    // caller can resolve where each sample description's media data lives.
    // The 1-based `data_reference_index` on every sample entry (§8.5.2.2)
    // indexes this table. The overwhelmingly common case — a single
    // self-contained `url ` entry, i.e. all samples in this same file — is
    // surfaced compactly as `dref_self_contained = "true"` with no
    // per-entry keys, so a consumer can confirm "no external resources" in
    // one lookup. When the table has more than one entry, or any entry is
    // *not* self-contained (an external `url `/`urn ` split-source track),
    // every entry is surfaced:
    //   * `dref_count` — total entries.
    //   * `dref_self_contained` — "true" only when *every* entry is
    //     self-contained, else "false".
    //   * `dref_<n>` (1-based, matching `data_reference_index`) —
    //     `<kind> self=<true|false>[ name=<urn>][ loc=<url>]`. `kind` is
    //     `url ` or `urn `; the `name=` token appears only for a `urn `
    //     with a name; `loc=` only when a location string is present.
    // Absent / unparseable `dref`, none of the keys are emitted.
    if let Some(d) = &t.dref {
        let all_self_contained =
            !d.entries.is_empty() && d.entries.iter().all(|e| e.is_self_contained());
        let single_self_contained = d.entries.len() == 1 && all_self_contained;
        params.options.insert(
            "dref_self_contained",
            if all_self_contained { "true" } else { "false" }.to_string(),
        );
        if !single_self_contained {
            params
                .options
                .insert("dref_count".to_string(), d.entries.len().to_string());
            for (i, e) in d.entries.iter().enumerate() {
                let mut s = format!("{} self={}", fourcc_token(&e.kind), e.is_self_contained());
                if let Some(name) = &e.name {
                    s.push_str(" name=");
                    s.push_str(name);
                }
                if let Some(loc) = &e.location {
                    s.push_str(" loc=");
                    s.push_str(loc);
                }
                params.options.insert(format!("dref_{}", i + 1), s);
            }
        }
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
        // §8.9.5 flag bit 7: when set, the high bit of each index is a
        // fragment-local-vs-global source selector. Mark it so tooling
        // reading the flat surface knows the indices need MSB resolution.
        if cg.index_msb_indicates_fragment_local_description {
            v.push_str(" fraglocal");
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

/// The concrete MP4 / ISOBMFF demuxer behind [`open`].
///
/// Constructed via [`open_typed`] when a caller needs the structured
/// inherent accessors (`sidxes` / `ssixes` / `psshes` / `senc_records` /
/// `sai_records` / `traf_sample_groups` / …) that don't fit the
/// [`Demuxer`] trait — most notably a CENC decrypting layer replaying
/// per-sample IVs and subsample maps from the per-fragment `senc`
/// records. All fields are private; the trait implementation is
/// identical through either construction path.
pub struct Mp4Demuxer {
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
    /// Parsed QuickTime Preview Atom (`pnot`), if the file carries one at
    /// the top level. Reachable via the public `Mp4Demuxer::pnot`
    /// accessor and surfaced on the flat metadata channel as `pnot`.
    /// Informational only — the demuxer does not consult it.
    #[allow(dead_code)]
    pnot: Option<PnotRecord>,
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
    /// ISO/IEC 14496-12 §8.9 per-fragment sample-group records. One per
    /// `traf` that carried at least one `sgpd` / `sbgp` / `csgp` box,
    /// keyed by `(track_idx, moof_sequence)`. Empty in non-fragmented
    /// files and in fragmented files that carry sample groups only at
    /// `trak`/`stbl` level (the common case).
    #[allow(dead_code)]
    traf_sample_groups: Vec<TrafSampleGroupRecord>,
    /// ISO/IEC 14496-12 §8.11 — the file-level `meta` box item
    /// infrastructure (HEIF / MIAF item catalogue), if the file carries
    /// one. Holds the primary item, item-location directory, item-info
    /// entries, and item references. `MetaItems::default()` (all-`None`)
    /// for a non-HEIF file or a plain iTunes-metadata `meta`. Reachable
    /// via the public `Mp4Demuxer::meta_items()` accessor; also surfaced
    /// on the flat metadata channel as `meta_*` keys.
    #[allow(dead_code)]
    meta_items: MetaItems,
    /// §8.11.7 file-level `meco` AdditionalMetadataContainerBox — the
    /// additional `meta` boxes (distinct handler types) plus `mere`
    /// relations that complement the primary `meta`. `MecoBox::default()`
    /// (empty) when no `meco` is present. Reachable via the public
    /// `Mp4Demuxer::meco()` accessor; also surfaced on the flat metadata
    /// channel as `meco_*` keys.
    #[allow(dead_code)]
    meco: MecoBox,
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

    /// The parsed QuickTime Preview Atom (`pnot`), if the file carried one
    /// at the top level. Locates the movie's preview (poster) image: the
    /// FourCC of the atom holding the preview data, which instance of that
    /// type to use, and the Macintosh-format modification date. `None`
    /// when no `pnot` was present. Also surfaced on the flat metadata
    /// channel as `pnot`.
    #[allow(dead_code)]
    pub fn pnot(&self) -> Option<&PnotRecord> {
        self.pnot.as_ref()
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

    /// ISO/IEC 14496-12 §8.9 — per-fragment sample-group records, one
    /// per `traf` that carried at least one `sgpd` / `sbgp` / `csgp`
    /// box. A movie fragment may declare its own **fragment-local**
    /// group descriptions (§8.9.3 `sgpd` inside `traf`) and map its
    /// samples into either those or the `trak`-level descriptions via a
    /// `sbgp` (§8.9.2) or `csgp` (§8.9.5) — the `csgp` bit-7
    /// `index_msb_indicates_fragment_local_description` flag selects the
    /// source per index and is only legal in this `traf` context. Each
    /// record references this demuxer's `streams()` list via `track_idx`
    /// and the containing `mfhd.sequence_number` via `moof_sequence`; the
    /// parsed boxes preserve their on-wire values verbatim (grouping-type
    /// semantics belong to the caller, mirroring the `stbl`-level
    /// surface). Most commonly populated for CENC `seig` key rotation
    /// across a fragment's samples. Empty in non-fragmented files and in
    /// fragmented files carrying sample groups only at `trak`/`stbl`
    /// level (the common case).
    #[allow(dead_code)]
    pub fn traf_sample_groups(&self) -> &[TrafSampleGroupRecord] {
        &self.traf_sample_groups
    }

    /// ISO/IEC 14496-12 §8.11 — the file-level `meta` box item
    /// infrastructure (HEIF / MIAF item catalogue): the primary item ID
    /// (`pitm`), the item-location directory (`iloc`), item-info entries
    /// (`iinf` / `infe`), and typed item references (`iref`), plus the
    /// `meta`'s handler type. Returns an all-`None` [`MetaItems`] (via
    /// [`MetaItems::is_empty`]) for a non-HEIF file or a `meta` that
    /// carries only iTunes-style `hdlr` + `ilst`. The same data is also
    /// surfaced on the flat metadata channel as `meta_*` keys.
    #[allow(dead_code)]
    pub fn meta_items(&self) -> &MetaItems {
        &self.meta_items
    }

    /// The file-level `meco` AdditionalMetadataContainerBox (ISO/IEC
    /// 14496-12 §8.11.7), if one was present. The additional `meta` boxes
    /// (each a fully-parsed [`MetaItems`] with a handler type distinct
    /// from the primary `meta`) plus the `mere` relation boxes
    /// (§8.11.8) describing how the same-level `meta` boxes relate.
    /// Returns an empty [`MecoBox`] ([`MecoBox::is_empty`]) when no
    /// `meco` was present. The same data is surfaced on the flat
    /// metadata channel as `meco_*` keys.
    #[allow(dead_code)]
    pub fn meco(&self) -> &MecoBox {
        &self.meco
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

/// Serialise a `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12
/// §8.16.5) from a [`PrftRecord`] — the byte-exact inverse of
/// [`parse_prft_box`].
///
/// Layout (§8.16.5.2): a FullBox preamble carrying the record's `version`
/// (0 or 1) and its 24-bit `flags` (verbatim — `0` in the 2015 edition,
/// the named NTP-annotation bits in the 2022 edition), then the 32-bit
/// `reference_track_ID`, the 64-bit NTP `ntp_timestamp`, and the
/// `media_time` — written as a 32-bit field for version 0 and a 64-bit
/// field for version 1.
///
/// Returns `None` when `version == 0` but `media_time` exceeds
/// `u32::MAX` (it would not fit the v0 32-bit field and re-parse
/// identically — the caller must bump the record to version 1), or for a
/// `version` other than 0/1 (no other version is defined).
pub fn build_prft_box(record: &PrftRecord) -> Option<Vec<u8>> {
    if record.version > 1 {
        return None;
    }
    let mut body = Vec::with_capacity(24);
    // FullBox preamble: 1-byte version + the 24-bit flags (low 3 bytes).
    body.push(record.version);
    body.extend_from_slice(&record.flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&record.reference_track_id.to_be_bytes());
    body.extend_from_slice(&record.ntp_timestamp.to_be_bytes());
    if record.version == 0 {
        let mt: u32 = record.media_time.try_into().ok()?;
        body.extend_from_slice(&mt.to_be_bytes());
    } else {
        body.extend_from_slice(&record.media_time.to_be_bytes());
    }
    Some(wrap_box(&crate::boxes::PRFT, &body))
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

/// Serialise a `pdin` (ProgressiveDownloadInfoBox, ISO/IEC 14496-12
/// §8.1.3) from its `(rate, initial_delay)` entries — the byte-exact
/// inverse of [`parse_pdin_box`] (FullBox version 0, flags 0).
///
/// Layout (§8.1.3.2): the 4-byte FullBox preamble followed by one
/// `(rate, initial_delay)` u32 pair per entry, each big-endian. `rate`
/// is a download bitrate in bytes/second; `initial_delay` is the
/// suggested initial playback delay (in the file's time-scale ticks)
/// when downloading at that rate. The entries are written in supplied
/// order — §8.1.3.3 expects them sorted by ascending `rate`, but this
/// builder preserves the caller's order rather than re-sorting. An empty
/// entry list yields a preamble-only box (a legal, content-free `pdin`).
pub fn build_pdin_box(record: &PdinRecord) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + record.entries.len() * 8);
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    for e in &record.entries {
        body.extend_from_slice(&e.rate.to_be_bytes());
        body.extend_from_slice(&e.initial_delay.to_be_bytes());
    }
    wrap_box(&crate::boxes::PDIN, &body)
}

/// Parse a QuickTime `pnot` (Preview Atom) body from `body` (the 12 bytes
/// after the plain 8-byte box header — `pnot` is a plain `Box`, no FullBox
/// preamble): `unsigned int(32) modification_date`, `unsigned int(16)
/// version`, `unsigned int(32) atom_type`, `unsigned int(16) atom_index`.
/// Returns `None` for a body shorter than the fixed 12 bytes; trailing
/// bytes are ignored.
fn parse_pnot(body: &[u8]) -> Option<PnotRecord> {
    if body.len() < 12 {
        return None;
    }
    let modification_date = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let version = u16::from_be_bytes([body[4], body[5]]);
    let atom_type = [body[6], body[7], body[8], body[9]];
    let atom_index = u16::from_be_bytes([body[10], body[11]]);
    Some(PnotRecord {
        modification_date,
        version,
        atom_type,
        atom_index,
    })
}

/// Parse a standalone QuickTime `pnot` (Preview Atom) body from `body`
/// (the 12 bytes after the plain 8-byte box header). Exposed so tooling
/// holding the atom's payload can recover the preview locator without
/// re-running `open()`. `Err` for a body shorter than the fixed 12-byte
/// layout; trailing bytes are ignored.
pub fn parse_pnot_box(body: &[u8]) -> Result<PnotRecord> {
    parse_pnot(body).ok_or_else(|| Error::invalid("MP4: pnot too short"))
}

/// Build a complete QuickTime `pnot` (Preview Atom) box from a
/// [`PnotRecord`]. The byte-exact inverse of [`parse_pnot_box`]: the four
/// fields with no FullBox preamble.
pub fn build_pnot_box(r: &PnotRecord) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&r.modification_date.to_be_bytes());
    body.extend_from_slice(&r.version.to_be_bytes());
    body.extend_from_slice(&r.atom_type);
    body.extend_from_slice(&r.atom_index.to_be_bytes());
    wrap_box(&crate::boxes::PNOT, &body)
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

/// Serialise a [`LevaRecord`] into a complete `leva` box — 8-byte header
/// (`[size:u32]['leva']`) plus the §8.8.13.2 v0 body — for fragmented-MP4
/// emitters that declare per-level sample assignments in `mvex`.
///
/// The body is the §8.8.13.2 `FullBox(version = 0, flags = 0)` preamble,
/// the one-byte `level_count`, then one variable-length entry per
/// [`LevaEntry`]: `track_id`(u32), a packed
/// `padding_flag<<7 | assignment_type` byte, and the assignment-type
/// tail (`grouping_type` for type 0; `grouping_type` +
/// `grouping_type_parameter` for type 1; nothing for types 2/3;
/// `sub_track_id` for type 4; nothing for reserved types > 4 — the spec
/// defines no tail length there, so none is written).
///
/// The caller owns the §8.8.13.3 conformance constraints the record type
/// cannot express: `level_count` should be ≥ 2, and the
/// `assignment_type` sequence should be "zero or more of type 2 or 3,
/// followed by zero or more of exactly one type." `assignment_type`
/// values above the 7-bit field (> 127) are rejected rather than
/// silently truncated into the `padding_flag` bit.
pub fn build_leva_box(record: &LevaRecord) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(8 + record.entries.len() * 9);
    body.extend_from_slice(&[0u8; 4]); // FullBox(version = 0, flags = 0)
    if record.entries.len() > u8::MAX as usize {
        return Err(Error::invalid("MP4: leva level_count exceeds 255"));
    }
    body.push(record.entries.len() as u8);
    for e in &record.entries {
        if e.assignment_type > 0x7f {
            return Err(Error::invalid(
                "MP4: leva assignment_type exceeds the 7-bit field",
            ));
        }
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
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"leva");
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a standalone `csgp` (CompactSampleToGroupBox, ISO/IEC
/// 14496-12:2020 §8.9.5) body from `body` (the bytes after the 8/16-byte
/// box header).
///
/// Exposed as a public entry point so tooling that already has the box's
/// payload bytes in hand (a DASH packager, a sample-group editor, a
/// fixture validator) can recover the bit-packed pattern table without
/// re-running `open()`, and so the write side
/// (`sample_groups::build_csgp`) can be round-trip tested against the
/// canonical reader. Returns `Err` for a body shorter than the 4-byte
/// FullBox preamble or with any truncated bit-packed field. The returned
/// [`CsgpBox`] preserves every `sample_group_description_index` verbatim
/// (including the fragment-local high bit when the box came from a
/// `traf`).
pub fn parse_csgp_box(body: &[u8]) -> Result<CsgpBox> {
    parse_csgp(body)
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

/// Serialise a [`TrepRecord`] into a complete `trep`
/// (TrackExtensionPropertiesBox, ISO/IEC 14496-12 §8.8.15) box — 8-byte
/// header (`[size:u32]['trep']`) plus the §8.8.15.2 `FullBox(version = 0,
/// flags = 0)` body — for a muxer placing it inside the init-segment
/// `mvex`.
///
/// The body is the FullBox preamble, the 4-byte `track_id`, then each
/// child box in order. The one base-spec-defined child is `assp`
/// (§8.8.16): when a [`TrepChild`] carries a `Some(AsspRecord)`, that
/// record is serialised via [`build_assp_box`] (its `fourcc` is then
/// required to be `assp`, and its `payload_len` is ignored — the bytes
/// come from the typed record). A child with `assp == None` is emitted as
/// an empty placeholder box of its `fourcc` with no payload; since this
/// crate does not model arbitrary child payloads, a non-`assp` child with
/// a non-zero `payload_len` is rejected rather than emitting a box whose
/// declared length would not match its (absent) content.
///
/// A round-trip `parse_trep_box(&build_trep_box(rec)?[8..])` reproduces
/// `rec` for records built from typed `assp` children (and bare-track-id
/// records).
pub fn build_trep_box(record: &TrepRecord) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    body.extend_from_slice(&record.track_id.to_be_bytes());
    for child in &record.children {
        match &child.assp {
            Some(a) => {
                if &child.fourcc != b"assp" {
                    return Err(Error::invalid(
                        "MP4: trep child carries an AsspRecord but its fourcc is not 'assp'",
                    ));
                }
                body.extend_from_slice(&build_assp_box(a)?);
            }
            None => {
                if child.payload_len != 0 {
                    return Err(Error::invalid(
                        "MP4: trep child without a typed record must have payload_len == 0",
                    ));
                }
                // Empty placeholder box: 8-byte header, no payload.
                body.extend_from_slice(&8u32.to_be_bytes());
                body.extend_from_slice(&child.fourcc);
            }
        }
    }
    let total = 8 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"trep");
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a standalone `assp` (AlternativeStartupSequencePropertiesBox,
/// ISO/IEC 14496-12 §8.8.16) body from `body` (the bytes after the
/// 8/16-byte box header — `assp` is a `FullBox`, so the body opens with
/// the 4-byte version/flags preamble).
///
/// Exposed as a public entry point so tooling that already has the box's
/// payload bytes in hand (an alternative-startup-sequence analyser
/// pairing this with the §10.3.2 `alst` sample group, a DASH packager,
/// a fixture validator) can recover the typed [`AsspRecord`] without
/// re-running `open()`. The demuxer also fills this in automatically for
/// the `assp` child of any parsed `trep` (see [`TrepChild::assp`]).
///
/// Returns `Err` for a body shorter than the 4-byte FullBox preamble, a
/// truncated v0 offset / v1 `num_entries`, a v1 `num_entries` that
/// overruns the body, or an unsupported version (only 0 and 1 are
/// defined by §8.8.16.1).
pub fn parse_assp_box(body: &[u8]) -> Result<AsspRecord> {
    parse_assp(body)
}

/// Serialise an [`AsspRecord`] into a complete `assp` box — 8-byte
/// header (`[size:u32]['assp']`) plus the §8.8.16.2 `FullBox(version,
/// flags = 0)` body — for a muxer placing it inside a `trep`
/// (TrackExtensionPropertiesBox, §8.8.15).
///
/// The byte-exact inverse of [`parse_assp_box`]: a v0 record writes the
/// single entry's `min_initial_alt_startup_offset`; a v1 record writes
/// `num_entries` followed by each entry's `(grouping_type_parameter,
/// min_initial_alt_startup_offset)` pair. Rejects a record whose
/// `version` is neither 0 nor 1, a v0 record that does not carry exactly
/// one entry, or a v0 entry that carries a `grouping_type_parameter`
/// (which has no slot in the v0 wire layout) — the builder never emits a
/// box that would not round-trip.
pub fn build_assp_box(record: &AsspRecord) -> Result<Vec<u8>> {
    let body: Vec<u8> = match record.version {
        0 => {
            if record.entries.len() != 1 {
                return Err(Error::invalid("MP4: assp v0 must carry exactly one entry"));
            }
            let e = &record.entries[0];
            if e.grouping_type_parameter.is_some() {
                return Err(Error::invalid(
                    "MP4: assp v0 entry must not carry grouping_type_parameter",
                ));
            }
            let mut b = Vec::with_capacity(4 + 4);
            b.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags 0
            b.extend_from_slice(&e.min_initial_alt_startup_offset.to_be_bytes());
            b
        }
        1 => {
            let num_entries = record.entries.len();
            if num_entries > u32::MAX as usize {
                return Err(Error::invalid("MP4: assp v1 num_entries exceeds u32"));
            }
            let mut b = Vec::with_capacity(4 + 4 + num_entries * 8);
            b.extend_from_slice(&[1, 0, 0, 0]); // version 1 + flags 0
            b.extend_from_slice(&(num_entries as u32).to_be_bytes());
            for e in &record.entries {
                let gtp = e.grouping_type_parameter.unwrap_or(0);
                b.extend_from_slice(&gtp.to_be_bytes());
                b.extend_from_slice(&e.min_initial_alt_startup_offset.to_be_bytes());
            }
            b
        }
        v => {
            return Err(Error::invalid(format!("MP4: assp unsupported version {v}")));
        }
    };
    let total = 8 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"assp");
    out.extend_from_slice(&body);
    Ok(out)
}

/// Parse a standalone `amve` (AmbientViewingEnvironmentBox, ISO/IEC
/// 14496-12 post-2015 addition) body from `body` (the 8 bytes after the
/// plain 8-byte box header — `amve` is a `Box`, not a `FullBox`, so
/// there is no version/flags preamble).
///
/// Exposed as a public entry point so tooling that already has the box's
/// payload bytes in hand (an HDR-metadata extractor, a HEIF/AVIF
/// item-property reader carrying the same box, a fixture validator) can
/// recover the `(ambient_illuminance, ambient_light_x, ambient_light_y)`
/// triple without re-running `open()`. Returns `Err` for a body shorter
/// than the fixed 8-byte layout; trailing bytes beyond byte 8 (reserved
/// for a future edition) are ignored.
pub fn parse_amve_box(body: &[u8]) -> Result<AmveRecord> {
    parse_amve(body).ok_or_else(|| Error::invalid("MP4: amve too short"))
}

/// Parse a standalone `btrt` (BitRateBox, ISO/IEC 14496-12 §8.5.2) body
/// from `body` (the 12 bytes after the plain 8-byte box header — `btrt`
/// is a `Box`, not a `FullBox`, so there is no version/flags preamble).
///
/// Exposed as a public entry point so tooling that already has the box's
/// payload bytes in hand (a bitrate / bandwidth extractor, an ABR
/// packager validating a producer's declared rates, a fixture checker)
/// can recover the `(buffer_size_db, max_bitrate, avg_bitrate)` triple
/// without re-running `open()`. Returns `Err` for a body shorter than
/// the fixed 12-byte layout; trailing bytes beyond byte 12 are ignored.
pub fn parse_btrt_box(body: &[u8]) -> Result<BtrtRecord> {
    parse_btrt(body).ok_or_else(|| Error::invalid("MP4: btrt too short"))
}

/// Parse a standalone `pasp` (PixelAspectRatioBox, ISO/IEC 14496-12
/// §12.1.4) body. `Err` for a body shorter than the fixed 8-byte
/// layout; trailing bytes are ignored.
pub fn parse_pasp_box(body: &[u8]) -> Result<PaspRecord> {
    parse_pasp(body).ok_or_else(|| Error::invalid("MP4: pasp too short"))
}

/// Parse a standalone `clap` (CleanApertureBox, ISO/IEC 14496-12
/// §12.1.4) body. `Err` for a body shorter than the fixed 32-byte
/// (eight u32) layout; trailing bytes are ignored.
pub fn parse_clap_box(body: &[u8]) -> Result<ClapRecord> {
    parse_clap(body).ok_or_else(|| Error::invalid("MP4: clap too short"))
}

/// Parse a standalone `colr` (ColourInformationBox, ISO/IEC 14496-12
/// §12.1.5) body. `Err` for a body shorter than the 4-byte
/// `colour_type` or with a truncated `nclx` payload.
pub fn parse_colr_box(body: &[u8]) -> Result<ColrRecord> {
    parse_colr(body).ok_or_else(|| Error::invalid("MP4: colr malformed"))
}

/// Build a complete `pasp` (PixelAspectRatioBox, ISO/IEC 14496-12
/// §12.1.4) box from a [`PaspRecord`]. The byte-exact inverse of
/// [`parse_pasp_box`].
pub fn build_pasp_box(r: &PaspRecord) -> Vec<u8> {
    let mut body = r.h_spacing.to_be_bytes().to_vec();
    body.extend_from_slice(&r.v_spacing.to_be_bytes());
    wrap_box(b"pasp", &body)
}

/// Build a complete `clap` (CleanApertureBox, ISO/IEC 14496-12 §12.1.4)
/// box from a [`ClapRecord`]. The byte-exact inverse of
/// [`parse_clap_box`].
pub fn build_clap_box(r: &ClapRecord) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    for v in [
        r.width_n,
        r.width_d,
        r.height_n,
        r.height_d,
        r.horiz_off_n,
        r.horiz_off_d,
        r.vert_off_n,
        r.vert_off_d,
    ] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    wrap_box(b"clap", &body)
}

/// Build a complete `colr` (ColourInformationBox, ISO/IEC 14496-12
/// §12.1.5) box from a [`ColrRecord`]. The byte-exact inverse of
/// [`parse_colr_box`] (an `nclx` box's seven reserved low bits are
/// written as 0, matching the parser's mask).
pub fn build_colr_box(r: &ColrRecord) -> Vec<u8> {
    let mut body = Vec::new();
    match r {
        ColrRecord::Nclx {
            colour_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        } => {
            body.extend_from_slice(b"nclx");
            body.extend_from_slice(&colour_primaries.to_be_bytes());
            body.extend_from_slice(&transfer_characteristics.to_be_bytes());
            body.extend_from_slice(&matrix_coefficients.to_be_bytes());
            body.push(if *full_range { 0x80 } else { 0x00 });
        }
        ColrRecord::RestrictedIcc(d) => {
            body.extend_from_slice(b"rICC");
            body.extend_from_slice(d);
        }
        ColrRecord::UnrestrictedIcc(d) => {
            body.extend_from_slice(b"prof");
            body.extend_from_slice(d);
        }
        ColrRecord::Other { colour_type, data } => {
            body.extend_from_slice(colour_type);
            body.extend_from_slice(data);
        }
    }
    wrap_box(b"colr", &body)
}

/// Parse a standalone `stvi` (StereoVideoBox, ISO/IEC 14496-12
/// §8.15.4.2) body from `body` (the bytes after the 4-byte FullBox
/// version/flags preamble — `stvi` is a `FullBox`).
///
/// Exposed as a public entry point so tooling that already has the box's
/// payload bytes in hand (a stereoscopic-video extractor, a
/// restricted-scheme inspector, a fixture validator) can recover the
/// `(single_view_allowed, stereo_scheme, stereo_indication_type)` triple
/// without re-running `open()`. Returns `Err` for a body too short to
/// hold the fixed header, or one whose `length` field overruns the bytes
/// present; trailing optional `any_box` bytes after the
/// `stereo_indication_type` array are ignored.
pub fn parse_stvi_box(body: &[u8]) -> Result<StviRecord> {
    parse_stvi(body).ok_or_else(|| Error::invalid("MP4: stvi malformed"))
}

/// Serialise a [`StviRecord`] into a complete `stvi` box — 8-byte header
/// (`[size:u32]['stvi']`) plus the §8.15.4.2.2 `FullBox(version = 0,
/// flags = 0)` body — for a restricted-scheme (`stvi` SchemeType) muxer
/// that places it inside a sample entry's `sinf/schi`.
///
/// The body is the packed `reserved(30)` / `single_view_allowed(2)`
/// word (only the low 2 bits of `single_view_allowed` are written; the
/// reserved bits are zero per §8.15.4.2.3), the `stereo_scheme` u32, the
/// `length` u32 derived from `stereo_indication_type.len()`, and the
/// `stereo_indication_type` bytes verbatim. No trailing `any_box` is
/// emitted. The byte-exact inverse of [`parse_stvi_box`] (which consumes
/// the body after the 8-byte header) — a round-trip through the two
/// preserves every field. `length` must fit a `u32`; an indication
/// longer than `u32::MAX` is rejected.
pub fn build_stvi_box(record: &StviRecord) -> Result<Vec<u8>> {
    let length = record.stereo_indication_type.len();
    if length > u32::MAX as usize {
        return Err(Error::invalid("MP4: stvi indication exceeds u32 length"));
    }
    let body_len = 4 + 4 + 4 + 4 + length; // preamble + word + scheme + length + array
    let mut out = Vec::with_capacity(8 + body_len);
    out.extend_from_slice(&((8 + body_len) as u32).to_be_bytes());
    out.extend_from_slice(b"stvi");
    out.extend_from_slice(&[0u8; 4]); // FullBox version 0, flags 0
                                      // First word: reserved(30) = 0 in the high bits, single_view_allowed
                                      // in the low 2 bits.
    out.extend_from_slice(&((record.single_view_allowed as u32) & 0x3).to_be_bytes());
    out.extend_from_slice(&record.stereo_scheme.to_be_bytes());
    out.extend_from_slice(&(length as u32).to_be_bytes());
    out.extend_from_slice(&record.stereo_indication_type);
    Ok(out)
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
            amve: None,
            stvi: None,
            btrt: None,
            pasp: None,
            clap: None,
            colr: None,
            hmhd: None,
            gmin: None,
            tcmi: None,
            tmcd: None,
            text_entry: None,
            load_settings: None,
            rtp_hint: None,
            mpeg2ts_hint: None,
            hint_stats: crate::hint::HintStatistics::default(),
            dref: None,
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

    /// `build_tref_box` is the byte-exact inverse of `parse_tref`: a
    /// multi-type reference set re-parses to the same `(type, ids)` pairs.
    #[test]
    fn build_tref_round_trips_multiple_types() {
        let refs = vec![
            (*b"subt", vec![4u32, 5]),
            (*b"cdsc", vec![2u32]),
            (*b"chap", vec![3u32]),
        ];
        let boxed = super::build_tref_box(&refs).unwrap();
        // Outer box is a plain `tref` container (8-byte header).
        assert_eq!(&boxed[4..8], b"tref");
        let mut t = fresh_track();
        super::parse_tref(&boxed[8..], &mut t).unwrap();
        assert_eq!(t.tref, refs);
    }

    /// An empty `track_IDs` array for a type is legal (box references
    /// nothing) and round-trips to an empty list.
    #[test]
    fn build_tref_empty_id_array() {
        let refs = vec![(*b"hint", Vec::<u32>::new())];
        let boxed = super::build_tref_box(&refs).unwrap();
        let mut t = fresh_track();
        super::parse_tref(&boxed[8..], &mut t).unwrap();
        // An all-empty child contributes a `(type, [])` pair.
        assert_eq!(t.tref.len(), 1);
        assert_eq!(&t.tref[0].0, b"hint");
        assert!(t.tref[0].1.is_empty());
    }

    /// §8.3.3.3 — a zero `track_ID` is rejected (it would be dropped by
    /// the parser, breaking round-trip).
    #[test]
    fn build_tref_rejects_zero_id() {
        let refs = vec![(*b"font", vec![0u32])];
        assert!(super::build_tref_box(&refs).is_none());
    }

    /// No reference pairs yields a bare empty `tref` container.
    #[test]
    fn build_tref_empty_set() {
        let boxed = super::build_tref_box(&[]).unwrap();
        assert_eq!(boxed.len(), 8);
        assert_eq!(&boxed[4..8], b"tref");
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

    /// `build_trgr_box` is the byte-exact inverse of `parse_trgr`: a
    /// multi-type group set re-parses to the same `(type, id)` pairs.
    #[test]
    fn build_trgr_round_trips() {
        let groups = vec![(*b"msrc", 17u32), (*b"ster", 42u32)];
        let boxed = super::build_trgr_box(&groups);
        assert_eq!(&boxed[4..8], b"trgr");
        let mut t = fresh_track();
        super::parse_trgr(&boxed[8..], &mut t).unwrap();
        assert_eq!(t.trgr, groups);
    }

    /// No group pairs yields a bare empty `trgr` container.
    #[test]
    fn build_trgr_empty_set() {
        let boxed = super::build_trgr_box(&[]);
        assert_eq!(boxed.len(), 8);
        assert_eq!(&boxed[4..8], b"trgr");
        let mut t = fresh_track();
        super::parse_trgr(&boxed[8..], &mut t).unwrap();
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

    #[test]
    fn parse_track_udta_picks_up_hinf() {
        let stats = crate::hint::HintStatistics {
            bytes_sent_with_rtp_64: Some(123_456),
            packets_sent_64: Some(789),
            ..crate::hint::HintStatistics::default()
        };
        let hinf = crate::hint::build_hinf_box(&stats);
        let mut udta = Vec::new();
        udta.extend_from_slice(&hinf);
        let mut t = fresh_track();
        super::parse_track_udta(&udta, &mut t);
        assert_eq!(t.hint_stats, stats);
        assert_eq!(t.hint_stats.bytes_sent_with_rtp_64, Some(123_456));
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

    /// `build_cprt_box` is the byte-exact inverse of `parse_cprt`: a
    /// language + UTF-8 notice re-parses to the same record.
    #[test]
    fn build_cprt_round_trips() {
        let boxed = super::build_cprt_box(b"eng", "(c) 2026 Example").unwrap();
        assert_eq!(&boxed[4..8], b"cprt");
        let mut t = fresh_track();
        super::parse_cprt(&boxed[8..], &mut t);
        assert_eq!(t.copyrights.len(), 1);
        assert_eq!(&t.copyrights[0].language, b"eng");
        assert_eq!(t.copyrights[0].notice, "(c) 2026 Example");
    }

    /// An empty notice round-trips while preserving the language.
    #[test]
    fn build_cprt_empty_notice() {
        let boxed = super::build_cprt_box(b"fra", "").unwrap();
        let mut t = fresh_track();
        super::parse_cprt(&boxed[8..], &mut t);
        assert_eq!(&t.copyrights[0].language, b"fra");
        assert_eq!(t.copyrights[0].notice, "");
    }

    /// A non-lowercase language byte cannot be encoded into the 5-bit
    /// field; an embedded NUL in the notice would truncate on re-parse.
    #[test]
    fn build_cprt_rejects_bad_inputs() {
        assert!(super::build_cprt_box(b"EN1", "x").is_none());
        assert!(super::build_cprt_box(b"eng", "bad\0notice").is_none());
    }

    /// `build_kind_box` is the byte-exact inverse of `parse_kind`.
    #[test]
    fn build_kind_round_trips() {
        let boxed = super::build_kind_box("urn:mpeg:dash:role:2011", "subtitle").unwrap();
        assert_eq!(&boxed[4..8], b"kind");
        let mut t = fresh_track();
        super::parse_kind(&boxed[8..], &mut t);
        assert_eq!(t.kinds.len(), 1);
        assert_eq!(t.kinds[0].0, "urn:mpeg:dash:role:2011");
        assert_eq!(t.kinds[0].1, "subtitle");
    }

    /// An empty `value` round-trips (URI-only kind).
    #[test]
    fn build_kind_empty_value() {
        let boxed = super::build_kind_box("urn:scheme", "").unwrap();
        let mut t = fresh_track();
        super::parse_kind(&boxed[8..], &mut t);
        assert_eq!(t.kinds[0].0, "urn:scheme");
        assert_eq!(t.kinds[0].1, "");
    }

    /// An empty scheme URI (§8.10.4.3) or an embedded NUL is rejected.
    #[test]
    fn build_kind_rejects_bad_inputs() {
        assert!(super::build_kind_box("", "x").is_none());
        assert!(super::build_kind_box("urn:a\0b", "x").is_none());
        assert!(super::build_kind_box("urn:a", "x\0y").is_none());
    }

    /// `build_tsel_box` is the byte-exact inverse of `parse_tsel`,
    /// preserving the signed switch_group and the FourCC attribute list.
    #[test]
    fn build_tsel_round_trips() {
        let boxed = super::build_tsel_box(-5, &[*b"bitr", *b"frar"]);
        assert_eq!(&boxed[4..8], b"tsel");
        let mut t = fresh_track();
        super::parse_tsel(&boxed[8..], &mut t);
        let sel = t.tsel.unwrap();
        assert_eq!(sel.switch_group, -5);
        assert_eq!(sel.attribute_list, vec![*b"bitr", *b"frar"]);
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

    /// `build_stsg_box` is the byte-exact inverse of `parse_stsg`.
    #[test]
    fn build_stsg_round_trips() {
        let boxed = super::build_stsg_box(b"tele", &[1, 2, 3]).unwrap();
        assert_eq!(&boxed[4..8], b"stsg");
        let sg = super::parse_stsg(&boxed[8..]).unwrap();
        assert_eq!(&sg.grouping_type, b"tele");
        assert_eq!(sg.group_description_indices, vec![1, 2, 3]);
    }

    /// An empty index list yields `item_count == 0`.
    #[test]
    fn build_stsg_empty_indices() {
        let boxed = super::build_stsg_box(b"roll", &[]).unwrap();
        let sg = super::parse_stsg(&boxed[8..]).unwrap();
        assert_eq!(&sg.grouping_type, b"roll");
        assert!(sg.group_description_indices.is_empty());
    }

    /// `build_stri_box` is the byte-exact inverse of `parse_stri`,
    /// including signed group fields and the FourCC attribute list.
    #[test]
    fn build_stri_round_trips() {
        let boxed = super::build_stri_box(-2, 7, 9, &[*b"tesc", *b"bitr"]);
        assert_eq!(&boxed[4..8], b"stri");
        let st = super::parse_stri(&boxed[8..]).unwrap();
        assert_eq!(st.switch_group, -2);
        assert_eq!(st.alternate_group, 7);
        assert_eq!(st.sub_track_id, 9);
        assert_eq!(st.attribute_list, vec![*b"tesc", *b"bitr"]);
    }

    /// `build_strk_box` composes a full `strk` (stri + strd/stsg) that
    /// re-parses through `parse_strk` to the same sub-track record.
    #[test]
    fn build_strk_full_round_trips() {
        let groups = vec![(*b"tele", vec![1u32, 2]), (*b"roll", vec![4u32])];
        let boxed = super::build_strk_box(2, 2, 5, &[*b"tesc"], &groups).unwrap();
        assert_eq!(&boxed[4..8], b"strk");
        let mut t = fresh_track();
        super::parse_strk(&boxed[8..], &mut t);
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

    /// With no sample groups, `build_strk_box` emits a `stri`-only `strk`
    /// (no empty `strd` container).
    #[test]
    fn build_strk_stri_only_omits_strd() {
        let boxed = super::build_strk_box(1, 0, 3, &[], &[]).unwrap();
        let mut t = fresh_track();
        super::parse_strk(&boxed[8..], &mut t);
        assert_eq!(t.sub_tracks.len(), 1);
        assert_eq!(t.sub_tracks[0].sub_track_id, 3);
        assert!(t.sub_tracks[0].sample_groups.is_empty());
        // The only child is the `stri` box: no `strd` container present.
        assert!(!boxed.windows(4).any(|w| w == b"strd"));
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

    /// §12.4.2 — `hmhd` body after the FullBox preamble is two 16-bit
    /// PDU sizes, two 32-bit bitrates, and a reserved 32-bit word.
    /// Decode a typical hint-track header.
    #[test]
    fn parse_hmhd_pdu_sizes_and_bitrates() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]); // version 0, flags 0
        body.extend_from_slice(&1500u16.to_be_bytes()); // maxPDUsize
        body.extend_from_slice(&820u16.to_be_bytes()); // avgPDUsize
        body.extend_from_slice(&3_000_000u32.to_be_bytes()); // maxbitrate
        body.extend_from_slice(&1_200_000u32.to_be_bytes()); // avgbitrate
        body.extend_from_slice(&0u32.to_be_bytes()); // reserved
        let h = super::parse_hmhd(&body).unwrap();
        assert_eq!(h.max_pdu_size, 1500);
        assert_eq!(h.avg_pdu_size, 820);
        assert_eq!(h.max_bitrate, 3_000_000);
        assert_eq!(h.avg_bitrate, 1_200_000);
    }

    /// The parser does not require `version == 0` — a stray non-zero
    /// version byte still yields a usable header rather than a drop
    /// (mirroring `parse_vmhd`).
    #[test]
    fn parse_hmhd_tolerates_nonzero_version() {
        let mut body = Vec::new();
        body.extend_from_slice(&[2u8, 0, 0, 0]); // version 2
        body.extend_from_slice(&64u16.to_be_bytes());
        body.extend_from_slice(&64u16.to_be_bytes());
        body.extend_from_slice(&500u32.to_be_bytes());
        body.extend_from_slice(&500u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        let h = super::parse_hmhd(&body).unwrap();
        assert_eq!(h.max_pdu_size, 64);
        assert_eq!(h.max_bitrate, 500);
    }

    /// A non-zero reserved trailing word is read past, not surfaced —
    /// the parse still succeeds with the four real fields intact.
    #[test]
    fn parse_hmhd_nonzero_reserved_tolerated() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.extend_from_slice(&10u16.to_be_bytes());
        body.extend_from_slice(&20u16.to_be_bytes());
        body.extend_from_slice(&30u32.to_be_bytes());
        body.extend_from_slice(&40u32.to_be_bytes());
        body.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // reserved != 0
        let h = super::parse_hmhd(&body).unwrap();
        assert_eq!(h.avg_bitrate, 40);
    }

    /// A body shorter than the full 20 bytes (preamble + 2×u16 +
    /// 2×u32 + reserved u32) is rejected — a truncated tail would
    /// surface noise as a bitrate.
    #[test]
    fn parse_hmhd_too_short_is_rejected() {
        // 19 bytes: one short of the reserved word.
        let body = vec![0u8; 19];
        assert!(super::parse_hmhd(&body).is_err());
    }

    /// `parse_minf` lands the `hmhd` on the track.
    #[test]
    fn parse_minf_picks_up_hmhd() {
        let mut hmhd = Vec::new();
        hmhd.extend_from_slice(&[0u8, 0, 0, 0]);
        hmhd.extend_from_slice(&1400u16.to_be_bytes());
        hmhd.extend_from_slice(&700u16.to_be_bytes());
        hmhd.extend_from_slice(&2_500_000u32.to_be_bytes());
        hmhd.extend_from_slice(&900_000u32.to_be_bytes());
        hmhd.extend_from_slice(&0u32.to_be_bytes());
        let mut minf = Vec::new();
        minf.extend(wrap_box_full_size(b"hmhd", &hmhd));

        let mut t = fresh_track();
        super::parse_minf(&minf, &mut t).unwrap();
        let h = t.hmhd.expect("hmhd should be parsed");
        assert_eq!(h.max_pdu_size, 1400);
        assert_eq!(h.avg_pdu_size, 700);
        assert_eq!(h.max_bitrate, 2_500_000);
        assert_eq!(h.avg_bitrate, 900_000);
    }

    /// §12.4.2 fixes the quantity at one — `parse_minf` keeps the first
    /// `hmhd` and ignores a stray duplicate.
    #[test]
    fn parse_minf_keeps_first_hmhd_ignores_duplicate() {
        let mut first = Vec::new();
        first.extend_from_slice(&[0u8, 0, 0, 0]);
        first.extend_from_slice(&111u16.to_be_bytes());
        first.extend_from_slice(&[0u8; 14]);
        let mut second = Vec::new();
        second.extend_from_slice(&[0u8, 0, 0, 0]);
        second.extend_from_slice(&222u16.to_be_bytes());
        second.extend_from_slice(&[0u8; 14]);
        let mut minf = Vec::new();
        minf.extend(wrap_box_full_size(b"hmhd", &first));
        minf.extend(wrap_box_full_size(b"hmhd", &second));

        let mut t = fresh_track();
        super::parse_minf(&minf, &mut t).unwrap();
        let h = t.hmhd.expect("hmhd should be parsed");
        assert_eq!(h.max_pdu_size, 111);
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

    /// `parse_gmin` decodes the QuickTime Base Media Info Atom body
    /// (graphicsmode + opcolor + balance) after its FullBox preamble.
    #[test]
    fn parse_gmin_fields() {
        let mut body = vec![0u8, 0, 0, 0]; // version + flags
        body.extend_from_slice(&0x0040u16.to_be_bytes()); // graphicsmode = dither copy
        body.extend_from_slice(&0x8000u16.to_be_bytes()); // opcolor R
        body.extend_from_slice(&0x8001u16.to_be_bytes()); // opcolor G
        body.extend_from_slice(&0x8002u16.to_be_bytes()); // opcolor B
        body.extend_from_slice(&(-256i16).to_be_bytes()); // balance = -1.0 (8.8)
        body.extend_from_slice(&[0u8, 0]); // reserved
        let g = super::parse_gmin(&body).expect("gmin should parse");
        assert_eq!(g.graphicsmode, 0x0040);
        assert_eq!(g.opcolor, [0x8000, 0x8001, 0x8002]);
        assert_eq!(g.balance, -256);
    }

    /// A `gmin` body shorter than the fixed 16 bytes is rejected rather
    /// than surfacing truncation noise.
    #[test]
    fn parse_gmin_too_short_is_rejected() {
        assert!(super::parse_gmin(&[0u8; 15]).is_none());
        assert!(super::parse_gmin_box(&[0u8; 15]).is_err());
    }

    /// `build_gmin_box` → `parse_gmin_box` is a byte-exact round-trip, and
    /// the built box carries the correct 8-byte `gmin` header + 16-byte
    /// body (24 bytes total).
    #[test]
    fn build_gmin_box_round_trips() {
        let g = super::GminBox {
            graphicsmode: 0x0100,
            opcolor: [0x1234, 0x5678, 0x9abc],
            balance: 128,
        };
        let boxed = super::build_gmin_box(&g);
        assert_eq!(boxed.len(), 24);
        assert_eq!(&boxed[4..8], b"gmin");
        assert_eq!(
            u32::from_be_bytes([boxed[0], boxed[1], boxed[2], boxed[3]]),
            24
        );
        let back = super::parse_gmin_box(&boxed[8..]).expect("round-trip parse");
        assert_eq!(back, g);
    }

    /// `build_gmhd_box` wraps a single built `gmin`; `parse_gmhd` walks it
    /// back to the same record.
    #[test]
    fn build_gmhd_box_wraps_gmin() {
        let g = super::GminBox {
            graphicsmode: 0,
            opcolor: [0, 0, 0],
            balance: 0,
        };
        let gmhd = super::build_gmhd_box(&g);
        assert_eq!(&gmhd[4..8], b"gmhd");
        // total = 8 (gmhd header) + 24 (gmin box) = 32
        assert_eq!(gmhd.len(), 32);
        let mut t = fresh_track();
        super::parse_gmhd(&gmhd[8..], &mut t);
        assert_eq!(t.gmin, Some(g));
        assert_eq!(t.tcmi, None);
    }

    /// A `gmhd` carrying an unrelated child before `gmin` still resolves
    /// the `gmin` (media-specific children are skipped).
    #[test]
    fn parse_gmhd_skips_foreign_children() {
        let g = super::GminBox {
            graphicsmode: 0x0040,
            opcolor: [1, 2, 3],
            balance: -64,
        };
        // A `text` media-info child (arbitrary bytes) before the `gmin`.
        let mut inner = super::wrap_box(b"text", &[0xAAu8; 20]);
        inner.extend_from_slice(&super::build_gmin_box(&g));
        let mut t = fresh_track();
        super::parse_gmhd(&inner, &mut t);
        assert_eq!(t.gmin, Some(g));
    }

    /// A timecode-track `gmhd` carrying both a `gmin` and a `tcmi` resolves
    /// each into its own track field.
    #[test]
    fn parse_gmhd_captures_gmin_and_tcmi() {
        let g = super::GminBox {
            graphicsmode: 0,
            opcolor: [0, 0, 0],
            balance: 0,
        };
        let tc = super::TcmiBox {
            text_font: 0,
            text_face: 0,
            text_size: 12,
            text_color: [0xFFFF, 0xFFFF, 0xFFFF],
            background_color: [0, 0, 0],
            font_name: "Chicago".to_string(),
        };
        let mut inner = super::build_gmin_box(&g);
        inner.extend_from_slice(&super::build_tcmi_box(&tc));
        let mut t = fresh_track();
        super::parse_gmhd(&inner, &mut t);
        assert_eq!(t.gmin, Some(g));
        assert_eq!(t.tcmi, Some(tc));
    }

    /// `build_tcmi_box` → `parse_tcmi_box` is a byte-exact round-trip,
    /// including the Pascal-string font name.
    #[test]
    fn build_tcmi_box_round_trips() {
        let tc = super::TcmiBox {
            text_font: 3,
            text_face: 0x01, // Bold
            text_size: 18,
            text_color: [0x1234, 0x5678, 0x9abc],
            background_color: [0xdead, 0xbeef, 0xcafe],
            font_name: "Monaco".to_string(),
        };
        let boxed = super::build_tcmi_box(&tc);
        assert_eq!(&boxed[4..8], b"tcmi");
        let back = super::parse_tcmi_box(&boxed[8..]).expect("round-trip parse");
        assert_eq!(back, tc);
    }

    /// A `tcmi` with an empty font name round-trips to an empty `String`.
    #[test]
    fn build_tcmi_box_empty_name_round_trips() {
        let tc = super::TcmiBox {
            text_font: 0,
            text_face: 0,
            text_size: 10,
            text_color: [0, 0, 0],
            background_color: [0xFFFF, 0xFFFF, 0xFFFF],
            font_name: String::new(),
        };
        let boxed = super::build_tcmi_box(&tc);
        let back = super::parse_tcmi_box(&boxed[8..]).expect("round-trip parse");
        assert_eq!(back, tc);
    }

    /// A `tcmi` body shorter than the fixed 22-byte prefix is rejected;
    /// a font-name length that overruns the body is clamped rather than
    /// dropping the whole atom.
    #[test]
    fn parse_tcmi_short_and_overrun() {
        assert!(super::parse_tcmi(&[0u8; 21]).is_none());
        assert!(super::parse_tcmi_box(&[0u8; 21]).is_err());
        // 22 fixed bytes + a length byte claiming 200 chars but only 3
        // present → the name is clamped to the 3 available bytes.
        let mut body = vec![0u8; 22];
        body.push(200);
        body.extend_from_slice(b"abc");
        let tc = super::parse_tcmi(&body).expect("clamped parse");
        assert_eq!(tc.font_name, "abc");
    }

    /// Surfacing: a parsed `tcmi` exposes its rendering parameters on
    /// `params.options` for a timecode track.
    #[test]
    fn build_stream_info_surfaces_tcmi_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"tmcd";
        t.timescale = 1000;
        t.tcmi = Some(super::TcmiBox {
            text_font: 0,
            text_face: 1,
            text_size: 24,
            text_color: [0xFFFF, 0xFFFF, 0xFFFF],
            background_color: [0, 0, 0],
            font_name: "Helvetica".to_string(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tcmi_text_font"), Some("0"));
        assert_eq!(info.params.options.get("tcmi_text_face"), Some("1"));
        assert_eq!(info.params.options.get("tcmi_text_size"), Some("24"));
        assert_eq!(
            info.params.options.get("tcmi_text_color"),
            Some("65535 65535 65535")
        );
        assert_eq!(
            info.params.options.get("tcmi_background_color"),
            Some("0 0 0")
        );
        assert_eq!(info.params.options.get("tcmi_font_name"), Some("Helvetica"));
    }

    /// `build_tmcd_sample_entry` → `parse_tmcd_sample_entry_box` is a
    /// byte-exact round-trip; the drop-frame example (29.97 fps NTSC:
    /// timescale 30000, frame_duration 1001, 30 nominal frames, drop-frame
    /// flag) decodes field-for-field.
    #[test]
    fn tmcd_sample_entry_round_trips_ntsc_drop_frame() {
        let e = super::TmcdSampleEntry {
            flags: 0x0001 | 0x0002, // drop-frame + 24-hour max
            timescale: 30_000,
            frame_duration: 1001,
            number_of_frames: 30,
        };
        let boxed = super::build_tmcd_sample_entry(&e, 1);
        assert_eq!(&boxed[4..8], b"tmcd");
        // data_reference_index at preamble offset 6..8 (box header + 6).
        assert_eq!(u16::from_be_bytes([boxed[8 + 6], boxed[8 + 7]]), 1);
        let back = super::parse_tmcd_sample_entry_box(&boxed[8..]).expect("round-trip");
        assert_eq!(back, e);
        assert!(back.drop_frame());
        assert!(back.twenty_four_hour_max());
        assert!(!back.negative_times_ok());
        assert!(!back.counter());
    }

    /// A `tmcd` entry too short to hold the fixed numeric fields is
    /// rejected rather than surfacing truncation noise.
    #[test]
    fn tmcd_sample_entry_too_short_is_rejected() {
        assert!(super::parse_tmcd_sample_entry(&[0u8; 24]).is_none());
        assert!(super::parse_tmcd_sample_entry_box(&[0u8; 24]).is_err());
    }

    /// A `tmcd` entry parsed through `parse_stsd` populates `t.tmcd` and
    /// leaves the FourCC/media-type handling intact.
    #[test]
    fn parse_stsd_captures_tmcd_entry() {
        let e = super::TmcdSampleEntry {
            flags: 0x0008, // counter
            timescale: 25,
            frame_duration: 1,
            number_of_frames: 25,
        };
        let entry = super::build_tmcd_sample_entry(&e, 1);
        // stsd: FullBox preamble (4) + entry_count(=1) + the entry box.
        let mut stsd = vec![0u8, 0, 0, 0];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&entry);
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        super::parse_stsd(&stsd, &mut t).expect("stsd parse");
        assert_eq!(t.codec_id_fourcc, *b"tmcd");
        assert_eq!(t.tmcd, Some(e));
        assert!(t.tmcd.unwrap().counter());
    }

    /// Surfacing: a parsed `tmcd` sample entry exposes its fields (flags,
    /// decoded booleans, timescale, frame_duration, number_of_frames) on
    /// `params.options`.
    #[test]
    fn build_stream_info_surfaces_tmcd_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"tmcd";
        t.timescale = 30_000;
        t.tmcd = Some(super::TmcdSampleEntry {
            flags: 0x0001,
            timescale: 30_000,
            frame_duration: 1001,
            number_of_frames: 30,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("tmcd_flags"), Some("1"));
        assert_eq!(info.params.options.get("tmcd_drop_frame"), Some("true"));
        assert_eq!(info.params.options.get("tmcd_24hour_max"), Some("false"));
        assert_eq!(info.params.options.get("tmcd_timescale"), Some("30000"));
        assert_eq!(info.params.options.get("tmcd_frame_duration"), Some("1001"));
        assert_eq!(info.params.options.get("tmcd_number_of_frames"), Some("30"));
    }

    /// `build_text_sample_entry` → `parse_text_sample_entry_box` is a
    /// byte-exact round-trip, including the signed justification, the
    /// signed default-text-box rectangle, and the Pascal-string font name.
    #[test]
    fn text_sample_entry_round_trips() {
        let e = super::TextSampleEntry {
            display_flags: 0x2000 | 0x1000, // anti-alias + drop-shadow
            text_justification: -1,         // right-justified
            background_color: [0x1111, 0x2222, 0x3333],
            default_text_box: [0, 0, 60, 320],
            font_number: 0,
            font_face: 0x02, // italic
            foreground_color: [0xFFFF, 0xFFFF, 0xFFFF],
            text_name: "Geneva".to_string(),
        };
        let boxed = super::build_text_sample_entry(&e, 1);
        assert_eq!(&boxed[4..8], b"text");
        assert_eq!(u16::from_be_bytes([boxed[8 + 6], boxed[8 + 7]]), 1);
        let back = super::parse_text_sample_entry_box(&boxed[8..]).expect("round-trip");
        assert_eq!(back, e);
    }

    /// A `text` entry shorter than the 51-byte fixed prefix is rejected.
    #[test]
    fn text_sample_entry_too_short_is_rejected() {
        assert!(super::parse_text_sample_entry(&[0u8; 50]).is_none());
        assert!(super::parse_text_sample_entry_box(&[0u8; 50]).is_err());
    }

    /// A `text` entry with a font-name length that overruns the body is
    /// clamped to the bytes present rather than dropping the whole entry.
    #[test]
    fn text_sample_entry_name_overrun_clamped() {
        let mut e = vec![0u8; 51];
        e.push(200); // claim 200 chars
        e.extend_from_slice(b"xy");
        let tx = super::parse_text_sample_entry(&e).expect("clamped parse");
        assert_eq!(tx.text_name, "xy");
    }

    /// A `text` entry parsed through `parse_stsd` populates `t.text_entry`
    /// and keeps the raw post-preamble bytes as extradata.
    #[test]
    fn parse_stsd_captures_text_entry() {
        let e = super::TextSampleEntry {
            display_flags: 0,
            text_justification: 1, // centered
            background_color: [0, 0, 0],
            default_text_box: [0, 0, 0, 0],
            font_number: 0,
            font_face: 0,
            foreground_color: [0xFFFF, 0xFFFF, 0xFFFF],
            text_name: String::new(),
        };
        let entry = super::build_text_sample_entry(&e, 1);
        let mut stsd = vec![0u8, 0, 0, 0];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&entry);
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        super::parse_stsd(&stsd, &mut t).expect("stsd parse");
        assert_eq!(t.codec_id_fourcc, *b"text");
        assert_eq!(t.text_entry, Some(e));
        assert!(!t.extradata.is_empty());
    }

    /// Surfacing: a parsed `text` sample entry exposes its draw parameters
    /// on `params.options`.
    #[test]
    fn build_stream_info_surfaces_text_entry_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"text";
        t.timescale = 1000;
        t.text_entry = Some(super::TextSampleEntry {
            display_flags: 0x2000,
            text_justification: -1,
            background_color: [0, 0, 0],
            default_text_box: [1, 2, 3, 4],
            font_number: 0,
            font_face: 1,
            foreground_color: [0xFFFF, 0xFFFF, 0xFFFF],
            text_name: "Courier".to_string(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("text_display_flags"), Some("8192"));
        assert_eq!(info.params.options.get("text_justification"), Some("-1"));
        assert_eq!(
            info.params.options.get("text_default_text_box"),
            Some("1 2 3 4")
        );
        assert_eq!(
            info.params.options.get("text_foreground_color"),
            Some("65535 65535 65535")
        );
        assert_eq!(info.params.options.get("text_font_face"), Some("1"));
        assert_eq!(info.params.options.get("text_font_name"), Some("Courier"));
    }

    /// `build_load_settings_box` → `parse_load_settings_box` is a
    /// byte-exact round-trip; the signed `-1` "preload to end of track"
    /// sentinel and the flag / hint decoders behave as specified.
    #[test]
    fn load_settings_round_trips_and_decodes_flags() {
        let ls = super::LoadSettingsBox {
            preload_start_time: 0,
            preload_duration: -1, // to end of track
            preload_flags: 2,     // preload only if enabled
            default_hints: 0x0020 | 0x0100,
        };
        let boxed = super::build_load_settings_box(&ls);
        assert_eq!(&boxed[4..8], b"load");
        assert_eq!(boxed.len(), 24);
        let back = super::parse_load_settings_box(&boxed[8..]).expect("round-trip");
        assert_eq!(back, ls);
        assert!(!back.preload_always());
        assert!(back.preload_if_enabled());
        assert!(back.double_buffer());
        assert!(back.high_quality());
    }

    /// A `load` body shorter than the fixed 16 bytes is rejected.
    #[test]
    fn load_settings_too_short_is_rejected() {
        assert!(super::parse_load_settings(&[0u8; 15]).is_none());
        assert!(super::parse_load_settings_box(&[0u8; 15]).is_err());
    }

    /// A `load` atom nested in a `trak` is picked up by `parse_trak`.
    #[test]
    fn parse_trak_picks_up_nested_load() {
        let ls = super::LoadSettingsBox {
            preload_start_time: 100,
            preload_duration: 5000,
            preload_flags: 1,
            default_hints: 0,
        };
        let mut tkhd = vec![0u8; 92]; // FullBox v0 + 84 bytes
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes()); // track_id = 1

        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // timescale

        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 8]);
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"\0");

        let mut mdia = Vec::new();
        mdia.extend(wrap_box_full_size(b"mdhd", &mdhd));
        mdia.extend(wrap_box_full_size(b"hdlr", &hdlr));

        let mut trak = Vec::new();
        trak.extend(wrap_box_full_size(b"tkhd", &tkhd));
        trak.extend(wrap_box_full_size(b"mdia", &mdia));
        // `load` is a plain Box: its body is the 16 numeric bytes.
        trak.extend_from_slice(&super::build_load_settings_box(&ls));

        let t = super::parse_trak(&trak).unwrap().unwrap();
        assert_eq!(t.load_settings, Some(ls));
    }

    /// Surfacing: a parsed `load` exposes its preload / hint fields and the
    /// decoded flag booleans on `params.options`.
    #[test]
    fn build_stream_info_surfaces_load_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.load_settings = Some(super::LoadSettingsBox {
            preload_start_time: 0,
            preload_duration: -1,
            preload_flags: 1,
            default_hints: 0x0100,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("load_preload_duration"), Some("-1"));
        assert_eq!(info.params.options.get("load_preload_flags"), Some("1"));
        assert_eq!(info.params.options.get("load_preload_always"), Some("true"));
        assert_eq!(
            info.params.options.get("load_preload_if_enabled"),
            Some("false")
        );
        assert_eq!(info.params.options.get("load_high_quality"), Some("true"));
        assert_eq!(info.params.options.get("load_double_buffer"), Some("false"));
    }

    /// Surfacing: a parsed `gmin` exposes `gmin_graphicsmode` (decimal),
    /// `gmin_opcolor` (three space-separated decimal components), and
    /// `gmin_balance` (signed decimal) on `params.options`.
    #[test]
    fn build_stream_info_surfaces_gmin_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Subtitle;
        t.codec_id_fourcc = *b"text";
        t.timescale = 1000;
        t.gmin = Some(super::GminBox {
            graphicsmode: 64,
            opcolor: [1, 2, 3],
            balance: -256,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("gmin_graphicsmode"), Some("64"));
        assert_eq!(info.params.options.get("gmin_opcolor"), Some("1 2 3"));
        assert_eq!(info.params.options.get("gmin_balance"), Some("-256"));
    }

    /// Absent `gmin`, none of the `gmin_*` keys are emitted.
    #[test]
    fn build_stream_info_no_gmin_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("gmin_graphicsmode"), None);
        assert_eq!(info.params.options.get("gmin_opcolor"), None);
        assert_eq!(info.params.options.get("gmin_balance"), None);
    }

    /// Build an 8-byte `amve` body (AmbientViewingEnvironmentBox):
    /// `ambient_illuminance` u32 + `ambient_light_x` u16 +
    /// `ambient_light_y` u16, all big-endian.
    fn amve_body(illum: u32, x: u16, y: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&illum.to_be_bytes());
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out
    }

    /// Build a 78-byte `VisualSampleEntry` preamble with `width` /
    /// `height` set, followed by any child boxes already in `children`.
    fn video_sample_entry_body(width: u16, height: u16, children: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 6]); // reserved
        out.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        out.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution 72dpi
        out.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution 72dpi
        out.extend_from_slice(&[0u8; 4]); // reserved
        out.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        out.extend_from_slice(&[0u8; 32]); // compressorname
        out.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        out.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
        debug_assert_eq!(out.len(), 78);
        out.extend_from_slice(children);
        out
    }

    /// `parse_amve` decodes the BT.2035 reference-environment worked
    /// example (10 lux, D65 chromaticity) field-for-field.
    #[test]
    fn parse_amve_bt2035_example() {
        // 10 lux → 100000 (0.0001 lux/unit); x = 0.3127 → 15635; y =
        // 0.3290 → 16450 (0.00002/unit).
        let body = amve_body(100_000, 15_635, 16_450);
        let rec = super::parse_amve(&body).expect("amve should parse");
        assert_eq!(rec.ambient_illuminance, 100_000);
        assert_eq!(rec.ambient_light_x, 15_635);
        assert_eq!(rec.ambient_light_y, 16_450);
    }

    /// `parse_amve` ignores bytes past the fixed 8-byte body (reserved for
    /// a future edition) rather than failing.
    #[test]
    fn parse_amve_tolerates_trailing_bytes() {
        let mut body = amve_body(7, 1, 2);
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let rec = super::parse_amve(&body).expect("amve should parse");
        assert_eq!(rec.ambient_illuminance, 7);
        assert_eq!(rec.ambient_light_x, 1);
        assert_eq!(rec.ambient_light_y, 2);
    }

    /// A body shorter than the fixed 8-byte layout is rejected.
    #[test]
    fn parse_amve_rejects_short_body() {
        assert!(super::parse_amve(&[0u8; 7]).is_none());
        assert!(super::parse_amve(&[]).is_none());
        // The public entry returns Err for the same case.
        assert!(super::parse_amve_box(&[0u8; 7]).is_err());
    }

    /// The public `parse_amve_box` entry decodes a well-formed body.
    #[test]
    fn parse_amve_box_public_entry() {
        let body = amve_body(50_000, 25_000, 30_000);
        let rec = super::parse_amve_box(&body).expect("amve should parse");
        assert_eq!(rec.ambient_illuminance, 50_000);
        assert_eq!(rec.ambient_light_x, 25_000);
        assert_eq!(rec.ambient_light_y, 30_000);
    }

    /// `parse_video_sample_entry` lands an `amve` child box on the track.
    #[test]
    fn parse_video_sample_entry_picks_up_amve() {
        let amve = wrap_box_full_size(b"amve", &amve_body(100_000, 15_635, 16_450));
        let entry = video_sample_entry_body(1920, 1080, &amve);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();

        assert_eq!(t.width, Some(1920));
        assert_eq!(t.height, Some(1080));
        let a = t.amve.expect("amve should be parsed");
        assert_eq!(a.ambient_illuminance, 100_000);
        assert_eq!(a.ambient_light_x, 15_635);
        assert_eq!(a.ambient_light_y, 16_450);
    }

    /// First `amve` wins when two sample-entry child boxes carry one.
    #[test]
    fn parse_video_sample_entry_amve_first_wins() {
        let mut children = wrap_box_full_size(b"amve", &amve_body(1, 2, 3));
        children.extend(wrap_box_full_size(b"amve", &amve_body(9, 9, 9)));
        let entry = video_sample_entry_body(640, 480, &children);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();
        let a = t.amve.expect("amve should be parsed");
        assert_eq!(a.ambient_illuminance, 1);
        assert_eq!(a.ambient_light_x, 2);
        assert_eq!(a.ambient_light_y, 3);
    }

    /// A malformed (too-short) `amve` child box is dropped silently — the
    /// sample entry still parses (width/height land) with no `amve`.
    #[test]
    fn parse_video_sample_entry_drops_malformed_amve() {
        let amve = wrap_box_full_size(b"amve", &[0u8; 4]); // too short
        let entry = video_sample_entry_body(1280, 720, &amve);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();
        assert_eq!(t.width, Some(1280));
        assert!(t.amve.is_none());
    }

    /// Surfacing: a parsed `amve` exposes the three raw fields (decimal)
    /// on `params.options`.
    #[test]
    fn build_stream_info_surfaces_amve_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"hvc1";
        t.timescale = 1000;
        t.amve = Some(super::AmveRecord {
            ambient_illuminance: 100_000,
            ambient_light_x: 15_635,
            ambient_light_y: 16_450,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("amve_ambient_illuminance"),
            Some("100000")
        );
        assert_eq!(
            info.params.options.get("amve_ambient_light_x"),
            Some("15635")
        );
        assert_eq!(
            info.params.options.get("amve_ambient_light_y"),
            Some("16450")
        );
    }

    /// Absence: a track with no `amve` emits none of the `amve_*` keys.
    #[test]
    fn build_stream_info_no_amve_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("amve_ambient_illuminance"), None);
        assert_eq!(info.params.options.get("amve_ambient_light_x"), None);
        assert_eq!(info.params.options.get("amve_ambient_light_y"), None);
    }

    /// Build a 12-byte `btrt` body (BitRateBox): `bufferSizeDB` +
    /// `maxBitrate` + `avgBitrate`, all big-endian u32.
    fn btrt_body(buf: u32, max: u32, avg: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&buf.to_be_bytes());
        out.extend_from_slice(&max.to_be_bytes());
        out.extend_from_slice(&avg.to_be_bytes());
        out
    }

    /// `parse_btrt` decodes the three big-endian u32 fields field-for-field.
    #[test]
    fn parse_btrt_three_u32_fields() {
        let body = btrt_body(65_536, 5_000_000, 3_200_000);
        let rec = super::parse_btrt(&body).expect("btrt should parse");
        assert_eq!(rec.buffer_size_db, 65_536);
        assert_eq!(rec.max_bitrate, 5_000_000);
        assert_eq!(rec.avg_bitrate, 3_200_000);
    }

    /// `parse_btrt` ignores bytes past the fixed 12-byte body rather than
    /// failing, matching the `parse_amve` posture.
    #[test]
    fn parse_btrt_tolerates_trailing_bytes() {
        let mut body = btrt_body(1, 2, 3);
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let rec = super::parse_btrt(&body).expect("btrt should parse");
        assert_eq!(rec.buffer_size_db, 1);
        assert_eq!(rec.max_bitrate, 2);
        assert_eq!(rec.avg_bitrate, 3);
    }

    /// A body shorter than the fixed 12 bytes is rejected; the public
    /// entry returns `Err` for the same input.
    #[test]
    fn parse_btrt_rejects_short_body() {
        assert!(super::parse_btrt(&[0u8; 11]).is_none());
        assert!(super::parse_btrt(&[]).is_none());
        assert!(super::parse_btrt_box(&[0u8; 11]).is_err());
    }

    /// The public `parse_btrt_box` entry decodes a well-formed body.
    #[test]
    fn parse_btrt_box_public_entry() {
        let body = btrt_body(8_192, 1_500_000, 900_000);
        let rec = super::parse_btrt_box(&body).expect("btrt should parse");
        assert_eq!(rec.buffer_size_db, 8_192);
        assert_eq!(rec.max_bitrate, 1_500_000);
        assert_eq!(rec.avg_bitrate, 900_000);
    }

    /// `parse_video_sample_entry` lands a `btrt` child box on the track.
    #[test]
    fn parse_video_sample_entry_picks_up_btrt() {
        let btrt = wrap_box_full_size(b"btrt", &btrt_body(131_072, 8_000_000, 6_000_000));
        let entry = video_sample_entry_body(3840, 2160, &btrt);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();

        assert_eq!(t.width, Some(3840));
        let b = t.btrt.expect("btrt should be parsed");
        assert_eq!(b.buffer_size_db, 131_072);
        assert_eq!(b.max_bitrate, 8_000_000);
        assert_eq!(b.avg_bitrate, 6_000_000);
    }

    fn pasp_body(h: u32, v: u32) -> Vec<u8> {
        let mut b = h.to_be_bytes().to_vec();
        b.extend_from_slice(&v.to_be_bytes());
        b
    }

    fn clap_body(vals: [u32; 8]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in vals {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    }

    #[test]
    fn parse_pasp_clap_colr_bodies() {
        let p = super::parse_pasp(&pasp_body(16, 15)).unwrap();
        assert_eq!(p.h_spacing, 16);
        assert_eq!(p.v_spacing, 15);
        assert!(super::parse_pasp(&[0u8; 4]).is_none());

        let c = super::parse_clap(&clap_body([1920, 1, 1080, 1, 0, 1, 0, 1])).unwrap();
        assert_eq!(c.width_n, 1920);
        assert_eq!(c.height_n, 1080);
        assert_eq!(c.horiz_off_d, 1);
        assert!(super::parse_clap(&[0u8; 16]).is_none());

        // nclx: BT.709 primaries(1)/transfer(1)/matrix(1), full-range set.
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.push(0x80); // full_range_flag=1
        match super::parse_colr(&nclx).unwrap() {
            super::ColrRecord::Nclx {
                colour_primaries,
                transfer_characteristics,
                matrix_coefficients,
                full_range,
            } => {
                assert_eq!(colour_primaries, 1);
                assert_eq!(transfer_characteristics, 1);
                assert_eq!(matrix_coefficients, 1);
                assert!(full_range);
            }
            other => panic!("expected nclx, got {other:?}"),
        }

        // prof: an ICC profile (raw bytes preserved).
        let mut prof = b"prof".to_vec();
        prof.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        match super::parse_colr(&prof).unwrap() {
            super::ColrRecord::UnrestrictedIcc(d) => {
                assert_eq!(d, vec![0xDE, 0xAD, 0xBE, 0xEF])
            }
            other => panic!("expected prof, got {other:?}"),
        }

        // unknown colour type → Other.
        let mut other = b"xxxx".to_vec();
        other.push(9);
        match super::parse_colr(&other).unwrap() {
            super::ColrRecord::Other { colour_type, data } => {
                assert_eq!(&colour_type, b"xxxx");
                assert_eq!(data, vec![9]);
            }
            o => panic!("expected Other, got {o:?}"),
        }
        assert!(super::parse_colr(&[1, 2, 3]).is_none());
    }

    #[test]
    fn build_pasp_clap_colr_round_trip() {
        let p = super::PaspRecord {
            h_spacing: 64,
            v_spacing: 45,
        };
        let built = super::build_pasp_box(&p);
        assert_eq!(super::parse_pasp_box(&built[8..]).unwrap(), p);

        let c = super::ClapRecord {
            width_n: 1920,
            width_d: 1,
            height_n: 1080,
            height_d: 1,
            horiz_off_n: 3,
            horiz_off_d: 2,
            vert_off_n: 5,
            vert_off_d: 4,
        };
        let built = super::build_clap_box(&c);
        assert_eq!(super::parse_clap_box(&built[8..]).unwrap(), c);

        for colr in [
            super::ColrRecord::Nclx {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                full_range: true,
            },
            super::ColrRecord::UnrestrictedIcc(vec![1, 2, 3, 4]),
            super::ColrRecord::RestrictedIcc(vec![9, 8, 7]),
            super::ColrRecord::Other {
                colour_type: *b"zzzz",
                data: vec![0, 1, 2],
            },
        ] {
            let built = super::build_colr_box(&colr);
            assert_eq!(super::parse_colr_box(&built[8..]).unwrap(), colr);
        }
    }

    #[test]
    fn parse_video_sample_entry_picks_up_pasp_clap_colr() {
        let mut children = wrap_box_full_size(b"pasp", &pasp_body(40, 33));
        children.extend(wrap_box_full_size(
            b"clap",
            &clap_body([1280, 1, 720, 1, 0, 1, 0, 1]),
        ));
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&9u16.to_be_bytes()); // primaries BT.2020
        nclx.extend_from_slice(&16u16.to_be_bytes()); // transfer PQ
        nclx.extend_from_slice(&9u16.to_be_bytes()); // matrix BT.2020-NC
        nclx.push(0x00); // full_range = 0
        children.extend(wrap_box_full_size(b"colr", &nclx));
        let entry = video_sample_entry_body(1280, 720, &children);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();

        assert_eq!(t.pasp.unwrap().h_spacing, 40);
        assert_eq!(t.clap.unwrap().width_n, 1280);
        match t.colr.unwrap() {
            super::ColrRecord::Nclx {
                colour_primaries,
                full_range,
                ..
            } => {
                assert_eq!(colour_primaries, 9);
                assert!(!full_range);
            }
            o => panic!("expected nclx, got {o:?}"),
        }
    }

    #[test]
    fn build_stream_info_surfaces_pasp_clap_colr_options() {
        let mut children = wrap_box_full_size(b"pasp", &pasp_body(64, 45));
        children.extend(wrap_box_full_size(
            b"clap",
            &clap_body([1920, 1, 1080, 1, 0, 1, 0, 1]),
        ));
        let mut nclx = b"nclx".to_vec();
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.extend_from_slice(&1u16.to_be_bytes());
        nclx.push(0x80);
        children.extend(wrap_box_full_size(b"colr", &nclx));
        let entry = video_sample_entry_body(1920, 1080, &children);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        super::parse_video_sample_entry(&entry, &mut t).unwrap();
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        let o = &info.params.options;
        assert_eq!(o.get("pasp_h_spacing"), Some("64"));
        assert_eq!(o.get("pasp_v_spacing"), Some("45"));
        assert_eq!(o.get("clap_width"), Some("1920/1"));
        assert_eq!(o.get("clap_horiz_off"), Some("0/1"));
        assert_eq!(o.get("colr_type"), Some("nclx"));
        assert_eq!(o.get("colr_primaries"), Some("1"));
        assert_eq!(o.get("colr_full_range"), Some("1"));
    }

    /// First `btrt` wins when two sample-entry child boxes carry one.
    #[test]
    fn parse_video_sample_entry_btrt_first_wins() {
        let mut children = wrap_box_full_size(b"btrt", &btrt_body(1, 2, 3));
        children.extend(wrap_box_full_size(b"btrt", &btrt_body(9, 9, 9)));
        let entry = video_sample_entry_body(640, 480, &children);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();
        let b = t.btrt.expect("btrt should be parsed");
        assert_eq!(b.buffer_size_db, 1);
        assert_eq!(b.max_bitrate, 2);
        assert_eq!(b.avg_bitrate, 3);
    }

    /// A malformed (too-short) `btrt` child box is dropped silently — the
    /// sample entry still parses (width/height land) with no `btrt`.
    #[test]
    fn parse_video_sample_entry_drops_malformed_btrt() {
        let btrt = wrap_box_full_size(b"btrt", &[0u8; 8]); // too short
        let entry = video_sample_entry_body(1280, 720, &btrt);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();
        assert_eq!(t.width, Some(1280));
        assert!(t.btrt.is_none());
    }

    /// `parse_audio_sample_entry` also lands a `btrt` (the box is
    /// codec-agnostic and may follow any sample entry, §8.5.2).
    #[test]
    fn parse_audio_sample_entry_picks_up_btrt() {
        let btrt = wrap_box_full_size(b"btrt", &btrt_body(4_096, 256_000, 192_000));
        let mut entry = audio_sample_entry_body(2, 1);
        entry.extend_from_slice(&btrt);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Audio;
        super::parse_audio_sample_entry(&entry, &mut t).unwrap();

        assert_eq!(t.channels, Some(2));
        let b = t.btrt.expect("btrt should be parsed");
        assert_eq!(b.buffer_size_db, 4_096);
        assert_eq!(b.max_bitrate, 256_000);
        assert_eq!(b.avg_bitrate, 192_000);
    }

    /// Surfacing: a parsed `btrt` exposes the three fields (decimal) on
    /// `params.options`.
    #[test]
    fn build_stream_info_surfaces_btrt_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.btrt = Some(super::BtrtRecord {
            buffer_size_db: 131_072,
            max_bitrate: 8_000_000,
            avg_bitrate: 6_000_000,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("btrt_buffer_size_db"),
            Some("131072")
        );
        assert_eq!(info.params.options.get("btrt_max_bitrate"), Some("8000000"));
        assert_eq!(info.params.options.get("btrt_avg_bitrate"), Some("6000000"));
    }

    /// Absence: a track with no `btrt` emits none of the `btrt_*` keys.
    #[test]
    fn build_stream_info_no_btrt_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("btrt_buffer_size_db"), None);
        assert_eq!(info.params.options.get("btrt_max_bitrate"), None);
        assert_eq!(info.params.options.get("btrt_avg_bitrate"), None);
    }

    /// Build an `stvi` (StereoVideoBox) body, FullBox-preamble-relative:
    /// `int(30) reserved | int(2) single_view_allowed` (one big-endian
    /// u32 word) + `int(32) stereo_scheme` + `int(32) length` +
    /// `int(8)[length] stereo_indication_type`. The 4-byte FullBox
    /// version/flags preamble is prepended (version 0, flags 0).
    fn stvi_body(single_view_allowed: u8, stereo_scheme: u32, indication: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        // FullBox version 0, flags 0.
        out.extend_from_slice(&[0u8; 4]);
        // First word: reserved(30) high bits zero, single_view_allowed(2)
        // low bits.
        let word = (single_view_allowed as u32) & 0x3;
        out.extend_from_slice(&word.to_be_bytes());
        out.extend_from_slice(&stereo_scheme.to_be_bytes());
        out.extend_from_slice(&(indication.len() as u32).to_be_bytes());
        out.extend_from_slice(indication);
        out
    }

    /// `parse_stvi` decodes a scheme-1 (14496-10 frame-packing) box: a
    /// 4-byte `stereo_indication_type` carrying a u32 arrangement type.
    #[test]
    fn parse_stvi_scheme1_frame_packing() {
        // single_view_allowed = 3 (both views OK on monoscopic);
        // stereo_scheme = 1; indication = side-by-side type (3) as u32.
        let body = stvi_body(3, 1, &3u32.to_be_bytes());
        let rec = super::parse_stvi(&body).expect("stvi should parse");
        assert_eq!(rec.single_view_allowed, 3);
        assert_eq!(rec.stereo_scheme, 1);
        assert_eq!(rec.stereo_indication_type, vec![0, 0, 0, 3]);
        assert!(rec.right_view_monoscopic_allowed());
        assert!(rec.left_view_monoscopic_allowed());
    }

    /// `parse_stvi` masks the 30 reserved high bits off the first word so
    /// a producer slip on them does not corrupt `single_view_allowed`.
    #[test]
    fn parse_stvi_masks_reserved_bits() {
        // scheme 3, 2-byte indication. The helper wrote 0x0000_0000 in
        // the first word (bytes 4..8) for single_view_allowed = 0; set
        // some reserved high bits there to prove they are masked off.
        let mut body = stvi_body(0, 3, &[0x01, 0x00]);
        body[4] = 0xFF;
        body[5] = 0xFF;
        body[6] = 0xFF;
        body[7] = 0xFE; // low 2 bits = 0b10 → single_view_allowed = 2
        let rec = super::parse_stvi(&body).expect("stvi should parse");
        assert_eq!(rec.single_view_allowed, 2);
        assert!(!rec.right_view_monoscopic_allowed());
        assert!(rec.left_view_monoscopic_allowed());
        assert_eq!(rec.stereo_scheme, 3);
        assert_eq!(rec.stereo_indication_type, vec![0x01, 0x00]);
    }

    /// `parse_stvi` ignores trailing optional `any_box` bytes after the
    /// `stereo_indication_type` array.
    #[test]
    fn parse_stvi_ignores_trailing_any_box() {
        let mut body = stvi_body(1, 1, &4u32.to_be_bytes());
        // Append a fake trailing box.
        body.extend_from_slice(&8u32.to_be_bytes());
        body.extend_from_slice(b"free");
        let rec = super::parse_stvi(&body).expect("stvi should parse");
        assert_eq!(rec.single_view_allowed, 1);
        assert_eq!(rec.stereo_indication_type, vec![0, 0, 0, 4]);
    }

    /// A `length` that overruns the bytes present is rejected rather than
    /// reading past the end or into trailing box content.
    #[test]
    fn parse_stvi_rejects_overrunning_length() {
        let mut body = stvi_body(0, 1, &[]);
        // Body now has length = 0; rewrite the length field (bytes 12..16)
        // to claim 16 indication bytes that are not present.
        body[12..16].copy_from_slice(&16u32.to_be_bytes());
        assert!(super::parse_stvi(&body).is_none());
    }

    /// A body too short to hold the fixed header is rejected; the public
    /// entry returns `Err` for the same input.
    #[test]
    fn parse_stvi_rejects_short_body() {
        assert!(super::parse_stvi(&[0u8; 15]).is_none());
        assert!(super::parse_stvi_box(&[0u8; 15]).is_err());
    }

    /// The public `parse_stvi_box` entry decodes a well-formed body.
    #[test]
    fn parse_stvi_box_public_entry() {
        let body = stvi_body(2, 2, &7u32.to_be_bytes());
        let rec = super::parse_stvi_box(&body).expect("stvi should parse");
        assert_eq!(rec.single_view_allowed, 2);
        assert_eq!(rec.stereo_scheme, 2);
        assert_eq!(rec.stereo_indication_type, vec![0, 0, 0, 7]);
    }

    /// An empty `stereo_indication_type` (length 0) parses with an empty
    /// byte vec.
    #[test]
    fn parse_stvi_empty_indication() {
        let body = stvi_body(0, 99, &[]);
        let rec = super::parse_stvi(&body).expect("stvi should parse");
        assert_eq!(rec.stereo_scheme, 99);
        assert!(rec.stereo_indication_type.is_empty());
    }

    /// `walk_schi` recovers an `stvi` child from a `schi` body.
    #[test]
    fn walk_schi_picks_up_stvi() {
        let stvi = wrap_box_full_size(b"stvi", &stvi_body(1, 1, &3u32.to_be_bytes()));
        let children = super::walk_schi(&stvi);
        let s = children.stvi.expect("stvi should be parsed");
        assert_eq!(s.single_view_allowed, 1);
        assert_eq!(s.stereo_scheme, 1);
        assert!(children.tenc.is_none());
    }

    /// A non-encrypted restricted-scheme video sample entry carries its
    /// `sinf` directly as a child box: `parse_video_sample_entry`
    /// descends it into `schi` and lands the `stvi` on the track.
    #[test]
    fn parse_video_sample_entry_picks_up_stvi_via_sinf() {
        let stvi = wrap_box_full_size(b"stvi", &stvi_body(3, 1, &3u32.to_be_bytes()));
        let schi = wrap_box_full_size(b"schi", &stvi);
        let sinf = wrap_box_full_size(b"sinf", &schi);
        let entry = video_sample_entry_body(1920, 1080, &sinf);

        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        super::parse_video_sample_entry(&entry, &mut t).unwrap();

        assert_eq!(t.width, Some(1920));
        let s = t.stvi.expect("stvi should be parsed");
        assert_eq!(s.single_view_allowed, 3);
        assert_eq!(s.stereo_scheme, 1);
        assert_eq!(s.stereo_indication_type, vec![0, 0, 0, 3]);
    }

    /// Surfacing: a parsed `stvi` exposes its fields on `params.options`,
    /// with the indication bytes as lowercase hex.
    #[test]
    fn build_stream_info_surfaces_stvi_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.stvi = Some(super::StviRecord {
            single_view_allowed: 3,
            stereo_scheme: 1,
            stereo_indication_type: vec![0x00, 0x00, 0x00, 0x03],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("stvi_single_view_allowed"),
            Some("3")
        );
        assert_eq!(info.params.options.get("stvi_stereo_scheme"), Some("1"));
        assert_eq!(info.params.options.get("stvi_indication"), Some("00000003"));
    }

    /// An empty indication omits the `stvi_indication` key (the two
    /// fixed-field keys still emit).
    #[test]
    fn build_stream_info_stvi_empty_indication_omits_hex() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.stvi = Some(super::StviRecord {
            single_view_allowed: 0,
            stereo_scheme: 2,
            stereo_indication_type: Vec::new(),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("stvi_single_view_allowed"),
            Some("0")
        );
        assert_eq!(info.params.options.get("stvi_stereo_scheme"), Some("2"));
        assert_eq!(info.params.options.get("stvi_indication"), None);
    }

    #[test]
    fn build_stream_info_surfaces_rtp_hint_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"rtp ";
        t.timescale = 90_000;
        t.rtp_hint = Some(crate::hint::RtpHintSampleEntry {
            format: *b"rtp ",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1450,
            timescale: Some(90_000),
            time_offset: Some(-100),
            sequence_offset: Some(42),
            srpp: None,
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("rtp_hint_format"), Some("rtp "));
        assert_eq!(
            info.params.options.get("rtp_hint_max_packet_size"),
            Some("1450")
        );
        assert_eq!(info.params.options.get("rtp_hint_timescale"), Some("90000"));
        assert_eq!(
            info.params.options.get("rtp_hint_time_offset"),
            Some("-100")
        );
        assert_eq!(
            info.params.options.get("rtp_hint_sequence_offset"),
            Some("42")
        );
        assert_eq!(info.params.options.get("rtp_hint_srtp"), None);
    }

    #[test]
    fn build_stream_info_srtp_hint_marks_srtp() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"srtp";
        t.timescale = 90_000;
        t.rtp_hint = Some(crate::hint::RtpHintSampleEntry {
            format: *b"srtp",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1400,
            timescale: Some(90_000),
            time_offset: None,
            sequence_offset: None,
            srpp: Some(crate::hint::SrppBox {
                version: 0,
                encryption_algorithm_rtp: 0x2020_2020,
                encryption_algorithm_rtcp: 0x2020_2020,
                integrity_algorithm_rtp: 0x2020_2020,
                integrity_algorithm_rtcp: 0x2020_2020,
                scheme_bytes: vec![],
            }),
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("rtp_hint_format"), Some("srtp"));
        assert_eq!(info.params.options.get("rtp_hint_srtp"), Some("true"));
        // tsro/snro absent → keys omitted.
        assert_eq!(info.params.options.get("rtp_hint_time_offset"), None);
        assert_eq!(info.params.options.get("rtp_hint_sequence_offset"), None);
    }

    #[test]
    fn build_stream_info_no_rtp_hint_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("rtp_hint_format"), None);
        assert_eq!(info.params.options.get("rtp_hint_max_packet_size"), None);
    }

    #[test]
    fn build_stream_info_surfaces_mpeg2ts_hint_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"sm2t";
        t.timescale = 90_000;
        t.mpeg2ts_hint = Some(crate::hint::Mpeg2TsHintSampleEntry {
            format: *b"sm2t",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            preceding_bytes_len: 4,
            trailing_bytes_len: 16,
            precomputed_only: true,
            additional_data: vec![],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("m2t_hint_format"), Some("sm2t"));
        assert_eq!(
            info.params.options.get("m2t_hint_preceding_bytes"),
            Some("4")
        );
        assert_eq!(
            info.params.options.get("m2t_hint_trailing_bytes"),
            Some("16")
        );
        assert_eq!(
            info.params.options.get("m2t_hint_precomputed"),
            Some("true")
        );
    }

    #[test]
    fn build_stream_info_surfaces_hinf_on_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"rtp ";
        t.timescale = 90_000;
        t.hint_stats = crate::hint::HintStatistics {
            bytes_sent_with_rtp_64: Some(1_000_000),
            packets_sent_64: Some(5000),
            bytes_sent_no_rtp_64: Some(940_000),
            media_bytes_sent: Some(800_000),
            largest_packet: Some(1452),
            max_rates: vec![
                crate::hint::MaxRate {
                    period: 1000,
                    bytes: 64_000,
                },
                crate::hint::MaxRate {
                    period: 2000,
                    bytes: 120_000,
                },
            ],
            payload_ids: vec![crate::hint::PayloadId {
                payload_id: 96,
                rtpmap: "H264/90000".to_string(),
            }],
            ..crate::hint::HintStatistics::default()
        };
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("hinf_bytes_sent"), Some("1000000"));
        assert_eq!(info.params.options.get("hinf_packets_sent"), Some("5000"));
        assert_eq!(
            info.params.options.get("hinf_payload_bytes"),
            Some("940000")
        );
        assert_eq!(info.params.options.get("hinf_media_bytes"), Some("800000"));
        assert_eq!(info.params.options.get("hinf_largest_packet"), Some("1452"));
        assert_eq!(info.params.options.get("hinf_maxr_count"), Some("2"));
        assert_eq!(info.params.options.get("hinf_payload_count"), Some("1"));
        // Absent sub-boxes omit their keys.
        assert_eq!(info.params.options.get("hinf_immediate_bytes"), None);
    }

    #[test]
    fn build_stream_info_no_hinf_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("hinf_bytes_sent"), None);
        assert_eq!(info.params.options.get("hinf_maxr_count"), None);
    }

    #[test]
    fn build_stream_info_mpeg2ts_hint_no_precomputed_omits_key() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Data;
        t.codec_id_fourcc = *b"rm2t";
        t.timescale = 90_000;
        t.mpeg2ts_hint = Some(crate::hint::Mpeg2TsHintSampleEntry {
            format: *b"rm2t",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            preceding_bytes_len: 0,
            trailing_bytes_len: 0,
            precomputed_only: false,
            additional_data: vec![],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("m2t_hint_format"), Some("rm2t"));
        assert_eq!(info.params.options.get("m2t_hint_precomputed"), None);
    }

    /// Absence: a track with no `stvi` emits none of the `stvi_*` keys.
    #[test]
    fn build_stream_info_no_stvi_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("stvi_single_view_allowed"), None);
        assert_eq!(info.params.options.get("stvi_stereo_scheme"), None);
        assert_eq!(info.params.options.get("stvi_indication"), None);
    }

    /// Build a `dref` body: FullBox(0,0) preamble + entry_count + the
    /// concatenated child boxes.
    fn build_dref(children: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]); // version 0, flags 0
        body.extend_from_slice(&(children.len() as u32).to_be_bytes());
        for c in children {
            body.extend_from_slice(c);
        }
        body
    }

    /// A self-contained `url ` entry: 4-byte FullBox preamble with the
    /// low flag bit set, no string body.
    fn url_self_contained() -> Vec<u8> {
        wrap_box_full_size(b"url ", &[0u8, 0, 0, 1])
    }

    /// An external `url ` entry: flags = 0, then a NULL-terminated URL.
    fn url_external(loc: &str) -> Vec<u8> {
        let mut payload = vec![0u8; 4]; // version 0, flags 0
        payload.extend_from_slice(loc.as_bytes());
        payload.push(0);
        wrap_box_full_size(b"url ", &payload)
    }

    /// §8.7.2.2: the canonical single self-contained `url ` entry — the
    /// overwhelmingly common shape. Parses to one entry, self-contained,
    /// no location string.
    #[test]
    fn parse_dref_single_self_contained_url() {
        let body = build_dref(&[url_self_contained()]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 1);
        assert_eq!(&d.entries[0].kind, b"url ");
        assert!(d.entries[0].is_self_contained());
        assert_eq!(d.entries[0].location, None);
        assert_eq!(d.entries[0].name, None);
    }

    /// §8.7.2.3: a `url ` entry whose self-contained bit is clear carries
    /// a NULL-terminated `location` URL naming the external resource.
    #[test]
    fn parse_dref_external_url_location() {
        let body = build_dref(&[url_external("http://example.com/a.mp4")]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 1);
        assert!(!d.entries[0].is_self_contained());
        assert_eq!(
            d.entries[0].location.as_deref(),
            Some("http://example.com/a.mp4")
        );
    }

    /// §8.7.2.2: a `urn ` entry carries a NULL-terminated `name` (the URN)
    /// followed by an optional NULL-terminated `location`.
    #[test]
    fn parse_dref_urn_name_and_location() {
        let mut payload = vec![0u8; 4]; // version 0, flags 0
        payload.extend_from_slice(b"urn:example:res");
        payload.push(0);
        payload.extend_from_slice(b"ftp://host/res");
        payload.push(0);
        let body = build_dref(&[wrap_box_full_size(b"urn ", &payload)]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 1);
        assert_eq!(&d.entries[0].kind, b"urn ");
        assert_eq!(d.entries[0].name.as_deref(), Some("urn:example:res"));
        assert_eq!(d.entries[0].location.as_deref(), Some("ftp://host/res"));
    }

    /// §8.7.2.3: `location` is optional for a `urn ` — a name with no
    /// trailing location string parses with `location == None`.
    #[test]
    fn parse_dref_urn_name_only() {
        let mut payload = vec![0u8; 4];
        payload.extend_from_slice(b"urn:example:res");
        payload.push(0);
        let body = build_dref(&[wrap_box_full_size(b"urn ", &payload)]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries[0].name.as_deref(), Some("urn:example:res"));
        assert_eq!(d.entries[0].location, None);
    }

    /// §8.7.2: a split-source track — two entries, the first
    /// self-contained, the second external. The 1-based entry order is
    /// preserved (it aligns with `data_reference_index`).
    #[test]
    fn parse_dref_multiple_entries_order_preserved() {
        let body = build_dref(&[url_self_contained(), url_external("rel/path.bin")]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 2);
        assert!(d.entries[0].is_self_contained());
        assert!(!d.entries[1].is_self_contained());
        assert_eq!(d.entries[1].location.as_deref(), Some("rel/path.bin"));
    }

    /// §8.7.2.1: a non-`url `/`urn ` child is non-conforming and is
    /// dropped (no bogus entry inserted), but conforming siblings around
    /// it still parse.
    #[test]
    fn parse_dref_drops_unknown_child() {
        let bogus = wrap_box_full_size(b"foo ", &[0u8; 4]);
        let body = build_dref(&[bogus, url_self_contained()]);
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 1);
        assert!(d.entries[0].is_self_contained());
    }

    /// A `dref` body shorter than the 8-byte FullBox + entry_count floor
    /// is rejected.
    #[test]
    fn parse_dref_too_short_is_rejected() {
        assert!(super::parse_dref(&[0u8; 7]).is_err());
    }

    /// A forged `entry_count` far larger than the bytes present does not
    /// over-allocate and does not invent entries — parsing stops at the
    /// real data.
    #[test]
    fn parse_dref_forged_count_does_not_overrun() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // forged count
        body.extend_from_slice(&url_self_contained());
        let d = super::parse_dref(&body).unwrap();
        assert_eq!(d.entries.len(), 1);
    }

    /// `parse_minf` walks `dinf` → `dref` and lands the table on the
    /// track.
    #[test]
    fn parse_minf_picks_up_dref_via_dinf() {
        let dref = build_dref(&[url_external("http://h/v.mp4")]);
        let dref_box = wrap_box_full_size(b"dref", &dref);
        let dinf = wrap_box_full_size(b"dinf", &dref_box);

        let mut t = fresh_track();
        super::parse_minf(&dinf, &mut t).unwrap();
        let d = t.dref.expect("dref should be parsed");
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[0].location.as_deref(), Some("http://h/v.mp4"));
    }

    /// Surfacing: a single self-contained `url ` (the common case)
    /// collapses to `dref_self_contained = "true"` with no per-entry
    /// or count keys.
    #[test]
    fn build_stream_info_surfaces_single_self_contained_dref() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.dref = Some(super::DrefBox {
            entries: vec![super::DataEntry {
                kind: *b"url ",
                flags: 1,
                name: None,
                location: None,
            }],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("dref_self_contained"), Some("true"));
        assert_eq!(info.params.options.get("dref_count"), None);
        assert_eq!(info.params.options.get("dref_1"), None);
    }

    /// Surfacing: an external split-source `dref` emits `dref_count`,
    /// `dref_self_contained = "false"`, and a `dref_<n>` per entry.
    #[test]
    fn build_stream_info_surfaces_external_dref_entries() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        t.dref = Some(super::DrefBox {
            entries: vec![
                super::DataEntry {
                    kind: *b"url ",
                    flags: 1,
                    name: None,
                    location: None,
                },
                super::DataEntry {
                    kind: *b"urn ",
                    flags: 0,
                    name: Some("urn:x".to_string()),
                    location: Some("http://h/r".to_string()),
                },
            ],
        });
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(
            info.params.options.get("dref_self_contained"),
            Some("false")
        );
        assert_eq!(info.params.options.get("dref_count"), Some("2"));
        assert_eq!(info.params.options.get("dref_1"), Some("url  self=true"));
        assert_eq!(
            info.params.options.get("dref_2"),
            Some("urn  self=false name=urn:x loc=http://h/r")
        );
    }

    /// Absence: a track with no `dref` emits none of the `dref_*` keys.
    #[test]
    fn build_stream_info_no_dref_no_options() {
        let mut t = fresh_track();
        t.media_type = oxideav_core::MediaType::Video;
        t.codec_id_fourcc = *b"avc1";
        t.timescale = 1000;
        let info = super::build_stream_info(0, &t, &oxideav_core::NullCodecResolver);
        assert_eq!(info.params.options.get("dref_self_contained"), None);
        assert_eq!(info.params.options.get("dref_count"), None);
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

    /// §8.9.5 flag bit 7 — `index_msb_indicates_fragment_local_description`
    /// is captured from `flags & 0x80`, the index field width is recorded,
    /// and the per-index resolver splits the source-selector MSB off the
    /// stored value while leaving the raw index verbatim.
    #[test]
    fn parse_csgp_captures_fragment_local_flag_and_resolves_msb() {
        // 8-bit index width (index_size_code=1) + bit 7 set:
        // flags = 1 | (0<<2) | (0<<4) | (1<<7) = 0x81. count/pattern at
        // 4-bit (code 0). pattern_length=3, sample_count=2 → 0x32 byte.
        // indices 0x80,0x01,0x83 each one byte (8-bit width).
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x81]);
        body.extend_from_slice(b"seig");
        body.extend_from_slice(&1u32.to_be_bytes()); // pattern_count
        body.push(0x32); // pattern_length=3 (4b) | sample_count=2 (4b)
        body.extend_from_slice(&[0x80, 0x01, 0x83]); // three 8-bit indices
        let cg = super::parse_csgp(&body).unwrap();

        assert!(cg.index_msb_indicates_fragment_local_description);
        assert_eq!(cg.index_field_bits, 8);
        // Raw values preserved.
        assert_eq!(cg.patterns[0].indices, vec![0x80, 0x01, 0x83]);

        let pat = &cg.patterns[0];
        let r0 = pat
            .resolve_index(
                0,
                cg.index_msb_indicates_fragment_local_description,
                cg.index_field_bits,
            )
            .unwrap();
        assert!(r0.fragment_local);
        assert_eq!(r0.value, 0);
        let r1 = pat
            .resolve_index(
                1,
                cg.index_msb_indicates_fragment_local_description,
                cg.index_field_bits,
            )
            .unwrap();
        assert!(!r1.fragment_local);
        assert_eq!(r1.value, 1);
        let r2 = pat
            .resolve_index(
                2,
                cg.index_msb_indicates_fragment_local_description,
                cg.index_field_bits,
            )
            .unwrap();
        assert!(r2.fragment_local);
        assert_eq!(r2.value, 3);
    }

    /// The resolver passes indices through verbatim when the flag is clear,
    /// and handles every documented width's MSB position.
    #[test]
    fn resolve_csgp_index_width_aware() {
        // Flag clear → verbatim, never fragment-local.
        let r = super::resolve_csgp_index(0x8000_0001, false, 32);
        assert!(!r.fragment_local);
        assert_eq!(r.value, 0x8000_0001);

        // 4-bit width: MSB at bit 3 (mask 0x8).
        let r = super::resolve_csgp_index(0x9, true, 4);
        assert!(r.fragment_local);
        assert_eq!(r.value, 0x1);

        // 16-bit width: MSB at bit 15 (mask 0x8000).
        let r = super::resolve_csgp_index(0x8001, true, 16);
        assert!(r.fragment_local);
        assert_eq!(r.value, 0x1);

        // 32-bit width: MSB at bit 31.
        let r = super::resolve_csgp_index(0x8000_0005, true, 32);
        assert!(r.fragment_local);
        assert_eq!(r.value, 5);

        // Degenerate width 0 → no MSB, verbatim.
        let r = super::resolve_csgp_index(0xFF, true, 0);
        assert!(!r.fragment_local);
        assert_eq!(r.value, 0xFF);
    }

    /// §8.9.5 pattern → sample expansion: a pattern whose `sample_count`
    /// exceeds its `pattern_length` cycles its index entries across the
    /// samples, ending mid-pattern when the count does not divide evenly.
    #[test]
    fn csgp_resolve_samples_cycles_pattern() {
        // Single pattern: indices [10, 20, 30], sample_count = 7. The
        // length-3 run cycles: 10,20,30,10,20,30,10.
        let cg = super::CsgpBox {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            index_field_bits: 16,
            patterns: vec![super::CsgpPattern {
                sample_count: 7,
                indices: vec![10, 20, 30],
            }],
        };
        let resolved = cg.resolve_samples(7);
        let values: Vec<u32> = resolved.iter().map(|r| r.value).collect();
        assert_eq!(values, vec![10, 20, 30, 10, 20, 30, 10]);
        assert!(resolved.iter().all(|r| !r.fragment_local));
    }

    /// §8.9.5: two patterns concatenate, each cycled across its own
    /// `sample_count`, and `sample_count == pattern_length` uses the
    /// pattern once un-repeated.
    #[test]
    fn csgp_resolve_samples_multi_pattern() {
        let cg = super::CsgpBox {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            index_field_bits: 16,
            patterns: vec![
                // length 2, count 2 → used once: 1,2
                super::CsgpPattern {
                    sample_count: 2,
                    indices: vec![1, 2],
                },
                // length 1, count 3 → cycled: 9,9,9
                super::CsgpPattern {
                    sample_count: 3,
                    indices: vec![9],
                },
            ],
        };
        let values: Vec<u32> = cg.resolve_samples(5).iter().map(|r| r.value).collect();
        assert_eq!(values, vec![1, 2, 9, 9, 9]);
    }

    /// §8.9.5: when the patterns cover fewer than `total_samples` samples,
    /// the trailing samples take the `sgpd` default — surfaced as value 0.
    #[test]
    fn csgp_resolve_samples_pads_trailing_default() {
        let cg = super::CsgpBox {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            index_field_bits: 16,
            patterns: vec![super::CsgpPattern {
                sample_count: 2,
                indices: vec![5],
            }],
        };
        // 2 mapped + 3 trailing defaults = 5.
        let values: Vec<u32> = cg.resolve_samples(5).iter().map(|r| r.value).collect();
        assert_eq!(values, vec![5, 5, 0, 0, 0]);
    }

    /// §8.9.5: a sum of `sample_count` exceeding `total_samples` is clamped
    /// so the output never overruns the track's real sample count.
    #[test]
    fn csgp_resolve_samples_clamps_overflow() {
        let cg = super::CsgpBox {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            index_field_bits: 16,
            patterns: vec![super::CsgpPattern {
                sample_count: 100,
                indices: vec![7],
            }],
        };
        let resolved = cg.resolve_samples(3);
        assert_eq!(resolved.len(), 3);
        assert!(resolved.iter().all(|r| r.value == 7));
    }

    /// §8.9.5 expansion honours the fragment-local MSB convention: each
    /// emitted index is split into (fragment_local, value) per the box's
    /// flag and field width.
    #[test]
    fn csgp_resolve_samples_resolves_fragment_local_msb() {
        // 8-bit indices, bit-7 flag set: 0x81 = frag-local value 1,
        // 0x02 = global value 2.
        let cg = super::CsgpBox {
            grouping_type: *b"seig",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: true,
            index_field_bits: 8,
            patterns: vec![super::CsgpPattern {
                sample_count: 4,
                indices: vec![0x81, 0x02],
            }],
        };
        let resolved = cg.resolve_samples(4);
        assert_eq!(resolved.len(), 4);
        assert!(resolved[0].fragment_local);
        assert_eq!(resolved[0].value, 1);
        assert!(!resolved[1].fragment_local);
        assert_eq!(resolved[1].value, 2);
        // Cycle repeats.
        assert!(resolved[2].fragment_local);
        assert_eq!(resolved[2].value, 1);
        assert!(!resolved[3].fragment_local);
        assert_eq!(resolved[3].value, 2);
    }

    /// §8.9.5: a pattern with an empty index run cannot be cycled and
    /// contributes nothing — its `sample_count` is skipped without panic.
    #[test]
    fn csgp_resolve_samples_empty_pattern_skipped() {
        let cg = super::CsgpBox {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            index_field_bits: 16,
            patterns: vec![
                super::CsgpPattern {
                    sample_count: 5,
                    indices: vec![],
                },
                super::CsgpPattern {
                    sample_count: 2,
                    indices: vec![3],
                },
            ],
        };
        // Empty pattern maps nothing; second pattern maps 3,3; the rest
        // of total_samples=4 pads with the default 0.
        let values: Vec<u32> = cg.resolve_samples(4).iter().map(|r| r.value).collect();
        assert_eq!(values, vec![3, 3, 0, 0]);
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

    /// §8.9.5: `pattern_size_code` and `count_size_code` must agree on
    /// whether the 4-bit width is used. A box with `pattern_size_code = 0`
    /// (4-bit) but `count_size_code = 1` (8-bit) is an invalid file and is
    /// rejected rather than mis-decoded.
    #[test]
    fn parse_csgp_mixed_4bit_width_rejected() {
        // flags = count_size_code(1) << 2 = 0x04: pattern_size_code = 0,
        // count_size_code = 1, index_size_code = 0 → a 4/non-4 mix.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x04]);
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&0u32.to_be_bytes()); // pattern_count = 0
        let err = super::parse_csgp(&body);
        assert!(err.is_err(), "mixed 4-bit/non-4-bit width must be rejected");
    }

    /// The mirror case — `pattern_size_code = 1` (8-bit), `count_size_code
    /// = 0` (4-bit) — is equally invalid and rejected.
    #[test]
    fn parse_csgp_mixed_4bit_width_rejected_other_way() {
        // flags = pattern_size_code(1) << 4 = 0x10.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x10]);
        body.extend_from_slice(b"roll");
        body.extend_from_slice(&0u32.to_be_bytes()); // pattern_count = 0
        assert!(super::parse_csgp(&body).is_err());
    }

    /// Both codes at 0 (4-bit) agree and parse fine; both ≥ 1 agree and
    /// parse fine. (Guards against the constraint check over-rejecting.)
    #[test]
    fn parse_csgp_agreeing_widths_accepted() {
        // both 4-bit (flags = 0), pattern_count = 0.
        let mut both_4 = Vec::new();
        both_4.extend_from_slice(&[0u8, 0, 0, 0]);
        both_4.extend_from_slice(b"roll");
        both_4.extend_from_slice(&0u32.to_be_bytes());
        assert!(super::parse_csgp(&both_4).is_ok());

        // both 8-bit: pattern_size_code = 1 (<<4 = 0x10), count_size_code =
        // 1 (<<2 = 0x04) → flags = 0x14, pattern_count = 0.
        let mut both_8 = Vec::new();
        both_8.extend_from_slice(&[0u8, 0, 0, 0x14]);
        both_8.extend_from_slice(b"roll");
        both_8.extend_from_slice(&0u32.to_be_bytes());
        assert!(super::parse_csgp(&both_8).is_ok());
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

    /// `build_pdin_box` is the byte-exact inverse of `parse_pdin_box`: a
    /// two-entry table re-parses to the same `(rate, initial_delay)` pairs
    /// in order.
    #[test]
    fn build_pdin_box_round_trips() {
        let record = super::PdinRecord {
            entries: vec![
                super::PdinEntry {
                    rate: 125_000,
                    initial_delay: 2_500,
                },
                super::PdinEntry {
                    rate: 250_000,
                    initial_delay: 1_200,
                },
            ],
        };
        let boxed = super::build_pdin_box(&record);
        assert_eq!(&boxed[4..8], b"pdin");
        let r = super::parse_pdin_box(&boxed[8..]).unwrap();
        assert_eq!(r.entries, record.entries);
    }

    /// An empty entry list yields a preamble-only `pdin` (12 bytes:
    /// 8-byte header + 4-byte FullBox preamble).
    #[test]
    fn build_pdin_box_empty() {
        let boxed = super::build_pdin_box(&super::PdinRecord { entries: vec![] });
        assert_eq!(boxed.len(), 12);
        let r = super::parse_pdin_box(&boxed[8..]).unwrap();
        assert!(r.entries.is_empty());
    }

    /// `build_pnot_box` → `parse_pnot_box` is a byte-exact round-trip; the
    /// typical `PICT` / index-1 poster locator decodes field-for-field.
    #[test]
    fn pnot_box_round_trips() {
        let r = super::PnotRecord {
            modification_date: 0xC0FF_EE00,
            version: 0,
            atom_type: *b"PICT",
            atom_index: 1,
        };
        let boxed = super::build_pnot_box(&r);
        assert_eq!(&boxed[4..8], b"pnot");
        assert_eq!(boxed.len(), 20); // 8 header + 12 body
        let back = super::parse_pnot_box(&boxed[8..]).expect("round-trip");
        assert_eq!(back, r);
    }

    /// A `pnot` body shorter than the fixed 12 bytes is rejected.
    #[test]
    fn pnot_box_too_short_is_rejected() {
        assert!(super::parse_pnot(&[0u8; 11]).is_none());
        assert!(super::parse_pnot_box(&[0u8; 11]).is_err());
    }

    /// `build_prft_box` is the byte-exact inverse of `parse_prft_box` for
    /// a version-0 (32-bit media_time) record, including the 2022-edition
    /// `flags` annotation bits.
    #[test]
    fn build_prft_box_v0_round_trips() {
        let record = super::PrftRecord {
            reference_track_id: 1,
            ntp_timestamp: 0x1234_5678_9ABC_DEF0,
            media_time: 90_000,
            version: 0,
            flags: 0x00_0001, // encoder_input_output
        };
        let boxed = super::build_prft_box(&record).unwrap();
        assert_eq!(&boxed[4..8], b"prft");
        let r = super::parse_prft_box(&boxed[8..]).unwrap().unwrap();
        assert_eq!(r.reference_track_id, 1);
        assert_eq!(r.ntp_timestamp, 0x1234_5678_9ABC_DEF0);
        assert_eq!(r.media_time, 90_000);
        assert_eq!(r.version, 0);
        assert_eq!(r.flags, 0x00_0001);
        assert!(r.is_encoder_input_output());
    }

    /// A version-1 record (64-bit media_time beyond u32 range) round-trips.
    #[test]
    fn build_prft_box_v1_round_trips() {
        let record = super::PrftRecord {
            reference_track_id: 3,
            ntp_timestamp: 0xDEAD_BEEF_CAFE_F00D,
            media_time: 0x1_0000_0001,
            version: 1,
            flags: 0,
        };
        let boxed = super::build_prft_box(&record).unwrap();
        let r = super::parse_prft_box(&boxed[8..]).unwrap().unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(r.media_time, 0x1_0000_0001);
        assert_eq!(r.reference_track_id, 3);
    }

    /// A version-0 record whose media_time overflows the 32-bit field is
    /// rejected (the caller must bump to version 1); an unknown version
    /// is rejected.
    #[test]
    fn build_prft_box_rejects_inconsistent() {
        let overflow = super::PrftRecord {
            reference_track_id: 1,
            ntp_timestamp: 0,
            media_time: 0x1_0000_0000,
            version: 0,
            flags: 0,
        };
        assert!(super::build_prft_box(&overflow).is_none());
        let bad_version = super::PrftRecord {
            version: 2,
            ..overflow
        };
        assert!(super::build_prft_box(&bad_version).is_none());
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

    // ---- assp (AlternativeStartupSequencePropertiesBox, §8.8.16) -------

    /// A version-0 `assp` body decodes to a single implied entry carrying
    /// the signed `min_initial_alt_startup_offset` and no
    /// `grouping_type_parameter`.
    #[test]
    fn parse_assp_v0_single_offset() {
        let mut body = vec![0u8; 4]; // version 0 + flags 0
        body.extend_from_slice(&(-5i32).to_be_bytes());
        let r = super::parse_assp(&body).unwrap();
        assert_eq!(r.version, 0);
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].grouping_type_parameter, None);
        assert_eq!(r.entries[0].min_initial_alt_startup_offset, -5);
    }

    /// A version-1 `assp` body decodes `num_entries` keyed
    /// `(grouping_type_parameter, offset)` pairs in order.
    #[test]
    fn parse_assp_v1_keyed_entries() {
        let mut body = vec![1u8, 0, 0, 0]; // version 1 + flags 0
        body.extend_from_slice(&2u32.to_be_bytes()); // num_entries
        body.extend_from_slice(&7u32.to_be_bytes());
        body.extend_from_slice(&(-3i32).to_be_bytes());
        body.extend_from_slice(&9u32.to_be_bytes());
        body.extend_from_slice(&100i32.to_be_bytes());
        let r = super::parse_assp(&body).unwrap();
        assert_eq!(r.version, 1);
        assert_eq!(r.entries.len(), 2);
        assert_eq!(r.entries[0].grouping_type_parameter, Some(7));
        assert_eq!(r.entries[0].min_initial_alt_startup_offset, -3);
        assert_eq!(r.entries[1].grouping_type_parameter, Some(9));
        assert_eq!(r.entries[1].min_initial_alt_startup_offset, 100);
    }

    /// A truncated v0 offset, a truncated v1 `num_entries`, a v1
    /// `num_entries` that overruns the body, and an unsupported version
    /// are all rejected.
    #[test]
    fn parse_assp_rejects_malformed() {
        // too short for the FullBox preamble
        assert!(super::parse_assp(&[0u8; 3]).is_err());
        // v0 with no offset
        assert!(super::parse_assp(&[0u8; 4]).is_err());
        // v1 with no num_entries
        assert!(super::parse_assp(&[1u8, 0, 0, 0]).is_err());
        // v1 declaring 4 entries but carrying bytes for one
        let mut overrun = vec![1u8, 0, 0, 0];
        overrun.extend_from_slice(&4u32.to_be_bytes());
        overrun.extend_from_slice(&[0u8; 8]);
        assert!(super::parse_assp(&overrun).is_err());
        // unsupported version 2
        assert!(super::parse_assp(&[2u8, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    /// `build_assp_box` is the byte-exact inverse of `parse_assp_box` for
    /// both versions (modulo the 8-byte box header the builder prepends).
    #[test]
    fn assp_build_parse_round_trip() {
        for record in [
            super::AsspRecord {
                version: 0,
                entries: vec![super::AsspEntry {
                    grouping_type_parameter: None,
                    min_initial_alt_startup_offset: -42,
                }],
            },
            super::AsspRecord {
                version: 1,
                entries: vec![
                    super::AsspEntry {
                        grouping_type_parameter: Some(0),
                        min_initial_alt_startup_offset: 0,
                    },
                    super::AsspEntry {
                        grouping_type_parameter: Some(0xDEAD_BEEF),
                        min_initial_alt_startup_offset: i32::MIN,
                    },
                ],
            },
        ] {
            let bytes = super::build_assp_box(&record).unwrap();
            assert_eq!(&bytes[4..8], b"assp");
            assert_eq!(
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
                bytes.len()
            );
            let reparsed = super::parse_assp_box(&bytes[8..]).unwrap();
            assert_eq!(reparsed, record);
        }
    }

    /// `build_assp_box` rejects records that cannot round-trip: a v0
    /// record with a `grouping_type_parameter`, a v0 record with ≠ 1
    /// entry, or an unsupported version.
    #[test]
    fn assp_build_rejects_invalid_records() {
        // v0 entry carrying a grouping_type_parameter (no v0 wire slot)
        assert!(super::build_assp_box(&super::AsspRecord {
            version: 0,
            entries: vec![super::AsspEntry {
                grouping_type_parameter: Some(1),
                min_initial_alt_startup_offset: 0,
            }],
        })
        .is_err());
        // v0 with two entries
        assert!(super::build_assp_box(&super::AsspRecord {
            version: 0,
            entries: vec![
                super::AsspEntry {
                    grouping_type_parameter: None,
                    min_initial_alt_startup_offset: 0,
                };
                2
            ],
        })
        .is_err());
        // unsupported version
        assert!(super::build_assp_box(&super::AsspRecord {
            version: 9,
            entries: vec![],
        })
        .is_err());
    }

    /// When an `assp` child of a `trep` decodes cleanly, the `TrepChild`
    /// carries the typed `AsspRecord`; a malformed `assp` (or any other
    /// child type) leaves `assp = None`.
    #[test]
    fn parse_trep_decodes_assp_child() {
        // A clean v0 assp body: version+flags (4) + offset (4) = 8 bytes.
        let mut assp_body = vec![0u8; 4];
        assp_body.extend_from_slice(&(-7i32).to_be_bytes());
        let assp = build_child_box(b"assp", &assp_body);
        // A different child stays opaque.
        let other = build_child_box(b"xtra", &[0; 4]);
        let body = build_trep(11, &[assp, other]);
        let r = super::parse_trep(&body).unwrap();
        assert_eq!(r.children.len(), 2);
        let a = r.children[0].assp.as_ref().expect("assp should decode");
        assert_eq!(a.version, 0);
        assert_eq!(a.entries[0].min_initial_alt_startup_offset, -7);
        assert!(r.children[1].assp.is_none());
    }

    /// `build_trep_box` round-trips a `TrepRecord` carrying a typed `assp`
    /// child (and a bare empty-payload child) through `parse_trep_box`.
    #[test]
    fn build_trep_box_round_trips_assp_child() {
        let record = super::TrepRecord {
            track_id: 5,
            children: vec![
                super::TrepChild {
                    fourcc: *b"assp",
                    payload_len: 0,
                    assp: Some(super::AsspRecord {
                        version: 1,
                        entries: vec![super::AsspEntry {
                            grouping_type_parameter: Some(2),
                            min_initial_alt_startup_offset: -8,
                        }],
                    }),
                },
                super::TrepChild {
                    fourcc: *b"zzzz",
                    payload_len: 0,
                    assp: None,
                },
            ],
        };
        let bytes = super::build_trep_box(&record).unwrap();
        assert_eq!(&bytes[4..8], b"trep");
        let reparsed = super::parse_trep_box(&bytes[8..]).unwrap();
        assert_eq!(reparsed.track_id, 5);
        assert_eq!(reparsed.children.len(), 2);
        // The typed `assp` record round-trips verbatim (payload_len is a
        // derived on-wire length the builder recomputes, so we compare
        // the fourcc + typed record rather than the whole struct).
        assert_eq!(&reparsed.children[0].fourcc, b"assp");
        assert_eq!(reparsed.children[0].assp, record.children[0].assp);
        // The opaque empty-payload child round-trips fourcc + zero length.
        assert_eq!(&reparsed.children[1].fourcc, b"zzzz");
        assert_eq!(reparsed.children[1].payload_len, 0);
        assert!(reparsed.children[1].assp.is_none());
    }

    /// `build_trep_box` rejects a child whose `assp` record is set but
    /// whose fourcc isn't `assp`, and a non-`assp` child with a non-zero
    /// `payload_len` (the crate can't synthesise opaque child bytes).
    #[test]
    fn build_trep_box_rejects_inconsistent_children() {
        // assp record under a non-assp fourcc
        assert!(super::build_trep_box(&super::TrepRecord {
            track_id: 1,
            children: vec![super::TrepChild {
                fourcc: *b"abcd",
                payload_len: 0,
                assp: Some(super::AsspRecord {
                    version: 0,
                    entries: vec![super::AsspEntry {
                        grouping_type_parameter: None,
                        min_initial_alt_startup_offset: 0,
                    }],
                }),
            }],
        })
        .is_err());
        // opaque child with non-zero payload_len
        assert!(super::build_trep_box(&super::TrepRecord {
            track_id: 1,
            children: vec![super::TrepChild {
                fourcc: *b"abcd",
                payload_len: 4,
                assp: None,
            }],
        })
        .is_err());
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

    /// `build_leva_box` emits a box whose body round-trips through
    /// `parse_leva_box` — covering every assignment_type tail (0 carries
    /// grouping_type, 1 adds grouping_type_parameter, 2/3 are bare, 4
    /// carries sub_track_id) plus the padding_flag bit packing.
    #[test]
    fn leva_build_round_trip_all_assignment_types() {
        let record = super::LevaRecord {
            entries: vec![
                super::LevaEntry {
                    track_id: 1,
                    padding_flag: true,
                    assignment_type: 2,
                    grouping_type: 0,
                    grouping_type_parameter: 0,
                    sub_track_id: 0,
                },
                super::LevaEntry {
                    track_id: 2,
                    padding_flag: false,
                    assignment_type: 0,
                    grouping_type: u32::from_be_bytes(*b"rash"),
                    grouping_type_parameter: 0,
                    sub_track_id: 0,
                },
                super::LevaEntry {
                    track_id: 3,
                    padding_flag: true,
                    assignment_type: 1,
                    grouping_type: u32::from_be_bytes(*b"roll"),
                    grouping_type_parameter: 9,
                    sub_track_id: 0,
                },
                super::LevaEntry {
                    track_id: 4,
                    padding_flag: false,
                    assignment_type: 4,
                    grouping_type: 0,
                    grouping_type_parameter: 0,
                    sub_track_id: 77,
                },
            ],
        };
        let boxed = super::build_leva_box(&record).unwrap();
        assert_eq!(&boxed[4..8], b"leva");
        assert_eq!(
            boxed.len(),
            u32::from_be_bytes(boxed[..4].try_into().unwrap()) as usize
        );
        let parsed = super::parse_leva_box(&boxed[8..]).unwrap();
        assert_eq!(parsed.entries, record.entries);
    }

    /// `build_leva_box` rejects an `assignment_type` that does not fit the
    /// 7-bit field rather than corrupting it into the `padding_flag` bit.
    #[test]
    fn leva_build_rejects_oversized_assignment_type() {
        let record = super::LevaRecord {
            entries: vec![super::LevaEntry {
                track_id: 1,
                padding_flag: false,
                assignment_type: 200,
                grouping_type: 0,
                grouping_type_parameter: 0,
                sub_track_id: 0,
            }],
        };
        assert!(super::build_leva_box(&record).is_err());
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

    /// `build_subs_box` is the byte-exact inverse of `parse_subs_box` for
    /// a multi-entry version-0 table (sparse deltas, multiple sub-samples,
    /// non-trivial codec_specific_parameters and flags).
    #[test]
    fn build_subs_box_v0_round_trips() {
        let record = super::SubsBox {
            version: 0,
            flags: 0x00_aa_55,
            entries: vec![
                super::SubsEntry {
                    sample_delta: 1,
                    subsamples: vec![
                        super::SubSampleEntry {
                            subsample_size: 100,
                            subsample_priority: 5,
                            discardable: 0,
                            codec_specific_parameters: 1,
                        },
                        super::SubSampleEntry {
                            subsample_size: 50,
                            subsample_priority: 4,
                            discardable: 1,
                            codec_specific_parameters: 2,
                        },
                    ],
                },
                super::SubsEntry {
                    sample_delta: 2,
                    subsamples: vec![super::SubSampleEntry {
                        subsample_size: 80,
                        subsample_priority: 6,
                        discardable: 0,
                        codec_specific_parameters: 0xdead_beef,
                    }],
                },
            ],
        };
        let boxed = super::build_subs_box(&record).unwrap();
        assert_eq!(&boxed[4..8], b"subs");
        let s = super::parse_subs_box(&boxed[8..]).unwrap();
        assert_eq!(s.version, 0);
        assert_eq!(s.flags, 0x00_aa_55);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].subsamples[1].subsample_size, 50);
        assert_eq!(s.entries[0].subsamples[1].discardable, 1);
        assert_eq!(
            s.entries[1].subsamples[0].codec_specific_parameters,
            0xdead_beef
        );
    }

    /// Version 1 widens `subsample_size` to 32-bit; a value above the
    /// 16-bit ceiling round-trips through the builder.
    #[test]
    fn build_subs_box_v1_wide_size() {
        let record = super::SubsBox {
            version: 1,
            flags: 0,
            entries: vec![super::SubsEntry {
                sample_delta: 1,
                subsamples: vec![super::SubSampleEntry {
                    subsample_size: 0x0001_2345,
                    subsample_priority: 0,
                    discardable: 0,
                    codec_specific_parameters: 0,
                }],
            }],
        };
        let boxed = super::build_subs_box(&record).unwrap();
        let s = super::parse_subs_box(&boxed[8..]).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.entries[0].subsamples[0].subsample_size, 0x0001_2345);
    }

    /// An empty entry list (a content-free `subs`) round-trips, and an
    /// entry with a zero `subsample_count` is preserved.
    #[test]
    fn build_subs_box_empty_and_degenerate() {
        let empty = super::build_subs_box(&super::SubsBox::default()).unwrap();
        assert!(super::parse_subs_box(&empty[8..])
            .unwrap()
            .entries
            .is_empty());
        let degenerate = super::SubsBox {
            version: 0,
            flags: 0,
            entries: vec![super::SubsEntry {
                sample_delta: 5,
                subsamples: vec![],
            }],
        };
        let boxed = super::build_subs_box(&degenerate).unwrap();
        let s = super::parse_subs_box(&boxed[8..]).unwrap();
        assert_eq!(s.entries[0].sample_delta, 5);
        assert!(s.entries[0].subsamples.is_empty());
    }

    /// A version-0 record whose `subsample_size` overflows the 16-bit v0
    /// field is rejected (the caller must use version 1); an undefined
    /// version is rejected.
    #[test]
    fn build_subs_box_rejects_inconsistent() {
        let overflow = super::SubsBox {
            version: 0,
            flags: 0,
            entries: vec![super::SubsEntry {
                sample_delta: 1,
                subsamples: vec![super::SubSampleEntry {
                    subsample_size: 0x0001_0000,
                    subsample_priority: 0,
                    discardable: 0,
                    codec_specific_parameters: 0,
                }],
            }],
        };
        assert!(super::build_subs_box(&overflow).is_none());
        let bad_version = super::SubsBox {
            version: 2,
            ..super::SubsBox::default()
        };
        assert!(super::build_subs_box(&bad_version).is_none());
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

    /// `build_saiz_box` is the byte-exact inverse of `parse_saiz_box`
    /// for a constant-size table without an aux-info-type key.
    #[test]
    fn build_saiz_box_constant_size_round_trips() {
        let record = super::SaizBox {
            aux_info_type: None,
            aux_info_type_parameter: None,
            default_sample_info_size: 8,
            sample_count: 4,
            per_sample: vec![],
        };
        let boxed = super::build_saiz_box(&record).unwrap();
        assert_eq!(&boxed[4..8], b"saiz");
        let s = super::parse_saiz_box(&boxed[8..]).unwrap();
        assert_eq!(s.default_sample_info_size, 8);
        assert_eq!(s.sample_count, 4);
        assert!(s.aux_info_type.is_none());
        assert!(s.per_sample.is_empty());
    }

    /// A variable-size `saiz` with a `(cenc, 7)` aux-info-type key round-
    /// trips, including the per-sample size table.
    #[test]
    fn build_saiz_box_variable_with_aux_type() {
        let record = super::SaizBox {
            aux_info_type: Some(*b"cenc"),
            aux_info_type_parameter: Some(7),
            default_sample_info_size: 0,
            sample_count: 3,
            per_sample: vec![10, 20, 30],
        };
        let boxed = super::build_saiz_box(&record).unwrap();
        let s = super::parse_saiz_box(&boxed[8..]).unwrap();
        assert_eq!(s.aux_info_type, Some(*b"cenc"));
        assert_eq!(s.aux_info_type_parameter, Some(7));
        assert_eq!(s.sample_count, 3);
        assert_eq!(s.per_sample, vec![10, 20, 30]);
    }

    /// Inconsistent `saiz` records are rejected: a constant size with a
    /// non-empty per-sample table, or a variable table whose declared
    /// `sample_count` disagrees with `per_sample.len()`.
    #[test]
    fn build_saiz_box_rejects_inconsistent() {
        let both = super::SaizBox {
            default_sample_info_size: 8,
            per_sample: vec![1],
            ..Default::default()
        };
        assert!(super::build_saiz_box(&both).is_none());
        let count_mismatch = super::SaizBox {
            default_sample_info_size: 0,
            sample_count: 5,
            per_sample: vec![1, 2],
            ..Default::default()
        };
        assert!(super::build_saiz_box(&count_mismatch).is_none());
    }

    /// `build_saio_box` is the byte-exact inverse of `parse_saio_box` for
    /// both v0 (32-bit offsets) and v1 (64-bit offsets, with aux key).
    #[test]
    fn build_saio_box_v0_and_v1_round_trip() {
        let v0 = super::SaioBox {
            version: 0,
            aux_info_type: None,
            aux_info_type_parameter: None,
            offsets: vec![0x1000, 0x2000],
        };
        let boxed = super::build_saio_box(&v0).unwrap();
        assert_eq!(&boxed[4..8], b"saio");
        let s = super::parse_saio_box(&boxed[8..]).unwrap();
        assert_eq!(s.version, 0);
        assert_eq!(s.offsets, vec![0x1000, 0x2000]);

        let v1 = super::SaioBox {
            version: 1,
            aux_info_type: Some(*b"cenc"),
            aux_info_type_parameter: Some(7),
            offsets: vec![0x1_0000_0000, 0xffff_ffff_ffff_0000],
        };
        let boxed = super::build_saio_box(&v1).unwrap();
        let s = super::parse_saio_box(&boxed[8..]).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.aux_info_type, Some(*b"cenc"));
        assert_eq!(s.aux_info_type_parameter, Some(7));
        assert_eq!(s.offsets, vec![0x1_0000_0000, 0xffff_ffff_ffff_0000]);
    }

    /// A v0 `saio` whose offset exceeds the 32-bit field is rejected (the
    /// caller must use version 1); an undefined version is rejected.
    #[test]
    fn build_saio_box_rejects_inconsistent() {
        let overflow = super::SaioBox {
            version: 0,
            aux_info_type: None,
            aux_info_type_parameter: None,
            offsets: vec![0x1_0000_0000],
        };
        assert!(super::build_saio_box(&overflow).is_none());
        let bad_version = super::SaioBox {
            version: 2,
            ..Default::default()
        };
        assert!(super::build_saio_box(&bad_version).is_none());
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
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
            &mut Vec::new(),
        )
        .expect("walk succeeds despite bad pssh");
        assert!(moof_psshes.is_empty(), "malformed pssh dropped silently");
    }

    /// §8.9 — a `traf` carrying a fragment-local `sgpd` + `sbgp` + `csgp`
    /// is collected into a `TrafSampleGroupRecord` keyed by
    /// `(track_idx, moof_sequence)`, with each box's values preserved
    /// verbatim. Exercises the structured-record side that the
    /// integration test (which only sees `Box<dyn Demuxer>`) cannot reach.
    #[test]
    fn parse_traf_collects_fragment_local_sample_groups() {
        // tfhd: FullBox(flags=0) + track_ID = 1. No optional fields.
        let mut tfhd_body = vec![0u8, 0, 0, 0];
        tfhd_body.extend_from_slice(&1u32.to_be_bytes());
        let tfhd = wrap_box(b"tfhd", &tfhd_body);

        // sgpd / sbgp / csgp built via the public sample_groups builders.
        let sgpd =
            crate::sample_groups::build_sgpd(&crate::sample_groups::SampleGroupDescription {
                grouping_type: *b"seig",
                default_sample_description_index: None,
                entries: vec![vec![0x00, 0x01]],
            });
        let sbgp = crate::sample_groups::build_sbgp(&crate::sample_groups::SampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            entries: vec![(2, 1)],
        });
        let csgp = crate::sample_groups::build_csgp(&crate::sample_groups::CompactSampleToGroup {
            grouping_type: *b"seig",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: true,
            patterns: vec![crate::sample_groups::CompactSampleToGroupPattern {
                sample_count: 2,
                indices: vec![0x81],
            }],
        });

        let mut traf_body = Vec::new();
        traf_body.extend_from_slice(&tfhd);
        traf_body.extend_from_slice(&sgpd);
        traf_body.extend_from_slice(&sbgp);
        traf_body.extend_from_slice(&csgp);

        let mut track = fresh_track();
        track.track_id = 1;
        let tracks = [track];

        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = vec![0];
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut groups: Vec<super::TrafSampleGroupRecord> = Vec::new();

        super::parse_traf(
            &traf_body,
            0,
            &tracks,
            &mut samples,
            &mut next_dts,
            9,
            &mut senc,
            &mut sai,
            &mut groups,
        )
        .expect("traf walk succeeds");

        assert_eq!(groups.len(), 1);
        let r = &groups[0];
        assert_eq!(r.track_idx, 0);
        assert_eq!(r.moof_sequence, 9);
        assert_eq!(r.sgpd.len(), 1);
        assert_eq!(r.sgpd[0].grouping_type, *b"seig");
        assert_eq!(r.sgpd[0].entries, vec![vec![0x00, 0x01]]);
        assert_eq!(r.sbgp.len(), 1);
        assert_eq!(r.sbgp[0].grouping_type, *b"roll");
        assert_eq!(r.sbgp[0].entries, vec![(2, 1)]);
        assert_eq!(r.csgp.len(), 1);
        assert_eq!(r.csgp[0].grouping_type, *b"seig");
        assert!(r.csgp[0].index_msb_indicates_fragment_local_description);
        assert_eq!(r.csgp[0].patterns[0].sample_count, 2);
        assert_eq!(r.csgp[0].patterns[0].indices, vec![0x81]);
        // 0x81 at 8-bit width → fragment_local, value 1.
        let resolved = r.csgp[0].patterns[0]
            .resolve_index(0, true, r.csgp[0].index_field_bits)
            .unwrap();
        assert!(resolved.fragment_local);
        assert_eq!(resolved.value, 1);
    }

    /// A `traf` with no sample-group boxes contributes no
    /// `TrafSampleGroupRecord`.
    #[test]
    fn parse_traf_without_sample_groups_records_nothing() {
        let mut tfhd_body = vec![0u8, 0, 0, 0];
        tfhd_body.extend_from_slice(&1u32.to_be_bytes());
        let traf_body = wrap_box(b"tfhd", &tfhd_body);

        let mut track = fresh_track();
        track.track_id = 1;
        let tracks = [track];

        let mut samples: Vec<super::SampleRef> = Vec::new();
        let mut next_dts: Vec<i64> = vec![0];
        let mut senc: Vec<super::SencRecord> = Vec::new();
        let mut sai: Vec<super::SaiRecord> = Vec::new();
        let mut groups: Vec<super::TrafSampleGroupRecord> = Vec::new();

        super::parse_traf(
            &traf_body,
            0,
            &tracks,
            &mut samples,
            &mut next_dts,
            1,
            &mut senc,
            &mut sai,
            &mut groups,
        )
        .expect("traf walk succeeds");

        assert!(groups.is_empty());
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

    // ----------------------------------------------------------------
    // §8.11 meta item infrastructure (iloc / pitm / iinf / infe / iref)
    // ----------------------------------------------------------------

    /// Wrap `body` in a `[size][fourcc]` box header (32-bit size form).
    fn box_bytes(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut v = total.to_be_bytes().to_vec();
        v.extend_from_slice(fourcc);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parse_iloc_v0_single_extent() {
        // version 0, offset_size=4 length_size=4 base_offset_size=0,
        // item_count=1, item_ID=1, dref=0, extent_count=1,
        // extent_offset=0x1000, extent_length=0x40.
        let mut b = vec![0u8, 0, 0, 0]; // version/flags
        b.push(0x44); // offset_size=4, length_size=4
        b.push(0x00); // base_offset_size=0, reserved=0
        b.extend_from_slice(&1u16.to_be_bytes()); // item_count
        b.extend_from_slice(&1u16.to_be_bytes()); // item_ID
        b.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
                                                  // base_offset omitted (size 0)
        b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        b.extend_from_slice(&0x1000u32.to_be_bytes()); // extent_offset
        b.extend_from_slice(&0x40u32.to_be_bytes()); // extent_length
        let iloc = super::parse_iloc_box(&b).expect("iloc parses");
        assert_eq!(iloc.version, 0);
        assert_eq!(iloc.offset_size, 4);
        assert_eq!(iloc.length_size, 4);
        assert_eq!(iloc.base_offset_size, 0);
        assert_eq!(iloc.items.len(), 1);
        let it = &iloc.items[0];
        assert_eq!(it.item_id, 1);
        assert_eq!(it.construction_method, 0);
        assert_eq!(it.base_offset, 0);
        assert_eq!(it.extents.len(), 1);
        assert_eq!(it.extents[0].extent_offset, 0x1000);
        assert_eq!(it.extents[0].extent_length, 0x40);
    }

    #[test]
    fn parse_iloc_v1_construction_method_and_index() {
        // version 1, offset_size=8 length_size=8 base_offset_size=4
        // index_size=4, item_count=1, item_ID=7, construction_method=1
        // (idat), dref=0, base_offset=0x10, extent_count=1,
        // extent_index=2, extent_offset=0x20, extent_length=0x30.
        let mut b = vec![1u8, 0, 0, 0];
        b.push(0x88); // offset_size=8, length_size=8
        b.push(0x44); // base_offset_size=4, index_size=4
        b.extend_from_slice(&1u16.to_be_bytes()); // item_count
        b.extend_from_slice(&7u16.to_be_bytes()); // item_ID
        b.extend_from_slice(&0x0001u16.to_be_bytes()); // reserved(12)+constr(4)=1
        b.extend_from_slice(&0u16.to_be_bytes()); // dref
        b.extend_from_slice(&0x10u32.to_be_bytes()); // base_offset (4)
        b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        b.extend_from_slice(&2u32.to_be_bytes()); // extent_index (4)
        b.extend_from_slice(&0x20u64.to_be_bytes()); // extent_offset (8)
        b.extend_from_slice(&0x30u64.to_be_bytes()); // extent_length (8)
        let iloc = super::parse_iloc_box(&b).expect("iloc v1 parses");
        assert_eq!(iloc.version, 1);
        assert_eq!(iloc.index_size, 4);
        let it = &iloc.items[0];
        assert_eq!(it.item_id, 7);
        assert_eq!(it.construction_method, 1);
        assert_eq!(it.base_offset, 0x10);
        assert_eq!(it.extents[0].extent_index, 2);
        assert_eq!(it.extents[0].extent_offset, 0x20);
        assert_eq!(it.extents[0].extent_length, 0x30);
    }

    #[test]
    fn parse_iloc_v2_32bit_item_id() {
        // version 2 widens item_count and item_ID to 32 bits.
        let mut b = vec![2u8, 0, 0, 0];
        b.push(0x44); // offset_size=4, length_size=4
        b.push(0x00); // base_offset_size=0, index_size=0
        b.extend_from_slice(&1u32.to_be_bytes()); // item_count (32-bit)
        b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // item_ID (32-bit)
        b.extend_from_slice(&0x0000u16.to_be_bytes()); // reserved+constr=0
        b.extend_from_slice(&0u16.to_be_bytes()); // dref
        b.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        b.extend_from_slice(&0x55u32.to_be_bytes()); // extent_offset
        b.extend_from_slice(&0x66u32.to_be_bytes()); // extent_length
        let iloc = super::parse_iloc_box(&b).expect("iloc v2 parses");
        assert_eq!(iloc.items[0].item_id, 0x0001_0000);
        assert_eq!(iloc.items[0].extents[0].extent_offset, 0x55);
    }

    #[test]
    fn parse_iloc_rejects_bad_field_width() {
        // offset_size=5 is not in {0,4,8} → reject.
        let mut b = vec![0u8, 0, 0, 0];
        b.push(0x54); // offset_size=5 (invalid), length_size=4
        b.push(0x00);
        b.extend_from_slice(&0u16.to_be_bytes());
        assert!(super::parse_iloc_box(&b).is_none());
    }

    #[test]
    fn parse_pitm_v0_and_v1() {
        let mut v0 = vec![0u8, 0, 0, 0];
        v0.extend_from_slice(&0x1234u16.to_be_bytes());
        assert_eq!(super::parse_pitm_box(&v0), Some(0x1234));
        let mut v1 = vec![1u8, 0, 0, 0];
        v1.extend_from_slice(&0x0001_2345u32.to_be_bytes());
        assert_eq!(super::parse_pitm_box(&v1), Some(0x0001_2345));
    }

    #[test]
    fn parse_iinf_v0_with_infe_v2() {
        // iinf v0: entry_count(16)=1 then one infe v2 box.
        // infe v2: item_ID(16)=1, prot_index(16)=0, item_type="hvc1",
        // item_name="image\0".
        let mut infe_body = vec![2u8, 0, 0, 0];
        infe_body.extend_from_slice(&1u16.to_be_bytes());
        infe_body.extend_from_slice(&0u16.to_be_bytes());
        infe_body.extend_from_slice(b"hvc1");
        infe_body.extend_from_slice(b"image\0");
        let infe = box_bytes(b"infe", &infe_body);

        let mut iinf_body = vec![0u8, 0, 0, 0];
        iinf_body.extend_from_slice(&1u16.to_be_bytes()); // entry_count
        iinf_body.extend_from_slice(&infe);

        let iinf = super::parse_iinf_box(&iinf_body).expect("iinf parses");
        assert_eq!(iinf.entries.len(), 1);
        let e = &iinf.entries[0];
        assert_eq!(e.item_id, 1);
        assert_eq!(&e.item_type, b"hvc1");
        assert_eq!(e.item_name, "image");
        assert_eq!(e.version, 2);
    }

    #[test]
    fn parse_infe_v0_name_and_content_type() {
        // v0/v1 shape: item_ID(16), prot_index(16), item_name\0,
        // content_type\0, content_encoding\0(optional).
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(b"thumb\0");
        body.extend_from_slice(b"image/jpeg\0");
        // wrap + reparse via iinf to exercise parse_infe through public path
        let infe = box_bytes(b"infe", &body);
        let mut iinf_body = vec![0u8, 0, 0, 0];
        iinf_body.extend_from_slice(&1u16.to_be_bytes());
        iinf_body.extend_from_slice(&infe);
        let iinf = super::parse_iinf_box(&iinf_body).unwrap();
        let e = &iinf.entries[0];
        assert_eq!(e.item_id, 5);
        assert_eq!(e.item_name, "thumb");
        assert_eq!(e.content_type, "image/jpeg");
        assert_eq!(e.version, 0);
        assert_eq!(&e.item_type, &[0u8; 4]);
    }

    #[test]
    fn parse_iref_v0_groups() {
        // iref v0: one dimg ref from item 1 to items {2,3}, one thmb
        // ref from item 4 to item 1.
        let mut dimg = 1u16.to_be_bytes().to_vec();
        dimg.extend_from_slice(&2u16.to_be_bytes()); // ref_count
        dimg.extend_from_slice(&2u16.to_be_bytes());
        dimg.extend_from_slice(&3u16.to_be_bytes());
        let dimg = box_bytes(b"dimg", &dimg);
        let mut thmb = 4u16.to_be_bytes().to_vec();
        thmb.extend_from_slice(&1u16.to_be_bytes());
        thmb.extend_from_slice(&1u16.to_be_bytes());
        let thmb = box_bytes(b"thmb", &thmb);
        let mut body = vec![0u8, 0, 0, 0]; // version 0
        body.extend_from_slice(&dimg);
        body.extend_from_slice(&thmb);
        let iref = super::parse_iref_box(&body).expect("iref parses");
        assert_eq!(iref.version, 0);
        assert_eq!(iref.references.len(), 2);
        assert_eq!(&iref.references[0].reference_type, b"dimg");
        assert_eq!(iref.references[0].from_item_id, 1);
        assert_eq!(iref.references[0].to_item_ids, vec![2, 3]);
        assert_eq!(&iref.references[1].reference_type, b"thmb");
        assert_eq!(iref.references[1].from_item_id, 4);
        assert_eq!(iref.references[1].to_item_ids, vec![1]);
    }

    #[test]
    fn parse_iref_v1_32bit_ids() {
        let mut g = 0x0001_0000u32.to_be_bytes().to_vec();
        g.extend_from_slice(&1u16.to_be_bytes()); // ref_count
        g.extend_from_slice(&0x0002_0000u32.to_be_bytes());
        let g = box_bytes(b"cdsc", &g);
        let mut body = vec![1u8, 0, 0, 0]; // version 1
        body.extend_from_slice(&g);
        let iref = super::parse_iref_box(&body).unwrap();
        assert_eq!(iref.version, 1);
        assert_eq!(iref.references[0].from_item_id, 0x0001_0000);
        assert_eq!(iref.references[0].to_item_ids, vec![0x0002_0000]);
    }

    #[test]
    fn parse_meta_items_full_heif_meta() {
        // Assemble a meta body with hdlr(pict) + pitm + iloc + iinf + iref.
        let mut hdlr = vec![0u8, 0, 0, 0]; // version/flags
        hdlr.extend_from_slice(&[0, 0, 0, 0]); // pre_defined
        hdlr.extend_from_slice(b"pict"); // handler_type
        hdlr.extend_from_slice(&[0u8; 12]); // reserved
        hdlr.push(0); // name terminator
        let hdlr = box_bytes(b"hdlr", &hdlr);

        let mut pitm = vec![0u8, 0, 0, 0];
        pitm.extend_from_slice(&1u16.to_be_bytes());
        let pitm = box_bytes(b"pitm", &pitm);

        let mut iloc_body = vec![0u8, 0, 0, 0];
        iloc_body.push(0x44);
        iloc_body.push(0x00);
        iloc_body.extend_from_slice(&1u16.to_be_bytes()); // item_count
        iloc_body.extend_from_slice(&1u16.to_be_bytes()); // item_ID
        iloc_body.extend_from_slice(&0u16.to_be_bytes()); // dref
        iloc_body.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        iloc_body.extend_from_slice(&0x100u32.to_be_bytes());
        iloc_body.extend_from_slice(&0x10u32.to_be_bytes());
        let iloc = box_bytes(b"iloc", &iloc_body);

        let mut infe_body = vec![2u8, 0, 0, 0];
        infe_body.extend_from_slice(&1u16.to_be_bytes());
        infe_body.extend_from_slice(&0u16.to_be_bytes());
        infe_body.extend_from_slice(b"hvc1");
        infe_body.extend_from_slice(b"\0");
        let infe = box_bytes(b"infe", &infe_body);
        let mut iinf_body = vec![0u8, 0, 0, 0];
        iinf_body.extend_from_slice(&1u16.to_be_bytes());
        iinf_body.extend_from_slice(&infe);
        let iinf = box_bytes(b"iinf", &iinf_body);

        let mut dimg = 1u16.to_be_bytes().to_vec();
        dimg.extend_from_slice(&1u16.to_be_bytes());
        dimg.extend_from_slice(&2u16.to_be_bytes());
        let dimg = box_bytes(b"dimg", &dimg);
        let mut iref_body = vec![0u8, 0, 0, 0];
        iref_body.extend_from_slice(&dimg);
        let iref = box_bytes(b"iref", &iref_body);

        // meta body = version/flags(4) + children.
        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&hdlr);
        meta.extend_from_slice(&pitm);
        meta.extend_from_slice(&iloc);
        meta.extend_from_slice(&iinf);
        meta.extend_from_slice(&iref);

        let mi = super::parse_meta_items(&meta);
        assert_eq!(&mi.handler_type, b"pict");
        assert_eq!(mi.primary_item_id, Some(1));
        assert_eq!(mi.iloc.as_ref().unwrap().items.len(), 1);
        assert_eq!(mi.iinf.as_ref().unwrap().entries[0].item_name, "");
        assert_eq!(&mi.iinf.as_ref().unwrap().entries[0].item_type, b"hvc1");
        assert_eq!(mi.iref.as_ref().unwrap().references.len(), 1);
        assert!(!mi.is_empty());
    }

    #[test]
    fn meta_walk_surfaces_fiin_fd_item_info() {
        use crate::fd::{FiinBox, FparBox, PartitionEntry};

        // hdlr(`fdel`) + a fiin built via the fd builders.
        let mut hdlr = vec![0u8, 0, 0, 0];
        hdlr.extend_from_slice(&[0, 0, 0, 0]);
        hdlr.extend_from_slice(b"fdel");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let hdlr = box_bytes(b"hdlr", &hdlr);

        let fiin = FiinBox {
            partition_entries: vec![PartitionEntry {
                fpar: Some(FparBox {
                    version: 0,
                    item_id: 1,
                    packet_payload_size: 1400,
                    fec_encoding_id: 0,
                    fec_instance_id: 0,
                    max_source_block_length: 16,
                    encoding_symbol_length: 1024,
                    max_number_of_encoding_symbols: 16,
                    scheme_specific_info: String::new(),
                    entries: vec![],
                }),
                fecr: None,
                fire: None,
            }],
            session_info: None,
            group_id_to_name: None,
        };
        let fiin_bytes = crate::fd::build_fiin_box(&fiin);

        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&hdlr);
        meta.extend_from_slice(&fiin_bytes);

        let mi = super::parse_meta_items(&meta);
        assert_eq!(&mi.handler_type, b"fdel");
        let parsed = mi.fiin.as_ref().expect("fiin parsed");
        assert_eq!(parsed.partition_entries.len(), 1);
        assert_eq!(parsed, &fiin);
        assert!(!mi.is_empty());

        // The flat metadata surface carries the partition count.
        let mut kv = Vec::new();
        super::surface_meta_items(&mi, &mut kv);
        assert!(kv
            .iter()
            .any(|(k, v)| k == "meta_fiin_partitions" && v == "1"));
    }

    #[test]
    fn mere_round_trips() {
        let r = super::MereRelation {
            first_metabox_handler_type: *b"mp7t",
            second_metabox_handler_type: *b"mdir",
            metabox_relation: 5,
        };
        let bytes = super::build_mere_box(&r);
        // [size][mere] header then 13-byte body.
        assert_eq!(&bytes[4..8], b"mere");
        let body = &bytes[8..];
        assert_eq!(body.len(), 13);
        assert_eq!(super::parse_mere_box(body).unwrap(), r);
    }

    #[test]
    fn mere_truncated_rejected() {
        // FullBox preamble + only one handler type — short of 13 bytes.
        assert!(super::parse_mere_box(&[0, 0, 0, 0, b'a', b'b', b'c', b'd']).is_none());
    }

    #[test]
    fn meco_walk_collects_metas_and_relations() {
        // meco body = two additional meta boxes (distinct handler types)
        // + one mere relation between them.
        fn meta_with_handler(h: &[u8; 4]) -> Vec<u8> {
            let mut hdlr = vec![0u8, 0, 0, 0];
            hdlr.extend_from_slice(&[0, 0, 0, 0]);
            hdlr.extend_from_slice(h);
            hdlr.extend_from_slice(&[0u8; 12]);
            hdlr.push(0);
            let hdlr = box_bytes(b"hdlr", &hdlr);
            let mut meta = vec![0u8, 0, 0, 0];
            meta.extend_from_slice(&hdlr);
            box_bytes(b"meta", &meta)
        }
        let meta_a = meta_with_handler(b"mp7t");
        let meta_b = meta_with_handler(b"mdir");
        let mere = super::build_mere_box(&super::MereRelation {
            first_metabox_handler_type: *b"mp7t",
            second_metabox_handler_type: *b"mdir",
            metabox_relation: 3,
        });

        let mut meco_body = Vec::new();
        meco_body.extend_from_slice(&meta_a);
        meco_body.extend_from_slice(&meta_b);
        meco_body.extend_from_slice(&mere);

        let meco = super::parse_meco_box(&meco_body);
        assert!(!meco.is_empty());
        assert_eq!(meco.metas.len(), 2);
        assert_eq!(&meco.metas[0].handler_type, b"mp7t");
        assert_eq!(&meco.metas[1].handler_type, b"mdir");
        assert_eq!(meco.relations.len(), 1);
        assert_eq!(meco.relations[0].metabox_relation, 3);
        assert_eq!(&meco.relations[0].first_metabox_handler_type, b"mp7t");
    }

    #[test]
    fn meco_empty_body_is_empty() {
        assert!(super::parse_meco_box(&[]).is_empty());
    }

    #[test]
    fn meta_items_idat_resolution_and_lookups() {
        // meta with iloc (method 1 / idat) for item 1 (offset 4, len 6)
        // + idat box of 16 bytes + iinf naming item 1.
        let idat_payload: Vec<u8> = (0u8..16).collect();
        let idat = box_bytes(b"idat", &idat_payload);

        let mut iloc = vec![1u8, 0, 0, 0]; // version 1 (needs constr method)
        iloc.push(0x44); // offset_size=4, length_size=4
        iloc.push(0x00); // base_offset_size=0, index_size=0
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item_count
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item_ID
        iloc.extend_from_slice(&0x0001u16.to_be_bytes()); // reserved+constr=1
        iloc.extend_from_slice(&0u16.to_be_bytes()); // dref
        iloc.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        iloc.extend_from_slice(&4u32.to_be_bytes()); // extent_offset
        iloc.extend_from_slice(&6u32.to_be_bytes()); // extent_length
        let iloc = box_bytes(b"iloc", &iloc);

        let mut infe = vec![2u8, 0, 0, 0];
        infe.extend_from_slice(&1u16.to_be_bytes());
        infe.extend_from_slice(&0u16.to_be_bytes());
        infe.extend_from_slice(b"Exif");
        infe.extend_from_slice(b"exif\0");
        let infe = box_bytes(b"infe", &infe);
        let mut iinf = vec![0u8, 0, 0, 0];
        iinf.extend_from_slice(&1u16.to_be_bytes());
        iinf.extend_from_slice(&infe);
        let iinf = box_bytes(b"iinf", &iinf);

        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&iloc);
        meta.extend_from_slice(&iinf);
        meta.extend_from_slice(&idat);
        let mi = super::parse_meta_items(&meta);

        assert_eq!(mi.idat.len(), 16);
        // item_data_from_idat concatenates the extent (offset 4, len 6).
        assert_eq!(mi.item_data_from_idat(1), Some(vec![4u8, 5, 6, 7, 8, 9]));
        // byte ranges reflect base_offset(0) + extent_offset(4).
        let ranges = mi.item_byte_ranges(1).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].offset, 4);
        assert_eq!(ranges[0].length, 6);
        // lookups by ID.
        assert_eq!(mi.iloc_item(1).unwrap().construction_method, 1);
        assert_eq!(mi.item_info(1).unwrap().item_name, "exif");
        assert!(mi.iloc_item(99).is_none());
        // a file-method item would not resolve via idat.
        assert!(mi.item_data_from_idat(99).is_none());
    }

    #[test]
    fn meta_items_idat_overrun_rejected() {
        // extent runs past the 4-byte idat → resolution returns None.
        let idat = box_bytes(b"idat", &[1u8, 2, 3, 4]);
        let mut iloc = vec![1u8, 0, 0, 0];
        iloc.push(0x44);
        iloc.push(0x00);
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&0x0001u16.to_be_bytes()); // constr=1
        iloc.extend_from_slice(&0u16.to_be_bytes());
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&2u32.to_be_bytes()); // offset 2
        iloc.extend_from_slice(&10u32.to_be_bytes()); // length 10 (overruns)
        let iloc = box_bytes(b"iloc", &iloc);
        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&iloc);
        meta.extend_from_slice(&idat);
        let mi = super::parse_meta_items(&meta);
        assert!(mi.item_data_from_idat(1).is_none());
    }

    // ---- builders: byte-exact round-trips ----

    /// Strip the 8-byte `[size][fourcc]` header from a built box,
    /// returning its body (what the `parse_*` entry points consume).
    fn box_body(built: &[u8]) -> &[u8] {
        &built[8..]
    }

    #[test]
    fn build_iloc_round_trips_v0_v1_v2() {
        let cases = [
            super::IlocBox {
                version: 0,
                offset_size: 4,
                length_size: 4,
                base_offset_size: 0,
                index_size: 0,
                items: vec![super::IlocItem {
                    item_id: 3,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: 0,
                    extents: vec![super::IlocExtent {
                        extent_index: 0,
                        extent_offset: 0x1234,
                        extent_length: 0x56,
                    }],
                }],
            },
            super::IlocBox {
                version: 1,
                offset_size: 8,
                length_size: 8,
                base_offset_size: 4,
                index_size: 4,
                items: vec![super::IlocItem {
                    item_id: 9,
                    construction_method: 1,
                    data_reference_index: 2,
                    base_offset: 0x40,
                    extents: vec![super::IlocExtent {
                        extent_index: 5,
                        extent_offset: 0x9999,
                        extent_length: 0x1000,
                    }],
                }],
            },
            super::IlocBox {
                version: 2,
                offset_size: 4,
                length_size: 4,
                base_offset_size: 0,
                index_size: 0,
                items: vec![super::IlocItem {
                    item_id: 0x0010_0000,
                    construction_method: 0,
                    data_reference_index: 0,
                    base_offset: 0,
                    extents: vec![super::IlocExtent {
                        extent_index: 0,
                        extent_offset: 7,
                        extent_length: 8,
                    }],
                }],
            },
        ];
        for c in cases {
            let built = super::build_iloc_box(&c).expect("build");
            let parsed = super::parse_iloc_box(box_body(&built)).expect("parse");
            assert_eq!(parsed, c, "iloc v{} round-trip", c.version);
        }
    }

    #[test]
    fn build_iloc_rejects_inconsistent_records() {
        // v0 with a non-zero construction_method cannot round-trip.
        let bad = super::IlocBox {
            version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 0,
            index_size: 0,
            items: vec![super::IlocItem {
                item_id: 1,
                construction_method: 1, // illegal for v0
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![],
            }],
        };
        assert!(super::build_iloc_box(&bad).is_none());
        // v0 with index_size != 0 (reserved nibble) is rejected.
        let bad2 = super::IlocBox {
            version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 0,
            index_size: 4,
            items: vec![],
        };
        assert!(super::build_iloc_box(&bad2).is_none());
    }

    #[test]
    fn build_pitm_round_trips() {
        let built = super::build_pitm_box(42);
        assert_eq!(super::parse_pitm_box(box_body(&built)), Some(42));
        let built = super::build_pitm_box(0x0010_0000);
        assert_eq!(super::parse_pitm_box(box_body(&built)), Some(0x0010_0000));
    }

    #[test]
    fn build_iinf_round_trips_mixed_versions() {
        let b = super::IinfBox {
            entries: vec![
                super::ItemInfoEntry {
                    item_id: 1,
                    protection_index: 0,
                    item_type: *b"hvc1",
                    item_name: "primary".to_string(),
                    content_type: String::new(),
                    content_encoding: String::new(),
                    version: 2,
                },
                super::ItemInfoEntry {
                    item_id: 2,
                    protection_index: 0,
                    item_type: *b"mime",
                    item_name: "meta".to_string(),
                    content_type: "application/rdf+xml".to_string(),
                    content_encoding: String::new(),
                    version: 2,
                },
                super::ItemInfoEntry {
                    item_id: 3,
                    protection_index: 0,
                    item_type: [0; 4],
                    item_name: "legacy".to_string(),
                    content_type: "image/jpeg".to_string(),
                    content_encoding: String::new(),
                    version: 0,
                },
            ],
        };
        let built = super::build_iinf_box(&b).expect("build");
        let parsed = super::parse_iinf_box(box_body(&built)).expect("parse");
        assert_eq!(parsed, b);
    }

    #[test]
    fn build_iref_round_trips_v0_v1() {
        let v0 = super::IrefBox {
            version: 0,
            references: vec![
                super::ItemReference {
                    reference_type: *b"dimg",
                    from_item_id: 1,
                    to_item_ids: vec![2, 3, 4],
                },
                super::ItemReference {
                    reference_type: *b"thmb",
                    from_item_id: 5,
                    to_item_ids: vec![1],
                },
            ],
        };
        let built = super::build_iref_box(&v0).unwrap();
        assert_eq!(super::parse_iref_box(box_body(&built)), Some(v0));

        let v1 = super::IrefBox {
            version: 1,
            references: vec![super::ItemReference {
                reference_type: *b"cdsc",
                from_item_id: 0x0001_0000,
                to_item_ids: vec![0x0002_0000],
            }],
        };
        let built = super::build_iref_box(&v1).unwrap();
        assert_eq!(super::parse_iref_box(box_body(&built)), Some(v1));
    }

    #[test]
    fn build_idat_round_trips_through_meta_walk() {
        // Assemble a meta with built iloc(method1) + built idat and
        // confirm item_data_from_idat reads back the original bytes.
        let payload: Vec<u8> = (10u8..30).collect();
        let idat = super::build_idat_box(&payload);
        let iloc = super::build_iloc_box(&super::IlocBox {
            version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 0,
            index_size: 0,
            items: vec![super::IlocItem {
                item_id: 1,
                construction_method: 1,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![super::IlocExtent {
                    extent_index: 0,
                    extent_offset: 3,
                    extent_length: 5,
                }],
            }],
        })
        .unwrap();
        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&iloc);
        meta.extend_from_slice(&idat);
        let mi = super::parse_meta_items(&meta);
        assert_eq!(mi.item_data_from_idat(1), Some(payload[3..8].to_vec()));
    }

    #[test]
    fn parse_meta_items_plain_itunes_is_empty() {
        // A meta with just hdlr(mdir) + ilst carries no §8.11 items.
        let mut hdlr = vec![0u8, 0, 0, 0];
        hdlr.extend_from_slice(&[0, 0, 0, 0]);
        hdlr.extend_from_slice(b"mdir");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let hdlr = box_bytes(b"hdlr", &hdlr);
        let ilst = box_bytes(b"ilst", &[]);
        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&hdlr);
        meta.extend_from_slice(&ilst);
        let mi = super::parse_meta_items(&meta);
        assert_eq!(&mi.handler_type, b"mdir");
        assert!(mi.is_empty());
    }
}
