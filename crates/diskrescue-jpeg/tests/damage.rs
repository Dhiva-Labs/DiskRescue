//! Damaged files: truncation, flipped bytes, bad-sector holes.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_jpeg::core::format::{ExtentStatus, FileFormat, ValidationStatus};
use diskrescue_jpeg::core::source::{MemSource, ReadError, SharedSource, Source};
use diskrescue_jpeg::core::{Range, RangeSet};
use diskrescue_jpeg::scan::ScanErrorKind;
use diskrescue_jpeg::*;

/// First entropy byte and EOI offset of a clean file.
fn scan_span(d: &[u8]) -> (u64, u64) {
    let h = walk_headers(&*mem(d.to_vec()), 0, d.len() as u64).unwrap();
    let a = analyze(d).unwrap();
    (h.scan_data_offset.unwrap(), a.eoi_offset.unwrap())
}

#[test]
fn truncation_is_eof_with_proportional_rows() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    for n in ["noise_1024x768.jpg", "noise_rst_1024x768.jpg"] {
        let d = read(dir, n);
        let (sdo, eoi) = scan_span(&d);
        let rows = analyze(&d).unwrap().mcu_rows_total as u64;
        for pct in [30u64, 60, 90] {
            let cut = d.len() * pct as usize / 100;
            let a = analyze(&d[..cut]).unwrap();
            let e = a
                .first_error
                .unwrap_or_else(|| panic!("{n} {pct}%: no error"));
            assert_eq!(e.kind, ScanErrorKind::Eof, "{n} {pct}%");
            assert!(a.eoi_offset.is_none());
            let expected = rows * (cut as u64 - sdo) / (eoi - sdo);
            let got = a.mcu_rows_decoded as i64;
            assert!(
                (got - expected as i64).abs() <= 2,
                "{n} {pct}%: {got} rows decoded, expected about {expected}"
            );
            // measure: truncated at the end of the data.
            let m = f
                .measure(&mem(d[..cut].to_vec()), 0, u64::MAX)
                .unwrap()
                .unwrap();
            assert_eq!(m.status, ExtentStatus::Truncated, "{n} {pct}%");
            assert_eq!(m.len, cut as u64);
            assert!(m.err_offset.is_some());
            // validate: damaged, with the decodable share.
            let v = f.validate(&mem(d[..cut].to_vec())).unwrap();
            assert_eq!(v.status, ValidationStatus::Damaged);
            assert!(
                v.problems.iter().any(|p| p.starts_with("truncated:")),
                "{:?}",
                v.problems
            );
            let frac = v.intact_fraction.unwrap();
            assert!(
                (frac - expected as f32 / rows as f32).abs() < 0.06,
                "{n} {pct}%: {frac}"
            );
            assert!(v
                .repairable_by
                .iter()
                .any(|r| r == "append_eoi" || r.starts_with("truncate")));
        }
    }
}

#[test]
fn truncated_progressive_keeps_the_whole_image() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let d = read(dir, "prog_noise_1024x768.jpg");
    let full = analyze(&d).unwrap();
    for pct in [50usize, 80] {
        let cut = d.len() * pct / 100;
        let a = analyze(&d[..cut]).unwrap();
        assert_eq!(a.first_error.unwrap().kind, ScanErrorKind::Eof);
        // The DC scans come first: every row is visible.
        assert_eq!(a.mcu_rows_decoded, a.mcu_rows_total);
        let complete = a.scans.iter().filter(|s| s.complete).count();
        assert!(complete < full.scans.len());
        assert!(a.last_complete_scan_end.is_some());
        let v = f.validate(&mem(d[..cut].to_vec())).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
        assert!(
            v.problems.iter().any(|p| p.contains("reduced quality")),
            "{:?}",
            v.problems
        );
    }
}

/// XOR 4 bytes at `at` so that no `FF` is created or destroyed (a new
/// marker would move the damage before `at`).
fn flip4(d: &[u8], at: usize, rng: &mut Rng) -> Option<Vec<u8>> {
    if d[at - 1..at + 5].contains(&0xFF) {
        return None;
    }
    let mut x = d.to_vec();
    for b in &mut x[at..at + 4] {
        let v = *b ^ (1 + rng.below(254) as u8);
        if v == 0xFF {
            return None;
        }
        *b = v;
    }
    Some(x)
}

#[test]
fn flipped_bytes_without_restart_markers() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "noise_1024x768.jpg");
    let mut rng = Rng(42);
    let mut lags = Vec::new();
    let (mut tried, mut found, mut close) = (0, 0, 0);
    for t in 0..40usize {
        let at = d.len() / 2 + t * 211;
        let Some(x) = flip4(&d, at, &mut rng) else {
            continue;
        };
        tried += 1;
        let a = analyze(&x).unwrap();
        let Some(e) = a.first_error else {
            continue;
        };
        found += 1;
        // The analyzer never blames bytes before the damage.
        assert!(
            e.byte_offset >= at as u64,
            "error at {} before flip at {at}",
            e.byte_offset
        );
        if e.byte_offset <= at as u64 + 64 {
            close += 1;
        }
        lags.push(e.byte_offset - at as u64);
    }
    lags.sort_unstable();
    eprintln!(
        "no restart markers: {found}/{tried} detected, {close} within 64 bytes, lags {lags:?}"
    );
    assert!(tried >= 30);
    assert!(found * 10 >= tried * 9, "{found}/{tried} detected");
    // Without restart markers the decoder can run on garbage for a while;
    // on this image an MCU is about 230 bytes, so most errors land within
    // two MCUs of the flip and a few surface much later.
    let near = lags.iter().filter(|&&l| l <= 512).count();
    assert!(
        near * 10 >= found * 8,
        "{near}/{found} within 512 bytes: {lags:?}"
    );
    assert!(close >= 1);
}

#[test]
fn flipped_bytes_with_restart_markers_are_located_by_interval() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let d = read(dir, "noise_rst_1024x768.jpg");
    let ri = analyze(&d).unwrap().restart_interval as u64;
    assert!(ri > 0);
    let rst = rst_offsets(&d);
    let mut rng = Rng(7);
    let (mut tried, mut found) = (0, 0);
    for t in 0..40usize {
        let at = d.len() / 2 + t * 307;
        let Some(x) = flip4(&d, at, &mut rng) else {
            continue;
        };
        tried += 1;
        let a = analyze(&x).unwrap();
        let Some(e) = a.first_error else {
            continue;
        };
        found += 1;
        let flip_interval = rst.iter().filter(|&&o| o < at).count() as i64;
        let err_interval = (e.mcu_index / ri) as i64;
        assert!(
            (err_interval - flip_interval).abs() <= 2,
            "flip in interval {flip_interval}, error in interval {err_interval} ({e:?})"
        );
        assert!(e.byte_offset >= at as u64);
        // Decoding resumes at the next restart marker.
        assert!(a.corrupt_intervals >= 1);
        assert!(a.mcus_decoded > a.mcus_total * 9 / 10);
        assert!(a.eoi_offset.is_some());
        let (ok, rows) = (a.last_good_restart.unwrap(), a.mcu_rows_total);
        assert!(ok.1 <= e.mcu_index && rows > 0);
        if t == 0 {
            let v = f.validate(&mem(x.clone())).unwrap();
            assert_eq!(v.status, ValidationStatus::Damaged);
            assert!(
                v.problems.iter().any(|p| p.contains("restart interval")),
                "{:?}",
                v.problems
            );
            let m = f.measure(&mem(x), 0, u64::MAX).unwrap().unwrap();
            assert_eq!(m.status, ExtentStatus::Complete);
            assert_eq!(m.len, d.len() as u64);
            assert!(m.err_offset.is_some());
        }
    }
    assert!(found * 10 >= tried * 9, "{found}/{tried} detected");
}

#[test]
fn flip_exactly_in_the_middle() {
    let Some(dir) = fixtures() else { return };
    for n in [
        "noise_1024x768.jpg",
        "noise_rst_1024x768.jpg",
        "photo_1024x768.jpg",
    ] {
        let d = read(dir, n);
        let mut at = d.len() / 2;
        let mut rng = Rng(1);
        let x = loop {
            if let Some(x) = flip4(&d, at, &mut rng) {
                break x;
            }
            at += 1;
        };
        let a = analyze(&x).unwrap();
        if let Some(e) = a.first_error {
            assert!(e.byte_offset >= at as u64, "{n}: {e:?} before {at}");
            assert_ne!(e.kind, ScanErrorKind::Eof, "{n}");
        }
    }
}

/// A rescued image: bad sectors read as zeros and are listed as unreadable.
struct HoleSource {
    data: MemSource,
    holes: RangeSet,
}

impl Source for HoleSource {
    fn size(&self) -> u64 {
        self.data.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.data.read_at(offset, buf)
    }
    fn describe(&self) -> String {
        "image with holes".into()
    }
    fn unreadable(&self) -> Option<RangeSet> {
        Some(self.holes.clone())
    }
}

#[test]
fn bad_sector_holes_are_reported_where_they_are() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    for n in ["noise_rst_1024x768.jpg", "photo_1024x768.jpg"] {
        let d = read(dir, n);
        let start = 4096;
        let mut image = vec![0u8; start];
        image.extend_from_slice(&d);
        image.resize(image.len() + 4096, 0);
        let hole = Range::with_len((start + d.len() * 2 / 5) as u64 & !511, 4096);
        image[hole.start as usize..hole.end as usize].fill(0);
        let src: SharedSource = Arc::new(HoleSource {
            data: MemSource::new(image.clone()),
            holes: RangeSet::from_range(hole),
        });
        let m = f.measure(&src, start as u64, u64::MAX).unwrap().unwrap();
        let rel = hole.start - start as u64;
        assert_eq!(m.err_offset, Some(rel), "{n}: {:?}", m.notes);
        assert_eq!(m.len, d.len() as u64, "{n}");
        assert_eq!(
            m.meta.extra.get("unreadable_bytes").map(String::as_str),
            Some("4096")
        );
        let file: SharedSource = Arc::new(HoleSource {
            data: MemSource::new(image[start..start + d.len()].to_vec()),
            holes: RangeSet::from_range(Range::with_len(rel, 4096)),
        });
        let v = f.validate(&file).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged, "{n}");
        if n.contains("rst") {
            // Restart markers let decoding continue after the hole.
            assert!(
                v.intact_fraction.unwrap() > 0.9,
                "{n}: {:?}",
                v.intact_fraction
            );
        }
        // A source whose reads fail inside the hole ends the data there.
        let failing: SharedSource =
            Arc::new(MemSource::new(image.clone()).with_unreadable(RangeSet::from_range(hole)));
        let m = f
            .measure(&failing, start as u64, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated, "{n}");
        assert_eq!(m.len, rel);
        assert!(
            m.notes.iter().any(|x| x.contains("unreadable")),
            "{:?}",
            m.notes
        );
    }
}

#[test]
fn next_file_after_a_truncated_jpeg_ends_it() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let d = read(dir, "photo_1024x768.jpg");
    let other = read(dir, "base_444_320x240.jpg");
    let cut = d.len() / 2;
    let mut buf = d[..cut].to_vec();
    buf.extend_from_slice(&other);
    let m = f.measure(&mem(buf), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut as u64 + 2, "{} vs cut {cut}", m.len);
    assert!(m.len + 64 >= cut as u64);
}
