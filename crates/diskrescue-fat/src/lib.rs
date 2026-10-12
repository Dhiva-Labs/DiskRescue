//! FAT12/16/32 and exFAT parsers with deleted-file recovery.
//!
//! Nearly every camera card and pen drive is FAT32 or exFAT, so this crate
//! is where most names of lost photos and videos come back. Both parsers
//! implement [`diskrescue_core::fs::FileSystem`]:
//!
//! * [`FatFs`] ([`open_fat`]): FAT12/16/32 with long names, deleted entries
//!   (name from the deleted LFN run when it verifies, else the 8.3 name with
//!   its lost first character shown as `_`; data assumed contiguous from the
//!   first cluster), deleted directories, the FAT32 backup boot sector,
//!   per-sector FAT1 -> FAT2 fallback, and reconstruction of a destroyed
//!   boot sector from the FAT copies and directories (FAT12/16 have no
//!   backup boot sector).
//! * [`ExFatFs`] ([`open_exfat`]): exFAT directory entry sets with checksum
//!   verification, NoFatChain and FAT-chained files, ValidDataLength,
//!   deleted sets (exact extents for NoFatChain files), the allocation
//!   bitmap and the backup boot region with its checksum sector.
//!
//! Both scan for orphan directories (directory clusters in free space, or
//! allocated but unreachable) and list their contents under
//! `$orphans/cluster_<n>/`.
//!
//! Everything read from the medium is bounds-checked; directory recursion,
//! chain lengths, directory sizes and the number of entries are capped, so
//! hostile or random input yields errors or partial listings, never panics
//! or unbounded work. Unreadable sectors are zero-filled and reported, both
//! in metadata (logged, listing continues) and in file data
//! ([`diskrescue_core::fs::FileReadReport::unreadable`]).
//!
//! Entry ids are the volume byte offsets of the directory entry (the short
//! entry on FAT, the File entry on exFAT); synthetic `$orphans` entries use
//! ids with bit 62 or 63 set. Top-level entries have `parent_id == None`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod common;
pub mod exfat;
pub mod fat;
mod io;
mod table;
mod util;

pub use diskrescue_core as core;
pub use exfat::{open_exfat, ExFatFs};
pub use fat::{open_fat, FatFs};

use diskrescue_core::fs::{FileSystem, FsKind};
use diskrescue_core::source::SourceExt;
use diskrescue_core::{Error, Result, SharedSource};

/// Options for opening FAT and exFAT volumes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FatOptions {
    /// List deleted entries (and the contents of deleted and orphan
    /// directories). Default true.
    pub include_deleted: bool,
    /// Scan unallocated (and unreachable allocated) clusters for directory
    /// clusters and list them under `$orphans/cluster_<n>/`. Only effective
    /// with `include_deleted`. Default true.
    pub scan_orphans: bool,
    /// Fall back to the backup boot sector (FAT32 sector 6, exFAT sector 12)
    /// when the primary is unreadable or invalid, and when no backup is
    /// usable (FAT12/16 have none) reconstruct the FAT geometry from the
    /// two FAT copies and the directory structure. Default true.
    pub use_backup_boot_sector: bool,
    /// Maximum number of entries a walk reports (guard against hostile
    /// volumes). Default 10 million.
    pub max_entries: usize,
    /// exFAT: when a file's ValidDataLength is below its DataLength, read
    /// the bytes in between from the medium instead of writing zeros. The
    /// filesystem calls them unwritten, but after a camcorder lost power
    /// mid-recording they hold the end of the video (the directory entry
    /// was not updated); otherwise they hold stale data. The read report
    /// notes the range either way. Default false (zeros, reported as
    /// holes).
    #[serde(default)]
    pub read_past_valid_data_length: bool,
}

impl Default for FatOptions {
    fn default() -> Self {
        FatOptions {
            include_deleted: true,
            scan_orphans: true,
            use_backup_boot_sector: true,
            max_entries: 10_000_000,
            read_past_valid_data_length: false,
        }
    }
}

/// Identify a FAT or exFAT volume from its first bytes (`head` should be
/// the first 4096 bytes). FAT width follows the Microsoft cluster-count rule
/// (a FAT32-layout BPB is always FAT32); exFAT is recognised by the
/// `EXFAT   ` name with the zeroed legacy BPB. When sector 0 is not a valid
/// FAT boot sector, the FAT32 backup boot sector at sector 6 (512-byte
/// sectors) is tried.
pub fn probe(head: &[u8]) -> Option<FsKind> {
    if head.len() < 512 {
        return None;
    }
    if exfat::probe_sector(head) {
        return Some(FsKind::ExFat);
    }
    if let Some(k) = fat::probe_sector(head) {
        return Some(k);
    }
    if head.len() >= 7 * 512 {
        if let Some(FsKind::Fat32) = fat::probe_sector(&head[6 * 512..7 * 512]) {
            return Some(FsKind::Fat32);
        }
    }
    None
}

/// Open a FAT or exFAT volume, detecting which. Tries the primary boot
/// sector first and falls back to each parser's backup handling; the
/// heuristic FAT boot sector reconstruction comes last, after exFAT's
/// backup boot region had its chance.
pub fn open(src: SharedSource, opts: &FatOptions) -> Result<Box<dyn FileSystem>> {
    let head = src.read_vec(0, 4096).unwrap_or_default();
    let exfat_first = head.len() >= 11 && &head[3..11] == b"EXFAT   ";
    if exfat_first {
        match open_exfat(src.clone(), opts) {
            Ok(fs) => return Ok(Box::new(fs)),
            Err(e) => log::debug!("exFAT open failed: {e}"),
        }
    }
    let fat_err = match fat::open_fat_impl(src.clone(), opts, false) {
        Ok(fs) => return Ok(Box::new(fs)),
        Err(e) => e,
    };
    if !exfat_first {
        if let Ok(fs) = open_exfat(src.clone(), opts) {
            return Ok(Box::new(fs));
        }
    }
    if opts.use_backup_boot_sector {
        if let Ok(fs) = fat::open_fat_impl(src.clone(), opts, true) {
            return Ok(Box::new(fs));
        }
    }
    Err(match fat_err {
        Error::Corrupt { detail, .. } => Error::corrupt(
            "FAT/exFAT boot sector",
            format!("no valid FAT or exFAT boot sector ({detail})"),
        ),
        other => other,
    })
}
