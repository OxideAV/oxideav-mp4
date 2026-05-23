# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
