//! Every page fits the smallest window (900 x 600) without panicking, in
//! the dark theme too.

use std::time::Duration;

use crate::common::*;
use diskrescue_gui::Page;
use egui_kittest::Harness;

#[test]
fn every_page_draws_at_the_minimum_size_in_both_themes() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let (app, _) = fake_app(fake_devices());
        let mut h = Harness::builder()
            .with_size(egui::vec2(900.0, 600.0))
            .with_theme(theme)
            .with_max_steps(64)
            .build_eframe(move |_cc| app);
        wait_for(&mut h, Duration::from_secs(10), "the drive list", |h| {
            has_label(h, "Rescue SanDisk Ultra")
        });
        click(&mut h, "Rescue SanDisk Ultra");
        for step in 0..3 {
            h.state_mut().wizard.as_mut().unwrap().step = step;
            h.step();
            h.step();
        }
        for page in [
            Page::ImageFile,
            Page::Repair,
            Page::Results,
            Page::Help,
            Page::Drives,
        ] {
            h.state_mut().go(page);
            h.step();
            h.step();
        }
        h.state_mut().show_preview_progress(
            "Rescue of SanDisk SDXC Card",
            diskrescue_gui::progress::preview_snapshot(),
        );
        h.step();
        h.step();
        let name = if theme == egui::Theme::Dark {
            "small-dark"
        } else {
            "small-light"
        };
        if std::env::var_os("DISKRESCUE_SMALL_SHOTS").is_some() {
            screenshot(h, name);
        }
    }
}
