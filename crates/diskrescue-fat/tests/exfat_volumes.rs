//! exFAT volumes made by mkfs.exfat and filled by the test writer, checked
//! by fsck.exfat -n and dump.exfat, then read back through the crate.

mod support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_core::fs::{EntryConfidence, FsKind};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_fat::{open, open_exfat, probe, FatOptions};
use support::exfat_writer::{ExImage, ExMode};
use support::*;

const MIB: u64 = 1024 * 1024;
const UNICODE: &str = "Ünïcödé фото 日本.png";

struct Built {
    img: PathBuf,
    live: HashMap<String, Vec<u8>>,
    deleted: HashMap<String, (Vec<u8>, EntryConfidence)>,
    vdl: (String, Vec<u8>, u64),
}

fn long255() -> String {
    format!("{}.dat", "e".repeat(251))
}

fn build(tmp: &Path) -> Option<Built> {
    let img = tmp.join("exfat.img");
    if !mkfs_exfat(&img, 64 * MIB, "TESTEX") {
        return None;
    }
    let mut w = ExImage::open(&img);
    let cs = w.cs as usize;
    let root = w.root_dir();
    let mut live = HashMap::new();
    let mut put = |w: &mut ExImage,
                   d: &support::exfat_writer::ExDir,
                   path: &str,
                   data: Vec<u8>,
                   mode: ExMode| {
        let name = path.rsplit('/').next().unwrap();
        w.put(d, name, &data, mode);
        live.insert(path.to_string(), data);
    };
    put(
        &mut w,
        &root,
        "Holiday Photo 2024 exfat.jpg",
        jpeg_like(cs * 3 + 100, 1),
        ExMode::NoFatChain,
    );
    put(
        &mut w,
        &root,
        "chained clip.mp4",
        jpeg_like(cs * 20 + 9, 2),
        ExMode::FragmentedChain,
    );
    put(&mut w, &root, "empty.bin", Vec::new(), ExMode::NoFatChain);
    put(&mut w, &root, UNICODE, text_like(cs + 5, 3), ExMode::Chain);
    put(
        &mut w,
        &root,
        &long255(),
        text_like(99, 4),
        ExMode::NoFatChain,
    );
    let dcim = w.mkdir(&root, "DCIM");
    let sub = w.mkdir(&dcim, "100MSDCF");
    for i in 0..6u64 {
        let p = format!("DCIM/100MSDCF/DSC{:05}.JPG", i);
        put(
            &mut w,
            &sub,
            &p,
            jpeg_like(cs * (i as usize + 1) + 3, 10 + i),
            ExMode::NoFatChain,
        );
    }
    let vdl_data = jpeg_like(cs * 4, 5);
    let vdl = cs as u64 + 10;
    w.put_vdl(
        &root,
        "camcorder.mov",
        &vdl_data,
        ExMode::NoFatChain,
        Some(vdl),
    );
    // Files that get deleted.
    let d1 = jpeg_like(cs * 2 + 50, 6);
    w.put(&root, "deleted photo.jpg", &d1, ExMode::NoFatChain);
    let d2 = text_like(cs * 3 + 1, 7);
    w.put(&root, "deleted chained.bin", &d2, ExMode::Chain);
    let d3 = text_like(cs * 4 + 1, 8);
    w.put(&root, "deleted frag.bin", &d3, ExMode::FragmentedChain);
    let old = w.mkdir(&root, "Old Folder");
    let d4 = text_like(cs + 9, 9);
    w.put(&old, "inner.txt", &d4, ExMode::NoFatChain);
    fsck_exfat(&img);
    w.delete(&root, "deleted photo.jpg");
    w.delete(&root, "deleted chained.bin");
    w.delete(&root, "deleted frag.bin");
    w.delete(&old, "inner.txt");
    w.delete(&root, "Old Folder");
    fsck_exfat(&img);
    let mut deleted = HashMap::new();
    deleted.insert(
        "deleted photo.jpg".to_string(),
        (d1, EntryConfidence::DeletedIntact),
    );
    deleted.insert(
        "deleted chained.bin".to_string(),
        (d2, EntryConfidence::DeletedGuessed),
    );
    deleted.insert(
        "deleted frag.bin".to_string(),
        (d3, EntryConfidence::DeletedGuessed),
    );
    deleted.insert(
        "Old Folder/inner.txt".to_string(),
        (d4, EntryConfidence::DeletedIntact),
    );
    Some(Built {
        img,
        live,
        deleted,
        vdl: ("camcorder.mov".to_string(), vdl_data, vdl),
    })
}

#[test]
fn exfat_sixty_four_mib() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path()) else {
        return;
    };
    let src = source(&b.img);
    let head = std::fs::read(&b.img).unwrap()[..4096].to_vec();
    assert_eq!(probe(&head), Some(FsKind::ExFat));
    let fs = open(src.clone(), &FatOptions::default()).unwrap();
    let info = fs.info().clone();
    assert_eq!(info.kind, FsKind::ExFat);
    assert_eq!(info.label.as_deref(), Some("TESTEX"));
    assert_eq!(info.cluster_size, 4096);
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);
    let all = entries(&*fs);
    let ids: HashSet<u64> = all.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), all.len());
    assert!(
        !all.iter().any(|e| e.path.starts_with("$orphans")),
        "unexpected orphans"
    );

    for (path, data) in &b.live {
        let e = by_path(&all, path);
        assert!(!e.deleted);
        assert_eq!(e.confidence, EntryConfidence::Allocated, "{path}");
        assert_eq!(e.size, data.len() as u64);
        let (got, rep) = read(&*fs, e);
        assert!(got == *data, "{path}: content differs");
        assert!(rep.is_complete() && rep.holes.is_empty(), "{path}: {rep:?}");
    }
    let clip = by_path(&all, "chained clip.mp4");
    assert!(clip.extents.len() > 10);
    // Local 12:30:10 at UTC+05:30 = 07:00:10Z; create time has +0.5 s.
    let h = by_path(&all, "Holiday Photo 2024 exfat.jpg");
    assert_eq!(h.modified.unwrap().to_rfc3339(), "2024-05-01T07:00:10Z");
    assert_eq!(h.created.unwrap().nanos, 500_000_000);
    assert_eq!(
        by_path(&all, "DCIM/100MSDCF").parent_id,
        Some(by_path(&all, "DCIM").id)
    );

    // ValidDataLength < DataLength: the tail reads as zeros (a hole).
    let (vp, vdata, vdl) = &b.vdl;
    let e = by_path(&all, vp);
    assert_eq!(e.size, vdata.len() as u64);
    let (got, rep) = read(&*fs, e);
    assert!(got[..*vdl as usize] == vdata[..*vdl as usize]);
    assert!(got[*vdl as usize..].iter().all(|&x| x == 0));
    assert_eq!(
        rep.holes.as_slice(),
        &[Range::new(*vdl, vdata.len() as u64)]
    );
    assert!(rep.unreadable.is_empty());

    for (path, (data, conf)) in &b.deleted {
        let e = by_path(&all, path);
        assert!(e.deleted, "{path}");
        assert_eq!(e.confidence, *conf, "{path}: {:?}", e.flags);
        let (got, rep) = read(&*fs, e);
        assert!(got == *data, "{path}: deleted content differs");
        assert!(rep.reused.is_empty());
    }
    assert!(by_path(&all, "deleted frag.bin")
        .flags
        .iter()
        .any(|f| f.contains("still intact")));
    let old = by_path(&all, "Old Folder");
    assert!(old.is_dir && old.deleted);
    assert_eq!(old.confidence, EntryConfidence::DeletedIntact);

    // Free space vs the bitmap and dump.exfat.
    let ex = open_exfat(src.clone(), &FatOptions::default()).unwrap();
    let free = ex.free_clusters();
    let un = fs.unallocated().unwrap();
    assert_eq!(un.total_len(), free * 4096);
    if let Some(dump_free) = dump_exfat_free(&b.img) {
        assert_eq!(free, dump_free, "free clusters vs dump.exfat");
    }
    for path in b.live.keys() {
        for x in &by_path(&all, path).extents {
            assert!(!un.intersects(*x), "{path} overlaps free space");
        }
    }
    // Deleted NoFatChain data lies in free space.
    for x in &by_path(&all, "deleted photo.jpg").extents {
        assert!(un.covers(*x));
    }
    assert_eq!(ex.boot_sector().bytes_per_sector_shift, 9);
    assert_eq!(ex.cluster_to_offset(2), info.data_start);
    let clip_first = (clip.extents[0].start - info.data_start) / 4096 + 2;
    // The chain covers whole clusters; the entry's extents stop at the size.
    let chain = ex.chain(clip_first as u32);
    assert_eq!(chain.len(), clip.extents.len());
    for (c, e) in chain.iter().zip(&clip.extents) {
        assert_eq!(c.start, e.start);
    }
}

#[test]
fn exfat_backup_boot_region() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path()) else {
        return;
    };
    let orig = std::fs::read(&b.img).unwrap();
    // Sector 0 destroyed.
    let mut bytes = orig.clone();
    bytes[..512].fill(0);
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let fs = open(src.clone(), &FatOptions::default()).unwrap();
    assert_eq!(fs.info().kind, FsKind::ExFat);
    assert!(
        fs.info().warnings.iter().any(|w| w.contains("backup")),
        "{:?}",
        fs.info().warnings
    );
    let all = entries(&*fs);
    let e = by_path(&all, "chained clip.mp4");
    assert!(read(&*fs, e).0 == b.live["chained clip.mp4"]);
    let no_backup = FatOptions {
        use_backup_boot_sector: false,
        ..FatOptions::default()
    };
    assert!(open_exfat(src, &no_backup).is_err());
    // A damaged extended boot sector only breaks the checksum.
    let mut bytes = orig.clone();
    bytes[512 + 100] ^= 0x5A;
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let fs = open_exfat(src.clone(), &FatOptions::default()).unwrap();
    assert!(fs
        .volume_info()
        .warnings
        .iter()
        .any(|w| w.contains("backup")));
    let fs = open_exfat(src, &no_backup).unwrap();
    assert!(fs
        .volume_info()
        .warnings
        .iter()
        .any(|w| w.contains("checksum")));
}

#[test]
fn exfat_unreadable_sector_inside_file() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path()) else {
        return;
    };
    let fs0 = open_exfat(source(&b.img), &FatOptions::default()).unwrap();
    let all = entries(&fs0);
    let path = "chained clip.mp4";
    let e = by_path(&all, path).clone();
    let ext = e.extents[2];
    let bad = Range::with_len(ext.start + 1024, 512);
    let file_off = e.extents[0].len() + e.extents[1].len() + 1024;
    let bytes = std::fs::read(&b.img).unwrap();
    let src: SharedSource =
        Arc::new(MemSource::new(bytes).with_unreadable(RangeSet::from_range(bad)));
    let fs = open_exfat(src, &FatOptions::default()).unwrap();
    let (got, rep) = read(&fs, &e);
    assert_eq!(rep.unreadable.as_slice(), &[Range::with_len(file_off, 512)]);
    let want = &b.live[path];
    let a = file_off as usize;
    assert!(got[a..a + 512].iter().all(|&x| x == 0));
    assert!(got[..a] == want[..a] && got[a + 512..] == want[a + 512..]);
}

#[test]
fn exfat_orphans_after_reformat_and_lost_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("re.img");
    if !mkfs_exfat(&img, 64 * MIB, "CAM") {
        return;
    }
    let mut w = ExImage::open(&img);
    let cs = w.cs as usize;
    let root = w.root_dir();
    let dcim = w.mkdir(&root, "DCIM");
    let sub = w.mkdir(&dcim, "100MSDCF");
    let mut files = Vec::new();
    for i in 0..4u64 {
        let d = jpeg_like(cs * 2 + 5 * i as usize, 80 + i);
        let name = format!("Holiday picture {i}.jpg");
        w.put(&sub, &name, &d, ExMode::NoFatChain);
        files.push((name, d));
    }
    let lost = w.mkdir(&root, "LOST DIR");
    let ld = jpeg_like(cs * 3, 90);
    w.put(&lost, "kept.jpg", &ld, ExMode::Chain);
    fsck_exfat(&img);
    // Lost directory: its set in the root reads as zeros, data intact.
    w.wipe(&root, "LOST DIR");
    let lost_c = lost.clusters[0];
    drop(w);
    let fs = open_exfat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let e = by_path(&all, &format!("$orphans/cluster_{lost_c}/kept.jpg"));
    assert_eq!(e.confidence, EntryConfidence::Allocated);
    assert!(read(&fs, e).0 == ld);
    assert!(by_path(&all, &format!("$orphans/cluster_{lost_c}"))
        .flags
        .iter()
        .any(|f| f.contains("lost directory")));

    // Quick reformat: the old directory clusters are now free.
    let t = tool("mkfs.exfat").unwrap();
    let (ok, out) = run(&t, &["-L", "CAM", img.to_str().unwrap()]);
    assert!(ok, "{out}");
    let fs = open_exfat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let dcim_c = dcim.clusters[0];
    for (name, d) in &files {
        let p = format!("$orphans/cluster_{dcim_c}/100MSDCF/{name}");
        let e = by_path(&all, &p);
        assert!(!e.deleted);
        assert_eq!(e.confidence, EntryConfidence::DeletedIntact, "{p}");
        assert!(read(&fs, e).0 == *d, "{p}");
    }
    let tops: Vec<_> = all
        .iter()
        .filter(|e| e.path.starts_with("$orphans/cluster_") && e.path.matches('/').count() == 1)
        .map(|e| e.path.clone())
        .collect();
    assert!(
        tops.contains(&format!("$orphans/cluster_{dcim_c}")),
        "{tops:?}"
    );
    assert!(
        !tops.contains(&format!("$orphans/cluster_{}", sub.clusters[0])),
        "{tops:?}"
    );
}

#[test]
fn exfat_root_pointer_destroyed_root_found_as_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(b) = build(tmp.path()) else {
        return;
    };
    let mut bytes = std::fs::read(&b.img).unwrap();
    // Point FirstClusterOfRootDirectory nowhere in both boot regions
    // (their checksums no longer match either).
    for base in [0usize, 12 * 512] {
        bytes[base + 0x60..base + 0x64].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
    }
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let fs = open_exfat(src, &FatOptions::default()).unwrap();
    let w = &fs.volume_info().warnings;
    assert!(
        w.iter().any(|x| x.contains("root directory cluster")),
        "{w:?}"
    );
    assert!(w.iter().any(|x| x.contains("checksum")), "{w:?}");
    let all = entries(&fs);
    // mkfs.exfat puts the root at cluster 5 on a 64 MiB volume.
    let root = "$orphans/cluster_5";
    by_path(&all, root);
    for path in [
        "Holiday Photo 2024 exfat.jpg",
        "DCIM/100MSDCF/DSC00003.JPG",
        "chained clip.mp4",
    ] {
        let e = by_path(&all, &format!("{root}/{path}"));
        assert!(!e.deleted);
        assert!(read(&fs, e).0 == b.live[path], "{path}");
    }
    let (data, _) = &b.deleted["deleted photo.jpg"];
    let e = by_path(&all, &format!("{root}/deleted photo.jpg"));
    assert!(e.deleted);
    assert!(read(&fs, e).0 == *data);
}
