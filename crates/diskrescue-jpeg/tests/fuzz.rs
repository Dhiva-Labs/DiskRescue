//! 5000 mutated realistic files through every public entry point: no
//! panic may escape (zune-jpeg runs behind a panic guard), every extent
//! stays inside the limit and the source, and "Valid" is never reported
//! for data the analyzer found an error in.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use common::*;
use diskrescue_jpeg::core::format::{FileFormat, RepairContext, RepairResult, ValidationStatus};
use diskrescue_jpeg::core::source::{MemSource, SharedSource};
use diskrescue_jpeg::core::{Range, RangeSet};
use diskrescue_jpeg::*;

const SMALL: [&str; 9] = [
    "small_cam_mpf_rst.jpg",
    "small_phone_gain_motion.jpg",
    "small_ps_prog_rst.jpg",
    "mj_411.jpg",
    "mj_440_rst.jpg",
    "mj_12bit_gray.jpg",
    "ps_cmyk_progressive.jpg",
    "cam_progressive_rst1.jpg",
    "mj_dering_q70.jpg",
];

fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut d = base.to_vec();
    for _ in 0..1 + rng.below(3) {
        if d.len() < 8 {
            break;
        }
        let len = d.len() as u64;
        match rng.below(10) {
            0 => {
                for _ in 0..1 + rng.below(8) {
                    let p = rng.below(len) as usize;
                    d[p] = rng.next() as u8;
                }
            }
            1 => d.truncate(rng.below(len) as usize),
            2 => {
                let p = rng.below(len) as usize;
                let n = 1 + rng.below(300) as usize;
                let junk = rng.bytes(n);
                d.splice(p..p, junk);
            }
            3 => {
                // An absurd length field somewhere in the first 64 KiB.
                let p = 2 + rng.below(len.min(65536) - 2) as usize;
                if p + 1 < d.len() {
                    let v = [0x00, 0x01, 0x02, 0xFF][rng.below(4) as usize];
                    d[p] = v;
                    d[p + 1] = rng.next() as u8;
                }
            }
            4 => {
                let p = rng.below(len) as usize;
                let n = (rng.below(8192) as usize).min(d.len() - p);
                let fill = [0x00, 0xFF, 0x55][rng.below(3) as usize];
                d[p..p + n].fill(fill);
            }
            5 => {
                // A marker where it does not belong.
                let p = rng.below(len - 1) as usize;
                d[p] = 0xFF;
                d[p + 1] = [
                    0xD8, 0xD9, 0xDA, 0xC4, 0xDB, 0xC0, 0xC2, 0xDD, 0xD0, 0xE1, 0x01, 0x00,
                ][rng.below(12) as usize];
            }
            6 => {
                // Delete a chunk (a lost cluster).
                let p = rng.below(len) as usize;
                let n = (rng.below(4096) as usize).min(d.len() - p);
                d.drain(p..p + n);
            }
            7 => {
                // Duplicate a chunk.
                let p = rng.below(len) as usize;
                let n = (rng.below(2048) as usize).min(d.len() - p);
                let chunk = d[p..p + n].to_vec();
                let q = rng.below(d.len() as u64) as usize;
                d.splice(q..q, chunk);
            }
            8 => {
                // Frame dimensions / sampling / table selectors.
                if let Some(i) = d
                    .windows(2)
                    .position(|w| w[0] == 0xFF && matches!(w[1], 0xC0..=0xC2))
                {
                    let k = i + 5 + rng.below(12) as usize;
                    if k < d.len() {
                        d[k] = rng.next() as u8;
                    }
                }
            }
            _ => {
                // Scan header fields.
                if let Some(i) = d.windows(2).position(|w| w == [0xFF, 0xDA]) {
                    let k = i + 4 + rng.below(10) as usize;
                    if k < d.len() {
                        d[k] = rng.next() as u8;
                    }
                }
            }
        }
    }
    d
}

fn exercise(d: &[u8], donors: &[std::path::PathBuf], rng: &mut Rng) -> Vec<String> {
    let mut problems = Vec::new();
    let f = JpegFormat::new();
    let _ = f.probe(d);
    let src = mem(d.to_vec());
    for limit in [u64::MAX, (d.len() / 3) as u64, 1000] {
        if let Some(m) = f.measure(&src, 0, limit).unwrap() {
            if m.len > limit.min(d.len() as u64) {
                problems.push(format!("measure len {} > limit {limit}", m.len));
            }
            for n in &m.nested {
                if n.range.end > m.len || n.range.start > n.range.end {
                    problems.push(format!("nested {:?} outside len {}", n.range, m.len));
                }
            }
            if let Some(e) = m.err_offset {
                if e > d.len() as u64 {
                    problems.push(format!("err_offset {e} past the data"));
                }
            }
        }
    }
    let _ = walk_headers(&*src, 0, u64::MAX);
    let a = analyze(d);
    // Every other case on a source with a bad-sector hole.
    let src: SharedSource = if rng.below(2) == 0 {
        src
    } else {
        let hole = rng.below(d.len().max(1) as u64);
        Arc::new(
            MemSource::new(d.to_vec())
                .with_unreadable(RangeSet::from_range(Range::with_len(hole, 512))),
        )
    };
    let _ = f.measure(&src, 0, u64::MAX).unwrap();
    let v = f.validate(&src).unwrap();
    let holed = src.unreadable().is_some();
    if let Ok(a) = &a {
        if a.first_error.is_some() && v.status == ValidationStatus::Valid {
            problems.push(format!("Valid despite {:?}", a.first_error));
        }
        if a.mcus_decoded > a.mcus_total || a.mcu_rows_decoded > a.mcu_rows_total {
            problems.push("decoded more than the total".into());
        }
    }
    if let Some(i) = d.windows(4).position(|w| w == b"Exif") {
        let _ = parse_app1(&d[i..], i as u64);
        let _ = parse_app1(&d[i..], u64::MAX - 3);
    }
    // Repairs are the expensive part (re-validation, donor scoring): one
    // context and one single strategy per case keeps 5000 cases quick.
    let ctx = match rng.below(3) {
        0 => RepairContext::default(),
        1 => RepairContext {
            aggressive: true,
            ..Default::default()
        },
        _ => RepairContext {
            aggressive: true,
            donors: donors.to_vec(),
            ..Default::default()
        },
    };
    let mut out = Vec::new();
    let r = match f.repair(&src, &mut out, &ctx) {
        Ok(r) => r,
        // Copying from a live source with a bad sector reports the read
        // error (nothing else may fail).
        Err(e) if holed && e.is_read_error() => return problems,
        Err(e) => {
            problems.push(format!("repair failed: {e}"));
            return problems;
        }
    };
    if r.output_len != out.len() as u64 {
        problems.push("output_len mismatch".into());
    }
    if r.result == RepairResult::Unrepairable && !out.is_empty() {
        problems.push("unrepairable but wrote output".into());
    }
    // A repaired file must not trip the reference decoder (outputs that keep
    // damaged data, PartiallyRepaired, may: its panic guard catches that).
    if r.result == RepairResult::Repaired
        && zune_check(&out).error.as_deref() == Some("decoder panicked")
    {
        problems.push("repaired output makes zune-jpeg panic".into());
    }
    let s = [
        Strategy::StructuralFix,
        Strategy::AppendEoi,
        Strategy::TruncateAtLastGoodRestart,
        Strategy::TruncateAtLastGoodMcuRow,
        Strategy::InsertStandardHuffmanTables,
        Strategy::StripTrailingGarbage,
        Strategy::ReplaceDamagedIntervals,
    ][rng.below(7) as usize]
        .clone();
    let mut out = Vec::new();
    if let Ok(r) = apply(&src, &mut out, &[s], &RepairContext::default()) {
        if r.output_len != out.len() as u64 {
            problems.push("apply output_len mismatch".into());
        }
    }
    problems
}

#[test]
fn five_thousand_mutations_never_panic() {
    let Some(dir) = camera_fixtures() else { return };
    let files: Vec<Vec<u8>> = SMALL.iter().map(|n| read(dir, n)).collect();
    let donors = vec![dir.join("small_cam_mpf_rst.jpg"), dir.join("mj_411.jpg")];
    let start = Instant::now();
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    // Four workers; every case is reproducible from its round number.
    const ROUNDS: u64 = 5000;
    const WORKERS: u64 = 4;
    let mut failures: Vec<(u64, String, Vec<u8>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..WORKERS)
            .map(|w| {
                let (files, donors) = (&files, &donors);
                s.spawn(move || {
                    let mut failures = Vec::new();
                    for round in (w..ROUNDS).step_by(WORKERS as usize) {
                        let base = &files[(round % files.len() as u64) as usize];
                        let d = mutate(base, &mut Rng(0xF022_5EED ^ (round << 8) ^ round));
                        let mut case_rng = Rng(round + 1);
                        let t0 = Instant::now();
                        let res =
                            catch_unwind(AssertUnwindSafe(|| exercise(&d, donors, &mut case_rng)));
                        if t0.elapsed().as_secs_f32() > 2.0 {
                            eprintln!("round {round}: {:?}", t0.elapsed());
                        }
                        match res {
                            Ok(p) if p.is_empty() => {}
                            Ok(p) => failures.push((round, p.join("; "), d)),
                            Err(e) => {
                                let msg = e
                                    .downcast_ref::<String>()
                                    .cloned()
                                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                                    .unwrap_or_default();
                                failures.push((round, format!("PANIC {msg}"), d));
                            }
                        }
                    }
                    failures
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    failures.sort_by_key(|f| f.0);
    std::panic::set_hook(quiet);
    eprintln!("{ROUNDS} mutations in {:?}", start.elapsed());
    if let Some((round, _, d)) = failures.first() {
        let p = std::env::temp_dir().join(format!("jpeg-fuzz-failure-{round}.jpg"));
        let _ = std::fs::write(&p, d);
        eprintln!("first failing input saved to {}", p.display());
    }
    let report: Vec<String> = failures
        .iter()
        .take(20)
        .map(|(r, m, _)| format!("round {r}: {m}"))
        .collect();
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        report.join("\n")
    );
}

/// A mutated progressive file (truncated inside a scan, junk marker
/// `FF 55` after it) on which zune-jpeg 0.4's lenient decoder loops
/// forever when the data has no EOI: validation, repair and the public
/// `zune_check` must all return, quickly.
#[test]
fn reference_decoder_hang_is_contained() {
    let d = committed("zune_hang_progressive.jpg");
    let (tx, rx) = std::sync::mpsc::channel();
    let data = d.clone();
    std::thread::spawn(move || {
        let f = JpegFormat::new();
        let z = zune_check(&data);
        let v = f.validate(&mem(data.clone())).unwrap();
        let mut out = Vec::new();
        let r = f
            .repair(&mem(data), &mut out, &RepairContext::default())
            .unwrap();
        let _ = tx.send((z, v, r, out));
    });
    let (z, v, r, out) = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the reference decoder hangs");
    assert!(z.lenient_ok && !z.strict_ok, "{z:?}");
    assert_eq!(v.status, ValidationStatus::Damaged, "{:?}", v.problems);
    assert_ne!(r.result, RepairResult::Unrepairable, "{r:?}");
    assert!(zune_dims(&out).is_some());
}
