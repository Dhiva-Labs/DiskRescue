//! Building the file tree and [`diskrescue_core::fs::FileSystem::walk`].
//!
//! # Whole-MFT mode (default)
//!
//! Pass 1 reads the MFT sequentially in large chunks and keeps a compact
//! node per base record: sequence number, in-use and directory flags, and
//! its names. Names come from `$FILE_NAME` attributes: per parent
//! directory, DOS 8.3 names are dropped when a Win32/POSIX name exists, and
//! every remaining (parent, name) pair is one hard link.
//!
//! Each link is placed under its parent when the parent record exists, is a
//! directory and its sequence number matches the reference (a freed parent
//! may carry the reference's sequence + 1, see
//! [`crate::record::sequence_matches`]). Record 5 is the root. Deleted
//! directories whose record is intact are valid parents, so a deleted
//! folder comes back with its structure. Links whose parent is missing,
//! reused or not a directory become *orphan roots*, emitted as
//! `$orphans/<record>/<name>` (with flags `orphan` and the reason).
//! Records unreachable from the root and from every orphan root (directory
//! cycles) are turned into orphan roots too, one per cycle. System records
//! (0..15) and the `$Extend` subtree are not reported.
//!
//! Pass 2 is an iterative depth-first emission: directories before their
//! children, children sorted by name, every record re-read to build the
//! full entry (data streams, extents, confidence). Each named `$DATA`
//! stream becomes an extra entry `name:stream` flagged `ads`. When deleted
//! entries are wanted, each directory's `$I30` index is also read: entries
//! that refer to records which no longer hold that file (record reused or
//! unreadable), and entries recovered from index slack, are emitted as
//! name-only entries with [`EntryConfidence::DeletedNoData`].
//!
//! # Directory-tree mode (`walk_whole_mft = false`)
//!
//! Only the `$I30` indexes are walked from the root; orphans are not found.
//! Deleted entries come from index slack / unused index blocks.
//!
//! # Entry ids
//!
//! `id = record | (sub << 48)`: sub 0 is the primary name, `1..0x4000` are
//! further hard links (same data), `0x4000..0x8000` are alternate data
//! streams (index into the record's named `$DATA` streams sorted by name),
//! and `0x8000` marks synthetic entries (virtual `$orphans` directories and
//! index-only names) whose low bits are a per-walk counter. The root
//! directory itself (id 5) is not reported; its children carry
//! `parent_id = Some(5)`.

use std::collections::{BTreeMap, HashMap, HashSet};

use diskrescue_core::fs::{EntryConfidence, FileEntry, WalkControl};
use diskrescue_core::time::Timestamp;
use diskrescue_core::{Error, Range, RangeSet};

use crate::attr::{
    mref, mseq, types, FileName, ATTR_SPARSE, REPARSE_TAG_MOUNT_POINT, REPARSE_TAG_SYMLINK,
};
use crate::bitmap::ClusterState;
use crate::index::IndexSource;
use crate::record::sequence_matches;
use crate::stream::{AllocationMap, ExtentKind, Stream, StreamData};
use crate::volume::{classify_record, is_fatal, FileInfo, NtfsFs, RecordRead};
use crate::Result;

/// Root directory record.
pub const ROOT: u64 = 5;
/// First record number of user files (0..15 are system records).
pub const FIRST_USER_RECORD: u64 = 16;
/// Nameless records below this are reserved and never reported.
const FIRST_UNRESERVED: u64 = 24;
/// Unreadable records reported one by one before switching to a summary.
const MAX_RECORD_WARNINGS: u64 = 64;
/// Maximum directory depth followed before the rest is re-rooted.
pub const MAX_DEPTH: usize = 1024;
/// Mask of the record-number part of an entry id.
pub const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// Sub-id of the first additional hard link.
pub const SUB_LINK_MAX: u64 = 0x3FFF;
/// Sub-id base of alternate data streams.
pub const SUB_ADS: u64 = 0x4000;
/// Sub-id of synthetic entries.
pub const SUB_SYNTHETIC: u64 = 0x8000;
/// Top-level directory holding entries whose parent chain is lost.
pub const ORPHANS_DIR: &str = "$orphans";

/// Flag strings used in [`FileEntry::flags`].
pub mod flags {
    /// Alternate data stream (`name:stream`).
    pub const ADS: &str = "ads";
    /// NTFS-compressed data (LZNT1).
    pub const COMPRESSED: &str = "compressed";
    /// EFS-encrypted data (not recoverable).
    pub const ENCRYPTED: &str = "encrypted";
    /// Sparse data.
    pub const SPARSE: &str = "sparse";
    /// Reparse point (not followed).
    pub const REPARSE: &str = "reparse";
    /// Symbolic link reparse point.
    pub const SYMLINK: &str = "symlink";
    /// Junction / mount point.
    pub const JUNCTION: &str = "junction";
    /// Additional hard link of a file reported elsewhere too.
    pub const HARDLINK: &str = "hardlink";
    /// The MFT record had a fixup mismatch (torn write).
    pub const TORN: &str = "torn-record";
    /// Placed under `$orphans`.
    pub const ORPHAN: &str = "orphan";
    /// Orphan reason: parent record missing or unreadable.
    pub const PARENT_MISSING: &str = "parent-missing";
    /// Orphan reason: parent record was reused (sequence mismatch).
    pub const PARENT_REUSED: &str = "parent-reused";
    /// Orphan reason: parent record is not a directory.
    pub const PARENT_NOT_DIR: &str = "parent-not-directory";
    /// Orphan reason: parent chain forms a cycle.
    pub const CYCLE: &str = "directory-cycle";
    /// Orphan reason: path deeper than the depth limit.
    pub const TOO_DEEP: &str = "path-too-deep";
    /// Orphan reason: the record has no file name.
    pub const NAMELESS: &str = "nameless";
    /// Name-only entry recovered from directory index slack.
    pub const INDEX_SLACK: &str = "index-slack";
    /// Name-only entry from an index entry whose record was reused.
    pub const STALE_INDEX: &str = "stale-index-entry";
    /// The entry's MFT record is unreadable; only the index name survives.
    pub const RECORD_UNREADABLE: &str = "mft-record-unreadable";
    /// Synthetic directory (`$orphans` and its per-record holders).
    pub const VIRTUAL: &str = "virtual";
    /// Part of the data mapping is lost (missing attribute pieces).
    pub const INCOMPLETE: &str = "incomplete-runs";
    /// The size in the record was impossible (larger than the allocation
    /// or the volume) and was clamped to the mapped data.
    pub const SIZE_CLAMPED: &str = "size-clamped";
    /// Attribute pieces were taken from extension records the attribute
    /// list no longer names (deleted file or damaged list): a guess.
    pub const PIECES_GUESSED: &str = "attribute-pieces-guessed";
}

/// Build an entry id.
pub fn make_id(record: u64, sub: u64) -> u64 {
    (record & RECORD_MASK) | (sub << 48)
}

/// Split an entry id into record number and sub-id.
pub fn split_id(id: u64) -> (u64, u64) {
    (id & RECORD_MASK, id >> 48)
}

struct Node {
    seq: u16,
    in_use: bool,
    is_dir: bool,
    has_data: bool,
    links: Vec<(u64, String)>,
}

/// One thing to emit inside a directory frame.
enum Item {
    Rec {
        rec: u64,
        link: u16,
        name: String,
        extra: Vec<&'static str>,
    },
    Ready(Box<FileEntry>),
}

impl Item {
    fn sort_key(&self) -> (String, u64) {
        match self {
            Item::Rec { rec, name, .. } => (name.to_lowercase(), *rec),
            Item::Ready(e) => (e.name.to_lowercase(), e.id),
        }
    }
}

struct Frame {
    prefix: String,
    parent_id: Option<u64>,
    items: Vec<Item>,
    idx: usize,
    depth: usize,
}

/// Choose the links (parent reference, name) of a record from its
/// `$FILE_NAME` attributes.
pub(crate) fn select_links(names: &[FileName]) -> Vec<(u64, String)> {
    let mut parents: Vec<u64> = Vec::new();
    for n in names {
        if !parents.contains(&n.parent_record()) {
            parents.push(n.parent_record());
        }
    }
    let mut out: Vec<(u64, String)> = Vec::new();
    for p in parents {
        let group: Vec<&FileName> = names.iter().filter(|n| n.parent_record() == p).collect();
        let any_long = group.iter().any(|n| !n.is_dos_only());
        for n in group {
            if any_long && n.is_dos_only() {
                continue;
            }
            if out.iter().any(|(r, nm)| mref(*r) == p && *nm == n.name) {
                continue;
            }
            if out.len() as u64 > SUB_LINK_MAX {
                return out;
            }
            out.push((n.parent, n.name.clone()));
        }
    }
    out
}

fn is_system(rec: u64) -> bool {
    rec < FIRST_USER_RECORD && rec != ROOT
}

fn ts(ft: u64) -> Option<Timestamp> {
    Timestamp::from_filetime(ft)
}

/// What a data stream looks like to the walker.
struct StreamDesc {
    size: u64,
    extents: Vec<Range>,
    resident: bool,
    confidence: EntryConfidence,
    flags: Vec<&'static str>,
}

impl NtfsFs {
    pub(crate) fn walk_impl(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        if self.opts.walk_whole_mft {
            self.walk_mft(visit)
        } else {
            self.walk_tree(visit)
        }
    }

    // ----- pass 1 -------------------------------------------------------

    fn scan_nodes(&self) -> Result<(HashMap<u64, Node>, RangeSet)> {
        let mut nodes = HashMap::new();
        let mut unreadable = RangeSet::new();
        let mut ext_map: HashMap<u64, Vec<u64>> = HashMap::new();
        let per_chunk = ((256 * 1024) / self.rsize).max(1);
        let mut n = 0u64;
        let mut skipped_bad = 0u64;
        while n < self.record_count {
            let k = per_chunk.min(self.record_count - n);
            let mut buf = vec![0u8; (k * self.rsize) as usize];
            let mut ann = crate::stream::Annotations::default();
            crate::stream::read_plain(
                &self.disk,
                &self.mft,
                self.cluster,
                n * self.rsize,
                &mut buf,
                &mut ann,
                None,
                false,
            )?;
            for i in 0..k {
                let rec_no = n + i;
                let a = (i * self.rsize) as usize;
                let raw = buf[a..a + self.rsize as usize].to_vec();
                let rec = match classify_record(rec_no, raw, &ann, rec_no * self.rsize) {
                    RecordRead::Ok(r) => r,
                    RecordRead::Unreadable => {
                        if unreadable.total_len() < MAX_RECORD_WARNINGS {
                            log::warn!("NTFS: MFT record {rec_no} is unreadable; skipped");
                        }
                        unreadable.insert(Range::with_len(rec_no, 1));
                        continue;
                    }
                    RecordRead::Invalid(crate::record::RecordError::Baad) => {
                        skipped_bad += 1;
                        log::warn!("NTFS: MFT record {rec_no} is marked BAAD; skipped");
                        continue;
                    }
                    RecordRead::Invalid(crate::record::RecordError::Corrupt(e)) => {
                        log::debug!("NTFS: MFT record {rec_no}: {e}");
                        continue;
                    }
                    RecordRead::Invalid(_) => continue,
                };
                if let Some(base) = rec.base_record() {
                    ext_map.entry(base).or_default().push(rec_no);
                    continue;
                }
                if !rec.in_use() && !self.opts.include_deleted {
                    continue;
                }
                if rec.is_torn() {
                    log::warn!("NTFS: MFT record {rec_no} has a torn write; parsed anyway");
                }
                let (own, _) = rec.own_attributes(Some(self.total_clusters));
                let mut names: Vec<FileName> = own
                    .iter()
                    .filter(|a| a.type_code == types::FILE_NAME)
                    .filter_map(|a| a.value().and_then(FileName::parse))
                    .collect();
                let mut has_data = own
                    .iter()
                    .any(|a| a.type_code == types::DATA && a.name.is_empty() && a.data_size() > 0);
                // Names (hard links) and data may continue in extension
                // records.
                if own.iter().any(|a| a.type_code == types::ATTRIBUTE_LIST) {
                    let all = self.fold(&rec, false).attrs;
                    names = all
                        .iter()
                        .filter(|a| a.type_code == types::FILE_NAME)
                        .filter_map(|a| a.value().and_then(FileName::parse))
                        .collect();
                    has_data |= all.iter().any(|a| {
                        a.type_code == types::DATA && a.name.is_empty() && a.data_size() > 0
                    });
                }
                nodes.insert(
                    rec_no,
                    Node {
                        seq: rec.sequence,
                        in_use: rec.in_use(),
                        is_dir: rec.is_directory(),
                        has_data,
                        links: select_links(&names),
                    },
                );
            }
            n += k;
        }
        if skipped_bad > 0 {
            log::warn!("NTFS: {skipped_bad} BAAD MFT records skipped");
        }
        if unreadable.total_len() > MAX_RECORD_WARNINGS {
            log::warn!(
                "NTFS: {} MFT records unreadable in total ({} ranges); skipped",
                unreadable.total_len(),
                unreadable.len()
            );
        }
        let _ = self.ext_map.set(ext_map);
        Ok((nodes, unreadable))
    }

    // ----- whole-MFT walk ----------------------------------------------

    fn walk_mft(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        let (nodes, unreadable) = self.scan_nodes()?;
        let mut children: HashMap<u64, Vec<(u64, u16)>> = HashMap::new();
        let mut roots: Vec<(u64, u16, &'static str)> = Vec::new();
        let mut recs: Vec<u64> = nodes.keys().copied().collect();
        recs.sort_unstable();
        for &rec in &recs {
            let node = &nodes[&rec];
            if rec == ROOT || is_system(rec) {
                continue;
            }
            if node.links.is_empty() {
                if rec >= FIRST_UNRESERVED && node.has_data {
                    roots.push((rec, u16::MAX, flags::NAMELESS));
                }
                continue;
            }
            for (li, (parent, _)) in node.links.iter().enumerate() {
                match place(&nodes, rec, *parent) {
                    Ok(p) => children.entry(p).or_default().push((rec, li as u16)),
                    Err(reason) => roots.push((rec, li as u16, reason)),
                }
            }
        }
        // Reachability: everything hanging off the root, system records and
        // orphan roots; whatever is left sits in a parent cycle.
        let mut reached: HashSet<u64> = HashSet::new();
        let mut expanded: HashSet<u64> = HashSet::new();
        let mut queue: Vec<u64> = vec![ROOT];
        queue.extend((0..FIRST_USER_RECORD).filter(|r| *r != ROOT));
        queue.extend(roots.iter().map(|r| r.0));
        reach(&children, &nodes, &mut queue, &mut reached, &mut expanded);
        for &rec in &recs {
            if rec == ROOT || is_system(rec) || reached.contains(&rec) {
                continue;
            }
            let node = &nodes[&rec];
            if node.links.is_empty() {
                continue;
            }
            roots.push((rec, 0, flags::CYCLE));
            let mut q = vec![rec];
            reach(&children, &nodes, &mut q, &mut reached, &mut expanded);
        }
        roots.sort_by_key(|r| (r.0, r.1));

        let mut em = Emitter {
            fs: self,
            visit,
            nodes: Some(&nodes),
            unreadable: &unreadable,
            children: &children,
            expanded: HashSet::new(),
            emitted: HashSet::new(),
            synthetic: 0,
            deferred: Vec::new(),
            unmatched: HashMap::new(),
        };
        let root_items = em.items_for_dir(ROOT)?;
        if em.run(Frame {
            prefix: String::new(),
            parent_id: Some(ROOT),
            items: root_items,
            idx: 0,
            depth: 0,
        })? {
            return Ok(());
        }
        em.emit_orphans(roots)
    }

    // ----- directory-tree walk -------------------------------------------

    fn walk_tree(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        if self.file_info(ROOT)?.is_none() {
            return Err(Error::corrupt(
                "NTFS root directory",
                "record 5 is unreadable; walk the whole MFT instead",
            ));
        }
        let children = HashMap::new();
        let unreadable = RangeSet::new();
        let mut em = Emitter {
            fs: self,
            visit,
            nodes: None,
            unreadable: &unreadable,
            children: &children,
            expanded: HashSet::new(),
            emitted: HashSet::new(),
            synthetic: 0,
            deferred: Vec::new(),
            unmatched: HashMap::new(),
        };
        em.expanded.insert(ROOT);
        let items = em.items_for_dir(ROOT)?;
        if em.run(Frame {
            prefix: String::new(),
            parent_id: Some(ROOT),
            items,
            idx: 0,
            depth: 0,
        })? {
            return Ok(());
        }
        em.emit_orphans(Vec::new())
    }

    // ----- entry construction ---------------------------------------------

    fn describe_stream(&self, stream: Option<&Stream>, deleted: bool) -> Result<StreamDesc> {
        let live = |confidence| StreamDesc {
            size: 0,
            extents: Vec::new(),
            resident: false,
            confidence,
            flags: Vec::new(),
        };
        let Some(s) = stream else {
            return Ok(live(if deleted {
                EntryConfidence::DeletedNoData
            } else {
                EntryConfidence::Allocated
            }));
        };
        let mut fl = Vec::new();
        match &s.data {
            StreamData::Resident(v) => Ok(StreamDesc {
                size: v.len() as u64,
                extents: Vec::new(),
                resident: true,
                confidence: if deleted {
                    EntryConfidence::DeletedIntact
                } else {
                    EntryConfidence::Allocated
                },
                flags: fl,
            }),
            StreamData::NonResident(n) => {
                let compressed = s.is_compressed();
                if compressed {
                    fl.push(flags::COMPRESSED);
                }
                if s.flags & ATTR_SPARSE != 0
                    || (!compressed && n.extents.iter().any(|e| e.kind == ExtentKind::Sparse))
                {
                    fl.push(flags::SPARSE);
                }
                let limit = if compressed { u64::MAX } else { n.data_size };
                let extents = n.byte_extents(self.cluster, limit);
                let needed_vcn = if compressed {
                    n.vcn_end()
                } else {
                    n.data_size.div_ceil(self.cluster)
                };
                if n.size_clamped {
                    fl.push(flags::SIZE_CLAMPED);
                }
                let missing = n
                    .extents
                    .iter()
                    .any(|e| e.kind == ExtentKind::Missing && e.vcn < needed_vcn)
                    || n.vcn_end() < needed_vcn
                    || n.size_clamped;
                if missing {
                    fl.push(flags::INCOMPLETE);
                }
                let confidence = if !deleted {
                    EntryConfidence::Allocated
                } else {
                    let real = n.real_clusters(needed_vcn);
                    if missing && real.is_empty() && n.data_size > 0 {
                        EntryConfidence::DeletedNoData
                    } else if missing || self.any_claimed(&real)? {
                        EntryConfidence::DeletedGuessed
                    } else {
                        EntryConfidence::DeletedIntact
                    }
                };
                Ok(StreamDesc {
                    size: n.data_size,
                    extents,
                    resident: false,
                    confidence,
                    flags: fl,
                })
            }
        }
    }

    /// True when any of the clusters is allocated now, or its state is
    /// unknown.
    fn any_claimed(&self, runs: &[(u64, u64)]) -> Result<bool> {
        let Some(bm) = &self.bitmap else {
            return Ok(!runs.is_empty());
        };
        let mut hit = false;
        for &(lcn, count) in runs {
            bm.for_each_state(
                &self.disk,
                lcn,
                lcn.saturating_add(count),
                &mut |_, _, st| {
                    if st != ClusterState::Free {
                        hit = true;
                    }
                },
            )?;
            if hit {
                break;
            }
        }
        Ok(hit)
    }

    /// Entries for one link of a record: the main entry, then (for the
    /// primary link) one entry per alternate data stream.
    #[allow(clippy::too_many_arguments)]
    fn entries_for(
        &self,
        info: &FileInfo,
        link: u16,
        name: &str,
        prefix: &str,
        parent_id: Option<u64>,
        extra: &[&'static str],
    ) -> Result<Vec<FileEntry>> {
        let rec = &info.record;
        let deleted = !rec.in_use();
        let is_dir = rec.is_directory();
        let mut base_flags: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        if info.encrypted {
            base_flags.push(flags::ENCRYPTED.into());
        }
        if let Some(tag) = info.reparse_tag {
            base_flags.push(flags::REPARSE.into());
            match tag {
                REPARSE_TAG_SYMLINK => base_flags.push(flags::SYMLINK.into()),
                REPARSE_TAG_MOUNT_POINT => base_flags.push(flags::JUNCTION.into()),
                _ => {}
            }
        }
        if info.torn {
            base_flags.push(flags::TORN.into());
        }
        if info.guessed {
            base_flags.push(flags::PIECES_GUESSED.into());
        }
        let fname = info
            .names
            .iter()
            .find(|n| n.name == name)
            .or(info.names.first());
        let (created, modified, accessed) = match (&info.si, fname) {
            (Some(si), _) => (ts(si.created), ts(si.modified), ts(si.accessed)),
            (None, Some(f)) => (ts(f.created), ts(f.modified), ts(f.accessed)),
            (None, None) => (None, None, None),
        };
        let name = safe_name(name);
        let sub = if link == u16::MAX { 0 } else { link as u64 };
        let mut main_flags = base_flags.clone();
        if sub > 0 {
            main_flags.push(flags::HARDLINK.into());
        }
        let desc = if is_dir {
            StreamDesc {
                size: 0,
                extents: Vec::new(),
                resident: false,
                confidence: if deleted {
                    EntryConfidence::DeletedIntact
                } else {
                    EntryConfidence::Allocated
                },
                flags: Vec::new(),
            }
        } else {
            self.describe_stream(info.data.as_ref(), deleted)?
        };
        main_flags.extend(desc.flags.iter().map(|s| s.to_string()));
        let confidence = guessed_cap(desc.confidence, info.guessed || info.torn);
        let main = FileEntry {
            id: make_id(rec.number, sub),
            parent_id,
            path: format!("{prefix}{name}"),
            name: name.clone(),
            size: desc.size,
            is_dir,
            deleted,
            confidence,
            created,
            modified,
            accessed,
            extents: desc.extents,
            resident: desc.resident,
            flags: main_flags,
        };
        let mut out = vec![main];
        if sub == 0 {
            for (j, s) in info
                .ads
                .iter()
                .enumerate()
                .take((SUB_SYNTHETIC - SUB_ADS) as usize)
            {
                let d = self.describe_stream(Some(s), deleted)?;
                let mut fl = base_flags.clone();
                fl.push(flags::ADS.into());
                fl.extend(d.flags.iter().map(|s| s.to_string()));
                let ads_name = format!("{name}:{}", safe_name(&s.name));
                out.push(FileEntry {
                    id: make_id(rec.number, SUB_ADS + j as u64),
                    parent_id,
                    path: format!("{prefix}{ads_name}"),
                    name: ads_name,
                    size: d.size,
                    is_dir: false,
                    deleted,
                    confidence: guessed_cap(d.confidence, info.guessed || info.torn),
                    created,
                    modified,
                    accessed,
                    extents: d.extents,
                    resident: d.resident,
                    flags: fl,
                });
            }
        }
        Ok(out)
    }
}

/// Make an on-disk name safe as one `/`-separated path component: `/` and
/// NUL become `_`, and `.` / `..` (never valid NTFS names) get a `_`
/// prefix, so a hostile name cannot add or climb path levels.
pub fn safe_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out.insert(0, '_');
    }
    out
}

/// A deleted entry whose attribute pieces were guessed, or whose record
/// had a torn write (its run list may be stale), is at best
/// [`EntryConfidence::DeletedGuessed`].
fn guessed_cap(c: EntryConfidence, guessed: bool) -> EntryConfidence {
    if guessed && c == EntryConfidence::DeletedIntact {
        EntryConfidence::DeletedGuessed
    } else {
        c
    }
}

/// Where a link of `rec` with parent reference `parent` belongs.
fn place(
    nodes: &HashMap<u64, Node>,
    rec: u64,
    parent: u64,
) -> std::result::Result<u64, &'static str> {
    let p = mref(parent);
    let ps = mseq(parent);
    if p == ROOT {
        return Ok(ROOT);
    }
    if p == rec {
        return Err(flags::CYCLE);
    }
    match nodes.get(&p) {
        None => Err(flags::PARENT_MISSING),
        Some(pn) if !sequence_matches(pn.seq, pn.in_use, ps) => Err(flags::PARENT_REUSED),
        Some(pn) if !pn.is_dir => Err(flags::PARENT_NOT_DIR),
        Some(_) => Ok(p),
    }
}

fn reach(
    children: &HashMap<u64, Vec<(u64, u16)>>,
    nodes: &HashMap<u64, Node>,
    queue: &mut Vec<u64>,
    reached: &mut HashSet<u64>,
    expanded: &mut HashSet<u64>,
) {
    while let Some(r) = queue.pop() {
        reached.insert(r);
        if !expanded.insert(r) {
            continue;
        }
        if let Some(kids) = children.get(&r) {
            for &(c, _) in kids {
                reached.insert(c);
                if nodes.get(&c).is_some_and(|n| n.is_dir) && !expanded.contains(&c) {
                    queue.push(c);
                }
            }
        }
    }
}

struct Emitter<'a, 'v> {
    fs: &'a NtfsFs,
    visit: &'v mut dyn FnMut(&FileEntry) -> WalkControl,
    /// Pass-1 nodes (whole-MFT mode) or `None` (directory-tree mode).
    nodes: Option<&'a HashMap<u64, Node>>,
    /// Record numbers (as positions) whose MFT record was unreadable.
    unreadable: &'a RangeSet,
    children: &'a HashMap<u64, Vec<(u64, u16)>>,
    expanded: HashSet<u64>,
    emitted: HashSet<u64>,
    synthetic: u64,
    /// Directories whose children were cut off by the depth limit.
    deferred: Vec<(u64, u16)>,
    /// Per-record count of index names not matching any of its links.
    unmatched: HashMap<u64, u16>,
}

impl Emitter<'_, '_> {
    fn next_synthetic(&mut self) -> u64 {
        self.synthetic += 1;
        make_id(self.synthetic, SUB_SYNTHETIC)
    }

    fn virtual_dir(&mut self, path: String, name: String, parent_id: Option<u64>) -> FileEntry {
        FileEntry {
            id: self.next_synthetic(),
            parent_id,
            path,
            name,
            size: 0,
            is_dir: true,
            deleted: false,
            confidence: EntryConfidence::Allocated,
            created: None,
            modified: None,
            accessed: None,
            extents: Vec::new(),
            resident: false,
            flags: vec![flags::VIRTUAL.into()],
        }
    }

    /// Run a frame (and everything pushed beneath it) to completion.
    /// Returns `true` when the visitor asked to stop.
    fn run(&mut self, first: Frame) -> Result<bool> {
        let mut stack = vec![first];
        while let Some(top) = stack.last_mut() {
            if top.idx >= top.items.len() {
                stack.pop();
                continue;
            }
            let i = top.idx;
            top.idx += 1;
            let item = std::mem::replace(
                &mut top.items[i],
                Item::Rec {
                    rec: 0,
                    link: 0,
                    name: String::new(),
                    extra: Vec::new(),
                },
            );
            let prefix = top.prefix.clone();
            let parent_id = top.parent_id;
            let depth = top.depth;
            match item {
                Item::Ready(mut e) => {
                    e.path = format!("{prefix}{}", e.name);
                    e.parent_id = parent_id;
                    if (self.visit)(&e) == WalkControl::Stop {
                        return Ok(true);
                    }
                }
                Item::Rec {
                    rec,
                    link,
                    name,
                    extra,
                } => {
                    let info = match self.fs.file_info(rec) {
                        Ok(Some(i)) => i,
                        Ok(None) => continue,
                        Err(e) if is_fatal(&e) => return Err(e),
                        Err(e) => {
                            log::warn!("NTFS: record {rec}: {e}");
                            continue;
                        }
                    };
                    let entries = self
                        .fs
                        .entries_for(&info, link, &name, &prefix, parent_id, &extra)?;
                    // Directories can be reached twice through damaged parent
                    // chains; index-mode listings can repeat files.
                    let track = entries[0].is_dir || self.nodes.is_none();
                    if track && !self.emitted.insert(entries[0].id) {
                        continue;
                    }
                    let main_ctl = (self.visit)(&entries[0]);
                    if main_ctl == WalkControl::Stop {
                        return Ok(true);
                    }
                    for e in &entries[1..] {
                        if (self.visit)(e) == WalkControl::Stop {
                            return Ok(true);
                        }
                    }
                    let main = &entries[0];
                    if main.is_dir
                        && main_ctl == WalkControl::Continue
                        && !self.expanded.contains(&rec)
                    {
                        if depth + 1 >= MAX_DEPTH {
                            self.deferred.push((rec, link));
                            continue;
                        }
                        self.expanded.insert(rec);
                        let items = self.items_for_dir_info(&info)?;
                        stack.push(Frame {
                            prefix: format!("{}/", main.path),
                            parent_id: Some(main.id),
                            items,
                            idx: 0,
                            depth: depth + 1,
                        });
                    }
                }
            }
        }
        Ok(false)
    }

    fn items_for_dir(&mut self, dir: u64) -> Result<Vec<Item>> {
        match self.fs.file_info(dir)? {
            Some(info) => self.items_for_dir_info(&info),
            None => {
                // Root unreadable: children are still known from pass 1.
                let mut items = self.child_items(dir);
                sort_items(&mut items);
                Ok(items)
            }
        }
    }

    fn child_items(&self, dir: u64) -> Vec<Item> {
        let Some(nodes) = self.nodes else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if let Some(kids) = self.children.get(&dir) {
            for &(c, li) in kids {
                if let Some(n) = nodes.get(&c) {
                    if let Some((_, name)) = n.links.get(li as usize) {
                        items.push(Item::Rec {
                            rec: c,
                            link: li,
                            name: name.clone(),
                            extra: Vec::new(),
                        });
                    }
                }
            }
        }
        items
    }

    fn items_for_dir_info(&mut self, info: &FileInfo) -> Result<Vec<Item>> {
        let dir = info.record.number;
        let mut items = if self.nodes.is_some() {
            let mut v = self.child_items(dir);
            if self.fs.opts.include_deleted {
                let names: HashSet<String> = v
                    .iter()
                    .filter_map(|i| match i {
                        Item::Rec { name, .. } => Some(name.clone()),
                        Item::Ready(_) => None,
                    })
                    .collect();
                v.extend(self.index_extras_mft(info, &names)?);
            }
            v
        } else {
            self.index_items(info)?
        };
        sort_items(&mut items);
        Ok(items)
    }

    /// Name-only entries from a directory's index (whole-MFT mode): index
    /// entries whose record no longer holds that file, and slack keys whose
    /// file reference was overwritten and whose name the directory does not
    /// list any more. `child_names` are the names already placed in it.
    fn index_extras_mft(
        &mut self,
        info: &FileInfo,
        child_names: &HashSet<String>,
    ) -> Result<Vec<Item>> {
        let nodes = self.nodes.expect("whole-MFT mode");
        let dir = info.record.number;
        let cands = self.fs.index_candidates(dir, &info.attrs, true)?;
        let mut groups: BTreeMap<u64, Vec<crate::index::IndexCandidate>> = BTreeMap::new();
        let mut unknown = Vec::new();
        for c in cands {
            let crec = mref(c.reference);
            if crec == dir {
                continue;
            }
            if crec == 0 {
                if lost_reference(&c, dir) && !child_names.contains(&c.key.name) {
                    unknown.push(c);
                }
                continue;
            }
            if crec < FIRST_USER_RECORD {
                continue;
            }
            if let Some(n) = nodes.get(&crec) {
                if sequence_matches(n.seq, n.in_use, mseq(c.reference)) {
                    continue;
                }
            }
            groups.entry(crec).or_default().push(c);
        }
        let mut out = Vec::new();
        for (crec, cands) in groups {
            let unavailable = self.unreadable.contains(crec) || crec >= self.fs.record_count;
            for c in pick_names(cands) {
                let e = self.name_only_entry(&c, unavailable);
                out.push(Item::Ready(Box::new(e)));
            }
        }
        for c in pick_names(unknown) {
            let e = self.name_only_entry(&c, false);
            out.push(Item::Ready(Box::new(e)));
        }
        Ok(out)
    }

    /// Children of a directory from its index (directory-tree mode).
    fn index_items(&mut self, info: &FileInfo) -> Result<Vec<Item>> {
        let dir = info.record.number;
        let want_deleted = self.fs.opts.include_deleted;
        let cands = self.fs.index_candidates(dir, &info.attrs, want_deleted)?;
        let mut groups: BTreeMap<u64, Vec<crate::index::IndexCandidate>> = BTreeMap::new();
        let mut unknown = Vec::new();
        for c in cands {
            let crec = mref(c.reference);
            if crec == dir {
                continue;
            }
            if crec == 0 {
                if want_deleted && lost_reference(&c, dir) {
                    unknown.push(c);
                }
                continue;
            }
            if crec < FIRST_USER_RECORD {
                continue;
            }
            groups.entry(crec).or_default().push(c);
        }
        let mut out = Vec::new();
        for (crec, cands) in groups {
            let rr = if crec >= self.fs.record_count {
                RecordRead::Unreadable
            } else {
                self.fs.read_record(crec)?
            };
            for c in pick_names(cands) {
                let cseq = mseq(c.reference);
                let live_src = c.source == IndexSource::Live;
                match &rr {
                    RecordRead::Ok(rec) if !rec.is_extension() => {
                        let same_file = sequence_matches(rec.sequence, rec.in_use(), cseq)
                            && (rec.in_use() || want_deleted);
                        if same_file {
                            if rec.in_use() && !live_src {
                                // Stale copy of a live file's entry.
                                continue;
                            }
                            let links = select_links(&self.fs.record_names(rec));
                            let li = links
                                .iter()
                                .position(|(p, n)| mref(*p) == dir && *n == c.key.name);
                            let link = match li {
                                Some(li) => Some(li as u16),
                                // The index names the file but its record
                                // does not (damaged FILE_NAME): give it an
                                // id of its own.
                                None if rec.in_use() || links.is_empty() => {
                                    let k = self.unmatched.entry(crec).or_insert(0);
                                    *k += 1;
                                    let sub = (links.len() as u64 + *k as u64).min(SUB_LINK_MAX);
                                    Some(sub as u16)
                                }
                                None => None,
                            };
                            if let Some(link) = link {
                                out.push(Item::Rec {
                                    rec: crec,
                                    link,
                                    name: c.key.name.clone(),
                                    extra: Vec::new(),
                                });
                            }
                        } else if want_deleted {
                            let e = self.name_only_entry(&c, false);
                            out.push(Item::Ready(Box::new(e)));
                        }
                    }
                    RecordRead::Unreadable => {
                        if want_deleted || live_src {
                            let e = self.name_only_entry(&c, true);
                            out.push(Item::Ready(Box::new(e)));
                        }
                    }
                    _ => {
                        if want_deleted {
                            let e = self.name_only_entry(&c, false);
                            out.push(Item::Ready(Box::new(e)));
                        }
                    }
                }
            }
        }
        let listed: HashSet<String> = out
            .iter()
            .map(|i| match i {
                Item::Rec { name, .. } => name.clone(),
                Item::Ready(e) => e.name.clone(),
            })
            .collect();
        for c in pick_names(unknown) {
            if !listed.contains(&c.key.name) {
                let e = self.name_only_entry(&c, false);
                out.push(Item::Ready(Box::new(e)));
            }
        }
        Ok(out)
    }

    fn name_only_entry(
        &mut self,
        c: &crate::index::IndexCandidate,
        record_unreadable: bool,
    ) -> FileEntry {
        let mut fl: Vec<String> = Vec::new();
        let deleted = if record_unreadable {
            fl.push(flags::RECORD_UNREADABLE.into());
            c.source != IndexSource::Live
        } else {
            true
        };
        match c.source {
            IndexSource::Slack | IndexSource::UnusedBlock => fl.push(flags::INDEX_SLACK.into()),
            IndexSource::Live => {
                if !record_unreadable {
                    fl.push(flags::STALE_INDEX.into())
                }
            }
        }
        let is_dir = c.key.is_directory();
        // Path and parent id are filled in by the frame that emits it.
        FileEntry {
            id: self.next_synthetic(),
            parent_id: None,
            path: safe_name(&c.key.name),
            name: safe_name(&c.key.name),
            size: if is_dir { 0 } else { c.key.real_size },
            is_dir,
            deleted,
            confidence: EntryConfidence::DeletedNoData,
            created: ts(c.key.created),
            modified: ts(c.key.modified),
            accessed: ts(c.key.accessed),
            extents: Vec::new(),
            resident: false,
            flags: fl,
        }
    }

    /// Emit the `$orphans` tree.
    fn emit_orphans(&mut self, roots: Vec<(u64, u16, &'static str)>) -> Result<()> {
        let mut roots = roots;
        loop {
            if roots.is_empty() && self.deferred.is_empty() {
                return Ok(());
            }
            for (rec, link) in std::mem::take(&mut self.deferred) {
                roots.push((rec, link, flags::TOO_DEEP));
            }
            let top = self.virtual_dir(ORPHANS_DIR.into(), ORPHANS_DIR.into(), None);
            let top_id = top.id;
            match (self.visit)(&top) {
                WalkControl::Stop => return Ok(()),
                WalkControl::SkipChildren => return Ok(()),
                WalkControl::Continue => {}
            }
            for (rec, link, reason) in std::mem::take(&mut roots) {
                let holder_path = format!("{ORPHANS_DIR}/{rec}");
                let holder = self.virtual_dir(holder_path.clone(), rec.to_string(), Some(top_id));
                let holder_id = holder.id;
                match (self.visit)(&holder) {
                    WalkControl::Stop => return Ok(()),
                    WalkControl::SkipChildren => continue,
                    WalkControl::Continue => {}
                }
                let name = match self.nodes.and_then(|n| n.get(&rec)) {
                    Some(n) => n
                        .links
                        .get(link as usize)
                        .map(|l| l.1.clone())
                        .unwrap_or_else(|| format!("record_{rec}")),
                    None => format!("record_{rec}"),
                };
                let extra = vec![flags::ORPHAN, reason];
                let link = if link == u16::MAX { 0 } else { link };
                // Re-emitting a too-deep directory would duplicate its id;
                // emit its children directly instead.
                let items = if reason == flags::TOO_DEEP {
                    match self.fs.file_info(rec)? {
                        Some(info) => {
                            self.expanded.insert(rec);
                            self.items_for_dir_info(&info)?
                        }
                        None => Vec::new(),
                    }
                } else {
                    vec![Item::Rec {
                        rec,
                        link,
                        name,
                        extra,
                    }]
                };
                if self.run(Frame {
                    prefix: format!("{holder_path}/"),
                    parent_id: Some(holder_id),
                    items,
                    idx: 0,
                    depth: 0,
                })? {
                    return Ok(());
                }
            }
        }
    }
}

/// Names of the system files in the root directory.
const SYSTEM_NAMES: [&str; 12] = [
    "$MFT", "$MFTMirr", "$LogFile", "$Volume", "$AttrDef", ".", "$Bitmap", "$Boot", "$BadClus",
    "$Secure", "$UpCase", "$Extend",
];

/// A stale index key whose file reference was overwritten (reads as 0).
/// Live entries with reference 0 are `$MFT` itself, never lost ones.
fn lost_reference(c: &crate::index::IndexCandidate, dir: u64) -> bool {
    c.source != IndexSource::Live
        && c.key.parent_record() == dir
        && !(dir == ROOT && SYSTEM_NAMES.contains(&c.key.name.as_str()))
}

fn sort_items(items: &mut [Item]) {
    items.sort_by_cached_key(|i| i.sort_key());
}

/// Keep one candidate per distinct name of a record, dropping DOS names
/// when a long name exists and preferring live sources.
fn pick_names(mut cands: Vec<crate::index::IndexCandidate>) -> Vec<crate::index::IndexCandidate> {
    cands.sort_by_key(|c| c.source);
    let any_long = cands.iter().any(|c| !c.key.is_dos_only());
    let mut out: Vec<crate::index::IndexCandidate> = Vec::new();
    for c in cands {
        if any_long && c.key.is_dos_only() {
            continue;
        }
        if out.iter().any(|o| o.key.name == c.key.name) {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_change_the_path_structure() {
        assert_eq!(safe_name("a/b"), "a_b");
        assert_eq!(safe_name(".."), "_..");
        assert_eq!(safe_name("."), "_.");
        assert_eq!(safe_name("x\0y"), "x_y");
        assert_eq!(safe_name("normal name.jpg"), "normal name.jpg");
        assert_eq!(safe_name(""), "_");
    }

    #[test]
    fn link_selection_prefers_long_names() {
        let mk = |p: u64, n: &str, ns: u8| {
            FileName::parse(&crate::attr::build_file_name(p, n, ns, 0, 0)).unwrap()
        };
        let names = vec![
            mk(5, "LONGNA~1.JPG", 2),
            mk(5, "Long name.jpg", 1),
            mk(70, "other.jpg", 0),
            mk(70, "other.jpg", 0),
            mk(80, "ONLY~1.TXT", 2),
        ];
        let links = select_links(&names);
        let got: Vec<(u64, &str)> = links.iter().map(|(p, n)| (*p, n.as_str())).collect();
        assert_eq!(
            got,
            vec![(5, "Long name.jpg"), (70, "other.jpg"), (80, "ONLY~1.TXT")]
        );
        assert_eq!(split_id(make_id(77, 3)), (77, 3));
    }
}
