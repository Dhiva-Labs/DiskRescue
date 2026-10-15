//! Every screen rendered with fixture data (no terminal, no real device).

mod common;

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use diskrescue_tui::app::{Overlay, Screen};
use diskrescue_tui::config::Config;
use diskrescue_tui::screens::help::ScreenId;
use diskrescue_tui::testing::*;
use diskrescue_tui::widgets::Theme;
use diskrescue_tui::{App, Event};
use ratatui::style::Color;

fn app_with_devices(theme: Theme) -> (App, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests(dir.path(), vec![failing_usb_stick(), system_disk()]);
    cfg.theme = theme;
    let mut app = App::new(cfg);
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    (app, dir)
}

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
fn home_lists_devices_flags_and_the_other_entries() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Disks, cards and USB drives",
            "/dev/fixture-sdb",
            "29.8 GiB",
            "USB flash drive",
            "SanDisk Ultra",
            "[failing]",
            "/dev/fixture-nvme0n1",
            "[system disk]",
            "Open an image file",
            "Repair or check files",
            "Open previous results",
            "normal user",
        ],
    );
    // Details of the failing stick: advice in plain words, health, partitions.
    contains_all(
        &s,
        &[
            "About the selection",
            "Serial",
            "4C530001",
            "failing",
            "safest choice",
            "health check FAILED",
            "12 pending sectors",
            "Partitions and filesystems",
            "vfat",
        ],
    );
    // The system disk: the warning and its partitions.
    press(&mut app, KeyCode::Down);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "THIS IS THE SYSTEM DISK",
            "ext4",
            &format!("mounted at {}", system_root().display()),
        ],
    );
    // The other entries explain themselves.
    press(&mut app, KeyCode::Down);
    contains_all(&render(&app, 100, 30), &["A disk image is a file"]);
    press(&mut app, KeyCode::Down);
    contains_all(&render(&app, 100, 30), &["will not play"]);
    press(&mut app, KeyCode::Down);
    contains_all(&render(&app, 100, 30), &["report.json"]);
    // The list stops at the end.
    press(&mut app, KeyCode::Down);
    assert!(render(&app, 100, 30).contains("report.json"));
    // j and k move too; r refreshes.
    app.handle_event(ch('k'));
    contains_all(&render(&app, 100, 30), &["will not play"]);
    app.handle_event(ch('r'));
    assert!(app.status().is_some_and(|s| s.contains("again")));
}

#[test]
fn home_without_devices_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new(Config::for_tests(dir.path(), vec![]));
    app.resize(80, 24);
    app.handle_event(Event::Tick);
    let s = render(&app, 80, 24);
    contains_all(
        &s,
        &[
            "No disks, cards or USB drives found",
            "Open an image file",
            "Repair or check files",
        ],
    );
}

#[test]
fn the_home_screen_fits_in_the_smallest_window() {
    let (app, _d) = app_with_devices(Theme::new(true));
    let s = render(&app, 80, 24);
    contains_all(
        &s,
        &[
            "DiskRescue",
            "/dev/fixture-sdb",
            "Open an image file",
            "Enter  open",
            "q  quit",
        ],
    );
    assert_eq!(s.lines().count(), 24);
}

#[test]
fn the_action_menu_has_the_four_actions() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Menu);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Rescue (copy, then recover and repair) - recommended for failing drives",
            "Copy to an image only",
            "Recover directly (healthy drives, accidental deletion)",
            "Preview what can be recovered (scan)",
            "What do you want to do?",
            "stressed as",
        ],
    );
    press(&mut app, KeyCode::Down);
    contains_all(&render(&app, 100, 30), &["Makes the image and stops"]);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
}

#[test]
fn the_system_disk_cannot_be_rescued() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Menu);
    let s = render(&app, 100, 30);
    assert_eq!(s.matches("(not available)").count(), 4, "{s}");
    contains_all(
        &s,
        &[
            "Why it is not available",
            "disk the computer is running from",
        ],
    );
    // Choosing an action explains instead of opening a form.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Menu);
    assert!(matches!(app.overlay(), Some(Overlay::Message { .. })));
    contains_all(&render(&app, 100, 30), &["shut the computer down"]);
    press(&mut app, KeyCode::Esc);
    assert!(app.overlay().is_none());
    // The number keys do the same.
    app.handle_event(ch('2'));
    assert!(matches!(app.overlay(), Some(Overlay::Message { .. })));
}

#[test]
fn the_rescue_form_has_good_defaults_and_all_its_fields() {
    let (mut app, d) = app_with_devices(Theme::new(true));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Where should the recovered files be saved?",
            &format!("DiskRescue{}fixture-sdb", std::path::MAIN_SEPARATOR),
            "Image file (the copy of the device)",
            "fixture-sdb.img",
            "What do you want back?",
            "[x] Photos",
            "[x] RAW photos",
            "[x] Videos",
            "[ ] Audio",
            "[ ] Every file",
            "Which files?",
            "Existing and deleted",
            "Repair damaged files?",
            "yes, repair what can be repaired",
            "More options",
            "Start",
            "press Enter to check and start",
        ],
    );
    assert!(s.contains(&d.path().display().to_string()) || s.contains("DiskRescue/fixture-sdb"));
    // The extra options.
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    let s = render(&app, 100, 40);
    contains_all(
        &s,
        &[
            "Search the raw data for lost files?",
            "(*) Automatic",
            "( ) Free space only",
            "( ) Everything",
            "( ) Off",
        ],
    );
    press(&mut app, KeyCode::Down);
    contains_all(
        &render(&app, 100, 40),
        &["Reference files (donors)", "Enter adds a file"],
    );
    press(&mut app, KeyCode::Down);
    let s = render(&app, 100, 40);
    contains_all(
        &s,
        &[
            "How to read the device (profile)",
            "(*) Automatic (usb-flash)",
            "( ) hdd",
            "( ) sd",
            "small reads, 10 s timeouts",
            "reads of",
        ],
    );
    // Moving through the profile list shows each description.
    press(&mut app, KeyCode::Right);
    contains_all(
        &render(&app, 100, 40),
        &["(*) hdd", "64 KiB reads, exponential skipping"],
    );
    press(&mut app, KeyCode::Down);
    contains_all(
        &render(&app, 100, 40),
        &["Give up on one read after (seconds)", "empty = automatic"],
    );
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    contains_all(
        &render(&app, 100, 40),
        &["Size of one read", "for example 64k"],
    );
}

#[test]
fn a_wrong_number_is_pointed_out_on_the_form() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter); // open More options
                                     // Down to the timeout: carve, donors, profile, timeout.
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    type_text(&mut app, "soon");
    app.handle_event(ctrl('s'));
    assert_eq!(app.screen_id(), ScreenId::Form, "the form must not start");
    let s = render(&app, 100, 40);
    contains_all(&s, &["is not a number of seconds", "Give up on one read"]);
}

#[test]
fn the_other_forms() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    press(&mut app, KeyCode::Enter);
    // Copy to an image only.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let s = render(&app, 100, 30);
    contains_all(&s, &["Folder for the copy", "Image file", "More options"]);
    assert!(!s.contains("What do you want back?"), "{s}");
    press(&mut app, KeyCode::Esc);
    // Preview.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let s = render(&app, 100, 30);
    contains_all(&s, &["What do you want back?", "Which files?"]);
    assert!(!s.contains("saved?"), "a preview saves nothing:\n{s}");
}

#[test]
fn help_lists_the_keys_of_the_current_screen() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    app.handle_event(ch('?'));
    assert!(matches!(app.overlay(), Some(Overlay::Help)));
    contains_all(
        &render(&app, 100, 30),
        &[
            "Keys for the home screen",
            "open the selection",
            "look for devices again",
            "Everywhere",
            "Ctrl-C",
            "Nothing is ever written to the device",
        ],
    );
    // Any key closes it.
    press(&mut app, KeyCode::Char('x'));
    assert!(app.overlay().is_none());
    // Another screen, another list.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::F(1));
    contains_all(
        &render(&app, 100, 30),
        &[
            "Keys for a form",
            "complete a path you are typing",
            "browse for a folder or file",
            "check and start from anywhere",
        ],
    );
    press(&mut app, KeyCode::Esc);
    // A text field takes '?' as a letter.
    for _ in 0..5 {
        press(&mut app, KeyCode::Up);
    }
    app.handle_event(ch('?'));
    assert!(app.overlay().is_none());
    contains_all(&render(&app, 100, 30), &["?"]);
}

#[test]
fn a_window_that_is_too_small_gets_a_message() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    app.handle_event(Event::Resize(60, 20));
    assert!(app.too_small());
    let s = render(&app, 60, 20);
    contains_all(
        &s,
        &[
            "DiskRescue needs a bigger window.",
            "at least 80 columns by 24 rows",
            "This window is 60 by 20.",
        ],
    );
    assert!(!s.contains("/dev/fixture-sdb"));
    // Keys do nothing meanwhile, and the screen comes back when it grows.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Home);
    app.handle_event(Event::Resize(80, 24));
    assert!(!app.too_small());
    assert!(render(&app, 80, 24).contains("/dev/fixture-sdb"));
    // Exactly one column too narrow.
    app.handle_event(Event::Resize(79, 24));
    assert!(render(&app, 79, 24).contains("bigger window"));
}

#[test]
fn without_colour_nothing_depends_on_colour() {
    let (mut app, _d) = app_with_devices(Theme::new(false));
    let check = |app: &App| {
        let buf = render_buffer(app, 100, 30);
        for cell in buf.content() {
            assert!(
                matches!(cell.fg, Color::Reset) && matches!(cell.bg, Color::Reset),
                "coloured cell {:?}",
                cell
            );
        }
    };
    check(&app);
    // The selected row is still marked, by reverse video.
    let buf = render_buffer(&app, 100, 30);
    assert!(buf
        .content()
        .iter()
        .any(|c| c.modifier.contains(ratatui::style::Modifier::REVERSED)));
    // The flags are words, so they survive.
    contains_all(&render(&app, 100, 30), &["[failing]", "[system disk]"]);
    for _ in 0..2 {
        press(&mut app, KeyCode::Enter);
        check(&app);
    }
    press(&mut app, KeyCode::Up);
    check(&app);
    // And colours are really used when they are on.
    let (app, _d) = app_with_devices(Theme::new(true));
    let buf = render_buffer(&app, 100, 30);
    assert!(buf
        .content()
        .iter()
        .any(|c| !matches!(c.fg, Color::Reset) || !matches!(c.bg, Color::Reset)));
}

#[test]
fn the_mouse_selects_opens_and_scrolls() {
    let (mut app, _d) = app_with_devices(Theme::new(true));
    // Draw once so that the app knows where the rows are.
    let _ = render(&app, 100, 30);
    let click = |app: &mut App, col: u16, row: u16| {
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }));
    };
    // The second device starts on the third line of the list (below the
    // border and the first device's two lines).
    click(&mut app, 8, 4);
    let s = render(&app, 100, 30);
    contains_all(&s, &["THIS IS THE SYSTEM DISK"]);
    // Clicking the selected row again opens it.
    click(&mut app, 8, 4);
    assert_eq!(app.screen_id(), ScreenId::Menu);
    press(&mut app, KeyCode::Esc);
    // The wheel moves the selection.
    let _ = render(&app, 100, 30);
    app.handle_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 8,
        row: 6,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }));
    assert!(matches!(app.screen(), Screen::Home));
}
