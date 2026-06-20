//! Integration tests for the ISO/IEC 14496-12 §8.16.4 Subsegment Index
//! Box (`ssix`) round 279 typed accessor + builder.
//!
//! `ssix` is a top-level box that must immediately follow the `sidx`
//! it documents (§8.16.4.1), mapping levels (per the §8.8.13 Level
//! Assignment Box) to byte ranges of the indexed subsegments. The
//! tests build a real fragmented MP4 with our muxer (with per-segment
//! `sidx` emission enabled), surgically splice a synthetic `ssix` box
//! right after a `sidx`, and then re-open the spliced bytes to
//! confirm:
//!
//! * the flat metadata channel surfaces a `ssix_<n>` shape summary
//!   (`"<subsegment_count> <total_range_count>"`) per box in file
//!   order,
//! * the public `parse_ssix_box` entry point recovers the same
//!   structured `SsixRecord` from the standalone body,
//! * absence emits no `ssix_*` keys at all (omission, not zero-count),
//! * a malformed `ssix` is dropped silently (the demuxer remains
//!   parseable — ssix is informational, not load-bearing for
//!   playback),
//! * `build_ssix_box` round-trips through `parse_ssix_box` and
//!   rejects a `range_size` that overflows the 24-bit wire field,
//! * the truncation / version guards reject short bodies.
//!
//! The spec layout reference is
//! `docs/container/isobmff/bmff.txt` §8.16.4 (ISO/IEC 14496-12:2015).

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::demux::{
    build_ssix_box, parse_sidx_box, parse_ssix_box, LevaEntry, SsixRange, SsixRecord,
    SsixSubsegment,
};
use oxideav_mp4::muxer::open_with_options;
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

/// Per-process monotonic counter so each `mux_fragmented_pcm_to_bytes`
/// call uses a distinct tempfile path; the harness runs tests in
/// parallel by default and a PID-only filename causes a race in which
/// one test deletes another's still-being-read file.
static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Build an `ssix` body by hand (4-byte FullBox preamble +
/// subsegment_count + per-subsegment range lists laid out per
/// §8.16.4.2) and wrap it in a 32-bit box header —
/// `[size:u32]['ssix'][body]`. Independent of `build_ssix_box` so the
/// splice fixture does not assume the builder it is meant to check.
fn build_ssix_box_manual(subsegments: &[&[(u8, u32)]]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
    body.extend_from_slice(&(subsegments.len() as u32).to_be_bytes());
    for ranges in subsegments {
        body.extend_from_slice(&(ranges.len() as u32).to_be_bytes());
        for &(level, range_size) in *ranges {
            let b = range_size.to_be_bytes();
            body.extend_from_slice(&[level, b[1], b[2], b[3]]); // u8 + u24
        }
    }
    let total = 8 + body.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(b"ssix");
    out.extend_from_slice(&body);
    out
}

/// Locate the Nth (0-based) occurrence of a top-level FourCC in
/// `bytes` by literal byte scan — used only against well-formed MP4
/// test fixtures where every match really is a box-type token.
fn find_fourcc_nth(bytes: &[u8], tag: &[u8; 4], n: usize) -> Option<usize> {
    bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == &tag[..])
        .map(|(i, _)| i)
        .nth(n)
}

/// Splice a complete `ssix` box immediately after the Nth `sidx` box
/// — the §8.16.4.1 placement ("A Subsegment Index box, if any, shall
/// be the next box after the associated Segment Index box"). Both are
/// top-level boxes, so no enclosing size needs patching. The spliced
/// fixture leaves the sidx's `first_offset` untouched (a real
/// packager would bump it by the ssix size); that only matters to
/// byte-precise sidx seeking, which these tests never exercise.
fn splice_ssix_after_sidx(file: &[u8], ssix_box: &[u8], nth_sidx: usize) -> Vec<u8> {
    let sidx_type_pos = find_fourcc_nth(file, b"sidx", nth_sidx).expect("sidx FourCC present");
    let sidx_size_pos = sidx_type_pos - 4;
    let sidx_size = u32::from_be_bytes([
        file[sidx_size_pos],
        file[sidx_size_pos + 1],
        file[sidx_size_pos + 2],
        file[sidx_size_pos + 3],
    ]) as usize;
    let sidx_end = sidx_size_pos + sidx_size;
    let mut out = Vec::with_capacity(file.len() + ssix_box.len());
    out.extend_from_slice(&file[..sidx_end]);
    out.extend_from_slice(ssix_box);
    out.extend_from_slice(&file[sidx_end..]);
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
            // Emit a per-segment `sidx` so the spliced `ssix` has a
            // Segment Index box to document (§8.16.4.1 placement).
            emit_random_access_indexes: true,
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

/// Mux a tiny fragmented PCM stream (2 packets → 2 segments, each
/// preceded by its own `sidx`) to a tempfile and return its bytes for
/// splicing.
fn mux_fragmented_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-ssix-r279-{}-{}.mp4",
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

fn open_bytes(bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux opens")
}

#[test]
fn ssix_after_sidx_surfaces_metadata_and_matches_standalone_parse() {
    let bytes = mux_fragmented_pcm_to_bytes();

    // One subsegment (matching the one-reference sidx our muxer
    // emits), partitioned into two levels: a small level-0 head and a
    // level-1 tail (§8.16.4.1 requires range_count >= 2).
    let ssix_box = build_ssix_box_manual(&[&[(0, 64), (1, 2_048)]]);
    let spliced = splice_ssix_after_sidx(&bytes, &ssix_box, 0);

    let mut dmx = open_bytes(spliced);
    let md = dmx.metadata().to_vec();
    let ssix0 = md
        .iter()
        .find(|(k, _)| k == "ssix_0")
        .expect("ssix_0 metadata key missing");
    assert_eq!(ssix0.1, "1 2", "1 subsegment carrying 2 ranges in total");
    assert!(
        !md.iter().any(|(k, _)| k == "ssix_1"),
        "exactly one ssix box was spliced"
    );

    // The standalone parser recovers the same structured record from
    // the body bytes (the spliced box minus its 8-byte header).
    let r = parse_ssix_box(&ssix_box[8..]).expect("standalone parse succeeds");
    assert_eq!(
        r,
        SsixRecord {
            subsegments: vec![SsixSubsegment {
                ranges: vec![
                    SsixRange {
                        level: 0,
                        range_size: 64
                    },
                    SsixRange {
                        level: 1,
                        range_size: 2_048
                    },
                ],
            }],
        }
    );

    // The splice must not perturb packet demuxing.
    let mut count = 0;
    while dmx.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 2, "still demuxes 2 packets with ssix spliced");
}

#[test]
fn two_ssix_boxes_enumerate_in_file_order() {
    let bytes = mux_fragmented_pcm_to_bytes();

    // First segment: 1 subsegment × 2 ranges. Second segment: 1
    // subsegment × 3 ranges (so the two summaries are distinguishable).
    let ssix_a = build_ssix_box_manual(&[&[(0, 100), (1, 900)]]);
    let ssix_b = build_ssix_box_manual(&[&[(0, 50), (1, 150), (2, 800)]]);

    // Splice after the SECOND sidx first so the first splice's byte
    // shift cannot invalidate the second sidx's scan position.
    let spliced = splice_ssix_after_sidx(&bytes, &ssix_b, 1);
    let spliced = splice_ssix_after_sidx(&spliced, &ssix_a, 0);

    let dmx = open_bytes(spliced);
    let md = dmx.metadata();
    let get = |key: &str| {
        md.iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("{key} metadata key missing"))
            .1
            .clone()
    };
    assert_eq!(get("ssix_0"), "1 2");
    assert_eq!(get("ssix_1"), "1 3");
}

#[test]
fn absent_ssix_emits_no_keys() {
    let dmx = open_bytes(mux_fragmented_pcm_to_bytes());
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k.starts_with("ssix_")),
        "no ssix box → no ssix_* metadata keys"
    );
}

#[test]
fn malformed_ssix_is_dropped_not_fatal() {
    let bytes = mux_fragmented_pcm_to_bytes();

    // Declare 2 subsegments but supply only one range list — the body
    // ends mid-table, so the parser reports truncation and the walk
    // drops the box (ssix is informational, not load-bearing).
    let good = build_ssix_box_manual(&[&[(0, 10), (1, 20)]]);
    let mut truncated_body = good[8..].to_vec();
    truncated_body[4..8].copy_from_slice(&2u32.to_be_bytes()); // subsegment_count = 2
    let total = 8 + truncated_body.len();
    let mut bad = Vec::with_capacity(total);
    bad.extend_from_slice(&(total as u32).to_be_bytes());
    bad.extend_from_slice(b"ssix");
    bad.extend_from_slice(&truncated_body);

    let spliced = splice_ssix_after_sidx(&bytes, &bad, 0);
    let mut dmx = open_bytes(spliced);
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k.starts_with("ssix_")),
        "malformed ssix is dropped, not surfaced"
    );
    let mut count = 0;
    while dmx.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 2, "malformed ssix must not break packet demuxing");
}

// --- standalone parser / builder edge cases -------------------------------

#[test]
fn zero_subsegments_parse_to_empty_record() {
    let body = build_ssix_box_manual(&[])[8..].to_vec();
    let r = parse_ssix_box(&body).unwrap();
    assert!(r.subsegments.is_empty());
}

#[test]
fn body_below_floor_is_rejected() {
    // 7 bytes — less than FullBox preamble + subsegment_count.
    assert!(parse_ssix_box(&[0u8; 7]).is_err());
}

#[test]
fn nonzero_version_is_rejected() {
    // Only version 0 is defined (§8.16.4.2); a future version would
    // carry an unknown layout, so mis-reading it as v0 is worse than
    // refusing.
    let mut body = build_ssix_box_manual(&[&[(0, 1), (1, 1)]])[8..].to_vec();
    body[0] = 1;
    assert!(parse_ssix_box(&body).is_err());
}

#[test]
fn truncated_range_list_is_rejected() {
    // range_count = 3 but only two 4-byte records present.
    let mut body = build_ssix_box_manual(&[&[(0, 1), (1, 1)]])[8..].to_vec();
    body[8..12].copy_from_slice(&3u32.to_be_bytes());
    assert!(parse_ssix_box(&body).is_err());
}

#[test]
fn missing_range_count_is_rejected() {
    // subsegment_count = 1 but the body ends before its range_count.
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
    body.extend_from_slice(&1u32.to_be_bytes()); // subsegment_count = 1
    assert!(parse_ssix_box(&body).is_err());
}

#[test]
fn builder_round_trips_and_rejects_oversize_range() {
    let record = SsixRecord {
        subsegments: vec![SsixSubsegment {
            ranges: vec![
                SsixRange {
                    level: 3,
                    range_size: 0xFF_FFFF, // u24 ceiling survives
                },
                SsixRange {
                    level: 4,
                    range_size: 1,
                },
            ],
        }],
    };
    let boxed = build_ssix_box(&record).unwrap();
    assert_eq!(&boxed[4..8], b"ssix");
    assert_eq!(parse_ssix_box(&boxed[8..]).unwrap(), record);

    let too_big = SsixRecord {
        subsegments: vec![SsixSubsegment {
            ranges: vec![SsixRange {
                level: 0,
                range_size: 0x100_0000, // one past the 24-bit field
            }],
        }],
    };
    assert!(build_ssix_box(&too_big).is_err());
}

/// Round 351 capstone — the fragmented muxer emits a real `sidx`+`ssix`
/// pair (not a spliced one) when `FragmentedOptions::emit_ssix` is set.
///
/// This is the write/read symmetry test for `ssix`: mux a two-fragment
/// PCM stream with `emit_ssix = true`, then for each fragment locate the
/// muxer-written `sidx` and the `ssix` that immediately follows it,
/// parse both with the public entry points, and confirm that
///
/// * the `ssix` carries `subsegment_count == 1` (matching the
///   one-reference `sidx`, per §8.16.4.1),
/// * the subsegment has exactly two ranges (§8.16.4 "range_count >= 2"),
/// * the two ranges carry the configured `(metadata_level, media_level)`
///   numbers, and
/// * the ranges **partition** the subsegment — their sizes sum exactly
///   to the `sidx`'s `referenced_size`, with the trailing range equal to
///   the `mdat` size.
fn mux_with_emit_ssix(levels: Vec<LevaEntry>, ssix_levels: (u8, u8)) -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let mut opts = fragmented_options();
    if let Some(frag) = opts.fragmented.as_mut() {
        frag.levels = levels;
        frag.emit_ssix = true;
        frag.ssix_levels = ssix_levels;
        // Keep styp off so the subsegment is exactly moof + mdat — makes
        // the range-partition arithmetic in the assertions explicit.
        frag.styp = None;
    }
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-ssix-mux-r351-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
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
fn muxer_emitted_ssix_partitions_each_fragment_subsegment() {
    // Two-level config (a level-1 metadata level + a level-2 media level),
    // the same numbers the muxer labels the two ssix ranges with.
    let levels = vec![
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
            padding_flag: false,
            assignment_type: 2,
            grouping_type: 0,
            grouping_type_parameter: 0,
            sub_track_id: 0,
        },
    ];
    let bytes = mux_with_emit_ssix(levels, (1, 2));

    // Two fragments → two sidx, each immediately followed by an ssix.
    for nth in 0..2 {
        let sidx_type = find_fourcc_nth(&bytes, b"sidx", nth).expect("sidx present");
        let sidx_size_pos = sidx_type - 4;
        let sidx_size =
            u32::from_be_bytes(bytes[sidx_size_pos..sidx_size_pos + 4].try_into().unwrap())
                as usize;
        let sidx_end = sidx_size_pos + sidx_size;

        // The very next box must be the ssix (§8.16.4.1 placement).
        assert_eq!(
            &bytes[sidx_end + 4..sidx_end + 8],
            b"ssix",
            "ssix must immediately follow its sidx (fragment {nth})"
        );
        let ssix_size =
            u32::from_be_bytes(bytes[sidx_end..sidx_end + 4].try_into().unwrap()) as usize;

        // Parse the sidx to recover its single reference's size, which is
        // the size of the subsegment the ssix partitions.
        let sidx_body = &bytes[sidx_type + 4..sidx_end];
        let sidx = parse_sidx_box(sidx_body, sidx_end as u64)
            .expect("sidx body parses")
            .expect("sidx has a record");
        assert_eq!(sidx.references.len(), 1, "muxer emits a one-reference sidx");
        let subsegment_size = sidx.references[0].referenced_size as u64;

        // Parse the ssix the muxer wrote.
        let ssix_body = &bytes[sidx_end + 8..sidx_end + ssix_size];
        let r = parse_ssix_box(ssix_body).expect("ssix body parses");
        assert_eq!(r.subsegments.len(), 1, "ssix subsegment_count == 1");
        let ranges = &r.subsegments[0].ranges;
        assert_eq!(ranges.len(), 2, "two ranges (metadata + media)");
        assert_eq!(ranges[0].level, 1, "first range is the metadata level");
        assert_eq!(ranges[1].level, 2, "second range is the media level");

        // Range partition: the two ranges sum to the whole subsegment.
        let sum = ranges[0].range_size as u64 + ranges[1].range_size as u64;
        assert_eq!(
            sum, subsegment_size,
            "ranges must partition the subsegment exactly (fragment {nth})"
        );
        // The metadata range is non-empty (it holds at least the moof).
        assert!(ranges[0].range_size > 0, "metadata range non-empty");
        // The media range equals the mdat size: mdat is the trailing box
        // of the subsegment, and the subsegment here is moof + mdat with
        // styp suppressed, so range[1] == subsegment - moof.
        assert!(ranges[1].range_size > 0, "media range non-empty");
    }

    // The demuxer surfaces a `ssix_<n>` metadata summary per box, in file
    // order — the muxer's output is genuinely re-parseable as ssix.
    let dmx = open_bytes(bytes);
    let md = dmx.metadata().to_vec();
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "ssix_0")
            .map(|(_, v)| v.as_str()),
        Some("1 2"),
        "first muxer-emitted ssix: 1 subsegment, 2 ranges"
    );
    assert_eq!(
        md.iter()
            .find(|(k, _)| k == "ssix_1")
            .map(|(_, v)| v.as_str()),
        Some("1 2"),
        "second muxer-emitted ssix: 1 subsegment, 2 ranges"
    );
}

#[test]
fn emit_ssix_false_writes_no_ssix() {
    // Default `emit_ssix = false`: a fragmented file with sidx emission on
    // but emit_ssix off carries no ssix box at all.
    let bytes = mux_fragmented_pcm_to_bytes();
    assert!(
        find_fourcc_nth(&bytes, b"ssix", 0).is_none(),
        "no ssix should be written when emit_ssix is false"
    );
    let dmx = open_bytes(bytes);
    assert!(
        !dmx.metadata().iter().any(|(k, _)| k.starts_with("ssix")),
        "no ssix metadata when emit_ssix is false"
    );
}

#[test]
fn muxer_emitted_ssix_honours_custom_level_numbers() {
    // Custom (metadata_level, media_level) = (7, 9) flows through to the
    // emitted ssix's range labels.
    let bytes = mux_with_emit_ssix(Vec::new(), (7, 9));
    let sidx_type = find_fourcc_nth(&bytes, b"sidx", 0).expect("sidx present");
    let sidx_size_pos = sidx_type - 4;
    let sidx_size =
        u32::from_be_bytes(bytes[sidx_size_pos..sidx_size_pos + 4].try_into().unwrap()) as usize;
    let sidx_end = sidx_size_pos + sidx_size;
    let ssix_size = u32::from_be_bytes(bytes[sidx_end..sidx_end + 4].try_into().unwrap()) as usize;
    let ssix_body = &bytes[sidx_end + 8..sidx_end + ssix_size];
    let r = parse_ssix_box(ssix_body).expect("ssix body parses");
    let ranges = &r.subsegments[0].ranges;
    assert_eq!(ranges[0].level, 7);
    assert_eq!(ranges[1].level, 9);
}
