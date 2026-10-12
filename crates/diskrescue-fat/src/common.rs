//! Plumbing shared by the FAT and exFAT parsers: the cluster geometry
//! abstraction, `read_file`, the walk emitter (entry limit, stop/skip
//! control), synthetic `$orphans/` entries and run/extent conversions.

use std::io::Write;

use diskrescue_core::fs::{EntryConfidence, FileEntry, FileReadReport, WalkControl};
use diskrescue_core::{Error, Range, RangeSet, Result, SharedSource};

use crate::io::{write_zeros, DataReader};

/// Allocation state of one cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClusterState {
    /// Not allocated.
    Free,
    /// Allocated to some file or directory.
    Allocated,
    /// Marked bad.
    Bad,
    /// The allocation metadata for this cluster is unreadable.
    Unknown,
}

/// Cluster geometry and allocation lookups of a volume.
pub(crate) trait ClusterMap: Send + Sync {
    /// Bytes per cluster.
    fn cluster_bytes(&self) -> u64;
    /// Volume offset of cluster 2.
    fn heap_offset(&self) -> u64;
    /// Highest valid cluster number.
    fn max_cluster(&self) -> u32;
    /// States of clusters `first..first+count` (count is small, <= 65536).
    fn states(&self, first: u32, count: u32) -> Vec<ClusterState>;
    /// Highest cluster whose allocation metadata (FAT entry, bitmap bit)
    /// lies inside the source; later clusters can only be `Unknown`.
    fn meta_limit(&self) -> u32;
    /// Size of the underlying source.
    fn source_len(&self) -> u64;

    /// Highest cluster worth scanning: known metadata or data inside the
    /// source. Bounds every whole-volume scan by the size of the source, so
    /// an absurd cluster count costs nothing.
    fn scan_limit(&self) -> u32 {
        let src = self.source_len();
        let data_limit = if src <= self.heap_offset() {
            1
        } else {
            let n = (src - self.heap_offset()) / self.cluster_bytes();
            (n + 1).min(u32::MAX as u64) as u32
        };
        self.max_cluster().min(self.meta_limit().max(data_limit))
    }

    /// State of one cluster.
    fn state(&self, c: u32) -> ClusterState {
        self.states(c, 1)
            .first()
            .copied()
            .unwrap_or(ClusterState::Unknown)
    }

    /// Volume offset of cluster `c` (`c >= 2`).
    fn offset_of(&self, c: u32) -> u64 {
        self.heap_offset()
            .saturating_add((c.saturating_sub(2) as u64).saturating_mul(self.cluster_bytes()))
    }

    /// Cluster containing volume offset `off`, if inside the heap.
    fn cluster_at(&self, off: u64) -> Option<u32> {
        if off < self.heap_offset() {
            return None;
        }
        let c = (off - self.heap_offset()) / self.cluster_bytes() + 2;
        if c > self.max_cluster() as u64 {
            None
        } else {
            Some(c as u32)
        }
    }

    /// True when `c` is a valid cluster number.
    fn valid(&self, c: u32) -> bool {
        c >= 2 && c <= self.max_cluster()
    }
}

/// Batch size for bulk state scans.
pub(crate) const STATE_BATCH: u32 = 65_536;

/// Visit every cluster state in `2..=scan_limit` in batches.
pub(crate) fn for_each_state(map: &dyn ClusterMap, mut f: impl FnMut(u32, ClusterState) -> bool) {
    let max = map.scan_limit() as u64;
    let mut c = 2u64;
    while c <= max {
        let n = (max - c + 1).min(STATE_BATCH as u64) as u32;
        let states = map.states(c as u32, n);
        for (i, s) in states.into_iter().enumerate() {
            if !f(c as u32 + i as u32, s) {
                return;
            }
        }
        c += n as u64;
    }
}

/// Free clusters as volume-relative byte ranges. Clusters whose
/// allocation state is unreadable are included (they are candidates for
/// carving) as long as they lie inside the source.
pub(crate) fn unallocated_ranges(map: &dyn ClusterMap, source_size: u64) -> RangeSet {
    let cs = map.cluster_bytes();
    let mut set = RangeSet::new();
    let mut run: Option<(u32, u32)> = None;
    let flush = |set: &mut RangeSet, run: (u32, u32)| {
        let start = map.offset_of(run.0);
        set.insert(Range::with_len(start, run.1 as u64 * cs));
    };
    for_each_state(map, |c, s| {
        let free = match s {
            ClusterState::Free => true,
            ClusterState::Unknown => map.offset_of(c) < source_size,
            _ => false,
        };
        if free {
            run = match run {
                Some((a, n)) if a as u64 + n as u64 == c as u64 => Some((a, n + 1)),
                Some(r) => {
                    flush(&mut set, r);
                    Some((c, 1))
                }
                None => Some((c, 1)),
            };
        }
        true
    });
    if let Some(r) = run {
        flush(&mut set, r);
    }
    set
}

/// Convert cluster runs into volume byte extents, cutting the total at
/// `byte_limit` when given.
pub(crate) fn runs_to_extents(
    map: &dyn ClusterMap,
    runs: &[(u32, u32)],
    byte_limit: Option<u64>,
) -> Vec<Range> {
    let cs = map.cluster_bytes();
    let mut out: Vec<Range> = Vec::new();
    let mut left = byte_limit.unwrap_or(u64::MAX);
    for &(first, count) in runs {
        if left == 0 {
            break;
        }
        let start = map.offset_of(first);
        let len = (count as u64 * cs).min(left);
        left -= len;
        match out.last_mut() {
            Some(last) if last.end == start => last.end += len,
            _ => out.push(Range::with_len(start, len)),
        }
    }
    out
}

/// Clusters `first..first+n` clipped to the volume; returns the runs and
/// whether clipping happened.
pub(crate) fn contiguous_runs(map: &dyn ClusterMap, first: u32, n: u64) -> (Vec<(u32, u32)>, bool) {
    if n == 0 || !map.valid(first) {
        return (Vec::new(), n > 0);
    }
    let avail = map.max_cluster() as u64 - first as u64 + 1;
    let take = n.min(avail).min(u32::MAX as u64);
    (vec![(first, take as u32)], take < n)
}

/// Clusters of `runs` that are currently allocated (or marked bad).
pub(crate) fn allocated_clusters(map: &dyn ClusterMap, runs: &[(u32, u32)]) -> u64 {
    let mut n = 0u64;
    for &(first, count) in runs {
        let mut done = 0u32;
        while done < count {
            let k = (count - done).min(STATE_BATCH);
            n += map
                .states(first + done, k)
                .iter()
                .filter(|s| matches!(s, ClusterState::Allocated | ClusterState::Bad))
                .count() as u64;
            done += k;
        }
    }
    n
}

/// Flag prefix carrying exFAT's ValidDataLength.
pub(crate) const VDL_FLAG: &str = "valid_data_length=";

/// Flags that describe attributes rather than recovery notes.
const ATTRIBUTE_FLAGS: &[&str] = &["hidden", "system", "read-only", "orphan", "synthetic"];

fn valid_data_length(entry: &FileEntry) -> Option<u64> {
    entry
        .flags
        .iter()
        .find_map(|f| f.strip_prefix(VDL_FLAG))
        .and_then(|v| v.parse().ok())
}

/// Shared implementation of `FileSystem::read_file`.
///
/// `past_vdl`: read the bytes between exFAT's ValidDataLength and the size
/// from the medium instead of writing zeros (see
/// [`crate::FatOptions::read_past_valid_data_length`]).
pub(crate) fn read_entry(
    src: &SharedSource,
    map: &dyn ClusterMap,
    entry: &FileEntry,
    sink: &mut dyn Write,
    past_vdl: bool,
) -> Result<FileReadReport> {
    if entry.is_dir {
        return Err(Error::InvalidArgument(format!(
            "{} is a directory",
            entry.path
        )));
    }
    if entry.confidence == EntryConfidence::DeletedNoData {
        return Err(Error::NotFound(format!(
            "data location of deleted file {} is lost; use carving",
            entry.path
        )));
    }
    let size = entry.size;
    let recorded_vdl = valid_data_length(entry).unwrap_or(size).min(size);
    let vdl = if past_vdl { size } else { recorded_vdl };
    let mut report = FileReadReport::default();
    if past_vdl && recorded_vdl < size {
        report.notes.push(format!(
            "bytes {recorded_vdl}..{size} lie past ValidDataLength (never written according to the \
             filesystem) and were read as stored: an interrupted recording, or stale data"
        ));
    }
    for f in &entry.flags {
        if !ATTRIBUTE_FLAGS.contains(&f.as_str()) && !f.starts_with(VDL_FLAG) {
            report.notes.push(f.clone());
        }
    }
    let check_reused = entry.deleted || entry.confidence != EntryConfidence::Allocated;
    let known_bad = src.unreadable().filter(|s| !s.is_empty());
    let mut reader = DataReader::new(&**src);
    let cs = map.cluster_bytes();
    let mut pos = 0u64;
    for ext in &entry.extents {
        if pos >= size {
            break;
        }
        let take = ext.len().min(size - pos);
        if take == 0 {
            continue;
        }
        let readable = if pos < vdl { take.min(vdl - pos) } else { 0 };
        if readable > 0 {
            reader.copy(
                ext.start,
                readable,
                pos,
                sink,
                &mut report.unreadable,
                &mut report.notes,
            )?;
            if let Some(kb) = &known_bad {
                for r in kb
                    .intersection_with_range(Range::with_len(ext.start, readable))
                    .iter()
                {
                    report.unreadable.insert(Range::new(
                        pos + (r.start - ext.start),
                        pos + (r.end - ext.start),
                    ));
                }
            }
        }
        if take > readable {
            write_zeros(sink, take - readable)?;
            report.holes.insert(Range::new(pos + readable, pos + take));
        }
        if check_reused {
            if let Some(first) = map.cluster_at(ext.start) {
                let last_byte = ext.start + take - 1;
                let last = map.cluster_at(last_byte).unwrap_or(map.max_cluster());
                let mut c = first as u64;
                while c <= last as u64 {
                    let k = ((last as u64 - c + 1).min(STATE_BATCH as u64)) as u32;
                    for (i, s) in map.states(c as u32, k).into_iter().enumerate() {
                        if matches!(s, ClusterState::Allocated | ClusterState::Bad) {
                            let co = map.offset_of(c as u32 + i as u32);
                            let r =
                                Range::with_len(co, cs).intersect(Range::with_len(ext.start, take));
                            if let Some(r) = r {
                                report.reused.insert(Range::new(
                                    pos + (r.start - ext.start),
                                    pos + (r.end - ext.start),
                                ));
                            }
                        }
                    }
                    c += k as u64;
                }
            }
        }
        pos += take;
    }
    if pos < size {
        write_zeros(sink, size - pos)?;
        if pos < vdl {
            report.unreadable.insert(Range::new(pos, vdl));
            report.notes.push(format!(
                "only {pos} of {size} bytes have a known location; the rest was zero-filled"
            ));
        }
        let hole_start = pos.max(vdl);
        if hole_start < size {
            report.holes.insert(Range::new(hole_start, size));
        }
    }
    if !report.reused.is_empty() {
        report.notes.push(format!(
            "{} bytes lie in clusters now allocated to another file and are probably overwritten",
            report.reused.total_len()
        ));
    }
    report.bytes_written = size;
    Ok(report)
}

/// Synthetic id of the `$orphans` directory.
pub(crate) const ORPHANS_ID: u64 = u64::MAX;
/// Name of the synthetic orphan directory.
pub(crate) const ORPHANS_DIR: &str = "$orphans";

/// Synthetic id of `$orphans/cluster_<c>`.
pub(crate) fn orphan_dir_id(c: u32) -> u64 {
    (1u64 << 62) | c as u64
}

/// Directory entry for `$orphans`.
pub(crate) fn orphans_root_entry() -> FileEntry {
    FileEntry {
        id: ORPHANS_ID,
        parent_id: None,
        path: ORPHANS_DIR.to_string(),
        name: ORPHANS_DIR.to_string(),
        size: 0,
        is_dir: true,
        deleted: true,
        confidence: EntryConfidence::DeletedGuessed,
        created: None,
        modified: None,
        accessed: None,
        extents: Vec::new(),
        resident: false,
        flags: vec!["synthetic".to_string()],
    }
}

/// Directory entry for `$orphans/cluster_<c>`.
pub(crate) fn orphan_dir_entry(map: &dyn ClusterMap, c: u32, lost: bool) -> FileEntry {
    let name = format!("cluster_{c}");
    let mut flags = vec!["synthetic".to_string(), "orphan".to_string()];
    flags.push(if lost {
        "directory cluster is allocated but not reachable from the root (lost directory)"
            .to_string()
    } else {
        "directory found in unallocated space; its own entry is lost".to_string()
    });
    FileEntry {
        id: orphan_dir_id(c),
        parent_id: Some(ORPHANS_ID),
        path: format!("{ORPHANS_DIR}/{name}"),
        name,
        size: 0,
        is_dir: true,
        deleted: true,
        confidence: EntryConfidence::DeletedGuessed,
        created: None,
        modified: None,
        accessed: None,
        extents: vec![Range::with_len(map.offset_of(c), map.cluster_bytes())],
        resident: false,
        flags,
    }
}

/// Wraps the caller's visitor: counts entries, enforces `max_entries`,
/// remembers `Stop` and whether any directory was skipped.
pub(crate) struct Emitter<'a> {
    visit: &'a mut dyn FnMut(&FileEntry) -> WalkControl,
    emitted: usize,
    max: usize,
    /// The walk must end (Stop requested or the entry limit was reached).
    pub stopped: bool,
    /// Some directory's children were skipped at the caller's request.
    pub skipped_any: bool,
    /// Parsed entries of the directories on the current recursion path.
    pending: usize,
}

impl<'a> Emitter<'a> {
    /// New emitter with an entry limit.
    pub(crate) fn new(visit: &'a mut dyn FnMut(&FileEntry) -> WalkControl, max: usize) -> Self {
        Emitter {
            visit,
            emitted: 0,
            max,
            stopped: false,
            skipped_any: false,
            pending: 0,
        }
    }

    /// Account for `n` parsed entries held in memory while a directory is
    /// being processed. Returns false (and stops the walk) when the entries
    /// pending along the recursion path exceed [`MAX_PENDING`].
    pub(crate) fn reserve(&mut self, n: usize) -> bool {
        self.pending = self.pending.saturating_add(n);
        if self.pending > MAX_PENDING {
            log::warn!(
                "more than {MAX_PENDING} directory entries pending along one path; stopping the walk"
            );
            self.stopped = true;
            return false;
        }
        true
    }

    /// Release entries accounted with [`Emitter::reserve`].
    pub(crate) fn release(&mut self, n: usize) {
        self.pending = self.pending.saturating_sub(n);
    }

    /// Hand one entry to the visitor.
    pub(crate) fn emit(&mut self, e: &FileEntry) -> WalkControl {
        if self.stopped {
            return WalkControl::Stop;
        }
        if self.emitted >= self.max {
            log::warn!(
                "entry limit of {} reached; the rest of the volume is not listed",
                self.max
            );
            self.stopped = true;
            return WalkControl::Stop;
        }
        self.emitted += 1;
        let c = (self.visit)(e);
        match c {
            WalkControl::Stop => self.stopped = true,
            WalkControl::SkipChildren if e.is_dir => self.skipped_any = true,
            _ => {}
        }
        c
    }
}

/// Raw bytes of a directory plus where they came from.
pub(crate) struct DirData {
    /// Concatenated directory bytes.
    pub bytes: Vec<u8>,
    /// `(dir offset, volume offset, length)` of each contiguous piece.
    pub segments: Vec<(u64, u64, u64)>,
    /// Directory-relative byte ranges that were unreadable (zero-filled).
    pub bad: RangeSet,
}

impl DirData {
    /// Volume offset of directory byte `rel` (segments are in directory
    /// order, so this is a binary search: a directory spread over tens of
    /// thousands of fragments must not cost a scan per entry).
    pub(crate) fn vol_offset(&self, rel: u64) -> u64 {
        let i = self.segments.partition_point(|&(d, _, l)| d + l <= rel);
        if let Some(&(d, v, l)) = self.segments.get(i) {
            if rel >= d && rel < d + l {
                return v + (rel - d);
            }
        }
        // Not reachable for offsets inside `bytes`; keep a unique value.
        u64::MAX - rel
    }
}

/// Note attached to deleted entries whose clusters overlap those of
/// another deleted entry.
pub(crate) const SHARED_NOTE: &str = "some of its clusters are also claimed by another deleted file or directory; one of them was written over the other";

/// Most claims recorded in the first pass (24 bytes each).
const MAX_CLAIMS: usize = 4_000_000;

/// Clusters claimed by deleted entries, collected in a silent first pass
/// over the directory tree so that every deleted entry sharing clusters
/// with another one can be flagged before it is reported (filesystems spec
/// 4.2: both are listed, with a shared-cluster note, and neither is
/// "intact"). Also caches the outcome of expensive first-cluster guesses so
/// the second pass does not repeat their reads.
#[derive(Debug, Default)]
pub(crate) struct ClaimIndex {
    /// `(first cluster, end cluster exclusive, entry id)` sorted by start.
    claims: Vec<(u64, u64, u64)>,
    /// Running maximum of the end over `claims[..=i]`.
    prefix_end: Vec<u64>,
    /// Entries that overlap another entry.
    shared: std::collections::HashSet<u64>,
    /// Resolved first clusters (entry id -> cluster, flags).
    resolved: std::collections::HashMap<u64, (u32, Vec<String>)>,
    /// The first pass hit [`MAX_CLAIMS`].
    pub truncated: bool,
}

impl ClaimIndex {
    /// Record the clusters of a deleted entry.
    pub(crate) fn record(&mut self, id: u64, runs: &[(u32, u32)]) {
        for &(s, n) in runs {
            if n == 0 {
                continue;
            }
            if self.claims.len() >= MAX_CLAIMS {
                self.truncated = true;
                return;
            }
            self.claims.push((s as u64, s as u64 + n as u64, id));
        }
    }

    /// Remember a resolved first cluster.
    pub(crate) fn remember(&mut self, id: u64, cluster: u32, flags: &[String]) {
        if self.resolved.len() < MAX_CLAIMS {
            self.resolved.insert(id, (cluster, flags.to_vec()));
        }
    }

    /// A first cluster resolved in the first pass.
    pub(crate) fn resolved(&self, id: u64) -> Option<&(u32, Vec<String>)> {
        self.resolved.get(&id)
    }

    /// Sort the claims and find every entry that overlaps another one.
    pub(crate) fn finish(&mut self) {
        self.claims.sort_unstable();
        let n = self.claims.len();
        let mut max_end = 0u64;
        let mut max_id = None;
        self.prefix_end = Vec::with_capacity(n);
        for i in 0..n {
            let (s, e, id) = self.claims[i];
            if s < max_end {
                if let Some(m) = max_id.filter(|&m| m != id) {
                    self.shared.insert(id);
                    self.shared.insert(m);
                }
            }
            if let Some(&(ns, _, nid)) = self.claims.get(i + 1) {
                if ns < e && nid != id {
                    self.shared.insert(id);
                    self.shared.insert(nid);
                }
            }
            if e > max_end {
                max_end = e;
                max_id = Some(id);
            }
            self.prefix_end.push(max_end);
        }
    }

    /// True when the entry overlapped another deleted entry in the first
    /// pass.
    pub(crate) fn is_shared(&self, id: u64) -> bool {
        self.shared.contains(&id)
    }

    /// True when `runs` overlap any recorded claim of another entry (used
    /// for entries the first pass did not see, such as orphans).
    pub(crate) fn overlaps(&self, id: u64, runs: &[(u32, u32)]) -> bool {
        for &(s, n) in runs {
            let (s, e) = (s as u64, s as u64 + n as u64);
            let k = self.claims.partition_point(|c| c.0 < e);
            if k == 0 || self.prefix_end[k - 1] <= s {
                continue;
            }
            // Some claim starting before `e` ends after `s`; make sure it is
            // not this entry's own (scan back a little, then assume so).
            let mut j = k;
            let mut seen = 0;
            while j > 0 && seen < 64 {
                j -= 1;
                seen += 1;
                let c = self.claims[j];
                if c.1 > s && c.2 != id {
                    return true;
                }
            }
            if seen == 64 {
                return true;
            }
        }
        false
    }
}

/// Maximum number of parsed directory entries held in memory at once.
pub(crate) const MAX_PENDING: usize = 1 << 21;
/// Maximum directory depth followed.
pub(crate) const MAX_DEPTH: usize = 256;
/// Maximum bytes of one directory read into memory (2^20 entries).
pub(crate) const MAX_DIR_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum number of orphan directory clusters surfaced.
pub(crate) const MAX_ORPHAN_DIRS: usize = 100_000;

/// Read the first `head_len` bytes of each cluster in `clusters` (sorted
/// ascending) without polluting the metadata cache, batching runs of small
/// consecutive clusters into one read. Unreadable heads are skipped.
pub(crate) fn scan_heads(
    meta: &crate::io::MetaReader,
    map: &dyn ClusterMap,
    clusters: &[u32],
    head_len: usize,
    mut f: impl FnMut(u32, &[u8]),
) {
    let cs = map.cluster_bytes();
    let head_len = (head_len as u64).min(cs) as usize;
    let batch_limit: u64 = 1024 * 1024;
    let mut i = 0usize;
    while i < clusters.len() {
        let mut j = i + 1;
        if cs <= 16 * 1024 {
            while j < clusters.len()
                && clusters[j] as u64 == clusters[j - 1] as u64 + 1
                && (j - i + 1) as u64 * cs <= batch_limit
            {
                j += 1;
            }
        }
        let single = |c: u32, f: &mut dyn FnMut(u32, &[u8])| {
            if let Some(h) = meta.read_uncached(map.offset_of(c), head_len) {
                if h.len() == head_len {
                    f(c, &h);
                }
            }
        };
        if j - i > 1 {
            let start = map.offset_of(clusters[i]);
            let len = ((j - i) as u64 * cs) as usize;
            match meta.read_uncached(start, len) {
                Some(buf) if buf.len() == len => {
                    for (k, &c) in clusters[i..j].iter().enumerate() {
                        let o = k * cs as usize;
                        f(c, &buf[o..o + head_len]);
                    }
                }
                _ => {
                    for &c in &clusters[i..j] {
                        single(c, &mut f);
                    }
                }
            }
        } else {
            single(clusters[i], &mut f);
        }
        i = j;
    }
}
