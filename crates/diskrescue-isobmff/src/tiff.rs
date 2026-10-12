//! Minimal TIFF/EXIF IFD reader for the metadata embedded in HEIF `Exif`
//! items and Canon CR3 `CMT1`/`CMT2` boxes.
//!
//! Reads IFD0 and, when present, the Exif sub-IFD (one level). Only the
//! tags needed for naming and sorting are decoded: Make, Model, Software,
//! Orientation, DateTime, DateTimeOriginal and the pixel dimensions. Every
//! offset is bounds-checked against the buffer; entry counts are capped.

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{ascii_trimmed, Endian};

/// Fields decoded from a TIFF structure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TiffInfo {
    /// Make (0x010F).
    pub make: Option<String>,
    /// Model (0x0110).
    pub model: Option<String>,
    /// Software (0x0131).
    pub software: Option<String>,
    /// Orientation (0x0112).
    pub orientation: Option<u8>,
    /// DateTime (0x0132).
    pub datetime: Option<Timestamp>,
    /// DateTimeOriginal (0x9003).
    pub datetime_original: Option<Timestamp>,
    /// Width (0x0100 or PixelXDimension 0xA002).
    pub width: Option<u32>,
    /// Height (0x0101 or PixelYDimension 0xA003).
    pub height: Option<u32>,
}

impl TiffInfo {
    /// Best capture time: DateTimeOriginal, else DateTime.
    pub fn created(&self) -> Option<Timestamp> {
        self.datetime_original
            .or(self.datetime)
            .filter(|t| t.is_plausible())
    }
}

fn type_size(t: u16) -> usize {
    match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// Parse a TIFF structure starting at `buf[0]` (`II*\0` / `MM\0*`). The
/// Exif sub-IFD is followed when present. Returns `None` when the header
/// is not TIFF.
pub fn parse_tiff(buf: &[u8]) -> Option<TiffInfo> {
    let e = match buf.get(0..4)? {
        [0x49, 0x49, 0x2A, 0x00] => Endian::Little,
        [0x4D, 0x4D, 0x00, 0x2A] => Endian::Big,
        _ => return None,
    };
    let ifd0 = e.u32(buf, 4)? as usize;
    let mut info = TiffInfo::default();
    let mut queue = vec![(ifd0, 0u8)];
    let mut visited = Vec::new();
    while let Some((off, depth)) = queue.pop() {
        if depth > 2 || visited.contains(&off) || visited.len() > 8 {
            continue;
        }
        visited.push(off);
        let Some(n) = e.u16(buf, off) else { continue };
        if n == 0 || n > 512 {
            continue;
        }
        for i in 0..n as usize {
            let at = off + 2 + i * 12;
            let (Some(tag), Some(typ), Some(count)) =
                (e.u16(buf, at), e.u16(buf, at + 2), e.u32(buf, at + 4))
            else {
                break;
            };
            let sz = type_size(typ).saturating_mul(count as usize);
            let value_at = if sz <= 4 {
                at + 8
            } else {
                match e.u32(buf, at + 8) {
                    Some(o) => o as usize,
                    None => continue,
                }
            };
            let Some(value) = buf.get(value_at..value_at.saturating_add(sz)) else {
                continue;
            };
            let int = || -> Option<u32> {
                match typ {
                    3 => e.u16(value, 0).map(u32::from),
                    4 => e.u32(value, 0),
                    _ => None,
                }
            };
            let text = || -> Option<String> {
                (typ == 2)
                    .then(|| ascii_trimmed(value))
                    .filter(|s| !s.is_empty())
            };
            match tag {
                0x010F => info.make = info.make.take().or_else(text),
                0x0110 => info.model = info.model.take().or_else(text),
                0x0131 => info.software = info.software.take().or_else(text),
                0x0112 => info.orientation = int().filter(|v| (1..=8).contains(v)).map(|v| v as u8),
                0x0132 => {
                    info.datetime = text().and_then(|s| Timestamp::from_exif_str(&s));
                }
                0x9003 => {
                    info.datetime_original = text().and_then(|s| Timestamp::from_exif_str(&s));
                }
                0x0100 | 0xA002 => {
                    if info.width.is_none() {
                        info.width = int().filter(|&v| v > 0);
                    }
                }
                0x0101 | 0xA003 => {
                    if info.height.is_none() {
                        info.height = int().filter(|&v| v > 0);
                    }
                }
                0x8769 => {
                    if let Some(o) = int() {
                        queue.push((o as usize, depth + 1));
                    }
                }
                _ => {}
            }
        }
    }
    Some(info)
}

/// Build a small big- or little-endian TIFF with ASCII and SHORT tags (used
/// by tests and fixture builders).
pub fn build_tiff(little: bool, ascii: &[(u16, &str)], exif_ascii: &[(u16, &str)]) -> Vec<u8> {
    let e = |v: u32, n: usize| -> Vec<u8> {
        let b = v.to_be_bytes();
        let s = &b[4 - n..];
        if little {
            s.iter().rev().copied().collect()
        } else {
            s.to_vec()
        }
    };
    let mut out = if little {
        b"II*\0".to_vec()
    } else {
        b"MM\0*".to_vec()
    };
    out.extend(e(8, 4));
    let n0 = ascii.len() + usize::from(!exif_ascii.is_empty());
    let ifd0_len = 2 + 12 * n0 + 4;
    let exif_off = 8 + ifd0_len;
    let exif_len = if exif_ascii.is_empty() {
        0
    } else {
        2 + 12 * exif_ascii.len() + 4
    };
    let mut data_off = exif_off + exif_len;
    let mut data = Vec::new();
    let entries =
        |tags: &[(u16, &str)], out: &mut Vec<u8>, data: &mut Vec<u8>, data_off: &mut usize| {
            for (tag, s) in tags {
                let mut bytes = s.as_bytes().to_vec();
                bytes.push(0);
                out.extend(e(*tag as u32, 2));
                out.extend(e(2, 2));
                out.extend(e(bytes.len() as u32, 4));
                if bytes.len() <= 4 {
                    bytes.resize(4, 0);
                    out.extend(bytes);
                } else {
                    out.extend(e(*data_off as u32, 4));
                    *data_off += bytes.len();
                    data.extend(bytes);
                }
            }
        };
    out.extend(e(n0 as u32, 2));
    entries(ascii, &mut out, &mut data, &mut data_off);
    if !exif_ascii.is_empty() {
        out.extend(e(0x8769, 2));
        out.extend(e(4, 2));
        out.extend(e(1, 4));
        out.extend(e(exif_off as u32, 4));
    }
    out.extend(e(0, 4));
    if !exif_ascii.is_empty() {
        out.extend(e(exif_ascii.len() as u32, 2));
        entries(exif_ascii, &mut out, &mut data, &mut data_off);
        out.extend(e(0, 4));
    }
    out.extend(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_both_endians() {
        for little in [true, false] {
            let t = build_tiff(
                little,
                &[(0x010F, "Canon"), (0x0110, "Canon EOS R5")],
                &[(0x9003, "2023:07:14 09:05:59")],
            );
            let i = parse_tiff(&t).unwrap();
            assert_eq!(i.make.as_deref(), Some("Canon"));
            assert_eq!(i.model.as_deref(), Some("Canon EOS R5"));
            assert_eq!(i.created().unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
        }
        assert!(parse_tiff(b"nope").is_none());
        // Self-referencing IFD loop terminates.
        let mut t = b"II*\0\x08\0\0\0\x01\0\x69\x87\x04\0\x01\0\0\0\x08\0\0\0\0\0\0\0".to_vec();
        t.extend_from_slice(&[0; 16]);
        assert!(parse_tiff(&t).is_some());
    }
}
