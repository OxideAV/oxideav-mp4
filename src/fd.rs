//! FD (File Delivery) Item Information family — ISO/IEC 14496-12 §8.13.
//!
//! Files intended for transmission over ALC/LCT or FLUTE are stored as
//! items in a top-level `meta` box (§8.13.1). The partitioning of those
//! source files into source blocks and FEC/File reservoirs is recorded by
//! the **FD Item Information Box** (`fiin`, §8.13.2) and its descendants:
//!
//! ```text
//! fiin  FDItemInformationBox   (FullBox v0)   — meta/
//!   paen  PartitionEntry       (Box)          — one per partitioning
//!     fpar  FilePartitionBox   (FullBox v0/1) — mandatory, exactly one
//!     fecr  FECReservoirBox    (FullBox v0/1) — optional
//!     fire  FileReservoirBox   (FullBox v0/1) — optional
//!   segr  FDSessionGroupBox    (Box)          — optional
//!   gitn  GroupIdToNameBox     (FullBox v0)   — optional
//! ```
//!
//! Plus the **FEC Information Box** (`feci`, §9.2.4.7) carried inside an
//! FD hint-track sample's `extr` Extra Data Box.
//!
//! Every parser here is the byte-exact inverse of its builder: a record
//! produced by `parse_*` re-serialises identically through `build_*`.
//! All integers are big-endian (§7).

use crate::boxes::*;

// ---------------------------------------------------------------------------
// Small read helpers (local to this module — the meta-item helpers in
// demux.rs operate on the {0,4,8} width set; the FD family uses fixed
// 8/16/24/32-bit reads plus a NULL-terminated string).
// ---------------------------------------------------------------------------

fn rd_u8(buf: &[u8], pos: &mut usize) -> Option<u8> {
    let v = *buf.get(*pos)?;
    *pos += 1;
    Some(v)
}

fn rd_u16(buf: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos + 2 > buf.len() {
        return None;
    }
    let v = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]);
    *pos += 2;
    Some(v)
}

fn rd_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Some(v)
}

/// Read a NULL-terminated UTF-8 string starting at `pos` (the §8.13 `string`
/// type). Advances `pos` past the terminator. Returns `None` if no
/// terminator is found before the end of the buffer (a malformed string).
fn rd_cstr(buf: &[u8], pos: &mut usize) -> Option<String> {
    let start = *pos;
    let rel = buf[start..].iter().position(|&b| b == 0)?;
    let s = String::from_utf8_lossy(&buf[start..start + rel]).into_owned();
    *pos = start + rel + 1;
    Some(s)
}

/// `item_ID` / `entry_count` width selector. §8.13.3/4/7 widen these fields
/// from 16 to 32 bits when `version == 1`.
fn rd_id(buf: &[u8], pos: &mut usize, version: u8) -> Option<u32> {
    if version == 0 {
        rd_u16(buf, pos).map(u32::from)
    } else {
        rd_u32(buf, pos)
    }
}

fn wr_id(out: &mut Vec<u8>, value: u32, version: u8) {
    if version == 0 {
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else {
        out.extend_from_slice(&value.to_be_bytes());
    }
}

/// Read the FullBox preamble (version byte + 24-bit flags). Returns
/// `(version, flags)` and advances `pos` by 4. `None` on truncation.
fn rd_fullbox(buf: &[u8], pos: &mut usize) -> Option<(u8, u32)> {
    if *pos + 4 > buf.len() {
        return None;
    }
    let version = buf[*pos];
    let flags = u32::from_be_bytes([0, buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Some((version, flags))
}

fn wr_fullbox(out: &mut Vec<u8>, version: u8, flags: u32) {
    out.push(version);
    out.extend_from_slice(&flags.to_be_bytes()[1..]);
}

/// Wrap `body` in `[size:u32][fourcc]` (32-bit size form, §4.2). The FD
/// boxes are small enough that the largesize form is never needed.
fn wrap(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let total = (8 + body.len()) as u32;
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&total.to_be_bytes());
    v.extend_from_slice(fourcc);
    v.extend_from_slice(body);
    v
}

/// Iterate the child boxes of a container body, yielding
/// `(fourcc, child_payload)` for each. Stops at the first malformed /
/// truncated header (matching the lenient walk in `parse_meta_items`).
fn each_child<F: FnMut([u8; 4], &[u8])>(body: &[u8], mut f: F) {
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let size32 = u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        let mut fourcc = [0u8; 4];
        fourcc.copy_from_slice(&body[pos + 4..pos + 8]);
        let (total, hdr_len) = if size32 == 1 {
            if pos + 16 > body.len() {
                break;
            }
            let large = u64::from_be_bytes([
                body[pos + 8],
                body[pos + 9],
                body[pos + 10],
                body[pos + 11],
                body[pos + 12],
                body[pos + 13],
                body[pos + 14],
                body[pos + 15],
            ]);
            (large as usize, 16usize)
        } else if size32 == 0 {
            (body.len() - pos, 8usize)
        } else {
            (size32 as usize, 8usize)
        };
        if total < hdr_len || pos + total > body.len() {
            break;
        }
        f(fourcc, &body[pos + hdr_len..pos + total]);
        pos += total;
    }
}

// ===========================================================================
// fpar — File Partition Box (§8.13.3)
// ===========================================================================

/// One `(block_count, block_size)` partitioning entry of an `fpar`
/// (§8.13.3.3): `block_count` consecutive source blocks of `block_size`
/// bytes each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FparEntry {
    /// Number of consecutive source blocks of size `block_size`.
    pub block_count: u16,
    /// Size of a block in bytes.
    pub block_size: u32,
}

/// Decoded `fpar` File Partition Box (ISO/IEC 14496-12 §8.13.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FparBox {
    /// FullBox version (0 ⇒ 16-bit `item_ID`/`entry_count`; 1 ⇒ 32-bit).
    pub version: u8,
    /// References the `iloc` item the partitioning applies to.
    pub item_id: u32,
    /// Target ALC/LCT or FLUTE packet payload size.
    pub packet_payload_size: u16,
    /// FEC encoding scheme ID (RFC 5052 registration).
    pub fec_encoding_id: u8,
    /// More specific FEC encoder identification (under-specified schemes).
    pub fec_instance_id: u16,
    /// Maximum number of source symbols per source block.
    pub max_source_block_length: u16,
    /// Size of one encoding symbol in bytes.
    pub encoding_symbol_length: u16,
    /// Maximum number of encoding symbols generated per source block.
    pub max_number_of_encoding_symbols: u16,
    /// Base64-encoded FEC-OTI scheme-specific info (NULL-terminated on the
    /// wire; the terminator is not part of the stored string).
    pub scheme_specific_info: String,
    /// `(block_count, block_size)` partitioning of the source file.
    pub entries: Vec<FparEntry>,
}

/// Parse an `fpar` body (the bytes after the `[size][fpar]` header).
/// `None` on any truncation or malformed string.
pub fn parse_fpar_box(body: &[u8]) -> Option<FparBox> {
    let mut p = 0usize;
    let (version, _flags) = rd_fullbox(body, &mut p)?;
    let item_id = rd_id(body, &mut p, version)?;
    let packet_payload_size = rd_u16(body, &mut p)?;
    let _reserved = rd_u8(body, &mut p)?;
    let fec_encoding_id = rd_u8(body, &mut p)?;
    let fec_instance_id = rd_u16(body, &mut p)?;
    let max_source_block_length = rd_u16(body, &mut p)?;
    let encoding_symbol_length = rd_u16(body, &mut p)?;
    let max_number_of_encoding_symbols = rd_u16(body, &mut p)?;
    let scheme_specific_info = rd_cstr(body, &mut p)?;
    let entry_count = rd_id(body, &mut p, version)? as usize;
    let mut entries = Vec::with_capacity(entry_count.min(body.len()));
    for _ in 0..entry_count {
        let block_count = rd_u16(body, &mut p)?;
        let block_size = rd_u32(body, &mut p)?;
        entries.push(FparEntry {
            block_count,
            block_size,
        });
    }
    Some(FparBox {
        version,
        item_id,
        packet_payload_size,
        fec_encoding_id,
        fec_instance_id,
        max_source_block_length,
        encoding_symbol_length,
        max_number_of_encoding_symbols,
        scheme_specific_info,
        entries,
    })
}

/// Serialise an `fpar` box (complete `[size][fpar]...`). The byte-exact
/// inverse of [`parse_fpar_box`].
pub fn build_fpar_box(b: &FparBox) -> Vec<u8> {
    let mut body = Vec::new();
    wr_fullbox(&mut body, b.version, 0);
    wr_id(&mut body, b.item_id, b.version);
    body.extend_from_slice(&b.packet_payload_size.to_be_bytes());
    body.push(0); // reserved
    body.push(b.fec_encoding_id);
    body.extend_from_slice(&b.fec_instance_id.to_be_bytes());
    body.extend_from_slice(&b.max_source_block_length.to_be_bytes());
    body.extend_from_slice(&b.encoding_symbol_length.to_be_bytes());
    body.extend_from_slice(&b.max_number_of_encoding_symbols.to_be_bytes());
    body.extend_from_slice(b.scheme_specific_info.as_bytes());
    body.push(0); // string terminator
    wr_id(&mut body, b.entries.len() as u32, b.version);
    for e in &b.entries {
        body.extend_from_slice(&e.block_count.to_be_bytes());
        body.extend_from_slice(&e.block_size.to_be_bytes());
    }
    wrap(&FPAR, &body)
}

// ===========================================================================
// fecr / fire — FEC Reservoir / File Reservoir Box (§8.13.4 / §8.13.7)
// ===========================================================================

/// One `(item_ID, symbol_count)` reservoir entry shared by `fecr` (§8.13.4)
/// and `fire` (§8.13.7) — both have identical wire layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservoirEntry {
    /// Location of the reservoir item associated with a source block.
    pub item_id: u32,
    /// Number of (repair / file) symbols contained in the reservoir.
    pub symbol_count: u32,
}

/// Decoded `fecr` (FEC Reservoir, §8.13.4) or `fire` (File Reservoir,
/// §8.13.7) box. Both carry a versioned `(item_ID, symbol_count)` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservoirBox {
    /// FullBox version (0 ⇒ 16-bit `item_ID`/`entry_count`; 1 ⇒ 32-bit).
    pub version: u8,
    /// Reservoir entries, one per source block.
    pub entries: Vec<ReservoirEntry>,
}

/// Parse a reservoir body (`fecr` or `fire`). `None` on truncation.
pub fn parse_reservoir_box(body: &[u8]) -> Option<ReservoirBox> {
    let mut p = 0usize;
    let (version, _flags) = rd_fullbox(body, &mut p)?;
    let entry_count = rd_id(body, &mut p, version)? as usize;
    let mut entries = Vec::with_capacity(entry_count.min(body.len()));
    for _ in 0..entry_count {
        let item_id = rd_id(body, &mut p, version)?;
        let symbol_count = rd_u32(body, &mut p)?;
        entries.push(ReservoirEntry {
            item_id,
            symbol_count,
        });
    }
    Some(ReservoirBox { version, entries })
}

/// Serialise a reservoir box with the supplied `fourcc` (`fecr` or
/// `fire`). The byte-exact inverse of [`parse_reservoir_box`].
pub fn build_reservoir_box(fourcc: &[u8; 4], b: &ReservoirBox) -> Vec<u8> {
    let mut body = Vec::new();
    wr_fullbox(&mut body, b.version, 0);
    wr_id(&mut body, b.entries.len() as u32, b.version);
    for e in &b.entries {
        wr_id(&mut body, e.item_id, b.version);
        body.extend_from_slice(&e.symbol_count.to_be_bytes());
    }
    wrap(fourcc, &body)
}

// ===========================================================================
// paen — Partition Entry Box (§8.13.2)
// ===========================================================================

/// Decoded `paen` PartitionEntry (ISO/IEC 14496-12 §8.13.2): one mandatory
/// `fpar`, an optional `fecr`, and an optional `fire`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionEntry {
    /// `fpar` File Partition Box (mandatory; `None` only for a malformed
    /// entry whose `fpar` was missing or unparseable).
    pub fpar: Option<FparBox>,
    /// `fecr` FEC Reservoir Box (optional).
    pub fecr: Option<ReservoirBox>,
    /// `fire` File Reservoir Box (optional).
    pub fire: Option<ReservoirBox>,
}

/// Parse a `paen` body, dispatching its children.
pub fn parse_paen_box(body: &[u8]) -> PartitionEntry {
    let mut out = PartitionEntry {
        fpar: None,
        fecr: None,
        fire: None,
    };
    each_child(body, |fourcc, child| match fourcc {
        FPAR => out.fpar = parse_fpar_box(child),
        FECR => out.fecr = parse_reservoir_box(child),
        FIRE => out.fire = parse_reservoir_box(child),
        _ => {}
    });
    out
}

/// Serialise a `paen` box. Emits `fpar` then (if present) `fecr` then
/// `fire`, matching the §8.13.2.2 declaration order.
pub fn build_paen_box(e: &PartitionEntry) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(fpar) = &e.fpar {
        body.extend_from_slice(&build_fpar_box(fpar));
    }
    if let Some(fecr) = &e.fecr {
        body.extend_from_slice(&build_reservoir_box(&FECR, fecr));
    }
    if let Some(fire) = &e.fire {
        body.extend_from_slice(&build_reservoir_box(&FIRE, fire));
    }
    wrap(&PAEN, &body)
}

// ===========================================================================
// segr — FD Session Group Box (§8.13.5)
// ===========================================================================

/// One session group of a `segr` (§8.13.5.3): the file groups it complies
/// with plus the FD hint tracks (channels) it spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGroup {
    /// File-group IDs the session group complies with.
    pub group_ids: Vec<u32>,
    /// Track IDs of the FD hint tracks (channels) in this group. The
    /// first is the base channel.
    pub hint_track_ids: Vec<u32>,
}

/// Decoded `segr` FD Session Group Box (ISO/IEC 14496-12 §8.13.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegrBox {
    /// The session groups, in declaration order.
    pub session_groups: Vec<SessionGroup>,
}

/// Parse a `segr` body. `None` on truncation.
pub fn parse_segr_box(body: &[u8]) -> Option<SegrBox> {
    let mut p = 0usize;
    let num = rd_u16(body, &mut p)? as usize;
    let mut session_groups = Vec::with_capacity(num.min(body.len()));
    for _ in 0..num {
        let entry_count = rd_u8(body, &mut p)? as usize;
        let mut group_ids = Vec::with_capacity(entry_count.min(body.len()));
        for _ in 0..entry_count {
            group_ids.push(rd_u32(body, &mut p)?);
        }
        let num_channels = rd_u16(body, &mut p)? as usize;
        let mut hint_track_ids = Vec::with_capacity(num_channels.min(body.len()));
        for _ in 0..num_channels {
            hint_track_ids.push(rd_u32(body, &mut p)?);
        }
        session_groups.push(SessionGroup {
            group_ids,
            hint_track_ids,
        });
    }
    Some(SegrBox { session_groups })
}

/// Serialise a `segr` box. The byte-exact inverse of [`parse_segr_box`].
pub fn build_segr_box(b: &SegrBox) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(b.session_groups.len() as u16).to_be_bytes());
    for g in &b.session_groups {
        body.push(g.group_ids.len() as u8);
        for id in &g.group_ids {
            body.extend_from_slice(&id.to_be_bytes());
        }
        body.extend_from_slice(&(g.hint_track_ids.len() as u16).to_be_bytes());
        for id in &g.hint_track_ids {
            body.extend_from_slice(&id.to_be_bytes());
        }
    }
    wrap(&SEGR, &body)
}

// ===========================================================================
// gitn — Group ID to Name Box (§8.13.6)
// ===========================================================================

/// One `(group_ID, group_name)` mapping of a `gitn` (§8.13.6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupName {
    /// File-group ID.
    pub group_id: u32,
    /// NULL-terminated UTF-8 group name (terminator not stored).
    pub group_name: String,
}

/// Decoded `gitn` Group ID to Name Box (ISO/IEC 14496-12 §8.13.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitnBox {
    /// The `(group_ID, group_name)` mappings, in declaration order.
    pub entries: Vec<GroupName>,
}

/// Parse a `gitn` body. `None` on truncation or a malformed name string.
pub fn parse_gitn_box(body: &[u8]) -> Option<GitnBox> {
    let mut p = 0usize;
    let (_version, _flags) = rd_fullbox(body, &mut p)?;
    let entry_count = rd_u16(body, &mut p)? as usize;
    let mut entries = Vec::with_capacity(entry_count.min(body.len()));
    for _ in 0..entry_count {
        let group_id = rd_u32(body, &mut p)?;
        let group_name = rd_cstr(body, &mut p)?;
        entries.push(GroupName {
            group_id,
            group_name,
        });
    }
    Some(GitnBox { entries })
}

/// Serialise a `gitn` box. The byte-exact inverse of [`parse_gitn_box`].
pub fn build_gitn_box(b: &GitnBox) -> Vec<u8> {
    let mut body = Vec::new();
    wr_fullbox(&mut body, 0, 0);
    body.extend_from_slice(&(b.entries.len() as u16).to_be_bytes());
    for e in &b.entries {
        body.extend_from_slice(&e.group_id.to_be_bytes());
        body.extend_from_slice(e.group_name.as_bytes());
        body.push(0); // terminator
    }
    wrap(&GITN, &body)
}

// ===========================================================================
// fiin — FD Item Information Box (§8.13.2)
// ===========================================================================

/// Decoded `fiin` FD Item Information Box (ISO/IEC 14496-12 §8.13.2): the
/// partition entries plus an optional session-group box and group-name box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiinBox {
    /// Partition entries (`paen`), implicitly numbered from 1.
    pub partition_entries: Vec<PartitionEntry>,
    /// Optional `segr` FD Session Group Box.
    pub session_info: Option<SegrBox>,
    /// Optional `gitn` Group ID to Name Box.
    pub group_id_to_name: Option<GitnBox>,
}

/// Parse a `fiin` body. The leading `entry_count` is informational — the
/// parser walks the actual `paen` children rather than trusting the count,
/// so a count that disagrees with the box contents does not desync the
/// walk. `None` only on a truncated FullBox preamble / count.
pub fn parse_fiin_box(body: &[u8]) -> Option<FiinBox> {
    let mut p = 0usize;
    let (_version, _flags) = rd_fullbox(body, &mut p)?;
    let _entry_count = rd_u16(body, &mut p)?;
    let mut partition_entries = Vec::new();
    let mut session_info = None;
    let mut group_id_to_name = None;
    each_child(&body[p..], |fourcc, child| match fourcc {
        PAEN => partition_entries.push(parse_paen_box(child)),
        SEGR => session_info = parse_segr_box(child),
        GITN => group_id_to_name = parse_gitn_box(child),
        _ => {}
    });
    Some(FiinBox {
        partition_entries,
        session_info,
        group_id_to_name,
    })
}

/// Serialise a `fiin` box. Writes `entry_count` from the partition-entry
/// vector length, then the `paen` array, then (if present) `segr` and
/// `gitn`, matching §8.13.2.2 declaration order.
pub fn build_fiin_box(b: &FiinBox) -> Vec<u8> {
    let mut body = Vec::new();
    wr_fullbox(&mut body, 0, 0);
    body.extend_from_slice(&(b.partition_entries.len() as u16).to_be_bytes());
    for e in &b.partition_entries {
        body.extend_from_slice(&build_paen_box(e));
    }
    if let Some(segr) = &b.session_info {
        body.extend_from_slice(&build_segr_box(segr));
    }
    if let Some(gitn) = &b.group_id_to_name {
        body.extend_from_slice(&build_gitn_box(gitn));
    }
    wrap(&FIIN, &body)
}

// ===========================================================================
// feci — FEC Information Box (§9.2.4.7)
// ===========================================================================

/// Decoded `feci` FEC Information Box (ISO/IEC 14496-12 §9.2.4.7). A plain
/// Box (no version/flags) carried inside an `extr` Extra Data Box of an FD
/// hint-track sample. Fixed 7-byte body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeciBox {
    /// FEC encoding scheme ID (RFC 5052 registration).
    pub fec_encoding_id: u8,
    /// FEC encoder identification for under-specified schemes.
    pub fec_instance_id: u16,
    /// Source block the encoding symbol(s) in the FD packet derive from.
    pub source_block_number: u16,
    /// Which specific encoding symbol(s) the FD packet carries.
    pub encoding_symbol_id: u16,
}

/// Parse a `feci` body (the 7 bytes after the `[size][feci]` header).
/// `None` on truncation.
pub fn parse_feci_box(body: &[u8]) -> Option<FeciBox> {
    let mut p = 0usize;
    let fec_encoding_id = rd_u8(body, &mut p)?;
    let fec_instance_id = rd_u16(body, &mut p)?;
    let source_block_number = rd_u16(body, &mut p)?;
    let encoding_symbol_id = rd_u16(body, &mut p)?;
    Some(FeciBox {
        fec_encoding_id,
        fec_instance_id,
        source_block_number,
        encoding_symbol_id,
    })
}

/// Serialise a `feci` box. The byte-exact inverse of [`parse_feci_box`].
pub fn build_feci_box(b: &FeciBox) -> Vec<u8> {
    let mut body = Vec::with_capacity(7);
    body.push(b.fec_encoding_id);
    body.extend_from_slice(&b.fec_instance_id.to_be_bytes());
    body.extend_from_slice(&b.source_block_number.to_be_bytes());
    body.extend_from_slice(&b.encoding_symbol_id.to_be_bytes());
    wrap(&FECI, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip a `[size:u32][fourcc]` header, asserting the size field
    /// matches the total length and the fourcc matches.
    fn unwrap_box<'a>(bytes: &'a [u8], fourcc: &[u8; 4]) -> &'a [u8] {
        assert!(bytes.len() >= 8);
        let total = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(total, bytes.len(), "size field must equal total length");
        assert_eq!(&bytes[4..8], fourcc, "fourcc mismatch");
        &bytes[8..]
    }

    #[test]
    fn fpar_v0_round_trips() {
        let b = FparBox {
            version: 0,
            item_id: 0x1234,
            packet_payload_size: 1400,
            fec_encoding_id: 129,
            fec_instance_id: 7,
            max_source_block_length: 64,
            encoding_symbol_length: 1024,
            max_number_of_encoding_symbols: 255,
            scheme_specific_info: "AAEC".to_string(),
            entries: vec![
                FparEntry {
                    block_count: 3,
                    block_size: 4096,
                },
                FparEntry {
                    block_count: 1,
                    block_size: 512,
                },
            ],
        };
        let bytes = build_fpar_box(&b);
        let body = unwrap_box(&bytes, &FPAR);
        assert_eq!(parse_fpar_box(body).unwrap(), b);
    }

    #[test]
    fn fpar_v1_uses_32bit_ids() {
        let b = FparBox {
            version: 1,
            item_id: 0x0001_0000,
            packet_payload_size: 1200,
            fec_encoding_id: 0,
            fec_instance_id: 0,
            max_source_block_length: 100,
            encoding_symbol_length: 1316,
            max_number_of_encoding_symbols: 0,
            scheme_specific_info: String::new(),
            entries: vec![FparEntry {
                block_count: 0x1_0000_u32 as u16, // wraps to 0 — still round-trips as stored
                block_size: 0xDEAD_BEEF,
            }],
        };
        let bytes = build_fpar_box(&b);
        let body = unwrap_box(&bytes, &FPAR);
        // item_id stored as 32-bit here, so the full value survives.
        let parsed = parse_fpar_box(body).unwrap();
        assert_eq!(parsed.item_id, 0x0001_0000);
        assert_eq!(parsed.entries[0].block_size, 0xDEAD_BEEF);
    }

    #[test]
    fn fpar_empty_scheme_info_terminator_present() {
        let b = FparBox {
            version: 0,
            item_id: 1,
            packet_payload_size: 0,
            fec_encoding_id: 0,
            fec_instance_id: 0,
            max_source_block_length: 0,
            encoding_symbol_length: 0,
            max_number_of_encoding_symbols: 0,
            scheme_specific_info: String::new(),
            entries: vec![],
        };
        let bytes = build_fpar_box(&b);
        let body = unwrap_box(&bytes, &FPAR);
        // FullBox(4) + item_ID(2) + payload_size(2) + reserved(1) +
        // fec_enc(1) + fec_inst(2) + msbl(2) + esl(2) + mnes(2) +
        // string-terminator(1) + entry_count(2) = 21 bytes.
        assert_eq!(body.len(), 21);
        assert_eq!(parse_fpar_box(body).unwrap(), b);
    }

    #[test]
    fn fpar_unterminated_string_rejected() {
        // FullBox + item_ID + fixed fields, then bytes with no NUL.
        let mut body = Vec::new();
        wr_fullbox(&mut body, 0, 0);
        body.extend_from_slice(&1u16.to_be_bytes()); // item_id
        body.extend_from_slice(&0u16.to_be_bytes()); // payload size
        body.push(0); // reserved
        body.push(0); // fec_encoding_id
        body.extend_from_slice(&0u16.to_be_bytes()); // fec_instance
        body.extend_from_slice(&0u16.to_be_bytes()); // msbl
        body.extend_from_slice(&0u16.to_be_bytes()); // esl
        body.extend_from_slice(&0u16.to_be_bytes()); // mnes
        body.extend_from_slice(b"no terminator here"); // no NUL
        assert!(parse_fpar_box(&body).is_none());
    }

    #[test]
    fn reservoir_fecr_round_trips_both_versions() {
        for version in [0u8, 1u8] {
            let b = ReservoirBox {
                version,
                entries: vec![
                    ReservoirEntry {
                        item_id: 10,
                        symbol_count: 100,
                    },
                    ReservoirEntry {
                        item_id: 20,
                        symbol_count: 0,
                    },
                ],
            };
            let bytes = build_reservoir_box(&FECR, &b);
            let body = unwrap_box(&bytes, &FECR);
            assert_eq!(parse_reservoir_box(body).unwrap(), b);
        }
    }

    #[test]
    fn reservoir_fire_uses_distinct_fourcc() {
        let b = ReservoirBox {
            version: 0,
            entries: vec![ReservoirEntry {
                item_id: 5,
                symbol_count: 50,
            }],
        };
        let bytes = build_reservoir_box(&FIRE, &b);
        let body = unwrap_box(&bytes, &FIRE);
        assert_eq!(parse_reservoir_box(body).unwrap(), b);
    }

    #[test]
    fn paen_with_all_three_children_round_trips() {
        let e = PartitionEntry {
            fpar: Some(FparBox {
                version: 0,
                item_id: 1,
                packet_payload_size: 1400,
                fec_encoding_id: 1,
                fec_instance_id: 0,
                max_source_block_length: 32,
                encoding_symbol_length: 512,
                max_number_of_encoding_symbols: 64,
                scheme_specific_info: "x".to_string(),
                entries: vec![FparEntry {
                    block_count: 2,
                    block_size: 1024,
                }],
            }),
            fecr: Some(ReservoirBox {
                version: 0,
                entries: vec![ReservoirEntry {
                    item_id: 2,
                    symbol_count: 8,
                }],
            }),
            fire: Some(ReservoirBox {
                version: 1,
                entries: vec![ReservoirEntry {
                    item_id: 3,
                    symbol_count: 9,
                }],
            }),
        };
        let bytes = build_paen_box(&e);
        let body = unwrap_box(&bytes, &PAEN);
        assert_eq!(parse_paen_box(body), e);
    }

    #[test]
    fn paen_fpar_only_round_trips() {
        let e = PartitionEntry {
            fpar: Some(FparBox {
                version: 0,
                item_id: 1,
                packet_payload_size: 0,
                fec_encoding_id: 0,
                fec_instance_id: 0,
                max_source_block_length: 0,
                encoding_symbol_length: 0,
                max_number_of_encoding_symbols: 0,
                scheme_specific_info: String::new(),
                entries: vec![],
            }),
            fecr: None,
            fire: None,
        };
        let bytes = build_paen_box(&e);
        let body = unwrap_box(&bytes, &PAEN);
        let parsed = parse_paen_box(body);
        assert_eq!(parsed, e);
        assert!(parsed.fecr.is_none() && parsed.fire.is_none());
    }

    #[test]
    fn segr_round_trips() {
        let b = SegrBox {
            session_groups: vec![
                SessionGroup {
                    group_ids: vec![1, 2, 3],
                    hint_track_ids: vec![10, 11],
                },
                SessionGroup {
                    group_ids: vec![],
                    hint_track_ids: vec![20],
                },
            ],
        };
        let bytes = build_segr_box(&b);
        let body = unwrap_box(&bytes, &SEGR);
        assert_eq!(parse_segr_box(body).unwrap(), b);
    }

    #[test]
    fn gitn_round_trips_utf8_names() {
        let b = GitnBox {
            entries: vec![
                GroupName {
                    group_id: 1,
                    group_name: "base".to_string(),
                },
                GroupName {
                    group_id: 2,
                    group_name: "enhancement-層".to_string(),
                },
            ],
        };
        let bytes = build_gitn_box(&b);
        let body = unwrap_box(&bytes, &GITN);
        assert_eq!(parse_gitn_box(body).unwrap(), b);
    }

    #[test]
    fn fiin_full_round_trips() {
        let b = FiinBox {
            partition_entries: vec![
                PartitionEntry {
                    fpar: Some(FparBox {
                        version: 0,
                        item_id: 1,
                        packet_payload_size: 1400,
                        fec_encoding_id: 0,
                        fec_instance_id: 0,
                        max_source_block_length: 16,
                        encoding_symbol_length: 1024,
                        max_number_of_encoding_symbols: 16,
                        scheme_specific_info: String::new(),
                        entries: vec![FparEntry {
                            block_count: 1,
                            block_size: 16384,
                        }],
                    }),
                    fecr: Some(ReservoirBox {
                        version: 0,
                        entries: vec![ReservoirEntry {
                            item_id: 2,
                            symbol_count: 4,
                        }],
                    }),
                    fire: None,
                },
                PartitionEntry {
                    fpar: Some(FparBox {
                        version: 0,
                        item_id: 3,
                        packet_payload_size: 1200,
                        fec_encoding_id: 1,
                        fec_instance_id: 1,
                        max_source_block_length: 8,
                        encoding_symbol_length: 512,
                        max_number_of_encoding_symbols: 8,
                        scheme_specific_info: "AA==".to_string(),
                        entries: vec![],
                    }),
                    fecr: None,
                    fire: None,
                },
            ],
            session_info: Some(SegrBox {
                session_groups: vec![SessionGroup {
                    group_ids: vec![1],
                    hint_track_ids: vec![100, 101],
                }],
            }),
            group_id_to_name: Some(GitnBox {
                entries: vec![GroupName {
                    group_id: 1,
                    group_name: "main".to_string(),
                }],
            }),
        };
        let bytes = build_fiin_box(&b);
        let body = unwrap_box(&bytes, &FIIN);
        assert_eq!(parse_fiin_box(body).unwrap(), b);
    }

    #[test]
    fn fiin_entry_count_ignored_for_walk() {
        // Build a valid fiin, then corrupt the entry_count field to a
        // wrong value: the walk should still recover both paen entries
        // because it iterates the actual children, not the count.
        let b = FiinBox {
            partition_entries: vec![
                PartitionEntry {
                    fpar: Some(FparBox {
                        version: 0,
                        item_id: 1,
                        packet_payload_size: 0,
                        fec_encoding_id: 0,
                        fec_instance_id: 0,
                        max_source_block_length: 0,
                        encoding_symbol_length: 0,
                        max_number_of_encoding_symbols: 0,
                        scheme_specific_info: String::new(),
                        entries: vec![],
                    }),
                    fecr: None,
                    fire: None,
                },
                PartitionEntry {
                    fpar: Some(FparBox {
                        version: 0,
                        item_id: 2,
                        packet_payload_size: 0,
                        fec_encoding_id: 0,
                        fec_instance_id: 0,
                        max_source_block_length: 0,
                        encoding_symbol_length: 0,
                        max_number_of_encoding_symbols: 0,
                        scheme_specific_info: String::new(),
                        entries: vec![],
                    }),
                    fecr: None,
                    fire: None,
                },
            ],
            session_info: None,
            group_id_to_name: None,
        };
        let mut bytes = build_fiin_box(&b);
        // entry_count lives at body offset [4..6] (after FullBox preamble),
        // i.e. at byte offset 8+4 = 12 in the full box.
        bytes[12] = 0xFF;
        bytes[13] = 0xFF;
        let body = unwrap_box(&bytes, &FIIN);
        let parsed = parse_fiin_box(body).unwrap();
        assert_eq!(parsed.partition_entries.len(), 2);
    }

    #[test]
    fn feci_round_trips() {
        let b = FeciBox {
            fec_encoding_id: 129,
            fec_instance_id: 0x1234,
            source_block_number: 0x5678,
            encoding_symbol_id: 0x9ABC,
        };
        let bytes = build_feci_box(&b);
        let body = unwrap_box(&bytes, &FECI);
        assert_eq!(body.len(), 7);
        assert_eq!(parse_feci_box(body).unwrap(), b);
    }

    #[test]
    fn feci_truncated_rejected() {
        assert!(parse_feci_box(&[0x81, 0x00]).is_none());
    }
}
