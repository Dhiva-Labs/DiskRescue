//! A small TIFF writer: a generic IFD layout engine plus the two fixtures
//! built on it, a baseline uncompressed RGB TIFF and a Canon CR2-like file
//! (`CR` magic at offset 8, four IFDs, JPEG preview, raw strip).
//!
//! Layout produced by [`TiffBuilder::build`]:
//!
//! ```text
//! header (8 bytes, or custom) | IFD0 table | IFD0 out-of-line values |
//! IFD1 table | ... | blob 0 | blob 1 | ...
//! ```
//!
//! Entries are sorted by tag inside each IFD as TIFF 6.0 requires. Values
//! longer than four bytes go out of line directly after their IFD table;
//! "blobs" (strips, embedded JPEGs) are appended after every IFD so an entry
//! can reference a blob's offset or length before the blob is laid out.
//! Everything is word-aligned.

use crate::media::canvas;
use crate::rng::Rng;

/// TIFF field types we emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// 8-bit unsigned.
    Byte = 1,
    /// NUL-terminated 7-bit ASCII.
    Ascii = 2,
    /// 16-bit unsigned.
    Short = 3,
    /// 32-bit unsigned.
    Long = 4,
    /// Two LONGs (numerator, denominator).
    Rational = 5,
    /// Opaque bytes.
    Undefined = 7,
}

impl FieldType {
    fn size(self) -> usize {
        match self {
            FieldType::Byte | FieldType::Ascii | FieldType::Undefined => 1,
            FieldType::Short => 2,
            FieldType::Long => 4,
            FieldType::Rational => 8,
        }
    }
}

/// How an entry's value is produced.
#[derive(Debug, Clone)]
pub enum Value {
    /// Literal values (already in count units).
    Shorts(Vec<u16>),
    /// Literal LONGs.
    Longs(Vec<u32>),
    /// ASCII string (NUL appended automatically).
    Ascii(String),
    /// Raw bytes with the given type.
    Bytes(FieldType, Vec<u8>),
    /// One RATIONAL.
    Rational(u32, u32),
    /// Offset of blob `n` as a LONG.
    BlobOffset(usize),
    /// Length of blob `n` as a LONG.
    BlobLen(usize),
    /// Offset of IFD `n` as a LONG (sub-IFD pointer such as ExifIFD).
    IfdOffset(usize),
}

/// One directory entry before layout.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Tag number.
    pub tag: u16,
    /// Value.
    pub value: Value,
}

/// One IFD before layout.
#[derive(Debug, Clone, Default)]
pub struct Ifd {
    /// Entries (any order; sorted on output).
    pub entries: Vec<Entry>,
    /// Index of the IFD linked from this one's "next IFD" pointer.
    pub next: Option<usize>,
}

impl Ifd {
    /// Add an entry.
    pub fn push(&mut self, tag: u16, value: Value) -> &mut Self {
        self.entries.push(Entry { tag, value });
        self
    }
}

/// Builder for a whole TIFF structure.
#[derive(Debug, Clone)]
pub struct TiffBuilder {
    /// Big-endian (`MM`) instead of little-endian (`II`).
    pub big_endian: bool,
    /// Bytes between the 8-byte header and IFD0 (CR2 uses 8 extra bytes).
    pub header_extra: Vec<u8>,
    /// IFDs; IFD0 is index 0 and is the one the header points at.
    pub ifds: Vec<Ifd>,
    /// Opaque data blocks referenced by `Value::BlobOffset/BlobLen`.
    pub blobs: Vec<Vec<u8>>,
}

/// Where things ended up after [`TiffBuilder::build`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TiffLayout {
    /// Offset of each IFD table (from the start of the TIFF header).
    pub ifd_offsets: Vec<u32>,
    /// Offset of each blob.
    pub blob_offsets: Vec<u32>,
}

impl TiffBuilder {
    /// Empty little-endian builder.
    pub fn new() -> Self {
        TiffBuilder {
            big_endian: false,
            header_extra: Vec::new(),
            ifds: Vec::new(),
            blobs: Vec::new(),
        }
    }

    /// Add an IFD and return its index.
    pub fn add_ifd(&mut self, ifd: Ifd) -> usize {
        self.ifds.push(ifd);
        self.ifds.len() - 1
    }

    /// Add a blob and return its index.
    pub fn add_blob(&mut self, data: Vec<u8>) -> usize {
        self.blobs.push(data);
        self.blobs.len() - 1
    }

    fn put16(&self, out: &mut Vec<u8>, v: u16) {
        if self.big_endian {
            out.extend_from_slice(&v.to_be_bytes());
        } else {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    fn put32(&self, out: &mut Vec<u8>, v: u32) {
        if self.big_endian {
            out.extend_from_slice(&v.to_be_bytes());
        } else {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    /// Serialise a value into (type, count, raw bytes), given the layout.
    fn encode_value(&self, v: &Value, layout: &TiffLayout) -> (FieldType, u32, Vec<u8>) {
        let mut raw = Vec::new();
        match v {
            Value::Shorts(s) => {
                for &x in s {
                    self.put16(&mut raw, x);
                }
                (FieldType::Short, s.len() as u32, raw)
            }
            Value::Longs(l) => {
                for &x in l {
                    self.put32(&mut raw, x);
                }
                (FieldType::Long, l.len() as u32, raw)
            }
            Value::Ascii(s) => {
                raw.extend_from_slice(s.as_bytes());
                raw.push(0);
                (FieldType::Ascii, raw.len() as u32, raw)
            }
            Value::Bytes(t, b) => (*t, (b.len() / t.size().max(1)) as u32, b.clone()),
            Value::Rational(n, d) => {
                self.put32(&mut raw, *n);
                self.put32(&mut raw, *d);
                (FieldType::Rational, 1, raw)
            }
            Value::BlobOffset(i) => {
                self.put32(&mut raw, layout.blob_offsets.get(*i).copied().unwrap_or(0));
                (FieldType::Long, 1, raw)
            }
            Value::BlobLen(i) => {
                self.put32(
                    &mut raw,
                    self.blobs.get(*i).map(|b| b.len() as u32).unwrap_or(0),
                );
                (FieldType::Long, 1, raw)
            }
            Value::IfdOffset(i) => {
                self.put32(&mut raw, layout.ifd_offsets.get(*i).copied().unwrap_or(0));
                (FieldType::Long, 1, raw)
            }
        }
    }

    /// Compute the layout and emit the bytes.
    pub fn build(&self) -> (Vec<u8>, TiffLayout) {
        let header_len = 8 + self.header_extra.len();
        let mut layout = TiffLayout::default();
        // Pass 1: positions.
        let mut pos = pad2(header_len);
        for ifd in &self.ifds {
            layout.ifd_offsets.push(pos as u32);
            pos += 2 + 12 * ifd.entries.len() + 4;
            for e in &ifd.entries {
                let (_, _, raw) = self.encode_value(&e.value, &layout);
                if raw.len() > 4 {
                    pos += pad2(raw.len());
                }
            }
        }
        for b in &self.blobs {
            layout.blob_offsets.push(pos as u32);
            pos += pad2(b.len());
        }
        // Pass 2: emit.
        let mut out = Vec::with_capacity(pos);
        out.extend_from_slice(if self.big_endian { b"MM" } else { b"II" });
        self.put16(&mut out, 42);
        self.put32(&mut out, layout.ifd_offsets.first().copied().unwrap_or(0));
        out.extend_from_slice(&self.header_extra);
        while out.len() < pad2(header_len) {
            out.push(0);
        }
        for ifd in &self.ifds {
            let mut entries = ifd.entries.clone();
            entries.sort_by_key(|e| e.tag);
            let table_len = 2 + 12 * entries.len() + 4;
            let mut data_pos = out.len() + table_len;
            let mut tail: Vec<u8> = Vec::new();
            self.put16(&mut out, entries.len() as u16);
            for e in &entries {
                let (t, count, raw) = self.encode_value(&e.value, &layout);
                self.put16(&mut out, e.tag);
                self.put16(&mut out, t as u16);
                self.put32(&mut out, count);
                if raw.len() <= 4 {
                    let mut inline = raw.clone();
                    inline.resize(4, 0);
                    out.extend_from_slice(&inline);
                } else {
                    self.put32(&mut out, data_pos as u32);
                    tail.extend_from_slice(&raw);
                    if raw.len() % 2 == 1 {
                        tail.push(0);
                    }
                    data_pos += pad2(raw.len());
                }
            }
            let next = ifd
                .next
                .and_then(|n| layout.ifd_offsets.get(n).copied())
                .unwrap_or(0);
            self.put32(&mut out, next);
            out.extend_from_slice(&tail);
        }
        for b in &self.blobs {
            out.extend_from_slice(b);
            if b.len() % 2 == 1 {
                out.push(0);
            }
        }
        (out, layout)
    }
}

impl Default for TiffBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn pad2(n: usize) -> usize {
    n + (n & 1)
}

/// Options for [`tiff`] and [`cr2_like`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiffOpts {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Content seed.
    pub seed: u64,
    /// Rows per strip (several strips exercise StripOffsets arrays).
    pub rows_per_strip: u32,
    /// Big-endian (`MM`) file.
    pub big_endian: bool,
    /// Camera make written to tag 0x010F.
    pub make: String,
    /// Camera model written to tag 0x0110.
    pub model: String,
}

impl Default for TiffOpts {
    fn default() -> Self {
        TiffOpts {
            width: 160,
            height: 120,
            seed: 1,
            rows_per_strip: 16,
            big_endian: false,
            make: "DiskRescue".into(),
            model: "FixtureCam".into(),
        }
    }
}

/// Add the standard baseline-RGB tags for an uncompressed strip image whose
/// strips are blobs `first_blob .. first_blob + nstrips`.
fn rgb_strip_ifd(
    b: &mut TiffBuilder,
    ifd: &mut Ifd,
    w: u32,
    h: u32,
    rows_per_strip: u32,
    pixels: &[u8],
) {
    let rps = rows_per_strip.max(1).min(h);
    let stride = w as usize * 3;
    let mut offsets = Vec::new();
    let mut lens = Vec::new();
    for strip in pixels.chunks(stride * rps as usize) {
        let idx = b.add_blob(strip.to_vec());
        offsets.push(idx);
        lens.push(strip.len() as u32);
    }
    ifd.push(0x0100, Value::Longs(vec![w]));
    ifd.push(0x0101, Value::Longs(vec![h]));
    ifd.push(0x0102, Value::Shorts(vec![8, 8, 8]));
    ifd.push(0x0103, Value::Shorts(vec![1]));
    ifd.push(0x0106, Value::Shorts(vec![2]));
    ifd.push(0x0115, Value::Shorts(vec![3]));
    ifd.push(0x0116, Value::Longs(vec![rps]));
    ifd.push(0x011A, Value::Rational(72, 1));
    ifd.push(0x011B, Value::Rational(72, 1));
    ifd.push(0x0128, Value::Shorts(vec![2]));
    ifd.push(0x011C, Value::Shorts(vec![1]));
    if offsets.len() == 1 {
        ifd.push(0x0111, Value::BlobOffset(offsets[0]));
        ifd.push(0x0117, Value::Longs(lens));
    } else {
        // Multi-strip offsets must be resolved after blob layout, so they go
        // through a two-step build (see `tiff`).
        ifd.push(
            0x0111,
            Value::Longs(offsets.iter().map(|&i| i as u32).collect()),
        );
        ifd.push(0x0117, Value::Longs(lens));
    }
}

/// Resolve StripOffsets entries that were left as blob indices: build once
/// to learn the blob offsets, substitute, build again (layout is identical
/// because value sizes do not change).
fn resolve_strip_offsets(b: &mut TiffBuilder) -> (Vec<u8>, TiffLayout) {
    let (_, layout) = b.build();
    for ifd in &mut b.ifds {
        for e in &mut ifd.entries {
            if e.tag == 0x0111 {
                if let Value::Longs(v) = &e.value {
                    if v.len() > 1 {
                        let resolved = v
                            .iter()
                            .map(|&i| layout.blob_offsets.get(i as usize).copied().unwrap_or(0))
                            .collect();
                        e.value = Value::Longs(resolved);
                    }
                }
            }
        }
    }
    b.build()
}

/// Baseline uncompressed RGB TIFF with several strips.
pub fn tiff(opts: &TiffOpts) -> Vec<u8> {
    let (w, h) = canvas::clamp_dims(opts.width, opts.height);
    let pixels = canvas::canvas_rgb(w, h, opts.seed);
    let mut b = TiffBuilder {
        big_endian: opts.big_endian,
        ..TiffBuilder::new()
    };
    let mut ifd0 = Ifd::default();
    ifd0.push(0x010F, Value::Ascii(opts.make.clone()));
    ifd0.push(0x0110, Value::Ascii(opts.model.clone()));
    ifd0.push(0x0131, Value::Ascii("DiskRescue fixtures".into()));
    rgb_strip_ifd(&mut b, &mut ifd0, w, h, opts.rows_per_strip, &pixels);
    b.add_ifd(ifd0);
    resolve_strip_offsets(&mut b).0
}

/// A Canon CR2-like TIFF: `II*\0`, IFD0 at 16, `CR\x02\x00` at offset 8,
/// IFD3 offset at 12, four chained IFDs: IFD0 = JPEG preview (compression 6,
/// 0x0111/0x0117 strip = the JPEG), IFD1 = thumbnail (0x0201/0x0202), IFD2 =
/// small uncompressed RGB, IFD3 = "raw" strip (random bytes starting with
/// `FF D8 FF C3`, lossless-JPEG style) with the 0xC640 CR2 slice tag. The
/// file is structurally valid for walkers; it is not a decodable raw.
pub fn cr2_like(opts: &TiffOpts) -> Vec<u8> {
    let (w, h) = canvas::clamp_dims(opts.width, opts.height);
    let (w, h) = (w.max(8), h.max(8));
    let preview = crate::media::jpeg::baseline_jpeg(w, h, 75, opts.seed);
    let thumb = crate::media::jpeg::baseline_jpeg(w / 4, h / 4, 50, opts.seed);
    let mut rng = Rng::new(opts.seed ^ 0xC2);
    let mut raw = vec![0xFF, 0xD8, 0xFF, 0xC3];
    let mut body = rng.bytes((w as usize * h as usize * 3 / 2).max(64));
    // Random data must not accidentally contain a JPEG signature.
    for i in 0..body.len().saturating_sub(2) {
        if body[i] == 0xFF && body[i + 1] == 0xD8 {
            body[i] = 0xFE;
        }
    }
    raw.extend(body);
    raw.extend_from_slice(&[0xFF, 0xD9]);

    let mut b = TiffBuilder::new();
    let ifd3_offset_pos = 12;
    b.header_extra = {
        let mut v = b"CR\x02\x00".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]); // IFD3 offset, patched below
        v
    };
    let preview_blob = b.add_blob(preview);
    let thumb_blob = b.add_blob(thumb);
    let small_pixels = canvas::canvas_rgb(w / 2, h / 2, opts.seed);
    let raw_blob = b.add_blob(raw);

    let mut ifd0 = Ifd::default();
    ifd0.push(0x0100, Value::Longs(vec![w]));
    ifd0.push(0x0101, Value::Longs(vec![h]));
    ifd0.push(0x0102, Value::Shorts(vec![8, 8, 8]));
    ifd0.push(0x0103, Value::Shorts(vec![6]));
    ifd0.push(0x010F, Value::Ascii("Canon".into()));
    ifd0.push(0x0110, Value::Ascii(format!("Canon {}", opts.model)));
    ifd0.push(0x0111, Value::BlobOffset(preview_blob));
    ifd0.push(0x0112, Value::Shorts(vec![1]));
    ifd0.push(0x0117, Value::BlobLen(preview_blob));
    ifd0.push(0x0132, Value::Ascii("2024:05:06 07:08:09".into()));
    ifd0.push(0x8769, Value::IfdOffset(4));
    ifd0.next = Some(1);
    let mut ifd1 = Ifd::default();
    ifd1.push(0x0103, Value::Shorts(vec![6]));
    ifd1.push(0x0201, Value::BlobOffset(thumb_blob));
    ifd1.push(0x0202, Value::BlobLen(thumb_blob));
    ifd1.next = Some(2);
    let mut ifd2 = Ifd::default();
    rgb_strip_ifd(&mut b, &mut ifd2, w / 2, h / 2, h / 2, &small_pixels);
    ifd2.next = Some(3);
    let mut ifd3 = Ifd::default();
    ifd3.push(0x0103, Value::Shorts(vec![6]));
    ifd3.push(0x0111, Value::BlobOffset(raw_blob));
    ifd3.push(0x0117, Value::BlobLen(raw_blob));
    ifd3.push(0xC5D8, Value::Longs(vec![1]));
    ifd3.push(0xC640, Value::Shorts(vec![1, w as u16 / 2, w as u16 / 2]));
    let mut exif = Ifd::default();
    exif.push(0x9003, Value::Ascii("2024:05:06 07:08:09".into()));
    exif.push(0x9004, Value::Ascii("2024:05:06 07:08:09".into()));
    b.add_ifd(ifd0);
    b.add_ifd(ifd1);
    b.add_ifd(ifd2);
    b.add_ifd(ifd3);
    b.add_ifd(exif);
    let (mut bytes, layout) = resolve_strip_offsets(&mut b);
    let ifd3_off = layout.ifd_offsets[3].to_le_bytes();
    bytes[ifd3_offset_pos..ifd3_offset_pos + 4].copy_from_slice(&ifd3_off);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::util::bytes::{u16le, u32le};

    fn ifd_entries(b: &[u8], off: usize) -> Vec<(u16, u16, u32, u32)> {
        let n = u16le(b, off).unwrap() as usize;
        (0..n)
            .map(|i| {
                let e = off + 2 + i * 12;
                (
                    u16le(b, e).unwrap(),
                    u16le(b, e + 2).unwrap(),
                    u32le(b, e + 4).unwrap(),
                    u32le(b, e + 8).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn baseline_tiff_structure() {
        let t = tiff(&TiffOpts {
            width: 40,
            height: 50,
            rows_per_strip: 16,
            ..Default::default()
        });
        assert_eq!(&t[..4], b"II*\0");
        let ifd0 = u32le(&t, 4).unwrap() as usize;
        assert_eq!(ifd0, 8);
        let entries = ifd_entries(&t, ifd0);
        let tags: Vec<u16> = entries.iter().map(|e| e.0).collect();
        assert!(tags.windows(2).all(|w| w[0] < w[1]), "tags sorted");
        let strips = entries.iter().find(|e| e.0 == 0x0111).unwrap();
        assert_eq!(strips.2, 4, "ceil(50/16) strips");
        let counts = entries.iter().find(|e| e.0 == 0x0117).unwrap();
        let off_tab = strips.3 as usize;
        let cnt_tab = counts.3 as usize;
        let pixels = canvas::canvas_rgb(40, 50, 1);
        let mut pos = 0;
        for i in 0..4 {
            let o = u32le(&t, off_tab + i * 4).unwrap() as usize;
            let c = u32le(&t, cnt_tab + i * 4).unwrap() as usize;
            assert_eq!(&t[o..o + c], &pixels[pos..pos + c]);
            pos += c;
        }
        assert_eq!(pos, pixels.len());
        let be = tiff(&TiffOpts {
            big_endian: true,
            ..Default::default()
        });
        assert_eq!(&be[..4], b"MM\0*");
    }

    #[test]
    fn cr2_structure() {
        let c = cr2_like(&TiffOpts::default());
        assert_eq!(&c[..4], b"II*\0");
        assert_eq!(&c[8..12], b"CR\x02\x00");
        assert_eq!(u32le(&c, 4).unwrap(), 16);
        let ifd3 = u32le(&c, 12).unwrap() as usize;
        // Chain: IFD0 -> IFD1 -> IFD2 -> IFD3 -> 0
        let mut off = 16;
        let mut count = 0;
        while off != 0 && count < 10 {
            let n = u16le(&c, off).unwrap() as usize;
            let next = u32le(&c, off + 2 + n * 12).unwrap() as usize;
            count += 1;
            if count == 4 {
                assert_eq!(off, ifd3);
            }
            off = next;
        }
        assert_eq!(count, 4);
        // IFD0's strip is a JPEG.
        let e = ifd_entries(&c, 16);
        let so = e.iter().find(|e| e.0 == 0x0111).unwrap().3 as usize;
        assert_eq!(&c[so..so + 2], &[0xFF, 0xD8]);
        let raw = ifd_entries(&c, ifd3);
        let ro = raw.iter().find(|e| e.0 == 0x0111).unwrap().3 as usize;
        assert_eq!(&c[ro..ro + 4], &[0xFF, 0xD8, 0xFF, 0xC3]);
    }
}
