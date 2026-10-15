//! "Repair photos & videos": damaged files the user already has. Files
//! are dropped onto the window or added with a dialog; the originals are
//! never changed.

use std::path::{Path, PathBuf};

use diskrescue_app::jobs::{start_repair, RepairState};
use diskrescue_app::plan::RepairPlan;
use diskrescue_app::results::{self, ResultItem};
use diskrescue_app::{text, JobHandle, JobOutcome};
use diskrescue_core::format::ValidationStatus;
use diskrescue_recover::report::{Origin, Outcome};
use egui::{vec2, RichText, Sense, Stroke, Ui, WidgetType};

use crate::progress::category_of;
use crate::tasks::Task;
use crate::theme::{self, Kind};
use crate::wizard::expand;
use crate::{icons, system, widgets};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Files,
    Donors,
    Output,
}

/// One repaired (or not) file.
#[derive(Debug, Clone)]
pub struct Row {
    /// The input.
    pub input: PathBuf,
    /// Its state before repair.
    pub before: Option<ValidationStatus>,
    /// What happened.
    pub outcome: Outcome,
    /// The repaired copy.
    pub repaired: Option<PathBuf>,
    /// Notes (what was done, why not).
    pub notes: Vec<String>,
}

/// The page's state.
pub struct RepairPage {
    /// Files to repair.
    pub files: Vec<PathBuf>,
    /// Healthy reference files.
    pub donors: Vec<PathBuf>,
    /// Where the repaired copies go, as typed.
    pub output: String,
    /// The running (or last) repair.
    pub job: Option<JobHandle<RepairState>>,
    /// The output of the running (or last) repair.
    pub job_output: PathBuf,
    /// The folder name shown instead of `job_output` (for screenshots made
    /// on another system, which write the path in that system's style).
    #[doc(hidden)]
    pub shown_folder: Option<String>,
    /// Why the last repair did not run.
    pub error: Option<String>,
    picker: Option<(Target, Task<Option<Vec<PathBuf>>>)>,
    note: Option<String>,
    scroll_top: bool,
}

impl Default for RepairPage {
    fn default() -> Self {
        RepairPage {
            files: Vec::new(),
            donors: Vec::new(),
            output: crate::wizard::free_folder(
                &system::home_dir().join("DiskRescue").join("Repaired"),
            )
            .display()
            .to_string(),
            job: None,
            job_output: PathBuf::new(),
            shown_folder: None,
            error: None,
            picker: None,
            note: None,
            scroll_top: false,
        }
    }
}

impl RepairPage {
    /// Add files to repair (duplicates ignored).
    pub fn add_files(&mut self, files: impl IntoIterator<Item = PathBuf>) {
        for f in files {
            if f.is_file() && !self.files.contains(&f) {
                self.files.push(f);
            }
        }
    }

    /// Add reference files.
    pub fn add_donors(&mut self, files: impl IntoIterator<Item = PathBuf>) {
        for f in files {
            if !self.donors.contains(&f) {
                self.donors.push(f);
            }
        }
    }

    /// True while a repair runs.
    pub fn running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Start repairing `files` with `donors` into `output`.
    pub fn start(&mut self, files: Vec<PathBuf>, donors: Vec<PathBuf>, output: PathBuf) {
        self.error = None;
        self.job_output = output.clone();
        self.job = Some(start_repair(RepairPlan {
            files,
            output,
            donors,
            aggressive: false,
        }));
    }

    /// Take finished dialogs.
    pub fn update(&mut self, ctx: &egui::Context) {
        if let Some((target, t)) = &mut self.picker {
            if let Some(r) = t.poll() {
                let target = *target;
                self.picker = None;
                if let Some(v) = r {
                    match target {
                        Target::Files => self.add_files(v),
                        Target::Donors => self.add_donors(v),
                        Target::Output => {
                            if let Some(p) = v.into_iter().next() {
                                self.output = p.display().to_string();
                            }
                        }
                    }
                }
            }
        }
        if self.running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn pick(&mut self, ctx: &egui::Context, target: Target) {
        if self.picker.is_some() {
            return;
        }
        let start = system::home_dir();
        let task = Task::spawn(ctx, "diskrescue-dialog", move || {
            let d = rfd::FileDialog::new().set_directory(start);
            match target {
                Target::Files => d
                    .set_title("Choose the damaged photos and videos")
                    .pick_files(),
                Target::Donors => d
                    .set_title("Choose healthy files from the same camera or phone")
                    .pick_files(),
                Target::Output => d
                    .set_title("Choose where to save the repaired copies")
                    .set_can_create_directories(true)
                    .pick_folder()
                    .map(|p| vec![p]),
            }
        });
        self.picker = Some((target, task));
    }

    /// `p` (inside the output folder) as shown to the user.
    fn shown(&self, p: &Path) -> String {
        match (&self.shown_folder, p.strip_prefix(&self.job_output)) {
            (Some(folder), Ok(rel)) => {
                let mut s = folder.trim_end_matches('\\').to_string();
                for part in rel.components() {
                    s.push('\\');
                    s.push_str(&part.as_os_str().to_string_lossy());
                }
                s
            }
            _ => p.display().to_string(),
        }
    }

    /// The rows of the last repair (live while it runs).
    pub fn rows(&self) -> Vec<Row> {
        let Some(job) = &self.job else {
            return Vec::new();
        };
        let s = job.snapshot();
        if let Some(report) = &s.report {
            let items: Vec<ResultItem> = results::items(report, &self.job_output);
            return report
                .files
                .iter()
                .zip(items)
                .map(|(f, it)| Row {
                    input: match &f.origin {
                        Origin::Input { path } => path.clone(),
                        _ => PathBuf::from(f.origin_text()),
                    },
                    before: f.validation.as_ref().map(|v| v.status),
                    outcome: f.outcome,
                    repaired: it.repaired.or(
                        if f.outcome == Outcome::Repaired || f.outcome == Outcome::PartiallyRepaired
                        {
                            it.path
                        } else {
                            None
                        },
                    ),
                    notes: it.notes,
                })
                .collect();
        }
        s.files
            .iter()
            .map(|f| Row {
                input: PathBuf::from(&f.name),
                before: None,
                outcome: f.outcome,
                repaired: None,
                notes: Vec::new(),
            })
            .collect()
    }

    /// Draw the page.
    pub fn show(&mut self, ui: &mut Ui, is_root: bool) {
        let ctx = ui.ctx().clone();
        if self.scroll_top {
            ui.scroll_to_cursor(Some(egui::Align::TOP));
            self.scroll_top = false;
        }
        widgets::page_title(
            ui,
            "Repair photos & videos",
            "For files you already have that will not open or play, such as a video that says \"moov \
             atom not found\" or a photo that is half grey. Your files are never changed; repaired \
             copies are saved separately.",
        );
        if self.job.is_some() || self.error.is_some() {
            self.outcome_section(ui, is_root);
            ui.add_space(8.0);
            widgets::section(ui, "Repair more files");
        }

        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        widgets::card(ui, |ui| {
            let h = 90.0;
            let (rect, resp) =
                ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
            resp.widget_info(|| {
                egui::WidgetInfo::labeled(WidgetType::Other, true, "Drop files here to repair them")
            });
            let accent = theme::accent(ui.visuals());
            let stroke_c = if hovering {
                accent
            } else {
                ui.visuals().weak_text_color()
            };
            let fill = if hovering {
                theme::palette(ui.visuals(), Kind::Info).bg
            } else {
                theme::palette(ui.visuals(), Kind::Neutral).bg
            };
            ui.painter().rect_filled(rect, 8.0, fill);
            dashed_rect(ui, rect.shrink(2.0), Stroke::new(1.5, stroke_c));
            ui.painter().text(
                rect.center() - vec2(0.0, 10.0),
                egui::Align2::CENTER_CENTER,
                if hovering {
                    "Release to add the files"
                } else {
                    "Drop damaged photos and videos here"
                },
                egui::FontId::proportional(17.0),
                ui.visuals().strong_text_color(),
            );
            ui.painter().text(
                rect.center() + vec2(0.0, 14.0),
                egui::Align2::CENTER_CENTER,
                "JPEG, PNG, MP4, MOV, AVI",
                egui::FontId::proportional(13.0),
                ui.visuals().weak_text_color(),
            );
            ui.horizontal(|ui| {
                if widgets::button(ui, self.picker.is_none(), "Add files...").clicked() {
                    self.pick(&ctx, Target::Files);
                }
                if !self.files.is_empty() && ui.button("Clear the list").clicked() {
                    self.files.clear();
                }
            });
            if !self.files.is_empty() {
                widgets::section(ui, &format!("Files to repair ({})", self.files.len()));
                let mut remove = None;
                for (i, f) in self.files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                        icons::file(
                            ui.painter(),
                            r,
                            category_of(f),
                            theme::palette(ui.visuals(), Kind::Neutral).fg,
                        );
                        ui.label(system::file_name(f));
                        widgets::weak(
                            ui,
                            f.parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        );
                        let r = ui.small_button("Remove");
                        let r = widgets::accessible(
                            r,
                            WidgetType::Button,
                            &format!("Remove {}", system::file_name(f)),
                        );
                        if r.clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.files.remove(i);
                }
            }
        });

        ui.add_space(6.0);
        widgets::card(ui, |ui| {
            ui.label(RichText::new("Reference files (recommended for videos)").strong());
            widgets::weak(
                ui,
                "A healthy photo or video taken with the same camera or phone, with the same \
                 settings (resolution, frame rate). DiskRescue uses it to rebuild what is missing, \
                 for example the index of a video that will not play.",
            );
            let mut remove = None;
            for (i, d) in self.donors.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(system::file_name(d));
                    widgets::weak(
                        ui,
                        d.parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    );
                    let r = ui.small_button("Remove");
                    let r = widgets::accessible(
                        r,
                        WidgetType::Button,
                        &format!("Remove reference {}", system::file_name(d)),
                    );
                    if r.clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.donors.remove(i);
            }
            if widgets::button(ui, self.picker.is_none(), "Add reference files...").clicked() {
                self.pick(&ctx, Target::Donors);
            }
        });

        ui.add_space(6.0);
        widgets::card(ui, |ui| {
            let f = widgets::path_field(
                ui,
                "Save repaired copies to",
                &mut self.output,
                "A folder",
                "Choose folder...",
                self.picker.is_none(),
            );
            if f.browse {
                self.pick(&ctx, Target::Output);
            }
            let out = expand(&self.output);
            let beside = self
                .files
                .iter()
                .any(|f| f.parent().is_some_and(|d| out.starts_with(d)));
            if !self.output.trim().is_empty() && beside {
                widgets::callout_text(
                    ui,
                    Kind::Warn,
                    "This saves the copies next to the damaged files",
                    "If those files are on the card or drive you want to rescue, choose a folder on \
                     your computer instead: writing to that card or drive can overwrite deleted \
                     photos.",
                );
            }
        });

        ui.add_space(10.0);
        let running = self.running();
        let can = !self.files.is_empty() && !self.output.trim().is_empty() && !running;
        let label = match self.files.len() {
            0 => "Repair".to_string(),
            1 => "Repair 1 file".to_string(),
            n => format!("Repair {n} files"),
        };
        ui.horizontal(|ui| {
            let r = widgets::primary_button(ui, can, &label);
            let r = widgets::accessible(r, WidgetType::Button, "Repair");
            if r.clicked() {
                let out = expand(&self.output);
                self.start(self.files.clone(), self.donors.clone(), out);
                self.scroll_top = true;
            }
            if running {
                widgets::busy(ui, 18.0);
                let s = self.job.as_ref().map(|j| j.snapshot()).unwrap_or_default();
                let stage = if s.pipeline.stage_label.is_empty() {
                    "Starting...".to_string()
                } else {
                    s.pipeline.stage_label.clone()
                };
                ui.label(stage);
            }
        });
    }

    fn outcome_section(&mut self, ui: &mut Ui, is_root: bool) {
        if let Some(e) = &self.error {
            widgets::callout_text(ui, Kind::Bad, "The repair could not run", e);
        }
        if let Some(n) = &self.note {
            widgets::callout_text(ui, Kind::Warn, "", n);
        }
        if let Some(job) = &self.job {
            if !job.is_finished() {
                let s = job.snapshot();
                ui.horizontal(|ui| {
                    widgets::busy(ui, 18.0);
                    let stage = if s.pipeline.stage_label.is_empty() {
                        "Starting...".to_string()
                    } else {
                        format!("{}...", s.pipeline.stage_label)
                    };
                    ui.label(RichText::new(stage).strong());
                });
            }
            match job.outcome() {
                Some(JobOutcome::Refused(m)) | Some(JobOutcome::Failed(m)) => {
                    widgets::callout_text(
                        ui,
                        Kind::Bad,
                        "The repair could not run",
                        &widgets::gui_text(&m),
                    );
                }
                Some(JobOutcome::Finished) => {
                    let rows = self.rows();
                    let fixed = rows
                        .iter()
                        .filter(|r| {
                            matches!(r.outcome, Outcome::Repaired | Outcome::PartiallyRepaired)
                        })
                        .count();
                    let partly = rows
                        .iter()
                        .filter(|r| r.outcome == Outcome::PartiallyRepaired)
                        .count();
                    let kind = if fixed > 0 { Kind::Good } else { Kind::Warn };
                    widgets::callout_text(
                        ui,
                        kind,
                        &format!(
                            "Done: {} of {} repaired{}",
                            fixed,
                            text::plural(rows.len() as u64, "file", "files"),
                            if partly > 0 {
                                format!(" ({partly} only partly)")
                            } else {
                                String::new()
                            }
                        ),
                        &format!(
                            "Repaired copies are in {}.",
                            self.shown(&self.job_output.join("repaired"))
                        ),
                    );
                }
                _ => {}
            }
        }
        let rows = self.rows();
        if !rows.is_empty() {
            widgets::section(ui, "Results");
            for (i, r) in rows.iter().enumerate() {
                ui.push_id(i, |ui| {
                    widgets::card(ui, |ui| self.row(ui, r, is_root));
                });
            }
        }
    }

    fn row(&mut self, ui: &mut Ui, r: &Row, is_root: bool) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(system::file_name(&r.input)).strong());
            if let Some(b) = r.before {
                widgets::weak(ui, "was");
                let k = Kind::from(text::validation_tone(Some(b)));
                widgets::badge(ui, text::validation_label(Some(b)), k);
            }
            widgets::weak(ui, "now");
            let k = Kind::from(text::outcome_tone(r.outcome));
            widgets::badge(ui, text::repair_result_label(r.outcome), k);
        });
        ui.label(outcome_text(r.outcome));
        if !r.notes.is_empty() {
            egui::CollapsingHeader::new("Details")
                .id_salt("notes")
                .show(ui, |ui| {
                    for n in &r.notes {
                        ui.label(RichText::new(widgets::gui_text(n)).small());
                    }
                });
        }
        if let Some(p) = &r.repaired {
            let shown = self.shown(p);
            ui.horizontal(|ui| {
                widgets::weak(ui, format!("Repaired copy: {shown}"));
            });
            if !is_root {
                ui.horizontal(|ui| {
                    let name = system::file_name(&r.input);
                    let b = ui.button("Open repaired copy");
                    let b = widgets::accessible(
                        b,
                        WidgetType::Button,
                        &format!("Open the repaired copy of {name}"),
                    );
                    if b.clicked() {
                        if let Err(e) = system::open_path(p) {
                            self.note = Some(e);
                        }
                    }
                    let b = ui.button("Show in folder");
                    let b = widgets::accessible(
                        b,
                        WidgetType::Button,
                        &format!("Show the repaired copy of {name} in its folder"),
                    );
                    if b.clicked() {
                        if let Err(e) = system::show_in_folder(p) {
                            self.note = Some(e);
                        }
                    }
                });
            }
        }
    }
}

/// What an outcome means for the person holding the file.
pub fn outcome_text(o: Outcome) -> &'static str {
    match o {
        Outcome::Repaired => "The repaired copy opens normally.",
        Outcome::PartiallyRepaired => {
            "The repaired copy opens, but part of it is missing (for example a grey area at the \
             bottom of a photo, or a video that ends early)."
        }
        Outcome::Damaged => {
            "This file could not be repaired. A reference file from the same camera or phone may \
             help."
        }
        Outcome::Failed => "This file could not be read as a photo or video.",
        Outcome::Recovered | Outcome::RecoveredUnverified => "This file is not damaged.",
        Outcome::Duplicate => "The same file was given twice.",
    }
}

fn dashed_rect(ui: &Ui, rect: egui::Rect, stroke: Stroke) {
    let p = ui.painter();
    let dash = 8.0;
    let gap = 5.0;
    let edge = |a: egui::Pos2, b: egui::Pos2| {
        let len = a.distance(b);
        let dir = (b - a) / len.max(1.0);
        let mut t = 0.0;
        while t < len {
            let e = (t + dash).min(len);
            p.line_segment([a + dir * t, a + dir * e], stroke);
            t += dash + gap;
        }
    };
    edge(rect.left_top(), rect.right_top());
    edge(rect.right_top(), rect.right_bottom());
    edge(rect.right_bottom(), rect.left_bottom());
    edge(rect.left_bottom(), rect.left_top());
}

/// Files dropped onto the window, as paths.
pub fn dropped_paths(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect()
    })
}

/// True when `p` looks like a disk image.
pub fn looks_like_image(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(ext.as_str(), "img" | "dd" | "raw" | "iso" | "bin" | "image")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shown_folder_replaces_the_output_folder() {
        let mut page = RepairPage {
            job_output: PathBuf::from("/out"),
            ..RepairPage::default()
        };
        let p = Path::new("/out/repaired/a.jpg");
        assert_eq!(page.shown(p), p.display().to_string());
        page.shown_folder = Some(r"C:\Pictures\Repaired\".into());
        assert_eq!(page.shown(p), r"C:\Pictures\Repaired\repaired\a.jpg");
        let other = Path::new("/elsewhere/a.jpg");
        assert_eq!(page.shown(other), other.display().to_string());
    }
}
