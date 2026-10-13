//! The block status map: what is known about every byte of the source.
//!
//! The map is a sorted `Vec` of [`MapBlock`]s that are contiguous, never
//! overlap, never have zero size, and together cover exactly `[0, size)`.
//! Adjacent blocks always have different statuses (they are merged on every
//! change), so a maximal run of bad sectors is exactly one block. This is the
//! same representation GNU ddrescue uses (`Mapfile::sblock_vector`); lookups
//! are binary searches and a status change splices at most a few entries.
//!
//! Per-status byte totals and the number of bad areas are maintained
//! incrementally so [`StatusMap::stats`] is O(1) even for maps with a
//! million blocks.
//!
//! Limits: positions are `u64` but the mapfile format caps them at
//! `i64::MAX` (see [`crate::mapfile`]); the map itself does not care.

use diskrescue_core::{Error, Range, RangeSet, Result};

/// Status of a byte range, with ddrescue's characters (`? * / - +`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockStatus {
    /// Never read (`?`).
    NonTried,
    /// A large read failed; the edges have not been probed sector by sector (`*`).
    NonTrimmed,
    /// Edges trimmed; the interior has not been read sector by sector (`/`).
    NonScraped,
    /// A single-sector read failed (`-`).
    BadSector,
    /// Read successfully and written to the output (`+`).
    Finished,
}

impl BlockStatus {
    /// Every status, in ddrescue's "processed state" order.
    pub const ALL: [BlockStatus; 5] = [
        BlockStatus::NonTried,
        BlockStatus::NonTrimmed,
        BlockStatus::NonScraped,
        BlockStatus::BadSector,
        BlockStatus::Finished,
    ];

    /// Every status except [`BlockStatus::Finished`].
    pub const UNFINISHED: [BlockStatus; 4] = [
        BlockStatus::NonTried,
        BlockStatus::NonTrimmed,
        BlockStatus::NonScraped,
        BlockStatus::BadSector,
    ];

    /// The ddrescue mapfile character.
    pub fn as_char(self) -> char {
        match self {
            BlockStatus::NonTried => '?',
            BlockStatus::NonTrimmed => '*',
            BlockStatus::NonScraped => '/',
            BlockStatus::BadSector => '-',
            BlockStatus::Finished => '+',
        }
    }

    /// Parse a ddrescue mapfile character.
    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => BlockStatus::NonTried,
            '*' => BlockStatus::NonTrimmed,
            '/' => BlockStatus::NonScraped,
            '-' => BlockStatus::BadSector,
            '+' => BlockStatus::Finished,
            _ => return None,
        })
    }

    /// ddrescue's "processed state" rank: 0 (non-tried) .. 4 (finished).
    pub fn rank(self) -> u8 {
        match self {
            BlockStatus::NonTried => 0,
            BlockStatus::NonTrimmed => 1,
            BlockStatus::NonScraped => 2,
            BlockStatus::BadSector => 3,
            BlockStatus::Finished => 4,
        }
    }

    /// How alarming the status is when several share one display cell:
    /// bad sector > non-scraped > non-trimmed > non-tried > finished.
    pub fn severity(self) -> u8 {
        match self {
            BlockStatus::Finished => 0,
            BlockStatus::NonTried => 1,
            BlockStatus::NonTrimmed => 2,
            BlockStatus::NonScraped => 3,
            BlockStatus::BadSector => 4,
        }
    }

    /// Human-readable name as ddrescue prints it.
    pub fn name(self) -> &'static str {
        match self {
            BlockStatus::NonTried => "non-tried",
            BlockStatus::NonTrimmed => "non-trimmed",
            BlockStatus::NonScraped => "non-scraped",
            BlockStatus::BadSector => "bad-sector",
            BlockStatus::Finished => "finished",
        }
    }

    fn index(self) -> usize {
        self.rank() as usize
    }
}

impl std::fmt::Display for BlockStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One entry of the map: `size` bytes starting at `pos` share `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MapBlock {
    /// First byte.
    pub pos: u64,
    /// Length in bytes (never 0 inside a [`StatusMap`]).
    pub size: u64,
    /// Status of every byte in the block.
    pub status: BlockStatus,
}

impl MapBlock {
    /// One past the last byte.
    pub fn end(&self) -> u64 {
        self.pos + self.size
    }

    /// The block as a half-open range.
    pub fn range(&self) -> Range {
        Range::new(self.pos, self.end())
    }
}

/// Direction of a sweep over the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    /// Towards higher offsets.
    Forward,
    /// Towards lower offsets.
    Backward,
}

impl Direction {
    /// True for [`Direction::Forward`].
    pub fn is_forward(self) -> bool {
        self == Direction::Forward
    }

    /// The opposite direction.
    pub fn reversed(self) -> Self {
        match self {
            Direction::Forward => Direction::Backward,
            Direction::Backward => Direction::Forward,
        }
    }
}

/// Byte totals per status, as ddrescue reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    /// Bytes marked finished.
    pub rescued: u64,
    /// Bytes never read.
    pub non_tried: u64,
    /// Bytes in failed large reads whose edges were not trimmed yet.
    pub non_trimmed: u64,
    /// Bytes trimmed but not scraped yet.
    pub non_scraped: u64,
    /// Bytes in bad sectors.
    pub bad_sectors: u64,
    /// Number of maximal runs of bad sectors.
    pub bad_areas: u64,
    /// Bytes in failed reads: `non_trimmed + non_scraped + bad_sectors`.
    pub errsize: u64,
}

impl Stats {
    /// Total bytes covered (all statuses).
    pub fn total(&self) -> u64 {
        self.rescued + self.non_tried + self.non_trimmed + self.non_scraped + self.bad_sectors
    }

    /// Bytes that still need work (anything but finished and bad sectors).
    pub fn pending(&self) -> u64 {
        self.non_tried + self.non_trimmed + self.non_scraped
    }

    /// Fraction of bytes rescued, 0.0 ..= 1.0 (1.0 for an empty map).
    pub fn rescued_fraction(&self) -> f64 {
        let t = self.total();
        if t == 0 {
            1.0
        } else {
            self.rescued as f64 / t as f64
        }
    }

    /// Bytes with the given status.
    pub fn bytes(&self, status: BlockStatus) -> u64 {
        match status {
            BlockStatus::NonTried => self.non_tried,
            BlockStatus::NonTrimmed => self.non_trimmed,
            BlockStatus::NonScraped => self.non_scraped,
            BlockStatus::BadSector => self.bad_sectors,
            BlockStatus::Finished => self.rescued,
        }
    }

    fn from_sizes(sizes: &[u64; 5], bad_areas: u64) -> Self {
        Stats {
            rescued: sizes[4],
            non_tried: sizes[0],
            non_trimmed: sizes[1],
            non_scraped: sizes[2],
            bad_sectors: sizes[3],
            bad_areas,
            errsize: sizes[1] + sizes[2] + sizes[3],
        }
    }
}

/// What [`StatusMap::resize`] may do when the map shrinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePolicy {
    /// Grow with non-tried bytes; when shrinking, refuse (error) if any
    /// finished byte would be dropped. This is ddrescue's rule ("Rescued
    /// data in mapfile goes past end of input file").
    KeepFinished,
    /// Grow or shrink unconditionally, dropping whatever lies beyond.
    Truncate,
}

/// Contiguous, sorted, merged status blocks covering exactly `[0, size)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMap {
    blocks: Vec<MapBlock>,
    size: u64,
    sizes: [u64; 5],
    bad_blocks: u64,
}

impl Default for StatusMap {
    fn default() -> Self {
        StatusMap::new(0)
    }
}

impl StatusMap {
    /// A map of `size` bytes, all non-tried.
    pub fn new(size: u64) -> Self {
        let mut m = StatusMap {
            blocks: Vec::new(),
            size,
            sizes: [0; 5],
            bad_blocks: 0,
        };
        if size > 0 {
            m.blocks.push(MapBlock {
                pos: 0,
                size,
                status: BlockStatus::NonTried,
            });
            m.sizes[BlockStatus::NonTried.index()] = size;
        }
        m
    }

    /// Build from blocks that are already known to be contiguous from 0
    /// with non-zero sizes (the mapfile parser checks that). Equal
    /// neighbours are merged.
    pub(crate) fn from_contiguous(blocks: Vec<MapBlock>) -> Self {
        let mut merged: Vec<MapBlock> = Vec::with_capacity(blocks.len());
        for b in blocks {
            if b.size == 0 {
                continue;
            }
            match merged.last_mut() {
                Some(last) if last.status == b.status && last.end() == b.pos => last.size += b.size,
                _ => merged.push(b),
            }
        }
        let size = merged.last().map(|b| b.end()).unwrap_or(0);
        let mut m = StatusMap {
            blocks: merged,
            size,
            sizes: [0; 5],
            bad_blocks: 0,
        };
        m.recount();
        m
    }

    fn recount(&mut self) {
        self.sizes = [0; 5];
        self.bad_blocks = 0;
        for b in &self.blocks {
            self.sizes[b.status.index()] += b.size;
            if b.status == BlockStatus::BadSector {
                self.bad_blocks += 1;
            }
        }
    }

    /// Total size covered by the map.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// True when the map covers no bytes.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Number of blocks.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// The blocks, in ascending order.
    pub fn blocks(&self) -> &[MapBlock] {
        &self.blocks
    }

    /// Iterate over the blocks in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = MapBlock> + '_ {
        self.blocks.iter().copied()
    }

    /// Index of the block containing `pos`, if `pos < size`.
    pub fn index_of(&self, pos: u64) -> Option<usize> {
        if pos >= self.size {
            return None;
        }
        let i = self.blocks.partition_point(|b| b.end() <= pos);
        (i < self.blocks.len()).then_some(i)
    }

    /// Status of the byte at `pos`, or `None` past the end.
    pub fn get(&self, pos: u64) -> Option<BlockStatus> {
        self.index_of(pos).map(|i| self.blocks[i].status)
    }

    /// The whole block containing `pos`.
    pub fn block_at(&self, pos: u64) -> Option<MapBlock> {
        self.index_of(pos).map(|i| self.blocks[i])
    }

    /// Set the status of every byte in `range` (clamped to the map) and
    /// merge with equal neighbours.
    pub fn set(&mut self, range: Range, status: BlockStatus) {
        let start = range.start.min(self.size);
        let end = range.end.min(self.size);
        if start >= end {
            return;
        }
        let (Some(i), Some(j)) = (self.index_of(start), self.index_of(end - 1)) else {
            return;
        };
        if i == j && self.blocks[i].status == status {
            return;
        }
        let first = self.blocks[i];
        let last = self.blocks[j];
        let (mut lo, mut hi) = (i, j);
        let (mut new_start, mut new_end) = (start, end);
        let mut left_piece = None;
        if first.pos < start {
            if first.status == status {
                new_start = first.pos;
            } else {
                left_piece = Some(MapBlock {
                    pos: first.pos,
                    size: start - first.pos,
                    status: first.status,
                });
            }
        } else if i > 0 && self.blocks[i - 1].status == status {
            lo = i - 1;
            new_start = self.blocks[i - 1].pos;
        }
        let mut right_piece = None;
        if last.end() > end {
            if last.status == status {
                new_end = last.end();
            } else {
                right_piece = Some(MapBlock {
                    pos: end,
                    size: last.end() - end,
                    status: last.status,
                });
            }
        } else if j + 1 < self.blocks.len() && self.blocks[j + 1].status == status {
            hi = j + 1;
            new_end = self.blocks[j + 1].end();
        }
        for b in &self.blocks[lo..=hi] {
            self.sizes[b.status.index()] -= b.size;
            if b.status == BlockStatus::BadSector {
                self.bad_blocks -= 1;
            }
        }
        let mut repl: Vec<MapBlock> = Vec::with_capacity(3);
        repl.extend(left_piece);
        repl.push(MapBlock {
            pos: new_start,
            size: new_end - new_start,
            status,
        });
        repl.extend(right_piece);
        for b in &repl {
            self.sizes[b.status.index()] += b.size;
            if b.status == BlockStatus::BadSector {
                self.bad_blocks += 1;
            }
        }
        self.blocks.splice(lo..=hi, repl);
    }

    /// Like [`StatusMap::set`], but only bytes whose current status is in
    /// `from` change. Used where a status must never be overwritten (for
    /// example: never demote finished data to a failure status).
    pub fn set_where(&mut self, range: Range, from: &[BlockStatus], to: BlockStatus) {
        let start = range.start.min(self.size);
        let end = range.end.min(self.size);
        if start >= end {
            return;
        }
        let mut targets: Vec<Range> = Vec::new();
        if let Some(mut i) = self.index_of(start) {
            while i < self.blocks.len() && self.blocks[i].pos < end {
                let b = self.blocks[i];
                if from.contains(&b.status) {
                    if let Some(r) = b.range().intersect(Range::new(start, end)) {
                        targets.push(r);
                    }
                }
                i += 1;
            }
        }
        for r in targets {
            self.set(r, to);
        }
    }

    /// Statistics for the whole map (O(1)).
    pub fn stats(&self) -> Stats {
        Stats::from_sizes(&self.sizes, self.bad_blocks)
    }

    /// Statistics for the bytes inside `window` (O(blocks in window)).
    /// A bad area that straddles the window edge counts once.
    pub fn stats_in(&self, window: Range) -> Stats {
        let mut sizes = [0u64; 5];
        let mut bad = 0u64;
        let start = window.start.min(self.size);
        let end = window.end.min(self.size);
        if start < end {
            if let Some(mut i) = self.index_of(start) {
                while i < self.blocks.len() && self.blocks[i].pos < end {
                    let b = self.blocks[i];
                    if let Some(r) = b.range().intersect(Range::new(start, end)) {
                        sizes[b.status.index()] += r.len();
                        if b.status == BlockStatus::BadSector {
                            bad += 1;
                        }
                    }
                    i += 1;
                }
            }
        }
        Stats::from_sizes(&sizes, bad)
    }

    /// The first (forward) or last (backward) stretch with `status`,
    /// starting the search at `from` (forward: bytes `>= from`; backward:
    /// bytes `< from`). The result is cropped at `from`.
    pub fn find_next(&self, from: u64, status: BlockStatus, direction: Direction) -> Option<Range> {
        self.find_next_any(from, &[status], direction)
            .map(|b| b.range())
    }

    /// Like [`StatusMap::find_next`] for any of `statuses`; returns the
    /// cropped block with its status.
    pub fn find_next_any(
        &self,
        from: u64,
        statuses: &[BlockStatus],
        direction: Direction,
    ) -> Option<MapBlock> {
        let i = self.find_index_any(from, statuses, direction)?;
        let b = self.blocks[i];
        Some(match direction {
            Direction::Forward => {
                let pos = b.pos.max(from);
                MapBlock {
                    pos,
                    size: b.end() - pos,
                    status: b.status,
                }
            }
            Direction::Backward => {
                let end = b.end().min(from);
                MapBlock {
                    pos: b.pos,
                    size: end - b.pos,
                    status: b.status,
                }
            }
        })
    }

    /// Index of the block [`StatusMap::find_next_any`] would return.
    pub(crate) fn find_index_any(
        &self,
        from: u64,
        statuses: &[BlockStatus],
        direction: Direction,
    ) -> Option<usize> {
        match direction {
            Direction::Forward => {
                let mut i = self.index_of(from)?;
                while i < self.blocks.len() {
                    if statuses.contains(&self.blocks[i].status) {
                        return Some(i);
                    }
                    i += 1;
                }
                None
            }
            Direction::Backward => {
                if from == 0 || self.blocks.is_empty() {
                    return None;
                }
                let mut i = self.index_of(from.min(self.size) - 1)?;
                loop {
                    if statuses.contains(&self.blocks[i].status) {
                        return Some(i);
                    }
                    if i == 0 {
                        return None;
                    }
                    i -= 1;
                }
            }
        }
    }

    /// Every byte whose status is in `statuses`.
    pub fn ranges_with(&self, statuses: &[BlockStatus]) -> RangeSet {
        self.blocks
            .iter()
            .filter(|b| statuses.contains(&b.status))
            .map(|b| b.range())
            .collect()
    }

    /// Every byte marked [`BlockStatus::BadSector`].
    pub fn bad_ranges(&self) -> RangeSet {
        self.ranges_with(&[BlockStatus::BadSector])
    }

    /// Every byte that is not [`BlockStatus::Finished`]: the bytes an image
    /// made from this map does not hold rescued data for.
    pub fn unfinished_ranges(&self) -> RangeSet {
        self.ranges_with(&BlockStatus::UNFINISHED)
    }

    /// Change the covered size. Growing appends non-tried bytes; shrinking
    /// follows `policy`.
    pub fn resize(&mut self, new_size: u64, policy: ResizePolicy) -> Result<()> {
        use std::cmp::Ordering;
        match new_size.cmp(&self.size) {
            Ordering::Equal => {}
            Ordering::Greater => {
                let add = new_size - self.size;
                match self.blocks.last_mut() {
                    Some(last) if last.status == BlockStatus::NonTried => last.size += add,
                    _ => self.blocks.push(MapBlock {
                        pos: self.size,
                        size: add,
                        status: BlockStatus::NonTried,
                    }),
                }
                self.size = new_size;
                self.recount();
            }
            Ordering::Less => {
                if policy == ResizePolicy::KeepFinished {
                    if let Some(i) = self.index_of(new_size) {
                        if self.blocks[i..]
                            .iter()
                            .any(|b| b.status == BlockStatus::Finished)
                        {
                            return Err(Error::InvalidArgument(format!(
                                "rescued data in the map goes past {new_size} bytes; refusing to \
                                 shrink the map from {} bytes",
                                self.size
                            )));
                        }
                    }
                }
                match self.index_of(new_size) {
                    Some(i) => {
                        self.blocks.truncate(i + 1);
                        let last = &mut self.blocks[i];
                        if last.pos >= new_size {
                            self.blocks.pop();
                        } else {
                            last.size = new_size - last.pos;
                        }
                    }
                    None => self.blocks.clear(),
                }
                self.size = new_size;
                self.recount();
            }
        }
        Ok(())
    }

    /// ddrescue's `join_subsectors`: make every block boundary a multiple of
    /// `sector` unless one of the two neighbours is finished. A partial
    /// sector is moved into the less processed neighbour, so a status can
    /// only be demoted (re-read later), never promoted, and finished data is
    /// never touched. Returns the number of boundaries fixed.
    pub fn join_subsectors(&mut self, sector: u64) -> usize {
        if sector <= 1 || self.blocks.len() < 2 {
            return 0;
        }
        // One pass over a stack, so a map of a badly damaged disk (hundreds
        // of thousands of blocks) is fixed in linear time. After each block
        // is pushed, the boundary below the top is fixed until it is
        // aligned or next to finished data. A fix either aligns that
        // boundary or removes a block, and a removal can only expose the
        // boundary below, which is then fixed the same way. This makes
        // exactly the changes ddrescue's left-to-right walk with one step
        // back makes (the tests compare the two on random maps).
        let mut out: Vec<MapBlock> = Vec::with_capacity(self.blocks.len());
        let mut fixed = 0usize;
        for &b in &self.blocks {
            match out.last_mut() {
                Some(top) if top.status == b.status => top.size += b.size,
                _ => out.push(b),
            }
            while out.len() >= 2 {
                let n = out.len();
                let (left, right) = (out[n - 2], out[n - 1]);
                let p = left.end();
                if p % sector == 0
                    || left.status == BlockStatus::Finished
                    || right.status == BlockStatus::Finished
                {
                    break;
                }
                fixed += 1;
                let sec_start = p - p % sector;
                let sec_end = sec_start.saturating_add(sector);
                if left.status.rank() <= right.status.rank() {
                    // The partial sector goes to the left block.
                    let to = sec_end.min(right.end());
                    out[n - 2].size = to - left.pos;
                    if to == right.end() {
                        out.pop();
                    } else {
                        out[n - 1].pos = to;
                        out[n - 1].size = right.end() - to;
                    }
                } else {
                    // The partial sector goes to the right block.
                    let from = sec_start.max(left.pos);
                    out[n - 1].pos = from;
                    out[n - 1].size = right.end() - from;
                    if from == left.pos {
                        out.remove(n - 2);
                        if n >= 3 && out[n - 3].status == out[n - 2].status {
                            out[n - 3].size += out[n - 2].size;
                            out.pop();
                        }
                    } else {
                        out[n - 2].size = from - left.pos;
                    }
                }
            }
        }
        if fixed > 0 {
            self.blocks = out;
            self.recount();
        }
        fixed
    }

    /// Check every invariant; returns a description of the first violation.
    /// Used by tests and debug assertions.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut expect = 0u64;
        let mut sizes = [0u64; 5];
        let mut bad = 0u64;
        for (i, b) in self.blocks.iter().enumerate() {
            if b.size == 0 {
                return Err(format!("block {i} has size 0"));
            }
            if b.pos != expect {
                return Err(format!(
                    "block {i} starts at {} but the previous one ends at {expect}",
                    b.pos
                ));
            }
            if i > 0 && self.blocks[i - 1].status == b.status {
                return Err(format!("blocks {} and {i} have the same status", i - 1));
            }
            expect = b
                .pos
                .checked_add(b.size)
                .ok_or_else(|| format!("block {i} overflows"))?;
            sizes[b.status.index()] += b.size;
            if b.status == BlockStatus::BadSector {
                bad += 1;
            }
        }
        if expect != self.size {
            return Err(format!(
                "blocks cover {expect} bytes but the map size is {}",
                self.size
            ));
        }
        if sizes != self.sizes || bad != self.bad_blocks {
            return Err("cached totals do not match the blocks".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn r(s: u64, e: u64) -> Range {
        Range::new(s, e)
    }

    fn chars(m: &StatusMap) -> String {
        m.iter()
            .map(|b| format!("{}:{}{}", b.pos, b.size, b.status.as_char()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn status_chars_roundtrip() {
        for s in BlockStatus::ALL {
            assert_eq!(BlockStatus::from_char(s.as_char()), Some(s));
        }
        assert_eq!(BlockStatus::from_char('F'), None);
        assert_eq!(BlockStatus::from_char('x'), None);
    }

    #[test]
    fn set_splits_and_merges() {
        let mut m = StatusMap::new(100);
        m.set(r(10, 20), BlockStatus::Finished);
        assert_eq!(chars(&m), "0:10? 10:10+ 20:80?");
        m.set(r(20, 30), BlockStatus::Finished);
        assert_eq!(chars(&m), "0:10? 10:20+ 30:70?");
        m.set(r(0, 10), BlockStatus::Finished);
        assert_eq!(chars(&m), "0:30+ 30:70?");
        m.set(r(50, 60), BlockStatus::BadSector);
        m.set(r(60, 70), BlockStatus::BadSector);
        assert_eq!(m.stats().bad_areas, 1);
        m.set(r(55, 56), BlockStatus::Finished);
        assert_eq!(m.stats().bad_areas, 2);
        m.set(r(0, 1000), BlockStatus::NonTried);
        assert_eq!(chars(&m), "0:100?");
        assert_eq!(m.stats().bad_areas, 0);
        m.validate().unwrap();
    }

    #[test]
    fn set_where_never_touches_other_statuses() {
        let mut m = StatusMap::new(100);
        m.set(r(10, 20), BlockStatus::Finished);
        m.set_where(r(0, 100), &[BlockStatus::NonTried], BlockStatus::NonTrimmed);
        assert_eq!(chars(&m), "0:10* 10:10+ 20:80*");
        m.validate().unwrap();
    }

    #[test]
    fn find_next_both_directions() {
        let mut m = StatusMap::new(100);
        m.set(r(10, 20), BlockStatus::Finished);
        m.set(r(40, 50), BlockStatus::Finished);
        assert_eq!(
            m.find_next(0, BlockStatus::Finished, Direction::Forward),
            Some(r(10, 20))
        );
        assert_eq!(
            m.find_next(15, BlockStatus::Finished, Direction::Forward),
            Some(r(15, 20))
        );
        assert_eq!(
            m.find_next(20, BlockStatus::Finished, Direction::Forward),
            Some(r(40, 50))
        );
        assert_eq!(
            m.find_next(50, BlockStatus::Finished, Direction::Forward),
            None
        );
        assert_eq!(
            m.find_next(100, BlockStatus::Finished, Direction::Backward),
            Some(r(40, 50))
        );
        assert_eq!(
            m.find_next(45, BlockStatus::Finished, Direction::Backward),
            Some(r(40, 45))
        );
        assert_eq!(
            m.find_next(40, BlockStatus::Finished, Direction::Backward),
            Some(r(10, 20))
        );
        assert_eq!(
            m.find_next(10, BlockStatus::Finished, Direction::Backward),
            None
        );
        assert_eq!(
            m.find_next_any(
                0,
                &[BlockStatus::BadSector, BlockStatus::NonTried],
                Direction::Forward
            )
            .map(|b| (b.range(), b.status)),
            Some((r(0, 10), BlockStatus::NonTried))
        );
        assert_eq!(
            StatusMap::new(0).find_next(0, BlockStatus::NonTried, Direction::Backward),
            None
        );
    }

    #[test]
    fn resize_policies() {
        let mut m = StatusMap::new(100);
        m.set(r(80, 90), BlockStatus::Finished);
        m.resize(150, ResizePolicy::KeepFinished).unwrap();
        assert_eq!(chars(&m), "0:80? 80:10+ 90:60?");
        assert!(m.resize(85, ResizePolicy::KeepFinished).is_err());
        m.resize(90, ResizePolicy::KeepFinished).unwrap();
        assert_eq!(chars(&m), "0:80? 80:10+");
        m.resize(85, ResizePolicy::Truncate).unwrap();
        assert_eq!(chars(&m), "0:80? 80:5+");
        m.resize(0, ResizePolicy::Truncate).unwrap();
        assert!(m.is_empty());
        m.resize(10, ResizePolicy::KeepFinished).unwrap();
        assert_eq!(chars(&m), "0:10?");
        m.validate().unwrap();
    }

    impl StatusMap {
        /// The original walk (ddrescue's loop with one step back, one
        /// `set` per fix), kept as the reference for the linear version.
        fn join_subsectors_reference(&mut self, sector: u64) -> usize {
            if sector <= 1 {
                return 0;
            }
            let mut fixed = 0usize;
            let mut i = 0usize;
            // Each fix removes one unaligned non-finished boundary and steps
            // back one index, so the loop runs at most ~3 x (number of blocks)
            // iterations; the guard only exists to make that bound explicit.
            let mut guard = self.blocks.len().saturating_mul(4) + 16;
            while i + 1 < self.blocks.len() && guard > 0 {
                guard -= 1;
                let left = self.blocks[i];
                let right = self.blocks[i + 1];
                let p = left.end();
                if p % sector == 0
                    || left.status == BlockStatus::Finished
                    || right.status == BlockStatus::Finished
                {
                    i += 1;
                    continue;
                }
                let sec_start = p - p % sector;
                let sec_end = sec_start.saturating_add(sector);
                if left.status.rank() <= right.status.rank() {
                    self.set(Range::new(p, sec_end.min(right.end())), left.status);
                } else {
                    self.set(Range::new(sec_start.max(left.pos), p), right.status);
                }
                fixed += 1;
                i = i.saturating_sub(1);
            }
            fixed
        }
    }

    /// The linear `join_subsectors` makes the same changes as the walk it
    /// replaced, on random maps with every status, odd sizes and several
    /// sector sizes.
    #[test]
    fn join_subsectors_matches_the_reference_walk() {
        let statuses = [
            BlockStatus::NonTried,
            BlockStatus::NonTrimmed,
            BlockStatus::NonScraped,
            BlockStatus::BadSector,
            BlockStatus::Finished,
        ];
        let mut rng = StdRng::seed_from_u64(0x0a11_09e5);
        let mut total = 0usize;
        for round in 0..4000 {
            let n = rng.gen_range(1..40);
            let mut blocks = Vec::new();
            let mut pos = 0u64;
            for _ in 0..n {
                let size = match rng.gen_range(0..4) {
                    0 => rng.gen_range(1..8),
                    1 => rng.gen_range(1..600),
                    2 => 512 * rng.gen_range(1..5),
                    _ => rng.gen_range(1..5000),
                };
                let status = statuses[rng.gen_range(0..statuses.len())];
                blocks.push(MapBlock { pos, size, status });
                pos += size;
            }
            let sector = [2u64, 7, 512, 4096][rng.gen_range(0..4)];
            let mut a = StatusMap::from_contiguous(blocks);
            let mut b = a.clone();
            let fa = a.join_subsectors(sector);
            let fb = b.join_subsectors_reference(sector);
            assert_eq!(a.blocks(), b.blocks(), "round {round}, sector {sector}");
            assert_eq!(fa, fb, "round {round}, sector {sector}");
            assert_eq!(a.stats(), b.stats(), "round {round}");
            a.validate().unwrap();
            total += fa;
        }
        assert!(total > 10_000, "only {total} boundaries needed fixing");
    }

    #[test]
    fn join_subsectors_is_linear_on_a_badly_damaged_map() {
        let blocks: Vec<MapBlock> = (0..200_000u64)
            .map(|i| MapBlock {
                pos: i * 512,
                size: 512,
                status: [
                    BlockStatus::Finished,
                    BlockStatus::BadSector,
                    BlockStatus::NonScraped,
                    BlockStatus::NonTrimmed,
                    BlockStatus::NonTried,
                ][(i % 5) as usize],
            })
            .collect();
        let mut m = StatusMap::from_contiguous(blocks);
        let t0 = std::time::Instant::now();
        m.join_subsectors(4096);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            t0.elapsed()
        );
        m.validate().unwrap();
    }

    #[test]
    fn join_subsectors_moves_partial_sectors_to_less_processed() {
        let mut m = StatusMap::from_contiguous(vec![
            MapBlock {
                pos: 0,
                size: 700,
                status: BlockStatus::NonTried,
            },
            MapBlock {
                pos: 700,
                size: 1348,
                status: BlockStatus::BadSector,
            },
            MapBlock {
                pos: 2048,
                size: 100,
                status: BlockStatus::Finished,
            },
            MapBlock {
                pos: 2148,
                size: 1948,
                status: BlockStatus::NonScraped,
            },
        ]);
        let fixed = m.join_subsectors(512);
        assert!(fixed >= 1);
        // 700 is moved up to 1024 (non-tried is less processed than bad);
        // boundaries next to finished data stay where they are.
        assert_eq!(chars(&m), "0:1024? 1024:1024- 2048:100+ 2148:1948/");
        m.validate().unwrap();
    }

    /// Property test: random `set` calls keep every invariant and agree
    /// with a byte-per-entry model.
    #[test]
    fn random_sets_keep_invariants_and_match_model() {
        let mut rng = StdRng::seed_from_u64(0x5eed_1234);
        for round in 0..200 {
            let size: u64 = rng.gen_range(1..=3000);
            let mut m = StatusMap::new(size);
            let mut model = vec![BlockStatus::NonTried; size as usize];
            for _ in 0..rng.gen_range(1..80) {
                let a = rng.gen_range(0..=size + 10);
                let b = rng.gen_range(0..=size + 10);
                let (s, e) = (a.min(b), a.max(b));
                let st = BlockStatus::ALL[rng.gen_range(0..5)];
                m.set(r(s, e), st);
                for x in s.min(size)..e.min(size) {
                    model[x as usize] = st;
                }
                m.validate()
                    .unwrap_or_else(|e| panic!("round {round}: {e}"));
                assert_eq!(m.size(), size);
                let total: u64 = m.iter().map(|b| b.size).sum();
                assert_eq!(total, size);
            }
            for (i, st) in model.iter().enumerate() {
                assert_eq!(m.get(i as u64), Some(*st), "round {round} byte {i}");
            }
            // Bad areas = maximal runs of BadSector in the model.
            let runs = model
                .iter()
                .enumerate()
                .filter(|(i, s)| {
                    **s == BlockStatus::BadSector
                        && (*i == 0 || model[i - 1] != BlockStatus::BadSector)
                })
                .count() as u64;
            assert_eq!(m.stats().bad_areas, runs);
            assert_eq!(m.stats().total(), size);
            assert_eq!(m.stats_in(r(0, size)), m.stats());
            // Sweeps of join_subsectors / resize keep the invariants too.
            let mut j = m.clone();
            j.join_subsectors(rng.gen_range(1..700));
            j.validate().unwrap();
            for (i, st) in model.iter().enumerate() {
                if *st == BlockStatus::Finished {
                    assert_eq!(j.get(i as u64), Some(BlockStatus::Finished));
                }
            }
            let mut t = m.clone();
            t.resize(rng.gen_range(0..size * 2), ResizePolicy::Truncate)
                .unwrap();
            t.validate().unwrap();
        }
    }

    #[test]
    fn huge_and_degenerate_ranges_are_harmless() {
        let mut m = StatusMap::new(u64::MAX);
        m.set(r(u64::MAX - 10, u64::MAX), BlockStatus::BadSector);
        m.set(r(5, 4), BlockStatus::Finished);
        m.set(r(u64::MAX, u64::MAX), BlockStatus::Finished);
        m.validate().unwrap();
        assert_eq!(m.stats().bad_sectors, 10);
        assert_eq!(m.get(u64::MAX), None);
        assert_eq!(
            m.find_next(u64::MAX, BlockStatus::BadSector, Direction::Backward),
            Some(r(u64::MAX - 10, u64::MAX))
        );
    }
}
