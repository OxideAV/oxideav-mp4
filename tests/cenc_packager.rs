//! End-to-end tests for the write-side CENC packager
//! (`CencFragmentPackager`): plaintext packets in, self-describing
//! protected fragmented MP4 out — IV generation, all-scheme
//! encryption, subsample maps, key rotation — gated by this crate's
//! own demux + decrypt (ISO/IEC 23001-7 §7 + §9 + §10).

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::cenc::{parse_seig, CencScheme, CencSchemeDecision, SubsampleEntry, TencBox};
use oxideav_mp4::cenc_cipher::decrypt_sample_in_place;
use oxideav_mp4::cenc_packager::{CencFragmentPackager, TrackKey};
use oxideav_mp4::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions, TrackProtection};

const KEY_A: [u8; 16] = [
    0x2B, 0x7E, 0x15, 0x16, 0x28, 0xAE, 0xD2, 0xA6, 0xAB, 0xF7, 0x15, 0x88, 0x09, 0xCF, 0x4F, 0x3C,
];
const KEY_B: [u8; 16] = [
    0x1F, 0x35, 0x2C, 0x07, 0x3B, 0x61, 0x08, 0xD7, 0x2D, 0x98, 0x10, 0xA3, 0x09, 0x14, 0xDF, 0xF4,
];
const KID_A: [u8; 16] = [0xA1; 16];
const KID_B: [u8; 16] = [0xB2; 16];

fn pcm_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn plaintext(i: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|b| (b as u8).wrapping_mul(29).wrapping_add(i as u8 * 3 + 5))
        .collect()
}

fn audio_packet(stream: &StreamInfo, i: i64, data: Vec<u8>) -> Packet {
    let mut pkt = Packet::new(stream.index, stream.time_base, data);
    pkt.pts = Some(i * 1024);
    pkt.duration = Some(1024);
    pkt.flags.keyframe = true;
    pkt
}

fn frag_opts(n: u32) -> FragmentedOptions {
    FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(n),
        emit_random_access_indexes: false,
        styp: None,
        ..FragmentedOptions::default()
    }
}

fn tenc_cenc_iv8() -> TencBox {
    TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: KID_A,
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    }
}

fn tenc_cbcs(constant_iv: Vec<u8>) -> TencBox {
    TencBox {
        version: 1,
        default_is_protected: 1,
        default_per_sample_iv_size: 0,
        default_kid: KID_A,
        default_crypt_byte_block: 1,
        default_skip_byte_block: 9,
        default_constant_iv: Some(constant_iv),
    }
}

fn options_for(scheme: [u8; 4], tenc: TencBox, frag: FragmentedOptions) -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        fragmented: Some(frag),
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: scheme,
            scheme_version: 0x0001_0000,
            tenc,
        }],
        ..Mp4MuxerOptions::default()
    }
}

fn new_packager(
    path: &std::path::Path,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
    keys: Vec<TrackKey>,
) -> CencFragmentPackager {
    let frag = options.fragmented.clone().unwrap();
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    CencFragmentPackager::new(ws, streams, options, frag, keys).unwrap()
}

fn demux_file(path: &std::path::Path) -> oxideav_mp4::demux::Mp4Demuxer {
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap()
}

fn drain(dmx: &mut oxideav_mp4::demux::Mp4Demuxer) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    got
}

/// Plaintext in → packager (cenc, per-sample 8-byte IVs it generates
/// itself) → demux → decrypt via the file's senc records → byte-exact
/// plaintext. The generated IVs must be unique counter values.
#[test]
fn packager_cenc_full_sample_end_to_end() {
    let stream = pcm_stream(0);
    let tenc = tenc_cenc_iv8();
    let options = options_for(*b"cenc", tenc.clone(), frag_opts(2));
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 100)).collect();
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-cenc.mp4");
    {
        let mut pkg = new_packager(
            &tmp,
            std::slice::from_ref(&stream),
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        for (i, plain) in plaintexts.iter().enumerate() {
            pkg.write_packet(&audio_packet(&stream, i as i64, plain.clone()))
                .unwrap();
        }
        pkg.write_trailer().unwrap();
    }

    let mut dmx = demux_file(&tmp);
    let entries: Vec<_> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.clone())
        .collect();
    assert_eq!(entries.len(), 4);
    // Counter-shaped unique IVs: big-endian counter 1..=4 in bytes 0..8.
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(
            e.initialization_vector,
            ((i as u64) + 1).to_be_bytes().to_vec(),
            "IV of sample {i} is the counter value"
        );
    }

    let decision = CencSchemeDecision::new(CencScheme::Cenc, tenc).unwrap();
    let mut got = drain(&mut dmx);
    assert_eq!(got.len(), 4);
    for (i, data) in got.iter_mut().enumerate() {
        assert_ne!(data, &plaintexts[i], "payload must be ciphered on disk");
        decrypt_sample_in_place(
            &decision,
            &KEY_A,
            Some(&entries[i].initialization_vector),
            None,
            data,
        )
        .unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}

/// cbcs pattern encryption (1:9, constant IV) with per-sample subsample
/// maps through `write_packet_with_subsamples` — the video-track shape.
/// Clear prefixes must survive on disk verbatim (§9.5).
#[test]
fn packager_cbcs_pattern_subsamples_end_to_end() {
    let stream = pcm_stream(0);
    let constant_iv = vec![0x66; 16];
    let tenc = tenc_cbcs(constant_iv);
    let options = options_for(*b"cbcs", tenc.clone(), frag_opts(2));
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 100)).collect();
    let subs = [SubsampleEntry {
        bytes_of_clear_data: 20,
        bytes_of_protected_data: 80,
    }];
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-cbcs.mp4");
    {
        let mut pkg = new_packager(
            &tmp,
            std::slice::from_ref(&stream),
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        for (i, plain) in plaintexts.iter().enumerate() {
            pkg.write_packet_with_subsamples(
                &audio_packet(&stream, i as i64, plain.clone()),
                &subs,
            )
            .unwrap();
        }
        pkg.write_trailer().unwrap();
    }

    let mut dmx = demux_file(&tmp);
    let entries: Vec<_> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.clone())
        .collect();
    assert_eq!(entries.len(), 4);
    for e in &entries {
        assert!(e.initialization_vector.is_empty(), "constant IV — no bytes");
        assert_eq!(e.subsamples.len(), 1);
    }

    let decision = CencSchemeDecision::new(CencScheme::Cbcs, tenc).unwrap();
    let mut got = drain(&mut dmx);
    for (i, data) in got.iter_mut().enumerate() {
        assert_eq!(
            &data[..20],
            &plaintexts[i][..20],
            "BytesOfClearData prefix stays clear on disk"
        );
        assert_ne!(data, &plaintexts[i]);
        decrypt_sample_in_place(&decision, &KEY_A, None, Some(&entries[i].subsamples), data)
            .unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}

/// Mid-stream key rotation through the packager: samples 0–1 under the
/// default key, `rotate_key` to (KID_B, KEY_B), samples 2–3 under the
/// rotated key. The decrypt side resolves each sample's KID purely
/// from the file (sbgp/sgpd seig resolution) and recovers everything.
#[test]
fn packager_key_rotation_end_to_end() {
    let stream = pcm_stream(0);
    let tenc = tenc_cenc_iv8();
    let options = options_for(*b"cenc", tenc.clone(), frag_opts(2));
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 80)).collect();
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-rotate.mp4");
    {
        let mut pkg = new_packager(
            &tmp,
            std::slice::from_ref(&stream),
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        for (i, plain) in plaintexts.iter().enumerate() {
            if i == 2 {
                pkg.rotate_key(0, KID_B, KEY_B, None).unwrap();
            }
            pkg.write_packet(&audio_packet(&stream, i as i64, plain.clone()))
                .unwrap();
        }
        pkg.write_trailer().unwrap();
    }

    let mut dmx = demux_file(&tmp);
    let keystore: std::collections::HashMap<[u8; 16], [u8; 16]> =
        [(KID_A, KEY_A), (KID_B, KEY_B)].into_iter().collect();

    // Resolve per-(moof, sample) KIDs from the fragment-local groups.
    let mut kids_by_moof: std::collections::HashMap<u32, Vec<[u8; 16]>> = Default::default();
    for rec in dmx.traf_sample_groups() {
        let sbgp = &rec.sbgp[0];
        let sgpd = &rec.sgpd[0];
        let mut kids = Vec::new();
        for &(count, index) in &sbgp.entries {
            let kid = if index == 0 {
                KID_A
            } else {
                parse_seig(&sgpd.entries[(index - 0x10001) as usize])
                    .unwrap()
                    .kid
            };
            kids.extend(std::iter::repeat_n(kid, count as usize));
        }
        kids_by_moof.insert(rec.moof_sequence, kids);
    }
    // Fragment 1 has no groups (all defaults); fragment 2 is fully
    // rotated.
    assert!(!kids_by_moof.contains_key(&1), "fragment 1: no seig boxes");
    assert_eq!(kids_by_moof[&2], vec![KID_B, KID_B]);

    let entries: Vec<_> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.clone())
        .collect();
    let decision = CencSchemeDecision::new(CencScheme::Cenc, tenc).unwrap();
    let mut got = drain(&mut dmx);
    for (i, data) in got.iter_mut().enumerate() {
        let moof_seq = (i / 2) as u32 + 1;
        let kid = kids_by_moof
            .get(&moof_seq)
            .map(|v| v[i % 2])
            .unwrap_or(KID_A);
        let key = keystore[&kid];
        decrypt_sample_in_place(
            &decision,
            &key,
            Some(&entries[i].initialization_vector),
            None,
            data,
        )
        .unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}

/// `rotate_key` back to the default KID (with the default constant-IV
/// state) clears the override — subsequent samples map to group 0.
#[test]
fn rotate_back_to_default_clears_override() {
    let stream = pcm_stream(0);
    let options = options_for(*b"cenc", tenc_cenc_iv8(), frag_opts(3));
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-rotate-back.mp4");
    {
        let mut pkg = new_packager(
            &tmp,
            std::slice::from_ref(&stream),
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        pkg.write_packet(&audio_packet(&stream, 0, plaintext(0, 32)))
            .unwrap();
        pkg.rotate_key(0, KID_B, KEY_B, None).unwrap();
        pkg.write_packet(&audio_packet(&stream, 1, plaintext(1, 32)))
            .unwrap();
        pkg.rotate_key(0, KID_A, KEY_A, None).unwrap();
        pkg.write_packet(&audio_packet(&stream, 2, plaintext(2, 32)))
            .unwrap();
        pkg.write_trailer().unwrap();
    }
    let dmx = demux_file(&tmp);
    let rec = &dmx.traf_sample_groups()[0];
    assert_eq!(
        rec.sbgp[0].entries,
        vec![(1, 0), (1, 0x10001), (1, 0)],
        "default → rotated → default"
    );
}

/// A second, unprotected stream passes through the packager untouched.
#[test]
fn unprotected_stream_passes_through() {
    let streams = [pcm_stream(0), pcm_stream(1)];
    let options = options_for(*b"cenc", tenc_cenc_iv8(), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-passthrough.mp4");
    let clear_payload = plaintext(9, 64);
    {
        let mut pkg = new_packager(
            &tmp,
            &streams,
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        for i in 0..2i64 {
            pkg.write_packet(&audio_packet(&streams[0], i, plaintext(i as usize, 64)))
                .unwrap();
            pkg.write_packet(&audio_packet(&streams[1], i, clear_payload.clone()))
                .unwrap();
        }
        pkg.write_trailer().unwrap();
    }
    let mut dmx = demux_file(&tmp);
    assert_eq!(
        dmx.streams()[1].params.options.get("protection_scheme"),
        None,
        "stream 1 is unprotected"
    );
    let got = drain(&mut dmx);
    assert!(
        got.iter().filter(|d| *d == &clear_payload).count() == 2,
        "clear track payloads pass through verbatim"
    );
}

// --- construction / rotation errors ---------------------------------------

#[test]
fn missing_key_for_protected_stream_fails_at_new() {
    let stream = pcm_stream(0);
    let options = options_for(*b"cenc", tenc_cenc_iv8(), frag_opts(2));
    let frag = options.fragmented.clone().unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let err = CencFragmentPackager::new(ws, std::slice::from_ref(&stream), options, frag, [])
        .err()
        .expect("must fail");
    assert!(format!("{err}").contains("no content key"), "err = {err}");
}

#[test]
fn key_for_unprotected_stream_fails_at_new() {
    let stream = pcm_stream(0);
    let options = Mp4MuxerOptions {
        fragmented: Some(frag_opts(2)),
        ..Mp4MuxerOptions::default()
    };
    let frag = options.fragmented.clone().unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let err = CencFragmentPackager::new(
        ws,
        std::slice::from_ref(&stream),
        options,
        frag,
        vec![TrackKey {
            stream_index: 0,
            key: KEY_A,
        }],
    )
    .err()
    .expect("must fail");
    assert!(
        format!("{err}").contains("no track_protection"),
        "err = {err}"
    );
}

#[test]
fn rotation_constant_iv_discipline() {
    // Per-sample-IV track: a constant IV on rotate is rejected.
    let stream = pcm_stream(0);
    let options = options_for(*b"cenc", tenc_cenc_iv8(), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-rot-civ1.mp4");
    let mut pkg = new_packager(
        &tmp,
        std::slice::from_ref(&stream),
        options,
        vec![TrackKey {
            stream_index: 0,
            key: KEY_A,
        }],
    );
    assert!(pkg.rotate_key(0, KID_B, KEY_B, Some(vec![0; 16])).is_err());

    // Constant-IV track: rotating WITHOUT a fresh constant IV is
    // rejected; with one it succeeds.
    let options = options_for(*b"cbcs", tenc_cbcs(vec![0x11; 16]), frag_opts(2));
    let tmp2 = std::env::temp_dir().join("oxideav-mp4-pkg-rot-civ2.mp4");
    let mut pkg = new_packager(
        &tmp2,
        std::slice::from_ref(&stream),
        options,
        vec![TrackKey {
            stream_index: 0,
            key: KEY_A,
        }],
    );
    assert!(pkg.rotate_key(0, KID_B, KEY_B, None).is_err());
    pkg.rotate_key(0, KID_B, KEY_B, Some(vec![0x22; 16]))
        .unwrap();
}

#[test]
fn subsamples_on_unprotected_stream_are_rejected() {
    let streams = [pcm_stream(0), pcm_stream(1)];
    let options = options_for(*b"cenc", tenc_cenc_iv8(), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-subs-unprot.mp4");
    let mut pkg = new_packager(
        &tmp,
        &streams,
        options,
        vec![TrackKey {
            stream_index: 0,
            key: KEY_A,
        }],
    );
    pkg.write_header().unwrap();
    let subs = [SubsampleEntry {
        bytes_of_clear_data: 4,
        bytes_of_protected_data: 12,
    }];
    let err = pkg
        .write_packet_with_subsamples(&audio_packet(&streams[1], 0, vec![0; 16]), &subs)
        .unwrap_err();
    assert!(format!("{err}").contains("unprotected"), "err = {err}");
}

/// The usual rotation workflow pairs the key switch with a
/// fragment-scoped licence blob: `set_next_segment_pssh` +
/// `rotate_key` before the rotated samples. The pssh must land inside
/// the moof of the fragment carrying the rotated samples (ISO/IEC
/// 23001-7 §8.1.1 scoping) and everything still decrypts.
#[test]
fn rotation_with_moof_scoped_pssh() {
    let stream = pcm_stream(0);
    let tenc = tenc_cenc_iv8();
    let options = options_for(*b"cenc", tenc.clone(), frag_opts(2));
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 64)).collect();
    let tmp = std::env::temp_dir().join("oxideav-mp4-pkg-rot-pssh.mp4");
    {
        let mut pkg = new_packager(
            &tmp,
            std::slice::from_ref(&stream),
            options,
            vec![TrackKey {
                stream_index: 0,
                key: KEY_A,
            }],
        );
        pkg.write_header().unwrap();
        for (i, plain) in plaintexts.iter().enumerate() {
            if i == 2 {
                // New licence blob scoped to the rotated fragment.
                pkg.set_next_segment_pssh([oxideav_mp4::cenc::PsshBox {
                    version: 1,
                    system_id: [0xEE; 16],
                    kids: vec![KID_B],
                    data: vec![0xFA, 0xCE],
                }]);
                pkg.rotate_key(0, KID_B, KEY_B, None).unwrap();
            }
            pkg.write_packet(&audio_packet(&stream, i as i64, plain.clone()))
                .unwrap();
        }
        pkg.write_trailer().unwrap();
    }

    let mut dmx = demux_file(&tmp);
    // The pssh scopes to fragment 2 (the rotated one).
    let moof_psshes = dmx.moof_psshes();
    assert_eq!(moof_psshes.len(), 1);
    assert_eq!(moof_psshes[0].moof_sequence, 2, "§8.1.1 fragment scoping");
    assert_eq!(moof_psshes[0].pssh.kids, vec![KID_B]);

    // And the rotated fragment still decrypts under KEY_B.
    let entries: Vec<_> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.clone())
        .collect();
    let decision = CencSchemeDecision::new(CencScheme::Cenc, tenc).unwrap();
    let mut got = drain(&mut dmx);
    for (i, data) in got.iter_mut().enumerate() {
        let key = if i < 2 { KEY_A } else { KEY_B };
        decrypt_sample_in_place(
            &decision,
            &key,
            Some(&entries[i].initialization_vector),
            None,
            data,
        )
        .unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}
