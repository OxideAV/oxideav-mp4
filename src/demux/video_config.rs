//! Decoded-picture layout of an H.264 / H.265 track, read from its
//! decoder configuration record.
//!
//! The sample entry itself only says `avc1` / `hvc1` and the
//! dimensions. Consumers that plan pixel-format conversions before
//! decoding (a still-image encoder that needs RGB, a Y4M muxer)
//! need the layout the decoder will emit, which the configuration
//! record pins down:
//!
//! * `avcC` (ISO/IEC 14496-15 §5.3.3.1) carries the SPS NAL units;
//!   `chroma_format_idc` and the bit depths are read from the first
//!   SPS (ITU-T H.264 §7.3.2.1.1). Profiles without those syntax
//!   elements are 4:2:0 at 8 bits (§7.4.2.1.1 inference).
//! * `hvcC` (§8.3.3.1) carries `chromaFormat`,
//!   `bitDepthLumaMinus8` and `bitDepthChromaMinus8` directly.
//!
//! The mapping follows the decoders' output convention: planar Y, Cb,
//! Cr (luma only for 4:0:0); samples above 8 bits as little-endian
//! `u16`.

use oxideav_core::PixelFormat;

/// The picture layout implied by `extradata` for `codec_id` (`h264`
/// or `h265` / `hevc`), or `None` when the record is absent,
/// malformed or describes a layout without a [`PixelFormat`].
pub(crate) fn pixel_format_from_config(codec_id: &str, extradata: &[u8]) -> Option<PixelFormat> {
    let (chroma, luma_bits, chroma_bits) = match codec_id {
        "h264" => avcc_layout(extradata)?,
        "h265" | "hevc" => hvcc_layout(extradata)?,
        _ => return None,
    };
    if chroma != 0 && luma_bits != chroma_bits {
        return None;
    }
    use PixelFormat::*;
    Some(match (chroma, luma_bits) {
        (0, 8) => Gray8,
        (0, 10) => Gray10Le,
        (0, 12) => Gray12Le,
        (1, 8) => Yuv420P,
        (2, 8) => Yuv422P,
        (3, 8) => Yuv444P,
        (1, 10) => Yuv420P10Le,
        (2, 10) => Yuv422P10Le,
        (3, 10) => Yuv444P10Le,
        (1, 12) => Yuv420P12Le,
        (2, 12) => Yuv422P12Le,
        (3, 12) => Yuv444P12Le,
        _ => return None,
    })
}

/// `(chroma_format_idc, luma bits, chroma bits)` from the first SPS of
/// an `AVCDecoderConfigurationRecord`.
fn avcc_layout(rec: &[u8]) -> Option<(u8, u8, u8)> {
    if rec.len() < 8 || rec[0] != 1 || rec[5] & 0x1f == 0 {
        return None;
    }
    let len = u16::from_be_bytes([rec[6], rec[7]]) as usize;
    let sps = rec.get(8..8 + len)?;
    sps_layout(sps)
}

/// H.264 §7.3.2.1.1 up to `bit_depth_chroma_minus8`.
fn sps_layout(nal: &[u8]) -> Option<(u8, u8, u8)> {
    // NAL header (1 byte) must be an SPS (nal_unit_type 7).
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(nal.get(1..)?);
    let profile_idc = *rbsp.first()?;
    let mut r = BitReader::new(rbsp.get(3..)?);
    let _sps_id = r.ue()?;
    if !matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        return Some((1, 8, 8));
    }
    let chroma = r.ue()?;
    if chroma > 3 {
        return None;
    }
    if chroma == 3 && r.bit()? {
        // separate_colour_plane_flag: three independently coded
        // colour planes — no single planar layout to promise.
        return None;
    }
    let luma = r.ue()?.checked_add(8)?;
    let chroma_bits = r.ue()?.checked_add(8)?;
    Some((
        chroma as u8,
        u8::try_from(luma).ok()?,
        u8::try_from(chroma_bits).ok()?,
    ))
}

/// `HEVCDecoderConfigurationRecord` bytes 16..=18: `chromaFormat`
/// (low 2 bits), `bitDepthLumaMinus8`, `bitDepthChromaMinus8` (low 3
/// bits each).
fn hvcc_layout(rec: &[u8]) -> Option<(u8, u8, u8)> {
    if rec.len() < 23 || rec[0] != 1 {
        return None;
    }
    Some((rec[16] & 0x03, (rec[17] & 0x07) + 8, (rec[18] & 0x07) + 8))
}

/// Strip emulation-prevention bytes (`00 00 03` → `00 00`, H.264
/// §7.4.1).
fn unescape(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b == 1)
    }

    /// `ue(v)` Exp-Golomb (§9.1), capped at 31 leading zeros.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v = 0u64;
        for _ in 0..zeros {
            v = (v << 1) | self.bit()? as u64;
        }
        u32::try_from((1u64 << zeros) - 1 + v).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(sps: &[u8]) -> Vec<u8> {
        let mut r = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        r.extend_from_slice(sps);
        r.push(0);
        r
    }

    #[test]
    fn baseline_and_main_are_420_8bit() {
        // profile 66, sps_id 0 (`1`), then anything.
        let sps = [0x67, 66, 0xc0, 0x1e, 0b1000_0000];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Yuv420P)
        );
    }

    #[test]
    fn high_profiles_read_chroma_and_depth_from_the_sps() {
        // profile 100: sps_id ue=0 `1`, chroma ue=1 `010`,
        // luma-8 ue=0 `1`, chroma-8 ue=0 `1` → 1010 11xx
        let sps = [0x67, 100, 0, 0x1f, 0b1010_1100];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Yuv420P)
        );
        // profile 110: chroma 1, luma-8 = 2 `011`, chroma-8 = 2 `011`
        // → 1 010 011 011 → 1010 0110 11xx xxxx
        let sps = [0x67, 110, 0, 0x1f, 0b1010_0110, 0b1100_0000];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Yuv420P10Le)
        );
        // profile 244: chroma 3 `00100`, separate_colour_plane 0,
        // depths 0 / 0 → 1 00100 0 1 1
        let sps = [0x67, 244, 0, 0x1f, 0b1001_0001, 0b1000_0000];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Yuv444P)
        );
        // chroma 0 (monochrome) `1`: 1 1 1 1
        let sps = [0x67, 100, 0, 0x1f, 0b1111_0000];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Gray8)
        );
    }

    #[test]
    fn emulation_prevention_is_stripped_before_parsing() {
        // profile 100, constraint 0, level 0 → `00 00 03` escape in
        // the header bytes; sps_id 0, chroma 2 `011`, depths 0/0.
        let sps = [0x67, 100, 0, 0, 3, 0b1011_1100];
        assert_eq!(
            pixel_format_from_config("h264", &avcc(&sps)),
            Some(PixelFormat::Yuv422P)
        );
    }

    #[test]
    fn hvcc_fields_map_directly() {
        let mut rec = vec![1u8; 23];
        rec[16] = 0xfc | 1;
        rec[17] = 0xf8 | 2;
        rec[18] = 0xf8 | 2;
        assert_eq!(
            pixel_format_from_config("h265", &rec),
            Some(PixelFormat::Yuv420P10Le)
        );
        rec[18] = 0xf8;
        assert_eq!(pixel_format_from_config("hevc", &rec), None);
    }

    #[test]
    fn missing_or_foreign_records_give_none() {
        assert_eq!(pixel_format_from_config("h264", &[]), None);
        assert_eq!(pixel_format_from_config("h264", &[1, 2, 3]), None);
        assert_eq!(pixel_format_from_config("vp9", &[1; 30]), None);
    }
}
