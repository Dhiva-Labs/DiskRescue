//! Adobe Photoshop PSD / PSB.
//!
//! # Layout
//!
//! A 26-byte header (`8BPS`, version 1 or 2, six zero bytes, channels,
//! height, width, depth, colour mode, all big-endian) is followed by four
//! sections: colour mode data and image resources (each a 32-bit length and
//! that many bytes), layer and mask information (a 32-bit length, 64-bit in
//! PSB) and the merged image data (a 16-bit compression method and the
//! pixels).
//!
//! # Extent
//!
//! The first three sections are skipped by their length fields without
//! reading them. The image data length follows from the compression method:
//!
//! * 0, raw: `channels * height * ceil(width * depth / 8)`;
//! * 1, RLE: the table of `height * channels` row sizes (16-bit in PSD,
//!   32-bit in PSB) plus the sum of those sizes. Each size must be
//!   plausible for a PackBits row, which catches garbage;
//! * 2 and 3, ZIP: the zlib stream is inflated into a discarding buffer to
//!   find its end, and its output must equal the raw size.
//!
//! # Metadata
//!
//! The image resources are walked (bounded) for the Photoshop JPEG thumbnail
//! (0x040C, reported as a nested thumbnail) and the Exif block (0x0422),
//! which gives make, model, capture date and orientation.
//!
//! # Limits
//!
//! PSD is capped at 2 GiB and PSB at 4 GiB. Layer data is skipped, not
//! validated.

use diskrescue_core::format::{
    Confidence, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{u16be, u32be, u64be};
use diskrescue_core::{Range, Result};
use flate2::{Decompress, FlushDecompress, Status};

use crate::common::{failed, sized, validate_with, Inspected};
use crate::tiff::{exif_meta, walk_blob};
use crate::win::{read_error, Fail, Win};

const PSD_CAP: u64 = 2 << 30;
const PSB_CAP: u64 = 4 << 30;
const MAX_RESOURCES: u32 = 8192;
const SIGNATURES: [Signature; 2] = [
    Signature::at_start(b"8BPS\x00\x01"),
    Signature::at_start(b"8BPS\x00\x02"),
];

/// Photoshop documents (PSD and PSB).
#[derive(Debug, Clone, Copy, Default)]
pub struct PsdFormat;

#[derive(Debug, Clone, Copy)]
struct Header {
    psb: bool,
    channels: u64,
    height: u64,
    width: u64,
    depth: u64,
    mode: u16,
}

impl Header {
    fn cap(&self) -> u64 {
        if self.psb {
            PSB_CAP
        } else {
            PSD_CAP
        }
    }

    fn row_bytes(&self) -> u64 {
        (self.width * self.depth).div_ceil(8)
    }

    /// Size of the uncompressed image data.
    fn raw_size(&self) -> u64 {
        self.channels
            .saturating_mul(self.height)
            .saturating_mul(self.row_bytes())
    }

    fn mode_name(&self) -> &'static str {
        match self.mode {
            0 => "bitmap",
            1 => "grayscale",
            2 => "indexed",
            3 => "rgb",
            4 => "cmyk",
            7 => "multichannel",
            8 => "duotone",
            9 => "lab",
            _ => "unknown",
        }
    }
}

fn parse_header(h: &[u8]) -> Option<Header> {
    if h.len() < 26 || &h[..4] != b"8BPS" {
        return None;
    }
    let version = u16be(h, 4)?;
    if !(1..=2).contains(&version) || h[6..12] != [0; 6] {
        return None;
    }
    let psb = version == 2;
    let channels = u16be(h, 12)? as u64;
    let height = u32be(h, 14)? as u64;
    let width = u32be(h, 18)? as u64;
    let depth = u16be(h, 22)? as u64;
    let mode = u16be(h, 24)?;
    let max_dim = if psb { 300_000 } else { 30_000 };
    if !(1..=56).contains(&channels)
        || !(1..=max_dim).contains(&height)
        || !(1..=max_dim).contains(&width)
        || !matches!(depth, 1 | 8 | 16 | 32)
        || mode > 9
    {
        return None;
    }
    Some(Header {
        psb,
        channels,
        height,
        width,
        depth,
        mode,
    })
}

/// A decoded outcome of the section walk.
struct Layout {
    header: Header,
    resources: (u64, u64),
    image_start: u64,
    anomalies: Vec<String>,
}

enum Stop {
    /// The window or medium ended while reading a length field.
    Fail(Fail),
    /// The section length stored at this offset reaches beyond the format
    /// cap, so the layout is implausible.
    TooBig(u64),
}

fn layout(win: &Win, header: Header) -> std::result::Result<Layout, Stop> {
    let mut pos = 26u64;
    let mut anomalies = Vec::new();
    let cap = header.cap();
    let read32 = |at: u64| -> std::result::Result<u64, Stop> {
        let b = win.read(at, 4).map_err(Stop::Fail)?;
        Ok(u32be(&b, 0).unwrap_or(0) as u64)
    };
    // Colour mode data.
    let cm_at = pos;
    let cm = read32(pos)?;
    if cm != 0 && header.mode != 2 && header.mode != 8 {
        anomalies.push(format!(
            "colour mode data of {cm} bytes for colour mode {}",
            header.mode_name()
        ));
    }
    pos = pos.saturating_add(4).saturating_add(cm);
    if pos >= cap {
        return Err(Stop::TooBig(cm_at));
    }
    // Image resources.
    let res_at = pos;
    let res_len = read32(pos)?;
    let res_start = pos + 4;
    pos = res_start.saturating_add(res_len);
    if pos >= cap {
        return Err(Stop::TooBig(res_at));
    }
    // Layer and mask info.
    let lm_at = pos;
    let lm = if header.psb {
        let b = win.read(pos, 8).map_err(Stop::Fail)?;
        let v = u64be(&b, 0).unwrap_or(0);
        pos += 8;
        v
    } else {
        let v = read32(pos)?;
        pos += 4;
        v
    };
    pos = pos.saturating_add(lm);
    if pos >= cap {
        return Err(Stop::TooBig(lm_at));
    }
    Ok(Layout {
        header,
        resources: (res_start, res_len),
        image_start: pos,
        anomalies,
    })
}

/// How the image data section ended.
enum Data {
    /// `len` bytes of image data, header included.
    Len(u64),
    /// Walk stopped at this offset because of damage.
    Bad(u64, String),
    /// Reading failed.
    Read(Fail),
}

/// Sum a table of RLE row sizes in streaming chunks.
fn rle_length(win: &Win, l: &Layout, at: u64) -> Data {
    let h = &l.header;
    let rows = h.channels * h.height;
    let esz: u64 = if h.psb { 4 } else { 2 };
    let table = rows * esz;
    let max_row = h.row_bytes() + h.row_bytes() / 128 + 2;
    let mut sum: u64 = 0;
    let mut done = 0u64;
    const CHUNK: u64 = 64 * 1024;
    while done < table {
        let want = (table - done).min(CHUNK) as usize;
        let buf = match win.read(at + done, want) {
            Ok(b) => b,
            Err(f) => return Data::Read(f),
        };
        for (i, c) in buf.chunks_exact(esz as usize).enumerate() {
            let v = if h.psb {
                u32::from_be_bytes([c[0], c[1], c[2], c[3]]) as u64
            } else {
                u16::from_be_bytes([c[0], c[1]]) as u64
            };
            if v > max_row {
                let row_at = at + done + i as u64 * esz;
                return Data::Bad(
                    row_at,
                    format!("implausible RLE row size {v} (at most {max_row})"),
                );
            }
            sum += v;
        }
        done += want as u64;
    }
    Data::Len(2 + table + sum)
}

/// Inflate the zlib stream at `at` to find its end.
fn zip_length(win: &Win, l: &Layout, at: u64) -> Data {
    let expected = l.header.raw_size();
    let mut d = Decompress::new(true);
    let mut out = vec![0u8; 64 * 1024];
    let mut consumed = 0u64;
    const CHUNK: usize = 64 * 1024;
    loop {
        let input = match win.read_upto(at + consumed, CHUNK) {
            Ok(b) => b,
            Err(f) => return Data::Read(f),
        };
        if input.is_empty() {
            return Data::Read(Fail::Eof(win.avail()));
        }
        let mut used = 0usize;
        loop {
            let before_in = d.total_in();
            let before_out = d.total_out();
            let r = d.decompress(&input[used..], &mut out, FlushDecompress::None);
            let took = (d.total_in() - before_in) as usize;
            used += took;
            if took == 0 && d.total_out() == before_out && !matches!(r, Ok(Status::StreamEnd)) {
                return Data::Bad(at + d.total_in(), "ZIP stream made no progress".into());
            }
            match r {
                Ok(Status::StreamEnd) => {
                    let total_out = d.total_out();
                    let end = at + d.total_in();
                    if total_out != expected {
                        return Data::Bad(
                            end,
                            format!(
                                "ZIP image data inflates to {total_out} bytes, expected {expected}"
                            ),
                        );
                    }
                    return Data::Len(2 + d.total_in());
                }
                Ok(_) => {
                    if used >= input.len() {
                        break;
                    }
                }
                Err(e) => {
                    return Data::Bad(at + d.total_in(), format!("invalid ZIP stream: {e}"));
                }
            }
            if d.total_out() > expected {
                return Data::Bad(
                    at + d.total_in(),
                    "ZIP image data inflates beyond the image size".into(),
                );
            }
        }
        consumed += input.len() as u64;
        if consumed > l.header.cap() {
            return Data::Bad(
                at + consumed,
                "ZIP stream longer than the format allows".into(),
            );
        }
    }
}

/// A `(offset, length)` region of the file.
type Region = (u64, u64);

/// Walk the image resource blocks for the thumbnail and Exif.
fn resources(win: &Win, start: u64, len: u64) -> (Option<Region>, Option<Region>) {
    let end = start.saturating_add(len).min(win.avail());
    let mut pos = start;
    let mut thumb = None;
    let mut exif = None;
    for _ in 0..MAX_RESOURCES {
        if pos.saturating_add(12) > end {
            break;
        }
        let Ok(h) = win.read(pos, 7) else { break };
        if &h[..4] != b"8BIM" && &h[..4] != b"MeSa" {
            break;
        }
        let id = u16be(&h, 4).unwrap_or(0);
        let name_len = h[6] as u64;
        // Pascal string padded so that length byte + text is even.
        let name_total = (1 + name_len).div_ceil(2) * 2;
        let size_at = pos + 6 + name_total;
        let Ok(sz) = win.read(size_at, 4) else { break };
        let size = u32be(&sz, 0).unwrap_or(0) as u64;
        let data = size_at + 4;
        if data.saturating_add(size) > end {
            break;
        }
        match id {
            0x040C if thumb.is_none() && size > 28 => {
                // 28-byte thumbnail header (compressed size at +20), then JPEG.
                if let Ok(t) = win.read(data, 28) {
                    let csize = u32be(&t, 20).unwrap_or(0) as u64;
                    if csize > 0 && csize <= size - 28 {
                        thumb = Some((data + 28, csize));
                    }
                }
            }
            0x0422 if exif.is_none() && size >= 16 => exif = Some((data, size)),
            _ => {}
        }
        pos = data + size + (size & 1);
    }
    (thumb, exif)
}

fn inspect(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Inspected>> {
    let limit = limit.min(PSB_CAP);
    let win = Win::new(src, start, limit, 64 * 1024);
    let head = match win.read_upto(0, 26) {
        Ok(h) => h,
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let Some(header) = parse_header(&head) else {
        return Ok(None);
    };
    let conf = Confidence::MEDIUM;
    let l = match layout(&win, header) {
        Ok(l) => l,
        Err(Stop::Fail(Fail::Eof(_))) if win.avail() < 34 => return Ok(None),
        Err(Stop::Fail(f)) => {
            let m = failed(&win, f, conf, "the PSD section lengths");
            return Ok(Some(Inspected::new(with_basic_meta(m, &header), None)));
        }
        Err(Stop::TooBig(at)) => {
            let m = Measured::truncated(
                at,
                conf.lowered(30),
                at,
                format!(
                    "the section length at offset {at} reaches beyond the {} byte limit",
                    header.cap()
                ),
            );
            return Ok(Some(Inspected::new(with_basic_meta(m, &header), None)));
        }
    };
    // Image data.
    let at = l.image_start;
    let comp = match win.read(at, 2) {
        Ok(b) => u16be(&b, 0).unwrap_or(99),
        Err(f) => {
            let m = failed(&win, f, conf, "the PSD image data");
            return Ok(Some(Inspected::new(with_basic_meta(m, &header), None)));
        }
    };
    let data = match comp {
        0 => {
            let raw = header.raw_size();
            if at.saturating_add(2).saturating_add(raw) > header.cap() {
                Data::Bad(at, "raw image data larger than the format cap".into())
            } else {
                Data::Len(2 + raw)
            }
        }
        1 => rle_length(&win, &l, at + 2),
        2 | 3 => zip_length(&win, &l, at + 2),
        c => Data::Bad(at, format!("unknown compression method {c}")),
    };
    let mut notes = l.anomalies.clone();
    let mut conf = conf;
    let mut expected = None;
    let mut m = match data {
        Data::Len(n) => {
            let total = at + n;
            expected = Some(total);
            if comp == 2 || comp == 3 {
                conf = Confidence::new(75);
            }
            sized(&win, total, conf, "the PSD image data")
        }
        Data::Bad(off, why) => Measured::truncated(off, conf.lowered(30), off, why),
        Data::Read(f) => failed(&win, f, conf, "the PSD image data"),
    };
    let bump = notes.len().min(3) as u8 * 5;
    m.confidence = m.confidence.lowered(bump);
    let mut meta = basic_meta(&header);
    meta.extra
        .insert("compression".into(), compression_name(comp).into());
    // Resources: thumbnail and Exif.
    let (thumb, exif) = resources(&win, l.resources.0, l.resources.1);
    if let Some((off, len)) = thumb {
        if off < m.len {
            m.nested.push(Nested {
                range: Range::new(off, (off + len).min(m.len)),
                format: Some("jpeg".into()),
                role: NestedRole::Thumbnail,
            });
        }
    }
    if let Some((off, len)) = exif {
        let skip = match win.read_upto(off, 6) {
            Ok(b) if b.as_slice() == b"Exif\0\0" => 6,
            _ => 0,
        };
        if let Some(w) = walk_blob(src, start + off + skip, len - skip) {
            let em = exif_meta(&w);
            meta.created = em.created;
            meta.make = em.make;
            meta.model = em.model;
            meta.orientation = em.orientation;
            meta.software = em.software;
        }
    }
    m.meta = meta;
    m.notes.append(&mut notes);
    crate::common::log_measured(&win, "psd", &m, false);
    Ok(Some(Inspected::new(m, expected)))
}

fn compression_name(c: u16) -> &'static str {
    match c {
        0 => "raw",
        1 => "rle",
        2 => "zip",
        3 => "zip-prediction",
        _ => "unknown",
    }
}

fn basic_meta(h: &Header) -> MediaMeta {
    let mut meta = MediaMeta {
        width: Some(h.width as u32),
        height: Some(h.height as u32),
        ..MediaMeta::default()
    };
    meta.extra.insert("channels".into(), h.channels.to_string());
    meta.extra.insert("depth".into(), h.depth.to_string());
    meta.extra.insert("color_mode".into(), h.mode_name().into());
    if h.psb {
        meta.extra.insert("psb".into(), "true".into());
    }
    meta
}

fn with_basic_meta(mut m: Measured, h: &Header) -> Measured {
    m.meta = basic_meta(h);
    m
}

impl FileFormat for PsdFormat {
    fn id(&self) -> &'static str {
        "psd"
    }

    fn name(&self) -> &'static str {
        "Photoshop document"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["psd", "psb"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        40
    }

    fn max_size(&self) -> u64 {
        PSB_CAP
    }

    fn priority(&self) -> u8 {
        55
    }

    fn probe(&self, head: &[u8]) -> bool {
        if head.len() < 26 {
            return head.len() >= 6 && SIGNATURES.iter().any(|s| s.matches(head));
        }
        parse_header(head).is_some()
    }

    fn may_contain(&self, inner: &str) -> bool {
        inner == "jpeg"
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        Ok(inspect(src, start, limit)?.map(|i| i.measured))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        validate_with("PSD", src, inspect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::ExtentStatus;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    fn header(psb: bool, ch: u16, h: u32, w: u32, depth: u16, mode: u16) -> Vec<u8> {
        let mut b = b"8BPS".to_vec();
        b.extend_from_slice(&(if psb { 2u16 } else { 1 }).to_be_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&ch.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&depth.to_be_bytes());
        b.extend_from_slice(&mode.to_be_bytes());
        b
    }

    fn raw_psd(ch: u16, h: u32, w: u32, depth: u16) -> Vec<u8> {
        let mut b = header(false, ch, h, w, depth, 3);
        b.extend_from_slice(&0u32.to_be_bytes()); // colour mode
        b.extend_from_slice(&0u32.to_be_bytes()); // resources
        b.extend_from_slice(&0u32.to_be_bytes()); // layers
        b.extend_from_slice(&0u16.to_be_bytes()); // raw
        let rb = (w as usize * depth as usize).div_ceil(8);
        b.resize(b.len() + ch as usize * h as usize * rb, 0x77);
        b
    }

    fn mem(b: Vec<u8>) -> SharedSource {
        Arc::new(MemSource::new(b))
    }

    #[test]
    fn raw_image_data_length_is_computed() {
        for (ch, h, w, d) in [(3u16, 5u32, 7u32, 8u16), (1, 9, 9, 1), (4, 3, 3, 16)] {
            let p = raw_psd(ch, h, w, d);
            let mut padded = p.clone();
            padded.extend_from_slice(&[9; 77]);
            let m = PsdFormat
                .measure(&mem(padded), 0, u64::MAX)
                .unwrap()
                .unwrap();
            assert_eq!(m.len, p.len() as u64, "{ch} {h} {w} {d}");
            assert_eq!(m.status, ExtentStatus::Complete);
            assert_eq!(m.meta.width, Some(w));
        }
    }

    #[test]
    fn rle_length_is_the_table_plus_rows() {
        let (ch, h, w) = (2u16, 3u32, 8u32);
        let mut b = header(false, ch, h, w, 8, 1);
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(&1u16.to_be_bytes());
        let sizes = [3u16, 4, 5, 2, 9, 1];
        for s in sizes {
            b.extend_from_slice(&s.to_be_bytes());
        }
        for s in sizes {
            b.extend(std::iter::repeat_n(0x11u8, s as usize));
        }
        let m = PsdFormat
            .measure(&mem(b.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, b.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        // An implausible row size is flagged.
        let tbl = b.len() - sizes.iter().map(|s| *s as usize).sum::<usize>() - 12;
        b[tbl..tbl + 2].copy_from_slice(&5000u16.to_be_bytes());
        let m = PsdFormat.measure(&mem(b), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(tbl as u64));
    }

    #[test]
    fn zip_data_is_inflated_to_find_the_end() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let (ch, h, w) = (1u16, 16u32, 16u32);
        let raw = vec![0x55u8; 256];
        let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&raw).unwrap();
        let z = e.finish().unwrap();
        let mut b = header(false, ch, h, w, 8, 1);
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(&z);
        let mut padded = b.clone();
        padded.extend_from_slice(&[0xEE; 300]);
        let m = PsdFormat
            .measure(&mem(padded), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, b.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        // Wrong image size makes the inflated size disagree.
        let mut wrong = b.clone();
        wrong[18..22].copy_from_slice(&40u32.to_be_bytes());
        let m = PsdFormat
            .measure(&mem(wrong), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
    }

    #[test]
    fn truncation_rejections_and_garbage() {
        let p = raw_psd(3, 8, 8, 8);
        let cut = p.len() - 50;
        let m = PsdFormat
            .measure(&mem(p[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(cut as u64));
        let mut bad = p.clone();
        bad[7] = 9; // reserved bytes must be zero... version byte actually
        bad[4] = 0;
        bad[5] = 7;
        assert!(PsdFormat.measure(&mem(bad), 0, u64::MAX).unwrap().is_none());
        let mut reserved = p.clone();
        reserved[8] = 1;
        assert!(PsdFormat
            .measure(&mem(reserved.clone()), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(!PsdFormat.probe(&reserved));
        let mut x = 0xABCD_1234u32;
        for i in 0..3000 {
            let mut b = p.clone();
            for _ in 0..(1 + i % 4) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let pos = (x as usize) % 60;
                b[pos] = (x >> 7) as u8;
            }
            if i % 5 == 0 {
                b.truncate((x as usize >> 4) % b.len());
            }
            let s = mem(b.clone());
            let _ = PsdFormat.probe(&b);
            let _ = PsdFormat.measure(&s, 0, u64::MAX);
            let _ = PsdFormat.validate(&s);
        }
    }

    #[test]
    fn psb_uses_64_bit_layer_length() {
        let mut b = header(true, 1, 4, 4, 8, 1);
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.resize(b.len() + 16, 1);
        let m = PsdFormat
            .measure(&mem(b.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, b.len() as u64);
        assert_eq!(m.meta.extra["psb"], "true");
    }
}
