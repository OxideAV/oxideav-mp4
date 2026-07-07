//! CENC fragment packager — the write-side driver that turns
//! **plaintext** packets into a protected fragmented MP4 (ISO/IEC
//! 23001-7 + ISO/IEC 14496-12 §8.8).
//!
//! [`CencFragmentPackager`] wraps the [`FragmentedMuxer`] and owns the
//! per-track crypto state the muxer deliberately doesn't hold:
//!
//! * the 16-byte AES-128 content keys, keyed by `KID` (the muxer only
//!   ever sees ciphertext + container metadata);
//! * per-sample Initialization Vector generation for the per-sample-IV
//!   schemes (§9.4.2 — "AES-CTR mode encryption SHALL use a unique IV
//!   per sample"): a per-track 64-bit counter written big-endian into
//!   IV bytes 0..8. Under §9.3 the CTR block counter occupies IV bytes
//!   8..16, so distinct top halves guarantee non-overlapping keystreams
//!   across samples under one key; under CBC each IV is simply unique;
//! * the active `seig` override for **key rotation**
//!   ([`Self::rotate_key`]): once rotated, every subsequent sample of
//!   the track is encrypted under the new key and mapped — via the
//!   fragment-local `sgpd('seig')` + `sbgp('seig')` the muxer emits
//!   (§6 + §8.9.4) — to a group entry naming the new `KID`, so the file
//!   remains self-describing across the rotation boundary.
//!
//! Per sample, the packager plans the §9.4–9.6 clear/encrypted
//! partition from the effective parameters (`tenc` defaults or the
//! active `seig` override), encrypts in place via
//! [`crate::cenc_cipher::encrypt_sample_in_place`], and hands the
//! ciphertext plus its §7.1 auxiliary information to
//! [`FragmentedMuxer::write_protected_packet_grouped`] — which emits
//! the per-fragment `senc` / `saiz` / `saio` (+ `seig` groups) that
//! make each fragment independently decryptable (§7.2.1).

use std::collections::HashMap;

use oxideav_core::{Error, Packet, Result, StreamInfo, WriteSeek};

use crate::cenc::{
    CencScheme, CencSchemeDecision, IvSupply, SeigEntry, SencSample, SubsampleEntry, TencBox,
};
use crate::cenc_cipher::encrypt_sample_in_place;
use crate::frag::FragmentedMuxer;
use crate::options::{FragmentedOptions, Mp4MuxerOptions};

/// One content key handed to [`CencFragmentPackager::new`]: the AES-128
/// key for the `tenc.default_KID` of the protected stream at
/// `stream_index`.
#[derive(Clone)]
pub struct TrackKey {
    /// Index into the packager's `streams` slice — must match a
    /// `Mp4MuxerOptions::track_protection` entry.
    pub stream_index: usize,
    /// 16-byte AES-128 content key for the track's default KID.
    pub key: [u8; 16],
}

/// Per-track crypto state.
struct TrackCrypto {
    scheme: CencScheme,
    /// Track defaults as written into `schi/tenc`.
    tenc: TencBox,
    /// KID → content key store. Seeded with `(tenc.default_KID, key)`;
    /// grows on [`CencFragmentPackager::rotate_key`].
    keys: HashMap<[u8; 16], [u8; 16]>,
    /// Active `seig` override (key rotation). `None` → `tenc` defaults.
    active_override: Option<SeigEntry>,
    /// Next per-sample IV counter (written big-endian into IV bytes
    /// 0..8). Starts at 1 so the all-zero IV is never emitted.
    iv_counter: u64,
}

impl TrackCrypto {
    /// Effective encryption parameters for the next sample: the `tenc`
    /// defaults with any active `seig` override applied (ISO/IEC
    /// 23001-7 §6: "Encryption parameters specified in a sample group
    /// SHALL override the corresponding default parameter values").
    fn effective_tenc(&self) -> TencBox {
        match &self.active_override {
            None => self.tenc.clone(),
            Some(seig) => TencBox {
                version: self.tenc.version,
                default_is_protected: seig.is_protected,
                default_per_sample_iv_size: seig.per_sample_iv_size,
                default_kid: seig.kid,
                default_crypt_byte_block: seig.crypt_byte_block,
                default_skip_byte_block: seig.skip_byte_block,
                default_constant_iv: seig.constant_iv.clone(),
            },
        }
    }

    /// Generate the next per-sample IV of `size` bytes (8 or 16): the
    /// 64-bit counter in bytes 0..8, zero elsewhere. See the module
    /// docs for why the counter lives in the *top* half.
    fn next_iv(&mut self, size: u8) -> Vec<u8> {
        let mut iv = vec![0u8; size as usize];
        iv[..8].copy_from_slice(&self.iv_counter.to_be_bytes());
        self.iv_counter += 1;
        iv
    }
}

/// Write-side CENC packager over the fragmented muxer. See the module
/// docs for the division of labour; constructed via [`Self::new`].
pub struct CencFragmentPackager {
    muxer: FragmentedMuxer,
    /// Indexed by stream index; `None` for unprotected streams (their
    /// packets pass through to the plain write path).
    tracks: Vec<Option<TrackCrypto>>,
}

impl CencFragmentPackager {
    /// Build a packager: same construction as
    /// [`crate::frag::open_fragmented_typed`] (the
    /// `options.track_protection` directives drive the `sinf`
    /// envelopes and `tenc` defaults) plus one [`TrackKey`] per
    /// protected stream.
    ///
    /// Fails when a protected stream is missing its key, a key targets
    /// an unprotected stream, or the underlying muxer rejects the
    /// `(scheme, tenc)` pair.
    pub fn new(
        output: Box<dyn WriteSeek>,
        streams: &[StreamInfo],
        options: Mp4MuxerOptions,
        frag_options: FragmentedOptions,
        keys: impl IntoIterator<Item = TrackKey>,
    ) -> Result<CencFragmentPackager> {
        let mut tracks: Vec<Option<TrackCrypto>> = Vec::with_capacity(streams.len());
        for i in 0..streams.len() {
            tracks.push(
                options
                    .track_protection
                    .iter()
                    .find(|p| p.stream_index == i)
                    .map(|p| TrackCrypto {
                        scheme: CencScheme::from_fourcc(&p.scheme_type),
                        tenc: p.tenc.clone(),
                        keys: HashMap::new(),
                        active_override: None,
                        iv_counter: 1,
                    }),
            );
        }
        for tk in keys {
            let slot = tracks
                .get_mut(tk.stream_index)
                .and_then(|t| t.as_mut())
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "CENC packager: key for stream {} which has no track_protection \
                         directive",
                        tk.stream_index
                    ))
                })?;
            slot.keys.insert(slot.tenc.default_kid, tk.key);
        }
        for (i, t) in tracks.iter().enumerate() {
            if let Some(t) = t {
                if t.keys.is_empty() {
                    return Err(Error::invalid(format!(
                        "CENC packager: protected stream {i} has no content key"
                    )));
                }
            }
        }
        let muxer = crate::frag::open_fragmented_typed(output, streams, options, frag_options)?;
        Ok(CencFragmentPackager { muxer, tracks })
    }

    /// Write the init segment (`ftyp + moov`, including the `sinf`
    /// envelopes and any moov-level `pssh`).
    pub fn write_header(&mut self) -> Result<()> {
        use oxideav_core::Muxer;
        self.muxer.write_header()
    }

    /// Flush any pending fragment and finish the file.
    pub fn write_trailer(&mut self) -> Result<()> {
        use oxideav_core::Muxer;
        self.muxer.write_trailer()
    }

    /// Encrypt and write one **plaintext** packet as a full sample
    /// (§9.4 / §9.7 — the shape for audio and other non-NAL tracks).
    /// Unprotected streams pass through to the plain write path.
    pub fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.write_packet_inner(packet, None)
    }

    /// Encrypt and write one **plaintext** packet under a §9.5
    /// subsample map: each `(BytesOfClearData, BytesOfProtectedData)`
    /// pair leaves a prefix in the clear (NAL headers, parameter sets)
    /// and ciphers the suffix, per the scheme's §10 constraints. The
    /// totals must cover the packet exactly (§9.5.1). Rejected on an
    /// unprotected stream (a subsample map has no meaning there).
    pub fn write_packet_with_subsamples(
        &mut self,
        packet: &Packet,
        subsamples: &[SubsampleEntry],
    ) -> Result<()> {
        self.write_packet_inner(packet, Some(subsamples))
    }

    fn write_packet_inner(
        &mut self,
        packet: &Packet,
        subsamples: Option<&[SubsampleEntry]>,
    ) -> Result<()> {
        use oxideav_core::Muxer;
        let idx = packet.stream_index as usize;
        let crypto = match self.tracks.get_mut(idx) {
            Some(Some(c)) => c,
            Some(None) => {
                if subsamples.is_some() {
                    return Err(Error::invalid(format!(
                        "CENC packager: subsample map on unprotected stream {idx}"
                    )));
                }
                return self.muxer.write_packet(packet);
            }
            None => {
                return Err(Error::invalid(format!(
                    "CENC packager: unknown stream index {idx}"
                )))
            }
        };

        let effective = crypto.effective_tenc();
        let decision = CencSchemeDecision::new(crypto.scheme, effective.clone())?;
        let kid = effective.default_kid;
        let key = *crypto.keys.get(&kid).ok_or_else(|| {
            Error::invalid(format!(
                "CENC packager: no content key for the active KID on stream {idx}"
            ))
        })?;

        // Per-sample IV (§9.2): generated for per-sample-IV schemes,
        // absent for constant-IV schemes.
        let iv: Vec<u8> = match decision.iv_supply() {
            IvSupply::PerSample { size } => crypto.next_iv(size),
            IvSupply::Constant => Vec::new(),
            IvSupply::None => {
                return Err(Error::invalid(format!(
                    "CENC packager: stream {idx} effective parameters are unprotected \
                     (isProtected == 0) — route clear samples through the muxer directly"
                )))
            }
        };

        let mut data = packet.data.clone();
        encrypt_sample_in_place(
            &decision,
            &key,
            if iv.is_empty() { None } else { Some(&iv) },
            subsamples,
            &mut data,
        )?;

        let senc = SencSample {
            initialization_vector: iv,
            subsamples: subsamples.map(<[_]>::to_vec).unwrap_or_default(),
        };
        let seig = crypto.active_override.clone();

        let mut out = packet.clone();
        out.data = data;
        self.muxer.write_protected_packet_grouped(&out, senc, seig)
    }

    /// Rotate the content key of a protected stream: subsequent samples
    /// are encrypted under `key` and mapped, via a `seig` sample-group
    /// override naming `kid` (ISO/IEC 23001-7 §6), to the new key —
    /// the fragment-local `sgpd`/`sbgp` emission keeps every fragment
    /// self-describing across the boundary.
    ///
    /// The override inherits the track's scheme geometry (IV size and
    /// pattern) from `tenc`; only the key material changes.
    /// `constant_iv` must be `Some(8/16 bytes)` exactly when the track
    /// uses constant IVs (`tenc.default_Per_Sample_IV_Size == 0`) —
    /// each rotated key gets its own constant IV — and `None`
    /// otherwise.
    ///
    /// Rotating to `tenc.default_KID` restores the default mapping
    /// (equivalent to [`Self::reset_to_default_key`], with the key
    /// store updated).
    pub fn rotate_key(
        &mut self,
        stream_index: usize,
        kid: [u8; 16],
        key: [u8; 16],
        constant_iv: Option<Vec<u8>>,
    ) -> Result<()> {
        let crypto = self
            .tracks
            .get_mut(stream_index)
            .and_then(|t| t.as_mut())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "CENC packager: rotate_key on stream {stream_index} which has no \
                     track_protection directive"
                ))
            })?;
        let uses_constant_iv =
            crypto.tenc.default_is_protected == 1 && crypto.tenc.default_per_sample_iv_size == 0;
        if uses_constant_iv != constant_iv.is_some() {
            return Err(Error::invalid(if uses_constant_iv {
                "CENC packager: rotate_key on a constant-IV track requires a new constant IV"
            } else {
                "CENC packager: rotate_key constant IV supplied but the track uses \
                 per-sample IVs"
            }));
        }
        crypto.keys.insert(kid, key);
        if kid == crypto.tenc.default_kid
            && constant_iv.as_ref() == crypto.tenc.default_constant_iv.as_ref()
        {
            crypto.active_override = None;
            return Ok(());
        }
        let seig = SeigEntry {
            crypt_byte_block: crypto.tenc.default_crypt_byte_block,
            skip_byte_block: crypto.tenc.default_skip_byte_block,
            is_protected: 1,
            per_sample_iv_size: crypto.tenc.default_per_sample_iv_size,
            kid,
            constant_iv,
        };
        // §6 round-trip validation up front (rejects e.g. a bad
        // constant-IV length) so the error surfaces here, not at the
        // next write.
        crate::cenc::build_seig_entry(&seig)?;
        crypto.active_override = Some(seig);
        Ok(())
    }

    /// Drop any active key override: subsequent samples return to the
    /// `tenc` defaults (`default_KID` + the default key).
    pub fn reset_to_default_key(&mut self, stream_index: usize) -> Result<()> {
        let crypto = self
            .tracks
            .get_mut(stream_index)
            .and_then(|t| t.as_mut())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "CENC packager: reset_to_default_key on stream {stream_index} which has \
                     no track_protection directive"
                ))
            })?;
        crypto.active_override = None;
        Ok(())
    }

    /// Queue `pssh` boxes into the next fragment's `moof` (ISO/IEC
    /// 23001-7 §8.1.1) — the licence-delivery channel that usually
    /// accompanies a key rotation. Delegates to
    /// [`FragmentedMuxer::set_next_segment_pssh`].
    pub fn set_next_segment_pssh(&mut self, pssh: impl IntoIterator<Item = crate::cenc::PsshBox>) {
        self.muxer.set_next_segment_pssh(pssh)
    }
}
