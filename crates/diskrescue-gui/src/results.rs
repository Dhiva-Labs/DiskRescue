//! "Results": the recovered files as a grid of thumbnails, with filters,
//! search, and a detail panel for each file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_app::results::{self, ResultItem};
use diskrescue_app::text;
use diskrescue_core::media::MediaCategory;
use diskrescue_recover::report::{Outcome, Report};
use egui::{vec2, RichText, Ui, WidgetType};

use crate::progress::tile;
use crate::tasks::Task;
use crate::theme::{self, Kind};
use crate::thumbs::{Thumb, Thumbnails};
use crate::{system, widgets};

/// The filters above the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Every file that was saved.
    All,
    /// Photos and RAW photos.
    Photos,
    /// Videos.
    Videos,
    /// Files with problems left.
    Damaged,
    /// Files with a repaired copy.
    Repaired,
    /// Files found as deleted files.
    Deleted,
}

impl Filter {
    /// Every filter, in display order.
    pub const ALL: [Filter; 6] = [
        Filter::All,
        Filter::Photos,
        Filter::Videos,
        Filter::Damaged,
        Filter::Repaired,
        Filter::Deleted,
    ];

    /// Its name.
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Photos => "Photos",
            Filter::Videos => "Videos",
            Filter::Damaged => "Damaged",
            Filter::Repaired => "Repaired",
            Filter::Deleted => "Deleted",
        }
    }

    /// True when `item` passes.
    pub fn matches(self, item: &ResultItem) -> bool {
        match self {
            Filter::All => true,
            Filter::Photos => matches!(
                item.category,
                MediaCategory::Photo | MediaCategory::RawPhoto
            ),
            Filter::Videos => item.category == MediaCategory::Video,
            Filter::Damaged => {
                matches!(item.outcome, Outcome::Damaged | Outcome::PartiallyRepaired)
            }
            Filter::Repaired => {
                matches!(item.outcome, Outcome::Repaired | Outcome::PartiallyRepaired)
            }
            Filter::Deleted => item.deleted,
        }
    }
}

/// The file a result item shows (the repaired copy when there is one).
pub fn shown_path(item: &ResultItem) -> Option<&Path> {
    item.repaired.as_deref().or(item.path.as_deref())
}

/// The name of a result item.
pub fn item_name(item: &ResultItem) -> String {
    match item.path.as_deref().or(item.repaired.as_deref()) {
        Some(p) => system::file_name(p),
        None => format!("file {}", item.id),
    }
}

/// Where a file was found, in words.
pub fn origin_text(item: &ResultItem) -> String {
    let o = item.origin.as_str();
    if let Some(rest) = o.strip_prefix("vol") {
        if let Some((vol, path)) = rest.split_once(":/") {
            let path = path.trim_end_matches(" (deleted)");
            return if item.deleted {
                format!("{path} on volume {vol} (it had been deleted)")
            } else {
                format!("{path} on volume {vol}")
            };
        }
    }
    if let Some(off) = o.strip_prefix("carved at offset ") {
        let n = u64::from_str_radix(off.trim_start_matches("0x"), 16).unwrap_or(0);
        return format!(
            "Found by searching the raw data, {} from the start of the drive (its name was lost)",
            text::bytes(n)
        );
    }
    o.to_string()
}

type Loaded = Result<(Report, Vec<ResultItem>), String>;

struct DonorPicker {
    target: usize,
    candidates: Vec<usize>,
    file_task: Option<Task<Option<PathBuf>>>,
}

/// What the page asks the app to do.
pub enum Action {
    /// Repair `file` using `donor` as the reference; results go to `output`.
    Repair {
        /// The damaged file.
        file: PathBuf,
        /// The healthy reference.
        donor: PathBuf,
        /// Where the repaired copy goes.
        output: PathBuf,
    },
    /// Remember this results folder.
    Opened(PathBuf),
    /// Forget a recent folder.
    Forget(PathBuf),
}

/// The Results page.
pub struct Results {
    /// The folder shown.
    pub folder: Option<PathBuf>,
    /// The folder field, as typed.
    pub folder_text: String,
    /// Every file of the report.
    pub items: Vec<ResultItem>,
    /// The report.
    pub report: Option<Arc<Report>>,
    /// Why the last load failed.
    pub error: Option<String>,
    /// The filter.
    pub filter: Filter,
    /// The search text.
    pub search: String,
    /// The file shown in the detail panel (index into `items`).
    pub selected: Option<usize>,
    names: Vec<String>,
    counts: [usize; 6],
    /// The folder name shown in the title instead of the real one (for
    /// screenshots).
    #[doc(hidden)]
    pub shown_folder: Option<String>,
    loading: Option<(PathBuf, Task<Loaded>)>,
    picker: Option<Task<Option<PathBuf>>>,
    donor: Option<DonorPicker>,
    note: Option<String>,
}

impl Default for Results {
    fn default() -> Self {
        Results {
            folder: None,
            folder_text: String::new(),
            items: Vec::new(),
            report: None,
            error: None,
            filter: Filter::All,
            search: String::new(),
            selected: None,
            names: Vec::new(),
            counts: [0; 6],
            shown_folder: None,
            loading: None,
            picker: None,
            donor: None,
            note: None,
        }
    }
}

impl Results {
    /// Show a report that is already in memory.
    pub fn set_report(&mut self, folder: &Path, report: Arc<Report>) {
        self.items = results::items(&report, folder);
        self.names = self
            .items
            .iter()
            .map(|it| format!("{}\n{}", item_name(it), it.origin).to_lowercase())
            .collect();
        for (i, f) in Filter::ALL.iter().enumerate() {
            self.counts[i] = self
                .items
                .iter()
                .filter(|it| it.path.is_some() || it.repaired.is_some())
                .filter(|it| f.matches(it))
                .count();
        }
        self.report = Some(report);
        self.folder = Some(folder.to_path_buf());
        self.folder_text = folder.display().to_string();
        self.error = None;
        self.selected = None;
        self.filter = Filter::All;
        self.search.clear();
        self.donor = None;
    }

    /// Load the results folder `folder` in the background.
    pub fn load(&mut self, ctx: &egui::Context, folder: PathBuf) {
        self.folder_text = folder.display().to_string();
        let f = folder.clone();
        self.loading = Some((
            folder,
            Task::spawn(ctx, "diskrescue-results", move || {
                let r = results::load(&f).map_err(|e| e.message().to_string())?;
                let items = results::items(&r, &f);
                Ok((r, items))
            }),
        ));
    }

    /// True while a folder is being loaded.
    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// The items that pass the filter and the search, in report order.
    pub fn visible(&self) -> Vec<usize> {
        let q = self.search.trim().to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.path.is_some() || it.repaired.is_some())
            .filter(|(_, it)| self.filter.matches(it))
            .filter(|(i, _)| q.is_empty() || self.names.get(*i).is_some_and(|n| n.contains(&q)))
            .map(|(i, _)| i)
            .collect()
    }

    fn count(&self, f: Filter) -> usize {
        Filter::ALL
            .iter()
            .position(|x| *x == f)
            .map(|i| self.counts[i])
            .unwrap_or(0)
    }

    /// Take finished loads and dialogs. Returns the folder that was opened.
    pub fn update(&mut self) -> Option<Action> {
        let mut action = None;
        if let Some((folder, task)) = &mut self.loading {
            if let Some(r) = task.poll() {
                let folder = folder.clone();
                self.loading = None;
                match r {
                    Ok((report, _items)) => {
                        self.set_report(&folder, Arc::new(report));
                        action = Some(Action::Opened(folder));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if let Some(t) = &mut self.picker {
            if let Some(r) = t.poll() {
                self.picker = None;
                if let Some(p) = r {
                    self.folder_text = p.display().to_string();
                }
            }
        }
        action
    }

    /// Draw the page.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        thumbs: &mut Thumbnails,
        recent: &[PathBuf],
        is_root: bool,
    ) -> Option<Action> {
        let mut action = None;
        if self.report.is_none() {
            return self.chooser(ui, recent);
        }
        let folder = self.folder.clone().unwrap_or_default();

        if self.selected.is_some() {
            egui::Panel::right("details")
                .resizable(true)
                .default_size(360.0)
                .min_size(300.0)
                .frame(
                    egui::Frame::new()
                        .fill(theme::card_fill(ui.visuals()))
                        .inner_margin(egui::Margin::same(16)),
                )
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if let Some(a) = self.details(ui, thumbs, &folder, is_root) {
                                action = Some(a);
                            }
                        });
                });
        }

        let shown = self
            .shown_folder
            .clone()
            .unwrap_or_else(|| folder.display().to_string());
        widgets::page_title(ui, "Results", &shown);
        ui.horizontal_wrapped(|ui| {
            if !is_root && ui.button("Open the folder").clicked() {
                if let Err(e) = system::open_path(&folder) {
                    self.note = Some(e);
                }
            }
            if ui.button("Open another folder").clicked() {
                self.report = None;
                self.items.clear();
                self.selected = None;
            }
        });
        if let Some(n) = &self.note {
            widgets::callout_text(ui, Kind::Warn, "", n);
        }
        self.summary_line(ui);

        ui.horizontal_wrapped(|ui| {
            for f in Filter::ALL {
                let label = format!("{} ({})", f.label(), self.count(f));
                if ui.selectable_label(self.filter == f, label).clicked() {
                    self.filter = f;
                }
            }
            ui.add_space(12.0);
            let l = ui.label("Search");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("File name")
                    .desired_width(200.0),
            )
            .labelled_by(l.id);
        });
        ui.add_space(4.0);

        let visible = self.visible();
        if visible.is_empty() {
            widgets::weak(ui, "No files match.");
            return action;
        }
        let side = 132.0;
        let cell_w = side + 12.0;
        let cell_h = side + 22.0 + 12.0;
        let cols = ((ui.available_width() + 12.0) / cell_w).floor().max(1.0) as usize;
        let rows = visible.len().div_ceil(cols);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, cell_h, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        for &i in visible.iter().skip(row * cols).take(cols) {
                            let it = &self.items[i];
                            let Some(path) = shown_path(it) else { continue };
                            let path = path.to_path_buf();
                            let name = item_name(it);
                            let kind = Kind::from(text::outcome_tone(it.outcome));
                            let dot = (kind != Kind::Good).then_some(kind);
                            let r = tile(ui, thumbs, &path, it.category, side, &name, dot);
                            let r = if self.selected == Some(i) {
                                ui.painter().rect_stroke(
                                    r.rect.expand(3.0),
                                    8.0,
                                    egui::Stroke::new(2.5, theme::accent(ui.visuals())),
                                    egui::StrokeKind::Outside,
                                );
                                r
                            } else {
                                r
                            };
                            if r.clicked() {
                                self.selected = Some(i);
                            }
                        }
                    });
                }
            });

        if let Some(a) = self.donor_modal(ui, thumbs, &folder) {
            action = Some(a);
        }
        action
    }

    fn summary_line(&self, ui: &mut Ui) {
        let Some(r) = &self.report else { return };
        let t = &r.totals;
        let mut parts = vec![text::plural(
            t.files_written,
            "file recovered",
            "files recovered",
        )];
        if t.deleted > 0 {
            parts.push(format!("{} deleted", text::count(t.deleted)));
        }
        if t.carved > 0 {
            parts.push(format!("{} found in the raw data", text::count(t.carved)));
        }
        let dup = self
            .items
            .iter()
            .filter(|i| i.outcome == Outcome::Duplicate)
            .count();
        if dup > 0 {
            parts.push(format!("{} duplicates skipped", text::count(dup as u64)));
        }
        let failed = self
            .items
            .iter()
            .filter(|i| i.outcome == Outcome::Failed)
            .count();
        if failed > 0 {
            parts.push(format!(
                "{} could not be recovered",
                text::count(failed as u64)
            ));
        }
        if r.interrupted {
            parts.push("the rescue was stopped early".into());
        }
        widgets::weak(ui, parts.join("  ·  "));
    }

    fn chooser(&mut self, ui: &mut Ui, recent: &[PathBuf]) -> Option<Action> {
        let mut action = None;
        widgets::page_title(
            ui,
            "Results",
            "Look through the files of a rescue, here or later.",
        );
        if let Some(e) = &self.error {
            widgets::callout_text(ui, Kind::Bad, "That folder cannot be opened", e);
        }
        widgets::card(ui, |ui| {
            let f = widgets::path_field(
                ui,
                "Results folder",
                &mut self.folder_text,
                "The folder you saved a rescue to",
                "Choose folder...",
                self.picker.is_none(),
            );
            if f.browse && self.picker.is_none() {
                let start = system::home_dir();
                self.picker = Some(Task::spawn(ui.ctx(), "diskrescue-dialog", move || {
                    rfd::FileDialog::new()
                        .set_title("Choose the folder of a rescue")
                        .set_directory(start)
                        .pick_folder()
                }));
            }
            let ok = !self.folder_text.trim().is_empty() && self.loading.is_none();
            ui.horizontal(|ui| {
                if widgets::primary_button(ui, ok, "Open").clicked() {
                    let p = crate::wizard::expand(&self.folder_text);
                    self.error = None;
                    self.load(ui.ctx(), p);
                }
                if self.loading.is_some() {
                    ui.spinner();
                    ui.label("Opening...");
                }
            });
        });
        if !recent.is_empty() {
            widgets::section(ui, "Recent rescues");
            for p in recent {
                ui.horizontal(|ui| {
                    let r = ui.add_enabled(
                        self.loading.is_none(),
                        egui::Button::new(p.display().to_string()),
                    );
                    if r.clicked() {
                        self.error = None;
                        self.load(ui.ctx(), p.clone());
                    }
                    let f = ui.small_button("Forget");
                    let f = widgets::accessible(
                        f,
                        WidgetType::Button,
                        &format!("Forget {}", p.display()),
                    );
                    if f.clicked() {
                        action = Some(Action::Forget(p.clone()));
                    }
                });
            }
        }
        action
    }

    fn details(
        &mut self,
        ui: &mut Ui,
        thumbs: &mut Thumbnails,
        folder: &Path,
        is_root: bool,
    ) -> Option<Action> {
        let i = self.selected?;
        let it = self.items.get(i)?.clone();
        let action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(item_name(&it)).strong().size(17.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close details").clicked() {
                    self.selected = None;
                }
            });
        });
        let path = shown_path(&it).map(Path::to_path_buf);
        let w = ui.available_width();
        let preview_h = (w * 0.75).min(320.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, preview_h), egui::Sense::hover());
        let name = item_name(&it);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(WidgetType::Image, true, format!("Preview of {name}"))
        });
        ui.painter()
            .rect_filled(rect, 8.0, theme::palette(ui.visuals(), Kind::Neutral).bg);
        let thumb = match (&path, it.category) {
            (Some(p), MediaCategory::Photo | MediaCategory::RawPhoto) => thumbs.get(p, 640),
            _ => Thumb::None,
        };
        match thumb {
            Thumb::Ready(t) => {
                let ts = t.size_vec2();
                let scale = (rect.width() / ts.x).min(rect.height() / ts.y);
                let r = egui::Rect::from_center_size(rect.center(), ts * scale);
                ui.painter().image(
                    t.id(),
                    r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            Thumb::Loading => {
                egui::Spinner::new().paint_at(
                    ui,
                    egui::Rect::from_center_size(rect.center(), vec2(24.0, 24.0)),
                );
            }
            Thumb::None => {
                let c = theme::palette(ui.visuals(), Kind::Neutral).fg;
                crate::icons::file(
                    ui.painter(),
                    egui::Rect::from_center_size(
                        rect.center(),
                        vec2(preview_h * 0.5, preview_h * 0.5),
                    ),
                    it.category,
                    c,
                );
            }
        }

        ui.horizontal_wrapped(|ui| {
            let kind = Kind::from(text::outcome_tone(it.outcome));
            widgets::badge(ui, &status_label(it.outcome), kind);
            if it.deleted {
                widgets::badge(ui, "Deleted", Kind::Info);
            }
            if it.unreadable_bytes > 0 {
                widgets::badge(ui, "Has unreadable parts", Kind::Warn);
            }
        });

        let m = &it.meta;
        let mut rows: Vec<(&str, String)> = Vec::new();
        if it.captured.is_some() {
            rows.push(("Taken", text::date_text(it.captured)));
        }
        let camera = match (&m.make, &m.model) {
            (Some(a), Some(b)) if b.to_lowercase().starts_with(&a.to_lowercase()) => {
                Some(b.clone())
            }
            (Some(a), Some(b)) => Some(format!("{a} {b}")),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            _ => None,
        };
        if let Some(c) = camera {
            rows.push(("Camera", c));
        }
        if let (Some(w), Some(h)) = (m.width, m.height) {
            rows.push(("Size", format!("{w} × {h} pixels")));
        }
        if let Some(d) = m.duration_ms {
            rows.push((
                "Length",
                text::duration(std::time::Duration::from_millis(d)),
            ));
        }
        rows.push(("File size", text::bytes(it.size)));
        rows.push(("Found", origin_text(&it)));
        if it.unreadable_bytes > 0 {
            rows.push((
                "Unreadable",
                format!(
                    "{} could not be read and is blank",
                    text::bytes(it.unreadable_bytes)
                ),
            ));
        }
        if let Some(p) = &it.path {
            rows.push((
                "Saved as",
                p.strip_prefix(folder).unwrap_or(p).display().to_string(),
            ));
        }
        if let Some(p) = &it.repaired {
            rows.push((
                "Repaired copy",
                p.strip_prefix(folder).unwrap_or(p).display().to_string(),
            ));
        }
        widgets::facts(ui, "details_facts", &rows);

        if !it.problems.is_empty() {
            widgets::section(ui, "Problems");
            for p in &it.problems {
                ui.label(format!("• {}", widgets::gui_text(p)));
            }
        }
        if !it.notes.is_empty() {
            widgets::section(ui, "Repair and notes");
            for n in &it.notes {
                ui.label(format!("• {}", widgets::gui_text(n)));
            }
        }

        ui.add_space(8.0);
        if !is_root {
            ui.horizontal_wrapped(|ui| {
                if let Some(p) = &it.path {
                    if ui.button("Open").clicked() {
                        if let Err(e) = system::open_path(p) {
                            self.note = Some(e);
                        }
                    }
                    if ui.button("Show in folder").clicked() {
                        if let Err(e) = system::show_in_folder(p) {
                            self.note = Some(e);
                        }
                    }
                }
                if let Some(p) = &it.repaired {
                    if ui.button("Open repaired copy").clicked() {
                        if let Err(e) = system::open_path(p) {
                            self.note = Some(e);
                        }
                    }
                }
            });
        }
        let damaged = matches!(it.outcome, Outcome::Damaged | Outcome::PartiallyRepaired);
        if damaged && it.path.is_some() {
            ui.add_space(4.0);
            widgets::weak(
                ui,
                "A healthy file from the same camera or phone can help rebuild this one.",
            );
            if widgets::button(ui, self.donor.is_none(), "Repair with a reference file...")
                .clicked()
            {
                let candidates = self.donor_candidates(i);
                self.donor = Some(DonorPicker {
                    target: i,
                    candidates,
                    file_task: None,
                });
            }
        }
        action
    }

    /// Healthy files of the same format, best matches first (same camera,
    /// same size).
    fn donor_candidates(&self, target: usize) -> Vec<usize> {
        let t = &self.items[target];
        let mut v: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(i, it)| {
                *i != target
                    && it.path.is_some()
                    && matches!(
                        it.outcome,
                        Outcome::Recovered | Outcome::RecoveredUnverified
                    )
                    && it.format.is_some()
                    && it.format == t.format
            })
            .map(|(i, it)| {
                let mut score = 0;
                if it.meta.model.is_some() && it.meta.model == t.meta.model {
                    score += 4;
                }
                if it.meta.make.is_some() && it.meta.make == t.meta.make {
                    score += 2;
                }
                if it.meta.width.is_some()
                    && it.meta.width == t.meta.width
                    && it.meta.height == t.meta.height
                {
                    score += 3;
                }
                if it.meta.video_codec.is_some() && it.meta.video_codec == t.meta.video_codec {
                    score += 1;
                }
                (score, i)
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.into_iter().map(|(_, i)| i).take(40).collect()
    }

    fn donor_modal(
        &mut self,
        ui: &mut Ui,
        thumbs: &mut Thumbnails,
        folder: &Path,
    ) -> Option<Action> {
        let picker = self.donor.as_mut()?;
        let target = picker.target;
        if let Some(t) = &mut picker.file_task {
            if let Some(r) = t.poll() {
                picker.file_task = None;
                if let Some(donor) = r {
                    let action = self.repair_action(target, donor, folder);
                    self.donor = None;
                    return action;
                }
            }
        }
        let picker = self.donor.as_ref()?;
        let candidates = picker.candidates.clone();
        let busy = picker.file_task.is_some();
        let mut chosen: Option<PathBuf> = None;
        let mut close = false;
        let mut browse = false;
        let target_item = self.items[target].clone();
        egui::Modal::new(egui::Id::new("donor_picker")).show(ui.ctx(), |ui| {
            ui.set_width(520.0);
            ui.heading("Repair with a reference file");
            ui.label(format!(
                "Pick a healthy file taken with the same camera or phone as {}, at the same size and \
                 settings. DiskRescue copies what is missing from the damaged file (such as its header \
                 or index) from the reference. Neither file is changed; the repaired copy is saved \
                 separately.",
                item_name(&target_item)
            ));
            ui.add_space(4.0);
            if candidates.is_empty() {
                widgets::weak(ui, "No healthy file of the same kind was found in these results.");
            } else {
                ui.label(RichText::new("Healthy files of the same kind, best matches first").strong());
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for &c in &candidates {
                        let it = &self.items[c];
                        let Some(p) = it.path.clone() else { continue };
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), egui::Sense::hover());
                            match thumbs.get(&p, 96) {
                                Thumb::Ready(t) => {
                                    let ts = t.size_vec2();
                                    let s = (48.0 / ts.x).min(48.0 / ts.y);
                                    ui.painter().image(
                                        t.id(),
                                        egui::Rect::from_center_size(r.center(), ts * s),
                                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                        egui::Color32::WHITE,
                                    );
                                }
                                _ => crate::icons::file(
                                    ui.painter(),
                                    r,
                                    it.category,
                                    theme::palette(ui.visuals(), Kind::Neutral).fg,
                                ),
                            }
                            ui.vertical(|ui| {
                                ui.label(item_name(it));
                                let mut hints = Vec::new();
                                if it.meta.model.is_some() && it.meta.model == target_item.meta.model {
                                    hints.push("same camera");
                                }
                                if it.meta.width.is_some()
                                    && it.meta.width == target_item.meta.width
                                    && it.meta.height == target_item.meta.height
                                {
                                    hints.push("same size");
                                }
                                if !hints.is_empty() {
                                    widgets::weak(ui, hints.join(", "));
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let b = ui.button("Use this file");
                                let b = widgets::accessible(
                                    b,
                                    WidgetType::Button,
                                    &format!("Use {} as the reference", item_name(it)),
                                );
                                if b.clicked() {
                                    chosen = Some(p.clone());
                                }
                            });
                        });
                    }
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("Choose another file...")).clicked() {
                    browse = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if browse {
            let start = system::home_dir();
            if let Some(p) = self.donor.as_mut() {
                p.file_task = Some(Task::spawn(ui.ctx(), "diskrescue-dialog", move || {
                    rfd::FileDialog::new()
                        .set_title("Choose a healthy photo or video from the same camera or phone")
                        .set_directory(start)
                        .pick_file()
                }));
            }
        }
        if close {
            self.donor = None;
            return None;
        }
        if let Some(donor) = chosen {
            self.donor = None;
            return self.repair_action(target, donor, folder);
        }
        None
    }

    fn repair_action(&self, target: usize, donor: PathBuf, folder: &Path) -> Option<Action> {
        let file = self.items.get(target)?.path.clone()?;
        let stem = file
            .file_stem()
            .map(|s| system::safe_name(&s.to_string_lossy()))
            .unwrap_or_else(|| "file".into());
        let output = crate::wizard::free_folder(&folder.join("repairs").join(stem));
        Some(Action::Repair {
            file,
            donor,
            output,
        })
    }
}

/// An outcome in the words of the results page.
pub fn status_label(o: Outcome) -> String {
    match o {
        Outcome::Recovered => "Recovered, intact".into(),
        Outcome::RecoveredUnverified => "Recovered".into(),
        Outcome::Damaged => "Damaged".into(),
        Outcome::Repaired => "Repaired".into(),
        Outcome::PartiallyRepaired => "Partly repaired".into(),
        Outcome::Duplicate => "Duplicate".into(),
        Outcome::Failed => "Could not be recovered".into(),
    }
}
