//! Lost-partition scan over images whose partition table was destroyed.

mod common;

use common::*;
use diskrescue_core::fs::FsKind;
use diskrescue_core::partition::PartitionEntry;
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, ReadError, ReadErrorKind, SharedSource, Source};
use diskrescue_partition::{scan_lost_partitions, ScanOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Vol {
    start: u64,
    size: u64,
    kind: FsKind,
    mk: Mk,
}

/// FAT32 (40 MiB) @1 MiB, NTFS @48 MiB, exFAT @70 MiB, ext4 (1 KiB blocks) @100 MiB
/// on a 140 MiB disk with an all-zero first sector (no partition table).
fn four_volumes(dir: &std::path::Path) -> Option<(Disk, Vec<Vol>)> {
    let layout = [
        (Mk::Fat32, MIB, FsKind::Fat32),
        (Mk::Ntfs, 48 * MIB, FsKind::Ntfs),
        (Mk::ExFat, 70 * MIB, FsKind::ExFat),
        (Mk::Ext4Block1k, 100 * MIB, FsKind::Ext4),
    ];
    let disk = Disk::create(dir, "lost.img", 140 * MIB);
    let mut vols = Vec::new();
    for (mk, start, kind) in layout {
        let img = mkfs(dir, mk)?;
        disk.embed(start, &img);
        vols.push(Vol {
            start,
            size: mk.size(),
            kind,
            mk,
        });
    }
    Some((disk, vols))
}

fn scan(src: &SharedSource, step: u64) -> Vec<PartitionEntry> {
    let opts = ScanOptions {
        step,
        ..ScanOptions::default()
    };
    scan_lost_partitions(src, &opts, &mut |_, _| {}).unwrap()
}

fn assert_found(found: &[PartitionEntry], vols: &[Vol]) {
    assert_eq!(found.len(), vols.len(), "{found:#?}");
    for (i, (f, v)) in found.iter().zip(vols).enumerate() {
        assert_eq!(f.start, v.start, "{:?} start", v.mk);
        assert_eq!(f.size, v.size, "{:?} size", v.mk);
        assert_eq!(f.fs, Some(v.kind), "{:?} fs", v.mk);
        assert_eq!(f.origin, "scan");
        assert_eq!(f.index, i as u32 + 1);
    }
}

#[test]
fn finds_every_volume_once_with_default_and_thorough_steps() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    let src = disk.source();
    for step in [4096u64, 512] {
        let found = scan(&src, step);
        assert_found(&found, &vols);
    }
}

#[test]
fn works_alongside_an_intact_table_and_ignores_it() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    disk.write_at(
        0,
        &mbr_sector(&[MbrPart {
            status: 0x80,
            ptype: 0x0C,
            lba: 2048,
            sectors: 1000,
        }]),
    );
    assert_found(&scan(&disk.source(), 4096), &vols);
}

#[test]
fn destroyed_boot_sectors_are_resolved_through_backups_thorough() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    // Kill the primary structure of every volume.
    disk.zero(vols[0].start, 512); // FAT32 sector 0
    disk.zero(vols[1].start, 512); // NTFS sector 0
    disk.zero(vols[2].start, 512); // exFAT sector 0
    disk.zero(vols[3].start + 1024, 1024); // ext superblock
    let found = scan(&disk.source(), 512);
    assert_found(&found, &vols);
}

#[test]
fn destroyed_boot_sectors_default_step_finds_aligned_backups() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    disk.zero(vols[0].start, 512);
    disk.zero(vols[2].start, 512);
    let found = scan(&disk.source(), 4096);
    // FAT32 (sector 6) and exFAT (sector 12) copies are probed relative to
    // each aligned offset, so these two come back with their true starts.
    let fat = found.iter().find(|e| e.fs == Some(FsKind::Fat32)).unwrap();
    assert_eq!((fat.start, fat.size), (vols[0].start, vols[0].size));
    let ex = found.iter().find(|e| e.fs == Some(FsKind::ExFat)).unwrap();
    assert_eq!((ex.start, ex.size), (vols[2].start, vols[2].size));
    assert!(found.iter().any(|e| e.fs == Some(FsKind::Ntfs)));
    assert!(found.iter().any(|e| e.fs == Some(FsKind::Ext4)));
    assert_eq!(found.len(), 4, "{found:#?}");
}

#[test]
fn ext4_with_4k_blocks_found_through_the_group_1_backup() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ext4Block4k) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 200 * MIB);
    disk.embed(MIB, &img);
    disk.zero(MIB + 1024, 1024);
    let found = scan(&disk.source(), 4096);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].start, MIB);
    assert_eq!(found[0].size, Mk::Ext4Block4k.size());
    assert_eq!(found[0].fs, Some(FsKind::Ext4));
}

#[test]
fn fat32_with_4096_byte_sectors_and_damaged_boot_sector() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Fat32Sector4096) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 80 * MIB);
    disk.embed(4 * MIB, &img);
    disk.zero(4 * MIB, 4096);
    let found = scan(&disk.source(), 4096);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].start, 4 * MIB);
    assert_eq!(found[0].size, Mk::Fat32Sector4096.size());
}

#[test]
fn legacy_sector_63_partition_needs_the_fine_step() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Fat16) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 30 * MIB);
    disk.embed(63 * 512, &img);
    let found = scan(&disk.source(), 512);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].start, 63 * 512);
    assert_eq!(found[0].fs, Some(FsKind::Fat16));
}

#[test]
fn nested_filesystem_images_inside_a_volume_are_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(outer), Some(inner)) = (mkfs(dir.path(), Mk::Fat32), mkfs(dir.path(), Mk::ExFat))
    else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 60 * MIB);
    disk.embed(MIB, &outer);
    disk.embed(MIB + 8 * MIB, &inner); // a disk image stored as file data
    let found = scan(&disk.source(), 512);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].fs, Some(FsKind::Fat32));
}

#[test]
fn truncated_image_clips_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Fat32) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 30 * MIB);
    // Copy only the first 29 MiB of the image.
    let data = std::fs::read(&img).unwrap();
    disk.write_at(MIB, &data[..29 * MIB as usize]);
    let found = scan(&disk.source(), 4096);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].start, MIB);
    assert_eq!(found[0].size, 29 * MIB);
}

#[test]
fn options_start_end_max_results_and_step_rounding() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    let src = disk.source();
    // Window containing only the NTFS volume.
    let opts = ScanOptions {
        start: 47 * MIB,
        end: Some(66 * MIB),
        ..ScanOptions::default()
    };
    let f = scan_lost_partitions(&src, &opts, &mut |_, _| {}).unwrap();
    assert_found(&f, &vols[1..2]);
    // Starting inside the first volume skips it.
    let opts = ScanOptions {
        start: 2 * MIB,
        ..ScanOptions::default()
    };
    let f = scan_lost_partitions(&src, &opts, &mut |_, _| {}).unwrap();
    assert_found(&f, &vols[1..]);
    // max_results
    let opts = ScanOptions {
        max_results: 2,
        ..ScanOptions::default()
    };
    let f = scan_lost_partitions(&src, &opts, &mut |_, _| {}).unwrap();
    assert_found(&f, &vols[..2]);
    // Odd steps are rounded up to a multiple of 512 and must still find everything.
    for step in [0u64, 1, 700, 5000] {
        let opts = ScanOptions {
            step,
            ..ScanOptions::default()
        };
        let f = scan_lost_partitions(&src, &opts, &mut |_, _| {}).unwrap();
        // 5000 rounds to 5120: not a divisor of the MiB-aligned starts, so
        // only require that nothing is invented.
        assert!(f.len() <= 4);
        if step < 1024 {
            assert_found(&f, &vols);
        }
    }
    // Empty and inverted windows.
    let opts = ScanOptions {
        start: 10 * MIB,
        end: Some(10 * MIB),
        ..ScanOptions::default()
    };
    assert!(scan_lost_partitions(&src, &opts, &mut |_, _| {})
        .unwrap()
        .is_empty());
    let opts = ScanOptions {
        start: 100 * MIB,
        end: Some(10 * MIB),
        ..ScanOptions::default()
    };
    assert!(scan_lost_partitions(&src, &opts, &mut |_, _| {})
        .unwrap()
        .is_empty());
}

#[test]
fn progress_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let Some((disk, vols)) = four_volumes(dir.path()) else {
        return;
    };
    let src = disk.source();
    // Progress is monotonic and finishes at total.
    let mut calls: Vec<(u64, u64)> = Vec::new();
    let found = scan_lost_partitions(&src, &ScanOptions::default(), &mut |d, t| {
        calls.push((d, t))
    })
    .unwrap();
    assert_found(&found, &vols);
    assert!(calls.len() >= 3);
    assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(calls.iter().all(|&(d, t)| d <= t && t == 140 * MIB));
    assert_eq!(*calls.last().unwrap(), (140 * MIB, 140 * MIB));

    // Cancelled before the first chunk: nothing, no final progress.
    let flag = Arc::new(AtomicBool::new(true));
    let opts = ScanOptions {
        cancel: Some(flag),
        ..ScanOptions::default()
    };
    let mut n = 0;
    let f = scan_lost_partitions(&src, &opts, &mut |_, _| n += 1).unwrap();
    assert!(f.is_empty());
    assert_eq!(n, 1, "only the initial progress call");

    // Cancelled from the progress callback after the first chunk.
    let flag = Arc::new(AtomicBool::new(false));
    let opts = ScanOptions {
        cancel: Some(flag.clone()),
        ..ScanOptions::default()
    };
    let f = scan_lost_partitions(&src, &opts, &mut |d, _| {
        if d > 0 {
            flag.store(true, Ordering::Relaxed);
        }
    })
    .unwrap();
    // Only the volumes in the first 8 MiB chunk (none start there except FAT32 at 1 MiB).
    assert!(f.len() <= 1, "{f:#?}");
}

#[test]
fn blank_and_random_devices_yield_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let blank = Disk::create(dir.path(), "blank.img", 512 * MIB);
    let src = blank.source();
    let mut last = (0, 0);
    let f = scan_lost_partitions(&src, &ScanOptions::default(), &mut |d, t| last = (d, t)).unwrap();
    assert!(f.is_empty());
    assert_eq!(last, (512 * MIB, 512 * MIB));

    let mut rng = Rng(99);
    let noise = mem_source(rng.bytes(12 * MIB as usize));
    let f = scan(&noise, 512);
    assert!(f.is_empty(), "{f:#?}");
    let ones = mem_source(vec![0xFFu8; 4 * MIB as usize]);
    assert!(scan(&ones, 512).is_empty());
}

#[test]
fn read_errors_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(ex), Some(nt)) = (mkfs(dir.path(), Mk::ExFat), mkfs(dir.path(), Mk::Ntfs)) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 40 * MIB);
    disk.embed(MIB, &ex);
    disk.embed(20 * MIB, &nt);
    let mut holes = RangeSet::new();
    holes.insert(Range::new(MIB, MIB + 1024)); // exFAT sectors 0-1: main boot sector gone
    holes.insert(Range::new(10 * MIB, 10 * MIB + 8192)); // free space
    holes.insert(Range::new(24 * MIB, 24 * MIB + 65536)); // inside the NTFS data
    holes.insert(Range::new(38 * MIB, 39 * MIB)); // free space, 1 MiB of bad sectors
    let src: SharedSource = Arc::new(MemSource::new(disk.mem()).with_unreadable(holes));
    let found = scan(&src, 4096);
    assert_eq!(found.len(), 2, "{found:#?}");
    assert_eq!((found[0].start, found[0].fs), (MIB, Some(FsKind::ExFat)));
    assert_eq!(
        (found[1].start, found[1].fs),
        (20 * MIB, Some(FsKind::Ntfs))
    );
}

#[test]
fn fatal_source_errors_propagate() {
    struct Dying(MemSource);
    impl Source for Dying {
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn read_at(&self, o: u64, b: &mut [u8]) -> Result<usize, ReadError> {
            if o >= 4 * MIB {
                return Err(ReadError::new(
                    o,
                    b.len(),
                    ReadErrorKind::DeviceGone,
                    "unplugged",
                ));
            }
            self.0.read_at(o, b)
        }
        fn describe(&self) -> String {
            "dying".into()
        }
    }
    let src: SharedSource = Arc::new(Dying(MemSource::new(vec![0u8; 16 * MIB as usize])));
    let e = scan_lost_partitions(&src, &ScanOptions::default(), &mut |_, _| {}).unwrap_err();
    assert!(e.is_read_error(), "{e}");
}
