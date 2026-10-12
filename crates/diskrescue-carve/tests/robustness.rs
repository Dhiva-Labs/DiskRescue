//! Hostile and damaged inputs: random data, zero regions, unreadable
//! ranges, failing reads, cancellation and misbehaving formats.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use diskrescue_carve::{CarveMode, Carver, PENALTY_UNREADABLE};
use diskrescue_core::format::{Confidence, ExtentStatus, FormatRegistry};
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, RangeSet, ReadError, ReadErrorKind, SharedSource, Source};

fn mem(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

fn hostile_registry() -> FormatRegistry {
    registry()
        .with(Arc::new(GreedyFmt))
        .with(Arc::new(Hostile("boom", BOOM_SIGS)))
        .with(Arc::new(Hostile("prbm", PRBM_SIGS)))
        .with(Arc::new(Hostile("liar", LIAR_SIGS)))
        .with(Arc::new(Hostile("null", NULL_SIGS)))
}

#[test]
fn random_data_yields_no_files_and_no_panic() {
    let src = mem(random_bytes(64 << 20, 99));
    for alignment in [512, 1] {
        let mut o = opts(hostile_registry());
        o.alignment = alignment;
        let (files, sum) = carve(o, &src);
        assert!(files.is_empty(), "alignment {alignment}: {files:?}");
        assert_eq!(sum.scanned, 64 << 20);
    }
}

#[test]
fn sprinkled_magics_over_random_data_keep_the_output_contract() {
    // Thousands of magic strings with garbage behind them, at random
    // (aligned and unaligned) offsets, plus truncated real files.
    let size = 16 << 20;
    let mut img = random_bytes(size, 5);
    let mut rng = Rng::new(77);
    let magics: [&[u8]; 9] = [
        b"TST1", b"CNT1", b"SPKT", b"zfmt", b"RAWS", b"BOOM", b"LIAR", b"NULL", b"PRBM",
    ];
    for i in 0..4000u64 {
        let off = rng.below(size as u64 - 64) as usize;
        let off = if i % 2 == 0 { off / 512 * 512 } else { off };
        let m = magics[rng.below(magics.len() as u64) as usize];
        put(&mut img, off, m);
        if i % 7 == 0 {
            // A plausible length field pointing anywhere, even past the end.
            let len = rng.below(64 << 20) as u32;
            put(&mut img, off + 4, &len.to_le_bytes());
        }
    }
    for i in 0..50u64 {
        let off = (rng.below(size as u64 - 300_000) / 512 * 512) as usize;
        let mut f = test_file(12 + rng.below(200_000) as usize, i);
        let cut = rng.below(f.len() as u64) as usize;
        f.truncate(cut.max(8));
        put(&mut img, off, &f);
    }
    let src = mem(img);
    for (alignment, workers) in [(512, 4), (1, 8)] {
        let mut o = opts(hostile_registry());
        o.alignment = alignment;
        o.workers = workers;
        o.chunk_size = 1 << 20;
        let (files, sum) = carve(o, &src);
        assert!(sum.candidates > 0);
        // Whatever was found obeys the contract (checked in `carve`), and
        // misbehaving formats were contained.
        for f in &files {
            assert_ne!(f.format, "boom");
            assert_ne!(f.format, "null");
            if f.format == "liar" {
                assert!(f.len <= 1 << 20);
            }
        }
    }
}

#[test]
fn hostile_formats_are_contained() {
    let size = 4 << 20;
    let mut img = random_bytes(size, 6);
    put(&mut img, 4096, b"BOOM");
    put(&mut img, 8192, b"PRBM");
    put(&mut img, 16_384, b"NULL");
    put(&mut img, 65_536, b"LIAR");
    put(&mut img, (2 << 20) + 4096, &test_file(5000, 1));
    let src = mem(img);
    let (files, sum) = carve(opts(hostile_registry()), &src);
    assert_eq!(
        spans(&files),
        vec![
            ("liar".into(), 65_536, 1 << 20),
            ("test".into(), (2 << 20) + 4096, 5000)
        ]
    );
    assert_eq!(files[0].status, ExtentStatus::Capped);
    assert!(files[0].notes.iter().any(|n| n.contains("clamped")));
    assert!(sum.false_positives >= 3);
}

#[test]
fn zero_regions_are_skipped_without_losing_files() {
    let size = 16 << 20;
    let mut img = vec![0u8; size];
    // Islands of data in a zero sea, and files right after zero runs.
    put(&mut img, 3 << 20, &random_bytes(100_000, 1));
    put(&mut img, 4 << 20, &test_file(50_000, 2));
    put(&mut img, (5 << 20) + 4096, &zfmt_file(20_000, 3));
    put(&mut img, (9 << 20) + 512, &test_file(4000, 4));
    put(&mut img, 12 << 20, &stream(8, 5));
    // A zfmt whose zero size bytes are the tail of an otherwise zero block.
    let z = zfmt_file(1000, 6);
    put(&mut img, (14 << 20) - 4096 + 3584, &z);
    let src = mem(img);
    let mut results = Vec::new();
    for zero_skip in [true, false] {
        let mut o = opts(registry());
        o.zero_skip = zero_skip;
        o.chunk_size = 1 << 20;
        let (files, _) = carve(o, &src);
        results.push(files);
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(
        spans(&results[0]),
        vec![
            ("test".into(), 4 << 20, 50_000),
            ("zfmt".into(), (5 << 20) + 4096, 20_000),
            ("test".into(), (9 << 20) + 512, 4000),
            ("strm".into(), 12 << 20, 8 * 512),
            ("zfmt".into(), (14 << 20) - 512, 1000),
        ]
    );
}

#[test]
fn known_unreadable_ranges_are_skipped_and_reported() {
    let size = 4 << 20;
    let mut img = random_bytes(size, 30);
    put(&mut img, 8192, &test_file(60_000, 1)); // hole inside
    put(&mut img, 1 << 20, &test_file(4000, 2)); // header inside a hole
    put(&mut img, 2 << 20, &test_file(4000, 3)); // clean
    let holes = RangeSet::from_ranges([
        Range::new(8192 + 12_288, 8192 + 16_384),
        Range::new((1 << 20) - 512, (1 << 20) + 1024),
        Range::new(3 << 20, (3 << 20) + 65_536),
    ]);
    let src: SharedSource = Arc::new(MemSource::new(img).with_unreadable(holes.clone()));
    let (files, sum) = carve(opts(registry()), &src);
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), 8192, 60_000),
            ("test".into(), 2 << 20, 4000)
        ]
    );
    assert_eq!(
        files[0].unreadable.as_slice(),
        &[Range::new(12_288, 16_384)]
    );
    assert_eq!(files[0].confidence, Confidence(90 - PENALTY_UNREADABLE));
    assert!(files[0].notes.iter().any(|n| n.contains("unreadable")));
    assert!(files[1].unreadable.is_empty());
    assert_eq!(sum.unreadable_skipped, holes.total_len());
}

/// A source whose bad sectors are not advertised by `unreadable()`: every
/// read touching them fails, like a raw image of a failing disk.
struct Failing {
    data: Vec<u8>,
    bad: RangeSet,
    reads: AtomicU64,
}

impl Source for Failing {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let size = self.data.len() as u64;
        if offset >= size {
            return Ok(0);
        }
        let n = (size - offset).min(buf.len() as u64);
        if let Some(h) = self.bad.first_intersecting(Range::new(offset, offset + n)) {
            let at = h.start.max(offset);
            return Err(ReadError::new(
                at,
                buf.len(),
                ReadErrorKind::Io,
                "medium error",
            ));
        }
        buf[..n as usize].copy_from_slice(&self.data[offset as usize..(offset + n) as usize]);
        Ok(n as usize)
    }
    fn describe(&self) -> String {
        "failing test disk".into()
    }
}

#[test]
fn read_errors_mid_chunk_never_abort_the_scan() {
    let size = 12 << 20;
    let mut img = random_bytes(size, 31);
    put(&mut img, 4096, &test_file(30_000, 1)); // a bad sector inside
    put(&mut img, 9 << 20, &test_file(30_000, 2)); // after a bad region
    let bad = RangeSet::from_ranges([
        Range::new(4096 + 10_240, 4096 + 10_241),
        Range::new(5 << 20, (5 << 20) + 200_000),
    ]);
    let src: SharedSource = Arc::new(Failing {
        data: img,
        bad,
        reads: AtomicU64::new(0),
    });
    let (files, sum) = carve(opts(registry()), &src);
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), 4096, 30_000),
            ("test".into(), 9 << 20, 30_000)
        ]
    );
    // The bad sector inside the first file was found by the scanner before
    // the file was emitted, at sector granularity.
    assert_eq!(
        files[0].unreadable.as_slice(),
        &[Range::new(10_240, 10_752)]
    );
    // 1 sector + the bad region rounded out to sectors.
    let region = ((5u64 << 20) + 200_000).div_ceil(512) * 512 - (5 << 20);
    assert_eq!(sum.unreadable_skipped, 512 + region);
    let mut out = Vec::new();
    let rep = Carver::extract(&src, &files[0], &mut out).unwrap();
    assert_eq!(rep.bytes, 30_000);
    assert_eq!(rep.unreadable.as_slice(), &[Range::new(10_240, 10_752)]);
}

/// A source that disappears for good after a number of reads.
struct Vanishing {
    inner: MemSource,
    left: AtomicU64,
}

impl Source for Vanishing {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.left.fetch_sub(1, Ordering::Relaxed) == 0 {
            self.left.store(0, Ordering::Relaxed);
            return Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::DeviceGone,
                "gone",
            ));
        }
        self.inner.read_at(offset, buf)
    }
    fn describe(&self) -> String {
        "vanishing".into()
    }
}

#[test]
fn a_vanished_device_is_an_error_not_a_hang() {
    let src: SharedSource = Arc::new(Vanishing {
        inner: MemSource::new(random_bytes(8 << 20, 32)),
        left: AtomicU64::new(3),
    });
    let mut o = opts(registry());
    o.chunk_size = 1 << 20;
    let carver = Carver::new(o).unwrap();
    let r = carver.scan(&src, &mut |_| {}, &mut |_| true);
    assert!(r.is_err());
}

#[test]
fn cancel_flag_stops_the_scan_early() {
    let size = 32 << 20;
    let mut img = random_bytes(size, 40);
    for i in 0..32 {
        put(&mut img, i << 20, &test_file(10_000, i as u64));
    }
    let src = mem(img);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut o = opts(registry());
    o.chunk_size = 1 << 20;
    o.cancel = Some(cancel.clone());
    let carver = Carver::new(o).unwrap();
    let mut files = Vec::new();
    let sum = carver
        .scan(&src, &mut |_| {}, &mut |f| {
            files.push(f);
            if files.len() == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
            true
        })
        .unwrap();
    assert!(sum.cancelled);
    assert!(files.len() >= 3 && files.len() < 32, "{}", files.len());
    assert!(sum.scanned < size as u64);
    check_invariants(&files, size as u64);
}

#[test]
fn cancel_from_progress_callback() {
    let src = mem(random_bytes(32 << 20, 41));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut o = opts(registry());
    o.chunk_size = 1 << 20;
    o.cancel = Some(cancel.clone());
    let carver = Carver::new(o).unwrap();
    let mut calls = 0;
    let sum = carver
        .scan(
            &src,
            &mut |_| {
                calls += 1;
                if calls == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &mut |_| true,
        )
        .unwrap();
    assert!(sum.cancelled);
    assert!(sum.scanned <= 3 << 20, "{}", sum.scanned);
}

#[test]
fn on_file_returning_false_stops_immediately() {
    let size = 16 << 20;
    let mut img = random_bytes(size, 42);
    for i in 0..16 {
        put(&mut img, i << 20, &test_file(10_000, i as u64));
    }
    let src = mem(img);
    let carver = Carver::new(opts(registry())).unwrap();
    let mut n = 0;
    let sum = carver
        .scan(&src, &mut |_| {}, &mut |_| {
            n += 1;
            n < 2
        })
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(sum.files, 2);
    assert!(sum.cancelled);
}

#[test]
fn unallocated_mode_with_empty_or_out_of_range_set() {
    let src = mem(random_bytes(1 << 20, 43));
    let mut o = opts(registry());
    o.mode = CarveMode::Unallocated(RangeSet::new());
    let (files, sum) = carve(o, &src);
    assert!(files.is_empty());
    assert_eq!(sum.scanned, 0);
    let mut o = opts(registry());
    o.mode = CarveMode::Unallocated(RangeSet::from_range(Range::new(5 << 20, 6 << 20)));
    let (files, sum) = carve(o, &src);
    assert!(files.is_empty());
    assert_eq!(sum.scanned, 0);
}

#[test]
fn tiny_and_empty_sources() {
    for len in [0usize, 1, 4, 11, 12, 511, 512, 4097] {
        let mut v = random_bytes(len, len as u64);
        if len >= 12 {
            v = test_file(len, 3);
        }
        let src = mem(v);
        let (files, _) = carve(opts(registry()), &src);
        if len >= 12 {
            assert_eq!(spans(&files), vec![("test".into(), 0, len as u64)]);
        } else {
            assert!(files.is_empty());
        }
    }
}

#[test]
fn extract_and_hash_tolerate_absurd_records() {
    let data = random_bytes(10_000, 50);
    let src = mem(data.clone());
    let base = {
        let mut img = data.clone();
        put(&mut img, 0, &test_file(5000, 1));
        let (files, _) = carve(opts(registry()), &mem(img));
        files[0].clone()
    };
    for (start, len) in [
        (20_000u64, 100u64),
        (9_990, u64::MAX),
        (u64::MAX - 5, 10),
        (0, 0),
        (5_000, 5_000),
    ] {
        let mut f = base.clone();
        f.start = start;
        f.len = len;
        let mut out = Vec::new();
        let rep = Carver::extract(&src, &f, &mut out).unwrap();
        assert_eq!(rep.bytes, out.len() as u64);
        let expect_len = (start.min(10_000)..start.saturating_add(len).min(10_000)).count();
        assert_eq!(out.len(), expect_len);
        let h = Carver::hash(&src, &f).unwrap();
        assert_eq!(h, blake3::hash(&out).to_hex().to_string());
    }
}
