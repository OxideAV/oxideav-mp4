//! Integration tests for `prft` (ProducerReferenceTimeBox, ISO/IEC
//! 14496-12 §8.16.5) *muxing* in the fragmented-MP4 writer.
//!
//! Round-trip strategy: drive the concrete [`FragmentedMuxer`] (via
//! `open_fragmented_typed`), queue a `prft` before each fragment with
//! [`FragmentedMuxer::set_next_segment_prft`], then re-parse the emitted
//! bytes — both by scanning for every top-level `prft` box and decoding
//! it with the public `parse_prft_box`, and by running the produced file
//! back through the fragmented demuxer to confirm `prft` emission does
//! not disturb the sample-table round-trip.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::demux::parse_prft_box;
use oxideav_mp4::frag::open_fragmented_typed;
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

// --- scaffolding ----------------------------------------------------------

fn pcm_stream() -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    params.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn make_pcm_packets(n: usize, frames: i64) -> Vec<Packet> {
    let stream = pcm_stream();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut payload = Vec::with_capacity(frames as usize * 4);
        for j in 0..frames as usize {
            let l = ((i * 1024 + j) as i16).wrapping_mul(7);
            let r = ((i * 1024 + j) as i16).wrapping_mul(11);
            payload.extend_from_slice(&l.to_le_bytes());
            payload.extend_from_slice(&r.to_le_bytes());
        }
        let mut p = Packet::new(0, stream.time_base, payload);
        p.pts = Some((i as i64) * frames);
        p.dts = Some((i as i64) * frames);
        p.duration = Some(frames);
        p.flags.keyframe = true;
        out.push(p);
    }
    out
}

fn frag_options(emit_indexes: bool, styp: bool) -> (Mp4MuxerOptions, FragmentedOptions) {
    let frag = FragmentedOptions {
        // One fragment per packet → predictable per-fragment prft.
        cadence: FragmentCadence::EveryNPackets(1),
        styp: if styp {
            Some(BrandPreset::Custom {
                major: *b"msdh",
                compatible: vec![*b"msdh", *b"msix"],
            })
        } else {
            None
        },
        emit_random_access_indexes: emit_indexes,
        levels: Vec::new(),
        emit_ssix: false,
        ssix_levels: (1, 2),
        treps: Vec::new(),
    };
    let opts = Mp4MuxerOptions {
        brand: BrandPreset::Custom {
            major: *b"iso6",
            compatible: vec![*b"iso6", *b"mp41", *b"dash"],
        },
        faststart: false,
        fragmented: Some(frag.clone()),
        write_edit_list: true,
        track_sample_groups: Vec::new(),
        large_mdat: false,
        ..Mp4MuxerOptions::default()
    };
    (opts, frag)
}

/// Collect the (offset, body) of every top-level `prft` box in `bytes`,
/// walking the box tree only at the file's top level (prft is a
/// file-level box per §8.16.5).
fn scan_prft_boxes(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let total = if size == 1 {
            // 64-bit largesize — none of our boxes use it; bail.
            break;
        } else if size == 0 {
            bytes.len() - pos
        } else {
            size
        };
        if total < 8 || pos + total > bytes.len() {
            break;
        }
        if kind == b"prft" {
            out.push(bytes[pos + 8..pos + total].to_vec());
        }
        pos += total;
    }
    out
}

// --- tests ----------------------------------------------------------------

/// A `prft` queued before each fragment round-trips byte-exactly through
/// `parse_prft_box`, and the box version auto-selects v0 for a small
/// `media_time`.
#[test]
fn prft_per_fragment_v0_roundtrips() {
    let stream = pcm_stream();
    let packets = make_pcm_packets(3, 1024);

    // NTP: 2024-01-01T00:00:00Z ≈ 3_913_056_000 seconds since 1900.
    let base_ntp_secs: u64 = 3_913_056_000;
    let mut expected: Vec<(u32, u64, u64)> = Vec::new();

    let path = std::env::temp_dir().join(format!("oxideav-mp4-prft-{}.mp4", std::process::id()));
    {
        let (opts, frag) = frag_options(true, true);
        let f = std::fs::File::create(&path).unwrap();
        let out: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            open_fragmented_typed(out, std::slice::from_ref(&stream), opts, frag).unwrap();
        mux.write_header().unwrap();
        for (i, p) in packets.iter().enumerate() {
            // Each fragment is one packet; queue a prft naming track 1, an
            // NTP one second apart, media_time = i*1024 (fragment bmdt).
            let ntp = (base_ntp_secs + i as u64) << 32;
            let media = (i as u64) * 1024;
            mux.set_next_segment_prft(1, ntp, media, 0);
            expected.push((1, ntp, media));
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();

    let prfts = scan_prft_boxes(&bytes);
    assert_eq!(
        prfts.len(),
        expected.len(),
        "one prft per fragment expected"
    );
    for (raw, (rt, ntp, media)) in prfts.iter().zip(expected.iter()) {
        // v0 layout: 4 (ver/flags) + 4 (track) + 8 (ntp) + 4 (media) = 20.
        assert_eq!(raw.len(), 20, "small media_time → version 0 (u32)");
        assert_eq!(raw[0], 0, "version 0");
        let rec = parse_prft_box(raw).unwrap().unwrap();
        assert_eq!(rec.reference_track_id, *rt);
        assert_eq!(rec.ntp_timestamp, *ntp);
        assert_eq!(rec.media_time, *media);
        assert_eq!(rec.version, 0);
        assert_eq!(rec.flags, 0);
    }
}

/// `set_next_segment_prft_v1` forces a 64-bit `media_time` box even when
/// the value fits in u32; flags annotation bits round-trip.
#[test]
fn prft_forced_v1_and_flags_roundtrip() {
    let stream = pcm_stream();
    let packets = make_pcm_packets(2, 1024);
    let (opts, frag) = frag_options(false, false);

    let path = std::env::temp_dir().join(format!("oxideav-mp4-prftv1-{}.mp4", std::process::id()));
    let f = std::fs::File::create(&path).unwrap();
    let out: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = open_fragmented_typed(out, std::slice::from_ref(&stream), opts, frag).unwrap();
    mux.write_header().unwrap();

    // 0x000004 = file_write_time annotation bit (2022 edition).
    let ntp: u64 = 3_913_056_000u64 << 32;
    mux.set_next_segment_prft_v1(1, ntp, 0, 0x00_0004);
    mux.write_packet(&packets[0]).unwrap();
    mux.set_next_segment_prft_v1(1, ntp, 1024, 0x00_0004);
    mux.write_packet(&packets[1]).unwrap();
    mux.write_trailer().unwrap();

    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();

    let prfts = scan_prft_boxes(&bytes);
    assert_eq!(prfts.len(), 2);
    for raw in &prfts {
        // v1 layout: 4 + 4 + 8 + 8 = 24 bytes.
        assert_eq!(raw.len(), 24, "forced version 1 → u64 media_time");
        assert_eq!(raw[0], 1, "version 1");
        let rec = parse_prft_box(raw).unwrap().unwrap();
        assert_eq!(rec.version, 1);
        assert_eq!(rec.reference_track_id, 1);
        assert_eq!(rec.ntp_timestamp, ntp);
        assert_eq!(rec.flags, 0x00_0004);
        assert!(rec.is_file_write_time());
        assert!(!rec.is_finalization_time());
    }
}

/// Queuing a `prft` does not disturb the fragmented sample-table
/// round-trip: the demuxer still recovers every packet's payload.
#[test]
fn prft_does_not_disturb_sample_roundtrip() {
    let stream = pcm_stream();
    let packets = make_pcm_packets(4, 1024);
    let (opts, frag) = frag_options(true, true);

    let path = std::env::temp_dir().join(format!("oxideav-mp4-prftrt-{}.mp4", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let out: Box<dyn WriteSeek> = Box::new(f);
        let mut mux =
            open_fragmented_typed(out, std::slice::from_ref(&stream), opts, frag).unwrap();
        mux.write_header().unwrap();
        let ntp: u64 = 3_913_056_000u64 << 32;
        for (i, p) in packets.iter().enumerate() {
            // prft only on even fragments to also exercise the
            // "no prft this fragment" path.
            if i % 2 == 0 {
                mux.set_next_segment_prft(1, ntp + ((i as u64) << 32), (i as u64) * 1024, 0);
            }
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();

    // Two prfts (fragments 0 and 2).
    assert_eq!(scan_prft_boxes(&bytes).len(), 2);

    // Re-demux and compare payloads.
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mp4::demux::open(input, &oxideav_core::NullCodecResolver).unwrap();
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => got.push(pkt.data.to_vec()),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), packets.len(), "all samples recovered");
    for (a, b) in got.iter().zip(packets.iter()) {
        assert_eq!(a, &b.data, "sample payload byte-exact");
    }
}
