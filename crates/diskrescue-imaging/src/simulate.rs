//! A fault-injecting [`Source`] for tests: a failing device in memory.
//!
//! [`FaultySource`] serves bytes from a buffer (or a backing
//! [`FileSource`]) and injects the failure modes real media show:
//!
//! * `bad(range)`: every read touching the range fails with
//!   [`ReadErrorKind::Io`].
//! * `flaky(range, p, seed)`: each sector of the range fails with
//!   probability `p` on every read attempt. The outcome is a pure function
//!   of `(seed, sector, attempt number of that sector)`, so runs are
//!   reproducible regardless of thread timing.
//! * `slow(range, latency)`: reads touching the range sleep first.
//! * `hang(range, duration)`: reads touching the range take `duration`;
//!   with a per-read timeout shorter than that, the read returns
//!   [`ReadErrorKind::Timeout`] after the timeout (like an abandoned
//!   worker thread in the device layer).
//! * `vanish(after_reads, reopens_needed)`: once more than `after_reads`
//!   reads have been issued, every read returns
//!   [`ReadErrorKind::DeviceGone`] until [`Source::reopen`] has been called
//!   `reopens_needed` times (earlier calls fail with `DeviceGone`); then
//!   the device works again. May be given several times (sorted by
//!   `after_reads`) to make the device vanish repeatedly.
//! * `stale_after_error`: the read following a failed read returns zeros
//!   and reports success (a USB bridge returning stale buffers).
//! * `error_offset_granularity(g)`: a failed read reports the first failing
//!   offset rounded down to `g` (never below the read start); 0 reports the
//!   read start (no information, like most O_DIRECT block devices).
//! * `short_reads(true)`: a read that hits a fault after good sectors
//!   returns the good prefix as a short read instead of an error.
//! * `strict_alignment` (on by default): reads whose offset or length is
//!   not a multiple of the sector size fail with
//!   [`ReadErrorKind::Unaligned`] (a read may end at the source end).
//!
//! Every read is appended to a log ([`FaultySource::read_log`]) with its
//! outcome, and [`FaultySource::counters`] tracks totals, including
//! unaligned reads and reads extending past the end, so tests can assert
//! the engine never issues them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diskrescue_core::source::{FileSource, ReadError, ReadErrorKind, Source};
use diskrescue_core::Range;

/// What happened to one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimOutcome {
    /// Served this many bytes.
    Ok(usize),
    /// Reported success but returned zeros (stale data after an error).
    Stale(usize),
    /// Failed with this kind; `reported_at` is the offset in the error.
    Error {
        /// Classification returned.
        kind: ReadErrorKind,
        /// Offset reported in the error.
        reported_at: u64,
    },
}

/// One entry of the read log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    /// Requested offset.
    pub offset: u64,
    /// Requested length.
    pub len: usize,
    /// Timeout passed by the caller, if any.
    pub timeout: Option<Duration>,
    /// Outcome.
    pub outcome: SimOutcome,
}

impl ReadRecord {
    /// True when the read returned data (including stale data).
    pub fn is_ok(&self) -> bool {
        matches!(self.outcome, SimOutcome::Ok(_) | SimOutcome::Stale(_))
    }

    /// Requested range.
    pub fn range(&self) -> Range {
        Range::with_len(self.offset, self.len as u64)
    }
}

/// Totals since the source was built.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimCounters {
    /// Reads issued (all outcomes).
    pub reads: u64,
    /// Reads that returned data.
    pub ok: u64,
    /// Reads that failed with a media error.
    pub errors: u64,
    /// Reads that timed out.
    pub timeouts: u64,
    /// Reads refused because the device was gone.
    pub device_gone: u64,
    /// Reads that returned stale zeros.
    pub stale: u64,
    /// Misaligned reads (refused when strict).
    pub unaligned: u64,
    /// Reads extending past the end of the source.
    pub out_of_range: u64,
    /// Calls to `reopen`.
    pub reopen_calls: u64,
    /// `reopen` calls that brought a vanished device back.
    pub successful_reopens: u64,
    /// Bytes returned (good data only).
    pub bytes_read: u64,
}

#[derive(Debug, Clone)]
enum FaultKind {
    Bad,
    Flaky { p: f64, seed: u64 },
    Slow(Duration),
    Hang(Duration),
}

#[derive(Debug, Clone)]
struct Fault {
    range: Range,
    kind: FaultKind,
}

#[derive(Debug, Clone, Copy)]
struct Vanish {
    after_reads: u64,
    reopens_needed: u32,
}

enum Backing {
    Mem(Arc<Vec<u8>>),
    File(FileSource),
}

#[derive(Default)]
struct SimState {
    log: Vec<ReadRecord>,
    counters: SimCounters,
    flaky_attempts: HashMap<u64, u32>,
    vanished: bool,
    /// Index of the next vanish event that has not fired yet.
    next_vanish: usize,
    /// `reopens_needed` of the event currently in effect.
    reopens_needed: u32,
    reopen_attempts: u32,
    stale_pending: bool,
}

/// Builder for [`FaultySource`].
pub struct FaultySourceBuilder {
    backing: Backing,
    sector: u32,
    faults: Vec<Fault>,
    vanish: Vec<Vanish>,
    stale_after_error: bool,
    granularity: Option<u64>,
    short_reads: bool,
    strict: bool,
    log_reads: bool,
    name: String,
}

impl FaultySourceBuilder {
    /// Logical sector size (default 512).
    pub fn sector_size(mut self, sector: u32) -> Self {
        self.sector = sector.max(1);
        self
    }

    /// Reads touching `range` always fail with an I/O error.
    pub fn bad(mut self, range: Range) -> Self {
        self.faults.push(Fault {
            range,
            kind: FaultKind::Bad,
        });
        self
    }

    /// Each sector in `range` fails with probability `fail_probability` on
    /// every attempt, deterministically from `seed`.
    pub fn flaky(mut self, range: Range, fail_probability: f64, seed: u64) -> Self {
        self.faults.push(Fault {
            range,
            kind: FaultKind::Flaky {
                p: fail_probability.clamp(0.0, 1.0),
                seed,
            },
        });
        self
    }

    /// Reads touching `range` take at least `latency` (and succeed).
    pub fn slow(mut self, range: Range, latency: Duration) -> Self {
        self.faults.push(Fault {
            range,
            kind: FaultKind::Slow(latency),
        });
        self
    }

    /// Reads touching `range` take `duration`; a shorter per-read timeout
    /// turns them into [`ReadErrorKind::Timeout`].
    pub fn hang(mut self, range: Range, duration: Duration) -> Self {
        self.faults.push(Fault {
            range,
            kind: FaultKind::Hang(duration),
        });
        self
    }

    /// Once more than `after_reads` reads were issued the device vanishes
    /// until `reopen` has been called `reopens_needed` times. Can be called
    /// several times for repeated losses.
    pub fn vanish(mut self, after_reads: u64, reopens_needed: u32) -> Self {
        self.vanish.push(Vanish {
            after_reads,
            reopens_needed,
        });
        self.vanish.sort_by_key(|v| v.after_reads);
        self
    }

    /// The read following a failed read returns zeros and succeeds.
    pub fn stale_after_error(mut self, on: bool) -> Self {
        self.stale_after_error = on;
        self
    }

    /// Precision of the failing offset in errors (default: the sector
    /// size; 0 = always the read start).
    pub fn error_offset_granularity(mut self, granularity: u64) -> Self {
        self.granularity = Some(granularity);
        self
    }

    /// Return the good prefix as a short read instead of failing.
    pub fn short_reads(mut self, on: bool) -> Self {
        self.short_reads = on;
        self
    }

    /// Refuse misaligned reads (default on).
    pub fn strict_alignment(mut self, on: bool) -> Self {
        self.strict = on;
        self
    }

    /// Keep a log of every read (default on).
    pub fn log_reads(mut self, on: bool) -> Self {
        self.log_reads = on;
        self
    }

    /// Name returned by `describe`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Build the source.
    pub fn build(self) -> FaultySource {
        let size = match &self.backing {
            Backing::Mem(v) => v.len() as u64,
            Backing::File(f) => f.size(),
        };
        let sector = self.sector as u64;
        FaultySource {
            backing: self.backing,
            size,
            sector: self.sector,
            faults: self.faults,
            vanish: self.vanish,
            stale_after_error: self.stale_after_error,
            granularity: self.granularity.unwrap_or(sector),
            short_reads: self.short_reads,
            strict: self.strict,
            log_reads: self.log_reads,
            name: self.name,
            state: Mutex::new(SimState::default()),
        }
    }
}

/// A simulated failing device. See the module documentation.
pub struct FaultySource {
    backing: Backing,
    size: u64,
    sector: u32,
    faults: Vec<Fault>,
    vanish: Vec<Vanish>,
    stale_after_error: bool,
    granularity: u64,
    short_reads: bool,
    strict: bool,
    log_reads: bool,
    name: String,
    state: Mutex<SimState>,
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn roll(seed: u64, sector: u64, attempt: u32) -> f64 {
    let h = splitmix64(seed ^ splitmix64(sector ^ splitmix64(attempt as u64 + 1)));
    (h >> 11) as f64 / (1u64 << 53) as f64
}

impl FaultySource {
    /// Start building a source over `data`.
    pub fn builder(data: Vec<u8>) -> FaultySourceBuilder {
        FaultySource::builder_shared(Arc::new(data))
    }

    /// Start building a source over shared data (no copy).
    pub fn builder_shared(data: Arc<Vec<u8>>) -> FaultySourceBuilder {
        FaultySourceBuilder {
            backing: Backing::Mem(data),
            sector: 512,
            faults: Vec::new(),
            vanish: Vec::new(),
            stale_after_error: false,
            granularity: None,
            short_reads: false,
            strict: true,
            log_reads: true,
            name: "simulated device".into(),
        }
    }

    /// Start building a source backed by a file.
    pub fn builder_file(file: FileSource) -> FaultySourceBuilder {
        let mut b = FaultySource::builder_shared(Arc::new(Vec::new()));
        b.name = format!("simulated device over {}", file.describe());
        b.backing = Backing::File(file);
        b
    }

    /// The in-memory data, when memory backed.
    pub fn data(&self) -> Option<&[u8]> {
        match &self.backing {
            Backing::Mem(v) => Some(v),
            Backing::File(_) => None,
        }
    }

    /// A copy of the read log.
    pub fn read_log(&self) -> Vec<ReadRecord> {
        self.lock().log.clone()
    }

    /// Number of reads logged so far.
    pub fn read_log_len(&self) -> usize {
        self.lock().log.len()
    }

    /// Forget the read log.
    pub fn clear_log(&self) {
        self.lock().log.clear();
    }

    /// Totals so far.
    pub fn counters(&self) -> SimCounters {
        self.lock().counters
    }

    /// True while the simulated device is gone.
    pub fn is_vanished(&self) -> bool {
        self.lock().vanished
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SimState> {
        // A panic in another test thread must not hide the state.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, st: &mut SimState, rec: ReadRecord) {
        if self.log_reads {
            st.log.push(rec);
        }
    }

    fn copy_data(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        match &self.backing {
            Backing::Mem(v) => {
                let s = offset as usize;
                buf.copy_from_slice(&v[s..s + buf.len()]);
                Ok(())
            }
            Backing::File(f) => {
                let mut done = 0usize;
                while done < buf.len() {
                    let n = f.read_at(offset + done as u64, &mut buf[done..])?;
                    if n == 0 {
                        return Err(ReadError::new(
                            offset + done as u64,
                            buf.len() - done,
                            ReadErrorKind::Io,
                            "backing file shorter than expected",
                        ));
                    }
                    done += n;
                }
                Ok(())
            }
        }
    }

    fn do_read(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> Result<usize, ReadError> {
        let len = buf.len();
        let sector = self.sector as u64;
        let mut st = self.lock();
        st.counters.reads += 1;
        if offset.saturating_add(len as u64) > self.size {
            st.counters.out_of_range += 1;
        }
        if offset >= self.size {
            self.record(
                &mut st,
                ReadRecord {
                    offset,
                    len,
                    timeout,
                    outcome: SimOutcome::Ok(0),
                },
            );
            return Ok(0);
        }
        let n = ((self.size - offset).min(len as u64)) as usize;
        let end = offset + n as u64;
        let misaligned = offset % sector != 0 || (len as u64 % sector != 0 && end != self.size);
        if misaligned {
            st.counters.unaligned += 1;
            if self.strict {
                let err = ReadError::unaligned(offset, len, self.sector);
                self.record(
                    &mut st,
                    ReadRecord {
                        offset,
                        len,
                        timeout,
                        outcome: SimOutcome::Error {
                            kind: ReadErrorKind::Unaligned,
                            reported_at: offset,
                        },
                    },
                );
                return Err(err);
            }
        }
        // Device presence.
        let gone = if st.vanished {
            true
        } else if let Some(v) = self.vanish.get(st.next_vanish).copied() {
            if st.counters.reads > v.after_reads {
                st.next_vanish += 1;
                st.vanished = true;
                st.reopens_needed = v.reopens_needed;
                st.reopen_attempts = 0;
                true
            } else {
                false
            }
        } else {
            false
        };
        if gone {
            st.counters.device_gone += 1;
            self.record(
                &mut st,
                ReadRecord {
                    offset,
                    len,
                    timeout,
                    outcome: SimOutcome::Error {
                        kind: ReadErrorKind::DeviceGone,
                        reported_at: offset,
                    },
                },
            );
            return Err(ReadError::new(
                offset,
                len,
                ReadErrorKind::DeviceGone,
                "simulated device disappeared",
            ));
        }
        let r = Range::new(offset, end);
        let mut delay = Duration::ZERO;
        let mut hang: Option<Duration> = None;
        let mut media_faults = false;
        for f in self.faults.iter().filter(|f| f.range.overlaps(r)) {
            match f.kind {
                FaultKind::Slow(d) => delay = delay.max(d),
                FaultKind::Hang(d) => hang = Some(hang.map_or(d, |h| h.max(d))),
                FaultKind::Bad | FaultKind::Flaky { .. } => media_faults = true,
            }
        }
        if let Some(h) = hang {
            if let Some(t) = timeout {
                if h > t {
                    st.counters.timeouts += 1;
                    self.record(
                        &mut st,
                        ReadRecord {
                            offset,
                            len,
                            timeout,
                            outcome: SimOutcome::Error {
                                kind: ReadErrorKind::Timeout,
                                reported_at: offset,
                            },
                        },
                    );
                    drop(st);
                    std::thread::sleep(t);
                    return Err(ReadError::new(
                        offset,
                        len,
                        ReadErrorKind::Timeout,
                        format!("simulated hang: no answer within {t:?}"),
                    ));
                }
            }
            delay = delay.max(h);
        }
        if st.stale_pending {
            st.stale_pending = false;
            st.counters.stale += 1;
            self.record(
                &mut st,
                ReadRecord {
                    offset,
                    len,
                    timeout,
                    outcome: SimOutcome::Stale(n),
                },
            );
            drop(st);
            buf[..n].fill(0);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            return Ok(n);
        }
        // First failing byte, sector by sector in address order.
        let mut fail_at: Option<(u64, u64)> = None;
        if media_faults {
            let mut s = offset - offset % sector;
            'sectors: while s < end {
                let sr = Range::new(s.max(offset), (s + sector).min(end));
                for f in self.faults.iter().filter(|f| f.range.overlaps(sr)) {
                    let failed = match f.kind {
                        FaultKind::Bad => true,
                        FaultKind::Flaky { p, seed } => {
                            let attempt = st.flaky_attempts.entry(s).or_insert(0);
                            let x = roll(seed, s, *attempt);
                            *attempt += 1;
                            x < p
                        }
                        _ => false,
                    };
                    if failed {
                        // (first failing byte, start of its sector in the read)
                        fail_at = Some((sr.start.max(f.range.start), sr.start));
                        break 'sectors;
                    }
                }
                s += sector;
            }
        }
        let outcome;
        let result;
        match fail_at {
            None => {
                st.counters.ok += 1;
                st.counters.bytes_read += n as u64;
                outcome = SimOutcome::Ok(n);
                result = Ok(n);
            }
            Some((fa, fsec)) => {
                if self.stale_after_error {
                    st.stale_pending = true;
                }
                if self.short_reads && fsec > offset {
                    let good = (fsec - offset) as usize;
                    st.counters.ok += 1;
                    st.counters.bytes_read += good as u64;
                    outcome = SimOutcome::Ok(good);
                    result = Ok(good);
                } else {
                    st.counters.errors += 1;
                    let reported = if self.granularity == 0 {
                        offset
                    } else {
                        (fa - fa % self.granularity).max(offset)
                    };
                    outcome = SimOutcome::Error {
                        kind: ReadErrorKind::Io,
                        reported_at: reported,
                    };
                    result = Err(ReadError::new(
                        reported,
                        len,
                        ReadErrorKind::Io,
                        "simulated media error",
                    ));
                }
            }
        }
        self.record(
            &mut st,
            ReadRecord {
                offset,
                len,
                timeout,
                outcome,
            },
        );
        drop(st);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        if let Ok(k) = result {
            self.copy_data(offset, &mut buf[..k])?;
        }
        result
    }
}

impl Source for FaultySource {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.do_read(offset, buf, None)
    }

    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        self.do_read(offset, buf, Some(timeout))
    }

    fn describe(&self) -> String {
        self.name.clone()
    }

    fn is_device(&self) -> bool {
        true
    }

    fn requires_alignment(&self) -> bool {
        self.strict
    }

    fn reopen(&self) -> Result<(), ReadError> {
        let mut st = self.lock();
        st.counters.reopen_calls += 1;
        if !st.vanished {
            return Ok(());
        }
        st.reopen_attempts += 1;
        if st.reopen_attempts >= st.reopens_needed {
            st.vanished = false;
            st.counters.successful_reopens += 1;
            Ok(())
        } else {
            Err(ReadError::new(
                0,
                0,
                ReadErrorKind::DeviceGone,
                "simulated device not present yet",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::SourceExt;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn clean_reads_and_alignment() {
        let s = FaultySource::builder(data(4096)).build();
        let mut b = vec![0u8; 1024];
        assert_eq!(s.read_at(512, &mut b).unwrap(), 1024);
        assert_eq!(&b[..], &data(4096)[512..1536]);
        assert_eq!(
            s.read_at(100, &mut b).unwrap_err().kind,
            ReadErrorKind::Unaligned
        );
        let mut tail = vec![0u8; 1024];
        assert_eq!(s.read_at(3584, &mut tail).unwrap(), 512);
        let c = s.counters();
        assert_eq!(c.unaligned, 1);
        assert_eq!(c.out_of_range, 1);
        assert_eq!(s.read_log().len(), 3);
    }

    #[test]
    fn bad_ranges_granularity_and_short_reads() {
        let s = FaultySource::builder(data(8192))
            .bad(Range::new(2048 + 100, 2048 + 101))
            .build();
        let mut b = vec![0u8; 4096];
        let e = s.read_at(0, &mut b).unwrap_err();
        assert_eq!((e.kind, e.offset), (ReadErrorKind::Io, 2048));
        let s0 = FaultySource::builder(data(8192))
            .bad(Range::new(2148, 2149))
            .error_offset_granularity(0)
            .build();
        assert_eq!(s0.read_at(0, &mut b).unwrap_err().offset, 0);
        let ss = FaultySource::builder(data(8192))
            .bad(Range::new(2148, 2149))
            .short_reads(true)
            .build();
        assert_eq!(ss.read_at(0, &mut b).unwrap(), 2048);
        assert_eq!(&b[..2048], &data(8192)[..2048]);
        assert!(ss.read_at(2048, &mut b).is_err());
    }

    #[test]
    fn flaky_is_deterministic() {
        let mk = || {
            FaultySource::builder(data(64 * 1024))
                .flaky(Range::new(0, 64 * 1024), 0.5, 42)
                .build()
        };
        let (a, b) = (mk(), mk());
        let mut buf = vec![0u8; 512];
        for i in 0..400u64 {
            let off = (i % 128) * 512;
            assert_eq!(
                a.read_at(off, &mut buf).is_ok(),
                b.read_at(off, &mut buf).is_ok()
            );
        }
        let c = a.counters();
        assert!(c.errors > 100 && c.ok > 100, "{c:?}");
    }

    #[test]
    fn vanish_and_reopen() {
        let s = FaultySource::builder(data(4096)).vanish(2, 2).build();
        let mut b = vec![0u8; 512];
        assert!(s.read_at(0, &mut b).is_ok());
        assert!(s.read_at(512, &mut b).is_ok());
        assert_eq!(
            s.read_at(1024, &mut b).unwrap_err().kind,
            ReadErrorKind::DeviceGone
        );
        assert!(s.reopen().is_err());
        assert!(s.is_vanished());
        assert!(s.reopen().is_ok());
        assert!(s.read_at(1024, &mut b).is_ok());
        assert_eq!(s.counters().reopen_calls, 2);
    }

    #[test]
    fn hang_times_out_and_stale_after_error() {
        let s = FaultySource::builder(data(4096))
            .hang(Range::new(0, 512), Duration::from_millis(30))
            .bad(Range::new(1024, 1536))
            .stale_after_error(true)
            .build();
        let mut b = vec![0u8; 512];
        let e = s
            .read_at_timeout(0, &mut b, Duration::from_millis(5))
            .unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Timeout);
        assert!(s.read_at_timeout(0, &mut b, Duration::from_secs(1)).is_ok());
        assert!(s.read_at(1024, &mut b).is_err());
        b.fill(7);
        assert_eq!(s.read_at(2048, &mut b).unwrap(), 512);
        assert!(b.iter().all(|&x| x == 0));
        assert_eq!(s.read_at(2048, &mut b).unwrap(), 512);
        assert_eq!(&b[..], &data(4096)[2048..2560]);
    }

    #[test]
    fn random_reads_never_panic() {
        let s = FaultySource::builder(data(10_000))
            .bad(Range::new(3000, 3500))
            .flaky(Range::new(6000, 7000), 0.3, 1)
            .strict_alignment(false)
            .build();
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..2000 {
            let off = rng.gen_range(0..12_000u64);
            let len = rng.gen_range(0..3000usize);
            let mut b = vec![0u8; len];
            let _ = s.read_at(off, &mut b);
        }
        assert!(s.read_vec(9_990, 100).is_ok());
    }
}
