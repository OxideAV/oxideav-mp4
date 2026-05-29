//! Regression test for an extended-size (`size=1 largesize=u64::MAX`)
//! box that lands at a non-zero file offset.
//!
//! Companion to oxideav-mov's round 187 `synth_round187_extended_size_overflow`
//! test, which closed the same shape on the QTFF atom walker. Reproducer:
//!
//! ```text
//! 00 00 00 08 66 72 65 65    # box #1: size=8, type='free'
//! 00 00 00 01 6d 64 61 74    # box #2: size=1 (extended), type='mdat'
//! ff ff ff ff ff ff ff ff    # largesize = u64::MAX
//! …trailing garbage…
//! ```
//!
//! At `start = 8` and `total_size = u64::MAX`, the computation
//! `start + total_size = u64::MAX + 8` overflows `u64`. Debug builds
//! panic with `attempt to add with overflow`; release builds silently
//! wrap to a small value, then either pass the past-EOF guard or
//! trigger an unrelated parse error far from the actual cause.
//!
//! The fix lives in `read_box_header` (`src/boxes.rs`): it now
//! `checked_add`s the start offset and the declared total size and
//! rejects any header that would overflow `u64`. Every downstream
//! `body_start + payload_size()` arithmetic site — including the
//! §8.16.3 `sidx` end-anchor computation at `demux.rs:53` — inherits
//! the bound automatically: once we know `start + total_size <=
//! u64::MAX`, the algebraically equal `body_start + payload_size`
//! also fits.

use std::io::Cursor;

use oxideav_core::{NullCodecResolver, ReadSeek};

/// Replays the exact shape that triggers the overflow end-to-end
/// through `demux::open`. The headline contract is "no panic": the
/// demuxer must surface a clean `Err`, not a debug-build overflow
/// panic or a release-build wrap that propagates corrupted offsets
/// into the sample-table walker.
#[test]
fn extended_size_overflow_through_demuxer_does_not_panic() {
    let mut bytes = Vec::new();
    // Box #1: size=8, fourcc='free'. Pushes the next box's start to 8.
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x08]);
    bytes.extend_from_slice(b"free");
    // Box #2: size=1 (extended), fourcc='mdat', largesize=u64::MAX.
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    bytes.extend_from_slice(b"mdat");
    bytes.extend_from_slice(&u64::MAX.to_be_bytes());
    // Trailing garbage past the (declared but unreachable) box body.
    bytes.extend_from_slice(&[0xff; 64]);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let res = oxideav_mp4::demux::open(rs, &NullCodecResolver);
    assert!(
        res.is_err(),
        "u64-overflowing largesize must surface Err, not Ok(_); panic-free is the headline contract"
    );
}
