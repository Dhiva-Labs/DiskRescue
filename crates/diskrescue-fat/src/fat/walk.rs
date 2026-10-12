//! Directory tree walk, deleted-entry recovery and the orphan directory
//! scan for FAT volumes. See the parent module docs for the rules.

use std::collections::HashSet;

use diskrescue_core::fs::{EntryConfidence, FileEntry, WalkControl};
use diskrescue_core::{Range, RangeSet, Result};

use super::dir::{
    parse_dir, sniff_dir, DirData, ParsedDir, RawItem, ATTR_HIDDEN, ATTR_READ_ONLY, ATTR_SYSTEM,
};
use super::FatFs;
use crate::common::{
    allocated_clusters, contiguous_runs, for_each_state, orphan_dir_entry, orphan_dir_id,
    orphans_root_entry, runs_to_extents, scan_heads, ClusterMap, ClusterState, Emitter, MAX_DEPTH,
    MAX_DIR_BYTES, MAX_ORPHAN_DIRS, ORPHANS_ID,
};
use crate::util::{div_ceil, is_media_name, join_path, looks_like_media, sanitize_name};

/// Where a directory's entries come from, and how far its FAT is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Reachable from the root through live entries.
    Live,
    /// Inside a deleted directory.
    Deleted,
    /// Inside an orphan directory found in free space.
    OrphanFree,
    /// Inside an allocated but unreachable directory.
    OrphanLost,
}

impl Ctx {
    fn trust_fat(self) -> bool {
        matches!(self, Ctx::Live | Ctx::OrphanLost)
    }

    fn is_orphan(self) -> bool {
        matches!(self, Ctx::OrphanFree | Ctx::OrphanLost)
    }
}

/// How to load a directory.
#[derive(Debug, Clone, Copy)]
enum DirSrc {
    /// FAT12/16 fixed root region `(offset, length)`.
    Fixed(u64, u64),
    /// Follow the FAT from this cluster.
    Chain(u32),
    /// This free cluster and following free clusters while directory-like.
    Contiguous(u32),
}

struct Walker<'a, 'b> {
    fs: &'a FatFs,
    em: Emitter<'b>,
    /// Every directory cluster already listed.
    dir_clusters: RangeSet,
    /// Clusters claimed by live files.
    claimed: RangeSet,
}

fn add_runs(set: &mut RangeSet, runs: &[(u32, u32)]) {
    for &(s, n) in runs {
        set.insert(Range::new(s as u64, s as u64 + n as u64));
    }
}

/// Load the root directory and return its label entry, if any.
pub(super) fn root_label(fs: &FatFs) -> Option<String> {
    let mut ignore = |_: &FileEntry| WalkControl::Continue;
    let mut w = Walker {
        fs,
        em: Emitter::new(&mut ignore, 0),
        dir_clusters: RangeSet::new(),
        claimed: RangeSet::new(),
    };
    let (data, _) = w.load(root_src(fs))?;
    let p = parse_dir(&data, fs.is_fat32(), fs.max_cluster());
    p.label
}

fn root_src(fs: &FatFs) -> DirSrc {
    match fs.geometry().root_region {
        Some((off, len)) => DirSrc::Fixed(off, len),
        None => DirSrc::Chain(fs.geometry().root_clus),
    }
}

/// Walk the whole volume.
pub(super) fn run(fs: &FatFs, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
    let mut w = Walker {
        fs,
        em: Emitter::new(visit, fs.opts().max_entries),
        dir_clusters: RangeSet::new(),
        claimed: RangeSet::new(),
    };
    w.walk_dir(root_src(fs), None, "", Ctx::Live, 0);
    if !w.em.stopped && fs.opts().scan_orphans && fs.opts().include_deleted {
        w.scan_orphans();
    }
    Ok(())
}

impl<'a, 'b> Walker<'a, 'b> {
    fn cs(&self) -> u64 {
        self.fs.cluster_bytes()
    }

    fn max_dir_clusters(&self) -> u64 {
        (MAX_DIR_BYTES / self.cs()).max(1)
    }

    fn head(&self, c: u32) -> Option<Vec<u8>> {
        let len = self.cs().min(512) as usize;
        self.fs.meta().read_uncached(self.fs.offset_of(c), len)
    }

    fn sniff(&self, bytes: &[u8]) -> super::dir::Sniff {
        sniff_dir(bytes, self.fs.is_fat32(), self.fs.max_cluster())
    }

    fn read_runs(&mut self, runs: &[(u32, u32)]) -> DirData {
        let cs = self.cs();
        let mut data = DirData {
            bytes: Vec::new(),
            segments: Vec::new(),
            bad: RangeSet::new(),
        };
        for &(s, n) in runs {
            let off = self.fs.offset_of(s);
            let len = n as u64 * cs;
            let base = data.bytes.len() as u64;
            let (bytes, bad) = self.fs.meta().read_vec(off, len as usize);
            for r in bad.iter() {
                data.bad
                    .insert(Range::new(base + (r.start - off), base + (r.end - off)));
            }
            data.bytes.extend_from_slice(&bytes);
            data.segments.push((base, off, len));
        }
        add_runs(&mut self.dir_clusters, runs);
        data
    }

    /// Load a directory's bytes. Returns `None` when there is nothing
    /// directory-like to read.
    fn load(&mut self, src: DirSrc) -> Option<(DirData, Vec<String>)> {
        match src {
            DirSrc::Fixed(off, len) => {
                let (bytes, bad) = self.fs.meta().read_vec(off, len as usize);
                let bad = bad.shifted_down(off);
                Some((
                    DirData {
                        bytes,
                        segments: vec![(0, off, len)],
                        bad,
                    },
                    Vec::new(),
                ))
            }
            DirSrc::Chain(first) => {
                let ch = self
                    .fs
                    .table()
                    .follow(first, self.max_dir_clusters(), false);
                let mut notes = ch.notes;
                // Cut at a cluster that is already listed (cross-link).
                let mut runs = Vec::new();
                'outer: for &(s, n) in &ch.runs {
                    for k in 0..n {
                        let c = s + k;
                        if self.dir_clusters.contains(c as u64) {
                            notes.push(format!(
                                "directory chain reaches cluster {c}, which belongs to another directory; cut there"
                            ));
                            if k > 0 {
                                runs.push((s, k));
                            }
                            break 'outer;
                        }
                    }
                    runs.push((s, n));
                }
                if runs.is_empty() {
                    return None;
                }
                Some((self.read_runs(&runs), notes))
            }
            DirSrc::Contiguous(first) => {
                if !self.fs.valid(first) || self.dir_clusters.contains(first as u64) {
                    return None;
                }
                let head = self.head(first)?;
                let sn = self.sniff(&head);
                if !sn.is_dir || sn.dot.is_some_and(|d| d != first) {
                    return None;
                }
                let want = match self.fs.state(first) {
                    ClusterState::Unknown => ClusterState::Unknown,
                    _ => ClusterState::Free,
                };
                let mut count = 1u32;
                let limit = self
                    .max_dir_clusters()
                    .min(2 * 1024 * 1024 / self.cs())
                    .max(1);
                loop {
                    let c = first as u64 + count as u64;
                    if count as u64 >= limit
                        || c > self.fs.max_cluster() as u64
                        || self.dir_clusters.contains(c)
                        || self.fs.state(c as u32) != want
                    {
                        break;
                    }
                    let Some(h) = self.head(c as u32) else { break };
                    let s = self.sniff(&h);
                    if !s.is_dir || s.dot.is_some() {
                        break;
                    }
                    count += 1;
                }
                Some((self.read_runs(&[(first, count)]), Vec::new()))
            }
        }
    }

    fn walk_dir(&mut self, src: DirSrc, parent: Option<u64>, path: &str, ctx: Ctx, depth: usize) {
        if self.em.stopped {
            return;
        }
        let Some((data, notes)) = self.load(src) else {
            return;
        };
        let parsed: ParsedDir = parse_dir(&data, self.fs.is_fat32(), self.fs.max_cluster());
        drop(data);
        let held = parsed.items.len();
        if !self.em.reserve(held) {
            return;
        }
        for n in notes.iter().chain(parsed.notes.iter()) {
            log::warn!("{}: {n}", if path.is_empty() { "/" } else { path });
        }
        for item in parsed.items {
            if self.em.stopped {
                return;
            }
            let deleted = item.deleted;
            if deleted && !self.fs.opts().include_deleted {
                continue;
            }
            let name = sanitize_name(&item.name);
            let entry_path = join_path(path, &name);
            let mut flags = Vec::new();
            if item.attr & ATTR_HIDDEN != 0 {
                flags.push("hidden".to_string());
            }
            if item.attr & ATTR_SYSTEM != 0 {
                flags.push("system".to_string());
            }
            if item.attr & ATTR_READ_ONLY != 0 {
                flags.push("read-only".to_string());
            }
            if ctx.is_orphan() {
                flags.push("orphan".to_string());
            }
            if ctx == Ctx::Deleted && !deleted {
                flags.push("parent directory was deleted".to_string());
            }
            flags.extend(item.notes.iter().cloned());
            let is_dir = item.is_dir();
            let (extents, confidence, sub) = if is_dir {
                self.plan_dir(&item, ctx, &mut flags)
            } else {
                let (e, c) = self.plan_file(&item, ctx, &mut flags);
                (e, c, None)
            };
            let entry = FileEntry {
                id: item.id,
                parent_id: parent,
                path: entry_path,
                name,
                size: if is_dir { 0 } else { item.size as u64 },
                is_dir,
                deleted,
                confidence,
                created: item.created,
                modified: item.modified,
                accessed: item.accessed,
                extents,
                resident: false,
                flags,
            };
            let ctl = self.em.emit(&entry);
            if ctl == WalkControl::Stop {
                return;
            }
            if let (true, WalkControl::Continue, Some(sub)) = (is_dir, ctl, sub) {
                if depth + 1 >= MAX_DEPTH {
                    log::warn!(
                        "{}: directory nesting deeper than {MAX_DEPTH}; not descending",
                        entry.path
                    );
                    continue;
                }
                let sub_ctx = if ctx.trust_fat() && !deleted {
                    ctx
                } else if ctx.is_orphan() {
                    Ctx::OrphanFree
                } else {
                    Ctx::Deleted
                };
                self.walk_dir(sub, Some(entry.id), &entry.path, sub_ctx, depth + 1);
            }
        }
        self.em.release(held);
    }

    /// Choose the first cluster of a deleted entry whose high word may have
    /// been zeroed (FAT32 volumes with more than 65535 clusters).
    fn resolve_first(
        &self,
        item: &RawItem,
        size: u64,
        is_dir: bool,
        flags: &mut Vec<String>,
    ) -> u32 {
        let lo = item.lo as u32;
        let max = self.fs.max_cluster();
        if !self.fs.is_fat32() || item.hi != 0 || max <= 0xFFFF {
            return item.first_cluster(self.fs.is_fat32());
        }
        if !is_dir && size == 0 {
            return lo;
        }
        let check = |c: u32| -> bool {
            match self.head(c) {
                Some(h) if is_dir => self.sniff(&h).dot == Some(c),
                Some(h) => looks_like_media(&h),
                None => false,
            }
        };
        let free = |c: u32| self.fs.valid(c) && self.fs.state(c) == ClusterState::Free;
        let lo_ok = free(lo);
        if lo_ok && check(lo) {
            return lo;
        }
        // A content signature only tells something about media files;
        // directories are checked through their self-pointing "." entry.
        let guess = is_dir || is_media_name(&item.name);
        for h in (1..=(max >> 16)).filter(|_| guess) {
            let c = (h << 16) | lo;
            if c != lo && free(c) && check(c) {
                flags.push(format!(
                    "first cluster high word was zeroed on delete; cluster {c} chosen because {}",
                    if is_dir {
                        "its '.' entry points to itself"
                    } else {
                        "it starts with a media signature"
                    }
                ));
                return c;
            }
        }
        if !lo_ok && lo != 0 {
            flags.push("first cluster may be incomplete (high word zeroed on delete)".to_string());
        }
        lo
    }

    fn plan_file(
        &mut self,
        item: &RawItem,
        ctx: Ctx,
        flags: &mut Vec<String>,
    ) -> (Vec<Range>, EntryConfidence) {
        let fs = self.fs;
        let size = item.size as u64;
        let cs = self.cs();
        let needed = div_ceil(size, cs);
        if ctx.trust_fat() && !item.deleted {
            let first = item.first_cluster(fs.is_fat32());
            if size == 0 {
                if first != 0 {
                    flags.push(format!("empty file still points to cluster {first}"));
                }
                return (Vec::new(), EntryConfidence::Allocated);
            }
            if first == 0 {
                flags
                    .push("non-empty file has no first cluster; data location unknown".to_string());
                return (Vec::new(), EntryConfidence::Allocated);
            }
            let ch = fs.table().follow(first, needed, true);
            flags.extend(ch.notes.iter().cloned());
            let mut runs = ch.runs.clone();
            if ch.broken && ch.clusters < needed {
                if let Some(last) = ch.last() {
                    let (more, _) =
                        contiguous_runs(fs, last.saturating_add(1), needed - ch.clusters);
                    if !more.is_empty() {
                        flags.push(
                            "remainder after the broken cluster chain assumed contiguous"
                                .to_string(),
                        );
                        runs.extend(more);
                    }
                }
            }
            add_runs(&mut self.claimed, &runs);
            return (
                runs_to_extents(fs, &runs, Some(size)),
                EntryConfidence::Allocated,
            );
        }
        // Deleted, or inside a deleted/orphan directory: the chain is gone.
        if size == 0 {
            return (Vec::new(), EntryConfidence::DeletedIntact);
        }
        let first = if item.deleted {
            self.resolve_first(item, size, false, flags)
        } else {
            item.first_cluster(fs.is_fat32())
        };
        if first == 0 {
            flags.push("data location lost (first cluster is 0)".to_string());
            return (Vec::new(), EntryConfidence::DeletedNoData);
        }
        if !fs.valid(first) {
            flags.push(format!(
                "first cluster {first} is outside the volume; data location lost"
            ));
            return (Vec::new(), EntryConfidence::DeletedNoData);
        }
        let (runs, clipped) = contiguous_runs(fs, first, needed);
        flags.push("extents guessed (assumed contiguous)".to_string());
        if clipped {
            flags.push("assumed extents run past the last cluster; clipped".to_string());
        }
        let reused = allocated_clusters(fs, &runs);
        if reused > 0 {
            flags.push(format!(
                "{reused} of {} clusters are now allocated to another file",
                runs.iter().map(|r| r.1 as u64).sum::<u64>()
            ));
        }
        (
            runs_to_extents(fs, &runs, Some(size)),
            EntryConfidence::DeletedGuessed,
        )
    }

    fn plan_dir(
        &mut self,
        item: &RawItem,
        ctx: Ctx,
        flags: &mut Vec<String>,
    ) -> (Vec<Range>, EntryConfidence, Option<DirSrc>) {
        let fs = self.fs;
        if ctx.trust_fat() && !item.deleted {
            let first = item.first_cluster(fs.is_fat32());
            if !fs.valid(first) {
                flags.push(format!("directory points to invalid cluster {first}"));
                return (Vec::new(), EntryConfidence::Allocated, None);
            }
            if self.dir_clusters.contains(first as u64) {
                flags.push(
                    "directory already listed elsewhere (cross-linked or looping)".to_string(),
                );
                return (Vec::new(), EntryConfidence::Allocated, None);
            }
            let ch = fs.table().follow(first, self.max_dir_clusters(), false);
            let ext = runs_to_extents(fs, &ch.runs, None);
            return (ext, EntryConfidence::Allocated, Some(DirSrc::Chain(first)));
        }
        let first = if item.deleted {
            self.resolve_first(item, 0, true, flags)
        } else {
            item.first_cluster(fs.is_fat32())
        };
        if !fs.valid(first) {
            flags.push(format!(
                "directory cluster {first} is outside the volume; contents lost"
            ));
            return (Vec::new(), EntryConfidence::DeletedNoData, None);
        }
        let one = vec![Range::with_len(fs.offset_of(first), self.cs())];
        if self.dir_clusters.contains(first as u64) {
            flags.push("directory cluster already listed elsewhere".to_string());
            return (one, EntryConfidence::DeletedGuessed, None);
        }
        if fs.state(first) != ClusterState::Free {
            flags.push(
                "directory cluster is now allocated to another file; contents not listed"
                    .to_string(),
            );
            return (one, EntryConfidence::DeletedGuessed, None);
        }
        let looks = self
            .head(first)
            .map(|h| {
                let s = self.sniff(&h);
                s.is_dir && s.dot.is_none_or(|d| d == first)
            })
            .unwrap_or(false);
        if !looks {
            flags.push(
                "directory cluster no longer holds directory entries; contents lost".to_string(),
            );
            return (one, EntryConfidence::DeletedGuessed, None);
        }
        (
            one,
            EntryConfidence::DeletedGuessed,
            Some(DirSrc::Contiguous(first)),
        )
    }

    /// Find directory clusters outside the walked tree and list them under
    /// `$orphans/`.
    fn scan_orphans(&mut self) {
        let fs = self.fs;
        let lost_ok = !self.em.skipped_any;
        let src_size = fs.meta().size();
        let cs = self.cs();
        let mut cands: Vec<(u32, bool)> = Vec::new();
        let mut truncated = false;
        let mut todo: Vec<(u32, bool)> = Vec::new();
        for_each_state(fs, |c, s| {
            if fs.offset_of(c).saturating_add(cs) > src_size {
                return false;
            }
            let lost = lost_ok
                && s == ClusterState::Allocated
                && !self.claimed.contains(c as u64)
                && !self.dir_clusters.contains(c as u64);
            // Free clusters, and clusters whose FAT entry is unreadable.
            let free = matches!(s, ClusterState::Free | ClusterState::Unknown)
                && !self.dir_clusters.contains(c as u64);
            if free || lost {
                todo.push((c, lost));
            }
            true
        });
        let fat32 = fs.is_fat32();
        let max = fs.max_cluster();
        for chunk in todo.chunks(65_536) {
            let clusters: Vec<u32> = chunk.iter().map(|b| b.0).collect();
            let lost: HashSet<u32> = chunk.iter().filter(|b| b.1).map(|b| b.0).collect();
            scan_heads(fs.meta(), fs, &clusters, 512, |c, head| {
                if cands.len() < MAX_ORPHAN_DIRS && sniff_dir(head, fat32, max).is_dir {
                    cands.push((c, lost.contains(&c)));
                }
            });
            if cands.len() >= MAX_ORPHAN_DIRS {
                truncated = true;
                break;
            }
        }
        drop(todo);
        if truncated {
            log::warn!(
                "more than {MAX_ORPHAN_DIRS} orphan directory clusters; the rest is ignored"
            );
        }
        if cands.is_empty() {
            return;
        }
        // Which candidates are children or continuations of others?
        let cand_set: HashSet<u32> = cands.iter().map(|c| c.0).collect();
        let mut referenced: HashSet<u32> = HashSet::new();
        for &(c, _) in &cands {
            let (bytes, bad) = fs.meta().read_vec(fs.offset_of(c), cs as usize);
            let off = fs.offset_of(c);
            let data = DirData {
                bytes,
                segments: vec![(0, off, cs)],
                bad: bad.shifted_down(off),
            };
            let p = parse_dir(&data, fs.is_fat32(), fs.max_cluster());
            for it in &p.items {
                if it.is_dir() {
                    let f = it.first_cluster(fs.is_fat32());
                    if f != c && cand_set.contains(&f) {
                        referenced.insert(f);
                    }
                }
            }
            if let Some(pc) = p.dotdot {
                if pc != c && cand_set.contains(&pc) {
                    referenced.insert(c);
                }
            }
            if p.dot.is_none() && c > 2 && cand_set.contains(&(c - 1)) {
                referenced.insert(c);
            }
        }
        let mut root_emitted = false;
        for pass in 0..2 {
            for &(c, lost) in &cands {
                if self.em.stopped {
                    return;
                }
                if (pass == 0 && referenced.contains(&c)) || self.dir_clusters.contains(c as u64) {
                    continue;
                }
                if !root_emitted {
                    root_emitted = true;
                    match self.em.emit(&orphans_root_entry()) {
                        WalkControl::Continue => {}
                        _ => return,
                    }
                }
                let e = orphan_dir_entry(fs, c, lost);
                match self.em.emit(&e) {
                    WalkControl::Stop => return,
                    WalkControl::SkipChildren => {
                        self.dir_clusters.insert(Range::new(c as u64, c as u64 + 1));
                        continue;
                    }
                    WalkControl::Continue => {}
                }
                let (src, ctx) = if lost {
                    (DirSrc::Chain(c), Ctx::OrphanLost)
                } else {
                    (DirSrc::Contiguous(c), Ctx::OrphanFree)
                };
                debug_assert_eq!(e.parent_id, Some(ORPHANS_ID));
                self.walk_dir(src, Some(orphan_dir_id(c)), &e.path, ctx, 2);
                // Make sure a candidate that failed to load is not retried.
                self.dir_clusters.insert(Range::new(c as u64, c as u64 + 1));
            }
        }
    }
}
