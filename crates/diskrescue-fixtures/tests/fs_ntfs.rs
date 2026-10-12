#[macro_use]
mod common;

use std::path::Path;

use common::*;
use diskrescue_fixtures::fs::{build, ntfs_mount_available, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::tools;

const DEL_PHOTO: &str = "Pictures/2023/gone.jpg";

fn spec() -> FsSpec {
    let mut s = FsSpec::new(FsType::Ntfs, 64 << 20).cluster_size(4096);
    s.label = Some("WINDISK".into());
    s.dirs.push("Empty".into());
    s.files.push(FileSpec::new("IMG_0001.JPG", data(30_000, 1)));
    s.files
        .push(FileSpec::new("Holiday Photo 2024.jpg", data(9_000, 2)).mtime(1_700_000_000));
    s.files.push(FileSpec::new(
        "Pictures/2023/Summer/IMG_0002.jpg",
        data(70_000, 3),
    ));
    s.files.push(FileSpec::new(
        "Pictures/2023/Summer/IMG_0002.jpg:Zone.Identifier",
        b"[ZoneTransfer]\r\nZoneId=3\r\n".to_vec(),
    ));
    s.files.push(FileSpec::new(
        "readme.txt",
        b"tiny resident file\r\n".to_vec(),
    ));
    s.files.push(FileSpec::new(DEL_PHOTO, data(50_000, 4)));
    s.files
        .push(FileSpec::new("Videos/clip.mp4", data(300_000, 5)));
    s.deleted.push(DEL_PHOTO.into());
    s
}

fn ntfsls(img: &Path, path: &str) -> String {
    let (ok, text) = check("ntfsls", &["-l", "-p", path, img.to_str().unwrap()]);
    assert!(ok, "ntfsls {path}: {text}");
    text
}

fn ntfscat(img: &Path, path: &str) -> Vec<u8> {
    let out = tools::run("ntfscat", &[img.to_str().unwrap(), path]).unwrap();
    out.stdout
}

#[test]
fn ntfs_with_mount_directories_streams_deletion() {
    if skip_unless(&[
        "mkntfs",
        "ntfsls",
        "ntfscat",
        "ntfsundelete",
        "ntfsfix",
        "ntfsinfo",
    ]) {
        return;
    }
    if !ntfs_mount_available() {
        eprintln!("skipped: ntfs-3g FUSE mount unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("ntfs.img");
    let s = spec();
    let rep = build(&s, &img).unwrap();
    assert!(rep.deleted_supported);
    assert_eq!(rep.tool, "mkntfs+ntfs-3g");
    assert_eq!(rep.cluster_size, 4096);
    // The volume is consistent.
    let (ok, text) = check("ntfsfix", &["-n", img.to_str().unwrap()]);
    assert!(ok && text.contains("processed successfully"), "{text}");

    // Directory listings.
    let root = ntfsls(&img, "/");
    for n in [
        "IMG_0001.JPG",
        "Holiday Photo 2024.jpg",
        "readme.txt",
        "Pictures",
        "Videos",
        "Empty",
    ] {
        assert!(root.contains(n), "root lacks {n}:\n{root}");
    }
    assert!(ntfsls(&img, "/Pictures/2023").contains("Summer"));
    assert!(
        !ntfsls(&img, "/Pictures/2023").contains("gone.jpg"),
        "deleted file is gone"
    );

    // Live content through ntfscat; deleted content at the recorded extents.
    for f in &s.files {
        if f.path.contains(':') {
            continue;
        }
        let p = rep.find(&f.path).unwrap();
        assert_eq!(p.deleted, s.deleted.contains(&f.path));
        if !p.deleted {
            assert_bytes!(ntfscat(&img, &format!("/{}", f.path)), f.data, "{}", f.path);
        }
        if f.data.len() > 1000 {
            assert!(!p.extents.is_empty(), "{} must be non-resident", f.path);
            assert_eq!(
                read_extents(&img, &p.extents),
                f.data,
                "extents of {}",
                f.path
            );
        } else {
            assert!(p.extents.is_empty(), "{} is resident", f.path);
        }
    }

    // Named stream.
    let (ok, zone) = check(
        "ntfscat",
        &[
            "-a",
            "$DATA",
            "-n",
            "Zone.Identifier",
            img.to_str().unwrap(),
            "/Pictures/2023/Summer/IMG_0002.jpg",
        ],
    );
    assert!(ok && zone.contains("ZoneId=3"), "{zone}");

    // Deletion is visible to ntfsundelete with intact data.
    let (ok, text) = check("ntfsundelete", &["-s", img.to_str().unwrap()]);
    assert!(ok, "{text}");
    let line = text
        .lines()
        .find(|l| l.contains("gone.jpg"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("100%"), "{line}");

    // mtime was applied through the mount.
    let info = tools::run_stdout(
        "ntfsinfo",
        &["-F", "/Holiday Photo 2024.jpg", img.to_str().unwrap()],
    )
    .unwrap();
    assert!(
        info.contains("File Altered Time:\t Tue Nov 14 22:13:20 2023"),
        "{info}"
    );
}

#[test]
fn ntfs_flat_fallback_without_mount() {
    if skip_unless(&["mkntfs", "ntfscp", "ntfsls", "ntfscat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("flat.img");
    let mut s = spec();
    s.no_mount = true;
    let rep = build(&s, &img).unwrap();
    assert!(!rep.deleted_supported);
    assert!(
        rep.notes.iter().any(|n| n.contains("unavailable")),
        "{:?}",
        rep.notes
    );
    let root = ntfsls(&img, "/");
    assert!(root.contains("IMG_0001.JPG"));
    assert!(root.contains("Videos_clip.mp4"), "{root}");
    assert_eq!(
        ntfscat(&img, "/Videos_clip.mp4"),
        s.files
            .iter()
            .find(|f| f.path == "Videos/clip.mp4")
            .unwrap()
            .data
    );
}

#[test]
fn ntfs_root_only_uses_ntfscp() {
    if skip_unless(&["mkntfs", "ntfscp", "ntfscat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("cp.img");
    let mut s = FsSpec::new(FsType::Ntfs, 32 << 20);
    s.files.push(FileSpec::new("a.bin", data(20_000, 1)));
    s.files.push(FileSpec::new("b.bin", data(100, 2)));
    let rep = build(&s, &img).unwrap();
    assert_eq!(rep.tool, "mkntfs+ntfscp");
    assert_bytes!(ntfscat(&img, "/a.bin"), s.files[0].data);
    assert_bytes!(ntfscat(&img, "/b.bin"), s.files[1].data);
    if tools::have("ntfsinfo") {
        assert_eq!(
            read_extents(&img, &rep.find("a.bin").unwrap().extents),
            s.files[0].data
        );
    }
}

#[test]
fn ntfs_bad_specs() {
    if skip_unless(&["mkntfs"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("x.img");
    let mut s = FsSpec::new(FsType::Ntfs, 32 << 20);
    s.files.push(FileSpec::new("bad|name", vec![1]));
    assert!(build(&s, &img).is_err());
    let mut s = FsSpec::new(FsType::Ntfs, 32 << 20).cluster_size(1000);
    s.files.push(FileSpec::new("x", vec![1]));
    assert!(build(&s, &img).is_err());
    let mut s = FsSpec::new(FsType::Ntfs, 32 << 20);
    s.deleted.push("nothing".into());
    assert!(build(&s, &img).is_err());
}

#[test]
fn no_stale_mounts_are_left_behind() {
    if !ntfs_mount_available() {
        eprintln!("skipped: ntfs-3g FUSE mount unavailable");
        return;
    }
    let before = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let count = |t: &str| {
        t.lines()
            .filter(|l| l.contains("fuse") && l.contains("ntfs"))
            .count()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut s = FsSpec::new(FsType::Ntfs, 32 << 20);
    s.files.push(FileSpec::new("d/a.bin", data(5000, 1)));
    s.deleted.push("d/a.bin".into());
    build(&s, &dir.path().join("m.img")).unwrap();
    let after = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    assert!(
        !after.contains(dir.path().to_str().unwrap()),
        "mount point under the temp dir must be gone"
    );
    // Other tests may mount concurrently, so only compare to a sane bound.
    assert!(count(&after) <= count(&before) + 8);
}
