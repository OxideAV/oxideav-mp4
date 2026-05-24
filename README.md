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
  callers know packet payloads are still ciphertext. CENC key
  management (`tenc`, `pssh`, `senc`, `saiz` / `saio`) is **not**
  implemented — the demuxer surfaces packets verbatim and leaves
  decryption to a layer with key material.
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

### Not (yet) supported

- Fragmented-MP4 *muxing* — the demuxer reads `moof`+`mdat`
  segments, but the muxer only emits a single moov-at-end (or
  faststart) shape.
- `mfra`/`tfra` random-access index consumption (the demuxer skips
  these; sequential demux works without them).
- `sidx` segment-index seek-time mapping (skipped; sequential demux
  works without it).
- Sample-group *muxing* — the demuxer reads `sbgp` / `sgpd` and
  surfaces them on `params.options`, but the muxer does not emit
  sample-group boxes.
- CENC decryption proper — the demuxer detects protected tracks
  (`encv` / `enca` / `enct` / `encs` → original FourCC plus the
  scheme on `params.options`) but it doesn't read `tenc`, `pssh`,
  `senc`, or `saiz` / `saio` and it doesn't decrypt sample bytes.
  Adding that needs the base ISO/IEC 23001-7 spec (currently only
  AMD1 / 2019 is in `docs/container/cenc/`).
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

## License

MIT — see [LICENSE](LICENSE).
