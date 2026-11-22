//! Live black-box matrix, foreign → ours.
//!
//! `ffmpeg` writes every codec it will put into `mp4` / `mov` / `3gp` /
//! `m4a` / `ismv` and one clip in every file layout its muxer exposes;
//! each file is demuxed here and by `ffprobe`, and the two reports are
//! compared per track: packet count, payload (Adler-32), pts / dts /
//! duration, key-frame and discard flags, codec id, time base,
//! dimensions, sample rate, channel count, configuration record, start
//! time.
//!
//! The test skips when the tools are absent and generates everything in
//! a temporary directory. Where this crate reports something else than
//! that reader on purpose, the rule is stated at the comparison
//! ([`Quirk`], [`compare`]) with the clause it follows.
//!
//! `OXIDEAV_MP4_MATRIX=<substring>` restricts the run to matching case
//! names; `OXIDEAV_MP4_MATRIX_KEEP=1` keeps the generated files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, MediaType, RuntimeContext};

/// The tool version the expectations below were validated against. An
/// older build may mux or report differently; its result is printed
/// and not asserted.
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

/// Deliberate differences from the reference reader, per case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quirk {
    /// Signed composition offsets (`ctts` / `trun` version 1): this
    /// crate reports CT = DT + offset (ISO/IEC 14496-12 §8.6.1.3); the
    /// reader moves every pts or dts of the stream by one constant so
    /// that no dts exceeds its pts. Timestamps are compared up to a
    /// constant per stream.
    SignedOffsets,
    /// A global `sidx`: the reader marks only each indexed subsegment's
    /// first frame as a key frame; the `trun` sample flags (§8.8.8)
    /// still say which samples are sync samples. Key flags of video
    /// are not compared.
    SidxKeyFrames,
    /// One sample per fragment, written with a zero default duration:
    /// the reader derives durations from the next fragment's decode
    /// time. Durations and the discard flag are not compared.
    ZeroDurations,
    /// The audio edit list of a delayed `moov` trims the priming
    /// packet, which this crate flags `discard` (§8.6.6) and the reader
    /// does not for this layout.
    DiscardFlag,
    /// QuickTime partial sync samples (`stps`, open-GOP I-frames): "not
    /// full sync samples" (QuickTime File Format, "Partial Sync Sample
    /// Atom"), so not key frames here; the reader flags them.
    PartialSync,
}

struct Case {
    name: String,
    ext: &'static str,
    args: String,
    quirks: Vec<Quirk>,
}

const V: &str = "-f lavfi -i testsrc2=s=64x48:r=25:d=1";
const A: &str = "-f lavfi -i sine=f=440:d=1";

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add = |name: &str, ext: &'static str, args: String, quirks: &[Quirk]| {
        out.push(Case {
            name: format!("{name}.{ext}"),
            ext,
            args,
            quirks: quirks.to_vec(),
        });
    };

    // --- every codec × every container flavour ---------------------
    let video: &[(&str, &str)] = &[
        ("h264", "-c:v libx264 -bf 2 -g 10"),
        ("h264_nob", "-c:v libx264 -bf 0 -g 10"),
        ("h264_avc3", "-c:v libx264 -tag:v avc3"),
        (
            "hevc_hvc1",
            "-c:v libx265 -tag:v hvc1 -x265-params log-level=error -g 10",
        ),
        (
            "hevc_hev1",
            "-c:v libx265 -tag:v hev1 -x265-params log-level=error -g 10",
        ),
        ("av1", "-c:v libsvtav1 -svtav1-params lp=1 -g 10"),
        ("vp9", "-c:v libvpx-vp9 -g 10 -cpu-used 8"),
        ("vp8", "-c:v libvpx -g 10"),
        ("mpeg4", "-c:v mpeg4 -bf 2 -g 10"),
        ("mpeg4_nob", "-c:v mpeg4 -g 10"),
        ("mpeg1", "-c:v mpeg1video -bf 2 -g 10"),
        ("mpeg2", "-c:v mpeg2video -bf 2 -g 10"),
        ("mjpeg", "-c:v mjpeg"),
        ("h263", "-s 176x144 -c:v h263"),
        ("jpeg2000", "-c:v jpeg2000"),
        ("png", "-c:v png"),
        ("ffv1", "-c:v ffv1 -strict -2"),
        ("vc2", "-c:v vc2 -strict -2"),
        ("prores", "-c:v prores_ks"),
        ("dnxhd", "-s 1280x720 -pix_fmt yuv422p -c:v dnxhd -b:v 60M"),
        ("qtrle", "-c:v qtrle"),
        ("cfhd", "-c:v cfhd"),
        ("dv", "-s 720x576 -pix_fmt yuv420p -c:v dvvideo"),
        ("svq1", "-c:v svq1"),
        ("v210", "-c:v v210"),
        ("h261", "-s 176x144 -c:v h261"),
    ];
    let audio: &[(&str, &str)] = &[
        ("aac", "-c:a aac"),
        ("aac_he", "-c:a aac_at -profile:a 4 -ac 2"),
        ("aac_hev2", "-c:a aac_at -profile:a 28 -ac 2"),
        ("mp3", "-c:a libmp3lame"),
        ("mp2", "-c:a mp2"),
        ("ac3", "-c:a ac3"),
        ("eac3", "-c:a eac3"),
        ("opus", "-c:a libopus"),
        ("flac", "-c:a flac -strict -2"),
        ("alac", "-c:a alac"),
        ("truehd", "-c:a truehd -strict -2 -ac 2"),
        ("dts", "-c:a dca -strict -2 -ac 2"),
        ("pcm_s16le", "-c:a pcm_s16le"),
        ("pcm_s16be", "-c:a pcm_s16be"),
        ("pcm_s24le", "-c:a pcm_s24le"),
        ("pcm_s24be", "-c:a pcm_s24be"),
        ("pcm_s32le", "-c:a pcm_s32le"),
        ("pcm_s32be", "-c:a pcm_s32be"),
        ("pcm_f32le", "-c:a pcm_f32le"),
        ("pcm_f32be", "-c:a pcm_f32be"),
        ("pcm_f64le", "-c:a pcm_f64le"),
        ("pcm_f64be", "-c:a pcm_f64be"),
        ("pcm_u8", "-c:a pcm_u8"),
        ("pcm_s8", "-c:a pcm_s8"),
        ("alaw", "-ar 8000 -c:a pcm_alaw"),
        ("mulaw", "-ar 8000 -c:a pcm_mulaw"),
        ("adpcm_ima_qt", "-c:a adpcm_ima_qt"),
        ("ilbc", "-ar 8000 -c:a ilbc_at"),
    ];
    // The video formats only the QuickTime flavour takes.
    let mov_only = [
        "prores", "dnxhd", "qtrle", "cfhd", "dv", "svq1", "v210", "h261",
    ];
    for ext in ["mp4", "mov", "3gp", "m4a", "ismv"] {
        // Smooth Streaming files carry signed composition offsets.
        let quirks: &[Quirk] = if ext == "ismv" {
            &[Quirk::SignedOffsets]
        } else {
            &[]
        };
        if ext != "m4a" {
            for (name, enc) in video {
                if ext != "mov" && mov_only.contains(name) {
                    continue;
                }
                let quirks = if ext == "mov" && *name == "mpeg2" {
                    &[Quirk::PartialSync]
                } else {
                    quirks
                };
                add(&format!("v_{name}"), ext, format!("{V} {enc}"), quirks);
            }
        }
        for (name, enc) in audio {
            add(&format!("a_{name}"), ext, format!("{A} {enc}"), quirks);
        }
    }

    // --- layouts on H.264 (B-frames or not) + AAC ------------------
    let av = format!("{V} {A} -c:v libx264 -bf 2 -g 10 -c:a aac");
    let av_nob = format!("{V} {A} -c:v libx264 -bf 0 -g 10 -c:a aac");
    use Quirk::*;
    let flags: &[(&str, &[Quirk])] = &[
        ("faststart", &[]),
        ("frag_keyframe", &[]),
        ("empty_moov", &[]),
        ("frag_keyframe+empty_moov", &[]),
        ("frag_keyframe+empty_moov+default_base_moof", &[]),
        ("frag_keyframe+separate_moof", &[]),
        (
            "frag_keyframe+empty_moov+separate_moof+default_base_moof",
            &[],
        ),
        ("frag_keyframe+empty_moov+omit_tfhd_offset", &[]),
        ("frag_keyframe+omit_tfhd_offset", &[]),
        ("dash", &[]),
        ("cmaf", &[SignedOffsets]),
        ("frag_keyframe+global_sidx", &[SidxKeyFrames]),
        ("dash+global_sidx", &[SidxKeyFrames]),
        ("cmaf+global_sidx", &[SignedOffsets, SidxKeyFrames]),
        ("skip_sidx+dash", &[]),
        ("negative_cts_offsets", &[SignedOffsets]),
        ("frag_keyframe+negative_cts_offsets", &[SignedOffsets]),
        (
            "negative_cts_offsets+frag_keyframe+empty_moov+default_base_moof",
            &[SignedOffsets],
        ),
        ("frag_keyframe+empty_moov+frag_discont", &[]),
        ("frag_every_frame", &[ZeroDurations]),
        ("frag_custom", &[]),
        ("frag_keyframe+empty_moov+skip_trailer", &[]),
        ("frag_keyframe+delay_moov", &[DiscardFlag]),
        ("dash+delay_moov", &[DiscardFlag]),
        ("cmaf+delay_moov", &[SignedOffsets, DiscardFlag]),
        ("isml+frag_keyframe", &[]),
        ("frag_keyframe+write_colr", &[]),
        ("hybrid_fragmented+frag_keyframe", &[]),
        ("disable_chpl", &[]),
        ("rtphint", &[]),
    ];
    for (f, quirks) in flags {
        let tag = f.replace('+', "_");
        add(
            &format!("lay_{tag}"),
            "mp4",
            format!("{av} -movflags {f}"),
            quirks,
        );
        add(
            &format!("layb_{tag}"),
            "mp4",
            format!("{av_nob} -movflags {f}"),
            quirks,
        );
    }
    let mut lay = |name: &str, ext: &'static str, args: String| {
        add(&format!("lay_{name}"), ext, args, &[]);
    };
    lay(
        "frag_duration",
        "mp4",
        format!("{av} -frag_duration 200000"),
    );
    lay("frag_size", "mp4", format!("{av} -frag_size 2000"));
    lay(
        "min_frag_duration",
        "mp4",
        format!("{av} -movflags frag_keyframe -min_frag_duration 500000"),
    );
    lay(
        "frag_interleave",
        "mp4",
        format!("{av} -movflags frag_keyframe+empty_moov -frag_interleave 3"),
    );
    lay("no_edit_list", "mp4", format!("{av} -use_editlist 0"));
    lay(
        "no_edit_list_frag",
        "mp4",
        format!("{av} -use_editlist 0 -movflags frag_keyframe+empty_moov"),
    );
    lay(
        "negative_ts_kept",
        "mp4",
        format!("{av} -avoid_negative_ts disabled"),
    );
    lay("start_1s", "mp4", format!("{av} -output_ts_offset 1"));
    lay(
        "start_1s_frag",
        "mp4",
        format!("{av} -output_ts_offset 1 -movflags frag_keyframe+empty_moov"),
    );
    let delayed = |video_first: bool, tail: &str| {
        let (v, a) = if video_first {
            (format!("-itsoffset 0.7 {V}"), A.to_string())
        } else {
            (V.to_string(), format!("-itsoffset 0.5 {A}"))
        };
        format!("{v} {a} -c:v libx264 -bf 2 -g 10 -c:a aac {tail}")
    };
    lay("audio_delay", "mp4", delayed(false, ""));
    lay("video_delay", "mp4", delayed(true, ""));
    lay(
        "video_delay_frag",
        "mp4",
        delayed(true, "-movflags frag_keyframe+empty_moov"),
    );
    let vfr = "-f lavfi -i testsrc2=s=64x48:r=25:d=2 \
               -vf select=not(mod(n\\,3))+gt(n\\,30) -fps_mode vfr -c:v libx264 -bf 2";
    lay("vfr", "mp4", vfr.to_string());
    lay(
        "vfr_frag",
        "mp4",
        format!("{vfr} -movflags frag_keyframe+empty_moov"),
    );
    lay(
        "ntsc",
        "mp4",
        "-f lavfi -i testsrc2=s=64x48:r=30000/1001:d=1 -c:v libx264 -bf 2".into(),
    );
    lay("one_frame", "mp4", format!("{V} -frames:v 1 -c:v libx264"));
    lay(
        "one_frame_frag",
        "mp4",
        format!("{V} -frames:v 1 -c:v libx264 -movflags frag_keyframe+empty_moov"),
    );
    lay("one_packet", "mp4", format!("{A} -c:a aac -frames:a 1"));
    let long = "-f lavfi -i testsrc2=s=64x48:r=25:d=120 -f lavfi -i sine=f=440:d=120 \
                -c:v libx264 -preset ultrafast -bf 2 -g 50 -c:a aac";
    lay("long", "mp4", long.to_string());
    lay(
        "long_frag",
        "mp4",
        format!("{long} -movflags frag_keyframe+empty_moov+default_base_moof"),
    );
    lay(
        "rotate_90",
        "mp4",
        format!("-display_rotation 90 {V} -c:v libx264"),
    );
    lay(
        "sample_aspect",
        "mp4",
        format!("{V} -vf setsar=4/3 -c:v libx264"),
    );
    let colour = "-color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv";
    lay(
        "colr",
        "mp4",
        format!("{V} -c:v libx264 {colour} -movflags write_colr"),
    );
    lay("colr", "mov", format!("{V} -c:v libx264 {colour}"));
    lay(
        "clean_aperture",
        "mov",
        format!("{V} -c:v prores_ks -vf crop=60:40"),
    );
    lay(
        "gamma",
        "mov",
        format!("{V} -c:v libx264 -movflags write_gama"),
    );
    lay(
        "interlaced",
        "mov",
        format!("{V} -c:v mpeg2video -flags +ildct+ilme"),
    );
    let four = format!(
        "{V} {A} -f lavfi -i sine=f=880:d=1 -f lavfi -i testsrc=s=32x32:r=10:d=1 \
         -map 0 -map 1 -map 2 -map 3 -c:v libx264 -c:a aac"
    );
    lay(
        "four_tracks",
        "mp4",
        format!("{four} -metadata:s:a:0 language=eng -metadata:s:a:1 language=fra"),
    );
    lay(
        "four_tracks_frag",
        "mp4",
        format!("{four} -movflags frag_keyframe+empty_moov+default_base_moof"),
    );
    lay(
        "timecode",
        "mp4",
        format!("{V} -c:v libx264 -timecode 01:00:00:00"),
    );
    lay(
        "timecode",
        "mov",
        format!("{V} -c:v libx264 -timecode 01:00:00:00"),
    );
    lay("no_timecode", "mov", format!("{av} -write_tmcd 0"));
    let sine = |rate: u32, tail: &str| format!("-f lavfi -i sine=f=440:d=1:r={rate} {tail}");
    lay("aac_6ch", "mp4", sine(44100, "-ac 6 -c:a aac"));
    lay("aac_8ch", "mp4", sine(44100, "-ac 8 -c:a aac"));
    lay("aac_48k", "mp4", sine(48000, "-c:a aac"));
    lay("aac_96k", "mp4", sine(96000, "-c:a aac"));
    lay("aac_8k", "mp4", sine(8000, "-c:a aac"));
    lay("ac3_6ch", "mp4", sine(48000, "-ac 6 -c:a ac3"));
    lay("eac3_6ch", "mp4", sine(48000, "-ac 6 -c:a eac3"));
    lay("opus_6ch", "mp4", sine(48000, "-ac 6 -c:a libopus"));
    lay(
        "flac_6ch_96k",
        "mp4",
        sine(96000, "-ac 6 -sample_fmt s32 -c:a flac -strict -2"),
    );
    lay(
        "alac_24bit",
        "mp4",
        sine(96000, "-ac 2 -sample_fmt s32p -c:a alac"),
    );
    lay("pcm_6ch", "mov", sine(48000, "-ac 6 -c:a pcm_s24le"));
    lay("pcm_6ch", "mp4", sine(48000, "-ac 6 -c:a pcm_s24le"));
    lay(
        "h264_10bit",
        "mp4",
        format!("{V} -pix_fmt yuv420p10le -c:v libx264"),
    );
    lay(
        "hevc_10bit",
        "mp4",
        format!("{V} -pix_fmt yuv420p10le -c:v libx265 -x265-params log-level=error"),
    );
    let frag = "-movflags frag_keyframe+empty_moov";
    lay(
        "hevc_frag",
        "mp4",
        format!("{V} -c:v libx265 -x265-params log-level=error -g 10 {frag}"),
    );
    lay(
        "av1_frag",
        "mp4",
        format!("{V} -c:v libsvtav1 -svtav1-params lp=1 -g 10 {frag}"),
    );
    lay(
        "vp9_frag",
        "mp4",
        format!("{V} -c:v libvpx-vp9 -g 10 -cpu-used 8 {frag}"),
    );
    lay(
        "mpeg4_frag",
        "mp4",
        format!("{V} -c:v mpeg4 -bf 2 -g 10 {frag}"),
    );
    lay("opus_frag", "mp4", format!("{A} -c:a libopus {frag}"));
    let cut = "-movflags empty_moov -frag_duration 200000";
    lay(
        "flac_frag",
        "mp4",
        format!("{A} -c:a flac -strict -2 {cut}"),
    );
    lay("aac_frag", "mp4", format!("{A} -c:a aac {cut}"));
    lay("mp3_frag", "mp4", format!("{A} -c:a libmp3lame {cut}"));
    lay("pcm_frag", "mp4", format!("{A} -c:a pcm_s16le {cut}"));
    lay("ac3_frag", "mp4", format!("{A} -c:a ac3 {cut}"));
    lay(
        "aac_cmaf",
        "mp4",
        format!("{A} -c:a aac -movflags cmaf -frag_duration 200000"),
    );
    lay(
        "large_odd",
        "mp4",
        "-f lavfi -i testsrc2=s=1921x1081:r=25:d=0.2 -pix_fmt yuv444p \
         -c:v libx264 -preset ultrafast"
            .into(),
    );
    lay(
        "tiny_odd",
        "mp4",
        "-f lavfi -i testsrc2=s=17x9:r=25:d=0.2 -pix_fmt yuv444p -c:v libx264".into(),
    );
    lay(
        "chunk_duration",
        "mp4",
        format!("{av} -chunk_duration 500000"),
    );
    lay("chunk_size", "mp4", format!("{av} -chunk_size 4096"));
    lay("frag", "mov", format!("{av} {frag}"));
    lay("faststart", "mov", format!("{av} -movflags faststart"));
    lay("btrt", "mp4", format!("{av} -write_btrt 1"));
    lay("prft", "mp4", format!("{av} -write_prft wallclock {frag}"));
    lay(
        "hevc_bframes",
        "mp4",
        format!("{V} -c:v libx265 -x265-params log-level=error:bframes=3 -g 25"),
    );
    lay(
        "hw_h264",
        "mp4",
        format!("{V} -s 128x96 -c:v h264_videotoolbox"),
    );
    lay(
        "hw_hevc",
        "mp4",
        format!("{V} -s 128x96 -c:v hevc_videotoolbox -tag:v hvc1"),
    );
    lay("aac_other_encoder", "mp4", format!("{A} -c:a aac_at"));
    lay("alac_other_encoder", "m4a", format!("{A} -c:a alac_at"));
    lay(
        "movie_timescale",
        "mp4",
        format!("{av} -movie_timescale 90000 -video_track_timescale 90000"),
    );
    add(
        "lay_hevc_bframes_cmaf",
        "mp4",
        format!("{V} -c:v libx265 -x265-params log-level=error:bframes=3 -g 25 -movflags cmaf"),
        &[SignedOffsets],
    );

    // --- fragmenting flags in every combination ---------------------
    for cut in ["frag_keyframe", "frag_keyframe+empty_moov", "dash", "cmaf"] {
        for (n, extra) in [
            "default_base_moof",
            "separate_moof",
            "omit_tfhd_offset",
            "separate_moof+default_base_moof",
            "separate_moof+omit_tfhd_offset",
            "global_sidx+separate_moof",
            "frag_discont+default_base_moof",
            "negative_cts_offsets+separate_moof",
        ]
        .into_iter()
        .enumerate()
        {
            let mut quirks = Vec::new();
            if cut == "cmaf" || extra.contains("negative_cts") {
                quirks.push(SignedOffsets);
            }
            if extra.contains("global_sidx") {
                quirks.push(SidxKeyFrames);
            }
            let tag = cut.replace('+', "_");
            add(
                &format!("combo_{tag}_{n}"),
                "mp4",
                format!("{av} -movflags {cut}+{extra}"),
                &quirks,
            );
            add(
                &format!("combob_{tag}_{n}"),
                "mp4",
                format!("{av_nob} -movflags {cut}+{extra}"),
                &quirks,
            );
        }
    }

    // --- audio: rates × channel counts ------------------------------
    for (name, enc) in [
        ("aac", "-c:a aac"),
        ("mp3", "-c:a libmp3lame"),
        ("ac3", "-c:a ac3"),
        ("eac3", "-c:a eac3"),
        ("opus", "-c:a libopus"),
        ("flac", "-c:a flac -strict -2"),
        ("alac", "-c:a alac"),
        ("pcm24", "-c:a pcm_s24le"),
        ("pcmf32be", "-c:a pcm_f32be"),
    ] {
        for rate in [8000, 22050, 32000, 48000, 88200, 96000, 192000] {
            for channels in [1, 2, 6] {
                for ext in ["mp4", "mov"] {
                    add(
                        &format!("rate_{name}_{rate}_{channels}"),
                        ext,
                        format!("-f lavfi -i sine=f=440:d=0.5:r={rate} -ac {channels} {enc}"),
                        &[],
                    );
                }
            }
        }
    }

    // --- video: sizes × pixel formats × GOP shapes ------------------
    for (name, enc) in [
        ("h264", "-c:v libx264 -preset ultrafast"),
        (
            "hevc",
            "-c:v libx265 -preset ultrafast -x265-params log-level=error",
        ),
        ("mpeg4", "-c:v mpeg4"),
        ("mjpeg", "-c:v mjpeg"),
        ("vp9", "-c:v libvpx-vp9 -cpu-used 8"),
        ("prores", "-c:v prores_ks"),
    ] {
        for size in ["16x16", "33x17", "640x360", "1280x720"] {
            for (g, gop) in ["-g 1", "-g 5 -bf 0", "-g 12 -bf 3", "-g 300 -bf 2"]
                .into_iter()
                .enumerate()
            {
                for ext in ["mp4", "mov"] {
                    add(
                        &format!("size_{name}_{size}_{g}"),
                        ext,
                        format!("-f lavfi -i testsrc2=s={size}:r=30:d=0.5 {enc} {gop}"),
                        &[],
                    );
                }
            }
        }
    }
    for pix in [
        "yuv420p",
        "yuv422p",
        "yuv444p",
        "yuv420p10le",
        "yuv422p10le",
        "gray",
    ] {
        for (name, enc) in [
            ("h264", "-c:v libx264 -preset ultrafast"),
            (
                "hevc",
                "-c:v libx265 -preset ultrafast -x265-params log-level=error",
            ),
            ("vp9", "-c:v libvpx-vp9 -cpu-used 8"),
            ("av1", "-c:v libsvtav1 -svtav1-params lp=1"),
        ] {
            add(
                &format!("pix_{name}_{pix}"),
                "mp4",
                format!("-f lavfi -i testsrc2=s=64x64:r=25:d=0.2 -pix_fmt {pix} {enc}"),
                &[],
            );
        }
    }

    // --- protected (ISO/IEC 23001-7 `cenc`), key known --------------
    let enc = "-encryption_scheme cenc-aes-ctr \
               -encryption_key 00112233445566778899aabbccddeeff \
               -encryption_kid a0a1a2a3a4a5a6a7a8a9aaabacadaeaf";
    add("enc_cenc", "mp4", format!("{av} {enc}"), &[]);
    add(
        "enc_cenc_frag",
        "mp4",
        format!("{av} {enc} -movflags frag_keyframe+empty_moov+default_base_moof"),
        &[],
    );
    add(
        "enc_cenc_hevc",
        "mp4",
        format!("{V} -c:v libx265 -x265-params log-level=error {enc}"),
        &[],
    );
    out
}

/// Cases that need generated side inputs (subtitles, chapters, a cover
/// image), which live in the scratch directory.
fn side_input_cases(dir: &Path) -> Vec<Case> {
    let srt = dir.join("in.srt");
    std::fs::write(
        &srt,
        "1\n00:00:00,100 --> 00:00:00,400\nHello\n\n\
         2\n00:00:00,600 --> 00:00:00,900\nWorld <b>bold</b>\n",
    )
    .unwrap();
    let chapters = dir.join("in.ffmeta");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\ntitle=T\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=400\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=400\nEND=1000\ntitle=Two\n",
    )
    .unwrap();
    let cover = dir.join("in.png");
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=s=32x32:d=0.04")
        .args(["-frames:v", "1"])
        .arg(&cover)
        .status();
    let (srt, chapters, cover) = (srt.display(), chapters.display(), cover.display());
    let mut out = Vec::new();
    let mut add = |name: &str, ext: &'static str, args: String| {
        out.push(Case {
            name: format!("{name}.{ext}"),
            ext,
            args,
            quirks: if ext == "ismv" {
                vec![Quirk::SignedOffsets]
            } else {
                Vec::new()
            },
        });
    };
    let text = format!("{V} -i {srt} -c:v libx264 -c:s mov_text");
    add("sub_text", "mp4", text.clone());
    add("sub_text", "mov", text.clone());
    add(
        "sub_text_frag",
        "mp4",
        format!("{text} -movflags frag_keyframe+empty_moov"),
    );
    add("sub_text_only", "mp4", format!("-i {srt} -c:s mov_text"));
    add(
        "sub_ttml",
        "ismv",
        format!("{V} -i {srt} -c:v libx264 -c:s ttml"),
    );
    let chap = format!("{V} {A} -i {chapters} -map 0 -map 1 -map_chapters 2 -c:v libx264 -c:a aac");
    add("chapters", "mp4", chap.clone());
    add("chapters", "mov", chap);
    add(
        "chapters",
        "m4a",
        format!("{A} -i {chapters} -map 0 -map_chapters 1 -c:a aac"),
    );
    if made.is_ok_and(|s| s.success()) {
        add(
            "cover",
            "m4a",
            format!("{A} -i {cover} -map 0 -map 1 -c:a aac -c:v copy -disposition:v attached_pic"),
        );
        add(
            "cover_video",
            "mp4",
            format!(
                "{V} {A} -i {cover} -map 0 -map 1 -map 2 -c:v:0 libx264 -c:a aac \
                 -c:v:1 copy -disposition:v:1 attached_pic"
            ),
        );
    }
    add(
        "tags",
        "m4a",
        format!("{A} -c:a aac -metadata title=T -metadata artist=Ar -metadata album=Al"),
    );
    out
}

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

#[derive(Clone, Debug, PartialEq)]
struct Pkt {
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    key: bool,
    discard: bool,
    hash: u32,
}

#[derive(Default, Debug)]
struct Stream {
    codec: String,
    kind: String,
    tag: String,
    time_base: String,
    width: u32,
    height: u32,
    sample_rate: u32,
    channels: u32,
    start: Option<i64>,
    extradata: Option<(usize, u32)>,
    attached_pic: bool,
    packets: Vec<Pkt>,
}

fn ffprobe(path: &Path, extra: &[&str]) -> Option<BTreeMap<u32, Stream>> {
    let run = |args: &[&str]| {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_data_hash", "adler32"])
            .args(extra)
            .args(args)
            .arg(path)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let mut streams: BTreeMap<u32, Stream> = BTreeMap::new();
    let entries = "stream=index,codec_name,codec_type,codec_tag_string,width,height,sample_rate,\
                   channels,time_base,start_pts,extradata_size,extradata_hash\
                   :stream_disposition=attached_pic";
    for line in run(&["-show_entries", entries, "-of", "flat"])?.lines() {
        let Some(rest) = line.strip_prefix("streams.stream.") else {
            continue;
        };
        let Some((index, kv)) = rest.split_once('.') else {
            continue;
        };
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        let s = streams.entry(index.parse().ok()?).or_default();
        match key {
            "codec_name" => s.codec = value.into(),
            "codec_type" => s.kind = value.into(),
            "codec_tag_string" => s.tag = value.into(),
            "time_base" => s.time_base = value.into(),
            "width" => s.width = value.parse().unwrap_or(0),
            "height" => s.height = value.parse().unwrap_or(0),
            "sample_rate" => s.sample_rate = value.parse().unwrap_or(0),
            "channels" => s.channels = value.parse().unwrap_or(0),
            "start_pts" => s.start = value.parse().ok(),
            "extradata_size" => {
                let size = value.parse().unwrap_or(0);
                s.extradata = Some((size, s.extradata.map_or(1, |e| e.1)));
            }
            "extradata_hash" => {
                let hash = u32::from_str_radix(value.trim_start_matches("adler32:"), 16).ok()?;
                s.extradata = Some((s.extradata.map_or(0, |e| e.0), hash));
            }
            "disposition.attached_pic" => s.attached_pic = value == "1",
            _ => {}
        }
    }
    let entries = "packet=stream_index,pts,dts,duration,size,flags,data_hash";
    for line in run(&["-show_entries", entries, "-of", "csv=p=0"])?.lines() {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 7 {
            continue;
        }
        let flags = f[5].as_bytes();
        streams
            .entry(f[0].parse().ok()?)
            .or_default()
            .packets
            .push(Pkt {
                pts: f[1].parse().ok(),
                dts: f[2].parse().ok(),
                duration: f[3].parse().ok(),
                size: f[4].parse().ok()?,
                key: flags.first() == Some(&b'K'),
                discard: flags.get(1) == Some(&b'D'),
                hash: u32::from_str_radix(f[6].trim_start_matches("adler32:"), 16).ok()?,
            });
    }
    Some(streams)
}

struct Ours {
    streams: Vec<oxideav_core::StreamInfo>,
    packets: BTreeMap<u32, Vec<(Pkt, Vec<u8>)>>,
    pictures: usize,
    chapters: Vec<String>,
}

fn demux(path: &Path) -> Result<Ours, String> {
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut d =
        oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).map_err(|e| e.to_string())?;
    let streams = d.streams().to_vec();
    let mut packets: BTreeMap<u32, Vec<(Pkt, Vec<u8>)>> = BTreeMap::new();
    loop {
        match d.next_packet() {
            Ok(p) => packets.entry(p.stream_index).or_default().push((
                Pkt {
                    pts: p.pts,
                    dts: p.dts,
                    duration: p.duration,
                    size: p.data.len(),
                    key: p.flags.keyframe,
                    discard: p.flags.discard,
                    hash: adler32(&p.data),
                },
                p.data,
            )),
            Err(e) if e.is_eof() => break,
            Err(e) => return Err(format!("next_packet: {e}")),
        }
    }
    Ok(Ours {
        streams,
        packets,
        pictures: d.attached_pictures().len(),
        chapters: d
            .chapters()
            .iter()
            .map(|c| c.title.clone().unwrap_or_default())
            .collect(),
    })
}

/// This crate's codec id for the reader's codec name; `None` for a
/// format the static table does not map (it is then `mp4:<fourcc>`,
/// `mp4a:0x<oti>` or `mp4v:0x<oti>`).
fn expected_codec(theirs: &Stream, ext: &str) -> Option<String> {
    let name = theirs.codec.as_str();
    // Big-endian PCM is delivered little-endian.
    if let Some(pcm) = name.strip_prefix("pcm_")
        && let Some(layout) = pcm.strip_suffix("be")
    {
        return Some(format!("pcm_{layout}le"));
    }
    let id = match name {
        // QuickTime text is not 3GPP timed text.
        "mov_text" if theirs.tag == "text" => "text",
        "h264" | "av1" | "vp9" | "vp8" | "mpeg1video" | "mpeg2video" | "mjpeg" | "h263"
        | "prores" | "aac" | "mp3" | "ac3" | "eac3" | "opus" | "flac" | "alac" | "dts"
        | "mov_text" | "qtrle" | "svq1" | "dvvideo" | "adpcm_ima_qt" | "jpeg2000" => name,
        "hevc" => "h265",
        "mpeg4" => "mpeg4video",
        // PNG has an `mp4v` object type from the registration authority
        // only; the QuickTime `png ` compression type is documented.
        "png" if theirs.tag == "png " => "png",
        // MPEG audio layer II shares the layer III object types.
        "mp2" if theirs.tag == "mp4a" => "mp3",
        name if name.starts_with("pcm_") => name,
        "ttml" => "ttml",
        _ => return None,
    };
    // The QuickTime flavour writes its own tags for some of these.
    if ext == "mov" && matches!(name, "mpeg1video" | "mpeg2video" | "jpeg2000") {
        return None;
    }
    Some(id.into())
}

fn unmapped(id: &str) -> bool {
    id.starts_with("mp4:") || id.starts_with("mp4a:0x") || id.starts_with("mp4v:0x")
}

/// Compare one file; returns the differences found.
fn compare(case: &Case, theirs: &BTreeMap<u32, Stream>, ours: &Ours) -> Vec<String> {
    let mut issues = Vec::new();
    let has = |q: Quirk| case.quirks.contains(&q);
    // An attached picture is a stream to the reader and an
    // `attached_pictures()` entry here.
    let pics = theirs.values().filter(|s| s.attached_pic).count();
    if ours.pictures != pics {
        issues.push(format!("pictures: theirs {pics} ours {}", ours.pictures));
    }
    let tracks = theirs.values().filter(|s| !s.attached_pic).count();
    if ours.streams.len() != tracks {
        issues.push(format!(
            "stream count: theirs {tracks} ours {}",
            ours.streams.len()
        ));
    }
    for (&i, t) in theirs {
        if t.attached_pic {
            continue;
        }
        let Some(o) = ours.streams.get(i as usize) else {
            continue;
        };
        let p = &o.params;
        let id = p.codec_id.as_str();
        match expected_codec(t, case.ext) {
            Some(want) if want != id => {
                issues.push(format!("s{i} codec: theirs {} ours {id}", t.codec))
            }
            None if !unmapped(id) && !matches!(t.kind.as_str(), "data" | "subtitle") => issues
                .push(format!(
                    "s{i} codec: theirs {} ({}) ours {id}",
                    t.codec, t.tag
                )),
            _ => {}
        }
        let tb = format!("{}/{}", o.time_base.0.num, o.time_base.0.den);
        if tb != t.time_base {
            issues.push(format!("s{i} time base: theirs {} ours {tb}", t.time_base));
        }
        let timed = matches!(t.kind.as_str(), "video" | "audio");
        if t.kind == "video" && (p.width, p.height) != (Some(t.width), Some(t.height)) {
            issues.push(format!(
                "s{i} size: theirs {}x{} ours {:?}x{:?}",
                t.width, t.height, p.width, p.height
            ));
        }
        // MLP / TrueHD: the `mlpa` entry's rate field is not the 16.16
        // one and has no documentation here.
        if t.kind == "audio" && t.codec != "truehd" {
            if p.sample_rate != Some(t.sample_rate) {
                issues.push(format!(
                    "s{i} rate: theirs {} ours {:?}",
                    t.sample_rate, p.sample_rate
                ));
            }
            if p.channels.map(u32::from) != Some(t.channels) {
                issues.push(format!(
                    "s{i} channels: theirs {} ours {:?}",
                    t.channels, p.channels
                ));
            }
        }
        // Configuration records, where the container holds one and
        // both sides hand it over in the same framing.
        let (len, hash) = (p.extradata.len(), adler32(&p.extradata));
        let same_record = match id {
            "h264" | "h265" | "av1" | "aac" | "mpeg4video" | "opus" | "mov_text" => {
                t.extradata.is_none_or(|e| e == (len, hash))
            }
            // The reader hands over the STREAMINFO block; this crate
            // the metadata block with its 4-byte header.
            "flac" => len >= 4 && t.extradata == Some((len - 4, adler32(&p.extradata[4..]))),
            // The reader hands over the whole `alac` box; this crate
            // the cookie inside it (12 bytes of box and FullBox header).
            "alac" => t.extradata.is_some_and(|e| e.0 == len + 12),
            _ => true,
        };
        if !same_record {
            issues.push(format!(
                "s{i} extradata: theirs {:?} ours ({len}, {hash:#x})",
                t.extradata
            ));
        }
        if timed && !has(Quirk::SignedOffsets) && o.start_time != t.start {
            issues.push(format!(
                "s{i} start: theirs {:?} ours {:?}",
                t.start, o.start_time
            ));
        }

        // --- packets ----------------------------------------------
        if !matches!(t.kind.as_str(), "video" | "audio" | "subtitle") {
            // Hint, timecode and chapter tracks: the reader consumes or
            // re-labels them; this crate delivers the samples.
            continue;
        }
        let empty = Vec::new();
        let mut mine: Vec<&(Pkt, Vec<u8>)> =
            ours.packets.get(&i).unwrap_or(&empty).iter().collect();
        if t.kind == "subtitle" {
            // A text track ends in an empty zero-duration sample the
            // reader drops.
            while mine
                .last()
                .is_some_and(|(p, _)| p.duration == Some(0) && mine.len() > t.packets.len())
            {
                mine.pop();
            }
        }
        if mine.len() != t.packets.len() {
            issues.push(format!(
                "s{i} packets: theirs {} ours {}",
                t.packets.len(),
                mine.len()
            ));
            continue;
        }
        // Width of a big-endian PCM sample this crate swapped.
        let swap = t
            .codec
            .strip_prefix("pcm_")
            .filter(|l| l.ends_with("be"))
            .map(|l| l[1..l.len() - 2].parse::<usize>().unwrap_or(8) / 8)
            .unwrap_or(0);
        // The reader's parser recomputes these codecs' durations from
        // the frame headers; the last sample's table duration is the
        // shortened one.
        let parsed = matches!(t.codec.as_str(), "mp3" | "mp2" | "ac3" | "eac3");
        let shift = |a: Option<i64>, b: Option<i64>| Some(a? - b?);
        // Signed offsets: the reader's constant goes onto the pts in
        // one part of a file and off the dts in another, so it is the
        // composition offset (pts - dts) that differs by a constant,
        // with one of the two timestamps equal.
        let offset_shift = |a: &Pkt, b: &Pkt| Some(shift(a.pts, a.dts)? - shift(b.pts, b.dts)?);
        let first_shift = match (t.packets.first(), mine.first()) {
            (Some(a), Some((b, _))) => offset_shift(a, b),
            _ => Some(0),
        };
        let mut bad = 0;
        for (n, (a, (b, data))) in t.packets.iter().zip(mine.iter()).enumerate() {
            let last = n + 1 == t.packets.len();
            let hash = if swap > 1 {
                let stored: Vec<u8> = data
                    .chunks(swap)
                    .flat_map(|s| s.iter().rev().copied())
                    .collect();
                adler32(&stored)
            } else {
                b.hash
            };
            let mut same = a.size == b.size && a.hash == hash;
            if has(Quirk::SignedOffsets) {
                same &= offset_shift(a, b) == first_shift && (a.pts == b.pts || a.dts == b.dts);
            } else {
                same &= a.pts == b.pts && a.dts == b.dts;
            }
            if !(has(Quirk::ZeroDurations) || a.duration.is_none() || parsed && last) {
                same &= a.duration == b.duration;
            }
            if has(Quirk::PartialSync) {
                same &= a.key || !b.key;
            } else if !(t.kind == "subtitle" || t.kind == "video" && has(Quirk::SidxKeyFrames)) {
                same &= a.key == b.key;
            }
            if !(has(Quirk::DiscardFlag) || has(Quirk::ZeroDurations)) {
                same &= a.discard == b.discard;
            }
            if !same {
                bad += 1;
                if bad <= 2 {
                    issues.push(format!("s{i} packet {n}: theirs {a:?} ours {b:?}"));
                }
            }
        }
        if bad > 2 {
            issues.push(format!("s{i}: {bad} packets differ"));
        }
    }
    issues
}

enum Outcome {
    /// The tool did not write this combination.
    NotWritten,
    Pass,
    Fail(Vec<String>),
}

fn run_case(dir: &Path, case: &Case) -> Outcome {
    let path = dir.join(&case.name);
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin"])
        .args(case.args.split_whitespace())
        .args(["-bitexact", "-t", "150"])
        .arg(&path)
        .output();
    let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if !made.is_ok_and(|o| o.status.success()) || written == 0 {
        return Outcome::NotWritten;
    }
    let Some(theirs) = ffprobe(&path, &[]) else {
        return Outcome::NotWritten;
    };
    if theirs.is_empty() {
        return Outcome::NotWritten;
    }
    let ours = match demux(&path) {
        Ok(o) => o,
        Err(e) => return Outcome::Fail(vec![format!("demux: {e}")]),
    };
    let mut issues = compare(case, &theirs, &ours);
    if case.name.starts_with("chapters") && ours.chapters != ["One", "Two"] {
        issues.push(format!("chapters: {:?}", ours.chapters));
    }
    // A protected track: the media type and the original format are
    // recovered, the scheme is reported.
    if case.name.starts_with("enc_") {
        for s in &ours.streams {
            if s.params.options.get("protection_scheme") != Some("cenc") {
                issues.push(format!("s{}: no protection scheme", s.index));
            }
            if s.params.media_type == MediaType::Data {
                issues.push(format!("s{}: media type lost", s.index));
            }
        }
    }
    if issues.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(issues)
    }
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
fn foreign_files_demux_like_the_reference_reader() {
    let (Some(major), Some(_)) = (tool_major("ffmpeg"), tool_major("ffprobe")) else {
        eprintln!("skipped: ffmpeg / ffprobe not found");
        return;
    };
    let scratch =
        Scratch(std::env::temp_dir().join(format!("oxideav-mp4-matrix-{}", std::process::id())));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let dir = scratch.0.as_path();
    let filter = std::env::var("OXIDEAV_MP4_MATRIX").unwrap_or_default();
    let mut all = cases();
    all.extend(side_input_cases(dir));
    all.retain(|c| c.name.contains(&filter));

    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = all.get(i) else {
                        break;
                    };
                    let outcome = run_case(dir, case);
                    results.lock().unwrap().push((i, outcome));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);

    let (mut pass, mut skipped, mut failed) = (0, 0, 0);
    for (i, outcome) in &results {
        match outcome {
            Outcome::Pass => pass += 1,
            Outcome::NotWritten => skipped += 1,
            Outcome::Fail(issues) => {
                failed += 1;
                eprintln!("FAIL {}", all[*i].name);
                for issue in issues {
                    eprintln!("    {issue}");
                }
            }
        }
    }
    eprintln!(
        "black-box matrix: {pass} of {} files agree ({failed} differ); \
         {skipped} combinations the tool did not write",
        pass + failed
    );
    if major < VALIDATED_MAJOR {
        eprintln!(
            "tool version {major} is older than the validated {VALIDATED_MAJOR}: not asserted"
        );
        return;
    }
    assert_eq!(failed, 0, "{failed} files differ from the reference reader");
    if filter.is_empty() {
        assert!(pass >= 600, "only {pass} files were compared");
    }
}
