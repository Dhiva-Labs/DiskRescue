//! Logging to standard error.
//!
//! By default only errors are shown (the library crates report damage
//! through events and reports); `-v` adds progress detail and `-vv`
//! debugging output. Log lines never garble a progress bar and never reach
//! the full-screen view: while it is active they go to its log panel.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

/// Lines captured for the full-screen view while it is active.
static CAPTURE: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

/// Keep this many captured lines.
const CAPTURE_LINES: usize = 200;

struct CliLogger {
    inner: env_logger::Logger,
}

impl Log for CliLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.inner.matches(record) {
            return;
        }
        if let Ok(mut guard) = CAPTURE.lock() {
            if let Some(buf) = guard.as_mut() {
                if buf.len() >= CAPTURE_LINES {
                    buf.pop_front();
                }
                buf.push_back(format!("{}: {}", record.level(), record.args()));
                return;
            }
        }
        self.inner.log(record);
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Writes through the progress-bar group so that bars are redrawn cleanly.
struct BarSafeWriter;

impl Write for BarSafeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mp = crate::progress::multi();
        let mut res = Ok(buf.len());
        mp.suspend(|| {
            let mut err = std::io::stderr().lock();
            res = err.write_all(buf).map(|_| buf.len());
        });
        res
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

static INIT: OnceLock<()> = OnceLock::new();

/// Level filter for `-v` occurrences.
pub fn level_for(verbose: u8) -> LevelFilter {
    match verbose {
        0 => LevelFilter::Error,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

/// Install the logger. `RUST_LOG` overrides the `-v` level when set.
pub fn init(verbose: u8) {
    INIT.get_or_init(|| {
        let mut b = env_logger::Builder::new();
        b.filter_level(level_for(verbose));
        if verbose < 2 {
            // The imaging engine's messages also arrive as events that the
            // screen already shows.
            b.filter_module("diskrescue_imaging", LevelFilter::Error);
        }
        b.parse_env("RUST_LOG");
        b.format(|buf, record| {
            let level = record.level();
            match level {
                log::Level::Error => writeln!(buf, "error: {}", record.args()),
                log::Level::Warn => writeln!(buf, "warning: {}", record.args()),
                _ => writeln!(buf, "[{}] {}: {}", level, record.target(), record.args()),
            }
        });
        b.target(env_logger::Target::Pipe(Box::new(BarSafeWriter)));
        let logger = b.build();
        let max = logger.filter();
        if log::set_boxed_logger(Box::new(CliLogger { inner: logger })).is_ok() {
            log::set_max_level(max);
        }
    });
}

/// Start capturing log lines for the full-screen view.
pub fn start_capture() {
    if let Ok(mut g) = CAPTURE.lock() {
        *g = Some(VecDeque::new());
    }
}

/// Stop capturing and return what was captured.
pub fn stop_capture() -> Vec<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut g| g.take())
        .map(|b| b.into_iter().collect())
        .unwrap_or_default()
}

/// Take the lines captured so far (capture continues).
pub fn drain_capture() -> Vec<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut g| g.as_mut().map(|b| b.drain(..).collect()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_levels() {
        assert_eq!(level_for(0), LevelFilter::Error);
        assert_eq!(level_for(1), LevelFilter::Info);
        assert_eq!(level_for(2), LevelFilter::Debug);
        assert_eq!(level_for(5), LevelFilter::Trace);
    }

    #[test]
    fn capture_round_trip() {
        start_capture();
        assert!(drain_capture().is_empty());
        let _ = stop_capture();
    }
}
