//! Integration test for the ISO/IEC 14496-12 §8.11 file-level `meta`
//! box item infrastructure (HEIF / MIAF item catalogue): `pitm`
//! (PrimaryItemBox §8.11.4), `iloc` (ItemLocationBox §8.11.3), `iinf` /
//! `infe` (ItemInfoBox §8.11.6), and `iref` (ItemReferenceBox §8.11.12).
//!
//! The demuxer's top-level walk parses a file-level `meta` into the
//! structured `MetaItems` record (reachable internally via
//! `Mp4Demuxer::meta_items()`) and surfaces a flat summary on the
//! `metadata()` channel as `meta_*` keys. `Mp4Demuxer` itself is not
//! public, so this test exercises the flat surface — the same shape the
//! `pdin` integration test uses.
//!
//! The fixture is built by muxing a tiny PCM stream with the regular
//! muxer (to get a valid `ftyp` + `moov` + `mdat`) and then splicing a
//! synthetic HEIF-style `meta` box in after the `ftyp`. The demuxer
//! still finds a valid track from the `moov`, so `open` succeeds and the
//! `meta_*` keys appear alongside the normal stream.

use std::io::Cursor;
use std::sync::atomic::{AtomicU32, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

static NEXT_TMP: AtomicU32 = AtomicU32::new(0);

/// Wrap `body` in a `[size:u32][fourcc]` 32-bit box header.
fn box_bytes(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut v = total.to_be_bytes().to_vec();
    v.extend_from_slice(fourcc);
    v.extend_from_slice(body);
    v
}

/// Build a HEIF-style `meta` box: hdlr(pict) + pitm + iloc + iinf(infe) +
/// iref(dimg). The `meta` body opens with the FullBox version/flags word.
fn build_heif_meta() -> Vec<u8> {
    // hdlr — handler_type "pict".
    let mut hdlr = vec![0u8, 0, 0, 0];
    hdlr.extend_from_slice(&[0, 0, 0, 0]); // pre_defined
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0u8; 12]); // reserved[3]
    hdlr.push(0); // empty name
    let hdlr = box_bytes(b"hdlr", &hdlr);

    // pitm v0 → primary item_ID = 1.
    let mut pitm = vec![0u8, 0, 0, 0];
    pitm.extend_from_slice(&1u16.to_be_bytes());
    let pitm = box_bytes(b"pitm", &pitm);

    // iloc v0: offset_size=4, length_size=4, base_offset_size=0; one
    // item (ID 1) with one extent at offset 0x800 length 0x100, plus a
    // second item (ID 2, a thumbnail) at offset 0x900 length 0x40.
    let mut iloc = vec![0u8, 0, 0, 0];
    iloc.push(0x44); // offset_size=4, length_size=4
    iloc.push(0x00); // base_offset_size=0
    iloc.extend_from_slice(&2u16.to_be_bytes()); // item_count
    for (id, off, len) in [(1u16, 0x800u32, 0x100u32), (2, 0x900, 0x40)] {
        iloc.extend_from_slice(&id.to_be_bytes());
        iloc.extend_from_slice(&0u16.to_be_bytes()); // dref
        iloc.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        iloc.extend_from_slice(&off.to_be_bytes());
        iloc.extend_from_slice(&len.to_be_bytes());
    }
    let iloc = box_bytes(b"iloc", &iloc);

    // iinf v0 with two infe v2 entries.
    let mut infe1 = vec![2u8, 0, 0, 0];
    infe1.extend_from_slice(&1u16.to_be_bytes()); // item_ID
    infe1.extend_from_slice(&0u16.to_be_bytes()); // protection_index
    infe1.extend_from_slice(b"hvc1"); // item_type
    infe1.extend_from_slice(b"primary\0");
    let infe1 = box_bytes(b"infe", &infe1);
    let mut infe2 = vec![2u8, 0, 0, 0];
    infe2.extend_from_slice(&2u16.to_be_bytes());
    infe2.extend_from_slice(&0u16.to_be_bytes());
    infe2.extend_from_slice(b"hvc1");
    infe2.extend_from_slice(b"thumb\0");
    let infe2 = box_bytes(b"infe", &infe2);
    let mut iinf = vec![0u8, 0, 0, 0];
    iinf.extend_from_slice(&2u16.to_be_bytes()); // entry_count
    iinf.extend_from_slice(&infe1);
    iinf.extend_from_slice(&infe2);
    let iinf = box_bytes(b"iinf", &iinf);

    // iref v0: a `thmb` reference from item 2 (thumbnail) to item 1.
    let mut thmb = 2u16.to_be_bytes().to_vec();
    thmb.extend_from_slice(&1u16.to_be_bytes()); // reference_count
    thmb.extend_from_slice(&1u16.to_be_bytes()); // to_item_ID
    let thmb = box_bytes(b"thmb", &thmb);
    let mut iref = vec![0u8, 0, 0, 0];
    iref.extend_from_slice(&thmb);
    let iref = box_bytes(b"iref", &iref);

    let mut meta = vec![0u8, 0, 0, 0]; // FullBox version/flags
    meta.extend_from_slice(&hdlr);
    meta.extend_from_slice(&pitm);
    meta.extend_from_slice(&iloc);
    meta.extend_from_slice(&iinf);
    meta.extend_from_slice(&iref);
    box_bytes(b"meta", &meta)
}

/// Splice a top-level box in after the `ftyp`.
fn splice_after_ftyp(file: &[u8], extra: &[u8]) -> Vec<u8> {
    let ftyp_size = u32::from_be_bytes([file[0], file[1], file[2], file[3]]) as usize;
    assert!(ftyp_size <= file.len());
    let mut out = Vec::with_capacity(file.len() + extra.len());
    out.extend_from_slice(&file[..ftyp_size]);
    out.extend_from_slice(extra);
    out.extend_from_slice(&file[ftyp_size..]);
    out
}

fn pcm_stream_info() -> StreamInfo {
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

fn make_pcm_payload(samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        out.extend_from_slice(&(i as i16).wrapping_mul(3).to_le_bytes());
        out.extend_from_slice(&(i as i16).wrapping_mul(5).to_le_bytes());
    }
    out
}

fn mux_pcm_to_bytes() -> Vec<u8> {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-meta-items-r369-{}-{}.mp4",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed),
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..2 {
            let mut pkt = Packet::new(
                0,
                stream.time_base,
                make_pcm_payload(frames_per_packet as usize),
            );
            pkt.pts = Some(i * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn md_get<'a>(md: &'a [(String, String)], key: &str) -> Option<&'a String> {
    md.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

#[test]
fn heif_meta_surfaces_item_catalogue() {
    let spliced = splice_after_ftyp(&mux_pcm_to_bytes(), &build_heif_meta());
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();

    assert_eq!(md_get(md, "meta_handler"), Some(&"pict".to_string()));
    assert_eq!(md_get(md, "meta_primary_item"), Some(&"1".to_string()));
    assert_eq!(md_get(md, "meta_item_count"), Some(&"2".to_string()));
    assert_eq!(md_get(md, "meta_iloc_count"), Some(&"2".to_string()));
    assert_eq!(md_get(md, "meta_iref_count"), Some(&"1".to_string()));
    assert_eq!(
        md_get(md, "meta_item_0"),
        Some(&"id=1 type=hvc1 name=primary".to_string())
    );
    assert_eq!(
        md_get(md, "meta_item_1"),
        Some(&"id=2 type=hvc1 name=thumb".to_string())
    );

    // The spliced HEIF meta must not disturb the muxer's real audio
    // track — the demuxer still finds one PCM stream.
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("pcm_s16le"));
}

/// Build a top-level `meco` AdditionalMetadataContainerBox (§8.11.7)
/// holding two additional `meta` boxes (distinct handler types) plus one
/// `mere` relation (§8.11.8) between them.
fn build_meco() -> Vec<u8> {
    fn meta_with_handler(h: &[u8; 4]) -> Vec<u8> {
        let mut hdlr = vec![0u8, 0, 0, 0];
        hdlr.extend_from_slice(&[0, 0, 0, 0]);
        hdlr.extend_from_slice(h);
        hdlr.extend_from_slice(&[0u8; 12]);
        hdlr.push(0);
        let hdlr = box_bytes(b"hdlr", &hdlr);
        let mut meta = vec![0u8, 0, 0, 0];
        meta.extend_from_slice(&hdlr);
        box_bytes(b"meta", &meta)
    }
    // mere: FullBox preamble + two handler types + relation byte.
    let mut mere = vec![0u8, 0, 0, 0];
    mere.extend_from_slice(b"mp7t");
    mere.extend_from_slice(b"mdir");
    mere.push(3); // complementary
    let mere = box_bytes(b"mere", &mere);

    let mut meco = Vec::new();
    meco.extend_from_slice(&meta_with_handler(b"mp7t"));
    meco.extend_from_slice(&meta_with_handler(b"mdir"));
    meco.extend_from_slice(&mere);
    box_bytes(b"meco", &meco)
}

#[test]
fn meco_surfaces_additional_metas_and_relations() {
    let spliced = splice_after_ftyp(&mux_pcm_to_bytes(), &build_meco());
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(spliced));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let md = dmx.metadata();

    assert_eq!(md_get(md, "meco_meta_count"), Some(&"2".to_string()));
    assert_eq!(md_get(md, "meco_meta_0"), Some(&"mp7t".to_string()));
    assert_eq!(md_get(md, "meco_meta_1"), Some(&"mdir".to_string()));
    assert_eq!(md_get(md, "meco_relation_count"), Some(&"1".to_string()));
    assert_eq!(
        md_get(md, "meco_relation_0"),
        Some(&"mp7t-mdir=3".to_string())
    );

    // The real audio track is undisturbed.
    assert_eq!(dmx.streams().len(), 1);
}

#[test]
fn non_heif_file_emits_no_meta_keys() {
    // Vanilla muxer output carries no file-level HEIF `meta` box, so no
    // `meta_*` keys should appear (absence by omission).
    let bytes = mux_pcm_to_bytes();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    for (k, _) in dmx.metadata() {
        assert!(
            !k.starts_with("meta_"),
            "unexpected meta_* key {k} in non-HEIF file",
        );
    }
}
