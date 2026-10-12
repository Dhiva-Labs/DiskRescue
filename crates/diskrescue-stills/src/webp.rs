//! WebP (RIFF container).
//!
//! # Algorithm
//!
//! A WebP file is `RIFF`, a 32-bit little-endian size (the file length minus
//! 8), `WEBP` and a sequence of chunks, each a FourCC, a 32-bit size, the
//! payload and one pad byte when the size is odd. The first chunk must be
//! `VP8 ` (lossy), `VP8L` (lossless) or `VP8X` (extended); its bitstream
//! header is parsed for the image dimensions (start code `9D 01 2A` for
//! VP8, signature byte `2F` and a zero version for VP8L). The candidate is
//! rejected if that fails.
//!
//! The length is `8 + riff_size`. The top-level chunks are then walked and
//! must add up to exactly that end (a difference of one byte is tolerated):
//!
//! * chunks end exactly at the RIFF end: `Complete`, high confidence;
//! * the walk stops early at a chunk with a non-printable FourCC: `Truncated`
//!   at the walk position;
//! * a chunk runs past the RIFF end: `Truncated` at that chunk, length is the
//!   RIFF end.
//!
//! Every top-level `VP8 ` / `VP8L` chunk must carry a valid bitstream header
//! (a shown key frame whose first partition fits in the chunk, as libwebp
//! requires; or the VP8L signature with version 0); a chunk that does not
//! ends the walk there.
//!
//! The RIFF size alone does not prove that the bytes it covers are this
//! file's: a WebP cut short and followed by other files still declares its
//! old size. The payload after the last chunk header the walk verified is
//! therefore scanned for the start of another file at sector-aligned offsets
//! (see `crate::foreign`; a TIFF or JPEG header inside the `EXIF` chunk is
//! the file's own Exif block). Such a start ends the file: `Truncated` at
//! that offset with lower confidence.
//!
//! Metadata: dimensions from `VP8X` (canvas) or the bitstream; the `EXIF`
//! chunk is parsed with the TIFF walker for the capture date, make, model and
//! orientation.
//!
//! # Limits
//!
//! Chunk payloads (the VP8 bitstream, `ANMF` frames) are not decoded or
//! checksummed, so a file cut short and followed by zeros or by data that
//! is not another file's header still measures to its declared size.

use diskrescue_core::format::{Confidence, FileFormat, Measured, Signature, Validation};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{u24le, u32le};
use diskrescue_core::Result;

use crate::common::{cut_at, failed, foreign_note, validate_with, Inspected};
use crate::foreign::{self, Sig};
use crate::tiff::{exif_meta, walk_blob};
use crate::win::{read_error, Cursor, Fail, Win};

const MAX_SIZE: u64 = 4 << 30;
const MAX_CHUNKS: u32 = 1_000_000;
const MAX_EXIF: u64 = 1 << 20;
const SIGNATURES: [Signature; 1] = [Signature::at_start(b"RIFF").with_also(b"WEBP", 8)];

/// WebP images.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebpFormat;

/// Dimensions from a `VP8 ` chunk payload: a key frame (bit 0 clear) of
/// profile 0..3 with the `9D 01 2A` start code.
fn vp8_dims(p: &[u8]) -> Option<(u32, u32)> {
    if p.len() < 10 || p[0] & 1 != 0 || (p[0] >> 1) & 7 > 3 || p[3..6] != [0x9D, 0x01, 0x2A] {
        return None;
    }
    let w = (u16::from_le_bytes([p[6], p[7]]) & 0x3FFF) as u32;
    let h = (u16::from_le_bytes([p[8], p[9]]) & 0x3FFF) as u32;
    (w > 0 && h > 0).then_some((w, h))
}

/// The rest of libwebp's `VP8GetInfo` checks on a `VP8 ` frame tag: the
/// frame is shown and its first partition fits in the `chunk_size`-byte
/// chunk.
fn vp8_consistent(p: &[u8], chunk_size: u64) -> bool {
    let Some(t) = p.get(..3) else {
        return false;
    };
    let bits = u32::from(t[0]) | u32::from(t[1]) << 8 | u32::from(t[2]) << 16;
    bits & 0x10 != 0 && u64::from(bits >> 5) < chunk_size
}

/// Dimensions and alpha flag from a `VP8L` chunk payload.
fn vp8l_dims(p: &[u8]) -> Option<(u32, u32, bool)> {
    if p.len() < 5 || p[0] != 0x2F {
        return None;
    }
    let bits = u32::from_le_bytes([p[1], p[2], p[3], p[4]]);
    if bits >> 29 != 0 {
        return None;
    }
    Some((
        (bits & 0x3FFF) + 1,
        ((bits >> 14) & 0x3FFF) + 1,
        bits >> 28 & 1 == 1,
    ))
}

/// Canvas size and flags from a `VP8X` chunk payload.
fn vp8x_info(p: &[u8]) -> Option<(u32, u32, u8)> {
    if p.len() < 10 {
        return None;
    }
    Some((u24le(p, 4)? + 1, u24le(p, 7)? + 1, p[0]))
}

fn printable(id: &[u8]) -> bool {
    id.iter().all(|b| (0x20..=0x7E).contains(b))
}

/// Check the first 20 bytes; shared by `probe` and `measure`.
fn header_ok(h: &[u8]) -> bool {
    if h.len() < 20 || !SIGNATURES[0].matches(h) {
        return false;
    }
    let riff = u32le(h, 4).unwrap_or(0);
    riff >= 12 && matches!(&h[12..16], b"VP8 " | b"VP8L" | b"VP8X")
}

enum End {
    Exact,
    Short(u64),
    Over(u64),
    Stopped(Fail),
}

struct Scan {
    riff_end: u64,
    end: End,
    kind: &'static str,
    canvas: Option<(u32, u32)>,
    image: Option<(u32, u32)>,
    alpha: bool,
    animated: bool,
    frames: u32,
    exif: Option<(u64, u64)>,
    flags_reserved: bool,
    /// A `VP8 ` frame header that libwebp would refuse.
    vp8_inconsistent: bool,
    /// Payload start of the last chunk whose header the walk accepted: the
    /// bytes before it are vouched for by chunk headers.
    last_payload: u64,
}

fn scan(win: &Win) -> std::result::Result<Option<Scan>, Fail> {
    let mut cur = Cursor::new(win, 16 * 1024);
    let h = match cur.slice(0, 20) {
        Ok(h) => h.to_vec(),
        Err(Fail::Eof(_)) => return Ok(None),
        Err(f) => return Err(f),
    };
    if !header_ok(&h) {
        return Ok(None);
    }
    let riff_end = 8 + u32le(&h, 4).unwrap_or(0) as u64;
    let mut s = Scan {
        riff_end,
        end: End::Exact,
        kind: "lossy",
        canvas: None,
        image: None,
        alpha: false,
        animated: false,
        frames: 0,
        exif: None,
        flags_reserved: false,
        vp8_inconsistent: false,
        last_payload: 20,
    };
    // The first chunk's bitstream header must parse.
    let first_size = u32le(&h, 16).unwrap_or(0) as usize;
    let probe_len = first_size.clamp(0, 32);
    let payload = match cur.slice(20, probe_len) {
        Ok(p) => p.to_vec(),
        Err(Fail::Eof(_)) => Vec::new(),
        Err(f) => return Err(f),
    };
    let complete_payload = payload.len() == probe_len;
    match &h[12..16] {
        b"VP8 " => {
            s.kind = "lossy";
            if complete_payload {
                match vp8_dims(&payload) {
                    Some(d) => {
                        s.image = Some(d);
                        s.vp8_inconsistent |= !vp8_consistent(&payload, first_size as u64);
                    }
                    None => return Ok(None),
                }
            }
        }
        b"VP8L" => {
            s.kind = "lossless";
            if complete_payload {
                match vp8l_dims(&payload) {
                    Some((w, hh, a)) => {
                        s.image = Some((w, hh));
                        s.alpha = a;
                    }
                    None => return Ok(None),
                }
            }
        }
        _ => {
            s.kind = "extended";
            if complete_payload {
                match vp8x_info(&payload) {
                    Some((w, hh, flags)) => {
                        s.canvas = Some((w, hh));
                        s.alpha = flags & 0x10 != 0;
                        s.animated = flags & 0x02 != 0;
                        s.flags_reserved = flags & 0xC1 != 0;
                    }
                    None => return Ok(None),
                }
            }
        }
    }
    let mut pos = 12u64;
    let mut chunks = 0u32;
    s.end = loop {
        if pos.saturating_add(8) > riff_end {
            break if riff_end.saturating_sub(pos) <= 1 {
                End::Exact
            } else {
                End::Short(pos)
            };
        }
        chunks += 1;
        if chunks > MAX_CHUNKS {
            break End::Short(pos);
        }
        let hdr = match cur.slice(pos, 8) {
            Ok(b) => [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]],
            Err(f) => break End::Stopped(f),
        };
        let id = &hdr[..4];
        if !printable(id) {
            break End::Short(pos);
        }
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        let cend = pos + 8 + size + (size & 1);
        if cend > riff_end {
            // A writer that left the pad byte out of the last odd-sized chunk.
            if size & 1 == 1 && cend == riff_end + 1 {
                break End::Exact;
            }
            break End::Over(pos);
        }
        match id {
            b"VP8 " | b"VP8L" if pos > 12 => {
                // Every image bitstream must start like one.
                let n = size.min(32) as usize;
                let header = match cur.slice(pos + 8, n) {
                    Ok(p) => {
                        if id == b"VP8 " {
                            s.vp8_inconsistent |= !vp8_consistent(p, size);
                            vp8_dims(p).map(|d| (d, false))
                        } else {
                            vp8l_dims(p).map(|(w, hh, a)| ((w, hh), a))
                        }
                    }
                    Err(f) => break End::Stopped(f),
                };
                let Some((dims, a)) = header else {
                    break End::Short(pos);
                };
                if s.image.is_none() {
                    s.image = Some(dims);
                }
                s.alpha |= a;
            }
            b"ANIM" => s.animated = true,
            b"ANMF" => s.frames += 1,
            b"ALPH" => s.alpha = true,
            b"EXIF" if s.exif.is_none() => s.exif = Some((pos + 8, size)),
            _ => {}
        }
        s.last_payload = pos + 8;
        pos = cend;
    };
    Ok(Some(s))
}

fn inspect(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Inspected>> {
    let limit = limit.min(MAX_SIZE);
    let win = Win::new(src, start, limit, 64 * 1024);
    let s = match scan(&win) {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(None),
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let conf = Confidence::HIGH;
    let mut notes: Vec<String> = Vec::new();
    let mut m = match s.end {
        End::Exact => crate::common::sized(&win, s.riff_end, conf, "the RIFF container"),
        // A walk that ended beyond the readable bytes is a short source or a
        // limit, not a bad chunk.
        End::Short(at) | End::Over(at) if at >= win.avail() => {
            failed(&win, win.short_fail(win.avail()), conf, "a RIFF chunk")
        }
        End::Short(at) => {
            notes.push(format!(
                "chunk walk stops at {at} but the RIFF size declares {} bytes",
                s.riff_end
            ));
            Measured::truncated(
                at,
                Confidence::MEDIUM.lowered(30),
                at,
                format!("invalid RIFF chunk at offset {at}"),
            )
        }
        End::Over(at) => Measured::truncated(
            s.riff_end.min(win.avail()),
            Confidence::MEDIUM.lowered(30),
            at,
            format!("chunk at offset {at} runs past the RIFF size"),
        ),
        End::Stopped(f) => failed(&win, f, conf, "a RIFF chunk"),
    };
    if s.flags_reserved {
        m.confidence = m.confidence.lowered(10);
        notes.push("reserved VP8X flag bits are set".into());
    }
    if s.vp8_inconsistent {
        m.confidence = m.confidence.lowered(20);
        notes.push(
            "a VP8 frame header is not decodable (hidden frame, or a first partition larger than its chunk)"
                .into(),
        );
    }
    // Another file starting inside the bytes the chunk headers do not vouch
    // for ends this one. Exact walks are vouched for up to the last chunk
    // header; otherwise only the first one is trusted.
    let from = match s.end {
        End::Exact => s.last_payload,
        _ => 20,
    };
    let exif = s.exif.map(|(off, size)| (off, off.saturating_add(size)));
    let allowed = |off: u64, sig: Sig| {
        matches!(sig, Sig::Tiff | Sig::Jpeg) && exif.is_some_and(|(a, b)| (a..b).contains(&off))
    };
    if let Some(hit) = foreign::scan(&win, from, m.len, &allowed) {
        let declared = s.riff_end;
        cut_at(
            &mut m,
            hit.offset,
            foreign_note(hit, declared, "the RIFF header"),
        );
    }
    let mut meta = MediaMeta::default();
    match (s.canvas, s.image) {
        (Some((cw, ch)), img) => {
            meta.width = Some(cw);
            meta.height = Some(ch);
            if let Some((iw, ih)) = img {
                if (iw, ih) != (cw, ch) {
                    m.confidence = m.confidence.lowered(10);
                    notes.push(format!("canvas {cw}x{ch} differs from the image {iw}x{ih}"));
                }
            }
        }
        (None, Some((iw, ih))) => {
            meta.width = Some(iw);
            meta.height = Some(ih);
        }
        (None, None) => {}
    }
    meta.extra.insert("webp_type".into(), s.kind.into());
    if s.alpha {
        meta.extra.insert("alpha".into(), "true".into());
    }
    if s.animated {
        meta.extra.insert("animated".into(), "true".into());
        meta.extra.insert("frames".into(), s.frames.to_string());
    }
    // Exif chunk.
    if let Some((off, size)) = s.exif {
        if (16..=MAX_EXIF).contains(&size) && off + size <= win.avail() {
            let skip = match win.read_upto(off, 6) {
                Ok(b) if b.as_slice() == b"Exif\0\0" => 6,
                _ => 0,
            };
            if let Some(w) = walk_blob(src, start + off + skip, size - skip) {
                let em = exif_meta(&w);
                if meta.width.is_none() {
                    meta.width = em.width;
                    meta.height = em.height;
                }
                meta.created = em.created;
                meta.make = em.make;
                meta.model = em.model;
                meta.orientation = em.orientation;
                meta.software = em.software;
            }
        }
    }
    m.meta = meta;
    m.notes.extend(notes);
    crate::common::log_measured(&win, "webp", &m, matches!(s.end, End::Stopped(Fail::Io(_))));
    Ok(Some(Inspected::new(m, Some(s.riff_end))))
}

impl FileFormat for WebpFormat {
    fn id(&self) -> &'static str {
        "webp"
    }

    fn name(&self) -> &'static str {
        "WebP image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["webp"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        20
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    /// The `EXIF` chunk holds an Exif block (a little TIFF file) that may
    /// carry a JPEG thumbnail (carving spec 2.3).
    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "tiff" | "jpeg")
    }

    fn probe(&self, head: &[u8]) -> bool {
        if head.len() < 20 {
            return SIGNATURES[0].matches(head);
        }
        header_ok(head)
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        Ok(inspect(src, start, limit)?.map(|i| i.measured))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        validate_with("WebP", src, inspect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::ExtentStatus;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        c.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.iter().flatten().copied().collect();
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        f.extend_from_slice(b"WEBP");
        f.extend_from_slice(&body);
        f
    }

    fn vp8(w: u16, h: u16, extra: usize) -> Vec<u8> {
        let mut p = vec![0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A];
        p.extend_from_slice(&w.to_le_bytes());
        p.extend_from_slice(&h.to_le_bytes());
        p.resize(10 + extra, 0x33);
        chunk(b"VP8 ", &p)
    }

    fn mem(b: Vec<u8>) -> SharedSource {
        Arc::new(MemSource::new(b))
    }

    #[test]
    fn lossy_lossless_and_extended() {
        let lossy = riff(&[vp8(320, 200, 21)]);
        let m = WebpFormat
            .measure(&mem(lossy.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, lossy.len() as u64);
        assert_eq!((m.meta.width, m.meta.height), (Some(320), Some(200)));
        assert_eq!(m.status, ExtentStatus::Complete);

        let bits: u32 = (99) | (49 << 14) | (1 << 28);
        let mut p = vec![0x2F];
        p.extend_from_slice(&bits.to_le_bytes());
        p.extend_from_slice(&[1, 2, 3]);
        let ll = riff(&[chunk(b"VP8L", &p)]);
        let m = WebpFormat
            .measure(&mem(ll.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!((m.meta.width, m.meta.height), (Some(100), Some(50)));
        assert_eq!(m.meta.extra["alpha"], "true");

        let mut x = vec![0x10, 0, 0, 0];
        x.extend_from_slice(&[0x3F, 1, 0, 0xC7, 0, 0]);
        let ext = riff(&[chunk(b"VP8X", &x), vp8(320, 200, 7), chunk(b"XMP ", b"abc")]);
        let mut padded = ext.clone();
        padded.extend_from_slice(&[0xAA; 100]);
        let m = WebpFormat
            .measure(&mem(padded), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, ext.len() as u64);
        assert_eq!((m.meta.width, m.meta.height), (Some(320), Some(200)));
    }

    #[test]
    fn truncation_and_bad_sizes() {
        let f = riff(&[vp8(64, 64, 200)]);
        let cut = f.len() - 17;
        let m = WebpFormat
            .measure(&mem(f[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(cut as u64));
        // RIFF size too large for a clean chunk walk.
        let mut big = f.clone();
        big.extend_from_slice(&[0u8; 64]);
        big[4..8].copy_from_slice(&((f.len() - 8 + 40) as u32).to_le_bytes());
        let m = WebpFormat.measure(&mem(big), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(f.len() as u64));
    }

    #[test]
    fn rejects_non_webp_riff_and_bad_bitstreams() {
        let mut avi = riff(&[vp8(8, 8, 4)]);
        avi[8..12].copy_from_slice(b"AVI ");
        assert!(WebpFormat
            .measure(&mem(avi), 0, u64::MAX)
            .unwrap()
            .is_none());
        let mut bad = riff(&[vp8(8, 8, 4)]);
        bad[20 + 3] = 0; // break the start code
        assert!(WebpFormat
            .measure(&mem(bad.clone()), 0, u64::MAX)
            .unwrap()
            .is_none());
        let zero = riff(&[vp8(0, 8, 4)]);
        assert!(WebpFormat
            .measure(&mem(zero), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(!WebpFormat.probe(&bad[..8]));
    }

    #[test]
    fn a_chunk_that_jumps_past_the_readable_bytes_never_extends_the_result() {
        // RIFF size 5 bytes larger than the chunks, source cut inside the
        // last chunk: the walk lands beyond the readable bytes.
        let mut f = riff(&[vp8(64, 64, 200)]);
        let chunks_end = f.len();
        f.extend_from_slice(&[0u8; 5]);
        let declared = (f.len() - 8) as u32;
        f[4..8].copy_from_slice(&declared.to_le_bytes());
        let cut = chunks_end - 10;
        let m = WebpFormat
            .measure(&mem(f[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!((m.len, m.err_offset), (cut as u64, Some(cut as u64)));
        // The same through a limit.
        let m = WebpFormat
            .measure(&mem(f.clone()), 0, cut as u64)
            .unwrap()
            .unwrap();
        assert_eq!((m.len, m.status), (cut as u64, ExtentStatus::Capped));
    }

    #[test]
    fn random_mutations_never_panic() {
        let base = riff(&[vp8(33, 17, 100), chunk(b"EXIF", &[0x49; 40])]);
        let mut x = 0xC0FFEEu32;
        for i in 0..4000 {
            let mut b = base.clone();
            for _ in 0..(1 + i % 5) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let p = (x as usize) % b.len();
                b[p] = (x >> 9) as u8;
            }
            if i % 4 == 0 {
                b.truncate((x as usize >> 3) % b.len());
            }
            let s = mem(b.clone());
            let _ = WebpFormat.probe(&b);
            let _ = WebpFormat.measure(&s, 0, u64::MAX);
            let _ = WebpFormat.validate(&s);
        }
    }
}
