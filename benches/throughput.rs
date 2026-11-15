//! Timed throughput run (no harness): mux, `moov` finalisation, open,
//! packet read and seek on a large self-muxed progressive file and a
//! fragmented one.
//!
//! ```text
//! cargo bench --bench throughput                 # 2 M samples / 4000 fragments
//! MP4_BENCH_SAMPLES=200000 MP4_BENCH_FRAGMENTS=500 cargo bench --bench throughput
//! ```
//!
//! Each stage is run `MP4_BENCH_RUNS` times (default 5) and the median
//! is printed.

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, StreamInfo,
    TimeBase,
};
use oxideav_mp4::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

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

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn streams() -> Vec<StreamInfo> {
    let mut video = CodecParameters::video(CodecId::new("mjpeg"));
    video.width = Some(64);
    video.height = Some(64);
    let mut audio = CodecParameters::audio(CodecId::new("pcm_s16le"));
    audio.sample_rate = Some(48_000);
    audio.channels = Some(2);
    vec![
        StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: video,
        },
        StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params: audio,
        },
    ]
}

/// Interleaved packets: one 40 ms video frame (every 25th a keyframe,
/// sizes varying) then two 20 ms audio packets.
fn packets(streams: &[StreamInfo], total: usize) -> Vec<Packet> {
    let mut out = Vec::with_capacity(total);
    let mut n = 0usize;
    while out.len() < total {
        let size = 24 + (n * 7) % 40;
        out.push(
            Packet::new(0, streams[0].time_base, vec![n as u8; size])
                .with_pts(n as i64 * 40)
                .with_dts(n as i64 * 40)
                .with_duration(40)
                .with_keyframe(n.is_multiple_of(25)),
        );
        for k in 0..2 {
            let t = (n as i64 * 2 + k) * 960;
            out.push(
                Packet::new(1, streams[1].time_base, vec![k as u8; 16])
                    .with_pts(t)
                    .with_dts(t)
                    .with_duration(960)
                    .with_keyframe(true),
            );
        }
        n += 1;
    }
    out
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

struct MuxTimes {
    bytes: Vec<u8>,
    packets: Duration,
    trailer: Duration,
}

fn mux(streams: &[StreamInfo], pkts: &[Packet], options: Mp4MuxerOptions) -> MuxTimes {
    let sink = Sink::default();
    let mut m: Box<dyn Muxer> =
        oxideav_mp4::muxer::open_with_options(Box::new(sink.clone()), streams, options).unwrap();
    m.write_header().unwrap();
    let t0 = Instant::now();
    for p in pkts {
        m.write_packet(p).unwrap();
    }
    let packets = t0.elapsed();
    let t1 = Instant::now();
    m.write_trailer().unwrap();
    let trailer = t1.elapsed();
    drop(m);
    let bytes = std::mem::take(sink.0.lock().unwrap().get_mut());
    MuxTimes {
        bytes,
        packets,
        trailer,
    }
}

fn moov_size(bytes: &[u8]) -> usize {
    let mut off = 0usize;
    while off + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        if &bytes[off + 4..off + 8] == b"moov" {
            return size;
        }
        if size < 8 {
            break;
        }
        off += size;
    }
    0
}

fn report(label: &str, streams: &[StreamInfo], pkts: &[Packet], options: Mp4MuxerOptions) {
    let runs = env("MP4_BENCH_RUNS", 5);
    let n = pkts.len() as f64;
    let mut mux_p = Vec::new();
    let mut mux_t = Vec::new();
    let mut bytes = Vec::new();
    for _ in 0..runs {
        let m = mux(streams, pkts, options.clone());
        mux_p.push(m.packets);
        mux_t.push(m.trailer);
        bytes = m.bytes;
    }
    let (mux_p, mux_t) = (median(mux_p), median(mux_t));
    println!(
        "{label}: {} packets, file {:.1} MiB, moov {:.2} MiB",
        pkts.len(),
        bytes.len() as f64 / 1048576.0,
        moov_size(&bytes) as f64 / 1048576.0
    );
    println!(
        "  mux write_packet   {:>9.1} ms  ({:.2} M packets/s)",
        mux_p.as_secs_f64() * 1e3,
        n / mux_p.as_secs_f64() / 1e6
    );
    println!("  mux write_trailer  {:>9.1} ms", mux_t.as_secs_f64() * 1e3);

    // The same file through an unbuffered `std::fs::File`: every read
    // the demuxer issues is a system call.
    let path = std::env::temp_dir().join(format!(
        "oxideav-mp4-bench-{}-{}.mp4",
        std::process::id(),
        label.len()
    ));
    std::fs::write(&path, &bytes).unwrap();
    let mut file_t = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let f = std::fs::File::open(&path).unwrap();
        let mut d = oxideav_mp4::demux::open_typed(Box::new(f), &NullCodecResolver).unwrap();
        let mut count = 0usize;
        while d.next_packet().is_ok() {
            count += 1;
        }
        assert_eq!(count, pkts.len());
        file_t.push(t0.elapsed());
    }
    let _ = std::fs::remove_file(&path);
    let file_t = median(file_t);
    println!(
        "  file open + drain  {:>9.1} ms  ({:.2} M packets/s)",
        file_t.as_secs_f64() * 1e3,
        n / file_t.as_secs_f64() / 1e6
    );

    let shared: Arc<[u8]> = bytes.into();
    let mut open_t = Vec::new();
    let mut read_t = Vec::new();
    let mut seek_t = Vec::new();
    let seeks = 2000usize;
    for _ in 0..runs {
        let t0 = Instant::now();
        let mut d = oxideav_mp4::demux::open_typed(
            Box::new(Cursor::new(shared.clone())),
            &NullCodecResolver,
        )
        .unwrap();
        open_t.push(t0.elapsed());
        let t1 = Instant::now();
        let mut count = 0usize;
        let mut last_video_pts = 0i64;
        loop {
            match d.next_packet() {
                Ok(p) => {
                    count += 1;
                    if p.stream_index == 0 {
                        last_video_pts = p.pts.unwrap_or(0);
                    }
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        read_t.push(t1.elapsed());
        assert_eq!(count, pkts.len());
        let t2 = Instant::now();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..seeks {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let target = (x % (last_video_pts as u64 + 1)) as i64;
            let landed = d.seek_to(0, target).unwrap();
            assert!(landed <= target);
        }
        seek_t.push(t2.elapsed());
    }
    let (open_t, read_t, seek_t) = (median(open_t), median(read_t), median(seek_t));
    println!(
        "  demux open         {:>9.1} ms",
        open_t.as_secs_f64() * 1e3
    );
    println!(
        "  demux next_packet  {:>9.1} ms  ({:.2} M packets/s)",
        read_t.as_secs_f64() * 1e3,
        n / read_t.as_secs_f64() / 1e6
    );
    println!(
        "  demux seek_to      {:>9.2} us per seek ({seeks} random video seeks)",
        seek_t.as_secs_f64() * 1e6 / seeks as f64
    );
}

fn main() {
    // `cargo bench` passes `--bench`; anything else that runs this
    // binary (`cargo test --all-targets`) is only checking that it
    // builds and starts, and gets a small file.
    let timed = std::env::args().any(|a| a == "--bench");
    let streams = streams();
    if !timed {
        let pkts = packets(&streams, 3000);
        report("smoke", &streams, &pkts, Mp4MuxerOptions::default());
        return;
    }
    let total = env("MP4_BENCH_SAMPLES", 2_000_000);
    let pkts = packets(&streams, total);
    report("progressive", &streams, &pkts, Mp4MuxerOptions::default());
    report(
        "progressive, compact tables (track-run chunks + stz2)",
        &streams,
        &pkts,
        Mp4MuxerOptions::default()
            .with_chunking(oxideav_mp4::ChunkPolicy::TrackRuns)
            .with_compact_sample_sizes(true),
    );
    report(
        "faststart",
        &streams,
        &pkts,
        Mp4MuxerOptions::default().with_faststart(true),
    );

    // Fragmented: 75 packets (25 video frames, one second) per fragment.
    let fragments = env("MP4_BENCH_FRAGMENTS", 4000);
    let pkts = packets(&streams, fragments * 75);
    let frag = |indexes: bool| {
        Mp4MuxerOptions::default().with_fragmented(Some(
            FragmentedOptions::default()
                .with_cadence(FragmentCadence::EveryNPackets(25))
                .with_emit_random_access_indexes(indexes),
        ))
    };
    report("fragmented (sidx + mfra)", &streams, &pkts, frag(true));
    report("fragmented (no index)", &streams, &pkts, frag(false));
}
