//! Windowed random-access reader used by every walker in this crate.
//!
//! A [`Window`] presents the bytes of one candidate file (`start .. start +
//! limit` of a [`Source`]) as a file-relative address space. It keeps one
//! buffer, refilled on demand, so a walker that reads many small headers
//! issues few, large reads, and a walker that jumps far ahead (skipping a big
//! chunk) does not drag a large speculative read along.
//!
//! Read errors never panic and never abort a walk by themselves: the first
//! failing offset is remembered ([`Window::fault`]) and the failing call
//! returns fewer bytes than requested, which every walker already treats as
//! "cannot continue here". The measuring code then reports the stop as
//! `Truncated` with `err_offset` at the failing offset.

use diskrescue_core::util::bytes;
use diskrescue_core::{ReadError, Source};

/// Default speculative fill size for forward (sequential) access.
const FILL_SEQUENTIAL: usize = 64 * 1024;
/// Fill size after a long jump; just enough for a header or two.
const FILL_SEEK: usize = 4 * 1024;
/// A jump forward of at most this many bytes past the buffer still counts as
/// sequential access.
const SEQUENTIAL_SLACK: u64 = 16 * 1024;
/// Largest single request any walker may make (guards against absurd sizes
/// read from hostile files).
pub(crate) const MAX_REQUEST: usize = 16 * 1024 * 1024;

/// First read failure seen by a [`Window`].
#[derive(Debug, Clone)]
pub(crate) struct Fault {
    /// File-relative offset of the first byte that could not be read.
    pub offset: u64,
    /// The underlying error.
    #[allow(dead_code)]
    pub error: ReadError,
}

/// Buffered, bounds-limited view of a candidate file inside a source.
pub(crate) struct Window<'a> {
    src: &'a dyn Source,
    base: u64,
    avail: u64,
    limited: bool,
    buf: Vec<u8>,
    buf_start: u64,
    fault: Option<Fault>,
    bytes_fetched: u64,
}

impl<'a> Window<'a> {
    /// Create a window over `src[start .. start + limit]`, clamped to the
    /// size of the source.
    pub fn new(src: &'a dyn Source, start: u64, limit: u64) -> Self {
        let size = src.size();
        let remaining = size.saturating_sub(start);
        let avail = remaining.min(limit);
        Window {
            src,
            base: start,
            avail,
            limited: limit < remaining,
            buf: Vec::new(),
            buf_start: 0,
            fault: None,
            bytes_fetched: 0,
        }
    }

    /// Number of readable bytes (file-relative end of the window).
    pub fn avail(&self) -> u64 {
        self.avail
    }

    /// True when the window ends at the caller's `limit` rather than at the
    /// end of the source (so more data may exist beyond it).
    pub fn limited(&self) -> bool {
        self.limited
    }

    /// The first read failure, if any.
    pub fn fault(&self) -> Option<&Fault> {
        self.fault.as_ref()
    }

    /// Forget the recorded read failure when it lies before `pos`: the
    /// caller accounted for it (a known hole it walked across), and a later
    /// failure should be recorded instead.
    pub fn forget_fault_before(&mut self, pos: u64) {
        if self.fault.as_ref().is_some_and(|f| f.offset < pos) {
            self.fault = None;
        }
    }

    /// Total bytes requested from the source so far (for tests and tuning).
    #[allow(dead_code)]
    pub fn bytes_fetched(&self) -> u64 {
        self.bytes_fetched
    }

    fn buf_end(&self) -> u64 {
        self.buf_start + self.buf.len() as u64
    }

    /// Up to `n` bytes at file offset `pos`, clamped to the window. The slice
    /// is shorter than `n` near the end of the window or after a read
    /// failure, and empty beyond it.
    pub fn get(&mut self, pos: u64, n: usize) -> &[u8] {
        let n = n.min(MAX_REQUEST);
        if pos >= self.avail || n == 0 {
            return &[];
        }
        let want = (n as u64).min(self.avail - pos) as usize;
        let covered = pos >= self.buf_start && pos + want as u64 <= self.buf_end();
        if !covered {
            self.fill(pos, want);
        }
        if pos >= self.buf_start && pos < self.buf_end() {
            let off = (pos - self.buf_start) as usize;
            let end = (off + want).min(self.buf.len());
            &self.buf[off..end]
        } else {
            &[]
        }
    }

    /// Exactly `n` bytes at `pos`, or `None` when fewer are available.
    pub fn exact(&mut self, pos: u64, n: usize) -> Option<&[u8]> {
        let s = self.get(pos, n);
        if s.len() == n {
            Some(s)
        } else {
            None
        }
    }

    /// Copy of `n` bytes at `pos` (for small structures).
    pub fn vec(&mut self, pos: u64, n: usize) -> Option<Vec<u8>> {
        self.exact(pos, n).map(|s| s.to_vec())
    }

    /// One byte.
    pub fn u8(&mut self, pos: u64) -> Option<u8> {
        self.exact(pos, 1).map(|s| s[0])
    }

    /// Little-endian `u32`.
    pub fn u32le(&mut self, pos: u64) -> Option<u32> {
        bytes::u32le(self.exact(pos, 4)?, 0)
    }

    /// A four-character code.
    pub fn fourcc(&mut self, pos: u64) -> Option<[u8; 4]> {
        let s = self.exact(pos, 4)?;
        Some([s[0], s[1], s[2], s[3]])
    }

    fn fill(&mut self, pos: u64, want: usize) {
        let sequential = pos >= self.buf_start && pos <= self.buf_end() + SEQUENTIAL_SLACK;
        let spec = if sequential {
            FILL_SEQUENTIAL
        } else {
            FILL_SEEK
        };
        let len = want.max(spec).min((self.avail - pos) as usize);
        self.buf.clear();
        self.buf.resize(len, 0);
        self.buf_start = pos;
        let mut done = 0usize;
        let mut failure: Option<ReadError> = None;
        while done < len {
            match self
                .src
                .read_at(self.base + pos + done as u64, &mut self.buf[done..])
            {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        self.bytes_fetched += len as u64;
        self.buf.truncate(done);
        if let Some(e) = failure {
            // A failure in the speculative tail does not matter when the
            // bytes actually requested were delivered.
            if done < want {
                let at = e.offset.saturating_sub(self.base).max(pos + done as u64);
                if self.fault.is_none() {
                    log::debug!("read failure at file offset {at}: {e}");
                    self.fault = Some(Fault {
                        offset: at,
                        error: e,
                    });
                }
            }
        }
    }
}

/// Printable form of a fourcc for notes and metadata.
pub(crate) fn fourcc_string(cc: [u8; 4]) -> String {
    cc.iter()
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                '.'
            }
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Add a note when the measured extent overlaps known-unreadable ranges.
pub(crate) fn note_unreadable(
    src: &dyn Source,
    start: u64,
    m: &mut diskrescue_core::format::Measured,
) {
    if let Some(set) = src.unreadable() {
        let r = diskrescue_core::Range::with_len(start, m.len);
        let inside = set.intersection_with_range(r).total_len();
        if inside > 0 {
            m.notes.push(format!(
                "{inside} bytes inside this file are in known-unreadable ranges"
            ));
        }
    }
}

/// Small deterministic xorshift generator (tests and sampling only).
#[cfg(test)]
pub(crate) struct XorShift(pub u64);

#[cfg(test)]
impl XorShift {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 24) as u8).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use diskrescue_core::{Range, RangeSet};

    #[test]
    fn window_clamps_and_reads() {
        let src = MemSource::new((0..=255u8).collect());
        let mut w = Window::new(&src, 10, 100);
        assert_eq!(w.avail(), 100);
        assert!(w.limited());
        assert_eq!(w.get(0, 4), &[10, 11, 12, 13]);
        assert_eq!(w.get(98, 10), &[108, 109]);
        assert!(w.get(100, 1).is_empty());
        assert_eq!(w.u32le(0), Some(0x0D0C0B0A));
        assert_eq!(w.u8(1), Some(11));
        assert!(w.exact(98, 3).is_none());
        let w2 = Window::new(&src, 250, u64::MAX);
        assert_eq!(w2.avail(), 6);
        assert!(!w2.limited());
        assert!(Window::new(&src, 1000, 10).get(0, 1).is_empty());
    }

    #[test]
    fn window_records_fault_but_serves_good_prefix() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(5000, 6000));
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let src = MemSource::new(data.clone()).with_unreadable(bad);
        let mut w = Window::new(&src, 0, u64::MAX);
        // The speculative fill crosses the hole but the request does not.
        assert_eq!(w.get(0, 16), &data[..16]);
        assert!(w.fault().is_none());
        assert!(w.get(5100, 8).is_empty());
        assert_eq!(w.fault().unwrap().offset, 5100);
    }

    #[test]
    fn fourcc_printing() {
        assert_eq!(fourcc_string(*b"FMP4"), "FMP4");
        assert_eq!(fourcc_string([0, b'a', 1, b'b']), ".a.b");
        assert_eq!(fourcc_string(*b"DIV3"), "DIV3");
    }
}
