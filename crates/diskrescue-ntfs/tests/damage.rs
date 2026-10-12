//! Damaged volumes: bad sectors in the MFT and in file data, torn records,
//! a destroyed boot sector or MFT record 0, patched sizes, and fuzzed or
//! truncated images that must never panic.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet};
use diskrescue_ntfs::attr::types;
use diskrescue_ntfs::walk::split_id;
use diskrescue_ntfs::{open, NtfsFs, NtfsOptions};

/// Build an image with `n` files copied in by ntfscp. Returns the image
/// bytes and the file contents by name.
fn populated(dir: &Path, n: u64, cluster: u32) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let img = dir.join("base.img");
    mkntfs(&img, 16, cluster, "DAMAGE", &[]);
    let mut files = Vec::new();
    for i in 0..n {
        let size = match i % 3 {
            0 => 200 + i as usize,
            1 => 20_000 + 1000 * i as usize,
            _ => 150_000 + 7 * i as usize,
        };
        let name = format!("file{i:02}.bin");
        let bytes = data(1000 + i, size);
        let p = dir.join(&name);
        write(&p, &bytes);
        ntfscp(&img, &p, &format!("/{name}"));
        files.push((name, bytes));
    }
    (std::fs::read(&img).unwrap(), files)
}

/// Byte offset of MFT record `n` in the image (assumes the record lies in
/// the first MFT fragment, true for these small volumes).
fn record_offset(fs: &NtfsFs, n: u64) -> u64 {
    let first = fs.mft_extents()[0];
    let off = first.start + n * fs.record_size();
    assert!(off + fs.record_size() <= first.end);
    off
}

fn rec_of(e: &FileEntry) -> u64 {
    split_id(e.id).0
}

/// A sink that refuses to grow beyond a limit (keeps hostile sizes cheap).
struct Capped {
    n: u64,
    cap: u64,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.n + buf.len() as u64 > self.cap {
            return Err(std::io::Error::other("cap reached"));
        }
        self.n += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Open, walk and read everything; must not panic. Returns entry count.
fn exercise(bytes: Vec<u8>, opts: &NtfsOptions) -> usize {
    let Ok(fs) = open(Arc::new(MemSource::new(bytes)), opts) else {
        return 0;
    };
    let mut list = Vec::new();
    let _ = fs.walk(&mut |e| {
        list.push(e.clone());
        diskrescue_core::fs::WalkControl::Continue
    });
    for e in list.iter().take(200) {
        let mut sink = Capped {
            n: 0,
            cap: 32 << 20,
        };
        let _ = fs.read_file(e, &mut sink);
    }
    let _ = fs.unallocated();
    let _ = fs.allocated();
    for n in 0..fs.record_count().min(40) {
        if let Ok(r) = fs.record(n) {
            for a in fs.attributes(&r) {
                let _ = fs.data_runs(&a);
            }
        }
    }
    list.len()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[test]
fn backup_boot_sector_when_sector_zero_is_damaged() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (good, files) = populated(tmp.path(), 4, 4096);
    for variant in 0..2 {
        let mut img = good.clone();
        if variant == 0 {
            img[..512].fill(0);
        } else {
            let mut r = Rng(77);
            for b in &mut img[..512] {
                *b = r.next() as u8;
            }
        }
        let fs = open_mem(img, &NtfsOptions::default()).unwrap();
        assert!(fs.boot_sector().is_backup);
        assert!(
            fs.info()
                .warnings
                .iter()
                .any(|w| w.contains("backup boot sector")),
            "{:?}",
            fs.info().warnings
        );
        let all = entries(&fs);
        for (name, bytes) in &files {
            assert_eq!(&read(&fs, find(&all, name)).0, bytes);
        }
    }
    // Asking for the backup on an intact volume.
    let fs = open_mem(
        good.clone(),
        &NtfsOptions {
            use_backup_boot_sector: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(fs.boot_sector().is_backup);
    // Both copies destroyed: a clean error, not a panic.
    let mut img = good;
    img[..512].fill(0);
    let n = img.len();
    img[n - 512..].fill(0);
    assert!(open_mem(img, &NtfsOptions::default()).is_err());
}

#[test]
fn unreadable_sector_in_the_mft_skips_one_record() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = populated(tmp.path(), 12, 4096);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let clean_entries = entries(&clean);
    let victim = find(&clean_entries, "file04.bin").clone();
    let off = record_offset(&clean, rec_of(&victim));
    // The second 512-byte sector of the victim's record cannot be read.
    let bad = RangeSet::from_range(Range::new(off + 512, off + 1024));
    let src = MemSource::new(img.clone()).with_unreadable(bad);
    let fs = open(Arc::new(src), &NtfsOptions::default()).unwrap();
    let all = entries(&fs);
    // The record is skipped; the root index still names the file, so it
    // survives as a name-only entry that cannot be read.
    let lost: Vec<&FileEntry> = all.iter().filter(|e| e.name == "file04.bin").collect();
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].confidence, EntryConfidence::DeletedNoData);
    assert!(!lost[0].deleted);
    assert!(lost[0].flags.iter().any(|f| f == "mft-record-unreadable"));
    // The index key carries the size, useful for carving.
    assert_eq!(lost[0].size, files[4].1.len() as u64);
    assert!(fs.read_file(lost[0], &mut Vec::new()).is_err());
    for (name, bytes) in &files {
        if name == "file04.bin" {
            continue;
        }
        let (got, rep) = read(&fs, find(&all, name));
        assert_eq!(&got, bytes);
        assert!(rep.is_complete());
    }
    // Reading the lost record's entry fails cleanly.
    assert!(fs.read_file(&victim, &mut Vec::new()).is_err());

    // The root directory's record unreadable: files are still placed.
    let off = record_offset(&clean, 5);
    let bad = RangeSet::from_range(Range::new(off, off + 512));
    let src = MemSource::new(img).with_unreadable(bad);
    let fs = open(Arc::new(src), &NtfsOptions::default()).unwrap();
    assert!(fs.info().warnings.iter().any(|w| w.contains("root")));
    let all = entries(&fs);
    for (name, bytes) in &files {
        assert_eq!(&read(&fs, find(&all, name)).0, bytes);
    }
}

#[test]
fn unreadable_sector_in_file_data_is_reported() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = populated(tmp.path(), 3, 4096);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let all = entries(&clean);
    let e = find(&all, "file02.bin").clone();
    let expect = &files[2].1;
    let ext = e.extents[0];
    let bad_abs = Range::new(ext.start + 8192, ext.start + 8192 + 1024);
    let src = MemSource::new(img).with_unreadable(RangeSet::from_range(bad_abs));
    let fs = open(Arc::new(src), &NtfsOptions::default()).unwrap();
    let (got, rep) = read(&fs, &e);
    assert_eq!(got.len(), expect.len());
    assert_eq!(rep.unreadable.as_slice(), &[Range::new(8192, 9216)]);
    assert!(got[8192..9216].iter().all(|&b| b == 0));
    assert_eq!(got[..8192], expect[..8192]);
    assert_eq!(got[9216..], expect[9216..]);
    assert!(!rep.is_complete());
}

#[test]
fn torn_record_is_parsed_and_flagged() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (mut img, files) = populated(tmp.path(), 3, 4096);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let all = entries(&clean);
    let e = find(&all, "file01.bin");
    let off = record_offset(&clean, rec_of(e)) as usize;
    // Break the update sequence check bytes of the record's second sector.
    img[off + 1022] ^= 0x5A;
    let fs = open_mem(img, &NtfsOptions::default()).unwrap();
    let all = entries(&fs);
    let e = find(&all, "file01.bin");
    assert!(e.flags.iter().any(|f| f == "torn-record"), "{:?}", e.flags);
    let (got, rep) = read(&fs, e);
    assert_eq!(got, files[1].1);
    assert!(
        rep.notes.iter().any(|n| n.contains("torn")),
        "{:?}",
        rep.notes
    );
}

#[test]
fn mft_record_zero_from_the_mirror_or_by_probing() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = populated(tmp.path(), 5, 4096);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let mft0 = record_offset(&clean, 0) as usize;
    let mirr = (clean.boot_sector().mftmirr_lcn * clean.cluster_size()) as usize;
    let rs = clean.record_size() as usize;

    let mut a = img.clone();
    a[mft0..mft0 + rs].fill(0);
    let fs = open_mem(a, &NtfsOptions::default()).unwrap();
    assert!(
        fs.info().warnings.iter().any(|w| w.contains("$MFTMirr")),
        "{:?}",
        fs.info().warnings
    );
    let all = entries(&fs);
    for (name, bytes) in &files {
        assert_eq!(&read(&fs, find(&all, name)).0, bytes);
    }

    let mut b = img;
    b[mft0..mft0 + rs].fill(0);
    b[mirr..mirr + rs].fill(0);
    let fs = open_mem(b, &NtfsOptions::default()).unwrap();
    assert!(
        fs.info()
            .warnings
            .iter()
            .any(|w| w.contains("reconstructed")),
        "{:?}",
        fs.info().warnings
    );
    let all = entries(&fs);
    for (name, bytes) in &files {
        assert_eq!(&read(&fs, find(&all, name)).0, bytes);
    }
}

#[test]
fn initialized_size_shorter_than_data_size_reads_zeros() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (mut img, files) = populated(tmp.path(), 3, 4096);
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let all = entries(&clean);
    let e = find(&all, "file02.bin");
    let rec = clean.record(rec_of(e)).unwrap();
    let attrs = clean.attributes(&rec);
    let data_attr = attrs
        .iter()
        .find(|a| a.type_code == types::DATA && a.name.is_empty())
        .unwrap();
    assert!(!data_attr.is_resident());
    let field = record_offset(&clean, rec_of(e)) as usize + data_attr.offset + 0x38;
    // The 8-byte field must not straddle a fixup slot (last 2 bytes of a
    // 512-byte stride); mkntfs layouts keep it well away.
    assert!(field % 512 < 504);
    let init = 100_000u64;
    img[field..field + 8].copy_from_slice(&init.to_le_bytes());
    let fs = open_mem(img, &NtfsOptions::default()).unwrap();
    let all = entries(&fs);
    let e = find(&all, "file02.bin");
    let (got, rep) = read(&fs, e);
    let expect = &files[2].1;
    assert_eq!(got.len(), expect.len());
    assert_eq!(got[..init as usize], expect[..init as usize]);
    assert!(got[init as usize..].iter().all(|&b| b == 0));
    assert!(rep.holes.contains(init));
    assert!(rep.notes.iter().any(|n| n.contains("initialized")));
}

#[test]
fn truncated_images_do_not_panic() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, files) = populated(tmp.path(), 6, 4096);
    for cut in [
        0usize,
        100,
        512,
        4096,
        20_000,
        100_000,
        1 << 20,
        img.len() / 2,
        img.len() - 4096,
    ] {
        exercise(img[..cut.min(img.len())].to_vec(), &NtfsOptions::default());
    }
    // Losing only the tail keeps everything before it readable, with the
    // truncation reported.
    let fs = open_mem(img[..img.len() / 2].to_vec(), &NtfsOptions::default()).unwrap();
    assert!(fs
        .info()
        .warnings
        .iter()
        .any(|w| w.contains("past the end")));
    let all = entries(&fs);
    assert!(all.len() >= files.len());
    for e in &all {
        if e.is_dir {
            continue;
        }
        let mut sink = Vec::new();
        let rep = fs.read_file(e, &mut sink).unwrap();
        assert_eq!(sink.len() as u64, e.size);
        let beyond = e.extents.iter().any(|r| r.end > (img.len() / 2) as u64);
        assert_eq!(beyond, !rep.unreadable.is_empty(), "{}", e.path);
    }
}

#[test]
fn random_and_mutated_volumes_do_not_panic() {
    let mut rng = Rng(0xDEAD_BEEF_1234_5678);
    // Pure noise, with and without a plausible boot sector.
    for i in 0..40 {
        let len = 512 + rng.below(3 << 20) as usize;
        let mut v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if i % 2 == 0 {
            let mut b = vec![0u8; 512];
            b[3..11].copy_from_slice(b"NTFS    ");
            b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
            b[0x0D] = [1u8, 8, 0x80, 0xF4][i % 4];
            let sectors = (len as u64 / 512).max(2) - 1;
            b[0x28..0x30].copy_from_slice(&sectors.to_le_bytes());
            let clusters = sectors / b[0x0D].max(1) as u64;
            b[0x30..0x38].copy_from_slice(&(rng.below(clusters.max(1))).to_le_bytes());
            b[0x38..0x40].copy_from_slice(&(rng.below(clusters.max(1))).to_le_bytes());
            b[0x40] = 0xF6;
            b[0x44] = 1;
            b[510] = 0x55;
            b[511] = 0xAA;
            v[..512].copy_from_slice(&b);
        }
        exercise(v, &NtfsOptions::default());
    }
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    // Mutations of a real volume's metadata.
    let tmp = tempfile::tempdir().unwrap();
    // NTFS_FUZZ_IMAGE lets a local run mutate a richer image instead.
    let img = match std::env::var_os("NTFS_FUZZ_IMAGE").filter(|p| !p.is_empty()) {
        Some(p) => std::fs::read(p).unwrap(),
        None => populated(tmp.path(), 8, 4096).0,
    };
    let clean = open_mem(img.clone(), &NtfsOptions::default()).unwrap();
    let mut targets: Vec<Range> = vec![Range::new(0, 512)];
    let mft = clean.mft_extents()[0];
    targets.push(Range::new(mft.start, mft.start + 80 * 1024));
    let root = clean.record(5).unwrap();
    for a in clean.attributes(&root) {
        for r in clean.data_runs(&a) {
            if let Some(l) = r.lcn {
                let c = clean.cluster_size();
                targets.push(Range::new(l * c, (l + r.len) * c));
            }
        }
    }
    drop(clean);
    // NTFS_FUZZ_ITERS raises the iteration count for longer local runs.
    let iters: u64 = std::env::var("NTFS_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let seed = std::env::var("NTFS_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0xC0FFEE);
    rng.0 ^= seed;
    for iter in 0..iters {
        let mut v = img.clone();
        let edits = 1 + rng.below(40);
        for _ in 0..edits {
            let t = targets[rng.below(targets.len() as u64) as usize];
            let at = (t.start + rng.below(t.len())) as usize;
            match rng.below(4) {
                0 => v[at] = rng.next() as u8,
                1 => v[at] ^= 1 << rng.below(8),
                2 => v[at] = 0xFF,
                _ => v[at] = 0,
            }
        }
        let opts = NtfsOptions {
            walk_whole_mft: iter % 3 != 0,
            include_deleted: iter % 5 != 0,
            ..Default::default()
        };
        exercise(v, &opts);
    }
}

#[test]
fn absurd_boot_sector_values() {
    let base = {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8;
        b[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&8u64.to_le_bytes());
        b[0x40] = 0xF6;
        b[0x44] = 1;
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    };
    for patch in 0..6 {
        let mut b = base.clone();
        match patch {
            0 => {}
            1 => b[0x28..0x30].copy_from_slice(&(1u64 << 50).to_le_bytes()),
            2 => b[0x40] = 0x81,
            3 => b[0x0D] = 0xFE,
            4 => b[0x30..0x38].copy_from_slice(&(1u64 << 40).to_le_bytes()),
            _ => b[0x44] = 0,
        }
        let mut img = b.clone();
        img.resize(1 << 20, 0);
        exercise(img, &NtfsOptions::default());
    }
}

#[test]
fn max_records_caps_the_walk() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (img, _) = populated(tmp.path(), 6, 4096);
    let fs = open_mem(
        img,
        &NtfsOptions {
            max_records: 66,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.record_count(), 66);
    assert!(fs.info().warnings.iter().any(|w| w.contains("max_records")));
    let all = entries(&fs);
    assert!(all
        .iter()
        .all(|e| rec_of(e) < 66 || e.flags.iter().any(|f| f == "virtual")));
    // Records beyond the cap that the root index names show up as
    // name-only entries, never as readable files.
    for e in &all {
        if rec_of(e) >= 66 && !e.flags.iter().any(|f| f == "virtual") {
            assert_eq!(e.confidence, EntryConfidence::DeletedNoData);
        }
    }
}
