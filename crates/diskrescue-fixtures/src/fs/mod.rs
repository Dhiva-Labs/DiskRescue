//! Filesystem image builders.
//!
//! [`build`] creates a populated filesystem image from an [`FsSpec`]:
//!
//! | Filesystem | Formatter | Population | Deletion |
//! |---|---|---|---|
//! | FAT12/16/32 | `mkfs.vfat` (or the built-in formatter when the tool is missing or `native_format` is set) | built-in writer: 8.3 + LFN entries, FAT chains, FSInfo | built-in: `0xE5` on the entries, FAT chain zeroed, data left in place |
//! | exFAT | `mkfs.exfat` | built-in writer: File/Stream/Name entry sets, bitmap, NoFatChain | built-in: InUse bit cleared, bitmap bits cleared |
//! | NTFS | `mkntfs` | `ntfscp` (root-only specs) or an `ntfs-3g` FUSE mount | real deletion through the mount |
//! | ext4 | `mkfs.ext4 -d` | staging directory | `debugfs -w` (extents are kept, as debugfs does) |
//!
//! Without `mkfs.exfat`/`mkntfs`/`mkfs.ext4` the corresponding builder
//! returns [`FixtureError::Missing`]. Without `fusermount3`, `/dev/fuse` or
//! `ntfs-3g` an NTFS spec that needs directories, deletions, timestamps or
//! named streams falls back to flat `ntfscp` population and reports
//! `deleted_supported = false` (see [`BuildReport::notes`]).
//!
//! Everything runs as a normal user. Volumes are sparse files, so a 2 GiB
//! image costs almost nothing on disk.

use std::path::Path;

use crate::error::{FixtureError, Result};

mod exfat;
mod ext4;
mod fat;
mod ntfs;
mod plan;

pub use ntfs::can_mount as ntfs_mount_available;
pub use plan::DEFAULT_MTIME;

/// Filesystem type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FsType {
    /// FAT12.
    Fat12,
    /// FAT16.
    Fat16,
    /// FAT32.
    #[default]
    Fat32,
    /// exFAT.
    ExFat,
    /// NTFS.
    Ntfs,
    /// ext4 (the only ext flavour fixtures use).
    Ext4,
}

impl FsType {
    /// Short lowercase name (`fat32`, `exfat`, ...).
    pub fn name(self) -> &'static str {
        match self {
            FsType::Fat12 => "fat12",
            FsType::Fat16 => "fat16",
            FsType::Fat32 => "fat32",
            FsType::ExFat => "exfat",
            FsType::Ntfs => "ntfs",
            FsType::Ext4 => "ext4",
        }
    }

    /// True for FAT12/16/32.
    pub fn is_fat(self) -> bool {
        matches!(self, FsType::Fat12 | FsType::Fat16 | FsType::Fat32)
    }
}

/// How to place a file's clusters (FAT and exFAT only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    /// Clusters in the first part of the file.
    pub first_clusters: u32,
    /// Free clusters left between the first and the second part (a later,
    /// smaller file placed first-fit will fill them).
    pub gap_clusters: u32,
}

/// One file to write. Construct with [`FileSpec::new`] or
/// `FileSpec { path, data, mtime, ..Default::default() }`; the struct may
/// gain optional fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileSpec {
    /// Path inside the volume, `/`-separated, no leading slash needed.
    /// Parent directories are created implicitly. On NTFS a final component
    /// of the form `name:stream` writes a named data stream.
    pub path: String,
    /// File contents.
    pub data: Vec<u8>,
    /// Modification time as seconds since the Unix epoch (UTC); `None` uses
    /// [`DEFAULT_MTIME`].
    pub mtime: Option<i64>,
    /// Fragmented placement (FAT/exFAT); `None` = one contiguous run.
    pub fragment: Option<Fragment>,
}

impl FileSpec {
    /// A file with the default timestamp and contiguous placement.
    pub fn new(path: impl Into<String>, data: Vec<u8>) -> Self {
        FileSpec {
            path: path.into(),
            data,
            mtime: None,
            fragment: None,
        }
    }

    /// Set the modification time.
    pub fn mtime(mut self, secs: i64) -> Self {
        self.mtime = Some(secs);
        self
    }

    /// Request fragmented placement.
    pub fn fragmented(mut self, first_clusters: u32, gap_clusters: u32) -> Self {
        self.fragment = Some(Fragment {
            first_clusters,
            gap_clusters,
        });
        self
    }
}

/// Description of the filesystem to build. Construct with [`FsSpec::new`] or
/// `FsSpec { kind, size, files, ..Default::default() }`; the struct may gain
/// optional fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsSpec {
    /// Filesystem type.
    pub kind: FsType,
    /// Image size in bytes (multiple of 512).
    pub size: u64,
    /// Cluster size in bytes (ext4: block size); `None` = a sensible default.
    pub cluster_size: Option<u32>,
    /// Volume label.
    pub label: Option<String>,
    /// Files to write, in order (allocation follows this order).
    pub files: Vec<FileSpec>,
    /// Paths (files or directories) to delete after everything is written.
    pub deleted: Vec<String>,
    /// Directories to create even if no file lives in them (created before
    /// any file).
    pub dirs: Vec<String>,
    /// FAT32 only: delete the way Windows does, zeroing the high word of the
    /// first cluster in the directory entry (Linux leaves it).
    pub windows_delete: bool,
    /// FAT only: always use the built-in formatter instead of `mkfs.vfat`.
    pub native_format: bool,
    /// NTFS only: never mount with ntfs-3g, which forces the flat `ntfscp`
    /// fallback (used to test that path on machines that can mount).
    pub no_mount: bool,
}

impl Default for FsSpec {
    /// An empty 64 MiB FAT32 spec.
    fn default() -> Self {
        FsSpec::new(FsType::Fat32, 64 << 20)
    }
}

impl FsSpec {
    /// An empty spec of the given type and size.
    pub fn new(kind: FsType, size: u64) -> Self {
        FsSpec {
            kind,
            size,
            cluster_size: None,
            label: None,
            files: Vec::new(),
            deleted: Vec::new(),
            dirs: Vec::new(),
            windows_delete: false,
            native_format: false,
            no_mount: false,
        }
    }

    /// Add a file.
    pub fn file(mut self, f: FileSpec) -> Self {
        self.files.push(f);
        self
    }

    /// Mark a path for deletion.
    pub fn delete(mut self, path: impl Into<String>) -> Self {
        self.deleted.push(path.into());
        self
    }

    /// Set the cluster size.
    pub fn cluster_size(mut self, bytes: u32) -> Self {
        self.cluster_size = Some(bytes);
        self
    }

    /// Set the label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Where a file ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedFile {
    /// Normalised path (no leading slash).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Volume-relative `(offset, length)` byte ranges holding the data, in
    /// file order. The lengths sum to `size` (the last range stops at the end
    /// of the data, not at the cluster boundary). Empty when unknown or when
    /// the file has no clusters.
    pub extents: Vec<(u64, u64)>,
    /// True when the file was deleted after writing (its data is still at
    /// `extents` unless the filesystem reused it, which fixtures never do).
    pub deleted: bool,
    /// Modification time written to the volume (Unix seconds, UTC).
    pub mtime: i64,
}

/// What [`build`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildReport {
    /// Tools used, e.g. `mkfs.vfat+builtin`.
    pub tool: String,
    /// Human-readable remarks (fallbacks taken, features unavailable).
    pub notes: Vec<String>,
    /// Whether deletions were really performed in the image.
    pub deleted_supported: bool,
    /// All files that were written (deleted ones included).
    pub placed: Vec<PlacedFile>,
    /// Cluster (block) size of the finished volume in bytes.
    pub cluster_size: u32,
    /// Byte offset of the first cluster / data area, volume-relative.
    pub data_start: u64,
}

impl BuildReport {
    /// Look up a placed file by path (leading slash optional).
    pub fn find(&self, path: &str) -> Option<&PlacedFile> {
        let p = path.trim_start_matches('/');
        self.placed.iter().find(|f| f.path == p)
    }
}

/// Build the image described by `spec` at `out` (created or truncated).
pub fn build(spec: &FsSpec, out: &Path) -> Result<BuildReport> {
    if spec.size == 0 || spec.size % 512 != 0 {
        return Err(FixtureError::invalid(
            "image size must be a non-zero multiple of 512",
        ));
    }
    if spec.size > MAX_IMAGE_SIZE {
        return Err(FixtureError::invalid(format!(
            "image size {} exceeds the fixture limit of {MAX_IMAGE_SIZE} bytes",
            spec.size
        )));
    }
    match spec.kind {
        FsType::Fat12 | FsType::Fat16 | FsType::Fat32 => fat::build(spec, out),
        FsType::ExFat => exfat::build(spec, out),
        FsType::Ntfs => ntfs::build(spec, out),
        FsType::Ext4 => ext4::build(spec, out),
    }
}

/// Largest image the builders accept (2 TiB; images are sparse, but the
/// formatting tools still have to describe every cluster).
pub const MAX_IMAGE_SIZE: u64 = 2 << 40;

/// The FAT and exFAT builders keep the whole allocation table in memory;
/// volumes with more clusters than this are refused (use a larger cluster
/// size).
pub(crate) const MAX_CLUSTERS: u64 = 1 << 24;

/// Create (or truncate) `out` and make it a sparse file of `size` bytes.
pub(crate) fn create_sparse(out: &Path, size: u64) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)?;
    f.set_len(size)?;
    Ok(f)
}
