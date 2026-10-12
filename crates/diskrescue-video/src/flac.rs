//! FLAC carving, validation and metadata.
//!
//! # Algorithm
//!
//! * `fLaC`, then metadata blocks (type, last-block flag, 24-bit length).
//!   STREAMINFO must come first and gives sample rate, channels, bits per
//!   sample, the total number of samples and frame/block size bounds.
//!   `VORBIS_COMMENT` gives tags; `PICTURE` blocks are reported as nested
//!   `CoverArt` (JPEG or PNG).
//! * Frames follow. A frame has no length field, so the walker *skips
//!   through* it: the header is parsed and checked with its CRC-8, then every
//!   subframe is stepped over (constant, verbatim, fixed and LPC predictors,
//!   Rice-coded residuals) without reconstructing samples, which gives the
//!   exact frame length. The frame CRC-16 is then verified over the bytes.
//!   Frame numbers (fixed block size) or sample numbers (variable block size)
//!   must be consecutive.
//! * When STREAMINFO states the total number of samples the walk stops as
//!   soon as that many samples have been seen and the status is `Complete`
//!   (so a second FLAC file directly behind the first is not swallowed).
//!   Without a total (live encoders write 0) the walk continues until no
//!   valid frame follows and the status is `UnknownLength`. A stream that
//!   ends before reaching its total is `Truncated`.
//! * A damaged frame is bridged: the walker searches (up to 256 KiB) for the
//!   next header that passes its CRC-8 and continues the numbering; the
//!   number of bridged frames is reported in the notes and lowers the
//!   confidence.
//!
//! The MD5 of the decoded audio in STREAMINFO is not verified (that needs a
//! full decoder).

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Range, Result, SharedSource};

use crate::io::{note_unreadable, Window};
use crate::meta::{crc16_update, crc8, parse_loose_date, EXTENSION_KEY};

const MAX_SIZE: u64 = 2 << 30;
const MAX_METADATA_BLOCKS: usize = 4096;
/// Initial attempt size when stepping through a frame.
const FRAME_FIRST_READ: usize = 32 * 1024;
/// Largest frame accepted (8 channels x 65535 samples x 32 bit is 2 MiB).
const FRAME_MAX_READ: usize = 4 * 1024 * 1024;
/// How far to look for the next header after a damaged frame.
const RESYNC_SCAN: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default)]
struct StreamInfo {
    min_block: u32,
    max_block: u32,
    min_frame: u32,
    max_frame: u32,
    sample_rate: u32,
    channels: u8,
    bps: u8,
    total_samples: u64,
}

fn parse_streaminfo(b: &[u8]) -> Option<StreamInfo> {
    if b.len() < 34 {
        return None;
    }
    let v = u64::from_be_bytes(b[10..18].try_into().ok()?);
    let si = StreamInfo {
        min_block: u32::from(u16::from_be_bytes([b[0], b[1]])),
        max_block: u32::from(u16::from_be_bytes([b[2], b[3]])),
        min_frame: (u32::from(b[4]) << 16) | (u32::from(b[5]) << 8) | u32::from(b[6]),
        max_frame: (u32::from(b[7]) << 16) | (u32::from(b[8]) << 8) | u32::from(b[9]),
        sample_rate: (v >> 44) as u32,
        channels: (((v >> 41) & 7) + 1) as u8,
        bps: (((v >> 36) & 31) + 1) as u8,
        total_samples: v & ((1u64 << 36) - 1),
    };
    // Block sizes below 16 are illegal except for a single short block.
    if si.max_block != 0 && si.min_block > si.max_block {
        return None;
    }
    if si.sample_rate == 0 || si.sample_rate > 655_350 {
        return None;
    }
    if si.max_frame != 0 && si.min_frame > si.max_frame {
        return None;
    }
    Some(si)
}

#[derive(Debug, Default)]
struct Metadata {
    info: StreamInfo,
    /// Offset of the first frame.
    end: u64,
    tags: Vec<(String, String)>,
    covers: Vec<Nested>,
    blocks: usize,
}

fn be32(b: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize)
}

fn parse_picture(win: &mut Window<'_>, pos: u64, len: u64) -> Option<Nested> {
    let head = win.vec(pos, len.min(2048) as usize)?;
    let mime_len = be32(&head, 4)?;
    let mime = head.get(8..8 + mime_len)?;
    let mut o = 8 + mime_len;
    let desc_len = be32(&head, o)?;
    o += 4 + desc_len;
    o += 16; // width, height, depth, colours
    let data_len = be32(&head, o)? as u64;
    o += 4;
    let start = pos + o as u64;
    let end = start.checked_add(data_len)?;
    if end > pos + len || data_len == 0 {
        return None;
    }
    let mime = String::from_utf8_lossy(mime).to_ascii_lowercase();
    let magic = win.get(start, 4).to_vec();
    let fmt = if mime.contains("jpeg") || mime.contains("jpg") || magic.starts_with(&[0xFF, 0xD8]) {
        "jpeg"
    } else if mime.contains("png") || magic.starts_with(&[0x89, b'P']) {
        "png"
    } else {
        return None;
    };
    Some(Nested {
        range: Range::new(start, end),
        format: Some(fmt.to_string()),
        role: NestedRole::CoverArt,
    })
}

fn parse_comments(b: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(vlen) = b
        .get(0..4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
    else {
        return out;
    };
    let mut o = 4usize.saturating_add(vlen);
    let Some(n) = b
        .get(o..o + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
    else {
        return out;
    };
    o += 4;
    for _ in 0..n.min(256) {
        let Some(l) = b
            .get(o..o + 4)
            .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize)
        else {
            break;
        };
        o += 4;
        let Some(s) = b.get(o..o.saturating_add(l)) else {
            break;
        };
        o += l;
        let s = String::from_utf8_lossy(s);
        if let Some((k, v)) = s.split_once('=') {
            out.push((k.to_ascii_uppercase(), v.chars().take(200).collect()));
        }
    }
    out
}

fn parse_metadata(win: &mut Window<'_>) -> Option<Metadata> {
    if win.exact(0, 4)? != b"fLaC" {
        return None;
    }
    let mut md = Metadata::default();
    let mut pos = 4u64;
    for i in 0..MAX_METADATA_BLOCKS {
        let h = win.vec(pos, 4)?;
        let last = h[0] & 0x80 != 0;
        let ty = h[0] & 0x7F;
        let len = (u64::from(h[1]) << 16) | (u64::from(h[2]) << 8) | u64::from(h[3]);
        if ty == 127 {
            return None;
        }
        let data = pos + 4;
        if data + len > win.avail() {
            return None;
        }
        if i == 0 {
            if ty != 0 || len != 34 {
                return None;
            }
            md.info = parse_streaminfo(&win.vec(data, 34)?)?;
        } else if ty == 0 {
            return None; // a second STREAMINFO
        }
        match ty {
            4 => {
                if let Some(b) = win.vec(data, len.min(64 * 1024) as usize) {
                    md.tags = parse_comments(&b);
                }
            }
            6 => {
                if let Some(n) = parse_picture(win, data, len) {
                    md.covers.push(n);
                }
            }
            _ => {}
        }
        md.blocks += 1;
        pos = data + len;
        if last {
            md.end = pos;
            return Some(md);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Frame headers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct FrameHeader {
    len: usize,
    variable: bool,
    block_size: u32,
    channels: u8,
    assign: u8,
    bps: u8,
    number: u64,
}

fn utf8_number(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let n = if first < 0x80 {
        1
    } else if first & 0xE0 == 0xC0 {
        2
    } else if first & 0xF0 == 0xE0 {
        3
    } else if first & 0xF8 == 0xF0 {
        4
    } else if first & 0xFC == 0xF8 {
        5
    } else if first & 0xFE == 0xFC {
        6
    } else if first == 0xFE {
        7
    } else {
        return None;
    };
    let mut v: u64 = if n == 1 {
        u64::from(first)
    } else if n == 7 {
        0
    } else {
        u64::from(first & (0x7F >> n))
    };
    for k in 1..n {
        let c = *b.get(k)?;
        if c & 0xC0 != 0x80 {
            return None;
        }
        v = (v << 6) | u64::from(c & 0x3F);
    }
    Some((v, n))
}

fn parse_frame_header(b: &[u8], si: &StreamInfo) -> Option<FrameHeader> {
    if b.len() < 6 || b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = b[1] & 1 != 0;
    let bs_code = b[2] >> 4;
    let sr_code = b[2] & 0x0F;
    let ch_code = b[3] >> 4;
    let bps_code = (b[3] >> 1) & 7;
    if b[3] & 1 != 0 {
        return None;
    }
    let (assign, channels) = match ch_code {
        0..=7 => (ch_code, ch_code + 1),
        8..=10 => (ch_code, 2),
        _ => return None,
    };
    let bps = match bps_code {
        0 => si.bps,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return None,
    };
    let (number, nlen) = utf8_number(&b[4..])?;
    let mut o = 4 + nlen;
    let block_size = match bs_code {
        0 => return None,
        1 => 192,
        2..=5 => 576u32 << (bs_code - 2),
        6 => {
            let v = u32::from(*b.get(o)?) + 1;
            o += 1;
            v
        }
        7 => {
            let v = u32::from(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?])) + 1;
            o += 2;
            v
        }
        _ => 256u32 << (bs_code - 8),
    };
    let sample_rate = match sr_code {
        0 => si.sample_rate,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => {
            let v = u32::from(*b.get(o)?) * 1000;
            o += 1;
            v
        }
        13 => {
            let v = u32::from(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]));
            o += 2;
            v
        }
        14 => {
            let v = u32::from(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?])) * 10;
            o += 2;
            v
        }
        _ => return None,
    };
    let crc = *b.get(o)?;
    if crc8(&b[..o]) != crc {
        return None;
    }
    // Consistency with STREAMINFO.
    if sample_rate != si.sample_rate || channels != si.channels || bps != si.bps {
        return None;
    }
    if si.max_block != 0 && block_size > si.max_block {
        return None;
    }
    if !variable && number > 0x7FFF_FFFF {
        return None;
    }
    Some(FrameHeader {
        len: o + 1,
        variable,
        block_size,
        channels,
        assign,
        bps,
        number,
    })
}

// ---------------------------------------------------------------------------
// Stepping through subframes
// ---------------------------------------------------------------------------

struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // bit position
    /// A read went past the end of the data.
    overrun: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits {
            data,
            pos: 0,
            overrun: false,
        }
    }

    fn fail(&mut self) -> Option<()> {
        self.overrun = true;
        None
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        let end = self.pos + n as usize;
        if end > self.data.len() * 8 {
            self.fail();
            return None;
        }
        let mut v = 0u64;
        let mut left = n;
        while left > 0 {
            let byte = u64::from(self.data[self.pos / 8]);
            let used = (self.pos % 8) as u32;
            let avail = 8 - used;
            let take = avail.min(left);
            v = (v << take) | ((byte >> (avail - take)) & ((1u64 << take) - 1));
            self.pos += take as usize;
            left -= take;
        }
        Some(v as u32)
    }

    fn skip(&mut self, n: u64) -> Option<()> {
        let end = self.pos as u64 + n;
        if end > self.data.len() as u64 * 8 {
            return self.fail();
        }
        self.pos = end as usize;
        Some(())
    }

    /// Count zero bits up to and including the next one bit.
    fn unary(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        loop {
            let byte_idx = self.pos / 8;
            let Some(&b) = self.data.get(byte_idx) else {
                self.fail();
                return None;
            };
            let used = (self.pos % 8) as u32;
            let masked = b << used; // drop consumed bits
            if masked != 0 {
                let lz = masked.leading_zeros();
                zeros += lz;
                self.pos += lz as usize + 1;
                return Some(zeros);
            }
            zeros += 8 - used;
            self.pos += (8 - used) as usize;
            if zeros > (1 << 24) {
                return None;
            }
        }
    }
}

fn skip_residual(r: &mut Bits<'_>, block_size: u32, order: u32) -> Option<()> {
    let method = r.read(2)?;
    let pbits = match method {
        0 => 4,
        1 => 5,
        _ => return None,
    };
    let part_order = r.read(4)?;
    let parts = 1u32 << part_order;
    if part_order > 0 && (block_size % parts != 0 || (block_size >> part_order) <= order) {
        return None;
    }
    if part_order == 0 && block_size <= order {
        return None;
    }
    let escape = (1u32 << pbits) - 1;
    for i in 0..parts {
        let n = if part_order == 0 {
            block_size - order
        } else if i == 0 {
            (block_size >> part_order) - order
        } else {
            block_size >> part_order
        };
        let param = r.read(pbits)?;
        if param == escape {
            let bits = r.read(5)?;
            r.skip(u64::from(n) * u64::from(bits))?;
        } else {
            for _ in 0..n {
                r.unary()?;
                r.skip(u64::from(param))?;
            }
        }
    }
    Some(())
}

fn skip_subframe(r: &mut Bits<'_>, block_size: u32, bps: u32) -> Option<()> {
    if r.read(1)? != 0 {
        return None;
    }
    let ty = r.read(6)?;
    let wasted = if r.read(1)? == 1 { r.unary()? + 1 } else { 0 };
    let bps = bps.checked_sub(wasted).filter(|&b| b >= 1)?;
    match ty {
        0 => r.skip(u64::from(bps)),
        1 => r.skip(u64::from(block_size) * u64::from(bps)),
        8..=15 => {
            let order = ty - 8;
            if order > 4 {
                return None;
            }
            r.skip(u64::from(order) * u64::from(bps))?;
            skip_residual(r, block_size, order)
        }
        32..=63 => {
            let order = ty - 31;
            if block_size <= order {
                return None;
            }
            r.skip(u64::from(order) * u64::from(bps))?;
            let precision = r.read(4)?;
            if precision == 15 {
                return None;
            }
            r.read(5)?; // shift
            r.skip(u64::from(order) * u64::from(precision + 1))?;
            skip_residual(r, block_size, order)
        }
        _ => None,
    }
}

/// Length in bytes of the frame whose header `h` starts at the beginning of
/// `data`, or the reason it cannot be determined.
#[derive(Debug, PartialEq, Eq)]
enum FrameLen {
    Ok(usize),
    /// Ran past `data`; more bytes may help.
    NeedMore,
    Invalid,
}

fn frame_len(data: &[u8], h: &FrameHeader) -> FrameLen {
    let mut r = Bits::new(&data[h.len.min(data.len())..]);
    if h.len > data.len() {
        return FrameLen::NeedMore;
    }
    for ch in 0..h.channels {
        let extra = match (h.assign, ch) {
            (8, 1) | (9, 0) | (10, 1) => 1,
            _ => 0,
        };
        if skip_subframe(&mut r, h.block_size, u32::from(h.bps) + extra).is_none() {
            return if r.overrun {
                FrameLen::NeedMore
            } else {
                FrameLen::Invalid
            };
        }
    }
    // Byte alignment, then the CRC-16.
    let bytes = r.pos.div_ceil(8) + 2;
    if h.len + bytes > data.len() {
        return FrameLen::NeedMore;
    }
    FrameLen::Ok(h.len + bytes)
}

// ---------------------------------------------------------------------------
// Walking frames
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// All samples promised by STREAMINFO were seen.
    Total,
    /// The data ended on a frame boundary.
    Eof,
    /// Limit reached.
    Limit,
    /// No valid frame follows.
    NoFrame,
    /// A frame is cut by the end of the data.
    Partial,
    Fault,
}

#[derive(Debug)]
struct FrameWalk {
    end: u64,
    frames: u64,
    samples: u64,
    damaged: u64,
    /// Bytes inside the walk that belong to damaged frames.
    damaged_bytes: u64,
    stop: Stop,
    /// First problem offset, if any.
    first_damage: Option<u64>,
}

/// Read the frame at `pos` as one verified unit.
fn read_frame(
    win: &mut Window<'_>,
    pos: u64,
    si: &StreamInfo,
) -> Option<(FrameHeader, usize, bool)> {
    let mut want = FRAME_FIRST_READ;
    loop {
        let data = win.get(pos, want);
        let h = parse_frame_header(data, si)?;
        match frame_len(data, &h) {
            FrameLen::Ok(n) => {
                let ok = crc16_update(0, &data[..n]) == 0;
                return Some((h, n, ok));
            }
            FrameLen::Invalid => return None,
            FrameLen::NeedMore => {
                if data.len() < want || want >= FRAME_MAX_READ {
                    return None;
                }
                want = (want * 4).min(FRAME_MAX_READ);
            }
        }
    }
}

/// Search forward from `from` for the next header consistent with the stream.
fn find_next_header(
    win: &mut Window<'_>,
    from: u64,
    si: &StreamInfo,
    expect: impl Fn(&FrameHeader) -> bool,
) -> Option<u64> {
    let mut p = from;
    let end = from.saturating_add(RESYNC_SCAN as u64).min(win.avail());
    const STEP: usize = 64 * 1024;
    while p < end {
        let block = win.get(p, STEP + 24).to_vec();
        if block.len() < 8 {
            return None;
        }
        // Candidate positions in this block; the extra 24 bytes only serve
        // to parse headers that start near its end.
        let scan = block.len().saturating_sub(1).min(STEP);
        for i in 0..scan {
            if block[i] == 0xFF && block[i + 1] & 0xFE == 0xF8 {
                let off = p + i as u64;
                if off >= end {
                    return None;
                }
                if let Some(h) = parse_frame_header(&block[i..], si) {
                    if expect(&h) {
                        return Some(off);
                    }
                }
            }
        }
        if block.len() <= STEP {
            return None;
        }
        p += scan as u64;
    }
    None
}

fn walk_frames(win: &mut Window<'_>, start: u64, si: &StreamInfo) -> FrameWalk {
    let avail = win.avail();
    let mut w = FrameWalk {
        end: start,
        frames: 0,
        samples: 0,
        damaged: 0,
        damaged_bytes: 0,
        stop: Stop::NoFrame,
        first_damage: None,
    };
    let total = si.total_samples;
    let mut pos = start;
    let mut base: Option<(bool, u64)> = None; // (variable, first number)
    loop {
        if total != 0 && w.samples >= total {
            w.stop = Stop::Total;
            break;
        }
        if pos >= avail {
            w.stop = if win.limited() {
                Stop::Limit
            } else {
                Stop::Eof
            };
            break;
        }
        // Expected numbering.
        let expect_number = base.map(|(var, first)| {
            if var {
                first + w.samples
            } else {
                first + w.frames
            }
        });
        let frame = read_frame(win, pos, si);
        let good = frame.filter(|(h, _, crc_ok)| {
            *crc_ok
                && base.is_none_or(|(var, _)| var == h.variable)
                && expect_number.is_none_or(|e| e == h.number)
        });
        if let Some((h, n, _)) = good {
            if base.is_none() {
                base = Some((h.variable, h.number));
            }
            w.samples += u64::from(h.block_size);
            w.frames += 1;
            pos += n as u64;
            w.end = pos;
            continue;
        }
        // Not a good frame here.
        if win.fault().is_some() {
            w.stop = Stop::Fault;
            break;
        }
        if base.is_none() {
            // The very first frame must be good.
            w.stop = Stop::NoFrame;
            break;
        }
        let (var, first) = base.unwrap_or((false, 0));
        let cur = (var, first + if var { w.samples } else { w.frames });
        // Could this be a frame cut by the end of the data?
        let remaining = avail - pos;
        if remaining < 4096 && frame.is_none() && parse_frame_header(win.get(pos, 32), si).is_some()
        {
            w.stop = if win.limited() {
                Stop::Limit
            } else {
                Stop::Partial
            };
            break;
        }
        // Bridge a damaged frame: next header with a later number.
        let next = find_next_header(win, pos + 1, si, |h| {
            h.variable == cur.0 && h.number > cur.1
        });
        match next {
            Some(np) => {
                let h = parse_frame_header(win.get(np, 32), si);
                if let Some(h) = h {
                    // The skipped region counts as damaged frame(s).
                    let skipped_frames = if var {
                        0
                    } else {
                        h.number.saturating_sub(first + w.frames)
                    };
                    log::warn!(
                        "FLAC: damaged frame at offset {pos}; resuming at {np} (frame/sample {})",
                        h.number
                    );
                    w.damaged += skipped_frames.max(1);
                    w.damaged_bytes += np - pos;
                    w.first_damage.get_or_insert(pos);
                    if var {
                        w.samples = h.number - first;
                    } else {
                        w.samples += u64::from(si.max_block.max(h.block_size)) * skipped_frames;
                        w.frames += skipped_frames;
                    }
                    pos = np;
                    continue;
                }
                w.stop = Stop::NoFrame;
                break;
            }
            None => {
                w.stop = Stop::NoFrame;
                w.first_damage.get_or_insert(pos);
                break;
            }
        }
    }
    w
}

struct Analysis {
    md: Metadata,
    walk: FrameWalk,
}

fn analyse(win: &mut Window<'_>) -> Option<Analysis> {
    let md = parse_metadata(win)?;
    let walk = walk_frames(win, md.end, &md.info);
    Some(Analysis { md, walk })
}

fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn build_meta(a: &Analysis) -> MediaMeta {
    let si = &a.md.info;
    let mut m = MediaMeta {
        audio_codec: Some("flac".into()),
        ..MediaMeta::default()
    };
    let samples = if si.total_samples > 0 {
        si.total_samples
    } else {
        a.walk.samples
    };
    if samples > 0 {
        m.duration_ms = Some(samples * 1000 / u64::from(si.sample_rate));
    }
    m.extra.insert(EXTENSION_KEY.into(), "flac".into());
    m.extra
        .insert("sample_rate".into(), si.sample_rate.to_string());
    m.extra.insert("channels".into(), si.channels.to_string());
    m.extra.insert("bits".into(), si.bps.to_string());
    m.extra.insert("frames".into(), a.walk.frames.to_string());
    if si.total_samples > 0 {
        m.extra
            .insert("total_samples".into(), si.total_samples.to_string());
    }
    for (src, dst) in [("TITLE", "title"), ("ARTIST", "artist"), ("ALBUM", "album")] {
        if let Some(v) = tag(&a.md.tags, src) {
            m.extra.insert(dst.into(), v.to_string());
        }
    }
    if let Some(d) = tag(&a.md.tags, "DATE") {
        m.extra.insert("year".into(), d.chars().take(4).collect());
        m.created = parse_loose_date(d);
    }
    if let Some(e) = tag(&a.md.tags, "ENCODER") {
        m.software = Some(e.to_string());
    }
    m
}

/// Format handler for FLAC.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlacFormat;

static SIGNATURES: [Signature; 2] = [
    Signature::at_start(b"fLaC\x00"),
    Signature::at_start(b"fLaC\x80"),
];

impl FileFormat for FlacFormat {
    fn id(&self) -> &'static str {
        "flac"
    }

    fn name(&self) -> &'static str {
        "FLAC audio"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["flac"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Audio
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        42
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        40
    }

    fn probe(&self, head: &[u8]) -> bool {
        head.len() >= 8 && &head[..4] == b"fLaC" && head[4] & 0x7F == 0 && head[5..8] == [0, 0, 34]
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "jpg" | "png")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(a) = analyse(&mut win) else {
            return Ok(None);
        };
        let w = &a.walk;
        let total = a.md.info.total_samples;
        let mut notes = Vec::new();
        let (status, err_offset) = match w.stop {
            Stop::Total => {
                if w.samples == total {
                    (ExtentStatus::Complete, None)
                } else {
                    notes.push(format!(
                        "frames hold {} samples but STREAMINFO says {total}",
                        w.samples
                    ));
                    (ExtentStatus::UnknownLength, None)
                }
            }
            Stop::Limit => (ExtentStatus::Capped, None),
            Stop::Eof | Stop::NoFrame => {
                if total != 0 && w.samples < total {
                    notes.push(format!(
                        "stream ends after {} of {total} samples",
                        w.samples
                    ));
                    (ExtentStatus::Truncated, Some(w.end))
                } else {
                    (ExtentStatus::UnknownLength, None)
                }
            }
            Stop::Partial => {
                notes.push("the last frame is cut short and was left out".into());
                (ExtentStatus::Truncated, Some(w.end))
            }
            Stop::Fault => {
                notes.push("read error while walking frames".into());
                (ExtentStatus::Truncated, Some(w.end))
            }
        };
        if w.damaged > 0 {
            notes.push(format!(
                "{} damaged frame(s) ({} bytes) were bridged",
                w.damaged, w.damaged_bytes
            ));
        }
        if w.frames == 0 && total != 0 {
            return Ok(None);
        }
        let mut confidence = match status {
            ExtentStatus::Complete => Confidence::HIGH,
            _ => Confidence::MEDIUM,
        };
        if w.damaged > 0 {
            confidence = confidence.lowered(25);
        }
        let mut m = Measured {
            len: w.end,
            status,
            confidence,
            err_offset,
            meta: build_meta(&a),
            nested: a
                .md
                .covers
                .iter()
                .filter(|n| n.range.end <= w.end)
                .cloned()
                .collect(),
            notes,
        };
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut win = Window::new(&**src, 0, u64::MAX);
        let size = win.avail();
        let Some(a) = analyse(&mut win) else {
            return Ok(Validation::invalid(
                "not a FLAC file (no fLaC marker with a valid STREAMINFO block)",
            ));
        };
        let w = &a.walk;
        let total = a.md.info.total_samples;
        let mut problems = Vec::new();
        if w.damaged > 0 {
            problems.push(format!(
                "{} frame(s) failed their CRC or could not be parsed (first near offset {})",
                w.damaged,
                w.first_damage.unwrap_or(0)
            ));
        }
        if total != 0 && w.samples < total {
            problems.push(format!(
                "only {} of the {total} samples promised by STREAMINFO are present",
                w.samples
            ));
        }
        if total != 0 && w.samples > total {
            problems.push(format!(
                "frames hold {} samples, more than the {total} in STREAMINFO",
                w.samples
            ));
        }
        match w.stop {
            Stop::Partial => problems.push("the file ends inside a frame".into()),
            Stop::Fault => problems.push("read error while walking frames".into()),
            _ => {}
        }
        if w.end < size && w.stop != Stop::Partial {
            problems.push(format!(
                "{} bytes after offset {} are not valid FLAC frames",
                size - w.end,
                w.end
            ));
        }
        if w.frames == 0 {
            problems.push("no audio frames".into());
        }
        let intact = if problems.is_empty() {
            1.0
        } else {
            let good = w.end.saturating_sub(w.damaged_bytes);
            (good as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
        };
        Ok(Validation {
            status: if problems.is_empty() {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Damaged
            },
            intact_fraction: Some(intact),
            problems,
            meta: build_meta(&a),
            repairable_by: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct BitWriter {
        out: Vec<u8>,
        cur: u32,
        n: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            BitWriter {
                out: Vec::new(),
                cur: 0,
                n: 0,
            }
        }
        fn put(&mut self, v: u32, bits: u32) {
            for i in (0..bits).rev() {
                self.cur = (self.cur << 1) | ((v >> i) & 1);
                self.n += 1;
                if self.n == 8 {
                    self.out.push(self.cur as u8);
                    self.cur = 0;
                    self.n = 0;
                }
            }
        }
        fn finish(mut self) -> Vec<u8> {
            while self.n != 0 {
                self.put(0, 1);
            }
            self.out
        }
    }

    fn streaminfo(block: u16, rate: u32, channels: u8, bps: u8, total: u64) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&block.to_be_bytes());
        b.extend_from_slice(&block.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let v = (u64::from(rate) << 44)
            | (u64::from(channels - 1) << 41)
            | (u64::from(bps - 1) << 36)
            | total;
        b.extend_from_slice(&v.to_be_bytes());
        b.extend_from_slice(&[0u8; 16]);
        b
    }

    /// A frame of 16-bit stereo, verbatim subframes holding `seed`-derived data.
    fn frame(number: u64, variable: bool, block: u32, seed: u32) -> Vec<u8> {
        let mut h = vec![0xFF, 0xF8 | u8::from(variable)];
        // block size code 6 (8 bit, minus 1) or 7 (16 bit); rate code 9 (44.1k)
        let (bs_code, extra): (u8, Vec<u8>) = if block <= 256 {
            (6, vec![(block - 1) as u8])
        } else {
            (7, ((block - 1) as u16).to_be_bytes().to_vec())
        };
        h.push((bs_code << 4) | 9);
        h.push((1 << 4) | (4 << 1)); // 2 channels independent, 16 bit
                                     // UTF-8 style number (up to 31 bits)
        if number < 0x80 {
            h.push(number as u8);
        } else if number < 0x800 {
            h.push(0xC0 | (number >> 6) as u8);
            h.push(0x80 | (number & 0x3F) as u8);
        } else if number < 0x10000 {
            h.push(0xE0 | (number >> 12) as u8);
            h.push(0x80 | ((number >> 6) & 0x3F) as u8);
            h.push(0x80 | (number & 0x3F) as u8);
        } else {
            h.push(0xF0 | (number >> 18) as u8);
            h.push(0x80 | ((number >> 12) & 0x3F) as u8);
            h.push(0x80 | ((number >> 6) & 0x3F) as u8);
            h.push(0x80 | (number & 0x3F) as u8);
        }
        h.extend_from_slice(&extra);
        let c = crc8(&h);
        h.push(c);
        let mut w = BitWriter::new();
        for ch in 0..2u32 {
            w.put(0, 1);
            w.put(1, 6); // verbatim
            w.put(0, 1);
            for i in 0..block {
                w.put(
                    (seed.wrapping_mul(31).wrapping_add(i * 7 + ch * 13)) & 0xFFFF,
                    16,
                );
            }
        }
        let mut f = h;
        f.extend_from_slice(&w.finish());
        let crc = crc16_update(0, &f);
        f.extend_from_slice(&crc.to_be_bytes());
        f
    }

    fn build(frames: usize, block: u32, total: Option<u64>) -> Vec<u8> {
        let total = total.unwrap_or(frames as u64 * u64::from(block));
        let mut d = b"fLaC".to_vec();
        d.push(0x80);
        d.extend_from_slice(&[0, 0, 34]);
        d.extend_from_slice(&streaminfo(block as u16, 44_100, 2, 16, total));
        for i in 0..frames {
            d.extend_from_slice(&frame(i as u64, false, block, i as u32));
        }
        d
    }

    fn mem(d: Vec<u8>) -> SharedSource {
        Arc::new(diskrescue_core::source::MemSource::new(d))
    }

    #[test]
    fn fixed_blocksize_stream_stops_at_the_promised_total() {
        let f = build(12, 4096, None);
        let mut d = f.clone();
        // A second file directly behind must not be swallowed.
        d.extend_from_slice(&build(5, 4096, None));
        let m = FlacFormat.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, f.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert_eq!(m.meta.duration_ms, Some(12 * 4096 * 1000 / 44_100));
        let v = FlacFormat.validate(&mem(f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    }

    #[test]
    fn short_last_frame_and_unknown_total() {
        let mut d = b"fLaC".to_vec();
        d.push(0x80);
        d.extend_from_slice(&[0, 0, 34]);
        d.extend_from_slice(&streaminfo(4096, 44_100, 2, 16, 0));
        for i in 0..6 {
            d.extend_from_slice(&frame(i, false, 4096, i as u32));
        }
        d.extend_from_slice(&frame(6, false, 1000, 77));
        let n = d.len();
        d.extend_from_slice(&[0x13; 5000]);
        let m = FlacFormat.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, n as u64);
        assert_eq!(m.status, ExtentStatus::UnknownLength);
        assert_eq!(m.meta.extra["frames"], "7");
    }

    #[test]
    fn variable_blocksize_with_total_is_complete() {
        let sizes = [4096u32, 2000, 1000, 4096, 300];
        let total: u64 = sizes.iter().map(|&s| u64::from(s)).sum();
        let mut d = b"fLaC".to_vec();
        d.push(0x80);
        d.extend_from_slice(&[0, 0, 34]);
        d.extend_from_slice(&streaminfo(4096, 44_100, 2, 16, total));
        let mut sample = 0u64;
        for (i, &s) in sizes.iter().enumerate() {
            d.extend_from_slice(&frame(sample, true, s, i as u32));
            sample += u64::from(s);
        }
        let n = d.len();
        d.extend_from_slice(&[0x99; 3000]);
        let m = FlacFormat.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, n as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
    }

    #[test]
    fn truncation_is_reported() {
        let f = build(10, 4096, None);
        let one = frame(0, false, 4096, 0).len();
        let cut = &f[..f.len() - one * 3 - 100];
        let m = FlacFormat
            .measure(&mem(cut.to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.len, (f.len() - one * 4) as u64);
        assert_eq!(m.err_offset, Some(m.len));
    }

    #[test]
    fn a_damaged_frame_is_bridged_and_reported() {
        let mut f = build(10, 4096, None);
        let one = frame(0, false, 4096, 0).len();
        let start = f.len() - 10 * one;
        f[start + 4 * one + 100] ^= 0xFF;
        let m = FlacFormat
            .measure(&mem(f.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, f.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert!(
            m.notes.iter().any(|n| n.contains("damaged")),
            "{:?}",
            m.notes
        );
        let v = FlacFormat.validate(&mem(f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
    }

    #[test]
    fn rejects_garbage() {
        assert!(FlacFormat
            .measure(&mem(b"fLaC\x00\x00\x00\x22".to_vec()), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(FlacFormat
            .measure(&mem(vec![0u8; 100]), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(!FlacFormat.probe(b"fLaC\x01\x00\x00\x22"));
    }

    #[test]
    fn utf8_numbers() {
        assert_eq!(utf8_number(&[0x7F]), Some((0x7F, 1)));
        assert_eq!(utf8_number(&[0xC2, 0x80]), Some((0x80, 2)));
        assert_eq!(utf8_number(&[0xE0, 0xA0, 0x80]), Some((0x800, 3)));
        assert_eq!(
            utf8_number(&[0xFE, 0x82, 0x80, 0x80, 0x80, 0x80, 0x80]),
            Some((0x80 << 24, 7))
        );
        assert_eq!(utf8_number(&[0x80]), None);
        assert_eq!(utf8_number(&[0xC2, 0x00]), None);
    }
}
