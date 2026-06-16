//! Integration test for the ISO/IEC 14496-12 (post-2015)
//! AmbientViewingEnvironmentBox (`amve`) public parse entry point.
//!
//! `amve` is the file-format carriage of the
//! `ambient_viewing_environment` SEI / ISO/IEC 23091-3 (CICP)
//! ambient-viewing-environment parameters. It is a plain `Box` (NOT a
//! `FullBox` — no version/flags byte) with a fixed 8-byte body:
//!   `unsigned int(32) ambient_illuminance` (0.0001 lux per unit)
//!   `unsigned int(16) ambient_light_x`     (0.00002 CIE 1931 x per unit)
//!   `unsigned int(16) ambient_light_y`     (0.00002 CIE 1931 y per unit)
//!
//! Inside a real file the box lives in a `VisualSampleEntry` (e.g.
//! `avc1` / `hvc1` / `av01`); the in-crate unit tests exercise that
//! `VisualSampleEntry` → track → `params.options` demux path. This file
//! pins the public `oxideav_mp4::demux::parse_amve_box` entry that
//! tooling holding the raw box payload uses, and the byte-exact wire
//! layout of the Rec. ITU-R BT.2035 reference-environment worked example.

use oxideav_mp4::demux::parse_amve_box;

/// The 8-byte `amve` body for the BT.2035 reference environment (10 lux
/// ambient, D65 background chromaticity x = 0.3127, y = 0.3290):
///   ambient_illuminance = 100000 (10 lux ÷ 0.0001)
///   ambient_light_x     = 15635  (0.3127 ÷ 0.00002)
///   ambient_light_y     = 16450  (0.3290 ÷ 0.00002)
/// Wire bytes: `00 01 86 A0  3D 13  40 42`.
const BT2035_BODY: [u8; 8] = [0x00, 0x01, 0x86, 0xA0, 0x3D, 0x13, 0x40, 0x42];

#[test]
fn parse_amve_box_bt2035_wire_example() {
    let rec = parse_amve_box(&BT2035_BODY).expect("amve should parse");
    assert_eq!(rec.ambient_illuminance, 100_000);
    assert_eq!(rec.ambient_light_x, 15_635);
    assert_eq!(rec.ambient_light_y, 16_450);

    // Decoded engineering values per the field units.
    assert!((rec.ambient_illuminance as f64 * 0.0001 - 10.0).abs() < 1e-9);
    assert!((rec.ambient_light_x as f64 * 0.00002 - 0.3127).abs() < 1e-9);
    assert!((rec.ambient_light_y as f64 * 0.00002 - 0.3290).abs() < 1e-9);
}

#[test]
fn parse_amve_box_full_range_fields() {
    // Max field values round-trip without saturation.
    let body = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    let rec = parse_amve_box(&body).expect("amve should parse");
    assert_eq!(rec.ambient_illuminance, u32::MAX);
    assert_eq!(rec.ambient_light_x, u16::MAX);
    assert_eq!(rec.ambient_light_y, u16::MAX);
}

#[test]
fn parse_amve_box_ignores_trailing_bytes() {
    // Bytes beyond the fixed 8-byte body (reserved for a future edition)
    // are ignored, not rejected.
    let mut body = BT2035_BODY.to_vec();
    body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let rec = parse_amve_box(&body).expect("amve should parse");
    assert_eq!(rec.ambient_illuminance, 100_000);
    assert_eq!(rec.ambient_light_x, 15_635);
    assert_eq!(rec.ambient_light_y, 16_450);
}

#[test]
fn parse_amve_box_rejects_short_body() {
    assert!(parse_amve_box(&[0u8; 7]).is_err());
    assert!(parse_amve_box(&[]).is_err());
}
