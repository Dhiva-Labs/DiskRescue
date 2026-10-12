//! Enumeration: the directory tree walk followed by the inode sweep.
//!
//! Pass 1 walks the tree depth-first from the root with an explicit stack
//! (no recursion), a set of already-listed directories (hard-linked or
//! looping directories are listed once) and a depth limit of 64. Every
//! directory's slack is scanned for deleted names when deleted entries are
//! wanted; a deleted name whose inode is still in use is a stale leftover
//! of a rename or relink and is dropped, any other is reported in place.
//!
//! Pass 2 reads every initialised inode table (honouring `INODE_UNINIT` and
//! `bg_itable_unused` only when the descriptor checksum vouches for them) in
//! large chunks and collects inodes pass 1 never reached: live ones (lost
//! because a directory block is unreadable) and deleted ones without a
//! recovered name. They are reported under `$orphans/inode_<n>`;
//! unreachable directories are walked so their contents keep their names,
//! and a directory whose `..` is itself an unreached directory is reported
//! inside that parent rather than at the top.
//!
//! Entry ids: the low 32 bits are the inode number; the high bits
//! disambiguate further entries for the same inode (hard links, several
//! deleted names, reused inodes).

use std::collections::{HashMap, HashSet};

use diskrescue_core::fs::{EntryConfidence, FileEntry, WalkControl};
use diskrescue_core::util::bytes::u16le;
use diskrescue_core::{Range, Result};

use crate::dir::RawDirent;
use crate::inode::{FileType, Inode, ROOT_INO};
use crate::map::{Run, MAX_LOGICAL_BLOCKS};
use crate::superblock::features::INCOMPAT_FILETYPE;
use crate::{deleted_confidence, ExtFs, InodeState, ORPHANS_DIR_ID};

/// Deepest directory nesting descended into.
const MAX_DEPTH: u32 = 64;
/// Name of the synthetic directory holding unreachable inodes.
pub(crate) const ORPHANS: &str = "$orphans";

/// How an inode is presented in a [`FileEntry`].
#[derive(Debug, Clone)]
pub(crate) struct Described {
    pub is_dir: bool,
    pub size: u64,
    pub deleted: bool,
    pub confidence: EntryConfidence,
    pub extents: Vec<Range>,
    pub resident: bool,
    pub flags: Vec<String>,
    /// Some content can be read back (data blocks, inline data, target).
    pub has_data: bool,
}

impl Described {
    fn unreadable(is_dir: bool, deleted: bool) -> Described {
        Described {
            is_dir,
            size: 0,
            deleted,
            confidence: EntryConfidence::DeletedNoData,
            extents: Vec::new(),
            resident: false,
            flags: vec!["inode_unreadable".into()],
            has_data: false,
        }
    }
}

/// Volume byte ranges of the initialised runs, clipped to `size`.
pub(crate) fn byte_extents(runs: &[Run], size: u64, bs: u64) -> Vec<Range> {
    runs.iter()
        .filter(|r| !r.uninit)
        .filter_map(|r| {
            let ls = r.logical.saturating_mul(bs);
            if ls >= size {
                return None;
            }
            let len = r.len.saturating_mul(bs).min(size - ls);
            Some(Range::with_len(r.physical.saturating_mul(bs), len))
        })
        .collect()
}

impl ExtFs {
    /// Logical size to use for an inode and whether it was guessed: a
    /// deleted inode whose size was zeroed but whose mapping survived gets
    /// the mapped length.
    pub(crate) fn effective_limit(&self, inode: &Inode, deleted: bool) -> (u64, bool) {
        let bs = self.sb.block_size as u64;
        if deleted && inode.size == 0 {
            (MAX_LOGICAL_BLOCKS, true)
        } else {
            (inode.size.div_ceil(bs), false)
        }
    }

    /// Describe an inode for a [`FileEntry`]. `reused` means the directory
    /// entry's type disagrees with the inode (the inode now belongs to
    /// another file); `hint_dir` is the directory entry's opinion of the
    /// type when the inode cannot tell.
    pub(crate) fn describe(
        &self,
        inode: &Inode,
        state: InodeState,
        reused: bool,
        hint_dir: bool,
    ) -> Described {
        let bs = self.sb.block_size as u64;
        let deleted = state != InodeState::Live;
        let ft = inode.file_type();
        if state == InodeState::Unused || reused {
            let mut d = Described::unreadable(hint_dir, true);
            d.flags = vec![if reused {
                "inode_reused"
            } else {
                "inode_wiped"
            }
            .into()];
            return d;
        }
        let mut d = Described {
            is_dir: ft == FileType::Directory,
            size: 0,
            deleted,
            confidence: EntryConfidence::Allocated,
            extents: Vec::new(),
            resident: false,
            flags: Vec::new(),
            has_data: false,
        };
        if inode.is_encrypted() {
            d.flags.push("encrypted".into());
        }
        if state == InodeState::OpenUnlinked {
            d.flags.push("open_unlinked".into());
        }
        match ft {
            FileType::Directory => {
                if deleted {
                    d.has_data = if inode.has_inline_data() {
                        inode.size > 0
                    } else {
                        let (limit, _) = self.effective_limit(inode, true);
                        self.map_inode(inode, limit).has_data()
                    };
                } else {
                    d.has_data = true;
                }
            }
            FileType::Regular | FileType::Symlink => {
                if ft == FileType::Symlink {
                    d.flags.push("symlink".into());
                }
                d.size = inode.size;
                if inode.has_inline_data() {
                    d.resident = true;
                    d.flags.push("inline".into());
                    d.has_data = inode.size > 0 && !inode.inline_data().is_empty();
                } else if inode.is_fast_symlink(self.sb.block_size) {
                    d.resident = true;
                    d.has_data = inode.size > 0;
                } else {
                    let (limit, guessed) = self.effective_limit(inode, deleted);
                    let m = self.map_inode(inode, limit);
                    if guessed && m.has_data() {
                        d.size = m.mapped_end().saturating_mul(bs);
                        d.flags.push("size_guessed".into());
                    }
                    d.has_data = m.has_data();
                    if m.runs.iter().any(|r| r.uninit) {
                        d.flags.push("uninit_extents".into());
                    }
                    if !m.unknown.is_empty() {
                        d.flags.push("mapping_incomplete".into());
                    }
                    let mapped: u64 = m.runs.iter().map(|r| r.len).sum();
                    if mapped < d.size.div_ceil(bs) && m.unknown.is_empty() {
                        d.flags.push("sparse".into());
                    }
                    d.extents = byte_extents(&m.runs, d.size, bs);
                }
            }
            other => {
                d.flags.push(format!("special:{}", other.name()));
            }
        }
        if deleted {
            d.confidence = deleted_confidence(state, d.has_data);
        }
        d
    }
}

struct Frame {
    id: u64,
    path: String,
    deleted: bool,
    depth: u32,
    children: Vec<RawDirent>,
    next: usize,
}

struct Parent {
    id: u64,
    path: String,
    deleted: bool,
    depth: u32,
}

/// One walk over the volume.
pub(crate) struct Walker<'a> {
    fs: &'a ExtFs,
    visit: &'a mut dyn FnMut(&FileEntry) -> WalkControl,
    reached: HashSet<u32>,
    listed_dirs: HashSet<u32>,
    dups: HashMap<u32, u32>,
    emitted: u64,
    stop: bool,
    orphans_dir: Option<bool>,
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

impl<'a> Walker<'a> {
    pub(crate) fn new(fs: &'a ExtFs, visit: &'a mut dyn FnMut(&FileEntry) -> WalkControl) -> Self {
        Walker {
            fs,
            visit,
            reached: HashSet::new(),
            listed_dirs: HashSet::new(),
            dups: HashMap::new(),
            emitted: 0,
            stop: false,
            orphans_dir: None,
        }
    }

    pub(crate) fn run(mut self) -> Result<()> {
        let fs = self.fs;
        match fs.inode(ROOT_INO) {
            Ok(root) if root.is_dir() => {
                self.reached.insert(ROOT_INO);
                self.listed_dirs.insert(ROOT_INO);
                let children = fs.list_dir(&root, fs.opts.include_deleted);
                self.dfs(Frame {
                    id: ROOT_INO as u64,
                    path: String::new(),
                    deleted: false,
                    depth: 1,
                    children,
                    next: 0,
                });
            }
            Ok(_) => log::warn!("root inode is not a directory; relying on the inode sweep"),
            Err(e) => log::warn!("root inode unreadable ({e}); relying on the inode sweep"),
        }
        if !self.stop {
            self.sweep();
        }
        Ok(())
    }

    /// Id for a new entry of `ino`; the first one is the inode number.
    fn next_id(&mut self, ino: u32) -> u64 {
        if self.reached.insert(ino) {
            ino as u64
        } else {
            self.alias_id(ino)
        }
    }

    /// Id that never collides with the inode's primary id.
    fn alias_id(&mut self, ino: u32) -> u64 {
        let n = self.dups.entry(ino).or_insert(0);
        *n = (*n + 1).min(0x7FFF_FFFF);
        ((*n as u64) << 32) | ino as u64
    }

    fn emit(&mut self, e: &FileEntry) -> WalkControl {
        if self.stop {
            return WalkControl::Stop;
        }
        self.emitted += 1;
        if self.emitted > self.fs.opts.max_inodes {
            log::warn!(
                "walk stopped after {} entries (max_inodes)",
                self.fs.opts.max_inodes
            );
            self.stop = true;
            return WalkControl::Stop;
        }
        let ctl = (self.visit)(e);
        if ctl == WalkControl::Stop {
            self.stop = true;
        }
        ctl
    }

    fn emit_entry(
        &mut self,
        id: u64,
        parent_id: u64,
        path: String,
        name: String,
        d: Described,
        inode: Option<&Inode>,
    ) -> WalkControl {
        let entry = FileEntry {
            id,
            parent_id: Some(parent_id),
            path,
            name,
            size: d.size,
            is_dir: d.is_dir,
            deleted: d.deleted,
            confidence: d.confidence,
            created: inode.and_then(|i| i.crtime),
            modified: inode.and_then(|i| i.mtime),
            accessed: inode.and_then(|i| i.atime),
            extents: d.extents,
            resident: d.resident,
            flags: d.flags,
        };
        self.emit(&entry)
    }

    fn dfs(&mut self, first: Frame) {
        let mut stack = vec![first];
        while !self.stop {
            let Some(top) = stack.last_mut() else { break };
            if top.next >= top.children.len() {
                stack.pop();
                continue;
            }
            let child = std::mem::take(&mut top.children[top.next]);
            top.next += 1;
            let parent = Parent {
                id: top.id,
                path: top.path.clone(),
                deleted: top.deleted,
                depth: top.depth,
            };
            if let Some(f) = self.child(&parent, child) {
                stack.push(f);
            }
        }
    }

    /// Report one directory entry; returns a frame when it is a directory
    /// to descend into.
    fn child(&mut self, p: &Parent, c: RawDirent) -> Option<Frame> {
        let fs = self.fs;
        if c.inode == 0 || c.inode > fs.sb.inodes_count || c.name == b"." || c.name == b".." {
            return None;
        }
        let in_deleted = c.hidden || p.deleted;
        if in_deleted && !fs.opts.include_deleted {
            return None;
        }
        let name = String::from_utf8_lossy(&c.name).into_owned();
        let path = join(&p.path, &name);
        let hint_dir = c.file_type == 2;
        let inode = match fs.inode(c.inode) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("inode {} ({path}) unreadable: {e}", c.inode);
                let id = self.next_id(c.inode);
                let d = Described::unreadable(hint_dir, in_deleted);
                self.emit_entry(id, p.id, path, name, d, None);
                return None;
            }
        };
        let state = ExtFs::classify(&inode, fs.inode_bitmap_bit(c.inode));
        if in_deleted && state == InodeState::Live {
            log::trace!("stale name {path} -> live inode {}; skipped", c.inode);
            return None;
        }
        if !in_deleted && state != InodeState::Live && !fs.opts.include_deleted {
            return None;
        }
        let reused = state != InodeState::Live
            && state != InodeState::Unused
            && fs.sb.has_incompat(INCOMPAT_FILETYPE)
            && FileType::from_dirent(c.file_type).is_some_and(|t| t != inode.file_type());
        let d = fs.describe(&inode, state, reused, hint_dir);
        let (is_dir, deleted, has_data) = (d.is_dir, d.deleted, d.has_data);
        // A reused inode belongs to another file the sweep may still report.
        let id = if reused {
            self.alias_id(c.inode)
        } else {
            self.next_id(c.inode)
        };
        let ctl = self.emit_entry(id, p.id, path.clone(), name, d, Some(&inode));
        if !is_dir || ctl != WalkControl::Continue || reused || state == InodeState::Unused {
            return None;
        }
        if deleted && !has_data {
            return None;
        }
        if p.depth >= MAX_DEPTH {
            log::warn!("{path}: deeper than {MAX_DEPTH} levels; not descending");
            return None;
        }
        if !self.listed_dirs.insert(c.inode) {
            return None;
        }
        let children = fs.list_dir(&inode, fs.opts.include_deleted);
        Some(Frame {
            id,
            path,
            deleted: p.deleted || deleted,
            depth: p.depth + 1,
            children,
            next: 0,
        })
    }

    /// Emit the `$orphans` directory once; false when the visitor declined
    /// to descend into it.
    fn ensure_orphans_dir(&mut self) -> bool {
        if let Some(ok) = self.orphans_dir {
            return ok;
        }
        let entry = FileEntry {
            id: ORPHANS_DIR_ID,
            parent_id: Some(ROOT_INO as u64),
            path: ORPHANS.into(),
            name: ORPHANS.into(),
            size: 0,
            is_dir: true,
            deleted: false,
            confidence: EntryConfidence::Allocated,
            created: None,
            modified: None,
            accessed: None,
            extents: Vec::new(),
            resident: false,
            flags: vec!["synthetic".into()],
        };
        let ok = self.emit(&entry) == WalkControl::Continue;
        self.orphans_dir = Some(ok);
        ok
    }

    /// Report an unreachable inode under `$orphans`.
    fn orphan(&mut self, ino: u32) {
        let fs = self.fs;
        let Ok(inode) = fs.inode(ino) else { return };
        let state = ExtFs::classify(&inode, fs.inode_bitmap_bit(ino));
        if state == InodeState::Unused || !self.ensure_orphans_dir() {
            return;
        }
        let name = format!("inode_{ino}");
        let path = format!("{ORPHANS}/{name}");
        let mut d = fs.describe(&inode, state, false, inode.is_dir());
        d.flags.push("orphan".into());
        let (is_dir, deleted, has_data) = (d.is_dir, d.deleted, d.has_data);
        let id = self.next_id(ino);
        let ctl = self.emit_entry(id, ORPHANS_DIR_ID, path.clone(), name, d, Some(&inode));
        if is_dir
            && ctl == WalkControl::Continue
            && (!deleted || has_data)
            && self.listed_dirs.insert(ino)
        {
            let children = fs.list_dir(&inode, fs.opts.include_deleted);
            self.dfs(Frame {
                id,
                path,
                deleted,
                depth: 2,
                children,
                next: 0,
            });
        }
    }

    /// Pass 2: find inodes the tree walk did not reach.
    fn sweep(&mut self) {
        let fs = self.fs;
        let sb = &fs.sb;
        let isz = sb.inode_size as usize;
        let ipg = sb.inodes_per_group as u64;
        let bs = sb.block_size as u64;
        let include_deleted = fs.opts.include_deleted;
        let mut dirs: Vec<u32> = Vec::new();
        let mut files: Vec<u32> = Vec::new();
        let mut examined = 0u64;
        let mut unreadable = 0u64;
        let chunk_slots = ((256 * 1024) / isz).max(1) as u64;
        'groups: for d in &fs.groups {
            if !d.valid {
                continue;
            }
            let trusted = fs.flags_trusted(d);
            if trusted && d.inode_uninit() {
                continue;
            }
            let n = if trusted {
                ipg.saturating_sub(d.itable_unused as u64)
            } else {
                ipg
            };
            let base = d.group as u64 * ipg;
            let bitmap = d
                .inode_bitmap
                .checked_mul(bs)
                .and_then(|o| fs.read_meta(o, ipg.div_ceil(8) as usize).ok());
            let Some(table) = d.inode_table.checked_mul(bs) else {
                continue;
            };
            let mut s = 0u64;
            while s < n {
                let cnt = chunk_slots.min(n - s);
                let (buf, bad) =
                    fs.read_robust(table.saturating_add(s * isz as u64), cnt as usize * isz);
                unreadable += bad.iter().map(|r| r.len()).sum::<u64>() / isz as u64;
                for k in 0..cnt {
                    examined += 1;
                    if examined > fs.opts.max_inodes {
                        log::warn!(
                            "inode sweep stopped after {} inodes (max_inodes)",
                            fs.opts.max_inodes
                        );
                        break 'groups;
                    }
                    let ino64 = base + s + k + 1;
                    if ino64 > sb.inodes_count as u64 {
                        break 'groups;
                    }
                    let ino = ino64 as u32;
                    if sb.is_special_inode(ino) || self.reached.contains(&ino) {
                        continue;
                    }
                    let off = k as usize * isz;
                    let raw = &buf[off..off + isz];
                    if u16le(raw, 0).unwrap_or(0) == 0 {
                        continue;
                    }
                    let Ok(inode) = Inode::parse(ino, raw, sb) else {
                        continue;
                    };
                    let idx = s + k;
                    let bit = bitmap
                        .as_ref()
                        .and_then(|b| b.get((idx / 8) as usize))
                        .map(|byte| byte & (1 << (idx % 8)) != 0);
                    let keep = match ExtFs::classify(&inode, bit) {
                        InodeState::Live => true,
                        InodeState::Deleted | InodeState::OpenUnlinked => include_deleted,
                        InodeState::Unused => false,
                    };
                    if keep {
                        if inode.is_dir() {
                            dirs.push(ino);
                        } else {
                            files.push(ino);
                        }
                    }
                }
                s += cnt;
            }
        }
        if unreadable > 0 {
            log::warn!("inode sweep: {unreadable} inode(s) in unreadable inode-table blocks");
        }
        // Directories first, outermost first, so contents keep their names.
        let candidates: HashSet<u32> = dirs.iter().copied().collect();
        let mut nested = Vec::new();
        for &ino in &dirs {
            if self.stop {
                return;
            }
            if self.reached.contains(&ino) {
                continue;
            }
            let parent = fs.inode(ino).ok().and_then(|i| fs.dir_parent(&i));
            if parent
                .is_some_and(|p| p != ino && candidates.contains(&p) && !self.reached.contains(&p))
            {
                nested.push(ino);
                continue;
            }
            self.orphan(ino);
        }
        for ino in nested.into_iter().chain(files) {
            if self.stop {
                return;
            }
            if !self.reached.contains(&ino) {
                self.orphan(ino);
            }
        }
    }
}
