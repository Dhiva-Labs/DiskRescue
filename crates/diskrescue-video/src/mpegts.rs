//! MPEG transport streams: 188-byte TS and 192-byte M2TS/AVCHD (`.mts`).
//!
//! # Algorithm
//!
//! A transport stream has no header and no trailer, so the extent is a *run*:
//! the stride is detected from the first packets (`0x47` every 188 bytes, or
//! every 192 bytes with the sync byte at offset 4 inside a 4-byte
//! `TP_extra_header`), and packets are counted while the sync byte holds. The
//! first sync miss ends the run; the status is `UnknownLength` because the
//! bytes after it may be damage or the next file. A candidate needs at least
//! 16 packets, each with a valid adaptation field control, and at least one
//! packet on a PID other than the null PID.
//!
//! While walking, continuity counters are tracked per PID; gaps lower the
//! confidence but do not end the run (recorders drop packets).
//!
//! Within the first 1024 packets the PAT is located (its CRC-32 is checked),
//! then the PMT, giving the elementary streams and the PCR PID. A PAT in that
//! window gives `Confidence::HIGH`, otherwise `MEDIUM`. The picture size is
//! read from the first H.264/HEVC SPS or MPEG-2 sequence header in the video
//! PID. Duration is the difference between the first PCR in the head and the
//! last PCR in the final 1024 packets (PES timestamps when there is no PCR).
//!
//! Known unreadable ranges ([`Source::unreadable`], e.g. from a ddrescue
//! mapfile) do not end the run: a hole read as zeros (or failing to read)
//! breaks the sync byte, but the packets after it still sit on the same
//! grid. When the walk stops in a packet that a known hole covers, it skips
//! to the end of the hole, resynchronises on the 188/192-byte grid (the
//! next [`RESYNC_PACKETS`] sync bytes after the hole must hold) and carries
//! on with the same run; the holes are listed in a note. A camera `.MTS`
//! with bad sectors therefore comes back as one file instead of two pieces
//! with the packets between the hole and the next aligned packet in
//! neither.
//!
//! Limits: stream-level damage inside a run (garbage that keeps the stride)
//! is not detected by `measure`; `validate` reports transport errors and
//! continuity gaps. A sync miss that no known hole explains ends the run.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Range, RangeSet, Result, SharedSource, Source};

use crate::io::{note_unreadable, Window};
use crate::meta::{
    crc32_mpeg, find_mpeg_sequence_header, mpeg_frame_rate, picture_from_annexb, PictureInfo,
    EXTENSION_KEY,
};

const MAX_SIZE: u64 = 64 << 30;
/// Minimum packets for a hit (false-positive guard).
const MIN_PACKETS: u64 = 16;
/// Packets examined for PAT/PMT/PCR and the elementary stream prefix.
const HEAD_PACKETS: u64 = 1024;
/// Packets read in one block while walking the run.
const BLOCK_PACKETS: usize = 4096;
/// Largest elementary stream prefix collected for the SPS search.
const ES_PREFIX: usize = 128 * 1024;
/// Consecutive sync bytes that must hold after a known hole before the run
/// continues there.
const RESYNC_PACKETS: u64 = 4;
/// Packets examined while resynchronising after a hole (sync bytes inside
/// further holes are skipped, not counted).
const RESYNC_SCAN: u64 = 64;
/// Holes listed by name in the note (the rest are counted).
const MAX_LISTED_HOLES: usize = 16;

/// What a program stream element is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EsKind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone)]
struct Es {
    pid: u16,
    stream_type: u8,
    kind: EsKind,
    codec: String,
}

fn classify_stream(stream_type: u8, descriptors: &[u8]) -> (EsKind, String) {
    let has = |tag: u8| {
        let mut p = 0;
        while p + 2 <= descriptors.len() {
            if descriptors[p] == tag {
                return true;
            }
            p += 2 + descriptors[p + 1] as usize;
        }
        false
    };
    match stream_type {
        0x01 => (EsKind::Video, "mpeg1video".into()),
        0x02 => (EsKind::Video, "mpeg2video".into()),
        0x10 => (EsKind::Video, "mpeg4".into()),
        0x1B => (EsKind::Video, "h264".into()),
        0x24 => (EsKind::Video, "hevc".into()),
        0xEA => (EsKind::Video, "vc1".into()),
        0x03 | 0x04 => (EsKind::Audio, "mpeg_audio".into()),
        0x0F => (EsKind::Audio, "aac".into()),
        0x11 => (EsKind::Audio, "aac_latm".into()),
        0x81 => (EsKind::Audio, "ac3".into()),
        0x80 => (EsKind::Audio, "pcm".into()),
        0x82 | 0x86 | 0x8A => (EsKind::Audio, "dts".into()),
        0x87 => (EsKind::Audio, "eac3".into()),
        0x06 => {
            if has(0x6A) {
                (EsKind::Audio, "ac3".into())
            } else if has(0x7A) {
                (EsKind::Audio, "eac3".into())
            } else if has(0x7B) {
                (EsKind::Audio, "dts".into())
            } else {
                (EsKind::Other, "private".into())
            }
        }
        0x90 => (EsKind::Other, "pgs".into()),
        t => (EsKind::Other, format!("type_0x{t:02x}")),
    }
}

/// Detect the packet layout from the first bytes: `(stride, offset of sync)`.
fn detect_layout(head: &[u8]) -> Option<(usize, usize)> {
    for (stride, off) in [(188usize, 0usize), (192, 4)] {
        let need = if head.len() > off + 2 * stride { 3 } else { 2 };
        let mut ok = 0;
        for k in 0..3 {
            let p = off + k * stride;
            let Some(&b) = head.get(p) else { break };
            if b != 0x47 {
                ok = 0;
                break;
            }
            ok += 1;
        }
        if ok >= need && head.len() > off + stride {
            return Some((stride, off));
        }
    }
    None
}

/// Statistics of one run of packets.
#[derive(Debug, Default)]
struct Run {
    packets: u64,
    tei: u64,
    cc_errors: u64,
    bad_afc: u64,
    /// The first 16 packets look like transport packets.
    plausible_start: bool,
    /// The run stopped on a sync miss (as opposed to the end of data).
    sync_miss: bool,
    /// Known unreadable ranges the run was carried across (file-relative).
    holes: Vec<Range>,
}

impl Run {
    /// Whether `off` lies in a hole the run was carried across.
    fn skipped(&self, off: u64) -> bool {
        self.holes.iter().any(|h| h.contains(off))
    }

    /// Note listing the holes the run was carried across.
    fn holes_note(&self) -> Option<String> {
        if self.holes.is_empty() {
            return None;
        }
        let mut list: Vec<String> = self
            .holes
            .iter()
            .take(MAX_LISTED_HOLES)
            .map(|h| format!("{}..{}", h.start, h.end))
            .collect();
        if self.holes.len() > MAX_LISTED_HOLES {
            list.push(format!("{} more", self.holes.len() - MAX_LISTED_HOLES));
        }
        Some(format!(
            "packet sync resumed after {} unreadable range(s) at file offsets {}",
            self.holes.len(),
            list.join(", ")
        ))
    }
}

/// Where the run that stopped in the packet at `pos` continues after the
/// known hole `h` (file-relative): the first packet on the grid after `pos`
/// whose sync byte lies past the hole, provided [`RESYNC_PACKETS`] sync
/// bytes from there on hold. Sync bytes inside further holes are skipped; a
/// readable one that is not `0x47` fails.
fn resync(
    win: &mut Window<'_>,
    holes: &RangeSet,
    h: Range,
    pos: u64,
    stride: u64,
    off: u64,
) -> Option<u64> {
    let first = (h.end.saturating_sub(off).div_ceil(stride) * stride).max(pos + stride);
    let mut verified = 0u64;
    for k in 0..RESYNC_SCAN {
        let q = first + k * stride + off;
        if holes.contains(q) {
            continue;
        }
        match win.u8(q) {
            Some(0x47) => verified += 1,
            _ => return None,
        }
        if verified == RESYNC_PACKETS {
            return Some(first);
        }
    }
    None
}

fn walk_run(win: &mut Window<'_>, stride: usize, off: usize, holes: &RangeSet) -> Run {
    let mut run = Run::default();
    let mut cc_last = vec![0xFFu8; 8192];
    let mut non_null_in_start = false;
    let mut start_ok = true;
    let block = stride * BLOCK_PACKETS;
    // File-relative start of the next packet; always on the grid.
    let mut pos = 0u64;
    loop {
        let data = win.get(pos, block);
        let full = data.len() == block;
        let whole = data.len() / stride;
        let done = pos / stride as u64;
        let mut miss = false;
        let mut i = 0usize;
        while i < whole {
            let base = i * stride + off;
            let p = &data[base..base + 4];
            if p[0] != 0x47 {
                miss = true;
                break;
            }
            let pid = (usize::from(p[1] & 0x1F) << 8) | usize::from(p[2]);
            let afc = (p[3] >> 4) & 3;
            let cc = p[3] & 0x0F;
            if p[1] & 0x80 != 0 {
                run.tei += 1;
            }
            if afc == 0 {
                run.bad_afc += 1;
                if done + (i as u64) < MIN_PACKETS {
                    start_ok = false;
                }
            }
            if pid != 0x1FFF {
                if done + (i as u64) < MIN_PACKETS {
                    non_null_in_start = true;
                }
                let has_payload = afc & 1 != 0;
                let last = cc_last[pid];
                if last != 0xFF {
                    let expected = if has_payload { (last + 1) & 0x0F } else { last };
                    if cc != expected && !(has_payload && cc == last) {
                        let discontinuity = afc & 2 != 0
                            && data.get(base + 5).is_some_and(|f| f & 0x80 != 0)
                            && data.get(base + 4).is_some_and(|&l| l > 0);
                        if !discontinuity {
                            run.cc_errors += 1;
                        }
                    }
                }
                cc_last[pid] = cc;
            }
            i += 1;
        }
        pos += (i * stride) as u64;
        if !miss && whole > 0 && full {
            continue;
        }
        // The run stops at `pos`: a sync miss, a short read or the end of
        // the window. A known hole in that packet does not end the stream
        // when the grid holds again after it.
        let hole = holes.first_intersecting(Range::new(pos, pos + stride as u64));
        let explained = hole.filter(|_| !miss || holes.contains(pos + off as u64));
        if let Some(h) = explained {
            if let Some(next) = resync(win, holes, h, pos, stride as u64, off as u64) {
                log::debug!(
                    "MPEG-TS: resynchronised at {next} after unreadable bytes {}..{}",
                    h.start,
                    h.end
                );
                run.holes.push(h);
                // Packets lost in the hole break continuity as expected.
                cc_last.fill(0xFF);
                win.forget_fault_before(next);
                pos = next;
                continue;
            }
        }
        run.sync_miss = miss;
        break;
    }
    run.packets = pos / stride as u64;
    run.plausible_start = start_ok && non_null_in_start;
    run
}

/// One transport packet's header fields and payload offset inside `data`.
struct Pkt<'a> {
    pid: u16,
    pusi: bool,
    afc: u8,
    data: &'a [u8], // the 188 bytes
}

impl<'a> Pkt<'a> {
    fn payload(&self) -> &'a [u8] {
        if self.afc & 1 == 0 {
            return &[];
        }
        let mut start = 4usize;
        if self.afc & 2 != 0 {
            start = 5 + self.data.get(4).copied().unwrap_or(0) as usize;
        }
        self.data.get(start..).unwrap_or(&[])
    }

    /// PCR in 27 MHz units, when present.
    fn pcr(&self) -> Option<u64> {
        if self.afc & 2 == 0 || self.data.len() < 12 {
            return None;
        }
        let alen = self.data[4] as usize;
        if alen < 7 || self.data[5] & 0x10 == 0 {
            return None;
        }
        let b = &self.data[6..12];
        let base = (u64::from(b[0]) << 25)
            | (u64::from(b[1]) << 17)
            | (u64::from(b[2]) << 9)
            | (u64::from(b[3]) << 1)
            | (u64::from(b[4]) >> 7);
        let ext = (u64::from(b[4] & 1) << 8) | u64::from(b[5]);
        Some(base * 300 + ext)
    }
}

fn packets<'a>(buf: &'a [u8], stride: usize, off: usize) -> impl Iterator<Item = Pkt<'a>> + 'a {
    buf.chunks_exact(stride).filter_map(move |c| {
        let d = c.get(off..off + 188)?;
        if d[0] != 0x47 {
            return None;
        }
        Some(Pkt {
            pid: (u16::from(d[1] & 0x1F) << 8) | u16::from(d[2]),
            pusi: d[1] & 0x40 != 0,
            afc: (d[3] >> 4) & 3,
            data: d,
        })
    })
}

/// First complete, CRC-valid PSI section on `pid`.
fn first_section(buf: &[u8], stride: usize, off: usize, pid: u16, table_id: u8) -> Option<Vec<u8>> {
    let mut cur: Option<Vec<u8>> = None;
    for p in packets(buf, stride, off) {
        if p.pid != pid {
            continue;
        }
        let pl = p.payload();
        if p.pusi {
            let Some(&ptr) = pl.first() else { continue };
            let Some(start) = pl.get(1 + ptr as usize..) else {
                cur = None;
                continue;
            };
            cur = Some(start.to_vec());
        } else if let Some(c) = cur.as_mut() {
            c.extend_from_slice(pl);
            if c.len() > 4200 {
                cur = None;
            }
        }
        if let Some(c) = &cur {
            if c.len() >= 3 {
                let slen = (usize::from(c[1] & 0x0F) << 8) | usize::from(c[2]);
                if c.len() >= 3 + slen {
                    let sec = &c[..3 + slen];
                    if sec[0] == table_id && slen >= 9 && crc32_mpeg(sec) == 0 {
                        return Some(sec.to_vec());
                    }
                    cur = None;
                }
            }
        }
    }
    None
}

/// What the head of the stream tells us.
#[derive(Debug, Default)]
struct HeadInfo {
    pat: bool,
    pmt: bool,
    programs: usize,
    streams: Vec<Es>,
    pcr_pid: Option<u16>,
    first_pcr: Option<u64>,
    first_pts: Option<(u16, u64)>,
    picture: Option<PictureInfo>,
    fps: Option<f64>,
}

fn pes_pts(payload: &[u8]) -> Option<u64> {
    if payload.len() < 14 || payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
        return None;
    }
    if payload[7] & 0x80 == 0 {
        return None;
    }
    let b = &payload[9..14];
    Some(
        (u64::from((b[0] >> 1) & 7) << 30)
            | (u64::from(b[1]) << 22)
            | (u64::from(b[2] >> 1) << 15)
            | (u64::from(b[3]) << 7)
            | u64::from(b[4] >> 1),
    )
}

fn parse_head(buf: &[u8], stride: usize, off: usize) -> HeadInfo {
    let mut h = HeadInfo::default();
    // PAT
    let mut pmt_pids: Vec<u16> = Vec::new();
    if let Some(pat) = first_section(buf, stride, off, 0, 0x00) {
        h.pat = true;
        let body = &pat[8..pat.len().saturating_sub(4)];
        for e in body.chunks_exact(4) {
            let prog = u16::from_be_bytes([e[0], e[1]]);
            let pid = (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]);
            if prog != 0 && pmt_pids.len() < 8 {
                pmt_pids.push(pid);
            }
        }
        h.programs = pmt_pids.len();
    }
    // PMT of the first program that has one
    for &pid in &pmt_pids {
        let Some(pmt) = first_section(buf, stride, off, pid, 0x02) else {
            continue;
        };
        if pmt.len() < 16 {
            continue;
        }
        h.pmt = true;
        h.pcr_pid = Some((u16::from(pmt[8] & 0x1F) << 8) | u16::from(pmt[9]));
        let info_len = (usize::from(pmt[10] & 0x0F) << 8) | usize::from(pmt[11]);
        let mut p = 12 + info_len;
        let end = pmt.len() - 4;
        while p + 5 <= end && h.streams.len() < 32 {
            let stream_type = pmt[p];
            let es_pid = (u16::from(pmt[p + 1] & 0x1F) << 8) | u16::from(pmt[p + 2]);
            let es_len = (usize::from(pmt[p + 3] & 0x0F) << 8) | usize::from(pmt[p + 4]);
            let desc = pmt.get(p + 5..(p + 5 + es_len).min(end)).unwrap_or(&[]);
            let (kind, codec) = classify_stream(stream_type, desc);
            h.streams.push(Es {
                pid: es_pid,
                stream_type,
                kind,
                codec,
            });
            p += 5 + es_len;
        }
        break;
    }
    // PCR and first PTS
    let pcr_pid = h.pcr_pid;
    for p in packets(buf, stride, off) {
        if h.first_pcr.is_none() && (pcr_pid.is_none() || pcr_pid == Some(p.pid)) {
            if let Some(v) = p.pcr() {
                h.first_pcr = Some(v);
                if h.pcr_pid.is_none() {
                    h.pcr_pid = Some(p.pid);
                }
            }
        }
        if h.first_pts.is_none() && p.pusi {
            let wanted = h.streams.is_empty()
                || h.streams
                    .iter()
                    .any(|s| s.pid == p.pid && s.kind != EsKind::Other);
            if wanted {
                if let Some(v) = pes_pts(p.payload()) {
                    h.first_pts = Some((p.pid, v));
                }
            }
        }
    }
    // Picture size from the video elementary stream.
    if let Some(v) = h.streams.iter().find(|s| s.kind == EsKind::Video) {
        let mut es: Vec<u8> = Vec::new();
        for p in packets(buf, stride, off) {
            if p.pid == v.pid {
                es.extend_from_slice(p.payload());
                if es.len() >= ES_PREFIX {
                    break;
                }
            }
        }
        match v.stream_type {
            0x1B => h.picture = picture_from_annexb(&es, false),
            0x24 => h.picture = picture_from_annexb(&es, true),
            0x01 | 0x02 => {
                if let Some(seq) = find_mpeg_sequence_header(&es) {
                    h.picture = Some(seq.picture);
                    h.fps = mpeg_frame_rate(seq.rate_code);
                }
            }
            _ => {}
        }
    }
    h
}

/// Last PCR (or PTS on `pts_pid`) in a tail buffer.
fn tail_times(
    buf: &[u8],
    stride: usize,
    off: usize,
    head: &HeadInfo,
) -> (Option<u64>, Option<u64>) {
    let mut last_pcr = None;
    let mut last_pts = None;
    for p in packets(buf, stride, off) {
        if head.pcr_pid.is_none_or(|pid| pid == p.pid) {
            if let Some(v) = p.pcr() {
                last_pcr = Some(v);
            }
        }
        if let Some((pid, _)) = head.first_pts {
            if p.pid == pid && p.pusi {
                if let Some(v) = pes_pts(p.payload()) {
                    last_pts = Some(v);
                }
            }
        }
    }
    (last_pcr, last_pts)
}

fn duration_ms(head: &HeadInfo, last_pcr: Option<u64>, last_pts: Option<u64>) -> Option<u64> {
    const PCR_WRAP: u64 = (1u64 << 33) * 300;
    const PTS_WRAP: u64 = 1u64 << 33;
    let ms = if let (Some(a), Some(b)) = (head.first_pcr, last_pcr) {
        let d = if b >= a { b - a } else { b + PCR_WRAP - a };
        d / 27_000
    } else if let (Some((_, a)), Some(b)) = (head.first_pts, last_pts) {
        let d = if b >= a { b - a } else { b + PTS_WRAP - a };
        d / 90
    } else {
        return None;
    };
    // A stream longer than 48 hours is a timestamp discontinuity, not a duration.
    (ms > 0 && ms < 48 * 3600 * 1000).then_some(ms)
}

struct Analysis {
    stride: usize,
    run: Run,
    head: HeadInfo,
    duration_ms: Option<u64>,
}

fn analyse(win: &mut Window<'_>, holes: &RangeSet) -> Option<Analysis> {
    let first = win.get(0, 4 * 192 + 8).to_vec();
    let (stride, off) = detect_layout(&first)?;
    log::trace!("MPEG-TS: stride {stride}, sync offset {off}");
    let run = walk_run(win, stride, off, holes);
    log::debug!(
        "MPEG-TS: run of {} packets, {} TEI, {} CC errors",
        run.packets,
        run.tei,
        run.cc_errors
    );
    if run.packets < MIN_PACKETS || !run.plausible_start {
        return None;
    }
    let n = run.packets.min(HEAD_PACKETS) as usize;
    let head_buf = win.get(0, n * stride).to_vec();
    let head = parse_head(&head_buf, stride, off);
    let tail_n = run.packets.min(HEAD_PACKETS);
    let tail_start = (run.packets - tail_n) * stride as u64;
    let tail_buf = win.get(tail_start, tail_n as usize * stride).to_vec();
    let (lp, lt) = tail_times(&tail_buf, stride, off, &head);
    let duration_ms = duration_ms(&head, lp, lt);
    Some(Analysis {
        stride,
        run,
        head,
        duration_ms,
    })
}

fn build_meta(a: &Analysis) -> MediaMeta {
    let mut m = MediaMeta::default();
    if let Some(p) = a.head.picture {
        m.width = Some(p.width);
        m.height = Some(p.height);
    }
    m.video_codec = a
        .head
        .streams
        .iter()
        .find(|s| s.kind == EsKind::Video)
        .map(|s| s.codec.clone());
    m.audio_codec = a
        .head
        .streams
        .iter()
        .find(|s| s.kind == EsKind::Audio)
        .map(|s| s.codec.clone());
    m.duration_ms = a.duration_ms;
    m.extra.insert(
        EXTENSION_KEY.into(),
        if a.stride == 192 { "mts" } else { "ts" }.into(),
    );
    m.extra.insert("packet_size".into(), a.stride.to_string());
    m.extra.insert("packets".into(), a.run.packets.to_string());
    m.extra
        .insert("programs".into(), a.head.programs.to_string());
    if let Some(f) = a.head.fps {
        m.extra.insert("fps".into(), format!("{f:.3}"));
    }
    let pids: Vec<String> = a
        .head
        .streams
        .iter()
        .map(|s| format!("0x{:04x}={}", s.pid, s.codec))
        .collect();
    if !pids.is_empty() {
        m.extra.insert("streams".into(), pids.join(","));
    }
    if a.run.cc_errors > 0 {
        m.extra
            .insert("cc_errors".into(), a.run.cc_errors.to_string());
    }
    m
}

/// Known unreadable ranges of `src` inside the window of `avail` bytes at
/// `start`, file-relative.
fn known_holes(src: &dyn Source, start: u64, avail: u64) -> RangeSet {
    match src.unreadable() {
        Some(set) if !set.is_empty() => set
            .intersection_with_range(Range::new(start, start.saturating_add(avail)))
            .shifted_down(start),
        _ => RangeSet::new(),
    }
}

/// Format handler for MPEG-TS and M2TS.
#[derive(Debug, Default, Clone, Copy)]
pub struct MpegTsFormat;

static SIGNATURES: [Signature; 2] = [
    Signature::at_start(&[0x47]).with_also(&[0x47], 188),
    Signature::at(&[0x47], 4).with_also(&[0x47], 196),
];

impl FileFormat for MpegTsFormat {
    fn id(&self) -> &'static str {
        "mts"
    }

    fn name(&self) -> &'static str {
        "MPEG transport stream (TS / M2TS / AVCHD)"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mts", "m2ts", "ts"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        MIN_PACKETS * 188
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        22
    }

    fn probe(&self, head: &[u8]) -> bool {
        let Some((stride, off)) = detect_layout(head) else {
            return false;
        };
        // Every packet we can see must have a legal adaptation field control.
        let mut seen = 0;
        let mut non_null = false;
        for k in 0..3 {
            let p = off + k * stride;
            let Some(h) = head.get(p..p + 4) else { break };
            if (h[3] >> 4) & 3 == 0 {
                return false;
            }
            if (u16::from(h[1] & 0x1F) << 8 | u16::from(h[2])) != 0x1FFF {
                non_null = true;
            }
            seen += 1;
        }
        seen >= 2 && non_null
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let holes = known_holes(&**src, start, win.avail());
        let Some(a) = analyse(&mut win, &holes) else {
            return Ok(None);
        };
        let len = a.run.packets * a.stride as u64;
        let mut notes = Vec::new();
        notes.extend(a.run.holes_note());
        // A read failure inside a hole the run was carried across is
        // accounted for (head and tail reads may touch it again).
        let fault = win.fault().filter(|f| !a.run.skipped(f.offset));
        let (status, err_offset) = if let Some(f) = fault {
            notes.push(format!("read error near offset {}", f.offset));
            (ExtentStatus::Truncated, Some(f.offset.max(len)))
        } else if !a.run.sync_miss && win.limited() && len + a.stride as u64 > win.avail() {
            (ExtentStatus::Capped, None)
        } else {
            (ExtentStatus::UnknownLength, None)
        };
        if a.run.cc_errors > 0 {
            notes.push(format!(
                "{} continuity counter errors (lost or damaged packets)",
                a.run.cc_errors
            ));
        }
        if a.run.tei > 0 {
            notes.push(format!(
                "{} packets flagged with transport errors",
                a.run.tei
            ));
        }
        if !a.head.pat {
            notes.push("no PAT within the first 1024 packets".into());
        }
        let mut conf = if a.head.pat {
            Confidence::HIGH
        } else {
            Confidence::MEDIUM
        };
        if a.run.cc_errors > 2 {
            conf = conf.lowered(5);
        }
        let mut m = Measured {
            len,
            status,
            confidence: conf,
            err_offset,
            meta: build_meta(&a),
            nested: Vec::new(),
            notes,
        };
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut win = Window::new(&**src, 0, u64::MAX);
        let size = win.avail();
        let holes = known_holes(&**src, 0, size);
        let Some(a) = analyse(&mut win, &holes) else {
            return Ok(Validation::invalid(
                "not an MPEG transport stream (no run of 16 packets with a regular sync byte)",
            ));
        };
        let covered = a.run.packets * a.stride as u64;
        let mut problems = Vec::new();
        if let Some(n) = a.run.holes_note() {
            problems.push(n);
        }
        if covered < size {
            if a.run.sync_miss {
                problems.push(format!(
                    "packet sync lost at offset {covered}; {} bytes follow that are not packets",
                    size - covered
                ));
            } else {
                problems.push(format!(
                    "the file ends inside a packet ({} trailing bytes)",
                    size - covered
                ));
            }
        }
        if a.run.tei > 0 {
            problems.push(format!(
                "{} packets are flagged with transport errors",
                a.run.tei
            ));
        }
        if a.run.cc_errors > 0 {
            problems.push(format!(
                "{} continuity counter errors (lost packets)",
                a.run.cc_errors
            ));
        }
        if a.run.bad_afc > 0 {
            problems.push(format!(
                "{} packets have a reserved adaptation field control",
                a.run.bad_afc
            ));
        }
        if !a.head.pat {
            problems.push("no PAT within the first 1024 packets".into());
        } else if !a.head.pmt {
            problems.push("PAT found but no PMT within the first 1024 packets".into());
        }
        let bad = a.run.tei + a.run.cc_errors + a.run.bad_afc;
        let intact = if problems.is_empty() {
            1.0
        } else {
            let good = covered.saturating_sub(bad * a.stride as u64);
            (good as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
        };
        Ok(Validation {
            status: if problems.is_empty() {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Damaged
            },
            intact_fraction: Some(intact),
            problems,
            meta: build_meta(&a),
            repairable_by: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, cc: u8, pusi: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x47, (pid >> 8) as u8 & 0x1F, pid as u8, 0x10 | (cc & 15)];
        if pusi {
            p[1] |= 0x40;
        }
        p.extend_from_slice(payload);
        p.resize(188, 0xFF);
        p
    }

    fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut s = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01];
        s.push(0xE0 | (pmt_pid >> 8) as u8);
        s.push(pmt_pid as u8);
        let crc = crc32_mpeg(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        let mut pl = vec![0u8]; // pointer field
        pl.extend_from_slice(&s);
        pl
    }

    fn pmt(video_pid: u16, audio_pid: u16) -> Vec<u8> {
        let mut s = vec![0x02, 0xB0, 0x00, 0x00, 0x01, 0xC1, 0x00, 0x00];
        s.extend_from_slice(&[0xE0 | (video_pid >> 8) as u8, video_pid as u8]); // PCR pid
        s.extend_from_slice(&[0xF0, 0x00]);
        s.push(0x1B);
        s.extend_from_slice(&[0xE0 | (video_pid >> 8) as u8, video_pid as u8, 0xF0, 0x00]);
        s.push(0x0F);
        s.extend_from_slice(&[0xE0 | (audio_pid >> 8) as u8, audio_pid as u8, 0xF0, 0x00]);
        let len = s.len() + 4 - 3;
        s[1] = 0xB0 | ((len >> 8) as u8 & 0x0F);
        s[2] = len as u8;
        let crc = crc32_mpeg(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        let mut pl = vec![0u8];
        pl.extend_from_slice(&s);
        pl
    }

    fn synthetic(n: usize, stride: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut cc = 0u8;
        let add = |p: Vec<u8>, v: &mut Vec<u8>| {
            if stride == 192 {
                v.extend_from_slice(&[0, 0, 0, v.len() as u8]);
            }
            v.extend_from_slice(&p);
        };
        add(packet(0, 0, true, &pat(0x100)), &mut v);
        add(packet(0x100, 0, true, &pmt(0x101, 0x102)), &mut v);
        for i in 0..n {
            add(packet(0x101, cc, i == 0, &[0, 0, 1, 0xE0]), &mut v);
            cc = (cc + 1) & 15;
        }
        v
    }

    #[test]
    fn layout_detection() {
        let ts = synthetic(20, 188);
        assert_eq!(detect_layout(&ts), Some((188, 0)));
        let m2 = synthetic(20, 192);
        assert_eq!(detect_layout(&m2), Some((192, 4)));
        assert_eq!(detect_layout(&[0u8; 2000]), None);
        assert_eq!(detect_layout(&[0x47u8; 100]), None);
    }

    #[test]
    fn synthetic_stream_is_measured() {
        for stride in [188usize, 192] {
            let ts = synthetic(30, stride);
            let mut data = ts.clone();
            data.extend_from_slice(&[0x55; 1000]);
            let src: SharedSource =
                std::sync::Arc::new(diskrescue_core::source::MemSource::new(data));
            let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
            assert_eq!(m.len, ts.len() as u64, "stride {stride}");
            assert_eq!(m.status, ExtentStatus::UnknownLength);
            assert_eq!(m.confidence, Confidence::HIGH);
            assert_eq!(m.meta.video_codec.as_deref(), Some("h264"));
            assert_eq!(m.meta.audio_codec.as_deref(), Some("aac"));
            assert_eq!(
                m.meta.extra["extension"],
                if stride == 192 { "mts" } else { "ts" }
            );
        }
    }

    #[test]
    fn too_few_packets_is_rejected() {
        let ts = synthetic(10, 188); // 12 packets
        let src: SharedSource = std::sync::Arc::new(diskrescue_core::source::MemSource::new(ts));
        assert!(MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().is_none());
    }

    #[test]
    fn repeated_sync_bytes_are_not_a_stream() {
        let src: SharedSource =
            std::sync::Arc::new(diskrescue_core::source::MemSource::new(vec![0x47; 20_000]));
        assert!(MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().is_none());
        assert!(!MpegTsFormat.probe(&[0x47; 1200]));
    }

    /// Bytes with known-unreadable ranges that read as zeros, the way the
    /// carver presents a rescued image (`ZeroFill` over a mapfile).
    struct ZeroHoles {
        data: Vec<u8>,
        holes: RangeSet,
    }

    impl ZeroHoles {
        fn source(mut data: Vec<u8>, holes: &[(u64, u64)]) -> SharedSource {
            let mut set = RangeSet::new();
            for &(at, n) in holes {
                data[at as usize..(at + n) as usize].fill(0);
                set.insert(Range::with_len(at, n));
            }
            std::sync::Arc::new(ZeroHoles { data, holes: set })
        }
    }

    impl Source for ZeroHoles {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(
            &self,
            offset: u64,
            buf: &mut [u8],
        ) -> std::result::Result<usize, diskrescue_core::ReadError> {
            let src = self.data.get(offset as usize..).unwrap_or(&[]);
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            Ok(n)
        }
        fn describe(&self) -> String {
            "zero holes".into()
        }
        fn unreadable(&self) -> Option<RangeSet> {
            Some(self.holes.clone())
        }
    }

    /// The same, but reads of the holes fail (a `MemSource` with simulated
    /// bad sectors).
    fn failing_holes(data: Vec<u8>, holes: &[(u64, u64)]) -> SharedSource {
        let mut set = RangeSet::new();
        for &(at, n) in holes {
            set.insert(Range::with_len(at, n));
        }
        std::sync::Arc::new(diskrescue_core::source::MemSource::new(data).with_unreadable(set))
    }

    fn with_junk(ts: &[u8]) -> Vec<u8> {
        let mut d = ts.to_vec();
        d.extend_from_slice(&[0x55; 3000]);
        d
    }

    #[test]
    fn a_known_hole_does_not_split_the_run() {
        for stride in [188usize, 192] {
            let ts = synthetic(600, stride);
            let data = with_junk(&ts);
            // A 4 KiB hole on a 4 KiB boundary in the middle, as left by
            // ddrescue on a camera card.
            let hole = (40_960u64, 4096u64);
            for (kind, src) in [
                ("zero-filled", ZeroHoles::source(data.clone(), &[hole])),
                ("failing", failing_holes(data.clone(), &[hole])),
            ] {
                let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
                assert_eq!(m.len, ts.len() as u64, "{kind} stride {stride}");
                assert_eq!(m.status, ExtentStatus::UnknownLength, "{kind}");
                assert_eq!(m.err_offset, None, "{kind}");
                assert_eq!(m.confidence, Confidence::HIGH, "{kind}: {:?}", m.notes);
                assert!(
                    m.notes.iter().any(|n| n.contains(
                        "packet sync resumed after 1 unreadable range(s) at file offsets 40960..45056"
                    )),
                    "{kind}: {:?}",
                    m.notes
                );
                assert!(m.notes.iter().any(|n| n.contains("known-unreadable")));
            }
            // Without the mapfile the zeros are just a sync miss.
            let mut zeroed = data.clone();
            zeroed[40_960..45_056].fill(0);
            let src: SharedSource =
                std::sync::Arc::new(diskrescue_core::source::MemSource::new(zeroed));
            let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
            assert!(m.len < 45_056, "stride {stride}");
        }
    }

    #[test]
    fn several_holes_close_together_and_a_hole_measured_from_inside_the_stream() {
        let ts = synthetic(1500, 188);
        let data = with_junk(&ts);
        // Two holes three packets apart, and one more later.
        let holes = [(51_200u64, 512u64), (52_224, 512), (180_224, 8192)];
        let src = ZeroHoles::source(data.clone(), &holes);
        let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, ts.len() as u64);
        assert!(
            m.notes
                .iter()
                .any(|n| n.contains("after 3 unreadable range(s)")),
            "{:?}",
            m.notes
        );
        // Measured from a later packet (a carver hit past the first holes).
        let start = 188 * 400;
        let m = MpegTsFormat
            .measure(&src, start, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, ts.len() as u64 - start);
        assert!(m
            .notes
            .iter()
            .any(|n| n.contains("after 1 unreadable range(s)")));
    }

    #[test]
    fn a_hole_the_grid_does_not_survive_ends_the_run() {
        let ts = synthetic(600, 188);
        // The stream ends inside the hole: after it, other data.
        let cut = 50_000usize;
        let mut data = ts[..cut].to_vec();
        data.extend_from_slice(&[0x47; 1]);
        data.extend(std::iter::repeat_n(0x11u8, 20_000));
        let hole = (49_152u64, 1024u64);
        let src = ZeroHoles::source(data, &[hole]);
        let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
        // The packet the hole starts in still counts (its sync byte is read).
        assert_eq!(m.len, (49_152 / 188 + 1) * 188);
        assert_eq!(m.status, ExtentStatus::UnknownLength);
        assert!(!m.notes.iter().any(|n| n.contains("packet sync resumed")));
        // A sync miss next to, but not explained by, a hole also ends it.
        let mut data = with_junk(&ts);
        data[188 * 300] = 0x00;
        let src = ZeroHoles::source(data, &[(188 * 300 + 100, 512)]);
        let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, 188 * 300);
    }

    #[test]
    fn validate_reports_the_holes() {
        let ts = synthetic(600, 192);
        let src = ZeroHoles::source(ts.clone(), &[(40_960, 4096)]);
        let v = MpegTsFormat.validate(&src).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
        assert!(v.problems.iter().any(|p| p.contains("unreadable range")));
        assert!(!v.problems.iter().any(|p| p.contains("sync lost")));
    }

    #[test]
    fn no_pat_gives_medium_confidence() {
        let mut v = Vec::new();
        for i in 0..40u8 {
            v.extend_from_slice(&packet(0x101, i, i == 0, &[0, 0, 1, 0xE0]));
        }
        let src: SharedSource = std::sync::Arc::new(diskrescue_core::source::MemSource::new(v));
        let m = MpegTsFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.confidence, Confidence::MEDIUM);
        assert_eq!(m.status, ExtentStatus::UnknownLength);
    }
}
