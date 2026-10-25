//! Every video stream's `CodecParameters` carries the decoded-picture
//! layout, output size and nominal frame rate its configuration record
//! and sample tables state. Fixtures are 2–3 frame clips written by
//! black-box ffmpeg encoders (`tests/fixtures/params/gen.sh`); each
//! expected row is what `ffprobe -show_streams` reports for the file
//! (`pix_fmt`, `width`, `height`, `r_frame_rate`).
use oxideav_core::{PixelFormat, Rational, RuntimeContext};

fn params(name: &str) -> oxideav_core::CodecParameters {
    let path = format!(
        "{}/tests/fixtures/params/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(&path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    oxideav_core::Demuxer::streams(&d)[0].params.clone()
}

/// `(file, codec, pix_fmt, width, height, r_frame_rate)`.
type Row = (
    &'static str,
    &'static str,
    PixelFormat,
    u32,
    u32,
    (i64, i64),
);

#[test]
fn video_params_match_ffprobe() {
    use PixelFormat::*;
    let rows: &[Row] = &[
        ("h264-yuv420p.mp4", "h264", Yuv420P, 32, 18, (25, 1)),
        ("h264-yuv420p10le.mp4", "h264", Yuv420P10Le, 32, 18, (25, 1)),
        ("h264-yuv422p.mp4", "h264", Yuv422P, 32, 18, (25, 1)),
        ("h264-yuv444p.mp4", "h264", Yuv444P, 32, 18, (25, 1)),
        ("h264-ntsc.mp4", "h264", Yuv420P, 32, 18, (30000, 1001)),
        ("hevc-gray.mp4", "h265", Gray8, 32, 18, (25, 1)),
        ("hevc-yuv420p.mp4", "h265", Yuv420P, 32, 18, (25, 1)),
        ("hevc-yuv420p10le.mp4", "h265", Yuv420P10Le, 32, 18, (25, 1)),
        ("hevc-yuv422p10le.mp4", "h265", Yuv422P10Le, 32, 18, (25, 1)),
        ("hevc-yuv444p.mp4", "h265", Yuv444P, 32, 18, (25, 1)),
        ("vp9-yuv420p.mp4", "vp9", Yuv420P, 32, 18, (25, 1)),
        ("vp9-yuv420p10le.mp4", "vp9", Yuv420P10Le, 32, 18, (25, 1)),
        ("vp9-yuv422p.mp4", "vp9", Yuv422P, 32, 18, (25, 1)),
        ("vp9-yuv444p.mp4", "vp9", Yuv444P, 32, 18, (25, 1)),
        ("av1-yuv420p.mp4", "av1", Yuv420P, 64, 64, (25, 1)),
        ("av1-yuv420p10le.mp4", "av1", Yuv420P10Le, 64, 64, (25, 1)),
    ];
    for &(file, codec, pf, w, h, (num, den)) in rows {
        let p = params(file);
        assert_eq!(p.codec_id.as_str(), codec, "{file}");
        assert_eq!(p.pixel_format, Some(pf), "{file}");
        assert_eq!((p.width, p.height), (Some(w), Some(h)), "{file}");
        let fr = p
            .frame_rate
            .unwrap_or_else(|| panic!("{file}: no frame rate"));
        assert_eq!((fr.num, fr.den), (num, den), "{file}");
        assert!(fr.equals_value(&Rational::new(num, den)), "{file}");
    }
}
