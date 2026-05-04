//! Integration tests for fragmented-MP4 (DASH / HLS / Smooth-Streaming /
//! CMAF) demux.
//!
//! Strategy: build a synthetic fragmented MP4 byte-by-byte (so the test
//! has no `ffmpeg` dependency at run time) with a known sample layout,
//! then run our demuxer against it and assert the per-sample table
//! (offset, size, dts, pts, duration, keyframe) matches what we wrote.
//!
//! ISO/IEC 14496-12 §8.8 — Movie Fragments. The synthetic file has the
//! structure:
//!
//! ```text
//! ftyp
//! moov
//!   mvhd  (movie timescale = 48000)
//!   trak  (track_ID = 1, codec = sowt PCM s16le)
//!     tkhd
//!     mdia
//!       mdhd  (track timescale = 48000)
//!       hdlr  (soun)
//!       minf
//!         stbl
//!           stsd (one sowt sample entry)
//!           stts/stsc/stsz/stco (all empty — no moov-resident samples)
//!   mvex
//!     trex  (track_ID = 1, default_sample_size = 4 (s16 stereo))
//! moof  (sequence 1)
//!   mfhd
//!   traf
//!     tfhd  (default-base-is-moof, default_sample_duration = 1)
//!     tfdt  (base_media_decode_time = 0)
//!     trun  (4 samples, data_offset to first byte of mdat)
//! mdat  (4 × 4 bytes = 16 bytes)
//! moof  (sequence 2)
//!   mfhd
//!   traf
//!     tfhd  (default-base-is-moof)
//!     tfdt  (base_media_decode_time = 4)
//!     trun  (3 samples)
//! mdat  (3 × 4 bytes = 12 bytes)
//! ```

use std::io::Cursor;

use oxideav_core::{CodecId, Error, ReadSeek};

// --- Box-builder helpers -------------------------------------------------

/// Wrap `body` in a box header with the given fourcc.
fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

/// `ftyp` with major brand `iso6` (CMAF / fragmented).
fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"iso6"); // major_brand
    body.extend_from_slice(&512u32.to_be_bytes()); // minor_version
    body.extend_from_slice(b"iso6");
    body.extend_from_slice(b"mp41");
    body.extend_from_slice(b"dash");
    boxed(b"ftyp", &body)
}

/// `mvhd` v0: 4 fullbox + 4 created + 4 modified + 4 timescale +
/// 4 duration + 4 rate + 2 vol + 2+8 reserved + 36 matrix + 24 pre_def
/// + 4 next_track_ID = 100 bytes payload.
fn mvhd(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 100];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[16..20].copy_from_slice(&0u32.to_be_bytes()); // duration (will be 0; no moov samples)
    body[20..24].copy_from_slice(&0x00010000u32.to_be_bytes()); // rate
    body[24..26].copy_from_slice(&0x0100u16.to_be_bytes()); // volume
                                                            // matrix (identity): a=1, b=0, u=0, c=0, d=1, v=0, x=0, y=0, w=1
    let matrix_off = 36;
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[matrix_off + i * 4..matrix_off + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    body[96..100].copy_from_slice(&2u32.to_be_bytes()); // next_track_ID
    boxed(b"mvhd", &body)
}

/// `tkhd` v0 with track_ID = 1, audio (no width/height).
fn tkhd_audio(track_id: u32) -> Vec<u8> {
    let mut body = vec![0u8; 80];
    // version(0) + flags(track-enabled | track-in-movie) = 0x000007
    body[0] = 0;
    body[1..4].copy_from_slice(&[0, 0, 0x07]);
    // 4 created + 4 modified
    body[12..16].copy_from_slice(&track_id.to_be_bytes());
    // 4 reserved
    body[20..24].copy_from_slice(&0u32.to_be_bytes()); // duration
                                                       // 4+4 reserved + 2 layer + 2 alt_group
    body[36..38].copy_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0 (audio)
                                                            // 2 reserved
    let matrix_off = 40;
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[matrix_off + i * 4..matrix_off + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    // last 8 bytes: width + height (0 for audio).
    boxed(b"tkhd", &body)
}

fn mdhd_audio(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    // 4 created + 4 modified
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    // 4 duration (0)
    // 2 language (0x55C4 = "und")
    body[20..22].copy_from_slice(&0x55C4u16.to_be_bytes());
    boxed(b"mdhd", &body)
}

fn hdlr_soun() -> Vec<u8> {
    // FullBox(4) + pre_defined(4) + handler_type(4) + reserved(12) + name(...).
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // FullBox version+flags
    body.extend_from_slice(&[0u8; 4]); // pre_defined
    body.extend_from_slice(b"soun"); // handler_type
    body.extend_from_slice(&[0u8; 12]); // reserved (3 × u32)
    body.extend_from_slice(b"audio\0"); // name (null-terminated UTF-8)
    boxed(b"hdlr", &body)
}

fn smhd() -> Vec<u8> {
    let body = vec![0u8; 8];
    boxed(b"smhd", &body)
}

fn dinf_dref() -> Vec<u8> {
    // dref: FullBox + entry_count(1) + url-self-referencing
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&[0u8; 4]);
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    // url FullBox: version + flags = 0x000001 (self-contained)
    let url = {
        let mut b = Vec::new();
        b.extend_from_slice(&[0, 0, 0, 1]);
        boxed(b"url ", &b)
    };
    dref_body.extend_from_slice(&url);
    let dref = boxed(b"dref", &dref_body);
    boxed(b"dinf", &dref)
}

/// `stsd` containing a single `sowt` PCM s16le sample entry (28-byte
/// AudioSampleEntry preamble — no extra child boxes).
fn stsd_sowt(channels: u16, sample_rate: u32) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry[16..18].copy_from_slice(&channels.to_be_bytes());
    entry[18..20].copy_from_slice(&16u16.to_be_bytes()); // sample_size 16
    entry[24..28].copy_from_slice(&(sample_rate << 16).to_be_bytes());
    let entry_box = boxed(b"sowt", &entry);
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]); // FullBox
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    body.extend_from_slice(&entry_box);
    boxed(b"stsd", &body)
}

/// Empty `stts`, `stsc`, `stsz`, `stco` (no moov-resident samples).
fn empty_stts() -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&0u32.to_be_bytes());
    boxed(b"stts", &body)
}
fn empty_stsc() -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&0u32.to_be_bytes());
    boxed(b"stsc", &body)
}
fn empty_stsz() -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 (per-sample)
    body.extend_from_slice(&0u32.to_be_bytes()); // sample_count = 0
    boxed(b"stsz", &body)
}
fn empty_stco() -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&0u32.to_be_bytes());
    boxed(b"stco", &body)
}

fn stbl_minimal() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&stsd_sowt(2, 48_000));
    body.extend_from_slice(&empty_stts());
    body.extend_from_slice(&empty_stsc());
    body.extend_from_slice(&empty_stsz());
    body.extend_from_slice(&empty_stco());
    boxed(b"stbl", &body)
}

fn minf_audio() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&smhd());
    body.extend_from_slice(&dinf_dref());
    body.extend_from_slice(&stbl_minimal());
    boxed(b"minf", &body)
}

fn mdia_audio(timescale: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&mdhd_audio(timescale));
    body.extend_from_slice(&hdlr_soun());
    body.extend_from_slice(&minf_audio());
    boxed(b"mdia", &body)
}

fn trak_audio(track_id: u32, timescale: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&tkhd_audio(track_id));
    body.extend_from_slice(&mdia_audio(timescale));
    boxed(b"trak", &body)
}

/// `trex`: FullBox(4) + track_ID(4) + DSDI(4) + d_dur(4) + d_size(4) + d_flags(4)
fn trex(track_id: u32, ddur: u32, dsiz: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // FullBox
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // DSDI
    body.extend_from_slice(&ddur.to_be_bytes());
    body.extend_from_slice(&dsiz.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    boxed(b"trex", &body)
}

fn mvex_for_track(track_id: u32, ddur: u32, dsiz: u32) -> Vec<u8> {
    boxed(b"mvex", &trex(track_id, ddur, dsiz))
}

fn moov_audio(timescale: u32, track_id: u32, default_sample_dur: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&mvhd(timescale));
    body.extend_from_slice(&trak_audio(track_id, timescale));
    body.extend_from_slice(&mvex_for_track(
        track_id,
        default_sample_dur,
        4, /* stereo s16 */
    ));
    boxed(b"moov", &body)
}

fn mfhd(seq: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // FullBox
    body.extend_from_slice(&seq.to_be_bytes());
    boxed(b"mfhd", &body)
}

/// `tfhd` with default-base-is-moof (0x020000) +
/// default_sample_duration_present (0x000008). Other defaults come from
/// `trex`.
fn tfhd_default_base_is_moof(track_id: u32, default_dur: u32) -> Vec<u8> {
    let flags: u32 = 0x020000 | 0x000008;
    let mut body = Vec::new();
    body.push(0); // version
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&default_dur.to_be_bytes());
    boxed(b"tfhd", &body)
}

fn tfdt_v1(bmdt: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(1); // version 1 — 64-bit bmdt
    body.extend_from_slice(&[0u8; 3]);
    body.extend_from_slice(&bmdt.to_be_bytes());
    boxed(b"tfdt", &body)
}

/// `trun` with `data-offset-present` (0x000001) +
/// `sample-size-present` (0x000200). One per-sample size each.
fn trun_sized(data_offset: i32, sizes: &[u32]) -> Vec<u8> {
    let flags: u32 = 0x000001 | 0x000200;
    let mut body = Vec::new();
    body.push(0); // version
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    body.extend_from_slice(&data_offset.to_be_bytes());
    for &s in sizes {
        body.extend_from_slice(&s.to_be_bytes());
    }
    boxed(b"trun", &body)
}

/// Build one `moof` + `mdat` pair for an audio track.
///
/// `bmdt` is the base_media_decode_time; `payload_chunks` is the
/// per-sample byte payload — concatenated into the mdat and listed in
/// the trun by size.
fn moof_mdat_pair(
    seq: u32,
    track_id: u32,
    default_dur: u32,
    bmdt: u64,
    payload_chunks: &[Vec<u8>],
) -> Vec<u8> {
    // Build the trun's data_offset by computing the moof's total
    // size first (since data_offset is relative to the moof's start
    // when default-base-is-moof is set).
    let sizes: Vec<u32> = payload_chunks.iter().map(|p| p.len() as u32).collect();

    // Two-pass: first build the trun with a placeholder data_offset of 0,
    // then compute moof_size and rewrite data_offset to (moof_size + 8)
    // (i.e. moof_size + mdat header_len = first byte of mdat payload).
    let placeholder_trun = trun_sized(0, &sizes);
    let mut traf_body = Vec::new();
    traf_body.extend_from_slice(&tfhd_default_base_is_moof(track_id, default_dur));
    traf_body.extend_from_slice(&tfdt_v1(bmdt));
    traf_body.extend_from_slice(&placeholder_trun);
    let traf = boxed(b"traf", &traf_body);

    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&mfhd(seq));
    moof_body.extend_from_slice(&traf);
    let moof = boxed(b"moof", &moof_body);
    let moof_size = moof.len() as i32;

    // Now rewrite the trun: data_offset = moof_size + 8 (mdat header).
    let real_trun = trun_sized(moof_size + 8, &sizes);
    let mut traf_body = Vec::new();
    traf_body.extend_from_slice(&tfhd_default_base_is_moof(track_id, default_dur));
    traf_body.extend_from_slice(&tfdt_v1(bmdt));
    traf_body.extend_from_slice(&real_trun);
    let traf = boxed(b"traf", &traf_body);

    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&mfhd(seq));
    moof_body.extend_from_slice(&traf);
    let moof = boxed(b"moof", &moof_body);
    assert_eq!(moof.len() as i32, moof_size, "moof size shifted");

    let mut mdat_body = Vec::new();
    for p in payload_chunks {
        mdat_body.extend_from_slice(p);
    }
    let mdat = boxed(b"mdat", &mdat_body);

    let mut out = Vec::with_capacity(moof.len() + mdat.len());
    out.extend_from_slice(&moof);
    out.extend_from_slice(&mdat);
    out
}

// --- Tests ---------------------------------------------------------------

#[test]
fn fragmented_two_segments_round_trip() {
    let track_id = 1u32;
    let timescale = 48_000u32;
    let default_dur = 1u32; // 1 sample tick = 1 frame at 48 kHz

    // Build two fragments. Per-sample payload: 4 bytes (stereo s16) each.
    let frag1: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i, i + 1, i + 2, i + 3]).collect();
    let frag2: Vec<Vec<u8>> = (0..3u8)
        .map(|i| vec![10 + i, 20 + i, 30 + i, 40 + i])
        .collect();

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov_audio(timescale, track_id, default_dur));
    file.extend_from_slice(&moof_mdat_pair(1, track_id, default_dur, 0, &frag1));
    file.extend_from_slice(&moof_mdat_pair(2, track_id, default_dur, 4, &frag2));

    // Demux it.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id,
        CodecId::new("pcm_s16le"),
        "sowt → pcm_s16le"
    );
    assert_eq!(dmx.streams()[0].params.channels, Some(2));
    assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));

    // Walk every packet.
    let mut got: Vec<(i64, i64, i64, Vec<u8>)> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push((
                p.pts.unwrap_or(0),
                p.dts.unwrap_or(0),
                p.duration.unwrap_or(0),
                p.data,
            )),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }

    assert_eq!(got.len(), 7, "expected 4 + 3 = 7 fragmented samples");

    // Expected sequence of (dts, dur, payload).
    let mut expected: Vec<(i64, i64, Vec<u8>)> = Vec::new();
    for (i, p) in frag1.iter().enumerate() {
        expected.push((i as i64, 1, p.clone()));
    }
    for (i, p) in frag2.iter().enumerate() {
        // bmdt for frag2 is 4.
        expected.push((4 + i as i64, 1, p.clone()));
    }

    for (i, ((pts, dts, dur, data), (exp_dts, exp_dur, exp_data))) in
        got.iter().zip(expected.iter()).enumerate()
    {
        // No ctts in our synthetic file → pts == dts.
        assert_eq!(pts, dts, "sample {i}: pts != dts (no ctts present)");
        assert_eq!(*dts, *exp_dts, "sample {i}: dts mismatch");
        assert_eq!(*dur, *exp_dur, "sample {i}: dur mismatch");
        assert_eq!(data, exp_data, "sample {i}: payload mismatch");
    }
}

/// Multi-segment edit list: two `elst` entries (one empty + one media)
/// must surface the *first non-empty* media_time as the leading shift.
#[test]
fn multi_segment_elst_uses_first_non_empty_media_time() {
    // A single-fragment fragmented file with an elst that has an empty
    // dwell + a real media segment.
    use std::io::Cursor;

    fn elst_v0(entries: &[(u32, i32)]) -> Vec<u8> {
        // FullBox + entry_count + (segment_duration u32, media_time i32, media_rate u32) per entry
        let mut body = vec![0u8; 4]; // FullBox
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(dur, mt) in entries {
            body.extend_from_slice(&dur.to_be_bytes());
            body.extend_from_slice(&mt.to_be_bytes());
            body.extend_from_slice(&0x00010000u32.to_be_bytes());
        }
        boxed(b"elst", &body)
    }

    fn edts_with(entries: &[(u32, i32)]) -> Vec<u8> {
        boxed(b"edts", &elst_v0(entries))
    }

    fn trak_audio_with_edts(track_id: u32, timescale: u32, edts: Vec<u8>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&tkhd_audio(track_id));
        body.extend_from_slice(&edts);
        body.extend_from_slice(&mdia_audio(timescale));
        boxed(b"trak", &body)
    }

    fn moov_audio_with_edts(timescale: u32, track_id: u32, ddur: u32, edts: Vec<u8>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&mvhd(timescale));
        body.extend_from_slice(&trak_audio_with_edts(track_id, timescale, edts));
        body.extend_from_slice(&mvex_for_track(track_id, ddur, 4));
        boxed(b"moov", &body)
    }

    let track_id = 1u32;
    let timescale = 48_000u32;
    let default_dur = 1u32;

    // elst: dwell of 100 movie ticks (media_time = -1) + real segment
    // starting at media_time = 5 (track-timescale) for 1000 ticks.
    let edts = edts_with(&[(100, -1), (1000, 5)]);

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov_audio_with_edts(
        timescale,
        track_id,
        default_dur,
        edts,
    ));

    let frag: Vec<Vec<u8>> = (0..3u8).map(|i| vec![i; 4]).collect();
    file.extend_from_slice(&moof_mdat_pair(1, track_id, default_dur, 10, &frag));

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    let mut got = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.dts.unwrap_or(0)),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), 3);
    // bmdt = 10, elst leading shift = 5 → first sample DTS = 10 - 5 = 5.
    assert_eq!(got, vec![5, 6, 7], "elst leading shift not applied");
}

/// Files with a `styp` segment-type box at the segment boundary
/// (CMAF / DASH) must still demux cleanly.
#[test]
fn styp_segment_marker_is_skipped() {
    fn styp() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"msdh");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(b"msdh");
        body.extend_from_slice(b"msix");
        boxed(b"styp", &body)
    }

    let track_id = 1u32;
    let timescale = 48_000u32;
    let default_dur = 1u32;
    let frag: Vec<Vec<u8>> = (0..2u8).map(|i| vec![i; 4]).collect();

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov_audio(timescale, track_id, default_dur));
    file.extend_from_slice(&styp()); // <-- segment-type box before each segment
    file.extend_from_slice(&moof_mdat_pair(1, track_id, default_dur, 0, &frag));

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut count = 0;
    while let Ok(_p) = dmx.next_packet() {
        count += 1;
    }
    assert_eq!(count, 2);
}
