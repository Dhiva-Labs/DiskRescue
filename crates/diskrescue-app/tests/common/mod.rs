//! Shared helpers for the integration tests. Sources are always regular
//! files; no test opens a block device.

#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use diskrescue_app::{JobHandle, JobOutcome, JobState};
use diskrescue_fixtures::sets::{self, Scenario};

/// Wait for a job with a generous limit (fails the test instead of hanging).
pub fn wait<S: JobState>(h: &JobHandle<S>, limit: Duration) -> JobOutcome {
    h.wait_timeout(limit)
        .unwrap_or_else(|| panic!("the job did not end within {limit:?}"))
}

/// Deterministic pseudo-random bytes.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Compare two files chunk by chunk.
pub fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(fa), Ok(fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    let (Ok(ma), Ok(mb)) = (fa.metadata(), fb.metadata()) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let mut ra = std::io::BufReader::new(fa);
    let mut rb = std::io::BufReader::new(fb);
    let mut x = vec![0u8; 1 << 20];
    let mut y = vec![0u8; 1 << 20];
    loop {
        let n = ra.read(&mut x).unwrap();
        if n == 0 {
            return true;
        }
        rb.read_exact(&mut y[..n]).unwrap();
        if x[..n] != y[..n] {
            return false;
        }
    }
}

/// Disk-backed scratch space (images are preallocated at full size).
pub fn scratch(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

fn cached(
    name: &str,
    ok: impl Fn(&Path, &Scenario) -> bool,
    build: fn(&Path) -> diskrescue_fixtures::Result<Scenario>,
) -> Option<Scenario> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixtures")
        .join(name);
    if let Ok(sc) = Scenario::load(&dir) {
        if ok(&dir, &sc) {
            return Some(sc);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    match build(&dir) {
        Ok(sc) => Some(sc),
        Err(e) if e.is_missing() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("cannot build the {name} fixture: {e}"),
    }
}

/// The camera-card scenario (shared with the CLI tests' cache).
pub fn camera_card() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        cached(
            "camera-sd-card",
            |_, sc| sc.disk_image.as_ref().is_some_and(|p| p.exists()),
            sets::camera_sd_card,
        )
    })
    .as_ref()
}

/// Loose damaged files (shared with the CLI tests' cache).
pub fn damaged_media() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        cached(
            "damaged-media",
            |dir, _| dir.join("no_moov.mp4").exists(),
            sets::damaged_media,
        )
    })
    .as_ref()
}

/// The `diskrescue` command-line program, built from this workspace into
/// the same target folder as this test (or `DISKRESCUE_CLI`). `None` (with
/// a message) when it cannot be built.
pub fn cli_binary() -> Option<PathBuf> {
    static CELL: OnceLock<Option<PathBuf>> = OnceLock::new();
    CELL.get_or_init(|| {
        if let Some(p) = std::env::var_os("DISKRESCUE_CLI") {
            return Some(PathBuf::from(p));
        }
        // This test runs from <target>/<profile>/deps/<name>.
        let exe = std::env::current_exe().ok()?;
        let profile_dir = exe.parent()?.parent()?.to_path_buf();
        let target_dir = profile_dir.parent()?.to_path_buf();
        let profile = profile_dir.file_name()?.to_string_lossy().into_owned();
        let bin = profile_dir.join(format!("diskrescue{}", std::env::consts::EXE_SUFFIX));
        let cargo = std::env::var_os("CARGO")
            .or_else(|| option_env!("CARGO").map(Into::into))
            .unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args([
            "build",
            "--quiet",
            "-p",
            "diskrescue",
            "--bin",
            "diskrescue",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"));
        match profile.as_str() {
            "debug" => {}
            "release" => {
                cmd.arg("--release");
            }
            other => {
                cmd.args(["--profile", other]);
            }
        }
        let built = match cmd.spawn() {
            Ok(mut child) => {
                let t = Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(st)) => break st.success(),
                        Ok(None) if t.elapsed() > Duration::from_secs(900) => {
                            let _ = child.kill();
                            break false;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                        Err(_) => break false,
                    }
                }
            }
            Err(_) => false,
        };
        if !built {
            eprintln!(
                "could not build the diskrescue program; using an existing build if there is one"
            );
        }
        if bin.is_file() {
            Some(bin)
        } else {
            eprintln!("skipped: no diskrescue program at {}", bin.display());
            None
        }
    })
    .clone()
}
