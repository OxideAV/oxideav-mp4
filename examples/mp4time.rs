//! Time the demuxer on a file: open, read every packet, then seek
//! around stream 0. Prints milliseconds and MiB/s; run it under
//! `/usr/bin/time -l` (macOS) or `/usr/bin/time -v` (Linux) for the
//! peak resident set.
//!
//! ```text
//! cargo run --release --example mp4time -- file.mp4
//! ```

use std::time::Instant;

use oxideav_core::RuntimeContext;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: mp4time <file>");
        std::process::exit(2);
    };
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mib = size as f64 / (1024.0 * 1024.0);
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(&path).expect("open file");

    let t = Instant::now();
    let mut d = oxideav_mp4::demux::open(Box::new(file), &ctx.codecs).expect("open mp4");
    let open_ms = t.elapsed().as_secs_f64() * 1e3;

    let t = Instant::now();
    let (mut packets, mut bytes) = (0u64, 0u64);
    let mut last_pts = 0i64;
    loop {
        match d.next_packet() {
            Ok(p) => {
                packets += 1;
                bytes += p.data.len() as u64;
                if p.stream_index == 0 {
                    last_pts = last_pts.max(p.pts.unwrap_or(0));
                }
            }
            Err(e) if e.is_eof() => break,
            Err(e) => panic!("{e}"),
        }
    }
    let read_s = t.elapsed().as_secs_f64();

    // 2000 seeks spread over stream 0 by a fixed stride.
    let t = Instant::now();
    let seeks = 2000i64;
    for i in 0..seeks {
        let target = (i * 7919 % seeks) * last_pts / seeks;
        let _ = d.seek_to(0, target);
    }
    let seek_us = t.elapsed().as_secs_f64() * 1e6 / seeks as f64;

    println!(
        "{mib:.1} MiB, {} streams, {packets} packets, {:.1} MiB payload",
        d.streams().len(),
        bytes as f64 / (1024.0 * 1024.0)
    );
    println!("open           {open_ms:10.2} ms");
    println!(
        "read all       {:10.2} ms  ({:.0} MiB/s, {:.2} M packets/s)",
        read_s * 1e3,
        mib / read_s,
        packets as f64 / read_s / 1e6
    );
    println!("seek           {seek_us:10.2} us per seek");
}
