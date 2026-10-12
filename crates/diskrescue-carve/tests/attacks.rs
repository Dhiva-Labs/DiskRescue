//! Adversarial sources and options: sources that return no data before
//! their advertised end, demand 4 KiB-aligned direct I/O, time out,
//! disappear and come back, return short or over-long reads; and absurd
//! option values. None may panic, hang, allocate without bound or lose a
//! readable file.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use diskrescue_carve::{CarveMode, CarvedFile, Carver, ScanSummary, MAX_CHUNK_SIZE};
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, FormatRegistry, Measured, Nested, NestedRole, Signature,
    Validation,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, ReadError, ReadErrorKind, SharedSource, Source};

/// Returns an absurd measurement record: reversed and out-of-file nested
/// ranges (some pointing at real `test` files), 200 000 nested entries, an
/// error offset of `u64::MAX` and 100 000 notes.
struct Garbage;

static GARB_SIGS: &[Signature] = &[Signature::at_start(b"GARB")];

impl FileFormat for Garbage {
    fn id(&self) -> &'static str {
        "garb"
    }
    fn name(&self) -> &'static str {
        "garbage"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["garb"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }
    fn signatures(&self) -> &'static [Signature] {
        GARB_SIGS
    }
    fn min_size(&self) -> u64 {
        1
    }
    fn max_size(&self) -> u64 {
        1 << 20
    }
    fn may_contain(&self, inner: &str) -> bool {
        inner == "test"
    }
    fn measure(
        &self,
        _: &SharedSource,
        _: u64,
        limit: u64,
    ) -> diskrescue_core::Result<Option<Measured>> {
        let len = limit.min(100_000);
        let mut m = Measured::truncated(len, Confidence(60), u64::MAX, "garbage");
        m.status = ExtentStatus::Truncated;
        let mut nested = vec![
            Nested {
                range: Range {
                    start: 50_000,
                    end: 10,
                },
                format: Some("test".into()),
                role: NestedRole::Thumbnail,
            },
            Nested {
                range: Range::new(90_000, u64::MAX),
                format: Some("test".into()),
                role: NestedRole::Preview,
            },
            Nested {
                range: Range::new(8192, 8192 + 5000),
                format: Some("test".into()),
                role: NestedRole::Thumbnail,
            },
            Nested {
                range: Range::new(u64::MAX - 1, u64::MAX),
                format: None,
                role: NestedRole::Trailer,
            },
        ];
        nested.extend((0..200_000u64).map(|i| Nested {
            range: Range::with_len(i % 100_000, 1),
            format: Some("nope".into()),
            role: NestedRole::Frame,
        }));
        m.nested = nested;
        m.notes = (0..100_000).map(|i| format!("note {i}")).collect();
        Ok(Some(m))
    }
    fn validate(&self, _: &SharedSource) -> diskrescue_core::Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

#[test]
fn absurd_measurement_records_are_sanitised() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 90);
    let base = 65_536;
    img[base..base + 4].copy_from_slice(b"GARB");
    put(&mut img, base + 8192, &test_file(5000, 1));
    let src: SharedSource = Arc::new(MemSource::new(img));
    let reg = FormatRegistry::new()
        .with(Arc::new(Garbage))
        .with(Arc::new(LenFmt::test()));
    for promote in [false, true] {
        let mut o = opts(reg.clone());
        o.promote_nested = promote;
        let s2 = src.clone();
        let (files, _) = with_timeout(move || carve(o, &s2));
        let g = &files[0];
        assert_eq!(
            (g.format.as_str(), g.start, g.len),
            ("garb", 65_536, 100_000)
        );
        assert!(g.err_offset.is_some_and(|e| e <= g.len));
        assert!(g.nested.len() <= 65_536);
        assert!(g
            .nested
            .iter()
            .all(|n| n.range.start < n.range.end && n.range.end <= g.len));
        assert!(g.notes.len() < 100);
        // The test file the record points at comes out as a promoted child
        // (the parent is truncated), never as an overlapping top-level file.
        let kids: Vec<_> = files.iter().filter(|f| f.parent == Some(g.id)).collect();
        assert_eq!(kids.len(), 1, "{:?}", spans(&files));
        assert_eq!((kids[0].start, kids[0].len), (65_536 + 8192, 5000));
    }
}

fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(120))
        .expect("finished in time")
}

/// Four test files at 4 KiB-aligned offsets in a random 8 MiB image.
fn image() -> (Vec<u8>, Vec<(String, u64, u64)>) {
    let size = 8 << 20;
    let mut img = random_bytes(size, 77);
    let mut want = Vec::new();
    for (i, at) in [65_536usize, 1 << 20, (3 << 20) + 4096, 6 << 20]
        .into_iter()
        .enumerate()
    {
        let len = 40_000 + 1000 * i;
        put(&mut img, at, &test_file(len, i as u64));
        want.push(("test".to_string(), at as u64, len as u64));
    }
    (img, want)
}

fn run(src: SharedSource, chunk: usize) -> (Vec<CarvedFile>, ScanSummary) {
    let mut o = opts(registry());
    o.chunk_size = chunk;
    carve(o, &src)
}

/// Returns `Ok(0)` (no data, no error) for reads touching `[gap)` until
/// `once` reads have been refused, and for everything at or past `real_end`.
struct NoData {
    data: Vec<u8>,
    claimed: u64,
    real_end: u64,
    gap: Range,
    refusals: AtomicU64,
}

impl Source for NoData {
    fn size(&self) -> u64 {
        self.claimed
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.real_end {
            return Ok(0);
        }
        let end = (offset + buf.len() as u64).min(self.real_end);
        if self.gap.overlaps(Range::new(offset, end)) && self.refusals.load(Ordering::Relaxed) > 0 {
            if offset >= self.gap.start {
                self.refusals.fetch_sub(1, Ordering::Relaxed);
                return Ok(0);
            }
            // Short read up to the gap.
            let n = (self.gap.start - offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..self.gap.start as usize]);
            return Ok(n);
        }
        let n = (end - offset) as usize;
        buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
        Ok(n)
    }
    fn describe(&self) -> String {
        "no-data source".into()
    }
}

#[test]
fn a_transient_empty_read_does_not_skip_the_rest_of_the_chunk() {
    let (img, want) = image();
    let size = img.len() as u64;
    // The scanner's first read hits the gap at 40 KiB; files after it in the
    // same 8 MiB chunk must still be found.
    let src: SharedSource = Arc::new(NoData {
        data: img,
        claimed: size,
        real_end: size,
        gap: Range::new(40_960, 41_472),
        refusals: AtomicU64::new(1),
    });
    let (files, sum) = run(src, 8 << 20);
    assert_eq!(spans(&files), want);
    assert_eq!(sum.unreadable_skipped, 512);
}

#[test]
fn a_source_shorter_than_its_size_is_carved_up_to_its_data() {
    let (img, want) = image();
    let real = 5 << 20;
    let src: SharedSource = Arc::new(NoData {
        data: img,
        claimed: 64 << 20,
        real_end: real,
        gap: Range::new(0, 0),
        refusals: AtomicU64::new(0),
    });
    let (files, sum) = run(src.clone(), 1 << 20);
    let expect: Vec<_> = want.into_iter().filter(|w| w.1 + w.2 <= real).collect();
    assert_eq!(spans(&files), expect);
    // Everything past the data is reported, not silently skipped.
    assert_eq!(sum.unreadable_skipped, (64 << 20) - real);
    assert_eq!(sum.scanned, 64 << 20);
    // Extraction of a record pointing past the data zero-fills and says so.
    let mut f = files[0].clone();
    f.start = real - 100;
    f.len = 1000;
    let mut out = Vec::new();
    let rep = Carver::extract(&src, &f, &mut out).unwrap();
    assert_eq!(rep.bytes, 1000);
    assert_eq!(rep.unreadable.as_slice(), &[Range::new(100, 1000)]);
}

/// A 4Kn direct-I/O device: offsets and lengths must be multiples of 4096
/// (`Unaligned` otherwise); reads touching a bad sector fail with `Io` (or
/// `Timeout`) at that sector.
struct Direct4k {
    data: Vec<u8>,
    bad: RangeSet,
    slow: RangeSet,
    unaligned_requests: AtomicU64,
}

impl Source for Direct4k {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn sector_size(&self) -> u32 {
        4096
    }
    fn requires_alignment(&self) -> bool {
        true
    }
    fn is_device(&self) -> bool {
        true
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset % 4096 != 0 || buf.len() % 4096 != 0 {
            self.unaligned_requests.fetch_add(1, Ordering::Relaxed);
            return Err(ReadError::unaligned(offset, buf.len(), 4096));
        }
        let size = self.data.len() as u64;
        if offset >= size {
            return Ok(0);
        }
        let n = (size - offset).min(buf.len() as u64);
        let r = Range::new(offset, offset + n);
        for (set, kind) in [
            (&self.bad, ReadErrorKind::Io),
            (&self.slow, ReadErrorKind::Timeout),
        ] {
            if let Some(h) = set.first_intersecting(r) {
                let at = h.start.max(offset) / 4096 * 4096;
                return Err(ReadError::new(at, buf.len(), kind, "4Kn medium error"));
            }
        }
        buf[..n as usize].copy_from_slice(&self.data[offset as usize..(offset + n) as usize]);
        Ok(n as usize)
    }
    fn describe(&self) -> String {
        "4Kn direct device".into()
    }
}

#[test]
fn four_k_direct_io_with_bad_and_slow_sectors() {
    let (img, want) = image();
    let bad = RangeSet::from_ranges([
        Range::with_len(65_536 + 8192, 1),        // inside file 0
        Range::with_len((2 << 20) + 100, 10_000), // between files
    ]);
    let slow = RangeSet::from_range(Range::with_len((1 << 20) + 12_288, 4096)); // inside file 1
    let src: SharedSource = Arc::new(Direct4k {
        data: img.clone(),
        bad,
        slow,
        unaligned_requests: AtomicU64::new(0),
    });
    let (files, sum) = run(src.clone(), 1 << 20);
    assert_eq!(spans(&files), want);
    assert_eq!(files[0].unreadable.as_slice(), &[Range::new(8192, 12_288)]);
    assert_eq!(
        files[1].unreadable.as_slice(),
        &[Range::new(12_288, 16_384)]
    );
    assert!(files[0].confidence < Confidence(90));
    assert!(files[2].unreadable.is_empty() && files[3].unreadable.is_empty());
    // Two 4 KiB sectors inside files, three around the 10 000-byte region.
    assert_eq!(sum.unreadable_skipped, 2 * 4096 + 3 * 4096);
    // Measurement reads at arbitrary offsets went through bounce buffers.
    let mut out = Vec::new();
    let rep = Carver::extract(&src, &files[0], &mut out).unwrap();
    assert_eq!(rep.unreadable.as_slice(), &[Range::new(8192, 12_288)]);
    let mut expect = img[65_536..65_536 + 40_000].to_vec();
    expect[8192..12_288].fill(0);
    assert_eq!(out, expect);
    let h = Carver::hash(&src, &files[0]).unwrap();
    assert_eq!(h, blake3::hash(&expect).to_hex().to_string());
}

/// Disappears after `after` reads and comes back on `reopen`.
struct Flappy {
    inner: MemSource,
    reads: AtomicU64,
    after: u64,
    gone: AtomicBool,
    reopens: AtomicU64,
}

impl Source for Flappy {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.reads.fetch_add(1, Ordering::Relaxed) == self.after {
            self.gone.store(true, Ordering::Relaxed);
        }
        if self.gone.load(Ordering::Relaxed) {
            return Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::DeviceGone,
                "USB reset",
            ));
        }
        self.inner.read_at(offset, buf)
    }
    fn reopen(&self) -> Result<(), ReadError> {
        self.reopens.fetch_add(1, Ordering::Relaxed);
        self.gone.store(false, Ordering::Relaxed);
        Ok(())
    }
    fn describe(&self) -> String {
        "flappy".into()
    }
}

#[test]
fn a_device_that_comes_back_after_a_reset_is_carved_completely() {
    let (img, want) = image();
    for after in [0u64, 1, 3, 7] {
        let flappy = Arc::new(Flappy {
            inner: MemSource::new(img.clone()),
            reads: AtomicU64::new(0),
            after,
            gone: AtomicBool::new(false),
            reopens: AtomicU64::new(0),
        });
        let src: SharedSource = flappy.clone();
        let (files, sum) = run(src, 1 << 20);
        assert_eq!(spans(&files), want, "after {after}");
        assert_eq!(sum.unreadable_skipped, 0);
        assert_eq!(flappy.reopens.load(Ordering::Relaxed), 1);
    }
}

/// Returns at most `max` bytes per call, or claims to have read more than
/// it was asked for (`liar`).
struct Odd {
    inner: MemSource,
    max: usize,
    liar: bool,
}

impl Source for Odd {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let n = buf.len().min(self.max);
        let got = self.inner.read_at(offset, &mut buf[..n])?;
        Ok(if self.liar && got > 0 { got + 17 } else { got })
    }
    fn describe(&self) -> String {
        "odd".into()
    }
}

#[test]
fn short_and_over_long_reads() {
    let (img, want) = image();
    let reference = run(Arc::new(MemSource::new(img.clone())), 1 << 20).0;
    assert_eq!(spans(&reference), want);
    for max in [1usize, 777, 4096, 100_000] {
        let src: SharedSource = Arc::new(Odd {
            inner: MemSource::new(img.clone()),
            max,
            liar: false,
        });
        // One byte per call is slow; a smaller image is enough there.
        if max == 1 {
            let small: SharedSource = Arc::new(Odd {
                inner: MemSource::new(img[..200_000].to_vec()),
                max,
                liar: false,
            });
            let (files, _) = run(small, 64 << 10);
            assert_eq!(spans(&files), want[..1].to_vec());
            continue;
        }
        let (files, _) = run(src, 1 << 20);
        assert_eq!(files, reference, "max {max}");
    }
    // A source that over-reports: no panic, and readable files still come
    // back (their contents are whatever the buffer held).
    let src: SharedSource = Arc::new(Odd {
        inner: MemSource::new(img),
        max: 100_000,
        liar: true,
    });
    let (files, _) = run(src, 1 << 20);
    check_invariants(&files, 8 << 20);
}

#[test]
fn absurd_options_are_clamped_not_obeyed() {
    let (img, want) = image();
    let src: SharedSource = Arc::new(MemSource::new(img));
    // Huge chunk and probe window: clamped, so no attempt to allocate the
    // address space; output unchanged.
    let mut o = opts(registry());
    o.chunk_size = usize::MAX;
    o.probe_window = usize::MAX;
    o.workers = 3;
    let c = Carver::new(o.clone()).unwrap();
    assert!(format!("{c:?}").contains(&format!("chunk: {MAX_CHUNK_SIZE}")));
    let (files, _) = carve(o, &src);
    assert_eq!(spans(&files), want);
    // Size cap 0: nothing can be measured.
    let mut o = opts(registry());
    o.max_file_size = Some(0);
    assert!(carve(o, &src).0.is_empty());
    // Size cap u64::MAX: same as none.
    let mut o = opts(registry());
    o.max_file_size = Some(u64::MAX);
    assert_eq!(spans(&carve(o, &src).0), want);
    // Alignment u64::MAX: only offset 0 could qualify.
    let mut o = opts(registry());
    o.alignment = u64::MAX;
    assert!(carve(o, &src).0.is_empty());
    // A cluster grid beyond the source: every header is off the grid.
    let mut o = opts(registry());
    o.cluster = Some((u64::MAX, 1));
    let (files, _) = carve(o, &src);
    assert_eq!(spans(&files), want);
    assert!(files.iter().all(|f| !f.aligned));
    let mut o = opts(registry());
    o.cluster = Some((0, u64::MAX));
    assert_eq!(spans(&carve(o, &src).0), want);
    // Unallocated ranges past the end, reversed, or empty.
    let mut o = opts(registry());
    o.mode = CarveMode::Unallocated(RangeSet::from_ranges([
        Range::new(u64::MAX - 10, u64::MAX),
        Range::new(100, 50),
        Range::new(65_536, 65_537),
    ]));
    let (files, sum) = carve(o, &src);
    assert_eq!(spans(&files), want[..1].to_vec());
    assert_eq!(sum.scanned, 1);
}
