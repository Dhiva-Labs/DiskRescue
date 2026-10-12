//! JPEG repair (image-formats spec 3.4-3.6), without re-encoding.
//!
//! [`plan`] turns what the header walk and the scan analyzer found into a
//! list of [`Strategy`]s; [`apply`] assembles the repaired file as a list
//! of pieces (byte ranges of the damaged file, of a donor, and a few new
//! bytes), re-validates that virtual file (header walk, Huffman analysis,
//! zune-jpeg decode) and only then streams it to the output. Nothing is
//! written when the result still does not decode.
//!
//! Strategies:
//!
//! * `StructuralFix`: drop junk before SOI, duplicate SOIs and stray bytes;
//!   rewrite damaged APPn/COM lengths to the resynchronised chain.
//! * `InsertStandardHuffmanTables`: insert the Annex K DHT before SOS
//!   (Motion-JPEG frames and other DHT-less files).
//! * `DonorHeader`: replace SOI..SOS with a healthy file's header (same
//!   dimensions, components and sampling required); the scan data start is
//!   the damaged file's intact SOS, else the best-decoding candidate
//!   offset (surviving SOS, the donor's header length, the end of a zeroed
//!   run, an `00 3F 00` SOS tail), or the offset given in the strategy.
//! * `AppendEoi`: end the data with `FF D9` (dropping a dangling `FF` and
//!   any foreign data after the scan).
//! * `TruncateAtLastGoodRestart`: cut at the restart marker that starts the
//!   first damaged interval, append EOI.
//! * `TruncateAtLastGoodMcuRow`: baseline with restarts: cut at the last
//!   restart boundary that starts an MCU row; baseline without restarts:
//!   keep the data up to the last good MCU (its last byte padded with
//!   1-bits) and append EOI, decoders show the rest grey; progressive: keep
//!   the complete scans and append EOI.
//! * `StripTrailingGarbage`: drop unrecognised bytes after the EOI (MPF
//!   images and motion-photo trailers are kept).

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use diskrescue_core::format::{RepairContext, RepairOutcome, RepairResult, ValidationStatus};
use diskrescue_core::source::{FileSource, ReadError, SharedSource, Source, SourceExt};
use diskrescue_core::Result;

use crate::cursor::{Cursor, SCAN_CHUNK};
use crate::markers::{self, parse_sof, walk_headers, FrameInfo, Headers};
use crate::scan::{
    analyze_after_headers, analyze_source, AnalyzeOptions, ScanAnalysis, ScanErrorKind,
};
use crate::tables::standard_dht;
use crate::trailer::extend_after_eoi;
use crate::validate::{find_soi, validate_source};

/// MCUs decoded when scoring a candidate scan start for a donor header.
const SCORE_MCUS: u64 = 4096;
/// How much of the damaged file is searched for header remnants.
const REMNANT_SEARCH: u64 = 1 << 20;

/// One repair step. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Strategy {
    /// Rebuild the header segment chain (junk, duplicate SOI, stray bytes,
    /// damaged APPn/COM lengths).
    StructuralFix,
    /// Append the EOI marker.
    AppendEoi,
    /// Cut at the restart marker before the first damaged interval.
    TruncateAtLastGoodRestart,
    /// Cut at the last complete MCU row (or the last good MCU / the last
    /// complete progressive scan).
    TruncateAtLastGoodMcuRow,
    /// Insert the Annex K Huffman tables before SOS.
    InsertStandardHuffmanTables,
    /// Replace the header with a donor's.
    DonorHeader {
        /// Healthy JPEG from the same camera and settings.
        donor: PathBuf,
        /// File offset where the damaged file's scan data starts; `None`
        /// lets the repair find it.
        scan_offset: Option<u64>,
    },
    /// Drop unrecognised bytes after the EOI.
    StripTrailingGarbage,
}

impl Strategy {
    /// Stable name used in `Validation::repairable_by` and reports.
    pub fn name(&self) -> &'static str {
        match self {
            Strategy::StructuralFix => "structural_fix",
            Strategy::AppendEoi => "append_eoi",
            Strategy::TruncateAtLastGoodRestart => "truncate_at_last_good_restart",
            Strategy::TruncateAtLastGoodMcuRow => "truncate_at_last_good_mcu_row",
            Strategy::InsertStandardHuffmanTables => "insert_standard_huffman_tables",
            Strategy::DonorHeader { .. } => "donor_header",
            Strategy::StripTrailingGarbage => "strip_trailing_garbage",
        }
    }
}

/// True when two frames share dimensions, component count, sampling,
/// precision and coding process (what a donor header must match).
pub fn same_geometry(a: &FrameInfo, b: &FrameInfo) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.precision == b.precision
        && a.progressive == b.progressive
        && a.components.len() == b.components.len()
        && a.components
            .iter()
            .zip(&b.components)
            .all(|(x, y)| x.h == y.h && x.v == y.v)
}

fn describe_frame(f: &FrameInfo) -> String {
    let s = f
        .subsampling()
        .unwrap_or_else(|| format!("{} components", f.components.len()));
    format!("{}x{} {} {}", f.width, f.height, f.process(), s)
}

/// Choose the repair steps for a file. `ctx.aggressive` allows steps that
/// discard data (truncation); donors come from `ctx.donors`.
pub fn plan(headers: &Headers, analysis: &ScanAnalysis, ctx: &RepairContext) -> Vec<Strategy> {
    let mut v = Vec::new();
    let header_broken = !headers.reached_scan()
        || !headers.missing_quant_tables().is_empty()
        || headers.sof.as_ref().is_some_and(|f| f.check().is_err());
    if header_broken {
        if let Some(d) = pick_donor(headers, ctx) {
            v.push(Strategy::DonorHeader {
                donor: d,
                scan_offset: None,
            });
        }
        return v;
    }
    if headers.soi_offset > 0 || !headers.issues.is_empty() {
        v.push(Strategy::StructuralFix);
    }
    if analysis.used_default_tables && !headers.avi1 {
        v.push(Strategy::InsertStandardHuffmanTables);
    }
    let no_eoi = analysis.eoi_offset.is_none();
    match analysis.first_error {
        Some(e) if e.kind == ScanErrorKind::Eof => {
            if ctx.aggressive {
                v.push(Strategy::TruncateAtLastGoodMcuRow);
            } else if no_eoi {
                v.push(Strategy::AppendEoi);
            }
        }
        Some(e) if e.kind == ScanErrorKind::DataAfterEoi => {
            if no_eoi {
                v.push(Strategy::AppendEoi);
            }
        }
        Some(_) => {
            if ctx.aggressive {
                if analysis.restart_interval > 0 && analysis.last_good_restart.is_some() {
                    v.push(Strategy::TruncateAtLastGoodRestart);
                } else {
                    v.push(Strategy::TruncateAtLastGoodMcuRow);
                }
            } else if no_eoi {
                v.push(Strategy::AppendEoi);
            }
        }
        None => {
            if no_eoi {
                v.push(Strategy::AppendEoi);
            }
        }
    }
    let motion = headers
        .motion_photo
        .as_ref()
        .is_some_and(|m| m.motion_photo);
    // With MPF images or a motion-photo trailer the bytes after EOI are
    // mostly legitimate. `plan` cannot see trailers that need a search
    // (Samsung SEF); applying the strip step keeps every recognised
    // structure anyway, and `JpegFormat::repair` only uses it when
    // validation found unrecognised bytes.
    if analysis.trailing_bytes > 0
        && analysis.eoi_offset.is_some()
        && headers.mpf.is_none()
        && !motion
    {
        v.push(Strategy::StripTrailingGarbage);
    }
    v
}

/// The donor to use: the first readable one whose frame matches the damaged
/// file; when the damaged file's frame is unknown, the first usable donor;
/// when none matches, the first usable donor (apply then explains why it
/// cannot be used).
fn pick_donor(headers: &Headers, ctx: &RepairContext) -> Option<PathBuf> {
    let mut fallback = None;
    for d in ctx.donors.iter().take(32) {
        let Ok(f) = FileSource::open(d) else {
            continue;
        };
        let Some(s) = find_soi(&f) else {
            continue;
        };
        let Ok(h) = walk_headers(&f, s, f.size() - s) else {
            continue;
        };
        if !h.reached_scan() || !h.missing_quant_tables().is_empty() {
            continue;
        }
        let sof = h.sof.as_ref().expect("reached_scan implies SOF");
        match &headers.sof {
            Some(t) if same_geometry(t, sof) => return Some(d.clone()),
            _ => {
                fallback.get_or_insert_with(|| d.clone());
            }
        }
    }
    fallback
}

// ---------------------------------------------------------------------
// Output assembly
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Piece {
    Src { from: u64, len: u64 },
    Donor { from: u64, len: u64 },
    Bytes(Arc<Vec<u8>>),
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Piece::Src { len, .. } | Piece::Donor { len, .. } => *len,
            Piece::Bytes(b) => b.len() as u64,
        }
    }

    fn bytes(b: &[u8]) -> Piece {
        Piece::Bytes(Arc::new(b.to_vec()))
    }
}

/// A virtual file made of pieces of the damaged file, a donor and new
/// bytes. It is a [`Source`], so the repaired result can be validated
/// before anything is written.
struct Splice {
    pieces: Vec<(u64, Piece)>,
    size: u64,
    src: SharedSource,
    donor: Option<SharedSource>,
}

impl Splice {
    fn new(pieces: Vec<Piece>, src: SharedSource, donor: Option<SharedSource>) -> Self {
        let mut at = 0u64;
        let mut v = Vec::with_capacity(pieces.len());
        for p in pieces {
            let l = p.len();
            if l == 0 {
                continue;
            }
            v.push((at, p));
            at += l;
        }
        Splice {
            pieces: v,
            size: at,
            src,
            donor,
        }
    }
}

impl Source for Splice {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        if offset >= self.size {
            return Ok(0);
        }
        let want = ((self.size - offset).min(buf.len() as u64)) as usize;
        let mut done = 0usize;
        let mut i = self
            .pieces
            .partition_point(|(start, p)| start + p.len() <= offset);
        while done < want && i < self.pieces.len() {
            let (start, piece) = &self.pieces[i];
            let pos = offset + done as u64;
            let within = pos - start;
            let n = ((piece.len() - within) as usize).min(want - done);
            let dst = &mut buf[done..done + n];
            match piece {
                Piece::Bytes(b) => dst.copy_from_slice(&b[within as usize..within as usize + n]),
                Piece::Src { from, .. } | Piece::Donor { from, .. } => {
                    let s = match piece {
                        Piece::Src { .. } => &self.src,
                        _ => self.donor.as_ref().unwrap_or(&self.src),
                    };
                    s.read_exact_at(from + within, dst).map_err(|mut e| {
                        e.offset = e.offset.saturating_sub(from + within) + pos;
                        e
                    })?;
                }
            }
            done += n;
            i += 1;
        }
        Ok(done)
    }

    fn describe(&self) -> String {
        format!("repaired copy of {}", self.src.describe())
    }
}

fn write_splice(s: &Splice, out: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < s.size {
        let n = ((s.size - pos) as usize).min(buf.len());
        s.read_exact_at(pos, &mut buf[..n])?;
        out.write_all(&buf[..n])?;
        pos += n as u64;
    }
    out.flush()?;
    Ok(pos)
}

struct Built {
    pieces: Vec<Piece>,
    donor: Option<SharedSource>,
    applied: Vec<String>,
    notes: Vec<String>,
}

enum Build {
    Ok(Built),
    Unrepairable(Vec<String>),
}

fn unrepairable(notes: Vec<String>) -> RepairOutcome {
    RepairOutcome {
        result: RepairResult::Unrepairable,
        applied: Vec::new(),
        output_len: 0,
        validation: None,
        notes,
    }
}

/// Apply `strategies` to `src`, re-validate the result and write it to
/// `out` (nothing is written when the result does not decode).
pub fn apply(
    src: &SharedSource,
    out: &mut dyn Write,
    strategies: &[Strategy],
    ctx: &RepairContext,
) -> Result<RepairOutcome> {
    let _ = ctx;
    let built = match build(src, strategies)? {
        Build::Ok(b) => b,
        Build::Unrepairable(notes) => return Ok(unrepairable(notes)),
    };
    let splice = Splice::new(built.pieces, src.clone(), built.donor);
    let detail = validate_source(&splice, true)?;
    let v = detail.validation;
    // Partially decodable: the reference decoder shows something, or the
    // analyzer decoded part of the image (zune-jpeg 0.4 cannot decode some
    // valid files, e.g. progressive with restart markers).
    let decodes = detail
        .zune
        .as_ref()
        .is_some_and(|z| z.strict_ok || z.lenient_ok)
        || v.intact_fraction.unwrap_or(0.0) > 0.0;
    let result = match v.status {
        ValidationStatus::Valid if strategies.is_empty() => RepairResult::NoRepairNeeded,
        ValidationStatus::Valid => RepairResult::Repaired,
        ValidationStatus::Damaged if decodes => RepairResult::PartiallyRepaired,
        _ => RepairResult::Unrepairable,
    };
    let mut notes = built.notes;
    if result == RepairResult::Unrepairable {
        notes.push("the repaired candidate still does not decode; nothing was written".into());
        return Ok(RepairOutcome {
            result,
            applied: built.applied,
            output_len: 0,
            validation: Some(v),
            notes,
        });
    }
    if result == RepairResult::PartiallyRepaired {
        if let Some(f) = v.intact_fraction {
            notes.push(format!(
                "{:.0}% of the image decodes; the rest shows as grey",
                f * 100.0
            ));
        }
    }
    let n = write_splice(&splice, out)?;
    Ok(RepairOutcome {
        result,
        applied: built.applied,
        output_len: n,
        validation: Some(v),
        notes,
    })
}

fn has(strategies: &[Strategy], name: &str) -> bool {
    strategies.iter().any(|s| s.name() == name)
}

fn build(src: &SharedSource, strategies: &[Strategy]) -> Result<Build> {
    if let Some(Strategy::DonorHeader { donor, scan_offset }) = strategies
        .iter()
        .find(|s| matches!(s, Strategy::DonorHeader { .. }))
    {
        return build_donor(src, donor, *scan_offset);
    }
    let size = src.size();
    let Some(soi) = find_soi(&**src) else {
        return Ok(Build::Unrepairable(vec![
            "no JPEG SOI marker found; a donor header is needed".into(),
        ]));
    };
    let mut cur = Cursor::new(&**src, soi, size - soi, SCAN_CHUNK);
    let h = markers::walk(&mut cur)?;
    if !h.reached_scan() {
        return Ok(Build::Unrepairable(vec![
            "the header does not reach the first scan; a donor header is needed".into(),
        ]));
    }
    let a = analyze_after_headers(&mut cur, &h, &AnalyzeOptions::default());
    let mut applied = Vec::new();
    let mut notes = Vec::new();
    let mut pieces = Vec::new();
    let sos_off = h.sos_offset.expect("reached_scan implies SOS");
    let sdo = h.scan_data_offset.expect("reached_scan implies scan data");

    // Header.
    if has(strategies, "structural_fix") {
        if soi > 0 {
            applied.push(format!("dropped {soi} bytes before the SOI marker"));
        }
        pieces.push(Piece::bytes(&[0xFF, 0xD8]));
        let n = h.segments.len().saturating_sub(1);
        for seg in &h.segments[..n] {
            if seg.len > 0xFFFF || seg.len < 2 {
                notes.push(format!(
                    "dropped {} segment at offset {} (length {} cannot be stored)",
                    markers::marker_name(seg.marker),
                    seg.offset,
                    seg.len
                ));
                continue;
            }
            let l = seg.len as u16;
            pieces.push(Piece::bytes(&[0xFF, seg.marker, (l >> 8) as u8, l as u8]));
            pieces.push(Piece::Src {
                from: soi + seg.data_offset,
                len: seg.payload_len(),
            });
        }
        if !h.issues.is_empty() {
            let what: Vec<String> = h.issues.iter().map(|i| i.describe()).collect();
            applied.push(format!(
                "rebuilt the header segment chain ({})",
                what.join("; ")
            ));
        }
    } else {
        pieces.push(Piece::Src {
            from: soi,
            len: sos_off,
        });
    }
    if has(strategies, "insert_standard_huffman_tables") {
        pieces.push(Piece::Bytes(Arc::new(standard_dht())));
        applied.push("inserted the standard Huffman tables (ITU-T T.81 Annex K) before SOS".into());
    }
    pieces.push(Piece::Src {
        from: soi + sos_off,
        len: sdo - sos_off,
    });

    // Scan data.
    let mut cut = false;
    let mut patch: Option<Vec<u8>> = None;
    let mut data_end = match a.eoi_offset {
        Some(e) => e,
        None if has(strategies, "append_eoi") => a.data_end,
        None => size - soi,
    };
    if has(strategies, "truncate_at_last_good_restart") {
        match a.last_good_restart {
            Some((off, mcu)) => {
                data_end = off;
                cut = true;
                applied.push(format!(
                    "cut the scan at the restart marker before MCU {mcu} (offset {})",
                    soi + off
                ));
            }
            None => notes.push(
                "no clean restart marker before the damage; cutting after the last good MCU".into(),
            ),
        }
    }
    if !cut
        && (has(strategies, "truncate_at_last_good_mcu_row")
            || has(strategies, "truncate_at_last_good_restart"))
    {
        if a.progressive {
            if let Some(e) = a.last_complete_scan_end {
                let n = a.scans.iter().filter(|s| s.complete).count();
                data_end = e;
                cut = true;
                applied.push(format!("kept the {n} complete progressive scan(s)"));
            }
        } else if let Some((off, mcu)) = a.last_good_row_restart {
            data_end = off;
            cut = true;
            applied.push(format!(
                "cut the scan at the restart marker before MCU row {} (offset {})",
                mcu / a.mcus_per_row.max(1) as u64,
                soi + off
            ));
        } else if let Some(p) = a.last_good_mcu {
            data_end = p.byte_offset;
            if p.bit > 0 {
                let b = p.byte_value | ((1u16 << (8 - p.bit)) - 1) as u8;
                patch = Some(if b == 0xFF { vec![0xFF, 0x00] } else { vec![b] });
            }
            cut = true;
            applied.push(format!(
                "cut the scan after MCU {} (offset {}); the rest decodes as grey",
                p.mcu_index,
                soi + p.byte_offset
            ));
        }
        if !cut {
            notes.push("nothing decodable to keep before the damage".into());
        }
    }
    let data_end = data_end.max(sdo);
    let ends_at_eoi = !cut && a.eoi_offset.is_some();
    let mut data_len = data_end - sdo;
    if !ends_at_eoi && patch.is_none() && data_len > 0 && cur.byte(data_end - 1) == Some(0xFF) {
        data_len -= 1;
    }
    pieces.push(Piece::Src {
        from: soi + sdo,
        len: data_len,
    });
    if let Some(p) = patch {
        pieces.push(Piece::bytes(&p));
    }

    // Tail.
    if ends_at_eoi {
        let eoi = a.eoi_offset.expect("checked above");
        let file_end = size - soi;
        let tail_end = if has(strategies, "strip_trailing_garbage") {
            let after = extend_after_eoi(&mut cur, &h, eoi + 2);
            if after.end < file_end {
                applied.push(format!(
                    "removed {} bytes of unrecognised data after the image",
                    file_end - after.end
                ));
            }
            after.end
        } else {
            file_end
        };
        pieces.push(Piece::Src {
            from: soi + eoi,
            len: tail_end.saturating_sub(eoi),
        });
    } else if cut || has(strategies, "append_eoi") {
        pieces.push(Piece::bytes(&[0xFF, 0xD9]));
        applied.push("appended the EOI marker".into());
        if !cut && a.data_end + 2 < size - soi && a.eoi_offset.is_none() {
            notes.push(format!(
                "dropped {} bytes that do not belong to the image",
                (size - soi).saturating_sub(a.data_end)
            ));
        }
    }
    Ok(Build::Ok(Built {
        pieces,
        donor: None,
        applied,
        notes,
    }))
}

/// Search the start of a damaged file for an intact SOF segment.
fn scan_for_sof(src: &dyn Source) -> Option<FrameInfo> {
    let head = src
        .read_vec(0, REMNANT_SEARCH.min(src.size()) as usize)
        .ok()?;
    let mut i = 0usize;
    while let Some(k) = memchr::memchr(0xFF, head.get(i..)?) {
        let p = i + k;
        i = p + 1;
        let Some(&m) = head.get(p + 1) else {
            break;
        };
        if !matches!(m, 0xC0..=0xC2) {
            continue;
        }
        let Some(len) = head
            .get(p + 2..p + 4)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
        else {
            break;
        };
        let Some(payload) = head.get(p + 4..p + 2 + len as usize) else {
            continue;
        };
        if let Ok(f) = parse_sof(m, p as u64, payload) {
            if len as usize == 8 + 3 * f.components.len() && f.check().is_ok() && f.height > 0 {
                return Some(f);
            }
        }
    }
    None
}

/// Candidate offsets where the damaged file's scan data may start.
fn scan_candidates(src: &dyn Source, victim: Option<(u64, &Headers)>, donor_abs: u64) -> Vec<u64> {
    let mut c = Vec::new();
    if let Some((soi, h)) = victim {
        if let (true, Some(d)) = (h.reached_scan(), h.scan_data_offset) {
            c.push(soi + d);
        }
    }
    let head = src
        .read_vec(0, REMNANT_SEARCH.min(src.size()) as usize)
        .unwrap_or_default();
    // Surviving SOS segments.
    let mut found = 0;
    for (i, w) in head.windows(5).enumerate() {
        if w[0] == 0xFF && w[1] == 0xDA {
            let len = u16::from_be_bytes([w[2], w[3]]) as usize;
            let ns = w[4] as usize;
            if (1..=4).contains(&ns) && len == 6 + 2 * ns {
                c.push((i + 2 + len) as u64);
                found += 1;
                if found >= 8 {
                    break;
                }
            }
        }
    }
    c.push(donor_abs);
    // End of a leading run of zeros or 0xFF (overwritten header).
    if let Some(&first) = head.first() {
        if first == 0x00 || first == 0xFF {
            if let Some(z) = head.iter().position(|&b| b != first) {
                if z >= 16 {
                    c.push(z as u64);
                    let lo = z.saturating_sub(16);
                    let hi = (z + 4096).min(head.len());
                    if let Some(w) = head.get(lo..hi) {
                        for (k, t) in w.windows(3).enumerate() {
                            if t == [0x00, 0x3F, 0x00] {
                                c.push((lo + k + 3) as u64);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    c.retain(|x| *x < src.size() && seen.insert(*x));
    c.truncate(48);
    c
}

fn score(a: &ScanAnalysis) -> u64 {
    match a.first_error {
        None => a.mcus_decoded + 1,
        Some(e) if e.scan == 0 => e.mcu_index,
        Some(_) => a.scans.first().map(|s| s.mcus_decoded).unwrap_or(0),
    }
}

fn build_donor(
    src: &SharedSource,
    donor_path: &PathBuf,
    scan_offset: Option<u64>,
) -> Result<Build> {
    let donor: SharedSource = match FileSource::open(donor_path) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            return Ok(Build::Unrepairable(vec![format!(
                "cannot open donor {}: {e}",
                donor_path.display()
            )]))
        }
    };
    let Some(dsoi) = find_soi(&*donor) else {
        return Ok(Build::Unrepairable(vec![format!(
            "donor {} is not a JPEG",
            donor_path.display()
        )]));
    };
    let dh = walk_headers(&*donor, dsoi, donor.size() - dsoi)?;
    if !dh.reached_scan() || !dh.missing_quant_tables().is_empty() {
        return Ok(Build::Unrepairable(vec![format!(
            "donor {} has no complete header",
            donor_path.display()
        )]));
    }
    let dsof = dh.sof.clone().expect("reached_scan implies SOF");
    let dlen = dh.scan_data_offset.expect("reached_scan implies scan data");
    let size = src.size();
    let vsoi = find_soi(&**src);
    let vh = vsoi.and_then(|s| walk_headers(&**src, s, size - s).ok());
    let victim_sof = vh
        .as_ref()
        .and_then(|h| h.sof.clone())
        .or_else(|| scan_for_sof(&**src));
    let mut notes = Vec::new();
    match &victim_sof {
        Some(vs) if !same_geometry(vs, &dsof) => {
            return Ok(Build::Unrepairable(vec![format!(
                "donor {} is {} but the damaged file is {}: a donor must have the same \
                 dimensions, components and sampling",
                donor_path.display(),
                describe_frame(&dsof),
                describe_frame(vs)
            )]));
        }
        Some(_) => {}
        None => notes.push(
            "the damaged file has no readable frame header; dimensions are taken from the donor"
                .into(),
        ),
    }
    let header = Piece::Donor {
        from: dsoi,
        len: dlen,
    };
    let candidates = match scan_offset {
        Some(o) => vec![o],
        None => scan_candidates(&**src, vsoi.zip(vh.as_ref()), dsoi + dlen),
    };
    let opts = AnalyzeOptions {
        max_mcus: Some(SCORE_MCUS),
        ..Default::default()
    };
    let mut best: Option<(u64, u64)> = None;
    for c in candidates {
        if c >= size {
            continue;
        }
        let sp = Splice::new(
            vec![
                header.clone(),
                Piece::Src {
                    from: c,
                    len: size - c,
                },
            ],
            src.clone(),
            Some(donor.clone()),
        );
        let Ok((_, a)) = analyze_source(&sp, 0, sp.size(), &opts) else {
            continue;
        };
        let s = score(&a);
        log::debug!("donor candidate scan start {c}: score {s}");
        if best.is_none_or(|(bs, _)| s > bs) {
            best = Some((s, c));
        }
    }
    let Some((_, start)) = best.filter(|(s, _)| *s > 0) else {
        return Ok(Build::Unrepairable(vec![format!(
            "no candidate start of the scan data decodes with the header of donor {}",
            donor_path.display()
        )]));
    };
    let sp = Splice::new(
        vec![
            header.clone(),
            Piece::Src {
                from: start,
                len: size - start,
            },
        ],
        src.clone(),
        Some(donor.clone()),
    );
    let mut cur = Cursor::new(&sp, 0, sp.size(), SCAN_CHUNK);
    let h2 = markers::walk(&mut cur)?;
    let a2 = analyze_after_headers(&mut cur, &h2, &AnalyzeOptions::default());
    if a2
        .first_error
        .is_some_and(|e| e.kind == ScanErrorKind::DataAfterEoi && e.scan == 0)
    {
        return Ok(Build::Unrepairable(vec![format!(
            "the damaged file's scan data continues past the {} MCUs of donor {}: the \
             donor has different dimensions or sampling",
            a2.scans.first().map(|s| s.mcus_total).unwrap_or(0),
            donor_path.display()
        )]));
    }
    let mut pieces = vec![header];
    let mut applied = vec![
        format!(
            "replaced the header (SOI..SOS) with the one from donor {}",
            donor_path.display()
        ),
        format!("took the scan data from offset {start}"),
    ];
    match a2.eoi_offset {
        Some(e) => {
            let end = start + (e + 2 - dlen);
            pieces.push(Piece::Src {
                from: start,
                len: end - start,
            });
            if end < size {
                notes.push(format!(
                    "{} bytes after the image's EOI were not kept",
                    size - end
                ));
            }
        }
        None => {
            let mut len = a2.data_end.saturating_sub(dlen);
            if len > 0 && cur.byte(dlen + len - 1) == Some(0xFF) {
                len -= 1;
            }
            pieces.push(Piece::Src { from: start, len });
            pieces.push(Piece::bytes(&[0xFF, 0xD9]));
            applied.push("appended the EOI marker".into());
        }
    }
    notes.push("colours may be off if the quantisation tables of the donor differ".into());
    if let Some(vq) = vh.as_ref().filter(|h| !h.dqt.is_empty()) {
        let same = vq
            .dqt
            .iter()
            .all(|q| dh.quant_table(q.id).is_some_and(|d| d.values == q.values));
        if !same {
            notes.push("the donor's quantisation tables differ from the damaged file's".into());
        }
    }
    if dh.exif.is_some() {
        notes.push("the Exif metadata now describes the donor shot".into());
    }
    Ok(Build::Ok(Built {
        pieces,
        donor: Some(donor),
        applied,
        notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::tests::tiny_jpeg;
    use diskrescue_core::source::MemSource;

    #[test]
    fn strategy_names_are_stable() {
        assert_eq!(Strategy::AppendEoi.name(), "append_eoi");
        assert_eq!(
            Strategy::DonorHeader {
                donor: "x".into(),
                scan_offset: None
            }
            .name(),
            "donor_header"
        );
    }

    #[test]
    fn splice_reads_across_pieces() {
        let src: SharedSource = Arc::new(MemSource::new((0..100u8).collect()));
        let s = Splice::new(
            vec![
                Piece::Src { from: 10, len: 5 },
                Piece::bytes(&[200, 201]),
                Piece::Src { from: 0, len: 3 },
            ],
            src,
            None,
        );
        assert_eq!(s.size(), 10);
        let v = s.read_vec(0, 100).unwrap();
        assert_eq!(v, vec![10, 11, 12, 13, 14, 200, 201, 0, 1, 2]);
        let v = s.read_vec(4, 3).unwrap();
        assert_eq!(v, vec![14, 200, 201]);
    }

    #[test]
    fn append_eoi_on_tiny_truncated_file() {
        let d = tiny_jpeg();
        let cut = d[..d.len() - 2].to_vec();
        let src: SharedSource = Arc::new(MemSource::new(cut));
        let mut out = Vec::new();
        let r = apply(
            &src,
            &mut out,
            &[Strategy::AppendEoi],
            &RepairContext::default(),
        )
        .unwrap();
        assert_ne!(r.result, RepairResult::Unrepairable, "{r:?}");
        assert!(out.ends_with(&[0xFF, 0xD9]));
    }
}
