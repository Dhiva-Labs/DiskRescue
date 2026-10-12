//! Repairing and checking loose files, and repairing a damaged result with
//! a reference file chosen from the results.

mod common;

use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;
use diskrescue_recover::report::Outcome;
use diskrescue_tui::app::Screen;
use diskrescue_tui::screens::help::ScreenId;
use diskrescue_tui::screens::results::Filter;
use diskrescue_tui::testing::*;
use diskrescue_tui::App;

fn contains_all(screen: &str, needles: &[&str]) {
    let flat = common::flatten(screen);
    for n in needles {
        assert!(
            screen.contains(n) || flat.contains(&common::flatten(n)),
            "missing {n:?} in:\n{screen}"
        );
    }
}

/// The damaged files of the scenario plus one healthy video, in one folder.
fn inputs(dir: &Path) -> Option<PathBuf> {
    let sc = common::damaged_media()?;
    let folder = dir.join("loose");
    std::fs::create_dir_all(&folder).unwrap();
    for name in [
        "truncated.jpg",
        "zeroed_header.jpg",
        "bad_crc.png",
        "truncated.png",
        "no_moov.mp4",
        "truncated.mp4",
        "no_idx1.avi",
    ] {
        std::fs::copy(sc.root.join(name), folder.join(name)).unwrap();
    }
    std::fs::copy(sc.root.join("donors/donor.mp4"), folder.join("good.mp4")).unwrap();
    Some(folder)
}

/// "Repair or check files": choose every photo and video in `folder`.
fn choose_files(app: &mut App, folder: &Path) {
    app.handle_event(ch('f'));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    app.handle_event(ch('/'));
    common::replace_text(app, &folder.display().to_string());
    press(app, KeyCode::Enter);
    // Tick every photo and video of the folder, then "Done".
    app.handle_event(ch('a'));
    contains_all(
        &render(app, 100, 30),
        &[
            "[x] no_moov.mp4",
            "[x] good.mp4",
            "Done: use the 8 chosen file(s)",
        ],
    );
    press(app, KeyCode::Enter);
    assert!(matches!(
        app.overlay(),
        Some(diskrescue_tui::app::Overlay::Choice { .. })
    ));
}

fn results_screen(app: &App) -> &diskrescue_tui::screens::results::ResultsScreen {
    match app.screen() {
        Screen::Results(r) => r,
        _ => panic!("not on the results:\n{}", render(app, 100, 30)),
    }
}

fn outcome_of(app: &App, name: &str) -> Outcome {
    let data = &results_screen(app).data;
    let i = data
        .sources
        .iter()
        .position(|p| {
            p.as_ref()
                .is_some_and(|p| p.file_name().is_some_and(|n| n == name))
        })
        .unwrap_or_else(|| panic!("{name} is not in the results"));
    data.items[i].outcome
}

#[test]
fn repairing_loose_files_then_repairing_one_with_a_reference_file() {
    let home = common::scratch("tui-repair-");
    let Some(folder) = inputs(home.path()) else {
        return;
    };
    let fixed = home.path().join("fixed");
    let mut app = common::app_in(home.path());

    choose_files(&mut app, &folder);
    contains_all(
        &render(&app, 100, 30),
        &[
            "8 files chosen. What now?",
            "Repair them (recommended)",
            "Only check whether they are intact",
        ],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Files: 8 files chosen",
            "Folder for the repaired copies",
            "Reference files (donors)",
            "Allow repairs that cut damaged parts out?",
            "no, normal repair only",
        ],
    );
    first_field(&mut app);
    common::replace_text(&mut app, &fixed.display().to_string());
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(
        &render(&app, 100, 30),
        &[
            "Repair of 8 files: ready to start",
            "Try to repair 8 files.",
            "The original files are never changed.",
            "Files to repair: 8 files",
        ],
    );
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Finished",
            "Done:",
            "repaired",
            "Check the files",
            "Repair damaged files",
        ],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Results);
    let s = render(&app, 100, 30);
    contains_all(&s, &["Results in", "Damaged (", "Repaired ("]);

    // The healthy video needs nothing, and serves as a reference for the
    // other video. The photo with the destroyed header needs a healthy
    // photo of the same camera, and there is none among the inputs.
    assert!(matches!(
        outcome_of(&app, "good.mp4"),
        Outcome::Recovered | Outcome::RecoveredUnverified
    ));
    assert_eq!(outcome_of(&app, "zeroed_header.jpg"), Outcome::Damaged);
    assert!(matches!(
        outcome_of(&app, "no_moov.mp4"),
        Outcome::Repaired | Outcome::PartiallyRepaired
    ));
    // The inputs were not touched.
    assert_eq!(
        std::fs::read(folder.join("zeroed_header.jpg")).unwrap(),
        std::fs::read(
            common::damaged_media()
                .unwrap()
                .root
                .join("zeroed_header.jpg")
        )
        .unwrap()
    );

    // Select the photo in the Damaged list and ask for a reference file.
    app.handle_event(ch('2'));
    assert_eq!(results_screen(&app).filter, Filter::Damaged);
    let idx = results_screen(&app)
        .view
        .iter()
        .position(|&i| {
            results_screen(&app).data.sources[i]
                .as_ref()
                .is_some_and(|p| p.ends_with("zeroed_header.jpg"))
        })
        .expect("zeroed_header.jpg is listed as damaged");
    for _ in 0..idx {
        press(&mut app, KeyCode::Down);
    }
    contains_all(
        &render(&app, 100, 30),
        &[
            "zeroed_header.jpg",
            "DAMAGED",
            "Press r to repair it with a healthy reference file",
        ],
    );
    // A file that is fine cannot be repaired with a reference.
    app.handle_event(ch('1'));
    let ok_idx = results_screen(&app)
        .view
        .iter()
        .position(|&i| results_screen(&app).data.items[i].outcome == Outcome::Recovered)
        .unwrap();
    app.handle_event(ch('2'));
    app.handle_event(ch('1'));
    for _ in 0..ok_idx {
        press(&mut app, KeyCode::Down);
    }
    app.handle_event(ch('r'));
    assert!(app
        .status()
        .is_some_and(|s| s.contains("Only damaged files")));
    app.handle_event(ch('2'));
    for _ in 0..idx {
        press(&mut app, KeyCode::Down);
    }
    app.handle_event(ch('r'));
    assert_eq!(app.screen_id(), ScreenId::Donor);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Repair with a reference file",
            "Damaged file:",
            "zeroed_header.jpg",
            "good.mp4",
            "different format (mp4)",
            "Browse for another file...",
        ],
    );
    // Browse for a photo from the same camera and choose it.
    app.handle_event(ch('b'));
    assert_eq!(app.screen_id(), ScreenId::Pick);
    app.handle_event(ch('/'));
    let donor = common::damaged_media()
        .unwrap()
        .root
        .join("donors/donor.jpg");
    common::replace_text(&mut app, &donor.display().to_string());
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Preflight);
    common::wait_for_check(&mut app);
    contains_all(
        &render(&app, 100, 30),
        &["Repair of 1 file", "ready to start", "donor.jpg"],
    );
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Results);
    assert!(
        matches!(
            outcome_of(&app, "zeroed_header.jpg"),
            Outcome::Repaired | Outcome::PartiallyRepaired
        ),
        "{}",
        render(&app, 100, 30)
    );
    let repaired = fixed.join("manual-repair/zeroed_header-1/repaired/zeroed_header.jpg");
    assert!(repaired.is_file(), "{}", repaired.display());
    contains_all(&render(&app, 100, 30), &["manual-repair", "repaired"]);
    // Esc returns to the first results, which are untouched.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Results);
    assert_eq!(outcome_of(&app, "zeroed_header.jpg"), Outcome::Damaged);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Home);
}

fn first_field(app: &mut App) {
    for _ in 0..3 {
        press(app, KeyCode::PageUp);
    }
}

#[test]
fn a_reference_file_can_be_added_in_the_form_and_removed_again() {
    let home = common::scratch("tui-donor-");
    let Some(folder) = inputs(home.path()) else {
        return;
    };
    let fixed = home.path().join("fixed2");
    let mut app = common::app_in(home.path());
    app.handle_event(ch('f'));
    app.handle_event(ch('/'));
    common::replace_text(
        &mut app,
        &folder.join("zeroed_header.jpg").display().to_string(),
    );
    // A path that is a file is chosen at once.
    press(&mut app, KeyCode::Enter);
    contains_all(&render(&app, 100, 30), &["1 file chosen. What now?"]);
    press(&mut app, KeyCode::Enter);
    first_field(&mut app);
    common::replace_text(&mut app, &fixed.display().to_string());
    press(&mut app, KeyCode::Down);
    contains_all(
        &render(&app, 100, 30),
        &["Reference files (donors)", "(none)"],
    );
    // Enter opens the browser; type the path of the healthy video.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Pick);
    app.handle_event(ch('/'));
    let donor = common::damaged_media()
        .unwrap()
        .root
        .join("donors/donor.jpg");
    common::replace_text(&mut app, &donor.display().to_string());
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Form);
    let s = render(&app, 100, 30);
    contains_all(&s, &["donor.jpg", "d removes the highlighted one"]);
    assert!(!s.contains("(none)"), "{s}");
    // d removes it; add it again.
    app.handle_event(ch('d'));
    contains_all(&render(&app, 100, 30), &["(none)"]);
    press(&mut app, KeyCode::Enter);
    app.handle_event(ch('/'));
    common::replace_text(&mut app, &donor.display().to_string());
    press(&mut app, KeyCode::Enter);
    app.handle_event(ctrl('s'));
    common::wait_for_check(&mut app);
    contains_all(
        &render(&app, 100, 30),
        &["Healthy reference files:", "donor.jpg"],
    );
    press(&mut app, KeyCode::Enter);
    common::wait_for_job(&mut app);
    press(&mut app, KeyCode::Enter);
    assert!(matches!(
        outcome_of(&app, "zeroed_header.jpg"),
        Outcome::Repaired | Outcome::PartiallyRepaired
    ));
    contains_all(&render(&app, 100, 30), &["repaired", "Repaired (1)"]);
}

#[test]
fn checking_files_shows_a_verdict_for_each() {
    let home = common::scratch("tui-verify-");
    let Some(folder) = inputs(home.path()) else {
        return;
    };
    let mut app = common::app_in(home.path());
    choose_files(&mut app, &folder);
    // "Only check whether they are intact".
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let ok = tick_until(&mut app, std::time::Duration::from_secs(60), |a| {
        a.screen_id() == ScreenId::Run
    });
    assert!(ok, "{}", render(&app, 100, 30));
    common::wait_for_job(&mut app);
    contains_all(
        &render(&app, 100, 30),
        &["Finished", "Checked 8 files:", "need attention"],
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen_id(), ScreenId::Verify);
    let s = render(&app, 100, 30);
    contains_all(
        &s,
        &[
            "Check of 8 files",
            "need attention. Press r to try to repair them.",
            "DAMAGED",
            "truncated.jpg",
            "good.mp4",
            "ok",
        ],
    );
    // r offers to repair the files that need attention.
    app.handle_event(ch('r'));
    assert_eq!(app.screen_id(), ScreenId::Form);
    let s = render(&app, 100, 30);
    contains_all(&s, &["Folder for the repaired copies"]);
    assert!(
        !s.contains("8 files chosen"),
        "only the files that need attention are offered:\n{s}"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen_id(), ScreenId::Verify);
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.screen_id(), ScreenId::Home);
}
