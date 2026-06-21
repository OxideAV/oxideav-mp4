//! Integration tests for the ISO/IEC 14496-12 §8.15.4.2 Stereo Video
//! Box (`stvi`) public read + write entry points.
//!
//! `stvi` lives inside the `schi` (SchemeInformationBox) of a sample
//! entry's `sinf` when the restricted-scheme SchemeType is `stvi`
//! (stereoscopic video, §8.15.4.1). It indicates that decoded frames
//! carry either two spatially packed constituent frames forming a stereo
//! pair (frame packing) or one of two views of a stereo pair. The
//! in-crate unit tests exercise the `VisualSampleEntry → sinf → schi →
//! track → params.options` demux path; this file pins the public
//! `oxideav_mp4::demux::parse_stvi_box` / `build_stvi_box` entry points
//! that a restricted-scheme muxer / inspector uses, and the byte-exact
//! `FullBox(0,0)` §8.15.4.2.2 wire layout.
//!
//! Spec layout reference: `docs/container/isobmff/bmff.txt` §8.15.4.2.

use oxideav_mp4::demux::{build_stvi_box, parse_stvi_box, StviRecord};

/// Strip the 8-byte box header (`size` u32 + `type` u32) from a complete
/// box, returning the preamble-relative body that `parse_stvi_box`
/// consumes.
fn box_body(complete: &[u8]) -> &[u8] {
    assert!(complete.len() >= 8);
    assert_eq!(&complete[4..8], b"stvi");
    let size = u32::from_be_bytes([complete[0], complete[1], complete[2], complete[3]]) as usize;
    assert_eq!(size, complete.len(), "box size field must match length");
    &complete[8..]
}

/// `stereo_scheme == 1` (ISO/IEC 14496-10 frame-packing-arrangement SEI
/// scheme): a 4-byte `stereo_indication_type` carrying the u32
/// arrangement type. Here `3` = side-by-side per Table D-8.
#[test]
fn build_then_parse_scheme1_frame_packing() {
    let rec = StviRecord {
        single_view_allowed: 3, // both views OK on a monoscopic display
        stereo_scheme: 1,
        stereo_indication_type: 3u32.to_be_bytes().to_vec(),
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    // 8 header + 4 FullBox preamble + 4 word + 4 scheme + 4 length + 4 array.
    assert_eq!(complete.len(), 8 + 4 + 4 + 4 + 4 + 4);
    let back = parse_stvi_box(box_body(&complete)).expect("parse should succeed");
    assert_eq!(back, rec);
    assert!(back.right_view_monoscopic_allowed());
    assert!(back.left_view_monoscopic_allowed());
}

/// `stereo_scheme == 3` (ISO/IEC 23000-11): a 2-byte
/// `stereo_indication_type` (composition type + `is_left_first` LSB).
#[test]
fn build_then_parse_scheme3_two_bytes() {
    let rec = StviRecord {
        single_view_allowed: 1, // only the right view on a monoscopic display
        stereo_scheme: 3,
        stereo_indication_type: vec![0x02, 0x01], // composition 2, is_left_first = 1
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    let back = parse_stvi_box(box_body(&complete)).expect("parse should succeed");
    assert_eq!(back, rec);
    assert!(back.right_view_monoscopic_allowed());
    assert!(!back.left_view_monoscopic_allowed());
}

/// An empty `stereo_indication_type` (length 0) round-trips with a
/// `length` field of 0 and no array bytes.
#[test]
fn build_then_parse_empty_indication() {
    let rec = StviRecord {
        single_view_allowed: 0, // stereoscopic display only
        stereo_scheme: 1,
        stereo_indication_type: Vec::new(),
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    assert_eq!(complete.len(), 8 + 4 + 4 + 4 + 4);
    let back = parse_stvi_box(box_body(&complete)).expect("parse should succeed");
    assert_eq!(back, rec);
    assert!(!back.right_view_monoscopic_allowed());
    assert!(!back.left_view_monoscopic_allowed());
}

/// The exact wire bytes of a built `stvi` box are pinned: header +
/// FullBox(0,0) preamble + `single_view_allowed` word + scheme + length
/// + indication, all big-endian.
#[test]
fn build_wire_bytes_are_exact() {
    let rec = StviRecord {
        single_view_allowed: 2,
        stereo_scheme: 1,
        stereo_indication_type: 4u32.to_be_bytes().to_vec(),
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    let expected: &[u8] = &[
        0x00, 0x00, 0x00, 0x1C, // size = 28
        b's', b't', b'v', b'i', // type
        0x00, 0x00, 0x00, 0x00, // FullBox version 0, flags 0
        0x00, 0x00, 0x00, 0x02, // reserved(30)=0 | single_view_allowed(2)=2
        0x00, 0x00, 0x00, 0x01, // stereo_scheme = 1
        0x00, 0x00, 0x00, 0x04, // length = 4
        0x00, 0x00, 0x00, 0x04, // stereo_indication_type = 4
    ];
    assert_eq!(complete, expected);
}

/// `parse_stvi_box` rejects a body too short to hold the fixed header.
#[test]
fn parse_rejects_short_body() {
    assert!(parse_stvi_box(&[0u8; 15]).is_err());
    assert!(parse_stvi_box(&[]).is_err());
}

/// `parse_stvi_box` rejects a `length` field that overruns the body
/// (rather than reading trailing `any_box` content or past the end).
#[test]
fn parse_rejects_overrunning_length() {
    // FullBox preamble + word + scheme + length=8, but no indication bytes.
    let body = [
        0x00, 0x00, 0x00, 0x00, // FullBox
        0x00, 0x00, 0x00, 0x00, // single_view_allowed word
        0x00, 0x00, 0x00, 0x01, // stereo_scheme
        0x00, 0x00, 0x00, 0x08, // length = 8 (overruns)
    ];
    assert!(parse_stvi_box(&body).is_err());
}

/// `parse_stvi_box` ignores trailing optional `any_box` bytes after the
/// `stereo_indication_type` array — a real `stvi` may carry nested boxes.
#[test]
fn parse_ignores_trailing_any_box() {
    let rec = StviRecord {
        single_view_allowed: 1,
        stereo_scheme: 1,
        stereo_indication_type: 1u32.to_be_bytes().to_vec(),
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    // Append a trailing `free` box to the body — parse_stvi_box reads the
    // body slice it is handed and stops at the indication array.
    let mut body = box_body(&complete).to_vec();
    body.extend_from_slice(&8u32.to_be_bytes());
    body.extend_from_slice(b"free");
    let back = parse_stvi_box(&body).expect("parse should succeed");
    assert_eq!(back, rec);
    // The original (no-trailing) box still parses identically.
    let orig = parse_stvi_box(box_body(&complete)).expect("parse should succeed");
    assert_eq!(orig, rec);
}

/// The reserved 30 high bits of the `single_view_allowed` word are
/// emitted as zero by the builder and masked off on parse.
#[test]
fn build_masks_single_view_allowed_to_two_bits() {
    // Even if a caller passes a u8 with high bits set (only the low 2
    // bits are meaningful), the builder writes only the low 2 bits.
    let rec = StviRecord {
        single_view_allowed: 0xFE, // low 2 bits = 0b10 → 2
        stereo_scheme: 2,
        stereo_indication_type: Vec::new(),
    };
    let complete = build_stvi_box(&rec).expect("build should succeed");
    let back = parse_stvi_box(box_body(&complete)).expect("parse should succeed");
    assert_eq!(back.single_view_allowed, 2);
    assert!(!back.right_view_monoscopic_allowed());
    assert!(back.left_view_monoscopic_allowed());
}
