//! ISO Base Media File Format box header reader (ISO/IEC 14496-12).
//!
//! A box has a 4-byte big-endian size field followed by a 4-byte FourCC type.
//! - `size == 1` → the *next* 8 bytes are a 64-bit large size.
//! - `size == 0` → box extends to end of file.
//!
//! All multi-byte integers in MP4 are big-endian.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{Error, Result};

/// Decoded box header.
#[derive(Clone, Copy, Debug)]
pub struct BoxHeader {
    /// FourCC type, as a u32 with the 4 ASCII characters in big-endian order.
    pub fourcc: [u8; 4],
    /// Total box size in bytes (including the header itself). `None` means
    /// "rest of file" (input size==0).
    pub total_size: Option<u64>,
    /// Bytes consumed by the header (8 or 16).
    pub header_len: u64,
}

impl BoxHeader {
    pub fn type_str(&self) -> &str {
        std::str::from_utf8(&self.fourcc).unwrap_or("????")
    }

    pub fn payload_size(&self) -> Option<u64> {
        self.total_size.map(|t| t - self.header_len)
    }
}

pub fn read_box_header<R: Read + Seek + ?Sized>(r: &mut R) -> Result<Option<BoxHeader>> {
    let start = r.stream_position()?;

    let mut hdr = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        match r.read(&mut hdr[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Ok(None);
                } else {
                    return Err(Error::invalid("MP4: truncated box header"));
                }
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let mut fourcc = [0u8; 4];
    fourcc.copy_from_slice(&hdr[4..8]);
    // ISO/IEC 14496-12 §4.2: `size` is the total box length including
    // the header. `size == 0` ⇒ extends to EOF; `size == 1` ⇒ a 64-bit
    // `largesize` follows; otherwise the value must be at least the
    // header length itself (8 bytes for the 32-bit form, 16 bytes once
    // largesize has been consumed). Any other small value would make
    // the body length negative; reject it here so every caller that
    // computes `payload = total_size - header_len` is safe.
    let (total_size, header_len) = match size32 {
        0 => (None, 8u64),
        1 => {
            let mut ext = [0u8; 8];
            r.read_exact(&mut ext)?;
            let large = u64::from_be_bytes(ext);
            if large < 16 {
                return Err(Error::invalid("MP4: box largesize < 16"));
            }
            (Some(large), 16u64)
        }
        n if n < 8 => {
            return Err(Error::invalid("MP4: box size < 8"));
        }
        n => (Some(n as u64), 8u64),
    };

    // Reject any header whose declared end byte would overflow `u64`.
    // This is the single point that bounds every downstream
    // `body_start + payload_size` / `box_end` computation: once we have
    // proven `start + total_size <= u64::MAX`, the equivalent form
    // `(start + header_len) + (total_size - header_len)` also fits, so
    // the top-level walker in `demux.rs` (the `sidx` `body_start +
    // payload_size` site and every `cur.position() + payload` site
    // under it) can no longer integer-overflow on a forged extended
    // size near `u64::MAX`. Companion to round 187 in `oxideav-mov`,
    // which closed the same shape on the QTFF atom walker for fuzz
    // crash `353fbd8c…`: an 8-byte placeholder box followed by a
    // `size=1` extended box whose `largesize = u64::MAX` overflows
    // every downstream `start + total_size` arithmetic site in debug
    // builds (panic) and silently wraps in release builds.
    if let Some(t) = total_size {
        if start.checked_add(t).is_none() {
            return Err(Error::invalid(format!(
                "MP4: box '{}' declared size {t} from offset {start} overflows u64",
                std::str::from_utf8(&fourcc).unwrap_or("????"),
            )));
        }
    }

    Ok(Some(BoxHeader {
        fourcc,
        total_size,
        header_len,
    }))
}

/// Read the full payload of a box as bytes. Fails if the box size is
/// unknown OR if the input ends before the declared payload size.
///
/// The size field is attacker-controlled (a 4 GiB+ `size32` field
/// fits in 32 bits), so we must not pre-allocate the declared length
/// before we know the input can supply it. `Read::take` + grow-as-we-go
/// caps both the allocation and the read budget at whatever the input
/// actually delivers; a partial read then trips the `payload as
/// usize` length check and we return `Error::invalid` instead of
/// `read_exact`'s `UnexpectedEof`.
pub fn read_box_body<R: Read + ?Sized>(r: &mut R, h: &BoxHeader) -> Result<Vec<u8>> {
    let payload = h
        .payload_size()
        .ok_or_else(|| Error::invalid("MP4: cannot read open-ended box body"))?;
    let mut buf = Vec::new();
    r.take(payload).read_to_end(&mut buf)?;
    if buf.len() as u64 != payload {
        return Err(Error::invalid("MP4: truncated box body"));
    }
    Ok(buf)
}

/// Skip the payload of a box in a seekable reader.
pub fn skip_box_body<R: Seek + ?Sized>(r: &mut R, h: &BoxHeader) -> Result<()> {
    if let Some(payload) = h.payload_size() {
        if payload > 0 {
            r.seek(SeekFrom::Current(payload as i64))?;
        }
    } else {
        // "rest of file" — seek to end.
        r.seek(SeekFrom::End(0))?;
    }
    Ok(())
}

/// Convert a 4-char literal into a FourCC byte array.
pub const fn fourcc(s: &str) -> [u8; 4] {
    let b = s.as_bytes();
    [b[0], b[1], b[2], b[3]]
}

// Common box types.
pub const FTYP: [u8; 4] = fourcc("ftyp");
/// `pdin` — ProgressiveDownloadInfoBox (ISO/IEC 14496-12 §8.1.3). A
/// top-level FullBox (file scope, quantity zero or one) carrying pairs
/// of `(rate, initial_delay)` u32s, each pair recommending an initial
/// playback delay (milliseconds) for a given effective download rate
/// (bytes/second). A receiver estimates the rate it is observing and
/// interpolates between adjacent pairs (or extrapolates from the first
/// / last entry) to pick a buffer occupancy that lets playback proceed
/// without stalls. The box is meant to be placed as early as possible
/// in the file (§8.1.3.1); the demuxer captures it from the top-level
/// walk and surfaces both a structured `PdinRecord` accessor and a flat
/// `pdin` metadata summary.
pub const PDIN: [u8; 4] = fourcc("pdin");
pub const MOOV: [u8; 4] = fourcc("moov");
pub const MVHD: [u8; 4] = fourcc("mvhd");
pub const TRAK: [u8; 4] = fourcc("trak");
pub const TKHD: [u8; 4] = fourcc("tkhd");
/// `tref` — TrackReferenceBox (ISO/IEC 14496-12 §8.3.3).
pub const TREF: [u8; 4] = fourcc("tref");
/// `trgr` — TrackGroupBox (ISO/IEC 14496-12 §8.3.4). Sits inside `trak`
/// as a plain container of zero or more `TrackGroupTypeBox` children
/// (each a FullBox whose FourCC is the `track_group_type` — `msrc` is
/// the spec-named example for multi-source presentations). Each child
/// body starts with a 32-bit `track_group_id`; the `(track_group_type,
/// track_group_id)` pair uniquely identifies a track group within the
/// file, and tracks carrying that same pair belong to the same group.
/// Track groups do **not** indicate dependency relationships — that's
/// what `tref` is for (§8.3.3).
pub const TRGR: [u8; 4] = fourcc("trgr");
pub const EDTS: [u8; 4] = fourcc("edts");
pub const MDIA: [u8; 4] = fourcc("mdia");
pub const MDHD: [u8; 4] = fourcc("mdhd");
/// `elng` — ExtendedLanguageBox (ISO/IEC 14496-12 §8.4.6). A peer of
/// the media header inside `mdia`; carries a NULL-terminated BCP 47
/// (RFC 4646) language tag string.
pub const ELNG: [u8; 4] = fourcc("elng");
pub const HDLR: [u8; 4] = fourcc("hdlr");
pub const MINF: [u8; 4] = fourcc("minf");
/// `vmhd` — VideoMediaHeaderBox (ISO/IEC 14496-12 §12.1.2, defined per
/// §8.4.5). Sits inside `minf`; the media-type-specific header for a
/// video track. Carries a 16-bit `graphicsmode` composition mode and a
/// three-component 16-bit `opcolor` (red, green, blue) for graphics
/// modes that use it. Note the spec fixes `flags` at 1 for this box.
pub const VMHD: [u8; 4] = fourcc("vmhd");
/// `hmhd` — HintMediaHeaderBox (ISO/IEC 14496-12 §12.4.2, defined per
/// §8.4.5). Sits inside `minf`; the media-type-specific header for a
/// hint track (a `hdlr` of type `hint`). A `FullBox(0, 0)` carrying
/// protocol-independent streaming statistics: `maxPDUsize` /
/// `avgPDUsize` (16-bit byte sizes of the largest / average Protocol
/// Data Unit) and `maxbitrate` / `avgbitrate` (32-bit bits/second over
/// any one-second window / the whole presentation), followed by a
/// reserved 32-bit zero.
pub const HMHD: [u8; 4] = fourcc("hmhd");
/// `gmhd` — QuickTime Base Media Information Header Atom. Sits inside
/// `minf` in place of a typed media header (`vmhd` / `smhd`) for media
/// types derived from the base media handler (text, timecode, music,
/// generic). A pure container whose required child is the `gmin` Base
/// Media Info Atom.
pub const GMHD: [u8; 4] = fourcc("gmhd");
/// `gmin` — QuickTime Base Media Info Atom. Sits inside `gmhd`; a
/// `FullBox` carrying the base media's control information: a 16-bit
/// `graphicsmode` composition mode, a three-component 16-bit `opcolor`,
/// a signed 16-bit stereo sound `balance`, and a reserved 16-bit zero.
pub const GMIN: [u8; 4] = fourcc("gmin");
/// `tcmi` — QuickTime Timecode Media Information Atom. Sits inside a
/// timecode track's `gmhd`; a `FullBox` carrying the text-rendering
/// parameters that govern how the timecode text is displayed: a text
/// font id, face (style), point size, 48-bit RGB text and background
/// colours, and a Pascal-string font name.
pub const TCMI: [u8; 4] = fourcc("tcmi");
/// `dinf` — DataInformationBox (ISO/IEC 14496-12 §8.7.1). Sits inside
/// `minf` (mandatory, exactly one) or `meta` (optional). A pure
/// container whose sole child of interest is the `dref` DataReferenceBox
/// declaring where the track's media data physically lives.
pub const DINF: [u8; 4] = fourcc("dinf");
/// `dref` — DataReferenceBox (ISO/IEC 14496-12 §8.7.2). Sits inside
/// `dinf`; a `FullBox(0, 0)` carrying an `entry_count` and that many
/// `DataEntryBox` children (each a `url ` or `urn ` FullBox). The
/// 1-based `data_reference_index` on every sample entry (§8.5.2.2)
/// indexes into this table, so it declares whether each description's
/// samples are self-contained in this file or stored in an external
/// resource.
pub const DREF: [u8; 4] = fourcc("dref");
/// `url ` — DataEntryUrlBox (ISO/IEC 14496-12 §8.7.2.2). A `dref`
/// child. When `flags & 1` is set the media is in this same file and no
/// URL string is present; otherwise the body is a NULL-terminated UTF-8
/// URL naming the external resource.
pub const URL_: [u8; 4] = fourcc("url ");
/// `urn ` — DataEntryUrnBox (ISO/IEC 14496-12 §8.7.2.2). A `dref`
/// child carrying a NULL-terminated `name` (the URN) and an optional
/// NULL-terminated `location` URL.
pub const URN_: [u8; 4] = fourcc("urn ");
pub const STBL: [u8; 4] = fourcc("stbl");
pub const STSD: [u8; 4] = fourcc("stsd");
pub const STTS: [u8; 4] = fourcc("stts");
pub const STSS: [u8; 4] = fourcc("stss");
pub const STSC: [u8; 4] = fourcc("stsc");
pub const STSZ: [u8; 4] = fourcc("stsz");
pub const STZ2: [u8; 4] = fourcc("stz2");
pub const STCO: [u8; 4] = fourcc("stco");
/// `stsh` — ShadowSyncSampleBox (ISO/IEC 14496-12 §8.6.3). Sits inside
/// `stbl`; an optional table of `(shadowed_sample_number,
/// sync_sample_number)` pairs that name an alternative sync sample to
/// use when seeking to a non-sync sample. Ignored in normal forward
/// play.
pub const STSH: [u8; 4] = fourcc("stsh");
/// `stdp` — DegradationPriorityBox (ISO/IEC 14496-12 §8.5.3). Sits
/// inside `stbl`; an optional per-sample table of 16-bit
/// degradation-priority values. `sample_count` is implicit from
/// `stsz` / `stz2`, mirroring the `sdtp` convention. The exact
/// meaning and acceptable value range of `priority` are left to
/// derived specifications (§8.5.3.1); the container preserves the
/// raw u16 per sample without interpreting it.
pub const STDP: [u8; 4] = fourcc("stdp");
/// `sdtp` — SampleDependencyTypeBox (ISO/IEC 14496-12 §8.6.4). Sits
/// inside `stbl`; an optional per-sample table of dependency hints —
/// `is_leading`, `sample_depends_on`, `sample_is_depended_on`,
/// `sample_has_redundancy` — useful for trick-mode playback (fast
/// forward / random-access roll-forward) and for dropping disposable
/// samples without decoding them. One byte per sample;
/// `sample_count` is implicit from `stsz` / `stz2`.
pub const SDTP: [u8; 4] = fourcc("sdtp");
/// `padb` — PaddingBitsBox (ISO/IEC 14496-12 §8.7.6). Sits inside
/// `stbl`; optional table recording, for each sample, how many bits at
/// the tail of the sample's last byte are padding (a value 0..=7). The
/// on-wire encoding packs two samples into one byte: each nibble is
/// `bit(1) reserved=0; bit(3) pad`. With `sample_count` declared
/// explicitly in the box (not implicit from `stsz`), the table has
/// `(sample_count + 1) / 2` bytes — the trailing nibble is unused when
/// `sample_count` is odd. The padding count tells a bitstream consumer
/// how many trailing bits of the sample to ignore; the container
/// preserves the per-sample u8s without applying any padding itself.
pub const PADB: [u8; 4] = fourcc("padb");
pub const CTTS: [u8; 4] = fourcc("ctts");
/// `cslg` — CompositionToDecodeBox (ISO/IEC 14496-12 §8.6.1.4). Sits
/// inside `stbl` (or `trep`); relates the composition and decoding
/// timelines when signed composition offsets (a v1 `ctts`) are in use.
pub const CSLG: [u8; 4] = fourcc("cslg");
pub const CO64: [u8; 4] = fourcc("co64");
/// `sbgp` — SampleToGroupBox (ISO/IEC 14496-12 §8.9.2). Sits inside
/// `stbl` (or `traf`); a run-length table mapping samples to a sample
/// group description index for a given `grouping_type`. Purely
/// descriptive metadata — the sample-group entries it indexes carry
/// codec/grouping-specific properties parsed by an upper layer.
pub const SBGP: [u8; 4] = fourcc("sbgp");
/// `sgpd` — SampleGroupDescriptionBox (ISO/IEC 14496-12 §8.9.3). Sits
/// inside `stbl` (or `traf`); the table of per-group descriptive
/// entries that an `sbgp` of the same `grouping_type` indexes. Entry
/// payloads are grouping-type-specific and opaque to the container.
pub const SGPD: [u8; 4] = fourcc("sgpd");
/// `csgp` — CompactSampleToGroupBox (ISO/IEC 14496-12:2020 §8.9.5).
/// Sits inside `stbl` (or `traf`); the compact form of `sbgp`. Instead
/// of one `(sample_count, group_description_index)` pair per run, it
/// encodes a small set of repeating index **patterns** that are
/// replicated across the track, with field widths selected by code
/// values packed into the `FullBox.flags`. Like `sbgp`, it indexes the
/// per-group entries of an `sgpd` of the same `grouping_type`.
pub const CSGP: [u8; 4] = fourcc("csgp");
/// `subs` — SubSampleInformationBox (ISO/IEC 14496-12 §8.7.7). Sits
/// inside `stbl` (or `traf`); an optional sparse table describing how
/// selected samples decompose into smaller, semantically meaningful
/// sub-samples (e.g. NAL units, slices, parameter sets for H.264 per
/// ISO/IEC 14496-15). Per-sub-sample fields: `subsample_size`
/// (16-bit at version 0, 32-bit at version 1), `subsample_priority`,
/// `discardable`, and a `codec_specific_parameters` blob whose
/// semantics are owned by the carried codec. Purely descriptive — a
/// track decodes correctly without it; the table feeds trick modes,
/// CENC sub-sample encryption mapping, and selective-discard pipelines.
pub const SUBS: [u8; 4] = fourcc("subs");
/// `saiz` — SampleAuxiliaryInformationSizesBox (ISO/IEC 14496-12 §8.7.8).
/// Sits inside `stbl` or `traf`. Per-sample sizes for one stream of
/// sample auxiliary information identified by an `(aux_info_type,
/// aux_info_type_parameter)` key. The matching `saio` of the same key
/// supplies the byte offsets where each chunk's auxiliary info lives.
/// One common consumer is CENC: the per-sample IV + subsample map
/// (ISO/IEC 23001-7) is an auxiliary-information stream of type
/// `cenc` / `cbc1` / `cens` / `cbcs`, and the `saiz`+`saio` pair is the
/// spec-permitted alternative to a `senc` box for IV carriage.
pub const SAIZ: [u8; 4] = fourcc("saiz");
/// `saio` — SampleAuxiliaryInformationOffsetsBox (ISO/IEC 14496-12 §8.7.9).
/// Sits inside `stbl` or `traf`. Provides byte-offset position
/// information for one stream of sample auxiliary information whose
/// per-sample sizes are recorded in the matching `saiz`. Versions 0 / 1
/// select 32-bit / 64-bit offsets. In a `traf` the offsets are
/// relative to the base_data_offset established by `tfhd` (or the
/// `moof` start when `default-base-is-moof` is set, per §8.8.14).
pub const SAIO: [u8; 4] = fourcc("saio");
pub const ELST: [u8; 4] = fourcc("elst");
pub const MDAT: [u8; 4] = fourcc("mdat");
pub const FREE: [u8; 4] = fourcc("free");
pub const SKIP: [u8; 4] = fourcc("skip");
pub const UDTA: [u8; 4] = fourcc("udta");
pub const META: [u8; 4] = fourcc("meta");
pub const ILST: [u8; 4] = fourcc("ilst");
pub const DATA: [u8; 4] = fourcc("data");
/// `pitm` — Primary Item Box (ISO/IEC 14496-12 §8.11.4). Sits inside a
/// `meta` box; quantity zero or one. Names the item that holds (or
/// points at) the primary resource of the meta box's handler.
pub const PITM: [u8; 4] = fourcc("pitm");
/// `iloc` — Item Location Box (ISO/IEC 14496-12 §8.11.3). Sits inside a
/// `meta` box; quantity zero or one. A directory mapping each item ID to
/// the byte extents (offset + length) that hold its data.
pub const ILOC: [u8; 4] = fourcc("iloc");
/// `iinf` — Item Information Box (ISO/IEC 14496-12 §8.11.6). Sits inside
/// a `meta` box; quantity zero or one. Array of `infe` entries naming
/// and typing the items declared by a sibling `iloc`.
pub const IINF: [u8; 4] = fourcc("iinf");
/// `infe` — Item Info Entry (ISO/IEC 14496-12 §8.11.6). One per item
/// inside `iinf`; FullBox carrying the item's ID, type code, and name.
pub const INFE: [u8; 4] = fourcc("infe");
/// `iref` — Item Reference Box (ISO/IEC 14496-12 §8.11.12). Sits inside
/// a `meta` box; quantity zero or one. Collects typed from→to item-ID
/// links (e.g. `dimg` derivation, `thmb` thumbnail, `cdsc` description).
pub const IREF: [u8; 4] = fourcc("iref");
/// `idat` — Item Data Box (ISO/IEC 14496-12 §8.11.11). Sits inside a
/// `meta` box; quantity zero or one. Holds the bytes of items whose
/// `iloc` construction_method is 1 (idat-relative).
pub const IDAT: [u8; 4] = fourcc("idat");
/// `meco` — Additional Metadata Container Box (ISO/IEC 14496-12 §8.11.7).
/// Sits at File / `moov` / `trak` level; quantity zero or one. Holds one
/// or more additional `meta` boxes (each with a distinct handler type)
/// that complement the primary `meta`, plus zero or more `mere` relation
/// boxes. The box body is just a sequence of child boxes.
pub const MECO: [u8; 4] = fourcc("meco");
/// `mere` — Metabox Relation Box (ISO/IEC 14496-12 §8.11.8). Sits inside
/// a `meco`; quantity zero or more. FullBox(version=0) body is two 32-bit
/// handler-type codes plus a 1-byte relation enum (1..=5) describing how
/// the two same-level `meta` boxes relate.
pub const MERE: [u8; 4] = fourcc("mere");
/// `fiin` — FD Item Information Box (ISO/IEC 14496-12 §8.13.2). Sits
/// inside a `meta` box; quantity zero or one. FullBox(version=0) carrying
/// a 16-bit `entry_count` of `paen` PartitionEntry boxes, optionally
/// followed by a `segr` session-group box and a `gitn` group-id-to-name
/// box.
pub const FIIN: [u8; 4] = fourcc("fiin");
/// `paen` — Partition Entry Box (ISO/IEC 14496-12 §8.13.2). Sits inside
/// `fiin`; a plain Box wrapping one mandatory `fpar`, an optional `fecr`,
/// and an optional `fire`.
pub const PAEN: [u8; 4] = fourcc("paen");
/// `fpar` — File Partition Box (ISO/IEC 14496-12 §8.13.3). Sits inside
/// `paen`; mandatory, exactly one. FullBox(version 0/1) describing the
/// FEC partitioning of a source file into source blocks and symbols.
pub const FPAR: [u8; 4] = fourcc("fpar");
/// `fecr` — FEC Reservoir Box (ISO/IEC 14496-12 §8.13.4). Sits inside
/// `paen`; zero or one. FullBox(version 0/1) listing (item_ID,
/// symbol_count) pairs for the FEC reservoirs of each source block.
pub const FECR: [u8; 4] = fourcc("fecr");
/// `segr` — FD Session Group Box (ISO/IEC 14496-12 §8.13.5). Sits inside
/// `fiin`; zero or one. Plain Box listing session groups, each with a set
/// of file-group IDs and a list of FD hint-track IDs.
pub const SEGR: [u8; 4] = fourcc("segr");
/// `gitn` — Group ID to Name Box (ISO/IEC 14496-12 §8.13.6). Sits inside
/// `fiin`; zero or one. FullBox(version=0) mapping file-group IDs to
/// NULL-terminated UTF-8 group names.
pub const GITN: [u8; 4] = fourcc("gitn");
/// `fire` — File Reservoir Box (ISO/IEC 14496-12 §8.13.7). Sits inside
/// `paen`; zero or one. FullBox(version 0/1) listing (item_ID,
/// symbol_count) pairs for the File reservoirs of each source block.
pub const FIRE: [u8; 4] = fourcc("fire");
/// `feci` — FEC Information Box (ISO/IEC 14496-12 §9.2.4.7). Sits inside
/// an `extr` Extra Data Box of an FD hint-track sample; zero or one.
/// Plain Box carrying FEC_encoding_ID, FEC_instance_ID,
/// source_block_number and encoding_symbol_ID for one FD packet.
pub const FECI: [u8; 4] = fourcc("feci");
/// `rtp ` — RTP server hint sample entry (ISO/IEC 14496-12 §9.1.2). The
/// sample-description entry format for an RTP server hint track (media
/// handler `hint`).
pub const RTP_HINT: [u8; 4] = fourcc("rtp ");
/// `srtp` — SRTP hint sample entry (ISO/IEC 14496-12 §9.1.2). Same body
/// as `rtp ` plus a mandatory `srpp` SRTPProcessBox.
pub const SRTP_HINT: [u8; 4] = fourcc("srtp");
/// `rrtp` — RTP reception hint sample entry (ISO/IEC 14496-12 §9.4.1.2).
/// Same body as `rtp `; a distinct FourCC so a reception hint track (which
/// may contain errors) is not mistaken for a valid server hint track.
pub const RRTP_HINT: [u8; 4] = fourcc("rrtp");
/// `tims` — timescale entry (ISO/IEC 14496-12 §9.1.2). Required
/// additional-data box inside an RTP hint sample entry; carries the RTP
/// clock `timescale`.
pub const TIMS: [u8; 4] = fourcc("tims");
/// `tsro` — time offset box (ISO/IEC 14496-12 §9.1.2). Optional
/// additional-data box inside an RTP hint sample entry; a signed offset
/// applied to the RTP timestamp (inferred 0 when absent).
pub const TSRO: [u8; 4] = fourcc("tsro");
/// `snro` — sequence offset box (ISO/IEC 14496-12 §9.1.2). Optional
/// additional-data box inside an RTP hint sample entry; a signed offset
/// applied to the RTP sequence number (inferred 0 when absent).
pub const SNRO: [u8; 4] = fourcc("snro");
/// `srpp` — SRTP Process Box (ISO/IEC 14496-12 §9.1.2.1). Mandatory child
/// of an `srtp` SRTP hint sample entry; FullBox carrying the four SRTP
/// encryption/integrity algorithm identifiers plus a `schm`/`schi` pair.
pub const SRPP: [u8; 4] = fourcc("srpp");
/// `rtcp` — RTCP reception hint sample entry (ISO/IEC 14496-12 §9.4.2.3).
/// Identical in structure to the RTP sample entry (`rtp `) but with no
/// defined additional-data boxes; a distinct FourCC for RTCP reception
/// hint tracks.
pub const RTCP_HINT: [u8; 4] = fourcc("rtcp");
/// `sm2t` — MPEG-2 TS server hint sample entry (ISO/IEC 14496-12
/// §9.3.3.2). Carries the MPEG2TSSampleEntry body
/// (hinttrackversion/highestcompatibleversion + preceding/trailing byte
/// lengths + precomputed-only flag).
pub const SM2T: [u8; 4] = fourcc("sm2t");
/// `rm2t` — MPEG-2 TS reception hint sample entry (ISO/IEC 14496-12
/// §9.3.3.2). Same MPEG2TSSampleEntry body as `sm2t`; a distinct FourCC
/// for reception hint tracks.
pub const RM2T: [u8; 4] = fourcc("rm2t");
/// `tmcd` — QuickTime Timecode sample entry (and the timecode media
/// type). Its `stsd` entry, after the shared 8-byte sample-entry
/// preamble, carries a reserved u32, a 32-bit `flags` field (drop-frame /
/// 24-hour-max / negative-OK / counter), a 32-bit `timescale`, a 32-bit
/// `frame_duration`, an 8-bit `number_of_frames`, and a reserved u8.
pub const TMCD: [u8; 4] = fourcc("tmcd");
/// `load` — QuickTime Track Load Settings Atom. Sits inside `trak`; a
/// plain `Box` whose 16-byte body carries `preload_start_time`,
/// `preload_duration`, `preload_flags` (preload-always `1` /
/// preload-if-enabled `2`), and `default_hints` (double-buffer `0x0020` /
/// high-quality `0x0100`) — how a reader should preload / play the track.
pub const LOAD: [u8; 4] = fourcc("load");
/// `pnot` — QuickTime Preview Atom. A top-level `Box` whose 12-byte body
/// locates the movie's preview (poster) image: `modification_date`, a
/// `version` (0), an `atom_type` (typically `PICT`), and an `atom_index`
/// (typically 1) identifying which atom of that type is the preview.
pub const PNOT: [u8; 4] = fourcc("pnot");
/// `hinf` — Hint Statistics Box (ISO/IEC 14496-12 §9.1.5). A container in
/// a hint track's `udta` holding optional statistic sub-boxes (`trpy` /
/// `nump` / `tpyl` / `maxr` / `dmed` / `payt` / …) summarising the
/// packetised stream a server would generate.
pub const HINF: [u8; 4] = fourcc("hinf");
/// `kind` — Track Kind Box (ISO/IEC 14496-12 §8.10.4). Sits inside a
/// track-level `udta` and labels the track's role with a (schemeURI,
/// value) pair. Both strings are NULL-terminated C strings; `value`
/// may be empty (its terminator still present) when `schemeURI`
/// alone fully identifies the kind. Zero or more per track.
pub const KIND: [u8; 4] = fourcc("kind");
/// `cprt` — Copyright Box (ISO/IEC 14496-12 §8.10.2). Sits inside a
/// `udta` box (either inside `moov` for a file-wide copyright or inside
/// a `trak` for a per-track copyright). FullBox(version=0, flags=0)
/// body is a 1-bit pad + three 5-bit characters packed into 16 bits
/// encoding an ISO 639-2/T language code (each character is the ASCII
/// value minus 0x60), followed by a NULL-terminated UTF-8 string —
/// or, if the first two bytes are the UTF-16 BOM (0xFEFF), a
/// NULL-terminated UTF-16BE string — carrying the copyright notice.
/// Quantity is zero or more: a producer can attach one box per language
/// for multi-lingual notices. Distinct from the 3GPP TS 26.244 `cprt`
/// shape (one of `titl` / `auth` / `cprt` / …) that lumps all the
/// metadata strings under the same FullBox layout — the BMFF box's
/// language and notice are surfaced separately to preserve the typed
/// language code.
pub const CPRT: [u8; 4] = fourcc("cprt");
/// `tsel` — Track Selection Box (ISO/IEC 14496-12 §8.10.3). Sits in a
/// track-level `udta`; quantity zero or one. FullBox(version=0, flags=0)
/// body is `template int(32) switch_group = 0; unsigned int(32)
/// attribute_list[]` to end of box. Tracks sharing the same non-zero
/// `switch_group` are switchable during playback (and must already share
/// the alternate group declared on `tkhd`, §8.3.2). The `attribute_list`
/// is a list of FourCC tags drawn from the spec's descriptive set
/// (`tesc`, `fgsc`, `cgsc`, `spsc`, `resc`, `vwsc`) and differentiating
/// set (`bitr`, `cdec`, `lang`, …) that characterise what the track
/// offers and how it differs from its siblings, so a media-selection
/// layer can pick from the alternate group on language / bitrate /
/// codec criteria.
pub const TSEL: [u8; 4] = fourcc("tsel");
/// `strk` — Sub Track Box (ISO/IEC 14496-12 §8.14.3). A plain `Box`
/// (not a FullBox) that sits inside a track-level `udta`; quantity zero
/// or more — one per sub track defined within the containing track. Its
/// body is a container holding a mandatory `stri` (Sub Track Information,
/// §8.14.4) and a mandatory `strd` (Sub Track Definition, §8.14.5). Sub
/// tracks assign *parts* of a track to the same alternate / switch groups
/// that whole tracks use (§8.3.2 / §8.10.3), so a media-selection layer
/// can pick among layered-codec alternatives (SVC / MVC temporal,
/// spatial, SNR, or view layers) that don't map cleanly onto track
/// boundaries (§8.14.1).
pub const STRK: [u8; 4] = fourcc("strk");
/// `stri` — Sub Track Information Box (ISO/IEC 14496-12 §8.14.4). A
/// `FullBox(version = 0, flags = 0)` inside `strk`; mandatory, quantity
/// one. Body: `template int(16) switch_group; template int(16)
/// alternate_group; template unsigned int(32) sub_track_ID; unsigned
/// int(32) attribute_list[]` (to end of box). The two group fields use
/// the same global numbering as the track-level `tkhd.alternate_group`
/// (§8.3.2) and `tsel.switch_group` (§8.10.3) so groups can span track
/// and sub-track boundaries; `attribute_list` reuses §8.10.3.5's
/// descriptive / differentiating FourCC vocabulary (`tesc`, `fgsc`,
/// `cgsc`, `spsc`, `resc`, `vwsc`, `bitr`, `frar`, `nvws`, …).
pub const STRI: [u8; 4] = fourcc("stri");
/// `strd` — Sub Track Definition Box (ISO/IEC 14496-12 §8.14.5). A
/// plain `Box` inside `strk`; mandatory, quantity one. Holds the objects
/// that *define* (rather than describe) the sub track — for the generic
/// (non-codec-specific) mechanism that is zero or more `stsg` Sub Track
/// Sample Group boxes (§8.14.6).
pub const STRD: [u8; 4] = fourcc("strd");
/// `stsg` — Sub Track Sample Group Box (ISO/IEC 14496-12 §8.14.6). A
/// `FullBox(version = 0, flags = 0)` inside `strd`; quantity zero or
/// more. Body: `unsigned int(32) grouping_type; unsigned int(16)
/// item_count; unsigned int(32) group_description_index[item_count]`. It
/// defines the sub track as the union of one or more sample groups by
/// naming the `sgpd` (§8.9.3) description indices that describe the
/// samples belonging to the sub track for the shared `grouping_type`.
pub const STSG: [u8; 4] = fourcc("stsg");

// Fragmented-MP4 box types (ISO/IEC 14496-12 §8.8 — Movie Fragments).
pub const MVEX: [u8; 4] = fourcc("mvex");
/// `mehd` — MovieExtendsHeaderBox (ISO/IEC 14496-12 §8.8.2). Sits inside
/// `mvex`; optional FullBox carrying the overall presentation duration
/// of a fragmented movie, including fragments, in the movie timescale.
/// Version 0 stores the duration as u32, version 1 as u64. Per §8.8.2.3:
/// "if an MP4 file is created in real-time, such as used in live
/// streaming, it is not likely that the fragment_duration is known in
/// advance and this box may be omitted." When the box is present, it
/// supplies a duration the demuxer can report even when `mvhd.duration`
/// is zero (the typical sealed-fragmented-file pattern: a final
/// authoring step writes `mehd` after the fragments are laid down).
pub const MEHD: [u8; 4] = fourcc("mehd");
pub const TREX: [u8; 4] = fourcc("trex");
/// `leva` — LevelAssignmentBox (ISO/IEC 14496-12 §8.8.13). Sits inside
/// `mvex`; optional FullBox(version=0, flags=0) with quantity zero or
/// one per file. Maps tracks (or sample groups, parameterized sample
/// groups, sub-tracks) to "levels" inside subsequent movie fragments;
/// samples mapped to level n may depend on samples of any level m ≤ n
/// but never on samples of level p > n (§8.8.13.1 "Levels specify
/// subsets of the file"). Levels cannot be specified for the initial
/// movie — the box applies to all moof fragments that follow. The DASH
/// (ISO/IEC 23009-1) usage indexes each subsegment named by a `sidx`
/// (§8.16.3) as a "fraction"; within a fraction, data for each level
/// shall appear contiguously and in increasing order of level value.
/// Demuxer carries the parsed entries through `leva_entries()`; the
/// flat metadata channel surfaces `leva_count` + `leva_<n>` summaries.
pub const LEVA: [u8; 4] = fourcc("leva");
/// `trep` — TrackExtensionPropertiesBox (ISO/IEC 14496-12 §8.8.15). A
/// FullBox(version=0, flags=0) inside `mvex` that documents or
/// summarises characteristics of one track (named by its `track_id`
/// field) in the subsequent movie fragments. It may contain any number
/// of child boxes (e.g. `assp`, the Alternative Startup Sequence
/// Properties Box, §8.8.16). Quantity is zero or more per `mvex`, with
/// zero or one per track.
pub const TREP: [u8; 4] = fourcc("trep");
pub const MOOF: [u8; 4] = fourcc("moof");
pub const MFHD: [u8; 4] = fourcc("mfhd");
pub const TRAF: [u8; 4] = fourcc("traf");
pub const TFHD: [u8; 4] = fourcc("tfhd");
pub const TFDT: [u8; 4] = fourcc("tfdt");
pub const TRUN: [u8; 4] = fourcc("trun");
pub const SIDX: [u8; 4] = fourcc("sidx");
/// `ssix` — SubsegmentIndexBox (ISO/IEC 14496-12 §8.16.4). A top-level
/// FullBox(version=0, flags=0) mapping levels (as assigned by the
/// `leva` LevelAssignmentBox, §8.8.13) to byte ranges of the indexed
/// subsegment, so a client can fetch partial subsegments by byte
/// range. Spec placement: zero or one per `sidx` that indexes only
/// leaf subsegments, immediately following the associated `sidx`;
/// `subsegment_count` shall equal that sidx's `reference_count`.
pub const SSIX: [u8; 4] = fourcc("ssix");
pub const STYP: [u8; 4] = fourcc("styp");
/// `prft` — ProducerReferenceTimeBox (ISO/IEC 14496-12 §8.16.5). A
/// top-level FullBox (file scope) that supplies a wall-clock time
/// (NTP-format) correlated with a media time for one track. Used by
/// low-latency DASH/CMAF live streaming so a consumer can match
/// production wall-clock against media presentation time. Spec
/// placement: must follow `styp`/`sidx` (if any) and precede the
/// `moof` it refers to.
pub const PRFT: [u8; 4] = fourcc("prft");
/// `emsg` — DASH Event Message Box (ISO/IEC 23009-1 §5.10.3.3). A
/// top-level FullBox carried in a media segment before the first
/// `moof` of the segment it applies to; zero or more per segment, each
/// carrying one timed in-band event (`scheme_id_uri` + `value` +
/// timing triple + opaque `message_data`). Version 0 leads with the
/// two null-terminated strings and uses a 32-bit segment-relative
/// `presentation_time_delta`; version 1 leads with the integers and
/// uses a 64-bit absolute `presentation_time`. Parsed via
/// [`crate::emsg::parse_emsg_box`]; the demuxer captures instances
/// during the top-level walk (keyed by the index of the following
/// `moof`) and the fragmented muxer emits queued instances ahead of
/// the next fragment.
pub const EMSG: [u8; 4] = fourcc("emsg");
// Random-access boxes (§8.8.10–12 + §8.16.3).
pub const MFRA: [u8; 4] = fourcc("mfra");
pub const TFRA: [u8; 4] = fourcc("tfra");
pub const MFRO: [u8; 4] = fourcc("mfro");

// Handler types.
pub const HANDLER_SOUN: [u8; 4] = fourcc("soun");
pub const HANDLER_VIDE: [u8; 4] = fourcc("vide");
/// `subt` — Subtitle media (ISO/IEC 14496-12 §12.6.1).
pub const HANDLER_SUBT: [u8; 4] = fourcc("subt");
/// `text` — Timed text media (ISO/IEC 14496-12 §12.5.1). Also used by
/// the QuickTime / 3GPP `tx3g` carriage.
pub const HANDLER_TEXT: [u8; 4] = fourcc("text");
/// `sbtl` — QuickTime subtitle handler (legacy variant; common in
/// `.mov` files muxed by Apple tools alongside the spec `subt`).
pub const HANDLER_SBTL: [u8; 4] = fourcc("sbtl");
/// `meta` — Timed metadata handler (ISO/IEC 14496-12 §8.11).
pub const HANDLER_META: [u8; 4] = fourcc("meta");

// Protection scheme boxes (ISO/IEC 14496-12 §8.12). Files containing
// CENC-encrypted media rewrite the sample-entry FourCC to one of the
// `enc*` placeholders below and bury the original FourCC plus the
// scheme parameters inside a `sinf` container.
//
// We recognise `enc*` and unwrap to the original FourCC via `sinf/frma`
// so callers see the right codec id. The actual key-management
// surface (`tenc`, `pssh`, `senc`, `saiz`/`saio` payloads) is
// scheme-specific and lives in ISO/IEC 23001-7, which is partially
// covered in `docs/container/cenc/`; full CENC decryption is a
// separate slice.
pub const SINF: [u8; 4] = fourcc("sinf");
pub const FRMA: [u8; 4] = fourcc("frma");
pub const SCHM: [u8; 4] = fourcc("schm");
pub const SCHI: [u8; 4] = fourcc("schi");
pub const ENCV: [u8; 4] = fourcc("encv");
pub const ENCA: [u8; 4] = fourcc("enca");
pub const ENCT: [u8; 4] = fourcc("enct");
pub const ENCS: [u8; 4] = fourcc("encs");

// `stvi` — StereoVideoBox (ISO/IEC 14496-12 §8.15.4.2). A `FullBox`
// that sits inside the `schi` (SchemeInformationBox) of a sample
// entry's `sinf` when the SchemeType is `stvi` (stereoscopic video,
// §8.15.4.1). Indicates that decoded frames carry either two spatially
// packed constituent frames (frame packing) or one of two views of a
// stereo pair (left / right in different tracks). Parsed structurally
// via `crate::demux`; this bare FourCC lets the sinf/schi walker
// dispatch on it.
pub const STVI: [u8; 4] = fourcc("stvi");

// CENC boxes (ISO/IEC 23001-7). Parsed via the structured types in
// `crate::cenc`; the bare FourCC constants here let the demux walker
// dispatch on them.
//
// * `tenc` — TrackEncryptionBox (§8.2); per-track defaults, sits inside
//   `schi` inside `sinf`.
// * `pssh` — ProtectionSystemSpecificHeaderBox (§8.1); a DRM-system
//   opaque header, sits at moov level (and optionally moof level).
// * `senc` — SampleEncryptionBox (§7.2); per-sample IVs and an optional
//   subsample map, sits in `traf` (or `trak`).
pub const TENC: [u8; 4] = fourcc("tenc");
pub const PSSH: [u8; 4] = fourcc("pssh");
pub const SENC: [u8; 4] = fourcc("senc");

/// `uuid` — the reserved extended-type box (ISO/IEC 14496-12 §4.2).
/// When `type == 'uuid'`, a 16-byte `usertype` (a full UUID, RFC 4122
/// network/big-endian byte order) immediately follows the standard box
/// header and precedes the body, letting a vendor define extension
/// boxes without registering a FourCC. This crate recognises the three
/// legacy PIFF (Protected Interoperable File Format) encryption
/// usertypes — the pre-CENC predecessors of `senc` / `tenc` / `pssh` —
/// via the constants and parsers in [`crate::cenc`]; any other
/// usertype is skipped as an unknown box.
pub const UUID: [u8; 4] = fourcc("uuid");

// ---------------------------------------------------------------------------
// HEIF / MIAF item-properties family (ISO/IEC 23008-12 §9.3, referenced from
// ISO/IEC 14496-12). The `iprp` ItemPropertiesBox lives inside a `meta` box
// and associates items (declared by a sibling `iloc` / `iinf`) with an
// ordered set of small property records held in `ipco`, mapped per item by
// one or more `ipma` association boxes.
// ---------------------------------------------------------------------------

/// `iprp` — Item Properties Box (ISO/IEC 23008-12 §9.3.1). Sits inside a
/// `meta` box; quantity zero or one. Holds exactly one `ipco`
/// ItemPropertyContainerBox followed by one or more `ipma`
/// ItemPropertyAssociation boxes.
pub const IPRP: [u8; 4] = fourcc("iprp");
/// `ipco` — Item Property Container Box (ISO/IEC 23008-12 §9.3.1). Sits
/// inside `iprp`; quantity exactly one. An implicitly-indexed (1-based)
/// list of property boxes, each a `Box` or `FullBox`.
pub const IPCO: [u8; 4] = fourcc("ipco");
/// `ipma` — Item Property Association Box (ISO/IEC 23008-12 §9.3.1). Sits
/// inside `iprp`; quantity one or more. FullBox mapping each item ID to a
/// list of `(essential, property_index)` pairs.
pub const IPMA: [u8; 4] = fourcc("ipma");
/// `ispe` — Image Spatial Extents Property (ISO/IEC 23008-12 §6.5.3).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) carrying
/// the reconstructed image `(image_width, image_height)`.
pub const ISPE: [u8; 4] = fourcc("ispe");
/// `pixi` — Pixel Information Property (ISO/IEC 23008-12 §6.5.6).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) listing
/// the per-channel bit depth of the reconstructed image.
pub const PIXI: [u8; 4] = fourcc("pixi");
/// `rloc` — Relative Location Property (ISO/IEC 23008-12 §6.5.7).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) giving an
/// item's `(horizontal_offset, vertical_offset)` within its `tbas` base.
pub const RLOC: [u8; 4] = fourcc("rloc");
/// `auxC` — Auxiliary Type Property (ISO/IEC 23008-12 §6.5.8). Descriptive
/// `ipco` child; ItemFullProperty(version=0, flags) carrying a
/// NULL-terminated URN `aux_type` plus type-specific `aux_subtype` bytes.
pub const AUXC: [u8; 4] = fourcc("auxC");
/// `irot` — Image Rotation Property (ISO/IEC 23008-12 §6.5.10).
/// Transformative `ipco` child; ItemProperty (plain Box) whose low 2 bits
/// are an anti-clockwise rotation `angle` (`angle * 90` degrees).
pub const IROT: [u8; 4] = fourcc("irot");
/// `imir` — Image Mirroring Property (ISO/IEC 23008-12 §6.5.12).
/// Transformative `ipco` child; ItemProperty (plain Box) whose low bit is a
/// mirror `axis` (0 = vertical axis, 1 = horizontal axis).
pub const IMIR: [u8; 4] = fourcc("imir");
/// `lsel` — Layer Selector Property (ISO/IEC 23008-12 §6.5.11).
/// Descriptive `ipco` child; ItemProperty (plain Box) carrying a 16-bit
/// `layer_id` selecting one reconstructed image of a multi-layer item.
pub const LSEL: [u8; 4] = fourcc("lsel");
/// `grpl` — Groups List Box (ISO/IEC 23008-12 §9.4.2). Sits inside a
/// `meta` box (not one in an `meco`); quantity zero or one. A plain `Box`
/// holding a set of `EntityToGroupBox`es, each a FullBox whose FourCC is
/// the `grouping_type` (`altr`, `ster`, …) and whose body lists the item /
/// track IDs that share the grouping characteristic.
pub const GRPL: [u8; 4] = fourcc("grpl");
/// `udes` — User Description Property (ISO/IEC 23008-12 §6.5.20).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) carrying
/// four NUL-terminated UTF-8 strings: `lang` (RFC 5646), `name`,
/// `description`, and comma-separated `tags`.
pub const UDES: [u8; 4] = fourcc("udes");
/// `altt` — Accessibility Text Property (ISO/IEC 23008-12 §6.5.21).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) carrying
/// the NUL-terminated UTF-8 `alt_text` plus an `alt_lang` (RFC 5646).
pub const ALTT: [u8; 4] = fourcc("altt");
/// `iscl` — Image Scaling Property (ISO/IEC 23008-12 §6.5.13).
/// Transformative `ipco` child; ItemFullProperty(version=0, flags=0)
/// carrying the horizontal / vertical scaling ratios as 16-bit
/// numerator/denominator pairs.
pub const ISCL: [u8; 4] = fourcc("iscl");
/// `rref` — Required Reference Types Property (ISO/IEC 23008-12 §6.5.17).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) listing
/// the `iref` reference types a reader must understand to decode the item.
pub const RREF: [u8; 4] = fourcc("rref");
/// `crtt` — Creation Time Property (ISO/IEC 23008-12 §6.5.18). Descriptive
/// `ipco` child; ItemFullProperty(version=0, flags=0) carrying a 64-bit
/// `creation_time` (microseconds since 1904-01-01 UTC).
pub const CRTT: [u8; 4] = fourcc("crtt");
/// `mdft` — Modification Time Property (ISO/IEC 23008-12 §6.5.19).
/// Descriptive `ipco` child; ItemFullProperty(version=0, flags=0) carrying
/// a 64-bit `modification_time` (microseconds since 1904-01-01 UTC).
pub const MDFT: [u8; 4] = fourcc("mdft");

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn box_size_below_eight_is_rejected_not_underflow() {
        // ISO/IEC 14496-12 §4.2: `size` is the total box length
        // including the 8-byte header. A non-sentinel value < 8
        // would imply a negative body length and used to overflow
        // `payload_size = total_size - header_len`. Every value
        // from 2..=7 must return Err rather than panic.
        for bad in 2u32..=7 {
            let mut buf = Vec::with_capacity(8);
            buf.extend_from_slice(&bad.to_be_bytes());
            buf.extend_from_slice(b"junk");
            let err = read_box_header(&mut Cursor::new(buf)).expect_err("size < 8 must be invalid");
            assert!(format!("{err}").contains("MP4"), "{err}");
        }
    }

    #[test]
    fn box_largesize_below_sixteen_is_rejected_not_underflow() {
        // `size32 == 1` means a 64-bit largesize follows; the
        // total length must then be at least the 16-byte header
        // (size32 + fourcc + largesize). A largesize of 0..=15
        // would underflow `payload_size`.
        for bad in 0u64..=15 {
            let mut buf = Vec::with_capacity(16);
            buf.extend_from_slice(&1u32.to_be_bytes());
            buf.extend_from_slice(b"junk");
            buf.extend_from_slice(&bad.to_be_bytes());
            let err =
                read_box_header(&mut Cursor::new(buf)).expect_err("largesize < 16 must be invalid");
            assert!(format!("{err}").contains("MP4"), "{err}");
        }
    }

    #[test]
    fn box_largesize_overflowing_u64_from_nonzero_start_is_rejected() {
        // Companion to round 187 in `oxideav-mov`: an 8-byte placeholder
        // box at offset 0 followed by a `size=1 largesize=u64::MAX`
        // box at offset 8 used to overflow every downstream
        // `body_start + payload_size` computation (the §8.16.3 `sidx`
        // body-end anchor in `demux.rs` line 53 is the closest
        // analogue to mov's `body_end` arithmetic). At `start = 8`
        // and `total_size = u64::MAX`, `start + total_size = u64::MAX + 8`
        // — debug builds panic with "attempt to add with overflow";
        // release builds silently wrap. The header-level
        // `checked_add` guard rejects the box before any caller
        // touches the arithmetic.
        let mut buf = Vec::new();
        // Box #1: size=8, fourcc=free. Pushes the next box's start to 8.
        buf.extend_from_slice(&8u32.to_be_bytes());
        buf.extend_from_slice(b"free");
        // Box #2: size=1 (extended), largesize=u64::MAX. Anchored at
        // offset 8, so `start + total_size` overflows.
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&u64::MAX.to_be_bytes());

        let mut cur = Cursor::new(buf);
        // First box must parse cleanly: start=0 + total_size=8 fits.
        let h1 = read_box_header(&mut cur)
            .expect("first box parses")
            .expect("first box present");
        assert_eq!(h1.total_size, Some(8));
        // Second box: u64-overflow must surface as Err, not panic / wrap.
        let err =
            read_box_header(&mut cur).expect_err("u64 overflow must be rejected at header read");
        let msg = format!("{err}");
        assert!(
            msg.contains("overflow") && msg.contains("mdat"),
            "expected u64-overflow rejection naming the box, got: {msg}"
        );
    }

    #[test]
    fn box_largesize_one_below_overflow_is_accepted() {
        // Boundary case: `start + largesize == u64::MAX` is still
        // representable, so `checked_add` returns `Some(_)` and the
        // header is accepted. Drive `read_box_header` directly — the
        // body would extend past the 16-byte cursor but the framing
        // itself must be returned to the caller intact.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut cur = Cursor::new(buf);
        let hdr = read_box_header(&mut cur)
            .expect("header at start=0 with largesize=u64::MAX does not overflow")
            .expect("a 16-byte header is present");
        assert_eq!(hdr.fourcc, *b"mdat");
        assert_eq!(hdr.total_size, Some(u64::MAX));
        assert_eq!(hdr.header_len, 16);
    }

    #[test]
    fn box_size_eight_is_a_valid_empty_box() {
        // The minimum legal non-sentinel size — header only, zero
        // body — must still parse cleanly and report a payload of
        // zero.
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(&8u32.to_be_bytes());
        buf.extend_from_slice(b"free");
        let hdr = read_box_header(&mut Cursor::new(buf))
            .unwrap()
            .expect("size = 8 must parse");
        assert_eq!(hdr.total_size, Some(8));
        assert_eq!(hdr.header_len, 8);
        assert_eq!(hdr.payload_size(), Some(0));
        assert_eq!(&hdr.fourcc, b"free");
    }
}
