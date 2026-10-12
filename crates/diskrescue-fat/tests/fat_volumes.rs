//! FAT12/16/32 volumes made by mkfs.vfat and filled by the test writer,
//! checked by fsck.vfat -n, then read back through the crate.

mod support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_core::fs::{EntryConfidence, FsKind, WalkControl};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_fat::{open, open_fat, probe, FatOptions};
use support::fat_writer::{Alloc, Dir, FatImage};
use support::*;

const MIB: u64 = 1024 * 1024;

struct Built {
    img: PathBuf,
    /// path -> content of every file that must read back exactly.
    live: HashMap<String, Vec<u8>>,
    /// path -> content of deleted files whose data is still intact.
    deleted: HashMap<String, Vec<u8>>,
    used_total: Option<(u64, u64)>,
}

fn long255() -> String {
    format!("{}.dat", "n".repeat(251))
}

const UNICODE: &str = "Ünïcödé фото 日本.png";

fn build(tmp: &Path, bits: u32, size: u64) -> Option<Built> {
    let img = tmp.join(format!("fat{bits}.img"));
    if !mkfs_vfat(&img, size, bits, &format!("TEST{bits}")) {
        return None;
    }
    let mut w = FatImage::open(&img);
    assert_eq!(w.bits, bits, "mkfs made a different FAT width");
    let cs = w.cs() as usize;
    let mut live = HashMap::new();
    let mut deleted = HashMap::new();
    let mut put = |w: &mut FatImage, d: Dir, path: &str, data: Vec<u8>, how: Alloc| {
        let name = path.rsplit('/').next().unwrap();
        w.put(d, name, &data, how);
        live.insert(path.to_string(), data);
    };
    put(
        &mut w,
        Dir::Root,
        "Holiday Photo 2024.jpg",
        jpeg_like(cs * 3 + 100, 1),
        Alloc::Contiguous,
    );
    put(
        &mut w,
        Dir::Root,
        "readme.txt",
        text_like(10, 2),
        Alloc::Contiguous,
    );
    put(
        &mut w,
        Dir::Root,
        "EMPTY.BIN",
        Vec::new(),
        Alloc::Contiguous,
    );
    put(
        &mut w,
        Dir::Root,
        UNICODE,
        text_like(cs + 5, 3),
        Alloc::Contiguous,
    );
    put(
        &mut w,
        Dir::Root,
        &long255(),
        text_like(77, 4),
        Alloc::Contiguous,
    );
    let dcim = w.mkdir(Dir::Root, "DCIM");
    let canon = w.mkdir(dcim, "100CANON");
    put(
        &mut w,
        canon,
        "DCIM/100CANON/IMG_0001.JPG",
        jpeg_like(cs * 40 + 7, 5),
        Alloc::Fragmented,
    );
    for i in 0..70 {
        let p = format!("DCIM/100CANON/IMG_{:04}.JPG", 1000 + i);
        put(
            &mut w,
            canon,
            &p,
            jpeg_like(100 + i, 100 + i as u64),
            Alloc::Contiguous,
        );
    }
    // Files that get deleted afterwards.
    let del_long = jpeg_like(cs * 2 + 50, 6);
    w.put(canon, "deleted photo.jpg", &del_long, Alloc::Contiguous);
    let del_short = text_like(30, 7);
    w.put(Dir::Root, "GONE.TXT", &del_short, Alloc::Contiguous);
    let old = w.mkdir(Dir::Root, "Old Folder");
    let inner = text_like(cs + 1, 8);
    w.put(old, "inner file.txt", &inner, Alloc::Contiguous);
    let before = fsck_vfat(&img);
    assert!(before.is_some() || tool("fsck.vfat").is_none());
    w.delete(canon, "deleted photo.jpg", false);
    w.delete(Dir::Root, "GONE.TXT", false);
    w.delete(old, "inner file.txt", false);
    w.delete(Dir::Root, "Old Folder", false);
    deleted.insert("DCIM/100CANON/deleted photo.jpg".to_string(), del_long);
    deleted.insert("_ONE.TXT".to_string(), del_short);
    deleted.insert("Old Folder/inner file.txt".to_string(), inner);
    let used_total = fsck_vfat(&img);
    Some(Built {
        img,
        live,
        deleted,
        used_total,
    })
}

fn check_volume(bits: u32, size: u64, kind: FsKind) {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), bits, size) else {
        return;
    };
    let src = source(&b.img);
    let head = std::fs::read(&b.img).unwrap()[..4096].to_vec();
    assert_eq!(probe(&head), Some(kind));
    let fs = open(src.clone(), &FatOptions::default()).unwrap();
    let info = fs.info().clone();
    assert_eq!(info.kind, kind);
    assert_eq!(info.label.as_deref(), Some(format!("TEST{bits}").as_str()));
    assert!(info.serial.is_some());
    assert!(info.warnings.is_empty(), "warnings: {:?}", info.warnings);
    let all = entries(&*fs);

    // Ids are unique and every parent is an emitted directory.
    let ids: HashSet<u64> = all.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), all.len());
    let dirs: HashSet<u64> = all.iter().filter(|e| e.is_dir).map(|e| e.id).collect();
    for e in &all {
        if let Some(p) = e.parent_id {
            assert!(dirs.contains(&p), "{}: parent not emitted", e.path);
        }
    }
    // Directories come before their children.
    let mut seen = HashSet::new();
    for e in &all {
        if let Some(p) = e.parent_id {
            assert!(seen.contains(&p), "{} listed before its directory", e.path);
        }
        seen.insert(e.id);
    }
    assert!(
        !all.iter().any(|e| e.path.starts_with("$orphans")),
        "unexpected orphans"
    );

    for (path, data) in &b.live {
        let e = by_path(&all, path);
        assert!(!e.deleted, "{path}");
        assert_eq!(e.confidence, EntryConfidence::Allocated);
        assert_eq!(e.size, data.len() as u64);
        assert_eq!(e.extent_bytes(), data.len() as u64, "{path}");
        let (got, rep) = read(&*fs, e);
        assert!(got == *data, "{path}: content differs");
        assert!(rep.is_complete(), "{path}: {rep:?}");
        assert!(rep.holes.is_empty());
    }
    // Timestamps and names.
    let h = by_path(&all, "Holiday Photo 2024.jpg");
    assert_eq!(h.modified.unwrap().to_rfc3339(), "2024-05-01T12:30:10Z");
    let c = h.created.unwrap();
    assert_eq!(
        (c.to_rfc3339().as_str(), c.nanos),
        ("2024-05-01T12:30:10Z", 500_000_000)
    );
    assert_eq!(h.accessed.unwrap().to_rfc3339(), "2024-05-01T00:00:00Z");
    assert_eq!(h.extension().as_deref(), Some("jpg"));
    assert_eq!(by_path(&all, "EMPTY.BIN").extents.len(), 0);
    let big = by_path(&all, "DCIM/100CANON/IMG_0001.JPG");
    assert!(
        big.extents.len() > 10,
        "fragmented file should have many extents"
    );
    assert!(by_path(&all, "DCIM").is_dir);
    assert_eq!(
        by_path(&all, "DCIM/100CANON").parent_id,
        Some(by_path(&all, "DCIM").id)
    );

    // Deleted entries.
    for (path, data) in &b.deleted {
        let e = by_path(&all, path);
        assert!(e.deleted, "{path}");
        assert_eq!(e.confidence, EntryConfidence::DeletedGuessed, "{path}");
        assert!(
            e.flags.iter().any(|f| f.contains("assumed contiguous")),
            "{path}: {:?}",
            e.flags
        );
        let (got, rep) = read(&*fs, e);
        assert!(got == *data, "{path}: deleted content differs");
        assert!(rep.reused.is_empty(), "{path}: {rep:?}");
        assert!(rep.notes.iter().any(|n| n.contains("assumed contiguous")));
    }
    let gone = by_path(&all, "_ONE.TXT");
    assert!(gone.flags.contains(&"first character lost".to_string()));
    let old = by_path(&all, "Old Folder");
    assert!(old.is_dir && old.deleted);
    assert_eq!(
        by_path(&all, "Old Folder/inner file.txt").parent_id,
        Some(old.id)
    );

    // Free space matches fsck's count and (FAT32) the FSInfo sector.
    let fat = open_fat(src.clone(), &FatOptions::default()).unwrap();
    let free = fat.free_clusters();
    let un = fs.unallocated().unwrap();
    assert_eq!(un.total_len(), free * info.cluster_size as u64);
    if let Some((used, total)) = b.used_total {
        assert_eq!(total, fat.cluster_count() as u64);
        assert_eq!(free, total - used, "free clusters vs fsck.vfat");
    }
    if kind == FsKind::Fat32 {
        assert_eq!(fat.fs_info().unwrap().free_count(), Some(free as u32));
    }
    // Free ranges lie in the data area, are cluster aligned and exclude live data.
    for r in un.iter() {
        assert!(r.start >= info.data_start);
        assert_eq!((r.start - info.data_start) % info.cluster_size as u64, 0);
    }
    for path in b.live.keys() {
        for x in &by_path(&all, path).extents {
            assert!(!un.intersects(*x), "{path} overlaps free space");
        }
    }
    let alloc = fs.allocated().unwrap();
    assert_eq!(alloc.total_len() + un.total_len(), info.total_size);
    // Chain helper agrees with the entry's extents.
    let first = fat.cluster_to_offset(2);
    assert_eq!(first, info.data_start);
    assert_eq!(fat.boot_sector().bpb_num_fats, 2);
}

#[test]
fn fat12_two_mib() {
    check_volume(12, 2 * MIB, FsKind::Fat12);
}

#[test]
fn fat16_thirty_two_mib() {
    check_volume(16, 32 * MIB, FsKind::Fat16);
}

#[test]
fn fat32_sixty_four_mib() {
    check_volume(32, 64 * MIB, FsKind::Fat32);
}

#[test]
fn walk_control_skip_and_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), 16, 32 * MIB) else {
        return;
    };
    let fs = open(source(&b.img), &FatOptions::default()).unwrap();
    let mut paths = Vec::new();
    fs.walk(&mut |e| {
        paths.push(e.path.clone());
        if e.path == "DCIM" {
            WalkControl::SkipChildren
        } else {
            WalkControl::Continue
        }
    })
    .unwrap();
    assert!(paths.contains(&"DCIM".to_string()));
    assert!(!paths.iter().any(|p| p.starts_with("DCIM/")));
    // Skipped directories must not reappear as lost orphans.
    assert!(
        !paths.iter().any(|p| p.starts_with("$orphans")),
        "{paths:?}"
    );
    let mut n = 0;
    fs.walk(&mut |_| {
        n += 1;
        if n == 3 {
            WalkControl::Stop
        } else {
            WalkControl::Continue
        }
    })
    .unwrap();
    assert_eq!(n, 3);
    // Entry limit.
    let opts = FatOptions {
        max_entries: 5,
        ..FatOptions::default()
    };
    let fs = open(source(&b.img), &opts).unwrap();
    assert_eq!(entries(&*fs).len(), 5);
    // Without deleted entries.
    let opts = FatOptions {
        include_deleted: false,
        ..FatOptions::default()
    };
    let fs = open(source(&b.img), &opts).unwrap();
    let all = entries(&*fs);
    assert!(all.iter().all(|e| !e.deleted));
    assert_eq!(all.iter().filter(|e| !e.is_dir).count(), b.live.len());
}

#[test]
fn fat32_windows_delete_zeroes_high_word() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("win.img");
    if !mkfs_vfat(&img, 64 * MIB, 32, "WIN") {
        return;
    }
    let mut w = FatImage::open(&img);
    assert!(w.count > 70_100);
    let cs = w.cs() as usize;
    let photo = jpeg_like(cs * 3 + 9, 11);
    w.put(Dir::Root, "far away.jpg", &photo, Alloc::At(70_000));
    let notes = text_like(cs * 2, 12);
    w.put(Dir::Root, "notes far.txt", &notes, Alloc::At(70_010));
    // Something live where the truncated cluster number points.
    let blocker = text_like(cs, 13);
    w.put(
        Dir::Root,
        "blocker.bin",
        &blocker,
        Alloc::At(70_010 - 65_536),
    );
    w.delete(Dir::Root, "far away.jpg", true);
    w.delete(Dir::Root, "notes far.txt", true);
    fsck_vfat(&img);
    let fat = open_fat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fat);
    let far = by_path(&all, "far away.jpg");
    assert!(far.deleted);
    assert_eq!(far.extents[0].start, fat.cluster_to_offset(70_000));
    assert!(
        far.flags.iter().any(|f| f.contains("high word")),
        "{:?}",
        far.flags
    );
    assert!(read(&fat, far).0 == photo);
    let n = by_path(&all, "notes far.txt");
    assert!(
        n.flags
            .iter()
            .any(|f| f.contains("first cluster may be incomplete")),
        "{:?}",
        n.flags
    );
    assert_eq!(n.confidence, EntryConfidence::DeletedGuessed);
    let (_, rep) = read(&fat, n);
    assert!(!rep.reused.is_empty());
    assert!(!rep.is_complete());
}

#[test]
fn backup_boot_sector_is_used() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), 32, 64 * MIB) else {
        return;
    };
    let mut bytes = std::fs::read(&b.img).unwrap();
    bytes[..512].fill(0);
    let src: SharedSource = Arc::new(MemSource::new(bytes.clone()));
    assert_eq!(probe(&bytes[..4096]), Some(FsKind::Fat32));
    let fs = open(src.clone(), &FatOptions::default()).unwrap();
    assert!(fs.info().warnings.iter().any(|w| w.contains("backup")));
    let all = entries(&*fs);
    let e = by_path(&all, "DCIM/100CANON/IMG_0001.JPG");
    assert!(read(&*fs, e).0 == b.live["DCIM/100CANON/IMG_0001.JPG"]);
    let no_backup = FatOptions {
        use_backup_boot_sector: false,
        ..FatOptions::default()
    };
    assert!(open_fat(src.clone(), &no_backup).is_err());
    // Unreadable sector 0 behaves the same.
    let orig = std::fs::read(&b.img).unwrap();
    let src: SharedSource =
        Arc::new(MemSource::new(orig).with_unreadable(RangeSet::from_range(Range::new(0, 512))));
    let fs = open_fat(src, &FatOptions::default()).unwrap();
    assert!(fs
        .volume_info()
        .warnings
        .iter()
        .any(|w| w.contains("backup")));
}

#[test]
fn fat16_without_boot_sector_is_reconstructed() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), 16, 32 * MIB) else {
        return;
    };
    let mut bytes = std::fs::read(&b.img).unwrap();
    bytes[..512].fill(0);
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    // Without the fallbacks the volume cannot be opened, and fails cleanly.
    let strict = FatOptions {
        use_backup_boot_sector: false,
        ..FatOptions::default()
    };
    assert!(open(src.clone(), &strict).is_err());
    // With them (the default) the geometry is rebuilt from the FATs and
    // directories, and every live file reads back exactly.
    let fs = open(src, &FatOptions::default()).unwrap();
    assert_eq!(fs.info().kind, FsKind::Fat16);
    assert!(fs
        .info()
        .warnings
        .iter()
        .any(|w| w.contains("reconstructed")));
    let all = entries(&*fs);
    for (path, data) in &b.live {
        let e = by_path(&all, path);
        assert_eq!(&read(&*fs, e).0, data, "{path}");
    }
}

#[test]
fn unreadable_fat1_sector_falls_back_to_fat2() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), 16, 32 * MIB) else {
        return;
    };
    let w = FatImage::open(&b.img);
    let fat1 = w.fat_offset(0);
    let fat_len = w.fat_sz * w.bps;
    drop(w);
    let bytes = std::fs::read(&b.img).unwrap();
    // The whole of FAT1 is unreadable: every lookup must come from FAT2.
    let src: SharedSource = Arc::new(
        MemSource::new(bytes).with_unreadable(RangeSet::from_range(Range::with_len(fat1, fat_len))),
    );
    let fs = open_fat(src, &FatOptions::default()).unwrap();
    let all = entries(&fs);
    for (path, data) in &b.live {
        let e = by_path(&all, path);
        assert!(read(&fs, e).0 == *data, "{path}");
    }
}

#[test]
fn unreadable_sector_inside_file_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path(), 12, 2 * MIB) else {
        return;
    };
    let bytes = std::fs::read(&b.img).unwrap();
    let fs0 = open_fat(source(&b.img), &FatOptions::default()).unwrap();
    let all = entries(&fs0);
    let path = "DCIM/100CANON/IMG_0001.JPG";
    let e = by_path(&all, path).clone();
    // Second extent, one sector in.
    let ext = e.extents[1];
    let bad = Range::with_len(ext.start + 512, 512);
    let file_off = e.extents[0].len() + 512;
    let src: SharedSource =
        Arc::new(MemSource::new(bytes).with_unreadable(RangeSet::from_range(bad)));
    let fs = open_fat(src, &FatOptions::default()).unwrap();
    let (got, rep) = read(&fs, &e);
    assert_eq!(rep.unreadable.as_slice(), &[Range::with_len(file_off, 512)]);
    assert!(!rep.is_complete());
    let want = &b.live[path];
    let a = file_off as usize;
    assert!(got[a..a + 512].iter().all(|&x| x == 0));
    assert!(got[..a] == want[..a]);
    assert!(got[a + 512..] == want[a + 512..]);
}

#[test]
fn orphan_directories_after_reformat() {
    let tmp = tempfile::tempdir().unwrap();
    for (bits, size) in [(32u32, 64 * MIB), (16, 32 * MIB)] {
        let img = tmp.path().join(format!("re{bits}.img"));
        if !mkfs_vfat(&img, size, bits, "CAM") {
            return;
        }
        let mut w = FatImage::open(&img);
        let cs = w.cs() as usize;
        let dcim = w.mkdir(Dir::Root, "DCIM");
        let sub = w.mkdir(dcim, "100CANON");
        let mut files = Vec::new();
        // Four files with one LFN slot each: the directory fits in one
        // 512-byte cluster, so the contiguity guess for it is exact.
        for i in 0..4u64 {
            let d = jpeg_like(cs * 2 + 17 * i as usize, 40 + i);
            let name = format!("Holiday {i}.jpg");
            w.put(sub, &name, &d, Alloc::Contiguous);
            files.push((name, d));
        }
        let Dir::Cluster(dcim_c) = dcim else {
            unreachable!()
        };
        drop(w);
        // Quick reformat: FATs and root are rewritten, data clusters stay.
        let t = tool("mkfs.vfat").unwrap();
        let (ok, out) = run(
            &t,
            &["-F", &bits.to_string(), "-n", "CAM", img.to_str().unwrap()],
        );
        assert!(ok, "{out}");
        let fs = open_fat(source(&img), &FatOptions::default()).unwrap();
        let all = entries(&fs);
        let root = by_path(&all, "$orphans");
        assert!(root.is_dir);
        let od = by_path(&all, &format!("$orphans/cluster_{dcim_c}"));
        assert!(od.is_dir && od.parent_id == Some(root.id));
        for (name, d) in &files {
            let p = format!("$orphans/cluster_{dcim_c}/100CANON/{name}");
            let e = by_path(&all, &p);
            assert!(
                !e.deleted,
                "{p}: deleted flag must be preserved (entry was live)"
            );
            assert_eq!(e.confidence, EntryConfidence::DeletedGuessed);
            assert!(e.flags.contains(&"orphan".to_string()));
            assert!(read(&fs, e).0 == *d, "{p}");
        }
        // The subdirectory was reached through its parent, not listed twice.
        assert_eq!(
            all.iter()
                .filter(|e| e.path.starts_with("$orphans/cluster_") && e.parent_id == Some(root.id))
                .count(),
            1
        );
    }
}

#[test]
fn lost_directory_with_intact_fat() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("lost.img");
    if !mkfs_vfat(&img, 32 * MIB, 16, "LOST") {
        return;
    }
    let mut w = FatImage::open(&img);
    let cs = w.cs() as usize;
    let photos = w.mkdir(Dir::Root, "PHOTOS");
    let d1 = jpeg_like(cs * 5 + 3, 70);
    w.put(photos, "fragmented one.jpg", &d1, Alloc::Fragmented);
    let d2 = jpeg_like(cs + 3, 71);
    w.put(photos, "two.jpg", &d2, Alloc::Contiguous);
    w.wipe_entry(Dir::Root, "PHOTOS");
    let Dir::Cluster(c) = photos else {
        unreachable!()
    };
    drop(w);
    let fs = open_fat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let od = by_path(&all, &format!("$orphans/cluster_{c}"));
    assert!(od.flags.iter().any(|f| f.contains("lost directory")));
    for (name, d) in [("fragmented one.jpg", &d1), ("two.jpg", &d2)] {
        let e = by_path(&all, &format!("$orphans/cluster_{c}/{name}"));
        assert_eq!(e.confidence, EntryConfidence::Allocated, "{name}");
        assert!(read(&fs, e).0 == *d, "{name}");
    }
}

#[test]
fn chain_helper_and_cycles() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("chain.img");
    if !mkfs_vfat(&img, 2 * MIB, 12, "CHAIN") {
        return;
    }
    let mut w = FatImage::open(&img);
    let cs = w.cs();
    let p = w.put(
        Dir::Root,
        "frag.bin",
        &text_like(cs as usize * 6, 3),
        Alloc::Fragmented,
    );
    // Make a loop: last cluster points back at the first.
    let last = *p.clusters.last().unwrap();
    w.set(last, p.clusters[0]);
    w.flush();
    drop(w);
    let fat = open_fat(source(&img), &FatOptions::default()).unwrap();
    let ch = fat.chain(p.clusters[0]);
    assert_eq!(ch.iter().map(|r| r.len()).sum::<u64>(), 6 * cs);
    assert_eq!(ch[0].start, fat.cluster_to_offset(p.clusters[0]));
    let all = entries(&fat);
    let e = by_path(&all, "frag.bin");
    assert_eq!(e.extent_bytes(), 6 * cs);
    assert!(
        e.flags
            .iter()
            .any(|f| f.contains("longer than the file size")),
        "{:?}",
        e.flags
    );
}
