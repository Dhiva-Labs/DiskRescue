//! Ctrl-C handling.
//!
//! The first Ctrl-C (or SIGTERM / SIGHUP, or `q` in the full-screen view)
//! asks the running command to stop cleanly: the imaging engine saves its
//! mapfile, the recovery pipeline writes its report. A second Ctrl-C within
//! three seconds quits immediately with exit code 130.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use diskrescue_imaging::Control;

/// How long after the first Ctrl-C a second one forces an immediate exit.
pub const FORCE_WINDOW: Duration = Duration::from_secs(3);

/// Shared interrupt state.
pub struct Interrupt {
    flag: Arc<AtomicBool>,
    controls: Mutex<Vec<Arc<Control>>>,
    first: Mutex<Option<Instant>>,
}

static GLOBAL: OnceLock<Arc<Interrupt>> = OnceLock::new();

/// What a signal or key press should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// First request: stop cleanly.
    Stop,
    /// Repeated request inside [`FORCE_WINDOW`]: leave now.
    Force,
}

impl Interrupt {
    fn new() -> Self {
        Interrupt {
            flag: Arc::new(AtomicBool::new(false)),
            controls: Mutex::new(Vec::new()),
            first: Mutex::new(None),
        }
    }

    /// The cancel flag for APIs that take an `Arc<AtomicBool>` (recovery,
    /// device re-attach waits).
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// True once a stop was requested.
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Register an imaging control so that a stop request cancels it.
    /// If a stop was already requested the control is cancelled at once.
    pub fn attach(&self, control: Arc<Control>) {
        if self.is_set() {
            control.cancel();
        }
        if let Ok(mut v) = self.controls.lock() {
            v.push(control);
        }
    }

    /// Forget registered controls (when an imaging run is over).
    pub fn detach_all(&self) {
        if let Ok(mut v) = self.controls.lock() {
            v.clear();
        }
    }

    /// Ask everything that is running to stop cleanly.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(v) = self.controls.lock() {
            for c in v.iter() {
                c.cancel();
            }
        }
    }

    /// Record a user request to stop and decide what to do with it.
    pub fn request(&self) -> Step {
        let now = Instant::now();
        let mut first = match self.first.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match *first {
            Some(t) if now.duration_since(t) < FORCE_WINDOW => Step::Force,
            _ => {
                *first = Some(now);
                drop(first);
                self.trigger();
                Step::Stop
            }
        }
    }
}

/// The process-wide interrupt state (created on first use).
pub fn global() -> Arc<Interrupt> {
    GLOBAL.get_or_init(|| Arc::new(Interrupt::new())).clone()
}

/// Install the Ctrl-C / termination handler. Safe to call more than once.
pub fn install() -> Arc<Interrupt> {
    let state = global();
    let handler_state = state.clone();
    let res = ctrlc::set_handler(move || {
        let tui = crate::tui::is_active();
        match handler_state.request() {
            Step::Stop => {
                if !tui {
                    eprintln!(
                        "\nStopping: finishing the current step and saving what has been done so far. \
                         (Press Ctrl-C again within 3 seconds to quit immediately.)"
                    );
                }
            }
            Step::Force => force_exit(),
        }
    });
    if let Err(e) = res {
        // Already installed (second call) or unsupported: not fatal.
        log::debug!("could not install the Ctrl-C handler: {e}");
    }
    state
}

/// Leave immediately with the "interrupted" exit code, restoring the terminal first.
pub fn force_exit() -> ! {
    crate::tui::restore();
    eprintln!("\nQuitting immediately. Anything already saved is kept; run the same command again to continue.");
    std::process::exit(crate::error::exit::INTERRUPTED);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_request_stops_second_forces() {
        let i = Interrupt::new();
        let c = Control::new();
        i.attach(c.clone());
        assert!(!i.is_set());
        assert_eq!(i.request(), Step::Stop);
        assert!(i.is_set());
        assert!(c.is_cancelled());
        assert_eq!(i.request(), Step::Force);
    }

    #[test]
    fn attach_after_stop_cancels_immediately() {
        let i = Interrupt::new();
        i.trigger();
        let c = Control::new();
        i.attach(c.clone());
        assert!(c.is_cancelled());
        i.detach_all();
    }
}
