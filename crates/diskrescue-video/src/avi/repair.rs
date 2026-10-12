//! AVI repair planning.
//!
//! A repair never writes while it works out what to do. It walks the damaged
//! file once, decides which chunks survive, builds the new header patches and
//! the new `idx1` in memory, and describes the output as a [`SegSource`]:
//! literal bytes plus slices of the input. The plan is validated like any
//! other file before it is streamed out.

use std::sync::Arc;

use diskrescue_core::util::bytes;
use diskrescue_core::{SharedSource, Source};

use crate::io::Window;
use crate::meta::annexb_nals;
use crate::seg::SegSource;

use super::hdr::{video_codec, StreamFormat, VideoClass};
use super::scan::{avix_movi, scan_head, MAX_AVIX};
use super::walk::{resync, IdKind, MoviWalker, Step, WalkEnd};

/// How much of the file a repair is allowed to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AviStrategy {
    /// Walk `movi`, drop what is damaged, write a fresh `idx1`, and fix every
    /// RIFF/LIST size and frame count. The most thorough option and the
    /// default of [`AviFormat::repair`](super::AviFormat).
    RebuildIndex,
    /// Cut the file after the last good chunk and fix sizes and counts, but
    /// write no index (the `avih` index flag is cleared).
    TruncateToLastChunk,
    /// Only patch the size and count fields; every other byte is kept, even
    /// if damaged.
    FixSizes,
}

impl AviStrategy {
    /// Stable name, as used in `Validation::repairable_by`.
    pub fn name(self) -> &'static str {
        match self {
            AviStrategy::RebuildIndex => super::validate::REBUILD,
            AviStrategy::TruncateToLastChunk => super::validate::TRUNCATE,
            AviStrategy::FixSizes => super::validate::FIX_SIZES,
        }
    }

    /// Parse a name produced by [`AviStrategy::name`].
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "rebuild_index" => AviStrategy::RebuildIndex,
            "truncate_to_last_chunk" => AviStrategy::TruncateToLastChunk,
            "fix_sizes" => AviStrategy::FixSizes,
            _ => return None,
        })
    }
}

/// Maximum size of the header region copied into memory for patching.
const MAX_PRE_MOVI: u64 = 32 * 1024 * 1024;
/// How far a resync may scan past damage.
const RESYNC_SCAN: u64 = 1024 * 1024;

const AVIIF_KEYFRAME: u32 = 0x10;
const AVIIF_NO_TIME: u32 = 0x100;

/// Result of planning.
pub(crate) struct Plan {
    pub out: SegSource,
    pub applied: Vec<String>,
    pub notes: Vec<String>,
}

/// Why no plan could be made.
pub(crate) struct Unrepairable(pub String);

/// One RIFF chunk of the output: its header bytes and the copied runs.
struct Section {
    /// Literal bytes that open the section (RIFF/LIST headers; for the
    /// first RIFF the patched pre-movi region plus the `LIST movi` header
    /// are added at assembly time).
    runs: Vec<Run>,
    /// Content length (sum of runs).
    len: u64,
}

enum Run {
    Src(u64, u64),
    Pad,
}

impl Section {
    fn new() -> Self {
        Section {
            runs: Vec::new(),
            len: 0,
        }
    }
    fn src(&mut self, off: u64, len: u64) {
        // Adjacent slices merge, so a healthy file is a handful of runs
        // rather than one per chunk.
        if let Some(Run::Src(o, l)) = self.runs.last_mut() {
            if *o + *l == off {
                *l += len;
                self.len += len;
                return;
            }
        }
        self.runs.push(Run::Src(off, len));
        self.len += len;
    }
    fn pad(&mut self) {
        self.runs.push(Run::Pad);
        self.len += 1;
    }
}

/// Decide whether a video chunk is a keyframe.
fn is_keyframe(class: VideoClass, data: &[u8], size: u32) -> bool {
    if size == 0 {
        return false;
    }
    match class {
        VideoClass::AllKey | VideoClass::Unknown => true,
        VideoClass::Mpeg4 => {
            let lim = data.len().min(4096);
            data[..lim]
                .windows(5)
                .find(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && w[3] == 0xB6)
                .is_some_and(|w| w[4] >> 6 == 0)
        }
        VideoClass::MsMpeg4V3 => data.first().is_some_and(|b| b >> 6 == 0),
        VideoClass::H264 | VideoClass::Hevc => h26x_keyframe(class == VideoClass::Hevc, data),
    }
}

fn h26x_keyframe(hevc: bool, data: &[u8]) -> bool {
    let mut nals: Vec<(usize, usize)> = annexb_nals(data, 32);
    if nals.is_empty() && data.len() > 5 {
        // Length-prefixed (AVCC style) samples.
        let mut p = 0usize;
        while p + 5 <= data.len() && nals.len() < 32 {
            let l = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
            if l == 0 || p + 4 + l > data.len() {
                break;
            }
            nals.push((p + 4, p + 4 + l));
            p += 4 + l;
        }
    }
    for (s, _) in nals {
        let b = data[s];
        if hevc {
            let t = (b >> 1) & 0x3F;
            match t {
                16..=21 => return true,
                0..=9 => return false,
                _ => {}
            }
        } else {
            match b & 0x1F {
                5 => return true,
                1..=4 => return false,
                _ => {}
            }
        }
    }
    false
}

fn put32(buf: &mut [u8], off: u64, v: u32) {
    let off = off as usize;
    if let Some(s) = buf.get_mut(off..off + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// Build the repair plan.
pub(crate) fn plan(
    src: &SharedSource,
    strategy: AviStrategy,
    aggressive: bool,
) -> Result<Plan, Unrepairable> {
    let mut win = Window::new(&**src, 0, u64::MAX);
    let avail = win.avail();
    let head = scan_head(&mut win).ok_or_else(|| {
        Unrepairable(
            "the AVI header list (hdrl) or the movi list could not be located; \
             the header would have to be rebuilt from a donor file"
                .into(),
        )
    })?;
    if head.movi_pos > MAX_PRE_MOVI {
        return Err(Unrepairable("implausibly large AVI header region".into()));
    }
    let mut pre = win
        .vec(0, head.movi_pos as usize)
        .ok_or_else(|| Unrepairable("cannot read the AVI header region".into()))?;

    let mut applied = Vec::new();
    let mut notes = Vec::new();

    // Codec classes per stream index.
    let classes: Vec<VideoClass> = head
        .hdrl
        .streams
        .iter()
        .map(|s| {
            if s.is_video() {
                video_codec(s).1
            } else {
                VideoClass::AllKey
            }
        })
        .collect();
    let unknown_codecs: Vec<String> = head
        .hdrl
        .streams
        .iter()
        .filter(|s| s.is_video() && video_codec(s).1 == VideoClass::Unknown)
        .map(|s| video_codec(s).0)
        .collect();
    if !unknown_codecs.is_empty() && strategy == AviStrategy::RebuildIndex {
        notes.push(format!(
            "keyframes of codec(s) {} are unknown; every frame is flagged as a keyframe",
            unknown_codecs.join(", ")
        ));
    }

    // ------------------------------------------------------------------
    // Walk every movi list.
    // ------------------------------------------------------------------
    let mut sections: Vec<Section> = vec![Section::new()];
    let mut entries: Vec<u8> = Vec::new(); // new idx1 body (first RIFF only)
    let mut out_off: u64 = 4; // offset of the next chunk from the movi tag
    let mut counts = [0u64; 100];
    let mut bytes_by = [0u64; 100];
    let mut first_counts = [0u64; 100];
    let mut dropped_ix = 0u64;
    let mut flattened = false;
    let mut skipped_bytes = 0u64;
    let mut idx_capped = false;
    let mut first_movi_end = head.movi_data();
    let mut tail_note: Option<String> = None;
    let mut avix_hdrs: Vec<u64> = Vec::new(); // source offsets of AVIX RIFF headers
    let mut first_riff_end = avail;
    let mut fix_has_idx1 = false;

    if strategy == AviStrategy::FixSizes {
        // Nothing is dropped; walk only to learn the counts and the end of
        // the first movi.
        let walked = super::validate::walk_all(&mut win, &head);
        counts = walked.chunks;
        bytes_by = walked.bytes;
        first_counts = walked.first_riff_chunks;
        first_movi_end = walked.first_movi_end.max(head.movi_data());
        first_riff_end = if walked.avix > 0 {
            walked.first_riff_end
        } else {
            avail
        };
        fix_has_idx1 = walked.idx1.is_some();
        if !matches!(
            walked.end,
            WalkEnd::Idx1(_) | WalkEnd::Riff(_) | WalkEnd::Clean(_)
        ) {
            notes.push(format!(
                "damage at offset {} is left in place (only sizes were fixed)",
                walked.end.pos()
            ));
        }
    } else {
        let mut seg_idx = 0usize;
        let mut walker = MoviWalker::new(head.movi_data(), avail);
        let mut first_riff = true;
        let mut good_end = head.movi_data();
        loop {
            match walker.step(&mut win) {
                Step::Chunk(c) => {
                    good_end = c.next.min(avail);
                    match c.kind {
                        IdKind::OdmlIndex => dropped_ix += 1,
                        IdKind::List => flattened = true,
                        IdKind::Junk
                        | IdKind::Video(_)
                        | IdKind::Audio(_)
                        | IdKind::Palette(_)
                        | IdKind::Other(_) => {
                            let size = u64::from(c.hdr.size);
                            // Copy header + data, then the pad byte if it
                            // exists in the source; synthesise it otherwise.
                            let has_pad_in_src =
                                c.hdr.size & 1 == 1 && c.next > c.hdr.pos + 8 + size;
                            sections[seg_idx].src(c.hdr.pos, 8 + size + u64::from(has_pad_in_src));
                            if c.hdr.size & 1 == 1 && !has_pad_in_src {
                                sections[seg_idx].pad();
                            }
                            let this_len = 8 + size + u64::from(c.hdr.size & 1);
                            let stream = usize::from(c.kind.stream().unwrap_or(0)).min(99);
                            if c.kind.is_data() {
                                counts[stream] += 1;
                                bytes_by[stream] += size;
                            }
                            if first_riff {
                                if c.kind.is_data() {
                                    first_counts[stream] += 1;
                                    if strategy == AviStrategy::RebuildIndex {
                                        if out_off > u64::from(u32::MAX) - 16 {
                                            idx_capped = true;
                                        } else {
                                            let flags = chunk_flags(
                                                &mut win,
                                                &classes,
                                                c.kind,
                                                stream,
                                                c.hdr.data(),
                                                c.hdr.size,
                                            );
                                            entries.extend_from_slice(&c.hdr.id);
                                            entries.extend_from_slice(&flags.to_le_bytes());
                                            entries
                                                .extend_from_slice(&(out_off as u32).to_le_bytes());
                                            entries.extend_from_slice(&c.hdr.size.to_le_bytes());
                                        }
                                    }
                                }
                                out_off += this_len;
                            }
                        }
                        IdKind::Idx1 | IdKind::Riff => {}
                    }
                }
                Step::End(end) => {
                    let mut resumed = false;
                    match end {
                        WalkEnd::Garbage(p) | WalkEnd::Partial(p) | WalkEnd::Fault(p) => {
                            if aggressive && p < avail {
                                if let Some(q) =
                                    resync(&mut win, p + 1, walker.hard_end(), RESYNC_SCAN)
                                {
                                    log::warn!(
                                        "AVI repair: skipping {} damaged bytes at offset {p}",
                                        q - p
                                    );
                                    skipped_bytes += q - p;
                                    notes.push(format!(
                                        "skipped {} damaged bytes at offset {p}",
                                        q - p
                                    ));
                                    walker.jump(q);
                                    resumed = true;
                                }
                            }
                            if !resumed {
                                tail_note = Some(format!(
                                    "data from offset {p} on is damaged or missing and was dropped"
                                ));
                            }
                        }
                        _ => {}
                    }
                    if resumed {
                        continue;
                    }
                    if first_riff {
                        first_movi_end = good_end;
                    }
                    // Another RIFF (AVIX) after idx1 / at the boundary?
                    let next_riff = match end {
                        WalkEnd::Idx1(p) => {
                            let c = super::hdr::read_chunk(&mut win, p);
                            c.filter(|c| c.pos + 8 + u64::from(c.size) <= avail)
                                .map(|c| c.next().min(avail))
                        }
                        WalkEnd::Riff(p) => Some(p),
                        _ => None,
                    };
                    if let Some(q) = next_riff {
                        if avix_hdrs.len() < MAX_AVIX {
                            if let Some(data) = avix_movi(&mut win, q) {
                                avix_hdrs.push(q);
                                sections.push(Section::new());
                                seg_idx += 1;
                                first_riff = false;
                                walker = MoviWalker::new(data, avail);
                                good_end = data;
                                continue;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Patch the header region.
    // ------------------------------------------------------------------
    let video_streams: Vec<usize> = head
        .hdrl
        .streams
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_video())
        .map(|(i, _)| i)
        .collect();
    let first_video = video_streams.first().copied();
    let first_riff_video = first_video.map_or(0, |i| first_counts[i.min(99)]);
    let total_video = first_video.map_or(0, |i| counts[i.min(99)]);

    let has_odml = head.hdrl.streams.iter().any(|s| s.indx_pos.is_some());
    let write_index = strategy == AviStrategy::RebuildIndex && !idx_capped;
    // After the repair the file has an index when we wrote one or, for
    // FixSizes, when the old one was left in place.
    let index_present = write_index || (strategy == AviStrategy::FixSizes && fix_has_idx1);

    if let Some(a) = &head.hdrl.avih {
        put32(&mut pre, a.pos + 16, first_riff_video as u32);
        let flags = if index_present {
            a.flags | 0x10
        } else {
            a.flags & !0x10
        };
        put32(&mut pre, a.pos + 12, flags);
        applied.push(format!("set avih.dwTotalFrames to {first_riff_video}"));
    }
    if let Some((pos, _)) = head.hdrl.dmlh {
        put32(&mut pre, pos, total_video as u32);
    }
    for (i, s) in head.hdrl.streams.iter().enumerate() {
        let n = counts[i.min(99)];
        // PCM-style audio counts blocks of `dwSampleSize` bytes, everything
        // else counts chunks.
        let len = match (&s.format, s.sample_size) {
            (StreamFormat::Audio { .. }, unit) if unit > 0 => {
                bytes_by[i.min(99)].div_ceil(u64::from(unit))
            }
            _ => n,
        };
        put32(&mut pre, s.strh_pos + 32, len as u32);
        if strategy != AviStrategy::FixSizes {
            if let Some(ix) = s.indx_pos {
                // The OpenDML super index points at ix## chunks that are
                // dropped or moved; neutralise it.
                if let Some(id) = pre.get_mut(ix as usize..ix as usize + 4) {
                    id.copy_from_slice(b"JUNK");
                }
            }
        }
    }
    if has_odml && strategy != AviStrategy::FixSizes {
        applied.push("neutralised the OpenDML super index (indx became JUNK)".into());
    }
    applied.push("recomputed stream lengths (strh.dwLength)".into());

    // ------------------------------------------------------------------
    // Assemble.
    // ------------------------------------------------------------------
    let mut out = SegSource::new(Arc::clone(src));
    if strategy == AviStrategy::FixSizes {
        // movi LIST header: new size; everything else verbatim.
        let movi_content = first_movi_end.saturating_sub(head.movi_pos + 8);
        let movi_size = movi_content.min(u64::from(u32::MAX)) as u32;
        let riff_end = first_riff_end;
        put32(
            &mut pre,
            4,
            (riff_end.saturating_sub(8)).min(u64::from(u32::MAX)) as u32,
        );
        let mut movi_hdr = Vec::with_capacity(12);
        movi_hdr.extend_from_slice(b"LIST");
        movi_hdr.extend_from_slice(&movi_size.to_le_bytes());
        movi_hdr.extend_from_slice(b"movi");
        out.push_mem(pre);
        out.push_mem(movi_hdr);
        out.push_src(head.movi_data(), avail - head.movi_data());
        applied.push("patched RIFF and LIST movi sizes".into());
    } else {
        let first = &sections[0];
        let idx_len = if write_index {
            8 + entries.len() as u64
        } else {
            0
        };
        let riff0_len = pre.len() as u64 + 12 + first.len + idx_len;
        put32(&mut pre, 4, (riff0_len - 8).min(u64::from(u32::MAX)) as u32);
        let mut movi_hdr = Vec::with_capacity(12);
        movi_hdr.extend_from_slice(b"LIST");
        movi_hdr
            .extend_from_slice(&((4 + first.len).min(u64::from(u32::MAX)) as u32).to_le_bytes());
        movi_hdr.extend_from_slice(b"movi");
        out.push_mem(pre);
        out.push_mem(movi_hdr);
        emit_runs(&mut out, &first.runs);
        if write_index {
            let mut hdr = Vec::with_capacity(8);
            hdr.extend_from_slice(b"idx1");
            hdr.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            out.push_mem(hdr);
            out.push_mem(std::mem::take(&mut entries));
            applied.push("wrote a new idx1 index".into());
        } else {
            applied.push("dropped the index (idx1)".into());
        }
        for sec in sections.iter().skip(1) {
            let mut h = Vec::with_capacity(24);
            let riff_size = 4 + 8 + 4 + sec.len;
            h.extend_from_slice(b"RIFF");
            h.extend_from_slice(&(riff_size.min(u64::from(u32::MAX)) as u32).to_le_bytes());
            h.extend_from_slice(b"AVIX");
            h.extend_from_slice(b"LIST");
            h.extend_from_slice(&((4 + sec.len).min(u64::from(u32::MAX)) as u32).to_le_bytes());
            h.extend_from_slice(b"movi");
            out.push_mem(h);
            emit_runs(&mut out, &sec.runs);
        }
        applied.push("rewrote RIFF and LIST movi sizes".into());
        if dropped_ix > 0 {
            applied.push(format!("dropped {dropped_ix} OpenDML ix## index chunk(s)"));
        }
        if flattened {
            applied.push("flattened 'rec ' lists".into());
        }
        if skipped_bytes > 0 {
            applied.push(format!("skipped {skipped_bytes} damaged bytes"));
        }
        if let Some(t) = tail_note {
            applied.push("truncated after the last intact chunk".into());
            notes.push(t);
        }
        if idx_capped {
            notes.push(
                "the file exceeds 4 GiB: idx1 cannot address it, so no index was written".into(),
            );
        }
    }
    log::debug!(
        "AVI repair plan ({strategy:?}): {} bytes out of {avail}, {} step(s)",
        out.len(),
        applied.len()
    );
    Ok(Plan {
        out,
        applied,
        notes,
    })
}

/// Flags for one `idx1` entry.
fn chunk_flags(
    win: &mut Window<'_>,
    classes: &[VideoClass],
    kind: IdKind,
    stream: usize,
    data_pos: u64,
    size: u32,
) -> u32 {
    match kind {
        IdKind::Video(_) => {
            let class = classes.get(stream).copied().unwrap_or(VideoClass::Unknown);
            let n = (size as usize).min(match class {
                VideoClass::H264 | VideoClass::Hevc => 32 * 1024,
                _ => 4096,
            });
            let data = if matches!(class, VideoClass::AllKey | VideoClass::Unknown) {
                &[][..]
            } else {
                win.get(data_pos, n)
            };
            if is_keyframe(class, data, size) {
                AVIIF_KEYFRAME
            } else {
                0
            }
        }
        IdKind::Palette(_) => AVIIF_NO_TIME,
        _ => AVIIF_KEYFRAME,
    }
}

fn emit_runs(out: &mut SegSource, runs: &[Run]) {
    for r in runs {
        match r {
            Run::Src(off, len) => out.push_src(*off, *len),
            Run::Pad => out.push_mem(vec![0]),
        }
    }
}

/// Rough check used by tests: the u32 at `off`.
#[allow(dead_code)]
pub(crate) fn peek32(src: &dyn Source, off: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    src.read_at(off, &mut b).ok()?;
    bytes::u32le(&b, 0)
}
