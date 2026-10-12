//! Directory indexes (`$I30`): `$INDEX_ROOT`, `$INDEX_ALLOCATION` and the
//! `$BITMAP` that says which index blocks are in use.
//!
//! Every index entry's key is a complete `$FILE_NAME` value, so a directory
//! listing yields child names, parent references, sizes and times without
//! touching the children's MFT records. For recovery the B-tree order does
//! not matter, so instead of descending sub-node pointers the reader visits
//! every index block of the allocation in order. That keeps working when an
//! interior node is unreadable, and it naturally includes:
//!
//! * entries of blocks the `$I30` bitmap marks unused (whole blocks freed
//!   when a directory shrank), reported as [`IndexSource::UnusedBlock`];
//! * "slack": bytes between a block's used length and its allocated length,
//!   which keep copies of entries that were removed or shifted. Slack is
//!   scanned for `$FILE_NAME` keys at 8-byte alignment and a candidate is
//!   accepted only when its parent reference names this directory, its
//!   namespace and name are valid and its timestamps are plausible.
//!
//! INDX blocks get their update sequence array applied before parsing; a
//! torn block is still parsed. Loops are bounded by block size, block
//! count ([`MAX_INDEX_BLOCKS`]) and entry count ([`MAX_DIR_ENTRIES`]).

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{u16le, u32le, u64le, u8_at};

use crate::attr::{mref, types, Attribute, FileName};
use crate::fixup::apply_fixups;
use crate::stream::{read_all, read_plain, Annotations, StreamData};
use crate::volume::NtfsFs;
use crate::Result;

/// Upper bound on index blocks read for one directory.
pub const MAX_INDEX_BLOCKS: u64 = 1 << 20;
/// Upper bound on entries collected for one directory.
pub const MAX_DIR_ENTRIES: usize = 1 << 20;
/// Name of the file-name index.
pub const I30: &str = "$I30";

/// Index entry flag: the entry has a sub-node.
pub const ENTRY_SUBNODE: u16 = 0x01;
/// Index entry flag: last entry of the node (no key).
pub const ENTRY_LAST: u16 = 0x02;

/// Where an index entry was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexSource {
    /// A live entry of the root or an in-use block.
    Live,
    /// An entry of a block the `$I30` bitmap marks unused.
    UnusedBlock,
    /// Recovered from slack space after the used part of a node.
    Slack,
}

/// One directory entry found in an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCandidate {
    /// File reference of the child.
    pub reference: u64,
    /// The child's `$FILE_NAME` as stored in the index.
    pub key: FileName,
    /// Where it was found.
    pub source: IndexSource,
}

/// Node header shared by `$INDEX_ROOT` and INDX blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeHeader {
    /// Offset of the first entry, relative to the node header.
    pub entries_offset: u32,
    /// Bytes in use, relative to the node header.
    pub index_length: u32,
    /// Bytes allocated, relative to the node header.
    pub allocated_length: u32,
    /// 0x01: the node has children (root: an `$INDEX_ALLOCATION` exists).
    pub flags: u8,
}

impl NodeHeader {
    fn parse(b: &[u8], at: usize) -> Option<Self> {
        Some(NodeHeader {
            entries_offset: u32le(b, at)?,
            index_length: u32le(b, at + 4)?,
            allocated_length: u32le(b, at + 8)?,
            flags: u8_at(b, at + 12)?,
        })
    }
}

/// Decoded `$INDEX_ROOT` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRoot {
    /// Indexed attribute type (0x30 for directories).
    pub indexed_type: u32,
    /// Collation rule.
    pub collation: u32,
    /// Size of an index block in bytes.
    pub block_size: u32,
    /// Clusters (or 512-byte blocks) per index block.
    pub clusters_per_block: i8,
    /// Root node header.
    pub node: NodeHeader,
}

impl IndexRoot {
    /// Parse an `$INDEX_ROOT` value.
    pub fn parse(v: &[u8]) -> Option<Self> {
        Some(IndexRoot {
            indexed_type: u32le(v, 0)?,
            collation: u32le(v, 4)?,
            block_size: u32le(v, 8)?,
            clusters_per_block: u8_at(v, 12)? as i8,
            node: NodeHeader::parse(v, 0x10)?,
        })
    }
}

/// Parse the entries of a node whose header sits at `hdr` inside `b`.
/// Live entries go to `out` with `source`; when `slack` is true the bytes
/// between the used and allocated length are scanned too.
pub fn parse_node(
    b: &[u8],
    hdr: usize,
    source: IndexSource,
    slack: bool,
    dir_record: u64,
    out: &mut Vec<IndexCandidate>,
) {
    let Some(node) = NodeHeader::parse(b, hdr) else {
        return;
    };
    let start = hdr.saturating_add(node.entries_offset as usize);
    let used_end = hdr.saturating_add(node.index_length as usize).min(b.len());
    let alloc_end = hdr
        .saturating_add(node.allocated_length as usize)
        .min(b.len());
    let mut off = start;
    let mut live_end = start;
    while off + 0x10 <= used_end && out.len() < MAX_DIR_ENTRIES {
        let reference = u64le(b, off).unwrap_or(0);
        let elen = u16le(b, off + 8).unwrap_or(0) as usize;
        let klen = u16le(b, off + 10).unwrap_or(0) as usize;
        let flags = u16le(b, off + 12).unwrap_or(0);
        if elen < 0x10 || elen % 8 != 0 || off + elen > used_end {
            break;
        }
        if flags & ENTRY_LAST != 0 {
            live_end = off + elen;
            break;
        }
        if klen >= 0x42 && 0x10 + klen <= elen {
            if let Some(key) = FileName::parse(&b[off + 0x10..off + 0x10 + klen]) {
                out.push(IndexCandidate {
                    reference,
                    key,
                    source,
                });
            }
        }
        off += elen;
        live_end = off;
    }
    if slack {
        let from = live_end.max(start).min(alloc_end);
        scan_slack(b, from, alloc_end, dir_record, out);
    }
}

/// Scan `b[from..to]` for stale index entries of directory `dir_record`.
///
/// Removing the last entry of a node moves the end marker over the removed
/// entry's 16-byte header, so its key typically starts exactly at the used
/// length and its file reference is gone; the scan therefore anchors on
/// keys, starting at `from` itself. The reference is taken from the 8 bytes
/// 16 bytes before the key when they lie in the scanned area and is 0
/// (unknown) otherwise.
pub fn scan_slack(
    b: &[u8],
    from: usize,
    to: usize,
    dir_record: u64,
    out: &mut Vec<IndexCandidate>,
) {
    let to = to.min(b.len());
    // Keys of 8-byte aligned entries are 8-byte aligned too.
    let mut k = (from + 7) & !7;
    while k + 0x42 <= to && out.len() < MAX_DIR_ENTRIES {
        match plausible_key(&b[k..to], dir_record) {
            Some(key) => {
                let reference = if k >= from + 0x10 {
                    u64le(b, k - 0x10).unwrap_or(0)
                } else {
                    0
                };
                let len = key.encoded_len();
                out.push(IndexCandidate {
                    reference,
                    key,
                    source: IndexSource::Slack,
                });
                k = (k + len + 0x10 + 7) & !7;
            }
            None => k += 8,
        }
    }
}

fn plausible_key(b: &[u8], dir_record: u64) -> Option<FileName> {
    // Cheap pre-checks before decoding the name.
    if mref(u64le(b, 0)?) != dir_record {
        return None;
    }
    let key = FileName::parse(b)?;
    let created = Timestamp::from_filetime(key.created)?;
    let modified = Timestamp::from_filetime(key.modified)?;
    if !created.is_plausible() || !modified.is_plausible() {
        return None;
    }
    if key.allocated_size % 8 != 0 || key.real_size > key.allocated_size.max(1 << 40) {
        return None;
    }
    if key
        .name
        .chars()
        .any(|c| c == '\0' || c == '/' || c == '\u{FFFD}')
    {
        return None;
    }
    Some(key)
}

impl NtfsFs {
    /// Collect the entries of a directory's `$I30` index. With
    /// `want_deleted`, entries of unused blocks and slack are included.
    pub(crate) fn index_candidates(
        &self,
        dir_record: u64,
        attrs: &[Attribute],
        want_deleted: bool,
    ) -> Result<Vec<IndexCandidate>> {
        let mut out = Vec::new();
        let root = attrs
            .iter()
            .find(|a| a.type_code == types::INDEX_ROOT && a.name == I30);
        let mut block_size = self.boot.index_record_size as u64;
        if let Some(v) = root.and_then(|a| a.value()) {
            if let Some(ir) = IndexRoot::parse(v) {
                let bs = ir.block_size as u64;
                if bs.is_power_of_two() && (512..=65536).contains(&bs) {
                    block_size = bs;
                }
                parse_node(v, 0x10, IndexSource::Live, false, dir_record, &mut out);
            }
        }
        let Some(alloc) = self.stream_of(attrs, types::INDEX_ALLOCATION, I30) else {
            return Ok(out);
        };
        let StreamData::NonResident(alloc) = &alloc.data else {
            return Ok(out);
        };
        let bitmap = match self.stream_of(attrs, types::BITMAP, I30) {
            Some(s) => {
                let need = alloc.data_size / block_size / 8 + 1;
                let (bytes, ann) = read_all(&self.disk, &s, self.cluster, need.min(1 << 20))?;
                Some((bytes, ann.unreadable))
            }
            None => None,
        };
        let blocks = (alloc.data_size / block_size).min(MAX_INDEX_BLOCKS);
        let vcn_unit = if block_size >= self.cluster {
            self.cluster
        } else {
            512
        };
        let mut buf = vec![0u8; block_size as usize];
        for b in 0..blocks {
            if out.len() >= MAX_DIR_ENTRIES {
                break;
            }
            let in_use = match &bitmap {
                Some((bytes, bad)) => {
                    let byte = (b / 8) as usize;
                    if bad.contains(byte as u64) || byte >= bytes.len() {
                        true
                    } else {
                        bytes[byte] & (1 << (b % 8)) != 0
                    }
                }
                None => true,
            };
            if !in_use && !want_deleted {
                continue;
            }
            let mut ann = Annotations::default();
            read_plain(
                &self.disk,
                alloc,
                self.cluster,
                b * block_size,
                &mut buf,
                &mut ann,
                None,
                true,
            )?;
            if !ann.unreadable.is_empty() {
                log::warn!(
                    "NTFS: index block {b} of directory {dir_record} is unreadable; its entries are lost"
                );
                continue;
            }
            if &buf[0..4] != b"INDX" {
                if in_use {
                    log::debug!(
                        "NTFS: index block {b} of directory {dir_record} has no INDX signature"
                    );
                }
                continue;
            }
            let mut block = buf.clone();
            match apply_fixups(&mut block) {
                Ok(torn) if !torn.is_empty() => log::warn!(
                    "NTFS: index block {b} of directory {dir_record}: torn write in {torn:?}"
                ),
                Ok(_) => {}
                Err(e) => {
                    log::warn!("NTFS: index block {b} of directory {dir_record}: {e}");
                    continue;
                }
            }
            let vcn = u64le(&block, 0x10).unwrap_or(0);
            if vcn != b * block_size / vcn_unit {
                log::debug!("NTFS: index block {b} of directory {dir_record} claims VCN {vcn}");
            }
            let source = if in_use {
                IndexSource::Live
            } else {
                IndexSource::UnusedBlock
            };
            parse_node(&block, 0x18, source, want_deleted, dir_record, &mut out);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::{build_file_name, namespace};

    fn entry(reference: u64, key: &[u8], flags: u16) -> Vec<u8> {
        let len = (0x10 + key.len() + 7) & !7;
        let mut e = vec![0u8; len];
        e[0..8].copy_from_slice(&reference.to_le_bytes());
        e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(key.len() as u16).to_le_bytes());
        e[12..14].copy_from_slice(&flags.to_le_bytes());
        e[0x10..0x10 + key.len()].copy_from_slice(key);
        e
    }

    fn last() -> Vec<u8> {
        let mut e = vec![0u8; 0x10];
        e[8..10].copy_from_slice(&0x10u16.to_le_bytes());
        e[12..14].copy_from_slice(&ENTRY_LAST.to_le_bytes());
        e
    }

    #[test]
    fn live_entries_and_slack() {
        let dir = 64u64;
        let k1 = build_file_name(dir | (1 << 48), "a.jpg", namespace::POSIX, 0, 100);
        let k2 = build_file_name(dir | (1 << 48), "gone.jpg", namespace::WIN32, 0, 5000);
        let k3 = build_file_name(7 | (1 << 48), "other.jpg", namespace::WIN32, 0, 5000);
        let mut node = vec![0u8; 0x10];
        let mut body = entry(70 | (1 << 48), &k1, 0);
        body.extend(last());
        let used = 0x10 + body.len();
        // Slack: a removed entry of this directory and one of another.
        body.extend(entry(71 | (2 << 48), &k2, 0));
        body.extend(entry(72 | (2 << 48), &k3, 0));
        body.extend(vec![0u8; 64]);
        let alloc = 0x10 + body.len();
        node[0..4].copy_from_slice(&0x10u32.to_le_bytes());
        node[4..8].copy_from_slice(&(used as u32).to_le_bytes());
        node[8..12].copy_from_slice(&(alloc as u32).to_le_bytes());
        node.extend(body);
        let mut out = Vec::new();
        parse_node(&node, 0, IndexSource::Live, true, dir, &mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].key.name, "a.jpg");
        assert_eq!(out[0].source, IndexSource::Live);
        assert_eq!(out[1].key.name, "gone.jpg");
        assert_eq!(out[1].source, IndexSource::Slack);
        assert_eq!(mref(out[1].reference), 71);
        // Without slack scanning only the live entry is reported.
        let mut out = Vec::new();
        parse_node(&node, 0, IndexSource::Live, false, dir, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn hostile_nodes_do_not_panic() {
        let mut seed = 0x9E37_79B9u32;
        for _ in 0..500 {
            let len = (seed % 600) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let mut out = Vec::new();
            parse_node(&buf, 0, IndexSource::Live, true, 5, &mut out);
            parse_node(&buf, 0x18, IndexSource::Live, true, 5, &mut out);
            let _ = IndexRoot::parse(&buf);
        }
        let mut out = Vec::new();
        // Absurd header values.
        let mut node = vec![0xFFu8; 64];
        node[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        parse_node(&node, 0, IndexSource::Live, true, 5, &mut out);
        scan_slack(&node, 1000, 10, 5, &mut out);
        assert!(out.is_empty());
    }
}
