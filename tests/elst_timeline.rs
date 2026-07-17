//! Integration tests for the full ISO/IEC 14496-12 §8.6.6 edit-list
//! timeline mapping on the plain (non-fragmented) demux path.
//!
//! Strategy: build a synthetic MP4 byte-by-byte (no external tooling at
//! run time) with a moov-resident sample table plus an `edts/elst` of a
//! known shape, then assert the demuxed per-packet `(pts, dts, discard)`
//! matches the §8.6.6 mapping:
//!
//! * an initial trim (`media_time > 0`) subtracts the trim point and
//!   flags earlier samples as decode pre-roll (`discard`),
//! * a leading empty edit (`media_time = -1`) pushes the presentation
//!   timeline out by its (movie-timescale) duration,
//! * a dwell (`media_rate_integer = 0`) inserts presentation time
//!   without consuming media,
//! * media excised between two segments is delivered with `discard`
//!   set,
//! * v0 and v1 entry layouts decode identically.
//!
//! A PATH-gated black-box cross-check runs `ffprobe` (as an opaque CLI)
//! over a muxer-produced start-delay file and asserts the reported
//! first pts equals the delay — i.e. an independent reader interprets
//! the elst the same way this demuxer does.

use std::io::Cursor;

use oxideav_core::{Error, ReadSeek};

// --- Box-builder helpers -------------------------------------------------

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"isom");
    body.extend_from_slice(&512u32.to_be_bytes());
    body.extend_from_slice(b"isom");
    body.extend_from_slice(b"mp41");
    boxed(b"ftyp", &body)
}

fn mvhd(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 100];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..24].copy_from_slice(&0x00010000u32.to_be_bytes()); // rate
    body[24..26].copy_from_slice(&0x0100u16.to_be_bytes()); // volume
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[36 + i * 4..36 + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    body[96..100].copy_from_slice(&2u32.to_be_bytes()); // next_track_ID
    boxed(b"mvhd", &body)
}

fn tkhd_audio(track_id: u32) -> Vec<u8> {
    let mut body = vec![0u8; 80];
    body[1..4].copy_from_slice(&[0, 0, 0x07]); // enabled | in-movie
    body[12..16].copy_from_slice(&track_id.to_be_bytes());
    body[36..38].copy_from_slice(&0x0100u16.to_be_bytes()); // volume
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[40 + i * 4..40 + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    boxed(b"tkhd", &body)
}

fn mdhd_audio(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..22].copy_from_slice(&0x55C4u16.to_be_bytes()); // "und"
    boxed(b"mdhd", &body)
}

fn hdlr_soun() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(b"soun");
    body.extend_from_slice(&[0u8; 12]);
    body.extend_from_slice(b"audio\0");
    boxed(b"hdlr", &body)
}

fn smhd() -> Vec<u8> {
    boxed(b"smhd", &[0u8; 8])
}

fn dinf_dref() -> Vec<u8> {
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&[0u8; 4]);
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    let url = boxed(b"url ", &[0, 0, 0, 1]);
    dref_body.extend_from_slice(&url);
    boxed(b"dinf", &boxed(b"dref", &dref_body))
}

fn stsd_sowt(channels: u16, sample_rate: u32) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes());
    entry[16..18].copy_from_slice(&channels.to_be_bytes());
    entry[18..20].copy_from_slice(&16u16.to_be_bytes());
    entry[24..28].copy_from_slice(&(sample_rate << 16).to_be_bytes());
    let entry_box = boxed(b"sowt", &entry);
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&entry_box);
    boxed(b"stsd", &body)
}

/// `stts` with one run: `count` samples of `delta` ticks each.
fn stts_uniform(count: u32, delta: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    body.extend_from_slice(&delta.to_be_bytes());
    boxed(b"stts", &body)
}

/// `stsc` with one entry: every chunk carries `spc` samples.
fn stsc_single(spc: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
    body.extend_from_slice(&spc.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    boxed(b"stsc", &body)
}

/// `stsz` with a constant `sample_size` for `count` samples.
fn stsz_uniform(count: u32, size: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&size.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    boxed(b"stsz", &body)
}

/// `stco` with a single chunk offset.
fn stco_single(offset: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&offset.to_be_bytes());
    boxed(b"stco", &body)
}

/// `elst` v0 from `(segment_duration, media_time, media_rate_integer)`
/// triples.
fn elst_v0(entries: &[(u32, i32, u16)]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(dur, mt, rate_int) in entries {
        body.extend_from_slice(&dur.to_be_bytes());
        body.extend_from_slice(&mt.to_be_bytes());
        body.extend_from_slice(&rate_int.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes()); // media_rate_fraction
    }
    boxed(b"elst", &body)
}

/// `elst` v1 (64-bit widths) from the same triples.
fn elst_v1(entries: &[(u64, i64, u16)]) -> Vec<u8> {
    let mut body = vec![1u8, 0, 0, 0];
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(dur, mt, rate_int) in entries {
        body.extend_from_slice(&dur.to_be_bytes());
        body.extend_from_slice(&mt.to_be_bytes());
        body.extend_from_slice(&rate_int.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
    }
    boxed(b"elst", &body)
}

/// Assemble a complete one-track PCM file: `count` samples of
/// `sample_dur` ticks / `sample_size` bytes each, with the given
/// (possibly empty) `edts` payload spliced between `tkhd` and `mdia`.
fn build_file(
    movie_timescale: u32,
    media_timescale: u32,
    count: u32,
    sample_dur: u32,
    edts: &[u8],
) -> Vec<u8> {
    const SAMPLE_SIZE: u32 = 4;
    let build_moov = |chunk_off: u32| -> Vec<u8> {
        let mut stbl = Vec::new();
        stbl.extend_from_slice(&stsd_sowt(2, media_timescale));
        stbl.extend_from_slice(&stts_uniform(count, sample_dur));
        stbl.extend_from_slice(&stsc_single(count));
        stbl.extend_from_slice(&stsz_uniform(count, SAMPLE_SIZE));
        stbl.extend_from_slice(&stco_single(chunk_off));
        let stbl = boxed(b"stbl", &stbl);
        let mut minf = Vec::new();
        minf.extend_from_slice(&smhd());
        minf.extend_from_slice(&dinf_dref());
        minf.extend_from_slice(&stbl);
        let minf = boxed(b"minf", &minf);
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&mdhd_audio(media_timescale));
        mdia.extend_from_slice(&hdlr_soun());
        mdia.extend_from_slice(&minf);
        let mdia = boxed(b"mdia", &mdia);
        let mut trak = Vec::new();
        trak.extend_from_slice(&tkhd_audio(1));
        trak.extend_from_slice(edts);
        trak.extend_from_slice(&mdia);
        let trak = boxed(b"trak", &trak);
        let mut moov = Vec::new();
        moov.extend_from_slice(&mvhd(movie_timescale));
        moov.extend_from_slice(&trak);
        boxed(b"moov", &moov)
    };
    // Two-pass: moov size does not depend on the stco value's width.
    let ftyp = ftyp();
    let moov_len = build_moov(0).len();
    let mdat_payload_off = (ftyp.len() + moov_len + 8) as u32;
    let moov = build_moov(mdat_payload_off);
    let mut mdat_body = Vec::new();
    for i in 0..count {
        mdat_body.extend_from_slice(&[i as u8; SAMPLE_SIZE as usize]);
    }
    let mut file = Vec::new();
    file.extend_from_slice(&ftyp);
    file.extend_from_slice(&moov);
    file.extend_from_slice(&boxed(b"mdat", &mdat_body));
    file
}

/// Demux every packet and return `(pts, dts, discard)` triples.
fn demux_timing(file: Vec<u8>) -> Vec<(i64, i64, bool)> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((p.pts.unwrap(), p.dts.unwrap(), p.flags.discard)),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    out
}

// --- Tests ---------------------------------------------------------------

/// Initial trim: `media_time = 200` maps composition time 200 to
/// presentation 0; the two earlier samples are decode pre-roll.
#[test]
fn initial_trim_shifts_and_flags_preroll() {
    let edts = boxed(b"edts", &elst_v0(&[(400, 200, 1)]));
    // 6 samples × 100 ticks; movie ts == media ts (1:1 rescale).
    let got = demux_timing(build_file(1000, 1000, 6, 100, &edts));
    assert_eq!(
        got,
        vec![
            (-200, -200, true),
            (-100, -100, true),
            (0, 0, false),
            (100, 100, false),
            (200, 200, false),
            // Past the elst's declared 400-tick end: tolerated
            // (implicit trailing empty edit is a movie-timeline
            // construct, not a sample drop).
            (300, 300, false),
        ]
    );
}

/// Leading empty edit + trim, across differing timescales: 300 movie
/// ticks @ 1000 = 14400 media ticks @ 48000 of presentation delay, then
/// the segment maps media_time 100 there.
#[test]
fn empty_edit_delay_plus_trim_rescales() {
    let edts = boxed(b"edts", &elst_v0(&[(300, -1, 1), (500, 100, 1)]));
    let got = demux_timing(build_file(1000, 48_000, 3, 100, &edts));
    // delta = 14400 - 100 = 14300.
    assert_eq!(
        got,
        vec![
            (14_300, 14_300, true), // cts 0 < media_time 100: pre-roll
            (14_400, 14_400, false),
            (14_500, 14_500, false),
        ]
    );
}

/// A dwell (`media_rate_integer = 0`) inserts presentation time
/// without consuming media: samples after the dwell point shift out by
/// its duration.
#[test]
fn dwell_pushes_later_segment_out() {
    // Segment A: media [0, 200). Dwell: 100 ticks holding media 200.
    // Segment B: media [200, 400) starting at presentation 300.
    let edts = boxed(
        b"edts",
        &elst_v0(&[(200, 0, 1), (100, 200, 0), (200, 200, 1)]),
    );
    let got = demux_timing(build_file(1000, 1000, 4, 100, &edts));
    assert_eq!(
        got,
        vec![
            (0, 0, false),
            (100, 100, false),
            (300, 300, false), // +100 dwell insert
            (400, 400, false),
        ]
    );
}

/// Media excised between two segments (with an inserted empty edit
/// keeping presentation deltas non-decreasing): the excised samples
/// are delivered with the discard flag, timestamps extrapolated from
/// the preceding segment.
#[test]
fn interior_excision_flags_discard() {
    // Segment A: media [0, 200) → pres [0, 200). Empty edit: 300.
    // Segment B: media [400, 600) → pres [500, 700). Media [200, 400)
    // is excised.
    let edts = boxed(
        b"edts",
        &elst_v0(&[(200, 0, 1), (300, -1, 1), (200, 400, 1)]),
    );
    let got = demux_timing(build_file(1000, 1000, 8, 100, &edts));
    assert_eq!(
        got,
        vec![
            (0, 0, false),
            (100, 100, false),
            (200, 200, true), // excised
            (300, 300, true), // excised
            (500, 500, false),
            (600, 600, false),
            (700, 700, false), // past declared end: tolerated
            (800, 800, false),
        ]
    );
}

/// The v1 (64-bit) entry layout decodes to the same mapping as v0.
#[test]
fn elst_v1_layout_maps_identically() {
    let edts_0 = boxed(b"edts", &elst_v0(&[(300, -1, 1), (500, 100, 1)]));
    let edts_1 = boxed(b"edts", &elst_v1(&[(300, -1, 1), (500, 100, 1)]));
    let a = demux_timing(build_file(1000, 48_000, 3, 100, &edts_0));
    let b = demux_timing(build_file(1000, 48_000, 3, 100, &edts_1));
    assert_eq!(a, b, "v0 and v1 elst must map identically");
}

/// A media-reordering edit list (presentation delta would move
/// backwards) falls back to the single leading-shift mapping so DTS
/// stays monotonic.
#[test]
fn non_monotonic_list_falls_back_to_leading_shift() {
    // Excision with no compensating empty edit: delta drops 0 → -200.
    let edts = boxed(b"edts", &elst_v0(&[(200, 0, 1), (200, 400, 1)]));
    let got = demux_timing(build_file(1000, 1000, 4, 100, &edts));
    // Leading shift = first non-empty media_time = 0 → identity.
    assert_eq!(
        got,
        vec![
            (0, 0, false),
            (100, 100, false),
            (200, 200, false),
            (300, 300, false),
        ]
    );
}

/// Hostile v1 magnitudes: u64::MAX durations and extreme media_times
/// must open and demux without panicking (saturating math end-to-end).
#[test]
fn hostile_giant_elst_values_do_not_panic() {
    let shapes: Vec<Vec<(u64, i64, u16)>> = vec![
        vec![(u64::MAX, -1, 1), (u64::MAX, 10, 1)],
        vec![(u64::MAX, i64::MAX, 1)],
        vec![(0, i64::MAX, 1)],
        vec![(u64::MAX, i64::MIN + 1, 1)],
        vec![(u64::MAX, 0, 0)], // giant dwell
    ];
    for entries in shapes {
        let edts = boxed(b"edts", &elst_v1(&entries));
        let file = build_file(1, 48_000, 3, 100, &edts);
        let _ = demux_timing(file); // must not panic
    }
}

/// The typed `Mp4Demuxer::edit_list` accessor and the flat
/// `elst_entry_count` / `elst_<n>` options keys surface the declared
/// list verbatim, and feeding the typed slice back through
/// `build_elst_box` reproduces the on-the-wire entries.
#[test]
fn edit_list_surfaced_typed_and_flat() {
    use oxideav_mp4::demux::{build_elst_box, parse_elst_box, EditListEntry};

    let edts = boxed(b"edts", &elst_v0(&[(300, -1, 1), (500, 100, 1)]));
    let file = build_file(1000, 48_000, 3, 100, &edts);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();

    let expect = vec![
        EditListEntry {
            segment_duration: 300,
            media_time: -1,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        },
        EditListEntry {
            segment_duration: 500,
            media_time: 100,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        },
    ];
    assert_eq!(dmx.edit_list(0), expect.as_slice());
    assert!(dmx.edit_list(7).is_empty(), "out-of-range index is empty");

    use oxideav_core::Demuxer;
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("elst_entry_count"), Some("2"));
    assert_eq!(opts.get("elst_0"), Some("dur=300 media_time=-1 rate=1"));
    assert_eq!(opts.get("elst_1"), Some("dur=500 media_time=100 rate=1"));

    // Remux path: typed slice → build_elst_box → parse_elst_box is the
    // identity.
    let rebuilt = build_elst_box(dmx.edit_list(0)).unwrap();
    assert_eq!(parse_elst_box(&rebuilt[8..]).unwrap(), expect);
}

/// A track without `edts` emits neither the typed entries nor the
/// flat keys.
#[test]
fn no_elst_no_surface() {
    let file = build_file(1000, 48_000, 3, 100, &[]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert!(dmx.edit_list(0).is_empty());
    use oxideav_core::Demuxer;
    let opts = &dmx.streams()[0].params.options;
    assert!(opts.get("elst_entry_count").is_none());
    assert!(opts.get("elst_0").is_none());
}

/// Black-box cross-check: an independent reader (`ffprobe`, invoked as
/// an opaque CLI) reports the same first-packet presentation time for a
/// muxer-produced start-delay file as this crate's own demuxer.
/// Skipped when `ffprobe` is not on `$PATH`.
#[test]
fn ffprobe_agrees_on_start_delay() {
    use oxideav_core::{
        CodecId, CodecParameters, MediaType, Packet, StreamInfo, TimeBase, WriteSeek,
    };
    use std::process::Command;

    if Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: ffprobe not on PATH");
        return;
    }

    // Mux 3 PCM packets starting at pts 24_000 @ 48 kHz (= 0.5 s).
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.media_type = MediaType::Audio;
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-elst-ffprobe-{}.mp4",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..3i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 1024 * 4]);
            pkt.pts = Some(24_000 + i * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Our own demuxer: first pts must be 24_000 (0.5 s).
    let bytes = std::fs::read(&tmp).unwrap();
    let ours = demux_timing(bytes);
    assert_eq!(ours[0].0, 24_000, "our demuxer maps the start delay");

    // Black-box reader: first packet pts_time must be 0.5.
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-show_packets",
            "-select_streams",
            "a:0",
            "-of",
            "csv=p=0",
            "-show_entries",
            "packet=pts_time",
        ])
        .arg(&tmp)
        .output()
        .expect("run ffprobe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .expect("ffprobe reported at least one packet")
        .trim()
        .trim_end_matches(',')
        .to_string();
    let first: f64 = first.parse().expect("pts_time parses as float");
    assert!(
        (first - 0.5).abs() < 1e-6,
        "independent reader sees the 0.5 s start delay, got {first}"
    );
    let _ = std::fs::remove_file(&tmp);
}

// --- Mux-side explicit edit lists ----------------------------------------

mod mux_explicit {
    use super::demux_timing;
    use oxideav_core::{
        CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
    };
    use oxideav_mp4::demux::EditListEntry;
    use oxideav_mp4::{Mp4MuxerOptions, TrackEditList};
    use std::io::Cursor;

    fn pcm_stream() -> StreamInfo {
        let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
        params.channels = Some(2);
        params.sample_rate = Some(48_000);
        StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params,
        }
    }

    fn entry(dur: u64, mt: i64, rate: i16) -> EditListEntry {
        EditListEntry {
            segment_duration: dur,
            media_time: mt,
            media_rate_integer: rate,
            media_rate_fraction: 0,
        }
    }

    /// Mux 3 PCM packets (first pts `start_pts`) with `opts`, return
    /// the produced bytes.
    fn mux_pcm(start_pts: i64, opts: Mp4MuxerOptions) -> Vec<u8> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stream = pcm_stream();
        let tmp = std::env::temp_dir().join(format!(
            "oxideav-mp4-elst-explicit-{}-{n}.mp4",
            std::process::id()
        ));
        {
            let f = std::fs::File::create(&tmp).unwrap();
            let ws: Box<dyn WriteSeek> = Box::new(f);
            let mut mux =
                oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts)
                    .unwrap();
            mux.write_header().unwrap();
            for i in 0..3i64 {
                let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 1024 * 4]);
                pkt.pts = Some(start_pts + i * 1024);
                pkt.duration = Some(1024);
                pkt.flags.keyframe = true;
                mux.write_packet(&pkt).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let bytes = std::fs::read(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);
        bytes
    }

    /// An explicit edit list is emitted verbatim and drives the demux
    /// timeline: a 250-movie-tick (0.25 s = 12000 @ 48 kHz) empty edit
    /// delays every packet.
    #[test]
    fn explicit_list_muxed_and_recovered() {
        let entries = vec![entry(250, -1, 1), entry(64, 0, 1)];
        let opts = Mp4MuxerOptions {
            track_edit_lists: vec![TrackEditList {
                stream_index: 0,
                entries: entries.clone(),
            }],
            ..Mp4MuxerOptions::default()
        };
        let bytes = mux_pcm(0, opts);
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
        let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
        assert_eq!(
            dmx.edit_list(0),
            entries.as_slice(),
            "declared list recovered verbatim"
        );
        let got = demux_timing(bytes);
        // 250 movie ticks @ 1000 = 12000 media ticks @ 48000.
        assert_eq!(got[0], (12_000, 12_000, false));
        assert_eq!(got[1], (13_024, 13_024, false));
    }

    /// An explicit list overrides the automatic start-delay emission:
    /// a plain media_time = 0 list on a delayed input yields unshifted
    /// output timestamps (the automatic form would have re-created the
    /// 24_000-tick delay).
    #[test]
    fn explicit_list_overrides_automatic() {
        let opts = Mp4MuxerOptions {
            track_edit_lists: vec![TrackEditList {
                stream_index: 0,
                entries: vec![entry(64, 0, 1)],
            }],
            ..Mp4MuxerOptions::default()
        };
        let got = demux_timing(mux_pcm(24_000, opts));
        assert_eq!(
            got[0].0, 0,
            "explicit media_time=0 list suppressed the delay"
        );
    }

    /// An explicit list is written even when `write_edit_list` is
    /// `false` — the flag governs only the automatic emission.
    #[test]
    fn explicit_list_ignores_write_edit_list_flag() {
        let entries = vec![entry(250, -1, 1), entry(64, 0, 1)];
        let opts = Mp4MuxerOptions {
            write_edit_list: false,
            track_edit_lists: vec![TrackEditList {
                stream_index: 0,
                entries,
            }],
            ..Mp4MuxerOptions::default()
        };
        let got = demux_timing(mux_pcm(0, opts));
        assert_eq!(got[0].0, 12_000, "explicit list written despite the flag");
    }

    /// §8.6.6.3 violations and bad stream indices fail at `open`, not
    /// at `write_trailer`.
    #[test]
    fn invalid_explicit_list_fails_at_open() {
        let stream = pcm_stream();
        let open_with = |tel: TrackEditList| {
            let opts = Mp4MuxerOptions {
                track_edit_lists: vec![tel],
                ..Mp4MuxerOptions::default()
            };
            let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts)
                .map(|_| ())
        };
        // Trailing empty edit (§8.6.6.3).
        assert!(open_with(TrackEditList {
            stream_index: 0,
            entries: vec![entry(64, 0, 1), entry(100, -1, 1)],
        })
        .is_err());
        // media_rate_integer outside {0, 1}.
        assert!(open_with(TrackEditList {
            stream_index: 0,
            entries: vec![entry(64, 0, 2)],
        })
        .is_err());
        // Empty entry list.
        assert!(open_with(TrackEditList {
            stream_index: 0,
            entries: vec![],
        })
        .is_err());
        // Out-of-range stream index.
        assert!(open_with(TrackEditList {
            stream_index: 3,
            entries: vec![entry(64, 0, 1)],
        })
        .is_err());
    }
}

// --- Fragmented muxer explicit edit lists --------------------------------

mod mux_explicit_fragmented {
    use super::demux_timing;
    use oxideav_core::{
        CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
    };
    use oxideav_mp4::demux::EditListEntry;
    use oxideav_mp4::{
        BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions, TrackEditList,
    };
    use std::io::Cursor;

    /// The §8.6.6.1 zero-duration fragmented idiom: an explicit
    /// `media_time = 1024, segment_duration = 0` trim in the init
    /// segment applies to every subsequent movie fragment ("the edit
    /// provides the offset for the movie and subsequent movie
    /// fragments") — the shape a packager uses to cut priming
    /// samples ahead of the audio a CMAF track actually presents.
    #[test]
    fn fragmented_open_ended_trim_spans_fragments() {
        let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
        params.channels = Some(2);
        params.sample_rate = Some(48_000);
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params,
        };
        let entries = vec![EditListEntry {
            segment_duration: 0,
            media_time: 1024,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }];
        let opts = Mp4MuxerOptions {
            fragmented: Some(FragmentedOptions {
                cadence: FragmentCadence::EveryNPackets(2),
                styp: Some(BrandPreset::Custom {
                    major: *b"msdh",
                    compatible: vec![*b"msdh", *b"msix"],
                }),
                ..FragmentedOptions::default()
            }),
            track_edit_lists: vec![TrackEditList {
                stream_index: 0,
                entries: entries.clone(),
            }],
            ..Mp4MuxerOptions::default()
        };
        let tmp =
            std::env::temp_dir().join(format!("oxideav-mp4-elst-frag-{}.mp4", std::process::id()));
        {
            let f = std::fs::File::create(&tmp).unwrap();
            let ws: Box<dyn WriteSeek> = Box::new(f);
            let mut mux =
                oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts)
                    .unwrap();
            mux.write_header().unwrap();
            // 4 packets × 1024 ticks → 2 fragments of 2 packets each.
            for i in 0..4i64 {
                let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 1024 * 4]);
                pkt.pts = Some(i * 1024);
                pkt.duration = Some(1024);
                pkt.flags.keyframe = true;
                mux.write_packet(&pkt).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let bytes = std::fs::read(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);

        // Typed accessor recovers the declared list from the init moov.
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
        let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
        assert_eq!(dmx.edit_list(0), entries.as_slice());

        // The trim spans both fragments: packet 0 (cts 0) is priming
        // pre-roll (discard, pts -1024), packet 1 (cts 1024) is the
        // first presented sample at pts 0, and packets 2..3 — in the
        // SECOND fragment — keep the same -1024 delta.
        let got = demux_timing(bytes);
        assert_eq!(
            got,
            vec![
                (-1024, -1024, true),
                (0, 0, false),
                (1024, 1024, false),
                (2048, 2048, false),
            ]
        );
    }
}

// --- Timeline-consistency: start_time + seek -----------------------------

/// `StreamInfo::start_time` reflects the mapped presentation start:
/// a 300-movie-tick empty edit at movie ts 1000 / media ts 48000 puts
/// the first presented sample at 14400.
#[test]
fn stream_start_time_reflects_delay() {
    let edts = boxed(b"edts", &elst_v0(&[(300, -1, 1), (500, 100, 1)]));
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file(1000, 48_000, 3, 100, &edts)));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].start_time, Some(14_400));

    // No elst → 0 as before.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(build_file(1000, 48_000, 3, 100, &[])));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].start_time, Some(0));
}

/// `seek_to` operates on the mapped presentation timeline: seeking to
/// a post-trim pts lands on the sample whose *mapped* pts is at or
/// before it, and the packet read after the seek carries that pts.
#[test]
fn seek_uses_mapped_timeline() {
    // Trim at 200: samples map to pts -200, -100, 0, 100, 200, 300.
    let edts = boxed(b"edts", &elst_v0(&[(400, 200, 1)]));
    let file = build_file(1000, 1000, 6, 100, &edts);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let landed = dmx.seek_to(0, 150).unwrap();
    assert_eq!(landed, 100, "nearest sync sample at-or-before mapped 150");
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.pts, Some(100));
    // Seeking to the mapped start lands on pts 0 (not the pre-roll).
    let landed = dmx.seek_to(0, 0).unwrap();
    assert_eq!(landed, 0);
}
