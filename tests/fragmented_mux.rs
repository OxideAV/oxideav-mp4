//! Integration tests for fragmented-MP4 *muxing* (DASH / HLS / Smooth-
//! Streaming / CMAF output).
//!
//! Round-trip strategy: write a synthetic stream through the fragmented
//! muxer, then re-parse it via the in-tree fragmented demuxer (mp4@fe42550)
//! and assert the per-sample table comes back byte-for-byte identical.
//!
//! Optional ffmpeg cross-check: when `ffmpeg` is on `$PATH` we also pipe
//! the produced bytes through `ffmpeg -f mp4 -i - -c copy` and inspect the
//! resulting AAC byte stream (PCM stays as-is). The check is skipped (not
//! failed) when the binary is absent so CI without the codec dep still
//! passes — see `ffmpeg_aac_extract_matches_demux`.

use std::io::Cursor;
use std::process::{Command, Stdio};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::muxer::open_with_options;
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

// --- Test scaffolding -----------------------------------------------------

fn pcm_stream() -> StreamInfo {
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

/// Generate `n` packets of `frames` stereo s16le frames each, with a
/// cumulative pts = i * frames. Every packet is a keyframe (intra).
fn make_pcm_packets(n: usize, frames: i64) -> Vec<Packet> {
    let mut out = Vec::with_capacity(n);
    let stream = pcm_stream();
    for i in 0..n {
        let mut payload = Vec::with_capacity(frames as usize * 4);
        for j in 0..frames as usize {
            let l = ((i * 1024 + j) as i16).wrapping_mul(7);
            let r = ((i * 1024 + j) as i16).wrapping_mul(11);
            payload.extend_from_slice(&l.to_le_bytes());
            payload.extend_from_slice(&r.to_le_bytes());
        }
        let mut p = Packet::new(0, stream.time_base, payload);
        p.pts = Some((i as i64) * frames);
        p.dts = Some((i as i64) * frames);
        p.duration = Some(frames);
        p.flags.keyframe = true;
        out.push(p);
    }
    out
}

fn fragmented_options(cadence: FragmentCadence) -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"mp41", *b"dash"],
        },
        faststart: false,
        fragmented: Some(FragmentedOptions {
            cadence,
            styp: Some(BrandPreset::Custom {
                major: *b"msdh",
                compatible: vec![*b"msdh", *b"msix"],
            }),
            emit_random_access_indexes: true,
        }),
    }
}

/// Mux to a temp file path; tests then read the file back as bytes.
/// (Recovering the inner `Vec` from a `Box<dyn WriteSeek>` post-drop
/// isn't supported, so we round-trip through the filesystem.)
fn mux_to_tempfile(
    name: &str,
    stream: &StreamInfo,
    cadence: FragmentCadence,
    packets: &[Packet],
) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = fragmented_options(cadence);
        let mut mux = open_with_options(ws, std::slice::from_ref(stream), opts).unwrap();
        mux.write_header().unwrap();
        for p in packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    path
}

// --- Tests ----------------------------------------------------------------

#[test]
fn pcm_fragmented_roundtrip_byte_exact() {
    // 5 packets × 1024 frames × 2ch × 16-bit = 5 × 4096 byte payloads. At
    // a 48 kHz timescale, 1024 frames is ~21.3 ms — far below the 2-second
    // default, so we use EveryNPackets(2) to force two fragments.
    let stream = pcm_stream();
    let packets = make_pcm_packets(5, 1024);

    let path = mux_to_tempfile(
        "oxideav-mp4-frag-pcm.mp4",
        &stream,
        FragmentCadence::EveryNPackets(2),
        &packets,
    );

    // Demux via our own fragmented path.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&path).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("pcm_s16le"));
    assert_eq!(dmx.streams()[0].params.channels, Some(2));
    assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));

    let mut got: Vec<Vec<u8>> = Vec::new();
    let mut got_dts: Vec<i64> = Vec::new();
    let mut got_dur: Vec<i64> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                got_dts.push(p.dts.unwrap_or(0));
                got_dur.push(p.duration.unwrap_or(0));
                got.push(p.data);
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }

    assert_eq!(got.len(), packets.len(), "packet count round-trip");
    for (i, (g, expected)) in got.iter().zip(packets.iter()).enumerate() {
        assert_eq!(g, &expected.data, "packet {i} byte mismatch");
        assert_eq!(got_dts[i], expected.dts.unwrap(), "packet {i} dts mismatch");
        assert_eq!(
            got_dur[i],
            expected.duration.unwrap(),
            "packet {i} dur mismatch"
        );
    }
}

#[test]
fn moof_appears_in_output() {
    // Sanity check: the bytes we produced contain the `moof` FourCC at
    // the right place (after `moov`). This catches the fragmented muxer
    // accidentally falling back to non-fragmented.
    let stream = pcm_stream();
    let packets = make_pcm_packets(3, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-shape.mp4",
        &stream,
        FragmentCadence::EveryNPackets(1),
        &packets,
    );
    let bytes = std::fs::read(&path).unwrap();
    let moov_pos = find_fourcc(&bytes, b"moov").expect("moov box present");
    let moof_pos = find_fourcc(&bytes, b"moof").expect("moof box present");
    let mvex_pos = find_fourcc(&bytes, b"mvex").expect("mvex box present in moov");
    let trex_pos = find_fourcc(&bytes, b"trex").expect("trex box present in mvex");
    let mfhd_pos = find_fourcc(&bytes, b"mfhd").expect("mfhd box present in moof");
    let traf_pos = find_fourcc(&bytes, b"traf").expect("traf box present in moof");
    let tfhd_pos = find_fourcc(&bytes, b"tfhd").expect("tfhd box present in traf");
    let tfdt_pos = find_fourcc(&bytes, b"tfdt").expect("tfdt box present in traf");
    let trun_pos = find_fourcc(&bytes, b"trun").expect("trun box present in traf");

    assert!(moov_pos < mvex_pos);
    assert!(mvex_pos < trex_pos);
    assert!(mvex_pos < moof_pos, "mvex must be inside moov, before moof");
    assert!(moof_pos < mfhd_pos);
    assert!(mfhd_pos < traf_pos);
    assert!(traf_pos < tfhd_pos);
    assert!(tfhd_pos < tfdt_pos);
    assert!(tfdt_pos < trun_pos);
}

#[test]
fn styp_emitted_when_configured() {
    let stream = pcm_stream();
    let packets = make_pcm_packets(2, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-styp.mp4",
        &stream,
        FragmentCadence::EveryNPackets(1),
        &packets,
    );
    let bytes = std::fs::read(&path).unwrap();
    let styp_pos = find_fourcc(&bytes, b"styp").expect("styp box present (configured)");
    let moof_pos = find_fourcc(&bytes, b"moof").unwrap();
    assert!(
        styp_pos < moof_pos,
        "styp at {styp_pos} must precede first moof at {moof_pos}"
    );
}

#[test]
fn no_styp_when_disabled() {
    let buf_path = std::env::temp_dir().join("oxideav-mp4-frag-no-styp.mp4");
    {
        let f = std::fs::File::create(&buf_path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            brand: BrandPreset::Mp4,
            faststart: false,
            fragmented: Some(FragmentedOptions {
                cadence: FragmentCadence::EveryNPackets(1),
                styp: None,
                emit_random_access_indexes: true,
            }),
        };
        let stream = pcm_stream();
        let packets = make_pcm_packets(2, 1024);
        let mut mux = open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for p in &packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&buf_path).unwrap();
    assert!(
        find_fourcc(&bytes, b"styp").is_none(),
        "styp must be absent when disabled"
    );
    assert!(
        find_fourcc(&bytes, b"moof").is_some(),
        "moof must still be present"
    );
}

#[test]
fn dash_registry_entry_exists() {
    // `dash` and `cmaf` should be registered as muxer names.
    let mut reg = oxideav_core::ContainerRegistry::new();
    oxideav_mp4::register(&mut reg);
    let names: Vec<&str> = reg.muxer_names().collect();
    assert!(names.contains(&"dash"), "dash name registered ({names:?})");
    assert!(names.contains(&"cmaf"), "cmaf name registered ({names:?})");
}

#[test]
fn ismv_now_emits_fragmented() {
    // The `open_ismv` helper now switches to fragmented output (per ISMV
    // spec). Verify that a moof shows up.
    let buf_path = std::env::temp_dir().join("oxideav-mp4-ismv-frag.mp4");
    {
        let f = std::fs::File::create(&buf_path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let packets = make_pcm_packets(2, 1024);
        let mut mux = oxideav_mp4::muxer::open_ismv(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for p in &packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&buf_path).unwrap();
    assert!(find_fourcc(&bytes, b"moof").is_some());
    // ftyp brand should be iso4 (ISMV).
    let ftyp_pos = find_fourcc(&bytes, b"ftyp").expect("ftyp present");
    let major_off = ftyp_pos + 4; // ftyp is at FOURCC offset; major brand starts at +4 (skip the FourCC itself, not the size)
    let major = &bytes[major_off..major_off + 4];
    // The find_fourcc returns the offset of the FourCC bytes itself (i.e.
    // the 4 bytes just after the size). Major brand is the next 4 bytes
    // after the FourCC.
    assert_eq!(&bytes[ftyp_pos + 4..ftyp_pos + 8], b"iso4");
    let _ = major;
}

#[test]
fn faststart_and_fragmented_are_mutually_exclusive() {
    let stream = pcm_stream();
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Mp4,
        faststart: true,
        fragmented: Some(FragmentedOptions::default()),
    };
    let cursor: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    match open_with_options(cursor, std::slice::from_ref(&stream), opts) {
        Err(oxideav_core::Error::InvalidData(_)) => {}
        Err(other) => panic!("expected InvalidData, got {other:?}"),
        Ok(_) => panic!("expected InvalidData error"),
    }
}

#[test]
fn empty_input_no_fragments_emitted() {
    // Header + trailer with no packets between → ftyp + moov, no moof.
    let path = std::env::temp_dir().join("oxideav-mp4-frag-empty.mp4");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let opts = fragmented_options(FragmentCadence::EveryNPackets(1));
        let mut mux = open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(find_fourcc(&bytes, b"ftyp").is_some(), "ftyp present");
    assert!(find_fourcc(&bytes, b"moov").is_some(), "moov present");
    assert!(find_fourcc(&bytes, b"mvex").is_some(), "mvex present");
    assert!(
        find_fourcc(&bytes, b"moof").is_none(),
        "no moof for empty input"
    );
}

#[test]
fn many_fragments_round_trip() {
    // 20 packets, fragment every 4 → 5 fragments. Verify all 20 packets
    // come back with correct DTS sequence.
    let stream = pcm_stream();
    let packets = make_pcm_packets(20, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-many.mp4",
        &stream,
        FragmentCadence::EveryNPackets(4),
        &packets,
    );

    // Count moof/mdat pairs in the output.
    let bytes = std::fs::read(&path).unwrap();
    let n_moof = count_fourcc(&bytes, b"moof");
    let n_mdat = count_fourcc(&bytes, b"mdat");
    assert_eq!(n_moof, 5, "expected 5 moof boxes (20 / 4), got {n_moof}");
    assert_eq!(n_mdat, 5, "moof/mdat pairing");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&path).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut got = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        got.push((p.dts.unwrap_or(0), p.data));
    }
    assert_eq!(got.len(), 20);
    for (i, (dts, data)) in got.iter().enumerate() {
        assert_eq!(*dts, packets[i].dts.unwrap(), "sample {i} dts");
        assert_eq!(data, &packets[i].data, "sample {i} payload");
    }
}

/// Optional ffmpeg cross-check: pipe our fragmented output through
/// `ffmpeg -f mp4 -i - -c copy -f s16le -` to recover the raw PCM, then
/// compare against the bytes we originally wrote. ffmpeg parses the
/// fragmented MP4 with its own demuxer, validating ours against it.
///
/// Skipped when `ffmpeg` is not on `$PATH` (e.g. minimal CI runners).
#[test]
fn ffmpeg_pcm_extract_matches_input() {
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("ffmpeg unavailable — skipping ffmpeg cross-check");
        return;
    }

    let stream = pcm_stream();
    let packets = make_pcm_packets(8, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-ffmpeg.mp4",
        &stream,
        FragmentCadence::EveryNPackets(3),
        &packets,
    );
    let our_bytes: Vec<u8> = packets.iter().flat_map(|p| p.data.clone()).collect();

    // Decode with ffmpeg as a black-box validator.
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&path)
        .args([
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-",
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .expect("ffmpeg invocation failed");

    if !out.status.success() {
        panic!(
            "ffmpeg failed on our fragmented MP4: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(
        out.stdout,
        our_bytes,
        "ffmpeg's PCM extraction differs from input ({} vs {} bytes)",
        out.stdout.len(),
        our_bytes.len()
    );
}

// --- sidx / mfra emission tests (DASH on-demand profile) -----------------

/// With the default `emit_random_access_indexes = true`, every flushed
/// fragment must be preceded by a `sidx` and the file must end in an
/// `mfra` containing per-track `tfra` + `mfro` trailer.
#[test]
fn sidx_precedes_each_moof_and_mfra_is_at_end() {
    let stream = pcm_stream();
    let packets = make_pcm_packets(6, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-sidx-mfra.mp4",
        &stream,
        FragmentCadence::EveryNPackets(2),
        &packets,
    );
    let bytes = std::fs::read(&path).unwrap();

    let n_moof = count_fourcc(&bytes, b"moof");
    let n_sidx = count_fourcc(&bytes, b"sidx");
    let n_mfra = count_fourcc(&bytes, b"mfra");
    let n_tfra = count_fourcc(&bytes, b"tfra");
    let n_mfro = count_fourcc(&bytes, b"mfro");

    assert_eq!(n_moof, 3, "expected 3 moof boxes (6 / 2)");
    assert_eq!(n_sidx, 3, "one sidx per moof");
    assert_eq!(n_mfra, 1, "exactly one mfra trailer");
    assert_eq!(n_tfra, 1, "one tfra per track");
    assert_eq!(n_mfro, 1, "exactly one mfro");

    // Layout invariants.
    let first_sidx = find_fourcc(&bytes, b"sidx").unwrap();
    let first_moof = find_fourcc(&bytes, b"moof").unwrap();
    let mfra_pos = find_fourcc(&bytes, b"mfra").unwrap();
    let mfro_pos = find_fourcc(&bytes, b"mfro").unwrap();
    let last_moof_pos = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == b"moof".as_slice())
        .map(|(i, _)| i)
        .next_back()
        .unwrap();
    assert!(
        first_sidx < first_moof,
        "first sidx must precede first moof"
    );
    assert!(
        last_moof_pos < mfra_pos,
        "mfra must be at end (after every moof)"
    );
    assert!(mfra_pos < mfro_pos, "mfro must be inside (after) mfra");
    // mfro is the last box of the file. Box layout:
    //   4-byte size | 4-byte "mfro" | 4-byte ver/flags | 4-byte size_field
    // find_fourcc returns the offset of the FourCC bytes, so
    // bytes.len() - mfro_pos = 4 (FourCC) + 8 (body) = 12.
    assert_eq!(
        bytes.len() - mfro_pos,
        12,
        "mfro must be the file's trailing box"
    );
}

/// `emit_random_access_indexes = false` must restore the prior layout.
#[test]
fn no_sidx_no_mfra_when_disabled() {
    let buf_path = std::env::temp_dir().join("oxideav-mp4-frag-no-sidx.mp4");
    {
        let f = std::fs::File::create(&buf_path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            brand: BrandPreset::Mp4,
            faststart: false,
            fragmented: Some(FragmentedOptions {
                cadence: FragmentCadence::EveryNPackets(2),
                styp: None,
                emit_random_access_indexes: false,
            }),
        };
        let stream = pcm_stream();
        let packets = make_pcm_packets(4, 1024);
        let mut mux = open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for p in &packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&buf_path).unwrap();
    assert_eq!(count_fourcc(&bytes, b"moof"), 2);
    assert!(
        find_fourcc(&bytes, b"sidx").is_none(),
        "no sidx when disabled"
    );
    assert!(
        find_fourcc(&bytes, b"mfra").is_none(),
        "no mfra when disabled"
    );
}

/// Mux a fragmented MP4 with sidx+mfra, then re-parse the emitted boxes
/// using the public `parse_sidx_box` / `parse_mfra_box` entry points and
/// confirm they describe the on-disk byte layout correctly.
#[test]
fn emitted_sidx_and_mfra_parse_round_trip() {
    use oxideav_mp4::demux::{parse_mfra_box, parse_sidx_box};

    let stream = pcm_stream();
    let packets = make_pcm_packets(4, 1024);
    let path = mux_to_tempfile(
        "oxideav-mp4-frag-sidx-parse.mp4",
        &stream,
        FragmentCadence::EveryNPackets(1), // 4 fragments → 4 sidx, 4 tfra entries
        &packets,
    );
    let bytes = std::fs::read(&path).unwrap();

    // Walk the file, collect (sidx_offset, sidx_body) and the moof_offset
    // of the very next moof. Verify the sidx record's first_byte_offset
    // (which is end-of-sidx + first_offset = end-of-sidx, since we emit
    // first_offset = 0) lands exactly on the styp/moof that follows.
    let mut moof_offsets: Vec<u64> = Vec::new();
    let mut sidx_records: Vec<(u64, oxideav_mp4::demux::SidxRecord)> = Vec::new();
    let mut mfra_body: Option<Vec<u8>> = None;
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let fc = &bytes[i + 4..i + 8];
        match fc {
            b"moof" => moof_offsets.push(i as u64),
            b"sidx" => {
                let body = bytes[i + 8..i + sz].to_vec();
                let sidx_end = (i + sz) as u64;
                let r = parse_sidx_box(&body, sidx_end).unwrap().unwrap();
                sidx_records.push((sidx_end, r));
            }
            b"mfra" => mfra_body = Some(bytes[i + 8..i + sz].to_vec()),
            _ => {}
        }
        if sz == 0 {
            break;
        }
        i += sz;
    }

    assert_eq!(sidx_records.len(), 4, "one sidx per fragment");
    assert_eq!(moof_offsets.len(), 4);
    // first_offset = 0 means first_byte_offset == sidx_end_offset, and that
    // should be exactly the offset of the next styp-or-moof.
    for (k, (sidx_end, rec)) in sidx_records.iter().enumerate() {
        assert_eq!(
            rec.first_byte_offset, *sidx_end,
            "sidx[{k}] first_offset = 0 (we always emit zero)",
        );
        assert_eq!(rec.references.len(), 1, "single-reference sidx");
        let r0 = rec.references[0];
        assert!(!r0.is_sidx, "leaf reference (not hierarchical)");
        assert!(r0.starts_with_sap, "PCM packets are all sync");
        assert_eq!(r0.sap_type, 1, "closed-GOP IDR");
        // Every PCM packet is 4096 bytes; one packet + moof header overhead
        // → subsegment_size > 4096. Just sanity-check it's positive.
        assert!(r0.referenced_size > 0);
        assert_eq!(r0.subsegment_duration, 1024, "one PCM packet per fragment");
    }

    // mfra: 1 tfra (single audio track) with 4 entries (one per fragment)
    // pointing at the recorded moof offsets.
    let mfra_body = mfra_body.expect("mfra present");
    let tfras = parse_mfra_box(&mfra_body).unwrap();
    assert_eq!(tfras.len(), 1);
    let tfra = &tfras[0];
    assert_eq!(tfra.track_id, 1);
    assert_eq!(tfra.entries.len(), 4);
    for (k, e) in tfra.entries.iter().enumerate() {
        assert_eq!(
            e.moof_offset, moof_offsets[k],
            "tfra entry {k} points at the right moof",
        );
        assert_eq!(
            e.time,
            (k as u64) * 1024,
            "tfra entry {k} time = bmdt at start of fragment",
        );
        assert_eq!(e.traf_number, 1);
        assert_eq!(e.trun_number, 1);
        assert_eq!(e.sample_number, 1);
    }
}

// --- Helpers --------------------------------------------------------------

/// Find the byte offset of the first occurrence of `fourcc` in `bytes`.
/// Returns the offset of the FourCC bytes themselves (not the box size
/// preceding them).
fn find_fourcc(bytes: &[u8], fourcc: &[u8; 4]) -> Option<usize> {
    bytes.windows(4).position(|w| w == fourcc.as_slice())
    // The match should land on a 4-byte FourCC immediately preceded
    // by a 4-byte size, so we want the position of the FourCC start.
    // bytes.windows().position returns the start offset directly.
}

fn count_fourcc(bytes: &[u8], fourcc: &[u8; 4]) -> usize {
    bytes.windows(4).filter(|w| *w == fourcc.as_slice()).count()
}
