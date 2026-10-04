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

/// The size of the pictures the decoder outputs for `codec_id` (`h264`
/// or `h265` / `hevc`), read from the first SPS of the configuration
/// record in `extradata`: the coded size reduced by the cropping
/// window (H.264 §7.4.2.1.1 `frame_crop_*_offset` scaled by
/// `CropUnitX` / `CropUnitY`; H.265 §7.4.3.2.1 `conf_win_*_offset`
/// scaled by `SubWidthC` / `SubHeightC`). `None` when the record is
/// absent or the SPS cannot be read.
///
/// A sample entry may declare the coded (macroblock-aligned) size —
/// 1920x1088 for a 1080p stream — while the decoder emits the cropped
/// 1920x1080 pictures; the stream parameters must describe the
/// latter.
pub(crate) fn cropped_dimensions_from_config(
    codec_id: &str,
    extradata: &[u8],
) -> Option<(u32, u32)> {
    match codec_id {
        "h264" => avc_sps_cropped_size(avcc_first_sps(extradata)?),
        "h265" | "hevc" => hevc_sps_cropped_size(hvcc_first_sps(extradata)?),
        _ => None,
    }
}

/// The first SPS NAL unit of an `AVCDecoderConfigurationRecord`
/// (ISO/IEC 14496-15 §5.3.3.1).
fn avcc_first_sps(rec: &[u8]) -> Option<&[u8]> {
    if rec.len() < 8 || rec[0] != 1 || rec[5] & 0x1f == 0 {
        return None;
    }
    let len = u16::from_be_bytes([rec[6], rec[7]]) as usize;
    rec.get(8..8 + len)
}

/// The first SPS NAL unit (`nal_unit_type` 33) of an
/// `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15 §8.3.3.1): the
/// 23-byte fixed header ends with `numOfArrays`; each array is one
/// byte (`NAL_unit_type` in the low 6 bits), a 16-bit `numNalus` and
/// that many 16-bit-length-prefixed NAL units.
fn hvcc_first_sps(rec: &[u8]) -> Option<&[u8]> {
    if rec.len() < 23 || rec[0] != 1 {
        return None;
    }
    let arrays = rec[22];
    let mut pos = 23;
    for _ in 0..arrays {
        let nal_type = rec.get(pos)? & 0x3f;
        let count = u16::from_be_bytes([*rec.get(pos + 1)?, *rec.get(pos + 2)?]);
        pos += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes([*rec.get(pos)?, *rec.get(pos + 1)?]) as usize;
            let nal = rec.get(pos + 2..pos + 2 + len)?;
            if nal_type == 33 {
                return Some(nal);
            }
            pos += 2 + len;
        }
    }
    None
}

/// H.264 §7.3.2.1.1 `seq_parameter_set_data()` up to the frame
/// cropping offsets → the cropped output size (§7.4.2.1.1).
fn avc_sps_cropped_size(nal: &[u8]) -> Option<(u32, u32)> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(nal.get(1..)?);
    let profile_idc = *rbsp.first()?;
    let mut r = BitReader::new(rbsp.get(3..)?);
    let _sps_id = r.ue()?;
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()?;
        }
        let _bit_depth_luma_minus8 = r.ue()?;
        let _bit_depth_chroma_minus8 = r.ue()?;
        let _qpprime_y_zero_transform_bypass = r.bit()?;
        if r.bit()? {
            // seq_scaling_matrix_present_flag: §7.3.2.1.1.1
            // scaling_list() for every present list.
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.bit()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let _log2_max_frame_num_minus4 = r.ue()?;
    match r.ue()? {
        0 => {
            let _log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
        }
        1 => {
            let _delta_pic_order_always_zero = r.bit()?;
            let _offset_for_non_ref_pic = r.se()?;
            let _offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                let _offset_for_ref_frame = r.se()?;
            }
        }
        _ => {}
    }
    let _max_num_ref_frames = r.ue()?;
    let _gaps_in_frame_num_value_allowed = r.bit()?;
    let width_in_mbs = r.ue()?.checked_add(1)?;
    let height_in_map_units = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.bit()?;
    if !frame_mbs_only {
        let _mb_adaptive_frame_field = r.bit()?;
    }
    let _direct_8x8_inference = r.bit()?;
    let width = width_in_mbs.checked_mul(16)?;
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let height = height_in_map_units.checked_mul(16 * field_factor)?;
    if !r.bit()? {
        return Some((width, height));
    }
    let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
    // eq. 7-19 .. 7-22: CropUnitX / CropUnitY from ChromaArrayType.
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_w, sub_h) = match chroma_array_type {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_x = left.checked_add(right)?.checked_mul(sub_w)?;
    let crop_y = top.checked_add(bottom)?.checked_mul(sub_h * field_factor)?;
    if crop_x >= width || crop_y >= height {
        return None;
    }
    Some((width - crop_x, height - crop_y))
}

/// H.264 §7.3.2.1.1.1 `scaling_list()`: only the delta syntax is
/// read; the values are irrelevant here.
fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            let delta = i64::from(r.se()?);
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// H.265 §7.3.2.2.1 `seq_parameter_set_rbsp()` up to the conformance
/// window → the cropped output size (§7.4.3.2.1).
fn hevc_sps_cropped_size(nal: &[u8]) -> Option<(u32, u32)> {
    // Two-byte NAL unit header; nal_unit_type 33 = SPS_NUT.
    if (nal.first()? >> 1) & 0x3f != 33 {
        return None;
    }
    let rbsp = unescape(nal.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)? as usize;
    r.skip(1)?; // sps_temporal_id_nesting_flag
                // §7.3.3 profile_tier_level(1, sps_max_sub_layers_minus1): the
                // general profile (88 bits) + general_level_idc (8 bits) …
    r.skip(96)?;
    let mut present = [(false, false); 8];
    for p in present.iter_mut().take(max_sub_layers_minus1) {
        *p = (r.bit()?, r.bit()?);
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1))?; // reserved_zero_2bits
    }
    for &(profile, level) in present.iter().take(max_sub_layers_minus1) {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    let _sps_id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_plane = chroma_format_idc == 3 && r.bit()?;
    let width = r.ue()?;
    let height = r.ue()?;
    if !r.bit()? {
        return Some((width, height));
    }
    let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
    // Table 6-1: SubWidthC / SubHeightC (1 / 1 when ChromaArrayType is 0).
    let (sub_w, sub_h) = match (chroma_format_idc, separate_colour_plane) {
        (1, _) => (2, 2),
        (2, _) => (2, 1),
        _ => (1, 1),
    };
    let crop_x = left.checked_add(right)?.checked_mul(sub_w)?;
    let crop_y = top.checked_add(bottom)?.checked_mul(sub_h)?;
    if crop_x >= width || crop_y >= height {
        return None;
    }
    Some((width - crop_x, height - crop_y))
}

/// `(chroma_format_idc, luma bits, chroma bits)` from the first SPS of
/// an `AVCDecoderConfigurationRecord`.
fn avcc_layout(rec: &[u8]) -> Option<(u8, u8, u8)> {
    sps_layout(avcc_first_sps(rec)?)
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

    /// `u(n)` for `n <= 32`.
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.bit()?);
        }
        Some(v)
    }

    /// Skip `n` bits, failing past the end of the data.
    fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n)?;
        if end > self.data.len() * 8 {
            return None;
        }
        self.pos = end;
        Some(())
    }

    /// `se(v)` (§9.1.1): `ue(v)` mapped to 0, 1, −1, 2, −2, …
    fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).ok()
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

    /// SPS NAL units written by black-box encoders for a 100x60 clip
    /// (coded as 112x64), extracted from:
    ///
    /// ```text
    /// ffmpeg -f lavfi -i testsrc2=size=100x60:rate=25 -frames:v 1 \
    ///        -c:v libx264 -pix_fmt yuv420p -f h264 a.h264
    /// ffmpeg -f lavfi -i testsrc2=size=100x60:rate=25 -frames:v 1 \
    ///        -c:v libx265 -pix_fmt yuv420p -f hevc a.hevc
    /// ```
    const X264_SPS_100X60: &[u8] = &[
        0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x47, 0x27, 0x9e, 0xf0, 0x11, 0x00, 0x00, 0x03, 0x00,
        0x01, 0x00, 0x00, 0x03, 0x00, 0x32, 0x0f, 0x12, 0x25, 0x96,
    ];
    const X265_SPS_100X60: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x1e, 0xa0, 0x34, 0x81, 0x07, 0x77, 0x96, 0x56, 0x6b, 0x93, 0x2b, 0xc0, 0x5a,
        0x02, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x32, 0x10,
    ];

    fn hvcc(sps: &[u8]) -> Vec<u8> {
        let mut r = vec![1u8; 22];
        r.push(1); // numOfArrays
        r.push(0x80 | 33);
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        r.extend_from_slice(sps);
        r
    }

    #[test]
    fn h264_cropping_window_gives_the_output_size() {
        assert_eq!(
            cropped_dimensions_from_config("h264", &avcc(X264_SPS_100X60)),
            Some((100, 60))
        );
    }

    #[test]
    fn h264_without_cropping_gives_the_coded_size() {
        // Baseline: sps_id 0 `1`, log2_max_frame_num-4 = 0 `1`,
        // poc_type 2 `011`, max_num_ref_frames 1 `010`, gaps 0,
        // width_in_mbs-1 = 7 `0001000`, height_in_map_units-1 = 5
        // `00110`, frame_mbs_only 1, direct_8x8 1, cropping 0:
        // 1 1 011 010 | 0 0001000 | 00110 1 1 0.
        let sps = [0x67, 66, 0xc0, 0x1e, 0b1101_1010, 0b0000_1000, 0b0011_0110];
        assert_eq!(
            cropped_dimensions_from_config("h264", &avcc(&sps)),
            Some((128, 96))
        );
    }

    #[test]
    fn hevc_conformance_window_gives_the_output_size() {
        assert_eq!(
            cropped_dimensions_from_config("h265", &hvcc(X265_SPS_100X60)),
            Some((100, 60))
        );
        assert_eq!(cropped_dimensions_from_config("hevc", &[1; 23]), None);
    }

    #[test]
    fn missing_or_foreign_records_give_none() {
        assert_eq!(cropped_dimensions_from_config("h264", &[]), None);
        assert_eq!(cropped_dimensions_from_config("vp9", &[1; 30]), None);
        assert_eq!(pixel_format_from_config("h264", &[]), None);
        assert_eq!(pixel_format_from_config("h264", &[1, 2, 3]), None);
        assert_eq!(pixel_format_from_config("vp9", &[1; 30]), None);
    }
}
