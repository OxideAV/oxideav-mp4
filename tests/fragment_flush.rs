//! Explicit fragment-boundary API tests.
//!
//! The public `flush_fragment` method is for callers that own a segment
//! boundary, such as a Blazar sink. Automatic cadence behavior remains a
//! separate internal path.

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::frag::open_fragmented_typed;
use oxideav_mp4::options::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

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
    let mut packet = Packet::new(0, stream.time_base, vec![i as u8; (frames * 4) as usize]);
    packet.pts = Some(i * frames);
    packet.dts = Some(i * frames);
    packet.duration = Some(frames);
    packet.flags.keyframe = true;
    packet
}

fn count_box(bytes: &[u8], fourcc: &[u8; 4]) -> usize {
    bytes.windows(4).filter(|window| *window == fourcc).count()
}

fn open_muxer(path: &std::path::Path) -> oxideav_mp4::frag::FragmentedMuxer {
    let file = std::fs::File::create(path).unwrap();
    let output: Box<dyn WriteSeek> = Box::new(file);
    let options = Mp4MuxerOptions::default();
    let fragment_options = FragmentedOptions {
        cadence: FragmentCadence::EverySeconds(1000.0),
        styp: None,
        emit_random_access_indexes: false,
        ..FragmentedOptions::default()
    };
    open_fragmented_typed(output, &[pcm_stream()], options, fragment_options).unwrap()
}

#[test]
fn explicit_flush_emits_pending_fragment_without_waiting_for_cadence() {
    let path = std::env::temp_dir().join("oxideav-mp4-explicit-flush.mp4");
    let mut muxer = open_muxer(&path);
    muxer.write_header().unwrap();

    muxer.write_packet(&pcm_packet(0, 256)).unwrap();
    muxer.flush_fragment().unwrap();

    muxer.write_packet(&pcm_packet(1, 256)).unwrap();
    muxer.flush_fragment().unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(count_box(&bytes, b"moof"), 2);
    assert_eq!(count_box(&bytes, b"mdat"), 2);
}

#[test]
fn explicit_flush_with_no_pending_samples_is_a_noop() {
    let path = std::env::temp_dir().join("oxideav-mp4-empty-flush.mp4");
    let mut muxer = open_muxer(&path);
    muxer.write_header().unwrap();
    muxer.flush_fragment().unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(count_box(&bytes, b"moof"), 0);
    assert_eq!(count_box(&bytes, b"mdat"), 0);
}

#[test]
fn explicit_flush_retains_a_trailing_keyframe() {
    let path = std::env::temp_dir().join("oxideav-mp4-keyframe-flush.mp4");
    let mut muxer = open_muxer(&path);
    muxer.write_header().unwrap();
    muxer.write_packet(&pcm_packet(0, 256)).unwrap();
    muxer.flush_fragment().unwrap();
    muxer.write_trailer().unwrap();
    drop(muxer);

    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(count_box(&bytes, b"moof"), 1);
    assert_eq!(count_box(&bytes, b"mdat"), 1);
}
