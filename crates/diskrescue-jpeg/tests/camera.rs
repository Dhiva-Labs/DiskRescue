//! Realistic camera and phone files (`tests/fixtures/gen_camera.py`):
//! large Exif blocks with MakerNotes, IFD1 thumbnails, MPF previews and
//! gain maps, ICC profiles over several APP2 segments, XMP, Adobe APP14,
//! Photoshop APP13 thumbnails, every common sampling factor, restart
//! intervals, progressive files, CMYK, 12-bit and mozjpeg-style
//! deringing. Every one must validate as Valid, analyze cleanly and
//! measure to its exact length in every surrounding.

mod common;

use common::*;
use diskrescue_jpeg::core::format::{
    ExtentStatus, FileFormat, NestedRole, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_jpeg::*;

fn camera_set() -> Option<Vec<(String, Vec<u8>, u64)>> {
    let dir = camera_fixtures()?;
    Some(
        camera_manifest(dir)
            .into_iter()
            .map(|(n, main)| {
                let d = read(dir, &n);
                (n, d, main)
            })
            .collect(),
    )
}

#[test]
fn camera_files_are_valid_and_clean() {
    let Some(set) = camera_set() else { return };
    let f = JpegFormat::new();
    let mut failures = Vec::new();
    for (name, d, main) in &set {
        let v = f.validate(&mem(d.clone())).unwrap();
        if v.status != ValidationStatus::Valid || v.intact_fraction != Some(1.0) {
            failures.push(format!("{name}: validate {:?} {:?}", v.status, v.problems));
        }
        if !v.problems.is_empty() {
            failures.push(format!("{name}: problems on a clean file {:?}", v.problems));
        }
        let a = analyze(&d[..*main as usize]).unwrap();
        if !a.is_clean() || a.mcu_rows_decoded != a.mcu_rows_total || a.trailing_bytes != 0 {
            failures.push(format!(
                "{name}: analyze {:?} rows {}/{} notes {:?}",
                a.first_error, a.mcu_rows_decoded, a.mcu_rows_total, a.notes
            ));
        }
        let mut out = Vec::new();
        let r = f
            .repair(&mem(d.clone()), &mut out, &RepairContext::default())
            .unwrap();
        if r.result != RepairResult::NoRepairNeeded || out != *d {
            failures.push(format!("{name}: repair of a clean file {:?}", r.result));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// What can follow a file on a medium.
fn followers(set: &[(String, Vec<u8>, u64)], rng: &mut Rng) -> Vec<(&'static str, Vec<u8>)> {
    let other = set
        .iter()
        .find(|(n, _, _)| n == "ps_keep_rgb.jpg")
        .map(|(_, d, _)| d.clone())
        .unwrap();
    let mut zeros_then_jpeg = vec![0u8; 4096 - 1234];
    zeros_then_jpeg.extend_from_slice(&other);
    vec![
        ("nothing", Vec::new()),
        ("zeros", vec![0u8; 70_000]),
        ("ff-fill", vec![0xFF; 3000]),
        ("random", rng.bytes(50_000)),
        ("next-jpeg", other.clone()),
        ("zeros+jpeg", zeros_then_jpeg),
        ("eoi-bytes", [0xFF, 0xD9, 0xFF, 0xD9, 0, 0].repeat(100)),
    ]
}

#[test]
fn camera_files_measure_exactly_in_any_surrounding() {
    let Some(set) = camera_set() else { return };
    let f = JpegFormat::new();
    let mut rng = Rng(0xCA3E7A);
    let mut failures = Vec::new();
    let follow = followers(&set, &mut rng);
    for (name, d, main) in &set {
        for (what, tail) in &follow {
            let prefix = rng.below(3000) as usize;
            let mut buf = rng.bytes(prefix);
            buf.extend_from_slice(d);
            buf.extend_from_slice(tail);
            let src = mem(buf);
            let m = f.measure(&src, prefix as u64, u64::MAX).unwrap();
            let Some(m) = m else {
                failures.push(format!("{name} + {what}: rejected"));
                continue;
            };
            if m.len != d.len() as u64 || m.status != ExtentStatus::Complete {
                failures.push(format!(
                    "{name} + {what}: len {} (want {}) {:?} {:?}",
                    m.len,
                    d.len(),
                    m.status,
                    m.notes
                ));
            }
            if m.confidence < diskrescue_jpeg::core::format::Confidence::HIGH {
                failures.push(format!("{name} + {what}: confidence {:?}", m.confidence));
            }
            for n in &m.nested {
                if n.range.end > m.len || n.range.start < 2 {
                    failures.push(format!("{name} + {what}: nested {:?} outside", n.range));
                }
            }
            if *main < d.len() as u64
                && !m
                    .nested
                    .iter()
                    .any(|n| n.range.end == d.len() as u64 && n.role != NestedRole::Thumbnail)
            {
                failures.push(format!(
                    "{name} + {what}: data after the primary EOI is not nested: {:?}",
                    m.nested
                ));
            }
        }
        // The carver's limit cuts the file: never longer than the limit.
        for limit in [*main / 2, *main, *main + 1, d.len() as u64 - 1] {
            if let Some(m) = f.measure(&mem(d.clone()), 0, limit).unwrap() {
                if m.len > limit {
                    failures.push(format!("{name}: len {} > limit {limit}", m.len));
                }
                // Cut inside the primary image or an MPF image (whose index
                // gives its extent) it must not claim to be complete; a
                // Samsung trailer cut before its SEFT directory cannot be
                // recognised, so the image ends at what is recognisable.
                let complete_ok = name.contains("samsung") && limit >= *main;
                if limit < d.len() as u64 && m.status == ExtentStatus::Complete && !complete_ok {
                    failures.push(format!("{name}: complete at limit {limit} {:?}", m.notes));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn repair_with(
    d: &[u8],
    ctx: &RepairContext,
) -> (diskrescue_jpeg::core::format::RepairOutcome, Vec<u8>) {
    let mut out = Vec::new();
    let r = JpegFormat::new()
        .repair(&mem(d.to_vec()), &mut out, ctx)
        .unwrap();
    assert_eq!(r.output_len, out.len() as u64);
    (r, out)
}

/// Which reference decoders handle the clean original: zune-jpeg 0.4 has
/// no 12-bit support and fails on progressive files with a restart
/// interval of 1; Pillow has no 12-bit support (ffmpeg stands in).
struct Decoders {
    zune: bool,
    pillow: bool,
}

fn decoders_for(name: &str) -> Decoders {
    Decoders {
        zune: !name.contains("12bit") && name != "cam_progressive_rst1.jpg",
        pillow: !name.contains("12bit"),
    }
}

fn ffmpeg_decodes(path: &std::path::Path) -> Option<bool> {
    let o = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .ok()?;
    Some(o.status.success())
}

/// A repair output must decode with zune-jpeg and with Pillow (ffmpeg for
/// 12-bit files).
fn output_decodes(label: &str, dec: &Decoders, out: &[u8], failures: &mut Vec<String>) {
    if dec.zune && zune_dims(out).is_none() {
        let e = zune_check(out).error;
        failures.push(format!(
            "{label}: zune-jpeg cannot decode the repair output ({e:?})"
        ));
    }
    // A file of its own: tests run in parallel threads of one process, and a
    // shared path let one test overwrite the file another was decoding.
    let file = tempfile::Builder::new()
        .prefix("djr-")
        .suffix(".jpg")
        .tempfile()
        .unwrap();
    std::fs::write(file.path(), out).unwrap();
    let p = file.path();
    if dec.pillow {
        if pillow_decodes(p) == Some(false) {
            failures.push(format!("{label}: Pillow cannot decode the repair output"));
        }
    } else if ffmpeg_decodes(p) == Some(false) {
        failures.push(format!("{label}: ffmpeg cannot decode the repair output"));
    }
}

fn scan_start(d: &[u8]) -> u64 {
    walk_headers(&*mem(d.to_vec()), 0, u64::MAX)
        .unwrap()
        .scan_data_offset
        .unwrap()
}

#[test]
fn damaged_camera_files_are_damaged_and_repairs_decode() {
    let Some(set) = camera_set() else { return };
    let f = JpegFormat::new();
    let mut rng = Rng(0xDA3A6E);
    let mut failures = Vec::new();
    let aggressive = RepairContext {
        aggressive: true,
        ..Default::default()
    };
    for (name, d, main) in &set {
        let sdo = scan_start(d);
        let dec = decoders_for(name);
        let span = *main - sdo;
        // Truncation inside the scan data.
        for pct in [3u64, 30, 60, 90, 99] {
            let cut = (sdo + span * pct / 100) as usize;
            let t = &d[..cut];
            let v = f.validate(&mem(t.to_vec())).unwrap();
            if v.status != ValidationStatus::Damaged {
                failures.push(format!("{name} cut {pct}%: {:?}", v.status));
            }
            for (label, ctx) in [
                ("plain", RepairContext::default()),
                ("aggr", aggressive.clone()),
            ] {
                let (r, out) = repair_with(t, &ctx);
                // Without a reference decoder (12-bit) and without a whole
                // MCU row nothing can be shown to decode; the noisy 12-bit
                // fixtures' optimised DC tables have no category-0 code, so
                // the scan cannot be completed with empty MCUs either.
                let unverifiable = !dec.zune && !dec.pillow && pct < 10;
                if r.result == RepairResult::Unrepairable && unverifiable {
                    continue;
                }
                if matches!(
                    r.result,
                    RepairResult::Unrepairable | RepairResult::NoRepairNeeded
                ) {
                    failures.push(format!(
                        "{name} cut {pct}% {label}: {:?} {:?}",
                        r.result, r.notes
                    ));
                    continue;
                }
                output_decodes(
                    &format!("{name} cut {pct}% {label}"),
                    &dec,
                    &out,
                    &mut failures,
                );
            }
        }
        // Corruption inside the scan data: if the analyzer sees it, the
        // file is never Valid; aggressive repairs decode.
        for round in 0..6 {
            let mut c = d[..*main as usize].to_vec();
            let at = sdo + 16 + rng.below(span.saturating_sub(64).max(1));
            for k in 0..4 {
                let p = (at + k) as usize;
                if p + 2 < c.len() {
                    c[p] ^= 1 + rng.below(255) as u8;
                }
            }
            let v = f.validate(&mem(c.clone())).unwrap();
            if let Ok(a) = diskrescue_jpeg::scan::analyze_with(&c, &AnalyzeOptions::default()) {
                if a.first_error.is_some() && v.status == ValidationStatus::Valid {
                    failures.push(format!("{name} flip {round}: analyzer error but Valid"));
                }
            }
            if v.status == ValidationStatus::Damaged {
                let (r, out) = repair_with(&c, &aggressive);
                if r.result != RepairResult::Unrepairable {
                    output_decodes(
                        &format!("{name} flip {round} aggr"),
                        &dec,
                        &out,
                        &mut failures,
                    );
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Header destroyed (everything before the scan data zeroed, or only the
/// metadata with the SOF intact): a donor from the same camera settings
/// restores the picture; a donor whose geometry, coding process, restart
/// interval or Huffman tables differ is refused.
#[test]
fn donor_transplant_refuses_incompatible_donors() {
    let Some(dir) = camera_fixtures() else { return };
    let f = JpegFormat::new();
    let mut failures = Vec::new();
    let cases: [(&str, &[&str], &[&str]); 2] = [
        (
            "dv_victim_422_rst4.jpg",
            &["dv_ok.jpg", "dv_quality75.jpg"],
            &[
                "dv_sub420.jpg",
                "dv_sub444.jpg",
                "dv_larger_480x360.jpg",
                "dv_smaller_320x200.jpg",
                "dv_progressive.jpg",
                "dv_no_dri.jpg",
                "dv_dri8.jpg",
                "dv_optimised.jpg",
                "dv_gray.jpg",
            ],
        ),
        (
            "dw_victim_420.jpg",
            &["dw_ok.jpg"],
            &[
                "dw_optimised.jpg",
                "dw_larger_352x240.jpg",
                "dw_sub422.jpg",
                "dv_ok.jpg",
            ],
        ),
    ];
    for (victim, good, bad) in cases {
        let v = read(dir, victim);
        let h = walk_headers(&*mem(v.clone()), 0, u64::MAX).unwrap();
        let sdo = h.scan_data_offset.unwrap() as usize;
        let sof = h.sof.as_ref().unwrap().offset as usize;
        let mut all_zero = v.clone();
        all_zero[..sdo].fill(0);
        let mut meta_zero = v.clone();
        meta_zero[..sof].fill(0);
        for (how, damaged) in [
            ("header zeroed", &all_zero),
            ("metadata zeroed", &meta_zero),
        ] {
            let dv = f.validate(&mem(damaged.clone())).unwrap();
            if dv.status != ValidationStatus::Damaged {
                failures.push(format!("{victim} {how}: {:?}", dv.status));
            }
            for donor in good {
                let ctx = RepairContext {
                    donors: vec![dir.join(donor)],
                    ..Default::default()
                };
                let (r, out) = repair_with(damaged, &ctx);
                let label = format!("{victim} {how} + {donor}");
                if !matches!(
                    r.result,
                    RepairResult::Repaired | RepairResult::PartiallyRepaired
                ) {
                    failures.push(format!("{label}: {:?} {:?}", r.result, r.notes));
                    continue;
                }
                if *donor == good[0] && r.result != RepairResult::Repaired {
                    failures.push(format!("{label}: {:?} {:?}", r.result, r.validation));
                }
                output_decodes(&label, &decoders_for(victim), &out, &mut failures);
                let osdo = scan_start(&out) as usize;
                if out[osdo..] != v[sdo..] {
                    failures.push(format!("{label}: scan data not the victim's own"));
                }
            }
            for donor in bad {
                let ctx = RepairContext {
                    donors: vec![dir.join(donor)],
                    aggressive: true,
                    ..Default::default()
                };
                let (r, out) = repair_with(damaged, &ctx);
                if r.result != RepairResult::Unrepairable || !out.is_empty() {
                    failures.push(format!(
                        "{victim} {how} + {donor}: accepted ({:?}, {:?})",
                        r.result, r.applied
                    ));
                } else if r.notes.is_empty() {
                    failures.push(format!(
                        "{victim} {how} + {donor}: refused without a reason"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
