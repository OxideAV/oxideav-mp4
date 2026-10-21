//! Gapless audio timing (Mp4Demuxer::gapless_timing) on the two ways
//! files carry it: the M4A iTunSMPB item, and an edit list with an AAC
//! roll group.

use oxideav_core::RuntimeContext;
use oxideav_mp4::demux::{open_typed, GaplessSource, GaplessTiming};

fn timing(data: &[u8]) -> Option<GaplessTiming> {
    let ctx = RuntimeContext::new();
    open_typed(Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs)
        .unwrap()
        .gapless_timing(0)
}

#[test]
fn m4a_itunsmpb() {
    // macOS's AAC encoder: 50 ms at 48 kHz; afinfo reports
    // 2400 valid frames + 2112 priming + 608 remainder.
    assert_eq!(
        timing(include_bytes!("fixtures/apple-aac-5.1.m4a")),
        Some(GaplessTiming {
            priming: 2112,
            valid: 2400,
            remainder: 608,
            source: GaplessSource::ITunSmpb
        })
    );
}

#[test]
fn edit_list_with_roll_group() {
    // elst media_time 1024, segment 0.5 s at 48 kHz; afinfo reports
    // 24000 valid frames + 1024 priming + 576 remainder.
    assert_eq!(
        timing(include_bytes!("fixtures/aac-elst-roll.mp4")),
        Some(GaplessTiming {
            priming: 1024,
            valid: 24000,
            remainder: 576,
            source: GaplessSource::EditList
        })
    );
}
