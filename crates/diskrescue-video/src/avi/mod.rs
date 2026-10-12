//! AVI (RIFF) with OpenDML continuation: carving, validation, metadata and
//! repair.
//!
//! # Structure
//!
//! ```text
//! RIFF 'AVI ' ( LIST hdrl ( avih, LIST strl ( strh strf [indx] )..., [LIST odml ( dmlh )] )
//!               [LIST INFO] [JUNK]
//!               LIST movi ( ##dc | ##db | ##wb | ##pc | ##tx | ix## | LIST rec )*
//!               [idx1] )
//! RIFF 'AVIX' ( LIST movi ( ... ) )*          (OpenDML, files above 1 GiB)
//! ```
//!
//! # Measuring
//!
//! When the RIFF size field, the `movi` list size and the chunks that follow
//! `movi` tile the file exactly, the size fields are trusted (after spot
//! checks of the first chunks and the first and last `idx1` entries) and the
//! extent is the sum of the RIFF chunks, `AVIX` continuations included. When
//! they do not (size 0 from a crashed recorder, a cut file, damage), the
//! walker follows `movi` chunk by chunk and stops at the first thing that is
//! not a chunk; the status is then `Truncated` with `err_offset` at that
//! point. Motion-JPEG frames inside `movi` are not listed as nested files;
//! [`AviFormat::may_contain`] tells the carver that JPEG data inside an AVI
//! belongs to it.
//!
//! # Repair
//!
//! [`AviFormat::repair_with`] implements [`AviStrategy`]: `RebuildIndex`,
//! `TruncateToLastChunk` and `FixSizes`. The generic `repair` picks
//! `RebuildIndex`. Keyframe flags for the new `idx1` come from the bitstream
//! for MPEG-4 part 2, MS-MPEG4 v3, H.264 and HEVC and are "all key" for
//! MJPEG, DV and raw video; for any other codec every frame is flagged as a
//! keyframe and a note says so. Limits: the header list (`hdrl`) must be
//! intact (no donor-based header rebuild), `idx1` cannot address data beyond
//! 4 GiB, and OpenDML super indexes are neutralised instead of rebuilt.

pub(crate) mod hdr;
mod repair;
mod scan;
mod validate;
mod walk;

use std::io::Write;

use diskrescue_core::format::{
    Measured, RepairContext, RepairOutcome, RepairResult, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::{Result, SharedSource};

use crate::io::{note_unreadable, Window};
use crate::meta::EXTENSION_KEY;

pub use repair::AviStrategy;

use scan::{build_meta, confidence, extent, scan_head};

const MAX_SIZE: u64 = 64 << 30;

/// Format handler for AVI files.
#[derive(Debug, Default, Clone, Copy)]
pub struct AviFormat;

static SIGNATURES: [Signature; 1] = [Signature::at_start(b"RIFF").with_also(b"AVI ", 8)];

impl AviFormat {
    /// Repair with an explicit strategy. See the module documentation.
    pub fn repair_with(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        strategy: AviStrategy,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        let before = validate::analyse(&**src);
        let Some(before) = before else {
            return Ok(unrepairable("not an AVI file (no RIFF AVI header)"));
        };
        if before.problems.is_empty() {
            // Already fine: output equals input.
            let mut plan = crate::seg::SegSource::new(src.clone());
            plan.push_src(0, src.size());
            let n = plan.len();
            plan.write_to(out)?;
            return Ok(RepairOutcome {
                result: RepairResult::NoRepairNeeded,
                applied: Vec::new(),
                output_len: n,
                validation: Some(validate::to_validation(before)),
                notes: Vec::new(),
            });
        }
        let plan = match repair::plan(src, strategy, ctx.aggressive) {
            Ok(p) => p,
            Err(repair::Unrepairable(why)) => return Ok(unrepairable(&why)),
        };
        let mut notes = plan.notes;
        let applied = plan.applied;
        let seg = std::sync::Arc::new(plan.out);
        let planned: SharedSource = seg.clone();
        let after = validate::analyse(&*planned);
        let Some(after) = after else {
            return Ok(unrepairable(
                "the repaired output did not parse as an AVI; nothing was written",
            ));
        };
        let result = if after.problems.is_empty() {
            RepairResult::Repaired
        } else {
            RepairResult::PartiallyRepaired
        };
        // Stream the planned output.
        let zero_filled = seg.write_to(out)?;
        if zero_filled > 0 {
            notes.push(format!(
                "{zero_filled} unreadable input bytes were replaced by zeros"
            ));
        }
        let validation = validate::to_validation(after);
        if result == RepairResult::PartiallyRepaired {
            notes.push("the repaired file still has problems; see validation".into());
        }
        Ok(RepairOutcome {
            result,
            applied,
            output_len: seg.len(),
            validation: Some(validation),
            notes,
        })
    }
}

fn unrepairable(why: &str) -> RepairOutcome {
    RepairOutcome {
        result: RepairResult::Unrepairable,
        applied: Vec::new(),
        output_len: 0,
        validation: None,
        notes: vec![why.to_string()],
    }
}

impl diskrescue_core::format::FileFormat for AviFormat {
    fn id(&self) -> &'static str {
        "avi"
    }

    fn name(&self) -> &'static str {
        "AVI video"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["avi"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        56
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        62
    }

    fn probe(&self, head: &[u8]) -> bool {
        if !scan::is_avi_header(head) {
            return false;
        }
        // Every AVI starts with LIST hdrl; accept a leading JUNK as well.
        match head.get(12..24) {
            Some(b) if &b[0..4] == b"LIST" => &b[8..12] == b"hdrl",
            Some(b) if &b[0..4] == b"JUNK" => true,
            Some(_) => false,
            None => true, // too short to tell
        }
    }

    fn may_contain(&self, inner: &str) -> bool {
        // Motion-JPEG frames.
        inner == "jpeg" || inner == "jpg"
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(head) = scan_head(&mut win) else {
            return Ok(None);
        };
        let e = extent(&mut win, &head);
        if e.len < 56 {
            return Ok(None);
        }
        let mut status = e.status;
        let mut err_offset = e.err_offset;
        let mut notes = e.notes.clone();
        if let Some(f) = win.fault() {
            if status != diskrescue_core::format::ExtentStatus::Complete {
                status = diskrescue_core::format::ExtentStatus::Truncated;
                err_offset = Some(f.offset.min(e.len.max(f.offset)));
            }
        }
        if e.len > win.avail() {
            return Ok(None);
        }
        let mut meta = build_meta(&head, e.avix, e.walked_video_frames, e.idx1.is_some());
        meta.extra.insert(EXTENSION_KEY.into(), "avi".into());
        let conf = confidence(&head, &e);
        let mut m = Measured {
            len: e.len,
            status,
            confidence: conf,
            err_offset,
            meta,
            nested: Vec::new(),
            notes: std::mem::take(&mut notes),
        };
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        Ok(match validate::analyse(&**src) {
            Some(a) => validate::to_validation(a),
            None => Validation::invalid("not an AVI file (no RIFF AVI header with hdrl and movi)"),
        })
    }

    fn repair(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        self.repair_with(src, out, AviStrategy::RebuildIndex, ctx)
    }

    fn can_repair(&self) -> bool {
        true
    }
}

#[allow(dead_code)]
fn status_name(s: ValidationStatus) -> &'static str {
    match s {
        ValidationStatus::Valid => "valid",
        ValidationStatus::Damaged => "damaged",
        ValidationStatus::Invalid => "invalid",
    }
}
