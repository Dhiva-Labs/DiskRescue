//! Carving scenarios over synthetic images with fake formats.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_carve::{CarveMode, Carver, PENALTY_ALLOCATED, PENALTY_OFF_CLUSTER};
use diskrescue_core::format::{Confidence, ExtentStatus, FormatRegistry, Signature};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::{Range, RangeSet, SharedSource};

fn mem(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

/// Lay out `n` test files back to back on 512-byte boundaries after
/// `first`, returning the image and the expected spans.
fn back_to_back(
    size: usize,
    first: usize,
    n: usize,
    seed: u64,
) -> (Vec<u8>, Vec<(String, u64, u64)>) {
    let mut img = random_bytes(size, seed);
    let mut rng = Rng::new(seed ^ 0xABCD);
    let mut pos = first;
    let mut want = Vec::new();
    for i in 0..n {
        let len = 12 + rng.below(300_000) as usize;
        if pos + len > size {
            break;
        }
        put(&mut img, pos, &test_file(len, seed + i as u64));
        want.push(("test".to_string(), pos as u64, len as u64));
        pos = (pos + len).div_ceil(512) * 512;
    }
    (img, want)
}

#[test]
fn files_back_to_back_are_found_exactly() {
    let (img, want) = back_to_back(24 << 20, 0, 60, 1);
    assert!(want.len() > 40);
    let src = mem(img);
    let (files, sum) = carve(opts(registry()), &src);
    assert_eq!(spans(&files), want);
    for f in &files {
        assert_eq!(f.status, ExtentStatus::Complete);
        assert_eq!(f.confidence, Confidence(90));
        assert_eq!(f.category, MediaCategory::Photo);
        assert_eq!(f.extension, "tst");
        assert!(f.aligned);
        assert!(f.unreadable.is_empty());
        assert!(f.parent.is_none());
    }
    assert_eq!(sum.files, want.len() as u64);
    assert_eq!(sum.by_format.get("test"), Some(&(want.len() as u64)));
    assert_eq!(sum.bytes_attributed, want.iter().map(|w| w.2).sum::<u64>());
    assert_eq!(sum.scanned, 24 << 20);
    assert!(!sum.cancelled);
}

#[test]
fn output_is_independent_of_workers_chunks_and_zero_skip() {
    // A mixed image: test files, a container with a nested file, a stream,
    // zero gaps and zfmt boxes.
    let size = 6 << 20;
    let mut img = random_bytes(size, 7);
    img[1 << 20..(1 << 20) + 307_200].fill(0);
    put(&mut img, 4096, &test_file(70_000, 1));
    let inner = test_file(5000, 2);
    put(
        &mut img,
        102_400,
        &cont_file(200_000, &[(8192, inner)], true, 3),
    );
    put(&mut img, 409_600, &stream(40, 4));
    // Right after an all-zero block, starting with zero bytes itself.
    put(&mut img, (1 << 20) + 307_200, &zfmt_file(9000, 5));
    put(&mut img, 2 << 20, &test_file(1_500_000, 6));
    let src = mem(img);
    let mut base = None;
    for (workers, chunk, zero_skip) in [
        (1, 8 << 20, true),
        (8, 8 << 20, false),
        (3, 65_536, true),
        (16, 4096, true),
        (2, 1 << 20, false),
    ] {
        let mut o = opts(registry());
        o.workers = workers;
        o.chunk_size = chunk;
        o.zero_skip = zero_skip;
        let (files, _) = carve(o, &src);
        match &base {
            None => base = Some(files),
            Some(b) => assert_eq!(
                &files, b,
                "workers {workers} chunk {chunk} zero_skip {zero_skip}"
            ),
        }
    }
    let files = base.unwrap();
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), 4096, 70_000),
            ("cont".into(), 102_400, 200_000),
            ("strm".into(), 409_600, 40 * 512),
            ("zfmt".into(), (1 << 20) + 307_200, 9000),
            ("test".into(), 2 << 20, 1_500_000),
        ]
    );
}

#[test]
fn files_at_and_across_chunk_boundaries() {
    let chunk = 8 << 20;
    let size = 3 * chunk;
    let mut img = random_bytes(size, 11);
    // Starts 512 bytes before the boundary and runs across it.
    put(&mut img, chunk - 512, &test_file(100_000, 1));
    // Starts exactly on the second boundary.
    put(&mut img, 2 * chunk, &test_file(4096, 2));
    let src = mem(img);
    let (files, _) = carve(opts(registry()), &src);
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), (chunk - 512) as u64, 100_000),
            ("test".into(), (2 * chunk) as u64, 4096),
        ]
    );
}

#[test]
fn header_split_across_chunks_is_found_once() {
    let chunk = 8 << 20;
    let size = 2 * chunk + 4096;
    let mut img = random_bytes(size, 12);
    // `TST1` straddles the boundary: "TS" | "T1".
    put(&mut img, chunk - 2, &test_file(2000, 1));
    // An offset signature whose start is in the first chunk and whose
    // pattern lies entirely in the second.
    put(&mut img, 2 * chunk - 4, &zfmt_file(3000, 2));
    let src = mem(img);
    let mut o = opts(registry());
    o.alignment = 2;
    let (files, sum) = carve(o, &src);
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), (chunk - 2) as u64, 2000),
            ("zfmt".into(), (2 * chunk - 4) as u64, 3000),
        ]
    );
    assert_eq!(sum.files, 2);
}

#[test]
fn embedded_nested_object_is_not_a_separate_file() {
    let size = 2 << 20;
    let mut img = random_bytes(size, 13);
    let inner = test_file(20_000, 1);
    // Child on a sector boundary (it would be a candidate on its own).
    let cont = cont_file(300_000, &[(4096, inner.clone())], true, 2);
    put(&mut img, 65_536, &cont);
    let src = mem(img);
    let (files, sum) = carve(opts(registry()), &src);
    assert_eq!(spans(&files), vec![("cont".into(), 65_536, 300_000)]);
    let f = &files[0];
    assert_eq!(f.status, ExtentStatus::Complete);
    assert_eq!(f.nested.len(), 1);
    assert_eq!(f.nested[0].range, Range::new(4096, 4096 + 20_000));
    assert_eq!(f.nested[0].format.as_deref(), Some("test"));
    assert_eq!(sum.files, 1);
}

#[test]
fn damaged_parent_with_valid_nested_object_is_promoted() {
    let size = 2 << 20;
    let mut img = random_bytes(size, 14);
    let good = test_file(10_000, 1);
    let mut bad = test_file(6000, 2);
    bad[5996..].copy_from_slice(b"XXXX"); // nested object without its trailer
    let cont = cont_file(200_000, &[(1000, good), (50_000, bad)], false, 3);
    put(&mut img, 131_072, &cont);
    let src = mem(img);
    let (files, sum) = carve(opts(registry()), &src);
    assert_eq!(
        spans(&files),
        vec![
            ("cont".into(), 131_072, 200_000),
            ("test".into(), 131_072 + 1000, 10_000),
        ]
    );
    assert_eq!(files[0].status, ExtentStatus::Truncated);
    assert_eq!(files[0].nested.len(), 2);
    let child = &files[1];
    assert_eq!(child.parent, Some(files[0].id));
    assert_eq!(child.status, ExtentStatus::Complete);
    assert!(!child.aligned);
    assert!(child.notes.iter().any(|n| n.contains("promoted")));
    assert_eq!(sum.files, 2);
    assert_eq!(sum.bytes_attributed, 200_000);
}

#[test]
fn promote_nested_extracts_children_of_intact_parents() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 15);
    let cont = cont_file(
        100_000,
        &[(512, test_file(3000, 1)), (8192, test_file(4000, 2))],
        true,
        3,
    );
    put(&mut img, 4096, &cont);
    let src = mem(img);
    let (files, _) = carve(opts(registry()), &src);
    assert_eq!(files.len(), 1, "intact parent: nothing promoted by default");
    let mut o = opts(registry());
    o.promote_nested = true;
    let (files, _) = carve(o, &src);
    assert_eq!(
        spans(&files),
        vec![
            ("cont".into(), 4096, 100_000),
            ("test".into(), 4096 + 512, 3000),
            ("test".into(), 4096 + 8192, 4000),
        ]
    );
    assert!(files[1..].iter().all(|f| f.parent == Some(1)));
}

#[test]
fn unaligned_and_garbage_candidates_are_rejected() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 16);
    put(&mut img, 1000, &test_file(5000, 1)); // not on a sector boundary
                                              // Aligned magic with an absurd length.
    put(&mut img, 16_384, b"TST1\xff\xff\xff\x7f");
    // Aligned magic with a plausible length but no trailer.
    let mut no_trailer = test_file(9000, 2);
    no_trailer[8996..].copy_from_slice(b"ABCD");
    put(&mut img, 32_768, &no_trailer);
    let src = mem(img);
    let (files, sum) = carve(opts(registry()), &src);
    assert!(files.is_empty(), "{files:?}");
    assert!(sum.false_positives >= 2);
    assert!(sum.candidates >= 2);
    for alignment in [1, 8] {
        let mut o = opts(registry());
        o.alignment = alignment;
        let (files, _) = carve(o, &src);
        assert_eq!(spans(&files), vec![("test".into(), 1000, 5000)]);
        assert!(files[0].aligned || alignment == 1);
    }
}

#[test]
fn unallocated_mode_scans_only_free_ranges() {
    let size = 4 << 20;
    let mut img = random_bytes(size, 17);
    put(&mut img, 65_536, &test_file(30_000, 1)); // free
    put(&mut img, 1 << 20, &test_file(30_000, 2)); // allocated: skipped
    put(&mut img, 2 << 20, &test_file(16_384, 3)); // starts free, runs into allocated
    let free = RangeSet::from_ranges([
        Range::new(0, 512 << 10),
        Range::new(2 << 20, (2 << 20) + 4096),
    ]);
    let src = mem(img);
    let mut o = opts(registry());
    o.mode = CarveMode::Unallocated(free.clone());
    let (files, sum) = carve(o, &src);
    assert_eq!(
        spans(&files),
        vec![
            ("test".into(), 65_536, 30_000),
            ("test".into(), 2 << 20, 16_384),
        ]
    );
    assert_eq!(files[0].confidence, Confidence(90));
    assert_eq!(files[1].confidence, Confidence(90 - PENALTY_ALLOCATED));
    assert!(files[1]
        .notes
        .iter()
        .any(|n| n.contains("extends into allocated space")));
    assert_eq!(sum.scanned, free.total_len());
}

#[test]
fn weak_length_parent_ends_at_next_aligned_header() {
    let size = 2 << 20;
    let mut img = random_bytes(size, 18);
    put(&mut img, 0, b"RAWS");
    put(&mut img, 65_536, &test_file(10_000, 1));
    let reg = registry().with(Arc::new(GreedyFmt));
    let src = mem(img);
    let (files, _) = carve(opts(reg), &src);
    assert_eq!(
        spans(&files),
        vec![("raws".into(), 0, 65_536), ("test".into(), 65_536, 10_000)]
    );
    assert_eq!(files[0].status, ExtentStatus::Truncated);
    assert_eq!(files[0].err_offset, Some(65_536));
}

#[test]
fn long_stream_is_one_file_measured_a_few_times_not_per_packet() {
    let size = 16 << 20;
    let mut img = random_bytes(size, 25);
    let packets = 20_000;
    put(&mut img, 4096, &stream(packets, 1));
    let after = 4096 + packets * 512;
    put(&mut img, after, &test_file(5000, 2));
    let strm = Arc::new(StreamFmt::default());
    let reg = FormatRegistry::new()
        .with(Arc::new(LenFmt::test()))
        .with(strm.clone());
    let src = mem(img);
    let mut o = opts(reg);
    o.workers = 8;
    o.chunk_size = 1 << 20;
    let (files, sum) = carve(o, &src);
    assert_eq!(
        spans(&files),
        vec![
            ("strm".into(), 4096, (packets * 512) as u64),
            ("test".into(), after as u64, 5000),
        ]
    );
    assert_eq!(files[0].status, ExtentStatus::UnknownLength);
    assert!(sum.candidates >= packets as u64);
    let measures = strm.measures.load(std::sync::atomic::Ordering::Relaxed);
    assert!(measures <= 3, "stream measured {measures} times");
}

#[test]
fn same_start_higher_priority_format_wins() {
    let high = LenFmt {
        id: "high",
        sigs: leak(vec![Signature::at_start(b"TST1").with_also(b"HIGH", 8)]),
        priority: 80,
        conf: 95,
        ..LenFmt::test()
    };
    let size = 1 << 20;
    let mut img = random_bytes(size, 19);
    let mut a = test_file(5000, 1);
    a[8..12].copy_from_slice(b"HIGH");
    put(&mut img, 8192, &a);
    put(&mut img, 65_536, &test_file(5000, 2));
    let reg = registry().with(Arc::new(high));
    let src = mem(img);
    let (files, _) = carve(opts(reg), &src);
    assert_eq!(
        spans(&files),
        vec![("high".into(), 8192, 5000), ("test".into(), 65_536, 5000)]
    );
}

#[test]
fn cluster_grid_marks_off_grid_headers() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 20);
    // data_start 4096, 8 KiB clusters.
    put(&mut img, 4096 + 8192 * 3, &test_file(3000, 1));
    put(&mut img, 4096 + 8192 * 10 + 512, &test_file(3000, 2));
    let src = mem(img);
    let mut o = opts(registry());
    o.cluster = Some((4096, 8192));
    let (files, _) = carve(o, &src);
    assert_eq!(files.len(), 2);
    assert!(files[0].aligned);
    assert_eq!(files[0].confidence, Confidence(90));
    assert!(!files[1].aligned);
    assert_eq!(files[1].confidence, Confidence(90 - PENALTY_OFF_CLUSTER));
}

#[test]
fn global_size_cap_limits_measurement() {
    let size = 4 << 20;
    let mut img = random_bytes(size, 21);
    put(&mut img, 0, &test_file(2 << 20, 1));
    let src = mem(img);
    let mut o = opts(registry());
    o.max_file_size = Some(1 << 20);
    let (files, _) = carve(o, &src);
    assert_eq!(spans(&files), vec![("test".into(), 0, 1 << 20)]);
    assert_eq!(files[0].status, ExtentStatus::Truncated);
}

#[test]
fn extract_and_hash_round_trip_including_holes() {
    let size = 1 << 20;
    let mut img = random_bytes(size, 22);
    let file = test_file(100_000, 1);
    put(&mut img, 8192, &file);
    let holes = RangeSet::from_range(Range::new(8192 + 20_480, 8192 + 24_576));
    let src: SharedSource = Arc::new(MemSource::new(img.clone()).with_unreadable(holes));
    let (files, _) = carve(opts(registry()), &src);
    assert_eq!(spans(&files), vec![("test".into(), 8192, 100_000)]);
    let f = &files[0];
    assert_eq!(f.unreadable.as_slice(), &[Range::new(20_480, 24_576)]);
    let mut out = Vec::new();
    let rep = Carver::extract(&src, f, &mut out).unwrap();
    assert_eq!(rep.bytes, 100_000);
    assert_eq!(rep.unreadable.as_slice(), &[Range::new(20_480, 24_576)]);
    let mut expect = file.clone();
    expect[20_480..24_576].fill(0);
    assert_eq!(out, expect);
    let h = Carver::hash(&src, f).unwrap();
    assert_eq!(h, blake3::hash(&expect).to_hex().to_string());

    // Without holes the hash is the plain content hash.
    let clean: SharedSource = Arc::new(MemSource::new(img));
    assert_eq!(
        Carver::hash(&clean, f).unwrap(),
        blake3::hash(&file).to_hex().to_string()
    );
}

#[test]
fn file_source_gives_same_result_as_memory() {
    let (img, want) = back_to_back(10 << 20, 4096, 30, 23);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.img");
    std::fs::write(&path, &img).unwrap();
    let fsrc: SharedSource = Arc::new(FileSource::open(&path).unwrap());
    let (files, _) = carve(opts(registry()), &fsrc);
    assert_eq!(spans(&files), want);
    let msrc = mem(img);
    let (mfiles, _) = carve(opts(registry()), &msrc);
    assert_eq!(files, mfiles);
}

#[test]
fn progress_is_reported_and_reaches_total() {
    let (img, _) = back_to_back(20 << 20, 0, 10, 24);
    let src = mem(img);
    let carver = Carver::new(opts(registry())).unwrap();
    let mut reports = Vec::new();
    let sum = carver
        .scan(&src, &mut |p| reports.push(p.clone()), &mut |_| true)
        .unwrap();
    assert!(reports.len() >= 3);
    assert!(reports.windows(2).all(|w| w[0].scanned <= w[1].scanned));
    let last = reports.last().unwrap();
    assert_eq!(last.scanned, 20 << 20);
    assert_eq!(last.total, 20 << 20);
    assert_eq!(last.files, sum.files);
}

#[test]
fn invalid_options_are_refused() {
    assert!(Carver::new(diskrescue_carve::CarveOptions::default()).is_err());
    let mut o = opts(registry());
    o.chunk_size = 100;
    assert!(Carver::new(o).is_err());
    let mut o = opts(registry());
    o.cluster = Some((0, 0));
    assert!(Carver::new(o).is_err());
    static EMPTY: &[Signature] = &[];
    let reg = FormatRegistry::new().with(Arc::new(SigOnly("none", EMPTY)));
    assert!(Carver::new(opts(reg)).is_err());
    // workers 0 means "all cores"; alignment 0 means 1.
    let mut o = opts(registry());
    o.workers = 0;
    o.alignment = 0;
    let c = Carver::new(o).unwrap();
    assert!(c.workers() >= 1);
}

#[test]
fn randomized_layouts_are_deterministic_across_configurations() {
    for seed in 0..6u64 {
        let size = 4 << 20;
        let mut img = random_bytes(size, seed + 100);
        let mut rng = Rng::new(seed + 1);
        let mut pos = 0usize;
        while pos < size - 400_000 {
            pos = (pos + rng.below(60_000) as usize).div_ceil(512) * 512;
            let tag = seed * 1_000_000 + pos as u64;
            let bytes = match rng.below(7) {
                0 => test_file(12 + rng.below(150_000) as usize, tag),
                1 => cont_file(
                    20_000 + rng.below(100_000) as usize,
                    &[
                        (4096, test_file(3000, tag + 1)),
                        (777, test_file(1000, tag + 2)),
                    ],
                    rng.below(2) == 0,
                    tag,
                ),
                2 => stream(2 + rng.below(100) as usize, tag),
                3 => zfmt_file(16 + rng.below(100_000) as usize, tag),
                4 => {
                    let mut v = random_bytes(64, tag);
                    v[..4].copy_from_slice(b"RAWS");
                    v
                }
                5 => {
                    // Unaligned file.
                    pos += 100;
                    test_file(12 + rng.below(20_000) as usize, tag)
                }
                _ => {
                    // A file whose end was overwritten by the next one.
                    let mut v = test_file(30_000, tag);
                    v.truncate(10_000 + rng.below(15_000) as usize);
                    v
                }
            };
            put(&mut img, pos, &bytes);
            pos += bytes.len();
        }
        let src = mem(img);
        for promote in [false, true] {
            let mut base: Option<Vec<diskrescue_carve::CarvedFile>> = None;
            for (workers, chunk, zero_skip) in [
                (1, 8 << 20, true),
                (12, 65_536, false),
                (4, 4096, true),
                (32, 1 << 20, true),
                (12, 65_536, false),
                (32, 1 << 20, true),
            ] {
                let mut o = opts(registry().with(Arc::new(GreedyFmt)));
                o.workers = workers;
                o.chunk_size = chunk;
                o.zero_skip = zero_skip;
                o.promote_nested = promote;
                let (files, _) = carve(o, &src);
                match &base {
                    None => {
                        assert!(!files.is_empty());
                        base = Some(files);
                    }
                    Some(b) => assert_eq!(
                        &files, b,
                        "seed {seed} promote {promote} workers {workers} chunk {chunk}"
                    ),
                }
            }
        }
    }
}

/// Mirrors how the recover pipeline drives the carver (struct literal,
/// `workers: 0`, cancel flag, `extract` into a writer), so an API change
/// that would break it fails here first.
#[test]
fn pipeline_style_usage_compiles_and_works() {
    use std::sync::atomic::AtomicBool;
    let size = 2 << 20;
    let mut img = random_bytes(size, 26);
    put(&mut img, 8192, &test_file(50_000, 1));
    let src = mem(img);
    let cancel = Arc::new(AtomicBool::new(false));
    let carver = Carver::new(diskrescue_carve::CarveOptions {
        registry: registry(),
        mode: CarveMode::Unallocated(RangeSet::from_range(Range::new(0, 1 << 20))),
        alignment: 512,
        cluster: Some((4096, 4096_u32 as u64)),
        chunk_size: 64 << 20,
        probe_window: 64 << 10,
        workers: 0,
        min_confidence: Confidence::LOW,
        max_file_size: None,
        promote_nested: false,
        zero_skip: true,
        cancel: Some(cancel.clone()),
    })
    .unwrap();
    let total = RangeSet::from_range(Range::new(0, 1 << 20)).total_len();
    let mut last = (0u64, 0u64);
    let mut records = Vec::new();
    let summary = carver
        .scan(
            &src,
            &mut |p: &diskrescue_carve::ScanProgress| last = (p.scanned, p.files),
            &mut |cf: diskrescue_carve::CarvedFile| {
                let mut w: Vec<u8> = Vec::new();
                let er = Carver::extract(&src, &cf, &mut w).unwrap();
                records.push((cf.start, cf.len, cf.status, cf.parent, er.bytes, w.len()));
                true
            },
        )
        .unwrap();
    assert_eq!(
        records,
        vec![(8192, 50_000, ExtentStatus::Complete, None, 50_000, 50_000)]
    );
    assert_eq!(summary.scanned, total);
    assert_eq!(last, (total, 1));
    assert_eq!(
        (summary.candidates, summary.files, summary.false_positives),
        (1, 1, 0)
    );
}
