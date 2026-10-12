//! A device that needs administrator rights is copied by a helper program;
//! here the helper is the real `diskrescue` program run as the current user,
//! so that everything but the password prompt is exercised.

mod common;

use crossterm::event::KeyCode;
use diskrescue_core::device::DeviceKind;
use diskrescue_tui::config::{Config, Elevation, FixedDevice};
use diskrescue_tui::screens::help::ScreenId;
use diskrescue_tui::testing::*;
use diskrescue_tui::{App, Event};

fn contains_all(screen: &str, needles: &[&str]) {
    let flat = common::flatten(screen);
    for n in needles {
        assert!(
            screen.contains(n) || flat.contains(&common::flatten(n)),
            "missing {n:?} in:\n{screen}"
        );
    }
}

#[test]
fn a_rescue_of_a_device_that_needs_root_runs_as_copy_then_recovery() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-helper-");
    let out = home.path().join("rescued");
    let mut info = device(&img.display().to_string(), DeviceKind::SdCard, 272 << 20);
    info.model = Some("Camera card".into());
    let mut cfg = Config::for_tests(
        home.path(),
        vec![FixedDevice {
            info,
            needs_root: true,
        }],
    );
    cfg.elevation = Elevation::Program(cli);
    let mut app = App::new(cfg);
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    contains_all(&render(&app, 100, 30), &["[needs root]"]);

    // Rescue: the check says that a password will be asked for.
    press(&mut app, KeyCode::Enter);
    contains_all(
        &render(&app, 100, 30),
        &["Your password will be asked for when the copy starts"],
    );
    press(&mut app, KeyCode::Enter);
    for _ in 0..3 {
        press(&mut app, KeyCode::PageUp);
    }
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "ready to start",
            "Your password will be asked for, to read the device",
            "Step 1: copy",
            "Step 2: recover files from the image",
        ],
    );
    assert!(
        !s.contains("PROBLEM"),
        "permission is not a problem here:\n{s}"
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run);
    let run = app.run().unwrap();
    assert!(run.privileged && run.step == (1, 2));

    // Step 1 is the helper's copy, shown like any other.
    let ok = tick_until(&mut app, std::time::Duration::from_secs(120), |a| {
        a.run().is_some_and(|r| r.step.0 == 2 || !r.is_running())
    });
    assert!(ok, "{}", render(&app, 100, 30));
    common::wait_for_job(&mut app);
    let run = app.run().unwrap();
    assert!(
        run.outcome.as_ref().unwrap().is_finished(),
        "{:?}\n{}",
        run.outcome,
        render(&app, 100, 30)
    );
    assert_eq!(run.step, (2, 2));
    let s = render(&app, 100, 30);
    contains_all(&s, &["Finished", "Done:", "recovered", "Step 2 of 2"]);
    // The copy is there, with its progress map, and the files came back.
    assert!(std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".img")));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Results);
    contains_all(&render(&app, 100, 30), &["Deleted (2)", "IMG_0004.JPG"]);
}

#[test]
fn the_helper_cannot_be_paused_but_can_be_stopped() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let home = common::scratch("tui-helper2-");
    let src = home.path().join("big.bin");
    {
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(256 << 20).unwrap();
    }
    let mut info = device(&src.display().to_string(), DeviceKind::UsbFlash, 256 << 20);
    info.model = Some("Big stick".into());
    let mut cfg = Config::for_tests(
        home.path(),
        vec![FixedDevice {
            info,
            needs_root: true,
        }],
    );
    cfg.elevation = Elevation::Program(cli);
    let mut app = App::new(cfg);
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    // "Copy to an image only", with small reads so that it stays running.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    for _ in 0..3 {
        press(&mut app, KeyCode::PageUp);
    }
    common::replace_text(&mut app, &home.path().join("copy").display().to_string());
    // Rows: folder, image, More options, profile... block size is the last.
    for _ in 0..2 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    type_text(&mut app, "512");
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    press(&mut app, KeyCode::Enter);
    let started = tick_until(&mut app, std::time::Duration::from_secs(120), |a| {
        render(a, 120, 40).contains("Copied")
    });
    assert!(started, "{}", render(&app, 120, 40));
    // The helper has no pause: the footer does not offer it and p explains.
    assert!(!render(&app, 120, 40).contains("pause"));
    app.handle_event(ch('p'));
    assert!(app.status().is_some_and(|s| s.contains("cannot be paused")));
    app.handle_event(ch('s'));
    app.handle_event(ch('y'));
    let ok = tick_until(&mut app, std::time::Duration::from_secs(120), |a| {
        !a.job_running()
    });
    assert!(ok);
    contains_all(
        &render(&app, 100, 30),
        &[
            "Stopped",
            "Everything done so far is saved",
            "To continue later",
        ],
    );
    // The resume command names the copy that can be continued.
    let s = render(&app, 160, 40);
    assert!(
        s.contains("sudo diskrescue image") || s.contains("diskrescue image"),
        "{s}"
    );
}
