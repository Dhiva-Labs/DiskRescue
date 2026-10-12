//! `volumes()`: table + probes + per-partition sources + quick-scan fallback.

mod common;

use common::*;
use diskrescue_core::fs::FsKind;
use diskrescue_core::partition::PartitionScheme;
use diskrescue_core::source::FileSource;
use diskrescue_core::{SharedSource, Source, SourceExt};
use diskrescue_partition::volumes;
use std::sync::Arc;

fn sectors(bytes: u64) -> u32 {
    (bytes / 512) as u32
}

#[test]
fn mbr_disk_gives_one_source_per_partition() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f12), Some(e1k)) = (
        mkfs(dir.path(), Mk::Fat12),
        mkfs(dir.path(), Mk::Ext4Block1k),
    ) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 60 * MIB);
    disk.embed(MIB, &f12);
    disk.embed(10 * MIB, &e1k);
    disk.write_at(
        0,
        &mbr_sector(&[
            MbrPart {
                status: 0x80,
                ptype: 0x01,
                lba: sectors(MIB),
                sectors: sectors(Mk::Fat12.size()),
            },
            MbrPart {
                status: 0,
                ptype: 0x83,
                lba: sectors(10 * MIB),
                sectors: sectors(Mk::Ext4Block1k.size()),
            },
            // A partition with nothing in it (e.g. swap that was never formatted).
            MbrPart {
                status: 0,
                ptype: 0x82,
                lba: sectors(50 * MIB),
                sectors: sectors(4 * MIB),
            },
        ]),
    );
    let src = disk.source();
    let (table, vols) = volumes(&src, 512).unwrap();
    assert_eq!(table.scheme, PartitionScheme::Mbr);
    assert_eq!(vols.len(), 3);
    assert_eq!(vols[0].probe.as_ref().unwrap().kind, FsKind::Fat12);
    assert_eq!(vols[0].entry.fs, Some(FsKind::Fat12));
    assert_eq!(vols[1].entry.fs, Some(FsKind::Ext4));
    assert!(vols[2].probe.is_none());
    assert_eq!(vols[2].entry.fs, None);
    for v in &vols {
        assert_eq!(v.source.size(), v.entry.size);
        let want = disk.read_at(v.entry.start, 4096);
        let got = v.source.read_vec(0, 4096).unwrap();
        assert_eq!(got, want);
        assert!(v.source.describe().contains("partition"));
    }
    // The table returned and the volumes agree.
    assert_eq!(
        table.entries,
        vols.iter().map(|v| v.entry.clone()).collect::<Vec<_>>()
    );
    // The ext superblock magic is visible through the view.
    let sb = vols[1].source.read_exact_vec(1024 + 0x38, 2).unwrap();
    assert_eq!(sb, vec![0x53, 0xEF]);
}

#[test]
fn zeroed_table_is_recovered_by_the_quick_scan() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f12), Some(f16), Some(e2)) = (
        mkfs(dir.path(), Mk::Fat12),
        mkfs(dir.path(), Mk::Fat16),
        mkfs(dir.path(), Mk::Ext2),
    ) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 80 * MIB);
    disk.embed(MIB, &f12);
    disk.embed(8 * MIB, &f16);
    disk.embed(32 * MIB, &e2);
    let src = disk.source();
    let (table, vols) = volumes(&src, 512).unwrap();
    assert_eq!(table.scheme, PartitionScheme::Unknown);
    assert_eq!(vols.len(), 3, "{:#?}", table.entries);
    assert!(table.entries.iter().all(|e| e.origin == "scan"));
    assert_eq!(
        table.entries.iter().map(|e| e.start).collect::<Vec<_>>(),
        vec![MIB, 8 * MIB, 32 * MIB]
    );
    assert_eq!(
        vols.iter().map(|v| v.entry.fs).collect::<Vec<_>>(),
        vec![Some(FsKind::Fat12), Some(FsKind::Fat16), Some(FsKind::Ext2)]
    );
    assert!(table.warnings.iter().any(|w| w.contains("quick scan")));
    assert_eq!(vols[1].source.size(), Mk::Fat16.size());
}

#[test]
fn quick_scan_also_covers_the_end_of_a_large_device() {
    let dir = tempfile::tempdir().unwrap();
    let Some(nt) = mkfs(dir.path(), Mk::Ntfs) else {
        return;
    };
    let size = 400 * MIB;
    let disk = Disk::create(dir.path(), "big.img", size);
    let start = size - 20 * MIB;
    disk.embed(start, &nt);
    let (table, vols) = volumes(&disk.source(), 0).unwrap();
    assert_eq!(vols.len(), 1, "{:#?}", table);
    assert_eq!(table.entries[0].start, start);
    assert_eq!(table.entries[0].fs, Some(FsKind::Ntfs));
}

#[test]
fn table_pointing_at_nothing_triggers_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let Some(f12) = mkfs(dir.path(), Mk::Fat12) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 40 * MIB);
    disk.embed(MIB, &f12);
    // MBR from an earlier layout: its only entry points at zeros.
    disk.write_at(
        0,
        &mbr_sector(&[MbrPart {
            status: 0x80,
            ptype: 0x83,
            lba: sectors(20 * MIB),
            sectors: sectors(10 * MIB),
        }]),
    );
    let (table, vols) = volumes(&disk.source(), 512).unwrap();
    assert_eq!(table.scheme, PartitionScheme::Mbr);
    assert_eq!(vols.len(), 2);
    assert_eq!(table.entries[0].origin, "scan");
    assert_eq!(table.entries[0].fs, Some(FsKind::Fat12));
    assert_eq!(table.entries[1].origin, "mbr");
    assert_eq!(table.entries[1].fs, None);
    assert!(vols[0].probe.is_some() && vols[1].probe.is_none());
}

#[test]
fn superfloppy_volume() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::ExFat) else {
        return;
    };
    let src: SharedSource = Arc::new(FileSource::open(&img).unwrap());
    let (table, vols) = volumes(&src, 512).unwrap();
    assert_eq!(table.scheme, PartitionScheme::None);
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].entry.origin, "superfloppy");
    assert_eq!(vols[0].probe.as_ref().unwrap().kind, FsKind::ExFat);
    assert_eq!(vols[0].source.size(), Mk::ExFat.size());
}

#[test]
fn blank_device_has_no_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let disk = Disk::create(dir.path(), "blank.img", 300 * MIB);
    let (table, vols) = volumes(&disk.source(), 512).unwrap();
    assert_eq!(table.scheme, PartitionScheme::Unknown);
    assert!(vols.is_empty());
    assert!(table.warnings.iter().any(|w| w.contains("no volumes")));
    let (t2, v2) = volumes(&mem_source(Vec::new()), 512).unwrap();
    assert!(t2.entries.is_empty() && v2.is_empty());
}

#[test]
fn gpt_disk_with_4k_sectors() {
    let dir = tempfile::tempdir().unwrap();
    let Some(f32k) = mkfs(dir.path(), Mk::Fat32Sector4096) else {
        return;
    };
    let size = 100 * MIB;
    let disk = Disk::create(dir.path(), "g4k.img", size);
    let total = size / 4096;
    let first = 256u64; // 1 MiB
    let last = first + Mk::Fat32Sector4096.size() / 4096 - 1;
    let b = GptBuild::new(
        4096,
        total,
        vec![gpt_part(GUID_BASIC, 1, first, last, "data")],
    );
    b.write(&disk);
    disk.embed(first * 4096, &f32k);
    for hint in [0u32, 512, 4096] {
        let (table, vols) = volumes(&disk.source(), hint).unwrap();
        assert_eq!(table.scheme, PartitionScheme::Gpt, "hint {hint}");
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].entry.fs, Some(FsKind::Fat32));
        assert_eq!(vols[0].entry.start, MIB);
        assert_eq!(vols[0].source.size(), Mk::Fat32Sector4096.size());
    }
}
