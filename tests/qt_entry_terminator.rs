//! A QuickTime video sample description can end in a four-byte zero
//! terminator after its extension boxes.
use oxideav_core::{Demuxer, Error, RuntimeContext};

#[test]
fn prores_movie_with_a_terminated_sample_description() {
    // Three 16x16 ProRes 4444 frames from macOS's encoder (VTCompressionSession)
    // written by AVAssetWriter: the ap4h entry's colr and fiel boxes are
    // followed by four zero bytes.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/apple-prores-4444-16x16.mov"
    );
    let ctx = RuntimeContext::new();
    let file = std::fs::File::open(path).unwrap();
    let mut d = oxideav_mp4::demux::open_typed(Box::new(file), &ctx.codecs).unwrap();
    let p = &d.streams()[0].params;
    assert_eq!((p.width, p.height), (Some(16), Some(16)));
    let mut bytes = 0;
    let mut packets = 0;
    loop {
        match d.next_packet() {
            Ok(p) => {
                packets += 1;
                bytes += p.data.len();
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((packets, bytes), (3, 4032));
}
