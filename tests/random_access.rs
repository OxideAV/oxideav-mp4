//! Integration tests for the `sidx` (SegmentIndexBox §8.16.3) and
//! `mfra/tfra` (Movie Fragment Random Access §8.8.10–11) parsers.
//!
//! Strategy: build synthetic boxes byte-for-byte, parse them via the
//! public `parse_sidx_box` / `parse_mfra_box` entry points, assert the
//! parsed structure matches what we wrote.

use oxideav_mp4::demux::{parse_mfra_box, parse_sidx_box};

// --- Box-builder helpers --------------------------------------------------

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

/// Build a `sidx` v0 body (caller wraps in the box header).
///
/// Layout: 4-byte FullBox(0,0) + 4 reference_id + 4 timescale +
/// 4 ept(u32) + 4 first_offset(u32) + 2 reserved + 2 ref_count + N×12.
fn build_sidx_v0(
    reference_id: u32,
    timescale: u32,
    ept: u32,
    first_offset: u32,
    refs: &[(bool, u32, u32, bool, u8)],
) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // FullBox version=0 flags=0
    body.extend_from_slice(&reference_id.to_be_bytes());
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&ept.to_be_bytes());
    body.extend_from_slice(&first_offset.to_be_bytes());
    body.extend_from_slice(&[0, 0]); // reserved
    body.extend_from_slice(&(refs.len() as u16).to_be_bytes());
    for &(is_sidx, sz, dur, sap, sap_t) in refs {
        let r0 = if is_sidx { 0x8000_0000 } else { 0 } | (sz & 0x7FFF_FFFF);
        body.extend_from_slice(&r0.to_be_bytes());
        body.extend_from_slice(&dur.to_be_bytes());
        let r2 = if sap { 0x8000_0000 } else { 0 } | (((sap_t as u32) & 0x7) << 28);
        body.extend_from_slice(&r2.to_be_bytes());
    }
    body
}

/// Build a `tfra` v0 body. `lengths` is the packed
/// `(traf_len-1) | (trun_len-1) | (sample_len-1)` 6-bit field.
///
/// Each entry: 4 time(u32) + 4 moof_offset(u32) + traf_len bytes traf_n
/// + trun_len bytes trun_n + sample_len bytes sample_n.
fn build_tfra_v0(
    track_id: u32,
    len_traf: u8,
    len_trun: u8,
    len_sample: u8,
    entries: &[(u32, u32, u32, u32, u32)],
) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // FullBox version=0
    body.extend_from_slice(&track_id.to_be_bytes());
    let lengths: u32 =
        (((len_traf - 1) as u32) << 4) | (((len_trun - 1) as u32) << 2) | ((len_sample - 1) as u32);
    body.extend_from_slice(&lengths.to_be_bytes());
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(t, m, traf, trun, samp) in entries {
        body.extend_from_slice(&t.to_be_bytes());
        body.extend_from_slice(&m.to_be_bytes());
        push_var(&mut body, traf, len_traf);
        push_var(&mut body, trun, len_trun);
        push_var(&mut body, samp, len_sample);
    }
    body
}

fn push_var(out: &mut Vec<u8>, value: u32, n: u8) {
    let bytes = value.to_be_bytes();
    out.extend_from_slice(&bytes[(4 - n as usize)..]);
}

fn build_mfra(tfras: &[Vec<u8>]) -> Vec<u8> {
    let mut mfra_body = Vec::new();
    for t in tfras {
        mfra_body.extend_from_slice(&boxed(b"tfra", t));
    }
    // mfro: FullBox + size(u32). Size includes the mfro box itself.
    let mfro_body = {
        let mut b = vec![0u8; 4];
        // mfra outer size = 8 (header) + body + mfro_box(16) — we don't
        // actually have to be exact; the parser doesn't read mfro.
        b.extend_from_slice(&0u32.to_be_bytes());
        b
    };
    mfra_body.extend_from_slice(&boxed(b"mfro", &mfro_body));
    mfra_body
}

// --- sidx tests -----------------------------------------------------------

#[test]
fn sidx_v0_single_reference_round_trips() {
    let body = build_sidx_v0(
        1,                               // reference_id (track_ID)
        48_000,                          // timescale
        0,                               // ept
        16,                              // first_offset (16 bytes after end of sidx)
        &[(false, 4096, 1024, true, 1)], // one media reference
    );
    // The sidx ends at offset 200 (arbitrary anchor for the test).
    let sidx_end = 200u64;
    let r = parse_sidx_box(&body, sidx_end).unwrap().unwrap();
    assert_eq!(r.reference_id, 1);
    assert_eq!(r.timescale, 48_000);
    assert_eq!(r.earliest_presentation_time, 0);
    assert_eq!(r.first_byte_offset, 200 + 16);
    assert_eq!(r.references.len(), 1);
    let rr = r.references[0];
    assert!(!rr.is_sidx);
    assert_eq!(rr.referenced_size, 4096);
    assert_eq!(rr.subsegment_duration, 1024);
    assert!(rr.starts_with_sap);
    assert_eq!(rr.sap_type, 1);
}

#[test]
fn sidx_v0_multi_reference_round_trips() {
    let body = build_sidx_v0(
        2,
        90_000,
        12_345,
        0,
        &[
            (false, 1000, 90, true, 1),
            (false, 2000, 180, true, 1),
            (false, 1500, 90, true, 1),
        ],
    );
    let r = parse_sidx_box(&body, 0).unwrap().unwrap();
    assert_eq!(r.references.len(), 3);
    assert_eq!(r.earliest_presentation_time, 12_345);
    let total_dur: u32 = r.references.iter().map(|x| x.subsegment_duration).sum();
    assert_eq!(total_dur, 360);
}

#[test]
fn sidx_v1_64bit_fields_round_trip() {
    let mut body = vec![0u8; 4];
    body[0] = 1; // version 1
    body.extend_from_slice(&7u32.to_be_bytes()); // reference_id
    body.extend_from_slice(&30u32.to_be_bytes()); // timescale
    body.extend_from_slice(&0xDEAD_BEEF_u64.to_be_bytes()); // ept
    body.extend_from_slice(&100u64.to_be_bytes()); // first_offset
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&1u16.to_be_bytes());
    // ref entry
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&30u32.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());

    let r = parse_sidx_box(&body, 1000).unwrap().unwrap();
    assert_eq!(r.reference_id, 7);
    assert_eq!(r.earliest_presentation_time, 0xDEAD_BEEF);
    assert_eq!(r.first_byte_offset, 1000 + 100);
}

#[test]
fn sidx_truncated_returns_invalid() {
    let body = vec![0u8; 4]; // FullBox only — missing reference_id
    assert!(parse_sidx_box(&body, 0).is_err());
}

#[test]
fn sidx_hierarchical_marks_is_sidx() {
    let body = build_sidx_v0(
        1,
        1000,
        0,
        0,
        &[
            (true, 200, 10, false, 0), // hierarchical reference
            (false, 100, 5, true, 1),  // media reference
        ],
    );
    let r = parse_sidx_box(&body, 0).unwrap().unwrap();
    assert!(r.references[0].is_sidx);
    assert!(!r.references[1].is_sidx);
}

// --- tfra / mfra tests ----------------------------------------------------

#[test]
fn tfra_v0_round_trips() {
    let tfra_body = build_tfra_v0(
        1,
        1,
        1,
        1,
        &[
            (0, 1024, 1, 1, 1),
            (4800, 2048, 1, 1, 1),
            (9600, 3072, 1, 1, 1),
        ],
    );
    let mfra_body = build_mfra(&[tfra_body]);
    let tfras = parse_mfra_box(&mfra_body).unwrap();
    assert_eq!(tfras.len(), 1);
    let t = &tfras[0];
    assert_eq!(t.track_id, 1);
    assert_eq!(t.entries.len(), 3);
    assert_eq!(t.entries[0].time, 0);
    assert_eq!(t.entries[0].moof_offset, 1024);
    assert_eq!(t.entries[1].time, 4800);
    assert_eq!(t.entries[2].moof_offset, 3072);
}

#[test]
fn tfra_v1_64bit_fields_round_trip() {
    let mut body = vec![0u8; 4];
    body[0] = 1; // version 1
    body.extend_from_slice(&5u32.to_be_bytes()); // track_id
                                                 // lengths: traf=2, trun=3, sample=4 → encoded (1,2,3) → (1<<4)|(2<<2)|3 = 0x1B
    body.extend_from_slice(&0x0000_001Bu32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
                                                 // entry: 8-byte time, 8-byte moof, 2 traf, 3 trun, 4 sample
    body.extend_from_slice(&0x1234_5678_9ABC_DEF0u64.to_be_bytes());
    body.extend_from_slice(&0xCAFE_BABE_DEAD_BEEFu64.to_be_bytes());
    body.extend_from_slice(&[0x12, 0x34]); // traf_n = 0x1234
    body.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // trun_n
    body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]); // sample_n

    let mfra_body = boxed(b"tfra", &body);
    let tfras = parse_mfra_box(&mfra_body).unwrap();
    assert_eq!(tfras.len(), 1);
    let e = tfras[0].entries[0];
    assert_eq!(e.time, 0x1234_5678_9ABC_DEF0);
    assert_eq!(e.moof_offset, 0xCAFE_BABE_DEAD_BEEF);
    assert_eq!(e.traf_number, 0x1234);
    assert_eq!(e.trun_number, 0x00AA_BBCC);
    assert_eq!(e.sample_number, 0x0102_0304);
}

#[test]
fn mfra_with_two_tracks_returns_two_tfras() {
    let t1 = build_tfra_v0(1, 1, 1, 1, &[(0, 100, 1, 1, 1)]);
    let t2 = build_tfra_v0(2, 1, 1, 1, &[(0, 200, 1, 1, 1)]);
    let mfra_body = build_mfra(&[t1, t2]);
    let tfras = parse_mfra_box(&mfra_body).unwrap();
    assert_eq!(tfras.len(), 2);
    assert_eq!(tfras[0].track_id, 1);
    assert_eq!(tfras[1].track_id, 2);
    assert_eq!(tfras[0].entries[0].moof_offset, 100);
    assert_eq!(tfras[1].entries[0].moof_offset, 200);
}

#[test]
fn empty_mfra_returns_no_tfras() {
    let mfra_body = build_mfra(&[]);
    let tfras = parse_mfra_box(&mfra_body).unwrap();
    assert!(tfras.is_empty());
}

#[test]
fn tfra_truncated_returns_invalid() {
    let body = vec![0u8; 8]; // FullBox + track_id only — missing lengths
    let mfra_body = boxed(b"tfra", &body);
    // Parser is lenient on the outer mfra walk but parse_tfra should
    // surface the underlying truncation.
    assert!(parse_mfra_box(&mfra_body).is_err());
}

// --- Integration: mfra in a synthetic fragmented file --------------------

/// Build a small fragmented MP4 + tfra and exercise `seek_to`. The
/// tfra fast-path lands on the keyframe whose tfra entry covers the
/// requested pts; we verify the resulting next_packet() returns that
/// keyframe.
#[test]
fn tfra_drives_seek_to_correct_keyframe() {
    use oxideav_core::{
        CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
    };
    use oxideav_mp4::muxer::open_with_options;
    use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};
    use std::io::Cursor;

    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Mp4,
        faststart: false,
        fragmented: Some(FragmentedOptions {
            cadence: FragmentCadence::EveryNPackets(1),
            styp: None,
            // These tests synthesise their own mfra; suppress native
            // sidx + mfra emission so the appended boxes are the only ones.
            emit_random_access_indexes: false,
        }),
    };
    let path = std::env::temp_dir().join("oxideav-mp4-tfra-seek.mp4");

    // Mux 5 fragments and remember the moof byte offsets.
    let mut frag_byte_offsets: Vec<u64> = Vec::new();
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..5i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![i as u8; 32]);
            pkt.pts = Some(i * 1024);
            pkt.dts = Some(i * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Walk the file to discover moof byte offsets.
    let bytes = std::fs::read(&path).unwrap();
    let mut i = 0usize;
    while i + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let fc = &bytes[i + 4..i + 8];
        if fc == b"moof" {
            frag_byte_offsets.push(i as u64);
        }
        if sz == 0 {
            break;
        }
        i += sz;
    }
    assert_eq!(frag_byte_offsets.len(), 5);

    // Append an mfra describing fragments 0..4 with their real byte offsets.
    let entries: Vec<(u32, u32, u32, u32, u32)> = frag_byte_offsets
        .iter()
        .enumerate()
        .map(|(i, &off)| ((i as u32) * 1024, off as u32, 1, 1, 1))
        .collect();
    let tfra_body = build_tfra_v0(1, 1, 1, 1, &entries);
    let mfra_body = build_mfra(&[tfra_body]);
    let mut new_bytes = bytes.clone();
    new_bytes.extend_from_slice(&boxed(b"mfra", &mfra_body));
    std::fs::write(&path, &new_bytes).unwrap();

    // Seek to pts=2500 (mid-fragment-3 — pts=2048 keyframe should be
    // chosen, since 3072 > 2500 and 2048 ≤ 2500).
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(new_bytes));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let landed_pts = dmx.seek_to(0, 2500).unwrap();
    assert_eq!(landed_pts, 2048, "tfra should pick the pts=2048 keyframe");
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.pts, Some(2048));
    assert_eq!(pkt.data, vec![2u8; 32]);
}

/// End-to-end smoke test: build a tiny fragmented MP4 with an mfra at
/// the end, ensure `demux::open` parses it without errors and the file
/// still demuxes cleanly. (We don't surface the parsed table through the
/// `Box<dyn Demuxer>` trait; this just confirms the parser runs and
/// doesn't reject the file.)
#[test]
fn mfra_in_fragmented_file_does_not_break_demux() {
    use std::io::Cursor;

    // Reuse the fragmented muxer to produce a real file, then append a
    // synthetic mfra. The muxer doesn't yet emit mfra (deferred to a
    // follow-up #408 muxer-side change); we tack one on by hand here.
    use oxideav_core::{
        CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
    };
    use oxideav_mp4::muxer::open_with_options;
    use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Mp4,
        faststart: false,
        fragmented: Some(FragmentedOptions {
            cadence: FragmentCadence::EveryNPackets(1),
            styp: None,
            // These tests synthesise their own mfra; suppress native
            // sidx + mfra emission so the appended boxes are the only ones.
            emit_random_access_indexes: false,
        }),
    };
    let path = std::env::temp_dir().join("oxideav-mp4-mfra-smoke.mp4");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..3i64 {
            let mut pkt = Packet::new(0, stream.time_base, vec![0u8; 16]);
            pkt.pts = Some(i * 1024);
            pkt.dts = Some(i * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Append an mfra with a tfra describing 3 fragments.
    let mut bytes = std::fs::read(&path).unwrap();
    let tfra_body = build_tfra_v0(
        1,
        1,
        1,
        1,
        &[
            (0, 100, 1, 1, 1),
            (1024, 200, 1, 1, 1),
            (2048, 300, 1, 1, 1),
        ],
    );
    let mfra_body = build_mfra(&[tfra_body]);
    bytes.extend_from_slice(&boxed(b"mfra", &mfra_body));
    std::fs::write(&path, &bytes).unwrap();

    // Re-open: the demuxer must not choke on the appended mfra.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut count = 0;
    while dmx.next_packet().is_ok() {
        count += 1;
    }
    assert_eq!(count, 3, "still demuxes 3 packets with mfra appended");
}
