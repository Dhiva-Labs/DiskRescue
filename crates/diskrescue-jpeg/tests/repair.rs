//! Repair: every strategy produces a file that decodes, and repairs are
//! refused when they cannot work.

mod common;

use common::*;
use diskrescue_jpeg::core::format::{FileFormat, RepairContext, RepairResult, ValidationStatus};
use diskrescue_jpeg::markers::HeaderIssue;
use diskrescue_jpeg::*;

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

fn aggressive() -> RepairContext {
    RepairContext {
        aggressive: true,
        ..Default::default()
    }
}

#[test]
fn donor_header_restores_a_zeroed_header() {
    let Some(dir) = fixtures() else { return };
    let victim = read(dir, "victim_640x480.jpg");
    let mut damaged = victim.clone();
    damaged[..600].fill(0);
    let f = JpegFormat::new();
    let v = f.validate(&mem(damaged.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged, "{:?}", v.problems);
    assert!(v.repairable_by.iter().any(|r| r == "donor_header"));
    // Without a donor nothing can be done.
    let (r, out) = repair_with(&damaged, &RepairContext::default());
    assert_eq!(r.result, RepairResult::Unrepairable);
    assert!(out.is_empty());
    let ctx = RepairContext {
        donors: vec![dir.join("donor_640x480.jpg")],
        ..Default::default()
    };
    let (r, out) = repair_with(&damaged, &ctx);
    assert!(
        matches!(
            r.result,
            RepairResult::Repaired | RepairResult::PartiallyRepaired
        ),
        "{r:?}"
    );
    assert_eq!(zune_dims(&out), Some((640, 480)));
    assert!(r.notes.iter().any(|n| n.contains("colours may be off")));
    // Same settings: the whole victim scan decodes with the donor tables.
    assert_eq!(r.result, RepairResult::Repaired, "{r:?}");
    assert!(zune_strict(&out));
    let a = analyze(&out).unwrap();
    assert!(a.is_clean());
    // The scan data is the victim's own, byte for byte.
    let v_scan = &victim[walk_headers(&*mem(victim.clone()), 0, u64::MAX)
        .unwrap()
        .scan_data_offset
        .unwrap() as usize..];
    let o_scan = &out[walk_headers(&*mem(out.clone()), 0, u64::MAX)
        .unwrap()
        .scan_data_offset
        .unwrap() as usize..];
    assert_eq!(v_scan, o_scan);
}

#[test]
fn donor_with_other_dimensions_is_refused() {
    let Some(dir) = fixtures() else { return };
    let victim = read(dir, "victim_640x480.jpg");
    let ctx = RepairContext {
        donors: vec![dir.join("donor_wrong_320x240.jpg")],
        ..Default::default()
    };
    // SOF survives (only APP0/DQT destroyed): the mismatch is explicit.
    let mut damaged = victim.clone();
    damaged[2..120].fill(0);
    let (r, out) = repair_with(&damaged, &ctx);
    assert_eq!(r.result, RepairResult::Unrepairable, "{r:?}");
    assert!(out.is_empty());
    assert!(
        r.notes.iter().any(|n| n.contains("same dimensions")),
        "{:?}",
        r.notes
    );
    // SOF destroyed too: the scan data does not fit the donor geometry.
    let mut damaged = victim.clone();
    damaged[..600].fill(0);
    let (r, out) = repair_with(&damaged, &ctx);
    assert_eq!(r.result, RepairResult::Unrepairable, "{r:?}");
    assert!(out.is_empty());
}

#[test]
fn donor_scan_offset_can_be_given_explicitly() {
    let Some(dir) = fixtures() else { return };
    let victim = read(dir, "victim_640x480.jpg");
    let sdo = walk_headers(&*mem(victim.clone()), 0, u64::MAX)
        .unwrap()
        .scan_data_offset
        .unwrap();
    let mut damaged = victim.clone();
    damaged[..sdo as usize].fill(0x55);
    let mut out = Vec::new();
    let r = apply(
        &mem(damaged),
        &mut out,
        &[Strategy::DonorHeader {
            donor: dir.join("donor_640x480.jpg"),
            scan_offset: Some(sdo),
        }],
        &RepairContext::default(),
    )
    .unwrap();
    assert_eq!(r.result, RepairResult::Repaired, "{r:?}");
    assert_eq!(zune_dims(&out), Some((640, 480)));
}

#[test]
fn missing_dht_gets_the_standard_tables() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "std_640x480.jpg");
    let h = walk_headers(&*mem(d.clone()), 0, u64::MAX).unwrap();
    assert!(
        h.dht.iter().all(|t| t.is_standard()),
        "fixture must use Annex K tables"
    );
    let stripped = strip_dht(&d);
    assert!(stripped.len() < d.len());
    assert!(!zune_check(&stripped).lenient_ok);
    let f = JpegFormat::new();
    let v = f.validate(&mem(stripped.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems.iter().any(|p| p.contains("DHT missing")),
        "{:?}",
        v.problems
    );
    assert_eq!(
        v.repairable_by,
        vec!["insert_standard_huffman_tables".to_string()]
    );
    let (r, out) = repair_with(&stripped, &RepairContext::default());
    assert_eq!(r.result, RepairResult::Repaired, "{r:?}");
    assert!(zune_strict(&out));
    assert_eq!(zune_dims(&out), Some((640, 480)));
    let a = analyze(&out).unwrap();
    assert!(a.is_clean() && !a.used_default_tables);
}

#[test]
fn append_eoi_makes_a_truncated_file_viewable() {
    let Some(dir) = fixtures() else { return };
    for n in [
        "noise_1024x768.jpg",
        "photo_1024x768.jpg",
        "prog_noise_1024x768.jpg",
        "noise_rst_1024x768.jpg",
    ] {
        let d = read(dir, n);
        let cut = &d[..d.len() * 6 / 10];
        let (r, out) = repair_with(cut, &RepairContext::default());
        assert_eq!(r.result, RepairResult::PartiallyRepaired, "{n}: {r:?}");
        assert!(r.applied.iter().any(|a| a.contains("EOI")), "{n}");
        assert!(out.ends_with(&[0xFF, 0xD9]), "{n}");
        assert_eq!(zune_dims(&out), Some((1024, 768)), "{n}");
        let a = analyze(&out).unwrap();
        assert!(a.eoi_offset.is_some());
        let frac = r.validation.unwrap().intact_fraction.unwrap();
        if !n.starts_with("prog") {
            assert!((0.45..0.7).contains(&frac), "{n}: {frac}");
        }
    }
}

#[test]
fn aggressive_truncation_cuts_at_clean_boundaries() {
    let Some(dir) = fixtures() else { return };
    // Truncated without restart markers: cut after the last good MCU.
    // The rest of the scan is re-encoded as empty (grey) MCUs, so the
    // output is a complete, standard-conforming file.
    let d = read(dir, "photo_1024x768.jpg");
    let cut = &d[..d.len() / 2];
    let before = analyze(cut).unwrap();
    let kept = before.last_good_mcu.unwrap();
    let (r, out) = repair_with(cut, &aggressive());
    assert_eq!(r.result, RepairResult::PartiallyRepaired, "{r:?}");
    assert!(zune_strict(&out));
    assert_eq!(zune_dims(&out), Some((1024, 768)));
    let after = analyze(&out).unwrap();
    assert!(after.is_clean(), "{:?}", after.first_error);
    assert_eq!(after.mcus_decoded, before.mcus_total);
    assert_eq!(after.eoi_offset, Some(out.len() as u64 - 2));
    assert_eq!(
        &out[..kept.byte_offset as usize],
        &cut[..kept.byte_offset as usize]
    );
    let frac = r.validation.unwrap().intact_fraction.unwrap();
    let want = before.mcus_decoded as f32 / before.mcus_total as f32;
    assert!((frac - want).abs() < 0.01, "{frac} vs {want}");

    // Corruption with restart markers: only the damaged interval is
    // replaced; the intervals after it are kept.
    let d = read(dir, "noise_rst_1024x768.jpg");
    let mut rng = Rng(11);
    let mut at = d.len() / 2;
    let x = loop {
        if !d[at - 1..at + 5].contains(&0xFF) {
            let mut x = d.clone();
            for b in &mut x[at..at + 4] {
                *b ^= 1 + rng.below(200) as u8;
            }
            if !x[at..at + 4].contains(&0xFF) && analyze(&x).unwrap().first_error.is_some() {
                break x;
            }
        }
        at += 13;
    };
    let a = analyze(&x).unwrap();
    let (restart_off, restart_mcu) = a.last_good_restart.unwrap();
    // Not aggressive: discarding data is not allowed.
    let (r, out) = repair_with(&x, &RepairContext::default());
    assert_eq!(r.result, RepairResult::Unrepairable, "{r:?}");
    assert!(out.is_empty());
    assert!(
        r.notes.iter().any(|n| n.contains("aggressive")),
        "{:?}",
        r.notes
    );
    let damaged = a.damaged_ranges.clone();
    assert!(!damaged.is_empty(), "{a:?}");
    let (r, out) = repair_with(&x, &aggressive());
    assert_eq!(r.result, RepairResult::PartiallyRepaired, "{r:?}");
    assert!(
        r.applied
            .iter()
            .any(|s| s.contains("damaged restart interval")),
        "{:?}",
        r.applied
    );
    let first = damaged[0];
    assert!(first.byte_start <= restart_off + 2);
    assert_eq!(
        &out[..first.byte_start as usize],
        &x[..first.byte_start as usize]
    );
    // Everything from the RST where decoding resumed is the original.
    let last = damaged[damaged.len() - 1];
    let tail = &x[last.byte_end as usize..];
    assert!(out.ends_with(tail));
    let after = analyze(&out).unwrap();
    assert!(after.is_clean(), "{:?}", after.first_error);
    assert_eq!(after.mcus_decoded, a.mcus_total);
    assert!(zune_strict(&out));
    assert_eq!(zune_dims(&out), Some((1024, 768)));
    let lost: u64 = damaged.iter().map(|r| r.end_mcu - r.first_mcu).sum();
    let frac = r.validation.unwrap().intact_fraction.unwrap();
    assert!((frac - (1.0 - lost as f32 / a.mcus_total as f32)).abs() < 1e-3);
    assert!(frac > 0.99, "{frac}");

    // The plain cut at the restart marker before the damage, on request:
    // the original data up to that marker, then grey MCUs.
    let mut out = Vec::new();
    let r = apply(
        &mem(x.clone()),
        &mut out,
        &[Strategy::TruncateAtLastGoodRestart],
        &RepairContext::default(),
    )
    .unwrap();
    assert_eq!(r.result, RepairResult::PartiallyRepaired, "{r:?}");
    assert_eq!(&out[..restart_off as usize], &x[..restart_off as usize]);
    let after = analyze(&out).unwrap();
    assert!(after.is_clean());
    assert_eq!(
        after.restart_intervals_seen as u64,
        a.mcus_total.div_ceil(4) - 1
    );
    let frac = r.validation.unwrap().intact_fraction.unwrap();
    assert!((frac - restart_mcu as f32 / a.mcus_total as f32).abs() < 1e-3);
    assert!(zune_strict(&out));
}

#[test]
fn aggressive_progressive_repair_keeps_complete_scans() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "prog_noise_1024x768.jpg");
    let cut = &d[..d.len() * 7 / 10];
    let (r, out) = repair_with(cut, &aggressive());
    assert!(
        matches!(
            r.result,
            RepairResult::Repaired | RepairResult::PartiallyRepaired
        ),
        "{r:?}"
    );
    let a = analyze(&out).unwrap();
    assert!(a.is_clean(), "{:?}", a.first_error);
    assert!(zune_strict(&out));
}

#[test]
fn structural_damage_is_fixed() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "base_420_640x480.jpg");
    let app0_end = 4 + u16::from_be_bytes([d[4], d[5]]) as usize;
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    // Stray bytes between segments.
    let mut x = d[..app0_end].to_vec();
    x.extend_from_slice(&[0x00, 0x12, 0x34, 0x56, 0x78]);
    x.extend_from_slice(&d[app0_end..]);
    cases.push(("stray bytes", x));
    // Doubled SOI.
    let mut x = vec![0xFF, 0xD8];
    x.extend_from_slice(&d);
    cases.push(("double SOI", x));
    // Junk before the SOI.
    let mut x = Rng(4).bytes(300);
    x.retain(|&b| b != 0xFF);
    x.extend_from_slice(&d);
    cases.push(("junk before SOI", x));
    // APP1 (Exif) length 37 too long.
    let p = app0_end;
    assert_eq!(&d[p..p + 2], &[0xFF, 0xE1]);
    let mut x = d.clone();
    let l = u16::from_be_bytes([x[p + 2], x[p + 3]]) + 37;
    x[p + 2..p + 4].copy_from_slice(&l.to_be_bytes());
    cases.push(("APP1 length +37", x));
    let f = JpegFormat::new();
    for (what, x) in cases {
        let v = f.validate(&mem(x.clone())).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Damaged,
            "{what}: {:?}",
            v.problems
        );
        assert!(
            v.repairable_by.iter().any(|r| r == "structural_fix"),
            "{what}"
        );
        let (r, out) = repair_with(&x, &RepairContext::default());
        assert_eq!(r.result, RepairResult::Repaired, "{what}: {r:?}");
        let h = walk_headers(&*mem(out.clone()), 0, u64::MAX).unwrap();
        assert!(h.issues.is_empty(), "{what}: {:?}", h.issues);
        assert!(analyze(&out).unwrap().is_clean(), "{what}");
        assert!(zune_strict(&out), "{what}");
        assert_eq!(
            h.exif.unwrap().make.as_deref(),
            Some("DiskRescue"),
            "{what}"
        );
    }
    let mut bad = d.clone();
    let l = u16::from_be_bytes([bad[p + 2], bad[p + 3]]) + 37;
    bad[p + 2..p + 4].copy_from_slice(&l.to_be_bytes());
    let h = walk_headers(&*mem(bad), 0, u64::MAX).unwrap();
    assert!(h
        .issues
        .iter()
        .any(|i| matches!(i, HeaderIssue::BadLength { marker: 0xE1, .. })));
}

#[test]
fn trailing_garbage_is_stripped_but_trailers_are_kept() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    for n in ["base_444_320x240.jpg", "mpf2.jpg", "motion_samsung.jpg"] {
        let d = read(dir, n);
        let mut x = d.clone();
        x.extend_from_slice(&Rng(8).bytes(1000));
        let v = f.validate(&mem(x.clone())).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{n}");
        assert!(
            v.problems.iter().any(|p| p.contains("1000 bytes")),
            "{n}: {:?}",
            v.problems
        );
        assert_eq!(
            v.repairable_by,
            vec!["strip_trailing_garbage".to_string()],
            "{n}"
        );
        let (r, out) = repair_with(&x, &RepairContext::default());
        assert_eq!(r.result, RepairResult::Repaired, "{n}: {r:?}");
        assert_eq!(out, d, "{n}");
    }
}

#[test]
fn missing_eoi_is_appended() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "base_422_200x150.jpg");
    let x = &d[..d.len() - 2];
    let f = JpegFormat::new();
    let v = f.validate(&mem(x.to_vec())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems.iter().any(|p| p.contains("EOI")),
        "{:?}",
        v.problems
    );
    assert_eq!(v.intact_fraction, Some(1.0));
    let (r, out) = repair_with(x, &RepairContext::default());
    assert_eq!(r.result, RepairResult::Repaired, "{r:?}");
    assert_eq!(out, d);
}

#[test]
fn plan_matches_the_damage() {
    let Some(dir) = fixtures() else { return };
    let d = read(dir, "noise_rst_1024x768.jpg");
    let src = mem(d[..d.len() / 2].to_vec());
    let (h, a) = analyze_source(&*src, 0, u64::MAX, &AnalyzeOptions::default()).unwrap();
    assert_eq!(
        plan(&h, &a, &RepairContext::default()),
        vec![Strategy::AppendEoi]
    );
    assert_eq!(
        plan(&h, &a, &aggressive()),
        vec![Strategy::TruncateAtLastGoodMcuRow]
    );
    let (h, a) = analyze_source(&*mem(d.clone()), 0, u64::MAX, &AnalyzeOptions::default()).unwrap();
    assert!(plan(&h, &a, &aggressive()).is_empty());
}
