//! Fujifilm RAF.
//!
//! # Layout
//!
//! All fields are big-endian. The file starts with the 16-byte magic
//! `FUJIFILMCCD-RAW `, a 4-byte ASCII format version, an 8-byte camera id and
//! a 32-byte NUL-padded model name. At offset 84 six words describe three
//! regions of the file: the embedded Exif JPEG (offset, length), the CFA
//! header, a table of records (offset, length), and the CFA raw data (offset,
//! length).
//!
//! The extent is the largest `offset + length` of the three pairs, which is
//! also PhotoRec's rule. Dual-shot bodies store a second set 28 bytes after
//! the first (at 120..136); it is honoured only when it is fully present and
//! lies after the first CFA, since the words there mean something else on
//! other bodies.
//!
//! # Checks
//!
//! Offsets must clear the 108-byte header and every end must be below 1 GiB.
//! The JPEG at the JPEG offset must start with `FF D8`; if it does not the
//! extent is still reported but confidence drops and the preview is not
//! listed as nested. Metadata comes from the CFA header record 0x0100 (raw
//! dimensions) and from the Exif of the embedded JPEG, read with the TIFF
//! walker.
//!
//! # Limits
//!
//! The raw data is not decoded or checksummed.

use diskrescue_core::format::{
    Confidence, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{ascii_trimmed, u16be, u32be};
use diskrescue_core::{Range, Result};

use crate::common::{sized, validate_with, Inspected};
use crate::jpegmini;
use crate::tiff::{exif_meta, walk_blob};
use crate::win::{read_error, Win};

const MAX_SIZE: u64 = 4 << 30;
/// Largest end offset accepted for any region.
const REGION_CAP: u64 = 1 << 30;
const HEADER_MIN: u64 = 108;
const SIGNATURES: [Signature; 1] = [Signature::at_start(b"FUJIFILMCCD-RAW ")];

/// Fujifilm RAF raw files.
#[derive(Debug, Clone, Copy, Default)]
pub struct RafFormat;

/// A `(offset, length)` region of the file.
type Region = (u64, u64);

/// Parsed fixed header.
#[derive(Debug, Clone)]
struct Header {
    version: String,
    camera_id: String,
    model: String,
    jpeg: Region,
    cfa_header: Region,
    cfa: Region,
    second: Option<[Region; 2]>,
    compression: Option<u32>,
    multi_shot: bool,
}

fn region(head: &[u8], at: usize) -> Option<Region> {
    let off = u32be(head, at)? as u64;
    let len = u32be(head, at + 4)? as u64;
    Some((off, len))
}

/// Validate one region: absent (length 0) or inside bounds.
fn region_ok(r: Region) -> bool {
    r.1 == 0 || (r.0 >= HEADER_MIN && r.0.saturating_add(r.1) <= REGION_CAP)
}

fn parse(head: &[u8]) -> Option<Header> {
    if head.len() < HEADER_MIN as usize || !SIGNATURES[0].matches(head) {
        return None;
    }
    let version = &head[16..20];
    if !version.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let jpeg = region(head, 84)?;
    let cfa_header = region(head, 92)?;
    let cfa = region(head, 100)?;
    if !region_ok(jpeg) || !region_ok(cfa_header) || !region_ok(cfa) {
        return None;
    }
    if jpeg.1 == 0 && cfa.1 == 0 {
        return None;
    }
    let second = if head.len() >= 136 && u32be(head, 120).unwrap_or(0) != 0 {
        let hdr = region(head, 120)?;
        let data = region(head, 128)?;
        (hdr.1 > 0 && data.1 > 0 && region_ok(hdr) && region_ok(data) && data.0 >= cfa.0)
            .then_some([hdr, data])
    } else {
        None
    };
    let compression = u32be(head, 108).filter(|c| *c <= 0xFF);
    Some(Header {
        version: String::from_utf8_lossy(version).into_owned(),
        camera_id: ascii_trimmed(&head[20..28]),
        model: ascii_trimmed(&head[28..60]),
        jpeg,
        cfa_header,
        cfa,
        second,
        compression,
        multi_shot: u32be(head, 64) == Some(1),
    })
}

/// Raw image dimensions from the CFA header table (record 0x0100).
fn raw_dims(win: &Win, hdr: Region) -> Option<(u32, u32)> {
    if hdr.1 < 4 || hdr.0 >= win.avail() {
        return None;
    }
    let bytes = win.read_upto(hdr.0, hdr.1.min(64 * 1024) as usize).ok()?;
    let count = u32be(&bytes, 0)?.min(256);
    let mut pos = 4usize;
    for _ in 0..count {
        let tag = u16be(&bytes, pos)?;
        let size = u16be(&bytes, pos + 2)? as usize;
        let data = bytes.get(pos + 4..pos + 4 + size)?;
        if tag == 0x0100 && size >= 4 {
            let h = u16be(data, 0)? as u32;
            let w = u16be(data, 2)? as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        pos += 4 + size;
    }
    None
}

fn inspect(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Inspected>> {
    let limit = limit.min(MAX_SIZE);
    let win = Win::new(src, start, limit, 4096);
    let head = match win.read_upto(0, 160) {
        Ok(h) => h,
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let Some(h) = parse(&head) else {
        return Ok(None);
    };
    let avail = win.avail();
    let mut extent = [h.jpeg, h.cfa_header, h.cfa]
        .iter()
        .filter(|r| r.1 > 0)
        .map(|r| r.0 + r.1)
        .max()
        .unwrap_or(HEADER_MIN);
    let mut notes: Vec<String> = Vec::new();
    if let Some([hdr, data]) = h.second {
        let end = (hdr.0 + hdr.1).max(data.0 + data.1);
        if end <= avail {
            extent = extent.max(end);
            notes.push("dual-shot file: second raw set included".into());
        } else {
            notes.push("second raw set ignored (not fully present)".into());
        }
    }
    extent = extent.max(HEADER_MIN);
    let mut conf = Confidence::HIGH;
    // The embedded JPEG must start with FF D8.
    let mut nested = Vec::new();
    let mut exif_dims: (Option<u32>, Option<u32>) = (None, None);
    let mut meta = MediaMeta {
        make: Some("FUJIFILM".into()),
        model: (!h.model.is_empty()).then(|| h.model.clone()),
        ..MediaMeta::default()
    };
    if h.jpeg.1 > 0 && h.jpeg.0 < avail {
        let jhead = win
            .read_upto(h.jpeg.0, (64 * 1024u64).min(h.jpeg.1) as usize)
            .unwrap_or_default();
        match jpegmini::inspect(&jhead) {
            Some(j) => {
                let end = (h.jpeg.0 + h.jpeg.1).min(extent);
                nested.push(Nested {
                    range: Range::new(h.jpeg.0, end),
                    format: Some("jpeg".into()),
                    role: NestedRole::Preview,
                });
                if let Some((toff, tlen)) = j.exif {
                    if let Some(w) = walk_blob(src, start + h.jpeg.0 + toff as u64, tlen as u64) {
                        let em = exif_meta(&w);
                        meta.created = em.created;
                        meta.orientation = em.orientation;
                        meta.software = em.software;
                        if em.make.is_some() {
                            meta.make = em.make;
                        }
                        if em.model.is_some() {
                            meta.model = em.model;
                        }
                        exif_dims = (em.width, em.height);
                    }
                }
                if j.width > 0 {
                    meta.extra
                        .insert("preview_size".into(), format!("{}x{}", j.width, j.height));
                }
            }
            None => {
                conf = conf.lowered(30);
                notes.push(format!(
                    "no JPEG start (FF D8) at the JPEG offset {}; preview not listed",
                    h.jpeg.0
                ));
            }
        }
    }
    if let Some((w, hh)) = raw_dims(&win, h.cfa_header) {
        meta.width = Some(w);
        meta.height = Some(hh);
    } else {
        meta.width = exif_dims.0;
        meta.height = exif_dims.1;
        notes.push("raw dimensions not found in the CFA header".into());
    }
    meta.extra.insert("raf_version".into(), h.version.clone());
    if !h.camera_id.is_empty() {
        meta.extra.insert("camera_id".into(), h.camera_id.clone());
    }
    if let Some(c) = h.compression {
        meta.extra.insert(
            "raf_compression".into(),
            match c {
                0 => "uncompressed",
                2 => "lossless",
                3 => "lossy",
                _ => "unknown",
            }
            .into(),
        );
    }
    if h.multi_shot {
        meta.extra.insert("multi_shot".into(), "true".into());
    }
    meta.extra.insert("extension".into(), "raf".into());
    let mut m = sized(&win, extent, conf, "the RAF regions");
    m.meta = meta;
    m.nested = nested
        .into_iter()
        .filter(|n| n.range.start < m.len)
        .map(|mut n| {
            n.range = Range::new(n.range.start, n.range.end.min(m.len));
            n
        })
        .collect();
    m.notes.extend(notes);
    crate::common::log_measured(&win, "raf", &m, false);
    Ok(Some(Inspected::new(m, Some(extent))))
}

impl FileFormat for RafFormat {
    fn id(&self) -> &'static str {
        "raf"
    }

    fn name(&self) -> &'static str {
        "Fujifilm RAF raw"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["raf"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::RawPhoto
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        108
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        60
    }

    fn probe(&self, head: &[u8]) -> bool {
        // Too short to read the offsets: let `measure` decide.
        (head.len() < HEADER_MIN as usize && SIGNATURES[0].matches(head)) || parse(head).is_some()
    }

    /// The embedded JPEG (with its Exif block) and the CFA section, which is
    /// a TIFF container (carving spec 2.2.6), are part of the file.
    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "tiff")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        Ok(inspect(src, start, limit)?.map(|i| i.measured))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        validate_with("RAF", src, inspect)
    }
}
