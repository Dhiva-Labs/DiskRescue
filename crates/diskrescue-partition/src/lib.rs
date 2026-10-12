//! Partition tables, filesystem probing and lost-partition scanning.
//!
//! This crate answers three questions about a disk image or device, all
//! through a read-only [`diskrescue_core::Source`]:
//!
//! 1. **What is the partition layout?** [`read_partition_table`] parses MBR
//!    (with EBR chains), GPT (with CRC validation and backup-header fallback),
//!    hybrid MBRs, 4096-byte-sector media and "superfloppy" volumes that have
//!    no table at all.
//! 2. **What is in a partition?** [`probe_filesystem`] validates FAT12/16/32,
//!    exFAT, NTFS and ext2/3/4 boot sectors and superblocks (using the backup
//!    copies when the primary is damaged) and recognises btrfs, XFS, HFS+,
//!    APFS, ISO 9660, LUKS, swap and squashfs.
//! 3. **Where did the partitions go?** [`scan_lost_partitions`] sweeps the
//!    device for filesystem signatures when the table is destroyed, and
//!    [`volumes`] combines table, probes and a quick scan into ready-to-use
//!    per-partition sources.
//!
//! Nothing here parses file or directory structures; that is the job of the
//! filesystem crates. Everything is bounds-checked and safe on hostile input.

#![warn(missing_docs)]

pub use diskrescue_core as core;

pub mod gpt;
mod io;
pub mod mbr;
pub mod probe;
pub mod scan;
pub mod table;
pub mod volume;

pub use probe::{probe_filesystem, FsProbe};
pub use scan::{scan_lost_partitions, ScanOptions};
pub use table::{
    read_partition_table, read_partition_table_detailed, read_partition_table_with, TableDetection,
};
pub use volume::{volumes, Volume};
