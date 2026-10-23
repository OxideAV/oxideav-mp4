//! Gapless audio timing (Mp4Demuxer::gapless_timing) on the two ways
//! files carry it: the M4A iTunSMPB item, and an edit list with an AAC
//! roll group.

use oxideav_core::{Demuxer, RuntimeContext};
use oxideav_mp4::demux::{GaplessSource, GaplessTiming, open_typed};

fn timing(data: &[u8]) -> Option<GaplessTiming> {
    let ctx = RuntimeContext::new();
    open_typed(Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs)
        .unwrap()
        .gapless_timing(0)
}

/// The stream's reported sample rate plus the flat `gapless_*` mirror.
fn rate_and_flat(data: &[u8]) -> (Option<u32>, Vec<(String, String)>) {
    let ctx = RuntimeContext::new();
    let d = open_typed(Box::new(std::io::Cursor::new(data.to_vec())), &ctx.codecs).unwrap();
    let s = &d.streams()[0];
    let mut flat: Vec<(String, String)> = s
        .params
        .options
        .iter()
        .filter(|(k, _)| k.starts_with("gapless_"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    flat.sort();
    (s.params.sample_rate, flat)
}

#[test]
fn m4a_itunsmpb() {
    // macOS's AAC encoder: 50 ms at 48 kHz; afinfo reports
    // 2400 valid frames + 2112 priming + 608 remainder.
    assert_eq!(
        timing(include_bytes!("fixtures/apple-aac-5.1.m4a")),
        Some(GaplessTiming::new(2112, 2400, 608, GaplessSource::ITunSmpb))
    );
    let (rate, flat) = rate_and_flat(include_bytes!("fixtures/apple-aac-5.1.m4a"));
    assert_eq!(rate, Some(48_000));
    assert_eq!(
        flat,
        [
            ("gapless_priming", "2112"),
            ("gapless_remainder", "608"),
            ("gapless_source", "iTunSMPB"),
            ("gapless_valid", "2400"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
    );
}

#[test]
fn he_aac_itunsmpb_counts_scale_to_the_reported_rate() {
    // macOS's HE-AAC encoder: 1 s mono at 44.1 kHz, stored with the
    // 22.05 kHz core rate in the `mp4a` entry. afinfo reads the file at
    // 22050 Hz and reports 22050 valid frames + 2112 priming + 1438
    // remainder (25 frames of 1024 core samples). The demuxer reports
    // the 44.1 kHz SBR output rate, so every count doubles with it.
    let data = include_bytes!("fixtures/apple-heaac-mono.m4a");
    let (rate, flat) = rate_and_flat(data);
    assert_eq!(rate, Some(44_100));
    let t = timing(data).unwrap();
    assert_eq!(
        t,
        GaplessTiming::new(4224, 44_100, 2876, GaplessSource::ITunSmpb)
    );
    assert_eq!(t.total(), 50 * 1024);
    assert_eq!(
        flat[1],
        ("gapless_remainder".to_string(), "2876".to_string())
    );
}

#[test]
fn edit_list_with_roll_group() {
    // elst media_time 1024, segment 0.5 s at 48 kHz; afinfo reports
    // 24000 valid frames + 1024 priming + 576 remainder.
    assert_eq!(
        timing(include_bytes!("fixtures/aac-elst-roll.mp4")),
        Some(GaplessTiming::new(
            1024,
            24000,
            576,
            GaplessSource::EditList
        ))
    );
    let (_, flat) = rate_and_flat(include_bytes!("fixtures/aac-elst-roll.mp4"));
    assert_eq!(flat[2], ("gapless_source".to_string(), "elst".to_string()));
}

#[test]
fn no_carrier_means_none() {
    // A PCM track: no iTunSMPB, no roll group — nothing to report, and
    // no `gapless_*` keys either.
    let data = include_bytes!("fixtures/pcm-mono.mov");
    assert_eq!(timing(data), None);
    let (_, flat) = rate_and_flat(data);
    assert!(flat.is_empty());
}
