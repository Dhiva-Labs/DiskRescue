//! Regressions for data loss found in review: a file whose length field
//! overruns its real end must not swallow the files written after it, and the
//! carver's own confidence adjustments must never drop a file the format
//! accepted.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_carve::{CarveMode, CarvedFile, PENALTY_CONFLICT, PENALTY_SPLIT};
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, FormatRegistry, Measured, Signature, Validation,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::MemSource;
use diskrescue_core::util::bytes::u32le;
use diskrescue_core::{Range, RangeSet, Result, SharedSource, SourceExt};

fn mem(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

/// A box whose u32 BE size claims 256 KiB but only its first 16 KiB were
/// written before the next files: the `zfmt` measure trusts the size field
/// (as real box and RIFF formats do) and reports it complete at 90.
fn overclaiming_box() -> Vec<u8> {
    let mut v = zfmt_file(16 << 10, 99);
    v[0..4].copy_from_slice(&(256u32 << 10).to_be_bytes());
    v
}

#[test]
fn overclaimed_length_does_not_swallow_later_files() {
    let size = 2 << 20;
    let mut img = random_bytes(size, 1);
    let base = 1 << 20;
    put(&mut img, base, &overclaiming_box());
    // Fully inside the claim: a conflict child.
    let inner = test_file(50_000, 2);
    put(&mut img, base + (64 << 10), &inner);
    // Starts inside the claim and runs past its end: cuts the box.
    let across = test_file(100_000, 3);
    put(&mut img, base + (192 << 10), &across);
    let src = mem(img);
    for (workers, chunk) in [(1usize, 8usize << 20), (4, 64 << 10), (8, 4096)] {
        let mut o = opts(registry());
        o.workers = workers;
        o.chunk_size = chunk;
        let (files, _) = carve(o, &src);
        let got: Vec<(String, u64, u64, Option<u64>)> = files
            .iter()
            .map(|f| (f.format.clone(), f.start, f.len, f.parent))
            .collect();
        let b = base as u64;
        assert_eq!(
            got,
            vec![
                ("zfmt".into(), b, 192 << 10, None),
                ("test".into(), b + (64 << 10), 50_000, Some(1)),
                ("test".into(), b + (192 << 10), 100_000, None),
            ],
            "workers={workers} chunk={chunk}"
        );
        let boxf = &files[0];
        assert_eq!(boxf.status, ExtentStatus::Truncated);
        assert_eq!(
            boxf.confidence,
            Confidence(90 - PENALTY_SPLIT - PENALTY_CONFLICT)
        );
        assert!(boxf.notes.iter().any(|n| n.contains("probably damaged")));
        assert!(boxf.notes.iter().any(|n| n.contains("runs past")));
        let child = &files[1];
        assert_eq!(child.status, ExtentStatus::Complete);
        assert_eq!(child.confidence, Confidence(90));
        assert!(child
            .notes
            .iter()
            .any(|n| n.contains("length is probably wrong")));
        let h = diskrescue_carve::Carver::hash(&src, child).unwrap();
        assert_eq!(h, blake3::hash(&inner).to_hex().to_string());
        let h = diskrescue_carve::Carver::hash(&src, &files[2]).unwrap();
        assert_eq!(h, blake3::hash(&across).to_hex().to_string());
    }
}

#[test]
fn containable_and_unaligned_files_inside_a_complete_file_stay_inside() {
    // `cont` may contain `test`: an aligned test file inside an intact
    // container is its embedded object, not a conflict. An unaligned one is
    // no evidence either way.
    let size = 1 << 20;
    let mut img = random_bytes(size, 4);
    let c = cont_file(200_000, &[(8192, test_file(5000, 5))], true, 6);
    put(&mut img, 4096, &c);
    put(&mut img, 4096 + 100_001, &test_file(5000, 7));
    let src = mem(img);
    let (files, _) = carve(opts(registry()), &src);
    assert_eq!(spans(&files), vec![("cont".into(), 4096, 200_000)]);
    assert_eq!(files[0].confidence, Confidence(90));
}

/// A run-length-like format: `WEAK` + u32 LE length; always `Truncated` at
/// confidence 25 (like a power-cut MP4 with no `moov`), and 20 points less
/// when the source reports unreadable bytes inside (as the real MP4 and
/// JPEG measures do).
struct WeakFmt;

static WEAK_SIGS: &[Signature] = &[Signature::at_start(b"WEAK")];

impl FileFormat for WeakFmt {
    fn id(&self) -> &'static str {
        "weak"
    }
    fn name(&self) -> &'static str {
        "weak"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["wk"]
    }
    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }
    fn signatures(&self) -> &'static [Signature] {
        WEAK_SIGS
    }
    fn min_size(&self) -> u64 {
        16
    }
    fn max_size(&self) -> u64 {
        16 << 20
    }
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let h = src.read_vec(start, 8)?;
        let Some(len) = u32le(&h, 4).map(u64::from) else {
            return Ok(None);
        };
        if h.get(..4) != Some(&b"WEAK"[..]) || len < 16 {
            return Ok(None);
        }
        let len = len.min(limit);
        let mut m = Measured::truncated(len, Confidence::LOW, len, "no index");
        if src.overlaps_unreadable(start, len) {
            m.confidence = m.confidence.lowered(20);
        }
        Ok(Some(m))
    }
    fn validate(&self, _: &SharedSource) -> Result<Validation> {
        Ok(Validation::invalid("test format"))
    }
}

fn weak_file(len: usize, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(len, seed);
    v[0..4].copy_from_slice(b"WEAK");
    v[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    v
}

fn weak_registry() -> FormatRegistry {
    registry().with(Arc::new(WeakFmt))
}

fn weak_files(files: &[CarvedFile]) -> Vec<&CarvedFile> {
    files.iter().filter(|f| f.format == "weak").collect()
}

#[test]
fn unreadable_bytes_never_drop_a_barely_accepted_file() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 8);
    put(&mut img, 65_536, &weak_file(200_000, 9));
    let holes = RangeSet::from_range(Range::with_len(65_536 + 100_000, 512));
    let src: SharedSource = Arc::new(MemSource::new(img).with_unreadable(holes));
    let (files, _) = carve(opts(weak_registry()), &src);
    let w = weak_files(&files);
    assert_eq!(w.len(), 1, "the damaged file must be reported: {files:?}");
    assert_eq!((w[0].start, w[0].len), (65_536, 200_000));
    assert_eq!(w[0].unreadable.total_len(), 512);
    assert!(w[0].confidence < Confidence::LOW);
    assert!(w[0]
        .notes
        .iter()
        .any(|n| n.contains("after the carver's adjustments")));
}

#[test]
fn allocated_space_and_off_cluster_penalties_never_drop_a_file() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 10);
    // Off the 8 KiB cluster grid (data_start 0), running from free space
    // into allocated space.
    put(&mut img, 65_536 + 512, &weak_file(100_000, 11));
    let src = mem(img);
    let mut o = opts(weak_registry());
    o.cluster = Some((0, 8192));
    o.mode = CarveMode::Unallocated(RangeSet::from_range(Range::new(0, 131_072)));
    let (files, _) = carve(o, &src);
    let w = weak_files(&files);
    assert_eq!(w.len(), 1, "{files:?}");
    assert_eq!(w[0].confidence, Confidence(0));
    assert!(w[0].notes.iter().any(|n| n.contains("allocated space")));
    assert!(w[0].notes.iter().any(|n| n.contains("cluster boundary")));
}

#[test]
fn randomized_overclaims_are_deterministic_and_lose_nothing() {
    // Overclaiming boxes, intact test files and weak files laid out at
    // random aligned offsets: whatever the thread timing, the output is the
    // same, and every intact test file comes back whole (top-level or as a
    // conflict child).
    for seed in 0..6u64 {
        let size = 4 << 20;
        let mut img = random_bytes(size, 100 + seed);
        let mut rng = Rng::new(seed);
        let mut tests = Vec::new();
        let mut pos = 4096usize;
        while pos + 300_000 < size {
            match rng.below(3) {
                0 => {
                    let b = overclaiming_box();
                    put(&mut img, pos, &b);
                    pos += b.len();
                }
                1 => {
                    let len = 1000 + rng.below(80_000) as usize;
                    let f = test_file(len, seed * 1000 + pos as u64);
                    put(&mut img, pos, &f);
                    tests.push((pos as u64, f));
                    pos += len;
                }
                _ => {
                    let len = 1000 + rng.below(30_000) as usize;
                    put(&mut img, pos, &weak_file(len, pos as u64));
                    pos += len;
                }
            }
            pos = pos.div_ceil(512) * 512 + 512 * rng.below(16) as usize;
        }
        let src = mem(img);
        let mut outputs = Vec::new();
        for (workers, chunk) in [(1usize, 8usize << 20), (8, 4096), (3, 100_000)] {
            let mut o = opts(weak_registry());
            o.workers = workers;
            o.chunk_size = chunk;
            outputs.push(carve(o, &src).0);
        }
        assert_eq!(outputs[0], outputs[1], "seed {seed}");
        assert_eq!(outputs[0], outputs[2], "seed {seed}");
        for (start, data) in &tests {
            let f = outputs[0]
                .iter()
                .find(|f| f.start == *start && f.format == "test")
                .unwrap_or_else(|| panic!("seed {seed}: test file at {start} lost"));
            assert_eq!(f.len, data.len() as u64, "seed {seed} at {start}");
        }
    }
}
