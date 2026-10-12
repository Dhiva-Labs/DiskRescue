//! NTFS reader with deleted-file recovery, built to survive damaged volumes.
//!
//! [`open`] parses a volume from a [`SharedSource`] spanning exactly one
//! NTFS partition and returns an [`NtfsFs`] implementing
//! [`diskrescue_core::fs::FileSystem`]: it lists live and deleted files with
//! their paths, streams file content (resident, non-resident, sparse and
//! LZNT1-compressed data) with an honest report of holes, unreadable bytes
//! and reused clusters, and exposes the free-cluster map for the carver.
//!
//! # How it reads a volume
//!
//! * **Boot sector** ([`boot`]): sector 0, or the backup in the volume's
//!   last sector when sector 0 is damaged. Negative-encoded sectors per
//!   cluster (clusters up to 2 MiB) and record sizes are decoded.
//! * **MFT** ([`volume`]): record 0 (or its `$MFTMirr` copy) maps the MFT,
//!   which is read through its run list because it is usually fragmented.
//!   Every record has its update sequence array applied ([`fixup`]) before
//!   parsing; torn records are parsed with a warning, `BAAD` and
//!   unreadable records are skipped with a warning and never abort a walk.
//! * **Attributes** ([`attr`], [`runs`]): resident and non-resident
//!   attributes, data runs with sparse runs, `$ATTRIBUTE_LIST` folding of
//!   extension records (cycle protected), named streams, reparse points and
//!   EFS detection.
//! * **Tree** ([`walk`]): by default every MFT record is enumerated and
//!   placed through its `$FILE_NAME` parent references (hard links,
//!   orphans under `$orphans/`, deleted directories keep their children);
//!   directory indexes ([`index`]) contribute names of files whose records
//!   were reused, including entries recovered from index slack.
//! * **Deleted files**: a record with the in-use flag clear keeps its run
//!   list; its clusters are checked against `$Bitmap` ([`bitmap`]) to
//!   decide between [`EntryConfidence::DeletedIntact`] and
//!   [`EntryConfidence::DeletedGuessed`]. When deletion truncated the
//!   attribute list, extension records that still point back at the base
//!   record supply the missing pieces (flag `attribute-pieces-guessed`).
//!
//! # Entries
//!
//! * Paths use `/`, no leading slash; the root (record 5) is not reported
//!   and its children carry `parent_id = Some(5)`.
//! * Entries whose parent is missing, reused (sequence mismatch), not a
//!   directory, or part of a cycle appear as `$orphans/<record>/<name>`
//!   under synthetic `virtual` directories, with flags naming the reason.
//!   Children of a deleted directory whose record is intact stay under it.
//! * Ids are `record | sub << 48` (see [`walk`]): sub 0 primary name,
//!   `1..0x4000` further hard links, `0x4000..0x8000` alternate data streams
//!   (`name:stream`, flag `ads`), `0x8000` synthetic entries.
//! * Name-only entries (flags `index-slack`, `stale-index-entry`,
//!   `mft-record-unreadable`) carry [`EntryConfidence::DeletedNoData`].
//!
//! # Limits
//!
//! * EFS-encrypted content is detected and refused.
//! * When both MFT record 0 and its mirror are lost, the MFT is assumed to
//!   be contiguous from the boot sector's MFT cluster; later fragments are
//!   not found.
//! * `$LogFile` and `$UsnJrnl` are not replayed.
//! * Name-only entries (from indexes) have no data; they exist so the carver
//!   can be told which names and sizes to look for.
//!
//! All parsers are bounds-checked and loop-bounded; hostile input yields
//! warnings, skipped items or errors, never panics.

#![warn(missing_docs)]

pub mod attr;
pub mod bitmap;
pub mod boot;
pub mod fixup;
pub mod index;
mod io;
pub mod lznt1;
mod read;
pub mod record;
pub mod runs;
mod stream;
pub mod volume;
pub mod walk;

#[cfg(test)]
mod synth_tests;

use std::io::Write;

pub use diskrescue_core as core;
use diskrescue_core::fs::{
    EntryConfidence, FileEntry, FileReadReport, FileSystem, VolumeInfo, WalkControl,
};
use diskrescue_core::{Range, RangeSet, Result, SharedSource};

pub use attr::{
    Attribute, AttributeBody, FileName, NonResidentInfo, ResidentValue, StandardInformation,
};
pub use bitmap::ClusterState;
pub use boot::BootSector;
pub use record::MftRecord;
pub use runs::Run;
pub use volume::NtfsFs;

use stream::AllocationMap;

/// Options for [`open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsOptions {
    /// Report deleted files and directories (default `true`).
    pub include_deleted: bool,
    /// Enumerate every MFT record instead of only walking the directory
    /// tree, so orphans and deleted records appear (default `true`).
    pub walk_whole_mft: bool,
    /// Read the backup boot sector (last sector of the volume) first
    /// (default `false`; the backup is always tried when sector 0 is bad).
    pub use_backup_boot_sector: bool,
    /// Decompress NTFS-compressed (LZNT1) files in `read_file` (default
    /// `true`; when `false` such files are refused).
    pub decompress: bool,
    /// Maximum number of MFT records examined.
    pub max_records: u64,
}

/// Default for [`NtfsOptions::max_records`]: 2^32 records (the Windows limit).
pub const DEFAULT_MAX_RECORDS: u64 = 1 << 32;

impl Default for NtfsOptions {
    fn default() -> Self {
        NtfsOptions {
            include_deleted: true,
            walk_whole_mft: true,
            use_backup_boot_sector: false,
            decompress: true,
            max_records: DEFAULT_MAX_RECORDS,
        }
    }
}

/// True when `head` (at least the first 512 bytes of a volume) starts with
/// a valid NTFS boot sector.
pub fn probe(head: &[u8]) -> bool {
    boot::probe(head)
}

/// Open the NTFS volume spanning `src`.
pub fn open(src: SharedSource, opts: &NtfsOptions) -> Result<NtfsFs> {
    NtfsFs::open_impl(src, opts)
}

impl FileSystem for NtfsFs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn walk(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        self.walk_impl(visit)
    }

    fn read_file(&self, entry: &FileEntry, sink: &mut dyn Write) -> Result<FileReadReport> {
        self.read_file_impl(entry, sink)
    }

    fn unallocated(&self) -> Result<RangeSet> {
        let total = self.info.total_size;
        let window = Range::new(0, total);
        let Some(bm) = &self.bitmap else {
            log::warn!("NTFS: no $Bitmap; reporting the whole volume as unallocated");
            return Ok(RangeSet::from_range(window));
        };
        let mut out = RangeSet::new();
        let cluster = self.cluster;
        let mut unknown = 0u64;
        bm.for_each_state(&self.disk, 0, self.total_clusters, &mut |s, e, st| {
            if st != ClusterState::Allocated {
                if st == ClusterState::Unknown {
                    unknown += e - s;
                }
                out.insert(Range::new(
                    s.saturating_mul(cluster),
                    e.saturating_mul(cluster),
                ));
            }
        })?;
        if unknown > 0 {
            log::warn!(
                "NTFS: allocation state of {unknown} cluster(s) unknown; reported as unallocated"
            );
        }
        Ok(out.intersection_with_range(window))
    }

    fn source(&self) -> &SharedSource {
        &self.src
    }
}

/// True when an entry's data can be read at all (not a directory and not a
/// name-only entry).
pub fn has_data(entry: &FileEntry) -> bool {
    !entry.is_dir && entry.confidence != EntryConfidence::DeletedNoData
}
