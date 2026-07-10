//! PIFF legacy `uuid` encryption boxes — the pre-CENC predecessors of
//! `senc` / `tenc` / `pssh`, carried as ISO/IEC 14496-12 §4.2
//! extended-type boxes with vendor UUIDs by Smooth Streaming /
//! PlayReady-era packagers.
//!
//! Strategy: build a synthetic fragmented file byte-by-byte, emitting
//! the PIFF boxes through this crate's own `build_piff_*` serialisers
//! (so write side and read side gate each other), then demux and
//! assert the typed records, the CENC-surface bridging, and the flat
//! metadata mirror.

use oxideav_core::ReadSeek;
use oxideav_mp4::cenc::{
    build_piff_pssh_box, build_piff_senc_box, build_piff_tenc_box, build_senc_box, build_tenc_box,
    PiffSencBox, PiffSencOverride, PiffTencBox, PsshBox, SencBox, SencSample, SubsampleEntry,
    TencBox, PIFF_ALGORITHM_AES_CTR,
};

// --- Box-builder helpers (mirroring tests/fragmented.rs) ------------------

fn boxed(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"isml"); // major_brand (Smooth Streaming lineage)
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(b"isml");
    body.extend_from_slice(b"iso6");
    boxed(b"ftyp", &body)
}

fn mvhd(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 100];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..24].copy_from_slice(&0x00010000u32.to_be_bytes()); // rate
    body[24..26].copy_from_slice(&0x0100u16.to_be_bytes()); // volume
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[36 + i * 4..40 + i * 4].copy_from_slice(&v.to_be_bytes());
    }
    body[96..100].copy_from_slice(&2u32.to_be_bytes()); // next_track_ID
    boxed(b"mvhd", &body)
}

fn tkhd_audio(track_id: u32) -> Vec<u8> {
    let mut body = vec![0u8; 80];
    body[1..4].copy_from_slice(&[0, 0, 0x07]);
    body[12..16].copy_from_slice(&track_id.to_be_bytes());
    body[36..38].copy_from_slice(&0x0100u16.to_be_bytes());
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[40 + i * 4..44 + i * 4].copy_from_slice(&v.to_be_bytes());
    }
    boxed(b"tkhd", &body)
}

fn mdhd_audio(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..22].copy_from_slice(&0x55C4u16.to_be_bytes()); // "und"
    boxed(b"mdhd", &body)
}

fn hdlr_soun() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(b"soun");
    body.extend_from_slice(&[0u8; 12]);
    body.extend_from_slice(b"audio\0");
    boxed(b"hdlr", &body)
}

fn dinf_dref() -> Vec<u8> {
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&[0u8; 4]);
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    let url = {
        let mut b = Vec::new();
        b.extend_from_slice(&[0, 0, 0, 1]); // self-contained
        boxed(b"url ", &b)
    };
    dref_body.extend_from_slice(&url);
    boxed(b"dinf", &boxed(b"dref", &dref_body))
}

/// A protected `enca` AudioSampleEntry: the 28-byte audio preamble
/// (as if the original `sowt` FourCC were present, per §8.12.0) with
/// the given `sinf` appended as a child box.
fn stsd_enca(sinf: &[u8]) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry[16..18].copy_from_slice(&2u16.to_be_bytes()); // channels
    entry[18..20].copy_from_slice(&16u16.to_be_bytes()); // sample_size
    entry[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes());
    entry.extend_from_slice(sinf);
    let entry_box = boxed(b"enca", &entry);
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&entry_box);
    boxed(b"stsd", &body)
}

/// `sinf` for the PIFF era: `frma`(sowt) + `schm` with the given
/// scheme_type + `schi` wrapping the provided children (a PIFF `uuid`
/// tenc, optionally alongside a CENC `tenc`).
fn sinf_with_schi_children(scheme_type: &[u8; 4], schi_children: &[u8]) -> Vec<u8> {
    let frma = boxed(b"frma", b"sowt");
    let mut schm_body = Vec::new();
    schm_body.extend_from_slice(&[0u8; 4]);
    schm_body.extend_from_slice(scheme_type);
    schm_body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    let schm = boxed(b"schm", &schm_body);
    let schi = boxed(b"schi", schi_children);
    let mut body = Vec::new();
    body.extend_from_slice(&frma);
    body.extend_from_slice(&schm);
    body.extend_from_slice(&schi);
    boxed(b"sinf", &body)
}

fn empty_stbl_tables() -> Vec<u8> {
    let mut out = Vec::new();
    for fourcc in [b"stts", b"stsc", b"stco"] {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&boxed(fourcc, &body));
    }
    let mut stsz = vec![0u8; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes());
    stsz.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&boxed(b"stsz", &stsz));
    out
}

fn trak_protected_audio(track_id: u32, timescale: u32, stsd: &[u8]) -> Vec<u8> {
    let mut stbl_body = Vec::new();
    stbl_body.extend_from_slice(stsd);
    stbl_body.extend_from_slice(&empty_stbl_tables());
    let stbl = boxed(b"stbl", &stbl_body);
    let smhd = boxed(b"smhd", &[0u8; 8]);
    let mut minf_body = Vec::new();
    minf_body.extend_from_slice(&smhd);
    minf_body.extend_from_slice(&dinf_dref());
    minf_body.extend_from_slice(&stbl);
    let minf = boxed(b"minf", &minf_body);
    let mut mdia_body = Vec::new();
    mdia_body.extend_from_slice(&mdhd_audio(timescale));
    mdia_body.extend_from_slice(&hdlr_soun());
    mdia_body.extend_from_slice(&minf);
    let mdia = boxed(b"mdia", &mdia_body);
    let mut body = Vec::new();
    body.extend_from_slice(&tkhd_audio(track_id));
    body.extend_from_slice(&mdia);
    boxed(b"trak", &body)
}

fn mvex(track_id: u32) -> Vec<u8> {
    let mut trex_body = vec![0u8; 4];
    trex_body.extend_from_slice(&track_id.to_be_bytes());
    trex_body.extend_from_slice(&1u32.to_be_bytes()); // DSDI
    trex_body.extend_from_slice(&1024u32.to_be_bytes()); // default duration
    trex_body.extend_from_slice(&0u32.to_be_bytes()); // default size
    trex_body.extend_from_slice(&0u32.to_be_bytes()); // default flags
    boxed(b"mvex", &boxed(b"trex", &trex_body))
}

fn mfhd(seq: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&seq.to_be_bytes());
    boxed(b"mfhd", &body)
}

fn tfhd_default_base_is_moof(track_id: u32) -> Vec<u8> {
    let flags: u32 = 0x020000 | 0x000008;
    let mut body = Vec::new();
    body.push(0);
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&1024u32.to_be_bytes()); // default duration
    boxed(b"tfhd", &body)
}

fn tfdt_v1(bmdt: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(1);
    body.extend_from_slice(&[0u8; 3]);
    body.extend_from_slice(&bmdt.to_be_bytes());
    boxed(b"tfdt", &body)
}

fn trun_sized(data_offset: i32, sizes: &[u32]) -> Vec<u8> {
    let flags: u32 = 0x000001 | 0x000200;
    let mut body = Vec::new();
    body.push(0);
    body.extend_from_slice(&flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    body.extend_from_slice(&data_offset.to_be_bytes());
    for &s in sizes {
        body.extend_from_slice(&s.to_be_bytes());
    }
    boxed(b"trun", &body)
}

/// One `moof` + `mdat` pair whose `traf` carries the provided extra
/// boxes (PIFF senc / CENC senc) between `tfdt` and `trun`.
fn moof_mdat(seq: u32, track_id: u32, traf_extra: &[u8], payload: &[Vec<u8>]) -> Vec<u8> {
    let sizes: Vec<u32> = payload.iter().map(|p| p.len() as u32).collect();
    let build = |data_offset: i32| {
        let mut traf_body = Vec::new();
        traf_body.extend_from_slice(&tfhd_default_base_is_moof(track_id));
        traf_body.extend_from_slice(&tfdt_v1(0));
        traf_body.extend_from_slice(traf_extra);
        traf_body.extend_from_slice(&trun_sized(data_offset, &sizes));
        let traf = boxed(b"traf", &traf_body);
        let mut moof_body = Vec::new();
        moof_body.extend_from_slice(&mfhd(seq));
        moof_body.extend_from_slice(&traf);
        boxed(b"moof", &moof_body)
    };
    let moof_size = build(0).len() as i32;
    let moof = build(moof_size + 8);
    let mut mdat_payload = Vec::new();
    for p in payload {
        mdat_payload.extend_from_slice(p);
    }
    let mdat = boxed(b"mdat", &mdat_payload);
    let mut out = moof;
    out.extend_from_slice(&mdat);
    out
}

fn demux(bytes: Vec<u8>) -> oxideav_mp4::demux::Mp4Demuxer {
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap()
}

fn meta_value(dmx: &oxideav_mp4::demux::Mp4Demuxer, key: &str) -> Option<String> {
    use oxideav_core::Demuxer;
    dmx.metadata()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn piff_tenc_ctr(kid: [u8; 16]) -> PiffTencBox {
    PiffTencBox {
        algorithm_id: PIFF_ALGORITHM_AES_CTR,
        iv_size: 8,
        kid,
    }
}

// --- Tests -----------------------------------------------------------------

/// A PIFF-only file: `uuid` tenc in `schi`, `uuid` pssh at moov level,
/// `uuid` senc (no override) in the traf. The demuxer must surface the
/// typed PIFF records, bridge the senc into the standard CENC surface
/// (per-sample IV width recovered from the PIFF tenc default), expose
/// the track defaults as `piff_*` stream options, and unwrap the
/// `enca` sample entry back to the original codec via `frma`.
#[test]
fn piff_only_file_round_trips_through_demux_surface() {
    let kid = [0x42; 16];
    let sysid = [0x9A; 16];
    let piff_tenc = piff_tenc_ctr(kid);
    let piff_pssh = PsshBox {
        version: 0,
        system_id: sysid,
        kids: Vec::new(),
        data: b"playready-object".to_vec(),
    };
    let piff_senc = PiffSencBox {
        flags: 0,
        override_params: None,
        samples: vec![
            SencSample {
                initialization_vector: vec![0xA0; 8],
                subsamples: Vec::new(),
            },
            SencSample {
                initialization_vector: vec![0xA1; 8],
                subsamples: Vec::new(),
            },
        ],
    };

    let sinf = sinf_with_schi_children(b"piff", &build_piff_tenc_box(&piff_tenc).unwrap());
    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd(48_000));
    moov_body.extend_from_slice(&trak_protected_audio(1, 48_000, &stsd_enca(&sinf)));
    moov_body.extend_from_slice(&mvex(1));
    moov_body.extend_from_slice(&build_piff_pssh_box(&piff_pssh).unwrap());
    let moov = boxed(b"moov", &moov_body);

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov);
    file.extend_from_slice(&moof_mdat(
        1,
        1,
        &build_piff_senc_box(&piff_senc).unwrap(),
        &[vec![0x11; 32], vec![0x22; 32]],
    ));

    let dmx = demux(file);

    // Track surface: enca unwrapped to sowt, scheme + PIFF defaults on
    // the stream options.
    use oxideav_core::Demuxer;
    let stream = &dmx.streams()[0];
    assert_eq!(stream.params.options.get("protection_scheme"), Some("piff"));
    assert_eq!(stream.params.options.get("piff_algorithm_id"), Some("1"));
    assert_eq!(stream.params.options.get("piff_iv_size"), Some("8"));
    assert_eq!(
        stream.params.options.get("piff_kid"),
        Some("42".repeat(16).as_str())
    );
    // No CENC tenc in this file — the cenc_* keys stay reserved for
    // the 4CC box.
    assert!(stream.params.options.get("cenc_default_kid").is_none());

    // Typed track default + CENC bridges.
    assert_eq!(dmx.piff_tencs(), &[Some(piff_tenc.clone())]);
    let decision = piff_tenc.scheme_decision().expect("AES-CTR routes to cenc");
    assert_eq!(
        decision.cipher_mode(),
        Some(oxideav_mp4::cenc::CipherMode::Ctr)
    );

    // moov-level uuid pssh.
    assert_eq!(dmx.piff_psshes(), &[piff_pssh]);
    assert!(dmx.psshes().is_empty(), "no 4CC pssh in this file");
    assert_eq!(
        meta_value(&dmx, "piff_pssh_0").as_deref(),
        Some(format!("{} 0 16", "9a".repeat(16)).as_str())
    );

    // uuid senc: typed record + bridge into the standard surface.
    assert_eq!(dmx.piff_senc_records().len(), 1);
    let rec = &dmx.piff_senc_records()[0];
    assert_eq!(rec.track_idx, 0);
    assert_eq!(rec.moof_sequence, 1);
    assert_eq!(rec.senc, piff_senc);
    let bridged = dmx.senc_records();
    assert_eq!(bridged.len(), 1, "PIFF-only traf bridges into senc_records");
    assert_eq!(bridged[0].senc.samples, piff_senc.samples);
    assert_eq!(
        meta_value(&dmx, "piff_senc_0").as_deref(),
        Some("track=0 seq=1 samples=2 flags=0x00000000 override=0")
    );

    // The samples themselves still demux (offsets are moof-relative).
    let mut dmx: Box<dyn Demuxer> = Box::new(dmx);
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, vec![0x11; 32]);
}

/// A PIFF senc whose `flags & 1` override triple replaces the track
/// defaults: the 16-byte IVs parse against the override's IV_size
/// (the track's PIFF tenc says 8), the subsample maps survive, and the
/// override travels on the typed record while the bridged CENC record
/// masks the PIFF-only flag bit.
#[test]
fn piff_senc_override_triple_wins_over_track_default() {
    let piff_tenc = piff_tenc_ctr([0x42; 16]);
    let piff_senc = PiffSencBox {
        flags: 0x0000_0003,
        override_params: Some(PiffSencOverride {
            algorithm_id: PIFF_ALGORITHM_AES_CTR,
            iv_size: 16,
            kid: [0x77; 16],
        }),
        samples: vec![SencSample {
            initialization_vector: vec![0xB0; 16],
            subsamples: vec![SubsampleEntry {
                bytes_of_clear_data: 4,
                bytes_of_protected_data: 28,
            }],
        }],
    };

    let sinf = sinf_with_schi_children(b"piff", &build_piff_tenc_box(&piff_tenc).unwrap());
    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd(48_000));
    moov_body.extend_from_slice(&trak_protected_audio(1, 48_000, &stsd_enca(&sinf)));
    moov_body.extend_from_slice(&mvex(1));
    let moov = boxed(b"moov", &moov_body);

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov);
    file.extend_from_slice(&moof_mdat(
        1,
        1,
        &build_piff_senc_box(&piff_senc).unwrap(),
        &[vec![0x33; 32]],
    ));

    let dmx = demux(file);
    assert_eq!(dmx.piff_senc_records().len(), 1);
    let rec = &dmx.piff_senc_records()[0];
    assert_eq!(rec.senc, piff_senc);
    assert!(rec.senc.has_override());
    assert_eq!(rec.senc.override_params.unwrap().iv_size, 16);

    let bridged = dmx.senc_records();
    assert_eq!(bridged.len(), 1);
    assert_eq!(
        bridged[0].senc.flags, 0x0000_0002,
        "PIFF-only override bit masked out of the bridged senc"
    );
    assert!(bridged[0].senc.uses_subsample_encryption());
    assert_eq!(bridged[0].senc.samples, piff_senc.samples);
    assert_eq!(
        meta_value(&dmx, "piff_senc_0").as_deref(),
        Some("track=0 seq=1 samples=1 flags=0x00000003 override=1")
    );
}

/// A dual-branded file (a PIFF-1.3-era packager emitting both the
/// `uuid` and the 4CC forms side by side): the 4CC `senc` owns the
/// standard surface — exactly one `SencRecord`, no duplicate — while
/// the `uuid` twin is preserved on the PIFF surface, and both `tenc`
/// forms surface their own stream-option key families.
#[test]
fn dual_branded_traf_does_not_double_report() {
    let kid = [0x42; 16];
    let cenc_tenc = TencBox {
        version: 0,
        default_is_protected: 1,
        default_per_sample_iv_size: 8,
        default_kid: kid,
        default_crypt_byte_block: 0,
        default_skip_byte_block: 0,
        default_constant_iv: None,
    };
    let piff_tenc = piff_tenc_ctr(kid);
    let cenc_senc = SencBox {
        flags: 0,
        samples: vec![SencSample {
            initialization_vector: vec![0xC0; 8],
            subsamples: Vec::new(),
        }],
    };
    let piff_senc = PiffSencBox {
        flags: 0,
        override_params: None,
        samples: cenc_senc.samples.clone(),
    };

    // schi carrying BOTH tenc forms; scheme_type is the CENC `cenc`
    // (the 4CC branding wins the schm slot in dual files).
    let mut schi_children = build_tenc_box(&cenc_tenc).unwrap();
    schi_children.extend_from_slice(&build_piff_tenc_box(&piff_tenc).unwrap());
    let sinf = sinf_with_schi_children(b"cenc", &schi_children);

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd(48_000));
    moov_body.extend_from_slice(&trak_protected_audio(1, 48_000, &stsd_enca(&sinf)));
    moov_body.extend_from_slice(&mvex(1));
    let moov = boxed(b"moov", &moov_body);

    let mut traf_extra = build_senc_box(&cenc_senc).unwrap();
    traf_extra.extend_from_slice(&build_piff_senc_box(&piff_senc).unwrap());

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov);
    file.extend_from_slice(&moof_mdat(1, 1, &traf_extra, &[vec![0x44; 32]]));

    let dmx = demux(file);

    // Exactly one record on each surface.
    assert_eq!(dmx.senc_records().len(), 1, "4CC senc owns the surface");
    assert_eq!(dmx.senc_records()[0].senc, cenc_senc);
    assert_eq!(dmx.piff_senc_records().len(), 1);
    assert_eq!(dmx.piff_senc_records()[0].senc, piff_senc);

    // Both tenc families visible on the stream options.
    use oxideav_core::Demuxer;
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("cenc_default_kid"), Some("42".repeat(16).as_str()));
    assert_eq!(opts.get("piff_kid"), Some("42".repeat(16).as_str()));
    assert_eq!(opts.get("protection_scheme"), Some("cenc"));
    assert_eq!(dmx.piff_tencs(), &[Some(piff_tenc)]);
}

/// A PIFF `uuid` pssh inside a `moof` binds to the fragment's
/// `mfhd.sequence_number` — the legacy counterpart of the moof-level
/// 4CC pssh surface.
#[test]
fn piff_pssh_inside_moof_is_keyed_by_sequence() {
    let piff_tenc = piff_tenc_ctr([0x42; 16]);
    let sinf = sinf_with_schi_children(b"piff", &build_piff_tenc_box(&piff_tenc).unwrap());
    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd(48_000));
    moov_body.extend_from_slice(&trak_protected_audio(1, 48_000, &stsd_enca(&sinf)));
    moov_body.extend_from_slice(&mvex(1));
    let moov = boxed(b"moov", &moov_body);

    let frag_pssh = PsshBox {
        version: 0,
        system_id: [0xE1; 16],
        kids: Vec::new(),
        data: b"rotated-licence".to_vec(),
    };
    // Hand-build the moof: mfhd + uuid pssh + traf.
    let mut traf_body = Vec::new();
    traf_body.extend_from_slice(&tfhd_default_base_is_moof(1));
    traf_body.extend_from_slice(&tfdt_v1(0));
    traf_body.extend_from_slice(&trun_sized(0, &[]));
    let traf = boxed(b"traf", &traf_body);
    let mut moof_body = Vec::new();
    moof_body.extend_from_slice(&mfhd(9));
    moof_body.extend_from_slice(&build_piff_pssh_box(&frag_pssh).unwrap());
    moof_body.extend_from_slice(&traf);
    let moof = boxed(b"moof", &moof_body);

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov);
    file.extend_from_slice(&moof);
    file.extend_from_slice(&boxed(b"mdat", &[]));

    let dmx = demux(file);
    assert_eq!(dmx.piff_moof_psshes().len(), 1);
    assert_eq!(dmx.piff_moof_psshes()[0].moof_sequence, 9);
    assert_eq!(dmx.piff_moof_psshes()[0].pssh, frag_pssh);
    assert!(dmx.moof_psshes().is_empty());
    assert_eq!(
        meta_value(&dmx, "piff_moof_pssh_0").as_deref(),
        Some(format!("systemid={} seq=9 kids=0 data=15", "e1".repeat(16)).as_str())
    );
}

/// Hostile inputs: a truncated PIFF senc body is dropped without
/// failing the open; a `uuid` box with an unknown usertype (e.g. the
/// Smooth Streaming tfxd) is skipped everywhere it may appear.
#[test]
fn malformed_and_unknown_uuid_boxes_are_tolerated() {
    let piff_tenc = piff_tenc_ctr([0x42; 16]);
    let sinf = sinf_with_schi_children(b"piff", &build_piff_tenc_box(&piff_tenc).unwrap());
    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd(48_000));
    moov_body.extend_from_slice(&trak_protected_audio(1, 48_000, &stsd_enca(&sinf)));
    moov_body.extend_from_slice(&mvex(1));
    // Unknown usertype at moov level: 16 arbitrary bytes + junk.
    let mut unknown_uuid_body = vec![0x6D; 16];
    unknown_uuid_body.extend_from_slice(b"opaque");
    moov_body.extend_from_slice(&boxed(b"uuid", &unknown_uuid_body));
    let moov = boxed(b"moov", &moov_body);

    // traf extras: a truncated PIFF senc (flags claim an override but
    // the body ends), plus an unknown-usertype uuid box.
    let mut bad_senc_body = oxideav_mp4::cenc::PIFF_SENC_USERTYPE.to_vec();
    bad_senc_body.extend_from_slice(&[0, 0, 0, 1, 0xAA]); // v0, flags=1, truncated triple
    let mut traf_extra = boxed(b"uuid", &bad_senc_body);
    let mut tfxd_like = vec![0x6D; 16];
    tfxd_like.extend_from_slice(&[0u8; 20]);
    traf_extra.extend_from_slice(&boxed(b"uuid", &tfxd_like));

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp());
    file.extend_from_slice(&moov);
    file.extend_from_slice(&moof_mdat(1, 1, &traf_extra, &[vec![0x55; 32]]));

    let dmx = demux(file);
    assert!(dmx.piff_senc_records().is_empty(), "truncated senc dropped");
    assert!(dmx.senc_records().is_empty());
    assert!(dmx.piff_psshes().is_empty(), "unknown usertype skipped");
    // The stream and its samples remain fully usable.
    use oxideav_core::Demuxer;
    assert_eq!(dmx.piff_tencs().len(), 1);
    let mut dmx: Box<dyn Demuxer> = Box::new(dmx);
    let pkt = dmx.next_packet().unwrap();
    assert_eq!(pkt.data, vec![0x55; 32]);
}
