//! File Allocation Table access shared by FAT12/16/32 and exFAT.
//!
//! Entries are read lazily through the [`MetaReader`] page cache. Every
//! lookup tries the preferred FAT copy first (FAT1, or the active FAT when
//! FAT32 mirroring is disabled) and falls back to the other copy when the
//! bytes of the entry lie in an unreadable sector. A lookup whose bytes are
//! unreadable in every copy returns `None`.
//!
//! Chains are followed with a visited set (stored as cluster ranges, so
//! contiguous chains cost almost nothing) and a hard step limit, so a
//! looping or absurdly long chain can neither hang nor exhaust memory.

use std::collections::BTreeMap;
use std::sync::Arc;

use diskrescue_core::Range;

use crate::io::MetaReader;

/// Width and value encoding of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FatWidth {
    /// 12-bit packed entries.
    Fat12,
    /// 16-bit entries.
    Fat16,
    /// 32-bit entries with the top 4 bits reserved.
    Fat32,
    /// exFAT: full 32-bit entries.
    ExFat,
}

/// Decoded meaning of one FAT entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FatEntry {
    /// 0: free (on exFAT also "not described by the FAT").
    Free,
    /// Next cluster of the chain.
    Next(u32),
    /// End of chain.
    Eoc,
    /// Bad cluster marker.
    Bad,
    /// Reserved value or a cluster number outside the volume.
    Invalid(u32),
}

/// Runs of consecutive clusters `(first, count)` in file order.
pub(crate) type Runs = Vec<(u32, u32)>;

/// Outcome of following a chain.
#[derive(Debug, Clone, Default)]
pub(crate) struct Chain {
    /// Runs of consecutive clusters `(first, count)` in chain order.
    pub runs: Vec<(u32, u32)>,
    /// Number of clusters collected.
    pub clusters: u64,
    /// The chain stopped at something other than an end-of-chain marker
    /// or the requested length (free/bad/invalid/unreadable entry, loop).
    pub broken: bool,
    /// Human-readable notes about anything unusual.
    pub notes: Vec<String>,
}

impl Chain {
    /// Last cluster collected.
    pub(crate) fn last(&self) -> Option<u32> {
        self.runs.last().map(|&(s, n)| s + (n - 1))
    }
}

/// Append cluster `c` to a run list, extending the last run when contiguous.
pub(crate) fn push_run(runs: &mut Vec<(u32, u32)>, c: u32) {
    if let Some(last) = runs.last_mut() {
        if last.0 as u64 + last.1 as u64 == c as u64 {
            last.1 += 1;
            return;
        }
    }
    runs.push((c, 1));
}

/// A set of cluster numbers kept as disjoint runs in a B-tree, so inserts
/// and lookups cost O(log runs) whatever the insertion order. (A sorted
/// vector of ranges degrades to quadratic time when a damaged FAT chain
/// walks backwards through every other cluster.)
#[derive(Debug, Clone, Default)]
pub(crate) struct ClusterSet {
    /// Run start -> run end (exclusive).
    runs: BTreeMap<u64, u64>,
}

impl ClusterSet {
    /// Empty set.
    pub(crate) fn new() -> Self {
        ClusterSet::default()
    }

    /// True when cluster `c` is in the set.
    pub(crate) fn contains(&self, c: u64) -> bool {
        self.runs
            .range(..=c)
            .next_back()
            .is_some_and(|(_, &end)| c < end)
    }

    /// True when any cluster of `r` is in the set.
    pub(crate) fn intersects(&self, r: Range) -> bool {
        if r.is_empty() {
            return false;
        }
        if self.contains(r.start) {
            return true;
        }
        self.runs.range(r.start..r.end).next().is_some()
    }

    /// Add the clusters of `r`, merging with touching runs.
    pub(crate) fn insert(&mut self, r: Range) {
        if r.is_empty() {
            return;
        }
        let (mut start, mut end) = (r.start, r.end);
        if let Some((&ps, &pe)) = self.runs.range(..=start).next_back() {
            if pe >= start {
                start = ps;
                end = end.max(pe);
                self.runs.remove(&ps);
            }
        }
        while let Some((&ns, &ne)) = self.runs.range(start..).next() {
            if ns > end {
                break;
            }
            end = end.max(ne);
            self.runs.remove(&ns);
        }
        self.runs.insert(start, end);
    }
}

/// Lazily read FAT.
pub(crate) struct FatTable {
    meta: Arc<MetaReader>,
    width: FatWidth,
    copies: Vec<u64>,
    fat_bytes: u64,
    entries: u64,
    max_cluster: u32,
}

impl FatTable {
    /// `copies` are the volume byte offsets of each FAT copy, preferred
    /// first; `fat_bytes` is the size of one copy; `max_cluster` the highest
    /// valid cluster number.
    pub(crate) fn new(
        meta: Arc<MetaReader>,
        width: FatWidth,
        copies: Vec<u64>,
        fat_bytes: u64,
        max_cluster: u32,
    ) -> Self {
        let mut t = FatTable {
            meta,
            width,
            copies,
            fat_bytes,
            entries: 0,
            max_cluster,
        };
        let mut n = match width {
            FatWidth::Fat12 => fat_bytes.saturating_mul(2) / 3 + 1,
            FatWidth::Fat16 => fat_bytes / 2,
            FatWidth::Fat32 | FatWidth::ExFat => fat_bytes / 4,
        };
        while n > 0 && t.span_u64(n - 1).is_none() {
            n -= 1;
        }
        t.entries = n;
        t
    }

    /// Byte offset of entry `c` inside one FAT and the number of bytes it
    /// occupies, or `None` when it lies outside the table.
    fn span(&self, c: u32) -> Option<(u64, usize)> {
        self.span_u64(c as u64)
    }

    fn span_u64(&self, c: u64) -> Option<(u64, usize)> {
        let (off, len) = match self.width {
            FatWidth::Fat12 => (c + c / 2, 2),
            FatWidth::Fat16 => (c * 2, 2),
            FatWidth::Fat32 | FatWidth::ExFat => (c * 4, 4),
        };
        if off.saturating_add(len as u64) > self.fat_bytes {
            None
        } else {
            Some((off, len))
        }
    }

    fn decode(&self, c: u32, b: &[u8]) -> u32 {
        match self.width {
            FatWidth::Fat12 => {
                let v = u16::from_le_bytes([b[0], b[1]]);
                if c & 1 == 1 {
                    (v >> 4) as u32
                } else {
                    (v & 0x0FFF) as u32
                }
            }
            FatWidth::Fat16 => u16::from_le_bytes([b[0], b[1]]) as u32,
            FatWidth::Fat32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0x0FFF_FFFF,
            FatWidth::ExFat => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    }

    /// Raw value of the entry for cluster `c`, falling back to the other
    /// FAT copies when unreadable.
    ///
    /// A copy whose sector holding the entry is entirely 0xFF (an
    /// unprogrammed flash page exposed by a failing controller, or a
    /// reader's error fill) is not trusted while another copy still has
    /// real data there; when every copy is 0xFF there, the value stands.
    pub(crate) fn raw(&self, c: u32) -> Option<u32> {
        let (off, len) = self.span(c)?;
        let mut buf = [0u8; 4];
        let mut ff_value = None;
        for (i, &base) in self.copies.iter().enumerate() {
            let bad = self.meta.read(base + off, &mut buf[..len]);
            if bad.is_empty() {
                let v = self.decode(c, &buf);
                let last = i + 1 == self.copies.len();
                if !last && v == self.all_ones() && self.ff_sectors(base, off, len) {
                    ff_value.get_or_insert(v);
                    continue;
                }
                if i > 0 {
                    log::debug!("FAT entry {c} read from FAT copy {}", i + 1);
                }
                return Some(v);
            }
        }
        ff_value
    }

    /// The value every entry of a 0xFF-filled sector decodes to.
    fn all_ones(&self) -> u32 {
        match self.width {
            FatWidth::Fat12 => 0xFFF,
            FatWidth::Fat16 => 0xFFFF,
            FatWidth::Fat32 => 0x0FFF_FFFF,
            FatWidth::ExFat => 0xFFFF_FFFF,
        }
    }

    /// True when every 512-byte sector of FAT copy `base` touched by the
    /// entry at `off..off+len` is readable and entirely 0xFF.
    fn ff_sectors(&self, base: u64, off: u64, len: usize) -> bool {
        let first = off / 512;
        let last = (off + len as u64 - 1) / 512;
        (first..=last).all(|sec| {
            let s = sec * 512;
            let n = 512.min(self.fat_bytes.saturating_sub(s)) as usize;
            let (b, bad) = self.meta.read_vec(base + s, n);
            n > 0 && bad.is_empty() && b.iter().all(|&x| x == 0xFF)
        })
    }

    /// Classify a raw value.
    pub(crate) fn classify(&self, v: u32) -> FatEntry {
        let (bad, eoc) = match self.width {
            FatWidth::Fat12 => (0xFF7, 0xFF8),
            FatWidth::Fat16 => (0xFFF7, 0xFFF8),
            FatWidth::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
            FatWidth::ExFat => (0xFFFF_FFF7, 0xFFFF_FFF8),
        };
        if v == 0 {
            FatEntry::Free
        } else if v == bad {
            FatEntry::Bad
        } else if v >= eoc {
            FatEntry::Eoc
        } else if v >= 2 && v <= self.max_cluster {
            FatEntry::Next(v)
        } else {
            FatEntry::Invalid(v)
        }
    }

    /// Decoded entry for cluster `c`.
    pub(crate) fn entry(&self, c: u32) -> Option<FatEntry> {
        self.raw(c).map(|v| self.classify(v))
    }

    /// Raw values for clusters `first..first+count`, one bulk read per FAT
    /// copy. Entries unreadable in every copy are `None`.
    pub(crate) fn raw_batch(&self, first: u32, count: u32) -> Vec<Option<u32>> {
        let mut out = vec![None; count as usize];
        if count == 0 {
            return out;
        }
        let Some((start, _)) = self.span(first) else {
            return out;
        };
        let end = (first as u64 + count as u64).min(self.entries);
        if end <= first as u64 {
            return out;
        }
        let last = end - 1;
        let Some((end_off, end_len)) = self.span(last as u32) else {
            return out;
        };
        let len = (end_off + end_len as u64 - start) as usize;
        let mut pending: Vec<u32> = (first..=last as u32).collect();
        // Values from 0xFF-filled sectors, used only when no other copy
        // has real data for the entry (see `raw`).
        let mut ff_vals: Vec<Option<u32>> = Vec::new();
        let ones = self.all_ones();
        for (ci, &base) in self.copies.iter().enumerate() {
            if pending.is_empty() {
                break;
            }
            let last_copy = ci + 1 == self.copies.len();
            let (buf, bad) = self.meta.read_vec(base + start, len);
            let mut still = Vec::new();
            let mut memo: Option<(u64, u64, bool)> = None;
            for c in pending {
                let (off, l) = match self.span(c) {
                    Some(s) => s,
                    None => continue,
                };
                let rel = (off - start) as usize;
                if bad.intersects(Range::with_len(base + off, l as u64)) {
                    still.push(c);
                    continue;
                }
                let v = self.decode(c, &buf[rel..rel + l]);
                if !last_copy && v == ones {
                    let key = (off / 512, (off + l as u64 - 1) / 512);
                    let ff = match memo {
                        Some((a, b, f)) if (a, b) == key => f,
                        _ => {
                            let f = self.ff_sectors(base, off, l);
                            memo = Some((key.0, key.1, f));
                            f
                        }
                    };
                    if ff {
                        if ff_vals.is_empty() {
                            ff_vals = vec![None; count as usize];
                        }
                        ff_vals[(c - first) as usize].get_or_insert(v);
                        still.push(c);
                        continue;
                    }
                }
                out[(c - first) as usize] = Some(v);
            }
            pending = still;
        }
        if !ff_vals.is_empty() {
            for c in pending {
                let i = (c - first) as usize;
                if out[i].is_none() {
                    out[i] = ff_vals[i];
                }
            }
        }
        out
    }

    /// Follow the chain starting at `first`, collecting at most `limit`
    /// clusters. `size_limited` means `limit` is derived from a file size,
    /// so a chain that does not end exactly there earns a note.
    pub(crate) fn follow(&self, first: u32, limit: u64, size_limited: bool) -> Chain {
        let mut chain = Chain::default();
        if limit == 0 {
            return chain;
        }
        let mut visited = ClusterSet::new();
        let mut c = first;
        loop {
            if c < 2 || c > self.max_cluster {
                chain
                    .notes
                    .push(format!("cluster chain points to invalid cluster {c}"));
                chain.broken = true;
                break;
            }
            if visited.contains(c as u64) {
                chain
                    .notes
                    .push(format!("cluster chain loops back to cluster {c}"));
                chain.broken = true;
                break;
            }
            visited.insert(Range::new(c as u64, c as u64 + 1));
            push_run(&mut chain.runs, c);
            chain.clusters += 1;
            let e = self.entry(c);
            if chain.clusters >= limit {
                if size_limited && !matches!(e, Some(FatEntry::Eoc)) {
                    chain.notes.push(
                        "cluster chain is longer than the file size; cut at the file size"
                            .to_string(),
                    );
                }
                break;
            }
            match e {
                Some(FatEntry::Next(n)) => c = n,
                Some(FatEntry::Eoc) => {
                    if size_limited {
                        chain.notes.push(format!(
                            "cluster chain ends after {} clusters but the size needs {limit}",
                            chain.clusters
                        ));
                    }
                    break;
                }
                Some(FatEntry::Bad) => {
                    chain
                        .notes
                        .push(format!("cluster chain reaches a bad-cluster marker at {c}"));
                    chain.broken = true;
                    break;
                }
                Some(FatEntry::Free) => {
                    chain
                        .notes
                        .push(format!("cluster chain broken: cluster {c} is marked free"));
                    chain.broken = true;
                    break;
                }
                Some(FatEntry::Invalid(v)) => {
                    chain.notes.push(format!(
                        "cluster chain broken: invalid FAT value {v:#x} at cluster {c}"
                    ));
                    chain.broken = true;
                    break;
                }
                None => {
                    chain.notes.push(format!(
                        "FAT entry of cluster {c} is unreadable in every FAT copy"
                    ));
                    chain.broken = true;
                    break;
                }
            }
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use diskrescue_core::{RangeSet, SharedSource};

    fn table(width: FatWidth, fat: Vec<u8>, copies: usize, max: u32) -> FatTable {
        let fat_len = fat.len() as u64;
        let mut data = Vec::new();
        for _ in 0..copies {
            data.extend_from_slice(&fat);
        }
        let src: SharedSource = Arc::new(MemSource::new(data));
        let meta = Arc::new(MetaReader::new(src));
        let offs = (0..copies as u64).map(|i| i * fat_len).collect();
        FatTable::new(meta, width, offs, fat_len, max)
    }

    #[test]
    fn fat12_packing() {
        // Entries: 0=FF8, 1=FFF, 2=003, 3=FFF, 4=000, 5=FF7
        let vals = [0xFF8u16, 0xFFF, 0x003, 0xFFF, 0x000, 0xFF7];
        let mut fat = vec![0u8; 512];
        for (i, &v) in vals.iter().enumerate() {
            let off = i + i / 2;
            let cur = u16::from_le_bytes([fat[off], fat[off + 1]]);
            let new = if i & 1 == 1 {
                (cur & 0x000F) | (v << 4)
            } else {
                (cur & 0xF000) | (v & 0x0FFF)
            };
            fat[off..off + 2].copy_from_slice(&new.to_le_bytes());
        }
        let t = table(FatWidth::Fat12, fat, 1, 300);
        assert_eq!(t.entry(2), Some(FatEntry::Next(3)));
        assert_eq!(t.entry(3), Some(FatEntry::Eoc));
        assert_eq!(t.entry(4), Some(FatEntry::Free));
        assert_eq!(t.entry(5), Some(FatEntry::Bad));
        let b = t.raw_batch(0, 6);
        assert_eq!(b, vals.iter().map(|&v| Some(v as u32)).collect::<Vec<_>>());
        let ch = t.follow(2, 10, false);
        assert_eq!(ch.runs, vec![(2, 2)]);
        assert!(!ch.broken);
        // Entry beyond the table.
        assert_eq!(t.raw(341), None);
        assert_eq!(t.raw_batch(339, 4), vec![Some(0), Some(0), None, None]);
    }

    #[test]
    fn fat32_loops_and_masking() {
        let mut fat = vec![0u8; 64];
        let put = |fat: &mut Vec<u8>, c: usize, v: u32| {
            fat[c * 4..c * 4 + 4].copy_from_slice(&v.to_le_bytes())
        };
        put(&mut fat, 2, 0xF000_0003); // top bits ignored
        put(&mut fat, 3, 4);
        put(&mut fat, 4, 2); // loop
        let t = table(FatWidth::Fat32, fat, 2, 15);
        let ch = t.follow(2, 100, false);
        assert_eq!(ch.runs, vec![(2, 3)]);
        assert!(ch.broken);
        let ch = t.follow(2, 2, true);
        assert_eq!(ch.clusters, 2);
        assert!(ch.notes[0].contains("longer"));
    }

    #[test]
    fn cluster_set_merges_in_any_order() {
        let mut s = ClusterSet::new();
        for c in (10..40u64).rev().step_by(2) {
            s.insert(Range::new(c, c + 1));
        }
        assert!(s.contains(39) && s.contains(11) && !s.contains(38) && !s.contains(10));
        for c in (10..40u64).step_by(2) {
            s.insert(Range::new(c, c + 1));
        }
        assert_eq!(s.runs.len(), 1);
        assert!(s.contains(10) && s.contains(39) && !s.contains(40) && !s.contains(9));
        s.insert(Range::new(50, 60));
        s.insert(Range::new(45, 52));
        s.insert(Range::new(40, 45));
        assert_eq!(s.runs.len(), 1);
        assert!(s.intersects(Range::new(0, 11)));
        assert!(s.intersects(Range::new(59, 70)));
        assert!(!s.intersects(Range::new(60, 70)));
        assert!(!s.intersects(Range::new(0, 10)));
    }

    #[test]
    fn ff_sector_falls_back_to_other_copy() {
        // FAT16, 2 copies; copy 1's first sector is 0xFF, copy 2 is real.
        let mut fat = vec![0u8; 1024];
        fat[0..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]);
        fat[4..6].copy_from_slice(&3u16.to_le_bytes());
        fat[6..8].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let mut data = vec![0xFFu8; 512];
        data.extend_from_slice(&fat[512..]);
        data.extend_from_slice(&fat);
        let src: SharedSource = Arc::new(MemSource::new(data.clone()));
        let meta = Arc::new(MetaReader::new(src));
        let t = FatTable::new(meta, FatWidth::Fat16, vec![0, 1024], 1024, 400);
        assert_eq!(t.entry(2), Some(FatEntry::Next(3)));
        assert_eq!(t.entry(3), Some(FatEntry::Eoc));
        assert_eq!(t.entry(4), Some(FatEntry::Free));
        let b = t.raw_batch(2, 4);
        assert_eq!(b, vec![Some(3), Some(0xFFFF), Some(0), Some(0)]);
        // Both copies 0xFF there: the value stands (a legitimate run of
        // end-of-chain markers looks the same).
        let mut both = vec![0xFFu8; 512];
        both.extend_from_slice(&fat[512..]);
        let mut d2 = both.clone();
        d2.extend_from_slice(&both);
        let src: SharedSource = Arc::new(MemSource::new(d2));
        let meta = Arc::new(MetaReader::new(src));
        let t = FatTable::new(meta, FatWidth::Fat16, vec![0, 1024], 1024, 400);
        assert_eq!(t.entry(2), Some(FatEntry::Eoc));
        assert_eq!(t.raw_batch(2, 2), vec![Some(0xFFFF), Some(0xFFFF)]);
    }

    #[test]
    fn falls_back_to_second_copy() {
        let mut fat = vec![0u8; 1024];
        fat[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        let mut data = fat.clone();
        data.extend_from_slice(&fat);
        let src: SharedSource = Arc::new(
            MemSource::new(data).with_unreadable(RangeSet::from_range(Range::new(0, 512))),
        );
        let meta = Arc::new(MetaReader::new(src));
        let t = FatTable::new(meta, FatWidth::Fat32, vec![0, 1024], 1024, 200);
        assert_eq!(t.entry(2), Some(FatEntry::Eoc));
        assert_eq!(t.raw_batch(1, 3), vec![Some(0), Some(0x0FFF_FFFF), Some(0)]);
    }
}
