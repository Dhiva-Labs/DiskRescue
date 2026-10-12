//! Explaining "permission denied" when opening a device.
//!
//! Reading `/dev/sdX` needs only read permission on the node: the node is
//! normally `root:disk` mode 0660, so either run as root (`sudo`) or be a
//! member of the owning group. `CAP_SYS_RAWIO`/`CAP_SYS_ADMIN` are not
//! needed for `pread` or the `BLK*GET*` ioctls (they are for SMART
//! pass-through). On Windows, raw disk access needs an elevated
//! (administrator) process.

use std::io;
use std::path::Path;

/// A hint for the user when `err` is a permission error opening `path`,
/// `None` for other errors.
pub fn privilege_hint(err: &io::Error, path: &Path) -> Option<String> {
    if !is_permission_error(err) {
        return None;
    }
    Some(platform_hint(err, path))
}

fn is_permission_error(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
    }
    #[cfg(windows)]
    {
        // ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD
        matches!(err.raw_os_error(), Some(5) | Some(1314))
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(unix)]
fn group_name(gid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    text.lines().take(100_000).find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _pw = f.next()?;
        let id: u32 = f.next()?.trim().parse().ok()?;
        (id == gid && !name.is_empty()).then(|| name.to_string())
    })
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // gid_t is u32 on Linux but not on every Unix
fn my_groups() -> Vec<u32> {
    // SAFETY: getgroups(0, NULL) only returns the count; the second call
    // writes at most `n` gid_t values into a buffer of exactly that size.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let mut out: Vec<u32> = Vec::new();
    if n > 0 {
        let mut v: Vec<libc::gid_t> = vec![0; n as usize];
        let m = unsafe { libc::getgroups(n, v.as_mut_ptr()) };
        if m > 0 {
            v.truncate(m as usize);
            out.extend(v.iter().map(|g| *g as u32));
        }
    }
    // SAFETY: getegid has no preconditions.
    out.push(unsafe { libc::getegid() } as u32);
    out
}

#[cfg(unix)]
fn platform_hint(err: &io::Error, path: &Path) -> String {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let p = path.display();
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return format!(
            "running as root but opening {p} was refused ({err}): a security module (SELinux/AppArmor), \
             a container or a sandbox is blocking it"
        );
    }
    let meta = std::fs::metadata(path).ok();
    if let Some(m) = &meta {
        if m.is_file() {
            return format!(
                "you do not have read permission on {p}: check its owner and mode (ls -l), or run with sudo"
            );
        }
    }
    let is_dev = meta
        .as_ref()
        .map(|m| m.file_type().is_block_device() || m.file_type().is_char_device())
        .unwrap_or(true);
    let gid = meta.as_ref().map(|m| m.gid());
    let group = gid
        .and_then(group_name)
        .unwrap_or_else(|| "disk".to_string());
    if let Some(g) = gid {
        if g != 0 && my_groups().contains(&g) {
            return format!(
                "you are in group '{group}' but opening {p} was refused ({err}); if you were added recently, \
                 log out and back in (or run `newgrp {group}`), or run with sudo"
            );
        }
    }
    if is_dev {
        format!(
            "reading {p} needs read permission on the device node: run with sudo, or add yourself to the \
             '{group}' group (sudo usermod -aG {group} $USER, then log out and back in), or grant access for \
             this session only with: sudo setfacl -m u:$USER:r {p}"
        )
    } else {
        format!("permission denied on {p}: run with sudo or fix the permissions")
    }
}

#[cfg(windows)]
fn platform_hint(err: &io::Error, path: &Path) -> String {
    format!(
        "opening {} was refused ({err}): raw disk access needs administrator rights; right-click the terminal \
         and choose \"Run as administrator\", then run DiskRescue again",
        path.display()
    )
}

#[cfg(not(any(unix, windows)))]
fn platform_hint(err: &io::Error, path: &Path) -> String {
    format!(
        "opening {} was refused ({err}): run with administrator rights",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_permission_errors_get_hints() {
        let p = Path::new("/dev/sdz");
        assert!(privilege_hint(&io::Error::from(io::ErrorKind::NotFound), p).is_none());
        let h = privilege_hint(&io::Error::from(io::ErrorKind::PermissionDenied), p).unwrap();
        assert!(h.contains("sudo") || h.contains("root"), "{h}");
        #[cfg(unix)]
        {
            let h = privilege_hint(&io::Error::from_raw_os_error(libc::EPERM), p).unwrap();
            assert!(h.contains("/dev/sdz"));
            assert!(privilege_hint(&io::Error::from_raw_os_error(libc::EIO), p).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_hint_mentions_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("img");
        std::fs::write(&f, b"x").unwrap();
        let h = privilege_hint(&io::Error::from(io::ErrorKind::PermissionDenied), &f).unwrap();
        // Root (in CI containers) gets the security-module message instead.
        assert!(h.contains("ls -l") || h.contains("root"), "{h}");
    }
}
