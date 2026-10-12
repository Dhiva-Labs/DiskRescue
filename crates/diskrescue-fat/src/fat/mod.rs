//! FAT12 / FAT16 / FAT32.
//!
//! Opening: the boot sector at sector 0 is validated (spec 2.2.3); when it
//! is unreadable or invalid and the options allow it, the FAT32 backup boot
//! sector at sector 6 is tried for every legal sector size and a warning is
//! recorded. The geometry follows the Microsoft rules; a FAT too small for
//! the claimed cluster count is clipped with a warning. The FAT is read
//! lazily with FAT1 -> FAT2 fallback per unreadable sector.
//!
//! Listing ([`FileSystem::walk`]): depth-first from the root (the fixed
//! region on FAT12/16, a cluster chain on FAT32). Live files get their
//! extents from the FAT chain (cycle-protected, cut at the file size).
//! Deleted files (first byte 0xE5) keep size, timestamps and the first
//! cluster; their chain is gone, so their data is assumed to be the
//! contiguous clusters from the first cluster on (`DeletedGuessed`). On
//! FAT32 volumes with more than 65535 clusters, Windows zeroes the high
//! word of the first cluster on delete; candidates `h << 16 | low` are
//! then checked for a media signature (files) or a self-pointing `.` entry
//! (directories). Deleted directories are listed from their first cluster
//! and following free clusters while they still look like directories.
//!
//! Orphans: after the tree walk, free clusters (and allocated clusters no
//! live file or directory claims, when the walk was complete) are sniffed
//! with the directory heuristic. Hits whose parent is not another hit are
//! listed under `$orphans/cluster_<n>/`.
//!
//! When no valid boot sector is left (FAT12/16 have no backup; FAT32 may
//! have lost sector 6 too), the geometry is reconstructed from the two FAT
//! copies and the directory structure (see the `rebuild` module, filesystems
//! spec 2.2.10) and the volume warnings say so.
//!
//! Limits: short names are decoded as CP437 only.

mod boot;
mod dir;
mod rebuild;
mod walk;

pub use boot::{BootSector, FsInfo};

use std::io::Write;
use std::sync::{Arc, OnceLock};

use diskrescue_core::fs::{FileEntry, FileReadReport, FileSystem, FsKind, VolumeInfo, WalkControl};
use diskrescue_core::{Error, Range, RangeSet, Result, SharedSource};

use crate::common::{self, ClaimIndex, ClusterMap, ClusterState};
use crate::io::MetaReader;
use crate::table::{FatTable, FatWidth};
use crate::FatOptions;

pub(crate) use boot::probe_sector;

/// A parsed FAT12/16/32 volume.
pub struct FatFs {
    src: SharedSource,
    meta: Arc<MetaReader>,
    boot: BootSector,
    fs_info: Option<FsInfo>,
    geo: boot::Geometry,
    table: FatTable,
    info: VolumeInfo,
    opts: FatOptions,
    /// Clusters claimed by deleted entries (first walk pass, computed once).
    claims: OnceLock<ClaimIndex>,
}

impl std::fmt::Debug for FatFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FatFs")
            .field("source", &self.src.describe())
            .field("info", &self.info)
            .finish()
    }
}

/// Open a FAT12/16/32 volume. See the module docs.
pub fn open_fat(src: SharedSource, opts: &FatOptions) -> Result<FatFs> {
    open_fat_impl(src, opts, true)
}

/// [`open_fat`], optionally without the heuristic boot sector
/// reconstruction (so [`crate::open`] can try exFAT's backup boot region
/// first).
pub(crate) fn open_fat_impl(src: SharedSource, opts: &FatOptions, rebuild: bool) -> Result<FatFs> {
    let meta = Arc::new(MetaReader::new(src.clone()));
    let size = src.size();
    if size < 512 {
        return Err(Error::corrupt(
            "FAT boot sector",
            format!("volume is only {size} bytes"),
        ));
    }
    let mut warnings = Vec::new();
    let (sec0, bad0) = meta.read_vec(0, 512);
    let primary = if bad0.is_empty() {
        BootSector::parse(&sec0)
    } else {
        None
    };
    let reason = match &primary {
        None if !bad0.is_empty() => "boot sector unreadable".to_string(),
        None => "boot sector too short".to_string(),
        Some(b) => match b.geometry() {
            Ok(_) => String::new(),
            Err(r) => r,
        },
    };
    let (boot, geo) = if reason.is_empty() {
        let b = primary.expect("checked above");
        let g = b
            .geometry()
            .map_err(|r| Error::corrupt("FAT boot sector", r))?;
        (b, g)
    } else {
        if !opts.use_backup_boot_sector {
            return Err(Error::corrupt("FAT boot sector", reason));
        }
        let mut found = None;
        for bps in [512u64, 1024, 2048, 4096] {
            let (sec, bad) = meta.read_vec(6 * bps, 512);
            if !bad.is_empty() {
                continue;
            }
            let Some(b) = BootSector::parse(&sec) else {
                continue;
            };
            if b.bpb_byts_per_sec as u64 != bps || !b.is_fat32_layout() {
                continue;
            }
            if let Ok(g) = b.geometry() {
                if g.kind == FsKind::Fat32 {
                    found = Some((b, g));
                    break;
                }
            }
        }
        match found {
            Some(x) => {
                log::warn!(
                    "primary FAT boot sector invalid ({reason}); using the backup at sector 6"
                );
                warnings.push(format!(
                    "primary boot sector invalid ({reason}); using the backup boot sector at sector 6"
                ));
                x
            }
            None => {
                let rebuilt = if rebuild {
                    rebuild::reconstruct(&meta, size)
                } else {
                    None
                };
                match rebuilt {
                    Some((b, g, notes)) => {
                        log::warn!(
                            "FAT boot sector invalid ({reason}) and no backup; geometry reconstructed"
                        );
                        warnings.push(format!(
                            "boot sector invalid ({reason}) and no usable backup boot sector"
                        ));
                        warnings.extend(notes);
                        (b, g)
                    }
                    None => return Err(Error::corrupt("FAT boot sector", reason)),
                }
            }
        }
    };
    warnings.extend(geo.warnings.iter().cloned());

    // FSInfo (FAT32): primary, then the copy after the backup boot sector.
    let mut fs_info = None;
    if geo.kind == FsKind::Fat32 {
        let bps = geo.bytes_per_sector;
        let mut sectors = vec![boot.bpb_fs_info as u64];
        if boot.bpb_bk_boot_sec != 0 {
            sectors.push(boot.bpb_bk_boot_sec as u64 + 1);
        }
        for s in sectors {
            if s == 0 || s >= boot.bpb_rsvd_sec_cnt as u64 {
                continue;
            }
            let (b, bad) = meta.read_vec(s * bps, 512);
            if bad.is_empty() {
                if let Some(fi) = FsInfo::parse(&b) {
                    fs_info = Some(fi);
                    break;
                }
            }
        }
    }

    let width = match geo.kind {
        FsKind::Fat12 => FatWidth::Fat12,
        FsKind::Fat16 => FatWidth::Fat16,
        _ => FatWidth::Fat32,
    };
    let table = FatTable::new(
        meta.clone(),
        width,
        geo.fats.clone(),
        geo.fat_bytes,
        geo.max_cluster,
    );

    // FAT[1] carries the clean-shutdown and hard-error bits.
    if let Some(v1) = table.raw(1) {
        let (clean, noerr) = match geo.kind {
            FsKind::Fat16 => (0x8000, 0x4000),
            FsKind::Fat32 => (0x0800_0000, 0x0400_0000),
            _ => (0, 0),
        };
        if clean != 0 && v1 & clean == 0 {
            warnings.push(
                "volume was not cleanly unmounted (FAT dirty flag set); expect torn metadata"
                    .to_string(),
            );
        }
        if noerr != 0 && v1 & noerr == 0 {
            warnings.push("the driver recorded disk I/O errors on this volume".to_string());
        }
    }
    if let Some(v0) = table.raw(0) {
        if v0 & 0xFF != boot.bpb_media as u32 {
            warnings.push(format!(
                "FAT[0] media byte {:#04x} differs from the boot sector's {:#04x}",
                v0 & 0xFF,
                boot.bpb_media
            ));
        }
    }
    if size < geo.total_size {
        warnings.push(format!(
            "image is {} bytes shorter than the volume; data near the end is missing",
            geo.total_size - size
        ));
    }

    let info = VolumeInfo {
        kind: geo.kind,
        label: boot.label(),
        cluster_size: geo.cluster_size as u32,
        total_size: geo.total_size,
        data_start: geo.data_start,
        serial: boot.serial(),
        warnings,
    };
    let mut fs = FatFs {
        src,
        meta,
        boot,
        fs_info,
        geo,
        table,
        info,
        opts: opts.clone(),
        claims: OnceLock::new(),
    };
    // The label entry in the root directory wins over the boot sector copy.
    if let Some(l) = walk::root_label(&fs) {
        fs.info.label = Some(l);
    }
    for w in &fs.info.warnings {
        log::warn!("{}: {w}", fs.src.describe());
    }
    Ok(fs)
}

impl FatFs {
    /// Volume facts (same as [`FileSystem::info`]).
    pub fn volume_info(&self) -> &VolumeInfo {
        &self.info
    }

    /// The parsed boot sector in use (the backup when the primary was bad).
    pub fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// The FAT32 FSInfo sector, when present and valid.
    pub fn fs_info(&self) -> Option<&FsInfo> {
        self.fs_info.as_ref()
    }

    /// FAT width.
    pub fn kind(&self) -> FsKind {
        self.geo.kind
    }

    /// Number of data clusters (CountOfClusters, after clipping).
    pub fn cluster_count(&self) -> u32 {
        self.geo.count_of_clusters
    }

    /// Volume byte offset of cluster `cluster` (valid clusters start at 2).
    pub fn cluster_to_offset(&self, cluster: u32) -> u64 {
        self.offset_of(cluster)
    }

    /// Volume-relative extents of the chain starting at `first_cluster`,
    /// following the FAT up to the end-of-chain marker (cycle-protected,
    /// at most the cluster count).
    pub fn chain(&self, first_cluster: u32) -> Vec<Range> {
        let ch = self
            .table
            .follow(first_cluster, self.geo.count_of_clusters as u64, false);
        common::runs_to_extents(self, &ch.runs, None)
    }

    /// Raw FAT entry value of `cluster` (`None` when unreadable in every copy).
    pub fn fat_entry(&self, cluster: u32) -> Option<u32> {
        self.table.raw(cluster)
    }

    /// Number of free clusters according to the FAT.
    pub fn free_clusters(&self) -> u64 {
        let mut n = 0u64;
        common::for_each_state(self, |_, s| {
            if s == ClusterState::Free {
                n += 1;
            }
            true
        });
        n
    }

    pub(crate) fn is_fat32(&self) -> bool {
        self.geo.kind == FsKind::Fat32
    }

    pub(crate) fn opts(&self) -> &FatOptions {
        &self.opts
    }

    pub(crate) fn meta(&self) -> &MetaReader {
        &self.meta
    }

    pub(crate) fn table(&self) -> &FatTable {
        &self.table
    }

    pub(crate) fn geometry(&self) -> &boot::Geometry {
        &self.geo
    }

    /// Deleted-entry cluster claims, collected by a silent first walk.
    pub(crate) fn claims(&self) -> &ClaimIndex {
        self.claims.get_or_init(|| walk::collect_claims(self))
    }
}

impl ClusterMap for FatFs {
    fn cluster_bytes(&self) -> u64 {
        self.geo.cluster_size
    }

    fn heap_offset(&self) -> u64 {
        self.geo.data_start
    }

    fn max_cluster(&self) -> u32 {
        self.geo.max_cluster
    }

    fn meta_limit(&self) -> u32 {
        let src = self.meta.size();
        let bits: u64 = match self.geo.kind {
            FsKind::Fat12 => 12,
            FsKind::Fat16 => 16,
            _ => 32,
        };
        let mut best = 0u64;
        for &base in &self.geo.fats {
            if src > base {
                let bytes = (src - base).min(self.geo.fat_bytes);
                best = best.max(bytes * 8 / bits);
            }
        }
        best.saturating_sub(1).min(u32::MAX as u64) as u32
    }

    fn source_len(&self) -> u64 {
        self.meta.size()
    }

    fn states(&self, first: u32, count: u32) -> Vec<ClusterState> {
        self.table
            .raw_batch(first, count)
            .into_iter()
            .map(|v| match v {
                None => ClusterState::Unknown,
                Some(0) => ClusterState::Free,
                Some(v) => match self.table.classify(v) {
                    crate::table::FatEntry::Bad => ClusterState::Bad,
                    _ => ClusterState::Allocated,
                },
            })
            .collect()
    }
}

impl FileSystem for FatFs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn walk(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        walk::run(self, visit)
    }

    fn read_file(&self, entry: &FileEntry, sink: &mut dyn Write) -> Result<FileReadReport> {
        common::read_entry(&self.src, self, entry, sink, false)
    }

    fn unallocated(&self) -> Result<RangeSet> {
        Ok(common::unallocated_ranges(self, self.src.size()))
    }

    fn source(&self) -> &SharedSource {
        &self.src
    }
}
