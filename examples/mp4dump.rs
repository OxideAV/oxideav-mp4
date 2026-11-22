//! Print what the demuxer reports for a file: one `S` line per stream
//! (codec id, time base, start time, duration, dimensions or sample
//! rate / channels, extradata size and Adler-32, then every
//! `params.options` key) followed by one line per packet —
//! `stream,pts,dts,duration,size,flags,adler32:<hash>` — where `flags`
//! is `K` (key frame) / `D` (discard) or `_` in each position.
//!
//! ```text
//! cargo run --example mp4dump -- file.mp4 [--no-options]
//! ```

use oxideav_core::{Demuxer, RuntimeContext};

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: mp4dump <file> [--no-options]");
        std::process::exit(2);
    };
    let show_options = args.next().as_deref() != Some("--no-options");
    let ctx = RuntimeContext::new();
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let mut d = match oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    for s in d.streams() {
        let p = &s.params;
        println!(
            "S,{},{},{}/{},start={:?},dur={:?},{}x{},rate={:?},ch={:?},fr={:?},extradata={}:adler32:{:08x}",
            s.index,
            p.codec_id.as_str(),
            s.time_base.0.num,
            s.time_base.0.den,
            s.start_time,
            s.duration,
            p.width.unwrap_or(0),
            p.height.unwrap_or(0),
            p.sample_rate,
            p.channels,
            p.frame_rate.map(|r| (r.num, r.den)),
            p.extradata.len(),
            adler32(&p.extradata),
        );
        if show_options {
            for (k, v) in p.options.iter() {
                println!("O,{},{k}={v}", s.index);
            }
        }
    }
    if show_options {
        for (k, v) in d.metadata() {
            println!("M,{k}={v}");
        }
    }
    loop {
        match d.next_packet() {
            Ok(p) => println!(
                "{},{},{},{},{},{}{}_,adler32:{:08x}",
                p.stream_index,
                p.pts.map_or("N/A".into(), |v| v.to_string()),
                p.dts.map_or("N/A".into(), |v| v.to_string()),
                p.duration.map_or("N/A".into(), |v| v.to_string()),
                p.data.len(),
                if p.flags.keyframe { 'K' } else { '_' },
                if p.flags.discard { 'D' } else { '_' },
                adler32(&p.data),
            ),
            Err(e) if e.is_eof() => break,
            Err(e) => {
                println!("ERR,{e}");
                break;
            }
        }
    }
}
