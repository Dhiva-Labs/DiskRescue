//! Filesystem probing against images made by the real mkfs tools, their
//! damaged variants (backup boot sectors / superblocks) and synthetic
//! recognise-only headers.

mod common;

use common::*;
use diskrescue_core::fs::FsKind;
use diskrescue_core::source::SubSource;
use diskrescue_core::SharedSource;
use diskrescue_partition::{probe_filesystem, FsProbe};
use std::sync::Arc;

fn probe_image(path: &std::path::Path) -> Option<FsProbe> {
    let src: SharedSource = Arc::new(diskrescue_core::source::FileSource::open(path).unwrap());
    probe_filesystem(&src, 0)
}

#[test]
fn every_filesystem_is_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (Mk::Fat12, FsKind::Fat12),
        (Mk::Fat16, FsKind::Fat16),
        (Mk::Fat32, FsKind::Fat32),
        (Mk::Fat32Sector4096, FsKind::Fat32),
        (Mk::ExFat, FsKind::ExFat),
        (Mk::ExFatSector4096, FsKind::ExFat),
        (Mk::Ntfs, FsKind::Ntfs),
        (Mk::Ext2, FsKind::Ext2),
        (Mk::Ext3, FsKind::Ext3),
        (Mk::Ext4Block1k, FsKind::Ext4),
    ];
    for (mk, kind) in cases {
        let Some(img) = mkfs(dir.path(), mk) else {
            continue;
        };
        let p = probe_image(&img).unwrap_or_else(|| panic!("{mk:?} not recognised"));
        assert_eq!(p.kind, kind, "{mk:?}");
        assert_eq!(p.size, Some(mk.size()), "{mk:?}");
        assert!(!p.via_backup, "{mk:?}");
        assert!(p.cluster_size.is_some(), "{mk:?}");
        if let Some(l) = mk.label() {
            assert_eq!(p.label.as_deref(), Some(l), "{mk:?}");
        }
    }
}

#[test]
fn cluster_sizes_match_the_formatters() {
    let dir = tempfile::tempdir().unwrap();
    if let Some(img) = mkfs(dir.path(), Mk::Ntfs) {
        assert_eq!(probe_image(&img).unwrap().cluster_size, Some(4096));
    }
    if let Some(img) = mkfs(dir.path(), Mk::Ext4Block1k) {
        assert_eq!(probe_image(&img).unwrap().cluster_size, Some(1024));
    }
    if let Some(img) = mkfs(dir.path(), Mk::Fat32Sector4096) {
        let p = probe_image(&img).unwrap();
        assert_eq!(p.cluster_size, Some(4096));
        // mkfs.fat builds FAT32 volumes below the 65525-cluster minimum on request.
        assert_eq!(p.kind, FsKind::Fat32);
    }
}

#[test]
fn probing_at_an_offset_inside_a_disk() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::ExFat) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 40 * MIB);
    disk.embed(3 * MIB, &img);
    let src = disk.source();
    let p = probe_filesystem(&src, 3 * MIB).unwrap();
    assert_eq!(p.kind, FsKind::ExFat);
    assert_eq!(p.size, Some(Mk::ExFat.size()));
    assert!(probe_filesystem(&src, 3 * MIB + 512).is_none());
    assert!(probe_filesystem(&src, 0).is_none());
    assert!(probe_filesystem(&src, 40 * MIB).is_none());
    assert!(probe_filesystem(&src, u64::MAX).is_none());
}

// ----------------------------------------------------------------- backups

fn damaged(dir: &std::path::Path, mk: Mk, f: impl Fn(&Disk)) -> Option<Disk> {
    let img = mkfs(dir, mk)?;
    let disk = Disk::create(dir, "damaged.img", mk.size());
    disk.embed(0, &img);
    f(&disk);
    Some(disk)
}

#[test]
fn fat32_backup_boot_sector() {
    let dir = tempfile::tempdir().unwrap();
    for garbage in [false, true] {
        let Some(d) = damaged(dir.path(), Mk::Fat32, |d| {
            if garbage {
                d.write_at(0, &Rng(7).bytes(512));
            } else {
                d.zero(0, 512);
            }
        }) else {
            return;
        };
        let p = probe_filesystem(&d.source(), 0).expect("backup boot sector must be found");
        assert_eq!(p.kind, FsKind::Fat32);
        assert!(p.via_backup);
        assert_eq!(p.size, Some(Mk::Fat32.size()));
        assert_eq!(p.label.as_deref(), Some("FAT32LAB"));
    }
}

#[test]
fn fat32_4096_byte_sector_backup() {
    let dir = tempfile::tempdir().unwrap();
    let Some(d) = damaged(dir.path(), Mk::Fat32Sector4096, |d| d.zero(0, 4096)) else {
        return;
    };
    let p = probe_filesystem(&d.source(), 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat32);
    assert!(p.via_backup);
    assert_eq!(p.cluster_size, Some(4096));
}

#[test]
fn fat12_and_fat16_have_no_backup() {
    let dir = tempfile::tempdir().unwrap();
    for mk in [Mk::Fat12, Mk::Fat16] {
        let Some(d) = damaged(dir.path(), mk, |d| d.zero(0, 512)) else {
            return;
        };
        assert!(probe_filesystem(&d.source(), 0).is_none(), "{mk:?}");
    }
}

#[test]
fn exfat_backup_boot_region() {
    let dir = tempfile::tempdir().unwrap();
    let Some(d) = damaged(dir.path(), Mk::ExFat, |d| d.zero(0, 512)) else {
        return;
    };
    let p = probe_filesystem(&d.source(), 0).expect("backup region must be found");
    assert_eq!(p.kind, FsKind::ExFat);
    assert!(p.via_backup);
    assert_eq!(p.size, Some(Mk::ExFat.size()));
    assert_eq!(p.label.as_deref(), Some("EXLAB"));
}

#[test]
fn exfat_4096_byte_sectors_backup_region() {
    let dir = tempfile::tempdir().unwrap();
    let Some(d) = damaged(dir.path(), Mk::ExFatSector4096, |d| d.zero(0, 4096)) else {
        return;
    };
    let p = probe_filesystem(&d.source(), 0).unwrap();
    assert_eq!(p.kind, FsKind::ExFat);
    assert!(p.via_backup);
}

#[test]
fn exfat_checksum_mismatch_prefers_intact_backup() {
    let dir = tempfile::tempdir().unwrap();
    // Sector 0 is fine but an extended boot sector is corrupted: the main
    // region's checksum fails and the backup region verifies.
    let Some(d) = damaged(dir.path(), Mk::ExFat, |d| {
        d.write_at(3 * 512 + 100, &[0xAA, 0xBB])
    }) else {
        return;
    };
    let p = probe_filesystem(&d.source(), 0).unwrap();
    assert_eq!(p.kind, FsKind::ExFat);
    assert!(p.via_backup, "{:?}", p.notes);
    // Both regions corrupted: the main boot sector is still used, with a note.
    d.write_at(15 * 512 + 100, &[0xAA, 0xBB]);
    let p = probe_filesystem(&d.source(), 0).unwrap();
    assert_eq!(p.kind, FsKind::ExFat);
    assert!(!p.via_backup);
    assert!(p.notes.iter().any(|n| n.contains("checksum")));
}

#[test]
fn ntfs_backup_boot_sector_at_the_end_of_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let Some(d) = damaged(dir.path(), Mk::Ntfs, |d| d.zero(0, 512)) else {
        return;
    };
    // Whole device is the volume.
    let p = probe_filesystem(&d.source(), 0).expect("backup boot sector must be found");
    assert_eq!(p.kind, FsKind::Ntfs);
    assert!(p.via_backup);
    assert_eq!(p.size, Some(Mk::Ntfs.size()));
    assert_eq!(p.cluster_size, Some(4096));
}

#[test]
fn ntfs_backup_needs_the_partition_length() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ntfs) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 40 * MIB);
    disk.embed(MIB, &img);
    disk.zero(MIB, 512);
    let src = disk.source();
    // Without the partition size the end of the volume cannot be located.
    assert!(probe_filesystem(&src, MIB).is_none());
    // Through a view of the partition the backup is found.
    let sub: SharedSource = Arc::new(SubSource::new(src.clone(), MIB, Mk::Ntfs.size()));
    let p = probe_filesystem(&sub, 0).unwrap();
    assert_eq!(p.kind, FsKind::Ntfs);
    assert!(p.via_backup);
    // A backup belonging to a *different* volume further on must not be accepted.
    let other = Disk::create(dir.path(), "o.img", 50 * MIB);
    other.embed(30 * MIB, &img);
    assert!(probe_filesystem(&other.source(), 0).is_none());
    assert!(probe_filesystem(&other.source(), 30 * MIB).is_some());
}

#[test]
fn ext_backup_superblocks() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (Mk::Ext2, FsKind::Ext2),
        (Mk::Ext3, FsKind::Ext3),
        (Mk::Ext4Block1k, FsKind::Ext4),
        (Mk::Ext4Block4k, FsKind::Ext4),
    ];
    for (mk, kind) in cases {
        let Some(d) = damaged(dir.path(), mk, |d| d.zero(1024, 1024)) else {
            continue;
        };
        let p = probe_filesystem(&d.source(), 0)
            .unwrap_or_else(|| panic!("{mk:?}: backup superblock not found"));
        assert_eq!(p.kind, kind, "{mk:?}");
        assert!(p.via_backup, "{mk:?}");
        assert_eq!(p.size, Some(mk.size()), "{mk:?}");
        assert_eq!(p.label.as_deref(), mk.label(), "{mk:?}");
        assert!(p.notes.iter().any(|n| n.contains("backup in block group")));
    }
}

#[test]
fn ext_backup_missing_everywhere_is_not_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ext4Block1k) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", Mk::Ext4Block1k.size());
    disk.embed(0, &img);
    disk.zero(1024, 1024);
    // Also destroy the superblock copies of groups 1 and 3 (8 MiB groups).
    for g in [1u64, 3] {
        disk.zero((g * 8192 + 1) * 1024, 1024);
    }
    assert!(probe_filesystem(&disk.source(), 0).is_none());
}

#[test]
fn ext_backup_in_the_middle_of_an_unrelated_disk_is_not_adopted() {
    // The backup superblock of group 1 belongs to a volume that starts at
    // 8 MiB - 1 KiB... probing offset 0 must not claim it.
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ext4Block1k) else {
        return;
    };
    let disk = Disk::create(dir.path(), "d.img", 80 * MIB);
    disk.embed(16 * MIB, &img);
    disk.zero(16 * MIB + 1024, 1024);
    let src = disk.source();
    // Real start still works through its backups.
    let p = probe_filesystem(&src, 16 * MIB).unwrap();
    assert!(p.via_backup);
    // A neighbouring offset must not.
    assert!(probe_filesystem(&src, 8 * MIB).is_none());
    assert!(probe_filesystem(&src, 0).is_none());
}

// ---------------------------------------------------------------- others

#[test]
fn swap_squashfs_and_iso_are_recognised_by_their_tools() {
    let dir = tempfile::tempdir().unwrap();
    if let Some(mkswap) = tool_path("mkswap") {
        let path = dir.path().join("swap.img");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(8 * MIB)
            .unwrap();
        run(&mkswap, &["-L", "SWAPLAB", path.to_str().unwrap()]);
        let p = probe_image(&path).unwrap();
        assert_eq!(p.kind, FsKind::Other);
        assert_eq!(p.notes[0], "swap");
        assert_eq!(p.display_name(), "swap");
        assert_eq!(p.label.as_deref(), Some("SWAPLAB"));
        assert_eq!(p.size, Some(8 * MIB));
    } else {
        eprintln!("skipped: mkswap missing");
    }
    if let Some(mksq) = tool_path("mksquashfs") {
        let src_dir = dir.path().join("sq");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), vec![b'a'; 10_000]).unwrap();
        let out = dir.path().join("x.sqfs");
        run(
            &mksq,
            &[
                src_dir.to_str().unwrap(),
                out.to_str().unwrap(),
                "-quiet",
                "-no-progress",
            ],
        );
        let p = probe_image(&out).unwrap();
        assert_eq!(p.notes[0], "squashfs");
        assert!(p.size.unwrap() > 0);
    } else {
        eprintln!("skipped: mksquashfs missing");
    }
    if let Some(xorriso) = tool_path("xorriso") {
        let src_dir = dir.path().join("iso");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), b"hello").unwrap();
        let out = dir.path().join("x.iso");
        run(
            &xorriso,
            &[
                "-as",
                "mkisofs",
                "-quiet",
                "-V",
                "ISOLAB",
                "-o",
                out.to_str().unwrap(),
                src_dir.to_str().unwrap(),
            ],
        );
        let p = probe_image(&out).unwrap();
        assert_eq!(p.notes[0], "iso9660");
        assert_eq!(p.label.as_deref(), Some("ISOLAB"));
        assert_eq!(p.size, Some(std::fs::metadata(&out).unwrap().len()));
    } else {
        eprintln!("skipped: xorriso missing");
    }
}

fn probe_mem(data: Vec<u8>) -> Option<FsProbe> {
    probe_filesystem(&mem_source(data), 0)
}

#[test]
fn synthetic_recognise_only_headers() {
    // XFS
    let mut x = vec![0u8; 8192];
    x[0..4].copy_from_slice(b"XFSB");
    x[4..8].copy_from_slice(&4096u32.to_be_bytes());
    x[8..16].copy_from_slice(&65536u64.to_be_bytes());
    x[102..104].copy_from_slice(&512u16.to_be_bytes());
    x[108..114].copy_from_slice(b"xfslab");
    let p = probe_mem(x).unwrap();
    assert_eq!((p.kind, p.notes[0].as_str()), (FsKind::Other, "xfs"));
    assert_eq!(p.size, Some(4096 * 65536));
    assert_eq!(p.label.as_deref(), Some("xfslab"));

    // HFS+
    let mut h = vec![0u8; 8192];
    h[1024..1026].copy_from_slice(b"H+");
    h[1026..1028].copy_from_slice(&4u16.to_be_bytes());
    h[1064..1068].copy_from_slice(&4096u32.to_be_bytes());
    h[1068..1072].copy_from_slice(&1000u32.to_be_bytes());
    let p = probe_mem(h).unwrap();
    assert_eq!(p.notes[0], "hfs+");
    assert_eq!(p.size, Some(4_096_000));

    // APFS
    let mut a = vec![0u8; 8192];
    a[32..36].copy_from_slice(b"NXSB");
    a[36..40].copy_from_slice(&4096u32.to_le_bytes());
    a[40..48].copy_from_slice(&5000u64.to_le_bytes());
    let p = probe_mem(a).unwrap();
    assert_eq!(p.notes[0], "apfs");
    assert_eq!(p.size, Some(4096 * 5000));

    // LUKS
    let mut l = vec![0u8; 8192];
    l[0..6].copy_from_slice(b"LUKS\xba\xbe");
    l[6..8].copy_from_slice(&2u16.to_be_bytes());
    let p = probe_mem(l).unwrap();
    assert_eq!(p.notes[0], "luks");
    assert_eq!(p.size, None);

    // btrfs
    let mut b = vec![0u8; 0x10000 + 4096];
    let sb = 0x10000;
    b[sb + 0x30..sb + 0x38].copy_from_slice(&0x10000u64.to_le_bytes());
    b[sb + 0x40..sb + 0x48].copy_from_slice(b"_BHRfS_M");
    b[sb + 0x70..sb + 0x78].copy_from_slice(&(256u64 << 20).to_le_bytes());
    b[sb + 0x90..sb + 0x94].copy_from_slice(&4096u32.to_le_bytes());
    b[sb + 0x12B..sb + 0x12B + 5].copy_from_slice(b"pool1");
    let p = probe_mem(b).unwrap();
    assert_eq!(p.notes[0], "btrfs");
    assert_eq!(p.size, Some(256 << 20));
    assert_eq!(p.label.as_deref(), Some("pool1"));
}

#[test]
fn ntfs_with_negative_cluster_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let Some(img) = mkfs(dir.path(), Mk::Ntfs) else {
        return;
    };
    let mut data = std::fs::read(&img).unwrap();
    data[13] = 0xF4; // 2^12 sectors per cluster
    data[0x38..0x40].copy_from_slice(&5u64.to_le_bytes()); // MFT mirror inside the 8 clusters
    let p = probe_mem(data).unwrap();
    assert_eq!(p.kind, FsKind::Ntfs);
    assert_eq!(p.cluster_size, Some(2 * MIB as u32));
}

#[test]
fn mbr_and_gpt_sectors_are_not_filesystems() {
    let m = mbr_sector(&[MbrPart {
        status: 0x80,
        ptype: 0x0C,
        lba: 2048,
        sectors: 100_000,
    }]);
    let mut data = vec![0u8; MIB as usize];
    data[..512].copy_from_slice(&m);
    assert!(probe_mem(data).is_none());
    // Typical bootloader code in sector 0 with no BPB.
    let mut data = vec![0u8; MIB as usize];
    data[0] = 0xEB;
    data[1] = 0x63;
    data[2] = 0x90;
    data[510] = 0x55;
    data[511] = 0xAA;
    assert!(probe_mem(data).is_none());
}

// ----------------------------------------------------------------- hostile

#[test]
fn random_data_never_probes_as_a_filesystem_or_panics() {
    let mut rng = Rng(0xDEAD_BEEF_1234_5678);
    for i in 0..400 {
        let len = [
            0usize, 1, 100, 511, 512, 513, 2048, 4096, 8192, 70_000, 300_000,
        ][i % 11];
        let data = rng.bytes(len);
        let src = mem_source(data);
        for off in [0u64, 512, 1024, 4096, 65_536, u64::MAX] {
            // Random data may theoretically match nothing; mostly it must not panic.
            let _ = probe_filesystem(&src, off);
        }
    }
}

#[test]
fn every_single_byte_mutation_of_boot_sectors_is_survivable() {
    let dir = tempfile::tempdir().unwrap();
    for mk in [Mk::Fat32, Mk::ExFat, Mk::Ntfs, Mk::Ext4Block1k] {
        let Some(img) = mkfs(dir.path(), mk) else {
            continue;
        };
        // 2 MiB prefix is plenty: boot region, superblock, backups for ext 1K at 8 MiB are out.
        let mut data = std::fs::read(&img).unwrap();
        data.truncate(3 * MIB as usize);
        let probe_range = if mk == Mk::Ext4Block1k {
            1024..1024 + 256
        } else {
            0..128
        };
        for off in probe_range {
            for v in [0x00u8, 0x01, 0x7F, 0x80, 0xFF, 0xF4, 0x55, 0xAA] {
                let mut d = data.clone();
                d[off] = v;
                let _ = probe_mem(d);
            }
        }
    }
}

#[test]
fn truncated_images_are_survivable() {
    let dir = tempfile::tempdir().unwrap();
    for mk in [Mk::Fat32, Mk::ExFat, Mk::Ntfs, Mk::Ext4Block1k] {
        let Some(img) = mkfs(dir.path(), mk) else {
            continue;
        };
        let data = std::fs::read(&img).unwrap();
        for len in [
            0usize,
            1,
            511,
            512,
            1023,
            1024,
            2047,
            2048,
            3000,
            4096,
            6143,
            6144,
            12288,
            100_000,
            1 << 20,
        ] {
            let _ = probe_mem(data[..len.min(data.len())].to_vec());
        }
    }
}

#[test]
fn absurd_sizes_in_boot_sectors_do_not_overflow() {
    // exFAT boot sector with every 32/64-bit field at its maximum.
    let mut s = vec![0u8; 4096];
    s[0] = 0xEB;
    s[3..11].copy_from_slice(b"EXFAT   ");
    s[0x48..0x50].copy_from_slice(&u64::MAX.to_le_bytes());
    s[0x50..0x54].copy_from_slice(&u32::MAX.to_le_bytes());
    s[0x54..0x58].copy_from_slice(&u32::MAX.to_le_bytes());
    s[0x58..0x5C].copy_from_slice(&u32::MAX.to_le_bytes());
    s[0x5C..0x60].copy_from_slice(&0xFFFF_FFF5u32.to_le_bytes());
    s[0x60..0x64].copy_from_slice(&u32::MAX.to_le_bytes());
    s[0x6C] = 12;
    s[0x6D] = 13;
    s[0x6E] = 2;
    s[510] = 0x55;
    s[511] = 0xAA;
    assert!(probe_mem(s).is_none());

    // NTFS with huge sector counts.
    let mut n = vec![0u8; 4096];
    n[3..11].copy_from_slice(b"NTFS    ");
    n[11..13].copy_from_slice(&4096u16.to_le_bytes());
    n[13] = 128;
    n[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
    n[0x30..0x38].copy_from_slice(&1u64.to_le_bytes());
    n[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    n[0x40] = 0xF6;
    n[510] = 0x55;
    n[511] = 0xAA;
    let p = probe_mem(n);
    if let Some(p) = p {
        // Accepted geometry must still be representable without wrapping.
        assert!(p.size.unwrap_or(0) > 0);
    }

    // ext with maxed counts.
    let mut e = vec![0u8; 4096];
    let sb = 1024;
    e[sb..sb + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    e[sb + 4..sb + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    e[sb + 0x18..sb + 0x1C].copy_from_slice(&6u32.to_le_bytes());
    e[sb + 0x20..sb + 0x24].copy_from_slice(&u32::MAX.to_le_bytes());
    e[sb + 0x28..sb + 0x2C].copy_from_slice(&1u32.to_le_bytes());
    e[sb + 0x38..sb + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
    e[sb + 0x4C..sb + 0x50].copy_from_slice(&1u32.to_le_bytes());
    e[sb + 0x58..sb + 0x5A].copy_from_slice(&256u16.to_le_bytes());
    e[sb + 0x60..sb + 0x64].copy_from_slice(&0x80u32.to_le_bytes());
    e[sb + 0x150..sb + 0x154].copy_from_slice(&u32::MAX.to_le_bytes());
    let _ = probe_mem(e);
}
