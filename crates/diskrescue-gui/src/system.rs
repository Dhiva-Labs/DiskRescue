//! Small things the desktop does for us: opening files and folders, the
//! home folder, whether we run as root.

use std::path::{Path, PathBuf};
use std::process::Command;

/// True when the app runs as root (opening files then would start the file
/// viewer as root, so those buttons are hidden).
pub fn is_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The user's home folder (the current folder when unknown).
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The login name, for instructions.
pub fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "$USER".to_string())
}

/// A string usable as a file name: path separators and characters Windows
/// refuses become `_`, surrounding spaces and dots are dropped.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim().trim_matches('.').trim();
    if t.is_empty() {
        "drive".to_string()
    } else {
        t.chars().take(60).collect()
    }
}

fn spawn_detached(mut cmd: Command) -> Result<(), String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd.spawn() {
        Ok(mut child) => {
            // Reap it in the background so that it never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
        Err(e) => Err(format!("could not start {program}: {e}")),
    }
}

/// Open a file or folder with the desktop's default application.
pub fn open_path(path: &Path) -> Result<(), String> {
    let cmd = if cfg!(windows) {
        let mut c = Command::new("explorer");
        c.arg(path);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };
    spawn_detached(cmd)
}

/// Show a file in the file manager (its folder, with the file selected
/// where the system supports it).
pub fn show_in_folder(path: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{}", path.display()));
        return spawn_detached(c);
    }
    if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg("-R").arg(path);
        return spawn_detached(c);
    }
    let folder = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    };
    open_path(&folder)
}

/// A file name for display (the whole path when it has none).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Make a path absolute without touching the disk.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe() {
        assert_eq!(safe_name("SanDisk/Ultra: 32GB"), "SanDisk_Ultra_ 32GB");
        assert_eq!(safe_name("  ..  "), "drive");
    }
}
