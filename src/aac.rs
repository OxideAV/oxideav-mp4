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
//!
//! The demuxer uses [`asc_rates`] to report an HE-AAC stream at its SBR
//! output rate when the sample entry carries the core rate.

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
            )));
        }
    };
    Ok(asc_bytes(2, idx, cfg))
}

/// The rate-relevant summary of an `AudioSpecificConfig`
/// (ISO/IEC 14496-3 §1.6.2.1 Table 1.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AscRates {
    /// Core (AAC) sampling frequency.
    pub core_rate: u32,
    /// SBR output sampling frequency, when SBR is signalled —
    /// explicitly (`audioObjectType` 5 / 29) or through the §1.6.5
    /// backward-compatible `syncExtensionType 0x2b7` trailer.
    pub sbr_rate: Option<u32>,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }
    fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }
    fn aot(&mut self) -> Option<u32> {
        match self.read(5)? {
            31 => Some(32 + self.read(6)?),
            v => Some(v),
        }
    }
    fn rate(&mut self) -> Option<u32> {
        match self.read(4)? {
            15 => self.read(24),
            i => SAMPLE_RATES.get(i as usize).copied(),
        }
    }
}

/// The output channel count an `AudioSpecificConfig` declares: its
/// `channelConfiguration` (ISO/IEC 14496-3:2019 Table 1.23: 1-6
/// channels, 7 is 7.1 Front = 8, 11 is 6.1 = 7, 12 is 7.1 Back = 8,
/// 13 is 22.2 = 24, 14 is 7.1 Top = 8), or 2 for a mono core with
/// parametric stereo signalled hierarchically (AOT 29) or by the
/// §1.6.6 backward-compatible sync extension (`0x2B7` SBR, then
/// `0x548` with `psPresentFlag`). `None` for configuration 0 (a PCE
/// describes the channels), the reserved values 8-10 / 15, or when the
/// leading fields do not parse.
pub(crate) fn asc_channel_count(asc: &[u8]) -> Option<u16> {
    let mut b = Bits { data: asc, pos: 0 };
    let aot = b.aot()?;
    b.rate()?;
    let n = match b.read(4)? {
        cfg @ 1..=6 => cfg as u16,
        7 | 12 | 14 => 8,
        11 => 7,
        13 => 24,
        _ => return None,
    };
    if n != 1 {
        return Some(n);
    }
    if aot == 29 {
        return Some(2);
    }
    if !(1..=4).contains(&aot) {
        return Some(1);
    }
    // GASpecificConfig (Table 4.1) for AOT 1-4, then the sync extensions.
    let ps = (|| {
        b.read(1)?;
        if b.read(1)? == 1 {
            b.read(14)?;
        }
        if b.read(1)? == 1 {
            b.read(1)?;
        }
        if b.remaining() < 16 || b.read(11)? != 0x2B7 || b.aot()? != 5 || b.read(1)? != 1 {
            return Some(false);
        }
        b.rate()?;
        Some(b.remaining() >= 12 && b.read(11)? == 0x548 && b.read(1)? == 1)
    })()
    .unwrap_or(false);
    Some(if ps { 2 } else { 1 })
}

/// Parse the sampling-frequency fields of an `AudioSpecificConfig`.
/// The §1.6.5 trailing SBR probe is only attempted for the plain
/// General-Audio object types (1–4) without an inline PCE, whose
/// `GASpecificConfig` length is fixed; anything else reports the core
/// rate alone (`None` only when the leading fields do not parse).
pub(crate) fn asc_rates(asc: &[u8]) -> Option<AscRates> {
    let mut b = Bits { data: asc, pos: 0 };
    let aot = b.aot()?;
    let core = b.rate()?;
    let chan_cfg = b.read(4)?;
    if aot == 5 || aot == 29 {
        // Hierarchical signalling: the leading rate is the core rate,
        // extensionSamplingFrequency the SBR output rate.
        let ext = b.rate()?;
        return Some(AscRates {
            core_rate: core,
            sbr_rate: Some(ext),
        });
    }
    let mut out = AscRates {
        core_rate: core,
        sbr_rate: None,
    };
    if !(1..=4).contains(&aot) || chan_cfg == 0 {
        return Some(out);
    }
    // GASpecificConfig (Table 4.1) for AOT 1-4: frameLengthFlag,
    // dependsOnCoreCoder (+ 14-bit coreCoderDelay), extensionFlag
    // (+ extensionFlag3 for these AOTs).
    let parse_tail = |b: &mut Bits<'_>| -> Option<Option<u32>> {
        b.read(1)?;
        if b.read(1)? == 1 {
            b.read(14)?;
        }
        if b.read(1)? == 1 {
            b.read(1)?;
        }
        if b.remaining() < 16 || b.read(11)? != 0x2B7 {
            return Some(None);
        }
        if b.aot()? != 5 || b.read(1)? != 1 {
            return Some(None);
        }
        Some(Some(b.rate()?))
    };
    out.sbr_rate = parse_tail(&mut b).flatten();
    Some(out)
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
    fn asc_channel_count_follows_channel_configuration() {
        assert_eq!(asc_channel_count(&[0x12, 0x10]), Some(2)); // LC 44.1k stereo
        assert_eq!(asc_channel_count(&[0x11, 0xB0]), Some(6)); // LC 48k 5.1
        assert_eq!(asc_channel_count(&[0x11, 0xB8]), Some(8)); // LC 48k 7.1
        assert_eq!(asc_channel_count(&[0x11, 0x88]), Some(1)); // LC 48k mono
        // HE-AAC v2, sync-extension signalling (macOS's encoder):
        // 22.05k mono core, SBR to 44.1k, psPresentFlag.
        assert_eq!(
            asc_channel_count(&[0x13, 0x88, 0x56, 0xE5, 0xA5, 0x48, 0x80]),
            Some(2)
        );
        // HE-AAC v1 mono: SBR, no PS extension.
        assert_eq!(asc_channel_count(&[0x13, 0x08, 0x56, 0xE5, 0x98]), Some(1));
        assert_eq!(asc_channel_count(&[0x11, 0x80]), None); // PCE
        assert_eq!(asc_channel_count(&[]), None);
        // Table 1.23 configurations 11-14 (6.1, 7.1 Back, 22.2, 7.1
        // Top); 8-10 and 15 are reserved.
        assert_eq!(asc_channel_count(&[0x11, 0xD8]), Some(7)); // LC 48k cc 11
        assert_eq!(asc_channel_count(&[0x11, 0xE0]), Some(8)); // LC 48k cc 12
        assert_eq!(asc_channel_count(&[0x11, 0xE8]), Some(24)); // LC 48k cc 13
        assert_eq!(asc_channel_count(&[0x11, 0xF0]), Some(8)); // LC 48k cc 14
        assert_eq!(asc_channel_count(&[0x11, 0xC0]), None); // reserved 8
        assert_eq!(asc_channel_count(&[0x11, 0xF8]), None); // reserved 15
    }

    #[test]
    fn asc_rates_cover_all_sbr_signalling_forms() {
        assert_eq!(asc_rates(&[0x12, 0x10]).unwrap().sbr_rate, None);
        assert_eq!(
            asc_rates(&[0x13, 0x90, 0x56, 0xE5, 0xA0]),
            Some(AscRates {
                core_rate: 22_050,
                sbr_rate: Some(44_100)
            })
        );
        assert_eq!(
            asc_rates(&[0x12, 0x10, 0x56, 0xE5, 0x00]).unwrap().sbr_rate,
            None
        );
        assert_eq!(
            asc_rates(&[0x2B, 0x92, 0x08, 0x00]).unwrap().sbr_rate,
            Some(44_100)
        );
        assert_eq!(asc_rates(&[]), None);
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
