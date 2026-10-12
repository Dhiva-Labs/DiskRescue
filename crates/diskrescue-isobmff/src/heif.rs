//! HEIF / HEIC / AVIF still images (ISO/IEC 23008-12) as a [`FileFormat`].
//!
//! The file is the same box structure as a movie, but the image data is
//! addressed by the `meta` box: `iinf` lists the items (`hvc1`, `av01`,
//! `grid`, `Exif`, `jpeg` ...), `iloc` gives each item's byte extents,
//! `iprp/ipco` holds properties (`ispe` = image size) and `ipma` associates
//! them with items, `pitm` names the primary item.
//!
//! Measuring walks the top-level boxes; the extent is the end of the last
//! box. Every `iloc` extent stored in the file (construction method 0) must
//! lie inside it; an `mdat` of size 0 is resolved from the largest `iloc`
//! extent. Metadata: dimensions from the primary item's `ispe`, make /
//! model / capture time from the `Exif` item (its payload starts with a
//! 4-byte offset to the TIFF header). HEVC thumbnails are items of the same
//! file, not separate files; only `jpeg` items are reported as nested.

use std::collections::BTreeMap;

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::util::bytes::{u16be, u32be, u64be};
use diskrescue_core::{Range, Result, SharedSource, Source, SourceExt};

use crate::boxes::{self, full_box_header, BoxIter};
use crate::mp4::{classify_brands, parse_ftyp, BrandClass};
use crate::tiff;

/// One HEIF item.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    /// item_ID.
    pub id: u32,
    /// item_type (`hvc1`, `av01`, `grid`, `Exif`, `jpeg`, `mime` ...).
    pub kind: [u8; 4],
    /// iloc construction method (0 file, 1 idat, 2 item).
    pub construction_method: u8,
    /// Extents `(offset, length)`; for method 0 offsets are file-relative.
    pub extents: Vec<(u64, u64)>,
}

/// Parsed HEIF `meta` box.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeifMeta {
    /// Handler type (`pict`).
    pub handler: [u8; 4],
    /// Primary item.
    pub primary: Option<u32>,
    /// Items with their locations.
    pub items: Vec<Item>,
    /// `ispe` properties by 1-based property index.
    pub ispe: BTreeMap<u32, (u32, u32)>,
    /// Item -> associated property indices.
    pub assoc: BTreeMap<u32, Vec<u32>>,
    /// Problems.
    pub problems: Vec<String>,
}

impl HeifMeta {
    /// Dimensions of the primary item (or the largest `ispe`).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        if let Some(p) = self.primary {
            if let Some(props) = self.assoc.get(&p) {
                for i in props {
                    if let Some(d) = self.ispe.get(i) {
                        return Some(*d);
                    }
                }
            }
        }
        self.ispe
            .values()
            .max_by_key(|d| d.0 as u64 * d.1 as u64)
            .copied()
    }

    /// End (file-relative) of the furthest extent stored in the file.
    pub fn max_extent(&self) -> Option<u64> {
        self.items
            .iter()
            .filter(|i| i.construction_method == 0)
            .flat_map(|i| i.extents.iter().map(|&(o, l)| o.saturating_add(l)))
            .max()
    }
}

fn uint(p: &[u8], off: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        4 => u32be(p, off).map(u64::from),
        8 => u64be(p, off),
        _ => None,
    }
}

fn parse_iloc(p: &[u8], out: &mut BTreeMap<u32, Item>, problems: &mut Vec<String>) {
    let Some((version, _)) = full_box_header(p) else {
        return;
    };
    let (Some(&b0), Some(&b1)) = (p.get(4), p.get(5)) else {
        return;
    };
    let off_size = (b0 >> 4) as usize;
    let len_size = (b0 & 0xF) as usize;
    let base_size = (b1 >> 4) as usize;
    let idx_size = if version >= 1 { (b1 & 0xF) as usize } else { 0 };
    for s in [off_size, len_size, base_size, idx_size] {
        if ![0, 4, 8].contains(&s) {
            problems.push("iloc field size invalid".into());
            return;
        }
    }
    let (count, mut pos) = if version < 2 {
        (u16be(p, 6).map(u32::from), 8usize)
    } else {
        (u32be(p, 6), 10usize)
    };
    let count = count.unwrap_or(0).min(65_536);
    // Extents with all field sizes 0 take no bytes: without a bound a
    // tiny box could declare 65 536 items x 65 535 extents.
    let empty_extents = off_size + len_size + idx_size == 0;
    let mut total_extents = 0usize;
    for _ in 0..count {
        let (id, np) = if version < 2 {
            (u16be(p, pos).map(u32::from), pos + 2)
        } else {
            (u32be(p, pos), pos + 4)
        };
        let Some(id) = id else { break };
        pos = np;
        let mut method = 0u8;
        if version >= 1 {
            let Some(m) = u16be(p, pos) else { break };
            method = (m & 0xF) as u8;
            pos += 2;
        }
        pos += 2; // data_reference_index
        let Some(base) = uint(p, pos, base_size) else {
            break;
        };
        pos += base_size;
        let Some(ext_count) = u16be(p, pos) else {
            break;
        };
        pos += 2;
        let ext_count = if empty_extents {
            ext_count.min(1)
        } else {
            ext_count
        };
        total_extents += ext_count as usize;
        if total_extents > 1 << 20 {
            problems.push("iloc declares too many extents".into());
            return;
        }
        let mut extents = Vec::new();
        for _ in 0..ext_count {
            pos += idx_size;
            let (Some(o), Some(l)) = (uint(p, pos, off_size), uint(p, pos + off_size, len_size))
            else {
                problems.push("iloc truncated".into());
                return;
            };
            pos += off_size + len_size;
            extents.push((base.saturating_add(o), l));
        }
        let e = out.entry(id).or_insert_with(|| Item {
            id,
            ..Default::default()
        });
        e.construction_method = method;
        e.extents = extents;
    }
}

fn parse_iinf(p: &[u8], base: u64, out: &mut BTreeMap<u32, Item>) {
    let Some((version, _)) = full_box_header(p) else {
        return;
    };
    let skip = if version == 0 { 6 } else { 8 };
    let Some(body) = p.get(skip..) else { return };
    for infe in BoxIter::new(body, base + skip as u64).take(65_536) {
        if !infe.header.is(b"infe") {
            continue;
        }
        let q = infe.payload;
        let Some((v, _)) = full_box_header(q) else {
            continue;
        };
        let (id, kind) = match v {
            2 => (
                u16be(q, 4).map(u32::from),
                q.get(8..12).and_then(|s| s.try_into().ok()),
            ),
            3 => (u32be(q, 4), q.get(10..14).and_then(|s| s.try_into().ok())),
            _ => (u16be(q, 4).map(u32::from), Some([0u8; 4])),
        };
        if let (Some(id), Some(kind)) = (id, kind) {
            out.entry(id)
                .or_insert_with(|| Item {
                    id,
                    ..Default::default()
                })
                .kind = kind;
        }
    }
}

fn parse_iprp(p: &[u8], base: u64, m: &mut HeifMeta) {
    for b in BoxIter::new(p, base).take(64) {
        if b.header.is(b"ipco") {
            for (i, prop) in BoxIter::new(b.payload, b.header.payload_offset())
                .take(65_536)
                .enumerate()
            {
                if prop.header.is(b"ispe") {
                    if let (Some(w), Some(h)) = (u32be(prop.payload, 4), u32be(prop.payload, 8)) {
                        m.ispe.insert(i as u32 + 1, (w, h));
                    }
                }
            }
        } else if b.header.is(b"ipma") {
            let q = b.payload;
            let Some((v, flags)) = full_box_header(q) else {
                continue;
            };
            let count = u32be(q, 4).unwrap_or(0).min(65_536);
            let mut pos = 8usize;
            for _ in 0..count {
                let id = if v < 1 {
                    u16be(q, pos).map(u32::from)
                } else {
                    u32be(q, pos)
                };
                let Some(id) = id else { break };
                pos += if v < 1 { 2 } else { 4 };
                let Some(&n) = q.get(pos) else { break };
                pos += 1;
                let mut props = Vec::new();
                for _ in 0..n {
                    let idx = if flags & 1 != 0 {
                        let Some(x) = u16be(q, pos) else { break };
                        pos += 2;
                        (x & 0x7FFF) as u32
                    } else {
                        let Some(&x) = q.get(pos) else { break };
                        pos += 1;
                        (x & 0x7F) as u32
                    };
                    props.push(idx);
                }
                m.assoc.insert(id, props);
            }
        }
    }
}

/// Parse a HEIF `meta` payload (FullBox; `base` = absolute offset of the
/// payload).
pub fn parse_heif_meta(p: &[u8], base: u64) -> HeifMeta {
    let mut m = HeifMeta::default();
    let Some(body) = p.get(4..) else {
        m.problems.push("meta too short".into());
        return m;
    };
    let base = base + 4;
    let mut items: BTreeMap<u32, Item> = BTreeMap::new();
    for b in BoxIter::new(body, base).take(4096) {
        match &b.header.kind {
            b"hdlr" => {
                if let Some(h) = b.payload.get(8..12) {
                    m.handler.copy_from_slice(h);
                }
            }
            b"pitm" => {
                let v = full_box_header(b.payload).map(|x| x.0).unwrap_or(0);
                m.primary = if v == 0 {
                    u16be(b.payload, 4).map(u32::from)
                } else {
                    u32be(b.payload, 4)
                };
            }
            b"iloc" => parse_iloc(b.payload, &mut items, &mut m.problems),
            b"iinf" => parse_iinf(b.payload, b.header.payload_offset(), &mut items),
            b"iprp" => parse_iprp(b.payload, b.header.payload_offset(), &mut m),
            _ => {}
        }
    }
    m.items = items.into_values().collect();
    m
}

/// HEIF/HEIC/AVIF stills.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeifFormat;

const SIGS: &[Signature] = &[Signature::at(b"ftyp", 4)];

/// Result of walking a HEIF file.
struct HeifWalk {
    len: u64,
    status: ExtentStatus,
    err_offset: Option<u64>,
    meta: Option<HeifMeta>,
    notes: Vec<String>,
    ext: &'static str,
    has_mdat: bool,
}

fn walk_heif(src: &dyn Source, start: u64, limit: u64) -> Result<Option<HeifWalk>> {
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
    let ext = match classify_brands(&major, &compat) {
        BrandClass::Heif(e) => e,
        _ => return Ok(None),
    };
    let (tops, stop) =
        boxes::walk_top_level(src, start, region_end, &|k| boxes::is_top_level_type(k))?;
    let mut w = HeifWalk {
        len: 0,
        status: ExtentStatus::Complete,
        err_offset: None,
        meta: None,
        notes: Vec::new(),
        ext,
        has_mdat: tops.iter().any(|t| t.header.is(b"mdat")),
    };
    if let Some(mb) = tops.iter().find(|t| t.header.is(b"meta")) {
        match boxes::read_payload(src, &mb.header, region_end, 64 << 20) {
            Ok(p) => w.meta = Some(parse_heif_meta(&p, mb.header.payload_offset())),
            Err(e) => w.notes.push(format!("meta unreadable: {e}")),
        }
    }
    if w.meta.is_none() && !tops.iter().any(|t| t.header.is(b"moov")) {
        return Ok(None);
    }
    let last = tops.last().copied();
    let mut end = last.map(|t| t.end.min(region_end)).unwrap_or(start);
    let max_ext = w
        .meta
        .as_ref()
        .and_then(|m| m.max_extent())
        .map(|e| start.saturating_add(e));
    let size_zero_last = last.is_some_and(|t| t.header.size == 0);
    match stop {
        _ if size_zero_last || stop == boxes::TopStop::Truncated => {
            let t = last.expect("last box exists");
            if t.header.size == 0
                || (t.header.is(b"mdat") && max_ext.is_some_and(|e| e <= region_end))
            {
                // Size 0 or oversized mdat: use the iloc evidence.
                match max_ext {
                    Some(e) if e <= region_end && e >= t.header.payload_offset() => {
                        end = e;
                        if t.header.size == 0 {
                            w.notes.push("mdat size 0: end taken from iloc".into());
                        } else {
                            w.notes
                                .push("mdat size runs past the data: end taken from iloc".into());
                        }
                    }
                    _ => {
                        end = region_end;
                        w.status = if source_ends {
                            ExtentStatus::Truncated
                        } else {
                            ExtentStatus::Capped
                        };
                        w.err_offset = Some(end - start);
                    }
                }
            } else {
                end = region_end;
                w.status = if source_ends {
                    ExtentStatus::Truncated
                } else {
                    ExtentStatus::Capped
                };
                w.err_offset = Some(end - start);
                w.notes.push(format!(
                    "'{}' box runs past the {}",
                    t.header.kind_str(),
                    if source_ends {
                        "end of the data"
                    } else {
                        "size cap"
                    }
                ));
            }
        }
        _ => {
            if let Some(e) = max_ext {
                if e > end {
                    w.notes.push("item extents run past the last box".into());
                    if e > region_end {
                        w.status = ExtentStatus::Truncated;
                        w.err_offset = Some(end - start);
                    } else {
                        end = e;
                    }
                }
            }
        }
    }
    if let Some(m) = &w.meta {
        for p in &m.problems {
            w.notes.push(p.clone());
        }
    }
    // The item locations may point at data that is not there (the file
    // was cut and other data follows): item data must look like its type.
    if w.status == ExtentStatus::Complete {
        if let Some(m) = &w.meta {
            if let Some(bad) = implausible_item(src, m, start, end) {
                if bad > start && bad < end {
                    w.notes.push(format!(
                        "item data from offset {} on does not match the item types: file cut",
                        bad - start
                    ));
                    end = bad;
                    w.status = ExtentStatus::Truncated;
                    w.err_offset = Some(bad - start);
                }
            }
        }
    }
    w.len = end - start;
    if w.len == 0 {
        return Ok(None);
    }
    Ok(Some(w))
}

/// Where the data of one item extent stops looking like its item type:
/// `None` when the type gives nothing to check, `Some(None)` when it looks
/// right, `Some(Some(rel))` when it fails at `rel` bytes into the extent
/// (0: already the first bytes). HEVC items are walked NAL unit by NAL
/// unit (lengths must tile the extent; no damaged bytes inside, see
/// [`crate::rebuild::nal_bytes_damaged`]); JPEG items must start with SOI
/// and end with EOI.
fn item_extent_check(kind: &[u8; 4], data: &[u8]) -> Option<Option<u64>> {
    match kind {
        b"hvc1" | b"hev1" | b"hvt1" => {
            // 4-byte NAL lengths (hvcC allows 2 and 1, but writers use 4;
            // shorter lengths match random bytes too easily).
            let mut best: Option<u64> = Some(0);
            for l in [4usize] {
                let mut p = 0usize;
                let fail = loop {
                    if p == data.len() {
                        break None;
                    }
                    let Some(len) = data.get(p..p + l) else {
                        break Some(p as u64);
                    };
                    let n = len.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
                    let Some(nal) = data.get(p + l..p + l + n).filter(|x| x.len() >= 2) else {
                        break Some(p as u64);
                    };
                    let ok = crate::hevc::parse_nal_header(nal)
                        .is_some_and(|h| h.nal_type <= 40 && !(22..=31).contains(&h.nal_type));
                    if !ok {
                        break Some(p as u64);
                    }
                    // Damage inside a NAL unit whose header is fine: the
                    // data stops being this item there.
                    if let Some(at) = crate::rebuild::nal_damage_offset(nal) {
                        break Some((p + l + at.max(1)) as u64);
                    }
                    p += l + n;
                };
                match fail {
                    None => return Some(None),
                    Some(f) => best = best.max(Some(f)),
                }
            }
            Some(best)
        }
        b"av01" => data.first().map(|&b| {
            let obu_type = (b >> 3) & 0xF;
            (b & 0x80 != 0 || b & 1 != 0 || !(1..=8).contains(&obu_type)).then_some(0)
        }),
        b"jpeg" => {
            if !data.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return Some(Some(0));
            }
            let tail = &data[data.len().saturating_sub(32)..];
            let eoi = tail.windows(2).any(|w| w == [0xFF, 0xD9]);
            Some((!eoi).then_some(2))
        }
        b"Exif" => {
            let skip = u32be(data, 0)? as usize;
            let tiff = data.get(4 + skip..4 + skip + 4)?;
            Some((tiff != b"II*\0" && tiff != b"MM\0*").then_some(0))
        }
        _ => None,
    }
}

/// The absolute offset where item data stops looking like its item type
/// (the file was cut there and other data follows). Extents are checked
/// in file order, whole when at most 16 MiB (64 MiB in all). A failure at
/// the very start of an extent counts only after an earlier extent looked
/// right (otherwise the check may simply not fit the writer). `None` when
/// everything checked looks fine or nothing could be checked.
fn implausible_item(src: &dyn Source, m: &HeifMeta, start: u64, end: u64) -> Option<u64> {
    let mut extents: Vec<(u64, u64, [u8; 4])> = m
        .items
        .iter()
        .filter(|i| i.construction_method == 0)
        .flat_map(|i| i.extents.iter().map(move |&(o, l)| (o, l, i.kind)))
        .filter_map(|(o, l, k)| {
            let a = start.checked_add(o)?;
            (a < end).then_some((a, l.min(end - a), k))
        })
        .collect();
    extents.sort_unstable();
    let mut seen_good = false;
    let mut budget: u64 = 64 << 20;
    for (abs, len, kind) in extents.into_iter().take(4096) {
        let want = if len == 0 || len > 16 << 20 || len > budget {
            64
        } else {
            len
        };
        budget = budget.saturating_sub(want);
        let data = src.read_vec(abs, want as usize).unwrap_or_default();
        let whole = want == len;
        let verdict = if whole {
            item_extent_check(&kind, &data)
        } else {
            // Only the start was read: a failure past it means nothing.
            item_extent_check(&kind, &data).map(|v| v.filter(|&r| r == 0))
        };
        match verdict {
            Some(None) => seen_good = true,
            Some(Some(0)) => {
                if seen_good {
                    return Some(abs);
                }
            }
            Some(Some(rel)) => return Some(abs + rel),
            None => {}
        }
    }
    None
}

fn heif_meta(src: &dyn Source, start: u64, w: &HeifWalk) -> (MediaMeta, Vec<Nested>) {
    let mut meta = MediaMeta::default();
    let mut nested = Vec::new();
    meta.extra.insert("extension".into(), w.ext.into());
    let Some(m) = &w.meta else {
        return (meta, nested);
    };
    if let Some((wd, ht)) = m.dimensions() {
        meta.width = Some(wd);
        meta.height = Some(ht);
    }
    let primary_kind = m
        .primary
        .and_then(|p| m.items.iter().find(|i| i.id == p))
        .map(|i| i.kind);
    if let Some(k) = primary_kind {
        meta.video_codec = Some(
            match &k {
                b"hvc1" => "hevc",
                b"av01" => "av1",
                b"jpeg" => "mjpeg",
                b"grid" => "grid",
                _ => "unknown",
            }
            .into(),
        );
    }
    for item in &m.items {
        if item.construction_method != 0 || item.extents.len() != 1 {
            continue;
        }
        let (off, len) = item.extents[0];
        if &item.kind == b"Exif" && (8..=1 << 20).contains(&len) && off.saturating_add(len) <= w.len
        {
            if let Ok(b) = src.read_vec(start.saturating_add(off), len as usize) {
                let skip = u32be(&b, 0).unwrap_or(0) as usize;
                if let Some(t) = b.get(4 + skip..).and_then(tiff::parse_tiff) {
                    meta.make = t.make.clone();
                    meta.model = t.model.clone();
                    meta.software = t.software.clone();
                    meta.orientation = t.orientation;
                    meta.created = t.created();
                }
            }
        }
        if &item.kind == b"jpeg" && off.saturating_add(len) <= w.len && len > 0 {
            let role = if Some(item.id) == m.primary {
                NestedRole::Preview
            } else {
                NestedRole::Thumbnail
            };
            nested.push(Nested {
                range: Range::with_len(off, len),
                format: Some("jpeg".into()),
                role,
            });
        }
    }
    (meta, nested)
}

impl FileFormat for HeifFormat {
    fn id(&self) -> &'static str {
        "heic"
    }

    fn name(&self) -> &'static str {
        "HEIF/HEIC/AVIF image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["heic", "heif", "avif", "heics"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn min_size(&self) -> u64 {
        256
    }

    fn max_size(&self) -> u64 {
        512 << 20
    }

    fn priority(&self) -> u8 {
        70
    }

    fn probe(&self, head: &[u8]) -> bool {
        parse_ftyp(head)
            .is_some_and(|(_, m, c)| matches!(classify_brands(&m, &c), BrandClass::Heif(_)))
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "tiff")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let s: &dyn Source = &**src;
        let Some(w) = walk_heif(s, start, limit)? else {
            return Ok(None);
        };
        let (meta, nested) = heif_meta(s, start, &w);
        let mut conf = if w.status == ExtentStatus::Complete {
            Confidence::MEDIUM
        } else {
            Confidence::LOW
        };
        if w.status == ExtentStatus::Complete
            && w.has_mdat
            && meta.width.is_some()
            && w.notes.is_empty()
        {
            conf = Confidence::HIGH;
        }
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
        let Some(w) = walk_heif(s, 0, size)? else {
            return Ok(Validation::invalid("not a HEIF/AVIF file"));
        };
        let (meta, _) = heif_meta(s, 0, &w);
        let mut v = Validation::valid(meta);
        v.problems = w.notes.clone();
        if w.status != ExtentStatus::Complete {
            v.problems.insert(0, "file truncated".into());
        }
        if w.len < size {
            v.problems
                .push(format!("{} bytes of trailing data", size - w.len));
        }
        match &w.meta {
            None => v.problems.push("meta box missing".into()),
            Some(m) => {
                if m.primary.is_none() {
                    v.problems.push("no primary item (pitm)".into());
                }
                if m.dimensions().is_none() {
                    v.problems.push("no ispe property".into());
                }
                let present = m
                    .items
                    .iter()
                    .filter(|i| i.construction_method == 0 && !i.extents.is_empty())
                    .filter(|i| i.extents.iter().all(|&(o, l)| o.saturating_add(l) <= w.len))
                    .count();
                let total = m
                    .items
                    .iter()
                    .filter(|i| i.construction_method == 0 && !i.extents.is_empty())
                    .count();
                if total > 0 {
                    v.intact_fraction = Some(present as f32 / total as f32);
                    if present < total {
                        v.problems.push(format!(
                            "{} of {total} items have extents outside the file",
                            total - present
                        ));
                    }
                }
            }
        }
        if !v.problems.is_empty() {
            v.status = ValidationStatus::Damaged;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iloc_v0_and_v1() {
        // v0: offset_size 4, length_size 4, base 0; one item, one extent.
        let mut p = vec![0, 0, 0, 0, 0x44, 0x00, 0, 1, 0, 7, 0, 0, 0, 1];
        p.extend_from_slice(&100u32.to_be_bytes());
        p.extend_from_slice(&50u32.to_be_bytes());
        let mut items = BTreeMap::new();
        let mut probs = Vec::new();
        parse_iloc(&p, &mut items, &mut probs);
        assert_eq!(items[&7].extents, vec![(100, 50)]);
        assert!(probs.is_empty());
        // Truncated input: no panic.
        parse_iloc(&p[..p.len() - 3], &mut items, &mut probs);
        assert!(!probs.is_empty());
    }
}
