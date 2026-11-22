#![no_main]

//! Mux round-trip contract — valid-by-construction recipes must mux,
//! and demuxing the produced bytes must reproduce the identity:
//!
//!   * `open_typed` on our own muxer output MUST succeed;
//!   * stream count matches the plan;
//!   * every packet payload comes back byte-exact, in order, per
//!     stream — plain, faststart (`[ftyp][moov][mdat]` rewrite with
//!     patched chunk offsets), and fragmented (`moof`/`traf`/`trun`
//!     boundaries, optional `styp` + `sidx`/`mfra` indexes) layouts
//!     alike;
//!   * pts round-trips exactly (including a muxer-written start-delay
//!     `elst`) unless the plan inserted §8.8.7 empty time, which
//!     legitimately shifts downstream timestamps.
//!
//! Any Err out of the muxer on a plan the recipe module constructed,
//! any failed open, and any identity mismatch is a crash — this is
//! the target that turns "our muxer and demuxer disagree" into a
//! fuzzable invariant instead of a fixed test matrix.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer as _, NullCodecResolver, ReadSeek};
use oxideav_mp4_fuzz::{MuxPlan, Recipe};

fuzz_target!(|data: &[u8]| {
    let mut r = Recipe::new(data);
    let plan = MuxPlan::decode(&mut r);
    let built = plan.run();
    assert!(
        !built.bytes.is_empty(),
        "muxer produced an empty file from a valid plan"
    );

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(built.bytes.clone()));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &NullCodecResolver)
        .expect("demuxer must open our own muxer output");
    assert_eq!(
        dmx.streams().len(),
        built.tracks.len(),
        "stream count mismatch on round-trip"
    );

    // The demuxer surfaces each stream at its written mdhd media
    // timescale: the sample rate for audio and the stream's own clock
    // for video, both identical to the recipe time base, so the
    // conversion below is the identity (kept for a muxer that picks
    // another scale).
    let out_tb: Vec<(i64, i64)> = dmx
        .streams()
        .iter()
        .map(|s| (s.time_base.0.num, s.time_base.0.den))
        .collect();
    let rescale = |pts_in: i64, den_in: i64, si: usize| -> i64 {
        let (num_out, den_out) = out_tb[si];
        let wide = pts_in as i128 * den_out as i128;
        let div = den_in as i128 * num_out.max(1) as i128;
        assert_eq!(
            wide % div,
            0,
            "recipe pts {pts_in}/{den_in} does not rescale exactly to stream {si} \
             time base {num_out}/{den_out}"
        );
        (wide / div) as i64
    };

    // Drain everything, grouped by stream.
    let n = built.tracks.len();
    let mut got_payloads: Vec<Vec<Vec<u8>>> = vec![Vec::new(); n];
    let mut got_pts: Vec<Vec<i64>> = vec![Vec::new(); n];
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                let si = p.stream_index as usize;
                assert!(si < n, "packet stream_index {si} out of range");
                got_pts[si].push(p.pts.unwrap_or(i64::MIN));
                got_payloads[si].push(p.data);
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error on own muxer output: {e}"),
        }
    }

    // Uncompressed audio written one frame a packet comes back with
    // the frames of a chunk concatenated (the demuxer delivers one-tick
    // PCM frames up to 1024 to a packet): the byte stream and the time
    // of every delivered packet are the identity there, not the packet
    // boundaries.
    let frame_bytes: Vec<usize> = dmx
        .streams()
        .iter()
        .map(|s| {
            let p = &s.params;
            if p.codec_id.as_str().starts_with("pcm_") {
                usize::from(p.channels.unwrap_or(1).max(1)) * 2
            } else {
                0
            }
        })
        .collect();

    for (si, expect) in built.tracks.iter().enumerate() {
        let regrouped = frame_bytes[si] > 0 && got_payloads[si].len() != expect.payloads.len();
        if regrouped {
            assert_eq!(
                got_payloads[si].concat(),
                expect.payloads.concat(),
                "payload byte mismatch on stream {si}"
            );
            if built.check_pts {
                let mut at = rescale(expect.pts[0], expect.tb_den, si);
                for (k, data) in got_payloads[si].iter().enumerate() {
                    assert_eq!(got_pts[si][k], at, "pts mismatch on stream {si} packet {k}");
                    at += (data.len() / frame_bytes[si]) as i64;
                }
            }
            continue;
        }
        assert_eq!(
            got_payloads[si].len(),
            expect.payloads.len(),
            "packet count mismatch on stream {si}"
        );
        for (k, want) in expect.payloads.iter().enumerate() {
            assert_eq!(
                &got_payloads[si][k], want,
                "payload byte mismatch on stream {si} packet {k}"
            );
        }
        if built.check_pts {
            for (k, want) in expect.pts.iter().enumerate() {
                let want = rescale(*want, expect.tb_den, si);
                assert_eq!(
                    got_pts[si][k], want,
                    "pts mismatch on stream {si} packet {k}"
                );
            }
        }
    }

    // Gapless: the item written for track 0 reads back as given, with
    // the valid length the muxer derived from the finished track.
    // (An item whose valid length is zero — a one-frame track that is
    // all priming — says nothing and reads back as no timing.)
    if let Some((priming, total)) = built.gapless.filter(|&(p, t)| t > p) {
        let g = dmx
            .gapless_timing(0)
            .expect("gapless item written but not read back");
        assert_eq!(
            (g.priming, g.valid, g.remainder, g.conflict),
            (priming, total - priming, 0, false),
            "gapless timing differs on round-trip"
        );
    } else {
        assert!(
            dmx.gapless_timing(0).is_none(),
            "gapless timing reported for a plan that wrote none"
        );
    }

    // Seek back to the start and re-read the first packet of stream 0:
    // the sync-sample landing must reproduce the same leading payload.
    if dmx.seek_to(0, 0).is_ok() {
        if let Ok(p) = dmx.next_packet() {
            if p.stream_index == 0 && built.check_pts {
                assert!(
                    built.tracks[0].payloads.concat().starts_with(&p.data),
                    "post-seek first packet of stream 0 differs"
                );
            }
        }
    }
});
