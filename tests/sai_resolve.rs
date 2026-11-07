//! `Mp4Demuxer::resolve_sai_aux_info` — bridging the `senc`-less CENC
//! carriage (per-sample auxiliary information located by `saiz`+`saio`,
//! ISO/IEC 14496-12 §8.7.8–9 / §8.8.14) into the `senc_records`
//! surface.
//!
//! Strategy: package a protected fragmented file with this crate's own
//! muxer (which writes `senc` + a `saio` whose single offset points at
//! the `senc` entry table), then byte-surgically rename the `senc`
//! boxes to `skip` — the aux-info *bytes* stay in place at the offsets
//! the `saio` names, exactly the shape of a producer that carries aux
//! info without a `senc`. The demuxer then sees `saiz`/`saio` but no
//! `senc`; `resolve_sai_aux_info` must fetch + parse the run and the
//! decrypt loop driven by the synthesised records must recover
//! byte-exact plaintext.

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::cenc::{CencScheme, CencSchemeDecision, SencSample, SubsampleEntry, TencBox};
use oxideav_mp4::cenc_cipher::{decrypt_sample_in_place, encrypt_sample_in_place};
use oxideav_mp4::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions, TrackProtection};

const KEY: [u8; 16] = [
    0x60, 0x3D, 0xEB, 0x10, 0x15, 0xCA, 0x71, 0xBE, 0x2B, 0x73, 0xAE, 0xF0, 0x85, 0x7D, 0x77, 0x81,
];

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

fn plaintext(i: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|b| (b as u8).wrapping_mul(13).wrapping_add(i as u8 * 7 + 1))
        .collect()
}

fn tenc_iv8() -> TencBox {
    TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: [0x42; 16],
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    }
}

/// Per-sample aux-info ground truth: `(iv, subsamples)`.
type AuxTruth = Vec<(Vec<u8>, Vec<SubsampleEntry>)>;
/// `(file bytes, plaintexts, aux truth, routing decision)`.
type Packaged = (Vec<u8>, Vec<Vec<u8>>, AuxTruth, CencSchemeDecision);

/// Package four encrypted samples (two fragments) under `cenc`, with a
/// subsample map on the last two samples. Returns the file bytes plus
/// the per-sample `(iv, subsamples)` ground truth.
fn package(tmp_name: &str) -> Packaged {
    let tenc = tenc_iv8();
    let decision = CencSchemeDecision::new(CencScheme::Cenc, tenc.clone()).unwrap();
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 100)).collect();
    let mut aux: Vec<(Vec<u8>, Vec<SubsampleEntry>)> = Vec::new();
    for i in 0..4usize {
        let iv = vec![i as u8 + 1; 8];
        let subs = if i >= 2 {
            vec![
                SubsampleEntry {
                    bytes_of_clear_data: 30,
                    bytes_of_protected_data: 40,
                },
                SubsampleEntry {
                    bytes_of_clear_data: 10,
                    bytes_of_protected_data: 20,
                },
            ]
        } else {
            Vec::new()
        };
        aux.push((iv, subs));
    }

    let stream = pcm_stream();
    let options = Mp4MuxerOptions::default()
        .with_fragmented(Some(
            FragmentedOptions::default()
                .with_cadence(FragmentCadence::EveryNPackets(2))
                .with_emit_random_access_indexes(false)
                .with_styp(None),
        ))
        .with_track_protection(vec![TrackProtection::new(0, *b"cenc", 0x0001_0000, tenc)]);
    let tmp = std::env::temp_dir().join(tmp_name);
    {
        let frag = options.fragmented.clone().unwrap();
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::frag::open_fragmented_typed(
            ws,
            std::slice::from_ref(&stream),
            options,
            frag,
        )
        .unwrap();
        mux.write_header().unwrap();
        for (i, plain) in plaintexts.iter().enumerate() {
            let (iv, subs) = &aux[i];
            let mut data = plain.clone();
            encrypt_sample_in_place(
                &decision,
                &KEY,
                Some(iv.as_slice()),
                if subs.is_empty() {
                    None
                } else {
                    Some(subs.as_slice())
                },
                &mut data,
            )
            .unwrap();
            let mut pkt = Packet::new(0, stream.time_base, data);
            pkt.pts = Some(i as i64 * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_protected_packet(
                &pkt,
                SencSample {
                    initialization_vector: iv.clone(),
                    subsamples: subs.clone(),
                },
            )
            .unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    (bytes, plaintexts, aux, decision)
}

fn read_u32(b: &[u8], at: usize) -> usize {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// Rename every `senc` inside a `moof/traf` to `skip` (a legal
/// ignored box), leaving the entry-table bytes — which the `saio`
/// offsets point at — untouched.
fn rename_senc_to_skip(bytes: &mut [u8]) -> usize {
    let mut renamed = 0;
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = read_u32(bytes, pos).max(8);
        if &bytes[pos + 4..pos + 8] == b"moof" {
            let end = (pos + size).min(bytes.len());
            let mut c = pos + 8;
            while c + 8 <= end {
                let csize = read_u32(bytes, c).max(8);
                if &bytes[c + 4..c + 8] == b"traf" {
                    let tend = (c + csize).min(bytes.len());
                    let mut t = c + 8;
                    while t + 8 <= tend {
                        let tsize = read_u32(bytes, t).max(8);
                        if &bytes[t + 4..t + 8] == b"senc" {
                            bytes[t + 4..t + 8].copy_from_slice(b"skip");
                            renamed += 1;
                        }
                        t += tsize;
                    }
                }
                c += csize;
            }
        }
        pos += size;
    }
    renamed
}

/// The senc-less carriage: `saiz`/`saio` alone drive decryption after
/// `resolve_sai_aux_info` synthesises the records.
#[test]
fn resolve_sai_aux_info_bridges_senc_less_files() {
    let (mut bytes, plaintexts, aux, decision) = package("oxideav-mp4-sai-resolve.mp4");
    assert_eq!(rename_senc_to_skip(&mut bytes), 2, "two fragments' senc");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert!(dmx.senc_records().is_empty(), "no senc boxes left to parse");
    assert_eq!(dmx.sai_records().len(), 2, "saiz/saio survive per traf");

    let added = dmx.resolve_sai_aux_info().unwrap();
    assert_eq!(added, 2, "one synthesised record per fragment");

    // The synthesised records carry the exact per-sample ground truth.
    let entries: Vec<SencSample> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.iter().cloned())
        .collect();
    assert_eq!(entries.len(), 4);
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.initialization_vector, aux[i].0, "sample {i} IV");
        assert_eq!(e.subsamples, aux[i].1, "sample {i} subsample map");
    }

    // And they drive decryption to byte-exact plaintext.
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), 4);
    for (i, data) in got.iter_mut().enumerate() {
        let e = &entries[i];
        decrypt_sample_in_place(
            &decision,
            &KEY,
            Some(e.initialization_vector.as_slice()),
            if e.subsamples.is_empty() {
                None
            } else {
                Some(e.subsamples.as_slice())
            },
            data,
        )
        .unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}

/// A file whose fragments already carry `senc` gains nothing — the
/// parsed boxes stay authoritative and no duplicates appear.
#[test]
fn resolve_sai_aux_info_noop_when_senc_present() {
    let (bytes, _plain, _aux, _decision) = package("oxideav-mp4-sai-resolve-noop.mp4");
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.senc_records().len(), 2, "senc parsed as usual");
    let added = dmx.resolve_sai_aux_info().unwrap();
    assert_eq!(added, 0, "senc stays authoritative");
    assert_eq!(dmx.senc_records().len(), 2, "no duplicate records");
}

/// Hostile shape: a `saio` pointing past EOF is skipped without
/// failing the resolve (truncated-capture posture).
#[test]
fn resolve_sai_aux_info_skips_out_of_range_offset() {
    let (mut bytes, _plain, _aux, _decision) = package("oxideav-mp4-sai-resolve-oob.mp4");
    assert_eq!(rename_senc_to_skip(&mut bytes), 2);
    // Corrupt every saio offset to point far past EOF. The v0 saio our
    // muxer writes is FullBox(8) + entry_count(4) + offset(4); find the
    // boxes and blow up the offset field.
    let mut patched = 0;
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        if &bytes[pos..pos + 4] == b"saio" {
            // fourcc at `pos`; body = version/flags(4) + count(4) + u32 offset
            let off_at = pos + 4 + 4 + 4;
            if off_at + 4 <= bytes.len() {
                bytes[off_at..off_at + 4].copy_from_slice(&0x7FFF_FF00u32.to_be_bytes());
                patched += 1;
            }
        }
        pos += 1;
    }
    assert_eq!(patched, 2, "both saio offsets corrupted");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    let added = dmx.resolve_sai_aux_info().unwrap();
    assert_eq!(added, 0, "out-of-range runs skipped, no error");
}
