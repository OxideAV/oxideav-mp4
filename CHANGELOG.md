# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Explicit per-track edit lists on the muxer: `Mp4MuxerOptions::track_edit_lists` (new `TrackEditList` record — `stream_index` + `demux::EditListEntry` entries) emits a caller-supplied `edts/elst` verbatim, overriding the automatic start-delay emission for that track and written even when `write_edit_list` is `false` (the flag now governs only the automatic behaviour). Serialised through `demux::build_elst_box`, so §8.6.6.3 round-trip violations and out-of-range stream indices fail at `open`, never at `write_trailer`. Completes demux↔mux elst parity: a remuxer feeds `Mp4Demuxer::edit_list`'s slice straight back in. 4 integration tests (verbatim emission + timeline recovery, automatic-form override, flag independence, open-time rejection sweep)
- Typed §8.6.6 edit-list public surface: `demux::EditListEntry` (`segment_duration` movie-timescale / `media_time` media-timescale / `media_rate_integer` / `media_rate_fraction`, with `is_empty_edit()` / `is_dwell()` helpers), the per-stream `Mp4Demuxer::edit_list(stream)` accessor, flat `elst_entry_count` + `elst_<n>` (`dur=<d> media_time=<mt> rate=<int>`) keys on `params.options`, and the standalone byte pair `demux::parse_elst_box` / `build_elst_box` (byte-exact inverses; auto v0/v1 width promotion; build rejects §8.6.6.3 violations — `media_rate_integer` outside {0, 1}, a final empty edit, `media_time` below the -1 empty-edit sentinel, an empty entry list). Lets a remuxer carry a source's elst across unchanged and a validator check the declaration without re-walking the moov. 6 unit tests + 2 integration tests
- Full ISO/IEC 14496-12 §8.6.6 edit-list timeline mapping on the demux path (moov sample tables and `moof` fragments alike). Previously only the first non-empty edit's `media_time` was subtracted; now each edit segment contributes its own presentation delta: leading empty edits delay the track start by their movie-timescale duration (so a muxer-written start-delay elst round-trips to the original packet pts), initial trims subtract the trim point, dwells (`media_rate_integer = 0`) insert presentation time without consuming media, and multi-segment lists hop each mapped media range into place (movie→media `segment_duration` rescale with saturating 128-bit math; a zero duration is the §8.6.6.1 open-ended form). Media the list never presents — decode pre-roll before the first mapped composition time, and ranges excised between segments — is still delivered for decoding but carries the packet `discard` flag. Non-media-monotonic lists (media re-ordered or repeated, where per-sample deltas would break DTS monotonicity) fall back to the previous leading-shift mapping. 11 new timeline unit tests + 8 integration tests over synthetic elst shapes (trim / delay+trim across timescales / dwell / padded excision / v1 64-bit layout / hostile u64::MAX magnitudes), including a PATH-gated black-box cross-check that an independent reader reports the same start delay on a muxer-produced file

### Other

- Marked the internal `boxes` module `#[doc(hidden)]` — the low-level BMFF box-header reader and FourCC constant table (`BoxHeader`, `read_box_header`/`read_box_body`/`skip_box_body`, `fourcc`, ~150 FourCC constants) are implementation plumbing the README documents no stable surface for; excluding them from the documented public API keeps cargo-semver-checks bump-level detection focused on the real demux/mux/CENC/HEIF API. No signature or behaviour change.
- Round 407 — PIFF legacy `uuid` encryption boxes + DASH `emsg`. PIFF (Protected Interoperable File Format 1.1/1.3, the pre-CENC predecessors of `senc`/`tenc`/`pssh` carried as ISO/IEC 14496-12 §4.2 `uuid` extended-type boxes): all three vendor usertypes parsed and re-emitted — `schi`-level TrackEncryptionBox (`cenc::PiffTencBox`, u24 AlgorithmID / IV_size / KID, surfaced as `piff_*` stream options + `Mp4Demuxer::piff_tencs`), moov/moof-level ProtectionSystemSpecificHeaderBox (byte-identical to a v0 `pssh` payload; `piff_psshes` / `piff_moof_psshes` + `piff_pssh_<n>` / `piff_moof_pssh_<n>` metadata), traf-level SampleEncryptionBox with the PIFF-only `flags & 1` inline override triple (`cenc::PiffSencBox` / `PiffSencOverride`; `piff_senc_records` + `piff_senc_<n>` metadata). PIFF-only trafs bridge into the standard `senc_records` surface (tables are byte-compatible) and `PiffTencBox::to_tenc`/`scheme_decision` route legacy tracks into the existing `CencSchemeDecision` decryption path; dual-branded files keep the 4CC boxes authoritative with no double-reporting. Write side: `build_piff_tenc_box` / `build_piff_senc_box` / `build_piff_pssh_box`, byte-exact inverses with round-trip rejection. `emsg` (ISO/IEC 23009-1 §5.10.3.3 DASH Event Message Box): new `emsg` module with the typed `EmsgBox` (scheme_id_uri / value / timescale / `EmsgTime` Delta-v0-vs-Absolute-v1 / event_duration + unknown sentinel / id / message_data), `parse_emsg_box` / `build_emsg_box` covering both version field orders; demux captures instances keyed by the following moof's index (`Mp4Demuxer::emsgs` + `emsg_<n>` metadata) and `FragmentedMuxer::set_next_segment_emsg` queues write-side events per segment, emitted between `styp` and `moof` with their bytes counted into the `sidx` referenced_size / `ssix` metadata range. `Mp4Demuxer::emsg_absolute_time` resolves a v0 delta to an absolute presentation time by anchoring it to the earliest presentation time of the moof the box preceded (per-moof anchors captured during the fragment walk, cross-timescale-safe via rational comparison and rescaled into the emsg timescale); v1 passes through. 11 integration tests (mux→demux round trips, byte-level sidx coverage gate, v0-delta anchoring across timescales, dual-branding, hostile truncation/UTF-8/version/NUL inputs) + 20 unit tests incl. full truncation sweeps over all three PIFF bodies
- Round 396 — self-describing CENC fragment packaging (ISO/IEC 23001-7 §7.1–7.2 + ISO/IEC 14496-12 §8.7.8–9 / §8.8.14): new `FragmentedMuxer::write_protected_packet(packet, SencSample)` queues each protected sample's §7.1 auxiliary information (per-sample IV + optional subsample map) alongside the payload, validated at queue time (§9.2 IV-supply discipline against the track `tenc`, §9.5.1 subsample-total coverage, §7.2.3 all-samples-or-none per fragment — mixing with plain `write_packet` on one track within a fragment is rejected both ways). Each flush emits per `traf`: a §7.2 `senc` (`UseSubSampleEncryption` flag iff any sample maps subsamples), a §8.7.8 `saiz` (constant-size shortcut when every sample's aux-info size agrees, per-sample table otherwise, `aux_info_type` omitted per the §7.1 SHOULD), and a §8.7.9 `saio` whose single offset is patched post-layout to the moof-relative position of the first `senc` entry (§8.8.14 `default-base-is-moof`), making every fragment independently decryptable (§7.2.1). Constant-IV tracks without subsample maps omit all three boxes (§7.1 empty-aux rule). `EveryKeyframe` detach carries the senc entry with the replayed sample. Demux side: new public `demux::open_typed` returns the (now-public) `Mp4Demuxer` so decrypt/indexing layers reach the structured `senc_records` / `sai_records` / `traf_sample_groups` accessors without downcasting. 9 integration tests including a byte-level gate proving each `saio` offset lands on its fragment's first IV
- Round 396 — CENC `seig` key-rotation signalling on write (ISO/IEC 23001-7 §6 + ISO/IEC 14496-12 §8.9.4): new `cenc::build_seig_entry` (byte-exact inverse of `parse_seig`, with §6 round-trip validation: 4-bit pattern ceilings, §9.1 IV-size set, constant-IV-presence coherence) and `FragmentedMuxer::write_protected_packet_grouped(packet, senc, Option<SeigEntry>)` mapping each protected sample to an optional per-sample-group override (the key-rotation channel). At flush, the distinct entries used by the fragment's samples are deduplicated in first-use order into one fragment-local `sgpd('seig')` (§8.9.3 traf container) and the per-sample mapping run-length-encoded into one `sbgp('seig')` with the §8.9.4 fragment-local index numbering (0x10001 = first local description, 0 = tenc defaults). Queue-time guard: a protected override may not change `Per_Sample_IV_Size` (the fragment's `senc` stores one IV width, §7.2.3). Tests: mid-fragment KID rotation, A/B/A/B two-key dedupe order, clear-samples group (`isProtected = 0`), IV-width-change rejection
- Round 396 — `CencFragmentPackager` (new `cenc_packager` module): plaintext-in, protected-fMP4-out write driver owning the crypto state the container layer doesn't hold — the KID-keyed AES-128 content-key store, per-sample IV generation (per-track 64-bit counter in IV bytes 0..8 so §9.3 CTR counter blocks never collide across samples under one key, honouring §9.4.2 unique-IV-per-sample), and the active `seig` override. `write_packet` (§9.4/§9.7 full-sample), `write_packet_with_subsamples` (§9.5 subsample/pattern, clear prefixes preserved), `rotate_key(kid, key, constant_iv?)` (constant-IV schemes take a fresh constant IV per key; per-sample-IV schemes must not; rotating back to the default KID clears the override), `reset_to_default_key`, `set_next_segment_pssh` delegate (§8.1.1 moof-scoped licence blobs). Unprotected sibling streams pass through to the plain write path
- Round 396 — packager→demux→decrypt round-trip gate on every §10 scheme × fragmented shape: `cenc` full-sample (8/16-byte IVs) + subsample, `cbc1` full-sample (§9.4.3 clear tail) + subsample, `cens` 2:1 CTR pattern, `cbcs` 1:9 CBC pattern under a constant IV (zero-length senc IVs) + the §9.7 whole-block full-sample shape (no senc/saiz/saio in the file at all — decrypt from `tenc` alone), plus a key-rotation loop whose decrypt side resolves each sample's KID purely from the file (sbgp('seig') run-length → fragment-local sgpd entry → `parse_seig` → KID-keyed store). 9 + 10 new integration tests across `tests/cenc_frag_decrypt.rs` / `tests/cenc_packager.rs`
- Round 396 — `EveryKeyframe` final-sample fix: with `FragmentCadence::EveryKeyframe`, `write_trailer`'s final flush used to run the cadence detach — popping the trailing keyframe "for the next fragment" that never comes — silently dropping the stream's last sample (plain and protected paths alike). The final flush now skips the detach (`flush_fragment_inner(detach_trailing_keyframe)`); regression test proves every written packet demuxes back byte-exact. Plus CENC×index interplay gates: protected fragments under full DASH chrome (sidx + styp + mfra) keep the moof-relative `saio` byte gate intact; two protected tracks in one moof patch independent `saio` offsets against the shared moof origin; the packager rotation workflow with a moof-scoped `pssh` decrypts across both key epochs

- Round 389 — moof-level `pssh` emission (ISO/IEC 23001-7 §8.1.1): new `FragmentedMuxer::set_next_segment_pssh(iter)` queues one or more `pssh` boxes to be written *inside* the next fragment's `moof` (after `mfhd`, before the `traf` boxes), scoping a DRM header to that fragment's samples for per-fragment key rotation — the movie-fragment counterpart to the moov-level `Mp4MuxerOptions::pssh` init-segment boxes. Threaded through `build_moof` / `build_moof_inner` (the box size folds into the enclosing `sidx` `referenced_size` since `moof_size` is derived from the built bytes). Consumed per fragment; serialised through `build_pssh_box`. The demuxer already surfaces these on `moof_pssh_<n>` keyed by `mfhd.sequence_number`. One integration test driving `open_fragmented_typed`, attaching a v1 moof `pssh` to the first fragment and asserting it surfaces on exactly that fragment
- Round 389 — CENC protected-track muxing (ISO/IEC 14496-12 §8.12 + ISO/IEC 23001-7 §4.1 / §8.1): new `Mp4MuxerOptions::track_protection` (`TrackProtection { stream_index, scheme_type, scheme_version, tenc }`) wraps a muxed track's sample entry into its protected `encv` / `enca` / `enct` / `encs` form with a `sinf`(`frma` + `schm` + `schi`/`tenc`) envelope, serialised through the new public `cenc::build_sinf_box` (validated at `open` via `CencSchemeDecision::new` + `build_tenc_box` round-trip rules — an envelope this crate's own demuxer would reject cannot be emitted, and an incoherent scheme/tenc pair fails at `open`, never at write time). New `Mp4MuxerOptions::pssh` emits moov-level `pssh` boxes after the `trak` boxes (fragmented mode: in the init-segment `moov` after `mvex`), serialised through `build_pssh_box`. Applies to both the plain and fragmented muxers. `TrackProtection` / `TrackSampleGroups` re-exported at crate root. Three integration tests: a full black-box encrypt→mux→demux→decrypt loop (plaintext packets encrypted per-sample via `encrypt_sample_in_place`, muxed as a protected `enca` track + v1 `pssh`, demuxed back asserting `protection_scheme` / `cenc_default_*` / `pssh_0` and ciphertext-verbatim packets, then decrypted byte-exact via `decrypt_sample_in_place`), a fragmented init-segment protection surface check, and the incoherent-pair open failure
- Round 389 — CENC write-side foundations (ISO/IEC 23001-7:2016 §7.2 / §8.1 / §8.2 / §9): new public byte-exact builders `cenc::build_tenc_box` / `build_pssh_box` / `build_senc_box` — each emits a complete `[size][fourcc]` box whose body is the inverse of the matching parser (`parse_tenc` / `parse_pssh` / `parse_senc`), with round-trip validation (a v0 `tenc` carrying a pattern pair, a >4-bit pattern component, an IV size outside {0, 8, 16}, or a constant IV whose presence disagrees with the §9.1 `isProtected`/`IV_size` rule is rejected; a v0 `pssh` with KIDs or >u32 counts is rejected; a `senc` with mixed per-sample IV widths, >u16 subsample counts, subsamples under a cleared `UseSubSampleEncryption` flag, or >24-bit flags is rejected). New write-side cipher duals `cenc_cipher::encrypt_sample_in_place` / `encrypt_steps_in_place`: the same `plan_sample_cipher` partition and §9.2 IV-supply discipline as the decrypt path, executed forward — CTR is the identical §9.3 keystream XOR, CBC encrypts with the same per-`cbcs`-subsample IV restart, §9.5.1 chain continuity, and §9.4.3 whole-block discipline — so encrypt→decrypt with identical arguments is the identity. 9 builder round-trip/rejection unit tests + 5 encryptor tests (all-schemes encrypt→decrypt identity across a two-subsample shape, first-principles CTR keystream equality, reference-CBC chain equality with clear tail, cbcs constant-IV per-subsample restart producing identical ciphertext halves, shared malformed-plan guards)
- Round 389 — muxer codec write-side coverage ×12 (ISO/IEC 14496-12 §8.5.2 / §12.1.3 / §12.2.3 + ISO/IEC 14496-15 §8.4 + ISO/IEC 14496-1 §7.2.6): `sample_entries::sample_entry_for` now packages every remaining codec id the demuxer's sample-entry table resolves that has a well-defined write shape — video `h265` → `hvc1`+`hvcC`, `av1` → `av01`+`av1C`, `vp9`/`vp8` → `vp09`/`vp08`+`vpcC` (config record = extradata verbatim, the demuxer's surfaced form; missing extradata is rejected at `open`), `h263` → `s263` with a `d263` config child when extradata is present; audio `opus` → `Opus`+`dOps` (the demuxer-prepended `OpusHead` magic is stripped on write and re-prepended on read — byte-exact both ways), `alac` → `alac` entry + `alac` magic-cookie FullBox child, `ac3` → `ac-3`+`dac3` and `eac3` → `ec-3`+`dec3` (config body verbatim), `mp3` → `mp4a` with an `esds` carrying `objectTypeIndication = 0x6B` and no DecoderSpecificInfo (the demuxer's OTI refinement resolves it back to `mp3`), `pcm_mulaw`/`pcm_alaw` → `ulaw`/`alaw` plain 8-bit AudioSampleEntries. The AAC `esds` assembly is refactored onto a shared `build_esds_body(oti, dsi)` used by both `aac` and `mp3`. Demux-side symmetry additions: `parse_video_sample_entry` now surfaces a `d263` config-box body as extradata and `parse_audio_sample_entry` surfaces the `alac` magic cookie (post-version/flags bytes, mirroring the `dfLa` convention). 10 new sample-entry unit tests + 2 table-driven integration tests (`tests/mux_roundtrip.rs`) remuxing all 12 codecs mux → demux with byte-exact packet and extradata round-trip assertions

- Round 379 — HEIF item-properties timestamps (`crtt` / `mdft`, ISO/IEC 23008-12 §6.5.18 / §6.5.19): two more typed `demux::ItemProperty` variants. `crtt` CreationTimeProperty and `mdft` ModificationTimeProperty each carry a 64-bit time in microseconds since 1904-01-01 UTC, decoded by `parse_ipco_box` / `parse_item_property` and re-emitted byte-exact by `build_item_property`. Surfaced in the `meta_iprp_property_<n>` summary as `crtt <us>` / `mdft <us>`. Two new round-trip unit tests. Two new FourCC consts in `boxes` (`CRTT` / `MDFT`) (ISO/IEC 23008-12 §6.5)
- Round 379 — HEIF item-properties extended set (`udes` / `altt` / `iscl` / `rref`, ISO/IEC 23008-12 §6.5.13 / §6.5.17 / §6.5.20 / §6.5.21): four more typed `demux::ItemProperty` variants beyond the base round-379 set. `udes` UserDescriptionProperty (§6.5.20) carries four NUL-terminated UTF-8 strings (`lang` RFC 5646 tag, `name`, `description`, comma-separated `tags`, any may be empty); `altt` AccessibilityTextProperty (§6.5.21) an HTML-`alt`-style `alt_text` plus its `alt_lang`; `iscl` ImageScaling (§6.5.13) the horizontal / vertical scaling ratios as 16-bit numerator/denominator pairs (a transformative property); `rref` RequiredReferenceTypesProperty (§6.5.17) the list of `iref` reference-type FourCCs a reader must understand to decode the item (e.g. `pred` for a predictively-coded image). Each is decoded by `parse_ipco_box` / `parse_item_property` and re-emitted byte-exact by `build_item_property`; a property truncated for its declared type (e.g. an `rref` whose count overruns the body) falls back to `ItemProperty::Other` so its `ipco` index slot survives. The `meta_iprp_property_<n>` summary tokens cover the new types (`udes [<lang>] <name>`, `altt <alt_text>`, `iscl <wn>/<wd>x<hn>/<hd>`, `rref <type,…>`). Six new round-trip unit tests (udes full / empty, altt, iscl, rref full + empty, rref overrun-to-Other). Four new FourCC consts in `boxes` (`UDES` / `ALTT` / `ISCL` / `RREF`) (ISO/IEC 23008-12 §6.5)
- Round 379 — HEIF item-catalogue flat-channel enrichment (ISO/IEC 14496-12 §8.11.3 / §8.11.12): the file-level `meta` box's already-parsed `iloc` and `iref` records now surface per-entry detail on `Demuxer::metadata()`, not just a count. `meta_iloc_<n>` (`id=<item_id> method=<construction_method> extents=<n> length=<total>`) lets a HEIF consumer spot where each item lives — file (0) / idat (1) / item (2) offset, extent count, and total byte length — without downcasting to `MetaItems::iloc`. `meta_iref_<n>` (`type=<fourcc> from=<item_id> to=<id,…>`) exposes the relationship graph (thumbnail `thmb`, auxiliary `auxl`, derivation `dimg`, description `cdsc`, pre-derived `base`, predictive `pred`, tile-base `tbas`, scalable-base `exbl`, …) so the item relationships are queryable from the flat channel. The existing `meta_iloc_count` / `meta_iref_count` keys are unchanged; absent the boxes no keys are emitted. Catalogue integration test extended with `meta_iloc_0` / `meta_iloc_1` / `meta_iref_0` assertions (ISO/IEC 14496-12 §8.11)
- Round 379 — HEIF / MIAF entity-grouping family (`grpl` / `EntityToGroupBox`, ISO/IEC 23008-12 §9.4) read + write: a file-level `meta` box's `grpl` GroupsListBox is now decoded into the new public `demux::EntityGroups` record — every `EntityToGroupBox` (each a FullBox whose FourCC is the `grouping_type`: `altr` alternatives, `ster` stereo pair, …) becomes a typed `demux::EntityToGroup` carrying the `grouping_type`, `group_id`, and the `entity_id` list (item / track IDs). Reachable via `MetaItems::grpl`; `EntityGroups::by_type(grouping_type)` filters (e.g. all `altr` sets) and `EntityGroups::by_id(group_id)` locates a group. Public byte-exact builders `demux::build_grpl_box` / `build_entity_to_group_box` are the inverses of `parse_grpl_box` (FullBox version 0, flags 0 per group). An `EntityToGroupBox` whose `num_entities_in_group` overruns its body is dropped (contributes no group) without aborting the `grpl` walk. The demuxer surfaces `meta_grpl_group_count` plus `meta_grpl_group_<n>` (`type=<fourcc> id=<id> entities=<id,…>`) on `Demuxer::metadata()`. Absent `grpl`, no keys are emitted. Six standalone round-trip unit tests (`altr` / `ster` / multi-group + index helpers / empty group / empty `grpl` / malformed-entity drop) + one end-to-end demuxer test surfacing the `meta_grpl_*` keys. New FourCC const `GRPL` in `boxes` (ISO/IEC 23008-12 §9.4)
- Round 379 — HEIF / MIAF item-properties family (`iprp` / `ipco` / `ipma`, ISO/IEC 23008-12 §9.3) read + write: a file-level `meta` box's `iprp` ItemPropertiesBox is now decoded into the new public `demux::ItemProperties` record — the `ipco` ItemPropertyContainerBox property list (implicitly 1-indexed) plus every `ipma` ItemPropertyAssociation box merged into per-item `(essential, property_index)` lists. Reachable via `MetaItems::iprp`; `ItemProperties::properties_for(item_id)` resolves an item to its ordered `(essential, &ItemProperty)` pairs (skipping index-0 / out-of-range references), and `ItemProperties::property(index)` looks up the 1-based `ipco` slot. The typed `demux::ItemProperty` enum models the base property set: `ispe` ImageSpatialExtents (§6.5.3), `pixi` PixelInformation (§6.5.6), `rloc` RelativeLocation (§6.5.7), `auxC` AuxiliaryType (§6.5.8, NUL-terminated URN + subtype tail), `irot` ImageRotation (§6.5.10), `imir` ImageMirroring (§6.5.12), `lsel` LayerSelector (§6.5.11), plus `pasp` / `clap` / `colr` reusing this crate's existing 14496-12 records (§6.5.4 / §6.5.9 / §6.5.5); any unrecognised property box is preserved verbatim as `ItemProperty::Other` so its index slot survives a round-trip. Reserved high bits on `irot` / `imir` are masked on read. Public byte-exact builders `demux::build_iprp_box` / `build_ipco_box` / `build_ipma_box` / `build_item_property` are the inverses of the parsers (`parse_iprp_box` / `parse_ipco_box` / `parse_ipma_box`); `build_ipma_box` auto-selects the narrowest item-ID width (v0 16-bit / v1 32-bit) and `property_index` width (`flags & 1` 7-/15-bit), sorts entries by ascending `item_ID` (§9.3.1), and rejects an index past the 15-bit ceiling. The demuxer surfaces a compact summary on `Demuxer::metadata()`: `meta_iprp_property_count`, `meta_iprp_property_<n>` (a `<fourcc> <decoded>` token), and `meta_iprp_item_<n>` (`id=<id> props=<idx[,idx*…]>`, `*` marking an essential association). Absent `iprp`, no keys are emitted. 19 standalone round-trip unit tests (every property box, narrow / wide-index / wide-id / sorted / truncated `ipma`, unknown-property index preservation, `properties_for` resolution) + one end-to-end demuxer test surfacing the `meta_iprp_*` keys. Ten new FourCC consts in `boxes` (`IPRP` / `IPCO` / `IPMA` / `ISPE` / `PIXI` / `RLOC` / `AUXC` / `IROT` / `IMIR` / `LSEL`) (ISO/IEC 23008-12 §9.3 / §6.5)

- Round 375 — `saiz` / `saio` (Sample Auxiliary Information Sizes / Offsets, ISO/IEC 14496-12 §8.7.8 / §8.7.9) public parse + build surface: the demuxer's internal `saiz` / `saio` parse paths are now exposed as public `demux::parse_saiz_box` / `parse_saio_box` plus new byte-exact `demux::build_saiz_box` / `build_saio_box`, and the carried records `demux::SaizBox` / `SaioBox` are now public with public fields. `build_saiz_box` writes the `(aux_info_type, aux_info_type_parameter)` key only when present (setting `flags & 1`), then the 8-bit `default_sample_info_size`, 32-bit `sample_count`, and — for a variable-size table — the per-sample size array (§8.7.8.2); it rejects a constant-size record carrying a per-sample table or a variable table whose `sample_count` disagrees with `per_sample.len()`. `build_saio_box` writes the same optional aux key then the 32-bit `entry_count` and the offset array as 32-bit (v0) / 64-bit (v1) fields (§8.7.9.2); it rejects an undefined version or a v0 offset above `u32::MAX`. CENC/DRM tooling can now decode and re-emit the auxiliary-info size/offset tables (the `senc`-companion boxes) without re-running `open()`. Five round-trip unit tests (saiz constant + variable-with-key + inconsistent rejection; saio v0/v1 + inconsistent rejection) (ISO/IEC 14496-12 §8.7.8 / §8.7.9)
- Round 375 — `subs` (SubSampleInformationBox, ISO/IEC 14496-12 §8.7.7) public parse + build surface: the demuxer's internal `subs` parse path is now exposed as public `demux::parse_subs_box` plus a new byte-exact `demux::build_subs_box`, and the carried records `demux::SubsBox` / `SubsEntry` / `SubSampleEntry` are now public with public fields. `build_subs_box` serialises the FullBox preamble (record `version` 0/1 + codec-owned 24-bit `flags` verbatim), the 32-bit `entry_count`, and per entry the sparse `sample_delta`, 16-bit `subsample_count`, and per sub-sample the `subsample_size` (16-bit at v0, 32-bit at v1), `subsample_priority`, `discardable` and `codec_specific_parameters` (§8.7.7.2). Rejects records that cannot round-trip: an undefined version, an `entry_count` / `subsample_count` exceeding its on-wire field, or — at version 0 — a `subsample_size` above `u16::MAX`. A codec-aware layer (e.g. an AVC NAL splitter reading `codec_specific_parameters`) can now both decode and re-emit a `subs` table without re-running `open()`. Four round-trip unit tests (v0 multi-entry, v1 wide size, empty + degenerate zero-count, inconsistent-record rejection) (ISO/IEC 14496-12 §8.7.7)
- Round 375 — `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12 §8.16.5) standalone builder: new public `demux::build_prft_box` serialises a `PrftRecord` into a `prft` box — the byte-exact inverse of `parse_prft_box`. The FullBox preamble carries the record's `version` (0/1) and its 24-bit `flags` verbatim (the 2022-edition NTP-annotation bits), then the 32-bit `reference_track_ID`, the 64-bit NTP timestamp, and `media_time` as a 32-bit field for v0 / 64-bit for v1 (§8.16.5.2). Rejects a v0 record whose `media_time` exceeds `u32::MAX` (would not round-trip — bump to v1) or an undefined version. This complements the fragmented muxer's existing per-segment `prft` emission with a stateless box builder usable outside the writer. Three round-trip unit tests (v0 with flags, v1 64-bit media_time, inconsistent-record rejection) (ISO/IEC 14496-12 §8.16.5)
- Round 375 — `pdin` (ProgressiveDownloadInfoBox, ISO/IEC 14496-12 §8.1.3) builder: new public `demux::build_pdin_box` serialises a `PdinRecord` into a `pdin` box — the byte-exact inverse of `parse_pdin_box` (FullBox version 0, flags 0). The body is the 4-byte preamble followed by one big-endian `(rate, initial_delay)` u32 pair per entry (§8.1.3.2), written in supplied order (the builder does not re-sort by ascending `rate`). An empty entry list yields a preamble-only box. Two round-trip unit tests (two-entry table, empty) (ISO/IEC 14496-12 §8.1.3)
- Round 375 — `trgr` (TrackGroupBox, ISO/IEC 14496-12 §8.3.4) builder: new public `demux::build_trgr_box` serialises a list of `(track_group_type, track_group_id)` pairs into a `trgr` container box — the byte-exact inverse of `parse_trgr`. The outer `trgr` is a plain container; each pair becomes one `TrackGroupTypeBox` whose FourCC is the `track_group_type` and whose body is the 4-byte FullBox preamble (version 0, flags 0) + the 32-bit `track_group_id` (§8.3.4.2). Pairs are emitted in supplied order; the per-`track_group_type` extension tail is not modelled. Two round-trip unit tests (multi-type, empty set) (ISO/IEC 14496-12 §8.3.4)
- Round 375 — track-`udta` selection-box builders (`cprt` / `kind` / `tsel`, ISO/IEC 14496-12 §8.10): new public `demux::build_cprt_box` / `build_kind_box` / `build_tsel_box` are the write counterparts to the existing parse path. `build_cprt_box` serialises a CopyrightBox (§8.10.2): FullBox v0 + the 16-bit packed `language` word (three 5-bit ISO 639-2/T characters, each `ASCII - 0x60`) + the NUL-terminated UTF-8 notice — rejecting a non-lowercase language byte (unrepresentable in 5 bits) or an embedded NUL in the notice. `build_kind_box` serialises a KindBox (§8.10.4): FullBox v0 + the `schemeURI` and `value` NUL-terminated UTF-8 strings (value terminator always written, even when empty) — rejecting an empty URI (§8.10.4.3) or an embedded NUL. `build_tsel_box` serialises a TrackSelectionBox (§8.10.3): FullBox v0 + signed `switch_group` + the FourCC `attribute_list`. Each is the byte-exact inverse of its parser. Seven round-trip unit tests (cprt full / empty / bad-input rejection, kind full / empty-value / bad-input rejection, tsel signed + attributes) (ISO/IEC 14496-12 §8.10)
- Round 375 — Sub-track (`strk` / `stri` / `strd` / `stsg`, ISO/IEC 14496-12 §8.14) builders: new public `demux::build_stsg_box` / `build_stri_box` / `build_strk_box` are the write counterparts to the existing parse path. `build_stsg_box` serialises a SubTrackSampleGroupBox (§8.14.6, FullBox v0: `grouping_type` FourCC + 16-bit `item_count` + the `group_description_index` array; rejects an index list exceeding the 16-bit count). `build_stri_box` serialises a SubTrackInformationBox (§8.14.4, signed `switch_group` / `alternate_group`, `sub_track_ID`, then the FourCC `attribute_list` to the end of the box). `build_strk_box` composes a complete SubTrackBox (§8.14.3): the mandatory `stri` followed — when any sample groups are supplied — by a single `strd` (SubTrackDefinitionBox, §8.14.5) wrapping one `stsg` per `(grouping_type, indices)` pair; with no sample groups the content-free `strd` is omitted. Each builder is the byte-exact inverse of its parser. Five round-trip unit tests (stsg full + empty indices, stri with signed groups + attribute list, full strk re-parse, stri-only strk omits strd) (ISO/IEC 14496-12 §8.14)
- Round 375 — `tref` (TrackReferenceBox, ISO/IEC 14496-12 §8.3.3) builder: new public `demux::build_tref_box` serialises a list of `(reference_type, track_IDs)` pairs into a `tref` container box — the byte-exact inverse of `parse_tref`. The outer `tref` is a plain container (no FullBox preamble); each pair becomes one `TrackReferenceTypeBox` whose FourCC is the `reference_type` and whose body is the packed big-endian `track_ID` array (§8.3.3.2). Pairs are emitted in supplied order; a zero `track_ID` is rejected (§8.3.3.3 "never zero" — it would be silently dropped by the parser, breaking round-trip), while an empty `track_IDs` array for a type is permitted. Four round-trip unit tests (multi-type, empty-id array, zero-id rejection, empty set) (ISO/IEC 14496-12 §8.3.3)
- Round 372 — `hinf` Hint Statistics Box (ISO/IEC 14496-12 §9.1.5) parse + build: a container in a hint track's `udta` holding optional delivery-statistic sub-boxes summarising the packetised stream a server would generate. New public `hint::HintStatistics` record (with `MaxRate` / `PayloadId` helpers) + byte-exact `parse_hinf_box` / `build_hinf_box` covering all §9.1.5 sub-boxes: `trpy` / `nump` / `tpyl` (u64 bytes-with-RTP / packets / bytes-without-RTP), the `totl` / `npck` / `tpay` u32 variants, zero-or-more `maxr` max-rate windows (period + bytes), `dmed` / `dimm` / `drep` (media / immediate-mode / repeated bytes, u64), `tmin` / `tmax` (signed relative-time extremes, ms), `pmax` / `dmax` (largest-packet bytes / longest-packet ms), and zero-or-more `payt` payload-ID + length-prefixed rtpmap entries. Every field is `None` / empty when its sub-box is absent ("not all these sub-boxes may be present", §9.1.5); the builder emits only present fields, in declaration order. The track-`udta` walk now decodes the first `hinf` into `Track::hint_stats` and surfaces `hinf_bytes_sent` / `hinf_packets_sent` / `hinf_payload_bytes` / `hinf_media_bytes` / `hinf_immediate_bytes` / `hinf_repeated_bytes` / `hinf_largest_packet` / `hinf_maxr_count` / `hinf_payload_count` on `params.options` (the u32 variants fall back to the u64 forms; absent keys omitted). Four `hint` module round-trip tests (full, partial, empty, multi-`maxr`) + three `demux` tests (`parse_track_udta` pickup, surfacing, absence). New FourCC const `HINF` in `boxes` (ISO/IEC 14496-12 §9.1.5)
- Round 372 — `rtcp` reception + MPEG-2 TS (`sm2t` / `rm2t`) hint sample entries (ISO/IEC 14496-12 §9.4.2.3 / §9.3.3.2): the RTCP reception hint sample entry (`rtcp`) is structurally identical to `rtp ` (§9.4.2.3, no defined additional-data boxes), so it now routes through the same `hint::RtpHintSampleEntry` parse path and `Track::rtp_hint` surface. The MPEG-2 Transport Stream hint sample entries — `sm2t` (server) and `rm2t` (reception), §9.3.3.2 — get a new public `hint::Mpeg2TsHintSampleEntry` record + byte-exact `parse_mpeg2ts_hint_sample_entry` / `build_mpeg2ts_hint_sample_entry`: the MPEG2TSSampleEntry body (`hinttrackversion` / `highestcompatibleversion` + `precedingbyteslen` / `trailingbyteslen` per-TS-packet wrapper byte counts + the `precomputed_only_flag` top bit, with the 7 reserved low bits masked, then opaque `additionaldata` bytes preserved verbatim). The `stsd` walk dispatches `sm2t` / `rm2t` into `Track::mpeg2ts_hint` and surfaces `m2t_hint_format` / `m2t_hint_preceding_bytes` / `m2t_hint_trailing_bytes` / `m2t_hint_precomputed` on `params.options` — a de-hinter strips the wrapping bytes before reassembling the 188-byte TS. Four `hint` module round-trip tests (rtcp uses rtp body, sm2t server, rm2t with additionaldata, truncation rejection) + two `demux` `build_stream_info` tests. New FourCC consts `RTCP_HINT`/`SM2T`/`RM2T` in `boxes` (ISO/IEC 14496-12 §9.4.2.3 / §9.3.3.2)
- Round 372 — RTP / SRTP / reception hint sample-entry family (ISO/IEC 14496-12 §9.1.2 / §9.4.1.2) parse + build in a new `hint` module: the sample-description entry format for an RTP server hint track (`rtp `), an SRTP secure hint track (`srtp`), and an RTP reception hint track (`rrtp`) — all sharing one body (`hinttrackversion`, `highestcompatibleversion`, `maxpacketsize`, then an `additionaldata` box set). New public `hint::RtpHintSampleEntry` record + byte-exact `parse_rtp_hint_sample_entry` / `build_rtp_hint_sample_entry` decoding the §9.1.2 additional-data boxes: `tims` (timescale entry, required), `tsro` (signed time offset, optional), `snro` (signed sequence offset, optional), and — for `srtp` — the `srpp` SRTPProcessBox (§9.1.2.1, the four SRTP encryption/integrity algorithm 4CCs plus a verbatim `schm`/`schi` tail, modelled as `hint::SrppBox` with its own `parse_srpp_box` / `build_srpp_box`). The demuxer's `stsd` walk dispatches a hint track's active entry (hint handler ⇒ `MediaType::Data`, so dispatched by FourCC) into `Track::rtp_hint` and surfaces `rtp_hint_format` / `rtp_hint_max_packet_size` / `rtp_hint_timescale` / `rtp_hint_time_offset` / `rtp_hint_sequence_offset` (signed; offset keys omitted when absent) / `rtp_hint_srtp` on `params.options`. A streaming server reconstructs the RTP packetisation parameters without re-walking `stsd`. Six `hint` module round-trip unit tests (tims-only, all-offsets, srtp+srpp, srpp empty-tail, truncated-preamble rejection, unknown-additional-data tolerance) + three `demux` `build_stream_info` tests (rtp surfacing, srtp marker, absence). New FourCC consts `RTP_HINT`/`SRTP_HINT`/`RRTP_HINT`/`TIMS`/`TSRO`/`SNRO`/`SRPP` in `boxes` (ISO/IEC 14496-12 §9.1.2 / §9.1.2.1 / §9.4.1.2)
- Round 372 — `meco` / `mere` Additional Metadata Container family (ISO/IEC 14496-12 §8.11.7 / §8.11.8) demux: a file-level `meco` AdditionalMetadataContainerBox holds one or more additional `meta` boxes (each with a handler type distinct from the primary `meta`) that complement it, plus zero or more `mere` MetaboxRelationBoxes describing how two same-level `meta` boxes relate. The top-level walk now parses the first file-level `meco` into a new public `demux::MecoBox` (`metas: Vec<MetaItems>` — each additional `meta` fully decoded through `parse_meta_items`, capturing its handler type + any §8.11 item infrastructure — and `relations: Vec<MereRelation>`). `MereRelation` carries the two 32-bit handler-type codes and the §8.11.8.3 relation enum (1 unknown … 5 second-is-subset-of-first), preserved verbatim. New public stateless parsers `demux::parse_meco_box` / `parse_mere_box` + builder `demux::build_mere_box` (byte-exact inverse; the `meco` body is a plain box sequence so no dedicated `meco` builder is needed). Records reachable via the new `Mp4Demuxer::meco()` accessor; a flat summary is surfaced on `Demuxer::metadata()` as `meco_meta_count` / `meco_meta_<n>` (handler FourCC) / `meco_relation_count` / `meco_relation_<n>` (`"<first>-<second>=<relation>"`). `MetaItems` now derives `PartialEq`/`Eq`. Four `demux` unit tests (mere round-trip + truncation, meco walk collecting 2 metas + 1 relation, empty body) + one `tests/meta_items.rs` integration case (file-level `meco` spliced after `ftyp`, real audio track undisturbed) (ISO/IEC 14496-12 §8.11.7 / §8.11.8)
- Round 372 — FD (File Delivery) Item Information family (ISO/IEC 14496-12 §8.13) parse + build in a new `fd` module: the §8.13 boxes that record how a source file carried in a top-level `meta` is partitioned into source blocks and FEC/File reservoirs for ALC/LCT / FLUTE transmission. New public typed records + byte-exact parser/builder pairs for `fpar` FilePartitionBox (§8.13.3, v0/v1 16-/32-bit `item_ID`/`entry_count`, the FEC-OTI scheme-specific NULL-terminated string, and the `(block_count, block_size)` partitioning list), `fecr` FECReservoirBox + `fire` FileReservoirBox (§8.13.4 / §8.13.7, the shared versioned `(item_ID, symbol_count)` list — one `parse_reservoir_box`/`build_reservoir_box` pair serves both, dispatched by FourCC), `paen` PartitionEntry (§8.13.2, one mandatory `fpar` + optional `fecr` + optional `fire`), `segr` FDSessionGroupBox (§8.13.5, session groups each carrying a file-group-ID set + an FD-hint-track-ID channel list), `gitn` GroupIdToNameBox (§8.13.6, `(group_ID, group_name)` UTF-8 mappings), `fiin` FDItemInformationBox (§8.13.2, the partition-entry array + optional `segr` + optional `gitn`; the parser walks the actual `paen` children rather than trusting the leading `entry_count`, so a corrupt count cannot desync the walk), and `feci` FECInformationBox (§9.2.4.7, the fixed 7-byte FEC encoding/instance/source-block/symbol descriptor carried in an FD hint sample's `extr`). The file-level `meta` walk now decodes a `fiin` into `MetaItems::fiin` and surfaces `meta_fiin_partitions` / `meta_fiin_session_groups` / `meta_fiin_group_names` on `Demuxer::metadata()`. 14 `fd` module round-trip unit tests + one `demux` integration test driving a `meta`→`fiin` through `parse_meta_items`. New FourCC consts `MECO`/`MERE`/`FIIN`/`PAEN`/`FPAR`/`FECR`/`SEGR`/`GITN`/`FIRE`/`FECI` in `boxes` (ISO/IEC 14496-12 §8.13 / §9.2.4.7)
- Round 369 — `pasp` / `clap` / `colr` box builders (ISO/IEC 14496-12 §12.1.4–5): public `demux::build_pasp_box` / `build_clap_box` / `build_colr_box` serialise the picture-geometry / colour descriptor boxes, each the byte-exact inverse of its parser (an `nclx` box's seven reserved low bits are written as 0, matching the parser's mask; `rICC` / `prof` / `Other` colour types preserve their raw payload). Completes parse/build symmetry for the VisualSampleEntry descriptor family. One round-trip unit test covering pasp, clap, and all four `colr` variants (ISO/IEC 14496-12 §12.1.4 / §12.1.5)
- Round 369 — VisualSampleEntry `pasp` / `clap` / `colr` boxes (ISO/IEC 14496-12 §12.1.4–5) demux: the video sample-entry sub-box walk previously decoded `amve` / `btrt` / `sinf` but skipped the three core picture-geometry / colour descriptor boxes. It now parses `pasp` (PixelAspectRatioBox §12.1.4, `hSpacing` / `vSpacing`), `clap` (CleanApertureBox §12.1.4, the eight-u32 width/height/horiz-off/vert-off `N/D` fractions) and `colr` (ColourInformationBox §12.1.5, the `colour_type`-discriminated payload — `nclx` on-screen colours with the three ISO/IEC 23091-2 16-bit codes + `full_range_flag`, `rICC` / `prof` ICC profiles kept as raw bytes, and an `Other` fall-back preserving an unknown type + payload). Typed `PaspRecord` / `ClapRecord` / `ColrRecord` records on the track; first one on the active entry wins (the spec permits several `colr`, most-accurate-first). Surfaced on `params.options` as `pasp_h_spacing` / `pasp_v_spacing`; `clap_width` / `clap_height` / `clap_horiz_off` / `clap_vert_off` (each `"<N>/<D>"`); and `colr_type` plus, for `nclx`, `colr_primaries` / `colr_transfer` / `colr_matrix` / `colr_full_range`, or for an ICC profile `colr_icc_len`. New public standalone parsers `demux::parse_pasp_box` / `parse_clap_box` / `parse_colr_box`. Six new unit tests (body parsing of all three incl. nclx/prof/Other + short-body rejection; sample-entry pickup; `params.options` surfacing). A renderer or HDR-aware pipeline now reads the display aspect, crop rectangle, and colour signalling straight from the container without touching the codec bitstream (ISO/IEC 14496-12 §12.1.4 / §12.1.5)
- Round 369 — §8.11 meta-item box builders (write counterparts to the demux parsers): new public `demux::build_iloc_box` / `build_pitm_box` / `build_iinf_box` / `build_iref_box` / `build_idat_box` serialise the HEIF/MIAF item infrastructure, each the byte-exact inverse of its parser so a `parse_*` record re-emits identically. `build_iloc_box` honours the record's field-width selectors verbatim (so an arbitrary v0/v1/v2 `IlocBox` round-trips) and rejects records that would not round-trip — a `*_size` outside {0,4,8}, a version-0 item carrying a non-zero `construction_method` (the field is absent on the wire for v0), or `index_size > 0` on a version-0 box. `build_pitm_box` auto-selects version 0 (16-bit) / 1 (32-bit) by ID magnitude; `build_iinf_box` auto-selects the `entry_count` width and serialises each `infe` (versions 0/1/2/3, with the v2/3 `mime`/`uri ` tails) — rejecting a name/type string with an embedded NUL that would corrupt the framing; `build_iref_box` emits the typed `SingleItemTypeReferenceBox` groups at the record's 16-/32-bit width. Six new round-trip unit tests (iloc v0/v1/v2 + inconsistent-record rejection, pitm, mixed-version iinf, iref v0/v1, and a built-iloc+built-idat assembly read back through `item_data_from_idat`). Completes parse/build symmetry for the §8.11 item family (ISO/IEC 14496-12 §8.11.3 / §8.11.4 / §8.11.6 / §8.11.11 / §8.11.12)
- Round 369 — `idat` (ItemDataBox, ISO/IEC 14496-12 §8.11.11) capture + item-byte resolution: a file-level `meta`'s `idat` box bytes are now captured into `MetaItems::idat` and the §8.11.3.3 data-origin rules are honoured by new resolution helpers. `MetaItems::item_byte_ranges(item_id)` returns each extent's `(offset, length)` with the item's `base_offset` folded in (relative to the data origin selected by the item's `construction_method` — absolute file offset for method 0, `idat` offset for method 1). `MetaItems::item_data_from_idat(item_id)` materialises the concatenated bytes of an item whose `construction_method == 1` (idat-resident, e.g. a small Exif/XMP blob stored inline in the `meta`), with the §8.11.3.3 "entire source" (`extent_length == 0`, single extent) convention and bounds-checking that rejects any extent overrunning the `idat`. File-offset items (method 0) are deliberately not re-read from the input here (the demuxer does not re-read untimed items); their absolute ranges come from `item_byte_ranges`. Convenience `MetaItems::iloc_item(id)` / `item_info(id)` look up a single item's `iloc` location / `iinf` info by ID. `idat` length is surfaced on `Demuxer::metadata()` as `meta_idat_len`. Two new unit tests (idat resolution + lookups; overrun rejection) (ISO/IEC 14496-12 §8.11.11 / §8.11.3.3)
- Round 369 — file-level `meta` box item infrastructure (HEIF / MIAF item catalogue, ISO/IEC 14496-12 §8.11) demux: the top-level walk previously parsed a `meta` box only inside `moov`/`trak` for iTunes-style `ilst` metadata and ignored a *file-level* `meta` entirely — that is the home of the HEIF/MIAF still-image item catalogue. The opener now walks a file-level `meta` and decodes its four §8.11 child boxes into a new public `demux::MetaItems` record: `pitm` (PrimaryItemBox §8.11.4, primary `item_ID`, v0 16-bit / v1 32-bit), `iloc` (ItemLocationBox §8.11.3, the full v0/v1/v2 layout — the box-global `offset_size`/`length_size`/`base_offset_size`/`index_size` nibble selectors from the set {0,4,8}, per-item `construction_method` (file/idat/item) + `data_reference_index` + `base_offset`, and the per-extent `extent_index`/`extent_offset`/`extent_length` loop with v2's 32-bit `item_ID`/`item_count` widening), `iinf`/`infe` (ItemInfoBox §8.11.6, every `infe` version 0/1/2/3 — v0/1 `(name, content_type, content_encoding)` and v2/3's 32-bit `item_type` FourCC with `mime`/`uri ` tails), and `iref` (ItemReferenceBox §8.11.12, the typed `SingleItemTypeReferenceBox` groups, v0 16-bit / v1 32-bit `from`/`to` item IDs). The `meta`'s `hdlr` handler_type (e.g. `pict`) is captured too. New public stateless parsers `demux::parse_iloc_box` / `parse_pitm_box` / `parse_iinf_box` / `parse_iref_box` / `parse_meta_items` decode each box from its body; malformed/truncated boxes are dropped gracefully rather than aborting the open, and invalid field widths (a `*_size` outside {0,4,8}) are rejected. Records reachable via the new `Mp4Demuxer::meta_items()` accessor; a flat summary is surfaced on `Demuxer::metadata()` as `meta_handler` / `meta_primary_item` / `meta_item_count` / `meta_item_<n>` (`id=<id> type=<fourcc> name=<name>`) / `meta_iloc_count` / `meta_iref_count`. A plain iTunes `meta` (just `hdlr` + `ilst`) yields an empty `MetaItems` (`MetaItems::is_empty()`) and emits no item keys. New `tests/meta_items.rs` integration case (HEIF `meta` spliced after `ftyp`, two items + thumbnail `iref`, real audio track undisturbed) + 11 `src/demux.rs` unit tests covering iloc v0/v1/v2 + bad-width rejection, pitm v0/v1, iinf with infe v0/v2, iref v0/v1, and the full `parse_meta_items` assembly (ISO/IEC 14496-12 §8.11)

- Round 364 — fragment-local sample groups (`sgpd` / `sbgp` / `csgp` inside `traf`, §8.9.2 / §8.9.3 / §8.9.5) demux: the `traf` walk previously skipped sample-group boxes; it now parses each fragment's `sgpd` description table plus `sbgp` / `csgp` per-sample maps and collects them into a new public `demux::TrafSampleGroupRecord` keyed by `(track_idx, moof_sequence)`. This is where the `csgp` bit-7 `index_msb_indicates_fragment_local_description` flag is actually legal (§8.9.5) — a fragment can declare fragment-local group descriptions and map samples into either those or the `trak`-level global ones (most commonly CENC `seig` key rotation across a fragment's samples). A per-`traf` summary is surfaced via `Demuxer::metadata()` as `frag_sample_group_<n>` (`"track=<t> seq=<s> sgpd=<n> sbgp=<m> csgp=<k>"`); structured records reachable via the new `Mp4Demuxer::traf_sample_groups()` accessor. The demuxer-internal `SbgpBox` / `SgpdBox` parse structs are now public (their fields preserve on-wire values verbatim). Sample-group boxes don't disturb the `trun` sample walk. New `tests/fragmented.rs` integration case (metadata + sample-walk) + two `src/demux.rs` unit tests (structured-record values incl. fragment-local MSB resolution, and the no-groups path) (ISO/IEC 14496-12 §8.9 inside `traf`)
- Round 364 — `csgp` (CompactSampleToGroupBox, §8.9.5) non-fragmented mux emission: `TrackSampleGroups` gains a `csgp: Vec<sample_groups::CompactSampleToGroup>` field (empty by default) alongside the existing `sbgp`/`sgpd`. When non-empty, the muxer writes each track's `csgp` boxes into its `stbl` after the chunk-offset table and after any `sbgp` (so the `sgpd` description tables they reference are declared first). `sbgp` and `csgp` are alternative encodings of one per-sample → group mapping — §8.9.5 permits at most one form per `grouping_type`, so a caller picks one per type while distinct types may each pick their own form. This wires the already-public `sample_groups::build_csgp` builder into the muxer, closing the write half of `csgp` (the demuxer already parsed it via `parse_csgp_box`); a muxer-emitted `csgp` round-trips byte-exact through the demuxer's `csgp_<n>` metadata surface (grouping type, optional `param=`, optional `fraglocal` marker, per-pattern `count*i0,i1` tokens) including the width-code auto-selection, the `grouping_type_parameter`, and the fragment-local MSB flag. Default (empty `csgp`) output is byte-identical to before. New `tests/sample_groups_mux.rs` cases cover single-pattern, multi-pattern + `grouping_type_parameter`, `sbgp`/`csgp` coexistence on distinct grouping types, and fragment-local-flag round-trips (ISO/IEC 14496-12:2020 §8.9.5)
- Round 360 — `trep` (TrackExtensionPropertiesBox, §8.8.15) fragmented-mux emission: new `FragmentedOptions::treps: Vec<demux::TrepRecord>` (empty by default). When non-empty, the fragmented muxer writes one `trep` per record after the `trex` boxes (and after any `leva`) inside the init-segment `mvex`, each documenting characteristics of one track in the subsequent fragments (the one base-spec-defined child, `assp` §8.8.16, is serialised from its typed `AsspRecord` when present). Default (empty) output is byte-identical to before. New public `demux::build_trep_box(&TrepRecord)` builder — the write counterpart to `parse_trep_box`, serialising the FullBox preamble + `track_id` + children (a typed `assp` child via `build_assp_box`, any other child as an empty placeholder box; a non-`assp` child carrying an `AsspRecord`, or an opaque child with a non-zero `payload_len`, is rejected). A muxer-emitted `trep`+`assp` reads back through the demuxer's `mvex` walk (`trep_<n>` metadata). Closes the write/read symmetry for `trep` (ISO/IEC 14496-12 §8.8.15)
- Round 360 — Alternative Startup Sequence Properties Box (`assp`, §8.8.16) typed parse + builder: the one base-spec-defined `trep` (TrackExtensionPropertiesBox, §8.8.15) child is now decoded into a typed `AsspRecord` (carried on `TrepChild::assp`) instead of being recorded by type+length only. v0 yields one implied entry (signed `min_initial_alt_startup_offset`, no `grouping_type_parameter`); v1 yields a `num_entries`-long keyed list of `(grouping_type_parameter, min_initial_alt_startup_offset)` pairs (§8.8.16.1 ties the box version to the `alst` §10.3.2 `sbgp` version). Public `demux::parse_assp_box` / `demux::build_assp_box` round-trip byte-exact; the builder rejects records that would not round-trip (v0 with a `grouping_type_parameter`, v0 ≠ 1 entry, unsupported version). The flat `trep_<n>` metadata appends the offset(s) after the `assp` token (`assp(off=<i>)` for v0; `assp(<gtp>:<off> ...)` for v1). A malformed `assp` still contributes its type+length (`assp = None`) so the child list stays faithful to the wire; parse rejects truncated bodies, a v1 `num_entries` overrunning the body, and versions other than 0/1 (ISO/IEC 14496-12 §8.8.16)
- Round 358 — Stereo Video Box (`stvi`, §8.15.4.2) builder: public `demux::build_stvi_box(&StviRecord)` serialises a complete `[size]['stvi']` box (the §8.15.4.2.2 `FullBox(0,0)` body — packed `reserved(30)`/`single_view_allowed(2)` word, `stereo_scheme`, derived `length`, `stereo_indication_type` array) for a restricted-scheme muxer placing it inside a sample entry's `sinf/schi`. The byte-exact inverse of `parse_stvi_box`; only the low 2 bits of `single_view_allowed` are written (reserved bits zero). New `tests/stvi.rs` round-trips scheme-1 (frame packing) / scheme-3 (23000-11) / empty-indication shapes, pins the exact wire bytes, and covers short-body / overrunning-length rejection + trailing `any_box` tolerance (ISO/IEC 14496-12 §8.15.4.2)
- Round 358 — Stereo Video Box (`stvi`, §8.15.4.2) inside a sample entry's `sinf/schi`: typed `StviRecord` (`single_view_allowed`, `stereo_scheme`, raw `stereo_indication_type` bytes) with `right_view_monoscopic_allowed()` / `left_view_monoscopic_allowed()` accessors + public `demux::parse_stvi_box`. Recovered from the restricted-scheme `stvi` (stereoscopic video, §8.15.4.1) `schi` on both encrypted (`enc*`) and non-encrypted (sample-entry-resident `sinf`) video entries; first one wins. Surfaced on `params.options` as `stvi_single_view_allowed` / `stvi_stereo_scheme` (decimal) + `stvi_indication` (raw indication bytes as lowercase hex, omitted when empty). The first word's 30 reserved bits are masked off; a `length` that overruns the body is rejected, and trailing optional `any_box` bytes are ignored (ISO/IEC 14496-12 §8.15.4.2)
- Round 351 — `ssix` (SubsegmentIndexBox, §8.16.4) fragmented-mux emission: new `FragmentedOptions::emit_ssix` (default `false`) + `ssix_levels: (u8, u8)` (default `(1, 2)`). When `emit_ssix` is set (with `emit_random_access_indexes` on), the muxer writes an `ssix` immediately after each per-fragment `sidx`, carrying `subsegment_count == 1` (matching the one-reference `sidx`) and partitioning the fragment's single subsegment (`styp? + prft? + moof + mdat`) into two level byte ranges — the leading `styp? + prft? + moof` metadata range (`ssix_levels.0`) and the trailing `mdat` media range (`ssix_levels.1`) — that together cover every byte (§8.16.4 `range_count ≥ 2` + partition constraints hold by construction). The muxer-emitted `sidx`+`ssix` pair reads back through `parse_sidx_box` / `parse_ssix_box` with the ranges summing exactly to the `sidx` `referenced_size`, completing `leva`/`ssix` partial-subsegment-fetch read+write symmetry (ISO/IEC 14496-12 §8.16.4)
- Round 351 — `leva` (LevelAssignmentBox, §8.8.13) fragmented-mux emission: new `FragmentedOptions::levels: Vec<demux::LevaEntry>` (empty by default). When non-empty, the fragmented muxer writes one `leva` after the `trex` boxes inside the init-segment `mvex`, declaring how the file is partitioned into levels for partial-subsegment fetch (the level numbers a sibling `ssix` refers to). Default (empty) output is byte-identical to before — no `leva` is written. A muxer-emitted `leva` reads back through the demuxer's `mvex` walk (`leva_count` / `leva_<n>` metadata, `leva_entries()`), closing the write/read symmetry for `leva` (ISO/IEC 14496-12 §8.8.13)
- Round 351 — `leva` (LevelAssignmentBox, §8.8.13) builder: public `demux::build_leva_box(&LevaRecord)` serialises the §8.8.13.2 v0 body (FullBox preamble, `level_count`, then per-level `track_id` + packed `padding_flag<<7 | assignment_type` + assignment-type tail) into a complete box, the write counterpart to the existing read-only `parse_leva_box`. Every assignment_type tail is covered (0 → `grouping_type`; 1 → `+grouping_type_parameter`; 2/3 → bare; 4 → `sub_track_id`; reserved > 4 → no tail per spec); an `assignment_type` that overflows the 7-bit field is rejected rather than corrupting the `padding_flag` bit. Round-trips byte-exact through `parse_leva_box` (ISO/IEC 14496-12 §8.8.13)
- Round 346 — large-mdat muxing: `Mp4MuxerOptions::large_mdat` (default `false`) reserves the ISO/IEC 14496-12 §4.2 extended-size `mdat` header (`[size=1]["mdat"][largesize:u64]`) so the media payload may exceed 4 GiB. The direct-write path picks the header form up front (it streams `mdat` before knowing its size); the faststart path additionally auto-promotes when the compact 32-bit `size` would overflow. Sub-4-GiB output without the flag stays byte-identical (compact 8-byte header); the >4-GiB direct-write error message now names `large_mdat`. Closes the "mdat > 4 GiB" gap in the README's not-yet-supported list (ISO/IEC 14496-12 §4.2)
- Round 346 — prft (ProducerReferenceTimeBox) muxing: the fragmented-MP4 writer can now emit a `prft` immediately before any fragment's `moof` (after any `sidx`/`styp`, per §8.16.5). `FragmentedMuxer::set_next_segment_prft(reference_track_id, ntp_timestamp, media_time, flags)` queues a box for the next fragment (version auto-selected: v0 u32 `media_time` when it fits, else v1 u64); `set_next_segment_prft_v1(..)` forces v1. The queued box is consumed per fragment and its size is folded into the enclosing `sidx` `referenced_size` so a byte-range fetch still reaches the `moof`. Closes the write half of the previously read-only `prft` support (the demuxer already parsed it via `parse_prft_box`); round-trips byte-exact through `parse_prft_box` and leaves the fragmented sample-table round-trip undisturbed (ISO/IEC 14496-12 §8.16.5)
- Round 343 — typed §10 sample-group description entries (`sample_group_entries` module): `parse_*`/`build_*` decoders/builders interpreting the opaque `sgpd` entry blobs the demuxer surfaces, for every grouping type the base spec defines — `roll`/`prol` RollRecoveryEntry (signed `roll_distance`, §10.1.1), `rash` RateShareEntry (single- + multi-operation-point shapes, §10.2.2), `alst` AlternativeStartupEntry (variable `roll_count` offset array + optional output-rate tail, §10.3.2), `rap ` VisualRandomAccessEntry (§10.4.2), `tele` TemporalLevelEntry (§10.5.2), `sap ` SAPEntry (§10.6.2). Each round-trips byte-exact; `SampleGroupGroupingType` routes a `grouping_type` FourCC to its §10 variant. Mirrors the `cenc::parse_seig` precedent for typed grouping-type interpretation above the container boundary (ISO/IEC 14496-12:2015 §10)
- Round 340 — prft 2022-edition flag bits captured: `PrftRecord` now carries the 24-bit FullBox `flags` (0 in the 2015 edition) plus `is_encoder_input_output()` / `is_finalization_time()` / `is_file_write_time()` / `is_arbitrary_association()` / `is_realtime_offset()` accessors describing what the NTP time represents; set bits are appended as trailing name tokens to the surfaced `prft_<n>` metadata value (the three-integer prefix stays backward-compatible) (ISO/IEC 14496-12 §8.16.5, 2022 flag table)
- Round 340 — csgp §8.9.5 `pattern_size_code`/`count_size_code` 4-bit-agreement constraint enforced on both sides: `parse_csgp` rejects a box that mixes the 4-bit width (code 0) with a wider code between the two selectors (an invalid file), and `build_csgp` promotes a 4-bit selector off code 0 when the other would be wider, so the muxer never emits the invalid mix (ISO/IEC 14496-12:2020 §8.9.5)
- Round 337 — csgp pattern → sample expansion: `CsgpBox::resolve_samples(total_samples)` materialises one resolved `sample_group_description_index` per sample in decoding order — cycling each pattern's index run across its `sample_count` (cycle may end mid-pattern), padding trailing unmapped samples with the `sgpd` default (value 0), clamping a `sample_count` sum that overruns the track, and running every emitted index through the bit-7 fragment-local-vs-global MSB convention. Empty-index patterns map nothing rather than panicking (ISO/IEC 14496-12:2020 §8.9.5)
- Round 335 — csgp index_msb_indicates_fragment_local_description (flag bit 7): CsgpBox now records the flag + index field width (index_field_bits); CsgpPattern::resolve_index splits the fragment-local-vs-global source-selector MSB into a CsgpResolvedIndex (raw indices stay verbatim); build_csgp emits bit 7; surfaced as a `fraglocal` marker on csgp_<n> (ISO/IEC 14496-12:2020 §8.9.5)
- Round 329 — Bit Rate Box (btrt) inside any SampleEntry: typed BtrtRecord + public parse_btrt_box, surfaced on params.options as btrt_buffer_size_db / btrt_max_bitrate / btrt_avg_bitrate (ISO/IEC 14496-12 §8.5.2)
- Round 325 — Ambient Viewing Environment Box (amve) inside VisualSampleEntry: typed AmveRecord + public parse_amve_box, surfaced on params.options as amve_ambient_illuminance / amve_ambient_light_x / amve_ambient_light_y (ISO/IEC 14496-12 post-2015 / ISO/IEC 23091-3)
- Round 320 — csgp CompactSampleToGroupBox builder (sample_groups::build_csgp) + public parse_csgp_box read entry point (ISO/IEC 14496-12:2020 §8.9.5)
- Round 314 — Hint Media Header Box (hmhd) typed accessor (ISO/IEC 14496-12 §12.4.2)

## [0.0.9](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.8...v0.0.9) - 2026-06-15

### Other

- demux dref DataReferenceBox (dinf/url /urn , ISO/IEC 14496-12 §8.7.1-2)
- parse and surface all sample-description entries (§8.5.2)
- Sub Track boxes (strk/stri/strd/stsg, ISO/IEC 14496-12 §8.14)
- parse csgp CompactSampleToGroupBox (ISO/IEC 14496-12:2020 §8.9.5)
- parse trep TrackExtensionPropertiesBox (ISO/IEC 14496-12 §8.8.15)
- Round 283 — CENC AES-128 CTR/CBC cipher driver (ISO/IEC 23001-7:2016 §9)
- Round 279 — Subsegment Index Box (ssix) typed parse + builder (ISO/IEC 14496-12 §8.16.4)
- Round 272 — Video Media Header Box (vmhd) typed accessor (ISO/IEC 14496-12 §12.1.2)
- Round 264 — Level Assignment Box (leva) typed accessor (ISO/IEC 14496-12 §8.8.13)
- Round 259 — Progressive Download Information Box (pdin) typed accessor (ISO/IEC 14496-12 §8.1.3)
- Round 256 — Padding Bits Box (padb) typed accessor (ISO/IEC 14496-12 §8.7.6)
- Round 252 — Copyright Box (cprt) typed accessor (ISO/IEC 14496-12 §8.10.2)
- drop release-plz.toml — use release-plz defaults across the workspace
- Round 245 — per-sample CENC cipher walker (ISO/IEC 23001-7:2016 §9.4–9.6)
- Round 242 — SampleFlags typed accessor for §8.8.3.1 trun/tfhd/trex packed u32
- Round 239 — collect moof-level pssh boxes keyed by mfhd.sequence_number (ISO/IEC 23001-7:2016 §8.1.1)
- Round 235 — typed CENC scheme-decision router + seig sample-group entry parser (ISO/IEC 23001-7:2016 §4.2 / §6 / §10)
- Round 228 — Track Selection Box parsing (ISO/IEC 14496-12 §8.10.3)
- Round 221 — Movie Extends Header Box parsing (ISO/IEC 14496-12 §8.8.2)
- Round 216 — Degradation Priority Box parsing (ISO/IEC 14496-12 §8.5.3)
- Round 210 — Track Group Box parsing (ISO/IEC 14496-12 §8.3.4)
- Round 203 — saiz / saio parsing (ISO/IEC 14496-12 §8.7.8 + §8.7.9)

### Added

- Data Reference Box (`dinf` / `dref` / `url ` / `urn `, ISO/IEC
  14496-12 §8.7.1–2) — round 309. The `minf` walker now recurses into
  the `dinf` DataInformationBox (§8.7.1) for its `dref` DataReferenceBox
  (§8.7.2), the table of media-data locations that every sample entry's
  1-based `data_reference_index` (§8.5.2.2) indexes into. Each
  `DataEntryBox` is parsed as a `url ` DataEntryUrlBox (the §8.7.2.3
  self-contained flag, or a NULL-terminated `location` URL when external)
  or a `urn ` DataEntryUrnBox (a NULL-terminated `name` plus an optional
  `location`); a non-`url `/`urn ` child is dropped without disturbing
  the entry ordering that `data_reference_index` relies on. Surfaced on
  `params.options`: the common single self-contained `url ` case collapses
  to `dref_self_contained = "true"` with no per-entry keys (a one-lookup
  "all samples in this file" check); a multi-entry or external (split-
  source) table additionally emits `dref_count` and `dref_<n>` (1-based,
  matching `data_reference_index`) = `"<kind> self=<true|false>
  [ name=<urn>][ loc=<url>]"`. Robustness mirrors the rest of the demuxer:
  a forged `entry_count` cannot over-allocate (capacity bounded by the
  8-byte-per-child floor of the remaining body), a child overrunning the
  body ends the walk at what was read, an unknown FullBox version is
  tolerated, and a malformed `dref` is dropped without failing the `minf`
  parse (the file still demuxes against the single-source default). New
  internal types `DrefBox` / `DataEntry`; new `parse_dinf` / `parse_dref`
  / `read_c_string` / `read_c_string_split` helpers; new `DREF` / `URL_`
  / `URN_` box constants. 12 new unit tests cover the single
  self-contained entry, external URL location, `urn ` name+location and
  name-only, multi-entry ordering, unknown-child drop, too-short and
  forged-count guards, the `minf`→`dinf`→`dref` pickup path, and the
  three surfacing shapes (single-self-contained collapse, external
  per-entry surfacing, absence-no-keys). Source: ISO/IEC 14496-12:2015
  §8.7.1–2 (staged PDF `docs/container/isobmff/bmff.txt`).
- Multiple sample descriptions per track (`stsd`, ISO/IEC 14496-12
  §8.5.2) — round 306. `parse_stsd` now walks **every** `SampleEntry`
  in the box rather than discarding all but the first. Entry `[0]`
  still drives active decode dispatch and codec-config parsing; the
  additional entries (present when a track switches description
  mid-stream via a per-chunk `stsc.sample_description_index ≥ 2` or a
  fragment's `tfhd` / `trex` `sample_description_index`) are recorded
  with their FourCC and §8.5.2.2 `data_reference_index`. When
  `entry_count > 1` they surface on `params.options` as `stsd_count`
  plus `stsd_<n>` (1-based, matching the spec's
  `sample_description_index`) = `<fourcc> dref=<data_reference_index>`.
  Single-description tracks emit no `stsd_*` keys. A forged
  `entry_count` exceeding the bytes present stops at what could be read
  (no over-allocation, no invented entries). Four unit tests cover the
  multi-entry record, options surfacing, single-entry no-keys, and the
  over-count truncation case.

- Sub Track boxes (`strk` / `stri` / `strd` / `stsg`, ISO/IEC
  14496-12 §8.14) — round 300. The track-level `udta` walker now
  recognises the `strk` Sub Track box (§8.14.3): each assigns *part*
  of a track to the same alternate / switch groups whole tracks use
  (§8.3.2 / §8.10.3), the mechanism for selecting among layered-codec
  alternatives (SVC / MVC temporal, spatial, SNR, view layers) that
  don't map onto track boundaries (§8.14.1). The mandatory `stri`
  (Sub Track Information, §8.14.4) is parsed for `switch_group`
  (i16), `alternate_group` (i16), `sub_track_ID` (u32), and the
  §8.14.4.3 `attribute_list[]` of descriptive / differentiating
  FourCCs; the mandatory `strd` (Sub Track Definition, §8.14.5) is
  walked one level for its `stsg` (Sub Track Sample Group, §8.14.6)
  children, each carrying a `grouping_type` plus its `sgpd`
  description-index list. Surfaced on `params.options` as
  `subtrack_<n>` = `"id=<id> switch=<s> alt=<a> [attrs=<fourcc...>]
  [stsg=<grouping_type>:<idx>,<idx>;...]"`. A `strk` missing its
  mandatory `stri` contributes no sub track; an unknown FullBox
  version on `stri` / `stsg`, a too-short `stri`, or a `stsg` whose
  `item_count` overruns the body are rejected — the boxes are
  informational and a malformed entry never aborts the open. 21 new
  unit tests. Source: ISO/IEC 14496-12:2015 §8.14 (staged PDF).
- Track Extension Properties Box (`trep`, ISO/IEC 14496-12 §8.8.15) —
  round 291. The `mvex` walker now recognises the `trep` `FullBox(0, 0)`
  that documents a track's characteristics in subsequent movie
  fragments: it reads the `unsigned int(32) track_id` (§8.8.15.3) and
  records the type + payload length of each nested child box (§8.8.15.2
  "any number of boxes", e.g. an `assp` Alternative Startup Sequence
  Properties Box) without recursing into them. Quantity is zero or more
  (zero or one per track, §8.8.15.1); every instance is collected in
  file order. Surfaced on `Demuxer::metadata()` as `trep_<n>` with value
  `"<track_id> children=<k>[ <fourcc>...]"`, via the public
  `demux::parse_trep_box(&[u8]) -> Result<TrepRecord>` entry point, and
  via `Mp4Demuxer::treps() -> &[TrepRecord]` (downcast). Over-long child
  boxes are clamped to the remaining body, the 64-bit `largesize` child
  form is handled, and a malformed `trep` is dropped silently so a
  producer slip cannot brick `open()` (the box is informational). New
  public types `TrepRecord` / `TrepChild`. 14 new tests (9 unit + 5
  integration in `tests/trep.rs`).
- CENC AES-128 CTR / CBC cipher driver (ISO/IEC 23001-7:2016 §9,
  module `cenc_cipher`) — round 283. The crate's CENC stack is no
  longer parse-only: `cenc_cipher::decrypt_sample_in_place(decision,
  key, per_sample_iv, subsamples, data)` decrypts one protected
  sample in place for all four §10 schemes (`cenc` / `cbc1` /
  `cens` / `cbcs`), executing the `CipherStep` partitions from
  `cenc::plan_sample_cipher` against AES-128 via the
  `aes` / `ctr` / `cbc` primitive crates (new dependencies — cipher
  building blocks only). IV discipline is resolved from the
  `CencSchemeDecision`: per-sample IVs are length-checked against
  `tenc.default_Per_Sample_IV_Size`, the constant IV comes from
  `tenc.default_constant_IV`, and supplying a per-sample IV to a
  constant-IV configuration is rejected (§9.2 — IVs are either
  constant or per-sample). `cenc_cipher::expand_iv` applies the §9.1
  8-byte-IV expansion (bytes 0..8 = IV, bytes 8..16 zero); the §9.3
  CTR counter is the low 64 bits of the expanded IV, big-endian,
  incremented per encrypted cipher block and wrapping to zero
  without carrying into bytes 0..8. §9.5.1 continuity rules are
  executed: for non-`cbcs` schemes one continuous keystream / cipher
  chain spans the concatenated protected runs (partial CTR blocks
  continue across subsample boundaries, `cens` skip blocks consume
  no keystream, the `cbc1` chain crosses clear gaps), while `cbcs`
  reseeds its chain from the constant IV on each subsample's
  `iv_restart` step and chains across skip runs within a subsample.
  Encrypted CBC runs not a multiple of 16 bytes are rejected
  (§9.4.3 / §10.2). The step-level engine
  `cenc_cipher::decrypt_steps_in_place(mode, key, iv, steps, data)`
  is public for `seig`-overridden sample groups where the caller
  assembles its own plan/IV pairing. 23 new tests: a FIPS-197
  Appendix C.1 known-answer AES anchor, first-principles ECB-built
  §9.3 keystream checks (8-byte-IV counter start at zero; 16-byte-IV
  64-bit counter wrap leaving bytes 0..8 untouched), known-key
  round-trips for every scheme × subsample × pattern shape (`cenc`
  partial-tail + mid-block subsample continuity, `cbc1` clear-tail +
  chain continuity, `cens` 2:1 counter-skip semantics, `cbcs` 1:9
  stride with chain-over-skip and per-subsample restart, §9.7
  whole-block audio), and the IV-resolution / alignment /
  inconsistent-plan error arms.
- Subsegment Index Box typed parse + builder (ISO/IEC 14496-12
  §8.16.4, `ssix`) — round 279. The top-level `SubsegmentIndexBox`
  that maps levels (as assigned by the §8.8.13 `leva`
  LevelAssignmentBox) to byte ranges of the subsegments indexed by
  the immediately preceding `sidx` is now parsed by `parse_ssix`.
  The on-wire body — a 4-byte `FullBox(version=0, flags=0)` preamble,
  `unsigned int(32) subsegment_count`, and per subsegment an
  `unsigned int(32) range_count` followed by `range_count` packed
  `(unsigned int(8) level, unsigned int(24) range_size)` records per
  §8.16.4.2 — decodes into `SsixRecord { subsegments:
  Vec<SsixSubsegment> }` with each `SsixSubsegment` carrying its
  ordered `Vec<SsixRange { level: u8, range_size: u32 }>`. Only
  version 0 is defined; a different version byte is rejected rather
  than mis-read against the v0 layout. The two u32 counts are
  attacker-controlled, so capacity is pre-allocated against the bytes
  actually remaining, never against the declared counts, and any
  count that outruns the body is a truncation error. The §8.16.4.1
  `range_count >= 2` writer constraint is **not** enforced on read (a
  reader gains nothing by rejecting a one-range partition). The
  top-level walk collects every `ssix` in file order and surfaces a
  shape summary per box on `Demuxer::metadata()` as `ssix_<n>` =
  `"<subsegment_count> <total_range_count>"` (the full range table
  can be one 4-byte record per partial subsegment — too large for the
  flat channel); absent `ssix`, no keys are emitted. Like `leva`, a
  malformed instance is dropped without failing `open()` — the box is
  informational for this demuxer (seeking uses `sidx` / `tfra`). The
  structured record is reachable via the public
  `oxideav_mp4::demux::parse_ssix_box(&[u8])` entry point and the
  `Mp4Demuxer::ssixes() -> &[SsixRecord]` accessor (downcast); the
  matching `build_ssix_box(&SsixRecord)` builder serialises a
  complete `[size]['ssix']` box for segment-index emitters that pair
  it with a `sidx`, rejecting `range_size` values that overflow the
  24-bit wire field rather than silently masking them. Eleven new
  tests cover the spliced-after-`sidx` placement (a real fragmented
  mux output re-opened with one and two `ssix` boxes), omission,
  malformed-drop, u24 ceiling round-trip, oversize-range rejection,
  and the truncation / version guards.

- Video Media Header Box typed accessor (ISO/IEC 14496-12 §12.1.2,
  defined per §8.4.5) — round 272. A `vmhd` (`VideoMediaHeaderBox`)
  inside `minf` is now parsed by `parse_vmhd`. After the 4-byte
  `FullBox` preamble (the spec fixes `version=0, flags=1`), the body is
  `unsigned int(16) graphicsmode` followed by `unsigned int(16)[3]
  opcolor` — eight payload bytes, twelve total — decoded into a
  `VmhdBox { graphicsmode: u16, opcolor: [u16; 3] }`. `graphicsmode`
  is preserved verbatim (`0` is the spec's `copy` mode; derived
  specifications may extend the set, so a non-zero value is not
  normalised), and the three `opcolor` (red, green, blue) components
  are read in order. The parser tolerates a non-zero `version` byte
  rather than dropping an otherwise-usable header, but rejects a body
  shorter than the full 12 bytes — a truncated tail would surface
  truncation noise as an `opcolor` component. The `minf` walker in
  `parse_minf` captures the first instance seen (§12.1.2 fixes the
  quantity at exactly one) and silently ignores stray duplicates; a
  malformed `vmhd` is dropped without failing the surrounding `minf`
  parse, so the track simply has no video header. The parsed values are
  surfaced on `Demuxer::metadata()` as `vmhd_graphicsmode` (decimal)
  and `vmhd_opcolor` (the three components space-separated, decimal),
  mirroring the existing per-track media-box surfacing convention
  (`cslg_*`), so a caller compositing the track over an existing image
  reads them instead of re-walking `minf`.

- Level Assignment Box typed accessor (ISO/IEC 14496-12 §8.8.13) —
  round 264. A `LevelAssignmentBox` (`leva`) inside `mvex` is now parsed
  by the typed handler `parse_leva`. The on-wire body — a 4-byte
  `FullBox(version=0, flags=0)` preamble followed by `unsigned int(8)
  level_count` and `level_count` per-level entries laid out per
  §8.8.13.2 — is decoded into a `LevaRecord { entries: Vec<LevaEntry> }`
  where each `LevaEntry` carries `track_id` (u32), `padding_flag`
  (high bit of the §8.8.13.2 packed byte), `assignment_type` (low 7
  bits), plus the variant-specific tail fields `grouping_type`,
  `grouping_type_parameter`, and `sub_track_id`. The five spec-defined
  `assignment_type` values (0/1/2/3/4) are each consumed with their
  correct tail length; reserved values (> 4) carry only the 5-byte
  header and a downstream validator can flag them. The `mvex` walker
  in `parse_mvex` captures the first instance seen (§8.8.13.1 fixes
  quantity at zero or one per file); subsequent copies are silently
  ignored. The parsed table is surfaced on `Demuxer::metadata()` as
  `leva_count` (total entry count, decimal) plus `leva_<n>` per-entry
  strings of the shape `"<track_id> pad=<0|1> at=<assignment_type>
  [grouping_type=<u32>] [grouping_type_parameter=<u32>]
  [sub_track_id=<u32>]"` — the trailing tokens are emitted only when
  their assignment_type variant uses them, mirroring the §8.8.13.2
  variant tails. The structured record is reachable via the public
  `oxideav_mp4::demux::parse_leva_box(&[u8])` entry point for tooling
  that already has the payload bytes in hand, and via
  `Mp4Demuxer::leva_entries() -> Option<&[LevaEntry]>` (downcast).
  A truncated entry header or a truncated variant tail is rejected
  outright at parse time rather than silently dropping the rest of the
  table — a short table would lie about which levels exist. A
  malformed `leva` reaching `parse_mvex` is dropped silently so a
  producer slip cannot brick `open()` (the box is informational; the
  file remains parseable). A zero `level_count` is permitted at parse
  time so a downstream validator can spot the §8.8.13.3 "level_count
  shall be greater than or equal to 2" violation; the version byte is
  tolerated even if non-zero (the spec pins it to 0 but the payload
  layout is unambiguous), matching `parse_pdin` / `parse_padb` /
  `parse_stdp` posture for FullBox version-bit slips.
- Progressive Download Information Box typed accessor
  (ISO/IEC 14496-12 §8.1.3) — round 259. A `ProgressiveDownloadInfoBox`
  (`pdin`) at file scope is now parsed by the typed handler
  `parse_pdin`. The on-wire body — a 4-byte `FullBox(version=0, flags=0)`
  preamble followed by an array of `(rate, initial_delay)` u32 pairs
  in big-endian — is decoded into a `PdinRecord { entries: Vec<PdinEntry> }`
  where each `PdinEntry { rate, initial_delay }` corresponds to one
  spec-defined progressive-download hint pair. The top-level walker
  in `demux::open` captures the first instance during the file walk
  (§8.1.3.1 fixes quantity at zero or one); subsequent copies are
  silently ignored. The parsed table is surfaced on
  `Demuxer::metadata()` as `pdin_count` (total pair count, decimal)
  plus `pdin_<n>` (one per pair, `"rate initial_delay"` decimal
  space-separated, 0-based in file order). An explicitly empty box
  (preamble only, zero pairs) emits `pdin_count = 0` and no
  `pdin_<n>` keys; absent `pdin`, no keys are emitted at all so a
  consumer can distinguish "no box" from "box but empty". The
  structured record is reachable via the public
  `oxideav_mp4::demux::parse_pdin_box(&[u8])` entry point for tooling
  that already has the payload bytes in hand, and via
  `Mp4Demuxer::pdin_entries() -> Option<&[PdinEntry]>` (downcast).
  A body whose post-preamble length is not a multiple of 8 (one
  `(rate, initial_delay)` u32 pair per entry per §8.1.3.2) is
  rejected outright rather than silently truncated — a framing slip
  is the producer's bug to fix. The version byte is tolerated even if
  non-zero (the spec pins it to 0 but the payload layout is
  unambiguous), matching `parse_padb` / `parse_stdp` posture for
  FullBox version-bit slips.
- Padding Bits Box typed accessor (ISO/IEC 14496-12 §8.7.6) — round 256.
  A `PaddingBitsBox` (`padb`) inside a track's `stbl` is now parsed by
  the typed handler `parse_padb`. The on-wire layout packs two samples
  per byte (each nibble is `bit(1) reserved=0; bit(3) pad`); the
  accessor unpacks them into a `Vec<u8>` of per-sample padding-bit
  counts in `0..=7` (decode order). Unlike `sdtp` / `stdp`, `padb`
  declares its own `sample_count` on the wire (§8.7.6.2) and the table
  is self-contained — the trailing unused nibble when `sample_count`
  is odd is dropped during parse, and a body shorter than the declared
  size is rejected. The reserved high bit of each nibble (required
  zero by §8.7.6.2) is masked off so a producer slip on the reserved
  bit cannot corrupt the count. The parsed table is exposed on
  `params.options` as four `padb_*` keys: `padb_count` (total entries),
  `padb_max` (largest pad count, `0..=7`), `padb_nonzero_count`
  (samples whose pad count is non-zero — the count that actually
  matters to a bitstream consumer; an all-zero table means the track
  is byte-aligned and the box is informational only), and `padb_hist`
  (eight-bucket histogram `n0:n1:n2:n3:n4:n5:n6:n7` where `nK` is the
  count of samples whose pad count is `K`). The histogram fully
  captures the distribution in one short string. Absent `padb`, none
  of the keys are emitted.
- Copyright Box typed accessor (ISO/IEC 14496-12 §8.10.2) — round 252.
  A `CopyrightBox` (`cprt`) inside a track-level `udta` is now parsed
  by the typed handler `parse_cprt`, distinct from the 3GPP-style
  `udta` aggregator that lumps `cprt` into the file-wide flat metadata
  channel. The 16-bit packed language word (§8.10.2.3 — 1 pad bit +
  three 5-bit characters at `(ASCII - 0x60)`) is decoded back to a
  3-letter ISO 639-2/T lowercase tag; the C-string notice is UTF-8
  by default, UTF-16BE when it opens with the byte-order mark, with
  the trailing NUL terminator stripped. Multiple `cprt` boxes per
  track are supported (the §8.10.2.1 "Zero or more" multilingual
  case). Each record is surfaced on `params.options` as
  `copyright_<n>` (the notice; omitted for an empty notice so a
  consumer can detect absent vs. empty) and `copyright_<n>_lang`
  (the 3-letter tag; emitted even with an empty notice to preserve
  the language declaration). A body shorter than the 6-byte FullBox
  + language word minimum or an unknown FullBox version (§8.10.2.2
  pins it to 0) is silently dropped — the box is informational and
  a malformed entry never aborts the open.
- Per-sample CENC cipher walker — round 245. The new
  `cenc::plan_sample_cipher(decision, subsamples, sample_len) ->
  Result<Vec<CipherStep>>` partitions a sample's plaintext
  `0..sample_len` byte range into the typed ordered sequence of
  contiguous `(offset, len, kind, iv_restart)` runs an AES-128 layer
  iterates to perform decryption. The walker consumes the existing
  `cenc::CencSchemeDecision` (scheme + tenc) and the per-sample
  subsample list already surfaced by `parse_senc`, and runs each ISO/IEC
  23001-7:2016 spec carve-out:
  - §9.4.2 (`cenc` full-sample): one `Encrypted` step over the entire
    sample including the trailing partial 16-byte block (CTR encrypts
    partial cipher blocks).
  - §9.4.3 (`cbc1` full-sample) and §9.7 (whole-block full-sample on
    pattern schemes for non-video tracks): an `Encrypted` step over the
    whole-block prefix and a trailing `Clear` step over the 0–15-byte
    partial block.
  - §9.5 subsample encryption: per-subsample `Clear(BytesOfClearData)`
    then either one `Encrypted(BytesOfProtectedData)` span (non-pattern
    schemes, §9.5.1) or a §9.6 pattern walk (`(crypt_byte_block * 16,
    skip_byte_block * 16)` alternation with the trailing partial
    `crypt_byte_block` left in the clear).
  - §9.5.1 IV restart discipline: the `iv_restart` flag is `true` only
    on the first `Encrypted` step inside each subsample under `cbcs`
    (the spec's "treat each Subsample as a separate chain of cipher
    blocks, starting with the Initialization Vector associated with the
    sample"); `cenc` / `cbc1` / `cens` carry a continuous chain /
    counter across subsamples so the flag stays `false`.
  - §9.5.1 totals invariant: subsample `clear + protected` sums MUST
    equal `sample_len`; mismatch is `Err`.
  - §9.5.1 both-zero prohibition: a subsample with `clear == 0 &&
    protected == 0` is rejected.
  - `IvSupply::None` (unprotected track default) and
    `CencScheme::Unknown` are rejected — the walker structurally
    targets §10-registered schemes, and a private dialect supplies its
    own equivalent.

  **This crate performs no AES operation.** `CipherStep` is a static
  dispatch contract built from container-side bytes only; the actual
  key-derivation + AES block call is delegated to a downstream layer
  with key material from the `pssh.SystemID`. Nineteen unit tests
  exercise CTR / CBC full-sample, mixed clear / encrypted subsamples,
  the four-scheme `iv_restart` matrix, the 1:9 pattern at one full
  repetition, a trailing partial `crypt_byte_block` that goes clear, a
  truncated mid-skip run, the totals-mismatch + both-zero +
  unprotected + unknown-scheme rejection paths, the empty-sample
  edge, and a partition-cover invariant (`Σ step.len == sample_len`
  with contiguous offsets).

- Typed accessor for the §8.8.3.1 `sample_flags` 32-bit field — round
  242. The same packed `u32` appears in four sites across ISO/IEC
  14496-12: `default_sample_flags` in `trex` (§8.8.3) and `tfhd`
  (§8.8.7), and `first_sample_flags` / per-sample `sample_flags` in
  `trun` (§8.8.8). §8.8.3.1 fixes the on-wire layout (MSB→LSB):
  `bit(4) reserved=0; unsigned int(2) is_leading; unsigned int(2)
  sample_depends_on; unsigned int(2) sample_is_depended_on; unsigned
  int(2) sample_has_redundancy; bit(3) sample_padding_value; bit(1)
  sample_is_non_sync_sample; unsigned int(16)
  sample_degradation_priority;`. The new `demux::SampleFlags` struct
  surfaces each named field together with `SampleFlags::from_u32` /
  `to_u32` round-trip helpers and `is_sync_sample()` for the positive
  predicate (the inverse of the §8.8.3.1 non-sync bit, matching the
  §8.6.2 "absence = sync" convention). A free-function
  `demux::parse_sample_flags` provides a thin entry point for callers
  that hold the raw `u32` from a `trex`/`tfhd`/`trun` parse. §8.8.3.1
  cross-references the §8.6.4.3 value tables for the four 2-bit
  enums — the typed accessor surfaces the raw 2-bit small-int rather
  than mapping to a typed enum, mirroring the existing private
  `SdtpEntry` parser policy so callers stay in charge of the
  format-specific value overrides §8.6.4.3 allows. Decoder tolerates
  non-zero `reserved` bits (silent recovery, matching the rest of
  the demuxer); re-encode strips them back to zero per §8.8.3.1.
  Field-width masking on encode prevents an out-of-range field from
  bleeding into its neighbour. Nine unit tests cover the all-zero
  (sync) decode, the non-sync-only sentinel `0x0001_0000`, the
  canonical fMP4 I-picture and B-picture patterns, low-16-bit
  degradation-priority isolation, the 3-bit padding field's full 0..7
  range, the saturated all-fields round-trip (`0x0FFF_FFFF`),
  reserved-bit tolerance, and the free-function/struct equivalence.
- moof-level `pssh` (ProtectionSystemSpecificHeaderBox) parsing per
  ISO/IEC 23001-7:2016 §8.1.1 — round 239. §8.1.1 lists the box's
  container as "Movie (`moov`) or Movie Fragment (`moof`)", and the
  same clause's normative reader rule pins fragment-scoped
  lookups: "readers SHALL examine all Protection System Specific
  Header boxes in the Movie Box and in the Movie Fragment Box
  associated with the sample (but not those in other Movie
  Fragment Boxes)". The fragmented walk (`parse_moof`) now
  collects each `pssh` directly inside a `moof` into a new
  `demux::MoofPsshRecord` (`pssh: cenc::PsshBox`,
  `moof_sequence: u32`), keying each by the enclosing
  `mfhd.sequence_number` so the §8.1.1 scope can be honoured by a
  downstream DRM layer without re-walking the file. Records land
  in moof-walk order and preserve the on-wire walk order across
  multiple pssh boxes per fragment (the "single file constructed
  to be playable by multiple key/DRM systems" shape called out in
  §8.1.1). v0 (no KID list) and v1 (KID_count + KIDs) are both
  supported through the existing `cenc::parse_pssh` path; a
  malformed pssh inside a moof is dropped without aborting the
  fragment, mirroring the moov-level recovery policy. Surfaced
  through `Demuxer::metadata()` as `moof_pssh_<n>` keys with value
  `"systemid=<hex> seq=<mfhd_seq> kids=<n> data=<len>"`; structured
  records are reachable via the new `Mp4Demuxer::moof_psshes()`
  accessor. pssh that precedes the `mfhd` inside a moof (§8.8 lists
  mfhd first but does not forbid other top-level boxes preceding
  it) still binds to the correct sequence_number via a
  buffered-finalize pass. Four unit tests cover the canonical
  mfhd-then-pssh case, two-pssh-per-fragment (v0 + v1), pssh-before
  -mfhd ordering, and silent recovery from a truncated pssh body.

- Typed CENC scheme-decision router + `seig` sample-group entry
  parser (ISO/IEC 23001-7:2016 §4.2 / §6 / §10 — round 235). The
  four protection schemes defined in §10 — `cenc` (AES-CTR full /
  NAL-subsample), `cbc1` (AES-CBC full / NAL-subsample), `cens`
  (AES-CTR pattern subsample), `cbcs` (AES-CBC pattern subsample)
  — are exposed as the typed `cenc::CencScheme` enum (with a
  passthrough `Unknown([u8; 4])` variant for private DRM dialects
  that ship a custom `scheme_type` FourCC in `sinf/schm`). The two
  binary axes a downstream decryptor dispatches on — cipher mode
  (`cenc::CipherMode::{Ctr, Cbc}`, §9.3 / §9.4) and the
  pattern-encryption flag (`cens` / `cbcs` only, §9.6) — are
  surfaced as `CencScheme::cipher_mode()` and
  `CencScheme::uses_pattern_encryption()`. A scheme value bundled
  with the parsed `tenc` becomes a `cenc::CencSchemeDecision` —
  the typed routing slip a future AES layer can pattern-match
  against — built via `CencSchemeDecision::new(scheme, tenc)` with
  structural validation only: the scheme's
  `required_tenc_version()` (when known) must match `tenc.version`
  (§10.1 / §10.2 pin v0; §10.3 / §10.4 pin v1), and pattern
  schemes must carry a non-zero `(crypt_byte_block,
  skip_byte_block)` pair (§9.6). The track-default IV-supply
  discipline (§9.1) is recovered via `CencSchemeDecision::iv_supply()`
  as the typed `cenc::IvSupply` enum — `PerSample { size: u8 }`
  for the on-the-wire-IV case, `Constant` for the
  `default_constant_IV` case, `None` for unprotected tracks. This
  crate performs no AES operation; the bundle is a static dispatch
  contract built from container-side bytes only, leaving the key
  derivation + AES block call to a layer with key material from
  the named `pssh.SystemID`. The §6
  `CencSampleEncryptionInformationGroupEntry` (`seig`) — the
  sample-group entry the spec defines for overriding the
  track-default `(crypt_byte_block, skip_byte_block, isProtected,
  Per_Sample_IV_Size, KID, constant_IV?)` for a group of samples
  (the mechanism for mixing encrypted/unencrypted samples in one
  track, or rotating keys per scene) — is parsed by
  `cenc::parse_seig(body)` into the typed `cenc::SeigEntry`,
  with the same IV-supply / pattern-flag accessors as the
  track-default router. §6's "clients SHALL ignore additional
  bytes after the fields defined" trailing-bytes rule is honoured.
- Track Selection Box parsing (ISO/IEC 14496-12 §8.10.3, `tsel` —
  round 228). The optional `TrackSelectionBox` inside a track-level
  `udta` declares two media-selection signals: `switch_group`
  (signed 32-bit, §8.10.3.4 — groups tracks that are
  interchangeable *during* playback within the alternate group
  declared on `tkhd`) and `attribute_list` (§8.10.3.5 — a list of
  FourCC tags drawn from the descriptive set `tesc` / `fgsc` /
  `cgsc` / `spsc` / `resc` / `vwsc` and the differentiating set
  `bitr` / `cdec` / `lang` / …, characterising what the track
  offers). Body layout after the 4-byte FullBox preamble is a
  signed 32-bit `switch_group` followed by zero or more 4-byte
  FourCCs running to the end of the box; trailing partial-FourCC
  bytes are ignored (matching the `subs` / `sidx` "trailing partial
  record" handling). Surfaced on `params.options` as
  `tsel_switch_group` (the signed integer, emitted even when zero
  so a caller can distinguish present-but-zero from absent) and
  `tsel_attributes` (space-separated FourCCs, mirroring the
  `tref_<type>` value convention; omitted when the attribute list
  is empty). A `tsel` body shorter than the 8-byte minimum, or an
  unknown FullBox version (the spec pins it to 0), is silently
  dropped — the box is informational and a malformed entry never
  aborts the open. Container preserves the raw FourCCs; mapping
  them to consumer-level semantics (e.g. a player selecting by
  language vs. bitrate within an alternate group) is delegated.
  Absent `tsel`, no `tsel_*` keys are emitted.
- Movie Extends Header Box parsing (ISO/IEC 14496-12 §8.8.2, `mehd` —
  round 221). A sealed fragmented file's `mvex/mehd`
  MovieExtendsHeaderBox carries the overall presentation
  `fragment_duration` of the movie including all fragments, in the
  movie timescale (per §8.8.2.3). Version 0 stores the duration as a
  32-bit field; version 1 widens it to 64 bits. Both layouts are read
  and widened to `u64`. When `mehd` is present with a non-zero
  duration, it takes precedence over `mvhd.duration` for the value
  surfaced through `Demuxer::duration_micros` — sealed fragmented
  files typically carry `mvhd.duration = 0` (the moov has no resident
  samples that contribute to a non-fragment duration) so `mehd` is
  the only authoritative total. The raw value (in the movie
  timescale) is also reflected verbatim on `Demuxer::metadata()` as
  `mehd_fragment_duration`, mirroring the rest of the crate's flat
  metadata surface. A `mehd` instance with an unknown version (the
  spec defines only 0 and 1) or a truncated body is dropped silently
  rather than failing the open (defensive; the demuxer falls back to
  `mvhd.duration` in that case). Absent `mehd`, the metadata key is
  not emitted and the demuxer's pre-r221 mvhd-only path is taken.
- Degradation Priority Box parsing (ISO/IEC 14496-12 §8.5.3, `stdp` —
  round 216). A track's `stbl/stdp` DegradationPriorityBox is an
  optional per-sample table of 16-bit `priority` values. The
  `sample_count` is implicit from `stsz` / `stz2` (mirroring `sdtp`),
  so the on-disk body after the FullBox preamble is a packed
  big-endian u16 array. The spec leaves the value semantics to
  derived specifications (§8.5.3.3 "Specifications derived from this
  define the exact meaning and acceptable range of the `priority`
  field"), so the container preserves the raw u16s without
  interpretation. Surfaced on `params.options` as four summary keys
  per track-with-`stdp`: `stdp_count` (total entries), `stdp_min` /
  `stdp_max` (value spread), and `stdp_sum` (u64 — a u16 priority ×
  2^32 samples fits comfortably; consumers compute the mean as `sum /
  count`). A renderer dropping samples under bitrate / CPU pressure
  consults the carrying spec for the priority ordering. A trailing
  odd byte (the spec never produces one — `priority` is always
  16-bit) is silently ignored rather than failing the whole box.
  Absent `stdp`, no keys are emitted.
- Track Group Box parsing (ISO/IEC 14496-12 §8.3.4, `trgr` — round
  210). Each typed `TrackGroupTypeBox` child inside `trak/trgr` is
  parsed as a `(track_group_type, track_group_id)` pair. The child's
  FourCC is the grouping type (`msrc` is the spec-named example, for
  multi-source presentations); its body is a 4-byte FullBox preamble
  (`version = 0`, `flags = 0`) followed by the 32-bit
  `track_group_id`. Tracks sharing the same `(type, id)` pair belong
  to the same group per §8.3.4.3. Track groups are a membership
  signal, **not** a dependency relationship — that role belongs to
  `tref` (§8.3.3). Surfaced on `params.options` as `trgr_<n>`
  (0-based encounter index) with value `"<type> <id>"`, mirroring the
  `kind_<n>` two-field shape. The spec does not forbid two children
  of the same `track_group_type` on one track, so both encounter-order
  entries are preserved (unlike `tref`, which caps at one per
  `reference_type`). Trailing bytes inside a child are reserved for
  derived-spec extensions and ignored; a non-zero `version` (the spec
  pins it to 0) is silently skipped so unknown extensions never
  mis-parse. Absent `trgr`, no keys are emitted.
- Sample Auxiliary Information Sizes / Offsets parsing (ISO/IEC
  14496-12 §8.7.8 / §8.7.9, `saiz` / `saio` — round 203). Both boxes
  are read inside `stbl` (track-level absolute offsets) and inside
  `traf` (movie-fragment, `tfhd.base_data_offset`-relative offsets per
  §8.8.14). The optional `(aux_info_type, aux_info_type_parameter)`
  key block (gated by FullBox `flags & 1`) is captured when present
  and left as `None`/`None` otherwise so callers can apply §8.7.8.3's
  implied-value rule themselves. `saio` v0 (32-bit) and v1 (64-bit)
  offsets are both read and widened to `u64`; `version` is preserved.
  Truncation guards mirror the existing `subs` parser — a forged
  `sample_count` / `entry_count` is caught before the per-sample /
  per-entry alloc. Track-level boxes surface as `saiz_<n>` /
  `saio_<n>` keys on `params.options`; fragment-level pairs surface
  as `frag_sai_<n>` keys through `Demuxer::metadata()` plus a public
  `Mp4Demuxer::sai_records()` accessor that returns the structured
  per-fragment records (`SaiRecord` with `track_idx`,
  `moof_sequence`, `Vec<TrafSaiz>`, `Vec<TrafSaio>`) for a CENC layer
  consuming the spec-permitted `senc` alternative IV-carriage path
  (ISO/IEC 23001-7 §7.3). Parse-only — the auxiliary-information
  bytes themselves stay in the mdat at the offsets the `saio` names.
- CENC metadata parsing (ISO/IEC 23001-7:2016 — round 196). Three
  new structured parsers in `crate::cenc`:
  - `parse_tenc` for the §8.2 TrackEncryptionBox (v0 + v1 with
    pattern-encryption block counts and the constant-IV variant
    used by `cbcs` / `cens`),
  - `parse_pssh` for the §8.1 ProtectionSystemSpecificHeaderBox
    (v0 SystemID + Data; v1 adds the KID list, with `KID_count == 0`
    surfaced as an empty `kids` Vec per §8.1.3's "apply to all
    KIDs" rule),
  - `parse_senc` for the §7.2 SampleEncryptionBox (per-sample IVs
    with the spec-required `per_sample_iv_size` recovered from the
    matching `tenc`, plus the optional `UseSubSampleEncryption`
    `{BytesOfClearData, BytesOfProtectedData}` table).
  Demux integration: `tenc` is auto-discovered inside `sinf/schi`
  during sample-entry parsing and surfaced on `params.options` as
  `cenc_default_kid` / `cenc_default_iv_size` / `cenc_tenc_version`
  / (v1) `cenc_default_crypt_byte_block` /
  `cenc_default_skip_byte_block` / `cenc_default_constant_iv`;
  `pssh` is collected at moov level and surfaced as `pssh_<n>`
  metadata; per-fragment `senc` is collected during the moof walk
  and surfaced as `senc_<n>` metadata. Parse-only — no AES /
  decryption op runs in this crate.

## [0.0.8](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.7...v0.0.8) - 2026-05-29

### Other

- Round 189 — reject extended-size boxes that overflow u64
- use parsed sidx for seek when no tfra is available (§8.16.3)
- parse subs SubSampleInformationBox (ISO/IEC 14496-12 §8.7.7)
- parse prft ProducerReferenceTimeBox (ISO/IEC 14496-12 §8.16.5)
- Sample-group muxing — write side of sbgp + sgpd (ISO/IEC 14496-12 §8.9)
- DASH/CMAF-friendly write-side Segment Type Box emitter
- add cargo-fuzz demux target + fix 3 DoS classes it found
- parse sdtp (SampleDependencyTypeBox §8.6.4)
- demux Sample Group structures (sbgp + sgpd, ISO/IEC 14496-12 §8.9)
- demux Shadow Sync Sample Box (stsh, ISO/IEC 14496-12 §8.6.3)
- demux Composition to Decode Box (cslg, ISO/IEC 14496-12 §8.6.1.4)
- demux Track Kind Box (kind, ISO/IEC 14496-12 §8.10.4)
- demux ExtendedLanguageBox (elng, ISO/IEC 14496-12 §8.4.6)
- emit edts/elst edit list for positive start delay (§8.6.5–6)
- mp4 muxer: subtitle / timed-text track support (mov_text, webvtt, ttml, sbtt, stxt)
- parse tref + surface track references as tref_<type> options
- subtitle-handler dispatch + sinf protection unwrap

### Fixed

- Reject extended-size (`size=1 largesize=u64::MAX`) boxes that overflow
  `u64` at the header reader rather than at every downstream
  `body_start + payload_size` arithmetic site. The `read_box_header`
  signature is now `Read + Seek + ?Sized` (every existing caller already
  passes a `Cursor` or `Box<dyn ReadSeek>`, so this is API-source
  compatible) and captures the start offset of each box. After the
  size32 / largesize discrimination it `checked_add`s `start +
  total_size` and rejects any header whose declared end byte would
  overflow `u64`. Without the guard, an 8-byte placeholder box at
  offset 0 followed by a `size=1` extended box whose `largesize =
  u64::MAX` panics debug builds with `attempt to add with overflow` at
  `demux.rs:53` (the `sidx` body-end anchor `body_start +
  payload_size`); release builds silently wrap to a tiny value and
  pass the past-EOF guard, propagating corrupted offsets into the
  sample-table walker. Companion to round 187 in `oxideav-mov` which
  closed the same shape on the QTFF atom walker for fuzz crash
  `353fbd8c…`. Three new tests: two unit tests in `src/boxes.rs`
  pin the boundary (header at `start + largesize = u64::MAX` is
  accepted; `start + largesize > u64::MAX` is rejected with an error
  message naming the offending fourcc), and one integration test in
  `tests/largesize_overflow.rs` replays the crash shape end-to-end
  through `demux::open` and asserts a clean `Err` surfaces.

### Added

- `sidx`-driven seek fast-path (ISO/IEC 14496-12 §8.16.3
  SegmentIndexBox). The demuxer already parsed `sidx` records into a
  `Vec<SidxRecord>`; until now they were kept for downstream tooling
  only and `seek_to` ignored them. This change adds a secondary
  fast-path between the existing `tfra` walk and the linear-scan
  fallback: when no `tfra` covers the requested track (typical for
  DASH on-demand profile files, which carry `sidx` but no `mfra`),
  `seek_to` walks every `sidx` whose `reference_id` matches the
  track, expands each one's references into virtual
  `(time, byte_offset)` anchors, and picks the latest anchor whose
  decode-time start is at or before the requested pts (translated
  from track-media-timescale into sidx-timescale per §8.16.3). The
  returned byte offset feeds the same "scan forward to the first
  keyframe at-or-after this offset" loop the `tfra` path uses, so
  the sample-list scan is bounded by one subsegment instead of the
  whole file. Both on-the-wire shapes are handled: a single `sidx`
  indexing N subsegments (on-demand profile) and one `sidx` per
  fragment indexing one subsegment each (live profile, which is
  what our own muxer emits). Hierarchical (nested) sidx references
  are walked for byte-range accounting only — they don't carry a
  media-time anchor we can land on. A `sidx` whose `reference_id`
  doesn't match any track's `track_ID` is ignored (with the linear
  scan as the safe fallback), and a pts that predates every indexed
  subsegment snaps to the first media reference's offset so the
  seek still lands on a real keyframe boundary instead of falling
  through to O(N) over the whole sample table. Three new integration
  tests in `tests/random_access.rs`:
  `sidx_drives_seek_to_correct_keyframe_when_no_mfra` mux-then-strip
  the trailing `mfra` and confirm `seek_to(pts=2500)` lands on the
  `pts=2048` keyframe (exact-pts seek + negative-pts snap-to-zero
  also covered); `seek_to_still_works_without_sidx_or_mfra`
  cross-checks the linear-scan fallback still gets the right
  keyframe when neither index is present (correctness, not perf);
  `sidx_with_wrong_reference_id_is_ignored` patches every `sidx`'s
  reference_id to a nonexistent track and confirms the demuxer
  falls through to the linear scan and still lands on the correct
  pts. Closes the README "Not yet supported — `sidx`
  segment-index seek-time mapping (skipped; sequential demux works
  without it)" item.
- Sub-Sample Information Box demux (`subs`, ISO/IEC 14496-12 §8.7.7).
  The optional per-track sparse table that describes how selected
  samples decompose into smaller, semantically-meaningful sub-samples
  (NAL units, slices, parameter sets per the codec binding — e.g.
  ISO/IEC 14496-15 for H.264). Each entry carries a `sample_delta`
  from the previous entry (the table is sparse — samples without
  sub-sample structure produce no row) plus a list of
  `(subsample_size, subsample_priority, discardable,
  codec_specific_parameters)` rows. Both v0 (16-bit `subsample_size`)
  and v1 (32-bit) layouts are read and normalised to `u32`; the
  FullBox `flags` is preserved verbatim so co-resident `subs` boxes
  with codec-specific `flags` semantics (§8.7.7.1) can be told apart.
  Each `subs` is surfaced on `params.options` as `subs_<n>` (0-based
  encounter index); the value starts with `"v<version> flags=<f>"`
  and is followed by one space-separated
  `delta=<d>[:size,priority,discardable,csp[;...]]` block per entry
  (`csp` rendered as lowercase 8-digit hex). The container does not
  interpret `subsample_priority` / `discardable` /
  `codec_specific_parameters` — those small ints are codec-specific
  and the spec hands them to a layer that knows the carried
  encoding. Absent `subs`, no keys are emitted. Ten unit tests in
  `demux::tests` cover v0 round-trip with three sub-samples + a
  delta-2 follow-up entry, v1 32-bit `subsample_size` widening
  (0x0001_2345 sentinel above the 16-bit ceiling), the legal
  `subsample_count = 0` shape (an addressed sample with no
  sub-sample structure), 24-bit `flags` preservation, both
  truncation error paths (FullBox-preamble underflow and
  mid-sub-sample cutoff), `parse_stbl` dispatch wiring,
  multi-`subs`-per-track accumulation (distinct `flags` per
  §8.7.7.1), the `subs_<n>` options surfacing (full byte-exact
  expected string), and the absence-no-keys path.

- Producer Reference Time Box demux (`prft`, ISO/IEC 14496-12 §8.16.5).
  A top-level FullBox carrying a UTC wall-clock instant in NTP-64
  format (RFC 5905) correlated with a media time on one reference
  track — used by low-latency DASH / CMAF live streaming so a
  consumer can match production wall-clock against media presentation
  time. The demuxer parses every `prft` it encounters during the
  top-level box walk and surfaces them as `prft_<n>` (0-based file
  order) container-metadata entries; each value is three
  space-separated decimal integers `"reference_track_ID
  ntp_timestamp media_time"`. Both v0 (32-bit `media_time`) and v1
  (64-bit `media_time`) layouts are read; absent `prft`, no keys are
  emitted. Public `oxideav_mp4::demux::parse_prft_box(&[u8]) →
  Result<Option<PrftRecord>>` entry point exposes the structured
  record (`reference_track_id`, `ntp_timestamp`, `media_time`,
  `version`) for tooling that wants the parsed type directly. Six
  tests in `tests/random_access.rs` cover v0 round-trip, v1
  round-trip with 48-bit `media_time`, truncation error paths (below
  16-byte floor, v0-missing-media_time, v1-partial-media_time), and
  an end-to-end integration that splices two `prft` boxes into a
  real fragmented MP4 and confirms the demuxer's `metadata()`
  surfaces both `prft_0` and `prft_1` with their expected values.

- Sample-group muxing (ISO/IEC 14496-12 §8.9.2 SampleToGroupBox +
  §8.9.3 SampleGroupDescriptionBox) — write-side dual of the
  pre-existing `sbgp` / `sgpd` demux. New `oxideav_mp4::sample_groups`
  module exposes `SampleToGroup` and `SampleGroupDescription` types
  plus stateless byte builders `build_sbgp(&SampleToGroup) → Vec<u8>`
  and `build_sgpd(&SampleGroupDescription) → Vec<u8>`. `Mp4MuxerOptions`
  gains a `track_sample_groups: Vec<TrackSampleGroups>` field — each
  entry binds a `stream_index` to lists of `sbgp` / `sgpd` boxes to
  emit on that track's `stbl` (after the chunk-offset table; `sgpd`
  before `sbgp` per §8.5.1). The grouping-type-specific entry payload
  is opaque to the container — callers supply already-serialised
  `Vec<u8>` per entry. Version pick for `sgpd` is automatic per
  §8.9.3.2: v2 when `default_sample_description_index = Some(_)` with
  shared-length entries, v1 with fixed `default_length` for
  shared-length entries, v1 with per-entry `description_length` for
  mixed-length entries (the deprecated v0 no-length-signalling form
  is not emitted). `sbgp` picks v0 (no `grouping_type_parameter`) or
  v1 (`Some(_)`) per §8.9.2; movie-fragment-local indices ≥ `0x10001`
  (§8.9.4) are written verbatim, the muxer does not resolve them.
  Ten unit tests in `src/sample_groups.rs` cover the byte layout
  (sbgp v0 / v1, sgpd v1 fixed / v1 variable / v2, zero-entry boxes,
  fragment-local index preservation, v2-fallback-to-v1 when entries
  differ); five integration tests in `tests/sample_groups_mux.rs` mux
  a PCM track with caller-supplied groups and re-demux to verify the
  surfaced `params.options` keys round-trip exactly as encoded.

- Public `styp` module — write-side Segment Type Box (`styp`,
  ISO/IEC 14496-12 §8.16.2) byte emitter mirroring the read-side
  `parse_styp` landed in oxideav-mov. `build_styp(major, compat) →
  Vec<u8>` and the streaming dual `write_styp(writer, major, compat)`
  produce the spec layout `[size:u32][type:'styp'][major:4]
  [minor:u32=0][compat:4]*N`; `build_styp_with_minor` / `write_styp_with_minor`
  preserve a non-zero `minor_version` for parity with the §4.3 `ftyp`
  shape. Caller-driven per-segment control is wired through
  `FragmentedMuxer::write_fragmented_segment_with_styp(major_brand,
  compat_brands)` — an inherent method that marks the *next* emitted
  segment's `styp` to use the given DASH/CMAF `(major, compat)` pair,
  overriding the configured `FragmentedOptions::styp` preset for one
  segment (the override is consumed on use and subsequent segments fall
  back to the preset). A new `open_fragmented_typed` factory returns the
  concrete `FragmentedMuxer` (instead of the trait-object form used by
  the registry) so callers can reach the inherent method. Seven byte-
  exact integration tests in `tests/styp_write.rs` cover the byte
  layout, the override semantics, segment placement (`styp` immediately
  precedes `moof`), the empty-compat 16-byte form, and the
  preset-vs-override interaction.

- `cargo-fuzz` target `demux` over the BMFF box-tree walker. Feeds
  arbitrary bytes through `demux::open` (with `NullCodecResolver`),
  exercises `streams()` / `metadata()` / `duration_micros()`, drains
  up to 256 packets via `next_packet()`, and re-runs the sample-table
  walker via `seek_to(0, 0)`. Bounded so a pathological-but-legitimate
  stream cannot dominate fuzz time. Lives in its own `[workspace]` so
  it does not interfere with the umbrella; `fuzz/Cargo.lock` is
  committed for reproducibility while the library root keeps
  `Cargo.lock` ignored. A small seed corpus (minimal ftyp+moov, ftyp
  alone, empty moov, plus two regression artefacts for the fixes
  below) lives at `fuzz/corpus/demux/`.

- Sample Dependency Type Box (`sdtp`, ISO/IEC 14496-12 §8.6.4) demux.
  The `stbl` parser now reads the optional SampleDependencyTypeBox —
  a `FullBox(version = 0, flags = 0)` whose body, after the FullBox
  preamble, is one byte per sample carrying four 2-bit fields packed
  MSB-first (`is_leading`, `sample_depends_on`,
  `sample_is_depended_on`, `sample_has_redundancy`). The
  `sample_count` is implicit from `stsz` / `stz2` so the table's
  length is simply `body.len() - 4` bytes — no entry count is
  re-stated. Each per-sample 2-bit field uses the spec's four-valued
  enum (§8.6.4.3 — e.g. `sample_depends_on = 2` → "I-picture";
  `sample_is_depended_on = 2` → "no other sample depends on this,
  safe to drop in trick mode"; `is_leading = 3` → "leading sample
  decodable from the prior referenced I-picture"). The raw values
  are stored on the track for downstream renderers and seek
  heuristics. Surfaced on `StreamInfo.params.options` as a small
  summary rather than per-sample (per-sample would flood the map for
  a typical track): five keys — `sdtp_count` (total entries),
  `sdtp_leading_count` (samples with `is_leading ∈ {1, 3}`),
  `sdtp_independent_count` (samples with `sample_depends_on = 2`,
  i.e. I-pictures per the dependency hint), `sdtp_disposable_count`
  (samples with `sample_is_depended_on = 2`, safe to drop), and
  `sdtp_redundant_count` (samples with `sample_has_redundancy = 1`).
  A too-short box (less than the FullBox preamble) is rejected as
  malformed; a track with no `sdtp` emits none of the keys (the
  demuxer falls back to `stss` for keyframe detection). Verified by
  eight unit tests (two-entry decode, empty-table acceptance,
  too-short rejection, MSB-first bit-packing-order pivot, all-zero
  "unknown" entries, `stbl` pickup, options surfacing with counts,
  and absence-omits-options).
- Sample Group structures (`sbgp` + `sgpd`, ISO/IEC 14496-12 §8.9)
  demux. The `stbl` parser now reads the SampleToGroupBox (`sbgp`,
  §8.9.2) and SampleGroupDescriptionBox (`sgpd`, §8.9.3). A track may
  carry several of each — one pair per `grouping_type` (e.g. `roll`,
  `rap `, `sync`, `alst`, `prol`) — so both are accumulated rather than
  overwritten. The `sbgp` is a `FullBox(version, 0)`: `grouping_type`,
  an optional `grouping_type_parameter` (version 1 only), then a
  run-length table of `(sample_count, group_description_index)` pairs
  mapping decode-order sample runs to a description index (index 0 =
  "member of no group of this type"; an index ≥ 0x10001 is a
  movie-fragment-local reference per §8.9.4 and is preserved verbatim —
  the demuxer does not resolve fragment-local groups). The `sgpd` is a
  `FullBox(version, 0)`: `grouping_type`, an optional `default_length`
  (version 1), an optional `default_sample_description_index`
  (version ≥ 2), then per-group descriptive entries. Entry sizing
  follows §8.9.3.2: version 1 with a non-zero `default_length` gives
  fixed-size entries; version 1 with `default_length == 0` prefixes
  each entry with a `u32` `description_length`; version 0 carries no
  per-entry length signalling (§8.9.3.3 NOTE — its use is deprecated
  precisely because entries can't be scanned), so the remaining body is
  captured as one combined blob rather than guessing a fixed entry
  size. The entry payloads are grouping-type-specific blobs the
  container does not interpret — they are surfaced verbatim. Each
  `sbgp` is exposed on `StreamInfo.params.options` as `sbgp_<n>`
  (0-based encounter index): the grouping type, an optional `param=<P>`,
  then space-separated `count:index` pairs. Each `sgpd` is exposed as
  `sgpd_<n>`: the grouping type, an optional `default=<D>`, then the
  per-group entry payloads rendered as lowercase hex. The two share
  `grouping_type` so a caller can pair an `sbgp` with the `sgpd` of the
  same type. Truncated, too-short, or over-claimed boxes are rejected
  as malformed; a track with no sample groups emits none of the keys.
  Verified by 14 unit tests (sbgp v0 / v1-with-parameter /
  fragment-local-index / empty / truncated / too-short; sgpd v1-fixed /
  v1-variable / v2-default-index / v0-combined-blob / variable-truncated;
  `stbl` accumulation; options surfacing; absence-omits-options).
- Shadow Sync Sample Box (`stsh`, ISO/IEC 14496-12 §8.6.3) demux.
  The `stbl` parser now reads the optional ShadowSyncSampleBox — a
  `FullBox(version = 0, flags = 0)` whose body is a `u32` `entry_count`
  followed by that many `(shadowed_sample_number, sync_sample_number)`
  pairs (each a big-endian `u32`). The first member names a (normally
  non-sync) sample; the second names a sync sample (key frame) that can
  be decoded in its place when seeking to or before the shadowed
  sample (§8.6.3.1). The table is a pure seek optimisation — it is
  ignored in normal forward play and a track decodes correctly without
  it. Each pair is surfaced on `StreamInfo.params.options` as
  `stsh_<n>` (0-based encounter index) with value `"shadowed sync"`
  (both 1-based sample numbers, space-separated, mirroring the
  `tref_<type>` / `kind_<n>` convention). A too-short or truncated box
  is rejected as malformed; an adversarial `entry_count` cannot trigger
  a giant up-front allocation (capacity is clamped to the body's byte
  budget) and the per-entry bounds check rejects an over-claimed count.
  A track with no `stsh` emits none of the keys. Verified by eight unit
  tests (two-entry read, empty table, too-short rejection, mid-entry
  truncation rejection, huge-count rejection, `stbl` pickup, options
  surfacing, and absence-omits-options).
- Composition to Decode Box (`cslg`, ISO/IEC 14496-12 §8.6.1.4)
  demux. The `stbl` parser now reads the optional
  CompositionToDecodeBox — a `FullBox(version, 0)` whose body is
  five signed integers (32-bit in version 0, 64-bit in version 1):
  `compositionToDTSShift`, `leastDecodeToDisplayDelta`,
  `greatestDecodeToDisplayDelta`, `compositionStartTime`, and
  `compositionEndTime`. When signed (v1) `ctts` composition offsets
  are in use, this box documents the composition↔decode timeline
  relationship: the DTS shift that guarantees `CTS ≥ DTS` for every
  sample (honouring the profile/level buffer model), the least and
  greatest composition offsets, and the smallest/largest computed
  composition times (`compositionEndTime = 0` means "unknown" per
  §8.6.1.4.3). All five fields are widened to `i64` on read so
  callers see one shape regardless of the on-wire version, and are
  surfaced on `StreamInfo.params.options` as `cslg_<field>` decimal
  strings (media timescale). A too-short or truncated box is
  rejected as malformed rather than zero-filled; a track with no
  `cslg` emits none of the keys. Verified by seven unit tests (v0
  layout, v1 64-bit value past `i32::MAX`, too-short rejection,
  mid-field truncation rejection, `stbl` pickup, options surfacing,
  and absence-omits-options).
- Track Kind Box (`kind`, ISO/IEC 14496-12 §8.10.4) demux. The `trak`
  parser now walks the track-level `udta` container (previously
  skipped — only moov-level `udta` was read) and picks up each
  `KindBox`: a `FullBox(version = 0, flags = 0)` preamble followed
  by two NULL-terminated UTF-8 C strings, `schemeURI` then `value`
  (§8.10.4.2). Per §8.10.4.3 the URI alone identifies the kind when
  no value follows; when a value is present the URI identifies a
  naming scheme (e.g. `urn:mpeg:dash:role:2011`) and `value` is the
  kind name (`main`, `caption`, `commentary`, …). Multiple `kind`
  boxes per track are supported — the spec note explicitly allows
  several schemes co-labelling the same track. Each parsed pair is
  surfaced on `StreamInfo.params.options` as `kind_<n>` (0-based
  encounter index); the value is the URI alone when the box carried
  no name, or `"URI value"` (space-separated, mirroring the
  `tref_<type>` convention) when both fields are populated. A
  too-short / empty-URI / non-UTF-8 box is silently skipped rather
  than failing the demux, since the box is optional. Verified by
  ten unit tests (URI-only, URI+value, missing-value-NUL
  tolerance, empty-URI rejection, too-short skip, multi-kind
  collection inside one `udta`, non-kind-children skip, options
  surfacing with and without the box, and an end-to-end
  nested-in-`trak/udta` pickup).
- Extended language tag (`elng`, ISO/IEC 14496-12 §8.4.6) demux. The
  `mdia` parser now reads the optional ExtendedLanguageBox — a
  `FullBox` (version 0, flags 0) preamble followed by a
  NULL-terminated UTF-8 BCP 47 (RFC 4646) language tag such as
  `en-US`, `zh-Hant-HK`, or `es-419`. The tag is surfaced on
  `StreamInfo.params.options["language"]`; it is richer than
  `mdhd`'s packed 3-character ISO 639-2 code (which cannot express
  region / script / variant subtags) and overrides it per §8.4.6.1.
  Tracks with no `elng` omit the option (callers fall back to the
  `mdhd` language). The tag is read up to the first NUL (a missing
  terminator is tolerated); a too-short or empty box is silently
  skipped rather than failing the demux, since the box is optional.
  Verified by six unit tests (BCP 47 read, region/script subtags +
  missing-NUL tolerance, malformed/empty skip, options surfacing
  with and without the box, and an end-to-end nested-in-`mdia`
  pickup).

### Fixed

- DoS — three classes of attacker-controlled crash in the box-tree
  walker, caught by the new `demux` fuzz target:
  - `read_box_header` panicked on `size = 2..=7` with a `total_size −
    header_len` subtraction overflow (size 2..=7 is malformed —
    smaller than the header itself, implying a negative body), and
    on `size = 1` + `largesize = 0..=15` (the largesize form's
    16-byte header has the same minimum) with the same underflow.
    Both now return `Error::invalid("MP4: box size < 8")` /
    `Error::invalid("MP4: box largesize < 16")` before the math.
  - `read_box_body` did `vec![0u8; payload_size as usize]` against an
    unverified declared size, OOMing on a 9-byte input whose declared
    body length was ~4 GiB. Now uses `Read::take` + `read_to_end` so
    the allocation matches what the input actually delivers, surfacing
    a truncation as `Error::invalid("MP4: truncated box body")`. The
    sibling helper `read_bytes_vec` (used by the intra-`moov` walker
    for `trak`/`mvhd`/`udta`/`meta`/`mvex` payloads and the `moof`
    capture path) got the same treatment.
  - `parse_moov` and its sibling walkers (`parse_mvex`,
    `parse_track_udta`, `parse_minf`, `parse_stbl`, the fragmented
    `traf`/`trun` parsers) advanced the in-memory cursor with
    `cur.set_position(cur.position() + psz as u64)` over an unknown
    box, which panicked with `attempt to add with overflow` when
    `psz` was 32-bit-max-class. Replaced all 13 call sites with a
    new `skip_cursor_bytes` helper that does `saturating_add` and
    clamps to the buffer end, letting the surrounding `while
    cur.position() < end` loop terminate cleanly on the next
    iteration.

- `mux_roundtrip.rs` edit-list helper now uses a unique temp file
  per call, fixing an intermittent failure when two tests muxing
  with identical `(start_pts, write_edit_list)` args ran in parallel
  and truncated each other's output mid-read.

- Edit-list **muxer** support (`edts`/`elst`, ISO/IEC 14496-12
  §8.6.5–6). When a track's first packet carries a positive
  presentation timestamp, the muxer now writes a per-track Edit Box
  between `tkhd` and `mdia` holding a two-entry Edit List: a leading
  **empty edit** (`media_time = -1`) whose `segment_duration` is the
  start delay expressed in the movie timescale, followed by a normal
  `media_time = 0` segment covering the track's media duration — the
  §8.6.5 "An empty edit is used to offset the start time of a track"
  idiom. The box uses version 0 (32-bit) fields, auto-promoting to
  version 1 (64-bit) when any duration exceeds the 32-bit range. Tracks
  whose first PTS is zero/absent emit no `edts` (implicit 1:1 timeline).
  Controlled by the new `Mp4MuxerOptions::write_edit_list` flag
  (default `true`); set it `false` to suppress emission. The leading
  empty edit means the demuxer's existing leading-media-time shift
  (which acts only on the first non-empty edit, here `media_time = 0`)
  leaves demuxed timestamps unchanged, so a demux→mux→demux round-trip
  preserves packet bytes. Verified by three `build_edts` unit tests
  (absence on zero PTS, v0 layout, v1 promotion) and four integration
  tests (emission, no-emission on zero start, option suppression, and a
  full demuxer round-trip).
- Subtitle / timed-text **muxer** support for `mov_text` (`tx3g`,
  3GPP TS 26.245), `webvtt` (`wvtt`), `ttml` (`stpp`), `sbtt`, and
  `stxt`. Closes the round-trip loop with the existing subtitle
  demuxer: the muxer accepts the codec ids the demuxer surfaces and
  carries their `extradata` (the post-preamble sample-entry payload —
  tx3g's 18-byte default header, vttC config, stpp namespace strings,
  sbtt/stxt MIME strings) verbatim back into the new sample entry.
  Subtitle tracks emit the BMFF subtitle media-handler
  (`hdlr.handler_type = 'subt'`, BMFF §12.6.1) and SubtitleMediaHeader
  box (`sthd`, BMFF §12.6.2) for `wvtt`/`stpp`/`sbtt`/`stxt`; the
  `mov_text` codec maps to the BMFF text handler
  (`hdlr.handler_type = 'text'`, BMFF §12.5.1) and a null media header
  (`nmhd`, BMFF §12.5.2) because the 3GPP/QuickTime `tx3g` carriage
  is a text-handler form. Verified by five demux→mux→demux round-trip
  tests (codec id + media type + extradata + packet bytes) plus a
  routing test that scans the emitted bytes for the expected
  `text`/`subt` handler FourCC and `nmhd`/`sthd` media-header box.
- Track Reference Box (`tref`, ISO/IEC 14496-12 §8.3.3) demux. The
  `trak` parser now walks the `tref` container and collects each
  inner `TrackReferenceTypeBox` (whose FourCC is the reference type
  — `hint`, `cdsc`, `font`, `hind`, `vdep`, `vplx`, `subt`, `chap`,
  `tmcd`, etc. — and whose body is a packed big-endian `u32`
  `track_IDs[]` array). The parsed (`reference_type`, `track_IDs`)
  pairs are surfaced on `StreamInfo.params.options` as
  `tref_<type>` keys whose value is a space-separated list of
  referenced track IDs (e.g. `tref_chap = "3"`,
  `tref_subt = "10 11"`). Zero IDs (spec-prohibited per §8.3.3.3)
  are silently dropped; repeated `reference_type` boxes inside one
  `tref` keep the first occurrence (spec says at most one per
  type). Sub-box bodies whose length isn't a multiple of 4 are
  rejected as malformed.
- Subtitle / timed-text track demux (ISO/IEC 14496-12 §12.5–6).
  The handler-type box (`hdlr`) now recognises `subt` (BMFF
  Subtitle), `sbtl` (QuickTime subtitle), and `text` (BMFF
  timed text — used by 3GPP `tx3g` carriage) and lands the
  resulting track as `MediaType::Subtitle`. Sample-entry FourCC
  dispatch covers `tx3g` → `mov_text`, `text` → `text`, `wvtt`
  → `webvtt`, `stpp` → `ttml`, `sbtt` → `sbtt`, `stxt` →
  `stxt`, `c608` → `eia_608`, and `c708` → `eia_708`. The
  post-preamble bytes (3GPP `tx3g` display flags + colours +
  default text box; BMFF stpp / sbtt / stxt UTF-8 strings;
  WebVTT `vttC` config) are preserved verbatim in
  `params.extradata` for downstream renderers — no per-codec
  body parsing is performed (3GPP TS 26.245 is not in `docs/`
  and BMFF §12.5–6 leaves the strings caller-interpretable).
- Protected sample-entry unwrap (ISO/IEC 14496-12 §8.12). When a
  sample entry's outer FourCC is `encv` / `enca` / `enct` /
  `encs`, the demuxer walks the inner `sinf` container to
  recover the original (un-transformed) FourCC from `frma` and
  the protection scheme type from `schm`. The stream surfaces
  the un-transformed codec id (so decoders are set up as if the
  track were plain) and exposes the scheme on
  `params.options["protection_scheme"]` (e.g. `cenc`, `cbc1`,
  `cens`, `cbcs`) so callers can detect protection without
  decoding the sample bytes. CENC key management (`tenc`,
  `pssh`, `senc`, `saiz` / `saio`) is **not** implemented; that
  needs the base ISO/IEC 23001-7 spec, only AMD1 / 2019 of
  which is present in `docs/container/cenc/`.

## [0.0.7](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.6...v0.0.7) - 2026-05-06

### Other

- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- registry calls: rename make_decoder/make_encoder → first_decoder/first_encoder
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- switch mjpeg call to register_codecs ([#502](https://github.com/OxideAV/oxideav-mp4/pull/502))
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-mp4/pull/502))

## [0.0.6](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.5...v0.0.6) - 2026-05-04

### Other

- emit sidx + mfra/tfra/mfro random-access indexes
- parse sidx + mfra/tfra random-access indexes
- fragmented-MP4 (DASH/HLS/CMAF) writer
- fragmented-MP4 (DASH/HLS/CMAF) support
- clamp adversarial stsc samples_per_chunk to n_samples
- surface dac3 / dec3 sample-entry sub-boxes as extradata
- rustfmt fix for dts_fourcc test
- map AC-3, E-AC-3, DTS, G.711 sample-entry FourCCs

### Added

- Fragmented-MP4 muxer now emits `sidx` (SegmentIndexBox
  §8.16.3) before each `moof+mdat` and an `mfra`
  (MovieFragmentRandomAccessBox §8.8.10) trailer with
  per-track `tfra` (§8.8.11) + `mfro` (§8.8.13) at end of
  file. The pair lets DASH on-demand profile players seek
  by byte range without scanning every fragment, and lets
  HLS / Smooth-Streaming clients land directly on the right
  moof for a target presentation time. New
  `FragmentedOptions::emit_random_access_indexes` flag (default
  `true`) gates the emission for callers that prefer the
  prior bare-`moof+mdat` shape. Each emitted sidx is a
  one-reference index covering the immediately-following
  styp+moof+mdat (`first_offset = 0`, SAP type 1 when the
  anchor track's first sample is a sync sample); the mfra
  carries one tfra entry per sync sample per emitted
  fragment. Cross-validated against ffprobe / ffmpeg
  (parses every box and recovers byte-exact PCM payload).
- `sidx` (SegmentIndexBox §8.16.3) and `mfra/tfra` (Movie
  Fragment Random Access §8.8.10–11) parsers. The demuxer now
  reads them at open time and exposes `parse_sidx_box` /
  `parse_mfra_box` as public entry points for tooling.
  `seek_to` consults the per-track `tfra` table when present
  and lands directly on the moof byte offset of the last
  random-access point at-or-before the requested pts —
  O(log N) on the tfra plus a bounded scan within one
  fragment, instead of the prior O(N) walk over every sample.
  Falls through to the linear scan when no `mfra` is present.
- Fragmented-MP4 (DASH / HLS / Smooth-Streaming / CMAF) **mux**
  (ISO/IEC 14496-12 §8.8 + DASH-IF Interop). New
  `Mp4MuxerOptions::fragmented = Some(FragmentedOptions { .. })`
  switches the muxer into init-segment + media-segment shape:
  `ftyp + moov(mvex+trex)` first, then `styp? + moof + mdat`
  per fragment cadence boundary. Cadence policies:
  `EverySeconds(f64)` (default 2 s, anchored on track 0),
  `EveryKeyframe`, and `EveryNPackets(u32)` for tests.
  `tfhd` always sets `default-base-is-moof` (0x020000); per-
  fragment defaults (size / duration / flags) are emitted only
  when all samples in the run agree, otherwise per-sample
  fields land in `trun`. Round-trips byte-exactly through our
  own demuxer + verified against ffmpeg 8.1 as a black-box
  validator (PCM extraction matches the original packets).
  Two new registry entries: `dash` and `cmaf` (both pick the
  fragmented path with an `iso6` / `dash` / `cmfc` brand);
  the existing `ismv` entry now also emits real fragmented
  MP4 (was previously a non-fragmented file with an ISMV
  ftyp brand — Smooth-Streaming clients rejected it).
- Fragmented-MP4 (DASH / HLS / Smooth-Streaming / CMAF) demux
  (ISO/IEC 14496-12 §8.8). The top-level walk continues past
  `moov` and stitches each `moof`+`mdat` pair onto the
  per-track sample list. `mvex/trex` per-track defaults plus
  `mfhd`, `traf`, `tfhd`, `tfdt`, `trun` are honoured;
  `default-base-is-moof`, `tfhd.base_data_offset`, per-sample
  size / duration / flags / composition-time-offset overrides,
  and v0/v1 `tfdt.base_media_decode_time` all work.
  `styp`, `sidx`, `mfra` segment-marker boxes are skipped
  cleanly. Verified against an `ffmpeg -movflags
  +frag_keyframe+empty_moov` AAC fixture: all 88 packet
  positions and durations match `ffprobe -show_packets`
  byte-for-byte.
- Multi-segment edit list (`elst`): the full entry list is
  parsed and stored. The leading `media_time` shift (first
  non-empty edit) drives sample timestamps as before.
- Sample-entry FourCC mappings for AC-3 (`ac-3` / `AC-3` →
  `ac3`), E-AC-3 (`ec-3` / `EC-3` → `eac3`), DTS family
  (`dtsc` / `dtsh` / `dtsl` / `dtse` → `dts`), and G.711
  (`ulaw` → `pcm_mulaw`, `alaw` → `pcm_alaw`). MP4-RA
  `objectTypeIndication` 0xA5 / 0xA6 / 0xA9 inside an `mp4a`
  esds also now resolve to `ac3` / `eac3` / `dts` respectively.
- `dac3` (ETSI TS 102 366 Annex F.4) and `dec3` (Annex G.4)
  audio sample-entry sub-boxes are now parsed and surfaced as
  `params.extradata`, matching the existing `dfLa` / `dOps` /
  `avcC` / `hvcC` handling.

### Fixed

- Sample-table expansion (`expand_samples`) clamps adversarial
  `stsc` `samples_per_chunk` values to the track's total
  sample count. A malicious file with `samples_per_chunk =
  u32::MAX` previously caused the inner per-chunk loop to spin
  ~4 billion times before the `sample_i >= n_samples` guard
  fired — a multi-minute hang on a tiny input. The clamp keeps
  the inner loop's iteration count bounded by `n_samples` per
  entry, matching what well-formed files already exercise.

## [0.0.5](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.4...v0.0.5) - 2026-05-03

### Other

- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- parse ctts + edts/elst so packets carry composition timestamps
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

## [0.0.4](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.3...v0.0.4) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- map ProRes FourCCs (apco/apcs/apcn/apch/ap4h/ap4x) to codec_id "prores"
- bump oxideav-mjpeg dep to "0.1"
- bump oxideav-container dep to "0.1"
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- delegate codec-id lookup to CodecResolver (registry-backed)
- thread &dyn CodecResolver through open()
- drop unimplemented fragmentation flags + document codec IDs
- document demuxer + muxer scope, usage, and OTI dispatch
