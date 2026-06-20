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
  (spec-prohibited) are dropped.
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
  keys are emitted.
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
  `tref_<type>` convention) when both are present.
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
  not surfaced separately.
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
  group) is delegated. Absent `tsel`, no keys are emitted.
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
  `strk`, no keys are emitted.
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
  and `sai_records()` is empty.
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
  `is_realtime_offset()` accessors for the 2022 flag bits.
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
  `parse_stdp` posture for FullBox version-bit slips.
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
  recorded by type + payload length only (`TrepChild`) — this crate does
  not recurse into them, leaving their semantics to a downstream consumer
  that recognises the specific child. Each `trep` is surfaced on
  `Demuxer::metadata()` as `trep_<n>` (0-based file order) with value
  `"<track_id> children=<k>[ <fourcc>...]"` — the track ID, the child
  count, and each child's four-character type in order. The structured
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

### Muxer

Only codecs with an `mp4` sample-entry packaging are accepted. Codec
knowledge is confined to `sample_entries::sample_entry_for`; the rest
of the muxer appends opaque packet bytes.

Supported encode codec ids (produced sample entry FourCC in
parentheses):

- `pcm_s16le` → `sowt`
- `flac` → `fLaC` with `dfLa` config (requires STREAMINFO extradata)
- `aac` → `mp4a` with `esds` (requires AudioSpecificConfig extradata)
- `h264` → `avc1` with `avcC` (requires AVCConfigurationRecord extradata)
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
