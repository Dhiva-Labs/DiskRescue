//! Background jobs: every operation runs on its own thread and exposes its
//! state for polling.
//!
//! A `start_*` function returns at once with a [`JobHandle`]. The job runs
//! its preflight check first and ends with [`JobOutcome::Refused`] when
//! something blocks it; otherwise it works and updates its state, which a
//! user interface reads with [`JobHandle::snapshot`] as often as it likes
//! (snapshots are cheap: the large parts are shared, not copied).
//!
//! Rules every job follows:
//!
//! * Nothing is written to the source; the checks of [`crate::preflight`]
//!   run again right before the work starts.
//! * [`JobHandle::cancel`] stops cleanly: imaging saves its progress map,
//!   recovery writes its report for what was found so far.
//! * Files created while running as root under sudo or pkexec are given
//!   back to the user when the job ends ([`crate::ownership`]).
//! * Dropping a handle of a running job cancels it.
//! * A panic inside a job ends it with [`JobOutcome::Failed`]; it never
//!   takes the user interface down.

mod files;
mod image;
mod pipeline;
mod recover;
mod scan;

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use diskrescue_imaging::Control;

use crate::error::AppError;

pub use files::{start_repair, start_verify, RepairState, VerifyState};
pub use image::{start_image, ImageState, ImageStatus, LogLine};
pub use pipeline::{CarveProgress, FinishedFile, OutcomeCounts, Pipeline, StageProgress};
pub use recover::{start_carve, start_recover, RecoverState, RecoverStatus};
pub use scan::{start_scan, ScanState};

/// How many finished files and log lines a state keeps.
pub const RECENT: usize = 200;

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// It ran to the end.
    Finished,
    /// It was stopped with [`JobHandle::cancel`]; what was done is kept
    /// (and imaging can be resumed).
    Cancelled,
    /// It did not start because it would have been unsafe or impossible;
    /// nothing was written. The text says why.
    Refused(String),
    /// It started but could not finish. The text says why.
    Failed(String),
}

impl JobOutcome {
    /// The outcome for an error.
    pub fn from_error(e: AppError) -> Self {
        match e {
            AppError::Refused(m) => JobOutcome::Refused(m),
            AppError::Invalid(m) | AppError::Failed(m) => JobOutcome::Failed(m),
        }
    }

    /// True for [`JobOutcome::Finished`].
    pub fn is_finished(&self) -> bool {
        matches!(self, JobOutcome::Finished)
    }
}

/// State types of jobs. A pause or stop request is mirrored into the state
/// so that the next snapshot shows it.
pub trait JobState: Clone + Send + 'static {
    /// The user paused (`true`) or resumed the job.
    fn set_paused(&mut self, paused: bool) {
        let _ = paused;
    }
    /// The user asked the job to stop.
    fn set_stopping(&mut self) {}
}

/// Lock a mutex, ignoring poisoning (a panicking job must not make the
/// state unreadable).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// Run control shared by a handle and its job.
pub(crate) struct Shared {
    pausable: bool,
    cancel: Arc<AtomicBool>,
    paused: AtomicBool,
    control: Mutex<Option<Arc<Control>>>,
    on_cancel: Mutex<Option<Hook>>,
    finished: AtomicBool,
    outcome: Mutex<Option<JobOutcome>>,
}

impl Shared {
    fn new(pausable: bool) -> Self {
        Shared {
            pausable,
            cancel: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            control: Mutex::new(None),
            on_cancel: Mutex::new(None),
            finished: AtomicBool::new(false),
            outcome: Mutex::new(None),
        }
    }

    /// The stop flag (for the recovery pipeline and device re-attach waits).
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// True once a stop was requested.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Register the imaging engine's control: stop and pause requests reach
    /// it from now on (a stop that came earlier cancels it at once).
    pub(crate) fn attach_control(&self, c: Arc<Control>) {
        c.pause(self.paused.load(Ordering::SeqCst));
        *lock(&self.control) = Some(c.clone());
        if self.is_cancelled() {
            c.cancel();
        }
    }

    /// Forget the engine's control (its run is over).
    pub(crate) fn detach_control(&self) {
        *lock(&self.control) = None;
    }

    /// Run `hook` when the job is cancelled (at once if it already was).
    /// The hook must be idempotent: it may run more than once.
    pub(crate) fn on_cancel(&self, hook: Hook) {
        *lock(&self.on_cancel) = Some(hook);
        if self.is_cancelled() {
            if let Some(h) = lock(&self.on_cancel).as_ref() {
                h();
            }
        }
    }

    pub(crate) fn clear_on_cancel(&self) {
        *lock(&self.on_cancel) = None;
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(c) = lock(&self.control).as_ref() {
            c.cancel();
        }
        if let Some(h) = lock(&self.on_cancel).as_ref() {
            h();
        }
    }

    fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
        if let Some(c) = lock(&self.control).as_ref() {
            c.pause(paused);
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

/// What a job's work function gets: its state and its run control.
pub(crate) struct Ctx<S> {
    pub(crate) state: Arc<Mutex<S>>,
    pub(crate) shared: Arc<Shared>,
}

impl<S> Ctx<S> {
    /// Change the state.
    pub(crate) fn update(&self, f: impl FnOnce(&mut S)) {
        f(&mut lock(&self.state));
    }
}

/// A running (or finished) background job with state `S`.
///
/// `JobHandle` is `Send + Sync`; every method takes `&self` and returns
/// quickly except [`JobHandle::wait`].
pub struct JobHandle<S: JobState> {
    state: Arc<Mutex<S>>,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl<S: JobState> std::fmt::Debug for JobHandle<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("finished", &self.is_finished())
            .field("cancelled", &self.shared.is_cancelled())
            .field("outcome", &self.outcome())
            .finish_non_exhaustive()
    }
}

impl<S: JobState> JobHandle<S> {
    /// A copy of the current state. Cheap: large parts are shared.
    pub fn snapshot(&self) -> S {
        lock(&self.state).clone()
    }

    /// Ask the job to stop. Imaging saves its progress map and can be
    /// resumed later; recovery keeps what it found and writes its report.
    /// Calling it again has no further effect.
    pub fn cancel(&self) {
        if !self.shared.is_cancelled() {
            lock(&self.state).set_stopping();
        }
        self.shared.cancel();
    }

    /// Pause (`true`) or resume (`false`). Only imaging run by this
    /// process pauses (between reads, after saving its progress map; for a
    /// recovery, while it images); other jobs ignore it (see
    /// [`JobHandle::can_pause`]).
    pub fn pause(&self, paused: bool) {
        if !self.shared.pausable {
            return;
        }
        self.shared.pause(paused);
        lock(&self.state).set_paused(paused);
    }

    /// True for jobs that honour [`JobHandle::pause`].
    pub fn can_pause(&self) -> bool {
        self.shared.pausable
    }

    /// True while paused.
    pub fn is_paused(&self) -> bool {
        self.shared.is_paused()
    }

    /// True once [`JobHandle::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.shared.is_cancelled()
    }

    /// True once the job has ended (its outcome is known).
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::SeqCst)
    }

    /// The outcome, once the job has ended.
    pub fn outcome(&self) -> Option<JobOutcome> {
        lock(&self.shared.outcome).clone()
    }

    /// Block until the job ends and return its outcome.
    pub fn wait(&self) -> JobOutcome {
        let handle = lock(&self.thread).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        while !self.is_finished() {
            // Another caller is joining the thread; the outcome follows.
            std::thread::sleep(Duration::from_millis(10));
        }
        self.outcome().unwrap_or_else(|| {
            JobOutcome::Failed("the job ended without a result (this is a bug)".into())
        })
    }

    /// Wait at most `timeout` for the job to end.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<JobOutcome> {
        let deadline = std::time::Instant::now() + timeout;
        while !self.is_finished() {
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(self.wait())
    }
}

impl<S: JobState> Drop for JobHandle<S> {
    fn drop(&mut self) {
        if !self.is_finished() {
            self.shared.cancel();
        }
    }
}

/// Start `work` on a new thread named `name`. `written` gives the paths the
/// job writes, computed when it ends, for the ownership hand-back.
/// `pausable` says whether [`JobHandle::pause`] means anything for it.
pub(crate) fn spawn<S, W, P>(
    name: &str,
    pausable: bool,
    initial: S,
    written: P,
    work: W,
) -> JobHandle<S>
where
    S: JobState,
    W: FnOnce(&Ctx<S>) -> JobOutcome + Send + 'static,
    P: FnOnce() -> Vec<PathBuf> + Send + 'static,
{
    let state = Arc::new(Mutex::new(initial));
    let shared = Arc::new(Shared::new(pausable));
    let ctx = Ctx {
        state: state.clone(),
        shared: shared.clone(),
    };
    let started = SystemTime::now() - Duration::from_secs(2);
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let outcome = match std::panic::catch_unwind(AssertUnwindSafe(|| work(&ctx))) {
                Ok(o) => o,
                Err(_) => JobOutcome::Failed(
                    "the operation stopped unexpectedly (this is a bug). Anything already saved is kept; \
                     nothing was written to the source."
                        .into(),
                ),
            };
            let written = std::panic::catch_unwind(AssertUnwindSafe(written)).unwrap_or_default();
            crate::ownership::hand_back(&written, started);
            *lock(&ctx.shared.outcome) = Some(outcome);
            ctx.shared.clear_on_cancel();
            ctx.shared.finished.store(true, Ordering::SeqCst);
        });
    let thread = match spawned {
        Ok(h) => Some(h),
        Err(e) => {
            *lock(&shared.outcome) = Some(JobOutcome::Failed(format!(
                "cannot start a background thread: {e}"
            )));
            shared.finished.store(true, Ordering::SeqCst);
            None
        }
    };
    JobHandle {
        state,
        shared,
        thread: Mutex::new(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Counter {
        n: u32,
        paused: bool,
        stopping: bool,
    }

    impl JobState for Counter {
        fn set_paused(&mut self, paused: bool) {
            self.paused = paused;
        }
        fn set_stopping(&mut self) {
            self.stopping = true;
        }
    }

    #[test]
    fn handles_report_state_cancel_and_panics() {
        let h = spawn(
            "t",
            true,
            Counter::default(),
            Vec::new,
            |ctx: &Ctx<Counter>| {
                while !ctx.shared.is_cancelled() {
                    ctx.update(|s| s.n += 1);
                    std::thread::sleep(Duration::from_millis(1));
                }
                JobOutcome::Cancelled
            },
        );
        while h.snapshot().n < 3 {
            std::thread::sleep(Duration::from_millis(1));
        }
        h.pause(true);
        assert!(h.snapshot().paused && h.is_paused());
        assert!(!h.is_finished());
        h.cancel();
        h.cancel();
        assert_eq!(h.wait(), JobOutcome::Cancelled);
        assert!(h.snapshot().stopping);
        assert!(h.is_finished());

        let h = spawn(
            "p",
            false,
            Counter::default(),
            Vec::new,
            |_ctx: &Ctx<Counter>| panic!("boom"),
        );
        assert!(matches!(h.wait(), JobOutcome::Failed(m) if m.contains("bug")));
    }

    #[test]
    fn dropping_a_running_job_cancels_it() {
        let seen = Arc::new(AtomicBool::new(false));
        let s2 = seen.clone();
        let h = spawn(
            "d",
            false,
            Counter::default(),
            Vec::new,
            move |ctx: &Ctx<Counter>| {
                while !ctx.shared.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                s2.store(true, Ordering::SeqCst);
                JobOutcome::Cancelled
            },
        );
        let shared = h.shared.clone();
        drop(h);
        let t = std::time::Instant::now();
        while !shared.finished.load(Ordering::SeqCst) {
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(seen.load(Ordering::SeqCst));
    }
}
