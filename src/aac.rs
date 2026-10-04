//! AAC carriage normalisation for the MP4 muxers (ISO/IEC 14496-14
//! §3.1.2 / §5.6, ISO/IEC 14496-3 §1.A.2).
//!
//! An MP4 `mp4a` sample is exactly one *bare* AAC access unit; the
//! decoder configuration rides out-of-band in the `esds`
//! DecoderSpecificInfo (the §1.6.2.1 `AudioSpecificConfig`). Encoders
//! and ADTS demuxers in the framework hand over ADTS frames instead —
//! the §1.A.2 transport header in front of every access unit — so the
//! muxer:
//!
//! * strips the 7-byte (or, with `protection_absent == 0`, 9-byte)
//!   ADTS header from every sample ([`strip_adts`]);
//! * synthesises the `AudioSpecificConfig` when the stream carries no
//!   extradata — from the first ADTS header when one is available
//!   ([`asc_from_adts`]), else AAC-LC from the stream's sample rate and
//!   channel count ([`lc_asc_from_params`]).

use std::borrow::Cow;

use oxideav_core::{CodecParameters, Error, Result};

/// ISO/IEC 14496-3 Table 1.18 sampling frequencies, indexed by
/// `samplingFrequencyIndex` (0..=12).
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The fixed-header fields of an ADTS frame that matter for carriage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdtsInfo {
    /// `profile_ObjectType` (= `audioObjectType - 1`).
    pub profile: u8,
    pub sampling_frequency_index: u8,
    pub channel_configuration: u8,
    /// Header length: 7, or 9 with the CRC.
    pub header_len: usize,
    /// `aac_frame_length` (header included).
    pub frame_len: usize,
    /// `number_of_raw_data_blocks_in_frame + 1`.
    pub raw_blocks: u8,
}

/// Parse an ADTS header (§1.A.2.2.1 / §1.A.2.2.2) at the start of
/// `data`. Returns `None` unless the 12-bit syncword, the zero `layer`
/// field, a defined sampling index and an `aac_frame_length` equal to
/// `data.len()` all check out — a bare access unit that merely begins
/// with `0xFFF` bits is not mistaken for a header.
pub(crate) fn parse_adts(data: &[u8]) -> Option<AdtsInfo> {
    if data.len() < 7 || data[0] != 0xFF || data[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = data[1] & 0x01 != 0;
    let profile = data[2] >> 6;
    let sampling_frequency_index = (data[2] >> 2) & 0x0F;
    if sampling_frequency_index > 12 {
        return None;
    }
    let channel_configuration = ((data[2] & 0x01) << 2) | (data[3] >> 6);
    let frame_len = (usize::from(data[3] & 0x03) << 11)
        | (usize::from(data[4]) << 3)
        | usize::from(data[5] >> 5);
    let raw_blocks = (data[6] & 0x03) + 1;
    let header_len = if protection_absent { 7 } else { 9 };
    if frame_len != data.len() || frame_len < header_len {
        return None;
    }
    Some(AdtsInfo {
        profile,
        sampling_frequency_index,
        channel_configuration,
        header_len,
        frame_len,
        raw_blocks,
    })
}

/// Return the bare access unit inside `data`: the payload after the
/// ADTS header when `data` is one complete single-block ADTS frame,
/// otherwise `data` unchanged (already a bare access unit).
///
/// A multi-block ADTS frame (`number_of_raw_data_blocks_in_frame > 0`)
/// holds several access units with no in-band boundaries (§1.A.2.2.3
/// only locates them when the CRC is present) and is rejected — an MP4
/// sample must be exactly one access unit.
pub(crate) fn strip_adts(data: &[u8]) -> Result<Cow<'_, [u8]>> {
    match parse_adts(data) {
        Some(info) if info.raw_blocks > 1 => Err(Error::unsupported(
            "mp4 muxer: multi-block ADTS frames cannot be split into MP4 samples",
        )),
        Some(info) => Ok(Cow::Borrowed(&data[info.header_len..info.frame_len])),
        None => Ok(Cow::Borrowed(data)),
    }
}

/// The two-byte §1.6.2.1 `AudioSpecificConfig` equivalent to an ADTS
/// fixed header: `audioObjectType = profile + 1`, the same sampling
/// index and channel configuration, and a default `GASpecificConfig`
/// (1024-line frames, no core coder, no extension).
pub(crate) fn asc_from_adts(info: &AdtsInfo) -> Vec<u8> {
    asc_bytes(
        info.profile + 1,
        info.sampling_frequency_index,
        info.channel_configuration,
    )
}

/// An AAC-LC `AudioSpecificConfig` for `params.sample_rate` /
/// `params.channels` (Table 1.19 default layouts: 1–6 channels, 8 → 7).
pub(crate) fn lc_asc_from_params(params: &CodecParameters) -> Result<Vec<u8>> {
    let rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("mp4 muxer: aac requires sample_rate"))?;
    let idx = SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .ok_or_else(|| {
            Error::invalid(format!(
                "mp4 muxer: aac stream missing extradata (AudioSpecificConfig) and \
                 {rate} Hz has no samplingFrequencyIndex"
            ))
        })? as u8;
    let cfg = match params.channels {
        Some(c @ 1..=6) => c as u8,
        Some(8) => 7,
        other => {
            return Err(Error::invalid(format!(
                "mp4 muxer: aac stream missing extradata (AudioSpecificConfig) and \
                 {other:?} channels have no default channelConfiguration"
            )))
        }
    };
    Ok(asc_bytes(2, idx, cfg))
}

fn asc_bytes(aot: u8, sfi: u8, cfg: u8) -> Vec<u8> {
    // audioObjectType(5) samplingFrequencyIndex(4) channelConfiguration(4)
    // frameLengthFlag(1)=0 dependsOnCoreCoder(1)=0 extensionFlag(1)=0
    let bits: u16 =
        (u16::from(aot & 0x1F) << 11) | (u16::from(sfi & 0x0F) << 7) | (u16::from(cfg & 0x0F) << 3);
    bits.to_be_bytes().to_vec()
}

/// `params` with an `AudioSpecificConfig` guaranteed in `extradata`
/// for an AAC stream (synthesised from the stream geometry when
/// absent). Returns `None` when nothing needed to change.
pub(crate) fn with_asc(params: &CodecParameters) -> Result<Option<CodecParameters>> {
    if params.codec_id.as_str() != "aac" || !params.extradata.is_empty() {
        return Ok(None);
    }
    let mut p = params.clone();
    p.extradata = lc_asc_from_params(params)?;
    Ok(Some(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts(payload_len: usize, crc: bool, blocks: u8) -> Vec<u8> {
        let hl = if crc { 9 } else { 7 };
        let fl = hl + payload_len;
        let mut v = vec![
            0xFF,
            if crc { 0xF0 } else { 0xF1 },
            (1 << 6) | (4 << 2), // LC, 44.1 kHz, cfg high bit 0
            (2 << 6) | ((fl >> 11) as u8 & 0x03),
            (fl >> 3) as u8,
            ((fl as u8 & 0x07) << 5) | 0x1F,
            0xFC | (blocks - 1),
        ];
        v.resize(hl, 0);
        v.extend(vec![0xABu8; payload_len]);
        v
    }

    #[test]
    fn strips_header_and_crc() {
        let f = adts(10, false, 1);
        assert_eq!(&*strip_adts(&f).unwrap(), &[0xAB; 10][..]);
        let f = adts(10, true, 1);
        assert_eq!(&*strip_adts(&f).unwrap(), &[0xAB; 10][..]);
    }

    #[test]
    fn bare_access_unit_passes_through() {
        // Starts with 0xFFF bits but its "frame length" does not match.
        let au = [0xFF, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC, 0x00];
        assert_eq!(&*strip_adts(&au).unwrap(), &au[..]);
        let au = [0x21, 0x10, 0x05];
        assert_eq!(&*strip_adts(&au).unwrap(), &au[..]);
    }

    #[test]
    fn multi_block_frames_are_rejected() {
        assert!(strip_adts(&adts(10, false, 2)).is_err());
    }

    #[test]
    fn asc_matches_adts_header() {
        let info = parse_adts(&adts(4, false, 1)).unwrap();
        // AAC-LC (2), 44.1 kHz (4), stereo (2): 00010 0100 0010 000.
        assert_eq!(asc_from_adts(&info), vec![0x12, 0x10]);
        let mut p = CodecParameters::audio(oxideav_core::CodecId::new("aac"));
        p.sample_rate = Some(48_000);
        p.channels = Some(1);
        assert_eq!(lc_asc_from_params(&p).unwrap(), vec![0x11, 0x88]);
        p.sample_rate = Some(12_345);
        assert!(lc_asc_from_params(&p).is_err());
    }
}
