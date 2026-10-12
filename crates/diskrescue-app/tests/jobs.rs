//! Background jobs on regular-file sources and the fixture scenarios.

mod common;

use std::io::{Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use diskrescue_app::jobs::{
    start_carve, start_image, start_recover, start_repair, start_scan, start_verify, ImageStatus,
    RecoverStatus,
};
use diskrescue_app::plan::{
    CarveMode, CarvePlan, ImagePlan, RecoverPlan, RepairPlan, ScanPlan, VerifyPlan,
};
use diskrescue_app::results;
use diskrescue_app::JobOutcome;
use diskrescue_fixtures::sets::Recoverability;
use diskrescue_imaging::MapFile;
use diskrescue_recover::report::Outcome;
use diskrescue_recover::Stage;

const LIMIT: Duration = Duration::from_secs(300);

#[test]
fn imaging_a_file_runs_to_completion() {
    let dir = common::scratch("img-");
    let src = dir.path().join("source.bin");
    std::fs::write(&src, common::noise(8 << 20, 1)).unwrap();
    let img = dir.path().join("out").join("copy.img");
    let h = start_image(ImagePlan::new(&src, &img));
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    assert!(h.is_finished());
    assert!(
        common::same_content(&src, &img),
        "image differs from source"
    );

    let s = h.snapshot();
    assert_eq!(s.status, ImageStatus::Done);
    let sum = s.summary.as_ref().expect("summary");
    assert!(sum.finished && !sum.cancelled);
    assert_eq!(sum.stats.rescued, 8 << 20);
    assert_eq!(s.copy_size, 8 << 20);
    assert_eq!(s.profile.as_ref().unwrap().name, "generic");
    assert!(s.source.is_some());
    let map = s.map.as_ref().expect("a block map snapshot");
    assert_eq!(map.size(), 8 << 20);
    assert!(!s.logs.is_empty());
    assert!(
        s.resume_command.starts_with("diskrescue image "),
        "{}",
        s.resume_command
    );
    assert_eq!(s.stats().unwrap().rescued, 8 << 20);

    // Running it again finds nothing to do and does not open the source.
    let h = start_image(ImagePlan::new(&src, &img));
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    assert!(s.already_complete, "{s:?}");
    assert_eq!(s.final_stats.unwrap().rescued, 8 << 20);
}

#[test]
fn imaging_pauses_cancels_and_resumes() {
    let dir = common::scratch("resume-");
    // A sparse 256 MiB source copied in 512-byte reads takes a while; real
    // data at both ends shows that nothing is mixed up.
    let src = dir.path().join("big.bin");
    let head = common::noise(1 << 20, 2);
    let tail = common::noise(1 << 20, 3);
    {
        let mut f = std::fs::File::create(&src).unwrap();
        f.set_len(256 << 20).unwrap();
        f.write_all(&head).unwrap();
        f.seek(SeekFrom::Start((255 << 20) as u64)).unwrap();
        f.write_all(&tail).unwrap();
    }
    let img = dir.path().join("big.img");
    let mut plan = ImagePlan::new(&src, &img);
    plan.tuning.block_size = Some(512);
    let h = start_image(plan.clone());
    let t = Instant::now();
    loop {
        let s = h.snapshot();
        if s.progress.as_ref().is_some_and(|p| p.stats.rescued > 0) {
            break;
        }
        assert!(!h.is_finished(), "finished too early: {:?}", h.outcome());
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the copy never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    h.pause(true);
    assert!(h.is_paused() && h.snapshot().paused);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!h.is_finished());
    h.cancel();
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Cancelled);
    let s = h.snapshot();
    assert!(s.stopping);
    let sum = s.summary.expect("summary after cancel");
    assert!(sum.cancelled && !sum.finished);
    let partial = MapFile::load(&img.with_extension("img.map"))
        .unwrap()
        .map
        .stats();
    assert!(
        partial.rescued > 0 && partial.rescued < 256 << 20,
        "{partial:?}"
    );

    // Start the same copy again (with larger reads): it resumes and finishes.
    plan.tuning.block_size = None;
    let h = start_image(plan);
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    let before = s.resumed_from.expect("resumed");
    assert!(before.rescued >= partial.rescued);
    assert!(common::same_content(&src, &img), "resumed image differs");
}

#[test]
fn an_unsafe_image_job_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    std::fs::write(&src, common::noise(4096, 4)).unwrap();
    let h = start_image(ImagePlan::new(&src, &src));
    match common::wait(&h, LIMIT) {
        JobOutcome::Refused(m) => assert!(m.contains("the source itself"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(std::fs::read(&src).unwrap(), common::noise(4096, 4));
    assert!(!dir.path().join("source.bin.map").exists());
}

#[test]
fn recovering_the_camera_card() {
    let Some(sc) = common::camera_card() else {
        return;
    };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let h = start_recover(RecoverPlan::new(img, &out));

    // Snapshots of a running job are cheap.
    let mut n = 0u32;
    let mut spent = Duration::ZERO;
    while !h.is_finished() {
        let t = Instant::now();
        let s = h.snapshot();
        spent += t.elapsed();
        n += 1;
        std::hint::black_box(&s);
        std::thread::sleep(Duration::from_micros(500));
    }
    assert!(n > 0);
    let avg = spent / n;
    assert!(
        avg < Duration::from_millis(1),
        "a snapshot took {avg:?} on average over {n}"
    );

    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    assert_eq!(s.status, RecoverStatus::Done);
    assert!(!s.interrupted);
    let report = s.report.as_ref().expect("report");
    assert_eq!(report.totals.files_written, 11);
    assert_eq!(report.totals.deleted, 2);
    let p = &s.pipeline;
    assert_eq!(p.counts.total(), report.files.len() as u64);
    assert_eq!(
        p.counts.get(Outcome::Recovered) + p.counts.get(Outcome::RecoveredUnverified),
        report
            .files
            .iter()
            .filter(|f| matches!(f.outcome, Outcome::Recovered | Outcome::RecoveredUnverified))
            .count() as u64
    );
    assert!(!p.recent_files.is_empty());
    assert_eq!(p.volumes.len(), 1);
    assert_eq!(p.volumes[0].label.as_deref(), Some("EOS_DIGITAL"));
    for st in [Stage::Partitions, Stage::Extracting, Stage::Validating] {
        assert!(p.stages.iter().any(|x| x.stage == st), "{st:?} missing");
    }
    assert!(p.stages.iter().all(|x| x.took.is_some()));
    let extracting = p
        .stages
        .iter()
        .find(|x| x.stage == Stage::Extracting)
        .unwrap();
    assert_eq!(extracting.done, extracting.total);
    assert!(p.carve.is_some(), "auto carving looks at the free space");

    // The results can be read back.
    let loaded = results::load(&out).unwrap();
    assert_eq!(loaded.totals.files_written, 11);
    let items = results::items(&loaded, &out);
    assert_eq!(items.len(), loaded.files.len());
    let written: Vec<_> = items.iter().filter_map(|i| i.path.as_ref()).collect();
    assert_eq!(written.len(), 11);
    assert!(written.iter().all(|p| p.is_file() && p.starts_with(&out)));
    assert_eq!(items.iter().filter(|i| i.deleted).count(), 2);
    let photo = items
        .iter()
        .find(|i| i.origin.ends_with("IMG_0001.JPG"))
        .expect("IMG_0001.JPG");
    assert!(photo.captured.is_some());
    assert!(results::thumbnail(photo.path.as_ref().unwrap(), 96).is_some());
    let truth: usize = sc
        .files
        .iter()
        .filter(|f| f.expect == Recoverability::Live)
        .count();
    assert!(truth >= 9);
}

#[test]
fn recovering_with_an_image_first_fills_the_nested_state() {
    let Some(sc) = common::camera_card() else {
        return;
    };
    let src = sc.disk_image.as_ref().unwrap();
    let dir = common::scratch("imageto-");
    let mut plan = RecoverPlan::new(src, dir.path().join("out"));
    plan.image_to = Some(dir.path().join("copy.img"));
    plan.carve = CarveMode::Off;
    let h = start_recover(plan);
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    let img = s.image.as_ref().expect("nested imaging state");
    assert!(img.summary.as_ref().is_some_and(|x| x.finished));
    assert!(img.map.is_some());
    assert!(common::same_content(src, &dir.path().join("copy.img")));
    let report = s.report.unwrap();
    assert_eq!(report.totals.files_written, 11);
    assert!(report
        .source
        .mapfile
        .as_ref()
        .is_some_and(|m| m.ends_with("copy.img.map")));
}

#[test]
fn cancelling_a_recovery_keeps_a_report() {
    let Some(sc) = common::camera_card() else {
        return;
    };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let h = start_carve(CarvePlan::new(img, &out));
    h.cancel();
    let o = common::wait(&h, LIMIT);
    assert!(
        matches!(o, JobOutcome::Cancelled | JobOutcome::Finished),
        "{o:?}"
    );
    let s = h.snapshot();
    assert!(s.stopping);
    if o == JobOutcome::Cancelled {
        assert!(s.interrupted || s.report.is_none());
    }
}

#[test]
fn scanning_writes_nothing() {
    let Some(sc) = common::camera_card() else {
        return;
    };
    let img = sc.disk_image.as_ref().unwrap();
    let before = std::fs::metadata(img).unwrap().modified().unwrap();
    let h = start_scan(ScanPlan {
        source: img.clone(),
        ..ScanPlan::default()
    });
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    let survey = s.survey.expect("survey");
    assert_eq!(survey.files.len(), 11);
    assert_eq!(survey.files.iter().filter(|f| f.deleted).count(), 2);
    assert_eq!(s.pipeline.volumes.len(), 1);
    assert_eq!(std::fs::metadata(img).unwrap().modified().unwrap(), before);
}

#[test]
fn repairing_and_verifying_damaged_media() {
    let Some(sc) = common::damaged_media() else {
        return;
    };
    let root = &sc.root;
    let dir = tempfile::tempdir().unwrap();
    let fixed = dir.path().join("fixed");
    let inputs = vec![root.join("truncated.jpg"), root.join("no_moov.mp4")];
    let h = start_repair(RepairPlan {
        files: inputs.clone(),
        output: fixed.clone(),
        donors: vec![root.join("donors/donor.mp4")],
        aggressive: false,
    });
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    assert!(s.done);
    assert_eq!(s.inputs, 2);
    assert_eq!(s.files.len(), 2, "{:?}", s.files);
    let report = s.report.expect("report");
    assert!(report
        .files
        .iter()
        .all(|f| matches!(f.outcome, Outcome::Repaired | Outcome::PartiallyRepaired)));
    let repaired_mp4 = fixed.join("repaired/no_moov.mp4");
    assert!(repaired_mp4.is_file());
    let items = results::items(&results::load(&fixed).unwrap(), &fixed);
    assert!(items
        .iter()
        .all(|i| i.repaired.as_ref().is_some_and(|p| p.is_file())));

    let mut files = inputs;
    files.push(repaired_mp4);
    files.push(root.join("originals").join("*.png"));
    let h = start_verify(VerifyPlan { files });
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    assert!(s.done);
    assert_eq!(s.results.len(), s.total);
    assert!(s.total >= 5, "{}", s.total);
    let verdict = |name: &str| {
        let r = s
            .results
            .iter()
            .find(|r| r.path.ends_with(name))
            .unwrap_or_else(|| panic!("{name} missing"));
        results::verify_verdict(r)
    };
    assert_eq!(verdict("no_moov.mp4").0, "DAMAGED");
    assert!(!verdict("repaired/no_moov.mp4").1);
    assert_eq!(
        s.problems,
        s.results
            .iter()
            .filter(|r| results::verify_verdict(r).1)
            .count()
    );

    // Unusable inputs are refused before anything runs.
    let h = start_repair(RepairPlan {
        files: vec![dir.path().join("nothing.jpg")],
        output: dir.path().join("x"),
        ..RepairPlan::default()
    });
    assert!(
        matches!(common::wait(&h, LIMIT), JobOutcome::Refused(m) if m.contains("does not exist"))
    );
    assert!(!dir.path().join("x").exists());
}
