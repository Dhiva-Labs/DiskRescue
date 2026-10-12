#[macro_use]
mod common;

use common::*;
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::tools;

fn spec(block: u32) -> FsSpec {
    let mut s = FsSpec::new(FsType::Ext4, 32 << 20).cluster_size(block);
    s.label = Some("LINUXDISK".into());
    s.dirs.push("empty".into());
    s.files.push(FileSpec::new("IMG_0001.JPG", data(30_000, 1)));
    s.files
        .push(FileSpec::new("Holiday Photo 2024.jpg", data(9_000, 2)).mtime(1_700_000_000));
    s.files
        .push(FileSpec::new("Pictures/2023/IMG_0002.jpg", data(70_000, 3)));
    s.files.push(FileSpec::new("tiny.txt", b"hello".to_vec()));
    s.files.push(FileSpec::new("zero.bin", vec![]));
    s.files
        .push(FileSpec::new("Videos/clip.mp4", data(300_000, 4)));
    s.files.push(FileSpec::new("gone.jpg", data(50_000, 5)));
    s.files
        .push(FileSpec::new("dir_gone/a.bin", data(20_000, 6)));
    s.files
        .push(FileSpec::new("dir_gone/b.bin", data(3_000, 7)));
    s.deleted.push("gone.jpg".into());
    s.deleted.push("dir_gone".into());
    s
}

#[test]
fn ext4_volumes_are_clean_and_extents_match() {
    if skip_unless(&["mkfs.ext4", "debugfs", "e2fsck", "dumpe2fs"]) {
        return;
    }
    for block in [1024u32, 2048, 4096] {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("ext4.img");
        let s = spec(block);
        let rep = build(&s, &img).unwrap();
        assert!(rep.deleted_supported);
        assert_eq!(rep.cluster_size, block);
        let (ok, text) = check("e2fsck", &["-fn", img.to_str().unwrap()]);
        assert!(ok, "e2fsck block={block}:\n{text}");
        let (_, sb) = check("dumpe2fs", &["-h", img.to_str().unwrap()]);
        assert!(sb.contains("LINUXDISK"), "{sb}");
        assert!(
            sb.contains(&format!("Block size:               {block}")),
            "{sb}"
        );

        for f in &s.files {
            let p = rep.find(&f.path).unwrap();
            let want_deleted = s.deleted.contains(&f.path) || f.path.starts_with("dir_gone/");
            assert_eq!(p.deleted, want_deleted, "{}", f.path);
            if f.data.is_empty() {
                assert!(p.extents.is_empty());
            } else {
                assert_eq!(
                    read_extents(&img, &p.extents),
                    f.data,
                    "{} block={block}",
                    f.path
                );
            }
            let (_, stat) = check(
                "debugfs",
                &[
                    "-R",
                    &format!("stat \"/{}\"", f.path),
                    img.to_str().unwrap(),
                ],
            );
            if want_deleted {
                assert!(
                    stat.contains("File not found") || stat.contains("not found"),
                    "{} should be unlinked:\n{stat}",
                    f.path
                );
            } else {
                assert!(stat.contains(&format!("Size: {}", f.data.len())), "{stat}");
            }
        }
        // debugfs rm frees the inodes; lsdel sees both deleted regular files
        // with their data blocks still referenced.
        let (_, lsdel) = check("debugfs", &["-R", "lsdel", img.to_str().unwrap()]);
        assert!(lsdel.contains("deleted inodes found"), "{lsdel}");
        let found: u32 = lsdel
            .lines()
            .find(|l| l.contains("deleted inodes found"))
            .and_then(|l| l.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        assert_eq!(found, 4, "gone.jpg, dir_gone and its two files:\n{lsdel}");
        // Live content equals the source tree (read back through debugfs).
        let live = s
            .files
            .iter()
            .find(|f| f.path == "Pictures/2023/IMG_0002.jpg")
            .unwrap();
        let out = tools::run(
            "debugfs",
            &[
                "-R",
                "cat \"/Pictures/2023/IMG_0002.jpg\"",
                img.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert_bytes!(out.stdout, live.data);
        // mtime is applied.
        let (_, stat) = check(
            "debugfs",
            &[
                "-R",
                "stat \"/Holiday Photo 2024.jpg\"",
                img.to_str().unwrap(),
            ],
        );
        assert!(stat.contains("mtime: 0x6553"), "{stat}");
    }
}

#[test]
fn ext4_bad_specs_and_determinism() {
    if skip_unless(&["mkfs.ext4", "debugfs", "dumpe2fs"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("x.img");
    let mut s = FsSpec::new(FsType::Ext4, 32 << 20).cluster_size(3000);
    s.files.push(FileSpec::new("a", vec![1]));
    assert!(build(&s, &img).is_err());
    let mut s = FsSpec::new(FsType::Ext4, 32 << 20);
    s.label = Some("a label longer than sixteen bytes".into());
    assert!(build(&s, &img).is_err());
    let mut s = FsSpec::new(FsType::Ext4, 32 << 20);
    s.deleted.push("nothing".into());
    assert!(build(&s, &img).is_err());
    // UUID and hash seed are pinned.
    let mut s = FsSpec::new(FsType::Ext4, 32 << 20);
    s.files.push(FileSpec::new("a", vec![1]));
    build(&s, &img).unwrap();
    let (_, sb) = check("dumpe2fs", &["-h", img.to_str().unwrap()]);
    assert!(sb.contains("d15c0e5c-0e5c-4a11-8e5c-0d15c0e5c001"), "{sb}");
    assert!(sb.contains("d15c0e5c-0e5c-4a11-8e5c-0d15c0e5c002"), "{sb}");
}
