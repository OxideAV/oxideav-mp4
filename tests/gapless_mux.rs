//! Gapless audio timing written by the muxers (`TrackGapless`) and
//! reconciled by the demuxer (`Mp4Demuxer::gapless_timing`): the edit
//! list + `roll` group carriage (ISO/IEC 14496-12 §8.6.6 / §10.1), the
//! `iTunSMPB` freeform item, files that carry only one of them, and
//! files whose carriages contradict each other.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, StreamInfo, TimeBase,
};
use oxideav_mp4::demux::{EditListEntry, GaplessSource, GaplessTiming, Mp4Demuxer, open_typed};
use oxideav_mp4::sample_groups::{SampleGroupDescription, SampleToGroup};
use oxideav_mp4::{
    FragmentCadence, FragmentedOptions, Mp4MuxerOptions, TrackEditList, TrackGapless,
    TrackSampleGroups,
};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

const RATE: u32 = 44_100;
const FRAME: u64 = 1024;
/// AAC-style encoder delay and the padding of a 7628-sample signal in
/// ten 1024-sample packets.
const PRIMING: u64 = 2112;
const PACKETS: u64 = 10;
const VALID: u64 = 7628;
const REMAINDER: u64 = PACKETS * FRAME - PRIMING - VALID; // 500

fn aac_stream() -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.sample_rate = Some(RATE);
    params.channels = Some(2);
    // AudioSpecificConfig: AAC-LC, 44.1 kHz, stereo.
    params.extradata = vec![0x12, 0x10];
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Mux `PACKETS` 1024-sample packets starting at pts 0.
fn mux(options: Mp4MuxerOptions) -> Vec<u8> {
    try_mux(options).unwrap()
}

fn try_mux(options: Mp4MuxerOptions) -> oxideav_core::Result<Vec<u8>> {
    let stream = aac_stream();
    let sink = Sink::default();
    let mut m: Box<dyn Muxer> = oxideav_mp4::muxer::open_with_options(
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
        options,
    )?;
    m.write_header()?;
    for i in 0..PACKETS {
        let data: Vec<u8> = (0..40).map(|j| (i * 7 + j) as u8).collect();
        let pts = (i * FRAME) as i64;
        let p = Packet::new(0, stream.time_base, data)
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(FRAME as i64)
            .with_keyframe(true);
        m.write_packet(&p)?;
    }
    m.write_trailer()?;
    drop(m);
    Ok(sink.0.lock().unwrap().get_ref().clone())
}

fn open(bytes: &[u8]) -> Mp4Demuxer {
    open_typed(Box::new(Cursor::new(bytes.to_vec())), &NullCodecResolver).unwrap()
}

fn fragmented() -> Mp4MuxerOptions {
    Mp4MuxerOptions::default().with_fragmented(Some(
        FragmentedOptions::default().with_cadence(FragmentCadence::EveryNPackets(3)),
    ))
}

/// `(pts, discard)` of every packet.
fn timeline(d: &mut Mp4Demuxer) -> Vec<(i64, bool)> {
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push((p.pts.unwrap(), p.flags.discard));
    }
    out
}

fn option(d: &Mp4Demuxer, key: &str) -> Option<String> {
    d.streams()[0].params.options.get(key).map(str::to_string)
}

#[test]
fn both_carriages_report_the_exact_counts() {
    let bytes = mux(Mp4MuxerOptions::default()
        .with_track_gapless(vec![TrackGapless::new(0, PRIMING, REMAINDER)]));
    let mut d = open(&bytes);
    // The edit list alone is exact only to a millisecond (movie
    // timescale 1000); it agrees with the item within that tick, so
    // the item's sample-exact numbers are the result.
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::ITunSmpb
        ))
    );
    assert_eq!(d.audio_roll_distance(0), Some(-1));
    assert_eq!(option(&d, "gapless_priming").as_deref(), Some("2112"));
    assert_eq!(option(&d, "gapless_valid").as_deref(), Some("7628"));
    assert_eq!(option(&d, "gapless_remainder").as_deref(), Some("500"));
    assert_eq!(option(&d, "gapless_conflict"), None);
    assert_eq!(option(&d, "audio_roll_distance").as_deref(), Some("-1"));
    // The edit list is on the packet timeline: presentation time zero
    // is media time 2112, the two packets wholly inside the priming
    // are decode-only, and the packet the edit cuts into is presented.
    let t = timeline(&mut d);
    assert_eq!(t.len(), PACKETS as usize);
    assert_eq!(t[0], (-2112, true));
    assert_eq!(t[1], (-1088, true));
    assert_eq!(t[2], (-64, false));
    assert_eq!(t[3], (960, false));
    assert!(t[3..].iter().all(|&(_, discard)| !discard));
    // The item is written in the twelve-field layout it is read from.
    let item = d
        .metadata()
        .iter()
        .find(|(k, _)| k == "----:com.apple.iTunes:iTunSMPB")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(
        item,
        "00000000 00000840 000001F4 0000000000001DCC 00000000 00000000 00000000 00000000 \
         00000000 00000000 00000000 00000000"
    );
}

#[test]
fn item_layout_matches_the_reported_file() {
    // The fixture's own item, regenerated from its three counts.
    let d = open(include_bytes!("fixtures/apple-aac-20000.m4a"));
    let item = d
        .metadata()
        .iter()
        .find(|(k, _)| k == "----:com.apple.iTunes:iTunSMPB")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(
        oxideav_mp4::demux::format_itunsmpb(2112, 416, 20000).trim(),
        item
    );
}

#[test]
fn edit_list_only_is_exact_at_a_sample_rate_movie_timescale() {
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_gapless(vec![
            TrackGapless::new(0, PRIMING, REMAINDER).with_write_itunsmpb(false),
        ]));
    let d = open(&bytes);
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::EditList
        ))
    );
    assert!(
        d.metadata().iter().all(|(k, _)| !k.contains("iTunSMPB")),
        "no item was asked for"
    );
    assert_eq!(option(&d, "gapless_source").as_deref(), Some("elst"));
}

#[test]
fn edit_list_only_at_the_default_movie_timescale_rounds_to_a_tick() {
    let bytes = mux(Mp4MuxerOptions::default().with_track_gapless(vec![
        TrackGapless::new(0, PRIMING, REMAINDER).with_write_itunsmpb(false),
    ]));
    let g = open(&bytes).gapless_timing(0).unwrap();
    // 7628 samples = 172.97 ms: the segment is written as 172 ms
    // (rounded down so it never reaches into the padding), which reads
    // back as 7585 samples; the rest of the track is remainder.
    assert_eq!(g.priming, PRIMING);
    assert_eq!(g.valid, 172 * u64::from(RATE) / 1000);
    assert_eq!(g.total(), PACKETS * FRAME);
    assert_eq!(g.source, GaplessSource::EditList);
    assert!(VALID - g.valid < u64::from(RATE).div_ceil(1000));
}

#[test]
fn item_only_leaves_the_packet_timeline_alone() {
    let bytes = mux(Mp4MuxerOptions::default().with_track_gapless(vec![
        TrackGapless::new(0, PRIMING, REMAINDER).with_write_edit_list(false),
    ]));
    let mut d = open(&bytes);
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::ITunSmpb
        ))
    );
    assert_eq!(d.audio_roll_distance(0), None);
    assert!(d.edit_list(0).is_empty());
    let t = timeline(&mut d);
    assert_eq!(t[0], (0, false));
    assert!(t.iter().all(|&(_, discard)| !discard));
}

#[test]
fn explicit_valid_length_is_written_as_given() {
    // A caller that knows the signal length states it; the remainder
    // then only fills the item's padding field.
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_gapless(vec![
            TrackGapless::new(0, PRIMING, 400).with_valid(Some(7000)),
        ]));
    let d = open(&bytes);
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            7000,
            400,
            GaplessSource::ITunSmpb
        ))
    );
}

#[test]
fn no_directive_no_carriage() {
    let d = open(&mux(Mp4MuxerOptions::default()));
    assert_eq!(d.gapless_timing(0), None);
    assert_eq!(d.audio_roll_distance(0), None);
    assert!(
        d.streams()[0]
            .params
            .options
            .iter()
            .all(|(k, _)| !k.starts_with("gapless_"))
    );
}

fn roll_groups(distance: i16) -> TrackSampleGroups {
    TrackSampleGroups::default()
        .with_stream_index(0)
        .with_sgpd(vec![SampleGroupDescription {
            grouping_type: *b"roll",
            default_sample_description_index: None,
            entries: vec![distance.to_be_bytes().to_vec()],
        }])
        .with_sbgp(vec![SampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            entries: vec![(PACKETS as u32, 1)],
        }])
}

fn edit(media_time: i64, valid: u64) -> TrackEditList {
    TrackEditList::default()
        .with_stream_index(0)
        .with_entries(vec![EditListEntry {
            segment_duration: valid,
            media_time,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        }])
}

/// An item written from `TrackGapless`, an edit list written by hand:
/// the two can be made to disagree.
fn contradictory(media_time: i64, valid: u64, signalled: bool) -> Vec<u8> {
    let mut o = Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_gapless(vec![
            TrackGapless::new(0, PRIMING, REMAINDER).with_write_edit_list(false),
        ])
        .with_track_edit_lists(vec![edit(media_time, valid)]);
    if signalled {
        o = o.with_track_sample_groups(vec![roll_groups(-1)]);
    }
    mux(o)
}

#[test]
fn contradiction_signalled_edit_list_wins() {
    // The item says 2112 / 7628 / 500; the edit list, with its roll
    // group, says one packet of priming and 8000 valid samples.
    let d = open(&contradictory(1024, 8000, true));
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(1024, 8000, 1216, GaplessSource::EditList).with_conflict(true))
    );
    assert_eq!(option(&d, "gapless_conflict").as_deref(), Some("1"));
    assert_eq!(option(&d, "gapless_source").as_deref(), Some("elst"));
}

#[test]
fn contradiction_plain_edit_list_loses_to_the_item() {
    let d = open(&contradictory(1024, 8000, false));
    assert_eq!(
        d.gapless_timing(0),
        Some(
            GaplessTiming::new(PRIMING, VALID, REMAINDER, GaplessSource::ITunSmpb)
                .with_conflict(true)
        )
    );
}

#[test]
fn plain_whole_track_edit_list_is_no_contradiction() {
    // Media time zero for the whole track, no roll group: the edit
    // list says nothing about encoder delay, the item stands alone.
    let d = open(&contradictory(0, PACKETS * FRAME, false));
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::ITunSmpb
        ))
    );
}

#[test]
fn contradiction_overrunning_edit_list_loses() {
    // A signalled edit list describing more samples than the track
    // holds cannot be right; the item, which fits, is reported.
    let d = open(&contradictory(1024, 20_000, true));
    assert_eq!(
        d.gapless_timing(0),
        Some(
            GaplessTiming::new(PRIMING, VALID, REMAINDER, GaplessSource::ITunSmpb)
                .with_conflict(true)
        )
    );
}

#[test]
fn agreeing_carriages_written_separately() {
    let d = open(&contradictory(PRIMING as i64, VALID, true));
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::ITunSmpb
        ))
    );
}

#[test]
fn plain_trimming_edit_list_alone() {
    // No item, no roll group: §8.6.6 still says the presentation
    // starts at media time 1024 and lasts 8000 samples.
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_edit_lists(vec![edit(1024, 8000)]));
    let d = open(&bytes);
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            1024,
            8000,
            1216,
            GaplessSource::EditListTrim
        ))
    );
    assert_eq!(option(&d, "gapless_source").as_deref(), Some("elst-trim"));
    // A whole-track edit list trims nothing and is not a carriage.
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_edit_lists(vec![edit(0, PACKETS * FRAME)]));
    assert_eq!(open(&bytes).gapless_timing(0), None);
}

#[test]
fn prol_group_signals_the_edit_list_too() {
    let groups = TrackSampleGroups::default()
        .with_stream_index(0)
        .with_sgpd(vec![SampleGroupDescription {
            grouping_type: *b"prol",
            default_sample_description_index: None,
            entries: vec![(-2i16).to_be_bytes().to_vec()],
        }])
        .with_sbgp(vec![SampleToGroup {
            grouping_type: *b"prol",
            grouping_type_parameter: None,
            entries: vec![(PACKETS as u32, 1)],
        }]);
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_edit_lists(vec![edit(PRIMING as i64, VALID)])
        .with_track_sample_groups(vec![groups]));
    let d = open(&bytes);
    assert_eq!(d.audio_roll_distance(0), Some(-2));
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::EditList
        ))
    );
}

#[test]
fn leading_empty_edit_does_not_hide_the_trim() {
    // A start delay (empty edit) ahead of the media edit.
    let entries = vec![
        EditListEntry {
            segment_duration: 4410,
            media_time: -1,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        },
        EditListEntry {
            segment_duration: VALID,
            media_time: PRIMING as i64,
            media_rate_integer: 1,
            media_rate_fraction: 0,
        },
    ];
    let bytes = mux(Mp4MuxerOptions::default()
        .with_movie_timescale(RATE)
        .with_track_edit_lists(vec![
            TrackEditList::default()
                .with_stream_index(0)
                .with_entries(entries),
        ])
        .with_track_sample_groups(vec![roll_groups(-1)]));
    assert_eq!(
        open(&bytes).gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::EditList
        ))
    );
}

#[test]
fn negative_first_pts_writes_a_trimming_edit_list() {
    // No directive: packets timestamped from -2112 make the muxer
    // write the pre-roll edit on its own; it reads back as a trim.
    let stream = aac_stream();
    let sink = Sink::default();
    let mut m = oxideav_mp4::muxer::open_with_options(
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
        Mp4MuxerOptions::default().with_movie_timescale(RATE),
    )
    .unwrap();
    m.write_header().unwrap();
    for i in 0..PACKETS {
        let pts = (i * FRAME) as i64 - PRIMING as i64;
        // The last packet declares only its valid part.
        let dur = if i == PACKETS - 1 {
            FRAME - REMAINDER
        } else {
            FRAME
        };
        let p = Packet::new(0, stream.time_base, vec![i as u8; 30])
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(dur as i64)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let g = open(&bytes).gapless_timing(0).unwrap();
    assert_eq!(g.source, GaplessSource::EditListTrim);
    assert_eq!((g.priming, g.valid), (PRIMING, VALID));
}

#[test]
fn fragmented_with_a_stated_valid_length() {
    let bytes = mux(fragmented()
        .with_movie_timescale(RATE)
        .with_track_gapless(vec![
            TrackGapless::new(0, PRIMING, REMAINDER).with_valid(Some(VALID)),
        ]));
    let mut d = open(&bytes);
    assert!(d.is_fragmented());
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::ITunSmpb
        ))
    );
    assert_eq!(d.audio_roll_distance(0), Some(-1));
    let t = timeline(&mut d);
    assert_eq!(t.len(), PACKETS as usize);
    assert_eq!(&t[..3], &[(-2112, true), (-1088, true), (-64, false)]);
}

#[test]
fn fragmented_without_a_valid_length_is_filled_in_at_the_trailer() {
    // The init segment precedes every sample: with no stated length
    // the edit is written open-ended and the item zeroed, and
    // `write_trailer` fills both in from the finished track.
    for movie_timescale in [1000, RATE] {
        let bytes = mux(fragmented()
            .with_movie_timescale(movie_timescale)
            .with_track_gapless(vec![TrackGapless::new(0, PRIMING, REMAINDER)]));
        let mut d = open(&bytes);
        assert!(d.is_fragmented());
        assert_eq!(
            d.edit_list(0)[0].segment_duration,
            VALID * u64::from(movie_timescale) / u64::from(RATE)
        );
        assert_eq!(
            d.gapless_timing(0),
            Some(GaplessTiming::new(
                PRIMING,
                VALID,
                REMAINDER,
                GaplessSource::ITunSmpb
            ))
        );
        assert_eq!(timeline(&mut d).len(), PACKETS as usize);
    }
    // The edit list on its own is filled in the same way.
    let bytes = mux(fragmented()
        .with_movie_timescale(RATE)
        .with_track_gapless(vec![
            TrackGapless::new(0, PRIMING, REMAINDER).with_write_itunsmpb(false),
        ]));
    assert_eq!(
        open(&bytes).gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::EditList
        ))
    );
}

#[test]
fn fragmented_stream_cut_before_the_trailer_stays_open_ended() {
    // A recording that never reaches `write_trailer`: the edit is the
    // §8.6.6.1 open-ended form, the zeroed item is not a carriage, and
    // the valid length runs to the end of the fragments written.
    let stream = aac_stream();
    let sink = Sink::default();
    let mut m = oxideav_mp4::muxer::open_with_options(
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
        fragmented().with_track_gapless(vec![TrackGapless::new(0, PRIMING, REMAINDER)]),
    )
    .unwrap();
    m.write_header().unwrap();
    for i in 0..PACKETS {
        let pts = (i * FRAME) as i64;
        let p = Packet::new(0, stream.time_base, vec![i as u8; 40])
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(FRAME as i64)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
    }
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let mut d = open(&bytes);
    // Three whole fragments of three packets made it out.
    let written = timeline(&mut d).len() as u64;
    assert_eq!(written, 9);
    assert_eq!(d.edit_list(0)[0].segment_duration, 0);
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            written * FRAME - PRIMING,
            0,
            GaplessSource::EditList
        ))
    );
}

#[test]
fn fragmented_track_shorter_than_its_priming_fails_at_the_trailer() {
    let options = fragmented().with_track_gapless(vec![TrackGapless::new(0, PACKETS * FRAME, 1)]);
    assert!(try_mux(options).is_err());
}

#[test]
fn directives_that_cannot_be_written_are_refused() {
    let refused = |o: Mp4MuxerOptions| try_mux(o).is_err();
    // Stream slot out of range.
    assert!(refused(
        Mp4MuxerOptions::default().with_track_gapless(vec![TrackGapless::new(3, 1, 1)])
    ));
    // Two directives for one stream.
    assert!(refused(Mp4MuxerOptions::default().with_track_gapless(
        vec![
            TrackGapless::new(0, 1, 1),
            TrackGapless::new(0, 2, 2).with_write_itunsmpb(false),
        ]
    )));
    // An explicit edit list and a gapless one for the same stream.
    assert!(refused(
        Mp4MuxerOptions::default()
            .with_track_edit_lists(vec![edit(0, 100)])
            .with_track_gapless(vec![TrackGapless::new(0, 1, 1)])
    ));
    // More priming + padding than the track has samples.
    assert!(refused(Mp4MuxerOptions::default().with_track_gapless(
        vec![TrackGapless::new(0, PACKETS * FRAME, 1)]
    )));
    // A stated valid length that overruns the track.
    assert!(refused(Mp4MuxerOptions::default().with_track_gapless(
        vec![TrackGapless::new(0, PRIMING, 0).with_valid(Some(PACKETS * FRAME)),]
    )));
    // A zero movie timescale.
    assert!(refused(Mp4MuxerOptions::default().with_movie_timescale(0)));
}

#[test]
fn movie_timescale_reaches_every_duration_field() {
    let bytes = mux(Mp4MuxerOptions::default().with_movie_timescale(RATE));
    let d = open(&bytes);
    assert_eq!(d.movie_header().timescale, RATE);
    assert_eq!(d.movie_header().duration, PACKETS * FRAME);
    assert_eq!(d.duration_micros(), Some(232_199));
}

#[test]
fn pcm_with_a_start_delay_and_a_short_last_packet_is_not_gapless() {
    // Found by the mux_roundtrip fuzz target: uncompressed audio has
    // no frames to pad, so a shorter final packet is just a shorter
    // packet — the whole-track edit behind the start delay trims
    // nothing and no timing is reported.
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.sample_rate = Some(8000);
    params.channels = Some(1);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 8000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let sink = Sink::default();
    let mut m = oxideav_mp4::muxer::open_with_options(
        Box::new(sink.clone()),
        std::slice::from_ref(&stream),
        Mp4MuxerOptions::default(),
    )
    .unwrap();
    m.write_header().unwrap();
    let mut pts = 80;
    for frames in [50i64, 10] {
        let p = Packet::new(0, stream.time_base, vec![0x5A; frames as usize * 2])
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(frames)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
        pts += frames;
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let d = open(&bytes);
    assert!(d.edit_list(0)[0].is_empty_edit());
    assert_eq!(d.gapless_timing(0), None);
}

/// `(offset, size)` of the first child box of this type in `data[from..to]`.
fn child(data: &[u8], from: usize, to: usize, name: &[u8; 4]) -> (usize, usize) {
    let mut at = from;
    while at + 8 <= to {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if &data[at + 4..at + 8] == name {
            return (at, size);
        }
        assert!(size >= 8);
        at += size;
    }
    panic!("no {}", String::from_utf8_lossy(name));
}

#[test]
fn roll_group_declared_in_a_movie_fragment_signals_the_edit_list() {
    // A fragmented file whose init segment has the trimming edit but
    // no sample group description: on its own a plain trim.
    let plain = mux(fragmented()
        .with_movie_timescale(RATE)
        .with_track_edit_lists(vec![edit(PRIMING as i64, VALID)]));
    let d = open(&plain);
    assert_eq!(d.audio_roll_distance(0), None);
    assert_eq!(
        d.gapless_timing(0).map(|g| g.source),
        Some(GaplessSource::EditListTrim)
    );

    // The same file with a `roll` description (distance -1, §10.1)
    // appended to the first track fragment: the moof and traf grow by
    // the box, and the trun's data offset (relative to the moof) with
    // them.
    let mut sgpd = Vec::new();
    sgpd.extend_from_slice(&26u32.to_be_bytes());
    sgpd.extend_from_slice(b"sgpd");
    sgpd.extend_from_slice(&[1, 0, 0, 0]); // version 1
    sgpd.extend_from_slice(b"roll");
    sgpd.extend_from_slice(&2u32.to_be_bytes()); // default_length
    sgpd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    sgpd.extend_from_slice(&(-1i16).to_be_bytes());
    assert_eq!(sgpd.len(), 26);

    let (moof_at, moof_size) = child(&plain, 0, plain.len(), b"moof");
    let (traf_at, traf_size) = child(&plain, moof_at + 8, moof_at + moof_size, b"traf");
    let (trun_at, _) = child(&plain, traf_at + 8, traf_at + traf_size, b"trun");
    assert_eq!(plain[trun_at + 11] & 1, 1, "trun carries a data offset");
    let mut grown = plain.clone();
    let bump = |b: &mut [u8], at: usize| {
        let v = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) + 26;
        b[at..at + 4].copy_from_slice(&v.to_be_bytes());
    };
    bump(&mut grown, moof_at);
    bump(&mut grown, traf_at);
    bump(&mut grown, trun_at + 16); // version/flags, sample_count, data_offset
    let end_of_traf = traf_at + traf_size;
    grown.splice(end_of_traf..end_of_traf, sgpd);

    let mut d = open(&grown);
    assert_eq!(d.audio_roll_distance(0), Some(-1));
    assert_eq!(
        d.gapless_timing(0),
        Some(GaplessTiming::new(
            PRIMING,
            VALID,
            REMAINDER,
            GaplessSource::EditList
        ))
    );
    // Every payload is still where the tables say.
    let mut reference = open(&plain);
    loop {
        match (d.next_packet(), reference.next_packet()) {
            (Ok(a), Ok(b)) => assert_eq!(a.data, b.data),
            (Err(_), Err(_)) => break,
            _ => panic!("packet counts differ"),
        }
    }
}
