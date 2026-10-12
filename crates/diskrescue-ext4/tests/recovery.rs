//! Deleted files, damaged metadata, unreadable sectors and hostile input.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem, FsKind, WalkControl};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_ext4::{open, probe, ExtFs, ExtOptions};

/// Source tree for the deletion tests.
fn deletion_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut m = BTreeMap::new();
    let files = [
        ("top.txt", b"top level\n".to_vec()),
        ("photos/keep.jpg", noise(21, 40 * 1024)),
        ("photos/victim.jpg", noise(22, 30 * 1024 + 5)),
        ("photos/zz_last.jpg", noise(23, 3000)),
        ("gone/inner.txt", b"inside a deleted directory\n".to_vec()),
        ("gone/inner2.bin", noise(24, 9000)),
        ("lost/unlinked.bin", noise(25, 7000)),
    ];
    for (rel, data) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &data).unwrap();
        m.insert(rel.to_string(), data);
    }
    m
}

fn debugfs_w(img: &Path, cmd: &str) {
    run("debugfs", &["-w", "-R", cmd, &img.to_string_lossy()]);
}

fn build_deleted(
    mkfs: &str,
    size: u64,
    args: &[&str],
) -> Option<(
    tempfile::TempDir,
    std::path::PathBuf,
    BTreeMap<String, Vec<u8>>,
)> {
    if !have(&[mkfs, "debugfs"]) {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let data = deletion_tree(&src);
    let img = dir.path().join("del.img");
    make_image(&img, size, mkfs, args, Some(&src));
    Some((dir, img, data))
}

fn check_debugfs_deletions(mkfs: &str, size: u64, args: &[&str]) {
    let Some((_dir, img, data)) = build_deleted(mkfs, size, args) else {
        return;
    };
    debugfs_w(&img, "rm /photos/victim.jpg");
    debugfs_w(&img, "rm /gone/inner.txt");
    debugfs_w(&img, "rm /gone/inner2.bin");
    debugfs_w(&img, "rmdir /gone");

    let fs = open_path(&img);
    let all = entries(&fs);
    let map = by_path(&all);
    let deleted: BTreeSet<&str> = all
        .iter()
        .filter(|e| e.deleted)
        .map(|e| e.path.as_str())
        .collect();
    assert_eq!(
        deleted,
        [
            "gone",
            "gone/inner.txt",
            "gone/inner2.bin",
            "photos/victim.jpg"
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );
    assert!(!all.iter().any(|e| e.path.starts_with("$orphans")));
    // debugfs keeps the mapping: the data comes back, flagged as a guess.
    for p in ["photos/victim.jpg", "gone/inner.txt", "gone/inner2.bin"] {
        let e = &map[p];
        assert_eq!(e.confidence, EntryConfidence::DeletedGuessed, "{p}");
        assert_eq!(e.size, data[p].len() as u64);
        let (bytes, rep) = read(&fs, e);
        assert!(bytes == data[p], "{p}: content differs");
        assert!(
            rep.reused.is_empty() && rep.unreadable.is_empty(),
            "{p}: {rep:?}"
        );
    }
    assert!(map["gone"].is_dir);
    assert_eq!(map["gone"].confidence, EntryConfidence::DeletedGuessed);
    for p in ["top.txt", "photos/keep.jpg", "photos/zz_last.jpg"] {
        let (bytes, _) = read(&fs, &map[p]);
        assert!(bytes == data[p]);
        assert_eq!(map[p].confidence, EntryConfidence::Allocated);
    }

    // Without deleted entries nothing deleted is listed.
    let opts = ExtOptions {
        include_deleted: false,
        ..ExtOptions::default()
    };
    let live = open(file_source(&img), &opts).unwrap();
    assert!(entries(&live).iter().all(|e| !e.deleted));

    // Emulate the kernel: i_block, size and block count zeroed.
    let ino = ExtFs::inode_number(&map["photos/victim.jpg"]);
    let slots: Vec<String> = (0..12)
        .map(|i| i.to_string())
        .chain(["IND", "DIND", "TIND"].map(String::from))
        .collect();
    for slot in slots {
        debugfs_w(&img, &format!("set_inode_field <{ino}> block[{slot}] 0"));
    }
    debugfs_w(&img, &format!("set_inode_field <{ino}> size 0"));
    debugfs_w(&img, &format!("set_inode_field <{ino}> blocks 0"));
    let fs = open_path(&img);
    let map = by_path(&entries(&fs));
    let e = &map["photos/victim.jpg"];
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedNoData);
    assert!(e.extents.is_empty());
    assert!(e.modified.is_some(), "timestamps survive");
    let mut sink = Vec::new();
    assert!(fs.read_file(e, &mut sink).is_err());
}

#[test]
fn debugfs_deletions_ext4() {
    check_debugfs_deletions("mkfs.ext4", 64 << 20, &["-b", "4096"]);
}

#[test]
fn debugfs_deletions_ext2_block_maps() {
    check_debugfs_deletions("mke2fs", 16 << 20, &["-t", "ext2", "-b", "1024"]);
}

#[test]
fn unlinked_and_nameless_inodes_go_to_orphans() {
    let Some((_dir, img, data)) = build_deleted("mkfs.ext4", 16 << 20, &["-b", "1024"]) else {
        return;
    };
    let before = by_path(&entries(&open_path(&img)));
    let unlinked_ino = ExtFs::inode_number(&before["lost/unlinked.bin"]);
    let victim_ino = ExtFs::inode_number(&before["photos/victim.jpg"]);
    // `unlink` removes the name but keeps the inode allocated: a lost file.
    debugfs_w(&img, "unlink /lost/unlinked.bin");
    // `rm` then wipe the name in the directory block, as kernels >= 5.14 do.
    debugfs_w(&img, "rm /photos/victim.jpg");
    let mut bytes = std::fs::read(&img).unwrap();
    let pos = bytes
        .windows(10)
        .position(|w| w == b"victim.jpg")
        .expect("name in directory block");
    bytes[pos - 8..pos + 10].fill(0);
    std::fs::write(&img, &bytes).unwrap();

    let fs = open_path(&img);
    let all = entries(&fs);
    let map = by_path(&all);
    assert!(!map.contains_key("lost/unlinked.bin"));
    assert!(!map.contains_key("photos/victim.jpg"));
    assert!(map["$orphans"].is_dir);
    let lost = &map[&format!("$orphans/inode_{unlinked_ino}")];
    assert!(!lost.deleted);
    assert_eq!(lost.confidence, EntryConfidence::Allocated);
    assert!(read(&fs, lost).0 == data["lost/unlinked.bin"]);
    let victim = &map[&format!("$orphans/inode_{victim_ino}")];
    assert!(victim.deleted);
    assert_eq!(victim.confidence, EntryConfidence::DeletedGuessed);
    assert!(read(&fs, victim).0 == data["photos/victim.jpg"]);
    assert_eq!(victim.parent_id, Some(diskrescue_ext4::ORPHANS_DIR_ID));
}

#[test]
fn killed_inode_behind_a_live_name_is_deleted() {
    let Some((_dir, img, _data)) = build_deleted("mkfs.ext4", 16 << 20, &["-b", "1024"]) else {
        return;
    };
    // kill_file frees the inode and blocks but leaves the directory entry.
    debugfs_w(&img, "kill_file /top.txt");
    let fs = open_path(&img);
    let map = by_path(&entries(&fs));
    let e = &map["top.txt"];
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedGuessed);
}

fn zero(img: &Path, start: u64, len: u64) {
    let mut bytes = std::fs::read(img).unwrap();
    bytes[start as usize..(start + len) as usize].fill(0);
    std::fs::write(img, &bytes).unwrap();
}

fn live_paths(fs: &ExtFs) -> BTreeSet<String> {
    entries(fs)
        .into_iter()
        .filter(|e| !e.deleted)
        .map(|e| e.path)
        .collect()
}

fn check_backup_superblock(size: u64, bs: &str, backup_offset: u64) {
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let expect = standard_tree(&src);
    let img = dir.path().join("sb.img");
    make_image(&img, size, "mkfs.ext4", &["-b", bs], Some(&src));
    let good = open_path(&img);
    let want = live_paths(&good);
    zero(&img, 1024, 1024);
    let s = file_source(&img);
    assert_eq!(
        probe(&s),
        Some(FsKind::Ext4),
        "probe falls back to the group-1 copy"
    );
    let fs = open(s.clone(), &ExtOptions::default()).unwrap();
    assert_eq!(fs.superblock_offset(), backup_offset);
    assert!(fs.info().warnings.iter().any(|w| w.contains("backup")));
    assert_eq!(live_paths(&fs), want);
    let map = by_path(&entries(&fs));
    if let Expect::File(data) = &expect["DCIM/100CANON/IMG_0002.JPG"] {
        assert!(read(&fs, &map["DCIM/100CANON/IMG_0002.JPG"]).0 == *data);
    }
    let no_backup = ExtOptions {
        use_backup_superblocks: false,
        ..ExtOptions::default()
    };
    assert!(open(s, &no_backup).is_err());
}

#[test]
fn backup_superblock_1k() {
    check_backup_superblock(16 << 20, "1024", 8193 * 1024);
}

#[test]
fn backup_superblock_4k() {
    check_backup_superblock(160 << 20, "4096", 128 << 20);
}

#[test]
fn zeroed_primary_gdt_uses_backup() {
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let expect = standard_tree(&src);
    let img = dir.path().join("gdt.img");
    make_image(&img, 160 << 20, "mkfs.ext4", &["-b", "4096"], Some(&src));
    let want = live_paths(&open_path(&img));
    zero(&img, 4096, 4096);
    let fs = open_path(&img);
    assert!(fs
        .group_descriptor(0)
        .is_some_and(|d| d.valid && d.from_backup));
    assert!(fs.info().warnings.iter().any(|w| w.contains("backup GDT")));
    assert_eq!(live_paths(&fs), want);
    let map = by_path(&entries(&fs));
    for (p, exp) in &expect {
        if let Expect::File(data) = exp {
            assert!(read(&fs, &map[p]).0 == *data, "{p}");
        }
    }
    exercise(&fs);
}

/// Volume byte offset of file byte `off` given the entry's extents.
fn volume_offset(e: &FileEntry, off: u64) -> u64 {
    let mut base = 0u64;
    for r in &e.extents {
        if off < base + r.len() {
            return r.start + (off - base);
        }
        base += r.len();
    }
    panic!("offset {off} not mapped in {}", e.path);
}

#[test]
fn unreadable_sectors_inside_a_file() {
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let expect = standard_tree(&src);
    let img = dir.path().join("bad.img");
    make_image(&img, 16 << 20, "mkfs.ext4", &["-b", "1024"], Some(&src));
    let path = "DCIM/100CANON/IMG_0001.JPG";
    let Expect::File(data) = &expect[path] else {
        unreachable!()
    };
    let e = by_path(&entries(&open_path(&img)))[path].clone();
    // One bad sector in block 5 and one whole bad block at block 10.
    let s1 = volume_offset(&e, 5 * 1024 + 512);
    let s2 = volume_offset(&e, 10 * 1024);
    let bad = RangeSet::from_ranges([Range::with_len(s1, 512), Range::with_len(s2, 1024)]);
    let src: SharedSource = Arc::new(mem_source(&img).with_unreadable(bad));
    let fs = open(src, &ExtOptions::default()).unwrap();
    let e = by_path(&entries(&fs))[path].clone();
    let (bytes, rep) = read(&fs, &e);
    assert_eq!(bytes.len(), data.len());
    let want_bad = RangeSet::from_ranges([
        Range::new(5 * 1024 + 512, 6 * 1024),
        Range::new(10 * 1024, 11 * 1024),
    ]);
    assert_eq!(rep.unreadable, want_bad);
    assert!(!rep.is_complete());
    for r in want_bad.iter() {
        assert!(bytes[r.start as usize..r.end as usize]
            .iter()
            .all(|&b| b == 0));
    }
    let mut patched = data.clone();
    for r in want_bad.iter() {
        patched[r.start as usize..r.end as usize].fill(0);
    }
    assert!(bytes == patched, "good bytes must be intact");
}

#[test]
fn unreadable_inode_table_block() {
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    standard_tree(&src);
    let img = dir.path().join("itab.img");
    make_image(&img, 16 << 20, "mkfs.ext4", &["-b", "1024"], Some(&src));
    let good = open_path(&img);
    let map = by_path(&entries(&good));
    let ino = ExtFs::inode_number(&map["hello.txt"]);
    let sb = good.superblock();
    let d = good
        .group_descriptor((ino - 1) / sb.inodes_per_group)
        .unwrap();
    let idx = ((ino - 1) % sb.inodes_per_group) as u64;
    let off = d.inode_table * 1024 + idx * sb.inode_size as u64;
    let block = off / 1024 * 1024;
    let src: SharedSource = Arc::new(
        mem_source(&img).with_unreadable(RangeSet::from_range(Range::with_len(block, 1024))),
    );
    let fs = open(src, &ExtOptions::default()).unwrap();
    let all = entries(&fs);
    let m = by_path(&all);
    let e = &m["hello.txt"];
    assert!(e.flags.iter().any(|f| f == "inode_unreadable"));
    assert_eq!(e.confidence, EntryConfidence::DeletedNoData);
    let mut sink = Vec::new();
    assert!(fs.read_file(e, &mut sink).is_err());
    // Files whose inodes live elsewhere still read fine.
    assert!(m.contains_key("DCIM/100CANON/IMG_0002.JPG"));
    exercise(&fs);
}

#[test]
fn walk_control_and_max_inodes() {
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    standard_tree(&src);
    let img = dir.path().join("ctl.img");
    make_image(&img, 16 << 20, "mkfs.ext4", &["-b", "1024"], Some(&src));
    let fs = open_path(&img);
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
    assert!(paths.iter().any(|p| p == "DCIM"));
    assert!(!paths.iter().any(|p| p.starts_with("DCIM/")));
    let capped = open(
        file_source(&img),
        &ExtOptions {
            max_inodes: 4,
            ..ExtOptions::default()
        },
    )
    .unwrap();
    assert_eq!(entries(&capped).len(), 4);
}

#[test]
fn random_and_tiny_inputs_never_panic() {
    for (seed, len) in [
        (1u64, 0usize),
        (2, 100),
        (3, 2047),
        (4, 2048),
        (5, 4096),
        (6, 1 << 20),
    ] {
        let src: SharedSource = Arc::new(MemSource::new(noise(seed, len)));
        assert_eq!(probe(&src), None);
        assert!(open(src, &ExtOptions::default()).is_err());
    }
    // A valid magic in garbage is not enough.
    let mut v = noise(9, 64 * 1024);
    v[1024 + 0x38] = 0x53;
    v[1024 + 0x39] = 0xEF;
    let src: SharedSource = Arc::new(MemSource::new(v));
    assert_eq!(probe(&src), None);
    assert!(open(src, &ExtOptions::default()).is_err());
}

fn fixture_image(args: &[&str], mkfs: &str) -> Option<(tempfile::TempDir, Vec<u8>)> {
    if !have(&[mkfs]) {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    standard_tree(&src);
    let img = dir.path().join("fuzz.img");
    make_image(&img, 16 << 20, mkfs, args, Some(&src));
    let bytes = std::fs::read(&img).unwrap();
    Some((dir, bytes))
}

#[test]
fn truncated_images_never_panic() {
    let Some((_d, bytes)) = fixture_image(&["-b", "1024"], "mkfs.ext4") else {
        return;
    };
    let mut opened = 0;
    for cut in [
        1024usize,
        2047,
        2048,
        3000,
        5000,
        64 << 10,
        300 << 10,
        1 << 20,
        8 << 20,
        15 << 20,
    ] {
        let src: SharedSource = Arc::new(MemSource::new(bytes[..cut].to_vec()));
        if let Ok(fs) = open(src, &ExtOptions::default()) {
            assert!(fs.info().warnings.iter().any(|w| w.contains("shorter")));
            exercise(&fs);
            opened += 1;
        }
    }
    assert!(opened >= 6, "only {opened} truncated images opened");
}

/// Corrupt `flips` random bytes in the first `region` bytes, `rounds`
/// times; returns (volumes that opened, entries exercised).
fn corrupt_and_exercise(bytes: &[u8], rounds: u64, region: usize, flips: usize) -> (u64, usize) {
    let (mut opened, mut seen) = (0u64, 0usize);
    for seed in 0..rounds {
        let mut v = bytes.to_vec();
        let r = noise(1000 + seed, flips * 8);
        for k in 0..flips {
            let pos = u32::from_le_bytes(r[k * 8..k * 8 + 4].try_into().unwrap()) as usize % region;
            v[pos] = r[k * 8 + 4];
        }
        // Keep the primary superblock intact half the time so deeper
        // structures get exercised.
        if seed % 2 == 0 {
            v[1024..2048].copy_from_slice(&bytes[1024..2048]);
        }
        let src: SharedSource = Arc::new(MemSource::new(v));
        let _ = probe(&src);
        if let Ok(fs) = open(src, &ExtOptions::default()) {
            opened += 1;
            seen += exercise(&fs);
        }
    }
    (opened, seen)
}

#[test]
fn corrupted_metadata_never_panics_ext4() {
    let Some((_d, bytes)) = fixture_image(&["-b", "1024"], "mkfs.ext4") else {
        return;
    };
    let (opened, seen) = corrupt_and_exercise(&bytes, 24, 1 << 20, 400);
    assert!(
        opened >= 12 && seen > 100,
        "opened {opened}, entries {seen}"
    );
    let (opened, seen) = corrupt_and_exercise(&bytes, 8, 3 << 20, 20_000);
    assert!(opened >= 4 && seen > 0, "opened {opened}, entries {seen}");
}

#[test]
fn corrupted_metadata_never_panics_ext2() {
    let Some((_d, bytes)) = fixture_image(&["-t", "ext2", "-b", "1024"], "mke2fs") else {
        return;
    };
    let (opened, seen) = corrupt_and_exercise(&bytes, 24, 1 << 20, 400);
    assert!(
        opened >= 12 && seen > 100,
        "opened {opened}, entries {seen}"
    );
}

#[test]
fn absurd_geometry_is_bounded() {
    let Some((_d, mut bytes)) = fixture_image(&["-b", "1024", "-O", "^64bit"], "mkfs.ext4") else {
        return;
    };
    // Claim 2^30 blocks of 1 KiB (1 TiB) on a 16 MiB image with consistent
    // per-group counts: 131072 groups of 16 inodes.
    let sb = 1024;
    let groups: u64 = (1u64 << 30) / 8192;
    bytes[sb..sb + 4].copy_from_slice(&((groups * 16) as u32).to_le_bytes());
    bytes[sb + 4..sb + 8].copy_from_slice(&((1u32 << 30) + 1).to_le_bytes());
    bytes[sb + 0x28..sb + 0x2C].copy_from_slice(&16u32.to_le_bytes());
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let start = std::time::Instant::now();
    let fs = open(src, &ExtOptions::default()).expect("consistent geometry opens");
    assert!(fs.info().warnings.iter().any(|w| w.contains("shorter")));
    assert_eq!(fs.info().total_size, ((1u64 << 30) + 1) * 1024);
    exercise(&fs);
    assert!(start.elapsed().as_secs() < 60, "took {:?}", start.elapsed());
}
