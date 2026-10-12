//! [`TimeoutSource`]: real per-read timeouts for any [`Source`].
//!
//! Core sources (files, memory, partition views) ignore the `timeout`
//! argument of [`Source::read_at_timeout`]. Wrapping one in a
//! `TimeoutSource` runs every read on an abandonable worker thread (see
//! [`crate::worker`]), so a read that blocks is reported as
//! [`ReadErrorKind::Timeout`](diskrescue_core::ReadErrorKind::Timeout)
//! after the deadline and later reads still work. [`crate::DeviceSource`]
//! uses the same mechanism internally; tests use this wrapper with slow
//! in-memory sources to exercise the imaging engine's hang handling.
//!
//! A zero timeout means "use the default timeout"; `Duration::MAX` waits
//! without a deadline.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::ranges::RangeSet;
use diskrescue_core::source::{ReadError, Source};

use crate::aligned::PAGE;
use crate::worker::{ReadBackend, WorkerPool};

struct SourceBackend<S: Source>(Arc<S>);

impl<S: Source + 'static> ReadBackend for SourceBackend<S> {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.0.read_at(offset, buf)
    }
}

/// Wraps a [`Source`] so that reads honour a timeout. See the module docs.
pub struct TimeoutSource<S: Source> {
    inner: Arc<S>,
    pool: WorkerPool,
    default_timeout: Duration,
}

impl<S: Source + 'static> TimeoutSource<S> {
    /// Wrap `inner`; plain [`Source::read_at`] calls use `default_timeout`.
    pub fn new(inner: S, default_timeout: Duration) -> Self {
        Self::from_arc(Arc::new(inner), default_timeout)
    }

    /// Wrap a shared source.
    pub fn from_arc(inner: Arc<S>, default_timeout: Duration) -> Self {
        let pool = WorkerPool::new(
            short_name(&inner.describe()),
            Arc::new(SourceBackend(inner.clone())),
            PAGE.max(inner.sector_size() as usize),
        );
        TimeoutSource {
            inner,
            pool,
            default_timeout: if default_timeout.is_zero() {
                Duration::from_secs(30)
            } else {
                default_timeout
            },
        }
    }

    /// The wrapped source.
    pub fn inner(&self) -> &Arc<S> {
        &self.inner
    }

    /// Timeout used by [`Source::read_at`].
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Reads abandoned because they exceeded their timeout (total).
    pub fn abandoned_workers(&self) -> u64 {
        self.pool.abandoned()
    }

    /// Abandoned reads that have not returned yet.
    pub fn hung_workers(&self) -> u64 {
        self.pool.hung()
    }
}

pub(crate) fn short_name(s: &str) -> String {
    let s = s.rsplit(['/', '\\']).next().unwrap_or(s);
    s.chars().take(12).collect()
}

impl<S: Source + 'static> Source for TimeoutSource<S> {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.read_at_timeout(offset, buf, self.default_timeout)
    }

    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        if buf.is_empty() || offset >= self.inner.size() {
            return Ok(0);
        }
        let timeout = if timeout.is_zero() {
            self.default_timeout
        } else {
            timeout
        };
        let len = buf.len();
        self.pool.read(offset, len, buf, timeout)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn is_device(&self) -> bool {
        self.inner.is_device()
    }

    fn requires_alignment(&self) -> bool {
        self.inner.requires_alignment()
    }

    fn reopen(&self) -> Result<(), ReadError> {
        self.inner.reopen()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        self.inner.unreadable()
    }

    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::{MemSource, ReadErrorKind, SourceExt};

    #[test]
    fn passes_through_and_clamps() {
        let s = TimeoutSource::new(MemSource::new((0..100u8).collect()), Duration::ZERO);
        assert_eq!(s.default_timeout(), Duration::from_secs(30));
        assert_eq!(
            s.read_exact_vec(90, 10).unwrap(),
            (90..100u8).collect::<Vec<_>>()
        );
        let mut b = [0u8; 20];
        assert_eq!(s.read_at(95, &mut b).unwrap(), 5);
        assert_eq!(s.read_at(100, &mut b).unwrap(), 0);
        assert_eq!(s.read_at(5, &mut []).unwrap(), 0);
        assert_eq!(s.size(), 100);
        assert_eq!(s.describe(), "memory");
        assert_eq!(s.reopen().unwrap_err().kind, ReadErrorKind::Unsupported);
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("/dev/sdb (SanDisk)"), "sdb (SanDisk");
        assert_eq!(short_name("memory"), "memory");
    }
}
