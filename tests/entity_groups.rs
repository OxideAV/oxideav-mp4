//! Integration tests for the HEIF / MIAF entity-grouping family (ISO/IEC
//! 23008-12 §9.4): the `grpl` GroupsListBox holding zero or more
//! `EntityToGroupBox`es (each a FullBox whose FourCC is the
//! `grouping_type` — `altr`, `ster`, …) mapping item / track IDs into a
//! named group.
//!
//! Exercises the public standalone parsers / builders
//! (`oxideav_mp4::demux::parse_grpl_box` / `build_grpl_box`,
//! `build_entity_to_group_box`) with parse→build→parse round-trips.

use oxideav_mp4::demux::{
    build_entity_to_group_box, build_grpl_box, parse_grpl_box, EntityGroups, EntityToGroup,
};

fn body_of(boxed: &[u8]) -> &[u8] {
    &boxed[8..]
}

fn assert_box_header(boxed: &[u8], fourcc: &[u8; 4]) {
    let size = u32::from_be_bytes([boxed[0], boxed[1], boxed[2], boxed[3]]) as usize;
    assert_eq!(size, boxed.len(), "box size mismatch for {fourcc:?}");
    assert_eq!(&boxed[4..8], fourcc);
}

#[test]
fn altr_group_round_trips() {
    let g = EntityToGroup {
        grouping_type: *b"altr",
        group_id: 100,
        entity_ids: vec![1, 2, 3],
    };
    let boxed = build_entity_to_group_box(&g);
    assert_box_header(&boxed, b"altr");
    // FullBox(0,0) + group_id + num(3) + 3 ids = 4 + 4 + 4 + 12 = 24 body.
    assert_eq!(body_of(&boxed).len(), 24);

    let grpl = EntityGroups { groups: vec![g] };
    let boxed = build_grpl_box(&grpl);
    assert_box_header(&boxed, b"grpl");
    let parsed = parse_grpl_box(body_of(&boxed));
    assert_eq!(parsed, grpl);
}

#[test]
fn ster_stereo_pair_round_trips() {
    // §9.4.3: `ster` carries exactly two entity IDs, entity 0 = left view.
    let g = EntityToGroup {
        grouping_type: *b"ster",
        group_id: 7,
        entity_ids: vec![10, 11],
    };
    let grpl = EntityGroups { groups: vec![g] };
    let boxed = build_grpl_box(&grpl);
    let parsed = parse_grpl_box(body_of(&boxed));
    assert_eq!(parsed, grpl);
    let stereo = parsed.by_type(*b"ster").next().unwrap();
    assert_eq!(stereo.entity_ids, vec![10, 11], "left=10, right=11");
}

#[test]
fn multiple_groups_round_trip_and_index_helpers() {
    let grpl = EntityGroups {
        groups: vec![
            EntityToGroup {
                grouping_type: *b"altr",
                group_id: 1,
                entity_ids: vec![2, 3],
            },
            EntityToGroup {
                grouping_type: *b"altr",
                group_id: 2,
                entity_ids: vec![4, 5],
            },
            EntityToGroup {
                grouping_type: *b"ster",
                group_id: 3,
                entity_ids: vec![6, 7],
            },
        ],
    };
    let boxed = build_grpl_box(&grpl);
    let parsed = parse_grpl_box(body_of(&boxed));
    assert_eq!(parsed, grpl);

    // by_type filters; by_id locates.
    assert_eq!(parsed.by_type(*b"altr").count(), 2);
    assert_eq!(parsed.by_type(*b"ster").count(), 1);
    assert_eq!(parsed.by_id(2).unwrap().entity_ids, vec![4, 5]);
    assert!(parsed.by_id(99).is_none());
}

#[test]
fn empty_group_no_entities_round_trips() {
    let g = EntityToGroup {
        grouping_type: *b"altr",
        group_id: 5,
        entity_ids: vec![],
    };
    let grpl = EntityGroups { groups: vec![g] };
    let parsed = parse_grpl_box(body_of(&build_grpl_box(&grpl)));
    assert_eq!(parsed, grpl);
}

#[test]
fn empty_grpl_is_empty() {
    let parsed = parse_grpl_box(&[]);
    assert!(parsed.is_empty());
}

#[test]
fn malformed_entity_overrun_is_dropped_not_panicking() {
    // An EntityToGroupBox claiming 99 entities but only carrying one is
    // dropped (contributes no group); a following well-formed box still
    // parses.
    fn box_bytes(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut v = total.to_be_bytes().to_vec();
        v.extend_from_slice(fourcc);
        v.extend_from_slice(body);
        v
    }
    // Bad box: FullBox(0,0) + group_id 1 + num=99 + one id.
    let mut bad = vec![0u8; 4];
    bad.extend_from_slice(&1u32.to_be_bytes());
    bad.extend_from_slice(&99u32.to_be_bytes());
    bad.extend_from_slice(&42u32.to_be_bytes());
    let bad = box_bytes(b"altr", &bad);

    // Good box after it.
    let good = build_entity_to_group_box(&EntityToGroup {
        grouping_type: *b"ster",
        group_id: 2,
        entity_ids: vec![3, 4],
    });

    let mut grpl_body = bad;
    grpl_body.extend_from_slice(&good);
    let parsed = parse_grpl_box(&grpl_body);
    assert_eq!(parsed.groups.len(), 1, "bad box dropped, good box kept");
    assert_eq!(parsed.groups[0].grouping_type, *b"ster");
}
