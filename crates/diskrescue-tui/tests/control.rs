//! Stopping, quitting and what the keys mean while a job runs.

mod common;

use std::time::Duration;

use crossterm::event::KeyCode;
use diskrescue_tui::app::{Overlay, Screen};
use diskrescue_tui::screens::help::ScreenId;
use diskrescue_tui::testing::*;
use diskrescue_tui::Event;

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
fn ctrl_c_asks_the_job_to_stop_and_a_second_one_quits() {
    let home = common::scratch("tui-ctrlc-");
    let (mut app, _out) = common::slow_rescue(home.path());
    assert!(app.job_running());
    app.handle_event(Event::Interrupt);
    assert!(!app.should_quit(), "the first Ctrl-C only stops the job");
    contains_all(
        &render(&app, 100, 30),
        &["Stopping and saving", "Ctrl-C again to quit right now"],
    );
    // The job saves its progress and ends as stopped.
    let ok = tick_until(&mut app, Duration::from_secs(60), |a| !a.job_running());
    assert!(ok);
    assert!(!app.should_quit());
    contains_all(
        &render(&app, 100, 30),
        &["Stopped", "Everything done so far is saved"],
    );

    // A second request while the first is still being carried out quits.
    let home2 = common::scratch("tui-ctrlc2-");
    let (mut app, _out) = common::slow_rescue(home2.path());
    app.handle_event(Event::Interrupt);
    app.handle_event(Event::Interrupt);
    assert!(app.should_quit());
    assert_eq!(app.exit_code(), 130);
}

#[test]
fn ctrl_c_with_nothing_running_quits_at_once() {
    let home = common::scratch("tui-idle-");
    let mut app = common::app_in(home.path());
    app.handle_event(ctrl('c'));
    assert!(app.should_quit());
    assert_eq!(app.exit_code(), 0);
    // The key does the same as the signal, on any screen.
    let mut app = common::app_in(home.path());
    app.handle_event(ch('i'));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    app.handle_event(ctrl('c'));
    assert!(app.should_quit());
}

#[test]
fn quitting_while_a_job_runs_asks_and_waits_for_the_save() {
    let home = common::scratch("tui-quit-");
    let (mut app, out) = common::slow_rescue(home.path());
    // Leave the screen; the job keeps running and the home screen says so.
    app.handle_event(ch('b'));
    assert_eq!(app.screen_id(), ScreenId::Home);
    assert!(app.job_running());
    contains_all(&render(&app, 100, 30), &["Rescue of stick.bin", "press g"]);
    // q asks; n changes nothing.
    app.handle_event(ch('q'));
    assert!(matches!(app.overlay(), Some(Overlay::Confirm { .. })));
    contains_all(
        &render(&app, 100, 30),
        &[
            "A job is still running",
            "Stop it, save what it has done, and quit?",
        ],
    );
    app.handle_event(ch('n'));
    assert!(app.overlay().is_none() && !app.should_quit() && app.job_running());
    // Enter does not mean yes.
    app.handle_event(ch('q'));
    press(&mut app, KeyCode::Enter);
    assert!(!app.should_quit() && app.job_running());
    // Yes stops the job, and the app leaves when it has saved.
    app.handle_event(ch('q'));
    app.handle_event(ch('y'));
    assert!(app.status().is_some_and(|s| s.contains("closes by itself")));
    let ok = tick_until(&mut app, Duration::from_secs(60), |a| a.should_quit());
    assert!(ok);
    assert_eq!(app.exit_code(), 0);
    // What was done is on disk: the image and its progress map.
    assert!(std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".map")));
}

#[test]
fn quitting_from_deeper_screens_asks_first() {
    let home = common::scratch("tui-quit2-");
    let mut app = common::app_in(home.path());
    app.handle_event(ch('i'));
    app.handle_event(ch('q'));
    assert!(matches!(app.overlay(), Some(Overlay::Confirm { .. })));
    contains_all(&render(&app, 100, 30), &["Quit DiskRescue?"]);
    app.handle_event(ch('n'));
    assert!(!app.should_quit());
    // Typing a q into a path is not a command.
    app.handle_event(ch('/'));
    app.handle_event(ch('q'));
    assert!(app.overlay().is_none() && !app.should_quit());
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
    // Esc on the home screen does not quit; q does.
    press(&mut app, KeyCode::Esc);
    assert!(!app.should_quit());
    app.handle_event(ch('q'));
    assert!(app.should_quit());
}

#[test]
fn only_one_job_runs_at_a_time_and_esc_does_not_leave_a_running_one() {
    let home = common::scratch("tui-one-");
    let (mut app, _out) = common::slow_rescue(home.path());
    // Esc on a running job explains how to leave it.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Run);
    assert!(app
        .status()
        .is_some_and(|s| s.contains("Press s to stop and save")));
    app.handle_event(ch('b'));
    // Starting another is refused with an explanation.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    app.handle_event(ctrl('s'));
    assert_eq!(app.screen_id(), ScreenId::Form);
    assert!(matches!(app.overlay(), Some(Overlay::Message { .. })));
    contains_all(
        &render(&app, 100, 30),
        &["Another job is running", "Only one job can run at a time"],
    );
    press(&mut app, KeyCode::Esc);
    // g goes back to the job, s stops it with a confirmation.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
    app.handle_event(ch('g'));
    assert!(matches!(app.screen(), Screen::Run));
    app.handle_event(ch('s'));
    assert!(matches!(app.overlay(), Some(Overlay::Confirm { .. })));
    app.handle_event(ch('y'));
    let ok = tick_until(&mut app, Duration::from_secs(60), |a| !a.job_running());
    assert!(ok);
    // Finished jobs can be left with Esc (stopped is not an error) or h.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
}

#[test]
fn a_refused_job_returns_to_its_form_with_the_reason() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-refused-");
    let out = home.path().join("results");
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Enter);
    for _ in 0..3 {
        press(&mut app, KeyCode::PageUp);
    }
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(&render(&app, 100, 30), &["ready to start"]);
    // The job checks again right before it starts. Something changed in the
    // meantime: the folder for the results became a file.
    std::fs::write(&out, b"in the way").unwrap();
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Not started",
            "Nothing was started: it would not have been safe.",
            "exists and is not a folder",
            "Press Esc to go back and change the settings.",
        ],
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"in the way");
    // Esc goes back to the form, with everything still filled in.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Form);
    contains_all(
        &render(&app, 100, 30),
        &["Where should the recovered files be saved?"],
    );
}
