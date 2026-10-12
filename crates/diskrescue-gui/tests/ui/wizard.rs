//! The wizard's safety checks: blockers disable Start; only overridable
//! ones can be accepted; administrator rights get their own banner. The
//! checks are made-up results (no real device is involved).

use std::path::PathBuf;
use std::time::Duration;

use crate::common::*;
use diskrescue_app::preflight::{Blocker, BlockerCode, Override, Preflight};
use diskrescue_gui::Page;
use egui_kittest::kittest::{NodeT, Queryable};

const LIMIT: Duration = Duration::from_secs(20);

fn blocker(code: BlockerCode, message: &str, over: Option<Override>) -> Blocker {
    Blocker {
        code,
        message: message.into(),
        override_flag: over,
    }
}

fn open_wizard(h: &mut egui_kittest::Harness<'static, diskrescue_gui::DiskRescueApp>, name: &str) {
    wait_for(h, LIMIT, "the drive list", |h| {
        has_label(h, &format!("Rescue {name}"))
    });
    click(h, &format!("Rescue {name}"));
    assert_eq!(h.state().page, Page::Wizard);
    assert!(has_label(h, "Photos"));
    click(h, "Next");
}

#[test]
fn an_overridable_blocker_needs_the_risk_accepted_before_start() {
    let (app, backend) = fake_app(fake_devices());
    *backend.preflight.lock().unwrap() = Some(Preflight {
        blockers: vec![blocker(
            BlockerCode::SourceMounted,
            "/dev/sdb is mounted at /media/me/SANDISK. Unmount it first, or add --allow-mounted.",
            Some(Override::AllowMounted),
        )],
        warnings: vec![],
        resolved: None,
    });
    let mut h = harness(app);
    open_wizard(&mut h, "SanDisk Ultra");
    // Step 2: the defaults are filled in; the copy is on by default.
    assert!(has_label(&h, "Save recovered files to"));
    assert!(has_label(
        &h,
        "Copy the drive to an image first (recommended for failing drives)"
    ));
    assert!(h.state().wizard.as_ref().unwrap().image_first);
    wait_for(&mut h, LIMIT, "Next", |h| enabled(h, "Next"));
    click(&mut h, "Next");

    wait_for(&mut h, LIMIT, "the safety checks", |h| {
        has_label(h, "The drive is in use by the system (mounted)")
    });
    assert!(
        !has_label_containing(&h, "--allow-mounted"),
        "command-line hints are hidden"
    );
    assert!(!enabled(&h, "Start rescue"), "a blocker must disable Start");
    click(&mut h, "I understand the risk");
    assert!(
        enabled(&h, "Start rescue"),
        "accepting the risk enables Start"
    );
    let plan = h.state().wizard.as_ref().unwrap().start_plan();
    assert!(plan.allow_mounted);
    assert!(plan.image_to.is_some());
    click(&mut h, "I understand the risk");
    assert!(!enabled(&h, "Start rescue"));
    assert!(
        !h.state()
            .wizard
            .as_ref()
            .unwrap()
            .start_plan()
            .allow_mounted
    );
    click(&mut h, "I understand the risk");

    // Neutral paths for the screenshot.
    {
        let w = h.state_mut().wizard.as_mut().unwrap();
        w.output = "/media/backup/SanDisk Ultra - recovered".into();
        w.image_path = "/media/backup/SanDisk Ultra.img".into();
    }
    wait_for(&mut h, LIMIT, "the checks of the new paths", |h| {
        enabled(h, "Start rescue")
    });
    screenshot(h, "wizard-review");
}

#[test]
fn a_blocker_that_cannot_be_overridden_keeps_start_disabled() {
    let (app, backend) = fake_app(fake_devices());
    *backend.preflight.lock().unwrap() = Some(Preflight {
        blockers: vec![
            blocker(
                BlockerCode::OutputOnSource,
                "the output folder /media/me/SANDISK/out is on the device being recovered",
                None,
            ),
            blocker(
                BlockerCode::SourceMounted,
                "/dev/sdb is mounted.",
                Some(Override::AllowMounted),
            ),
        ],
        warnings: vec!["Reading directly from the device. That is fine.".into()],
        resolved: None,
    });
    let mut h = harness(app);
    open_wizard(&mut h, "SanDisk Ultra");
    // The output problem is shown on the "where" step and blocks Next.
    wait_for(&mut h, LIMIT, "the output check", |h| {
        has_label(h, "That place is on the drive being rescued")
    });
    assert!(!enabled(&h, "Next"));
    // Even on the last step (reached directly), Start stays off after the
    // overridable one is accepted.
    h.state_mut().wizard.as_mut().unwrap().step = 2;
    wait_for(&mut h, LIMIT, "the review", |h| {
        has_label(h, "I understand the risk")
    });
    click(&mut h, "I understand the risk");
    assert!(!enabled(&h, "Start rescue"));
    assert_eq!(
        h.query_all_by_label("I understand the risk").count(),
        1,
        "only the overridable blocker has a checkbox"
    );
    assert!(has_label(&h, "Change where to save"));
}

#[test]
fn a_drive_that_needs_administrator_rights_gets_both_options() {
    let devices = fake_devices();
    let backend = FakeBackend {
        needs_privilege: [PathBuf::from("/dev/mmcblk0")].into_iter().collect(),
        ..FakeBackend::new(devices)
    };
    *backend.preflight.lock().unwrap() = Some(Preflight {
        blockers: vec![blocker(
            BlockerCode::PermissionDenied,
            "/dev/mmcblk0: Permission denied (os error 13). Run as root.",
            None,
        )],
        warnings: vec![],
        resolved: None,
    });
    let app = diskrescue_gui::DiskRescueApp::with_backend(
        std::sync::Arc::new(backend),
        diskrescue_gui::Saved::default(),
    );
    let mut h = harness(app);
    wait_for(&mut h, LIMIT, "the badge", |h| {
        has_label(h, "Needs administrator rights")
    });
    open_wizard(&mut h, "SD64G");
    // The copy is required (it is what runs with administrator rights).
    let sw = h.get_by_label("Copy the drive to an image first (recommended for failing drives)");
    assert!(sw.accesskit_node().is_disabled());
    wait_for(&mut h, LIMIT, "Next", |h| enabled(h, "Next"));
    click(&mut h, "Next");
    wait_for(&mut h, LIMIT, "the banner", |h| {
        has_label(
            h,
            "DiskRescue needs administrator rights to read this drive",
        )
    });
    assert!(!enabled(&h, "Start rescue"));
    if cfg!(target_os = "linux") {
        // Never clicked here: it would ask for a password for real.
        assert!(enabled(&h, "Copy with administrator rights"));
        assert!(has_label(&h, "Copy command"));
    }
}

#[test]
fn the_administrator_copy_is_followed_by_recovery_with_normal_rights() {
    let Some(sc) = camera_card() else {
        return;
    };
    let image = sc.disk_image.clone().unwrap();
    // A made-up card whose device node is the fixture image.
    let size = std::fs::metadata(&image).unwrap().len();
    let card = device(
        image.to_str().unwrap(),
        diskrescue_core::device::DeviceKind::SdCard,
        "",
        "EOS card",
        size,
    );
    let backend = FakeBackend {
        needs_privilege: [image.clone()].into_iter().collect(),
        ..FakeBackend::new(vec![card])
    };
    *backend.preflight.lock().unwrap() = Some(Preflight {
        blockers: vec![blocker(
            BlockerCode::PermissionDenied,
            "Permission denied (os error 13).",
            None,
        )],
        warnings: vec![],
        resolved: None,
    });
    let app = diskrescue_gui::DiskRescueApp::with_backend(
        std::sync::Arc::new(backend),
        diskrescue_gui::Saved::default(),
    );
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("card copy.img");
    let out = dir.path().join("Recovered");
    let mut h = harness(app);
    open_wizard(&mut h, "EOS card");
    type_into(
        &mut h,
        "Save recovered files to",
        &out.display().to_string(),
    );
    type_into(&mut h, "Image file", &copy.display().to_string());
    wait_for(&mut h, LIMIT, "Next", |h| enabled(h, "Next"));
    click(&mut h, "Next");
    wait_for(&mut h, LIMIT, "the banner", |h| {
        enabled(h, "Copy with administrator rights")
    });
    click(&mut h, "Copy with administrator rights");
    assert_eq!(h.state().page, Page::Progress);
    assert!(has_label(
        &h,
        "Step 1 of 2: Copying the drive with administrator rights"
    ));
    wait_for(
        &mut h,
        Duration::from_secs(300),
        "the copy and the recovery",
        |h| has_label(h, "See your recovered files"),
    );
    assert!(has_label_containing(
        &h,
        "Finished: 11 files were recovered"
    ));
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        std::fs::read(&image).unwrap()
    );
    assert!(out.join("report.json").is_file());
}
