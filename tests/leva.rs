//! Integration tests for the ISO/IEC 14496-12 §8.8.13 Level Assignment
//! Box (`leva`) round 264 typed accessor.
//!
//! `leva` sits inside `mvex` (which itself sits inside `moov`); a
//! fragmented MP4 is therefore the natural carrier. The tests build a
//! real fragmented MP4 with our muxer, surgically splice a synthetic
//! `leva` box at the end of `mvex`, and then re-open the spliced bytes
//! to confirm:
//!
//! * the flat metadata channel surfaces `leva_count` + `leva_<n>`
//!   strings with the variant-specific tails per §8.8.13.2,
//! * the public `parse_leva_box` entry point recovers the same
//!   structured `LevaEntry` table from the standalone body,
//! * absence emits no `leva_*` keys at all (omission, not zero-count),
//! * a malformed `leva` is dropped silently (the demuxer remains
//!   parseable — leva is informational, not load-bearing for playback),
//! * the public `parse_leva_box` entry point round-trips a standalone
//!   body without an `open()` call.
//!
//! The spec layout reference is
//! `docs/container/isobmff/ISO_IEC_14496-12_2015a.pdf` §8.8.13.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::muxer::open_with_options;
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

/// Per-process monotonic counter so each `mux_pcm_to_bytes` call uses a
/// distinct tempfile path; the harness runs tests in parallel by default
/// and a PID-only filename causes a race in which one test deletes
/// another's still-being-read file.
static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Build a `leva` body (4-byte FullBox preamble + level_count + N
/// per-level entries laid out per §8.8.13.2) and wrap it in a 32-bit
/// box header — `[size:u32]['leva'][body]`.
///
/// Each `LevaEntryBuild` carries the same field set as the public
/// `LevaEntry` type; the variant-specific tail is selected by
/// `assignment_type`.
#[derive(Clone, Copy, Debug)]
struct LevaEntryBuild {
    track_id: u32,
    padding_flag: bool,
    assignment_type: u8,
    grouping_type: u32,
    grouping_type_parameter: u32,
    sub_track_id: u32,
}

fn build_leva_box(entries: &[LevaEntryBuild]) -> Vec<u8> {
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
    let total = 8 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"leva");
    out.extend_from_slice(&body);
    out
}

/// Locate the first occurrence of a 4-byte FourCC in `bytes`. The
/// search is a literal byte scan — used only against well-formed
/// MP4 streams in test fixtures where every byte that matches the
/// FourCC really is a box-type token.
fn find_fourcc(bytes: &[u8], tag: &[u8; 4]) -> Option<usize> {
    // Returns the position of the FourCC itself, not its 4-byte size prefix.
    bytes.windows(4).position(|w| w == &tag[..])
}

/// Splice a `leva` box at the end of the `mvex` container inside
/// `moov`. The mux output sequence is `[ftyp][moov[...mvex[trex...]]][...]`;
/// we locate `mvex`'s 4-byte size prefix (4 bytes before its FourCC),
/// extend it by `leva_box.len()` and insert the box just after `mvex`'s
/// current end. The enclosing `moov` size grows by the same delta.
///
/// Both `moov` and `mvex` are emitted by our muxer with 32-bit sizes
/// (no `largesize` extension), which the test fixtures keep small
/// enough to remain so after the splice.
fn splice_leva_into_mvex(file: &[u8], leva_box: &[u8]) -> Vec<u8> {
    // Find moov / mvex FourCCs (4-byte size prefix sits 4 bytes earlier).
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

    let delta = leva_box.len();
    let new_mvex_size = (mvex_size + delta) as u32;
    let new_moov_size = (moov_size + delta) as u32;

    let mut out = Vec::with_capacity(file.len() + delta);
    // Prefix up to (and including) the mvex children — i.e. everything up to mvex_end.
    out.extend_from_slice(&file[..mvex_end]);
    // Inject the leva box at the end of mvex's child list.
    out.extend_from_slice(leva_box);
    // Tail of the file after mvex (mdat etc.).
    out.extend_from_slice(&file[mvex_end..]);

    // Patch the mvex size in place.
    out[mvex_size_pos..mvex_size_pos + 4].copy_from_slice(&new_mvex_size.to_be_bytes());
    // Patch the enclosing moov size by the same delta.
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
        }),
        write_edit_list: false,
        track_sample_groups: Vec::new(),
        large_mdat: false,
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

/// Mux a tiny fragmented PCM stream to a tempfile and return its
/// bytes for splicing. Routes through a real file rather than a
/// `Cursor` so the `dyn WriteSeek + 'static` constraint on the muxer
/// is satisfied without borrow juggling. The fragmented muxer emits
/// `mvex` (with a `trex` child) inside `moov` — the splice target for
/// `leva`.
fn mux_fragmented_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-leva-r264-{}-{}.mp4",
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
fn leva_inside_mvex_surfaces_on_metadata_and_accessor() {
    // §8.8.13 — three per-level entries covering the three "typical"
    // tail variants (sample-group by grouping_type, parameterized
    // sample-group, sub-track) plus one tail-less type-3 entry to
    // confirm the loop boundary is right.
    let entries = vec![
        LevaEntryBuild {
            track_id: 1,
            padding_flag: true,
            assignment_type: 0,
            grouping_type: u32::from_be_bytes(*b"roll"),
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
        LevaEntryBuild {
            track_id: 1,
            padding_flag: false,
            assignment_type: 1,
            grouping_type: u32::from_be_bytes(*b"alst"),
            grouping_type_parameter: 0x1234_5678,
            sub_track_id: 0,
        },
        LevaEntryBuild {
            track_id: 2,
            padding_flag: false,
            assignment_type: 3,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
        LevaEntryBuild {
            track_id: 3,
            padding_flag: true,
            assignment_type: 4,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0x0000_1010,
        },
    ];
    let leva = build_leva_box(&entries);
    let spliced = splice_leva_into_mvex(&mux_fragmented_pcm_to_bytes(), &leva);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");

    // Flat metadata: `leva_count` + `leva_<n>` (one per level).
    let md = dmx.metadata();
    let count = md.iter().find(|(k, _)| k == "leva_count").map(|(_, v)| v);
    assert_eq!(count, Some(&"4".to_string()));

    let key = |i: usize| format!("leva_{i}");
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(
        get(&key(0)),
        Some(format!(
            "1 pad=1 at=0 grouping_type={}",
            entries[0].grouping_type
        )),
    );
    assert_eq!(
        get(&key(1)),
        Some(format!(
            "1 pad=0 at=1 grouping_type={} grouping_type_parameter={}",
            entries[1].grouping_type, entries[1].grouping_type_parameter,
        )),
    );
    assert_eq!(get(&key(2)), Some("2 pad=0 at=3".to_string()));
    assert_eq!(
        get(&key(3)),
        Some(format!(
            "3 pad=1 at=4 sub_track_id={}",
            entries[3].sub_track_id
        )),
    );

    // Cross-check the public `parse_leva_box` entry against the
    // standalone body we synthesised so the metadata channel and the
    // structured-record entry stay in lockstep. (The Mp4Demuxer struct
    // itself stays crate-private — downstream tooling reaches the
    // typed record via the public `parse_leva_box` entry point or via
    // the flat metadata channel that this test already covers.)
    let standalone = oxideav_mp4::demux::parse_leva_box(&build_leva_box(&entries)[8..])
        .expect("standalone leva body parses");
    assert_eq!(standalone.entries.len(), 4);
    assert_eq!(standalone.entries[0].assignment_type, 0);
    assert_eq!(standalone.entries[1].assignment_type, 1);
    assert_eq!(standalone.entries[2].assignment_type, 3);
    assert_eq!(standalone.entries[3].assignment_type, 4);
    assert_eq!(standalone.entries[3].sub_track_id, entries[3].sub_track_id);
}

#[test]
fn leva_absent_emits_no_leva_keys() {
    // Vanilla fragmented muxer output has no `leva` — confirm omission
    // (no `leva_count`, no `leva_<n>`) rather than a zero-count token.
    let bytes = mux_fragmented_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    for (k, _) in md {
        assert!(
            !k.starts_with("leva"),
            "unexpected leva metadata key {k} on a file with no leva box",
        );
    }
}

#[test]
fn leva_zero_level_count_emits_zero_count_only() {
    // §8.8.13.3 pins `level_count >= 2`, but the demuxer accepts
    // whatever the producer wrote so a downstream validator can flag
    // a short table. A zero-count leva must surface `leva_count = 0`
    // and no `leva_<n>` entries.
    let leva = build_leva_box(&[]);
    let spliced = splice_leva_into_mvex(&mux_fragmented_pcm_to_bytes(), &leva);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let count = md.iter().find(|(k, _)| k == "leva_count").map(|(_, v)| v);
    assert_eq!(count, Some(&"0".to_string()));
    let any_entry = md
        .iter()
        .any(|(k, _)| k.starts_with("leva_") && k != "leva_count");
    assert!(!any_entry, "zero-count leva must not emit leva_<n> entries");
}

#[test]
fn leva_malformed_body_is_dropped_silently() {
    // A truncated `leva` (claims level_count = 1 but stops mid-entry-
    // header) is non-fatal: the demuxer drops the box and continues.
    // `leva` is informational — playback must still succeed.
    let mut bad_body = Vec::new();
    bad_body.extend_from_slice(&[0u8; 4]); // preamble
    bad_body.push(1); // level_count = 1
    bad_body.extend_from_slice(&[0u8; 4]); // partial track_id only (no packed byte)
    let total = 8 + bad_body.len();
    let mut leva_box = Vec::with_capacity(total);
    leva_box.extend_from_slice(&(total as u32).to_be_bytes());
    leva_box.extend_from_slice(b"leva");
    leva_box.extend_from_slice(&bad_body);

    let spliced = splice_leva_into_mvex(&mux_fragmented_pcm_to_bytes(), &leva_box);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver)
        .expect("malformed leva must not abort open");
    let md = dmx.metadata();
    for (k, _) in md {
        assert!(
            !k.starts_with("leva"),
            "malformed leva was silently dropped — no leva_* keys expected, saw {k}",
        );
    }
}

#[test]
fn leva_public_entry_point_parses_standalone_body() {
    // `parse_leva_box` lets tooling that already has the box's payload
    // bytes in hand recover the typed `LevaEntry` table without an
    // `open()` round-trip. Input is the bytes AFTER the 8-byte box
    // header (size + type).
    let entries = vec![
        LevaEntryBuild {
            track_id: 5,
            padding_flag: false,
            assignment_type: 2,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
        LevaEntryBuild {
            track_id: 6,
            padding_flag: true,
            assignment_type: 0,
            grouping_type: u32::from_be_bytes(*b"tele"),
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
    ];
    let leva_full = build_leva_box(&entries);
    let body = &leva_full[8..];
    let record = oxideav_mp4::demux::parse_leva_box(body).expect("standalone leva body parses");
    assert_eq!(record.entries.len(), entries.len());
    assert_eq!(record.entries[0].track_id, 5);
    assert_eq!(record.entries[0].assignment_type, 2);
    assert_eq!(record.entries[1].track_id, 6);
    assert!(record.entries[1].padding_flag);
    assert_eq!(record.entries[1].assignment_type, 0);
    assert_eq!(
        record.entries[1].grouping_type,
        u32::from_be_bytes(*b"tele")
    );
}

/// Round 351 — the fragmented muxer emits a real `leva` (not a spliced
/// one) when `FragmentedOptions::levels` is non-empty, and the demuxer
/// reads it back through the flat metadata channel. This is the
/// write/read symmetry counterpart to the splice-based tests above:
/// `build_leva_box` (the muxer's write primitive) and `parse_leva`
/// (the demuxer's `mvex` walk) agree byte-for-byte.
#[test]
fn muxer_emitted_leva_reads_back_via_metadata() {
    use oxideav_mp4::demux::LevaEntry;

    let levels = vec![
        // Two type-2 (level-by-track) entries followed by one type-0
        // (sample-group) entry — a §8.8.13.3-conformant ordering.
        LevaEntry {
            track_id: 1,
            padding_flag: false,
            assignment_type: 2,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
        LevaEntry {
            track_id: 1,
            padding_flag: true,
            assignment_type: 2,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
        LevaEntry {
            track_id: 1,
            padding_flag: false,
            assignment_type: 0,
            grouping_type: u32::from_be_bytes(*b"roll"),
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
    ];

    let stream = pcm_stream_info();
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-leva-mux-r351-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut opts = fragmented_options();
        if let Some(frag) = opts.fragmented.as_mut() {
            frag.levels = levels.clone();
        }
        let mut mux =
            open_with_options(ws, std::slice::from_ref(&stream), opts).expect("open muxer");
        mux.write_header().unwrap();
        for i in 0..2 {
            let mut pkt = Packet::new(0, stream.time_base, make_pcm_payload(512));
            pkt.pts = Some(i * 512);
            pkt.duration = Some(512);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    let md = dmx.metadata();
    let count = md.iter().find(|(k, _)| k == "leva_count").map(|(_, v)| v);
    assert_eq!(
        count,
        Some(&"3".to_string()),
        "muxer-emitted leva should surface level_count = 3"
    );
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("leva_0"), Some("1 pad=0 at=2".to_string()));
    assert_eq!(get("leva_1"), Some("1 pad=1 at=2".to_string()));
    assert_eq!(
        get("leva_2"),
        Some(format!(
            "1 pad=0 at=0 grouping_type={}",
            u32::from_be_bytes(*b"roll")
        )),
    );
}

/// A fragmented file muxed with the default (empty) `levels` carries no
/// `leva` at all — the byte-identical default `mvex` is preserved.
#[test]
fn muxer_without_levels_emits_no_leva() {
    let bytes = mux_fragmented_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens");
    for (k, _) in dmx.metadata() {
        assert!(
            !k.starts_with("leva"),
            "default muxer output must carry no leva box, found {k}"
        );
    }
}
