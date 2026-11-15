//! Compact sample tables from the non-fragmented muxer: track-run
//! chunks (`ChunkPolicy::TrackRuns`), the `stz2` compact sample-size
//! box (ISO/IEC 14496-12 §8.7.3.3) and the absent sync-sample table of
//! an all-sync track (§8.6.2.1). Whatever the tables' shape, the
//! demuxed packets are the same.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, StreamInfo, TimeBase,
};
use oxideav_mp4::{ChunkPolicy, Mp4MuxerOptions};

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

/// Every box of this type inside the sample-table containers.
fn boxes<'a>(data: &'a [u8], name: &[u8; 4]) -> Vec<&'a [u8]> {
    const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];
    let mut out = Vec::new();
    let mut at = 0;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if size < 8 || at + size > data.len() {
            break;
        }
        let kind = &data[at + 4..at + 8];
        if kind == name {
            out.push(&data[at + 8..at + size]);
        } else if CONTAINERS.iter().any(|c| &c[..] == kind) {
            out.extend(boxes(&data[at + 8..at + size], name));
        }
        at += size;
    }
    out
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn streams() -> Vec<StreamInfo> {
    let mut v = CodecParameters::video(CodecId::new("mjpeg"));
    v.width = Some(16);
    v.height = Some(16);
    let mut a = CodecParameters::audio(CodecId::new("pcm_s16le"));
    a.channels = Some(1);
    a.sample_rate = Some(8000);
    vec![
        StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: v,
        },
        StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 8000),
            duration: None,
            start_time: Some(0),
            params: a,
        },
    ]
}

/// `frames` video frames of the given sizes (every fifth a sync
/// sample), each followed by three audio packets.
fn packets(streams: &[StreamInfo], sizes: &[usize]) -> Vec<Packet> {
    let mut out = Vec::new();
    for (i, &size) in sizes.iter().enumerate() {
        out.push(
            Packet::new(0, streams[0].time_base, vec![i as u8; size])
                .with_pts(i as i64 * 40)
                .with_dts(i as i64 * 40)
                .with_duration(40)
                .with_keyframe(i % 5 == 0),
        );
        for k in 0..3 {
            let t = (i as i64 * 3 + k) * 100;
            out.push(
                Packet::new(1, streams[1].time_base, vec![(i as u8) ^ 0x55; 200])
                    .with_pts(t)
                    .with_dts(t)
                    .with_duration(100)
                    .with_keyframe(true),
            );
        }
    }
    out
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

type Row = (u32, Option<i64>, Option<i64>, Option<i64>, bool, Vec<u8>);

fn demux(bytes: &[u8]) -> Vec<Row> {
    let mut d = oxideav_mp4::demux::open(Box::new(Cursor::new(bytes.to_vec())), &NullCodecResolver)
        .unwrap();
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push((
            p.stream_index,
            p.pts,
            p.dts,
            p.duration,
            p.flags.keyframe,
            p.data,
        ));
    }
    out
}

fn sizes(max: usize, n: usize) -> Vec<usize> {
    (0..n).map(|i| 1 + (i * 37) % max).collect()
}

#[test]
fn all_sync_track_has_no_sync_table() {
    let streams = streams();
    let bytes = mux(
        &streams,
        &packets(&streams, &sizes(90, 20)),
        Mp4MuxerOptions::default(),
    );
    // Video (every fifth sample sync) keeps its table; audio (all
    // sync) has none, and still demuxes as all-sync.
    let stss = boxes(&bytes, b"stss");
    assert_eq!(stss.len(), 1);
    assert_eq!(be32(stss[0], 4), 4);
    let rows = demux(&bytes);
    assert!(rows.iter().filter(|r| r.0 == 1).all(|r| r.4));
    assert_eq!(rows.iter().filter(|r| r.0 == 0 && r.4).count(), 4);
}

#[test]
fn track_runs_and_compact_sizes_demux_identically() {
    let streams = streams();
    for (max, field_size) in [(15usize, 4u8), (250, 8), (3000, 16)] {
        // An odd sample count exercises the 4-bit padding nibble.
        let pkts = packets(&streams, &sizes(max, 21));
        let plain = mux(&streams, &pkts, Mp4MuxerOptions::default());
        let compact = mux(
            &streams,
            &pkts,
            Mp4MuxerOptions::default()
                .with_chunking(ChunkPolicy::TrackRuns)
                .with_compact_sample_sizes(true),
        );
        assert_eq!(demux(&plain), demux(&compact), "max size {max}");
        assert_eq!(demux(&compact).len(), 21 * 4);

        // Video sizes vary: stz2 at the narrowest width that fits.
        let stz2 = boxes(&compact, b"stz2");
        assert_eq!(stz2.len(), 1, "max size {max}");
        assert_eq!(stz2[0][7], field_size);
        assert_eq!(be32(stz2[0], 8), 21);
        assert_eq!(
            stz2[0].len(),
            12 + (21 * usize::from(field_size)).div_ceil(8)
        );
        // Audio sizes are constant: the 20-byte stsz stays.
        let stsz = boxes(&compact, b"stsz");
        assert_eq!(stsz.len(), 1);
        assert_eq!((be32(stsz[0], 4), be32(stsz[0], 8)), (200, 63));
        assert!(boxes(&plain, b"stz2").is_empty());

        // Chunks: one per run. Video runs are single frames, audio
        // runs three packets — a third of the per-sample chunks.
        let chunk_counts =
            |b: &[u8]| -> Vec<u32> { boxes(b, b"stco").iter().map(|c| be32(c, 4)).collect() };
        assert_eq!(chunk_counts(&plain), [21, 63]);
        assert_eq!(chunk_counts(&compact), [21, 21]);
        assert!(compact.len() < plain.len());
    }
}

#[test]
fn sizes_past_sixteen_bits_keep_stsz() {
    let streams = streams();
    let pkts = packets(&streams, &[70_000, 10, 20]);
    let bytes = mux(
        &streams,
        &pkts,
        Mp4MuxerOptions::default().with_compact_sample_sizes(true),
    );
    assert!(boxes(&bytes, b"stz2").is_empty());
    assert_eq!(boxes(&bytes, b"stsz").len(), 2);
    assert_eq!(demux(&bytes).len(), 12);
}

#[test]
fn a_single_track_run_is_capped() {
    // One track, 2500 samples: runs never end on their own, so the
    // 1024-sample cap makes three chunks.
    let streams = &streams()[..1];
    let pkts: Vec<Packet> = (0..2500)
        .map(|i| {
            Packet::new(0, streams[0].time_base, vec![i as u8; 3 + i % 5])
                .with_pts(i as i64 * 40)
                .with_dts(i as i64 * 40)
                .with_duration(40)
                .with_keyframe(true)
        })
        .collect();
    let plain = mux(streams, &pkts, Mp4MuxerOptions::default());
    for faststart in [false, true] {
        let compact = mux(
            streams,
            &pkts,
            Mp4MuxerOptions::default()
                .with_faststart(faststart)
                .with_chunking(ChunkPolicy::TrackRuns)
                .with_compact_sample_sizes(true),
        );
        assert_eq!(be32(boxes(&compact, b"stco")[0], 4), 3);
        assert_eq!(boxes(&compact, b"stz2")[0][7], 4);
        assert_eq!(demux(&plain), demux(&compact));
    }
}
