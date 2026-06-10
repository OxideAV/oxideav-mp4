# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
