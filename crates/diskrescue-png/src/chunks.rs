//! Streaming PNG chunk walker.
//!
//! [`walk`] reads a PNG chunk by chunk, verifying every CRC while streaming
//! the chunk data through a fixed 64 KiB window, so a 200 MB file costs 200 MB
//! of sequential reads and a few kilobytes of memory.
//!
//! # Tolerance
//!
//! The walker is built for damaged media rather than for rejecting files:
//!
//! * A chunk whose CRC does not match is still recorded (`crc_ok == false`)
//!   and the walk continues at the position its length field announces.
//! * When the bytes where a chunk header should be are not a header (four
//!   ASCII letters and a length below 2^31), the walker scans forward, up to
//!   [`WalkOptions::gap_window`] (1 MiB by default), for a registered chunk
//!   type whose length and CRC check out, and resumes there. The skipped bytes
//!   are reported in [`ChunkWalk::gaps`].
//! * When a chunk's length field itself is damaged, the walker first looks for
//!   the length that makes the chunk's CRC valid (a rolling CRC over the bytes
//!   that follow) and otherwise treats everything up to the next recognisable
//!   chunk as the chunk's data.
//! * A missing IEND is not an error: the walk simply ends with
//!   [`StopReason::SourceEnd`], [`StopReason::Limit`] or
//!   [`StopReason::Damage`].
//!
//! Resynchronisation never targets a second IHDR and stops at a PNG signature,
//! so a truncated image followed on disk by another PNG is not stitched to it.
//!
//! All offsets in the result are relative to the `start` passed to [`walk`].

use diskrescue_core::ranges::Range;
use diskrescue_core::source::{ReadError, Source};
use diskrescue_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::io::{crc_step, crc_update, Reader, BLOCK, CRC_INIT};

pub use crate::ihdr::Ihdr;

/// The 8-byte PNG file signature.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Largest chunk length the specification allows (`2^31 - 1`).
pub const MAX_CHUNK_LEN: u32 = 0x7FFF_FFFF;

/// A declared chunk length beyond this cannot belong to any file this crate
/// carves (the largest PNG it measures is 256 MiB): the length field is
/// treated as damaged.
const IMPLAUSIBLE_CHUNK_LEN: u32 = 1 << 28;

/// Chunk types accepted as resynchronisation targets (everything registered
/// for PNG except IHDR, which can only legitimately appear once, first).
const RESYNC_TAGS: &[&[u8; 4]] = &[
    b"IDAT", b"IEND", b"PLTE", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB", b"cICP",
    b"mDCV", b"cLLI", b"bKGD", b"hIST", b"pHYs", b"sPLT", b"tIME", b"iTXt", b"tEXt", b"zTXt",
    b"eXIf", b"acTL", b"fcTL", b"fdAT", b"oFFs", b"pCAL", b"sCAL", b"gIFg", b"gIFt", b"gIFx",
    b"sTER", b"vpAg", b"caNv", b"iDOT",
];

/// One chunk found by [`walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Offset of the length field, relative to the walk start.
    pub offset: u64,
    /// Data length in bytes. For a chunk whose length field was damaged this
    /// is the length the walker settled on (see [`Chunk::length_repaired`]).
    pub len: u32,
    /// Four-letter chunk type.
    pub kind: [u8; 4],
    /// True when the stored CRC matches the CRC of type + data.
    pub crc_ok: bool,
    /// Offset of the first data byte (`offset + 8`).
    pub data_offset: u64,
    /// CRC as stored in the file (0 when the chunk was cut before its CRC).
    pub crc_stored: u32,
    /// CRC computed over the data that was actually readable.
    pub crc_computed: u32,
    /// False when the chunk's data or CRC runs past the end of the readable
    /// bytes (truncation, unreadable sectors).
    pub complete: bool,
    /// True when unparseable bytes were skipped immediately before this
    /// chunk.
    pub after_gap: bool,
    /// True when the stored length field was wrong and the walker derived
    /// `len` from the CRC or from the next chunk.
    pub length_repaired: bool,
}

impl Chunk {
    /// Offset one past the chunk's CRC.
    pub fn end(&self) -> u64 {
        self.data_offset + self.len as u64 + 4
    }

    /// Range of the chunk's data bytes.
    pub fn data_range(&self) -> Range {
        Range::with_len(self.data_offset, self.len as u64)
    }

    /// True for critical chunks (uppercase first letter).
    pub fn is_critical(&self) -> bool {
        self.kind[0] & 0x20 == 0
    }

    /// True when the chunk type equals `kind`.
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }

    /// The type as a string (lossy for non-ASCII garbage).
    pub fn kind_name(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

/// Why a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// An IEND chunk was found.
    Iend,
    /// The source ended before an IEND chunk.
    SourceEnd,
    /// The read limit was reached before an IEND chunk.
    Limit,
    /// Damage that could not be skipped: an unreadable region or a chunk
    /// header that no resynchronisation could explain.
    Damage,
    /// The caller asked to stop at the first IDAT ([`WalkOptions::stop_at_idat`]).
    Requested,
}

/// Tuning knobs for [`walk_with`].
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Look for the next valid chunk after unparseable bytes or a damaged
    /// length field.
    pub resync: bool,
    /// How far to scan for the next chunk after garbage between chunks.
    pub gap_window: u64,
    /// How far to scan when a chunk's length field is suspected to be wrong.
    pub length_window: u64,
    /// Stop (without reading it) at the first IDAT chunk. Used to read only
    /// the header chunks of donor files.
    pub stop_at_idat: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            resync: true,
            gap_window: 1 << 20,
            length_window: 32 << 20,
            stop_at_idat: false,
        }
    }
}

/// The result of walking a PNG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkWalk {
    /// Absolute offset in the source where the file starts.
    pub start: u64,
    /// True when the 8-byte signature was present.
    pub signature_ok: bool,
    /// Every chunk accepted, in file order, including chunks with a bad CRC
    /// and a final chunk cut short by the end of the data.
    pub chunks: Vec<Chunk>,
    /// The first 13-byte IHDR chunk, parsed. The fields are not validated;
    /// use [`Ihdr::check`].
    pub ihdr: Option<Ihdr>,
    /// Data ranges of the IDAT chunks, in order, including damaged ones.
    pub idat_ranges: Vec<Range>,
    /// For each entry of `idat_ranges`: `(crc_ok, complete)`. The lenient
    /// decoder uses the CRC verdicts to tell which part of the image data can
    /// be vouched for.
    pub idat_status: Vec<(bool, bool)>,
    /// Offset of the IEND chunk, when found.
    pub iend_offset: Option<u64>,
    /// First problem met: offset and description.
    pub first_bad: Option<(u64, String)>,
    /// Observations (resynchronisations, repaired lengths, ...).
    pub notes: Vec<String>,
    /// Byte ranges skipped while resynchronising (garbage between chunks).
    pub gaps: Vec<Range>,
    /// Offsets of chunks whose contents are implausible (for example a pHYs
    /// chunk with a unit of 5, or a profile that does not inflate). Filled in
    /// by validation, not by [`walk`]; empty otherwise.
    pub malformed: Vec<u64>,
    /// One past the last byte the walk attributes to the file: the end of
    /// IEND, or the end of the readable/accepted data when there is none.
    pub end: u64,
    /// Why the walk ended.
    pub stop: StopReason,
    /// Offset of the chunk (or header position) at which the walk stopped:
    /// the IEND chunk, the chunk that was cut short or did not parse, or the
    /// end of the data when it ended on a chunk boundary.
    pub stop_offset: u64,
    /// True for Apple CgBI files (raw deflate, no zlib wrapper).
    pub cgbi: bool,
}

impl ChunkWalk {
    fn new(start: u64) -> Self {
        ChunkWalk {
            start,
            signature_ok: false,
            chunks: Vec::new(),
            ihdr: None,
            idat_ranges: Vec::new(),
            idat_status: Vec::new(),
            iend_offset: None,
            first_bad: None,
            notes: Vec::new(),
            gaps: Vec::new(),
            malformed: Vec::new(),
            end: 0,
            stop: StopReason::SourceEnd,
            stop_offset: 0,
            cgbi: false,
        }
    }

    /// True when an IEND chunk was found.
    pub fn has_iend(&self) -> bool {
        self.iend_offset.is_some()
    }

    /// Chunks whose CRC did not match (including cut-short chunks).
    pub fn bad_crc_chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.chunks.iter().filter(|c| !c.crc_ok)
    }

    /// True when every critical chunk (IHDR, PLTE, IDAT, IEND) is complete
    /// with a matching CRC.
    pub fn critical_crc_ok(&self) -> bool {
        self.chunks
            .iter()
            .filter(|c| c.is_critical())
            .all(|c| c.crc_ok)
    }

    /// Total payload bytes of all IDAT chunks.
    pub fn idat_bytes(&self) -> u64 {
        self.idat_ranges.iter().map(Range::len).sum()
    }

    /// First chunk of the given type.
    pub fn find(&self, kind: &[u8; 4]) -> Option<&Chunk> {
        self.chunks.iter().find(|c| c.is(kind))
    }

    /// The first violation of the chunk ordering rules, if any: IHDR first
    /// (after Apple's CgBI), a single PLTE before the image data, and the
    /// IDAT chunks consecutive.
    pub fn order_problem(&self) -> Option<String> {
        let ihdr_at = if self.cgbi { 1 } else { 0 };
        match self.chunks.get(ihdr_at) {
            Some(c) if c.is(b"IHDR") => {}
            _ => return Some("IHDR is not the first chunk".to_string()),
        }
        let first = self.chunks.iter().position(|c| c.is(b"IDAT"));
        let last = self.chunks.iter().rposition(|c| c.is(b"IDAT"));
        if let (Some(f), Some(l)) = (first, last) {
            if self.chunks[f..=l].iter().any(|c| !c.is(b"IDAT")) {
                return Some("IDAT chunks are not consecutive".to_string());
            }
            if self.chunks[f..].iter().any(|c| c.is(b"PLTE")) {
                return Some("PLTE comes after the image data".to_string());
            }
        }
        if self.chunks.iter().filter(|c| c.is(b"PLTE")).count() > 1 {
            return Some("more than one PLTE chunk".to_string());
        }
        None
    }
}

/// Walk the chunks of the PNG that starts at `start`, reading at most
/// `limit` bytes, with default options. See the module documentation.
pub fn walk(src: &dyn Source, start: u64, limit: u64) -> Result<ChunkWalk> {
    walk_with(src, start, limit, &WalkOptions::default())
}

/// [`walk`] with explicit [`WalkOptions`].
pub fn walk_with(
    src: &dyn Source,
    start: u64,
    limit: u64,
    opts: &WalkOptions,
) -> Result<ChunkWalk> {
    let mut w = Walker {
        rd: Reader::new(src, start, limit),
        opts,
        w: ChunkWalk::new(start),
        buf: vec![0u8; BLOCK],
        seen_ihdr: false,
    };
    w.run()?;
    let mut walk = w.w;
    if !walk.gaps.is_empty() {
        let skipped: u64 = walk.gaps.iter().map(Range::len).sum();
        walk.notes.push(format!(
            "skipped {} bytes of unparseable data in {} place(s)",
            skipped,
            walk.gaps.len()
        ));
    }
    log::debug!(
        "png walk @{}: {} chunks, iend={:?}, stop={:?}, end={}",
        start,
        walk.chunks.len(),
        walk.iend_offset,
        walk.stop,
        walk.end
    );
    Ok(walk)
}

enum Header {
    Valid,
    Invalid,
    End,
}

struct Verified {
    complete: bool,
    /// Data bytes actually read.
    got: u64,
    computed: u32,
    stored: u32,
    err: Option<ReadError>,
}

struct Walker<'a> {
    rd: Reader<'a>,
    opts: &'a WalkOptions,
    w: ChunkWalk,
    buf: Vec<u8>,
    seen_ihdr: bool,
}

fn parse_header(h: &[u8; 8]) -> (u32, [u8; 4]) {
    (
        u32::from_be_bytes([h[0], h[1], h[2], h[3]]),
        [h[4], h[5], h[6], h[7]],
    )
}

fn header_plausible(h: &[u8; 8]) -> bool {
    let (len, kind) = parse_header(h);
    len <= MAX_CHUNK_LEN && kind.iter().all(u8::is_ascii_alphabetic)
}

impl Walker<'_> {
    fn bad(&mut self, offset: u64, what: String) {
        log::debug!("png walk: problem at {offset}: {what}");
        if self.w.first_bad.is_none() {
            self.w.first_bad = Some((offset, what));
        }
    }

    fn halt(&mut self, reason: StopReason, at: u64, end: u64) {
        self.w.stop = reason;
        self.w.stop_offset = at;
        self.w.end = end;
    }

    fn end_reason(&self) -> StopReason {
        if self.rd.limited {
            StopReason::Limit
        } else {
            StopReason::SourceEnd
        }
    }

    /// Record a read error: media errors end the walk as damage, anything
    /// else is propagated.
    fn read_failed(&mut self, pos: u64, e: ReadError) -> Result<()> {
        if !e.kind.is_media_error() {
            return Err(Error::Read(e));
        }
        self.bad(pos, format!("unreadable data at offset {}", e.offset));
        self.halt(StopReason::Damage, pos, pos);
        Ok(())
    }

    fn stop_at_end(&mut self, pos: u64) {
        let left = self.rd.avail.saturating_sub(pos);
        let reason = self.end_reason();
        self.halt(reason, pos, pos + left);
        if left > 0 {
            self.bad(pos, "truncated chunk header".to_string());
        } else if self.w.iend_offset.is_none() {
            self.bad(pos, "data ends without an IEND chunk".to_string());
        }
    }

    fn peek_header(&mut self, at: u64) -> Header {
        if at >= self.rd.avail {
            return Header::End;
        }
        match self.rd.read_array::<8>(at) {
            Ok(Some(h)) if header_plausible(&h) => Header::Valid,
            _ => Header::Invalid,
        }
    }

    /// Stream the data and CRC of a chunk whose header says `len` bytes.
    fn verify(&mut self, pos: u64, len: u32, kind: &[u8; 4], head: &mut Vec<u8>) -> Verified {
        let data_off = pos + 8;
        let mut state = crc_update(CRC_INIT, kind);
        let mut off = data_off;
        let mut remaining = len as u64;
        while remaining > 0 {
            let n = remaining.min(BLOCK as u64) as usize;
            let f = self.rd.read(off, &mut self.buf[..n]);
            state = crc_update(state, &self.buf[..f.got]);
            if head.len() < 32 {
                let take = (32 - head.len()).min(f.got);
                head.extend_from_slice(&self.buf[..take]);
            }
            off += f.got as u64;
            remaining -= f.got as u64;
            if f.got < n {
                return Verified {
                    complete: false,
                    got: len as u64 - remaining,
                    computed: !state,
                    stored: 0,
                    err: f.err,
                };
            }
        }
        match self.rd.read_array::<4>(off) {
            Ok(Some(b)) => Verified {
                complete: true,
                got: len as u64,
                computed: !state,
                stored: u32::from_be_bytes(b),
                err: None,
            },
            Ok(None) => Verified {
                complete: false,
                got: len as u64,
                computed: !state,
                stored: 0,
                err: None,
            },
            Err(e) => Verified {
                complete: false,
                got: len as u64,
                computed: !state,
                stored: 0,
                err: Some(e),
            },
        }
    }

    fn crc_matches(&mut self, pos: u64, len: u32, kind: &[u8; 4]) -> bool {
        let mut none = Vec::new();
        let v = self.verify(pos, len, kind, &mut none);
        v.complete && v.computed == v.stored
    }

    /// Scan `[from, scan_end)` for the start of a registered chunk whose
    /// length and CRC are valid. Gives up at a PNG signature.
    fn scan_for_chunk(&mut self, from: u64, scan_end: u64) -> Option<u64> {
        let mut block = vec![0u8; BLOCK + 8];
        let mut p = from;
        while p < scan_end && p + 8 <= self.rd.avail {
            let want = (self.rd.avail - p).min(BLOCK as u64 + 7) as usize;
            let f = self.rd.read(p, &mut block[..want]);
            if f.got < 8 {
                return None;
            }
            let last = f.got - 8;
            for i in 0..=last {
                let s = p + i as u64;
                if s >= scan_end {
                    return None;
                }
                let b0 = block[i];
                if b0 == SIGNATURE[0] && block[i..i + 8] == SIGNATURE {
                    log::debug!("png resync: hit another PNG signature at {s}, giving up");
                    return None;
                }
                let tag = [block[i + 4], block[i + 5], block[i + 6], block[i + 7]];
                if !tag[0].is_ascii_alphabetic() || !RESYNC_TAGS.iter().any(|t| **t == tag) {
                    continue;
                }
                let len = u32::from_be_bytes([block[i], block[i + 1], block[i + 2], block[i + 3]]);
                if len > MAX_CHUNK_LEN || s + 12 + len as u64 > self.rd.avail {
                    continue;
                }
                if self.crc_matches(s, len, &tag) {
                    return Some(s);
                }
            }
            if f.err.is_some() {
                return None;
            }
            p += last as u64 + 1;
        }
        None
    }

    /// Look for a PNG signature inside `[from, from + len)`. A signature in
    /// the middle of a chunk means another file starts there and this one was
    /// cut short.
    fn find_signature(&mut self, from: u64, len: u64) -> Option<u64> {
        let mut block = vec![0u8; BLOCK + 8];
        let end = (from + len).min(self.rd.avail);
        let mut p = from;
        while p + 8 <= end {
            let want = (end - p).min(BLOCK as u64 + 7) as usize;
            let f = self.rd.read(p, &mut block[..want]);
            if f.got < 8 {
                return None;
            }
            let last = f.got - 8;
            for i in 0..=last {
                if block[i] == SIGNATURE[0] && block[i..i + 8] == SIGNATURE {
                    return Some(p + i as u64);
                }
            }
            p += last as u64 + 1;
        }
        None
    }

    /// Find the length that makes the chunk at `pos` (type `kind`) carry a
    /// valid CRC, by a rolling CRC over the bytes that follow. The candidate
    /// must be followed by a plausible chunk header or by the end of the data.
    fn find_len_by_crc(&mut self, pos: u64, kind: &[u8; 4], declared: u32) -> Option<u32> {
        let data_off = pos + 8;
        if self.rd.avail < data_off + 4 {
            return None;
        }
        let max_len = (self.rd.avail - data_off - 4).min(self.opts.length_window);
        let mut state = crc_update(CRC_INIT, kind);
        let mut block = vec![0u8; BLOCK + 4];
        let mut consumed = 0u64;
        loop {
            let p = data_off + consumed;
            if p + 4 > self.rd.avail {
                return None;
            }
            let want = (self.rd.avail - p).min(BLOCK as u64 + 4) as usize;
            let f = self.rd.read(p, &mut block[..want]);
            if f.got < 4 {
                return None;
            }
            let usable = f.got - 4;
            for i in 0..usable {
                if consumed > max_len {
                    return None;
                }
                let word = u32::from_be_bytes([block[i], block[i + 1], block[i + 2], block[i + 3]]);
                if !state == word && consumed != declared as u64 {
                    let next = data_off + consumed + 4;
                    if !matches!(self.peek_header(next), Header::Invalid) {
                        return Some(consumed as u32);
                    }
                }
                state = crc_step(state, block[i]);
                consumed += 1;
            }
            if usable == 0 || f.err.is_some() {
                return None;
            }
        }
    }

    /// The chunk at `pos` has a length field that cannot be right. Returns
    /// the length to use instead.
    ///
    /// `scan_next` also allows looking for the next recognisable chunk; it
    /// is skipped for chunks that merely run past the end of the data, where
    /// the cheap rolling-CRC check is enough and a second scan would only
    /// re-read the tail of a truncated file.
    fn resolve_length(
        &mut self,
        pos: u64,
        declared: u32,
        kind: &[u8; 4],
        scan_next: bool,
    ) -> Option<u32> {
        if !self.opts.resync {
            return None;
        }
        if let Some(l) = self.find_len_by_crc(pos, kind, declared) {
            log::debug!("png walk: chunk at {pos} length {declared} -> {l} (CRC match)");
            return Some(l);
        }
        if !scan_next {
            return None;
        }
        let span = (declared as u64)
            .saturating_add(self.opts.gap_window)
            .min(self.opts.length_window);
        let from = pos + 12;
        let to = self.rd.avail.min(from.saturating_add(span));
        let next = self.scan_for_chunk(from, to)?;
        let l = next - pos - 12;
        if l > MAX_CHUNK_LEN as u64 {
            return None;
        }
        log::debug!("png walk: chunk at {pos} length {declared} -> {l} (next chunk at {next})");
        Some(l as u32)
    }

    fn run(&mut self) -> Result<()> {
        let sig = match self.rd.read_array::<8>(0) {
            Ok(s) => s,
            Err(e) => return self.read_failed(0, e),
        };
        match sig {
            Some(s) if s == SIGNATURE => {}
            Some(_) => {
                self.bad(0, "missing PNG signature".to_string());
                self.halt(StopReason::Damage, 0, 0);
                return Ok(());
            }
            None => {
                self.bad(0, "file shorter than the PNG signature".to_string());
                let reason = self.end_reason();
                let end = self.rd.avail;
                self.halt(reason, 0, end);
                return Ok(());
            }
        }
        self.w.signature_ok = true;
        self.w.end = 8;
        self.w.stop_offset = 8;
        let mut pos = 8u64;
        let mut after_gap = false;

        loop {
            let hdr = match self.rd.read_array::<8>(pos) {
                Err(e) => return self.read_failed(pos, e),
                Ok(None) => {
                    self.stop_at_end(pos);
                    return Ok(());
                }
                Ok(Some(h)) => h,
            };
            let (mut len, kind) = parse_header(&hdr);
            let letters = kind.iter().all(u8::is_ascii_alphabetic);

            if !letters {
                // Not a chunk header at all: garbage between chunks.
                let to = self.rd.avail.min(pos.saturating_add(self.opts.gap_window));
                let found = if self.opts.resync {
                    self.scan_for_chunk(pos, to)
                } else {
                    None
                };
                match found {
                    Some(s) => {
                        self.bad(pos, format!("invalid chunk header at offset {pos}"));
                        self.w.gaps.push(Range::new(pos, s));
                        pos = s;
                        after_gap = true;
                        continue;
                    }
                    None => {
                        self.bad(pos, format!("invalid chunk header at offset {pos}"));
                        self.halt(StopReason::Damage, pos, pos);
                        return Ok(());
                    }
                }
            }

            if kind == *b"IHDR" && self.seen_ihdr {
                self.bad(pos, "second IHDR chunk (another image starts here?)".into());
                self.halt(StopReason::Damage, pos, pos);
                return Ok(());
            }
            if self.opts.stop_at_idat && kind == *b"IDAT" {
                self.halt(StopReason::Requested, pos, pos);
                return Ok(());
            }

            let mut repaired = false;
            let overruns = pos + 12 + len as u64 > self.rd.avail;
            if len > MAX_CHUNK_LEN || overruns {
                // A chunk that is merely longer than the caller's read limit
                // is not suspicious; one cut by the end of the data may be.
                let cut_by_limit = len <= MAX_CHUNK_LEN && self.rd.limited;
                if !cut_by_limit {
                    if let Some(l) =
                        self.resolve_length(pos, len, &kind, len > IMPLAUSIBLE_CHUNK_LEN)
                    {
                        len = l;
                        repaired = true;
                    }
                }
            }
            if len > MAX_CHUNK_LEN {
                // Unusable length and nothing to resynchronise on.
                self.bad(pos, format!("invalid length in {} chunk", name(&kind)));
                self.halt(StopReason::Damage, pos, pos);
                return Ok(());
            }

            let mut head = Vec::with_capacity(32);
            let mut v = self.verify(pos, len, &kind, &mut head);
            if v.complete && v.computed != v.stored {
                // Another file's signature inside the data: this image was
                // cut short and something else was written after it.
                if let Some(sig_at) = self.find_signature(pos + 8, len as u64 + 4) {
                    let got = sig_at - (pos + 8);
                    v = Verified {
                        complete: false,
                        got: got.min(len as u64),
                        computed: v.computed,
                        stored: 0,
                        err: None,
                    };
                    self.bad(
                        pos,
                        format!(
                            "another PNG signature at offset {sig_at} inside a {} chunk",
                            name(&kind)
                        ),
                    );
                    self.w.notes.push(format!(
                        "image data is cut at offset {sig_at}, where another PNG starts"
                    ));
                    let chunk = Chunk {
                        offset: pos,
                        len,
                        kind,
                        crc_ok: false,
                        data_offset: pos + 8,
                        crc_stored: 0,
                        crc_computed: v.computed,
                        complete: false,
                        after_gap,
                        length_repaired: repaired,
                    };
                    if kind == *b"IDAT" && v.got > 0 {
                        self.w.idat_ranges.push(Range::with_len(pos + 8, v.got));
                        self.w.idat_status.push((false, false));
                    }
                    self.w.chunks.push(chunk);
                    self.halt(StopReason::Damage, pos, sig_at);
                    return Ok(());
                }
            }
            if v.complete && v.computed != v.stored && !repaired {
                // A CRC failure followed by something that is not a header
                // suggests the length field is the damaged part.
                if let Header::Invalid = self.peek_header(pos + 12 + len as u64) {
                    if let Some(l) = self.resolve_length(pos, len, &kind, true) {
                        len = l;
                        repaired = true;
                        head.clear();
                        v = self.verify(pos, len, &kind, &mut head);
                    }
                }
            }

            let crc_ok = v.complete && v.computed == v.stored;
            let chunk = Chunk {
                offset: pos,
                len,
                kind,
                crc_ok,
                data_offset: pos + 8,
                crc_stored: v.stored,
                crc_computed: v.computed,
                complete: v.complete,
                after_gap,
                length_repaired: repaired,
            };
            after_gap = false;

            if !v.complete {
                // Cut short by the end of the data or an unreadable region.
                let data_end = pos + 8 + v.got;
                if kind == *b"IDAT" && v.got > 0 {
                    self.w.idat_ranges.push(Range::with_len(pos + 8, v.got));
                    self.w.idat_status.push((false, false));
                }
                self.w.chunks.push(chunk);
                match v.err {
                    Some(e) => {
                        self.read_failed(pos, e)?;
                        self.w.end = data_end;
                    }
                    None => {
                        let reason = self.end_reason();
                        let end = self.rd.avail;
                        self.halt(reason, pos, end);
                        self.bad(
                            pos,
                            format!("{} chunk runs past the end of the data", name(&kind)),
                        );
                    }
                }
                return Ok(());
            }

            // Bookkeeping for a fully read chunk.
            if self.w.chunks.is_empty() {
                if kind == *b"CgBI" {
                    self.w.cgbi = true;
                } else if kind != *b"IHDR" {
                    self.bad(pos, format!("first chunk is {}, not IHDR", name(&kind)));
                }
            } else if self.w.cgbi && self.w.chunks.len() == 1 && kind != *b"IHDR" {
                self.bad(
                    pos,
                    format!("expected IHDR after CgBI, found {}", name(&kind)),
                );
            }
            if kind == *b"IHDR" {
                self.seen_ihdr = true;
                if len as usize == Ihdr::LEN && self.w.ihdr.is_none() {
                    self.w.ihdr = Ihdr::parse(&head[..Ihdr::LEN.min(head.len())]);
                }
            }
            if !crc_ok {
                self.bad(pos, format!("CRC mismatch in {} chunk", name(&kind)));
            }
            if repaired {
                self.w.notes.push(format!(
                    "{} chunk at offset {pos}: damaged length field, using {len}",
                    name(&kind)
                ));
            }
            if kind == *b"IDAT" {
                self.w.idat_ranges.push(chunk.data_range());
                self.w.idat_status.push((crc_ok, true));
            }
            let end = chunk.end();
            self.w.chunks.push(chunk);
            self.w.end = end;
            if kind == *b"IEND" {
                self.w.iend_offset = Some(pos);
                self.halt(StopReason::Iend, pos, end);
                return Ok(());
            }
            pos = end;
        }
    }
}

fn name(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::chunk_crc;
    use diskrescue_core::source::MemSource;

    fn chunk_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&chunk_crc(kind, data).to_be_bytes());
        v
    }

    fn ihdr_data() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&4u32.to_be_bytes());
        d.extend_from_slice(&4u32.to_be_bytes());
        d.extend_from_slice(&[8, 0, 0, 0, 0]);
        d
    }

    fn sample(idats: usize) -> Vec<u8> {
        let mut v = SIGNATURE.to_vec();
        v.extend(chunk_bytes(b"IHDR", &ihdr_data()));
        v.extend(chunk_bytes(b"tEXt", b"Software\0test"));
        for i in 0..idats {
            let data: Vec<u8> = (0..300).map(|j| (i * 7 + j) as u8).collect();
            v.extend(chunk_bytes(b"IDAT", &data));
        }
        v.extend(chunk_bytes(b"IEND", &[]));
        v
    }

    fn walk_bytes(b: &[u8]) -> ChunkWalk {
        walk(&MemSource::new(b.to_vec()), 0, u64::MAX).unwrap()
    }

    #[test]
    fn clean_file() {
        let b = sample(3);
        let w = walk_bytes(&b);
        assert!(w.signature_ok);
        assert_eq!(w.chunks.len(), 6);
        assert!(w.chunks.iter().all(|c| c.crc_ok && c.complete));
        assert_eq!(w.stop, StopReason::Iend);
        assert_eq!(w.end, b.len() as u64);
        assert_eq!(w.iend_offset, Some(b.len() as u64 - 12));
        assert_eq!(w.idat_ranges.len(), 3);
        assert_eq!(w.ihdr.unwrap().width, 4);
        assert!(w.first_bad.is_none());
        assert!(w.critical_crc_ok());
    }

    #[test]
    fn stops_at_iend_ignoring_trailing_data() {
        let mut b = sample(1);
        let len = b.len();
        b.extend_from_slice(&[0xAB; 5000]);
        let w = walk_bytes(&b);
        assert_eq!(w.end, len as u64);
        assert_eq!(w.stop, StopReason::Iend);
    }

    #[test]
    fn missing_iend_and_cut_chunks() {
        let b = sample(2);
        // Remove IEND exactly.
        let w = walk_bytes(&b[..b.len() - 12]);
        assert!(w.iend_offset.is_none());
        assert_eq!(w.stop, StopReason::SourceEnd);
        assert_eq!(w.end, b.len() as u64 - 12);
        assert!(w.first_bad.is_some());
        // Cut in the middle of the second IDAT.
        let cut = b.len() - 12 - 100;
        let w = walk_bytes(&b[..cut]);
        assert_eq!(w.end, cut as u64);
        assert!(!w.chunks.last().unwrap().complete);
        assert_eq!(w.stop, StopReason::SourceEnd);
        // Cut inside a header.
        let w = walk_bytes(&b[..8 + 12 + 13 + 3]);
        assert_eq!(w.end, (8 + 12 + 13 + 3) as u64);
    }

    #[test]
    fn limit_is_reported() {
        let b = sample(2);
        let src = MemSource::new(b.clone());
        let w = walk(&src, 0, 100).unwrap();
        assert_eq!(w.stop, StopReason::Limit);
        assert_eq!(w.end, 100);
        // Limit equal to the file length is not a limit hit.
        let w = walk(&src, 0, b.len() as u64).unwrap();
        assert_eq!(w.stop, StopReason::Iend);
    }

    #[test]
    fn bad_crc_is_recorded_and_walk_continues() {
        let mut b = sample(3);
        // Flip a byte inside the second IDAT's data.
        let w0 = walk_bytes(&b);
        let target = w0.chunks[3];
        b[target.data_offset as usize + 10] ^= 0x55;
        let w = walk_bytes(&b);
        assert_eq!(w.chunks.len(), 6);
        assert!(!w.chunks[3].crc_ok);
        assert!(w.chunks[4].crc_ok);
        assert_eq!(w.stop, StopReason::Iend);
        assert!(!w.critical_crc_ok());
        assert_eq!(w.first_bad.as_ref().unwrap().0, target.offset);
    }

    #[test]
    fn garbage_between_chunks_is_skipped() {
        let clean = sample(3);
        let w0 = walk_bytes(&clean);
        let at = w0.chunks[3].offset as usize;
        let mut b = clean[..at].to_vec();
        b.extend((0..777u32).map(|i| (i * 131 % 251) as u8 | 0x80));
        b.extend_from_slice(&clean[at..]);
        let w = walk_bytes(&b);
        assert_eq!(w.stop, StopReason::Iend);
        assert_eq!(w.gaps.len(), 1);
        assert_eq!(w.gaps[0], Range::new(at as u64, at as u64 + 777));
        assert!(w.chunks[3].after_gap);
        assert_eq!(w.chunks.len(), 6);
        assert!(w.chunks.iter().all(|c| c.crc_ok));
        // Without resync the walk stops at the garbage.
        let src = MemSource::new(b);
        let opts = WalkOptions {
            resync: false,
            ..Default::default()
        };
        let w = walk_with(&src, 0, u64::MAX, &opts).unwrap();
        assert_eq!(w.stop, StopReason::Damage);
        assert_eq!(w.end, at as u64);
    }

    #[test]
    fn damaged_length_is_recovered_by_crc() {
        let clean = sample(3);
        let w0 = walk_bytes(&clean);
        let c = w0.chunks[3];
        let mut b = clean.clone();
        // Make the length larger (still within the file).
        let bad_len = c.len + 200;
        b[c.offset as usize..c.offset as usize + 4].copy_from_slice(&bad_len.to_be_bytes());
        let w = walk_bytes(&b);
        assert_eq!(w.chunks.len(), 6, "{:?}", w.notes);
        assert!(w.chunks[3].length_repaired);
        assert_eq!(w.chunks[3].len, c.len);
        assert!(w.chunks[3].crc_ok);
        assert_eq!(w.stop, StopReason::Iend);
        // Make it enormous.
        let mut b = clean.clone();
        b[c.offset as usize..c.offset as usize + 4].copy_from_slice(&0x7FFF_0000u32.to_be_bytes());
        let w = walk_bytes(&b);
        assert_eq!(w.chunks.len(), 6);
        assert_eq!(w.chunks[3].len, c.len);
        // Shrink it.
        let mut b = clean.clone();
        b[c.offset as usize..c.offset as usize + 4].copy_from_slice(&(c.len - 100).to_be_bytes());
        let w = walk_bytes(&b);
        assert_eq!(w.stop, StopReason::Iend);
        assert_eq!(w.chunks[3].len, c.len);
    }

    #[test]
    fn damaged_length_and_data_resyncs_on_next_chunk() {
        let clean = sample(3);
        let w0 = walk_bytes(&clean);
        let c = w0.chunks[3];
        let mut b = clean.clone();
        b[c.offset as usize..c.offset as usize + 4].copy_from_slice(&0x7000_0000u32.to_be_bytes());
        b[c.data_offset as usize + 5] ^= 0xFF;
        let w = walk_bytes(&b);
        assert_eq!(w.stop, StopReason::Iend);
        assert_eq!(w.chunks.len(), 6);
        assert!(!w.chunks[3].crc_ok);
        assert_eq!(w.chunks[3].len, c.len);
        assert!(w.chunks[3].length_repaired);
    }

    #[test]
    fn does_not_resync_into_another_png() {
        let first = sample(2);
        let second = sample(2);
        let cut = first.len() - 12 - 150;
        let mut b = first[..cut].to_vec();
        b.extend_from_slice(&second);
        let w = walk_bytes(&b);
        // The first file's last IDAT is cut and then swallows the start of
        // the second file; the walk must not hop onto the second image's
        // chunks and report an IEND from there.
        assert_ne!(w.stop, StopReason::Iend, "{:?}", w.chunks);
    }

    #[test]
    fn signature_and_tiny_inputs() {
        assert!(!walk_bytes(b"").signature_ok);
        assert!(!walk_bytes(&SIGNATURE[..5]).signature_ok);
        let w = walk_bytes(&SIGNATURE);
        assert!(w.signature_ok);
        assert!(w.chunks.is_empty());
        assert_eq!(w.stop, StopReason::SourceEnd);
        let w = walk_bytes(b"GIF89a-not-a-png-file");
        assert!(!w.signature_ok);
    }

    #[test]
    fn second_ihdr_stops_the_walk() {
        let mut b = SIGNATURE.to_vec();
        b.extend(chunk_bytes(b"IHDR", &ihdr_data()));
        b.extend(chunk_bytes(b"IHDR", &ihdr_data()));
        let w = walk_bytes(&b);
        assert_eq!(w.chunks.len(), 1);
        assert_eq!(w.stop, StopReason::Damage);
    }

    #[test]
    fn cgbi_before_ihdr_is_accepted() {
        let mut b = SIGNATURE.to_vec();
        b.extend(chunk_bytes(b"CgBI", &[0, 0, 0, 0]));
        b.extend(chunk_bytes(b"IHDR", &ihdr_data()));
        b.extend(chunk_bytes(b"IEND", &[]));
        let w = walk_bytes(&b);
        assert!(w.cgbi);
        assert!(w.ihdr.is_some());
        assert!(w.first_bad.is_none());
    }

    #[test]
    fn stop_at_idat_reads_only_headers() {
        let b = sample(2);
        let src = MemSource::new(b);
        let opts = WalkOptions {
            stop_at_idat: true,
            ..Default::default()
        };
        let w = walk_with(&src, 0, u64::MAX, &opts).unwrap();
        assert_eq!(w.stop, StopReason::Requested);
        assert_eq!(w.chunks.len(), 2);
        assert!(w.ihdr.is_some());
    }

    #[test]
    fn unreadable_region_is_damage_not_error() {
        let b = sample(3);
        let w0 = walk_bytes(&b);
        let c = w0.chunks[3];
        let mut bad = diskrescue_core::RangeSet::new();
        bad.insert(Range::new(c.data_offset + 100, c.data_offset + 200));
        let src = MemSource::new(b).with_unreadable(bad);
        let w = walk(&src, 0, u64::MAX).unwrap();
        assert_eq!(w.stop, StopReason::Damage);
        assert!(w.end <= c.data_offset + 100);
        assert!(!w.chunks.last().unwrap().complete);
    }

    #[test]
    fn start_offset_is_respected() {
        let b = sample(1);
        let mut padded = vec![0x11u8; 1000];
        padded.extend_from_slice(&b);
        padded.extend_from_slice(&[0x22; 100]);
        let src = MemSource::new(padded);
        let w = walk(&src, 1000, u64::MAX).unwrap();
        assert_eq!(w.start, 1000);
        assert_eq!(w.end, b.len() as u64);
        assert_eq!(w.chunks[0].offset, 8);
    }
}
