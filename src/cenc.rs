//! Common Encryption (CENC) metadata parsers — ISO/IEC 23001-7:2016.
//!
//! Parse-only support for the three boxes that carry CENC framing on top
//! of the protected-sample-entry envelope already handled in
//! `demux.rs` (the `sinf/frma/schm` unwrap). No decryption is performed
//! by *this module*; the structures here surface key identifiers, IV
//! layout, and (when present) per-sample IV / subsample maps. The
//! AES-128 CTR / CBC execution layer that consumes them lives in
//! [`crate::cenc_cipher`].
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
//!
//! ## Scheme typing + decryption router
//!
//! The four protection schemes defined by ISO/IEC 23001-7:2016 §4.2 are
//! exposed as the typed [`CencScheme`] enum. A scheme value packages
//! the two routing axes a downstream decryptor needs in order to pick a
//! code path:
//!
//! * **Cipher mode** (`AES-CTR` vs `AES-CBC`) — drives the actual block
//!   operation. See [`CencScheme::cipher_mode`].
//! * **Pattern encryption** (`cens` / `cbcs` only) — selects whether the
//!   protected range is fully encrypted or follows a `(crypt_byte_block,
//!   skip_byte_block)` pattern recovered from the v1 `tenc`. See
//!   [`CencScheme::uses_pattern_encryption`].
//!
//! The combination of the parsed `tenc` and a `CencScheme` is bundled
//! into a [`CencSchemeDecision`] — the typed "routing slip" the
//! decryption layer pattern-matches against. This module performs no
//! AES operation; the bundle is a static dispatch contract built from
//! container-side bytes only. The actual AES block execution is in
//! [`crate::cenc_cipher::decrypt_sample_in_place`], which takes the
//! decision plus caller-supplied key material.
//!
//! The §6 `CencSampleEncryptionInformationGroupEntry` (`seig`) — a
//! sample-group description entry that lets a track override the
//! default `tenc` parameters for a group of samples — is parsed by
//! [`parse_seig`] into a [`SeigEntry`]. Combined with the matching
//! `sbgp` of grouping type `*b"seig"` already surfaced by the demuxer,
//! a CENC consumer recovers the per-sample-group `(KID, IV-size,
//! constant-IV, pattern)` override picture without further byte-layer
//! work in this crate.

use oxideav_core::{Error, Result};

/// The four CENC protection schemes (§4.2 + §10).
///
/// Each variant names a `scheme_type` four-character code that appears
/// in the `sinf/schm` box. The fourth variant — [`CencScheme::Unknown`] —
/// preserves any other value verbatim (private DRM dialects sometimes
/// register their own scheme codes that nonetheless travel the same
/// `sinf/schm` envelope; a typed router should pass those through
/// without losing them).
///
/// The two binary axes that any decryption layer needs are the
/// **cipher mode** (CTR vs CBC) and whether **pattern encryption** is
/// in effect (`cens` / `cbcs` only — §9.6). They are surfaced as
/// dedicated accessors so the caller does not have to re-read the
/// FourCC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CencScheme {
    /// `cenc` — AES-CTR full-sample or video-NAL subsample encryption
    /// (§10.1). Counter mode. No pattern. Mandatory to support per
    /// §10.1.
    Cenc,
    /// `cbc1` — AES-CBC full-sample or video-NAL subsample encryption
    /// (§10.2). Cipher-block-chaining mode. No pattern. Optional.
    Cbc1,
    /// `cens` — AES-CTR subsample **pattern** encryption (§10.3).
    /// Counter mode with `(crypt_byte_block, skip_byte_block)` pattern.
    /// `tenc.version == 1` required.
    Cens,
    /// `cbcs` — AES-CBC subsample **pattern** encryption (§10.4).
    /// Cipher-block-chaining mode with `(crypt_byte_block,
    /// skip_byte_block)` pattern. Constant-IV per sample group is the
    /// typical configuration. `tenc.version == 1` required.
    Cbcs,
    /// A `scheme_type` four-character code not registered in §10.
    /// Surfaced verbatim so callers carrying a private DRM dialect can
    /// route on their own table. No cipher-mode / pattern guarantees
    /// can be made; the typed accessors return `None` / `false`.
    Unknown([u8; 4]),
}

/// The AES block cipher mode of operation a [`CencScheme`] dispatches
/// to (§9.3 / §9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherMode {
    /// AES-CTR (counter mode) — used by `cenc` and `cens` (§9.3).
    Ctr,
    /// AES-CBC (cipher-block-chaining) — used by `cbc1` and `cbcs`
    /// (§9.4.3).
    Cbc,
}

impl CencScheme {
    /// Map a `sinf/schm.scheme_type` four-character code to the typed
    /// [`CencScheme`]. Codes outside the §10 set become
    /// [`CencScheme::Unknown`] — the FourCC is preserved so a caller
    /// with a private registry can still route.
    pub fn from_fourcc(fourcc: &[u8; 4]) -> CencScheme {
        match fourcc {
            b"cenc" => CencScheme::Cenc,
            b"cbc1" => CencScheme::Cbc1,
            b"cens" => CencScheme::Cens,
            b"cbcs" => CencScheme::Cbcs,
            other => CencScheme::Unknown(*other),
        }
    }

    /// The FourCC that appears on the wire for this scheme.
    pub fn fourcc(&self) -> [u8; 4] {
        match self {
            CencScheme::Cenc => *b"cenc",
            CencScheme::Cbc1 => *b"cbc1",
            CencScheme::Cens => *b"cens",
            CencScheme::Cbcs => *b"cbcs",
            CencScheme::Unknown(fc) => *fc,
        }
    }

    /// The AES cipher mode this scheme dispatches to. `None` for an
    /// unrecognised scheme — a caller routing on a private dialect must
    /// supply its own mode mapping.
    pub fn cipher_mode(&self) -> Option<CipherMode> {
        match self {
            CencScheme::Cenc | CencScheme::Cens => Some(CipherMode::Ctr),
            CencScheme::Cbc1 | CencScheme::Cbcs => Some(CipherMode::Cbc),
            CencScheme::Unknown(_) => None,
        }
    }

    /// `true` for the pattern-encryption schemes (`cens` / `cbcs`,
    /// §9.6). For non-pattern schemes (`cenc` / `cbc1`) the
    /// `(crypt_byte_block, skip_byte_block)` pair on `tenc` is required
    /// to be zero (§10.1 / §10.2). Unknown schemes return `false`.
    pub fn uses_pattern_encryption(&self) -> bool {
        matches!(self, CencScheme::Cens | CencScheme::Cbcs)
    }

    /// The `tenc.version` value the scheme pins per §10. `cenc` and
    /// `cbc1` pin v0; `cens` and `cbcs` pin v1. Unknown schemes return
    /// `None` (the version is not constrained by this part).
    pub fn required_tenc_version(&self) -> Option<u8> {
        match self {
            CencScheme::Cenc | CencScheme::Cbc1 => Some(0),
            CencScheme::Cens | CencScheme::Cbcs => Some(1),
            CencScheme::Unknown(_) => None,
        }
    }
}

/// A typed routing slip — the combination of a parsed `tenc` (track
/// defaults) and the four-character `scheme_type` recovered from
/// `sinf/schm`, packaged so a downstream decryption layer can pick a
/// code path without re-reading any byte-layer structure.
///
/// The decision is intentionally **dispatch metadata only**: it carries
/// no key material, runs no AES operation, and does not pre-derive a
/// per-sample IV. Its job is to package the container-side facts
/// (scheme FourCC, cipher mode, pattern flag, per-sample-IV vs
/// constant-IV vs no-IV configuration) so that a caller with a key
/// blob in hand has a single typed value to switch on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CencSchemeDecision {
    /// The parsed `scheme_type` from `sinf/schm`.
    pub scheme: CencScheme,
    /// The track-level defaults from `tenc`.
    pub tenc: TencBox,
}

/// The track-level IV-supply discipline a [`CencSchemeDecision`]
/// dispatches to (§9.1).
///
/// Each protected sample carries an IV via exactly one of:
///
/// * **Per-sample** (`per_sample_iv_size ∈ {8, 16}`) — the IV is stored
///   either inside `senc` or in `mdat`-resident sample-auxiliary
///   information located via `saiz` + `saio`.
/// * **Constant** (`per_sample_iv_size == 0 && isProtected == 1`) — the
///   IV is the `default_constant_IV` on `tenc` (or the equivalent on a
///   `seig` group entry); no per-sample IV bytes are stored.
/// * **None** (`isProtected == 0`) — the sample is unprotected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IvSupply {
    /// Per-sample IV stored on the wire; width is 8 or 16 bytes.
    PerSample { size: u8 },
    /// Constant IV used for every sample in the track or group; the
    /// actual bytes live on the [`TencBox`] (or [`SeigEntry`]).
    Constant,
    /// `isProtected == 0` — the sample is not encrypted.
    None,
}

impl CencSchemeDecision {
    /// Bundle a parsed `tenc` and a `scheme_type` FourCC into a typed
    /// router. Performs **structural** validation only — no key
    /// material, no AES operation:
    ///
    /// * the scheme's `required_tenc_version()` (when known) must match
    ///   `tenc.version`, and
    /// * pattern-encryption schemes (`cens` / `cbcs`) must carry a
    ///   non-zero `(crypt_byte_block, skip_byte_block)` pair (§9.6 —
    ///   "When the fields … are non-zero numbers, pattern encryption
    ///   SHALL be applied").
    ///
    /// Mismatches return `Err` with a descriptive message; a CENC
    /// consumer can then decide whether to refuse the file or treat it
    /// as a known-bad fixture.
    pub fn new(scheme: CencScheme, tenc: TencBox) -> Result<CencSchemeDecision> {
        if let Some(req) = scheme.required_tenc_version() {
            if tenc.version != req {
                return Err(Error::invalid(format!(
                    "CENC scheme decision: scheme {} requires tenc version {} but tenc.version = {}",
                    fourcc_str(&scheme.fourcc()),
                    req,
                    tenc.version
                )));
            }
        }
        if scheme.uses_pattern_encryption()
            && tenc.default_crypt_byte_block == 0
            && tenc.default_skip_byte_block == 0
        {
            return Err(Error::invalid(format!(
                "CENC scheme decision: pattern-encryption scheme {} requires non-zero (crypt_byte_block, skip_byte_block)",
                fourcc_str(&scheme.fourcc())
            )));
        }
        Ok(CencSchemeDecision { scheme, tenc })
    }

    /// The AES cipher mode this decision dispatches to. `None` for an
    /// unknown scheme (the typed [`CencScheme::Unknown`] variant); a
    /// caller routing on a private dialect supplies its own mapping.
    pub fn cipher_mode(&self) -> Option<CipherMode> {
        self.scheme.cipher_mode()
    }

    /// `true` if the scheme is `cens` or `cbcs`.
    pub fn uses_pattern_encryption(&self) -> bool {
        self.scheme.uses_pattern_encryption()
    }

    /// The track-level IV-supply discipline implied by the parsed
    /// `tenc` (§9.1):
    ///
    /// * `IvSupply::None` when `default_isProtected == 0` — there is no
    ///   IV because there is nothing to decrypt at the track default.
    /// * `IvSupply::Constant` when `default_isProtected == 1 &&
    ///   default_Per_Sample_IV_Size == 0` — IV comes from
    ///   `tenc.default_constant_iv` (or a `seig` group override).
    /// * `IvSupply::PerSample { size }` otherwise (`size` is 8 or 16) —
    ///   IV comes from `senc` or `saiz` / `saio` per sample.
    ///
    /// A `seig` override on a sample group can replace either of the
    /// `IvSupply::Constant` / `IvSupply::PerSample` arms for a subset
    /// of samples; the typed group-level analogue is on [`SeigEntry`].
    pub fn iv_supply(&self) -> IvSupply {
        if self.tenc.default_is_protected != 1 {
            return IvSupply::None;
        }
        if self.tenc.default_per_sample_iv_size == 0 {
            IvSupply::Constant
        } else {
            IvSupply::PerSample {
                size: self.tenc.default_per_sample_iv_size,
            }
        }
    }
}

/// `CencSampleEncryptionInformationGroupEntry` (`seig`) — a per-sample-group
/// override of the `tenc` defaults (§6). Paired with an `sbgp` of
/// `grouping_type = *b"seig"`, this entry overrides
/// `(isProtected, Per_Sample_IV_Size, KID, pattern, constant_IV)` for
/// the samples mapped to the group.
///
/// The on-wire layout (§6) is:
///
/// ```text
/// aligned(8) class CencSampleEncryptionInformationGroupEntry
///     extends SampleGroupEntry('seig')
/// {
///     unsigned int(8)    reserved = 0;
///     unsigned int(4)    crypt_byte_block;
///     unsigned int(4)    skip_byte_block;
///     unsigned int(8)    isProtected;
///     unsigned int(8)    Per_Sample_IV_Size;
///     unsigned int(8)[16] KID;
///     if (isProtected == 1 && Per_Sample_IV_Size == 0) {
///         unsigned int(8)    constant_IV_size;
///         unsigned int(8)[constant_IV_size] constant_IV;
///     }
/// }
/// ```
///
/// Note the "clients SHALL ignore additional bytes after the fields
/// defined" rule (§6 closing paragraph): trailing bytes from a future
/// edition are not a parse error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeigEntry {
    /// Pattern-encryption encrypted-block count (always zero on the
    /// non-pattern schemes per §10.1 / §10.2).
    pub crypt_byte_block: u8,
    /// Pattern-encryption skipped-block count.
    pub skip_byte_block: u8,
    /// `isProtected` for samples mapped to this group. `0` overrides
    /// the track default to "unencrypted for this group" (§6 second
    /// paragraph).
    pub is_protected: u8,
    /// `Per_Sample_IV_Size` for samples mapped to this group; valid
    /// values are 0 / 8 / 16 (§9.1).
    pub per_sample_iv_size: u8,
    /// `KID` for samples mapped to this group. May differ from the
    /// track default `tenc.default_KID`, allowing a sample group to be
    /// decrypted with a separate key.
    pub kid: [u8; 16],
    /// When `is_protected == 1 && per_sample_iv_size == 0`, the
    /// constant IV bytes (8 or 16). `None` otherwise.
    pub constant_iv: Option<Vec<u8>>,
}

impl SeigEntry {
    /// Group-level IV-supply discipline (the per-group analogue of
    /// [`CencSchemeDecision::iv_supply`]).
    pub fn iv_supply(&self) -> IvSupply {
        if self.is_protected != 1 {
            return IvSupply::None;
        }
        if self.per_sample_iv_size == 0 {
            IvSupply::Constant
        } else {
            IvSupply::PerSample {
                size: self.per_sample_iv_size,
            }
        }
    }

    /// Whether this group entry overrides the default to a
    /// pattern-encryption configuration (§9.6 — "When the fields … are
    /// non-zero numbers, pattern encryption SHALL be applied").
    pub fn uses_pattern_encryption(&self) -> bool {
        self.crypt_byte_block != 0 || self.skip_byte_block != 0
    }
}

/// Parse one `CencSampleEncryptionInformationGroupEntry` payload (§6).
///
/// The input is the per-entry blob handed to a `grouping_type =
/// *b"seig"` `sgpd` description — i.e. the bytes the `sample_groups`
/// surface already preserves verbatim. The trailing-bytes rule of §6
/// is honoured (extra bytes after the fixed-size record are silently
/// ignored; a future edition's extension does not fail the parse).
///
/// Returns the typed [`SeigEntry`].
pub fn parse_seig(body: &[u8]) -> Result<SeigEntry> {
    // Fixed prefix: 1 reserved + 1 packed (crypt|skip) + 1 isProtected
    //              + 1 IV_size + 16 KID = 20 bytes.
    if body.len() < 20 {
        return Err(Error::invalid("CENC seig: short payload"));
    }
    // body[0] reserved (must be 0; not enforced).
    let packed = body[1];
    let crypt_byte_block = (packed >> 4) & 0x0F;
    let skip_byte_block = packed & 0x0F;
    let is_protected = body[2];
    let per_sample_iv_size = body[3];
    let mut kid = [0u8; 16];
    kid.copy_from_slice(&body[4..20]);

    let mut cursor = 20usize;
    let constant_iv = if is_protected == 1 && per_sample_iv_size == 0 {
        if body.len() < cursor + 1 {
            return Err(Error::invalid(
                "CENC seig: missing constant_IV_size when isProtected==1 && IV_size==0",
            ));
        }
        let civ_size = body[cursor] as usize;
        cursor += 1;
        // §9.1: only 8 and 16 are supported sizes for constant IVs.
        if civ_size != 8 && civ_size != 16 {
            return Err(Error::invalid(format!(
                "CENC seig: constant_IV_size {civ_size} not in {{8, 16}}"
            )));
        }
        if body.len() < cursor + civ_size {
            return Err(Error::invalid("CENC seig: truncated constant_IV"));
        }
        Some(body[cursor..cursor + civ_size].to_vec())
    } else {
        // §9.1: legal IV sizes are 0, 8, 16. Reject other values here so
        // a malformed sample-group entry doesn't smuggle "IV size 4"
        // past the typed router.
        if !(per_sample_iv_size == 0 || per_sample_iv_size == 8 || per_sample_iv_size == 16) {
            return Err(Error::invalid(format!(
                "CENC seig: Per_Sample_IV_Size {per_sample_iv_size} not in {{0, 8, 16}}"
            )));
        }
        None
    };

    Ok(SeigEntry {
        crypt_byte_block,
        skip_byte_block,
        is_protected,
        per_sample_iv_size,
        kid,
        constant_iv,
    })
}

/// Lossy ASCII rendering of a FourCC, used inside error messages.
fn fourcc_str(fc: &[u8; 4]) -> String {
    let mut out = String::with_capacity(4);
    for &b in fc {
        if (0x20..=0x7E).contains(&b) {
            out.push(b as char);
        } else {
            out.push('?');
        }
    }
    out
}

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

// ---- Write side (§7.2 / §8.1 / §8.2 emission) --------------------------

/// Append a complete `[size][fourcc]` box around `body` bytes.
fn wrap_full_box(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = 8 + body.len() as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

/// Serialise a [`TencBox`] into a complete `[size]['tenc']` box — the
/// byte-exact inverse of [`parse_tenc`] (§8.2.2). Rejects records that
/// would not round-trip:
///
/// * a version other than 0 / 1,
/// * a v0 record carrying a non-zero pattern pair (the packed
///   `crypt:4|skip:4` byte only exists at v1),
/// * a pattern component above the 4-bit field ceiling,
/// * `default_per_sample_iv_size` outside {0, 8, 16},
/// * a constant IV whose presence disagrees with the §9.1 rule
///   (`Some` iff `isProtected == 1 && IV_size == 0`) or whose length
///   is not 8 / 16.
pub fn build_tenc_box(tenc: &TencBox) -> Result<Vec<u8>> {
    if tenc.version > 1 {
        return Err(Error::invalid(format!(
            "MP4 tenc build: undefined version {}",
            tenc.version
        )));
    }
    if tenc.version == 0
        && (tenc.default_crypt_byte_block != 0 || tenc.default_skip_byte_block != 0)
    {
        return Err(Error::invalid(
            "MP4 tenc build: pattern pair requires version 1",
        ));
    }
    if tenc.default_crypt_byte_block > 0x0F || tenc.default_skip_byte_block > 0x0F {
        return Err(Error::invalid(
            "MP4 tenc build: pattern component exceeds its 4-bit field",
        ));
    }
    if !matches!(tenc.default_per_sample_iv_size, 0 | 8 | 16) {
        return Err(Error::invalid(format!(
            "MP4 tenc build: default_Per_Sample_IV_Size {} not in {{0, 8, 16}}",
            tenc.default_per_sample_iv_size
        )));
    }
    let needs_constant_iv = tenc.default_is_protected == 1 && tenc.default_per_sample_iv_size == 0;
    match (&tenc.default_constant_iv, needs_constant_iv) {
        (Some(iv), true) => {
            if iv.len() != 8 && iv.len() != 16 {
                return Err(Error::invalid(format!(
                    "MP4 tenc build: default_constant_IV length {} not in {{8, 16}}",
                    iv.len()
                )));
            }
        }
        (None, true) => {
            return Err(Error::invalid(
                "MP4 tenc build: isProtected==1 && IV_size==0 requires a constant IV",
            ));
        }
        (Some(_), false) => {
            return Err(Error::invalid(
                "MP4 tenc build: constant IV present but not required (would not round-trip)",
            ));
        }
        (None, false) => {}
    }
    let mut body = Vec::with_capacity(24 + 17);
    body.extend_from_slice(&[tenc.version, 0, 0, 0]); // FullBox version + flags
    body.push(0); // reserved
    if tenc.version == 0 {
        body.push(0); // reserved
    } else {
        body.push((tenc.default_crypt_byte_block << 4) | tenc.default_skip_byte_block);
    }
    body.push(tenc.default_is_protected);
    body.push(tenc.default_per_sample_iv_size);
    body.extend_from_slice(&tenc.default_kid);
    if let Some(iv) = &tenc.default_constant_iv {
        body.push(iv.len() as u8);
        body.extend_from_slice(iv);
    }
    Ok(wrap_full_box(b"tenc", &body))
}

/// Serialise a [`PsshBox`] into a complete `[size]['pssh']` box — the
/// byte-exact inverse of [`parse_pssh`] (§8.1.2). Rejects a v0 record
/// carrying KIDs (the KID array only exists at v1) and count / size
/// values that overflow their 32-bit on-wire fields.
pub fn build_pssh_box(pssh: &PsshBox) -> Result<Vec<u8>> {
    if pssh.version == 0 && !pssh.kids.is_empty() {
        return Err(Error::invalid(
            "MP4 pssh build: KID list requires version >= 1",
        ));
    }
    let kid_count = u32::try_from(pssh.kids.len())
        .map_err(|_| Error::invalid("MP4 pssh build: KID_count exceeds u32"))?;
    let data_size = u32::try_from(pssh.data.len())
        .map_err(|_| Error::invalid("MP4 pssh build: DataSize exceeds u32"))?;
    let mut body = Vec::with_capacity(4 + 16 + 4 + pssh.kids.len() * 16 + 4 + pssh.data.len());
    body.extend_from_slice(&[pssh.version, 0, 0, 0]);
    body.extend_from_slice(&pssh.system_id);
    if pssh.version > 0 {
        body.extend_from_slice(&kid_count.to_be_bytes());
        for kid in &pssh.kids {
            body.extend_from_slice(kid);
        }
    }
    body.extend_from_slice(&data_size.to_be_bytes());
    body.extend_from_slice(&pssh.data);
    Ok(wrap_full_box(b"pssh", &body))
}

/// Serialise a [`SencBox`] into a complete `[size]['senc']` box — the
/// byte-exact inverse of [`parse_senc`] (§7.2.2), emitted at FullBox
/// version 0 with the record's 24-bit `flags` verbatim.
///
/// The on-wire box does not record the IV width (§7.2.3), so the same
/// consistency the parser demands is enforced here: every sample's
/// `initialization_vector` must have one shared length ∈ {0, 8, 16}.
/// When the `UseSubSampleEncryption` bit (`0x000002`) is clear, no
/// sample may carry a subsample map (it would be dropped on re-parse);
/// when set, every subsample count must fit its 16-bit field.
pub fn build_senc_box(senc: &SencBox) -> Result<Vec<u8>> {
    if senc.flags > 0x00FF_FFFF {
        return Err(Error::invalid(
            "MP4 senc build: flags exceed the 24-bit field",
        ));
    }
    let sample_count = u32::try_from(senc.samples.len())
        .map_err(|_| Error::invalid("MP4 senc build: sample_count exceeds u32"))?;
    let iv_size = senc
        .samples
        .first()
        .map(|s| s.initialization_vector.len())
        .unwrap_or(0);
    if !matches!(iv_size, 0 | 8 | 16) {
        return Err(Error::invalid(format!(
            "MP4 senc build: per-sample IV length {iv_size} not in {{0, 8, 16}}"
        )));
    }
    let use_subsamples = senc.uses_subsample_encryption();
    let mut body = Vec::with_capacity(8 + senc.samples.len() * (iv_size + 8));
    body.push(0); // version 0
    body.extend_from_slice(&senc.flags.to_be_bytes()[1..4]);
    body.extend_from_slice(&sample_count.to_be_bytes());
    for (i, sample) in senc.samples.iter().enumerate() {
        if sample.initialization_vector.len() != iv_size {
            return Err(Error::invalid(format!(
                "MP4 senc build: sample {i} IV length {} differs from the shared width {iv_size}",
                sample.initialization_vector.len()
            )));
        }
        body.extend_from_slice(&sample.initialization_vector);
        if use_subsamples {
            let sub_count = u16::try_from(sample.subsamples.len()).map_err(|_| {
                Error::invalid(format!(
                    "MP4 senc build: sample {i} subsample_count exceeds u16"
                ))
            })?;
            body.extend_from_slice(&sub_count.to_be_bytes());
            for sub in &sample.subsamples {
                body.extend_from_slice(&sub.bytes_of_clear_data.to_be_bytes());
                body.extend_from_slice(&sub.bytes_of_protected_data.to_be_bytes());
            }
        } else if !sample.subsamples.is_empty() {
            return Err(Error::invalid(format!(
                "MP4 senc build: sample {i} carries subsamples but UseSubSampleEncryption is clear"
            )));
        }
    }
    Ok(wrap_full_box(b"senc", &body))
}

/// Serialise a [`SeigEntry`] into a raw
/// `CencSampleEncryptionInformationGroupEntry` payload (§6) — the
/// byte-exact inverse of [`parse_seig`]. The returned bytes are the
/// *entry* blob a `grouping_type = *b"seig"` `sgpd` description
/// carries (no box header — sample-group entries are not boxes).
///
/// Rejects records that would not round-trip:
///
/// * a pattern component above its 4-bit field ceiling,
/// * `per_sample_iv_size` outside {0, 8, 16} (§9.1),
/// * a constant IV whose presence disagrees with the §6 rule
///   (`Some` iff `isProtected == 1 && Per_Sample_IV_Size == 0`) or
///   whose length is not 8 / 16 (§9.1).
pub fn build_seig_entry(entry: &SeigEntry) -> Result<Vec<u8>> {
    if entry.crypt_byte_block > 0x0F || entry.skip_byte_block > 0x0F {
        return Err(Error::invalid(
            "CENC seig build: pattern component exceeds its 4-bit field",
        ));
    }
    if !matches!(entry.per_sample_iv_size, 0 | 8 | 16) {
        return Err(Error::invalid(format!(
            "CENC seig build: Per_Sample_IV_Size {} not in {{0, 8, 16}}",
            entry.per_sample_iv_size
        )));
    }
    let needs_constant_iv = entry.is_protected == 1 && entry.per_sample_iv_size == 0;
    match (&entry.constant_iv, needs_constant_iv) {
        (Some(iv), true) => {
            if iv.len() != 8 && iv.len() != 16 {
                return Err(Error::invalid(format!(
                    "CENC seig build: constant_IV length {} not in {{8, 16}}",
                    iv.len()
                )));
            }
        }
        (None, true) => {
            return Err(Error::invalid(
                "CENC seig build: isProtected==1 && IV_size==0 requires a constant IV",
            ));
        }
        (Some(_), false) => {
            return Err(Error::invalid(
                "CENC seig build: constant IV present but not required (would not round-trip)",
            ));
        }
        (None, false) => {}
    }
    let mut out = Vec::with_capacity(20 + 17);
    out.push(0); // reserved
    out.push((entry.crypt_byte_block << 4) | entry.skip_byte_block);
    out.push(entry.is_protected);
    out.push(entry.per_sample_iv_size);
    out.extend_from_slice(&entry.kid);
    if let Some(iv) = &entry.constant_iv {
        out.push(iv.len() as u8);
        out.extend_from_slice(iv);
    }
    Ok(out)
}

/// Serialise a complete `sinf` ProtectionSchemeInfoBox (ISO/IEC
/// 14496-12 §8.12 as profiled by ISO/IEC 23001-7 §4.1) for a muxer
/// wrapping a sample entry into its protected (`encv` / `enca` /
/// `enct` / `encs`) form:
///
/// ```text
///   sinf
///     frma  original_format          (§8.12.2 — the unprotected FourCC)
///     schm  scheme_type + version    (§8.12.5, FullBox v0 flags 0)
///     schi                            (§8.12.6)
///       tenc  track defaults          (23001-7 §8.2)
/// ```
///
/// The `(scheme_type, tenc)` pair is validated through
/// [`CencSchemeDecision::new`] — a §10 scheme must match the `tenc`
/// version it pins and pattern schemes must carry a non-zero pattern
/// pair — and the `tenc` itself through [`build_tenc_box`]'s round-trip
/// rules, so a `sinf` that this crate's own demuxer (or any conforming
/// reader) would reject cannot be emitted. `scheme_version` is the
/// §8.12.5 32-bit version word (0x0001_0000 for every ISO/IEC 23001-7
/// scheme edition to date).
pub fn build_sinf_box(
    original_format: [u8; 4],
    scheme_type: [u8; 4],
    scheme_version: u32,
    tenc: &TencBox,
) -> Result<Vec<u8>> {
    CencSchemeDecision::new(CencScheme::from_fourcc(&scheme_type), tenc.clone())?;
    let tenc_bytes = build_tenc_box(tenc)?;

    let frma = wrap_full_box(b"frma", &original_format);
    let mut schm_body = Vec::with_capacity(12);
    schm_body.extend_from_slice(&[0, 0, 0, 0]); // FullBox version 0 + flags 0
    schm_body.extend_from_slice(&scheme_type);
    schm_body.extend_from_slice(&scheme_version.to_be_bytes());
    let schm = wrap_full_box(b"schm", &schm_body);
    let schi = wrap_full_box(b"schi", &tenc_bytes);

    let mut sinf_body = Vec::with_capacity(frma.len() + schm.len() + schi.len());
    sinf_body.extend_from_slice(&frma);
    sinf_body.extend_from_slice(&schm);
    sinf_body.extend_from_slice(&schi);
    Ok(wrap_full_box(b"sinf", &sinf_body))
}

// ---- Per-sample cipher walker (§9.4–9.6) ------------------------------

/// The kind of byte run a [`CipherStep`] names.
///
/// A sample's payload bytes are partitioned into contiguous runs that
/// either pass through verbatim ([`CipherStepKind::Clear`]) or feed the
/// AES block call selected by the bundled [`CencSchemeDecision`]
/// ([`CipherStepKind::Encrypted`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherStepKind {
    /// Pass the byte run through verbatim — clear data (per
    /// `BytesOfClearData`), an unencrypted pattern skip block per §9.6,
    /// or the trailing partial block left in the clear under
    /// `cbc1` / `cbcs` / pattern truncation rules.
    Clear,
    /// Feed the byte run to the AES block call selected by the
    /// [`CencSchemeDecision::cipher_mode`] (CTR or CBC).
    Encrypted,
}

/// One byte-level step in a sample's cipher plan.
///
/// Together a `Vec<CipherStep>` partitions the sample plaintext range
/// `0..sample_len` into contiguous, non-overlapping runs. A downstream
/// AES layer iterates the steps, applying the cipher to
/// [`CipherStepKind::Encrypted`] runs and copying [`CipherStepKind::Clear`]
/// runs verbatim. This module does not perform the AES block call;
/// `CipherStep` is a static dispatch contract built from container-side
/// bytes only (the [`CencSchemeDecision`] + [`SencSample`] subsample
/// list + sample length). The executing AES layer is
/// [`crate::cenc_cipher::decrypt_steps_in_place`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CipherStep {
    /// Byte offset from the start of the sample payload.
    pub offset: u64,
    /// Run length in bytes. The sum of every step's `len` equals
    /// `sample_len`.
    pub len: u64,
    /// Whether the run is clear or encrypted.
    pub kind: CipherStepKind,
    /// Spec-required IV restart on this step.
    ///
    /// `true` only on the first [`CipherStepKind::Encrypted`] step of
    /// each subsample under the `cbcs` scheme (§9.5.1 — "The `cbcs`
    /// scheme SHALL treat each Subsample as a separate chain of cipher
    /// blocks, starting with the Initialization Vector associated with
    /// the sample"). Always `false` for clear steps and for `cenc` /
    /// `cbc1` / `cens` (those schemes carry one continuous cipher chain
    /// / counter across subsamples in a sample). A consumer using a
    /// constant IV resets its CBC chain state to the constant IV when
    /// it sees this flag.
    pub iv_restart: bool,
}

/// Plan a single sample's cipher partition (§9.4–9.6).
///
/// Inputs:
/// * `decision` — the typed [`CencSchemeDecision`] for the track (or a
///   `seig`-overridden group equivalent built by the caller). Drives
///   cipher-mode + pattern-flag dispatch.
/// * `subsamples` — `Some(_)` for §9.5 subsample encryption (the slice
///   pulled from the matching [`SencSample::subsamples`]); `None` for
///   §9.4 full-sample encryption.
/// * `sample_len` — the plaintext sample length in bytes (the size
///   recorded in `stsz` / `stz2` / `trun.sample_size`).
///
/// Returns the ordered `Vec<CipherStep>` partitioning `0..sample_len`.
///
/// # Spec contract honoured
///
/// * §9.5.1 — "The total length of all `BytesOfClearData` and
///   `BytesOfProtectedData` in a sample SHALL equal the length of the
///   sample". Enforced — mismatched totals return `Err`.
/// * §9.5.1 — "The Subsample encryption entries SHALL NOT include an
///   entry with a zero value in both the `BytesOfClearData` field and
///   in the `BytesOfProtectedData` field." Enforced.
/// * §9.4.3 — "AES-CBC mode requires all encrypted cipher blocks to be
///   16 bytes … leave partial Blocks unencrypted." On §9.4 full-sample
///   `cbc1`, a trailing 1–15 bytes of `sample_len` are emitted as a
///   final [`CipherStepKind::Clear`] step.
/// * §9.4.2 — "AES-CTR mode encryption SHALL use a unique IV per sample
///   and encrypt all bytes in the sample." On §9.4 full-sample `cenc`,
///   the entire `sample_len` is one Encrypted step (the last block may
///   be a partial cipher block — still encrypted).
/// * §9.6 — "the partial pattern SHALL be followed until truncated by
///   the `BytesOfProtectedData` size and any partial `crypt_byte_block`
///   SHALL remain unencrypted." A trailing partial block inside a
///   pattern's encrypted run is emitted as Clear.
/// * §9.5.1 / §9.6 — "The IV SHALL apply to the first encrypted cipher
///   block of each Subsample" under `cbcs`. The first Encrypted step
///   inside each subsample carries `iv_restart = true` for `cbcs` only.
/// * §9.7 — whole-block full-sample encryption is the default for
///   non-video tracks on `cens` / `cbcs` (per §10.3 / §10.4) when the
///   caller passes `subsamples = None`. Trailing 0–15 bytes stay in
///   the clear, IV restart per sample.
///
/// # Errors
///
/// * `Error::invalid` if the subsample size totals do not equal
///   `sample_len`, if a subsample has both clear and protected = 0
///   (§9.5.1 prohibition), or if a subsample's
///   `BytesOfClearData + BytesOfProtectedData` overflows `u64`.
/// * `Error::invalid` if the [`CencSchemeDecision`] reports
///   `IvSupply::None` (track is unprotected at default) — that arm
///   has no plan to make; callers route the sample through their
///   "no cipher" path.
/// * `Error::invalid` if the decision's scheme is
///   [`CencScheme::Unknown`] — the structural rules above are tied to
///   the §10-registered scheme set; a private dialect supplies its
///   own walker.
pub fn plan_sample_cipher(
    decision: &CencSchemeDecision,
    subsamples: Option<&[SubsampleEntry]>,
    sample_len: u64,
) -> Result<Vec<CipherStep>> {
    // Unprotected track default → no plan. Caller routes through "copy
    // verbatim" rather than asking the walker for a NOP partition.
    if decision.iv_supply() == IvSupply::None {
        return Err(Error::invalid(
            "MP4 cenc cipher plan: track default is unprotected (IvSupply::None) — no plan",
        ));
    }
    // Unknown scheme: structural rules below depend on §10 carve-outs.
    let mode = decision.cipher_mode().ok_or_else(|| {
        Error::invalid(
            "MP4 cenc cipher plan: unknown scheme — caller supplies its own dialect walker",
        )
    })?;
    let uses_pattern = decision.uses_pattern_encryption();
    let crypt_blocks = decision.tenc.default_crypt_byte_block as u64;
    let skip_blocks = decision.tenc.default_skip_byte_block as u64;

    let mut plan: Vec<CipherStep> = Vec::new();

    match subsamples {
        None => {
            // §9.4 / §9.7 — full-sample or whole-block full-sample.
            // Pattern schemes on a sample with no subsamples land here
            // for non-video tracks (§10.3 / §10.4 carve-out); §9.7 says
            // every sample is encrypted from offset 0 to the last
            // 16-byte boundary with trailing 0–15 bytes left clear.
            //
            // For §9.4.2 (`cenc` full-sample): CTR encrypts every byte
            // including the partial trailing block. For §9.4.3 (`cbc1`
            // full-sample) and §9.7 (any CBC or CTR whole-block path):
            // trailing partial block stays in clear.
            //
            // The "trailing partial stays clear" rule applies to every
            // CBC path; under pattern schemes the §9.7 rule also makes
            // it apply to CTR (no §9.4.2 carve-out for pattern-CTR on
            // a full sample because pattern by definition lives in §9.6
            // which prohibits a partial encrypted block in the
            // pattern's encrypted run).
            let leave_partial_clear = match mode {
                CipherMode::Cbc => true,
                CipherMode::Ctr => uses_pattern, // §9.7 whole-block when patterned non-video.
            };
            let whole_blocks = (sample_len / 16) * 16;
            let tail = sample_len - whole_blocks;
            if leave_partial_clear && tail != 0 {
                if whole_blocks > 0 {
                    plan.push(CipherStep {
                        offset: 0,
                        len: whole_blocks,
                        kind: CipherStepKind::Encrypted,
                        iv_restart: false,
                    });
                }
                plan.push(CipherStep {
                    offset: whole_blocks,
                    len: tail,
                    kind: CipherStepKind::Clear,
                    iv_restart: false,
                });
            } else if sample_len > 0 {
                // §9.4.2 `cenc` full-sample (or whole-block path with
                // sample_len already a multiple of 16): one Encrypted
                // step covering everything.
                plan.push(CipherStep {
                    offset: 0,
                    len: sample_len,
                    kind: CipherStepKind::Encrypted,
                    iv_restart: false,
                });
            }
            // sample_len == 0: empty plan. The CENC framing for a
            // zero-byte sample is degenerate; the caller has nothing to
            // do either way.
            return Ok(plan);
        }
        Some(subs) => {
            // §9.5 — subsample encryption. Walk subsamples in order;
            // each contributes a clear prefix + a protected suffix.
            let mut cursor: u64 = 0;
            let cbcs_restart_per_sub = matches!(decision.scheme, CencScheme::Cbcs);
            for (i, s) in subs.iter().enumerate() {
                let clear = s.bytes_of_clear_data as u64;
                let protected = s.bytes_of_protected_data as u64;
                // §9.5.1 prohibition: clear == 0 && protected == 0 is
                // illegal (subsample entries SHALL NOT include a
                // both-zero entry).
                if clear == 0 && protected == 0 {
                    return Err(Error::invalid(format!(
                        "MP4 cenc cipher plan: subsample {i} is both-zero (§9.5.1 prohibition)"
                    )));
                }
                let row_len = clear.checked_add(protected).ok_or_else(|| {
                    Error::invalid(format!(
                        "MP4 cenc cipher plan: subsample {i} clear+protected overflow"
                    ))
                })?;
                let row_end = cursor.checked_add(row_len).ok_or_else(|| {
                    Error::invalid("MP4 cenc cipher plan: subsample run-length overflow")
                })?;
                if row_end > sample_len {
                    return Err(Error::invalid(format!(
                        "MP4 cenc cipher plan: subsample {i} ends at {row_end} past sample_len {sample_len}"
                    )));
                }
                if clear > 0 {
                    plan.push(CipherStep {
                        offset: cursor,
                        len: clear,
                        kind: CipherStepKind::Clear,
                        iv_restart: false,
                    });
                    cursor += clear;
                }
                if protected > 0 {
                    if uses_pattern {
                        // §9.6 pattern walker. Repeat
                        // (crypt_byte_block*16 encrypted, skip_byte_block*16 clear)
                        // until the protected range is exhausted; a
                        // trailing partial encrypted block stays clear.
                        plan_pattern_run(
                            &mut plan,
                            cursor,
                            protected,
                            crypt_blocks,
                            skip_blocks,
                            cbcs_restart_per_sub,
                        );
                    } else {
                        // §9.5.1 non-pattern: one encrypted span. CTR
                        // counter / CBC chain are logically continuous
                        // across subsamples in a sample.
                        plan.push(CipherStep {
                            offset: cursor,
                            len: protected,
                            kind: CipherStepKind::Encrypted,
                            iv_restart: false,
                        });
                    }
                    cursor += protected;
                }
            }
            if cursor != sample_len {
                return Err(Error::invalid(format!(
                    "MP4 cenc cipher plan: subsample total {cursor} != sample_len {sample_len} (§9.5.1)"
                )));
            }
        }
    }
    Ok(plan)
}

/// Emit pattern-encryption steps (§9.6) for one subsample's protected
/// range starting at `offset` and `protected_len` bytes long.
///
/// Pushes alternating `crypt_byte_block * 16`-byte Encrypted runs and
/// `skip_byte_block * 16`-byte Clear runs into `plan`. A trailing
/// partial 16-byte block at the end of the protected range stays in
/// the clear per §9.6 ("any partial `crypt_byte_block` SHALL remain
/// unencrypted").
///
/// `cbcs_restart_per_sub` controls the `iv_restart` flag on the **first**
/// Encrypted step emitted for this subsample — `true` only for the
/// `cbcs` scheme (§9.5.1 per-subsample IV restart).
///
/// Special case `crypt_byte_block == 0`: the pattern degenerates to
/// "skip everything" — the entire protected range is emitted as Clear.
/// Special case `skip_byte_block == 0` with `crypt_byte_block > 0`:
/// the pattern degenerates to "encrypt everything in 16-byte blocks
/// with trailing partial clear" — equivalent to §9.7.
fn plan_pattern_run(
    plan: &mut Vec<CipherStep>,
    offset: u64,
    protected_len: u64,
    crypt_blocks: u64,
    skip_blocks: u64,
    cbcs_restart_per_sub: bool,
) {
    if protected_len == 0 {
        return;
    }
    if crypt_blocks == 0 {
        // Pattern says "encrypt nothing" — entire protected range is
        // clear. (Pathological; the validation in
        // CencSchemeDecision::new rejects (0, 0) for pattern schemes,
        // but a sample-group seig override with crypt=0, skip>0 is
        // theoretically expressible and §9.6 says the partial pattern
        // is followed until truncated.)
        plan.push(CipherStep {
            offset,
            len: protected_len,
            kind: CipherStepKind::Clear,
            iv_restart: false,
        });
        return;
    }
    let crypt_bytes = crypt_blocks * 16;
    let skip_bytes = skip_blocks * 16;
    let pattern_bytes = crypt_bytes + skip_bytes;
    let mut remaining = protected_len;
    let mut cursor = offset;
    let mut first_encrypted_in_sub = true;
    while remaining > 0 {
        // Encrypted portion of the pattern.
        let take_full = crypt_bytes.min(remaining - (remaining % 16));
        if take_full > 0 {
            plan.push(CipherStep {
                offset: cursor,
                len: take_full,
                kind: CipherStepKind::Encrypted,
                iv_restart: cbcs_restart_per_sub && first_encrypted_in_sub,
            });
            cursor += take_full;
            remaining -= take_full;
            first_encrypted_in_sub = false;
        }
        // If the encrypted-block side did not consume its full quota,
        // we have a trailing partial 16-byte block (or zero bytes left).
        // §9.6: partial crypt block stays unencrypted. Promote the
        // remainder to Clear and stop — the pattern is terminated.
        if take_full < crypt_bytes {
            if remaining > 0 {
                plan.push(CipherStep {
                    offset: cursor,
                    len: remaining,
                    kind: CipherStepKind::Clear,
                    iv_restart: false,
                });
                cursor += remaining;
            }
            break;
        }
        // Skipped portion of the pattern.
        if skip_bytes > 0 && remaining > 0 {
            let take_skip = skip_bytes.min(remaining);
            plan.push(CipherStep {
                offset: cursor,
                len: take_skip,
                kind: CipherStepKind::Clear,
                iv_restart: false,
            });
            cursor += take_skip;
            remaining -= take_skip;
        }
        // Guard against an infinite loop in the (unreachable, validated)
        // pattern_bytes == 0 case.
        if pattern_bytes == 0 {
            break;
        }
    }
    // `cursor` and `offset + protected_len` are equal by construction
    // when remaining hits zero; the unused `cursor` value is dropped.
    let _ = cursor;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build_tenc_box / build_pssh_box / build_senc_box ------------

    fn box_body(bytes: &[u8], fourcc: &[u8; 4]) -> Vec<u8> {
        assert_eq!(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
            bytes.len(),
            "box size field must match total length"
        );
        assert_eq!(&bytes[4..8], fourcc);
        bytes[8..].to_vec()
    }

    #[test]
    fn build_tenc_v0_round_trips() {
        let rec = TencBox {
            version: 0,
            default_is_protected: 1,
            default_per_sample_iv_size: 16,
            default_kid: [0x42; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        };
        let bytes = build_tenc_box(&rec).unwrap();
        assert_eq!(parse_tenc(&box_body(&bytes, b"tenc")).unwrap(), rec);
    }

    #[test]
    fn build_tenc_v1_pattern_constant_iv_round_trips() {
        let rec = TencBox {
            version: 1,
            default_is_protected: 1,
            default_per_sample_iv_size: 0,
            default_kid: [0x0F; 16],
            default_crypt_byte_block: 1,
            default_skip_byte_block: 9,
            default_constant_iv: Some(vec![0xCC; 16]),
        };
        let bytes = build_tenc_box(&rec).unwrap();
        assert_eq!(parse_tenc(&box_body(&bytes, b"tenc")).unwrap(), rec);
    }

    #[test]
    fn build_tenc_rejects_non_round_trippable_records() {
        let base = TencBox {
            version: 0,
            default_is_protected: 1,
            default_per_sample_iv_size: 8,
            default_kid: [0; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        };
        // v0 with a pattern pair.
        let mut r = base.clone();
        r.default_crypt_byte_block = 1;
        assert!(build_tenc_box(&r).is_err());
        // Undefined version.
        let mut r = base.clone();
        r.version = 2;
        assert!(build_tenc_box(&r).is_err());
        // IV size outside {0, 8, 16}.
        let mut r = base.clone();
        r.default_per_sample_iv_size = 4;
        assert!(build_tenc_box(&r).is_err());
        // Constant IV required but absent.
        let mut r = base.clone();
        r.default_per_sample_iv_size = 0;
        assert!(build_tenc_box(&r).is_err());
        // Constant IV present but not required.
        let mut r = base.clone();
        r.default_constant_iv = Some(vec![0xAA; 8]);
        assert!(build_tenc_box(&r).is_err());
        // Constant IV with a bad length.
        let mut r = base;
        r.default_per_sample_iv_size = 0;
        r.default_constant_iv = Some(vec![0xAA; 12]);
        assert!(build_tenc_box(&r).is_err());
    }

    #[test]
    fn build_seig_per_sample_iv_round_trips() {
        let rec = SeigEntry {
            crypt_byte_block: 0,
            skip_byte_block: 0,
            is_protected: 1,
            per_sample_iv_size: 8,
            kid: [0x7E; 16],
            constant_iv: None,
        };
        let bytes = build_seig_entry(&rec).unwrap();
        assert_eq!(bytes.len(), 20, "fixed-prefix-only entry is 20 bytes");
        assert_eq!(parse_seig(&bytes).unwrap(), rec);
    }

    #[test]
    fn build_seig_pattern_constant_iv_round_trips() {
        let rec = SeigEntry {
            crypt_byte_block: 1,
            skip_byte_block: 9,
            is_protected: 1,
            per_sample_iv_size: 0,
            kid: [0xC4; 16],
            constant_iv: Some(vec![0x5D; 16]),
        };
        let bytes = build_seig_entry(&rec).unwrap();
        assert_eq!(bytes[1], 0x19, "crypt:4|skip:4 packing");
        assert_eq!(parse_seig(&bytes).unwrap(), rec);
    }

    #[test]
    fn build_seig_unprotected_group_round_trips() {
        // §6: isProtected = 0 overrides the track default to "clear for
        // this group" — no constant IV even at IV size 0.
        let rec = SeigEntry {
            crypt_byte_block: 0,
            skip_byte_block: 0,
            is_protected: 0,
            per_sample_iv_size: 0,
            kid: [0u8; 16],
            constant_iv: None,
        };
        let bytes = build_seig_entry(&rec).unwrap();
        assert_eq!(parse_seig(&bytes).unwrap(), rec);
    }

    #[test]
    fn build_seig_rejects_non_round_trippable_records() {
        let base = SeigEntry {
            crypt_byte_block: 0,
            skip_byte_block: 0,
            is_protected: 1,
            per_sample_iv_size: 8,
            kid: [0x11; 16],
            constant_iv: None,
        };
        // Pattern nibble overflow.
        let mut r = base.clone();
        r.crypt_byte_block = 16;
        assert!(build_seig_entry(&r).is_err());
        // IV size outside {0, 8, 16}.
        let mut r = base.clone();
        r.per_sample_iv_size = 4;
        assert!(build_seig_entry(&r).is_err());
        // Constant IV required but absent.
        let mut r = base.clone();
        r.per_sample_iv_size = 0;
        assert!(build_seig_entry(&r).is_err());
        // Constant IV present but not required.
        let mut r = base.clone();
        r.constant_iv = Some(vec![0xAA; 8]);
        assert!(build_seig_entry(&r).is_err());
        // Constant IV with a bad length.
        let mut r = base;
        r.per_sample_iv_size = 0;
        r.constant_iv = Some(vec![0xAA; 12]);
        assert!(build_seig_entry(&r).is_err());
    }

    #[test]
    fn build_pssh_v0_and_v1_round_trip() {
        let v0 = PsshBox {
            version: 0,
            system_id: [0x11; 16],
            kids: Vec::new(),
            data: vec![1, 2, 3, 4, 5],
        };
        let bytes = build_pssh_box(&v0).unwrap();
        assert_eq!(parse_pssh(&box_body(&bytes, b"pssh")).unwrap(), v0);

        let v1 = PsshBox {
            version: 1,
            system_id: [0x22; 16],
            kids: vec![[0xA0; 16], [0xB1; 16]],
            data: Vec::new(),
        };
        let bytes = build_pssh_box(&v1).unwrap();
        assert_eq!(parse_pssh(&box_body(&bytes, b"pssh")).unwrap(), v1);
    }

    #[test]
    fn build_pssh_rejects_v0_with_kids() {
        let rec = PsshBox {
            version: 0,
            system_id: [0; 16],
            kids: vec![[1; 16]],
            data: Vec::new(),
        };
        assert!(build_pssh_box(&rec).is_err());
    }

    #[test]
    fn build_senc_plain_iv_round_trips() {
        let rec = SencBox {
            flags: 0,
            samples: vec![
                SencSample {
                    initialization_vector: vec![1; 8],
                    subsamples: Vec::new(),
                },
                SencSample {
                    initialization_vector: vec![2; 8],
                    subsamples: Vec::new(),
                },
            ],
        };
        let bytes = build_senc_box(&rec).unwrap();
        assert_eq!(parse_senc(&box_body(&bytes, b"senc"), 8).unwrap(), rec);
    }

    #[test]
    fn build_senc_subsample_round_trips() {
        let rec = SencBox {
            flags: 0x0000_0002,
            samples: vec![SencSample {
                initialization_vector: vec![7; 16],
                subsamples: vec![
                    SubsampleEntry {
                        bytes_of_clear_data: 13,
                        bytes_of_protected_data: 96,
                    },
                    SubsampleEntry {
                        bytes_of_clear_data: 4,
                        bytes_of_protected_data: 32,
                    },
                ],
            }],
        };
        let bytes = build_senc_box(&rec).unwrap();
        assert_eq!(parse_senc(&box_body(&bytes, b"senc"), 16).unwrap(), rec);
    }

    #[test]
    fn build_senc_constant_iv_scheme_round_trips() {
        // Zero-length IVs (constant-IV scheme) with subsample maps —
        // the cbcs shape.
        let rec = SencBox {
            flags: 0x0000_0002,
            samples: vec![SencSample {
                initialization_vector: Vec::new(),
                subsamples: vec![SubsampleEntry {
                    bytes_of_clear_data: 5,
                    bytes_of_protected_data: 160,
                }],
            }],
        };
        let bytes = build_senc_box(&rec).unwrap();
        assert_eq!(parse_senc(&box_body(&bytes, b"senc"), 0).unwrap(), rec);
    }

    #[test]
    fn build_senc_rejects_inconsistent_records() {
        // Mixed IV widths.
        let rec = SencBox {
            flags: 0,
            samples: vec![
                SencSample {
                    initialization_vector: vec![1; 8],
                    subsamples: Vec::new(),
                },
                SencSample {
                    initialization_vector: vec![2; 16],
                    subsamples: Vec::new(),
                },
            ],
        };
        assert!(build_senc_box(&rec).is_err());
        // Subsamples present with the flag clear.
        let rec = SencBox {
            flags: 0,
            samples: vec![SencSample {
                initialization_vector: vec![1; 8],
                subsamples: vec![SubsampleEntry {
                    bytes_of_clear_data: 1,
                    bytes_of_protected_data: 2,
                }],
            }],
        };
        assert!(build_senc_box(&rec).is_err());
        // Flags exceeding the 24-bit field.
        let rec = SencBox {
            flags: 0x0100_0000,
            samples: Vec::new(),
        };
        assert!(build_senc_box(&rec).is_err());
        // Bad shared IV width.
        let rec = SencBox {
            flags: 0,
            samples: vec![SencSample {
                initialization_vector: vec![1; 4],
                subsamples: Vec::new(),
            }],
        };
        assert!(build_senc_box(&rec).is_err());
    }

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

    // ---- CencScheme typed router -----------------------------------

    #[test]
    fn scheme_fourcc_round_trip() {
        for fc in [b"cenc", b"cbc1", b"cens", b"cbcs"] {
            let s = CencScheme::from_fourcc(fc);
            assert_eq!(&s.fourcc(), fc, "round trip {fc:?}");
        }
        // Unknown FourCC round-trips verbatim.
        let priv_fc = b"priv";
        let s = CencScheme::from_fourcc(priv_fc);
        assert!(matches!(s, CencScheme::Unknown(b) if &b == priv_fc));
        assert_eq!(s.fourcc(), *priv_fc);
    }

    #[test]
    fn scheme_cipher_mode_routes_ctr_vs_cbc() {
        assert_eq!(
            CencScheme::Cenc.cipher_mode(),
            Some(CipherMode::Ctr),
            "cenc → CTR (§10.1)"
        );
        assert_eq!(
            CencScheme::Cens.cipher_mode(),
            Some(CipherMode::Ctr),
            "cens → CTR (§10.3)"
        );
        assert_eq!(
            CencScheme::Cbc1.cipher_mode(),
            Some(CipherMode::Cbc),
            "cbc1 → CBC (§10.2)"
        );
        assert_eq!(
            CencScheme::Cbcs.cipher_mode(),
            Some(CipherMode::Cbc),
            "cbcs → CBC (§10.4)"
        );
        // Unknown scheme cannot be routed.
        assert_eq!(CencScheme::Unknown(*b"priv").cipher_mode(), None);
    }

    #[test]
    fn scheme_pattern_flag() {
        assert!(!CencScheme::Cenc.uses_pattern_encryption());
        assert!(!CencScheme::Cbc1.uses_pattern_encryption());
        assert!(CencScheme::Cens.uses_pattern_encryption());
        assert!(CencScheme::Cbcs.uses_pattern_encryption());
        assert!(!CencScheme::Unknown(*b"priv").uses_pattern_encryption());
    }

    #[test]
    fn scheme_required_tenc_version() {
        // §10.1 / §10.2 pin tenc v0 (no pattern fields needed).
        assert_eq!(CencScheme::Cenc.required_tenc_version(), Some(0));
        assert_eq!(CencScheme::Cbc1.required_tenc_version(), Some(0));
        // §10.3 / §10.4 pin tenc v1 (carry the pattern pair).
        assert_eq!(CencScheme::Cens.required_tenc_version(), Some(1));
        assert_eq!(CencScheme::Cbcs.required_tenc_version(), Some(1));
        // Unknown: unconstrained.
        assert_eq!(CencScheme::Unknown(*b"priv").required_tenc_version(), None);
    }

    fn cenc_tenc_v0_per_sample() -> TencBox {
        TencBox {
            version: 0,
            default_is_protected: 1,
            default_per_sample_iv_size: 16,
            default_kid: [0xAA; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        }
    }

    fn cbcs_tenc_v1_constant_iv() -> TencBox {
        TencBox {
            version: 1,
            default_is_protected: 1,
            default_per_sample_iv_size: 0,
            default_kid: [0xBB; 16],
            default_crypt_byte_block: 1,
            default_skip_byte_block: 9,
            default_constant_iv: Some(vec![0xCA; 16]),
        }
    }

    #[test]
    fn decision_routes_cenc_to_ctr_per_sample() {
        // Typical cenc track: AES-CTR, per-sample 16-byte IV, no pattern.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample())
            .expect("cenc decision");
        assert_eq!(d.cipher_mode(), Some(CipherMode::Ctr));
        assert!(!d.uses_pattern_encryption());
        assert_eq!(d.iv_supply(), IvSupply::PerSample { size: 16 });
    }

    #[test]
    fn decision_routes_cbcs_to_cbc_constant_iv_pattern() {
        // Typical cbcs track: AES-CBC, constant IV, 1:9 protection
        // pattern.
        let d = CencSchemeDecision::new(CencScheme::Cbcs, cbcs_tenc_v1_constant_iv())
            .expect("cbcs decision");
        assert_eq!(d.cipher_mode(), Some(CipherMode::Cbc));
        assert!(d.uses_pattern_encryption());
        assert_eq!(d.iv_supply(), IvSupply::Constant);
        assert_eq!(d.tenc.default_constant_iv.as_deref(), Some(&[0xCA; 16][..]));
    }

    #[test]
    fn decision_rejects_version_mismatch() {
        // cenc requires tenc v0 but caller supplied a v1 tenc.
        let bad = TencBox {
            version: 1,
            ..cenc_tenc_v0_per_sample()
        };
        let err = CencSchemeDecision::new(CencScheme::Cenc, bad).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("requires tenc version 0"), "msg={msg}");
    }

    #[test]
    fn decision_rejects_pattern_scheme_with_zero_pattern() {
        // cbcs scheme but tenc has crypt=skip=0 → not actually patterned.
        let bad = TencBox {
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            ..cbcs_tenc_v1_constant_iv()
        };
        let err = CencSchemeDecision::new(CencScheme::Cbcs, bad).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("pattern-encryption"), "msg={msg}");
    }

    #[test]
    fn decision_unprotected_track_is_iv_none() {
        // isProtected=0 — entire track is plaintext at default.
        let plain = TencBox {
            version: 0,
            default_is_protected: 0,
            default_per_sample_iv_size: 0,
            default_kid: [0u8; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        };
        let d = CencSchemeDecision::new(CencScheme::Cenc, plain).expect("plaintext decision");
        assert_eq!(d.iv_supply(), IvSupply::None);
    }

    #[test]
    fn decision_unknown_scheme_routes_unconstrained() {
        // Private DRM dialect with a custom FourCC: structural router
        // accepts the bundle but reports None for cipher mode (since the
        // dialect is not §10-registered).
        let d =
            CencSchemeDecision::new(CencScheme::from_fourcc(b"priv"), cenc_tenc_v0_per_sample())
                .expect("unknown scheme decision");
        assert!(matches!(d.scheme, CencScheme::Unknown(b) if &b == b"priv"));
        assert_eq!(d.cipher_mode(), None);
        assert!(!d.uses_pattern_encryption());
    }

    // ---- seig sample-group entry parser ----------------------------

    #[test]
    fn seig_per_sample_iv_round_trip() {
        // reserved=0, crypt=0, skip=0, isProtected=1, IV_size=16,
        // KID = 0x11..0x20.
        let mut body: Vec<u8> = vec![
            0,  // reserved
            0,  // crypt|skip packed nibbles, both 0
            1,  // isProtected
            16, // Per_Sample_IV_Size
        ];
        let kid: [u8; 16] = [
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E,
            0x1F, 0x20,
        ];
        body.extend_from_slice(&kid);
        let s = parse_seig(&body).expect("seig per-sample IV");
        assert_eq!(s.is_protected, 1);
        assert_eq!(s.per_sample_iv_size, 16);
        assert_eq!(s.kid, kid);
        assert_eq!(s.crypt_byte_block, 0);
        assert_eq!(s.skip_byte_block, 0);
        assert!(s.constant_iv.is_none());
        assert_eq!(s.iv_supply(), IvSupply::PerSample { size: 16 });
        assert!(!s.uses_pattern_encryption());
    }

    #[test]
    fn seig_pattern_constant_iv_round_trip() {
        // Typical cbcs-style group override: pattern 1:9, constant IV
        // size 16.
        let mut body: Vec<u8> = vec![
            0,            // reserved
            (1 << 4) | 9, // crypt=1 | skip=9
            1,            // isProtected
            0,            // Per_Sample_IV_Size = 0 ⇒ constant IV in use
        ];
        body.extend_from_slice(&[0x77; 16]); // KID
        body.push(16); // constant_IV_size
        body.extend_from_slice(&[0xDD; 16]);
        let s = parse_seig(&body).expect("seig pattern + constant IV");
        assert_eq!(s.crypt_byte_block, 1);
        assert_eq!(s.skip_byte_block, 9);
        assert_eq!(s.per_sample_iv_size, 0);
        assert_eq!(s.constant_iv.as_deref(), Some(&[0xDD; 16][..]));
        assert_eq!(s.iv_supply(), IvSupply::Constant);
        assert!(s.uses_pattern_encryption());
    }

    #[test]
    fn seig_unprotected_group_override() {
        // isProtected=0 — override the track default for these samples
        // to plaintext. IV size must be 0 in this case (§6 + §9.1).
        let mut body: Vec<u8> = vec![
            0, // reserved
            0, // crypt|skip
            0, // isProtected = 0
            0, // IV_size = 0
        ];
        body.extend_from_slice(&[0u8; 16]); // KID (zeroed per §9.1 recommendation)
        let s = parse_seig(&body).expect("seig unprotected group");
        assert_eq!(s.is_protected, 0);
        assert_eq!(s.iv_supply(), IvSupply::None);
    }

    #[test]
    fn seig_ignores_trailing_bytes_per_spec_note() {
        // §6 closing paragraph: "clients SHALL ignore additional bytes
        // after the fields defined". Extra trailing bytes don't fail
        // the parse.
        let mut body: Vec<u8> = vec![0, 0, 1, 8];
        body.extend_from_slice(&[0x33; 16]);
        // Trailing bytes from a hypothetical future extension.
        body.extend_from_slice(&[0xFE, 0xED, 0xBE, 0xEF]);
        let s = parse_seig(&body).expect("seig with trailing bytes");
        assert_eq!(s.per_sample_iv_size, 8);
    }

    #[test]
    fn seig_rejects_unsupported_iv_size() {
        let mut body: Vec<u8> = vec![0, 0, 1, 4]; // IV_size = 4 — not in {0, 8, 16}
        body.extend_from_slice(&[0u8; 16]);
        assert!(parse_seig(&body).is_err());
    }

    #[test]
    fn seig_rejects_unsupported_constant_iv_size() {
        let mut body: Vec<u8> = vec![0, 0, 1, 0];
        body.extend_from_slice(&[0u8; 16]);
        body.push(4); // constant_IV_size = 4 — not 8 or 16
        body.extend_from_slice(&[0u8; 4]);
        assert!(parse_seig(&body).is_err());
    }

    #[test]
    fn seig_rejects_short_payload() {
        // Fixed prefix is 20 bytes; 19 must fail.
        assert!(parse_seig(&[0u8; 19]).is_err());
    }

    #[test]
    fn seig_rejects_truncated_constant_iv() {
        let mut body: Vec<u8> = vec![0, 0, 1, 0];
        body.extend_from_slice(&[0u8; 16]);
        body.push(16);
        body.extend_from_slice(&[0u8; 15]); // one byte short
        assert!(parse_seig(&body).is_err());
    }

    // ---- cipher walker (§9.4–9.6) ----------------------------------

    fn cbc1_tenc_v0_per_sample() -> TencBox {
        TencBox {
            version: 0,
            default_is_protected: 1,
            default_per_sample_iv_size: 16,
            default_kid: [0x12; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        }
    }

    fn cens_tenc_v1_pattern_1_9() -> TencBox {
        TencBox {
            version: 1,
            default_is_protected: 1,
            default_per_sample_iv_size: 8,
            default_kid: [0x44; 16],
            default_crypt_byte_block: 1,
            default_skip_byte_block: 9,
            default_constant_iv: None,
        }
    }

    fn sub(clear: u16, protected: u32) -> SubsampleEntry {
        SubsampleEntry {
            bytes_of_clear_data: clear,
            bytes_of_protected_data: protected,
        }
    }

    #[test]
    fn plan_cenc_full_sample_ctr_one_encrypted_span() {
        // §9.4.2 — CTR encrypts every byte in the sample including the
        // trailing partial block.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let plan = plan_sample_cipher(&d, None, 137).expect("full-sample cenc");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].offset, 0);
        assert_eq!(plan[0].len, 137);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert!(!plan[0].iv_restart);
    }

    #[test]
    fn plan_cbc1_full_sample_leaves_trailing_partial_clear() {
        // §9.4.3 — CBC leaves partial trailing block unencrypted.
        // sample_len = 137 = 8*16 + 9 → 128 encrypted + 9 clear.
        let d = CencSchemeDecision::new(CencScheme::Cbc1, cbc1_tenc_v0_per_sample()).unwrap();
        let plan = plan_sample_cipher(&d, None, 137).expect("full-sample cbc1");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[0].len, 128);
        assert_eq!(plan[1].kind, CipherStepKind::Clear);
        assert_eq!(plan[1].offset, 128);
        assert_eq!(plan[1].len, 9);
    }

    #[test]
    fn plan_cbc1_full_sample_aligned_no_clear_tail() {
        // sample_len multiple of 16 → no trailing clear step.
        let d = CencSchemeDecision::new(CencScheme::Cbc1, cbc1_tenc_v0_per_sample()).unwrap();
        let plan = plan_sample_cipher(&d, None, 64).expect("aligned cbc1");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[0].len, 64);
    }

    #[test]
    fn plan_cenc_subsamples_clear_then_encrypted() {
        // §9.5 cenc with two NAL-like subsamples.
        // sub0: 5 clear + 32 protected, sub1: 3 clear + 16 protected.
        // Total = 56.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(5, 32), sub(3, 16)];
        let plan = plan_sample_cipher(&d, Some(&subs), 56).expect("subsamples cenc");
        assert_eq!(plan.len(), 4);
        assert_eq!(
            plan[0],
            CipherStep {
                offset: 0,
                len: 5,
                kind: CipherStepKind::Clear,
                iv_restart: false
            }
        );
        assert_eq!(
            plan[1],
            CipherStep {
                offset: 5,
                len: 32,
                kind: CipherStepKind::Encrypted,
                iv_restart: false
            }
        );
        assert_eq!(
            plan[2],
            CipherStep {
                offset: 37,
                len: 3,
                kind: CipherStepKind::Clear,
                iv_restart: false
            }
        );
        assert_eq!(
            plan[3],
            CipherStep {
                offset: 40,
                len: 16,
                kind: CipherStepKind::Encrypted,
                iv_restart: false
            }
        );
    }

    #[test]
    fn plan_cbc1_subsamples_no_iv_restart() {
        // §9.5.1 — cbc1 forms one continuous cipher chain per sample
        // across subsamples; iv_restart stays false on every step.
        let d = CencSchemeDecision::new(CencScheme::Cbc1, cbc1_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(0, 32), sub(0, 16)];
        let plan = plan_sample_cipher(&d, Some(&subs), 48).expect("subsamples cbc1");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[1].kind, CipherStepKind::Encrypted);
        assert!(plan.iter().all(|s| !s.iv_restart));
    }

    #[test]
    fn plan_cbcs_subsamples_iv_restart_per_subsample() {
        // §9.5.1 — cbcs restarts the cipher chain at the start of each
        // subsample's protected range. iv_restart=true on the first
        // Encrypted of each subsample.
        let d = CencSchemeDecision::new(CencScheme::Cbcs, cbcs_tenc_v1_constant_iv()).unwrap();
        // 1:9 pattern means crypt=16, skip=144 per repetition (160 total).
        // sub0: 8 clear + 160 protected = full one-repetition pattern.
        // sub1: 8 clear + 160 protected = another full one-repetition pattern.
        let subs = vec![sub(8, 160), sub(8, 160)];
        let plan = plan_sample_cipher(&d, Some(&subs), 336).expect("subsamples cbcs");
        // Per sub: Clear(8) + Encrypted(16) + Clear(144) = 3 steps.
        // Two subs → 6 steps total.
        assert_eq!(plan.len(), 6);
        // sub0 first encrypted: iv_restart = true.
        assert_eq!(plan[1].kind, CipherStepKind::Encrypted);
        assert!(plan[1].iv_restart);
        // sub1 first encrypted: iv_restart = true.
        assert_eq!(plan[4].kind, CipherStepKind::Encrypted);
        assert!(plan[4].iv_restart);
    }

    #[test]
    fn plan_cens_subsamples_no_iv_restart_under_ctr() {
        // §9.5.1 — under cens, CTR counter is continuous across
        // subsamples; iv_restart is false even for the first encrypted
        // of a subsample.
        let d = CencSchemeDecision::new(CencScheme::Cens, cens_tenc_v1_pattern_1_9()).unwrap();
        let subs = vec![sub(8, 160), sub(8, 160)];
        let plan = plan_sample_cipher(&d, Some(&subs), 336).expect("subsamples cens");
        assert!(plan.iter().all(|s| !s.iv_restart));
    }

    #[test]
    fn plan_pattern_1_9_one_repetition_exact() {
        // Pattern walker: crypt=1, skip=9 (1:9 = cbcs canonical).
        // protected_len = 16 + 144 = 160 → one full pattern, no
        // trailing partial.
        let d = CencSchemeDecision::new(CencScheme::Cbcs, cbcs_tenc_v1_constant_iv()).unwrap();
        let subs = vec![sub(0, 160)];
        let plan = plan_sample_cipher(&d, Some(&subs), 160).expect("one-rep pattern");
        // Encrypted(16) + Clear(144).
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            CipherStep {
                offset: 0,
                len: 16,
                kind: CipherStepKind::Encrypted,
                iv_restart: true
            }
        );
        assert_eq!(
            plan[1],
            CipherStep {
                offset: 16,
                len: 144,
                kind: CipherStepKind::Clear,
                iv_restart: false
            }
        );
    }

    #[test]
    fn plan_pattern_trailing_partial_encrypted_block_goes_clear() {
        // §9.6 — partial crypt_byte_block at the end of the protected
        // range stays in the clear. Use crypt=2, skip=0 (degenerate
        // "encrypt every block, partial trailing stays clear").
        let pattern_tenc = TencBox {
            version: 1,
            default_is_protected: 1,
            default_per_sample_iv_size: 16,
            default_kid: [0x77; 16],
            default_crypt_byte_block: 2,
            default_skip_byte_block: 1,
            default_constant_iv: None,
        };
        let d = CencSchemeDecision::new(CencScheme::Cens, pattern_tenc).unwrap();
        // protected_len = 2*16 + 5 = 37. Pattern: encrypt 32, then we
        // want to encrypt up to another 32 — but only 5 bytes remain,
        // which is a partial crypt block → those 5 go Clear.
        let subs = vec![sub(0, 37)];
        let plan = plan_sample_cipher(&d, Some(&subs), 37).expect("partial crypt → clear");
        // Steps: Encrypted(32) + Clear(5).
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[0].len, 32);
        assert_eq!(plan[1].kind, CipherStepKind::Clear);
        assert_eq!(plan[1].offset, 32);
        assert_eq!(plan[1].len, 5);
    }

    #[test]
    fn plan_pattern_truncated_mid_skip_run() {
        // Pattern: crypt=1, skip=9. protected_len = 16 + 50 = 66.
        // After encrypted(16), 50 bytes remain. Skip would take 144 →
        // truncated to 50 Clear bytes.
        let d = CencSchemeDecision::new(CencScheme::Cbcs, cbcs_tenc_v1_constant_iv()).unwrap();
        let subs = vec![sub(0, 66)];
        let plan = plan_sample_cipher(&d, Some(&subs), 66).expect("truncated skip run");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[0].len, 16);
        assert_eq!(plan[1].kind, CipherStepKind::Clear);
        assert_eq!(plan[1].len, 50);
    }

    #[test]
    fn plan_rejects_subsample_totals_short_of_sample_len() {
        // §9.5.1 — clear+protected MUST equal sample_len.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(5, 32)];
        let err = plan_sample_cipher(&d, Some(&subs), 100).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("subsample total") || msg.contains("!="),
            "msg={msg}"
        );
    }

    #[test]
    fn plan_rejects_subsample_totals_over_sample_len() {
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(50, 100)];
        let err = plan_sample_cipher(&d, Some(&subs), 100).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("past sample_len"), "msg={msg}");
    }

    #[test]
    fn plan_rejects_both_zero_subsample() {
        // §9.5.1 prohibition: an entry with both clear=0 and protected=0
        // is invalid.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(10, 10), sub(0, 0), sub(5, 5)];
        let err = plan_sample_cipher(&d, Some(&subs), 30).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("both-zero"), "msg={msg}");
    }

    #[test]
    fn plan_rejects_unprotected_track_default() {
        // IvSupply::None — caller should not have called the walker.
        let plain = TencBox {
            version: 0,
            default_is_protected: 0,
            default_per_sample_iv_size: 0,
            default_kid: [0u8; 16],
            default_crypt_byte_block: 0,
            default_skip_byte_block: 0,
            default_constant_iv: None,
        };
        let d = CencSchemeDecision::new(CencScheme::Cenc, plain).unwrap();
        let err = plan_sample_cipher(&d, None, 100).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unprotected"), "msg={msg}");
    }

    #[test]
    fn plan_rejects_unknown_scheme() {
        let d =
            CencSchemeDecision::new(CencScheme::from_fourcc(b"priv"), cenc_tenc_v0_per_sample())
                .unwrap();
        let err = plan_sample_cipher(&d, None, 100).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown scheme"), "msg={msg}");
    }

    #[test]
    fn plan_empty_sample_yields_empty_plan() {
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let plan = plan_sample_cipher(&d, None, 0).expect("empty sample");
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_subsample_with_only_clear_emits_clear_only() {
        // A subsample with clear>0, protected=0 is legal (a NAL whose
        // entire body is left in the clear, e.g. a parameter-set NAL).
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(20, 0), sub(0, 16)];
        let plan = plan_sample_cipher(&d, Some(&subs), 36).expect("clear-only sub");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].kind, CipherStepKind::Clear);
        assert_eq!(plan[0].len, 20);
        assert_eq!(plan[1].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[1].offset, 20);
        assert_eq!(plan[1].len, 16);
    }

    #[test]
    fn plan_subsample_with_only_protected_emits_encrypted_only() {
        // A subsample with clear=0, protected>0 is legal.
        let d = CencSchemeDecision::new(CencScheme::Cenc, cenc_tenc_v0_per_sample()).unwrap();
        let subs = vec![sub(0, 32)];
        let plan = plan_sample_cipher(&d, Some(&subs), 32).expect("protected-only sub");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].kind, CipherStepKind::Encrypted);
        assert_eq!(plan[0].len, 32);
    }

    #[test]
    fn plan_partitions_cover_sample_len_exactly() {
        // Property: the sum of step lengths equals sample_len for any
        // valid input, and offsets are strictly increasing with len.
        let d = CencSchemeDecision::new(CencScheme::Cbcs, cbcs_tenc_v1_constant_iv()).unwrap();
        let subs = vec![sub(8, 160), sub(4, 156), sub(0, 16)];
        let total: u64 = subs
            .iter()
            .map(|s| s.bytes_of_clear_data as u64 + s.bytes_of_protected_data as u64)
            .sum();
        let plan = plan_sample_cipher(&d, Some(&subs), total).expect("partition cover");
        let mut prev_end = 0u64;
        let mut sum = 0u64;
        for step in &plan {
            assert_eq!(step.offset, prev_end, "non-contiguous step");
            assert!(step.len > 0, "zero-length step");
            prev_end = step.offset + step.len;
            sum += step.len;
        }
        assert_eq!(sum, total);
        assert_eq!(prev_end, total);
    }
}
