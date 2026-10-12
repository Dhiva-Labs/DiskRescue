//! Validation and the repair strategies of `Mp4Format`.

mod common;

use common::*;
use diskrescue_core::format::{FileFormat, RepairContext, RepairResult, ValidationStatus};
use diskrescue_core::Source;
use diskrescue_isobmff::analysis::analyze;
use diskrescue_isobmff::repair::{apply, plan, Strategy};
use diskrescue_isobmff::Mp4Format;

fn repair_to(
    name: &str,
    bytes: Vec<u8>,
    ctx: &RepairContext,
) -> (std::path::PathBuf, RepairResult) {
    let src = mem(bytes);
    let out_path = scratch(&format!("{name}.repaired.mp4"));
    let mut out = std::fs::File::create(&out_path).unwrap();
    let o = Mp4Format.repair(&src, &mut out, ctx).expect("repair");
    drop(out);
    if o.result != RepairResult::Unrepairable {
        assert_eq!(o.output_len, std::fs::metadata(&out_path).unwrap().len());
        assert!(o.validation.is_some());
    }
    (out_path, o.result)
}

#[test]
fn healthy_files_validate() {
    if !ffmpeg_or_skip() {
        return;
    }
    for name in [
        "h264",
        "h264_faststart",
        "h265",
        "mov",
        "mov_pcm",
        "3gp",
        "m4a",
    ] {
        let src = open(&fixture(name));
        let v = Mp4Format.validate(&src).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Valid,
            "{name}: {:?}",
            v.problems
        );
        assert_eq!(v.intact_fraction, Some(1.0));
        assert!(v.repairable_by.is_empty());
        assert!(v.meta.duration_ms.is_some());
        // Repairing a healthy file is a no-op copy.
        let (out, r) = repair_to(
            name,
            std::fs::read(fixture(name)).unwrap(),
            &RepairContext::default(),
        );
        assert_eq!(r, RepairResult::NoRepairNeeded);
        assert_eq!(
            std::fs::read(out).unwrap(),
            std::fs::read(fixture(name)).unwrap()
        );
    }
}

#[test]
fn missing_moov_needs_a_reference() {
    if !ffmpeg_or_skip() {
        return;
    }
    let dmg = strip_box(&std::fs::read(fixture("h264")).unwrap(), b"moov");
    let v = Mp4Format.validate(&mem(dmg.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert_eq!(v.problems[0], "moov missing");
    assert!(v
        .repairable_by
        .contains(&"rebuild_moov (needs a reference clip)".to_string()));
    // Without a donor nothing is written.
    let (out, r) = repair_to("nodonor", dmg.clone(), &RepairContext::default());
    assert_eq!(r, RepairResult::Unrepairable);
    assert_eq!(std::fs::metadata(out).unwrap().len(), 0);
    // With a donor the moov is rebuilt.
    let ctx = RepairContext {
        donors: vec![fixture("h264_ref")],
        ..Default::default()
    };
    let (out, r) = repair_to("withdonor", dmg, &ctx);
    assert!(
        matches!(r, RepairResult::Repaired | RepairResult::PartiallyRepaired),
        "{r:?}"
    );
    decodes_clean(&out).unwrap();
    ffprobe_clean(&out).unwrap();
}

#[test]
fn clip_tables_on_truncated_faststart() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264_faststart");
    let bytes = std::fs::read(&orig).unwrap();
    let cut = bytes.len() * 2 / 3;
    let dmg = bytes[..cut].to_vec();
    let v = Mp4Format.validate(&mem(dmg.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems.iter().any(|p| p.starts_with("mdat truncated: ")),
        "{:?}",
        v.problems
    );
    assert!(v.repairable_by.contains(&"clip_tables_to_file".to_string()));
    assert!(v.intact_fraction.unwrap() < 1.0);
    let (out, r) = repair_to("clip", dmg, &RepairContext::default());
    assert_eq!(r, RepairResult::PartiallyRepaired);
    decodes_clean(&out).unwrap();
    ffprobe_clean(&out).unwrap();
    for sel in ["v", "a"] {
        let kept = packets(&orig, sel)
            .iter()
            .filter(|p| p.pos + p.size <= cut as u64)
            .count();
        assert_eq!(packets(&out, sel).len(), kept, "stream {sel}");
    }
    let v = Mp4Format.validate(&open(&out)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn fix_mdat_size_restores_the_original() {
    if !ffmpeg_or_skip() {
        return;
    }
    for name in ["h264_faststart", "h264"] {
        let orig = std::fs::read(fixture(name)).unwrap();
        let dmg = zero_box_size(&orig, b"mdat");
        let a = analyze(&*mem(dmg.clone())).unwrap();
        let p = plan(&a, &RepairContext::default());
        assert_eq!(p, vec![Strategy::FixMdatSize], "{name}: {:?}", a.problems);
        let (out, r) = repair_to(&format!("fix_{name}"), dmg, &RepairContext::default());
        assert_eq!(r, RepairResult::Repaired, "{name}");
        assert_eq!(std::fs::read(out).unwrap(), orig, "{name}");
    }
}

#[test]
fn trailing_garbage_is_stripped() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = std::fs::read(fixture("h264")).unwrap();
    let mut dmg = orig.clone();
    dmg.extend(noise(10_000, 5));
    let v = Mp4Format.validate(&mem(dmg.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v
        .repairable_by
        .contains(&"strip_trailing_garbage".to_string()));
    let (out, r) = repair_to("garbage", dmg, &RepairContext::default());
    assert_eq!(r, RepairResult::Repaired);
    assert_eq!(std::fs::read(out).unwrap(), orig);
}

#[test]
fn offsets_are_relocated() {
    if !ffmpeg_or_skip() {
        return;
    }
    // Remove the 8-byte 'free' box between moov and mdat: every chunk
    // offset now points 8 bytes too far.
    let orig_path = fixture("h264_faststart");
    let orig = std::fs::read(&orig_path).unwrap();
    let dmg = strip_box(&orig, b"free");
    assert_eq!(dmg.len() + 8, orig.len());
    let a = analyze(&*mem(dmg.clone())).unwrap();
    assert_eq!(a.relocation_delta, Some(-8), "{:?}", a.problems);
    let p = plan(&a, &RepairContext::default());
    assert_eq!(p[0], Strategy::RelocateOffsets);
    let (out, r) = repair_to("relocate", dmg, &RepairContext::default());
    assert_eq!(r, RepairResult::Repaired);
    decodes_clean(&out).unwrap();
    let (a, b) = (packets(&orig_path, "v"), packets(&out, "v"));
    assert_eq!(a.len(), b.len());
}

#[test]
fn move_moov_first() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig_path = fixture("h264");
    let src = open(&orig_path);
    let out_path = scratch("faststart.mp4");
    let mut out = std::fs::File::create(&out_path).unwrap();
    let o = apply(
        &src,
        &mut out,
        &[Strategy::MoveMoovFirst],
        &RepairContext::default(),
    )
    .unwrap();
    drop(out);
    assert_eq!(o.result, RepairResult::Repaired, "{o:?}");
    assert_eq!(o.output_len, src.size());
    let bytes = std::fs::read(&out_path).unwrap();
    let order: Vec<[u8; 4]> = top_boxes(&bytes).iter().map(|b| b.0).collect();
    let moov = order.iter().position(|k| k == b"moov").unwrap();
    let mdat = order.iter().position(|k| k == b"mdat").unwrap();
    assert!(moov < mdat);
    decodes_clean(&out_path).unwrap();
    let (a, b) = (packets(&orig_path, "a"), packets(&out_path, "a"));
    assert_eq!(a.len(), b.len());
}

#[test]
fn stale_moov_is_detected_and_rebuilt() {
    if !ffmpeg_or_skip() {
        return;
    }
    // The moov of another recording glued to this mdat (case G).
    let this = std::fs::read(fixture("h264")).unwrap();
    let other = std::fs::read(fixture("h264_ref")).unwrap();
    let other_moov = top_boxes(&other)
        .into_iter()
        .find(|b| &b.0 == b"moov")
        .unwrap();
    let mut dmg = strip_box(&this, b"moov");
    dmg.extend_from_slice(&other[other_moov.1..other_moov.1 + other_moov.2]);
    let a = analyze(&*mem(dmg.clone())).unwrap();
    assert!(a.moov.is_some());
    assert!(a.moov_stale(), "{:?}", a.problems);
    let v = Mp4Format.validate(&mem(dmg.clone())).unwrap();
    assert!(v
        .repairable_by
        .contains(&"rebuild_moov (needs a reference clip)".to_string()));
    let ctx = RepairContext {
        donors: vec![fixture("h264_ref")],
        ..Default::default()
    };
    let (out, r) = repair_to("stale", dmg, &ctx);
    assert!(matches!(
        r,
        RepairResult::Repaired | RepairResult::PartiallyRepaired
    ));
    decodes_clean(&out).unwrap();
    let frames = streams(&out)
        .iter()
        .find(|s| s.kind == "video")
        .unwrap()
        .frames;
    assert_eq!(frames, 90);
}

#[test]
fn zeroed_moov_is_rejected() {
    if !ffmpeg_or_skip() {
        return;
    }
    // Firmware that preallocates the moov and crashes leaves zeros.
    let orig = std::fs::read(fixture("h264_faststart")).unwrap();
    let mut dmg = orig.clone();
    let moov = top_boxes(&orig)
        .into_iter()
        .find(|b| &b.0 == b"moov")
        .unwrap();
    for b in &mut dmg[moov.1 + 8..moov.1 + moov.2] {
        *b = 0;
    }
    let v = Mp4Format.validate(&mem(dmg)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems[0].starts_with("moov corrupt"),
        "{:?}",
        v.problems
    );
}
