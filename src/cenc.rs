//! Common Encryption (CENC) metadata parsers — ISO/IEC 23001-7:2016.
//!
//! Parse-only support for the three boxes that carry CENC framing on top
//! of the protected-sample-entry envelope already handled in
//! `demux.rs` (the `sinf/frma/schm` unwrap). No decryption is performed;
//! the structures here surface key identifiers, IV layout, and (when
//! present) per-sample IV / subsample maps to a downstream layer.
//!
//! Box coverage:
//!
//! * `tenc` (Track Encryption Box) — §8.2: track-level defaults
//!   (`default_isProtected`, `default_Per_Sample_IV_Size`, `default_KID`)
//!   plus v1 pattern-encryption parameters (`default_crypt_byte_block`,
//!   `default_skip_byte_block`) and the optional constant-IV used when
//!   `default_isProtected==1 && default_Per_Sample_IV_Size==0`.
//! * `pssh` (Protection System Specific Header Box) — §8.1: a DRM
//!   system's opaque header keyed by a 16-byte SystemID UUID, optionally
//!   carrying (v1) a list of applicable KIDs.
//! * `senc` (Sample Encryption Box) — §7.2: per-sample IV array and an
//!   optional (per `flags & 0x000002`) subsample table of
//!   `{BytesOfClearData u16, BytesOfProtectedData u32}` pairs. Parsed
//!   only when the caller supplies the `per_sample_iv_size` (recovered
//!   from the matching `tenc.default_Per_Sample_IV_Size`); the box
//!   itself does not carry it (§7.2.3).
//!
//! All fields are returned exactly as encoded — endianness is converted
//! but no further normalisation is applied. The constant-IV slice is
//! returned only when the spec mandates its presence; pattern-encryption
//! fields are surfaced separately so callers can detect v1 boxes.

use oxideav_core::{Error, Result};

/// `tenc` — TrackEncryptionBox payload (§8.2).
///
/// The version field comes from the surrounding FullBox header. v0
/// covers basic per-sample-IV encryption; v1 adds the
/// `crypt_byte_block` / `skip_byte_block` pattern-encryption pair
/// required by the `cens` / `cbcs` schemes (§10.3 / §10.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TencBox {
    /// FullBox version — 0 for plain per-sample-IV encryption, 1 for
    /// pattern-encryption schemes (`cens` / `cbcs`).
    pub version: u8,
    /// `default_isProtected` — 0 if the track default is "not protected"
    /// (samples are plaintext), 1 if encrypted under the scheme named
    /// in the outer `schm`. Values 2..=0xFF are reserved (§9.1).
    pub default_is_protected: u8,
    /// `default_Per_Sample_IV_Size` — supported values are 0, 8, 16
    /// (§9.1). Zero with `default_is_protected==1` signals that a
    /// constant IV is in use (and `default_constant_iv` must be
    /// populated).
    pub default_per_sample_iv_size: u8,
    /// `default_KID` — the 16-byte key identifier associated with the
    /// track default.
    pub default_kid: [u8; 16],
    /// v1: count of encrypted 16-byte AES blocks in the protection
    /// pattern. Zero (and `skip == 0`) means the pattern is not in use
    /// even on a v1 box. Always zero for v0.
    pub default_crypt_byte_block: u8,
    /// v1: count of unencrypted 16-byte AES blocks in the protection
    /// pattern. Always zero for v0.
    pub default_skip_byte_block: u8,
    /// When `default_is_protected==1 && default_per_sample_iv_size==0`,
    /// the spec requires a constant IV (8 or 16 bytes). `None` in every
    /// other configuration.
    pub default_constant_iv: Option<Vec<u8>>,
}

/// `pssh` — ProtectionSystemSpecificHeaderBox payload (§8.1).
///
/// One per DRM system signalled in the file. Multiple instances are
/// permitted at moov level and at moof level; the spec instructs
/// readers to consider both pools when looking up a SystemID match
/// (§8.1.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsshBox {
    /// FullBox version — 0 omits the KID array; 1 includes it.
    pub version: u8,
    /// 16-byte UUID identifying the content protection system. The
    /// concrete UUID values (Widevine, PlayReady, Marlin, W3C Clear
    /// Key, etc.) live in a public registry outside this spec; treat
    /// as an opaque 128-bit tag.
    pub system_id: [u8; 16],
    /// v1: list of KIDs the `data` blob applies to. Empty (and absent
    /// from the on-wire box) on v0; an empty `kids` Vec on v1 means
    /// `KID_count == 0`, which per §8.1.3 implies "apply to all KIDs
    /// in the containing movie/movie fragment".
    pub kids: Vec<[u8; 16]>,
    /// `Data` blob — opaque DRM-system-specific payload of `DataSize`
    /// bytes.
    pub data: Vec<u8>,
}

/// One entry in a `senc` subsample table (§7.2): a `(clear, protected)`
/// pair describing how a single sample's payload alternates between an
/// unencrypted prefix and an encrypted suffix. Multiple entries within
/// one sample carve a sample into successive `(clear, protected)`
/// runs — useful for NAL-structured video where parameter-set NALs are
/// left in the clear so a non-decrypting demuxer can still walk the
/// stream (§9.5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubsampleEntry {
    /// `BytesOfClearData` — bytes in this subsample that are left in
    /// the clear (u16-wide on the wire).
    pub bytes_of_clear_data: u16,
    /// `BytesOfProtectedData` — bytes in this subsample that are
    /// encrypted (u32-wide on the wire).
    pub bytes_of_protected_data: u32,
}

/// One per-sample entry in a `senc` box: the per-sample IV plus the
/// optional subsample table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SencSample {
    /// `InitializationVector` — exactly `per_sample_iv_size` bytes
    /// (0, 8, or 16; see §9.1). Empty when the track is on a
    /// constant-IV scheme (`per_sample_iv_size == 0`).
    pub initialization_vector: Vec<u8>,
    /// Subsample map; only populated when the `senc` `flags` field has
    /// the `UseSubSampleEncryption` bit (`0x02`) set.
    pub subsamples: Vec<SubsampleEntry>,
}

/// `senc` — SampleEncryptionBox payload (§7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SencBox {
    /// FullBox flags. Only `0x000002` (`UseSubSampleEncryption`) is
    /// defined; preserved verbatim so callers can detect unknown
    /// future flags.
    pub flags: u32,
    /// Per-sample entries; `samples.len() == sample_count` from the
    /// on-wire box.
    pub samples: Vec<SencSample>,
}

impl SencBox {
    /// Set when `flags & 0x000002 != 0`.
    pub fn uses_subsample_encryption(&self) -> bool {
        (self.flags & 0x0000_0002) != 0
    }
}

/// Parse a `tenc` box body (everything after the 8-byte
/// FullBox-in-Box header — i.e. starting from `version << 24 | flags`
/// then `reserved`).
///
/// Layout (§8.2.2):
/// ```text
///   FullBox header (4 bytes: version + flags)
///   reserved u8
///   v0:  reserved u8
///   v1:  default_crypt_byte_block:4 | default_skip_byte_block:4   (one byte)
///   default_isProtected           u8
///   default_Per_Sample_IV_Size    u8
///   default_KID                   u8[16]
///   if (default_isProtected == 1 && default_Per_Sample_IV_Size == 0) {
///       default_constant_IV_size  u8
///       default_constant_IV       u8[default_constant_IV_size]
///   }
/// ```
pub fn parse_tenc(body: &[u8]) -> Result<TencBox> {
    // FullBox header is 4 bytes (version + 24-bit flags).
    if body.len() < 4 {
        return Err(Error::invalid("MP4 tenc: missing FullBox header"));
    }
    let version = body[0];
    // Minimum payload = 4 FullBox + 1 reserved + 1 (reserved or pattern)
    //                 + 1 isProtected + 1 IV_size + 16 KID = 24 bytes.
    if body.len() < 24 {
        return Err(Error::invalid("MP4 tenc: short payload"));
    }
    // body[1..4] are the 24-bit flags — reserved to zero per §8.2 (no
    // semantics defined). Skip.
    // body[4]   reserved
    let (crypt_block, skip_block) = match version {
        0 => {
            // Second reserved byte (must be 0 but we don't enforce).
            (0u8, 0u8)
        }
        _ => {
            // v1 (and any forward-compat ">= 1" per §8.2.3): the
            // second byte is packed `crypt:4 | skip:4`.
            let packed = body[5];
            ((packed >> 4) & 0x0F, packed & 0x0F)
        }
    };
    let default_is_protected = body[6];
    let default_per_sample_iv_size = body[7];
    let mut default_kid = [0u8; 16];
    default_kid.copy_from_slice(&body[8..24]);

    let mut cursor = 24usize;
    let default_constant_iv = if default_is_protected == 1 && default_per_sample_iv_size == 0 {
        if body.len() < cursor + 1 {
            return Err(Error::invalid(
                "MP4 tenc: missing default_constant_IV_size when isProtected==1 && IV_size==0",
            ));
        }
        let civ_size = body[cursor] as usize;
        cursor += 1;
        // §9.1: only 8 and 16 are supported sizes for constant IVs.
        if civ_size != 8 && civ_size != 16 {
            return Err(Error::invalid(format!(
                "MP4 tenc: default_constant_IV_size {civ_size} not in {{8, 16}}"
            )));
        }
        if body.len() < cursor + civ_size {
            return Err(Error::invalid("MP4 tenc: truncated default_constant_IV"));
        }
        let iv = body[cursor..cursor + civ_size].to_vec();
        Some(iv)
    } else {
        // §9.1: when constant IVs are not in use,
        // `default_Per_Sample_IV_Size` must be 0, 8, or 16. The 0 case
        // with `isProtected != 1` simply means "no encryption."
        if !(default_per_sample_iv_size == 0
            || default_per_sample_iv_size == 8
            || default_per_sample_iv_size == 16)
        {
            return Err(Error::invalid(format!(
                "MP4 tenc: default_Per_Sample_IV_Size {default_per_sample_iv_size} not in {{0, 8, 16}}"
            )));
        }
        None
    };

    Ok(TencBox {
        version,
        default_is_protected,
        default_per_sample_iv_size,
        default_kid,
        default_crypt_byte_block: crypt_block,
        default_skip_byte_block: skip_block,
        default_constant_iv,
    })
}

/// Parse a `pssh` box body.
///
/// Layout (§8.1.2):
/// ```text
///   FullBox header               (4 bytes: version + flags)
///   SystemID                     u8[16]
///   if (version > 0) {
///       KID_count                u32
///       KID                      u8[16] × KID_count
///   }
///   DataSize                     u32
///   Data                         u8[DataSize]
/// ```
pub fn parse_pssh(body: &[u8]) -> Result<PsshBox> {
    if body.len() < 4 + 16 {
        return Err(Error::invalid("MP4 pssh: short payload"));
    }
    let version = body[0];
    // body[1..4] are 24-bit flags (reserved zero per spec).
    let mut system_id = [0u8; 16];
    system_id.copy_from_slice(&body[4..20]);

    let mut cursor = 20usize;
    let mut kids: Vec<[u8; 16]> = Vec::new();
    if version > 0 {
        if body.len() < cursor + 4 {
            return Err(Error::invalid("MP4 pssh: missing KID_count"));
        }
        let kid_count = u32::from_be_bytes([
            body[cursor],
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
        ]) as usize;
        cursor += 4;
        // Reject KID arrays that would overrun the box body. The
        // `kid_count` is attacker-controlled (4 GiB-1 fits in u32) so
        // we must not pre-allocate the vector before the byte budget
        // is checked.
        let kid_bytes = kid_count
            .checked_mul(16)
            .ok_or_else(|| Error::invalid("MP4 pssh: KID_count overflow"))?;
        if body.len() < cursor + kid_bytes {
            return Err(Error::invalid("MP4 pssh: truncated KID array"));
        }
        kids.reserve_exact(kid_count);
        for i in 0..kid_count {
            let off = cursor + i * 16;
            let mut k = [0u8; 16];
            k.copy_from_slice(&body[off..off + 16]);
            kids.push(k);
        }
        cursor += kid_bytes;
    }
    if body.len() < cursor + 4 {
        return Err(Error::invalid("MP4 pssh: missing DataSize"));
    }
    let data_size = u32::from_be_bytes([
        body[cursor],
        body[cursor + 1],
        body[cursor + 2],
        body[cursor + 3],
    ]) as usize;
    cursor += 4;
    if body.len() < cursor + data_size {
        return Err(Error::invalid("MP4 pssh: truncated Data"));
    }
    let data = body[cursor..cursor + data_size].to_vec();

    Ok(PsshBox {
        version,
        system_id,
        kids,
        data,
    })
}

/// Parse a `senc` box body.
///
/// The on-wire box does not record the IV width — per §7.2.3 it is
/// recovered from the matching track's `tenc.default_Per_Sample_IV_Size`
/// (or, when a sample group overrides the default, from the
/// corresponding `CencSampleEncryptionInformationGroupEntry`). The
/// caller supplies it as `per_sample_iv_size`; valid values are 0, 8,
/// and 16. A value of 0 means a constant-IV scheme is in effect and
/// the on-wire `InitializationVector` slice is omitted from each
/// entry.
///
/// Layout (§7.2.2):
/// ```text
///   FullBox header                (4 bytes: version + flags)
///   sample_count                  u32
///   {
///       InitializationVector      u8[per_sample_iv_size]
///       if (flags & 0x000002) {       // UseSubSampleEncryption
///           subsample_count       u16
///           {
///               BytesOfClearData      u16
///               BytesOfProtectedData  u32
///           } × subsample_count
///       }
///   } × sample_count
/// ```
pub fn parse_senc(body: &[u8], per_sample_iv_size: u8) -> Result<SencBox> {
    // §9.1: only 0 / 8 / 16 are supported on-wire IV widths.
    if !(per_sample_iv_size == 0 || per_sample_iv_size == 8 || per_sample_iv_size == 16) {
        return Err(Error::invalid(format!(
            "MP4 senc: per_sample_iv_size {per_sample_iv_size} not in {{0, 8, 16}}"
        )));
    }
    if body.len() < 4 + 4 {
        return Err(Error::invalid("MP4 senc: short payload"));
    }
    let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
    let use_subsamples = (flags & 0x0000_0002) != 0;
    let sample_count = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;

    let iv_size = per_sample_iv_size as usize;
    let mut cursor = 8usize;
    let mut samples: Vec<SencSample> = Vec::with_capacity(sample_count.min(body.len() / 8));
    for _ in 0..sample_count {
        if body.len() < cursor + iv_size {
            return Err(Error::invalid("MP4 senc: truncated InitializationVector"));
        }
        let iv = body[cursor..cursor + iv_size].to_vec();
        cursor += iv_size;
        let mut subsamples: Vec<SubsampleEntry> = Vec::new();
        if use_subsamples {
            if body.len() < cursor + 2 {
                return Err(Error::invalid("MP4 senc: missing subsample_count"));
            }
            let sub_count = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
            cursor += 2;
            // Each entry is 6 bytes (u16 clear + u32 protected).
            let sub_bytes = sub_count
                .checked_mul(6)
                .ok_or_else(|| Error::invalid("MP4 senc: subsample_count overflow"))?;
            if body.len() < cursor + sub_bytes {
                return Err(Error::invalid("MP4 senc: truncated subsample table"));
            }
            subsamples.reserve_exact(sub_count);
            for _ in 0..sub_count {
                let clear = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
                let protected = u32::from_be_bytes([
                    body[cursor + 2],
                    body[cursor + 3],
                    body[cursor + 4],
                    body[cursor + 5],
                ]);
                subsamples.push(SubsampleEntry {
                    bytes_of_clear_data: clear,
                    bytes_of_protected_data: protected,
                });
                cursor += 6;
            }
        }
        samples.push(SencSample {
            initialization_vector: iv,
            subsamples,
        });
    }

    Ok(SencBox { flags, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tenc -------------------------------------------------------

    #[test]
    fn tenc_v0_per_sample_iv_16_round_trip() {
        // FullBox v0 + 24-bit flags=0 + reserved 0 + reserved 0 +
        // isProtected=1 + IV_size=16 + KID (16 bytes 0xAA..0xBB).
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]); // version + flags
        body.push(0); // reserved
        body.push(0); // reserved
        body.push(1); // default_isProtected
        body.push(16); // default_Per_Sample_IV_Size
        let kid: [u8; 16] = [
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99,
        ];
        body.extend_from_slice(&kid);
        let t = parse_tenc(&body).expect("v0 parse");
        assert_eq!(t.version, 0);
        assert_eq!(t.default_is_protected, 1);
        assert_eq!(t.default_per_sample_iv_size, 16);
        assert_eq!(t.default_kid, kid);
        assert_eq!(t.default_crypt_byte_block, 0);
        assert_eq!(t.default_skip_byte_block, 0);
        assert!(t.default_constant_iv.is_none());
    }

    #[test]
    fn tenc_v1_pattern_with_constant_iv_8() {
        // Pattern-encryption (cbcs) typical: v1, isProtected=1,
        // IV_size=0 + constant_IV 8 bytes, crypt:skip = 1:9.
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]); // version=1 + flags
        body.push(0); // reserved
        body.push((1 << 4) | 9); // crypt=1 | skip=9
        body.push(1); // default_isProtected
        body.push(0); // default_Per_Sample_IV_Size = 0 → constant IV
        body.extend_from_slice(&[0x01; 16]); // KID
        body.push(8); // default_constant_IV_size
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
        let t = parse_tenc(&body).expect("v1 parse");
        assert_eq!(t.version, 1);
        assert_eq!(t.default_crypt_byte_block, 1);
        assert_eq!(t.default_skip_byte_block, 9);
        assert_eq!(t.default_per_sample_iv_size, 0);
        assert_eq!(
            t.default_constant_iv.as_deref(),
            Some(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE][..])
        );
    }

    #[test]
    fn tenc_v0_iv_size_8_no_constant() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.push(0);
        body.push(0);
        body.push(1);
        body.push(8);
        body.extend_from_slice(&[0u8; 16]);
        let t = parse_tenc(&body).expect("v0 parse");
        assert_eq!(t.default_per_sample_iv_size, 8);
        assert!(t.default_constant_iv.is_none());
    }

    #[test]
    fn tenc_rejects_unsupported_iv_size() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.push(0);
        body.push(0);
        body.push(1);
        body.push(4); // not in {0, 8, 16}
        body.extend_from_slice(&[0u8; 16]);
        assert!(parse_tenc(&body).is_err());
    }

    #[test]
    fn tenc_rejects_unsupported_constant_iv_size() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.push(0);
        body.push(0);
        body.push(1);
        body.push(0); // IV_size=0 ⇒ constant IV required
        body.extend_from_slice(&[0u8; 16]);
        body.push(4); // constant_IV_size 4 — not 8 or 16
        body.extend_from_slice(&[0u8; 4]);
        assert!(parse_tenc(&body).is_err());
    }

    #[test]
    fn tenc_rejects_short_payload() {
        assert!(parse_tenc(&[0u8, 0, 0]).is_err());
        assert!(parse_tenc(&[0u8; 23]).is_err());
    }

    #[test]
    fn tenc_rejects_truncated_constant_iv() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]);
        body.push(0);
        body.push(0);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&[0u8; 16]);
        body.push(16);
        body.extend_from_slice(&[0u8; 15]); // one byte short
        assert!(parse_tenc(&body).is_err());
    }

    // ---- pssh -------------------------------------------------------

    #[test]
    fn pssh_v0_round_trip() {
        // version=0, flags=0, SystemID = synthetic 0x10..0x1F,
        // Data = 4 bytes 0xAA, 0xBB, 0xCC, 0xDD.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        let sysid: [u8; 16] = [
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
            0x1E, 0x1F,
        ];
        body.extend_from_slice(&sysid);
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let p = parse_pssh(&body).expect("v0 parse");
        assert_eq!(p.version, 0);
        assert_eq!(p.system_id, sysid);
        assert!(p.kids.is_empty());
        assert_eq!(p.data, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn pssh_v1_two_kids_round_trip() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]);
        body.extend_from_slice(&[0x42; 16]);
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&[0xAA; 16]);
        body.extend_from_slice(&[0xBB; 16]);
        body.extend_from_slice(&0u32.to_be_bytes()); // empty Data
        let p = parse_pssh(&body).expect("v1 parse");
        assert_eq!(p.version, 1);
        assert_eq!(p.kids.len(), 2);
        assert_eq!(p.kids[0], [0xAA; 16]);
        assert_eq!(p.kids[1], [0xBB; 16]);
        assert!(p.data.is_empty());
    }

    #[test]
    fn pssh_v1_empty_kid_list_per_spec_means_apply_to_all() {
        // §8.1.3: "Boxes ... with an empty list, SHALL be considered
        // to apply to all KIDs in the file or movie fragment." We
        // surface that as `kids.is_empty()` rather than baking a
        // special enum value — the version is preserved so callers can
        // tell "v0 (no KID list)" apart from "v1 (KID_count == 0)".
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]);
        body.extend_from_slice(&[0u8; 16]);
        body.extend_from_slice(&0u32.to_be_bytes()); // KID_count = 0
        body.extend_from_slice(&3u32.to_be_bytes());
        body.extend_from_slice(&[1, 2, 3]);
        let p = parse_pssh(&body).expect("v1 empty-kid parse");
        assert_eq!(p.version, 1);
        assert!(p.kids.is_empty());
        assert_eq!(p.data, vec![1, 2, 3]);
    }

    #[test]
    fn pssh_rejects_kid_count_overrun() {
        let mut body = Vec::new();
        body.extend_from_slice(&[1u8, 0, 0, 0]);
        body.extend_from_slice(&[0u8; 16]);
        body.extend_from_slice(&100u32.to_be_bytes()); // claims 100 KIDs
        body.extend_from_slice(&[0u8; 16]); // only one
        assert!(parse_pssh(&body).is_err());
    }

    #[test]
    fn pssh_rejects_truncated_data() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.extend_from_slice(&[0u8; 16]);
        body.extend_from_slice(&8u32.to_be_bytes());
        body.extend_from_slice(&[1u8; 4]); // claims 8, supplies 4
        assert!(parse_pssh(&body).is_err());
    }

    // ---- senc -------------------------------------------------------

    #[test]
    fn senc_v0_iv16_no_subsamples_round_trip() {
        // flags=0 (no subsamples), sample_count=2, IV size 16.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&[0x11; 16]);
        body.extend_from_slice(&[0x22; 16]);
        let s = parse_senc(&body, 16).expect("no-sub parse");
        assert_eq!(s.flags, 0);
        assert!(!s.uses_subsample_encryption());
        assert_eq!(s.samples.len(), 2);
        assert_eq!(s.samples[0].initialization_vector, vec![0x11; 16]);
        assert_eq!(s.samples[1].initialization_vector, vec![0x22; 16]);
        assert!(s.samples[0].subsamples.is_empty());
    }

    #[test]
    fn senc_subsamples_round_trip() {
        // flags=0x02 (UseSubSampleEncryption), one sample, IV size 8,
        // two subsamples: (3, 17) and (5, 11).
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x02]); // version + flags
        body.extend_from_slice(&1u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&[0x33; 8]); // IV
        body.extend_from_slice(&2u16.to_be_bytes()); // subsample_count
        body.extend_from_slice(&3u16.to_be_bytes());
        body.extend_from_slice(&17u32.to_be_bytes());
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(&11u32.to_be_bytes());
        let s = parse_senc(&body, 8).expect("sub parse");
        assert!(s.uses_subsample_encryption());
        assert_eq!(s.samples.len(), 1);
        assert_eq!(s.samples[0].initialization_vector, vec![0x33; 8]);
        assert_eq!(s.samples[0].subsamples.len(), 2);
        assert_eq!(s.samples[0].subsamples[0].bytes_of_clear_data, 3);
        assert_eq!(s.samples[0].subsamples[0].bytes_of_protected_data, 17);
        assert_eq!(s.samples[0].subsamples[1].bytes_of_clear_data, 5);
        assert_eq!(s.samples[0].subsamples[1].bytes_of_protected_data, 11);
    }

    #[test]
    fn senc_constant_iv_scheme_iv0() {
        // IV size 0 (constant-IV scheme): per-sample IV slice is empty.
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.extend_from_slice(&3u32.to_be_bytes());
        // No per-sample IV bytes; no subsample table either.
        let s = parse_senc(&body, 0).expect("iv0 parse");
        assert_eq!(s.samples.len(), 3);
        for entry in &s.samples {
            assert!(entry.initialization_vector.is_empty());
        }
    }

    #[test]
    fn senc_rejects_invalid_iv_size() {
        let body = vec![0u8, 0, 0, 0, 0, 0, 0, 0];
        assert!(parse_senc(&body, 4).is_err());
    }

    #[test]
    fn senc_rejects_truncated_iv() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0]);
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&[0x44; 16]);
        // second IV missing entirely
        assert!(parse_senc(&body, 16).is_err());
    }

    #[test]
    fn senc_rejects_truncated_subsample_table() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0u8, 0, 0, 0x02]);
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&[0x55; 16]);
        body.extend_from_slice(&3u16.to_be_bytes()); // claims 3 subs
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // only 1 supplied
        assert!(parse_senc(&body, 16).is_err());
    }
}
