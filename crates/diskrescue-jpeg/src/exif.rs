//! Exif (TIFF inside APP1) and MPF (TIFF inside APP2) parsing.
//!
//! Own minimal reader, no external crate: the payload is at most 64 KiB, so
//! it is parsed in memory with every offset bounded to the segment (Exif
//! thumbnails whose offsets point outside the segment are ignored, as some
//! phones write overflowing values). The IFD chain is capped at 8 IFDs, each
//! IFD at 512 entries, and a visited set prevents pointer cycles.
//!
//! Extracted: IFD0 Make/Model/Software/Orientation/DateTime and image size,
//! the Exif IFD (0x8769) dates and pixel dimensions, the GPS IFD (0x8825)
//! position, and the IFD1 thumbnail (0x0201/0x0202) as a file-relative
//! range: the thumbnail is a complete JPEG inside the APP1 segment and is
//! the reason a blind search for `FF D9` finds the wrong end of the file.

use std::collections::{BTreeMap, HashSet};

use diskrescue_core::media::MediaMeta;
use diskrescue_core::time::Timestamp;
use diskrescue_core::Range;

use crate::tiff::{Entry, Tiff};

/// Most IFDs followed in one TIFF structure.
const MAX_IFDS: usize = 8;

/// What the Exif block of a JPEG says.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExifInfo {
    /// ExifIFD 0x9003 DateTimeOriginal (`YYYY:MM:DD HH:MM:SS`).
    pub date_time_original: Option<String>,
    /// ExifIFD 0x9004 DateTimeDigitized.
    pub date_time_digitized: Option<String>,
    /// IFD0 0x0132 DateTime (last modification).
    pub date_time: Option<String>,
    /// IFD0 0x010F camera make.
    pub make: Option<String>,
    /// IFD0 0x0110 camera model.
    pub model: Option<String>,
    /// IFD0 0x0131 software.
    pub software: Option<String>,
    /// IFD0 0x0112 orientation (1..=8).
    pub orientation: Option<u16>,
    /// Pixel width (ExifIFD 0xA002, else IFD0 0x0100).
    pub width: Option<u32>,
    /// Pixel height (ExifIFD 0xA003, else IFD0 0x0101).
    pub height: Option<u32>,
    /// IFD1 thumbnail JPEG, as a range relative to the start of the file
    /// (only when it lies inside the APP1 segment and starts with SOI).
    pub thumbnail: Option<Range>,
    /// GPS latitude/longitude in decimal degrees (south/west negative).
    pub gps: Option<(f64, f64)>,
    /// Other useful tags (`offset_time_original`, `sub_sec_time_original`,
    /// `image_unique_id`, `lens_model`, `body_serial`), keyed by name.
    pub extra: BTreeMap<String, String>,
}

impl ExifInfo {
    /// Best capture timestamp: DateTimeOriginal, then DateTimeDigitized,
    /// then DateTime; implausible values (unset camera clocks) are skipped.
    pub fn created(&self) -> Option<Timestamp> {
        [
            &self.date_time_original,
            &self.date_time_digitized,
            &self.date_time,
        ]
        .into_iter()
        .flatten()
        .filter_map(|s| Timestamp::from_exif_str(s))
        .find(|t| t.is_plausible())
    }

    /// Copy what the Exif block knows into `meta` (existing values win).
    pub fn fill_meta(&self, meta: &mut MediaMeta) {
        if meta.make.is_none() {
            meta.make = self.make.clone();
        }
        if meta.model.is_none() {
            meta.model = self.model.clone();
        }
        if meta.software.is_none() {
            meta.software = self.software.clone();
        }
        if meta.orientation.is_none() {
            meta.orientation = self
                .orientation
                .filter(|o| (1..=8).contains(o))
                .map(|o| o as u8);
        }
        if meta.created.is_none() {
            meta.created = self.created();
        }
        if let Some((lat, lon)) = self.gps {
            meta.extra
                .entry("gps".into())
                .or_insert_with(|| format!("{lat:.6},{lon:.6}"));
        }
        for (k, v) in &self.extra {
            meta.extra
                .entry(format!("exif.{k}"))
                .or_insert_with(|| v.clone());
        }
    }
}

/// Parse an APP1 Exif payload.
///
/// `data` is the segment payload (after the two length bytes) and must
/// start with `"Exif\0"` plus one pad byte; `app1_file_offset` is the
/// file-relative offset of `data[0]`, used to express the thumbnail range
/// relative to the file. Returns `None` when the payload is not Exif or its
/// TIFF header is unusable; partial IFD damage still yields what was read.
pub fn parse_app1(data: &[u8], app1_file_offset: u64) -> Option<ExifInfo> {
    if data.len() < 14 || &data[..5] != b"Exif\0" {
        return None;
    }
    let tiff_start = 6usize;
    let (t, first) = Tiff::new(&data[tiff_start..])?;
    let mut info = ExifInfo::default();
    let mut visited = HashSet::new();

    // IFD0 and the chain (IFD1 holds the thumbnail).
    let mut next = first;
    let mut index = 0usize;
    let mut exif_ifd = None;
    let mut gps_ifd = None;
    while next != 0 && index < MAX_IFDS && visited.insert(next) {
        let Some((entries, n)) = t.ifd(next) else {
            break;
        };
        if index == 0 {
            for e in &entries {
                match e.tag {
                    0x010F => info.make = t.ascii(e),
                    0x0110 => info.model = t.ascii(e),
                    0x0131 => info.software = t.ascii(e),
                    0x0132 => info.date_time = t.ascii(e),
                    0x0112 => info.orientation = t.uint(e).map(|v| v as u16),
                    0x0100 => info.width = info.width.or(t.uint(e)),
                    0x0101 => info.height = info.height.or(t.uint(e)),
                    0x8769 => exif_ifd = t.uint(e),
                    0x8825 => gps_ifd = t.uint(e),
                    _ => {}
                }
            }
        } else if index == 1 {
            info.thumbnail = thumbnail(&t, &entries, tiff_start, app1_file_offset);
        }
        next = n;
        index += 1;
    }

    if let Some(off) = exif_ifd.filter(|o| visited.insert(*o)) {
        if let Some((entries, _)) = t.ifd(off) {
            let mut w = None;
            let mut h = None;
            for e in &entries {
                match e.tag {
                    0x9003 => info.date_time_original = t.ascii(e),
                    0x9004 => info.date_time_digitized = t.ascii(e),
                    0xA002 => w = t.uint(e),
                    0xA003 => h = t.uint(e),
                    0x9011 => put(&mut info.extra, "offset_time_original", t.ascii(e)),
                    0x9291 => put(&mut info.extra, "sub_sec_time_original", t.ascii(e)),
                    0xA420 => put(&mut info.extra, "image_unique_id", t.ascii(e)),
                    0xA434 => put(&mut info.extra, "lens_model", t.ascii(e)),
                    0xA431 => put(&mut info.extra, "body_serial", t.ascii(e)),
                    _ => {}
                }
            }
            if let (Some(w), Some(h)) = (w, h) {
                if w > 0 && h > 0 {
                    info.width = Some(w);
                    info.height = Some(h);
                }
            }
        }
    }

    if let Some(off) = gps_ifd.filter(|o| visited.insert(*o)) {
        if let Some((entries, _)) = t.ifd(off) {
            info.gps = gps(&t, &entries);
        }
    }
    Some(info)
}

fn put(map: &mut BTreeMap<String, String>, key: &str, v: Option<String>) {
    if let Some(v) = v {
        map.insert(key.to_string(), v);
    }
}

fn thumbnail(t: &Tiff<'_>, entries: &[Entry], tiff_start: usize, app1: u64) -> Option<Range> {
    let off = entries
        .iter()
        .find(|e| e.tag == 0x0201)
        .and_then(|e| t.uint(e))? as usize;
    let len = entries
        .iter()
        .find(|e| e.tag == 0x0202)
        .and_then(|e| t.uint(e))? as usize;
    if len < 4 || off.checked_add(len)? > t.data.len() {
        return None;
    }
    if t.data.get(off..off + 2)? != [0xFF, 0xD8] {
        return None;
    }
    let start = app1.checked_add((tiff_start + off) as u64)?;
    start.checked_add(len as u64)?;
    Some(Range::with_len(start, len as u64))
}

fn gps(t: &Tiff<'_>, entries: &[Entry]) -> Option<(f64, f64)> {
    let find = |tag: u16| entries.iter().find(|e| e.tag == tag);
    let dms = |e: &Entry| -> Option<f64> {
        let v = t.rationals(e)?;
        let deg = *v.first()?;
        let min = v.get(1).copied().unwrap_or(0.0);
        let sec = v.get(2).copied().unwrap_or(0.0);
        Some(deg + min / 60.0 + sec / 3600.0)
    };
    let mut lat = dms(find(0x0002)?)?;
    let mut lon = dms(find(0x0004)?)?;
    if find(0x0001).and_then(|e| t.ascii(e)).as_deref() == Some("S") {
        lat = -lat;
    }
    if find(0x0003).and_then(|e| t.ascii(e)).as_deref() == Some("W") {
        lon = -lon;
    }
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return None;
    }
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some((lat, lon))
}

/// One entry of the MPF MP Index IFD (CIPA DC-007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpEntry {
    /// Individual image attribute (flags, format, type code).
    pub attribute: u32,
    /// Image size in bytes (SOI..EOI).
    pub size: u32,
    /// Offset relative to the MPF TIFF header; 0 for the first image.
    pub offset: u32,
    /// Dependent image 1 entry number.
    pub dependent1: u16,
    /// Dependent image 2 entry number.
    pub dependent2: u16,
}

impl MpEntry {
    /// The MP type code (low 24 bits of the attribute).
    pub fn type_code(&self) -> u32 {
        self.attribute & 0x00FF_FFFF
    }

    /// Short description of the type code.
    pub fn type_name(&self) -> &'static str {
        match self.type_code() {
            0x030000 => "baseline primary",
            0x010001..=0x010005 => "large thumbnail",
            0x020001 => "panorama frame",
            0x020002 => "disparity (3D) frame",
            0x020003 => "multi-angle frame",
            _ => "auxiliary image",
        }
    }
}

/// The MP Index of an APP2 `MPF\0` segment.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpfInfo {
    /// File-relative offset of the MPF TIFF header (all entry offsets are
    /// relative to it).
    pub tiff_offset: u64,
    /// NumberOfImages (0xB001), when present.
    pub number_of_images: Option<u32>,
    /// MP entries (0xB002).
    pub entries: Vec<MpEntry>,
}

impl MpfInfo {
    /// True when the file is a stereo/multi-angle MPO rather than a JPEG
    /// with auxiliary images.
    pub fn is_mpo(&self) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(e.type_code(), 0x020002 | 0x020003))
    }
}

/// Parse an APP2 MPF payload (`data` starts with `"MPF\0"`; `payload_file_offset`
/// is the file-relative offset of `data[0]`).
pub fn parse_mpf(data: &[u8], payload_file_offset: u64) -> Option<MpfInfo> {
    if data.len() < 12 || &data[..4] != b"MPF\0" {
        return None;
    }
    let (t, first) = Tiff::new(&data[4..])?;
    let (entries, _) = t.ifd(first)?;
    let mut info = MpfInfo {
        tiff_offset: payload_file_offset.saturating_add(4),
        ..Default::default()
    };
    for e in &entries {
        match e.tag {
            0xB001 => info.number_of_images = t.uint(e),
            0xB002 => {
                let Some(b) = t.value_bytes(e) else {
                    continue;
                };
                for chunk in b.chunks_exact(16).take(64) {
                    let (Some(attribute), Some(size), Some(offset), Some(d1), Some(d2)) = (
                        t.endian.u32(chunk, 0),
                        t.endian.u32(chunk, 4),
                        t.endian.u32(chunk, 8),
                        t.endian.u16(chunk, 12),
                        t.endian.u16(chunk, 14),
                    ) else {
                        continue;
                    };
                    info.entries.push(MpEntry {
                        attribute,
                        size,
                        offset,
                        dependent1: d1,
                        dependent2: d2,
                    });
                }
            }
            _ => {}
        }
    }
    (!info.entries.is_empty()).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `Exif\0\0` + little-endian TIFF with IFD0 (Make, Orientation,
    /// ExifIFD, GPS), ExifIFD (DateTimeOriginal), GPS IFD, IFD1 thumbnail.
    pub(crate) fn sample_exif(thumb: &[u8]) -> Vec<u8> {
        let mut t: Vec<u8> = b"II*\0\x08\0\0\0".to_vec();
        let dt = b"2024:05:06 07:08:09\0";
        // Layout offsets (relative to TIFF start).
        let ifd0 = 8usize;
        let ifd0_len = 2 + 4 * 12 + 4;
        let exif = ifd0 + ifd0_len;
        let exif_len = 2 + 12 + 4;
        let gps = exif + exif_len;
        let gps_len = 2 + 4 * 12 + 4;
        let ifd1 = gps + gps_len;
        let ifd1_len = 2 + 2 * 12 + 4;
        let dt_off = ifd1 + ifd1_len;
        let lat_off = dt_off + dt.len();
        let lon_off = lat_off + 24;
        let thumb_off = lon_off + 24;
        let entry = |t: &mut Vec<u8>, tag: u16, typ: u16, count: u32, val: u32| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&typ.to_le_bytes());
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&val.to_le_bytes());
        };
        t.extend_from_slice(&4u16.to_le_bytes());
        entry(&mut t, 0x010F, 2, 4, u32::from_le_bytes(*b"Acme"));
        entry(&mut t, 0x0112, 3, 1, 6);
        entry(&mut t, 0x8769, 4, 1, exif as u32);
        entry(&mut t, 0x8825, 4, 1, gps as u32);
        t.extend_from_slice(&(ifd1 as u32).to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        entry(&mut t, 0x9003, 2, dt.len() as u32, dt_off as u32);
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        entry(&mut t, 0x0001, 2, 2, u32::from_le_bytes(*b"S\0\0\0"));
        entry(&mut t, 0x0002, 5, 3, lat_off as u32);
        entry(&mut t, 0x0003, 2, 2, u32::from_le_bytes(*b"E\0\0\0"));
        entry(&mut t, 0x0004, 5, 3, lon_off as u32);
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut t, 0x0201, 4, 1, thumb_off as u32);
        entry(&mut t, 0x0202, 4, 1, thumb.len() as u32);
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(dt);
        for (n, d) in [(33u32, 1u32), (30, 1), (0, 1)] {
            t.extend_from_slice(&n.to_le_bytes());
            t.extend_from_slice(&d.to_le_bytes());
        }
        for (n, d) in [(151u32, 1u32), (12, 1), (36, 1)] {
            t.extend_from_slice(&n.to_le_bytes());
            t.extend_from_slice(&d.to_le_bytes());
        }
        assert_eq!(t.len(), thumb_off);
        t.extend_from_slice(thumb);
        let mut v = b"Exif\0\0".to_vec();
        v.extend_from_slice(&t);
        v
    }

    #[test]
    fn parses_hand_built_exif() {
        let thumb = [0xFF, 0xD8, 0xFF, 0xDB, 1, 2, 3, 0xFF, 0xD9];
        let app1 = sample_exif(&thumb);
        let info = parse_app1(&app1, 100).unwrap();
        assert_eq!(info.make.as_deref(), Some("Acme"));
        assert_eq!(info.orientation, Some(6));
        assert_eq!(
            info.date_time_original.as_deref(),
            Some("2024:05:06 07:08:09")
        );
        let created = info.created().unwrap();
        assert_eq!(created.to_rfc3339(), "2024-05-06T07:08:09Z");
        let th = info.thumbnail.unwrap();
        assert_eq!(th.len(), thumb.len() as u64);
        let rel = (th.start - 100) as usize;
        assert_eq!(&app1[rel..rel + thumb.len()], &thumb);
        let (lat, lon) = info.gps.unwrap();
        assert!((lat + 33.5).abs() < 1e-9);
        assert!((lon - (151.0 + 12.0 / 60.0 + 36.0 / 3600.0)).abs() < 1e-9);
        let mut meta = MediaMeta::default();
        info.fill_meta(&mut meta);
        assert_eq!(meta.orientation, Some(6));
        assert!(meta.extra.contains_key("gps"));
    }

    #[test]
    fn rejects_thumbnail_outside_segment_and_survives_garbage() {
        let thumb = [0xFF, 0xD8, 0xFF, 0xD9];
        let mut app1 = sample_exif(&thumb);
        // Cut the thumbnail off: offset + len now overflows the segment.
        app1.truncate(app1.len() - 2);
        let info = parse_app1(&app1, 0).unwrap();
        assert!(info.thumbnail.is_none());
        // Every truncation and random corruption must not panic.
        let full = sample_exif(&thumb);
        for cut in 0..full.len() {
            let _ = parse_app1(&full[..cut], 0);
        }
        let mut x = 0x1234_5678u32;
        for i in 0..2000 {
            let mut d = full.clone();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let p = (x as usize) % d.len();
                d[p] = (x >> 8) as u8 ^ i as u8;
            }
            let _ = parse_app1(&d, u64::MAX - 10);
        }
        assert!(parse_app1(b"Exif\0\0MM\0*\xff\xff\xff\xff", 0).is_some());
        assert!(parse_app1(b"NotExif", 0).is_none());
    }

    #[test]
    fn parses_mpf_index() {
        let mut t: Vec<u8> = b"MM\0*\0\0\0\x08".to_vec();
        t.extend_from_slice(&3u16.to_be_bytes());
        let be = |t: &mut Vec<u8>, tag: u16, typ: u16, count: u32, val: u32| {
            t.extend_from_slice(&tag.to_be_bytes());
            t.extend_from_slice(&typ.to_be_bytes());
            t.extend_from_slice(&count.to_be_bytes());
            t.extend_from_slice(&val.to_be_bytes());
        };
        be(&mut t, 0xB000, 7, 4, u32::from_be_bytes(*b"0100"));
        be(&mut t, 0xB001, 4, 1, 2);
        let data_off = 8 + 2 + 3 * 12 + 4;
        be(&mut t, 0xB002, 7, 32, data_off);
        t.extend_from_slice(&0u32.to_be_bytes());
        for (attr, size, off) in [(0x2003_0000u32, 1000u32, 0u32), (0x0001_0001, 500, 2000)] {
            t.extend_from_slice(&attr.to_be_bytes());
            t.extend_from_slice(&size.to_be_bytes());
            t.extend_from_slice(&off.to_be_bytes());
            t.extend_from_slice(&[0, 0, 0, 0]);
        }
        let mut payload = b"MPF\0".to_vec();
        payload.extend_from_slice(&t);
        let m = parse_mpf(&payload, 40).unwrap();
        assert_eq!(m.tiff_offset, 44);
        assert_eq!(m.number_of_images, Some(2));
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.entries[1].offset, 2000);
        assert_eq!(m.entries[1].type_name(), "large thumbnail");
        assert!(!m.is_mpo());
        for cut in 0..payload.len() {
            let _ = parse_mpf(&payload[..cut], 0);
        }
    }
}
