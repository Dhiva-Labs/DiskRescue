//! Real images built with mke2fs: every feature combination must list the
//! exact tree that was put in, read every file back byte for byte, and
//! report the same free space as dumpe2fs.

mod common;

use std::collections::BTreeSet;

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileSystem, FsKind};
use diskrescue_core::{Range, RangeSet};
use diskrescue_ext4::{open, probe, ExtFs, ExtOptions};

struct Variant {
    mkfs: &'static str,
    size: u64,
    args: &'static [&'static str],
    kind: FsKind,
    block_size: u32,
    /// mke2fs with inline_data records a sparse file's size only up to its
    /// last data block (an e2fsprogs quirk, confirmed with debugfs stat).
    sparse_tail_lost: bool,
}

fn check_unallocated(fs: &ExtFs, img: &std::path::Path) {
    let (ranges, sb_free) = dumpe2fs_free_blocks(img);
    let bs = fs.block_size() as u64;
    let expected: RangeSet = ranges
        .iter()
        .map(|&(a, b)| Range::new(a * bs, b * bs))
        .collect();
    let got = fs.unallocated().unwrap();
    assert_eq!(
        got.total_len() / bs,
        sb_free,
        "free block count vs superblock"
    );
    assert_eq!(got, expected, "free ranges vs dumpe2fs");
    // allocated() is the complement inside the volume.
    let alloc = fs.allocated().unwrap();
    assert_eq!(alloc.total_len() + got.total_len(), fs.info().total_size);
}

fn check_variant(v: &Variant) {
    if !have(&[v.mkfs, "dumpe2fs"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let expect = standard_tree(&src);
    let img = dir.path().join("vol.img");
    let mut args: Vec<&str> = v.args.to_vec();
    args.extend(["-L", "TESTVOL"]);
    make_image(&img, v.size, v.mkfs, &args, Some(&src));

    let s = file_source(&img);
    assert_eq!(probe(&s), Some(v.kind), "{:?}", v.args);
    let fs = open(s, &ExtOptions::default()).unwrap();
    let info = fs.info();
    assert_eq!(info.kind, v.kind);
    assert_eq!(info.label.as_deref(), Some("TESTVOL"));
    assert_eq!(info.cluster_size, v.block_size);
    assert_eq!(
        info.total_size,
        fs.superblock().blocks_count * v.block_size as u64
    );
    assert!(info.serial.as_ref().is_some_and(|s| s.len() == 36));
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);

    let all = entries(&fs);
    let map = by_path(&all);
    assert_eq!(map.len(), all.len(), "paths unique");
    let ids: BTreeSet<u64> = all.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), all.len(), "ids unique");
    // Parents are reported before their children.
    let mut seen = BTreeSet::new();
    for e in &all {
        if let Some((parent, _)) = e.path.rsplit_once('/') {
            assert!(seen.contains(parent), "{} before its parent", e.path);
        }
        seen.insert(e.path.clone());
        assert!(!e.deleted, "unexpected deleted entry {}", e.path);
        assert_eq!(e.confidence, EntryConfidence::Allocated);
    }
    let got: BTreeSet<String> = all
        .iter()
        .map(|e| e.path.clone())
        .filter(|p| p != "lost+found")
        .collect();
    let want: BTreeSet<String> = expect.keys().cloned().collect();
    assert_eq!(got, want);

    for (path, exp) in &expect {
        let e = &map[path];
        assert!(e.modified.is_some_and(|t| t.is_plausible()), "{path}");
        match exp {
            Expect::Dir => assert!(e.is_dir, "{path}"),
            Expect::File(data) => {
                assert!(!e.is_dir);
                assert_eq!(e.size, data.len() as u64, "{path}");
                let (bytes, rep) = read(&fs, e);
                assert!(bytes == *data, "{path}: content differs");
                assert!(rep.unreadable.is_empty() && rep.holes.is_empty(), "{path}");
                assert!(rep.is_complete());
                if !e.resident {
                    assert_eq!(e.extent_bytes(), data.len() as u64, "{path} extents");
                }
            }
            Expect::Symlink(target) => {
                assert!(e.flags.iter().any(|f| f == "symlink"), "{path}");
                let (bytes, _) = read(&fs, e);
                assert_eq!(bytes, target.as_bytes(), "{path}");
                assert_eq!(
                    e.resident,
                    target.len() < 60 || e.flags.iter().any(|f| f == "inline")
                );
            }
            Expect::Sparse(data) => {
                let (bytes, rep) = read(&fs, e);
                if v.sparse_tail_lost {
                    assert!(
                        e.size < data.len() as u64 && e.size > 1_840_000,
                        "{}",
                        e.size
                    );
                    assert!(
                        bytes[..] == data[..e.size as usize],
                        "{path}: content differs"
                    );
                } else {
                    assert!(bytes == *data, "{path}: content differs");
                }
                assert!(!rep.holes.is_empty(), "{path}: holes not reported");
                assert!(rep.unreadable.is_empty());
                assert!(e.flags.iter().any(|f| f == "sparse"), "{:?}", e.flags);
                assert!(e.extent_bytes() < data.len() as u64);
            }
        }
    }
    // The helper API agrees with the walk.
    let big = &map["DCIM/100CANON/IMG_0002.JPG"];
    let inode = fs.inode(ExtFs::inode_number(big)).unwrap();
    assert_eq!(inode.size, big.size);
    let runs = fs.extents_of(&inode);
    let mapped: u64 = runs.iter().map(|r| r.len).sum();
    assert_eq!(mapped, big.size.div_ceil(v.block_size as u64));
    assert!(fs.group_descriptor(0).is_some_and(|d| d.valid));
    assert!(fs.group_descriptor(fs.group_count()).is_none());

    check_unallocated(&fs, &img);

    // The journal is allocated space, reported separately for carving.
    let journal = fs.journal_ranges();
    if fs
        .superblock()
        .has_compat(diskrescue_ext4::features::COMPAT_HAS_JOURNAL)
    {
        assert!(!journal.is_empty());
        let free = fs.unallocated().unwrap();
        assert!(journal.iter().all(|r| !free.intersects(*r)));
        assert!(journal.iter().map(|r| r.len()).sum::<u64>() >= 1 << 20);
    } else {
        assert!(journal.is_empty());
    }
}

#[test]
fn ext4_4k_default_64bit() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 64 << 20,
        args: &["-b", "4096"],
        kind: FsKind::Ext4,
        block_size: 4096,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_1k_blocks() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 16 << 20,
        args: &["-b", "1024"],
        kind: FsKind::Ext4,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_without_64bit() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 32 << 20,
        args: &["-b", "4096", "-O", "^64bit"],
        kind: FsKind::Ext4,
        block_size: 4096,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_block_mapped_without_extents() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 32 << 20,
        args: &["-b", "1024", "-O", "^extents,^64bit"],
        kind: FsKind::Ext4,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_inline_data() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 16 << 20,
        args: &["-b", "4096", "-O", "inline_data"],
        kind: FsKind::Ext4,
        block_size: 4096,
        sparse_tail_lost: true,
    });
    // Separately check that small files really are inline.
    if !have(&["mkfs.ext4"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    standard_tree(&src);
    std::fs::write(src.join("hundred.bin"), noise(77, 100)).unwrap();
    let img = dir.path().join("inline.img");
    make_image(
        &img,
        16 << 20,
        "mkfs.ext4",
        &["-O", "inline_data"],
        Some(&src),
    );
    let fs = open_path(&img);
    let map = by_path(&entries(&fs));
    for p in ["hello.txt", "hundred.bin", "small/tiny.txt"] {
        let e = &map[p];
        assert!(
            e.resident && e.flags.iter().any(|f| f == "inline"),
            "{p}: {:?}",
            e.flags
        );
        assert!(e.extents.is_empty());
    }
    let (bytes, _) = read(&fs, &map["hundred.bin"]);
    assert_eq!(bytes, noise(77, 100));
    // Inline directory contents are listed.
    let small = &map["small"];
    let inode = fs.inode(ExtFs::inode_number(small)).unwrap();
    assert!(inode.has_inline_data());
    assert!(map.contains_key("small/tiny.txt"));
}

#[test]
fn ext2_block_maps_1k() {
    check_variant(&Variant {
        mkfs: "mke2fs",
        size: 16 << 20,
        args: &["-t", "ext2", "-b", "1024"],
        kind: FsKind::Ext2,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext3_with_journal() {
    check_variant(&Variant {
        mkfs: "mke2fs",
        size: 32 << 20,
        args: &["-t", "ext3", "-b", "4096"],
        kind: FsKind::Ext3,
        block_size: 4096,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_meta_bg_layout() {
    check_variant(&Variant {
        mkfs: "mke2fs",
        size: 64 << 20,
        args: &["-t", "ext4", "-b", "1024", "-O", "meta_bg,^resize_inode"],
        kind: FsKind::Ext4,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}

#[test]
fn htree_directory_lists_linearly() {
    if !have(&["mkfs.ext4", "e2fsck", "debugfs"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("many")).unwrap();
    for i in 0..1500 {
        std::fs::write(
            src.join(format!("many/file_{i:04}.txt")),
            format!("file {i}\n"),
        )
        .unwrap();
    }
    let img = dir.path().join("htree.img");
    make_image(&img, 32 << 20, "mkfs.ext4", &["-b", "1024"], Some(&src));
    // e2fsck -D rebuilds large directories as hash trees.
    run("e2fsck", &["-fyD", &img.to_string_lossy()]);
    let fs = open_path(&img);
    let all = entries(&fs);
    let map = by_path(&all);
    let many = fs.inode(ExtFs::inode_number(&map["many"])).unwrap();
    assert!(many.is_htree(), "e2fsck -D did not build an htree");
    let kids: Vec<_> = all.iter().filter(|e| e.path.starts_with("many/")).collect();
    assert_eq!(kids.len(), 1500);
    assert!(kids.iter().all(|e| !e.deleted));
    for i in [0, 777, 1499] {
        let (b, _) = read(&fs, &map[&format!("many/file_{i:04}.txt")]);
        assert_eq!(b, format!("file {i}\n").as_bytes());
    }
    // Deleting inside the htree: names come back from the leaf slack.
    let p = img.to_string_lossy().into_owned();
    run("debugfs", &["-w", "-R", "rm /many/file_0007.txt", &p]);
    run("debugfs", &["-w", "-R", "rm /many/file_1234.txt", &p]);
    let fs = open_path(&img);
    let all = entries(&fs);
    let deleted: BTreeSet<String> = all
        .iter()
        .filter(|e| e.deleted)
        .map(|e| e.path.clone())
        .collect();
    let want: BTreeSet<String> = ["many/file_0007.txt", "many/file_1234.txt"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(deleted, want);
    let map = by_path(&all);
    let (b, _) = read(&fs, &map["many/file_1234.txt"]);
    assert_eq!(b, b"file 1234\n");
}

#[test]
fn ext4_gdt_csum_crc16() {
    // uninit_bg without metadata_csum: crc16 descriptor checksums. A wrong
    // checksum implementation would surface as a warning (and untrusted
    // UNINIT flags would change the free-space result).
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 64 << 20,
        args: &["-b", "1024", "-O", "^metadata_csum,uninit_bg"],
        kind: FsKind::Ext4,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}

#[test]
fn ext4_sparse_super2() {
    check_variant(&Variant {
        mkfs: "mkfs.ext4",
        size: 64 << 20,
        args: &["-b", "1024", "-O", "sparse_super2"],
        kind: FsKind::Ext4,
        block_size: 1024,
        sparse_tail_lost: false,
    });
}
