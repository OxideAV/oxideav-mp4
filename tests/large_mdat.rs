//! Integration tests for the `Mp4MuxerOptions::large_mdat` 64-bit
//! `largesize` mdat header (ISO/IEC 14496-12 §4.2 extended size form).
//!
//! We can't actually write a >4 GiB payload in a unit test, so these
//! assert the *header shape* a >4 GiB file would use — `[size=1]["mdat"]
//! [largesize:u64]` where `largesize` equals the box's total size — and
//! that the resulting file still round-trips through the demuxer (which
//! already handles the extended form, per tests/largesize_overflow.rs).

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::muxer::open_with_options;
use oxideav_mp4::options::{BrandPreset, Mp4MuxerOptions};

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

fn make_packets(n: usize, frames: i64) -> Vec<Packet> {
    let stream = pcm_stream();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut payload = Vec::with_capacity(frames as usize * 4);
        for j in 0..frames as usize {
            let l = ((i * 997 + j) as i16).wrapping_mul(7);
            let r = ((i * 997 + j) as i16).wrapping_mul(11);
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

fn base_opts() -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        brand: BrandPreset::Mp4,
        faststart: false,
        fragmented: None,
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: true,
        ..Mp4MuxerOptions::default()
    }
}

/// Find the top-level `mdat` box header and return
/// `(size32, largesize_opt)` — `size32` is the raw 32-bit `size` field,
/// `largesize_opt` is `Some(u64)` when `size32 == 1` (extended form).
fn find_mdat_header(bytes: &[u8]) -> (u32, Option<u64>) {
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        let kind = &bytes[pos + 4..pos + 8];
        let (advance, large) = if size == 1 {
            // Extended: 8-byte largesize follows the type.
            assert!(pos + 16 <= bytes.len(), "truncated largesize header");
            let ls = u64::from_be_bytes([
                bytes[pos + 8],
                bytes[pos + 9],
                bytes[pos + 10],
                bytes[pos + 11],
                bytes[pos + 12],
                bytes[pos + 13],
                bytes[pos + 14],
                bytes[pos + 15],
            ]);
            (ls, Some(ls))
        } else if size == 0 {
            ((bytes.len() - pos) as u64, None)
        } else {
            (size as u64, None)
        };
        if kind == b"mdat" {
            return (size, large);
        }
        if advance < 8 {
            break;
        }
        pos += advance as usize;
    }
    panic!("no mdat box found");
}

/// Mux to bytes through a tempfile (the muxer owns the `WriteSeek`).
fn mux_bytes(opts: Mp4MuxerOptions, stream: &StreamInfo, packets: &[Packet], tag: &str) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!(
        "oxideav-mp4-large-mdat-{tag}-{}.mp4",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(ws, std::slice::from_ref(stream), opts).unwrap();
        mux.write_header().unwrap();
        for p in packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();
    bytes
}

fn demux_payloads(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => got.push(pkt.data.to_vec()),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    got
}

/// Direct-write mode with `large_mdat`: the mdat header is the §4.2
/// extended form and the file round-trips.
#[test]
fn direct_large_mdat_header_extended_and_roundtrips() {
    let stream = pcm_stream();
    let packets = make_packets(4, 1024);
    let bytes = mux_bytes(base_opts(), &stream, &packets, "direct");

    let (size32, large) = find_mdat_header(&bytes);
    assert_eq!(size32, 1, "extended-form size field must be 1");
    let ls = large.expect("largesize present");
    // largesize == total box size == 16-byte header + payload.
    let payload: usize = packets.iter().map(|p| p.data.len()).sum();
    assert_eq!(ls, (16 + payload) as u64);

    let got = demux_payloads(bytes);
    assert_eq!(got.len(), packets.len());
    for (a, b) in got.iter().zip(packets.iter()) {
        assert_eq!(a, &b.data, "payload byte-exact");
    }
}

/// faststart mode with `large_mdat`: same extended header, chunk offsets
/// account for the 16-byte header, and the file round-trips.
#[test]
fn faststart_large_mdat_header_extended_and_roundtrips() {
    let stream = pcm_stream();
    let packets = make_packets(4, 1024);
    let mut opts = base_opts();
    opts.faststart = true;
    let bytes = mux_bytes(opts, &stream, &packets, "faststart");

    let (size32, large) = find_mdat_header(&bytes);
    assert_eq!(size32, 1);
    let ls = large.expect("largesize present");
    let payload: usize = packets.iter().map(|p| p.data.len()).sum();
    assert_eq!(ls, (16 + payload) as u64);

    let got = demux_payloads(bytes);
    assert_eq!(got.len(), packets.len());
    for (a, b) in got.iter().zip(packets.iter()) {
        assert_eq!(a, &b.data, "payload byte-exact");
    }
}

/// Without the flag, a small file keeps the compact 32-bit header
/// (byte-identical to the historical output) and still round-trips.
#[test]
fn default_compact_header_unchanged() {
    let stream = pcm_stream();
    let packets = make_packets(3, 1024);
    let mut opts = base_opts();
    opts.large_mdat = false;
    let bytes = mux_bytes(opts, &stream, &packets, "compact");

    let (size32, large) = find_mdat_header(&bytes);
    assert!(large.is_none(), "compact header has no largesize");
    let payload: usize = packets.iter().map(|p| p.data.len()).sum();
    assert_eq!(size32, (8 + payload) as u32, "compact size = 8 + payload");

    let got = demux_payloads(bytes);
    assert_eq!(got.len(), packets.len());
}
