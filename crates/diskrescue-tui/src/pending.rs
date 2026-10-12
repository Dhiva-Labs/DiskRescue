//! Work done off the screen's thread: the result arrives later and is
//! picked up by polling, so drawing and keys never wait for the disk.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

/// A result that is being computed on another thread.
pub struct Pending<T> {
    rx: Receiver<Result<T, String>>,
    done: bool,
}

impl<T: Send + 'static> Pending<T> {
    /// Run `f` on a new thread. A panic in `f` becomes an error message.
    pub fn spawn(f: impl FnOnce() -> T + Send + 'static) -> Pending<T> {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("diskrescue-tui-work".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(f))
                    .map_err(|_| "the check stopped unexpectedly (this is a bug)".to_string());
                let _ = tx.send(r);
            });
        if let Err(e) = spawned {
            let (tx2, rx2) = channel();
            let _ = tx2.send(Err(format!("cannot start a background thread: {e}")));
            return Pending {
                rx: rx2,
                done: false,
            };
        }
        Pending { rx, done: false }
    }

    /// The result, once it has arrived (only once).
    pub fn poll(&mut self) -> Option<Result<T, String>> {
        if self.done {
            return None;
        }
        match self.rx.try_recv() {
            Ok(r) => {
                self.done = true;
                Some(r)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                Some(Err(
                    "the check ended without a result (this is a bug)".into()
                ))
            }
        }
    }
}

impl<T> std::fmt::Debug for Pending<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending").field("done", &self.done).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait<T: Send + 'static>(p: &mut Pending<T>) -> Result<T, String> {
        let t = Instant::now();
        loop {
            if let Some(r) = p.poll() {
                return r;
            }
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn results_arrive_once_and_panics_are_contained() {
        let mut p = Pending::spawn(|| 7);
        assert_eq!(wait(&mut p), Ok(7));
        assert!(p.poll().is_none());
        let mut p: Pending<u8> = Pending::spawn(|| panic!("boom"));
        assert!(wait(&mut p).unwrap_err().contains("bug"));
    }
}
