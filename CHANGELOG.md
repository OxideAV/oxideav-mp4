# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
