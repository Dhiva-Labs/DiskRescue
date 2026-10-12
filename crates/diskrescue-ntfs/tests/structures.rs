//! Structures that only appear on busy volumes: attribute lists (many hard
//! links, data runs split across extension records) and directory index
//! slack holding names of deleted files.

mod common;

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileSystem};
use diskrescue_ntfs::attr::types;
use diskrescue_ntfs::walk::split_id;
use diskrescue_ntfs::NtfsOptions;

#[test]
fn attribute_lists_links_and_split_data() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("al.img");
    mkntfs(&img, 24, 4096, "ALIST", &[]);
    let shared = data(1, 5000);
    let frag_len;
    {
        let m = Mount::new(&img, "");
        // 30 hard links with long names overflow the record: the extra
        // $FILE_NAMEs move to extension records.
        write(&m.path("multi.bin"), &shared);
        std::fs::create_dir(m.path("L")).unwrap();
        for k in 0..30 {
            std::fs::hard_link(
                m.path("multi.bin"),
                m.path(&format!("L/link_with_a_fairly_long_name_{k:03}.bin")),
            )
            .unwrap();
        }
        // Checkerboard the free space, then write a file into the holes so
        // its run list no longer fits in one record.
        std::fs::create_dir(m.path("f")).unwrap();
        let mut n = 0u64;
        while std::fs::write(m.path(&format!("f/{n}")), data(1000 + n, 4096)).is_ok() {
            n += 1;
            assert!(n < 100_000);
        }
        for i in (0..n).step_by(2) {
            std::fs::remove_file(m.path(&format!("f/{i}"))).unwrap();
        }
        let st = statvfs_free(&m.dir);
        frag_len = ((st.saturating_sub(600_000)) / 65536 * 65536) as usize;
        write(&m.path("frag.bin"), &data(2, frag_len));
        m.unmount();
    }
    let frag = data(2, frag_len);
    for walk_whole_mft in [true, false] {
        let fs = open_img(
            &img,
            &NtfsOptions {
                walk_whole_mft,
                ..Default::default()
            },
        );
        let all = entries(&fs);
        let links: Vec<_> = all
            .iter()
            .filter(|e| e.path.starts_with("L/link_with"))
            .collect();
        assert_eq!(links.len(), 30, "whole MFT: {walk_whole_mft}");
        for l in &links {
            assert_eq!(read(&fs, l).0, shared);
        }
        let mut ids: Vec<u64> = links.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 30);
        let e = find(&all, "frag.bin");
        assert!(e.extents.len() > 100, "{} extents", e.extents.len());
        let (got, rep) = read(&fs, e);
        assert!(rep.is_complete(), "{:?}", rep.notes);
        assert!(got == frag);
        // The file's $DATA really is split over several records.
        let rec = fs.record(split_id(e.id).0).unwrap();
        let attrs = fs.attributes(&rec);
        assert!(attrs.iter().any(|a| a.type_code == types::ATTRIBUTE_LIST));
        let holders: std::collections::BTreeSet<u64> = attrs
            .iter()
            .filter(|a| a.type_code == types::DATA)
            .map(|a| a.record)
            .collect();
        assert!(holders.len() >= 2, "{holders:?}");
    }
    if have(&["ntfscat"]) {
        assert!(ntfscat(&img, "/frag.bin") == frag);
    }

    // Deleting it: ntfs-3g drops the $FILE_NAME and truncates the attribute
    // list, but the extension records still point back at the base record.
    {
        let m = Mount::new(&img, "");
        std::fs::remove_file(m.path("frag.bin")).unwrap();
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let rec_entry = all
        .iter()
        .find(|e| e.deleted && e.size == frag_len as u64 && !e.extents.is_empty())
        .expect("deleted fragmented file listed");
    assert!(rec_entry.path.starts_with("$orphans/"));
    assert_eq!(rec_entry.confidence, EntryConfidence::DeletedGuessed);
    assert!(rec_entry
        .flags
        .iter()
        .any(|f| f == "attribute-pieces-guessed"));
    let (got, rep) = read(&fs, rec_entry);
    assert!(rep.unreadable.is_empty(), "{:?}", rep.notes);
    assert!(got == frag, "recovered content differs");
}

fn statvfs_free(p: &std::path::Path) -> u64 {
    let out = std::process::Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or(0)
}

#[test]
fn index_slack_keeps_names_of_reused_records() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("slack.img");
    mkntfs(&img, 16, 4096, "SLACK", &[]);
    let last = data(40, 3000);
    {
        let m = Mount::new(&img, "");
        std::fs::create_dir(m.path("Photos")).unwrap();
        for i in 1..=40u64 {
            let bytes = if i == 40 { last.clone() } else { data(i, 3000) };
            write(&m.path(&format!("Photos/IMG_{i:04}.JPG")), &bytes);
        }
        m.unmount();
    }
    {
        // Removing the last entry of a node leaves its key in the slack.
        let m = Mount::new(&img, "");
        std::fs::remove_file(m.path("Photos/IMG_0040.JPG")).unwrap();
        m.unmount();
    }
    // Record still intact: the whole-MFT walk recovers the file itself,
    // the tree walk only finds the name in the slack.
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let e = find(&all, "Photos/IMG_0040.JPG");
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedIntact);
    assert_eq!(read(&fs, e).0, last);
    assert_eq!(
        all.iter()
            .filter(|e| e.path == "Photos/IMG_0040.JPG")
            .count(),
        1
    );
    let tree = entries(&open_img(
        &img,
        &NtfsOptions {
            walk_whole_mft: false,
            ..Default::default()
        },
    ));
    let s = find(&tree, "Photos/IMG_0040.JPG");
    assert!(s.flags.iter().any(|f| f == "index-slack"), "{:?}", s.flags);
    assert_eq!(s.size, 3000);
    assert_eq!(s.confidence, EntryConfidence::DeletedNoData);

    // A new file reuses the record: only the slack remembers the name.
    {
        let m = Mount::new(&img, "");
        write(&m.path("newer.txt"), b"takes the free record");
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let e = find(&all, "Photos/IMG_0040.JPG");
    assert!(e.deleted);
    assert_eq!(e.confidence, EntryConfidence::DeletedNoData);
    assert!(e.flags.iter().any(|f| f == "index-slack"));
    assert_eq!(e.size, 3000);
    assert!(e.modified.is_some_and(|t| t.is_plausible()));
    assert!(fs.read_file(e, &mut Vec::new()).is_err());
    let photos = find(&all, "Photos");
    assert_eq!(e.parent_id, Some(photos.id));
    // No other slack noise: every live photo is listed exactly once.
    for i in 1..40 {
        let p = format!("Photos/IMG_{i:04}.JPG");
        assert_eq!(all.iter().filter(|e| e.path == p).count(), 1, "{p}");
    }
    assert!(!all
        .iter()
        .any(|e| e.name.starts_with('$') && !e.path.starts_with("$orphans")));
}
