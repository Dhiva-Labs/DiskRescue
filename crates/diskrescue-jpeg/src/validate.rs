//! Whole-file validation: header walk + Huffman scan analysis + a
//! reference decode with zune-jpeg.
//!
//! The analyzer is authoritative for *where* a file is damaged; zune-jpeg
//! (run in strict mode, then lenient mode) confirms that the bytes really
//! decode and stands in when the analyzer cannot look at the entropy data
//! (arithmetic coding, DNL, missing tables). zune-jpeg is only run on files
//! up to [`ZUNE_MAX_BYTES`] and [`ZUNE_MAX_PIXELS`], inside a panic guard.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::time::Duration;

use diskrescue_core::format::{RepairContext, Validation, ValidationStatus};
use diskrescue_core::media::MediaMeta;
use diskrescue_core::source::{ReadError, Source};
use diskrescue_core::{Range, Result};
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::cursor::{Cursor, SCAN_CHUNK};
use crate::markers::{self, HeaderEnd, Headers, APP0};
use crate::repair::plan;
use crate::scan::{
    analyze_after_headers, AnalysisDepth, AnalyzeOptions, ScanAnalysis, ScanErrorKind,
};
use crate::trailer::extend_after_eoi;

/// Largest file handed to zune-jpeg.
pub const ZUNE_MAX_BYTES: u64 = 64 << 20;
/// Largest image (in pixels) handed to zune-jpeg.
pub const ZUNE_MAX_PIXELS: u64 = 50_000_000;
/// How far into a file the SOI is searched for when it does not start
/// with one.
pub const SOI_SEARCH: u64 = 64 * 1024;

/// Outcome of the zune-jpeg reference decode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZuneCheck {
    /// Strict decode succeeded.
    pub strict_ok: bool,
    /// Lenient decode succeeded (missing data filled with grey).
    pub lenient_ok: bool,
    /// Decoded width.
    pub width: u32,
    /// Decoded height.
    pub height: u32,
    /// Rows at the bottom of the lenient output that are uniformly grey
    /// (the decoder's fill for missing data).
    pub grey_tail_rows: u32,
    /// Error of the strict decode, if it failed.
    pub error: Option<String>,
}

/// Longest a reference decode may take before it is abandoned.
pub const ZUNE_TIMEOUT: Duration = Duration::from_secs(60);

/// Decode `data` with zune-jpeg, strictly and then leniently.
///
/// The decoder allocates its whole output up front from the frame header
/// it parses itself (which need not be the one this crate's header walk
/// used: a second SOF, a different resynchronisation), so the dimensions
/// are read first and images above [`ZUNE_MAX_PIXELS`] are not decoded:
/// an allocation of several GiB aborts the process, which no panic guard
/// can catch.
///
/// zune-jpeg 0.4 can also loop forever: its lenient progressive decoder
/// retries the last marker when the data ends without an EOI. The lenient
/// pass therefore always sees an EOI at the end, and the whole check runs
/// on a worker thread that is abandoned after [`ZUNE_TIMEOUT`] (the
/// result then says so) instead of hanging the caller.
pub fn zune_check(data: &[u8]) -> ZuneCheck {
    zune_check_owned(data.to_vec())
}

/// [`zune_check`] on an owned buffer (no extra copy).
pub(crate) fn zune_check_owned(data: Vec<u8>) -> ZuneCheck {
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("zune-check".into())
        .spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| zune_decode(data)));
            let _ = tx.send(r.unwrap_or_else(|_| ZuneCheck {
                error: Some("decoder panicked".into()),
                ..Default::default()
            }));
        });
    if worker.is_err() {
        return ZuneCheck {
            error: Some("reference decoder could not be started".into()),
            ..Default::default()
        };
    }
    rx.recv_timeout(ZUNE_TIMEOUT).unwrap_or_else(|_| ZuneCheck {
        error: Some(format!(
            "reference decoder gave up after {} s",
            ZUNE_TIMEOUT.as_secs()
        )),
        ..Default::default()
    })
}

fn zune_decode(mut data: Vec<u8>) -> ZuneCheck {
    let base = DecoderOptions::default()
        .set_max_width(1 << 16)
        .set_max_height(1 << 16);
    let mut out = ZuneCheck::default();
    let headers = catch_unwind(AssertUnwindSafe(|| {
        let mut d = JpegDecoder::new_with_options(&data[..], base.set_strict_mode(false));
        d.decode_headers().map(|_| d.info())
    }));
    match headers {
        Ok(Ok(Some(info))) => {
            let pixels = info.width as u64 * info.height as u64;
            if pixels == 0 || pixels > ZUNE_MAX_PIXELS {
                out.error = Some(format!(
                    "{}x{} frame: not decoded (limit {ZUNE_MAX_PIXELS} pixels)",
                    info.width, info.height
                ));
                return out;
            }
        }
        Ok(Ok(None)) => {
            out.error = Some("no frame header".into());
            return out;
        }
        Ok(Err(e)) => {
            out.error = Some(e.to_string());
            return out;
        }
        Err(_) => {
            out.error = Some("decoder panicked".into());
            return out;
        }
    }
    let strict = catch_unwind(AssertUnwindSafe(|| {
        let mut d = JpegDecoder::new_with_options(&data[..], base.set_strict_mode(true));
        d.decode().map(|_| d.info())
    }));
    match strict {
        Ok(Ok(info)) => {
            out.strict_ok = true;
            out.lenient_ok = true;
            if let Some(i) = info {
                out.width = i.width as u32;
                out.height = i.height as u32;
            }
            return out;
        }
        Ok(Err(e)) => out.error = Some(e.to_string()),
        Err(_) => out.error = Some("decoder panicked".into()),
    }
    if !data.ends_with(&[0xFF, 0xD9]) {
        data.extend_from_slice(&[0xFF, 0xD9]);
    }
    let lenient = catch_unwind(AssertUnwindSafe(|| {
        let mut d = JpegDecoder::new_with_options(&data[..], base.set_strict_mode(false));
        d.decode().map(|px| (px, d.info()))
    }));
    if let Ok(Ok((px, Some(info)))) = lenient {
        out.lenient_ok = true;
        out.width = info.width as u32;
        out.height = info.height as u32;
        let (w, h) = (info.width as usize, info.height as usize);
        if w > 0 && h > 0 && px.len() >= w * h {
            let row = px.len() / h;
            let mut grey = 0u32;
            for r in (0..h).rev() {
                let line = &px[r * row..(r + 1) * row];
                if line.iter().all(|&v| v.abs_diff(128) <= 1) {
                    grey += 1;
                } else {
                    break;
                }
            }
            out.grey_tail_rows = grey;
        }
    }
    out
}

/// Detailed validation result shared by `validate`, `repair` and the
/// re-validation of repaired output.
#[derive(Debug, Clone)]
pub(crate) struct Detail {
    pub(crate) validation: Validation,
    pub(crate) headers: Option<Headers>,
    pub(crate) analysis: Option<ScanAnalysis>,
    pub(crate) zune: Option<ZuneCheck>,
    /// Bytes after the recognised end (EOI, MPF images, trailers).
    pub(crate) extra_trailing: u64,
}

/// Up to `len` bytes at `offset`, stopping at the first unreadable byte (a
/// bad sector further on must not hide the readable start of a file).
/// `Err` only when not even the first byte can be read.
pub(crate) fn read_prefix(
    src: &dyn Source,
    offset: u64,
    len: usize,
) -> std::result::Result<Vec<u8>, ReadError> {
    let size = src.size();
    if offset >= size {
        return Ok(Vec::new());
    }
    let want = (size - offset).min(len as u64) as usize;
    let mut buf = vec![0u8; want];
    let mut done = 0usize;
    while done < want {
        let pos = offset + done as u64;
        match src.read_at(pos, &mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n.min(want - done),
            Err(e) => {
                // Bytes before the failing offset may not have been
                // stored: read that good stretch on its own.
                let good = e.offset.saturating_sub(pos).min((want - done) as u64) as usize;
                if good > 0 {
                    if let Ok(n) = src.read_at(pos, &mut buf[done..done + good]) {
                        done += n.min(good);
                    }
                }
                if done == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }
    buf.truncate(done);
    Ok(buf)
}

/// Find the SOI of the file: offset 0, or the first `FF D8 FF` in the first
/// [`SOI_SEARCH`] bytes whose header chain reaches a scan.
pub(crate) fn find_soi(src: &dyn Source) -> Option<u64> {
    let size = src.size();
    let head = read_prefix(src, 0, SOI_SEARCH.min(size) as usize).ok()?;
    if head.starts_with(&[0xFF, 0xD8]) {
        return Some(0);
    }
    let mut from = 0usize;
    for _ in 0..64 {
        let i = memchr::memmem::find(&head[from..], &[0xFF, 0xD8, 0xFF])? + from;
        if let Ok(h) = markers::walk_headers(src, i as u64, size - i as u64) {
            if h.reached_scan() {
                return Some(i as u64);
            }
        }
        from = i + 1;
    }
    None
}

/// Heuristic for a file whose header was destroyed but whose scan data
/// survives: it ends with EOI (allowing some slack) and contains stuffed
/// bytes or remnants of header segments near the start.
pub(crate) fn looks_like_headerless(src: &dyn Source) -> bool {
    let size = src.size();
    if size < 64 {
        return false;
    }
    let tail_len = size.min(4096);
    let Ok(tail) = read_prefix(src, size - tail_len, tail_len as usize) else {
        return false;
    };
    let has_eoi = tail.windows(2).any(|w| w == [0xFF, 0xD9]);
    let Ok(head) = read_prefix(src, 0, SOI_SEARCH.min(size) as usize) else {
        return false;
    };
    let stuffed = head.windows(2).filter(|w| *w == [0xFF, 0x00]).count();
    let segments = head
        .windows(2)
        .filter(|w| w[0] == 0xFF && matches!(w[1], 0xC0 | 0xC2 | 0xC4 | 0xDA | 0xDB))
        .count();
    has_eoi && (stuffed >= 2 || segments >= 1)
}

/// Metadata from the headers.
pub(crate) fn build_meta(h: &Headers) -> MediaMeta {
    let mut m = MediaMeta::default();
    if let Some(f) = &h.sof {
        m.width = Some(f.width as u32);
        m.height = (f.height > 0).then_some(f.height as u32);
        m.extra.insert("jpeg.process".into(), f.process().into());
        m.extra
            .insert("jpeg.components".into(), f.components.len().to_string());
        if f.precision != 8 {
            m.extra
                .insert("jpeg.precision".into(), f.precision.to_string());
        }
        if let Some(s) = f.subsampling() {
            m.extra.insert("jpeg.subsampling".into(), s);
        }
    }
    if let Some(e) = &h.exif {
        e.fill_meta(&mut m);
        if m.height.is_none() {
            m.height = e.height;
        }
    }
    if let Some(ri) = h.dri.filter(|&r| r > 0) {
        m.extra
            .insert("jpeg.restart_interval".into(), ri.to_string());
    }
    if let Some(mpf) = &h.mpf {
        m.extra
            .insert("jpeg.mpf_images".into(), mpf.entries.len().to_string());
        if mpf.is_mpo() {
            m.extra.insert("jpeg.mpo".into(), "true".into());
        }
    }
    if h.motion_photo.as_ref().is_some_and(|x| x.motion_photo) {
        m.extra.insert("jpeg.motion_photo".into(), "true".into());
    }
    if !h.thumbnails.is_empty() {
        m.extra
            .insert("jpeg.thumbnails".into(), h.thumbnails.len().to_string());
    }
    m
}

/// True when this looks like a stand-alone thumbnail (carving-engine 2.2.1:
/// at most 16 KiB, first marker APP0, smaller than 200x200).
pub(crate) fn probable_thumbnail(h: &Headers, len: u64) -> bool {
    let first_app0 = h.segments.first().is_some_and(|s| s.marker == APP0);
    let small = h
        .sof
        .as_ref()
        .is_some_and(|f| f.width < 200 && f.height < 200);
    len <= 16 * 1024 && first_app0 && small
}

/// Describe the analyzer's first error for a report.
pub(crate) fn describe_error(a: &ScanAnalysis, base: u64) -> Option<String> {
    let e = a.first_error?;
    let pct = (a.row_fraction() * 100.0).floor() as u32;
    Some(match e.kind {
        ScanErrorKind::Eof => {
            if a.progressive && a.mcu_rows_decoded >= a.mcu_rows_total && a.mcu_rows_total > 0 {
                let n = a.scans.iter().filter(|s| s.complete).count();
                format!(
                    "truncated progressive JPEG: {n} complete scan(s); the whole image is \
                     visible at reduced quality"
                )
            } else {
                format!("truncated: {pct}% of MCU rows decodable")
            }
        }
        ScanErrorKind::DataAfterEoi => format!(
            "unexpected entropy-coded data after the last MCU at offset {}",
            base + e.byte_offset
        ),
        kind => {
            let scan = a.scans.get(e.scan as usize);
            let place = if a.restart_interval > 0 {
                let ri = a.restart_interval as u64;
                let total = scan.map(|s| s.mcus_total).unwrap_or(a.mcus_total);
                format!(
                    "restart interval {} of {}",
                    e.mcu_index / ri + 1,
                    total.div_ceil(ri).max(1)
                )
            } else {
                let per_row = scan
                    .filter(|s| s.rows_total > 0)
                    .map(|s| (s.mcus_total / s.rows_total as u64).max(1))
                    .unwrap_or(1);
                let rows = scan.map(|s| s.rows_total).unwrap_or(a.mcu_rows_total);
                format!("MCU row {} of {}", e.mcu_index / per_row + 1, rows)
            };
            let scan_txt = if a.progressive {
                format!(", scan {}", e.scan + 1)
            } else {
                String::new()
            };
            format!(
                "corrupt scan data at offset {} ({place}{scan_txt}; {kind:?})",
                base + e.byte_offset
            )
        }
    })
}

/// Full validation of a source that spans exactly one file.
pub(crate) fn validate_source(src: &dyn Source, run_zune: bool) -> Result<Detail> {
    let size = src.size();
    let fail = |status: ValidationStatus, problems: Vec<String>, rep: Vec<String>| Detail {
        validation: Validation {
            status,
            intact_fraction: Some(0.0),
            problems,
            meta: MediaMeta::default(),
            repairable_by: rep,
        },
        headers: None,
        analysis: None,
        zune: None,
        extra_trailing: 0,
    };
    if size < 4 {
        return Ok(fail(
            ValidationStatus::Invalid,
            vec!["file too small to be a JPEG".into()],
            vec![],
        ));
    }
    // Nothing readable at the start: that is a read error to report, not
    // evidence that the file is no JPEG.
    read_prefix(src, 0, 4)?;
    let Some(soi) = find_soi(src) else {
        if looks_like_headerless(src) {
            let mut d = fail(
                ValidationStatus::Damaged,
                vec![
                    "JPEG header missing or destroyed (no SOI marker); a healthy file from \
                     the same camera can supply it"
                        .into(),
                ],
                vec!["donor_header".into()],
            );
            d.validation.intact_fraction = None;
            return Ok(d);
        }
        return Ok(fail(
            ValidationStatus::Invalid,
            vec!["no JPEG start-of-image marker".into()],
            vec![],
        ));
    };
    let mut cur = Cursor::new(src, soi, size - soi, SCAN_CHUNK);
    let h = markers::walk(&mut cur)?;
    let mut meta = build_meta(&h);
    let mut problems = Vec::new();
    if soi > 0 {
        problems.push(format!("{soi} bytes of junk before the SOI marker"));
    }
    problems.extend(h.issues.iter().map(|i| i.describe()));
    let aggressive = RepairContext {
        aggressive: true,
        ..Default::default()
    };

    if !h.reached_scan() {
        problems.push(match h.end {
            HeaderEnd::Sos if h.sof.is_none() => "SOF (frame header) missing".to_string(),
            HeaderEnd::Sos => "first SOS segment is malformed".to_string(),
            HeaderEnd::Eoi { offset } => format!("EOI at offset {} before any scan", soi + offset),
            HeaderEnd::NestedSoi { offset } => {
                format!(
                    "a new image starts at offset {} inside the header",
                    soi + offset
                )
            }
            HeaderEnd::Truncated { offset } => {
                format!("truncated inside the header at offset {}", soi + offset)
            }
            HeaderEnd::Unreadable { offset } => {
                format!(
                    "unreadable data inside the header at offset {}",
                    soi + offset
                )
            }
            HeaderEnd::NoMarker { offset } => {
                format!("header segment chain broken at offset {}", soi + offset)
            }
            HeaderEnd::TooLarge { offset } => {
                format!(
                    "header larger than allowed (gave up at offset {})",
                    soi + offset
                )
            }
        });
        let a = ScanAnalysis::default();
        let mut rep: Vec<String> = plan(&h, &a, &aggressive)
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        rep.push("donor_header".into());
        rep.dedup();
        return Ok(Detail {
            validation: Validation {
                status: ValidationStatus::Damaged,
                intact_fraction: Some(0.0),
                problems,
                meta,
                repairable_by: rep,
            },
            headers: Some(h),
            analysis: None,
            zune: None,
            extra_trailing: 0,
        });
    }

    let sof = h.sof.clone().expect("reached_scan implies SOF");
    let mut structural = soi > 0 || !h.issues.is_empty();
    if let Err(e) = sof.check() {
        problems.push(format!("invalid frame header: {e}"));
        structural = true;
    }
    let missing_q = h.missing_quant_tables();
    if !missing_q.is_empty() {
        problems.push(format!("DQT missing (quantisation table {missing_q:?})"));
        structural = true;
    }
    let opts = AnalyzeOptions {
        unreadable: src
            .unreadable()
            .map(|set| {
                set.intersection_with_range(Range::with_len(soi, size - soi))
                    .shifted_down(soi)
            })
            .filter(|set| !set.is_empty()),
        ..Default::default()
    };
    let a = analyze_after_headers(&mut cur, &h, &opts);
    if a.used_default_tables {
        if h.avi1 {
            meta.extra
                .insert("jpeg.mjpeg_default_tables".into(), "true".into());
        } else {
            problems.push("DHT missing (Motion-JPEG frame without Huffman tables)".into());
            structural = true;
        }
    }
    if let Some(e) = &a.first_error {
        let _ = e;
        if let Some(text) = describe_error(&a, soi) {
            problems.push(text);
        }
    }
    if a.corrupt_intervals > 0 {
        problems.push(format!(
            "{} restart interval(s) damaged; decoding resumes after them",
            a.corrupt_intervals
        ));
    }
    let mut extra_trailing = 0;
    // End of everything that belongs to the file (file-relative).
    let mut extent_end = a.data_end;
    if let Some(eoi) = a.eoi_offset {
        let after = extend_after_eoi(&mut cur, &h, eoi + 2);
        // MPF images and trailers are part of the file: read them once so
        // bad sectors there are noticed as well.
        cur.read_through(eoi + 2, after.end);
        extent_end = after.end;
        let extra = (size - soi).saturating_sub(after.end);
        extra_trailing = extra;
        if extra > 0 {
            problems.push(format!(
                "{extra} bytes of unrecognised data after the end of the image"
            ));
        }
        if after.truncated {
            // The primary image is whole; what is missing is an extra
            // image or a trailer (or a stale MPF index), so this is
            // reported without calling the file damaged.
            problems.extend(
                after
                    .notes
                    .iter()
                    .filter(|n| n.contains("cut off") || n.contains("beyond"))
                    .cloned(),
            );
        }
        if !after.kinds.is_empty() {
            meta.extra
                .insert("jpeg.trailer".into(), after.kinds.join(","));
        }
    } else if a.first_error.is_none() {
        problems.push("EOI marker missing".into());
    }
    if let Some(off) = cur.read_error_offset() {
        problems.push(format!("unreadable data at offset {}", soi + off));
        structural = true;
    }
    // Bad sectors the source already knows about (a rescued image's
    // mapfile) anywhere in the file: the bytes there are not the file's.
    if let Some(bad) = src.unreadable() {
        let holes = bad.intersection_with_range(Range::new(soi, soi + extent_end.max(1)));
        if !holes.is_empty() {
            if cur.read_error_offset().is_none() {
                problems.push(format!(
                    "{} unreadable bytes inside the file (first at offset {})",
                    holes.total_len(),
                    holes.iter().next().map(|r| r.start).unwrap_or(soi)
                ));
            }
            structural = true;
        }
    }

    let pixels = sof.width as u64 * sof.height as u64;
    // The reference decoder needs the primary image only; a hole in it
    // ends the bytes handed over (the decoder then sees a truncated file).
    let primary_len = a.eoi_offset.map_or(size - soi, |e| (e + 2).min(size - soi));
    let zune = (run_zune
        && primary_len <= ZUNE_MAX_BYTES
        && pixels > 0
        && pixels <= ZUNE_MAX_PIXELS
        && !sof.lossless)
        .then(|| {
            read_prefix(src, soi, primary_len as usize)
                .ok()
                .map(zune_check_owned)
        })
        .flatten();

    let analysis_ok = a.first_error.is_none() && a.eoi_offset.is_some();
    let unsure = a.depth == AnalysisDepth::Structural;
    let zune_ok = zune.as_ref().map(|z| z.strict_ok);
    let valid = analysis_ok && !structural && (!unsure || zune_ok.unwrap_or(true));
    if let Some(z) = &zune {
        if analysis_ok && !z.strict_ok {
            // zune-jpeg 0.4 fails on some valid files (progressive scans
            // with restart markers, which libjpeg decodes). When every
            // symbol was decoded here, its verdict is only recorded.
            let why = z.error.clone().unwrap_or_default();
            if z.lenient_ok || a.depth == AnalysisDepth::Full {
                meta.extra
                    .insert("jpeg.reference_decoder".into(), format!("strict: {why}"));
            } else {
                problems.push(format!("reference decoder rejects the file: {why}"));
            }
        }
    }
    let intact = if valid {
        1.0
    } else if unsure {
        match &zune {
            Some(z) if z.strict_ok => 1.0,
            Some(z) if z.lenient_ok && z.height > 0 => {
                1.0 - z.grey_tail_rows as f32 / z.height as f32
            }
            Some(_) => 0.0,
            None => {
                if analysis_ok {
                    1.0
                } else {
                    0.0
                }
            }
        }
    } else if a.first_error.is_some() {
        if a.corrupt_intervals > 0 {
            a.mcu_fraction()
        } else {
            a.row_fraction()
        }
    } else {
        1.0
    };
    if unsure {
        meta.extra
            .insert("jpeg.scan_validation".into(), "structural".into());
    }
    let mut rep: Vec<String> = plan(&h, &a, &aggressive)
        .iter()
        .map(|s| s.name().to_string())
        .collect();
    // The plan cannot see trailers (Samsung SEF needs a search); the
    // extent computed here decides whether anything is really unrecognised.
    rep.retain(|r| r != "strip_trailing_garbage");
    if extra_trailing > 0 {
        rep.push("strip_trailing_garbage".into());
    }
    let mut status = if valid {
        ValidationStatus::Valid
    } else {
        ValidationStatus::Damaged
    };
    let mut intact = intact;
    // A file repaired earlier carries a comment saying how much of it is grey
    // filler: it decodes, but that part of the picture is lost.
    if let Some((padded, total)) = grey_fill_comment(src, soi, &h) {
        status = ValidationStatus::Damaged;
        intact = intact.min(1.0 - padded as f32 / total as f32);
        problems.push(format!(
            "partly recovered: {padded} of {total} blocks are grey filler written by an earlier repair \
             (that part of the picture was lost)"
        ));
    }
    Ok(Detail {
        validation: Validation {
            status,
            intact_fraction: Some(intact.clamp(0.0, 1.0)),
            problems,
            meta,
            repairable_by: rep,
        },
        headers: Some(h),
        analysis: Some(a),
        zune,
        extra_trailing,
    })
}

/// Comment written into a JPEG whose missing part a repair filled with grey
/// blocks; followed by `<padded>/<total>` MCU counts.
pub const GREY_FILL_COMMENT: &str = "DiskRescue repair: grey fill ";

/// The `(padded, total)` MCU counts of a grey-fill comment, if the file has one.
pub(crate) fn grey_fill_comment(src: &dyn Source, soi: u64, h: &Headers) -> Option<(u64, u64)> {
    use diskrescue_core::source::SourceExt;
    for seg in h.segments.iter().filter(|s| s.marker == 0xFE) {
        let n = (seg.len as usize).saturating_sub(2).min(96);
        let mut buf = vec![0u8; n];
        if src.read_exact_at(soi + seg.data_offset, &mut buf).is_err() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        let Some(rest) = text.strip_prefix(GREY_FILL_COMMENT) else {
            continue;
        };
        let Some((a, b)) = rest.trim_end_matches('\0').trim().split_once('/') else {
            continue;
        };
        if let (Ok(padded), Ok(total)) = (a.parse::<u64>(), b.parse::<u64>()) {
            if total > 0 && padded <= total {
                return Some((padded, total));
            }
        }
    }
    None
}
