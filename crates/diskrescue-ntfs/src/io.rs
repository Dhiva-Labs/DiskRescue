//! Damage-tolerant positional reads over the volume.
//!
//! [`Disk::read`] fills a buffer from the source and never fails because of
//! bad media: unreadable bytes (read errors, bytes past the end of a
//! truncated image, ranges the source's bad-block map lists) are zero-filled
//! and returned as a [`RangeSet`] of absolute offsets. Only a vanished
//! device ([`ReadErrorKind::DeviceGone`]) is reported as an error, because
//! continuing would turn the whole volume into zeros.
//!
//! On a failed bulk read the reader shortens the request to the reported
//! failing offset, and when a read fails at its very first byte it marks
//! one fallback unit (512 bytes or the source's sector size) bad and moves
//! on, so a single bad sector costs one unit and not the whole request.
//!
//! Metadata reads (MFT records, index blocks, bitmaps) go through a
//! [`BlockCache`]; file content is read directly so it does not evict
//! metadata. Sources that require sector-aligned reads get aligned bounce
//! buffers.

use diskrescue_core::source::{BlockCache, ReadErrorKind, Source};
use diskrescue_core::{Range, RangeSet, SharedSource};

use crate::Result;

/// Block size of the metadata cache.
const CACHE_BLOCK: usize = 64 * 1024;
/// Number of cached blocks (16 MiB).
const CACHE_BLOCKS: usize = 256;

/// Volume reader with bad-range bookkeeping.
pub(crate) struct Disk {
    src: SharedSource,
    cache: BlockCache,
    size: u64,
    unit: u64,
    align: Option<u64>,
    known_bad: Option<RangeSet>,
}

impl Disk {
    /// Wrap a source.
    pub fn new(src: SharedSource) -> Self {
        let sector = src.sector_size().max(1) as u64;
        let align = src.requires_alignment().then_some(sector);
        let known_bad = src.unreadable().filter(|s| !s.is_empty());
        Disk {
            cache: BlockCache::new(src.clone(), CACHE_BLOCK, CACHE_BLOCKS),
            size: src.size(),
            unit: sector.max(512),
            align,
            known_bad,
            src,
        }
    }

    /// Source size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Fill `buf` with the bytes at `offset`. Unreadable bytes are zeroed
    /// and returned as absolute ranges. `cached` routes the read through the
    /// metadata cache.
    pub fn read(&self, offset: u64, buf: &mut [u8], cached: bool) -> Result<RangeSet> {
        let mut bad = RangeSet::new();
        if buf.is_empty() {
            return Ok(bad);
        }
        let len = buf.len() as u64;
        let Some(end) = offset.checked_add(len) else {
            buf.fill(0);
            bad.insert(Range::new(offset, u64::MAX));
            return Ok(bad);
        };
        let avail_end = end.min(self.size).max(offset);
        if avail_end < end {
            let from = (avail_end - offset) as usize;
            buf[from..].fill(0);
            bad.insert(Range::new(avail_end, end));
        }
        let mut pos = offset;
        let mut limit = avail_end;
        while pos < avail_end {
            if limit <= pos {
                limit = avail_end;
            }
            let a = (pos - offset) as usize;
            let b = (limit - offset) as usize;
            match self.read_some(pos, &mut buf[a..b], cached) {
                Ok(n) if n > 0 => {
                    pos += n as u64;
                    if pos >= limit {
                        limit = avail_end;
                    }
                }
                Ok(_) => {
                    pos = self.mark_bad(offset, buf, pos, avail_end, &mut bad);
                    limit = avail_end;
                }
                Err(e) if e.kind == ReadErrorKind::DeviceGone => return Err(e.into()),
                Err(e) => {
                    if e.offset > pos && e.offset < limit {
                        // Retry the good prefix on its own.
                        limit = e.offset;
                    } else {
                        log::debug!("read error at {pos}: {e}");
                        pos = self.mark_bad(offset, buf, pos, avail_end, &mut bad);
                        limit = avail_end;
                    }
                }
            }
        }
        if let Some(known) = &self.known_bad {
            for r in known
                .intersection_with_range(Range::new(offset, end))
                .iter()
            {
                let a = (r.start - offset) as usize;
                let b = (r.end - offset) as usize;
                buf[a..b].fill(0);
                bad.insert(r);
            }
        }
        Ok(bad)
    }

    fn mark_bad(
        &self,
        offset: u64,
        buf: &mut [u8],
        pos: u64,
        avail_end: u64,
        bad: &mut RangeSet,
    ) -> u64 {
        let next = ((pos / self.unit) + 1)
            .saturating_mul(self.unit)
            .min(avail_end);
        let a = (pos - offset) as usize;
        let b = (next - offset) as usize;
        buf[a..b].fill(0);
        bad.insert(Range::new(pos, next));
        next
    }

    fn read_some(
        &self,
        pos: u64,
        buf: &mut [u8],
        cached: bool,
    ) -> std::result::Result<usize, diskrescue_core::ReadError> {
        if cached {
            match self.cache.read_at(pos, buf) {
                Ok(n) if n > 0 => return Ok(n),
                Err(e) if e.kind == ReadErrorKind::DeviceGone => return Err(e),
                // A short block cached in front of a bad sector, or a failed
                // block: fall back to a direct read of just this range.
                _ => {}
            }
        }
        self.direct(pos, buf)
    }

    fn direct(
        &self,
        pos: u64,
        buf: &mut [u8],
    ) -> std::result::Result<usize, diskrescue_core::ReadError> {
        let Some(a) = self.align else {
            return self.src.read_at(pos, buf);
        };
        let start = pos / a * a;
        let end = (pos + buf.len() as u64).div_ceil(a) * a;
        let mut tmp = vec![0u8; (end - start) as usize];
        let n = self.src.read_at(start, &mut tmp)? as u64;
        let skip = pos - start;
        if n <= skip {
            return Ok(0);
        }
        let k = ((n - skip) as usize).min(buf.len());
        buf[..k].copy_from_slice(&tmp[skip as usize..skip as usize + k]);
        Ok(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn clean_reads_and_past_end() {
        let d = Disk::new(Arc::new(MemSource::new(data(10_000))));
        let mut buf = vec![0u8; 100];
        let bad = d.read(50, &mut buf, false).unwrap();
        assert!(bad.is_empty());
        assert_eq!(buf, data(10_000)[50..150].to_vec());
        let mut buf = vec![0xFFu8; 100];
        let bad = d.read(9_950, &mut buf, true).unwrap();
        assert_eq!(bad.as_slice(), &[Range::new(10_000, 10_050)]);
        assert!(buf[50..].iter().all(|&b| b == 0));
        let mut buf = vec![1u8; 10];
        let bad = d.read(u64::MAX - 3, &mut buf, false).unwrap();
        assert!(!bad.is_empty());
    }

    /// A source that, like a direct-I/O device, rejects unaligned reads.
    struct Aligned(Vec<u8>);

    impl Source for Aligned {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn sector_size(&self) -> u32 {
            4096
        }
        fn requires_alignment(&self) -> bool {
            true
        }
        fn read_at(
            &self,
            offset: u64,
            buf: &mut [u8],
        ) -> std::result::Result<usize, diskrescue_core::ReadError> {
            if offset % 4096 != 0 || buf.len() % 4096 != 0 {
                return Err(diskrescue_core::ReadError::unaligned(
                    offset,
                    buf.len(),
                    4096,
                ));
            }
            let start = offset.min(self.size()) as usize;
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
        fn describe(&self) -> String {
            "aligned".into()
        }
    }

    #[test]
    fn unaligned_requests_on_aligned_sources() {
        let d = Disk::new(Arc::new(Aligned(data(64 * 1024))));
        let expect = data(64 * 1024);
        for (off, len) in [(1u64, 10usize), (4095, 2), (5000, 9000), (60_000, 5536)] {
            for cached in [false, true] {
                let mut buf = vec![0u8; len];
                let bad = d.read(off, &mut buf, cached).unwrap();
                assert!(bad.is_empty(), "{off}+{len}");
                assert_eq!(buf, expect[off as usize..off as usize + len].to_vec());
            }
        }
    }

    #[test]
    fn bad_sector_costs_one_unit() {
        let mut set = RangeSet::new();
        set.insert(Range::new(70_000, 70_100));
        let src = MemSource::new(data(200_000)).with_unreadable(set);
        let d = Disk::new(Arc::new(src));
        for cached in [false, true] {
            let mut buf = vec![0xEEu8; 100_000];
            let bad = d.read(20_000, &mut buf, cached).unwrap();
            let ranges: Vec<Range> = bad.iter().collect();
            assert_eq!(ranges.len(), 1, "{ranges:?}");
            let r = ranges[0];
            assert!(r.start <= 70_000 && r.end >= 70_100, "{r:?}");
            assert!(r.len() <= 1024, "{r:?}");
            let expect = data(200_000);
            for (i, &b) in buf.iter().enumerate() {
                let abs = 20_000 + i as u64;
                if r.contains(abs) {
                    assert_eq!(b, 0);
                } else {
                    assert_eq!(b, expect[abs as usize], "byte {abs}");
                }
            }
            // A read entirely after the bad sector inside the same cache
            // block must still succeed.
            let mut buf = vec![0u8; 512];
            let bad = d.read(71_000, &mut buf, cached).unwrap();
            assert!(bad.is_empty());
            assert_eq!(buf, expect[71_000..71_512].to_vec());
        }
    }
}
