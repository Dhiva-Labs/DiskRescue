//! Helpers for the integration tests. Sources are always regular files; no
//! test opens a block device.

#![allow(dead_code)]

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crossterm::event::KeyCode;
use diskrescue_fixtures::sets::{self, Scenario};
use diskrescue_tui::config::Config;
use diskrescue_tui::testing::{ctrl, press, tick_until, type_text};
use diskrescue_tui::{App, Event};

/// Disk-backed scratch space (images are preallocated at full size).
pub fn scratch(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

fn cached(
    name: &str,
    ok: impl Fn(&Path, &Scenario) -> bool,
    build: fn(&Path) -> diskrescue_fixtures::Result<Scenario>,
) -> Option<Scenario> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixtures")
        .join(name);
    if let Ok(sc) = Scenario::load(&dir) {
        if ok(&dir, &sc) {
            return Some(sc);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    match build(&dir) {
        Ok(sc) => Some(sc),
        Err(e) if e.is_missing() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("cannot build the {name} fixture: {e}"),
    }
}

/// The camera-card scenario (shared with the other crates' test caches).
pub fn camera_card() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        cached(
            "camera-sd-card",
            |_, sc| sc.disk_image.as_ref().is_some_and(|p| p.exists()),
            sets::camera_sd_card,
        )
    })
    .as_ref()
}

/// Loose damaged files (shared with the other crates' test caches).
pub fn damaged_media() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        cached(
            "damaged-media",
            |dir, _| dir.join("no_moov.mp4").exists(),
            sets::damaged_media,
        )
    })
    .as_ref()
}

/// An app with no devices, home and working folder in `home`.
pub fn app_in(home: &Path) -> App {
    let mut cfg = Config::for_tests(home, vec![]);
    cfg.cwd = home.to_path_buf();
    let mut app = App::new(cfg);
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    app
}

/// Type `text` into the focused text field after clearing it.
pub fn replace_text(app: &mut App, text: &str) {
    app.handle_event(ctrl('u'));
    app.handle_event(ctrl('k'));
    type_text(app, text);
}

/// Open an image file with the file browser: `i`, `/`, the path, Enter.
pub fn open_image(app: &mut App, image: &Path) {
    press(app, KeyCode::Char('i'));
    press(app, KeyCode::Char('/'));
    replace_text(app, &image.display().to_string());
    press(app, KeyCode::Enter);
}

/// Wait (ticking) until the check screen has an answer.
pub fn wait_for_check(app: &mut App) {
    let ok = tick_until(app, Duration::from_secs(60), |a| {
        matches!(a.screen(), diskrescue_tui::app::Screen::Preflight(p) if p.result().is_some())
            || !matches!(a.screen(), diskrescue_tui::app::Screen::Preflight(_))
    });
    assert!(ok, "the safety check never answered");
}

/// Wait (ticking) until the job has ended.
pub fn wait_for_job(app: &mut App) {
    let ok = tick_until(app, Duration::from_secs(300), |a| {
        a.run().is_some_and(|r| !r.is_running())
    });
    assert!(ok, "the job never ended");
}

/// A screen as one line of text: box borders removed, line breaks and runs
/// of spaces turned into single spaces, so that a sentence that was wrapped
/// by the layout can still be found.
pub fn flatten(screen: &str) -> String {
    let no_boxes: String = screen
        .chars()
        .map(|c| {
            if "│┌┐└┘─".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    no_boxes.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An app with one "device" that is a big, mostly empty regular file, and
/// a rescue of it under way with tiny reads, so that it stays running.
pub fn slow_rescue(home: &Path) -> (App, std::path::PathBuf) {
    use diskrescue_tui::config::FixedDevice;
    use diskrescue_tui::testing::{device, render};
    let src = home.join("stick.bin");
    {
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(256 << 20).unwrap();
    }
    let out = home.join("rescue");
    let mut info = device(
        &src.display().to_string(),
        diskrescue_core::device::DeviceKind::UsbFlash,
        256 << 20,
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
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    for _ in 0..3 {
        press(&mut app, KeyCode::PageUp);
    }
    replace_text(&mut app, &out.display().to_string());
    // More options, then the size of one read (the last field).
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    for _ in 0..6 {
        press(&mut app, KeyCode::Down);
    }
    type_text(&mut app, "512");
    app.handle_event(ctrl('s'));
    wait_for_check(&mut app);
    press(&mut app, KeyCode::Enter);
    let started = tick_until(&mut app, Duration::from_secs(60), |a| {
        a.run().is_some_and(|r| {
            matches!(&r.snap, diskrescue_tui::screens::run::Snap::Recover(s)
                if s.image.as_ref().is_some_and(|i| i.progress.is_some()))
        })
    });
    assert!(started, "{}", render(&app, 100, 30));
    (app, out)
}
