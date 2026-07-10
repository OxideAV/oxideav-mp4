//! `emsg` — DASH Event Message Box (ISO/IEC 23009-1 §5.10.3.3).
//!
//! A top-level box carried in a **Media Segment**, placed before the
//! first `moof` of the segment it applies to ("in-band event"). Each
//! instance carries one timed event: a `(scheme_id_uri, value)` pair
//! naming the message scheme, a timing triple (`timescale`, a start
//! time, an `event_duration`), a de-duplication `id`, and an opaque
//! scheme-specific `message_data` blob that extends to the end of the
//! box (SCTE-35 splice sections, ad markers, application events, …).
//!
//! Two FullBox versions exist and the **field order differs**:
//!
//! * **version 0** puts the two null-terminated strings *first*, then
//!   `timescale`, a **32-bit relative** `presentation_time_delta`
//!   (offset from the earliest presentation time of the containing
//!   segment), `event_duration`, `id`.
//! * **version 1** puts the integers first — `timescale`, a **64-bit
//!   absolute** `presentation_time` on the media presentation
//!   timeline, `event_duration`, `id` — then the two strings.
//!
//! A parser must branch on `version` before reading any body field,
//! because the very first field differs (`string` vs `u32 timescale`).
//! The v0 delta is segment-relative, so the same event bytes cannot be
//! relocated across segments without recomputation; v1's absolute time
//! is segment-independent.
//!
//! This module owns the byte layer ([`parse_emsg_box`] /
//! [`build_emsg_box`]); capture during demux (with the index of the
//! following `moof`) lives in [`crate::demux`], and write-side
//! emission ahead of a fragment's `moof` in
//! [`crate::frag::FragmentedMuxer::set_next_segment_emsg`].

use oxideav_core::{Error, Result};

/// `event_duration` sentinel: `0xFFFFFFFF` = duration unknown /
/// unbounded (ISO/IEC 23009-1 §5.10.3.3).
pub const EMSG_UNKNOWN_DURATION: u32 = 0xFFFF_FFFF;

/// The event start time of an [`EmsgBox`] — the one field whose width
/// *and* meaning change with the FullBox version, so it is carried as
/// a typed choice rather than two nullable fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmsgTime {
    /// Version 0 — `presentation_time_delta`: a 32-bit offset, in
    /// `timescale` units, from the earliest presentation time of the
    /// containing segment (the following `moof`'s first sample) to the
    /// event start.
    Delta(u32),
    /// Version 1 — `presentation_time`: a 64-bit **absolute** event
    /// start on the media presentation timeline, in `timescale` units.
    Absolute(u64),
}

/// Parsed `emsg` DASHEventMessageBox (ISO/IEC 23009-1 §5.10.3.3).
///
/// The FullBox version is implied by the [`EmsgBox::presentation`]
/// variant ([`EmsgTime::Delta`] ⇒ v0, [`EmsgTime::Absolute`] ⇒ v1) and
/// recoverable via [`EmsgBox::version`]; every other field is common
/// to both versions (only their on-wire *order* differs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmsgBox {
    /// Null-terminated UTF-8 URI identifying the message scheme
    /// (e.g. `urn:scte:scte35:2013:bin`). Stored without the
    /// terminator.
    pub scheme_id_uri: String,
    /// Null-terminated UTF-8 scheme-specific value / subtype. May be
    /// empty (an empty string is a lone `0x00` byte on the wire).
    pub value: String,
    /// Ticks per second for [`EmsgBox::presentation`] and
    /// [`EmsgBox::event_duration`].
    pub timescale: u32,
    /// Event start time — see [`EmsgTime`].
    pub presentation: EmsgTime,
    /// Event duration in `timescale` units;
    /// [`EMSG_UNKNOWN_DURATION`] (`0xFFFFFFFF`) = unknown / unbounded.
    pub event_duration: u32,
    /// Instance identifier, unique within the scope of
    /// `(scheme_id_uri, value)`. Used to de-duplicate repeated in-band
    /// signalling of the same event across segments.
    pub id: u32,
    /// Scheme-specific body extending to the end of the box. Not
    /// length-prefixed on the wire — its length is
    /// `box_size − (bytes consumed so far)`.
    pub message_data: Vec<u8>,
}

impl EmsgBox {
    /// The FullBox version this record serialises as: 0 when the start
    /// time is a segment-relative [`EmsgTime::Delta`], 1 when it is an
    /// absolute [`EmsgTime::Absolute`].
    pub fn version(&self) -> u8 {
        match self.presentation {
            EmsgTime::Delta(_) => 0,
            EmsgTime::Absolute(_) => 1,
        }
    }

    /// Version 1 absolute `presentation_time`; `None` on a v0 record.
    pub fn presentation_time(&self) -> Option<u64> {
        match self.presentation {
            EmsgTime::Absolute(t) => Some(t),
            EmsgTime::Delta(_) => None,
        }
    }

    /// Version 0 segment-relative `presentation_time_delta`; `None` on
    /// a v1 record.
    pub fn presentation_time_delta(&self) -> Option<u32> {
        match self.presentation {
            EmsgTime::Delta(d) => Some(d),
            EmsgTime::Absolute(_) => None,
        }
    }

    /// `true` when `event_duration` carries the `0xFFFFFFFF`
    /// unknown/unbounded sentinel.
    pub fn event_duration_unknown(&self) -> bool {
        self.event_duration == EMSG_UNKNOWN_DURATION
    }
}

/// Read a null-terminated UTF-8 string starting at `cursor`; returns
/// the decoded string and the cursor position just past the
/// terminator. An unterminated string (no `0x00` before the end of the
/// body) or invalid UTF-8 is an error — both fields are declared
/// null-terminated UTF-8 by §5.10.3.3, and running past the terminator
/// would misalign every field after it.
fn read_cstring(body: &[u8], cursor: usize, what: &str) -> Result<(String, usize)> {
    let rest = body
        .get(cursor..)
        .ok_or_else(|| Error::invalid(format!("MP4 emsg: truncated before {what}")))?;
    let nul = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::invalid(format!("MP4 emsg: unterminated {what}")))?;
    let s = std::str::from_utf8(&rest[..nul])
        .map_err(|_| Error::invalid(format!("MP4 emsg: {what} is not valid UTF-8")))?;
    Ok((s.to_string(), cursor + nul + 1))
}

fn read_u32(body: &[u8], cursor: usize, what: &str) -> Result<u32> {
    let b = body
        .get(cursor..cursor + 4)
        .ok_or_else(|| Error::invalid(format!("MP4 emsg: truncated {what}")))?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u64(body: &[u8], cursor: usize, what: &str) -> Result<u64> {
    let b = body
        .get(cursor..cursor + 8)
        .ok_or_else(|| Error::invalid(format!("MP4 emsg: truncated {what}")))?;
    Ok(u64::from_be_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// Parse an `emsg` box body (everything after the 8-byte size/fourcc
/// header, i.e. starting at the FullBox version byte).
///
/// Branches on `version` **before** reading any body field — the very
/// first field differs between the versions (v0 leads with the
/// `scheme_id_uri` string, v1 with the `u32 timescale`). Versions
/// other than 0 / 1 are rejected: their field order is undefined by
/// ISO/IEC 23009-1, so no byte of the body can be interpreted.
pub fn parse_emsg_box(body: &[u8]) -> Result<EmsgBox> {
    if body.len() < 4 {
        return Err(Error::invalid("MP4 emsg: missing FullBox header"));
    }
    let version = body[0];
    // body[1..4] — 24-bit flags, reserved zero per §5.10.3.3; carried
    // values are ignored rather than rejected (reader tolerance).
    match version {
        0 => {
            // string scheme_id_uri; string value; u32 timescale;
            // u32 presentation_time_delta; u32 event_duration; u32 id;
            // message_data to end of box.
            let (scheme_id_uri, cursor) = read_cstring(body, 4, "scheme_id_uri")?;
            let (value, cursor) = read_cstring(body, cursor, "value")?;
            let timescale = read_u32(body, cursor, "timescale")?;
            let delta = read_u32(body, cursor + 4, "presentation_time_delta")?;
            let event_duration = read_u32(body, cursor + 8, "event_duration")?;
            let id = read_u32(body, cursor + 12, "id")?;
            Ok(EmsgBox {
                scheme_id_uri,
                value,
                timescale,
                presentation: EmsgTime::Delta(delta),
                event_duration,
                id,
                message_data: body[cursor + 16..].to_vec(),
            })
        }
        1 => {
            // u32 timescale; u64 presentation_time; u32 event_duration;
            // u32 id; string scheme_id_uri; string value; message_data
            // to end of box.
            let timescale = read_u32(body, 4, "timescale")?;
            let presentation_time = read_u64(body, 8, "presentation_time")?;
            let event_duration = read_u32(body, 16, "event_duration")?;
            let id = read_u32(body, 20, "id")?;
            let (scheme_id_uri, cursor) = read_cstring(body, 24, "scheme_id_uri")?;
            let (value, cursor) = read_cstring(body, cursor, "value")?;
            Ok(EmsgBox {
                scheme_id_uri,
                value,
                timescale,
                presentation: EmsgTime::Absolute(presentation_time),
                event_duration,
                id,
                message_data: body[cursor..].to_vec(),
            })
        }
        v => Err(Error::invalid(format!(
            "MP4 emsg: undefined version {v} (field order unknown)"
        ))),
    }
}

/// Serialise an [`EmsgBox`] into a complete `[size]['emsg']` box — the
/// byte-exact inverse of [`parse_emsg_box`]. The FullBox version is
/// selected by the [`EmsgTime`] variant (Delta ⇒ v0, Absolute ⇒ v1);
/// `flags` are emitted as zero per §5.10.3.3.
///
/// Rejects records that would not round-trip: a string carrying an
/// interior NUL byte (the on-wire strings are null-terminated, so an
/// embedded NUL would truncate the string on re-parse and misalign
/// every field after it), and a total box size exceeding the 32-bit
/// header field.
pub fn build_emsg_box(e: &EmsgBox) -> Result<Vec<u8>> {
    for (name, s) in [("scheme_id_uri", &e.scheme_id_uri), ("value", &e.value)] {
        if s.as_bytes().contains(&0) {
            return Err(Error::invalid(format!(
                "MP4 emsg build: {name} contains an interior NUL byte"
            )));
        }
    }
    let mut body = Vec::with_capacity(
        4 + e.scheme_id_uri.len() + e.value.len() + 2 + 20 + e.message_data.len(),
    );
    match e.presentation {
        EmsgTime::Delta(delta) => {
            body.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags 0
            body.extend_from_slice(e.scheme_id_uri.as_bytes());
            body.push(0);
            body.extend_from_slice(e.value.as_bytes());
            body.push(0);
            body.extend_from_slice(&e.timescale.to_be_bytes());
            body.extend_from_slice(&delta.to_be_bytes());
            body.extend_from_slice(&e.event_duration.to_be_bytes());
            body.extend_from_slice(&e.id.to_be_bytes());
        }
        EmsgTime::Absolute(pt) => {
            body.extend_from_slice(&[1, 0, 0, 0]); // version 1 + flags 0
            body.extend_from_slice(&e.timescale.to_be_bytes());
            body.extend_from_slice(&pt.to_be_bytes());
            body.extend_from_slice(&e.event_duration.to_be_bytes());
            body.extend_from_slice(&e.id.to_be_bytes());
            body.extend_from_slice(e.scheme_id_uri.as_bytes());
            body.push(0);
            body.extend_from_slice(e.value.as_bytes());
            body.push(0);
        }
    }
    body.extend_from_slice(&e.message_data);
    let total = u32::try_from(8 + body.len())
        .map_err(|_| Error::invalid("MP4 emsg build: box size exceeds u32"))?;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(b"emsg");
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_v0() -> EmsgBox {
        EmsgBox {
            scheme_id_uri: "urn:scte:scte35:2013:bin".to_string(),
            value: "1001".to_string(),
            timescale: 90_000,
            presentation: EmsgTime::Delta(45_000),
            event_duration: 270_000,
            id: 7,
            message_data: vec![0xFC, 0x30, 0x11, 0x00],
        }
    }

    fn sample_v1() -> EmsgBox {
        EmsgBox {
            scheme_id_uri: "urn:example:events:2024".to_string(),
            value: String::new(),
            timescale: 1_000,
            presentation: EmsgTime::Absolute(0x0001_2345_6789_ABCD),
            event_duration: EMSG_UNKNOWN_DURATION,
            id: 0xDEAD_BEEF,
            message_data: b"payload".to_vec(),
        }
    }

    #[test]
    fn v0_round_trip() {
        let e = sample_v0();
        let bytes = build_emsg_box(&e).unwrap();
        assert_eq!(&bytes[4..8], b"emsg");
        assert_eq!(bytes[8], 0, "version 0");
        let back = parse_emsg_box(&bytes[8..]).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.version(), 0);
        assert_eq!(back.presentation_time_delta(), Some(45_000));
        assert_eq!(back.presentation_time(), None);
        assert!(!back.event_duration_unknown());
    }

    #[test]
    fn v1_round_trip_with_empty_value_and_unknown_duration() {
        let e = sample_v1();
        let bytes = build_emsg_box(&e).unwrap();
        assert_eq!(bytes[8], 1, "version 1");
        let back = parse_emsg_box(&bytes[8..]).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.version(), 1);
        assert_eq!(back.presentation_time(), Some(0x0001_2345_6789_ABCD));
        assert_eq!(back.presentation_time_delta(), None);
        assert!(back.event_duration_unknown());
    }

    #[test]
    fn v0_field_order_is_strings_first() {
        // §5.10.3.3 v0: scheme_id_uri, value, timescale,
        // presentation_time_delta, event_duration, id. Byte-level gate
        // on the on-wire order, independent of the parser.
        let e = EmsgBox {
            scheme_id_uri: "a".to_string(),
            value: "b".to_string(),
            timescale: 1,
            presentation: EmsgTime::Delta(2),
            event_duration: 3,
            id: 4,
            message_data: vec![0xAA],
        };
        let bytes = build_emsg_box(&e).unwrap();
        let body = &bytes[8..];
        assert_eq!(&body[4..6], b"a\0");
        assert_eq!(&body[6..8], b"b\0");
        assert_eq!(&body[8..12], &1u32.to_be_bytes());
        assert_eq!(&body[12..16], &2u32.to_be_bytes());
        assert_eq!(&body[16..20], &3u32.to_be_bytes());
        assert_eq!(&body[20..24], &4u32.to_be_bytes());
        assert_eq!(&body[24..], &[0xAA]);
    }

    #[test]
    fn v1_field_order_is_integers_first() {
        // §5.10.3.3 v1: timescale, presentation_time (u64),
        // event_duration, id, scheme_id_uri, value.
        let e = EmsgBox {
            scheme_id_uri: "a".to_string(),
            value: "b".to_string(),
            timescale: 1,
            presentation: EmsgTime::Absolute(2),
            event_duration: 3,
            id: 4,
            message_data: vec![0xAA],
        };
        let bytes = build_emsg_box(&e).unwrap();
        let body = &bytes[8..];
        assert_eq!(&body[4..8], &1u32.to_be_bytes());
        assert_eq!(&body[8..16], &2u64.to_be_bytes());
        assert_eq!(&body[16..20], &3u32.to_be_bytes());
        assert_eq!(&body[20..24], &4u32.to_be_bytes());
        assert_eq!(&body[24..26], b"a\0");
        assert_eq!(&body[26..28], b"b\0");
        assert_eq!(&body[28..], &[0xAA]);
    }

    #[test]
    fn empty_message_data_round_trips() {
        let mut e = sample_v0();
        e.message_data.clear();
        let bytes = build_emsg_box(&e).unwrap();
        let back = parse_emsg_box(&bytes[8..]).unwrap();
        assert!(back.message_data.is_empty());
        assert_eq!(back, e);
    }

    #[test]
    fn undefined_version_is_rejected() {
        for v in [2u8, 3, 0xFF] {
            let body = [v, 0, 0, 0, 0, 0, 0, 0];
            let err = parse_emsg_box(&body).expect_err("undefined version must fail");
            assert!(format!("{err}").contains("version"), "{err}");
        }
    }

    #[test]
    fn truncated_bodies_are_rejected_not_panic() {
        // Every prefix of a valid v0 and v1 body must either parse to
        // the same record (only possible at full length) or return a
        // clean error — never panic.
        for sample in [sample_v0(), sample_v1()] {
            let bytes = build_emsg_box(&sample).unwrap();
            let body = &bytes[8..];
            let msg_len = sample.message_data.len();
            for cut in 0..body.len() {
                match parse_emsg_box(&body[..cut]) {
                    Ok(parsed) => {
                        // A cut inside message_data still parses (the
                        // blob just shortens) — everything earlier
                        // must have been intact.
                        assert!(cut >= body.len() - msg_len, "cut {cut} of {}", body.len());
                        assert!(parsed.message_data.len() < msg_len);
                    }
                    Err(e) => {
                        assert!(format!("{e}").contains("MP4 emsg"), "{e}");
                    }
                }
            }
        }
    }

    #[test]
    fn unterminated_string_is_rejected() {
        // v0 body whose scheme_id_uri never terminates.
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(b"urn:no-terminator-here");
        let err = parse_emsg_box(&body).expect_err("unterminated string must fail");
        assert!(format!("{err}").contains("unterminated"), "{err}");
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&[0xFF, 0xFE, 0x00]); // invalid UTF-8 + NUL
        body.extend_from_slice(b"\0"); // empty value
        body.extend_from_slice(&[0u8; 16]);
        let err = parse_emsg_box(&body).expect_err("invalid UTF-8 must fail");
        assert!(format!("{err}").contains("UTF-8"), "{err}");
    }

    #[test]
    fn interior_nul_in_string_is_rejected_on_build() {
        let mut e = sample_v0();
        e.value = "a\0b".to_string();
        let err = build_emsg_box(&e).expect_err("interior NUL must fail");
        assert!(format!("{err}").contains("NUL"), "{err}");
    }

    #[test]
    fn nonzero_flags_are_tolerated_on_parse() {
        // Flags are reserved-zero; a producer that sets them anyway
        // should not brick the reader (the fields carry no defined
        // semantics).
        let e = sample_v0();
        let mut bytes = build_emsg_box(&e).unwrap();
        bytes[9] = 0x12; // flags high byte inside the FullBox header
        let back = parse_emsg_box(&bytes[8..]).unwrap();
        assert_eq!(back, e);
    }
}
