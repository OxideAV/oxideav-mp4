//! Integration test for the public write side of `csgp`
//! (CompactSampleToGroupBox, ISO/IEC 14496-12:2020 §8.9.5).
//!
//! Drives `sample_groups::build_csgp` and the canonical reader
//! `demux::parse_csgp_box` across the crate's public API boundary, the
//! same way an external segment-index emitter would: build the compact
//! box, strip the 8-byte ISO BMFF header, and confirm the reader
//! reconstructs every field — across the four §8.9.5 bit-field widths,
//! the optional `grouping_type_parameter`, multiple patterns, and the
//! fragment-local high bit of a `sample_group_description_index`.

use oxideav_mp4::demux::parse_csgp_box;
use oxideav_mp4::sample_groups::{build_csgp, CompactSampleToGroup, CompactSampleToGroupPattern};

#[test]
fn build_csgp_box_header_is_well_formed() {
    let c = CompactSampleToGroup {
        grouping_type: *b"roll",
        grouping_type_parameter: None,
        patterns: vec![CompactSampleToGroupPattern {
            sample_count: 4,
            indices: vec![1, 0, 2],
        }],
    };
    let b = build_csgp(&c);
    // 8-byte box header: size(u32) + type 'csgp'.
    let declared = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    assert_eq!(declared, b.len());
    assert_eq!(&b[4..8], b"csgp");
    // FullBox version is pinned to 0 by §8.9.5.
    assert_eq!(b[8], 0);
}

#[test]
fn build_then_parse_recovers_every_field() {
    let cases = vec![
        // All values ≤ 0xF → 4-bit widths everywhere.
        CompactSampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 3,
                indices: vec![1, 2, 0],
            }],
        },
        // Mixed widths + grouping_type_parameter + several patterns.
        CompactSampleToGroup {
            grouping_type: *b"rap ",
            grouping_type_parameter: Some(0xDEAD_BEEF),
            patterns: vec![
                CompactSampleToGroupPattern {
                    sample_count: 0x1234,
                    indices: vec![0x10, 0, 0xFF, 0x7F],
                },
                CompactSampleToGroupPattern {
                    sample_count: 1,
                    indices: vec![0x1_0000],
                },
            ],
        },
        // Fragment-local high bit set (32-bit-wide index).
        CompactSampleToGroup {
            grouping_type: *b"sync",
            grouping_type_parameter: None,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 9,
                indices: vec![0x8000_0001, 0x8000_0002],
            }],
        },
        // Zero patterns is a legal compact box.
        CompactSampleToGroup {
            grouping_type: *b"prol",
            grouping_type_parameter: None,
            patterns: vec![],
        },
    ];

    for c in &cases {
        let bytes = build_csgp(c);
        let parsed = parse_csgp_box(&bytes[8..]).expect("csgp must re-parse");
        assert_eq!(parsed.grouping_type, c.grouping_type);
        assert_eq!(parsed.grouping_type_parameter, c.grouping_type_parameter);
        assert_eq!(parsed.patterns.len(), c.patterns.len());
        for (got, want) in parsed.patterns.iter().zip(&c.patterns) {
            assert_eq!(got.sample_count, want.sample_count);
            assert_eq!(got.indices, want.indices);
        }
    }
}
