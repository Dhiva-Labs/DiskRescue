//! PNG repair.
//!
//! [`plan`] looks at a chunk walk and (optionally) the lenient decode of the
//! image data and proposes [`Strategy`] values; [`apply`] carries them out
//! into a writer and re-validates the result.
//!
//! | Strategy | What it does |
//! |---|---|
//! | [`Strategy::FixCrc`] | recompute the CRC of chunks whose data is believed intact: IDAT chunks when the lenient decode ran through the whole stream, IHDR/PLTE/tRNS and harmless ancillary chunks |
//! | [`Strategy::ReconstructIend`] | append the 12-byte IEND chunk |
//! | [`Strategy::TruncatedIdat`] | re-encode the image: the recovered rows are kept, the missing rows are filled with mid-grey (transparent for alpha types) and a valid non-interlaced PNG is written |
//! | [`Strategy::DonorIhdr`] | take IHDR (and PLTE for palette images) from a healthy file whose header makes the image data decode |
//! | [`Strategy::DropBadAncillary`] | drop ancillary chunks that fail their CRC and cannot be vouched for (colour-management chunks, damaged compressed text) |
//! | [`Strategy::ResyncChunks`] | drop unparseable garbage between valid chunks |
//!
//! The repaired file is built in a spool (memory up to 16 MiB, then a
//! temporary file), validated with the same code as
//! [`FileFormat::validate`](diskrescue_core::format::FileFormat::validate)
//! and only then copied to the caller's writer; a repair that does not
//! produce something better than the input writes nothing.
//!
//! # Limits
//!
//! Deflate damage is not repairable: everything after the first bad block is
//! lost, and [`Strategy::TruncatedIdat`] says so honestly in its notes
//! ("N of M rows recovered") and in the result
//! ([`RepairResult::PartiallyRepaired`]). Interlaced images are recovered per
//! Adam7 pass: the output is the last complete pass replicated to full size
//! with later, partial passes overlaid. CgBI files (Apple's non-standard
//! variant) are only repaired structurally.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use diskrescue_core::format::{RepairContext, RepairOutcome, RepairResult, ValidationStatus};
use diskrescue_core::source::{FileSource, ReadError, Source, SourceExt};
use diskrescue_core::{Error, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::chunks::{self, Chunk, ChunkWalk, WalkOptions};
use crate::encode::{
    chunk_bytes, fill_for, fill_row, write_chunk, write_signature, Fill, IdatWriter,
};
use crate::filter::filter_adaptive;
use crate::format::{analyze_source, Analysis};
use crate::ihdr::Ihdr;
use crate::io::{crc_update, BLOCK, CRC_INIT};
use crate::lenient::{self, LenientImage, LenientOptions};
use crate::sanity;

/// Largest buffer allocated to re-encode an interlaced image.
const MAX_INTERLACED_BUFFER: u64 = 128 << 20;

/// Most pixels of an interlaced image that will be re-encoded.
const MAX_INTERLACED_PIXELS: u64 = 64 << 20;

/// Re-encoding refuses images whose raw size exceeds this (a damaged header
/// can claim billions of rows; the filler for them would take minutes).
const MAX_REENCODE_RAW: u64 = 1 << 30;

/// Output kept in memory up to this size before spilling to a temp file.
const SPOOL_MEMORY: usize = 16 << 20;

/// Most donor files tried.
const MAX_DONORS: usize = 16;

/// One repair step. [`plan`] returns them in the order they are reported;
/// [`apply`] performs them in a fixed, safe order regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    /// Recompute CRCs of chunks whose data is believed intact.
    FixCrc,
    /// Append a missing IEND chunk.
    ReconstructIend,
    /// Re-encode the image from the recovered rows, filling the rest.
    /// `keep_rows` is the most rows to keep (`u32::MAX` keeps every
    /// recoverable row); it is advisory for interlaced images, which are
    /// recovered pass by pass.
    TruncatedIdat {
        /// Upper bound on the rows taken from the damaged image data.
        keep_rows: u32,
    },
    /// Take IHDR (and PLTE for palette images) from a donor file.
    DonorIhdr {
        /// Path of a healthy PNG from the same source.
        donor: PathBuf,
    },
    /// Drop ancillary chunks with a bad CRC that cannot be vouched for.
    DropBadAncillary,
    /// Drop garbage between valid chunks.
    ResyncChunks,
}

impl Strategy {
    /// Stable machine name, as listed in `Validation::repairable_by`.
    pub fn name(&self) -> &'static str {
        match self {
            Strategy::FixCrc => "fix_crc",
            Strategy::ReconstructIend => "reconstruct_iend",
            Strategy::TruncatedIdat { .. } => "truncated_idat",
            Strategy::DonorIhdr { .. } => "donor_ihdr",
            Strategy::DropBadAncillary => "drop_bad_ancillary",
            Strategy::ResyncChunks => "resync_chunks",
        }
    }
}

/// What to do with a chunk copied from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Keep,
    FixCrc,
    Drop,
}

/// Propose repairs for a walked file. `img` is the lenient decode of its
/// image data (statistics are enough; pixels are not needed).
pub fn plan(walk: &ChunkWalk, img: Option<&LenientImage>, ctx: &RepairContext) -> Vec<Strategy> {
    let mut v = Vec::new();
    if !walk.signature_ok || walk.chunks.is_empty() {
        return v;
    }
    let ihdr_ok = walk.ihdr.is_some_and(|i| i.check().is_ok());
    let first_ihdr = walk.chunks.iter().find(|c| c.is(b"IHDR"));
    let ihdr_crc_bad = first_ihdr.is_some_and(|c| !c.crc_ok);
    let hard_fail = img.is_some_and(|i| i.rows_recovered == 0 && i.inflate_error.is_some());
    let ihdr_suspect = !ihdr_ok || (ihdr_crc_bad && hard_fail);
    let donors: Vec<_> = ctx.donors.iter().take(MAX_DONORS).collect();
    if ihdr_suspect {
        for d in &donors {
            v.push(Strategy::DonorIhdr {
                donor: (*d).clone(),
            });
        }
    }

    // Can the image data be rescued (re-encoded) at all?
    let reencode = img.is_some_and(|i| !i.is_complete() && i.rows_recovered > 0);
    let donor_may_help = ihdr_suspect && !donors.is_empty();
    if !ihdr_ok && !donor_may_help {
        return Vec::new();
    }
    let hopeless =
        img.is_some_and(|i| i.rows_recovered == 0 && !i.is_complete()) && !donor_may_help;
    if hopeless {
        return Vec::new();
    }

    if !walk.gaps.is_empty()
        || walk.chunks.iter().any(|c| c.length_repaired)
        || walk.order_problem().is_some()
    {
        v.push(Strategy::ResyncChunks);
    }
    let malformed = |c: &Chunk| walk.malformed.contains(&c.offset);
    let droppable = |c: &Chunk| {
        c.complete
            && !sanity::is_known_critical(&c.kind)
            && (malformed(c) || (!c.crc_ok && !sanity::is_vouchable(&c.kind)))
    };
    if walk.chunks.iter().any(droppable) {
        v.push(Strategy::DropBadAncillary);
    }
    let fix_targets = walk
        .chunks
        .iter()
        .filter(|c| !c.crc_ok && c.complete && !droppable(c))
        .filter(|c| !(reencode && c.is(b"IDAT")))
        .count();
    if fix_targets > 0 {
        v.push(Strategy::FixCrc);
    }
    if reencode {
        v.push(Strategy::TruncatedIdat {
            keep_rows: img.map_or(0, |i| i.rows_recovered),
        });
    } else if donor_may_help {
        v.push(Strategy::TruncatedIdat {
            keep_rows: u32::MAX,
        });
    }
    if !walk.has_iend() && !reencode {
        v.push(Strategy::ReconstructIend);
    }
    v
}

/// Apply `strategies` to the PNG in `src`, writing the result to `out`.
///
/// Returns [`RepairResult::Unrepairable`] without writing anything when the
/// strategies do not lead to an improvement.
pub fn apply(
    src: &dyn Source,
    out: &mut dyn Write,
    strategies: &[Strategy],
    ctx: &RepairContext,
) -> Result<RepairOutcome> {
    let analysis = analyze_source(src)?;
    apply_analyzed(src, &analysis, out, strategies, ctx)
}

/// What a donor contributes.
struct DonorChoice {
    ihdr: Ihdr,
    plte: Option<Vec<u8>>,
    name: String,
}

enum Item {
    /// Copy a chunk of the input; `new_crc` recomputes its CRC.
    Copy { idx: usize, new_crc: bool },
    /// Bytes that are already a complete chunk.
    Raw(Vec<u8>),
    /// The re-encoded image data.
    NewIdat,
}

/// How much of the image the re-encoded data holds.
struct Reencode {
    kept: u32,
    total: u32,
    fraction: f32,
}

/// The strategies, flattened.
struct Flags {
    fix_crc: bool,
    drop_bad: bool,
    resync: bool,
    recon_iend: bool,
    trunc: Option<u32>,
}

impl Flags {
    fn of(strategies: &[Strategy]) -> Self {
        Flags {
            fix_crc: strategies.iter().any(|s| matches!(s, Strategy::FixCrc)),
            drop_bad: strategies
                .iter()
                .any(|s| matches!(s, Strategy::DropBadAncillary)),
            resync: strategies
                .iter()
                .any(|s| matches!(s, Strategy::ResyncChunks)),
            recon_iend: strategies
                .iter()
                .any(|s| matches!(s, Strategy::ReconstructIend)),
            trunc: strategies.iter().find_map(|s| match s {
                Strategy::TruncatedIdat { keep_rows } => Some(*keep_rows),
                _ => None,
            }),
        }
    }
}

/// The planned order and treatment of the output chunks.
struct Layout {
    items: Vec<Item>,
    fixed: usize,
    dropped: Vec<String>,
    reordered: bool,
    stopped_at_gap: bool,
}

struct LayoutInput<'a> {
    walk: &'a ChunkWalk,
    ihdr: Ihdr,
    fl: &'a Flags,
    reencode: bool,
    ihdr_replaced: bool,
    donor_plte: Option<&'a [u8]>,
}

/// Decide what happens to every chunk and put the survivors in the order the
/// specification requires: IHDR, the chunks that preceded the image data
/// (with the palette among them), the image data, the rest.
fn layout(li: &LayoutInput<'_>) -> Layout {
    let walk = li.walk;
    let fl = li.fl;
    let ihdr = li.ihdr;
    let first_idat = walk.chunks.iter().position(|c| c.is(b"IDAT"));
    let mut head: Vec<Item> = Vec::new();
    let mut pre: Vec<Item> = Vec::new();
    let mut idat: Vec<Item> = Vec::new();
    let mut post: Vec<Item> = Vec::new();
    let mut late_plte: Option<Item> = None;
    let mut have_plte = false;
    let mut have_ihdr = false;
    let mut out = Layout {
        items: Vec::new(),
        fixed: 0,
        dropped: Vec::new(),
        reordered: walk.order_problem().is_some(),
        stopped_at_gap: false,
    };

    for (idx, c) in walk.chunks.iter().enumerate() {
        if c.after_gap && !fl.resync {
            out.stopped_at_gap = true;
            break;
        }
        let after_idat = first_idat.is_some_and(|f| idx > f);
        match &c.kind {
            b"CgBI" => {
                if idx == 0 {
                    head.push(Item::Copy {
                        idx,
                        new_crc: false,
                    });
                }
            }
            b"IHDR" => {
                if have_ihdr {
                    continue;
                }
                have_ihdr = true;
                if li.ihdr_replaced || (li.reencode && ihdr.is_interlaced()) {
                    let mut h = ihdr;
                    if li.reencode {
                        h.interlace = 0;
                    }
                    head.push(Item::Raw(chunk_bytes(b"IHDR", &h.to_bytes())));
                } else {
                    let fix = !c.crc_ok && fl.fix_crc;
                    out.fixed += fix as usize;
                    head.push(Item::Copy { idx, new_crc: fix });
                }
            }
            b"IEND" => {}
            b"IDAT" => {
                if li.reencode {
                    continue;
                }
                if c.complete || c.len > 0 {
                    let fix = (!c.crc_ok || !c.complete) && fl.fix_crc;
                    out.fixed += (fix && c.complete) as usize;
                    idat.push(Item::Copy {
                        idx,
                        new_crc: fix || !c.complete,
                    });
                }
            }
            b"PLTE" => {
                if have_plte || !c.complete || !sanity::plte_len_ok(c.len, Some(&ihdr)) {
                    out.dropped.push(c.kind_name());
                    continue;
                }
                have_plte = true;
                let fix = !c.crc_ok && fl.fix_crc;
                out.fixed += fix as usize;
                let item = Item::Copy { idx, new_crc: fix };
                if after_idat {
                    late_plte = Some(item);
                } else {
                    pre.push(item);
                }
            }
            b"acTL" | b"fcTL" | b"fdAT" if li.reencode => out.dropped.push(c.kind_name()),
            _ => {
                if !c.complete {
                    continue;
                }
                let malformed = walk.malformed.contains(&c.offset);
                let vouch = sanity::is_vouchable(&c.kind);
                let decision = if c.crc_ok && !malformed {
                    Decision::Keep
                } else if malformed || !vouch {
                    if fl.drop_bad {
                        Decision::Drop
                    } else if !c.crc_ok && fl.fix_crc {
                        Decision::FixCrc
                    } else {
                        Decision::Keep
                    }
                } else if fl.fix_crc {
                    Decision::FixCrc
                } else {
                    Decision::Keep
                };
                match decision {
                    Decision::Drop => out.dropped.push(c.kind_name()),
                    d => {
                        let fix = d == Decision::FixCrc;
                        out.fixed += fix as usize;
                        let item = Item::Copy { idx, new_crc: fix };
                        if after_idat {
                            post.push(item);
                        } else {
                            pre.push(item);
                        }
                    }
                }
            }
        }
    }
    if !have_ihdr {
        head.push(Item::Raw(chunk_bytes(b"IHDR", &ihdr.to_bytes())));
        out.reordered = true;
    }
    if let Some(p) = late_plte {
        pre.push(p);
    }
    if !have_plte && ihdr.color_type == 3 {
        if let Some(p) = li.donor_plte {
            pre.push(Item::Raw(chunk_bytes(b"PLTE", p)));
        }
    }
    out.items.extend(head);
    out.items.extend(pre);
    if li.reencode {
        out.items.push(Item::NewIdat);
    } else {
        out.items.extend(idat);
    }
    out.items.extend(post);
    out
}

fn unrepairable(analysis: &Analysis, notes: Vec<String>) -> RepairOutcome {
    RepairOutcome {
        result: RepairResult::Unrepairable,
        applied: Vec::new(),
        output_len: 0,
        validation: Some(analysis.validation.clone()),
        notes,
    }
}

/// Errors that mean "this input cannot be handled" rather than "the world
/// is broken": reported as an unrepairable file, not propagated.
fn is_soft_error(e: &Error) -> bool {
    matches!(
        e,
        Error::Unsupported(_) | Error::Corrupt { .. } | Error::InvalidArgument(_)
    )
}

pub(crate) fn apply_analyzed(
    src: &dyn Source,
    analysis: &Analysis,
    out: &mut dyn Write,
    strategies: &[Strategy],
    ctx: &RepairContext,
) -> Result<RepairOutcome> {
    match apply_inner(src, analysis, out, strategies, ctx) {
        Err(e) if is_soft_error(&e) => Ok(unrepairable(analysis, vec![e.to_string()])),
        other => other,
    }
}

fn apply_inner(
    src: &dyn Source,
    analysis: &Analysis,
    out: &mut dyn Write,
    strategies: &[Strategy],
    ctx: &RepairContext,
) -> Result<RepairOutcome> {
    let walk = &analysis.walk;
    if !walk.signature_ok || walk.chunks.is_empty() {
        return Ok(unrepairable(analysis, vec!["not a PNG file".to_string()]));
    }
    let fl = Flags::of(strategies);
    let mut donors: Vec<&Path> = strategies
        .iter()
        .filter_map(|s| match s {
            Strategy::DonorIhdr { donor } => Some(donor.as_path()),
            _ => None,
        })
        .collect();
    donors.truncate(MAX_DONORS);

    let mut applied: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // ---- Which header describes the image data? -------------------------
    let own_ihdr = walk.ihdr.filter(|i| i.check().is_ok());
    let mut best_score = own_ihdr.as_ref().and_then(|ih| score(src, walk, ih));
    let mut chosen = own_ihdr;
    let mut donor_plte: Option<Vec<u8>> = None;
    let mut ihdr_replaced = false;
    for d in &donors {
        let Some(dc) = try_donor(src, walk, d) else {
            notes.push(format!(
                "donor {} does not match the image data",
                d.display()
            ));
            continue;
        };
        let s = score(src, walk, &dc.ihdr);
        if s.is_some() && (best_score.is_none() || s > best_score) {
            best_score = s;
            chosen = Some(dc.ihdr);
            donor_plte = dc.plte;
            ihdr_replaced = true;
            applied.push(format!("took IHDR from donor {}", dc.name));
        }
    }
    let Some(ihdr) = chosen else {
        return Ok(unrepairable(
            analysis,
            vec!["no usable IHDR and no donor header fits the image data".to_string()],
        ));
    };

    // Statistics under the chosen header.
    let stats = match (&analysis.image, ihdr_replaced) {
        (Some(i), false) if own_ihdr.is_some() => i.clone(),
        _ => lenient::analyze_with(src, walk, &ihdr)?,
    };
    let image_ok = stats.is_complete();
    let wants_reencode = fl.trunc.is_some() && !image_ok;
    let reencode = wants_reencode && !walk.cgbi;
    if wants_reencode && walk.cgbi {
        notes
            .push("Apple CgBI image data cannot be re-encoded; only structural fixes apply".into());
    }
    if reencode && stats.rows_recovered == 0 {
        return Ok(unrepairable(
            analysis,
            vec!["no image rows could be recovered".to_string()],
        ));
    }

    // The palette in effect: the file's own, else the donor's.
    let own_plte = walk
        .chunks
        .iter()
        .find(|c| c.is(b"PLTE") && c.complete && sanity::plte_len_ok(c.len, Some(&ihdr)));
    let plte_data: Option<Vec<u8>> = match own_plte {
        Some(c) => src
            .read_vec(walk.start + c.data_offset, c.len as usize)
            .ok(),
        None => donor_plte.clone(),
    };

    // ---- Decide what happens to every chunk -----------------------------
    let lay = layout(&LayoutInput {
        walk,
        ihdr,
        fl: &fl,
        reencode,
        ihdr_replaced,
        donor_plte: if own_plte.is_none() {
            donor_plte.as_deref()
        } else {
            None
        },
    });
    if lay.stopped_at_gap {
        notes.push("data after garbage between chunks was left out".to_string());
    }
    if own_plte.is_none() && ihdr.color_type == 3 && donor_plte.is_some() {
        applied.push("took PLTE from the donor".to_string());
    }
    if fl.resync && !walk.gaps.is_empty() {
        let n: u64 = walk.gaps.iter().map(|g| g.len()).sum();
        applied.push(format!("dropped {n} bytes of garbage between chunks"));
    }
    if fl.resync && lay.reordered && walk.order_problem().is_some() {
        applied.push("put the chunks back in the required order".to_string());
    }
    if walk.chunks.iter().any(|c| c.length_repaired) {
        applied.push("rewrote damaged chunk length fields".to_string());
    }
    if lay.fixed > 0 {
        applied.push(format!("recomputed the CRC of {} chunk(s)", lay.fixed));
    }
    if !lay.dropped.is_empty() {
        applied.push(format!(
            "dropped {} damaged or obsolete chunk(s): {}",
            lay.dropped.len(),
            lay.dropped.join(", ")
        ));
    }
    let write_iend = walk.has_iend() || fl.recon_iend || reencode;
    if !walk.has_iend() && write_iend {
        applied.push("appended IEND".to_string());
    }

    // ---- Write into the spool ------------------------------------------
    let fill = fill_for(&ihdr, plte_data.as_deref());
    let mut spool = Spool::new(ctx.scratch_dir.as_deref());
    let mut enc_info: Option<Reencode> = None;
    {
        let w: &mut dyn Write = &mut spool;
        write_signature(w)?;
        for it in &lay.items {
            match it {
                Item::Raw(b) => w.write_all(b)?,
                Item::Copy { idx, new_crc } => {
                    copy_chunk(src, walk, &walk.chunks[*idx], *new_crc, w)?;
                }
                Item::NewIdat => {
                    enc_info = Some(write_new_idat(
                        src,
                        walk,
                        &ihdr,
                        &stats,
                        fl.trunc.unwrap_or(u32::MAX),
                        &fill,
                        w,
                    )?);
                }
            }
        }
        if let Some(e) = &enc_info {
            // Record in the file how much of it is real, so a later check
            // does not take the filled picture for an intact one.
            let frac = if ihdr.is_interlaced() {
                e.fraction
            } else if e.total > 0 {
                e.kept as f32 / e.total as f32
            } else {
                1.0
            };
            if frac < 1.0 {
                let text = format!(
                    "{}\0{}{:.4}",
                    crate::format::REPAIR_MARKER_KEYWORD,
                    crate::format::REPAIR_MARKER_PREFIX,
                    frac
                );
                write_chunk(w, b"tEXt", text.as_bytes())?;
            }
        }
        if write_iend {
            write_chunk(w, b"IEND", &[])?;
        }
    }
    let spooled = spool.finish()?;
    let out_len = spooled.size();

    // ---- Re-validate and decide ----------------------------------------
    let check = analyze_source(&spooled)?;
    let orig_fraction = analysis.validation.intact_fraction.unwrap_or(0.0);
    let new_fraction = check.validation.intact_fraction.unwrap_or(0.0);
    let partial_content = enc_info
        .as_ref()
        .is_some_and(|e| e.kept < e.total || e.fraction < 1.0)
        || (reencode && stats.checksum_failed);
    if reencode && stats.checksum_failed {
        notes.push(
            "the image data failed its checksum: every row is present but some pixels may be wrong"
                .to_string(),
        );
    }
    if let Some(e) = &enc_info {
        if ihdr.is_interlaced() {
            applied.push(format!(
                "re-encoded the image from {:.0}% of its pixels (Adam7 passes); the rest is a blocky preview or {}",
                e.fraction * 100.0,
                fill.describe
            ));
            if partial_content {
                notes.push(format!(
                    "{:.0}% of the pixels were recovered; the rest is not real image data",
                    e.fraction * 100.0
                ));
            }
        } else if e.kept < e.total {
            applied.push(format!(
                "re-encoded the image: {} of {} rows recovered, the rest filled ({})",
                e.kept, e.total, fill.describe
            ));
            notes.push(format!(
                "{} of {} rows recovered; the bottom {} rows are not real image data",
                e.kept,
                e.total,
                e.total - e.kept
            ));
        } else {
            applied.push("re-encoded the image data (all rows recovered)".to_string());
        }
    }

    let result = match check.unmarked_status {
        ValidationStatus::Valid if partial_content => RepairResult::PartiallyRepaired,
        ValidationStatus::Valid => RepairResult::Repaired,
        ValidationStatus::Damaged if new_fraction > orig_fraction => {
            RepairResult::PartiallyRepaired
        }
        _ => RepairResult::Unrepairable,
    };
    if result == RepairResult::Unrepairable {
        notes.extend(check.validation.problems.iter().cloned());
        notes.push("the repaired output is not better than the input; nothing written".into());
        return Ok(RepairOutcome {
            result,
            applied,
            output_len: 0,
            validation: Some(check.validation),
            notes,
        });
    }
    copy_range(&spooled, 0, out_len, out)?;
    Ok(RepairOutcome {
        result,
        applied,
        output_len: out_len,
        validation: Some(check.validation),
        notes,
    })
}

/// How well a header explains the image data: (decoded cleanly, rows).
fn score(src: &dyn Source, walk: &ChunkWalk, ihdr: &Ihdr) -> Option<(bool, u32)> {
    let img = lenient::analyze_with(src, walk, ihdr).ok()?;
    let need = img.rows_total.min(8);
    if img.rows_recovered < need || img.rows_recovered == 0 {
        return None;
    }
    Some((img.inflate_error.is_none(), img.rows_recovered))
}

/// Read a donor's header chunks.
fn try_donor(src: &dyn Source, walk: &ChunkWalk, path: &Path) -> Option<DonorChoice> {
    let fs = FileSource::open(path).ok()?;
    let opts = WalkOptions {
        stop_at_idat: true,
        resync: false,
        ..Default::default()
    };
    let dw = chunks::walk_with(&fs, 0, fs.size(), &opts).ok()?;
    let ihdr = dw.ihdr.filter(|i| i.check().is_ok())?;
    let plte = dw
        .chunks
        .iter()
        .find(|c| c.is(b"PLTE") && c.complete && c.crc_ok && c.len <= 768)
        .and_then(|c| fs.read_vec(c.data_offset, c.len as usize).ok());
    score(src, walk, &ihdr)?;
    Some(DonorChoice {
        ihdr,
        plte,
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    })
}

/// Stream `len` bytes at absolute offset `off` of `src` into `out`.
pub(crate) fn copy_range(src: &dyn Source, off: u64, len: u64, out: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; BLOCK];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(BLOCK as u64) as usize;
        src.read_exact_at(off + done, &mut buf[..n])?;
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    Ok(done)
}

/// Copy one chunk of the input, optionally with a freshly computed CRC.
/// A chunk cut short is written with the bytes that exist and a new CRC.
fn copy_chunk(
    src: &dyn Source,
    walk: &ChunkWalk,
    c: &Chunk,
    new_crc: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let base = walk.start + c.offset;
    if c.complete && !new_crc && !c.length_repaired {
        copy_range(src, base, 12 + c.len as u64, out)?;
        return Ok(());
    }
    let avail = walk.end.saturating_sub(c.data_offset);
    let len = (c.len as u64).min(avail);
    let keep_stored_crc = c.complete && !new_crc;
    out.write_all(&(len as u32).to_be_bytes())?;
    out.write_all(&c.kind)?;
    let mut state = crc_update(CRC_INIT, &c.kind);
    let mut buf = vec![0u8; BLOCK];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(BLOCK as u64) as usize;
        src.read_exact_at(walk.start + c.data_offset + done, &mut buf[..n])?;
        state = crc_update(state, &buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
    }
    let crc = if keep_stored_crc {
        c.crc_stored
    } else {
        !state
    };
    out.write_all(&crc.to_be_bytes())?;
    Ok(())
}

/// Filters rows adaptively and feeds them to a zlib encoder.
struct RowWriter {
    bpp: usize,
    prev: Vec<u8>,
    scratch: Vec<u8>,
    best: Vec<u8>,
}

impl RowWriter {
    fn new(ihdr: &Ihdr) -> Self {
        RowWriter {
            bpp: ihdr.filter_bpp(),
            prev: vec![0u8; ihdr.row_bytes() as usize],
            scratch: Vec::new(),
            best: Vec::new(),
        }
    }

    fn push(&mut self, enc: &mut dyn Write, row: &[u8]) -> io::Result<()> {
        let ft = filter_adaptive(row, &self.prev, self.bpp, &mut self.scratch, &mut self.best);
        enc.write_all(&[ft])?;
        enc.write_all(&self.best)?;
        self.prev.copy_from_slice(row);
        Ok(())
    }
}

/// Re-encode the recovered image data as a fresh non-interlaced zlib stream
/// in IDAT chunks. Missing rows get the fill colour.
fn write_new_idat(
    src: &dyn Source,
    walk: &ChunkWalk,
    ihdr: &Ihdr,
    stats: &LenientImage,
    keep_rows: u32,
    fill: &Fill,
    out: &mut dyn Write,
) -> Result<Reencode> {
    let mut idat = IdatWriter::new(out);
    let mut enc = ZlibEncoder::new(&mut idat, Compression::new(6));
    let mut rw = RowWriter::new(ihdr);
    let result = if !ihdr.is_interlaced() {
        let total = ihdr.height;
        if ihdr.expected_inflated_len() > MAX_REENCODE_RAW {
            return Err(Error::unsupported(format!(
                "a {}x{} image is too large to re-encode",
                ihdr.width, ihdr.height
            )));
        }
        let mut kept = 0u32;
        let limit = keep_rows.min(total);
        {
            let mut sink = |ev: lenient::RowEvent<'_>| -> Result<()> {
                if kept < limit {
                    rw.push(&mut enc, ev.data)?;
                    kept += 1;
                }
                Ok(())
            };
            lenient::scan_rows(src, walk, ihdr, &mut sink)?;
        }
        let mut frow = vec![0u8; ihdr.row_bytes() as usize];
        fill_row(ihdr, fill, ihdr.width, &mut frow);
        let mut todo = (total - kept) as u64;
        if todo > 0 {
            // The first filler row is filtered against the last real row;
            // every further one is identical to its predecessor, so filter
            // type 2 (Up) turns it into zeros, written in big blocks.
            rw.push(&mut enc, &frow)?;
            todo -= 1;
            let stride = frow.len() + 1;
            let per_block = (BLOCK / stride).max(1) as u64;
            let mut block = Vec::with_capacity(per_block as usize * stride);
            for _ in 0..per_block {
                block.push(2u8);
                block.resize(block.len() + frow.len(), 0);
            }
            while todo > 0 {
                let n = todo.min(per_block);
                enc.write_all(&block[..n as usize * stride])?;
                todo -= n;
            }
        }
        Reencode {
            kept,
            total,
            fraction: if total == 0 {
                0.0
            } else {
                kept as f32 / total as f32
            },
        }
    } else {
        if ihdr.width as u64 * ihdr.height as u64 > MAX_INTERLACED_PIXELS {
            return Err(Error::unsupported(format!(
                "a {}x{} interlaced image is too large to re-encode",
                ihdr.width, ihdr.height
            )));
        }
        let mut img = lenient::decode_with_ihdr(
            src,
            walk,
            ihdr,
            &LenientOptions {
                keep_pixels: true,
                max_pixel_bytes: MAX_INTERLACED_BUFFER,
            },
        )?;
        lenient::upsample_interlaced(&mut img, &fill.pixel);
        let rb = img.row_bytes();
        for y in 0..ihdr.height as usize {
            if let Some(r) = img.pixels.get(y * rb..(y + 1) * rb) {
                rw.push(&mut enc, r)?;
            }
        }
        Reencode {
            kept: stats.rows_recovered.min(keep_rows),
            total: stats.rows_total,
            fraction: stats.intact_fraction(),
        }
    };
    enc.finish()?;
    idat.finish()?;
    Ok(result)
}

// ---- Spool ---------------------------------------------------------------

static SPOOL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Output buffer: memory first, a temporary file when it grows.
struct Spool {
    mem: Vec<u8>,
    file: Option<(BufWriter<File>, PathBuf)>,
    len: u64,
    dir: PathBuf,
}

impl Spool {
    fn new(dir: Option<&Path>) -> Self {
        Spool {
            mem: Vec::new(),
            file: None,
            len: 0,
            dir: dir
                .map(Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir),
        }
    }

    fn spill(&mut self) -> io::Result<()> {
        let n = SPOOL_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = self
            .dir
            .join(format!("diskrescue-png-{}-{}.tmp", std::process::id(), n));
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut w = BufWriter::new(f);
        w.write_all(&self.mem)?;
        self.mem = Vec::new();
        self.file = Some((w, path));
        Ok(())
    }

    fn finish(mut self) -> Result<SpoolSource> {
        match self.file.take() {
            None => Ok(SpoolSource::Mem(std::mem::take(&mut self.mem))),
            Some((w, path)) => {
                let f = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
                let size = self.len;
                Ok(SpoolSource::File {
                    src: FileSource::from_file(f, path.clone(), size),
                    _guard: TempGuard(path),
                })
            }
        }
    }
}

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() && self.mem.len() + buf.len() > SPOOL_MEMORY {
            self.spill()?;
        }
        match &mut self.file {
            Some((w, _)) => w.write_all(buf)?,
            None => self.mem.extend_from_slice(buf),
        }
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some((w, _)) => w.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if let Some((_, p)) = self.file.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

enum SpoolSource {
    Mem(Vec<u8>),
    File { src: FileSource, _guard: TempGuard },
}

impl Source for SpoolSource {
    fn size(&self) -> u64 {
        match self {
            SpoolSource::Mem(v) => v.len() as u64,
            SpoolSource::File { src, .. } => src.size(),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        match self {
            SpoolSource::Mem(v) => {
                if offset >= v.len() as u64 {
                    return Ok(0);
                }
                let s = offset as usize;
                let n = (v.len() - s).min(buf.len());
                buf[..n].copy_from_slice(&v[s..s + n]);
                Ok(n)
            }
            SpoolSource::File { src, .. } => src.read_at(offset, buf),
        }
    }

    fn describe(&self) -> String {
        "repair spool".to_string()
    }
}
