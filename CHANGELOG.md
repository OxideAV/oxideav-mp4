# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.5](https://github.com/OxideAV/oxideav-mp4/compare/v0.0.4...v0.0.5) - 2026-04-27

### Other

- parse ctts + edts/elst so packets carry composition timestamps
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

### Fixed

- **Composition timestamps for streams with B-frames** (ISO/IEC
  14496-12 §8.6.1.3 ctts + §8.6.6 edts/elst). The demuxer now
  parses the `ctts` (CompositionOffsetBox) entries and the
  `edts/elst` (EditListBox) leading `media_time` shift, so the
  `pts` it stamps onto each packet is the composition timestamp
  (CTS = DTS + ctts_offset − elst_media_time) instead of the
  raw decode timestamp. This is what downstream codec decoders
  pair with each decoded picture so display-order output ends
  up with monotonic pts. Pre-fix every B-frame run in an MP4
  fed `pts == dts` to the H.264 decoder, which then carried the
  wrong-frame pts through POC reordering and emitted apparent
  out-of-order timestamps to the player. Audio + intra-only
  video streams (which never carry a `ctts` box) take the
  same code path with `cts_offset = 0` — bytes-identical to the
  pre-fix output.
- Packet `dts` is now reported separately from `pts` and is
  shifted by `elst_media_time` to match the same presentation
  timeline.

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
