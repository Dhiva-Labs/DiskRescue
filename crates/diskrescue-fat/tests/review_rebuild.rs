//! FAT boot sector reconstruction (filesystems spec 2.2.10): "the card
//! needs formatting" volumes whose sector 0 is gone must still give names
//! and exact file contents, and nothing that is not a two-FAT FAT volume may
//! be "reconstructed" into one.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use diskrescue_core::fs::{EntryConfidence, FsKind};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, SharedSource};
use diskrescue_fat::{open, open_fat, FatOptions};
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use support::*;

const MIB: u64 = 1024 * 1024;

/// A camera-like volume from the fixtures: DCIM tree, a few photos and a
/// video, one deleted photo.
fn camera_spec(kind: FsType, size: u64, cs: Option<u32>) -> FsSpec {
    let mut s = FsSpec::new(kind, size);
    s.cluster_size = cs;
    s.label = Some("CANON_DC".into());
    let c = cs.unwrap_or(4096) as usize;
    s.files.push(FileSpec::new(
        "DCIM/100CANON/IMG_0001.JPG",
        jpeg_like(3 * c + 100, 1),
    ));
    s.files.push(FileSpec::new(
        "DCIM/100CANON/IMG_0002.JPG",
        jpeg_like(5 * c + 7, 2),
    ));
    s.files.push(FileSpec::new(
        "DCIM/100CANON/MVI_0003.MOV",
        jpeg_like(20 * c + 1, 3),
    ));
    s.files
        .push(FileSpec::new("DCIM/100CANON/IMG_0004.JPG", jpeg_like(c, 4)));
    s.files
        .push(FileSpec::new("MISC/AUTPRINT.MRK", text_like(300, 5)));
    s.deleted.push("DCIM/100CANON/IMG_0004.JPG".into());
    s
}

/// Build, destroy the boot sector(s), reopen and compare everything with
/// the intact volume.
fn check_rebuild(spec: &FsSpec, destroy: &[u64], garbage: bool, extra_tail: u64) -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("cam.img");
    let rep = match build(spec, &img) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: {e}");
            return false;
        }
    };
    let bytes = std::fs::read(&img).unwrap();
    let intact: SharedSource = Arc::new(MemSource::new(bytes.clone()));
    let fs0 = open(intact, &FatOptions::default()).unwrap();
    let want: HashMap<String, _> = entries(&*fs0)
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut broken = bytes.clone();
    let mut r = Rng::new(77);
    for &s in destroy {
        let at = (s * 512) as usize;
        if garbage {
            broken[at..at + 512].copy_from_slice(&r.bytes(512));
        } else {
            broken[at..at + 512].fill(0);
        }
    }
    broken.extend(std::iter::repeat_n(0u8, extra_tail as usize));
    let src: SharedSource = Arc::new(MemSource::new(broken));
    let fs = open(src.clone(), &FatOptions::default()).unwrap_or_else(|e| {
        panic!(
            "{:?} cs={:?}: not reconstructed: {e}",
            spec.kind, spec.cluster_size
        )
    });
    let info = fs.info();
    assert_eq!(info.kind, fs0.info().kind);
    assert_eq!(info.cluster_size, fs0.info().cluster_size);
    assert_eq!(info.data_start, fs0.info().data_start);
    assert_eq!(info.data_start, rep.data_start);
    assert!(
        info.warnings.iter().any(|w| w.contains("reconstructed")),
        "{:?}",
        info.warnings
    );
    assert_eq!(
        info.label.as_deref(),
        Some("CANON_DC"),
        "label from the root entry"
    );
    let got = entries(&*fs);
    assert_eq!(got.len(), want.len(), "{got:#?}");
    for e in &got {
        let w = want
            .get(&e.path)
            .unwrap_or_else(|| panic!("unexpected {}", e.path));
        assert_eq!(e.extents, w.extents, "{}", e.path);
        assert_eq!(e.deleted, w.deleted);
        if e.is_dir {
            continue;
        }
        let (a, ra) = read(&*fs, e);
        let (b, _) = read(&*fs0, w);
        assert_eq!(a, b, "{}", e.path);
        if !e.deleted {
            assert!(ra.is_complete());
            let truth = spec.files.iter().find(|f| f.path == e.path).unwrap();
            assert_eq!(a, truth.data, "{}", e.path);
        } else {
            assert_eq!(e.confidence, EntryConfidence::DeletedGuessed);
        }
    }
    // The exact sector count died with the boot sector (mkfs.fat even
    // rounds it down to whole tracks), so the rebuilt volume may run a few
    // clusters further, up to what the source and the FAT allow; inside the
    // original volume the free space must be identical.
    let un0 = fs0.unallocated().unwrap();
    let un = fs.unallocated().unwrap();
    let f0 = open_fat(
        Arc::new(MemSource::new(bytes.clone())) as SharedSource,
        &FatOptions::default(),
    )
    .unwrap();
    let end0 = f0.cluster_to_offset(f0.cluster_count() + 2);
    assert_eq!(
        un.intersection_with_range(Range::new(0, end0)),
        un0,
        "unallocated space"
    );
    let extra = un.intersection_with_range(Range::new(end0, u64::MAX));
    assert!(
        extra.total_len() <= extra_tail + 64 * 512 + 2 * info.cluster_size as u64,
        "{extra:?}"
    );
    // open_fat directly reconstructs too.
    assert!(open_fat(src, &FatOptions::default()).is_ok());
    true
}

#[test]
fn fat16_card_without_boot_sector() {
    for cs in [512u32, 2048, 4096, 16384, 32768] {
        let size = 20_000 * cs as u64;
        if !check_rebuild(&camera_spec(FsType::Fat16, size, Some(cs)), &[0], false, 0) {
            return;
        }
    }
}

#[test]
fn fat16_card_with_garbage_boot_sector() {
    check_rebuild(
        &camera_spec(FsType::Fat16, 64 * MIB, Some(2048)),
        &[0],
        true,
        0,
    );
}

#[test]
fn fat16_partition_larger_than_volume() {
    check_rebuild(
        &camera_spec(FsType::Fat16, 32 * MIB, Some(2048)),
        &[0],
        false,
        3 * MIB,
    );
}

#[test]
fn fat12_card_without_boot_sector() {
    for (size, cs) in [(2 * MIB, 512u32), (8 * MIB, 2048), (16 * MIB, 4096)] {
        if !check_rebuild(&camera_spec(FsType::Fat12, size, Some(cs)), &[0], false, 0) {
            return;
        }
    }
}

#[test]
fn fat32_without_boot_sector_and_backup() {
    check_rebuild(
        &camera_spec(FsType::Fat32, 80_000 * 4096, Some(4096)),
        &[0, 6],
        false,
        0,
    );
}

/// Unusual formatter choices: many reserved sectors, a 224-entry root, a
/// 112-entry root (floppy style), root files only (no subdirectory).
#[test]
fn unusual_layouts() {
    let Some(mkfs) = need("mkfs.vfat") else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    for (bits, size, args, subdir) in [
        (
            16u32,
            48 * MIB,
            vec!["-R", "64", "-r", "224", "-s", "8"],
            true,
        ),
        (12, 3 * MIB, vec!["-r", "112", "-s", "4"], true),
        (16, 32 * MIB, vec!["-R", "1", "-s", "4"], false),
        (12, 2 * MIB, vec!["-s", "1"], false),
    ] {
        let img = tmp.path().join(format!("u{bits}.img"));
        sparse(&img, size);
        let mut a: Vec<&str> = vec!["-F", if bits == 12 { "12" } else { "16" }];
        a.extend(args.iter().copied());
        a.push(img.to_str().unwrap());
        let (ok, out) = run(&mkfs, &a);
        assert!(ok, "{out}");
        use support::fat_writer::{Alloc, Dir, FatImage};
        let mut w = FatImage::open(&img);
        let cs = w.cs() as usize;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let d = if subdir {
            let d = w.mkdir(Dir::Root, "DCIM");
            Some(d)
        } else {
            None
        };
        for i in 0..4u64 {
            let name = format!("DSC{i:05}.JPG");
            let data = jpeg_like(cs * (i as usize + 1) + 33, i + 10);
            let dir = d.unwrap_or(Dir::Root);
            w.put(dir, &name, &data, Alloc::Contiguous);
            let p = if subdir {
                format!("DCIM/{name}")
            } else {
                name.clone()
            };
            files.push((p, data));
        }
        drop(w);
        fsck_vfat(&img);
        let mut bytes = std::fs::read(&img).unwrap();
        bytes[..512].fill(0);
        let fs = open(Arc::new(MemSource::new(bytes)), &FatOptions::default())
            .unwrap_or_else(|e| panic!("FAT{bits} {args:?}: {e}"));
        assert_eq!(
            fs.info().kind,
            if bits == 12 {
                FsKind::Fat12
            } else {
                FsKind::Fat16
            }
        );
        let all = entries(&*fs);
        for (p, data) in &files {
            let e = by_path(&all, p);
            assert_eq!(&read(&*fs, e).0, data, "FAT{bits} {args:?} {p}");
        }
    }
}

/// Nothing that is not a two-FAT FAT volume is turned into one.
#[test]
fn no_false_reconstruction() {
    // Zeros, noise, and noise with a lone FAT signature.
    let zeros: SharedSource = Arc::new(MemSource::new(vec![0u8; 4 * MIB as usize]));
    assert!(open(zeros, &FatOptions::default()).is_err());
    let mut noise = Rng::new(5).bytes(4 * MIB as usize);
    assert!(open(
        Arc::new(MemSource::new(noise.clone())) as SharedSource,
        &FatOptions::default()
    )
    .is_err());
    noise[512..515].copy_from_slice(&[0xF8, 0xFF, 0xFF]);
    noise[512 * 40..512 * 40 + 3].copy_from_slice(&[0xF8, 0xFF, 0xFF]);
    assert!(open(
        Arc::new(MemSource::new(noise)) as SharedSource,
        &FatOptions::default()
    )
    .is_err());
    let tmp = tempfile::tempdir().unwrap();
    // exFAT with both boot regions gone: one FAT only, never "FAT16".
    let img = tmp.path().join("x.img");
    let mut spec = FsSpec::new(FsType::ExFat, 64 * MIB);
    spec.files.push(FileSpec::new(
        "DCIM/100CANON/IMG_0001.JPG",
        jpeg_like(9000, 1),
    ));
    if build(&spec, &img).is_ok() {
        let mut b = std::fs::read(&img).unwrap();
        b[..24 * 512].fill(0);
        let src: SharedSource = Arc::new(MemSource::new(b));
        assert!(open(src, &FatOptions::default()).is_err());
    }
    // A FAT16 volume with a single FAT cannot be told apart reliably.
    if let Some(mkfs) = need("mkfs.vfat") {
        let img = tmp.path().join("one.img");
        sparse(&img, 32 * MIB);
        let (ok, out) = run(&mkfs, &["-F", "16", "-f", "1", img.to_str().unwrap()]);
        assert!(ok, "{out}");
        let mut b = std::fs::read(&img).unwrap();
        b[..512].fill(0);
        let src: SharedSource = Arc::new(MemSource::new(b));
        assert!(open(src, &FatOptions::default()).is_err());
    }
}

/// Thousands of sectors that start like a FAT (identical or not) must not
/// make the reconstruction search slow.
#[test]
fn signature_flood_is_bounded() {
    let t0 = std::time::Instant::now();
    let mut r = Rng::new(9);
    let mut a = r.bytes(24 * MIB as usize);
    for s in 1..(a.len() / 512) {
        a[s * 512..s * 512 + 4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]);
    }
    let _ = open(
        Arc::new(MemSource::new(a)) as SharedSource,
        &FatOptions::default(),
    );
    // All identical: every pair is an exact FAT copy candidate.
    let mut b = vec![0u8; 24 * MIB as usize];
    for s in 1..(b.len() / 512) {
        b[s * 512..s * 512 + 4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]);
    }
    let _ = open(
        Arc::new(MemSource::new(b)) as SharedSource,
        &FatOptions::default(),
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        t0.elapsed()
    );
}
