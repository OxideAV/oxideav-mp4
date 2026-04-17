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
oxideav-core = "0.0"
oxideav-codec = "0.0"
oxideav-container = "0.0"
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

- Sample-entry FourCCs recognised: `mp4a`, `fLaC`/`flac`, `Opus`/`opus`,
  `alac`, `avc1`/`avc3`, `hvc1`/`hev1`, `vp08`, `vp09`, `av01`,
  `jpeg`/`mjpa`/`mjpb`, `mp4v`, `s263`/`h263`, `lpcm`/`sowt`/`twos`.
  Unknown FourCCs are surfaced as `CodecId("mp4:xxxx")` so callers can
  plug their own decoders.
- `mp4a` / `mp4v` disambiguation: when an `esds` box is present the
  MPEG-4 `objectTypeIndication` byte is consulted to pick the concrete
  codec id (e.g. `mp3`, `aac`, `mpeg1video`, `mpeg2video`,
  `mpeg4video`).
- Codec-specific config records (`avcC`, `hvcC`, `av1C`, `vpcC`,
  `dfLa`, `dOps`, esds DSI) are forwarded as `extradata`.
- Sample-table expansion: `stts`, `stsc`, `stsz`/`stz2`, `stco`/`co64`,
  `stss`. `next_packet` serves samples in file-offset order.
- Seek: `seek_to(stream, pts)` lands on the nearest sync-sample ≤ pts
  (or the first keyframe of the stream if none qualify).
- Metadata: 3GPP `udta` boxes (`titl`/`auth`/…) and iTunes-style
  `meta`/`ilst` are surfaced via `Demuxer::metadata()`.

### Muxer

Only codecs with an `mp4` sample-entry packaging are accepted. Codec
knowledge is confined to `sample_entries::sample_entry_for`; the rest
of the muxer appends opaque packet bytes.

Currently encoded at `write_header` time: `pcm_s16le` (`sowt`),
`flac` (`fLaC` + `dfLa`), `aac` (`mp4a` + `esds`), `h264` (`avc1` +
`avcC`), `mjpeg` (`jpeg`). Other codec ids fail with
`Error::Unsupported` at open, never at `write_packet` time.

Chunk offsets auto-promote from `stco` (32-bit) to `co64` (64-bit) when
any offset exceeds 4 GiB. The mdat box header stays 32-bit — files
whose mdat payload exceeds 4 GiB fail at `write_trailer`.

### Not (yet) supported

- Fragmented MP4 (moof / mfra / trun). The `frag_keyframe`,
  `empty_moov`, `delay_moov`, and `fragment_duration_ms` knobs exist on
  `Mp4MuxerOptions` but are no-ops — the muxer emits a single `moov`
  per file.
- Edit lists (`elst`) on demux or mux.
- Sample groups (`sbgp`/`sgpd`), subtitle tracks, DRM (`sinf`/`pssh`).
- Multiple sample descriptions per track (only the first entry of
  `stsd` is used).

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
