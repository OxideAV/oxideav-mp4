//! RTP / SRTP hint-track sample-entry family — ISO/IEC 14496-12 §9.1.
//!
//! RTP server hint tracks (media handler `hint`) carry an entry-format of
//! `rtp ` in their sample description (§9.1.2); SRTP hint tracks use
//! `srtp` and RTP reception hint tracks use `rrtp` (§9.4.1.2). All three
//! share one body:
//!
//! ```text
//! class RtpHintSampleEntry() extends SampleEntry ('rtp ') {
//!     uint(16) hinttrackversion = 1;
//!     uint(16) highestcompatibleversion = 1;
//!     uint(32) maxpacketsize;
//!     box      additionaldata[];
//! }
//! ```
//!
//! The `additionaldata` set of boxes is drawn from:
//!
//! ```text
//! class timescaleentry() extends Box('tims') { uint(32) timescale; }
//! class timeoffset()     extends Box('tsro') { int(32)  offset; }
//! class sequenceoffset   extends Box('snro') { int(32)  offset; }
//! ```
//!
//! `tims` is required; `tsro` / `snro` are optional (§9.1.2). An SRTP
//! entry additionally carries an `srpp` SRTPProcessBox (§9.1.2.1):
//!
//! ```text
//! class SRTPProcessBox extends FullBox('srpp', version, 0) {
//!     unsigned int(32) encryption_algorithm_rtp;
//!     unsigned int(32) encryption_algorithm_rtcp;
//!     unsigned int(32) integrity_algorithm_rtp;
//!     unsigned int(32) integrity_algorithm_rtcp;
//!     SchemeTypeBox        scheme_type_box;
//!     SchemeInformationBox info;
//! }
//! ```
//!
//! This module models the fixed `srpp` algorithm quad and preserves its
//! trailing `schm`/`schi` bytes verbatim (those boxes are parsed
//! elsewhere in the crate; here they are an opaque tail so the entry
//! round-trips byte-exact). Every parser is the byte-exact inverse of its
//! builder. All integers big-endian (§7).

use crate::boxes::*;

/// The first four fixed `srpp` algorithm identifiers (ISO/IEC 14496-12
/// §9.1.2.1) plus the verbatim trailing `SchemeTypeBox` +
/// `SchemeInformationBox` bytes (kept opaque so the box round-trips).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrppBox {
    /// FullBox version (carried verbatim; the spec leaves it open).
    pub version: u8,
    /// RTP encryption algorithm 4CC (`0x20202020` ⇒ "decided elsewhere").
    pub encryption_algorithm_rtp: u32,
    /// RTCP encryption algorithm 4CC.
    pub encryption_algorithm_rtcp: u32,
    /// RTP integrity algorithm 4CC.
    pub integrity_algorithm_rtp: u32,
    /// RTCP integrity algorithm 4CC.
    pub integrity_algorithm_rtcp: u32,
    /// The trailing `schm` (SchemeTypeBox) + `schi`
    /// (SchemeInformationBox) bytes, verbatim. Empty when the producer
    /// omitted them (non-conformant but tolerated).
    pub scheme_bytes: Vec<u8>,
}

/// Decoded RTP / SRTP / reception hint sample entry (ISO/IEC 14496-12
/// §9.1.2 / §9.4.1.2). The `format` FourCC distinguishes `rtp `, `srtp`,
/// and `rrtp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpHintSampleEntry {
    /// The sample-entry FourCC: `rtp ` (server), `srtp` (secure), or
    /// `rrtp` (reception).
    pub format: [u8; 4],
    /// `data_reference_index` from the §8.5.2.2 SampleEntry preamble.
    pub data_reference_index: u16,
    /// `hinttrackversion` (currently 1).
    pub hint_track_version: u16,
    /// `highestcompatibleversion` — oldest backward-compatible version.
    pub highest_compatible_version: u16,
    /// `maxpacketsize` — size of the largest packet this track generates.
    pub max_packet_size: u32,
    /// `tims` timescale (required, §9.1.2). `None` only for a
    /// non-conformant entry that omitted it.
    pub timescale: Option<u32>,
    /// `tsro` time offset (optional; inferred 0 when absent).
    pub time_offset: Option<i32>,
    /// `snro` sequence offset (optional; inferred 0 when absent).
    pub sequence_offset: Option<i32>,
    /// `srpp` SRTPProcessBox — present only for `srtp` entries.
    pub srpp: Option<SrppBox>,
}

fn rd_u16(buf: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos + 2 > buf.len() {
        return None;
    }
    let v = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]);
    *pos += 2;
    Some(v)
}

fn rd_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Some(v)
}

fn wrap(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&total.to_be_bytes());
    v.extend_from_slice(fourcc);
    v.extend_from_slice(body);
    v
}

/// Walk the `additionaldata` child boxes, yielding `(fourcc, payload)`.
/// Stops at the first malformed header.
fn each_child<F: FnMut([u8; 4], &[u8])>(body: &[u8], mut f: F) {
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let size = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        let mut fourcc = [0u8; 4];
        fourcc.copy_from_slice(&body[pos + 4..pos + 8]);
        let total = if size == 0 {
            body.len() - pos
        } else {
            size as usize
        };
        if total < 8 || pos + total > body.len() {
            break;
        }
        f(fourcc, &body[pos + 8..pos + total]);
        pos += total;
    }
}

/// Parse an `srpp` body (after the `[size][srpp]` header). `None` on
/// truncation of the fixed 4-byte preamble + four algorithm words.
pub fn parse_srpp_box(body: &[u8]) -> Option<SrppBox> {
    let mut p = 0usize;
    if body.is_empty() {
        return None;
    }
    let version = body[0];
    p += 4; // version + 24-bit flags
    let encryption_algorithm_rtp = rd_u32(body, &mut p)?;
    let encryption_algorithm_rtcp = rd_u32(body, &mut p)?;
    let integrity_algorithm_rtp = rd_u32(body, &mut p)?;
    let integrity_algorithm_rtcp = rd_u32(body, &mut p)?;
    Some(SrppBox {
        version,
        encryption_algorithm_rtp,
        encryption_algorithm_rtcp,
        integrity_algorithm_rtp,
        integrity_algorithm_rtcp,
        scheme_bytes: body[p..].to_vec(),
    })
}

/// Serialise an `srpp` box. The byte-exact inverse of [`parse_srpp_box`].
pub fn build_srpp_box(b: &SrppBox) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b.version);
    body.extend_from_slice(&[0, 0, 0]); // flags
    body.extend_from_slice(&b.encryption_algorithm_rtp.to_be_bytes());
    body.extend_from_slice(&b.encryption_algorithm_rtcp.to_be_bytes());
    body.extend_from_slice(&b.integrity_algorithm_rtp.to_be_bytes());
    body.extend_from_slice(&b.integrity_algorithm_rtcp.to_be_bytes());
    body.extend_from_slice(&b.scheme_bytes);
    wrap(&SRPP, &body)
}

/// Parse a hint sample entry of type `format` (`rtp `, `srtp`, `rrtp`)
/// from its full box payload (the bytes after the `[size][format]`
/// header). `None` on truncation of the fixed preamble. Unknown
/// additional-data boxes are ignored.
pub fn parse_rtp_hint_sample_entry(format: [u8; 4], entry: &[u8]) -> Option<RtpHintSampleEntry> {
    let mut p = 0usize;
    // §8.5.2.2 SampleEntry preamble: 6 reserved + data_reference_index.
    if entry.len() < 8 {
        return None;
    }
    p += 6;
    let data_reference_index = rd_u16(entry, &mut p)?;
    let hint_track_version = rd_u16(entry, &mut p)?;
    let highest_compatible_version = rd_u16(entry, &mut p)?;
    let max_packet_size = rd_u32(entry, &mut p)?;

    let mut out = RtpHintSampleEntry {
        format,
        data_reference_index,
        hint_track_version,
        highest_compatible_version,
        max_packet_size,
        timescale: None,
        time_offset: None,
        sequence_offset: None,
        srpp: None,
    };
    each_child(&entry[p..], |fourcc, child| match fourcc {
        TIMS if child.len() >= 4 => {
            out.timescale = Some(u32::from_be_bytes([child[0], child[1], child[2], child[3]]));
        }
        TSRO if child.len() >= 4 => {
            out.time_offset = Some(i32::from_be_bytes([child[0], child[1], child[2], child[3]]));
        }
        SNRO if child.len() >= 4 => {
            out.sequence_offset =
                Some(i32::from_be_bytes([child[0], child[1], child[2], child[3]]));
        }
        SRPP => out.srpp = parse_srpp_box(child),
        _ => {}
    });
    Some(out)
}

/// Serialise a hint sample entry (complete `[size][format]...`). The
/// byte-exact inverse of [`parse_rtp_hint_sample_entry`]: the
/// additional-data boxes are written in the order `tims`, `tsro`, `snro`,
/// `srpp` (only those present). `tims` is required by §9.1.2 but the
/// builder honours `None` (emitting nothing) so a faithfully-parsed
/// non-conformant entry round-trips.
pub fn build_rtp_hint_sample_entry(e: &RtpHintSampleEntry) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&e.data_reference_index.to_be_bytes());
    body.extend_from_slice(&e.hint_track_version.to_be_bytes());
    body.extend_from_slice(&e.highest_compatible_version.to_be_bytes());
    body.extend_from_slice(&e.max_packet_size.to_be_bytes());
    if let Some(ts) = e.timescale {
        body.extend_from_slice(&wrap(&TIMS, &ts.to_be_bytes()));
    }
    if let Some(off) = e.time_offset {
        body.extend_from_slice(&wrap(&TSRO, &off.to_be_bytes()));
    }
    if let Some(off) = e.sequence_offset {
        body.extend_from_slice(&wrap(&SNRO, &off.to_be_bytes()));
    }
    if let Some(srpp) = &e.srpp {
        body.extend_from_slice(&build_srpp_box(srpp));
    }
    wrap(&e.format, &body)
}

/// Decoded MPEG-2 TS hint sample entry (ISO/IEC 14496-12 §9.3.3.2): the
/// `sm2t` (server) / `rm2t` (reception) entry format for an MPEG-2
/// Transport Stream hint track. The body adds preceding/trailing per-TS-
/// packet byte counts and a precomputed-only flag to the common hint
/// sample-entry preamble.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mpeg2TsHintSampleEntry {
    /// The sample-entry FourCC: `sm2t` (server) or `rm2t` (reception).
    pub format: [u8; 4],
    /// `data_reference_index` from the §8.5.2.2 SampleEntry preamble.
    pub data_reference_index: u16,
    /// `hinttrackversion` (currently 1).
    pub hint_track_version: u16,
    /// `highestcompatibleversion`.
    pub highest_compatible_version: u16,
    /// `precedingbyteslen` — bytes preceding each MPEG-2 TS packet (e.g.
    /// a recording-device timecode).
    pub preceding_bytes_len: u8,
    /// `trailingbyteslen` — bytes following each MPEG-2 TS packet (e.g. a
    /// checksum).
    pub trailing_bytes_len: u8,
    /// `precomputed_only_flag` — the top bit of the next byte; when set,
    /// the associated samples are purely precomputed (§9.3.3.3).
    pub precomputed_only: bool,
    /// The trailing `additionaldata` box bytes, verbatim (PSI/SI static
    /// metadata boxes — kept opaque so the entry round-trips byte-exact).
    pub additional_data: Vec<u8>,
}

/// Parse an MPEG-2 TS hint sample entry (`sm2t` / `rm2t`) from its full
/// box payload (the bytes after the `[size][format]` header). `None` on
/// truncation of the fixed preamble.
pub fn parse_mpeg2ts_hint_sample_entry(
    format: [u8; 4],
    entry: &[u8],
) -> Option<Mpeg2TsHintSampleEntry> {
    let mut p = 0usize;
    // §8.5.2.2 SampleEntry preamble: 6 reserved + data_reference_index.
    if entry.len() < 8 {
        return None;
    }
    p += 6;
    let data_reference_index = rd_u16(entry, &mut p)?;
    let hint_track_version = rd_u16(entry, &mut p)?;
    let highest_compatible_version = rd_u16(entry, &mut p)?;
    // precedingbyteslen(8) + trailingbyteslen(8) + flag/reserved(8).
    if p + 3 > entry.len() {
        return None;
    }
    let preceding_bytes_len = entry[p];
    let trailing_bytes_len = entry[p + 1];
    let precomputed_only = entry[p + 2] & 0x80 != 0;
    p += 3;
    Some(Mpeg2TsHintSampleEntry {
        format,
        data_reference_index,
        hint_track_version,
        highest_compatible_version,
        preceding_bytes_len,
        trailing_bytes_len,
        precomputed_only,
        additional_data: entry[p..].to_vec(),
    })
}

/// Serialise an MPEG-2 TS hint sample entry. The byte-exact inverse of
/// [`parse_mpeg2ts_hint_sample_entry`] (the 7 reserved low bits of the
/// flag byte are written as 0, matching the parser's mask).
pub fn build_mpeg2ts_hint_sample_entry(e: &Mpeg2TsHintSampleEntry) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&e.data_reference_index.to_be_bytes());
    body.extend_from_slice(&e.hint_track_version.to_be_bytes());
    body.extend_from_slice(&e.highest_compatible_version.to_be_bytes());
    body.push(e.preceding_bytes_len);
    body.push(e.trailing_bytes_len);
    body.push(if e.precomputed_only { 0x80 } else { 0x00 });
    body.extend_from_slice(&e.additional_data);
    wrap(&e.format, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_box<'a>(bytes: &'a [u8], fourcc: &[u8; 4]) -> &'a [u8] {
        let total = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(total, bytes.len());
        assert_eq!(&bytes[4..8], fourcc);
        &bytes[8..]
    }

    #[test]
    fn rtp_entry_with_tims_only_round_trips() {
        let e = RtpHintSampleEntry {
            format: *b"rtp ",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1450,
            timescale: Some(90_000),
            time_offset: None,
            sequence_offset: None,
            srpp: None,
        };
        let bytes = build_rtp_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"rtp ");
        assert_eq!(parse_rtp_hint_sample_entry(*b"rtp ", body).unwrap(), e);
    }

    #[test]
    fn rtp_entry_with_all_offsets_round_trips() {
        let e = RtpHintSampleEntry {
            format: *b"rrtp",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1200,
            timescale: Some(48_000),
            time_offset: Some(-12345),
            sequence_offset: Some(7777),
            srpp: None,
        };
        let bytes = build_rtp_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"rrtp");
        let parsed = parse_rtp_hint_sample_entry(*b"rrtp", body).unwrap();
        assert_eq!(parsed, e);
        assert_eq!(parsed.time_offset, Some(-12345));
        assert_eq!(parsed.sequence_offset, Some(7777));
    }

    #[test]
    fn srtp_entry_round_trips_with_srpp() {
        let srpp = SrppBox {
            version: 0,
            encryption_algorithm_rtp: u32::from_be_bytes(*b"ACM1"),
            encryption_algorithm_rtcp: u32::from_be_bytes(*b"    "),
            integrity_algorithm_rtp: u32::from_be_bytes(*b"SHM1"),
            integrity_algorithm_rtcp: u32::from_be_bytes(*b"    "),
            // A trailing schm box (verbatim opaque bytes).
            scheme_bytes: {
                let mut schm = vec![0u8, 0, 0, 0]; // FullBox version/flags
                schm.extend_from_slice(b"srtp"); // scheme_type
                schm.extend_from_slice(&1u32.to_be_bytes()); // scheme_version
                let total = (8 + schm.len()) as u32;
                let mut v = total.to_be_bytes().to_vec();
                v.extend_from_slice(b"schm");
                v.extend_from_slice(&schm);
                v
            },
        };
        let e = RtpHintSampleEntry {
            format: *b"srtp",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1400,
            timescale: Some(90_000),
            time_offset: Some(0),
            sequence_offset: None,
            srpp: Some(srpp.clone()),
        };
        let bytes = build_rtp_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"srtp");
        let parsed = parse_rtp_hint_sample_entry(*b"srtp", body).unwrap();
        assert_eq!(parsed, e);
        assert_eq!(parsed.srpp.as_ref().unwrap(), &srpp);
    }

    #[test]
    fn srpp_round_trips_empty_scheme_tail() {
        let b = SrppBox {
            version: 0,
            encryption_algorithm_rtp: 0x2020_2020,
            encryption_algorithm_rtcp: 0x2020_2020,
            integrity_algorithm_rtp: 0x2020_2020,
            integrity_algorithm_rtcp: 0x2020_2020,
            scheme_bytes: vec![],
        };
        let bytes = build_srpp_box(&b);
        let body = unwrap_box(&bytes, b"srpp");
        // FullBox(4) + four algorithm words(16) = 20 bytes.
        assert_eq!(body.len(), 20);
        assert_eq!(parse_srpp_box(body).unwrap(), b);
    }

    #[test]
    fn entry_truncated_preamble_rejected() {
        // Only the 8-byte SampleEntry preamble, no version fields.
        let short = [0u8; 8];
        assert!(parse_rtp_hint_sample_entry(*b"rtp ", &short).is_none());
    }

    #[test]
    fn rtcp_entry_uses_rtp_body() {
        // §9.4.2.3: rtcp is structurally identical to rtp ; no defined
        // additional-data boxes, but tims may still be carried.
        let e = RtpHintSampleEntry {
            format: *b"rtcp",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1500,
            timescale: Some(90_000),
            time_offset: None,
            sequence_offset: None,
            srpp: None,
        };
        let bytes = build_rtp_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"rtcp");
        assert_eq!(parse_rtp_hint_sample_entry(*b"rtcp", body).unwrap(), e);
    }

    #[test]
    fn mpeg2ts_server_entry_round_trips() {
        let e = Mpeg2TsHintSampleEntry {
            format: *b"sm2t",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            preceding_bytes_len: 4,
            trailing_bytes_len: 16,
            precomputed_only: true,
            additional_data: vec![],
        };
        let bytes = build_mpeg2ts_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"sm2t");
        assert_eq!(parse_mpeg2ts_hint_sample_entry(*b"sm2t", body).unwrap(), e);
    }

    #[test]
    fn mpeg2ts_reception_entry_round_trips_with_additional_data() {
        // A trailing opaque additionaldata box (e.g. a PSI/SI metadata box).
        let extra = wrap(b"abcd", &[1, 2, 3]);
        let e = Mpeg2TsHintSampleEntry {
            format: *b"rm2t",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            preceding_bytes_len: 0,
            trailing_bytes_len: 0,
            precomputed_only: false,
            additional_data: extra.clone(),
        };
        let bytes = build_mpeg2ts_hint_sample_entry(&e);
        let body = unwrap_box(&bytes, b"rm2t");
        let parsed = parse_mpeg2ts_hint_sample_entry(*b"rm2t", body).unwrap();
        assert_eq!(parsed, e);
        assert!(!parsed.precomputed_only);
        assert_eq!(parsed.additional_data, extra);
    }

    #[test]
    fn mpeg2ts_entry_truncated_rejected() {
        // Preamble present but the 3 fixed bytes missing.
        let mut short = vec![0u8; 6];
        short.extend_from_slice(&1u16.to_be_bytes()); // dref
        short.extend_from_slice(&1u16.to_be_bytes()); // version
        short.extend_from_slice(&1u16.to_be_bytes()); // highest
        assert!(parse_mpeg2ts_hint_sample_entry(*b"sm2t", &short).is_none());
    }

    #[test]
    fn unknown_additional_data_box_ignored() {
        let e = RtpHintSampleEntry {
            format: *b"rtp ",
            data_reference_index: 1,
            hint_track_version: 1,
            highest_compatible_version: 1,
            max_packet_size: 1000,
            timescale: Some(8000),
            time_offset: None,
            sequence_offset: None,
            srpp: None,
        };
        let mut bytes = build_rtp_hint_sample_entry(&e);
        // Append an unknown box inside the entry (extend the size field).
        let extra = wrap(b"abcd", &[1, 2, 3, 4]);
        let new_total = (bytes.len() + extra.len()) as u32;
        bytes[0..4].copy_from_slice(&new_total.to_be_bytes());
        bytes.extend_from_slice(&extra);
        let body = unwrap_box(&bytes, b"rtp ");
        let parsed = parse_rtp_hint_sample_entry(*b"rtp ", body).unwrap();
        // The unknown box is skipped; the known fields survive.
        assert_eq!(parsed.timescale, Some(8000));
        assert!(parsed.srpp.is_none());
    }
}
