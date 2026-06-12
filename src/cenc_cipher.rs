//! CENC AES-128 CTR / CBC cipher driver — ISO/IEC 23001-7:2016 §9.
//!
//! This module executes the [`crate::cenc::CipherStep`] partitions
//! produced by [`crate::cenc::plan_sample_cipher`] against actual
//! AES-128 block operations, turning the parse-only CENC surfaces
//! (`tenc` / `senc` / `seig` / the [`crate::cenc::CencSchemeDecision`]
//! router) into working sample decryption for all four §10 schemes:
//!
//! * `cenc` — AES-CTR full-sample / subsample (§10.1),
//! * `cbc1` — AES-CBC full-sample / subsample (§10.2),
//! * `cens` — AES-CTR subsample pattern (§10.3),
//! * `cbcs` — AES-CBC subsample pattern, constant IV (§10.4).
//!
//! ## Spec contract executed here
//!
//! * **IV expansion (§9.1)** — "If the `Per_Sample_IV_Size` field is 8,
//!   then its value is copied to bytes 0 to 7 of the Initialization
//!   Vector and bytes 8 to 15 of the Initialization Vector are set to
//!   zero." [`expand_iv`] applies that rule uniformly (per-sample and
//!   constant IVs alike — `constant_IV_size` shares the {8, 16} value
//!   set).
//! * **CTR counter operation (§9.3)** — the counter block is the
//!   expanded IV; the least significant 8 bytes (bytes 8 to 15) act as
//!   a 64-bit big-endian block counter incremented by one after each
//!   encrypted cipher block, and on reaching the maximum value the
//!   8-byte counter resets to zero *without affecting bytes 0 to 7*.
//!   With an 8-byte IV the counter therefore starts at zero; with a
//!   16-byte IV it starts at whatever bytes 8..16 of the IV hold.
//! * **Keystream / chain continuity (§9.5.1)** — "For all schemes
//!   except the `cbcs` scheme, the protected byte sequences of a
//!   sample SHALL be treated as a logically continuous chain of 16
//!   byte cipher blocks, even when they are separated by Subsample
//!   `BytesOfClearData`, or a `skip_byte_block`." The driver feeds the
//!   `Encrypted` steps to one continuous cipher instance, so a partial
//!   cipher block at the end of one protected range continues in the
//!   next (`cenc`), the CTR counter only advances over encrypted
//!   blocks (`cens`), and the `cbc1` chain spans clear gaps.
//! * **`cbcs` per-subsample restart (§9.5.1 / §9.6)** — "The `cbcs`
//!   scheme SHALL treat each Subsample as a separate chain of cipher
//!   blocks, starting with the Initialization Vector associated with
//!   the sample." The [`crate::cenc::CipherStep::iv_restart`] flag the
//!   planner sets on the first encrypted step of each subsample resets
//!   the CBC chain to the (constant) IV. Within a subsample the chain
//!   is continuous across `skip_byte_block` runs.
//! * **CBC whole-block discipline (§9.4.3 / §9.5.1 / §10.2)** — every
//!   encrypted CBC run must be a whole number of 16-byte cipher
//!   blocks ("For `cbc1` protection scheme, `BytesOfProtectedData`
//!   size SHALL be adjusted to a multiple of 16 bytes"; the planner
//!   already leaves trailing partial blocks clear on the full-sample,
//!   §9.7 whole-block, and pattern paths). A non-aligned encrypted run
//!   under CBC is a malformed stream and is rejected.
//!
//! ## What is *not* here
//!
//! Key acquisition. The 16-byte content key is a caller input, looked
//! up by `KID` (`tenc.default_kid` / `seig.kid`) from whatever DRM or
//! key-store channel the application uses (`pssh` carries the
//! DRM-system blob; this crate surfaces it verbatim). Decryption is
//! deterministic once the key is in hand — no key derivation is
//! defined by ISO/IEC 23001-7.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, KeyIvInit, StreamCipher};
use oxideav_core::{Error, Result};

use crate::cenc::{
    CencSchemeDecision, CipherMode, CipherStep, CipherStepKind, IvSupply, SubsampleEntry,
};

type Aes128Ctr64 = ctr::Ctr64BE<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Expand an 8- or 16-byte CENC IV into the 16-byte AES block the
/// cipher consumes (§9.1).
///
/// * 16-byte IV — used verbatim (the entire 128-bit value).
/// * 8-byte IV — copied to bytes 0 to 7; bytes 8 to 15 are set to
///   zero. Under CTR those zero bytes are the starting value of the
///   64-bit block counter (§9.3).
///
/// Any other length is rejected — §9.1 admits only {8, 16} for both
/// `Per_Sample_IV_Size` and `constant_IV_size`.
pub fn expand_iv(iv: &[u8]) -> Result<[u8; 16]> {
    let mut block = [0u8; 16];
    match iv.len() {
        8 => block[..8].copy_from_slice(iv),
        16 => block.copy_from_slice(iv),
        n => {
            return Err(Error::invalid(format!(
                "CENC cipher: IV length {n} not in {{8, 16}} (§9.1)"
            )))
        }
    }
    Ok(block)
}

/// Decrypt the [`CipherStepKind::Encrypted`] runs of one sample in
/// place, leaving [`CipherStepKind::Clear`] runs untouched.
///
/// This is the low-level engine: the caller supplies the cipher mode,
/// the 16-byte AES-128 content key, the raw 8- or 16-byte IV (expanded
/// per §9.1), the step partition from
/// [`crate::cenc::plan_sample_cipher`], and the sample bytes. Most
/// callers want [`decrypt_sample_in_place`], which resolves the IV
/// discipline from the [`CencSchemeDecision`] and builds the plan
/// itself; this entry point exists for `seig`-overridden sample groups
/// where the caller assembled a custom plan/IV pairing.
///
/// Mode semantics:
///
/// * [`CipherMode::Ctr`] — one continuous keystream positioned at byte
///   0 of the concatenated encrypted runs (§9.5.1 logical continuity):
///   a partial cipher block ending one run continues in the next, and
///   skipped pattern blocks / clear gaps consume no keystream. The
///   counter is the low 64 bits of the expanded IV, big-endian,
///   wrapping without carry into bytes 0..8 (§9.3). `iv_restart` never
///   occurs on a CTR plan (§9.6 applies the IV to the first encrypted
///   block of each *sample*); encountering one is rejected as an
///   inconsistent plan.
/// * [`CipherMode::Cbc`] — one continuous cipher-block chain seeded
///   with the IV; a step with `iv_restart == true` (first encrypted
///   step of each `cbcs` subsample) reseeds the chain with the IV.
///   Every encrypted run must be a multiple of 16 bytes (§9.4.3 NOTE /
///   §9.5.1 / §10.2 — partial blocks are never CBC-encrypted).
///
/// Steps must lie within `data` (the planner guarantees this when
/// `sample_len == data.len()`; it is re-checked here because this
/// entry point accepts caller-assembled plans).
pub fn decrypt_steps_in_place(
    mode: CipherMode,
    key: &[u8; 16],
    iv: &[u8],
    steps: &[CipherStep],
    data: &mut [u8],
) -> Result<()> {
    let iv_block = expand_iv(iv)?;
    let key_ga = GenericArray::from_slice(key);
    let iv_ga = GenericArray::from_slice(&iv_block);

    // Validate the step geometry once, up front.
    for (i, step) in steps.iter().enumerate() {
        let end = step.offset.checked_add(step.len).ok_or_else(|| {
            Error::invalid(format!("CENC cipher: step {i} offset+len overflows u64"))
        })?;
        if end > data.len() as u64 {
            return Err(Error::invalid(format!(
                "CENC cipher: step {i} ends at {end} past sample length {}",
                data.len()
            )));
        }
    }

    match mode {
        CipherMode::Ctr => {
            let mut cipher = Aes128Ctr64::new(key_ga, iv_ga);
            for (i, step) in steps.iter().enumerate() {
                if step.kind != CipherStepKind::Encrypted {
                    continue;
                }
                if step.iv_restart {
                    return Err(Error::invalid(format!(
                        "CENC cipher: step {i} carries iv_restart under CTR — \
                         §9.6 applies the IV once per sample for CTR schemes"
                    )));
                }
                let range = step.offset as usize..(step.offset + step.len) as usize;
                cipher.apply_keystream(&mut data[range]);
            }
        }
        CipherMode::Cbc => {
            let mut dec = Aes128CbcDec::new(key_ga, iv_ga);
            for step in steps.iter() {
                if step.kind != CipherStepKind::Encrypted {
                    continue;
                }
                if step.iv_restart {
                    // §9.5.1: cbcs treats each subsample as a separate
                    // chain starting with the sample's IV.
                    dec = Aes128CbcDec::new(key_ga, iv_ga);
                }
                if step.len % 16 != 0 {
                    return Err(Error::invalid(format!(
                        "CENC cipher: encrypted CBC run of {} bytes is not a \
                         multiple of 16 (§9.4.3 / §9.5.1 — partial blocks stay clear)",
                        step.len
                    )));
                }
                let range = step.offset as usize..(step.offset + step.len) as usize;
                for block in data[range].chunks_exact_mut(16) {
                    dec.decrypt_block_mut(GenericArray::from_mut_slice(block));
                }
            }
        }
    }
    Ok(())
}

/// Decrypt one protected sample in place using the track-level routing
/// decision.
///
/// Inputs:
///
/// * `decision` — the [`CencSchemeDecision`] built from `sinf/schm` +
///   `tenc` (for a `seig`-overridden sample group, build the decision
///   from the group's parameters instead, or drop down to
///   [`decrypt_steps_in_place`]).
/// * `key` — the 16-byte AES-128 content key matching the sample's
///   `KID`. Key lookup is the caller's concern.
/// * `per_sample_iv` — the sample's `InitializationVector` from `senc`
///   or `saiz`/`saio` auxiliary data
///   ([`crate::cenc::SencSample::initialization_vector`]). Required —
///   and length-checked against `tenc.default_Per_Sample_IV_Size` —
///   when the decision reports [`IvSupply::PerSample`]. Must be `None`
///   (or empty, as `parse_senc` yields for IV size 0) when the
///   decision reports [`IvSupply::Constant`]: §9.2 — IVs are *either*
///   constant *or* per-sample, never both.
/// * `subsamples` — the sample's subsample map for §9.5 subsample
///   encryption ([`crate::cenc::SencSample::subsamples`]); `None` for
///   §9.4 full-sample / §9.7 whole-block encryption.
/// * `data` — the sample payload, decrypted in place.
///
/// Errors mirror [`crate::cenc::plan_sample_cipher`] (subsample totals,
/// both-zero entries, unknown scheme, unprotected track) plus the IV
/// resolution and CBC alignment failures described above.
pub fn decrypt_sample_in_place(
    decision: &CencSchemeDecision,
    key: &[u8; 16],
    per_sample_iv: Option<&[u8]>,
    subsamples: Option<&[SubsampleEntry]>,
    data: &mut [u8],
) -> Result<()> {
    let mode = decision.cipher_mode().ok_or_else(|| {
        Error::invalid("CENC cipher: unknown scheme — no §10-registered cipher mode")
    })?;
    let supplied = per_sample_iv.filter(|iv| !iv.is_empty());
    let iv: &[u8] =
        match decision.iv_supply() {
            IvSupply::None => return Err(Error::invalid(
                "CENC cipher: track default is unprotected (isProtected == 0) — nothing to decrypt",
            )),
            IvSupply::PerSample { size } => {
                let iv = supplied.ok_or_else(|| {
                    Error::invalid(format!(
                        "CENC cipher: Per_Sample_IV_Size {size} requires a per-sample IV (§9.2)"
                    ))
                })?;
                if iv.len() != size as usize {
                    return Err(Error::invalid(format!(
                        "CENC cipher: per-sample IV is {} bytes but Per_Sample_IV_Size is {size}",
                        iv.len()
                    )));
                }
                iv
            }
            IvSupply::Constant => {
                if supplied.is_some() {
                    return Err(Error::invalid(
                        "CENC cipher: constant-IV configuration was handed a per-sample IV \
                     (§9.2 — IVs are either constant or per-sample)",
                    ));
                }
                decision
                    .tenc
                    .default_constant_iv
                    .as_deref()
                    .ok_or_else(|| {
                        Error::invalid(
                            "CENC cipher: constant-IV configuration without default_constant_IV",
                        )
                    })?
            }
        };
    let plan = crate::cenc::plan_sample_cipher(decision, subsamples, data.len() as u64)?;
    decrypt_steps_in_place(mode, key, iv, &plan, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cenc::{CencScheme, TencBox};
    use aes::cipher::{BlockEncrypt, BlockEncryptMut, KeyInit};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    // ---- fixture helpers --------------------------------------------

    const KEY: [u8; 16] = [
        0x2B, 0x7E, 0x15, 0x16, 0x28, 0xAE, 0xD2, 0xA6, 0xAB, 0xF7, 0x15, 0x88, 0x09, 0xCF, 0x4F,
        0x3C,
    ];

    fn tenc(
        version: u8,
        iv_size: u8,
        crypt: u8,
        skip: u8,
        constant_iv: Option<Vec<u8>>,
    ) -> TencBox {
        TencBox {
            version,
            default_is_protected: 1,
            default_per_sample_iv_size: iv_size,
            default_kid: [0x11; 16],
            default_crypt_byte_block: crypt,
            default_skip_byte_block: skip,
            default_constant_iv: constant_iv,
        }
    }

    fn cenc_decision(iv_size: u8) -> CencSchemeDecision {
        CencSchemeDecision::new(CencScheme::Cenc, tenc(0, iv_size, 0, 0, None)).unwrap()
    }

    fn cbc1_decision() -> CencSchemeDecision {
        CencSchemeDecision::new(CencScheme::Cbc1, tenc(0, 16, 0, 0, None)).unwrap()
    }

    fn cens_decision(crypt: u8, skip: u8) -> CencSchemeDecision {
        CencSchemeDecision::new(CencScheme::Cens, tenc(1, 8, crypt, skip, None)).unwrap()
    }

    fn cbcs_decision(crypt: u8, skip: u8, iv: [u8; 16]) -> CencSchemeDecision {
        CencSchemeDecision::new(CencScheme::Cbcs, tenc(1, 0, crypt, skip, Some(iv.to_vec())))
            .unwrap()
    }

    /// AES-128 ECB of one block — used to build §9.3 keystream
    /// expectations from first principles (counter block per block
    /// index) rather than through the same CTR code path under test.
    fn ecb_block(key: &[u8; 16], block: [u8; 16]) -> [u8; 16] {
        let cipher = aes::Aes128::new(GenericArray::from_slice(key));
        let mut b = GenericArray::clone_from_slice(&block);
        cipher.encrypt_block(&mut b);
        b.into()
    }

    /// §9.3 counter block n for an 8-byte IV: IV in bytes 0..8, the
    /// 64-bit big-endian block index in bytes 8..16.
    fn ctr_block_iv8(iv8: [u8; 8], index: u64) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&iv8);
        b[8..].copy_from_slice(&index.to_be_bytes());
        b
    }

    fn deterministic_payload(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect()
    }

    // ---- IV expansion (§9.1) ----------------------------------------

    #[test]
    fn expand_iv_pads_8_byte_iv_into_high_bytes() {
        // §9.1: 8-byte value copied to bytes 0..8, bytes 8..16 zero.
        let iv8 = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7];
        let block = expand_iv(&iv8).expect("8-byte IV");
        assert_eq!(&block[..8], &iv8);
        assert_eq!(&block[8..], &[0u8; 8]);
    }

    #[test]
    fn expand_iv_uses_16_byte_iv_verbatim() {
        let iv16: Vec<u8> = (0..16).collect();
        assert_eq!(expand_iv(&iv16).expect("16-byte IV").to_vec(), iv16);
    }

    #[test]
    fn expand_iv_rejects_other_lengths() {
        for len in [0usize, 4, 7, 9, 15, 17, 32] {
            assert!(expand_iv(&vec![0u8; len]).is_err(), "len {len} must fail");
        }
    }

    // ---- AES anchor (FIPS-197 Appendix C.1) --------------------------

    #[test]
    fn aes128_fips197_known_block() {
        // FIPS-197 C.1 example vector: anchors that the wired AES-128
        // primitive computes the standard cipher, independent of any
        // CENC layering.
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF,
        ];
        let expected: [u8; 16] = [
            0x69, 0xC4, 0xE0, 0xD8, 0x6A, 0x7B, 0x04, 0x30, 0xD8, 0xCD, 0xB7, 0x80, 0x70, 0xB4,
            0xC5, 0x5A,
        ];
        assert_eq!(ecb_block(&key, pt), expected);
    }

    // ---- CTR counter construction (§9.3) -----------------------------

    #[test]
    fn ctr_8_byte_iv_counter_starts_at_zero_and_increments() {
        // Decrypting zeros yields the raw keystream; compare it to
        // first-principles ECB(counter-block) values per §9.3.
        let iv8 = [0x10, 0x32, 0x54, 0x76, 0x98, 0xBA, 0xDC, 0xFE];
        let d = cenc_decision(8);
        let mut data = vec![0u8; 40]; // 2 whole blocks + 8-byte partial
        decrypt_sample_in_place(&d, &KEY, Some(&iv8), None, &mut data).expect("ctr decrypt");
        let mut expected = Vec::new();
        for n in 0..3u64 {
            expected.extend_from_slice(&ecb_block(&KEY, ctr_block_iv8(iv8, n)));
        }
        assert_eq!(data, expected[..40], "keystream = ECB(IV||n) for n=0,1,2");
    }

    #[test]
    fn ctr_16_byte_iv_counter_wraps_without_carry() {
        // §9.3: when the low 64-bit counter reaches its maximum,
        // incrementing resets bytes 8..16 to zero *without affecting*
        // bytes 0..8.
        let mut iv16 = [0xAB; 16];
        iv16[8..].copy_from_slice(&u64::MAX.to_be_bytes());
        let d = cenc_decision(16);
        let mut data = vec![0u8; 32];
        decrypt_sample_in_place(&d, &KEY, Some(&iv16), None, &mut data).expect("ctr decrypt");

        let block0 = ecb_block(&KEY, iv16);
        let mut wrapped = [0xAB; 16];
        wrapped[8..].copy_from_slice(&0u64.to_be_bytes());
        let block1 = ecb_block(&KEY, wrapped);
        assert_eq!(&data[..16], &block0);
        assert_eq!(
            &data[16..],
            &block1,
            "counter must wrap to 0, high bytes untouched"
        );
    }

    // ---- cenc: full-sample + subsample continuity (§9.4.2 / §9.5.1) --

    #[test]
    fn cenc_full_sample_roundtrip_encrypts_partial_tail() {
        // §9.4.2: CTR full-sample encrypts *all* bytes including the
        // trailing partial block.
        let iv8 = [1, 2, 3, 4, 5, 6, 7, 8];
        let plaintext = deterministic_payload(53);
        // Build ciphertext from the first-principles keystream.
        let mut ct = plaintext.clone();
        for (i, byte) in ct.iter_mut().enumerate() {
            let ks = ecb_block(&KEY, ctr_block_iv8(iv8, (i / 16) as u64));
            *byte ^= ks[i % 16];
        }
        assert_ne!(ct[48..], plaintext[48..], "partial tail must be encrypted");
        let d = cenc_decision(8);
        decrypt_sample_in_place(&d, &KEY, Some(&iv8), None, &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    #[test]
    fn cenc_subsample_keystream_is_continuous_across_clear_gaps() {
        // §9.5.1: the protected byte sequences form one logically
        // continuous chain of cipher blocks even when separated by
        // BytesOfClearData — a partial block ending subsample 1's
        // protected range continues in subsample 2's.
        let iv8 = [9, 8, 7, 6, 5, 4, 3, 2];
        let subs = [
            SubsampleEntry {
                bytes_of_clear_data: 4,
                bytes_of_protected_data: 10,
            },
            SubsampleEntry {
                bytes_of_clear_data: 2,
                bytes_of_protected_data: 16,
            },
        ];
        let plaintext = deterministic_payload(32);
        // Encrypt by XOR-ing the *concatenated* protected bytes
        // (offsets 4..14 and 16..32 → 26 keystream bytes, byte-
        // continuous across the gap) with the §9.3 keystream.
        let mut ct = plaintext.clone();
        let protected_ranges = [(4usize, 14usize), (16, 32)];
        let mut ks_pos = 0usize;
        for (start, end) in protected_ranges {
            for byte in &mut ct[start..end] {
                let ks = ecb_block(&KEY, ctr_block_iv8(iv8, (ks_pos / 16) as u64));
                *byte ^= ks[ks_pos % 16];
                ks_pos += 1;
            }
        }
        let d = cenc_decision(8);
        decrypt_sample_in_place(&d, &KEY, Some(&iv8), Some(&subs), &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    // ---- cbc1: full-sample + chain continuity (§9.4.3 / §9.5.1) ------

    #[test]
    fn cbc1_full_sample_leaves_partial_tail_clear() {
        // §9.4.3 NOTE: CBC encrypts whole blocks only; the trailing
        // 1–15 bytes stay clear.
        let iv16: [u8; 16] = [0x42; 16];
        let plaintext = deterministic_payload(40);
        let mut ct = plaintext.clone();
        let mut enc = Aes128CbcEnc::new(
            GenericArray::from_slice(&KEY),
            GenericArray::from_slice(&iv16),
        );
        for block in ct[..32].chunks_exact_mut(16) {
            enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        assert_eq!(
            ct[32..],
            plaintext[32..],
            "tail must be untouched ciphertext-side"
        );
        let d = cbc1_decision();
        decrypt_sample_in_place(&d, &KEY, Some(&iv16), None, &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    #[test]
    fn cbc1_subsample_chain_is_continuous_across_clear_gaps() {
        // §9.5.1: "CBC mode cipher block chaining for the 'cbc1'
        // scheme SHALL be continuous per sample" — block 3 chains from
        // block 2's ciphertext even though clear bytes sit between
        // them on the wire.
        let iv16: [u8; 16] = [0x24; 16];
        let subs = [
            SubsampleEntry {
                bytes_of_clear_data: 5,
                bytes_of_protected_data: 32,
            },
            SubsampleEntry {
                bytes_of_clear_data: 7,
                bytes_of_protected_data: 16,
            },
        ];
        let plaintext = deterministic_payload(60);
        let mut ct = plaintext.clone();
        let mut enc = Aes128CbcEnc::new(
            GenericArray::from_slice(&KEY),
            GenericArray::from_slice(&iv16),
        );
        for (start, end) in [(5usize, 37usize), (44, 60)] {
            for block in ct[start..end].chunks_exact_mut(16) {
                enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
            }
        }
        let d = cbc1_decision();
        decrypt_sample_in_place(&d, &KEY, Some(&iv16), Some(&subs), &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    #[test]
    fn cbc1_rejects_non_block_aligned_protected_range() {
        // §10.2: BytesOfProtectedData SHALL be a multiple of 16 for
        // cbc1 — a 20-byte protected range is a malformed stream.
        let subs = [SubsampleEntry {
            bytes_of_clear_data: 4,
            bytes_of_protected_data: 20,
        }];
        let mut data = vec![0u8; 24];
        let d = cbc1_decision();
        let err = decrypt_sample_in_place(&d, &KEY, Some(&[0x42; 16]), Some(&subs), &mut data)
            .unwrap_err();
        assert!(format!("{err}").contains("multiple of 16"), "err = {err}");
    }

    // ---- cens: pattern CTR (§9.6 + §9.5.1) ----------------------------

    #[test]
    fn cens_pattern_skip_blocks_consume_no_keystream() {
        // §9.5.1: the chain is continuous "even when ... separated by
        // ... a skip_byte_block" + "The CTR mode counter SHALL be
        // incremented after each complete *encrypted* cipher block".
        // Pattern 2:1 over a 48-byte protected range: blocks 0,1
        // encrypted with counters 0,1 — block 2 skipped — and the
        // next subsample's first encrypted block continues at
        // counter 2.
        let iv8 = [0xCA, 0xFE, 0xF0, 0x0D, 0x00, 0x11, 0x22, 0x33];
        let subs = [
            SubsampleEntry {
                bytes_of_clear_data: 3,
                bytes_of_protected_data: 48,
            },
            SubsampleEntry {
                bytes_of_clear_data: 1,
                bytes_of_protected_data: 40,
            },
        ];
        // Sample layout (offsets):
        //   0..3    clear prefix
        //   3..19   enc (counter 0)     ┐ pattern 2:1
        //   19..35  enc (counter 1)     │ subsample 1
        //   35..51  skip                ┘
        //   51..52  clear prefix
        //   52..68  enc (counter 2)     ┐ pattern 2:1
        //   68..84  enc (counter 3)     │ subsample 2
        //   84..92  partial skip (8 B)  ┘ (§9.6 truncation)
        let plaintext = deterministic_payload(92);
        let mut ct = plaintext.clone();
        let enc_runs: [(usize, u64); 4] = [(3, 0), (19, 1), (52, 2), (68, 3)];
        for (start, counter) in enc_runs {
            let ks = ecb_block(&KEY, ctr_block_iv8(iv8, counter));
            for (j, byte) in ct[start..start + 16].iter_mut().enumerate() {
                *byte ^= ks[j];
            }
        }
        let d = cens_decision(2, 1);
        decrypt_sample_in_place(&d, &KEY, Some(&iv8), Some(&subs), &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    // ---- cbcs: pattern CBC, constant IV, per-subsample restart --------

    #[test]
    fn cbcs_pattern_restarts_chain_per_subsample_and_chains_over_skips() {
        // §9.5.1: "The 'cbcs' scheme SHALL treat each Subsample as a
        // separate chain of cipher blocks, starting with the
        // Initialization Vector associated with the sample" and the
        // chain is "continuous per Subsample" — i.e. the encrypted
        // block after a skip run chains from the previous *encrypted*
        // block's ciphertext. Pattern 1:9, two subsamples; the second
        // subsample's protected range is long enough (2 patterns +
        // one crypt block) to exercise chaining across two skip runs.
        let iv16: [u8; 16] = [0x77; 16];
        let subs = [
            SubsampleEntry {
                bytes_of_clear_data: 10,
                bytes_of_protected_data: 100,
            },
            SubsampleEntry {
                bytes_of_clear_data: 6,
                bytes_of_protected_data: 336,
            },
        ];
        let total = 10 + 100 + 6 + 336;
        let plaintext = deterministic_payload(total);
        let mut ct = plaintext.clone();
        // Encrypted blocks per the 1:9 pattern:
        //   subsample 1 (protected 10..110): block at 10 (then 84
        //     remaining bytes are all inside the 9-block skip run).
        //   subsample 2 (protected 116..452): blocks at 116, 276, 436
        //     (pattern stride 160; 336 = 2*160 + 16, so the final
        //     crypt block is whole and encrypted).
        for blocks in [vec![10usize], vec![116, 276, 436]] {
            let mut enc = Aes128CbcEnc::new(
                GenericArray::from_slice(&KEY),
                GenericArray::from_slice(&iv16),
            );
            for b in blocks {
                enc.encrypt_block_mut(GenericArray::from_mut_slice(&mut ct[b..b + 16]));
            }
        }
        let d = cbcs_decision(1, 9, iv16);
        decrypt_sample_in_place(&d, &KEY, None, Some(&subs), &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    #[test]
    fn cbcs_identical_subsamples_decrypt_identically_under_constant_iv() {
        // The constant IV + per-subsample restart make two identical
        // protected ranges produce identical ciphertext — and decrypt
        // back to the identical plaintext.
        let iv16: [u8; 16] = [0x05; 16];
        let subs = [
            SubsampleEntry {
                bytes_of_clear_data: 0,
                bytes_of_protected_data: 32,
            },
            SubsampleEntry {
                bytes_of_clear_data: 0,
                bytes_of_protected_data: 32,
            },
        ];
        let half = deterministic_payload(32);
        let plaintext: Vec<u8> = [half.clone(), half].concat();
        let mut ct = plaintext.clone();
        for sub in 0..2 {
            let mut enc = Aes128CbcEnc::new(
                GenericArray::from_slice(&KEY),
                GenericArray::from_slice(&iv16),
            );
            for block in ct[sub * 32..sub * 32 + 32].chunks_exact_mut(16) {
                enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
            }
        }
        assert_eq!(
            ct[..32].to_vec(),
            ct[32..].to_vec(),
            "constant IV ⇒ identical ct"
        );
        // Pattern 2:8 — the crypt run (2 blocks = 32 bytes) covers each
        // subsample's whole protected range, so both blocks of each
        // range are encrypted, chained from the restarted IV.
        let d = cbcs_decision(2, 8, iv16);
        decrypt_sample_in_place(&d, &KEY, None, Some(&subs), &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    #[test]
    fn cbcs_whole_block_full_sample_for_non_video_track() {
        // §9.7 / §10.4: non-video cbcs tracks use whole-block
        // full-sample encryption — skip_byte_block 0, IV reset per
        // sample, trailing partial block clear.
        let iv16: [u8; 16] = [0x3C; 16];
        let plaintext = deterministic_payload(50);
        let mut ct = plaintext.clone();
        let mut enc = Aes128CbcEnc::new(
            GenericArray::from_slice(&KEY),
            GenericArray::from_slice(&iv16),
        );
        for block in ct[..48].chunks_exact_mut(16) {
            enc.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }
        let d = cbcs_decision(1, 0, iv16);
        decrypt_sample_in_place(&d, &KEY, None, None, &mut ct).expect("decrypt");
        assert_eq!(ct, plaintext);
    }

    // ---- IV resolution errors -----------------------------------------

    #[test]
    fn per_sample_scheme_requires_iv() {
        let d = cenc_decision(8);
        let mut data = vec![0u8; 16];
        assert!(decrypt_sample_in_place(&d, &KEY, None, None, &mut data).is_err());
        // parse_senc yields an empty IV vec under IV size 0; an empty
        // slice is "absent", not a zero-length IV.
        assert!(decrypt_sample_in_place(&d, &KEY, Some(&[]), None, &mut data).is_err());
    }

    #[test]
    fn per_sample_iv_length_must_match_tenc() {
        let d = cenc_decision(8);
        let mut data = vec![0u8; 16];
        let err = decrypt_sample_in_place(&d, &KEY, Some(&[0u8; 16]), None, &mut data).unwrap_err();
        assert!(
            format!("{err}").contains("Per_Sample_IV_Size"),
            "err = {err}"
        );
    }

    #[test]
    fn constant_iv_scheme_rejects_supplied_per_sample_iv() {
        let d = cbcs_decision(1, 9, [0x66; 16]);
        let subs = [SubsampleEntry {
            bytes_of_clear_data: 0,
            bytes_of_protected_data: 16,
        }];
        let mut data = vec![0u8; 16];
        let err = decrypt_sample_in_place(&d, &KEY, Some(&[0u8; 16]), Some(&subs), &mut data)
            .unwrap_err();
        assert!(format!("{err}").contains("constant"), "err = {err}");
        // ...but an empty slice (what parse_senc produces for IV size
        // 0) is fine.
        decrypt_sample_in_place(&d, &KEY, Some(&[]), Some(&subs), &mut data)
            .expect("empty per-sample IV slice is treated as absent");
    }

    #[test]
    fn unprotected_decision_is_rejected() {
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
        let mut data = vec![0u8; 16];
        assert!(decrypt_sample_in_place(&d, &KEY, Some(&[0u8; 8]), None, &mut data).is_err());
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let d = CencSchemeDecision::new(CencScheme::from_fourcc(b"priv"), tenc(0, 8, 0, 0, None))
            .unwrap();
        let mut data = vec![0u8; 16];
        assert!(decrypt_sample_in_place(&d, &KEY, Some(&[0u8; 8]), None, &mut data).is_err());
    }

    // ---- step-level engine guards --------------------------------------

    #[test]
    fn steps_past_end_of_sample_are_rejected() {
        let steps = [CipherStep {
            offset: 8,
            len: 16,
            kind: CipherStepKind::Encrypted,
            iv_restart: false,
        }];
        let mut data = vec![0u8; 16];
        assert!(
            decrypt_steps_in_place(CipherMode::Ctr, &KEY, &[0u8; 8], &steps, &mut data).is_err()
        );
    }

    #[test]
    fn ctr_plan_with_iv_restart_is_rejected_as_inconsistent() {
        // §9.6: under CTR the IV applies once per sample; only cbcs
        // (CBC) plans carry iv_restart. A hand-rolled plan mixing the
        // two is refused rather than silently re-seeded.
        let steps = [CipherStep {
            offset: 0,
            len: 16,
            kind: CipherStepKind::Encrypted,
            iv_restart: true,
        }];
        let mut data = vec![0u8; 16];
        let err = decrypt_steps_in_place(CipherMode::Ctr, &KEY, &[0u8; 8], &steps, &mut data)
            .unwrap_err();
        assert!(format!("{err}").contains("iv_restart"), "err = {err}");
    }

    #[test]
    fn clear_steps_are_left_untouched() {
        // A plan of one clear run: data must come back bit-identical
        // under both modes.
        let steps = [CipherStep {
            offset: 0,
            len: 24,
            kind: CipherStepKind::Clear,
            iv_restart: false,
        }];
        let payload = deterministic_payload(24);
        for mode in [CipherMode::Ctr, CipherMode::Cbc] {
            let mut data = payload.clone();
            decrypt_steps_in_place(mode, &KEY, &[0u8; 16], &steps, &mut data).expect("clear");
            assert_eq!(data, payload, "{mode:?} must not touch clear runs");
        }
    }
}
