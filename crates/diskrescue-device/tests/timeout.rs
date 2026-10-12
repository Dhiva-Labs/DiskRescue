//! TimeoutSource: hung reads are abandoned, later reads keep working.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::{MemSource, ReadError, ReadErrorKind, Source, SourceExt};
use diskrescue_device::TimeoutSource;

/// A memory source whose reads at offset 0 take `delay`.
struct SlowSource {
    inner: MemSource,
    delay: Duration,
    calls: AtomicU64,
}

impl Source for SlowSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if offset == 0 {
            thread::sleep(self.delay);
        }
        self.inner.read_at(offset, buf)
    }

    fn describe(&self) -> String {
        "slow memory".into()
    }

    fn is_device(&self) -> bool {
        true
    }
}

fn data() -> Vec<u8> {
    (0..64 * 1024u32).map(|i| (i % 251) as u8).collect()
}

fn slow(delay: Duration) -> SlowSource {
    SlowSource {
        inner: MemSource::new(data()),
        delay,
        calls: AtomicU64::new(0),
    }
}

#[test]
fn hung_read_times_out_quickly_and_later_reads_work() {
    let src = TimeoutSource::new(slow(Duration::from_secs(2)), Duration::from_millis(100));
    let mut buf = vec![0xAAu8; 4096];
    let t = Instant::now();
    let err = src.read_at(0, &mut buf).unwrap_err();
    let took = t.elapsed();
    assert_eq!(err.kind, ReadErrorKind::Timeout);
    assert_eq!(err.offset, 0);
    assert!(took < Duration::from_millis(800), "took {took:?}");
    assert!(buf.iter().all(|&b| b == 0xAA), "nothing copied on timeout");
    assert_eq!(src.abandoned_workers(), 1);
    assert_eq!(src.hung_workers(), 1);

    // The next read gets a fresh worker and works at once.
    let t = Instant::now();
    let n = src.read_at(4096, &mut buf).unwrap();
    assert_eq!(n, 4096);
    assert_eq!(&buf[..], &data()[4096..8192]);
    assert!(t.elapsed() < Duration::from_millis(500));

    // An explicit per-call timeout overrides the default.
    let t = Instant::now();
    let err = src
        .read_at_timeout(0, &mut buf, Duration::from_millis(30))
        .unwrap_err();
    assert_eq!(err.kind, ReadErrorKind::Timeout);
    assert!(t.elapsed() < Duration::from_millis(600));
    assert_eq!(src.abandoned_workers(), 2);

    // A generous timeout lets the slow read complete.
    let n = src
        .read_at_timeout(0, &mut buf, Duration::from_secs(10))
        .unwrap();
    assert_eq!(n, 4096);
    assert_eq!(&buf[..], &data()[..4096]);

    // The abandoned workers return eventually and stop counting as hung.
    let deadline = Instant::now() + Duration::from_secs(6);
    while src.hung_workers() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(src.hung_workers(), 0);
    assert_eq!(src.abandoned_workers(), 2);
}

#[test]
fn concurrent_reads_are_not_blocked_by_a_hung_one() {
    let src = Arc::new(TimeoutSource::new(
        slow(Duration::from_millis(1500)),
        Duration::from_secs(5),
    ));
    let hung = {
        let s = src.clone();
        thread::spawn(move || {
            let mut b = vec![0u8; 512];
            s.read_at_timeout(0, &mut b, Duration::from_millis(200))
        })
    };
    thread::sleep(Duration::from_millis(20));
    let t = Instant::now();
    let mut handles = Vec::new();
    for i in 1..8u64 {
        let s = src.clone();
        handles.push(thread::spawn(move || {
            let mut b = vec![0u8; 1000];
            let n = s.read_at(i * 1000, &mut b).unwrap();
            (n, b)
        }));
    }
    for (i, h) in handles.into_iter().enumerate() {
        let (n, b) = h.join().unwrap();
        let off = (i + 1) * 1000;
        assert_eq!(n, 1000);
        assert_eq!(&b[..], &data()[off..off + 1000]);
    }
    assert!(t.elapsed() < Duration::from_millis(1000));
    assert_eq!(
        hung.join().unwrap().unwrap_err().kind,
        ReadErrorKind::Timeout
    );
}

/// A source that never answers in time, fails with a given kind, or lies
/// about how much it read.
struct Hostile {
    mode: u8,
}

impl Source for Hostile {
    fn size(&self) -> u64 {
        1 << 20
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        match self.mode {
            0 => {
                thread::sleep(Duration::from_secs(3));
                Ok(0)
            }
            1 => Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::DeviceGone,
                "gone",
            )),
            2 => Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::Timeout,
                "slow",
            )),
            // Claims more bytes than the buffer holds.
            _ => {
                buf.fill(7);
                Ok(buf.len() * 4 + 1)
            }
        }
    }

    fn describe(&self) -> String {
        "hostile".into()
    }
}

#[test]
fn hung_reads_are_bounded_and_errors_pass_through() {
    // Every read hangs: at most the hung limit of threads pile up, then
    // reads fail at once without spawning more.
    let src = TimeoutSource::new(Hostile { mode: 0 }, Duration::from_millis(5));
    let mut buf = vec![0u8; 512];
    for i in 0..60u64 {
        let e = src.read_at(i * 512, &mut buf).unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Timeout);
    }
    assert!(src.hung_workers() <= 32, "{}", src.hung_workers());
    assert!(src.abandoned_workers() <= 32, "{}", src.abandoned_workers());
    let t = Instant::now();
    let e = src.read_at(0, &mut buf).unwrap_err();
    assert!(e.detail.contains("still hung"), "{}", e.detail);
    assert!(t.elapsed() < Duration::from_millis(50));

    for (mode, kind) in [(1, ReadErrorKind::DeviceGone), (2, ReadErrorKind::Timeout)] {
        let src = TimeoutSource::new(Hostile { mode }, Duration::from_secs(5));
        let e = src.read_at(4096, &mut buf).unwrap_err();
        assert_eq!((e.kind, e.offset), (kind, 4096));
        assert_eq!(src.abandoned_workers(), 0);
    }
    // A lying source cannot make the wrapper report or copy more than the
    // caller's buffer.
    let src = TimeoutSource::new(Hostile { mode: 3 }, Duration::from_secs(5));
    let mut small = vec![0u8; 100];
    assert_eq!(src.read_at(0, &mut small).unwrap(), 100);
    assert!(small.iter().all(|&b| b == 7));
    // An unbounded wait is allowed.
    assert_eq!(
        src.read_at_timeout(0, &mut small, Duration::MAX).unwrap(),
        100
    );
}

#[test]
fn errors_and_metadata_pass_through() {
    let mut bad = RangeSet::new();
    bad.insert(Range::new(1000, 2000));
    let src = TimeoutSource::new(
        MemSource::named(data(), "mem").with_unreadable(bad),
        Duration::from_secs(5),
    );
    let mut buf = vec![0u8; 4096];
    // Good prefix as a short read, then the error at the bad offset.
    assert_eq!(src.read_at(0, &mut buf).unwrap(), 1000);
    let e = src.read_at(1000, &mut buf).unwrap_err();
    assert_eq!((e.kind, e.offset), (ReadErrorKind::Io, 1000));
    assert_eq!(src.size(), 64 * 1024);
    assert_eq!(src.describe(), "mem");
    assert!(src.overlaps_unreadable(1500, 1));
    assert_eq!(src.read_vec(65_000, 10_000).unwrap().len(), 536);
    // Zero timeout means "use the default".
    assert_eq!(
        src.read_at_timeout(4096, &mut buf, Duration::ZERO).unwrap(),
        4096
    );
    // Wrapping a shared dyn source works too.
    let shared: Arc<dyn Source> = Arc::new(MemSource::new(vec![1, 2, 3]));
    let t = TimeoutSource::new(shared, Duration::from_secs(1));
    assert_eq!(t.read_exact_vec(0, 3).unwrap(), vec![1, 2, 3]);
}
