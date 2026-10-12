//! Shared helpers for the integration tests: Pillow-generated fixtures,
//! in-memory sources, a read-counting source and a tiny PRNG.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use diskrescue_jpeg::core::source::{MemSource, ReadError, SharedSource, Source};

/// Directory with the Pillow-generated fixture set, or `None` (after an
/// explicit "skipped" message) when python3 + Pillow are unavailable.
pub fn fixtures() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| generate("all")).as_deref()
}

/// Directory with the large (> 20 MB) fixture.
pub fn large_fixture() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| generate("large")).as_deref()
}

fn generate(set: &str) -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("jpeg-fixtures-{}-{set}", env!("CARGO_CRATE_NAME")));
    let _ = std::fs::remove_dir_all(&dir);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gen_jpegs.py");
    match Command::new("python3")
        .arg(&script)
        .arg(&dir)
        .arg(set)
        .output()
    {
        Ok(o) if o.status.success() => Some(dir),
        Ok(o) => {
            eprintln!(
                "skipped: python3+Pillow missing ({})",
                String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            );
            None
        }
        Err(_) => {
            eprintln!("skipped: python3 missing");
            None
        }
    }
}

/// Read a generated fixture.
pub fn read(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Names of the generated JPEG fixtures.
pub fn jpeg_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jpg"))
        .collect();
    v.sort();
    v
}

/// Path of a committed fixture.
pub fn committed_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Read a committed fixture.
pub fn committed(name: &str) -> Vec<u8> {
    std::fs::read(committed_path(name)).unwrap()
}

/// Names of the committed fixtures.
pub const COMMITTED: [&str; 5] = [
    "small_exif_thumb.jpg",
    "small_progressive.jpg",
    "small_restart.jpg",
    "small_gray.jpg",
    "small_mpf.jpg",
];

/// Wrap bytes as a shared source.
pub fn mem(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

/// xorshift64* PRNG for reproducible test data.
pub struct Rng(pub u64);

impl Rng {
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

/// Lenient zune-jpeg decode: `(width, height)` when the bytes decode.
pub fn zune_dims(data: &[u8]) -> Option<(u32, u32)> {
    let z = diskrescue_jpeg::zune_check(data);
    (z.strict_ok || z.lenient_ok).then_some((z.width, z.height))
}

/// Strict zune-jpeg decode succeeds.
pub fn zune_strict(data: &[u8]) -> bool {
    diskrescue_jpeg::zune_check(data).strict_ok
}

/// A source that records the largest single read and the total bytes read.
pub struct CountingSource {
    pub inner: SharedSource,
    pub max_read: AtomicUsize,
    pub total: AtomicU64,
}

impl CountingSource {
    pub fn new(inner: SharedSource) -> Self {
        CountingSource {
            inner,
            max_read: AtomicUsize::new(0),
            total: AtomicU64::new(0),
        }
    }
}

impl Source for CountingSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.max_read.fetch_max(buf.len(), Ordering::Relaxed);
        let n = self.inner.read_at(offset, buf)?;
        self.total.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn describe(&self) -> String {
        "counting".into()
    }
}

/// Offsets of all `FF D0..D7` restart markers in `data`.
pub fn rst_offsets(data: &[u8]) -> Vec<usize> {
    data.windows(2)
        .enumerate()
        .filter(|(_, w)| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
        .map(|(i, _)| i)
        .collect()
}

/// Remove every DHT segment from the header of `data`.
pub fn strip_dht(data: &[u8]) -> Vec<u8> {
    let mut out = data[..2].to_vec();
    let mut p = 2usize;
    while p + 4 <= data.len() && data[p] == 0xFF && data[p + 1] != 0xDA {
        let len = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
        if data[p + 1] != 0xC4 {
            out.extend_from_slice(&data[p..p + 2 + len]);
        }
        p += 2 + len;
    }
    out.extend_from_slice(&data[p..]);
    out
}
