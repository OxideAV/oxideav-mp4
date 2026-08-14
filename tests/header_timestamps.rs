//! Integration tests for the header timestamps (ISO/IEC 14496-12
//! §8.2.2 `mvhd`, §8.3.2 `tkhd`, §8.4.2 `mdhd`), read and write.
//!
//! What is asserted:
//!
//! * the default (`creation_time: None`) still writes the historical
//!   all-zero, version-0 headers,
//! * all six fields the demuxer can report are independently
//!   settable, and a per-track entry replaces the pair it names
//!   rather than inheriting either half, `media` unset reusing the
//!   track's pair being the sole defaulting step,
//! * a timestamp past the 32-bit horizon (2040-02-06) in either half
//!   promotes its box to version 1 and the file still demuxes — on
//!   the plain, faststart and fragmented paths, the last two being
//!   where the extra 36 bytes could disturb chunk offsets or the
//!   sealed `mehd` patch,
//! * an out-of-range `stream_index` fails at `open`,
//! * a stamped file round-trips back through the typed accessors and
//!   flat metadata keys, while an unstamped one reads as unset and
//!   emits no keys.
//!
//! A PATH-gated black-box cross-check has `ffmpeg` — an independent
//! writer — stamp a file and asserts our reader recovers the same
//! instant, which a self-round-trip cannot establish.

use oxideav_core::{CodecId, CodecParameters, Demuxer, Packet, SampleFormat, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};
// Deliberately through the crate-root re-exports, which are the paths
// the README hands callers — importing from `demux::` / `options::`
// here would leave those `pub use`s unexercised by any compiled code.
use oxideav_mp4::{
    mp4_secs_from_system_time, mp4_secs_from_unix_secs, unix_secs_from_mp4_secs, FragmentedOptions,
    Mp4MuxerOptions, MP4_EPOCH_OFFSET_SECS,
};
use oxideav_mp4::{HeaderTimestamps, TrackHeaderTimestamps};

/// The pair a single-value stamp produces: the same instant in both
/// `creation_time` and `modification_time`.
fn stamped(t: u64) -> HeaderTimestamps {
    HeaderTimestamps {
        creation_time: t,
        modification_time: t,
    }
}

/// A distinct pair, for asserting the two fields stay apart.
fn pair(creation_time: u64, modification_time: u64) -> HeaderTimestamps {
    HeaderTimestamps {
        creation_time,
        modification_time,
    }
}

/// The common per-track override: one instant in both halves of the
/// `tkhd` pair, with `mdhd` reusing it.
fn track_time(stream_index: usize, creation_time: u64) -> TrackHeaderTimestamps {
    TrackHeaderTimestamps {
        stream_index,
        track: stamped(creation_time),
        media: None,
    }
}

// --- Box walking ---------------------------------------------------------

/// Split a buffer of concatenated boxes into `(fourcc, body offset
/// within `buf`, body)` triples. Structural, not a FourCC grep: a
/// `tkhd` byte pattern inside sample payload must not be mistaken for
/// a box header.
fn boxes(buf: &[u8]) -> Vec<([u8; 4], usize, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= buf.len() {
        let size = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let fourcc = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        // §4.2 size forms: 1 = 64-bit largesize follows, 0 = to EOF.
        let (header, total) = match size {
            1 => {
                if i + 16 > buf.len() {
                    break;
                }
                let large = u64::from_be_bytes([
                    buf[i + 8],
                    buf[i + 9],
                    buf[i + 10],
                    buf[i + 11],
                    buf[i + 12],
                    buf[i + 13],
                    buf[i + 14],
                    buf[i + 15],
                ]) as usize;
                (16, large)
            }
            0 => (8, buf.len() - i),
            n => (8, n),
        };
        if total < header || i + total > buf.len() {
            break;
        }
        out.push((fourcc, i + header, &buf[i + header..i + total]));
        i += total;
    }
    out
}

/// First child box of `body` with the given FourCC, as `(offset of its
/// body within `body`, that body)`.
fn child_at<'a>(body: &'a [u8], want: &[u8; 4]) -> Option<(usize, &'a [u8])> {
    boxes(body)
        .into_iter()
        .find(|(fourcc, _, _)| fourcc == want)
        .map(|(_, at, b)| (at, b))
}

/// First child box of `body` with the given FourCC.
fn child<'a>(body: &'a [u8], want: &[u8; 4]) -> Option<&'a [u8]> {
    child_at(body, want).map(|(_, b)| b)
}

/// Every child box of `body` with the given FourCC, in file order.
fn children<'a>(body: &'a [u8], want: &[u8; 4]) -> Vec<&'a [u8]> {
    boxes(body)
        .into_iter()
        .filter(|(fourcc, _, _)| fourcc == want)
        .map(|(_, _, b)| b)
        .collect()
}

/// `(version, creation_time, modification_time)` of an `mvhd` / `tkhd` /
/// `mdhd` body. All three put the timestamp pair immediately after the
/// FullBox preamble, u32 each at version 0 and u64 each at version 1.
fn header_times(body: &[u8]) -> (u8, u64, u64) {
    let version = body[0];
    if version == 0 {
        let c = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as u64;
        let m = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as u64;
        (0, c, m)
    } else {
        let c = u64::from_be_bytes([
            body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
        ]);
        let m = u64::from_be_bytes([
            body[12], body[13], body[14], body[15], body[16], body[17], body[18], body[19],
        ]);
        (1, c, m)
    }
}

/// The `mvhd` body of a muxed file.
fn mvhd(file: &[u8]) -> &[u8] {
    let moov = child(file, b"moov").expect("moov");
    child(moov, b"mvhd").expect("mvhd")
}

/// Byte offset of a sub-slice within the buffer it was carved from.
/// Lets the box walkers above double as an offset finder for tests
/// that patch header fields in place.
fn header_body_offsets(file: &[u8]) -> (usize, usize, usize) {
    // Walks from the file root accumulating each body's offset.
    // Differencing `as_ptr()` values would be shorter, but the org's
    // manual miri job runs `-Zmiri-strict-provenance`, which rejects
    // the pointer-to-integer cast.
    let (moov_at, moov) = child_at(file, b"moov").expect("moov");
    let (mvhd_at, _) = child_at(moov, b"mvhd").expect("mvhd");
    let (trak_at, trak) = child_at(moov, b"trak").expect("trak");
    let (tkhd_at, _) = child_at(trak, b"tkhd").expect("tkhd");
    let (mdia_at, mdia) = child_at(trak, b"mdia").expect("mdia");
    let (mdhd_at, _) = child_at(mdia, b"mdhd").expect("mdhd");
    let trak_abs = moov_at + trak_at;
    (
        moov_at + mvhd_at,
        trak_abs + tkhd_at,
        trak_abs + mdia_at + mdhd_at,
    )
}

/// Per-track `(tkhd body, mdhd body)` pairs, in `trak` order.
fn track_headers(file: &[u8]) -> Vec<(&[u8], &[u8])> {
    let moov = child(file, b"moov").expect("moov");
    children(moov, b"trak")
        .into_iter()
        .map(|trak| {
            let tkhd = child(trak, b"tkhd").expect("tkhd");
            let mdia = child(trak, b"mdia").expect("mdia");
            let mdhd = child(mdia, b"mdhd").expect("mdhd");
            (tkhd, mdhd)
        })
        .collect()
}

// --- Muxing helpers ------------------------------------------------------

fn pcm_stream_info(index: u32) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn packet(stream_index: u32, i: i64) -> Packet {
    let frames: i64 = 1024;
    let mut p = Packet::new(
        stream_index,
        TimeBase::new(1, 48_000),
        vec![0u8; frames as usize * 4],
    );
    p.pts = Some(i * frames);
    p.dts = Some(i * frames);
    p.duration = Some(frames);
    p.flags.keyframe = true;
    p
}

/// Mux `streams.len()` PCM tracks (3 packets each) with the given
/// options and return the file bytes. Routed through a temp file
/// because the muxer owns the `Box<dyn WriteSeek>`.
fn mux(streams: &[StreamInfo], opts: Mp4MuxerOptions, tag: &str) -> Vec<u8> {
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-header-timestamps-{tag}-{}.mp4",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut m = oxideav_mp4::muxer::open_with_options(ws, streams, opts).unwrap();
        m.write_header().unwrap();
        for i in 0..3i64 {
            for s in streams {
                m.write_packet(&packet(s.index, i)).unwrap();
            }
        }
        m.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// A representative stamp: 2023-11-14T22:13:20Z, comfortably inside the
/// 32-bit horizon.
const UNIX_2023: i64 = 1_700_000_000;
/// 2049-03-22T04:26:40Z — past 2040-02-06, so version-1 headers.
const UNIX_2049: i64 = 2_500_000_000;

// --- Tests ---------------------------------------------------------------

#[test]
fn epoch_conversion_matches_the_1904_origin() {
    // The Unix epoch itself is exactly the offset.
    assert_eq!(mp4_secs_from_unix_secs(0), Some(MP4_EPOCH_OFFSET_SECS));
    assert_eq!(
        mp4_secs_from_system_time(std::time::UNIX_EPOCH),
        Some(MP4_EPOCH_OFFSET_SECS)
    );
    // 1904-01-01 itself is representable as 0; one second earlier is not.
    let epoch_1904 = -(MP4_EPOCH_OFFSET_SECS as i64);
    assert_eq!(mp4_secs_from_unix_secs(epoch_1904), Some(0));
    assert_eq!(mp4_secs_from_unix_secs(epoch_1904 - 1), None);
    assert_eq!(mp4_secs_from_unix_secs(i64::MIN), None);
    // A pre-1970 but post-1904 SystemTime still converts.
    let pre_unix = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1_000_000);
    assert_eq!(
        mp4_secs_from_system_time(pre_unix),
        Some(MP4_EPOCH_OFFSET_SECS - 1_000_000)
    );
    // Sub-second remainders floor on both sides of 1970: each instant
    // names the second it falls inside, never the next one up.
    let after = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_500);
    assert_eq!(
        mp4_secs_from_system_time(after),
        Some(MP4_EPOCH_OFFSET_SECS + 1)
    );
    let before = std::time::UNIX_EPOCH - std::time::Duration::from_millis(1_500);
    assert_eq!(
        mp4_secs_from_system_time(before),
        Some(MP4_EPOCH_OFFSET_SECS - 2)
    );
    // 1904 boundary via SystemTime, including the floor at the edge.
    let epoch_1904_st =
        std::time::UNIX_EPOCH - std::time::Duration::from_secs(MP4_EPOCH_OFFSET_SECS);
    assert_eq!(mp4_secs_from_system_time(epoch_1904_st), Some(0));
    assert_eq!(
        mp4_secs_from_system_time(epoch_1904_st - std::time::Duration::from_millis(1)),
        None
    );
    // `unix_secs_from_mp4_secs` is the exact inverse across the range,
    // and saturates instead of wrapping at the far end of u64.
    assert_eq!(unix_secs_from_mp4_secs(MP4_EPOCH_OFFSET_SECS), 0);
    assert_eq!(unix_secs_from_mp4_secs(0), epoch_1904);
    for unix in [epoch_1904, -1, 0, 1, UNIX_2023, UNIX_2049] {
        let mp4 = mp4_secs_from_unix_secs(unix).unwrap();
        assert_eq!(unix_secs_from_mp4_secs(mp4), unix, "round trip {unix}");
    }
    assert_eq!(unix_secs_from_mp4_secs(u64::MAX), i64::MAX);
}

#[test]
fn default_options_keep_the_historical_zero_headers() {
    let streams = [pcm_stream_info(0)];
    let file = mux(&streams, Mp4MuxerOptions::default(), "default");

    assert_eq!(header_times(mvhd(&file)), (0, 0, 0));
    for (tkhd, mdhd) in track_headers(&file) {
        assert_eq!(header_times(tkhd), (0, 0, 0), "tkhd");
        assert_eq!(header_times(mdhd), (0, 0, 0), "mdhd");
    }
}

#[test]
fn post_2040_timestamp_promotes_headers_to_version_1() {
    let t = mp4_secs_from_unix_secs(UNIX_2049).unwrap();
    assert!(t > u32::MAX as u64, "test stamp must exceed the v0 horizon");
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        creation_time: Some(t),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "v1");

    assert_eq!(header_times(mvhd(&file)), (1, t, t));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (1, t, t), "tkhd");
    assert_eq!(header_times(headers[0].1), (1, t, t), "mdhd");

    // The promoted headers must still parse. Assert on fields that come
    // *from those boxes* — v1 shifts timescale and duration 12 bytes
    // later in each — not on the sample entry: `params.sample_rate` is
    // read out of `stsd` and would survive a broken mdhd offset.
    // `time_base` is 1/mdhd.timescale; `duration_micros` is mvhd's
    // duration over mvhd's timescale (3072 ticks @ 48 kHz = 64 ms).
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.streams()[0].time_base, TimeBase::new(1, 48_000));
    assert_eq!(dmx.duration_micros(), Some(64_000));
    // And the 64-bit timestamps themselves read back: the v1 branch of
    // the header-timestamp reader is a different code path from v0.
    assert_eq!(dmx.mvhd_timestamps(), stamped(t));
    assert_eq!(dmx.tkhd_timestamps(0), Some(stamped(t)));
    assert_eq!(dmx.mdhd_timestamps(0), Some(stamped(t)));
    let mut packets = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => packets += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(packets, 3);

    // The modification half drives promotion on its own: a v0-range
    // creation time next to a post-2040 modification time still needs
    // 64-bit fields, since the v0 layout can hold neither.
    let c = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    assert!(c <= u32::MAX as u64);
    let opts = Mp4MuxerOptions {
        creation_time: Some(c),
        modification_time: Some(t),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "promote-on-modification");
    assert_eq!(header_times(mvhd(&file)), (1, c, t));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (1, c, t), "tkhd");
    assert_eq!(header_times(headers[0].1), (1, c, t), "mdhd");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), pair(c, t));
    assert_eq!(dmx.mdhd_timestamps(0), Some(pair(c, t)));
}

#[test]
fn version_promotion_is_per_box() {
    // A movie-wide stamp inside the 32-bit horizon plus an override
    // past it: mvhd and the unoverridden track stay version 0 while
    // only the overridden track's tkhd/mdhd promote. The promotion is
    // decided per box, not once per file.
    let movie = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let late = mp4_secs_from_unix_secs(UNIX_2049).unwrap();
    assert!(movie <= u32::MAX as u64 && late > u32::MAX as u64);
    let streams = [pcm_stream_info(0), pcm_stream_info(1)];
    let opts = Mp4MuxerOptions {
        creation_time: Some(movie),
        track_header_timestamps: vec![track_time(1, late)],
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "mixed-version");

    assert_eq!(header_times(mvhd(&file)), (0, movie, movie));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (0, movie, movie), "trak0 tkhd");
    assert_eq!(header_times(headers[0].1), (0, movie, movie), "trak0 mdhd");
    assert_eq!(header_times(headers[1].0), (1, late, late), "trak1 tkhd");
    assert_eq!(header_times(headers[1].1), (1, late, late), "trak1 mdhd");

    // A moov mixing box versions still demuxes both tracks.
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 2);
    for s in dmx.streams() {
        assert_eq!(s.time_base, TimeBase::new(1, 48_000));
    }
}

#[test]
fn promoted_init_headers_do_not_dislodge_the_mehd_patch() {
    // §8.8.2: with `write_mehd` the trailer patches eight
    // `fragment_duration` bytes at an offset remembered when the init
    // moov was built. A version-1 mvhd + tkhd + mdhd pushes the mvex
    // 36 bytes further into that moov, so this is the case where a
    // hardcoded offset would write over eight unrelated bytes instead.
    let t = mp4_secs_from_unix_secs(UNIX_2049).unwrap();
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        fragmented: Some(FragmentedOptions {
            write_mehd: true,
            ..FragmentedOptions::default()
        }),
        creation_time: Some(t),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "fragmented-mehd-v1");

    // The promotion really did happen — otherwise the test proves nothing.
    assert_eq!(header_times(mvhd(&file)), (1, t, t));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (1, t, t), "tkhd");
    assert_eq!(header_times(headers[0].1), (1, t, t), "mdhd");

    // The sealed duration landed in the mehd's own field, walked
    // structurally rather than by fourcc search: 3 × 1024 media ticks
    // @ 48 kHz = 64 movie ticks at timescale 1000.
    let moov = child(&file, b"moov").expect("moov");
    let mvex = child(moov, b"mvex").expect("mvex");
    let mehd = child(mvex, b"mehd").expect("mehd");
    assert_eq!(mehd[0], 1, "version 1 (64-bit fragment_duration)");
    let dur = u64::from_be_bytes(mehd[4..12].try_into().unwrap());
    assert_eq!(dur, 64, "sealed fragment_duration in movie timescale");

    // And the demuxer agrees, which it would not if the patch had
    // clipped the surrounding boxes.
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.duration_micros(), Some(64_000));
}

#[test]
fn faststart_layout_stamps_the_same_values() {
    let t = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        faststart: true,
        creation_time: Some(t),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "faststart");

    // Sanity: moov really does precede mdat in this layout.
    let order: Vec<[u8; 4]> = boxes(&file).into_iter().map(|(f, _, _)| f).collect();
    let moov_at = order.iter().position(|f| f == b"moov").unwrap();
    let mdat_at = order.iter().position(|f| f == b"mdat").unwrap();
    assert!(moov_at < mdat_at);

    assert_eq!(header_times(mvhd(&file)), (0, t, t));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (0, t, t), "tkhd");
    assert_eq!(header_times(headers[0].1), (0, t, t), "mdhd");

    // Again with a v1-promoted stamp. Faststart is the one layout
    // where the moov's size feeds back into the chunk offsets it
    // contains, via the convergence loop — so the 36 bytes the
    // promotion adds have to settle before the offsets are baked. A
    // clean demux of every packet is what proves they did.
    let late = mp4_secs_from_unix_secs(UNIX_2049).unwrap();
    let opts = Mp4MuxerOptions {
        faststart: true,
        creation_time: Some(late),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "faststart-v1");
    assert_eq!(header_times(mvhd(&file)), (1, late, late));

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), stamped(late));
    let mut packets = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => packets += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(packets, 3, "chunk offsets survived the v1 moov growth");
}

#[test]
fn fragmented_init_segment_stamps_the_headers() {
    let movie = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let track1 = mp4_secs_from_unix_secs(UNIX_2023 + 3_600).unwrap();
    let media1 = track1 + 7;
    let streams = [pcm_stream_info(0), pcm_stream_info(1)];
    let opts = Mp4MuxerOptions {
        fragmented: Some(FragmentedOptions::default()),
        creation_time: Some(movie),
        // Track 1 dates its media apart from its track, so the init
        // segment's tkhd and mdhd must come out different — a path
        // that would look identical under the one-value shorthand.
        track_header_timestamps: vec![TrackHeaderTimestamps {
            stream_index: 1,
            track: stamped(track1),
            media: Some(stamped(media1)),
        }],
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "fragmented");

    assert_eq!(header_times(mvhd(&file)), (0, movie, movie));
    let headers = track_headers(&file);
    assert_eq!(headers.len(), 2);
    assert_eq!(header_times(headers[0].0), (0, movie, movie), "trak0 tkhd");
    assert_eq!(header_times(headers[0].1), (0, movie, movie), "trak0 mdhd");
    assert_eq!(
        header_times(headers[1].0),
        (0, track1, track1),
        "trak1 tkhd"
    );
    assert_eq!(
        header_times(headers[1].1),
        (0, media1, media1),
        "trak1 mdhd"
    );

    // The init segment writes zero durations, so here the timestamp is
    // the only thing that can trigger the version-1 promotion.
    let late = mp4_secs_from_unix_secs(UNIX_2049).unwrap();
    let opts = Mp4MuxerOptions {
        fragmented: Some(FragmentedOptions::default()),
        creation_time: Some(late),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "fragmented-v1");
    assert_eq!(header_times(mvhd(&file)), (1, late, late));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (1, late, late), "tkhd");
    assert_eq!(header_times(headers[0].1), (1, late, late), "mdhd");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), stamped(late));
    assert_eq!(dmx.streams()[0].time_base, TimeBase::new(1, 48_000));
}

#[test]
fn out_of_range_track_override_is_rejected_at_open() {
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        track_header_timestamps: vec![track_time(3, 1)],
        ..Mp4MuxerOptions::default()
    };
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let err = match oxideav_mp4::muxer::open_with_options(ws, &streams, opts.clone()) {
        Ok(_) => panic!("out-of-range track_header_timestamps accepted"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("track_header_timestamps"),
        "unexpected error: {err}"
    );

    // Same guard on the fragmented entry point.
    let frag_opts = Mp4MuxerOptions {
        fragmented: Some(FragmentedOptions::default()),
        ..opts
    };
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let err = match oxideav_mp4::muxer::open_with_options(ws, &streams, frag_opts) {
        Ok(_) => panic!("out-of-range track_header_timestamps accepted (fragmented)"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("track_header_timestamps"),
        "unexpected error: {err}"
    );
}

// --- Read side -----------------------------------------------------------

/// Look up a flat metadata key on the movie.
fn meta<'a>(dmx: &'a oxideav_mp4::demux::Mp4Demuxer, key: &str) -> Option<&'a str> {
    dmx.metadata()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn stamped_file_round_trips_through_the_demuxer() {
    // Write → read with no conversion anywhere: what the option set is
    // what the accessors return, in the same 1904-second units.
    let movie = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let track1 = mp4_secs_from_unix_secs(UNIX_2023 + 86_400).unwrap();
    let streams = [pcm_stream_info(0), pcm_stream_info(1)];
    let opts = Mp4MuxerOptions {
        creation_time: Some(movie),
        track_header_timestamps: vec![track_time(1, track1)],
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "roundtrip");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();

    // Typed surface: mvhd movie-wide, the override on track 1 only.
    assert_eq!(dmx.mvhd_timestamps(), stamped(movie));
    assert_eq!(dmx.tkhd_timestamps(0), Some(stamped(movie)));
    assert_eq!(dmx.mdhd_timestamps(0), Some(stamped(movie)));
    assert_eq!(dmx.tkhd_timestamps(1), Some(stamped(track1)));
    assert_eq!(dmx.mdhd_timestamps(1), Some(stamped(track1)));
    // Out-of-range stream index.
    assert_eq!(dmx.tkhd_timestamps(2), None);
    assert_eq!(dmx.mdhd_timestamps(2), None);

    // Flat surface, movie-level and per-stream, same units.
    let movie_s = movie.to_string();
    let track1_s = track1.to_string();
    assert_eq!(meta(&dmx, "mvhd_creation_time"), Some(movie_s.as_str()));
    assert_eq!(meta(&dmx, "mvhd_modification_time"), Some(movie_s.as_str()));
    let opts0 = &dmx.streams()[0].params.options;
    assert_eq!(opts0.get("tkhd_creation_time"), Some(movie_s.as_str()));
    assert_eq!(opts0.get("mdhd_creation_time"), Some(movie_s.as_str()));
    let opts1 = &dmx.streams()[1].params.options;
    assert_eq!(opts1.get("tkhd_creation_time"), Some(track1_s.as_str()));
    assert_eq!(opts1.get("mdhd_modification_time"), Some(track1_s.as_str()));

    // And back to a Unix timestamp, ending where the caller started.
    assert_eq!(
        unix_secs_from_mp4_secs(dmx.mvhd_timestamps().creation_time),
        UNIX_2023
    );
}

/// PATH-gated black-box cross-check: an *independent writer* stamps a
/// file, and our reader must recover the same instant. Proves the
/// intake agrees with the wider ecosystem on epoch and field offsets,
/// which no self-round-trip can establish — a reader and writer that
/// share a mistake still agree with each other.
#[test]
fn foreign_stamped_file_reads_back_the_same_instant() {
    use std::process::Command;

    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mp4-header-timestamps-foreign-{}.mp4",
        std::process::id()
    ));
    // `-metadata creation_time` takes an RFC 3339 instant; UNIX_2023 is
    // that same instant, so the file should read back as exactly it.
    let made = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("sine=frequency=440:duration=1")
        .args(["-c:a", "aac", "-metadata"])
        .arg("creation_time=2023-11-14T22:13:20Z")
        .arg(&tmp)
        .output()
        .expect("run ffmpeg");
    assert!(made.status.success(), "ffmpeg failed to write the fixture");

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    let expected = mp4_secs_from_unix_secs(UNIX_2023).unwrap();

    assert_eq!(
        dmx.mvhd_timestamps().creation_time,
        expected,
        "mvhd from an independent writer"
    );
    assert_eq!(
        dmx.tkhd_timestamps(0).unwrap().creation_time,
        expected,
        "tkhd from an independent writer"
    );
    assert_eq!(
        dmx.mdhd_timestamps(0).unwrap().creation_time,
        expected,
        "mdhd from an independent writer"
    );
    assert_eq!(
        unix_secs_from_mp4_secs(dmx.mvhd_timestamps().creation_time),
        UNIX_2023
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Every other test stamps `creation_time` and `modification_time`
/// with the same value, because that is all the muxer can write — so
/// a reader that filled both slots from one field, or swapped the
/// pair, or crossed `tkhd` with `mdhd`, would pass all of them. Patch
/// six distinct values straight into the bytes and demand six distinct
/// values back. Run for both header versions, since v0 and v1 are
/// separate branches of the reader.
#[test]
fn each_header_field_is_read_from_its_own_bytes() {
    for version in [0u8, 1u8] {
        // A stamp past the 32-bit horizon forces the v1 layout.
        let base = if version == 0 {
            mp4_secs_from_unix_secs(UNIX_2023).unwrap()
        } else {
            mp4_secs_from_unix_secs(UNIX_2049).unwrap()
        };
        let streams = [pcm_stream_info(0)];
        let opts = Mp4MuxerOptions {
            creation_time: Some(base),
            ..Mp4MuxerOptions::default()
        };
        let mut file = mux(&streams, opts, &format!("distinct-v{version}"));

        // Six values, each offset differently from the base so no two
        // fields can be confused and every one stays in its version's
        // representable range.
        let want: [u64; 6] = [base + 1, base + 2, base + 3, base + 4, base + 5, base + 6];
        let (mvhd_at, tkhd_at, mdhd_at) = header_body_offsets(&file);
        for (box_at, pair) in [
            (mvhd_at, [want[0], want[1]]),
            (tkhd_at, [want[2], want[3]]),
            (mdhd_at, [want[4], want[5]]),
        ] {
            assert_eq!(file[box_at], version, "fixture should be v{version}");
            // creation_time then modification_time, immediately after
            // the 4-byte FullBox preamble.
            for (i, v) in pair.iter().enumerate() {
                if version == 0 {
                    let at = box_at + 4 + i * 4;
                    file[at..at + 4].copy_from_slice(&(*v as u32).to_be_bytes());
                } else {
                    let at = box_at + 4 + i * 8;
                    file[at..at + 8].copy_from_slice(&v.to_be_bytes());
                }
            }
        }

        let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
        let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
        let pair = |c: u64, m: u64| HeaderTimestamps {
            creation_time: c,
            modification_time: m,
        };
        assert_eq!(
            dmx.mvhd_timestamps(),
            pair(want[0], want[1]),
            "mvhd v{version}"
        );
        assert_eq!(
            dmx.tkhd_timestamps(0),
            Some(pair(want[2], want[3])),
            "tkhd v{version}"
        );
        assert_eq!(
            dmx.mdhd_timestamps(0),
            Some(pair(want[4], want[5])),
            "mdhd v{version}"
        );
    }
}

/// The write surface reaches every field the read surface reports:
/// six independently-set values survive a mux→demux round trip. This
/// is the parity check — anything the demuxer can observe, the muxer
/// can produce.
#[test]
fn every_readable_field_is_independently_writable() {
    let base = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    // Six distinct instants, one per field.
    let (mv_c, mv_m) = (base + 1, base + 2);
    let (tk_c, tk_m) = (base + 3, base + 4);
    let (md_c, md_m) = (base + 5, base + 6);
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        creation_time: Some(mv_c),
        modification_time: Some(mv_m),
        track_header_timestamps: vec![TrackHeaderTimestamps {
            stream_index: 0,
            track: pair(tk_c, tk_m),
            media: Some(pair(md_c, md_m)),
        }],
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "full-parity");

    // On the wire.
    assert_eq!(header_times(mvhd(&file)), (0, mv_c, mv_m));
    let headers = track_headers(&file);
    assert_eq!(header_times(headers[0].0), (0, tk_c, tk_m), "tkhd");
    assert_eq!(header_times(headers[0].1), (0, md_c, md_m), "mdhd");

    // And back through the reader.
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), pair(mv_c, mv_m));
    assert_eq!(dmx.tkhd_timestamps(0), Some(pair(tk_c, tk_m)));
    assert_eq!(dmx.mdhd_timestamps(0), Some(pair(md_c, md_m)));
}

#[test]
fn an_entry_replaces_the_pair_and_media_defaults_to_track() {
    // The only defaulting in a `TrackHeaderTimestamps` is `media:
    // None` reusing the `track` pair. Everything else is stated
    // outright: an entry never takes half its value from the
    // movie-wide options, so no field can be perturbed by a
    // neighbouring one.
    let movie_c = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let movie_m = movie_c + 10;
    let track_c = movie_c + 20;
    let media3 = movie_c + 30;
    let streams = [
        pcm_stream_info(0),
        pcm_stream_info(1),
        pcm_stream_info(2),
        pcm_stream_info(3),
    ];
    let opts = Mp4MuxerOptions {
        creation_time: Some(movie_c),
        modification_time: Some(movie_m),
        track_header_timestamps: vec![
            // Track 0: one instant in both halves, mdhd mirroring tkhd.
            track_time(0, track_c),
            // Track 2: "created, never modified" — a zero modification
            // half is an ordinary value here, not a fallback trigger.
            TrackHeaderTimestamps {
                stream_index: 2,
                track: pair(track_c, 0),
                media: None,
            },
            // Track 3 overrides only its *media* dates while its track
            // keeps the movie's pair. Because `track` is stated rather
            // than inherited, the movie's modification half survives —
            // the case a per-field inheritance cascade got wrong by
            // collapsing tkhd's modification time onto its creation.
            TrackHeaderTimestamps {
                stream_index: 3,
                track: pair(movie_c, movie_m),
                media: Some(stamped(media3)),
            },
        ],
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "entry-shapes");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), pair(movie_c, movie_m));
    assert_eq!(dmx.tkhd_timestamps(0), Some(stamped(track_c)));
    assert_eq!(dmx.mdhd_timestamps(0), Some(stamped(track_c)));
    // Track 1 has no entry at all — both boxes take the movie pair.
    assert_eq!(dmx.tkhd_timestamps(1), Some(pair(movie_c, movie_m)));
    assert_eq!(dmx.mdhd_timestamps(1), Some(pair(movie_c, movie_m)));
    assert_eq!(dmx.tkhd_timestamps(2), Some(pair(track_c, 0)));
    assert_eq!(dmx.mdhd_timestamps(2), Some(pair(track_c, 0)));
    assert_eq!(
        dmx.tkhd_timestamps(3),
        Some(pair(movie_c, movie_m)),
        "a media-only override must not disturb tkhd"
    );
    assert_eq!(dmx.mdhd_timestamps(3), Some(stamped(media3)));
}

#[test]
fn modification_time_alone_is_written_without_a_creation_time() {
    // The `creation = 0, modification = t` shape: unusual for a new
    // file, but real files carry it and the demuxer reports it, so the
    // muxer must be able to produce it.
    let m = mp4_secs_from_unix_secs(UNIX_2023).unwrap();
    let streams = [pcm_stream_info(0)];
    let opts = Mp4MuxerOptions {
        modification_time: Some(m),
        ..Mp4MuxerOptions::default()
    };
    let file = mux(&streams, opts, "modification-only");

    assert_eq!(header_times(mvhd(&file)), (0, 0, m));
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.mvhd_timestamps(), pair(0, m));
    assert!(!dmx.mvhd_timestamps().is_unset());
    // The flat channel emits per field, so only the non-zero half
    // shows up.
    assert_eq!(meta(&dmx, "mvhd_creation_time"), None);
    assert_eq!(meta(&dmx, "mvhd_modification_time"), Some(&*m.to_string()));
}

#[test]
fn unstamped_file_reads_back_unset_and_emits_no_keys() {
    // Zero is "the producer didn't stamp this", so the flat channel
    // stays quiet rather than reporting 1904 for every default file.
    let streams = [pcm_stream_info(0)];
    let file = mux(&streams, Mp4MuxerOptions::default(), "roundtrip-unset");

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();

    assert!(dmx.mvhd_timestamps().is_unset());
    assert!(dmx.tkhd_timestamps(0).unwrap().is_unset());
    assert!(dmx.mdhd_timestamps(0).unwrap().is_unset());
    for key in ["mvhd_creation_time", "mvhd_modification_time"] {
        assert_eq!(meta(&dmx, key), None, "{key} should be absent");
    }
    let opts = &dmx.streams()[0].params.options;
    for key in [
        "tkhd_creation_time",
        "tkhd_modification_time",
        "mdhd_creation_time",
        "mdhd_modification_time",
    ] {
        assert!(opts.get(key).is_none(), "{key} should be absent");
    }
}
