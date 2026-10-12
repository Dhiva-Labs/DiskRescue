//! Test support: external tool discovery and two tiny, independent
//! writers (FAT12/16/32 and exFAT) that put files and directories into
//! images made by mkfs.vfat / mkfs.exfat and delete them the way the Linux
//! (or Windows) drivers do. The images are then checked with fsck.vfat -n /
//! fsck.exfat -n so a writer bug cannot hide a reader bug.

#![allow(dead_code)]

pub mod exfat_writer;
pub mod fat_writer;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use diskrescue_core::fs::{collect_entries, FileEntry, FileSystem};
use diskrescue_core::source::FileSource;
use diskrescue_core::SharedSource;

/// Locate an external tool in PATH, /usr/sbin or /sbin.
pub fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.push("/usr/sbin".into());
    dirs.push("/sbin".into());
    dirs.push("/usr/local/sbin".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Return the tool or print the standard skip message.
pub fn need(name: &str) -> Option<PathBuf> {
    let t = tool(name);
    if t.is_none() {
        eprintln!("skipped: {name} missing");
    }
    t
}

/// Run a command, returning (success, stdout+stderr).
pub fn run(cmd: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", cmd.display()));
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// Create a sparse file of `size` bytes.
pub fn sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).unwrap();
    f.set_len(size).unwrap();
}

/// mkfs.vfat an image; `bits` = 12/16/32. Returns false when the tool is missing.
pub fn mkfs_vfat(path: &Path, size: u64, bits: u32, label: &str) -> bool {
    let Some(t) = need("mkfs.vfat") else {
        return false;
    };
    sparse(path, size);
    let b = bits.to_string();
    let (ok, out) = run(&t, &["-F", &b, "-n", label, path.to_str().unwrap()]);
    assert!(ok, "mkfs.vfat failed: {out}");
    true
}

/// mkfs.exfat an image. Returns false when the tool is missing.
pub fn mkfs_exfat(path: &Path, size: u64, label: &str) -> bool {
    let Some(t) = need("mkfs.exfat") else {
        return false;
    };
    sparse(path, size);
    let (ok, out) = run(&t, &["-L", label, path.to_str().unwrap()]);
    assert!(ok, "mkfs.exfat failed: {out}");
    true
}

/// fsck.vfat -n must accept the image. Returns (used, total) clusters from
/// its summary line, or None when the tool is missing.
pub fn fsck_vfat(path: &Path) -> Option<(u64, u64)> {
    let t = need("fsck.vfat")?;
    let (ok, out) = run(&t, &["-n", path.to_str().unwrap()]);
    eprintln!("fsck.vfat: {}", out.trim());
    assert!(ok, "fsck.vfat -n rejected the image:\n{out}");
    assert!(
        !out.contains("differ") && !out.contains("Free cluster summary wrong"),
        "fsck.vfat -n complained:\n{out}"
    );
    // "<img>: N files, U/T clusters"
    let line = out.lines().find(|l| l.trim_end().ends_with(" clusters"))?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let frac = words.get(words.len().checked_sub(2)?)?;
    let (u, t) = frac.split_once('/')?;
    Some((u.parse().ok()?, t.parse().ok()?))
}

/// fsck.exfat -n must report the image clean. Returns false when missing.
pub fn fsck_exfat(path: &Path) -> bool {
    let Some(t) = need("fsck.exfat") else {
        return false;
    };
    let (ok, out) = run(&t, &["-n", path.to_str().unwrap()]);
    eprintln!("fsck.exfat: {}", out.trim());
    assert!(
        ok && out.contains("clean"),
        "fsck.exfat -n rejected the image:\n{out}"
    );
    true
}

/// Free cluster count reported by dump.exfat.
pub fn dump_exfat_free(path: &Path) -> Option<u64> {
    let t = need("dump.exfat")?;
    let (_, out) = run(&t, &[path.to_str().unwrap()]);
    let line = out.lines().find(|l| l.starts_with("Free Clusters"))?;
    line.split(':').nth(1)?.trim().parse().ok()
}

/// Open an image file as a shared source.
pub fn source(path: &Path) -> SharedSource {
    Arc::new(FileSource::open(path).unwrap())
}

/// All entries of a filesystem.
pub fn entries(fs: &dyn FileSystem) -> Vec<FileEntry> {
    collect_entries(fs).unwrap()
}

/// Find an entry by path.
pub fn by_path<'a>(all: &'a [FileEntry], path: &str) -> &'a FileEntry {
    all.iter().find(|e| e.path == path).unwrap_or_else(|| {
        panic!(
            "no entry {path:?}; have:\n{}",
            all.iter()
                .map(|e| format!("  {} (deleted={})", e.path, e.deleted))
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

/// Read a file through the filesystem.
pub fn read(fs: &dyn FileSystem, e: &FileEntry) -> (Vec<u8>, diskrescue_core::fs::FileReadReport) {
    let mut out = Vec::new();
    let rep = fs.read_file(e, &mut out).unwrap();
    assert_eq!(out.len() as u64, e.size, "{}: wrong length", e.path);
    assert_eq!(rep.bytes_written, e.size);
    (out, rep)
}

/// Deterministic pseudo-random generator (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Test content: a JPEG-looking payload of `len` bytes, unique per `seed`.
pub fn jpeg_like(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    let mut v = r.bytes(len);
    if len >= 4 {
        v[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    }
    v
}

/// Plain (non-media) payload.
pub fn text_like(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..len).map(|_| b'a' + (r.next() % 26) as u8).collect()
}
