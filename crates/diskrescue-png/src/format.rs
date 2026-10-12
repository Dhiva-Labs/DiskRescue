//! [`PngFormat`]: the [`FileFormat`] implementation (carving extent,
//! validation and repair entry point).

use std::collections::BTreeMap;
use std::io::Write;

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, RepairContext, RepairOutcome, RepairResult,
    Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::{Source, SourceExt};
use diskrescue_core::{Result, SharedSource};

use crate::chunks::{self, ChunkWalk, StopReason, SIGNATURE};
use crate::ihdr::Ihdr;
use crate::lenient::{self, LenientImage};
use crate::{meta, repair, sanity};

/// Smallest possible PNG: signature, 1x1 IHDR, a tiny IDAT and IEND.
pub const MIN_SIZE: u64 = 67;

/// Largest file the carver will measure.
pub const MAX_SIZE: u64 = 256 << 20;

/// Largest width or height accepted by [`FileFormat::probe`].
pub const MAX_PROBE_DIM: u32 = 65_535;

static SIGNATURES: [Signature; 1] = [Signature::at_start(&SIGNATURE)];

/// The PNG file format (W3C PNG 3rd edition / ISO 15948), including APNG and
/// Apple's CgBI variant.
#[derive(Debug, Clone, Copy, Default)]
pub struct PngFormat;

/// What the first bytes of a file say.
pub(crate) struct Head {
    pub ihdr: Ihdr,
    pub ihdr_crc_ok: bool,
}

/// Parse signature + (CgBI +) IHDR from the start of a candidate. The IHDR
/// fields must form a legal combination; the dimension cap is left to the
/// caller.
pub(crate) fn sniff(head: &[u8]) -> Option<Head> {
    use diskrescue_core::util::bytes::u32be;
    if head.get(..8)? != SIGNATURE {
        return None;
    }
    let mut pos = 8usize;
    let mut cgbi = false;
    for _ in 0..2 {
        let len = u32be(head, pos)? as usize;
        let kind = head.get(pos + 4..pos + 8)?;
        if kind == b"CgBI" && !cgbi && len <= 16 {
            cgbi = true;
            pos += 12 + len;
            continue;
        }
        if kind != b"IHDR" || len != Ihdr::LEN {
            return None;
        }
        let data = head.get(pos + 8..pos + 8 + len)?;
        let stored = u32be(head, pos + 8 + len)?;
        let ihdr = Ihdr::parse(data)?;
        ihdr.check().ok()?;
        return Some(Head {
            ihdr,
            ihdr_crc_ok: crate::io::chunk_crc(b"IHDR", data) == stored,
        });
    }
    None
}

/// Everything learned about a file in one pass.
pub(crate) struct Analysis {
    pub walk: ChunkWalk,
    /// Statistics-only decode; `None` when the IHDR is unusable.
    pub image: Option<LenientImage>,
    pub validation: Validation,
}

/// Walk and decode (statistics only) the PNG occupying all of `src`.
pub(crate) fn analyze_source(src: &dyn Source) -> Result<Analysis> {
    let walk = chunks::walk(src, 0, src.size())?;
    analyze_with_walk(src, walk)
}

/// Offsets of the chunks with implausible contents (see [`sanity`]).
fn find_malformed(src: &dyn Source, walk: &ChunkWalk) -> Vec<(u64, String)> {
    let ihdr = walk.ihdr.filter(|i| i.check().is_ok());
    let mut out = Vec::new();
    let mut examined = 0usize;
    for c in &walk.chunks {
        if !c.complete {
            continue;
        }
        if c.is(b"PLTE") {
            if !sanity::plte_len_ok(c.len, ihdr.as_ref()) {
                out.push((c.offset, format!("PLTE has {} bytes", c.len)));
            }
            continue;
        }
        if sanity::is_known_critical(&c.kind) {
            continue;
        }
        if c.is_critical() {
            out.push((c.offset, "unknown critical chunk".to_string()));
            continue;
        }
        if c.len > sanity::MAX_CHECK_LEN || examined >= 4096 {
            continue;
        }
        examined += 1;
        let Ok(data) = src.read_vec(walk.start + c.data_offset, c.len as usize) else {
            continue;
        };
        if data.len() != c.len as usize {
            continue;
        }
        if let Err(why) = sanity::check(&c.kind, &data, ihdr.as_ref()) {
            out.push((c.offset, why));
        }
    }
    out
}

pub(crate) fn analyze_with_walk(src: &dyn Source, mut walk: ChunkWalk) -> Result<Analysis> {
    let malformed = find_malformed(src, &walk);
    walk.malformed = malformed.iter().map(|(o, _)| *o).collect();
    let mut decode_problem: Option<String> = None;
    let image = match walk.ihdr {
        Some(ih) if ih.check().is_ok() => match lenient::analyze(src, &walk) {
            Ok(img) => Some(img),
            Err(e) if e.is_read_error() => return Err(e),
            Err(e) => {
                decode_problem = Some(e.to_string());
                None
            }
        },
        _ => None,
    };
    let validation = build_validation(src, &walk, image.as_ref(), decode_problem, &malformed);
    Ok(Analysis {
        walk,
        image,
        validation,
    })
}

fn summarize_kinds(kinds: impl Iterator<Item = String>) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for k in kinds {
        *counts.entry(k).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(k, n)| if n > 1 { format!("{k} x{n}") } else { k })
        .collect::<Vec<_>>()
        .join(", ")
}

fn build_validation(
    src: &dyn Source,
    walk: &ChunkWalk,
    image: Option<&LenientImage>,
    decode_problem: Option<String>,
    malformed: &[(u64, String)],
) -> Validation {
    if !walk.signature_ok {
        return Validation::invalid("not a PNG: missing signature");
    }
    if walk.chunks.is_empty() {
        return Validation::invalid("no chunks follow the PNG signature");
    }
    let mut problems: Vec<String> = Vec::new();

    let ihdr_usable = match walk.ihdr {
        None => {
            problems.push("no valid IHDR chunk at the start of the file".to_string());
            false
        }
        Some(ih) => match ih.check() {
            Ok(()) => true,
            Err(e) => {
                problems.push(format!("invalid IHDR: {e}"));
                false
            }
        },
    };
    if let Some(p) = decode_problem {
        problems.push(p);
    }

    let bad: Vec<_> = walk.chunks.iter().filter(|c| !c.crc_ok).collect();
    if !bad.is_empty() {
        problems.push(format!(
            "{} chunk(s) failed their CRC: {}",
            bad.len(),
            summarize_kinds(bad.iter().map(|c| c.kind_name()))
        ));
    }
    if !malformed.is_empty() {
        let list = malformed
            .iter()
            .filter_map(|(off, why)| {
                walk.chunks
                    .iter()
                    .find(|c| c.offset == *off)
                    .map(|c| format!("{} ({why})", c.kind_name()))
            })
            .take(5)
            .collect::<Vec<_>>()
            .join(", ");
        problems.push(format!("{} malformed chunk(s): {list}", malformed.len()));
    }
    if let Some(p) = walk.order_problem() {
        if ihdr_usable || walk.ihdr.is_some() {
            problems.push(p);
        }
    }
    if !walk.gaps.is_empty() {
        let skipped: u64 = walk.gaps.iter().map(|g| g.len()).sum();
        problems.push(format!(
            "{skipped} bytes of unparseable data between chunks"
        ));
    }
    if walk.chunks.iter().any(|c| c.length_repaired) {
        problems.push("a chunk length field is damaged".to_string());
    }
    match walk.stop {
        StopReason::Iend => {}
        StopReason::Damage => {
            if let Some((off, msg)) = &walk.first_bad {
                problems.push(format!("chunk structure breaks at offset {off}: {msg}"));
            }
        }
        _ => problems.push("the file ends without an IEND chunk".to_string()),
    }
    if walk.idat_ranges.is_empty() {
        problems.push("no IDAT chunk".to_string());
    }
    if let Some(ih) = walk.ihdr {
        if ih.color_type == 3 && walk.find(b"PLTE").is_none() {
            problems.push("palette image without a PLTE chunk".to_string());
        }
    }
    if let Some(img) = image {
        if let Some(e) = &img.inflate_error {
            problems.push(format!(
                "{e}; {} of {} rows recovered",
                img.rows_recovered, img.rows_total
            ));
            if img.crc_capped && img.rows_unverified > 0 {
                problems.push(format!(
                    "{} further rows decode but follow an image-data chunk that failed its CRC",
                    img.rows_unverified
                ));
            }
        } else if img.rows_recovered != img.rows_total {
            problems.push(format!(
                "only {} of {} rows decode",
                img.rows_recovered, img.rows_total
            ));
        }
    }

    let mut meta = meta::collect(src, walk);
    if walk.end < src.size() && walk.stop == StopReason::Iend {
        meta.extra
            .insert("trailing_bytes".into(), (src.size() - walk.end).to_string());
    }
    let intact = match image {
        Some(img) => img.intact_fraction(),
        None => 0.0,
    };
    let plan = repair::plan(walk, image, &RepairContext::default());
    let mut repairable_by: Vec<String> = plan.iter().map(|s| s.name().to_string()).collect();
    if !ihdr_usable {
        repairable_by.push("donor_ihdr".to_string());
    }
    repairable_by.dedup();

    let status = if problems.is_empty() {
        ValidationStatus::Valid
    } else {
        ValidationStatus::Damaged
    };
    Validation {
        status,
        intact_fraction: Some(if status == ValidationStatus::Valid {
            1.0
        } else {
            intact
        }),
        problems,
        meta,
        repairable_by: if status == ValidationStatus::Valid {
            Vec::new()
        } else {
            repairable_by
        },
    }
}

impl FileFormat for PngFormat {
    fn id(&self) -> &'static str {
        "png"
    }

    fn name(&self) -> &'static str {
        "PNG image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["png"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        MIN_SIZE
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    /// An `eXIf` chunk holds an Exif block (a little TIFF file) that may
    /// carry a JPEG thumbnail; neither is a file of its own (carving spec
    /// 2.3).
    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "tiff" | "jpeg")
    }

    fn probe(&self, head: &[u8]) -> bool {
        sniff(head).is_some_and(|h| h.ihdr.is_plausible(MAX_PROBE_DIM))
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let src: &dyn Source = src.as_ref();
        if start >= src.size() {
            return Ok(None);
        }
        let limit = limit.min(MAX_SIZE);
        let head_len = 64.min(limit) as usize;
        let head = src.read_vec(start, head_len)?;
        let Some(h) = sniff(&head) else {
            return Ok(None);
        };
        if !(h.ihdr.is_plausible(MAX_PROBE_DIM) || h.ihdr_crc_ok) {
            return Ok(None);
        }
        let walk = chunks::walk(src, start, limit)?;
        if !walk.signature_ok || walk.ihdr.is_none() {
            return Ok(None);
        }
        let meta = meta::collect(src, &walk);
        let has_idat = !walk.idat_ranges.is_empty();
        let mut notes: Vec<String> = walk.notes.clone();
        let len = walk.end.max(1);

        let mut m = match walk.stop {
            StopReason::Iend => {
                let anc_bad = walk
                    .chunks
                    .iter()
                    .filter(|c| !c.crc_ok && !c.is_critical())
                    .count();
                let repaired = walk.chunks.iter().any(|c| c.length_repaired);
                let mut conf = Confidence::HIGH;
                if !has_idat {
                    conf = Confidence::LOW;
                    notes.push("no IDAT chunk before IEND".to_string());
                } else if !walk.critical_crc_ok() || !walk.gaps.is_empty() || repaired {
                    conf = Confidence::MEDIUM;
                    if !walk.critical_crc_ok() {
                        notes.push("a critical chunk failed its CRC".to_string());
                    }
                } else if anc_bad > 0 {
                    conf = conf.lowered(5);
                    notes.push(format!("{anc_bad} ancillary chunk(s) failed their CRC"));
                }
                Measured::complete(len, conf)
            }
            StopReason::Limit => {
                let conf = if has_idat {
                    Confidence::MEDIUM
                } else {
                    Confidence::LOW
                };
                let mut m = Measured::capped(len, conf);
                m.notes.clear();
                notes.push("stopped at the read limit before IEND".to_string());
                m
            }
            StopReason::SourceEnd | StopReason::Damage | StopReason::Requested => {
                let conf = if has_idat {
                    Confidence::MEDIUM
                } else {
                    Confidence::LOW
                };
                let what = walk
                    .first_bad
                    .as_ref()
                    .map(|(_, m)| m.clone())
                    .unwrap_or_else(|| "no IEND chunk".to_string());
                let mut m = Measured::truncated(len, conf, walk.stop_offset.min(len), what);
                m.notes.clear();
                m
            }
        };
        m.meta = meta;
        m.notes = notes;
        debug_assert!(m.len <= limit || m.status == ExtentStatus::Complete);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let src: &dyn Source = src.as_ref();
        Ok(analyze_source(src)?.validation)
    }

    fn repair(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        let src: &dyn Source = src.as_ref();
        let analysis = analyze_source(src)?;
        if analysis.validation.status == ValidationStatus::Valid {
            let n = repair::copy_range(src, 0, src.size(), out)?;
            return Ok(RepairOutcome {
                result: RepairResult::NoRepairNeeded,
                applied: Vec::new(),
                output_len: n,
                validation: Some(analysis.validation),
                notes: Vec::new(),
            });
        }
        if analysis.validation.status == ValidationStatus::Invalid {
            return Ok(RepairOutcome {
                result: RepairResult::Unrepairable,
                applied: Vec::new(),
                output_len: 0,
                validation: Some(analysis.validation.clone()),
                notes: analysis.validation.problems.clone(),
            });
        }
        let strategies = repair::plan(&analysis.walk, analysis.image.as_ref(), ctx);
        if strategies.is_empty() {
            let mut notes = analysis.validation.problems.clone();
            notes.push("no applicable repair strategy".to_string());
            return Ok(RepairOutcome {
                result: RepairResult::Unrepairable,
                applied: Vec::new(),
                output_len: 0,
                validation: Some(analysis.validation),
                notes,
            });
        }
        repair::apply_analyzed(src, &analysis, out, &strategies, ctx)
    }

    fn can_repair(&self) -> bool {
        true
    }
}
