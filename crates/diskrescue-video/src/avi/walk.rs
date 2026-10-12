//! The `movi` chunk walker and `idx1` reader.
//!
//! The walker never trusts a size field further than it can check: every
//! chunk id must look like an AVI chunk id, every size must fit before the
//! hard end, and the even-padding rule is applied with a fallback for writers
//! that forget the pad byte.

use crate::io::Window;

use super::hdr::{read_chunk, ChunkHdr};

/// What a chunk id inside `movi` (or between the top-level lists) denotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdKind {
    /// `##dc` / `##db`: video data of stream `idx`.
    Video(u8),
    /// `##wb`: audio data.
    Audio(u8),
    /// `##pc`: palette change.
    Palette(u8),
    /// `##tx` and other two-letter stream chunk codes.
    Other(u8),
    /// `ix##`: OpenDML standard index.
    OdmlIndex,
    /// `JUNK` padding.
    Junk,
    /// `LIST` (normally `rec `).
    List,
    /// `idx1`: legacy index, ends `movi`.
    Idx1,
    /// `RIFF`: the next RIFF chunk (AVIX).
    Riff,
}

impl IdKind {
    /// True for chunks that carry stream data (indexed in `idx1`).
    pub fn is_data(self) -> bool {
        matches!(
            self,
            IdKind::Video(_) | IdKind::Audio(_) | IdKind::Palette(_) | IdKind::Other(_)
        )
    }

    /// Stream index of a data chunk.
    pub fn stream(self) -> Option<u8> {
        match self {
            IdKind::Video(i) | IdKind::Audio(i) | IdKind::Palette(i) | IdKind::Other(i) => Some(i),
            _ => None,
        }
    }
}

/// Classify a chunk id; `None` means "does not look like an AVI chunk id".
pub(crate) fn classify(id: [u8; 4]) -> Option<IdKind> {
    match &id {
        b"JUNK" => return Some(IdKind::Junk),
        b"LIST" => return Some(IdKind::List),
        b"idx1" => return Some(IdKind::Idx1),
        b"RIFF" => return Some(IdKind::Riff),
        _ => {}
    }
    if id[0] == b'i' && id[1] == b'x' && id[2].is_ascii_digit() && id[3].is_ascii_digit() {
        return Some(IdKind::OdmlIndex);
    }
    if id[0].is_ascii_digit() && id[1].is_ascii_digit() && id[2].is_ascii_lowercase() {
        let idx = (id[0] - b'0') * 10 + (id[1] - b'0');
        if id[3].is_ascii_lowercase() || id[3].is_ascii_digit() {
            return Some(match (&id[2], &id[3]) {
                (b'd', b'c') | (b'd', b'b') => IdKind::Video(idx),
                (b'w', b'b') => IdKind::Audio(idx),
                (b'p', b'c') => IdKind::Palette(idx),
                _ => IdKind::Other(idx),
            });
        }
    }
    None
}

/// One chunk seen by the walker.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkInfo {
    pub hdr: ChunkHdr,
    pub kind: IdKind,
    /// File offset where the next chunk starts (after padding).
    pub next: u64,
    /// The writer omitted the pad byte after an odd-sized chunk.
    pub pad_missing: bool,
}

/// Why a walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkEnd {
    /// Reached the hard end exactly at a chunk boundary.
    Clean(u64),
    /// The `idx1` chunk header is here.
    Idx1(u64),
    /// A `RIFF` header is here (the next RIFF chunk).
    Riff(u64),
    /// Not a chunk header.
    Garbage(u64),
    /// A valid header whose data does not fit (file cut, or size damaged).
    Partial(u64),
    /// Read failure.
    Fault(u64),
}

impl WalkEnd {
    /// Offset where the walk stopped.
    pub fn pos(self) -> u64 {
        match self {
            WalkEnd::Clean(p)
            | WalkEnd::Idx1(p)
            | WalkEnd::Riff(p)
            | WalkEnd::Garbage(p)
            | WalkEnd::Partial(p)
            | WalkEnd::Fault(p) => p,
        }
    }
}

/// Is there a plausible chunk header (or the clean end) at `pos`?
pub(crate) fn header_ok(win: &mut Window<'_>, pos: u64, hard_end: u64) -> bool {
    if pos >= hard_end {
        return pos == hard_end;
    }
    let Some(c) = read_chunk(win, pos) else {
        return false;
    };
    let Some(kind) = classify(c.id) else {
        return false;
    };
    match kind {
        IdKind::Idx1 | IdKind::Riff => true,
        _ => pos + 8 + u64::from(c.size) <= hard_end,
    }
}

/// One step of a [`MoviWalker`].
pub(crate) enum Step {
    /// A chunk.
    Chunk(ChunkInfo),
    /// The walk ended.
    End(WalkEnd),
}

/// Incremental chunk walker. Unlike [`walk_movi`] it does not hold the
/// window between steps, so a repair can inspect chunk payloads as it goes.
pub(crate) struct MoviWalker {
    pos: u64,
    hard_end: u64,
}

impl MoviWalker {
    /// Start at `pos` (the first byte after the `movi` fourcc).
    pub fn new(pos: u64, hard_end: u64) -> Self {
        MoviWalker { pos, hard_end }
    }

    /// Continue from another position (after a resync).
    pub fn jump(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// Hard end of the walk.
    pub fn hard_end(&self) -> u64 {
        self.hard_end
    }

    /// Advance by one chunk.
    pub fn step(&mut self, win: &mut Window<'_>) -> Step {
        let hard_end = self.hard_end.min(win.avail());
        let pos = self.pos;
        if pos >= hard_end {
            return Step::End(WalkEnd::Clean(pos));
        }
        let Some(c) = read_chunk(win, pos) else {
            return Step::End(if win.fault().is_some() {
                WalkEnd::Fault(win.fault().map_or(pos, |f| f.offset.max(pos)))
            } else {
                WalkEnd::Partial(pos)
            });
        };
        let Some(kind) = classify(c.id) else {
            return Step::End(WalkEnd::Garbage(pos));
        };
        match kind {
            IdKind::Idx1 => return Step::End(WalkEnd::Idx1(pos)),
            IdKind::Riff => return Step::End(WalkEnd::Riff(pos)),
            IdKind::List => {
                // Step into `rec ` groups; skip other lists by size.
                let ty = win.fourcc(c.data());
                if ty == Some(*b"rec ") {
                    self.pos = pos + 12;
                    return Step::Chunk(ChunkInfo {
                        hdr: c,
                        kind,
                        next: pos + 12,
                        pad_missing: false,
                    });
                }
                if c.size < 4 {
                    return Step::End(WalkEnd::Garbage(pos));
                }
            }
            _ => {}
        }
        let data_end = pos + 8 + u64::from(c.size);
        if data_end > hard_end {
            return Step::End(if data_end > win.avail() || win.fault().is_some() {
                WalkEnd::Partial(pos)
            } else {
                // Fits in the file but not in the list: the size is damaged.
                WalkEnd::Garbage(pos)
            });
        }
        let mut next = data_end + u64::from(c.size & 1);
        let mut pad_missing = false;
        if c.size & 1 == 1 && !header_ok(win, next, hard_end) && header_ok(win, data_end, hard_end)
        {
            next = data_end;
            pad_missing = true;
        }
        self.pos = next;
        Step::Chunk(ChunkInfo {
            hdr: c,
            kind,
            next,
            pad_missing,
        })
    }
}

/// Walk chunks from `pos` (the first byte after the `movi` fourcc) until
/// something other than a chunk is found or `hard_end` is reached. `visit` is
/// called for every chunk, including `JUNK`, `ix##` and `rec ` lists.
pub(crate) fn walk_movi(
    win: &mut Window<'_>,
    pos: u64,
    hard_end: u64,
    mut visit: impl FnMut(&ChunkInfo),
) -> WalkEnd {
    let mut w = MoviWalker::new(pos, hard_end);
    loop {
        match w.step(win) {
            Step::Chunk(c) => visit(&c),
            Step::End(e) => return e,
        }
    }
}

/// Search for the next plausible run of stream chunks starting at `from`,
/// scanning at most `max_scan` bytes. Used by aggressive repair to skip a
/// damaged region. A candidate must be followed by three more valid headers
/// (or the end), so a stray `00dc` inside compressed data does not count.
pub(crate) fn resync(win: &mut Window<'_>, from: u64, hard_end: u64, max_scan: u64) -> Option<u64> {
    let hard_end = hard_end.min(win.avail());
    let stop = from
        .saturating_add(max_scan)
        .min(hard_end.saturating_sub(8));
    let mut p = from;
    while p < stop {
        let block = win.get(p, 64 * 1024).to_vec();
        if block.len() < 8 {
            return None;
        }
        let last = block.len().saturating_sub(7);
        for i in 0..last {
            let off = p + i as u64;
            if off >= stop {
                return None;
            }
            let id = [block[i], block[i + 1], block[i + 2], block[i + 3]];
            if !matches!(
                classify(id),
                Some(IdKind::Video(_) | IdKind::Audio(_) | IdKind::Palette(_) | IdKind::Other(_))
            ) {
                continue;
            }
            if chain_ok(win, off, hard_end, 4) {
                return Some(off);
            }
        }
        p += last as u64;
        if last == 0 {
            return None;
        }
    }
    None
}

fn chain_ok(win: &mut Window<'_>, mut pos: u64, hard_end: u64, count: usize) -> bool {
    for _ in 0..count {
        if pos >= hard_end {
            return true;
        }
        let Some(c) = read_chunk(win, pos) else {
            return false;
        };
        match classify(c.id) {
            None => return false,
            Some(IdKind::Idx1 | IdKind::Riff) => return true,
            Some(_) => {}
        }
        let end = pos + 8 + u64::from(c.size);
        if end > hard_end {
            return false;
        }
        pos = end + u64::from(c.size & 1);
    }
    true
}

/// One 16-byte `idx1` entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Idx1Entry {
    pub id: [u8; 4],
    pub offset: u32,
    pub size: u32,
}

/// Sequential reader over the entries of an `idx1` chunk. It owns its block
/// buffer, so the caller keeps free use of the [`Window`] between entries.
pub(crate) struct Idx1Cursor {
    data: u64,
    total: u64,
    next: u64,
    block: Vec<u8>,
    block_first: u64,
}

impl Idx1Cursor {
    /// Cursor over `total` entries whose first entry is at file offset `data`.
    pub fn new(data: u64, total: u64) -> Self {
        Idx1Cursor {
            data,
            total,
            next: 0,
            block: Vec::new(),
            block_first: 0,
        }
    }

    /// Start at entry `index` instead of the first.
    pub fn at(data: u64, total: u64, index: u64) -> Self {
        let mut c = Idx1Cursor::new(data, total);
        c.next = index;
        c
    }

    /// Index of the entry the next call returns.
    pub fn position(&self) -> u64 {
        self.next
    }

    /// The next entry, or `None` at the end or when the data is cut short.
    pub fn next(&mut self, win: &mut Window<'_>) -> Option<Idx1Entry> {
        if self.next >= self.total {
            return None;
        }
        let in_block = self.next >= self.block_first
            && self.next < self.block_first + (self.block.len() / 16) as u64;
        if !in_block {
            let n = (self.total - self.next).min(4096) as usize;
            let have = win.get(self.data + self.next * 16, n * 16);
            let whole = have.len() / 16;
            if whole == 0 {
                return None;
            }
            self.block.clear();
            self.block.extend_from_slice(&have[..whole * 16]);
            self.block_first = self.next;
        }
        let k = (self.next - self.block_first) as usize * 16;
        let e = parse_entry(&self.block[k..k + 16]);
        self.next += 1;
        Some(e)
    }
}

fn parse_entry(b: &[u8]) -> Idx1Entry {
    let u = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    Idx1Entry {
        id: [b[0], b[1], b[2], b[3]],
        offset: u(8),
        size: u(12),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_classification() {
        assert_eq!(classify(*b"00dc"), Some(IdKind::Video(0)));
        assert_eq!(classify(*b"01wb"), Some(IdKind::Audio(1)));
        assert_eq!(classify(*b"12db"), Some(IdKind::Video(12)));
        assert_eq!(classify(*b"00pc"), Some(IdKind::Palette(0)));
        assert_eq!(classify(*b"02tx"), Some(IdKind::Other(2)));
        assert_eq!(classify(*b"ix00"), Some(IdKind::OdmlIndex));
        assert_eq!(classify(*b"JUNK"), Some(IdKind::Junk));
        assert_eq!(classify(*b"idx1"), Some(IdKind::Idx1));
        assert_eq!(classify(*b"\0\0\0\0"), None);
        assert_eq!(classify(*b"abcd"), None);
        assert_eq!(classify(*b"00DC"), None);
    }
}
