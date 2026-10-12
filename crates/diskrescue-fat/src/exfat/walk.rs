//! Directory tree walk, deleted-set recovery and the orphan directory scan
//! for exFAT volumes. See the parent module docs for the rules.

use std::collections::HashSet;

use diskrescue_core::fs::{EntryConfidence, FileEntry, WalkControl};
use diskrescue_core::{Range, RangeSet, Result};

use super::dir::{parse_dir, sniff_dir, SetItem};
use super::ExFatFs;
use crate::common::{
    allocated_clusters, contiguous_runs, for_each_state, orphan_dir_entry, orphan_dir_id,
    orphans_root_entry, runs_to_extents, scan_heads, ClusterMap, ClusterState, Emitter, MAX_DEPTH,
    MAX_DIR_BYTES, MAX_ORPHAN_DIRS, VDL_FLAG,
};
use crate::table::FatEntry;
use crate::util::{div_ceil, join_path, sanitize_name};

/// Where a directory's entries come from, and how far its metadata is
/// trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Live,
    Deleted,
    OrphanFree,
    OrphanLost,
}

impl Ctx {
    fn trust(self) -> bool {
        matches!(self, Ctx::Live | Ctx::OrphanLost)
    }

    fn is_orphan(self) -> bool {
        matches!(self, Ctx::OrphanFree | Ctx::OrphanLost)
    }
}

/// How to load a directory.
#[derive(Debug, Clone, Copy)]
enum DirSrc {
    /// Follow the FAT for at most this many clusters.
    Chain(u32, u64),
    /// Exactly these contiguous clusters.
    Contiguous(u32, u64),
    /// This cluster and following clusters in the same allocation state
    /// while they still hold entry sets (orphans).
    Scan(u32),
}

/// Bytes of a cluster head sniffed for entry sets.
const HEAD: usize = 4096;

struct Walker<'a, 'b> {
    fs: &'a ExFatFs,
    em: Emitter<'b>,
    dir_clusters: RangeSet,
    claimed: RangeSet,
    /// First clusters of directories named by entries of orphan candidates:
    /// an orphan scan never extends a directory into one of them.
    dir_starts: HashSet<u32>,
}

fn add_runs(set: &mut RangeSet, runs: &[(u32, u32)]) {
    for &(s, n) in runs {
        set.insert(Range::new(s as u64, s as u64 + n as u64));
    }
}

/// Walk the whole volume.
pub(super) fn run(fs: &ExFatFs, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
    let mut w = Walker {
        fs,
        em: Emitter::new(visit, fs.opts().max_entries),
        dir_clusters: RangeSet::new(),
        claimed: RangeSet::new(),
        dir_starts: HashSet::new(),
    };
    add_runs(&mut w.claimed, fs.system_runs());
    let root = fs.root_cluster();
    if fs.valid(root) {
        let limit = w.max_dir_clusters();
        w.walk_dir(DirSrc::Chain(root, limit), None, "", Ctx::Live, 0);
    }
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
        let len = self.cs().min(HEAD as u64) as usize;
        self.fs.meta().read_uncached(self.fs.offset_of(c), len)
    }

    /// Drop clusters already listed as directories from `runs`, cutting
    /// at the first one (cross-link protection).
    fn cut_visited(&self, runs: &[(u32, u32)], notes: &mut Vec<String>) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for &(s, n) in runs {
            for k in 0..n {
                if self.dir_clusters.contains((s + k) as u64) {
                    notes.push(format!(
                        "directory reaches cluster {}, which belongs to another directory; cut there",
                        s + k
                    ));
                    if k > 0 {
                        out.push((s, k));
                    }
                    return out;
                }
            }
            out.push((s, n));
        }
        out
    }

    fn load(&mut self, src: DirSrc) -> Option<(crate::common::DirData, Vec<String>)> {
        let fs = self.fs;
        let mut notes = Vec::new();
        let runs = match src {
            DirSrc::Chain(first, limit) => {
                let ch = fs.follow(first, limit.min(self.max_dir_clusters()), false);
                notes.extend(ch.notes);
                ch.runs
            }
            DirSrc::Contiguous(first, n) => {
                let (r, clipped) = contiguous_runs(fs, first, n.min(self.max_dir_clusters()));
                if clipped {
                    notes.push("directory runs past the last cluster; clipped".to_string());
                }
                r
            }
            DirSrc::Scan(first) => {
                if !fs.valid(first) || self.dir_clusters.contains(first as u64) {
                    return None;
                }
                if !self.head(first).is_some_and(|h| sniff_dir(&h)) {
                    return None;
                }
                let want = fs.state(first);
                let lost = want == ClusterState::Allocated;
                let limit = self
                    .max_dir_clusters()
                    .min((8 * 1024 * 1024 / self.cs()).max(1));
                let mut count = 1u64;
                while count < limit {
                    let c = first as u64 + count;
                    if c > fs.max_cluster() as u64
                        || self.dir_clusters.contains(c)
                        || fs.state(c as u32) != want
                        || (lost && self.claimed.contains(c))
                        || self.dir_starts.contains(&(c as u32))
                        || !self.head(c as u32).is_some_and(|h| sniff_dir(&h))
                    {
                        break;
                    }
                    count += 1;
                }
                vec![(first, count as u32)]
            }
        };
        let runs = self.cut_visited(&runs, &mut notes);
        if runs.is_empty() {
            return None;
        }
        add_runs(&mut self.dir_clusters, &runs);
        Some((fs.read_dir_runs(&runs), notes))
    }

    fn walk_dir(&mut self, src: DirSrc, parent: Option<u64>, path: &str, ctx: Ctx, depth: usize) {
        if self.em.stopped {
            return;
        }
        let Some((data, notes)) = self.load(src) else {
            return;
        };
        let parsed = parse_dir(&data, self.fs.upcase());
        drop(data);
        let held = parsed.items.len();
        if !self.em.reserve(held) {
            return;
        }
        for n in notes.iter().chain(parsed.notes.iter()) {
            log::warn!("{}: {n}", if path.is_empty() { "/" } else { path });
        }
        let heap_bytes = self.fs.cluster_count() as u64 * self.cs();
        for mut item in parsed.items {
            if self.em.stopped {
                return;
            }
            let mut clip_note = None;
            if item.data_length > heap_bytes {
                clip_note = Some(format!(
                    "DataLength {} exceeds the cluster heap; clipped to {heap_bytes}",
                    item.data_length
                ));
                item.data_length = heap_bytes;
                item.valid_data_length = item.valid_data_length.min(heap_bytes);
            }
            let deleted = item.deleted;
            if deleted && !self.fs.opts().include_deleted {
                continue;
            }
            let name = sanitize_name(&item.name);
            let entry_path = join_path(path, &name);
            let mut flags = Vec::new();
            if item.attrs & 0x02 != 0 {
                flags.push("hidden".to_string());
            }
            if item.attrs & 0x04 != 0 {
                flags.push("system".to_string());
            }
            if item.attrs & 0x01 != 0 {
                flags.push("read-only".to_string());
            }
            if ctx.is_orphan() {
                flags.push("orphan".to_string());
            }
            if ctx == Ctx::Deleted && !deleted {
                flags.push("parent directory was deleted".to_string());
            }
            flags.extend(item.notes.iter().cloned());
            flags.extend(clip_note);
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
                size: if is_dir { 0 } else { item.data_length },
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
                let sub_ctx = if ctx.trust() && !deleted {
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

    /// Cluster runs of a stream from its metadata.
    fn stream_runs(
        &self,
        item: &SetItem,
        n: u64,
        flags: &mut Vec<String>,
        exact_chain_only: bool,
    ) -> Vec<(u32, u32)> {
        let fs = self.fs;
        let first = item.first_cluster;
        if item.no_fat_chain() {
            let (runs, clipped) = contiguous_runs(fs, first, n);
            if clipped {
                flags.push("extents run past the last cluster; clipped".to_string());
            }
            return runs;
        }
        let ch = fs.follow(first, n, true);
        if exact_chain_only {
            if !ch.broken && ch.clusters == n && ch.notes.is_empty() {
                flags.push(
                    "cluster chain of the deleted file is still intact in the FAT".to_string(),
                );
                return ch.runs;
            }
            flags.push("extents guessed (assumed contiguous)".to_string());
            let (runs, clipped) = contiguous_runs(fs, first, n);
            if clipped {
                flags.push("assumed extents run past the last cluster; clipped".to_string());
            }
            return runs;
        }
        flags.extend(ch.notes.iter().cloned());
        let mut runs = ch.runs.clone();
        if ch.broken && ch.clusters < n {
            if let Some(last) = ch.last() {
                let (more, _) = contiguous_runs(fs, last.saturating_add(1), n - ch.clusters);
                if !more.is_empty() {
                    flags.push(
                        "remainder after the broken cluster chain assumed contiguous".to_string(),
                    );
                    runs.extend(more);
                }
            }
        }
        runs
    }

    fn plan_file(
        &mut self,
        item: &SetItem,
        ctx: Ctx,
        flags: &mut Vec<String>,
    ) -> (Vec<Range>, EntryConfidence) {
        let fs = self.fs;
        let dl = item.data_length;
        let vdl = item.valid_data_length;
        let trusted = ctx.trust() && !item.deleted;
        if vdl < dl {
            flags.push(format!("{VDL_FLAG}{vdl}"));
            flags.push(format!(
                "only the first {vdl} of {dl} bytes were written (ValidDataLength); the rest reads as zeros"
            ));
        }
        if dl == 0 {
            return (
                Vec::new(),
                if trusted {
                    EntryConfidence::Allocated
                } else {
                    EntryConfidence::DeletedIntact
                },
            );
        }
        let lost = if trusted {
            EntryConfidence::Allocated
        } else {
            EntryConfidence::DeletedNoData
        };
        if !item.allocation_possible() {
            flags.push("AllocationPossible is clear; no data clusters recorded".to_string());
            return (Vec::new(), lost);
        }
        let first = item.first_cluster;
        if !fs.valid(first) {
            flags.push(format!(
                "first cluster {first} is invalid; data location lost"
            ));
            return (Vec::new(), lost);
        }
        let n = div_ceil(dl, self.cs());
        if trusted {
            let runs = self.stream_runs(item, n, flags, false);
            add_runs(&mut self.claimed, &runs);
            return (
                runs_to_extents(fs, &runs, Some(dl)),
                EntryConfidence::Allocated,
            );
        }
        let chain_alive = !item.no_fat_chain()
            && matches!(
                fs.fat_state(first),
                Some(FatEntry::Next(_)) | Some(FatEntry::Eoc)
            );
        let runs = if item.no_fat_chain() || chain_alive {
            self.stream_runs(item, n, flags, true)
        } else {
            flags.push("extents guessed (assumed contiguous)".to_string());
            let (runs, clipped) = contiguous_runs(fs, first, n);
            if clipped {
                flags.push("assumed extents run past the last cluster; clipped".to_string());
            }
            runs
        };
        let reused = allocated_clusters(fs, &runs);
        let total: u64 = runs.iter().map(|r| r.1 as u64).sum();
        if reused > 0 {
            flags.push(format!(
                "{reused} of {total} clusters are now allocated to another file"
            ));
        }
        let exact = item.no_fat_chain() && total == n;
        let conf = if exact && reused == 0 {
            EntryConfidence::DeletedIntact
        } else {
            EntryConfidence::DeletedGuessed
        };
        (runs_to_extents(fs, &runs, Some(dl)), conf)
    }

    fn plan_dir(
        &mut self,
        item: &SetItem,
        ctx: Ctx,
        flags: &mut Vec<String>,
    ) -> (Vec<Range>, EntryConfidence, Option<DirSrc>) {
        let fs = self.fs;
        let trusted = ctx.trust() && !item.deleted;
        let first = item.first_cluster;
        let n = div_ceil(item.data_length, self.cs()).min(self.max_dir_clusters());
        let none_conf = if trusted {
            EntryConfidence::Allocated
        } else {
            EntryConfidence::DeletedNoData
        };
        if n == 0 {
            return (
                Vec::new(),
                if trusted {
                    EntryConfidence::Allocated
                } else {
                    EntryConfidence::DeletedIntact
                },
                None,
            );
        }
        if !fs.valid(first) {
            flags.push(format!("directory points to invalid cluster {first}"));
            return (Vec::new(), none_conf, None);
        }
        if self.dir_clusters.contains(first as u64) {
            flags.push("directory already listed elsewhere (cross-linked or looping)".to_string());
            return (
                Vec::new(),
                if trusted {
                    EntryConfidence::Allocated
                } else {
                    EntryConfidence::DeletedGuessed
                },
                None,
            );
        }
        if trusted {
            let src = if item.no_fat_chain() {
                DirSrc::Contiguous(first, n)
            } else {
                DirSrc::Chain(first, n)
            };
            let runs = self.stream_runs(item, n, &mut Vec::new(), false);
            return (
                runs_to_extents(fs, &runs, None),
                EntryConfidence::Allocated,
                Some(src),
            );
        }
        if fs.state(first) == ClusterState::Allocated {
            flags.push(
                "directory cluster is now allocated to another file; contents not listed"
                    .to_string(),
            );
            let one = vec![Range::with_len(fs.offset_of(first), self.cs())];
            return (one, EntryConfidence::DeletedGuessed, None);
        }
        let chain_alive = !item.no_fat_chain()
            && matches!(
                fs.fat_state(first),
                Some(FatEntry::Next(_)) | Some(FatEntry::Eoc)
            );
        let mut f2 = Vec::new();
        let runs = if item.no_fat_chain() || chain_alive {
            self.stream_runs(item, n, &mut f2, true)
        } else {
            contiguous_runs(fs, first, n).0
        };
        let reused = allocated_clusters(fs, &runs);
        let conf = if item.no_fat_chain() && reused == 0 {
            EntryConfidence::DeletedIntact
        } else {
            EntryConfidence::DeletedGuessed
        };
        let src = if item.no_fat_chain() || !chain_alive || f2.iter().any(|f| f.contains("guessed"))
        {
            DirSrc::Contiguous(first, n)
        } else {
            DirSrc::Chain(first, n)
        };
        (runs_to_extents(fs, &runs, None), conf, Some(src))
    }

    fn scan_orphans(&mut self) {
        let fs = self.fs;
        let lost_ok = !self.em.skipped_any;
        let src_size = fs.meta().size();
        let cs = self.cs();
        let mut todo: Vec<(u32, bool)> = Vec::new();
        for_each_state(fs, |c, s| {
            if fs.offset_of(c).saturating_add(cs) > src_size {
                return false;
            }
            if self.dir_clusters.contains(c as u64) {
                return true;
            }
            let lost = lost_ok && s == ClusterState::Allocated && !self.claimed.contains(c as u64);
            // Free clusters, clusters with an unreadable bitmap bit, and
            // allocated clusters nothing claims.
            if matches!(s, ClusterState::Free | ClusterState::Unknown) || lost {
                todo.push((c, lost));
            }
            true
        });
        let mut cands: Vec<(u32, bool)> = Vec::new();
        let mut truncated = false;
        for chunk in todo.chunks(65_536) {
            let clusters: Vec<u32> = chunk.iter().map(|b| b.0).collect();
            let lost: HashSet<u32> = chunk.iter().filter(|b| b.1).map(|b| b.0).collect();
            scan_heads(fs.meta(), fs, &clusters, HEAD, |c, head| {
                if cands.len() < MAX_ORPHAN_DIRS && sniff_dir(head) {
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
        let cand_set: HashSet<u32> = cands.iter().map(|c| c.0).collect();
        let mut referenced: HashSet<u32> = HashSet::new();
        for &(c, _) in &cands {
            let data = fs.read_dir_runs(&[(c, 1)]);
            let p = parse_dir(&data, fs.upcase());
            for it in &p.items {
                if it.is_dir() && it.first_cluster != c {
                    self.dir_starts.insert(it.first_cluster);
                    if cand_set.contains(&it.first_cluster) {
                        referenced.insert(it.first_cluster);
                    }
                }
            }
        }
        for &(c, _) in &cands {
            if c > 2 && cand_set.contains(&(c - 1)) && !self.dir_starts.contains(&c) {
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
                    if self.em.emit(&orphans_root_entry()) != WalkControl::Continue {
                        return;
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
                let ctx = if lost {
                    Ctx::OrphanLost
                } else {
                    Ctx::OrphanFree
                };
                self.walk_dir(DirSrc::Scan(c), Some(orphan_dir_id(c)), &e.path, ctx, 2);
                self.dir_clusters.insert(Range::new(c as u64, c as u64 + 1));
            }
        }
    }
}
