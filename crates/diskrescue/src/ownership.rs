//! Give recovered files back to the user who ran `sudo`.
//!
//! Reading a raw device usually needs root, so most people run
//! `sudo diskrescue ...`. Without this, everything written (the image, the
//! recovered photos, the reports) would belong to root and the user could
//! not move or delete it without sudo again.
//!
//! Only entries that this run created are changed: they must be owned by
//! root, lie inside the paths the command writes to, and have been modified
//! since the command started. Symbolic links are never followed.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The user to hand files back to: `SUDO_UID`/`SUDO_GID` when running as
/// root under sudo.
#[cfg(unix)]
fn sudo_owner() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    (uid != 0).then_some((uid, gid))
}

/// Hand every entry this run created under `roots` (files or directories)
/// back to the sudo user. Errors are logged, never fatal.
#[cfg(unix)]
pub fn hand_back(roots: &[PathBuf], started: SystemTime) {
    let Some((uid, gid)) = sudo_owner() else {
        return;
    };
    let mut changed = 0u64;
    for root in roots {
        walk(root, started, uid, gid, &mut changed, 0);
    }
    if changed > 0 {
        log::info!("gave {changed} file(s) back to user {uid}");
    }
}

#[cfg(unix)]
fn walk(path: &Path, started: SystemTime, uid: u32, gid: u32, changed: &mut u64, depth: u32) {
    use std::os::unix::fs::MetadataExt;
    if depth > 64 {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let ours = meta.uid() == 0 && meta.modified().map(|m| m >= started).unwrap_or(false);
    if ours {
        match std::os::unix::fs::lchown(path, Some(uid), Some(gid)) {
            Ok(()) => *changed += 1,
            Err(e) => log::warn!("could not give {} back to user {uid}: {e}", path.display()),
        }
    }
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                walk(&entry.path(), started, uid, gid, changed, depth + 1);
            }
        }
    }
}

/// No-op outside Unix.
#[cfg(not(unix))]
pub fn hand_back(_roots: &[PathBuf], _started: SystemTime) {}

/// The image file plus every sidecar the imaging step writes next to it
/// (`IMAGE.map`, `IMAGE.map.bak`, `IMAGE.map.device.json`, ...).
pub fn image_and_sidecars(image: &Path, map: Option<&Path>) -> Vec<PathBuf> {
    let mut out = vec![image.to_path_buf()];
    let map = map
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{}.map", image.display())));
    if let (Some(dir), Some(name)) = (map.parent(), map.file_name()) {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let prefix = name.to_string_lossy().into_owned();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    out.push(e.path());
                }
            }
        }
    }
    out.push(map);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecars_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("card.img");
        for name in [
            "card.img",
            "card.img.map",
            "card.img.map.bak",
            "card.img.map.device.json",
            "other.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let got = image_and_sidecars(&img, None);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for want in [
            "card.img",
            "card.img.map",
            "card.img.map.bak",
            "card.img.map.device.json",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        assert!(!names.iter().any(|n| n == "other.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn hand_back_is_a_noop_when_not_root_under_sudo() {
        // The test suite never runs as root; this must simply do nothing.
        let dir = tempfile::tempdir().unwrap();
        hand_back(&[dir.path().to_path_buf()], SystemTime::now());
    }
}
