//! AAC carriage in the MP4 muxers (ISO/IEC 14496-14 §3.1.2 / §5.6):
//! every `mp4a` sample is one bare access unit and the decoder
//! configuration is the `esds` DecoderSpecificInfo.
//!
//! Regression for "AAC can't be encoded into MP4": the framework's AAC
//! encoders hand over ADTS frames and (for AAC-LC) no extradata, and the
//! muxer refused the stream ("aac stream missing extradata") or would
//! have stored the ADTS headers inside `mdat`.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Muxer, NullCodecResolver, Packet, ReadSeek, StreamInfo, TimeBase,
};

/// Shared in-memory `WriteSeek` so the muxed bytes can be read back.
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

impl Sink {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

fn aac_stream(extradata: Vec<u8>) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.sample_rate = Some(44_100);
    params.channels = Some(2);
    params.extradata = extradata;
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// An ADTS frame (`protection_absent = 1`, AAC-LC, 44.1 kHz, stereo)
/// around `au`.
fn adts(au: &[u8]) -> Vec<u8> {
    let fl = au.len() + 7;
    let mut v = vec![
        0xFF,
        0xF1,
        (1 << 6) | (4 << 2),
        (2 << 6) | ((fl >> 11) as u8 & 0x03),
        (fl >> 3) as u8,
        ((fl as u8 & 0x07) << 5) | 0x1F,
        0xFC,
    ];
    v.extend_from_slice(au);
    v
}

/// Distinct, non-ADTS-looking access units.
fn access_units(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| (0..(20 + i * 3)).map(|j| (0x21 + i + j) as u8).collect())
        .collect()
}

type Opener =
    fn(Box<dyn oxideav_core::WriteSeek>, &[StreamInfo]) -> oxideav_core::Result<Box<dyn Muxer>>;

fn mux(open: Opener, stream: &StreamInfo, packets: &[(Vec<u8>, i64)]) -> Vec<u8> {
    let sink = Sink::default();
    let mut m = open(Box::new(sink.clone()), std::slice::from_ref(stream)).unwrap();
    m.write_header().unwrap();
    for (data, pts) in packets {
        let p = Packet::new(0, stream.time_base, data.clone())
            .with_pts(*pts)
            .with_dts(*pts)
            .with_duration(1024)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.bytes()
}

fn demux(bytes: Vec<u8>) -> (StreamInfo, Vec<Packet>) {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut d = oxideav_mp4::demux::open(rs, &NullCodecResolver).unwrap();
    let s = d.streams()[0].clone();
    let mut pkts = Vec::new();
    while let Ok(p) = d.next_packet() {
        pkts.push(p);
    }
    (s, pkts)
}

#[test]
fn adts_packets_without_extradata_become_bare_samples() {
    let aus = access_units(5);
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (adts(a), i as i64 * 1024))
        .collect();
    for open in [
        oxideav_mp4::muxer::open as Opener,
        oxideav_mp4::muxer::open_mov,
    ] {
        let (s, got) = demux(mux(open, &aac_stream(Vec::new()), &packets));
        assert_eq!(s.params.codec_id, CodecId::new("aac"));
        // AAC-LC, 44.1 kHz, stereo, recovered from the ADTS header.
        assert_eq!(s.params.extradata, vec![0x12, 0x10]);
        let got: Vec<_> = got.into_iter().map(|p| p.data).collect();
        assert_eq!(got, aus, "mdat must hold the bare access units");
    }
}

#[test]
fn bare_access_units_with_extradata_pass_through() {
    let aus = access_units(4);
    // HE-AAC v1 backward-compatible ASC (22.05 kHz core, 44.1 kHz SBR).
    let asc = vec![0x13, 0x90, 0x56, 0xE5, 0xA0];
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (a.clone(), i as i64 * 1024))
        .collect();
    let (s, got) = demux(mux(
        oxideav_mp4::muxer::open,
        &aac_stream(asc.clone()),
        &packets,
    ));
    assert_eq!(s.params.extradata, asc);
    assert_eq!(got.into_iter().map(|p| p.data).collect::<Vec<_>>(), aus);
}

#[test]
fn fragmented_output_strips_adts_and_synthesises_the_asc() {
    let aus = access_units(3);
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (adts(a), i as i64 * 1024))
        .collect();
    let bytes = mux(
        oxideav_mp4::muxer::open_dash,
        &aac_stream(Vec::new()),
        &packets,
    );
    let (s, got) = demux(bytes);
    assert_eq!(s.params.extradata, vec![0x12, 0x10]);
    assert_eq!(got.into_iter().map(|p| p.data).collect::<Vec<_>>(), aus);
}

#[test]
fn negative_first_pts_becomes_a_priming_edit() {
    // Encoder priming: the first access unit decodes to samples that
    // precede presentation time 0. A remux of an MP4 whose edit list
    // trims priming hands the muxer exactly this shape.
    let aus = access_units(4);
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (a.clone(), i as i64 * 1024 - 1024))
        .collect();
    let bytes = mux(
        oxideav_mp4::muxer::open,
        &aac_stream(vec![0x12, 0x10]),
        &packets,
    );
    let (_, got) = demux(bytes);
    assert_eq!(got.len(), 4);
    let pts: Vec<_> = got.iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts, vec![-1024, 0, 1024, 2048]);
    assert!(got[0].flags.discard, "the priming AU is never presented");
    assert!(!got[1].flags.discard);
}

#[test]
fn mov_files_open_through_the_mov_demuxer_name() {
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_mp4::register(&mut ctx);
    assert_eq!(ctx.containers.container_for_extension("mov"), Some("mov"));
    let aus = access_units(3);
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (adts(a), i as i64 * 1024))
        .collect();
    let bytes = mux(
        oxideav_mp4::muxer::open_mov,
        &aac_stream(Vec::new()),
        &packets,
    );
    let mut d = ctx
        .containers
        .open_demuxer("mov", Box::new(Cursor::new(bytes)), &NullCodecResolver)
        .unwrap();
    assert_eq!(d.streams()[0].params.codec_id, CodecId::new("aac"));
    assert_eq!(d.streams()[0].params.extradata, vec![0x12, 0x10]);
    assert_eq!(d.next_packet().unwrap().data, aus[0]);
}

#[test]
fn he_aac_with_a_core_rate_sample_entry_demuxes_at_the_sbr_rate() {
    // A writer that put the 22.05 kHz core rate in the sample entry of a
    // backward-compatible HE-AAC stream (44.1 kHz SBR output).
    let mut s = aac_stream(vec![0x13, 0x90, 0x56, 0xE5, 0xA0]);
    s.params.sample_rate = Some(22_050);
    s.time_base = TimeBase::new(1, 22_050);
    let aus = access_units(2);
    let packets: Vec<_> = aus
        .iter()
        .enumerate()
        .map(|(i, a)| (a.clone(), i as i64 * 1024))
        .collect();
    let (st, _) = demux(mux(oxideav_mp4::muxer::open, &s, &packets));
    assert_eq!(st.params.sample_rate, Some(44_100));
}
