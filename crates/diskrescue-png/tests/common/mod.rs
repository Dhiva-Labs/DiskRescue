//! Test support: a deterministic PNG generator (independent of the crate
//! under test), chunk helpers, a counting source and a tiny PRNG.
#![allow(dead_code)]

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use diskrescue_core::source::{ReadError, SharedSource, Source};

pub const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// xorshift64* PRNG.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = (self.next_u64() >> 32) as u8;
        }
    }

    /// Between 0 and `max - 1` random bytes.
    pub fn bytes_upto(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max) as usize;
        self.bytes(n)
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    pub color_type: u8,
    pub bit_depth: u8,
    pub interlaced: bool,
    /// IDAT payload size (0 = a single chunk).
    pub idat_chunk: usize,
    /// `None` cycles filters 0..=4 by row; `Some(n)` uses filter n.
    pub filter: Option<u8>,
    pub seed: u64,
    pub text: bool,
    /// Extra ancillary chunks (gAMA, pHYs) after IHDR.
    pub ancillary: bool,
    /// Add a trailing tEXt chunk after the IDATs.
    pub trailing_text: bool,
}

impl Spec {
    pub fn new(width: u32, height: u32, color_type: u8, bit_depth: u8) -> Self {
        Spec {
            width,
            height,
            color_type,
            bit_depth,
            interlaced: false,
            idat_chunk: 4096,
            filter: None,
            seed: 7,
            text: true,
            ancillary: false,
            trailing_text: false,
        }
    }

    pub fn interlaced(mut self) -> Self {
        self.interlaced = true;
        self
    }

    pub fn chunk(mut self, n: usize) -> Self {
        self.idat_chunk = n;
        self
    }

    pub fn seed(mut self, s: u64) -> Self {
        self.seed = s;
        self
    }

    pub fn channels(&self) -> usize {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            6 => 4,
            _ => panic!("bad colour type"),
        }
    }

    pub fn bits_pp(&self) -> usize {
        self.channels() * self.bit_depth as usize
    }

    pub fn row_bytes(&self) -> usize {
        (self.width as usize * self.bits_pp()).div_ceil(8)
    }

    pub fn bpp(&self) -> usize {
        (self.bits_pp() / 8).max(1)
    }

    pub fn palette(&self) -> Vec<u8> {
        let n = 1usize << self.bit_depth;
        (0..n)
            .flat_map(|i| {
                [
                    (i * 37 % 256) as u8,
                    (i * 91 % 256) as u8,
                    (255 - i * 13 % 256) as u8,
                ]
            })
            .collect()
    }
}

pub struct Built {
    pub png: Vec<u8>,
    /// Unfiltered rows, `height * row_bytes`, non-interlaced layout.
    pub raw: Vec<u8>,
    pub spec: Spec,
}

impl Built {
    pub fn row(&self, y: usize) -> &[u8] {
        let rb = self.spec.row_bytes();
        &self.raw[y * rb..(y + 1) * rb]
    }
}

/// Gradient plus noise, with padding bits cleared.
pub fn gen_raw(spec: &Spec) -> Vec<u8> {
    let mut rng = Rng::new(spec.seed);
    let rb = spec.row_bytes();
    let mut raw = vec![0u8; rb * spec.height as usize];
    for y in 0..spec.height as usize {
        let row = &mut raw[y * rb..(y + 1) * rb];
        if spec.bit_depth >= 8 {
            for (i, b) in row.iter_mut().enumerate() {
                let grad = (i * 3 + y * 5) as u8;
                *b = grad.wrapping_add((rng.next_u64() >> 40) as u8 & 0x3F);
            }
        } else {
            rng.fill(row);
            // Clear padding bits at the end of the row.
            let used = spec.width as usize * spec.bits_pp();
            let pad = rb * 8 - used;
            if pad > 0 {
                let last = row.len() - 1;
                row[last] &= 0xFFu8 << pad;
            }
        }
    }
    raw
}

fn filter_row(ftype: u8, raw: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
    let paeth = |a: u8, b: u8, c: u8| -> u8 {
        let (ai, bi, ci) = (a as i32, b as i32, c as i32);
        let p = ai + bi - ci;
        let (pa, pb, pc) = ((p - ai).abs(), (p - bi).abs(), (p - ci).abs());
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    };
    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(ftype);
    for i in 0..raw.len() {
        let a = if i >= bpp { raw[i - bpp] } else { 0 };
        let b = prev[i];
        let c = if i >= bpp { prev[i - bpp] } else { 0 };
        let pred = match ftype {
            0 => 0,
            1 => a,
            2 => b,
            3 => ((a as u16 + b as u16) / 2) as u8,
            4 => paeth(a, b, c),
            _ => panic!(),
        };
        out.push(raw[i].wrapping_sub(pred));
    }
    out
}

/// Extract pixel `x` of a packed row as (bits) in the low bits of a u64 (only
/// for depths <= 8 per sample combined into whole bytes where needed).
fn get_px(row: &[u8], x: usize, bits_pp: usize) -> Vec<u8> {
    if bits_pp >= 8 {
        let n = bits_pp / 8;
        row[x * n..x * n + n].to_vec()
    } else {
        let bit = x * bits_pp;
        let shift = 8 - bits_pp - (bit % 8);
        vec![(row[bit / 8] >> shift) & ((1u16 << bits_pp) - 1) as u8]
    }
}

fn put_px(row: &mut [u8], x: usize, bits_pp: usize, v: &[u8]) {
    if bits_pp >= 8 {
        let n = bits_pp / 8;
        row[x * n..x * n + n].copy_from_slice(v);
    } else {
        let bit = x * bits_pp;
        let shift = 8 - bits_pp - (bit % 8);
        let mask = (((1u16 << bits_pp) - 1) as u8) << shift;
        row[bit / 8] = (row[bit / 8] & !mask) | ((v[0] << shift) & mask);
    }
}

/// Filtered, serialised scanlines (all passes for interlaced images).
pub fn filtered_stream(spec: &Spec, raw: &[u8]) -> Vec<u8> {
    let rb = spec.row_bytes();
    let (w, h) = (spec.width as usize, spec.height as usize);
    let bpp = spec.bpp();
    let bits = spec.bits_pp();
    let pick = |n: usize| spec.filter.unwrap_or((n % 5) as u8);
    let mut out = Vec::new();
    if !spec.interlaced {
        let mut prev = vec![0u8; rb];
        for y in 0..h {
            let row = &raw[y * rb..(y + 1) * rb];
            out.extend(filter_row(pick(y), row, &prev, bpp));
            prev = row.to_vec();
        }
    } else {
        for &(x0, y0, dx, dy) in &ADAM7 {
            if x0 >= w || y0 >= h {
                continue;
            }
            let pw = (w - x0).div_ceil(dx);
            let prb = (pw * bits).div_ceil(8);
            let mut prev = vec![0u8; prb];
            let mut n = 0;
            let mut y = y0;
            while y < h {
                let mut prow = vec![0u8; prb];
                for i in 0..pw {
                    let px = get_px(&raw[y * rb..(y + 1) * rb], x0 + i * dx, bits);
                    put_px(&mut prow, i, bits, &px);
                }
                out.extend(filter_row(pick(n), &prow, &prev, bpp));
                prev = prow;
                n += 1;
                y += dy;
            }
        }
    }
    out
}

pub fn crc(kind: &[u8; 4], data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(kind);
    h.update(data);
    h.finalize()
}

pub fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + data.len());
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(data);
    v.extend_from_slice(&crc(kind, data).to_be_bytes());
    v
}

pub fn ihdr_bytes(spec: &Spec) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend_from_slice(&spec.width.to_be_bytes());
    d.extend_from_slice(&spec.height.to_be_bytes());
    d.extend_from_slice(&[spec.bit_depth, spec.color_type, 0, 0, spec.interlaced as u8]);
    d
}

pub fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    z.write_all(data).unwrap();
    z.finish().unwrap()
}

pub fn assemble(spec: &Spec, z: &[u8]) -> Vec<u8> {
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(spec)));
    if spec.text {
        png.extend(chunk(b"tEXt", b"Software\0diskrescue-png tests"));
    }
    if spec.ancillary {
        png.extend(chunk(b"gAMA", &45455u32.to_be_bytes()));
        let mut phys = Vec::new();
        phys.extend_from_slice(&2835u32.to_be_bytes());
        phys.extend_from_slice(&2835u32.to_be_bytes());
        phys.push(1);
        png.extend(chunk(b"pHYs", &phys));
    }
    if spec.color_type == 3 {
        png.extend(chunk(b"PLTE", &spec.palette()));
    }
    let piece = if spec.idat_chunk == 0 {
        z.len().max(1)
    } else {
        spec.idat_chunk
    };
    for part in z.chunks(piece) {
        png.extend(chunk(b"IDAT", part));
    }
    if spec.trailing_text {
        png.extend(chunk(b"tEXt", b"Comment\0after the image"));
    }
    png.extend(chunk(b"IEND", &[]));
    png
}

pub fn build(spec: &Spec) -> Built {
    let raw = gen_raw(spec);
    let z = zlib(&filtered_stream(spec, &raw), 6);
    Built {
        png: assemble(spec, &z),
        raw,
        spec: spec.clone(),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct C {
    pub kind: [u8; 4],
    pub offset: usize,
    pub len: usize,
}

impl C {
    pub fn data(&self) -> std::ops::Range<usize> {
        self.offset + 8..self.offset + 8 + self.len
    }

    pub fn crc_at(&self) -> usize {
        self.offset + 8 + self.len
    }

    pub fn end(&self) -> usize {
        self.offset + 12 + self.len
    }
}

/// List chunks of a well-formed PNG.
pub fn chunks_of(png: &[u8]) -> Vec<C> {
    let mut v = Vec::new();
    let mut pos = 8;
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = png[pos + 4..pos + 8].try_into().unwrap();
        if pos + 12 + len > png.len() {
            break;
        }
        v.push(C {
            kind,
            offset: pos,
            len,
        });
        pos += 12 + len;
    }
    v
}

pub fn find(png: &[u8], kind: &[u8; 4], nth: usize) -> C {
    chunks_of(png)
        .into_iter()
        .filter(|c| &c.kind == kind)
        .nth(nth)
        .unwrap_or_else(|| panic!("no {} chunk #{nth}", String::from_utf8_lossy(kind)))
}

/// Rewrite a chunk's CRC so it is valid again after editing its data.
pub fn fix_crc_at(png: &mut [u8], c: &C) {
    let v = crc(&c.kind, &png[c.data()]);
    png[c.crc_at()..c.crc_at() + 4].copy_from_slice(&v.to_be_bytes());
}

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub color: png::ColorType,
    pub depth: png::BitDepth,
    pub data: Vec<u8>,
}

/// Decode with the png crate (no transformations: palette indices and packed
/// samples stay as stored).
pub fn png_decode(bytes: &[u8]) -> Result<Decoded, png::DecodingError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    Ok(Decoded {
        width: info.width,
        height: info.height,
        color: info.color_type,
        depth: info.bit_depth,
        data: buf,
    })
}

/// Like [`png_decode`] but ignoring malformed text and ICC chunks, which the
/// validator does not judge.
pub fn png_decode_pixels(bytes: &[u8]) -> Result<Decoded, png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    Ok(Decoded {
        width: info.width,
        height: info.height,
        color: info.color_type,
        depth: info.bit_depth,
        data: buf,
    })
}

pub fn mem(bytes: Vec<u8>) -> SharedSource {
    Arc::new(diskrescue_core::source::MemSource::new(bytes))
}

/// A source that records how it is read.
pub struct Counting {
    inner: SharedSource,
    pub max_read: AtomicUsize,
    pub total: AtomicU64,
    pub calls: AtomicU64,
}

impl Counting {
    pub fn new(inner: SharedSource) -> Self {
        Counting {
            inner,
            max_read: AtomicUsize::new(0),
            total: AtomicU64::new(0),
            calls: AtomicU64::new(0),
        }
    }
}

impl Source for Counting {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.max_read.fetch_max(buf.len(), Ordering::Relaxed);
        self.calls.fetch_add(1, Ordering::Relaxed);
        let n = self.inner.read_at(offset, buf)?;
        self.total.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn describe(&self) -> String {
        "counting".into()
    }
}

/// All combinations of colour type and bit depth the specification allows.
pub fn all_modes() -> Vec<(u8, u8)> {
    vec![
        (0, 1),
        (0, 2),
        (0, 4),
        (0, 8),
        (0, 16),
        (2, 8),
        (2, 16),
        (3, 1),
        (3, 2),
        (3, 4),
        (3, 8),
        (4, 8),
        (4, 16),
        (6, 8),
        (6, 16),
    ]
}

/// Row `y` of the decoded png crate buffer.
pub fn dec_row(d: &Decoded, y: usize) -> &[u8] {
    let rb = d.data.len() / d.height as usize;
    &d.data[y * rb..(y + 1) * rb]
}

/// True when `fill_ok(row)` for a row that should be the repair filler.
pub fn is_uniform_grey(row: &[u8]) -> bool {
    row.iter().all(|&b| b == 0x80 || b == 0x00)
}
