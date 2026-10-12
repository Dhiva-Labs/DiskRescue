//! Shared helpers for the integration tests: sources, a deterministic RNG,
//! the Pillow fixture generator and a small writer for hand-built TIFF-family
//! files (the byte-level layouts are spelled out in the test files).

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use diskrescue_core::format::{ExtentStatus, FileFormat, Measured, ValidationStatus};
use diskrescue_core::source::{MemSource, SharedSource};

pub mod raws;

pub fn mem(bytes: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(bytes))
}

/// Measure a whole buffer from offset 0 with no limit; panics unless the
/// format recognises it.
pub fn measure(fmt: &dyn FileFormat, bytes: &[u8]) -> Measured {
    fmt.measure(&mem(bytes.to_vec()), 0, u64::MAX)
        .expect("measure returned an error")
        .unwrap_or_else(|| panic!("{} did not recognise the file", fmt.id()))
}

/// Deterministic xorshift generator.
#[derive(Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 24) as u8).collect()
    }
}

/// Lay `file` into a larger buffer with junk before and after.
/// Returns `(source, start_offset)`.
pub fn embed(file: &[u8], before: usize, after: usize, rng: &mut Rng) -> (SharedSource, u64) {
    let mut v = rng.bytes(before);
    v.extend_from_slice(file);
    v.extend(rng.bytes(after));
    (mem(v), before as u64)
}

// ---------------------------------------------------------------------------
// Pillow fixtures
// ---------------------------------------------------------------------------

static FIXTURES: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Directory with the Pillow-generated files, or `None` (after printing a
/// skip notice) when python3 or Pillow is missing.
pub fn pillow_dir() -> Option<&'static Path> {
    FIXTURES
        .get_or_init(|| {
            let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("stills-pillow");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = include_str!("../fixtures/gen_images.py");
            let mut child = match Command::new("python3")
                .arg("-")
                .arg(&dir)
                .stdin(Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => {
                    eprintln!("skipped: python3 missing");
                    return None;
                }
            };
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let st = child.wait().unwrap();
            match st.code() {
                Some(0) => Some(dir),
                Some(3) => {
                    eprintln!("skipped: python3 Pillow missing");
                    None
                }
                other => panic!("fixture generator failed: {other:?}"),
            }
        })
        .as_deref()
}

/// Bytes of one Pillow-generated fixture, or `None` to skip.
pub fn pillow_file(name: &str) -> Option<Vec<u8>> {
    let dir = pillow_dir()?;
    Some(std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}")))
}

#[macro_export]
macro_rules! need_pillow {
    ($name:expr) => {
        match $crate::common::pillow_file($name) {
            Some(b) => b,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------
// Synthetic JPEGs
// ---------------------------------------------------------------------------

/// A structurally plausible baseline JPEG of exactly `total` bytes (SOI,
/// JFIF, SOF0, SOS, filler without `FF`, EOI). Not decodable; enough for the
/// header walkers.
pub fn fake_jpeg(w: u16, h: u16, total: usize, rng: &mut Rng) -> Vec<u8> {
    let mut j = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ];
    j.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8]);
    j.extend_from_slice(&h.to_be_bytes());
    j.extend_from_slice(&w.to_be_bytes());
    j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    j.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]);
    assert!(total >= j.len() + 2, "fake_jpeg too small");
    while j.len() < total - 2 {
        j.push((rng.next() >> 20) as u8 & 0x7F);
    }
    j.extend_from_slice(&[0xFF, 0xD9]);
    j
}

/// Like [`fake_jpeg`] but with an Exif APP1 holding `exif` (a TIFF blob).
pub fn fake_jpeg_with_exif(w: u16, h: u16, total: usize, exif: &[u8], rng: &mut Rng) -> Vec<u8> {
    let base = fake_jpeg(w, h, total - exif.len() - 10, rng);
    let mut j = base[..2].to_vec();
    j.extend_from_slice(&[0xFF, 0xE1]);
    j.extend_from_slice(&((exif.len() + 8) as u16).to_be_bytes());
    j.extend_from_slice(b"Exif\0\0");
    j.extend_from_slice(exif);
    j.extend_from_slice(&base[2..]);
    assert_eq!(j.len(), total);
    j
}

/// A lossless-JPEG wrapper as used for CR2 / DNG sensor data (SOF3).
pub fn fake_lossless_jpeg(total: usize, rng: &mut Rng) -> Vec<u8> {
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xC4, 0, 19, 0];
    j.extend_from_slice(&[0; 16]);
    j.extend_from_slice(&[
        0xFF, 0xC3, 0, 17, 14, 0x0F, 0xA0, 0x17, 0x70, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0,
    ]);
    j.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0, 2, 0, 3, 0, 1, 0, 0]);
    while j.len() < total {
        j.push((rng.next() >> 20) as u8 & 0x7F);
    }
    j
}

// ---------------------------------------------------------------------------
// TIFF writer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum V {
    Byte(Vec<u8>),
    Ascii(String),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Undef(Vec<u8>),
    Rational(Vec<(u32, u32)>),
    Ifd(Vec<u32>),
    /// An out-of-line value whose data the caller places elsewhere: the entry
    /// carries `count` values of type `typ` and the offset `off`.
    Ptr {
        typ: u16,
        count: u32,
        off: u32,
    },
}

impl V {
    fn typ(&self) -> u16 {
        match self {
            V::Byte(_) => 1,
            V::Ascii(_) => 2,
            V::Short(_) => 3,
            V::Long(_) => 4,
            V::Rational(_) => 5,
            V::Undef(_) => 7,
            V::Ifd(_) => 13,
            V::Ptr { typ, .. } => *typ,
        }
    }

    fn count(&self) -> u32 {
        match self {
            V::Byte(v) | V::Undef(v) => v.len() as u32,
            V::Ascii(s) => s.len() as u32 + 1,
            V::Short(v) => v.len() as u32,
            V::Long(v) | V::Ifd(v) => v.len() as u32,
            V::Rational(v) => v.len() as u32,
            V::Ptr { count, .. } => *count,
        }
    }

    fn bytes(&self, be: bool) -> Vec<u8> {
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        match self {
            V::Byte(v) | V::Undef(v) => v.clone(),
            V::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                b
            }
            V::Short(v) => v.iter().flat_map(|x| u16b(*x)).collect(),
            V::Long(v) | V::Ifd(v) => v.iter().flat_map(|x| u32b(*x)).collect(),
            V::Rational(v) => v
                .iter()
                .flat_map(|(a, b)| u32b(*a).into_iter().chain(u32b(*b)))
                .collect(),
            V::Ptr { off, .. } => {
                let mut b = u32b(*off).to_vec();
                b.resize(8, 0); // forces the out-of-line form
                b
            }
        }
    }

    /// Bytes this value adds after the IFD (0 for inline values and pointers).
    fn overflow(&self) -> usize {
        match self {
            V::Ptr { .. } => 0,
            v => {
                let n = v.bytes(false).len();
                if n > 4 {
                    n + (n & 1)
                } else {
                    0
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct E {
    pub tag: u16,
    pub v: V,
}

pub fn e(tag: u16, v: V) -> E {
    E { tag, v }
}

pub fn short(tag: u16, v: u16) -> E {
    e(tag, V::Short(vec![v]))
}

pub fn long(tag: u16, v: u32) -> E {
    e(tag, V::Long(vec![v]))
}

pub fn ascii(tag: u16, s: &str) -> E {
    e(tag, V::Ascii(s.to_string()))
}

/// One laid-out item of a TIFF-family file.
pub enum Item {
    Ifd { entries: Vec<E>, next: u32 },
    Blob(Vec<u8>),
}

impl Item {
    pub fn ifd(entries: Vec<E>, next: u32) -> Item {
        Item::Ifd { entries, next }
    }

    fn len(&self) -> usize {
        match self {
            Item::Blob(b) => b.len(),
            Item::Ifd { entries, .. } => {
                2 + 12 * entries.len() + 4 + entries.iter().map(|e| e.v.overflow()).sum::<usize>()
            }
        }
    }
}

fn put(out: &mut Vec<u8>, at: usize, bytes: &[u8]) {
    if out.len() < at + bytes.len() {
        out.resize(at + bytes.len(), 0);
    }
    out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn write_ifd(out: &mut Vec<u8>, be: bool, at: usize, entries: &[E], next: u32) {
    let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
    let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
    let mut sorted: Vec<&E> = entries.iter().collect();
    sorted.sort_by_key(|e| e.tag);
    let n = sorted.len();
    put(out, at, &u16b(n as u16));
    let mut overflow = at + 2 + 12 * n + 4;
    for (i, e) in sorted.iter().enumerate() {
        let p = at + 2 + 12 * i;
        let data = e.v.bytes(be);
        put(out, p, &u16b(e.tag));
        put(out, p + 2, &u16b(e.v.typ()));
        put(out, p + 4, &u32b(e.v.count()));
        if let V::Ptr { off, .. } = &e.v {
            put(out, p + 8, &u32b(*off));
        } else if data.len() <= 4 {
            let mut field = [0u8; 4];
            field[..data.len()].copy_from_slice(&data);
            put(out, p + 8, &field);
        } else {
            put(out, p + 8, &u32b(overflow as u32));
            put(out, overflow, &data);
            overflow += data.len() + (data.len() & 1);
        }
    }
    put(out, at + 2 + 12 * n, &u32b(next));
}

/// Build a TIFF-family file. `f` receives the byte offset of every item (in
/// the order it returns them) and returns the header bytes plus the items.
/// It is called twice, once with zero offsets to learn the sizes.
pub fn build(be: bool, f: impl Fn(&[usize]) -> (Vec<u8>, Vec<Item>)) -> (Vec<u8>, Vec<usize>) {
    let zeros = vec![0usize; 64];
    let (h0, items0) = f(&zeros);
    let mut offs = Vec::new();
    let mut pos = h0.len();
    for it in &items0 {
        if pos % 2 == 1 {
            pos += 1;
        }
        offs.push(pos);
        pos += it.len();
    }
    let mut padded = offs.clone();
    padded.resize(64, 0);
    let (h, items) = f(&padded);
    assert_eq!(h.len(), h0.len());
    let mut out = h;
    for (it, &off) in items.iter().zip(&offs) {
        match it {
            Item::Blob(b) => put(&mut out, off, b),
            Item::Ifd { entries, next } => write_ifd(&mut out, be, off, entries, *next),
        }
    }
    (out, offs)
}

/// Where `write_ifd` will put the out-of-line data of `tag` for an IFD at
/// `ifd_off` (so a value can contain offsets relative to itself).
pub fn value_at(ifd_off: usize, entries: &[E], tag: u16) -> usize {
    let mut sorted: Vec<&E> = entries.iter().collect();
    sorted.sort_by_key(|e| e.tag);
    let mut cursor = ifd_off + 2 + 12 * sorted.len() + 4;
    for e in sorted {
        let o = e.v.overflow();
        if o > 0 {
            if e.tag == tag {
                return cursor;
            }
            cursor += o;
        }
    }
    panic!("tag {tag:#x} has no out-of-line value");
}

pub fn ptr(tag: u16, typ: u16, count: usize, off: usize) -> E {
    e(
        tag,
        V::Ptr {
            typ,
            count: count as u32,
            off: off as u32,
        },
    )
}

pub fn tiff_header(be: bool, ifd0: u32) -> Vec<u8> {
    let mut h = if be {
        b"MM\0*".to_vec()
    } else {
        b"II*\0".to_vec()
    };
    h.extend_from_slice(&if be {
        ifd0.to_be_bytes()
    } else {
        ifd0.to_le_bytes()
    });
    h
}

/// A small Exif TIFF blob (as inside a JPEG APP1 after `Exif\0\0`).
pub fn exif_tiff(be: bool, make: &str, model: &str, orient: u16, dt: &str) -> Vec<u8> {
    build(be, |o| {
        (
            tiff_header(be, o[0] as u32),
            vec![
                Item::ifd(
                    vec![
                        ascii(0x010F, make),
                        ascii(0x0110, model),
                        short(0x0112, orient),
                        ascii(0x0132, dt),
                        long(0x8769, o[1] as u32),
                    ],
                    0,
                ),
                Item::ifd(
                    vec![ascii(0x9003, dt), long(0xA002, 6000), long(0xA003, 4000)],
                    0,
                ),
            ],
        )
    })
    .0
}

pub fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

pub fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

// ---------------------------------------------------------------------------
// Shared assertions
// ---------------------------------------------------------------------------

/// The properties every complete file must have: exact length with junk
/// after it and at non-zero start offsets, and a clean validation.
pub fn check_exact(fmt: &dyn FileFormat, file: &[u8]) -> Measured {
    let m = measure(fmt, file);
    assert_eq!(
        m.status,
        ExtentStatus::Complete,
        "{}: {:?}",
        fmt.id(),
        m.notes
    );
    assert_eq!(m.len, file.len() as u64, "{} length", fmt.id());
    assert_eq!(m.err_offset, None);
    assert!(
        m.confidence.value() >= 50,
        "{}: {:?}",
        fmt.id(),
        m.confidence
    );

    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for (before, after) in [(0usize, 700usize), (4096, 0), (513, 100_000), (7, 3)] {
        let (src, start) = embed(file, before, after, &mut rng);
        let mm = fmt.measure(&src, start, u64::MAX).unwrap().unwrap();
        assert_eq!(mm.len, file.len() as u64, "{} at {start}+{after}", fmt.id());
        assert_eq!(mm.status, ExtentStatus::Complete);
        assert_eq!(
            mm.meta, m.meta,
            "metadata must not depend on the start offset"
        );
        assert_eq!(mm.nested, m.nested);
    }
    let v = fmt.validate(&mem(file.to_vec())).unwrap();
    assert_eq!(
        v.status,
        ValidationStatus::Valid,
        "{}: {:?}",
        fmt.id(),
        v.problems
    );
    assert_eq!(v.intact_fraction, Some(1.0));
    assert!(!fmt.can_repair());
    assert!(
        fmt.probe(&file[..file.len().min(65_536)]),
        "{} probe",
        fmt.id()
    );
    for n in &m.nested {
        assert!(
            n.range.end <= m.len && n.range.start < n.range.end,
            "{:?}",
            n
        );
    }
    m
}

/// Cutting the file anywhere must say Truncated with the cut as
/// `err_offset` (or not recognise the stub at all).
pub fn check_truncation(fmt: &dyn FileFormat, file: &[u8], cuts: &[usize]) {
    for &cut in cuts {
        let part = &file[..cut];
        let Some(m) = fmt.measure(&mem(part.to_vec()), 0, u64::MAX).unwrap() else {
            continue;
        };
        assert_eq!(
            m.status,
            ExtentStatus::Truncated,
            "{} cut {cut}: {m:?}",
            fmt.id()
        );
        assert_eq!(m.err_offset, Some(cut as u64), "{} cut {cut}", fmt.id());
        assert_eq!(m.len, cut as u64, "{} cut {cut}", fmt.id());
        let v = fmt.validate(&mem(part.to_vec())).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Damaged,
            "{} cut {cut}",
            fmt.id()
        );
    }
}
