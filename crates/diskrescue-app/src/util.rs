//! Small filesystem helpers: free space, allocated size, same-file checks,
//! wildcard expansion of file inputs.

use std::path::{Path, PathBuf};

/// Free space, in bytes, on the filesystem that holds `path` (or its nearest
/// existing ancestor). `None` when it cannot be determined.
pub fn free_space(path: &Path) -> Option<u64> {
    let mut cur = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    while !cur.exists() {
        if !cur.pop() {
            return None;
        }
    }
    free_space_of_existing(&cur)
}

#[cfg(unix)]
fn free_space_of_existing(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string and `st` is a properly
    // sized, writable statvfs buffer that statvfs fully initialises on success.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
}

#[cfg(windows)]
fn free_space_of_existing(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // SAFETY: `wide` is NUL terminated and the three out-pointers refer to
    // live u64 variables for the duration of the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

#[cfg(not(any(unix, windows)))]
fn free_space_of_existing(_path: &Path) -> Option<u64> {
    None
}

/// Bytes already allocated on disk for `meta` (sparse files count less).
pub fn allocated_bytes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512).min(meta.len())
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// True when both paths exist and name the same file (after resolving links).
pub fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// The folder that will contain `file` (the current folder when none is given).
pub fn parent_dir(file: &Path) -> PathBuf {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Case-insensitive `*` / `?` wildcard match on a file name.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand `*` and `?` in file names that the shell left alone (Windows `cmd`
/// does not expand them) and check that every input exists and is a file.
/// Duplicates are dropped.
pub fn expand_file_args(args: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for a in args {
        let name = a
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !a.exists() && (name.contains('*') || name.contains('?')) {
            let dir = match a.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| format!("cannot read the folder {}: {e}", dir.display()))?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && wildcard_match(&name, &e.file_name().to_string_lossy())
                })
                .map(|e| e.path())
                .collect();
            found.sort();
            if found.is_empty() {
                return Err(format!("no files match {}", a.display()));
            }
            out.extend(found);
            continue;
        }
        match std::fs::metadata(a) {
            Ok(m) if m.is_file() => out.push(a.clone()),
            Ok(m) if m.is_dir() => {
                return Err(format!(
                    "{} is a folder; give the files inside it (for example {}/*.jpg)",
                    a.display(),
                    a.display()
                ))
            }
            Ok(_) => {
                return Err(format!(
                    "{} is a device, not a file. This command works on files; \
                     to get files back from a device use 'diskrescue recover'",
                    a.display()
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{} does not exist", a.display()))
            }
            Err(e) => return Err(format!("cannot read {}: {e}", a.display())),
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_and_parent() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a");
        std::fs::write(&f, b"x").unwrap();
        assert!(same_file(&f, &dir.path().join(".").join("a")));
        assert!(!same_file(&f, &dir.path().join("missing")));
        assert_eq!(parent_dir(Path::new("card.img")), PathBuf::from("."));
    }

    #[test]
    fn wildcards_expand_and_check_inputs() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.jpg", "b.JPG", "c.png"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        assert_eq!(
            expand_file_args(&[dir.path().join("*.jpg")]).unwrap().len(),
            2
        );
        assert!(expand_file_args(&[dir.path().to_path_buf()])
            .unwrap_err()
            .contains("folder"));
        assert!(free_space(dir.path()).is_some());
    }
}
