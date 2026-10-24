//! A chunk is a contiguous run of one track's samples (ISO/IEC 14496-12
//! §8.7.4): interleaved tracks get one chunk per run, and
//! `ChunkPolicy::PacketBoundaries` ends a chunk at each `unit_boundary`.

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mp4::{ChunkPolicy, Mp4MuxerOptions};

fn boxes<'a>(data: &'a [u8], name: &str) -> Vec<&'a [u8]> {
    // every box of this type, at any depth inside the containers below
    const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if size < 8 || at + size > data.len() {
            break;
        }
        let kind = &data[at + 4..at + 8];
        if kind == name.as_bytes() {
            out.push(&data[at + 8..at + size]);
        } else if CONTAINERS.iter().any(|c| &c[..] == kind) {
            out.extend(boxes(&data[at + 8..at + size], name));
        }
        at += size;
    }
    out
}

fn u32s(b: &[u8]) -> Vec<u32> {
    b.chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

fn streams() -> Vec<StreamInfo> {
    // compressed audio: the sample-count policy groups up to 50 per chunk
    let mut a = CodecParameters::audio(CodecId::new("aac"));
    a.channels = Some(1);
    a.sample_rate = Some(8000);
    let mut v = CodecParameters::video(CodecId::new("mjpeg"));
    v.width = Some(16);
    v.height = Some(16);
    vec![
        StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 8000),
            duration: None,
            start_time: Some(0),
            params: a,
        },
        StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 600),
            duration: None,
            start_time: Some(0),
            params: v,
        },
    ]
}

/// (stream, unit_boundary) per packet, 10 bytes each.
fn mux(name: &str, order: &[(u32, bool)], chunking: ChunkPolicy) -> Vec<u8> {
    let streams = streams();
    let path = std::env::temp_dir().join(format!("oxideav-mp4-chunks-{name}.mp4"));
    {
        let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let options = Mp4MuxerOptions {
            chunking,
            ..Mp4MuxerOptions::default()
        };
        let mut mux = oxideav_mp4::muxer::open_with_options(out, &streams, options).unwrap();
        mux.write_header().unwrap();
        let mut n = [0i64; 2];
        for &(s, boundary) in order {
            let mut p = Packet::new(s, streams[s as usize].time_base, vec![s as u8; 10]);
            p.pts = Some(n[s as usize] * 20);
            p.duration = Some(20);
            p.flags.keyframe = true;
            p.flags.unit_boundary = boundary;
            n[s as usize] += 1;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let file = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}

#[test]
fn interleaved_runs_are_separate_chunks() {
    // audio x3, video, audio x2: the audio track has two chunks.
    let file = mux(
        "runs",
        &[
            (0, false),
            (0, false),
            (0, false),
            (1, false),
            (0, false),
            (0, false),
        ],
        ChunkPolicy::SampleCount,
    );
    let stsc = boxes(&file, "stsc");
    let stco = boxes(&file, "stco");
    // entry count, then (first chunk, samples per chunk, description)
    assert_eq!(u32s(&stsc[0][4..]), vec![2, 1, 3, 1, 2, 2, 1]);
    // entry count, then the chunk offsets
    let audio = u32s(&stco[0][4..]);
    let video = u32s(&stco[1][4..]);
    assert_eq!((audio[0], video[0]), (2, 1));
    assert_eq!(
        video[1] - audio[1],
        30,
        "the video sample follows three audio samples"
    );
    assert_eq!(
        audio[2] - video[1],
        10,
        "the second audio chunk follows the video sample"
    );
}

#[test]
fn packet_boundaries_end_chunks() {
    // two audio chunks of 2 back to back, then one video chunk of 2
    let file = mux(
        "boundaries",
        &[
            (0, false),
            (0, true),
            (0, false),
            (0, true),
            (1, false),
            (1, true),
        ],
        ChunkPolicy::PacketBoundaries,
    );
    let stsc = boxes(&file, "stsc");
    let stco = boxes(&file, "stco");
    assert_eq!(u32s(&stsc[0][4..]), vec![1, 1, 2, 1]);
    assert_eq!(u32s(&stsc[1][4..]), vec![1, 1, 2, 1]);
    let audio = u32s(&stco[0][4..]);
    assert_eq!(audio, vec![2, audio[1], audio[1] + 20]);
    assert_eq!(u32s(&stco[1][4..]), vec![1, audio[1] + 40]);
}
