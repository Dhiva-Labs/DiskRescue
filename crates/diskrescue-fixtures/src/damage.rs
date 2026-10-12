//! Damage injectors: functions that turn a healthy file into a broken one in
//! a controlled, reproducible way.
//!
//! The generic helpers ([`truncate`], [`zero_range`], [`flip_bytes`],
//! [`remove_range`], [`overwrite_prefix`]) work on any byte buffer. The
//! format-aware helpers know just enough structure to damage one specific
//! part: [`strip_box`] removes a top-level ISO BMFF box, [`png_corrupt_crc`]
//! breaks a chunk CRC, [`avi_strip_idx1`] removes the AVI index. All of them
//! tolerate malformed input (they leave unparsable tails alone) and never
//! panic.

use crate::rng::Rng;

/// Keep only the first `keep` bytes.
pub fn truncate(data: &[u8], keep: usize) -> Vec<u8> {
    data[..keep.min(data.len())].to_vec()
}

/// Zero `len` bytes starting at `start` (clamped to the buffer).
pub fn zero_range(data: &mut [u8], start: usize, len: usize) {
    let s = start.min(data.len());
    let e = start.saturating_add(len).min(data.len());
    data[s..e].fill(0);
}

/// Corrupt `count` distinct byte positions at or after `protect_prefix` by
/// XOR with a non-zero random value. Deterministic in `seed`. With fewer
/// than `count` candidate positions, every candidate is flipped.
pub fn flip_bytes(data: &mut [u8], seed: u64, count: usize, protect_prefix: usize) {
    if data.len() <= protect_prefix {
        return;
    }
    let span = data.len() - protect_prefix;
    let count = count.min(span);
    let mut rng = Rng::new(seed);
    let mut chosen = std::collections::BTreeSet::new();
    while chosen.len() < count {
        chosen.insert(protect_prefix + rng.below_usize(span));
    }
    for pos in chosen {
        let x = (rng.next_u32() % 255 + 1) as u8;
        data[pos] ^= x;
    }
}

/// Delete `len` bytes at `start` and close the gap (a file with a hole cut
/// out, as opposed to [`zero_range`], which keeps the length).
pub fn remove_range(data: &[u8], start: usize, len: usize) -> Vec<u8> {
    let s = start.min(data.len());
    let e = start.saturating_add(len).min(data.len());
    let mut out = Vec::with_capacity(data.len() - (e - s));
    out.extend_from_slice(&data[..s]);
    out.extend_from_slice(&data[e..]);
    out
}

/// Replace the first `len` bytes with random bytes, simulating a header
/// overwritten by another file.
pub fn overwrite_prefix(data: &mut [u8], len: usize, seed: u64) {
    let n = len.min(data.len());
    Rng::new(seed).fill(&mut data[..n]);
}

/// A top-level ISO BMFF box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopBox {
    /// Four-character type.
    pub kind: [u8; 4],
    /// Offset of the box header in the file.
    pub offset: usize,
    /// Total size in bytes (header included).
    pub size: usize,
}

/// Walk the top-level boxes: `size32 type [largesize]`, `size == 1` means a
/// 64-bit size follows, `size == 0` means "to the end of the file". Stops at
/// the first box that does not fit the buffer.
pub fn top_level_boxes(data: &[u8]) -> Vec<TopBox> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let kind = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let size = match size32 {
            0 => data.len() - pos,
            1 => {
                if pos + 16 > data.len() {
                    break;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[pos + 8..pos + 16]);
                match usize::try_from(u64::from_be_bytes(b)) {
                    Ok(v) => v,
                    Err(_) => break,
                }
            }
            n => n as usize,
        };
        if size < 8 || pos.checked_add(size).is_none_or(|e| e > data.len()) {
            break;
        }
        out.push(TopBox {
            kind,
            offset: pos,
            size,
        });
        pos += size;
    }
    out
}

/// Remove every top-level box of type `box_type` (for example `moov`). Bytes
/// after an unparsable box are kept unchanged.
pub fn strip_box(mp4: &[u8], box_type: &[u8; 4]) -> Vec<u8> {
    let boxes = top_level_boxes(mp4);
    let mut out = Vec::with_capacity(mp4.len());
    let mut pos = 0;
    for b in boxes {
        if &b.kind == box_type {
            out.extend_from_slice(&mp4[pos..b.offset]);
            pos = b.offset + b.size;
        }
    }
    out.extend_from_slice(&mp4[pos..]);
    out
}

/// A PNG chunk position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PngChunk {
    /// Chunk type.
    pub kind: [u8; 4],
    /// Offset of the length field.
    pub offset: usize,
    /// Data length.
    pub len: usize,
}

/// List the chunks of a PNG (stops at the first chunk that does not fit).
pub fn png_chunks(png: &[u8]) -> Vec<PngChunk> {
    let mut out = Vec::new();
    let mut pos = 8;
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let kind = [png[pos + 4], png[pos + 5], png[pos + 6], png[pos + 7]];
        if pos + 12 + len > png.len() {
            break;
        }
        out.push(PngChunk {
            kind,
            offset: pos,
            len,
        });
        pos += 12 + len;
    }
    out
}

/// Flip a bit in the CRC of the `nth` chunk of type `kind` (0-based). Returns
/// the offset of the damaged CRC byte, or `None` if there is no such chunk.
pub fn png_corrupt_crc(png: &mut [u8], kind: &[u8; 4], nth: usize) -> Option<usize> {
    let c = png_chunks(png)
        .into_iter()
        .filter(|c| &c.kind == kind)
        .nth(nth)?;
    let at = c.offset + 8 + c.len;
    png[at] ^= 0x01;
    Some(at)
}

/// Remove the `nth` chunk of type `kind` entirely (length through CRC).
pub fn png_strip_chunk(png: &[u8], kind: &[u8; 4], nth: usize) -> Vec<u8> {
    match png_chunks(png)
        .into_iter()
        .filter(|c| &c.kind == kind)
        .nth(nth)
    {
        Some(c) => remove_range(png, c.offset, 12 + c.len),
        None => png.to_vec(),
    }
}

/// Remove the `idx1` index chunk of an AVI, fix the RIFF size and clear the
/// `AVIF_HASINDEX` flag, as a recorder that crashed before finalising would
/// leave it (the `movi` list and headers are intact).
pub fn avi_strip_idx1(avi: &[u8]) -> Vec<u8> {
    if avi.len() < 12 || &avi[..4] != b"RIFF" || &avi[8..12] != b"AVI " {
        return avi.to_vec();
    }
    let mut out = avi.to_vec();
    // Top-level chunks inside the RIFF.
    let mut pos = 12;
    let mut cut = None;
    while pos + 8 <= out.len() {
        let id = [out[pos], out[pos + 1], out[pos + 2], out[pos + 3]];
        let size =
            u32::from_le_bytes([out[pos + 4], out[pos + 5], out[pos + 6], out[pos + 7]]) as usize;
        let padded = size + (size & 1);
        if pos + 8 + size > out.len() {
            break;
        }
        if &id == b"idx1" {
            cut = Some((pos, (8 + padded).min(out.len() - pos)));
            break;
        }
        pos += 8 + padded;
    }
    if let Some((start, len)) = cut {
        out = remove_range(&out, start, len);
        let riff = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&riff.to_le_bytes());
    }
    // avih.dwFlags &= ~AVIF_HASINDEX
    if let Some(i) = out.windows(4).position(|w| w == b"avih") {
        let flags_at = i + 8 + 12;
        if flags_at + 4 <= out.len() {
            out[flags_at] &= !0x10;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn generic_helpers() {
        let d: Vec<u8> = (0..100u8).collect();
        assert_eq!(truncate(&d, 10), (0..10u8).collect::<Vec<_>>());
        assert_eq!(truncate(&d, 1000).len(), 100);
        let mut z = d.clone();
        zero_range(&mut z, 90, 50);
        assert!(z[90..].iter().all(|&b| b == 0));
        assert_eq!(z[89], 89);
        zero_range(&mut z, 500, 5);
        let r = remove_range(&d, 10, 20);
        assert_eq!(r.len(), 80);
        assert_eq!(r[9], 9);
        assert_eq!(r[10], 30);
        assert_eq!(remove_range(&d, 95, 50).len(), 95);
        let mut o = d.clone();
        overwrite_prefix(&mut o, 16, 5);
        assert_ne!(o[..16], d[..16]);
        assert_eq!(o[16..], d[16..]);
        let mut o2 = d.clone();
        overwrite_prefix(&mut o2, 16, 5);
        assert_eq!(o, o2);
    }

    #[test]
    fn flip_respects_prefix_and_count() {
        let d = vec![0u8; 1000];
        let mut f = d.clone();
        flip_bytes(&mut f, 9, 20, 100);
        assert_eq!(f.iter().filter(|&&b| b != 0).count(), 20);
        assert!(f[..100].iter().all(|&b| b == 0));
        let mut g = d.clone();
        flip_bytes(&mut g, 9, 20, 100);
        assert_eq!(f, g);
        let mut tiny = vec![0u8; 5];
        flip_bytes(&mut tiny, 1, 50, 2);
        assert_eq!(tiny.iter().filter(|&&b| b != 0).count(), 3);
        flip_bytes(&mut tiny, 1, 50, 99);
    }

    #[test]
    fn strip_box_removes_only_matching() {
        let mut f = boxed(b"ftyp", b"isom0000");
        f.extend(boxed(b"moov", &[1; 40]));
        f.extend(boxed(b"mdat", &[2; 100]));
        let s = strip_box(&f, b"moov");
        assert_eq!(s.len(), f.len() - 48);
        let b = top_level_boxes(&s);
        assert_eq!(b.len(), 2);
        assert_eq!(&b[0].kind, b"ftyp");
        assert_eq!(&b[1].kind, b"mdat");
        // No match: unchanged. Garbage: unchanged and no panic.
        assert_eq!(strip_box(&f, b"free"), f);
        assert_eq!(strip_box(&[1, 2, 3], b"moov"), vec![1, 2, 3]);
        let mut trunc = f.clone();
        trunc.truncate(f.len() - 10);
        let t = strip_box(&trunc, b"moov");
        assert_eq!(t.len(), trunc.len() - 48);
    }

    #[test]
    fn box_walker_handles_large_and_zero_sizes() {
        let mut f = boxed(b"ftyp", b"isom0000");
        // 64-bit size box.
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(b"mdat");
        f.extend_from_slice(&(16u64 + 4).to_be_bytes());
        f.extend_from_slice(&[7; 4]);
        // size 0 = to EOF.
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(b"free");
        f.extend_from_slice(&[0; 9]);
        let b = top_level_boxes(&f);
        assert_eq!(b.len(), 3);
        assert_eq!(b[1].size, 20);
        assert_eq!(b[2].size, 17);
        // Absurd size stops the walk.
        let mut bad = boxed(b"ftyp", b"isom0000");
        bad.extend_from_slice(&u32::MAX.to_be_bytes());
        bad.extend_from_slice(b"moov");
        assert_eq!(top_level_boxes(&bad).len(), 1);
    }

    #[test]
    fn png_helpers() {
        let p = crate::media::png(&crate::media::PngOpts {
            width: 32,
            height: 32,
            idat_chunk: 200,
            ..Default::default()
        });
        let chunks = png_chunks(&p);
        assert_eq!(&chunks[0].kind, b"IHDR");
        assert_eq!(&chunks.last().unwrap().kind, b"IEND");
        let idat = chunks.iter().filter(|c| &c.kind == b"IDAT").count();
        assert!(idat >= 2);
        let mut bad = p.clone();
        let at = png_corrupt_crc(&mut bad, b"IDAT", 1).unwrap();
        assert_ne!(bad[at], p[at]);
        assert!(png_corrupt_crc(&mut bad, b"PLTE", 0).is_none());
        let s = png_strip_chunk(&p, b"tEXt", 0);
        assert!(s.len() < p.len());
        assert!(png_chunks(&s).iter().all(|c| &c.kind != b"tEXt"));
        assert_eq!(png_strip_chunk(&p, b"zzzz", 0), p);
    }

    #[test]
    fn avi_strip_idx1_on_synthetic_avi() {
        // RIFF AVI { LIST hdrl { avih }, LIST movi, idx1 }
        let mut avih = Vec::new();
        avih.extend_from_slice(b"avih");
        avih.extend_from_slice(&56u32.to_le_bytes());
        let mut body = vec![0u8; 56];
        body[12] = 0x10;
        avih.extend_from_slice(&body);
        let mut hdrl = b"hdrl".to_vec();
        hdrl.extend_from_slice(&avih);
        let mut list = b"LIST".to_vec();
        list.extend_from_slice(&(hdrl.len() as u32).to_le_bytes());
        list.extend_from_slice(&hdrl);
        let mut movi = b"LIST".to_vec();
        movi.extend_from_slice(&4u32.to_le_bytes());
        movi.extend_from_slice(b"movi");
        let mut idx1 = b"idx1".to_vec();
        idx1.extend_from_slice(&16u32.to_le_bytes());
        idx1.extend_from_slice(&[9; 16]);
        let mut riff = b"RIFF".to_vec();
        let inner: Vec<u8> = [b"AVI ".to_vec(), list, movi, idx1].concat();
        riff.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        riff.extend_from_slice(&inner);
        let out = avi_strip_idx1(&riff);
        assert_eq!(out.len(), riff.len() - 24);
        assert!(!out.windows(4).any(|w| w == b"idx1"));
        assert_eq!(
            u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
            out.len() - 8
        );
        let i = out.windows(4).position(|w| w == b"avih").unwrap();
        assert_eq!(out[i + 8 + 12] & 0x10, 0);
        assert_eq!(avi_strip_idx1(b"garbage"), b"garbage".to_vec());
    }
}
