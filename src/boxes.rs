//! ISO Base Media File Format box header reader (ISO/IEC 14496-12).
//!
//! A box has a 4-byte big-endian size field followed by a 4-byte FourCC type.
//! - `size == 1` → the *next* 8 bytes are a 64-bit large size.
//! - `size == 0` → box extends to end of file.
//!
//! All multi-byte integers in MP4 are big-endian.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{Error, Result};

/// Decoded box header.
#[derive(Clone, Copy, Debug)]
pub struct BoxHeader {
    /// FourCC type, as a u32 with the 4 ASCII characters in big-endian order.
    pub fourcc: [u8; 4],
    /// Total box size in bytes (including the header itself). `None` means
    /// "rest of file" (input size==0).
    pub total_size: Option<u64>,
    /// Bytes consumed by the header (8 or 16).
    pub header_len: u64,
}

impl BoxHeader {
    pub fn type_str(&self) -> &str {
        std::str::from_utf8(&self.fourcc).unwrap_or("????")
    }

    pub fn payload_size(&self) -> Option<u64> {
        self.total_size.map(|t| t - self.header_len)
    }
}

pub fn read_box_header<R: Read + ?Sized>(r: &mut R) -> Result<Option<BoxHeader>> {
    let mut hdr = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        match r.read(&mut hdr[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Ok(None);
                } else {
                    return Err(Error::invalid("MP4: truncated box header"));
                }
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let mut fourcc = [0u8; 4];
    fourcc.copy_from_slice(&hdr[4..8]);
    let (total_size, header_len) = match size32 {
        0 => (None, 8u64),
        1 => {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext)?;
            let large = u64::from_be_bytes(ext);
            (Some(large), 16u64)
        }
        n => (Some(n as u64), 8u64),
    };
    Ok(Some(BoxHeader {
        fourcc,
        total_size,
        header_len,
    }))
}

/// Read the full payload of a box as bytes. Fails if the box size is unknown.
pub fn read_box_body<R: Read + ?Sized>(r: &mut R, h: &BoxHeader) -> Result<Vec<u8>> {
    let payload = h
        .payload_size()
        .ok_or_else(|| Error::invalid("MP4: cannot read open-ended box body"))?;
    let mut buf = vec![0u8; payload as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Skip the payload of a box in a seekable reader.
pub fn skip_box_body<R: Seek + ?Sized>(r: &mut R, h: &BoxHeader) -> Result<()> {
    if let Some(payload) = h.payload_size() {
        if payload > 0 {
            r.seek(SeekFrom::Current(payload as i64))?;
        }
    } else {
        // "rest of file" — seek to end.
        r.seek(SeekFrom::End(0))?;
    }
    Ok(())
}

/// Convert a 4-char literal into a FourCC byte array.
pub const fn fourcc(s: &str) -> [u8; 4] {
    let b = s.as_bytes();
    [b[0], b[1], b[2], b[3]]
}

// Common box types.
pub const FTYP: [u8; 4] = fourcc("ftyp");
pub const MOOV: [u8; 4] = fourcc("moov");
pub const MVHD: [u8; 4] = fourcc("mvhd");
pub const TRAK: [u8; 4] = fourcc("trak");
pub const TKHD: [u8; 4] = fourcc("tkhd");
/// `tref` — TrackReferenceBox (ISO/IEC 14496-12 §8.3.3).
pub const TREF: [u8; 4] = fourcc("tref");
pub const EDTS: [u8; 4] = fourcc("edts");
pub const MDIA: [u8; 4] = fourcc("mdia");
pub const MDHD: [u8; 4] = fourcc("mdhd");
/// `elng` — ExtendedLanguageBox (ISO/IEC 14496-12 §8.4.6). A peer of
/// the media header inside `mdia`; carries a NULL-terminated BCP 47
/// (RFC 4646) language tag string.
pub const ELNG: [u8; 4] = fourcc("elng");
pub const HDLR: [u8; 4] = fourcc("hdlr");
pub const MINF: [u8; 4] = fourcc("minf");
pub const DINF: [u8; 4] = fourcc("dinf");
pub const STBL: [u8; 4] = fourcc("stbl");
pub const STSD: [u8; 4] = fourcc("stsd");
pub const STTS: [u8; 4] = fourcc("stts");
pub const STSS: [u8; 4] = fourcc("stss");
pub const STSC: [u8; 4] = fourcc("stsc");
pub const STSZ: [u8; 4] = fourcc("stsz");
pub const STZ2: [u8; 4] = fourcc("stz2");
pub const STCO: [u8; 4] = fourcc("stco");
/// `stsh` — ShadowSyncSampleBox (ISO/IEC 14496-12 §8.6.3). Sits inside
/// `stbl`; an optional table of `(shadowed_sample_number,
/// sync_sample_number)` pairs that name an alternative sync sample to
/// use when seeking to a non-sync sample. Ignored in normal forward
/// play.
pub const STSH: [u8; 4] = fourcc("stsh");
/// `sdtp` — SampleDependencyTypeBox (ISO/IEC 14496-12 §8.6.4). Sits
/// inside `stbl`; an optional per-sample table of dependency hints —
/// `is_leading`, `sample_depends_on`, `sample_is_depended_on`,
/// `sample_has_redundancy` — useful for trick-mode playback (fast
/// forward / random-access roll-forward) and for dropping disposable
/// samples without decoding them. One byte per sample;
/// `sample_count` is implicit from `stsz` / `stz2`.
pub const SDTP: [u8; 4] = fourcc("sdtp");
pub const CTTS: [u8; 4] = fourcc("ctts");
/// `cslg` — CompositionToDecodeBox (ISO/IEC 14496-12 §8.6.1.4). Sits
/// inside `stbl` (or `trep`); relates the composition and decoding
/// timelines when signed composition offsets (a v1 `ctts`) are in use.
pub const CSLG: [u8; 4] = fourcc("cslg");
pub const CO64: [u8; 4] = fourcc("co64");
/// `sbgp` — SampleToGroupBox (ISO/IEC 14496-12 §8.9.2). Sits inside
/// `stbl` (or `traf`); a run-length table mapping samples to a sample
/// group description index for a given `grouping_type`. Purely
/// descriptive metadata — the sample-group entries it indexes carry
/// codec/grouping-specific properties parsed by an upper layer.
pub const SBGP: [u8; 4] = fourcc("sbgp");
/// `sgpd` — SampleGroupDescriptionBox (ISO/IEC 14496-12 §8.9.3). Sits
/// inside `stbl` (or `traf`); the table of per-group descriptive
/// entries that an `sbgp` of the same `grouping_type` indexes. Entry
/// payloads are grouping-type-specific and opaque to the container.
pub const SGPD: [u8; 4] = fourcc("sgpd");
pub const ELST: [u8; 4] = fourcc("elst");
pub const MDAT: [u8; 4] = fourcc("mdat");
pub const FREE: [u8; 4] = fourcc("free");
pub const SKIP: [u8; 4] = fourcc("skip");
pub const UDTA: [u8; 4] = fourcc("udta");
pub const META: [u8; 4] = fourcc("meta");
pub const ILST: [u8; 4] = fourcc("ilst");
pub const DATA: [u8; 4] = fourcc("data");
/// `kind` — Track Kind Box (ISO/IEC 14496-12 §8.10.4). Sits inside a
/// track-level `udta` and labels the track's role with a (schemeURI,
/// value) pair. Both strings are NULL-terminated C strings; `value`
/// may be empty (its terminator still present) when `schemeURI`
/// alone fully identifies the kind. Zero or more per track.
pub const KIND: [u8; 4] = fourcc("kind");

// Fragmented-MP4 box types (ISO/IEC 14496-12 §8.8 — Movie Fragments).
pub const MVEX: [u8; 4] = fourcc("mvex");
pub const TREX: [u8; 4] = fourcc("trex");
pub const MOOF: [u8; 4] = fourcc("moof");
pub const MFHD: [u8; 4] = fourcc("mfhd");
pub const TRAF: [u8; 4] = fourcc("traf");
pub const TFHD: [u8; 4] = fourcc("tfhd");
pub const TFDT: [u8; 4] = fourcc("tfdt");
pub const TRUN: [u8; 4] = fourcc("trun");
pub const SIDX: [u8; 4] = fourcc("sidx");
pub const STYP: [u8; 4] = fourcc("styp");
// Random-access boxes (§8.8.10–12 + §8.16.3).
pub const MFRA: [u8; 4] = fourcc("mfra");
pub const TFRA: [u8; 4] = fourcc("tfra");
pub const MFRO: [u8; 4] = fourcc("mfro");

// Handler types.
pub const HANDLER_SOUN: [u8; 4] = fourcc("soun");
pub const HANDLER_VIDE: [u8; 4] = fourcc("vide");
/// `subt` — Subtitle media (ISO/IEC 14496-12 §12.6.1).
pub const HANDLER_SUBT: [u8; 4] = fourcc("subt");
/// `text` — Timed text media (ISO/IEC 14496-12 §12.5.1). Also used by
/// the QuickTime / 3GPP `tx3g` carriage.
pub const HANDLER_TEXT: [u8; 4] = fourcc("text");
/// `sbtl` — QuickTime subtitle handler (legacy variant; common in
/// `.mov` files muxed by Apple tools alongside the spec `subt`).
pub const HANDLER_SBTL: [u8; 4] = fourcc("sbtl");
/// `meta` — Timed metadata handler (ISO/IEC 14496-12 §8.11).
pub const HANDLER_META: [u8; 4] = fourcc("meta");

// Protection scheme boxes (ISO/IEC 14496-12 §8.12). Files containing
// CENC-encrypted media rewrite the sample-entry FourCC to one of the
// `enc*` placeholders below and bury the original FourCC plus the
// scheme parameters inside a `sinf` container.
//
// We recognise `enc*` and unwrap to the original FourCC via `sinf/frma`
// so callers see the right codec id. The actual key-management
// surface (`tenc`, `pssh`, `senc`, `saiz`/`saio` payloads) is
// scheme-specific and lives in ISO/IEC 23001-7, which is partially
// covered in `docs/container/cenc/`; full CENC decryption is a
// separate slice.
pub const SINF: [u8; 4] = fourcc("sinf");
pub const FRMA: [u8; 4] = fourcc("frma");
pub const SCHM: [u8; 4] = fourcc("schm");
pub const SCHI: [u8; 4] = fourcc("schi");
pub const ENCV: [u8; 4] = fourcc("encv");
pub const ENCA: [u8; 4] = fourcc("enca");
pub const ENCT: [u8; 4] = fourcc("enct");
pub const ENCS: [u8; 4] = fourcc("encs");
