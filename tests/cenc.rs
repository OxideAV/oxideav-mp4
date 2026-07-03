//! Integration tests for the ISO/IEC 23001-7 (Common Encryption)
//! `tenc` / `pssh` / `senc` parsers, plus the end-to-end surface that
//! routes parsed metadata onto the demuxer's `metadata()` channel and
//! per-stream `params.options`.
//!
//! Strategy: build synthetic boxes byte-for-byte (no external fixtures
//! — the public crate API parses what we wrote), then assert the
//! parsed structures match.

use oxideav_mp4::cenc::{parse_pssh, parse_senc, parse_tenc};

// --- Standalone box parsers ----------------------------------------------

#[test]
fn tenc_v0_per_sample_iv16_round_trip() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8, 0, 0, 0]); // version + flags
    body.push(0); // reserved
    body.push(0); // reserved
    body.push(1); // default_isProtected
    body.push(16); // default_Per_Sample_IV_Size
    let kid: [u8; 16] = [
        0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
        0x99,
    ];
    body.extend_from_slice(&kid);
    let t = parse_tenc(&body).expect("v0 tenc parse");
    assert_eq!(t.version, 0);
    assert_eq!(t.default_is_protected, 1);
    assert_eq!(t.default_per_sample_iv_size, 16);
    assert_eq!(t.default_kid, kid);
    assert!(t.default_constant_iv.is_none());
}

#[test]
fn tenc_v1_cbcs_pattern_1_9_constant_iv_16_round_trip() {
    // CBCS-style track-level default: v1, pattern 1:9, constant IV
    // (16 bytes), no per-sample IVs.
    let mut body = Vec::new();
    body.extend_from_slice(&[1u8, 0, 0, 0]);
    body.push(0); // reserved
    body.push((1 << 4) | 9); // crypt | skip
    body.push(1); // default_isProtected
    body.push(0); // default_Per_Sample_IV_Size = 0 → constant IV
    body.extend_from_slice(&[0x01; 16]); // KID
    body.push(16); // constant_IV_size
    body.extend_from_slice(&[0xAA; 16]);
    let t = parse_tenc(&body).expect("v1 tenc parse");
    assert_eq!(t.version, 1);
    assert_eq!(t.default_crypt_byte_block, 1);
    assert_eq!(t.default_skip_byte_block, 9);
    assert_eq!(t.default_constant_iv.as_deref(), Some(&[0xAA; 16][..]));
}

#[test]
fn pssh_v0_round_trip_with_synthetic_system_id() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8, 0, 0, 0]);
    let sysid: [u8; 16] = [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
        0x1F,
    ];
    body.extend_from_slice(&sysid);
    body.extend_from_slice(&4u32.to_be_bytes());
    body.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let p = parse_pssh(&body).expect("v0 pssh");
    assert_eq!(p.version, 0);
    assert_eq!(p.system_id, sysid);
    assert!(p.kids.is_empty());
    assert_eq!(p.data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
}

#[test]
fn pssh_v1_two_kids_round_trip() {
    let mut body = Vec::new();
    body.extend_from_slice(&[1u8, 0, 0, 0]);
    body.extend_from_slice(&[0x42; 16]);
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&[0xAA; 16]);
    body.extend_from_slice(&[0xBB; 16]);
    body.extend_from_slice(&0u32.to_be_bytes());
    let p = parse_pssh(&body).expect("v1 pssh");
    assert_eq!(p.version, 1);
    assert_eq!(p.kids.len(), 2);
    assert_eq!(p.kids[0], [0xAA; 16]);
    assert_eq!(p.kids[1], [0xBB; 16]);
}

#[test]
fn senc_v0_iv16_no_subsamples_round_trip() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8, 0, 0, 0]);
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&[0x11; 16]);
    body.extend_from_slice(&[0x22; 16]);
    let s = parse_senc(&body, 16).expect("no-sub senc");
    assert_eq!(s.samples.len(), 2);
    assert_eq!(s.samples[0].initialization_vector, vec![0x11; 16]);
    assert_eq!(s.samples[1].initialization_vector, vec![0x22; 16]);
    assert!(!s.uses_subsample_encryption());
}

#[test]
fn senc_subsample_encryption_round_trip() {
    // flags=0x02 (UseSubSampleEncryption), one sample, IV size 8, two
    // subsample entries. Mirrors the NAL-structured-video carriage
    // shape (§9.5.2) where parameter-set NALs are clear and slice
    // bodies are encrypted.
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8, 0, 0, 0x02]);
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&[0x33; 8]); // IV
    body.extend_from_slice(&2u16.to_be_bytes()); // subsample_count
    body.extend_from_slice(&3u16.to_be_bytes()); // clear
    body.extend_from_slice(&17u32.to_be_bytes()); // protected
    body.extend_from_slice(&5u16.to_be_bytes());
    body.extend_from_slice(&11u32.to_be_bytes());
    let s = parse_senc(&body, 8).expect("sub senc");
    assert!(s.uses_subsample_encryption());
    assert_eq!(s.samples[0].subsamples.len(), 2);
    assert_eq!(s.samples[0].subsamples[0].bytes_of_clear_data, 3);
    assert_eq!(s.samples[0].subsamples[0].bytes_of_protected_data, 17);
    assert_eq!(s.samples[0].subsamples[1].bytes_of_clear_data, 5);
    assert_eq!(s.samples[0].subsamples[1].bytes_of_protected_data, 11);
}

#[test]
fn pssh_rejects_kid_array_overrun_without_panic() {
    // KID_count = 100 but only one KID's worth of bytes follows. The
    // parser must reject (not allocate 100×16 bytes pre-validation).
    let mut body = Vec::new();
    body.extend_from_slice(&[1u8, 0, 0, 0]);
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&100u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 16]);
    assert!(parse_pssh(&body).is_err());
}

#[test]
fn senc_rejects_invalid_iv_size_per_spec_9_1() {
    // §9.1 lists 0 / 8 / 16 as the only supported widths.
    let body = vec![0u8, 0, 0, 0, 0, 0, 0, 0];
    assert!(parse_senc(&body, 4).is_err());
    assert!(parse_senc(&body, 12).is_err());
    assert!(parse_senc(&body, 32).is_err());
}

// --- Write side: protected-entry envelope + pssh mux → demux -------------

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::cenc::{CencScheme, CencSchemeDecision, PsshBox, TencBox};
use oxideav_mp4::cenc_cipher::{decrypt_sample_in_place, encrypt_sample_in_place};
use oxideav_mp4::{Mp4MuxerOptions, TrackProtection};

const CONTENT_KEY: [u8; 16] = [
    0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x00,
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

fn cenc_tenc(kid: [u8; 16]) -> TencBox {
    TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: kid,
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    }
}

/// Full black-box loop: encrypt plaintext samples with the write-side
/// cipher, mux them into a protected (`enca` + `sinf`) track with a
/// moov-level `pssh`, demux the file back, verify the protection
/// surface, then decrypt the demuxed ciphertext packets and compare
/// them to the original plaintext.
#[test]
fn protected_mux_demux_decrypt_round_trip() {
    let kid: [u8; 16] = [0x5A; 16];
    let tenc = cenc_tenc(kid);
    let decision = CencSchemeDecision::new(CencScheme::Cenc, tenc.clone()).unwrap();

    // Three plaintext packets; per-sample 8-byte IVs (the senc channel
    // a real packager would write is exercised by the frag muxer —
    // here the IVs travel out-of-band, as §7.2.3 allows).
    let plaintexts: Vec<Vec<u8>> = (0..3u8).map(|i| vec![i.wrapping_mul(29); 100]).collect();
    let ivs: Vec<[u8; 8]> = (0..3u8).map(|i| [i + 1; 8]).collect();
    let mut ciphertexts = plaintexts.clone();
    for ((ct, iv), plain) in ciphertexts.iter_mut().zip(&ivs).zip(&plaintexts) {
        encrypt_sample_in_place(&decision, &CONTENT_KEY, Some(iv), None, ct).unwrap();
        assert_ne!(ct, plain, "ciphertext must differ from plaintext");
    }

    let stream = pcm_stream();
    let options = Mp4MuxerOptions {
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: *b"cenc",
            scheme_version: 0x0001_0000,
            tenc: tenc.clone(),
        }],
        pssh: vec![PsshBox {
            version: 1,
            system_id: [0xEE; 16],
            kids: vec![kid],
            data: vec![0xD0, 0xD1, 0xD2],
        }],
        ..Mp4MuxerOptions::default()
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-mux-roundtrip.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), options)
                .unwrap();
        mux.write_header().unwrap();
        for (i, ct) in ciphertexts.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, ct.clone());
            pkt.pts = Some(i as i64 * 25);
            pkt.duration = Some(25);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    // The protected entry unwraps back to the original codec id, with
    // the protection surface on params.options.
    let params = dmx.streams()[0].params.clone();
    assert_eq!(params.codec_id, CodecId::new("pcm_s16le"));
    assert_eq!(params.options.get("protection_scheme"), Some("cenc"));
    assert_eq!(
        params.options.get("cenc_default_kid"),
        Some("5a".repeat(16).as_str())
    );
    assert_eq!(params.options.get("cenc_default_is_protected"), Some("1"));
    assert_eq!(params.options.get("cenc_default_iv_size"), Some("8"));

    // The moov-level pssh surfaces on metadata().
    let md: std::collections::HashMap<&str, &str> = dmx
        .metadata()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let pssh0 = md.get("pssh_0").copied().expect("pssh_0 metadata key");
    assert_eq!(pssh0, format!("{} 1 3", "ee".repeat(16)));

    // Packets come out as ciphertext; decrypt with the same decision +
    // IVs and recover the plaintext byte-exact.
    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got, ciphertexts, "demux must yield the ciphertext verbatim");
    for ((data, iv), plain) in got.iter_mut().zip(&ivs).zip(&plaintexts) {
        decrypt_sample_in_place(&decision, &CONTENT_KEY, Some(iv), None, data).unwrap();
        assert_eq!(data, plain, "decrypted payload must match the plaintext");
    }
}

/// The fragmented muxer applies the same §8.12 envelope + init-segment
/// pssh emission.
#[test]
fn protected_fragmented_init_segment_surfaces_protection() {
    let kid: [u8; 16] = [0x33; 16];
    let stream = pcm_stream();
    let options = Mp4MuxerOptions {
        fragmented: Some(oxideav_mp4::FragmentedOptions::default()),
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: *b"cenc",
            scheme_version: 0x0001_0000,
            tenc: cenc_tenc(kid),
        }],
        pssh: vec![PsshBox {
            version: 0,
            system_id: [0xAB; 16],
            kids: Vec::new(),
            data: vec![1, 2, 3, 4],
        }],
        ..Mp4MuxerOptions::default()
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-init.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), options)
                .unwrap();
        mux.write_header().unwrap();
        for i in 0..4i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![i as u8; 64]);
            pkt.pts = Some(i * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let params = dmx.streams()[0].params.clone();
    assert_eq!(params.codec_id, CodecId::new("pcm_s16le"));
    assert_eq!(params.options.get("protection_scheme"), Some("cenc"));
    assert_eq!(
        params.options.get("cenc_default_kid"),
        Some("33".repeat(16).as_str())
    );
    let md: std::collections::HashMap<&str, &str> = dmx
        .metadata()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let pssh0 = md.get("pssh_0").copied().expect("pssh_0 metadata key");
    assert_eq!(pssh0, format!("{} 0 4", "ab".repeat(16)));
}

/// A protection directive whose (scheme, tenc) pair is incoherent must
/// fail at open, never at write time.
#[test]
fn incoherent_protection_fails_at_open() {
    let stream = pcm_stream();
    // cbcs pins tenc v1 + pattern + constant IV; a v0 per-sample tenc
    // must be rejected.
    let options = Mp4MuxerOptions {
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: *b"cbcs",
            scheme_version: 0x0001_0000,
            tenc: cenc_tenc([0; 16]),
        }],
        ..Mp4MuxerOptions::default()
    };
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    assert!(
        oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), options).is_err()
    );
}

/// The fragmented muxer's `set_next_segment_pssh` writes a `pssh` box
/// inside the targeted fragment's `moof` (ISO/IEC 23001-7 §8.1.1), which
/// the demuxer surfaces on `moof_pssh_<n>` keyed by the fragment's
/// `mfhd.sequence_number`.
#[test]
fn moof_level_pssh_scopes_to_its_fragment() {
    use oxideav_core::Muxer;
    let stream = pcm_stream();
    let frag_opts = oxideav_mp4::FragmentedOptions {
        cadence: oxideav_mp4::FragmentCadence::EveryNPackets(2),
        emit_random_access_indexes: false,
        ..oxideav_mp4::FragmentedOptions::default()
    };
    let options = Mp4MuxerOptions {
        fragmented: Some(frag_opts.clone()),
        ..Mp4MuxerOptions::default()
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-moof-pssh.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut typed = oxideav_mp4::frag::open_fragmented_typed(
            ws,
            std::slice::from_ref(&stream),
            options,
            frag_opts,
        )
        .unwrap();
        typed.write_header().unwrap();
        // Attach a per-fragment pssh, then push two packets (cadence 2 →
        // one fragment). The box scopes to that fragment's moof.
        typed.set_next_segment_pssh([PsshBox {
            version: 1,
            system_id: [0x9A; 16],
            kids: vec![[0x01; 16], [0x02; 16]],
            data: vec![0xFE, 0xED],
        }]);
        for i in 0..4i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![i as u8; 32]);
            pkt.pts = Some(i * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            typed.write_packet(&pkt).unwrap();
        }
        typed.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md: std::collections::HashMap<&str, &str> = dmx
        .metadata()
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    // Exactly one moof_pssh, on fragment sequence 1 (the first fragment).
    let moof_pssh0 = md.get("moof_pssh_0").copied().expect("moof_pssh_0");
    assert_eq!(
        moof_pssh0,
        &format!("systemid={} seq=1 kids=2 data=2", "9a".repeat(16))[..]
    );
    assert!(
        !md.contains_key("moof_pssh_1"),
        "only the first fragment carries a pssh"
    );
}
