//! Live black-box matrix, ours → foreign, and the remux chains.
//!
//! One clip per codec is written by `ffmpeg`, demuxed by this crate and
//! written back by every mode of this crate's muxer. `ffprobe` must
//! then report, for the rewritten file, the packets and stream
//! parameters it reports for the original (payload hashes, timestamps,
//! durations, key frames, configuration records), print no warning, and
//! `ffmpeg` must decode the rewritten file to the same output as the
//! original. Each rewritten file is also copied through `ffmpeg -c copy`
//! and compared again (foreign → ours → foreign), and `mpv`, when
//! present, has to play a few of them to the end.
//!
//! Skips when the tools are absent. `OXIDEAV_MP4_MATRIX=<substring>`
//! restricts the run; `OXIDEAV_MP4_MATRIX_KEEP=1` keeps the files.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, Muxer, Packet, RuntimeContext, StreamInfo, WriteSeek};
use oxideav_mp4::{BrandPreset, ChunkPolicy, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

const VALIDATED_MAJOR: u32 = 9;

fn tool_major(tool: &str) -> Option<u32> {
    let out = Command::new(tool).arg("-version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().nth(2)?;
    let digits: String = version
        .trim_start_matches('n')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some(digits.parse().unwrap_or(0))
}

const V: &str = "-f lavfi -i testsrc2=s=64x48:r=25:d=1";
const A: &str = "-f lavfi -i sine=f=440:d=1";

/// `(name, container, arguments)` of the source clips.
fn sources() -> Vec<(String, &'static str, String)> {
    let mut out = Vec::new();
    let video = [
        ("h264", "-c:v libx264 -bf 2 -g 10"),
        ("h264_nob", "-c:v libx264 -bf 0 -g 10"),
        (
            "hevc",
            "-c:v libx265 -x265-params log-level=error:bframes=3 -g 10",
        ),
        ("av1", "-c:v libsvtav1 -svtav1-params lp=1 -g 10"),
        ("vp9", "-c:v libvpx-vp9 -g 10 -cpu-used 8"),
        ("mpeg4", "-c:v mpeg4 -bf 2 -g 10"),
        ("mpeg1", "-c:v mpeg1video -bf 2 -g 10"),
        ("mpeg2", "-c:v mpeg2video -bf 2 -g 10"),
        ("mjpeg", "-c:v mjpeg"),
        ("jpeg2000", "-c:v jpeg2000"),
    ];
    for (name, enc) in video {
        out.push((format!("v_{name}"), "mp4", format!("{V} {enc}")));
    }
    out.push(("v_h263".into(), "3gp", format!("{V} -s 176x144 -c:v h263")));
    out.push(("v_prores".into(), "mov", format!("{V} -c:v prores_ks")));
    let audio = [
        ("aac", "-c:a aac"),
        ("aac_he", "-c:a aac_at -profile:a 4 -ac 2"),
        ("aac_6ch_96k", "-ar 96000 -ac 6 -c:a aac"),
        ("mp3", "-c:a libmp3lame"),
        ("mp2", "-c:a mp2"),
        ("ac3", "-c:a ac3"),
        ("eac3", "-c:a eac3"),
        ("opus", "-c:a libopus"),
        ("opus_6ch", "-ar 48000 -ac 6 -c:a libopus"),
        ("flac", "-c:a flac -strict -2"),
        ("flac_96k", "-ar 96000 -sample_fmt s32 -c:a flac -strict -2"),
        ("alac", "-c:a alac"),
        ("dts", "-c:a dca -strict -2 -ac 2"),
        ("pcm_s16le", "-c:a pcm_s16le"),
        ("pcm_s24le", "-ac 2 -c:a pcm_s24le"),
        ("pcm_s32le", "-c:a pcm_s32le"),
        ("pcm_f32le", "-c:a pcm_f32le"),
        ("pcm_f64le", "-c:a pcm_f64le"),
        ("pcm_s16be", "-c:a pcm_s16be"),
    ];
    for (name, enc) in audio {
        out.push((format!("a_{name}"), "mp4", format!("{A} {enc}")));
    }
    out.push((
        "a_alaw".into(),
        "mov",
        format!("{A} -ar 8000 -c:a pcm_alaw"),
    ));
    out.push((
        "a_mulaw".into(),
        "mov",
        format!("{A} -ar 8000 -c:a pcm_mulaw"),
    ));
    // Several tracks, B-frames, a start delay.
    out.push((
        "av".into(),
        "mp4",
        format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac"),
    ));
    out.push((
        "av_delay".into(),
        "mp4",
        format!("-itsoffset 0.7 {V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac"),
    ));
    out.push((
        "av_frag".into(),
        "mp4",
        format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac -movflags frag_keyframe+empty_moov"),
    ));
    out.push((
        "v_ntsc".into(),
        "mp4",
        "-f lavfi -i testsrc2=s=64x48:r=30000/1001:d=1 -c:v libx264 -bf 2".into(),
    ));
    out.push((
        "v_vfr".into(),
        "mp4",
        "-f lavfi -i testsrc2=s=64x48:r=25:d=2 -vf select=not(mod(n\\,3))+gt(n\\,30) \
         -fps_mode vfr -c:v libx264 -bf 2"
            .into(),
    ));
    out.push((
        "v_one_frame".into(),
        "mp4",
        format!("{V} -frames:v 1 -c:v libx264"),
    ));
    out.push((
        "a_one_packet".into(),
        "mp4",
        format!("{A} -c:a aac -frames:a 1"),
    ));
    out.push((
        "v_10bit".into(),
        "mp4",
        format!("{V} -pix_fmt yuv420p10le -c:v libx265 -x265-params log-level=error"),
    ));
    out.push((
        "av_start_1s".into(),
        "mp4",
        format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac -output_ts_offset 1"),
    ));
    out.push((
        "av_long".into(),
        "mp4",
        "-f lavfi -i testsrc2=s=64x48:r=25:d=60 -f lavfi -i sine=f=440:d=60 \
         -c:v libx264 -preset ultrafast -bf 2 -g 50 -c:a aac"
            .into(),
    ));
    out.push((
        "av_ismv".into(),
        "ismv",
        format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac"),
    ));
    out.push((
        "av_cmaf".into(),
        "mp4",
        format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac -movflags cmaf"),
    ));
    out.push((
        "av_four".into(),
        "mp4",
        format!(
            "{V} {A} -f lavfi -i sine=f=880:d=1 -f lavfi -i testsrc=s=32x32:r=10:d=1 \
             -map 0 -map 1 -map 2 -map 3 -c:v libx264 -c:a aac"
        ),
    ));
    out
}

/// Sources this crate demuxes but has no sample entry to write.
const NOT_WRITTEN_BACK: [&str; 2] = ["v_prores", "a_dts"];

/// The writer modes of this crate.
fn modes() -> Vec<(&'static str, Mp4MuxerOptions)> {
    let frag = |cadence| FragmentedOptions::default().with_cadence(cadence);
    vec![
        ("progressive", Mp4MuxerOptions::default()),
        ("faststart", Mp4MuxerOptions::default().with_faststart(true)),
        (
            "mov",
            Mp4MuxerOptions::default().with_brand(BrandPreset::Mov),
        ),
        (
            "ismv",
            Mp4MuxerOptions::default().with_brand(BrandPreset::Ismv),
        ),
        (
            "large_mdat",
            Mp4MuxerOptions::default().with_large_mdat(true),
        ),
        (
            "track_runs",
            Mp4MuxerOptions::default()
                .with_chunking(ChunkPolicy::TrackRuns)
                .with_compact_sample_sizes(true),
        ),
        (
            "fragmented",
            Mp4MuxerOptions::default()
                .with_fragmented(Some(frag(FragmentCadence::EverySeconds(0.4)))),
        ),
        (
            "fragmented_key",
            Mp4MuxerOptions::default().with_fragmented(Some(frag(FragmentCadence::EveryKeyframe))),
        ),
        (
            "fragmented_plain",
            Mp4MuxerOptions::default().with_fragmented(Some(
                frag(FragmentCadence::EveryNPackets(7))
                    .with_styp(None)
                    .with_emit_random_access_indexes(false),
            )),
        ),
        (
            "fragmented_mehd",
            Mp4MuxerOptions::default().with_fragmented(Some(
                frag(FragmentCadence::EverySeconds(0.5)).with_write_mehd(true),
            )),
        ),
    ]
}

fn demux(path: &Path) -> Result<(Vec<StreamInfo>, Vec<Packet>), String> {
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut d =
        oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).map_err(|e| e.to_string())?;
    let streams = d.streams().to_vec();
    let mut packets = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.push(p),
            Err(e) if e.is_eof() => break,
            Err(e) => return Err(format!("next_packet: {e}")),
        }
    }
    Ok((streams, packets))
}

fn mux(
    path: &Path,
    streams: &[StreamInfo],
    packets: &[Packet],
    options: Mp4MuxerOptions,
) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let ws: Box<dyn WriteSeek> = Box::new(file);
    let mut m: Box<dyn Muxer> = oxideav_mp4::muxer::open_with_options(ws, streams, options)
        .map_err(|e| format!("open: {e}"))?;
    m.write_header().map_err(|e| format!("header: {e}"))?;
    for p in packets {
        m.write_packet(p).map_err(|e| format!("packet: {e}"))?;
    }
    m.write_trailer().map_err(|e| format!("trailer: {e}"))
}

/// What the reader reports for a file: one line per stream, one per
/// packet (grouped by stream), and what it printed on stderr.
struct Report {
    streams: Vec<String>,
    packets: Vec<Vec<String>>,
    stderr: String,
}

fn ffprobe(path: &Path) -> Option<Report> {
    let run = |entries: &str| {
        Command::new("ffprobe")
            .args(["-v", "warning", "-show_data_hash", "adler32"])
            .args(["-show_entries", entries, "-of", "csv=p=0"])
            .arg(path)
            .output()
            .ok()
    };
    let s = run(
        "stream=index,codec_name,profile,width,height,pix_fmt,sample_rate,channels,\
         time_base,extradata_hash",
    )?;
    let p = run("packet=stream_index,pts,dts,duration,size,flags,data_hash")?;
    let streams: Vec<String> = String::from_utf8_lossy(&s.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let mut packets = vec![Vec::new(); streams.len()];
    for line in String::from_utf8_lossy(&p.stdout).lines() {
        let Some((index, rest)) = line.split_once(',') else {
            continue;
        };
        let Ok(index) = index.parse::<usize>() else {
            continue;
        };
        if let Some(list) = packets.get_mut(index) {
            list.push(rest.trim_end_matches(',').to_string());
        }
    }
    Some(Report {
        streams,
        packets,
        stderr: String::from_utf8_lossy(&p.stderr).trim().to_string(),
    })
}

/// MD5 of everything `ffmpeg` decodes from the file, and its stderr.
fn decoded_hash(path: &Path) -> Option<(String, String)> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0", "-f", "hash", "-hash", "md5", "-"])
        .output()
        .ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// What a comparison leaves out.
#[derive(Clone, Copy, Default)]
struct Lenient {
    /// The configuration record hash: the reader's own muxer rewrites
    /// some records (`hvcC`) when it copies a stream.
    extradata: bool,
    /// The discard flag and the last packet's duration: in a fragmented
    /// file the reader neither flags the packet an edit list trims nor
    /// recomputes a shortened final duration.
    fragmented: bool,
    /// The reader's muxer starts the copy of a fragmented file at its
    /// own origin: timestamps are compared from each stream's first
    /// packet.
    rebased: bool,
    /// Timestamps altogether: the source's tracks run on a 10 MHz clock
    /// (Smooth Streaming) and are written back on their own (the audio
    /// sample rate), so only payloads, sizes and key frames compare.
    retimed: bool,
}

/// One packet line: `pts,dts,duration,size,flags,hash`.
fn same_packet(
    (want, want_first): (&str, &str),
    (got, got_first): (&str, &str),
    last: bool,
    lenient: Lenient,
) -> bool {
    let fields = |s: &str| -> Vec<String> { s.split(',').map(str::to_string).collect() };
    let (mut a, mut b) = (fields(want), fields(got));
    if a.len() != 6 || b.len() != 6 {
        return want == got;
    }
    if lenient.retimed {
        return a[3] == b[3] && a[4].as_bytes().first() == b[4].as_bytes().first() && a[5] == b[5];
    }
    if lenient.rebased {
        for (line, first) in [(&mut a, fields(want_first)), (&mut b, fields(got_first))] {
            for field in line.iter_mut().take(2) {
                if let (Ok(v), Ok(base)) = (field.parse::<i64>(), first[0].parse::<i64>()) {
                    *field = (v - base).to_string();
                }
            }
        }
    }
    let duration = a[2] == b[2] || a[2] == "N/A" || b[2] == "N/A" || lenient.fragmented && last;
    let flags = if lenient.fragmented {
        a[4].as_bytes().first() == b[4].as_bytes().first()
    } else {
        a[4] == b[4]
    };
    a[0] == b[0] && a[1] == b[1] && duration && a[3] == b[3] && flags && a[5] == b[5]
}

fn diff(what: &str, want: &Report, got: &Report, lenient: Lenient) -> Vec<String> {
    let mut issues = Vec::new();
    let line = |s: &String| {
        let s = if lenient.extradata {
            s.rsplit_once(',')
                .map_or(s.clone(), |(head, _)| head.to_string())
        } else {
            s.clone()
        };
        if lenient.retimed {
            // Drop the time base.
            let kept: Vec<&str> = s.split(',').filter(|f| !f.starts_with("1/")).collect();
            kept.join(",")
        } else {
            s
        }
    };
    if want
        .streams
        .iter()
        .map(line)
        .ne(got.streams.iter().map(line))
    {
        issues.push(format!(
            "{what} streams:\n      want {:?}\n      got  {:?}",
            want.streams, got.streams
        ));
    }
    for (i, (a, b)) in want.packets.iter().zip(&got.packets).enumerate() {
        if a.len() != b.len() {
            issues.push(format!(
                "{what} s{i} packets: want {} got {}",
                a.len(),
                b.len()
            ));
            continue;
        }
        let bad: Vec<usize> = (0..a.len())
            .filter(|&n| !same_packet((&a[n], &a[0]), (&b[n], &b[0]), n + 1 == a.len(), lenient))
            .collect();
        if let Some(&n) = bad.first() {
            issues.push(format!(
                "{what} s{i}: {} packets differ, first {n}: want {} got {}",
                bad.len(),
                a[n],
                b[n]
            ));
        }
    }
    let unaddressed = |s: &str| {
        // Messages carry the reader's context address.
        s.lines()
            .map(|l| l.split_once("] ").map_or(l, |(_, rest)| rest).to_string())
            .collect::<Vec<_>>()
    };
    if !got.stderr.is_empty() && unaddressed(&got.stderr) != unaddressed(&want.stderr) {
        let first = got.stderr.lines().next().unwrap_or("");
        issues.push(format!("{what} reader says: {first}"));
    }
    issues
}

enum Outcome {
    NotWritten,
    /// `(files compared, differences)`.
    Done(usize, Vec<String>),
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin", "-i"])
        .arg(from)
        .args(["-map", "0", "-c", "copy", "-bitexact"])
        .arg(to)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() && out.stderr.is_empty() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&out.stderr);
        Err(said.lines().next().unwrap_or("failed").to_string())
    }
}

/// Exit status of the player run to the end of the file.
fn play(path: &Path) -> Option<i32> {
    Command::new("mpv")
        .args(["--no-config", "--really-quiet", "--vo=null", "--ao=null"])
        .args(["--untimed", "--frames=200"])
        .arg(path)
        .output()
        .ok()?
        .status
        .code()
}

fn run_source(dir: &Path, name: &str, ext: &str, args: &str, mpv: bool) -> Outcome {
    let src = dir.join(format!("{name}.{ext}"));
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin"])
        .args(args.split_whitespace())
        .arg("-bitexact")
        .arg(&src)
        .output();
    if !made.is_ok_and(|o| o.status.success()) || std::fs::metadata(&src).is_err() {
        return Outcome::NotWritten;
    }
    let (Some(want), Some(want_decoded)) = (ffprobe(&src), decoded_hash(&src)) else {
        return Outcome::NotWritten;
    };
    // What the reader decodes from a fragmented file is not what it
    // decodes from the sample tables of the same streams: only there
    // does it trim the end of the last audio frame to the declared
    // duration, and a fragmented file's edit list is open-ended. The
    // decoded output is compared between sample-table files only; a
    // fragmented one has to decode cleanly.
    let source_fragmented =
        args.contains("-movflags frag") || args.contains("-movflags cmaf") || ext == "ismv";
    // Big-endian PCM is delivered — and so written back — little-endian:
    // the packets differ by design, the decoded audio must not.
    let swapped = name.ends_with("be");
    let retimed = ext == "ismv";
    // Signed composition offsets: the reader moves the pts of a
    // fragmented file and the dts of a sample-table one by its
    // constant, so timestamps compare from the first packet.
    let signed_offsets = args.contains("-movflags cmaf");
    // The reader's own copy of the source: what the copy of a file
    // written here is held to (the copying muxer moves start offsets
    // and rewrites some configuration records).
    let reference = dir.join(format!("{name}.reference.{ext}"));
    let want_copied = copy(&src, &reference)
        .ok()
        .and_then(|()| ffprobe(&reference));
    let want_played = if mpv { play(&src) } else { None };
    let mut issues = Vec::new();
    let mut compared = 0;
    let (streams, packets) = match demux(&src) {
        Ok(v) => v,
        Err(e) => return Outcome::Done(1, vec![format!("{name}: demux: {e}")]),
    };
    // Demuxed with the right codec id, not written back: the sample
    // entry needs facts the stream parameters do not carry (the ProRes
    // profile FourCC, the DTS `ddts` record).
    if NOT_WRITTEN_BACK.contains(&name) {
        let refused = mux(
            &dir.join(format!("{name}.refused.mp4")),
            &streams,
            &packets,
            Mp4MuxerOptions::default(),
        );
        return match refused {
            Err(e) if e.contains("no sample entry") => Outcome::Done(0, Vec::new()),
            other => Outcome::Done(
                1,
                vec![format!("{name}: expected a refusal, got {other:?}")],
            ),
        };
    }
    for (mode, options) in modes() {
        let tag = format!("{name}/{mode}");
        let out = dir.join(format!("{name}.{mode}.mp4"));
        compared += 1;
        if let Err(e) = mux(&out, &streams, &packets, options) {
            issues.push(format!("{tag}: mux: {e}"));
            continue;
        }
        let Some(got) = ffprobe(&out) else {
            issues.push(format!("{tag}: reader failed"));
            continue;
        };
        let fragmented = mode.starts_with("fragmented");
        let lenient = Lenient {
            fragmented,
            retimed,
            rebased: signed_offsets,
            ..Lenient::default()
        };
        if !swapped {
            issues.extend(diff(&tag, &want, &got, lenient));
        }
        match decoded_hash(&out) {
            Some((_, err)) if fragmented || source_fragmented => {
                if !err.is_empty() {
                    issues.push(format!("{tag}: decoder says: {err}"));
                }
            }
            Some(d) if d == want_decoded => {}
            Some((hash, err)) => issues.push(format!(
                "{tag}: decodes to {hash} ({err}), the original to {}",
                want_decoded.0
            )),
            None => issues.push(format!("{tag}: decoder failed")),
        }
        // This crate reads its own file back to the same packets.
        match demux(&out) {
            Ok((_, again)) => {
                let key = |p: &Packet| {
                    let times = (!retimed).then_some((p.pts, p.dts, p.duration));
                    (p.stream_index, times, p.flags.keyframe, p.data.clone())
                };
                let mut a: Vec<_> = packets.iter().map(key).collect();
                let mut b: Vec<_> = again.iter().map(key).collect();
                a.sort();
                b.sort();
                if a != b {
                    let n = a.iter().zip(&b).position(|(x, y)| x != y);
                    issues.push(format!(
                        "{tag}: own read-back differs ({} vs {} packets, first at {n:?})",
                        a.len(),
                        b.len()
                    ));
                }
            }
            Err(e) => issues.push(format!("{tag}: own read-back: {e}")),
        }
        // foreign → ours → foreign.
        let copied = dir.join(format!("{name}.{mode}.copy.{ext}"));
        match (copy(&out, &copied), &want_copied) {
            (Ok(()), Some(want)) if !swapped => {
                if let Some(got) = ffprobe(&copied) {
                    let lenient = Lenient {
                        extradata: true,
                        fragmented,
                        rebased: fragmented || signed_offsets,
                        retimed,
                    };
                    issues.extend(diff(&format!("{tag} copied"), want, &got, lenient));
                }
            }
            (Ok(()), _) => {}
            (Err(e), _) => issues.push(format!("{tag}: copy: {e}")),
        }
        // The player gets to the end of the file as it does with the
        // original.
        if mpv && matches!(mode, "progressive" | "fragmented") {
            let played = play(&out);
            if played != Some(0) && played != want_played {
                issues.push(format!(
                    "{tag}: player exit {played:?}, {want_played:?} on the original"
                ));
            }
        }
    }
    Outcome::Done(compared, issues)
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("OXIDEAV_MP4_MATRIX_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn files_written_here_read_like_the_originals() {
    let (Some(major), Some(_)) = (tool_major("ffmpeg"), tool_major("ffprobe")) else {
        eprintln!("skipped: ffmpeg / ffprobe not found");
        return;
    };
    let mpv = Command::new("mpv")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !mpv {
        eprintln!("mpv not found: playback legs skipped");
    }
    let scratch = Scratch(
        std::env::temp_dir().join(format!("oxideav-mp4-mux-matrix-{}", std::process::id())),
    );
    std::fs::create_dir_all(&scratch.0).unwrap();
    let dir = scratch.0.as_path();
    let filter = std::env::var("OXIDEAV_MP4_MATRIX").unwrap_or_default();
    let mut all = sources();
    all.retain(|s| s.0.contains(&filter));

    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, ext, args)) = all.get(i) else {
                        break;
                    };
                    let outcome = run_source(dir, name, ext, args, mpv);
                    results.lock().unwrap().push((i, outcome));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);
    let (mut files, mut bad, mut skipped) = (0, 0, 0);
    for (i, outcome) in &results {
        match outcome {
            Outcome::NotWritten => {
                skipped += 1;
                eprintln!("not written by the tool: {}", all[*i].0);
            }
            Outcome::Done(n, issues) => {
                files += n;
                bad += issues.len();
                for issue in issues {
                    eprintln!("FAIL {issue}");
                }
            }
        }
    }
    eprintln!(
        "ours → foreign: {files} files written from {} sources, {bad} differences; \
         {skipped} sources the tool did not write",
        results.len() - skipped
    );
    if major < VALIDATED_MAJOR {
        eprintln!(
            "tool version {major} is older than the validated {VALIDATED_MAJOR}: not asserted"
        );
        return;
    }
    assert_eq!(bad, 0, "{bad} differences from the originals");
}
