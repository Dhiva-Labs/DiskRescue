//! The recovery progress screen, drawn from states built by hand so that
//! every step of the checklist can be seen in the state it is in.

mod common;

use diskrescue_app::jobs::{Pipeline, RecoverState, RecoverStatus};
use diskrescue_app::plan::RecoverPlan;
use diskrescue_core::device::DeviceKind;
use diskrescue_core::fs::FsKind;
use diskrescue_recover::report::{Outcome, VolumeRecord};
use diskrescue_recover::{Event, Stage};
use diskrescue_tui::model::{Overrides, Plan};
use diskrescue_tui::screens::run::{Run, Snap};
use diskrescue_tui::testing::device;
use diskrescue_tui::widgets::Theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn contains_all(screen: &str, needles: &[&str]) {
    let flat = common::flatten(screen);
    for n in needles {
        assert!(
            screen.contains(n) || flat.contains(&common::flatten(n)),
            "missing {n:?} in:\n{screen}"
        );
    }
}

fn draw(run: &Run, w: u16, h: u16, theme: &Theme) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| run.draw(f, f.area(), theme)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A run whose state is `state` (the job behind it is a throwaway copy).
fn run_with(state: RecoverState) -> (Run, tempfile::TempDir) {
    let dir = common::scratch("tui-progress-");
    let src = dir.path().join("a.bin");
    std::fs::write(&src, vec![1u8; 1 << 16]).unwrap();
    let mut run = Run::start(
        Plan::Recover(RecoverPlan::new(&src, dir.path().join("out"))),
        Overrides::default(),
        false,
    )
    .unwrap();
    run.title = "Recovery from sd_card.img".into();
    run.snap = Snap::Recover(Box::new(state));
    (run, dir)
}

fn volume() -> VolumeRecord {
    VolumeRecord {
        index: 1,
        start: 0,
        size: 272 << 20,
        fs: Some(FsKind::Fat32),
        label: Some("EOS_DIGITAL".into()),
        origin: "superfloppy".into(),
        files_listed: 50,
        deleted_listed: 3,
        warnings: vec![],
        error: None,
    }
}

fn mid_run() -> RecoverState {
    let mut p = Pipeline::default();
    p.apply(Event::Stage(Stage::Partitions));
    p.apply(Event::Stage(Stage::Listing {
        volume: 1,
        fs: FsKind::Fat32,
    }));
    p.apply(Event::Volume(volume()));
    p.apply(Event::Stage(Stage::Extracting));
    p.apply(Event::Progress {
        done: 31,
        total: 50,
        bytes: 31 << 20,
    });
    for (name, o) in [
        ("files/vol1/DCIM/100CANON/IMG_0001.JPG", Outcome::Recovered),
        ("files/vol1/DCIM/100CANON/IMG_0002.JPG", Outcome::Recovered),
        ("files/vol1/DCIM/100CANON/IMG_0003.JPG", Outcome::Damaged),
        ("files/vol1/DCIM/100CANON/MVI_0003.MP4", Outcome::Repaired),
        (
            "deleted/vol1/DCIM/100CANON/IMG_0004.JPG",
            Outcome::Recovered,
        ),
        ("files/vol1/DCIM/100CANON/IMG_0005.JPG", Outcome::Duplicate),
        ("files/vol1/DCIM/100CANON/IMG_0006.JPG", Outcome::Recovered),
    ] {
        p.apply(Event::File {
            name: name.to_string(),
            outcome: o,
        });
    }
    RecoverState {
        status: RecoverStatus::Recovering,
        source: Some(device(
            "/home/me/card/sd_card.img",
            DeviceKind::ImageFile,
            272 << 20,
        )),
        output: "/home/me/DiskRescue/sd_card".into(),
        pipeline: p,
        ..RecoverState::default()
    }
}

#[test]
fn the_checklist_shows_what_is_done_what_runs_and_what_waits() {
    let (run, _d) = run_with(mid_run());
    let s = draw(&run, 100, 30, &Theme::new(true));
    println!("{s}");
    contains_all(
        &s,
        &[
            "Recovery from sd_card.img",
            "[x] Read the partition table",
            "[x] Read the file list of volume 1 (fat32)",
            "[>] Copy the files found by name",
            "[####",
            "31 of 50",
            "[ ] Search the raw data for lost files",
            "[ ] Check the files",
            "[ ] Repair damaged files",
            "[ ] Write the report",
            "found so far:  recovered 4   damaged 1   repaired 1   duplicates 1",
            "Volume 1: fat32 \"EOS_DIGITAL\"",
            "Latest files",
            "IMG_0006.JPG",
            "damaged",
            "MVI_0003.MP4",
            "duplicate",
        ],
    );
    // A step that was passed over without running is marked as not needed.
    let mut run = run;
    if let Snap::Recover(s) = &mut run.snap {
        s.pipeline.apply(Event::Stage(Stage::Validating));
    }
    let s = draw(&run, 100, 30, &Theme::new(true));
    contains_all(
        &s,
        &[
            "[x] Copy the files found by name",
            "[-] Search the raw data for lost files",
            "[>] Check the files",
            "[ ] Repair damaged files",
        ],
    );
}

#[test]
fn the_raw_data_search_shows_how_far_it_is() {
    let mut st = mid_run();
    st.pipeline.apply(Event::Stage(Stage::Carving));
    st.pipeline.apply(Event::Carve {
        scanned: 100 << 20,
        total: 272 << 20,
        files: 3,
    });
    let (run, _d) = run_with(st);
    let s = draw(&run, 100, 30, &Theme::new(true));
    contains_all(
        &s,
        &[
            "[>] Search the raw data for lost files",
            "3 files found",
            "raw data search: 100 MiB of 272 MiB read, 3 files found",
        ],
    );
}

#[test]
fn the_progress_fits_the_smallest_window_and_works_without_colour() {
    let (run, _d) = run_with(mid_run());
    let s = draw(&run, 80, 24, &Theme::new(false));
    assert_eq!(s.lines().count(), 24);
    contains_all(
        &s,
        &[
            "[>] Copy the files found by name",
            "[ ] Write the report",
            "found so far:",
            "Latest files",
        ],
    );
    // Only words and symbols: no escape codes, no colours needed.
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| run.draw(f, f.area(), &Theme::new(false)))
        .unwrap();
    for cell in term.backend().buffer().content() {
        assert!(matches!(cell.fg, ratatui::style::Color::Reset), "{cell:?}");
    }
}
