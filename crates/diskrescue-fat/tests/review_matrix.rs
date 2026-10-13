//! Review regression tests: volumes built by the independent
//! `diskrescue-fixtures` writer (mkfs.vfat / mkfs.exfat + its own
//! population code, fsck-validated) for every cluster size the mkfs tools
//! allow, read back through the crate and compared byte for byte with the
//! recorded content and extents. Also covers clusters reused after a delete,
//! very large directories and FAT32 volumes past 65535 clusters with the
//! Windows high-word-zeroing delete.

mod support;

use std::collections::HashMap;

use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem, FsKind};
use diskrescue_core::{Range, RangeSet};
use diskrescue_fat::{open, open_exfat, open_fat, FatOptions};
use diskrescue_fixtures::fs::{build, BuildReport, FileSpec, FsSpec, FsType};
use support::*;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// Content for the fixtures: JPEG-looking, unique per seed.
fn content(len: u64, seed: u64) -> Vec<u8> {
    jpeg_like(len as usize, seed)
}

/// Bytes of a file through the crate, with the length checked.
fn read_all(fs: &dyn FileSystem, e: &FileEntry) -> (Vec<u8>, diskrescue_core::fs::FileReadReport) {
    read(fs, e)
}

fn extents_of(p: &diskrescue_fixtures::fs::PlacedFile) -> Vec<Range> {
    let mut out: Vec<Range> = Vec::new();
    for &(o, l) in &p.extents {
        match out.last_mut() {
            Some(last) if last.end == o => last.end += l,
            _ => out.push(Range::with_len(o, l)),
        }
    }
    out
}

/// The standard file set for a cluster size: boundary sizes around one
/// cluster, a multi-cluster file in nested directories, long names, a
/// fragmented file whose gap is filled by a later file, and a deleted file.
fn spec_for(kind: FsType, size: u64, cs: u64) -> (FsSpec, Vec<String>) {
    let mut s = FsSpec::new(kind, size).cluster_size(cs as u32);
    s.label = Some("REVIEW".into());
    let big = cs >= MIB;
    let mut sizes: Vec<(String, u64)> = vec![
        ("empty.bin".into(), 0),
        ("one.bin".into(), 1),
        ("csm1.bin".into(), cs - 1),
        ("cs.bin".into(), cs),
        ("csp1.bin".into(), cs + 1),
    ];
    if !big {
        sizes.push(("DCIM/100TEST/IMG_0001.JPG".into(), 3 * cs + 17));
        sizes.push((
            "DCIM/100TEST/Long name with ünïcode ✓ and spaces.jpg".into(),
            2 * cs + 5,
        ));
        sizes.push((format!("DCIM/{}.jpg", "n".repeat(251)), 777));
    } else {
        sizes.push(("DCIM/100TEST/IMG_0001.JPG".into(), 4097));
    }
    for (seed, (p, l)) in (1..).zip(sizes.iter()) {
        s.files.push(FileSpec::new(p.clone(), content(*l, seed)));
    }
    if !big {
        // Fragmented: 3 clusters, a gap of 2, then the rest; the next file
        // fills the gap first-fit.
        s.files
            .push(FileSpec::new("frag.mov", content(10 * cs + 3, 50)).fragmented(3, 2));
        s.files
            .push(FileSpec::new("gapfill.bin", content(2 * cs, 51)));
        s.files
            .push(FileSpec::new("deleted.jpg", content(2 * cs + 100, 52)));
        s.files
            .push(FileSpec::new("after.jpg", content(cs + 9, 53)));
        s.deleted.push("deleted.jpg".into());
    }
    let paths = s.files.iter().map(|f| f.path.clone()).collect();
    (s, paths)
}

/// Build the spec, verify with fsck, open with the crate and compare every
/// file. Returns false when a tool is missing.
fn check_fixture(kind: FsType, size: u64, cs: u64) -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("vol.img");
    let (spec, _) = spec_for(kind, size, cs);
    let rep: BuildReport = match build(&spec, &img) {
        Ok(r) => r,
        Err(e) => {
            if format!("{e}").contains("missing") || format!("{e:?}").contains("Missing") {
                eprintln!("skipped: {e}");
                return false;
            }
            panic!("fixture build failed for {kind:?} cs={cs}: {e}");
        }
    };
    assert_eq!(rep.cluster_size as u64, cs, "fixture cluster size");
    let free_clusters = if kind == FsType::ExFat {
        fsck_exfat(&img);
        dump_exfat_free(&img)
    } else {
        fsck_vfat(&img).map(|(u, t)| t - u)
    };
    let src = source(&img);
    let fs: Box<dyn FileSystem> = open(src.clone(), &FatOptions::default()).unwrap();
    let want_kind = match kind {
        FsType::Fat12 => FsKind::Fat12,
        FsType::Fat16 => FsKind::Fat16,
        FsType::Fat32 => FsKind::Fat32,
        _ => FsKind::ExFat,
    };
    assert_eq!(fs.info().kind, want_kind, "cs={cs}");
    assert_eq!(fs.info().cluster_size as u64, cs);
    assert_eq!(fs.info().data_start, rep.data_start, "data start");
    assert_eq!(fs.info().label.as_deref(), Some("REVIEW"));
    let all = entries(&*fs);
    let unalloc = fs.unallocated().unwrap();
    let by: HashMap<&str, &FileEntry> = all.iter().map(|e| (e.path.as_str(), e)).collect();
    for f in &spec.files {
        let placed = rep.find(&f.path).unwrap();
        if placed.deleted {
            let e = all
                .iter()
                .find(|e| {
                    e.deleted
                        && e.name.len() == f.path.len()
                        && e.name[1..].eq_ignore_ascii_case(&f.path[1..])
                })
                .unwrap_or_else(|| panic!("deleted {} not listed", f.path));
            assert!(
                matches!(
                    e.confidence,
                    EntryConfidence::DeletedGuessed | EntryConfidence::DeletedIntact
                ),
                "{}: {:?}",
                f.path,
                e.confidence
            );
            let (bytes, rr) = read_all(&*fs, e);
            // Contiguous and never reused: the guess is right.
            assert_eq!(bytes, f.data, "deleted {} content (cs={cs})", f.path);
            assert!(rr.reused.is_empty(), "{}: {:?}", f.path, rr.reused);
            for r in &e.extents {
                assert!(
                    unalloc.covers(*r),
                    "deleted extent {r:?} not in unallocated()"
                );
            }
            continue;
        }
        let e = by
            .get(f.path.as_str())
            .unwrap_or_else(|| panic!("{} missing (cs={cs}); have {:?}", f.path, by.keys()));
        assert!(!e.deleted);
        assert_eq!(e.confidence, EntryConfidence::Allocated);
        assert_eq!(e.size, f.data.len() as u64);
        assert_eq!(
            e.extents,
            extents_of(placed),
            "{} extents (cs={cs})",
            f.path
        );
        let (bytes, rr) = read_all(&*fs, e);
        assert!(bytes == f.data, "{} content differs (cs={cs})", f.path);
        assert!(rr.is_complete(), "{}: {rr:?}", f.path);
        assert!(rr.holes.is_empty());
        for r in &e.extents {
            assert!(
                !unalloc.intersects(*r),
                "live extent {r:?} of {} reported unallocated",
                f.path
            );
        }
    }
    if let Some(free) = free_clusters {
        assert_eq!(
            unalloc.total_len(),
            free * cs,
            "unallocated() differs from the fsck/dump free count (cs={cs})"
        );
    }
    // Every unallocated range is cluster aligned inside the data area.
    for r in unalloc.iter() {
        assert!(r.start >= rep.data_start);
        assert_eq!((r.start - rep.data_start) % cs, 0);
        assert_eq!(r.len() % cs, 0);
    }
    true
}

#[test]
fn fat12_every_cluster_size() {
    for shift in 9..=16u32 {
        let cs = 1u64 << shift;
        let size = (3000 * cs).max(2 * MIB);
        // FAT12 must stay below 4085 clusters.
        let size = size.min(4000 * cs);
        if !check_fixture(FsType::Fat12, size, cs) {
            return;
        }
    }
}

#[test]
fn fat16_every_cluster_size() {
    for shift in 9..=16u32 {
        let cs = 1u64 << shift;
        if !check_fixture(FsType::Fat16, 20_000 * cs, cs) {
            return;
        }
    }
}

#[test]
fn fat32_every_cluster_size() {
    for shift in 9..=16u32 {
        let cs = 1u64 << shift;
        if !check_fixture(FsType::Fat32, 80_000 * cs, cs) {
            return;
        }
    }
}

#[test]
fn exfat_every_cluster_size() {
    for shift in 9..=25u32 {
        let cs = 1u64 << shift;
        let size = (64 * MIB).max(48 * cs);
        if !check_fixture(FsType::ExFat, size, cs) {
            return;
        }
    }
}

/// Deleted file whose clusters were partly reused by a file written later
/// (first fit), then the later file deleted as well.
fn reuse_after_delete(kind: FsType) {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("reuse.img");
    let cs = 4 * KIB;
    let size = match kind {
        FsType::Fat32 => 80_000 * cs,
        FsType::Fat16 => 20_000 * cs,
        _ => 64 * MIB,
    };
    let old = content(5 * cs, 1);
    let keep = content(2 * cs, 2);
    let mut spec = FsSpec::new(kind, size).cluster_size(cs as u32);
    spec.files
        .push(FileSpec::new("DCIM/OLD_0001.JPG", old.clone()));
    spec.files
        .push(FileSpec::new("DCIM/KEEP_002.JPG", keep.clone()));
    spec.deleted.push("DCIM/OLD_0001.JPG".into());
    let rep = match build(&spec, &img) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    let old_ext = extents_of(rep.find("DCIM/OLD_0001.JPG").unwrap());
    let new = content(3 * cs, 3);
    // Write a new photo after the delete with the independent test writer:
    // first fit lands on the deleted file's clusters.
    if kind == FsType::ExFat {
        use support::exfat_writer::{ExImage, ExMode};
        let mut w = ExImage::open(&img);
        let root = w.root_dir();
        let p = w.put(&root, "NEW_0003.JPG", &new, ExMode::NoFatChain);
        assert_eq!(w.coff(p.clusters[0]), old_ext[0].start, "first fit");
        drop(w);
        fsck_exfat(&img);
    } else {
        use support::fat_writer::{Alloc, Dir, FatImage};
        let mut w = FatImage::open(&img);
        let p = w.put(Dir::Root, "NEW_0003.JPG", &new, Alloc::Contiguous);
        assert_eq!(w.clus_off(p.clusters[0]), old_ext[0].start, "first fit");
        drop(w);
        fsck_vfat(&img);
    }
    let fs = open(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&*fs);
    let old_e = all
        .iter()
        .find(|e| e.deleted && e.path.ends_with("LD_0001.JPG"))
        .unwrap_or_else(|| panic!("old file not listed: {all:#?}"));
    assert_ne!(old_e.confidence, EntryConfidence::DeletedIntact);
    assert_ne!(old_e.confidence, EntryConfidence::Allocated);
    let (bytes, rr) = read(&*fs, old_e);
    assert_eq!(
        rr.reused,
        RangeSet::from_range(Range::new(0, 3 * cs)),
        "{kind:?}: reused clusters must be reported exactly"
    );
    assert!(!rr.is_complete());
    assert_eq!(&bytes[..3 * cs as usize], &new[..], "data now on disk");
    assert_eq!(&bytes[3 * cs as usize..], &old[3 * cs as usize..]);
    let k = by_path(&all, "DCIM/KEEP_002.JPG");
    assert_eq!(read(&*fs, k).0, keep);

    // Now delete the new file too: both deleted entries claim the same
    // clusters, so the old one cannot be called intact.
    if kind == FsType::ExFat {
        use support::exfat_writer::ExImage;
        let mut w = ExImage::open(&img);
        let root = w.root_dir();
        w.delete(&root, "NEW_0003.JPG");
        drop(w);
        fsck_exfat(&img);
    } else {
        use support::fat_writer::{Dir, FatImage};
        let mut w = FatImage::open(&img);
        w.delete(Dir::Root, "NEW_0003.JPG", false);
        drop(w);
        fsck_vfat(&img);
    }
    let fs = open(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&*fs);
    let old_e = all
        .iter()
        .find(|e| e.deleted && e.path.ends_with("LD_0001.JPG"))
        .unwrap();
    let new_e = all
        .iter()
        .find(|e| e.deleted && e.path.ends_with("EW_0003.JPG"))
        .unwrap();
    for e in [old_e, new_e] {
        assert_eq!(
            e.confidence,
            EntryConfidence::DeletedGuessed,
            "{kind:?} {}: deleted files sharing clusters are not intact",
            e.path
        );
        assert!(
            e.flags.iter().any(|f| f.contains("also claimed")),
            "{kind:?} {}: no shared-cluster note in {:?}",
            e.path,
            e.flags
        );
    }
    let (_, rr) = read(&*fs, old_e);
    assert!(rr.notes.iter().any(|n| n.contains("also claimed")));
}

#[test]
fn reused_clusters_fat16() {
    reuse_after_delete(FsType::Fat16);
}

#[test]
fn reused_clusters_fat32() {
    reuse_after_delete(FsType::Fat32);
}

#[test]
fn reused_clusters_exfat() {
    reuse_after_delete(FsType::ExFat);
}

/// A directory with thousands of long-named files.
fn large_directory(kind: FsType, n: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("big.img");
    let size = match kind {
        FsType::Fat32 => 128 * MIB,
        _ => 128 * MIB,
    };
    let mut spec = FsSpec::new(kind, size).cluster_size(4096);
    for i in 0..n {
        spec.files.push(FileSpec::new(
            format!("DCIM/100CANON/Photo number {i:05} taken on holiday.jpg"),
            content(1 + (i as u64 % 7) * 1000, i as u64),
        ));
    }
    for i in (0..n).step_by(97) {
        spec.deleted.push(spec.files[i].path.clone());
    }
    let t0 = std::time::Instant::now();
    let rep = match build(&spec, &img) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    eprintln!("built {n} files in {:?}", t0.elapsed());
    if kind == FsType::ExFat {
        fsck_exfat(&img);
    } else {
        fsck_vfat(&img);
    }
    let t0 = std::time::Instant::now();
    let fs = open(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&*fs);
    eprintln!("walked {} entries in {:?}", all.len(), t0.elapsed());
    assert!(t0.elapsed().as_secs() < 60);
    let live: Vec<&FileEntry> = all.iter().filter(|e| !e.deleted && !e.is_dir).collect();
    let deleted: Vec<&FileEntry> = all.iter().filter(|e| e.deleted && !e.is_dir).collect();
    let want_deleted = spec.deleted.len();
    assert_eq!(live.len(), n - want_deleted);
    assert_eq!(deleted.len(), want_deleted, "every deleted file listed");
    let by: HashMap<&str, &FileEntry> = all.iter().map(|e| (e.path.as_str(), e)).collect();
    for (i, f) in spec.files.iter().enumerate() {
        let placed = rep.find(&f.path).unwrap();
        if placed.deleted {
            // Long names of deleted files are rebuilt from the LFN run.
            let e = by
                .get(f.path.as_str())
                .unwrap_or_else(|| panic!("{}", f.path));
            assert!(e.deleted);
            if i % 5 == 0 {
                assert_eq!(read(&*fs, e).0, f.data);
            }
            continue;
        }
        let e = by
            .get(f.path.as_str())
            .unwrap_or_else(|| panic!("{}", f.path));
        if i % 13 == 0 {
            assert_eq!(read(&*fs, e).0, f.data, "{}", f.path);
        }
    }
}

#[test]
fn large_directory_fat32() {
    large_directory(FsType::Fat32, 12_000);
}

#[test]
fn large_directory_exfat() {
    large_directory(FsType::ExFat, 12_000);
}

/// FAT32 with more than 65535 clusters, files above cluster 65535 deleted
/// the Windows way (high word of the first cluster zeroed).
#[test]
fn fat32_windows_delete_above_65535_clusters() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("w.img");
    let cs = 512u64;
    let mut spec = FsSpec::new(FsType::Fat32, 160 * MIB).cluster_size(cs as u32);
    spec.windows_delete = true;
    // Filler pushes the next files past cluster 65535 + a bit.
    spec.files
        .push(FileSpec::new("filler.bin", text_like(70_000 * 512, 9)));
    let photo = content(50_000, 10);
    let video = content(120_000, 11);
    let text = text_like(3000, 12);
    spec.files
        .push(FileSpec::new("DCIM/IMG_0100.JPG", photo.clone()));
    spec.files.push(FileSpec::new("DCIM/MVI_0101.MP4", {
        let mut v = video.clone();
        v[4..8].copy_from_slice(b"ftyp");
        v
    }));
    spec.files
        .push(FileSpec::new("DCIM/notes.txt", text.clone()));
    spec.files
        .push(FileSpec::new("DCIM/SUB/IMG_0200.JPG", content(9000, 13)));
    spec.deleted.push("DCIM/IMG_0100.JPG".into());
    spec.deleted.push("DCIM/MVI_0101.MP4".into());
    spec.deleted.push("DCIM/notes.txt".into());
    spec.deleted.push("DCIM/SUB".into());
    let rep = match build(&spec, &img) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    fsck_vfat(&img);
    let fs = open_fat(source(&img), &FatOptions::default()).unwrap();
    assert!(fs.cluster_count() > 65535);
    let all = entries(&fs);
    // Deleted 8.3-only names lose their first character.
    let get = |p: &str| {
        let (dir, name) = p.rsplit_once('/').unwrap();
        let (dir, name) = (dir.replace("/SUB", "/_UB"), &name[1..]);
        all.iter()
            .find(|e| e.path == format!("{dir}/_{name}"))
            .unwrap_or_else(|| panic!("{p} missing: {all:#?}"))
    };
    let e = get("DCIM/IMG_0100.JPG");
    assert_eq!(
        e.extents,
        extents_of(rep.find("DCIM/IMG_0100.JPG").unwrap())
    );
    assert_eq!(read(&fs, e).0, photo);
    let e = get("DCIM/MVI_0101.MP4");
    let mut v = video.clone();
    v[4..8].copy_from_slice(b"ftyp");
    assert_eq!(read(&fs, e).0, v);
    // A deleted directory above 65535 is found through its '.' entry.
    let e = get("DCIM/SUB/IMG_0200.JPG");
    assert_eq!(read(&fs, e).0, content(9000, 13));
    // A non-media file cannot be placed: never silently pretend.
    let e = get("DCIM/notes.txt");
    let truth = extents_of(rep.find("DCIM/notes.txt").unwrap());
    if e.extents != truth {
        assert!(
            e.flags
                .iter()
                .any(|f| f.contains("incomplete") || f.contains("high word")),
            "wrong guess without a note: {:?}",
            e.flags
        );
    }
}

/// FAT and exFAT on 4096-byte sectors (written by the crate's own test
/// writer, which reads the geometry from the boot sector).
#[test]
fn four_k_sectors() {
    let Some(mkfs) = need("mkfs.vfat") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    for (bits, size, spc) in [
        (16u32, 128 * MIB, 1u32),
        (32, 1024 * MIB, 1),
        (16, 512 * MIB, 4),
    ] {
        let img = tmp.path().join(format!("f{bits}_{spc}.img"));
        sparse(&img, size);
        let (ok, out) = run(
            &mkfs,
            &[
                "-F",
                &bits.to_string(),
                "-S",
                "4096",
                "-s",
                &spc.to_string(),
                img.to_str().unwrap(),
            ],
        );
        assert!(ok, "{out}");
        use support::fat_writer::{Alloc, Dir, FatImage};
        let mut w = FatImage::open(&img);
        assert_eq!(w.bps, 4096);
        let a = content(3 * w.cs() + 11, 1);
        let b = content(w.cs() * 7 + 1, 2);
        w.put(
            Dir::Root,
            "A photo with a long name.jpg",
            &a,
            Alloc::Contiguous,
        );
        let d = w.mkdir(Dir::Root, "DCIM");
        w.put(d, "frag.mp4", &b, Alloc::Fragmented);
        w.put(d, "gone.jpg", &a, Alloc::Contiguous);
        w.delete(d, "gone.jpg", false);
        drop(w);
        fsck_vfat(&img);
        let fs = open(source(&img), &FatOptions::default()).unwrap();
        let all = entries(&*fs);
        assert_eq!(
            read(&*fs, by_path(&all, "A photo with a long name.jpg")).0,
            a
        );
        assert_eq!(read(&*fs, by_path(&all, "DCIM/frag.mp4")).0, b);
        assert_eq!(read(&*fs, by_path(&all, "DCIM/_one.jpg")).0, a);
    }
    let Some(mkfs) = need("mkfs.exfat") else {
        return;
    };
    let img = tmp.path().join("ex4k.img");
    sparse(&img, 256 * MIB);
    let (ok, out) = run(&mkfs, &["-s", "4096", img.to_str().unwrap()]);
    assert!(ok, "{out}");
    use support::exfat_writer::{ExImage, ExMode};
    let mut w = ExImage::open(&img);
    assert_eq!(w.bps, 4096);
    let a = content(3 * w.cs + 11, 1);
    let root = w.root_dir();
    w.put(&root, "nofat.jpg", &a, ExMode::NoFatChain);
    w.put(&root, "chained.mp4", &a, ExMode::FragmentedChain);
    w.put(&root, "gone.jpg", &a, ExMode::NoFatChain);
    w.delete(&root, "gone.jpg");
    drop(w);
    fsck_exfat(&img);
    let fs = open_exfat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    assert_eq!(read(&fs, by_path(&all, "nofat.jpg")).0, a);
    assert_eq!(read(&fs, by_path(&all, "chained.mp4")).0, a);
    let g = by_path(&all, "gone.jpg");
    assert_eq!(g.confidence, EntryConfidence::DeletedIntact);
    assert_eq!(read(&fs, g).0, a);
}

/// Pin the time budget for an adversarial chain: descending every other
/// cluster must not go quadratic.
#[test]
fn descending_fragmented_chain_is_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("desc.img");
    if !mkfs_vfat(&img, 128 * MIB, 32, "DESC") {
        return;
    }
    use support::fat_writer::{Alloc, Dir, FatImage};
    let mut w = FatImage::open(&img);
    let cs = w.cs();
    let n: u32 = 100_000;
    let p = w.put(Dir::Root, "big.bin", &[1u8], Alloc::Contiguous);
    let first = p.clusters[0];
    // Rewrite the chain: first -> top, top-2, top-4, ... descending.
    let top = w.count;
    let mut chain = vec![first];
    let mut c = top;
    while chain.len() < n as usize {
        chain.push(c);
        c -= 2;
    }
    for i in 0..chain.len() {
        let next = if i + 1 < chain.len() {
            chain[i + 1]
        } else {
            w.eoc()
        };
        w.set(chain[i], next);
    }
    w.flush();
    // Patch the size so the whole chain is wanted.
    let size = (n as u64 * cs) as u32;
    let off = p.entry_offset + 28;
    drop(w);
    {
        use diskrescue_fixtures::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        f.write_all_at(&size.to_le_bytes(), off).unwrap();
    }
    let t0 = std::time::Instant::now();
    let fs = open_fat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let e = all
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case("big.bin"))
        .unwrap();
    assert_eq!(e.extent_bytes(), n as u64 * cs);
    assert_eq!(fs.chain(first).len(), n as usize);
    let _ = fs.unallocated().unwrap();
    assert!(
        t0.elapsed().as_secs() < 20,
        "descending chain took {:?}",
        t0.elapsed()
    );
}

/// An interrupted recording: DataLength covers the whole clip but
/// ValidDataLength stopped early. By default the tail reads as zeros
/// (reported as holes); with `read_past_valid_data_length` the bytes on
/// the medium come back, with a note.
#[test]
fn exfat_valid_data_length_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("vdl.img");
    if !mkfs_exfat(&img, 64 * MIB, "VDL") {
        return;
    }
    use support::exfat_writer::{ExImage, ExMode};
    let mut w = ExImage::open(&img);
    let clip = content(10 * w.cs + 123, 7);
    let vdl = 3 * w.cs + 5;
    let root = w.root_dir();
    w.put_vdl(&root, "MVI_0001.MP4", &clip, ExMode::NoFatChain, Some(vdl));
    drop(w);
    fsck_exfat(&img);
    let fs = open_exfat(source(&img), &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let e = by_path(&all, "MVI_0001.MP4");
    assert_eq!(e.size, clip.len() as u64);
    let (out, rep) = read(&fs, e);
    assert_eq!(&out[..vdl as usize], &clip[..vdl as usize]);
    assert!(out[vdl as usize..].iter().all(|&b| b == 0));
    assert_eq!(
        rep.holes,
        RangeSet::from_range(Range::new(vdl, clip.len() as u64))
    );
    let opts = FatOptions {
        read_past_valid_data_length: true,
        ..FatOptions::default()
    };
    let fs = open_exfat(source(&img), &opts).unwrap();
    let (out, rep) = read(&fs, e);
    assert_eq!(out, clip);
    assert!(rep.holes.is_empty());
    assert!(rep.notes.iter().any(|n| n.contains("ValidDataLength")));
}
