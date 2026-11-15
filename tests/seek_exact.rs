//! `seek_to` exactness, checked exhaustively on small self-muxed files:
//! for every target time across (and beyond) the presentation range,
//! the packet the demuxer lands on must be the stream's last sync
//! sample at or before the target — on the presentation timeline, i.e.
//! with composition offsets (ISO/IEC 14496-12 §8.6.1.3) and the edit
//! list (§8.6.6) applied — for sample-table files and for fragmented
//! ones with and without `sidx` / `mfra`.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, StreamInfo, TimeBase,
};
use oxideav_mp4::demux::{EditListEntry, Mp4Demuxer, open_typed};
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

const AUDIO_RATE: u32 = 8000;
const AUDIO_FRAME: i64 = 160;

fn video_stream() -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("mjpeg"));
    params.width = Some(16);
    params.height = Some(16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn audio_stream(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.sample_rate = Some(AUDIO_RATE);
    params.channels = Some(1);
    // AudioSpecificConfig: AAC-LC, 8 kHz (index 11), mono.
    params.extradata = vec![0x15, 0x88];
    StreamInfo {
        index,
        time_base: TimeBase::new(1, AUDIO_RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// Video packets in decode order. `reorder` gives the three-frame
/// groups a composition order different from the decode order (the
/// anchor of each group is decoded first and presented last);
/// `gop` frames per sync sample; `start` shifts every timestamp.
fn video_packets(n: usize, gop: usize, reorder: bool, start: i64) -> Vec<Packet> {
    let tb = TimeBase::new(1, 1000);
    (0..n)
        .map(|i| {
            let dts = i as i64 * 40;
            // Display slot of decode index i: 0, then groups (3,1,2),
            // (6,4,5), ... — plus one frame of delay so pts >= dts.
            let slot = if !reorder || i == 0 {
                i as i64
            } else {
                let g = (i as i64 - 1) / 3;
                let k = (i as i64 - 1) % 3;
                g * 3 + [3, 1, 2][k as usize]
            };
            let pts = if reorder { (slot + 1) * 40 } else { slot * 40 };
            Packet::new(0, tb, vec![i as u8, 0xAB, (i >> 8) as u8, 7])
                .with_pts(pts + start)
                .with_dts(dts + start)
                .with_duration(40)
                .with_keyframe(i % gop == 0)
        })
        .collect()
}

fn audio_packets(stream: u32, n: usize) -> Vec<Packet> {
    let tb = TimeBase::new(1, AUDIO_RATE as i64);
    (0..n)
        .map(|i| {
            let t = i as i64 * AUDIO_FRAME;
            Packet::new(stream, tb, vec![0x40 | i as u8, 0x11, i as u8])
                .with_pts(t)
                .with_dts(t)
                .with_duration(AUDIO_FRAME)
                .with_keyframe(true)
        })
        .collect()
}

/// Interleave by decode time (seconds), stable.
fn interleave(mut a: Vec<Packet>, b: Vec<Packet>) -> Vec<Packet> {
    a.extend(b);
    a.sort_by(|x, y| {
        let t = |p: &Packet| {
            p.dts.unwrap() as f64
                / if p.stream_index == 0 {
                    1000.0
                } else {
                    AUDIO_RATE as f64
                }
        };
        t(x).partial_cmp(&t(y)).unwrap()
    });
    a
}

fn mux(streams: &[StreamInfo], packets: &[Packet], options: Mp4MuxerOptions) -> Vec<u8> {
    let sink = Sink::default();
    let mut m: Box<dyn Muxer> =
        oxideav_mp4::muxer::open_with_options(Box::new(sink.clone()), streams, options).unwrap();
    m.write_header().unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.0.lock().unwrap().get_ref().clone()
}

fn open(bytes: &[u8]) -> Mp4Demuxer {
    open_typed(Box::new(Cursor::new(bytes.to_vec())), &NullCodecResolver).unwrap()
}

fn drain(d: &mut Mp4Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push(p);
    }
    out
}

/// The next packet of `stream`, skipping other streams.
fn next_of(d: &mut Mp4Demuxer, stream: u32) -> Packet {
    loop {
        let p = d.next_packet().unwrap();
        if p.stream_index == stream {
            return p;
        }
    }
}

/// Seek `stream` to every time from before the first packet to after
/// the last and compare the landing packet with a scan of the whole
/// packet list. `pre_roll` is the number of packets of the stream the
/// landing point is moved back by (the audio roll distance).
fn check_every_target(bytes: &[u8], stream: u32, pre_roll: usize) -> usize {
    let mut d = open(bytes);
    let all = drain(&mut d);
    let mine: Vec<&Packet> = all.iter().filter(|p| p.stream_index == stream).collect();
    assert!(!mine.is_empty());
    let lo = mine.iter().map(|p| p.pts.unwrap()).min().unwrap();
    let hi = mine.iter().map(|p| p.pts.unwrap()).max().unwrap();
    let mut distinct = std::collections::BTreeSet::new();
    for target in (lo - 50)..=(hi + 50) {
        // Last sync sample at or before the target (later in decode
        // order on a tie); the first sync sample when there is none.
        let mut want: Option<usize> = None;
        for (i, p) in mine.iter().enumerate() {
            if p.flags.keyframe
                && p.pts.unwrap() <= target
                && want.is_none_or(|w| p.pts.unwrap() >= mine[w].pts.unwrap())
            {
                want = Some(i);
            }
        }
        let want = want.unwrap_or_else(|| mine.iter().position(|p| p.flags.keyframe).unwrap());
        let want = want.saturating_sub(pre_roll);
        let landed = d.seek_to(stream, target).unwrap();
        let got = next_of(&mut d, stream);
        assert_eq!(
            (got.pts, got.dts, &got.data),
            (mine[want].pts, mine[want].dts, &mine[want].data),
            "stream {stream} target {target}"
        );
        assert_eq!(Some(landed), got.pts, "returned time, target {target}");
        assert!(got.flags.keyframe);
        if pre_roll == 0 {
            assert!(landed <= target || want == 0, "target {target}");
        }
        distinct.insert(want);
    }
    distinct.len()
}

fn fragmented(indexes: bool, cadence: u32) -> Mp4MuxerOptions {
    Mp4MuxerOptions::default().with_fragmented(Some(
        FragmentedOptions::default()
            .with_cadence(FragmentCadence::EveryNPackets(cadence))
            .with_emit_random_access_indexes(indexes),
    ))
}

/// Every layout the muxers produce.
fn layouts() -> Vec<(&'static str, Mp4MuxerOptions)> {
    vec![
        ("progressive", Mp4MuxerOptions::default()),
        ("faststart", Mp4MuxerOptions::default().with_faststart(true)),
        ("fragmented + sidx/mfra", fragmented(true, 4)),
        ("fragmented, no index", fragmented(false, 4)),
        // Fragments that do not start on a sync sample and hold more
        // than one of them.
        ("fragmented, long fragments", fragmented(true, 11)),
        ("fragmented, long, no index", fragmented(false, 11)),
    ]
}

#[test]
fn video_without_reordering() {
    let streams = [video_stream()];
    let packets = video_packets(40, 5, false, 0);
    for (name, options) in layouts() {
        let bytes = mux(&streams, &packets, options);
        assert_eq!(check_every_target(&bytes, 0, 0), 8, "{name}");
    }
}

#[test]
fn video_with_composition_offsets() {
    let streams = [video_stream()];
    // Sync samples every 6 decode positions; with reordering their
    // presentation times are not their decode times.
    let packets = video_packets(43, 6, true, 0);
    for (name, options) in layouts() {
        let bytes = mux(&streams, &packets, options);
        let d = open(&bytes);
        assert!(
            d.samples(0)
                .iter()
                .any(|s| s.composition_time != s.decode_time),
            "{name}: offsets were written"
        );
        assert_eq!(check_every_target(&bytes, 0, 0), 8, "{name}");
    }
}

#[test]
fn video_with_a_start_delay() {
    // First presentation time 400 ms: the progressive muxers write an
    // empty edit, and every target is on the delayed timeline.
    let streams = [video_stream()];
    let packets = video_packets(30, 4, false, 400);
    for (name, options) in layouts().into_iter().take(2) {
        let bytes = mux(&streams, &packets, options);
        let mut d = open(&bytes);
        assert!(d.edit_list(0)[0].is_empty_edit(), "{name}");
        assert_eq!(drain(&mut d)[0].pts, Some(400), "{name}");
        assert_eq!(check_every_target(&bytes, 0, 0), 8, "{name}");
    }
}

#[test]
fn video_with_a_trimming_edit_list() {
    // The edit starts at media time 440 ms — inside the second group
    // of pictures — and lasts one second: presentation time zero is
    // media time 440, and the samples before it are decode-only.
    let streams = [video_stream()];
    for reorder in [false, true] {
        let packets = video_packets(40, 5, reorder, 0);
        let edit = TrackEditList::default()
            .with_stream_index(0)
            .with_entries(vec![EditListEntry {
                segment_duration: 1000,
                media_time: 440,
                media_rate_integer: 1,
                media_rate_fraction: 0,
            }]);
        for (name, options) in layouts() {
            let bytes = mux(
                &streams,
                &packets,
                options.with_track_edit_lists(vec![edit.clone()]),
            );
            let mut d = open(&bytes);
            let all = drain(&mut d);
            assert_eq!(all[0].pts.unwrap(), if reorder { -400 } else { -440 });
            assert!(all[0].flags.discard, "{name}");
            // Target zero lands on the sync sample the first
            // presented picture depends on, which is before the edit.
            let landed = d.seek_to(0, 0).unwrap();
            assert!(landed < 0, "{name}: {landed}");
            assert_eq!(
                check_every_target(&bytes, 0, 0),
                8,
                "{name} reorder={reorder}"
            );
        }
    }
}

#[test]
fn two_streams_seek_independently() {
    let streams = [video_stream(), audio_stream(1)];
    let packets = interleave(video_packets(40, 5, true, 0), audio_packets(1, 80));
    for (name, options) in layouts() {
        let bytes = mux(&streams, &packets, options);
        assert_eq!(check_every_target(&bytes, 0, 0), 8, "{name} video");
        assert_eq!(check_every_target(&bytes, 1, 0), 80, "{name} audio");
    }
}

#[test]
fn audio_with_priming_backs_up_by_the_roll_distance() {
    // 2.2 frames of priming, a roll group at distance -1: a seek lands
    // one packet before the packet holding the target, on the
    // edit-list timeline (the first presented sample is time zero).
    let streams = [audio_stream(0)];
    let packets = audio_packets(0, 60);
    let gapless = TrackGapless::new(0, 352, 100);
    for (name, options) in layouts() {
        let fragmented = options.fragmented.is_some();
        let directive = if fragmented {
            gapless.clone().with_valid(Some(60 * 160 - 452))
        } else {
            gapless.clone()
        };
        let bytes = mux(
            &streams,
            &packets,
            options
                .with_movie_timescale(AUDIO_RATE)
                .with_track_gapless(vec![directive]),
        );
        let mut d = open(&bytes);
        assert_eq!(d.audio_roll_distance(0), Some(-1), "{name}");
        let all = drain(&mut d);
        assert_eq!(all[0].pts, Some(-352), "{name}");
        // Time zero is in the third packet (media 320..480): decoding
        // starts at the second.
        assert_eq!(d.seek_to(0, 0).unwrap(), -192, "{name}");
        assert_eq!(next_of(&mut d, 0).data, all[1].data, "{name}");
        // A target inside packet 10 (presentation 1248..1408).
        assert_eq!(d.seek_to(0, 1300).unwrap(), 1088, "{name}");
        assert_eq!(next_of(&mut d, 0).data, all[9].data, "{name}");
        assert_eq!(check_every_target(&bytes, 0, 1), 59, "{name}");
    }
}

#[test]
fn audio_with_a_longer_roll_distance() {
    let streams = [audio_stream(0)];
    let packets = audio_packets(0, 30);
    let groups = TrackSampleGroups::default()
        .with_stream_index(0)
        .with_sgpd(vec![SampleGroupDescription {
            grouping_type: *b"roll",
            default_sample_description_index: None,
            entries: vec![(-3i16).to_be_bytes().to_vec()],
        }])
        .with_sbgp(vec![SampleToGroup {
            grouping_type: *b"roll",
            grouping_type_parameter: None,
            entries: vec![(30, 1)],
        }]);
    for (name, options) in layouts().into_iter().take(2) {
        let bytes = mux(
            &streams,
            &packets,
            options.with_track_sample_groups(vec![groups.clone()]),
        );
        assert_eq!(check_every_target(&bytes, 0, 3), 27, "{name}");
    }
}

#[test]
fn seek_errors() {
    let streams = [video_stream()];
    let bytes = mux(
        &streams,
        &video_packets(5, 5, false, 0),
        Mp4MuxerOptions::default(),
    );
    let mut d = open(&bytes);
    assert!(d.seek_to(1, 0).is_err(), "no such stream");
    // Seeking is repeatable and leaves the other calls working.
    assert_eq!(d.seek_to(0, i64::MAX).unwrap(), 0);
    assert_eq!(d.seek_to(0, i64::MIN).unwrap(), 0);
    assert_eq!(drain(&mut d).len(), 5);
}

#[test]
fn read_block_size_does_not_change_what_is_read() {
    // The read-ahead block is an I/O detail: whatever its size —
    // off, smaller than a sample, a few samples, the default — the
    // packets and the positions seeks land on are the same.
    let streams = [video_stream(), audio_stream(1)];
    let packets = interleave(video_packets(40, 5, true, 0), audio_packets(1, 80));
    for (name, options) in layouts() {
        let bytes = mux(&streams, &packets, options);
        let mut reference = open(&bytes);
        reference.set_read_block_size(0);
        let want = drain(&mut reference);
        assert_eq!(want.len(), 120, "{name}");
        for block in [1usize, 3, 4, 5, 9, 64, 1 << 16] {
            let mut d = open(&bytes);
            d.set_read_block_size(block);
            let got = drain(&mut d);
            assert_eq!(got.len(), want.len(), "{name} block {block}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(
                    (g.stream_index, g.pts, g.dts, &g.data),
                    (w.stream_index, w.pts, w.dts, &w.data),
                    "{name} block {block}"
                );
            }
            // Backwards and forwards again through the held block.
            for target in [900, 0, 1500, 40, 1499] {
                d.seek_to(0, target).unwrap();
                reference.seek_to(0, target).unwrap();
                for _ in 0..7 {
                    let (g, w) = (d.next_packet().unwrap(), reference.next_packet().unwrap());
                    assert_eq!((g.pts, &g.data), (w.pts, &w.data), "{name} block {block}");
                }
            }
        }
    }
}
