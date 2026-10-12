//! The whole rescue of an image file, driven through the user interface:
//! paths typed into the fields, Start, the job runs to the end, and the
//! results grid shows what came back (deleted files included). Then the
//! results page on its own: reopening a folder and the filters.

use std::time::Duration;

use crate::common::*;
use diskrescue_app::jobs::start_recover;
use diskrescue_app::plan::RecoverPlan;
use diskrescue_app::JobOutcome;
use diskrescue_core::media::MediaCategory;
use diskrescue_gui::Page;
use diskrescue_recover::report::{FileRecord, Origin, Outcome, Report};

const JOB_LIMIT: Duration = Duration::from_secs(300);
const UI_LIMIT: Duration = Duration::from_secs(30);

const ALL_FILES: [&str; 11] = [
    "IMG_0001.JPG",
    "IMG_0002.JPG",
    "IMG_0003.JPG",
    "IMG_0004.JPG",
    "IMG_0005.JPG",
    "IMG_0006.JPG",
    "IMG_0007.JPG",
    "IMG_0008.JPG",
    "CRW_0006.CR2",
    "MVI_0003.MP4",
    "MVI_0009.MP4",
];

#[test]
fn recovering_an_image_file_through_the_ui() {
    let Some(sc) = camera_card() else {
        return;
    };
    let image = sc.disk_image.clone().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("Recovered photos");
    let (app, _) = fake_app(vec![]);
    let mut h = harness(app);

    click(&mut h, "Recover from an image file");
    type_into(&mut h, "Image file", &image.display().to_string());
    wait_for(&mut h, UI_LIMIT, "the image to be looked at", |h| {
        enabled(h, "Continue")
    });
    assert!(has_label_containing(&h, "Size: "));
    click(&mut h, "Continue");
    assert_eq!(h.state().page, Page::Wizard);

    // Step 1: the defaults (photos, RAW, videos; deleted and existing).
    assert!(has_label(&h, "Photos"));
    click(&mut h, "Next");

    // Step 2: an image file is never copied again.
    assert!(!has_label(
        &h,
        "Copy the drive to an image first (recommended for failing drives)"
    ));
    type_into(
        &mut h,
        "Save recovered files to",
        &out.display().to_string(),
    );
    assert_eq!(
        h.state().wizard.as_ref().unwrap().output,
        out.display().to_string()
    );
    wait_for(&mut h, UI_LIMIT, "Next", |h| enabled(h, "Next"));
    click(&mut h, "Next");

    // Step 3: the real safety checks pass.
    wait_for(&mut h, UI_LIMIT, "the safety checks", |h| {
        enabled(h, "Start rescue")
    });
    assert!(has_label(&h, "Everything is ready"));
    click(&mut h, "Start rescue");
    assert_eq!(h.state().page, Page::Progress);

    wait_for(&mut h, JOB_LIMIT, "the rescue to finish", |h| {
        has_label(h, "See your recovered files")
    });
    assert!(has_label_containing(
        &h,
        "Finished: 11 files were recovered"
    ));
    assert!(!h.state().busy());
    click(&mut h, "See your recovered files");
    assert_eq!(h.state().page, Page::Results);

    for name in ALL_FILES {
        assert!(has_label(&h, name), "{name} is not in the grid");
    }
    assert!(has_label(&h, "Deleted (2)"));
    assert!(has_label(&h, "All (11)"));
    assert_eq!(
        h.state().saved.recent_results.first(),
        Some(&out),
        "the folder is remembered"
    );

    // The detail panel of a deleted photo.
    click(&mut h, "IMG_0004.JPG");
    assert!(has_label(&h, "Close details"));
    assert!(has_label(&h, "Preview of IMG_0004.JPG"));
    assert!(has_label_containing(&h, "(it had been deleted)"));
    assert!(has_label(&h, "Taken"));
    wait_for(&mut h, UI_LIMIT, "the thumbnails", |h| {
        !h.state().thumbnails_busy()
    });
    h.state_mut().results.shown_folder = Some("/media/backup/Recovered photos".into());
    screenshot(h, "results");
}

#[test]
fn results_folders_reopen_and_filter() {
    let Some(sc) = camera_card() else {
        return;
    };
    let image = sc.disk_image.clone().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let job = start_recover(RecoverPlan::new(&image, &out));
    assert_eq!(job.wait_timeout(JOB_LIMIT), Some(JobOutcome::Finished));

    let (app, _) = fake_app(vec![]);
    let mut h = harness(app);
    click(&mut h, "Results");
    type_into(&mut h, "Results folder", &out.display().to_string());
    click(&mut h, "Open");
    wait_for(&mut h, UI_LIMIT, "the results", |h| {
        has_label(h, "All (11)")
    });
    assert_eq!(h.state().saved.recent_results, vec![out.clone()]);

    let shown = |h: &egui_kittest::Harness<'_, diskrescue_gui::DiskRescueApp>| {
        ALL_FILES
            .iter()
            .filter(|n| has_label(h, n))
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(shown(&h).len(), 11);

    click(&mut h, "Deleted (2)");
    assert_eq!(shown(&h), vec!["IMG_0004.JPG", "MVI_0009.MP4"]);

    click(&mut h, "Videos (2)");
    assert_eq!(shown(&h), vec!["MVI_0003.MP4", "MVI_0009.MP4"]);

    click(&mut h, "Photos (9)");
    let photos = shown(&h);
    assert_eq!(photos.len(), 9);
    assert!(photos.iter().all(|n| !n.ends_with(".MP4")));

    // Nothing in these results is damaged or repaired.
    assert!(has_label(&h, "Damaged (0)"));
    assert!(has_label(&h, "Repaired (0)"));
    click(&mut h, "Damaged (0)");
    assert!(has_label(&h, "No files match."));

    click(&mut h, "All (11)");
    type_into(&mut h, "Search", "mvi_0003");
    assert_eq!(shown(&h), vec!["MVI_0003.MP4"]);

    // Back to the folder list: the folder is in the recent list.
    type_into(&mut h, "Search", "");
    click(&mut h, "Open another folder");
    assert!(has_label(&h, &out.display().to_string()));
    click(&mut h, &out.display().to_string());
    wait_for(&mut h, UI_LIMIT, "the results again", |h| {
        has_label(h, "All (11)")
    });
}

#[test]
fn a_damaged_result_is_repaired_with_a_reference_from_the_same_results() {
    let Some(sc) = damaged_media() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Recovered");
    let videos = folder.join("carved").join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::copy(sc.root.join("no_moov.mp4"), videos.join("no_moov.mp4")).unwrap();
    std::fs::copy(
        sc.root.join("donors/donor.mp4"),
        videos.join("MVI_0100.MP4"),
    )
    .unwrap();
    let mut report = Report::new(None);
    for (name, outcome) in [
        ("no_moov.mp4", Outcome::Damaged),
        ("MVI_0100.MP4", Outcome::Recovered),
    ] {
        let p = videos.join(name);
        let mut f = FileRecord::new(
            Origin::Input { path: p.clone() },
            MediaCategory::Video,
            std::fs::metadata(&p).unwrap().len(),
        );
        f.output = Some(std::path::PathBuf::from("carved/videos").join(name));
        f.format = Some("mp4".into());
        f.outcome = outcome;
        report.add_file(f);
    }

    let (app, _) = fake_app(vec![]);
    let mut h = harness(app);
    h.state_mut()
        .results
        .set_report(&folder, std::sync::Arc::new(report));
    click(&mut h, "Results");
    assert!(has_label(&h, "Damaged (1)"));
    click(&mut h, "no_moov.mp4");
    click(&mut h, "Repair with a reference file...");
    assert!(has_label(&h, "Use MVI_0100.MP4 as the reference"));
    click(&mut h, "Use MVI_0100.MP4 as the reference");
    assert_eq!(h.state().page, Page::Repair);
    wait_for(&mut h, JOB_LIMIT, "the repair", |h| {
        has_label_containing(h, "Done: ")
    });
    assert!(has_label_containing(&h, "Done: 1 of 1 file repaired"));
    let repaired = folder
        .join("repairs")
        .join("no_moov")
        .join("repaired")
        .join("no_moov.mp4");
    assert!(repaired.is_file(), "{}", repaired.display());
    assert!(!folder.join("report.previous.json").exists());
}
