# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
