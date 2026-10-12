//! Scanner throughput with twenty registered formats.
//!
//! `scan_64mib_with_twenty_formats` runs in every test pass and only checks
//! correctness (and prints the rate). The 1 GiB benchmark is ignored by
//! default; run it in release mode:
//!
//! ```text
//! cargo test -p diskrescue-carve --release --test throughput -- --ignored --nocapture
//! ```

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::*;
use diskrescue_carve::Carver;
use diskrescue_core::source::MemSource;
use diskrescue_core::SharedSource;

fn scan_rate(src: &SharedSource, chunk: usize, extra: bool) -> (f64, u64, u64) {
    let mut reg = twenty_formats();
    if extra {
        reg.register(Arc::new(LenFmt::test()));
    }
    let mut o = opts(reg);
    o.chunk_size = chunk;
    o.workers = 0;
    let carver = Carver::new(o).unwrap();
    let t = Instant::now();
    let mut files = 0u64;
    let sum = carver
        .scan(src, &mut |_| {}, &mut |_| {
            files += 1;
            true
        })
        .unwrap();
    let secs = t.elapsed().as_secs_f64();
    (src.size() as f64 / secs, sum.candidates, files)
}

#[test]
fn scan_64mib_with_twenty_formats() {
    let size = 64 << 20;
    let mut img = random_bytes(size, 1234);
    for i in 0..8usize {
        put(
            &mut img,
            i * (8 << 20) + 4096,
            &test_file(100_000, i as u64),
        );
    }
    let src: SharedSource = Arc::new(MemSource::new(img));
    let (rate, candidates, files) = scan_rate(&src, 8 << 20, true);
    println!(
        "64 MiB random image, 21 formats: {:.0} MB/s ({candidates} candidates, {files} files)",
        rate / 1e6
    );
    assert_eq!(files, 8);
}

#[test]
#[ignore = "benchmark: run in release mode with --ignored --nocapture"]
fn scan_1gib_random_over_1gbps() {
    let size = 1usize << 30;
    let src: SharedSource = Arc::new(MemSource::new(random_bytes(size, 4321)));
    // Warm-up pass (page faults, thread start-up), then three measured runs.
    let _ = scan_rate(&src, 64 << 20, false);
    let mut best = 0f64;
    for _ in 0..3 {
        let (rate, candidates, files) = scan_rate(&src, 64 << 20, false);
        println!(
            "1 GiB random image, 20 formats: {:.0} MB/s ({candidates} candidates, {files} files)",
            rate / 1e6
        );
        best = best.max(rate);
        assert_eq!(files, 0);
    }
    println!("best: {:.2} GB/s", best / 1e9);
    // For context (not asserted): deep scan and an all-zero image.
    let mut reg = twenty_formats();
    reg.register(Arc::new(LenFmt::test()));
    let mut o = opts(reg);
    o.chunk_size = 64 << 20;
    o.workers = 0;
    o.alignment = 1;
    let carver = Carver::new(o).unwrap();
    let t = Instant::now();
    let sum = carver.scan(&src, &mut |_| {}, &mut |_| true).unwrap();
    println!(
        "1 GiB random image, alignment 1: {:.0} MB/s ({} candidates)",
        size as f64 / t.elapsed().as_secs_f64() / 1e6,
        sum.candidates
    );
    drop(src);
    // Written explicitly so the pages are resident (a fresh `vec![0; n]` is
    // lazily mapped and would measure page faults, not the scanner).
    let mut z = vec![1u8; size];
    z.fill(0);
    let zero: SharedSource = Arc::new(MemSource::new(z));
    let _ = scan_rate(&zero, 64 << 20, false);
    let (rate, _, _) = scan_rate(&zero, 64 << 20, false);
    println!("1 GiB zero image: {:.0} MB/s", rate / 1e6);
    if cfg!(debug_assertions) {
        eprintln!("debug build: rate not asserted");
    } else {
        assert!(
            best > 1e9,
            "scanner rate {:.2} GB/s is below 1 GB/s",
            best / 1e9
        );
    }
}
