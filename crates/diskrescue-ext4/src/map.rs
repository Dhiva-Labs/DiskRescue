//! Logical-to-physical block mapping: extent trees and classic block maps.
//!
//! Both mappers produce a sorted, non-overlapping list of [`Run`]s plus the
//! logical ranges whose mapping could not be determined (unreadable index or
//! indirect block, pointer outside the volume). Logical blocks covered by
//! neither are sparse holes.
//!
//! Hostile-input bounds: extent trees are limited to depth 5 and a fixed
//! number of nodes with a visited set (a node referenced twice is treated as
//! corruption); block maps stop at the file size, never read an indirect
//! block twice and abort on the first pointer outside the volume (the rest
//! of the file is then "unknown", as the spec requires).

use std::collections::HashSet;

use diskrescue_core::util::bytes::{u16le, u32le};

/// Extent header magic.
pub const EXTENT_MAGIC: u16 = 0xF30A;
/// Deepest extent tree accepted.
pub const MAX_EXTENT_DEPTH: u16 = 5;
/// `ee_len` values above this mark uninitialised (preallocated) extents.
pub const EXT_INIT_MAX_LEN: u16 = 32768;
/// Index/leaf blocks read per extent tree at most.
const MAX_EXTENT_NODES: usize = 1 << 16;
/// Leaf extents kept per file at most.
const MAX_EXTENTS: usize = 1 << 22;
/// Indirect blocks read per block-mapped file at most.
const MAX_INDIRECT_BLOCKS: usize = 1 << 20;
/// Logical block numbers in an extent tree are 32-bit.
pub const MAX_LOGICAL_BLOCKS: u64 = 1 << 32;

/// A contiguous mapping of `len` logical blocks starting at `logical` to
/// physical blocks starting at `physical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Run {
    /// First logical block (file-relative).
    pub logical: u64,
    /// First physical block (volume-relative).
    pub physical: u64,
    /// Number of blocks.
    pub len: u64,
    /// Uninitialised extent: allocated but reads as zeros.
    pub uninit: bool,
}

impl Run {
    /// One past the last logical block.
    pub fn logical_end(&self) -> u64 {
        self.logical + self.len
    }
}

/// Volume geometry needed to validate block numbers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub block_size: u64,
    pub blocks_count: u64,
    pub first_data_block: u64,
}

impl Geometry {
    fn valid_range(&self, start: u64, len: u64) -> bool {
        start >= self.first_data_block.max(1)
            && start
                .checked_add(len)
                .is_some_and(|e| e <= self.blocks_count)
    }
}

/// Reads one filesystem block; `None` when unreadable.
pub(crate) trait BlockReader {
    fn read_block(&self, block: u64) -> Option<Vec<u8>>;
}

impl<F: Fn(u64) -> Option<Vec<u8>>> BlockReader for F {
    fn read_block(&self, block: u64) -> Option<Vec<u8>> {
        self(block)
    }
}

/// A file's block mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Mapping {
    /// Sorted, non-overlapping runs.
    pub runs: Vec<Run>,
    /// Sorted logical `[start, end)` ranges whose mapping is unknown.
    pub unknown: Vec<(u64, u64)>,
    /// Problems for the report.
    pub notes: Vec<String>,
}

impl Mapping {
    /// One past the last mapped logical block (0 when nothing is mapped).
    pub fn mapped_end(&self) -> u64 {
        self.runs.iter().map(Run::logical_end).max().unwrap_or(0)
    }

    /// Whether any initialised data block is mapped.
    pub fn has_data(&self) -> bool {
        self.runs.iter().any(|r| !r.uninit)
    }

    fn finish(mut self, limit: u64) -> Mapping {
        self.runs.sort_by_key(|r| (r.logical, r.physical));
        let mut out: Vec<Run> = Vec::with_capacity(self.runs.len());
        let mut cursor = 0u64;
        let mut overlaps = false;
        for mut r in self.runs.drain(..) {
            if r.logical >= limit {
                continue;
            }
            if r.logical_end() > limit {
                r.len = limit - r.logical;
            }
            if r.logical < cursor {
                overlaps = true;
                let cut = cursor - r.logical;
                if cut >= r.len {
                    continue;
                }
                r.logical += cut;
                r.physical += cut;
                r.len -= cut;
            }
            cursor = r.logical_end();
            match out.last_mut() {
                Some(last)
                    if last.logical_end() == r.logical
                        && last.physical + last.len == r.physical
                        && last.uninit == r.uninit =>
                {
                    last.len += r.len
                }
                _ => out.push(r),
            }
        }
        if overlaps {
            self.notes
                .push("overlapping extents; later duplicates ignored".into());
        }
        self.runs = out;
        let mut unknown: Vec<(u64, u64)> = self
            .unknown
            .iter()
            .map(|&(s, e)| (s, e.min(limit)))
            .filter(|&(s, e)| s < e)
            .collect();
        unknown.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, e) in unknown {
            match merged.last_mut() {
                Some(l) if s <= l.1 => l.1 = l.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.unknown = merged;
        self
    }
}

struct ExtentWalk<'a> {
    geo: Geometry,
    reader: &'a dyn BlockReader,
    visited: HashSet<u64>,
    m: Mapping,
    truncated: bool,
}

struct Header {
    entries: u16,
    depth: u16,
}

fn parse_header(node: &[u8]) -> Result<Header, String> {
    let magic = u16le(node, 0).ok_or("node too short")?;
    if magic != EXTENT_MAGIC {
        return Err(format!("bad extent magic {magic:#06x}"));
    }
    let entries = u16le(node, 2).ok_or("node too short")?;
    let max = u16le(node, 4).ok_or("node too short")?;
    let depth = u16le(node, 6).ok_or("node too short")?;
    let cap = node.len().saturating_sub(12) / 12;
    if entries > max || max as usize > cap {
        return Err(format!(
            "extent header entries {entries} max {max} cap {cap}"
        ));
    }
    if depth > MAX_EXTENT_DEPTH {
        return Err(format!("extent depth {depth}"));
    }
    Ok(Header { entries, depth })
}

impl ExtentWalk<'_> {
    fn node(&mut self, data: &[u8], expect_depth: Option<u16>, lstart: u64, lend: u64) {
        let h = match parse_header(data) {
            Ok(h) => h,
            Err(e) => {
                self.m.unknown.push((lstart, lend));
                self.m.notes.push(e);
                return;
            }
        };
        if expect_depth.is_some_and(|d| d != h.depth) {
            self.m.unknown.push((lstart, lend));
            self.m.notes.push(format!(
                "extent node depth {} where {} expected",
                h.depth,
                expect_depth.unwrap_or(0)
            ));
            return;
        }
        let n = h.entries as usize;
        if h.depth == 0 {
            for i in 0..n {
                let o = 12 + 12 * i;
                let (Some(lb), Some(raw_len), Some(hi), Some(lo)) = (
                    u32le(data, o),
                    u16le(data, o + 4),
                    u16le(data, o + 6),
                    u32le(data, o + 8),
                ) else {
                    break;
                };
                let (len, uninit) = if raw_len > EXT_INIT_MAX_LEN {
                    ((raw_len - EXT_INIT_MAX_LEN) as u64, true)
                } else {
                    (raw_len as u64, false)
                };
                if len == 0 {
                    continue;
                }
                let lb = lb as u64;
                let pb = lo as u64 | (hi as u64) << 32;
                if !self.geo.valid_range(pb, len) {
                    self.m.unknown.push((lb, lb + len));
                    self.m.notes.push(format!(
                        "extent at logical {lb} points outside the volume ({pb})"
                    ));
                    continue;
                }
                if self.m.runs.len() >= MAX_EXTENTS {
                    self.truncated = true;
                    return;
                }
                self.m.runs.push(Run {
                    logical: lb,
                    physical: pb,
                    len,
                    uninit,
                });
            }
            return;
        }
        for i in 0..n {
            let o = 12 + 12 * i;
            let (Some(lb), Some(lo), Some(hi)) =
                (u32le(data, o), u32le(data, o + 4), u16le(data, o + 8))
            else {
                break;
            };
            let child_start = (lb as u64).max(lstart);
            let child_end = if i + 1 < n {
                u32le(data, o + 12).map_or(lend, |v| (v as u64).clamp(child_start, lend))
            } else {
                lend
            };
            let child = lo as u64 | (hi as u64) << 32;
            if !self.geo.valid_range(child, 1) || !self.visited.insert(child) {
                self.m.unknown.push((child_start, child_end));
                self.m
                    .notes
                    .push(format!("extent index points to invalid block {child}"));
                continue;
            }
            if self.visited.len() > MAX_EXTENT_NODES {
                self.truncated = true;
                return;
            }
            match self.reader.read_block(child) {
                Some(b) => self.node(&b, Some(h.depth - 1), child_start, child_end),
                None => {
                    self.m.unknown.push((child_start, child_end));
                    self.m
                        .notes
                        .push(format!("extent tree block {child} unreadable"));
                }
            }
            if self.truncated {
                return;
            }
        }
    }
}

/// Map an extent tree rooted in `i_block` (60 bytes). Runs beyond `limit`
/// logical blocks are clipped.
pub(crate) fn map_extents(
    i_block: &[u8],
    geo: Geometry,
    limit: u64,
    reader: &dyn BlockReader,
) -> Mapping {
    let mut w = ExtentWalk {
        geo,
        reader,
        visited: HashSet::new(),
        m: Mapping::default(),
        truncated: false,
    };
    w.node(i_block, None, 0, MAX_LOGICAL_BLOCKS);
    if w.truncated {
        w.m.notes
            .push("extent tree too large; mapping truncated".into());
        let end = w.m.mapped_end();
        w.m.unknown.push((end, MAX_LOGICAL_BLOCKS));
    }
    w.m.finish(limit)
}

struct BlockMapWalk<'a> {
    geo: Geometry,
    reader: &'a dyn BlockReader,
    visited: HashSet<u64>,
    m: Mapping,
    limit: u64,
    ptrs: u64,
}

enum Flow {
    Go,
    Abort,
}

impl BlockMapWalk<'_> {
    fn span(&self, level: u32) -> u64 {
        self.ptrs.saturating_pow(level)
    }

    fn data(&mut self, lblk: u64, pblk: u64) {
        match self.m.runs.last_mut() {
            Some(r) if r.logical_end() == lblk && r.physical + r.len == pblk => r.len += 1,
            _ => self.m.runs.push(Run {
                logical: lblk,
                physical: pblk,
                len: 1,
                uninit: false,
            }),
        }
    }

    fn abort(&mut self, from: u64, why: String) -> Flow {
        self.m.unknown.push((from, self.limit));
        self.m.notes.push(why);
        Flow::Abort
    }

    /// Walk the indirect block `block` of `level` (1 = points to data)
    /// covering logical blocks from `base`.
    fn indirect(&mut self, block: u64, level: u32, base: u64) -> Flow {
        if !self.visited.insert(block) || self.visited.len() > MAX_INDIRECT_BLOCKS {
            return self.abort(base, format!("indirect block {block} repeated or too many"));
        }
        let span = self.span(level);
        let Some(data) = self.reader.read_block(block) else {
            self.m.unknown.push((base, base.saturating_add(span)));
            self.m
                .notes
                .push(format!("indirect block {block} unreadable"));
            return Flow::Go;
        };
        let child_span = self.span(level - 1);
        for j in 0..self.ptrs {
            let child_base = base.saturating_add(j.saturating_mul(child_span));
            if child_base >= self.limit {
                break;
            }
            let q = u32le(&data, (j * 4) as usize).unwrap_or(0) as u64;
            if q == 0 {
                continue;
            }
            if !self.geo.valid_range(q, 1) {
                return self.abort(child_base, format!("wild block pointer {q}"));
            }
            if level == 1 {
                self.data(child_base, q);
            } else if let Flow::Abort = self.indirect(q, level - 1, child_base) {
                return Flow::Abort;
            }
        }
        Flow::Go
    }
}

/// Map a classic ext2/ext3 block map (12 direct pointers, then single,
/// double and triple indirect) for the first `limit` logical blocks.
pub(crate) fn map_blockmap(
    i_block: &[u8],
    geo: Geometry,
    limit: u64,
    reader: &dyn BlockReader,
) -> Mapping {
    let mut w = BlockMapWalk {
        geo,
        reader,
        visited: HashSet::new(),
        m: Mapping::default(),
        limit,
        ptrs: (geo.block_size / 4).max(1),
    };
    let ptr = |i: usize| u32le(i_block, i * 4).unwrap_or(0) as u64;
    'outer: {
        for i in 0..12u64 {
            if i >= limit {
                break 'outer;
            }
            let p = ptr(i as usize);
            if p == 0 {
                continue;
            }
            if !geo.valid_range(p, 1) {
                w.abort(i, format!("wild block pointer {p}"));
                break 'outer;
            }
            w.data(i, p);
        }
        let mut base = 12u64;
        for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
            if base >= limit {
                break;
            }
            let p = ptr(slot);
            if p != 0 {
                if !geo.valid_range(p, 1) {
                    w.abort(base, format!("wild indirect pointer {p}"));
                    break;
                }
                if let Flow::Abort = w.indirect(p, level, base) {
                    break;
                }
            }
            base = base.saturating_add(w.span(level));
        }
    }
    let lim = w.limit;
    w.m.finish(lim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn geo() -> Geometry {
        Geometry {
            block_size: 1024,
            blocks_count: 10_000,
            first_data_block: 1,
        }
    }

    fn header(entries: u16, max: u16, depth: u16) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        v[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        v[2..4].copy_from_slice(&entries.to_le_bytes());
        v[4..6].copy_from_slice(&max.to_le_bytes());
        v[6..8].copy_from_slice(&depth.to_le_bytes());
        v
    }

    fn leaf(lb: u32, len: u16, pb: u32) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        v[0..4].copy_from_slice(&lb.to_le_bytes());
        v[4..6].copy_from_slice(&len.to_le_bytes());
        v[8..12].copy_from_slice(&pb.to_le_bytes());
        v
    }

    fn idx(lb: u32, child: u32) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        v[0..4].copy_from_slice(&lb.to_le_bytes());
        v[4..8].copy_from_slice(&child.to_le_bytes());
        v
    }

    fn pad(mut v: Vec<u8>, len: usize) -> Vec<u8> {
        v.resize(len, 0);
        v
    }

    #[test]
    fn extent_leaf_root_with_uninit_and_holes() {
        let mut root = header(3, 4, 0);
        root.extend(leaf(0, 2, 100));
        root.extend(leaf(5, 32768 + 3, 200)); // uninitialised, 3 blocks
        root.extend(leaf(10, 1, 300));
        let root = pad(root, 60);
        let m = map_extents(&root, geo(), 100, &|_| None);
        assert_eq!(m.runs.len(), 3);
        assert_eq!(m.runs[1].len, 3);
        assert!(m.runs[1].uninit);
        assert!(m.unknown.is_empty());
        // Clipped to the file size.
        let m = map_extents(&root, geo(), 6, &|_| None);
        assert_eq!(m.runs.last().unwrap().logical_end(), 6);
    }

    #[test]
    fn extent_index_nodes_and_failures() {
        let mut root = header(2, 4, 1);
        root.extend(idx(0, 50));
        root.extend(idx(100, 51));
        let root = pad(root, 60);
        let mut blocks = HashMap::new();
        let mut l1 = header(2, 84, 0);
        l1.extend(leaf(0, 4, 1000));
        l1.extend(leaf(4, 4, 1004)); // contiguous: merged
        blocks.insert(50u64, pad(l1, 1024));
        let reader = move |b: u64| blocks.get(&b).cloned();
        let m = map_extents(&root, geo(), 1000, &reader);
        assert_eq!(
            m.runs,
            vec![Run {
                logical: 0,
                physical: 1000,
                len: 8,
                uninit: false
            }]
        );
        // Block 51 unreadable: its range 100..limit is unknown.
        assert_eq!(m.unknown, vec![(100, 1000)]);
        assert!(!m.notes.is_empty());
    }

    #[test]
    fn extent_cycles_depth_and_garbage() {
        // Index pointing to itself (block 50 is an index node pointing at 50).
        let mut root = header(1, 4, 2);
        root.extend(idx(0, 50));
        let root = pad(root, 60);
        let mut n = header(1, 84, 1);
        n.extend(idx(0, 50));
        let n = pad(n, 1024);
        let reader = move |b: u64| if b == 50 { Some(n.clone()) } else { None };
        let m = map_extents(&root, geo(), 10, &reader);
        assert!(m.runs.is_empty());
        assert_eq!(m.unknown, vec![(0, 10)]);
        // Depth too large.
        let bad = pad(header(0, 4, 9), 60);
        assert!(map_extents(&bad, geo(), 10, &|_| None).runs.is_empty());
        // entries > max.
        let bad = pad(header(5, 4, 0), 60);
        assert_eq!(
            map_extents(&bad, geo(), 10, &|_| None).unknown,
            vec![(0, 10)]
        );
        // Extent beyond the volume.
        let mut r = header(1, 4, 0);
        r.extend(leaf(0, 10, 9_995));
        let m = map_extents(&pad(r, 60), geo(), 10, &|_| None);
        assert!(m.runs.is_empty());
        assert_eq!(m.unknown, vec![(0, 10)]);
        // Random bytes never panic.
        let mut seed = 12345u64;
        for _ in 0..2000 {
            let mut v = vec![0u8; 60];
            for b in v.iter_mut() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (seed >> 33) as u8;
            }
            v[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
            let _ = map_extents(&v, geo(), 1 << 20, &|_| Some(vec![0xA5; 1024]));
            let _ = map_blockmap(&v, geo(), 1 << 20, &|_| Some(vec![0x5A; 1024]));
        }
    }

    #[test]
    fn block_map_direct_indirect_double() {
        let mut ib = vec![0u8; 60];
        for i in 0..12u32 {
            ib[(i * 4) as usize..(i * 4 + 4) as usize].copy_from_slice(&(100 + i).to_le_bytes());
        }
        ib[48..52].copy_from_slice(&500u32.to_le_bytes()); // single indirect
        ib[52..56].copy_from_slice(&501u32.to_le_bytes()); // double indirect
        let mut ind = vec![0u8; 1024];
        for j in 0..256u32 {
            ind[(j * 4) as usize..(j * 4 + 4) as usize].copy_from_slice(&(112 + j).to_le_bytes());
        }
        let mut dind = vec![0u8; 1024];
        dind[0..4].copy_from_slice(&502u32.to_le_bytes());
        let mut ind2 = vec![0u8; 1024];
        ind2[0..4].copy_from_slice(&2000u32.to_le_bytes());
        // Hole at the second pointer, then data.
        ind2[8..12].copy_from_slice(&2002u32.to_le_bytes());
        let reader = move |b: u64| match b {
            500 => Some(ind.clone()),
            501 => Some(dind.clone()),
            502 => Some(ind2.clone()),
            _ => None,
        };
        let m = map_blockmap(&ib, geo(), 12 + 256 + 3, &reader);
        assert_eq!(
            m.runs[0],
            Run {
                logical: 0,
                physical: 100,
                len: 268,
                uninit: false
            }
        );
        assert_eq!(
            m.runs[1],
            Run {
                logical: 268,
                physical: 2000,
                len: 1,
                uninit: false
            }
        );
        assert_eq!(
            m.runs[2],
            Run {
                logical: 270,
                physical: 2002,
                len: 1,
                uninit: false
            }
        );
        assert!(m.unknown.is_empty());
    }

    #[test]
    fn block_map_wild_pointer_aborts() {
        let mut ib = vec![0u8; 60];
        ib[0..4].copy_from_slice(&100u32.to_le_bytes());
        ib[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let m = map_blockmap(&ib, geo(), 20, &|_| None);
        assert_eq!(m.runs.len(), 1);
        assert_eq!(m.unknown, vec![(1, 20)]);
        // Unreadable indirect block: only its span is unknown.
        let mut ib = vec![0u8; 60];
        ib[48..52].copy_from_slice(&700u32.to_le_bytes());
        let m = map_blockmap(&ib, geo(), 100, &|_| None);
        assert_eq!(m.unknown, vec![(12, 100)]);
    }
}
