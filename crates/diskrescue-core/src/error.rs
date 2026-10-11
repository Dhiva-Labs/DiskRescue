//! Error type shared by all crates.

use crate::source::ReadError;

/// The error type used across DiskRescue.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A plain OS or filesystem error (opening files, creating output).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A read from a [`crate::Source`] failed. Carries offset and kind so the
    /// caller can decide whether to skip, retry or mark the range bad.
    #[error(transparent)]
    Read(#[from] ReadError),

    /// The input is recognised but uses a feature we do not implement yet.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// On-disk structures are inconsistent. `what` names the structure
    /// (e.g. "FAT boot sector", "MFT record", "moov box").
    #[error("corrupt {what}: {detail}")]
    Corrupt { what: &'static str, detail: String },

    /// The caller passed something invalid (bad path, bad option combination).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Something that should exist does not (device, partition, file).
    #[error("not found: {0}")]
    NotFound(String),

    /// Operation refused for safety (writing to the source device, etc.).
    #[error("refused: {0}")]
    Refused(String),

    /// Catch-all with a message.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Convenience constructor for [`Error::Corrupt`].
    pub fn corrupt(what: &'static str, detail: impl Into<String>) -> Self {
        Error::Corrupt {
            what,
            detail: detail.into(),
        }
    }

    /// Convenience constructor for [`Error::Unsupported`].
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }

    /// Convenience constructor for [`Error::Other`].
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }

    /// True when the underlying cause is a read failure on the source, which
    /// callers usually treat as "this region is unreadable" rather than fatal.
    pub fn is_read_error(&self) -> bool {
        matches!(self, Error::Read(_))
    }
}

/// Result alias used across DiskRescue.
pub type Result<T, E = Error> = std::result::Result<T, E>;
