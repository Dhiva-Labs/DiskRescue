//! ext2 / ext3 / ext4 reader for DiskRescue.
//!
//! [`open`] parses a volume (a [`SharedSource`] spanning exactly one
//! filesystem) into an [`ExtFs`], which implements
//! [`diskrescue_core::fs::FileSystem`]: it lists live and deleted entries,
//! streams file contents with an honest report of holes and unreadable
//! ranges, and computes the free-space map the carver works on.
//!
//! # Algorithm
//!
//! 1. **Superblock** at byte 1024, validated like the kernel does at mount.
//!    When it is unreadable or invalid, the backups in groups 1, 3, 5, 7, 9,
//!    25, ... are tried for 1, 2 and 4 KiB blocks (default geometry); the
//!    copy used is reported in [`VolumeInfo::warnings`].
//! 2. **Group descriptors** (32 or 64 bytes, classic or `meta_bg` layout),
//!    each taken from the primary GDT or, when that copy is unreadable or
//!    invalid, from a backup.
//! 3. **Tree walk** from the root inode: directory blocks are parsed
//!    linearly (htree directories need no hashing), live entries are
//!    reported, and with [`ExtOptions::include_deleted`] the slack inside
//!    every entry is scanned for deleted names. A deleted name whose inode
//!    is free is reported in place (`dir/name`, `deleted = true`); deleted
//!    directories with intact blocks are descended into.
//! 4. **Inode sweep** over every initialised inode table: live inodes the
//!    tree walk did not reach (a lost directory block) and deleted inodes
//!    without a recovered name are reported under `$orphans/inode_<n>`.
//!
//! Deleted-data honesty: the kernel's ext4 driver zeroes the extent tree
//! (and block map) of a deleted inode, so most deleted ext4 files are
//! reported with [`EntryConfidence::DeletedNoData`] (name, timestamps, no
//! data; the carver must recover the content). Inodes whose mapping
//! survived (native ext2 driver, debugfs, a crash mid-unlink) are
//! [`EntryConfidence::DeletedGuessed`]; files that were unlinked while open
//! (orphan list, blocks still allocated) are
//! [`EntryConfidence::DeletedIntact`].
//!
//! # Limits
//!
//! * `bigalloc` volumes are refused (free-space math would be wrong).
//! * Encrypted (fscrypt) files are listed but not read; names are
//!   ciphertext.
//! * The journal is not replayed; a volume with a pending journal gets a
//!   warning and is read as it is on disk.
//! * Descriptors missing from every GDT copy are not inferred from flex_bg
//!   neighbours; their groups' inodes are lost to the walk.
//! * Extended attributes outside the inode body (xattr blocks, EA inodes)
//!   are not read; inline data only needs the in-inode `system.data`.
//!
//! Hostile input: every block number is checked against the volume, extent
//! trees are depth-limited (5) with cycle detection, block maps never read
//! an indirect block twice, directory recursion is cycle-protected and
//! depth-limited, and [`ExtOptions::max_inodes`] caps both the walk and the
//! sweep.

#![warn(missing_docs)]

use std::collections::HashSet;
use std::io::Write;

use diskrescue_core::fs::{
    EntryConfidence, FileEntry, FileReadReport, FileSystem, FsKind, VolumeInfo, WalkControl,
};
use diskrescue_core::source::BlockCache;
use diskrescue_core::{
    Error, Range, RangeSet, ReadError, ReadErrorKind, Result, SharedSource, SourceExt,
};

pub use diskrescue_core as core;

mod alloc;
pub mod csum;
mod dir;
mod gdt;
mod inode;
mod map;
mod read;
mod superblock;
mod walk;

pub use gdt::{bg_flags, desc_checksum, GroupDesc};
pub use inode::{iflags, FileType, Inode, ROOT_INO};
pub use map::{Run, EXTENT_MAGIC, MAX_EXTENT_DEPTH};
pub use superblock::{
    default_backup_offset, features, sparse_backup_groups, Superblock, EXT_MAGIC,
    SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE,
};

use dir::DirParseOpts;
use map::{Geometry, Mapping};
use superblock::features::*;

/// Identifier of the synthetic `$orphans` directory entry. Its low 32 bits
/// are 0, which is never a valid inode number.
pub const ORPHANS_DIR_ID: u64 = 0xFFFF_FFFF_0000_0000;

/// Options for [`open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtOptions {
    /// Report deleted entries (names from directory slack, deleted inodes
    /// from the inode sweep).
    pub include_deleted: bool,
    /// Try the backup superblocks when the primary is unreadable or invalid.
    pub use_backup_superblocks: bool,
    /// Upper bound on inode slots examined by the sweep and on entries
    /// reported by one walk (hostile-input guard).
    pub max_inodes: u64,
}

impl Default for ExtOptions {
    fn default() -> Self {
        ExtOptions {
            include_deleted: true,
            use_backup_superblocks: true,
            max_inodes: 100_000_000,
        }
    }
}

/// Recognise an ext2/3/4 volume and tell which. Reads the superblock at
/// byte 1024; when that one is invalid, also the group-1 backups for 1, 2
/// and 4 KiB blocks (a dead first sector should not hide the volume).
pub fn probe(src: &SharedSource) -> Option<FsKind> {
    let check = |off: u64| -> Option<Superblock> {
        let buf = src.read_exact_vec(off, SUPERBLOCK_SIZE).ok()?;
        let sb = Superblock::parse(&buf).ok()?;
        (!sb.has_incompat(INCOMPAT_JOURNAL_DEV)).then_some(sb)
    };
    if let Some(sb) = check(SUPERBLOCK_OFFSET) {
        return Some(sb.kind());
    }
    for bs in [1024u64, 2048, 4096] {
        let off = default_backup_offset(bs, 1);
        if let Some(sb) = check(off) {
            if backup_matches(&sb, off, 1) {
                return Some(sb.kind());
            }
        }
    }
    None
}

/// Whether a superblock found at `offset`, assumed to be the copy of group
/// `g`, actually describes that location. The copy sits at the first byte
/// of the group's first block (which for 1 KiB blocks is block 1 + g*bpg).
fn backup_matches(sb: &Superblock, offset: u64, g: u32) -> bool {
    let loc = sb.group_first_block(g).saturating_mul(sb.block_size as u64);
    loc == offset && (sb.block_group_nr as u32 == g || sb.block_group_nr == 0)
}

/// Find a usable superblock: the primary, else a backup.
fn find_superblock(
    src: &SharedSource,
    opts: &ExtOptions,
) -> Result<(Superblock, u64, Vec<String>)> {
    let mut warnings = Vec::new();
    let primary_err = match src.read_exact_vec(SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE) {
        Ok(buf) => match Superblock::parse(&buf) {
            Ok(sb) => return Ok((sb, SUPERBLOCK_OFFSET, warnings)),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    if !opts.use_backup_superblocks {
        return Err(Error::corrupt("ext superblock", primary_err));
    }
    let size = src.size();
    for bs in [1024u64, 2048, 4096] {
        let max_groups = size / (8 * bs * bs) + 1;
        for g in sparse_backup_groups(max_groups.min(1 << 20)) {
            let off = default_backup_offset(bs, g);
            if off + SUPERBLOCK_SIZE as u64 > size {
                break;
            }
            let Ok(buf) = src.read_exact_vec(off, SUPERBLOCK_SIZE) else {
                continue;
            };
            let Ok(sb) = Superblock::parse(&buf) else {
                continue;
            };
            if backup_matches(&sb, off, g) {
                let msg = format!(
                    "primary superblock unusable ({primary_err}); using backup from group {g} at byte {off}"
                );
                warnings.push(msg);
                return Ok((sb, off, warnings));
            }
        }
    }
    Err(Error::corrupt(
        "ext superblock",
        format!("{primary_err}; no valid backup superblock found"),
    ))
}

/// Parse an ext2/3/4 volume.
pub fn open(src: SharedSource, opts: &ExtOptions) -> Result<ExtFs> {
    let (sb, sb_offset, mut warnings) = find_superblock(&src, opts)?;
    if sb.has_incompat(INCOMPAT_JOURNAL_DEV) {
        return Err(Error::unsupported(
            "ext external journal device, not a filesystem",
        ));
    }
    if sb.has_ro_compat(RO_COMPAT_BIGALLOC) {
        return Err(Error::unsupported(
            "ext4 bigalloc volumes are not supported (cluster bitmaps)",
        ));
    }
    if sb.has_incompat(INCOMPAT_COMPRESSION) {
        warnings.push("compression feature set; compressed files will read as stored".into());
    }
    if sb.has_incompat(INCOMPAT_RECOVER) {
        warnings.push("journal needs recovery; reading the on-disk state without replay".into());
    }
    if sb.state & 0x2 != 0 {
        warnings.push("filesystem marked as having errors".into());
    }
    if sb.checksum_ok == Some(false) {
        warnings.push("superblock checksum mismatch".into());
    }
    if sb.has_incompat(INCOMPAT_ENCRYPT) {
        warnings.push("encryption feature set; encrypted files are listed but not read".into());
    }
    let bs = sb.block_size as u64;
    let total = sb.blocks_count.saturating_mul(bs);
    if total > src.size() {
        warnings.push(format!(
            "image is shorter than the filesystem ({} of {} bytes); data beyond the end is unreadable",
            src.size(),
            total
        ));
    }
    let bad = src.unreadable().filter(|s| !s.is_empty());
    let cache = BlockCache::new(src.clone(), 64 * 1024, 256);
    let mut fs = ExtFs {
        info: VolumeInfo {
            kind: sb.kind(),
            label: (!sb.volume_name.is_empty()).then(|| sb.volume_name.clone()),
            cluster_size: sb.block_size,
            total_size: total,
            data_start: 0,
            serial: Some(sb.uuid_string()),
            warnings: Vec::new(),
        },
        src,
        cache,
        bad,
        sb,
        sb_offset,
        groups: Vec::new(),
        opts: opts.clone(),
    };
    let source_blocks = fs.src.size() / bs;
    let loaded = gdt::load(&fs.sb, source_blocks, &|b| fs.read_block(b).ok());
    fs.groups = loaded.descs;
    warnings.extend(loaded.warnings);
    for w in &warnings {
        log::warn!("{}: {w}", fs.src.describe());
    }
    fs.info.warnings = warnings;
    log::debug!(
        "{}: {} volume, {} blocks of {} bytes, {} groups, {} inodes",
        fs.src.describe(),
        fs.info.kind,
        fs.sb.blocks_count,
        bs,
        fs.groups.len(),
        fs.sb.inodes_count
    );
    Ok(fs)
}

/// A parsed ext2/3/4 volume.
pub struct ExtFs {
    src: SharedSource,
    cache: BlockCache,
    bad: Option<RangeSet>,
    sb: Superblock,
    sb_offset: u64,
    groups: Vec<GroupDesc>,
    info: VolumeInfo,
    opts: ExtOptions,
}

/// What the parser knows about an inode's allocation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeState {
    /// In use.
    Live,
    /// Unlinked but still allocated (orphan list: deleted while open).
    OpenUnlinked,
    /// Freed.
    Deleted,
    /// Never used or wiped (mode 0).
    Unused,
}

impl ExtFs {
    /// The superblock in use (possibly a backup copy).
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Byte offset of the superblock copy in use (1024 for the primary).
    pub fn superblock_offset(&self) -> u64 {
        self.sb_offset
    }

    /// Descriptor of group `n`, if it could be loaded. Check
    /// [`GroupDesc::valid`] before trusting its locations.
    pub fn group_descriptor(&self, n: u32) -> Option<&GroupDesc> {
        self.groups.get(n as usize)
    }

    /// Number of group descriptors loaded.
    pub fn group_count(&self) -> u32 {
        self.groups.len() as u32
    }

    /// Block size in bytes.
    pub fn block_size(&self) -> u32 {
        self.sb.block_size
    }

    /// The options this volume was opened with.
    pub fn options(&self) -> &ExtOptions {
        &self.opts
    }

    /// Read and decode inode `n`.
    pub fn inode(&self, n: u32) -> Result<Inode> {
        if n == 0 || n > self.sb.inodes_count {
            return Err(Error::InvalidArgument(format!(
                "inode {n} outside 1..={}",
                self.sb.inodes_count
            )));
        }
        let ipg = self.sb.inodes_per_group;
        let g = (n - 1) / ipg;
        let idx = ((n - 1) % ipg) as u64;
        let desc = self
            .groups
            .get(g as usize)
            .filter(|d| d.valid)
            .ok_or_else(|| {
                Error::corrupt("ext group descriptor", format!("group {g} unavailable"))
            })?;
        let isz = self.sb.inode_size as u64;
        let off = desc
            .inode_table
            .checked_mul(self.sb.block_size as u64)
            .and_then(|o| o.checked_add(idx * isz))
            .ok_or_else(|| Error::corrupt("ext inode table", "offset overflow"))?;
        let raw = self.read_meta(off, isz as usize)?;
        Inode::parse(n, &raw, &self.sb)
    }

    /// The logical-to-physical runs of an inode's data, clipped to its
    /// size. Empty for inline data, fast symlinks, special files and inodes
    /// whose mapping was zeroed on deletion. Unreadable parts of the
    /// mapping are simply absent.
    pub fn extents_of(&self, inode: &Inode) -> Vec<Run> {
        let limit = inode.size.div_ceil(self.sb.block_size as u64);
        self.map_inode(inode, limit).runs
    }

    /// Volume byte ranges of the internal journal (inode
    /// `s_journal_inum`). The journal holds recent copies of metadata blocks
    /// and, with `data=journal`, file data, so it is worth carving on its
    /// own. Empty when there is no internal journal or it cannot be mapped.
    pub fn journal_ranges(&self) -> Vec<Range> {
        let ino = self.sb.journal_inum;
        if !self.sb.has_compat(COMPAT_HAS_JOURNAL) || ino == 0 {
            return Vec::new();
        }
        match self.inode(ino) {
            Ok(inode) => walk::byte_extents(
                &self.extents_of(&inode),
                inode.size,
                self.sb.block_size as u64,
            ),
            Err(e) => {
                log::warn!("journal inode {ino} unreadable: {e}");
                Vec::new()
            }
        }
    }

    /// The inode number an entry from [`FileSystem::walk`] refers to
    /// (0 for the synthetic `$orphans` directory).
    pub fn inode_number(entry: &FileEntry) -> u32 {
        entry.id as u32
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            block_size: self.sb.block_size as u64,
            blocks_count: self.sb.blocks_count,
            first_data_block: self.sb.first_data_block as u64,
        }
    }

    pub(crate) fn dir_opts(&self, scan_slack: bool) -> DirParseOpts {
        DirParseOpts {
            filetype: self.sb.has_incompat(INCOMPAT_FILETYPE),
            inodes_count: self.sb.inodes_count,
            scan_slack,
        }
    }

    /// Whether a descriptor's UNINIT flags and `itable_unused` may be
    /// trusted.
    pub(crate) fn flags_trusted(&self, d: &GroupDesc) -> bool {
        self.sb.has_group_desc_csum() && !d.from_backup && d.checksum_ok != Some(false)
    }

    pub(crate) fn known_bad(&self, offset: u64, len: u64) -> bool {
        self.bad
            .as_ref()
            .is_some_and(|s| s.intersects(Range::with_len(offset, len)))
    }

    /// Read metadata through the cache, falling back to an exact direct
    /// read (so one bad sector does not take a whole cache line with it).
    /// Ranges the source already knows to be unreadable fail.
    pub(crate) fn read_meta(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        if self.known_bad(offset, len as u64) {
            return Err(ReadError::new(
                offset,
                len,
                ReadErrorKind::Io,
                "inside a known-unreadable range",
            ));
        }
        match self.cache.read_exact_vec(offset, len) {
            Ok(v) => Ok(v),
            Err(_) => self.src.read_exact_vec(offset, len),
        }
    }

    /// Read filesystem block `b` (must be inside the volume).
    pub(crate) fn read_block(&self, b: u64) -> Result<Vec<u8>, ReadError> {
        let bs = self.sb.block_size as u64;
        if b >= self.sb.blocks_count {
            return Err(ReadError::new(
                b.saturating_mul(bs),
                bs as usize,
                ReadErrorKind::OutOfRange,
                format!("block {b} beyond blocks_count {}", self.sb.blocks_count),
            ));
        }
        self.read_meta(b * bs, bs as usize)
    }

    /// Map an inode's data blocks for its first `limit` logical blocks.
    pub(crate) fn map_inode(&self, inode: &Inode, limit: u64) -> Mapping {
        if inode.has_inline_data() || inode.is_fast_symlink(self.sb.block_size) {
            return Mapping::default();
        }
        match inode.file_type() {
            FileType::Regular | FileType::Directory | FileType::Symlink => {}
            _ => return Mapping::default(),
        }
        let reader = |b: u64| self.read_block(b).ok();
        if inode.has_extents() {
            map::map_extents(&inode.i_block, self.geometry(), limit, &reader)
        } else {
            map::map_blockmap(&inode.i_block, self.geometry(), limit, &reader)
        }
    }

    /// Bit of `ino` in its group's inode bitmap (None when unknown).
    pub(crate) fn inode_bitmap_bit(&self, ino: u32) -> Option<bool> {
        if ino == 0 || ino > self.sb.inodes_count {
            return None;
        }
        let ipg = self.sb.inodes_per_group;
        let g = (ino - 1) / ipg;
        let idx = ((ino - 1) % ipg) as u64;
        let d = self.groups.get(g as usize).filter(|d| d.valid)?;
        if self.flags_trusted(d) && d.inode_uninit() {
            return Some(false);
        }
        let off = d
            .inode_bitmap
            .checked_mul(self.sb.block_size as u64)?
            .checked_add(idx / 8)?;
        let byte = self.read_meta(off, 1).ok()?[0];
        Some(byte & (1 << (idx % 8)) != 0)
    }

    /// Bit of block `b` in its group's block bitmap (None when unknown).
    pub(crate) fn block_bitmap_bit(&self, b: u64) -> Option<bool> {
        let fdb = self.sb.first_data_block as u64;
        if b < fdb || b >= self.sb.blocks_count {
            return None;
        }
        let bpg = self.sb.blocks_per_group as u64;
        let g = (b - fdb) / bpg;
        let idx = (b - fdb) % bpg;
        let d = self.groups.get(g as usize).filter(|d| d.valid)?;
        if self.flags_trusted(d) && d.block_uninit() {
            return None;
        }
        let off = d
            .block_bitmap
            .checked_mul(self.sb.block_size as u64)?
            .checked_add(idx / 8)?;
        let byte = self.read_meta(off, 1).ok()?[0];
        Some(byte & (1 << (idx % 8)) != 0)
    }

    /// Classify an inode given its bitmap bit.
    pub(crate) fn classify(inode: &Inode, bit: Option<bool>) -> InodeState {
        if inode.mode == 0 || inode.file_type() == FileType::Unknown {
            return InodeState::Unused;
        }
        if inode.links_count > 0 && (bit != Some(false) || inode.dtime_raw == 0) {
            return InodeState::Live;
        }
        if inode.links_count == 0 && bit == Some(true) {
            return InodeState::OpenUnlinked;
        }
        InodeState::Deleted
    }

    /// Entries of a directory inode (live and, with `scan_slack`, deleted
    /// names). Unreadable blocks are skipped with a warning.
    pub(crate) fn list_dir(&self, inode: &Inode, scan_slack: bool) -> Vec<dir::RawDirent> {
        const MAX_DIR_ENTRIES: usize = 1 << 20;
        const MAX_DIR_BLOCKS: u64 = 1 << 21;
        let opts = self.dir_opts(scan_slack);
        let mut out = Vec::new();
        if inode.has_inline_data() {
            let (head, tail) = inode.inline_parts();
            dir::parse_block(&head[4..], opts, &mut out);
            if let Some(t) = tail {
                dir::parse_block(t, opts, &mut out);
            }
            return dedup_dirents(out);
        }
        let bs = self.sb.block_size as u64;
        let mut limit = inode.size.div_ceil(bs);
        if limit == 0 {
            // Deleted directories may have lost their size; use the mapping.
            limit = self.map_inode(inode, map::MAX_LOGICAL_BLOCKS).mapped_end();
        }
        let mapping = self.map_inode(inode, limit.min(MAX_DIR_BLOCKS));
        let htree = inode.is_htree();
        'runs: for run in mapping.runs.iter().filter(|r| !r.uninit) {
            for k in 0..run.len {
                if out.len() >= MAX_DIR_ENTRIES {
                    log::warn!(
                        "directory inode {}: more than {MAX_DIR_ENTRIES} entries; truncated",
                        inode.ino
                    );
                    break 'runs;
                }
                let lblk = run.logical + k;
                let data = match self.read_block(run.physical + k) {
                    Ok(d) => d,
                    Err(e) => {
                        log::warn!("directory inode {} block {lblk} unreadable: {e}", inode.ino);
                        continue;
                    }
                };
                if htree && lblk == 0 {
                    let no_slack = DirParseOpts {
                        scan_slack: false,
                        ..opts
                    };
                    dir::parse_block(&data, no_slack, &mut out);
                } else if htree && dir::is_dx_node(&data) {
                    continue;
                } else if !dir::parse_block(&data, opts, &mut out) {
                    log::debug!(
                        "directory inode {} block {lblk}: corrupt entry chain",
                        inode.ino
                    );
                }
            }
        }
        if !mapping.unknown.is_empty() {
            log::warn!(
                "directory inode {}: part of the block mapping is unreadable",
                inode.ino
            );
        }
        dedup_dirents(out)
    }

    /// Parent inode of a directory from its `..` entry.
    pub(crate) fn dir_parent(&self, inode: &Inode) -> Option<u32> {
        if inode.has_inline_data() {
            return diskrescue_core::util::bytes::u32le(&inode.i_block, 0);
        }
        let mapping = self.map_inode(inode, 1);
        let run = mapping
            .runs
            .first()
            .filter(|r| r.logical == 0 && !r.uninit)?;
        let data = self.read_block(run.physical).ok()?;
        let mut out = Vec::new();
        dir::parse_block(&data, self.dir_opts(false), &mut out);
        out.iter().find(|d| d.name == b"..").map(|d| d.inode)
    }
}

/// Drop deleted names that duplicate a live entry or each other.
fn dedup_dirents(v: Vec<dir::RawDirent>) -> Vec<dir::RawDirent> {
    let live: HashSet<(u32, Vec<u8>)> = v
        .iter()
        .filter(|d| !d.hidden)
        .map(|d| (d.inode, d.name.clone()))
        .collect();
    let mut seen: HashSet<(u32, Vec<u8>)> = HashSet::new();
    v.into_iter()
        .filter(|d| {
            if !d.hidden {
                return true;
            }
            let key = (d.inode, d.name.clone());
            !live.contains(&key) && seen.insert(key)
        })
        .collect()
}

/// Confidence for a deleted entry given what survives of its inode.
pub(crate) fn deleted_confidence(state: InodeState, has_data: bool) -> EntryConfidence {
    match (state, has_data) {
        (_, false) => EntryConfidence::DeletedNoData,
        (InodeState::OpenUnlinked, true) => EntryConfidence::DeletedIntact,
        _ => EntryConfidence::DeletedGuessed,
    }
}

impl FileSystem for ExtFs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn walk(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        walk::Walker::new(self, visit).run()
    }

    fn read_file(&self, entry: &FileEntry, sink: &mut dyn Write) -> Result<FileReadReport> {
        self.read_entry(entry, sink)
    }

    fn unallocated(&self) -> Result<RangeSet> {
        Ok(self.compute_unallocated())
    }

    fn source(&self) -> &SharedSource {
        &self.src
    }
}
