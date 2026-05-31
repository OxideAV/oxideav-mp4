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
