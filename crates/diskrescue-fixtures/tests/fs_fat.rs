#[macro_use]
mod common;

use std::path::Path;

use common::readers::FatImage;
use common::*;
use diskrescue_fixtures::fs::{build, BuildReport, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::FixtureError;

const LONG: &str = "DCIM/100CANON/a long file name with many characters in it.txt";

fn sample(kind: FsType, size: u64, native: bool) -> FsSpec {
    let mut s = FsSpec::new(kind, size);
    s.native_format = native;
    s.label = Some("FIXTURE".into());
    s.dirs.push("EMPTYDIR".into());
    s.files.push(FileSpec::new("IMG_0001.JPG", data(30_000, 1)));
    s.files
        .push(FileSpec::new("Holiday Photo 2024.jpg", data(9_000, 2)));
    s.files
        .push(FileSpec::new("DCIM/100CANON/IMG_0002.JPG", data(70_000, 3)).mtime(1_600_000_000));
    s.files.push(FileSpec::new(LONG, data(100, 4)));
    s.files.push(FileSpec::new("lower.txt", data(0, 5)));
    s.files
        .push(FileSpec::new("DCIM/MVI_0003.MP4", data(200_000, 6)));
    s.files
        .push(FileSpec::new("deleted_me.bin", data(40_000, 7)));
    s.files
        .push(FileSpec::new("Ünïcödé-日本語.txt", data(1234, 8)));
    s.deleted.push("deleted_me.bin".into());
    s.deleted.push(LONG.into());
    s
}

fn fsck(img: &Path) -> String {
    let (ok, text) = check("fsck.vfat", &["-n", "-l", img.to_str().unwrap()]);
    assert!(ok, "fsck.vfat failed:\n{text}");
    assert!(!text.contains("Dirty bit"), "{text}");
    text
}

/// Build, run fsck, compare against the independent reader and the report.
fn verify(kind: FsType, size: u64, native: bool) -> (tempfile::TempDir, BuildReport) {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("fat.img");
    let s = sample(kind, size, native);
    let rep = build(&s, &img).unwrap();
    assert!(rep.deleted_supported);
    let listing = fsck(&img);

    let mut fi = FatImage::open(&img);
    let ents = fi.walk();
    assert_eq!(fi.label.as_deref(), Some("FIXTURE"));
    assert!(ents.iter().any(|e| e.path == "EMPTYDIR" && e.is_dir));
    for f in &s.files {
        let want_deleted = s.deleted.contains(&f.path);
        let p = rep.find(&f.path).unwrap();
        assert_eq!(p.deleted, want_deleted);
        // The data sits at the recorded extents, deleted or not.
        assert_eq!(
            read_extents(&img, &p.extents),
            f.data,
            "extents of {}",
            f.path
        );

        // Directory entry: deleted names without LFN lose their first letter.
        let mut ent = ents.iter().find(|e| e.path == f.path);
        if ent.is_none() && want_deleted {
            let (dir_part, name) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
            let mut n = name.chars();
            n.next();
            let q = if dir_part.is_empty() {
                format!("?{}", n.as_str())
            } else {
                format!("{dir_part}/?{}", n.as_str())
            };
            ent = ents.iter().find(|e| e.path == q);
        }
        let ent = ent.unwrap_or_else(|| panic!("no entry for {}", f.path));
        assert_eq!(ent.deleted, want_deleted, "{}", f.path);
        assert_eq!(ent.size as usize, f.data.len(), "{}", f.path);
        if let Some(ok) = ent.lfn_ok {
            assert!(ok, "LFN checksum for {}", f.path);
        }
        let shown = f.path.rsplit('/').next().unwrap();
        if want_deleted {
            assert_eq!(ent.short[0], 0xE5);
            if f.data.is_empty() {
                continue;
            }
            assert_eq!(fi.fat[ent.first as usize], 0, "chain of {} freed", f.path);
            assert!(
                !listing.contains(shown),
                "deleted {shown} must not be listed"
            );
        } else {
            if !f.data.is_empty() {
                assert_eq!(
                    fi.read_file(ent.first, ent.size),
                    f.data,
                    "chain of {}",
                    f.path
                );
            }
            // fsck prints the stored short name, ignoring the NT case bits.
            assert!(
                listing.to_lowercase().contains(&shown.to_lowercase()),
                "fsck listing lacks {shown}:\n{listing}"
            );
        }
    }
    (dir, rep)
}

#[test]
fn fat12_tool_and_native() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    verify(FsType::Fat12, 8 << 20, false);
    verify(FsType::Fat12, 8 << 20, true);
}

#[test]
fn fat16_tool_and_native() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    verify(FsType::Fat16, 40 << 20, false);
    verify(FsType::Fat16, 40 << 20, true);
}

#[test]
fn fat32_tool_and_native() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    let (_d, rep) = verify(FsType::Fat32, 96 << 20, false);
    assert_eq!(rep.cluster_size, 1024);
    assert_eq!(rep.tool, "mkfs.vfat+builtin");
    let (_d, rep) = verify(FsType::Fat32, 96 << 20, true);
    assert_eq!(rep.tool, "builtin");
}

#[test]
fn explicit_cluster_sizes_and_alignment() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    for (kind, size, cs) in [
        (FsType::Fat16, 64u64 << 20, 4096u32),
        (FsType::Fat16, 100 << 20, 8192),
        (FsType::Fat32, 272 << 20, 4096),
        (FsType::Fat32, 64 << 20, 512),
        (FsType::Fat12, 4 << 20, 1024),
    ] {
        for native in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let img = dir.path().join("a.img");
            let mut s = FsSpec::new(kind, size).cluster_size(cs);
            s.native_format = native;
            s.files.push(FileSpec::new("A/B/FILE.BIN", data(50_000, 9)));
            let rep = build(&s, &img).unwrap();
            assert_eq!(rep.cluster_size, cs, "{kind:?} {cs}");
            fsck(&img);
            let p = rep.find("A/B/FILE.BIN").unwrap();
            assert_bytes!(read_extents(&img, &p.extents), data(50_000, 9));
            if native {
                // The built-in formatter aligns the data area to the cluster
                // size, so file starts are cluster-aligned (matters for
                // carving at the default 4 KiB alignment).
                assert_eq!(rep.data_start % cs as u64, 0, "{kind:?} {cs}");
                assert_eq!(p.extents[0].0 % cs as u64, 0);
            }
        }
    }
}

#[test]
fn deleted_directory_keeps_its_entries() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("d.img");
    let mut s = FsSpec::new(FsType::Fat32, 64 << 20);
    s.files.push(FileSpec::new("keep/one.bin", data(5000, 1)));
    s.files
        .push(FileSpec::new("gone/inner/two.bin", data(6000, 2)));
    s.files.push(FileSpec::new("gone/three.bin", data(7000, 3)));
    s.deleted.push("gone".into());
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    assert!(rep.find("gone/inner/two.bin").unwrap().deleted);
    assert!(rep.find("gone/three.bin").unwrap().deleted);
    assert!(!rep.find("keep/one.bin").unwrap().deleted);
    let mut fi = FatImage::open(&img);
    let ents = fi.walk();
    assert!(ents.iter().any(|e| e.path == "keep/one.bin" && !e.deleted));
    let gone = ents
        .iter()
        .find(|e| e.is_dir && e.deleted)
        .expect("deleted dir entry");
    assert_eq!(gone.short[0], 0xE5);
    assert_eq!(fi.fat[gone.first as usize], 0, "directory cluster freed");
    // The freed cluster still holds "." / ".." and the children's entries
    // (one subdirectory entry plus a file whose name lost its first letter).
    let raw = read_extents(&img, &[(fi.cluster_off(gone.first), 512)]);
    assert_eq!(&raw[0..2], b". ");
    assert_eq!(&raw[32..34], b"..");
    let names: Vec<&[u8]> = raw.chunks(32).skip(2).take(2).map(|e| &e[..11]).collect();
    assert!(names.iter().any(|n| n[0] == 0xE5 || n[0] == b'I'));
    for f in &s.files {
        let p = rep.find(&f.path).unwrap();
        assert_bytes!(read_extents(&img, &p.extents), f.data);
    }
}

#[test]
fn windows_style_delete_zeroes_the_high_cluster_word() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    // A 34 MB filler on 512-byte clusters pushes the next file past cluster 65535.
    for windows in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("w.img");
        let mut s = FsSpec::new(FsType::Fat32, 40 << 20).cluster_size(512);
        s.windows_delete = windows;
        s.files
            .push(FileSpec::new("filler.bin", vec![7u8; 34 << 20]));
        s.files.push(FileSpec::new("victim.bin", data(10_000, 4)));
        s.deleted.push("victim.bin".into());
        let rep = build(&s, &img).unwrap();
        fsck(&img);
        let mut fi = FatImage::open(&img);
        let ents = fi.walk();
        let v = ents.iter().find(|e| e.deleted).unwrap();
        let real_first = (rep.find("victim.bin").unwrap().extents[0].0 - rep.data_start) / 512 + 2;
        assert!(real_first > 65535, "test setup: first cluster {real_first}");
        if windows {
            assert_eq!(v.first_hi, 0);
            assert_eq!(v.first as u64, real_first & 0xFFFF);
        } else {
            assert_eq!(v.first as u64, real_first);
        }
    }
}

#[test]
fn fragmented_placement_and_gap_filling() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("f.img");
    let a = data(40_000, 1); // 10 clusters of 4 KiB
    let b = data(8_000, 2); // 2 clusters
    let mut s = FsSpec::new(FsType::Fat32, 272 << 20).cluster_size(4096);
    s.files
        .push(FileSpec::new("A.BIN", a.clone()).fragmented(5, 2));
    s.files.push(FileSpec::new("B.BIN", b.clone()));
    s.files.push(FileSpec::new("C.BIN", data(4096, 3)));
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    let pa = rep.find("A.BIN").unwrap();
    let pb = rep.find("B.BIN").unwrap();
    assert_eq!(pa.extents.len(), 2, "{:?}", pa.extents);
    assert_eq!(pa.extents[0].1, 5 * 4096);
    // B sits exactly in the gap between the two fragments.
    assert_eq!(pb.extents, vec![(pa.extents[0].0 + 5 * 4096, 8000)]);
    assert_eq!(pa.extents[1].0, pa.extents[0].0 + 7 * 4096);
    assert_bytes!(read_extents(&img, &pa.extents), a);
    assert_bytes!(read_extents(&img, &pb.extents), b);
    // The live fragmented file reads back through the FAT chain.
    let mut fi = FatImage::open(&img);
    let e = fi.walk().into_iter().find(|e| e.path == "A.BIN").unwrap();
    assert_bytes!(fi.read_file(e.first, e.size), a);
}

#[test]
fn root_directory_growth_and_limits() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    // The FAT32 root grows over many clusters.
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("g.img");
    let mut s = FsSpec::new(FsType::Fat32, 64 << 20).cluster_size(512);
    for i in 0..200 {
        s.files.push(FileSpec::new(
            format!("photo number {i:04} with long name.jpg"),
            data(100, i),
        ));
    }
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    let mut fi = FatImage::open(&img);
    assert_eq!(fi.walk().len(), 200);
    assert!(fi.chain(fi.root_cluster).len() > 10, "root chain length");
    assert_eq!(rep.placed.len(), 200);

    // FAT16 has a fixed root of 512 entries: 300 long names overflow it...
    let mut s = FsSpec::new(FsType::Fat16, 40 << 20);
    for i in 0..300 {
        s.files.push(FileSpec::new(
            format!("a long name for file {i:04}.dat"),
            vec![],
        ));
    }
    match build(&s, &dir.path().join("h.img")) {
        Err(FixtureError::Full(_)) => {}
        other => panic!("expected Full, got {other:?}"),
    }
    // ...but fit into a subdirectory.
    let mut s = FsSpec::new(FsType::Fat16, 40 << 20);
    for i in 0..300 {
        s.files.push(FileSpec::new(
            format!("sub/a long name for file {i:04}.dat"),
            vec![],
        ));
    }
    build(&s, &dir.path().join("i.img")).unwrap();
    fsck(&dir.path().join("i.img"));
}

#[test]
fn full_volume_and_bad_specs() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("e.img");
    let fails = |s: FsSpec| -> FixtureError { build(&s, &img).unwrap_err() };
    let mut s = FsSpec::new(FsType::Fat16, 20 << 20);
    s.files.push(FileSpec::new("big.bin", vec![1u8; 25 << 20]));
    assert!(matches!(fails(s), FixtureError::Full(_)));
    assert!(matches!(
        fails(FsSpec::new(FsType::Fat32, 0)),
        FixtureError::Invalid(_)
    ));
    assert!(matches!(
        fails(FsSpec::new(FsType::Fat32, 1000)),
        FixtureError::Invalid(_)
    ));
    assert!(matches!(
        fails(FsSpec::new(FsType::Fat32, 64 << 20).cluster_size(1000)),
        FixtureError::Invalid(_)
    ));
    assert!(matches!(
        fails(FsSpec::new(FsType::Fat32, 1 << 20)),
        FixtureError::Invalid(_)
    ));
    // Asking for FAT16 on a volume whose cluster count is FAT32-sized.
    assert!(build(&FsSpec::new(FsType::Fat16, 1 << 30).cluster_size(512), &img).is_err());
    for bad in [
        "a/b:c.txt",
        "q?.txt",
        "trailing.",
        "x\ty.txt",
        "ok/../no.txt",
    ] {
        let mut s = FsSpec::new(FsType::Fat32, 64 << 20);
        s.files.push(FileSpec::new(bad, vec![1]));
        assert!(
            matches!(fails(s), FixtureError::Invalid(_)),
            "{bad:?} must be rejected"
        );
    }
    let mut s = FsSpec::new(FsType::Fat32, 64 << 20);
    s.label = Some("THIS LABEL IS WAY TOO LONG".into());
    assert!(matches!(fails(s), FixtureError::Invalid(_)));
    let mut s = FsSpec::new(FsType::Fat32, 64 << 20);
    s.files.push(FileSpec::new("a.txt", vec![]));
    s.files.push(FileSpec::new("A.TXT", vec![]));
    assert!(matches!(fails(s), FixtureError::Invalid(_)));
    let mut s = FsSpec::new(FsType::Fat32, 64 << 20);
    s.deleted.push("nothing".into());
    assert!(matches!(fails(s), FixtureError::Invalid(_)));
}

#[test]
fn struct_literal_construction_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("lit.img");
    let spec = FsSpec {
        kind: FsType::Fat16,
        size: 20 << 20,
        files: vec![FileSpec {
            path: "A.TXT".into(),
            data: vec![1, 2, 3],
            mtime: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let rep = build(&spec, &img).unwrap();
    assert_eq!(rep.placed.len(), 1);
    assert_eq!(rep.placed[0].size, 3);
}

#[test]
fn oracle_detects_a_broken_chain() {
    // Proves the fsck checks above are not vacuous: freeing the first
    // cluster of a file in the FAT makes fsck.vfat fail.
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("o.img");
    let mut s = FsSpec::new(FsType::Fat16, 40 << 20);
    s.files.push(FileSpec::new("X.BIN", data(20_000, 1)));
    let rep = build(&s, &img).unwrap();
    fsck(&img);
    let fi = FatImage::open(&img);
    let first = (rep.find("X.BIN").unwrap().extents[0].0 - fi.data_off) / fi.cluster_bytes() + 2;
    let boot = std::fs::read(&img).unwrap();
    let rsvd = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.seek(SeekFrom::Start(rsvd * 512 + first * 2)).unwrap();
    f.write_all(&[0x00, 0x00]).unwrap();
    drop(f);
    let (ok, _) = check("fsck.vfat", &["-n", img.to_str().unwrap()]);
    assert!(!ok, "fsck must flag the broken chain");
}
