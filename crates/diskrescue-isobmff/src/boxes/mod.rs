//! ISO BMFF box layer: headers, damage-tolerant walking, the `moov` tree and
//! the sample-table arithmetic.
//!
//! # Algorithm
//!
//! A box is `u32 size, 4CC type`, optionally followed by a 64-bit
//! `largesize` (when `size == 1`) and a 16-byte user type (when the type is
//! `uuid`). `size == 0` means "to the end of the enclosing region". Nothing
//! read from the medium is trusted: every walk is bounded by its parent's
//! end, by the source end and by [`MAX_DEPTH`]; a child whose header is
//! impossible (size smaller than its own header) stops the walk of that
//! parent without an error, so callers see every box that was intact.
//!
//! The `moov` box is read into memory in one bounded read (it is an index,
//! typically kilobytes to a few megabytes; the cap is [`MAX_MOOV_SIZE`]) and
//! parsed from the slice by [`parse_moov`] / [`parse_moov_bytes`]. Media
//! data (`mdat`) is never loaded.
//!
//! # Limits
//!
//! * `stz2` (compact sample sizes) is supported for reading; `sdtp`, `sgpd`,
//!   `sbgp`, `cslg`, `stps` are recognised but not interpreted.
//! * Fragmented files (`moof`) are recognised by the top-level walkers in
//!   the format modules; `parse_moov` only reports `mvex` presence.

use std::ops::ControlFlow;

use diskrescue_core::util::bytes::{u32be, u64be};
use diskrescue_core::{Error, Result, Source, SourceExt};

mod meta;
mod moov;
mod table;

pub use meta::{parse_ilst_cover, CoverArt, MetaInfo};
pub use moov::*;
pub use table::{sample_table, sample_table_checked, Sample, TableCheck, MAX_SAMPLES};

/// Maximum nesting depth followed by any recursive walk.
pub const MAX_DEPTH: usize = 32;

/// Largest `moov` (or HEIF `meta`) box that is loaded into memory.
pub const MAX_MOOV_SIZE: u64 = 256 << 20;

/// Header of one box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    /// Absolute offset of the first header byte.
    pub offset: u64,
    /// Total size including the header; `0` means "extends to the end of the
    /// enclosing region" (see [`BoxHeader::end`]).
    pub size: u64,
    /// Length of the size/type part of the header: 8, or 16 when a 64-bit
    /// `largesize` is present. The 16-byte `uuid` user type, when present,
    /// is *not* included; use [`BoxHeader::payload_offset`].
    pub header_len: u8,
    /// Four-character type.
    pub kind: [u8; 4],
    /// Extended type of a `uuid` box.
    pub uuid: Option<[u8; 16]>,
}

impl BoxHeader {
    /// Bytes of header before the payload (size/type, largesize, uuid).
    pub fn full_header_len(&self) -> u64 {
        self.header_len as u64 + if self.uuid.is_some() { 16 } else { 0 }
    }

    /// Absolute offset of the first payload byte.
    pub fn payload_offset(&self) -> u64 {
        self.offset.saturating_add(self.full_header_len())
    }

    /// Absolute end offset, resolving `size == 0` against `region_end`.
    pub fn end(&self, region_end: u64) -> u64 {
        if self.size == 0 {
            region_end.max(self.payload_offset())
        } else {
            self.offset.saturating_add(self.size)
        }
    }

    /// Payload length, resolving `size == 0` against `region_end`.
    pub fn payload_len(&self, region_end: u64) -> u64 {
        self.end(region_end).saturating_sub(self.payload_offset())
    }

    /// True when the type equals `k`.
    pub fn is(&self, k: &[u8; 4]) -> bool {
        &self.kind == k
    }

    /// Printable form of the type (`moov`, `©mak`, `0x00000001`).
    pub fn kind_str(&self) -> String {
        fourcc_str(&self.kind)
    }
}

/// Printable form of a 4CC; non-printable codes are shown as hex.
pub fn fourcc_str(k: &[u8; 4]) -> String {
    if k.iter().all(|&c| (0x20..0x7F).contains(&c) || c == 0xA9) {
        k.iter()
            .map(|&c| if c == 0xA9 { '©' } else { c as char })
            .collect()
    } else {
        format!("0x{:08x}", u32::from_be_bytes(*k))
    }
}

/// True when every byte of the 4CC is printable ASCII (or `©`, used by
/// QuickTime user-data atoms).
pub fn is_printable_fourcc(k: &[u8; 4]) -> bool {
    k.iter().all(|&c| (0x20..0x7F).contains(&c) || c == 0xA9)
}

/// Top-level box types accepted by the carving walkers.
pub const TOP_LEVEL_TYPES: &[&[u8; 4]] = &[
    b"ftyp", b"free", b"skip", b"wide", b"mdat", b"moov", b"moof", b"mfra", b"meta", b"uuid",
    b"sidx", b"styp", b"pdin", b"udta", b"junk", b"PICT", b"prfl", b"pnot", b"idat", b"jumb",
    b"ssix", b"prft", b"emsg", b"pict", b"pitm",
];

/// True when `k` is a known top-level box type.
pub fn is_top_level_type(k: &[u8; 4]) -> bool {
    TOP_LEVEL_TYPES.contains(&k)
}

/// Parse a box header from the start of `buf` (absolute position `offset`).
///
/// Returns `None` when `buf` is too short for the header or the size field
/// is impossible (non-zero and smaller than the header). The box may extend
/// beyond `buf`; callers check that against their region.
pub fn parse_header(buf: &[u8], offset: u64) -> Option<BoxHeader> {
    let size32 = u32be(buf, 0)?;
    let kind: [u8; 4] = buf.get(4..8)?.try_into().ok()?;
    let (size, header_len) = match size32 {
        1 => {
            let large = u64be(buf, 8)?;
            if large < 16 {
                return None;
            }
            (large, 16u8)
        }
        0 => (0, 8u8),
        s => (s as u64, 8u8),
    };
    let uuid = if &kind == b"uuid" {
        let at = header_len as usize;
        let u: [u8; 16] = buf.get(at..at + 16)?.try_into().ok()?;
        Some(u)
    } else {
        None
    };
    let full = header_len as u64 + if uuid.is_some() { 16 } else { 0 };
    if size != 0 && size < full {
        return None;
    }
    Some(BoxHeader {
        offset,
        size,
        header_len,
        kind,
        uuid,
    })
}

/// Read the box header at `offset` from `src`. `limit` is the absolute end of
/// the enclosing region (the parent's end or the file end).
///
/// Returns `Ok(None)` when fewer than 8 bytes remain before `limit` or the
/// header is impossible. A box whose claimed size runs past `limit` is still
/// returned: the caller decides whether that means truncation.
pub fn read_header(src: &dyn Source, offset: u64, limit: u64) -> Result<Option<BoxHeader>> {
    let limit = limit.min(src.size());
    if offset >= limit || limit - offset < 8 {
        return Ok(None);
    }
    let want = (limit - offset).min(32) as usize;
    let buf = src.read_vec(offset, want)?;
    Ok(parse_header(&buf, offset))
}

/// Offset (relative to the payload start) where the child boxes of a
/// container start, or `None` when the type is not a known container.
///
/// `meta` is a FullBox in ISO files but a plain box in some QuickTime
/// files; `first_payload` (the first 12 payload bytes, when available)
/// disambiguates by looking for a child header.
pub fn children_skip(kind: &[u8; 4], first_payload: &[u8]) -> Option<u64> {
    match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"udta" | b"dinf" | b"mvex"
        | b"moof" | b"traf" | b"mfra" | b"tref" | b"iprp" | b"ipco" | b"ilst" | b"gmhd"
        | b"sinf" | b"schi" | b"wave" | b"tapt" | b"clip" | b"matt" | b"rmra" | b"CNTH" => Some(0),
        b"meta" => {
            // QuickTime-style meta: a child header starts right away.
            if let Some(k) = first_payload.get(4..8) {
                if k == b"hdlr" || k == b"keys" || k == b"ilst" {
                    return Some(0);
                }
            }
            Some(4)
        }
        b"dref" | b"stsd" => Some(8),
        b"iinf" => Some(4), // FullBox; entry count width depends on version (handled by caller)
        _ => None,
    }
}

/// Visit the direct children of `parent`, in order.
///
/// The walk stops (without error) at the first child whose header is
/// impossible, at the parent's end, after 1 000 000 children, or when `f`
/// returns [`ControlFlow::Break`]. Children whose claimed size runs past the
/// parent are reported once with their claimed size and end the walk.
/// Read errors are returned.
pub fn walk_children(
    src: &dyn Source,
    parent: &BoxHeader,
    f: &mut dyn FnMut(&BoxHeader) -> ControlFlow<()>,
) -> Result<()> {
    let region_end = parent.end(src.size()).min(src.size());
    let head = src.read_vec(parent.payload_offset(), 12)?;
    let skip = match children_skip(&parent.kind, &head) {
        Some(s) => s,
        None => {
            return Err(Error::unsupported(format!(
                "box '{}' is not a known container",
                parent.kind_str()
            )))
        }
    };
    let mut pos = parent.payload_offset().saturating_add(skip);
    let mut count = 0u32;
    while pos < region_end && count < 1_000_000 {
        let Some(h) = read_header(src, pos, region_end)? else {
            break;
        };
        count += 1;
        let end = h.end(region_end);
        if f(&h).is_break() {
            break;
        }
        if end > region_end || end <= pos {
            break;
        }
        pos = end;
    }
    Ok(())
}

/// Visit every box below `root` depth-first (pre-order), following known
/// containers up to [`MAX_DEPTH`] levels. `f` receives the depth (0 for the
/// direct children of `root`).
pub fn walk_tree(
    src: &dyn Source,
    root: &BoxHeader,
    f: &mut dyn FnMut(&BoxHeader, usize) -> ControlFlow<()>,
) -> Result<()> {
    fn rec(
        src: &dyn Source,
        parent: &BoxHeader,
        depth: usize,
        f: &mut dyn FnMut(&BoxHeader, usize) -> ControlFlow<()>,
        stop: &mut bool,
    ) -> Result<()> {
        if depth >= MAX_DEPTH || *stop {
            return Ok(());
        }
        let mut kids = Vec::new();
        walk_children(src, parent, &mut |h| {
            kids.push(*h);
            ControlFlow::Continue(())
        })?;
        for k in kids {
            if f(&k, depth).is_break() {
                *stop = true;
                return Ok(());
            }
            let head = src.read_vec(k.payload_offset(), 12)?;
            if children_skip(&k.kind, &head).is_some() {
                rec(src, &k, depth + 1, f, stop)?;
                if *stop {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
    let mut stop = false;
    rec(src, root, 0, f, &mut stop)
}

/// FullBox `version` and 24-bit `flags` from the first four payload bytes.
pub fn full_box_header(payload: &[u8]) -> Option<(u8, u32)> {
    let v = u32be(payload, 0)?;
    Some(((v >> 24) as u8, v & 0x00FF_FFFF))
}

/// A box found inside an in-memory buffer.
#[derive(Debug, Clone, Copy)]
pub struct SliceBox<'a> {
    /// Header (offsets are absolute: `base` + position in the buffer).
    pub header: BoxHeader,
    /// Payload bytes, clipped to the buffer (shorter than claimed when the
    /// box is truncated).
    pub payload: &'a [u8],
    /// The whole box (header and payload), clipped to the buffer.
    pub raw: &'a [u8],
    /// True when the claimed size ran past the buffer.
    pub truncated: bool,
}

/// Iterator over the boxes of an in-memory region. Stops at the first
/// impossible header; a truncated last box is yielded with `truncated`
/// set. At most 1 000 000 boxes are yielded.
pub struct BoxIter<'a> {
    buf: &'a [u8],
    pos: usize,
    base: u64,
    count: u32,
    done: bool,
}

impl<'a> BoxIter<'a> {
    /// Iterate `buf`, whose first byte lives at absolute offset `base`.
    pub fn new(buf: &'a [u8], base: u64) -> Self {
        BoxIter {
            buf,
            pos: 0,
            base,
            count: 0,
            done: false,
        }
    }

    /// Position (relative to the buffer) where iteration stopped or will
    /// continue.
    pub fn position(&self) -> usize {
        self.pos
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = SliceBox<'a>;

    fn next(&mut self) -> Option<SliceBox<'a>> {
        if self.done || self.pos + 8 > self.buf.len() || self.count >= 1_000_000 {
            return None;
        }
        let rest = &self.buf[self.pos..];
        let Some(h) = parse_header(rest, self.base + self.pos as u64) else {
            self.done = true;
            return None;
        };
        self.count += 1;
        let hl = h.full_header_len() as usize;
        let claimed = if h.size == 0 {
            rest.len() as u64
        } else {
            h.size
        };
        let (payload, raw, truncated) = if claimed > rest.len() as u64 {
            (&rest[hl.min(rest.len())..], rest, true)
        } else {
            let c = claimed as usize;
            (&rest[hl.min(c)..c], &rest[..c], false)
        };
        if truncated {
            self.done = true;
            self.pos = self.buf.len();
        } else {
            self.pos += claimed as usize;
        }
        Some(SliceBox {
            header: h,
            payload,
            raw,
            truncated,
        })
    }
}

/// Find the first child of type `kind` in an in-memory region.
pub fn find_child<'a>(buf: &'a [u8], base: u64, kind: &[u8; 4]) -> Option<SliceBox<'a>> {
    BoxIter::new(buf, base).find(|b| &b.header.kind == kind)
}

/// Read a whole box (header excluded) into memory, refusing boxes larger
/// than `cap`. A box that runs past the source end is read up to the end.
pub fn read_payload(src: &dyn Source, h: &BoxHeader, region_end: u64, cap: u64) -> Result<Vec<u8>> {
    let len = h.payload_len(region_end.min(src.size()));
    if len > cap {
        return Err(Error::unsupported(format!(
            "'{}' box of {} bytes exceeds the {} byte limit",
            h.kind_str(),
            len,
            cap
        )));
    }
    Ok(src.read_vec(h.payload_offset(), len as usize)?)
}

/// One top-level box seen by [`walk_top_level`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopBox {
    /// The header.
    pub header: BoxHeader,
    /// Resolved end (absolute), possibly beyond the region when truncated.
    pub end: u64,
    /// The claimed end lies beyond the region end.
    pub truncated: bool,
}

/// How a top-level walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopStop {
    /// The last box ended exactly at the region end.
    End,
    /// Fewer than 8 bytes remained (or an impossible header was found) at
    /// this absolute offset.
    Garbage(u64),
    /// A box of unknown type was found at this offset.
    UnknownType(u64),
    /// A second `ftyp` (the next file) starts at this offset.
    NextFile(u64),
    /// The last box claims to run past the region.
    Truncated,
    /// Too many boxes.
    TooMany(u64),
}

/// Walk top-level boxes from `start` to `region_end`, accepting only types
/// for which `accept` returns true. Bounded to 100 000 boxes.
pub fn walk_top_level(
    src: &dyn Source,
    start: u64,
    region_end: u64,
    accept: &dyn Fn(&[u8; 4]) -> bool,
) -> Result<(Vec<TopBox>, TopStop)> {
    let region_end = region_end.min(src.size());
    let mut pos = start;
    let mut out: Vec<TopBox> = Vec::new();
    loop {
        if pos == region_end {
            return Ok((out, TopStop::End));
        }
        if out.len() >= 100_000 {
            return Ok((out, TopStop::TooMany(pos)));
        }
        let Some(h) = read_header(src, pos, region_end)? else {
            return Ok((out, TopStop::Garbage(pos)));
        };
        if !accept(&h.kind) {
            return Ok((out, TopStop::UnknownType(pos)));
        }
        if !out.is_empty() && h.is(b"ftyp") {
            return Ok((out, TopStop::NextFile(pos)));
        }
        let end = h.end(region_end);
        let truncated = end > region_end;
        out.push(TopBox {
            header: h,
            end,
            truncated,
        });
        if truncated {
            return Ok((out, TopStop::Truncated));
        }
        pos = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn header_forms() {
        let h = parse_header(&bx(b"free", &[0; 4]), 10).unwrap();
        assert_eq!(h.size, 12);
        assert_eq!(h.payload_offset(), 18);
        let mut large = 1u32.to_be_bytes().to_vec();
        large.extend_from_slice(b"mdat");
        large.extend_from_slice(&40u64.to_be_bytes());
        let h = parse_header(&large, 0).unwrap();
        assert_eq!((h.size, h.header_len), (40, 16));
        let mut u = 40u32.to_be_bytes().to_vec();
        u.extend_from_slice(b"uuid");
        u.extend_from_slice(&[7; 16]);
        let h = parse_header(&u, 0).unwrap();
        assert_eq!(h.uuid, Some([7; 16]));
        assert_eq!(h.payload_offset(), 24);
        assert!(parse_header(&[0, 0, 0, 4, b'f', b'r', b'e', b'e'], 0).is_none());
        let zero = parse_header(&[0, 0, 0, 0, b'm', b'd', b'a', b't'], 0).unwrap();
        assert_eq!(zero.end(100), 100);
    }

    #[test]
    fn walking_children_and_tree() {
        let inner = [bx(b"mvhd", &[0; 8]), bx(b"trak", &bx(b"tkhd", &[1; 4]))].concat();
        let moov = bx(b"moov", &inner);
        let src = MemSource::new(moov.clone());
        let root = read_header(&src, 0, moov.len() as u64).unwrap().unwrap();
        let mut kinds = Vec::new();
        walk_tree(&src, &root, &mut |h, d| {
            kinds.push((h.kind_str(), d));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(
            kinds,
            vec![("mvhd".into(), 0), ("trak".into(), 0), ("tkhd".into(), 1)]
        );
        let it: Vec<_> = BoxIter::new(&inner, 8).map(|b| b.header.kind).collect();
        assert_eq!(it, vec![*b"mvhd", *b"trak"]);
    }

    #[test]
    fn nesting_bomb_is_bounded() {
        let mut b = bx(b"free", &[]);
        for _ in 0..200 {
            b = bx(b"trak", &b);
        }
        let src = MemSource::new(b.clone());
        let root = read_header(&src, 0, b.len() as u64).unwrap().unwrap();
        let mut max_depth = 0;
        walk_tree(&src, &root, &mut |_, d| {
            max_depth = max_depth.max(d);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert!(max_depth < MAX_DEPTH);
    }
}
