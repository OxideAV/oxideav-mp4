//! End-to-end CENC round-trip gate: plaintext → write-side cipher
//! (`encrypt_sample_in_place`) → fragmented packaging with per-fragment
//! `senc`/`saiz`/`saio` (+ `seig` groups for key rotation) → this
//! crate's own demuxer → per-sample decrypt driven **only** by what the
//! file says (tenc defaults + senc records + fragment-local sample
//! groups) → byte-exact plaintext.
//!
//! Every ISO/IEC 23001-7 §10 scheme is exercised across a multi-
//! fragment shape: `cenc` (§10.1, CTR full-sample + subsample), `cbc1`
//! (§10.2, CBC full-sample), `cens` (§10.3, CTR pattern subsample),
//! `cbcs` (§10.4, CBC pattern subsample + whole-block full-sample with
//! a constant IV).

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::cenc::{
    parse_seig, CencScheme, CencSchemeDecision, SencSample, SubsampleEntry, TencBox,
};
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

fn frag_options() -> FragmentedOptions {
    FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(2),
        emit_random_access_indexes: false,
        styp: None,
        ..FragmentedOptions::default()
    }
}

fn mux_options(scheme: [u8; 4], tenc: TencBox) -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        fragmented: Some(frag_options()),
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: scheme,
            scheme_version: 0x0001_0000,
            tenc,
        }],
        ..Mp4MuxerOptions::default()
    }
}

/// Package `plaintexts` under `(scheme, tenc)` — encrypting each sample
/// with the write-side cipher and the given per-sample IVs / subsample
/// maps — then demux and decrypt using only what the file describes,
/// asserting byte-exact recovery.
fn roundtrip_scheme(
    name: &str,
    scheme: CencScheme,
    tenc: TencBox,
    ivs: &[Vec<u8>],
    subsamples: &[Vec<SubsampleEntry>],
) {
    let decision = CencSchemeDecision::new(scheme, tenc.clone()).unwrap();
    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 100)).collect();

    let stream = pcm_stream();
    let options = mux_options(scheme.fourcc(), tenc.clone());
    let tmp = std::env::temp_dir().join(format!("oxideav-mp4-cenc-rt-{name}.mp4"));
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
            let mut data = plain.clone();
            let iv = if ivs[i].is_empty() {
                None
            } else {
                Some(ivs[i].as_slice())
            };
            let subs = if subsamples[i].is_empty() {
                None
            } else {
                Some(subsamples[i].as_slice())
            };
            encrypt_sample_in_place(&decision, &KEY, iv, subs, &mut data).unwrap();
            assert_ne!(&data, plain, "{name}: sample {i} must be ciphered");
            let mut pkt = Packet::new(0, stream.time_base, data);
            pkt.pts = Some(i as i64 * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            let senc = SencSample {
                initialization_vector: ivs[i].clone(),
                subsamples: subsamples[i].clone(),
            };
            mux.write_protected_packet(&pkt, senc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Demux; rebuild the routing decision from the file's own
    // signalling (scheme + tenc surface on params.options).
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    let params = dmx.streams()[0].params.clone();
    assert_eq!(
        params.options.get("protection_scheme"),
        Some(std::str::from_utf8(&scheme.fourcc()).unwrap()),
        "{name}: scheme signalling"
    );

    // Per-sample aux info from the per-fragment senc records (in moof
    // order, samples in trun order) — or, on the empty-aux constant-IV
    // path, from nothing at all.
    let senc_entries: Vec<SencSample> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.iter().cloned())
        .collect();
    let expects_aux =
        ivs.iter().any(|iv| !iv.is_empty()) || subsamples.iter().any(|s| !s.is_empty());
    if expects_aux {
        assert_eq!(
            senc_entries.len(),
            plaintexts.len(),
            "{name}: every sample has a senc entry"
        );
    } else {
        assert!(
            senc_entries.is_empty(),
            "{name}: §7.1 empty aux info must be omitted"
        );
    }

    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{name}: demux error: {e}"),
        }
    }
    assert_eq!(got.len(), plaintexts.len());

    for (i, data) in got.iter_mut().enumerate() {
        let (iv, subs): (Option<&[u8]>, Option<&[SubsampleEntry]>) = if expects_aux {
            let e = &senc_entries[i];
            (
                Some(e.initialization_vector.as_slice()),
                if e.subsamples.is_empty() {
                    None
                } else {
                    Some(e.subsamples.as_slice())
                },
            )
        } else {
            (None, None)
        };
        decrypt_sample_in_place(&decision, &KEY, iv, subs, data).unwrap();
        assert_eq!(
            data, &plaintexts[i],
            "{name}: sample {i} must decrypt byte-exact"
        );
    }
}

fn no_subs() -> Vec<Vec<SubsampleEntry>> {
    vec![Vec::new(); 4]
}

fn iv8s() -> Vec<Vec<u8>> {
    (0..4u8).map(|i| vec![i + 1; 8]).collect()
}

fn iv16s() -> Vec<Vec<u8>> {
    (0..4u8).map(|i| vec![i + 0x11; 16]).collect()
}

fn tenc_plain(version: u8, iv_size: u8) -> TencBox {
    TencBox {
        version,
        default_is_protected: 1,
        default_per_sample_iv_size: iv_size,
        default_kid: [0x42; 16],
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    }
}

fn tenc_pattern(iv_size: u8, crypt: u8, skip: u8, constant_iv: Option<Vec<u8>>) -> TencBox {
    TencBox {
        version: 1,
        default_is_protected: 1,
        default_per_sample_iv_size: iv_size,
        default_kid: [0x42; 16],
        default_crypt_byte_block: crypt,
        default_skip_byte_block: skip,
        default_constant_iv: constant_iv,
    }
}

/// One subsample map per sample covering the 100-byte payload:
/// 20 clear + 80 protected (80 = 5 whole AES blocks, legal for the
/// CBC schemes too).
fn subs_20_80() -> Vec<Vec<SubsampleEntry>> {
    vec![
        vec![SubsampleEntry {
            bytes_of_clear_data: 20,
            bytes_of_protected_data: 80,
        }];
        4
    ]
}

#[test]
fn cenc_full_sample_roundtrip() {
    roundtrip_scheme(
        "cenc-full",
        CencScheme::Cenc,
        tenc_plain(0, 8),
        &iv8s(),
        &no_subs(),
    );
}

#[test]
fn cenc_subsample_roundtrip() {
    roundtrip_scheme(
        "cenc-subs",
        CencScheme::Cenc,
        tenc_plain(0, 8),
        &iv8s(),
        &subs_20_80(),
    );
}

#[test]
fn cenc_iv16_full_sample_roundtrip() {
    roundtrip_scheme(
        "cenc-iv16",
        CencScheme::Cenc,
        tenc_plain(0, 16),
        &iv16s(),
        &no_subs(),
    );
}

#[test]
fn cbc1_full_sample_roundtrip() {
    // §10.2 pins Per_Sample_IV_Size = 16 for cbc1. The 100-byte sample
    // leaves a 4-byte partial tail clear (§9.4.3).
    roundtrip_scheme(
        "cbc1-full",
        CencScheme::Cbc1,
        tenc_plain(0, 16),
        &iv16s(),
        &no_subs(),
    );
}

#[test]
fn cbc1_subsample_roundtrip() {
    roundtrip_scheme(
        "cbc1-subs",
        CencScheme::Cbc1,
        tenc_plain(0, 16),
        &iv16s(),
        &subs_20_80(),
    );
}

#[test]
fn cens_pattern_subsample_roundtrip() {
    // §10.3: cens = CTR pattern encryption. Pattern 2:1 over the
    // 80-byte protected range.
    roundtrip_scheme(
        "cens-2-1",
        CencScheme::Cens,
        tenc_pattern(8, 2, 1, None),
        &iv8s(),
        &subs_20_80(),
    );
}

#[test]
fn cbcs_pattern_subsample_roundtrip() {
    // §10.4: cbcs = CBC pattern encryption with a constant IV
    // (Per_Sample_IV_Size = 0). The canonical 1:9 video pattern; senc
    // entries carry zero IV bytes + the subsample map.
    roundtrip_scheme(
        "cbcs-1-9",
        CencScheme::Cbcs,
        tenc_pattern(0, 1, 9, Some(vec![0x77; 16])),
        &vec![Vec::new(); 4],
        &subs_20_80(),
    );
}

#[test]
fn cbcs_whole_block_full_sample_roundtrip() {
    // §9.7 / §10.4 non-video shape: whole-block full-sample encryption
    // under the constant IV, no subsample maps — the §7.1 empty-aux
    // path where no senc / saiz / saio exists in the file at all and
    // decryption is driven purely by tenc.
    roundtrip_scheme(
        "cbcs-full",
        CencScheme::Cbcs,
        tenc_pattern(0, 1, 0, Some(vec![0x3C; 16])),
        &vec![Vec::new(); 4],
        &no_subs(),
    );
}

// --- key rotation: decrypt via fragment-local seig resolution -------------

/// Full key-rotation loop: two content keys, rotated mid-stream via
/// per-sample `seig` overrides. The decrypting side resolves each
/// sample's KID **from the file** — walking the fragment's
/// `sbgp('seig')` run-length map to a fragment-local `sgpd` entry
/// (§8.9.4 index 0x10001+) or falling back to `tenc.default_KID` — and
/// picks the matching key from its KID-keyed store.
#[test]
fn key_rotation_decrypts_via_seig_resolution() {
    use std::collections::HashMap;

    let kid_a = [0xA1; 16];
    let kid_b = [0xB2; 16];
    let key_a = KEY;
    let key_b: [u8; 16] = [
        0x1F, 0x35, 0x2C, 0x07, 0x3B, 0x61, 0x08, 0xD7, 0x2D, 0x98, 0x10, 0xA3, 0x09, 0x14, 0xDF,
        0xF4,
    ];
    let keystore: HashMap<[u8; 16], [u8; 16]> =
        [(kid_a, key_a), (kid_b, key_b)].into_iter().collect();

    let tenc = TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: kid_a,
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    };
    let seig_b = oxideav_mp4::cenc::SeigEntry {
        crypt_byte_block: 0,
        skip_byte_block: 0,
        is_protected: 1,
        per_sample_iv_size: 8,
        kid: kid_b,
        constant_iv: None,
    };
    // Sample → key schedule: fragment 1 (samples 0, 1) on the default
    // KID A; fragment 2 (samples 2, 3) rotated to KID B via seig.
    let sample_kids = [kid_a, kid_a, kid_b, kid_b];

    let decision_for = |kid: [u8; 16]| {
        let mut t = tenc.clone();
        t.default_kid = kid;
        CencSchemeDecision::new(CencScheme::Cenc, t).unwrap()
    };

    let plaintexts: Vec<Vec<u8>> = (0..4).map(|i| plaintext(i, 96)).collect();
    let ivs = iv8s();

    let stream = pcm_stream();
    let options = mux_options(*b"cenc", tenc.clone());
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-rt-rotation.mp4");
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
            let kid = sample_kids[i];
            let key = keystore[&kid];
            let mut data = plain.clone();
            encrypt_sample_in_place(&decision_for(kid), &key, Some(&ivs[i]), None, &mut data)
                .unwrap();
            let mut pkt = Packet::new(0, stream.time_base, data);
            pkt.pts = Some(i as i64 * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            let senc = SencSample {
                initialization_vector: ivs[i].clone(),
                subsamples: Vec::new(),
            };
            let seig = if kid == kid_b {
                Some(seig_b.clone())
            } else {
                None
            };
            mux.write_protected_packet_grouped(&pkt, senc, seig)
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();

    // Resolve each sample's KID from the file's own group signalling.
    // Build (moof_sequence → per-sample KID list) from the sbgp + sgpd
    // records; fragments without groups fall back to the default KID.
    let mut kids_by_moof: std::collections::HashMap<u32, Vec<[u8; 16]>> = HashMap::new();
    for rec in dmx.traf_sample_groups() {
        let sbgp = rec
            .sbgp
            .iter()
            .find(|b| &b.grouping_type == b"seig")
            .expect("seig sbgp");
        let sgpd = rec
            .sgpd
            .iter()
            .find(|g| &g.grouping_type == b"seig")
            .expect("seig sgpd");
        let mut kids = Vec::new();
        for &(count, index) in &sbgp.entries {
            let kid = if index == 0 {
                tenc.default_kid
            } else {
                assert!(index >= 0x10001, "§8.9.4 fragment-local index");
                let entry = &sgpd.entries[(index - 0x10001) as usize];
                parse_seig(entry).unwrap().kid
            };
            for _ in 0..count {
                kids.push(kid);
            }
        }
        kids_by_moof.insert(rec.moof_sequence, kids);
    }

    // Per-sample IVs from the senc records, keyed the same way.
    let mut ivs_by_moof: std::collections::HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    for rec in dmx.senc_records() {
        ivs_by_moof.insert(
            rec.moof_sequence,
            rec.senc
                .samples
                .iter()
                .map(|s| s.initialization_vector.clone())
                .collect(),
        );
    }

    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), 4);

    // Two samples per fragment (EveryNPackets(2)); moof sequences are
    // 1-based.
    for (i, data) in got.iter_mut().enumerate() {
        let moof_seq = (i / 2) as u32 + 1;
        let k = i % 2;
        let kid = kids_by_moof
            .get(&moof_seq)
            .map(|v| v[k])
            .unwrap_or(tenc.default_kid);
        assert_eq!(
            kid, sample_kids[i],
            "sample {i} resolves to its written KID"
        );
        let key = keystore[&kid];
        let iv = ivs_by_moof[&moof_seq][k].clone();
        decrypt_sample_in_place(&decision_for(kid), &key, Some(&iv), None, data).unwrap();
        assert_eq!(data, &plaintexts[i], "sample {i} decrypts byte-exact");
    }
}
