//! The running rescue: copying the drive (with its block map) and then the
//! recovery stages, the files found so far, and the result.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_app::jobs::{start_recover, ImageState, ImageStatus, RecoverState, RecoverStatus};
use diskrescue_app::plan::RecoverPlan;
use diskrescue_app::{text, JobHandle, JobOutcome};
use diskrescue_core::media::MediaCategory;
use diskrescue_imaging::{BlockStatus, StatusMap};
use diskrescue_recover::report::{Outcome, Report};
use egui::{vec2, Color32, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui};

use crate::icons;
use crate::theme::{self, Kind};
use crate::thumbs::{Thumb, Thumbnails};
use crate::{system, widgets};

/// The kinds of rescue job.
pub enum JobKind {
    /// A recovery (imaging first when its plan says so), run by this
    /// process.
    Recover(JobHandle<RecoverState>),
    /// A copy made with administrator rights by the command-line program,
    /// followed by a recovery from the copy with normal rights.
    Privileged {
        /// The copy.
        image: JobHandle<ImageState>,
        /// The recovery to start once the copy is complete.
        then: Box<RecoverPlan>,
        /// The recovery, once started.
        recover: Option<JobHandle<RecoverState>>,
    },
    /// A fixed state, for previews and screenshots.
    #[doc(hidden)]
    Preview(Box<Snapshot>),
}

/// Everything the page shows, taken from the job once per frame.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// The copy, when the rescue copies the drive first.
    pub image: Option<ImageState>,
    /// The copy runs with administrator rights in another program.
    pub privileged: bool,
    /// The recovery (its own copy step included).
    pub recover: Option<RecoverState>,
    /// How the job ended.
    pub outcome: Option<JobOutcome>,
    /// Pause is possible now.
    pub can_pause: bool,
    /// Paused.
    pub paused: bool,
    /// A stop was requested.
    pub stopping: bool,
}

impl Snapshot {
    /// True while the drive is being copied.
    pub fn imaging(&self) -> bool {
        if self.outcome.is_some() {
            return false;
        }
        match &self.recover {
            Some(r) => {
                matches!(r.status, RecoverStatus::Checking | RecoverStatus::Imaging)
                    && self.image.is_some()
            }
            None => self.image.is_some(),
        }
    }

    /// The final report, when there is one.
    pub fn report(&self) -> Option<&Arc<Report>> {
        self.recover.as_ref().and_then(|r| r.report.as_ref())
    }

    /// A one-line status and a fraction for the top bar.
    pub fn short_status(&self) -> (String, Option<f32>) {
        if let Some(o) = &self.outcome {
            let t = match o {
                JobOutcome::Finished => "Rescue finished",
                JobOutcome::Cancelled => "Rescue stopped",
                JobOutcome::Refused(_) => "Rescue did not start",
                JobOutcome::Failed(_) => "Rescue stopped with a problem",
            };
            return (t.into(), None);
        }
        if self.imaging() {
            if let Some(st) = self.image.as_ref().and_then(|i| i.stats()) {
                return (
                    format!(
                        "Copying the drive: {}",
                        text::percent(st.rescued, st.total())
                    ),
                    Some(st.rescued_fraction() as f32),
                );
            }
            return ("Copying the drive".into(), None);
        }
        if let Some(r) = &self.recover {
            let p = &r.pipeline;
            if let Some(cur) = p.current() {
                return (cur.label.clone(), cur.fraction().map(|f| f as f32));
            }
            return ("Starting...".into(), None);
        }
        ("Starting...".into(), None)
    }
}

/// The rescue that runs (or ran).
pub struct ActiveJob {
    /// "Rescue SanDisk Ultra".
    pub title: String,
    /// The results folder.
    pub output: PathBuf,
    /// The image file, when the drive is copied first.
    pub image_path: Option<PathBuf>,
    /// The job.
    pub kind: JobKind,
    /// The stop confirmation is shown.
    pub confirm_stop: bool,
    /// The results were handed to the Results page.
    pub results_taken: bool,
}

impl ActiveJob {
    /// Start the next step of a chained job (the recovery after a
    /// privileged copy).
    pub fn tick(&mut self) {
        if let JobKind::Privileged {
            image,
            then,
            recover,
        } = &mut self.kind
        {
            if recover.is_none() && image.outcome() == Some(JobOutcome::Finished) {
                *recover = Some(start_recover((**then).clone()));
            }
        }
    }

    /// What to show now.
    pub fn snapshot(&self) -> Snapshot {
        match &self.kind {
            JobKind::Recover(h) => {
                let s = h.snapshot();
                let imaging = matches!(s.status, RecoverStatus::Checking | RecoverStatus::Imaging);
                Snapshot {
                    image: s.image.clone(),
                    privileged: false,
                    can_pause: h.can_pause() && imaging && s.image.is_some() && !h.is_finished(),
                    paused: h.is_paused(),
                    stopping: h.is_cancelled(),
                    outcome: h.outcome(),
                    recover: Some(s),
                }
            }
            JobKind::Privileged { image, recover, .. } => {
                let outcome = match recover {
                    Some(r) => r.outcome(),
                    None => image.outcome().filter(|o| !o.is_finished()),
                };
                let stopping =
                    image.is_cancelled() || recover.as_ref().is_some_and(|r| r.is_cancelled());
                Snapshot {
                    image: Some(image.snapshot()),
                    privileged: true,
                    recover: recover.as_ref().map(|r| r.snapshot()),
                    outcome,
                    can_pause: false,
                    paused: false,
                    stopping,
                }
            }
            JobKind::Preview(s) => (**s).clone(),
        }
    }

    /// True until the job has ended.
    pub fn is_running(&self) -> bool {
        match &self.kind {
            JobKind::Recover(h) => !h.is_finished(),
            JobKind::Privileged { image, recover, .. } => match recover {
                Some(r) => !r.is_finished(),
                None => !image.is_finished() || image.outcome() == Some(JobOutcome::Finished),
            },
            JobKind::Preview(s) => s.outcome.is_none(),
        }
    }

    /// Ask the job to stop (it saves what it has).
    pub fn cancel(&self) {
        match &self.kind {
            JobKind::Recover(h) => h.cancel(),
            JobKind::Privileged { image, recover, .. } => {
                image.cancel();
                if let Some(r) = recover {
                    r.cancel();
                }
            }
            JobKind::Preview(_) => {}
        }
    }

    /// Pause or resume the copy.
    pub fn pause(&self, paused: bool) {
        if let JobKind::Recover(h) = &self.kind {
            h.pause(paused);
        }
    }
}

/// What the user asked for on the page.
pub enum Action {
    /// Show the results.
    SeeResults,
    /// Back to the wizard (after a refusal or a failure).
    BackToSettings,
    /// Start a new rescue.
    NewRescue,
    /// Open a folder in the file manager.
    Open(PathBuf),
}

/// Cached cells of the block map.
#[derive(Default)]
pub struct MapCache {
    key: Option<(usize, usize, usize, u64)>,
    cells: Vec<Option<BlockStatus>>,
}

fn compute_cells(map: &StatusMap, n: usize) -> Vec<Option<BlockStatus>> {
    let mut out: Vec<Option<BlockStatus>> = vec![None; n];
    let size = map.size();
    if size == 0 || n == 0 {
        return out;
    }
    for b in map.iter() {
        if b.size == 0 {
            continue;
        }
        let first = (u128::from(b.pos) * n as u128 / u128::from(size)) as usize;
        let last = (u128::from(b.pos + b.size - 1) * n as u128 / u128::from(size)) as usize;
        for c in out.iter_mut().take(last.min(n - 1) + 1).skip(first) {
            if c.is_none_or(|s| b.status.severity() > s.severity()) {
                *c = Some(b.status);
            }
        }
    }
    out
}

/// The block map: one cell per slice of the drive, coloured by the worst
/// status in it; the current position outlined.
pub fn block_map(ui: &mut Ui, map: &StatusMap, current: Option<u64>, cache: &mut MapCache) {
    let width = ui.available_width();
    let cell = 10.0;
    let cols = ((width + 2.0) / cell).floor().max(8.0) as usize;
    let rows = 18usize;
    let n = cols * rows;
    let key = (
        map as *const StatusMap as usize,
        n,
        map.len(),
        map.stats().rescued,
    );
    if cache.key != Some(key) {
        cache.cells = compute_cells(map, n);
        cache.key = Some(key);
    }
    let height = rows as f32 * cell;
    let (rect, resp) = ui.allocate_exact_size(vec2(cols as f32 * cell, height), Sense::hover());
    let st = map.stats();
    let label = format!(
        "Map of the drive: {} copied, {} unreadable",
        text::percent(st.rescued, st.total()),
        text::bytes(st.bad_sectors)
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, label.clone()));
    let colors = theme::map_colors(ui.visuals());
    let p = ui.painter_at(rect);
    for (i, c) in cache.cells.iter().enumerate() {
        let x = rect.left() + (i % cols) as f32 * cell;
        let y = rect.top() + (i / cols) as f32 * cell;
        let r = Rect::from_min_size(egui::pos2(x, y), vec2(cell - 2.0, cell - 2.0));
        let color = match c {
            Some(BlockStatus::Finished) => colors.rescued,
            Some(BlockStatus::NonTried) | None => colors.not_tried,
            Some(BlockStatus::NonTrimmed) | Some(BlockStatus::NonScraped) => colors.retrying,
            Some(BlockStatus::BadSector) => colors.bad,
        };
        p.rect_filled(r, 1.5, color);
    }
    if let Some(pos) = current {
        if map.size() > 0 {
            let i = ((u128::from(pos.min(map.size() - 1)) * n as u128) / u128::from(map.size()))
                as usize;
            let x = rect.left() + (i % cols) as f32 * cell;
            let y = rect.top() + (i / cols) as f32 * cell;
            let r = Rect::from_min_size(egui::pos2(x, y), vec2(cell - 2.0, cell - 2.0)).expand(1.5);
            p.rect_stroke(
                r,
                2.0,
                Stroke::new(2.0, colors.current),
                StrokeKind::Outside,
            );
        }
    }
}

fn legend(ui: &mut Ui, st: &diskrescue_imaging::Stats) {
    let colors = theme::map_colors(ui.visuals());
    ui.horizontal_wrapped(|ui| {
        let items = [
            (colors.rescued, "Copied", st.rescued),
            (colors.not_tried, "Not read yet", st.non_tried),
            (
                colors.retrying,
                "Being retried",
                st.non_trimmed + st.non_scraped,
            ),
            (colors.bad, "Unreadable", st.bad_sectors),
        ];
        for (c, name, bytes) in items {
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            ui.painter().rect_filled(r, 2.0, c);
            ui.label(format!("{name} {}", text::bytes(bytes)));
            ui.add_space(10.0);
        }
        let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
        ui.painter().rect_stroke(
            r.shrink(1.0),
            2.0,
            Stroke::new(2.0, colors.current),
            StrokeKind::Inside,
        );
        ui.label("Being read now");
    });
}

/// Draw the progress page.
pub fn show(
    ui: &mut Ui,
    job: &mut ActiveJob,
    thumbs: &mut Thumbnails,
    map_cache: &mut MapCache,
    is_root: bool,
) -> Option<Action> {
    let mut action = None;
    let snap = job.snapshot();
    widgets::page_title(
        ui,
        &job.title,
        &format!("Saving to {}", job.output.display()),
    );

    if let Some(o) = &snap.outcome {
        if let Some(a) = outcome_callout(ui, job, &snap, o, is_root) {
            action = Some(a);
        }
        ui.add_space(6.0);
    }

    let has_copy = snap.image.is_some();
    let steps = if has_copy { 2 } else { 1 };
    if let Some(img) = &snap.image {
        let copying = snap.imaging();
        let title = if snap.privileged {
            "Step 1 of 2: Copying the drive with administrator rights"
        } else {
            "Step 1 of 2: Copying the drive to an image file"
        };
        widgets::card(ui, |ui| {
            step_header(ui, title, step_state(copying, img_done(img, &snap), &snap));
            if copying
                || snap
                    .recover
                    .as_ref()
                    .is_none_or(|r| r.status == RecoverStatus::Done && r.report.is_none())
            {
                imaging_panel(ui, img, &snap, map_cache);
            } else {
                egui::CollapsingHeader::new("Show the copy and its map")
                    .id_salt("copy_details")
                    .show(ui, |ui| imaging_panel(ui, img, &snap, map_cache));
            }
        });
        ui.add_space(6.0);
    }

    let recovering_started = snap.recover.as_ref().is_some_and(|r| {
        !r.pipeline.stages.is_empty()
            || matches!(r.status, RecoverStatus::Opening | RecoverStatus::Recovering)
    });
    let title = if steps == 2 {
        "Step 2 of 2: Finding your photos and videos"
    } else {
        "Finding your photos and videos"
    };
    widgets::card(ui, |ui| {
        let state = if snap.outcome.is_some() && recovering_started {
            if matches!(snap.outcome, Some(JobOutcome::Finished)) {
                StepState::Done
            } else {
                StepState::Stopped
            }
        } else if recovering_started {
            StepState::Running
        } else if snap.outcome.is_some() {
            StepState::Stopped
        } else {
            StepState::Waiting
        };
        step_header(ui, title, state);
        if let Some(r) = &snap.recover {
            if recovering_started {
                recovery_panel(ui, r, &job.output, thumbs);
            } else if snap.outcome.is_none() && !snap.imaging() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Getting ready...");
                });
            } else if snap.imaging() {
                widgets::weak(ui, "Starts as soon as the copy is complete.");
            }
        } else if snap.outcome.is_none() {
            widgets::weak(ui, "Starts as soon as the copy is complete.");
        }
    });

    if snap.outcome.is_none() {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if snap.can_pause {
                let label = if snap.paused { "Resume" } else { "Pause" };
                if widgets::button(ui, !snap.stopping, label).clicked() {
                    job.pause(!snap.paused);
                }
            }
            let stop_label = if snap.stopping {
                "Stopping..."
            } else {
                "Stop..."
            };
            if widgets::danger_button(ui, !snap.stopping, stop_label).clicked() {
                job.confirm_stop = true;
            }
        });
        if snap.stopping {
            widgets::weak(
                ui,
                "Saving what was done so far and stopping. This can take a moment.",
            );
        }
    }

    if job.confirm_stop {
        confirm_stop(ui.ctx(), job, &snap);
    }
    action
}

fn img_done(img: &ImageState, snap: &Snapshot) -> bool {
    img.status == ImageStatus::Done
        && (img.summary.as_ref().is_some_and(|s| s.finished) || img.already_complete)
        && !matches!(snap.outcome, Some(JobOutcome::Refused(_)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepState {
    Waiting,
    Running,
    Done,
    Stopped,
}

fn step_state(copying: bool, done: bool, snap: &Snapshot) -> StepState {
    if done {
        StepState::Done
    } else if copying {
        StepState::Running
    } else if snap.outcome.is_some() {
        StepState::Stopped
    } else {
        StepState::Waiting
    }
}

fn step_header(ui: &mut Ui, title: &str, state: StepState) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
        let good = theme::palette(ui.visuals(), Kind::Good).fg;
        let warn = theme::palette(ui.visuals(), Kind::Warn).fg;
        let weak = ui.visuals().weak_text_color();
        match state {
            StepState::Done => icons::check(ui.painter(), r.center(), 11.0, good),
            StepState::Stopped => icons::cross(ui.painter(), r.center(), 11.0, warn),
            StepState::Waiting => icons::pending(ui.painter(), r.center(), 11.0, weak),
            StepState::Running => {
                egui::Spinner::new().size(20.0).paint_at(ui, r);
            }
        }
        ui.label(RichText::new(title).strong().size(17.0));
    });
}

fn imaging_panel(ui: &mut Ui, img: &ImageState, snap: &Snapshot, cache: &mut MapCache) {
    let stats = img.stats();
    if img.device_lost {
        widgets::callout_text(
            ui,
            Kind::Bad,
            "The drive disconnected",
            "The drive disconnected. DiskRescue is waiting for it to come back; try another port or \
             cable if it does not.",
        );
    }
    if snap.paused {
        widgets::callout_text(
            ui,
            Kind::Info,
            "Paused",
            "The drive is not being read. Press Resume to continue.",
        );
    }
    if let Some(d) = img.engine_pause {
        widgets::callout_text(
            ui,
            Kind::Info,
            "Giving the drive a short rest",
            &format!(
                "DiskRescue pauses for {} so that the drive can recover, then continues.",
                text::duration(d)
            ),
        );
    }
    if img.already_complete {
        widgets::weak(
            ui,
            "The copy was already complete; the drive was not read again.",
        );
    }
    match stats {
        None if snap.outcome.is_some() => {
            widgets::weak(ui, "The copy did not start.");
        }
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(match img.status {
                    ImageStatus::Checking => "Checking that everything is safe...",
                    _ if snap.privileged => "Waiting for administrator rights...",
                    _ => "Opening the drive...",
                });
            });
        }
        Some(st) => {
            let frac = st.rescued_fraction() as f32;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(text::percent(st.rescued, st.total()))
                        .strong()
                        .size(30.0),
                );
                ui.label(RichText::new("copied").size(17.0));
                if let Some(ph) = &img.phase {
                    ui.add_space(12.0);
                    widgets::weak(ui, format!("Now: {}", text::phase_label(ph)));
                }
            });
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width(ui.available_width())
                    .desired_height(10.0)
                    .fill(theme::map_colors(ui.visuals()).rescued),
            );
            let prog = img.progress.as_ref();
            let bad = st.errsize;
            let mut items = vec![
                widgets::Stat::new(
                    format!("copied, of {}", text::bytes(st.total())),
                    text::bytes(st.rescued),
                    None,
                ),
                widgets::Stat::new(
                    "could not be read (so far)",
                    text::bytes(bad),
                    Some(if bad > 0 { Kind::Bad } else { Kind::Good }),
                ),
            ];
            if let Some(p) = prog {
                let left = p
                    .eta
                    .map(text::duration)
                    .unwrap_or_else(|| "working it out".into());
                let errors = p.errors;
                items.extend([
                    widgets::Stat::new("speed", text::rate(p.current_rate), None),
                    widgets::Stat::new("time left in this pass", left, None),
                    widgets::Stat::new(
                        "read errors",
                        text::count(errors),
                        Some(if errors > 0 {
                            Kind::Warn
                        } else {
                            Kind::Neutral
                        }),
                    ),
                    widgets::Stat::new("running for", text::duration(p.elapsed), None),
                ]);
            }
            widgets::stats(ui, &items);
            if let Some(e) = prog.and_then(|p| p.last_error.as_ref()) {
                widgets::weak(ui, format!("Last read error: {e}"));
            }
            if let Some(map) = &img.map {
                ui.add_space(4.0);
                let current = if snap.imaging() {
                    prog.map(|p| p.current_pos)
                } else {
                    None
                };
                block_map(ui, map, current, cache);
                legend(ui, &map.stats());
            }
        }
    }
    for w in &img.warnings {
        widgets::callout_text(ui, Kind::Warn, "", &widgets::gui_text(w));
    }
    if !img.logs.is_empty() {
        egui::CollapsingHeader::new("Details")
            .id_salt("imaging_log")
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for l in img.logs.iter() {
                            ui.label(RichText::new(l.display()).monospace().small());
                        }
                    });
            });
    }
}

/// The category of a file, from its extension (for icons while the
/// thumbnail is unknown).
pub fn category_of(path: &Path) -> MediaCategory {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "gif" | "bmp" | "webp" | "tif" | "tiff"
        | "avif" | "psd" => MediaCategory::Photo,
        "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "pef" | "srw" | "crw" => {
            MediaCategory::RawPhoto
        }
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "mts" | "m2ts" | "ts" | "mpg" | "mpeg" | "vob"
        | "3gp" | "m4v" => MediaCategory::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" => MediaCategory::Audio,
        _ => MediaCategory::Other,
    }
}

/// A square tile with a thumbnail (or an icon) and a caption.
pub fn tile(
    ui: &mut Ui,
    thumbs: &mut Thumbnails,
    path: &Path,
    category: MediaCategory,
    side: f32,
    caption: &str,
    kind: Option<Kind>,
) -> egui::Response {
    let size = vec2(side, side + 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let name = caption.to_string();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name.clone()));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let img_rect = Rect::from_min_size(rect.min, vec2(side, side));
    let v = ui.visuals();
    ui.painter()
        .rect_filled(img_rect, 6.0, theme::palette(v, Kind::Neutral).bg);
    let wants_thumb = matches!(category, MediaCategory::Photo | MediaCategory::RawPhoto);
    let thumb = if wants_thumb {
        thumbs.get(path, (side * 2.0) as u32)
    } else {
        Thumb::None
    };
    let icon_color = theme::palette(ui.visuals(), Kind::Neutral).fg;
    match thumb {
        Thumb::Ready(t) => {
            let ts = t.size_vec2();
            let scale = (side / ts.x).min(side / ts.y);
            let r = Rect::from_center_size(img_rect.center(), ts * scale);
            ui.painter().image(
                t.id(),
                r,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        Thumb::Loading => {
            icons::file(
                ui.painter(),
                img_rect.shrink(side * 0.25),
                category,
                icon_color.gamma_multiply(0.5),
            );
        }
        Thumb::None => {
            icons::file(
                ui.painter(),
                img_rect.shrink(side * 0.22),
                category,
                icon_color,
            );
        }
    }
    if let Some(k) = kind {
        let c = theme::palette(ui.visuals(), k).fg;
        ui.painter()
            .circle_filled(img_rect.right_top() + vec2(-9.0, 9.0), 6.0, c);
    }
    if resp.hovered() {
        ui.painter().rect_stroke(
            img_rect,
            6.0,
            Stroke::new(2.0, theme::accent(ui.visuals())),
            StrokeKind::Inside,
        );
    }
    let font = FontId::proportional(12.0);
    let color = ui.visuals().text_color();
    let mut job = egui::text::LayoutJob::simple_singleline(caption.to_string(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: side,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.painter().layout_job(job);
    let text_pos = egui::pos2(rect.left(), img_rect.bottom() + 4.0);
    let clip = Rect::from_min_size(text_pos, vec2(side, 16.0));
    ui.painter_at(clip).galley(text_pos, galley, color);
    resp
}

fn recovery_panel(ui: &mut Ui, r: &RecoverState, output: &Path, thumbs: &mut Thumbnails) {
    let p = &r.pipeline;
    let done = r.status == RecoverStatus::Done;
    let n = p.stages.len();
    for (i, s) in p.stages.iter().enumerate() {
        let last = i + 1 == n;
        let running = last && !done;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            let good = theme::palette(ui.visuals(), Kind::Good).fg;
            if running {
                egui::Spinner::new().size(18.0).paint_at(ui, rect);
            } else if last && r.interrupted {
                icons::cross(
                    ui.painter(),
                    rect.center(),
                    10.0,
                    theme::palette(ui.visuals(), Kind::Warn).fg,
                );
            } else {
                icons::check(ui.painter(), rect.center(), 10.0, good);
            }
            let label = if running && (s.total > 0 || s.done > 0) {
                s.counting_label()
            } else {
                s.label.clone()
            };
            ui.label(if running {
                RichText::new(label).strong()
            } else {
                RichText::new(label)
            });
            if let Some(t) = s.took {
                if !running {
                    widgets::weak(ui, format!("({})", text::duration(t)));
                }
            }
        });
        if running {
            let bytes_stage = matches!(
                s.stage,
                diskrescue_recover::Stage::Carving | diskrescue_recover::Stage::Partitions
            );
            match s.fraction() {
                Some(f) => {
                    let label = if bytes_stage {
                        format!("{} of {}", text::bytes(s.done), text::bytes(s.total))
                    } else {
                        format!("{} of {}", text::count(s.done), text::count(s.total))
                    };
                    ui.add(
                        egui::ProgressBar::new(f as f32)
                            .text(label)
                            .desired_width(ui.available_width())
                            .desired_height(18.0),
                    );
                }
                None => {
                    ui.add(
                        egui::ProgressBar::new(0.0)
                            .animate(true)
                            .desired_width(ui.available_width())
                            .desired_height(18.0),
                    );
                }
            }
            if matches!(s.stage, diskrescue_recover::Stage::Carving) {
                if let Some(c) = &p.carve {
                    widgets::weak(
                        ui,
                        format!(
                            "{} found in the raw data so far",
                            text::plural(c.files, "file", "files")
                        ),
                    );
                }
            }
        }
    }
    if n == 0 && !done {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Opening...");
        });
    }

    let c = &p.counts;
    if c.total() > 0 {
        ui.add_space(6.0);
        let mut items = vec![widgets::Stat::new(
            "recovered",
            text::count(c.recovered + c.recovered_unverified),
            Some(Kind::Good),
        )];
        if c.repaired + c.partially_repaired > 0 {
            items.push(widgets::Stat::new(
                "repaired",
                text::count(c.repaired + c.partially_repaired),
                Some(Kind::Good),
            ));
        }
        if c.damaged > 0 {
            items.push(widgets::Stat::new(
                "damaged",
                text::count(c.damaged),
                Some(Kind::Warn),
            ));
        }
        if c.failed > 0 {
            items.push(widgets::Stat::new(
                "could not be recovered",
                text::count(c.failed),
                Some(Kind::Bad),
            ));
        }
        if c.duplicate > 0 {
            items.push(widgets::Stat::new(
                "duplicates skipped",
                text::count(c.duplicate),
                None,
            ));
        }
        widgets::stats(ui, &items);
    }

    let recent: Vec<_> = p
        .recent_files
        .iter()
        .rev()
        .filter(|f| f.outcome != Outcome::Duplicate)
        .take(14)
        .collect();
    if !recent.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Found most recently").strong());
        ui.horizontal_wrapped(|ui| {
            for f in recent {
                let path = output.join(&f.name);
                let name = system::file_name(Path::new(&f.name));
                let kind = Kind::from(text::outcome_tone(f.outcome));
                let kind = (kind != Kind::Good).then_some(kind);
                tile(ui, thumbs, &path, category_of(&path), 84.0, &name, kind)
                    .on_hover_text(format!("{} ({})", f.name, text::outcome_label(f.outcome)));
            }
        });
    }

    if !r.warnings.is_empty() || !p.warnings.is_empty() {
        let n = r.warnings.len() + p.warnings.len();
        egui::CollapsingHeader::new(format!("Notes ({n})"))
            .id_salt("recovery_notes")
            .show(ui, |ui| {
                for w in r.warnings.iter().chain(p.warnings.iter()) {
                    let (title, body) = crate::wizard::warning_text(w, false);
                    widgets::callout_text(ui, Kind::Warn, &title, &body);
                }
            });
    }
    if !p.volumes.is_empty() {
        egui::CollapsingHeader::new(format!("Volumes found ({})", p.volumes.len()))
            .id_salt("volumes")
            .show(ui, |ui| {
                for v in p.volumes.iter() {
                    ui.label(RichText::new(text::volume_line(v)).small());
                }
            });
    }
}

fn outcome_callout(
    ui: &mut Ui,
    job: &ActiveJob,
    snap: &Snapshot,
    o: &JobOutcome,
    is_root: bool,
) -> Option<Action> {
    let mut action = None;
    let report = snap.report();
    match o {
        JobOutcome::Finished => {
            let (title, body) = match report {
                Some(r) => {
                    let t = &r.totals;
                    let title = format!(
                        "Finished: {} recovered",
                        text::plural(t.files_written, "file was", "files were")
                    );
                    let mut body = format!("They are in {}.", job.output.display());
                    if t.deleted > 0 {
                        body.push_str(&format!(
                            " {} of them had been deleted.",
                            text::count(t.deleted)
                        ));
                    }
                    let c = &snap
                        .recover
                        .as_ref()
                        .map(|r| r.pipeline.counts)
                        .unwrap_or_default();
                    if c.damaged > 0 {
                        body.push_str(&format!(
                            " {} damaged: the Results page shows what is wrong with them.",
                            text::plural(c.damaged, "file is", "files are")
                        ));
                    }
                    (title, body)
                }
                None => ("Finished".to_string(), String::new()),
            };
            let kind = if report.is_some_and(|r| r.totals.files_written == 0) {
                Kind::Warn
            } else {
                Kind::Good
            };
            widgets::callout(ui, kind, &title, |ui| {
                if !body.is_empty() {
                    ui.label(body);
                }
                if report.is_some_and(|r| r.totals.files_written == 0) {
                    ui.label(
                        "No files were found. Try again with \"Everything\" selected, or with the raw \
                         data search set to \"The whole drive\" in the advanced settings.",
                    );
                }
                ui.horizontal(|ui| {
                    if report.is_some()
                        && widgets::primary_button(ui, true, "See your recovered files").clicked()
                    {
                        action = Some(Action::SeeResults);
                    }
                    if !is_root && ui.button("Open the folder").clicked() {
                        action = Some(Action::Open(job.output.clone()));
                    }
                    if ui.button("Rescue another drive").clicked() {
                        action = Some(Action::NewRescue);
                    }
                });
            });
        }
        JobOutcome::Cancelled => {
            widgets::callout(ui, Kind::Warn, "Stopped", |ui| {
                if report.is_some() {
                    ui.label(format!(
                        "The files found before you stopped are saved in {}.",
                        job.output.display()
                    ));
                }
                if let Some(img) = &job.image_path {
                    if snap.recover.as_ref().is_none_or(|r| r.report.is_none()) {
                        ui.label(format!(
                            "Everything copied so far is saved in {}. Start the same rescue again \
                             (same image name) to continue where it stopped.",
                            img.display()
                        ));
                    }
                }
                ui.horizontal(|ui| {
                    if report.is_some()
                        && widgets::primary_button(ui, true, "See the files found so far").clicked()
                    {
                        action = Some(Action::SeeResults);
                    }
                    if ui.button("Back to the settings").clicked() {
                        action = Some(Action::BackToSettings);
                    }
                });
            });
        }
        JobOutcome::Refused(m) => {
            widgets::callout(ui, Kind::Bad, "DiskRescue did not start", |ui| {
                ui.label(widgets::gui_text(m));
                ui.label("Nothing was written anywhere.");
                if ui.button("Back to the settings").clicked() {
                    action = Some(Action::BackToSettings);
                }
            });
        }
        JobOutcome::Failed(m) => {
            widgets::callout(
                ui,
                Kind::Bad,
                "The rescue stopped because of a problem",
                |ui| {
                    ui.label(widgets::gui_text(m));
                    ui.label("Everything already saved is kept. Nothing was written to the drive.");
                    ui.horizontal(|ui| {
                        if report.is_some()
                            && widgets::primary_button(ui, true, "See the files found so far")
                                .clicked()
                        {
                            action = Some(Action::SeeResults);
                        }
                        if ui.button("Back to the settings").clicked() {
                            action = Some(Action::BackToSettings);
                        }
                    });
                },
            );
        }
    }
    action
}

fn confirm_stop(ctx: &egui::Context, job: &mut ActiveJob, snap: &Snapshot) {
    let imaging = snap.imaging();
    let modal = egui::Modal::new(egui::Id::new("confirm_stop")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.heading("Stop the rescue?");
        if imaging {
            ui.label(
                "Everything copied so far is saved. You can continue later from where it stopped \
                 by starting the same rescue again.",
            );
        } else {
            ui.label("The files found so far are kept, and a report is written for them.");
        }
        ui.add_space(8.0);
        let mut close = false;
        ui.horizontal(|ui| {
            if widgets::button(ui, true, "Keep going").clicked() {
                close = true;
            }
            if widgets::danger_button(ui, true, "Stop and save").clicked() {
                job.cancel();
                close = true;
            }
        });
        close
    });
    if modal.inner || modal.should_close() {
        job.confirm_stop = false;
    }
}

/// A preview state with a partly copied drive, for screenshots.
#[doc(hidden)]
pub fn preview_snapshot() -> Snapshot {
    use diskrescue_core::device::{DeviceInfo, DeviceKind, Transport};
    use diskrescue_core::Range;
    use diskrescue_imaging::{Phase, Progress};
    let size: u64 = 32_017_047_552;
    let mut map = StatusMap::new(size);
    let mb = 1u64 << 20;
    map.set(Range::new(0, 14_200 * mb), BlockStatus::Finished);
    map.set(Range::new(3_100 * mb, 3_164 * mb), BlockStatus::NonTrimmed);
    map.set(Range::new(5_400 * mb, 5_401 * mb), BlockStatus::BadSector);
    map.set(Range::new(9_000 * mb, 9_090 * mb), BlockStatus::NonScraped);
    map.set(Range::new(9_090 * mb, 9_092 * mb), BlockStatus::BadSector);
    map.set(Range::new(21_000 * mb, 22_400 * mb), BlockStatus::Finished);
    map.set(Range::new(26_000 * mb, size), BlockStatus::Finished);
    let stats = map.stats();
    let info = DeviceInfo {
        path: PathBuf::from("/dev/sdb"),
        name: "sdb".into(),
        kind: DeviceKind::SdCard,
        transport: Transport::Usb,
        size,
        logical_sector: 512,
        physical_sector: 512,
        model: Some("SDXC Card".into()),
        vendor: Some("SanDisk".into()),
        serial: None,
        firmware: None,
        stable_id: None,
        removable: true,
        rotational: false,
        read_only: false,
        partitions: vec![],
        mountpoints: vec![],
        health: None,
        warnings: vec![],
    };
    let mut img = ImageState {
        status: ImageStatus::Copying,
        source: Some(info),
        image: PathBuf::from("/media/backup/SanDisk SDXC Card.img"),
        mapfile: PathBuf::from("/media/backup/SanDisk SDXC Card.img.map"),
        copy_size: size,
        phase: Some(Phase::Copying {
            pass: 1,
            reverse: false,
        }),
        progress: Some(Progress {
            phase: Phase::Copying {
                pass: 1,
                reverse: false,
            },
            stats,
            current_pos: 14_200 * mb,
            current_rate: 23.6e6,
            average_rate: 21.0e6,
            elapsed: std::time::Duration::from_secs(1_412),
            eta: Some(std::time::Duration::from_secs(14 * 60 + 20)),
            errors: 7,
            slow_reads: 2,
            timeouts: 1,
            reattaches: 0,
            last_error: Some("read error at 9.4 GB: input/output error".into()),
            size,
        }),
        map: Some(Arc::new(map)),
        ..ImageState::default()
    };
    img.log(log::Level::Info, "copying pass 1 (forwards)");
    let recover = RecoverState {
        status: RecoverStatus::Imaging,
        image: Some(img.clone()),
        ..RecoverState::default()
    };
    Snapshot {
        image: Some(img),
        privileged: false,
        recover: Some(recover),
        outcome: None,
        can_pause: true,
        paused: false,
        stopping: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::Range;

    #[test]
    fn map_cells_show_the_worst_status() {
        let mut m = StatusMap::new(1000);
        m.set(Range::new(0, 500), BlockStatus::Finished);
        m.set(Range::new(100, 101), BlockStatus::BadSector);
        let c = compute_cells(&m, 10);
        assert_eq!(c[0], Some(BlockStatus::Finished));
        assert_eq!(c[1], Some(BlockStatus::BadSector));
        assert_eq!(c[9], Some(BlockStatus::NonTried));
        assert_eq!(category_of(Path::new("a/B.JPG")), MediaCategory::Photo);
        assert_eq!(category_of(Path::new("x.mp4")), MediaCategory::Video);
    }
}
