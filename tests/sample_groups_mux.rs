//! Integration tests for write-side `sbgp` / `sgpd` emission
//! (ISO/IEC 14496-12 §8.9.2 / §8.9.3). Mux a small PCM stream with
//! caller-supplied sample groups, then re-demux and verify the
//! demuxer surfaces them on `params.options` exactly as encoded.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_mp4::options::{Mp4MuxerOptions, TrackSampleGroups};
use oxideav_mp4::sample_groups::{SampleGroupDescription, SampleToGroup};

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

fn make_pcm(samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        out.extend_from_slice(&(i as i16).wrapping_mul(7).to_le_bytes());
        out.extend_from_slice(&(i as i16).wrapping_mul(11).to_le_bytes());
    }
    out
}

/// Mux the stream with `tsg` attached, then return the file bytes.
fn mux_with_sample_groups(stream: &StreamInfo, tsg: TrackSampleGroups) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-sgmux-{}-{}.mp4",
        std::process::id(),
        seq
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            track_sample_groups: vec![tsg],
            large_mdat: false,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(stream), opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..4 {
            let mut pkt = Packet::new(0, stream.time_base, make_pcm(1024));
            pkt.pts = Some((i as i64) * 1024);
            pkt.duration = Some(1024);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

#[test]
fn sgpd_then_sbgp_roundtrip_via_demux() {
    // `roll` grouping: per-entry 2-byte signed `roll_distance` blob.
    // The container does not interpret it — pass it as opaque bytes.
    let sgpd = SampleGroupDescription {
        grouping_type: *b"roll",
        default_sample_description_index: None,
        entries: vec![vec![0xFF, 0xFB], vec![0x00, 0x05]],
    };
    let sbgp = SampleToGroup {
        grouping_type: *b"roll",
        grouping_type_parameter: None,
        // 4 samples; first 2 → group 1, next 1 → group 2, last 1 → none.
        entries: vec![(2, 1), (1, 2), (1, 0)],
    };
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![sbgp],
        sgpd: vec![sgpd],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    // The demuxer renders `sbgp_<n>` as `<gt>[ param=<P>][ count:index]*`
    // and `sgpd_<n>` as `<gt>[ default=<D>][ <hex_payload>]*`.
    assert_eq!(opts.get("sgpd_0"), Some("roll fffb 0005"));
    assert_eq!(opts.get("sbgp_0"), Some("roll 2:1 1:2 1:0"));
}

#[test]
fn sbgp_v1_grouping_type_parameter_preserved() {
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![SampleToGroup {
            grouping_type: *b"rap ",
            grouping_type_parameter: Some(42),
            entries: vec![(4, 1)],
        }],
        sgpd: vec![SampleGroupDescription {
            grouping_type: *b"rap ",
            default_sample_description_index: None,
            entries: vec![vec![0x01]],
        }],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("sbgp_0"), Some("rap  param=42 4:1"));
    assert_eq!(opts.get("sgpd_0"), Some("rap  01"));
}

#[test]
fn multiple_grouping_types_accumulate_in_order() {
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![
            SampleToGroup {
                grouping_type: *b"roll",
                grouping_type_parameter: None,
                entries: vec![(4, 1)],
            },
            SampleToGroup {
                grouping_type: *b"sync",
                grouping_type_parameter: None,
                entries: vec![(1, 1), (3, 0)],
            },
        ],
        sgpd: vec![
            SampleGroupDescription {
                grouping_type: *b"roll",
                default_sample_description_index: None,
                entries: vec![vec![0xFF, 0xFB]],
            },
            SampleGroupDescription {
                grouping_type: *b"sync",
                default_sample_description_index: None,
                entries: vec![vec![0xAA]],
            },
        ],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("sgpd_0"), Some("roll fffb"));
    assert_eq!(opts.get("sgpd_1"), Some("sync aa"));
    assert_eq!(opts.get("sbgp_0"), Some("roll 4:1"));
    assert_eq!(opts.get("sbgp_1"), Some("sync 1:1 3:0"));
}

#[test]
fn sgpd_v2_with_default_sample_description_index_roundtrips() {
    // Entries share length → v2 is selected.
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![SampleToGroup {
            grouping_type: *b"alst",
            grouping_type_parameter: None,
            entries: vec![(4, 1)],
        }],
        sgpd: vec![SampleGroupDescription {
            grouping_type: *b"alst",
            default_sample_description_index: Some(2),
            entries: vec![vec![0xCA, 0xFE], vec![0xBE, 0xEF]],
        }],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    // Demuxer renders the v2 default as `default=<D>`. The container
    // layer cannot recover per-entry boundaries from v2 alone (entry
    // length is grouping-type-specific, not signalled in the box) so
    // it folds all entry bytes into one combined blob — the
    // grouping-type-aware caller would re-split, in this case at 2
    // bytes per entry. We assert the combined-blob shape because
    // that's what the demuxer surfaces today.
    assert_eq!(opts.get("sgpd_0"), Some("alst default=2 cafebeef"));
    assert_eq!(opts.get("sbgp_0"), Some("alst 4:1"));
}

#[test]
fn no_sample_groups_means_no_keys() {
    // Default options → no sample-group entries → no sbgp_/sgpd_ keys.
    let stream = pcm_stream();
    let tmp =
        std::env::temp_dir().join(format!("oxideav-mp4-sgmux-none-{}.mp4", std::process::id()));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, make_pcm(1024));
        pkt.pts = Some(0);
        pkt.duration = Some(1024);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert!(!opts.iter().any(|(k, _)| k.starts_with("sbgp_")));
    assert!(!opts.iter().any(|(k, _)| k.starts_with("sgpd_")));
}
