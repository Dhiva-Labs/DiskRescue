//! Error type for fixture builders.

use crate::tools::Missing;

/// Anything that can go wrong while building a fixture.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// An external tool (or the Python Pillow module) is not installed.
    #[error(transparent)]
    Missing(#[from] Missing),

    /// Plain I/O failure on the fixture files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// An external tool ran but exited with a failure status.
    #[error("{tool} failed ({status}): {stderr}")]
    Tool {
        /// Program name.
        tool: String,
        /// Exit status as text (`exit status: 1`, `signal: 9`).
        status: String,
        /// Captured standard error, trimmed.
        stderr: String,
    },

    /// The spec asks for something impossible (bad path, bad combination).
    #[error("invalid fixture spec: {0}")]
    Invalid(String),

    /// The volume or directory has no room for what the spec asks for.
    #[error("no space: {0}")]
    Full(String),

    /// The requested variant is not implementable with the tools at hand.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// An on-disk structure the builder has to read back is inconsistent.
    #[error("corrupt {what}: {detail}")]
    Corrupt {
        /// Structure name (`FAT boot sector`, `exFAT root directory`).
        what: &'static str,
        /// Detail.
        detail: String,
    },
}

impl FixtureError {
    /// Convenience constructor for [`FixtureError::Corrupt`].
    pub fn corrupt(what: &'static str, detail: impl Into<String>) -> Self {
        FixtureError::Corrupt {
            what,
            detail: detail.into(),
        }
    }

    /// Convenience constructor for [`FixtureError::Invalid`].
    pub fn invalid(msg: impl Into<String>) -> Self {
        FixtureError::Invalid(msg.into())
    }

    /// True when the failure is "a tool is missing", which tests turn into a
    /// skip rather than a failure.
    pub fn is_missing(&self) -> bool {
        matches!(self, FixtureError::Missing(_))
    }
}

/// Result alias for this crate.
pub type Result<T, E = FixtureError> = std::result::Result<T, E>;
