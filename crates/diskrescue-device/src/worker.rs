//! Per-read timeouts with abandonable worker threads.
//!
//! A `pread()` into a bad sector can block for minutes in uninterruptible
//! sleep: signals do not interrupt it, threads cannot be cancelled and
//! closing the fd from another thread does not abort it. The only handle
//! userspace has is time. So every read runs on a worker thread and the
//! caller waits on a channel with a deadline (imaging spec §3.5):
//!
//! * The caller takes an idle worker (or spawns one), hands it a job that
//!   *moves* a pooled aligned buffer to the worker, and waits on the
//!   worker's private reply channel with `recv_timeout`.
//! * On success the bytes are copied into the caller's buffer, the buffer
//!   goes back to the pool and the worker back to the idle list.
//! * On timeout the worker is **abandoned**: it keeps its buffer and its
//!   reference to the backend (so the fd stays valid while the kernel still
//!   owns the request), its channels are dropped and the next read gets a
//!   fresh worker. Whatever the abandoned read returns later is discarded.
//!   Nothing is ever written to the caller's buffer after the call returned.
//!
//! Counters: [`WorkerPool::abandoned`] (ever) and [`WorkerPool::hung`]
//! (abandoned workers whose read has not returned yet). When too many reads
//! are hung the pool refuses to queue more and reports a timeout at once:
//! stacking requests on a drive that stopped answering only makes it worse.
//!
//! Several callers may read concurrently; each gets its own worker, so a
//! hung read never blocks an unrelated one beyond its own deadline.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diskrescue_core::source::{ReadError, ReadErrorKind};

use crate::aligned::{AlignedBuf, BufferPool};

/// Something a worker can read from. Implementations perform one blocking
/// positional read into `buf` and return the number of bytes read.
pub trait ReadBackend: Send + Sync + 'static {
    /// Blocking read of up to `buf.len()` bytes at `offset`.
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError>;
}

/// Default limit of simultaneously hung reads before the pool refuses
/// to queue more.
pub const DEFAULT_MAX_HUNG: u64 = 32;

/// Idle workers kept for reuse.
const MAX_IDLE: usize = 4;

struct Job {
    offset: u64,
    len: usize,
    buf: AlignedBuf,
}

struct Reply {
    result: Result<usize, ReadError>,
    buf: AlignedBuf,
}

struct Worker {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Reply>,
    abandoned: Arc<AtomicBool>,
    generation: u64,
}

struct Backend {
    reader: Arc<dyn ReadBackend>,
    generation: u64,
}

/// A pool of read workers bound to one backend. See the module docs.
pub struct WorkerPool {
    name: String,
    backend: Mutex<Option<Backend>>,
    generation: AtomicU64,
    idle: Mutex<Vec<Worker>>,
    buffers: BufferPool,
    abandoned_total: AtomicU64,
    hung: Arc<AtomicU64>,
    max_hung: u64,
}

impl WorkerPool {
    /// Pool reading from `backend`, with buffers aligned to `align`.
    /// `name` is used for thread names and messages.
    pub fn new(name: impl Into<String>, backend: Arc<dyn ReadBackend>, align: usize) -> Self {
        WorkerPool {
            name: name.into(),
            backend: Mutex::new(Some(Backend {
                reader: backend,
                generation: 0,
            })),
            generation: AtomicU64::new(0),
            idle: Mutex::new(Vec::new()),
            buffers: BufferPool::new(align),
            abandoned_total: AtomicU64::new(0),
            hung: Arc::new(AtomicU64::new(0)),
            max_hung: DEFAULT_MAX_HUNG,
        }
    }

    /// Change the hung-read limit.
    pub fn with_max_hung(mut self, max: u64) -> Self {
        self.max_hung = max.max(1);
        self
    }

    /// Workers abandoned because a read exceeded its timeout (total).
    pub fn abandoned(&self) -> u64 {
        self.abandoned_total.load(Ordering::SeqCst)
    }

    /// Abandoned workers whose read is still blocked in the OS.
    pub fn hung(&self) -> u64 {
        self.hung.load(Ordering::SeqCst)
    }

    /// Alignment of the internal buffers.
    pub fn align(&self) -> usize {
        self.buffers.align()
    }

    /// Drop the backend: idle workers exit, later reads fail with
    /// [`ReadErrorKind::DeviceGone`] until [`WorkerPool::attach`]. Workers
    /// that are still blocked keep their own reference until they return,
    /// so the underlying handle is closed only when nobody uses it.
    pub fn detach(&self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut b) = self.backend.lock() {
            *b = None;
        }
        self.drop_idle(generation);
    }

    /// Install a new backend (after re-opening a device).
    pub fn attach(&self, backend: Arc<dyn ReadBackend>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut b) = self.backend.lock() {
            *b = Some(Backend {
                reader: backend,
                generation,
            });
        }
        self.drop_idle(generation);
    }

    /// Changes every time the backend is detached or replaced. A read that
    /// started under an older generation used the previous handle.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// True when a backend is attached.
    pub fn is_attached(&self) -> bool {
        self.backend.lock().map(|b| b.is_some()).unwrap_or(false)
    }

    fn drop_idle(&self, current: u64) {
        if let Ok(mut idle) = self.idle.lock() {
            idle.retain(|w| w.generation == current);
        }
    }

    fn current_backend(&self) -> Option<(Arc<dyn ReadBackend>, u64)> {
        let b = self.backend.lock().ok()?;
        b.as_ref().map(|b| (b.reader.clone(), b.generation))
    }

    fn take_idle(&self, generation: u64) -> Option<Worker> {
        let mut idle = self.idle.lock().ok()?;
        while let Some(w) = idle.pop() {
            if w.generation == generation {
                return Some(w);
            }
        }
        None
    }

    fn put_idle(&self, w: Worker) {
        if w.generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut idle) = self.idle.lock() {
            if idle.len() < MAX_IDLE {
                idle.push(w);
            }
        }
    }

    fn spawn(&self, reader: Arc<dyn ReadBackend>, generation: u64) -> std::io::Result<Worker> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (reply_tx, reply_rx) = mpsc::channel::<Reply>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let flag = abandoned.clone();
        let hung = self.hung.clone();
        std::thread::Builder::new()
            .name(format!("dr-read-{}", self.name))
            .spawn(move || {
                while let Ok(mut job) = job_rx.recv() {
                    let len = job.len.min(job.buf.capacity());
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        reader.read(job.offset, job.buf.slice_mut(len))
                    }))
                    .unwrap_or_else(|_| {
                        Err(ReadError::new(
                            job.offset,
                            len,
                            ReadErrorKind::Io,
                            "the read worker panicked",
                        ))
                    });
                    if reply_tx
                        .send(Reply {
                            result,
                            buf: job.buf,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // The job channel closed (pool trimmed or worker abandoned)
                // or the caller stopped listening. If we were abandoned, we
                // are no longer hung.
                if flag.swap(false, Ordering::SeqCst) {
                    hung.fetch_sub(1, Ordering::SeqCst);
                }
            })?;
        Ok(Worker {
            tx: job_tx,
            rx: reply_rx,
            abandoned,
            generation,
        })
    }

    /// Read up to `out.len()` bytes at `offset` on a worker, waiting at most
    /// `timeout`. `job_len` is the length the backend reads (it may exceed
    /// `out.len()`, e.g. rounded up to the sector size for direct I/O);
    /// only `min(n, out.len())` bytes are copied back, and only on success.
    /// `Duration::MAX` waits without a deadline.
    pub fn read(
        &self,
        offset: u64,
        job_len: usize,
        out: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        let Some((reader, generation)) = self.current_backend() else {
            return Err(ReadError::new(
                offset,
                out.len(),
                ReadErrorKind::DeviceGone,
                format!(
                    "{} is not attached (device gone; waiting for reopen)",
                    self.name
                ),
            ));
        };
        let hung = self.hung();
        if hung >= self.max_hung {
            return Err(ReadError::new(
                offset,
                out.len(),
                ReadErrorKind::Timeout,
                format!(
                    "{hung} earlier reads on {} are still hung in the kernel; not queueing more",
                    self.name
                ),
            ));
        }
        let mut job = Job {
            offset,
            len: job_len,
            buf: self.buffers.take(job_len),
        };
        let mut worker = match self.take_idle(generation) {
            Some(w) => w,
            None => self.spawn(reader.clone(), generation).map_err(|e| {
                ReadError::new(
                    offset,
                    out.len(),
                    ReadErrorKind::Io,
                    format!("cannot start a read worker thread: {e}"),
                )
            })?,
        };
        // An idle worker may have died (it never should); retry once with a
        // fresh one.
        if let Err(mpsc::SendError(j)) = worker.tx.send(job) {
            job = j;
            worker = self.spawn(reader, generation).map_err(|e| {
                ReadError::new(
                    offset,
                    out.len(),
                    ReadErrorKind::Io,
                    format!("cannot start a read worker thread: {e}"),
                )
            })?;
            if worker.tx.send(job).is_err() {
                return Err(ReadError::new(
                    offset,
                    out.len(),
                    ReadErrorKind::Io,
                    "read worker exited unexpectedly",
                ));
            }
        }
        let started = Instant::now();
        let reply = if timeout == Duration::MAX {
            worker.rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            worker.rx.recv_timeout(timeout)
        };
        match reply {
            Ok(Reply { result, buf }) => {
                let r = match result {
                    Ok(n) => {
                        let n = n.min(out.len()).min(buf.capacity());
                        out[..n].copy_from_slice(buf.slice(n));
                        Ok(n)
                    }
                    Err(e) => Err(e),
                };
                self.buffers.put(buf);
                self.put_idle(worker);
                r
            }
            Err(RecvTimeoutError::Timeout) => {
                // Order matters: mark, count, then drop the channels, so the
                // worker can only observe the flag after the count went up.
                worker.abandoned.store(true, Ordering::SeqCst);
                let now_hung = self.hung.fetch_add(1, Ordering::SeqCst) + 1;
                self.abandoned_total.fetch_add(1, Ordering::SeqCst);
                drop(worker);
                log::warn!(
                    "{}: read of {} bytes at offset {} did not finish within {:?}; abandoned it ({} read(s) still hung)",
                    self.name,
                    job_len,
                    offset,
                    started.elapsed(),
                    now_hung
                );
                Err(ReadError::new(
                    offset,
                    out.len(),
                    ReadErrorKind::Timeout,
                    format!(
                        "no answer within {:.1}s; the read was abandoned ({now_hung} read(s) still hung)",
                        timeout.as_secs_f64()
                    ),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(ReadError::new(
                offset,
                out.len(),
                ReadErrorKind::Io,
                "read worker exited without answering",
            )),
        }
    }
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("name", &self.name)
            .field("abandoned", &self.abandoned())
            .field("hung", &self.hung())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Slow {
        delay_at_zero: Duration,
    }

    impl ReadBackend for Slow {
        fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
            if offset == 0 {
                std::thread::sleep(self.delay_at_zero);
            }
            if offset == 999 {
                panic!("boom");
            }
            for (i, b) in buf.iter_mut().enumerate() {
                *b = (offset as usize + i) as u8;
            }
            Ok(buf.len())
        }
    }

    #[test]
    fn timeout_abandon_and_recover() {
        let pool = WorkerPool::new(
            "t",
            Arc::new(Slow {
                delay_at_zero: Duration::from_millis(400),
            }),
            64,
        );
        let mut out = [0u8; 16];
        let t = Instant::now();
        let e = pool
            .read(0, 16, &mut out, Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Timeout);
        assert!(t.elapsed() < Duration::from_millis(300));
        assert_eq!(out, [0u8; 16], "caller buffer untouched on timeout");
        assert_eq!(pool.abandoned(), 1);
        assert_eq!(pool.hung(), 1);
        assert_eq!(
            pool.read(16, 4, &mut out, Duration::from_secs(5)).unwrap(),
            4
        );
        assert_eq!(&out[..4], &[16, 17, 18, 19]);
        // The hung worker eventually returns and is no longer counted.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.hung() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pool.hung(), 0);
    }

    #[test]
    fn panic_becomes_io_error_and_detach_reports_gone() {
        let pool = WorkerPool::new(
            "p",
            Arc::new(Slow {
                delay_at_zero: Duration::ZERO,
            }),
            64,
        );
        let mut out = [0u8; 8];
        let e = pool
            .read(999, 8, &mut out, Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Io);
        assert_eq!(pool.read(8, 8, &mut out, Duration::MAX).unwrap(), 8);
        pool.detach();
        assert!(!pool.is_attached());
        let e = pool.read(8, 8, &mut out, Duration::MAX).unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::DeviceGone);
        pool.attach(Arc::new(Slow {
            delay_at_zero: Duration::ZERO,
        }));
        assert_eq!(pool.read(8, 8, &mut out, Duration::MAX).unwrap(), 8);
    }

    #[test]
    fn hung_limit() {
        let pool = WorkerPool::new(
            "h",
            Arc::new(Slow {
                delay_at_zero: Duration::from_millis(500),
            }),
            64,
        )
        .with_max_hung(1);
        let mut out = [0u8; 8];
        assert!(pool
            .read(0, 8, &mut out, Duration::from_millis(20))
            .is_err());
        let t = Instant::now();
        let e = pool
            .read(8, 8, &mut out, Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Timeout);
        assert!(e.detail.contains("still hung"));
        assert!(t.elapsed() < Duration::from_millis(100));
    }
}
