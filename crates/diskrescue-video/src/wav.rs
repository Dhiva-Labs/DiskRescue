//! WAV (RIFF `WAVE`, RF64) carving, validation and metadata.
//!
//! # Algorithm
//!
//! The chunks after the 12-byte header are walked: `fmt ` (format tag,
//! channels, sample rate, block align, bits), `data`, and the metadata
//! chunks (`LIST INFO`, `bext`). When the RIFF size and the `data` size agree
//! with the chunk walk, the extent is `8 + RIFF size` (rounded up to even)
//! and the status is `Complete`. RF64 files take the 64-bit sizes from the
//! `ds64` chunk.
//!
//! Recorders that stream into a file and never finish it leave the RIFF size
//! or the `data` size at 0 or `0xFFFFFFFF`. For integer and float PCM the
//! data is then scanned in 4 KiB blocks for "audio-likeness" (small
//! sample-to-sample differences; compressed or random data fails), the first
//! block that is not audio ends the data, trailing all-zero blocks are
//! trimmed, and the status is `UnknownLength` with low confidence. For other
//! formats the data is taken to run to the end of the available bytes.
//!
//! A file cut short (declared size beyond the available bytes) is
//! `Truncated`, ending on a whole sample frame.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes;
use diskrescue_core::{Result, SharedSource};

use crate::avi::hdr::audio_codec;
use crate::io::{note_unreadable, Window};
use crate::meta::{clean_string, parse_loose_date, EXTENSION_KEY};

const MAX_SIZE: u64 = 4 << 30;
const MAX_CHUNKS: usize = 64;
const SCAN_BLOCK: usize = 4096;
const UNSET: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone, Copy)]
struct Fmt {
    tag: u16,
    channels: u16,
    sample_rate: u32,
    avg_bytes: u32,
    block_align: u16,
    bits: u16,
}

impl Fmt {
    fn plausible(&self) -> bool {
        self.channels >= 1
            && self.channels <= 64
            && (100..=1_000_000).contains(&self.sample_rate)
            && self.block_align >= 1
            && self.bits <= 64
    }

    fn is_pcm(&self) -> bool {
        matches!(self.tag, 1 | 3) && matches!(self.bits, 8 | 16 | 24 | 32 | 64)
    }
}

#[derive(Debug, Default)]
struct Parsed {
    rf64: bool,
    riff_size: u64,
    riff_size_set: bool,
    fmt: Option<Fmt>,
    data_pos: Option<u64>,
    data_len: u64,
    data_len_set: bool,
    /// Offset just past the chunks walked.
    walked_end: u64,
    software: Option<String>,
    created: Option<Timestamp>,
    title: Option<String>,
    originator: Option<String>,
    problems: Vec<String>,
}

fn parse_fmt(b: &[u8]) -> Option<Fmt> {
    let mut f = Fmt {
        tag: bytes::u16le(b, 0)?,
        channels: bytes::u16le(b, 2)?,
        sample_rate: bytes::u32le(b, 4)?,
        avg_bytes: bytes::u32le(b, 8)?,
        block_align: bytes::u16le(b, 12)?,
        bits: bytes::u16le(b, 14)?,
    };
    if f.tag == 0xFFFE && b.len() >= 26 {
        f.tag = bytes::u16le(b, 24)?;
    }
    Some(f)
}

fn parse(win: &mut Window<'_>) -> Option<Parsed> {
    let h = win.vec(0, 12)?;
    let rf64 = match &h[..4] {
        b"RIFF" => false,
        b"RF64" => true,
        _ => return None,
    };
    if &h[8..12] != b"WAVE" {
        return None;
    }
    let mut p = Parsed {
        rf64,
        riff_size: u64::from(bytes::u32le(&h, 4)?),
        ..Parsed::default()
    };
    p.riff_size_set = !(p.riff_size == 0 || p.riff_size == u64::from(UNSET));
    let avail = win.avail();
    let mut ds64: Option<(u64, u64)> = None;
    let mut pos = 12u64;
    p.walked_end = pos;
    for _ in 0..MAX_CHUNKS {
        let Some(c) = win.vec(pos, 8) else { break };
        let id = [c[0], c[1], c[2], c[3]];
        let size = u64::from(bytes::u32le(&c, 4)?);
        let data = pos + 8;
        match &id {
            b"ds64" if rf64 => {
                if let Some(d) = win.vec(data, 16) {
                    let riff = u64::from_le_bytes(d[0..8].try_into().ok()?);
                    let dat = u64::from_le_bytes(d[8..16].try_into().ok()?);
                    ds64 = Some((riff, dat));
                    p.riff_size = riff;
                    p.riff_size_set = riff != 0 && riff != u64::MAX;
                }
            }
            b"fmt " => {
                if size >= 16 {
                    let n = size.min(40) as usize;
                    if let Some(b) = win.vec(data, n) {
                        p.fmt = parse_fmt(&b).filter(Fmt::plausible);
                    }
                }
                if p.fmt.is_none() {
                    p.problems
                        .push("the fmt chunk is missing or implausible".into());
                    return Some(p);
                }
            }
            b"data" => {
                p.data_pos = Some(data);
                let (len, set) = if rf64 && size == u64::from(UNSET) {
                    match ds64 {
                        Some((_, d)) => (d, d != 0 && d != u64::MAX),
                        None => (size, false),
                    }
                } else {
                    (size, size != 0 && size != u64::from(UNSET))
                };
                p.data_len = len;
                p.data_len_set = set;
                // The data size is trustworthy only when it fits the RIFF
                // size and the available bytes enough to continue walking.
                if !set {
                    p.walked_end = data;
                    return Some(p);
                }
                let end = data.saturating_add(len);
                if p.riff_size_set && end > 8 + p.riff_size + 1 {
                    p.data_len_set = false;
                    p.walked_end = data;
                    return Some(p);
                }
                if end > avail {
                    p.walked_end = avail;
                    return Some(p);
                }
            }
            b"bext" => {
                if let Some(b) = win.vec(data, size.min(338) as usize) {
                    if b.len() >= 330 {
                        p.originator = clean_string(&b[256..288]);
                        let date = clean_string(&b[320..330]);
                        let time = b.get(330..338).and_then(clean_string);
                        if let Some(d) = date {
                            let s = match time {
                                Some(t) => format!("{d} {t}"),
                                None => d,
                            };
                            p.created = parse_loose_date(&s);
                        }
                    }
                }
            }
            b"LIST" if win.fourcc(data) == Some(*b"INFO") => {
                let end = (data + size).min(avail);
                let mut q = data + 4;
                let mut n = 0;
                while q + 8 <= end && n < 32 {
                    n += 1;
                    let Some(ch) = win.vec(q, 8) else { break };
                    let sz = u64::from(bytes::u32le(&ch, 4)?);
                    if q + 8 + sz > end {
                        break;
                    }
                    let tp = q + 8;
                    let tl = sz.min(256) as usize;
                    match &ch[..4] {
                        b"ISFT" => p.software = win_text(win, tp, tl),
                        b"INAM" => p.title = win_text(win, tp, tl),
                        b"ICRD" if p.created.is_none() => {
                            p.created = win_text(win, tp, tl).as_deref().and_then(parse_loose_date);
                        }
                        _ => {}
                    }
                    q += 8 + sz + (sz & 1);
                }
            }
            _ => {}
        }
        let next = data.saturating_add(size).saturating_add(size & 1);
        if next > avail {
            p.walked_end = avail;
            return Some(p);
        }
        pos = next;
        p.walked_end = pos;
        if p.riff_size_set && pos >= 8 + p.riff_size {
            break;
        }
    }
    Some(p)
}

fn win_text(win: &mut Window<'_>, pos: u64, n: usize) -> Option<String> {
    let b = win.vec(pos, n)?;
    clean_string(&b)
}

/// Is a 4 KiB block of PCM plausibly audio?
fn audio_like(block: &[u8], f: &Fmt) -> bool {
    if block.iter().all(|&b| b == 0) {
        return true;
    }
    let bytes_per = (f.bits as usize / 8).max(1);
    let stride = bytes_per * usize::from(f.channels).max(1);
    if block.len() < stride * 8 {
        return true;
    }
    match (f.tag, f.bits) {
        (3, 32) => {
            let mut sane = 0usize;
            let mut total = 0usize;
            for s in block.chunks_exact(4) {
                let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                total += 1;
                if v.is_finite() && v.abs() <= 8.0 {
                    sane += 1;
                }
            }
            sane * 100 >= total * 95
        }
        (3, 64) => {
            let mut sane = 0usize;
            let mut total = 0usize;
            for s in block.chunks_exact(8) {
                let v = f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]);
                total += 1;
                if v.is_finite() && v.abs() <= 8.0 {
                    sane += 1;
                }
            }
            sane * 100 >= total * 95
        }
        (_, 8) => {
            // Unsigned 8-bit: look at the first channel.
            let mut sum = 0u64;
            let mut n = 0u64;
            let mut prev: Option<i32> = None;
            for fr in block.chunks_exact(stride) {
                let v = i32::from(fr[0]);
                if let Some(p) = prev {
                    sum += u64::from((v - p).unsigned_abs());
                    n += 1;
                }
                prev = Some(v);
            }
            // Random bytes average ~85; audio stays well below 64.
            n > 0 && sum < n * 64
        }
        _ => {
            // 16/24/32-bit integers: the top 16 bits of the first channel.
            let hi = bytes_per - 2;
            let mut sum = 0u64;
            let mut n = 0u64;
            let mut prev: Option<i32> = None;
            for fr in block.chunks_exact(stride) {
                let v = i32::from(i16::from_le_bytes([fr[hi], fr[hi + 1]]));
                if let Some(p) = prev {
                    sum += u64::from((v - p).unsigned_abs());
                    n += 1;
                }
                prev = Some(v);
            }
            // Random data averages ~21845; speech and music stay far below.
            n > 0 && sum / n < 16_384
        }
    }
}

/// Granularity of the audio-likeness test.
const PIECE: usize = 256;

/// End of the data of an unfinished recording: scan forward while the
/// samples look like audio; trim trailing zeros. The result is accurate to
/// about one 256-byte piece.
fn scan_pcm_end(win: &mut Window<'_>, data_pos: u64, f: &Fmt) -> (u64, bool) {
    let avail = win.avail();
    let limit = MAX_SIZE.min(avail);
    let align = u64::from(f.block_align).max(1);
    let stride = (usize::from(f.block_align).max(1)).max(1);
    let piece = (PIECE / stride).max(8) * stride;
    // Reads and pieces are whole multiples of the sample frame so every
    // piece starts on a frame boundary.
    let scan_block = (SCAN_BLOCK / stride).max(8) * stride;
    let mut pos = data_pos;
    let mut good_end = data_pos;
    let mut stopped_by_noise = false;
    // The most recent passing piece and the good end before it: when noise
    // follows, that piece may be a mix of padding zeros and the start of the
    // noise.
    let mut last_piece: Option<(u64, Vec<u8>, u64)> = None;
    'outer: while pos < limit {
        let want = scan_block.min((limit - pos) as usize);
        let block = win.get(pos, want).to_vec();
        if block.is_empty() {
            break;
        }
        let mut off = 0usize;
        while off < block.len() {
            let end = (off + piece).min(block.len());
            let part = &block[off..end];
            if !audio_like(part, f) {
                stopped_by_noise = true;
                if let Some((start, bytes, before)) = last_piece.take() {
                    if let Some(run) = bytes.windows(16).rposition(|w| w.iter().all(|&b| b == 0)) {
                        let kept = bytes[..run].iter().rposition(|&b| b != 0);
                        good_end = before.max(kept.map_or(before, |k| start + k as u64 + 1));
                    }
                }
                break 'outer;
            }
            let before = good_end;
            if let Some(last) = part.iter().rposition(|&b| b != 0) {
                good_end = pos + off as u64 + last as u64 + 1;
            }
            last_piece = Some((pos + off as u64, part.to_vec(), before));
            off = end;
        }
        pos += block.len() as u64;
        if block.len() < want {
            break;
        }
    }
    let end = good_end.max(data_pos);
    let whole = (end - data_pos).div_ceil(align) * align;
    (data_pos + whole.min(limit - data_pos), stopped_by_noise)
}

struct Outcome {
    len: u64,
    status: ExtentStatus,
    err: Option<u64>,
    conf: Confidence,
    data_len: u64,
    notes: Vec<String>,
}

fn decide(win: &mut Window<'_>, p: &Parsed) -> Option<Outcome> {
    let avail = win.avail();
    let fmt = p.fmt?;
    let data_pos = p.data_pos?;
    let align = u64::from(fmt.block_align).max(1);
    let mut notes = Vec::new();
    let sizes_ok = p.riff_size_set && p.data_len_set;
    if sizes_ok {
        let riff_end = 8 + p.riff_size;
        let riff_end = riff_end + (riff_end & 1);
        let data_end = data_pos + p.data_len;
        if riff_end <= avail && data_end <= riff_end {
            let len = riff_end;
            if p.walked_end + 1 < riff_end && p.walked_end < riff_end {
                notes.push(format!(
                    "{} bytes inside the RIFF chunk after the last parsed chunk",
                    riff_end - p.walked_end
                ));
            }
            return Some(Outcome {
                len,
                status: ExtentStatus::Complete,
                err: None,
                conf: Confidence::HIGH,
                data_len: p.data_len,
                notes,
            });
        }
        if data_end <= riff_end || riff_end > avail {
            // Cut short: data (or the RIFF chunk) extends past the bytes we have.
            let have = avail.saturating_sub(data_pos).min(p.data_len);
            let whole = have / align * align;
            let len = data_pos + whole;
            if win.limited() {
                return Some(Outcome {
                    len,
                    status: ExtentStatus::Capped,
                    err: None,
                    conf: Confidence::MEDIUM,
                    data_len: whole,
                    notes,
                });
            }
            notes.push(format!(
                "the data chunk declares {} bytes but only {} are present",
                p.data_len, have
            ));
            return Some(Outcome {
                len,
                status: ExtentStatus::Truncated,
                err: Some(len),
                conf: Confidence::MEDIUM,
                data_len: whole,
                notes,
            });
        }
    }
    // Sizes are not usable: streaming recorder or damaged header.
    log::debug!(
        "WAV: sizes not finalised (RIFF {}, data {})",
        p.riff_size,
        p.data_len
    );
    notes.push("RIFF/data sizes are not finalised; extent estimated from the samples".into());
    if fmt.is_pcm() {
        let (end, noise) = scan_pcm_end(win, data_pos, &fmt);
        if end <= data_pos {
            return None;
        }
        let capped = !noise && win.limited() && end + SCAN_BLOCK as u64 >= win.avail();
        return Some(Outcome {
            len: end,
            status: if capped {
                ExtentStatus::Capped
            } else {
                ExtentStatus::UnknownLength
            },
            err: None,
            conf: Confidence::LOW,
            data_len: end - data_pos,
            notes,
        });
    }
    notes.push("not PCM: data assumed to run to the end of the available bytes".into());
    let end = avail.min(MAX_SIZE);
    if end <= data_pos {
        return None;
    }
    Some(Outcome {
        len: end,
        status: if win.limited() {
            ExtentStatus::Capped
        } else {
            ExtentStatus::UnknownLength
        },
        err: None,
        conf: Confidence::LOW,
        data_len: end - data_pos,
        notes,
    })
}

fn build_meta(p: &Parsed, data_len: u64) -> MediaMeta {
    let mut m = MediaMeta::default();
    if let Some(f) = &p.fmt {
        m.audio_codec = Some(audio_codec(f.tag, f.bits));
        m.extra
            .insert("sample_rate".into(), f.sample_rate.to_string());
        m.extra.insert("channels".into(), f.channels.to_string());
        m.extra.insert("bits".into(), f.bits.to_string());
        let byte_rate = if f.avg_bytes > 0 {
            u64::from(f.avg_bytes)
        } else {
            u64::from(f.sample_rate) * u64::from(f.block_align)
        };
        if byte_rate > 0 && data_len > 0 {
            m.duration_ms = Some(data_len * 1000 / byte_rate);
        }
    }
    m.extra.insert(EXTENSION_KEY.into(), "wav".into());
    if p.rf64 {
        m.extra.insert("rf64".into(), "true".into());
    }
    m.software = p.software.clone();
    m.created = p.created;
    if let Some(t) = &p.title {
        m.extra.insert("title".into(), t.clone());
    }
    if let Some(t) = &p.originator {
        m.extra.insert("originator".into(), t.clone());
    }
    m
}

/// Format handler for WAV and RF64 audio.
#[derive(Debug, Default, Clone, Copy)]
pub struct WavFormat;

static SIGNATURES: [Signature; 2] = [
    Signature::at_start(b"RIFF").with_also(b"WAVE", 8),
    Signature::at_start(b"RF64").with_also(b"WAVE", 8),
];

impl FileFormat for WavFormat {
    fn id(&self) -> &'static str {
        "wav"
    }

    fn name(&self) -> &'static str {
        "WAV audio"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["wav"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Audio
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        44
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        40
    }

    fn probe(&self, head: &[u8]) -> bool {
        if head.len() < 16 || &head[8..12] != b"WAVE" {
            return false;
        }
        if &head[..4] != b"RIFF" && &head[..4] != b"RF64" {
            return false;
        }
        // The first chunk id must look like a chunk id.
        head[12..16]
            .iter()
            .all(|b| b.is_ascii_graphic() || *b == b' ')
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(p) = parse(&mut win) else {
            return Ok(None);
        };
        let Some(o) = decide(&mut win, &p) else {
            return Ok(None);
        };
        if o.len < 44 {
            return Ok(None);
        }
        let mut m = Measured {
            len: o.len,
            status: o.status,
            confidence: o.conf,
            err_offset: o.err,
            meta: build_meta(&p, o.data_len),
            nested: Vec::new(),
            notes: o.notes,
        };
        if let Some(f) = win.fault() {
            m.notes.push(format!("read error near offset {}", f.offset));
            m.status = ExtentStatus::Truncated;
            m.err_offset = Some(f.offset.min(m.len));
        }
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut win = Window::new(&**src, 0, u64::MAX);
        let size = win.avail();
        let Some(p) = parse(&mut win) else {
            return Ok(Validation::invalid(
                "not a WAV file (no RIFF/RF64 WAVE header)",
            ));
        };
        let mut problems = p.problems.clone();
        let mut data_len = 0;
        match (p.fmt, p.data_pos) {
            (None, _) => {
                if problems.is_empty() {
                    problems.push("no fmt chunk".into());
                }
            }
            (_, None) => problems.push("no data chunk".into()),
            (Some(f), Some(dp)) => {
                let avail_data = size.saturating_sub(dp);
                if !p.data_len_set {
                    problems.push(
                        "the data chunk size is not set (0 or 0xFFFFFFFF): the recording was never finalised"
                            .into(),
                    );
                    data_len = avail_data;
                } else {
                    if p.data_len > avail_data {
                        problems.push(format!(
                            "the data chunk declares {} bytes but only {avail_data} are present (truncated)",
                            p.data_len
                        ));
                        data_len = avail_data;
                    } else {
                        data_len = p.data_len;
                    }
                    if data_len % u64::from(f.block_align).max(1) != 0 {
                        problems
                            .push("the data length is not a whole number of sample frames".into());
                    }
                }
                if !p.riff_size_set {
                    problems.push("the RIFF size is not set".into());
                } else {
                    let expect = 8 + p.riff_size;
                    if expect != size && expect + 1 != size && expect != size + 1 {
                        problems.push(format!(
                            "the RIFF size says the file is {expect} bytes long, it is {size}"
                        ));
                    }
                }
            }
        }
        let intact = if problems.is_empty() {
            1.0
        } else {
            let dp = p.data_pos.unwrap_or(0);
            ((dp + data_len) as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
        };
        Ok(Validation {
            status: if problems.is_empty() {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Damaged
            },
            intact_fraction: Some(intact),
            problems,
            meta: build_meta(&p, data_len),
            repairable_by: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn wav(pcm: &[u8], riff: Option<u32>, data: Option<u32>) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&riff.unwrap_or(36 + pcm.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.unwrap_or(pcm.len() as u32).to_le_bytes());
        v.extend_from_slice(pcm);
        v
    }

    fn sine(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| {
                let s = ((i as f64 * 0.05).sin() * 12000.0) as i16;
                let b = s.to_le_bytes();
                [b[0], b[1], b[0], b[1]]
            })
            .collect()
    }

    fn mem(d: Vec<u8>) -> SharedSource {
        Arc::new(diskrescue_core::source::MemSource::new(d))
    }

    #[test]
    fn finalised_wav_is_exact() {
        let pcm = sine(10_000);
        let f = wav(&pcm, None, None);
        let mut d = f.clone();
        d.extend_from_slice(&[0x77; 500]);
        let m = WavFormat.measure(&mem(d), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, f.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert_eq!(m.meta.audio_codec.as_deref(), Some("pcm_s16le"));
        assert_eq!(m.meta.duration_ms, Some(10_000 * 1000 / 44_100));
        let v = WavFormat.validate(&mem(f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    }

    #[test]
    fn unfinished_recording_scans_for_the_end() {
        let pcm = sine(20_000);
        let mut d = wav(&pcm, Some(0xFFFF_FFFF), Some(0xFFFF_FFFF));
        let want = d.len() as u64;
        d.extend_from_slice(&[0u8; 8192]); // zero padding
        d.extend_from_slice(&crate::io::XorShift(5).bytes(20_000)); // another file
        let m = WavFormat
            .measure(&mem(d.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::UnknownLength);
        assert_eq!(m.confidence, Confidence::LOW);
        // Ends at the last non-zero audio byte (within one block of the end).
        assert!(m.len <= want && m.len + 8 >= want, "{} vs {want}", m.len);
        let v = WavFormat
            .validate(&mem(d[..want as usize].to_vec()))
            .unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
        assert!(v.problems[0].contains("never finalised"));
    }

    #[test]
    fn cut_wav_is_truncated_on_a_sample_frame() {
        let pcm = sine(10_000);
        let f = wav(&pcm, None, None);
        let cut = &f[..f.len() - 1001];
        let m = WavFormat
            .measure(&mem(cut.to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!((m.len - 44) % 4, 0);
        assert!(m.len <= cut.len() as u64 && m.len + 4 > cut.len() as u64);
    }

    #[test]
    fn rejects_non_wav() {
        let mut d = wav(&sine(100), None, None);
        d[8..12].copy_from_slice(b"AVI ");
        assert!(WavFormat.measure(&mem(d), 0, u64::MAX).unwrap().is_none());
        assert!(WavFormat
            .measure(&mem(vec![0; 100]), 0, u64::MAX)
            .unwrap()
            .is_none());
    }
}
