//! Build MP4 `stsd` sample-entry payloads for specific codecs.
//!
//! This is the *only* place in the muxer where codec knowledge is encoded.
//! All other muxer code is codec-agnostic — it just appends opaque packet bytes.
//!
//! Each `sample_entry_for` returns:
//! - `fourcc`: the 4-byte sample entry type (e.g. `b"sowt"`, `b"mp4a"`, `b"fLaC"`,
//!   `b"avc1"`).
//! - `body`: the contents of the sample entry box (i.e. everything after the
//!   8-byte box header). For audio entries this begins with the 28-byte
//!   `AudioSampleEntryV0` preamble; for video entries it begins with the
//!   78-byte `VisualSampleEntry` preamble. Codec-specific subboxes
//!   (`dfLa`, `esds`, `avcC`, …) follow.
//!
//! References: ISO/IEC 14496-12 §8.5, ISO/IEC 14496-14, ISO/IEC 23003-5
//! (FLAC-in-ISOBMFF).

use oxideav_core::{CodecParameters, Error, MediaType, Result};

/// A complete sample-entry description.
pub(crate) struct SampleEntry {
    /// Sample-entry FourCC (the box type that goes inside `stsd`).
    pub fourcc: [u8; 4],
    /// Payload of the sample-entry box (everything after the 8-byte box header).
    pub body: Vec<u8>,
}

/// Build the sample entry for a stream. Errors with `Unsupported` if the codec
/// has no MP4 packaging in our table.
pub(crate) fn sample_entry_for(params: &CodecParameters) -> Result<SampleEntry> {
    match params.codec_id.as_str() {
        "pcm_s16le" => pcm_sowt(params),
        "flac" => flac_entry(params),
        "aac" => aac_entry(params),
        "h264" => h264_entry(params),
        "mjpeg" => mjpeg_entry(params),
        // Video codecs whose sample entry is the shared VisualSampleEntry
        // preamble plus one mandatory configuration-record child box.
        // The extradata is the config-record bytes exactly as this
        // crate's demuxer surfaces them (the box *body*), so a
        // demux → mux round-trip re-emits the byte-identical box.
        "h265" => video_config_entry(params, *b"hvc1", *b"hvcC", "HEVCDecoderConfigurationRecord"),
        "av1" => video_config_entry(params, *b"av01", *b"av1C", "AV1CodecConfigurationRecord"),
        "vp9" => video_config_entry(params, *b"vp09", *b"vpcC", "VPCodecConfigurationRecord"),
        "vp8" => video_config_entry(params, *b"vp08", *b"vpcC", "VPCodecConfigurationRecord"),
        "h263" => h263_entry(params),
        // Audio codecs carried as an AudioSampleEntry preamble plus a
        // codec-specific config child (or none). Extradata conventions
        // mirror the demuxer's surfaced form (see each builder).
        "opus" => opus_entry(params),
        "alac" => alac_entry(params),
        "ac3" => dolby_entry(params, *b"ac-3", *b"dac3"),
        "eac3" => dolby_entry(params, *b"ec-3", *b"dec3"),
        "mp3" => mp3_entry(params),
        "pcm_mulaw" => g711_entry(params, *b"ulaw"),
        "pcm_alaw" => g711_entry(params, *b"alaw"),
        // Subtitle / timed-text packagings (ISO/IEC 14496-12 §12.5–6
        // + 3GPP TS 26.245 for tx3g/mov_text). All five accept the
        // demuxer's `extradata` (the post-preamble body) verbatim,
        // so a demux → mux round-trip preserves the inner config /
        // namespace / mime declarations.
        "mov_text" => subtitle_entry(params, *b"tx3g"),
        "webvtt" => subtitle_entry(params, *b"wvtt"),
        "ttml" => subtitle_entry(params, *b"stpp"),
        "sbtt" => subtitle_entry(params, *b"sbtt"),
        "stxt" => subtitle_entry(params, *b"stxt"),
        other => Err(Error::unsupported(format!(
            "mp4 muxer: no sample entry for codec {other}"
        ))),
    }
}

/// Pick the BMFF handler type four-char-code for a subtitle codec.
///
/// `tx3g` / `text` live under the QuickTime/BMFF `text` handler
/// (3GPP TS 26.245 + ISO/IEC 14496-12 §12.5.1). `wvtt` / `stpp` /
/// `sbtt` / `stxt` live under the BMFF `subt` handler (§12.6.1).
pub(crate) fn subtitle_handler_for(codec_id: &str) -> [u8; 4] {
    match codec_id {
        "mov_text" => *b"text",
        _ => *b"subt",
    }
}

/// Whether the subtitle codec's media-header box should be `sthd`
/// (BMFF §12.6.2 SubtitleMediaHeader, used by `subt` handler) rather
/// than `nmhd` (BMFF §12.5.2 null-media-header, used by `text` handler).
pub(crate) fn subtitle_uses_sthd(codec_id: &str) -> bool {
    !matches!(codec_id, "mov_text")
}

/// Build a subtitle sample entry. The 8-byte preamble (6 reserved +
/// `data_reference_index = 1`) is fixed; the body that follows comes
/// straight from `params.extradata`. For BMFF text/subtitle entries
/// the extradata is the post-preamble payload — see the demuxer's
/// `parse_subtitle_sample_entry` round-trip.
fn subtitle_entry(params: &CodecParameters, fourcc: [u8; 4]) -> Result<SampleEntry> {
    if params.media_type != MediaType::Subtitle {
        return Err(Error::invalid(format!(
            "mp4 muxer: subtitle codec {} must be Subtitle media",
            params.codec_id.as_str()
        )));
    }
    let mut body = Vec::with_capacity(8 + params.extradata.len());
    // 6 reserved bytes + 2-byte data_reference_index (= 1).
    body.extend_from_slice(&[0u8; 6]);
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&params.extradata);
    Ok(SampleEntry { fourcc, body })
}

/// Motion JPEG sample entry. Modern ISOBMFF uses the `jpeg` FourCC with a
/// plain VisualSampleEntry; each sample is a self-contained JPEG byte
/// stream. (The legacy QuickTime `mjpa`/`mjpb` forms have extra quirks
/// we don't emit today.)
fn mjpeg_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("mp4 muxer: mjpeg must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("mp4 muxer: mjpeg requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("mp4 muxer: mjpeg requires height"))?;
    let body = visual_preamble(width, height).to_vec();
    Ok(SampleEntry {
        fourcc: *b"jpeg",
        body,
    })
}

/// 28-byte AudioSampleEntryV0 preamble.
fn audio_preamble(channels: u16, sample_size: u16, sample_rate: u32) -> [u8; 28] {
    let mut out = [0u8; 28];
    // 6 bytes reserved
    // data_reference_index = 1
    out[6] = 0;
    out[7] = 1;
    // 8 bytes reserved (version/revision/vendor in QT-style, all zero in ISO)
    // channel_count at offset 16
    out[16..18].copy_from_slice(&channels.to_be_bytes());
    // sample_size at offset 18
    out[18..20].copy_from_slice(&sample_size.to_be_bytes());
    // 2 bytes pre_defined + 2 bytes reserved
    // sample_rate as 16.16 fixed-point at offset 24
    let sr_fixed = sample_rate << 16;
    out[24..28].copy_from_slice(&sr_fixed.to_be_bytes());
    out
}

/// 78-byte VisualSampleEntry preamble.
fn visual_preamble(width: u32, height: u32) -> [u8; 78] {
    let mut out = [0u8; 78];
    // 6 bytes reserved
    // data_reference_index = 1
    out[6] = 0;
    out[7] = 1;
    // 16 bytes pre_defined/reserved (offsets 8..24)
    // width at offset 24 (u16)
    let w = width as u16;
    let h = height as u16;
    out[24..26].copy_from_slice(&w.to_be_bytes());
    out[26..28].copy_from_slice(&h.to_be_bytes());
    // horizresolution 72 dpi as 16.16 at offset 28
    let dpi = 72u32 << 16;
    out[28..32].copy_from_slice(&dpi.to_be_bytes());
    // vertresolution 72 dpi as 16.16 at offset 32
    out[32..36].copy_from_slice(&dpi.to_be_bytes());
    // reserved u32 at offset 36
    // frame_count u16 = 1 at offset 40
    out[40..42].copy_from_slice(&1u16.to_be_bytes());
    // 32 bytes compressorname (length-prefixed Pascal string) at offset 42
    // depth u16 = 0x0018 at offset 74
    out[74..76].copy_from_slice(&0x0018u16.to_be_bytes());
    // pre_defined i16 = -1 at offset 76
    out[76..78].copy_from_slice(&(-1i16).to_be_bytes());
    out
}

fn pcm_sowt(params: &CodecParameters) -> Result<SampleEntry> {
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: PCM requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: PCM requires sample_rate"))?;
    // sowt is 16-bit signed little-endian PCM; hard-coded 16 bps.
    let body = audio_preamble(channels, 16, sample_rate).to_vec();
    Ok(SampleEntry {
        fourcc: *b"sowt",
        body,
    })
}

fn flac_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: flac must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: flac requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: flac requires sample_rate"))?;
    // Bits per sample: pick from sample_format; default to 16.
    let bps = params
        .sample_format
        .map(|f| (f.bytes_per_sample() * 8) as u16)
        .unwrap_or(16);
    let mut body = audio_preamble(channels, bps, sample_rate).to_vec();

    // dfLa subbox: FullBox (version 0 + 3 bytes flags) followed by the
    // FLAC metadata blocks. oxideav-flac extradata is already the concatenated
    // metadata blocks (each with 4-byte header + payload).
    if params.extradata.is_empty() {
        return Err(Error::invalid(
            "mp4 muxer: flac stream missing extradata (STREAMINFO)",
        ));
    }
    let mut dfla_body = Vec::with_capacity(4 + params.extradata.len());
    dfla_body.extend_from_slice(&[0, 0, 0, 0]); // version 0 + 3 bytes flags
    dfla_body.extend_from_slice(&params.extradata);
    body.extend_from_slice(&write_simple_box(b"dfLa", &dfla_body));

    Ok(SampleEntry {
        fourcc: *b"fLaC",
        body,
    })
}

fn aac_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: aac must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: aac requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: aac requires sample_rate"))?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(
            "mp4 muxer: aac stream missing extradata (AudioSpecificConfig)",
        ));
    }
    let mut body = audio_preamble(channels, 16, sample_rate).to_vec();

    // esds box (full box): ES_Descriptor wrapping DecoderConfigDescriptor
    // wrapping DecoderSpecificInfo (the AudioSpecificConfig).
    // ObjectTypeIndication = 0x40 (AAC). See ISO/IEC 14496-1 §7.2.6.
    body.extend_from_slice(&write_simple_box(
        b"esds",
        &build_esds_body(0x40, &params.extradata),
    ));

    Ok(SampleEntry {
        fourcc: *b"mp4a",
        body,
    })
}

fn h264_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("mp4 muxer: h264 must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("mp4 muxer: h264 requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("mp4 muxer: h264 requires height"))?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(
            "mp4 muxer: h264 stream missing extradata (AVCC configuration)",
        ));
    }
    let mut body = visual_preamble(width, height).to_vec();
    // avcC box: extradata assumed to already be AVCConfigurationRecord bytes.
    body.extend_from_slice(&write_simple_box(b"avcC", &params.extradata));
    Ok(SampleEntry {
        fourcc: *b"avc1",
        body,
    })
}

/// Generic video sample entry: 78-byte VisualSampleEntry preamble plus one
/// mandatory configuration-record child box whose body is the stream's
/// extradata verbatim (the same bytes this crate's demuxer surfaces when it
/// meets the box — `hvcC` / `av1C` / `vpcC`). ISO/IEC 14496-12 §12.1.3
/// (VisualSampleEntry) + ISO/IEC 14496-15 §8.4 (hvc1/hvcC); the AV1 and VP
/// ISOBMFF bindings follow the same shape with their own record.
fn video_config_entry(
    params: &CodecParameters,
    fourcc: [u8; 4],
    config_fourcc: [u8; 4],
    record_name: &str,
) -> Result<SampleEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid(format!(
            "mp4 muxer: {} must be video",
            params.codec_id.as_str()
        )));
    }
    let width = params.width.ok_or_else(|| {
        Error::invalid(format!(
            "mp4 muxer: {} requires width",
            params.codec_id.as_str()
        ))
    })?;
    let height = params.height.ok_or_else(|| {
        Error::invalid(format!(
            "mp4 muxer: {} requires height",
            params.codec_id.as_str()
        ))
    })?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(format!(
            "mp4 muxer: {} stream missing extradata ({record_name})",
            params.codec_id.as_str()
        )));
    }
    let mut body = visual_preamble(width, height).to_vec();
    body.extend_from_slice(&write_simple_box(&config_fourcc, &params.extradata));
    Ok(SampleEntry { fourcc, body })
}

/// H.263 sample entry (`s263`, the 3GPP MP4 packaging this crate's demuxer
/// already recognises). When the stream carries extradata it is emitted
/// verbatim as the `d263` configuration child's body — the opaque symmetric
/// carriage of whatever the demux side surfaced; when absent, a plain
/// VisualSampleEntry is emitted (matching the mjpeg posture).
fn h263_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Video {
        return Err(Error::invalid("mp4 muxer: h263 must be video"));
    }
    let width = params
        .width
        .ok_or_else(|| Error::invalid("mp4 muxer: h263 requires width"))?;
    let height = params
        .height
        .ok_or_else(|| Error::invalid("mp4 muxer: h263 requires height"))?;
    let mut body = visual_preamble(width, height).to_vec();
    if !params.extradata.is_empty() {
        body.extend_from_slice(&write_simple_box(b"d263", &params.extradata));
    }
    Ok(SampleEntry {
        fourcc: *b"s263",
        body,
    })
}

/// Opus sample entry (`Opus` + `dOps`). This crate's demuxer surfaces the
/// dOps body with an 8-byte `OpusHead` magic prepended so downstream code
/// treats Ogg- and MP4-sourced Opus uniformly; the write side strips that
/// magic back off when present and emits the remaining bytes verbatim as
/// the `dOps` body — the byte-exact inverse. Extradata without the magic is
/// taken to already be the dOps body.
fn opus_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: opus must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: opus requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: opus requires sample_rate"))?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(
            "mp4 muxer: opus stream missing extradata (OpusHead / dOps config)",
        ));
    }
    let dops_body = match params.extradata.strip_prefix(b"OpusHead") {
        Some(rest) => rest,
        None => &params.extradata[..],
    };
    if dops_body.len() < 11 {
        return Err(Error::invalid(
            "mp4 muxer: opus extradata too short for a dOps config",
        ));
    }
    let mut body = audio_preamble(channels, 16, sample_rate).to_vec();
    body.extend_from_slice(&write_simple_box(b"dOps", dops_body));
    Ok(SampleEntry {
        fourcc: *b"Opus",
        body,
    })
}

/// ALAC sample entry (`alac` entry + `alac` config child). The child is a
/// FullBox — 4 bytes version/flags then the ALAC magic-cookie bytes — and
/// the demux side surfaces the post-version/flags cookie as extradata
/// (mirroring the dfLa convention), so the write side re-wraps the cookie
/// under a zero version/flags word: the byte-exact inverse.
fn alac_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: alac must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: alac requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: alac requires sample_rate"))?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(
            "mp4 muxer: alac stream missing extradata (ALAC magic cookie)",
        ));
    }
    let bps = params
        .sample_format
        .map(|f| (f.bytes_per_sample() * 8) as u16)
        .unwrap_or(16);
    let mut body = audio_preamble(channels, bps, sample_rate).to_vec();
    let mut cfg = Vec::with_capacity(4 + params.extradata.len());
    cfg.extend_from_slice(&[0, 0, 0, 0]); // FullBox version 0 + flags 0
    cfg.extend_from_slice(&params.extradata);
    body.extend_from_slice(&write_simple_box(b"alac", &cfg));
    Ok(SampleEntry {
        fourcc: *b"alac",
        body,
    })
}

/// AC-3 / E-AC-3 sample entry (`ac-3` + `dac3`, `ec-3` + `dec3` — ETSI
/// TS 102 366 Annex F / G carriage as this crate's demuxer reads it). The
/// demux side keeps the raw config-box body as extradata, so the write side
/// emits it verbatim: the byte-exact inverse.
fn dolby_entry(
    params: &CodecParameters,
    fourcc: [u8; 4],
    config_fourcc: [u8; 4],
) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid(format!(
            "mp4 muxer: {} must be audio",
            params.codec_id.as_str()
        )));
    }
    let channels = params.channels.ok_or_else(|| {
        Error::invalid(format!(
            "mp4 muxer: {} requires channels",
            params.codec_id.as_str()
        ))
    })?;
    let sample_rate = params.sample_rate.ok_or_else(|| {
        Error::invalid(format!(
            "mp4 muxer: {} requires sample_rate",
            params.codec_id.as_str()
        ))
    })?;
    if params.extradata.is_empty() {
        return Err(Error::invalid(format!(
            "mp4 muxer: {} stream missing extradata ({} config)",
            params.codec_id.as_str(),
            String::from_utf8_lossy(&config_fourcc)
        )));
    }
    let mut body = audio_preamble(channels, 16, sample_rate).to_vec();
    body.extend_from_slice(&write_simple_box(&config_fourcc, &params.extradata));
    Ok(SampleEntry { fourcc, body })
}

/// MP3-in-MP4 sample entry (`mp4a` + `esds` with `objectTypeIndication =
/// 0x6B`, MPEG-1 audio — the OTI this crate's demuxer refines back to the
/// `mp3` codec id). MP3 needs no DecoderSpecificInfo: the frame headers are
/// self-describing, so the DecoderConfigDescriptor carries no DSI child.
fn mp3_entry(params: &CodecParameters) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: mp3 must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: mp3 requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: mp3 requires sample_rate"))?;
    let mut body = audio_preamble(channels, 16, sample_rate).to_vec();
    body.extend_from_slice(&write_simple_box(b"esds", &build_esds_body(0x6B, &[])));
    Ok(SampleEntry {
        fourcc: *b"mp4a",
        body,
    })
}

/// G.711 µ-law / A-law sample entry (`ulaw` / `alaw`): a plain 28-byte
/// AudioSampleEntry, 8-bit samples, no config child.
fn g711_entry(params: &CodecParameters, fourcc: [u8; 4]) -> Result<SampleEntry> {
    if params.media_type != MediaType::Audio {
        return Err(Error::invalid("mp4 muxer: G.711 must be audio"));
    }
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("mp4 muxer: G.711 requires channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: G.711 requires sample_rate"))?;
    let body = audio_preamble(channels, 8, sample_rate).to_vec();
    Ok(SampleEntry { fourcc, body })
}

/// Build an `esds` FullBox body: 4 bytes version/flags then the
/// ES_Descriptor wrapping a DecoderConfigDescriptor (with the given
/// `objectTypeIndication` and optional DecoderSpecificInfo bytes) and an
/// SLConfigDescriptor (predefined = 2). ISO/IEC 14496-1 §7.2.6; streamType
/// is always audio (0x05) here.
fn build_esds_body(oti: u8, dsi_bytes: &[u8]) -> Vec<u8> {
    // DecoderSpecificInfo (tag 0x05) — omitted entirely when empty.
    let mut dsi = Vec::new();
    if !dsi_bytes.is_empty() {
        dsi.push(0x05);
        append_ber_length(&mut dsi, dsi_bytes.len() as u32);
        dsi.extend_from_slice(dsi_bytes);
    }

    // DecoderConfigDescriptor (tag 0x04): 13 fixed bytes + DSI.
    let mut dcd = Vec::new();
    dcd.push(0x04);
    append_ber_length(&mut dcd, 13 + dsi.len() as u32);
    dcd.push(oti);
    dcd.push((0x05 << 2) | 0x01); // streamType audio | upstream=0 | reserved=1
    dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB (u24) = 0
    dcd.extend_from_slice(&[0, 0, 0, 0]); // maxBitrate = 0
    dcd.extend_from_slice(&[0, 0, 0, 0]); // avgBitrate = 0
    dcd.extend_from_slice(&dsi);

    // SLConfigDescriptor (tag 0x06): predefined = 2.
    let mut slc = Vec::new();
    slc.push(0x06);
    append_ber_length(&mut slc, 1);
    slc.push(0x02);

    // ES_Descriptor (tag 0x03).
    let mut esd = Vec::new();
    esd.push(0x03);
    append_ber_length(&mut esd, 3 + dcd.len() as u32 + slc.len() as u32);
    esd.extend_from_slice(&[0, 0, 0]); // ES_ID = 0, flags = 0
    esd.extend_from_slice(&dcd);
    esd.extend_from_slice(&slc);

    let mut esds_body = Vec::with_capacity(4 + esd.len());
    esds_body.extend_from_slice(&[0, 0, 0, 0]);
    esds_body.extend_from_slice(&esd);
    esds_body
}

/// Wrap an already-built sample entry into its §8.12 protected form
/// (ISO/IEC 14496-12 §8.12 ProtectionSchemeInfoBox envelope, as
/// profiled by ISO/IEC 23001-7 §4.1): the entry FourCC becomes the
/// media type's `enc*` transform (`encv` video / `enca` audio /
/// `enct` subtitle-text / `encs` everything else, matching the
/// demuxer's unwrap table) and a `sinf` box carrying
/// `frma(original_format)` + `schm(scheme_type, scheme_version)` +
/// `schi(tenc)` is appended to the entry body. The demuxer recovers
/// the original codec id from `frma` and surfaces
/// `protection_scheme` / `cenc_default_*` on `params.options`.
pub(crate) fn apply_protection(
    entry: SampleEntry,
    media_type: MediaType,
    protection: &crate::options::TrackProtection,
) -> Result<SampleEntry> {
    let enc_fourcc: [u8; 4] = match media_type {
        MediaType::Video => *b"encv",
        MediaType::Audio => *b"enca",
        MediaType::Subtitle => *b"enct",
        _ => *b"encs",
    };
    let sinf = crate::cenc::build_sinf_box(
        entry.fourcc,
        protection.scheme_type,
        protection.scheme_version,
        &protection.tenc,
    )?;
    let mut body = entry.body;
    body.extend_from_slice(&sinf);
    Ok(SampleEntry {
        fourcc: enc_fourcc,
        body,
    })
}

/// Write a simple (non-FullBox) box: 4-byte size + 4-byte fourcc + body.
fn write_simple_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = 8 + body.len() as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// Append a BER-style variable-length encoding (as used in MPEG-4 descriptors).
fn append_ber_length(out: &mut Vec<u8>, mut value: u32) {
    // Emit 4 bytes: high-7-bits first, continuation flag = 0x80. We always emit
    // 4 bytes so the length is stable and easy to parse.
    let mut bytes = [0u8; 4];
    for i in (0..4).rev() {
        bytes[i] = (value & 0x7F) as u8;
        value >>= 7;
    }
    for b in &mut bytes[..3] {
        *b |= 0x80;
    }
    out.extend_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{CodecId, CodecParameters, SampleFormat};

    #[test]
    fn pcm_sowt_shape() {
        let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        p.sample_format = Some(SampleFormat::S16);
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"sowt");
        assert_eq!(e.body.len(), 28);
        // channels big-endian at offset 16
        assert_eq!(u16::from_be_bytes([e.body[16], e.body[17]]), 2);
        // sample size at offset 18
        assert_eq!(u16::from_be_bytes([e.body[18], e.body[19]]), 16);
    }

    #[test]
    fn flac_entry_has_dfla() {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        p.sample_format = Some(SampleFormat::S16);
        // Minimal extradata: one STREAMINFO metadata block header+payload.
        let mut extradata = Vec::new();
        extradata.extend_from_slice(&[0x80, 0, 0, 34]); // last block, type=STREAMINFO, length=34
        extradata.extend_from_slice(&[0u8; 34]);
        p.extradata = extradata;
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"fLaC");
        // Body: 28 byte audio preamble + dfLa box (8 header + 4 version/flags + 38 metadata)
        assert_eq!(e.body.len(), 28 + 8 + 4 + 38);
        // Check the dfLa box is present at offset 28.
        assert_eq!(&e.body[32..36], b"dfLa");
    }

    #[test]
    fn unsupported_codec_errors() {
        let p = CodecParameters::audio(CodecId::new("vorbis"));
        assert!(sample_entry_for(&p).is_err());
    }

    fn video_params(codec: &str, extradata: &[u8]) -> CodecParameters {
        let mut p = CodecParameters::video(CodecId::new(codec));
        p.width = Some(640);
        p.height = Some(360);
        p.extradata = extradata.to_vec();
        p
    }

    fn audio_params(codec: &str, extradata: &[u8]) -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new(codec));
        p.channels = Some(2);
        p.sample_rate = Some(48_000);
        p.extradata = extradata.to_vec();
        p
    }

    #[test]
    fn video_config_entries_wrap_extradata() {
        // (codec id, expected entry fourcc, expected config-box fourcc)
        for (codec, fourcc, cfg) in [
            ("h265", b"hvc1", b"hvcC"),
            ("av1", b"av01", b"av1C"),
            ("vp9", b"vp09", b"vpcC"),
            ("vp8", b"vp08", b"vpcC"),
        ] {
            let record = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE];
            let e = sample_entry_for(&video_params(codec, &record)).unwrap();
            assert_eq!(&e.fourcc, fourcc, "{codec} entry fourcc");
            // 78-byte preamble + 8-byte box header + record.
            assert_eq!(e.body.len(), 78 + 8 + record.len(), "{codec} body size");
            assert_eq!(&e.body[82..86], cfg, "{codec} config fourcc");
            assert_eq!(&e.body[86..], &record, "{codec} config body verbatim");
            // Width/height in the preamble.
            assert_eq!(u16::from_be_bytes([e.body[24], e.body[25]]), 640);
            assert_eq!(u16::from_be_bytes([e.body[26], e.body[27]]), 360);
        }
    }

    #[test]
    fn video_config_entries_require_extradata() {
        for codec in ["h265", "av1", "vp9", "vp8"] {
            assert!(
                sample_entry_for(&video_params(codec, &[])).is_err(),
                "{codec} must require a config record"
            );
        }
    }

    #[test]
    fn h263_entry_with_and_without_d263() {
        // With extradata: a d263 child carrying the bytes verbatim.
        let d263 = [b'O', b'X', b'A', b'V', 0, 10, 0];
        let e = sample_entry_for(&video_params("h263", &d263)).unwrap();
        assert_eq!(&e.fourcc, b"s263");
        assert_eq!(e.body.len(), 78 + 8 + d263.len());
        assert_eq!(&e.body[82..86], b"d263");
        assert_eq!(&e.body[86..], &d263);
        // Without extradata: a plain VisualSampleEntry.
        let e = sample_entry_for(&video_params("h263", &[])).unwrap();
        assert_eq!(e.body.len(), 78);
    }

    #[test]
    fn opus_entry_strips_opushead_magic() {
        // 11-byte dOps-shaped payload behind the OpusHead magic.
        let dops = [1u8, 2, 0x01, 0x38, 0, 0, 0xBB, 0x80, 0, 0, 0];
        let mut extradata = b"OpusHead".to_vec();
        extradata.extend_from_slice(&dops);
        let e = sample_entry_for(&audio_params("opus", &extradata)).unwrap();
        assert_eq!(&e.fourcc, b"Opus");
        assert_eq!(e.body.len(), 28 + 8 + dops.len());
        assert_eq!(&e.body[32..36], b"dOps");
        assert_eq!(&e.body[36..], &dops, "dOps body must lose the magic");

        // Extradata already without the magic is taken verbatim.
        let e2 = sample_entry_for(&audio_params("opus", &dops)).unwrap();
        assert_eq!(&e2.body[36..], &dops);

        // Too-short config is rejected.
        assert!(sample_entry_for(&audio_params("opus", b"OpusHead\x01")).is_err());
    }

    #[test]
    fn alac_entry_wraps_cookie_in_fullbox() {
        let cookie = [0u8, 0, 16, 0, 40, 10, 14, 2];
        let e = sample_entry_for(&audio_params("alac", &cookie)).unwrap();
        assert_eq!(&e.fourcc, b"alac");
        // 28 preamble + 8 header + 4 version/flags + cookie.
        assert_eq!(e.body.len(), 28 + 8 + 4 + cookie.len());
        assert_eq!(&e.body[32..36], b"alac");
        assert_eq!(&e.body[36..40], &[0, 0, 0, 0], "FullBox version/flags");
        assert_eq!(&e.body[40..], &cookie);
        assert!(sample_entry_for(&audio_params("alac", &[])).is_err());
    }

    #[test]
    fn dolby_entries_carry_config_verbatim() {
        let dac3 = [0x10u8, 0x4C, 0x40];
        let e = sample_entry_for(&audio_params("ac3", &dac3)).unwrap();
        assert_eq!(&e.fourcc, b"ac-3");
        assert_eq!(&e.body[32..36], b"dac3");
        assert_eq!(&e.body[36..], &dac3);

        let dec3 = [0x07u8, 0xC0, 0x20, 0x00, 0x00];
        let e = sample_entry_for(&audio_params("eac3", &dec3)).unwrap();
        assert_eq!(&e.fourcc, b"ec-3");
        assert_eq!(&e.body[32..36], b"dec3");
        assert_eq!(&e.body[36..], &dec3);

        assert!(sample_entry_for(&audio_params("ac3", &[])).is_err());
        assert!(sample_entry_for(&audio_params("eac3", &[])).is_err());
    }

    #[test]
    fn mp3_entry_emits_esds_with_mpeg1_oti() {
        let e = sample_entry_for(&audio_params("mp3", &[])).unwrap();
        assert_eq!(&e.fourcc, b"mp4a");
        assert_eq!(&e.body[32..36], b"esds");
        // Deterministic layout: 28 preamble + 8 box header + 4 version/flags
        // + ES tag(1) + BER len(4) + ES_ID/flags(3) + DCD tag(1) + BER
        // len(4) → the objectTypeIndication byte.
        assert_eq!(e.body[28 + 8 + 4 + 13], 0x6B, "OTI must be MPEG-1 audio");
        // No DecoderSpecificInfo: the DCD payload is exactly 13 bytes (the
        // final BER length byte sits just before the OTI).
        assert_eq!(e.body[28 + 8 + 4 + 12], 13, "DCD BER length");
    }

    #[test]
    fn g711_entries_are_plain_8bit_audio() {
        for (codec, fourcc) in [("pcm_mulaw", b"ulaw"), ("pcm_alaw", b"alaw")] {
            let e = sample_entry_for(&audio_params(codec, &[])).unwrap();
            assert_eq!(&e.fourcc, fourcc);
            assert_eq!(e.body.len(), 28, "{codec} has no config child");
            assert_eq!(
                u16::from_be_bytes([e.body[18], e.body[19]]),
                8,
                "{codec} sample size"
            );
        }
    }

    #[test]
    fn new_entries_reject_wrong_media_type() {
        // Video codec ids presented as audio params must error, and
        // vice-versa for audio codec ids.
        let p = CodecParameters::audio(CodecId::new("h265"));
        assert!(sample_entry_for(&p).is_err());
        let p = CodecParameters::video(CodecId::new("opus"));
        assert!(sample_entry_for(&p).is_err());
    }

    #[test]
    fn mov_text_entry_shape() {
        let mut p = CodecParameters::subtitle(CodecId::new("mov_text"));
        // 18-byte tx3g default header (display flags + text colours +
        // default text box + default style record). Exact contents are
        // opaque to the muxer.
        let tx3g_header: [u8; 18] = [
            0x00, 0x00, 0x00, 0x00, // display_flags
            0x01, 0x00, 0x00, 0x00, // horiz_justify + vert_justify + bg colour rgba
            0x00, 0x00, 0x00, 0x00, // bg colour (cont.) + reserved
            0x00, 0x00, 0x00, 0x00, // default_text_box (top,left)
            0x00, 0x00, // default_text_box (bottom,right)
        ];
        p.extradata = tx3g_header.to_vec();
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"tx3g");
        // 6 reserved + 2 dri + 18 extradata = 26.
        assert_eq!(e.body.len(), 26);
        // data_reference_index at offset 6 is big-endian 1.
        assert_eq!(u16::from_be_bytes([e.body[6], e.body[7]]), 1);
        assert_eq!(&e.body[8..], &tx3g_header);
    }

    #[test]
    fn webvtt_entry_shape() {
        let mut p = CodecParameters::subtitle(CodecId::new("webvtt"));
        // Minimal `vttC` config box: 4-byte size + "vttC" + "WEBVTT".
        let mut vttc = Vec::new();
        vttc.extend_from_slice(&14u32.to_be_bytes());
        vttc.extend_from_slice(b"vttC");
        vttc.extend_from_slice(b"WEBVTT");
        p.extradata = vttc.clone();
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"wvtt");
        assert_eq!(e.body.len(), 8 + vttc.len());
        // The inner `vttC` box header should be present at offset 12 (8 preamble + 4 size).
        assert_eq!(&e.body[12..16], b"vttC");
    }

    #[test]
    fn ttml_entry_shape() {
        let mut p = CodecParameters::subtitle(CodecId::new("ttml"));
        // stpp body: namespace + \0 + schema_location? + \0 + auxiliary_mime_types? + \0.
        let strings = b"http://www.w3.org/ns/ttml\0\0\0";
        p.extradata = strings.to_vec();
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"stpp");
        assert_eq!(e.body.len(), 8 + strings.len());
        assert!(e.body[8..].starts_with(b"http://www.w3.org/ns/ttml"));
    }

    #[test]
    fn sbtt_entry_shape() {
        let mut p = CodecParameters::subtitle(CodecId::new("sbtt"));
        // sbtt body: content_encoding? + \0 + mime_format + \0.
        let strings = b"\0text/plain\0";
        p.extradata = strings.to_vec();
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"sbtt");
        assert_eq!(e.body.len(), 8 + strings.len());
    }

    #[test]
    fn stxt_entry_shape() {
        let mut p = CodecParameters::subtitle(CodecId::new("stxt"));
        let strings = b"\0text/html\0";
        p.extradata = strings.to_vec();
        let e = sample_entry_for(&p).unwrap();
        assert_eq!(&e.fourcc, b"stxt");
        assert_eq!(e.body.len(), 8 + strings.len());
    }

    #[test]
    fn subtitle_handler_routing() {
        // mov_text under BMFF text handler.
        assert_eq!(&subtitle_handler_for("mov_text"), b"text");
        // wvtt/stpp/sbtt/stxt under the subtitle (subt) handler.
        for c in ["webvtt", "ttml", "sbtt", "stxt"] {
            assert_eq!(&subtitle_handler_for(c), b"subt");
        }
    }

    #[test]
    fn subtitle_header_routing() {
        assert!(!subtitle_uses_sthd("mov_text"));
        for c in ["webvtt", "ttml", "sbtt", "stxt"] {
            assert!(subtitle_uses_sthd(c));
        }
    }

    #[test]
    fn subtitle_rejects_wrong_media_type() {
        // codec_id says mov_text but the params claim Audio — must error.
        let p = CodecParameters::audio(CodecId::new("mov_text"));
        assert!(sample_entry_for(&p).is_err());
    }
}
