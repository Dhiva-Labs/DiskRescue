//! User metadata inside `moov`: QuickTime `udta` text atoms (`©mak`,
//! `©mod`, `©swr`, `©day`, `©xyz`), iTunes-style `udta/meta/ilst`, Apple
//! `moov/meta` with `keys` (`com.apple.quicktime.make` ...) and the 3GPP
//! asset boxes (`titl`, `auth`, `loci`, `yrrc` ...).
//!
//! Every parser is bounded by its slice; unknown items are ignored. Cover
//! art (`covr`) is reported as absolute byte ranges so the carver can mark
//! the embedded JPEG/PNG as nested.

use std::collections::BTreeMap;

use diskrescue_core::time::{days_from_civil, Timestamp};
use diskrescue_core::util::bytes::{u16be, u32be};

use super::{BoxIter, SliceBox};

/// An embedded cover image (absolute byte range in the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArt {
    /// Absolute offset of the image bytes.
    pub offset: u64,
    /// Length of the image bytes.
    pub len: u64,
    /// `jpeg`, `png` or `bmp` when the data type says so.
    pub format: Option<&'static str>,
}

/// Metadata gathered from `udta` / `meta`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetaInfo {
    /// Camera / phone make.
    pub make: Option<String>,
    /// Camera / phone model.
    pub model: Option<String>,
    /// Software or encoder string.
    pub software: Option<String>,
    /// Capture time from `©day` / `com.apple.quicktime.creationdate`.
    pub creation_time: Option<Timestamp>,
    /// ISO 6709 location string (`+37.3349-122.0090+052.123/`) or a 3GPP
    /// `loci` description.
    pub location: Option<String>,
    /// Title.
    pub title: Option<String>,
    /// Other textual items (`artist`, `comment`, raw keys...).
    pub extra: BTreeMap<String, String>,
    /// Embedded cover images.
    pub cover_art: Vec<CoverArt>,
}

impl MetaInfo {
    fn set_text(&mut self, key: &str, value: String) {
        let value = value.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if value.is_empty() {
            return;
        }
        let v = value.to_string();
        match key {
            "make" => {
                self.make.get_or_insert(v);
            }
            "model" => {
                self.model.get_or_insert(v);
            }
            "software" => {
                self.software.get_or_insert(v);
            }
            "date" => {
                if self.creation_time.is_none() {
                    if let Some(ts) = parse_iso8601(&v) {
                        self.creation_time = Some(ts);
                    }
                }
                self.extra.entry("date".into()).or_insert(v);
            }
            "location" => {
                self.location.get_or_insert(v);
            }
            "title" => {
                self.title.get_or_insert(v);
            }
            other => {
                if self.extra.len() < 64 {
                    self.extra.entry(other.to_string()).or_insert(v);
                }
            }
        }
    }
}

/// Map a QuickTime / iTunes 4CC item to a [`MetaInfo`] key.
fn item_key(kind: &[u8; 4]) -> Option<&'static str> {
    Some(match kind {
        b"\xA9mak" => "make",
        b"\xA9mod" => "model",
        b"\xA9swr" | b"\xA9too" | b"\xA9enc" => "software",
        b"\xA9day" => "date",
        b"\xA9xyz" => "location",
        b"\xA9nam" => "title",
        b"\xA9ART" | b"\xA9aut" => "artist",
        b"\xA9cmt" => "comment",
        b"\xA9des" => "description",
        b"\xA9alb" => "album",
        b"\xA9inf" => "information",
        _ => return None,
    })
}

/// Map an Apple `mdta` key to a [`MetaInfo`] key.
fn mdta_key(name: &str) -> &str {
    match name {
        "com.apple.quicktime.make" | "com.android.manufacturer" => "make",
        "com.apple.quicktime.model" | "com.android.model" => "model",
        "com.apple.quicktime.software" | "com.android.version" => "software",
        "com.apple.quicktime.creationdate" | "com.android.capture.time" => "date",
        "com.apple.quicktime.location.ISO6709" => "location",
        "com.apple.quicktime.title" => "title",
        other => other,
    }
}

/// Decode a text value: UTF-8, or UTF-16 with a BOM.
fn decode_text(b: &[u8]) -> String {
    if b.len() >= 2 && (b[..2] == [0xFE, 0xFF] || b[..2] == [0xFF, 0xFE]) {
        let be = b[0] == 0xFE;
        let units: Vec<u16> = b[2..]
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
        return String::from_utf16_lossy(&units);
    }
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Values found in the `data` children of an `ilst` item.
fn data_values<'a>(item: &SliceBox<'a>) -> Vec<(u32, u64, &'a [u8])> {
    let mut out = Vec::new();
    for d in BoxIter::new(item.payload, item.header.payload_offset()).take(64) {
        if &d.header.kind != b"data" || d.payload.len() < 8 {
            continue;
        }
        let typ = u32be(d.payload, 0).unwrap_or(0) & 0x00FF_FFFF;
        out.push((typ, d.header.payload_offset() + 8, &d.payload[8..]));
    }
    out
}

fn image_format(typ: u32, bytes: &[u8]) -> Option<&'static str> {
    match typ {
        13 => Some("jpeg"),
        14 => Some("png"),
        27 => Some("bmp"),
        _ => {
            if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
                Some("jpeg")
            } else if bytes.starts_with(b"\x89PNG") {
                Some("png")
            } else {
                None
            }
        }
    }
}

/// Collect cover art from an `ilst` payload (absolute offset `base`).
pub fn parse_ilst_cover(ilst: &[u8], base: u64) -> Vec<CoverArt> {
    let mut out = Vec::new();
    for item in BoxIter::new(ilst, base).take(4096) {
        if &item.header.kind != b"covr" {
            continue;
        }
        for (typ, off, bytes) in data_values(&item) {
            out.push(CoverArt {
                offset: off,
                len: bytes.len() as u64,
                format: image_format(typ, bytes),
            });
        }
    }
    out
}

/// Parse an `ilst` box payload. `keys` maps 1-based key indices to names
/// (Apple `mdta` metadata); empty for iTunes-style lists.
fn parse_ilst(ilst: &[u8], base: u64, keys: &[String], meta: &mut MetaInfo) {
    for item in BoxIter::new(ilst, base).take(4096) {
        let kind = item.header.kind;
        if &kind == b"covr" {
            continue;
        }
        let key: Option<String> = if !keys.is_empty() {
            let idx = u32::from_be_bytes(kind) as usize;
            keys.get(idx.wrapping_sub(1))
                .map(|k| mdta_key(k).to_string())
        } else {
            item_key(&kind).map(str::to_string)
        };
        let Some(key) = key else { continue };
        for (typ, _, bytes) in data_values(&item) {
            // Text types: 0 implicit, 1 UTF-8, 2 UTF-16.
            if typ <= 2 {
                meta.set_text(&key, decode_text(bytes));
            }
        }
    }
    meta.cover_art.extend(parse_ilst_cover(ilst, base));
}

/// Parse a `keys` box payload (FullBox) into key names.
fn parse_keys(p: &[u8]) -> Vec<String> {
    let mut keys = Vec::new();
    let count = u32be(p, 4).unwrap_or(0).min(4096);
    let mut pos = 8usize;
    for _ in 0..count {
        let Some(size) = u32be(p, pos) else { break };
        let size = size as usize;
        if size < 8 || pos + size > p.len() {
            break;
        }
        keys.push(String::from_utf8_lossy(&p[pos + 8..pos + size]).into_owned());
        pos += size;
    }
    keys
}

/// Parse a `meta` box (either form). `payload` starts right after the box
/// header; `base` is its absolute offset.
pub(crate) fn parse_meta_box(payload: &[u8], base: u64, meta: &mut MetaInfo) {
    let skip = match payload.get(4..8) {
        Some(k) if k == b"hdlr" || k == b"keys" || k == b"ilst" => 0usize,
        _ => 4,
    };
    let Some(body) = payload.get(skip..) else {
        return;
    };
    let base = base + skip as u64;
    let mut keys = Vec::new();
    for b in BoxIter::new(body, base).take(256) {
        if &b.header.kind == b"keys" {
            keys = parse_keys(b.payload);
        }
    }
    for b in BoxIter::new(body, base).take(256) {
        if &b.header.kind == b"ilst" {
            parse_ilst(b.payload, b.header.payload_offset(), &keys, meta);
        }
    }
}

/// Parse a `udta` payload.
pub(crate) fn parse_udta(payload: &[u8], base: u64, meta: &mut MetaInfo) {
    for b in BoxIter::new(payload, base).take(4096) {
        let kind = b.header.kind;
        if &kind == b"meta" {
            parse_meta_box(b.payload, b.header.payload_offset(), meta);
            continue;
        }
        if let Some(key) = item_key(&kind) {
            // QuickTime text atom: u16 length, u16 language, text.
            if let Some(len) = u16be(b.payload, 0) {
                let len = len as usize;
                if len + 4 <= b.payload.len() && len > 0 {
                    meta.set_text(key, decode_text(&b.payload[4..4 + len]));
                    continue;
                }
            }
            for (_, _, bytes) in data_values(&b) {
                meta.set_text(key, decode_text(bytes));
            }
            continue;
        }
        // 3GPP asset boxes: FullBox, u16 language, string.
        let key3 = match &kind {
            b"titl" => Some("title"),
            b"auth" => Some("artist"),
            b"cprt" => Some("copyright"),
            b"dscp" => Some("description"),
            b"perf" => Some("performer"),
            b"albm" => Some("album"),
            b"gnre" => Some("genre"),
            _ => None,
        };
        if let Some(k) = key3 {
            if b.payload.len() > 6 {
                meta.set_text(k, decode_text(&b.payload[6..]));
            }
            continue;
        }
        match &kind {
            b"loci" => {
                if b.payload.len() > 6 {
                    let name = decode_text(&b.payload[6..]);
                    let name = if name.is_empty() {
                        "3gpp location".to_string()
                    } else {
                        name
                    };
                    meta.set_text("location", name);
                }
            }
            b"yrrc" => {
                if let Some(y) = u16be(b.payload, 4) {
                    meta.extra.entry("year".into()).or_insert(y.to_string());
                }
            }
            _ => {}
        }
    }
}

/// Parse an ISO 8601 date-time as used by `©day` and Apple creation dates:
/// `YYYY-MM-DDTHH:MM:SS` with optional fraction and `Z` / `±HH:MM` /
/// `±HHMM` zone. A bare date is accepted (midnight UTC). Returns `None` for
/// anything else or an implausible year.
pub(crate) fn parse_iso8601(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |a: usize, l: usize| -> Option<i64> {
        let t = s.get(a..a + l)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let year = num(0, 4)?;
    if b.get(4) != Some(&b'-') {
        return None;
    }
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1900..=2200).contains(&year) {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let mut secs = days * 86_400;
    if b.len() >= 19 && (b[10] == b'T' || b[10] == b' ') {
        let hh = num(11, 2)?;
        let mm = num(14, 2)?;
        let ss = num(17, 2)?;
        if hh > 23 || mm > 59 || ss > 60 {
            return None;
        }
        secs += hh * 3600 + mm * 60 + ss;
        let mut i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        match b.get(i) {
            Some(&c) if c == b'+' || c == b'-' => {
                let sign = if c == b'+' { 1 } else { -1 };
                let zh = num(i + 1, 2)?;
                let zm = if b.get(i + 3) == Some(&b':') {
                    num(i + 4, 2).unwrap_or(0)
                } else {
                    num(i + 3, 2).unwrap_or(0)
                };
                secs -= sign * (zh * 3600 + zm * 60);
            }
            _ => {}
        }
    }
    let ts = Timestamp::from_unix(secs);
    ts.is_plausible().then_some(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn data(typ: u32, value: &[u8]) -> Vec<u8> {
        let mut p = typ.to_be_bytes().to_vec();
        p.extend_from_slice(&[0; 4]);
        p.extend_from_slice(value);
        bx(b"data", &p)
    }

    #[test]
    fn iso8601() {
        let t = parse_iso8601("2024-05-01T12:30:00+0200").unwrap();
        assert_eq!(t.to_rfc3339(), "2024-05-01T10:30:00Z");
        let t = parse_iso8601("2024-05-01T12:30:00.123Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2024-05-01T12:30:00Z");
        assert!(parse_iso8601("2024").is_none());
        assert!(parse_iso8601("garbage").is_none());
        assert_eq!(
            parse_iso8601("2023-07-14").unwrap().to_rfc3339(),
            "2023-07-14T00:00:00Z"
        );
    }

    #[test]
    fn quicktime_text_and_keys() {
        let mut mak = 5u16.to_be_bytes().to_vec();
        mak.extend_from_slice(&[0x55, 0xC4]);
        mak.extend_from_slice(b"Canon");
        let udta = bx(b"\xA9mak", &mak);
        let mut m = MetaInfo::default();
        parse_udta(&udta, 0, &mut m);
        assert_eq!(m.make.as_deref(), Some("Canon"));

        // Apple keys + ilst
        let key = |name: &str| bx(b"mdta", name.as_bytes());
        let mut keys = vec![0u8; 4];
        keys.extend_from_slice(&2u32.to_be_bytes());
        keys.extend(key("com.apple.quicktime.model"));
        keys.extend(key("com.apple.quicktime.creationdate"));
        let ilst = [
            bx(&1u32.to_be_bytes(), &data(1, b"iPhone 15")),
            bx(&2u32.to_be_bytes(), &data(1, b"2024-01-02T03:04:05Z")),
            bx(b"covr", &data(13, &[0xFF, 0xD8, 0xFF, 0xE0])),
        ]
        .concat();
        let meta = [bx(b"keys", &keys), bx(b"ilst", &ilst)].concat();
        let mut m = MetaInfo::default();
        parse_meta_box(&meta, 1000, &mut m);
        assert_eq!(m.model.as_deref(), Some("iPhone 15"));
        assert_eq!(
            m.creation_time.unwrap().to_rfc3339(),
            "2024-01-02T03:04:05Z"
        );
        assert_eq!(m.cover_art.len(), 1);
        assert_eq!(m.cover_art[0].len, 4);
        assert_eq!(m.cover_art[0].format, Some("jpeg"));
    }
}
