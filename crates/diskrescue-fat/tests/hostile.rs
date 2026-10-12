//! Random, truncated, mutated and absurd inputs: every public entry point
//! must return (an error or a result) without panicking and in bounded
//! time.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::fs::{FileSystem, WalkControl};
use diskrescue_core::source::MemSource;
use diskrescue_core::SharedSource;
use diskrescue_fat::{open, open_exfat, open_fat, probe, FatOptions};
use support::exfat_writer::{ExImage, ExMode};
use support::fat_writer::{Alloc, Dir, FatImage};
use support::*;

const MIB: u64 = 1024 * 1024;

/// Walk, read every reasonably sized file and compute free space.
fn exercise(fs: &dyn FileSystem) {
    let mut list = Vec::new();
    fs.walk(&mut |e| {
        list.push(e.clone());
        if list.len() > 20_000 {
            WalkControl::Stop
        } else {
            WalkControl::Continue
        }
    })
    .unwrap();
    for e in list
        .iter()
        .filter(|e| !e.is_dir && e.size <= 8 * MIB)
        .take(300)
    {
        let mut sink = std::io::sink();
        if let Ok(rep) = fs.read_file(e, &mut sink) {
            assert_eq!(rep.bytes_written, e.size);
        }
    }
    for e in list.iter().filter(|e| e.is_dir).take(5) {
        assert!(fs.read_file(e, &mut std::io::sink()).is_err());
    }
    let _ = fs.unallocated().unwrap();
    let _ = fs.info();
}

/// Try every opener; returns true when any of them accepted the bytes.
fn try_all(bytes: Vec<u8>) -> bool {
    let head_len = bytes.len().min(4096);
    let _ = probe(&bytes[..head_len]);
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let opts = FatOptions::default();
    let mut opened = false;
    if let Ok(fs) = open(src.clone(), &opts) {
        exercise(&*fs);
        opened = true;
    }
    if let Ok(fs) = open_fat(src.clone(), &opts) {
        exercise(&fs);
        opened = true;
    }
    if let Ok(fs) = open_exfat(src, &opts) {
        exercise(&fs);
        opened = true;
    }
    opened
}

#[test]
fn random_bytes() {
    for seed in 0..40u64 {
        let len = [0usize, 1, 100, 511, 512, 4096, 65_536, 300_000][seed as usize % 8];
        let data = Rng::new(seed).bytes(len);
        try_all(data.clone());
        let src: SharedSource = Arc::new(MemSource::new(data));
        if len >= 512 {
            // Pure noise must not be mistaken for a volume.
            assert!(open(src, &FatOptions::default()).is_err(), "seed {seed}");
        }
    }
}

fn small_fat(tmp: &Path, bits: u32, size: u64) -> Option<Vec<u8>> {
    let img = tmp.join(format!("h{bits}.img"));
    if !mkfs_vfat(&img, size, bits, "HOSTILE") {
        return None;
    }
    let mut w = FatImage::open(&img);
    let cs = w.cs() as usize;
    let a = w.mkdir(Dir::Root, "A dir");
    let b = w.mkdir(a, "B");
    w.put(
        Dir::Root,
        "root file.jpg",
        &jpeg_like(cs * 3, 1),
        Alloc::Contiguous,
    );
    w.put(a, "in a.txt", &text_like(cs + 3, 2), Alloc::Fragmented);
    w.put(b, "DEEP.BIN", &text_like(10, 3), Alloc::Contiguous);
    w.put(b, "to delete.jpg", &jpeg_like(cs * 2, 4), Alloc::Contiguous);
    w.delete(b, "to delete.jpg", false);
    drop(w);
    Some(std::fs::read(&img).unwrap())
}

fn small_exfat(tmp: &Path) -> Option<Vec<u8>> {
    let img = tmp.join("hx.img");
    if !mkfs_exfat(&img, 64 * MIB, "HOSTILE") {
        return None;
    }
    let mut w = ExImage::open(&img);
    let cs = w.cs as usize;
    let root = w.root_dir();
    let a = w.mkdir(&root, "A dir");
    w.put(
        &root,
        "root file.jpg",
        &jpeg_like(cs * 3, 1),
        ExMode::NoFatChain,
    );
    w.put(
        &a,
        "in a.txt",
        &text_like(cs * 4 + 3, 2),
        ExMode::FragmentedChain,
    );
    w.put(
        &a,
        "to delete.jpg",
        &jpeg_like(cs * 2, 4),
        ExMode::NoFatChain,
    );
    w.delete(&a, "to delete.jpg");
    drop(w);
    Some(std::fs::read(&img).unwrap())
}

/// Flip random bytes inside `region` of `base` for many seeds; returns how
/// many mutated images still opened.
fn mutate_many(base: &[u8], region: std::ops::Range<usize>, seeds: u64, budget: Duration) -> u64 {
    let t0 = Instant::now();
    let mut opened = 0;
    for seed in 0..seeds {
        let mut r = Rng::new(seed + 1000);
        let mut bytes = base.to_vec();
        let flips = 1 + r.below(200) as usize;
        for _ in 0..flips {
            let at = region.start + r.below((region.end - region.start) as u64) as usize;
            bytes[at] = r.next() as u8;
        }
        // Sometimes plant absurd values in a structured way.
        if seed % 3 == 0 {
            let at =
                region.start + (r.below(((region.end - region.start) / 32) as u64) as usize) * 32;
            if at + 32 <= bytes.len() {
                bytes[at + 20..at + 32].fill(0xFF);
            }
        }
        opened += try_all(bytes) as u64;
        assert!(
            t0.elapsed() < budget,
            "mutation tests too slow at seed {seed}"
        );
    }
    opened
}

#[test]
fn mutated_fat_volumes() {
    let tmp = tempfile::tempdir().unwrap();
    for (bits, size) in [(12u32, 2 * MIB), (16, 32 * MIB), (32, 64 * MIB)] {
        let Some(base) = small_fat(tmp.path(), bits, size) else {
            return;
        };
        // Boot sector, FATs, root and the first data clusters.
        let end = match bits {
            12 => base.len(),
            16 => 400 * 1024,
            _ => 1200 * 1024,
        };
        let seeds = if bits == 32 { 25 } else { 60 };
        mutate_many(&base, 0..end, seeds, Duration::from_secs(240));
        // Directory area only (keeps the boot sector valid more often).
        let w_start = match bits {
            12 => 4096,
            16 => 100 * 1024,
            _ => 1000 * 1024,
        };
        let opened = mutate_many(&base, w_start..end, seeds, Duration::from_secs(240));
        assert!(
            opened > seeds / 2,
            "FAT{bits}: only {opened} of {seeds} opened"
        );
    }
}

#[test]
fn mutated_exfat_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(base) = small_exfat(tmp.path()) else {
        return;
    };
    // Boot regions + FAT, then the heap start (bitmap, up-case, root, dirs).
    mutate_many(&base, 0..64 * 1024, 20, Duration::from_secs(240));
    let heap = 2 * MIB as usize;
    let opened = mutate_many(&base, heap..heap + 64 * 1024, 30, Duration::from_secs(240));
    assert!(opened > 15, "only {opened} of 30 opened");
}

#[test]
fn truncated_volumes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut images = Vec::new();
    if let Some(b) = small_fat(tmp.path(), 32, 64 * MIB) {
        images.push(b);
    }
    if let Some(b) = small_fat(tmp.path(), 12, 2 * MIB) {
        images.push(b);
    }
    if let Some(b) = small_exfat(tmp.path()) {
        images.push(b);
    }
    for base in images {
        for cut in [
            0usize,
            1,
            511,
            512,
            4096,
            6 * 512,
            64 * 1024,
            300 * 1024,
            1100 * 1024,
            2 * MIB as usize + 20_000,
        ] {
            let cut = cut.min(base.len());
            try_all(base[..cut].to_vec());
        }
        // A volume whose tail is missing still opens and warns.
        let cut = base.len() / 2;
        let src: SharedSource = Arc::new(MemSource::new(base[..cut].to_vec()));
        let fs = open(src, &FatOptions::default()).unwrap();
        assert!(fs.info().warnings.iter().any(|w| w.contains("shorter")));
        exercise(&*fs);
    }
}

#[test]
fn absurd_geometry_is_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    if let Some(base) = small_fat(tmp.path(), 32, 64 * MIB) {
        let mut b = base[..2 * MIB as usize].to_vec();
        b[0x20..0x24].copy_from_slice(&u32::MAX.to_le_bytes()); // TotSec32
        b[0x24..0x28].copy_from_slice(&0x0800_0000u32.to_le_bytes()); // FATSz32
        let t0 = Instant::now();
        try_all(b.clone());
        assert!(t0.elapsed() < Duration::from_secs(60));
        // Same with a bogus root cluster.
        b[0x2C..0x30].copy_from_slice(&0x0FFF_FFF0u32.to_le_bytes());
        try_all(b);
    }
    if let Some(base) = small_exfat(tmp.path()) {
        let mut b = base[..4 * MIB as usize].to_vec();
        b[0x48..0x50].copy_from_slice(&u64::MAX.to_le_bytes()); // VolumeLength
        b[0x54..0x58].copy_from_slice(&0x4000_0000u32.to_le_bytes()); // FatLength
        b[0x5C..0x60].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes()); // ClusterCount
        b[0x6D] = 0; // 512-byte clusters
        let t0 = Instant::now();
        // Main boot sector now fails its checksum; the backup is intact,
        // so also damage the backup the same way.
        let bk = 12 * 512;
        let main = b[..512].to_vec();
        b[bk..bk + 512].copy_from_slice(&main);
        try_all(b);
        assert!(t0.elapsed() < Duration::from_secs(60));
    }
}

#[test]
fn directory_loops_terminate() {
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("loop.img");
    if !mkfs_vfat(&img, 32 * MIB, 16, "LOOP") {
        return;
    }
    let mut w = FatImage::open(&img);
    let a = w.mkdir(Dir::Root, "A");
    let b = w.mkdir(a, "B");
    drop(w);
    // Make B's "." entry and a new entry inside B point back at A.
    let Dir::Cluster(ac) = a else { unreachable!() };
    let Dir::Cluster(bc) = b else { unreachable!() };
    let mut bytes = std::fs::read(&img).unwrap();
    let w = FatImage::open(&img);
    let boff = w.clus_off(bc) as usize;
    // Third slot of B: a directory entry "LOOP" -> A.
    let e = &mut bytes[boff + 64..boff + 96];
    e[..11].copy_from_slice(b"LOOP       ");
    e[11] = 0x10;
    e[24..26].copy_from_slice(&((44u16 << 9) | (5 << 5) | 1).to_le_bytes());
    e[26..28].copy_from_slice(&(ac as u16).to_le_bytes());
    let src: SharedSource = Arc::new(MemSource::new(bytes));
    let fs = open_fat(src, &FatOptions::default()).unwrap();
    let all = entries(&fs);
    let lp = by_path(&all, "A/B/LOOP");
    assert!(
        lp.flags.iter().any(|f| f.contains("already listed")),
        "{:?}",
        lp.flags
    );
    assert!(all.len() < 10);
}
