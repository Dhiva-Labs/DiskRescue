//! Internal I/O helpers: a windowed reader over a [`Source`], CRC tables and
//! an in-memory source that borrows its bytes.

use diskrescue_core::source::{ReadError, Source};

/// Size of the read window and of the streaming copy/CRC buffers. Every read
/// issued against a source is at most this large (plus a few bytes of
/// look-ahead), far below the 1 MiB budget callers rely on.
pub(crate) const BLOCK: usize = 64 * 1024;

/// Granularity used to find the exact edge of an unreadable region.
const SECTOR: usize = 512;

/// Result of a [`Reader::read`].
pub(crate) struct Fetch {
    /// Bytes placed at the start of the buffer.
    pub got: usize,
    /// Set when the read stopped because the source reported a media error.
    pub err: Option<ReadError>,
}

/// Bounded, windowed reader over a source. Offsets are relative to the file
/// start (`base`); nothing past `avail` bytes is ever requested.
pub(crate) struct Reader<'a> {
    src: &'a dyn Source,
    base: u64,
    /// Readable bytes from `base`: `min(limit, source size - base)`.
    pub avail: u64,
    /// True when `limit` cut the file short of the end of the source.
    pub limited: bool,
    win: Vec<u8>,
    win_start: u64,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(src: &'a dyn Source, base: u64, limit: u64) -> Self {
        let size = src.size().saturating_sub(base);
        let avail = size.min(limit);
        Reader {
            src,
            base,
            avail,
            limited: limit < size,
            win: Vec::new(),
            win_start: 0,
        }
    }

    /// Fill the window so that it contains `pos`. Returns an error only when
    /// not even the first sector at `pos` can be read.
    fn refill(&mut self, pos: u64) -> Result<(), ReadError> {
        let want = (self.avail - pos).min(BLOCK as u64) as usize;
        self.win.clear();
        self.win.resize(want, 0);
        self.win_start = pos;
        match self.src.read_at(self.base + pos, &mut self.win) {
            Ok(n) => {
                self.win.truncate(n);
                if n == 0 {
                    // The source is shorter than it claimed.
                    self.avail = pos;
                }
                Ok(())
            }
            Err(first) => {
                // Find the exact edge of the bad region sector by sector so
                // the good prefix is still usable.
                let mut done = 0usize;
                while done < want {
                    let step = SECTOR.min(want - done);
                    match self.src.read_at(
                        self.base + pos + done as u64,
                        &mut self.win[done..done + step],
                    ) {
                        Ok(0) => break,
                        Ok(n) => {
                            done += n;
                            if n < step {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                self.win.truncate(done);
                if done == 0 {
                    let mut e = first;
                    e.offset = e.offset.saturating_sub(self.base).max(pos);
                    Err(e)
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Read into `buf` starting at file offset `pos`. Stops early at the end
    /// of the available data or at the first unreadable sector.
    pub(crate) fn read(&mut self, pos: u64, buf: &mut [u8]) -> Fetch {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = pos + done as u64;
            if cur >= self.avail {
                break;
            }
            let in_win = cur >= self.win_start && cur < self.win_start + self.win.len() as u64;
            if !in_win {
                if let Err(e) = self.refill(cur) {
                    return Fetch {
                        got: done,
                        err: Some(e),
                    };
                }
                if self.win.is_empty() {
                    break;
                }
            }
            let off = (cur - self.win_start) as usize;
            let n = (self.win.len() - off).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&self.win[off..off + n]);
            done += n;
        }
        Fetch {
            got: done,
            err: None,
        }
    }

    /// Read exactly `N` bytes or report why not.
    pub(crate) fn read_array<const N: usize>(
        &mut self,
        pos: u64,
    ) -> Result<Option<[u8; N]>, ReadError> {
        let mut b = [0u8; N];
        let f = self.read(pos, &mut b);
        if let Some(e) = f.err {
            return Err(e);
        }
        Ok(if f.got == N { Some(b) } else { None })
    }
}

const fn make_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = make_crc_table();

/// Initial state of a byte-at-a-time CRC-32.
pub(crate) const CRC_INIT: u32 = 0xFFFF_FFFF;

/// Advance a raw CRC state by one byte.
#[inline]
pub(crate) fn crc_step(state: u32, b: u8) -> u32 {
    CRC_TABLE[((state ^ b as u32) & 0xFF) as usize] ^ (state >> 8)
}

/// Advance a raw CRC state (not yet inverted) by `data`.
#[inline]
pub(crate) fn crc_update(mut state: u32, data: &[u8]) -> u32 {
    for &b in data {
        state = CRC_TABLE[((state ^ b as u32) & 0xFF) as usize] ^ (state >> 8);
    }
    state
}

/// CRC-32 of `kind` followed by `data`, as stored in a PNG chunk.
pub(crate) fn chunk_crc(kind: &[u8; 4], data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(kind);
    h.update(data);
    h.finalize()
}

/// A source over a borrowed byte slice (no copy), used by the in-memory
/// decoder API.
pub(crate) struct SliceSource<'a>(pub &'a [u8]);

impl Source for SliceSource<'_> {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.0.len() as u64 {
            return Ok(0);
        }
        let start = offset as usize;
        let n = (self.0.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "memory".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::ranges::{Range, RangeSet};
    use diskrescue_core::source::MemSource;

    #[test]
    fn byte_crc_matches_crc32fast() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut h = crc32fast::Hasher::new();
        h.update(&data);
        assert_eq!(!crc_update(CRC_INIT, &data), h.finalize());
        assert_eq!(!crc_update(CRC_INIT, b"IEND"), 0xAE42_6082);
        assert_eq!(chunk_crc(b"IEND", &[]), 0xAE42_6082);
    }

    #[test]
    fn reader_serves_across_windows() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let src = MemSource::new(data.clone());
        let mut r = Reader::new(&src, 0, u64::MAX);
        let mut buf = vec![0u8; 150_000];
        let f = r.read(10, &mut buf);
        assert_eq!(f.got, 150_000);
        assert!(f.err.is_none());
        assert_eq!(buf, data[10..150_010]);
        // Past the end.
        let f = r.read(199_990, &mut buf);
        assert_eq!(f.got, 10);
        let f = r.read(200_000, &mut buf);
        assert_eq!(f.got, 0);
    }

    #[test]
    fn reader_honours_limit_and_base() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        let src = MemSource::new(data.clone());
        let mut r = Reader::new(&src, 100, 50);
        assert_eq!(r.avail, 50);
        assert!(r.limited);
        let mut buf = [0u8; 100];
        let f = r.read(0, &mut buf);
        assert_eq!(f.got, 50);
        assert_eq!(&buf[..50], &data[100..150]);
    }

    #[test]
    fn reader_reports_unreadable_edge() {
        let data = vec![7u8; 10_000];
        let mut bad = RangeSet::new();
        bad.insert(Range::new(5120, 6144));
        let src = MemSource::new(data).with_unreadable(bad);
        let mut r = Reader::new(&src, 0, u64::MAX);
        let mut buf = vec![0u8; 8000];
        let f = r.read(0, &mut buf);
        assert_eq!(f.got, 5120);
        assert!(f.err.is_some());
        let f = r.read(6144, &mut buf);
        assert_eq!(f.got, 3856);
        assert!(f.err.is_none());
    }
}
