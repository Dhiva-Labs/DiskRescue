//! Work that must not run on the user interface thread: anything that may
//! touch a disk (listing devices, safety checks, free space, file dialogs,
//! loading a report) runs on its own thread and is polled every frame.

use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A value being computed on a background thread.
pub struct Task<T> {
    rx: mpsc::Receiver<T>,
}

impl<T: Send + 'static> Task<T> {
    /// Run `f` on a new thread; the user interface is woken up when it is done.
    pub fn spawn(ctx: &egui::Context, name: &str, f: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let v = f();
                let _ = tx.send(v);
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            log::error!("cannot start a background thread: {e}");
        }
        Task { rx }
    }

    /// The value, once (`None` while it is being computed, and after it was
    /// taken or the thread died).
    pub fn poll(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

/// A value derived from an input that the user edits (a path being typed):
/// recomputed in the background a moment after the input stops changing.
pub struct Watch<K, T> {
    input: Option<K>,
    changed_at: Instant,
    running: Option<(K, Task<T>)>,
    result: Option<(K, T)>,
    delay: Duration,
}

impl<K: PartialEq + Clone + Send + 'static, T: Send + 'static> Watch<K, T> {
    /// A watch that waits `delay` after the last change.
    pub fn new(delay: Duration) -> Self {
        Watch {
            input: None,
            changed_at: Instant::now(),
            running: None,
            result: None,
            delay,
        }
    }

    /// Call every frame with the current input; `f` computes the value when
    /// it is out of date (it is only called then).
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        key: &K,
        f: impl FnOnce(K) -> T + Send + 'static,
    ) {
        let first = self.input.is_none();
        if self.input.as_ref() != Some(key) {
            self.input = Some(key.clone());
            self.changed_at = Instant::now();
        }
        if let Some((k, task)) = &mut self.running {
            if let Some(v) = task.poll() {
                self.result = Some((k.clone(), v));
                self.running = None;
            }
        }
        let fresh = self.result.as_ref().is_some_and(|(k, _)| k == key);
        let busy_with_key = self.running.as_ref().is_some_and(|(k, _)| k == key);
        if fresh || busy_with_key || self.running.is_some() {
            if self.running.is_some() {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            return;
        }
        let waited = self.changed_at.elapsed();
        if first || self.result.is_none() || waited >= self.delay {
            let k = key.clone();
            let task = Task::spawn(ctx, "diskrescue-check", move || f(k));
            self.running = Some((key.clone(), task));
        } else {
            ctx.request_repaint_after(self.delay - waited);
        }
    }

    /// The value for `key`, when it has been computed.
    pub fn get(&self, key: &K) -> Option<&T> {
        self.result
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// The latest value, even if it belongs to an older input.
    pub fn latest(&self) -> Option<&T> {
        self.result.as_ref().map(|(_, v)| v)
    }

    /// True while a value is being computed or about to be.
    pub fn is_busy(&self, key: &K) -> bool {
        self.get(key).is_none()
    }

    /// Forget the value so that it is computed again.
    pub fn invalidate(&mut self) {
        self.result = None;
        self.input = None;
    }
}
