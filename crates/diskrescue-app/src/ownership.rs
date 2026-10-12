//! Give recovered files back to the user who asked for elevated rights.
//!
//! Reading a raw device usually needs root, so most people run
//! `sudo diskrescue ...` (or a desktop app starts the command through
//! `pkexec`). Without this, everything written (the image, the recovered
//! photos, the reports) would belong to root and the user could not move
//! or delete it without administrator rights again.
//!
//! Only entries that this run created are changed: they must be owned by
//! root, lie inside the paths the operation writes to, and have been
//! modified since it started. Symbolic links are never followed.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The user to hand files back to when running as root: `SUDO_UID` and
/// `SUDO_GID` under sudo, `PKEXEC_UID` (and that user's primary group)
/// under pkexec.
#[cfg(unix)]
fn elevated_owner() -> Option<(u32, Option<u32>)> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let var = |name: &str| -> Option<u32> { std::env::var(name).ok()?.trim().parse().ok() };
    if let (Some(uid), Some(gid)) = (var("SUDO_UID"), var("SUDO_GID")) {
        return (uid != 0).then_some((uid, Some(gid)));
    }
    let uid = var("PKEXEC_UID")?;
    (uid != 0).then(|| (uid, primary_group(uid)))
}

/// The primary group of `uid` from the user database.
#[cfg(unix)]
fn primary_group(uid: u32) -> Option<u32> {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: `pwd` is a zeroed passwd struct, `buf` is a writable buffer of
    // the length passed, and `result` receives either null or `&mut pwd`;
    // only the plain integer `pw_gid` is read, and only on success.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            uid as libc::uid_t,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(pwd.pw_gid as u32)
    }
}

/// Hand every entry this run created under `roots` (files or directories)
/// back to the user who elevated. Errors are logged, never fatal. Does
/// nothing unless running as root under sudo or pkexec.
#[cfg(unix)]
pub fn hand_back(roots: &[PathBuf], started: SystemTime) {
    let Some((uid, gid)) = elevated_owner() else {
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
fn walk(
    path: &Path,
    started: SystemTime,
    uid: u32,
    gid: Option<u32>,
    changed: &mut u64,
    depth: u32,
) {
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
        match std::os::unix::fs::lchown(path, Some(uid), gid) {
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
    fn sidecars_include_the_identity_file() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["card.img", "card.img.map", "card.img.map.device.json", "x"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let got = image_and_sidecars(&dir.path().join("card.img"), None);
        assert!(got.iter().any(|p| p.ends_with("card.img.map.device.json")));
        assert!(!got.iter().any(|p| p.ends_with("x")));
    }

    #[cfg(unix)]
    #[test]
    fn nothing_happens_without_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(elevated_owner().is_none());
        let dir = tempfile::tempdir().unwrap();
        hand_back(&[dir.path().to_path_buf()], SystemTime::now());
        // The current user's primary group can be looked up.
        let me = unsafe { libc::getuid() };
        assert!(primary_group(me).is_some());
    }
}
