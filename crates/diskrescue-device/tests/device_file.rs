//! DeviceSource over regular files: the same code path as a block device
//! (workers, timeouts, alignment, re-attach), without touching hardware.

#![cfg(target_os = "linux")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Disk, FakeSys};
use diskrescue_core::device::DeviceKind;
use diskrescue_core::source::{ReadErrorKind, Source, SourceExt};
use diskrescue_core::Error;
use diskrescue_device::{open_any, open_device, sysfs, OpenOptions};

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + i / 512) % 256) as u8).collect()
}

fn buffered() -> OpenOptions {
    OpenOptions {
        direct: false,
        default_timeout: Duration::from_secs(10),
        allow_mounted: false,
        fallback_buffered: true,
    }
}

fn image(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p
}

#[test]
fn plain_file_reads_like_a_device() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(1 << 20);
    let p = image(dir.path(), "card.img", &data);
    let src = open_device(&p, &buffered()).unwrap();
    assert_eq!(src.size(), data.len() as u64);
    assert_eq!(src.sector_size(), 512);
    assert!(src.is_device());
    assert!(!src.requires_alignment());
    assert!(!src.direct_io());
    assert!(!src.is_block_device());
    assert_eq!(src.path(), Some(p.as_path()));
    assert_eq!(src.current_path(), p);
    let d = src.describe();
    assert!(d.contains("card.img") && d.contains("image file"), "{d}");
    assert_eq!(src.info().kind, DeviceKind::ImageFile);
    // Unaligned reads are fine in buffered mode.
    assert_eq!(src.read_exact_vec(3, 1000).unwrap(), data[3..1003].to_vec());
    let mut buf = vec![0u8; 8192];
    assert_eq!(src.read_at((1 << 20) - 100, &mut buf).unwrap(), 100);
    assert_eq!(src.read_at(1 << 20, &mut buf).unwrap(), 0);
    assert_eq!(src.read_at(u64::MAX, &mut buf).unwrap(), 0);
    assert_eq!(src.read_at(0, &mut []).unwrap(), 0);
    // The weak identity of a file gets its first-sector hash at open.
    let fp = src.fingerprint();
    assert_eq!(
        fp.first_sector_hash,
        Some(diskrescue_device::hash_first_sector(&data, 512))
    );
    assert_eq!(src.abandoned_workers(), 0);
    assert_eq!(src.reattach_count(), 0);
}

#[test]
fn direct_mode_on_a_file_chooses_a_valid_mode() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern((1 << 20) + 300);
    let p = image(dir.path(), "odd.img", &data);
    let opts = OpenOptions {
        direct: true,
        ..buffered()
    };
    let src = open_device(&p, &opts).unwrap();
    let sector = src.sector_size() as usize;
    if src.direct_io() {
        assert!(src.requires_alignment());
        let mut buf = vec![0u8; sector];
        let e = src.read_at(1, &mut buf).unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Unaligned);
        let mut odd = vec![0u8; sector + 1];
        assert_eq!(
            src.read_at(0, &mut odd).unwrap_err().kind,
            ReadErrorKind::Unaligned
        );
    } else {
        eprintln!("note: the temp filesystem refused O_DIRECT; buffered fallback chosen");
        assert!(src.info().warnings.iter().any(|w| w.contains("direct I/O")));
    }
    // Aligned reads work in either mode, including the short tail.
    let mut buf = vec![0u8; 4096];
    let n = src.read_at(1 << 20, &mut buf).unwrap();
    assert_eq!(n, 300);
    assert_eq!(&buf[..300], &data[1 << 20..]);
    let mut buf = vec![0u8; 64 * 1024];
    assert_eq!(src.read_at(65_536, &mut buf).unwrap(), 65_536);
    assert_eq!(&buf[..], &data[65_536..131_072]);
}

#[test]
fn concurrent_readers_get_correct_data() {
    let dir = tempfile::tempdir().unwrap();
    let data = Arc::new(pattern(4 << 20));
    let p = image(dir.path(), "big.img", &data);
    let src = Arc::new(open_device(&p, &buffered()).unwrap());
    let mut hs = Vec::new();
    for t in 0..6u64 {
        let s = src.clone();
        let d = data.clone();
        hs.push(thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            for i in 0..16u64 {
                let off = ((t * 16 + i) * 40_960) % ((4 << 20) - 65_536);
                let n = s.read_at(off, &mut buf).unwrap();
                assert_eq!(n, buf.len());
                assert_eq!(&buf[..], &d[off as usize..off as usize + n]);
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
}

#[test]
fn reopen_waits_for_the_file_to_come_back() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(256 * 1024);
    let p = image(dir.path(), "stick.img", &data);
    let src = open_device(&p, &buffered()).unwrap();
    src.set_reattach_deadline(Some(Duration::from_secs(5)));
    let away = dir.path().join("away.img");
    std::fs::rename(&p, &away).unwrap();
    let (p2, away2) = (p.clone(), away.clone());
    let mover = thread::spawn(move || {
        thread::sleep(Duration::from_millis(700));
        std::fs::rename(away2, p2).unwrap();
    });
    let t = Instant::now();
    src.reopen().unwrap();
    let took = t.elapsed();
    mover.join().unwrap();
    assert!(took >= Duration::from_millis(500), "{took:?}");
    assert!(took < Duration::from_secs(4), "{took:?}");
    assert_eq!(src.reattach_count(), 1);
    assert!(!src.is_gone());
    assert_eq!(
        src.read_exact_vec(1000, 100).unwrap(),
        data[1000..1100].to_vec()
    );
}

#[test]
fn reopen_gives_up_after_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let p = image(dir.path(), "gone.img", &pattern(8192));
    let src = open_device(&p, &buffered()).unwrap();
    src.set_reattach_deadline(Some(Duration::from_millis(600)));
    std::fs::remove_file(&p).unwrap();
    let t = Instant::now();
    let e = src.reopen().unwrap_err();
    assert_eq!(e.kind, ReadErrorKind::DeviceGone);
    assert!(e.detail.contains("did not come back"), "{}", e.detail);
    assert!(t.elapsed() >= Duration::from_millis(550));
    assert!(t.elapsed() < Duration::from_secs(3));
    // While gone, reads fail fast with DeviceGone.
    let mut b = vec![0u8; 512];
    assert_eq!(
        src.read_at(0, &mut b).unwrap_err().kind,
        ReadErrorKind::DeviceGone
    );
    assert!(src.is_gone());
}

#[test]
fn reopen_refuses_a_different_size_or_content() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(64 * 1024);
    let p = image(dir.path(), "card.img", &data);
    let src = open_device(&p, &buffered()).unwrap();
    src.set_reattach_deadline(Some(Duration::from_secs(5)));

    // Same name, different size: refused at once.
    std::fs::write(&p, pattern(32 * 1024)).unwrap();
    let t = Instant::now();
    let e = src.reopen().unwrap_err();
    assert_eq!(e.kind, ReadErrorKind::DeviceGone);
    assert!(e.detail.contains("instead of"), "{}", e.detail);
    assert!(t.elapsed() < Duration::from_secs(2));

    // Same size, different first sector: refused too.
    let mut other = data.clone();
    other[0] ^= 0xFF;
    std::fs::write(&p, &other).unwrap();
    let e = src.reopen().unwrap_err();
    assert!(e.detail.contains("first sector"), "{}", e.detail);

    // The original content comes back: accepted.
    std::fs::write(&p, &data).unwrap();
    src.reopen().unwrap();
    assert_eq!(src.read_exact_vec(0, 16).unwrap(), data[..16].to_vec());
}

#[test]
fn reopen_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let p = image(dir.path(), "c.img", &pattern(4096));
    let src = open_device(&p, &buffered()).unwrap();
    src.set_reattach_deadline(None);
    let flag = Arc::new(AtomicBool::new(false));
    src.set_cancel_flag(flag.clone());
    std::fs::remove_file(&p).unwrap();
    let f2 = flag.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        f2.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let t = Instant::now();
    let e = src.reopen().unwrap_err();
    canceller.join().unwrap();
    assert!(e.detail.contains("cancelled"), "{}", e.detail);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn open_any_and_error_cases() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(10_000);
    let p = image(dir.path(), "x.img", &data);
    let (src, info) = open_any(&p, &OpenOptions::default()).unwrap();
    assert!(!src.is_device(), "image files become a plain FileSource");
    assert_eq!(info.kind, DeviceKind::ImageFile);
    assert_eq!(info.size, 10_000);
    assert!(info.warnings.iter().any(|w| w.contains("multiple of 512")));
    assert_eq!(
        src.read_exact_vec(9_990, 10).unwrap(),
        data[9_990..].to_vec()
    );

    assert!(matches!(
        open_device(&dir.path().join("missing.img"), &buffered()),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        open_device(dir.path(), &buffered()),
        Err(Error::InvalidArgument(_))
    ));
    let empty = image(dir.path(), "empty.img", &[]);
    let s = open_device(&empty, &buffered()).unwrap();
    assert_eq!(s.size(), 0);
    let mut b = [0u8; 16];
    assert_eq!(s.read_at(0, &mut b).unwrap(), 0);
}

#[test]
fn loop_mounted_image_is_refused_unless_allowed() {
    let fs = FakeSys::new();
    let img = fs.base().join("photos.img");
    std::fs::write(&img, pattern(8192)).unwrap();
    let canon = std::fs::canonicalize(&img).unwrap();
    let lp = fs.add_disk(&Disk::virt("loop3", (7, 3), 16));
    fs.write(
        &format!("{lp}/loop/backing_file"),
        format!("{}\n", canon.display()),
    );
    fs.mountinfo("30 1 7:3 / /mnt/img rw - vfat /dev/loop3 rw\n");

    assert_eq!(
        sysfs::mountpoints_of_with(&fs.roots, &img).unwrap(),
        vec![PathBuf::from("/mnt/img")]
    );
    let info = sysfs::describe_with(&fs.roots, &img).unwrap();
    assert!(info.is_mounted());
    assert!(info.warnings.iter().any(|w| w.contains("loop-mounted")));
    assert!(sysfs::is_mounted_with(&fs.roots, &info));
    match sysfs::open_device_with(&fs.roots, &img, &buffered()) {
        Err(Error::Refused(m)) => assert!(m.contains("mounted"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let allowed = OpenOptions {
        allow_mounted: true,
        ..buffered()
    };
    let src = sysfs::open_device_with(&fs.roots, &img, &allowed).unwrap();
    assert_eq!(src.size(), 8192);

    // Not mounted any more: opens normally.
    fs.mountinfo("");
    assert!(sysfs::open_device_with(&fs.roots, &img, &buffered()).is_ok());
    assert!(sysfs::mountpoints_of_with(&fs.roots, &img)
        .unwrap()
        .is_empty());
}

#[test]
fn concurrent_reopens_reattach_once() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern(16 * 1024);
    let p = image(dir.path(), "dup.img", &data);
    let src = Arc::new(open_device(&p, &buffered()).unwrap());
    src.set_reattach_deadline(Some(Duration::from_secs(5)));
    let hs: Vec<_> = (0..4)
        .map(|_| {
            let s = src.clone();
            thread::spawn(move || s.reopen())
        })
        .collect();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    assert!(src.reattach_count() >= 1 && src.reattach_count() <= 4);
    assert!(!src.is_gone());
    assert_eq!(
        src.read_exact_vec(100, 10).unwrap(),
        data[100..110].to_vec()
    );
}
