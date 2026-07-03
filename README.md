# oxideav-mp4

Pure-Rust **MP4 / ISO Base Media File Format** container — demuxer
(probe + sample-table expansion + seek) and muxer (moov-at-end by
default, optional faststart rewrite). Three brand presets share one
implementation: `mp4`, `mov` (QuickTime), and `ismv` (Smooth Streaming
ftyp, non-fragmented layout). Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-container = "0.1"
oxideav-mp4 = "0.0"
```

## Quick use

### Demux an MP4 and feed packets into a codec

```rust
use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

let mut codecs = CodecRegistry::new();
let mut containers = ContainerRegistry::new();
oxideav_mp4::register(&mut containers);
// ... register whichever codecs you care about (aac, flac, h264, mjpeg, ...)

let input: Box<dyn oxideav_container::ReadSeek> =
    Box::new(std::fs::File::open("clip.mp4")?);
let mut dmx = containers.open("mp4", input)?;

// Sample entries are resolved to concrete codec ids. For `mp4a`/`mp4v`
// tracks the esds `objectTypeIndication` is honoured, so MP3-in-mp4
// comes out as "mp3", MPEG-1 video as "mpeg1video", AAC as "aac", etc.
let stream = &dmx.streams()[0];
let mut dec = codecs.make_decoder(&stream.params)?;

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            dec.send_packet(&pkt)?;
            while let Ok(frame) = dec.receive_frame() {
                // ... use frame ...
                let _ = frame;
            }
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Mux packets into an MP4

```rust
use oxideav_container::WriteSeek;

let f = std::fs::File::create("out.mp4")?;
let ws: Box<dyn WriteSeek> = Box::new(f);
let mut mux = oxideav_mp4::muxer::open(ws, &streams)?;
mux.write_header()?;
for pkt in packets { mux.write_packet(&pkt)?; }
mux.write_trailer()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Faststart (moov-at-front) layout

```rust
use oxideav_mp4::{BrandPreset, Mp4MuxerOptions};

let opts = Mp4MuxerOptions {
    brand: BrandPreset::Mp4,
    faststart: true,
    ..Mp4MuxerOptions::default()
};
let mut mux = oxideav_mp4::muxer::open_with_options(ws, &streams, opts)?;
```

In faststart mode the muxer buffers mdat in memory and writes
`[ftyp][moov][mdat]` at `write_trailer` time, patching chunk offsets so
the file is streamable from the first byte.

## Scope

### Demuxer

Sample-entry FourCCs resolve to these codec ids:

| FourCC                   | Codec id                                |
|--------------------------|-----------------------------------------|
| `mp4a`                   | `aac` (default); esds OTI refines to `mp3` (0x69/0x6B), `ac3` (0xA5), `eac3` (0xA6), `dts` (0xA9) |
| `mp4v`                   | `mpeg4video` (default); esds OTI refines to `mpeg1video`, `mpeg2video`, `h264`, `h265`, `mjpeg` |
| `alac`                   | `alac`                                  |
| `fLaC` / `flac`          | `flac`                                  |
| `Opus` / `opus`          | `opus`                                  |
| `ac-3` / `AC-3`          | `ac3` (Dolby Digital, ETSI TS 102 366 Annex F) |
| `ec-3` / `EC-3`          | `eac3` (Dolby Digital Plus, ETSI TS 102 366 Annex G) |
| `dtsc` / `dtsh` / `dtsl` / `dtse` | `dts` (DTS Coherent Acoustics / DTS-HD HR / DTS-HD MA / DTS Express, ETSI TS 102 114) |
| `ulaw` / `alaw`          | `pcm_mulaw` / `pcm_alaw` (G.711)        |
| `avc1` / `avc3`          | `h264`                                  |
| `hvc1` / `hev1`          | `h265`                                  |
| `vp08`                   | `vp8`                                   |
| `vp09`                   | `vp9`                                   |
| `av01`                   | `av1`                                   |
| `jpeg` / `mjpa` / `mjpb` | `mjpeg`                                 |
| `s263` / `h263`          | `h263`                                  |
| `lpcm` / `sowt` / `twos` | `pcm_s16le` (endianness of `twos` is not re-swapped) |
| `tx3g`                   | `mov_text` (3GPP TS 26.245 timed text — "movtext") |
| `text`                   | `text` (QuickTime plain text)            |
| `wvtt`                   | `webvtt` (W3C WebVTT-in-ISOBMFF)         |
| `stpp`                   | `ttml` (XML / TTML subtitle)             |
| `sbtt` / `stxt`          | `sbtt` / `stxt` (BMFF §12.5–6 text)      |
| `c608` / `c708`          | `eia_608` / `eia_708` (closed captions)  |
| `encv` / `enca` / `enct` / `encs` | original FourCC recovered from `sinf/frma`; `params.options["protection_scheme"]` carries the `schm.scheme_type` (e.g. `cenc`, `cbcs`) |
| any other                | `mp4:<fourcc>` — callers can register their own decoder |

- Codec-specific config records (`avcC`, `hvcC`, `av1C`, `vpcC`,
  `dfLa`, `dOps`, `dac3`, `dec3`, esds DSI) are forwarded as
  `extradata`.
- Sample-table expansion: `stts`, `stsc`, `stsz`/`stz2`, `stco`/`co64`,
  `stss`. `next_packet` serves samples in file-offset order.
- Fragmented MP4 (DASH / HLS / Smooth Streaming / CMAF): `mvex/trex`
  per-track defaults plus zero or more trailing `moof`+`mdat` pairs
  (`mfhd`, `traf`, `tfhd`, `tfdt`, `trun`) are stitched onto the
  initial sample table. `default-base-is-moof`, per-sample
  size/duration/flags/composition-time-offset overrides, and
  `tfhd.base_data_offset` are all honoured. `styp`, `sidx`, and `mfra`
  segment-index boxes are skipped (segment-precision seek hint
  consumption is a follow-up).
- Movie extends header (ISO/IEC 14496-12 §8.8.2, `mehd`): a sealed
  fragmented file's `mvex/mehd` MovieExtendsHeaderBox carries the
  overall presentation `fragment_duration` (in the movie timescale,
  per §8.8.2.3) — including all `moof` fragments — that an authoring
  step can write once the fragments are laid down. Both versions are
  read (v0 32-bit, v1 64-bit widened to `u64`). When present and
  non-zero, the value drives `Demuxer::duration_micros` and takes
  precedence over `mvhd.duration` — sealed fragmented files commonly
  carry `mvhd.duration = 0` (the moov has no resident samples that
  contribute to a non-fragment duration) and `mehd` is then the only
  authoritative total. The raw value (in the movie timescale) is also
  surfaced verbatim via `Demuxer::metadata()` as the
  `mehd_fragment_duration` key, for tooling wanting the untranslated
  number (e.g. CMAF live-edge probes confirming a producer's sealed
  total against their own running tally). Absent `mehd`, the key is
  not emitted and `duration_micros` falls back to `mvhd.duration` as
  before.
- Seek: `seek_to(stream, pts)` lands on the nearest sync-sample ≤ pts
  (or the first keyframe of the stream if none qualify).
- Metadata: 3GPP `udta` boxes (`titl`/`auth`/…) and iTunes-style
  `meta`/`ilst` are surfaced via `Demuxer::metadata()`.
- HEIF / MIAF item catalogue (ISO/IEC 14496-12 §8.11): a *file-level*
  `meta` box's item infrastructure is decoded into the public
  `demux::MetaItems` record — `pitm` (PrimaryItemBox §8.11.4), `iloc`
  (ItemLocationBox §8.11.3, the full v0/v1/v2 field-width-selector +
  construction-method + per-extent layout), `iinf`/`infe` (ItemInfoBox
  §8.11.6, every `infe` version 0–3, including the v2/3 32-bit
  `item_type` FourCC) and `iref` (ItemReferenceBox §8.11.12, typed
  from→to item-ID groups), plus the `meta`'s `hdlr` handler type. The
  structured records are reachable via `Mp4Demuxer::meta_items()`; a
  flat summary appears on `Demuxer::metadata()` as `meta_handler` /
  `meta_primary_item` / `meta_item_count` / `meta_item_<n>` /
  `meta_iloc_count` / `meta_iloc_<n>` (`id=<item_id>
  method=<construction_method> extents=<n> length=<total>` — where each
  item lives without downcasting) / `meta_iref_count` / `meta_iref_<n>`
  (`type=<fourcc> from=<item_id> to=<id,…>` — the relationship graph:
  thumbnail `thmb`, auxiliary `auxl`, derivation `dimg`, description
  `cdsc`, pre-derived `base`, predictive `pred`, tile-base `tbas`,
  scalable-base `exbl`, …). A plain iTunes `meta` (just
  `hdlr` + `ilst`) carries no §8.11 items and emits no `meta_*` keys.
  The standalone parsers `demux::parse_iloc_box` / `parse_pitm_box` /
  `parse_iinf_box` / `parse_iref_box` / `parse_meta_items` are public.
  This crate surfaces the item *catalogue* (locate / type / relate the
  items); it does not decode an item's codec payload. The `idat`
  (ItemDataBox §8.11.11) bytes are captured into `MetaItems::idat`;
  `MetaItems::item_byte_ranges(id)` resolves an item's extents to
  `(offset, length)` pairs (base-offset folded in, relative to the
  item's `construction_method` data origin) and
  `MetaItems::item_data_from_idat(id)` materialises an idat-resident
  item's concatenated bytes (with bounds checks). `meta_idat_len` is
  surfaced on `Demuxer::metadata()` when an `idat` is present. Write
  counterparts `demux::build_iloc_box` / `build_pitm_box` /
  `build_iinf_box` / `build_iref_box` / `build_idat_box` serialise each
  box byte-exact (the inverse of the parsers), so a caller can assemble
  a HEIF-style `meta` from `MetaItems` records; `build_iloc_box`
  rejects records that would not round-trip (bad field width, a v0 item
  with a non-zero construction method, etc.).
- HEIF / MIAF item properties (ISO/IEC 23008-12 §9.3 / §6.5, `iprp` /
  `ipco` / `ipma`): a file-level `meta`'s `iprp` ItemPropertiesBox is
  decoded into the public `demux::ItemProperties` (reachable via
  `MetaItems::iprp`) — the `ipco` ItemPropertyContainerBox property list
  (an implicitly 1-indexed sequence of property boxes) plus every `ipma`
  ItemPropertyAssociation box merged into per-item `(essential,
  property_index)` lists. `ItemProperties::properties_for(item_id)`
  resolves an item to its ordered `(essential, &ItemProperty)` pairs
  (transformative properties apply in sequence, §6.5.1; index-0 /
  out-of-range references are skipped) and `ItemProperties::property(idx)`
  looks up the 1-based `ipco` slot. The typed `demux::ItemProperty` enum
  models the base property set: `ispe` ImageSpatialExtents (§6.5.3),
  `pixi` PixelInformation (§6.5.6), `rloc` RelativeLocation (§6.5.7),
  `auxC` AuxiliaryType (§6.5.8, NUL-terminated URN + subtype tail), `irot`
  ImageRotation (§6.5.10), `imir` ImageMirroring (§6.5.12), `lsel`
  LayerSelector (§6.5.11), `udes` UserDescription (§6.5.20,
  lang/name/description/tags), `altt` AccessibilityText (§6.5.21), `iscl`
  ImageScaling (§6.5.13, a transformative H/V scale-ratio property), `rref`
  RequiredReferenceTypes (§6.5.17), `crtt` / `mdft` Creation / Modification
  time (§6.5.18–19, µs since 1904-01-01 UTC), plus `pasp` / `clap` / `colr`
  reusing this crate's existing 14496-12 records (§6.5.4 / §6.5.9 /
  §6.5.5); any unrecognised property box is preserved verbatim as
  `ItemProperty::Other` so its 1-based index slot survives a round-trip
  (the index an `ipma` references must stay stable). Reserved high bits on
  `irot` / `imir` are masked on read, and a property truncated for its
  declared type falls back to `Other` rather than reading past the end.
  The demuxer surfaces a compact summary on `Demuxer::metadata()`:
  `meta_iprp_property_count`, `meta_iprp_property_<n>` (a `<fourcc>
  <decoded>` token), and `meta_iprp_item_<n>` (`id=<id>
  props=<idx[,idx*…]>`, `*` marking an essential association). Public
  byte-exact builders `demux::build_iprp_box` / `build_ipco_box` /
  `build_ipma_box` / `build_item_property` are the inverses of the parsers
  (`parse_iprp_box` / `parse_ipco_box` / `parse_ipma_box`);
  `build_ipma_box` auto-selects the narrowest item-ID width (v0 16-bit / v1
  32-bit) and `property_index` width (`flags & 1` 7- / 15-bit), sorts
  entries by ascending `item_ID` (§9.3.1), and rejects an index past the
  15-bit ceiling. Absent `iprp`, no keys are emitted. This crate surfaces
  the property *catalogue*; applying a transform (rotate / mirror / scale /
  crop) to a decoded item is a renderer concern.
- HEIF / MIAF entity groups (ISO/IEC 23008-12 §9.4, `grpl` /
  `EntityToGroupBox`): a file-level `meta`'s `grpl` GroupsListBox is
  decoded into the public `demux::EntityGroups` (reachable via
  `MetaItems::grpl`) — every `EntityToGroupBox` (a FullBox whose FourCC is
  the `grouping_type`: `altr` alternatives where only one entity should be
  played, `ster` a two-entity stereo pair with entity 0 = left view, …)
  becomes a typed `demux::EntityToGroup` carrying the `grouping_type`,
  `group_id`, and the `entity_id` list (item IDs, or track IDs at file
  level). `EntityGroups::by_type(grouping_type)` filters (e.g. all `altr`
  alternative sets) and `EntityGroups::by_id(group_id)` locates a group.
  Surfaced on `Demuxer::metadata()` as `meta_grpl_group_count` plus
  `meta_grpl_group_<n>` (`type=<fourcc> id=<id> entities=<id,…>`). Public
  byte-exact builders `demux::build_grpl_box` / `build_entity_to_group_box`
  are the inverses of `parse_grpl_box`. An `EntityToGroupBox` whose
  `num_entities_in_group` overruns its body is dropped without aborting the
  `grpl` walk. Absent `grpl`, no keys are emitted.
- Extended language tag (ISO/IEC 14496-12 §8.4.6, `elng`): a track's
  `mdia/elng` ExtendedLanguageBox carries a NULL-terminated BCP 47
  (RFC 4646) tag richer than `mdhd`'s packed 3-char ISO 639-2 code
  (region / script / variant subtags). When present it is surfaced on
  `params.options["language"]` (e.g. `en-US`, `zh-Hant-HK`,
  `es-419`) and, per §8.4.6.1, overrides the `mdhd` language. Absent
  `elng`, the option is omitted (callers fall back to `mdhd`).
- Handler-type recognition (ISO/IEC 14496-12 §8.4.3): `soun` →
  Audio, `vide` → Video, `subt` / `sbtl` / `text` → Subtitle,
  `meta` → Data. Subtitle sample entries (`tx3g`, `text`, `wvtt`,
  `stpp`, `sbtt`, `stxt`, `c608`, `c708`) come out with
  `MediaType::Subtitle` and the per-codec id from the table above;
  their post-preamble payload (BMFF strings / tx3g header / vttC)
  is preserved verbatim in `params.extradata` for downstream
  renderers.
- Protected sample-entry unwrap (ISO/IEC 14496-12 §8.12): when the
  outer FourCC is `encv` / `enca` / `enct` / `encs`, the demuxer
  walks the inner `sinf` to recover the original codec FourCC
  from `frma` and the protection scheme from `schm`. The stream
  surfaces the un-transformed codec id so downstream decoders can
  be set up normally; `params.options["protection_scheme"]`
  carries the four-char scheme type (e.g. `cenc`, `cbcs`) so
  callers know packet payloads are still ciphertext.
- CENC metadata parsing (ISO/IEC 23001-7:2016): the three boxes
  that carry encryption framing on top of the §8.12 envelope are
  parsed structurally and surfaced to callers; the AES-128
  CTR / CBC decryption step itself lives in the `cenc_cipher`
  module (key acquisition stays with the caller).
  - `tenc` (§8.2 TrackEncryptionBox) — discovered inside
    `sinf/schi`. v0 captures `default_isProtected`,
    `default_Per_Sample_IV_Size`, and `default_KID`; v1 adds the
    `default_crypt_byte_block` / `default_skip_byte_block` pattern
    pair (for `cens` / `cbcs` schemes) and the
    `default_constant_IV` used when `isProtected==1 && IV_size==0`.
    Surfaced on `params.options` as `cenc_default_kid` (lowercase
    hex), `cenc_default_is_protected`, `cenc_default_iv_size`,
    `cenc_tenc_version`, and (v1 only) `cenc_default_crypt_byte_block`
    / `cenc_default_skip_byte_block` / `cenc_default_constant_iv`.
  - `pssh` (§8.1 ProtectionSystemSpecificHeaderBox) — collected at
    moov level. Each entry captures the 16-byte SystemID UUID,
    optional v1 KID list, and the DRM-system-specific opaque
    `Data` blob. Surfaced via `Demuxer::metadata()` as `pssh_<n>`
    keys with value `"<system_id_hex> <kid_count> <data_len>"`;
    structured records are reachable through the public
    `cenc::PsshBox` type for callers that downcast.
  - moof-level `pssh` (§8.1.1 — "Container: Movie (`moov`) or
    Movie Fragment (`moof`)"). pssh boxes that live directly inside
    a `moof` are collected per fragment and keyed by the enclosing
    `mfhd.sequence_number` so a downstream DRM layer can honour the
    §8.1.1 reader rule "examine all Protection System Specific
    Header boxes in the Movie Box and in the Movie Fragment Box
    associated with the sample (but not those in other Movie
    Fragment Boxes)". Surfaced via `Demuxer::metadata()` as
    `moof_pssh_<n>` keys with value
    `"systemid=<hex> seq=<mfhd_seq> kids=<n> data=<len>"`;
    structured records are reachable through the public
    `demux::MoofPsshRecord` type (one record per box in moof-walk
    order, `version` 0 + v1 with KID list both supported). A
    malformed pssh inside a moof is dropped without aborting the
    fragment, mirroring the moov-level recovery policy.
  - `senc` (§7.2 SampleEncryptionBox) — collected from every `traf`
    whose matching track carried a `tenc` default (so the
    per-sample IV width is recoverable per §7.2.3). Captures
    `flags`, the per-sample `InitializationVector`, and (when
    `UseSubSampleEncryption` is set) the
    `{BytesOfClearData, BytesOfProtectedData}` subsample map.
    Surfaced via `Demuxer::metadata()` as `senc_<n>` keys with
    value `"track=<idx> seq=<mfhd_seq> samples=<n>
    flags=0x<hex>"`; structured records are reachable through the
    public `cenc::SencBox` type.

  The standalone parsers (`cenc::parse_tenc` / `cenc::parse_pssh`
  / `cenc::parse_senc`) are public for callers that already have
  the box body in hand from another path (e.g. a non-MP4 carrier
  of CENC framing).
  - Typed scheme decision router (§4.2 + §10). The four `scheme_type`
    FourCCs defined in ISO/IEC 23001-7:2016 §10 — `cenc` (AES-CTR
    full / NAL-subsample), `cbc1` (AES-CBC full / NAL-subsample),
    `cens` (AES-CTR pattern subsample), `cbcs` (AES-CBC pattern
    subsample) — are exposed as the typed `cenc::CencScheme` enum.
    The two binary axes a downstream decryptor dispatches on —
    cipher mode (CTR vs CBC, `cenc::CipherMode`) and pattern-encryption
    flag — are surfaced via `CencScheme::cipher_mode()` and
    `CencScheme::uses_pattern_encryption()`. A scheme value bundled
    with the parsed `tenc` becomes a `cenc::CencSchemeDecision` —
    the typed routing slip a future AES layer can pattern-match
    against — built via `CencSchemeDecision::new(scheme, tenc)`
    with structural validation only (the scheme's
    `required_tenc_version()` must match `tenc.version`; pattern
    schemes must carry a non-zero `(crypt_byte_block,
    skip_byte_block)` pair per §9.6). The track-default IV-supply
    discipline (§9.1) — per-sample 8/16-byte IV vs constant IV vs
    no IV when `isProtected == 0` — is recovered via
    `CencSchemeDecision::iv_supply()` as the typed `cenc::IvSupply`
    enum. The bundle is a static dispatch contract built from
    container-side bytes only; the AES execution layer that
    consumes it is `cenc_cipher::decrypt_sample_in_place`, with
    key material (looked up by KID from the named `pssh.SystemID`
    system) supplied by the caller.
    Unknown `scheme_type` FourCCs are preserved verbatim through
    `CencScheme::Unknown([u8; 4])` so a caller carrying a private
    DRM dialect can still route on its own table.
  - Typed `CencSampleEncryptionInformationGroupEntry` parser (§6).
    A `seig` sample-group entry overrides the track-default `tenc`
    parameters for a group of samples — `(crypt_byte_block,
    skip_byte_block, isProtected, Per_Sample_IV_Size, KID,
    constant_IV?)` — and is the spec's mechanism for mixing
    encrypted and unencrypted samples in one track, or for
    rotating keys per scene. `cenc::parse_seig(body)` decodes one
    entry's payload (the opaque blob the existing `sgpd`
    `grouping_type = *b"seig"` surface already preserves) into the
    typed `cenc::SeigEntry`, mirroring the validation `parse_tenc`
    applies (constant-IV size ∈ {8, 16}, per-sample IV size ∈ {0,
    8, 16}). `SeigEntry::iv_supply()` and `::uses_pattern_encryption()`
    answer the same routing questions as the track-default
    accessors but at the per-group resolution. §6's
    "clients SHALL ignore additional bytes after the fields
    defined" rule is honoured — trailing bytes from a future
    edition do not fail the parse.
  - Per-sample cipher walker (§9.4 / §9.5 / §9.6).
    `cenc::plan_sample_cipher(decision, subsamples, sample_len)`
    consumes a parsed `CencSchemeDecision` plus the per-sample
    subsample list (from `parse_senc`) and returns the ordered
    `Vec<CipherStep>` partition of the sample plaintext into
    contiguous `(offset, len, kind, iv_restart)` runs an AES layer
    iterates: `Clear` runs pass through verbatim and `Encrypted` runs
    feed the cipher mode picked by `CencSchemeDecision::cipher_mode`
    (CTR vs CBC). The walker bakes in every §10-registered
    structural rule — full-sample CTR (§9.4.2) keeps the trailing
    partial block encrypted, full-sample CBC (§9.4.3) and `§9.7`
    whole-block leave it in the clear, subsample non-pattern is
    `Clear(BytesOfClearData) + Encrypted(BytesOfProtectedData)` per
    subsample with a continuous chain across them (§9.5.1),
    subsample pattern walks `crypt_byte_block * 16` encrypted +
    `skip_byte_block * 16` clear with the trailing partial
    `crypt_byte_block` reset to clear (§9.6), and the `cbcs` IV
    restart per subsample (§9.5.1) lights up only on the first
    `Encrypted` step of each subsample for `cbcs` (not `cenc` /
    `cbc1` / `cens`). §9.5.1 invariants are enforced —
    `Σ (clear+protected) == sample_len`, no both-zero subsample,
    no subsample running past sample_len. `IvSupply::None` and
    `CencScheme::Unknown` are rejected so a caller routes through
    its own "no cipher" / private-dialect path explicitly.
  - AES-128 CTR / CBC cipher driver (§9.3 / §9.4–§9.7, module
    `cenc_cipher`). `cenc_cipher::decrypt_sample_in_place(decision,
    key, per_sample_iv, subsamples, data)` resolves the IV
    discipline from the `CencSchemeDecision` (per-sample IV
    length-checked against `tenc`; constant IV pulled from
    `tenc.default_constant_IV`; supplying both is a §9.2 error),
    builds the `CipherStep` plan, and executes it in place against
    AES-128 (the `aes` / `ctr` / `cbc` primitive crates) — all
    four §10 schemes decrypt. The §9.1 IV expansion (8-byte IV →
    bytes 0..8, bytes 8..16 zero) is `cenc_cipher::expand_iv`; the
    §9.3 counter is the low 64 bits big-endian, starting at the
    expanded-IV value and wrapping to zero without carrying into
    bytes 0..8. §9.5.1 continuity is honoured: one continuous
    keystream / cipher chain spans the concatenated protected
    runs (a partial CTR block ending one subsample's protected
    range continues in the next; `cens` skip blocks consume no
    keystream; the `cbc1` chain crosses clear gaps), while `cbcs`
    reseeds its chain from the constant IV at each subsample's
    `iv_restart` step and chains across skip runs within the
    subsample. Encrypted CBC runs that are not a multiple of 16
    bytes are rejected (§9.4.3 / §10.2 — partial blocks are never
    CBC-encrypted). The step-level engine is public as
    `cenc_cipher::decrypt_steps_in_place(mode, key, iv, steps,
    data)` for `seig`-overridden sample groups where the caller
    assembles its own plan/IV pairing. Tested against a FIPS-197
    known-answer AES block anchor, first-principles ECB-built §9.3
    keystreams (including the 64-bit counter wrap), and synthetic
    known-key round-trips for every scheme × subsample × pattern
    shape (cens 2:1 counter continuity, cbcs 1:9 stride with
    chain-over-skip + per-subsample restart, §9.7 whole-block
    audio).
- Track references (ISO/IEC 14496-12 §8.3.3, `tref`): each typed
  `TrackReferenceTypeBox` inside `trak/tref` is parsed and the
  resulting `(reference_type → track_IDs)` pairs are surfaced on
  `params.options` as `tref_<type>` keys whose value is a
  space-separated list of referenced `track_ID`s (e.g.
  `tref_chap = "3"`, `tref_subt = "10 11"`, `tref_cdsc = "2"`).
  Useful for wiring subtitle→video (`subt`), chapter (`chap`),
  content description (`cdsc`), font (`font`), hint (`hint`),
  depth / parallax auxiliary video (`vdep` / `vplx`), and hint
  dependency (`hind`) relationships. `track_ID = 0` entries
  (spec-prohibited) are dropped. The write side is
  `demux::build_tref_box(&[(reference_type, track_IDs)])`, the
  byte-exact inverse of the parser.
- Track groups (ISO/IEC 14496-12 §8.3.4, `trgr`): each
  `TrackGroupTypeBox` child inside `trak/trgr` is parsed as a
  `(track_group_type, track_group_id)` pair — the child's FourCC
  names the grouping (`msrc` is the spec-named example, for
  multi-source presentations: tracks sharing a `track_group_id`
  under the `msrc` type originate from the same source, e.g. one
  participant's audio + video in a recorded video call) and the
  32-bit `track_group_id` identifies the group within the file.
  Two tracks carrying the same `(type, id)` pair belong to the
  same group (§8.3.4.3); track groups are **not** dependency
  relationships (use `tref` for those). Each child is surfaced on
  `params.options` as `trgr_<n>` (0-based encounter index) with
  value `"<type> <id>"`. The spec leaves the door open for two
  children of the same type on one track (unlike `tref` which
  caps at one per `reference_type`) so both encounter-order copies
  are preserved. Bytes trailing the 32-bit id inside a child are
  reserved for per-`track_group_type` extensions in derived specs
  and silently ignored at this layer; a child whose `version` field
  is non-zero (§8.3.4.2 pins it to 0) is also silently skipped, so
  unknown extensions never mis-parse. Absent `trgr`, none of the
  keys are emitted. The write side is
  `demux::build_trgr_box(&[(track_group_type, track_group_id)])`.
- Track Kind (ISO/IEC 14496-12 §8.10.4, `kind`): each `KindBox`
  inside a track-level `udta` is parsed as a `(schemeURI, value)`
  pair (both NULL-terminated C strings; an absent value is allowed,
  meaning the URI alone identifies the kind). Multiple `kind` boxes
  per track are supported — the spec explicitly allows several
  schemes co-labelling the same track (e.g. one DASH role
  `urn:mpeg:dash:role:2011 main` plus one iTunes-scheme tag). Each
  entry is surfaced on `params.options` as `kind_<n>` (0-based
  encounter index); the value is the URI alone when no name
  follows, or `"URI value"` (space-separated, mirroring the
  `tref_<type>` convention) when both are present. The write side is
  `demux::build_kind_box(scheme_uri, value)`.
- Track copyright (ISO/IEC 14496-12 §8.10.2, `cprt`): each
  `CopyrightBox` inside a track-level `udta` is parsed as a typed
  record carrying a 3-letter ISO 639-2/T language code and a decoded
  notice string. The 16-bit packed language word (§8.10.2.3 — bit 15
  pad + three 5-bit characters at `(ASCII - 0x60)`) is decoded back
  to lowercase ASCII; the notice C string is UTF-8 by default and
  UTF-16BE when it opens with the byte-order mark (0xFE 0xFF), with
  the trailing NUL terminator stripped. Multiple `cprt` boxes per
  track are supported — the spec's "Quantity: Zero or more" covers
  the multilingual case (one box per language). Each record is
  surfaced on `params.options` as the pair `copyright_<n>` (the
  notice; key omitted when the notice is empty) and `copyright_<n>_lang`
  (the 3-letter tag; emitted even with an empty notice so callers can
  tell "empty notice in `eng`" from "no `cprt`"). A box body shorter
  than the 6-byte FullBox + language word minimum or an unknown
  FullBox version (§8.10.2.2 pins it to 0) is silently dropped — the
  box is informational and a malformed entry never aborts the open.
  Distinct from the 3GPP TS 26.244 `cprt` shape lumped under the
  generic file-wide metadata channel, where the language code is
  not surfaced separately. The write side is
  `demux::build_cprt_box(language, notice)`.
- Track selection (ISO/IEC 14496-12 §8.10.3, `tsel`): the optional
  `TrackSelectionBox` inside a track-level `udta` carries two
  media-selection signals — `switch_group` (signed 32-bit, §8.10.3.4)
  groups tracks that are interchangeable *during* playback (e.g.
  bitrate-adaptive renditions of the same stream) within the
  alternate group declared on `tkhd` (§8.3.2), and `attribute_list`
  (§8.10.3.5) is a list of FourCC tags drawn from the descriptive
  set (`tesc` = temporal scalability, `fgsc` / `cgsc` = SNR
  scalability, `spsc` = spatial, `resc` = region-of-interest, `vwsc`
  = view) and differentiating set (`bitr` = bitrate, `cdec` = codec,
  `lang` = language, …) describing what the track offers. Surfaced
  on `params.options` as `tsel_switch_group` (the signed integer,
  emitted even when zero so callers can tell present-but-zero from
  absent) and `tsel_attributes` (space-separated FourCCs, mirroring
  the `tref_<type>` convention; omitted when the list is empty). A
  body shorter than the 8-byte FullBox + `switch_group` minimum or
  an unknown FullBox version is silently dropped — `tsel` is
  informational and a malformed entry never aborts the open.
  Container preserves the raw FourCCs; consumer mapping (e.g. a
  player selecting by language vs. bitrate within an alternate
  group) is delegated. Absent `tsel`, no keys are emitted. The write
  side is `demux::build_tsel_box(switch_group, attribute_list)`.
- Sub tracks (ISO/IEC 14496-12 §8.14, `strk` / `stri` / `strd` /
  `stsg`): a track-level `udta` may carry zero or more `strk` Sub
  Track boxes (§8.14.3), each assigning *part* of the track to the
  same alternate / switch groups that whole tracks use (§8.3.2 /
  §8.10.3) — the mechanism for selecting among layered-codec
  alternatives (SVC / MVC temporal, spatial, SNR, or view layers)
  that don't map onto track boundaries (§8.14.1). Each `strk`'s
  mandatory `stri` (Sub Track Information, §8.14.4) carries
  `switch_group` (signed 16-bit), `alternate_group` (signed 16-bit),
  `sub_track_ID` (32-bit), and an `attribute_list[]` of FourCCs from
  §8.14.4.3's descriptive (`tesc` / `fgsc` / `cgsc` / `spsc` /
  `resc` / `vwsc`) and differentiating (`bitr` / `frar` / `nvws` /
  …) vocabulary. The mandatory `strd` (Sub Track Definition,
  §8.14.5) is walked one level for its `stsg` (Sub Track Sample
  Group, §8.14.6) children — each names a `grouping_type` shared with
  the track's `sbgp` / `sgpd` (§8.9) plus the `sgpd` description
  indices that make up the sub track. Each sub track is surfaced on
  `params.options` as `subtrack_<n>` (0-based encounter index) with
  value `"id=<sub_track_ID> switch=<switch_group> alt=<alternate_group>
  [attrs=<fourcc...>] [stsg=<grouping_type>:<idx>,<idx>;...]"` — the
  three fixed fields are always present (0 is meaningful — it
  distinguishes present-but-unassigned from a missing field), the
  `attrs=` and `stsg=` blocks are omitted when empty, and FourCCs
  with non-printable bytes fall back to 8-digit hex (matching the
  `tsel_attributes` convention). A `strk` missing its mandatory
  `stri` contributes no sub track; an unknown FullBox version on
  `stri` / `stsg`, a too-short `stri`, or a `stsg` whose declared
  `item_count` overruns the body are rejected — the boxes are
  informational and a malformed entry never aborts the open. Absent
  `strk`, no keys are emitted. The write side is
  `demux::build_strk_box(switch_group, alternate_group, sub_track_id,
  attribute_list, sample_groups)` (with `build_stri_box` /
  `build_stsg_box` exposed for the individual children).
- Hint media header (ISO/IEC 14496-12 §12.4.2, `hmhd`): a hint
  track's `minf/hmhd` HintMediaHeaderBox carries protocol-independent
  streaming statistics for the packetised stream the hint track
  describes — `maxPDUsize` / `avgPDUsize` (byte sizes of the largest /
  average Protocol Data Unit) and `maxbitrate` / `avgbitrate`
  (bits/second over any one-second window / the whole presentation),
  with a trailing reserved 32-bit word read past but not surfaced.
  Used by a streaming server to size buffers / pace delivery without
  parsing every hint sample. The four fields are surfaced on
  `params.options` as `hmhd_max_pdu_size`, `hmhd_avg_pdu_size`,
  `hmhd_max_bitrate`, and `hmhd_avg_bitrate` (decimal). The FullBox
  `version` (pinned to 0 by §12.4.2.2) and a non-zero reserved word are
  tolerated rather than dropping a usable header, matching the `vmhd`
  posture; a body shorter than the full 20 bytes is rejected (a
  truncated tail would surface noise as a bitrate). Absent `hmhd`, none
  of the keys are emitted.
- QuickTime base media information header (`minf/gmhd/gmin`): QuickTime
  carries a Base Media Information Header Atom (`gmhd`) inside `minf` in
  place of a typed media header (`vmhd` / `smhd`) for media types derived
  from the base media handler — text, timecode, music, and generic
  tracks. Its required child, the Base Media Info Atom (`gmin`), is a
  FullBox whose fixed 12-byte body carries a 16-bit `graphicsmode`
  composition (transfer) mode, a three-component 16-bit `opcolor`, a
  signed 16-bit stereo sound `balance`, and a reserved 16-bit zero. The
  `gmhd` walk resolves `gmin` past any media-specific sibling atoms
  (`text` / `tmcd` / …). The three fields are surfaced on `params.options`
  as `gmin_graphicsmode` (decimal), `gmin_opcolor` (three space-separated
  decimal components), and `gmin_balance` (signed decimal). The FullBox
  `version` / `flags` (specified as 0) are tolerated rather than dropping
  a usable atom, matching the `vmhd` posture; a body shorter than the full
  16 bytes is rejected. Public byte-exact parse/build helpers
  `demux::parse_gmin_box` / `demux::build_gmin_box` (the `gmin` atom) and
  `demux::build_gmhd_box` (a minimal `gmhd` wrapping a single `gmin`) let
  tooling recover or assemble the atom without re-running `open()`. Absent
  `gmhd`, none of the keys are emitted.
- QuickTime timecode media info (`minf/gmhd/tcmi`): a timecode track
  (media type `tmcd`) carries a Timecode Media Information Atom (`tcmi`)
  inside its `gmhd`, governing how the on-screen timecode text is
  rendered. Its FullBox body carries a 16-bit `text_font` id, a
  `text_face` style bitmask (Bold `0x01` / Italic `0x02` / Underline
  `0x04` / Outline `0x08` / Shadow `0x10` / Condense `0x20` / Extend
  `0x40`), a 16-bit `text_size` point size, 48-bit (`[u16; 3]`) `text` and
  `background` RGB colours, and a Pascal-string `font_name`. The `gmhd`
  walk resolves both `gmin` and `tcmi` (each the first parseable instance
  wins). Surfaced on `params.options` as `tcmi_text_font` / `tcmi_text_face`
  / `tcmi_text_size` (decimal), `tcmi_text_color` / `tcmi_background_color`
  (three space-separated 16-bit components), and `tcmi_font_name` (omitted
  when empty). A body shorter than the 22-byte fixed prefix is rejected; a
  `font_name` length that overruns the body is clamped to the bytes
  present rather than dropping the atom. Public byte-exact
  `demux::parse_tcmi_box` / `demux::build_tcmi_box` round-trip the atom.
  Absent `tcmi`, none of the keys are emitted.
- QuickTime timecode sample description (`stsd` `tmcd` entry): a timecode
  track (media type `tmcd`) carries a single `tmcd` sample entry defining
  how its timecode samples are interpreted. After the shared 8-byte
  sample-entry preamble: a reserved u32, a 32-bit `flags` word (Drop-frame
  `0x01` / 24-hour-max `0x02` / Negative-OK `0x04` / Counter `0x08`), a
  32-bit `timescale`, a 32-bit `frame_duration`, and an 8-bit
  `number_of_frames`. Dispatched by FourCC (the `tmcd` handler maps to the
  Data media type). Surfaced on `params.options` as `tmcd_flags` (decimal),
  the decoded booleans `tmcd_drop_frame` / `tmcd_24hour_max` /
  `tmcd_negative_ok` / `tmcd_counter`, and `tmcd_timescale` /
  `tmcd_frame_duration` / `tmcd_number_of_frames` (decimal). The
  `TmcdSampleEntry` record's `drop_frame()` / `twenty_four_hour_max()` /
  `negative_times_ok()` / `counter()` helpers decode the flag word. Public
  byte-exact `demux::parse_tmcd_sample_entry_box` /
  `demux::build_tmcd_sample_entry` round-trip the entry. Absent a `tmcd`
  entry, none of the keys are emitted.
- QuickTime text sample description (`stsd` `text` entry): a QuickTime
  text track (media type `text`) carries a `text` sample entry defining
  how its text samples are drawn. After the shared 8-byte preamble: a
  32-bit `display_flags` word (Don't-auto-scale `0x02` / Use-movie-bg
  `0x08` / Scroll-in `0x20` / Scroll-out `0x40` / Horizontal-scroll
  `0x80` / Reverse-scroll `0x100` / Continuous-scroll `0x200` /
  Drop-shadow `0x1000` / Anti-alias `0x2000` / Key-text `0x4000`), a
  signed 32-bit `text_justification` (0 left / 1 centered / -1 right), a
  48-bit `background_color`, a 64-bit `default_text_box` (top/left/bottom/
  right), a `font_number`, `font_face` style, a 48-bit `foreground_color`,
  and a Pascal-string `text_name` (the font name). The structured fields
  are surfaced on `params.options` as `text_display_flags` /
  `text_justification` / `text_background_color` / `text_foreground_color`
  / `text_default_text_box` / `text_font_number` / `text_font_face` /
  `text_font_name` (font name omitted when empty), while the raw
  post-preamble bytes remain available as `params.extradata`. Public
  byte-exact `demux::parse_text_sample_entry_box` /
  `demux::build_text_sample_entry` round-trip the entry. Absent a
  structured `text` entry, none of the keys are emitted.
- QuickTime track load settings (`trak/load`): a QuickTime `load` atom
  indicates how a reader should preload and play the track. Its plain-Box
  16-byte body carries `preload_start_time`, `preload_duration` (`-1` =
  to end of track), `preload_flags` (preload-always `1` / preload-if-
  enabled `2`), and `default_hints` (double-buffer `0x0020` / high-quality
  `0x0100`), all in the movie timescale. The `LoadSettingsBox` record's
  `preload_always()` / `preload_if_enabled()` / `double_buffer()` /
  `high_quality()` helpers decode the flag / hint words. Surfaced on
  `params.options` as `load_preload_start_time` / `load_preload_duration`
  / `load_preload_flags` / `load_default_hints` (decimal) plus the decoded
  booleans `load_preload_always` / `load_preload_if_enabled` /
  `load_double_buffer` / `load_high_quality`. Public byte-exact
  `demux::parse_load_settings_box` / `demux::build_load_settings_box`
  round-trip the atom. Absent `load`, none of the keys are emitted.
- Hint sample entries (ISO/IEC 14496-12 §9.1.2 / §9.3.3.2 / §9.4): a
  hint track's `stsd` entry is decoded when it is an RTP server (`rtp `),
  SRTP (`srtp`), RTP reception (`rrtp`), or RTCP reception (`rtcp`)
  entry — a shared body (`maxpacketsize` + the `additionaldata` boxes
  `tims` timescale, `tsro` time offset, `snro` sequence offset, and for
  `srtp` the `srpp` SRTPProcessBox) surfaced as `rtp_hint_format`,
  `rtp_hint_max_packet_size`, `rtp_hint_timescale`,
  `rtp_hint_time_offset`, `rtp_hint_sequence_offset`, and
  `rtp_hint_srtp` — or an MPEG-2 TS server (`sm2t`) / reception (`rm2t`)
  entry, whose per-TS-packet wrapper byte counts and precomputed-only
  flag surface as `m2t_hint_format`, `m2t_hint_preceding_bytes`,
  `m2t_hint_trailing_bytes`, and `m2t_hint_precomputed`. The full typed
  records (with parse + build round-trip) live in the `hint` module.
- Hint statistics (ISO/IEC 14496-12 §9.1.5, `hinf`): a hint track's
  `udta/hinf` Hint Statistics Box is decoded into delivery totals — the
  `trpy` / `nump` / `tpyl` (and `totl` / `npck` / `tpay` u32 variants)
  byte/packet counts, `dmed` / `dimm` / `drep` media/immediate/repeated
  byte totals, `maxr` max-rate windows, `pmax` largest packet, and
  `payt` payload-ID rtpmap entries — surfaced as `hinf_bytes_sent`,
  `hinf_packets_sent`, `hinf_payload_bytes`, `hinf_media_bytes`,
  `hinf_immediate_bytes`, `hinf_repeated_bytes`, `hinf_largest_packet`,
  `hinf_maxr_count`, and `hinf_payload_count` (absent sub-boxes omit
  their keys). Full typed record + parse/build in the `hint` module.
- FD (File Delivery) item information (ISO/IEC 14496-12 §8.13): a
  file-level `meta`'s `fiin` FDItemInformationBox — the `paen` partition
  entries (`fpar` File Partition + optional `fecr` FEC Reservoir + `fire`
  File Reservoir boxes), plus optional `segr` FD session-group and `gitn`
  group-name boxes — is parsed into `MetaItems::fiin` and summarised as
  `meta_fiin_partitions` / `meta_fiin_session_groups` /
  `meta_fiin_group_names`. The full typed records (with parse + build
  round-trip, incl. the `feci` FEC Information Box, §9.2.4.7) live in the
  `fd` module.
- Additional metadata container (ISO/IEC 14496-12 §8.11.7 / §8.11.8,
  `meco` / `mere`): a file-level `meco` AdditionalMetadataContainerBox is
  parsed into a `MecoBox` carrying its additional `meta` boxes (each a
  fully-decoded `MetaItems` with a distinct handler type) plus the `mere`
  MetaboxRelationBoxes describing how same-level `meta` boxes relate,
  surfaced as `meco_meta_count` / `meco_meta_<n>` / `meco_relation_count`
  / `meco_relation_<n>`. Reachable via `Mp4Demuxer::meco()`.
- Composition-to-decode (ISO/IEC 14496-12 §8.6.1.4, `cslg`): a
  track's `stbl/cslg` CompositionToDecodeBox documents the
  composition↔decode timeline relationship implied by a signed
  (v1) `ctts` — the DTS shift that guarantees `CTS ≥ DTS`, the
  least / greatest composition offsets, and the composition
  start / end times. Both v0 (32-bit) and v1 (64-bit) layouts are
  read (widened to `i64`). The five fields are surfaced on
  `params.options` as `cslg_composition_to_dts_shift`,
  `cslg_least_decode_to_display_delta`,
  `cslg_greatest_decode_to_display_delta`,
  `cslg_composition_start_time`, and `cslg_composition_end_time`
  (decimal strings in the media timescale; `composition_end_time = 0`
  means "unknown" per §8.6.1.4.3). Absent `cslg`, none of the keys
  are emitted.
- Shadow sync samples (ISO/IEC 14496-12 §8.6.3, `stsh`): a track's
  `stbl/stsh` ShadowSyncSampleBox is an optional seek hint — a table
  of `(shadowed_sample_number, sync_sample_number)` pairs naming a
  sync sample (key frame) that can be decoded in place of a non-sync
  sample when seeking to or before it. Each pair is surfaced on
  `params.options` as `stsh_<n>` (0-based encounter index) with value
  `"shadowed sync"` (both 1-based sample numbers, space-separated). The
  table is purely a seek optimisation — it is ignored in normal
  forward play and a track decodes correctly without it. Absent
  `stsh`, none of the keys are emitted.
- Sample degradation priority (ISO/IEC 14496-12 §8.5.3, `stdp`): a
  track's `stbl/stdp` DegradationPriorityBox is an optional per-sample
  table of 16-bit `priority` values, one per sample (the
  `sample_count` is implicit from `stsz` / `stz2`, mirroring `sdtp`).
  The exact meaning and value range of `priority` are owned by
  derived specifications (§8.5.3.1 / §8.5.3.3 "Specifications derived
  from this define the exact meaning and acceptable range of the
  `priority` field"), so the container preserves the raw u16 without
  interpreting it. The demuxer surfaces a small summary on
  `params.options` rather than the per-sample table (which would
  dominate the options map for a typical track) — four keys per
  track-with-`stdp`: `stdp_count` (total entries), `stdp_min` /
  `stdp_max` (value spread), and `stdp_sum` (a u64 — a u16 priority ×
  2^32 samples fits comfortably; consumers compute the mean from
  `sum / count`). A renderer dropping samples under bitrate / CPU
  pressure consults the carrying spec for the priority ordering.
  Absent `stdp`, none of the keys are emitted.
- Sample padding bits (ISO/IEC 14496-12 §8.7.6, `padb`): a track's
  `stbl/padb` PaddingBitsBox is an optional table recording, per
  sample, how many bits at the tail of the sample's last byte are
  padding (a value in `0..=7`). On the wire two samples share a byte:
  each nibble is `bit(1) reserved=0; bit(3) pad`. Unlike `sdtp` /
  `stdp` the box declares its own `sample_count` (independent of
  `stsz` / `stz2`), so the table is self-contained — the trailing
  unused nibble when `sample_count` is odd is dropped during parse.
  The reserved high bit of each nibble (required zero by §8.7.6.2) is
  masked off so a producer slip on the reserved bit does not corrupt
  the surfaced pad count. The demuxer exposes a summary on
  `params.options` rather than the per-sample table — four keys per
  track-with-`padb`: `padb_count` (total entries), `padb_max` (largest
  pad count seen, `0..=7`), `padb_nonzero_count` (samples whose pad
  count is non-zero — the count that actually matters to a bitstream
  consumer; an all-zero `padb` means the track is byte-aligned), and
  `padb_hist` (eight-bucket histogram `n0:n1:n2:n3:n4:n5:n6:n7` where
  `nK` is the number of samples whose pad count is `K`, decimal). The
  histogram fully captures the value distribution in one short string;
  consumers can recover any aggregate without scanning. Absent `padb`,
  none of the keys are emitted.
- Sample dependency hints (ISO/IEC 14496-12 §8.6.4, `sdtp`): a
  track's `stbl/sdtp` SampleDependencyTypeBox is a per-sample table
  of four 2-bit fields — `is_leading`, `sample_depends_on`,
  `sample_is_depended_on`, `sample_has_redundancy` — packed one
  byte per sample (the `sample_count` is implicit from `stsz` /
  `stz2`). The table feeds trick-mode playback (drop disposable
  samples on fast-forward) and refines random-access roll-forward
  (a sample marked `sample_depends_on = 2` is an I-picture without
  needing the `stss` to mark it). The raw per-sample 2-bit values
  are decoded and stored on the track; the demuxer surfaces a small
  summary on `params.options` as five keys — `sdtp_count`,
  `sdtp_leading_count` (samples with `is_leading ∈ {1, 3}`),
  `sdtp_independent_count` (samples with `sample_depends_on = 2`),
  `sdtp_disposable_count` (samples with `sample_is_depended_on = 2`),
  and `sdtp_redundant_count` (samples with `sample_has_redundancy =
  1`). Absent `sdtp`, none of the keys are emitted (the demuxer
  falls back to `stss` for keyframe detection, as before).
- Multiple sample descriptions (ISO/IEC 14496-12 §8.5.2, `stsd`):
  every `SampleEntry` in the box is now parsed, not just the first.
  Entry `[0]` still drives active decode dispatch (its FourCC is the
  stream's resolved `codec`), and its codec-specific tail (audio /
  video preamble + child config boxes) is parsed as before. The
  additional entries — present when a track switches description
  mid-stream via a per-chunk `stsc.sample_description_index ≥ 2` or a
  fragment's `tfhd` / `trex` `sample_description_index` — are recorded
  with their FourCC and §8.5.2.2 `data_reference_index`. When
  `entry_count > 1` the demuxer surfaces them on `params.options`:
  `stsd_count` (total entries) plus `stsd_<n>` for each entry, where
  `<n>` is the **1-based** index matching the spec's
  `sample_description_index` (so a `stsc` / `tfhd` index value `k`
  looks up directly as `stsd_<k>`). Each value is
  `<fourcc> dref=<data_reference_index>`. A single-description track
  (the overwhelming common case) emits none of these keys — the active
  `codec` already carries that information. A declared `entry_count`
  larger than the bytes actually present stops at what could be read
  rather than failing the box or inventing entries.
- Data references (ISO/IEC 14496-12 §8.7.1–2, `dinf` / `dref` /
  `url ` / `urn `): a track's `minf/dinf/dref` DataReferenceBox is the
  table of media-data locations that every sample entry's 1-based
  `data_reference_index` (§8.5.2.2) indexes into — it declares whether a
  description's samples are self-contained in this file or stored in an
  external resource. Each `DataEntryBox` is a `url ` (with the
  §8.7.2.3 self-contained flag, or a NULL-terminated `location` URL when
  external) or a `urn ` (a NULL-terminated `name` plus an optional
  `location`); a non-`url `/`urn ` child is non-conforming and dropped.
  The overwhelmingly common single self-contained `url ` case is
  surfaced compactly on `params.options` as `dref_self_contained = "true"`
  with no per-entry keys (a one-lookup "no external resources" check).
  When the table has more than one entry, or any entry is *not*
  self-contained (a split-source track), the full table surfaces:
  `dref_count` (total entries), `dref_self_contained` ("true" only when
  *every* entry is self-contained), and `dref_<n>` (1-based, matching
  `data_reference_index`) = `<kind> self=<true|false>[ name=<urn>]
  [ loc=<url>]`. A forged `entry_count` cannot over-allocate (the
  smallest child is its 8-byte box header) and a child overrunning the
  body ends the walk at what was read; an unknown FullBox version is
  tolerated. A malformed `dref` is dropped without failing the `minf`
  parse (the file still demuxes against the single-source default).
  Absent / unparseable `dref`, none of the keys are emitted.
- Sample groups (ISO/IEC 14496-12 §8.9, `sbgp` + `sgpd`): a track's
  `stbl/sbgp` (SampleToGroupBox §8.9.2) run-length map and
  `stbl/sgpd` (SampleGroupDescriptionBox §8.9.3) per-group entries
  are parsed. Several of each are accumulated — one pair per
  `grouping_type` (`roll`, `rap `, `sync`, `alst`, `prol`, …). Each
  `sbgp` is surfaced on `params.options` as `sbgp_<n>` (0-based
  encounter index): the grouping type, an optional `param=<P>` (v1
  `grouping_type_parameter`), then space-separated `count:index`
  run-length pairs (`group_description_index` 0 = "no group of this
  type"; an index ≥ 0x10001 is a movie-fragment-local reference per
  §8.9.4, kept verbatim — the demuxer does not resolve fragment-local
  groups). Each `sgpd` is surfaced as `sgpd_<n>`: the grouping type,
  an optional `default=<D>` (v2 `default_sample_description_index`),
  then the per-group entry payloads as lowercase hex. Entry sizing
  honours §8.9.3.2 (v1 fixed `default_length`, v1 per-entry
  `description_length`, or the v0 deprecated no-length-signalling case
  captured as one combined blob). The entry payloads are
  grouping-type-specific and **not** interpreted by the container —
  they are surfaced verbatim for a layer that knows the
  `grouping_type` semantics. Absent both boxes, none of the keys are
  emitted.
- Typed §10 sample-group description entries (`sample_group_entries`
  module): a typed interpretation layer over the opaque `sgpd` entry
  blobs above, covering every `grouping_type` the **base** ISO/IEC
  14496-12 standard defines in §10 — `roll` / `prol` `RollRecoveryEntry`
  (signed `roll_distance`, §10.1.1), `rash` `RateShareEntry` (the
  single- and multi-operation-point wire shapes keyed by
  `operation_point_count`, §10.2.2), `alst` `AlternativeStartupEntry`
  (the variable `roll_count`-long `sample_offset[]` array plus the
  optional output-rate tail read "until the end of the structure",
  §10.3.2), `rap ` `VisualRandomAccessEntry`
  (`num_leading_samples_known` + 7-bit `num_leading_samples`, §10.4.2),
  `tele` `TemporalLevelEntry` (`level_independently_decodable`,
  §10.5.2), and `sap ` `SAPEntry` (`dependent_flag` + 4-bit `SAP_type`,
  §10.6.2). Each type has a `parse_*` decoder and a `build_*` builder
  that round-trip byte-exact; `decode_sample_group_entry(grouping_type,
  blob)` is the single typed dispatch point — it routes a
  `(grouping_type, blob)` pair (the same per-entry bytes the `sgpd_<n>`
  metadata renders as hex) to the matching parser and returns
  `Ok(Some(SampleGroupEntry))`, `Ok(None)` for a grouping type the base
  spec does not define (a codec-binding type like `sync`, or the CENC
  `seig` — handled by `cenc::parse_seig`), so a caller keeps the
  verbatim blob for those, and `Err(_)` only when a recognised §10 type
  carries a malformed payload. Bit-packed single-byte entries ignore
  reserved bits on read (§10.6.3 "allow and ignore reserved") and mask
  sub-byte fields to their declared widths on build; the parsers
  tolerate trailing bytes from a future edition except where the tail
  carries meaning (`alst`'s output-rate pieces, `rash`'s point list).
  This mirrors the `seig` precedent — typed grouping-type interpretation
  layered above the container boundary, leaving the demuxer's opaque-blob
  `sgpd_<n>` surface unchanged.
- Compact sample-to-group (ISO/IEC 14496-12:2020 §8.9.5, `csgp`): the
  compact alternative to `sbgp` is parsed. The `FullBox.flags` field is
  overloaded to carry three 2-bit width selectors (`index_size_code`,
  `count_size_code`, `pattern_size_code`, each mapped to a field width
  via `width = 4 << code` → 4/8/16/32 bits) plus a
  `grouping_type_parameter_present` bit (bit 6) and an
  `index_msb_indicates_fragment_local_description` bit (bit 7). The
  body's variable-width fields are bit-packed (no byte alignment between
  4-/8-bit fields) and read MSB-first. The §8.9.5 constraint that
  `pattern_size_code` and `count_size_code` must agree on whether the
  4-bit width (code 0) is used is enforced on both sides: a box that
  mixes 4-bit and non-4-bit between the two is rejected by the parser,
  and the builder promotes a 4-bit code off code 0 if the other would be
  wider so it never emits the invalid mix. The decoded body is a
  `pattern_count`-long array of
  `(pattern_length, sample_count)` followed by the per-pattern
  `sample_group_description_index` run. When bit 7 is set (legal only in
  a `traf`), the most-significant bit of each index — at the field's
  on-disk width — is a fragment-local-vs-global `sgpd` source selector
  rather than part of the index value; `CsgpBox` records the flag and the
  index field width (`index_field_bits`), and
  `CsgpPattern::resolve_index(n, flag, bits)` splits a stored index into
  a `CsgpResolvedIndex { fragment_local, value }` while the raw indices
  stay verbatim. Each `csgp` is accumulated and surfaced on
  `params.options` as `csgp_<n>`: the grouping type, an optional
  `param=<P>`, an optional `fraglocal` marker (bit 7 set), then one
  `count*idx0,idx1,…` token per pattern (`count` = `sample_count`, the
  index list = the pattern's per-sample indices; 0 = "no group",
  fragment-local high-bit kept verbatim). It shares `grouping_type` with
  the matching `sgpd_<m>`. Absent `csgp`, no keys are emitted. The
  structured record is reachable via the public
  `oxideav_mp4::demux::parse_csgp_box(&[u8])` entry point (typed
  `CsgpBox` / `CsgpPattern` / `CsgpResolvedIndex`) for tooling holding
  the box body; the write side is `sample_groups::build_csgp` (see Muxer
  below). `CsgpBox::resolve_samples(total_samples)` materialises the
  compact patterns into one resolved index *per sample* in decoding order
  — the same per-sample view `sbgp` already gives: each pattern's index
  run is cycled across its `sample_count` (the cycle may end mid-pattern),
  trailing samples not covered by any pattern take the `sgpd` default
  (surfaced as `value == 0`, the "no group" sentinel the caller swaps for
  the box's `default_group_description_index`), a `sample_count` sum that
  overruns the track is clamped to `total_samples`, and every emitted
  index is split through the bit-7 fragment-local-vs-global MSB
  convention. An empty-index pattern maps nothing rather than panicking.
- Sub-sample information (ISO/IEC 14496-12 §8.7.7, `subs`): a track's
  `stbl/subs` SubSampleInformationBox is an optional sparse table
  describing how selected samples decompose into smaller,
  semantically-meaningful sub-samples (e.g. NAL units / parameter sets
  for H.264 per ISO/IEC 14496-15, or arbitrary segment boundaries for
  codecs that define their own sub-sample contract). Each entry carries
  a sample-number delta from the previous entry, then a list of
  `(subsample_size, subsample_priority, discardable,
  codec_specific_parameters)` rows. Version 0 stores `subsample_size`
  as 16-bit; version 1 widens it to 32-bit (both layouts are read and
  normalised to `u32`). `flags` distinguishes co-resident `subs` boxes
  with different per-codec semantics (§8.7.7.1). The container
  preserves the carried codec's interpretation of
  `subsample_priority` / `discardable` / `codec_specific_parameters`
  verbatim — those small ints are opaque at this layer. Each `subs`
  encountered on a track is surfaced on `params.options` as
  `subs_<n>` (0-based encounter index); the value starts with
  `"v<version> flags=<f>"` and is followed by one space-separated
  `delta=<d>[:size,priority,discardable,csp[;...]]` block per entry
  (decimal for everything except `csp`, which is lowercase 8-digit
  hex). The trailing colon and per-sub-sample list are omitted when an
  entry has `subsample_count = 0`. Absent `subs`, no keys are emitted.
  The typed records (`demux::SubsBox` / `SubsEntry` / `SubSampleEntry`)
  are public, with a `demux::parse_subs_box` decoder and a byte-exact
  `demux::build_subs_box` builder.
- Sample auxiliary information sizes + offsets (ISO/IEC 14496-12
  §8.7.8–9, `saiz` + `saio`): a track's `stbl/saiz` + `stbl/saio` pair
  documents where per-sample auxiliary-information records live in the
  file, keyed by `(aux_info_type, aux_info_type_parameter)`. The most
  common consumer is CENC: when `senc` is absent, per-sample IVs +
  subsample maps (ISO/IEC 23001-7) are carried as an
  auxiliary-information stream of type `cenc` / `cbc1` / `cens` /
  `cbcs`, and the `saiz`+`saio` pair points to the bytes in the mdat.
  Both versions (v0 32-bit / v1 64-bit `saio` offsets) are read; the
  optional `aux_info_type` block (gated by `flags & 1`) is captured
  when present and surfaced as `None`/`None` otherwise so callers can
  apply §8.7.8.3's implied-value rule against the sample-entry FourCC
  / sinf scheme themselves. `saiz.default_sample_info_size` non-zero
  collapses the per-sample table; v1 `saio` offsets that exceed 32
  bits round-trip as `u64`. Each `saiz` / `saio` encountered on a
  track is surfaced on `params.options` as `saiz_<n>` / `saio_<n>`
  (0-based encounter index):
  - `saiz_<n> = "[type=<fourcc>] [param=<P>] default_size=<D> count=<N> [sizes=<s0>,<s1>,…]"`
  - `saio_<n> = "v<version> [type=<fourcc>] [param=<P>] offsets=<o0>,<o1>,…"`

  The `type=` / `param=` blocks are omitted when the FullBox `flags &
  1` bit was zero on disk; the `sizes=` block is omitted when
  `default_sample_info_size != 0`. Offsets are decimal; in `stbl` they
  are absolute file positions. For movie-fragment carriage (§8.8.14)
  the parser also surfaces a per-`traf` summary as `frag_sai_<n>`
  through `Demuxer::metadata()` (`"track=<t> seq=<s> saiz=<n>
  saio=<m>"`), and the structured per-fragment records (with offsets
  preserved as `tfhd.base_data_offset`-relative per §8.8.14) are
  reachable through the public `Mp4Demuxer::sai_records()` accessor on
  the demuxer (downcast). Absent `saiz` / `saio`, no keys are emitted
  and `sai_records()` is empty. The typed records (`demux::SaizBox` /
  `SaioBox`) are public, with `demux::parse_saiz_box` / `parse_saio_box`
  decoders and byte-exact `demux::build_saiz_box` / `build_saio_box`
  builders.
- Fragment-local sample groups (ISO/IEC 14496-12 §8.9.2 / §8.9.3 /
  §8.9.5 inside `traf`): a movie fragment may carry its own `sgpd`
  description table plus `sbgp` / `csgp` per-sample maps, so a
  fragment can declare **fragment-local** group descriptions and map
  its samples into either those or the `trak`-level (global) ones.
  The `csgp` bit-7 `index_msb_indicates_fragment_local_description`
  flag — whose whole purpose is this `traf` case (§8.9.5 makes it legal
  only here) — then selects the source per index. Each `traf`'s
  sample-group boxes are parsed verbatim (the same `parse_sbgp` /
  `parse_sgpd` / `parse_csgp` the `stbl`-level surface uses) and
  collected into a `TrafSampleGroupRecord` keyed by `(track_idx,
  moof_sequence)`. A per-`traf` summary is surfaced through
  `Demuxer::metadata()` as `frag_sample_group_<n>` (`"track=<t>
  seq=<s> sgpd=<n> sbgp=<m> csgp=<k>"`); the structured records (typed
  `SbgpBox` / `SgpdBox` / `CsgpBox`, on-wire values preserved — the
  most common use is CENC `seig` key rotation across a fragment's
  samples) are reachable through the public
  `Mp4Demuxer::traf_sample_groups()` accessor (downcast). The boxes do
  not disturb the `trun` sample walk. Absent fragment-local sample
  groups, no keys are emitted and `traf_sample_groups()` is empty.
- Producer reference time (ISO/IEC 14496-12 §8.16.5, `prft`): a
  top-level FullBox carrying a UTC wall-clock instant in NTP 64-bit
  format (RFC 5905 — high 32 bits = seconds since 1900-01-01 UTC,
  low 32 bits = fractional seconds) correlated with a media time on
  one reference track's media clock. Used by low-latency DASH / CMAF
  live streams so a consumer can match production wall-clock against
  media presentation time (and bound buffer occupancy without
  out-of-band timing signals). Each `prft` encountered during the
  top-level walk is surfaced on `Demuxer::metadata()` as `prft_<n>`
  (0-based file order); the value is three space-separated decimal
  integers `"reference_track_ID ntp_timestamp media_time"`. Both v0
  (32-bit `media_time`) and v1 (64-bit `media_time`) layouts are
  read; v1 `media_time` is widened to `u64` so callers see one type
  regardless. The 24-bit FullBox `flags` field — `0` in the 2015
  edition, named annotation bits in the 2022 edition (`encoder_input_output`
  0x1, `finalization_time` 0x2, `file_write_time` 0x4,
  `arbitrary_association` 0x8, the combined `realtime_offset` mask 0x18)
  describing what the NTP time represents — is captured verbatim and,
  when any bit is set, appended to the surfaced value as trailing
  space-separated name tokens after the three integers (the integer
  prefix stays backward-compatible). Absent `prft`, no keys are
  emitted. The structured record is also reachable via the public
  `oxideav_mp4::demux::parse_prft_box(&[u8])` entry point for tooling
  that wants the typed `PrftRecord` (`reference_track_id`,
  `ntp_timestamp`, `media_time`, `version`, `flags`) directly, with
  `is_encoder_input_output()` / `is_finalization_time()` /
  `is_file_write_time()` / `is_arbitrary_association()` /
  `is_realtime_offset()` accessors for the 2022 flag bits. A standalone
  `demux::build_prft_box(&PrftRecord)` builder (byte-exact inverse,
  v0/v1) complements the fragmented muxer's per-segment `prft` emission.
- Progressive download information (ISO/IEC 14496-12 §8.1.3, `pdin`): a
  top-level `FullBox(version = 0, flags = 0)` whose body is a sequence
  of `(rate, initial_delay)` u32 pairs (big-endian) — for each effective
  download `rate` (bytes/second) the box hints the suggested initial
  playback `initial_delay` (milliseconds) that lets the rest of the
  file arrive ahead of its playback deadline (§8.1.3.3). A receiver
  estimates its observed throughput, finds the bracketing pair, and
  linearly interpolates the delay (or extrapolates from the first /
  last entry when the observed rate sits outside the recorded range).
  §8.1.3.1 fixes quantity at zero or one per file and recommends it
  be placed as early as possible; the top-level walker captures the
  first instance and ignores any subsequent copies. The parsed table
  is surfaced on `Demuxer::metadata()` as `pdin_count` (total number of
  pairs, decimal) plus `pdin_<n>` (one per pair, `"rate initial_delay"`
  decimal space-separated, 0-based in file order); an explicitly empty
  box (preamble only, zero pairs — a producer signalling "no hints
  available") emits `pdin_count = 0` and no `pdin_<n>` keys. Absent
  `pdin`, none of the keys are emitted (the demuxer signals "no box"
  by omission rather than by a zero count). The structured record is
  also reachable via the public
  `oxideav_mp4::demux::parse_pdin_box(&[u8])` entry point for tooling
  that already has the box's payload bytes in hand (a DASH packager,
  a manifest emitter, a fixture validator), and via
  `Mp4Demuxer::pdin_entries()` (downcast) for callers holding the
  demuxer trait object. A body whose post-preamble length is not a
  multiple of 8 (one u32 pair per entry) is rejected outright per
  §8.1.3.2 — silently rounding to the nearest pair would mask a
  producer-side framing bug; a body shorter than the 4-byte FullBox
  preamble likewise fails the open. The version byte is tolerated even
  if the producer wrote a non-zero value (the spec pins it to 0 but
  the payload layout is unambiguous), matching `parse_padb` /
  `parse_stdp` posture for FullBox version-bit slips. The write side is
  `demux::build_pdin_box(&PdinRecord)`, the byte-exact inverse.
- Level assignment (ISO/IEC 14496-12 §8.8.13, `leva`): a `FullBox(0, 0)`
  inside `mvex` whose body opens with `unsigned int(8) level_count`
  and is followed by `level_count` per-level entries. Each entry maps
  one fragment level to a `track_id` (u32) with a `padding_flag` (top
  bit of the packed §8.8.13.2 byte) and a 7-bit `assignment_type`
  discriminator: 0 selects a sample grouping by `grouping_type` (u32),
  1 adds `grouping_type_parameter` (u32), 2/3 carry no further tail
  (level by track / by track-subsegment), 4 names a `sub_track_id`
  (u32) for sub-track scope. Levels specify subsets of subsequent
  movie fragments — samples mapped to level `n` may depend on any
  level `m ≤ n` but never on level `p > n` (§8.8.13.1). The §8.8.13
  level table cannot be specified for the initial movie; the box
  applies to all moof fragments that follow. In DASH (ISO/IEC 23009-1)
  each subsegment indexed by a `sidx` (§8.16.3) is a "fraction" and
  data for each level shall appear contiguously within it in
  increasing level order; `padding_flag = 1` declares that a
  conforming fraction can be formed by concatenating any positive
  integer number of levels and padding the last `mdat` to its
  declared size. §8.8.13.1 fixes quantity at zero or one per file;
  the `mvex` walker captures the first instance and ignores any
  subsequent copies. The parsed table is surfaced on
  `Demuxer::metadata()` as `leva_count` (total entry count, decimal)
  plus `leva_<n>` per-entry strings of the shape `"<track_id>
  pad=<0|1> at=<assignment_type> [grouping_type=<u32>]
  [grouping_type_parameter=<u32>] [sub_track_id=<u32>]"` — the
  trailing tokens are emitted only when their `assignment_type`
  variant uses them. The structured record is also reachable via the
  public `oxideav_mp4::demux::parse_leva_box(&[u8])` entry point for
  tooling that already has the box's payload bytes in hand (a DASH
  packager, a manifest emitter, a scalable-bitstream layer picker),
  and via `Mp4Demuxer::leva_entries() -> Option<&[LevaEntry]>`
  (downcast) for callers holding the demuxer trait object. A
  truncated entry header (less than 5 bytes for `track_id` +
  padding/assignment) or a truncated variant tail is rejected at
  parse time — a partial table would lie about which levels exist.
  A malformed `leva` reaching `parse_mvex` is dropped silently so a
  producer slip cannot brick `open()` (the box is informational —
  playback proceeds without level scoping). The §8.8.13.3 minimum
  `level_count ≥ 2` is **not** enforced (the demuxer carries
  whatever the producer wrote so a validator can flag the short
  table). Reserved `assignment_type` values (> 4) are surfaced as
  5-byte header entries with the variant-specific fields zero — the
  spec leaves their tail undefined so consuming any extra bytes would
  desynchronise the loop. The version byte is tolerated even if the
  producer wrote a non-zero value, matching `parse_pdin` / `parse_padb`
  / `parse_stdp` posture for FullBox version-bit slips.
- Track extension properties (ISO/IEC 14496-12 §8.8.15, `trep`): a
  `FullBox(0, 0)` inside `mvex` whose body opens with
  `unsigned int(32) track_id` and is followed by "any number of boxes"
  (§8.8.15.2) — e.g. an `assp` (Alternative Startup Sequence Properties
  Box, §8.8.16). The box documents or summarises characteristics of one
  track in the subsequent movie fragments. §8.8.15.1 fixes quantity at
  zero or more per `mvex` (zero or one per track); the `mvex` walker
  collects every instance in file order. The nested child boxes are
  recorded by type + payload length (`TrepChild`), and the one child the
  base spec defines here — `assp` (Alternative Startup Sequence
  Properties Box, §8.8.16) — is additionally decoded into a typed
  `AsspRecord` on `TrepChild::assp`; any other child stays opaque,
  leaving its semantics to a downstream consumer that recognises it. Each
  `trep` is surfaced on `Demuxer::metadata()` as `trep_<n>` (0-based file
  order) with value `"<track_id> children=<k>[ <fourcc>...]"` — the track
  ID, the child count, and each child's four-character type in order. A
  cleanly-decoded `assp` child appends its `min_initial_alt_startup`
  offset(s) after the `assp` token: `assp(off=<i>)` for v0, or
  `assp(<grouping_type_parameter>:<offset> ...)` for v1. The structured
  record is reachable via the public
  `oxideav_mp4::demux::parse_trep_box(&[u8])` entry point (for tooling
  holding the box's payload bytes) and via
  `Mp4Demuxer::treps() -> &[TrepRecord]` (downcast). A `trep` shorter
  than its 4-byte FullBox preamble or with a truncated `track_id` is
  rejected at parse time; a malformed `trep` reaching `parse_mvex` is
  dropped silently so a producer slip cannot brick `open()` (the box is
  informational). A child box whose declared size overruns the remaining
  `trep` body is clamped to the bytes available and ends the child walk;
  a trailing partial child header (fewer than 8 bytes) ends the walk
  cleanly. The 64-bit `largesize` child form (`size == 1`) is read via
  its 16-byte header. The version byte is tolerated even if non-zero,
  matching the `parse_leva` posture.
- Alternative startup sequence properties (ISO/IEC 14496-12 §8.8.16,
  `assp`): a `FullBox('assp', version, 0)` nested inside a `trep`
  (§8.8.15) that indicates the properties of the alternative startup
  sequence (`alst`, §10.3.2) sample groups in the subsequent track
  fragments of the `trep`'s track. §8.8.16.1 ties the box version to the
  `sbgp` version used for the `alst` grouping: version 0 (one implied
  entry — a signed `min_initial_alt_startup_offset`, no
  `grouping_type_parameter`) when the `alst` `sbgp` is v0; version 1 (a
  `num_entries`-long list of `(grouping_type_parameter,
  min_initial_alt_startup_offset)` pairs, one per alternative grouping)
  when it is v1. `min_initial_alt_startup_offset` is a lower bound on
  `sample_offset[1]` of the referred `alst` description entries: no value
  shall be smaller (§8.8.16.3). The demuxer decodes the `assp` child of
  any parsed `trep` into the typed `AsspRecord` (on `TrepChild::assp`)
  and appends the offset(s) to the flat `trep_<n>` metadata after the
  `assp` token (`assp(off=<i>)` for v0; `assp(<gtp>:<off> ...)` for v1).
  The structured record is also reachable via the public
  `oxideav_mp4::demux::parse_assp_box(&[u8])` entry point (typed
  `AsspRecord` / `AsspEntry`) for tooling holding the box body. The
  matching write side is `oxideav_mp4::demux::build_assp_box(&AsspRecord)`
  — the byte-exact inverse of `parse_assp_box`, emitting a complete
  `[size]['assp']` box for a muxer placing it inside a `trep`; it rejects
  records that would not round-trip (a v0 entry carrying a
  `grouping_type_parameter`, a v0 record without exactly one entry, or a
  version other than 0/1). Only versions 0 and 1 are defined; a truncated
  body, a v1 `num_entries` overrunning the body, or an unsupported
  version are rejected at parse time (a malformed `assp` then leaves
  `TrepChild::assp = None` while still contributing its type + length).
- Subsegment index (ISO/IEC 14496-12 §8.16.4, `ssix`): the top-level
  `FullBox(version = 0, flags = 0)` that maps levels (as assigned by
  the §8.8.13 `leva` box above) to byte ranges of the subsegments
  indexed by the immediately preceding `sidx` (§8.16.4.1 placement:
  zero or one per leaf-only `sidx`, as its next box;
  `subsegment_count` shall equal that sidx's `reference_count`), so a
  client can fetch a partial subsegment — e.g. only the lower temporal
  levels — by byte range. The §8.16.4.2 body (`subsegment_count` u32,
  then per subsegment a `range_count` u32 followed by packed
  `(level u8, range_size u24)` records that partition every byte of
  the subsegment, level-contiguous and in increasing level order for
  leaf subsegments) decodes into `SsixRecord` → `SsixSubsegment` →
  `SsixRange`. Every `ssix` met during the top-level walk is collected
  in file order and surfaced on `Demuxer::metadata()` as `ssix_<n>` =
  `"<subsegment_count> <total_range_count>"` (shape summary — the full
  table is reachable via the public
  `oxideav_mp4::demux::parse_ssix_box(&[u8])` entry point or the
  `Mp4Demuxer::ssixes()` accessor, downcast). Absent `ssix`, no keys
  are emitted; like `leva`, a malformed instance is dropped without
  failing `open()` (the box is informational — this demuxer seeks via
  `sidx` / `tfra` and never fetches partial subsegments). Only
  version 0 is defined and any other version byte is rejected rather
  than mis-read; both u32 counts are validated against the bytes
  actually present before any allocation. The §8.16.4.1 writer-side
  `range_count ≥ 2` minimum is **not** enforced on read. The matching
  `build_ssix_box(&SsixRecord)` builder emits a complete
  `[size]['ssix']` box for segment-index emitters pairing it with a
  `sidx`, rejecting `range_size` values above the 24-bit wire field's
  0xFF_FFFF ceiling instead of silently masking them.
- Ambient viewing environment (ISO/IEC 14496-12 post-2015 addition,
  `amve`): a track's `VisualSampleEntry` (`avc1` / `hvc1` / `av01` …)
  may carry an `AmbientViewingEnvironmentBox` — a plain `Box` (not a
  `FullBox`; no version/flags byte) with a fixed 8-byte body signalling
  the nominal ambient viewing environment for the display of the video.
  It is the file-format carriage of the same three syntax elements (with
  the same units and ranges) as the `ambient_viewing_environment` SEI
  message and the ISO/IEC 23091-3 (CICP) ambient-viewing-environment
  parameters: `ambient_illuminance` (0.0001 lux per unit),
  `ambient_light_x` / `ambient_light_y` (0.00002 CIE 1931 chromaticity
  per unit). The three raw integers are surfaced verbatim on
  `params.options` as `amve_ambient_illuminance`, `amve_ambient_light_x`,
  and `amve_ambient_light_y` (decimal) — a downstream HDR pipeline
  applies the unit scaling and can populate the matching SEI
  field-for-field with no conversion. The first `amve` met across a
  track's sample entries wins (one nominal environment per track); a
  body shorter than the fixed 8 bytes is dropped without aborting the
  open (the box is informational), and trailing bytes beyond byte 8
  (reserved for a future edition) are ignored. The structured record is
  reachable via the public `oxideav_mp4::demux::parse_amve_box(&[u8])`
  entry point (typed `AmveRecord` with `ambient_illuminance`,
  `ambient_light_x`, `ambient_light_y`) for tooling holding the box
  body. Absent `amve`, none of the keys are emitted. Distinct from and
  complementary to the mastering-display (`mdcv`) / content-light-level
  (`clli`) metadata, which describe the *content's* mastering
  environment rather than the *viewer's* ambient one.
- Stereo video (ISO/IEC 14496-12 §8.15.4.2, `stvi`): a video track that
  uses the restricted-scheme `stvi` SchemeType (stereoscopic video,
  §8.15.4.1) carries a `StereoVideoBox` inside its sample-entry
  `sinf/schi`, indicating that decoded frames hold either two spatially
  packed constituent frames forming a stereo pair (frame packing) or one
  of two views of a stereo pair (left / right in different tracks). The
  `FullBox(0, 0)` body is a packed `int(30) reserved` + `int(2)
  single_view_allowed`, then `int(32) stereo_scheme`, `int(32) length`,
  the `int(8)[length] stereo_indication_type` array, and optional
  trailing boxes. `single_view_allowed` signals which view(s) may be
  shown on a monoscopic single-view display (bit 0 = right view, bit 1 =
  left view); `stereo_scheme` selects the arrangement vocabulary (1 =
  the ISO/IEC 14496-10 frame-packing-arrangement SEI scheme, 2 = the
  ISO/IEC 13818-2 Annex L arrangement type, 3 = the ISO/IEC 23000-11
  scheme); `stereo_indication_type` is the scheme-specific arrangement
  code (preserved verbatim — its detailed meaning is owned by the named
  derived spec). The box is recovered from the `schi` of both encrypted
  (`enc*`) and non-encrypted (sample-entry-resident `sinf`) video
  entries; the first one encountered across a track's sample entries
  wins. Surfaced on `params.options` as `stvi_single_view_allowed` and
  `stvi_stereo_scheme` (decimal) plus `stvi_indication` (the raw
  `stereo_indication_type` bytes as lowercase hex, omitted when the
  indication is empty). The first 32-bit word's 30 reserved high bits
  are masked off so a producer slip on them does not corrupt
  `single_view_allowed`; a `length` that overruns the body is rejected
  rather than reading past the end, and trailing optional `any_box`
  bytes after the array are ignored. The structured record is reachable
  via the public `oxideav_mp4::demux::parse_stvi_box(&[u8])` entry point
  (typed `StviRecord` with `right_view_monoscopic_allowed()` /
  `left_view_monoscopic_allowed()` accessors) for tooling holding the
  box body. Absent `stvi`, none of the keys are emitted. The box version
  is tolerated even if non-zero (the layout is unambiguous), matching the
  `pdin` / `padb` posture. The matching write side is
  `oxideav_mp4::demux::build_stvi_box(&StviRecord)` — the byte-exact
  inverse of `parse_stvi_box`, emitting a complete `[size]['stvi']` box
  with the §8.15.4.2.2 `FullBox(0, 0)` body for a restricted-scheme
  (`stvi` SchemeType) muxer placing it inside a sample entry's
  `sinf/schi`.
- Bit rate (ISO/IEC 14496-12 §8.5.2, `btrt`): a `SampleEntry` (video /
  audio / metadata / text) may carry, optionally, a `BitRateBox` — a
  plain `Box` (no FullBox version/flags) with a fixed 12-byte body of
  three big-endian `u32`s: `bufferSizeDB` (size of the decoding buffer
  for the elementary stream, in bytes), `maxBitrate` (maximum rate in
  bits/second over any one-second window), and `avgBitrate` (average
  rate in bits/second over the whole presentation). It is the
  codec-agnostic carriage of the same three bandwidth quantities the
  MPEG-4 `esds` `DecoderConfigDescriptor` carries, usable by sample
  entries that have no `esds` (`avc1` / `hvc1` / `av01` video, `Opus` /
  `fLaC` audio, …). The three raw integers are surfaced verbatim on
  `params.options` as `btrt_buffer_size_db`, `btrt_max_bitrate`, and
  `btrt_avg_bitrate` (decimal) — an ABR packager validating a producer's
  declared rates, or a player sizing its receive buffer, reads them
  directly. The first `btrt` met on the active sample entry (`[0]`) wins
  (one per elementary stream); a body shorter than the fixed 12 bytes is
  dropped without aborting the open (the box is informational), and
  trailing bytes beyond byte 12 are ignored. The structured record is
  reachable via the public `oxideav_mp4::demux::parse_btrt_box(&[u8])`
  entry point (typed `BtrtRecord` with `buffer_size_db`, `max_bitrate`,
  `avg_bitrate`) for tooling holding the box body. Absent `btrt`, none
  of the keys are emitted.
- Picture geometry + colour (ISO/IEC 14496-12 §12.1.4–5, `pasp` / `clap`
  / `colr`): a `VisualSampleEntry` may carry a `PixelAspectRatioBox`
  (`pasp`, `hSpacing`/`vSpacing`), a `CleanApertureBox` (`clap`, the
  eight-u32 width/height/horiz-off/vert-off `N/D` fractions defining the
  active picture region), and one or more `ColourInformationBox`es
  (`colr`). For `colr` the three base-spec colour types are modelled:
  `nclx` (on-screen colours — `colour_primaries` / `transfer` /
  `matrix` 16-bit codes from ISO/IEC 23091-2 + `full_range_flag`),
  `rICC` / `prof` (restricted / unrestricted ICC profile, raw bytes
  preserved), and an `Other` fall-back keeping an unknown type +
  payload. Surfaced on `params.options` as `pasp_h_spacing` /
  `pasp_v_spacing`; `clap_width` / `clap_height` / `clap_horiz_off` /
  `clap_vert_off` (each `"<N>/<D>"`); and `colr_type` plus, for `nclx`,
  `colr_primaries` / `colr_transfer` / `colr_matrix` / `colr_full_range`
  (or `colr_icc_len` for an ICC profile). The first of each on the
  active entry wins (the spec lists `colr` boxes most-accurate-first).
  Public standalone parsers `demux::parse_pasp_box` / `parse_clap_box` /
  `parse_colr_box` decode each box body, and write counterparts
  `demux::build_pasp_box` / `build_clap_box` / `build_colr_box` emit
  each box byte-exact (the inverse of the parsers). A renderer reads the
  display aspect / crop rectangle and an HDR pipeline reads the colour
  signalling straight from the container. Absent the box, no keys are
  emitted.

### Muxer

Only codecs with an `mp4` sample-entry packaging are accepted. Codec
knowledge is confined to `sample_entries::sample_entry_for`; the rest
of the muxer appends opaque packet bytes.

Supported encode codec ids (produced sample entry FourCC in
parentheses):

- `pcm_s16le` → `sowt`
- `pcm_mulaw` / `pcm_alaw` → `ulaw` / `alaw` (plain 8-bit AudioSampleEntry)
- `flac` → `fLaC` with `dfLa` config (requires STREAMINFO extradata)
- `aac` → `mp4a` with `esds` (requires AudioSpecificConfig extradata)
- `mp3` → `mp4a` with `esds` `objectTypeIndication = 0x6B` (MPEG-1 audio;
  no DecoderSpecificInfo — the demuxer's OTI refinement resolves it back
  to `mp3`)
- `opus` → `Opus` with `dOps` (extradata is the demuxer's surfaced form —
  the dOps body behind an `OpusHead` magic; the magic is stripped on
  write and re-prepended on read, byte-exact both ways)
- `alac` → `alac` with `alac` magic-cookie config child (extradata is the
  cookie; the FullBox version/flags word is added on write / stripped on
  read)
- `ac3` → `ac-3` with `dac3` (extradata verbatim as the config-box body)
- `eac3` → `ec-3` with `dec3` (extradata verbatim)
- `h264` → `avc1` with `avcC` (requires AVCConfigurationRecord extradata)
- `h265` → `hvc1` with `hvcC` (requires HEVCDecoderConfigurationRecord)
- `av1` → `av01` with `av1C` (requires AV1CodecConfigurationRecord)
- `vp9` / `vp8` → `vp09` / `vp08` with `vpcC` (requires
  VPCodecConfigurationRecord, including its FullBox version/flags word —
  the demuxer's surfaced form)
- `h263` → `s263`, with a `d263` config child when extradata is present
  (opaque symmetric carriage; the demuxer surfaces the `d263` body)
- `mjpeg` → `jpeg`
- `mov_text` → `tx3g` (3GPP TS 26.245 timed text) — `text` handler + `nmhd`
- `webvtt` → `wvtt` (BMFF §12.6.3.2 XMLSubtitleSampleEntry sibling) — `subt` handler + `sthd`
- `ttml` → `stpp` (BMFF §12.6.3.2 XMLSubtitleSampleEntry) — `subt` + `sthd`
- `sbtt` → `sbtt` (BMFF §12.6.3.2 TextSubtitleSampleEntry) — `subt` + `sthd`
- `stxt` → `stxt` (BMFF §12.5.3.2 SimpleTextSampleEntry) — `subt` + `sthd`

For the subtitle codecs the muxer accepts the demuxer's surfaced
`extradata` verbatim (the post-preamble sample-entry payload: tx3g's
18-byte header, vttC config, stpp namespace strings, sbtt/stxt MIME
strings), so a demux → mux round-trip preserves the inner config.

Other codec ids fail with `Error::Unsupported` at `open`, never at
`write_packet` time.

Edit lists (`edts`/`elst`, ISO/IEC 14496-12 §8.6.5–6) are emitted
per-track when the first packet has a positive presentation timestamp:
a leading empty edit (`media_time = -1`) of the start delay (in the
movie timescale) followed by a `media_time = 0` segment for the track
duration, so a player offsets the track start instead of beginning at
presentation time 0. Version 0 (32-bit) by default, auto-promoting to
version 1 (64-bit) for over-32-bit durations. Tracks starting at PTS 0
get no `edts`. Controlled by `Mp4MuxerOptions::write_edit_list`
(default `true`).

Sample groups (`sbgp` / `sgpd` / `csgp`, ISO/IEC 14496-12 §8.9.2 /
§8.9.3 / §8.9.5) are emitted per-track when supplied via
`Mp4MuxerOptions::track_sample_groups` (a list of `TrackSampleGroups`,
each keyed by `stream_index`). Inside each track's `stbl`, after the
chunk-offset table, the muxer writes all `sgpd` description boxes first
(so the tables an index box references are declared ahead of it), then
`sbgp` run-length maps, then `csgp` compact maps. `sbgp` and `csgp` are
*alternative* encodings of the same per-sample → group mapping — §8.9.5
permits at most one form per `grouping_type`, so a caller picks one
form per `grouping_type` and never both, while distinct `grouping_type`s
on the same track may each pick their own form. The `csgp` width codes
(index / count / pattern) are chosen automatically as the narrowest
§8.9.5 widths that hold every value and packed into `FullBox.flags`
alongside the `grouping_type_parameter_present` and
`index_msb_indicates_fragment_local_description` bits; the
pattern-vs-count 4-bit-agreement constraint is satisfied by promoting a
lone 4-bit code off code 0 if its partner is wider. Each emitted box
round-trips byte-exact back through the demuxer's `sbgp_<n>` /
`sgpd_<n>` / `csgp_<n>` metadata surface. The stateless byte builders
(`sample_groups::build_sbgp` / `build_sgpd` / `build_csgp`) are public
for callers assembling boxes outside the muxer.

Chunk offsets auto-promote from `stco` (32-bit) to `co64` (64-bit) when
any offset exceeds 4 GiB. The `mdat` box header is 32-bit by default
(byte-identical to the historical output for sub-4-GiB files); set
`Mp4MuxerOptions::large_mdat = true` to reserve the ISO/IEC 14496-12
§4.2 extended-size header (`[size=1]["mdat"][largesize:u64]`) so the
media payload may exceed 4 GiB. Because the direct-write path streams
`mdat` to the output before its final size is known, the header form is
chosen up front from this flag; the faststart path (which buffers the
payload) additionally promotes automatically when the compact 32-bit
`size` would overflow. Without the flag, a direct-write `mdat` that
grows past 4 GiB fails at `write_trailer` with a message pointing at
`large_mdat`.

#### Fragmented / DASH / CMAF segment writing

The `dash`, `cmaf`, and `ismv` registry entries select the fragmented
muxer (`oxideav_mp4::frag::open_fragmented_typed`). It emits an init
segment (`ftyp` + `moov` with `mvex`/`trex` per track) followed by one
`styp? + sidx? + moof + mdat` segment per fragment cadence boundary, and
a trailing `mfra`/`tfra`/`mfro` random-access index. The per-segment
`styp` (ISO/IEC 14496-12 §8.16.2 Segment Type Box) is controlled by
`FragmentedOptions::styp`.

For caller-driven per-segment control, the
`FragmentedMuxer::write_fragmented_segment_with_styp(major_brand,
compat_brands)` inherent method marks the *next* emitted segment's
`styp` to use the given DASH/CMAF `(major, compat)` pair, overriding the
preset for one segment (then consumed). The stateless byte builder is
also exposed via the public `oxideav_mp4::styp` module —
`build_styp(major, compat)` / `write_styp(writer, major, compat)` —
mirroring the read-side `parse_styp` in oxideav-mov so a producer
round-tripping a parsed `Styp` can emit the same byte sequence.

A `prft` (ProducerReferenceTimeBox, ISO/IEC 14496-12 §8.16.5) can be
attached to the *next* fragment via
`FragmentedMuxer::set_next_segment_prft(reference_track_id,
ntp_timestamp, media_time, flags)` (or `..._prft_v1(..)` to force the
64-bit-`media_time` version-1 layout). The box is written immediately
before that fragment's `moof`, after any `sidx`/`styp` — the position
§8.16.5 requires since `prft` relates to the next `moof` in bitstream
order — and its size is folded into the enclosing `sidx`
`referenced_size` so a byte-range fetch still reaches the `moof`. The
request is consumed per fragment (one `prft` per `moof`). `ntp_timestamp`
is NTP 64-bit 32.32 fixed-point (high 32 bits = seconds since 1900-01-01,
low 32 bits = fractional seconds); `media_time` is the same instant on
the reference track's media clock (decode time). The box version is
auto-selected (v0 `u32` `media_time` when it fits, else v1 `u64`). This
is the write-side dual of the read-side `parse_prft_box` / `PrftRecord`.

A `leva` (LevelAssignmentBox, ISO/IEC 14496-12 §8.8.13) is emitted inside
the init-segment `mvex` (after the `trex` boxes) when
`FragmentedOptions::levels` is non-empty — one `LevaEntry` per declared
level, serialised through `demux::build_leva_box`. The level *order* in
that slice is the level *number* a sibling `ssix` refers to. The default
(empty) `levels` writes no `leva` and keeps the init segment
byte-identical to before. This is the write-side dual of the read-side
`parse_leva_box` / `LevaRecord`.

A `trep` (TrackExtensionPropertiesBox, ISO/IEC 14496-12 §8.8.15) is
emitted inside the init-segment `mvex` (after the `trex` boxes and after
any `leva`) for each record in `FragmentedOptions::treps` — one box per
record, in slice order, serialised through `demux::build_trep_box`. Each
record documents characteristics of one track in the subsequent
fragments; the one base-spec-defined child, `assp` (Alternative Startup
Sequence Properties Box, §8.8.16), is serialised from its typed
`AsspRecord` when present on a `TrepChild`. The default (empty) `treps`
writes no `trep` and keeps the init segment byte-identical to before.
This is the write-side dual of the read-side `parse_trep_box` /
`TrepRecord` (and `parse_assp_box` / `AsspRecord`).

An `ssix` (SubsegmentIndexBox, ISO/IEC 14496-12 §8.16.4) is emitted
immediately after each per-fragment `sidx` when `FragmentedOptions::
emit_ssix` is set (and `emit_random_access_indexes` is on, since the
`ssix` documents that `sidx`). Each emitted `ssix` carries
`subsegment_count == 1` — matching the one-reference `sidx` — and
partitions the fragment's single subsegment (`styp? + prft? + moof +
mdat`) into two level byte ranges that together cover every byte: the
leading `styp? + prft? + moof` metadata range and the trailing `mdat`
media range. The two level numbers are `FragmentedOptions::ssix_levels`
(default `(1, 2)`). The §8.16.4 `range_count ≥ 2` minimum and the
"ranges partition the subsegment" requirement hold by construction; a
range exceeding the 24-bit `range_size` field is rejected rather than
truncated. This is the write-side dual of the read-side `parse_ssix_box`
/ `SsixRecord`, completing `leva`/`ssix` partial-subsegment-fetch
read+write symmetry.

#### Sample-group muxing

Sample groups (`sbgp` / `sgpd`, ISO/IEC 14496-12 §8.9.2 / §8.9.3) are
emitted per track via `Mp4MuxerOptions::track_sample_groups`. Each
entry's `sbgp` and `sgpd` Vecs are placed at the end of the target
track's `stbl` body after the chunk-offset table; `sgpd` is written
before `sbgp` so the description table the per-sample index references
is declared first (§8.5.1 ordering). The
`oxideav_mp4::sample_groups::{SampleToGroup, SampleGroupDescription,
build_sbgp, build_sgpd}` API also stands alone for callers that want
to assemble the raw boxes themselves.

The version pick for `sgpd` is automatic per §8.9.3.2: a `Some(_)`
`default_sample_description_index` with shared-length entries → v2
(no per-entry length); shared-length entries alone → v1 with fixed
`default_length`; mixed-length entries → v1 with per-entry
`description_length`. The deprecated version-0 "no length signalling"
form is not emitted. The grouping-type-specific entry payload itself
is opaque to the container — callers supply already-serialised
`Vec<u8>` per entry.

`sbgp` chooses v0 (no `grouping_type_parameter`) or v1 (`Some(_)`) per
§8.9.2; a `group_description_index` ≥ `0x10001` (movie-fragment-local
per §8.9.4) is written verbatim, the muxer does not resolve it.

The compact form `csgp` (CompactSampleToGroupBox, ISO/IEC
14496-12:2020 §8.9.5) has a matching builder
`oxideav_mp4::sample_groups::build_csgp(&CompactSampleToGroup)`,
the byte-exact inverse of the demuxer's `parse_csgp` (round-tripped in
tests through the public `oxideav_mp4::demux::parse_csgp_box` entry
point). `CompactSampleToGroup` carries a `grouping_type`, an optional
`grouping_type_parameter`, and a list of `CompactSampleToGroupPattern`
(each a `sample_count` plus a per-pattern `indices` run). The three
bit-field width codes (`index_size_code` / `count_size_code` /
`pattern_size_code`) are chosen automatically as the narrowest §8.9.5
widths — `width = 4 << code`, i.e. 4/8/16/32 bits — that hold every
value present, and packed into the overloaded `FullBox.flags` field
together with the `grouping_type_parameter_present` bit. From
`pattern_count` onward the `(pattern_length, sample_count)` array and
the flattened index run are bit-packed MSB-first with no byte alignment
between fields (the trailing partial byte is zero-padded). A
`sample_group_description_index` ≥ `0x8000_0000` (the fragment-local
high bit, §8.9.4) is written verbatim; the builder does not synthesise
it.

### Seek strategy

`seek_to(stream, pts)` tries three strategies in order, picking the
first that applies:

1. **`tfra` fast-path (ISO/IEC 14496-12 §8.8.11).** If the file has
   a trailing `mfra` whose `tfra` indexes the requested track, the
   demuxer binary-searches the `tfra` time table for the largest
   `time ≤ pts`, translates the result to a `moof_offset`, and
   snaps to the first keyframe at-or-after that offset. O(log N) on
   `tfra` + a one-fragment-bounded sample-list scan.
2. **`sidx` fast-path (ISO/IEC 14496-12 §8.16.3).** If no `tfra`
   covers the track but the file carries one or more `sidx` boxes
   whose `reference_id` matches the track's `track_ID`, the demuxer
   walks every matching `sidx`, expands its references into virtual
   `(EPT, byte_offset)` anchors, and picks the latest anchor whose
   decode-time start is at-or-before `pts` (translated from the
   track's media timescale into the sidx timescale per §8.16.3).
   Both on-the-wire shapes are handled: a single `sidx` indexing
   every subsegment (DASH on-demand profile) and one `sidx` per
   subsegment (DASH live profile / what our own muxer emits).
   Hierarchical (nested) sidx references are walked for byte-range
   accounting only — they don't carry a media-time anchor we can
   land on. Timescale conversion uses `u128` arithmetic so the
   multiply doesn't overflow for long-duration tracks even when the
   track's media timescale and the sidx's timescale differ (per the
   spec-permitted but DASH-IF-deprecated case).
3. **Linear scan fallback.** Walks the sample table picking the
   last keyframe at-or-before `pts`. This is the unconditional
   safety net — when neither index applies, or when the indexed
   offset doesn't resolve cleanly (corrupt index, mdat layout the
   file lied about), `seek_to` still returns a correct cursor.

### Not (yet) supported

- In-pipeline CENC decryption — the AES-128 CTR / CBC driver
  exists (`cenc_cipher::decrypt_sample_in_place`, all four §10
  schemes), but the demuxer does not invoke it automatically:
  `read_packet` still yields ciphertext payloads and the caller —
  the party holding the content key for the sample's KID — calls
  the driver per sample. Key acquisition (DRM license exchange
  against the `pssh.SystemID` blob) is out of scope by design. The
  mdat-resident auxiliary-information bytes that the `saio`
  offsets name are not pre-fetched — a CENC consumer reading them
  seeks the input itself using the surfaced offsets.
- Per-sample *selection* of a non-active sample description. All
  `stsd` entries are now parsed and surfaced (see §8.5.2 above), but
  active decode always uses entry `[0]`: a per-chunk
  `stsc.sample_description_index ≥ 2` or a fragment's
  `tfhd.sample_description_index` override is recorded/surfaced but not
  yet honoured to re-dispatch the codec mid-stream.

## Container registry

```rust
let mut reg = oxideav_container::ContainerRegistry::new();
oxideav_mp4::register(&mut reg);
```

Registers:

- Demuxer `"mp4"` (also serving `.mp4`, `.m4a`, `.m4v`, `.3gp`, `.mov`,
  `.ismv`).
- Muxers `"mp4"`, `"mov"`, `"ismv"`.
- A content probe that recognises `ftyp` / `wide`+`ftyp` / `moov`.

## Fuzzing

A `cargo-fuzz` target exercises the BMFF box-tree walker on
arbitrary bytes:

```sh
cd fuzz
cargo +nightly fuzz run demux
```

The target opens, drains up to 256 packets, and re-seeks; it asserts
nothing panics, aborts, or OOMs. Seed corpus + regression artefacts
live at `fuzz/corpus/demux/`. The fuzz crate has its own `[workspace]`
and a committed `Cargo.lock` for reproducibility.

Pinned regressions worth calling out:

* **Extended-size u64 overflow** — a `size=1 largesize=u64::MAX`
  extended box anchored at a non-zero file offset used to overflow
  every downstream `body_start + payload_size` arithmetic site
  (the §8.16.3 `sidx` end-anchor computation is the most exposed
  example). `read_box_header` now `checked_add`s `start + total_size`
  and rejects the header before any caller computes a derived end
  byte. Replayed by `tests/largesize_overflow.rs` and two boundary
  unit tests in `src/boxes.rs`.

## License

MIT — see [LICENSE](LICENSE).
