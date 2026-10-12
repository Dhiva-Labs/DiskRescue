//! Matroska and WebM (EBML) carving, validation and metadata.
//!
//! # Grammar
//!
//! An element is `ID (1..4 bytes, marker kept) SIZE (1..8 bytes, marker
//! removed) DATA`. A size whose value bits are all ones means "unknown":
//! live recorders write the Segment and each Cluster that way. The walker
//! follows the top level of the Segment (SeekHead, Info, Tracks, Cluster,
//! Cues, Attachments, Tags, Chapters, Void, CRC-32) and never descends into
//! clusters of known size unless asked to (validation).
//!
//! # Extent
//!
//! * Segment of known size, walked to its end: `Complete`, length = EBML
//!   header + Segment header + Segment data.
//! * Unknown size: clusters (and the children of unknown-size clusters) are
//!   walked until something that is not an element of the grammar. If that
//!   happens at an element boundary the run is `UnknownLength` (a live file
//!   has no end marker; the bytes that follow are simply not part of it). If
//!   the last element is cut short, or an element is inconsistent with its
//!   parent, the status is `Truncated` with `err_offset` at the start of the
//!   offending element, and the length ends at the last complete element.
//!
//! Metadata comes from Info (timestamp scale, duration, `DateUTC`, muxing and
//! writing application) and Tracks (codec ids, picture size). When Info has
//! no duration (live files) it is estimated from the last cluster. Cover art
//! in Attachments is reported as nested `CoverArt` objects.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::time::Timestamp;
use diskrescue_core::{Range, Result, SharedSource};

use crate::io::{note_unreadable, Window};
use crate::meta::{clean_string, EXTENSION_KEY};

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEKHEAD: u32 = 0x114D_9B74;
const ID_INFO: u32 = 0x1549_A966;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CHAPTERS: u32 = 0x1043_A770;
const ID_TAGS: u32 = 0x1254_C367;
const ID_ATTACHMENTS: u32 = 0x1941_A469;
const ID_VOID: u32 = 0xEC;
const ID_CRC32: u32 = 0xBF;

const ID_DOCTYPE: u32 = 0x4282;
const ID_MAXIDLEN: u32 = 0x42F2;
const ID_MAXSIZELEN: u32 = 0x42F3;

const ID_TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const ID_DURATION: u32 = 0x4489;
const ID_DATE_UTC: u32 = 0x4461;
const ID_MUXING_APP: u32 = 0x4D80;
const ID_WRITING_APP: u32 = 0x5741;
const ID_TITLE: u32 = 0x7BA9;

const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_VIDEO: u32 = 0xE0;
const ID_AUDIO: u32 = 0xE1;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_SAMPLING_FREQ: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;

const ID_CL_TIMESTAMP: u32 = 0xE7;
const ID_CL_SILENT: u32 = 0x5854;
const ID_CL_POSITION: u32 = 0xA7;
const ID_CL_PREVSIZE: u32 = 0xAB;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_ENCRYPTED_BLOCK: u32 = 0xAF;

const ID_ATTACHED_FILE: u32 = 0x61A7;
const ID_FILE_NAME: u32 = 0x466E;
const ID_FILE_MIME: u32 = 0x4660;
const ID_FILE_DATA: u32 = 0x465C;

const MAX_SIZE: u64 = 64 << 30;
/// Cap on elements visited in one master element (guards hostile files).
const MAX_CHILDREN: usize = 1 << 20;
/// Cap on the declared size of metadata elements we read into memory.
const MAX_META_ELEMENT: u64 = 16 * 1024 * 1024;

fn is_level1(id: u32) -> bool {
    matches!(
        id,
        ID_SEEKHEAD
            | ID_INFO
            | ID_TRACKS
            | ID_CLUSTER
            | ID_CUES
            | ID_CHAPTERS
            | ID_TAGS
            | ID_ATTACHMENTS
            | ID_VOID
            | ID_CRC32
    )
}

fn is_cluster_child(id: u32) -> bool {
    matches!(
        id,
        ID_CL_TIMESTAMP
            | ID_CL_SILENT
            | ID_CL_POSITION
            | ID_CL_PREVSIZE
            | ID_SIMPLE_BLOCK
            | ID_BLOCK_GROUP
            | ID_ENCRYPTED_BLOCK
            | ID_VOID
            | ID_CRC32
    )
}

/// An element header.
#[derive(Debug, Clone, Copy)]
struct Elem {
    id: u32,
    pos: u64,
    hdr: u64,
    /// `None` = unknown size.
    size: Option<u64>,
}

impl Elem {
    fn data(&self) -> u64 {
        self.pos + self.hdr
    }
    /// End of the element if its size is known.
    fn end(&self) -> Option<u64> {
        self.size.and_then(|s| self.data().checked_add(s))
    }
}

enum Rd {
    Ok(Elem),
    /// Not enough bytes for a header (cut file or end of window).
    Short,
    /// Not a valid element header.
    Bad,
}

/// Read an element header at `pos`.
fn read_elem(win: &mut Window<'_>, pos: u64) -> Rd {
    let b = win.get(pos, 12);
    if b.is_empty() {
        return Rd::Short;
    }
    let first = b[0];
    if first == 0 {
        return Rd::Bad;
    }
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return Rd::Bad;
    }
    if b.len() < id_len + 1 {
        return Rd::Short;
    }
    let mut id = 0u32;
    for &x in &b[..id_len] {
        id = (id << 8) | u32::from(x);
    }
    let sfirst = b[id_len];
    if sfirst == 0 {
        return Rd::Bad;
    }
    let size_len = sfirst.leading_zeros() as usize + 1;
    if b.len() < id_len + size_len {
        return Rd::Short;
    }
    let mut v = u64::from(sfirst) & ((1u64 << (8 - size_len)) - 1);
    for &x in &b[id_len + 1..id_len + size_len] {
        v = (v << 8) | u64::from(x);
    }
    let all_ones = (1u64 << (7 * size_len)) - 1;
    let size = if v == all_ones { None } else { Some(v) };
    Rd::Ok(Elem {
        id,
        pos,
        hdr: (id_len + size_len) as u64,
        size,
    })
}

fn read_uint(win: &mut Window<'_>, e: &Elem) -> Option<u64> {
    let n = e.size?;
    if n > 8 {
        return None;
    }
    let b = win.exact(e.data(), n as usize)?;
    Some(b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)))
}

fn read_float(win: &mut Window<'_>, e: &Elem) -> Option<f64> {
    match e.size? {
        4 => {
            let b = win.exact(e.data(), 4)?;
            Some(f64::from(f32::from_be_bytes([b[0], b[1], b[2], b[3]])))
        }
        8 => {
            let b = win.exact(e.data(), 8)?;
            Some(f64::from_be_bytes(b.try_into().ok()?))
        }
        _ => None,
    }
}

fn read_text(win: &mut Window<'_>, e: &Elem) -> Option<String> {
    let n = e.size?.min(512) as usize;
    let b = win.exact(e.data(), n)?;
    clean_string(b)
}

/// Track information.
#[derive(Debug, Clone, Default)]
struct Track {
    ttype: u64,
    codec: String,
    width: u32,
    height: u32,
    sample_rate: f64,
    channels: u32,
}

/// Segment information.
#[derive(Debug, Clone, Default)]
struct SegInfo {
    scale: Option<u64>,
    duration: Option<f64>,
    date_ns: Option<i64>,
    muxing: Option<String>,
    writing: Option<String>,
    title: Option<String>,
}

fn parse_info(win: &mut Window<'_>, e: &Elem) -> SegInfo {
    let mut info = SegInfo::default();
    let Some(end) = e.end() else { return info };
    if e.size.unwrap_or(0) > MAX_META_ELEMENT {
        return info;
    }
    let mut p = e.data();
    for _ in 0..256 {
        if p >= end {
            break;
        }
        let Rd::Ok(c) = read_elem(win, p) else { break };
        let Some(ce) = c.end() else { break };
        if ce > end {
            break;
        }
        match c.id {
            ID_TIMESTAMP_SCALE => info.scale = read_uint(win, &c),
            ID_DURATION => info.duration = read_float(win, &c),
            ID_DATE_UTC => {
                if c.size == Some(8) {
                    if let Some(b) = win.exact(c.data(), 8) {
                        info.date_ns = Some(i64::from_be_bytes(b.try_into().unwrap_or([0; 8])));
                    }
                }
            }
            ID_MUXING_APP => info.muxing = read_text(win, &c),
            ID_WRITING_APP => info.writing = read_text(win, &c),
            ID_TITLE => info.title = read_text(win, &c),
            _ => {}
        }
        p = ce;
    }
    info
}

fn parse_tracks(win: &mut Window<'_>, e: &Elem) -> Vec<Track> {
    let mut tracks = Vec::new();
    let Some(end) = e.end() else { return tracks };
    if e.size.unwrap_or(0) > MAX_META_ELEMENT {
        return tracks;
    }
    let mut p = e.data();
    for _ in 0..256 {
        if p >= end || tracks.len() >= 128 {
            break;
        }
        let Rd::Ok(c) = read_elem(win, p) else { break };
        let Some(ce) = c.end() else { break };
        if ce > end {
            break;
        }
        if c.id == ID_TRACK_ENTRY {
            tracks.push(parse_track_entry(win, &c));
        }
        p = ce;
    }
    tracks
}

fn parse_track_entry(win: &mut Window<'_>, e: &Elem) -> Track {
    let mut t = Track::default();
    let Some(end) = e.end() else { return t };
    let mut p = e.data();
    for _ in 0..256 {
        if p >= end {
            break;
        }
        let Rd::Ok(c) = read_elem(win, p) else { break };
        let Some(ce) = c.end() else { break };
        if ce > end {
            break;
        }
        match c.id {
            ID_TRACK_TYPE => t.ttype = read_uint(win, &c).unwrap_or(0),
            ID_CODEC_ID => t.codec = read_text(win, &c).unwrap_or_default(),
            ID_TRACK_NUMBER => {}
            ID_VIDEO | ID_AUDIO => {
                let mut q = c.data();
                for _ in 0..64 {
                    if q >= ce {
                        break;
                    }
                    let Rd::Ok(d) = read_elem(win, q) else { break };
                    let Some(de) = d.end() else { break };
                    if de > ce {
                        break;
                    }
                    match (c.id, d.id) {
                        (ID_VIDEO, ID_PIXEL_WIDTH) => {
                            t.width =
                                read_uint(win, &d).unwrap_or(0).min(u64::from(u32::MAX)) as u32
                        }
                        (ID_VIDEO, ID_PIXEL_HEIGHT) => {
                            t.height =
                                read_uint(win, &d).unwrap_or(0).min(u64::from(u32::MAX)) as u32
                        }
                        (ID_AUDIO, ID_SAMPLING_FREQ) => {
                            t.sample_rate = read_float(win, &d).unwrap_or(0.0)
                        }
                        (ID_AUDIO, ID_CHANNELS) => {
                            t.channels = read_uint(win, &d).unwrap_or(0).min(1000) as u32
                        }
                        _ => {}
                    }
                    q = de;
                }
            }
            _ => {}
        }
        p = ce;
    }
    t
}

/// Cover art and other image attachments, as file-relative ranges.
fn parse_attachments(win: &mut Window<'_>, e: &Elem) -> Vec<Nested> {
    let mut out = Vec::new();
    let Some(end) = e.end() else { return out };
    let mut p = e.data();
    for _ in 0..64 {
        if p >= end {
            break;
        }
        let Rd::Ok(c) = read_elem(win, p) else { break };
        let Some(ce) = c.end() else { break };
        if ce > end {
            break;
        }
        if c.id == ID_ATTACHED_FILE {
            let (mut name, mut mime, mut data) = (None, None, None);
            let mut q = c.data();
            for _ in 0..16 {
                if q >= ce {
                    break;
                }
                let Rd::Ok(d) = read_elem(win, q) else { break };
                let Some(de) = d.end() else { break };
                if de > ce {
                    break;
                }
                match d.id {
                    ID_FILE_NAME => name = read_text(win, &d),
                    ID_FILE_MIME => mime = read_text(win, &d),
                    ID_FILE_DATA => data = Some(Range::new(d.data(), de)),
                    _ => {}
                }
                q = de;
            }
            if let Some(r) = data {
                let mime = mime.unwrap_or_default().to_ascii_lowercase();
                let name = name.unwrap_or_default().to_ascii_lowercase();
                let format = if mime == "image/jpeg"
                    || mime == "image/jpg"
                    || name.ends_with(".jpg")
                    || name.ends_with(".jpeg")
                {
                    Some("jpeg")
                } else if mime == "image/png" || name.ends_with(".png") {
                    Some("png")
                } else {
                    None
                };
                if let Some(f) = format {
                    out.push(Nested {
                        range: r,
                        format: Some(f.to_string()),
                        role: NestedRole::CoverArt,
                    });
                }
            }
        }
        p = ce;
    }
    out
}

/// How the walk of the segment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// A known-size segment was walked to its declared end.
    Done,
    /// The data ran out exactly at an element boundary.
    Exhausted,
    /// The caller's limit ended the walk.
    Limit,
    /// The next bytes do not start an element of the grammar.
    NonElement,
    /// An element extends past the available data.
    Partial,
    /// An element is inconsistent with its parent.
    Corrupt,
    /// Read failure.
    Fault,
}

#[derive(Debug, Default)]
struct Scan {
    doctype: String,
    seg_size: Option<u64>,
    seg_data: u64,
    /// File offset just past the last complete element.
    good_end: u64,
    /// Where the problem is, for `Partial`/`Corrupt`/`Fault`.
    err_at: u64,
    end: Option<End>,
    info: SegInfo,
    have_info: bool,
    tracks: Vec<Track>,
    nested: Vec<Nested>,
    clusters: u64,
    blocks: u64,
    first_cluster_ts: Option<u64>,
    /// (data position, end if known, timestamp) of the last cluster seen.
    last_cluster: Option<(u64, Option<u64>, u64)>,
    max_block_ts: Option<i64>,
    has_cues: bool,
    unknown_level1: u32,
    notes: Vec<String>,
}

impl Scan {
    fn bad_end(&self) -> bool {
        matches!(self.end, Some(End::Partial | End::Corrupt | End::Fault))
    }
}

/// Parse the EBML header at the start of the window. Returns the DocType
/// and the offset of the first byte after the header.
fn read_ebml_header(win: &mut Window<'_>) -> Option<(String, u64)> {
    let Rd::Ok(e) = read_elem(win, 0) else {
        return None;
    };
    if e.id != ID_EBML {
        return None;
    }
    let size = e.size?;
    if !(4..=4096).contains(&size) {
        return None;
    }
    let end = e.data() + size;
    let mut doctype = String::new();
    let mut p = e.data();
    for _ in 0..32 {
        if p >= end {
            break;
        }
        let Rd::Ok(c) = read_elem(win, p) else {
            return None;
        };
        let ce = c.end()?;
        if ce > end {
            return None;
        }
        match c.id {
            ID_DOCTYPE => doctype = read_text(win, &c)?,
            ID_MAXIDLEN if read_uint(win, &c)? > 4 => return None,
            ID_MAXSIZELEN if read_uint(win, &c)? > 8 => return None,
            _ => {}
        }
        p = ce;
    }
    if doctype != "matroska" && doctype != "webm" {
        return None;
    }
    Some((doctype, end))
}

/// Walk the children of a cluster. `end` is the declared end for a known
/// size, `None` for unknown size. Returns the position where children
/// stopped and why.
fn walk_cluster(
    win: &mut Window<'_>,
    data: u64,
    end: Option<u64>,
    seg_end: Option<u64>,
    scan: &mut Scan,
    cluster_ts: &mut u64,
) -> (u64, End) {
    let avail = win.avail();
    let mut p = data;
    let mut n = 0usize;
    loop {
        if let Some(e) = end {
            if p == e {
                return (p, End::Done); // cluster complete (reused variant)
            }
            if p > e {
                return (p, End::Corrupt);
            }
        }
        if n >= MAX_CHILDREN {
            return (p, End::Corrupt);
        }
        n += 1;
        if p >= avail {
            return (
                p,
                if win.limited() {
                    End::Limit
                } else {
                    End::Exhausted
                },
            );
        }
        match read_elem(win, p) {
            Rd::Short => {
                return (
                    p,
                    if win.fault().is_some() {
                        End::Fault
                    } else if win.limited() && avail - p >= 12 {
                        End::Limit
                    } else {
                        End::Partial
                    },
                )
            }
            Rd::Bad => {
                return (
                    p,
                    if end.is_some() {
                        End::Corrupt
                    } else {
                        End::NonElement
                    },
                );
            }
            Rd::Ok(c) => {
                if !is_cluster_child(c.id) {
                    // Unknown-size cluster ends at the first element that
                    // is not one of its children; known size: damage.
                    return (
                        p,
                        if end.is_some() {
                            End::Corrupt
                        } else {
                            End::NonElement
                        },
                    );
                }
                let Some(ce) = c.end() else {
                    return (p, End::Corrupt);
                };
                if end.is_some_and(|e| ce > e) || seg_end.is_some_and(|s| ce > s) {
                    return (p, End::Corrupt);
                }
                if ce > avail {
                    return (
                        p,
                        if win.limited() {
                            End::Limit
                        } else {
                            End::Partial
                        },
                    );
                }
                match c.id {
                    ID_CL_TIMESTAMP => {
                        if let Some(v) = read_uint(win, &c) {
                            *cluster_ts = v;
                        }
                    }
                    ID_SIMPLE_BLOCK => {
                        scan.blocks += 1;
                        if let Some(ts) = block_timestamp(win, &c, false) {
                            scan.max_block_ts = Some(scan.max_block_ts.map_or(ts, |m| m.max(ts)));
                        }
                    }
                    ID_BLOCK_GROUP => {
                        scan.blocks += 1;
                        if let Some(ts) = block_timestamp(win, &c, true) {
                            scan.max_block_ts = Some(scan.max_block_ts.map_or(ts, |m| m.max(ts)));
                        }
                    }
                    _ => {}
                }
                p = ce;
            }
        }
    }
}

/// Relative timestamp of a SimpleBlock, or of the Block inside a BlockGroup.
fn block_timestamp(win: &mut Window<'_>, e: &Elem, group: bool) -> Option<i64> {
    let (data, avail_len) = if group {
        // Find the Block (A1) child.
        let end = e.end()?;
        let mut p = e.data();
        let mut found = None;
        for _ in 0..16 {
            if p >= end {
                break;
            }
            let Rd::Ok(c) = read_elem(win, p) else { break };
            let ce = c.end()?;
            if ce > end {
                break;
            }
            if c.id == 0xA1 {
                found = Some((c.data(), ce - c.data()));
                break;
            }
            p = ce;
        }
        found?
    } else {
        (e.data(), e.size?)
    };
    if avail_len < 4 {
        return None;
    }
    let b = win.get(data, 12);
    if b.is_empty() || b[0] == 0 {
        return None;
    }
    let track_len = b[0].leading_zeros() as usize + 1;
    let rel = b.get(track_len..track_len + 2)?;
    Some(i64::from(i16::from_be_bytes([rel[0], rel[1]])))
}

/// Walk the Segment. `deep` also verifies the children of known-size
/// clusters (validation).
fn scan_segment(win: &mut Window<'_>, deep: bool) -> Option<Scan> {
    let (doctype, ebml_end) = read_ebml_header(win)?;
    let mut scan = Scan {
        doctype,
        ..Scan::default()
    };
    // Void elements may sit between the EBML header and the Segment.
    let mut seg_pos = ebml_end;
    for _ in 0..4 {
        match read_elem(win, seg_pos) {
            Rd::Ok(e) if e.id == ID_VOID => seg_pos = e.end()?,
            _ => break,
        }
    }
    let Rd::Ok(seg) = read_elem(win, seg_pos) else {
        return None;
    };
    if seg.id != ID_SEGMENT {
        return None;
    }
    scan.seg_data = seg.data();
    scan.seg_size = seg.size.filter(|&s| s >= 16);
    if seg.size.is_none() {
        log::debug!("Matroska: Segment has unknown size; walking clusters");
    }
    if seg.size.is_some() && scan.seg_size.is_none() {
        scan.notes
            .push("Segment size is implausibly small; treated as unknown".into());
    }
    let seg_end = scan.seg_size.and_then(|s| seg.data().checked_add(s));
    let avail = win.avail();
    let mut pos = seg.data();
    scan.good_end = pos;
    let mut n = 0usize;
    let end = 'walk: loop {
        if let Some(se) = seg_end {
            if pos == se {
                break End::Done;
            }
            if pos > se {
                scan.err_at = pos;
                break End::Corrupt;
            }
        }
        if pos >= avail {
            break if win.limited() {
                End::Limit
            } else {
                End::Exhausted
            };
        }
        if n >= MAX_CHILDREN {
            scan.err_at = pos;
            break End::Corrupt;
        }
        n += 1;
        let e = match read_elem(win, pos) {
            Rd::Ok(e) => e,
            Rd::Short => {
                scan.err_at = pos;
                break if win.fault().is_some() {
                    End::Fault
                } else if win.limited() && avail - pos >= 12 {
                    End::Limit
                } else {
                    End::Partial
                };
            }
            Rd::Bad => {
                scan.err_at = pos;
                break if seg_end.is_some() {
                    End::Corrupt
                } else {
                    End::NonElement
                };
            }
        };
        if !is_level1(e.id) {
            scan.err_at = pos;
            if seg_end.is_some() {
                // Tolerate a few unknown top-level elements in a sized
                // segment (newer spec revisions), skipping by size.
                if let (Some(ee), true) = (e.end(), scan.unknown_level1 < 4) {
                    if ee <= seg_end.unwrap_or(0) && ee <= avail {
                        scan.unknown_level1 += 1;
                        pos = ee;
                        scan.good_end = pos;
                        continue;
                    }
                }
                break End::Corrupt;
            }
            break End::NonElement;
        }
        match e.size {
            Some(_) => {
                let ee = e.end()?;
                if seg_end.is_some_and(|se| ee > se) {
                    scan.err_at = pos;
                    break End::Corrupt;
                }
                match e.id {
                    ID_CLUSTER => {
                        let mut ts = 0u64;
                        if ee > avail {
                            // The last cluster is cut: keep what is complete.
                            let (p2, why) =
                                walk_cluster(win, e.data(), None, seg_end, &mut scan, &mut ts);
                            scan.clusters += 1;
                            scan.first_cluster_ts.get_or_insert(ts);
                            scan.last_cluster = Some((e.data(), None, ts));
                            // Include the cluster header and the complete
                            // children; an unterminated cluster has no
                            // meaningful known size.
                            scan.good_end = p2;
                            scan.err_at = p2;
                            break match why {
                                End::NonElement | End::Exhausted | End::Partial => {
                                    if win.limited() {
                                        End::Limit
                                    } else {
                                        End::Partial
                                    }
                                }
                                other => other,
                            };
                        }
                        if deep {
                            let (p2, why) =
                                walk_cluster(win, e.data(), Some(ee), seg_end, &mut scan, &mut ts);
                            if why != End::Done {
                                scan.err_at = p2;
                                scan.good_end = p2;
                                break 'walk why;
                            }
                        } else if let Some(v) = first_child_timestamp(win, &e) {
                            ts = v;
                        }
                        scan.clusters += 1;
                        scan.first_cluster_ts.get_or_insert(ts);
                        scan.last_cluster = Some((e.data(), Some(ee), ts));
                    }
                    _ => {
                        if ee > avail {
                            scan.err_at = pos;
                            break if win.limited() {
                                End::Limit
                            } else {
                                End::Partial
                            };
                        }
                        match e.id {
                            ID_INFO => {
                                scan.info = parse_info(win, &e);
                                scan.have_info = true;
                            }
                            ID_TRACKS => scan.tracks = parse_tracks(win, &e),
                            ID_ATTACHMENTS => scan.nested.extend(parse_attachments(win, &e)),
                            ID_CUES => scan.has_cues = true,
                            _ => {}
                        }
                    }
                }
                pos = ee;
                scan.good_end = pos;
            }
            None => {
                // Unknown size is only meaningful for clusters.
                if e.id != ID_CLUSTER {
                    scan.err_at = pos;
                    break if seg_end.is_some() {
                        End::Corrupt
                    } else {
                        End::NonElement
                    };
                }
                let mut ts = 0u64;
                let (p2, why) = walk_cluster(win, e.data(), None, seg_end, &mut scan, &mut ts);
                scan.clusters += 1;
                scan.first_cluster_ts.get_or_insert(ts);
                scan.last_cluster = Some((e.data(), None, ts));
                match why {
                    End::NonElement => {
                        // The cluster ended; the element at p2 decides.
                        pos = p2;
                        scan.good_end = pos;
                        let at_level1 = matches!(read_elem(win, pos), Rd::Ok(x) if is_level1(x.id));
                        if !at_level1 {
                            scan.err_at = pos;
                            break End::NonElement;
                        }
                    }
                    other => {
                        scan.good_end = p2;
                        scan.err_at = p2;
                        break other;
                    }
                }
            }
        }
    };
    scan.end = Some(end);
    if end == End::Done {
        scan.good_end = seg_end.unwrap_or(scan.good_end);
    }
    Some(scan)
}

fn first_child_timestamp(win: &mut Window<'_>, cluster: &Elem) -> Option<u64> {
    // The Timestamp normally comes first, possibly after a CRC-32 or Void.
    let mut p = cluster.data();
    for _ in 0..4 {
        let Rd::Ok(c) = read_elem(win, p) else {
            return None;
        };
        match c.id {
            ID_CL_TIMESTAMP => return read_uint(win, &c),
            ID_CRC32 | ID_VOID => p = c.end()?,
            _ => return None,
        }
    }
    None
}

/// Estimated duration in milliseconds from the last cluster.
fn estimate_duration_ms(win: &mut Window<'_>, scan: &Scan) -> Option<u64> {
    let scale = scan.info.scale.unwrap_or(1_000_000);
    let (data, end, ts) = scan.last_cluster?;
    let mut max_rel = scan.max_block_ts.unwrap_or(0).max(0);
    if scan.max_block_ts.is_none() || end.is_some() {
        // Look at the blocks of the last cluster.
        let mut tmp = Scan::default();
        let mut cts = ts;
        walk_cluster(win, data, end, None, &mut tmp, &mut cts);
        if let Some(m) = tmp.max_block_ts {
            max_rel = max_rel.max(m);
        }
    }
    let ticks = ts.checked_add(max_rel as u64)?;
    let ns = (ticks as u128).checked_mul(u128::from(scale))?;
    Some((ns / 1_000_000) as u64)
}

fn codec_name(id: &str) -> String {
    match id {
        "V_MPEG4/ISO/AVC" => "h264".into(),
        "V_MPEGH/ISO/HEVC" => "hevc".into(),
        "V_VP8" => "vp8".into(),
        "V_VP9" => "vp9".into(),
        "V_AV1" => "av1".into(),
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => "mpeg4".into(),
        "V_MPEG4/MS/V3" => "msmpeg4v3".into(),
        "V_MPEG2" => "mpeg2video".into(),
        "V_MPEG1" => "mpeg1video".into(),
        "V_THEORA" => "theora".into(),
        "V_MJPEG" => "mjpeg".into(),
        "V_UNCOMPRESSED" => "rawvideo".into(),
        "A_AAC" => "aac".into(),
        s if s.starts_with("A_AAC/") => "aac".into(),
        "A_OPUS" => "opus".into(),
        "A_VORBIS" => "vorbis".into(),
        "A_MPEG/L3" => "mp3".into(),
        "A_MPEG/L2" => "mp2".into(),
        "A_MPEG/L1" => "mp1".into(),
        "A_AC3" => "ac3".into(),
        "A_EAC3" => "eac3".into(),
        "A_FLAC" => "flac".into(),
        s if s.starts_with("A_DTS") => "dts".into(),
        s if s.starts_with("A_PCM/") => "pcm".into(),
        "A_TRUEHD" => "truehd".into(),
        s => s.to_ascii_lowercase(),
    }
}

fn build_meta(win: &mut Window<'_>, scan: &Scan) -> MediaMeta {
    let mut m = MediaMeta::default();
    let video = scan.tracks.iter().find(|t| t.ttype == 1);
    let audio = scan.tracks.iter().find(|t| t.ttype == 2);
    if let Some(v) = video {
        if v.width > 0 && v.height > 0 {
            m.width = Some(v.width);
            m.height = Some(v.height);
        }
        m.video_codec = Some(codec_name(&v.codec));
    }
    if let Some(a) = audio {
        m.audio_codec = Some(codec_name(&a.codec));
        if a.sample_rate > 0.0 {
            m.extra.insert(
                "audio_sample_rate".into(),
                format!("{}", a.sample_rate as u64),
            );
        }
        if a.channels > 0 {
            m.extra
                .insert("audio_channels".into(), a.channels.to_string());
        }
    }
    let subs = scan.tracks.iter().filter(|t| t.ttype == 17).count();
    if subs > 0 {
        m.extra.insert("subtitle_tracks".into(), subs.to_string());
    }
    m.extra
        .insert("tracks".into(), scan.tracks.len().to_string());
    m.extra.insert("doctype".into(), scan.doctype.clone());
    m.extra.insert(
        "cues".into(),
        if scan.has_cues { "yes" } else { "no" }.into(),
    );
    m.extra.insert("clusters".into(), scan.clusters.to_string());
    let scale = scan.info.scale.unwrap_or(1_000_000);
    let from_info = scan
        .info
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * scale as f64 / 1_000_000.0).round() as u64);
    m.duration_ms = from_info.or_else(|| estimate_duration_ms(win, scan));
    if from_info.is_none() && m.duration_ms.is_some() {
        m.extra
            .insert("duration_source".into(), "last cluster".into());
    }
    m.software = scan
        .info
        .writing
        .clone()
        .or_else(|| scan.info.muxing.clone());
    if let Some(ns) = scan.info.date_ns {
        // Nanoseconds since 2001-01-01T00:00:00Z.
        let secs = 978_307_200i64.saturating_add(ns.div_euclid(1_000_000_000));
        let ts = Timestamp::from_unix(secs);
        if ts.is_plausible() {
            m.created = Some(ts);
        }
    }
    if let Some(t) = &scan.info.title {
        m.extra.insert("title".into(), t.clone());
    }
    let ext = if scan.doctype == "webm" {
        "webm"
    } else if video.is_none() && audio.is_some() {
        "mka"
    } else {
        "mkv"
    };
    m.extra.insert(EXTENSION_KEY.into(), ext.into());
    m
}

/// Format handler for Matroska and WebM.
#[derive(Debug, Default, Clone, Copy)]
pub struct MkvFormat;

static SIGNATURES: [Signature; 1] = [Signature::at_start(&[0x1A, 0x45, 0xDF, 0xA3])];

impl FileFormat for MkvFormat {
    fn id(&self) -> &'static str {
        "mkv"
    }

    fn name(&self) -> &'static str {
        "Matroska / WebM video"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mkv", "webm", "mka"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        64
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        60
    }

    fn probe(&self, head: &[u8]) -> bool {
        let src = diskrescue_core::source::MemSource::new(head.to_vec());
        let mut win = Window::new(&src, 0, u64::MAX);
        read_ebml_header(&mut win).is_some()
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "jpg" | "png")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(scan) = scan_segment(&mut win, false) else {
            return Ok(None);
        };
        // A header with nothing behind it is not a recoverable file.
        if !scan.have_info && scan.tracks.is_empty() && scan.clusters == 0 {
            return Ok(None);
        }
        let end = scan.end.unwrap_or(End::NonElement);
        let (status, err_offset) = match end {
            End::Done => (ExtentStatus::Complete, None),
            End::Limit => (ExtentStatus::Capped, None),
            End::Exhausted => {
                if scan.seg_size.is_some() {
                    (ExtentStatus::Truncated, Some(scan.good_end))
                } else {
                    (ExtentStatus::UnknownLength, None)
                }
            }
            End::NonElement => {
                if scan.seg_size.is_some() {
                    (ExtentStatus::Truncated, Some(scan.err_at))
                } else {
                    (ExtentStatus::UnknownLength, None)
                }
            }
            End::Partial | End::Corrupt | End::Fault => (
                ExtentStatus::Truncated,
                Some(scan.err_at.max(scan.good_end)),
            ),
        };
        let len = scan.good_end;
        if len < 64 {
            return Ok(None);
        }
        let mut notes = scan.notes.clone();
        if end == End::Partial {
            notes.push(format!(
                "last element at offset {} is incomplete; it was left out",
                scan.err_at
            ));
        } else if end == End::Corrupt {
            notes.push(format!("inconsistent element at offset {}", scan.err_at));
        } else if end == End::Fault {
            notes.push(format!("read error near offset {}", scan.err_at));
        }
        if scan.seg_size.is_none() && matches!(end, End::NonElement | End::Exhausted | End::Limit) {
            notes.push("Segment has unknown size (live or unfinalised recording)".into());
        }
        let meta = build_meta(&mut win, &scan);
        let mut conf = match (status, scan.have_info && !scan.tracks.is_empty()) {
            (ExtentStatus::Complete, true) => Confidence::HIGH,
            (ExtentStatus::Complete, false) => Confidence::MEDIUM,
            (_, true) => Confidence::MEDIUM,
            _ => Confidence::LOW,
        };
        if scan.clusters == 0 {
            conf = conf.lowered(20);
        }
        let mut m = Measured {
            len,
            status,
            confidence: conf,
            err_offset,
            meta,
            nested: scan
                .nested
                .iter()
                .filter(|n| n.range.end <= len)
                .cloned()
                .collect(),
            notes,
        };
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut win = Window::new(&**src, 0, u64::MAX);
        let Some(scan) = scan_segment(&mut win, true) else {
            return Ok(Validation::invalid(
                "not a Matroska/WebM file (no valid EBML header and Segment)",
            ));
        };
        let size = win.avail();
        let mut problems = Vec::new();
        let end = scan.end.unwrap_or(End::NonElement);
        match end {
            End::Done => {}
            End::Exhausted | End::Limit => {
                if let Some(s) = scan.seg_size {
                    problems.push(format!(
                        "Segment declares {s} bytes but the file ends at {size} (truncated)"
                    ));
                }
            }
            End::NonElement => {
                if scan.seg_size.is_some() {
                    problems.push(format!(
                        "unexpected data at offset {} inside the Segment",
                        scan.err_at
                    ));
                }
            }
            End::Partial => problems.push(format!(
                "the file ends inside an element at offset {} (truncated)",
                scan.err_at
            )),
            End::Corrupt => problems.push(format!(
                "inconsistent element at offset {} (size overruns its parent)",
                scan.err_at
            )),
            End::Fault => problems.push(format!("read error near offset {}", scan.err_at)),
        }
        if scan.good_end < size && !scan.bad_end() && problems.is_empty() {
            problems.push(format!(
                "{} trailing bytes after the Segment are not part of the file",
                size - scan.good_end
            ));
        }
        if scan.tracks.is_empty() {
            problems.push("no Tracks element".into());
        }
        if scan.clusters == 0 {
            problems.push("no Cluster elements (no media data)".into());
        }
        let meta = build_meta(&mut win, &scan);
        let status = if problems.is_empty() {
            ValidationStatus::Valid
        } else {
            ValidationStatus::Damaged
        };
        let intact = if problems.is_empty() {
            1.0
        } else {
            (scan.good_end as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
        };
        Ok(Validation {
            status,
            intact_fraction: Some(intact),
            problems,
            meta,
            repairable_by: Vec::new(),
        })
    }
}
