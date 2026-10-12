//! Partition table reading: MBR, EBR chains, GPT (primary, backup, damaged),
//! hybrid MBRs, 4096-byte sectors and superfloppy volumes.

mod common;

use common::*;
use diskrescue_core::fs::FsKind;
use diskrescue_core::partition::PartitionScheme;
use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::{SharedSource, Source};
use diskrescue_partition::{
    read_partition_table, read_partition_table_detailed, read_partition_table_with,
};
use std::sync::Arc;

fn sectors(bytes: u64) -> u32 {
    (bytes / 512) as u32
}

// ------------------------------------------------------------------ MBR

#[test]
fn mbr_with_three_primary_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f32i), Some(ntfs), Some(ext)) = (
        mkfs(dir.path(), Mk::Fat32),
        mkfs(dir.path(), Mk::Ntfs),
        mkfs(dir.path(), Mk::Ext4Block1k),
    ) else {
        return;
    };
    let disk = Disk::create(dir.path(), "mbr.img", 130 * MIB);
    let (o1, o2, o3) = (MIB, 48 * MIB, 70 * MIB);
    disk.embed(o1, &f32i);
    disk.embed(o2, &ntfs);
    disk.embed(o3, &ext);
    disk.write_at(
        0,
        &mbr_sector(&[
            MbrPart {
                status: 0x80,
                ptype: 0x0C,
                lba: sectors(o1),
                sectors: sectors(Mk::Fat32.size()),
            },
            MbrPart {
                status: 0,
                ptype: 0x07,
                lba: sectors(o2),
                sectors: sectors(Mk::Ntfs.size()),
            },
            MbrPart {
                status: 0,
                ptype: 0x83,
                lba: sectors(o3),
                sectors: sectors(Mk::Ext4Block1k.size()),
            },
        ]),
    );
    let src = disk.source();
    let t = read_partition_table(&src).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    assert_eq!(t.entries.len(), 3);
    let starts: Vec<u64> = t.entries.iter().map(|e| e.start).collect();
    assert_eq!(starts, vec![o1, o2, o3]);
    let idx: Vec<u32> = t.entries.iter().map(|e| e.index).collect();
    assert_eq!(idx, vec![1, 2, 3]);
    assert_eq!(t.entries[0].size, Mk::Fat32.size());
    assert!(t.entries[0].bootable);
    assert!(!t.entries[1].bootable);
    assert_eq!(t.entries[0].type_id, "0x0C");
    assert_eq!(t.entries[0].type_desc, "W95 FAT32 (LBA)");
    assert_eq!(t.entries[1].type_id, "0x07");
    assert_eq!(t.entries[2].type_desc, "Linux");
    assert_eq!(t.entries[0].origin, "mbr");
    assert_eq!(t.entries[0].fs, Some(FsKind::Fat32));
    assert_eq!(t.entries[1].fs, Some(FsKind::Ntfs));
    assert_eq!(t.entries[2].fs, Some(FsKind::Ext4));
    assert!(t.disk_guid.is_none());
}

#[test]
fn mbr_type_byte_is_only_a_hint() {
    // A camera wrote type 0x0B for an exFAT volume.
    let dir = tempfile::tempdir().unwrap();
    let Some(ex) = mkfs(dir.path(), Mk::ExFat) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 20 * MIB);
    disk.embed(MIB, &ex);
    disk.write_at(
        0,
        &mbr_sector(&[MbrPart {
            status: 0x80,
            ptype: 0x0B,
            lba: sectors(MIB),
            sectors: sectors(Mk::ExFat.size()),
        }]),
    );
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.entries.len(), 1);
    assert_eq!(t.entries[0].type_desc, "W95 FAT32");
    assert_eq!(t.entries[0].fs, Some(FsKind::ExFat));
}

#[test]
fn extended_partition_with_logical_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f16), Some(f12), Some(e2)) = (
        mkfs(dir.path(), Mk::Fat16),
        mkfs(dir.path(), Mk::Fat12),
        mkfs(dir.path(), Mk::Ext2),
    ) else {
        return;
    };
    let disk = Disk::create(dir.path(), "ebr.img", 80 * MIB);
    let s = |b: u64| sectors(b);
    // Primary FAT16 at 1 MiB; extended container from 22 MiB to the end.
    let p1 = s(MIB);
    let ext_start = s(22 * MIB);
    let ext_len = s(80 * MIB) - ext_start;
    disk.embed(MIB, &f16);
    disk.write_at(
        0,
        &mbr_sector(&[
            MbrPart {
                status: 0x80,
                ptype: 0x0E,
                lba: p1,
                sectors: s(Mk::Fat16.size()),
            },
            MbrPart {
                status: 0,
                ptype: 0x0F,
                lba: ext_start,
                sectors: ext_len,
            },
        ]),
    );
    // Three logical partitions: FAT12 (4 MiB), ext2 (16 MiB), FAT12 (4 MiB).
    // Each logical starts 2048 sectors after its EBR.
    let sizes = [s(Mk::Fat12.size()), s(Mk::Ext2.size()), s(Mk::Fat12.size())];
    let images = [&f12, &e2, &f12];
    let types = [0x01u8, 0x83, 0x01];
    let mut ebr_rel = 0u32; // relative to the extended partition start
    let mut expected_starts = Vec::new();
    for i in 0..3 {
        let ebr_abs = ext_start + ebr_rel;
        let logical_abs = ebr_abs + 2048;
        expected_starts.push(u64::from(logical_abs) * 512);
        disk.embed(u64::from(logical_abs) * 512, images[i]);
        let next_rel = ebr_rel + 2048 + sizes[i];
        let mut entries = vec![MbrPart {
            status: 0,
            ptype: types[i],
            lba: 2048,
            sectors: sizes[i],
        }];
        if i < 2 {
            entries.push(MbrPart {
                status: 0,
                ptype: 0x05,
                lba: next_rel,
                sectors: 2048 + sizes[i + 1],
            });
        }
        disk.write_at(u64::from(ebr_abs) * 512, &mbr_sector(&entries));
        ebr_rel = next_rel;
    }
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    assert_eq!(t.entries.len(), 4, "{:#?}", t.entries);
    assert_eq!(t.entries[0].origin, "mbr");
    for (e, want) in t.entries[1..].iter().zip(&expected_starts) {
        assert_eq!(e.origin, "ebr");
        assert_eq!(e.start, *want);
    }
    assert_eq!(t.entries[0].fs, Some(FsKind::Fat16));
    assert_eq!(t.entries[1].fs, Some(FsKind::Fat12));
    assert_eq!(t.entries[2].fs, Some(FsKind::Ext2));
    assert_eq!(t.entries[3].fs, Some(FsKind::Fat12));
    // The extended container itself is not a partition.
    assert!(t.entries.iter().all(|e| e.type_id != "0x0F"));
}

#[test]
fn cyclic_ebr_chain_is_cut() {
    let mut data = vec![0u8; 8 * MIB as usize];
    let m = mbr_sector(&[MbrPart {
        status: 0,
        ptype: 0x05,
        lba: 100,
        sectors: 10_000,
    }]);
    data[..512].copy_from_slice(&m);
    // EBR at 100 describes one logical partition and links to LBA 100 again.
    let ebr = mbr_sector(&[
        MbrPart {
            status: 0,
            ptype: 0x83,
            lba: 63,
            sectors: 500,
        },
        MbrPart {
            status: 0,
            ptype: 0x05,
            lba: 0,
            sectors: 10_000,
        },
    ]);
    data[100 * 512..101 * 512].copy_from_slice(&ebr);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert_eq!(t.entries.len(), 1);
    assert_eq!(t.entries[0].start, 163 * 512);
    assert!(
        t.warnings.iter().any(|w| w.contains("loops")),
        "{:?}",
        t.warnings
    );
}

#[test]
fn long_ebr_chain_is_capped_at_128() {
    let n = 400usize;
    let mut data = vec![0u8; (n + 10) * 512];
    data[..512].copy_from_slice(&mbr_sector(&[MbrPart {
        status: 0,
        ptype: 0x0F,
        lba: 1,
        sectors: (n + 8) as u32,
    }]));
    for i in 0..n {
        let ebr = mbr_sector(&[
            MbrPart {
                status: 0,
                ptype: 0x83,
                lba: 1,
                sectors: 1,
            },
            MbrPart {
                status: 0,
                ptype: 0x05,
                lba: (i + 1) as u32,
                sectors: 2,
            },
        ]);
        data[(1 + i) * 512..(2 + i) * 512].copy_from_slice(&ebr);
    }
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.entries.len(), 128);
    assert!(t.warnings.iter().any(|w| w.contains("128")));
}

#[test]
fn mbr_sanity_warnings_and_clipping() {
    let mut data = vec![0u8; 10 * MIB as usize]; // 20480 sectors
    let m = mbr_sector(&[
        MbrPart {
            status: 0,
            ptype: 0x83,
            lba: 0,
            sectors: 100,
        },
        MbrPart {
            status: 0,
            ptype: 0x83,
            lba: 50,
            sectors: 100,
        },
        MbrPart {
            status: 0,
            ptype: 0x83,
            lba: 20_000,
            sectors: 5_000,
        },
        MbrPart {
            status: 0,
            ptype: 0x83,
            lba: 100_000,
            sectors: 10,
        },
    ]);
    data[..512].copy_from_slice(&m);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert_eq!(t.entries.len(), 3, "{:?}", t.entries);
    let w = t.warnings.join("\n");
    assert!(w.contains("LBA 0"), "{w}");
    assert!(w.contains("overlaps"), "{w}");
    assert!(w.contains("clipped"), "{w}");
    assert!(w.contains("beyond the end"), "{w}");
    // Clipped entry ends exactly at the device end.
    let last = t.entries.last().unwrap();
    assert_eq!(last.start + last.size, 10 * MIB);
    // Entries are in ascending order and numbered from 1.
    assert!(t.entries.windows(2).all(|p| p[0].start <= p[1].start));
    assert_eq!(t.entries[0].index, 1);
}

#[test]
fn mbr_with_empty_table() {
    let mut data = vec![0u8; MIB as usize];
    data[510] = 0x55;
    data[511] = 0xAA;
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert!(t.entries.is_empty());
    assert!(t.warnings.iter().any(|w| w.contains("empty")));
}

#[test]
fn blank_and_tiny_sources_are_unknown() {
    for len in [0usize, 100, 512, MIB as usize] {
        let t = read_partition_table(&mem_source(vec![0u8; len])).unwrap();
        assert_eq!(t.scheme, PartitionScheme::Unknown, "len {len}");
        assert!(t.entries.is_empty());
        assert!(!t.warnings.is_empty());
    }
}

#[test]
fn zeroed_mbr_sector_with_unreadable_first_sector() {
    use diskrescue_core::{Range, RangeSet};
    let mut set = RangeSet::new();
    set.insert(Range::new(0, 512));
    let src: SharedSource = Arc::new(MemSource::new(vec![0u8; MIB as usize]).with_unreadable(set));
    let t = read_partition_table(&src).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Unknown);
    assert!(t.warnings.iter().any(|w| w.contains("unreadable")));
}

// ------------------------------------------------------------------ GPT

/// 64 MiB disk (131072 sectors) with three partitions and no filesystems.
fn gpt_fixture(dir: &std::path::Path) -> (Disk, GptBuild, GptLayout) {
    let disk = Disk::create(dir, "gpt.img", 64 * MIB);
    let b = GptBuild::new(
        512,
        131_072,
        vec![
            gpt_part(GUID_ESP, 1, 2048, 2048 + 20_480 - 1, "EFI System"),
            gpt_part(GUID_BASIC, 2, 22_528, 22_528 + 40_960 - 1, "Windows data"),
            gpt_part(GUID_LINUX, 3, 63_488, 100_000, "root"),
        ],
    );
    let l = b.write(&disk);
    (disk, b, l)
}

fn assert_three_gpt_entries(t: &diskrescue_core::partition::PartitionTable, origin: &str) {
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 3, "{:#?}", t.entries);
    assert_eq!(t.entries[0].start, 2048 * 512);
    assert_eq!(t.entries[0].size, 20_480 * 512);
    assert_eq!(t.entries[1].start, 22_528 * 512);
    assert_eq!(t.entries[2].start, 63_488 * 512);
    assert_eq!(t.entries[2].size, (100_000 - 63_488 + 1) * 512);
    assert!(t.entries.iter().all(|e| e.origin == origin));
    assert_eq!(
        t.disk_guid.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
}

#[test]
fn gpt_valid_primary() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    assert_eq!(t.entries[0].type_id, GUID_ESP);
    assert_eq!(t.entries[0].type_desc, "EFI System");
    assert_eq!(t.entries[1].type_desc, "Microsoft basic data");
    assert_eq!(t.entries[2].type_desc, "Linux filesystem");
    assert_eq!(t.entries[0].name.as_deref(), Some("EFI System"));
    assert_eq!(
        t.entries[0].guid.as_deref(),
        Some("01010101-0101-0101-0101-010101010101")
    );
    assert!(!t.entries[0].bootable);
}

#[test]
fn gpt_with_real_filesystems_fills_fs() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f32i), Some(ntfs)) = (mkfs(dir.path(), Mk::Fat32), mkfs(dir.path(), Mk::Ntfs)) else {
        return;
    };
    let disk = Disk::create(dir.path(), "gpt.img", 130 * MIB);
    let total = 130 * MIB / 512;
    let b = GptBuild::new(
        512,
        total,
        vec![
            gpt_part(GUID_ESP, 1, 2048, 2048 + Mk::Fat32.size() / 512 - 1, "esp"),
            gpt_part(
                GUID_BASIC,
                2,
                100 * 1024 * 2,
                100 * 1024 * 2 + Mk::Ntfs.size() / 512 - 1,
                "win",
            ),
        ],
    );
    b.write(&disk);
    disk.embed(MIB, &f32i);
    disk.embed(100 * MIB, &ntfs);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries[0].fs, Some(FsKind::Fat32));
    assert_eq!(t.entries[1].fs, Some(FsKind::Ntfs));
}

#[test]
fn gpt_primary_header_crc_failure_uses_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    // Flip a bit in the disk GUID of the primary header: CRC no longer matches.
    let mut h = disk.read_at(512, 512);
    h[60] ^= 0x01;
    disk.write_at(512, &h);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 3);
    assert!(t.entries.iter().all(|e| e.origin == "gpt-backup"));
    let w = t.warnings.join("\n");
    assert!(w.contains("primary") && w.contains("CRC"), "{w}");
    assert!(w.contains("backup"), "{w}");
}

#[test]
fn gpt_primary_array_crc_failure_uses_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    // Damage an unused slot of the primary entry array.
    disk.write_at(2 * 512 + 100 * 128 + 40, &[0xAB]);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt-backup");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("entry array CRC32 mismatch")));
}

#[test]
fn gpt_backup_only_after_first_sectors_zeroed() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, l) = gpt_fixture(dir.path());
    // The first 34 sectors (MBR, header, array) are gone.
    disk.zero(0, (l.first_usable) * 512);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt-backup");
    let w = t.warnings.join("\n");
    assert!(w.contains("primary GPT header (LBA 1) is missing"), "{w}");
    assert!(w.contains("protective MBR"), "{w}");
}

#[test]
fn gpt_zeroed_protective_mbr_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    disk.zero(0, 512);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t.warnings.iter().any(|w| w.contains("protective MBR")));
}

#[test]
fn gpt_backup_damaged_is_reported_but_primary_used() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, l) = gpt_fixture(dir.path());
    disk.zero(l.backup_header_lba * 512, 512);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t.warnings.iter().any(|w| w.contains("backup")));
}

#[test]
fn gpt_both_copies_with_bad_array_crc_still_usable() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, l) = gpt_fixture(dir.path());
    disk.write_at(l.primary_array_lba * 512 + 100 * 128 + 40, &[0xAB]);
    disk.write_at(l.backup_array_lba * 512 + 101 * 128 + 40, &[0xCD]);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("no GPT copy fully verified")));
}

#[test]
fn gpt_preferred_over_overwritten_protective_mbr() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    // A real MBR with a single FAT partition replaced the protective MBR.
    disk.write_at(
        0,
        &mbr_sector(&[MbrPart {
            status: 0x80,
            ptype: 0x0C,
            lba: 2048,
            sectors: 1000,
        }]),
    );
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("regular MBR") && w.contains("GPT")));
}

#[test]
fn hybrid_mbr_contributes_extra_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    // Protective entry plus an MBR-only partition after the GPT ones.
    disk.write_at(
        0,
        &mbr_sector(&[
            MbrPart {
                status: 0,
                ptype: 0xEE,
                lba: 1,
                sectors: 131_071,
            },
            MbrPart {
                status: 0x80,
                ptype: 0x83,
                lba: 110_000,
                sectors: 5_000,
            },
            // Mirror of GPT partition 1: must not be duplicated.
            MbrPart {
                status: 0,
                ptype: 0xEF,
                lba: 2048,
                sectors: 20_480,
            },
        ]),
    );
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 4, "{:#?}", t.entries);
    let extra = t.entries.iter().find(|e| e.origin == "mbr").unwrap();
    assert_eq!(extra.start, 110_000 * 512);
    assert!(extra.bootable);
    assert!(t.warnings.iter().any(|w| w.contains("hybrid")));
}

#[test]
fn gpt_entry_beyond_device_is_clipped_or_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let disk = Disk::create(dir.path(), "g.img", 64 * MIB);
    let b = GptBuild::new(
        512,
        131_072,
        vec![
            gpt_part(GUID_BASIC, 1, 2048, 4095, "ok"),
            gpt_part(GUID_BASIC, 2, 100_000, 200_000, "too long"),
            gpt_part(GUID_BASIC, 3, 300_000, 400_000, "far away"),
            gpt_part(GUID_BASIC, 4, 9000, 8000, "inverted"),
        ],
    );
    b.write(&disk);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 2, "{:#?}", t.entries);
    assert_eq!(t.entries[1].start + t.entries[1].size, 64 * MIB);
    let w = t.warnings.join("\n");
    assert!(w.contains("clipped"), "{w}");
    assert!(w.contains("beyond the end"), "{w}");
    assert!(w.contains("before its first LBA"), "{w}");
}

#[test]
fn gpt_unknown_type_guid_is_printed() {
    let dir = tempfile::tempdir().unwrap();
    let disk = Disk::create(dir.path(), "g.img", 8 * MIB);
    let b = GptBuild::new(
        512,
        16_384,
        vec![gpt_part(
            "12345678-1234-1234-1234-123456789ABC",
            1,
            2048,
            4095,
            "mystery",
        )],
    );
    b.write(&disk);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.entries[0].type_id, "12345678-1234-1234-1234-123456789ABC");
    assert!(t.entries[0]
        .type_desc
        .contains("12345678-1234-1234-1234-123456789ABC"));
}

#[test]
fn gpt_truncated_image_notes_missing_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    let mut data = disk.mem();
    data.truncate(120_000 * 512);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("backup") && (w.contains("truncated") || w.contains("incomplete"))));
}

#[test]
fn gpt_larger_entry_size_and_count_are_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let disk = Disk::create(dir.path(), "g.img", 16 * MIB);
    let mut b = GptBuild::new(
        512,
        32_768,
        vec![
            gpt_part(GUID_BASIC, 1, 2048, 4095, "a"),
            gpt_part(GUID_LINUX, 2, 5000, 6000, "b"),
        ],
    );
    b.num_entries = 64;
    b.entry_size = 256;
    b.write(&disk);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 2);
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
}

// ---------------------------------------------------------- 4096-byte sectors

fn gpt_4kn_fixture(dir: &std::path::Path) -> (Disk, GptBuild, GptLayout) {
    let disk = Disk::create(dir, "gpt4k.img", 64 * MIB);
    let total = 64 * MIB / 4096; // 16384 sectors
    let b = GptBuild::new(
        4096,
        total,
        vec![
            gpt_part(GUID_ESP, 1, 256, 256 + 2559, "esp"),
            gpt_part(GUID_BASIC, 2, 3072, 6000, "data"),
        ],
    );
    let l = b.write(&disk);
    (disk, b, l)
}

#[test]
fn gpt_on_4kn_media_header_at_byte_4096() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, l) = gpt_4kn_fixture(dir.path());
    assert_eq!(l.primary_header_lba * 4096, 4096);
    // FileSource reports 512-byte sectors; the 4096 layout must be detected.
    let d = read_partition_table_detailed(&disk.source(), None).unwrap();
    assert_eq!(d.sector_size, 4096);
    let t = d.table;
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.entries[0].start, 256 * 4096);
    assert_eq!(t.entries[0].size, 2560 * 4096);
    assert_eq!(t.entries[1].start, 3072 * 4096);
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    // An explicit hint gives the same answer, and a wrong hint is corrected.
    for hint in [4096u32, 512] {
        let t = read_partition_table_with(&disk.source(), hint).unwrap();
        assert_eq!(t.entries[0].start, 256 * 4096, "hint {hint}");
    }
}

#[test]
fn gpt_4kn_backup_only() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, l) = gpt_4kn_fixture(dir.path());
    disk.zero(0, l.first_usable * 4096);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert_eq!(t.entries.len(), 2);
    assert!(t.entries.iter().all(|e| e.origin == "gpt-backup"));
    assert_eq!(t.entries[0].start, 256 * 4096);
}

#[test]
fn gpt_with_source_reporting_4096_sectors() {
    struct Sector4k(MemSource);
    impl Source for Sector4k {
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn sector_size(&self) -> u32 {
            4096
        }
        fn read_at(&self, o: u64, b: &mut [u8]) -> Result<usize, diskrescue_core::ReadError> {
            self.0.read_at(o, b)
        }
        fn describe(&self) -> String {
            "4k".into()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_4kn_fixture(dir.path());
    let src: SharedSource = Arc::new(Sector4k(MemSource::new(disk.mem())));
    let d = read_partition_table_detailed(&src, None).unwrap();
    assert_eq!(d.sector_size, 4096);
    assert_eq!(d.table.entries.len(), 2);
}

#[test]
fn mbr_on_4kn_media_uses_4096_byte_lbas() {
    let dir = tempfile::tempdir().unwrap();
    let (Some(f32i), Some(ntfs)) = (
        mkfs(dir.path(), Mk::Fat32Sector4096),
        mkfs(dir.path(), Mk::Ntfs),
    ) else {
        return;
    };
    let disk = Disk::create(dir.path(), "mbr4k.img", 100 * MIB);
    let p1 = MIB;
    let p2 = p1 + Mk::Fat32Sector4096.size();
    disk.embed(p1, &f32i);
    disk.embed(p2, &ntfs);
    let lba = |b: u64| (b / 4096) as u32;
    disk.write_at(
        0,
        &mbr_sector(&[
            MbrPart {
                status: 0x80,
                ptype: 0x0C,
                lba: lba(p1),
                sectors: lba(Mk::Fat32Sector4096.size()),
            },
            MbrPart {
                status: 0,
                ptype: 0x07,
                lba: lba(p2),
                sectors: lba(Mk::Ntfs.size()),
            },
        ]),
    );
    let d = read_partition_table_detailed(&disk.source(), None).unwrap();
    assert_eq!(d.sector_size, 4096);
    assert_eq!(d.table.entries.len(), 2);
    assert_eq!(d.table.entries[0].start, p1);
    assert_eq!(d.table.entries[0].size, Mk::Fat32Sector4096.size());
    assert_eq!(d.table.entries[0].fs, Some(FsKind::Fat32));
    assert_eq!(d.table.entries[1].start, p2);
    assert_eq!(d.table.entries[1].fs, Some(FsKind::Ntfs));
}

// ------------------------------------------------------------ superfloppy

#[test]
fn superfloppy_for_every_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (Mk::Fat12, FsKind::Fat12),
        (Mk::Fat16, FsKind::Fat16),
        (Mk::Fat32, FsKind::Fat32),
        (Mk::ExFat, FsKind::ExFat),
        (Mk::Ntfs, FsKind::Ntfs),
        (Mk::Ext2, FsKind::Ext2),
        (Mk::Ext3, FsKind::Ext3),
        (Mk::Ext4Block1k, FsKind::Ext4),
    ];
    for (mk, kind) in cases {
        let Some(img) = mkfs(dir.path(), mk) else {
            continue;
        };
        let src: SharedSource = Arc::new(FileSource::open(&img).unwrap());
        let t = read_partition_table(&src).unwrap();
        assert_eq!(t.scheme, PartitionScheme::None, "{mk:?}");
        assert_eq!(t.entries.len(), 1, "{mk:?}");
        let e = &t.entries[0];
        assert_eq!(e.origin, "superfloppy", "{mk:?}");
        assert_eq!(e.start, 0);
        assert_eq!(e.size, mk.size(), "{mk:?}");
        assert_eq!(e.fs, Some(kind), "{mk:?}");
        assert_eq!(e.index, 1);
    }
}

#[test]
fn superfloppy_smaller_than_the_card() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ntfs) else {
        return;
    };
    let disk = Disk::create(dir.path(), "card.img", 40 * MIB);
    disk.embed(0, &img);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::None);
    assert_eq!(t.entries[0].size, Mk::Ntfs.size());
    assert_eq!(t.entries[0].fs, Some(FsKind::Ntfs));
}

#[test]
fn superfloppy_with_damaged_boot_sector_found_via_backup() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Fat32) else {
        return;
    };
    let disk = Disk::create(dir.path(), "card.img", Mk::Fat32.size());
    disk.embed(0, &img);
    disk.zero(0, 512);
    let t = read_partition_table(&disk.source()).unwrap();
    assert_eq!(t.scheme, PartitionScheme::None);
    assert_eq!(t.entries[0].fs, Some(FsKind::Fat32));
    assert_eq!(t.entries[0].origin, "superfloppy");
}

#[test]
fn superfloppy_filesystem_larger_than_image_is_clipped() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Fat32) else {
        return;
    };
    let mut data = std::fs::read(&img).unwrap();
    data.truncate(10 * MIB as usize);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::None);
    assert_eq!(t.entries[0].size, 10 * MIB);
    assert!(t.warnings.iter().any(|w| w.contains("clipped")));
}

#[test]
fn hybrid_iso_image_keeps_its_mbr() {
    // isohybrid-style USB stick: an MBR with a partition and an ISO 9660
    // volume descriptor at 0x8000.
    let mut data = vec![0u8; 4 * MIB as usize];
    data[..512].copy_from_slice(&mbr_sector(&[MbrPart {
        status: 0x80,
        ptype: 0x17,
        lba: 64,
        sectors: 2048,
    }]));
    let pvd = 0x8000;
    data[pvd] = 1;
    data[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
    data[pvd + 6] = 1;
    data[pvd + 80..pvd + 84].copy_from_slice(&2048u32.to_le_bytes());
    data[pvd + 128..pvd + 130].copy_from_slice(&2048u16.to_le_bytes());
    let t = read_partition_table(&mem_source(data.clone())).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Mbr);
    assert_eq!(t.entries.len(), 1);
    assert!(t.warnings.iter().any(|w| w.contains("iso9660")));
    // Without the MBR the same image is a superfloppy ISO.
    data[..512].fill(0);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::None);
    assert_eq!(t.entries[0].fs, Some(FsKind::Other));
    assert!(t.entries[0].type_desc.contains("iso9660"));
}

#[test]
fn gpt_with_wrong_revision_warns() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, b, l) = gpt_fixture(dir.path());
    // Rewrite both headers with revision 2.0 and valid CRCs.
    let arr = b.array();
    let crc = crc32fast::hash(&arr);
    for (my, alt, arr_lba) in [
        (1, l.backup_header_lba, l.primary_array_lba),
        (l.backup_header_lba, 1, l.backup_array_lba),
    ] {
        let mut h = b.header(my, alt, arr_lba, crc);
        h[8..12].copy_from_slice(&0x0002_0000u32.to_le_bytes());
        h[16..20].fill(0);
        let c = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&c.to_le_bytes());
        disk.write_at(my * 512, &h);
    }
    let t = read_partition_table(&disk.source()).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t.warnings.iter().any(|w| w.contains("revision")));
}

#[test]
fn gpt_image_padded_beyond_the_disk_still_finds_its_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (disk, _, _) = gpt_fixture(dir.path());
    // The image was copied to a larger device: the backup header is no
    // longer at the last LBA, but the primary header says where it is.
    let mut data = disk.mem();
    data.extend(vec![0u8; 4 * MIB as usize]);
    let t = read_partition_table(&mem_source(data.clone())).unwrap();
    assert_three_gpt_entries(&t, "gpt");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("disk resized") || w.contains("device ends")));
    // Known limit: with the primary header also gone, the backup (now in the
    // middle of the device) cannot be located; the protective MBR still says
    // the disk is GPT.
    data[512..1024].fill(0);
    let t = read_partition_table(&mem_source(data)).unwrap();
    assert_eq!(t.scheme, PartitionScheme::Gpt);
    assert!(t.entries.is_empty());
    assert!(t.warnings.iter().any(|w| w.contains("no valid GPT header")));
}
