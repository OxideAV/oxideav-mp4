# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
