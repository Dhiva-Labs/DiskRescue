//! A [`Source`] adapter that never fails on bad sectors.
//!
//! Every read the carver makes (scanner chunks, `measure`, extraction,
//! hashing) goes through [`ZeroFill`]. It
//!
//! * zero-fills byte ranges the inner source already reports as unreadable
//!   (`Source::unreadable`, e.g. from a ddrescue mapfile) without touching
//!   them;
//! * on a media error (`Io`, `Timeout`) re-reads the good prefix of the
//!   failed request, then skips exactly the failing sector, zero-fills it and
//!   records it as "discovered" so reports can name it;
//! * treats a read that returns no data before the advertised end of the
//!   source (a shrunk image, an optimistic device size) the same way, instead
//!   of taking it for the end of the data;
//! * re-opens the inner source once on `DeviceGone` and retries;
//! * expands reads to sector boundaries through a bounce buffer when the
//!   inner source needs aligned I/O;
//! * fails every read immediately once the shared abort flag is raised, so
//!   cancelled `measure` calls on worker threads return quickly.
//!
//! The result is the behaviour the carving spec assumes of its source: holes
//! read as zeros and are reported separately, so a hole inside a video's
//! `mdat` does not stop the box walk.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use diskrescue_core::{Range, RangeSet, ReadError, ReadErrorKind, SharedSource, Source};

/// Retries of a shrinking good prefix before the carver gives up on it and
/// treats the current position as bad.
const MAX_PREFIX_RETRIES: u32 = 8;

/// A source that returns no data before the end it advertised (it shrank,
/// or its size was optimistic) has each such sector zero-filled and
/// recorded as unreadable, like a media error. After this many such sectors
/// in a row, the rest of the request is treated the same way at once, so a
/// source that lost its tail costs one read per sector only briefly.
const MAX_EMPTY_SECTORS: u32 = 64;

/// Zero-filling, error-absorbing view of a source. See the module docs.
pub(crate) struct ZeroFill {
    inner: SharedSource,
    size: u64,
    sector: u64,
    aligned_io: bool,
    known: RangeSet,
    discovered: RwLock<RangeSet>,
    has_discovered: AtomicBool,
    abort: Arc<AtomicBool>,
}

impl ZeroFill {
    /// Wrap `inner`. `known` lists ranges to zero-fill without reading;
    /// `abort` makes every later read fail with `Unsupported`.
    pub(crate) fn new(inner: SharedSource, known: RangeSet, abort: Arc<AtomicBool>) -> Self {
        let size = inner.size();
        let sector = u64::from(inner.sector_size().max(1));
        let aligned_io = inner.requires_alignment();
        ZeroFill {
            inner,
            size,
            sector,
            aligned_io,
            known: known.intersection_with_range(Range::new(0, size)),
            discovered: RwLock::new(RangeSet::new()),
            has_discovered: AtomicBool::new(false),
            abort,
        }
    }

    /// Bad ranges found while reading (not in the known set).
    pub(crate) fn discovered(&self) -> RangeSet {
        self.discovered
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Known plus discovered unreadable bytes inside `window` (absolute).
    pub(crate) fn holes_within(&self, window: Range) -> RangeSet {
        let mut out = self.known.intersection_with_range(window);
        if self.has_discovered.load(Ordering::Acquire) {
            let d = self.discovered.read().unwrap_or_else(|p| p.into_inner());
            for r in d.intersection_with_range(window).iter() {
                out.insert(r);
            }
        }
        out
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    fn abort_error(offset: u64, len: usize) -> ReadError {
        ReadError::new(offset, len, ReadErrorKind::Unsupported, "carving cancelled")
    }

    fn record_bad(&self, r: Range) {
        if r.is_empty() {
            return;
        }
        log::debug!(
            "unreadable bytes {}..{} on {}; zero-filled",
            r.start,
            r.end,
            self.inner.describe()
        );
        let mut d = self.discovered.write().unwrap_or_else(|p| p.into_inner());
        d.insert(r);
        self.has_discovered.store(true, Ordering::Release);
    }

    /// First bad range (known or discovered) intersecting `r`.
    fn next_hole(&self, r: Range) -> Option<Range> {
        let a = self.known.first_intersecting(r);
        let b = if self.has_discovered.load(Ordering::Acquire) {
            self.discovered
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .first_intersecting(r)
        } else {
            None
        };
        match (a, b) {
            (Some(x), Some(y)) => Some(if y.start < x.start { y } else { x }),
            (x, None) => x,
            (None, y) => y,
        }
    }

    /// One read of the inner source, expanded to sector boundaries when the
    /// inner source needs aligned I/O.
    fn inner_read(&self, pos: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let s = self.sector;
        if !self.aligned_io || (pos % s == 0 && buf.len() as u64 % s == 0) {
            // A source claiming more bytes than asked for violates the
            // contract; never let that run a position past the buffer.
            return self.inner.read_at(pos, buf).map(|n| n.min(buf.len()));
        }
        let a = pos / s * s;
        let end = pos + buf.len() as u64;
        let b = end.div_ceil(s).saturating_mul(s);
        let mut tmp = vec![0u8; (b - a) as usize];
        let n = self.inner.read_at(a, &mut tmp)?.min(tmp.len());
        let got_end = a + n as u64;
        if got_end <= pos {
            return Ok(0);
        }
        let avail = (got_end - pos).min(buf.len() as u64) as usize;
        let off = (pos - a) as usize;
        buf[..avail].copy_from_slice(&tmp[off..off + avail]);
        Ok(avail)
    }

    /// Zero-fill and record `[pos, sector end)` (clamped to `end`) as
    /// unreadable; returns the new position.
    fn skip_sector(&self, start: u64, pos: u64, end: u64, buf: &mut [u8]) -> u64 {
        let bad_end = (pos + 1).div_ceil(self.sector).saturating_mul(self.sector);
        let bad_end = bad_end.clamp(pos + 1, end);
        buf[(pos - start) as usize..(bad_end - start) as usize].fill(0);
        self.record_bad(Range::new(pos, bad_end));
        bad_end
    }

    /// Fill `buf` (which maps to `[start, start + buf.len())`, no known holes
    /// inside, inside the advertised size) from the inner source, absorbing
    /// media errors and missing data. Returns `buf.len()` unless aborted or
    /// the source fails for good.
    fn read_piece(&self, start: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let end = start + buf.len() as u64;
        let mut pos = start;
        let mut limit = end;
        let mut retries = 0u32;
        let mut empty = 0u32;
        let mut reopened = false;
        while pos < end {
            if self.aborted() {
                return Err(Self::abort_error(pos, (end - pos) as usize));
            }
            let lim = limit.clamp(pos + 1, end);
            let lo = (pos - start) as usize;
            let hi = (lim - start) as usize;
            match self.inner_read(pos, &mut buf[lo..hi]) {
                Ok(0) => {
                    // No data before the advertised end: the contract says
                    // that cannot happen, so do not take it as the end of
                    // the source (that would silently skip the rest of a
                    // whole chunk). Treat it as unreadable instead.
                    empty += 1;
                    if empty >= MAX_EMPTY_SECTORS {
                        log::warn!(
                            "{} returns no data from offset {pos} on, before its advertised end; treating {} bytes as unreadable",
                            self.inner.describe(),
                            end - pos
                        );
                        buf[lo..].fill(0);
                        self.record_bad(Range::new(pos, end));
                        pos = end;
                    } else {
                        pos = self.skip_sector(start, pos, end, buf);
                    }
                    limit = end;
                    retries = 0;
                }
                Ok(n) => {
                    pos += n as u64;
                    retries = 0;
                    empty = 0;
                    if pos >= limit {
                        limit = end;
                    }
                }
                Err(e) if e.kind.is_media_error() => {
                    if e.offset > pos && e.offset < lim && retries < MAX_PREFIX_RETRIES {
                        // The bytes before the failing offset may not have
                        // been written: read just that good prefix again.
                        limit = e.offset;
                        retries += 1;
                        continue;
                    }
                    pos = self.skip_sector(start, pos, end, buf);
                    limit = end;
                    retries = 0;
                }
                Err(e) if e.kind == ReadErrorKind::DeviceGone && !reopened => {
                    reopened = true;
                    log::warn!(
                        "{} disappeared during carving; trying to re-open it",
                        self.inner.describe()
                    );
                    self.inner.reopen().map_err(|_| e)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok((pos - start) as usize)
    }
}

impl Source for ZeroFill {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.aborted() {
            return Err(Self::abort_error(offset, buf.len()));
        }
        if offset >= self.size {
            return Ok(0);
        }
        let want = (self.size - offset).min(buf.len() as u64);
        let end = offset + want;
        let mut pos = offset;
        while pos < end {
            let lo = (pos - offset) as usize;
            match self.next_hole(Range::new(pos, end)) {
                Some(h) if h.start <= pos => {
                    let stop = h.end.min(end);
                    buf[lo..(stop - offset) as usize].fill(0);
                    pos = stop;
                }
                hole => {
                    let stop = hole.map_or(end, |h| h.start);
                    let hi = (stop - offset) as usize;
                    let n = self.read_piece(pos, &mut buf[lo..hi])?;
                    pos += n as u64;
                    if n < hi - lo {
                        // The inner source ended early (it shrank, or its
                        // size was optimistic): report a short read.
                        break;
                    }
                }
            }
        }
        Ok((pos - offset) as usize)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn is_device(&self) -> bool {
        self.inner.is_device()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        Some(self.holes_within(Range::new(0, self.size)))
    }

    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use diskrescue_core::SourceExt;
    use std::sync::Mutex;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8 + 1).collect()
    }

    /// Fails reads that touch `bad` without advertising it via
    /// `unreadable()`, the way a real failing disk image behaves.
    struct Flaky {
        data: Vec<u8>,
        bad: RangeSet,
        calls: Mutex<u64>,
    }

    impl Source for Flaky {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
            *self.calls.lock().unwrap() += 1;
            let size = self.data.len() as u64;
            if offset >= size {
                return Ok(0);
            }
            let n = (size - offset).min(buf.len() as u64);
            if let Some(h) = self.bad.first_intersecting(Range::new(offset, offset + n)) {
                // Report the failing sector start like a disk would, after
                // scribbling over the buffer.
                buf.fill(0xEE);
                let at = h.start.max(offset) / 512 * 512;
                return Err(ReadError::new(
                    at.max(offset),
                    buf.len(),
                    ReadErrorKind::Io,
                    "bad",
                ));
            }
            buf[..n as usize].copy_from_slice(&self.data[offset as usize..(offset + n) as usize]);
            Ok(n as usize)
        }
        fn describe(&self) -> String {
            "flaky".into()
        }
    }

    #[test]
    fn known_holes_are_zero_filled_without_reading() {
        let d = data(10_000);
        let bad = RangeSet::from_range(Range::new(1000, 3000));
        let src: SharedSource = Arc::new(MemSource::new(d.clone()).with_unreadable(bad.clone()));
        let z = ZeroFill::new(src, bad, Arc::new(AtomicBool::new(false)));
        let got = z.read_exact_vec(0, 10_000).unwrap();
        assert_eq!(&got[..1000], &d[..1000]);
        assert!(got[1000..3000].iter().all(|&b| b == 0));
        assert_eq!(&got[3000..], &d[3000..]);
        assert!(z.discovered().is_empty());
        assert_eq!(
            z.holes_within(Range::new(0, 10_000)).as_slice(),
            &[Range::new(1000, 3000)]
        );
    }

    #[test]
    fn unknown_bad_sectors_are_skipped_at_sector_granularity() {
        let d = data(64 * 1024);
        let bad = RangeSet::from_ranges([Range::new(5000, 5001), Range::new(20_480, 21_504)]);
        let src: SharedSource = Arc::new(Flaky {
            data: d.clone(),
            bad,
            calls: Mutex::new(0),
        });
        let z = ZeroFill::new(src, RangeSet::new(), Arc::new(AtomicBool::new(false)));
        let got = z.read_exact_vec(100, 60_000).unwrap();
        let disc = z.discovered();
        assert_eq!(
            disc.as_slice(),
            &[Range::new(4608, 5120), Range::new(20_480, 21_504)]
        );
        for (i, &b) in got.iter().enumerate() {
            let abs = 100 + i as u64;
            if disc.contains(abs) {
                assert_eq!(b, 0, "hole byte {abs}");
            } else {
                assert_eq!(b, d[abs as usize], "good byte {abs}");
            }
        }
        // A second read does not retry the discovered sectors.
        let got2 = z.read_exact_vec(100, 60_000).unwrap();
        assert_eq!(got, got2);
    }

    #[test]
    fn mem_source_simulated_holes_are_absorbed() {
        let d = data(8192);
        let bad = RangeSet::from_range(Range::new(700, 900));
        // The known set is not handed to ZeroFill: errors must be absorbed.
        let src: SharedSource = Arc::new(MemSource::new(d.clone()).with_unreadable(bad));
        let z = ZeroFill::new(src, RangeSet::new(), Arc::new(AtomicBool::new(false)));
        let got = z.read_exact_vec(0, 8192).unwrap();
        assert_eq!(&got[..700], &d[..700]);
        assert!(got[700..1024].iter().all(|&b| b == 0));
        assert_eq!(&got[1024..], &d[1024..]);
    }

    #[test]
    fn abort_fails_fast_and_short_reads_at_end() {
        let d = data(1000);
        let src: SharedSource = Arc::new(MemSource::new(d.clone()));
        let abort = Arc::new(AtomicBool::new(false));
        let z = ZeroFill::new(src, RangeSet::new(), abort.clone());
        let mut buf = vec![0u8; 100];
        assert_eq!(z.read_at(950, &mut buf).unwrap(), 50);
        assert_eq!(z.read_at(1000, &mut buf).unwrap(), 0);
        abort.store(true, Ordering::Relaxed);
        let e = z.read_at(0, &mut buf).unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Unsupported);
    }
}
