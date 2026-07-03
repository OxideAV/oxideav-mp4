//! Integration tests for the ISO/IEC 14496-12 §8.8.15 Track Extension
//! Properties Box (`trep`).
//!
//! `trep` sits inside `mvex` (which itself sits inside `moov`); a
//! fragmented MP4 is therefore the natural carrier. The tests build a
//! real fragmented MP4 with our muxer, surgically splice one or more
//! synthetic `trep` boxes at the end of `mvex`, and then re-open the
//! spliced bytes to confirm:
//!
//! * the flat metadata channel surfaces one `trep_<n>` string per box,
//!   in file order, carrying the `track_id`, the child count, and each
//!   child box's four-character type (per §8.8.15.2 "any number of
//!   boxes"),
//! * the public `parse_trep_box` entry point recovers the same
//!   structured `TrepRecord` from the standalone body,
//! * absence emits no `trep_*` keys at all (omission, not zero-count),
//! * a malformed `trep` is dropped silently (the demuxer remains
//!   parseable — `trep` is informational, not load-bearing for
//!   playback).
//!
//! The spec layout reference is
//! `docs/container/isobmff/ISO_IEC_14496-12_2015a.pdf` §8.8.15.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::muxer::open_with_options;
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

/// Per-process monotonic counter so each mux call uses a distinct
/// tempfile path; the harness runs tests in parallel by default and a
/// PID-only filename causes a race in which one test deletes another's
/// still-being-read file.
static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Build a child box (8-byte header + payload) for embedding in a
/// `trep` body — `[size:u32][fourcc][payload]`.
fn child_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let size = 8 + payload.len();
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(&(size as u32).to_be_bytes());
    v.extend_from_slice(fourcc);
    v.extend_from_slice(payload);
    v
}

/// Build a `trep` box: `[size:u32]['trep'][FullBox preamble][track_id]
/// [child boxes...]` per §8.8.15.2.
fn build_trep_box(track_id: u32, children: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
    body.extend_from_slice(&track_id.to_be_bytes());
    for c in children {
        body.extend_from_slice(c);
    }
    let total = 8 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"trep");
    out.extend_from_slice(&body);
    out
}

/// Locate the first occurrence of a 4-byte FourCC in `bytes` (the
/// position of the FourCC itself, not its 4-byte size prefix). A literal
/// byte scan — used only against the well-formed MP4 streams these
/// fixtures emit.
fn find_fourcc(bytes: &[u8], tag: &[u8; 4]) -> Option<usize> {
    bytes.windows(4).position(|w| w == &tag[..])
}

/// Splice a box at the end of the `mvex` container inside `moov`. The
/// mux output sequence is `[ftyp][moov[...mvex[trex...]]][...]`; we
/// locate `mvex`'s 4-byte size prefix, extend it by `box_bytes.len()`,
/// insert the box just after `mvex`'s current end, and grow the
/// enclosing `moov` size by the same delta. Both boxes are emitted by
/// our muxer with 32-bit sizes and the fixtures stay small enough to
/// remain so after the splice.
fn splice_into_mvex(file: &[u8], box_bytes: &[u8]) -> Vec<u8> {
    let moov_type_pos = find_fourcc(file, b"moov").expect("moov FourCC present");
    let mvex_type_pos = find_fourcc(file, b"mvex").expect("mvex FourCC present");
    assert!(
        mvex_type_pos > moov_type_pos,
        "mvex sits inside moov in our fragmented muxer output",
    );

    let moov_size_pos = moov_type_pos - 4;
    let mvex_size_pos = mvex_type_pos - 4;

    let moov_size = u32::from_be_bytes([
        file[moov_size_pos],
        file[moov_size_pos + 1],
        file[moov_size_pos + 2],
        file[moov_size_pos + 3],
    ]) as usize;
    let mvex_size = u32::from_be_bytes([
        file[mvex_size_pos],
        file[mvex_size_pos + 1],
        file[mvex_size_pos + 2],
        file[mvex_size_pos + 3],
    ]) as usize;

    let mvex_end = mvex_size_pos + mvex_size;
    let moov_end = moov_size_pos + moov_size;
    assert!(
        mvex_end <= moov_end,
        "mvex end {mvex_end} must lie within moov end {moov_end}"
    );

    let delta = box_bytes.len();
    let new_mvex_size = (mvex_size + delta) as u32;
    let new_moov_size = (moov_size + delta) as u32;

    let mut out = Vec::with_capacity(file.len() + delta);
    out.extend_from_slice(&file[..mvex_end]);
    out.extend_from_slice(box_bytes);
    out.extend_from_slice(&file[mvex_end..]);

    out[mvex_size_pos..mvex_size_pos + 4].copy_from_slice(&new_mvex_size.to_be_bytes());
    out[moov_size_pos..moov_size_pos + 4].copy_from_slice(&new_moov_size.to_be_bytes());

    out
}

fn pcm_stream_info() -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn fragmented_options() -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"mp41", *b"dash"],
        },
        faststart: false,
        fragmented: Some(FragmentedOptions {
            cadence: FragmentCadence::EveryNPackets(1),
            styp: None,
            emit_random_access_indexes: false,
            levels: Vec::new(),
            emit_ssix: false,
            ssix_levels: (1, 2),
            treps: Vec::new(),
        }),
        write_edit_list: false,
        track_sample_groups: Vec::new(),
        large_mdat: false,
        ..Mp4MuxerOptions::default()
    }
}

fn make_pcm_payload(samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        let l = (i as i16).wrapping_mul(3);
        let r = (i as i16).wrapping_mul(5);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

/// Mux a tiny fragmented PCM stream to a tempfile and return its bytes
/// for splicing. The fragmented muxer emits `mvex` (with a `trex`
/// child) inside `moov` — the splice target for `trep`.
fn mux_fragmented_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-trep-r291-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = fragmented_options();
        let mut mux =
            open_with_options(ws, std::slice::from_ref(&stream), opts).expect("open muxer");
        mux.write_header().unwrap();
        for i in 0..2 {
            let mut pkt = Packet::new(
                0,
                stream.time_base,
                make_pcm_payload(frames_per_packet as usize),
            );
            pkt.pts = Some(i * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[test]
fn trep_inside_mvex_surfaces_on_metadata() {
    // §8.8.15 — a trep for track 1 nesting an `assp` (Alternative
    // Startup Sequence Properties Box, §8.8.16) plus a second box whose
    // type the demuxer records without interpreting.
    let assp = child_box(b"assp", &[0xaa; 8]);
    let unkn = child_box(b"xtra", &[]);
    let trep = build_trep_box(1, &[assp, unkn]);
    let spliced = splice_into_mvex(&mux_fragmented_pcm_to_bytes(), &trep);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");

    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("trep_0"),
        Some("1 children=2 assp xtra".to_string()),
        "trep_0 must carry track_id, child count, and child fourccs in order",
    );
    // Only one trep box was spliced.
    assert!(
        get("trep_1").is_none(),
        "only one trep box present — no trep_1 key expected",
    );
}

#[test]
fn multiple_trep_boxes_each_surface_in_order() {
    // §8.8.15.1 — quantity is zero or more (zero or one per track). Two
    // trep boxes (one per track) must each get their own trep_<n> key in
    // file order.
    let trep_a = build_trep_box(1, &[child_box(b"assp", &[0; 4])]);
    let trep_b = build_trep_box(2, &[]);
    let mut both = trep_a.clone();
    both.extend_from_slice(&trep_b);
    let spliced = splice_into_mvex(&mux_fragmented_pcm_to_bytes(), &both);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("trep_0"), Some("1 children=1 assp".to_string()));
    assert_eq!(get("trep_1"), Some("2 children=0".to_string()));
}

#[test]
fn trep_absent_emits_no_trep_keys() {
    // Vanilla fragmented muxer output has no `trep` — confirm omission
    // (no trep_<n> keys) rather than a zero token.
    let bytes = mux_fragmented_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    for (k, _) in md {
        assert!(
            !k.starts_with("trep"),
            "unexpected trep metadata key {k} on a file with no trep box",
        );
    }
}

#[test]
fn trep_malformed_body_is_dropped_silently() {
    // A truncated `trep` (FullBox preamble but no full track_id) is
    // non-fatal: the demuxer drops the box and continues. `trep` is
    // informational — playback must still succeed.
    let mut bad_body = Vec::new();
    bad_body.extend_from_slice(&[0u8; 4]); // preamble
    bad_body.extend_from_slice(&[0u8; 2]); // partial track_id (2 of 4)
    let total = 8 + bad_body.len();
    let mut trep_box = Vec::with_capacity(total);
    trep_box.extend_from_slice(&(total as u32).to_be_bytes());
    trep_box.extend_from_slice(b"trep");
    trep_box.extend_from_slice(&bad_body);

    let spliced = splice_into_mvex(&mux_fragmented_pcm_to_bytes(), &trep_box);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver)
        .expect("malformed trep must not abort open");
    let md = dmx.metadata();
    for (k, _) in md {
        assert!(
            !k.starts_with("trep"),
            "malformed trep was silently dropped — no trep_* keys expected, saw {k}",
        );
    }
}

#[test]
fn trep_public_entry_point_parses_standalone_body() {
    // `parse_trep_box` lets tooling that already has the box's payload
    // bytes in hand recover the typed `TrepRecord` without an `open()`
    // round-trip. Input is the bytes AFTER the 8-byte box header.
    let trep_full = build_trep_box(77, &[child_box(b"assp", &[1, 2, 3, 4, 5, 6])]);
    let body = &trep_full[8..];
    let record = oxideav_mp4::demux::parse_trep_box(body).expect("standalone trep body parses");
    assert_eq!(record.track_id, 77);
    assert_eq!(record.children.len(), 1);
    assert_eq!(&record.children[0].fourcc, b"assp");
    assert_eq!(record.children[0].payload_len, 6);
}

#[test]
fn trep_v0_assp_child_surfaces_offset_and_typed_record() {
    // §8.8.16 — a clean v0 `assp` child carries a signed
    // `min_initial_alt_startup_offset`. The demuxer decodes it into the
    // typed `TrepChild::assp` and appends "(off=<i>)" to the flat
    // `trep_<n>` metadata value.
    let mut assp_body = vec![0u8; 4]; // FullBox(v=0, flags=0)
    assp_body.extend_from_slice(&(-9i32).to_be_bytes());
    let assp = child_box(b"assp", &assp_body);
    let trep = build_trep_box(1, &[assp]);
    let spliced = splice_into_mvex(&mux_fragmented_pcm_to_bytes(), &trep);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("trep_0"),
        Some("1 children=1 assp(off=-9)".to_string()),
        "v0 assp offset must surface on the flat trep metadata",
    );
}

#[test]
fn trep_v1_assp_child_surfaces_keyed_offsets() {
    // §8.8.16 — a v1 `assp` child documents one entry per
    // `grouping_type_parameter`. Each surfaces as "<gtp>:<offset>".
    let mut assp_body = vec![1u8, 0, 0, 0]; // FullBox(v=1, flags=0)
    assp_body.extend_from_slice(&2u32.to_be_bytes()); // num_entries
    assp_body.extend_from_slice(&3u32.to_be_bytes()); // grouping_type_parameter
    assp_body.extend_from_slice(&(-1i32).to_be_bytes());
    assp_body.extend_from_slice(&4u32.to_be_bytes());
    assp_body.extend_from_slice(&0i32.to_be_bytes());
    let assp = child_box(b"assp", &assp_body);
    let trep = build_trep_box(1, &[assp]);
    let spliced = splice_into_mvex(&mux_fragmented_pcm_to_bytes(), &trep);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("trep_0"),
        Some("1 children=1 assp(3:-1 4:0)".to_string()),
        "v1 assp keyed offsets must surface on the flat trep metadata",
    );
}

#[test]
fn assp_public_build_parse_round_trips() {
    use oxideav_mp4::demux::{build_assp_box, parse_assp_box, AsspEntry, AsspRecord};

    // v0: a single implied entry.
    let v0 = AsspRecord {
        version: 0,
        entries: vec![AsspEntry {
            grouping_type_parameter: None,
            min_initial_alt_startup_offset: -123,
        }],
    };
    let bytes = build_assp_box(&v0).expect("build v0 assp");
    assert_eq!(&bytes[4..8], b"assp");
    let parsed = parse_assp_box(&bytes[8..]).expect("parse v0 assp");
    assert_eq!(parsed, v0);

    // v1: a keyed list.
    let v1 = AsspRecord {
        version: 1,
        entries: vec![
            AsspEntry {
                grouping_type_parameter: Some(10),
                min_initial_alt_startup_offset: 7,
            },
            AsspEntry {
                grouping_type_parameter: Some(20),
                min_initial_alt_startup_offset: -7,
            },
        ],
    };
    let bytes = build_assp_box(&v1).expect("build v1 assp");
    let parsed = parse_assp_box(&bytes[8..]).expect("parse v1 assp");
    assert_eq!(parsed, v1);
}

/// Mux a tiny fragmented PCM stream with `treps` configured on the
/// fragmented muxer, returning the file bytes. The muxer writes one
/// `trep` per record inside the init-segment `mvex`.
fn mux_fragmented_pcm_with_treps(treps: Vec<oxideav_mp4::demux::TrepRecord>) -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-trep-mux-r360-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut opts = fragmented_options();
        if let Some(fr) = opts.fragmented.as_mut() {
            fr.treps = treps;
        }
        let mut mux =
            open_with_options(ws, std::slice::from_ref(&stream), opts).expect("open muxer");
        mux.write_header().unwrap();
        for i in 0..2 {
            let mut pkt = Packet::new(
                0,
                stream.time_base,
                make_pcm_payload(frames_per_packet as usize),
            );
            pkt.pts = Some(i * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[test]
fn fragmented_muxer_emits_trep_with_assp_round_trip() {
    // §8.8.15 / §8.8.16 — configure the fragmented muxer to emit a `trep`
    // for track 1 carrying a typed v0 `assp`. The emitted init segment
    // must read back through the demuxer's `mvex` walk: the flat `trep_0`
    // metadata carries the track id + the assp offset, and `treps()`
    // recovers the structured record.
    use oxideav_mp4::demux::{AsspEntry, AsspRecord, TrepChild, TrepRecord};

    let assp = AsspRecord {
        version: 0,
        entries: vec![AsspEntry {
            grouping_type_parameter: None,
            min_initial_alt_startup_offset: -4,
        }],
    };
    let trep = TrepRecord {
        track_id: 1,
        children: vec![TrepChild {
            fourcc: *b"assp",
            payload_len: 0,
            assp: Some(assp.clone()),
        }],
    };
    let bytes = mux_fragmented_pcm_with_treps(vec![trep]);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("trep_0"),
        Some("1 children=1 assp(off=-4)".to_string()),
        "muxer-emitted trep+assp must read back on the flat metadata channel",
    );
    // Only one trep was configured.
    assert!(
        get("trep_1").is_none(),
        "exactly one trep configured — no trep_1 key expected",
    );
    let _ = assp;
}

#[test]
fn fragmented_muxer_no_treps_emits_no_trep_keys() {
    // The default (empty `treps`) keeps the init segment free of `trep`
    // boxes — no `trep_*` metadata keys.
    let bytes = mux_fragmented_pcm_with_treps(Vec::new());
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    for (k, _) in dmx.metadata() {
        assert!(
            !k.starts_with("trep"),
            "empty treps must emit no trep metadata, saw {k}",
        );
    }
}
