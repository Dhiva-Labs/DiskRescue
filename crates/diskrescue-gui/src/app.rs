//! The application: navigation, top bar, pages, the running rescue, and
//! what happens when the window is closed while it runs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use diskrescue_app::helper;
use diskrescue_app::jobs::start_recover;
use egui::{vec2, Align, Color32, CornerRadius, Layout, Margin, RichText, Sense, Ui};
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, SystemBackend};
use crate::drives::{self, Drives};
use crate::image_page::{self, ImagePage};
use crate::progress::{self, ActiveJob, JobKind, MapCache};
use crate::repair::{self, RepairPage};
use crate::results::{self, Results};
use crate::theme::{self, Kind};
use crate::thumbs::Thumbnails;
use crate::wizard::{self, Source, Wizard};
use crate::{help, icons, system, widgets};

/// What is kept between runs (in the app's configuration folder, through
/// eframe's persistence).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Saved {
    /// Results folders opened recently, newest first.
    pub recent_results: Vec<PathBuf>,
}

impl Saved {
    /// Put `p` first in the recent list.
    pub fn remember(&mut self, p: PathBuf) {
        self.recent_results.retain(|x| x != &p);
        self.recent_results.insert(0, p);
        self.recent_results.truncate(8);
    }
}

/// The pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Drive list.
    Drives,
    /// Choose an image file.
    ImageFile,
    /// The rescue wizard.
    Wizard,
    /// The running rescue.
    Progress,
    /// Results.
    Results,
    /// Repair loose files.
    Repair,
    /// Help.
    Help,
}

const NAV: [(Page, &str); 5] = [
    (Page::Drives, "Rescue from a drive"),
    (Page::ImageFile, "Recover from an image file"),
    (Page::Repair, "Repair photos & videos"),
    (Page::Results, "Results"),
    (Page::Help, "Help"),
];

#[derive(Default)]
struct Closing {
    asking: bool,
    stopping: bool,
    allowed: bool,
}

/// The whole app. All state lives here, so switching pages loses nothing.
pub struct DiskRescueApp {
    backend: Arc<dyn Backend>,
    /// The page shown.
    pub page: Page,
    /// The drive list.
    pub drives: Drives,
    /// The image file page.
    pub image_page: ImagePage,
    /// The wizard (kept while its rescue runs).
    pub wizard: Option<Wizard>,
    /// The rescue that runs or ran.
    pub job: Option<ActiveJob>,
    /// The results page.
    pub results: Results,
    /// The repair page.
    pub repair: RepairPage,
    /// What is kept between runs.
    pub saved: Saved,
    thumbs: Option<Thumbnails>,
    map_cache: MapCache,
    closing: Closing,
    is_root: bool,
    styled: bool,
}

impl DiskRescueApp {
    /// The app with the real system behind it.
    pub fn new(saved: Saved) -> Self {
        Self::with_backend(Arc::new(SystemBackend), saved)
    }

    /// The app with `backend` (tests use made-up drives).
    pub fn with_backend(backend: Arc<dyn Backend>, saved: Saved) -> Self {
        DiskRescueApp {
            backend,
            page: Page::Drives,
            drives: Drives::default(),
            image_page: ImagePage::default(),
            wizard: None,
            job: None,
            results: Results::default(),
            repair: RepairPage::default(),
            saved,
            thumbs: None,
            map_cache: MapCache::default(),
            closing: Closing::default(),
            is_root: system::is_root(),
            styled: false,
        }
    }

    /// True while a rescue or a repair runs.
    pub fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.is_running()) || self.repair.running()
    }

    /// True while thumbnails are being made.
    pub fn thumbnails_busy(&self) -> bool {
        self.thumbs.as_ref().is_some_and(|t| t.busy())
    }

    fn rescue_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.is_running())
    }

    /// Show a fixed progress state (for screenshots).
    #[doc(hidden)]
    pub fn show_preview_progress(&mut self, title: &str, snapshot: progress::Snapshot) {
        self.job = Some(ActiveJob {
            title: title.to_string(),
            output: PathBuf::from("/media/backup/SanDisk SDXC Card - recovered"),
            image_path: Some(PathBuf::from("/media/backup/SanDisk SDXC Card.img")),
            kind: JobKind::Preview(Box::new(snapshot)),
            confirm_stop: false,
            results_taken: false,
        });
        self.page = Page::Progress;
    }

    /// Draw one frame. eframe calls this through [`eframe::App::ui`]; the
    /// tests call it directly.
    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if !self.styled {
            theme::apply(&ctx);
            self.styled = true;
        }
        let thumbs = self.thumbs.get_or_insert_with(|| Thumbnails::new(&ctx));
        thumbs.poll(&ctx);

        self.handle_dropped_files(&ctx);
        self.update_background(&ctx);
        self.handle_close(&ctx);

        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(250.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::nav_fill(ui.visuals()))
                    .inner_margin(Margin::symmetric(14, 18)),
            )
            .show(ui, |ui| self.nav(ui));

        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(theme::page_fill(ui.visuals()))
                    .inner_margin(Margin::symmetric(24, 10)),
            )
            .show(ui, |ui| self.top_bar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::page_fill(ui.visuals()))
                    .inner_margin(Margin {
                        left: 28,
                        right: 28,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ui, |ui| self.page_ui(ui));

        self.close_dialogs(&ctx);
    }

    fn update_background(&mut self, ctx: &egui::Context) {
        self.drives
            .update(ctx, &self.backend, self.page == Page::Drives);
        self.image_page.update(ctx);
        if let Some(w) = &mut self.wizard {
            if self.page == Page::Wizard {
                w.update(ctx, &self.backend);
            }
        }
        if let Some(results::Action::Opened(p)) = self.results.update() {
            self.saved.remember(p);
        }
        self.repair.update(ctx);
        if let Some(job) = &mut self.job {
            job.tick();
            if job.is_running() {
                ctx.request_repaint_after(Duration::from_millis(100));
            } else if !job.results_taken {
                job.results_taken = true;
                let snap = job.snapshot();
                if let Some(r) = snap.report() {
                    self.results.set_report(&job.output, r.clone());
                    self.saved.remember(job.output.clone());
                }
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = repair::dropped_paths(ctx);
        if dropped.is_empty() {
            return;
        }
        match self.page {
            Page::ImageFile => {
                if let Some(p) = dropped.first() {
                    self.image_page.path = p.display().to_string();
                }
            }
            _ if dropped[0].is_dir() => {
                // A folder: the results of an earlier rescue.
                self.results.load(ctx, dropped[0].clone());
                self.page = Page::Results;
            }
            _ if dropped.len() == 1 && repair::looks_like_image(&dropped[0]) => {
                self.image_page.path = dropped[0].display().to_string();
                self.page = Page::ImageFile;
            }
            _ => {
                self.repair.add_files(dropped);
                self.page = Page::Repair;
            }
        }
    }

    fn nav(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
            icons::drive(
                ui.painter(),
                r,
                icons::DriveIcon::SdCard,
                theme::accent(ui.visuals()),
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("DiskRescue").strong().size(20.0));
                ui.label(
                    RichText::new("Get your photos back")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            });
        });
        ui.add_space(18.0);
        let highlighted = match self.page {
            Page::Wizard => match self.wizard.as_ref().map(|w| &w.source) {
                Some(Source::Image(_)) => Page::ImageFile,
                _ => Page::Drives,
            },
            p => p,
        };
        for (page, label) in NAV {
            let selected = highlighted == page;
            let accent = theme::accent(ui.visuals());
            let text = if selected {
                RichText::new(label).strong().color(theme::on_accent())
            } else {
                RichText::new(label)
            };
            let mut b = egui::Button::new(text)
                .min_size(vec2(ui.available_width(), 38.0))
                .corner_radius(CornerRadius::same(8))
                .frame_when_inactive(selected);
            if selected {
                b = b.fill(accent);
            }
            let r = ui.add(b);
            if r.clicked() {
                self.go(page);
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "DiskRescue only reads the drive you rescue. It never writes to it.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                )
                .wrap(),
            );
        });
    }

    /// Switch to `page` (the wizard keeps its state and can be returned to).
    pub fn go(&mut self, page: Page) {
        self.page = page;
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            let job = self.job.as_ref();
            match job {
                Some(j) => {
                    let snap = j.snapshot();
                    let (status, frac) = snap.short_status();
                    let running = j.is_running();
                    let kind = match &snap.outcome {
                        None => Kind::Info,
                        Some(diskrescue_app::JobOutcome::Finished) => Kind::Good,
                        Some(diskrescue_app::JobOutcome::Cancelled) => Kind::Warn,
                        Some(_) => Kind::Bad,
                    };
                    let pal = theme::palette(ui.visuals(), kind);
                    egui::Frame::new()
                        .fill(pal.bg)
                        .stroke(egui::Stroke::new(1.0, pal.border))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(12, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if running {
                                    ui.spinner();
                                } else {
                                    let (r, _) =
                                        ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                                    icons::kind_symbol(ui.painter(), r.center(), 8.0, kind, pal.fg);
                                }
                                ui.label(RichText::new(&j.title).strong());
                                ui.label(RichText::new(status).color(pal.fg));
                                if let Some(f) = frac {
                                    ui.add(
                                        egui::ProgressBar::new(f)
                                            .desired_width(140.0)
                                            .desired_height(8.0),
                                    );
                                }
                                if self.page != Page::Progress {
                                    let label = if running {
                                        "View progress"
                                    } else {
                                        "See details"
                                    };
                                    if ui.button(label).clicked() {
                                        self.page = Page::Progress;
                                    }
                                }
                            });
                        });
                }
                None => {
                    widgets::weak(ui, "Nothing is running.");
                }
            }
            if self.repair.running() {
                ui.add_space(8.0);
                ui.spinner();
                ui.label("Repairing files...");
                if self.page != Page::Repair && ui.button("View").clicked() {
                    self.page = Page::Repair;
                }
            }
        });
    }

    fn page_ui(&mut self, ui: &mut Ui) {
        let orphan = (self.page == Page::Wizard && self.wizard.is_none())
            || (self.page == Page::Progress && self.job.is_none());
        let page = if orphan { Page::Drives } else { self.page };
        match page {
            Page::Drives => {
                let mut action = None;
                let running = self.rescue_running();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(w) = &self.wizard {
                            if w.source.is_device() && !running {
                                ui.horizontal(|ui| {
                                    widgets::weak(
                                        ui,
                                        format!("You were setting up: {}.", w.source.name()),
                                    );
                                    if ui.button("Return to it").clicked() {
                                        self.page = Page::Wizard;
                                    }
                                });
                            }
                        }
                        action = drives::show(ui, &self.drives, running);
                    });
                match action {
                    Some(drives::Action::Rescue(e)) => {
                        self.wizard = Some(Wizard::for_device(*e));
                        self.page = Page::Wizard;
                    }
                    Some(drives::Action::Refresh) => {
                        let ctx = ui.ctx().clone();
                        self.drives.refresh(&ctx, &self.backend);
                    }
                    Some(drives::Action::ImageFile) => self.page = Page::ImageFile,
                    None => {}
                }
            }
            Page::ImageFile => {
                let mut action = None;
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(w) = &self.wizard {
                            if matches!(w.source, Source::Image(_)) && !self.rescue_running() {
                                ui.horizontal(|ui| {
                                    widgets::weak(
                                        ui,
                                        "You have a rescue of an image file in progress.",
                                    );
                                    if ui.button("Return to it").clicked() {
                                        self.page = Page::Wizard;
                                    }
                                });
                            }
                        }
                        action = self.image_page.show(ui);
                    });
                if let Some(image_page::Action::Continue(p)) = action {
                    self.wizard = Some(Wizard::for_image(p));
                    self.page = Page::Wizard;
                }
            }
            Page::Wizard => {
                let running = self.rescue_running();
                let Some(w) = &mut self.wizard else { return };
                match w.show(ui, running) {
                    Some(wizard::Action::Cancel) => {
                        let back = if w.source.is_device() {
                            Page::Drives
                        } else {
                            Page::ImageFile
                        };
                        self.wizard = None;
                        self.page = back;
                    }
                    Some(wizard::Action::Start(plan)) => self.start(*plan),
                    Some(wizard::Action::StartPrivileged { image, then }) => {
                        self.start_privileged(*image, *then)
                    }
                    None => {}
                }
            }
            Page::Progress => {
                let mut action = None;
                let thumbs = self.thumbs.as_mut().expect("created in ui()");
                if let Some(job) = &mut self.job {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            action =
                                progress::show(ui, job, thumbs, &mut self.map_cache, self.is_root);
                        });
                }
                match action {
                    Some(progress::Action::SeeResults) => {
                        if let Some(job) = &self.job {
                            if let Some(r) = job.snapshot().report() {
                                self.results.set_report(&job.output, r.clone());
                                self.saved.remember(job.output.clone());
                            }
                        }
                        self.page = Page::Results;
                    }
                    Some(progress::Action::BackToSettings) => {
                        if let Some(w) = &mut self.wizard {
                            w.step = 2;
                            self.page = Page::Wizard;
                        } else {
                            self.page = Page::Drives;
                        }
                    }
                    Some(progress::Action::NewRescue) => {
                        self.wizard = None;
                        self.page = Page::Drives;
                    }
                    Some(progress::Action::Open(p)) => {
                        if let Err(e) = system::open_path(&p) {
                            log::warn!("{e}");
                        }
                    }
                    None => {}
                }
            }
            Page::Results => {
                let thumbs = self.thumbs.as_mut().expect("created in ui()");
                let recent = self.saved.recent_results.clone();
                let action = self.results.show(ui, thumbs, &recent, self.is_root);
                match action {
                    Some(results::Action::Repair {
                        file,
                        donor,
                        output,
                    }) => {
                        if !self.repair.running() {
                            self.repair.files = vec![file.clone()];
                            self.repair.donors = vec![donor.clone()];
                            self.repair.output = output.display().to_string();
                            self.repair.start(vec![file], vec![donor], output);
                            self.page = Page::Repair;
                        }
                    }
                    Some(results::Action::Opened(p)) => self.saved.remember(p),
                    Some(results::Action::Forget(p)) => {
                        self.saved.recent_results.retain(|x| x != &p);
                    }
                    None => {}
                }
            }
            Page::Repair => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.repair.show(ui, self.is_root));
            }
            Page::Help => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, help::show);
            }
        }
    }

    fn job_title(&self) -> (String, Option<PathBuf>) {
        let Some(w) = &self.wizard else {
            return ("Rescue".into(), None);
        };
        let title = match &w.source {
            Source::Device(e) => format!("Rescue of {}", e.title),
            Source::Image(p) => format!("Recovery from {}", system::file_name(p)),
        };
        (title, w.plan().image_to)
    }

    fn start(&mut self, plan: diskrescue_app::plan::RecoverPlan) {
        let (title, image) = self.job_title();
        let output = plan.output.clone();
        let image_path = plan.image_to.clone().or(image);
        let handle = start_recover(plan);
        self.job = Some(ActiveJob {
            title,
            output,
            image_path,
            kind: JobKind::Recover(handle),
            confirm_stop: false,
            results_taken: false,
        });
        if let Some(w) = &mut self.wizard {
            w.start_error = None;
        }
        self.page = Page::Progress;
    }

    fn start_privileged(
        &mut self,
        image: diskrescue_app::plan::ImagePlan,
        then: diskrescue_app::plan::RecoverPlan,
    ) {
        let (title, _) = self.job_title();
        let image_path = image.image.clone();
        let output = then.output.clone();
        match helper::start_image_privileged(image) {
            Ok(h) => {
                self.job = Some(ActiveJob {
                    title,
                    output,
                    image_path: Some(image_path),
                    kind: JobKind::Privileged {
                        image: h,
                        then: Box::new(then),
                        recover: None,
                    },
                    confirm_stop: false,
                    results_taken: false,
                });
                if let Some(w) = &mut self.wizard {
                    w.start_error = None;
                }
                self.page = Page::Progress;
            }
            Err(e) => {
                if let Some(w) = &mut self.wizard {
                    w.start_error = Some(widgets::gui_text(e.message()));
                }
            }
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing.allowed && self.busy() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing.asking = true;
        }
        if self.closing.stopping {
            if self.busy() {
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                self.closing.stopping = false;
                self.closing.allowed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Stop everything and close once it is saved.
    pub fn stop_and_close(&mut self) {
        if let Some(j) = &self.job {
            j.cancel();
        }
        if let Some(j) = &self.repair.job {
            j.cancel();
        }
        self.closing.asking = false;
        self.closing.stopping = true;
    }

    fn close_dialogs(&mut self, ctx: &egui::Context) {
        if self.closing.asking {
            let mut keep = false;
            let mut stop = false;
            let m = egui::Modal::new(egui::Id::new("close_while_running")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.heading("A rescue is still running");
                ui.label(
                    "Stop it and close DiskRescue? Everything found so far is saved. A copy of a \
                     drive can be continued later by starting the same rescue again.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if widgets::button(ui, true, "Keep it running").clicked() {
                        keep = true;
                    }
                    if widgets::danger_button(ui, true, "Stop, save and close").clicked() {
                        stop = true;
                    }
                });
            });
            if keep || m.should_close() {
                self.closing.asking = false;
            }
            if stop {
                self.stop_and_close();
            }
        }
        if self.closing.stopping {
            egui::Modal::new(egui::Id::new("closing")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Saving and stopping...").strong());
                });
                ui.label("DiskRescue closes as soon as everything is saved.");
            });
        }
    }
}

/// The colour behind the window (matches the pages).
pub fn clear_color(v: &egui::Visuals) -> [f32; 4] {
    let c: Color32 = theme::page_fill(v);
    c.to_normalized_gamma_f32()
}

impl eframe::App for DiskRescueApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        DiskRescueApp::ui(self, ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.saved);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        clear_color(visuals)
    }
}
