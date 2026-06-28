//! Integration tests for the HEIF / MIAF item-properties family
//! (ISO/IEC 23008-12 §9.3 / §6.5): the `iprp` ItemPropertiesBox holding an
//! `ipco` ItemPropertyContainerBox (an implicitly 1-indexed list of
//! property boxes) plus one or more `ipma` ItemPropertyAssociation boxes.
//!
//! These exercise the public standalone parsers / builders
//! (`oxideav_mp4::demux::parse_iprp_box` / `build_iprp_box`,
//! `parse_ipco_box` / `build_ipco_box`, `parse_ipma_box` / `build_ipma_box`,
//! `parse_item_property` via `parse_ipco_box`) and confirm parse→build→parse
//! round-trips for every base property box.

use oxideav_mp4::demux::{
    build_ipco_box, build_ipma_box, build_iprp_box, build_item_property, parse_ipco_box,
    parse_ipma_box, parse_iprp_box, ClapRecord, ColrRecord, ItemProperties, ItemProperty,
    ItemPropertyAssociationEntry, PaspRecord, PropertyAssociation,
};

/// Strip the 8-byte `[size:u32][fourcc]` header off a built box, returning
/// the body (what the parsers consume).
fn body_of(boxed: &[u8]) -> &[u8] {
    &boxed[8..]
}

/// Assert a built box's header names `fourcc` and its declared size equals
/// its actual length (no over/under-run).
fn assert_box_header(boxed: &[u8], fourcc: &[u8; 4]) {
    let size = u32::from_be_bytes([boxed[0], boxed[1], boxed[2], boxed[3]]) as usize;
    assert_eq!(size, boxed.len(), "box size mismatch for {fourcc:?}");
    assert_eq!(&boxed[4..8], fourcc, "box fourcc mismatch");
}

#[test]
fn ispe_property_round_trips() {
    let p = ItemProperty::Ispe {
        image_width: 4032,
        image_height: 3024,
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"ispe");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);
}

#[test]
fn pixi_property_round_trips() {
    // 3-channel 8-bit (typical YCbCr 4:2:0 HEVC still).
    let p = ItemProperty::Pixi {
        bits_per_channel: vec![8, 8, 8],
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"pixi");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);

    // 1-channel 10-bit monochrome alpha plane variant.
    let mono = ItemProperty::Pixi {
        bits_per_channel: vec![10],
    };
    let parsed = parse_ipco_box(&build_item_property(&mono));
    assert_eq!(parsed, vec![mono]);
}

#[test]
fn rloc_property_round_trips() {
    let p = ItemProperty::Rloc {
        horizontal_offset: 512,
        vertical_offset: 256,
    };
    let parsed = parse_ipco_box(&build_item_property(&p));
    assert_eq!(parsed, vec![p]);
}

#[test]
fn auxc_property_round_trips_with_subtype() {
    // The canonical alpha-plane auxiliary URN, plus a synthetic subtype tail.
    let p = ItemProperty::AuxC {
        aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".to_string(),
        aux_subtype: vec![0x01, 0x02, 0x03],
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"auxC");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);
}

#[test]
fn auxc_property_round_trips_no_subtype() {
    let p = ItemProperty::AuxC {
        aux_type: "urn:com:example:depth".to_string(),
        aux_subtype: vec![],
    };
    let parsed = parse_ipco_box(&build_item_property(&p));
    assert_eq!(parsed, vec![p]);
}

#[test]
fn irot_all_angles_round_trip() {
    for angle in 0u8..=3 {
        let p = ItemProperty::Irot { angle };
        let boxed = build_item_property(&p);
        assert_box_header(&boxed, b"irot");
        // The irot body is a single byte (6 reserved + 2-bit angle).
        assert_eq!(body_of(&boxed), &[angle]);
        let parsed = parse_ipco_box(&boxed);
        assert_eq!(parsed, vec![p]);
    }
}

#[test]
fn irot_reserved_high_bits_are_masked_on_read() {
    // A producer slip setting the reserved high bits must not corrupt the
    // 2-bit angle — the parser masks to `& 0x03`.
    let boxed = {
        let mut v = 9u32.to_be_bytes().to_vec();
        v.extend_from_slice(b"irot");
        v.push(0xFE); // reserved=0x3F, angle should read as 0x2
        v
    };
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![ItemProperty::Irot { angle: 2 }]);
}

#[test]
fn imir_both_axes_round_trip() {
    for axis in 0u8..=1 {
        let p = ItemProperty::Imir { axis };
        let boxed = build_item_property(&p);
        assert_box_header(&boxed, b"imir");
        assert_eq!(body_of(&boxed), &[axis]);
        let parsed = parse_ipco_box(&boxed);
        assert_eq!(parsed, vec![p]);
    }
}

#[test]
fn lsel_property_round_trips() {
    let p = ItemProperty::Lsel { layer_id: 0xBEEF };
    let parsed = parse_ipco_box(&build_item_property(&p));
    assert_eq!(parsed, vec![p]);
}

#[test]
fn pasp_clap_colr_properties_reuse_14496_records() {
    let pasp = ItemProperty::Pasp(PaspRecord {
        h_spacing: 1,
        v_spacing: 1,
    });
    let clap = ItemProperty::Clap(ClapRecord {
        width_n: 1920,
        width_d: 1,
        height_n: 1080,
        height_d: 1,
        horiz_off_n: 0,
        horiz_off_d: 1,
        vert_off_n: 0,
        vert_off_d: 1,
    });
    let colr = ItemProperty::Colr(ColrRecord::Nclx {
        colour_primaries: 9,
        transfer_characteristics: 16,
        matrix_coefficients: 9,
        full_range: true,
    });
    for p in [pasp, clap, colr] {
        let parsed = parse_ipco_box(&build_item_property(&p));
        assert_eq!(parsed, vec![p]);
    }
}

#[test]
fn unknown_property_preserved_verbatim_keeps_index_slot() {
    // An ipco with [ispe, <unknown 'xxxx'>, irot] — the unknown box must
    // occupy index 2 so the irot stays at index 3.
    let props = vec![
        ItemProperty::Ispe {
            image_width: 100,
            image_height: 100,
        },
        ItemProperty::Other {
            box_type: *b"xxxx",
            body: vec![0xAA, 0xBB],
        },
        ItemProperty::Irot { angle: 1 },
    ];
    let ipco = build_ipco_box(&props);
    assert_box_header(&ipco, b"ipco");
    let parsed = parse_ipco_box(body_of(&ipco));
    assert_eq!(parsed, props);
    assert_eq!(parsed.len(), 3, "the unknown box must keep its index slot");
}

#[test]
fn ipma_narrow_round_trips() {
    // Two items, small indices, 16-bit item IDs → version 0 + 7-bit index.
    let entries = vec![
        ItemPropertyAssociationEntry {
            item_id: 1,
            associations: vec![
                PropertyAssociation {
                    essential: true,
                    property_index: 1,
                },
                PropertyAssociation {
                    essential: false,
                    property_index: 2,
                },
            ],
        },
        ItemPropertyAssociationEntry {
            item_id: 2,
            associations: vec![PropertyAssociation {
                essential: true,
                property_index: 3,
            }],
        },
    ];
    let boxed = build_ipma_box(&entries).expect("narrow ipma builds");
    assert_box_header(&boxed, b"ipma");
    // version 0 expected.
    assert_eq!(body_of(&boxed)[0], 0);
    let parsed = parse_ipma_box(body_of(&boxed));
    assert_eq!(parsed, entries);
}

#[test]
fn ipma_wide_index_round_trips() {
    // A property_index above 0x7F forces flags & 1 (15-bit index field).
    let entries = vec![ItemPropertyAssociationEntry {
        item_id: 5,
        associations: vec![PropertyAssociation {
            essential: true,
            property_index: 200,
        }],
    }];
    let boxed = build_ipma_box(&entries).expect("wide-index ipma builds");
    // flags low byte must carry bit 0 set.
    assert_eq!(body_of(&boxed)[3] & 1, 1, "flags & 1 must be set");
    let parsed = parse_ipma_box(body_of(&boxed));
    assert_eq!(parsed, entries);
}

#[test]
fn ipma_wide_item_id_round_trips() {
    // An item ID above 0xFFFF forces version 1 (32-bit item IDs).
    let entries = vec![ItemPropertyAssociationEntry {
        item_id: 0x0001_0001,
        associations: vec![PropertyAssociation {
            essential: false,
            property_index: 1,
        }],
    }];
    let boxed = build_ipma_box(&entries).expect("wide-id ipma builds");
    assert_eq!(body_of(&boxed)[0], 1, "version 1 for 32-bit item IDs");
    let parsed = parse_ipma_box(body_of(&boxed));
    assert_eq!(parsed, entries);
}

#[test]
fn ipma_sorts_entries_by_item_id() {
    // §9.3.1 requires increasing item_ID order; the builder sorts a copy.
    let entries = vec![
        ItemPropertyAssociationEntry {
            item_id: 9,
            associations: vec![],
        },
        ItemPropertyAssociationEntry {
            item_id: 3,
            associations: vec![],
        },
    ];
    let boxed = build_ipma_box(&entries).expect("builds");
    let parsed = parse_ipma_box(body_of(&boxed));
    assert_eq!(parsed[0].item_id, 3);
    assert_eq!(parsed[1].item_id, 9);
}

#[test]
fn iprp_full_round_trips() {
    // A realistic HEIF still: one image item (id 1) with ispe + pixi + colr,
    // an alpha auxiliary item (id 2) with ispe + pixi + auxC, and item 1
    // also carrying an irot transform.
    let props = vec![
        ItemProperty::Ispe {
            image_width: 4032,
            image_height: 3024,
        },
        ItemProperty::Pixi {
            bits_per_channel: vec![8, 8, 8],
        },
        ItemProperty::Colr(ColrRecord::Nclx {
            colour_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range: true,
        }),
        ItemProperty::Irot { angle: 1 },
        ItemProperty::AuxC {
            aux_type: "urn:mpeg:mpegB:cicp:systems:auxiliary:alpha".to_string(),
            aux_subtype: vec![],
        },
    ];
    let associations = vec![
        ItemPropertyAssociationEntry {
            item_id: 1,
            associations: vec![
                PropertyAssociation {
                    essential: false,
                    property_index: 1, // ispe
                },
                PropertyAssociation {
                    essential: false,
                    property_index: 2, // pixi
                },
                PropertyAssociation {
                    essential: false,
                    property_index: 3, // colr
                },
                PropertyAssociation {
                    essential: true,
                    property_index: 4, // irot (transformative → essential)
                },
            ],
        },
        ItemPropertyAssociationEntry {
            item_id: 2,
            associations: vec![
                PropertyAssociation {
                    essential: false,
                    property_index: 1, // ispe
                },
                PropertyAssociation {
                    essential: true,
                    property_index: 5, // auxC (essential for aux images)
                },
            ],
        },
    ];
    let iprp = ItemProperties {
        properties: props,
        associations,
    };
    let boxed = build_iprp_box(&iprp).expect("iprp builds");
    assert_box_header(&boxed, b"iprp");
    let parsed = parse_iprp_box(body_of(&boxed));
    assert_eq!(parsed, iprp);

    // Resolution: item 1's essential transform is the irot.
    let item1 = parsed.properties_for(1);
    assert_eq!(item1.len(), 4);
    assert_eq!(
        item1[3],
        (true, &ItemProperty::Irot { angle: 1 }),
        "item 1's 4th property is the essential irot"
    );

    // Item 2's auxC is essential.
    let item2 = parsed.properties_for(2);
    assert_eq!(item2.len(), 2);
    assert!(item2[1].0, "auxC association is essential");
    matches!(item2[1].1, ItemProperty::AuxC { .. });
}

#[test]
fn properties_for_skips_zero_and_out_of_range_indices() {
    let iprp = ItemProperties {
        properties: vec![ItemProperty::Irot { angle: 2 }],
        associations: vec![ItemPropertyAssociationEntry {
            item_id: 7,
            associations: vec![
                PropertyAssociation {
                    essential: false,
                    property_index: 0, // "no property" — skipped
                },
                PropertyAssociation {
                    essential: false,
                    property_index: 99, // past end — skipped
                },
                PropertyAssociation {
                    essential: true,
                    property_index: 1, // valid
                },
            ],
        }],
    };
    let resolved = iprp.properties_for(7);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0], (true, &ItemProperty::Irot { angle: 2 }));
}

#[test]
fn empty_iprp_is_empty() {
    let parsed = parse_iprp_box(&[]);
    assert!(parsed.is_empty());
}

#[test]
fn truncated_ipma_entry_ends_walk_cleanly() {
    // entry_count claims 2 but only one full entry is present.
    let mut body = vec![0u8]; // version 0
    body.extend_from_slice(&[0, 0, 0]); // flags 0
    body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2
                                                 // entry 1: item_id 1, 1 assoc.
    body.extend_from_slice(&1u16.to_be_bytes());
    body.push(1);
    body.push(0x81); // essential, index 1
                     // entry 2: item_id present but assoc list truncated.
    body.extend_from_slice(&2u16.to_be_bytes());
    // (no association_count byte) — walk must stop after entry 1.
    let parsed = parse_ipma_box(&body);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].item_id, 1);
}

#[test]
fn udes_property_round_trips() {
    let p = ItemProperty::Udes {
        lang: "en-US".to_string(),
        name: "Sunset".to_string(),
        description: "A photo of the sunset over the bay".to_string(),
        tags: "sunset,bay,golden-hour".to_string(),
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"udes");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);
}

#[test]
fn udes_property_empty_strings_round_trip() {
    // Every string optional / may be empty (§6.5.20.3).
    let p = ItemProperty::Udes {
        lang: String::new(),
        name: String::new(),
        description: String::new(),
        tags: String::new(),
    };
    let parsed = parse_ipco_box(&build_item_property(&p));
    assert_eq!(parsed, vec![p]);
}

#[test]
fn altt_property_round_trips() {
    let p = ItemProperty::Altt {
        alt_text: "A cat sitting on a windowsill".to_string(),
        alt_lang: "en".to_string(),
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"altt");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);
}

#[test]
fn iscl_property_round_trips() {
    // Scale up 3:2 horizontally, 5:4 vertically.
    let p = ItemProperty::Iscl {
        target_width_numerator: 3,
        target_width_denominator: 2,
        target_height_numerator: 5,
        target_height_denominator: 4,
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"iscl");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);
}

#[test]
fn rref_property_round_trips() {
    // A predictively-coded image item requires the `pred` reference type.
    let p = ItemProperty::Rref {
        reference_types: vec![*b"pred", *b"dimg"],
    };
    let boxed = build_item_property(&p);
    assert_box_header(&boxed, b"rref");
    let parsed = parse_ipco_box(&boxed);
    assert_eq!(parsed, vec![p]);

    // Empty list round-trips too.
    let empty = ItemProperty::Rref {
        reference_types: vec![],
    };
    let parsed = parse_ipco_box(&build_item_property(&empty));
    assert_eq!(parsed, vec![empty]);
}

#[test]
fn rref_overrun_count_falls_back_to_other() {
    // A count of 3 with only one FourCC present → preserved as Other so the
    // index slot survives and no read runs past the end.
    let mut body = vec![0u8; 4]; // FullBox(0,0)
    body.push(3); // count
    body.extend_from_slice(b"pred"); // only one of three
    let mut boxed = ((8 + body.len()) as u32).to_be_bytes().to_vec();
    boxed.extend_from_slice(b"rref");
    boxed.extend_from_slice(&body);
    let parsed = parse_ipco_box(&boxed);
    assert!(matches!(parsed[0], ItemProperty::Other { box_type, .. } if &box_type == b"rref"));
}
