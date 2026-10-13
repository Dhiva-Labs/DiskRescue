//! Shared helpers for the NTFS integration tests: external tool discovery,
//! image creation with mkntfs/ntfscp, an ntfs-3g FUSE mount guard that
//! always unmounts, deterministic test data and small FileSystem helpers.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::fs::{collect_entries, FileEntry, FileReadReport, FileSystem};
use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::SharedSource;
use diskrescue_ntfs::{open, NtfsFs, NtfsOptions};

/// Find an external tool in PATH or the sbin directories (mkntfs and
/// ntfscp live in /usr/sbin, which is not always in a user's PATH).
pub fn tool(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// True when every tool is available; otherwise prints the skip message.
pub fn have(tools: &[&str]) -> bool {
    for t in tools {
        if tool(t).is_none() {
            eprintln!("skipped: {t} missing");
            return false;
        }
    }
    true
}

/// True when FUSE mounts of NTFS images are possible: the tools exist,
/// /dev/fuse opens, and a trial mount of a scratch image works (checked once
/// per test binary). Prints the skip reason otherwise.
pub fn have_fuse() -> bool {
    static TRIAL: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    if !have(&["mkntfs", "ntfs-3g", "fusermount3"]) {
        return false;
    }
    let trial = TRIAL.get_or_init(|| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
            .map_err(|e| format!("/dev/fuse unusable ({e})"))?;
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let img = tmp.path().join("trial.img");
        mkntfs(&img, 8, 4096, "TRIAL", &[]);
        let m = Mount::try_new(&img, "")?;
        m.unmount();
        Ok(())
    });
    match trial {
        Ok(()) => true,
        Err(e) => {
            eprintln!("skipped: ntfs-3g FUSE mount unavailable: {e}");
            false
        }
    }
}

fn run(cmd: &mut Command) {
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("cannot run {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Create an NTFS image of `size_mib` MiB.
pub fn mkntfs(img: &Path, size_mib: u64, cluster: u32, label: &str, extra: &[&str]) {
    let f = std::fs::File::create(img).unwrap();
    f.set_len(size_mib << 20).unwrap();
    drop(f);
    let mut cmd = Command::new(tool("mkntfs").unwrap());
    cmd.args(["-F", "-Q", "-q", "-L", label, "-c", &cluster.to_string()])
        .args(extra)
        .arg(img);
    run(&mut cmd);
}

/// Copy a host file into the image with ntfscp (no mount needed).
pub fn ntfscp(img: &Path, src: &Path, dest: &str) {
    run(Command::new(tool("ntfscp").unwrap())
        .arg(img)
        .arg(src)
        .arg(dest));
}

/// Content of a file through ntfscat (oracle).
pub fn ntfscat(img: &Path, path: &str) -> Vec<u8> {
    let out = Command::new(tool("ntfscat").unwrap())
        .arg(img)
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "ntfscat {path} failed");
    out.stdout
}

/// An ntfs-3g mount of an image under a private temporary directory.
/// Dropping it unmounts with `fusermount3 -u` (falling back to a lazy
/// unmount) and waits for ntfs-3g to exit so the image is fully written,
/// even when the test panicked.
pub struct Mount {
    pub dir: PathBuf,
    child: Option<Child>,
    _tmp: tempfile::TempDir,
}

impl Mount {
    /// Mount `img` read-write. `opts` are extra `-o` options. Panics when
    /// the mount fails.
    pub fn new(img: &Path, opts: &str) -> Mount {
        Mount::try_new(img, opts).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Like [`Mount::new`] but reports failure instead of panicking.
    pub fn try_new(img: &Path, opts: &str) -> Result<Mount, String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let dir = tmp.path().join("mnt");
        std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
        let before = dev_of(&dir);
        let log_path = tmp.path().join("ntfs-3g.log");
        let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
        let mut cmd = Command::new(tool("ntfs-3g").unwrap());
        // no_detach keeps ntfs-3g in the foreground so the guard can wait
        // for it to finish writing the image after unmounting.
        let mut o = String::from("no_detach");
        if !opts.is_empty() {
            o.push(',');
            o.push_str(opts);
        }
        cmd.args(["-o", &o])
            .arg(img)
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        let child = cmd.spawn().map_err(|e| format!("spawn ntfs-3g: {e}"))?;
        // From here on the guard owns the process: any early return
        // unmounts (if needed) and reaps it.
        let mut m = Mount {
            dir,
            child: Some(child),
            _tmp: tmp,
        };
        let start = Instant::now();
        loop {
            if dev_of(&m.dir) != before {
                return Ok(m);
            }
            if let Some(Ok(Some(st))) = m.child.as_mut().map(|c| c.try_wait()) {
                m.child = None;
                let err = std::fs::read_to_string(&log_path).unwrap_or_default();
                return Err(format!("ntfs-3g exited early ({st}): {err}"));
            }
            if start.elapsed() > Duration::from_secs(20) {
                return Err("ntfs-3g did not mount within 20 s".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Path inside the mount.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    /// Unmount now (also done on drop).
    pub fn unmount(mut self) {
        self.do_unmount();
    }

    fn do_unmount(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let fm = tool("fusermount3").unwrap_or_else(|| PathBuf::from("fusermount3"));
        let mut ok = false;
        for _ in 0..50 {
            let st = Command::new(&fm)
                .arg("-u")
                .arg(&self.dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if st.map(|s| s.success()).unwrap_or(false) {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !ok {
            let _ = Command::new(&fm)
                .arg("-uz")
                .arg(&self.dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < Duration::from_secs(30) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        self.do_unmount();
    }
}

#[cfg(unix)]
fn dev_of(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).map(|m| m.dev()).unwrap_or(0)
}

/// Mounting (ntfs-3g) is Linux-only; elsewhere the mount tests skip.
#[cfg(not(unix))]
fn dev_of(_: &Path) -> u64 {
    0
}

/// Deterministic pseudo-random bytes (incompressible).
pub fn data(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Compressible text.
pub fn text(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut i = 0u64;
    while out.len() < len {
        out.extend_from_slice(
            format!("line {i}: the quick brown fox jumps over the lazy dog\n").as_bytes(),
        );
        i += 1;
    }
    out.truncate(len);
    out
}

/// Open an image file with the given options.
pub fn open_img(img: &Path, opts: &NtfsOptions) -> NtfsFs {
    let src: SharedSource = Arc::new(FileSource::open(img).unwrap());
    open(src, opts).unwrap_or_else(|e| panic!("open {}: {e}", img.display()))
}

/// Open an in-memory copy of an image.
pub fn open_mem(bytes: Vec<u8>, opts: &NtfsOptions) -> diskrescue_core::Result<NtfsFs> {
    open(Arc::new(MemSource::new(bytes)), opts)
}

/// All entries.
pub fn entries(fs: &NtfsFs) -> Vec<FileEntry> {
    collect_entries(fs).expect("walk")
}

/// Find a non-ADS entry by path.
pub fn find<'a>(entries: &'a [FileEntry], path: &str) -> &'a FileEntry {
    entries.iter().find(|e| e.path == path).unwrap_or_else(|| {
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        panic!("no entry {path}; have {paths:?}")
    })
}

/// Read an entry into memory.
pub fn read(fs: &NtfsFs, e: &FileEntry) -> (Vec<u8>, FileReadReport) {
    let mut out = Vec::new();
    let r = fs
        .read_file(e, &mut out)
        .unwrap_or_else(|err| panic!("read {}: {err}", e.path));
    assert_eq!(r.bytes_written, out.len() as u64);
    (out, r)
}

/// Write a host file.
pub fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
