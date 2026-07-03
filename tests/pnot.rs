//! Integration test for the QuickTime Preview Atom (`pnot`).
//!
//! `pnot` is a top-level plain `Box` that locates the movie's preview
//! (poster) image. The typed accessor lives on `Mp4Demuxer` (downcast);
//! the flat metadata surface is the single `pnot` key. Both shapes are
//! exercised here.
//!
//! The fixture is built by writing a real MP4 with the regular muxer and
//! splicing a synthetic `pnot` box between the `ftyp` and the `moov`.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Build a full `pnot` box: `[size:u32]['pnot'][modification_date:u32]
/// [version:u16][atom_type:4][atom_index:u16]` — a plain Box, 20 bytes.
fn build_pnot_box(modification_date: u32, atom_type: &[u8; 4], atom_index: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(20);
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(b"pnot");
    out.extend_from_slice(&modification_date.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // version
    out.extend_from_slice(atom_type);
    out.extend_from_slice(&atom_index.to_be_bytes());
    out
}

/// Splice a box into a real MP4 stream between `ftyp` and the next
/// top-level box.
fn splice_after_ftyp(file: &[u8], boxx: &[u8]) -> Vec<u8> {
    let ftyp_size = u32::from_be_bytes([file[0], file[1], file[2], file[3]]) as usize;
    assert!(ftyp_size <= file.len(), "ftyp size overruns file");
    let mut out = Vec::with_capacity(file.len() + boxx.len());
    out.extend_from_slice(&file[..ftyp_size]);
    out.extend_from_slice(boxx);
    out.extend_from_slice(&file[ftyp_size..]);
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

fn mux_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-pnot-r382-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
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
fn pnot_after_ftyp_surfaces_on_metadata() {
    let pnot = build_pnot_box(0xC0FF_EE00, b"PICT", 1);
    let spliced = splice_after_ftyp(&mux_pcm_to_bytes(), &pnot);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    let md = dmx.metadata();
    let val = md.iter().find(|(k, _)| k == "pnot").map(|(_, v)| v);
    assert_eq!(val, Some(&"PICT 1 mod=3237998080".to_string()));
}

#[test]
fn pnot_absent_emits_no_pnot_key() {
    let bytes = mux_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();
    assert!(
        !md.iter().any(|(k, _)| k == "pnot"),
        "unexpected pnot metadata key in non-pnot file",
    );
}

#[test]
fn pnot_public_entry_point_parses_standalone_body() {
    // The bytes AFTER the 8-byte box header.
    let full = build_pnot_box(42, b"PICT", 1);
    let record = oxideav_mp4::demux::parse_pnot_box(&full[8..]).unwrap();
    assert_eq!(record.modification_date, 42);
    assert_eq!(record.atom_type, *b"PICT");
    assert_eq!(record.atom_index, 1);
    assert_eq!(record.version, 0);
}
