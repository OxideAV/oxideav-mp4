//! Integration test for the write-side `styp` (Segment Type Box,
//! ISO/IEC 14496-12 §8.16.2) emitter.
//!
//! Two layers of byte-exact validation:
//!
//! 1. **Stateless byte builder** ([`oxideav_mp4::styp::build_styp`]) —
//!    the spec layout of `[size:u32][type:'styp'][major:4][minor:u32]
//!    [compat:4]*N` is round-trippable through a manual parser without
//!    touching the muxer state machine.
//!
//! 2. **Muxer integration** — the
//!    [`FragmentedMuxer::write_fragmented_segment_with_styp`] inherent
//!    method, set between `write_packet` calls, must override the
//!    configured `frag_options.styp` for exactly one segment, and the
//!    overridden bytes must be locatable verbatim inside the produced
//!    file at the right place (before the next `moof`).

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::frag::{open_fragmented_typed, FragmentedMuxer};
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};
use oxideav_mp4::styp;

// --- Stateless-builder byte-exact assertions -----------------------------

#[test]
fn build_styp_dash_init_segment_byte_layout() {
    // The spec example shape — `(major = "iso5", compat = ["iso5", "dash",
    // "msdh"])` — must serialise to exactly 28 bytes with the layout in
    // the styp module docstring.
    let bytes = styp::build_styp(*b"iso5", &[*b"iso5", *b"dash", *b"msdh"]);
    let want: &[u8] = &[
        0x00, 0x00, 0x00, 0x1c, // size = 28
        b's', b't', b'y', b'p', // type
        b'i', b's', b'o', b'5', // major
        0x00, 0x00, 0x00, 0x00, // minor
        b'i', b's', b'o', b'5', // compat[0]
        b'd', b'a', b's', b'h', // compat[1]
        b'm', b's', b'd', b'h', // compat[2]
    ];
    assert_eq!(bytes, want);
}

#[test]
fn build_styp_with_minor_round_trip() {
    // The byte builder's `*_with_minor` variant lets a writer round-trip
    // a parsed Styp's `minor_version` verbatim. Re-parse the emitted
    // bytes by hand and check field equality.
    let major = *b"cmfs";
    let minor = 0x0102_0304;
    let compat = [*b"cmfc", *b"cmff"];
    let bytes = styp::build_styp_with_minor(major, minor, &compat);
    // Header: 8-byte (size, type).
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    assert_eq!(size as usize, bytes.len());
    assert_eq!(&bytes[4..8], b"styp");
    // Body: 4-byte major + 4-byte minor + N×4 compat.
    assert_eq!(&bytes[8..12], &major);
    let parsed_minor = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    assert_eq!(parsed_minor, minor);
    assert_eq!(&bytes[16..20], &compat[0]);
    assert_eq!(&bytes[20..24], &compat[1]);
}

#[test]
fn write_styp_streams_to_writer_matches_vec_builder() {
    // Stream-emitter and Vec-builder must produce identical bytes — the
    // two are intended as interchangeable APIs differing only in
    // allocation behaviour.
    let major = *b"iso6";
    let compat = [*b"iso6", *b"cmfs", *b"cmff"];
    let v = styp::build_styp(major, &compat);
    let mut sink = Vec::new();
    styp::write_styp(&mut sink, major, &compat).unwrap();
    assert_eq!(sink, v);
}

// --- Muxer-integration helpers -------------------------------------------

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

fn pcm_packet(i: i64, frames: i64) -> Packet {
    let stream = pcm_stream();
    let payload = vec![(i & 0xFF) as u8; (frames * 4) as usize];
    let mut p = Packet::new(0, stream.time_base, payload);
    p.pts = Some(i * frames);
    p.dts = Some(i * frames);
    p.duration = Some(frames);
    p.flags.keyframe = true;
    p
}

/// Find every position where `needle` occurs as a 4-byte box type
/// (i.e. matches `bytes[i..i+4]`).
fn find_all(bytes: &[u8], needle: &[u8; 4]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == needle {
            out.push(i);
        }
        i += 1;
    }
    out
}

/// Open a fragmented muxer that writes to a fresh tempfile, returning
/// `(muxer, path)`. The caller drives `write_header` / per-packet /
/// `write_trailer` and then reads `path` back as bytes for inspection.
/// (`Box<dyn WriteSeek>` requires `'static`, so we go through the
/// filesystem rather than an in-memory `Cursor<&mut Vec<u8>>`.)
fn open_to_tempfile(
    name: &str,
    opts: Mp4MuxerOptions,
    frag_opts: FragmentedOptions,
) -> (FragmentedMuxer, std::path::PathBuf) {
    let path = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mux = open_fragmented_typed(ws, std::slice::from_ref(&stream), opts, frag_opts).unwrap();
    (mux, path)
}

// --- Muxer-integration byte-exact tests ----------------------------------

#[test]
fn write_fragmented_segment_with_styp_overrides_one_segment() {
    // Configure the fragmented muxer with one default styp (msdh) and
    // override the second segment's styp to a different brand pair
    // (cmfs / [cmfs, cmff]). Verify the produced file contains:
    //   - first  segment: default styp(msdh,  [msdh, msix])
    //   - second segment: overridden styp(cmfs, [cmfs, cmff])
    //   - third  segment: default styp again (override consumed on use)
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"dash"],
        },
        faststart: false,
        fragmented: None,
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: false,
    };
    let frag_opts = FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(1),
        styp: Some(BrandPreset::Custom {
            major: *b"msdh",
            compatible: vec![*b"msdh", *b"msix"],
        }),
        // Disable sidx to keep the per-segment byte layout simple:
        // each segment is just `styp + moof + mdat`.
        emit_random_access_indexes: false,
        levels: Vec::new(),
        emit_ssix: false,
        ssix_levels: (1, 2),
        treps: Vec::new(),
    };
    let path = std::env::temp_dir().join("oxideav-mp4-r127-styp-override.mp4");
    {
        let f = std::fs::File::create(&path).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mux =
            open_fragmented_typed(ws, std::slice::from_ref(&stream), opts, frag_opts).unwrap();
        mux.write_header().unwrap();

        // Segment 1 — default styp(msdh).
        mux.write_packet(&pcm_packet(0, 256)).unwrap();

        // Segment 2 — override to styp(cmfs).
        mux.write_fragmented_segment_with_styp(*b"cmfs", &[*b"cmfs", *b"cmff"]);
        mux.write_packet(&pcm_packet(1, 256)).unwrap();

        // Segment 3 — default styp again (the override consumed itself).
        mux.write_packet(&pcm_packet(2, 256)).unwrap();

        mux.write_trailer().unwrap();
    }
    let buf = std::fs::read(&path).unwrap();

    // The file must contain three styp boxes (one per segment) plus the
    // init ftyp. styp positions: find each occurrence.
    let styp_positions = find_all(&buf, b"styp");
    assert_eq!(
        styp_positions.len(),
        3,
        "expected 3 styp boxes, got {} (positions: {:?})",
        styp_positions.len(),
        styp_positions
    );

    // Each styp's bytes are located at position − 4 (the box size prefix
    // sits 4 bytes before the 'styp' fourcc).
    let read_styp = |type_pos: usize| -> &[u8] {
        let size_off = type_pos - 4;
        let size = u32::from_be_bytes([
            buf[size_off],
            buf[size_off + 1],
            buf[size_off + 2],
            buf[size_off + 3],
        ]) as usize;
        &buf[size_off..size_off + size]
    };

    let default_styp_bytes = styp::build_styp(*b"msdh", &[*b"msdh", *b"msix"]);
    let override_styp_bytes = styp::build_styp(*b"cmfs", &[*b"cmfs", *b"cmff"]);

    // Segment 1 — default.
    assert_eq!(read_styp(styp_positions[0]), default_styp_bytes.as_slice());
    // Segment 2 — override.
    assert_eq!(read_styp(styp_positions[1]), override_styp_bytes.as_slice());
    // Segment 3 — default again (the override is consumed on use).
    assert_eq!(read_styp(styp_positions[2]), default_styp_bytes.as_slice());
}

#[test]
fn write_fragmented_segment_with_styp_emits_styp_when_preset_is_none() {
    // Configure with `frag_options.styp = None` (no per-segment styp by
    // default) but explicitly request one for the first segment via the
    // override. Exactly one styp must appear in the file.
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"dash"],
        },
        faststart: false,
        fragmented: None,
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: false,
    };
    let frag_opts = FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(1),
        styp: None,
        emit_random_access_indexes: false,
        levels: Vec::new(),
        emit_ssix: false,
        ssix_levels: (1, 2),
        treps: Vec::new(),
    };
    let (mut mux, path) =
        open_to_tempfile("oxideav-mp4-r127-styp-preset-none.mp4", opts, frag_opts);
    mux.write_header().unwrap();
    // Segment 1 — override-only, this is the only styp the file gets.
    mux.write_fragmented_segment_with_styp(*b"msix", &[*b"msdh", *b"msix"]);
    mux.write_packet(&pcm_packet(0, 256)).unwrap();
    // Segment 2 — no override, no preset → no styp.
    mux.write_packet(&pcm_packet(1, 256)).unwrap();
    mux.write_trailer().unwrap();
    drop(mux);
    let buf = std::fs::read(&path).unwrap();

    let styp_positions = find_all(&buf, b"styp");
    assert_eq!(
        styp_positions.len(),
        1,
        "expected exactly 1 styp (override on segment 1), got {}",
        styp_positions.len()
    );
    let size = u32::from_be_bytes([
        buf[styp_positions[0] - 4],
        buf[styp_positions[0] - 3],
        buf[styp_positions[0] - 2],
        buf[styp_positions[0] - 1],
    ]) as usize;
    let want = styp::build_styp(*b"msix", &[*b"msdh", *b"msix"]);
    let start = styp_positions[0] - 4;
    assert_eq!(&buf[start..start + size], want.as_slice());
}

#[test]
fn write_fragmented_segment_with_styp_precedes_moof() {
    // Per §8.16.2 "Valid segment type boxes shall be the first box in a
    // segment". For the override flow that means: the override styp must
    // appear immediately before the segment's moof, not after it.
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"dash"],
        },
        faststart: false,
        fragmented: None,
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: false,
    };
    let frag_opts = FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(1),
        styp: None,
        emit_random_access_indexes: false,
        levels: Vec::new(),
        emit_ssix: false,
        ssix_levels: (1, 2),
        treps: Vec::new(),
    };
    let (mut mux, path) =
        open_to_tempfile("oxideav-mp4-r127-styp-precedes-moof.mp4", opts, frag_opts);
    mux.write_header().unwrap();
    mux.write_fragmented_segment_with_styp(*b"cmfs", &[*b"cmfs"]);
    mux.write_packet(&pcm_packet(0, 256)).unwrap();
    mux.write_trailer().unwrap();
    drop(mux);
    let buf = std::fs::read(&path).unwrap();

    let styp_pos = find_all(&buf, b"styp");
    let moof_pos = find_all(&buf, b"moof");
    assert_eq!(styp_pos.len(), 1);
    assert_eq!(moof_pos.len(), 1);
    // The styp box's 'styp' fourcc must precede the moof box's 'moof'
    // fourcc. Subtract 4 for the size prefix to compare start-of-box.
    let styp_start = styp_pos[0] - 4;
    let moof_start = moof_pos[0] - 4;
    assert!(
        styp_start < moof_start,
        "styp box (start {styp_start}) must precede moof box (start {moof_start})"
    );
    // And the styp must be IMMEDIATELY before the moof — no other box
    // between them — i.e. styp_end == moof_start.
    let styp_size = u32::from_be_bytes([
        buf[styp_start],
        buf[styp_start + 1],
        buf[styp_start + 2],
        buf[styp_start + 3],
    ]) as usize;
    assert_eq!(
        styp_start + styp_size,
        moof_start,
        "styp end must touch moof start"
    );
}

#[test]
fn write_fragmented_segment_with_styp_empty_compat_emits_sixteen_bytes() {
    // Override with an empty compat list — the emitted box body is just
    // the 8-byte (major + minor), so the total box is exactly 16 bytes.
    // Per §4.3 (inherited by §8.16.2) this is a legal styp.
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6"],
        },
        faststart: false,
        fragmented: None,
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: false,
    };
    let frag_opts = FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(1),
        styp: None,
        emit_random_access_indexes: false,
        levels: Vec::new(),
        emit_ssix: false,
        ssix_levels: (1, 2),
        treps: Vec::new(),
    };
    let (mut mux, path) =
        open_to_tempfile("oxideav-mp4-r127-styp-empty-compat.mp4", opts, frag_opts);
    mux.write_header().unwrap();
    mux.write_fragmented_segment_with_styp(*b"msdh", &[]);
    mux.write_packet(&pcm_packet(0, 256)).unwrap();
    mux.write_trailer().unwrap();
    drop(mux);
    let buf = std::fs::read(&path).unwrap();

    let styp_pos = find_all(&buf, b"styp");
    assert_eq!(styp_pos.len(), 1);
    let start = styp_pos[0] - 4;
    let size = u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
    assert_eq!(size, 16);
}
