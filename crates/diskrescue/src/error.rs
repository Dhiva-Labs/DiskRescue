//! Errors of the command-line layer and how they are shown to the user.
//!
//! The plain-language explanations live in `diskrescue_app::error`; this
//! module maps them onto exit codes.

#[cfg(all(test, unix))]
use std::path::Path;

use diskrescue_app::AppError;
use diskrescue_core::Error as CoreError;

#[cfg(all(test, unix))]
pub use diskrescue_app::error::explain_io;

/// Exit codes documented in `docs/USAGE.md`.
pub mod exit {
    /// Finished.
    pub const OK: i32 = 0;
    /// Error.
    pub const ERROR: i32 = 1;
    /// Invalid command line.
    pub const USAGE: i32 = 2;
    /// Refused for safety.
    pub const REFUSED: i32 = 3;
    /// Interrupted (progress was saved).
    pub const INTERRUPTED: i32 = 130;
}

/// Why a command did not finish normally.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Refused to start because it would be unsafe. Exit code 3.
    #[error("{0}")]
    Refused(String),
    /// The command line is valid for the parser but not for this command. Exit code 2.
    #[error("{0}")]
    Usage(String),
    /// Anything else that went wrong. Exit code 1.
    #[error("{0}")]
    Failed(String),
}

/// Result of a command: the exit code to use when it finishes without an error.
pub type CmdResult = Result<i32, CliError>;

impl CliError {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Refused(_) => exit::REFUSED,
            CliError::Usage(_) => exit::USAGE,
            CliError::Failed(_) => exit::ERROR,
        }
    }

    /// Shorthand for [`CliError::Failed`].
    pub fn failed(msg: impl Into<String>) -> Self {
        CliError::Failed(msg.into())
    }

    /// Shorthand for [`CliError::Refused`].
    #[cfg(test)]
    pub fn refused(msg: impl Into<String>) -> Self {
        CliError::Refused(msg.into())
    }

    /// Shorthand for [`CliError::Usage`].
    #[cfg(test)]
    pub fn usage(msg: impl Into<String>) -> Self {
        CliError::Usage(msg.into())
    }
}

impl From<AppError> for CliError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::Refused(m) => CliError::Refused(m),
            AppError::Invalid(m) => CliError::Usage(m),
            AppError::Failed(m) => CliError::Failed(m),
        }
    }
}

impl From<CoreError> for CliError {
    fn from(e: CoreError) -> Self {
        AppError::from(e).into()
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        AppError::from(e).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_documentation() {
        assert_eq!(CliError::refused("x").exit_code(), 3);
        assert_eq!(CliError::usage("x").exit_code(), 2);
        assert_eq!(CliError::failed("x").exit_code(), 1);
    }

    #[test]
    fn core_refusals_stay_refusals() {
        let e: CliError = CoreError::Refused("on the source".into()).into();
        assert_eq!(e.exit_code(), 3);
        let e: CliError = CoreError::other("boom").into();
        assert_eq!(e.exit_code(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn full_disk_and_big_file_are_explained() {
        let e = std::io::Error::from_raw_os_error(28);
        let m = explain_io(&e, Some(Path::new("/tmp/x")));
        assert!(m.contains("disk is full"), "{m}");
        let e = std::io::Error::from_raw_os_error(27);
        let m = explain_io(&e, Some(Path::new("/tmp/x")));
        assert!(m.contains("FAT32"), "{m}");
    }
}
