//! Structural analysis of a whole ISO BMFF movie file, shared by
//! validation and repair planning.
//!
//! Levels (video spec §3.3):
//! * L0: top-level box walk (ftyp first, known types, sizes consistent,
//!   trailing data detected).
//! * L1: `moov` parses; for every track the sample map is built and checked
//!   (samples inside the file, chunk offsets monotone, `stts` count equals
//!   `stsz` count, `stts` total equals `mdhd` duration within one sample).
//! * L2: sampled content checks: the first NAL unit of up to 256 evenly
//!   spaced video chunks must have a plausible length and header; for AAC
//!   tracks a sample of frames must walk to exactly their table size. A
//!   `moov` whose chunks do not start on samples is stale (from another
//!   recording) or corrupt.
//!
//! When the `mdat` size field is wrong (zero, or running past a trailing
//! `moov`), the trailing `moov` is searched backwards from the end of the
//! file (a box whose size reaches exactly the end), then forwards at 4-byte
//! alignment through the last 64 MiB.

use diskrescue_core::util::bytes::u32be;
use diskrescue_core::{Result, Source, SourceExt};

use crate::boxes::{
    self, is_top_level_type, parse_moov, sample_table_checked, walk_top_level, BoxHeader, Moov,
    Sample, TopBox, TopStop,
};
use crate::{aac, h264, hevc};

/// Per-track findings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackCheck {
    /// Track id.
    pub track_id: u32,
    /// Handler type.
    pub handler: [u8; 4],
    /// Codec name.
    pub codec: String,
    /// Samples declared by the tables.
    pub samples: u64,
    /// Samples entirely inside the file (and not overlapping unreadable
    /// ranges).
    pub inside: u64,
    /// Index of the first sample that is not inside.
    pub first_outside: Option<u64>,
    /// Some chunk offset lies beyond the end of the file.
    pub chunk_beyond_eof: bool,
    /// Smallest chunk offset.
    pub min_chunk_offset: Option<u64>,
    /// End of the last sample (absolute).
    pub max_extent: u64,
    /// Content checks performed / failed (L2).
    pub l2_checked: u32,
    /// Content checks that failed.
    pub l2_failed: u32,
    /// Checked samples whose first bytes are zero-filled or unreadable
    /// (holes: counted neither as passed nor as failed).
    pub l2_holes: u32,
    /// Samples whose content check (L3) found damage: video samples that
    /// are not made of whole, intact NAL units, AAC frames that are all
    /// zero, any sample overlapping a known-unreadable range. Indices
    /// (0-based, decode order), at most [`MAX_DAMAGED_LISTED`].
    pub damaged_samples: Vec<u32>,
    /// Samples the L3 check looked at.
    pub l3_checked: u64,
    /// Problems.
    pub problems: Vec<String>,
}

/// Largest number of damaged sample indices kept per track.
pub const MAX_DAMAGED_LISTED: usize = 1 << 20;

/// Bytes of sample data the validation reads for the L3 content check
/// (larger files are checked on evenly spaced samples).
pub const L3_BUDGET: u64 = 1 << 30;

/// Largest single sample the L3 check reads.
const L3_MAX_SAMPLE: u32 = 64 << 20;

/// Content check of one stored video sample: it must be a sequence of
/// whole length-prefixed NAL units (forbidden bit clear, no damaged bytes,
/// see [`crate::rebuild::nal_bytes_damaged`]), with at most 16 zero bytes
/// of padding after the last one. Returns `(intact, random_access)`;
/// `random_access` is set when it holds an IDR (H.264) / IRAP (HEVC) NAL.
pub fn check_video_sample(data: &[u8], l: usize, hevc_codec: bool) -> (bool, bool) {
    if !(1..=4).contains(&l) {
        return (false, false);
    }
    let mut p = 0usize;
    let mut nals = 0u32;
    let mut rap = false;
    while p < data.len() {
        let rest = &data[p..];
        if rest.len() <= 16 && rest.iter().all(|&b| b == 0) && nals > 0 {
            break;
        }
        if rest.len() < l + 1 {
            return (false, rap);
        }
        let n = rest[..l].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        if n == 0 || n > rest.len() - l {
            return (false, rap);
        }
        let nal = &rest[l..l + n];
        if nal[0] & 0x80 != 0 || crate::rebuild::nal_bytes_damaged(nal) {
            return (false, rap);
        }
        if hevc_codec {
            rap |= hevc::is_irap(hevc::nal_type(nal[0]));
        } else {
            rap |= h264::nal_type(nal[0]) == 5;
        }
        p += l + n;
        nals += 1;
        if nals > 100_000 {
            return (false, rap);
        }
    }
    (nals > 0, rap)
}

/// Content check of one stored AAC frame: readable, not all zero, and,
/// when the frame walker supports the configuration, exactly one raw data
/// block of the stored size.
pub fn aac_sample_intact(data: &[u8], size: u32, cfg: Option<&aac::AudioSpecificConfig>) -> bool {
    if data.len() != size as usize || (size > 0 && data.iter().all(|&b| b == 0)) {
        return false;
    }
    match cfg.filter(|c| aac::walk_supported(c)) {
        Some(c) => aac::frame_length(data, c) == Some(size as usize),
        None => true,
    }
}

/// L3: content check of every sample (or of evenly spaced samples within
/// `budget` bytes): see [`TrackCheck::damaged_samples`].
fn level3(
    src: &dyn Source,
    t: &crate::boxes::Trak,
    samples: &[Sample],
    tc: &mut TrackCheck,
    budget: u64,
) {
    let Some(e) = t.entry() else { return };
    let size = src.size();
    let unreadable = src.unreadable();
    let video = e
        .avcc
        .as_ref()
        .map(|c| (c.nal_length_size as usize, false))
        .or_else(|| e.hvcc.as_ref().map(|c| (c.nal_length_size as usize, true)));
    let aac = e.esds.as_ref().is_some_and(|es| es.object_type == 0x40);
    let aac_cfg = e
        .esds
        .as_ref()
        .and_then(|es| aac::parse_audio_specific_config(&es.audio_specific_config));
    let total: u64 = samples
        .iter()
        .filter(|s| s.end() <= size)
        .map(|s| s.size as u64)
        .sum();
    let step = if total > budget {
        (total / budget.max(1)).saturating_add(1) as usize
    } else {
        1
    };
    for s in samples.iter().step_by(step.max(1)) {
        if s.end() > size {
            break;
        }
        tc.l3_checked += 1;
        let holed = unreadable.as_ref().is_some_and(|u| {
            u.intersects(diskrescue_core::Range::with_len(s.offset, s.size as u64))
        });
        let damaged = if holed {
            true
        } else if let Some((l, hevc_codec)) = video {
            if s.size > L3_MAX_SAMPLE {
                false
            } else {
                match src.read_vec(s.offset, s.size as usize) {
                    Ok(d) if d.len() == s.size as usize => !check_video_sample(&d, l, hevc_codec).0,
                    _ => true,
                }
            }
        } else if aac && s.size <= 1 << 16 {
            match src.read_vec(s.offset, s.size as usize) {
                Ok(d) => !aac_sample_intact(&d, s.size, aac_cfg.as_ref()),
                _ => true,
            }
        } else {
            false
        };
        if damaged && tc.damaged_samples.len() < MAX_DAMAGED_LISTED {
            tc.damaged_samples.push(s.index);
        }
    }
    // When most samples fail, the check does not fit how this file was
    // written (or the index is stale, which L2 reports): do not call every
    // sample damaged, which would make a repair drop them.
    let n = tc.damaged_samples.len() as u64;
    if n > 8 && n * 2 > tc.l3_checked && unreadable.is_none() {
        tc.damaged_samples.clear();
        tc.problems.push(format!(
            "content check inconclusive: {n} of {} samples are not whole NAL units / frames",
            tc.l3_checked
        ));
    }
}

/// L1 checks of a constant-size track (uncompressed PCM) chunk by chunk,
/// for tracks with more samples than [`boxes::MAX_SAMPLES`] (every PCM
/// frame is a sample). Returns false when the tables are unusable.
fn constant_size_check(src: &dyn Source, t: &crate::boxes::Trak, tc: &mut TrackCheck) -> bool {
    let crate::boxes::SampleSizes::Constant { size: sz, count } = t.stsz else {
        return false;
    };
    let Ok(chunks) = boxes::chunk_runs(t) else {
        return false;
    };
    let size = src.size();
    let unreadable = src.unreadable();
    tc.samples = count as u64;
    tc.min_chunk_offset = t.stco.iter().copied().min();
    tc.chunk_beyond_eof = t.stco.iter().any(|&o| o >= size);
    let mut placed = 0u64;
    let mut inside = 0u64;
    let mut cut = false;
    for c in &chunks {
        let end = c.offset.saturating_add(c.samples as u64 * sz as u64);
        tc.max_extent = tc.max_extent.max(end);
        placed += c.samples as u64;
        if cut {
            continue;
        }
        let holed = unreadable
            .as_ref()
            .is_some_and(|u| u.intersects(diskrescue_core::Range::new(c.offset, end)));
        if end <= size && !holed {
            inside += c.samples as u64;
        } else {
            if !holed && c.offset < size && sz > 0 {
                inside += ((size - c.offset) / sz as u64).min(c.samples as u64);
            }
            cut = true;
        }
    }
    tc.inside = inside;
    if inside < tc.samples {
        tc.first_outside = Some(inside);
    }
    if placed < tc.samples {
        tc.problems.push(format!(
            "chunk map places {placed} of {} samples",
            tc.samples
        ));
    }
    let stts: u64 = t.stts.iter().map(|e| e.0 as u64).sum();
    if stts != tc.samples {
        tc.problems.push(format!(
            "stts covers {stts} samples, stsz declares {}",
            tc.samples
        ));
    }
    true
}

/// Findings for a whole file.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    /// File size.
    pub size: u64,
    /// Top-level boxes as walked.
    pub boxes: Vec<TopBox>,
    /// How the walk ended.
    pub stop: Option<TopStop>,
    /// Major brand from `ftyp`.
    pub major_brand: Option<[u8; 4]>,
    /// Compatible brands from `ftyp`.
    pub compatible: Vec<[u8; 4]>,
    /// The `moov` box header (found by the walk or by the fallback search).
    pub moov_box: Option<BoxHeader>,
    /// The parsed `moov`.
    pub moov: Option<Moov>,
    /// Why the `moov` could not be parsed.
    pub moov_error: Option<String>,
    /// The first `mdat` box.
    pub mdat: Option<BoxHeader>,
    /// Where the `mdat` really ends when its size field is wrong.
    pub mdat_real_end: Option<u64>,
    /// End of the last structurally valid top-level box.
    pub structure_end: u64,
    /// Offset of trailing bytes that are not boxes.
    pub trailing_garbage: Option<u64>,
    /// `moof` present.
    pub fragmented: bool,
    /// Per-track checks.
    pub tracks: Vec<TrackCheck>,
    /// Constant shift that would bring every chunk inside the media data,
    /// when the stored offsets are off by one (head of the file cut or
    /// boxes inserted before `mdat`).
    pub relocation_delta: Option<i64>,
    /// Problems, most important first.
    pub problems: Vec<String>,
}

impl Analysis {
    /// True when the moov parsed and its tables point at real samples.
    pub fn moov_usable(&self) -> bool {
        self.moov.is_some() && !self.moov_stale()
    }

    /// True when the L2 checks say the moov does not describe this mdat.
    pub fn moov_stale(&self) -> bool {
        let (c, f) = self
            .tracks
            .iter()
            .fold((0, 0), |a, t| (a.0 + t.l2_checked, a.1 + t.l2_failed));
        c > 0 && f * 5 > c
    }

    /// Samples declared / inside the file over all tracks.
    pub fn sample_counts(&self) -> (u64, u64) {
        self.tracks
            .iter()
            .fold((0, 0), |a, t| (a.0 + t.samples, a.1 + t.inside))
    }

    /// Largest sample end over all tracks.
    pub fn max_extent(&self) -> Option<u64> {
        self.tracks.iter().map(|t| t.max_extent).max()
    }
}

fn find_trailing_moov(src: &dyn Source, from: u64) -> Result<Option<BoxHeader>> {
    let size = src.size();
    let lo = from.max(size.saturating_sub(64 << 20));
    let mut end = size;
    let mut candidates: Vec<u64> = Vec::new();
    while end > lo {
        let start = end.saturating_sub(1 << 20).max(lo);
        let len = (end - start + 8).min(size - start);
        let buf = src.read_vec(start, len as usize)?;
        for j in (4..buf.len().saturating_sub(3)).rev() {
            if &buf[j..j + 4] != b"moov" {
                continue;
            }
            let at = start + j as u64 - 4;
            let sz = u32be(&buf, j - 4).unwrap_or(0) as u64;
            if sz < 16 || at < lo {
                continue;
            }
            if at + sz == size {
                if let Some(h) = boxes::read_header(src, at, size)? {
                    if parse_moov(src, &h).is_ok() {
                        return Ok(Some(h));
                    }
                }
            }
            if at % 4 == 0 && at + sz <= size {
                candidates.push(at);
            }
        }
        end = start;
    }
    candidates.sort_unstable();
    candidates.dedup();
    for at in candidates.into_iter().take(64) {
        if let Some(h) = boxes::read_header(src, at, size)? {
            if parse_moov(src, &h).is_ok() {
                return Ok(Some(h));
            }
        }
    }
    Ok(None)
}

/// Check the first NAL of a video sample (`head` = the first bytes).
fn nal_start_ok(head: &[u8], l: usize, hevc_codec: bool, sample_size: u32) -> bool {
    if head.len() < l + 2 {
        return false;
    }
    let n = head[..l].iter().fold(0u64, |a, &b| (a << 8) | b as u64);
    if n == 0 || n + l as u64 > sample_size as u64 {
        return false;
    }
    if hevc_codec {
        match hevc::parse_nal_header(&head[l..]) {
            Some(h) => hevc::is_slice_type(h.nal_type) || (32..=40).contains(&h.nal_type),
            None => false,
        }
    } else {
        let b = head[l];
        b & 0x80 == 0 && matches!(h264::nal_type(b), 1 | 5 | 6 | 7 | 8 | 9 | 12 | 13 | 14 | 15)
    }
}

fn level2(src: &dyn Source, moov: &Moov, idx: usize, samples: &[Sample], tc: &mut TrackCheck) {
    let t = &moov.tracks[idx];
    let Some(e) = t.entry() else { return };
    let size = src.size();
    let inside: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.end() <= size && s.size > 0)
        .collect();
    if inside.is_empty() {
        return;
    }
    if let Some((l, is_hevc)) = e
        .avcc
        .as_ref()
        .map(|c| (c.nal_length_size as usize, false))
        .or_else(|| e.hvcc.as_ref().map(|c| (c.nal_length_size as usize, true)))
    {
        // First sample of up to 256 evenly spaced chunks.
        let mut firsts: Vec<&Sample> = Vec::new();
        let mut last_chunk = u32::MAX;
        for s in &inside {
            if s.chunk != last_chunk {
                firsts.push(s);
                last_chunk = s.chunk;
            }
        }
        let step = firsts.len().div_ceil(256).max(1);
        for s in firsts.iter().step_by(step) {
            let head = src.read_vec(s.offset, 8).unwrap_or_default();
            if head.len() < 8 || head.iter().all(|&b| b == 0) {
                // A zero-filled or unreadable region is damage, not
                // evidence that the index belongs to another recording.
                tc.l2_holes += 1;
                continue;
            }
            tc.l2_checked += 1;
            if !nal_start_ok(&head, l, is_hevc, s.size) {
                tc.l2_failed += 1;
            }
        }
    } else if let Some(cfg) = e
        .esds
        .as_ref()
        .and_then(|es| aac::parse_audio_specific_config(&es.audio_specific_config))
        .filter(aac::walk_supported)
    {
        let step = inside.len().div_ceil(64).max(1);
        // No AAC frame exceeds 6144 bits per channel.
        let max_frame = 768 * (cfg.channels.max(1) as usize) + 64;
        for s in inside.iter().step_by(step) {
            if s.size as usize > max_frame {
                tc.l2_checked += 1;
                tc.l2_failed += 1;
                continue;
            }
            let data = src.read_vec(s.offset, s.size as usize).unwrap_or_default();
            if data.len() < s.size as usize || data.iter().all(|&b| b == 0) {
                tc.l2_holes += 1;
                continue;
            }
            tc.l2_checked += 1;
            if aac::frame_length(&data, &cfg) != Some(s.size as usize) {
                tc.l2_failed += 1;
            }
        }
    }
}

/// Analyse `src` as a complete movie file.
pub fn analyze(src: &dyn Source) -> Result<Analysis> {
    let size = src.size();
    let mut a = Analysis {
        size,
        ..Default::default()
    };
    let (tops, stop) = walk_top_level(src, 0, size, &|k| is_top_level_type(k))?;
    a.stop = Some(stop);
    a.boxes = tops.clone();
    a.structure_end = tops.last().map(|t| t.end.min(size)).unwrap_or(0);
    if let Some(f) = tops.first().filter(|t| t.header.is(b"ftyp")) {
        let p = src.read_vec(
            f.header.payload_offset(),
            f.header.payload_len(size).min(1024) as usize,
        )?;
        if p.len() >= 4 {
            a.major_brand = Some([p[0], p[1], p[2], p[3]]);
        }
        a.compatible = p
            .get(8..)
            .unwrap_or(&[])
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
    } else if !tops.is_empty() {
        a.problems.push("no ftyp box (QuickTime-style file)".into());
    }
    match stop {
        TopStop::Garbage(p) | TopStop::UnknownType(p) | TopStop::NextFile(p) => {
            a.trailing_garbage = Some(p);
        }
        TopStop::Truncated => {}
        _ => {}
    }
    a.fragmented = tops.iter().any(|t| t.header.is(b"moof"));
    a.mdat = tops.iter().find(|t| t.header.is(b"mdat")).map(|t| t.header);
    let mut moov_box = tops.iter().find(|t| t.header.is(b"moov")).map(|t| t.header);
    // mdat size 0 or running past the end hides a trailing moov.
    if moov_box.is_none() {
        if let Some(m) = &a.mdat {
            if let Some(h) = find_trailing_moov(src, m.payload_offset())? {
                a.mdat_real_end = Some(h.offset);
                a.problems.push(format!(
                    "mdat size field is wrong ({}): moov found at offset {}",
                    m.size, h.offset
                ));
                a.structure_end = h.end(size).min(size);
                a.trailing_garbage = None;
                moov_box = Some(h);
            }
        }
    }
    a.moov_box = moov_box;
    let Some(mb) = moov_box else {
        a.problems.insert(0, "moov missing".into());
        return Ok(a);
    };
    if mb.end(size) > size {
        a.problems.push("moov truncated".into());
    }
    match parse_moov(src, &mb) {
        Ok(m) => a.moov = Some(m),
        Err(e) => {
            a.moov_error = Some(e.to_string());
            a.problems.insert(0, format!("moov corrupt: {e}"));
            return Ok(a);
        }
    }
    let moov = a.moov.as_ref().expect("just set");
    if moov.has_mvex {
        a.problems
            .push("fragmented movie (moof): only the initial moov is checked".into());
    }
    for p in &moov.problems {
        a.problems.push(format!("moov: {p}"));
    }
    let unreadable = src.unreadable();
    let mut tracks = Vec::new();
    for (i, t) in moov.tracks.iter().enumerate() {
        let mut tc = TrackCheck {
            track_id: t.tkhd.track_id,
            handler: t.handler,
            codec: t.codec().unwrap_or_default(),
            ..Default::default()
        };
        tc.problems.extend(t.problems.iter().cloned());
        match sample_table_checked(t) {
            Ok((samples, check)) => {
                tc.samples = check.declared;
                tc.problems.extend(check.problems);
                tc.min_chunk_offset = t.stco.iter().copied().min();
                tc.chunk_beyond_eof = t.stco.iter().any(|&o| o >= size);
                tc.max_extent = samples.iter().map(Sample::end).max().unwrap_or(0);
                let mut inside = 0u64;
                for s in &samples {
                    let bad = s.end() > size
                        || unreadable.as_ref().is_some_and(|u| {
                            u.intersects(diskrescue_core::Range::with_len(s.offset, s.size as u64))
                        });
                    if bad {
                        break;
                    }
                    inside += 1;
                }
                tc.inside = inside;
                if inside < samples.len() as u64 {
                    tc.first_outside = Some(inside);
                }
                if t.timescale > 0 && !t.stts.is_empty() {
                    let total: u64 = t.stts.iter().map(|&(n, d)| n as u64 * d as u64).sum();
                    let maxd = t.stts.iter().map(|e| e.1 as u64).max().unwrap_or(0);
                    if t.mdhd.duration != 0
                        && t.mdhd.duration != u32::MAX as u64
                        && total.abs_diff(t.mdhd.duration) > maxd.max(1)
                    {
                        tc.problems.push(format!(
                            "stts total {total} differs from mdhd duration {}",
                            t.mdhd.duration
                        ));
                    }
                }
                level2(src, moov, i, &samples, &mut tc);
                level3(src, t, &samples, &mut tc, L3_BUDGET);
            }
            Err(e) => {
                if !constant_size_check(src, t, &mut tc) {
                    tc.problems.push(format!("sample table: {e}"));
                }
            }
        }
        tracks.push(tc);
    }
    a.tracks = tracks;
    // Relocation: every chunk outside (or the moov stale) but a constant
    // shift lines the first chunk up with the mdat payload.
    if let (Some(m), Some(min_off)) = (
        &a.mdat,
        a.tracks.iter().filter_map(|t| t.min_chunk_offset).min(),
    ) {
        let payload = m.payload_offset();
        let any_beyond = a.tracks.iter().any(|t| t.chunk_beyond_eof);
        if (any_beyond || a.moov_stale()) && min_off != payload {
            let delta = payload as i64 - min_off as i64;
            if relocation_works(src, a.moov.as_ref().expect("moov"), delta) {
                a.relocation_delta = Some(delta);
            }
        }
    }
    // Summaries.
    for t in &a.tracks {
        let kind = match &t.handler {
            b"vide" => "video",
            b"soun" => "audio",
            _ => "other",
        };
        if t.inside < t.samples {
            a.problems.push(format!(
                "mdat truncated: {} of {} {kind} samples inside the file",
                t.inside, t.samples
            ));
        }
        if t.chunk_beyond_eof {
            a.problems
                .push(format!("stco offsets beyond EOF (track {})", t.track_id));
        }
        if !t.damaged_samples.is_empty() {
            a.problems.push(format!(
                "track {}: {} {kind} sample(s) damaged (zero-filled, unreadable or broken data)",
                t.track_id,
                t.damaged_samples.len()
            ));
        } else if t.l2_holes > 0 {
            a.problems.push(format!(
                "track {}: {} sampled {kind} sample(s) start in zero-filled or unreadable data",
                t.track_id, t.l2_holes
            ));
        }
        for p in &t.problems {
            a.problems.push(format!("track {}: {p}", t.track_id));
        }
    }
    if a.moov_stale() {
        a.problems.insert(
            0,
            "moov does not describe this media data (chunks do not start on samples): stale or corrupt index"
                .into(),
        );
    }
    if let Some(m) = &a.mdat {
        if m.size == 0 && a.mdat_real_end.is_none() {
            a.problems
                .push("mdat size field is 0 (runs to the end of the file)".into());
        }
        let claimed_end = m.end(size);
        if m.size != 0 && claimed_end > size {
            a.problems.push(format!(
                "mdat truncated: claims {} bytes, {} present",
                m.size,
                size - m.offset
            ));
        }
    }
    if let Some(g) = a.trailing_garbage {
        let real_end = a.max_extent().unwrap_or(0).max(a.structure_end);
        if g >= real_end {
            a.problems.push(format!(
                "{} bytes of trailing data after the last box",
                size - g
            ));
        } else if a.mdat_real_end.is_none() {
            a.problems
                .push(format!("box structure breaks at offset {g}"));
        }
    }
    Ok(a)
}

fn relocation_works(src: &dyn Source, moov: &Moov, delta: i64) -> bool {
    let size = src.size();
    let mut checked = 0;
    for t in &moov.tracks {
        let Some(e) = t.entry() else { continue };
        let l = match (&e.avcc, &e.hvcc) {
            (Some(c), _) => (c.nal_length_size as usize, false),
            (_, Some(c)) => (c.nal_length_size as usize, true),
            _ => continue,
        };
        let Ok((samples, _)) = sample_table_checked(t) else {
            return false;
        };
        for s in samples.iter().take(8) {
            let off = s.offset as i64 + delta;
            if off < 0 || off as u64 + s.size as u64 > size {
                return false;
            }
            let head = src.read_vec(off as u64, 8).unwrap_or_default();
            if !nal_start_ok(&head, l.0, l.1, s.size) {
                return false;
            }
            checked += 1;
        }
    }
    checked > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    #[test]
    fn empty_and_garbage() {
        let a = analyze(&MemSource::new(Vec::new())).unwrap();
        assert_eq!(a.problems[0], "moov missing");
        let a = analyze(&MemSource::new(vec![0x5A; 1000])).unwrap();
        assert!(a.moov.is_none());
        assert!(nal_start_ok(&[0, 0, 0, 5, 0x65, 0x88], 4, false, 9));
        assert!(!nal_start_ok(&[0, 0, 0, 5, 0xE5, 0x88], 4, false, 9));
    }
}
