//! Map between MP4 sample-entry FourCCs and oxideav codec IDs.
//!
//! Two entry points:
//!
//! - [`from_sample_entry`] — pure FourCC lookup (no extra context). Used as
//!   the initial guess and when the track has no esds box.
//! - [`from_sample_entry_with_oti`] — FourCC + esds
//!   `objectTypeIndication` byte. This disambiguates `mp4a` / `mp4v`
//!   sample entries that can carry multiple codecs (MPEG-1 Audio vs. AAC;
//!   MPEG-1 video vs. MPEG-4 Part 2). Prefer this when the track has an
//!   esds box.
//!
//! OTI values come from the MP4 registration authority
//! (ISO/IEC 14496-1 Annex G / mp4ra.org).

use oxideav_core::CodecId;

pub fn from_sample_entry(fourcc: &[u8; 4]) -> CodecId {
    let id = match fourcc {
        b"mp4a" => "aac",
        b"alac" => "alac",
        b"fLaC" | b"flac" => "flac",
        b"Opus" | b"opus" => "opus",
        b"avc1" | b"avc3" => "h264",
        b"hvc1" | b"hev1" => "h265",
        b"vp08" => "vp8",
        b"vp09" => "vp9",
        b"av01" => "av1",
        b"jpeg" | b"mjpa" | b"mjpb" => "mjpeg",
        // MP4 sample entry `mp4v` is carried for both MPEG-1 video (OTI 0x6A)
        // and MPEG-4 Part 2 / ASP (OTI 0x20). Part 2 is overwhelmingly more
        // common in MP4, so default to `mpeg4video` here when no OTI is
        // available. The `*_with_oti` variant refines this.
        b"mp4v" => "mpeg4video",
        // ITU-T H.263 baseline. The 3GPP MP4 sample-entry FourCC is `s263`
        // (with a `d263`/`bitr` configuration sub-box); some legacy QuickTime
        // movies use `h263` directly.
        b"s263" | b"h263" => "h263",
        b"lpcm" | b"sowt" | b"twos" => "pcm_s16le",
        other => {
            let s = std::str::from_utf8(other).unwrap_or("????");
            return CodecId::new(format!("mp4:{s}"));
        }
    };
    CodecId::new(id)
}

/// Refined version of [`from_sample_entry`] that takes the esds
/// `objectTypeIndication` (OTI) byte into account. Only meaningful for
/// sample entries whose FourCC is `mp4a` / `mp4v` (where the OTI selects
/// the actual codec); for every other FourCC the OTI is ignored and we
/// fall back to [`from_sample_entry`].
///
/// Key OTI values from the MP4 registration authority:
///
/// | OTI  | Codec                                 |
/// |------|---------------------------------------|
/// | 0x20 | MPEG-4 Visual (Part 2 / ASP)          |
/// | 0x21 | H.264 / AVC video                     |
/// | 0x23 | H.265 / HEVC video                    |
/// | 0x40 | MPEG-4 Audio (AAC etc.)               |
/// | 0x60..=0x65 | MPEG-2 video (various profiles)|
/// | 0x66..=0x68 | MPEG-2 Audio AAC (LC/SSR/main) |
/// | 0x69 | MPEG-2 Audio Part 3 (MP2/MP3)         |
/// | 0x6A | MPEG-1 Video                          |
/// | 0x6B | MPEG-1 Audio Part 3 Layer I/II/III    |
/// | 0x6C | JPEG image                            |
pub fn from_sample_entry_with_oti(fourcc: &[u8; 4], oti: u8) -> CodecId {
    match fourcc {
        b"mp4a" => {
            // The `mp4a` sample entry carries any codec that speaks the
            // MPEG-4 ES descriptor framework. Disambiguate by OTI.
            let id = match oti {
                0x40 | 0x66 | 0x67 | 0x68 => "aac",
                // 0x69 = MPEG-2 audio, 0x6B = MPEG-1 audio. Both cover
                // Layers I-III; the actual layer lives in each frame's
                // syncword — map the container-level id to the most common
                // mapping (Layer III / "mp3"). Demuxers/decoders can refine
                // further by sniffing the bitstream if they care.
                0x69 | 0x6B => "mp3",
                _ => {
                    // Unknown/reserved OTI: keep the bare AAC default —
                    // matches historical behaviour. Callers who need the
                    // raw OTI can reach for `CodecId::new(format!("mp4a:0x{:02x}", oti))`.
                    "aac"
                }
            };
            CodecId::new(id)
        }
        b"mp4v" => {
            let id = match oti {
                0x6A => "mpeg1video",
                0x60..=0x65 => "mpeg2video",
                0x21 => "h264",
                0x23 => "h265",
                0x6C => "mjpeg",
                _ => "mpeg4video",
            };
            CodecId::new(id)
        }
        _ => from_sample_entry(fourcc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp4a_default_is_aac() {
        assert_eq!(from_sample_entry(b"mp4a"), CodecId::new("aac"));
    }

    #[test]
    fn mp4a_with_aac_oti_is_aac() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4a", 0x40),
            CodecId::new("aac")
        );
    }

    #[test]
    fn mp4a_with_mpeg1_audio_oti_is_mp3() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4a", 0x6B),
            CodecId::new("mp3")
        );
    }

    #[test]
    fn mp4a_with_mpeg2_audio_oti_is_mp3() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4a", 0x69),
            CodecId::new("mp3")
        );
    }

    #[test]
    fn mp4v_default_is_mpeg4video() {
        assert_eq!(from_sample_entry(b"mp4v"), CodecId::new("mpeg4video"));
    }

    #[test]
    fn mp4v_with_mpeg1_oti_is_mpeg1video() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4v", 0x6A),
            CodecId::new("mpeg1video")
        );
    }

    #[test]
    fn mp4v_with_mpeg2_oti_is_mpeg2video() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4v", 0x61),
            CodecId::new("mpeg2video")
        );
    }

    #[test]
    fn mp4v_with_part2_oti_is_mpeg4video() {
        assert_eq!(
            from_sample_entry_with_oti(b"mp4v", 0x20),
            CodecId::new("mpeg4video")
        );
    }

    #[test]
    fn oti_is_ignored_for_non_mp4_fourccs() {
        // avc1 should always be h264 regardless of OTI (which shouldn't even
        // exist on avc1 entries in practice, but we defend against garbage).
        assert_eq!(
            from_sample_entry_with_oti(b"avc1", 0x6A),
            CodecId::new("h264")
        );
    }

    #[test]
    fn unknown_fourcc_preserves_fallback() {
        let id = from_sample_entry(b"xyzw");
        assert_eq!(id.as_str(), "mp4:xyzw");
    }
}
