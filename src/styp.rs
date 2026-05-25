//! DASH/CMAF Segment Type Box (`styp`) — write side.
//!
//! ISO/IEC 14496-12:2015 §8.16.2 (p. 104). The Segment Type Box is the
//! write-side dual of `oxideav_mov::styp::parse_styp` (the read-side
//! parser landed in oxideav-mov). It is a file-level box that identifies
//! a DASH / CMAF / HLS-fMP4 media segment and declares the
//! specifications it conforms to. The on-disk layout is identical to
//! `ftyp` (§4.3) — only the box type differs.
//!
//! Per ISO/IEC 14496-12 §8.16.2:
//!
//! * "If segments are stored in separate files … it is recommended that
//!   these 'segment files' contain a segment-type box, which must be
//!   first if present, to enable identification of those files, and
//!   declaration of the specifications with which they are compliant."
//! * "A segment type has the same format as an 'ftyp' box [4.3], except
//!   that it takes the box type 'styp'."
//! * "Valid segment type boxes shall be the first box in a segment."
//!
//! Layout per §8.16.2.2 (= §4.3.2 with the box type switched):
//!
//! ```text
//! aligned(8) class SegmentTypeBox extends Box('styp') {
//!     unsigned int(32) major_brand;
//!     unsigned int(32) minor_version;
//!     unsigned int(32) compatible_brands[];   // to end of box
//! }
//! ```
//!
//! Box header layout (ISO/IEC 14496-12 §4.2):
//!
//! ```text
//! [size:u32 big-endian][type:'s','t','y','p'][body…]
//! ```
//!
//! The emitter here writes the short 32-bit `size` form — every legal
//! `styp` body is well under 4 GiB (compatible-brand lists are tiny). No
//! 64-bit `largesize` extension is needed.
//!
//! `minor_version` is fixed at `0`. Per §4.3.3 the field is informative
//! ("not a version of the major brand"); for `styp` specifically,
//! DASH-IF Interop guidance treats it as reserved-zero. Callers needing
//! a non-zero minor version can write the box themselves using
//! [`build_styp_with_minor`].

use std::io::{Result as IoResult, Write};

use crate::boxes::STYP;

/// Build a `styp` box as a contiguous `Vec<u8>` ready to write to the
/// front of a fragmented-MP4 segment.
///
/// `major_brand` is the segment's primary conformance label (DASH-IF
/// names `msdh`, `msix`, `risx`, CMAF `cmfs` / `cmff`, …). The
/// `compatible_brands` list may be empty; §4.3 (inherited by §8.16.2)
/// permits a zero-length list.
///
/// `minor_version` is set to `0`; see [`build_styp_with_minor`] for the
/// rare case where a writer needs to control it. The 8-byte ISO BMFF
/// box header (`size` + `type`) is prepended, so the returned slice is
/// suitable for direct `write_all`.
///
/// Byte layout for `(major = "iso5", compat = ["iso5", "dash", "msdh"])`:
///
/// ```text
/// 00 00 00 1c                  size = 28
/// 73 74 79 70                  type = 'styp'
/// 69 73 6f 35                  major_brand = 'iso5'
/// 00 00 00 00                  minor_version = 0
/// 69 73 6f 35                  compat[0]    = 'iso5'
/// 64 61 73 68                  compat[1]    = 'dash'
/// 6d 73 64 68                  compat[2]    = 'msdh'
/// ```
pub fn build_styp(major_brand: [u8; 4], compatible_brands: &[[u8; 4]]) -> Vec<u8> {
    build_styp_with_minor(major_brand, 0, compatible_brands)
}

/// Build a `styp` box with an explicit `minor_version`.
///
/// Most DASH/CMAF segments leave `minor_version = 0` (see [`build_styp`]);
/// this entry point exists for parity with the §4.3 `ftyp` shape so a
/// caller round-tripping a parsed [`oxideav_mov::styp::Styp`]
/// (`major + minor + compat`) can re-emit the same byte sequence.
///
/// The function never fails — all inputs are valid by construction. The
/// caller is responsible for not exceeding `u32::MAX - 16` compatible
/// brands (the practical limit is far smaller); §8.16.2 lists at most a
/// handful in real-world DASH/CMAF profiles.
pub fn build_styp_with_minor(
    major_brand: [u8; 4],
    minor_version: u32,
    compatible_brands: &[[u8; 4]],
) -> Vec<u8> {
    // Body: 4 (major) + 4 (minor) + 4×N (compat).
    let body_len = 8 + 4 * compatible_brands.len();
    // Full box: 8-byte header + body.
    let total = 8 + body_len;
    debug_assert!(
        total <= u32::MAX as usize,
        "styp box exceeds u32 size limit"
    );
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&STYP);
    out.extend_from_slice(&major_brand);
    out.extend_from_slice(&minor_version.to_be_bytes());
    for b in compatible_brands {
        out.extend_from_slice(b);
    }
    debug_assert_eq!(out.len(), total);
    out
}

/// Stream-friendly variant of [`build_styp`]: writes the styp box bytes
/// directly to `writer` without an intermediate `Vec` allocation per
/// segment.
///
/// Equivalent to `writer.write_all(&build_styp(major, compat))` but
/// avoids the temporary. Useful when the muxer is emitting one styp per
/// CMAF chunk on a hot path.
pub fn write_styp<W: Write>(
    writer: &mut W,
    major_brand: [u8; 4],
    compatible_brands: &[[u8; 4]],
) -> IoResult<()> {
    write_styp_with_minor(writer, major_brand, 0, compatible_brands)
}

/// Stream-friendly variant of [`build_styp_with_minor`].
pub fn write_styp_with_minor<W: Write>(
    writer: &mut W,
    major_brand: [u8; 4],
    minor_version: u32,
    compatible_brands: &[[u8; 4]],
) -> IoResult<()> {
    let body_len = 8 + 4 * compatible_brands.len();
    let total = (8 + body_len) as u32;
    writer.write_all(&total.to_be_bytes())?;
    writer.write_all(&STYP)?;
    writer.write_all(&major_brand)?;
    writer.write_all(&minor_version.to_be_bytes())?;
    for b in compatible_brands {
        writer.write_all(b)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip — parse the emitted bytes back as a flat box header +
    /// body slice and check the field-by-field decode matches the input.
    /// This is the byte-exact test the §8.16.2 specification implies: the
    /// emitter is correct iff a spec-correct parser recovers the input.
    fn parse_back(bytes: &[u8]) -> ([u8; 4], u32, Vec<[u8; 4]>) {
        // Box header: 4-byte size + 4-byte type.
        let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(size as usize, bytes.len(), "box size header mismatch");
        assert_eq!(&bytes[4..8], b"styp", "box type not styp");
        // Body: 4-byte major + 4-byte minor + N×4 compat (to end-of-box).
        let mut major = [0u8; 4];
        major.copy_from_slice(&bytes[8..12]);
        let minor = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let rest = &bytes[16..];
        assert_eq!(rest.len() % 4, 0, "compat-brand tail not 4-aligned");
        let mut brands = Vec::with_capacity(rest.len() / 4);
        for c in rest.chunks_exact(4) {
            let mut b = [0u8; 4];
            b.copy_from_slice(c);
            brands.push(b);
        }
        (major, minor, brands)
    }

    #[test]
    fn build_styp_byte_exact_for_dash_segment() {
        // Worked example from the module-level doc: msdh-major DASH
        // segment with three compatible brands. The expected layout is
        // 8-byte header + 8-byte (major+minor) + 12 bytes of compat =
        // 28 bytes total.
        let got = build_styp(*b"iso5", &[*b"iso5", *b"dash", *b"msdh"]);
        let want: &[u8] = &[
            0x00, 0x00, 0x00, 0x1c, // size = 28
            b's', b't', b'y', b'p', // type = 'styp'
            b'i', b's', b'o', b'5', // major = 'iso5'
            0x00, 0x00, 0x00, 0x00, // minor = 0
            b'i', b's', b'o', b'5', // compat[0]
            b'd', b'a', b's', b'h', // compat[1]
            b'm', b's', b'd', b'h', // compat[2]
        ];
        assert_eq!(got.as_slice(), want);
    }

    #[test]
    fn build_styp_with_empty_compat_is_sixteen_bytes() {
        // §4.3 (inherited by §8.16.2) permits a zero-length compatible-
        // brands list; the box is then exactly 8-byte header + 8-byte
        // (major + minor) = 16 bytes.
        let got = build_styp(*b"msdh", &[]);
        assert_eq!(got.len(), 16);
        let want: &[u8] = &[
            0x00, 0x00, 0x00, 0x10, // size = 16
            b's', b't', b'y', b'p', // type = 'styp'
            b'm', b's', b'd', b'h', // major = 'msdh'
            0x00, 0x00, 0x00, 0x00, // minor = 0
        ];
        assert_eq!(got.as_slice(), want);
    }

    #[test]
    fn build_styp_with_minor_round_trips_field_set() {
        // Lift a parsed-style (major, minor, compat) triple back through
        // the emitter and verify a manual parse recovers the same fields.
        let major = *b"avif";
        let minor = 0x0001_0002;
        let compat = vec![*b"mif1", *b"miaf"];
        let bytes = build_styp_with_minor(major, minor, &compat);
        let (got_major, got_minor, got_compat) = parse_back(&bytes);
        assert_eq!(got_major, major);
        assert_eq!(got_minor, minor);
        assert_eq!(got_compat, compat);
    }

    #[test]
    fn write_styp_matches_build_styp() {
        // The stream-emitter and the Vec-builder must produce the same
        // byte sequence for the same arguments.
        let major = *b"iso6";
        let compat = vec![*b"cmfs", *b"msdh"];
        let v = build_styp(major, &compat);
        let mut sink = Vec::new();
        write_styp(&mut sink, major, &compat).unwrap();
        assert_eq!(sink, v);
    }

    #[test]
    fn write_styp_with_minor_matches_build_with_minor() {
        let major = *b"heic";
        let minor = 0xDEAD_BEEF;
        let compat = vec![*b"mif1"];
        let v = build_styp_with_minor(major, minor, &compat);
        let mut sink = Vec::new();
        write_styp_with_minor(&mut sink, major, minor, &compat).unwrap();
        assert_eq!(sink, v);
    }

    #[test]
    fn build_styp_preserves_compat_brand_order() {
        // §4.3.3 / §8.16.2 leave the order of `compatible_brands` to the
        // writer; the emitter must preserve it verbatim.
        let in_order = vec![*b"iso5", *b"msdh", *b"msix"];
        let bytes = build_styp(*b"iso5", &in_order);
        let (_major, _minor, got) = parse_back(&bytes);
        assert_eq!(got, in_order);
    }

    #[test]
    fn write_styp_with_empty_compat_writes_sixteen_bytes() {
        let mut sink = Vec::new();
        write_styp(&mut sink, *b"cmfs", &[]).unwrap();
        assert_eq!(sink.len(), 16);
        assert_eq!(&sink[4..8], b"styp");
        assert_eq!(&sink[8..12], b"cmfs");
        assert_eq!(&sink[12..16], &[0u8; 4]);
    }

    #[test]
    fn build_styp_box_size_field_matches_total_length() {
        // Sanity: the box header's `size` field equals the total byte
        // length of the emitted bytes — invariant of every legal ISO BMFF
        // short-form box (§4.2 — `size` includes its own 4 bytes and the
        // 4-byte type).
        for (major, compat) in [
            (*b"iso5", &[][..]),
            (*b"msdh", &[*b"msdh", *b"msix"][..]),
            (*b"iso6", &[*b"iso6", *b"cmfs", *b"cmfc", *b"dash"][..]),
        ] {
            let bytes = build_styp(major, compat);
            let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(size as usize, bytes.len());
        }
    }

    #[test]
    fn build_styp_with_one_compat_brand_is_twenty_bytes() {
        // 8-byte header + 8-byte (major+minor) + 4-byte single compat =
        // 20 bytes. Smallest "real" segment marker shape.
        let bytes = build_styp(*b"msdh", &[*b"msdh"]);
        assert_eq!(bytes.len(), 20);
    }

    #[test]
    fn write_styp_returns_io_error_when_writer_fails() {
        // Stub writer that always errors — the emitter should propagate
        // the error verbatim, without silently truncating the styp box.
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> IoResult<usize> {
                Err(std::io::Error::other("boom"))
            }
            fn flush(&mut self) -> IoResult<()> {
                Ok(())
            }
        }
        let res = write_styp(&mut FailingWriter, *b"iso5", &[*b"msdh"]);
        assert!(res.is_err(), "writer error should propagate");
    }
}
