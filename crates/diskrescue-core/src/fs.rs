//! The interface every filesystem parser implements.
//!
//! A parser is constructed from a [`SharedSource`] that spans exactly one
//! volume (use [`crate::source::SubSource`] for a partition). It lists
//! existing and deleted entries and streams file contents. It never writes.

use std::io::Write;

use crate::ranges::{Range, RangeSet};
use crate::source::SharedSource;
use crate::time::Timestamp;
use crate::Result;

/// Filesystem family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ntfs,
    Ext2,
    Ext3,
    Ext4,
    /// Recognised but not parsed (HFS+, APFS, btrfs, XFS, ...).
    Other,
}

impl FsKind {
    /// Short lowercase name.
    pub fn name(&self) -> &'static str {
        match self {
            FsKind::Fat12 => "fat12",
            FsKind::Fat16 => "fat16",
            FsKind::Fat32 => "fat32",
            FsKind::ExFat => "exfat",
            FsKind::Ntfs => "ntfs",
            FsKind::Ext2 => "ext2",
            FsKind::Ext3 => "ext3",
            FsKind::Ext4 => "ext4",
            FsKind::Other => "other",
        }
    }
}

impl std::fmt::Display for FsKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How sure the parser is that a (deleted) entry's data is intact.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EntryConfidence {
    /// Live file; allocation known from the filesystem.
    Allocated,
    /// Deleted but the extent list is intact (NTFS, exFAT NoFatChain) and
    /// none of the blocks are reused by a live file.
    DeletedIntact,
    /// Deleted; extents were guessed (FAT contiguity assumption) or some
    /// blocks have been reused.
    DeletedGuessed,
    /// Deleted and the data location is unknown (ext4 zeroed the extent
    /// tree). Only the name/metadata is recoverable; data needs carving.
    DeletedNoData,
}

/// One directory entry (file or directory), live or deleted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    /// Parser-specific identifier (inode number, MFT record, directory entry
    /// offset). Unique within one filesystem instance.
    pub id: u64,
    /// Identifier of the parent directory (same namespace as `id`), if known.
    pub parent_id: Option<u64>,
    /// Full path inside the volume using `/` separators, without a leading
    /// slash. Deleted entries whose parent chain is lost are placed under
    /// `$orphans/`.
    pub path: String,
    /// File name (last path component).
    pub name: String,
    /// Logical size in bytes (0 for directories).
    pub size: u64,
    /// True for directories.
    pub is_dir: bool,
    /// True when the entry was deleted.
    pub deleted: bool,
    /// Confidence that the data can be read back intact.
    pub confidence: EntryConfidence,
    /// Creation time.
    pub created: Option<Timestamp>,
    /// Last modification time.
    pub modified: Option<Timestamp>,
    /// Last access time.
    pub accessed: Option<Timestamp>,
    /// Volume-relative byte extents holding the data, in file order. Empty
    /// when unknown or when the data is resident in metadata (NTFS small
    /// files, ext4 inline data), in which case `read_file` still works.
    pub extents: Vec<Range>,
    /// Data stored inside metadata rather than in data blocks.
    pub resident: bool,
    /// Compressed, encrypted, sparse or otherwise special; the parser may
    /// refuse to read it.
    pub flags: Vec<String>,
}

impl FileEntry {
    /// Lowercase extension without the dot, if any.
    pub fn extension(&self) -> Option<String> {
        let idx = self.name.rfind('.')?;
        if idx == 0 || idx + 1 >= self.name.len() {
            return None;
        }
        Some(self.name[idx + 1..].to_ascii_lowercase())
    }

    /// Total bytes covered by the known extents.
    pub fn extent_bytes(&self) -> u64 {
        self.extents.iter().map(Range::len).sum()
    }
}

/// What `read_file` managed to do.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileReadReport {
    /// Bytes written to the sink (always `size` unless the read was cut short).
    pub bytes_written: u64,
    /// File-relative ranges that could not be read from the source and were
    /// filled with zeros.
    pub unreadable: RangeSet,
    /// File-relative ranges that are sparse holes (legitimately zero).
    pub holes: RangeSet,
    /// Ranges whose blocks are also claimed by a live file (deleted-file
    /// recovery: data likely overwritten).
    pub reused: RangeSet,
    /// Notes for the report.
    pub notes: Vec<String>,
}

impl FileReadReport {
    /// True when every byte was read from the medium.
    pub fn is_complete(&self) -> bool {
        self.unreadable.is_empty() && self.reused.is_empty()
    }
}

/// Control flow for [`FileSystem::walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkControl {
    /// Keep going.
    Continue,
    /// Do not descend into this directory (ignored for files).
    SkipChildren,
    /// Stop the whole walk.
    Stop,
}

/// Volume-level facts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeInfo {
    /// Family.
    pub kind: FsKind,
    /// Volume label, if any.
    pub label: Option<String>,
    /// Allocation unit (cluster/block) size in bytes.
    pub cluster_size: u32,
    /// Total size of the volume in bytes as the filesystem believes it.
    pub total_size: u64,
    /// Volume-relative byte offset where the data area (first cluster/block)
    /// begins. Carvers align candidate starts to `data_start + k * cluster_size`.
    pub data_start: u64,
    /// Serial number / UUID as text, if any.
    pub serial: Option<String>,
    /// Problems found while parsing (backup boot sector used, bad checksums,
    /// bitmap unreadable). The parser continues on a best-effort basis.
    pub warnings: Vec<String>,
}

/// A parsed filesystem volume. Implementations are read-only and thread-safe.
pub trait FileSystem: Send + Sync {
    /// Static facts about the volume.
    fn info(&self) -> &VolumeInfo;

    /// Visit every entry, live and deleted, depth-first. Directories are
    /// reported before their children. Deleted entries that cannot be placed
    /// in the tree appear under `$orphans/`.
    fn walk(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()>;

    /// Stream the file's content to `sink`, zero-filling unreadable or missing
    /// blocks so offsets stay meaningful for later repair. Directories and
    /// entries with `EntryConfidence::DeletedNoData` return an error.
    fn read_file(&self, entry: &FileEntry, sink: &mut dyn Write) -> Result<FileReadReport>;

    /// Volume-relative byte ranges not allocated to any live file (the
    /// carver's "free space"). Includes blocks of deleted files.
    fn unallocated(&self) -> Result<RangeSet>;

    /// Volume-relative byte ranges allocated to live files, if cheaper to
    /// compute than the complement. Default derives it from `unallocated`.
    fn allocated(&self) -> Result<RangeSet> {
        let info = self.info();
        Ok(self
            .unallocated()?
            .complement_within(Range::new(0, info.total_size)))
    }

    /// The source this filesystem reads from.
    fn source(&self) -> &SharedSource;
}

/// Collect all entries of a filesystem into a vector (convenience for tests
/// and small volumes).
pub fn collect_entries(fs: &dyn FileSystem) -> Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    fs.walk(&mut |e| {
        out.push(e.clone());
        WalkControl::Continue
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_parsing() {
        let mut e = FileEntry {
            id: 1,
            parent_id: None,
            path: "DCIM/IMG_0001.JPG".into(),
            name: "IMG_0001.JPG".into(),
            size: 10,
            is_dir: false,
            deleted: false,
            confidence: EntryConfidence::Allocated,
            created: None,
            modified: None,
            accessed: None,
            extents: vec![Range::new(0, 10)],
            resident: false,
            flags: vec![],
        };
        assert_eq!(e.extension().as_deref(), Some("jpg"));
        e.name = ".hidden".into();
        assert_eq!(e.extension(), None);
        e.name = "noext".into();
        assert_eq!(e.extension(), None);
        e.name = "trailing.".into();
        assert_eq!(e.extension(), None);
        assert_eq!(e.extent_bytes(), 10);
    }
}
