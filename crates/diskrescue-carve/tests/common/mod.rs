//! Fake formats and image builders shared by the integration tests.
//!
//! None of these are real media formats; they exercise the carver's
//! contract with `FileFormat` (signatures, probe, measure, nesting).

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use diskrescue_carve::{CarveOptions, CarvedFile, Carver, ScanSummary};
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, FormatRegistry, Measured, Nested, NestedRole, Signature,
    Validation,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::util::bytes::{u32be, u32le};
use diskrescue_core::{Range, Result, SharedSource, SourceExt};

/// Small deterministic generator (xorshift64*).
pub struct Rng(u64);

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
        let mut chunks = buf.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rest = chunks.into_remainder();
        let v = self.next_u64().to_le_bytes();
        let n = rest.len();
        rest.copy_from_slice(&v[..n]);
    }
}

pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; len];
    Rng::new(seed).fill(&mut v);
    v
}

pub fn put(img: &mut [u8], off: usize, bytes: &[u8]) {
    img[off..off + bytes.len()].copy_from_slice(bytes);
}

pub fn leak(v: Vec<Signature>) -> &'static [Signature] {
    Box::leak(v.into_boxed_slice())
}

/// A format with a 4-byte magic, a u32 LE total length at +4 and a 4-byte
/// trailer at the end. Optional nested table: u32 LE count at +8, then
/// `count` x (u32 LE offset, u32 LE length) entries.
pub struct LenFmt {
    pub id: &'static str,
    pub sigs: &'static [Signature],
    pub trailer: &'static [u8],
    /// A missing trailer gives `Truncated` instead of rejecting.
    pub trailer_optional: bool,
    pub priority: u8,
    pub category: MediaCategory,
    pub conf: u8,
    pub min: u64,
    pub max: u64,
    pub contains: &'static [&'static str],
    pub nested_format: Option<&'static str>,
    pub measures: AtomicUsize,
}

impl LenFmt {
    /// The "TEST" photo format: `TST1`, length, payload, `TEND`.
    pub fn test() -> Self {
        LenFmt {
            id: "test",
            sigs: leak(vec![Signature::at_start(b"TST1")]),
            trailer: b"TEND",
            trailer_optional: false,
            priority: 50,
            category: MediaCategory::Photo,
            conf: 90,
            min: 12,
            max: 64 << 20,
            contains: &[],
            nested_format: None,
            measures: AtomicUsize::new(0),
        }
    }

    /// The "CONT" container: `CNT1`, length, nested table of `test`
    /// objects, `CEND` (optional: missing means truncated).
    pub fn container() -> Self {
        LenFmt {
            id: "cont",
            sigs: leak(vec![Signature::at_start(b"CNT1")]),
            trailer: b"CEND",
            trailer_optional: true,
            priority: 70,
            category: MediaCategory::Video,
            conf: 90,
            min: 16,
            max: 64 << 20,
            contains: &["test"],
            nested_format: Some("test"),
            measures: AtomicUsize::new(0),
        }
    }
}

impl FileFormat for LenFmt {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.id
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["tst", "test"]
    }
    fn category(&self) -> MediaCategory {
        self.category
    }
    fn signatures(&self) -> &'static [Signature] {
        self.sigs
    }
    fn min_size(&self) -> u64 {
        self.min
    }
    fn max_size(&self) -> u64 {
        self.max
    }
    fn priority(&self) -> u8 {
        self.priority
    }
    fn may_contain(&self, inner: &str) -> bool {
        self.contains.contains(&inner)
    }
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        self.measures.fetch_add(1, Ordering::Relaxed);
        let head = src.read_vec(start, limit.min(4096) as usize)?;
        if !self.sigs.iter().any(|s| s.matches(&head)) {
            return Ok(None);
        }
        let Some(len) = u32le(&head, 4).map(u64::from) else {
            return Ok(None);
        };
        if len < self.min || len > self.max {
            return Ok(None);
        }
        let mut nested = Vec::new();
        if let Some(fmt) = self.nested_format {
            let Some(count) = u32le(&head, 8) else {
                return Ok(None);
            };
            if count > 64 {
                return Ok(None);
            }
            for i in 0..count as usize {
                let (Some(off), Some(l)) = (u32le(&head, 12 + 8 * i), u32le(&head, 16 + 8 * i))
                else {
                    return Ok(None);
                };
                let r = Range::with_len(u64::from(off), u64::from(l));
                if r.end > len {
                    return Ok(None);
                }
                nested.push(Nested {
                    range: r,
                    format: Some(fmt.to_string()),
                    role: NestedRole::Thumbnail,
                });
            }
        }
        if len > limit {
            let mut m = Measured::truncated(
                limit,
                Confidence(self.conf).lowered(30),
                limit,
                "declared length runs past the read limit",
            );
            m.nested = nested
                .into_iter()
                .filter(|n| n.range.end <= limit)
                .collect();
            return Ok(Some(m));
        }
        let tl = self.trailer.len() as u64;
        let tail = src.read_vec(start + len - tl, tl as usize)?;
        let mut m = if tail == self.trailer {
            Measured::complete(len, Confidence(self.conf))
        } else if self.trailer_optional {
            Measured::truncated(
                len,
                Confidence(self.conf).lowered(50),
                len - tl,
                "trailer missing",
            )
        } else {
            return Ok(None);
        };
        m.nested = nested;
        Ok(Some(m))
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// A run-length format without an end marker: 512-byte packets that each
/// start with `SPKT`. Length is the packet run; status `UnknownLength`.
#[derive(Default)]
pub struct StreamFmt {
    pub measures: AtomicUsize,
}

static STRM_SIGS: &[Signature] = &[Signature::at_start(b"SPKT")];

impl FileFormat for StreamFmt {
    fn id(&self) -> &'static str {
        "strm"
    }
    fn name(&self) -> &'static str {
        "packet stream"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["strm"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }
    fn signatures(&self) -> &'static [Signature] {
        STRM_SIGS
    }
    fn min_size(&self) -> u64 {
        1024
    }
    fn max_size(&self) -> u64 {
        64 << 20
    }
    fn priority(&self) -> u8 {
        20
    }
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        self.measures.fetch_add(1, Ordering::Relaxed);
        let mut n = 0u64;
        while (n + 1) * 512 <= limit {
            let b = src.read_vec(start + n * 512, 4)?;
            if b != b"SPKT" {
                break;
            }
            n += 1;
        }
        if n < 2 {
            return Ok(None);
        }
        let mut m = Measured::complete(n * 512, Confidence(40));
        m.status = ExtentStatus::UnknownLength;
        Ok(Some(m))
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// A greedy format: anything after `RAWS` up to 1 MiB, unknown length.
pub struct GreedyFmt;

static RAWS_SIGS: &[Signature] = &[Signature::at_start(b"RAWS")];

impl FileFormat for GreedyFmt {
    fn id(&self) -> &'static str {
        "raws"
    }
    fn name(&self) -> &'static str {
        "raw stream"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["raw"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Audio
    }
    fn signatures(&self) -> &'static [Signature] {
        RAWS_SIGS
    }
    fn min_size(&self) -> u64 {
        16
    }
    fn max_size(&self) -> u64 {
        1 << 20
    }
    fn priority(&self) -> u8 {
        10
    }
    fn measure(&self, _: &SharedSource, _: u64, limit: u64) -> Result<Option<Measured>> {
        let mut m = Measured::complete(limit.min(1 << 20), Confidence(30));
        m.status = ExtentStatus::UnknownLength;
        Ok(Some(m))
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// An ISO-BMFF-like format: u32 BE total size at 0, `zfmt` at 4. Its files
/// start with zero bytes, like `00 00 00 18 ftyp`.
pub struct ZeroLeadFmt;

static ZFMT_SIGS: &[Signature] = &[Signature::at(b"zfmt", 4)];

impl FileFormat for ZeroLeadFmt {
    fn id(&self) -> &'static str {
        "zfmt"
    }
    fn name(&self) -> &'static str {
        "zero-led box"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["zf"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }
    fn signatures(&self) -> &'static [Signature] {
        ZFMT_SIGS
    }
    fn min_size(&self) -> u64 {
        16
    }
    fn max_size(&self) -> u64 {
        16 << 20
    }
    fn priority(&self) -> u8 {
        60
    }
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let h = src.read_vec(start, 8)?;
        let Some(size) = u32be(&h, 0).map(u64::from) else {
            return Ok(None);
        };
        if size < 16 || size > limit || h.get(4..8) != Some(&b"zfmt"[..]) {
            return Ok(None);
        }
        Ok(Some(Measured::complete(size, Confidence(90))))
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// Misbehaving formats: `BOOM` panics in measure, `PRBM` panics in probe,
/// `LIAR` claims a length far past the read limit, `NULL` claims length 0.
pub struct Hostile(pub &'static str, pub &'static [Signature]);

pub static BOOM_SIGS: &[Signature] = &[Signature::at_start(b"BOOM")];
pub static PRBM_SIGS: &[Signature] = &[Signature::at_start(b"PRBM")];
pub static LIAR_SIGS: &[Signature] = &[Signature::at_start(b"LIAR")];
pub static NULL_SIGS: &[Signature] = &[Signature::at_start(b"NULL")];

impl FileFormat for Hostile {
    fn id(&self) -> &'static str {
        self.0
    }
    fn name(&self) -> &'static str {
        self.0
    }
    fn extensions(&self) -> &'static [&'static str] {
        &[]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Other
    }
    fn signatures(&self) -> &'static [Signature] {
        self.1
    }
    fn min_size(&self) -> u64 {
        1
    }
    fn max_size(&self) -> u64 {
        1 << 20
    }
    fn probe(&self, head: &[u8]) -> bool {
        if self.0 == "prbm" {
            panic!("probe exploded");
        }
        self.1.iter().any(|s| s.matches(head))
    }
    fn measure(&self, _: &SharedSource, _: u64, limit: u64) -> Result<Option<Measured>> {
        match self.0 {
            "boom" => panic!("measure exploded"),
            "liar" => Ok(Some(Measured::complete(u64::MAX, Confidence(95)))),
            "null" => Ok(Some(Measured::complete(0, Confidence(95)))),
            _ => Ok(Some(Measured::complete(limit, Confidence(10)))),
        }
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// A format that never measures anything (scanner-only benchmarks).
pub struct SigOnly(pub &'static str, pub &'static [Signature]);

impl FileFormat for SigOnly {
    fn id(&self) -> &'static str {
        self.0
    }
    fn name(&self) -> &'static str {
        self.0
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["bin"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Other
    }
    fn signatures(&self) -> &'static [Signature] {
        self.1
    }
    fn min_size(&self) -> u64 {
        1
    }
    fn max_size(&self) -> u64 {
        1 << 30
    }
    fn measure(&self, _: &SharedSource, _: u64, _: u64) -> Result<Option<Measured>> {
        Ok(None)
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

/// The standard test registry: test, cont, strm, zfmt.
pub fn registry() -> FormatRegistry {
    FormatRegistry::new()
        .with(Arc::new(LenFmt::test()))
        .with(Arc::new(LenFmt::container()))
        .with(Arc::new(StreamFmt::default()))
        .with(Arc::new(ZeroLeadFmt))
}

/// Twenty formats whose signatures mirror the real signature table
/// (magic bytes from the carving spec), none of which measures anything.
pub fn twenty_formats() -> FormatRegistry {
    let sigs: Vec<(&'static str, Vec<Signature>)> = vec![
        ("jpeg", vec![Signature::at_start(b"\xFF\xD8\xFF")]),
        ("png", vec![Signature::at_start(b"\x89PNG\r\n\x1a\n")]),
        (
            "gif",
            vec![
                Signature::at_start(b"GIF87a"),
                Signature::at_start(b"GIF89a"),
            ],
        ),
        ("bmp", vec![Signature::at_start(b"BM")]),
        (
            "webp",
            vec![Signature::at_start(b"RIFF").with_also(b"WEBP", 8)],
        ),
        (
            "avi",
            vec![Signature::at_start(b"RIFF").with_also(b"AVI ", 8)],
        ),
        (
            "wav",
            vec![Signature::at_start(b"RIFF").with_also(b"WAVE", 8)],
        ),
        (
            "tiff",
            vec![Signature::at_start(b"II*\0"), Signature::at_start(b"MM\0*")],
        ),
        ("cr2", vec![Signature::at_start(b"II*\0\x10\0\0\0CR\x02\0")]),
        (
            "orf",
            vec![Signature::at_start(b"IIRO"), Signature::at_start(b"IIRS")],
        ),
        ("rw2", vec![Signature::at_start(b"IIU\0")]),
        ("raf", vec![Signature::at_start(b"FUJIFILMCCD-RAW ")]),
        ("psd", vec![Signature::at_start(b"8BPS")]),
        ("mp4", vec![Signature::at(b"ftyp", 4)]),
        (
            "mov",
            vec![Signature::at(b"moov", 4), Signature::at(b"mdat", 4)],
        ),
        ("mkv", vec![Signature::at_start(b"\x1A\x45\xDF\xA3")]),
        ("mp3", vec![Signature::at_start(b"ID3")]),
        ("flac", vec![Signature::at_start(b"fLaC")]),
        ("mpg", vec![Signature::at_start(b"\0\0\x01\xBA")]),
        ("m2v", vec![Signature::at_start(b"\0\0\x01\xB3")]),
    ];
    let mut reg = FormatRegistry::new();
    for (id, s) in sigs {
        reg.register(Arc::new(SigOnly(id, leak(s))));
    }
    assert_eq!(reg.len(), 20);
    reg
}

/// A `test` file of `len` bytes (>= 12) with a random payload.
pub fn test_file(len: usize, seed: u64) -> Vec<u8> {
    assert!(len >= 12);
    let mut v = random_bytes(len, seed);
    v[0..4].copy_from_slice(b"TST1");
    v[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    v[len - 4..].copy_from_slice(b"TEND");
    v
}

/// A `cont` file of `len` bytes holding `children` at the given offsets.
pub fn cont_file(len: usize, children: &[(usize, Vec<u8>)], trailer: bool, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(len, seed);
    v[0..4].copy_from_slice(b"CNT1");
    v[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    v[8..12].copy_from_slice(&(children.len() as u32).to_le_bytes());
    for (i, (off, c)) in children.iter().enumerate() {
        v[12 + 8 * i..16 + 8 * i].copy_from_slice(&(*off as u32).to_le_bytes());
        v[16 + 8 * i..20 + 8 * i].copy_from_slice(&(c.len() as u32).to_le_bytes());
        v[*off..*off + c.len()].copy_from_slice(c);
    }
    if trailer {
        v[len - 4..].copy_from_slice(b"CEND");
    } else {
        v[len - 4..].copy_from_slice(b"XXXX");
    }
    v
}

/// A `strm` run of `packets` 512-byte packets.
pub fn stream(packets: usize, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(packets * 512, seed);
    for p in 0..packets {
        v[p * 512..p * 512 + 4].copy_from_slice(b"SPKT");
    }
    v
}

/// A `zfmt` file of `len` bytes (starts with zero bytes for len < 2^24).
pub fn zfmt_file(len: usize, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(len, seed);
    v[0..4].copy_from_slice(&(len as u32).to_be_bytes());
    v[4..8].copy_from_slice(b"zfmt");
    v
}

/// Options used by most tests: 8 MiB chunks, 4 workers.
pub fn opts(reg: FormatRegistry) -> CarveOptions {
    let mut o = CarveOptions::new(reg);
    o.chunk_size = 8 << 20;
    o.workers = 4;
    o
}

/// Run a scan and collect everything.
pub fn carve(opts: CarveOptions, src: &SharedSource) -> (Vec<CarvedFile>, ScanSummary) {
    let carver = Carver::new(opts).expect("carver");
    let mut files = Vec::new();
    let summary = carver
        .scan(src, &mut |_| {}, &mut |f| {
            files.push(f);
            true
        })
        .expect("scan");
    check_invariants(&files, src.size());
    (files, summary)
}

/// Output contract: ids sequential, starts ascending, top-level files
/// disjoint, children inside their parent, everything inside the source.
pub fn check_invariants(files: &[CarvedFile], size: u64) {
    let mut last_top_end = 0u64;
    let mut last_start = 0u64;
    for (i, f) in files.iter().enumerate() {
        assert_eq!(f.id, i as u64 + 1, "ids are sequential");
        assert!(f.len > 0, "empty file {f:?}");
        assert!(f.end() <= size, "file past source end {f:?}");
        assert!(f.start >= last_start, "output not ascending at {f:?}");
        last_start = f.start;
        match f.parent {
            None => {
                assert!(f.start >= last_top_end, "overlapping files at {f:?}");
                last_top_end = f.end();
            }
            Some(p) => {
                let parent = &files[(p - 1) as usize];
                assert!(parent.parent.is_none());
                assert!(f.start >= parent.start && f.end() <= parent.end());
            }
        }
        for n in &f.nested {
            assert!(n.range.end <= f.len, "nested range outside file {f:?}");
        }
        for r in f.unreadable.iter() {
            assert!(r.end <= f.len);
        }
    }
}

/// `(format, start, len)` triples of top-level files.
pub fn spans(files: &[CarvedFile]) -> Vec<(String, u64, u64)> {
    files
        .iter()
        .map(|f| (f.format.clone(), f.start, f.len))
        .collect()
}
