//! MP4 / MOV / M4V / 3GP / 3G2 / M4A as a [`FileFormat`]: carving
//! measurement, validation and repair.
//!
//! # Measuring (carving)
//!
//! Starting at `ftyp` (or at `moov`/`mdat`/`wide`/`free` for ftyp-less
//! QuickTime files), top-level boxes are walked until the window ends, an
//! unknown type, another `ftyp` (the next file) or an impossible header.
//! The `moov` is parsed when met; its sample tables give the real end of the
//! media data when the `mdat` size is 0, wrong, or runs past the window.
//! A walk that hits garbage before any `moov`/`mdat` is a false positive.
//! Fragmented files are measured to the end of the last complete
//! `moof`+`mdat` pair.
//!
//! Status: `Complete` when the last box ends consistently and the `moov`
//! parses (`HIGH` confidence when the first video sample also starts with a
//! valid NAL unit); `UnknownLength` for an `mdat` of size 0 without a `moov`
//! inside a window that does not reach the source end; `Truncated` /
//! `Capped` when data runs past the source end / the window.

use std::io::Write;

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, RepairContext,
    RepairOutcome, RepairResult, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SubSource;
use diskrescue_core::{Range, Result, SharedSource, Source, SourceExt};

use crate::analysis::{analyze, Analysis};
use crate::boxes::{self, parse_header, parse_moov, sample_table, BoxHeader, Moov};
use crate::repair::{self, Strategy};

/// What kind of file an `ftyp` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandClass {
    /// A movie/audio file; the extension to use.
    Movie(&'static str),
    /// A HEIF/AVIF still image; the extension to use.
    Heif(&'static str),
    /// Canon CR3 raw.
    Cr3,
    /// Not recognised.
    Unknown,
}

fn movie_ext(b: &[u8; 4]) -> Option<&'static str> {
    Some(match b {
        b"qt  " => "mov",
        b"M4V " | b"M4VH" | b"M4VP" => "m4v",
        b"M4A " | b"M4B " | b"M4P " => "m4a",
        _ if b.starts_with(b"3gp")
            || b.starts_with(b"3gg")
            || b.starts_with(b"3gr")
            || b.starts_with(b"3gs")
            || b.starts_with(b"3ge")
            || b.starts_with(b"3g2") =>
        {
            "3gp"
        }
        b"isom" | b"iso2" | b"iso3" | b"iso4" | b"iso5" | b"iso6" | b"iso7" | b"iso8" | b"iso9"
        | b"mp41" | b"mp42" | b"mp71" | b"avc1" | b"mmp4" | b"MSNV" | b"NDAS" | b"NDSC"
        | b"NDSH" | b"NDSM" | b"NDSP" | b"NDSS" | b"NDXC" | b"NDXH" | b"NDXM" | b"NDXP"
        | b"NDXS" | b"dash" | b"cmfc" | b"cmf2" | b"f4v " | b"f4p " | b"XAVC" | b"caqv"
        | b"CAEP" | b"MQT " | b"kddi" | b"dby1" | b"hvc1" | b"av01" | b"jvt " | b"ROSS"
        | b"isml" | b"piff" | b"sdv " | b"MPPI" | b"CDes" => "mp4",
        _ => return None,
    })
}

fn heif_ext(b: &[u8; 4]) -> Option<&'static str> {
    Some(match b {
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" => "heic",
        b"avif" | b"avis" => "avif",
        b"mif1" | b"msf1" | b"mif2" => "heif",
        _ => return None,
    })
}

/// Classify a file by its `ftyp` brands.
pub fn classify_brands(major: &[u8; 4], compatible: &[[u8; 4]]) -> BrandClass {
    if major == b"crx " {
        return BrandClass::Cr3;
    }
    if let Some(e) = heif_ext(major) {
        if e == "heif" {
            // Generic HEIF: refine from the compatible brands.
            for c in compatible {
                match heif_ext(c) {
                    Some("heic") => return BrandClass::Heif("heic"),
                    Some("avif") => return BrandClass::Heif("avif"),
                    _ => {}
                }
            }
        }
        return BrandClass::Heif(e);
    }
    if let Some(e) = movie_ext(major) {
        return BrandClass::Movie(e);
    }
    if compatible.iter().any(|c| c == b"crx ") {
        return BrandClass::Cr3;
    }
    for c in compatible {
        if let Some(e) = heif_ext(c) {
            if e != "heif" || !compatible.iter().any(|x| movie_ext(x).is_some()) {
                return BrandClass::Heif(if e == "heif" { "heic" } else { e });
            }
        }
    }
    for c in compatible {
        if let Some(e) = movie_ext(c) {
            return BrandClass::Movie(if e == "m4a" || e == "mov" { e } else { "mp4" });
        }
    }
    BrandClass::Unknown
}

/// Parse an `ftyp` at the start of `head`: `(box, major, compatible)`.
pub fn parse_ftyp(head: &[u8]) -> Option<(BoxHeader, [u8; 4], Vec<[u8; 4]>)> {
    let h = parse_header(head, 0)?;
    if !h.is(b"ftyp") || h.size < 16 || h.size > 1024 {
        return None;
    }
    let major: [u8; 4] = head.get(8..12)?.try_into().ok()?;
    if !boxes::is_printable_fourcc(&major) {
        return None;
    }
    let end = (h.size as usize).min(head.len());
    let compat = head
        .get(16..end)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    Some((h, major, compat))
}

/// The MP4/MOV/3GP format.
#[derive(Debug, Default, Clone, Copy)]
pub struct Mp4Format;

/// `ftyp` files, and QuickTime movies without `ftyp` (old cameras) whose
/// first box is `moov`, `mdat`, `wide`, `free` or `skip`; [`Mp4Format`]'s
/// `probe` rejects implausible hits of the latter, and their measurements
/// get a lower confidence.
const SIGS: &[Signature] = &[
    Signature::at(b"ftyp", 4),
    Signature::at(b"moov", 4),
    Signature::at(b"mdat", 4),
    Signature::at(b"wide", 4),
    Signature::at(b"free", 4),
    Signature::at(b"skip", 4),
];

/// Box types that can follow the first box of an `ftyp`-less movie.
const QT_TOP: &[&[u8; 4]] = &[
    b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot", b"PICT",
];

/// Does a box header that is not `ftyp` start a QuickTime movie? Checked
/// on the first bytes only: the size must be plausible and what follows
/// the box (or, for `mdat`, its first payload bytes) must fit.
pub fn quicktime_head_plausible(head: &[u8]) -> bool {
    let Some(h) = parse_header(head, 0) else {
        return false;
    };
    let hl = h.full_header_len() as usize;
    match &h.kind {
        b"moov" => {
            // The first child is normally mvhd (108+ bytes); allow the
            // other children QuickTime writers put first.
            let Some(c) = head.get(hl..).and_then(|b| parse_header(b, hl as u64)) else {
                return false;
            };
            h.size >= 16
                && c.size >= 8
                && c.size <= h.size.saturating_sub(hl as u64)
                && matches!(
                    &c.kind,
                    b"mvhd" | b"prfl" | b"udta" | b"iods" | b"meta" | b"trak" | b"cmov" | b"uuid"
                )
                && (!c.is(b"mvhd") || c.size >= 100)
        }
        b"wide" => {
            h.size == 8
                && head
                    .get(8..)
                    .and_then(|b| parse_header(b, 8))
                    .is_some_and(|m| m.is(b"mdat"))
        }
        b"free" | b"skip" => {
            let size = h.size as usize;
            size >= 8
                && size + 8 <= head.len()
                && head
                    .get(size..)
                    .and_then(|b| parse_header(b, size as u64))
                    .is_some_and(|n| QT_TOP.contains(&&n.kind))
        }
        b"mdat" => {
            if h.size != 0 && h.size < 24 {
                return false;
            }
            let Some(p) = head.get(hl..hl + 8) else {
                return false;
            };
            // Motion JPEG, or a 4-byte length-prefixed H.264/HEVC NAL unit.
            if p.starts_with(&[0xFF, 0xD8, 0xFF]) {
                return true;
            }
            let n = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            if !(2..=8 << 20).contains(&n) || p[4] & 0x80 != 0 {
                return false;
            }
            let avc = matches!(p[4] & 0x1F, 1 | 5 | 6 | 7 | 9);
            let hevc = crate::hevc::parse_nal_header(&p[4..]).is_some_and(|nh| {
                crate::hevc::is_slice_type(nh.nal_type) || (32..=40).contains(&nh.nal_type)
            });
            avc || hevc
        }
        _ => false,
    }
}

/// Movie metadata from a parsed `moov` (offsets of nested objects are made
/// relative to `file_start`).
pub fn movie_meta(moov: &Moov, file_start: u64, file_len: u64) -> (MediaMeta, Vec<Nested>) {
    let mut meta = MediaMeta {
        created: moov
            .mvhd
            .created()
            .or(moov.udta_meta.creation_time.filter(|t| t.is_plausible())),
        duration_ms: moov
            .mvhd
            .duration_ms()
            .or_else(|| moov.tracks.iter().filter_map(|t| t.duration_ms()).max()),
        make: moov.udta_meta.make.clone(),
        model: moov.udta_meta.model.clone(),
        software: moov.udta_meta.software.clone(),
        ..Default::default()
    };
    if let Some(v) = moov.video() {
        let e = v.entry();
        let (w, h) = (v.tkhd.width(), v.tkhd.height());
        let (ew, eh) = e
            .map(|e| (e.width as u32, e.height as u32))
            .unwrap_or((0, 0));
        if w > 0 && h > 0 {
            meta.width = Some(w);
            meta.height = Some(h);
        } else if ew > 0 && eh > 0 {
            meta.width = Some(ew);
            meta.height = Some(eh);
        }
        meta.video_codec = v.codec();
        let m = v.tkhd.matrix;
        let rot = match (m[0] as i32, m[1] as i32) {
            (0, 0x10000) => Some("90"),
            (-0x10000, 0) => Some("180"),
            (0, -0x10000) => Some("270"),
            _ => None,
        };
        if let Some(r) = rot {
            meta.extra.insert("rotation".into(), r.into());
        }
    }
    if let Some(a) = moov.audio() {
        meta.audio_codec = a.codec();
        if let Some(e) = a.entry() {
            if e.sample_rate > 0 {
                meta.extra
                    .insert("audio_sample_rate".into(), e.sample_rate.to_string());
            }
        }
    }
    if let Some(l) = &moov.udta_meta.location {
        meta.extra.insert("location".into(), l.clone());
    }
    if let Some(t) = &moov.udta_meta.title {
        meta.extra.insert("title".into(), t.clone());
    }
    meta.extra
        .insert("tracks".into(), moov.tracks.len().to_string());
    let mut nested = Vec::new();
    let mut add = |off: u64, len: u64, fmt: Option<&str>, role: NestedRole| {
        if off >= file_start && len > 0 && off - file_start + len <= file_len {
            nested.push(Nested {
                range: Range::with_len(off - file_start, len),
                format: fmt.map(str::to_string),
                role,
            });
        }
    };
    for c in &moov.udta_meta.cover_art {
        add(c.offset, c.len, c.format, NestedRole::CoverArt);
    }
    if let Some(canon) = &moov.canon {
        if let Some((o, l)) = canon.cnth {
            add(o, l, Some("jpeg"), NestedRole::Thumbnail);
        }
    }
    (meta, nested)
}

/// Does the first video sample start with a valid NAL unit?
fn first_video_sample_ok(src: &dyn Source, moov: &Moov, file_start: u64) -> Option<bool> {
    let v = moov.video()?;
    let e = v.entry()?;
    let l = e
        .avcc
        .as_ref()
        .map(|c| c.nal_length_size)
        .or(e.hvcc.as_ref().map(|c| c.nal_length_size))? as usize;
    let s = *sample_table(v).ok()?.first()?;
    let head = src.read_vec(file_start + s.offset, l + 2).ok()?;
    if head.len() < l + 2 {
        return Some(false);
    }
    let n = head[..l].iter().fold(0u64, |a, &b| (a << 8) | b as u64);
    Some(n > 0 && n + l as u64 <= s.size as u64 && head[l] & 0x80 == 0)
}

fn top_ok(k: &[u8; 4]) -> bool {
    boxes::is_top_level_type(k)
}

impl Mp4Format {
    fn measure_impl(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let s: &dyn Source = &**src;
        let src_size = s.size();
        if start >= src_size {
            return Ok(None);
        }
        let region_end = start.saturating_add(limit).min(src_size);
        let source_ends = region_end == src_size;
        let head = s.read_vec(start, 64.min((region_end - start) as usize))?;
        let mut ext = "mov";
        let mut confidence_cut = 0u8;
        match parse_ftyp(&head) {
            Some((_, major, compat)) => match classify_brands(&major, &compat) {
                BrandClass::Movie(e) => ext = e,
                _ => return Ok(None),
            },
            None => {
                // ftyp-less QuickTime: must start with a plausible movie box.
                let probe =
                    s.read_vec(start, (64usize << 10).min((region_end - start) as usize))?;
                if !quicktime_head_plausible(&probe) {
                    return Ok(None);
                }
                confidence_cut = 20;
            }
        }
        let mut pos = start;
        let mut moov: Option<Moov> = None;
        let mut moov_err: Option<String> = None;
        let mut seen_moov = false;
        let mut seen_mdat = false;
        let mut open_mdat: Option<BoxHeader> = None;
        let mut truncated_box: Option<BoxHeader> = None;
        let mut garbage_at: Option<u64> = None;
        let mut last_complete_fragment_end: Option<u64> = None;
        let mut pending_moof = false;
        let mut fragmented = false;
        let mut count = 0u32;
        let mut mdat_box: Option<BoxHeader> = None;
        while pos < region_end {
            count += 1;
            if count > 100_000 {
                garbage_at = Some(pos);
                break;
            }
            let Some(h) = boxes::read_header(s, pos, region_end)? else {
                garbage_at = Some(pos);
                break;
            };
            if !top_ok(&h.kind) || (h.is(b"ftyp") && pos != start) {
                garbage_at = Some(pos);
                break;
            }
            if h.is(b"ftyp") && h.size == 0 {
                return Ok(None);
            }
            if h.size == 0 {
                if h.is(b"mdat") {
                    seen_mdat = true;
                    open_mdat = Some(h);
                    mdat_box.get_or_insert(h);
                }
                pos = region_end;
                break;
            }
            let end = h.offset.saturating_add(h.size);
            if end > region_end {
                if h.is(b"mdat") {
                    seen_mdat = true;
                    mdat_box.get_or_insert(h);
                }
                if h.is(b"moov") {
                    seen_moov = true;
                }
                truncated_box = Some(h);
                break;
            }
            match &h.kind {
                b"moov" => {
                    seen_moov = true;
                    match parse_moov(s, &h) {
                        Ok(m) => moov = Some(m),
                        Err(e) => moov_err = Some(e.to_string()),
                    }
                }
                b"mdat" => {
                    seen_mdat = true;
                    mdat_box.get_or_insert(h);
                    if pending_moof {
                        last_complete_fragment_end = Some(end);
                        pending_moof = false;
                    }
                }
                b"moof" => {
                    fragmented = true;
                    pending_moof = true;
                }
                _ => {}
            }
            pos = end;
        }
        if !seen_moov && !seen_mdat && !fragmented {
            return Ok(None);
        }
        let walk_end = pos.min(region_end);
        // Real extent from the sample tables (offsets are file-relative).
        let mut notes = Vec::new();
        let max_extent = moov
            .as_ref()
            .and_then(|m| tables_extent(m, &mut notes))
            .map(|e| start.saturating_add(e));
        let mut status;
        let mut len;
        let mut err_offset = None;
        let mut conf = Confidence::MEDIUM;
        if let Some(m) = open_mdat {
            // mdat with size 0.
            notes.push("mdat size is 0".to_string());
            if let Some(e) = max_extent {
                if e <= region_end {
                    len = e - start;
                    status = ExtentStatus::Complete;
                } else {
                    len = region_end - start;
                    status = if source_ends {
                        ExtentStatus::Truncated
                    } else {
                        ExtentStatus::Capped
                    };
                    err_offset = Some(len);
                }
            } else {
                // A trailing moov swallowed by the size-0 mdat?
                let window: SharedSource =
                    std::sync::Arc::new(SubSource::new(src.clone(), start, region_end - start));
                let rel_payload = m.payload_offset() - start;
                let a = analyze_tail_moov(&window, rel_payload)?;
                // The trailing moov must describe data that lies before it.
                let a = a.filter(|(moov_end, m2)| {
                    let mut ignored = Vec::new();
                    tables_extent(m2, &mut ignored).is_some_and(|e| e <= *moov_end)
                        && ignored.is_empty()
                });
                match a {
                    Some((moov_end, m2)) => {
                        len = moov_end;
                        status = ExtentStatus::Complete;
                        moov = Some(m2);
                        notes.push("moov found after the size-0 mdat".into());
                    }
                    None => {
                        len = region_end - start;
                        if source_ends {
                            status = ExtentStatus::Truncated;
                            err_offset = Some(len);
                            notes.push("no moov: recording was interrupted".into());
                        } else {
                            status = ExtentStatus::UnknownLength;
                        }
                        conf = Confidence::LOW;
                    }
                }
            }
        } else if let Some(t) = truncated_box {
            match (max_extent, t.is(b"mdat")) {
                (Some(e), true) if e <= region_end && e > t.payload_offset() => {
                    len = e - start;
                    status = ExtentStatus::Complete;
                    notes.push(
                        "mdat size runs past the data; end taken from the sample tables".into(),
                    );
                }
                _ => {
                    len = region_end - start;
                    status = if source_ends {
                        ExtentStatus::Truncated
                    } else {
                        ExtentStatus::Capped
                    };
                    err_offset = Some(len);
                    notes.push(format!(
                        "'{}' box at offset {} runs past the {}",
                        t.kind_str(),
                        t.offset - start,
                        if source_ends {
                            "end of the data"
                        } else {
                            "size cap"
                        }
                    ));
                    if fragmented {
                        if let Some(fe) = last_complete_fragment_end {
                            len = fe - start;
                            status = ExtentStatus::Truncated;
                            err_offset = Some(len);
                        }
                    }
                }
            }
        } else {
            len = walk_end - start;
            status = ExtentStatus::Complete;
            if let Some(e) = max_extent {
                if e > walk_end {
                    // mdat size smaller than the tables say.
                    if e <= region_end {
                        len = e - start;
                        notes.push("mdat size smaller than the sample tables".into());
                        conf = Confidence::MEDIUM;
                    } else {
                        len = region_end - start;
                        status = if source_ends {
                            ExtentStatus::Truncated
                        } else {
                            ExtentStatus::Capped
                        };
                        err_offset = Some(len);
                    }
                }
            }
            if fragmented && moov.as_ref().is_some_and(|m| m.has_mvex) {
                if pending_moof {
                    // A moof without its mdat at the end.
                    if let Some(fe) = last_complete_fragment_end {
                        len = fe - start;
                        status = ExtentStatus::Truncated;
                        err_offset = Some(len);
                    }
                }
            } else if !seen_moov {
                // No moov: the recording was interrupted (data ends or
                // garbage follows), or the window cut it short.
                status = if source_ends || garbage_at.is_some() {
                    ExtentStatus::Truncated
                } else {
                    ExtentStatus::Capped
                };
                err_offset = Some(len);
                notes.push("no moov box".into());
                conf = Confidence::LOW;
            }
        }
        if let Some(e) = &moov_err {
            notes.push(format!("moov does not parse: {e}"));
            conf = Confidence::LOW;
        }
        if let Some(m) = &moov {
            if !m.problems.is_empty() {
                notes.push(format!("moov: {}", m.problems.join("; ")));
            }
        }
        // The tables may describe data that is not there (the file was cut
        // and other data follows): the samples at the end must look like
        // samples.
        if status == ExtentStatus::Complete {
            if let Some(m) = &moov {
                if let Some(real_end) = implausible_tail(s, m, start, start + len)? {
                    let real = real_end.saturating_sub(start);
                    if real > 0 && real < len {
                        notes.push(format!(
                            "samples from offset {real} on are not media data: file cut at about {real} bytes"
                        ));
                        len = real;
                        status = ExtentStatus::Truncated;
                        err_offset = Some(real);
                        conf = Confidence::MEDIUM;
                    }
                }
            }
        }
        if status == ExtentStatus::Complete && moov.is_some() && moov_err.is_none() {
            let ok = moov
                .as_ref()
                .and_then(|m| first_video_sample_ok(s, m, start))
                .unwrap_or(true);
            conf = if ok && notes.is_empty() {
                Confidence::HIGH
            } else {
                Confidence::MEDIUM
            };
        }
        if mdat_box.is_none() && !fragmented {
            notes.push("no mdat box".into());
            conf = Confidence::LOW;
        }
        let conf = conf.lowered(confidence_cut);
        let (mut meta, nested) = match &moov {
            Some(m) => movie_meta(m, start, len),
            None => (MediaMeta::default(), Vec::new()),
        };
        meta.extra.insert("extension".into(), ext.to_string());
        let mut out = Measured {
            len,
            status,
            confidence: conf,
            err_offset,
            meta,
            nested,
            notes,
        };
        if out.len == 0 {
            return Ok(None);
        }
        if s.overlaps_unreadable(start, out.len) {
            // Noted only: the carver applies its own penalty for holes.
            out.notes.push("contains unreadable sectors".into());
        }
        Ok(Some(out))
    }
}

/// End (file-relative) of the data every track's sample tables describe.
/// Tracks whose tables are unusable contribute their largest chunk offset
/// (and a note), so data they point at still counts.
fn tables_extent(m: &Moov, notes: &mut Vec<String>) -> Option<u64> {
    let mut best: Option<u64> = None;
    for t in &m.tracks {
        let e = match boxes::track_extent(t) {
            Some(e) => Some(e),
            None => {
                notes.push(format!(
                    "track {}: sample tables unusable; extent from chunk offsets only",
                    t.tkhd.track_id
                ));
                t.stco.iter().max().map(|o| o.saturating_add(1))
            }
        };
        best = best.max(e);
    }
    best
}

/// Does the stored sample look like a sample of its track? `None` when
/// the track's codec gives nothing to check (PCM, unknown codecs).
fn sample_plausible(
    src: &dyn Source,
    t: &crate::boxes::Trak,
    abs: u64,
    size: u32,
) -> Result<Option<bool>> {
    let Some(e) = t.entry() else { return Ok(None) };
    let video = e
        .avcc
        .as_ref()
        .map(|c| (c.nal_length_size as usize, false))
        .or_else(|| e.hvcc.as_ref().map(|c| (c.nal_length_size as usize, true)));
    let aac = e
        .esds
        .as_ref()
        .filter(|es| es.object_type == 0x40)
        .and_then(|es| crate::aac::parse_audio_specific_config(&es.audio_specific_config))
        .filter(crate::aac::walk_supported);
    if let Some((l, hevc_codec)) = video {
        if size == 0 {
            return Ok(Some(false));
        }
        let want = (size as usize).min(4 << 20);
        let d = match src.read_vec(abs, want) {
            Ok(d) => d,
            Err(_) => return Ok(Some(false)),
        };
        if d.len() < want {
            return Ok(Some(false));
        }
        if want < size as usize {
            // Large sample: check the first NAL header only.
            let n = d.iter().take(l).fold(0u64, |a, &b| (a << 8) | b as u64);
            let h = d.get(l).copied().unwrap_or(0x80);
            return Ok(Some(n > 0 && n + l as u64 <= size as u64 && h & 0x80 == 0));
        }
        return Ok(Some(
            crate::analysis::check_video_sample(&d, l, hevc_codec).0,
        ));
    }
    if let Some(cfg) = aac {
        if size > 1 << 16 {
            return Ok(Some(false));
        }
        let d = match src.read_vec(abs, size as usize) {
            Ok(d) => d,
            Err(_) => return Ok(Some(false)),
        };
        return Ok(Some(crate::analysis::aac_sample_intact(
            &d,
            size,
            Some(&cfg),
        )));
    }
    Ok(None)
}

/// Spot-check the last samples of every checkable track (H.264/HEVC,
/// AAC). When they do not look like samples although the first sample of
/// the track does, binary-search the first implausible sample and return
/// its absolute offset (the smallest over the tracks): the data was cut
/// there. `None` when everything checked looks fine.
fn implausible_tail(src: &dyn Source, m: &Moov, start: u64, end: u64) -> Result<Option<u64>> {
    let mut first_bad: Option<u64> = None;
    for t in &m.tracks {
        let Ok(samples) = sample_table(t) else {
            continue;
        };
        let inside: Vec<&crate::boxes::Sample> = samples
            .iter()
            .filter(|x| x.size > 0 && start.saturating_add(x.end()) <= end)
            .collect();
        let n = inside.len();
        if n < 2 {
            continue;
        }
        let check = |i: usize| -> Result<Option<bool>> {
            let x = inside[i];
            sample_plausible(src, t, start.saturating_add(x.offset), x.size)
        };
        let mut tail_ok = true;
        for i in n.saturating_sub(3)..n {
            match check(i)? {
                None => break,
                Some(v) => tail_ok &= v,
            }
        }
        if tail_ok || check(0)? != Some(true) {
            // Fine, or the check does not fit this track at all.
            continue;
        }
        let (mut lo, mut hi) = (0usize, n - 1);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if check(mid)? == Some(true) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let bad = start.saturating_add(inside[hi].offset);
        first_bad = Some(first_bad.map_or(bad, |b| b.min(bad)));
    }
    Ok(first_bad)
}

/// Look for a moov inside `window` after `from` (relative); returns the
/// moov end (relative) and the parsed moov.
fn analyze_tail_moov(window: &SharedSource, from: u64) -> Result<Option<(u64, Moov)>> {
    let s: &dyn Source = &**window;
    let size = s.size();
    // Scan 4-byte aligned positions of the last 64 MiB for a parseable moov.
    let lo = from.max(size.saturating_sub(64 << 20));
    let mut start = lo;
    while start < size {
        let len = (size - start).min(1 << 20) + 8;
        let buf = s.read_vec(start, len as usize)?;
        let mut i = 0usize;
        while i + 8 <= buf.len() {
            if &buf[i + 4..i + 8] == b"moov" {
                let at = start + i as u64;
                if let Some(h) = boxes::read_header(s, at, size)? {
                    if h.end(size) <= size {
                        if let Ok(m) = parse_moov(s, &h) {
                            return Ok(Some((h.end(size), m)));
                        }
                    }
                }
            }
            i += 1;
        }
        start += 1 << 20;
    }
    Ok(None)
}

/// Validate a complete movie file held by any source.
pub fn validate_source(src: &dyn Source) -> Result<Validation> {
    let a = analyze(src)?;
    Ok(validation_from(&a))
}

fn validation_from(a: &Analysis) -> Validation {
    let mut v = Validation {
        status: ValidationStatus::Valid,
        intact_fraction: Some(1.0),
        problems: a.problems.clone(),
        meta: MediaMeta::default(),
        repairable_by: Vec::new(),
    };
    let structured =
        a.boxes.iter().any(|b| b.header.is(b"ftyp")) || a.moov_box.is_some() || a.mdat.is_some();
    if !structured {
        return Validation::invalid("not an ISO BMFF movie (no ftyp, moov or mdat box)");
    }
    if let Some(m) = &a.moov {
        let (meta, _) = movie_meta(m, 0, a.size);
        v.meta = meta;
    }
    if let Some(major) = a.major_brand {
        if let BrandClass::Movie(e) = classify_brands(&major, &a.compatible) {
            v.meta.extra.insert("extension".into(), e.into());
        }
    }
    let (declared, inside) = a.sample_counts();
    if a.moov.is_none() {
        v.intact_fraction = None;
    } else if declared > 0 {
        v.intact_fraction = Some(inside as f32 / declared as f32);
    }
    if !v.problems.is_empty() {
        v.status = ValidationStatus::Damaged;
    }
    if a.mdat.is_none() && !a.fragmented && a.moov.is_some() && declared > 0 {
        v.status = ValidationStatus::Damaged;
        v.problems.push("mdat missing".into());
    }
    let ctx = RepairContext::default();
    v.repairable_by = repair::plan(a, &ctx)
        .iter()
        .map(|s| s.name().to_string())
        .collect();
    let moov_bad = a.moov.is_none() || (a.moov_stale() && a.relocation_delta.is_none());
    if moov_bad && !a.fragmented && a.mdat.is_some() {
        v.repairable_by
            .push("rebuild_moov (needs a reference clip)".into());
    }
    v
}

impl FileFormat for Mp4Format {
    fn id(&self) -> &'static str {
        "mp4"
    }

    fn name(&self) -> &'static str {
        "MP4/QuickTime movie"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mp4", "mov", "m4v", "3gp", "3g2", "m4a"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn min_size(&self) -> u64 {
        256
    }

    fn max_size(&self) -> u64 {
        256 << 30
    }

    fn priority(&self) -> u8 {
        70
    }

    fn probe(&self, head: &[u8]) -> bool {
        match parse_ftyp(head) {
            Some((_, major, compat)) => {
                matches!(classify_brands(&major, &compat), BrandClass::Movie(_))
            }
            None => quicktime_head_plausible(head),
        }
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "png" | "heic" | "tiff")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        self.measure_impl(src, start, limit)
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        validate_source(&**src)
    }

    fn repair(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        let a = analyze(&**src)?;
        let v = validation_from(&a);
        if v.status == ValidationStatus::Valid {
            let n = repair::copy_source(&**src, out)?;
            return Ok(RepairOutcome {
                result: RepairResult::NoRepairNeeded,
                applied: Vec::new(),
                output_len: n,
                validation: Some(v),
                notes: Vec::new(),
            });
        }
        let strategies: Vec<Strategy> = repair::plan(&a, ctx);
        if strategies.is_empty() {
            let mut notes = vec!["no applicable repair".to_string()];
            if a.moov.is_none() && ctx.donors.is_empty() {
                notes.push(
                    "the moov is missing: supply a healthy clip from the same device as a donor"
                        .into(),
                );
            }
            return Ok(RepairOutcome {
                result: RepairResult::Unrepairable,
                applied: Vec::new(),
                output_len: 0,
                validation: Some(v),
                notes,
            });
        }
        repair::apply(src, out, &strategies, ctx)
    }

    fn can_repair(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brands() {
        assert_eq!(classify_brands(b"qt  ", &[]), BrandClass::Movie("mov"));
        assert_eq!(classify_brands(b"3gp4", &[]), BrandClass::Movie("3gp"));
        assert_eq!(classify_brands(b"M4V ", &[]), BrandClass::Movie("m4v"));
        assert_eq!(classify_brands(b"isom", &[]), BrandClass::Movie("mp4"));
        assert_eq!(classify_brands(b"heic", &[]), BrandClass::Heif("heic"));
        assert_eq!(
            classify_brands(b"mif1", &[*b"avif"]),
            BrandClass::Heif("avif")
        );
        assert_eq!(classify_brands(b"crx ", &[]), BrandClass::Cr3);
        assert_eq!(
            classify_brands(b"zzzz", &[*b"mp42"]),
            BrandClass::Movie("mp4")
        );
        assert_eq!(classify_brands(b"zzzz", &[]), BrandClass::Unknown);
    }
}
