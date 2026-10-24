//! A track's header fields, stored sample entries and sample table, as
//! the file declares them.
use oxideav_core::RuntimeContext;

#[test]
fn track_header_entries_and_samples_of_an_apple_m4a() {
    // 4097 stereo frames at 44100 Hz from macOS's AAC encoder (afconvert);
    // afinfo: 7 packets, 1004 bytes from offset 4096.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/apple-aac-4097.m4a"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    assert_eq!(d.major_brand(), Some(*b"M4A "));
    assert_eq!(d.movie_timescale(), 44100);
    let h = d.track_header(0).unwrap();
    assert_eq!(h.track_id, 1);
    assert_eq!(h.flags, 7);
    assert_eq!((h.width, h.height), (0, 0));
    assert_eq!(&h.handler_type, b"soun");
    assert_eq!(h.media_timescale, 44100);
    assert_eq!(h.media_duration, 7168);
    assert_eq!(h.language, 0);
    assert!(d.track_header(1).is_none());

    assert_eq!(d.sample_entry_count(0), 1);
    let e = d.sample_entry(0, 1).unwrap();
    assert_eq!(&e.format, b"mp4a");
    assert_eq!(e.data_reference_index, 1);
    assert_eq!(e.payload.len(), 102 - 8);
    assert_eq!(&e.payload[28 + 4..28 + 8], b"esds");
    assert!(d.sample_entry(0, 0).is_none());
    assert!(d.sample_entry(0, 2).is_none());

    let samples = d.samples(0);
    assert_eq!(samples.len(), 7);
    assert_eq!(samples[0].offset, 4096);
    assert_eq!(samples.iter().map(|s| u64::from(s.size)).sum::<u64>(), 1004);
    for (i, s) in samples.iter().enumerate() {
        if i > 0 {
            let p = samples[i - 1];
            assert_eq!(s.offset, p.offset + u64::from(p.size));
        }
        assert_eq!(s.decode_time, 1024 * i as i64);
        assert_eq!(s.composition_time, s.decode_time);
        assert_eq!(s.duration, 1024);
        assert!(s.sync);
        assert_eq!(s.sample_description_index, 1);
    }
    assert!(d.samples(1).is_empty());
}

#[test]
fn an_unfragmented_movie_is_not_fragmented() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/apple-aac-4097.m4a"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    assert!(!d.is_fragmented());
}
