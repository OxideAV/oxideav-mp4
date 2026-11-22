//! Box size forms (ISO/IEC 14496-12 §4.2) and whole-file bounds on the
//! demux side: a trailing box whose size field is zero extends to the
//! end of the file, the 64-bit `largesize` form is accepted wherever
//! the compact one is, and sample tables that describe more samples
//! than the file could hold are refused before they are expanded.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Muxer, NullCodecResolver, Packet, StreamInfo, TimeBase,
};
use oxideav_mp4::{ChunkPolicy, Mp4MuxerOptions};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn pcm_stream() -> StreamInfo {
    let mut a = CodecParameters::audio(CodecId::new("pcm_s16le"));
    a.channels = Some(1);
    a.sample_rate = Some(8000);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 8000),
        duration: None,
        start_time: Some(0),
        params: a,
    }
}

/// `n` two-byte PCM packets (one frame each): a constant-size table.
fn mux(n: usize, faststart: bool) -> Vec<u8> {
    let stream = pcm_stream();
    let sink = Sink::default();
    let mut m: Box<dyn Muxer> = oxideav_mp4::muxer::open_with_options(
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
        // Track-run chunks keep the chunk table (and so the `trak`) small.
        Mp4MuxerOptions::default()
            .with_faststart(faststart)
            .with_chunking(ChunkPolicy::TrackRuns),
    )
    .unwrap();
    m.write_header().unwrap();
    for i in 0..n {
        let p = Packet::new(0, stream.time_base, vec![i as u8, (i >> 8) as u8])
            .with_pts(i as i64)
            .with_dts(i as i64)
            .with_duration(1)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.0.lock().unwrap().get_ref().clone()
}

fn open(bytes: &[u8]) -> oxideav_core::Result<Box<dyn oxideav_core::Demuxer>> {
    oxideav_mp4::demux::open(Box::new(Cursor::new(bytes.to_vec())), &NullCodecResolver)
}

fn payloads(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut d = open(bytes).unwrap();
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push((p.stream_index, p.data));
    }
    out
}

/// `(offset, size)` of the top-level box of this type.
fn top_level(bytes: &[u8], name: &[u8; 4]) -> (usize, usize) {
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if &bytes[at + 4..at + 8] == name {
            return (at, size);
        }
        assert!(size >= 8);
        at += size;
    }
    panic!("no top-level {}", String::from_utf8_lossy(name));
}

#[test]
fn trailing_moov_with_size_zero_extends_to_the_end() {
    let bytes = mux(50, false);
    let (at, size) = top_level(&bytes, b"moov");
    assert_eq!(at + size, bytes.len(), "moov is the last box");
    let mut patched = bytes.clone();
    patched[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(payloads(&patched), payloads(&bytes));
    // One-tick PCM frames arrive concatenated: 50 two-byte frames.
    let bytes_out: usize = payloads(&patched).iter().map(|(_, d)| d.len()).sum();
    assert_eq!(bytes_out, 100);
}

#[test]
fn trailing_mdat_with_size_zero_extends_to_the_end() {
    let bytes = mux(50, true);
    let (at, size) = top_level(&bytes, b"mdat");
    assert_eq!(at + size, bytes.len(), "mdat is the last box");
    let mut patched = bytes.clone();
    patched[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(payloads(&patched), payloads(&bytes));
}

#[test]
fn moov_in_the_largesize_form() {
    let bytes = mux(50, false);
    let (at, size) = top_level(&bytes, b"moov");
    // [1]["moov"][largesize] + the same body; moov is last, so no
    // chunk offset moves.
    let mut patched = bytes[..at].to_vec();
    patched.extend_from_slice(&1u32.to_be_bytes());
    patched.extend_from_slice(b"moov");
    patched.extend_from_slice(&(size as u64 + 8).to_be_bytes());
    patched.extend_from_slice(&bytes[at + 8..]);
    assert_eq!(payloads(&patched), payloads(&bytes));
    // A largesize that runs past the end of the file is a truncation.
    let mut short = patched.clone();
    let n = short.len();
    short[at + 8..at + 16].copy_from_slice(&(size as u64 + 9).to_be_bytes());
    assert!(open(&short).is_err());
    assert_eq!(n, patched.len());
}

/// The file with its `trak` repeated until there are `copies` of it.
fn with_trak_copies(bytes: &[u8], copies: usize) -> Vec<u8> {
    let (moov_at, moov_size) = top_level(bytes, b"moov");
    assert_eq!(moov_at + moov_size, bytes.len());
    let moov = &bytes[moov_at + 8..];
    // The trak is the box after mvhd.
    let mvhd_size = u32::from_be_bytes(moov[..4].try_into().unwrap()) as usize;
    let trak_size = u32::from_be_bytes(moov[mvhd_size..mvhd_size + 4].try_into().unwrap()) as usize;
    assert_eq!(&moov[mvhd_size + 4..mvhd_size + 8], b"trak");
    let trak = &moov[mvhd_size..mvhd_size + trak_size];
    let mut body = moov.to_vec();
    for _ in 1..copies {
        body.extend_from_slice(trak);
    }
    let mut out = bytes[..moov_at].to_vec();
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"moov");
    out.extend_from_slice(&body);
    out
}

#[test]
fn tracks_cannot_claim_the_same_bytes_without_bound() {
    // 3000 two-byte samples in a constant-size table: 20 bytes of
    // `stsz` describe them all, and every copy of the `trak` (a few
    // hundred bytes) describes them again.
    let bytes = mux(3000, false);
    let two = with_trak_copies(&bytes, 2);
    let mut d = open(&two).unwrap();
    assert_eq!(d.streams().len(), 2, "two tracks may share their data");
    // 3000 one-tick frames a track, concatenated into packets.
    let mut frames = 0;
    while let Ok(p) = d.next_packet() {
        assert_eq!(p.duration, Some((p.data.len() / 2) as i64));
        frames += p.data.len() / 2;
    }
    assert_eq!(frames, 6000);

    // Forty copies: 120 000 samples from a file of some 30 kB — more
    // than two a byte, which no real table reaches.
    let forty = with_trak_copies(&bytes, 40);
    assert!(forty.len() < 40_000, "{}", forty.len());
    let err = match open(&forty) {
        Err(e) => e.to_string(),
        Ok(_) => panic!(
            "opened a file describing 120000 samples in {} bytes",
            forty.len()
        ),
    };
    assert!(err.contains("more samples than the file can back"), "{err}");
}
