//! End-to-end recovery tests over synthetic devices with known ground truth.
//!
//! Each scenario comes from `diskrescue-fixtures`, which records the BLAKE3
//! hash of every file it wrote. A file counts as recovered only when a file
//! with exactly that content appears in the output.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_core::media::MediaCategory;
use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::FileSource;
use diskrescue_core::SharedSource;
use diskrescue_fixtures::sets::{self, Recoverability, Scenario, TruthFile};
use diskrescue_fixtures::FixtureError;
use diskrescue_recover::filters::category_for_extension;
use diskrescue_recover::report::{Origin, Outcome, Report, SourceRecord};
use diskrescue_recover::{
    recover, repair_files, verify_files, CarvePolicy, NoopObserver, RecoverOptions,
};

fn scenario(build: fn(&Path) -> Result<Scenario, FixtureError>, dir: &Path) -> Option<Scenario> {
    match build(dir) {
        Ok(s) => Some(s),
        Err(e) if e.is_missing() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("fixture build failed: {e}"),
    }
}

fn is_media(t: &TruthFile) -> bool {
    let ext = t.path.rsplit('.').next().unwrap_or("");
    matches!(
        category_for_extension(ext),
        Some(MediaCategory::Photo | MediaCategory::RawPhoto | MediaCategory::Video)
    )
}

fn run(src: SharedSource, out: &Path, tweak: impl FnOnce(&mut RecoverOptions)) -> Report {
    let mut opts = RecoverOptions::new(out);
    opts.workers = 4;
    tweak(&mut opts);
    let source = SourceRecord {
        description: src.describe(),
        size: src.size(),
        ..Default::default()
    };
    recover(src, source, &opts, &NoopObserver).expect("recover")
}

fn open(p: &Path) -> SharedSource {
    Arc::new(FileSource::open(p).expect("open image"))
}

fn hash_file(p: &Path) -> String {
    diskrescue_core::util::hash::blake3_hex(&std::fs::read(p).expect("read output"))
}

/// Every Live / DeletedIntact media file must be in the output, byte for byte.
fn assert_all_recovered(report: &Report, out: &Path, scn: &Scenario) {
    let by_hash: BTreeMap<&str, Vec<&diskrescue_recover::FileRecord>> = report
        .files
        .iter()
        .filter(|f| f.output.is_some())
        .fold(BTreeMap::new(), |mut m, f| {
            if let Some(h) = f.blake3.as_deref() {
                m.entry(h).or_default().push(f);
            }
            m
        });
    let mut missing = Vec::new();
    for t in scn.files.iter().filter(|t| is_media(t)) {
        if !matches!(
            t.expect,
            Recoverability::Live | Recoverability::DeletedIntact
        ) {
            continue;
        }
        match by_hash.get(t.blake3.as_str()) {
            None => missing.push(format!("{} ({:?})", t.path, t.expect)),
            Some(recs) => {
                let rec = recs[0];
                let written = out.join(rec.output.as_ref().unwrap());
                assert_eq!(hash_file(&written), t.blake3, "{} content on disk", t.path);
                if t.expect == Recoverability::Live {
                    let via_fs = recs.iter().any(|r| {
                        matches!(&r.origin, Origin::Filesystem { path, deleted: false, .. } if path == &t.path)
                    });
                    assert!(
                        via_fs,
                        "{} should come back through the filesystem with its name",
                        t.path
                    );
                }
            }
        }
    }
    let listing: Vec<String> = report
        .files
        .iter()
        .map(|f| format!("{:?} {} {:?}", f.outcome, f.origin_text(), f.output))
        .collect();
    assert!(
        missing.is_empty(),
        "not recovered: {missing:?}\nrecovered:\n{}",
        listing.join("\n")
    );
}

/// No content may be written twice because the carver found a copy of a
/// file the filesystem already produced.
fn assert_no_carved_duplicates(report: &Report) {
    let mut seen: BTreeMap<&str, &Origin> = BTreeMap::new();
    for f in report
        .files
        .iter()
        .filter(|f| f.output.is_some() && f.outcome != Outcome::Duplicate)
    {
        let Some(h) = f.blake3.as_deref() else {
            continue;
        };
        if let Some(prev) = seen.insert(h, &f.origin) {
            let carved =
                matches!(f.origin, Origin::Carved { .. }) || matches!(prev, Origin::Carved { .. });
            assert!(
                !carved,
                "carved duplicate written: {} and {:?}",
                f.origin_text(),
                prev
            );
        }
    }
}

fn assert_reports_written(out: &Path) {
    let r = Report::read_json(&out.join("report.json")).expect("report.json");
    assert!(r.finished.is_some());
    let txt = std::fs::read_to_string(out.join("report.txt")).expect("report.txt");
    assert!(txt.contains("Recovered"));
}

#[test]
fn camera_sd_card_live_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::camera_sd_card, &dir.path().join("scn")) else {
        return;
    };
    let out = dir.path().join("out");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |_| {});
    assert_all_recovered(&report, &out, &scn);
    assert_no_carved_duplicates(&report);
    assert_reports_written(&out);
    assert!(report
        .volumes
        .iter()
        .any(|v| v.fs.is_some_and(|k| k.name() == "fat32")));
}

#[test]
fn phone_exfat_under_gpt() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::phone_exfat, &dir.path().join("scn")) else {
        return;
    };
    let out = dir.path().join("out");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |_| {});
    assert_all_recovered(&report, &out, &scn);
    assert_no_carved_duplicates(&report);
    assert!(report
        .volumes
        .iter()
        .any(|v| v.fs.is_some_and(|k| k.name() == "exfat")));
}

#[test]
fn ntfs_disk_under_mbr() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::ntfs_disk, &dir.path().join("scn")) else {
        return;
    };
    let out = dir.path().join("out");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |_| {});
    assert_all_recovered(&report, &out, &scn);
    assert_no_carved_duplicates(&report);
    assert!(report
        .volumes
        .iter()
        .any(|v| v.fs.is_some_and(|k| k.name() == "ntfs")));
}

#[test]
fn ext4_disk_under_gpt() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::ext4_disk, &dir.path().join("scn")) else {
        return;
    };
    let out = dir.path().join("out");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |_| {});
    assert_all_recovered(&report, &out, &scn);
    assert_no_carved_duplicates(&report);
}

/// The boot sector, both FATs and the root directory of a camera card are
/// wiped ("the card needs to be formatted"). Carving must still return every
/// contiguous photo and video byte for byte.
#[test]
fn wiped_filesystem_is_recovered_by_carving() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::camera_sd_card, &dir.path().join("scn")) else {
        return;
    };
    let img = scn.disk_image.clone().unwrap();
    let first_data = scn
        .files
        .iter()
        .flat_map(|t| t.extents.iter().map(|e| e.0))
        .min()
        .expect("files have extents");
    let mut data = std::fs::read(&img).unwrap();
    for b in &mut data[..first_data as usize] {
        *b = 0;
    }
    let wiped = dir.path().join("wiped.img");
    std::fs::write(&wiped, &data).unwrap();
    drop(data);

    let out = dir.path().join("out");
    let report = run(open(&wiped), &out, |_| {});
    assert!(report
        .files
        .iter()
        .all(|f| !matches!(f.origin, Origin::Filesystem { .. })));
    let carved: BTreeMap<&str, &diskrescue_recover::FileRecord> = report
        .files
        .iter()
        .filter(|f| f.output.is_some())
        .filter_map(|f| f.blake3.as_deref().map(|h| (h, f)))
        .collect();
    let mut missing = Vec::new();
    for t in scn.files.iter().filter(|t| is_media(t) && t.contiguous) {
        if !carved.contains_key(t.blake3.as_str()) {
            missing.push(t.path.clone());
        }
    }
    assert!(missing.is_empty(), "carving missed {missing:?}");
    let undated = out.join("carved/photos/undated");
    let dated = out.join("carved/photos/2024");
    assert!(
        dated.exists(),
        "photos with EXIF dates are filed by year/month"
    );
    let _ = undated;
}

/// A failing device: one sector inside a photo cannot be read. That photo
/// must be reported with the hole; every other file must be intact.
#[test]
fn unreadable_sector_is_reported_not_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::camera_sd_card, &dir.path().join("scn")) else {
        return;
    };
    let victim = scn
        .files
        .iter()
        .find(|t| t.path.ends_with("IMG_0002.JPG"))
        .expect("IMG_0002.JPG in scenario");
    let (start, len) = victim.extents[0];
    let bad = Range::with_len(start + (len / 2) / 512 * 512, 512);
    let src: SharedSource = Arc::new(
        FileSource::open(scn.disk_image.as_ref().unwrap())
            .unwrap()
            .with_unreadable(RangeSet::from_range(bad)),
    );
    let out = dir.path().join("out");
    let report = run(src, &out, |_| {});
    let rec = report
        .files
        .iter()
        .find(|f| matches!(&f.origin, Origin::Filesystem { path, .. } if path == &victim.path))
        .expect("victim listed");
    assert_eq!(rec.unreadable_bytes, 512, "hole reported: {rec:?}");
    assert_ne!(
        rec.outcome,
        Outcome::Recovered,
        "a photo with a hole must not be reported as intact"
    );
    for t in scn
        .files
        .iter()
        .filter(|t| is_media(t) && t.expect == Recoverability::Live && t.path != victim.path)
    {
        assert!(
            report
                .files
                .iter()
                .any(|f| f.blake3.as_deref() == Some(t.blake3.as_str())),
            "{} should be intact",
            t.path
        );
    }
    assert!(report.source.unreadable_bytes == 512);
}

/// Only free space is carved when the filesystem is healthy, and
/// `CarvePolicy::Off` disables carving entirely.
#[test]
fn carve_policies() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::camera_sd_card, &dir.path().join("scn")) else {
        return;
    };
    let out = dir.path().join("off");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |o| {
        o.carve = CarvePolicy::Off
    });
    assert!(report.carve.is_none());
    assert!(report
        .files
        .iter()
        .all(|f| !matches!(f.origin, Origin::Carved { .. })));
    let out = dir.path().join("free");
    let report = run(open(scn.disk_image.as_ref().unwrap()), &out, |o| {
        o.carve = CarvePolicy::FreeSpace
    });
    let c = report.carve.as_ref().expect("carve stats");
    assert!(c.unallocated_only);
    assert!(c.scanned < report.source.size);
}

/// Loose damaged files: each repair must produce a file that validates.
#[test]
fn repair_damaged_media() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::damaged_media, &dir.path().join("scn")) else {
        return;
    };
    let root = scn.root.clone();
    let damaged: Vec<&TruthFile> = scn
        .files
        .iter()
        .filter(|t| {
            matches!(
                t.expect,
                Recoverability::Repairable | Recoverability::PartialOnly
            )
        })
        .collect();
    assert!(damaged.len() >= 5, "scenario has damaged files");
    let inputs: Vec<PathBuf> = damaged.iter().map(|t| root.join(&t.path)).collect();
    let donors: Vec<PathBuf> = scn
        .files
        .iter()
        .filter(|t| t.expect == Recoverability::Donor)
        .map(|t| root.join(&t.path))
        .collect();
    let out = dir.path().join("out");
    let report = repair_files(&inputs, &out, &donors, false, &NoopObserver).expect("repair");

    for t in &damaged {
        let input = root.join(&t.path);
        let rec = report
            .files
            .iter()
            .find(|f| matches!(&f.origin, Origin::Input { path } if path == &input))
            .unwrap_or_else(|| panic!("{} in report", t.path));
        let want_full = t.expect == Recoverability::Repairable && !t.path.starts_with("truncated.");
        let ok = match rec.outcome {
            Outcome::Repaired => true,
            Outcome::PartiallyRepaired => !want_full,
            _ => false,
        };
        assert!(
            ok,
            "{}: outcome {:?}, notes {:?}, validation {:?}",
            t.path, rec.outcome, rec.notes, rec.validation
        );
        let repaired = out.join(rec.output.as_ref().expect("repaired output"));
        let v = &verify_files(std::slice::from_ref(&repaired))[0];
        if rec.outcome == Outcome::Repaired {
            assert_eq!(
                v.validation.as_ref().map(|x| x.status),
                Some(diskrescue_core::format::ValidationStatus::Valid),
                "{} repaired copy validates: {v:?}",
                t.path
            );
        }
        if t.path == "bad_crc.png" {
            let orig = scn
                .files
                .iter()
                .find(|o| Some(&o.path) == t.original.as_ref())
                .unwrap();
            assert_eq!(
                hash_file(&repaired),
                orig.blake3,
                "CRC repair restores the exact original"
            );
        }
        if t.path.ends_with(".mp4") || t.path.ends_with(".avi") {
            assert_decodes(&repaired);
        }
    }
    // The inputs were not modified.
    for t in &damaged {
        assert_eq!(
            hash_file(&root.join(&t.path)),
            t.blake3,
            "{} untouched",
            t.path
        );
    }
}

/// ffmpeg must decode the whole file without a single error.
fn assert_decodes(p: &Path) {
    if !diskrescue_fixtures::tools::have("ffmpeg") {
        eprintln!("skipped decode check: ffmpeg missing");
        return;
    }
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(p)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && err.trim().is_empty(),
        "{} does not decode cleanly: {err}",
        p.display()
    );
}

/// A Windows disk whose NTFS boot sector and backup boot sector are both
/// destroyed: the volume is rebuilt from its file table and files come back
/// with their names.
#[test]
fn ntfs_without_boot_sectors_keeps_names() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::ntfs_disk, &dir.path().join("scn")) else {
        return;
    };
    let img = scn.disk_image.clone().unwrap();
    let (start, size) = scn.partitions[0];
    let mut data = std::fs::read(&img).unwrap();
    // Primary boot sector (first sector) and backup (last sector of the volume).
    for b in &mut data[start as usize..start as usize + 512] {
        *b = 0;
    }
    let last = (start + size - 512) as usize;
    for b in &mut data[last..last + 512] {
        *b = 0;
    }
    let broken = dir.path().join("broken.img");
    std::fs::write(&broken, &data).unwrap();
    drop(data);

    let out = dir.path().join("out");
    let report = run(open(&broken), &out, |_| {});
    assert!(
        report
            .volumes
            .iter()
            .any(|v| v.origin == "mft-scan" && v.error.is_none()),
        "volume rebuilt from the MFT: {:?}",
        report.volumes
    );
    assert_all_recovered(&report, &out, &scn);
}

/// A camera card (FAT32, no partition table) whose boot sector and backup
/// boot sector are both destroyed: the volume is rebuilt from the FAT copies
/// and photos come back with their names.
#[test]
fn fat_card_without_boot_sectors_keeps_names() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = scenario(sets::camera_sd_card, &dir.path().join("scn")) else {
        return;
    };
    let img = scn.disk_image.clone().unwrap();
    let mut data = std::fs::read(&img).unwrap();
    for sector in [0usize, 1, 2, 6, 7, 8] {
        for b in &mut data[sector * 512..sector * 512 + 512] {
            *b = 0;
        }
    }
    let broken = dir.path().join("broken.img");
    std::fs::write(&broken, &data).unwrap();
    drop(data);
    let out = dir.path().join("out");
    let report = run(open(&broken), &out, |_| {});
    assert!(
        report
            .volumes
            .iter()
            .any(|v| v.origin == "fat-rebuild" && v.error.is_none()),
        "volume rebuilt from the FATs: {:?}",
        report.volumes
    );
    assert_all_recovered(&report, &out, &scn);
}
