//! BMP (Windows and OS/2 bitmaps).
//!
//! # Algorithm
//!
//! A bitmap is a 14-byte file header (`BM`, file size, reserved, offset of
//! the pixel array), a DIB header and the pixel array. The DIB header size
//! identifies its version (12, 40, 52, 56, 64, 108 or 124 bytes); the
//! dimensions, bit depth and compression come from it.
//!
//! The end of the file is the end of the pixel array:
//! `offset + stride * |height|` for uncompressed data, where
//! `stride = ((width * bpp + 31) / 32) * 4`, and `offset + biSizeImage` for
//! RLE, JPEG and PNG payloads. The file-size field is trusted only when it is
//! at least the computed end and at most 64 KiB more (an ICC profile or other
//! trailing data); writers are known to leave it zero or fill in the pixel
//! data size instead, so otherwise the computed end wins.
//!
//! # False positives
//!
//! `BM` is only two bytes. A candidate must have a DIB header size from the
//! list above, one plane, a bit depth that suits the compression, dimensions
//! up to 65535, a pixel-array offset of at most 64 KiB that clears the
//! headers, and a computed size of at most 1 GiB.
//!
//! # Limits
//!
//! The pixel data itself is not examined, so the confidence is `MEDIUM`.

use diskrescue_core::format::{
    Confidence, FileFormat, Measured, Nested, NestedRole, Signature, Validation,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{u16le, u32le};
use diskrescue_core::{Range, Result};

use crate::common::{sized, validate_with, Inspected};
use crate::win::{read_error, Win};

const MAX_SIZE: u64 = 1 << 30;
/// Longest header we ever need: file header + a 124-byte V5 header.
const HEADER_LEN: usize = 14 + 124;
const SIGNATURES: [Signature; 1] = [Signature::at_start(b"BM")];

/// Windows / OS/2 bitmaps.
#[derive(Debug, Clone, Copy, Default)]
pub struct BmpFormat;

/// The decoded header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    file_size: u32,
    off_bits: u32,
    dib_size: u32,
    width: u32,
    height: u32,
    top_down: bool,
    bpp: u16,
    compression: u32,
    size_image: u32,
    reserved_nonzero: bool,
}

/// Parse and sanity-check the headers in `head`. `None` means "not a BMP".
pub(crate) fn parse_header(head: &[u8]) -> Option<Header> {
    if head.get(..2)? != b"BM" {
        return None;
    }
    let file_size = u32le(head, 2)?;
    let reserved = u32le(head, 6)?;
    let off_bits = u32le(head, 10)?;
    let dib_size = u32le(head, 14)?;
    if !matches!(dib_size, 12 | 40 | 52 | 56 | 64 | 108 | 124) {
        return None;
    }
    let (width, height, planes, bpp, compression, size_image, top_down);
    if dib_size == 12 {
        width = u16le(head, 18)? as u32;
        height = u16le(head, 20)? as u32;
        planes = u16le(head, 22)?;
        bpp = u16le(head, 24)?;
        compression = 0;
        size_image = 0;
        top_down = false;
    } else {
        let w = u32le(head, 18)? as i32;
        let h = u32le(head, 22)? as i32;
        planes = u16le(head, 26)?;
        bpp = u16le(head, 28)?;
        compression = u32le(head, 30)?;
        size_image = u32le(head, 34)?;
        if w <= 0 {
            return None;
        }
        width = w as u32;
        top_down = h < 0;
        height = h.unsigned_abs();
    }
    if planes != 1 || width == 0 || height == 0 || width > 65_535 || height > 65_535 {
        return None;
    }
    if !matches!(bpp, 0 | 1 | 2 | 4 | 8 | 16 | 24 | 32) {
        return None;
    }
    if !matches!(compression, 0..=6 | 11..=13) {
        return None;
    }
    // Compression / depth coherence.
    let ok = match compression {
        1 => bpp == 8,
        2 => bpp == 4,
        3 | 6 => bpp == 16 || bpp == 32,
        4 | 5 => true,
        _ => bpp != 0,
    };
    if !ok {
        return None;
    }
    if off_bits < 14 + dib_size || off_bits > 65_536 {
        return None;
    }
    Some(Header {
        file_size,
        off_bits,
        dib_size,
        width,
        height,
        top_down,
        bpp,
        compression,
        size_image,
        reserved_nonzero: reserved != 0,
    })
}

impl Header {
    /// Row stride in bytes for uncompressed data.
    fn stride(&self) -> u64 {
        (self.width as u64 * self.bpp as u64).div_ceil(32) * 4
    }

    /// True when the pixel array size comes from `biSizeImage`.
    fn sized_by_field(&self) -> bool {
        matches!(self.compression, 1 | 2 | 4 | 5 | 12 | 13)
    }

    fn compression_name(&self) -> &'static str {
        match self.compression {
            0 => "rgb",
            1 => "rle8",
            2 => "rle4",
            3 => "bitfields",
            4 => "jpeg",
            5 => "png",
            6 => "alphabitfields",
            11 => "cmyk",
            12 => "cmykrle8",
            13 => "cmykrle4",
            _ => "unknown",
        }
    }
}

fn inspect(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Inspected>> {
    let limit = limit.min(MAX_SIZE);
    let win = Win::new(src, start, limit, 4096);
    let head = match win.read_upto(0, HEADER_LEN) {
        Ok(h) => h,
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let Some(h) = parse_header(&head) else {
        return Ok(None);
    };
    let mut notes: Vec<String> = Vec::new();
    // Pixel array length.
    let data_len = if h.sized_by_field() {
        h.size_image as u64
    } else {
        h.stride() * h.height as u64
    };
    let off = h.off_bits as u64;
    let mut computed_end = off + data_len;
    if h.sized_by_field() && h.size_image == 0 {
        // No size recorded: fall back to the file-size field.
        if (h.file_size as u64) > off {
            computed_end = h.file_size as u64;
            notes.push("biSizeImage is zero; used the file size field".into());
        } else {
            return Ok(None);
        }
    }
    if computed_end > MAX_SIZE {
        return Ok(None);
    }
    let fs = h.file_size as u64;
    let mut conf = Confidence::MEDIUM;
    let len = if fs >= computed_end && fs - computed_end < 64 * 1024 {
        if fs != computed_end {
            notes.push(format!(
                "file size field exceeds the pixel array by {} bytes",
                fs - computed_end
            ));
        }
        fs
    } else {
        // The size field is unusable (zero, too small or absurdly large):
        // the length comes from the pixel array alone.
        conf = conf.lowered(10);
        if fs != 0 {
            notes.push(format!(
                "file size field ({fs}) disagrees with the pixel array end ({computed_end}); used the latter"
            ));
        }
        computed_end
    };
    if h.reserved_nonzero {
        conf = conf.lowered(10);
        notes.push("reserved header field is not zero".into());
    }
    if h.dib_size == 12 && h.bpp > 8 && h.bpp != 24 {
        conf = conf.lowered(10);
    }
    let mut m = sized(&win, len, conf, "the bitmap");
    m.notes.extend(notes);
    let mut meta = MediaMeta {
        width: Some(h.width),
        height: Some(h.height),
        ..MediaMeta::default()
    };
    meta.extra
        .insert("bits_per_pixel".into(), h.bpp.to_string());
    meta.extra
        .insert("compression".into(), h.compression_name().into());
    if h.top_down {
        meta.extra.insert("row_order".into(), "top-down".into());
    }
    m.meta = meta;
    if matches!(h.compression, 4 | 5) && h.size_image > 0 {
        m.nested.push(Nested {
            range: Range::with_len(off, h.size_image as u64)
                .intersect(Range::new(0, len))
                .unwrap_or(Range::new(0, 0)),
            format: Some(if h.compression == 4 { "jpeg" } else { "png" }.into()),
            role: NestedRole::Container,
        });
    }
    crate::common::log_measured(&win, "bmp", &m, false);
    Ok(Some(Inspected::new(m, Some(len))))
}

impl FileFormat for BmpFormat {
    fn id(&self) -> &'static str {
        "bmp"
    }

    fn name(&self) -> &'static str {
        "BMP image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["bmp", "dib"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        58
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        30
    }

    fn probe(&self, head: &[u8]) -> bool {
        parse_header(head).is_some()
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        Ok(inspect(src, start, limit)?.map(|i| i.measured))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        validate_with("BMP", src, inspect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::ExtentStatus;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    /// Build an uncompressed bitmap with an INFO header.
    pub(crate) fn make_bmp(w: u32, h: i32, bpp: u16, file_size: Option<u32>) -> Vec<u8> {
        let stride = (w as usize * bpp as usize).div_ceil(32) * 4;
        let palette = if bpp <= 8 { 4usize << bpp } else { 0 };
        let off = 14 + 40 + palette;
        let data = stride * h.unsigned_abs() as usize;
        let total = off + data;
        let mut b = Vec::with_capacity(total);
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&file_size.unwrap_or(total as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&(off as u32).to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&bpp.to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        b.resize(total, 0x5A);
        b
    }

    fn mem(b: Vec<u8>) -> SharedSource {
        Arc::new(MemSource::new(b))
    }

    #[test]
    fn exact_sizes_and_strides() {
        for (w, h, bpp) in [
            (3u32, 5i32, 24u16),
            (7, 2, 32),
            (9, 4, 1),
            (5, -3, 8),
            (10, 10, 4),
        ] {
            let b = make_bmp(w, h, bpp, None);
            let mut padded = b.clone();
            padded.extend_from_slice(&[0xEE; 300]);
            let m = BmpFormat
                .measure(&mem(padded), 0, u64::MAX)
                .unwrap()
                .unwrap();
            assert_eq!(m.len, b.len() as u64, "{w}x{h}x{bpp}");
            assert_eq!(m.status, ExtentStatus::Complete);
            assert_eq!(m.meta.width, Some(w));
            assert_eq!(m.meta.height, Some(h.unsigned_abs()));
        }
    }

    #[test]
    fn zero_or_wrong_file_size_field_uses_computed_end() {
        for fs in [0u32, 17, 5] {
            let b = make_bmp(4, 4, 24, Some(fs));
            let m = BmpFormat
                .measure(&mem(b.clone()), 0, u64::MAX)
                .unwrap()
                .unwrap();
            assert_eq!(m.len, b.len() as u64);
        }
        // Field slightly larger (trailing profile): accepted.
        let b = make_bmp(4, 4, 24, None);
        let mut big = b.clone();
        big[2..6].copy_from_slice(&((b.len() + 100) as u32).to_le_bytes());
        big.extend_from_slice(&[1; 100]);
        let m = BmpFormat.measure(&mem(big), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, b.len() as u64 + 100);
        // Field absurdly larger: ignored.
        let mut huge = b.clone();
        huge[2..6].copy_from_slice(&0x7000_0000u32.to_le_bytes());
        let m = BmpFormat.measure(&mem(huge), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, b.len() as u64);
    }

    #[test]
    fn truncation_is_reported_at_the_cut() {
        let b = make_bmp(16, 16, 24, None);
        let cut = b.len() - 100;
        let m = BmpFormat
            .measure(&mem(b[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.err_offset, Some(cut as u64));
        assert_eq!(m.len, cut as u64);
    }

    #[test]
    fn rejects_bad_headers() {
        let good = make_bmp(8, 8, 24, None);
        for (off, val) in [(14usize, 41u8), (26, 2), (28, 7), (30, 9)] {
            let mut b = good.clone();
            b[off] = val;
            assert!(
                BmpFormat
                    .measure(&mem(b.clone()), 0, u64::MAX)
                    .unwrap()
                    .is_none(),
                "offset {off}"
            );
            assert!(!BmpFormat.probe(&b));
        }
        assert!(BmpFormat.probe(&good));
        assert!(BmpFormat
            .measure(&mem(b"BM".to_vec()), 0, u64::MAX)
            .unwrap()
            .is_none());
    }

    #[test]
    fn core_header_bitmaps_are_understood() {
        let (w, h) = (5u16, 3u16);
        let stride = (w as usize * 24).div_ceil(32) * 4;
        let off = 14 + 12;
        let total = off + stride * h as usize;
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&(total as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&(off as u32).to_le_bytes());
        b.extend_from_slice(&12u32.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&24u16.to_le_bytes());
        b.resize(total, 1);
        let m = BmpFormat
            .measure(&mem(b.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, total as u64);
        assert_eq!(m.meta.dimensions().unwrap(), "5x3");
    }

    #[test]
    fn validate_flags_trailing_and_truncated() {
        let b = make_bmp(8, 8, 24, None);
        let v = BmpFormat.validate(&mem(b.clone())).unwrap();
        assert_eq!(v.status, diskrescue_core::format::ValidationStatus::Valid);
        let v = BmpFormat
            .validate(&mem(b[..b.len() - 10].to_vec()))
            .unwrap();
        assert_eq!(v.status, diskrescue_core::format::ValidationStatus::Damaged);
        assert!(v.intact_fraction.unwrap() < 1.0);
    }

    #[test]
    fn random_headers_never_panic() {
        let mut x = 0xDEAD_BEEFu32;
        for i in 0..5000 {
            let mut b = make_bmp(4, 4, 24, None);
            for _ in 0..8 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let p = (x as usize) % 60;
                b[p] = (x >> 8) as u8;
            }
            if i % 7 == 0 {
                b.truncate((x as usize) % b.len());
            }
            let s = mem(b.clone());
            let _ = BmpFormat.probe(&b);
            let _ = BmpFormat.measure(&s, 0, u64::MAX);
            let _ = BmpFormat.validate(&s);
        }
    }
}
