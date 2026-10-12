//! Metadata extraction: IHDR geometry, text chunks and the `eXIf` chunk.
//!
//! Only small chunks are read (at most [`MAX_META_CHUNK`] bytes each, and at
//! most [`MAX_META_CHUNKS`] of them), so extraction costs a handful of tiny
//! reads however large the image is. Everything is bounds-checked; malformed
//! metadata is skipped, never an error.

use std::io::Read;

use diskrescue_core::media::MediaMeta;
use diskrescue_core::source::{Source, SourceExt};
use diskrescue_core::time::{days_from_civil, Timestamp};
use diskrescue_core::util::bytes::{ascii_trimmed, Endian};

use crate::chunks::ChunkWalk;

/// Largest metadata chunk that is read.
pub const MAX_META_CHUNK: u32 = 256 * 1024;

/// Most metadata chunks examined per file.
pub const MAX_META_CHUNKS: usize = 24;

/// Decompressed text beyond this many bytes is ignored.
const MAX_INFLATED_TEXT: u64 = 64 * 1024;

/// Fields pulled from an EXIF block.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ExifInfo {
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    pub orientation: Option<u8>,
    pub date_time_original: Option<String>,
    pub date_time_digitized: Option<String>,
    pub date_time: Option<String>,
}

/// Parse the TIFF structure of an `eXIf` chunk (an optional `Exif\0\0`
/// prefix is tolerated). Reads IFD0 and the Exif sub-IFD only.
pub(crate) fn parse_exif(data: &[u8]) -> ExifInfo {
    let data = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
    let mut info = ExifInfo::default();
    let endian = match data.get(0..2) {
        Some(b"II") => Endian::Little,
        Some(b"MM") => Endian::Big,
        _ => return info,
    };
    if endian.u16(data, 2) != Some(42) {
        return info;
    }
    let Some(ifd0) = endian.u32(data, 4) else {
        return info;
    };
    let mut exif_ifd: Option<u32> = None;
    read_ifd(
        data,
        ifd0 as usize,
        endian,
        &mut |tag, kind, count, field| match tag {
            0x010F => info.make = ascii_value(data, endian, kind, count, field),
            0x0110 => info.model = ascii_value(data, endian, kind, count, field),
            0x0131 => info.software = ascii_value(data, endian, kind, count, field),
            0x0132 => info.date_time = ascii_value(data, endian, kind, count, field),
            0x0112 if kind == 3 && count >= 1 => {
                info.orientation = endian
                    .u16(data, field)
                    .and_then(|v| u8::try_from(v).ok())
                    .filter(|v| (1..=8).contains(v));
            }
            0x8769 if kind == 4 => exif_ifd = endian.u32(data, field),
            _ => {}
        },
    );
    if let Some(off) = exif_ifd {
        read_ifd(
            data,
            off as usize,
            endian,
            &mut |tag, kind, count, field| match tag {
                0x9003 => info.date_time_original = ascii_value(data, endian, kind, count, field),
                0x9004 => info.date_time_digitized = ascii_value(data, endian, kind, count, field),
                _ => {}
            },
        );
    }
    info
}

/// Walk one IFD, calling `f(tag, type, count, offset_of_value_field)` for
/// each entry. Bounded to 512 entries.
fn read_ifd(data: &[u8], off: usize, endian: Endian, f: &mut dyn FnMut(u16, u16, u32, usize)) {
    let Some(n) = endian.u16(data, off) else {
        return;
    };
    for i in 0..(n as usize).min(512) {
        let e = off + 2 + i * 12;
        let (Some(tag), Some(kind), Some(count)) = (
            endian.u16(data, e),
            endian.u16(data, e + 2),
            endian.u32(data, e + 4),
        ) else {
            return;
        };
        f(tag, kind, count, e + 8);
    }
}

/// An ASCII IFD value (inline when it fits in four bytes, otherwise at the
/// offset stored in the value field).
fn ascii_value(data: &[u8], endian: Endian, kind: u16, count: u32, field: usize) -> Option<String> {
    if kind != 2 || count == 0 || count > 4096 {
        return None;
    }
    let count = count as usize;
    let bytes = if count <= 4 {
        data.get(field..field + count)?
    } else {
        let off = endian.u32(data, field)? as usize;
        data.get(off..off.checked_add(count)?)?
    };
    let s = ascii_trimmed(bytes);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn inflate_bounded(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(MAX_INFLATED_TEXT)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Decode a `tEXt`, `zTXt` or `iTXt` chunk into `(keyword, text)`.
pub(crate) fn parse_text(kind: &[u8; 4], data: &[u8]) -> Option<(String, String)> {
    let nul = data.iter().position(|&b| b == 0)?;
    let keyword = latin1(&data[..nul]);
    let rest = &data[nul + 1..];
    match kind {
        b"tEXt" => Some((keyword, latin1(rest))),
        b"zTXt" => {
            let (&method, comp) = rest.split_first()?;
            if method != 0 {
                return None;
            }
            Some((keyword, latin1(&inflate_bounded(comp)?)))
        }
        b"iTXt" => {
            let flag = *rest.first()?;
            let method = *rest.get(1)?;
            let rest = rest.get(2..)?;
            // Language tag and translated keyword, both NUL terminated.
            let n1 = rest.iter().position(|&b| b == 0)?;
            let rest = &rest[n1 + 1..];
            let n2 = rest.iter().position(|&b| b == 0)?;
            let text = &rest[n2 + 1..];
            let bytes = if flag == 1 {
                if method != 0 {
                    return None;
                }
                inflate_bounded(text)?
            } else {
                text.to_vec()
            };
            Some((keyword, String::from_utf8_lossy(&bytes).into_owned()))
        }
        _ => None,
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Parse the date formats seen in PNG text chunks: RFC 1123/2822
/// (`Tue, 07 Nov 2023 08:12:31 +0100`, the form the PNG specification
/// recommends), ISO 8601 (`2023-11-07T08:12:31Z`, `2023-11-07`) and the EXIF
/// `2023:11:07 08:12:31` form. Zone offsets are applied; unknown zone names
/// are treated as UTC.
pub(crate) fn parse_date(s: &str) -> Option<Timestamp> {
    let s = s.trim().trim_end_matches('\0');
    if s.len() < 8 {
        return None;
    }
    parse_iso(s)
        .or_else(|| parse_rfc2822(s))
        .or_else(|| Timestamp::from_exif_str(s))
}

fn digits(b: &[u8], from: usize, n: usize) -> Option<i64> {
    let s = b.get(from..from + n)?;
    if !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn zone_offset_secs(z: &str) -> i64 {
    let z = z.trim();
    let b = z.as_bytes();
    let sign = match b.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return 0,
    };
    let h = digits(b, 1, 2).unwrap_or(0);
    let m = if b.get(3) == Some(&b':') {
        digits(b, 4, 2)
    } else {
        digits(b, 3, 2)
    }
    .unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

fn valid_ymd(y: i64, mo: i64, d: i64) -> bool {
    (1900..=2200).contains(&y) && (1..=12).contains(&mo) && (1..=31).contains(&d)
}

fn parse_iso(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    let y = digits(b, 0, 4)?;
    if b.get(4) != Some(&b'-') {
        return None;
    }
    let mo = digits(b, 5, 2)?;
    if b.get(7) != Some(&b'-') {
        return None;
    }
    let d = digits(b, 8, 2)?;
    if !valid_ymd(y, mo, d) {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let mut secs = days * 86_400;
    if matches!(b.get(10), Some(b'T') | Some(b' ')) {
        let h = digits(b, 11, 2)?;
        let mi = digits(b, 14, 2)?;
        let se = digits(b, 17, 2).unwrap_or(0);
        if h > 23 || mi > 59 || se > 60 {
            return None;
        }
        secs += h * 3600 + mi * 60 + se;
        // Skip fractional seconds, then read the zone.
        let mut i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        if let Some(z) = s.get(i..) {
            secs -= zone_offset_secs(z);
        }
    }
    Some(Timestamp::from_unix(secs))
}

fn parse_rfc2822(s: &str) -> Option<Timestamp> {
    let mut tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.first().is_some_and(|t| t.ends_with(',')) {
        tokens.remove(0);
    }
    if tokens.len() < 4 {
        return None;
    }
    let d: i64 = tokens[0].parse().ok()?;
    let mon = tokens[1].to_ascii_lowercase();
    let mo = MONTHS.iter().position(|m| mon.starts_with(m))? as i64 + 1;
    let y: i64 = tokens[2].parse().ok()?;
    let y = if y < 100 {
        if y < 70 {
            2000 + y
        } else {
            1900 + y
        }
    } else {
        y
    };
    if !valid_ymd(y, mo, d) {
        return None;
    }
    let tb = tokens[3].as_bytes();
    let h = digits(tb, 0, 2)?;
    let mi = digits(tb, 3, 2)?;
    let se = digits(tb, 6, 2).unwrap_or(0);
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut secs = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + se;
    if let Some(z) = tokens.get(4) {
        secs -= zone_offset_secs(z);
    }
    Some(Timestamp::from_unix(secs))
}

/// Collect metadata for a walked PNG.
pub(crate) fn collect(src: &dyn Source, walk: &ChunkWalk) -> MediaMeta {
    let mut meta = MediaMeta::default();
    if let Some(ih) = walk.ihdr {
        meta.width = Some(ih.width);
        meta.height = Some(ih.height);
        meta.extra
            .insert("color_type".into(), ih.color_name().into());
        meta.extra
            .insert("bit_depth".into(), ih.bit_depth.to_string());
        if ih.is_interlaced() {
            meta.extra.insert("interlaced".into(), "adam7".into());
        }
    }
    if walk.cgbi {
        meta.extra.insert("variant".into(), "cgbi".into());
    }
    let mut exif = ExifInfo::default();
    let mut creation: Option<Timestamp> = None;
    let mut examined = 0usize;
    for c in &walk.chunks {
        if !c.complete {
            continue;
        }
        match &c.kind {
            b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME" | b"acTL" => {}
            _ => continue,
        }
        if c.len > MAX_META_CHUNK || examined >= MAX_META_CHUNKS {
            continue;
        }
        examined += 1;
        let Ok(data) = src.read_vec(walk.start + c.data_offset, c.len as usize) else {
            continue;
        };
        if data.len() != c.len as usize {
            continue;
        }
        match &c.kind {
            b"eXIf" => {
                let e = parse_exif(&data);
                if exif == ExifInfo::default() {
                    exif = e;
                }
            }
            b"tIME" => {
                if let (Some(y), Some(&mo), Some(&d), Some(&h), Some(&mi), Some(&s)) = (
                    diskrescue_core::util::bytes::u16be(&data, 0),
                    data.get(2),
                    data.get(3),
                    data.get(4),
                    data.get(5),
                    data.get(6),
                ) {
                    if valid_ymd(y as i64, mo as i64, d as i64) && h < 24 && mi < 60 && s <= 60 {
                        let t = Timestamp::from_unix(
                            days_from_civil(y as i64, mo as u32, d as u32) * 86_400
                                + h as i64 * 3600
                                + mi as i64 * 60
                                + s as i64,
                        );
                        meta.extra.insert("modified".into(), t.to_rfc3339());
                    }
                }
            }
            b"acTL" => {
                if let Some(n) = diskrescue_core::util::bytes::u32be(&data, 0) {
                    meta.extra.insert("apng_frames".into(), n.to_string());
                }
            }
            kind => {
                if let Some((key, text)) = parse_text(kind, &data) {
                    match key.as_str() {
                        "Creation Time"
                        | "date:create"
                        | "create-date"
                        | "exif:DateTimeOriginal"
                            if creation.is_none() =>
                        {
                            creation = parse_date(&text);
                        }
                        "Software" if meta.software.is_none() => {
                            let t = text.trim();
                            if !t.is_empty() {
                                meta.software = Some(t.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    meta.make = exif.make.clone();
    meta.model = exif.model.clone();
    meta.orientation = exif.orientation;
    if meta.software.is_none() {
        meta.software = exif.software.clone();
    }
    let exif_time = |s: &Option<String>| s.as_deref().and_then(Timestamp::from_exif_str);
    let candidates = [
        exif_time(&exif.date_time_original),
        creation,
        exif_time(&exif.date_time_digitized),
        exif_time(&exif.date_time),
    ];
    meta.created = candidates.into_iter().flatten().find(|t| t.is_plausible());
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiff(le: bool, entries: &[(u16, u16, u32, [u8; 4])], extra: &[u8]) -> Vec<u8> {
        let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut b = Vec::new();
        b.extend_from_slice(if le { b"II" } else { b"MM" });
        b.extend_from_slice(&w16(42));
        b.extend_from_slice(&w32(8));
        b.extend_from_slice(&w16(entries.len() as u16));
        for (tag, kind, count, val) in entries {
            b.extend_from_slice(&w16(*tag));
            b.extend_from_slice(&w16(*kind));
            b.extend_from_slice(&w32(*count));
            b.extend_from_slice(val);
        }
        b.extend_from_slice(&w32(0));
        b.extend_from_slice(extra);
        b
    }

    #[test]
    fn exif_both_endians_and_subifd() {
        for le in [true, false] {
            let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
            // Layout: header(8) + count(2) + 4 entries(48) + next(4) = 62
            // then Make string, then the Exif sub-IFD.
            let make = b"DiskRescue\0";
            let make_off = 62u32;
            let sub_off = make_off + make.len() as u32;
            let mut extra = Vec::new();
            extra.extend_from_slice(make);
            // sub-IFD with DateTimeOriginal at some offset after it.
            let date = b"2024:05:06 07:08:09\0";
            let date_off = sub_off + 2 + 12 + 4;
            let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
            extra.extend_from_slice(&w16(1));
            extra.extend_from_slice(&w16(0x9003));
            extra.extend_from_slice(&w16(2));
            extra.extend_from_slice(&w32(20));
            extra.extend_from_slice(&w32(date_off));
            extra.extend_from_slice(&w32(0));
            extra.extend_from_slice(date);
            let orient = if le { [6, 0, 0, 0] } else { [0, 6, 0, 0] };
            let t = tiff(
                le,
                &[
                    (0x010F, 2, 11, w32(make_off)),
                    (0x0112, 3, 1, orient),
                    (0x0110, 2, 3, *b"X1\0\0"),
                    (0x8769, 4, 1, w32(sub_off)),
                ],
                &extra,
            );
            let e = parse_exif(&t);
            assert_eq!(e.make.as_deref(), Some("DiskRescue"), "le={le}");
            assert_eq!(e.model.as_deref(), Some("X1"));
            assert_eq!(e.orientation, Some(6));
            assert_eq!(e.date_time_original.as_deref(), Some("2024:05:06 07:08:09"));
            // With the Exif prefix too.
            let mut p = b"Exif\0\0".to_vec();
            p.extend_from_slice(&t);
            assert_eq!(parse_exif(&p), e);
        }
    }

    #[test]
    fn exif_garbage_is_harmless() {
        assert_eq!(parse_exif(b""), ExifInfo::default());
        assert_eq!(parse_exif(b"II*\0"), ExifInfo::default());
        assert_eq!(parse_exif(b"MM\0*\xFF\xFF\xFF\xFF"), ExifInfo::default());
        let mut b = tiff(true, &[(0x010F, 2, 4096, [0xFF, 0xFF, 0xFF, 0x7F])], &[]);
        assert_eq!(parse_exif(&b).make, None);
        b.truncate(20);
        let _ = parse_exif(&b);
        // Self-referential sub-IFD pointer must not loop.
        let t = tiff(true, &[(0x8769, 4, 1, 8u32.to_le_bytes())], &[]);
        let _ = parse_exif(&t);
    }

    #[test]
    fn dates() {
        let want = Timestamp::from_unix(1_699_344_751); // 2023-11-07T08:12:31Z
        assert_eq!(parse_date("2023-11-07T08:12:31Z"), Some(want));
        assert_eq!(parse_date("2023-11-07 08:12:31"), Some(want));
        assert_eq!(parse_date("2023-11-07T09:12:31+01:00"), Some(want));
        assert_eq!(parse_date("2023-11-07T08:12:31.250Z"), Some(want));
        assert_eq!(parse_date("2023:11:07 08:12:31"), Some(want));
        assert_eq!(parse_date("Tue, 07 Nov 2023 08:12:31 GMT"), Some(want));
        assert_eq!(parse_date("07 Nov 2023 09:12:31 +0100"), Some(want));
        assert_eq!(
            parse_date("2023-11-07").unwrap().to_rfc3339(),
            "2023-11-07T00:00:00Z"
        );
        assert_eq!(parse_date("yesterday afternoon"), None);
        assert_eq!(parse_date("2023-13-45T99:99:99"), None);
        assert_eq!(parse_date(""), None);
    }

    #[test]
    fn text_chunks() {
        assert_eq!(
            parse_text(b"tEXt", b"Software\0Pillow"),
            Some(("Software".into(), "Pillow".into()))
        );
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, b"hello zip").unwrap();
        let z = z.finish().unwrap();
        let mut d = b"Comment\0\0".to_vec();
        d.extend_from_slice(&z);
        assert_eq!(
            parse_text(b"zTXt", &d),
            Some(("Comment".into(), "hello zip".into()))
        );
        let d = b"Creation Time\0\0\0en\0Zeit\0caf\xC3\xA9".to_vec();
        assert_eq!(
            parse_text(b"iTXt", &d),
            Some(("Creation Time".into(), "caf\u{e9}".into()))
        );
        assert_eq!(parse_text(b"tEXt", b"no-nul"), None);
        assert_eq!(parse_text(b"zTXt", b"k\0\0garbage"), None);
        assert_eq!(parse_text(b"iTXt", b"k\0"), None);
    }
}
