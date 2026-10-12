//! Deep AVI validation: full chunk walk, size fields, frame counts and the
//! `idx1` index compared against the chunks it claims to describe.

use diskrescue_core::format::{Validation, ValidationStatus};
use diskrescue_core::media::MediaMeta;
use diskrescue_core::Source;

use crate::io::Window;

use super::hdr::{read_chunk, ChunkHdr, StreamFormat};
use super::scan::{avix_movi, build_meta, scan_head, Head, MAX_AVIX};
use super::walk::{classify, walk_movi, IdKind, Idx1Cursor, WalkEnd};

/// Names of the repair strategies, as reported in
/// [`Validation::repairable_by`].
pub(crate) const FIX_SIZES: &str = "fix_sizes";
pub(crate) const TRUNCATE: &str = "truncate_to_last_chunk";
pub(crate) const REBUILD: &str = "rebuild_index";

/// Result of walking every chunk of a file.
#[derive(Debug, Clone)]
pub(crate) struct Walked {
    /// Data chunks per stream index (all RIFF chunks).
    pub chunks: [u64; 100],
    /// Data bytes per stream index (all RIFF chunks).
    pub bytes: [u64; 100],
    /// Data chunks per stream index in the first RIFF only.
    pub first_riff_chunks: [u64; 100],
    /// Data chunks in the first RIFF (all streams).
    pub first_riff_total: u64,
    /// Offset where the walk of the first `movi` stopped (idx1, end, damage).
    pub first_movi_end: u64,
    /// Offset where the first RIFF chunk ends: after its `idx1`, at the next
    /// `RIFF`, or where the walk stopped.
    pub first_riff_end: u64,
    /// How the last walk stopped.
    pub end: WalkEnd,
    /// Offset up to which the file is structurally sound.
    pub good_end: u64,
    /// The `idx1` chunk of the first RIFF.
    pub idx1: Option<ChunkHdr>,
    /// Number of AVIX continuations walked.
    pub avix: usize,
    /// Writers that omit pad bytes were seen.
    pub pad_missing: bool,
    /// Chunks that are not stream data: JUNK, ix##.
    pub other_chunks: u64,
}

impl Walked {
    fn new() -> Self {
        Walked {
            chunks: [0; 100],
            bytes: [0; 100],
            first_riff_chunks: [0; 100],
            first_riff_total: 0,
            first_movi_end: 0,
            first_riff_end: 0,
            end: WalkEnd::Clean(0),
            good_end: 0,
            idx1: None,
            avix: 0,
            pad_missing: false,
            other_chunks: 0,
        }
    }
}

/// Walk `movi` of the first RIFF and every AVIX continuation.
pub(crate) fn walk_all(win: &mut Window<'_>, head: &Head) -> Walked {
    let mut w = Walked::new();
    let avail = win.avail();
    let mut pos = head.movi_data();
    let mut first = true;
    loop {
        let end = {
            let wref = &mut w;
            walk_movi(win, pos, avail, |c| {
                if c.pad_missing {
                    wref.pad_missing = true;
                }
                match c.kind.stream() {
                    Some(i) if c.kind.is_data() => {
                        let i = usize::from(i);
                        wref.chunks[i] += 1;
                        wref.bytes[i] += u64::from(c.hdr.size);
                        if first {
                            wref.first_riff_chunks[i] += 1;
                            wref.first_riff_total += 1;
                        }
                    }
                    _ => {
                        if matches!(c.kind, IdKind::Junk | IdKind::OdmlIndex) {
                            wref.other_chunks += 1;
                        }
                    }
                }
            })
        };
        w.end = end;
        if first {
            w.first_movi_end = end.pos();
            w.first_riff_end = end.pos();
        }
        match end {
            WalkEnd::Idx1(p) => {
                let c = read_chunk(win, p);
                match c {
                    Some(c) if c.pos + 8 + u64::from(c.size) <= avail => {
                        if first {
                            w.idx1 = Some(c);
                            w.first_riff_end = c.next().min(avail);
                        }
                        w.good_end = c.next().min(avail);
                        // Continue with an AVIX if one follows.
                        if w.avix < MAX_AVIX {
                            if let Some(data) = avix_movi(win, w.good_end) {
                                pos = data;
                                w.avix += 1;
                                first = false;
                                continue;
                            }
                        }
                        break;
                    }
                    _ => {
                        w.good_end = p;
                        w.end = WalkEnd::Partial(p);
                        break;
                    }
                }
            }
            WalkEnd::Riff(p) => {
                w.good_end = p;
                if w.avix < MAX_AVIX {
                    if let Some(data) = avix_movi(win, p) {
                        pos = data;
                        w.avix += 1;
                        first = false;
                        continue;
                    }
                }
                break;
            }
            other => {
                w.good_end = other.pos();
                break;
            }
        }
    }
    w
}

/// Everything validation learns.
#[derive(Debug)]
pub(crate) struct Analysis {
    pub problems: Vec<String>,
    pub repairable: Vec<String>,
    pub meta: MediaMeta,
    pub intact: f32,
}

fn video_stream_indices(head: &Head) -> Vec<usize> {
    head.hdrl
        .streams
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_video())
        .map(|(i, _)| i)
        .collect()
}

/// Analyse a whole file. `None` when it is not an AVI.
pub(crate) fn analyse(src: &dyn Source) -> Option<Analysis> {
    let mut win = Window::new(src, 0, u64::MAX);
    let head = scan_head(&mut win)?;
    let walked = walk_all(&mut win, &head);
    let size = win.avail();
    let mut problems: Vec<String> = Vec::new();
    let mut rep_sizes = false;
    let mut rep_trunc = false;
    let mut rep_index = false;

    let tail_damage = !matches!(
        walked.end,
        WalkEnd::Idx1(_) | WalkEnd::Riff(_) | WalkEnd::Clean(_)
    ) || matches!(walked.end, WalkEnd::Clean(p) if p < size);
    match walked.end {
        WalkEnd::Garbage(p) => {
            problems.push(format!(
                "invalid chunk header at offset {p} ({} of {size} bytes intact)",
                walked.good_end
            ));
        }
        WalkEnd::Partial(p) => {
            problems.push(format!(
                "file ends inside a chunk starting at offset {p} (truncated)"
            ));
        }
        WalkEnd::Fault(p) => {
            problems.push(format!("read error near offset {p}"));
        }
        _ => {}
    }
    if tail_damage {
        rep_trunc = true;
        rep_index = true;
    }

    let has_index_flag = head.hdrl.avih.as_ref().is_some_and(|a| a.flags & 0x10 != 0);

    // The RIFF size field(s).
    let first_riff_end = if walked.avix > 0 {
        None
    } else {
        Some(walked.good_end)
    };
    if let Some(end) = first_riff_end {
        let declared = 8 + u64::from(head.riff_size);
        if !tail_damage && declared != end && declared + 1 != end && declared != end + 1 {
            problems.push(format!(
                "RIFF size field says {declared} bytes, the file structure ends at {end}"
            ));
            rep_sizes = true;
        }
        if !tail_damage && end != size && walked.idx1.is_some() {
            problems.push(format!("{} extra bytes after the index", size - end));
            rep_trunc = true;
        }
    }

    // The movi list size.
    let movi_declared_end = head.movi_declared_end();
    if matches!(walked.end, WalkEnd::Idx1(_) | WalkEnd::Riff(_)) || walked.avix == 0 {
        let actual = walked.first_movi_end;
        if !tail_damage
            && walked.avix == 0
            && movi_declared_end != actual
            && movi_declared_end + 1 != actual
        {
            problems.push(format!(
                "LIST movi size says it ends at {movi_declared_end}, the chunks end at {actual}"
            ));
            rep_sizes = true;
        }
    }

    // Frame counts of video streams.
    let video = video_stream_indices(&head);
    let total_video: u64 = video.iter().map(|&i| walked.chunks[i.min(99)]).sum::<u64>();
    if let Some(a) = &head.hdrl.avih {
        let first_video: u64 = video
            .iter()
            .map(|&i| walked.first_riff_chunks[i.min(99)])
            .sum();
        if !video.is_empty() && u64::from(a.total_frames) != first_video && !tail_damage {
            problems.push(format!(
                "avih.dwTotalFrames is {} but the first RIFF holds {first_video} video chunks",
                a.total_frames
            ));
            rep_sizes = true;
        } else if tail_damage && u64::from(a.total_frames) != first_video && !video.is_empty() {
            rep_sizes = true;
        }
    }
    if let Some((_, v)) = head.hdrl.dmlh {
        if !video.is_empty() && u64::from(v) != total_video && !tail_damage {
            problems.push(format!(
                "dmlh total frames is {v} but the file holds {total_video} video chunks"
            ));
            rep_sizes = true;
        }
    }
    for &vi in &video {
        let s = &head.hdrl.streams[vi];
        let n = walked.chunks[vi.min(99)];
        let expect_total = if head.hdrl.dmlh.is_some() || walked.avix == 0 {
            n
        } else {
            walked.first_riff_chunks[vi.min(99)]
        };
        let l = u64::from(s.length);
        if l != n && l != expect_total && !tail_damage {
            problems.push(format!(
                "stream {vi} header says {l} frames but {n} video chunks exist"
            ));
            rep_sizes = true;
        }
    }

    // The index.
    match walked.idx1 {
        None => {
            if !tail_damage {
                // A file that says it has no index and has none is a valid,
                // if slow to seek, AVI.
                if has_index_flag {
                    problems.push("avih says the file has an index but there is no idx1".into());
                    rep_index = true;
                }
            } else if has_index_flag {
                problems.push("the idx1 index was lost".into());
            }
        }
        Some(ix) => {
            let n = u64::from(ix.size) / 16;
            if u64::from(ix.size) % 16 != 0 {
                problems.push(format!("idx1 size {} is not a multiple of 16", ix.size));
                rep_index = true;
            }
            check_idx1(
                &mut win,
                &head,
                &walked,
                ix,
                n,
                &mut problems,
                &mut rep_index,
            );
        }
    }

    if walked.pad_missing {
        problems.push("odd-sized chunks are not padded to even length".into());
        rep_index = true;
    }

    let mut meta = build_meta(
        &head,
        walked.avix,
        if tail_damage || head.hdrl.avih.as_ref().is_none_or(|a| a.total_frames == 0) {
            total_video
        } else {
            0
        },
        walked.idx1.is_some(),
    );
    if tail_damage {
        meta.extra
            .insert("damage".into(), "truncated or damaged movi".into());
    }
    let mut repairable = Vec::new();
    if !problems.is_empty() {
        if rep_sizes && !rep_index && !rep_trunc {
            repairable.push(FIX_SIZES.to_string());
        }
        if rep_trunc || tail_damage {
            repairable.push(TRUNCATE.to_string());
        }
        repairable.push(REBUILD.to_string());
    }
    let intact = if problems.is_empty() {
        1.0
    } else {
        (walked.good_end as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
    };
    Some(Analysis {
        problems,
        repairable,
        meta,
        intact,
    })
}

fn check_idx1(
    win: &mut Window<'_>,
    head: &Head,
    walked: &Walked,
    ix: ChunkHdr,
    n: u64,
    problems: &mut Vec<String>,
    rep_index: &mut bool,
) {
    // Entries in the index vs data chunks of the first RIFF.
    let expected = walked.first_riff_total;
    let base = head.movi_pos + 8;
    let mut bad = 0u64;
    let mut first_bad: Option<(u64, String)> = None;
    let mut counted = 0u64;
    let mut last_off: Option<u64> = None;
    let mut convention: Option<bool> = None; // true = relative to movi tag
    let mut cur = Idx1Cursor::new(ix.data(), n);
    while let Some(e) = cur.next(win) {
        let i = cur.position() - 1;
        if matches!(classify(e.id), Some(IdKind::List)) || &e.id == b"rec " {
            continue;
        }
        counted += 1;
        let rel = base + u64::from(e.offset);
        let abs = u64::from(e.offset);
        let matches_at = |win: &mut Window<'_>, at: u64| -> bool {
            read_chunk(win, at).is_some_and(|c| c.id == e.id && c.size == e.size)
        };
        let ok = match convention {
            Some(true) => matches_at(win, rel).then_some(rel),
            Some(false) => matches_at(win, abs).then_some(abs),
            None => {
                if matches_at(win, rel) {
                    convention = Some(true);
                    Some(rel)
                } else if matches_at(win, abs) {
                    convention = Some(false);
                    Some(abs)
                } else {
                    None
                }
            }
        };
        match ok {
            Some(at) => {
                if let Some(prev) = last_off {
                    if at <= prev {
                        bad += 1;
                        first_bad.get_or_insert((i, "entries are out of order".into()));
                    }
                }
                last_off = Some(at);
            }
            None => {
                bad += 1;
                first_bad.get_or_insert((
                    i,
                    format!(
                        "does not match a '{}' chunk of {} bytes at offset {}",
                        crate::io::fourcc_string(e.id),
                        e.size,
                        e.offset
                    ),
                ));
            }
        }
    }
    if convention == Some(false) {
        problems.push("idx1 uses absolute file offsets (non-standard)".into());
        *rep_index = true;
    }
    if bad > 0 {
        let (i, why) = first_bad.unwrap_or((0, String::new()));
        problems.push(format!(
            "idx1 has {bad} bad entries; first is entry {i}, {why}"
        ));
        *rep_index = true;
    }
    if counted != expected {
        problems.push(format!(
            "idx1 lists {counted} chunks but movi holds {expected}"
        ));
        *rep_index = true;
    }
}

/// Turn an analysis into the public [`Validation`].
pub(crate) fn to_validation(a: Analysis) -> Validation {
    let status = if a.problems.is_empty() {
        ValidationStatus::Valid
    } else {
        ValidationStatus::Damaged
    };
    Validation {
        status,
        intact_fraction: Some(a.intact),
        problems: a.problems,
        meta: a.meta,
        repairable_by: a.repairable,
    }
}

/// Used by tests to look at stream layout.
#[allow(dead_code)]
pub(crate) fn stream_is_pcm_like(f: &StreamFormat) -> bool {
    matches!(f, StreamFormat::Audio { tag: 1, .. })
}
