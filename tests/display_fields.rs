//! The movie header's rate, volume and matrix, and a track header's layer,
//! alternate group, volume and matrix, as stored.
use oxideav_core::RuntimeContext;

#[test]
fn rotated_track_matrix_and_movie_header() {
    // One 16x16 frame whose tkhd carries a 90-degree rotation.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rot90-16x16.mov"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    let movie = d.movie_header();
    assert_eq!((movie.timescale, movie.duration), (12800, 512));
    assert_eq!((movie.rate, movie.volume), (0x10000, 0x100));
    assert_eq!(
        movie.matrix,
        [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000]
    );
    let track = d.track_header(0).unwrap();
    assert_eq!(
        (track.layer, track.alternate_group, track.volume),
        (0, 0, 0)
    );
    assert_eq!(
        track.matrix,
        [0, -0x10000, 0, 0x10000, 0, 0, 0, 0, 0x4000_0000]
    );
    assert_eq!((track.width, track.height), (16 << 16, 16 << 16));
}
