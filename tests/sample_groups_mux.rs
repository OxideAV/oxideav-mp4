//! Integration tests for write-side `sbgp` / `sgpd` emission
//! (ISO/IEC 14496-12 §8.9.2 / §8.9.3). Mux a small PCM stream with
//! caller-supplied sample groups, then re-demux and verify the
//! demuxer surfaces them on `params.options` exactly as encoded.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

use oxideav_mp4::options::{Mp4MuxerOptions, TrackSampleGroups};
use oxideav_mp4::sample_groups::{
    CompactSampleToGroup, CompactSampleToGroupPattern, SampleGroupDescription, SampleToGroup,
};

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
        csgp: vec![],
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
        csgp: vec![],
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
        csgp: vec![],
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
        csgp: vec![],
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
fn csgp_after_sgpd_roundtrip_via_demux() {
    // The compact alternative to `sbgp` (ISO/IEC 14496-12 §8.9.5). One
    // pattern, replayed across a run of samples. `sgpd` declares the
    // description table; `csgp` indexes into it compactly.
    let sgpd = SampleGroupDescription {
        grouping_type: *b"roll",
        default_sample_description_index: None,
        entries: vec![vec![0xFF, 0xFB], vec![0x00, 0x05]],
    };
    let csgp = CompactSampleToGroup {
        grouping_type: *b"roll",
        grouping_type_parameter: None,
        index_msb_indicates_fragment_local_description: false,
        // pattern [1, 2] replayed for 3 consecutive groups (= 6 samples).
        patterns: vec![CompactSampleToGroupPattern {
            sample_count: 3,
            indices: vec![1, 2],
        }],
    };
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![],
        sgpd: vec![sgpd],
        csgp: vec![csgp],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("sgpd_0"), Some("roll fffb 0005"));
    // Demuxer renders `csgp_<n>` as `<gt>[ param=P][ fraglocal] count*i0,i1`.
    assert_eq!(opts.get("csgp_0"), Some("roll 3*1,2"));
    // No `sbgp_` keys — this track used the compact form only.
    assert!(!opts.iter().any(|(k, _)| k.starts_with("sbgp_")));
}

#[test]
fn csgp_with_grouping_type_parameter_and_multi_pattern_roundtrip() {
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![],
        sgpd: vec![SampleGroupDescription {
            grouping_type: *b"rap ",
            default_sample_description_index: None,
            entries: vec![vec![0x01], vec![0x02]],
        }],
        csgp: vec![CompactSampleToGroup {
            grouping_type: *b"rap ",
            grouping_type_parameter: Some(42),
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![
                CompactSampleToGroupPattern {
                    sample_count: 2,
                    indices: vec![1, 2, 1],
                },
                CompactSampleToGroupPattern {
                    sample_count: 1,
                    indices: vec![2],
                },
            ],
        }],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("sgpd_0"), Some("rap  01 02"));
    assert_eq!(opts.get("csgp_0"), Some("rap  param=42 2*1,2,1 1*2"));
}

#[test]
fn sbgp_and_csgp_distinct_grouping_types_coexist() {
    // §8.9.5 forbids both forms for ONE grouping_type, but distinct
    // grouping_types may each pick their own form on the same track.
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![SampleToGroup {
            grouping_type: *b"sync",
            grouping_type_parameter: None,
            entries: vec![(1, 1), (5, 0)],
        }],
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
        csgp: vec![CompactSampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: false,
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 6,
                indices: vec![1],
            }],
        }],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("sgpd_0"), Some("roll fffb"));
    assert_eq!(opts.get("sgpd_1"), Some("sync aa"));
    assert_eq!(opts.get("sbgp_0"), Some("sync 1:1 5:0"));
    assert_eq!(opts.get("csgp_0"), Some("roll 6*1"));
}

#[test]
fn csgp_fragment_local_flag_preserved_in_stbl() {
    // The fragment-local MSB flag is legal only inside a `traf`, but the
    // muxer serialises it verbatim if a caller sets it — round-trip the
    // flag bit and the high-bit-set index through the demuxer.
    let stream = pcm_stream();
    let tsg = TrackSampleGroups {
        stream_index: 0,
        sbgp: vec![],
        sgpd: vec![SampleGroupDescription {
            grouping_type: *b"seig",
            default_sample_description_index: None,
            entries: vec![vec![0x00]],
        }],
        csgp: vec![CompactSampleToGroup {
            grouping_type: *b"seig",
            grouping_type_parameter: None,
            index_msb_indicates_fragment_local_description: true,
            // 8-bit-wide index with the high bit set (0x81) = fragment-local.
            patterns: vec![CompactSampleToGroupPattern {
                sample_count: 4,
                indices: vec![0x81],
            }],
        }],
    };
    let bytes = mux_with_sample_groups(&stream, tsg);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let opts = &dmx.streams()[0].params.options;
    // `fraglocal` marker + the raw 0x81 index preserved verbatim.
    assert_eq!(opts.get("csgp_0"), Some("seig fraglocal 4*129"));
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
