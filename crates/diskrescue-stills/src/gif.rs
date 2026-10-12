//! GIF (87a / 89a).
//!
//! # Algorithm
//!
//! A GIF is a 6-byte signature, a 7-byte logical screen descriptor, an
//! optional global colour table and then a stream of blocks until the
//! trailer byte `0x3B`:
//!
//! * `0x21` extension: a label byte followed by data sub-blocks;
//! * `0x2C` image descriptor: nine bytes, an optional local colour table, the
//!   LZW minimum code size and data sub-blocks;
//! * `0x3B` trailer: the end of the file.
//!
//! Sub-blocks are a length byte followed by that many bytes, ended by a
//! zero-length block. [`GifFormat::measure`] walks the blocks without
//! decoding anything and ends at the trailer, so the length is exact.
//!
//! # False positives
//!
//! The 6-byte signature is short enough that random data and text files
//! produce hits (the famous 783-byte GIFs of Foremost). A candidate is
//! therefore rejected unless the screen size is non-zero and the *first*
//! block is an extension with a known label or an image descriptor. Later
//! unknown block bytes end the walk as `Truncated` at that offset.
//!
//! # Validation
//!
//! [`GifFormat::validate`] repeats the walk and additionally decodes the LZW
//! code stream of every frame, without building pixels, to count the pixels
//! each frame produces. That catches corruption inside image data, which the
//! block walk alone cannot see.
//!
//! # Limits
//!
//! No repair is offered. Frames beyond the first 100,000 are not LZW-checked.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{u16le, u8_at};
use diskrescue_core::Result;

use crate::common::{failed, validate_with, Inspected};
use crate::win::{read_error, Cursor, Fail, Win};

const MAX_FRAMES_TRACKED: usize = 100_000;
const MAX_SIZE: u64 = 256 * 1024 * 1024;
const SIGNATURES: [Signature; 2] = [
    Signature::at_start(b"GIF87a"),
    Signature::at_start(b"GIF89a"),
];

/// GIF images, including animations.
#[derive(Debug, Clone, Copy, Default)]
pub struct GifFormat;

/// One image descriptor found by the walk.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Offset of the LZW minimum-code-size byte.
    data_off: u64,
    width: u16,
    height: u16,
}

/// Facts gathered by the walk.
#[derive(Debug, Default)]
struct Scan {
    width: u16,
    height: u16,
    version: [u8; 3],
    frames: u32,
    tracked: Vec<Frame>,
    delay_cs: u64,
    netscape_loop: Option<u16>,
    /// Offset of the byte after the last fully accepted block.
    good_end: u64,
    outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    /// Trailer found; length is `end`.
    Trailer(u64),
    /// A byte that is not a block introducer at this offset.
    BadBlock(u64),
    /// The window ended or the medium failed.
    Stopped(Fail),
}

/// Size in bytes of a colour table whose packed-field size bits are `bits`.
fn table_len(packed: u8, flag: u8) -> u64 {
    if packed & flag != 0 {
        3u64 << ((packed & 7) + 1)
    } else {
        0
    }
}

/// Whether the first block of a GIF is plausible: `Some(false)` for a definite
/// rejection, `Some(true)` for a pass, `None` when the bytes are missing.
fn first_block_ok(first: Option<u8>, label: Option<u8>) -> Option<bool> {
    match first? {
        0x2C => Some(true),
        0x21 => Some(matches!(label?, 0x01 | 0xF9 | 0xFE | 0xFF)),
        _ => Some(false),
    }
}

/// Check the first bytes of a candidate; used by `probe`. Returns `None` when
/// `head` is too short to decide.
fn quick_check(head: &[u8]) -> Option<bool> {
    if head.len() < 13 {
        return None;
    }
    if !SIGNATURES.iter().any(|s| s.matches(head)) {
        return Some(false);
    }
    let (w, h) = (u16le(head, 6)?, u16le(head, 8)?);
    if w == 0 || h == 0 {
        return Some(false);
    }
    let first = 13 + table_len(head[10], 0x80) as usize;
    first_block_ok(u8_at(head, first), u8_at(head, first + 1))
}

/// Skip data sub-blocks starting at `pos`; returns the offset after the
/// zero-length terminator.
fn skip_sub_blocks(cur: &mut Cursor<'_>, mut pos: u64) -> std::result::Result<u64, Fail> {
    loop {
        let len = cur.byte(pos)? as u64;
        pos += 1 + len;
        if len == 0 {
            return Ok(pos);
        }
    }
}

fn scan(win: &Win) -> std::result::Result<Option<Scan>, Fail> {
    let mut cur = Cursor::new(win, 64 * 1024);
    let head = match cur.slice(0, 13) {
        Ok(h) => h.to_vec(),
        Err(Fail::Eof(_)) => return Ok(None),
        Err(f) => return Err(f),
    };
    let mut pos = 13 + table_len(head[10], 0x80);
    if !SIGNATURES.iter().any(|g| g.matches(&head)) {
        return Ok(None);
    }
    if u16le(&head, 6) == Some(0) || u16le(&head, 8) == Some(0) {
        return Ok(None);
    }
    // The first block must be an image descriptor or a known extension; this
    // is what rejects Foremost-style false positives.
    let first = win.read_upto(pos, 2)?;
    match first_block_ok(first.first().copied(), first.get(1).copied()) {
        Some(false) => return Ok(None),
        Some(true) => {}
        // Nothing (or only a lone 0x21) after the colour table.
        None => return Ok(None),
    }
    let mut s = Scan {
        width: u16le(&head, 6).unwrap_or(0),
        height: u16le(&head, 8).unwrap_or(0),
        version: [head[3], head[4], head[5]],
        ..Scan::default()
    };
    s.good_end = pos;
    let outcome = loop {
        let b = match cur.byte(pos) {
            Ok(b) => b,
            Err(f) => break Outcome::Stopped(f),
        };
        match b {
            0x3B => break Outcome::Trailer(pos + 1),
            0x21 => {
                let label = match cur.byte(pos + 1) {
                    Ok(l) => l,
                    Err(f) => break Outcome::Stopped(f),
                };
                if !matches!(label, 0x01 | 0xF9 | 0xFE | 0xFF) {
                    break Outcome::BadBlock(pos);
                }
                if label == 0xF9 {
                    // Graphic control extension: block size 4, packed, delay.
                    match cur.slice(pos + 2, 5) {
                        Ok(g) => {
                            if g[0] != 4 {
                                break Outcome::BadBlock(pos);
                            }
                            s.delay_cs += u16::from_le_bytes([g[2], g[3]]) as u64;
                        }
                        Err(f) => break Outcome::Stopped(f),
                    }
                }
                if label == 0xFF {
                    if let Ok(a) = cur.slice(pos + 2, 16) {
                        if &a[1..12] == b"NETSCAPE2.0" && a[12] == 3 && a[13] == 1 {
                            s.netscape_loop = Some(u16::from_le_bytes([a[14], a[15]]));
                        }
                    }
                }
                match skip_sub_blocks(&mut cur, pos + 2) {
                    Ok(p) => pos = p,
                    Err(f) => break Outcome::Stopped(f),
                }
            }
            0x2C => {
                let d = match cur.slice(pos + 1, 9) {
                    Ok(d) => d.to_vec(),
                    Err(f) => break Outcome::Stopped(f),
                };
                let lct = table_len(d[8], 0x80);
                let lzw_at = pos + 10 + lct;
                let min_code = match cur.byte(lzw_at) {
                    Ok(m) => m,
                    Err(f) => break Outcome::Stopped(f),
                };
                if !(1..=8).contains(&min_code) {
                    break Outcome::BadBlock(pos);
                }
                match skip_sub_blocks(&mut cur, lzw_at + 1) {
                    Ok(p) => {
                        s.frames = s.frames.saturating_add(1);
                        if s.tracked.len() < MAX_FRAMES_TRACKED {
                            s.tracked.push(Frame {
                                data_off: lzw_at,
                                width: u16::from_le_bytes([d[4], d[5]]),
                                height: u16::from_le_bytes([d[6], d[7]]),
                            });
                        }
                        pos = p;
                    }
                    Err(f) => break Outcome::Stopped(f),
                }
            }
            _ => break Outcome::BadBlock(pos),
        }
        s.good_end = pos;
    };
    s.outcome = Some(outcome);
    Ok(Some(s))
}

fn inspect(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Inspected>> {
    let limit = limit.min(MAX_SIZE);
    let win = Win::new(src, start, limit, 64 * 1024);
    let s = match scan(&win) {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(None),
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let conf = if s.frames > 0 {
        Confidence::HIGH
    } else {
        Confidence::MEDIUM.lowered(20)
    };
    let mut m = match s.outcome {
        Some(Outcome::Trailer(end)) => Measured::complete(end, conf),
        Some(Outcome::BadBlock(at)) => Measured::truncated(
            s.good_end.min(at),
            conf.lowered(30),
            at,
            format!("unexpected byte where a GIF block should start at offset {at}"),
        ),
        Some(Outcome::Stopped(f)) => failed(&win, f, conf, "a GIF block"),
        None => return Ok(None),
    };
    if s.frames == 0 && m.status == ExtentStatus::Complete {
        m.notes.push("GIF without any image".into());
    }
    let mut meta = MediaMeta {
        width: Some(s.width as u32),
        height: Some(s.height as u32),
        ..MediaMeta::default()
    };
    meta.extra.insert(
        "version".into(),
        String::from_utf8_lossy(&s.version).into_owned(),
    );
    meta.extra.insert("frames".into(), s.frames.to_string());
    if s.frames > 1 {
        meta.duration_ms = Some(s.delay_cs * 10);
        meta.extra.insert("animated".into(), "true".into());
    }
    if let Some(l) = s.netscape_loop {
        meta.extra.insert("loop_count".into(), l.to_string());
    }
    crate::common::log_measured(
        &win,
        "gif",
        &m,
        matches!(s.outcome, Some(Outcome::Stopped(Fail::Io(_)))),
    );
    m.meta = meta;
    Ok(Some(Inspected::new(m, None)))
}

/// Outcome of decoding the LZW codes of one frame.
#[derive(Debug)]
enum Lzw {
    Ok,
    Bad(String),
}

/// Decode the code stream of one frame, counting pixels only.
fn lzw_check(
    cur: &mut Cursor<'_>,
    data_off: u64,
    expected_pixels: u64,
) -> std::result::Result<Lzw, Fail> {
    let min = cur.byte(data_off)? as u32;
    let clear = 1u32 << min;
    let eoi = clear + 1;
    let mut lens = [0u32; 4096];
    let mut next = clear + 2;
    let mut size = min + 1;
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut prev: Option<usize> = None;
    let mut pixels: u64 = 0;
    for l in lens.iter_mut().take(clear as usize) {
        *l = 1;
    }
    let mut pos = data_off + 1;
    let mut seen_eoi = false;
    'blocks: loop {
        let len = cur.byte(pos)? as usize;
        if len == 0 {
            break;
        }
        let block = cur.slice(pos + 1, len)?.to_vec();
        pos += 1 + len as u64;
        for byte in block {
            acc |= (byte as u32) << nbits;
            nbits += 8;
            while nbits >= size {
                let code = acc & ((1 << size) - 1);
                acc >>= size;
                nbits -= size;
                if code == clear {
                    next = clear + 2;
                    size = min + 1;
                    prev = None;
                    continue;
                }
                if code == eoi {
                    seen_eoi = true;
                    break 'blocks;
                }
                let n = match prev {
                    None => {
                        if code >= clear {
                            return Ok(Lzw::Bad(format!("first LZW code {code} is not a literal")));
                        }
                        1
                    }
                    Some(p) => {
                        let n = if code < next {
                            lens[code as usize]
                        } else if code == next {
                            lens[p] + 1
                        } else {
                            return Ok(Lzw::Bad(format!(
                                "LZW code {code} beyond the dictionary ({next} entries)"
                            )));
                        };
                        if next < 4096 {
                            lens[next as usize] = lens[p] + 1;
                            next += 1;
                            if next == (1 << size) && size < 12 {
                                size += 1;
                            }
                        }
                        n
                    }
                };
                pixels += n as u64;
                prev = Some(code as usize);
                if pixels > expected_pixels {
                    return Ok(Lzw::Bad(format!(
                        "LZW data decodes to more than the {expected_pixels} pixels of the frame"
                    )));
                }
            }
        }
    }
    if pixels < expected_pixels {
        let how = if seen_eoi {
            "ends early"
        } else {
            "has no end code and stops short"
        };
        return Ok(Lzw::Bad(format!(
            "LZW data {how}: {pixels} of {expected_pixels} pixels"
        )));
    }
    Ok(Lzw::Ok)
}

impl FileFormat for GifFormat {
    fn id(&self) -> &'static str {
        "gif"
    }

    fn name(&self) -> &'static str {
        "GIF image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["gif"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        35
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn probe(&self, head: &[u8]) -> bool {
        match quick_check(head) {
            Some(ok) => ok,
            // Too short to judge the first block: the signature must at
            // least match.
            None => SIGNATURES.iter().any(|s| s.matches(head)),
        }
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        Ok(inspect(src, start, limit)?.map(|i| i.measured))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut v = validate_with("GIF", src, inspect)?;
        if v.status == ValidationStatus::Invalid {
            return Ok(v);
        }
        // Deep check: LZW pixel counts per frame.
        let win = Win::new(src, 0, MAX_SIZE, 64 * 1024);
        let Ok(Some(s)) = scan(&win) else {
            return Ok(v);
        };
        let mut cur = Cursor::new(&win, 64 * 1024);
        let mut bad = 0usize;
        let mut checked = 0usize;
        for (i, f) in s.tracked.iter().enumerate() {
            let want = f.width as u64 * f.height as u64;
            match lzw_check(&mut cur, f.data_off, want) {
                Ok(Lzw::Ok) => checked += 1,
                Ok(Lzw::Bad(why)) => {
                    checked += 1;
                    bad += 1;
                    v.problems.push(format!("frame {}: {why}", i + 1));
                }
                Err(_) => break,
            }
        }
        if bad > 0 {
            v.status = ValidationStatus::Damaged;
            let total = s.frames.max(1) as f32;
            let good = (checked - bad) as f32;
            let frac = (good / total).clamp(0.0, 1.0);
            v.intact_fraction = Some(match v.intact_fraction {
                Some(old) => old.min(frac),
                None => frac,
            });
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    /// A 2x2 GIF with one frame, written by hand. LZW: min code size 2,
    /// codes: clear(4), 0, 1, 2, 3, eoi(5), 3 bits each.
    fn tiny_gif() -> Vec<u8> {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[2, 0, 2, 0, 0x80, 0, 0]); // 2x2, GCT of 2 entries
        g.extend_from_slice(&[0, 0, 0, 255, 255, 255]);
        g.extend_from_slice(&[0x21, 0xF9, 4, 0, 10, 0, 0, 0]); // GCE
        g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 2, 0, 2, 0, 0]);
        g.push(2); // min code size
                   // codes (3 bits each, LSB first): 4,0,1,0,5 -> pixels 0,1,0 ... build below
                   // The code width grows exactly as the decoder's does: one dictionary
                   // entry is added per code after the first following a clear code.
        let codes = [4u32, 0, 1, 0, 1, 5];
        let (mut acc, mut n, mut width, mut next) = (0u32, 0u32, 3u32, 6u32);
        let mut data = Vec::new();
        for (i, c) in codes.into_iter().enumerate() {
            acc |= c << n;
            n += width;
            while n >= 8 {
                data.push((acc & 0xFF) as u8);
                acc >>= 8;
                n -= 8;
            }
            if i >= 2 && c != 5 {
                next += 1;
                if next == (1 << width) {
                    width += 1;
                }
            }
        }
        if n > 0 {
            data.push(acc as u8);
        }
        g.push(data.len() as u8);
        g.extend_from_slice(&data);
        g.push(0);
        g.push(0x3B);
        g
    }

    fn mem(b: Vec<u8>) -> SharedSource {
        Arc::new(MemSource::new(b))
    }

    #[test]
    fn measures_a_hand_built_gif() {
        let g = tiny_gif();
        let m = GifFormat
            .measure(&mem(g.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, g.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert_eq!(m.meta.width, Some(2));
        assert_eq!(m.meta.extra["frames"], "1");
    }

    #[test]
    fn lzw_pixel_count_is_checked() {
        let g = tiny_gif();
        let v = GifFormat.validate(&mem(g)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
        // Claim a bigger frame than the data decodes to.
        let mut g = tiny_gif();
        let pos = g.iter().position(|&b| b == 0x2C).unwrap();
        g[pos + 5] = 9; // width 9 -> 9x2 pixels
        let v = GifFormat.validate(&mem(g)).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
    }

    #[test]
    fn rejects_foremost_style_false_positives() {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[10, 0, 10, 0, 0, 0, 0]);
        g.extend_from_slice(&[0x55; 100]);
        assert!(GifFormat
            .measure(&mem(g.clone()), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(!GifFormat.probe(&g));
        // Zero dimensions.
        let mut z = tiny_gif();
        z[6] = 0;
        z[7] = 0;
        assert!(GifFormat.measure(&mem(z), 0, u64::MAX).unwrap().is_none());
    }

    #[test]
    fn stops_at_the_trailer_and_reports_truncation() {
        let g = tiny_gif();
        let mut padded = g.clone();
        padded.extend_from_slice(&[0xAA; 500]);
        let m = GifFormat
            .measure(&mem(padded), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, g.len() as u64);
        let cut = g.len() - 4;
        let m = GifFormat
            .measure(&mem(g[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(cut as u64));
        assert_eq!(m.len, cut as u64);
    }

    #[test]
    fn limit_produces_capped() {
        let g = tiny_gif();
        let m = GifFormat.measure(&mem(g.clone()), 0, 30).unwrap().unwrap();
        assert_eq!(m.status, ExtentStatus::Capped);
        assert_eq!(m.len, 30);
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x9E37_79B9u32;
        for round in 0..3000 {
            let mut b = if round % 2 == 0 {
                tiny_gif()
            } else {
                b"GIF89a\x05\0\x05\0\x80\0\0".to_vec()
            };
            for _ in 0..(round % 90) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                b.push(x as u8);
            }
            if round % 3 == 0 && !b.is_empty() {
                let i = (x as usize) % b.len();
                b[i] ^= 0xFF;
            }
            let s = mem(b.clone());
            let _ = GifFormat.probe(&b);
            let _ = GifFormat.measure(&s, 0, u64::MAX);
            let _ = GifFormat.validate(&s);
        }
    }
}
