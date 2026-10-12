//! Top-level AVI structure: the RIFF header, the leading lists, the extent of
//! the file (RIFF sizes, `movi`, `idx1`, OpenDML `AVIX` continuations) and the
//! media metadata that can be derived from them.

use diskrescue_core::format::{Confidence, ExtentStatus};
use diskrescue_core::media::MediaMeta;

use crate::io::{fourcc_string, Window};

use super::hdr::{
    audio_codec, created_from, parse_hdrl, parse_info, read_chunk, video_codec, ChunkHdr, Hdrl,
    Info, StreamFormat,
};
use super::walk::{classify, walk_movi, IdKind, Idx1Cursor, WalkEnd};

/// Upper bound on RIFF-AVIX continuation chunks (64 GiB / 1 GiB, with slack).
pub(crate) const MAX_AVIX: usize = 4096;
/// How far past the start of `hdrl` we look for `LIST movi` when the `hdrl`
/// size field cannot be trusted.
const MOVI_SEARCH: u64 = 4 * 1024 * 1024;

/// Everything before the first chunk of `movi`.
#[derive(Debug, Clone)]
pub(crate) struct Head {
    /// The RIFF size field.
    pub riff_size: u32,
    pub hdrl: Hdrl,
    pub info: Info,
    /// File offset of the `LIST` header of the first `movi`.
    pub movi_pos: u64,
    /// The `LIST` size field of the first `movi`.
    pub movi_size: u32,
}

impl Head {
    /// First byte after the `movi` fourcc.
    pub fn movi_data(&self) -> u64 {
        self.movi_pos + 12
    }
    /// Declared end of the first `movi` list.
    pub fn movi_declared_end(&self) -> u64 {
        self.movi_pos + 8 + u64::from(self.movi_size)
    }
}

/// Quick check of the first 12 bytes.
pub(crate) fn is_avi_header(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"AVI "
}

/// Parse the leading structure. `None` means "not an AVI (or no `movi`)".
pub(crate) fn scan_head(win: &mut Window<'_>) -> Option<Head> {
    let hdr = win.exact(0, 12)?;
    if !is_avi_header(hdr) {
        return None;
    }
    let riff_size = win.u32le(4)?;
    let mut hdrl: Option<Hdrl> = None;
    let mut info = Info::default();
    let mut pos = 12u64;
    for _ in 0..64 {
        let c = read_chunk(win, pos)?;
        if &c.id == b"LIST" {
            let ty = win.fourcc(c.data())?;
            let list_end = c.pos + 8 + u64::from(c.size);
            match &ty {
                b"hdrl" if hdrl.is_none() => {
                    hdrl = Some(parse_hdrl(win, c.pos, list_end));
                }
                b"INFO" => info = parse_info(win, c.pos, list_end),
                b"movi" => {
                    return Some(Head {
                        riff_size,
                        hdrl: hdrl?,
                        info,
                        movi_pos: c.pos,
                        movi_size: c.size,
                    });
                }
                _ => {}
            }
            if &ty == b"hdrl" && !(c.size >= 4 && list_end < win.avail()) {
                // The hdrl size cannot be right; look for movi directly.
                break;
            }
        } else if classify(c.id).is_none() && !c.id.iter().all(|b| b.is_ascii_graphic()) {
            break;
        }
        pos = c.next();
    }
    let hdrl = hdrl?;
    let movi_pos = find_movi(win, hdrl.list_pos + 12)?;
    let c = read_chunk(win, movi_pos)?;
    Some(Head {
        riff_size,
        hdrl,
        info,
        movi_pos,
        movi_size: c.size,
    })
}

/// Search for the bytes `LIST ???? movi` starting at `from`.
fn find_movi(win: &mut Window<'_>, from: u64) -> Option<u64> {
    let mut p = from;
    let end = from.saturating_add(MOVI_SEARCH).min(win.avail());
    while p + 12 <= end {
        let block = win.get(p, 64 * 1024).to_vec();
        if block.len() < 12 {
            return None;
        }
        for i in 0..=(block.len() - 12) {
            if &block[i..i + 4] == b"LIST" && &block[i + 8..i + 12] == b"movi" {
                return Some(p + i as u64);
            }
        }
        p += (block.len() - 11) as u64;
    }
    None
}

/// Result of working out where the AVI ends.
#[derive(Debug, Clone)]
pub(crate) struct Extent {
    pub len: u64,
    pub status: ExtentStatus,
    pub err_offset: Option<u64>,
    /// Sizes were verified structurally without walking every chunk.
    pub trusted: bool,
    /// The `idx1` chunk of the first RIFF, if found.
    pub idx1: Option<ChunkHdr>,
    /// Number of `AVIX` continuations included.
    pub avix: usize,
    /// Video chunks counted by a full walk (0 when not walked).
    pub walked_video_frames: u64,
    pub notes: Vec<String>,
}

fn even(v: u64) -> u64 {
    v + (v & 1)
}

/// Determine the extent. Uses the size fields when they tile the file
/// exactly, otherwise walks `movi` chunk by chunk.
pub(crate) fn extent(win: &mut Window<'_>, head: &Head) -> Extent {
    if let Some(e) = trusted_extent(win, head) {
        log::trace!("AVI: size fields tile the file, extent {}", e.len);
        return e;
    }
    log::debug!("AVI: size fields do not tile the file; walking movi chunk by chunk");
    let e = walked_extent(win, head);
    log::debug!(
        "AVI: walk ended at {} ({:?}, {} AVIX)",
        e.len,
        e.status,
        e.avix
    );
    e
}

/// The first few chunks of a `movi` list (`movi_data..movi_end`) must be
/// plausible chunks that fit in the list.
fn first_chunks_ok(win: &mut Window<'_>, movi_data: u64, movi_end: u64) -> bool {
    let mut pos = movi_data;
    let mut seen = 0;
    while seen < 32 && pos < movi_end {
        let Some(c) = read_chunk(win, pos) else {
            return false;
        };
        match classify(c.id) {
            None | Some(IdKind::Idx1 | IdKind::Riff) => return false,
            Some(IdKind::List) => {
                if c.size < 4 {
                    return false;
                }
                if win.fourcc(c.data()) == Some(*b"rec ") {
                    pos += 12;
                    seen += 1;
                    continue;
                }
            }
            Some(_) => {}
        }
        if c.pos + 8 + u64::from(c.size) > movi_end {
            return false;
        }
        pos = c.next();
        seen += 1;
    }
    true
}

fn trusted_extent(win: &mut Window<'_>, head: &Head) -> Option<Extent> {
    let riff_size = u64::from(head.riff_size);
    if head.riff_size == 0 || head.riff_size == u32::MAX || riff_size < 4 + 8 {
        return None;
    }
    let avail = win.avail();
    let riff_end = even(8 + riff_size);
    // The final pad byte of the RIFF chunk may be absent.
    let riff_end = if riff_end > avail && 8 + riff_size <= avail {
        8 + riff_size
    } else {
        riff_end
    };
    if riff_end > avail || head.movi_size < 4 {
        return None;
    }
    let movi_end = head.movi_declared_end();
    if movi_end > riff_end || !first_chunks_ok(win, head.movi_data(), movi_end) {
        return None;
    }
    // Chunks between the end of `movi` and the end of the RIFF chunk must
    // tile it exactly.
    let mut pos = if movi_end == riff_end {
        movi_end
    } else {
        even(movi_end)
    };
    let mut idx1 = None;
    let mut guard = 0;
    while pos + 8 <= riff_end && guard < 256 {
        guard += 1;
        let c = read_chunk(win, pos)?;
        if !c.id.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return None;
        }
        if &c.id == b"idx1" && idx1.is_none() {
            idx1 = Some(c);
        }
        if pos + 8 + u64::from(c.size) > riff_end {
            return None;
        }
        pos = c.next();
    }
    if pos != riff_end {
        return None;
    }
    if let Some(ix) = idx1 {
        if !idx1_spot_check(win, head, &ix) {
            return None;
        }
    }
    let mut len = riff_end;
    let mut avix = 0usize;
    let mut notes = Vec::new();
    while avix < MAX_AVIX {
        let Some(h) = win.exact(len, 12) else { break };
        if &h[0..4] != b"RIFF" || &h[8..12] != b"AVIX" {
            break;
        }
        let sz = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
        let end = even(len + 8 + sz);
        if sz < 4 + 12 || end > avail {
            // A cut or damaged continuation: let the walker sort it out.
            return None;
        }
        let m = read_chunk(win, len + 12)?;
        if &m.id != b"LIST" || win.fourcc(m.data()) != Some(*b"movi") {
            return None;
        }
        if m.pos + 8 + u64::from(m.size) > end
            || !first_chunks_ok(win, m.pos + 12, m.pos + 8 + u64::from(m.size))
        {
            return None;
        }
        len = end;
        avix += 1;
    }
    if avix > 0 {
        notes.push(format!("OpenDML: {avix} AVIX continuation chunk(s)"));
    }
    Some(Extent {
        len,
        status: ExtentStatus::Complete,
        err_offset: None,
        trusted: true,
        idx1,
        avix,
        walked_video_frames: 0,
        notes,
    })
}

/// Check the first and last `idx1` entries against the chunks they name.
fn idx1_spot_check(win: &mut Window<'_>, head: &Head, ix: &ChunkHdr) -> bool {
    let n = u64::from(ix.size) / 16;
    if n == 0 {
        return true;
    }
    let base = head.movi_pos + 8;
    let check = |win: &mut Window<'_>, i: u64| -> bool {
        let Some(e) = Idx1Cursor::at(ix.data(), n, i).next(win) else {
            return false;
        };
        if matches!(classify(e.id), Some(IdKind::List)) {
            return true;
        }
        let abs = base + u64::from(e.offset);
        let rel_ok = |win: &mut Window<'_>, at: u64| -> bool {
            read_chunk(win, at).is_some_and(|c| c.id == e.id && c.size == e.size)
        };
        rel_ok(win, abs) || rel_ok(win, u64::from(e.offset))
    };
    check(win, 0) && check(win, n - 1)
}

fn walked_extent(win: &mut Window<'_>, head: &Head) -> Extent {
    let video_idx = head
        .hdrl
        .streams
        .iter()
        .position(|s| s.is_video())
        .map(|i| i as u8);
    let mut frames = 0u64;
    let mut notes = Vec::new();
    let mut idx1: Option<ChunkHdr> = None;
    let mut pos = head.movi_data();
    let mut avix = 0usize;
    let avail = win.avail();
    let mut first_riff = true;
    let (len, status, err) = loop {
        let end = walk_movi(win, pos, avail, |c| {
            if let IdKind::Video(i) = c.kind {
                if Some(i) == video_idx || video_idx.is_none() {
                    frames += 1;
                }
            }
        });
        match end {
            WalkEnd::Idx1(p) => {
                let Some(c) = read_chunk(win, p) else {
                    break (p, ExtentStatus::Truncated, Some(p));
                };
                let c_end = c.next();
                if c.pos + 8 + u64::from(c.size) > avail {
                    notes.push("idx1 index is cut short".to_string());
                    break (p, ExtentStatus::Truncated, Some(p));
                }
                if first_riff {
                    idx1 = Some(c);
                }
                // Anything between idx1 and a following AVIX is skipped.
                let mut q = c_end.min(avail);
                let mut found = false;
                if avix < MAX_AVIX {
                    if let Some(h) = win.exact(q, 12) {
                        if &h[0..4] == b"RIFF" && &h[8..12] == b"AVIX" {
                            found = true;
                        }
                    }
                }
                if !found {
                    // Clean end of the index; the RIFF size field was wrong.
                    q = c_end.min(avail);
                    notes.push("RIFF size field does not match the file; extent from walk".into());
                    break (q, ExtentStatus::Complete, None);
                }
                match avix_movi(win, q) {
                    Some(data) => {
                        pos = data;
                        avix += 1;
                        first_riff = false;
                    }
                    None => break (q, ExtentStatus::Truncated, Some(q)),
                }
            }
            WalkEnd::Riff(p) => {
                let is_avix = win.exact(p, 12).is_some_and(|h| &h[8..12] == b"AVIX");
                if is_avix && avix < MAX_AVIX {
                    match avix_movi(win, p) {
                        Some(data) => {
                            pos = data;
                            avix += 1;
                            first_riff = false;
                            continue;
                        }
                        None => break (p, ExtentStatus::Truncated, Some(p)),
                    }
                }
                notes.push("no idx1 index before the next RIFF chunk".into());
                break (p, ExtentStatus::Complete, None);
            }
            WalkEnd::Clean(p) => {
                if win.limited() {
                    break (p, ExtentStatus::Capped, None);
                }
                notes.push("movi list not terminated (no idx1, file ends)".into());
                break (p, ExtentStatus::Truncated, Some(p));
            }
            WalkEnd::Garbage(p) => {
                notes.push(format!("invalid chunk header at offset {p}"));
                break (p, ExtentStatus::Truncated, Some(p));
            }
            WalkEnd::Partial(p) => {
                notes.push(format!(
                    "chunk at offset {p} extends past the available data"
                ));
                break (p, ExtentStatus::Truncated, Some(p));
            }
            WalkEnd::Fault(p) => {
                notes.push(format!("read error near offset {p}"));
                break (p, ExtentStatus::Truncated, Some(p));
            }
        }
    };
    Extent {
        len,
        status,
        err_offset: err,
        trusted: false,
        idx1,
        avix,
        walked_video_frames: frames,
        notes,
    }
}

/// Position of the first chunk inside `movi` of the `RIFF AVIX` chunk at `p`.
pub(crate) fn avix_movi(win: &mut Window<'_>, p: u64) -> Option<u64> {
    let h = win.exact(p, 12)?;
    if &h[0..4] != b"RIFF" || &h[8..12] != b"AVIX" {
        return None;
    }
    // Skip JUNK or other chunks until `LIST movi`.
    let mut q = p + 12;
    for _ in 0..16 {
        let c = read_chunk(win, q)?;
        if &c.id == b"LIST" && win.fourcc(c.data()) == Some(*b"movi") {
            return Some(c.pos + 12);
        }
        classify(c.id)?;
        q = c.next();
    }
    None
}

/// Confidence for a measured AVI.
pub(crate) fn confidence(head: &Head, e: &Extent) -> Confidence {
    let base = match (e.status, e.trusted) {
        (ExtentStatus::Complete, true) => Confidence::HIGH,
        (ExtentStatus::Complete, false) => Confidence::HIGH.lowered(10),
        (ExtentStatus::Capped, _) => Confidence::MEDIUM,
        _ => Confidence::MEDIUM,
    };
    if head.hdrl.avih.is_none() || head.hdrl.streams.is_empty() {
        base.lowered(25)
    } else {
        base
    }
}

/// Build metadata from the parsed headers. `walked_frames` (when nonzero)
/// replaces header frame counts that are zero or obviously unfinished.
pub(crate) fn build_meta(
    head: &Head,
    e_avix: usize,
    walked_frames: u64,
    has_idx1: bool,
) -> MediaMeta {
    let mut m = MediaMeta::default();
    let h = &head.hdrl;
    let video = h.streams.iter().find(|s| s.is_video());
    let audio = h.streams.iter().find(|s| s.is_audio());

    if let Some(a) = &h.avih {
        if a.width > 0 && a.height > 0 && a.width < 100_000 && a.height < 100_000 {
            m.width = Some(a.width);
            m.height = Some(a.height);
        }
    }
    if let Some(v) = video {
        if let StreamFormat::Video { width, height, .. } = &v.format {
            if m.width.is_none() && *width > 0 && *height != 0 && width.abs() < 100_000 {
                m.width = Some(*width as u32);
                m.height = Some(height.unsigned_abs());
            }
        }
        let (name, _) = video_codec(v);
        m.video_codec = Some(name);
        if let StreamFormat::Video { compression, .. } = &v.format {
            if compression.iter().any(|&b| b != 0) {
                m.extra.insert("fourcc".into(), fourcc_string(*compression));
            }
        }
        if v.handler.iter().any(|&b| b != 0) {
            m.extra.insert("handler".into(), fourcc_string(v.handler));
        }
    }
    if let Some(a) = audio {
        if let StreamFormat::Audio {
            tag,
            channels,
            sample_rate,
            bits,
            ..
        } = &a.format
        {
            m.audio_codec = Some(audio_codec(*tag, *bits));
            m.extra
                .insert("audio_channels".into(), channels.to_string());
            m.extra
                .insert("audio_sample_rate".into(), sample_rate.to_string());
        }
    }
    // Frame count and rate.
    let mut frames = h
        .dmlh
        .map(|(_, v)| u64::from(v))
        .filter(|&v| v > 0)
        .or_else(|| video.map(|v| u64::from(v.length)).filter(|&v| v > 0))
        .or_else(|| {
            h.avih
                .as_ref()
                .map(|a| u64::from(a.total_frames))
                .filter(|&v| v > 0)
        })
        .unwrap_or(0);
    if walked_frames > 0 && (frames == 0 || walked_frames > frames) {
        frames = walked_frames;
    }
    let fps = video
        .filter(|v| v.scale > 0 && v.rate > 0)
        .map(|v| f64::from(v.rate) / f64::from(v.scale))
        .or_else(|| {
            h.avih
                .as_ref()
                .filter(|a| a.usec_per_frame > 0)
                .map(|a| 1_000_000.0 / f64::from(a.usec_per_frame))
        })
        .filter(|f| (0.1..=1000.0).contains(f));
    if let Some(f) = fps {
        m.extra.insert("fps".into(), format!("{f:.3}"));
        if frames > 0 {
            m.duration_ms = Some((frames as f64 / f * 1000.0).round() as u64);
        }
    }
    if frames > 0 {
        m.extra.insert("frames".into(), frames.to_string());
    }
    m.extra
        .insert("streams".into(), h.streams.len().to_string());
    m.extra.insert(
        "index".into(),
        if has_idx1 { "idx1" } else { "none" }.into(),
    );
    if e_avix > 0 || h.streams.iter().any(|s| s.indx_pos.is_some()) || h.dmlh.is_some() {
        m.extra.insert("opendml".into(), "true".into());
    }
    m.software = head.info.software.clone();
    m.created = created_from(&head.info, h);
    if let Some(t) = &head.info.title {
        m.extra.insert("title".into(), t.clone());
    }
    m
}
