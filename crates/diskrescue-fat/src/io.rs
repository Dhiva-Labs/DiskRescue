//! Damage-tolerant reading.
//!
//! Two readers live here:
//!
//! * [`MetaReader`] serves the small random reads of metadata (FAT sectors,
//!   directory clusters, the exFAT bitmap) from a page cache of 64 KiB pages.
//!   A page that cannot be read in one go is re-read sector by sector;
//!   sectors that still fail, sectors beyond the end of the source and
//!   sectors listed in [`Source::unreadable`] are zero-filled and remembered
//!   as bad, so every caller learns exactly which bytes are not real. Unlike
//!   a plain block cache, a page with a bad sector is cached too (with its
//!   bad map), so a damaged FAT sector costs one retry, not one per lookup.
//! * [`DataReader`] streams file contents in 1 MiB chunks straight from the
//!   source (bypassing the page cache). On a read error it narrows down to
//!   the failing sector, zero-fills exactly that sector and continues, so a
//!   file is never abandoned because of a bad block.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use diskrescue_core::source::Source;
use diskrescue_core::{Range, RangeSet, ReadErrorKind, SharedSource};

/// Page size of the metadata cache.
pub(crate) const PAGE_SIZE: u64 = 64 * 1024;
/// Number of pages kept (16 MiB).
const PAGE_CAPACITY: usize = 256;
/// Chunk size for streaming file data.
const DATA_CHUNK: u64 = 1024 * 1024;

/// A cached page: always `PAGE_SIZE` bytes, with the page-relative ranges
/// that are not real data.
struct Page {
    data: Vec<u8>,
    bad: RangeSet,
}

struct PageState {
    pages: HashMap<u64, (Arc<Page>, u64)>,
    tick: u64,
}

/// Page-cached, bad-sector-aware reader for metadata. See the module docs.
pub(crate) struct MetaReader {
    src: SharedSource,
    size: u64,
    known_bad: Option<RangeSet>,
    sector: u64,
    state: Mutex<PageState>,
}

/// Read `buf.len()` bytes at `off` or fail.
fn read_full(src: &dyn Source, off: u64, buf: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let pos = match off.checked_add(done as u64) {
            Some(p) => p,
            None => return false,
        };
        match src.read_at(pos, &mut buf[done..]) {
            Ok(0) | Err(_) => return false,
            Ok(n) => done += n,
        }
    }
    true
}

/// Sector granularity used for fallback reads: the source's sector size if
/// it is a sane power of two, else 512.
fn sector_of(src: &dyn Source) -> u64 {
    let s = src.sector_size() as u64;
    if s.is_power_of_two() && (512..=PAGE_SIZE).contains(&s) {
        s
    } else {
        512
    }
}

impl MetaReader {
    /// Wrap a source.
    pub(crate) fn new(src: SharedSource) -> Self {
        let size = src.size();
        let known_bad = src.unreadable().filter(|s| !s.is_empty());
        let sector = sector_of(&*src);
        MetaReader {
            src,
            size,
            known_bad,
            sector,
            state: Mutex::new(PageState {
                pages: HashMap::new(),
                tick: 0,
            }),
        }
    }

    /// Size of the source in bytes.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    fn load(&self, index: u64) -> Page {
        let page = PAGE_SIZE as usize;
        let start = index.saturating_mul(PAGE_SIZE);
        let mut data = vec![0u8; page];
        let mut bad = RangeSet::new();
        if start >= self.size {
            bad.insert(Range::new(0, PAGE_SIZE));
            return Page { data, bad };
        }
        let avail = (self.size - start).min(PAGE_SIZE) as usize;
        if !read_full(&*self.src, start, &mut data[..avail]) {
            log::debug!("metadata page at {start} unreadable, retrying per sector");
            let sector = self.sector as usize;
            let mut s = 0usize;
            while s < avail {
                let l = sector.min(avail - s);
                if !read_full(&*self.src, start + s as u64, &mut data[s..s + l]) {
                    data[s..s + l].fill(0);
                    bad.insert(Range::new(s as u64, (s + l) as u64));
                    log::warn!("unreadable sector at volume offset {}", start + s as u64);
                }
                s += l;
            }
        }
        if avail < page {
            bad.insert(Range::new(avail as u64, PAGE_SIZE));
        }
        if let Some(kb) = &self.known_bad {
            for r in kb
                .intersection_with_range(Range::new(start, start + PAGE_SIZE))
                .iter()
            {
                let a = (r.start - start) as usize;
                let b = (r.end - start) as usize;
                data[a..b].fill(0);
                bad.insert(Range::new(a as u64, b as u64));
            }
        }
        Page { data, bad }
    }

    fn page(&self, index: u64) -> Arc<Page> {
        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.tick += 1;
            let tick = st.tick;
            if let Some(entry) = st.pages.get_mut(&index) {
                entry.1 = tick;
                return entry.0.clone();
            }
        }
        let page = Arc::new(self.load(index));
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if st.pages.len() >= PAGE_CAPACITY {
            if let Some((&victim, _)) = st.pages.iter().min_by_key(|(_, (_, t))| *t) {
                st.pages.remove(&victim);
            }
        }
        let tick = st.tick;
        st.pages.insert(index, (page.clone(), tick));
        page
    }

    /// Fill `buf` from volume offset `off`. Returns the absolute ranges that
    /// could not be read (zero-filled in `buf`).
    pub(crate) fn read(&self, off: u64, buf: &mut [u8]) -> RangeSet {
        let mut bad = RangeSet::new();
        let len = buf.len();
        let mut done = 0usize;
        while done < len {
            let pos = match off.checked_add(done as u64) {
                Some(p) => p,
                None => {
                    buf[done..].fill(0);
                    bad.insert(Range::new(off.saturating_add(done as u64), u64::MAX));
                    break;
                }
            };
            let index = pos / PAGE_SIZE;
            let within = (pos % PAGE_SIZE) as usize;
            let n = (PAGE_SIZE as usize - within).min(len - done);
            if pos >= self.size {
                buf[done..].fill(0);
                bad.insert(Range::new(pos, pos.saturating_add((len - done) as u64)));
                break;
            }
            let page = self.page(index);
            buf[done..done + n].copy_from_slice(&page.data[within..within + n]);
            if !page.bad.is_empty() {
                let page_start = index * PAGE_SIZE;
                for r in page
                    .bad
                    .intersection_with_range(Range::new(within as u64, (within + n) as u64))
                    .iter()
                {
                    bad.insert(Range::new(page_start + r.start, page_start + r.end));
                }
            }
            done += n;
        }
        bad
    }

    /// Read `len` bytes at `off` into a new vector, with the bad ranges.
    pub(crate) fn read_vec(&self, off: u64, len: usize) -> (Vec<u8>, RangeSet) {
        let mut v = vec![0u8; len];
        let bad = self.read(off, &mut v);
        (v, bad)
    }

    /// Read a small header directly from the source without caching
    /// (orphan scans touch every free cluster once; caching would evict
    /// the FAT). Returns `None` when the bytes are unreadable.
    pub(crate) fn read_uncached(&self, off: u64, len: usize) -> Option<Vec<u8>> {
        if off >= self.size {
            return None;
        }
        let len = len.min((self.size - off) as usize);
        let mut v = vec![0u8; len];
        if !read_full(&*self.src, off, &mut v) {
            return None;
        }
        if let Some(kb) = &self.known_bad {
            if kb.intersects(Range::with_len(off, len as u64)) {
                return None;
            }
        }
        Some(v)
    }
}

/// Zero buffer used to pad sinks.
static ZEROS: [u8; 64 * 1024] = [0u8; 64 * 1024];

/// Write `n` zero bytes to `sink`.
pub(crate) fn write_zeros(sink: &mut dyn Write, mut n: u64) -> std::io::Result<()> {
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        sink.write_all(&ZEROS[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// Streams volume byte ranges into a sink, zero-filling unreadable sectors.
pub(crate) struct DataReader<'a> {
    src: &'a dyn Source,
    size: u64,
    sector: u64,
    align: bool,
    gone: bool,
    buf: Vec<u8>,
    beyond_noted: bool,
}

impl<'a> DataReader<'a> {
    /// New reader over `src`.
    pub(crate) fn new(src: &'a dyn Source) -> Self {
        DataReader {
            src,
            size: src.size(),
            sector: sector_of(src),
            align: src.requires_alignment(),
            gone: false,
            buf: Vec::new(),
            beyond_noted: false,
        }
    }

    fn down(&self, x: u64) -> u64 {
        x - x % self.sector
    }

    fn up(&self, x: u64) -> u64 {
        let r = x % self.sector;
        if r == 0 {
            x
        } else {
            x.saturating_add(self.sector - r)
        }
    }

    /// Copy `len` bytes from volume offset `off` to `sink`. `file_base` is
    /// the file offset that corresponds to `off`; unreadable bytes are added
    /// to `unreadable` relative to the file.
    pub(crate) fn copy(
        &mut self,
        off: u64,
        len: u64,
        file_base: u64,
        sink: &mut dyn Write,
        unreadable: &mut RangeSet,
        notes: &mut Vec<String>,
    ) -> std::io::Result<()> {
        let end = off.saturating_add(len);
        let mut pos = off;
        let mut limit: Option<u64> = None;
        let to_file = |p: u64| file_base + (p - off);
        while pos < end {
            if self.gone || pos >= self.size {
                if pos >= self.size && !self.gone && !self.beyond_noted {
                    notes.push("part of the file lies beyond the end of the image".to_string());
                    self.beyond_noted = true;
                }
                write_zeros(sink, end - pos)?;
                unreadable.insert(Range::new(to_file(pos), to_file(end)));
                break;
            }
            let chunk_end = end
                .min(pos.saturating_add(DATA_CHUNK))
                .min(self.size)
                .min(limit.unwrap_or(u64::MAX));
            let (a, b) = if self.align {
                (self.down(pos), self.up(chunk_end))
            } else {
                (pos, chunk_end)
            };
            let want = (b - a) as usize;
            if self.buf.len() < want {
                self.buf.resize(want, 0);
            }
            match self.src.read_at(a, &mut self.buf[..want]) {
                Ok(n) if a + n as u64 > pos => {
                    let got_end = (a + n as u64).min(chunk_end);
                    let s = (pos - a) as usize;
                    let e = (got_end - a) as usize;
                    sink.write_all(&self.buf[s..e])?;
                    pos = got_end;
                    if limit.is_some_and(|l| pos >= l) {
                        limit = None;
                    }
                }
                Ok(_) => {
                    // The source returned nothing for an offset it claims to
                    // hold: treat the sector as unreadable.
                    let bad_end = end.min(self.down(pos) + self.sector);
                    write_zeros(sink, bad_end - pos)?;
                    unreadable.insert(Range::new(to_file(pos), to_file(bad_end)));
                    pos = bad_end;
                    limit = None;
                }
                Err(e) => {
                    if e.kind == ReadErrorKind::DeviceGone {
                        log::warn!("device disappeared while reading file data: {e}");
                        notes.push("the device disappeared while reading; the rest of the file was zero-filled".to_string());
                        self.gone = true;
                        continue;
                    }
                    let fail = self.down(e.offset.clamp(a, b));
                    if fail > pos && fail < chunk_end {
                        // Re-read the good prefix first.
                        limit = Some(fail);
                    } else {
                        let bad_end = end.min(self.down(pos) + self.sector);
                        log::warn!("unreadable sector at volume offset {}", self.down(pos));
                        write_zeros(sink, bad_end - pos)?;
                        unreadable.insert(Range::new(to_file(pos), to_file(bad_end)));
                        pos = bad_end;
                        limit = None;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    fn src_with_hole(len: usize, hole: Range) -> SharedSource {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8 + 1).collect();
        Arc::new(MemSource::new(data).with_unreadable(RangeSet::from_range(hole)))
    }

    #[test]
    fn meta_reader_zero_fills_bad_sectors() {
        let src = src_with_hole(200_000, Range::new(70_000, 70_100));
        let m = MetaReader::new(src);
        let (v, bad) = m.read_vec(69_000, 2_000);
        assert_eq!(v.len(), 2_000);
        // The whole 512-byte sector containing the hole is bad.
        assert_eq!(bad.as_slice(), &[Range::new(69_632, 70_144)]);
        assert!(v[(69_632 - 69_000)..(70_144 - 69_000)]
            .iter()
            .all(|&b| b == 0));
        assert_ne!(v[0], 0);
        // Beyond the end.
        let (_, bad) = m.read_vec(199_900, 200);
        assert_eq!(bad.as_slice(), &[Range::new(200_000, 200_100)]);
        let (_, bad) = m.read_vec(u64::MAX - 10, 20);
        assert!(!bad.is_empty());
    }

    #[test]
    fn data_reader_reports_holes_relative_to_file() {
        let src = src_with_hole(10_000, Range::new(3_000, 3_010));
        let mut r = DataReader::new(&*src);
        let mut out = Vec::new();
        let mut un = RangeSet::new();
        let mut notes = Vec::new();
        r.copy(1_000, 8_000, 0, &mut out, &mut un, &mut notes)
            .unwrap();
        assert_eq!(out.len(), 8_000);
        // The good prefix of the sector is kept; from the failing byte to
        // the end of its sector (volume 3000..3072) is zero-filled.
        assert_eq!(un.as_slice(), &[Range::new(2_000, 2_072)]);
        assert!(out[2_000..2_072].iter().all(|&b| b == 0));
        assert_eq!(out[1_999], (2_999 % 251) as u8 + 1);
        assert_eq!(out[0], (1_000 % 251) as u8 + 1);
        assert_eq!(out[2_072], (3_072 % 251) as u8 + 1);
        // Past the end of the source.
        let mut out = Vec::new();
        let mut un = RangeSet::new();
        r.copy(9_900, 300, 50, &mut out, &mut un, &mut notes)
            .unwrap();
        assert_eq!(out.len(), 300);
        assert_eq!(un.as_slice(), &[Range::new(150, 350)]);
    }
}
