//! Uncompressed audio in MP4.
//!
//! * ISO/IEC 23003-5 `ipcm` / `fpcm` sample entries (with `pcmC`, and
//!   `srat` above 65535 Hz): every PCM codec round-trips byte-exact
//!   through the muxer and the demuxer, codec id and sample format
//!   included.
//! * Big-endian 23003-5 entries and the legacy QuickTime entries
//!   (`sowt`, `twos`, `lpcm` v2, `in24` + `enda`, `raw `, and the v0
//!   "sample size 1" table convention) demux to the little-endian PCM
//!   codecs with the samples in little-endian order.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

// ───────────────────────── mux → demux round trip ─────────────────────────

fn mux(codec: &str, rate: u32, channels: u16, packets: &[Vec<u8>], frames: u32) -> Vec<u8> {
    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(rate);
    params.channels = Some(channels);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, i64::from(rate)),
        duration: None,
        start_time: Some(0),
        params,
    };
    let path = std::env::temp_dir().join(format!(
        "oxideav-mp4-pcm-{codec}-{rate}-{}.mp4",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut m = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        m.write_header().unwrap();
        for (i, data) in packets.iter().enumerate() {
            let mut p = Packet::new(0, stream.time_base, data.clone());
            p.pts = Some(i as i64 * i64::from(frames));
            p.dts = p.pts;
            p.duration = Some(i64::from(frames));
            p.flags.keyframe = true;
            m.write_packet(&p).unwrap();
        }
        m.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

fn demux(file: Vec<u8>) -> (CodecParameters, Vec<u8>) {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut d = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let params = d.streams()[0].params.clone();
    let mut data = Vec::new();
    while let Ok(p) = d.next_packet() {
        data.extend_from_slice(&p.data);
    }
    (params, data)
}

#[test]
fn every_pcm_codec_round_trips_through_ipcm_or_fpcm() {
    let cases = [
        ("pcm_s8", 1usize, SampleFormat::S8, b"ipcm"),
        ("pcm_s16le", 2, SampleFormat::S16, b"ipcm"),
        ("pcm_s24le", 3, SampleFormat::S24, b"ipcm"),
        ("pcm_s32le", 4, SampleFormat::S32, b"ipcm"),
        ("pcm_f32le", 4, SampleFormat::F32, b"fpcm"),
        ("pcm_f64le", 8, SampleFormat::F64, b"fpcm"),
    ];
    for (codec, bytes, fmt, fourcc) in cases {
        for (rate, channels) in [(48_000u32, 2u16), (8_000, 1), (96_000, 6), (192_000, 2)] {
            let frames = 100u32;
            let packets: Vec<Vec<u8>> = (0..3)
                .map(|p| {
                    (0..frames as usize * usize::from(channels) * bytes)
                        .map(|i| (i * 7 + p * 13) as u8)
                        .collect()
                })
                .collect();
            let file = mux(codec, rate, channels, &packets, frames);
            assert!(
                file.windows(4).any(|w| w == fourcc),
                "{codec}: {} entry",
                String::from_utf8_lossy(fourcc)
            );
            assert!(file.windows(4).any(|w| w == b"pcmC"), "{codec}: pcmC");
            let (params, data) = demux(file);
            let tag = format!("{codec} {rate} Hz {channels} ch");
            assert_eq!(params.codec_id.as_str(), codec, "{tag}");
            assert_eq!(params.sample_format, Some(fmt), "{tag}");
            assert_eq!(params.sample_rate, Some(rate), "{tag}");
            assert_eq!(params.channels, Some(channels), "{tag}");
            assert_eq!(data, packets.concat(), "{tag}: samples");
        }
    }
}

// ───────────────────── hand-built legacy / big-endian files ─────────────────────

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn full(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.extend_from_slice(body);
    boxed(fourcc, &b)
}

/// AudioSampleEntry v0 preamble with `version` in the first reserved
/// 16 bits (QuickTime sound description version).
fn audio_preamble(version: u16, channels: u16, bits: u16, rate: u32) -> Vec<u8> {
    let mut e = vec![0u8; 28];
    e[6..8].copy_from_slice(&1u16.to_be_bytes());
    e[8..10].copy_from_slice(&version.to_be_bytes());
    e[16..18].copy_from_slice(&channels.to_be_bytes());
    e[18..20].copy_from_slice(&bits.to_be_bytes());
    e[24..28].copy_from_slice(&((rate & 0xFFFF) << 16).to_be_bytes());
    e
}

/// A one-track file: `entry` (the full sample-entry box) describing
/// `payload`, stored as `count` samples of `sample_size` bytes in one
/// chunk with one tick each.
fn file_with(
    stsd_version: u8,
    entry: Vec<u8>,
    payload: &[u8],
    count: u32,
    sample_size: u32,
) -> Vec<u8> {
    let build_moov = |off: u32| -> Vec<u8> {
        let mut stsd = vec![stsd_version, 0, 0, 0];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&entry);
        let mut stbl = boxed(b"stsd", &stsd);
        let mut stts = 1u32.to_be_bytes().to_vec();
        stts.extend_from_slice(&count.to_be_bytes());
        stts.extend_from_slice(&1u32.to_be_bytes());
        stbl.extend_from_slice(&full(b"stts", &stts));
        let mut stsc = 1u32.to_be_bytes().to_vec();
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&count.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stbl.extend_from_slice(&full(b"stsc", &stsc));
        let mut stsz = sample_size.to_be_bytes().to_vec();
        stsz.extend_from_slice(&count.to_be_bytes());
        stbl.extend_from_slice(&full(b"stsz", &stsz));
        let mut stco = 1u32.to_be_bytes().to_vec();
        stco.extend_from_slice(&off.to_be_bytes());
        stbl.extend_from_slice(&full(b"stco", &stco));
        let mut minf = full(b"smhd", &[0u8; 4]);
        let mut dref = 1u32.to_be_bytes().to_vec();
        dref.extend_from_slice(&boxed(b"url ", &[0, 0, 0, 1]));
        minf.extend_from_slice(&boxed(b"dinf", &full(b"dref", &dref)));
        minf.extend_from_slice(&boxed(b"stbl", &stbl));
        let mut mdhd = vec![0u8; 20];
        mdhd[8..12].copy_from_slice(&48_000u32.to_be_bytes());
        mdhd[16..18].copy_from_slice(&0x55C4u16.to_be_bytes());
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(b"soun");
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.extend_from_slice(b"audio\0");
        let mut mdia = full(b"mdhd", &mdhd);
        mdia.extend_from_slice(&full(b"hdlr", &hdlr));
        mdia.extend_from_slice(&boxed(b"minf", &minf));
        let mut tkhd = vec![0u8; 80];
        tkhd[1..4].copy_from_slice(&[0, 0, 7]);
        tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
        let mut trak = boxed(b"tkhd", &tkhd);
        trak.extend_from_slice(&boxed(b"mdia", &mdia));
        let mut mvhd = vec![0u8; 96];
        mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[92..96].copy_from_slice(&2u32.to_be_bytes());
        let mut moov = full(b"mvhd", &mvhd);
        moov.extend_from_slice(&boxed(b"trak", &trak));
        boxed(b"moov", &moov)
    };
    let mut ftyp = b"isom".to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"isom");
    let ftyp = boxed(b"ftyp", &ftyp);
    let base = (ftyp.len() + build_moov(0).len() + 8) as u32;
    let mut file = ftyp;
    file.extend_from_slice(&build_moov(base));
    file.extend_from_slice(&boxed(b"mdat", payload));
    file
}

/// 16-bit samples `[0x0102, 0x0304, -2, 0x7f00]` stored big-endian, and
/// what the demuxer must deliver.
const BE16: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0xff, 0xfe, 0x7f, 0x00];
const LE16: [u8; 8] = [0x02, 0x01, 0x04, 0x03, 0xfe, 0xff, 0x00, 0x7f];

#[test]
fn big_endian_ipcm_is_delivered_little_endian() {
    let mut e = audio_preamble(0, 2, 16, 48_000);
    e.extend_from_slice(&full(b"pcmC", &[0x00, 16]));
    let (p, data) = demux(file_with(0, boxed(b"ipcm", &e), &BE16, 2, 4));
    assert_eq!(p.codec_id.as_str(), "pcm_s16le");
    assert_eq!(data, LE16);
    // Big-endian float.
    let mut e = audio_preamble(0, 1, 32, 48_000);
    e.extend_from_slice(&full(b"pcmC", &[0x00, 32]));
    let be = 1.5f32.to_be_bytes();
    let (p, data) = demux(file_with(0, boxed(b"fpcm", &e), &be, 1, 4));
    assert_eq!(p.codec_id.as_str(), "pcm_f32le");
    assert_eq!(data, 1.5f32.to_le_bytes());
}

#[test]
fn quicktime_sowt_and_twos() {
    let e = audio_preamble(0, 2, 16, 48_000);
    let (p, data) = demux(file_with(0, boxed(b"sowt", &e), &LE16, 2, 4));
    assert_eq!(p.codec_id.as_str(), "pcm_s16le");
    assert_eq!(p.sample_format, Some(SampleFormat::S16));
    assert_eq!(data, LE16);
    let (p, data) = demux(file_with(0, boxed(b"twos", &e), &BE16, 2, 4));
    assert_eq!(p.codec_id.as_str(), "pcm_s16le");
    assert_eq!(data, LE16, "twos is big-endian");
    // 8-bit `twos` is signed; `raw ` is offset binary.
    let e8 = audio_preamble(0, 1, 8, 8_000);
    let (p, _) = demux(file_with(0, boxed(b"twos", &e8), &[1, 2, 3], 3, 1));
    assert_eq!(p.codec_id.as_str(), "pcm_s8");
    let (p, data) = demux(file_with(0, boxed(b"raw ", &e8), &[1, 2, 3], 3, 1));
    assert_eq!(p.codec_id.as_str(), "pcm_u8");
    assert_eq!(data, [1, 2, 3]);
}

/// Version-0 QuickTime sound descriptions count one sample per PCM
/// frame but record a sample size of 1 byte.
#[test]
fn quicktime_sample_size_one_convention() {
    let e = audio_preamble(0, 2, 16, 48_000);
    let (_, data) = demux(file_with(0, boxed(b"twos", &e), &BE16, 2, 1));
    assert_eq!(data, LE16);
}

#[test]
fn quicktime_v1_in24_with_enda() {
    // Sound description v1: four u32 after the v0 fields, then `wave`
    // holding `frma` + `enda` (1 = little-endian) + terminator.
    let mut e = audio_preamble(1, 1, 16, 48_000);
    for v in [1u32, 3, 3, 3] {
        e.extend_from_slice(&v.to_be_bytes());
    }
    for (le, payload, want) in [
        (1u16, [0x01u8, 0x02, 0x03], [0x01u8, 0x02, 0x03]),
        (0, [0x01, 0x02, 0x03], [0x03, 0x02, 0x01]),
    ] {
        let mut wave = boxed(b"frma", b"in24");
        wave.extend_from_slice(&boxed(b"enda", &le.to_be_bytes()));
        wave.extend_from_slice(&[0, 0, 0, 8, 0, 0, 0, 0]);
        let mut entry = e.clone();
        entry.extend_from_slice(&boxed(b"wave", &wave));
        let (p, data) = demux(file_with(0, boxed(b"in24", &entry), &payload, 1, 3));
        assert_eq!(p.codec_id.as_str(), "pcm_s24le");
        assert_eq!(data, want, "enda = {le}");
    }
}

#[test]
fn quicktime_v2_lpcm_flags() {
    // Sound description v2: Float64 rate, channel count,
    // constBitsPerChannel and the LPCM formatSpecificFlags.
    let lpcm = |flags: u32, bits: u32, channels: u32, rate: f64| {
        let mut e = audio_preamble(2, 3, 16, 0);
        e[16..28].copy_from_slice(&[0, 3, 0, 16, 0xff, 0xfe, 0, 0, 0, 1, 0, 0]);
        e.extend_from_slice(&72u32.to_be_bytes());
        e.extend_from_slice(&rate.to_bits().to_be_bytes());
        e.extend_from_slice(&channels.to_be_bytes());
        e.extend_from_slice(&0x7F00_0000u32.to_be_bytes());
        e.extend_from_slice(&bits.to_be_bytes());
        e.extend_from_slice(&flags.to_be_bytes());
        e.extend_from_slice(&(bits / 8 * channels).to_be_bytes());
        e.extend_from_slice(&1u32.to_be_bytes());
        boxed(b"lpcm", &e)
    };
    // Signed big-endian 16-bit, stereo, 88.2 kHz.
    let (p, data) = demux(file_with(
        0,
        lpcm(0x2 | 0x4 | 0x8, 16, 2, 88_200.0),
        &BE16,
        2,
        4,
    ));
    assert_eq!(p.codec_id.as_str(), "pcm_s16le");
    assert_eq!(p.sample_rate, Some(88_200));
    assert_eq!(p.channels, Some(2));
    assert_eq!(data, LE16);
    // Little-endian float, mono.
    let le = 0.25f32.to_le_bytes();
    let (p, data) = demux(file_with(0, lpcm(0x1 | 0x8, 32, 1, 44_100.0), &le, 1, 4));
    assert_eq!(p.codec_id.as_str(), "pcm_f32le");
    assert_eq!(data, le);
}
