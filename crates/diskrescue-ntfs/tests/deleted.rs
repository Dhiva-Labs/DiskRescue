//! Deleted files and directories produced by real deletions through an
//! ntfs-3g mount, cross-checked against ntfsundelete.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem};
use diskrescue_ntfs::walk::{split_id, SUB_ADS};
use diskrescue_ntfs::NtfsOptions;

/// `ntfsundelete -s` listing: record number -> (name, recoverable %).
fn ntfsundelete(img: &Path) -> BTreeMap<u64, (String, u32)> {
    let out = Command::new(tool("ntfsundelete").unwrap())
        .arg("-s")
        .arg(img)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        let Ok(inode) = f[0].parse::<u64>() else {
            continue;
        };
        let pct = f[2].trim_end_matches('%').parse::<u32>().unwrap_or(0);
        let name = f[6..].join(" ");
        if name != "<none>" {
            map.insert(inode, (name, pct));
        }
    }
    map
}

fn deleted_records(all: &[FileEntry]) -> BTreeMap<u64, &FileEntry> {
    all.iter()
        .filter(|e| e.deleted)
        .filter(|e| {
            let (_, sub) = split_id(e.id);
            sub == 0
        })
        .map(|e| (split_id(e.id).0, e))
        .collect()
}

#[test]
fn deleted_file_and_directory_match_ntfsundelete() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("del.img");
    mkntfs(&img, 32, 4096, "DEL", &[]);
    let a = data(1, 5000);
    let b = data(2, 300);
    let keep = data(3, 70_000);
    let loose = data(4, 40_000);
    {
        let m = Mount::new(&img, "");
        std::fs::create_dir_all(m.path("DCIM/sub")).unwrap();
        write(&m.path("DCIM/a.jpg"), &a);
        write(&m.path("DCIM/sub/b.jpg"), &b);
        write(&m.path("keep.bin"), &keep);
        write(&m.path("loose.jpg"), &loose);
        m.unmount();
    }
    {
        let m = Mount::new(&img, "");
        std::fs::remove_dir_all(m.path("DCIM")).unwrap();
        std::fs::remove_file(m.path("loose.jpg")).unwrap();
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);

    let dcim = find(&all, "DCIM");
    assert!(dcim.is_dir && dcim.deleted);
    let e = find(&all, "DCIM/a.jpg");
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedIntact);
    assert_eq!(e.parent_id, Some(dcim.id));
    let (got, rep) = read(&fs, e);
    assert_eq!(got, a);
    assert!(rep.is_complete(), "{rep:?}");
    let e = find(&all, "DCIM/sub/b.jpg");
    assert!(e.deleted && e.resident);
    assert_eq!(read(&fs, e).0, b);
    assert!(find(&all, "DCIM/sub").deleted);
    let e = find(&all, "loose.jpg");
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedIntact);
    assert_eq!(read(&fs, e).0, loose);
    let free = fs.unallocated().unwrap();
    for r in &e.extents {
        assert!(free.covers(*r), "deleted file's clusters are free space");
    }
    let k = find(&all, "keep.bin");
    assert!(!k.deleted);
    assert_eq!(read(&fs, k).0, keep);

    // Same deleted records and names as ntfsundelete, same verdicts.
    if have(&["ntfsundelete"]) {
        let theirs = ntfsundelete(&img);
        let ours = deleted_records(&all);
        let their_set: BTreeSet<(u64, String)> =
            theirs.iter().map(|(k, v)| (*k, v.0.clone())).collect();
        let our_set: BTreeSet<(u64, String)> =
            ours.iter().map(|(k, e)| (*k, e.name.clone())).collect();
        assert_eq!(our_set, their_set);
        for (rec, (_, pct)) in &theirs {
            let e = ours[rec];
            if !e.is_dir {
                assert_eq!(
                    *pct == 100,
                    e.confidence == EntryConfidence::DeletedIntact,
                    "{}: ntfsundelete {pct}%, ours {:?}",
                    e.path,
                    e.confidence
                );
            }
        }
    }

    // Live-only and tree-only walks hide them.
    let live = entries(&open_img(
        &img,
        &NtfsOptions {
            include_deleted: false,
            ..Default::default()
        },
    ));
    assert!(live.iter().all(|e| !e.deleted));
    assert!(live.iter().any(|e| e.path == "keep.bin"));
    let tree = entries(&open_img(
        &img,
        &NtfsOptions {
            walk_whole_mft: false,
            include_deleted: false,
            ..Default::default()
        },
    ));
    assert!(tree.iter().all(|e| !e.deleted));
}

#[test]
fn reused_clusters_downgrade_confidence() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("reuse.img");
    mkntfs(&img, 16, 4096, "REUSE", &[]);
    let mut count = 0u64;
    {
        let m = Mount::new(&img, "");
        while std::fs::write(
            m.path(&format!("fill{count}.bin")),
            data(100 + count, 131_072),
        )
        .is_ok()
        {
            count += 1;
            assert!(count < 1000);
        }
        for i in (0..count).step_by(2) {
            std::fs::remove_file(m.path(&format!("fill{i}.bin"))).unwrap();
        }
        write(&m.path("big.bin"), &data(7, 2_000_000));
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let deleted: Vec<&FileEntry> = all.iter().filter(|e| e.deleted).collect();
    assert!(!deleted.is_empty());
    let mut intact = 0;
    let mut guessed = 0;
    for e in &deleted {
        let n: u64 = e.name["fill".len()..e.name.len() - 4].parse().unwrap();
        let (got, rep) = read(&fs, e);
        match e.confidence {
            EntryConfidence::DeletedIntact => {
                intact += 1;
                assert!(rep.reused.is_empty());
                assert_eq!(got, data(100 + n, 131_072), "{}", e.name);
            }
            EntryConfidence::DeletedGuessed => {
                guessed += 1;
                assert!(!rep.reused.is_empty(), "{}", e.name);
                assert!(!rep.is_complete());
            }
            other => panic!("{}: unexpected {other:?}", e.name),
        }
    }
    assert!(
        intact > 0 && guessed > 0,
        "intact {intact} guessed {guessed}"
    );
    if have(&["ntfsundelete"]) {
        let theirs = ntfsundelete(&img);
        let ours = deleted_records(&all);
        assert_eq!(
            theirs.keys().collect::<Vec<_>>(),
            ours.keys().collect::<Vec<_>>()
        );
        for (rec, (name, pct)) in &theirs {
            let e = ours[rec];
            assert_eq!(&e.name, name);
            assert_eq!(
                *pct == 100,
                e.confidence == EntryConfidence::DeletedIntact,
                "{name}"
            );
        }
    }
}

#[test]
fn reused_parent_record_makes_an_orphan() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("orphan.img");
    mkntfs(&img, 16, 4096, "ORPH", &[]);
    let content = data(9, 20_000);
    {
        let m = Mount::new(&img, "");
        std::fs::create_dir(m.path("Album")).unwrap();
        write(&m.path("Album/photo.jpg"), &content);
        m.unmount();
    }
    {
        let m = Mount::new(&img, "");
        std::fs::remove_dir_all(m.path("Album")).unwrap();
        m.unmount();
    }
    {
        // The next new file takes the lowest free record: the old
        // directory's, so the photo's parent reference goes stale.
        let m = Mount::new(&img, "");
        write(&m.path("newer.txt"), b"reuses the directory record");
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let photo = all
        .iter()
        .find(|e| e.name == "photo.jpg")
        .expect("deleted photo listed");
    assert!(photo.deleted);
    assert!(photo.path.starts_with("$orphans/"), "{}", photo.path);
    assert!(photo.flags.iter().any(|f| f == "orphan"));
    assert!(
        photo.flags.iter().any(|f| f == "parent-reused"),
        "{:?}",
        photo.flags
    );
    let rec = split_id(photo.id).0;
    assert_eq!(photo.path, format!("$orphans/{rec}/photo.jpg"));
    // Virtual directories come first.
    let pos = |p: &str| all.iter().position(|e| e.path == p).unwrap();
    assert!(pos("$orphans") < pos(&format!("$orphans/{rec}")));
    assert!(pos(&format!("$orphans/{rec}")) < pos(&photo.path));
    assert!(find(&all, "$orphans").flags.iter().any(|f| f == "virtual"));
    let (got, rep) = read(&fs, photo);
    assert_eq!(got, content);
    assert!(rep.notes.iter().any(|n| n.contains("orphans")));
    // The directory record now holds the new file.
    assert!(!find(&all, "newer.txt").deleted);
    assert!(!all.iter().any(|e| e.path == "Album"));
    // Synthetic entries refuse reads.
    let v = find(&all, "$orphans");
    assert!(fs.read_file(v, &mut Vec::new()).is_err());
}

#[test]
fn named_stream_of_deleted_file() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ads.img");
    mkntfs(&img, 16, 4096, "ADS", &[]);
    let main = data(11, 9000);
    let side = data(12, 6000);
    {
        let m = Mount::new(&img, "streams_interface=windows");
        write(&m.path("clip.mp4"), &main);
        write(&m.path("clip.mp4:thumb"), &side);
        m.unmount();
    }
    {
        let m = Mount::new(&img, "");
        std::fs::remove_file(m.path("clip.mp4")).unwrap();
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let e = find(&all, "clip.mp4");
    assert!(e.deleted);
    assert_eq!(read(&fs, e).0, main);
    let s = find(&all, "clip.mp4:thumb");
    assert!(s.deleted);
    assert!(s.flags.iter().any(|f| f == "ads"));
    assert!(split_id(s.id).1 >= SUB_ADS);
    assert_eq!(read(&fs, s).0, side);
}
