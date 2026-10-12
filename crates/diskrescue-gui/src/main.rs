//! The DiskRescue desktop app.

#![cfg_attr(windows, windows_subsystem = "windows")]

use diskrescue_gui::{DiskRescueApp, Saved, APP_ID};

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DiskRescue")
            .with_app_id(APP_ID)
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DiskRescue",
        options,
        Box::new(|cc| {
            let saved: Saved = cc
                .storage
                .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
                .unwrap_or_default();
            Ok(Box::new(DiskRescueApp::new(saved)))
        }),
    )
}
