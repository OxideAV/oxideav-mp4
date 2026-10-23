//! An ALAC track's channel count and rate come from its ALACSpecificConfig,
//! not the sample entry's legacy fields.
use oxideav_core::{Demuxer, RuntimeContext};

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
}
