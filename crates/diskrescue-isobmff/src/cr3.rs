//! Canon CR3 raw images (ISO BMFF container, major brand `crx `) as a
//! [`FileFormat`].
//!
//! Layout (lclevy/canon_cr3): `ftyp`, `moov` (with the Canon metadata uuid
//! `85c0b687-820f-11e0-8111-f4ce462b6a48` holding `CNCV`, `CTBO`, the TIFF
//! blocks `CMT1` (IFD0) .. `CMT4` (GPS) and the `THMB` 160x120 JPEG, then
//! `mvhd` and 3-5 `trak`s with `CRAW` sample entries), an XMP uuid box, the
//! preview uuid `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16` with a `PRVW` box
//! (1620x1080 JPEG), and `mdat` (full-size JPEG first, then the raw data).
//!
//! The extent is the end of the last top-level box, cross-checked against
//! the `CTBO` table (max offset + size). Metadata comes from `CMT1`
//! (Make/Model/Software/Orientation/size) and `CMT2` (DateTimeOriginal).
//! `THMB`, `PRVW` and the full-size JPEG of track 1 are reported as nested
//! JPEGs so the carver does not emit them as separate photos.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::util::bytes::{u16be, u32be};
use diskrescue_core::{Range, Result, SharedSource, Source, SourceExt};

use crate::boxes::{self, parse_moov, sample_table, Moov};
use crate::mp4::{classify_brands, parse_ftyp, BrandClass};
use crate::tiff;

/// The preview uuid of CR3 files.
pub const PRVW_UUID: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

/// Canon CR3.
#[derive(Debug, Default, Clone, Copy)]
pub struct Cr3Format;

const SIGS: &[Signature] = &[Signature::at(b"ftyp", 4).with_also(b"crx ", 8)];

struct Cr3Walk {
    len: u64,
    status: ExtentStatus,
    err_offset: Option<u64>,
    moov: Option<Moov>,
    prvw: Option<(u64, u64, u16, u16)>,
    notes: Vec<String>,
    has_mdat: bool,
}

fn find_prvw(src: &dyn Source, payload_off: u64, payload_len: u64) -> Option<(u64, u64, u16, u16)> {
    let head = src
        .read_vec(payload_off, payload_len.min(128) as usize)
        .ok()?;
    for p in 0..head.len().saturating_sub(24) {
        if &head[p + 4..p + 8] != b"PRVW" {
            continue;
        }
        let box_size = u32be(&head, p)? as u64;
        let w = u16be(&head, p + 14)?;
        let h = u16be(&head, p + 16)?;
        let jpeg_len = u32be(&head, p + 20)? as u64;
        let jpeg_off = payload_off + p as u64 + 24;
        if jpeg_len == 0 || p as u64 + box_size > payload_len || jpeg_len + 24 > box_size {
            return None;
        }
        return Some((jpeg_off, jpeg_len, w, h));
    }
    None
}

fn walk_cr3(src: &dyn Source, start: u64, limit: u64) -> Result<Option<Cr3Walk>> {
    let size = src.size();
    if start >= size {
        return Ok(None);
    }
    let region_end = start.saturating_add(limit).min(size);
    let source_ends = region_end == size;
    let head = src.read_vec(start, 64.min((region_end - start) as usize))?;
    let Some((_, major, compat)) = parse_ftyp(&head) else {
        return Ok(None);
    };
    if classify_brands(&major, &compat) != BrandClass::Cr3 {
        return Ok(None);
    }
    let (tops, stop) =
        boxes::walk_top_level(src, start, region_end, &|k| boxes::is_top_level_type(k))?;
    let mut w = Cr3Walk {
        len: 0,
        status: ExtentStatus::Complete,
        err_offset: None,
        moov: None,
        prvw: None,
        notes: Vec::new(),
        has_mdat: tops.iter().any(|t| t.header.is(b"mdat")),
    };
    for t in &tops {
        if t.header.is(b"moov") && !t.truncated {
            match parse_moov(src, &t.header) {
                Ok(m) => w.moov = Some(m),
                Err(e) => w.notes.push(format!("moov does not parse: {e}")),
            }
        }
        if t.header.uuid == Some(PRVW_UUID) && !t.truncated {
            w.prvw = find_prvw(
                src,
                t.header.payload_offset(),
                t.header.payload_len(region_end),
            );
        }
    }
    if w.moov.is_none() && tops.len() <= 1 {
        return Ok(None);
    }
    let mut end = tops.last().map(|t| t.end.min(region_end)).unwrap_or(start);
    if stop == boxes::TopStop::Truncated {
        end = region_end;
        w.status = if source_ends {
            ExtentStatus::Truncated
        } else {
            ExtentStatus::Capped
        };
        w.err_offset = Some(end - start);
        w.notes.push(format!(
            "'{}' box runs past the {}",
            tops.last().map(|t| t.header.kind_str()).unwrap_or_default(),
            if source_ends {
                "end of the data"
            } else {
                "size cap"
            }
        ));
    }
    // Cross-check with CTBO.
    if let Some(canon) = w.moov.as_ref().and_then(|m| m.canon.as_ref()) {
        let ctbo_end = canon
            .ctbo
            .iter()
            .filter(|r| r.2 > 0)
            .map(|r| r.1.saturating_add(r.2))
            .max();
        if let Some(e) = ctbo_end {
            let e_abs = start.saturating_add(e);
            if e_abs > end {
                w.notes.push(format!(
                    "CTBO says the file is {e} bytes, the box walk ends at {}",
                    end - start
                ));
                if w.status == ExtentStatus::Complete {
                    w.status = ExtentStatus::Truncated;
                    w.err_offset = Some(end - start);
                }
            } else if e_abs < end {
                w.notes.push(format!(
                    "CTBO ends at {e}, before the last box ({})",
                    end - start
                ));
            }
        }
    }
    w.len = end - start;
    if w.len == 0 {
        return Ok(None);
    }
    Ok(Some(w))
}

fn cr3_meta(src: &dyn Source, start: u64, w: &Cr3Walk) -> (MediaMeta, Vec<Nested>) {
    let mut meta = MediaMeta::default();
    let mut nested = Vec::new();
    meta.extra.insert("extension".into(), "cr3".into());
    let rel = |off: u64, len: u64| -> Option<Range> {
        (off >= start && (off - start).saturating_add(len) <= w.len && len > 0)
            .then(|| Range::with_len(off - start, len))
    };
    if let Some(m) = &w.moov {
        if let Some(c) = &m.canon {
            if let Some(v) = &c.cncv {
                meta.extra.insert("canon_cncv".into(), v.clone());
            }
            let read_tiff = |r: Option<(u64, u64)>| -> Option<tiff::TiffInfo> {
                let (o, l) = r?;
                if l > 1 << 20 {
                    return None;
                }
                let b = src.read_vec(o, l as usize).ok()?;
                tiff::parse_tiff(&b)
            };
            if let Some(t) = read_tiff(c.cmt[0]) {
                meta.make = t.make.clone();
                meta.model = t.model.clone();
                meta.software = t.software.clone();
                meta.orientation = t.orientation;
                meta.width = t.width;
                meta.height = t.height;
                meta.created = t.created();
            }
            if let Some(t) = read_tiff(c.cmt[1]) {
                if let Some(ts) = t.created() {
                    meta.created = Some(ts);
                }
                if meta.width.is_none() {
                    meta.width = t.width;
                    meta.height = t.height;
                }
            }
            if let Some((o, l, _, _)) = c.thmb {
                if let Some(r) = rel(o, l) {
                    nested.push(Nested {
                        range: r,
                        format: Some("jpeg".into()),
                        role: NestedRole::Thumbnail,
                    });
                }
            }
        }
        if meta.width.is_none() {
            if let Some((wd, ht)) = m
                .tracks
                .iter()
                .filter_map(|t| t.entry().map(|e| (e.width as u32, e.height as u32)))
                .max_by_key(|d| d.0 as u64 * d.1 as u64)
                .filter(|d| d.0 > 0)
            {
                meta.width = Some(wd);
                meta.height = Some(ht);
            }
        }
        if meta.created.is_none() {
            meta.created = m.mvhd.created();
        }
        // Full-size JPEG: first sample of the first track.
        if let Some(t) = m.tracks.first() {
            if let Some(s) = sample_table(t).ok().and_then(|v| v.first().copied()) {
                let abs = start.saturating_add(s.offset);
                let magic = src.read_vec(abs, 3).unwrap_or_default();
                if magic == [0xFF, 0xD8, 0xFF] {
                    if let Some(r) = rel(abs, s.size as u64) {
                        nested.push(Nested {
                            range: r,
                            format: Some("jpeg".into()),
                            role: NestedRole::Preview,
                        });
                    }
                }
            }
        }
    }
    if let Some((o, l, _, _)) = w.prvw {
        if let Some(r) = rel(o, l) {
            nested.push(Nested {
                range: r,
                format: Some("jpeg".into()),
                role: NestedRole::Preview,
            });
        }
    }
    (meta, nested)
}

impl FileFormat for Cr3Format {
    fn id(&self) -> &'static str {
        "cr3"
    }

    fn name(&self) -> &'static str {
        "Canon CR3 raw image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["cr3", "crm"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::RawPhoto
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn min_size(&self) -> u64 {
        1024
    }

    fn max_size(&self) -> u64 {
        512 << 20
    }

    fn priority(&self) -> u8 {
        75
    }

    fn probe(&self, head: &[u8]) -> bool {
        parse_ftyp(head).is_some_and(|(_, m, c)| classify_brands(&m, &c) == BrandClass::Cr3)
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "tiff")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let s: &dyn Source = &**src;
        let Some(w) = walk_cr3(s, start, limit)? else {
            return Ok(None);
        };
        let (meta, nested) = cr3_meta(s, start, &w);
        let conf = match w.status {
            ExtentStatus::Complete if w.moov.is_some() && w.has_mdat && w.notes.is_empty() => {
                Confidence::HIGH
            }
            ExtentStatus::Complete => Confidence::MEDIUM,
            _ => Confidence::LOW,
        };
        Ok(Some(Measured {
            len: w.len,
            status: w.status,
            confidence: conf,
            err_offset: w.err_offset,
            meta,
            nested,
            notes: w.notes,
        }))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let s: &dyn Source = &**src;
        let size = s.size();
        let Some(w) = walk_cr3(s, 0, size)? else {
            return Ok(Validation::invalid("not a CR3 file"));
        };
        let (meta, nested) = cr3_meta(s, 0, &w);
        let mut v = Validation::valid(meta);
        v.problems = w.notes.clone();
        if w.status != ExtentStatus::Complete {
            v.problems.insert(0, "file truncated".into());
        }
        if w.moov.is_none() {
            v.problems.push("moov missing".into());
        }
        if !w.has_mdat {
            v.problems.push("mdat missing".into());
        }
        if w.len < size {
            v.problems
                .push(format!("{} bytes of trailing data", size - w.len));
        }
        if !nested.iter().any(|n| n.role == NestedRole::Preview) {
            v.problems.push("no embedded preview JPEG found".into());
        }
        if !v.problems.is_empty() {
            v.status = ValidationStatus::Damaged;
        }
        Ok(v)
    }
}
