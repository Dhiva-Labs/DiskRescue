//! MP3 (MPEG audio layers I-III) with ID3v2/ID3v1/APEv2 tags.
//!
//! # Algorithm
//!
//! * An optional ID3v2 tag opens the file: `ID3`, version 2/3/4, flags,
//!   four syncsafe size bytes. Its frames are read for title, artist, album,
//!   year and attached pictures (`APIC`/`PIC`), which are reported as nested
//!   `CoverArt` objects (JPEG or PNG, checked by magic bytes).
//! * Frames follow. Each header (sync, version, layer, bitrate, sample rate,
//!   padding) yields the frame length; the next header must sit exactly there
//!   and agree on version, layer and sample rate. At least eight consecutive
//!   frames are required for a hit. A leading `Xing`/`Info`/`VBRI` frame is
//!   recognised and not counted as audio.
//! * The walk ends at the first byte that is not a consistent frame. An
//!   ID3v1 tag (`TAG`, 128 bytes) or an APEv2 tag right after the last frame
//!   is included and makes the file `Complete`; otherwise the file has no end
//!   marker and the status is `UnknownLength`. A frame cut by the end of the
//!   data is `Truncated`.
//!
//! Limits: free-format bitrate streams are not recognised (index 0), and a
//! second MP3 with identical parameters directly behind the first (and no
//! ID3 tag between them) is walked into.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Range, Result, SharedSource};

use crate::io::{note_unreadable, Window};
use crate::meta::{clean_string, EXTENSION_KEY};
use crate::mpa::{parse_header, MpaHeader};

const MAX_SIZE: u64 = 512 << 20;
/// Frames required for a hit.
const MIN_FRAMES: u64 = 8;
/// Zero padding tolerated between an ID3v2 tag and the first frame.
const MAX_ZERO_GAP: u64 = 64 * 1024;
/// Largest ID3v2 tag accepted (guards the size field of hostile files).
const MAX_ID3_TAG: u64 = 128 * 1024 * 1024;

/// Information from ID3v2 tags.
#[derive(Debug, Default, Clone)]
struct Id3 {
    /// Total bytes of all leading tags.
    len: u64,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    year: Option<String>,
    covers: Vec<Nested>,
    notes: Vec<String>,
}

fn syncsafe(b: &[u8]) -> Option<u32> {
    if b.len() < 4 || b[..4].iter().any(|&x| x & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(b[0]) << 21)
            | (u32::from(b[1]) << 14)
            | (u32::from(b[2]) << 7)
            | u32::from(b[3]),
    )
}

/// Validate an ID3v2 header; returns `(version, flags, size)`.
fn id3_header(h: &[u8]) -> Option<(u8, u8, u32)> {
    if h.len() < 10 || &h[..3] != b"ID3" {
        return None;
    }
    let ver = h[3];
    if !(2..=4).contains(&ver) || h[4] == 0xFF {
        return None;
    }
    if h[5] & 0x0F != 0 {
        return None;
    }
    Some((ver, h[5], syncsafe(&h[6..10])?))
}

fn decode_text(enc: u8, b: &[u8]) -> Option<String> {
    let s = match enc {
        0 => b.iter().map(|&c| c as char).collect::<String>(),
        1 | 2 => {
            let (be, body) = match b {
                [0xFF, 0xFE, rest @ ..] => (false, rest),
                [0xFE, 0xFF, rest @ ..] => (true, rest),
                _ => (enc == 2, b),
            };
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(b).to_string(),
    };
    let s = s
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s.chars().take(200).collect())
    }
}

fn image_format(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else {
        None
    }
}

/// Offset just past a terminated string in `b` starting at `from`.
fn skip_terminated(b: &[u8], from: usize, enc: u8) -> Option<usize> {
    if enc == 1 || enc == 2 {
        let mut i = from;
        while i + 2 <= b.len() {
            if b[i] == 0 && b[i + 1] == 0 {
                return Some(i + 2);
            }
            i += 2;
        }
        None
    } else {
        b.get(from..)?
            .iter()
            .position(|&c| c == 0)
            .map(|p| from + p + 1)
    }
}

/// Parse the ID3v2 tags at the start of the window.
fn parse_id3(win: &mut Window<'_>) -> Id3 {
    let mut id3 = Id3::default();
    for _ in 0..4 {
        let Some(h) = win.vec(id3.len, 10) else { break };
        let Some((ver, flags, size)) = id3_header(&h) else {
            break;
        };
        let footer = ver == 4 && flags & 0x10 != 0;
        let body = u64::from(size);
        let total = 10 + body + if footer { 10 } else { 0 };
        if body > MAX_ID3_TAG || id3.len + total > win.avail() {
            break;
        }
        let start = id3.len;
        id3.len += total;
        if flags & 0x80 != 0 && ver != 4 {
            id3.notes
                .push("ID3v2 tag uses unsynchronisation; frames not read".into());
            continue;
        }
        let end = start + 10 + body;
        let mut p = start + 10;
        // Extended header.
        if flags & 0x40 != 0 {
            if let Some(eh) = win.vec(p, 4) {
                let skip = if ver == 4 {
                    u64::from(syncsafe(&eh).unwrap_or(0))
                } else {
                    u64::from(u32::from_be_bytes([eh[0], eh[1], eh[2], eh[3]])) + 4
                };
                p += skip.min(body);
            }
        }
        let mut n = 0;
        while n < 512 {
            n += 1;
            let (id, fsize, hdr) = if ver == 2 {
                let Some(f) = win.vec(p, 6) else { break };
                let sz = (u64::from(f[3]) << 16) | (u64::from(f[4]) << 8) | u64::from(f[5]);
                (f[..3].to_vec(), sz, 6u64)
            } else {
                let Some(f) = win.vec(p, 10) else { break };
                let sz = if ver == 4 {
                    match syncsafe(&f[4..8]) {
                        Some(v) => u64::from(v),
                        None => break,
                    }
                } else {
                    u64::from(u32::from_be_bytes([f[4], f[5], f[6], f[7]]))
                };
                (f[..4].to_vec(), sz, 10u64)
            };
            if id[0] == 0
                || !id
                    .iter()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                break;
            }
            if p + hdr + fsize > end {
                break;
            }
            let data_pos = p + hdr;
            match &id[..] {
                b"TIT2" | b"TT2" => id3.title = text_frame(win, data_pos, fsize),
                b"TPE1" | b"TP1" => id3.artist = text_frame(win, data_pos, fsize),
                b"TALB" | b"TAL" => id3.album = text_frame(win, data_pos, fsize),
                b"TYER" | b"TYE" | b"TDRC" => {
                    if id3.year.is_none() {
                        id3.year =
                            text_frame(win, data_pos, fsize).map(|s| s.chars().take(10).collect());
                    }
                }
                b"APIC" | b"PIC" => {
                    if let Some(n) = picture_frame(win, data_pos, fsize, ver == 2) {
                        id3.covers.push(n);
                    }
                }
                _ => {}
            }
            p += hdr + fsize;
        }
    }
    id3
}

fn text_frame(win: &mut Window<'_>, pos: u64, size: u64) -> Option<String> {
    if size < 2 {
        return None;
    }
    let n = size.min(512) as usize;
    let b = win.vec(pos, n)?;
    decode_text(b[0], &b[1..])
}

fn picture_frame(win: &mut Window<'_>, pos: u64, size: u64, v22: bool) -> Option<Nested> {
    let n = size.min(1024) as usize;
    let b = win.vec(pos, n)?;
    let enc = *b.first()?;
    let mut i = 1;
    if v22 {
        i += 3; // three-letter image format
    } else {
        i = skip_terminated(&b, i, 0)?; // MIME type
    }
    i += 1; // picture type
    i = skip_terminated(&b, i, enc)?; // description
    let data_start = pos + i as u64;
    let data_end = pos + size;
    if data_start >= data_end {
        return None;
    }
    let magic = win.get(data_start, 8).to_vec();
    let format = image_format(&magic)?;
    Some(Nested {
        range: Range::new(data_start, data_end),
        format: Some(format.to_string()),
        role: NestedRole::CoverArt,
    })
}

/// Result of walking the audio frames.
#[derive(Debug)]
struct Frames {
    first: MpaHeader,
    /// Offset of the first frame.
    start: u64,
    /// Offset just past the last good frame.
    end: u64,
    count: u64,
    /// A leading Xing/Info/VBRI frame was found.
    vbr_header: bool,
    xing_frames: Option<u32>,
    audio_frames: u64,
    samples: u64,
    min_bitrate: u32,
    max_bitrate: u32,
    /// What follows the last frame.
    stop: FrameStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameStop {
    /// Data ended exactly after a frame.
    Eof,
    /// The caller's limit ended the walk.
    Limit,
    /// The next bytes are not a consistent frame.
    Other,
    /// A consistent header whose frame does not fit in the data.
    Partial,
    Fault,
}

fn walk_frames(win: &mut Window<'_>, start: u64) -> Option<Frames> {
    let avail = win.avail();
    let h0 = win.get(start, 4);
    let first = parse_header(h0)?;
    let mut f = Frames {
        first,
        start,
        end: start,
        count: 0,
        vbr_header: false,
        xing_frames: None,
        audio_frames: 0,
        samples: 0,
        min_bitrate: u32::MAX,
        max_bitrate: 0,
        stop: FrameStop::Other,
    };
    let mut pos = start;
    loop {
        if pos >= avail {
            f.stop = if win.limited() {
                FrameStop::Limit
            } else {
                FrameStop::Eof
            };
            break;
        }
        let (parsed, hb_len, hb0) = {
            let hb = win.get(pos, 4);
            (parse_header(hb), hb.len(), hb.first().copied())
        };
        let Some(h) = parsed.filter(|h| h.same_stream(&first)) else {
            f.stop = if win.fault().is_some() {
                FrameStop::Fault
            } else if hb_len < 4 && !win.limited() && hb0 == Some(0xFF) {
                FrameStop::Partial
            } else if hb_len < 4 && win.limited() {
                FrameStop::Limit
            } else {
                FrameStop::Other
            };
            break;
        };
        let flen = h.frame_len as u64;
        if pos + flen > avail {
            f.stop = if win.limited() {
                FrameStop::Limit
            } else {
                FrameStop::Partial
            };
            break;
        }
        if f.count == 0 {
            // Xing / Info / VBRI marks a metadata frame, not audio.
            let side = 4 + if h.crc_protected { 2 } else { 0 } + h.side_info_len();
            let probe = win.get(pos, (side + 16).min(h.frame_len)).to_vec();
            if let Some(tag) = probe.get(side..side + 4) {
                if tag == b"Xing" || tag == b"Info" {
                    f.vbr_header = true;
                    let flags = u32::from_be_bytes(
                        probe
                            .get(side + 4..side + 8)
                            .and_then(|s| s.try_into().ok())
                            .unwrap_or([0; 4]),
                    );
                    if flags & 1 != 0 {
                        f.xing_frames = probe
                            .get(side + 8..side + 12)
                            .and_then(|s| s.try_into().ok())
                            .map(u32::from_be_bytes);
                    }
                }
            }
            if !f.vbr_header {
                if let Some(v) = win.get(pos + 36, 18).get(..18) {
                    if &v[..4] == b"VBRI" {
                        f.vbr_header = true;
                        f.xing_frames = Some(u32::from_be_bytes([v[14], v[15], v[16], v[17]]));
                    }
                }
            }
        }
        if !(f.count == 0 && f.vbr_header) {
            f.audio_frames += 1;
            f.samples += u64::from(h.samples);
            f.min_bitrate = f.min_bitrate.min(h.bitrate);
            f.max_bitrate = f.max_bitrate.max(h.bitrate);
        }
        f.count += 1;
        pos += flen;
        f.end = pos;
    }
    Some(f)
}

/// Trailing tag after the last frame: returns the byte length to absorb.
fn trailing_tag(win: &mut Window<'_>, pos: u64) -> (u64, bool) {
    // APEv2 (with header) then ID3v1, in either order of presence.
    let mut len = 0u64;
    let mut v1 = false;
    if let Some(h) = win.vec(pos, 32) {
        if &h[..8] == b"APETAGEX" {
            let size = u64::from(u32::from_le_bytes([h[12], h[13], h[14], h[15]]));
            let flags = u32::from_le_bytes([h[20], h[21], h[22], h[23]]);
            if flags & 0x8000_0000 != 0 && (32..=16 * 1024 * 1024).contains(&size) {
                len += 32 + size;
            }
        }
    }
    if let Some(t) = win.exact(pos + len, 3) {
        if t == b"TAG" && win.avail() >= pos + len + 128 {
            len += 128;
            v1 = true;
        }
    }
    (len, v1)
}

struct Analysis {
    id3: Id3,
    frames: Frames,
    tail_len: u64,
    id3v1: bool,
}

fn analyse(win: &mut Window<'_>) -> Option<Analysis> {
    let id3 = parse_id3(win);
    let mut start = id3.len;
    // Zero padding between the tag and the first frame.
    if id3.len > 0 {
        let mut gap = 0u64;
        while gap < MAX_ZERO_GAP && win.u8(start + gap) == Some(0) {
            gap += 1;
        }
        start += gap;
    }
    let frames = walk_frames(win, start)?;
    if frames.count < MIN_FRAMES {
        return None;
    }
    let (tail_len, id3v1) = if frames.stop == FrameStop::Other || frames.stop == FrameStop::Eof {
        trailing_tag(win, frames.end)
    } else {
        (0, false)
    };
    Some(Analysis {
        id3,
        frames,
        tail_len,
        id3v1,
    })
}

fn build_meta(a: &Analysis) -> MediaMeta {
    let f = &a.frames;
    let mut m = MediaMeta {
        audio_codec: Some(f.first.codec().to_string()),
        ..MediaMeta::default()
    };
    if f.audio_frames > 0 {
        m.duration_ms = Some(f.samples * 1000 / u64::from(f.first.sample_rate));
    }
    m.extra.insert(EXTENSION_KEY.into(), "mp3".into());
    if f.first.layer != 3 {
        m.extra
            .insert(EXTENSION_KEY.into(), f.first.codec().to_string());
    }
    m.extra
        .insert("sample_rate".into(), f.first.sample_rate.to_string());
    m.extra
        .insert("channels".into(), f.first.channels.to_string());
    if f.min_bitrate != u32::MAX {
        if f.min_bitrate == f.max_bitrate {
            m.extra
                .insert("bitrate_kbps".into(), (f.min_bitrate / 1000).to_string());
        } else {
            m.extra.insert("vbr".into(), "true".into());
            if let Some(d) = m.duration_ms.filter(|&d| d > 0) {
                let bytes = f.end.saturating_sub(f.start);
                m.extra
                    .insert("bitrate_kbps".into(), (bytes * 8 / d).to_string());
            }
        }
    }
    m.extra.insert("frames".into(), f.audio_frames.to_string());
    if let Some(t) = &a.id3.title {
        m.extra.insert("title".into(), t.clone());
    }
    if let Some(t) = &a.id3.artist {
        m.extra.insert("artist".into(), t.clone());
    }
    if let Some(t) = &a.id3.album {
        m.extra.insert("album".into(), t.clone());
    }
    if let Some(t) = &a.id3.year {
        m.extra.insert("year".into(), t.clone());
    }
    m
}

/// Format handler for MP3 / MPEG audio files.
#[derive(Debug, Default, Clone, Copy)]
pub struct Mp3Format;

static SIGNATURES: [Signature; 7] = [
    Signature::at_start(b"ID3"),
    Signature::at_start(&[0xFF, 0xFB]),
    Signature::at_start(&[0xFF, 0xFA]),
    Signature::at_start(&[0xFF, 0xF3]),
    Signature::at_start(&[0xFF, 0xF2]),
    Signature::at_start(&[0xFF, 0xE3]),
    Signature::at_start(&[0xFF, 0xE2]),
];

impl FileFormat for Mp3Format {
    fn id(&self) -> &'static str {
        "mp3"
    }

    fn name(&self) -> &'static str {
        "MP3 audio"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mp3", "mp2", "mp1"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Audio
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        1024
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        20
    }

    fn probe(&self, head: &[u8]) -> bool {
        if head.starts_with(b"ID3") {
            return id3_header(head).is_some();
        }
        // Bare frames: two consecutive consistent headers when visible.
        let Some(h) = parse_header(head) else {
            return false;
        };
        match head.get(h.frame_len..h.frame_len + 4) {
            Some(n) => parse_header(n).is_some_and(|n| n.same_stream(&h)),
            None => true,
        }
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "jpg" | "png")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(a) = analyse(&mut win) else {
            return Ok(None);
        };
        let f = &a.frames;
        let len = f.end + a.tail_len;
        let mut notes = a.id3.notes.clone();
        let (status, err) = match f.stop {
            FrameStop::Fault => {
                notes.push("read error while walking frames".into());
                (ExtentStatus::Truncated, Some(f.end))
            }
            FrameStop::Partial => {
                notes.push("the last frame is cut short and was left out".into());
                (ExtentStatus::Truncated, Some(f.end))
            }
            FrameStop::Limit => (ExtentStatus::Capped, None),
            _ => {
                if a.id3v1 {
                    (ExtentStatus::Complete, None)
                } else {
                    (ExtentStatus::UnknownLength, None)
                }
            }
        };
        if let Some(x) = f.xing_frames {
            let found = f.audio_frames;
            if u64::from(x).abs_diff(found) > 1 && f.stop != FrameStop::Partial {
                notes.push(format!("VBR header says {x} frames, {found} were found"));
            }
        }
        let confidence = if a.id3.len == 0 && f.count < 16 {
            Confidence::LOW
        } else {
            Confidence::MEDIUM
        };
        let mut m = Measured {
            len,
            status,
            confidence,
            err_offset: err,
            meta: build_meta(&a),
            nested: a
                .id3
                .covers
                .iter()
                .filter(|n| n.range.end <= len)
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
                "not an MP3 file (fewer than 8 consecutive valid frames)",
            ));
        };
        let f = &a.frames;
        let end = f.end + a.tail_len;
        let mut problems = Vec::new();
        match f.stop {
            FrameStop::Partial => problems.push(format!(
                "the file ends inside a frame (last good frame ends at {})",
                f.end
            )),
            FrameStop::Fault => problems.push(format!("read error after offset {}", f.end)),
            _ => {
                if end < size {
                    problems.push(format!(
                        "frames stop at offset {end}; the {} bytes after it are not valid MPEG audio",
                        size - end
                    ));
                }
            }
        }
        if let Some(x) = f.xing_frames {
            if u64::from(x).abs_diff(f.audio_frames) > 1 && f.stop != FrameStop::Partial {
                problems.push(format!(
                    "VBR header says {x} frames but {} were found",
                    f.audio_frames
                ));
            }
        }
        let intact = if problems.is_empty() {
            1.0
        } else {
            (end as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
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

/// Used by tests of other modules.
#[allow(dead_code)]
pub(crate) fn text_of(b: &[u8]) -> Option<String> {
    clean_string(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn frame(len: usize) -> Vec<u8> {
        // MPEG-1 layer III 128 kbit/s 44.1 kHz, no padding: 417 bytes.
        let mut f = vec![0xFF, 0xFB, 0x90, 0x00];
        f.resize(len, 0);
        f
    }

    fn mem(d: Vec<u8>) -> SharedSource {
        Arc::new(diskrescue_core::source::MemSource::new(d))
    }

    fn mp3(n: usize) -> Vec<u8> {
        (0..n).flat_map(|_| frame(417)).collect()
    }

    #[test]
    fn bare_frames_need_eight() {
        let m = Mp3Format
            .measure(&mem(mp3(8)), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, 8 * 417);
        assert_eq!(m.status, ExtentStatus::UnknownLength);
        assert!(Mp3Format
            .measure(&mem(mp3(7)), 0, u64::MAX)
            .unwrap()
            .is_none());
    }

    #[test]
    fn id3v1_tail_completes_the_file() {
        let mut d = mp3(20);
        let n = d.len();
        d.extend_from_slice(b"TAG");
        d.extend_from_slice(&[0u8; 125]);
        d.extend_from_slice(&[0x55; 300]);
        let m = Mp3Format.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, n as u64 + 128);
        assert_eq!(m.status, ExtentStatus::Complete);
    }

    #[test]
    fn partial_last_frame_is_truncated() {
        let mut d = mp3(20);
        d.truncate(19 * 417 + 100);
        let m = Mp3Format.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, 19 * 417);
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(19 * 417));
    }

    #[test]
    fn id3v2_with_picture() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6, 7, 8];
        let mut apic = vec![0u8]; // latin1
        apic.extend_from_slice(b"image/jpeg\0");
        apic.push(3);
        apic.extend_from_slice(b"cover\0");
        apic.extend_from_slice(&jpeg);
        let mut tit = vec![0u8];
        tit.extend_from_slice(b"Song");
        let mut frames = Vec::new();
        for (id, body) in [(b"TIT2", &tit), (b"APIC", &apic)] {
            frames.extend_from_slice(id);
            frames.extend_from_slice(&(body.len() as u32).to_be_bytes());
            frames.extend_from_slice(&[0, 0]);
            frames.extend_from_slice(body);
        }
        frames.extend_from_slice(&[0u8; 64]); // padding
        let sz = frames.len() as u32;
        let mut d = b"ID3".to_vec();
        d.extend_from_slice(&[3, 0, 0]);
        d.extend_from_slice(&[
            (sz >> 21) as u8 & 0x7F,
            (sz >> 14) as u8 & 0x7F,
            (sz >> 7) as u8 & 0x7F,
            sz as u8 & 0x7F,
        ]);
        d.extend_from_slice(&frames);
        let tag_len = d.len();
        d.extend_from_slice(&mp3(10));
        let m = Mp3Format
            .measure(&mem(d.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, d.len() as u64);
        assert_eq!(m.meta.extra["title"], "Song");
        assert_eq!(m.nested.len(), 1);
        let n = &m.nested[0];
        assert_eq!(n.role, NestedRole::CoverArt);
        assert_eq!(&d[n.range.start as usize..n.range.end as usize], &jpeg);
        assert_eq!(n.format.as_deref(), Some("jpeg"));
        assert!(tag_len < d.len());
    }

    #[test]
    fn noise_and_ff_runs_are_not_mp3() {
        assert!(Mp3Format
            .measure(&mem(vec![0xFF; 5000]), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(!Mp3Format.probe(&[0xFF; 100]));
        assert!(Mp3Format
            .measure(&mem(vec![0; 5000]), 0, u64::MAX)
            .unwrap()
            .is_none());
    }
}
