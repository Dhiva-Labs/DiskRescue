//! The repair page on damaged files with reference files: files dropped
//! onto the window, Repair, before/after status and the repaired copies.

use std::path::PathBuf;
use std::time::Duration;

use crate::common::*;
use diskrescue_gui::Page;
use diskrescue_recover::report::Outcome;

#[test]
fn damaged_files_are_repaired_with_reference_files() {
    let Some(sc) = damaged_media() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let damaged = dir.path().join("Phone videos");
    let good = dir.path().join("Good files");
    std::fs::create_dir_all(&damaged).unwrap();
    std::fs::create_dir_all(&good).unwrap();
    let mut inputs: Vec<PathBuf> = Vec::new();
    for n in ["truncated.jpg", "zeroed_header.jpg", "no_moov.mp4"] {
        let p = damaged.join(n);
        std::fs::copy(sc.root.join(n), &p).unwrap();
        inputs.push(p);
    }
    let mut donors: Vec<PathBuf> = Vec::new();
    for n in ["donor.jpg", "donor.mp4"] {
        let p = good.join(n);
        std::fs::copy(sc.root.join("donors").join(n), &p).unwrap();
        donors.push(p);
    }
    let out = dir.path().join("Repaired");

    let (app, _) = fake_app(vec![]);
    let mut h = harness(app);
    // Dropping files anywhere opens the repair page with them.
    drop_files(&mut h, &inputs);
    assert_eq!(h.state().page, Page::Repair);
    assert_eq!(h.state().repair.files, inputs);
    for n in ["truncated.jpg", "zeroed_header.jpg", "no_moov.mp4"] {
        assert!(has_label(&h, &format!("Remove {n}")), "{n} not listed");
    }
    // Reference files come from a file dialog in the app.
    h.state_mut().repair.add_donors(donors.clone());
    h.step();
    assert!(has_label(&h, "Remove reference donor.mp4"));
    type_into(
        &mut h,
        "Save repaired copies to",
        &out.display().to_string(),
    );
    assert!(enabled(&h, "Repair"));
    click(&mut h, "Repair");

    wait_for(&mut h, Duration::from_secs(300), "the repair", |h| {
        has_label_containing(h, "Done: ")
    });
    assert!(
        has_label_containing(&h, "Done: 3 of 3 files repaired"),
        "not everything was repaired"
    );
    let rows = h.state().repair.rows();
    assert_eq!(rows.len(), 3);
    for r in &rows {
        assert!(
            matches!(r.outcome, Outcome::Repaired | Outcome::PartiallyRepaired),
            "{:?}",
            r
        );
        let copy = r.repaired.as_ref().expect("a repaired copy");
        assert!(
            copy.is_file() && copy.starts_with(&out),
            "{}",
            copy.display()
        );
        assert!(r.before.is_some());
    }
    assert!(out.join("repaired").join("no_moov.mp4").is_file());
    assert!(has_label(&h, "Open the repaired copy of no_moov.mp4"));
    // The originals are untouched.
    for (i, n) in ["truncated.jpg", "zeroed_header.jpg", "no_moov.mp4"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            std::fs::read(&inputs[i]).unwrap(),
            std::fs::read(sc.root.join(n)).unwrap()
        );
    }
    screenshot(h, "repair");
}
