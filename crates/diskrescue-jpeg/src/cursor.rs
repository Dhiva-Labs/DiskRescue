//! A bounded, read-ahead window over a [`Source`].
//!
//! Every parser in this crate reads through a [`Cursor`]: it keeps one
//! chunk (64 KiB for header walks, 256 KiB for scan data; never more than
//! 1 MiB) of the file in memory and refills it on demand, so arbitrarily
//! large files are processed in constant memory. Offsets are relative to the
//! start of the JPEG (`base` in the source) and nothing past `limit` is ever
//! read.
//!
//! Read errors (bad sectors) end the readable data at the failing offset:
//! the cursor shrinks its limit, remembers the first error and reports
//! "end of data" from there on. Parsers then see a truncated file, which is
//! exactly how the rest of the pipeline wants damage reported.

use diskrescue_core::source::{ReadError, Source};

/// Chunk size for header walks.
pub(crate) const HEADER_CHUNK: usize = 64 * 1024;
/// Chunk size for scan data.
pub(crate) const SCAN_CHUNK: usize = 256 * 1024;
/// Hard upper bound on any single read issued to the source.
pub(crate) const MAX_READ: usize = 1 << 20;

/// Windowed reader; see the module docs.
pub(crate) struct Cursor<'a> {
    src: &'a dyn Source,
    base: u64,
    limit: u64,
    buf: Vec<u8>,
    buf_start: u64,
    chunk: usize,
    read_error: Option<ReadError>,
}

impl<'a> Cursor<'a> {
    /// Read `src` from absolute offset `base`, at most `limit` bytes.
    pub(crate) fn new(src: &'a dyn Source, base: u64, limit: u64, chunk: usize) -> Self {
        let avail = src.size().saturating_sub(base);
        Cursor {
            src,
            base,
            limit: limit.min(avail),
            buf: Vec::new(),
            buf_start: 0,
            chunk: chunk.clamp(512, MAX_READ),
            read_error: None,
        }
    }

    /// Number of readable bytes (shrinks when a read error is hit).
    pub(crate) fn limit(&self) -> u64 {
        self.limit
    }

    /// Absolute offset of the file start in the source.
    pub(crate) fn base(&self) -> u64 {
        self.base
    }

    /// File-relative offset of the first read error, if any.
    pub(crate) fn read_error_offset(&self) -> Option<u64> {
        self.read_error
            .as_ref()
            .map(|e| e.offset.saturating_sub(self.base))
    }

    /// The byte at file offset `pos`, or `None` past the readable end.
    #[inline]
    pub(crate) fn byte(&mut self, pos: u64) -> Option<u8> {
        let i = pos.wrapping_sub(self.buf_start);
        if i < self.buf.len() as u64 {
            return Some(self.buf[i as usize]);
        }
        self.byte_slow(pos)
    }

    #[cold]
    fn byte_slow(&mut self, pos: u64) -> Option<u8> {
        if pos >= self.limit {
            return None;
        }
        self.fill(pos);
        let i = pos.wrapping_sub(self.buf_start);
        if i < self.buf.len() as u64 {
            Some(self.buf[i as usize])
        } else {
            None
        }
    }

    /// Big-endian u16 at `pos`.
    pub(crate) fn u16be(&mut self, pos: u64) -> Option<u16> {
        Some(u16::from_be_bytes([self.byte(pos)?, self.byte(pos + 1)?]))
    }

    fn fill(&mut self, pos: u64) {
        let want = (self.limit - pos).min(self.chunk as u64) as usize;
        self.buf.clear();
        self.buf.resize(want, 0);
        self.buf_start = pos;
        match self.src.read_at(self.base + pos, &mut self.buf) {
            Ok(n) => {
                self.buf.truncate(n);
                if n == 0 {
                    // The source ended earlier than its size suggested. A
                    // short but non-empty read is not trusted as the end:
                    // the next refill asks again and sees the real cause.
                    self.limit = pos;
                }
            }
            Err(e) => {
                let bad = e.offset.saturating_sub(self.base).max(pos);
                log::debug!("read error at {}: {}", e.offset, e);
                self.limit = bad.min(self.limit);
                if self.read_error.is_none() {
                    self.read_error = Some(e);
                }
                self.buf.clear();
                if bad > pos {
                    // The contract does not let us trust partially filled
                    // buffers: re-read the good prefix on its own.
                    let len = (bad - pos) as usize;
                    self.buf.resize(len, 0);
                    match self.src.read_at(self.base + pos, &mut self.buf) {
                        Ok(n) => self.buf.truncate(n),
                        Err(_) => {
                            self.buf.clear();
                            self.limit = pos;
                        }
                    }
                }
            }
        }
    }

    /// Up to `len` bytes starting at `pos` (shorter at the readable end).
    pub(crate) fn read_vec(&mut self, pos: u64, len: usize) -> Vec<u8> {
        let end = pos.saturating_add(len as u64).min(self.limit);
        let mut out = Vec::with_capacity((end.saturating_sub(pos)) as usize);
        let mut p = pos;
        while p < end {
            if self.byte(p).is_none() {
                break;
            }
            let i = (p - self.buf_start) as usize;
            let n = (self.buf.len() - i).min((end - p) as usize);
            out.extend_from_slice(&self.buf[i..i + n]);
            p += n as u64;
        }
        out
    }

    /// Read `from..to` once (chunk by chunk, nothing kept), so that a read
    /// error in that range is noticed: afterwards [`Cursor::limit`] and
    /// [`Cursor::read_error_offset`] reflect it.
    pub(crate) fn read_through(&mut self, from: u64, to: u64) {
        let mut p = from;
        while p < to.min(self.limit) {
            if self.byte(p).is_none() {
                return;
            }
            // `byte` filled the window from `p` (or `p` was inside it).
            let window_end = self.buf_start + self.buf.len() as u64;
            p = window_end.max(p + 1);
        }
    }

    /// Exactly `len` bytes at `pos`, or `None` when not all are readable.
    pub(crate) fn read_exact(&mut self, pos: u64, len: usize) -> Option<Vec<u8>> {
        let v = self.read_vec(pos, len);
        (v.len() == len).then_some(v)
    }

    /// Offset of the first `0xFF` at or after `from` and before `end`.
    pub(crate) fn find_ff(&mut self, from: u64, end: u64) -> Option<u64> {
        let end = end.min(self.limit);
        let mut p = from;
        while p < end {
            self.byte(p)?;
            let i = (p - self.buf_start) as usize;
            let n = (self.buf.len() - i).min((end - p) as usize);
            if let Some(k) = memchr::memchr(0xFF, &self.buf[i..i + n]) {
                return Some(p + k as u64);
            }
            p += n as u64;
        }
        None
    }

    /// Offset of the first occurrence of `needle` starting in `from..end`.
    pub(crate) fn find_bytes(&mut self, from: u64, end: u64, needle: &[u8]) -> Option<u64> {
        if needle.is_empty() {
            return None;
        }
        let end = end.min(self.limit);
        let finder = memchr::memmem::Finder::new(needle);
        let mut p = from;
        let overlap = needle.len() as u64 - 1;
        while p < end {
            let take = (end - p + overlap).min(self.chunk as u64) as usize;
            let window = self.read_vec(p, take);
            if window.len() < needle.len() {
                return None;
            }
            if let Some(k) = finder.find(&window) {
                let hit = p + k as u64;
                return (hit < end).then_some(hit);
            }
            let advance = (window.len() as u64).saturating_sub(overlap).max(1);
            p += advance;
        }
        None
    }
}

/// A [`Source`] over a borrowed byte slice (used by the slice-based entry
/// points without copying the input).
pub(crate) struct SliceSource<'a>(pub(crate) &'a [u8]);

impl Source for SliceSource<'_> {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let len = self.0.len() as u64;
        if offset >= len {
            return Ok(0);
        }
        let n = ((len - offset).min(buf.len() as u64)) as usize;
        buf[..n].copy_from_slice(&self.0[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "memory slice".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use diskrescue_core::{Range, RangeSet};

    #[test]
    fn reads_across_chunks_and_respects_limit() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let src = SliceSource(&data);
        let mut c = Cursor::new(&src, 10, 3000, 512);
        assert_eq!(c.byte(0), Some(10));
        assert_eq!(c.byte(2999), Some(((3009) % 251) as u8));
        assert_eq!(c.byte(3000), None);
        let v = c.read_vec(500, 1000);
        assert_eq!(v, data[510..1510].to_vec());
        assert_eq!(c.read_vec(2990, 100).len(), 10);
        assert!(c.read_exact(2990, 11).is_none());
    }

    #[test]
    fn finds_bytes_and_ff() {
        let mut data = vec![0u8; 4000];
        data[1500] = 0xFF;
        data[3000..3004].copy_from_slice(b"SEFH");
        let src = SliceSource(&data);
        let mut c = Cursor::new(&src, 0, 4000, 512);
        assert_eq!(c.find_ff(0, 4000), Some(1500));
        assert_eq!(c.find_ff(1501, 4000), None);
        assert_eq!(c.find_bytes(0, 4000, b"SEFH"), Some(3000));
        assert_eq!(c.find_bytes(0, 3000, b"SEFH"), None);
        assert_eq!(c.find_bytes(2999, 3001, b"SEFH"), Some(3000));
    }

    #[test]
    fn read_error_ends_data() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(1000, 1512));
        let src = MemSource::new(vec![7u8; 4096]).with_unreadable(bad);
        let mut c = Cursor::new(&src, 0, 4096, 4096);
        assert_eq!(c.byte(999), Some(7));
        assert_eq!(c.byte(1000), None);
        assert_eq!(c.limit(), 1000);
        assert_eq!(c.read_error_offset(), Some(1000));
    }
}
