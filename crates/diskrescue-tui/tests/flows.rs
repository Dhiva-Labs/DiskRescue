//! Whole journeys through the app, on files built by diskrescue-fixtures.
//! No test opens a block device.

mod common;

use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use diskrescue_core::util::hash::blake3_hex;
use diskrescue_fixtures::sets::Recoverability;
use diskrescue_tui::app::{Overlay, Screen};
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

fn results_of(app: &App) -> &diskrescue_tui::screens::results::ResultsScreen {
    match app.screen() {
        Screen::Results(r) => r,
        _ => panic!("not on the results screen:\n{}", render(app, 100, 30)),
    }
}

/// Go to the first field of a form.
fn first_field(app: &mut App) {
    for _ in 0..3 {
        press(app, KeyCode::PageUp);
    }
}

/// Wait for the check, which must pass, then start.
fn confirm_start(app: &mut App) {
    common::wait_for_check(app);
    let s = render(app, 100, 30);
    assert!(s.contains("ready to start"), "{s}");
    press(app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run, "{}", render(app, 100, 30));
}

/// A rescue app whose only "device" is the regular file `source`.
fn app_with_file_device(home: &Path, source: &Path, size: u64) -> App {
    let mut info = device(
        &source.display().to_string(),
        diskrescue_core::device::DeviceKind::UsbFlash,
        size,
    );
    info.model = Some("Test stick".into());
    let cfg = Config::for_tests(
        home,
        vec![FixedDevice {
            info,
            needs_root: false,
        }],
    );
    let mut app = App::new(cfg);
    app.resize(100, 30);
    app.handle_event(Event::Tick);
    app
}

fn bytes_of(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn recovering_the_camera_card_from_an_image_and_browsing_the_results() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-recover-");
    let out = home.path().join("recovered");
    let mut app = common::app_in(home.path());

    // Open an image file; the menu offers Recover and Scan.
    common::open_image(&mut app, &img);
    assert_eq!(app.screen_id(), ScreenId::Menu);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Recover files from this image",
            "Preview what can be recovered (scan)",
        ],
    );
    assert!(
        !s.contains("Rescue (copy"),
        "an image cannot be rescued:\n{s}"
    );

    // Fill the form: the folder for the results, then Start.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    assert_eq!(app.screen_id(), ScreenId::Preflight);
    common::wait_for_check(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "ready to start",
            "What will happen",
            "Recover files from the image",
            "Looking for: photos, RAW photos, videos (existing and deleted files).",
            "is only read, never changed",
            "What was found",
        ],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run);

    // It runs in the background; the screen shows the stages.
    common::wait_for_job(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Finished",
            "Done:",
            "recovered",
            "Press Enter to browse the results",
            "Read the partition table",
            "Search the raw data for lost files",
            "Write the report",
            "Volume 1: fat32",
            "EOS_DIGITAL",
            "Latest files",
        ],
    );
    assert!(out.join("report.json").is_file());

    // The results browser.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Results);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Results in",
            "All (",
            "Damaged (0)",
            "Repaired (0)",
            "Deleted (2)",
            "Photos (",
            "Videos (",
            "Details",
            "Saved as",
        ],
    );
    // Every file of the card came back, byte for byte, the deleted ones too.
    let data = results_of(&app).data.clone();
    for t in &card.files {
        let name = Path::new(&t.path).file_name().unwrap();
        let item = data
            .items
            .iter()
            .find(|i| {
                i.path.as_ref().and_then(|p| p.file_name()) == Some(name)
                    && !matches!(i.outcome, diskrescue_recover::report::Outcome::Duplicate)
            })
            .unwrap_or_else(|| panic!("{} was not recovered", t.path));
        let path = item.path.as_ref().unwrap();
        assert_eq!(
            blake3_hex(&bytes_of(path)),
            t.blake3,
            "{} differs from the original",
            t.path
        );
        assert_eq!(
            item.deleted,
            t.expect == Recoverability::DeletedIntact,
            "{}",
            t.path
        );
        if item.deleted {
            assert!(path.starts_with(out.join("deleted")), "{}", path.display());
        }
    }
    // The Deleted filter lists exactly the two deleted files.
    app.handle_event(ch('4'));
    let s = render(&app, 100, 30);
    contains_all(&s, &["IMG_0004.JPG", "MVI_0009.MP4", "del"]);
    assert!(!s.contains("IMG_0001.JPG"), "{s}");
    assert_eq!(results_of(&app).view.len(), 2);
    // Filters without matches say so; the others narrow the list.
    app.handle_event(ch('2'));
    contains_all(&render(&app, 100, 30), &["No files match this filter"]);
    app.handle_event(ch('6'));
    let s = render(&app, 100, 30);
    contains_all(&s, &["MVI_0003.MP4", "video"]);
    assert!(!s.contains("IMG_0001.JPG"));
    app.handle_event(ch('5'));
    contains_all(&render(&app, 100, 30), &["IMG_0001.JPG", "CRW_0006.CR2"]);
    // Tab walks through the filters, back to All.
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Tab);
    assert_eq!(
        results_of(&app).filter,
        diskrescue_tui::screens::results::Filter::All
    );
    // Sorting by size, then reversed.
    let first_name = |app: &App| {
        let r = results_of(app);
        r.selected().map(|(_, it)| it.size).unwrap()
    };
    app.handle_event(ch('s'));
    contains_all(&render(&app, 100, 30), &["sorted by size"]);
    let biggest = first_name(&app);
    assert!(results_of(&app)
        .view
        .iter()
        .all(|&i| data.items[i].size <= biggest));
    app.handle_event(ch('S'));
    contains_all(&render(&app, 100, 30), &["(reversed)"]);
    // The detail pane describes the selected file.
    press(&mut app, KeyCode::Home);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Result",
            "Found at",
            "Type",
            "Taken",
            "Problems",
            "none found",
        ],
    );
    app.handle_event(ch('s'));
    contains_all(&render(&app, 100, 30), &["sorted by date"]);
    // "Open folder" shows the path when there is no opener.
    app.handle_event(ch('o'));
    assert!(app.last_folder().is_some_and(|p| p.starts_with(&out)));
    assert!(matches!(app.overlay(), Some(Overlay::Message { .. })));
    contains_all(
        &render(&app, 100, 30),
        &[
            "The files are in this folder",
            "yourself with your file manager",
        ],
    );
    press(&mut app, KeyCode::Esc);
    // Repairing needs a damaged file.
    app.handle_event(ch('r'));
    assert!(app
        .status()
        .is_some_and(|s| s.contains("Only damaged files")));
    // Esc leaves for the home screen.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
}

#[test]
fn earlier_results_can_be_opened_again() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-reopen-");
    let out = home.path().join("again");
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    confirm_start(&mut app);
    common::wait_for_job(&mut app);
    assert!(app.run().unwrap().outcome.as_ref().unwrap().is_finished());
    let count = {
        press(&mut app, KeyCode::Enter);
        results_of(&app).data.items.len()
    };

    // A new session: "Open previous results".
    let mut app = common::app_in(home.path());
    app.handle_event(ch('p'));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    // A folder without a report is explained.
    app.handle_event(ch('/'));
    common::replace_text(&mut app, &home.path().display().to_string());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter); // "use this folder"
    let ok = tick_until(&mut app, std::time::Duration::from_secs(10), |a| {
        matches!(a.overlay(), Some(Overlay::Message { .. }))
    });
    assert!(ok);
    contains_all(
        &render(&app, 100, 30),
        &["These results cannot be opened", "holds no report.json"],
    );
    press(&mut app, KeyCode::Esc);
    // The right folder opens.
    app.handle_event(ch('/'));
    common::replace_text(&mut app, &out.display().to_string());
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    let ok = tick_until(&mut app, std::time::Duration::from_secs(10), |a| {
        a.screen_id() == ScreenId::Results
    });
    assert!(ok, "{}", render(&app, 100, 30));
    assert_eq!(results_of(&app).data.items.len(), count);
    contains_all(&render(&app, 100, 30), &["Deleted (2)", "IMG_0004.JPG"]);
}

#[test]
fn a_preview_lists_what_could_be_recovered() {
    let Some(card) = common::camera_card() else {
        return;
    };
    let img = card.disk_image.clone().unwrap();
    let home = common::scratch("tui-scan-");
    let mut app = common::app_in(home.path());
    common::open_image(&mut app, &img);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let s = render(&app, 100, 30);
    assert!(!s.contains("saved?"));
    app.handle_event(ctrl('s'));
    // Nothing is written, so a clean check starts the preview by itself.
    let ok = tick_until(&mut app, std::time::Duration::from_secs(60), |a| {
        a.screen_id() == ScreenId::Run
    });
    assert!(ok, "{}", render(&app, 100, 30));
    common::wait_for_job(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &["Preview complete", "11 files found through the filesystem"],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Scan);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Preview:",
            "nothing was written",
            "Through the filesystem (11)",
            "Found in the raw data (0)",
            "deleted, guessed",
            "vol1:/DCIM/100CANON/IMG_0001.JPG",
            "vol1:/DCIM/100CANON/_MG_0004.JPG",
            "Volume 1: fat32",
            "2 deleted",
        ],
    );
    // The second list says the raw data was not searched.
    press(&mut app, KeyCode::Tab);
    contains_all(&render(&app, 100, 30), &["was not searched"]);
    // Recovering from the preview opens the form for the same image.
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(app.screen_id(), ScreenId::Form);
    contains_all(
        &render(&app, 100, 30),
        &["Where should the recovered files be saved?"],
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Scan);
}

#[test]
fn a_rescue_shows_the_dashboard_pauses_stops_and_resumes() {
    let home = common::scratch("tui-rescue-");
    // A big, mostly empty file: copying it in tiny reads takes a while.
    let src = home.path().join("stick.bin");
    {
        let f = std::fs::File::create(&src).unwrap();
        f.set_len(256 << 20).unwrap();
    }
    let out = home.path().join("rescue");
    let mut app = app_with_file_device(home.path(), &src, 256 << 20);
    contains_all(&render(&app, 100, 30), &["Test stick"]);

    // Rescue (the first menu entry), with small reads so that it stays slow.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    // More options: Block size is the last of the extra fields.
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Down); // More options
    press(&mut app, KeyCode::Enter); // open
    for _ in 0..6 {
        press(&mut app, KeyCode::Down);
    }
    type_text(&mut app, "512");
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(
        &render(&app, 100, 30),
        &[
            "Step 1: copy",
            "Step 2: recover files from the image",
            "Reading strategy: generic",
            "Room needed on the destination disk",
        ],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Run);

    // The imaging dashboard: map, progress and messages.
    let ok = tick_until(&mut app, std::time::Duration::from_secs(60), |a| {
        render(a, 120, 40).contains("Copied") && render(a, 120, 40).contains("Position")
    });
    assert!(ok, "{}", render(&app, 120, 40));
    let s = render(&app, 120, 40);
    contains_all(
        &s,
        &[
            "Map:",
            "Progress",
            "Messages",
            "Phase",
            "Speed",
            "Unreadable",
            "copied",
            "not read",
        ],
    );
    contains_all(&s, &["Step 1 of 2"]);
    assert!(s.contains('\u{2588}') || s.contains('\u{b7}'));

    // Pause and continue.
    app.handle_event(ch('p'));
    app.handle_event(Event::Tick);
    assert!(app.run().unwrap().is_paused());
    contains_all(&render(&app, 120, 40), &["PAUSED"]);
    contains_all(&render(&app, 120, 40), &["continue"]);
    app.handle_event(ch('p'));
    assert!(!app.run().unwrap().is_paused());

    // Stop and save asks first; "n" keeps going.
    app.handle_event(ch('s'));
    contains_all(
        &render(&app, 120, 40),
        &["Are you sure?", "Stop now and save what has been done?"],
    );
    app.handle_event(ch('n'));
    assert!(app.overlay().is_none() && app.job_running());
    app.handle_event(ch('s'));
    app.handle_event(ch('y'));
    let ok = tick_until(&mut app, std::time::Duration::from_secs(60), |a| {
        !a.job_running()
    });
    assert!(ok);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Stopped",
            "Everything done so far is saved",
            "To continue later",
            "diskrescue image",
        ],
    );
    assert!(
        out.join("stick.bin.img.map").is_file() || std::fs::read_dir(&out).unwrap().count() > 0
    );
    // Home shows where the job stands, and "g" goes back to it.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
    contains_all(&render(&app, 100, 30), &["stopped", "press g"]);
    app.handle_event(ch('g'));
    assert_eq!(app.screen_id(), ScreenId::Run);
    press(&mut app, KeyCode::Char('h'));

    // Start the same rescue again: the copy continues where it stopped.
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &out.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(&render(&app, 100, 30), &["Continuing an earlier copy"]);
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    let s = render(&app, 100, 30);
    contains_all(&s, &["Finished"]);
    assert!(
        diskrescue_tui_same(&src, &first_image(&out)),
        "the resumed image differs from the source"
    );
}

fn first_image(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "img"))
        .expect("an image in the output folder")
}

fn diskrescue_tui_same(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (Ok(fa), Ok(fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else {
        return false;
    };
    if fa.metadata().unwrap().len() != fb.metadata().unwrap().len() {
        return false;
    }
    let mut ra = std::io::BufReader::new(fa);
    let mut rb = std::io::BufReader::new(fb);
    let mut x = vec![0u8; 1 << 20];
    let mut y = vec![0u8; 1 << 20];
    loop {
        let n = ra.read(&mut x).unwrap();
        if n == 0 {
            return true;
        }
        rb.read_exact(&mut y[..n]).unwrap();
        if x[..n] != y[..n] {
            return false;
        }
    }
}
