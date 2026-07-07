//! Fragmented CENC packaging — per-fragment `senc` emission with
//! matching `saiz` / `saio` (ISO/IEC 23001-7 §7.1–7.2 + ISO/IEC
//! 14496-12 §8.7.8–9 / §8.8.14) through
//! `FragmentedMuxer::write_protected_packet`, verified by demuxing the
//! produced file back through this crate's own fragment walk.

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mp4::cenc::{SencSample, SubsampleEntry, TencBox};
use oxideav_mp4::{FragmentCadence, FragmentedOptions, Mp4MuxerOptions, TrackProtection};

fn pcm_stream(index: u32) -> StreamInfo {
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

fn tenc_iv8(kid: [u8; 16]) -> TencBox {
    TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: kid,
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    }
}

fn tenc_cbcs(kid: [u8; 16], constant_iv: Vec<u8>) -> TencBox {
    TencBox {
        version: 1,
        default_is_protected: 1,
        default_per_sample_iv_size: 0,
        default_kid: kid,
        default_crypt_byte_block: 1,
        default_skip_byte_block: 9,
        default_constant_iv: Some(constant_iv),
    }
}

fn frag_opts(n: u32) -> FragmentedOptions {
    FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(n),
        emit_random_access_indexes: false,
        styp: None,
        ..FragmentedOptions::default()
    }
}

fn protected_options(scheme: [u8; 4], tenc: TencBox, frag: FragmentedOptions) -> Mp4MuxerOptions {
    Mp4MuxerOptions {
        fragmented: Some(frag),
        track_protection: vec![TrackProtection {
            stream_index: 0,
            scheme_type: scheme,
            scheme_version: 0x0001_0000,
            tenc,
        }],
        ..Mp4MuxerOptions::default()
    }
}

fn open_typed(
    path: &std::path::Path,
    streams: &[StreamInfo],
    options: Mp4MuxerOptions,
) -> oxideav_mp4::frag::FragmentedMuxer {
    let frag = options.fragmented.clone().expect("fragmented options");
    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    oxideav_mp4::frag::open_fragmented_typed(ws, streams, options, frag).unwrap()
}

fn demux_file(path: &std::path::Path) -> oxideav_mp4::demux::Mp4Demuxer {
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(path).unwrap());
    oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap()
}

fn audio_packet(stream: &StreamInfo, i: i64, data: Vec<u8>) -> Packet {
    let mut pkt = Packet::new(stream.index, stream.time_base, data);
    pkt.pts = Some(i * 1024);
    pkt.duration = Some(1024);
    pkt.flags.keyframe = true;
    pkt
}

/// Minimal top-level box walk: return `(offset, size)` of every box
/// with the given fourcc.
fn top_level_boxes(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= bytes.len() {
        let size = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        if size < 8 || pos + size > bytes.len() {
            break;
        }
        if &bytes[pos + 4..pos + 8] == fourcc {
            out.push((pos, size));
        }
        pos += size;
    }
    out
}

// --- senc + saiz + saio emission -----------------------------------------

/// Per-sample-IV scheme (`cenc`, 8-byte IVs): two fragments of two
/// samples each must yield one `senc` per fragment carrying the queued
/// IVs, plus a matching constant-size `saiz` and a `saio` whose single
/// moof-relative offset lands exactly on the first IV byte.
#[test]
fn per_fragment_senc_saiz_saio_round_trip() {
    let kid = [0x5C; 16];
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8(kid), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-senc.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..4i64 {
            let pkt = audio_packet(&stream, i, vec![i as u8; 48]);
            let senc = SencSample {
                initialization_vector: vec![0xA0 + i as u8; 8],
                subsamples: Vec::new(),
            };
            mux.write_protected_packet(&pkt, senc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);

    // One senc per fragment, entries in write order with the exact IVs.
    let senc_records = dmx.senc_records();
    assert_eq!(senc_records.len(), 2, "one senc per fragment");
    for (frag, rec) in senc_records.iter().enumerate() {
        assert_eq!(rec.track_idx, 0);
        assert_eq!(rec.moof_sequence, frag as u32 + 1);
        assert!(!rec.senc.uses_subsample_encryption());
        assert_eq!(rec.senc.samples.len(), 2);
        for (k, s) in rec.senc.samples.iter().enumerate() {
            let expect = 0xA0 + (frag * 2 + k) as u8;
            assert_eq!(s.initialization_vector, vec![expect; 8], "IV of sample {k}");
            assert!(s.subsamples.is_empty());
        }
    }

    // One (saiz, saio) pair per fragment: constant 8-byte aux info.
    let sai_records = dmx.sai_records();
    assert_eq!(sai_records.len(), 2, "one sai record per fragment");
    for rec in sai_records {
        assert_eq!(rec.saiz.len(), 1);
        assert_eq!(rec.saio.len(), 1);
        let saiz = &rec.saiz[0];
        assert_eq!(saiz.aux_info_type, None, "§7.1: SHOULD omit aux_info_type");
        assert_eq!(saiz.default_sample_info_size, 8);
        assert_eq!(saiz.sample_count, 2);
        assert!(saiz.per_sample.is_empty());
        assert_eq!(rec.saio[0].offsets.len(), 1, "§8.8.14: one contiguous run");
    }

    // Byte-level gate: each saio offset, applied from its moof's first
    // byte (default-base-is-moof, §8.8.14), must land on that
    // fragment's first queued IV.
    let bytes = std::fs::read(&tmp).unwrap();
    let moofs = top_level_boxes(&bytes, b"moof");
    assert_eq!(moofs.len(), 2);
    for (frag, ((moof_pos, _), rec)) in moofs.iter().zip(sai_records).enumerate() {
        let off = rec.saio[0].offsets[0] as usize;
        let first_iv = 0xA0 + (frag * 2) as u8;
        assert_eq!(
            &bytes[moof_pos + off..moof_pos + off + 8],
            &[first_iv; 8],
            "saio offset must point at fragment {frag}'s first senc IV"
        );
    }
}

/// Subsample maps set the senc `UseSubSampleEncryption` flag (§7.2.3)
/// and grow the per-sample `saiz` sizes by `2 + 6·n` (§7.1); mixed
/// subsample counts force the variable-size `saiz` table.
#[test]
fn subsample_maps_survive_the_senc_round_trip() {
    let kid = [0x21; 16];
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8(kid), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-subsample.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        // Sample 0: two subsamples (10 + 20) + (6 + 12) = 48 bytes.
        let pkt = audio_packet(&stream, 0, vec![0x11; 48]);
        let senc = SencSample {
            initialization_vector: vec![0x01; 8],
            subsamples: vec![
                SubsampleEntry {
                    bytes_of_clear_data: 10,
                    bytes_of_protected_data: 20,
                },
                SubsampleEntry {
                    bytes_of_clear_data: 6,
                    bytes_of_protected_data: 12,
                },
            ],
        };
        mux.write_protected_packet(&pkt, senc).unwrap();
        // Sample 1: one subsample covering all 48 bytes.
        let pkt = audio_packet(&stream, 1, vec![0x22; 48]);
        let senc = SencSample {
            initialization_vector: vec![0x02; 8],
            subsamples: vec![SubsampleEntry {
                bytes_of_clear_data: 16,
                bytes_of_protected_data: 32,
            }],
        };
        mux.write_protected_packet(&pkt, senc).unwrap();
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    let senc_records = dmx.senc_records();
    assert_eq!(senc_records.len(), 1);
    let senc = &senc_records[0].senc;
    assert!(senc.uses_subsample_encryption(), "§7.2.3 flag 0x2");
    assert_eq!(senc.samples.len(), 2);
    assert_eq!(senc.samples[0].subsamples.len(), 2);
    assert_eq!(senc.samples[0].subsamples[0].bytes_of_clear_data, 10);
    assert_eq!(senc.samples[0].subsamples[0].bytes_of_protected_data, 20);
    assert_eq!(senc.samples[0].subsamples[1].bytes_of_clear_data, 6);
    assert_eq!(senc.samples[0].subsamples[1].bytes_of_protected_data, 12);
    assert_eq!(senc.samples[1].subsamples.len(), 1);

    // Variable-size saiz: 8 + 2 + 12 = 22 vs 8 + 2 + 6 = 16.
    let sai = dmx.sai_records();
    assert_eq!(sai.len(), 1);
    let saiz = &sai[0].saiz[0];
    assert_eq!(saiz.default_sample_info_size, 0, "sizes differ per sample");
    assert_eq!(saiz.per_sample, vec![22, 16]);
}

/// Constant-IV track (cbcs) without subsample maps: the §7.1 auxiliary
/// information is empty, so no `senc` / `saiz` / `saio` is written —
/// the file stays fully self-describing through `tenc` alone.
#[test]
fn constant_iv_full_sample_omits_empty_aux_info() {
    let kid = [0x77; 16];
    let stream = pcm_stream(0);
    let options = protected_options(*b"cbcs", tenc_cbcs(kid, vec![0x42; 16]), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-cbcs-omit.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..2i64 {
            let pkt = audio_packet(&stream, i, vec![i as u8; 64]);
            let senc = SencSample {
                initialization_vector: Vec::new(),
                subsamples: Vec::new(),
            };
            mux.write_protected_packet(&pkt, senc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    assert!(
        dmx.senc_records().is_empty(),
        "§7.1: empty aux info omitted"
    );
    assert!(dmx.sai_records().is_empty());
    // The protection surface itself still round-trips.
    let params = dmx.streams()[0].params.clone();
    assert_eq!(params.options.get("protection_scheme"), Some("cbcs"));
}

/// Constant-IV track (cbcs) WITH subsample maps: aux info is the
/// subsample table only (zero IV bytes per §9.1 / §7.1), so senc is
/// emitted with `Per_Sample_IV_Size == 0` entries.
#[test]
fn constant_iv_subsample_senc_has_zero_length_ivs() {
    let kid = [0x2F; 16];
    let stream = pcm_stream(0);
    let options = protected_options(*b"cbcs", tenc_cbcs(kid, vec![0x24; 16]), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-cbcs-subs.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..2i64 {
            let pkt = audio_packet(&stream, i, vec![i as u8; 96]);
            let senc = SencSample {
                initialization_vector: Vec::new(),
                subsamples: vec![SubsampleEntry {
                    bytes_of_clear_data: 32,
                    bytes_of_protected_data: 64,
                }],
            };
            mux.write_protected_packet(&pkt, senc).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    let senc_records = dmx.senc_records();
    assert_eq!(senc_records.len(), 1);
    let senc = &senc_records[0].senc;
    assert!(senc.uses_subsample_encryption());
    assert_eq!(senc.samples.len(), 2);
    for s in &senc.samples {
        assert!(s.initialization_vector.is_empty(), "constant IV: no bytes");
        assert_eq!(s.subsamples.len(), 1);
    }
    // saiz size = 0 IV + 2 count + 6 per entry = 8 bytes.
    let sai = dmx.sai_records();
    assert_eq!(sai[0].saiz[0].default_sample_info_size, 8);
}

// --- queue-discipline errors ----------------------------------------------

#[test]
fn protected_write_requires_a_protection_directive() {
    let stream = pcm_stream(0);
    let options = Mp4MuxerOptions {
        fragmented: Some(frag_opts(2)),
        ..Mp4MuxerOptions::default()
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-noprot.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let pkt = audio_packet(&stream, 0, vec![0; 16]);
    let senc = SencSample {
        initialization_vector: vec![0; 8],
        subsamples: Vec::new(),
    };
    let err = mux.write_protected_packet(&pkt, senc).unwrap_err();
    assert!(format!("{err}").contains("track_protection"), "err = {err}");
}

#[test]
fn mixing_plain_and_protected_writes_in_one_fragment_is_rejected() {
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x01; 16]), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-mix.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let senc = SencSample {
        initialization_vector: vec![0x07; 8],
        subsamples: Vec::new(),
    };
    mux.write_protected_packet(&audio_packet(&stream, 0, vec![0; 16]), senc.clone())
        .unwrap();
    // Plain write after a protected one — §7.2.3 all-or-none.
    let err = mux
        .write_packet(&audio_packet(&stream, 1, vec![0; 16]))
        .unwrap_err();
    assert!(format!("{err}").contains("mixes"), "err = {err}");
    // And the mirror image: plain first, protected second.
    let options = protected_options(*b"cenc", tenc_iv8([0x01; 16]), frag_opts(4));
    let tmp2 = std::env::temp_dir().join("oxideav-mp4-cenc-frag-mix2.mp4");
    let mut mux = open_typed(&tmp2, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    mux.write_packet(&audio_packet(&stream, 0, vec![0; 16]))
        .unwrap();
    let err = mux
        .write_protected_packet(&audio_packet(&stream, 1, vec![0; 16]), senc)
        .unwrap_err();
    assert!(format!("{err}").contains("mixes"), "err = {err}");
}

#[test]
fn iv_width_must_match_tenc() {
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x01; 16]), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-ivw.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let senc = SencSample {
        initialization_vector: vec![0x07; 16], // tenc says 8
        subsamples: Vec::new(),
    };
    let err = mux
        .write_protected_packet(&audio_packet(&stream, 0, vec![0; 16]), senc)
        .unwrap_err();
    assert!(
        format!("{err}").contains("Per_Sample_IV_Size"),
        "err = {err}"
    );
    // Constant-IV track handed IV bytes.
    let options = protected_options(*b"cbcs", tenc_cbcs([0x01; 16], vec![0; 16]), frag_opts(4));
    let tmp2 = std::env::temp_dir().join("oxideav-mp4-cenc-frag-ivw2.mp4");
    let mut mux = open_typed(&tmp2, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let senc = SencSample {
        initialization_vector: vec![0x07; 8],
        subsamples: Vec::new(),
    };
    let err = mux
        .write_protected_packet(&audio_packet(&stream, 0, vec![0; 16]), senc)
        .unwrap_err();
    assert!(format!("{err}").contains("constant IV"), "err = {err}");
}

#[test]
fn subsample_totals_must_cover_the_sample() {
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x01; 16]), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-subtot.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let senc = SencSample {
        initialization_vector: vec![0x07; 8],
        subsamples: vec![SubsampleEntry {
            bytes_of_clear_data: 4,
            bytes_of_protected_data: 8, // 12 ≠ 16
        }],
    };
    let err = mux
        .write_protected_packet(&audio_packet(&stream, 0, vec![0; 16]), senc)
        .unwrap_err();
    assert!(format!("{err}").contains("§9.5.1"), "err = {err}");
}

/// A protected track written through the plain `write_packet` path
/// (signalling-only mode — IVs travel out of band) keeps working: no
/// senc / saiz / saio is emitted.
#[test]
fn plain_writes_on_protected_track_emit_no_aux_boxes() {
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x0A; 16]), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-plain.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..4i64 {
            mux.write_packet(&audio_packet(&stream, i, vec![i as u8; 32]))
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let dmx = demux_file(&tmp);
    assert!(dmx.senc_records().is_empty());
    assert!(dmx.sai_records().is_empty());
    assert_eq!(
        dmx.streams()[0].params.options.get("protection_scheme"),
        Some("cenc")
    );
}

// --- seig key rotation (fragment-local sgpd + sbgp, §8.9.4 + 23001-7 §6) --

/// Mid-fragment key rotation: samples 0–1 on the `tenc` default KID,
/// samples 2–3 mapped (via a per-sample `seig` override) to a rotated
/// KID. The flush must emit a fragment-local `sgpd('seig')` with one
/// deduplicated entry plus an `sbgp('seig')` whose run-length map uses
/// the §8.9.4 fragment-local index 0x10001 — and the demuxer's
/// `traf_sample_groups()` must hand all of it back.
#[test]
fn seig_key_rotation_emits_fragment_local_groups() {
    use oxideav_mp4::cenc::{parse_seig, SeigEntry};
    let default_kid = [0x5A; 16];
    let rotated_kid = [0xB7; 16];
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8(default_kid), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-seig.mp4");
    let rotated = SeigEntry {
        crypt_byte_block: 0,
        skip_byte_block: 0,
        is_protected: 1,
        per_sample_iv_size: 8,
        kid: rotated_kid,
        constant_iv: None,
    };
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..4i64 {
            let pkt = audio_packet(&stream, i, vec![i as u8; 48]);
            let senc = SencSample {
                initialization_vector: vec![0xC0 + i as u8; 8],
                subsamples: Vec::new(),
            };
            let seig = if i >= 2 { Some(rotated.clone()) } else { None };
            mux.write_protected_packet_grouped(&pkt, senc, seig)
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    let groups = dmx.traf_sample_groups();
    assert_eq!(groups.len(), 1, "one traf carried sample groups");
    let rec = &groups[0];
    assert_eq!(rec.track_idx, 0);
    assert_eq!(rec.moof_sequence, 1);

    // Fragment-local sgpd: one deduplicated seig entry.
    assert_eq!(rec.sgpd.len(), 1);
    let sgpd = &rec.sgpd[0];
    assert_eq!(&sgpd.grouping_type, b"seig");
    assert_eq!(sgpd.entries.len(), 1, "two rotated samples share one entry");
    let parsed = parse_seig(&sgpd.entries[0]).expect("seig entry parses");
    assert_eq!(parsed, rotated);

    // sbgp: run-length map (2 default, 2 fragment-local index 0x10001),
    // totalling the fragment's sample count per §8.9.4.
    assert_eq!(rec.sbgp.len(), 1);
    let sbgp = &rec.sbgp[0];
    assert_eq!(&sbgp.grouping_type, b"seig");
    assert_eq!(sbgp.entries, vec![(2, 0), (2, 0x10001)]);

    // The senc still carries all four IVs (rotation doesn't change the
    // aux-info channel).
    assert_eq!(dmx.senc_records().len(), 1);
    assert_eq!(dmx.senc_records()[0].senc.samples.len(), 4);
}

/// Two distinct rotated keys inside one fragment dedupe into two sgpd
/// entries with consecutive fragment-local indices, in first-use order.
#[test]
fn seig_two_keys_dedupe_in_first_use_order() {
    use oxideav_mp4::cenc::SeigEntry;
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x00; 16]), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-seig2.mp4");
    let key_a = SeigEntry {
        crypt_byte_block: 0,
        skip_byte_block: 0,
        is_protected: 1,
        per_sample_iv_size: 8,
        kid: [0xA1; 16],
        constant_iv: None,
    };
    let key_b = SeigEntry {
        kid: [0xB2; 16],
        ..key_a.clone()
    };
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        // Pattern A, B, A, B — dedupes to two entries, four sbgp runs.
        for i in 0..4i64 {
            let pkt = audio_packet(&stream, i, vec![i as u8; 32]);
            let senc = SencSample {
                initialization_vector: vec![i as u8 + 1; 8],
                subsamples: Vec::new(),
            };
            let seig = if i % 2 == 0 {
                key_a.clone()
            } else {
                key_b.clone()
            };
            mux.write_protected_packet_grouped(&pkt, senc, Some(seig))
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let dmx = demux_file(&tmp);
    let rec = &dmx.traf_sample_groups()[0];
    assert_eq!(rec.sgpd[0].entries.len(), 2, "two distinct keys");
    assert_eq!(
        rec.sbgp[0].entries,
        vec![(1, 0x10001), (1, 0x10002), (1, 0x10001), (1, 0x10002)]
    );
}

/// A protected seig override that changes the per-sample IV width is
/// rejected — the fragment's senc stores one IV width for all samples
/// (§7.2.3).
#[test]
fn seig_iv_width_change_is_rejected() {
    use oxideav_mp4::cenc::SeigEntry;
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x00; 16]), frag_opts(4));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-seig-ivw.mp4");
    let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
    mux.write_header().unwrap();
    let senc = SencSample {
        initialization_vector: vec![0x01; 8],
        subsamples: Vec::new(),
    };
    let wide = SeigEntry {
        crypt_byte_block: 0,
        skip_byte_block: 0,
        is_protected: 1,
        per_sample_iv_size: 16, // tenc says 8
        kid: [0xEE; 16],
        constant_iv: None,
    };
    let err = mux
        .write_protected_packet_grouped(&audio_packet(&stream, 0, vec![0; 16]), senc, Some(wide))
        .unwrap_err();
    assert!(
        format!("{err}").contains("Per_Sample_IV_Size"),
        "err = {err}"
    );
}

/// An unprotected seig override (isProtected = 0 — a clear-samples
/// group, §6) rides along without disturbing the senc channel.
#[test]
fn seig_clear_group_round_trips() {
    use oxideav_mp4::cenc::{parse_seig, SeigEntry};
    let stream = pcm_stream(0);
    let options = protected_options(*b"cenc", tenc_iv8([0x0C; 16]), frag_opts(2));
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-seig-clear.mp4");
    let clear = SeigEntry {
        crypt_byte_block: 0,
        skip_byte_block: 0,
        is_protected: 0,
        per_sample_iv_size: 0,
        kid: [0u8; 16],
        constant_iv: None,
    };
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        // Sample 0 protected (defaults); sample 1 clear via seig.
        let senc0 = SencSample {
            initialization_vector: vec![0x11; 8],
            subsamples: Vec::new(),
        };
        mux.write_protected_packet(&audio_packet(&stream, 0, vec![0xAA; 32]), senc0)
            .unwrap();
        let senc1 = SencSample {
            initialization_vector: vec![0x22; 8],
            subsamples: Vec::new(),
        };
        mux.write_protected_packet_grouped(
            &audio_packet(&stream, 1, vec![0xBB; 32]),
            senc1,
            Some(clear.clone()),
        )
        .unwrap();
        mux.write_trailer().unwrap();
    }
    let dmx = demux_file(&tmp);
    let rec = &dmx.traf_sample_groups()[0];
    assert_eq!(rec.sbgp[0].entries, vec![(1, 0), (1, 0x10001)]);
    let parsed = parse_seig(&rec.sgpd[0].entries[0]).unwrap();
    assert_eq!(parsed, clear);
}

// --- indexed / multi-track / cadence interplay -----------------------------

/// Protected fragments with the full DASH index chrome enabled (sidx +
/// styp + mfra): the senc/saiz/saio triple still lands per traf and
/// the saio byte gate still holds — the offset is moof-relative
/// (§8.8.14), so the preceding sidx/styp don't disturb it.
#[test]
fn protected_fragments_with_sidx_styp_and_mfra() {
    let stream = pcm_stream(0);
    let frag = FragmentedOptions {
        cadence: FragmentCadence::EveryNPackets(2),
        emit_random_access_indexes: true,
        ..FragmentedOptions::default() // default styp(msdh) preset
    };
    let options = protected_options(*b"cenc", tenc_iv8([0x31; 16]), frag);
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-indexed.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..4i64 {
            let senc = SencSample {
                initialization_vector: vec![0xD0 + i as u8; 8],
                subsamples: Vec::new(),
            };
            mux.write_protected_packet(&audio_packet(&stream, i, vec![i as u8; 40]), senc)
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    assert_eq!(
        top_level_boxes(&bytes, b"sidx").len(),
        2,
        "one sidx per fragment"
    );
    assert_eq!(
        top_level_boxes(&bytes, b"styp").len(),
        2,
        "one styp per fragment"
    );
    assert_eq!(
        top_level_boxes(&bytes, b"mfra").len(),
        1,
        "random-access trailer"
    );

    let dmx = demux_file(&tmp);
    assert_eq!(dmx.senc_records().len(), 2);
    let sai = dmx.sai_records();
    assert_eq!(sai.len(), 2);
    let moofs = top_level_boxes(&bytes, b"moof");
    for (frag_idx, ((moof_pos, _), rec)) in moofs.iter().zip(sai).enumerate() {
        let off = rec.saio[0].offsets[0] as usize;
        let first_iv = 0xD0 + (frag_idx * 2) as u8;
        assert_eq!(
            &bytes[moof_pos + off..moof_pos + off + 8],
            &[first_iv; 8],
            "fragment {frag_idx}: saio offset unaffected by sidx/styp"
        );
    }
}

/// Two protected tracks in one moof: each traf carries its own senc +
/// saiz + saio, and each saio offset resolves to that track's first IV
/// (both patched against the shared moof origin).
#[test]
fn two_protected_tracks_patch_independent_saio_offsets() {
    let streams = [pcm_stream(0), pcm_stream(1)];
    let frag = frag_opts(2);
    let options = Mp4MuxerOptions {
        fragmented: Some(frag),
        track_protection: vec![
            TrackProtection {
                stream_index: 0,
                scheme_type: *b"cenc",
                scheme_version: 0x0001_0000,
                tenc: tenc_iv8([0x0A; 16]),
            },
            TrackProtection {
                stream_index: 1,
                scheme_type: *b"cenc",
                scheme_version: 0x0001_0000,
                tenc: tenc_iv8([0x0B; 16]),
            },
        ],
        ..Mp4MuxerOptions::default()
    };
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-2track.mp4");
    {
        let mut mux = open_typed(&tmp, &streams, options);
        mux.write_header().unwrap();
        // Interleave: t0, t1, t0 (flush fires on t0's 2nd packet), t1.
        for i in 0..2i64 {
            for s in &streams {
                let tag = if s.index == 0 { 0xA0 } else { 0xB0 };
                let senc = SencSample {
                    initialization_vector: vec![tag + i as u8; 8],
                    subsamples: Vec::new(),
                };
                mux.write_protected_packet(&audio_packet(s, i, vec![tag; 32]), senc)
                    .unwrap();
            }
        }
        mux.write_trailer().unwrap();
    }

    let bytes = std::fs::read(&tmp).unwrap();
    let moofs = top_level_boxes(&bytes, b"moof");
    let dmx = demux_file(&tmp);
    // First moof: both tracks contributed (track 0 twice, track 1 once).
    let first_moof_sais: Vec<_> = dmx
        .sai_records()
        .iter()
        .filter(|r| r.moof_sequence == 1)
        .collect();
    assert_eq!(first_moof_sais.len(), 2, "one sai record per traf");
    let (moof_pos, _) = moofs[0];
    for rec in first_moof_sais {
        let off = rec.saio[0].offsets[0] as usize;
        let expect = if rec.track_idx == 0 { 0xA0 } else { 0xB0 };
        assert_eq!(
            &bytes[moof_pos + off..moof_pos + off + 8],
            &[expect; 8],
            "track {}: saio points at its own traf's first IV",
            rec.track_idx
        );
    }
}

/// EveryKeyframe cadence detaches the just-pushed keyframe into the
/// next fragment — its senc entry must travel with it, keeping IVs
/// aligned to samples across the boundary.
#[test]
fn every_keyframe_cadence_keeps_senc_aligned() {
    let stream = pcm_stream(0);
    let frag = FragmentedOptions {
        cadence: FragmentCadence::EveryKeyframe,
        emit_random_access_indexes: false,
        styp: None,
        ..FragmentedOptions::default()
    };
    let options = protected_options(*b"cenc", tenc_iv8([0x44; 16]), frag);
    let tmp = std::env::temp_dir().join("oxideav-mp4-cenc-frag-ekf.mp4");
    {
        let mut mux = open_typed(&tmp, std::slice::from_ref(&stream), options);
        mux.write_header().unwrap();
        for i in 0..4i64 {
            let senc = SencSample {
                initialization_vector: vec![0xE0 + i as u8; 8],
                subsamples: Vec::new(),
            };
            // All keyframes → one sample per fragment after the first
            // detach cycle.
            mux.write_protected_packet(&audio_packet(&stream, i, vec![i as u8; 24]), senc)
                .unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let dmx = demux_file(&tmp);
    // Collect (moof_sequence, IVs) and flatten; the IV stream must be
    // 0xE0, 0xE1, 0xE2, 0xE3 in sample order regardless of how the
    // detach re-binned the fragments.
    let ivs: Vec<u8> = dmx
        .senc_records()
        .iter()
        .flat_map(|r| r.senc.samples.iter().map(|s| s.initialization_vector[0]))
        .collect();
    assert_eq!(ivs, vec![0xE0, 0xE1, 0xE2, 0xE3], "IVs track their samples");
    let total: usize = dmx
        .senc_records()
        .iter()
        .map(|r| r.senc.samples.len())
        .sum();
    assert_eq!(total, 4, "every sample carries aux info");
}
