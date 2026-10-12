//! Review regression tests: sources that fail mid-read (I/O errors,
//! timeouts, a device that disappears, short reads), truncation at every
//! sector boundary, and a FAT1 sector that reads back as 0xFF.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::fs::{FileEntry, FileSystem, WalkControl};
use diskrescue_core::source::{MemSource, ReadError, ReadErrorKind, Source};
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_fat::{open, open_exfat, open_fat, FatOptions};
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use support::*;

const MIB: u64 = 1024 * 1024;

#[derive(Clone)]
enum Mode {
    /// Return at most this many bytes per call.
    Short(usize),
    /// Reads touching the range fail with this kind (good prefix first).
    Fail(Range, ReadErrorKind),
    /// After this many calls every read fails with DeviceGone.
    GoneAfter(u64),
}

struct Flaky {
    data: Arc<Vec<u8>>,
    mode: Mode,
    calls: AtomicU64,
}

impl Flaky {
    fn shared(data: Arc<Vec<u8>>, mode: Mode) -> SharedSource {
        Arc::new(Flaky {
            data,
            mode,
            calls: AtomicU64::new(0),
        })
    }
}

impl Source for Flaky {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let n_call = self.calls.fetch_add(1, Ordering::Relaxed);
        let size = self.data.len() as u64;
        if offset >= size {
            return Ok(0);
        }
        let mut n = ((size - offset).min(buf.len() as u64)) as usize;
        match &self.mode {
            Mode::Short(k) => n = n.min(*k),
            Mode::Fail(r, kind) => {
                let want = Range::new(offset, offset + n as u64);
                if want.overlaps(*r) {
                    if offset < r.start {
                        n = (r.start - offset) as usize;
                    } else {
                        return Err(ReadError::new(offset, n, *kind, "flaky"));
                    }
                }
            }
            Mode::GoneAfter(k) => {
                if n_call >= *k {
                    return Err(ReadError::new(offset, n, ReadErrorKind::DeviceGone, "gone"));
                }
            }
        }
        buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "flaky".into()
    }
}

/// A FAT16 or exFAT image from the fixtures with a few files.
/// Path and content of each file written.
type Files = Vec<(String, Vec<u8>)>;

fn image(kind: FsType) -> Option<(Vec<u8>, Files)> {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("v.img");
    let mut spec = FsSpec::new(kind, 32 * MIB).cluster_size(2048);
    let files = vec![
        (
            "DCIM/100TEST/IMG_0001.JPG".to_string(),
            jpeg_like(70_000, 1),
        ),
        (
            "DCIM/100TEST/Long name photo.jpg".to_string(),
            jpeg_like(9_000, 2),
        ),
        ("video.mp4".to_string(), jpeg_like(300_000, 3)),
        ("gone.jpg".to_string(), jpeg_like(20_000, 4)),
    ];
    for (p, d) in &files {
        spec.files.push(FileSpec::new(p.clone(), d.clone()));
    }
    spec.deleted.push("gone.jpg".into());
    if let Err(e) = build(&spec, &img) {
        eprintln!("skipped: {e}");
        return None;
    }
    Some((std::fs::read(&img).unwrap(), files))
}

fn listing(fs: &dyn FileSystem) -> Vec<FileEntry> {
    let mut v = Vec::new();
    fs.walk(&mut |e| {
        v.push(e.clone());
        WalkControl::Continue
    })
    .unwrap();
    v
}

fn read_all(fs: &dyn FileSystem, e: &FileEntry) -> (Vec<u8>, diskrescue_core::fs::FileReadReport) {
    let mut out = Vec::new();
    let r = fs.read_file(e, &mut out).unwrap();
    assert_eq!(out.len() as u64, e.size);
    (out, r)
}

#[test]
fn short_reads_change_nothing() {
    for kind in [FsType::Fat16, FsType::ExFat] {
        let Some((bytes, _)) = image(kind) else {
            return;
        };
        let data = Arc::new(bytes);
        let clean: SharedSource = Arc::new(MemSource::new((*data).clone()));
        let fs0 = open(clean, &FatOptions::default()).unwrap();
        let want = listing(&*fs0);
        for k in [1usize, 7, 512, 513, 4095] {
            let fs = open(
                Flaky::shared(data.clone(), Mode::Short(k)),
                &FatOptions::default(),
            )
            .unwrap();
            let got = listing(&*fs);
            assert_eq!(got, want, "{kind:?} short {k}");
            assert_eq!(fs.unallocated().unwrap(), fs0.unallocated().unwrap());
            for e in got.iter().filter(|e| !e.is_dir) {
                let a = read_all(&*fs, e);
                let b = read_all(&*fs0, e);
                assert_eq!(a.0, b.0, "{}", e.path);
                assert_eq!(a.1, b.1, "{}", e.path);
            }
        }
    }
}

#[test]
fn failing_ranges_inside_file_data() {
    for kind in [FsType::Fat16, FsType::ExFat] {
        let Some((bytes, files)) = image(kind) else {
            return;
        };
        let data = Arc::new(bytes);
        let fs0 = open(
            Arc::new(MemSource::new((*data).clone())),
            &FatOptions::default(),
        )
        .unwrap();
        let all = listing(&*fs0);
        let e = all.iter().find(|e| e.path == "video.mp4").unwrap();
        let truth = &files.iter().find(|f| f.0 == "video.mp4").unwrap().1;
        // A bad range straddling a cluster boundary, not sector aligned.
        let base = e.extents[0].start;
        let bad = Range::new(base + 4000, base + 9000);
        for kind_err in [
            ReadErrorKind::Io,
            ReadErrorKind::Timeout,
            ReadErrorKind::OutOfRange,
        ] {
            let src = Flaky::shared(data.clone(), Mode::Fail(bad, kind_err));
            let fs = open(src, &FatOptions::default()).unwrap();
            let (out, rep) = read_all(&*fs, e);
            // The good prefix the source returned is kept; from the first
            // failing byte to the end of the last failing sector is a hole.
            let want = Range::new(4000, 9000_u64.div_ceil(512) * 512);
            assert_eq!(
                rep.unreadable,
                RangeSet::from_range(want),
                "{kind:?} {kind_err:?}"
            );
            assert!(!rep.is_complete());
            for (i, (&a, &b)) in out.iter().zip(truth.iter()).enumerate() {
                if want.contains(i as u64) {
                    assert_eq!(a, 0);
                } else {
                    assert_eq!(a, b, "byte {i}");
                }
            }
        }
    }
}

#[test]
fn device_gone_at_every_point_never_panics() {
    for kind in [FsType::Fat16, FsType::ExFat] {
        let Some((bytes, _)) = image(kind) else {
            return;
        };
        let data = Arc::new(bytes);
        let t0 = Instant::now();
        for k in 0..400u64 {
            let src = Flaky::shared(data.clone(), Mode::GoneAfter(k));
            let Ok(fs) = open(src, &FatOptions::default()) else {
                continue;
            };
            let all = listing(&*fs);
            for e in all.iter().filter(|e| !e.is_dir) {
                let mut out = Vec::new();
                if let Ok(rep) = fs.read_file(e, &mut out) {
                    assert_eq!(out.len() as u64, e.size);
                    // Once the device is gone nothing more can be read; a
                    // non-empty file must report its holes.
                    if e.size > 0 && k < 20 {
                        assert!(!rep.unreadable.is_empty() || !rep.holes.is_empty());
                    }
                }
            }
            let _ = fs.unallocated();
        }
        assert!(t0.elapsed() < Duration::from_secs(120));
    }
}

#[test]
fn truncation_at_every_sector() {
    for kind in [FsType::Fat12, FsType::Fat16, FsType::ExFat] {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("t.img");
        let size = if kind == FsType::Fat12 {
            2 * MIB
        } else {
            8 * MIB
        };
        let mut spec = FsSpec::new(kind, size).cluster_size(1024);
        spec.files
            .push(FileSpec::new("DCIM/A.JPG", jpeg_like(30_000, 1)));
        spec.files
            .push(FileSpec::new("Long named file.mov", jpeg_like(5_000, 2)));
        spec.files
            .push(FileSpec::new("del.jpg", jpeg_like(3_000, 3)));
        spec.deleted.push("del.jpg".into());
        if let Err(e) = build(&spec, &img) {
            eprintln!("skipped: {e}");
            return;
        }
        let bytes = std::fs::read(&img).unwrap();
        // Every sector boundary over the metadata area, plus odd lengths.
        let mut cuts: Vec<usize> = (0..=(700 * 1024)).step_by(512).collect();
        cuts.extend([1, 3, 511, 513, 1000, 4097, 65_535, 65_537]);
        let t0 = Instant::now();
        for cut in cuts {
            let cut = cut.min(bytes.len());
            let src: SharedSource = Arc::new(MemSource::new(bytes[..cut].to_vec()));
            for fs in [
                open(src.clone(), &FatOptions::default()),
                open_fat(src.clone(), &FatOptions::default())
                    .map(|f| Box::new(f) as Box<dyn FileSystem>),
                open_exfat(src.clone(), &FatOptions::default())
                    .map(|f| Box::new(f) as Box<dyn FileSystem>),
            ]
            .into_iter()
            .flatten()
            {
                let all = listing(&*fs);
                for e in all.iter().filter(|e| !e.is_dir) {
                    let mut out = Vec::new();
                    if let Ok(rep) = fs.read_file(e, &mut out) {
                        assert_eq!(out.len() as u64, e.size);
                        // Bytes past the cut are never reported as read.
                        let mut pos = 0u64;
                        for x in &e.extents {
                            let take = x.len().min(e.size.saturating_sub(pos));
                            if x.end > cut as u64 && take > 0 {
                                let past = x.start.max(cut as u64);
                                let rel = pos + (past - x.start);
                                let end = pos + take;
                                if rel < end {
                                    assert!(
                                        rep.unreadable.covers(Range::new(rel, end)),
                                        "{kind:?} cut {cut}: {} {rep:?}",
                                        e.path
                                    );
                                }
                            }
                            pos += take;
                        }
                    }
                }
                let un = fs.unallocated().unwrap();
                assert!(un.iter().all(|r| r.end <= fs.info().total_size));
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(300), "{kind:?}");
    }
}

/// A FAT1 sector that reads back as all 0xFF (unprogrammed NAND page
/// exposed by a failing controller) must not truncate every chain in it
/// when FAT2 still holds the real values.
#[test]
fn ff_filled_fat1_sector_uses_fat2() {
    let Some((mut bytes, files)) = image(FsType::Fat16) else {
        return;
    };
    let fs0 = open_fat(
        Arc::new(MemSource::new(bytes.clone())),
        &FatOptions::default(),
    )
    .unwrap();
    let rsvd = fs0.boot_sector().bpb_rsvd_sec_cnt as usize * 512;
    // Overwrite the first FAT1 sector (clusters 0..255) with 0xFF.
    bytes[rsvd..rsvd + 512].fill(0xFF);
    let fs = open_fat(Arc::new(MemSource::new(bytes)), &FatOptions::default()).unwrap();
    let all = listing(&fs);
    for (p, d) in &files {
        if p == "gone.jpg" {
            continue;
        }
        let e = all
            .iter()
            .find(|e| &e.path == p)
            .unwrap_or_else(|| panic!("{p}"));
        let (out, rep) = read_all(&fs, e);
        assert!(rep.is_complete(), "{p}: {rep:?}");
        assert_eq!(&out, d, "{p}");
    }
    assert_eq!(fs.unallocated().unwrap(), fs0.unallocated().unwrap());
}

/// 300 nested directories (deeper than the 256-level limit) on FAT16 and
/// exFAT, walked on a thread with a 2 MiB stack (the default for spawned
/// threads), with deleted levels so both walk passes recurse.
#[test]
fn deep_nesting_on_a_small_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let fat = tmp.path().join("deep.img");
    let ex = tmp.path().join("deepx.img");
    let have_fat = mkfs_vfat(&fat, 32 * MIB, 16, "DEEP");
    let have_ex = mkfs_exfat(&ex, 64 * MIB, "DEEP");
    if have_fat {
        use support::fat_writer::{Alloc, Dir, FatImage};
        let mut w = FatImage::open(&fat);
        let mut d = Dir::Root;
        for i in 0..300 {
            d = w.mkdir(d, &format!("level {i:03}"));
            if i % 50 == 49 {
                w.put(d, "gone.jpg", &jpeg_like(3000, i), Alloc::Contiguous);
                w.delete(d, "gone.jpg", false);
            }
        }
        w.put(d, "bottom.txt", b"bottom", Alloc::Contiguous);
        drop(w);
    }
    if have_ex {
        use support::exfat_writer::{ExImage, ExMode};
        let mut w = ExImage::open(&ex);
        let mut d = w.root_dir();
        for i in 0..300 {
            d = w.mkdir(&d, &format!("level {i:03}"));
            if i % 50 == 49 {
                w.put(&d, "gone.jpg", &jpeg_like(3000, i), ExMode::NoFatChain);
                w.delete(&d, "gone.jpg");
            }
        }
        drop(w);
    }
    let imgs: Vec<std::path::PathBuf> = [(have_fat, fat), (have_ex, ex)]
        .into_iter()
        .filter(|x| x.0)
        .map(|x| x.1)
        .collect();
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            for img in imgs {
                let fs = open(source(&img), &FatOptions::default()).unwrap();
                let all = listing(&*fs);
                let depth = all
                    .iter()
                    .map(|e| e.path.matches('/').count())
                    .max()
                    .unwrap();
                assert!((250..=256).contains(&depth), "depth {depth}");
                assert!(all.iter().filter(|e| e.deleted).count() >= 4);
            }
        })
        .unwrap();
    h.join().expect("deep walk overflowed or panicked");
}
