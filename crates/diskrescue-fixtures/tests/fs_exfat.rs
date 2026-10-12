#[macro_use]
mod common;

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use common::readers::ExImage;
use common::*;
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::FixtureError;

fn fsck(img: &Path) -> String {
    let (ok, text) = check("fsck.exfat", &["-n", img.to_str().unwrap()]);
    assert!(ok, "fsck.exfat failed:\n{text}");
    assert!(text.contains("clean"), "{text}");
    text
}

fn sample(cluster: Option<u32>) -> FsSpec {
    let mut s = FsSpec::new(FsType::ExFat, 64 << 20);
    s.cluster_size = cluster;
    s.label = Some("PHONE".into());
    s.dirs.push("Empty".into());
    s.files.push(FileSpec::new("IMG_0001.JPG", data(30_000, 1)));
    s.files.push(FileSpec::new(
        "Holiday Photo 2024 with a really long name that needs more name entries.jpg",
        data(9_000, 2),
    ));
    s.files
        .push(FileSpec::new("DCIM/Camera/IMG_0002.jpg", data(70_000, 3)).mtime(1_600_000_000));
    s.files.push(FileSpec::new(
        "Pictures/Fotos-é-日本語-😀.png",
        data(2_500, 4),
    ));
    s.files.push(FileSpec::new("empty.txt", vec![]));
    s.files
        .push(FileSpec::new("DCIM/Camera/VID_0003.mp4", data(300_000, 5)));
    s.files.push(FileSpec::new("deleted.bin", data(40_000, 6)));
    s.files
        .push(FileSpec::new("DCIM/Camera/deleted2.jpg", data(12_345, 7)));
    s.deleted.push("deleted.bin".into());
    s.deleted.push("DCIM/Camera/deleted2.jpg".into());
    s
}

#[test]
fn exfat_volumes_pass_fsck_and_match_the_reader() {
    if skip_unless(&["mkfs.exfat", "fsck.exfat"]) {
        return;
    }
    for cluster in [None, Some(512u32), Some(4096), Some(32768)] {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("ex.img");
        let s = sample(cluster);
        let rep = build(&s, &img).unwrap();
        assert!(rep.deleted_supported);
        if let Some(c) = cluster {
            assert_eq!(rep.cluster_size, c);
        }
        fsck(&img);

        let mut ex = ExImage::open(&img);
        assert_eq!(ex.label.as_deref(), Some("PHONE"));
        let ents = ex.walk();
        assert!(ents
            .iter()
            .any(|e| e.path == "Empty" && e.is_dir && !e.deleted));
        for e in &ents {
            assert!(e.checksum_ok, "{} entry-set checksum", e.path);
            assert!(e.hash_ok, "{} name hash", e.path);
            assert!(e.name_len_ok, "{} name length", e.path);
            assert_eq!(e.size, e.valid_len, "{}", e.path);
        }
        for f in &s.files {
            let want_deleted = s.deleted.contains(&f.path);
            let p = rep.find(&f.path).unwrap();
            assert_eq!(p.deleted, want_deleted);
            assert_bytes!(read_extents(&img, &p.extents), f.data, "extents {}", f.path);
            let e = ents
                .iter()
                .find(|e| e.path == f.path)
                .unwrap_or_else(|| panic!("no entry for {}", f.path));
            assert_eq!(e.deleted, want_deleted, "{}", f.path);
            assert_eq!(e.size as usize, f.data.len());
            assert!(
                e.no_fat_chain || f.data.is_empty(),
                "contiguous files use NoFatChain"
            );
            if f.data.is_empty() {
                assert_eq!(e.first, 0);
                continue;
            }
            // Bitmap: live clusters set, deleted clusters clear.
            let cb = ex.cb();
            let n = (f.data.len() as u64).div_ceil(cb) as u32;
            for c in e.first..e.first + n {
                assert_eq!(ex.bit(c), !want_deleted, "bitmap bit {c} of {}", f.path);
            }
            if !want_deleted {
                assert_bytes!(ex.read_file(e), f.data, "read back {}", f.path);
            }
        }
        // PercentInUse matches the bitmap.
        let used = ex.used_clusters();
        let pct = (used * 100 / ex.clusters as u64) as u8;
        assert_eq!(ex.percent_in_use, pct);
    }
}

#[test]
fn boot_region_checksums_and_backup_are_consistent() {
    if skip_unless(&["mkfs.exfat", "fsck.exfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("b.img");
    build(&sample(None), &img).unwrap();
    let raw = std::fs::read(&img).unwrap();
    let region = &raw[..512 * 24];
    // Main and backup regions are identical.
    assert_eq!(&region[..512 * 12], &region[512 * 12..]);
    // Re-derive the checksum independently.
    let mut sum = 0u32;
    for (i, &b) in region[..512 * 11].iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        sum = sum.rotate_right(1).wrapping_add(b as u32);
    }
    for chunk in region[512 * 11..512 * 12].chunks(4) {
        assert_eq!(u32::from_le_bytes(chunk.try_into().unwrap()), sum);
    }
    // The serial number is pinned.
    assert_eq!(&region[100..104], &0x4452_4558u32.to_le_bytes());
}

#[test]
fn root_directory_grows_and_fragments_use_fat_chains() {
    if skip_unless(&["mkfs.exfat", "fsck.exfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("g.img");
    let a = data(40_000, 1);
    let b = data(8_000, 2);
    let mut s = FsSpec::new(FsType::ExFat, 64 << 20).cluster_size(4096);
    for i in 0..300 {
        s.files.push(FileSpec::new(
            format!("file number {i:04}.dat"),
            data(10, i),
        ));
    }
    s.files
        .push(FileSpec::new("A.BIN", a.clone()).fragmented(5, 2));
    s.files.push(FileSpec::new("B.BIN", b.clone()));
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    let mut ex = ExImage::open(&img);
    let ents = ex.walk();
    assert_eq!(ents.len(), 302);
    let root_chain_len = {
        let mut n = 0;
        let mut c = ex.root;
        while c >= 2 && c < ex.clusters + 2 && n < 1000 {
            n += 1;
            c = ex.fat[c as usize];
        }
        n
    };
    assert!(root_chain_len > 5, "root grew to {root_chain_len} clusters");
    let ea = ents.iter().find(|e| e.path == "A.BIN").unwrap();
    assert!(!ea.no_fat_chain);
    assert_bytes!(ex.read_file(ea), a);
    let pa = rep.find("A.BIN").unwrap();
    assert_eq!(pa.extents.len(), 2);
    let pb = rep.find("B.BIN").unwrap();
    assert_eq!(pb.extents, vec![(pa.extents[0].0 + 5 * 4096, 8000)]);
    assert_bytes!(read_extents(&img, &pb.extents), b);
}

#[test]
fn deleted_directories_and_bad_specs() {
    if skip_unless(&["mkfs.exfat", "fsck.exfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("d.img");
    let mut s = FsSpec::new(FsType::ExFat, 64 << 20);
    s.files.push(FileSpec::new("keep/one.bin", data(5000, 1)));
    s.files
        .push(FileSpec::new("gone/inner/two.bin", data(6000, 2)));
    s.files.push(FileSpec::new("gone/three.bin", data(7000, 3)));
    s.deleted.push("gone".into());
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    let mut ex = ExImage::open(&img);
    let ents = ex.walk();
    assert!(ents.iter().any(|e| e.path == "keep/one.bin" && !e.deleted));
    assert!(ents
        .iter()
        .any(|e| e.path == "gone" && e.deleted && e.is_dir));
    for f in &s.files {
        assert_eq!(
            read_extents(&img, &rep.find(&f.path).unwrap().extents),
            f.data
        );
    }

    let fails = |s: FsSpec| build(&s, &dir.path().join("x.img")).unwrap_err();
    let mut bad = FsSpec::new(FsType::ExFat, 64 << 20);
    bad.files.push(FileSpec::new("bad:name", vec![1]));
    assert!(matches!(fails(bad), FixtureError::Invalid(_)));
    let mut bad = FsSpec::new(FsType::ExFat, 64 << 20);
    bad.label = Some("a label that is far too long".into());
    assert!(matches!(fails(bad), FixtureError::Invalid(_)));
    let mut bad = FsSpec::new(FsType::ExFat, 16 << 20);
    bad.files
        .push(FileSpec::new("big.bin", vec![1u8; 20 << 20]));
    assert!(matches!(fails(bad), FixtureError::Full(_)));
    let mut bad = FsSpec::new(FsType::ExFat, 64 << 20).cluster_size(1000);
    bad.files.push(FileSpec::new("x", vec![1]));
    assert!(matches!(fails(bad), FixtureError::Invalid(_)));
    let mut bad = FsSpec::new(FsType::ExFat, 64 << 20);
    bad.files.push(FileSpec::new("dup", vec![]));
    bad.files.push(FileSpec::new("DUP", vec![]));
    assert!(matches!(fails(bad), FixtureError::Invalid(_)));
}

#[test]
fn oracle_detects_a_wrong_name_hash_and_checksum() {
    // fsck.exfat verifies entry-set checksums and name hashes; flipping them
    // must make it complain, so the clean verdicts above mean something.
    if skip_unless(&["mkfs.exfat", "fsck.exfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("o.img");
    let mut s = FsSpec::new(FsType::ExFat, 64 << 20);
    s.files.push(FileSpec::new("victim.bin", data(5000, 1)));
    build(&s, &img).unwrap();
    fsck(&img);
    // Find the file entry in the root directory cluster and break the hash.
    let mut ex = ExImage::open(&img);
    let root_off = ex.cluster_off(ex.root);
    let raw = read_extents(&img, &[(root_off, 4096)]);
    let at = raw
        .chunks(32)
        .position(|e| e[0] == 0x85)
        .expect("file entry");
    let stream_hash_at = root_off + (at as u64 + 1) * 32 + 4;
    let _ = ex.walk();
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.seek(SeekFrom::Start(stream_hash_at)).unwrap();
    f.write_all(&[0x12, 0x34]).unwrap();
    drop(f);
    let (ok, text) = check("fsck.exfat", &["-n", img.to_str().unwrap()]);
    assert!(
        !ok || !text.contains("clean"),
        "damaged entry set must not pass:\n{text}"
    );
    assert!(Path::new(&img).exists());
}
