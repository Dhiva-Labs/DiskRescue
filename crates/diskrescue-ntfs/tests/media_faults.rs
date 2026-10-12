//! Media faults on real mkntfs volumes: sources that time out, vanish or
//! return short reads, damaged MFT records in the middle of the walk, torn
//! 4 KiB records, and a primary boot sector that validates but points at
//! the wrong MFT.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use diskrescue_core::fs::{collect_entries, FileSystem};
use diskrescue_core::source::{MemSource, ReadError, ReadErrorKind, Source};
use diskrescue_core::{Range, SharedSource};
use diskrescue_ntfs::walk::split_id;
use diskrescue_ntfs::{open, NtfsFs, NtfsOptions};

/// An image populated by ntfscp; returns bytes and contents by name.
fn image(dir: &Path, n: u64, cluster: u32, extra: &[&str]) -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
    let img = dir.join(format!("faults{cluster}{}.img", extra.join("")));
    mkntfs(&img, 16, cluster, "FAULTS", extra);
    let mut files = BTreeMap::new();
    for i in 0..n {
        let size = [300usize, 5000, 70_000, 200_001][i as usize % 4];
        let name = format!("f{i:03}.bin");
        let bytes = data(5000 + i, size);
        let p = dir.join(&name);
        write(&p, &bytes);
        ntfscp(&img, &p, &format!("/{name}"));
        files.insert(name, bytes);
    }
    (std::fs::read(&img).unwrap(), files)
}

fn mft_record_offset(fs: &NtfsFs, n: u64) -> u64 {
    let first = fs.mft_extents()[0];
    let off = first.start + n * fs.record_size();
    assert!(off + fs.record_size() <= first.end);
    off
}

/// A source that fails reads touching `bad` with `kind`, and can return
/// deliberately short reads.
struct Faulty {
    inner: MemSource,
    bad: Range,
    kind: ReadErrorKind,
    short: bool,
    calls: AtomicU64,
}

impl Source for Faulty {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let c = self.calls.fetch_add(1, Ordering::Relaxed);
        let len = buf.len() as u64;
        let r = Range::new(offset, offset.saturating_add(len));
        if let Some(hit) = r.intersect(self.bad) {
            if hit.start == offset {
                return Err(ReadError::new(offset, buf.len(), self.kind, "simulated"));
            }
            let good = (hit.start - offset) as usize;
            return self.inner.read_at(offset, &mut buf[..good]);
        }
        if self.short && buf.len() > 1 {
            // Pseudo-random short reads of 1..=len bytes.
            let x = (c.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) as usize;
            let n = 1 + x % buf.len();
            return self.inner.read_at(offset, &mut buf[..n]);
        }
        self.inner.read_at(offset, buf)
    }
    fn describe(&self) -> String {
        "faulty".into()
    }
}

fn faulty(img: &[u8], bad: Range, kind: ReadErrorKind, short: bool) -> SharedSource {
    Arc::new(Faulty {
        inner: MemSource::new(img.to_vec()),
        bad,
        kind,
        short,
        calls: AtomicU64::new(0),
    })
}

#[test]
fn timeouts_short_reads_and_a_vanishing_device() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = image(tmp.path(), 12, 4096, &[]);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let clean_all = entries(&clean);
    let victim = find(&clean_all, "f005.bin").clone();
    let off = mft_record_offset(&clean, split_id(victim.id).0);

    // Short reads everywhere: identical results.
    let fs = open(
        faulty(&img, Range::new(0, 0), ReadErrorKind::Io, true),
        &NtfsOptions::default(),
    )
    .unwrap();
    let all = entries(&fs);
    assert_eq!(all, clean_all);
    for (name, bytes) in &files {
        let (got, rep) = read(&fs, find(&all, name));
        assert_eq!(&got, bytes, "{name}");
        assert!(rep.is_complete());
    }

    // A timing-out sector inside one MFT record: that record is skipped,
    // everything else is intact.
    let fs = open(
        faulty(
            &img,
            Range::new(off + 512, off + 1024),
            ReadErrorKind::Timeout,
            false,
        ),
        &NtfsOptions::default(),
    )
    .unwrap();
    let all = entries(&fs);
    for (name, bytes) in &files {
        let e = find(&all, name);
        if name == "f005.bin" {
            assert!(fs.read_file(e, &mut Vec::new()).is_err());
            continue;
        }
        assert_eq!(&read(&fs, e).0, bytes, "{name}");
    }

    // The device vanishing while the MFT is walked is an error, never a
    // silently shortened listing.
    let fs = open(
        faulty(
            &img,
            Range::new(off, off + 512),
            ReadErrorKind::DeviceGone,
            false,
        ),
        &NtfsOptions::default(),
    );
    match fs {
        Err(e) => assert!(e.is_read_error(), "{e}"),
        Ok(fs) => {
            let r = collect_entries(&fs);
            assert!(r.is_err(), "walk must fail when the device is gone");
        }
    }
    // ... and while a file is read, the read fails.
    let ext = victim.extents.first().copied();
    if let Some(ext) = ext {
        let src = faulty(
            &img,
            Range::new(ext.start + 4096, ext.start + 8192),
            ReadErrorKind::DeviceGone,
            false,
        );
        let fs = open(src, &NtfsOptions::default()).unwrap();
        assert!(fs.read_file(&victim, &mut Vec::new()).is_err());
    }
}

#[test]
fn garbage_records_in_the_middle_of_the_mft() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = image(tmp.path(), 24, 4096, &[]);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let all = entries(&clean);
    let rs = clean.record_size() as usize;
    let victims: Vec<u64> = ["f003.bin", "f010.bin", "f011.bin", "f017.bin"]
        .iter()
        .map(|n| split_id(find(&all, n).id).0)
        .collect();
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for variant in 0..4 {
        let mut bad = img.clone();
        for &v in &victims {
            let off = mft_record_offset(&clean, v) as usize;
            let rec = &mut bad[off..off + rs];
            match variant {
                // Pure noise.
                0 => rec.iter_mut().for_each(|b| *b = rnd() as u8),
                // Noise behind a valid-looking header.
                1 => rec[0x38..].iter_mut().for_each(|b| *b = rnd() as u8),
                // Absurd attribute length.
                2 => rec[0x3C..0x40].copy_from_slice(&0xFFFF_FFF8u32.to_le_bytes()),
                // All 0xFF (unprogrammed flash).
                _ => rec.fill(0xFF),
            }
        }
        let fs = open_mem(bad, &NtfsOptions::default()).unwrap();
        let got = entries(&fs);
        for (name, bytes) in &files {
            let clean_rec = split_id(find(&all, name).id).0;
            if victims.contains(&clean_rec) {
                // Whatever survives of a destroyed record must not crash a
                // reader; name-only entries refuse to read.
                for e in got.iter().filter(|e| e.name == *name) {
                    let _ = fs.read_file(e, &mut Vec::new());
                }
                continue;
            }
            // Every file whose record was not touched is listed and intact.
            let e = find(&got, name);
            assert_eq!(split_id(e.id).0, clean_rec, "variant {variant}: {name}");
            let (got_bytes, rep) = read(&fs, e);
            assert_eq!(&got_bytes, bytes, "variant {variant}: {name}");
            assert!(rep.is_complete(), "variant {variant}: {name}");
        }
    }
}

#[test]
fn four_k_records_torn_and_bad_update_sequence() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = image(tmp.path(), 8, 4096, &["-s", "4096"]);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    assert_eq!(clean.record_size(), 4096);
    assert_eq!(clean.boot_sector().bytes_per_sector, 4096);
    let all = entries(&clean);
    let t = split_id(find(&all, "f002.bin").id).0;
    let u = split_id(find(&all, "f003.bin").id).0;
    let mut bad = img.clone();
    // Tear stride 5 (bytes 3070..3072 of the record).
    let off = mft_record_offset(&clean, t) as usize;
    bad[off + 6 * 512 - 2] ^= 0xA5;
    // Wrong update sequence count on another record.
    let off = mft_record_offset(&clean, u) as usize;
    bad[off + 6..off + 8].copy_from_slice(&3u16.to_le_bytes());
    let fs = open_mem(bad, &NtfsOptions::default()).unwrap();
    let got = entries(&fs);
    let e = find(&got, "f002.bin");
    assert!(e.flags.iter().any(|f| f == "torn-record"), "{:?}", e.flags);
    assert_eq!(fs.record(t).unwrap().torn_strides, vec![5]);
    assert_eq!(read(&fs, e).0, files["f002.bin"]);
    assert!(fs.record(u).is_err());
    // The index still names the rejected record's file: name only.
    let lost = find(&got, "f003.bin");
    assert!(fs.read_file(lost, &mut Vec::new()).is_err());
    for (name, bytes) in &files {
        if name != "f003.bin" {
            assert_eq!(&read(&fs, find(&got, name)).0, bytes, "{name}");
        }
    }
}

#[test]
fn valid_looking_primary_boot_sector_with_a_wrong_mft_location() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = image(tmp.path(), 4, 4096, &[]);
    let mut bad = img.clone();
    // Both LCNs still inside the volume, so sector 0 validates, but they
    // point at the wrong place.
    bad[0x30..0x38].copy_from_slice(&1000u64.to_le_bytes());
    bad[0x38..0x40].copy_from_slice(&1001u64.to_le_bytes());
    let fs = open_mem(bad, &NtfsOptions::default())
        .unwrap_or_else(|e| panic!("the backup boot sector was not tried: {e}"));
    assert!(fs.boot_sector().is_backup);
    let all = entries(&fs);
    for (name, bytes) in &files {
        assert_eq!(&read(&fs, find(&all, name)).0, bytes);
    }
}
