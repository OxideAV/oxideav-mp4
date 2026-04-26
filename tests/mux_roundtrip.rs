//! Integration tests for the MP4 muxer. We write a small stream via the muxer,
//! then re-parse it via the demuxer in the same crate, and check that the
//! packet bytes + sample tables round-trip cleanly.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Packet, SampleFormat, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};

fn pcm_stream_info() -> StreamInfo {
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

/// 2-channel 48 kHz S16LE: `samples` frames of a trivial ramp.
fn make_pcm_payload(samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 4);
    for i in 0..samples {
        let l = (i as i16).wrapping_mul(7);
        let r = (i as i16).wrapping_mul(11);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

#[test]
fn pcm_roundtrip_byte_exact() {
    // One stream, emit 3 packets of 1024 frames each (stereo s16).
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 1024;
    let total_packets = 3;

    // Generate the packets, then mux them to a temp file.
    let mut sent: Vec<Vec<u8>> = Vec::new();
    for i in 0..total_packets {
        sent.push(make_pcm_payload((frames_per_packet as usize) + i));
    }

    let tmp = std::env::temp_dir().join("oxideav-mp4-pcm-roundtrip.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in sent.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet + i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Demux and verify.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.format_name(), "mp4");
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id,
        CodecId::new("pcm_s16le"),
        "codec_id mismatch in MP4 PCM roundtrip"
    );
    assert_eq!(dmx.streams()[0].params.channels, Some(2));
    assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));

    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }

    // Byte-for-byte packet preservation. Note: our muxer puts each packet in
    // its own chunk (samples_per_chunk_target=1 for PCM), so sample boundaries
    // survive exactly.
    assert_eq!(got.len(), sent.len());
    for (i, (g, s)) in got.iter().zip(sent.iter()).enumerate() {
        assert_eq!(g, s, "packet {i} byte mismatch");
    }
}

#[test]
fn unsupported_codec_fails_at_open() {
    let mut params = CodecParameters::audio(CodecId::new("opus"));
    params.channels = Some(2);
    params.sample_rate = Some(48_000);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let cursor: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    match oxideav_mp4::muxer::open(cursor, &[stream]) {
        Err(oxideav_core::Error::Unsupported(_)) => {}
        Err(other) => panic!("expected Unsupported, got {other:?}"),
        Ok(_) => panic!("expected Unsupported error for opus"),
    }
}

#[test]
fn multi_track_two_streams() {
    // One PCM audio track + one FLAC audio track. Dual-audio is a fine stand-in
    // for audio+video; we avoid pulling in a video codec dependency.
    let pcm = pcm_stream_info();

    // Build a minimal FLAC extradata: just a STREAMINFO metadata block.
    let mut flac_extradata = Vec::new();
    flac_extradata.extend_from_slice(&[0x80, 0, 0, 34]);
    let mut si_payload = [0u8; 34];
    // min/max block size = 4096.
    si_payload[0..2].copy_from_slice(&4096u16.to_be_bytes());
    si_payload[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let packed: u64 = (48_000u64 << 44) | (1u64 << 41) | (15u64 << 36);
    si_payload[10..18].copy_from_slice(&packed.to_be_bytes());
    flac_extradata.extend_from_slice(&si_payload);

    let mut flac_params = CodecParameters::audio(CodecId::new("flac"));
    flac_params.channels = Some(2);
    flac_params.sample_rate = Some(48_000);
    flac_params.sample_format = Some(SampleFormat::S16);
    flac_params.extradata = flac_extradata;
    let flac_stream = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: flac_params,
    };

    let tmp = std::env::temp_dir().join("oxideav-mp4-multitrack.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![pcm.clone(), flac_stream.clone()];
        let mut mux = oxideav_mp4::muxer::open(ws, &streams).unwrap();
        mux.write_header().unwrap();
        // Write a few packets on each stream, interleaved.
        for i in 0..4 {
            let pcm_data = make_pcm_payload(512);
            let mut p = Packet::new(0, pcm.time_base, pcm_data);
            p.pts = Some(i * 512);
            p.duration = Some(512);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();

            // Fake FLAC frame — we don't decode it, just check it survives.
            let flac_payload: Vec<u8> = (0..200).map(|k| ((i * 17 + k) & 0xFF) as u8).collect();
            let mut pf = Packet::new(1, flac_stream.time_base, flac_payload);
            pf.pts = Some(i * 4096);
            pf.duration = Some(4096);
            pf.flags.keyframe = true;
            mux.write_packet(&pf).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 2, "expected 2 tracks");
    // Track order is preserved.
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("pcm_s16le"));
    assert_eq!(dmx.streams()[1].params.codec_id, CodecId::new("flac"));
    assert_eq!(dmx.streams()[1].params.channels, Some(2));
    assert_eq!(dmx.streams()[1].params.sample_rate, Some(48_000));
    // FLAC extradata should be the concatenated metadata blocks — i.e. the
    // original we wrote (demuxer strips the dfLa 4-byte version/flags).
    assert_eq!(
        dmx.streams()[1].params.extradata.len(),
        4 + 34,
        "expected one metadata block (header+payload) to survive round-trip"
    );
}

#[test]
fn flac_packet_bytes_preserved() {
    // FLAC with synthetic packets — make sure packet bytes + extradata survive
    // a muxer→demuxer round trip.
    let mut flac_extradata = Vec::new();
    flac_extradata.extend_from_slice(&[0x80, 0, 0, 34]);
    let mut si = [0u8; 34];
    si[0..2].copy_from_slice(&1024u16.to_be_bytes());
    si[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
    si[10..18].copy_from_slice(&packed.to_be_bytes());
    flac_extradata.extend_from_slice(&si);

    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(2);
    params.sample_rate = Some(44_100);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = flac_extradata.clone();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    };

    let tmp = std::env::temp_dir().join("oxideav-mp4-flac-bytes.mp4");
    let mut sent: Vec<Vec<u8>> = Vec::new();
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..5 {
            // Distinctive per-packet bytes.
            let payload: Vec<u8> = (0..(100 + i))
                .map(|k| ((i * 31 + k) & 0xFF) as u8)
                .collect();
            sent.push(payload.clone());
            let mut p = Packet::new(0, stream.time_base, payload);
            p.pts = Some(i as i64 * 4096);
            p.duration = Some(4096);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("flac"));
    // Extradata round-trips.
    assert_eq!(dmx.streams()[0].params.extradata, flac_extradata);
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), sent.len());
    for (i, (g, s)) in got.iter().zip(sent.iter()).enumerate() {
        assert_eq!(g, s, "FLAC packet {i} byte mismatch");
    }
}

#[test]
fn real_flac_encoder_roundtrip() {
    // End-to-end: PCM samples → FLAC encoder → MP4 muxer → MP4 demuxer → FLAC
    // decoder. Verifies both that packet bytes survive AND that the FLAC
    // extradata written via dfLa is valid (the decoder accepts it).
    use oxideav_core::{AudioFrame, Frame};

    let sample_rate: u32 = 48_000;
    let channels: u16 = 2;
    let frames_per_block: u32 = 4096;

    // Synthesize 2 blocks of sine-wave audio (pattern used in the FLAC codec's
    // own bit-exact round-trip test — avoids a pre-existing decoder corner case
    // with trivial ramps).
    let total_frames = (frames_per_block as usize) * 2;
    let mut pcm_i16 = Vec::with_capacity(total_frames * channels as usize);
    for i in 0..total_frames {
        let base =
            (i as f64 / sample_rate as f64 * 330.0 * 2.0 * std::f64::consts::PI).sin() * 15_000.0;
        let l = base as i16;
        let r = (base * 0.8) as i16;
        pcm_i16.push(l);
        pcm_i16.push(r);
    }
    let mut pcm_bytes = Vec::with_capacity(pcm_i16.len() * 2);
    for s in &pcm_i16 {
        pcm_bytes.extend_from_slice(&s.to_le_bytes());
    }

    // Build FLAC encoder.
    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(SampleFormat::S16);
    let mut encoder = oxideav_flac::encoder::make_encoder(&enc_params).unwrap();

    // Encode: feed one AudioFrame containing all samples, then flush.
    let frame = AudioFrame {
        samples: total_frames as u32,
        pts: Some(0),
        data: vec![pcm_bytes.clone()],
    };
    encoder.send_frame(&Frame::Audio(frame)).unwrap();
    encoder.flush().unwrap();

    let mut packets = Vec::new();
    loop {
        match encoder.receive_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(oxideav_core::Error::NeedMore) => break,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("encoder error: {e}"),
        }
    }
    assert!(!packets.is_empty(), "FLAC encoder produced no packets");
    let extradata = encoder.output_params().extradata.clone();
    assert!(!extradata.is_empty());

    // Mux to MP4.
    let mut stream_params = CodecParameters::audio(CodecId::new("flac"));
    stream_params.channels = Some(channels);
    stream_params.sample_rate = Some(sample_rate);
    stream_params.sample_format = Some(SampleFormat::S16);
    stream_params.extradata = extradata.clone();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params: stream_params,
    };

    let tmp = std::env::temp_dir().join("oxideav-mp4-real-flac.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for pkt in &packets {
            mux.write_packet(pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Demux and decode.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("flac"));
    let decoded_extradata = dmx.streams()[0].params.extradata.clone();
    assert_eq!(decoded_extradata, extradata);

    let decoder_params = dmx.streams()[0].params.clone();
    let mut decoder = oxideav_flac::decoder::make_decoder(&decoder_params).unwrap();

    let mut demuxed_packets = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => demuxed_packets.push(p),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(demuxed_packets.len(), packets.len());
    // Packet bytes identical.
    for (i, (a, b)) in demuxed_packets.iter().zip(packets.iter()).enumerate() {
        assert_eq!(
            a.data.len(),
            b.data.len(),
            "FLAC packet {i} size mismatch: got {} expected {}",
            a.data.len(),
            b.data.len()
        );
        assert_eq!(
            a.data, b.data,
            "FLAC packet {i} byte mismatch after MP4 roundtrip"
        );
    }

    // Sanity check: also verify the decoder can eat the ORIGINAL encoder
    // packets directly (without MP4). If this fails the bug is in the FLAC
    // codec, not the MP4 muxer.
    let mut baseline_decoder =
        oxideav_flac::decoder::make_decoder(encoder.output_params()).unwrap();
    for pkt in &packets {
        baseline_decoder.send_packet(pkt).unwrap();
        loop {
            match baseline_decoder.receive_frame() {
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("baseline decoder error on original packet: {e}"),
            }
        }
    }

    // Decode all packets.
    let mut decoded: Vec<i16> = Vec::new();
    for pkt in &demuxed_packets {
        decoder.send_packet(pkt).unwrap();
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Audio(a)) => {
                    for plane in &a.data {
                        for chunk in plane.chunks_exact(2) {
                            decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                        }
                    }
                }
                Ok(_) => {}
                Err(oxideav_core::Error::NeedMore) => break,
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("decoder error: {e}"),
            }
        }
    }
    decoder.flush().unwrap();
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(a)) => {
                for plane in &a.data {
                    for chunk in plane.chunks_exact(2) {
                        decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Bit-exact reconstruction.
    assert_eq!(decoded.len(), pcm_i16.len(), "decoded sample count differs");
    assert_eq!(
        decoded, pcm_i16,
        "decoded samples are not bit-exact after MP4 roundtrip"
    );
}

#[test]
fn mjpeg_roundtrip_via_mp4() {
    // Encode a tiny video frame to JPEG, mux it into MP4 as "mjpeg",
    // demux back, check the sample entry → codec_id mapping yields
    // "mjpeg" and that the decoded bytes round-trip.
    use oxideav_core::CodecRegistry;
    use oxideav_core::{Frame, MediaType, PixelFormat, VideoFrame, VideoPlane};

    // Build a synthetic 64x64 Yuv420P frame (gradient).
    let w = 64u32;
    let h = 64u32;
    let chroma_w = (w / 2) as usize;
    let chroma_h = (h / 2) as usize;
    let y_plane: Vec<u8> = (0..(w * h) as usize).map(|i| (i % 256) as u8).collect();
    let cb_plane: Vec<u8> = vec![128u8; chroma_w * chroma_h];
    let cr_plane: Vec<u8> = vec![128u8; chroma_w * chroma_h];

    let time_base = TimeBase::new(1, 25); // 25 fps
    let frame = Frame::Video(VideoFrame {
        pts: Some(0),
        planes: vec![
            VideoPlane {
                stride: w as usize,
                data: y_plane,
            },
            VideoPlane {
                stride: chroma_w,
                data: cb_plane,
            },
            VideoPlane {
                stride: chroma_w,
                data: cr_plane,
            },
        ],
    });

    // Encode one JPEG packet.
    let mut codecs = CodecRegistry::new();
    oxideav_mjpeg::register(&mut codecs);

    let mut enc_params = CodecParameters::video(CodecId::new("mjpeg"));
    enc_params.media_type = MediaType::Video;
    enc_params.width = Some(w);
    enc_params.height = Some(h);
    enc_params.pixel_format = Some(PixelFormat::Yuv420P);
    let mut enc = codecs.make_encoder(&enc_params).expect("mjpeg encoder");
    enc.send_frame(&frame).unwrap();
    let jpeg_bytes = match enc.receive_packet() {
        Ok(p) => p.data,
        Err(e) => panic!("encoder did not produce packet: {e:?}"),
    };
    assert!(!jpeg_bytes.is_empty());
    assert_eq!(
        &jpeg_bytes[0..2],
        &[0xFF, 0xD8],
        "encoded frame starts with SOI"
    );

    // Mux to a tempfile, then demux back.
    let stream_in = StreamInfo {
        index: 0,
        time_base,
        duration: None,
        start_time: Some(0),
        params: enc_params.clone(),
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-mjpeg-roundtrip.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut muxer = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream_in)).unwrap();
        muxer.write_header().unwrap();
        let mut pkt = Packet::new(0, time_base, jpeg_bytes.clone());
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        muxer.write_packet(&pkt).unwrap();
        muxer.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut demuxer = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let streams = demuxer.streams().to_vec();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].params.codec_id.as_str(), "mjpeg");
    assert_eq!(streams[0].params.media_type, MediaType::Video);
    assert_eq!(streams[0].params.width, Some(w));
    assert_eq!(streams[0].params.height, Some(h));

    let out_pkt = demuxer.next_packet().unwrap();
    assert_eq!(
        out_pkt.data, jpeg_bytes,
        "MP4 roundtrip preserves JPEG bytes"
    );
    assert!(matches!(
        demuxer.next_packet(),
        Err(oxideav_core::Error::Eof)
    ));
}

// --- Brand presets + faststart --------------------------------------------

use oxideav_core::ContainerRegistry;
use oxideav_mp4::{BrandPreset, Mp4MuxerOptions};

#[test]
fn mov_registry_entry_exists() {
    let mut reg = ContainerRegistry::new();
    oxideav_mp4::register(&mut reg);
    let names: Vec<&str> = reg.muxer_names().collect();
    assert!(
        names.contains(&"mov"),
        "expected 'mov' in muxer_names(), got {names:?}"
    );
}

#[test]
fn ismv_registry_entry_exists() {
    let mut reg = ContainerRegistry::new();
    oxideav_mp4::register(&mut reg);
    let names: Vec<&str> = reg.muxer_names().collect();
    assert!(
        names.contains(&"ismv"),
        "expected 'ismv' in muxer_names(), got {names:?}"
    );
}

/// Extract the ftyp major_brand (4 bytes immediately after the 8-byte box header).
fn read_ftyp_major_brand(bytes: &[u8]) -> [u8; 4] {
    // Top-level ftyp is first box: [size u32][kind "ftyp"][body...]
    assert_eq!(
        &bytes[4..8],
        b"ftyp",
        "expected first top-level box to be ftyp"
    );
    let mut brand = [0u8; 4];
    brand.copy_from_slice(&bytes[8..12]);
    brand
}

/// Walk the top-level box list and return the 4-byte types in order.
fn top_level_box_types(bytes: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(&bytes[pos + 4..pos + 8]);
        out.push(kind);
        if size == 0 {
            break;
        }
        pos += size;
    }
    out
}

#[test]
fn mov_brand_in_ftyp() {
    let stream = pcm_stream_info();
    let tmp = std::env::temp_dir().join("oxideav-mp4-mov-brand.mov");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            brand: BrandPreset::Mov,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        let mut pkt = Packet::new(0, stream.time_base, make_pcm_payload(1024));
        pkt.pts = Some(0);
        pkt.duration = Some(1024);
        pkt.flags.keyframe = true;
        mux.write_packet(&pkt).unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let brand = read_ftyp_major_brand(&bytes);
    assert_eq!(&brand, b"qt  ", "expected MOV major brand 'qt  '");
}

#[test]
fn mp4_faststart_has_moov_before_mdat() {
    let stream = pcm_stream_info();
    let tmp = std::env::temp_dir().join("oxideav-mp4-faststart-order.mp4");
    let frames_per_packet: i64 = 1024;
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            faststart: true,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for i in 0..3 {
            let payload = make_pcm_payload(frames_per_packet as usize + i);
            let mut pkt = Packet::new(0, stream.time_base, payload);
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet + i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let kinds = top_level_box_types(&bytes);
    // With faststart we expect ftyp, then moov, then mdat.
    let ftyp_idx = kinds.iter().position(|k| k == b"ftyp").expect("has ftyp");
    let moov_idx = kinds.iter().position(|k| k == b"moov").expect("has moov");
    let mdat_idx = kinds.iter().position(|k| k == b"mdat").expect("has mdat");
    assert_eq!(ftyp_idx, 0, "ftyp must be first");
    assert!(
        moov_idx < mdat_idx,
        "expected moov before mdat in faststart layout, got kinds={kinds:?}"
    );

    // Demuxer still accepts it.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("pcm_s16le"));
    let mut got_count = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => got_count += 1,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got_count, 3);
}

#[test]
fn faststart_roundtrip_pcm() {
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 1024;
    let total_packets = 3;

    let mut sent: Vec<Vec<u8>> = Vec::new();
    for i in 0..total_packets {
        sent.push(make_pcm_payload((frames_per_packet as usize) + i));
    }

    let tmp = std::env::temp_dir().join("oxideav-mp4-faststart-pcm.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            faststart: true,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in sent.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet + i as i64);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id, CodecId::new("pcm_s16le"));
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), sent.len());
    for (i, (g, s)) in got.iter().zip(sent.iter()).enumerate() {
        assert_eq!(g, s, "packet {i} byte mismatch");
    }
}

#[test]
fn faststart_roundtrip_flac() {
    use oxideav_core::{AudioFrame, Frame};

    let sample_rate: u32 = 48_000;
    let channels: u16 = 2;
    let frames_per_block: u32 = 4096;

    let total_frames = (frames_per_block as usize) * 2;
    let mut pcm_i16 = Vec::with_capacity(total_frames * channels as usize);
    for i in 0..total_frames {
        let base =
            (i as f64 / sample_rate as f64 * 330.0 * 2.0 * std::f64::consts::PI).sin() * 15_000.0;
        let l = base as i16;
        let r = (base * 0.8) as i16;
        pcm_i16.push(l);
        pcm_i16.push(r);
    }
    let mut pcm_bytes = Vec::with_capacity(pcm_i16.len() * 2);
    for s in &pcm_i16 {
        pcm_bytes.extend_from_slice(&s.to_le_bytes());
    }

    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(SampleFormat::S16);
    let mut encoder = oxideav_flac::encoder::make_encoder(&enc_params).unwrap();

    let frame = AudioFrame {
        samples: total_frames as u32,
        pts: Some(0),
        data: vec![pcm_bytes.clone()],
    };
    encoder.send_frame(&Frame::Audio(frame)).unwrap();
    encoder.flush().unwrap();

    let mut packets = Vec::new();
    loop {
        match encoder.receive_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(oxideav_core::Error::NeedMore) => break,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("encoder error: {e}"),
        }
    }
    assert!(!packets.is_empty());
    let extradata = encoder.output_params().extradata.clone();

    let mut stream_params = CodecParameters::audio(CodecId::new("flac"));
    stream_params.channels = Some(channels);
    stream_params.sample_rate = Some(sample_rate);
    stream_params.sample_format = Some(SampleFormat::S16);
    stream_params.extradata = extradata.clone();
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params: stream_params,
    };

    let tmp = std::env::temp_dir().join("oxideav-mp4-faststart-flac.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            faststart: true,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for pkt in &packets {
            mux.write_packet(pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Sanity: verify moov precedes mdat on disk.
    let raw = std::fs::read(&tmp).unwrap();
    let kinds = top_level_box_types(&raw);
    let moov_idx = kinds.iter().position(|k| k == b"moov").unwrap();
    let mdat_idx = kinds.iter().position(|k| k == b"mdat").unwrap();
    assert!(moov_idx < mdat_idx, "moov must precede mdat with faststart");

    // Decode and compare bit-exact.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.extradata, extradata);
    let decoder_params = dmx.streams()[0].params.clone();
    let mut decoder = oxideav_flac::decoder::make_decoder(&decoder_params).unwrap();

    let mut decoded: Vec<i16> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                decoder.send_packet(&pkt).unwrap();
                loop {
                    match decoder.receive_frame() {
                        Ok(Frame::Audio(a)) => {
                            for plane in &a.data {
                                for chunk in plane.chunks_exact(2) {
                                    decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(oxideav_core::Error::NeedMore) => break,
                        Err(oxideav_core::Error::Eof) => break,
                        Err(e) => panic!("decoder error: {e}"),
                    }
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    decoder.flush().unwrap();
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(a)) => {
                for plane in &a.data {
                    for chunk in plane.chunks_exact(2) {
                        decoded.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(decoded.len(), pcm_i16.len());
    assert_eq!(
        decoded, pcm_i16,
        "bit-exact PCM reconstruction required after MP4 faststart + FLAC roundtrip"
    );
}

#[test]
fn chunk_offsets_patched_after_faststart() {
    // Emit multiple chunks (pcm_s16le => 1 sample per chunk, so N packets
    // yields N chunks), then confirm the demuxer returns byte-exact packet
    // data after faststart. This implicitly exercises chunk-offset patching:
    // if offsets weren't shifted by moov_size, the demuxer would read garbage.
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 512;
    let total_packets = 8;

    let mut sent: Vec<Vec<u8>> = Vec::new();
    for i in 0..total_packets {
        // Distinctive per-packet pattern so mis-seeking is loud.
        let mut p = Vec::with_capacity(frames_per_packet as usize * 4);
        for k in 0..(frames_per_packet as usize) {
            let l = ((i as i16) * 1000 + k as i16).wrapping_mul(3);
            let r = ((i as i16) * 2000 + k as i16).wrapping_mul(5);
            p.extend_from_slice(&l.to_le_bytes());
            p.extend_from_slice(&r.to_le_bytes());
        }
        sent.push(p);
    }

    let tmp = std::env::temp_dir().join("oxideav-mp4-faststart-chunks.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let opts = Mp4MuxerOptions {
            faststart: true,
            ..Mp4MuxerOptions::default()
        };
        let mut mux =
            oxideav_mp4::muxer::open_with_options(ws, std::slice::from_ref(&stream), opts).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in sent.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    let mut got: Vec<Vec<u8>> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => got.push(p.data),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(got.len(), sent.len());
    for (i, (g, s)) in got.iter().zip(sent.iter()).enumerate() {
        assert_eq!(
            g, s,
            "packet {i} byte mismatch — chunk offset probably not patched after faststart"
        );
    }
}

#[test]
fn seek_to_nearest_keyframe() {
    // Mux a PCM stream where every packet is a keyframe (since PCM is intra-only).
    // Seek to a target pts and verify that the next packet produced comes from
    // a sample whose pts <= target.
    let stream = pcm_stream_info();
    let frames_per_packet: i64 = 1024;
    let total_packets = 10;

    let tmp = std::env::temp_dir().join("oxideav-mp4-seek.mp4");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mp4::muxer::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for i in 0..total_packets {
            let payload = make_pcm_payload(frames_per_packet as usize);
            let mut pkt = Packet::new(0, stream.time_base, payload);
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();

    // Seek to pts just past sample 5's start. Should land on sample 5.
    let target_pts = 5 * frames_per_packet + 100;
    let landed = dmx
        .seek_to(0, target_pts)
        .expect("MP4 demuxer should support seeking");
    assert_eq!(
        landed,
        5 * frames_per_packet,
        "expected to land on keyframe at pts={}",
        5 * frames_per_packet
    );
    // Next packet should have pts == landed.
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.pts, Some(landed));

    // Seek to pts 0 — should land on sample 0.
    let landed = dmx.seek_to(0, 0).unwrap();
    assert_eq!(landed, 0);
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.pts, Some(0));

    // Seek far past end — should land on the last keyframe.
    let landed = dmx.seek_to(0, i64::MAX / 2).unwrap();
    assert_eq!(landed, (total_packets as i64 - 1) * frames_per_packet);
}

// --- OTI-based codec dispatch --------------------------------------------

/// Build a minimal but fully-valid mp4 file where the only track uses the
/// `mp4a` sample entry with an `esds` box whose `objectTypeIndication`
/// picks `mpeg1_audio` (0x6B). This is the on-disk shape that MP4s carrying
/// MP3 audio use; the demuxer should resolve it to `CodecId("mp3")`
/// rather than the default `CodecId("aac")`.
fn build_mp4_with_mp4a_oti(oti: u8) -> Vec<u8> {
    fn box_be(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let total = (8 + body.len()) as u32;
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }
    fn u32be(x: u32) -> [u8; 4] {
        x.to_be_bytes()
    }
    fn u16be(x: u16) -> [u8; 2] {
        x.to_be_bytes()
    }

    // ftyp
    let mut ftyp_body = Vec::new();
    ftyp_body.extend_from_slice(b"mp42");
    ftyp_body.extend_from_slice(&u32be(0x0000_0200));
    ftyp_body.extend_from_slice(b"isom");
    ftyp_body.extend_from_slice(b"mp42");
    let ftyp = box_be(b"ftyp", &ftyp_body);

    // mdat (just one byte payload — demuxer only cares about the track tables).
    let mdat = box_be(b"mdat", &[0x00]);
    let mdat_payload_offset: u32 = (ftyp.len() + 8) as u32;

    // esds body: FullBox (4 bytes) + ES_Descriptor(0x03) { ES_ID u16 + flags u8
    //   + DecoderConfigDescriptor(0x04) {
    //       objectTypeIndication u8, streamType+flags u8,
    //       bufferSizeDB u24, maxBitrate u32, avgBitrate u32,
    //       DecoderSpecificInfo(0x05) — we leave this empty for the synthetic test.
    //     }
    //   + SLConfigDescriptor(0x06) { predefined=0x02 }
    // }
    // We use single-byte BER length encodings throughout.
    let dcd_body = {
        let mut v = Vec::new();
        v.push(oti); // objectTypeIndication
        v.push((0x05u8 << 2) | 0x01); // streamType=audio(5), upstream=0, reserved=1
        v.extend_from_slice(&[0, 0, 0]); // bufferSizeDB (u24)
        v.extend_from_slice(&[0, 0, 0, 0]); // maxBitrate
        v.extend_from_slice(&[0, 0, 0, 0]); // avgBitrate
        v
    };
    let mut dcd = vec![0x04u8, dcd_body.len() as u8];
    dcd.extend_from_slice(&dcd_body);

    let slc = vec![0x06u8, 0x01, 0x02];

    let mut esd = Vec::new();
    esd.push(0x03); // ES_Descriptor tag
    let esd_body_len = 3 + dcd.len() + slc.len();
    esd.push(esd_body_len as u8);
    esd.extend_from_slice(&[0, 0, 0]); // ES_ID (u16) + flags (u8)
    esd.extend_from_slice(&dcd);
    esd.extend_from_slice(&slc);

    let mut esds_body = vec![0u8, 0, 0, 0]; // FullBox version + flags
    esds_body.extend_from_slice(&esd);
    let esds = box_be(b"esds", &esds_body);

    // mp4a sample entry: 28-byte AudioSampleEntryV0 preamble + esds.
    let mut mp4a_body = Vec::new();
    mp4a_body.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // 6 bytes reserved
    mp4a_body.extend_from_slice(&u16be(1)); // data_reference_index
    mp4a_body.extend_from_slice(&[0u8; 8]); // reserved
    mp4a_body.extend_from_slice(&u16be(1)); // channel_count
    mp4a_body.extend_from_slice(&u16be(16)); // sample_size
    mp4a_body.extend_from_slice(&[0u8; 4]); // pre_defined + reserved
    mp4a_body.extend_from_slice(&u32be(44_100 << 16)); // sample_rate 16.16
    mp4a_body.extend_from_slice(&esds);
    let mp4a = box_be(b"mp4a", &mp4a_body);

    // stsd: FullBox + entry_count(u32=1) + mp4a sample entry.
    let mut stsd_body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
    stsd_body.extend_from_slice(&mp4a);
    let stsd = box_be(b"stsd", &stsd_body);

    // stts: 1 entry (1 sample, delta=1).
    let mut stts_body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
    stts_body.extend_from_slice(&u32be(1));
    stts_body.extend_from_slice(&u32be(1));
    let stts = box_be(b"stts", &stts_body);

    // stsc: 1 entry — chunk 1 has 1 sample, sample desc idx 1.
    let mut stsc_body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
    stsc_body.extend_from_slice(&u32be(1));
    stsc_body.extend_from_slice(&u32be(1));
    stsc_body.extend_from_slice(&u32be(1));
    let stsc = box_be(b"stsc", &stsc_body);

    // stsz: uniform size = 1 byte per sample.
    let mut stsz_body = vec![0u8, 0, 0, 0];
    stsz_body.extend_from_slice(&u32be(1)); // uniform
    stsz_body.extend_from_slice(&u32be(1)); // count
    let stsz = box_be(b"stsz", &stsz_body);

    // stco: 1 entry pointing at mdat payload start.
    let mut stco_body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
    stco_body.extend_from_slice(&u32be(mdat_payload_offset));
    let stco = box_be(b"stco", &stco_body);

    // stbl
    let mut stbl_body = Vec::new();
    stbl_body.extend_from_slice(&stsd);
    stbl_body.extend_from_slice(&stts);
    stbl_body.extend_from_slice(&stsc);
    stbl_body.extend_from_slice(&stsz);
    stbl_body.extend_from_slice(&stco);
    let stbl = box_be(b"stbl", &stbl_body);

    // dref: FullBox + count=1 + url " (flags=1, self-contained)
    let mut dref_body = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
    let url_box = box_be(b"url ", &[0u8, 0, 0, 1]);
    dref_body.extend_from_slice(&url_box);
    let dref = box_be(b"dref", &dref_body);
    let dinf = box_be(b"dinf", &dref);

    // smhd (FullBox)
    let smhd = box_be(b"smhd", &[0u8, 0, 0, 0, 0, 0, 0, 0]);

    let mut minf_body = Vec::new();
    minf_body.extend_from_slice(&smhd);
    minf_body.extend_from_slice(&dinf);
    minf_body.extend_from_slice(&stbl);
    let minf = box_be(b"minf", &minf_body);

    // hdlr — handler type "soun"
    let mut hdlr_body = vec![0u8, 0, 0, 0]; // FullBox
    hdlr_body.extend_from_slice(&u32be(0)); // pre_defined
    hdlr_body.extend_from_slice(b"soun"); // handler_type
    hdlr_body.extend_from_slice(&[0u8; 12]); // reserved
    hdlr_body.extend_from_slice(b"SoundHandler\0");
    let hdlr = box_be(b"hdlr", &hdlr_body);

    // mdhd (version 0): 4-byte FullBox + 4-byte creation + 4-byte modification
    //   + 4-byte timescale + 4-byte duration + 2-byte language + 2-byte pre_defined
    let mut mdhd_body = vec![0u8, 0, 0, 0];
    mdhd_body.extend_from_slice(&u32be(0)); // creation
    mdhd_body.extend_from_slice(&u32be(0)); // modification
    mdhd_body.extend_from_slice(&u32be(44_100)); // timescale
    mdhd_body.extend_from_slice(&u32be(1)); // duration
    mdhd_body.extend_from_slice(&u16be(0x55c4)); // "und"
    mdhd_body.extend_from_slice(&u16be(0));
    let mdhd = box_be(b"mdhd", &mdhd_body);

    let mut mdia_body = Vec::new();
    mdia_body.extend_from_slice(&mdhd);
    mdia_body.extend_from_slice(&hdlr);
    mdia_body.extend_from_slice(&minf);
    let mdia = box_be(b"mdia", &mdia_body);

    // tkhd (version 0): 92 bytes total — we only need it to be parsed-and-ignored
    // by the demuxer, which skips unknown-to-it children of `trak`. So just a
    // minimal tkhd won't be required; but structurally `trak` must contain
    // `mdia`, which it does.
    let trak = box_be(b"trak", &mdia);

    // mvhd (version 0): similar to tkhd — demuxer's parse_mvhd reads it.
    let mut mvhd_body = vec![0u8, 0, 0, 0];
    mvhd_body.extend_from_slice(&u32be(0)); // creation
    mvhd_body.extend_from_slice(&u32be(0)); // modification
    mvhd_body.extend_from_slice(&u32be(1000)); // timescale
    mvhd_body.extend_from_slice(&u32be(1)); // duration
    mvhd_body.extend_from_slice(&u32be(0x0001_0000)); // rate
    mvhd_body.extend_from_slice(&u16be(0x0100)); // volume
    mvhd_body.extend_from_slice(&[0u8; 10]); // reserved u16 + 2x u32
    let identity: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];
    for v in identity {
        mvhd_body.extend_from_slice(&u32be(v));
    }
    mvhd_body.extend_from_slice(&[0u8; 24]); // pre_defined (6x u32)
    mvhd_body.extend_from_slice(&u32be(2)); // next_track_id
    let mvhd = box_be(b"mvhd", &mvhd_body);

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd);
    moov_body.extend_from_slice(&trak);
    let moov = box_be(b"moov", &moov_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&mdat);
    out.extend_from_slice(&moov);
    out
}

#[test]
fn mp4a_mpeg1_audio_oti_resolves_to_mp3() {
    // OTI 0x6B = MPEG-1 Audio Layer I/II/III. Historically these are
    // packaged in MP4 as `mp4a` + esds; we should demux them as mp3.
    let bytes = build_mp4_with_mp4a_oti(0x6B);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(
        dmx.streams()[0].params.codec_id.as_str(),
        "mp3",
        "MP4 OTI=0x6B should resolve to 'mp3' (was 'aac' before OTI dispatch)"
    );
}

#[test]
fn mp4a_mpeg2_audio_oti_resolves_to_mp3() {
    // OTI 0x69 = MPEG-2 Audio Part 3.
    let bytes = build_mp4_with_mp4a_oti(0x69);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "mp3");
}

#[test]
fn mp4a_aac_oti_resolves_to_aac() {
    // OTI 0x40 = AAC. Baseline check that the historical path is preserved.
    let bytes = build_mp4_with_mp4a_oti(0x40);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mp4::demux::open(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(dmx.streams()[0].params.codec_id.as_str(), "aac");
}
