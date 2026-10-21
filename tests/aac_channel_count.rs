//! An AAC track whose `mp4a` entry says 2 channels while its
//! AudioSpecificConfig declares 5.1 (as macOS's AAC file writer emits)
//! reports the ASC's six channels.

use oxideav_core::{Demuxer, MediaType, RuntimeContext};

#[test]
fn apple_5_1_m4a_reports_six_channels() {
    let data = include_bytes!("fixtures/apple-aac-5.1.m4a");
    let ctx = RuntimeContext::new();
    let d = oxideav_mp4::demux::open(Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs)
        .unwrap();
    let s = &d.streams()[0];
    assert_eq!(s.params.media_type, MediaType::Audio);
    assert_eq!(s.params.codec_id.as_str(), "aac");
    assert_eq!(s.params.channels, Some(6));
    assert_eq!(s.params.sample_rate, Some(48_000));
}
