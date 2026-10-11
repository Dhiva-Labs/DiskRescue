//! Sorted, non-overlapping byte ranges.
//!
//! Used for bad-block maps, filesystem allocation bitmaps, "bytes already
//! attributed to a carved file", and similar bookkeeping. Ranges are
//! half-open: `start..end`.

use std::cmp::Ordering;

/// A half-open byte range `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Range {
    /// First byte (inclusive).
    pub start: u64,
    /// One past the last byte (exclusive).
    pub end: u64,
}

impl Range {
    /// Build a range. `end < start` is normalised to an empty range.
    pub fn new(start: u64, end: u64) -> Self {
        Range {
            start,
            end: end.max(start),
        }
    }

    /// Build a range from a start and a length.
    pub fn with_len(start: u64, len: u64) -> Self {
        Range {
            start,
            end: start.saturating_add(len),
        }
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// True when the range contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// True when `pos` lies inside the range.
    pub fn contains(&self, pos: u64) -> bool {
        pos >= self.start && pos < self.end
    }

    /// True when the two ranges share at least one byte.
    pub fn overlaps(&self, other: Range) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// Intersection, or `None` when disjoint.
    pub fn intersect(&self, other: Range) -> Option<Range> {
        let s = self.start.max(other.start);
        let e = self.end.min(other.end);
        if s < e {
            Some(Range::new(s, e))
        } else {
            None
        }
    }
}

impl PartialOrd for Range {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Range {
    fn cmp(&self, other: &Self) -> Ordering {
        self.start.cmp(&other.start).then(self.end.cmp(&other.end))
    }
}

/// A set of bytes represented as sorted, non-overlapping, non-adjacent ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RangeSet {
    ranges: Vec<Range>,
}

impl RangeSet {
    /// Empty set.
    pub fn new() -> Self {
        RangeSet { ranges: Vec::new() }
    }

    /// Set containing a single range.
    pub fn from_range(r: Range) -> Self {
        let mut s = RangeSet::new();
        s.insert(r);
        s
    }

    /// Build from any iterator of ranges (overlaps are merged).
    pub fn from_ranges<I: IntoIterator<Item = Range>>(iter: I) -> Self {
        let mut s = RangeSet::new();
        for r in iter {
            s.insert(r);
        }
        s
    }

    /// True when no bytes are in the set.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of distinct ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Total number of bytes in the set.
    pub fn total_len(&self) -> u64 {
        self.ranges.iter().map(Range::len).sum()
    }

    /// Iterate over the ranges in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = Range> + '_ {
        self.ranges.iter().copied()
    }

    /// Borrow the ranges as a slice.
    pub fn as_slice(&self) -> &[Range] {
        &self.ranges
    }

    /// Index of the first range whose end is after `pos` (binary search).
    fn first_candidate(&self, pos: u64) -> usize {
        self.ranges.partition_point(|r| r.end <= pos)
    }

    /// Insert a range, merging with neighbours and overlapping ranges.
    pub fn insert(&mut self, r: Range) {
        if r.is_empty() {
            return;
        }
        let mut start = r.start;
        let mut end = r.end;
        // First range that could touch or overlap us (adjacent counts).
        let lo = self.ranges.partition_point(|x| x.end < start);
        let mut hi = lo;
        while hi < self.ranges.len() && self.ranges[hi].start <= end {
            start = start.min(self.ranges[hi].start);
            end = end.max(self.ranges[hi].end);
            hi += 1;
        }
        self.ranges
            .splice(lo..hi, std::iter::once(Range::new(start, end)));
    }

    /// Remove a range (the set difference).
    pub fn remove(&mut self, r: Range) {
        if r.is_empty() || self.ranges.is_empty() {
            return;
        }
        let lo = self.first_candidate(r.start);
        let mut replacement = Vec::new();
        let mut hi = lo;
        while hi < self.ranges.len() && self.ranges[hi].start < r.end {
            let x = self.ranges[hi];
            if x.start < r.start {
                replacement.push(Range::new(x.start, r.start));
            }
            if x.end > r.end {
                replacement.push(Range::new(r.end, x.end));
            }
            hi += 1;
        }
        self.ranges.splice(lo..hi, replacement);
    }

    /// True when `pos` is in the set.
    pub fn contains(&self, pos: u64) -> bool {
        let i = self.first_candidate(pos);
        i < self.ranges.len() && self.ranges[i].contains(pos)
    }

    /// True when any byte of `r` is in the set.
    pub fn intersects(&self, r: Range) -> bool {
        self.first_intersecting(r).is_some()
    }

    /// The first stored range (in ascending order) that overlaps `r`.
    pub fn first_intersecting(&self, r: Range) -> Option<Range> {
        if r.is_empty() {
            return None;
        }
        let i = self.first_candidate(r.start);
        if i < self.ranges.len() && self.ranges[i].overlaps(r) {
            Some(self.ranges[i])
        } else {
            None
        }
    }

    /// True when every byte of `r` is in the set.
    pub fn covers(&self, r: Range) -> bool {
        if r.is_empty() {
            return true;
        }
        let i = self.first_candidate(r.start);
        i < self.ranges.len() && self.ranges[i].start <= r.start && self.ranges[i].end >= r.end
    }

    /// Bytes of this set that fall inside `window`.
    pub fn intersection_with_range(&self, window: Range) -> RangeSet {
        let mut out = RangeSet::new();
        if window.is_empty() {
            return out;
        }
        let mut i = self.first_candidate(window.start);
        while i < self.ranges.len() && self.ranges[i].start < window.end {
            if let Some(x) = self.ranges[i].intersect(window) {
                out.ranges.push(x);
            }
            i += 1;
        }
        out
    }

    /// Bytes present in both sets.
    pub fn intersection(&self, other: &RangeSet) -> RangeSet {
        let mut out = RangeSet::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.ranges.len() && j < other.ranges.len() {
            let (a, b) = (self.ranges[i], other.ranges[j]);
            if let Some(x) = a.intersect(b) {
                out.ranges.push(x);
            }
            if a.end < b.end {
                i += 1;
            } else {
                j += 1;
            }
        }
        out
    }

    /// Bytes present in either set.
    pub fn union(&self, other: &RangeSet) -> RangeSet {
        let mut out = self.clone();
        for r in other.iter() {
            out.insert(r);
        }
        out
    }

    /// Bytes of `window` that are NOT in the set.
    pub fn complement_within(&self, window: Range) -> RangeSet {
        let mut out = RangeSet::new();
        let mut cursor = window.start;
        for r in self.intersection_with_range(window).iter() {
            if r.start > cursor {
                out.ranges.push(Range::new(cursor, r.start));
            }
            cursor = r.end;
        }
        if cursor < window.end {
            out.ranges.push(Range::new(cursor, window.end));
        }
        out
    }

    /// Translate every range down by `delta` (ranges that would go negative
    /// are clipped at zero). Used to express parent-relative ranges inside a
    /// sub-source.
    pub fn shifted_down(&self, delta: u64) -> RangeSet {
        let mut out = RangeSet::new();
        for r in self.iter() {
            if r.end <= delta {
                continue;
            }
            out.ranges
                .push(Range::new(r.start.saturating_sub(delta), r.end - delta));
        }
        out
    }

    /// Translate every range up by `delta`.
    pub fn shifted_up(&self, delta: u64) -> RangeSet {
        RangeSet {
            ranges: self
                .iter()
                .map(|r| Range::new(r.start.saturating_add(delta), r.end.saturating_add(delta)))
                .collect(),
        }
    }

    /// The first range starting at or after `pos`, if any.
    pub fn next_range_from(&self, pos: u64) -> Option<Range> {
        let i = self.first_candidate(pos);
        self.ranges.get(i).copied().map(|r| {
            if r.start < pos {
                Range::new(pos, r.end)
            } else {
                r
            }
        })
    }
}

impl FromIterator<Range> for RangeSet {
    fn from_iter<I: IntoIterator<Item = Range>>(iter: I) -> Self {
        RangeSet::from_ranges(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[(u64, u64)]) -> RangeSet {
        v.iter().map(|&(s, e)| Range::new(s, e)).collect()
    }

    #[test]
    fn insert_merges_overlaps_and_adjacent() {
        let mut s = RangeSet::new();
        s.insert(Range::new(10, 20));
        s.insert(Range::new(30, 40));
        s.insert(Range::new(20, 30)); // adjacent on both sides
        assert_eq!(s.as_slice(), &[Range::new(10, 40)]);
        s.insert(Range::new(5, 12));
        s.insert(Range::new(38, 50));
        assert_eq!(s.as_slice(), &[Range::new(5, 50)]);
        s.insert(Range::new(100, 100)); // empty ignored
        assert_eq!(s.len(), 1);
        s.insert(Range::new(60, 70));
        s.insert(Range::new(0, 1000));
        assert_eq!(s.as_slice(), &[Range::new(0, 1000)]);
    }

    #[test]
    fn remove_splits_and_trims() {
        let mut s = set(&[(0, 100), (200, 300)]);
        s.remove(Range::new(40, 60));
        assert_eq!(
            s.as_slice(),
            &[Range::new(0, 40), Range::new(60, 100), Range::new(200, 300)]
        );
        s.remove(Range::new(90, 250));
        assert_eq!(
            s.as_slice(),
            &[Range::new(0, 40), Range::new(60, 90), Range::new(250, 300)]
        );
        s.remove(Range::new(0, 1000));
        assert!(s.is_empty());
    }

    #[test]
    fn queries() {
        let s = set(&[(10, 20), (30, 40)]);
        assert!(s.contains(10));
        assert!(!s.contains(20));
        assert!(s.contains(39));
        assert!(s.intersects(Range::new(0, 11)));
        assert!(!s.intersects(Range::new(20, 30)));
        assert_eq!(
            s.first_intersecting(Range::new(25, 100)),
            Some(Range::new(30, 40))
        );
        assert!(s.covers(Range::new(12, 18)));
        assert!(!s.covers(Range::new(12, 25)));
        assert_eq!(s.total_len(), 20);
        assert_eq!(s.next_range_from(15), Some(Range::new(15, 20)));
        assert_eq!(s.next_range_from(25), Some(Range::new(30, 40)));
        assert_eq!(s.next_range_from(40), None);
    }

    #[test]
    fn set_algebra() {
        let a = set(&[(0, 50), (100, 150)]);
        let b = set(&[(25, 125)]);
        assert_eq!(
            a.intersection(&b).as_slice(),
            &[Range::new(25, 50), Range::new(100, 125)]
        );
        assert_eq!(a.union(&b).as_slice(), &[Range::new(0, 150)]);
        assert_eq!(
            a.complement_within(Range::new(0, 200)).as_slice(),
            &[Range::new(50, 100), Range::new(150, 200)]
        );
        assert_eq!(
            a.intersection_with_range(Range::new(40, 110)).as_slice(),
            &[Range::new(40, 50), Range::new(100, 110)]
        );
        assert_eq!(
            a.shifted_down(30).as_slice(),
            &[Range::new(0, 20), Range::new(70, 120)]
        );
        assert_eq!(
            a.shifted_up(10).as_slice(),
            &[Range::new(10, 60), Range::new(110, 160)]
        );
    }
}
