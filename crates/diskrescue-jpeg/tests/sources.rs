//! Sources that misbehave the way failing media do: short reads, timeouts
//! and a device that disappears mid-read, on 512- and 4096-byte sectors.
//! Results must equal those on a healthy source (short reads), or report
//! the damage where it is (errors) - never "Valid" and never a panic.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use diskrescue_jpeg::core::format::{
    ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_jpeg::core::source::{MemSource, ReadError, ReadErrorKind, SharedSource, Source};
use diskrescue_jpeg::core::{Range, RangeSet};
use diskrescue_jpeg::*;

/// Returns at most `max` bytes per read (a different amount every call).
struct ShortReads {
    data: Vec<u8>,
    max: usize,
    calls: AtomicU64,
}

impl Source for ShortReads {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.size() {
            return Ok(0);
        }
        let c = self.calls.fetch_add(1, Ordering::Relaxed);
        let n = buf
            .len()
            .min(1 + (c as usize * 7919) % self.max)
            .min(self.data.len() - offset as usize);
        buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "short reads".into()
    }
}

/// Fails every read that reaches `bad` with `kind` (a timeout, or a device
/// that is gone from then on); reads before it succeed, the last one
/// short. Sector size is configurable.
struct Failing {
    data: Vec<u8>,
    bad: u64,
    kind: ReadErrorKind,
    sector: u32,
}

impl Source for Failing {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.size() {
            return Ok(0);
        }
        let gone = self.kind == ReadErrorKind::DeviceGone;
        if offset >= self.bad || (gone && offset >= self.bad) {
            return Err(ReadError::new(
                offset.max(self.bad),
                buf.len(),
                self.kind,
                "simulated",
            ));
        }
        let end = (offset + buf.len() as u64).min(self.size());
        if end > self.bad {
            // Good prefix, the next read reports the error.
            let n = (self.bad - offset) as usize;
            buf[..n].copy_from_slice(&self.data[offset as usize..self.bad as usize]);
            return Ok(n);
        }
        let n = (end - offset) as usize;
        buf[..n].copy_from_slice(&self.data[offset as usize..end as usize]);
        Ok(n)
    }

    fn describe(&self) -> String {
        format!("failing at {}", self.bad)
    }
}

fn subjects() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = COMMITTED
        .iter()
        .map(|n| (n.to_string(), committed(n)))
        .collect();
    if let Some(dir) = camera_fixtures() {
        for n in [
            "small_cam_mpf_rst.jpg",
            "small_phone_gain_motion.jpg",
            "small_ps_prog_rst.jpg",
        ] {
            v.push((n.to_string(), read(dir, n)));
        }
    }
    v
}

#[test]
fn short_reads_change_nothing() {
    let f = JpegFormat::new();
    for (name, d) in subjects() {
        let good = mem(d.clone());
        let short: SharedSource = Arc::new(ShortReads {
            data: d.clone(),
            max: 300,
            calls: AtomicU64::new(0),
        });
        let a = f.measure(&good, 0, u64::MAX).unwrap().unwrap();
        let b = f.measure(&short, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(a, b, "{name}");
        let a = f.validate(&good).unwrap();
        let b = f.validate(&short).unwrap();
        assert_eq!(a, b, "{name}");
        assert_eq!(b.status, ValidationStatus::Valid, "{name}");
        let mut out = Vec::new();
        let r = f
            .repair(&short, &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::NoRepairNeeded, "{name}");
        assert_eq!(out, d, "{name}");
    }
}

#[test]
fn read_errors_are_damage_where_they_are() {
    let f = JpegFormat::new();
    for (name, d) in subjects() {
        let h = walk_headers(&*mem(d.clone()), 0, u64::MAX).unwrap();
        let sdo = h.scan_data_offset.unwrap();
        // End of the primary image: measure only reads that far.
        let main_end = analyze(&d).unwrap().eoi_offset.unwrap() + 2;
        for sector in [512u32, 4096] {
            for kind in [
                ReadErrorKind::Timeout,
                ReadErrorKind::DeviceGone,
                ReadErrorKind::Io,
            ] {
                for bad in [
                    0u64,
                    3,
                    700,
                    sdo + 10,
                    (d.len() as u64) * 2 / 3,
                    d.len() as u64 - 1,
                ] {
                    let label = format!("{name} {kind:?} at {bad} ({sector})");
                    let src: SharedSource = Arc::new(Failing {
                        data: d.clone(),
                        bad,
                        kind,
                        sector,
                    });
                    match f.measure(&src, 0, u64::MAX) {
                        Ok(Some(m)) => {
                            assert!(m.len <= d.len() as u64, "{label}");
                            if bad < main_end {
                                assert_ne!(m.status, ExtentStatus::Complete, "{label}: {m:?}");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => assert!(e.is_read_error(), "{label}: {e}"),
                    }
                    match f.validate(&src) {
                        Ok(v) => {
                            assert_eq!(v.status, ValidationStatus::Damaged, "{label}: {v:?}");
                            assert!(
                                v.problems.iter().any(|p| p.contains("unreadable")),
                                "{label}: {:?}",
                                v.problems
                            );
                        }
                        Err(e) => assert!(e.is_read_error(), "{label}: {e}"),
                    }
                    let mut out = Vec::new();
                    if let Ok(r) = f.repair(&src, &mut out, &RepairContext::default()) {
                        assert_eq!(r.output_len, out.len() as u64, "{label}");
                        if r.result == RepairResult::Unrepairable {
                            assert!(out.is_empty(), "{label}");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn known_bad_sectors_inside_the_scan_are_damage() {
    let f = JpegFormat::new();
    for (name, d) in subjects() {
        let h = walk_headers(&*mem(d.clone()), 0, u64::MAX).unwrap();
        let sdo = h.scan_data_offset.unwrap();
        let main_end = analyze(&d).unwrap().eoi_offset.unwrap() + 2;
        let mid = (sdo + main_end) / 2;
        let src: SharedSource = Arc::new(
            MemSource::new(d.clone())
                .with_unreadable(RangeSet::from_range(Range::with_len(mid, 64))),
        );
        let v = f.validate(&src).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Damaged,
            "{name}: {:?}",
            v.problems
        );
        let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_ne!(m.status, ExtentStatus::Complete, "{name}: {m:?}");
        // A hole in what follows the primary image (MPF image, trailer)
        // still makes the file damaged.
        if main_end + 600 < d.len() as u64 {
            let src: SharedSource = Arc::new(
                MemSource::new(d.clone())
                    .with_unreadable(RangeSet::from_range(Range::with_len(main_end + 300, 64))),
            );
            let v = f.validate(&src).unwrap();
            assert_eq!(
                v.status,
                ValidationStatus::Damaged,
                "{name}: {:?}",
                v.problems
            );
            assert!(
                v.problems.iter().any(|p| p.contains("unreadable")),
                "{name}: {:?}",
                v.problems
            );
        }
    }
}

/// A rescued image: the holes read as zeros and are listed by
/// `unreadable()` (what `FileSource::with_unreadable` does with a mapfile).
struct Rescued {
    data: Vec<u8>,
    holes: RangeSet,
}

impl Rescued {
    fn new(mut data: Vec<u8>, hole: Range) -> Self {
        let end = (hole.end as usize).min(data.len());
        data[hole.start as usize..end].fill(0);
        Rescued {
            data,
            holes: RangeSet::from_range(hole),
        }
    }
}

impl Source for Rescued {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.size() {
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - offset as usize);
        buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "rescued image".into()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        Some(self.holes.clone())
    }
}

/// Known holes are reported by `measure` but do not lower its confidence:
/// the carver applies one penalty for holes itself.
#[test]
fn known_holes_are_noted_without_a_confidence_penalty() {
    let f = JpegFormat::new();
    // A hole in the MPF preview after the primary image: the extent and
    // the confidence are those of the healthy file.
    let d = committed("small_mpf.jpg");
    let main_end = analyze(&d).unwrap().eoi_offset.unwrap() + 2;
    let clean = f.measure(&mem(d.clone()), 0, u64::MAX).unwrap().unwrap();
    let hole = Range::with_len(main_end + (d.len() as u64 - main_end) / 2, 64);
    let src: SharedSource = Arc::new(Rescued::new(d.clone(), hole));
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, clean.len);
    assert_eq!(m.status, clean.status);
    assert_eq!(m.confidence, clean.confidence);
    assert!(
        m.notes.iter().any(|n| n.contains("64 unreadable bytes")),
        "{:?}",
        m.notes
    );
    assert_eq!(
        m.meta.extra.get("unreadable_bytes").map(String::as_str),
        Some("64")
    );
    // A hole in restart-marked scan data: the confidence is what the
    // analyzer's verdict gives (a damaged interval, decoding resumes), not
    // lowered again for the hole.
    let d = committed("small_restart.jpg");
    let h = walk_headers(&*mem(d.clone()), 0, u64::MAX).unwrap();
    let sdo = h.scan_data_offset.unwrap();
    let hole = Range::with_len((sdo + d.len() as u64) / 2, 16);
    let src: SharedSource = Arc::new(Rescued::new(d.clone(), hole));
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, d.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete, "{m:?}");
    assert_eq!(m.confidence, diskrescue_jpeg::core::format::Confidence(65));
    assert!(m.notes.iter().any(|n| n.contains("16 unreadable bytes")));
}
