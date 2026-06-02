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
  parsed structurally and surfaced to callers (no decryption — the
  AES key/decrypt op is left to a downstream layer with key
  material).
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
  regardless. Absent `prft`, no keys are emitted. The structured
  record is also reachable via the public
  `oxideav_mp4::demux::parse_prft_box(&[u8])` entry point for tooling
  that wants the typed `PrftRecord` (`reference_track_id`,
  `ntp_timestamp`, `media_time`, `version`) directly.

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
any offset exceeds 4 GiB. The mdat box header stays 32-bit — files
whose mdat payload exceeds 4 GiB fail at `write_trailer`.

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

- Fragmented-MP4 *muxing* — the demuxer reads `moof`+`mdat`
  segments, but the muxer only emits a single moov-at-end (or
  faststart) shape.
- CENC decryption proper — the demuxer **parses** the CENC framing
  (`tenc` defaults, `pssh` per-DRM headers, per-fragment `senc`
  per-sample IVs + subsample maps; see "CENC metadata parsing" in
  the demuxer feature list above; plus the spec-permitted
  alternative IV-carriage path via `saiz` / `saio` — see "Sample
  auxiliary information sizes + offsets") and surfaces the metadata,
  but it does not run the AES-128 CTR / CBC decryption step. That
  belongs to a downstream layer with key material from the named
  `pssh.SystemID`. The mdat-resident auxiliary-information bytes
  that the `saio` offsets name are not pre-fetched — a CENC
  consumer reading them seeks the input itself using the surfaced
  offsets.
- Multiple sample descriptions per track (only the first entry of
  `stsd` is used; `tfhd.sample_description_index` overrides are
  ignored).
- mdat payloads larger than 4 GiB (the 32-bit box header is not
  promoted to `largesize`).

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
  unit tests in `src/boxes.rs`. Companion to oxideav-mov's round 187
  fix on the QTFF atom walker.

## License

MIT — see [LICENSE](LICENSE).
