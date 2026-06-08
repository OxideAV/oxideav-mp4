//! Integration test for the ISO/IEC 14496-12 §8.1.3 Progressive
//! Download Information Box (`pdin`) round 259 typed accessor.
//!
//! The typed accessor lives on `Mp4Demuxer` (downcast); the flat
//! metadata surface is `pdin_count` + `pdin_<n>` (one per
//! `(rate, initial_delay)` pair). Both shapes are exercised here.
//!
//! The fixture is built by writing a real MP4 with the regular muxer
//! and then splicing a synthetic `pdin` box between the `ftyp` and the
//! `moov` (top-level walk order doesn't matter to the demuxer, but
//! §8.1.3.1 recommends pdin appear as early as possible so that's
//! where we place it). The demuxer must surface the entries on
//! `metadata()` and the underlying `Mp4Demuxer::pdin_entries()`
//! accessor must return the same pairs.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

/// Per-process monotonic counter so each `mux_pcm_to_bytes` call uses a
/// distinct tempfile path; the rust test harness runs tests in parallel
/// by default and a shared PID-only filename causes a race in which one
/// test deletes another's still-being-read file.
static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Build a `pdin` box body (4-byte FullBox preamble + N `(rate,
/// initial_delay)` u32 pairs in big-endian) and wrap it in a full
/// 32-bit-sized box header — `[size:u32]['pdin'][version+flags:4][entry*8]`.
fn build_pdin_box(entries: &[(u32, u32)]) -> Vec<u8> {
    let body_len = 4 + entries.len() * 8;
    let total_size = 8 + body_len;
    let mut out = Vec::with_capacity(total_size);
    out.extend_from_slice(&(total_size as u32).to_be_bytes());
    out.extend_from_slice(b"pdin");
    out.extend_from_slice(&[0u8; 4]); // FullBox(v=0, flags=0)
    for (rate, delay) in entries {
        out.extend_from_slice(&rate.to_be_bytes());
        out.extend_from_slice(&delay.to_be_bytes());
    }
    out
}

/// Splice a `pdin` box into a real MP4 stream between `ftyp` and the
/// next top-level box. The §8.1.3.1 placement guidance is "as early as
/// possible"; right after `ftyp` is the canonical spot. Returns the
/// re-spliced byte vector — same prefix (the `ftyp`), then the pdin,
/// then everything that originally followed `ftyp`.
fn splice_pdin_after_ftyp(file: &[u8], pdin_box: &[u8]) -> Vec<u8> {
    // ftyp is the first top-level box; its size field is at offsets
    // 0..4 (big-endian u32). The original opener does not write
    // largesize for ftyp, so a plain 32-bit size is correct here.
    let ftyp_size = u32::from_be_bytes([file[0], file[1], file[2], file[3]]) as usize;
    assert!(ftyp_size <= file.len(), "ftyp size overruns file");
    let mut out = Vec::with_capacity(file.len() + pdin_box.len());
    out.extend_from_slice(&file[..ftyp_size]); // ftyp
    out.extend_from_slice(pdin_box); // injected pdin
    out.extend_from_slice(&file[ftyp_size..]); // moov + mdat
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

/// Mux a tiny PCM stream to a temp file and return its bytes for
/// splicing. We route through a real file rather than a `Cursor` so
/// the `dyn WriteSeek + 'static` constraint on `oxideav_mp4::muxer::open`
/// is satisfied without juggling a borrow.
fn mux_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-pdin-r259-{}-{}.mp4",
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
fn pdin_after_ftyp_surfaces_on_metadata() {
    // §8.1.3 — three rate/initial_delay pairs, ascending rates so they
    // simulate a real ABR-aware progressive-download hint table.
    let pairs: Vec<(u32, u32)> = vec![(125_000, 5_000), (250_000, 2_500), (500_000, 1_200)];
    let pdin = build_pdin_box(&pairs);
    let spliced = splice_pdin_after_ftyp(&mux_pcm_to_bytes(), &pdin);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    // Flat metadata: `pdin_count` + `pdin_<n>` (one per pair).
    let md = dmx.metadata();
    let count = md.iter().find(|(k, _)| k == "pdin_count").map(|(_, v)| v);
    assert_eq!(count, Some(&"3".to_string()));
    for (i, (rate, delay)) in pairs.iter().enumerate() {
        let key = format!("pdin_{i}");
        let want = format!("{rate} {delay}");
        let got = md.iter().find(|(k, _)| k == &key).map(|(_, v)| v);
        assert_eq!(
            got,
            Some(&want),
            "pdin_{i} flat-metadata mismatch (rate={rate}, delay={delay})",
        );
    }
}

#[test]
fn pdin_absent_emits_no_pdin_keys() {
    // No splicing — vanilla muxer output. The flat-metadata surface
    // must carry no `pdin_*` keys at all (absence is signalled by
    // omission, not by a zero count).
    let bytes = mux_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();
    for (k, _) in md {
        assert!(
            !k.starts_with("pdin"),
            "unexpected pdin metadata key {k} in non-pdin file",
        );
    }
}

#[test]
fn pdin_empty_body_emits_zero_count_only() {
    // §8.1.3 permits an empty `pdin` (preamble only, zero pairs) — a
    // producer's way of saying "no hints available". The flat
    // metadata surfaces `pdin_count = 0` and no `pdin_<n>` keys.
    let pdin = build_pdin_box(&[]);
    let spliced = splice_pdin_after_ftyp(&mux_pcm_to_bytes(), &pdin);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();
    let count = md.iter().find(|(k, _)| k == "pdin_count").map(|(_, v)| v);
    assert_eq!(count, Some(&"0".to_string()));
    let any_pair = md
        .iter()
        .any(|(k, _)| k.starts_with("pdin_") && k != "pdin_count");
    assert!(!any_pair, "empty pdin must not emit pdin_<n> entries");
}

#[test]
fn pdin_unaligned_body_fails_open() {
    // A `pdin` body whose post-preamble length is not a multiple of 8
    // (one `(rate, initial_delay)` u32 pair) is malformed per §8.1.3.2.
    // The demuxer must reject the file at `open()` rather than silently
    // dropping the trailing partial pair — that error mode is the
    // producer's bug to fix.
    let mut pdin_body = Vec::new();
    pdin_body.extend_from_slice(&[0u8; 4]); // FullBox preamble
    pdin_body.extend_from_slice(&[0u8; 12]); // 1.5 pairs — unaligned
    let total_size = 8 + pdin_body.len();
    let mut pdin_box = Vec::with_capacity(total_size);
    pdin_box.extend_from_slice(&(total_size as u32).to_be_bytes());
    pdin_box.extend_from_slice(b"pdin");
    pdin_box.extend_from_slice(&pdin_body);

    let spliced = splice_pdin_after_ftyp(&mux_pcm_to_bytes(), &pdin_box);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    assert!(
        oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).is_err(),
        "unaligned pdin must fail open"
    );
}

#[test]
fn pdin_public_entry_point_parses_standalone_body() {
    // The `parse_pdin_box` public entry point lets tooling that
    // already has the box payload bytes recover the typed
    // `(rate, initial_delay)` table without running `open()`. The
    // input is the bytes AFTER the 8/16-byte box header.
    let pairs: Vec<(u32, u32)> = vec![(64_000, 3_000), (256_000, 1_500)];
    // Strip the 8-byte (size+type) header to get the body the public
    // entry expects.
    let pdin_full = build_pdin_box(&pairs);
    let body = &pdin_full[8..];
    let record = oxideav_mp4::demux::parse_pdin_box(body).unwrap();
    assert_eq!(record.entries.len(), pairs.len());
    for (i, (rate, delay)) in pairs.iter().enumerate() {
        assert_eq!(record.entries[i].rate, *rate);
        assert_eq!(record.entries[i].initial_delay, *delay);
    }
}
