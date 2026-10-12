//! Every screen, in both themes, at the default and the smallest window
//! size. Always laid out (nothing may panic); rendered and saved as PNG
//! files to `$DISKRESCUE_GALLERY_DIR` when it is set, for reviewing the
//! design.

use std::path::PathBuf;
use std::time::Duration;

use crate::common::*;
use diskrescue_gui::{DiskRescueApp, Page};
use egui_kittest::Harness;

fn shoot(h: &mut Harness<'static, DiskRescueApp>, name: &str) {
    for _ in 0..3 {
        h.step();
    }
    let Some(dir) = std::env::var_os("DISKRESCUE_GALLERY_DIR").map(PathBuf::from) else {
        return;
    };
    let _g = render_lock();
    let img = h.render().expect("headless rendering");
    std::fs::create_dir_all(&dir).unwrap();
    img.save(dir.join(format!("{name}.png"))).unwrap();
}

#[test]
fn every_screen_renders() {
    for (theme, size, tag) in [
        (egui::Theme::Light, SIZE, "light"),
        (egui::Theme::Dark, egui::vec2(900.0, 600.0), "dark-small"),
    ] {
        let (app, _) = fake_app(fake_devices());
        let mut h = Harness::builder()
            .with_size(size)
            .with_theme(theme)
            .with_max_steps(64)
            .build_eframe(move |_cc| app);
        wait_for(&mut h, Duration::from_secs(10), "the drive list", |h| {
            has_label(h, "Rescue SanDisk Ultra")
        });
        shoot(&mut h, &format!("{tag}-drives"));
        click(&mut h, "Rescue SanDisk Ultra");
        shoot(&mut h, &format!("{tag}-wizard-1"));
        h.state_mut().wizard.as_mut().unwrap().step = 1;
        h.step();
        click(&mut h, "Advanced settings");
        shoot(&mut h, &format!("{tag}-wizard-2"));
        h.state_mut().wizard.as_mut().unwrap().step = 2;
        wait_for(&mut h, Duration::from_secs(10), "the checks", |h| {
            !has_label(h, "Checking that everything is safe...")
        });
        shoot(&mut h, &format!("{tag}-wizard-3"));
        for (page, name) in [
            (Page::ImageFile, "image"),
            (Page::Repair, "repair"),
            (Page::Results, "results-chooser"),
            (Page::Help, "help"),
        ] {
            h.state_mut().go(page);
            h.step();
            shoot(&mut h, &format!("{tag}-{name}"));
        }
        h.state_mut().show_preview_progress(
            "Rescue of SanDisk SDXC Card",
            diskrescue_gui::progress::preview_snapshot(),
        );
        h.step();
        shoot(&mut h, &format!("{tag}-progress"));
        let _g = render_lock();
        drop(h);
    }
}
