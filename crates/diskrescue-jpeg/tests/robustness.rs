//! Hostile input: random bytes, plausible-looking garbage, mutated real
//! files and absurd sizes must never panic; large files are streamed.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use common::*;
use diskrescue_jpeg::core::format::{
    ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_jpeg::core::source::{FileSource, SharedSource};
use diskrescue_jpeg::*;

/// Call every public entry point on `d`; only panics fail the test.
fn exercise(d: &[u8], donors: &[std::path::PathBuf]) {
    let f = JpegFormat::new();
    let _ = f.probe(d);
    let src = mem(d.to_vec());
    for limit in [u64::MAX, (d.len() / 2) as u64, 300] {
        if let Some(m) = f.measure(&src, 0, limit).unwrap() {
            assert!(
                m.len <= limit.min(d.len() as u64),
                "len {} > limit {limit}",
                m.len
            );
            for n in &m.nested {
                assert!(n.range.end <= m.len);
            }
        }
    }
    let _ = walk_headers(&*src, 0, u64::MAX);
    let _ = analyze(d);
    let _ = parse_app1(d, 0);
    let v = f.validate(&src).unwrap();
    if v.status == ValidationStatus::Valid {
        assert!(v.intact_fraction.unwrap() > 0.0);
    }
    for ctx in [
        RepairContext::default(),
        RepairContext {
            aggressive: true,
            donors: donors.to_vec(),
            ..Default::default()
        },
    ] {
        let mut out = Vec::new();
        let r = f.repair(&src, &mut out, &ctx).unwrap();
        assert_eq!(r.output_len, out.len() as u64);
        if r.result == RepairResult::Unrepairable {
            assert!(out.is_empty());
        }
    }
}

#[test]
fn random_bytes_are_rejected() {
    let f = JpegFormat::new();
    let mut rng = Rng(0xC0FFEE);
    for i in 0..300 {
        let len = 1 + rng.below(if i % 10 == 0 { 70_000 } else { 3000 }) as usize;
        let mut d = rng.bytes(len);
        if d.starts_with(&[0xFF, 0xD8]) {
            d[0] = 0;
        }
        assert!(!f.probe(&d));
        assert!(f.measure(&mem(d.clone()), 0, u64::MAX).unwrap().is_none());
        assert!(analyze(&d).is_err());
        assert!(walk_headers(&*mem(d.clone()), 0, u64::MAX).is_err());
        let v = f.validate(&mem(d.clone())).unwrap();
        assert_ne!(v.status, ValidationStatus::Valid);
        let mut out = Vec::new();
        let r = f
            .repair(&mem(d), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::Unrepairable);
        assert!(out.is_empty());
    }
}

#[test]
fn soi_followed_by_garbage_is_rejected() {
    let f = JpegFormat::new();
    let mut rng = Rng(0xBAD5EED);
    let mut probed = 0;
    for i in 0..1000 {
        let mut d = vec![0xFF, 0xD8, 0xFF];
        let len = 1 + rng.below(if i % 50 == 0 { 100_000 } else { 4000 }) as usize;
        d.extend(rng.bytes(len));
        if f.probe(&d) {
            probed += 1;
        }
        assert!(
            f.measure(&mem(d.clone()), 0, u64::MAX).unwrap().is_none(),
            "garbage accepted (seed round {i})"
        );
        if i % 10 == 0 {
            exercise(&d, &[]);
        }
    }
    // The probe already rejects most of them.
    assert!(probed < 200, "{probed} of 1000 passed the probe");
}

#[test]
fn mutated_real_files_never_panic() {
    let mut files: Vec<Vec<u8>> = COMMITTED.iter().map(|n| committed(n)).collect();
    let mut donors = vec![committed_path("small_gray.jpg")];
    if let Some(dir) = fixtures() {
        for n in [
            "tiny_64x48.jpg",
            "prog_gray_64x48.jpg",
            "gray_rst_96x64.jpg",
            "odd_prog_203x77.jpg",
        ] {
            files.push(read(dir, n));
        }
        donors.push(dir.join("tiny_64x48.jpg"));
    }
    let mut rng = Rng(0x5EED);
    let start = Instant::now();
    for round in 0..2000 {
        let base = &files[round % files.len()];
        let mut d = base.clone();
        match rng.below(5) {
            0 => {
                for _ in 0..1 + rng.below(16) {
                    let p = rng.below(d.len() as u64) as usize;
                    d[p] = rng.next() as u8;
                }
            }
            1 => d.truncate(rng.below(d.len() as u64) as usize),
            2 => {
                let p = rng.below(d.len() as u64) as usize;
                let n = 1 + rng.below(64) as usize;
                let junk = rng.bytes(n);
                d.splice(p..p, junk);
            }
            3 => {
                // Overwrite a length field with an absurd value.
                let p = 2 + rng.below(d.len().min(600) as u64 - 2) as usize;
                if p + 1 < d.len() {
                    d[p] = 0xFF;
                    d[p + 1] = 0xFF;
                }
            }
            _ => {
                let p = rng.below(d.len() as u64) as usize;
                let n = (rng.below(4096) as usize).min(d.len() - p);
                d[p..p + n].fill(if rng.below(2) == 0 { 0 } else { 0xFF });
            }
        }
        exercise(&d, &donors);
    }
    eprintln!("2000 mutations in {:?}", start.elapsed());
}

#[test]
fn absurd_header_values_do_not_explode() {
    let base = committed("small_restart.jpg");
    let h = walk_headers(&*mem(base.clone()), 0, u64::MAX).unwrap();
    let sof = h.sof.unwrap().offset as usize;
    let mut cases = Vec::new();
    // 65535 x 65535 frame (rejected: more than 2^31 pixels).
    let mut d = base.clone();
    d[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    cases.push(d);
    // 46000 x 46000 frame with the original tiny scan.
    let mut d = base.clone();
    d[sof + 5..sof + 9].copy_from_slice(&[0xB3, 0xB0, 0xB3, 0xB0]);
    cases.push(d);
    // Height 0 (DNL).
    let mut d = base.clone();
    d[sof + 5..sof + 7].copy_from_slice(&[0, 0]);
    cases.push(d);
    // Every segment length set to 0xFFFF in turn.
    for s in &h.segments {
        let mut d = base.clone();
        let o = s.offset as usize;
        d[o + 2..o + 4].copy_from_slice(&[0xFF, 0xFF]);
        cases.push(d);
    }
    // Restart interval of 1 on a huge frame.
    let mut d = base.clone();
    d[sof + 5..sof + 9].copy_from_slice(&[0x40, 0x00, 0x40, 0x00]);
    cases.push(d);
    let start = Instant::now();
    for d in &cases {
        exercise(d, &[]);
    }
    assert!(
        start.elapsed().as_secs() < 30,
        "too slow: {:?}",
        start.elapsed()
    );
}

#[test]
fn large_file_is_measured_in_bounded_reads() {
    let Some(dir) = large_fixture() else { return };
    let path = dir.join("large.jpg");
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len > 20 << 20, "fixture is only {len} bytes");
    let file: SharedSource = Arc::new(FileSource::open(&path).unwrap());
    let counting = Arc::new(CountingSource::new(file));
    let src: SharedSource = counting.clone();
    let f = JpegFormat::new();
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, len);
    assert_eq!(m.status, ExtentStatus::Complete);
    let max = counting.max_read.load(Ordering::Relaxed);
    let total = counting.total.load(Ordering::Relaxed);
    assert!(max <= 1 << 20, "largest read {max} bytes");
    // One pass over the file (plus a few small re-reads).
    assert!(total <= len + (1 << 20), "read {total} bytes of {len}");
    // The streaming analyzer agrees.
    let (_, a) = analyze_source(&*src, 0, u64::MAX, &AnalyzeOptions::default()).unwrap();
    assert!(a.is_clean());
    assert!(counting.max_read.load(Ordering::Relaxed) <= 1 << 20);
}
