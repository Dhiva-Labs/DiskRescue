//! Bounded, cached access to the bytes of one candidate file.
//!
//! Every walker in this crate reads through a [`Win`]: a window that starts
//! at the candidate's first byte and never extends past `start + limit` or
//! the end of the source. Offsets handed to and returned from a window are
//! **file-relative**, which is also what [`diskrescue_core::format::Measured`]
//! wants. Reads go through [`BlockCache`] so the many small, scattered reads
//! of an IFD walk are cheap, and the window is a [`SubSource`] so the limit
//! is enforced by construction: no code path can read beyond it.
//!
//! [`Cursor`] adds a forward read-ahead buffer for the walkers that traverse
//! a file front to back (GIF sub-blocks, RIFF chunks, PSD tables).

use std::sync::Arc;

use diskrescue_core::source::{BlockCache, MemSource, SharedSource, Source, SubSource};
use diskrescue_core::{Error, ReadError, ReadErrorKind};

/// Why a window read could not be satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fail {
    /// The window ended (end of source, or the caller's limit) at this
    /// file-relative offset.
    Eof(u64),
    /// The medium failed to deliver the byte at this file-relative offset.
    Io(u64),
}

impl Fail {
    /// The file-relative offset the failure is about.
    pub(crate) fn offset(self) -> u64 {
        match self {
            Fail::Eof(o) | Fail::Io(o) => o,
        }
    }
}

/// A read-only window onto one candidate file.
pub(crate) struct Win {
    cache: BlockCache,
    avail: u64,
    capped: bool,
    start: u64,
}

impl Win {
    /// Window of at most `limit` bytes starting at `start` in `src`.
    pub(crate) fn new(src: &SharedSource, start: u64, limit: u64, block: usize) -> Win {
        let remaining = src.size().saturating_sub(start);
        let avail = remaining.min(limit);
        let capped = limit < remaining;
        let sub: SharedSource = Arc::new(SubSource::new(src.clone(), start, avail));
        Win {
            cache: BlockCache::new(sub, block, 64),
            avail,
            capped,
            start,
        }
    }

    /// Window over an in-memory buffer (used by `probe`).
    pub(crate) fn from_bytes(bytes: &[u8]) -> Win {
        let src: SharedSource = Arc::new(MemSource::new(bytes.to_vec()));
        Win::new(&src, 0, u64::MAX, 4096)
    }

    /// Number of readable bytes in the window.
    pub(crate) fn avail(&self) -> u64 {
        self.avail
    }

    /// True when the window ends because of the caller's limit rather than
    /// the end of the source.
    pub(crate) fn capped(&self) -> bool {
        self.capped
    }

    /// Absolute offset of the window's first byte in the underlying source.
    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    /// The failure for "the source returned nothing more at `at`": the end
    /// of the window if `at` is there, otherwise a hole in the medium (the
    /// block cache returns a short read in front of an unreadable byte).
    pub(crate) fn short_fail(&self, at: u64) -> Fail {
        if at >= self.avail {
            Fail::Eof(self.avail)
        } else {
            Fail::Io(at)
        }
    }

    /// Read up to `len` bytes at `off`. The result is shorter than `len` only
    /// at the end of the window or in front of an unreadable byte, and empty
    /// when `off` is at or past the end of the window.
    pub(crate) fn read_upto(&self, off: u64, len: usize) -> Result<Vec<u8>, Fail> {
        if off >= self.avail || len == 0 {
            return Ok(Vec::new());
        }
        let want = (self.avail - off).min(len as u64) as usize;
        let mut buf = vec![0u8; want];
        match self.cache.read_at(off, &mut buf) {
            Ok(0) => Err(self.short_fail(off)),
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e) => {
                let at = e.offset.clamp(off, off + want as u64);
                // Hand back the good prefix when the failure is further on.
                if at > off {
                    let mut prefix = vec![0u8; (at - off) as usize];
                    if let Ok(n) = self.cache.read_at(off, &mut prefix) {
                        prefix.truncate(n);
                        if !prefix.is_empty() {
                            return Ok(prefix);
                        }
                    }
                }
                Err(Fail::Io(at))
            }
        }
    }

    /// Read exactly `len` bytes at `off`.
    pub(crate) fn read(&self, off: u64, len: usize) -> Result<Vec<u8>, Fail> {
        let v = self.read_upto(off, len)?;
        if v.len() < len {
            return Err(self.short_fail(off.saturating_add(v.len() as u64)));
        }
        Ok(v)
    }

    /// Read up to `buf.len()` bytes at `off` straight from the source,
    /// bypassing the block cache (for sparse probes that would otherwise
    /// pull a whole cache block per sample). Returns the number of bytes
    /// read, `None` when the medium failed.
    pub(crate) fn peek(&self, off: u64, buf: &mut [u8]) -> Option<usize> {
        if off >= self.avail {
            return Some(0);
        }
        let want = ((self.avail - off).min(buf.len() as u64)) as usize;
        let mut done = 0usize;
        while done < want {
            match self
                .cache
                .inner()
                .read_at(off + done as u64, &mut buf[done..want])
            {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(_) => return None,
            }
        }
        Some(done)
    }

    /// File-relative ranges of the window the source already knows to be
    /// unreadable.
    pub(crate) fn unreadable(&self) -> Option<diskrescue_core::RangeSet> {
        self.cache.inner().unreadable().filter(|s| !s.is_empty())
    }
}

/// Forward read-ahead over a [`Win`].
///
/// When a refill stops in front of an unreadable byte the cursor remembers
/// where. Walkers that hop over data (GIF sub-blocks, RIFF chunks) would
/// otherwise carry on past the hole as if nothing had happened; instead any
/// access that reaches the hole fails with [`Fail::Io`] at its first byte.
pub(crate) struct Cursor<'a> {
    win: &'a Win,
    buf: Vec<u8>,
    base: u64,
    chunk: usize,
    hole: Option<u64>,
}

impl<'a> Cursor<'a> {
    /// Cursor refilling `chunk` bytes at a time.
    pub(crate) fn new(win: &'a Win, chunk: usize) -> Self {
        Cursor {
            win,
            buf: Vec::new(),
            base: 0,
            chunk: chunk.max(64),
            hole: None,
        }
    }

    /// `n` bytes at `off`.
    pub(crate) fn slice(&mut self, off: u64, n: usize) -> Result<&[u8], Fail> {
        if let Some(h) = self.hole {
            if off.saturating_add(n as u64) > h {
                return Err(Fail::Io(h));
            }
        }
        let in_buf = off >= self.base
            && off
                .checked_add(n as u64)
                .is_some_and(|e| e <= self.base + self.buf.len() as u64);
        if !in_buf {
            let want = n.max(self.chunk);
            let v = self.win.read_upto(off, want)?;
            let reachable = (self.win.avail() - off.min(self.win.avail())).min(want as u64);
            if (v.len() as u64) < reachable {
                self.hole = Some(off + v.len() as u64);
            }
            if v.len() < n {
                return Err(self.win.short_fail(off.saturating_add(v.len() as u64)));
            }
            self.base = off;
            self.buf = v;
        }
        let s = (off - self.base) as usize;
        Ok(&self.buf[s..s + n])
    }

    /// One byte at `off`.
    pub(crate) fn byte(&mut self, off: u64) -> Result<u8, Fail> {
        Ok(self.slice(off, 1)?[0])
    }
}

/// Convert a window failure that happened before anything could be said
/// about the file into the crate-wide error type.
pub(crate) fn read_error(win: &Win, at: u64) -> Error {
    Error::Read(ReadError::new(
        win.start().saturating_add(at),
        0,
        ReadErrorKind::Io,
        "unreadable bytes at the start of a candidate file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::{Range, RangeSet};

    fn mem(n: usize) -> SharedSource {
        Arc::new(MemSource::new((0..n).map(|i| (i % 251) as u8).collect()))
    }

    #[test]
    fn window_respects_start_and_limit() {
        let src = mem(10_000);
        let w = Win::new(&src, 100, 500, 512);
        assert_eq!(w.avail(), 500);
        assert!(w.capped());
        assert_eq!(w.read(0, 4).unwrap(), vec![100, 101, 102, 103]);
        assert_eq!(w.read_upto(498, 10).unwrap().len(), 2);
        assert_eq!(w.read(498, 10), Err(Fail::Eof(500)));
        assert!(w.read_upto(500, 4).unwrap().is_empty());
    }

    #[test]
    fn window_at_end_of_source_is_not_capped() {
        let src = mem(1000);
        let w = Win::new(&src, 900, u64::MAX, 512);
        assert_eq!(w.avail(), 100);
        assert!(!w.capped());
        let w = Win::new(&src, 5000, 10, 512);
        assert_eq!(w.avail(), 0);
    }

    #[test]
    fn unreadable_ranges_become_io_failures() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(1024, 2048));
        let src: SharedSource = Arc::new(
            MemSource::new((0..4096).map(|i| (i % 251) as u8).collect()).with_unreadable(bad),
        );
        let w = Win::new(&src, 0, u64::MAX, 512);
        assert!(w.read(0, 100).is_ok());
        assert_eq!(w.read(1500, 10), Err(Fail::Io(1500)));
        assert_eq!(w.read(3000, 10).unwrap().len(), 10);
    }

    #[test]
    fn a_hole_inside_a_block_is_reported_where_it_is() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(700, 760));
        let src: SharedSource = Arc::new(
            MemSource::new((0..4096).map(|i| (i % 251) as u8).collect()).with_unreadable(bad),
        );
        let w = Win::new(&src, 0, u64::MAX, 4096);
        assert_eq!(w.read_upto(0, 1000).unwrap().len(), 700);
        assert_eq!(w.read(690, 20), Err(Fail::Io(700)));
        // Past the hole the medium delivers again.
        assert_eq!(w.read(800, 4).unwrap(), vec![47, 48, 49, 50]);
        assert_eq!(w.read(760, 4).unwrap().len(), 4);
        assert_eq!(w.read(740, 4), Err(Fail::Io(740)));
        assert_eq!(w.read_upto(5000, 4).unwrap().len(), 0);
    }

    #[test]
    fn cursor_does_not_hop_over_a_hole() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(3000, 3100));
        let src: SharedSource = Arc::new(
            MemSource::new((0..8192).map(|i| (i % 251) as u8).collect()).with_unreadable(bad),
        );
        let w = Win::new(&src, 0, u64::MAX, 4096);
        let mut c = Cursor::new(&w, 4096);
        assert_eq!(c.byte(10).unwrap(), 10);
        // The refill saw the hole at 3000: reads before it still work,
        // anything touching or beyond it fails at the hole.
        assert_eq!(c.slice(2990, 10).unwrap().len(), 10);
        assert_eq!(c.slice(2995, 10), Err(Fail::Io(3000)));
        assert_eq!(c.byte(3050), Err(Fail::Io(3000)));
        assert_eq!(c.byte(5000), Err(Fail::Io(3000)));
        assert_eq!(c.byte(0).unwrap(), 0);
    }

    #[test]
    fn cursor_refills_and_reports_eof() {
        let src = mem(1000);
        let w = Win::new(&src, 0, u64::MAX, 512);
        let mut c = Cursor::new(&w, 128);
        assert_eq!(c.byte(0).unwrap(), 0);
        assert_eq!(c.byte(500).unwrap(), (500 % 251) as u8);
        assert_eq!(c.slice(990, 10).unwrap().len(), 10);
        assert_eq!(c.slice(995, 10), Err(Fail::Eof(1000)));
        assert_eq!(c.byte(1000), Err(Fail::Eof(1000)));
    }
}
