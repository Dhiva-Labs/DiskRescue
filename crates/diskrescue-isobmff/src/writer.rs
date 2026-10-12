//! Writing ISO BMFF: a small box builder, a `moov` generator driven by
//! per-sample lists, and [`SpliceSource`], a virtual file assembled from
//! in-memory header bytes and byte ranges of the damaged source.
//!
//! Repairs never build the output file in memory: they describe it as a
//! splice (ftyp + new moov + mdat header from memory, media bytes from the
//! source), re-validate that description through the normal [`Source`]
//! interface, and then stream it to the caller's writer.
//!
//! Layout written by [`assemble`]: `ftyp`, `moov`, `mdat` (fast-start), the
//! mdat holding the media samples of every track in their original file
//! order. Chunks are runs of consecutive samples of one track, capped at
//! [`MovieOut::max_chunk_samples`]. `co64` is used automatically when an
//! offset needs more than 32 bits, `mdat` gets a 64-bit size when larger
//! than 4 GiB, `mvhd`/`tkhd`/`mdhd` switch to version 1 when a duration
//! needs it.

use std::io::Write;
use std::sync::Arc;

use diskrescue_core::{Error, ReadError, Result, SharedSource, Source};

use crate::boxes::{Elst, Mvhd, Trak, IDENTITY};

/// Incremental box builder.
#[derive(Debug, Default, Clone)]
pub struct BoxBuf {
    /// Bytes written so far.
    pub buf: Vec<u8>,
    stack: Vec<usize>,
}

impl BoxBuf {
    /// Empty builder.
    pub fn new() -> Self {
        Self::default()
    }
    /// Open a box.
    pub fn start(&mut self, kind: &[u8; 4]) {
        self.stack.push(self.buf.len());
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.buf.extend_from_slice(kind);
    }
    /// Open a FullBox.
    pub fn start_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) {
        self.start(kind);
        self.u32(((version as u32) << 24) | (flags & 0x00FF_FFFF));
    }
    /// Close the innermost open box and patch its size.
    pub fn end(&mut self) {
        if let Some(at) = self.stack.pop() {
            let size = (self.buf.len() - at) as u32;
            self.buf[at..at + 4].copy_from_slice(&size.to_be_bytes());
        }
    }
    /// Append raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    /// Append a u8.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    /// Append a big-endian u16.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    /// Append a big-endian u32.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    /// Append a big-endian u64.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    /// Append `n` zero bytes.
    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
}

/// One sample (or a run of equal-size PCM samples) to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutSample {
    /// Offset of the bytes in the source.
    pub src_offset: u64,
    /// Bytes (for a run: total bytes of the run).
    pub size: u32,
    /// Duration of each sample in media ticks.
    pub duration: u32,
    /// Composition offset.
    pub cts_offset: i32,
    /// Sync sample.
    pub keyframe: bool,
    /// Number of table samples this entry stands for (PCM runs), else 1.
    pub count: u32,
    /// Bytes appended to the sample in the output (an end-of-sequence NAL
    /// unit that makes decoders flush before a restart after lost data).
    pub suffix: Option<NalSuffix>,
}

impl OutSample {
    /// Bytes the sample occupies in the output.
    pub fn stored_size(&self) -> u64 {
        self.size as u64 + self.suffix.map_or(0, |s| s.len as u64)
    }
}

/// A few bytes appended to a sample (see [`OutSample::suffix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalSuffix {
    /// The bytes; only the first `len` are used.
    pub bytes: [u8; 8],
    /// Number of bytes used (at most 8).
    pub len: u8,
}

impl NalSuffix {
    /// A length-prefixed NAL unit (`nal_length_size` bytes of length, then
    /// `nal`); `None` when it does not fit in 8 bytes.
    pub fn nal(nal_length_size: usize, nal: &[u8]) -> Option<NalSuffix> {
        let total = nal_length_size.checked_add(nal.len())?;
        if !(1..=4).contains(&nal_length_size) || total > 8 {
            return None;
        }
        let mut bytes = [0u8; 8];
        let len = (nal.len() as u32).to_be_bytes();
        bytes[..nal_length_size].copy_from_slice(&len[4 - nal_length_size..]);
        bytes[nal_length_size..total].copy_from_slice(nal);
        Some(NalSuffix {
            bytes,
            len: total as u8,
        })
    }

    /// The used bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..(self.len as usize).min(8)]
    }
}

/// A track to write.
#[derive(Debug, Clone)]
pub struct TrackOut {
    /// Track supplying `tkhd`, `mdhd`, `hdlr`, `minf` headers and `stsd`.
    pub template: Trak,
    /// Track id to write.
    pub track_id: u32,
    /// Samples in decode order.
    pub samples: Vec<OutSample>,
    /// Edit list to write (`None`: no `edts`). When `fit_elst` is set the
    /// last entry with a non-negative media time gets its duration set to
    /// the remaining media duration.
    pub elst: Option<Vec<Elst>>,
    /// Recompute the edit duration from the new media duration.
    pub fit_elst: bool,
    /// All samples have this size (`stsz` with a constant size).
    pub constant_size: Option<u32>,
}

impl TrackOut {
    /// Total table samples.
    pub fn sample_count(&self) -> u64 {
        self.samples.iter().map(|s| s.count as u64).sum()
    }
    /// Media duration in ticks.
    pub fn media_duration(&self) -> u64 {
        self.samples
            .iter()
            .map(|s| s.duration as u64 * s.count as u64)
            .sum()
    }
}

/// A movie to write.
#[derive(Debug, Clone)]
pub struct MovieOut {
    /// The complete `ftyp` box (header included), or empty for none.
    pub ftyp: Vec<u8>,
    /// Movie header template (timescale, times, rate, volume, matrix).
    pub mvhd: Mvhd,
    /// Tracks.
    pub tracks: Vec<TrackOut>,
    /// Raw boxes appended inside `moov` (`udta`, `meta`).
    pub user_boxes: Vec<Vec<u8>>,
    /// Maximum samples per chunk.
    pub max_chunk_samples: u32,
}

/// Where each track's chunks land in the output mdat.
struct Layout {
    /// Per track: chunks as (relative offset in mdat payload, table samples).
    chunks: Vec<Vec<(u64, u32)>>,
    /// Media pieces in output order.
    ranges: Vec<Piece>,
    /// Total mdat payload bytes.
    total: u64,
}

/// One piece of the output `mdat`.
enum Piece {
    /// `(offset, len)` in the source.
    Src(u64, u64),
    /// Bytes from memory.
    Mem(Vec<u8>),
}

fn layout(movie: &MovieOut) -> Layout {
    let mut order: Vec<(u64, usize, usize)> = Vec::new();
    for (t, tr) in movie.tracks.iter().enumerate() {
        for (i, s) in tr.samples.iter().enumerate() {
            order.push((s.src_offset, t, i));
        }
    }
    // File order keeps the original interleave. A track whose samples are
    // not in increasing source order (unusual writers) cannot be laid out
    // that way without breaking its chunk arithmetic, so fall back to one
    // track after another.
    let monotone = movie.tracks.iter().all(|t| {
        t.samples
            .windows(2)
            .all(|w| w[1].src_offset >= w[0].src_offset)
    });
    if monotone {
        order.sort_by_key(|&(off, t, i)| (off, t, i));
    }
    let mut chunks: Vec<Vec<(u64, u32)>> = vec![Vec::new(); movie.tracks.len()];
    let mut ranges: Vec<Piece> = Vec::new();
    let mut rel = 0u64;
    let mut cur: Option<usize> = None;
    let cap = movie.max_chunk_samples.max(1);
    for &(off, t, i) in &order {
        let s = &movie.tracks[t].samples[i];
        let need_new = match cur {
            Some(ct) if ct == t => chunks[t]
                .last()
                .is_none_or(|c| c.1.saturating_add(s.count) > cap),
            _ => true,
        };
        if need_new {
            chunks[t].push((rel, 0));
        }
        if let Some(c) = chunks[t].last_mut() {
            c.1 += s.count;
        }
        cur = Some(t);
        match ranges.last_mut() {
            Some(Piece::Src(o, l)) if *o + *l == off => *l += s.size as u64,
            _ => ranges.push(Piece::Src(off, s.size as u64)),
        }
        if let Some(sfx) = &s.suffix {
            ranges.push(Piece::Mem(sfx.as_slice().to_vec()));
        }
        rel += s.stored_size();
    }
    Layout {
        chunks,
        ranges,
        total: rel,
    }
}

fn rle<T: PartialEq + Copy>(items: impl Iterator<Item = (u32, T)>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for (n, v) in items {
        if n == 0 {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 == v && last.0.checked_add(n).is_some() => last.0 += n,
            _ => out.push((n, v)),
        }
    }
    out
}

fn scale(v: u64, from: u32, to: u32) -> u64 {
    if from == 0 {
        return 0;
    }
    ((v as u128 * to as u128 + from as u128 / 2) / from as u128) as u64
}

struct TrackDur {
    media: u64,
    movie: u64,
    elst: Option<Vec<Elst>>,
}

fn track_durations(t: &TrackOut, movie_ts: u32) -> TrackDur {
    let media = t.media_duration();
    let ts = t.template.mdhd.timescale.max(1);
    let elst = t.elst.clone().map(|mut entries| {
        if t.fit_elst {
            if let Some(e) = entries.iter_mut().rev().find(|e| e.media_time >= 0) {
                // The edit covers the composition timeline from its media
                // time to the end of the last presented sample.
                let mut dts = 0i64;
                let mut cts_end = 0i64;
                for s in &t.samples {
                    let span = s.duration as i64 * s.count as i64;
                    cts_end = cts_end.max(dts + s.cts_offset as i64 + span);
                    dts += span;
                }
                let remaining = (cts_end - e.media_time).max(0) as u64;
                e.segment_duration = scale(remaining, ts, movie_ts);
            }
        }
        entries
    });
    let movie = match &elst {
        Some(e) if !e.is_empty() => e.iter().map(|x| x.segment_duration).sum(),
        _ => scale(media, ts, movie_ts),
    };
    TrackDur { media, movie, elst }
}

fn write_mvhd(b: &mut BoxBuf, m: &Mvhd, duration: u64, next_id: u32) {
    let v1 = duration > u32::MAX as u64
        || m.creation_time > u32::MAX as u64
        || m.modification_time > u32::MAX as u64;
    b.start_full(b"mvhd", v1 as u8, 0);
    if v1 {
        b.u64(m.creation_time);
        b.u64(m.modification_time);
        b.u32(m.timescale);
        b.u64(duration);
    } else {
        b.u32(m.creation_time as u32);
        b.u32(m.modification_time as u32);
        b.u32(m.timescale);
        b.u32(duration as u32);
    }
    b.u32(if m.rate == 0 { 0x0001_0000 } else { m.rate });
    b.u16(if m.volume == 0 { 0x0100 } else { m.volume });
    b.zeros(10);
    let matrix = if m.matrix == [0; 9] {
        IDENTITY
    } else {
        m.matrix
    };
    for v in matrix {
        b.u32(v);
    }
    b.zeros(24);
    b.u32(next_id);
    b.end();
}

fn write_tkhd(b: &mut BoxBuf, t: &Trak, track_id: u32, duration: u64) {
    let k = &t.tkhd;
    let v1 = duration > u32::MAX as u64
        || k.creation_time > u32::MAX as u64
        || k.modification_time > u32::MAX as u64;
    let flags = if k.flags & 0x3 == 0 { 0x3 } else { k.flags };
    b.start_full(b"tkhd", v1 as u8, flags);
    if v1 {
        b.u64(k.creation_time);
        b.u64(k.modification_time);
        b.u32(track_id);
        b.u32(0);
        b.u64(duration);
    } else {
        b.u32(k.creation_time as u32);
        b.u32(k.modification_time as u32);
        b.u32(track_id);
        b.u32(0);
        b.u32(duration as u32);
    }
    b.zeros(8);
    b.u16(k.layer as u16);
    b.u16(k.alternate_group as u16);
    b.u16(k.volume as u16);
    b.u16(0);
    let matrix = if k.matrix == [0; 9] {
        IDENTITY
    } else {
        k.matrix
    };
    for v in matrix {
        b.u32(v);
    }
    b.u32(k.width_fixed);
    b.u32(k.height_fixed);
    b.end();
}

fn write_mdhd(b: &mut BoxBuf, t: &Trak, duration: u64) {
    let m = &t.mdhd;
    let v1 = duration > u32::MAX as u64
        || m.creation_time > u32::MAX as u64
        || m.modification_time > u32::MAX as u64;
    b.start_full(b"mdhd", v1 as u8, 0);
    if v1 {
        b.u64(m.creation_time);
        b.u64(m.modification_time);
        b.u32(m.timescale);
        b.u64(duration);
    } else {
        b.u32(m.creation_time as u32);
        b.u32(m.modification_time as u32);
        b.u32(m.timescale);
        b.u32(duration as u32);
    }
    b.u16(m.language_code);
    b.u16(m.quality);
    b.end();
}

fn write_elst(b: &mut BoxBuf, entries: &[Elst]) {
    let v1 = entries.iter().any(|e| {
        e.segment_duration > u32::MAX as u64
            || e.media_time > i32::MAX as i64
            || e.media_time < i32::MIN as i64
    });
    b.start(b"edts");
    b.start_full(b"elst", v1 as u8, 0);
    b.u32(entries.len() as u32);
    for e in entries {
        if v1 {
            b.u64(e.segment_duration);
            b.u64(e.media_time as u64);
        } else {
            b.u32(e.segment_duration as u32);
            b.u32(e.media_time as i32 as u32);
        }
        b.u16(e.rate_integer as u16);
        b.u16(e.rate_fraction as u16);
    }
    b.end();
    b.end();
}

fn default_hdlr(handler: &[u8; 4]) -> Vec<u8> {
    let mut b = BoxBuf::new();
    b.start_full(b"hdlr", 0, 0);
    b.u32(0);
    b.bytes(handler);
    b.zeros(12);
    b.bytes(match handler {
        b"vide" => b"VideoHandler\0",
        b"soun" => b"SoundHandler\0",
        _ => b"DataHandler\0",
    });
    b.end();
    b.buf
}

fn default_minf_boxes(handler: &[u8; 4]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut b = BoxBuf::new();
    match handler {
        b"vide" => {
            b.start_full(b"vmhd", 0, 1);
            b.zeros(8);
            b.end();
        }
        b"soun" => {
            b.start_full(b"smhd", 0, 0);
            b.zeros(4);
            b.end();
        }
        _ => {
            b.start_full(b"nmhd", 0, 0);
            b.end();
        }
    }
    out.push(b.buf);
    let mut d = BoxBuf::new();
    d.start(b"dinf");
    d.start_full(b"dref", 0, 0);
    d.u32(1);
    d.start_full(b"url ", 0, 1);
    d.end();
    d.end();
    d.end();
    out.push(d.buf);
    out
}

fn write_stbl(b: &mut BoxBuf, t: &TrackOut, chunks: &[(u64, u32)], base: u64, co64: bool) {
    b.start(b"stbl");
    // stsd
    b.start_full(b"stsd", 0, 0);
    b.u32(t.template.stsd.len() as u32);
    for e in &t.template.stsd {
        b.bytes(&e.raw);
    }
    b.end();
    // stts
    let stts = rle(t.samples.iter().map(|s| (s.count, s.duration)));
    b.start_full(b"stts", 0, 0);
    b.u32(stts.len() as u32);
    for (n, d) in &stts {
        b.u32(*n);
        b.u32(*d);
    }
    b.end();
    // ctts
    if t.samples.iter().any(|s| s.cts_offset != 0) {
        let v1 = t.samples.iter().any(|s| s.cts_offset < 0);
        let ctts = rle(t.samples.iter().map(|s| (s.count, s.cts_offset)));
        b.start_full(b"ctts", v1 as u8, 0);
        b.u32(ctts.len() as u32);
        for (n, o) in &ctts {
            b.u32(*n);
            b.u32(*o as u32);
        }
        b.end();
    }
    // stss
    if t.samples.iter().any(|s| !s.keyframe) {
        let mut idx = 1u32;
        let mut keys = Vec::new();
        for s in &t.samples {
            if s.keyframe {
                keys.push(idx);
            }
            idx = idx.saturating_add(s.count);
        }
        b.start_full(b"stss", 0, 0);
        b.u32(keys.len() as u32);
        for k in keys {
            b.u32(k);
        }
        b.end();
    }
    // stsc
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        if stsc.last().is_none_or(|l| l.1 != c.1) {
            stsc.push((i as u32 + 1, c.1));
        }
    }
    b.start_full(b"stsc", 0, 0);
    b.u32(stsc.len() as u32);
    for (first, n) in stsc {
        b.u32(first);
        b.u32(n);
        b.u32(1);
    }
    b.end();
    // stsz
    b.start_full(b"stsz", 0, 0);
    match t.constant_size {
        Some(sz) => {
            b.u32(sz);
            b.u32(t.sample_count() as u32);
        }
        None => {
            b.u32(0);
            b.u32(t.samples.len() as u32);
            for s in &t.samples {
                b.u32(s.stored_size().min(u32::MAX as u64) as u32);
            }
        }
    }
    b.end();
    // stco / co64
    if co64 {
        b.start_full(b"co64", 0, 0);
        b.u32(chunks.len() as u32);
        for c in chunks {
            b.u64(base + c.0);
        }
    } else {
        b.start_full(b"stco", 0, 0);
        b.u32(chunks.len() as u32);
        for c in chunks {
            b.u32((base + c.0) as u32);
        }
    }
    b.end();
    b.end();
}

fn build_moov(movie: &MovieOut, lay: &Layout, base: u64, co64: bool) -> Vec<u8> {
    let movie_ts = movie.mvhd.timescale.max(1);
    let durs: Vec<TrackDur> = movie
        .tracks
        .iter()
        .map(|t| track_durations(t, movie_ts))
        .collect();
    let movie_dur = durs.iter().map(|d| d.movie).max().unwrap_or(0);
    let next_id = movie.tracks.iter().map(|t| t.track_id).max().unwrap_or(0) + 1;
    let mut b = BoxBuf::new();
    b.start(b"moov");
    write_mvhd(&mut b, &movie.mvhd, movie_dur, next_id);
    for (i, t) in movie.tracks.iter().enumerate() {
        let d = &durs[i];
        b.start(b"trak");
        write_tkhd(&mut b, &t.template, t.track_id, d.movie);
        if let Some(e) = &d.elst {
            if !e.is_empty() {
                write_elst(&mut b, e);
            }
        }
        b.start(b"mdia");
        write_mdhd(&mut b, &t.template, d.media);
        if t.template.raw.hdlr.is_empty() {
            b.bytes(&default_hdlr(&t.template.handler));
        } else {
            b.bytes(&t.template.raw.hdlr);
        }
        b.start(b"minf");
        if t.template.raw.minf_boxes.is_empty() {
            for x in default_minf_boxes(&t.template.handler) {
                b.bytes(&x);
            }
        } else {
            for x in &t.template.raw.minf_boxes {
                b.bytes(x);
            }
        }
        write_stbl(&mut b, t, &lay.chunks[i], base, co64);
        b.end(); // minf
        b.end(); // mdia
        b.end(); // trak
    }
    for u in &movie.user_boxes {
        b.bytes(u);
    }
    b.end();
    b.buf
}

/// Build the output description of `movie` whose media bytes come from
/// `src`.
pub fn assemble(movie: &MovieOut, src: SharedSource) -> Result<SpliceSource> {
    if movie.tracks.is_empty() || movie.tracks.iter().all(|t| t.samples.is_empty()) {
        return Err(Error::InvalidArgument("no samples to write".into()));
    }
    for t in &movie.tracks {
        if t.template.stsd.is_empty() {
            return Err(Error::InvalidArgument(format!(
                "track {} has no sample description",
                t.track_id
            )));
        }
    }
    let lay = layout(movie);
    let mdat_hdr: u64 = if lay.total + 8 > u32::MAX as u64 {
        16
    } else {
        8
    };
    // First pass with 32-bit offsets to learn the moov size.
    let probe = build_moov(movie, &lay, 0, false);
    let base32 = movie.ftyp.len() as u64 + probe.len() as u64 + mdat_hdr;
    let co64 = base32 + lay.total > u32::MAX as u64;
    let moov = if co64 {
        let probe64 = build_moov(movie, &lay, 0, true);
        let base = movie.ftyp.len() as u64 + probe64.len() as u64 + mdat_hdr;
        build_moov(movie, &lay, base, true)
    } else {
        build_moov(movie, &lay, base32, false)
    };
    let mut head = movie.ftyp.clone();
    head.extend_from_slice(&moov);
    if mdat_hdr == 16 {
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        head.extend_from_slice(&(lay.total + 16).to_be_bytes());
    } else {
        head.extend_from_slice(&((lay.total + 8) as u32).to_be_bytes());
        head.extend_from_slice(b"mdat");
    }
    let mut sp = SpliceSource::new(src, "rebuilt movie");
    sp.push_mem(head);
    for piece in lay.ranges {
        match piece {
            Piece::Src(off, len) => sp.push_src(off, len),
            Piece::Mem(b) => sp.push_mem(b),
        }
    }
    Ok(sp)
}

#[derive(Clone)]
enum SegData {
    Mem(Arc<Vec<u8>>),
    Src(u64),
}

#[derive(Clone)]
struct Seg {
    start: u64,
    len: u64,
    data: SegData,
}

/// A virtual file made of in-memory pieces and ranges of another source.
#[derive(Clone)]
pub struct SpliceSource {
    segs: Vec<Seg>,
    size: u64,
    src: SharedSource,
    name: String,
}

impl SpliceSource {
    /// Empty splice over `src`.
    pub fn new(src: SharedSource, name: impl Into<String>) -> Self {
        SpliceSource {
            segs: Vec::new(),
            size: 0,
            src,
            name: name.into(),
        }
    }

    /// Append in-memory bytes.
    pub fn push_mem(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let len = bytes.len() as u64;
        self.segs.push(Seg {
            start: self.size,
            len,
            data: SegData::Mem(Arc::new(bytes)),
        });
        self.size += len;
    }

    /// Append `len` bytes of the underlying source starting at `offset`.
    pub fn push_src(&mut self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        if let Some(last) = self.segs.last_mut() {
            if let SegData::Src(o) = last.data {
                if o + last.len == offset {
                    last.len += len;
                    self.size += len;
                    return;
                }
            }
        }
        self.segs.push(Seg {
            start: self.size,
            len,
            data: SegData::Src(offset),
        });
        self.size += len;
    }

    /// Stream the whole virtual file into `out`; returns bytes written.
    pub fn write_to(&self, out: &mut dyn Write) -> Result<u64> {
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        for s in &self.segs {
            match &s.data {
                SegData::Mem(m) => {
                    out.write_all(m)?;
                    written += m.len() as u64;
                }
                SegData::Src(o) => {
                    let mut done = 0u64;
                    while done < s.len {
                        let n = (s.len - done).min(buf.len() as u64) as usize;
                        let got = self.src.read_at(o + done, &mut buf[..n])?;
                        if got == 0 {
                            return Err(Error::Read(ReadError::out_of_range(
                                o + done,
                                n,
                                self.src.size(),
                            )));
                        }
                        out.write_all(&buf[..got])?;
                        done += got as u64;
                        written += got as u64;
                    }
                }
            }
        }
        out.flush()?;
        Ok(written)
    }
}

impl Source for SpliceSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        if offset >= self.size {
            return Ok(0);
        }
        let mut done = 0usize;
        let mut idx = self.segs.partition_point(|s| s.start + s.len <= offset);
        while done < buf.len() && idx < self.segs.len() {
            let s = &self.segs[idx];
            let pos = offset + done as u64;
            let within = pos - s.start;
            let n = ((s.len - within) as usize).min(buf.len() - done);
            match &s.data {
                SegData::Mem(m) => {
                    buf[done..done + n].copy_from_slice(&m[within as usize..within as usize + n]);
                }
                SegData::Src(o) => {
                    let got = self.src.read_at(o + within, &mut buf[done..done + n])?;
                    if got < n {
                        return Ok(done + got);
                    }
                }
            }
            done += n;
            idx += 1;
        }
        Ok(done)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::{MemSource, SourceExt};

    #[test]
    fn splice_reads_across_segments() {
        let base: SharedSource = Arc::new(MemSource::new((0..100u8).collect()));
        let mut s = SpliceSource::new(base, "t");
        s.push_mem(vec![200, 201]);
        s.push_src(10, 3);
        s.push_src(13, 2);
        s.push_mem(vec![255]);
        assert_eq!(s.size(), 8);
        assert_eq!(
            s.read_exact_vec(0, 8).unwrap(),
            vec![200, 201, 10, 11, 12, 13, 14, 255]
        );
        assert_eq!(s.read_vec(3, 100).unwrap(), vec![11, 12, 13, 14, 255]);
        let mut out = Vec::new();
        assert_eq!(s.write_to(&mut out).unwrap(), 8);
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn run_length() {
        let v = rle([(1u32, 5u32), (2, 5), (1, 6), (0, 9)].into_iter());
        assert_eq!(v, vec![(3, 5), (1, 6)]);
    }
}
