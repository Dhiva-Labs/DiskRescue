//! Navigation, the drive list, the wizard's safety checks, previews.

use std::time::Duration;

use crate::common::*;
use diskrescue_gui::Page;

#[test]
fn navigation_reaches_every_page_and_keeps_state() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    wait_for(&mut h, Duration::from_secs(10), "the drive list", |h| {
        has_label(h, "Rescue SanDisk Ultra")
    });
    for (label, page, marker) in [
        ("Recover from an image file", Page::ImageFile, "Image file"),
        ("Repair photos & videos", Page::Repair, "Add files..."),
        ("Results", Page::Results, "Results folder"),
        ("Help", Page::Help, "First: stop using the drive"),
        ("Rescue from a drive", Page::Drives, "Refresh"),
    ] {
        click(&mut h, label);
        assert_eq!(h.state().page, page, "{label}");
        assert!(has_label(&h, marker), "{marker} missing on {label}");
    }
    // Typed text survives a page switch.
    click(&mut h, "Recover from an image file");
    type_into(&mut h, "Image file", "/nowhere/card.img");
    click(&mut h, "Help");
    click(&mut h, "Recover from an image file");
    assert_eq!(h.state().image_page.path, "/nowhere/card.img");
}

#[test]
fn drives_page_shows_cards_badges_and_blocks_the_system_disk() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    wait_for(&mut h, Duration::from_secs(10), "the drive list", |h| {
        has_label(h, "Rescue SanDisk Ultra")
    });
    for name in [
        "SanDisk Ultra",
        "SD64G",
        "WD Elements 25A2",
        "Samsung SSD 870 EVO",
        "WD Black SN770",
    ] {
        assert!(has_label(&h, name), "{name} missing");
    }
    // A drive in use: "Mounted", or on Windows (a drive letter) "In use".
    assert!(has_label(
        &h,
        if cfg!(windows) { "In use" } else { "Mounted" }
    ));
    assert!(has_label(&h, "Failing"));
    assert!(has_label(&h, "This is the disk your computer runs from"));
    assert!(enabled(&h, "Rescue SanDisk Ultra"));
    assert!(
        !enabled(&h, "Rescue WD Black SN770"),
        "the system disk must not be rescued"
    );
    assert!(!enabled(&h, "Rescue WD Elements 25A2") || has_label(&h, "Failing"));
    screenshot(h, "drives");
}

#[test]
fn rescue_opens_the_wizard_for_that_drive() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    wait_for(&mut h, Duration::from_secs(10), "the drive list", |h| {
        has_label(h, "Rescue SD64G")
    });
    click(&mut h, "Rescue SD64G");
    assert_eq!(h.state().page, Page::Wizard);
    assert!(has_label(&h, "Rescue SD64G"));
    // The drive list keeps a way back to it.
    click(&mut h, "Rescue from a drive");
    assert!(has_label(&h, "Return to it"));
    click(&mut h, "Return to it");
    assert_eq!(h.state().page, Page::Wizard);
}

#[test]
fn progress_shows_the_block_map_and_asks_before_stopping() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    h.state_mut().show_preview_progress(
        "Rescue of SanDisk SDXC Card",
        diskrescue_gui::progress::preview_snapshot(),
    );
    h.step();
    h.step();
    assert_eq!(h.state().page, Page::Progress);
    assert!(has_label_containing(&h, "Map of the drive: "));
    assert!(has_label_containing(&h, "Copied "));
    assert!(has_label_containing(&h, "Unreadable "));
    assert!(has_label(&h, "Pause"));

    click(&mut h, "Stop...");
    assert!(has_label(&h, "Stop the rescue?"));
    click(&mut h, "Keep going");
    assert!(!has_label(&h, "Stop the rescue?"));
    screenshot(h, "progress");
}

#[test]
fn a_drive_that_disconnects_is_explained() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    let mut snap = diskrescue_gui::progress::preview_snapshot();
    snap.image.as_mut().unwrap().device_lost = true;
    h.state_mut()
        .show_preview_progress("Rescue of SanDisk SDXC Card", snap);
    h.step();
    assert!(has_label_containing(
        &h,
        "The drive disconnected. DiskRescue is waiting for it to come back"
    ));
}

#[test]
fn closing_the_window_during_a_rescue_asks_first() {
    let (app, _) = fake_app(fake_devices());
    let mut h = harness(app);
    h.state_mut().show_preview_progress(
        "Rescue of SanDisk SDXC Card",
        diskrescue_gui::progress::preview_snapshot(),
    );
    h.step();
    h.input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    h.step();
    let cancelled = h
        .output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .is_some_and(|v| v.commands.contains(&egui::ViewportCommand::CancelClose));
    assert!(cancelled, "the close must be held back");
    h.step();
    assert!(has_label(&h, "A rescue is still running"));
    click(&mut h, "Keep it running");
    assert!(!has_label(&h, "A rescue is still running"));
}
