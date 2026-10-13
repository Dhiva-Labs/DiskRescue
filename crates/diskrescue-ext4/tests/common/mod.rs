//! Shared helpers for the integration tests: locating e2fsprogs, building
//! source trees and images, and reading entries back.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use diskrescue_core::fs::{collect_entries, FileEntry, FileReadReport, FileSystem};
use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::SharedSource;
use diskrescue_ext4::{open, ExtFs, ExtOptions};

/// Find an executable on PATH or in the sbin directories.
pub fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in ["/usr/local/sbin", "/usr/sbin", "/sbin", "/usr/bin", "/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// True when every tool exists; prints the skip reason otherwise.
pub fn have(names: &[&str]) -> bool {
    for n in names {
        if tool(n).is_none() {
            eprintln!("skipped: {n} missing");
            return false;
        }
    }
    true
}

/// Run a tool, panicking with its stderr on failure. Returns stdout.
pub fn run(name: &str, args: &[&str]) -> String {
    let exe = tool(name).unwrap_or_else(|| panic!("{name} missing"));
    let out = Command::new(&exe)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {name}: {e}"));
    assert!(
        out.status.success() || name == "e2fsck" && out.status.code().unwrap_or(8) < 4,
        "{name} {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Deterministic pseudo-random bytes.
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect()
}

/// What a test expects to find at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    File(Vec<u8>),
    Dir,
    Symlink(String),
    /// Sparse file: content plus "has holes".
    Sparse(Vec<u8>),
}

pub const LONG_NAME_LEN: usize = 240;

pub fn long_name() -> String {
    let mut s = "L".repeat(LONG_NAME_LEN - 4);
    s.push_str(".dat");
    s
}

/// Populate `root` with the standard test tree and return what each path
/// should contain.
pub fn standard_tree(root: &Path) -> BTreeMap<String, Expect> {
    let mut m = BTreeMap::new();
    let file = |rel: &str, data: Vec<u8>, m: &mut BTreeMap<String, Expect>| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &data).unwrap();
        m.insert(rel.to_string(), Expect::File(data));
    };
    file("hello.txt", b"hello world\n".to_vec(), &mut m);
    file("empty.bin", Vec::new(), &mut m);
    file(
        "DCIM/100CANON/IMG_0001.JPG",
        noise(1, 70 * 1024 + 123),
        &mut m,
    );
    file(
        "DCIM/100CANON/IMG_0002.JPG",
        noise(2, 300 * 1024 + 7),
        &mut m,
    );
    file("a/b/c/d/deep.txt", b"deep inside\n".to_vec(), &mut m);
    file(&long_name(), noise(3, 5000), &mut m);
    file("Фото_ünïcødé_✓.png", noise(4, 2048), &mut m);
    file("small/tiny.txt", b"tiny\n".to_vec(), &mut m);
    for d in [
        "DCIM",
        "DCIM/100CANON",
        "a",
        "a/b",
        "a/b/c",
        "a/b/c/d",
        "small",
    ] {
        m.insert(d.to_string(), Expect::Dir);
    }
    // The tree feeds mkfs.ext4 -d, which only exists on Unix.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
        m.insert("link".into(), Expect::Symlink("hello.txt".into()));
        let long_target = format!("DCIM/100CANON/{}", "t".repeat(90));
        std::os::unix::fs::symlink(&long_target, root.join("longlink")).unwrap();
        m.insert("longlink".into(), Expect::Symlink(long_target));
    }
    // Sparse file: 8 islands of data in 2 MiB, so the extent tree needs an
    // index level (more than 4 extents).
    let total = 2 * 1024 * 1024usize;
    let mut content = vec![0u8; total];
    let mut f = std::fs::File::create(root.join("sparse.bin")).unwrap();
    f.set_len(total as u64).unwrap();
    for k in 0..8usize {
        let off = k * 256 * 1024 + 100;
        let data = noise(10 + k as u64, 5000);
        content[off..off + data.len()].copy_from_slice(&data);
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(off as u64)).unwrap();
        f.write_all(&data).unwrap();
    }
    drop(f);
    m.insert("sparse.bin".into(), Expect::Sparse(content));
    m
}

/// Create a sparse image file of `size` bytes and run `mkfs` on it.
pub fn make_image(img: &Path, size: u64, mkfs: &str, args: &[&str], src: Option<&Path>) {
    let f = std::fs::File::create(img).unwrap();
    f.set_len(size).unwrap();
    drop(f);
    let mut a: Vec<String> = vec!["-F".into(), "-q".into()];
    a.extend(args.iter().map(|s| s.to_string()));
    if let Some(s) = src {
        a.push("-d".into());
        a.push(s.to_string_lossy().into_owned());
    }
    a.push(img.to_string_lossy().into_owned());
    let refs: Vec<&str> = a.iter().map(String::as_str).collect();
    run(mkfs, &refs);
}

pub fn file_source(img: &Path) -> SharedSource {
    Arc::new(FileSource::open(img).unwrap())
}

pub fn mem_source(img: &Path) -> MemSource {
    MemSource::new(std::fs::read(img).unwrap())
}

pub fn open_path(img: &Path) -> ExtFs {
    open(file_source(img), &ExtOptions::default()).unwrap()
}

pub fn entries(fs: &ExtFs) -> Vec<FileEntry> {
    collect_entries(fs).unwrap()
}

pub fn by_path(entries: &[FileEntry]) -> BTreeMap<String, FileEntry> {
    entries
        .iter()
        .map(|e| (e.path.clone(), e.clone()))
        .collect()
}

pub fn read(fs: &dyn FileSystem, e: &FileEntry) -> (Vec<u8>, FileReadReport) {
    let mut v = Vec::new();
    let rep = fs.read_file(e, &mut v).unwrap();
    assert_eq!(rep.bytes_written, v.len() as u64, "{}", e.path);
    (v, rep)
}

/// A sink that refuses to take more than `limit` bytes (keeps fuzz tests
/// from streaming absurd sizes).
pub struct Limited {
    pub limit: u64,
    pub got: u64,
}

impl Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.got + buf.len() as u64 > self.limit {
            return Err(std::io::Error::other("limit"));
        }
        self.got += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Walk, read every file into a limited sink and compute free space,
/// asserting only that nothing panics and ids are unique. Returns the
/// number of entries seen.
pub fn exercise(fs: &dyn FileSystem) -> usize {
    let mut all = Vec::new();
    let _ = fs.walk(&mut |e| {
        all.push(e.clone());
        diskrescue_core::fs::WalkControl::Continue
    });
    let mut ids = std::collections::HashSet::new();
    for e in &all {
        assert!(ids.insert(e.id), "duplicate id {} at {}", e.id, e.path);
        let mut sink = Limited {
            limit: 64 << 20,
            got: 0,
        };
        let _ = fs.read_file(e, &mut sink);
    }
    let _ = fs.unallocated();
    all.len()
}

/// Parse `dumpe2fs` output into the set of free blocks per the bitmaps.
pub fn dumpe2fs_free_blocks(img: &Path) -> (Vec<(u64, u64)>, u64) {
    let out = run("dumpe2fs", &[&img.to_string_lossy()]);
    let mut ranges = Vec::new();
    let mut sb_free = 0u64;
    for line in out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Free blocks:") {
            let rest = rest.trim();
            if line.starts_with("Free blocks:") {
                sb_free = rest.parse().unwrap_or(0);
                continue;
            }
            for item in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let (a, b) = match item.split_once('-') {
                    Some((a, b)) => (a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()),
                    None => {
                        let v = item.parse::<u64>().unwrap();
                        (v, v)
                    }
                };
                ranges.push((a, b + 1));
            }
        }
    }
    (ranges, sb_free)
}
