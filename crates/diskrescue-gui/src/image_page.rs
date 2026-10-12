//! "Recover from an image file": pick a copy made earlier (by DiskRescue,
//! GNU ddrescue or another tool), then continue in the wizard.

use std::path::PathBuf;
use std::time::Duration;

use diskrescue_app::{devices, source, text};
use diskrescue_core::device::DeviceKind;
use diskrescue_imaging::{MapFile, Stats};
use egui::Ui;

use crate::tasks::{Task, Watch};
use crate::theme::Kind;
use crate::wizard::expand;
use crate::{system, widgets};

/// What is known about the chosen file.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Its size.
    pub size: u64,
    /// The progress map next to it and what it says.
    pub map: Option<(PathBuf, Option<Stats>)>,
}

/// The page's state.
pub struct ImagePage {
    /// The path, as typed.
    pub path: String,
    info: Watch<PathBuf, Result<ImageInfo, String>>,
    picker: Option<Task<Option<PathBuf>>>,
}

impl Default for ImagePage {
    fn default() -> Self {
        ImagePage {
            path: String::new(),
            info: Watch::new(Duration::from_millis(300)),
            picker: None,
        }
    }
}

fn inspect(path: PathBuf) -> Result<ImageInfo, String> {
    let info = devices::describe(&path).map_err(|e| widgets::gui_text(e.message()))?;
    if info.kind != DeviceKind::ImageFile {
        return Err(
            "This is a drive, not an image file. To rescue a drive, use \"Rescue from a drive\"."
                .into(),
        );
    }
    if info.size == 0 {
        return Err("This file is empty: there is nothing to recover from it.".into());
    }
    let map_path = source::resolve_mapfile(&path);
    let map = (map_path != path).then(|| {
        let stats = MapFile::load(&map_path).ok().map(|m| m.map.stats());
        (map_path, stats)
    });
    Ok(ImageInfo {
        size: info.size,
        map,
    })
}

/// What the page asks the app to do.
pub enum Action {
    /// Continue in the wizard with this image.
    Continue(PathBuf),
}

impl ImagePage {
    /// Keep the file information up to date.
    pub fn update(&mut self, ctx: &egui::Context) {
        if let Some(t) = &mut self.picker {
            if let Some(r) = t.poll() {
                self.picker = None;
                if let Some(p) = r {
                    self.path = p.display().to_string();
                }
            }
        }
        if !self.path.trim().is_empty() {
            let p = expand(&self.path);
            self.info.update(ctx, &p, inspect);
        }
    }

    /// Draw the page.
    pub fn show(&mut self, ui: &mut Ui) -> Option<Action> {
        let mut action = None;
        widgets::page_title(
            ui,
            "Recover from an image file",
            "Use a copy of a drive made earlier, by DiskRescue or by another tool such as GNU ddrescue.",
        );
        widgets::card(ui, |ui| {
            let f = widgets::path_field(
                ui,
                "Image file",
                &mut self.path,
                "For example /media/backup/card.img",
                "Choose file...",
                self.picker.is_none(),
            );
            if f.browse && self.picker.is_none() {
                let start = system::home_dir();
                self.picker = Some(Task::spawn(ui.ctx(), "diskrescue-dialog", move || {
                    rfd::FileDialog::new()
                        .set_title("Choose the image file")
                        .set_directory(start)
                        .add_filter("Disk images", &["img", "dd", "raw", "bin", "iso", "image"])
                        .add_filter("All files", &["*"])
                        .pick_file()
                }));
            }
            if self.path.trim().is_empty() {
                widgets::weak(
                    ui,
                    "When the image has a progress map next to it (for example card.img.map), \
                     DiskRescue uses it to know which parts of the drive could not be read.",
                );
                return;
            }
            let p = expand(&self.path);
            match self.info.get(&p) {
                None => {
                    ui.horizontal(|ui| {
                        widgets::busy(ui, 18.0);
                        ui.label("Looking at the file...");
                    });
                }
                Some(Err(e)) => widgets::callout_text(ui, Kind::Bad, "This file cannot be used", e),
                Some(Ok(info)) => {
                    ui.label(format!("Size: {}", text::size_label(info.size)));
                    match &info.map {
                        Some((m, Some(st))) => {
                            let kind = if st.bad_sectors > 0 || st.pending() > 0 {
                                Kind::Warn
                            } else {
                                Kind::Good
                            };
                            widgets::callout_text(ui, kind, "Progress map found", &map_text(m, st));
                        }
                        Some((m, None)) => widgets::callout_text(
                            ui,
                            Kind::Warn,
                            "The progress map cannot be read",
                            &format!(
                                "{} exists but could not be read, so DiskRescue cannot tell which \
                                 parts of the drive were missing. Recovery still works.",
                                m.display()
                            ),
                        ),
                        None => {
                            widgets::weak(
                                ui,
                                "No progress map next to it: the whole file is used as it is.",
                            );
                        }
                    }
                }
            }
        });
        ui.add_space(10.0);
        let ready = self
            .info
            .get(&expand(&self.path))
            .is_some_and(|r| r.is_ok());
        if widgets::primary_button(ui, ready, "Continue").clicked() {
            action = Some(Action::Continue(expand(&self.path)));
        }
        ui.add_space(12.0);
        widgets::weak(ui, "You can also drop an image file onto this window.");
        action
    }
}

fn map_text(m: &std::path::Path, st: &Stats) -> String {
    let mut s = format!(
        "{}: {} of the drive was copied.",
        system::file_name(m),
        text::percent(st.rescued, st.total())
    );
    if st.bad_sectors > 0 {
        s.push_str(&format!(
            " {} could not be read; files stored there will be marked as damaged.",
            text::bytes(st.bad_sectors)
        ));
    }
    if st.pending() > 0 {
        s.push_str(
            " The copy was not finished: you can still recover from it, or finish the copy first \
             by rescuing the drive again with the same image name.",
        );
    }
    s
}
