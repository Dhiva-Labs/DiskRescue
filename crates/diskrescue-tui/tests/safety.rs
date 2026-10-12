//! The check before anything starts: problems in red, cautions in yellow,
//! and the confirmation that an overridable problem needs.

mod common;

use std::path::Path;

use crossterm::event::KeyCode;
use diskrescue_tui::app::Screen;
use diskrescue_tui::config::{Config, FixedDevice};
use diskrescue_tui::screens::help::ScreenId;
use diskrescue_tui::testing::*;
use diskrescue_tui::{App, Event};
use ratatui::style::Color;

fn contains_all(screen: &str, needles: &[&str]) {
    let flat = common::flatten(screen);
    for n in needles {
        assert!(
            screen.contains(n) || flat.contains(&common::flatten(n)),
            "missing {n:?} in:\n{screen}"
        );
    }
}

fn app_with_file_device(home: &Path, source: &Path) -> App {
    let size = std::fs::metadata(source).unwrap().len();
    let mut info = device(
        &source.display().to_string(),
        diskrescue_core::device::DeviceKind::UsbFlash,
        size,
    );
    info.model = Some("Test stick".into());
    let mut app = App::new(Config::for_tests(
        home,
        vec![FixedDevice {
            info,
            needs_root: false,
        }],
    ));
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    app
}

fn first_field(app: &mut App) {
    for _ in 0..3 {
        press(app, KeyCode::PageUp);
    }
}

/// A file that looks like a small device.
fn small_source(dir: &Path) -> std::path::PathBuf {
    let src = dir.join("stick.bin");
    let mut data = vec![0u8; 8 << 20];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    std::fs::write(&src, data).unwrap();
    src
}

/// Count the cells whose text starts at `needle` and have this colour.
fn colour_of(app: &App, needle: &str) -> Option<Color> {
    let buf = render_buffer(app, 100, 30);
    let w = buf.area.width as usize;
    let text: Vec<String> = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    for y in 0..buf.area.height as usize {
        let line: String = text[y * w..(y + 1) * w].concat();
        if let Some(pos) = line.find(needle) {
            // Character index, not byte index.
            let col = line[..pos].chars().count();
            return Some(buf.content()[y * w + col].fg);
        }
    }
    None
}

#[test]
fn an_output_that_is_the_source_is_refused_with_advice() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-block-");
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    // The output folder is the image itself.
    common::replace_text(&mut app, &img.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "cannot start as it is",
            "PROBLEM",
            "is not a folder",
            "What to do:",
            "Choose a new folder for the results",
        ],
    );
    assert!(!s.contains("To continue anyway"), "{s}");
    // The problem is red.
    assert_eq!(colour_of(&app, "x PROBLEM"), Some(Color::Red));
    // Enter does nothing; Esc goes back to the form with its values.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Preflight);
    assert!(app.run().is_none());
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Form);
    contains_all(&render(&app, 100, 30), &["sd_card.img"]);
}

#[test]
fn a_cautionary_warning_is_yellow_and_does_not_stop_anything() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-warn-");
    let out = home.path().join("again");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("report.json"), b"{}").unwrap();
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "ready to start",
            "Be careful",
            "already contains the report of an earlier run",
            "report.previous.txt",
        ],
    );
    assert_eq!(colour_of(&app, "! Be careful"), Some(Color::Yellow));
    // It is only a caution: Enter starts.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run);
    common::wait_for_job(&mut app);
}

#[test]
fn an_overridable_problem_needs_the_word_yes() {
    let home = common::scratch("tui-override-");
    let src = small_source(home.path());
    let out = home.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    // An image with this name exists, but there is no progress map for it.
    let existing = out.join("stick.bin.img");
    std::fs::write(&existing, b"something else entirely").unwrap();
    let mut app = app_with_file_device(home.path(), &src);

    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "needs your decision",
            "PROBLEM (you can continue anyway)",
            "already exists, but there is no progress map",
            "Choose a new image name.",
            "To continue anyway, type yes and press Enter",
            "go back instead (recommended",
        ],
    );
    assert!(!s.contains("--force"), "command-line flags leaked:\n{s}");
    assert_eq!(colour_of(&app, "x PROBLEM"), Some(Color::Red));
    // Enter alone, or anything but yes, changes nothing.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Preflight);
    contains_all(&render(&app, 100, 30), &["Type the word yes"]);
    type_text(&mut app, "no");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Preflight);
    assert!(app.run().is_none());
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        b"something else entirely"
    );
    // Letters are typed, not commands (q would quit elsewhere).
    app.handle_event(ctrl('u'));
    type_text(&mut app, "YES");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run);

    // The copy replaces the file, then the recovery follows from the image.
    let run = app.run().unwrap();
    assert_eq!(run.step.1, 2, "an override runs as copy, then recovery");
    common::wait_for_job(&mut app);
    let run = app.run().unwrap();
    assert_eq!(run.step, (2, 2));
    assert!(
        run.outcome.as_ref().unwrap().is_finished(),
        "{:?}",
        run.outcome
    );
    assert_eq!(
        std::fs::read(&existing).unwrap(),
        std::fs::read(&src).unwrap()
    );
    assert!(out.join("report.json").is_file());
}

#[test]
fn a_zero_byte_device_is_never_offered() {
    let home = common::scratch("tui-zero-");
    let dead = home.path().join("dead.bin");
    std::fs::write(&dead, b"").unwrap();
    let mut info = device(
        &dead.display().to_string(),
        diskrescue_core::device::DeviceKind::UsbFlash,
        0,
    );
    info.model = Some("Dead stick".into());
    let mut app = App::new(Config::for_tests(
        home.path(),
        vec![FixedDevice {
            info,
            needs_root: false,
        }],
    ));
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    contains_all(&render(&app, 100, 30), &["[failing]", "0 bytes"]);
    press(&mut app, KeyCode::Enter);
    let s = render(&app, 100, 30);
    assert_eq!(s.matches("(not available)").count(), 4, "{s}");
    contains_all(
        &s,
        &["controller has most likely failed", "data recovery lab"],
    );
}

#[test]
fn a_device_that_needs_root_says_so_everywhere() {
    let home = common::scratch("tui-root-");
    let mut info = device(
        "/dev/fixture-sdc",
        diskrescue_core::device::DeviceKind::SdCard,
        15_931_539_456,
    );
    info.model = Some("SD card".into());
    let mut app = App::new(Config::for_tests(
        home.path(),
        vec![FixedDevice {
            info,
            needs_root: true,
        }],
    ));
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "[needs root]",
            "needs administrator rights",
            "sudo diskrescue-tui",
        ],
    );
    // Recover directly cannot run without them: the check explains.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    contains_all(
        &render(&app, 100, 30),
        &["Close DiskRescue and start it again with: sudo diskrescue-tui"],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &["cannot start as it is", "does not exist", "What to do:"],
    );
    assert!(matches!(app.screen(), Screen::Preflight(_)));
}
