//! Repair strategies for MP4/MOV/3GP files.
//!
//! [`plan`] turns an [`Analysis`] into an ordered list of [`Strategy`]
//! values; [`apply`] runs them in order. Each strategy transforms the
//! current file into a new virtual file ([`crate::writer::SpliceSource`])
//! or reports that it does not apply, so strategies chain (relocate the
//! offsets, then clip the tables to the bytes present). The final virtual
//! file is re-validated through the normal validator before a single byte
//! is written to the output; an output that does not validate is never
//! written.
//!
//! | Strategy | When | What |
//! |---|---|---|
//! | `RebuildMoov` | moov missing, corrupt, zeroed or stale | scan the mdat with a reference clip ([`crate::rebuild`]) |
//! | `RelocateOffsets` | every chunk offset is off by a constant | shift `stco`/`co64` in place |
//! | `ClipTablesToFile` | moov fine, mdat truncated or holed | keep the samples that are present, rewrite tables and sizes |
//! | `FixMdatSize` | `mdat` size 0 / wrong / past EOF, samples all present | patch the size field |
//! | `StripTrailingGarbage` | bytes after the last box | cut them |
//! | `MoveMoovFirst` | on request only | fast-start rewrite (moov before mdat) |

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use diskrescue_core::format::{RepairContext, RepairOutcome, RepairResult, ValidationStatus};
use diskrescue_core::source::FileSource;
use diskrescue_core::{Error, Result, SharedSource, Source, SourceExt};

use crate::analysis::{analyze, Analysis};
use crate::boxes::{self, sample_table, BoxIter, SampleSizes};
use crate::rebuild::{learn, rebuild_to_splice, RebuildOptions};
use crate::writer::{assemble, MovieOut, OutSample, SpliceSource, TrackOut};

/// A repair step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// Rebuild the index from the media data using a healthy clip recorded
    /// by the same device with the same settings.
    RebuildMoov {
        /// Path of the reference clip.
        reference: PathBuf,
    },
    /// The index is intact but the media data is truncated: drop samples
    /// whose bytes are beyond the end of the file (or unreadable), fix every
    /// table, duration and the mdat size.
    ClipTablesToFile,
    /// All samples are present but the mdat size field is 0, too small or
    /// runs past the end of the file: patch it.
    FixMdatSize,
    /// Rewrite the file with the moov before the mdat (fast start).
    MoveMoovFirst,
    /// Remove bytes after the last box.
    StripTrailingGarbage,
    /// The chunk offsets are off by a constant (head of the file lost or
    /// boxes inserted): shift them so the first chunk starts at the mdat.
    RelocateOffsets,
}

impl Strategy {
    /// Stable name used in `Validation::repairable_by` and reports.
    pub fn name(&self) -> &'static str {
        match self {
            Strategy::RebuildMoov { .. } => "rebuild_moov",
            Strategy::ClipTablesToFile => "clip_tables_to_file",
            Strategy::FixMdatSize => "fix_mdat_size",
            Strategy::MoveMoovFirst => "move_moov_first",
            Strategy::StripTrailingGarbage => "strip_trailing_garbage",
            Strategy::RelocateOffsets => "relocate_offsets",
        }
    }
}

/// True when the mdat size field disagrees with the data.
pub fn mdat_size_wrong(a: &Analysis) -> bool {
    let Some(m) = &a.mdat else { return false };
    if a.mdat_real_end.is_some() || m.size == 0 {
        return true;
    }
    let claimed_end = m.end(a.size);
    if claimed_end > a.size {
        return true;
    }
    // Tables extend past the claimed end while bytes follow it.
    a.max_extent()
        .is_some_and(|e| e > claimed_end && e <= a.size)
}

/// Choose repair strategies for an analysed file, best first.
pub fn plan(a: &Analysis, ctx: &RepairContext) -> Vec<Strategy> {
    let mut v = Vec::new();
    if a.fragmented {
        return v;
    }
    let moov_bad = a.moov.is_none() || (a.moov_stale() && a.relocation_delta.is_none());
    if moov_bad {
        if a.mdat.is_some() || a.size > 0 {
            for d in &ctx.donors {
                v.push(Strategy::RebuildMoov {
                    reference: d.clone(),
                });
            }
        }
        return v;
    }
    if a.relocation_delta.is_some() {
        v.push(Strategy::RelocateOffsets);
    }
    let (declared, inside) = a.sample_counts();
    if inside < declared || a.relocation_delta.is_some() {
        v.push(Strategy::ClipTablesToFile);
    }
    if mdat_size_wrong(a) {
        v.push(Strategy::FixMdatSize);
    }
    if a.trailing_garbage.is_some() {
        v.push(Strategy::StripTrailingGarbage);
    }
    v
}

/// Rewrite every `stco`/`co64` entry of a complete `moov` box held in
/// memory (header included) through `f`. Fails when a new offset does not
/// fit the existing field.
pub fn patch_chunk_offsets(moov: &mut [u8], f: &dyn Fn(u64) -> Option<u64>) -> Result<u32> {
    let h = boxes::parse_header(moov, 0).ok_or_else(|| Error::corrupt("moov box", "bad header"))?;
    let base = h.full_header_len() as usize;
    let mut sites: Vec<(usize, bool, usize)> = Vec::new(); // (pos of entries, co64, count)
    let body = moov.get(base..).unwrap_or(&[]);
    for trak in BoxIter::new(body, base as u64).filter(|b| b.header.is(b"trak")) {
        let mdia =
            BoxIter::new(trak.payload, trak.header.payload_offset()).find(|b| b.header.is(b"mdia"));
        let Some(mdia) = mdia else { continue };
        let minf =
            BoxIter::new(mdia.payload, mdia.header.payload_offset()).find(|b| b.header.is(b"minf"));
        let Some(minf) = minf else { continue };
        let stbl =
            BoxIter::new(minf.payload, minf.header.payload_offset()).find(|b| b.header.is(b"stbl"));
        let Some(stbl) = stbl else { continue };
        for c in BoxIter::new(stbl.payload, stbl.header.payload_offset()) {
            let co64 = c.header.is(b"co64");
            if !(co64 || c.header.is(b"stco")) {
                continue;
            }
            let count = u32::from_be_bytes(
                c.payload
                    .get(4..8)
                    .and_then(|s| s.try_into().ok())
                    .unwrap_or([0; 4]),
            ) as usize;
            let entries = c.header.payload_offset() as usize + 8;
            let esz = if co64 { 8 } else { 4 };
            let avail = c.payload.len().saturating_sub(8) / esz;
            sites.push((entries, co64, count.min(avail)));
        }
    }
    let mut patched = 0u32;
    for (pos, co64, count) in sites {
        for i in 0..count {
            if co64 {
                let at = pos + i * 8;
                let old = u64::from_be_bytes(moov[at..at + 8].try_into().expect("8 bytes"));
                let new =
                    f(old).ok_or_else(|| Error::corrupt("co64 box", "offset out of range"))?;
                moov[at..at + 8].copy_from_slice(&new.to_be_bytes());
            } else {
                let at = pos + i * 4;
                let old = u32::from_be_bytes(moov[at..at + 4].try_into().expect("4 bytes")) as u64;
                let new = f(old)
                    .filter(|&n| n <= u32::MAX as u64)
                    .ok_or_else(|| Error::unsupported("chunk offset does not fit in stco"))?;
                moov[at..at + 4].copy_from_slice(&(new as u32).to_be_bytes());
            }
            patched += 1;
        }
    }
    Ok(patched)
}

fn read_box(src: &dyn Source, h: &boxes::BoxHeader) -> Result<Vec<u8>> {
    let size = src.size();
    let end = h.end(size);
    if end > size || end - h.offset > boxes::MAX_MOOV_SIZE {
        return Err(Error::corrupt("box", "truncated or too large"));
    }
    Ok(src.read_exact_vec(h.offset, (end - h.offset) as usize)?)
}

/// Result of one transformation.
struct Step {
    out: SpliceSource,
    note: String,
    lossy: bool,
}

fn fix_mdat_size(src: &SharedSource, a: &Analysis) -> Result<Option<Step>> {
    if !mdat_size_wrong(a) {
        return Ok(None);
    }
    let m = a.mdat.expect("mdat checked");
    let moov_after = a.moov_box.is_some_and(|mb| mb.offset > m.offset);
    let (real_end, file_end) = if let Some(e) = a.mdat_real_end {
        (e, a.structure_end.max(e))
    } else if let Some(ext) = a.max_extent() {
        let e = ext.min(a.size).max(m.payload_offset());
        let fe = if moov_after {
            a.moov_box.map(|mb| mb.end(a.size)).unwrap_or(e).max(e)
        } else {
            e
        };
        (e, fe)
    } else {
        return Ok(None);
    };
    if m.size != 0 && m.end(a.size) == real_end {
        return Ok(None);
    }
    let new_size = real_end - m.offset;
    let mut header = Vec::new();
    if m.header_len == 16 {
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(b"mdat");
        header.extend_from_slice(&new_size.to_be_bytes());
    } else {
        if new_size > u32::MAX as u64 {
            return Err(Error::unsupported(
                "mdat needs a 64-bit size but has a 32-bit header",
            ));
        }
        header.extend_from_slice(&(new_size as u32).to_be_bytes());
        header.extend_from_slice(b"mdat");
    }
    let mut sp = SpliceSource::new(src.clone(), "mdat size fixed");
    sp.push_src(0, m.offset);
    sp.push_mem(header);
    let after = m.offset + m.header_len as u64;
    sp.push_src(after, file_end.saturating_sub(after));
    Ok(Some(Step {
        out: sp,
        note: format!("set mdat size from {} to {new_size}", m.size),
        lossy: false,
    }))
}

fn strip_trailing(src: &SharedSource, a: &Analysis) -> Result<Option<Step>> {
    let Some(g) = a.trailing_garbage else {
        return Ok(None);
    };
    let real_end = a.max_extent().unwrap_or(0).max(a.structure_end);
    if g < real_end || g >= a.size || a.moov.is_none() {
        return Ok(None);
    }
    let mut sp = SpliceSource::new(src.clone(), "trailing data removed");
    sp.push_src(0, g);
    Ok(Some(Step {
        out: sp,
        note: format!("removed {} bytes after the last box", a.size - g),
        lossy: false,
    }))
}

fn relocate(src: &SharedSource, a: &Analysis) -> Result<Option<Step>> {
    let (Some(delta), Some(mb)) = (a.relocation_delta, a.moov_box) else {
        return Ok(None);
    };
    let mut moov = read_box(&**src, &mb)?;
    patch_chunk_offsets(&mut moov, &|o| {
        let n = o as i64 + delta;
        (n >= 0).then_some(n as u64)
    })?;
    let mut sp = SpliceSource::new(src.clone(), "offsets relocated");
    sp.push_src(0, mb.offset);
    let len = moov.len() as u64;
    sp.push_mem(moov);
    sp.push_src(mb.offset + len, a.size.saturating_sub(mb.offset + len));
    Ok(Some(Step {
        out: sp,
        note: format!("shifted every chunk offset by {delta} bytes"),
        lossy: false,
    }))
}

fn move_moov_first(src: &SharedSource, a: &Analysis) -> Result<Option<Step>> {
    let (Some(mb), Some(m)) = (a.moov_box, a.mdat) else {
        return Ok(None);
    };
    if mb.offset < m.offset {
        return Ok(None);
    }
    let ftyp_end = a
        .boxes
        .first()
        .filter(|b| b.header.is(b"ftyp"))
        .map(|b| b.end)
        .unwrap_or(0);
    let mut moov = read_box(&**src, &mb)?;
    let len = moov.len() as u64;
    let moov_at = mb.offset;
    patch_chunk_offsets(&mut moov, &|o| {
        if o >= ftyp_end && o < moov_at {
            Some(o + len)
        } else {
            Some(o)
        }
    })?;
    let end = a.structure_end.max(mb.offset + len).min(a.size);
    let mut sp = SpliceSource::new(src.clone(), "fast start");
    sp.push_src(0, ftyp_end);
    sp.push_mem(moov);
    sp.push_src(ftyp_end, moov_at - ftyp_end);
    sp.push_src(moov_at + len, end.saturating_sub(moov_at + len));
    Ok(Some(Step {
        out: sp,
        note: "moved moov before mdat".into(),
        lossy: false,
    }))
}

fn clip_tables(src: &SharedSource, a: &Analysis) -> Result<Option<Step>> {
    let Some(moov) = &a.moov else {
        return Ok(None);
    };
    let (declared, inside) = a.sample_counts();
    if inside >= declared {
        return Ok(None);
    }
    let mut tracks = Vec::new();
    let mut dropped_tracks = 0;
    for (t, check) in moov.tracks.iter().zip(&a.tracks) {
        let samples = sample_table(t)?;
        let keep = check.inside as usize;
        if keep == 0 {
            dropped_tracks += 1;
            continue;
        }
        let out: Vec<OutSample> = samples[..keep.min(samples.len())]
            .iter()
            .map(|s| OutSample {
                src_offset: s.offset,
                size: s.size,
                duration: s.duration,
                cts_offset: s.cts_offset,
                keyframe: s.keyframe,
                count: 1,
                suffix: None,
            })
            .collect();
        let constant_size = match t.stsz {
            SampleSizes::Constant { size, .. } => Some(size),
            _ => None,
        };
        let mut template = t.clone();
        if t.stss.is_none() {
            template.stss = None;
        }
        tracks.push(TrackOut {
            template,
            track_id: t.tkhd.track_id,
            samples: out,
            elst: t.elst.clone(),
            fit_elst: true,
            constant_size,
        });
    }
    if tracks.is_empty() {
        return Err(Error::corrupt(
            "mdat",
            "no complete sample left in the file",
        ));
    }
    let ftyp = match a.boxes.first() {
        Some(b) if b.header.is(b"ftyp") && !b.truncated => read_box(&**src, &b.header)?,
        _ => Vec::new(),
    };
    let movie = MovieOut {
        ftyp,
        mvhd: moov.mvhd.clone(),
        tracks,
        user_boxes: moov.user_boxes.clone(),
        max_chunk_samples: 1024,
    };
    let out = assemble(&movie, src.clone())?;
    let mut note = format!(
        "kept {inside} of {declared} samples whose bytes are present; tables, durations and mdat size rewritten"
    );
    if dropped_tracks > 0 {
        note.push_str(&format!("; {dropped_tracks} track(s) without data dropped"));
    }
    Ok(Some(Step {
        out,
        note,
        lossy: true,
    }))
}

fn rebuild_step(src: &SharedSource, reference: &PathBuf) -> Result<Option<Step>> {
    let r = FileSource::open(reference)?;
    let rs: SharedSource = Arc::new(r);
    let reference_t = learn(&rs)?;
    let (out, report) = rebuild_to_splice(src, &reference_t, &RebuildOptions::default())?;
    let mut note = format!(
        "rebuilt moov from reference {}: {} video samples ({} keyframes), {} audio samples, {} ms",
        reference.display(),
        report.video_samples,
        report.keyframes,
        report.audio_samples,
        report.duration_ms
    );
    if report.bytes_unclassified > 0 {
        note.push_str(&format!(
            "; {} bytes in {} range(s) could not be classified and were dropped",
            report.bytes_unclassified,
            report.unclassified_ranges.len()
        ));
    }
    for n in &report.notes {
        note.push_str("; ");
        note.push_str(n);
    }
    Ok(Some(Step {
        out,
        note,
        lossy: report.bytes_unclassified > 0 || report.stopped_at.is_some(),
    }))
}

/// Run `strategies` in order and return the final virtual file, the steps
/// applied, notes, and whether any media data was dropped.
pub fn apply_to_source(
    src: &SharedSource,
    strategies: &[Strategy],
) -> Result<(SharedSource, Vec<String>, Vec<String>, bool)> {
    let mut cur: SharedSource = src.clone();
    let mut applied = Vec::new();
    let mut notes = Vec::new();
    let mut lossy = false;
    let mut rebuilt = false;
    for s in strategies {
        if rebuilt && matches!(s, Strategy::RebuildMoov { .. }) {
            continue;
        }
        let a = analyze(&*cur)?;
        let step = match s {
            Strategy::RebuildMoov { reference } => {
                if a.moov_usable() && a.sample_counts().1 > 0 {
                    Ok(None)
                } else {
                    rebuild_step(&cur, reference)
                }
            }
            Strategy::ClipTablesToFile => clip_tables(&cur, &a),
            Strategy::FixMdatSize => fix_mdat_size(&cur, &a),
            Strategy::MoveMoovFirst => move_moov_first(&cur, &a),
            Strategy::StripTrailingGarbage => strip_trailing(&cur, &a),
            Strategy::RelocateOffsets => relocate(&cur, &a),
        };
        match step {
            Ok(Some(st)) => {
                log::debug!("repair step {}: {}", s.name(), st.note);
                applied.push(format!("{}: {}", s.name(), st.note));
                lossy |= st.lossy;
                if matches!(s, Strategy::RebuildMoov { .. }) {
                    rebuilt = true;
                }
                cur = Arc::new(st.out);
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("repair step {} failed: {e}", s.name());
                notes.push(format!("{} failed: {e}", s.name()));
            }
        }
    }
    Ok((cur, applied, notes, lossy))
}

/// Apply `strategies` to `src` and write the result to `out` after
/// re-validating it. Nothing is written when no strategy applied or the
/// result does not validate.
pub fn apply(
    src: &SharedSource,
    out: &mut dyn Write,
    strategies: &[Strategy],
    ctx: &RepairContext,
) -> Result<RepairOutcome> {
    let _ = ctx;
    let (cur, applied, mut notes, lossy) = apply_to_source(src, strategies)?;
    if applied.is_empty() {
        if notes.is_empty() {
            notes.push("no repair strategy applies to this file".into());
        }
        return Ok(RepairOutcome {
            result: RepairResult::Unrepairable,
            applied,
            output_len: 0,
            validation: None,
            notes,
        });
    }
    let validation = crate::mp4::validate_source(&*cur)?;
    if validation.status == ValidationStatus::Invalid
        || crate::analysis::analyze(&*cur)?.moov.is_none()
    {
        notes.push("the repaired file did not validate; nothing written".into());
        return Ok(RepairOutcome {
            result: RepairResult::Unrepairable,
            applied,
            output_len: 0,
            validation: Some(validation),
            notes,
        });
    }
    let output_len = copy_source(&*cur, out)?;
    let result = if validation.status == ValidationStatus::Valid && !lossy {
        RepairResult::Repaired
    } else {
        RepairResult::PartiallyRepaired
    };
    notes.extend(
        validation
            .problems
            .iter()
            .map(|p| format!("after repair: {p}")),
    );
    Ok(RepairOutcome {
        result,
        applied,
        output_len,
        validation: Some(validation),
        notes,
    })
}

/// Stream a whole source into `out` in 1 MiB blocks.
pub fn copy_source(src: &dyn Source, out: &mut dyn Write) -> Result<u64> {
    let size = src.size();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < size {
        let n = (size - pos).min(buf.len() as u64) as usize;
        src.read_exact_at(pos, &mut buf[..n])?;
        out.write_all(&buf[..n])?;
        pos += n as u64;
    }
    out.flush()?;
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::BoxBuf;

    #[test]
    fn chunk_offsets_are_patched() {
        let mut b = BoxBuf::new();
        b.start(b"moov");
        b.start(b"trak");
        b.start(b"mdia");
        b.start(b"minf");
        b.start(b"stbl");
        b.start_full(b"stco", 0, 0);
        b.u32(2);
        b.u32(100);
        b.u32(200);
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        let mut moov = b.buf;
        assert_eq!(patch_chunk_offsets(&mut moov, &|o| Some(o + 5)).unwrap(), 2);
        let n = moov.len();
        assert_eq!(&moov[n - 8..], &[0, 0, 0, 105, 0, 0, 0, 205]);
        assert!(patch_chunk_offsets(&mut moov, &|_| Some(u64::MAX)).is_err());
        assert_eq!(Strategy::FixMdatSize.name(), "fix_mdat_size");
    }
}
