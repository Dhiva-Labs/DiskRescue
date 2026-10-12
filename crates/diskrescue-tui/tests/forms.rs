//! Typing paths, completing them, browsing for folders, and a few screens
//! that depend on the size of the window or the mouse.

mod common;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use diskrescue_core::device::DeviceKind;
use diskrescue_tui::config::{Config, FixedDevice};
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

fn rescue_form(home: &std::path::Path) -> App {
    let mut info = device("/dev/fixture-sdb", DeviceKind::UsbFlash, 32_018_267_136);
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
    assert_eq!(app.screen_id(), ScreenId::Form);
    for _ in 0..3 {
        press(&mut app, KeyCode::PageUp);
    }
    app
}

#[test]
fn tab_completes_the_folder_being_typed_and_lists_the_matches() {
    let home = common::scratch("tui-tab-");
    std::fs::create_dir_all(home.path().join("disks/big")).unwrap();
    std::fs::create_dir_all(home.path().join("discs")).unwrap();
    std::fs::create_dir_all(home.path().join("other")).unwrap();
    let mut app = rescue_form(home.path());
    let base = home.path().display().to_string();
    common::replace_text(&mut app, &format!("{base}/ot"));
    press(&mut app, KeyCode::Tab);
    contains_all(&render(&app, 100, 30), &[&format!("{base}/other/")]);
    // Two folders start alike: the common part is added and both are shown.
    common::replace_text(&mut app, &format!("{base}/d"));
    press(&mut app, KeyCode::Tab);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[&format!("{base}/dis"), "Matches:", "disks/", "discs/"],
    );
    // One more letter, and Tab finishes the name.
    type_text(&mut app, "k");
    press(&mut app, KeyCode::Tab);
    contains_all(&render(&app, 100, 30), &[&format!("{base}/disks/")]);
    // The image file name follows the folder.
    contains_all(&render(&app, 100, 30), &["disks/fixture-sdb.img"]);
    // And "~" means the home folder.
    common::replace_text(&mut app, "~/ot");
    press(&mut app, KeyCode::Tab);
    contains_all(&render(&app, 100, 30), &["~/other/"]);
}

#[test]
fn the_folder_browser_fills_in_the_field() {
    let home = common::scratch("tui-browse-");
    std::fs::create_dir_all(home.path().join("disks/big")).unwrap();
    let mut app = rescue_form(home.path());
    // F2 opens the browser at the nearest folder that exists.
    press(&mut app, KeyCode::F(2));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Choose the folder for the results",
            "Use this folder:",
            "../  (up one folder)",
            "disks/",
        ],
    );
    // Into "disks", then "big", and choose it.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    contains_all(&render(&app, 100, 30), &["big/"]);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    let big = home.path().join("disks/big").display().to_string();
    let s = render(&app, 100, 30);
    contains_all(&s, &["disks/big", "fixture-sdb.img"]);
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(&render(&app, 100, 30), &["disks/big"]);
    let _ = big;
    // Cancelling the browser changes nothing.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::F(2));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Form);
}

#[test]
fn the_image_file_can_be_moved_to_another_folder_with_the_browser() {
    let home = common::scratch("tui-imgdir-");
    std::fs::create_dir_all(home.path().join("elsewhere")).unwrap();
    let mut app = rescue_form(home.path());
    press(&mut app, KeyCode::Down); // the image field
    press(&mut app, KeyCode::F(2));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    contains_all(
        &render(&app, 100, 30),
        &["Choose the folder for the image file"],
    );
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    let s = render(&app, 100, 30);
    contains_all(&s, &["elsewhere/fixture-sdb.img"]);
}

#[cfg(unix)]
#[test]
fn a_device_that_cannot_be_read_is_explained() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root can read anything
    }
    let home = common::scratch("tui-denied-");
    let src = home.path().join("locked.bin");
    std::fs::write(&src, vec![1u8; 4096]).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut info = device(&src.display().to_string(), DeviceKind::UsbFlash, 4096);
    info.model = Some("Locked".into());
    let mut app = App::new(Config::for_tests(
        home.path(),
        vec![FixedDevice {
            info,
            needs_root: true,
        }],
    ));
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    // Recover directly: the program has no rights to read it.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &home.path().join("out").display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "cannot start as it is",
            "PROBLEM",
            "Reading a raw device needs administrator rights",
            "sudo diskrescue-tui",
        ],
    );
    assert!(!s.contains("To continue anyway"));
    let _ = std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o600));
}

fn first_field(app: &mut App) {
    for _ in 0..3 {
        press(app, KeyCode::PageUp);
    }
}

#[test]
fn results_use_the_whole_window_and_react_to_the_mouse() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-mouse-");
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &home.path().join("out").display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Results);

    // A wide window puts the list and the details side by side.
    app.handle_event(Event::Resize(140, 40));
    let wide = render(&app, 140, 40);
    let line = wide.lines().find(|l| l.contains("Files")).unwrap();
    assert!(line.contains("Details"), "{wide}");
    // A narrow one stacks them.
    let narrow = render(&app, 100, 30);
    let line = narrow.lines().find(|l| l.contains("Files")).unwrap();
    assert!(!line.contains("Details"), "{narrow}");

    // Clicking a tab chooses its filter.
    let _ = render(&app, 100, 30);
    let click = |app: &mut App, col: u16, row: u16| {
        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    };
    // The tabs are on the third line of the body: " All (13)  Damaged ..."
    let tabs_row = narrow
        .lines()
        .position(|l| l.contains("Damaged (0)"))
        .unwrap() as u16;
    let col = narrow
        .lines()
        .nth(tabs_row as usize)
        .unwrap()
        .find("Deleted")
        .unwrap() as u16;
    click(&mut app, col + 1, tabs_row);
    let s = render(&app, 100, 30);
    contains_all(&s, &["IMG_0004.JPG", "MVI_0009.MP4"]);
    assert!(!s.contains("IMG_0001.JPG"), "{s}");
    // Clicking a row selects it; the detail pane follows.
    let _ = render(&app, 100, 30);
    let row = s.lines().position(|l| l.contains("MVI_0009.MP4")).unwrap() as u16;
    click(&mut app, 30, row);
    contains_all(&render(&app, 100, 30), &["MVI_0009.MP4", "video, mp4"]);
    // The wheel moves through the list.
    app.handle_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 30,
        row,
        modifiers: KeyModifiers::NONE,
    }));
    contains_all(&render(&app, 100, 30), &["IMG_0004.JPG"]);
}

#[test]
fn a_device_that_stops_answering_shows_a_banner_on_the_running_screen() {
    use diskrescue_app::jobs::ImageState;
    use diskrescue_app::plan::ImagePlan;
    use diskrescue_tui::model::{Overrides, Plan};
    use diskrescue_tui::screens::run::{Run, Snap};
    use diskrescue_tui::widgets::Theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let dir = common::scratch("tui-lost-");
    let src = dir.path().join("a.bin");
    std::fs::write(&src, vec![3u8; 1 << 20]).unwrap();
    let mut run = Run::start(
        Plan::Image(ImagePlan::new(&src, dir.path().join("a.img"))),
        Overrides::default(),
        None,
    )
    .unwrap();
    // The state a USB drive that dropped off the bus leaves behind.
    let state = ImageState {
        device_lost: true,
        device_lost_at: Some(4096),
        ..ImageState::default()
    };
    run.snap = Snap::Image(Box::new(state));
    let mut term = Terminal::new(TestBackend::new(110, 40)).unwrap();
    term.draw(|f| run.draw(f, f.area(), &Theme::new(true)))
        .unwrap();
    let buf = term.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..40 {
        for x in 0..110 {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    contains_all(
        &text,
        &[
            "The device stopped responding (at 4.00 KiB)",
            "Waiting for it to come back",
            "check the cable",
        ],
    );
}
