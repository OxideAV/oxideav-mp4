//! Per-packet `sample_description_index` surfacing (ISO/IEC 14496-12
//! §8.5.2 / §8.7.4 / §8.8.7).
//!
//! A track may carry several `stsd` sample descriptions and switch
//! between them mid-stream — per chunk via
//! `stsc.sample_description_index`, or per fragment via
//! `tfhd.sample_description_index` (falling back to the `trex`
//! default). Decode always uses entry `[0]`; the demuxer surfaces the
//! per-packet index through
//! `Mp4Demuxer::sample_description_index_of_last_packet` so a caller
//! can detect the switch and re-dispatch its decoder.

use std::io::Cursor;

use oxideav_core::{Error, ReadSeek};

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
    body.extend_from_slice(b"isom");
    body.extend_from_slice(&512u32.to_be_bytes());
    body.extend_from_slice(b"isom");
    body.extend_from_slice(b"mp41");
    boxed(b"ftyp", &body)
}

fn mvhd(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 100];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..24].copy_from_slice(&0x00010000u32.to_be_bytes());
    body[24..26].copy_from_slice(&0x0100u16.to_be_bytes());
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[36 + i * 4..36 + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    body[96..100].copy_from_slice(&2u32.to_be_bytes());
    boxed(b"mvhd", &body)
}

fn tkhd_audio(track_id: u32) -> Vec<u8> {
    let mut body = vec![0u8; 80];
    body[1..4].copy_from_slice(&[0, 0, 0x07]);
    body[12..16].copy_from_slice(&track_id.to_be_bytes());
    body[36..38].copy_from_slice(&0x0100u16.to_be_bytes());
    let identity: [u32; 9] = [0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000];
    for (i, v) in identity.iter().enumerate() {
        body[40 + i * 4..40 + i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    boxed(b"tkhd", &body)
}

fn mdhd_audio(timescale: u32) -> Vec<u8> {
    let mut body = vec![0u8; 24];
    body[12..16].copy_from_slice(&timescale.to_be_bytes());
    body[20..22].copy_from_slice(&0x55C4u16.to_be_bytes());
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

fn smhd() -> Vec<u8> {
    boxed(b"smhd", &[0u8; 8])
}

fn dinf_dref() -> Vec<u8> {
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&[0u8; 4]);
    dref_body.extend_from_slice(&1u32.to_be_bytes());
    let url = boxed(b"url ", &[0, 0, 0, 1]);
    dref_body.extend_from_slice(&url);
    boxed(b"dinf", &boxed(b"dref", &dref_body))
}

fn sowt_entry(sample_rate: u32) -> Vec<u8> {
    let mut entry = vec![0u8; 28];
    entry[6..8].copy_from_slice(&1u16.to_be_bytes());
    entry[16..18].copy_from_slice(&2u16.to_be_bytes());
    entry[18..20].copy_from_slice(&16u16.to_be_bytes());
    entry[24..28].copy_from_slice(&(sample_rate << 16).to_be_bytes());
    boxed(b"sowt", &entry)
}

/// `stsd` with TWO sowt entries (differing sample rates so the entries
/// are distinguishable).
fn stsd_two_entries() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&2u32.to_be_bytes());
    body.extend_from_slice(&sowt_entry(48_000));
    body.extend_from_slice(&sowt_entry(44_100));
    boxed(b"stsd", &body)
}

fn stts_uniform(count: u32, delta: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    body.extend_from_slice(&delta.to_be_bytes());
    boxed(b"stts", &body)
}

/// `stsc` from raw `(first_chunk, samples_per_chunk, sdi)` triples.
fn stsc(entries: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(fc, spc, sdi) in entries {
        body.extend_from_slice(&fc.to_be_bytes());
        body.extend_from_slice(&spc.to_be_bytes());
        body.extend_from_slice(&sdi.to_be_bytes());
    }
    boxed(b"stsc", &body)
}

fn stsz_uniform(count: u32, size: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&size.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    boxed(b"stsz", &body)
}

fn stco(offsets: &[u32]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for o in offsets {
        body.extend_from_slice(&o.to_be_bytes());
    }
    boxed(b"stco", &body)
}

/// Two chunks of two 4-byte samples each; chunk 1 decodes against stsd
/// entry 1, chunk 2 against entry 2.
fn build_two_sdi_file() -> Vec<u8> {
    const SAMPLE: u32 = 4;
    let build_moov = |off1: u32, off2: u32| -> Vec<u8> {
        let mut stbl = Vec::new();
        stbl.extend_from_slice(&stsd_two_entries());
        stbl.extend_from_slice(&stts_uniform(4, 100));
        stbl.extend_from_slice(&stsc(&[(1, 2, 1), (2, 2, 2)]));
        stbl.extend_from_slice(&stsz_uniform(4, SAMPLE));
        stbl.extend_from_slice(&stco(&[off1, off2]));
        let stbl = boxed(b"stbl", &stbl);
        let mut minf = Vec::new();
        minf.extend_from_slice(&smhd());
        minf.extend_from_slice(&dinf_dref());
        minf.extend_from_slice(&stbl);
        let minf = boxed(b"minf", &minf);
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&mdhd_audio(48_000));
        mdia.extend_from_slice(&hdlr_soun());
        mdia.extend_from_slice(&minf);
        let mdia = boxed(b"mdia", &mdia);
        let mut trak = Vec::new();
        trak.extend_from_slice(&tkhd_audio(1));
        trak.extend_from_slice(&mdia);
        let trak = boxed(b"trak", &trak);
        let mut moov = Vec::new();
        moov.extend_from_slice(&mvhd(1000));
        moov.extend_from_slice(&trak);
        boxed(b"moov", &moov)
    };
    let ftyp = ftyp();
    let moov_len = build_moov(0, 0).len();
    let base = (ftyp.len() + moov_len + 8) as u32;
    let moov = build_moov(base, base + 2 * SAMPLE);
    let mut file = Vec::new();
    file.extend_from_slice(&ftyp);
    file.extend_from_slice(&moov);
    file.extend_from_slice(&boxed(b"mdat", &[0u8; 16]));
    file
}

/// Drain packets, returning the SDI reported after each.
fn drain_sdis(file: Vec<u8>) -> Vec<u32> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(
        dmx.sample_description_index_of_last_packet(),
        None,
        "no packet returned yet"
    );
    let mut out = Vec::new();
    loop {
        use oxideav_core::Demuxer;
        match dmx.next_packet() {
            Ok(_) => out.push(dmx.sample_description_index_of_last_packet().unwrap()),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    out
}

/// A per-chunk `stsc` sample-description switch is reported per
/// packet: chunk 1 → index 1, chunk 2 → index 2.
#[test]
fn stsc_switch_reported_per_packet() {
    assert_eq!(drain_sdis(build_two_sdi_file()), vec![1, 1, 2, 2]);
}

/// The multi-entry `stsd` itself still surfaces on the stream options
/// (`stsd_count` / `stsd_<n>`) alongside the per-packet index.
#[test]
fn stsd_options_present_alongside_sdi() {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(build_two_sdi_file()));
    let dmx = oxideav_mp4::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    use oxideav_core::Demuxer;
    let opts = &dmx.streams()[0].params.options;
    assert_eq!(opts.get("stsd_count"), Some("2"));
    assert!(opts.get("stsd_1").is_some());
    assert!(opts.get("stsd_2").is_some());
}

// --- Fragmented: tfhd / trex sample_description_index --------------------

mod fragmented {
    use super::*;

    fn trex(track_id: u32, dsdi: u32) -> Vec<u8> {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&track_id.to_be_bytes());
        body.extend_from_slice(&dsdi.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes()); // default_sample_duration
        body.extend_from_slice(&4u32.to_be_bytes()); // default_sample_size
        body.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
        boxed(b"trex", &body)
    }

    fn empty_stbl_moov(track_id: u32, dsdi: u32) -> Vec<u8> {
        let mut stbl = Vec::new();
        stbl.extend_from_slice(&stsd_two_entries());
        stbl.extend_from_slice(&stts_uniform(0, 0));
        stbl.extend_from_slice(&stsc(&[]));
        stbl.extend_from_slice(&stsz_uniform(0, 0));
        stbl.extend_from_slice(&stco(&[]));
        let stbl = boxed(b"stbl", &stbl);
        let mut minf = Vec::new();
        minf.extend_from_slice(&smhd());
        minf.extend_from_slice(&dinf_dref());
        minf.extend_from_slice(&stbl);
        let minf = boxed(b"minf", &minf);
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&mdhd_audio(48_000));
        mdia.extend_from_slice(&hdlr_soun());
        mdia.extend_from_slice(&minf);
        let mdia = boxed(b"mdia", &mdia);
        let mut trak = Vec::new();
        trak.extend_from_slice(&tkhd_audio(track_id));
        trak.extend_from_slice(&mdia);
        let trak = boxed(b"trak", &trak);
        let mvex = boxed(b"mvex", &trex(track_id, dsdi));
        let mut moov = Vec::new();
        moov.extend_from_slice(&mvhd(48_000));
        moov.extend_from_slice(&trak);
        moov.extend_from_slice(&mvex);
        boxed(b"moov", &moov)
    }

    /// One-traf moof; `sdi = Some(_)` sets the tfhd 0x000002 flag.
    fn moof_mdat(seq: u32, track_id: u32, bmdt: u64, n: u32, sdi: Option<u32>) -> Vec<u8> {
        let mfhd = {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(&seq.to_be_bytes());
            boxed(b"mfhd", &b)
        };
        let tfhd = {
            // default-base-is-moof (0x020000) + optional SDI (0x000002).
            let flags: u32 = 0x020000 | if sdi.is_some() { 0x000002 } else { 0 };
            let mut b = Vec::new();
            b.push(0);
            b.extend_from_slice(&flags.to_be_bytes()[1..4]);
            b.extend_from_slice(&track_id.to_be_bytes());
            if let Some(v) = sdi {
                b.extend_from_slice(&v.to_be_bytes());
            }
            boxed(b"tfhd", &b)
        };
        let tfdt = {
            let mut b = vec![1u8, 0, 0, 0];
            b.extend_from_slice(&bmdt.to_be_bytes());
            boxed(b"tfdt", &b)
        };
        // trun with data_offset; sizes/durations come from trex.
        // box header(8) + FullBox(4) + sample_count(4) + data_offset(4).
        let trun_len = 8 + 4 + 4 + 4;
        let moof_len_wo_trun: usize = 8 + mfhd.len() + 8 /* traf hdr */ + tfhd.len() + tfdt.len();
        let moof_total = moof_len_wo_trun + trun_len;
        let data_offset = (moof_total + 8) as i32; // first byte of mdat payload
        let trun = {
            let mut b = vec![0u8; 1];
            b.extend_from_slice(&[0, 0, 0x01]); // data-offset-present
            b.extend_from_slice(&n.to_be_bytes());
            b.extend_from_slice(&data_offset.to_be_bytes());
            boxed(b"trun", &b)
        };
        let mut traf = Vec::new();
        traf.extend_from_slice(&tfhd);
        traf.extend_from_slice(&tfdt);
        traf.extend_from_slice(&trun);
        let traf = boxed(b"traf", &traf);
        let mut moof = Vec::new();
        moof.extend_from_slice(&mfhd);
        moof.extend_from_slice(&traf);
        let moof = boxed(b"moof", &moof);
        assert_eq!(moof.len(), moof_total, "moof size arithmetic");
        let mut out = moof;
        out.extend_from_slice(&boxed(b"mdat", &vec![0u8; (n * 4) as usize]));
        out
    }

    /// Fragment 1 inherits the `trex` default (1); fragment 2's `tfhd`
    /// overrides to 2; fragment 3 falls back to the default again.
    #[test]
    fn tfhd_override_and_trex_fallback() {
        let mut file = Vec::new();
        file.extend_from_slice(&ftyp());
        file.extend_from_slice(&empty_stbl_moov(1, 1));
        file.extend_from_slice(&moof_mdat(1, 1, 0, 2, None));
        file.extend_from_slice(&moof_mdat(2, 1, 2, 2, Some(2)));
        file.extend_from_slice(&moof_mdat(3, 1, 4, 2, None));
        assert_eq!(drain_sdis(file), vec![1, 1, 2, 2, 1, 1]);
    }
}
