//! Splitting the scan area into chunk reads.
//!
//! The scan area is a [`RangeSet`] (the whole source, or the unallocated
//! ranges). Each [`Window`] owns up to `chunk` bytes of address space: a
//! candidate file start belongs to exactly one window. Its read region runs
//! from the window start (rounded down to a sector) to the window end plus
//! `overlap` (rounded up to a sector, clamped to the source), so every owned
//! candidate has its full probe window and every pattern that starts in the
//! window is complete in the buffer, without a second read.
//!
//! In unallocated mode, ranges separated by less than [`MERGE_GAP`] bytes
//! share a window so a fragmented free-space map does not turn into many
//! tiny reads; the scanner only searches the in-set parts of a window.
//!
//! The planner can jump forward: `next(skip_until)` starts the next window at
//! `skip_until` when that lies ahead of the cursor (the region before it is
//! known to be inside an already-measured file). Skipped bytes still count
//! as covered for progress.

use diskrescue_core::{Range, RangeSet};

/// Unallocated ranges closer than this are read in one window.
pub(crate) const MERGE_GAP: u64 = 1 << 20;

/// One chunk read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    /// Candidate starts in this range (intersected with the scan set) are
    /// owned by this window.
    pub own: Range,
    /// Bytes to read into the buffer.
    pub read: Range,
    /// Bytes of the scan set below `own.end` (progress after this window).
    pub covered_after: u64,
}

/// Produces windows in ascending order.
pub(crate) struct Planner {
    ranges: Vec<Range>,
    idx: usize,
    covered_before_idx: u64,
    cursor: u64,
    chunk: u64,
    overlap: u64,
    sector: u64,
    size: u64,
}

impl Planner {
    /// Plan over `set` (already clipped to `[0, size)`).
    pub(crate) fn new(set: &RangeSet, size: u64, chunk: u64, overlap: u64, sector: u64) -> Self {
        Planner {
            ranges: set.iter().collect(),
            idx: 0,
            covered_before_idx: 0,
            cursor: 0,
            chunk: chunk.max(1),
            overlap,
            sector: sector.max(1),
            size,
        }
    }

    /// The next window at or after `max(cursor, skip_until)`, or `None` when
    /// the scan set is exhausted.
    pub(crate) fn next(&mut self, skip_until: u64) -> Option<Window> {
        let pos = self.cursor.max(skip_until);
        while self.idx < self.ranges.len() && self.ranges[self.idx].end <= pos {
            self.covered_before_idx += self.ranges[self.idx].len();
            self.idx += 1;
        }
        let first = *self.ranges.get(self.idx)?;
        let s = pos.max(first.start);
        let limit = s.saturating_add(self.chunk);
        let mut e = first.end.min(limit);
        let mut j = self.idx;
        while e == self.ranges[j].end && j + 1 < self.ranges.len() {
            let next = self.ranges[j + 1];
            if next.start >= limit || next.start - self.ranges[j].end > MERGE_GAP {
                break;
            }
            j += 1;
            e = next.end.min(limit);
        }
        self.cursor = e;
        let mut covered = self.covered_before_idx;
        for r in &self.ranges[self.idx..=j] {
            covered += r.end.min(e).saturating_sub(r.start);
        }
        let rs = s / self.sector * self.sector;
        let re = e
            .saturating_add(self.overlap)
            .div_ceil(self.sector)
            .saturating_mul(self.sector)
            .min(self.size);
        Some(Window {
            own: Range::new(s, e),
            read: Range::new(rs, re.max(e)),
            covered_after: covered,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_source_chunks_with_overlap() {
        let set = RangeSet::from_range(Range::new(0, 2500));
        let mut p = Planner::new(&set, 2500, 1000, 100, 512);
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(0, 1000));
        assert_eq!(w.read, Range::new(0, 1536));
        assert_eq!(w.covered_after, 1000);
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(1000, 2000));
        assert_eq!(w.read, Range::new(512, 2500));
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(2000, 2500));
        assert_eq!(w.read, Range::new(1536, 2500));
        assert_eq!(w.covered_after, 2500);
        assert!(p.next(0).is_none());
    }

    #[test]
    fn unallocated_ranges_merge_small_gaps_and_skip_large_ones() {
        let set = RangeSet::from_ranges([
            Range::new(4096, 8192),
            Range::new(12_288, 16_384),
            Range::new(10 << 20, (10 << 20) + 4096),
        ]);
        let mut p = Planner::new(&set, 64 << 20, 1 << 20, 0, 512);
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(4096, 16_384));
        assert_eq!(w.covered_after, 8192);
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(10 << 20, (10 << 20) + 4096));
        assert_eq!(w.covered_after, 12_288);
        assert!(p.next(0).is_none());
    }

    #[test]
    fn skip_until_jumps_and_counts_skipped_bytes_as_covered() {
        let set = RangeSet::from_range(Range::new(0, 10_000));
        let mut p = Planner::new(&set, 10_000, 1000, 0, 1);
        p.next(0).unwrap();
        let w = p.next(7_500).unwrap();
        assert_eq!(w.own, Range::new(7_500, 8_500));
        assert_eq!(w.covered_after, 8_500);
        let w = p.next(0).unwrap();
        assert_eq!(w.own, Range::new(8_500, 9_500));
        assert!(p.next(20_000).is_none());
    }

    #[test]
    fn empty_set_has_no_windows() {
        let mut p = Planner::new(&RangeSet::new(), 100, 10, 10, 1);
        assert!(p.next(0).is_none());
    }
}
