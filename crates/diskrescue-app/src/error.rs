//! Errors of the front-end layer and how they are explained to people.

use std::path::Path;

use diskrescue_core::Error as CoreError;

/// Why an operation was not started or did not finish.
///
/// The three kinds match the command-line exit codes: `Refused` is 3
/// (refused for safety), `Invalid` is 2 (the request does not make sense)
/// and `Failed` is 1. The text is a complete sentence for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppError {
    /// Refused because it would be unsafe (output on the source, a mounted
    /// source, an image that would be overwritten, ...).
    #[error("{0}")]
    Refused(String),
    /// The request itself is not valid (unknown profile, an offset beyond
    /// the end of the source, an image path that is a folder, ...).
    #[error("{0}")]
    Invalid(String),
    /// Anything else that went wrong.
    #[error("{0}")]
    Failed(String),
}

/// Result alias of this crate.
pub type Result<T, E = AppError> = std::result::Result<T, E>;

impl AppError {
    /// Shorthand for [`AppError::Failed`].
    pub fn failed(msg: impl Into<String>) -> Self {
        AppError::Failed(msg.into())
    }

    /// Shorthand for [`AppError::Refused`].
    pub fn refused(msg: impl Into<String>) -> Self {
        AppError::Refused(msg.into())
    }

    /// Shorthand for [`AppError::Invalid`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        AppError::Invalid(msg.into())
    }

    /// The text for the user.
    pub fn message(&self) -> &str {
        match self {
            AppError::Refused(m) | AppError::Invalid(m) | AppError::Failed(m) => m,
        }
    }

    /// True for [`AppError::Refused`].
    pub fn is_refusal(&self) -> bool {
        matches!(self, AppError::Refused(_))
    }
}

impl From<CoreError> for AppError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Refused(m) => AppError::Refused(m),
            CoreError::InvalidArgument(m) => AppError::Failed(m),
            CoreError::NotFound(m) => AppError::Failed(m),
            CoreError::Io(io) => AppError::Failed(explain_io(&io, None)),
            other => AppError::Failed(other.to_string()),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Failed(explain_io(&e, None))
    }
}

/// Plain-language text for an I/O error, with a hint for the usual causes
/// (permissions, a full disk, a file too large for FAT32).
pub fn explain_io(e: &std::io::Error, path: Option<&Path>) -> String {
    use std::io::ErrorKind::*;
    let text = e.to_string();
    let mut msg = match path {
        Some(p) if !text.contains(&p.display().to_string()) => format!("{}: {text}", p.display()),
        _ => text,
    };
    if let Some(p) = path {
        if let Some(h) = diskrescue_device::privilege_hint(e, p) {
            if !msg.contains(&h) {
                msg.push_str(". ");
                msg.push_str(&h);
            }
        }
    }
    let efbig = e.kind() == FileTooLarge || (cfg!(unix) && e.raw_os_error() == Some(27));
    let enospc = e.kind() == StorageFull || (cfg!(unix) && e.raw_os_error() == Some(28));
    if enospc {
        msg.push_str(
            ". The disk is full: free some space or choose a different location with more room",
        );
    } else if efbig {
        msg.push_str(
            ". The filesystem of that disk cannot hold a file this large (FAT32 stops at 4 GiB): \
             choose a disk formatted as NTFS, exFAT or ext4",
        );
    }
    msg
}

/// True when `e` is "permission denied" (or Windows' access denied /
/// privilege not held).
pub fn is_permission_error(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM))
    }
    #[cfg(windows)]
    {
        matches!(e.raw_os_error(), Some(5) | Some(1314))
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_keep_their_meaning() {
        let e: AppError = CoreError::Refused("on the source".into()).into();
        assert!(e.is_refusal());
        assert_eq!(e.message(), "on the source");
        let e: AppError = CoreError::other("boom").into();
        assert_eq!(e, AppError::Failed("boom".into()));
        let e: AppError = CoreError::InvalidArgument("bad".into()).into();
        assert_eq!(e, AppError::Failed("bad".into()));
    }

    #[cfg(unix)]
    #[test]
    fn full_disk_big_file_and_permissions_are_explained() {
        let m = explain_io(
            &std::io::Error::from_raw_os_error(28),
            Some(Path::new("/tmp/x")),
        );
        assert!(m.contains("disk is full"), "{m}");
        let m = explain_io(
            &std::io::Error::from_raw_os_error(27),
            Some(Path::new("/tmp/x")),
        );
        assert!(m.contains("FAT32"), "{m}");
        assert!(is_permission_error(&std::io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!is_permission_error(&std::io::Error::from_raw_os_error(
            libc::EIO
        )));
    }
}
