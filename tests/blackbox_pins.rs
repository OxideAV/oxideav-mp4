//! Demuxer behaviours pinned on small files written by a black-box
//! muxer (`tests/fixtures/blackbox/gen.sh`). The packet tables are what
//! `ffprobe -show_packets -show_data_hash adler32` reports for each
//! file; where this crate deliberately differs from that reader the
//! test says so.

use oxideav_core::{Demuxer, Packet, RuntimeContext, SampleFormat};
use oxideav_mp4::demux::Mp4Demuxer;

fn open(name: &str) -> Mp4Demuxer {
    let path = format!(
        "{}/tests/fixtures/blackbox/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(&path).unwrap();
    oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap()
}

fn packets(d: &mut Mp4Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p),
            Err(e) if e.is_eof() => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// `(stream, pts, dts, duration, size, key, adler32)`.
type Row = (u32, i64, i64, i64, usize, bool, u32);

fn rows(d: &mut Mp4Demuxer) -> Vec<Row> {
    packets(d)
        .iter()
        .map(|p| {
            (
                p.stream_index,
                p.pts.unwrap(),
                p.dts.unwrap(),
                p.duration.unwrap(),
                p.data.len(),
                p.flags.keyframe,
                adler32(&p.data),
            )
        })
        .collect()
}

const FRAG_INHERITED_BASE: &[Row] = &[
    (0, 0, 0, 810, 1225, true, 0x8ee5b694),
    (0, 810, 810, 512, 9, false, 0x04ff01ac),
    (0, 1322, 1322, 512, 9, false, 0x0564019f),
    (1, 0, 0, 1024, 67, true, 0xed791dc1),
    (1, 1024, 1024, 1024, 87, true, 0xfef12ce5),
    (1, 2048, 2048, 1024, 52, true, 0xd6861b20),
    (1, 3072, 3072, 1024, 48, true, 0x72d118e7),
    (1, 4096, 4096, 1024, 50, true, 0xaa281adc),
    (1, 5120, 5120, 1024, 50, true, 0xcb231b74),
    (1, 6144, 6144, 1024, 47, true, 0x7eb11a9d),
    (0, 1834, 1834, 512, 531, true, 0x2cd9dcba),
    (0, 2346, 2346, 512, 9, false, 0x0544019f),
    (0, 2858, 2858, 512, 50, false, 0x2132170e),
    (1, 7168, 7168, 1024, 50, true, 0x7d0118cc),
    (1, 8192, 8192, 1024, 49, true, 0xdc4d1acc),
    (1, 9216, 9216, 1024, 53, true, 0x01951cea),
    (1, 10240, 10240, 1024, 89, true, 0x27c2335c),
    (1, 11264, 11264, 344, 5, true, 0x03c501bf),
];

/// ISO/IEC 14496-12 §8.8.7.1: without `base-data-offset-present` and
/// without `default-base-is-moof`, the data of the second track
/// fragment of a movie fragment starts where the first one's ends.
#[test]
fn second_track_fragment_inherits_its_base_offset() {
    let mut d = open("frag-inherited-base.mp4");
    assert_eq!(rows(&mut d), FRAG_INHERITED_BASE);
}

/// The `dOps` box is rebuilt into the RFC 7845 §5.1 identification
/// header: version 1 and little-endian pre-skip / rate / gain.
#[test]
fn opus_extradata_is_the_identification_header() {
    let d = open("opus.mp4");
    let p = &d.streams()[0].params;
    assert_eq!(p.codec_id.as_str(), "opus");
    assert_eq!(
        p.extradata,
        [
            b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 1, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0,
            0, 0
        ]
    );
    // The edit list trims the 312-sample pre-skip; the packet it cuts
    // into is presented and the stream starts where the edit does.
    assert_eq!(d.streams()[0].start_time, Some(0));
}

/// The 16.16 `samplerate` field cannot hold 96 kHz; the codec
/// configuration states the rate.
#[test]
fn rates_above_the_sample_entry_field() {
    let d = open("aac-96k.mp4");
    let p = &d.streams()[0].params;
    assert_eq!((p.codec_id.as_str(), p.sample_rate), ("aac", Some(96_000)));

    let d = open("flac-96k-32bit.mp4");
    let p = &d.streams()[0].params;
    assert_eq!((p.codec_id.as_str(), p.sample_rate), ("flac", Some(96_000)));
    assert_eq!(p.channels, Some(1));
    assert_eq!(p.sample_format, Some(SampleFormat::S32));
}

const PCM_S24LE_MOV: &[Row] = &[
    (0, 0, 0, 1024, 3072, true, 0xab17e6ac),
    (0, 1024, 1024, 176, 528, true, 0xe29bc130),
];

/// Uncompressed audio has one table sample per audio frame; the frames
/// of a chunk are delivered together, 1024 to a packet.
#[test]
fn pcm_frames_are_delivered_in_packets() {
    let mut d = open("pcm-s24le.mov");
    assert_eq!(d.streams()[0].params.codec_id.as_str(), "pcm_s24le");
    assert_eq!(rows(&mut d), PCM_S24LE_MOV);

    // Big-endian samples (`ipcm` with a big-endian `pcmC`) are
    // delivered little-endian, so only the payload differs from what a
    // reader that hands them over as stored reports.
    let mut d = open("pcm-s16be.mp4");
    assert_eq!(d.streams()[0].params.codec_id.as_str(), "pcm_s16le");
    let got = rows(&mut d);
    let shape: Vec<(i64, i64, usize)> = got.iter().map(|r| (r.1, r.3, r.4)).collect();
    assert_eq!(shape, [(0, 1024, 2048), (1024, 176, 352)]);
    let mut d = open("pcm-s16be.mp4");
    let first = &packets(&mut d)[0].data;
    let stored: Vec<u8> = first.chunks(2).flat_map(|s| [s[1], s[0]]).collect();
    assert_eq!(adler32(&stored), 0x0234e6ac);
}

const SMOOTH: &[Row] = &[
    (0, 0, 0, 400000, 1301, true, 0xdc33cfcb),
    (0, 1200000, 400000, 400000, 10, false, 0x070b023e),
    (0, 400000, 800000, 400000, 10, false, 0x07a40205),
    (0, 800000, 1200000, 400000, 10, false, 0x064001a7),
    (0, 2000000, 1600000, 400000, 55, false, 0x192e1578),
    (0, 1600000, 2000000, 400000, 11, false, 0x0b89033c),
];

/// Smooth Streaming files: the media header's duration is all ones
/// ("unknown", §8.4.2.3) and the samples are all in fragments, whose
/// `trun` boxes carry signed composition offsets (§8.8.8).
#[test]
fn smooth_streaming_duration_and_signed_offsets() {
    let mut d = open("smooth.ismv");
    let s = &d.streams()[0];
    assert_eq!(s.duration, Some(6 * 400_000));
    assert_eq!(s.start_time, Some(0));
    // CT = DT + offset (§8.6.1.3): the first frame is presented at 0.
    // (The reader the table comes from shifts every pts by the largest
    // negative offset; the table has that shift taken out.)
    assert_eq!(rows(&mut d), SMOOTH);
}

const NO_EDIT_LIST: &[Row] = &[
    (0, 1024, 0, 512, 1301, true, 0xdc33cfcb),
    (0, 2560, 512, 512, 10, false, 0x070b023e),
    (0, 1536, 1024, 512, 10, false, 0x07a40205),
    (0, 2048, 1536, 512, 10, false, 0x064001a7),
    (0, 3584, 2048, 512, 55, false, 0x192e1578),
    (0, 3072, 2560, 512, 11, false, 0x0b89033c),
];

/// B-frames and no edit list: the first presented frame is at the
/// first composition offset, and that is where the stream starts.
#[test]
fn start_time_is_the_first_presented_timestamp() {
    let mut d = open("no-edit-list.mp4");
    assert_eq!(d.streams()[0].start_time, Some(1024));
    assert_eq!(rows(&mut d), NO_EDIT_LIST);
}
