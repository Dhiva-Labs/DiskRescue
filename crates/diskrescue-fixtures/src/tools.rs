//! Locating and running the external tools the builders depend on.
//!
//! Every lookup is cached for the life of the process so tests can call
//! [`have`] freely. Nothing here needs root; the tools are run as the current
//! user with a clean argument list (no shell).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

use crate::error::{FixtureError, Result};

/// A required external tool (or Python module) is not available.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("required tool missing: {0}")]
pub struct Missing(pub String);

fn cache() -> &'static Mutex<HashMap<String, Option<PathBuf>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Search `PATH` (plus the sbin directories, which are often not on a
/// user's `PATH`) for an executable called `name`.
pub fn find(name: &str) -> Option<PathBuf> {
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(name).cloned()) {
        return hit;
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in [
        "/usr/local/sbin",
        "/usr/sbin",
        "/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
    ] {
        dirs.push(PathBuf::from(extra));
    }
    let found = dirs
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| is_executable(p));
    if let Ok(mut c) = cache().lock() {
        c.insert(name.to_string(), found.clone());
    }
    found
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// True when `name` is an executable we can run.
pub fn have(name: &str) -> bool {
    find(name).is_some()
}

/// `Ok(())` when the tool exists, otherwise `Err(Missing)`.
pub fn require(name: &str) -> std::result::Result<PathBuf, Missing> {
    find(name).ok_or_else(|| Missing(name.to_string()))
}

/// Like [`run`] with extra environment variables for the child.
pub fn run_env(cmd: &str, args: &[&str], env: &[(&str, &str)]) -> Result<Output> {
    let exe = require(cmd)?;
    let mut c = Command::new(&exe);
    c.args(args).stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    log::debug!("run: {} {}", cmd, args.join(" "));
    let out = c.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FixtureError::Missing(Missing(cmd.to_string()))
        } else {
            FixtureError::Io(e)
        }
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        log::warn!("{cmd} failed: {} {stderr}", out.status);
        return Err(FixtureError::Tool {
            tool: cmd.to_string(),
            status: out.status.to_string(),
            stderr,
        });
    }
    Ok(out)
}

/// Run `cmd args...` with no stdin, capturing stdout and stderr. Returns
/// `Err(Missing)` when the program is absent and `Err(Tool)` when it exits
/// unsuccessfully; the caller gets the full [`Output`] otherwise.
pub fn run(cmd: &str, args: &[&str]) -> Result<Output> {
    run_in(cmd, args, None)
}

/// Like [`run`] but with a working directory.
pub fn run_in(cmd: &str, args: &[&str], cwd: Option<&Path>) -> Result<Output> {
    let exe = require(cmd)?;
    let mut c = Command::new(&exe);
    c.args(args).stdin(Stdio::null());
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    log::debug!("run: {} {}", cmd, args.join(" "));
    let out = c.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FixtureError::Missing(Missing(cmd.to_string()))
        } else {
            FixtureError::Io(e)
        }
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        log::warn!("{cmd} failed: {} {stderr}", out.status);
        return Err(FixtureError::Tool {
            tool: cmd.to_string(),
            status: out.status.to_string(),
            stderr,
        });
    }
    Ok(out)
}

/// Run `cmd args...` and return its [`Output`] whatever the exit status is
/// (only a missing program or a spawn failure is an error). Validators such
/// as `fsck.vfat -n` use this to inspect both the verdict and the messages.
pub fn run_raw(cmd: &str, args: &[&str]) -> Result<Output> {
    let exe = require(cmd)?;
    log::debug!("run_raw: {} {}", cmd, args.join(" "));
    Command::new(&exe)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FixtureError::Missing(Missing(cmd.to_string()))
            } else {
                FixtureError::Io(e)
            }
        })
}

/// Run and return stdout as text (lossy UTF-8).
pub fn run_stdout(cmd: &str, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&run(cmd, args)?.stdout).into_owned())
}

/// Run and report only success/failure (used for validators such as
/// `fsck.vfat -n` whose exit status is the verdict).
pub fn succeeds(cmd: &str, args: &[&str]) -> bool {
    run(cmd, args).is_ok()
}

fn ffmpeg_list(flag: &str) -> &'static str {
    static ENCODERS: OnceLock<String> = OnceLock::new();
    static MUXERS: OnceLock<String> = OnceLock::new();
    let cell = if flag == "-encoders" {
        &ENCODERS
    } else {
        &MUXERS
    };
    cell.get_or_init(|| run_stdout("ffmpeg", &["-hide_banner", flag]).unwrap_or_default())
}

/// True when the local `ffmpeg` lists `name` under `ffmpeg -encoders`.
pub fn ffmpeg_has_encoder(name: &str) -> bool {
    ffmpeg_list("-encoders")
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// True when the local `ffmpeg` lists `name` under `ffmpeg -muxers`.
pub fn ffmpeg_has_muxer(name: &str) -> bool {
    ffmpeg_list("-muxers")
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// True when `python3` can `import PIL`.
pub fn have_pillow() -> bool {
    static PIL: OnceLock<bool> = OnceLock::new();
    *PIL.get_or_init(|| succeeds("python3", &["-c", "import PIL.Image"]))
}

/// `Ok(())` when Pillow is importable.
pub fn require_pillow() -> std::result::Result<(), Missing> {
    if have_pillow() {
        Ok(())
    } else {
        Err(Missing("python3 + Pillow".into()))
    }
}

/// Run a Python snippet with `python3` (stdin is not used; the script is
/// passed with `-c`). `args` become `sys.argv[1..]`.
pub fn python(script: &str, args: &[&str]) -> Result<Output> {
    let mut all = vec!["-c", script];
    all.extend_from_slice(args);
    run("python3", &all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tool_is_missing() {
        assert!(!have("definitely-not-a-tool-xyz"));
        let e = run("definitely-not-a-tool-xyz", &[]).unwrap_err();
        assert!(e.is_missing());
        assert_eq!(
            e.to_string(),
            "required tool missing: definitely-not-a-tool-xyz"
        );
    }

    #[test]
    fn failing_tool_reports_status() {
        if !have("false") {
            eprintln!("skipped: false missing");
            return;
        }
        match run("false", &[]) {
            Err(FixtureError::Tool { tool, .. }) => assert_eq!(tool, "false"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
