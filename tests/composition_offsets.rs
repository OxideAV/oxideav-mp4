//! The muxer writes `ctts` (and `cslg` for negative offsets) from the
//! packets' presentation and decode timestamps (ISO/IEC 14496-12 §8.6.1.3-4).

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase, WriteSeek};

/// The payload of the first box at `path` (e.g. `["moov", "trak", ...]`).
fn find<'a>(mut data: &'a [u8], path: &[&str]) -> Option<&'a [u8]> {
    for (depth, name) in path.iter().enumerate() {
        let mut found = None;
        let mut at = 0;
        while at + 8 <= data.len() {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            if size < 8 || at + size > data.len() {
                return None;
            }
            if &data[at + 4..at + 8] == name.as_bytes() {
                found = Some(&data[at + 8..at + size]);
                break;
            }
            at += size;
        }
        data = found?;
        let _ = depth;
    }
    Some(data)
}

fn video() -> StreamInfo {
    let mut params = CodecParameters::video(CodecId::new("mjpeg"));
    params.width = Some(16);
    params.height = Some(16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 600),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// I P B B in decode order; `(dts, pts)` in 1/600 s, 20 ticks per frame.
fn mux(name: &str, times: &[(i64, i64)]) -> Vec<u8> {
    let stream = video();
    let path = std::env::temp_dir().join(format!("oxideav-mp4-ctts-{name}.mp4"));
    {
        let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let mut mux = oxideav_mp4::muxer::open(out, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, &(dts, pts)) in times.iter().enumerate() {
            let mut p = Packet::new(0, stream.time_base, vec![i as u8; 8]);
            p.dts = Some(dts);
            p.pts = Some(pts);
            p.duration = Some(20);
            p.flags.keyframe = i == 0;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let file = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}

const STBL: [&str; 5] = ["moov", "trak", "mdia", "minf", "stbl"];

fn stbl_box<'a>(file: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let mut path = STBL.to_vec();
    path.push(name);
    find(file, &path)
}

#[test]
fn non_negative_offsets_write_version_0_without_cslg() {
    let file = mux("positive", &[(0, 20), (20, 80), (40, 40), (60, 60)]);
    let ctts = stbl_box(&file, "ctts").expect("ctts");
    assert_eq!(ctts[0], 0, "version 0");
    // (1, 20) (1, 60) (2, 0) at media time scale 1000
    let entries: Vec<(u32, i32)> = ctts[8..]
        .chunks(8)
        .map(|c| {
            (
                u32::from_be_bytes(c[0..4].try_into().unwrap()),
                i32::from_be_bytes(c[4..8].try_into().unwrap()),
            )
        })
        .collect();
    assert_eq!(entries, vec![(1, 33), (1, 100), (2, 0)]);
    assert!(stbl_box(&file, "cslg").is_none());
}

#[test]
fn negative_offsets_write_version_1_and_cslg() {
    let file = mux("negative", &[(0, 0), (20, 60), (40, 20), (60, 40)]);
    let ctts = stbl_box(&file, "ctts").expect("ctts");
    assert_eq!(ctts[0], 1, "signed offsets need version 1");
    let cslg = stbl_box(&file, "cslg").expect("cslg");
    let f: Vec<i32> = cslg[4..]
        .chunks(4)
        .map(|c| i32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    // shift, least, greatest, composition start, composition end (ms)
    assert_eq!(f, vec![34, -34, 67, 0, 133]);
}

#[test]
fn equal_timestamps_write_no_ctts() {
    let file = mux("equal", &[(0, 0), (20, 20), (40, 40)]);
    assert!(stbl_box(&file, "ctts").is_none());
}

/// A B-frame stream whose decode times start before zero (here one
/// frame early, as encoders with reordering commonly emit): media time 0
/// is the first sample's *decode* time, so the edit list has to start
/// one frame in for the first frame to present at time 0 again.
#[test]
fn negative_first_dts_maps_presentation_zero_through_the_edit_list() {
    use oxideav_core::{Error, NullCodecResolver, ReadSeek};
    let file = mux("origin", &[(-20, 0), (0, 60), (20, 20), (40, 40)]);
    let elst = find(&file, &["moov", "trak", "edts", "elst"]).expect("elst");
    // version 0, one entry: segment_duration, media_time (33 ms = 20/600 s)
    assert_eq!(u32::from_be_bytes(elst[4..8].try_into().unwrap()), 1);
    assert_eq!(i32::from_be_bytes(elst[12..16].try_into().unwrap()), 33);

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let mut d = oxideav_mp4::demux::open(rs, &NullCodecResolver).unwrap();
    let mut pts = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => pts.push(p.pts.unwrap()),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    // Presentation order restored, starting at 0 (1/1000 s; DTS and PTS
    // round to the media time scale separately).
    assert_eq!(pts, vec![0, 100, 33, 66]);
}

/// Without packet durations the decode deltas come from the DTS
/// sequence, not from the reordered PTS (which would go backwards).
#[test]
fn decode_deltas_follow_dts_without_durations() {
    let stream = video();
    let path = std::env::temp_dir().join("oxideav-mp4-ctts-nodur.mp4");
    {
        let out: Box<dyn WriteSeek> = Box::new(std::fs::File::create(&path).unwrap());
        let mut mux = oxideav_mp4::muxer::open(out, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, &(dts, pts)) in [(0, 20), (20, 80), (40, 40), (60, 60)].iter().enumerate() {
            let mut p = Packet::new(0, stream.time_base, vec![i as u8; 8]);
            p.dts = Some(dts);
            p.pts = Some(pts);
            p.flags.keyframe = i == 0;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let file = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let stts = stbl_box(&file, "stts").expect("stts");
    let runs: Vec<(u32, u32)> = stts[8..]
        .chunks(8)
        .map(|c| {
            (
                u32::from_be_bytes(c[0..4].try_into().unwrap()),
                u32::from_be_bytes(c[4..8].try_into().unwrap()),
            )
        })
        .collect();
    // The first sample has no predecessor (1-tick fallback); every later
    // delta is the 33 ms DTS step, never the PTS jump back from 133 to 67.
    assert_eq!(runs.iter().map(|r| r.0).sum::<u32>(), 4);
    assert!(
        runs[1..].iter().all(|&(_, d)| d == 33 || d == 34),
        "{runs:?}"
    );
}
