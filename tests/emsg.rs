//! `emsg` DASH Event Message Box (ISO/IEC 23009-1 §5.10.3.3) — write
//! side through `FragmentedMuxer::set_next_segment_emsg`, read side
//! through the demuxer's `emsgs()` accessor + `emsg_<n>` metadata
//! keys, verified end-to-end by demuxing the produced file back
//! through this crate's own top-level walk.

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::emsg::{build_emsg_box, EmsgBox, EmsgTime, EMSG_UNKNOWN_DURATION};
use oxideav_mp4::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

fn pcm_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn frag_options(n: u32, indexes: bool) -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        fragmented: Some(FragmentedOptions {
            cadence: FragmentCadence::EveryNPackets(n),
            emit_random_access_indexes: indexes,
            styp: None,
            ..FragmentedOptions::default()
        }),
        ..Mp4MuxerOptions::default()
    }
}

fn open_typed(
    path: &std::path::Path,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
) -> oxideav_mp4::frag::FragmentedMuxer {
    let frag = options.fragmented.clone().expect("fragmented options");
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    oxideav_mp4::frag::open_fragmented_typed(ws, streams, options, frag).unwrap()
}

fn demux_file(path: &std::path::Path) -> oxideav_mp4::demux::Mp4Demuxer {
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap()
}

fn audio_packet(stream: &StreamInfo, i: i64) -> Packet {
    let mut pkt = Packet::new(stream.index, stream.time_base, vec![i as u8; 16]);
    pkt.pts = Some(i * 1024);
    pkt.duration = Some(1024);
    pkt.flags.keyframe = true;
    pkt
}

fn scte35_v0() -> EmsgBox {
    EmsgBox {
        scheme_id_uri: "urn:scte:scte35:2013:bin".to_string(),
        value: "1001".to_string(),
        timescale: 48_000,
        presentation: EmsgTime::Delta(0),
        event_duration: 96_000,
        id: 41,
        message_data: vec![0xFC, 0x30, 0x11],
    }
}

fn app_event_v1(id: u32) -> EmsgBox {
    EmsgBox {
        scheme_id_uri: "urn:example:events:2026".to_string(),
        value: String::new(),
        timescale: 1_000,
        presentation: EmsgTime::Absolute(1_234_567),
        event_duration: EMSG_UNKNOWN_DURATION,
        id,
        message_data: b"opaque-app-payload".to_vec(),
    }
}

/// Minimal top-level box walk: return `(offset, size)` of every box
/// with the given fourcc.
fn top_level_boxes(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        if size < 8 || pos + size > bytes.len() {
            break;
        }
        if &bytes[pos + 4..pos + 8] == fourcc {
            out.push((pos, size));
        }
        pos += size;
    }
    out
}

/// Queue a v0 event on fragment 1 and two v1 events on fragment 2 of
/// a two-fragment file, then demux: the `emsgs()` records must come
/// back in file order with the right `next_moof_index` anchors, exact
/// field values, and `emsg_<n>` metadata summaries.
#[test]
fn emsg_mux_demux_round_trip() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-roundtrip.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, false));
        mux.write_header().unwrap();
        mux.set_next_segment_emsg([scte35_v0()]);
        for i in 0..2i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        // First fragment (with the v0 event) has flushed; queue two v1
        // events for the second.
        mux.set_next_segment_emsg([app_event_v1(7), app_event_v1(8)]);
        for i in 2..4i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Byte-level gate: each emsg must sit at the top level, before the
    // moof of the segment it applies to.
    let bytes = std::fs::read(&tmp).unwrap();
    let emsg_positions = top_level_boxes(&bytes, b"emsg");
    let moof_positions = top_level_boxes(&bytes, b"moof");
    assert_eq!(emsg_positions.len(), 3);
    assert_eq!(moof_positions.len(), 2);
    assert!(emsg_positions[0].0 < moof_positions[0].0);
    assert!(moof_positions[0].0 < emsg_positions[1].0);
    assert!(emsg_positions[2].0 < moof_positions[1].0);

    let dmx = demux_file(&tmp);
    let records = dmx.emsgs();
    assert_eq!(records.len(), 3);

    assert_eq!(records[0].next_moof_index, 0, "v0 event precedes moof #0");
    assert_eq!(records[0].emsg, scte35_v0());
    assert_eq!(records[0].emsg.version(), 0);
    assert_eq!(records[0].emsg.presentation_time_delta(), Some(0));

    for (k, rec) in records[1..].iter().enumerate() {
        assert_eq!(rec.next_moof_index, 1, "v1 events precede moof #1");
        assert_eq!(rec.emsg, app_event_v1(7 + k as u32));
        assert_eq!(rec.emsg.version(), 1);
        assert_eq!(rec.emsg.presentation_time(), Some(1_234_567));
        assert!(rec.emsg.event_duration_unknown());
    }

    // Flat metadata mirror.
    let meta = dmx.metadata();
    let get = |key: &str| {
        meta.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("missing {key}"))
    };
    assert_eq!(
        get("emsg_0"),
        "scheme=urn:scte:scte35:2013:bin value=1001 timescale=48000 delta=0 \
         duration=96000 id=41 bytes=3 before_moof=0"
    );
    assert_eq!(
        get("emsg_1"),
        format!(
            "scheme=urn:example:events:2026 value= timescale=1000 time=1234567 \
             duration={EMSG_UNKNOWN_DURATION} id=7 bytes=18 before_moof=1"
        )
    );
    assert!(meta.iter().any(|(k, _)| k == "emsg_2"));
    assert!(!meta.iter().any(|(k, _)| k == "emsg_3"));
}

/// With `emit_random_access_indexes` on, the emsg bytes land inside
/// the subsegment the `sidx` references (between `sidx` and `moof`),
/// so the single reference's `referenced_size` must cover
/// `emsg + moof + mdat` exactly — a byte-range fetch driven by the
/// index must not fall short.
#[test]
fn emsg_bytes_are_counted_into_sidx_referenced_size() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-sidx.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, true));
        mux.write_header().unwrap();
        mux.set_next_segment_emsg([scte35_v0()]);
        for i in 0..2i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let sidxes = top_level_boxes(&bytes, b"sidx");
    assert_eq!(sidxes.len(), 1);
    let (sidx_pos, sidx_size) = sidxes[0];
    // This crate's writer emits sidx version 1: FullBox(4) +
    // reference_ID(4) + timescale(4) + earliest_presentation_time(8) +
    // first_offset(8) + reserved(2) + reference_count(2) +
    // [4-byte size word ...].
    let body = &bytes[sidx_pos + 8..sidx_pos + sidx_size];
    assert_eq!(body[0], 1, "writer emits sidx v1");
    let first_offset = u64::from_be_bytes(body[20..28].try_into().unwrap()) as usize;
    let referenced_size = {
        let w = u32::from_be_bytes([body[32], body[33], body[34], body[35]]);
        (w & 0x7FFF_FFFF) as usize
    };
    // The referenced subsegment starts at the first byte after the
    // sidx (+ first_offset, 0 here) and must span emsg + moof + mdat.
    let subseg_start = sidx_pos + sidx_size + first_offset;
    assert_eq!(&bytes[subseg_start + 4..subseg_start + 8], b"emsg");
    let emsg_size = top_level_boxes(&bytes, b"emsg")[0].1;
    let moof_size = top_level_boxes(&bytes, b"moof")[0].1;
    let mdat_size = top_level_boxes(&bytes, b"mdat")[0].1;
    assert_eq!(
        referenced_size,
        emsg_size + moof_size + mdat_size,
        "sidx referenced_size must cover the emsg bytes"
    );
    // And the moof must immediately follow the emsg.
    let emsg_pos = top_level_boxes(&bytes, b"emsg")[0].0;
    assert_eq!(
        &bytes[emsg_pos + emsg_size + 4..emsg_pos + emsg_size + 8],
        b"moof"
    );
}

/// A malformed `emsg` encountered during the top-level walk is
/// dropped (the events are informational) without failing the open or
/// disturbing the well-formed records already collected.
#[test]
fn malformed_emsg_is_dropped_without_failing_open() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-malformed.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, false));
        mux.write_header().unwrap();
        mux.set_next_segment_emsg([scte35_v0()]);
        for i in 0..2i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    // Append a hostile emsg at the top level: an undefined version 2
    // (field order unknown) and a v0 body whose string never
    // terminates.
    let mut bytes = std::fs::read(&tmp).unwrap();
    for bad_body in [vec![2u8, 0, 0, 0, 0, 0, 0, 0], {
        let mut b = vec![0u8, 0, 0, 0];
        b.extend_from_slice(b"urn:never-terminated");
        b
    }] {
        let total = (8 + bad_body.len()) as u32;
        bytes.extend_from_slice(&total.to_be_bytes());
        bytes.extend_from_slice(b"emsg");
        bytes.extend_from_slice(&bad_body);
    }
    std::fs::write(&tmp, &bytes).unwrap();

    let dmx = demux_file(&tmp);
    let records = dmx.emsgs();
    assert_eq!(records.len(), 1, "only the well-formed emsg survives");
    assert_eq!(records[0].emsg, scte35_v0());
    assert_eq!(
        dmx.metadata()
            .iter()
            .filter(|(k, _)| k.starts_with("emsg_"))
            .count(),
        1
    );
}

/// An emsg whose strings would not round-trip (interior NUL) must
/// surface its error at the flush that writes it — before any bytes of
/// the fragment hit the output.
#[test]
fn invalid_queued_emsg_fails_the_flush() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-invalid.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, false));
    mux.write_header().unwrap();
    let mut bad = scte35_v0();
    bad.value = "a\0b".to_string();
    mux.set_next_segment_emsg([bad]);
    mux.write_packet(&audio_packet(&stream, 0)).unwrap();
    // Second packet triggers the flush that serialises the queued box.
    let err = mux
        .write_packet(&audio_packet(&stream, 1))
        .expect_err("interior NUL must fail the flush");
    assert!(format!("{err}").contains("NUL"), "{err}");
}

/// Standalone byte builder sanity: `build_emsg_box` output re-parses
/// through the demux path when spliced ahead of a fragment by hand —
/// covering consumers that assemble segments themselves rather than
/// going through `FragmentedMuxer`.
#[test]
fn hand_spliced_emsg_parses_like_muxer_emitted() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-spliced.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, false));
        mux.write_header().unwrap();
        for i in 0..2i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let (moof_pos, _) = top_level_boxes(&bytes, b"moof")[0];
    let emsg = build_emsg_box(&app_event_v1(99)).unwrap();
    let mut spliced = Vec::with_capacity(bytes.len() + emsg.len());
    spliced.extend_from_slice(&bytes[..moof_pos]);
    spliced.extend_from_slice(&emsg);
    spliced.extend_from_slice(&bytes[moof_pos..]);
    let tmp2 = std::env::temp_dir().join("oxideav-mp4-emsg-spliced2.mp4");
    std::fs::write(&tmp2, &spliced).unwrap();

    let dmx = demux_file(&tmp2);
    assert_eq!(dmx.emsgs().len(), 1);
    assert_eq!(dmx.emsgs()[0].emsg, app_event_v1(99));
    assert_eq!(dmx.emsgs()[0].next_moof_index, 0);
    // The spliced-in box shifts the moof by its own length; sample
    // payloads must still demux (offsets are moof-relative via
    // default-base-is-moof, so the shift is transparent).
    let mut dmx: Box<dyn Demuxer> = Box::new(dmx);
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, vec![0u8; 16]);
}

/// `Mp4Demuxer::emsg_absolute_time` — v0 deltas resolve against the
/// earliest presentation time of the moof the box preceded (rescaled
/// into the emsg timescale); v1 records pass their absolute time
/// through; an un-anchorable v0 (box after the last moof) returns
/// `None`.
#[test]
fn emsg_absolute_time_anchors_v0_deltas_to_the_following_moof() {
    let stream = pcm_stream(0);
    let tmp = std::env::temp_dir().join("oxideav-mp4-emsg-abs.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), frag_options(2, false));
        mux.write_header().unwrap();
        // Fragment 1 (samples at pts 0, 1024): a v0 delta of 100.
        let mut e0 = scte35_v0();
        e0.presentation = EmsgTime::Delta(100);
        mux.set_next_segment_emsg([e0]);
        for i in 0..2i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        // Fragment 2 (samples at pts 2048, 3072): a v0 delta of 100 in
        // a *different* timescale (24000 = half the track's 48000), so
        // the anchor must rescale, plus a v1 for pass-through.
        let mut e1 = scte35_v0();
        e1.timescale = 24_000;
        e1.presentation = EmsgTime::Delta(100);
        mux.set_next_segment_emsg([e1, app_event_v1(5)]);
        for i in 2..4i64 {
            mux.write_packet(&audio_packet(&stream, i)).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    let records = dmx.emsgs();
    assert_eq!(records.len(), 3);

    // Fragment 1 anchor: earliest pts 0 → 0 + 100.
    assert_eq!(dmx.emsg_absolute_time(&records[0]), Some(100));
    // Fragment 2 anchor: earliest pts 2048 @ 48000 → 1024 @ 24000,
    // + 100 = 1124.
    assert_eq!(records[1].next_moof_index, 1);
    assert_eq!(dmx.emsg_absolute_time(&records[1]), Some(1124));
    // v1: absolute value passes through untouched.
    assert_eq!(dmx.emsg_absolute_time(&records[2]), Some(1_234_567));

    // Un-anchorable v0: a record claiming to precede a moof that
    // doesn't exist.
    let orphan = oxideav_mp4::demux::EmsgRecord {
        next_moof_index: 99,
        emsg: scte35_v0(),
    };
    assert_eq!(dmx.emsg_absolute_time(&orphan), None);
}
