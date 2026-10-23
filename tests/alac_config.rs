//! An ALAC track's channel count and rate come from its ALACSpecificConfig,
//! not the sample entry's legacy fields.
use oxideav_core::{Demuxer, RuntimeContext, SampleFormat};

#[test]
fn alac_stream_parameters_follow_the_cookie() {
    // 40 ms of a 6-channel 96 kHz tone from macOS's ALAC encoder (afconvert);
    // its sample entry says 2 channels at 44100 Hz.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/apple-alac-6ch-96k.m4a"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    let p = &d.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "alac");
    assert_eq!(p.channels, Some(6));
    assert_eq!(p.sample_rate, Some(96000));
    assert_eq!(p.sample_format, Some(SampleFormat::S16));
}

#[test]
fn alac_bit_depth_follows_the_cookie() {
    // 40 ms of a stereo 48 kHz tone from a 24-bit source, encoded by
    // macOS's ALAC encoder (afconvert): the entry's samplesize is the
    // legacy 16, the cookie's bitDepth is 24 (afinfo: "from 24-bit
    // source").
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/apple-alac-24bit.m4a"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    let p = &d.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "alac");
    assert_eq!(p.extradata[5], 24);
    assert_eq!((p.channels, p.sample_rate), (Some(2), Some(48_000)));
    assert_eq!(p.sample_format, Some(SampleFormat::S24));
}
