//! Helpers shared by the format implementations: turning walk failures into
//! [`Measured`] values and measurements into [`Validation`] verdicts.

use diskrescue_core::format::{Confidence, ExtentStatus, Measured, Validation, ValidationStatus};
use diskrescue_core::source::{SharedSource, Source};
use diskrescue_core::Result;

use crate::win::{Fail, Win};

/// A measurement together with the total size the file *claims* to have, so
/// that `validate` can report how much of it is present.
#[derive(Debug, Clone)]
pub(crate) struct Inspected {
    /// The measurement handed to the carver.
    pub measured: Measured,
    /// Declared total size, when the format states one.
    pub expected: Option<u64>,
}

impl Inspected {
    pub(crate) fn new(measured: Measured, expected: Option<u64>) -> Self {
        Inspected { measured, expected }
    }
}

/// Turn a failure that stopped a walk into a measurement.
///
/// * `Eof` with a limit-induced window end becomes `Capped` at the window end.
/// * `Eof` at the real end of the source becomes `Truncated` at that offset.
/// * `Io` becomes `Truncated` at the unreadable offset.
///
/// `conf` is the confidence the file would have had if complete.
pub(crate) fn failed(win: &Win, fail: Fail, conf: Confidence, what: &str) -> Measured {
    match fail {
        Fail::Eof(_) if win.capped() => Measured::capped(win.avail(), conf.lowered(40)).with_note(
            format!("measurement stopped at the size limit while reading {what}"),
        ),
        Fail::Eof(_) => Measured::truncated(
            win.avail(),
            conf.lowered(30),
            win.avail(),
            format!("source ends inside {what}"),
        ),
        Fail::Io(at) => Measured::truncated(
            at,
            conf.lowered(30),
            at,
            format!("unreadable bytes while reading {what}"),
        ),
    }
}

/// Measurement for a file whose structure ends cleanly at `declared` bytes:
/// `Complete` when that fits in the window, otherwise capped or truncated
/// at the window end.
pub(crate) fn sized(win: &Win, declared: u64, conf: Confidence, what: &str) -> Measured {
    if declared <= win.avail() {
        Measured::complete(declared, conf)
    } else if win.capped() {
        Measured::capped(win.avail(), conf.lowered(40)).with_note(format!(
            "{what} extends to {declared} bytes, beyond the measurement limit"
        ))
    } else {
        Measured::truncated(
            win.avail(),
            conf.lowered(30),
            win.avail(),
            format!(
                "{what} needs {declared} bytes but the source ends after {}",
                win.avail()
            ),
        )
    }
}

/// Trailing bytes after a measured end that `validate` tolerates as
/// filesystem slack.
const TRAILING_SLACK: u64 = 64 * 1024;

/// Build a [`Validation`] for a file spanning the whole of `src` from the
/// result of a fresh measurement.
pub(crate) fn verdict(what: &str, size: u64, inspected: Option<Inspected>) -> Result<Validation> {
    let Some(Inspected { measured, expected }) = inspected else {
        return Ok(Validation::invalid(format!("not a valid {what} file")));
    };
    let mut problems: Vec<String> = measured.notes.clone();
    let mut status = ValidationStatus::Valid;
    let mut fraction: Option<f32> = Some(1.0);
    if measured.status == ExtentStatus::Complete {
        if measured.len > size {
            status = ValidationStatus::Damaged;
            fraction = Some((size as f64 / measured.len.max(1) as f64) as f32);
            problems.push("measured length exceeds the file size".to_string());
        } else if measured.len < size {
            let extra = size - measured.len;
            problems.push(format!(
                "{extra} bytes of trailing data after the end of the {what} structure"
            ));
            if extra > TRAILING_SLACK {
                status = ValidationStatus::Damaged;
                fraction = Some((measured.len as f64 / size.max(1) as f64) as f32);
            }
        }
    } else if measured.status == ExtentStatus::UnknownLength {
        // The format does not store the length of its data (RW2): the
        // structure is sound, the rest cannot be verified.
        fraction = None;
        problems.push(
            "the length of the raw data is not stored in the file; it was not verified".into(),
        );
    } else {
        status = ValidationStatus::Damaged;
        fraction = expected
            .filter(|e| *e > 0)
            .map(|e| (measured.len as f64 / e as f64).clamp(0.0, 1.0) as f32);
    }
    Ok(Validation {
        status,
        intact_fraction: fraction,
        problems,
        meta: measured.meta,
        repairable_by: Vec::new(),
    })
}

/// Run `inspect` over a whole file and produce its validation.
pub(crate) fn validate_with(
    what: &str,
    src: &SharedSource,
    inspect: impl Fn(&SharedSource, u64, u64) -> Result<Option<Inspected>>,
) -> Result<Validation> {
    let size = src.size();
    let ins = inspect(src, 0, size)?;
    verdict(what, size, ins)
}

/// Case-insensitive `starts_with` on ASCII text.
pub(crate) fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Log the outcome of a measurement: `debug` for the summary, `warn` when the
/// medium itself failed inside the file.
pub(crate) fn log_measured(win: &Win, name: &str, m: &Measured, read_failed: bool) {
    log::debug!(
        "{name} at {}: {} bytes, {:?}, confidence {}, err_offset {:?}",
        win.start(),
        m.len,
        m.status,
        m.confidence.value(),
        m.err_offset
    );
    if read_failed {
        log::warn!(
            "{name} at {}: unreadable bytes at file offset {:?}; measurement stops there",
            win.start(),
            m.err_offset
        );
    }
    for n in &m.notes {
        log::trace!("{name} at {}: {n}", win.start());
    }
}
