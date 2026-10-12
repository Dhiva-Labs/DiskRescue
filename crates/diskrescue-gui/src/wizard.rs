//! The rescue wizard: what to recover, where to save it, then the safety
//! checks and Start.
//!
//! The checks shown on the last step are those of the plan without any
//! override; ticking "I understand the risk" records the override, which is
//! only applied to the plan that is started (the job checks it again).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use diskrescue_app::plan::{
    CarveMode, Categories, ImagePlan, RecoverPlan, RepairOptions, StateFilter, Tuning,
};
use diskrescue_app::preflight::{Blocker, BlockerCode, Override, Preflight, ResolvedRecover};
use diskrescue_app::{results, text, util};
use diskrescue_core::device::DeviceKind;
use diskrescue_core::profile::ImagingProfile;
use egui::{Align, Layout, RichText, Ui, WidgetType};

use crate::backend::Backend;
use crate::drives::DeviceEntry;
use crate::tasks::{Task, Watch};
use crate::theme::Kind;
use crate::{system, widgets};

/// The step names.
pub const STEPS: [&str; 3] = ["What to recover", "Where to save", "Review and start"];

/// What is being rescued.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// A disk, card or USB drive.
    Device(Box<DeviceEntry>),
    /// An image file made earlier.
    Image(PathBuf),
}

impl Source {
    /// The path given to the jobs.
    pub fn path(&self) -> PathBuf {
        match self {
            Source::Device(e) => e.info.path.clone(),
            Source::Image(p) => p.clone(),
        }
    }

    /// A name for titles.
    pub fn name(&self) -> String {
        match self {
            Source::Device(e) => e.title.clone(),
            Source::Image(p) => system::file_name(p),
        }
    }

    /// True for a device.
    pub fn is_device(&self) -> bool {
        matches!(self, Source::Device(_))
    }
}

/// Existing and/or deleted files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// Both (recommended).
    Both,
    /// Only deleted files.
    DeletedOnly,
    /// Only files that still exist.
    ExistingOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    Output,
    Image,
    Donor,
}

/// What the wizard asks the app to do.
pub enum Action {
    /// Leave the wizard.
    Cancel,
    /// Start a recovery (imaging first when the plan says so).
    Start(Box<RecoverPlan>),
    /// Copy the device with administrator rights, then recover from the
    /// copy with normal rights.
    StartPrivileged {
        /// The copy.
        image: Box<ImagePlan>,
        /// The recovery from the copy.
        then: Box<RecoverPlan>,
    },
}

type Check = Preflight<ResolvedRecover>;

/// The wizard's state (kept while the rescue runs, so that "Back to the
/// settings" returns to it).
pub struct Wizard {
    /// What is rescued.
    pub source: Source,
    /// 0, 1 or 2.
    pub step: usize,
    /// Photos.
    pub photos: bool,
    /// Camera RAW photos.
    pub raw: bool,
    /// Videos.
    pub videos: bool,
    /// Audio.
    pub audio: bool,
    /// Every file.
    pub everything: bool,
    /// Existing and/or deleted.
    pub which: Which,
    /// The folder for the results, as typed.
    pub output: String,
    /// Copy the device into an image first.
    pub image_first: bool,
    /// The image file, as typed.
    pub image_path: String,
    /// Imaging profile (`None`: chosen from the device type).
    pub profile: Option<String>,
    /// Read timeout in seconds (`None`: from the profile).
    pub timeout_secs: Option<u32>,
    /// Retry passes (`None`: from the profile).
    pub retries: Option<u32>,
    /// How much of the raw data to search.
    pub carve: CarveMode,
    /// Reference files for repairs.
    pub donors: Vec<PathBuf>,
    /// Risks the user accepted.
    pub acknowledged: HashSet<Override>,
    /// Why the last start failed.
    pub start_error: Option<String>,
    check: Watch<RecoverPlan, Check>,
    space: Watch<PathBuf, (Option<u64>, bool)>,
    image_space: Watch<PathBuf, Option<u64>>,
    picker: Option<(PickTarget, Task<Option<PathBuf>>)>,
}

/// `~` expanded, made absolute.
pub fn expand(s: &str) -> PathBuf {
    let t = s.trim();
    let p = if t == "~" {
        system::home_dir()
    } else if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        system::home_dir().join(rest)
    } else {
        PathBuf::from(t)
    };
    system::absolute(&p)
}

fn numbered(base: &Path, n: u32) -> PathBuf {
    if n <= 1 {
        return base.to_path_buf();
    }
    let parent = base.parent().unwrap_or(Path::new(""));
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match base.extension() {
        Some(ext) => parent.join(format!("{stem} ({n}).{}", ext.to_string_lossy())),
        None => parent.join(format!(
            "{} ({n})",
            base.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        )),
    }
}

/// A folder name that is free (missing or empty).
pub fn free_folder(base: &Path) -> PathBuf {
    for n in 1..1000 {
        let p = numbered(base, n);
        if !p.exists() || (p.is_dir() && !results::has_files(&p)) {
            return p;
        }
    }
    base.to_path_buf()
}

/// An image name that can be written: missing, or an earlier copy with its
/// progress map (which is continued). With `fresh`, only a missing name.
pub fn image_name(base: &Path, fresh: bool) -> PathBuf {
    for n in 1..1000 {
        let p = numbered(base, n);
        let map = diskrescue_app::source::default_map_path(&p);
        if !p.exists() && !map.exists() {
            return p;
        }
        if !fresh && p.is_file() && map.is_file() {
            return p;
        }
    }
    base.to_path_buf()
}

fn location_code(c: BlockerCode) -> bool {
    matches!(
        c,
        BlockerCode::OutputOnSource
            | BlockerCode::OutputIsSource
            | BlockerCode::OutputNotFolder
            | BlockerCode::OutputUnverified
            | BlockerCode::ImageIsSource
            | BlockerCode::ImageIsFolder
            | BlockerCode::ImageNotAFile
            | BlockerCode::MapIsImage
            | BlockerCode::NotEnoughSpace
            | BlockerCode::ImageExists
            | BlockerCode::ImageMissing
            | BlockerCode::DeviceChanged
    )
}

/// Overrides the recovery plan can apply (`allow_mounted`). The others
/// belong to imaging plans; their blockers are solved by choosing another
/// name or place instead.
fn supported(o: Override) -> bool {
    o == Override::AllowMounted
}

impl Wizard {
    fn new(source: Source, name: &str, image_first: bool) -> Self {
        let base = system::home_dir().join("DiskRescue");
        let name = system::safe_name(name);
        let output = free_folder(&base.join(format!("{name} - recovered")));
        let image = image_name(&base.join(format!("{name}.img")), false);
        Wizard {
            source,
            step: 0,
            photos: true,
            raw: true,
            videos: true,
            audio: false,
            everything: false,
            which: Which::Both,
            output: output.display().to_string(),
            image_first,
            image_path: image.display().to_string(),
            profile: None,
            timeout_secs: None,
            retries: None,
            carve: CarveMode::Auto,
            donors: Vec::new(),
            acknowledged: HashSet::new(),
            start_error: None,
            check: Watch::new(Duration::from_millis(350)),
            space: Watch::new(Duration::from_millis(250)),
            image_space: Watch::new(Duration::from_millis(250)),
            picker: None,
        }
    }

    /// A wizard for a drive (copying it first, the safe default).
    pub fn for_device(entry: DeviceEntry) -> Self {
        let name = entry.title.clone();
        Wizard::new(Source::Device(Box::new(entry)), &name, true)
    }

    /// A wizard for an image file.
    pub fn for_image(path: PathBuf) -> Self {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        Wizard::new(Source::Image(path), &name, false)
    }

    fn device(&self) -> Option<&DeviceEntry> {
        match &self.source {
            Source::Device(e) => Some(e),
            Source::Image(_) => None,
        }
    }

    /// The device can only be read with administrator rights: the copy
    /// then runs with those rights, so it is not optional.
    fn copy_required(&self) -> bool {
        self.device().is_some_and(|e| e.needs_privilege)
    }

    fn imaging(&self) -> bool {
        self.source.is_device() && (self.image_first || self.copy_required())
    }

    fn any_category(&self) -> bool {
        self.everything || self.photos || self.raw || self.videos || self.audio
    }

    fn tuning(&self) -> Tuning {
        Tuning {
            profile: self.profile.clone(),
            timeout: self.timeout_secs.map(|s| Duration::from_secs(u64::from(s))),
            retries: self.retries,
            ..Tuning::default()
        }
    }

    /// The plan as chosen, without overrides.
    pub fn plan(&self) -> RecoverPlan {
        let mut p = RecoverPlan::new(self.source.path(), expand(&self.output));
        p.categories = Categories {
            photos: self.photos && !self.everything,
            videos: self.videos && !self.everything,
            raw: self.raw && !self.everything,
            audio: self.audio && !self.everything,
            all_files: self.everything,
            min_size: None,
        };
        p.state = StateFilter {
            existing_only: self.which == Which::ExistingOnly,
            deleted_only: self.which == Which::DeletedOnly,
        };
        p.carve = self.carve;
        p.repair = RepairOptions {
            no_repair: false,
            donors: self.donors.clone(),
            aggressive: false,
        };
        if self.imaging() {
            p.image_to = Some(expand(&self.image_path));
            p.tuning = self.tuning();
        }
        p
    }

    /// The plan to start: the chosen plan plus the accepted overrides.
    pub fn start_plan(&self) -> RecoverPlan {
        let mut p = self.plan();
        p.allow_mounted = self.acknowledged.contains(&Override::AllowMounted);
        p
    }

    fn privileged_plans(&self) -> (ImagePlan, RecoverPlan) {
        let p = self.start_plan();
        let image = expand(&self.image_path);
        let mut ip = ImagePlan::new(self.source.path(), &image);
        ip.tuning = self.tuning();
        ip.allow_mounted = p.allow_mounted;
        let mut then = p;
        then.source = image;
        then.image_to = None;
        then.map = None;
        then.allow_mounted = false;
        then.tuning = Tuning::default();
        (ip, then)
    }

    fn paths_filled(&self) -> bool {
        !self.output.trim().is_empty() && (!self.imaging() || !self.image_path.trim().is_empty())
    }

    /// The current check result, when it is up to date.
    pub fn current_check(&self) -> Option<&Check> {
        self.check.get(&self.plan())
    }

    /// Keep the checks and free-space figures up to date.
    pub fn update(&mut self, ctx: &egui::Context, backend: &Arc<dyn Backend>) {
        if let Some((target, task)) = &mut self.picker {
            if let Some(r) = task.poll() {
                let target = *target;
                self.picker = None;
                if let Some(p) = r {
                    match target {
                        PickTarget::Output => self.output = p.display().to_string(),
                        PickTarget::Image => {
                            let p = if p.extension().is_none() {
                                p.with_extension("img")
                            } else {
                                p
                            };
                            self.image_path = p.display().to_string();
                        }
                        PickTarget::Donor => {
                            if !self.donors.contains(&p) {
                                self.donors.push(p);
                            }
                        }
                    }
                }
            }
        }
        if self.step >= 1 && self.paths_filled() {
            let plan = self.plan();
            let b = backend.clone();
            self.check
                .update(ctx, &plan, move |plan| b.preflight_recover(&plan));
            let out = expand(&self.output);
            self.space
                .update(ctx, &out, |p: PathBuf| (util::free_space(&p), p.exists()));
            if self.imaging() {
                let dir = util::parent_dir(&expand(&self.image_path));
                self.image_space
                    .update(ctx, &dir, |p: PathBuf| util::free_space(&p));
            }
        }
    }

    fn pick(&mut self, ctx: &egui::Context, target: PickTarget) {
        if self.picker.is_some() {
            return;
        }
        let start = match target {
            PickTarget::Output => expand(&self.output),
            PickTarget::Image => expand(&self.image_path),
            PickTarget::Donor => system::home_dir(),
        };
        let file_name = start
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let task = Task::spawn(ctx, "diskrescue-dialog", move || {
            let dir = existing_ancestor(&start).unwrap_or_else(system::home_dir);
            let d = rfd::FileDialog::new().set_directory(&dir);
            match target {
                PickTarget::Output => d
                    .set_title("Choose where to save the recovered files")
                    .set_can_create_directories(true)
                    .pick_folder(),
                PickTarget::Image => d
                    .set_title("Choose where to save the copy of the drive")
                    .set_file_name(file_name)
                    .set_can_create_directories(true)
                    .save_file(),
                PickTarget::Donor => d
                    .set_title("Choose a healthy photo or video from the same camera or phone")
                    .pick_file(),
            }
        });
        self.picker = Some((target, task));
    }

    /// Draw the wizard.
    pub fn show(&mut self, ui: &mut Ui, job_running: bool) -> Option<Action> {
        let mut action = None;
        let title = match &self.source {
            Source::Device(e) => format!("Rescue {}", e.title),
            Source::Image(p) => format!("Recover from {}", system::file_name(p)),
        };
        let subtitle = match &self.source {
            Source::Device(e) => e.subtitle(),
            Source::Image(p) => p.display().to_string(),
        };

        egui::Panel::bottom("wizard_buttons")
            .frame(
                egui::Frame::new()
                    .fill(crate::theme::page_fill(ui.visuals()))
                    .inner_margin(egui::Margin::symmetric(0, 10)),
            )
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let back = if self.step == 0 { "Cancel" } else { "Back" };
                    if widgets::button(ui, true, back).clicked() {
                        if self.step == 0 {
                            action = Some(Action::Cancel);
                        } else {
                            self.step -= 1;
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        match self.step {
                            0 => {
                                let ok = self.any_category();
                                let r = widgets::primary_button(ui, ok, "Next");
                                if r.on_disabled_hover_text("Choose at least one kind of file.")
                                    .clicked()
                                {
                                    self.step = 1;
                                }
                            }
                            1 => {
                                let (ok, why) = self.can_leave_step2();
                                let r = widgets::primary_button(ui, ok, "Next");
                                let r = match why {
                                    Some(w) => r.on_disabled_hover_text(w),
                                    None => r,
                                };
                                if r.clicked() {
                                    self.step = 2;
                                }
                            }
                            _ => {
                                let (ok, why) = self.can_start(job_running);
                                let r = widgets::primary_button(ui, ok, "Start rescue");
                                let r = match why {
                                    Some(w) => r.on_disabled_hover_text(w),
                                    None => r,
                                };
                                if r.clicked() {
                                    action = Some(Action::Start(Box::new(self.start_plan())));
                                }
                            }
                        }
                        if self.step > 0 && ui.button("Cancel").clicked() {
                            action = Some(Action::Cancel);
                        }
                    });
                });
            });

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::page_title(ui, &title, &subtitle);
                widgets::steps(ui, self.step, &STEPS);
                ui.add_space(10.0);
                match self.step {
                    0 => self.step_what(ui),
                    1 => self.step_where(ui),
                    _ => {
                        if let Some(a) = self.step_review(ui, job_running) {
                            action = Some(a);
                        }
                    }
                }
                ui.add_space(16.0);
            });
        action
    }

    fn step_what(&mut self, ui: &mut Ui) {
        widgets::section(ui, "What should DiskRescue look for?");
        widgets::card(ui, |ui| {
            let everything = self.everything;
            ui.add_enabled_ui(!everything, |ui| {
                kind_choice(
                    ui,
                    &mut self.photos,
                    "Photos",
                    "JPEG, PNG, HEIC and other pictures from cameras and phones",
                );
                kind_choice(
                    ui,
                    &mut self.raw,
                    "Camera RAW photos",
                    "CR2, CR3, NEF, ARW, DNG and other RAW files",
                );
                kind_choice(
                    ui,
                    &mut self.videos,
                    "Videos",
                    "MP4, MOV, AVI, MTS and other video clips",
                );
                kind_choice(
                    ui,
                    &mut self.audio,
                    "Audio",
                    "Voice memos and music: MP3, WAV, FLAC",
                );
            });
            ui.separator();
            kind_choice(
                ui,
                &mut self.everything,
                "Everything",
                "Every kind of file, including documents. Takes longer and finds more clutter.",
            );
            if !self.any_category() {
                widgets::callout_text(ui, Kind::Warn, "Choose at least one kind of file", "");
            }
        });

        widgets::section(ui, "Which files?");
        widgets::card(ui, |ui| {
            ui.radio_value(
                &mut self.which,
                Which::Both,
                "Deleted files and files that are still there (recommended)",
            );
            ui.radio_value(&mut self.which, Which::DeletedOnly, "Only deleted files");
            ui.radio_value(
                &mut self.which,
                Which::ExistingOnly,
                "Only files that are still there",
            );
            widgets::weak(
                ui,
                "Files that cannot be found by name (for example after formatting) are found by \
                 searching the raw data either way.",
            );
        });
    }

    fn location_blockers(&self) -> Vec<Blocker> {
        self.current_check()
            .map(|c| {
                c.blockers
                    .iter()
                    .filter(|b| location_code(b.code))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn can_leave_step2(&self) -> (bool, Option<&'static str>) {
        if self.output.trim().is_empty() {
            return (false, Some("Choose a folder for the recovered files."));
        }
        if self.imaging() && self.image_path.trim().is_empty() {
            return (false, Some("Choose a name for the copy of the drive."));
        }
        if !self.location_blockers().is_empty() {
            return (
                false,
                Some("Choose another place first (see the red message)."),
            );
        }
        (true, None)
    }

    fn step_where(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        widgets::section(ui, "Where should the recovered files go?");
        widgets::card(ui, |ui| {
            widgets::weak(
                ui,
                "Choose a folder on a different disk from the one you are rescuing, with plenty of \
                 free space. Nothing is ever written to the drive being rescued.",
            );
            let f = widgets::path_field(
                ui,
                "Save recovered files to",
                &mut self.output,
                "A folder on another disk",
                "Choose folder...",
                self.picker.is_none(),
            );
            if f.browse {
                self.pick(&ctx, PickTarget::Output);
            }
            let out = expand(&self.output);
            if !self.output.trim().is_empty() {
                let line = match self.space.get(&out) {
                    Some((Some(free), exists)) => format!(
                        "Free space there: {}.{}",
                        text::bytes(*free),
                        if *exists {
                            ""
                        } else {
                            " The folder will be created."
                        }
                    ),
                    Some((None, _)) => "The free space there could not be determined.".to_string(),
                    None => "Checking the free space...".to_string(),
                };
                widgets::weak(ui, line);
                let size = match &self.source {
                    Source::Device(e) => e.info.size,
                    Source::Image(_) => 0,
                };
                if let Some((Some(free), _)) = self.space.get(&out) {
                    if size > 0 && *free < size {
                        widgets::callout_text(
                            ui,
                            Kind::Warn,
                            "There may not be enough room",
                            &format!(
                                "If the drive was full, the recovered files can take up to {}. \
                                 Choose a bigger disk if you have one; otherwise DiskRescue stops \
                                 when this one is full and keeps what it saved.",
                                text::bytes(size)
                            ),
                        );
                    }
                }
            }
        });

        if self.source.is_device() {
            ui.add_space(6.0);
            widgets::card(ui, |ui| {
                let required = self.copy_required();
                let mut on = self.image_first || required;
                widgets::switch(
                    ui,
                    &mut on,
                    "Copy the drive to an image first (recommended for failing drives)",
                    !required,
                );
                if !required {
                    self.image_first = on;
                }
                if required {
                    widgets::weak(
                        ui,
                        "Needed for this drive: it can only be read with administrator rights, so \
                         DiskRescue copies it once with those rights and then works on the copy.",
                    );
                } else {
                    widgets::weak(
                        ui,
                        "DiskRescue reads the drive once, carefully, into an image file, then finds \
                         your files in that copy. If the drive gets worse, nothing more is lost, and \
                         the copy can be stopped and continued later. Turn this off only for a \
                         healthy drive (for example after accidental deletion).",
                    );
                }
                if self.imaging() {
                    ui.add_space(4.0);
                    let f = widgets::path_field(
                        ui,
                        "Image file",
                        &mut self.image_path,
                        "For example /media/backup/card.img",
                        "Choose...",
                        self.picker.is_none(),
                    );
                    if f.browse {
                        self.pick(&ctx, PickTarget::Image);
                    }
                    let size = self.device().map(|e| e.info.size).unwrap_or(0);
                    let dir = util::parent_dir(&expand(&self.image_path));
                    let free = match self.image_space.get(&dir) {
                        Some(Some(f)) => format!("{} free there", text::bytes(*f)),
                        Some(None) => "free space unknown".into(),
                        None => "checking the free space".into(),
                    };
                    widgets::weak(
                        ui,
                        format!(
                            "The copy needs up to {} (the size of the drive); {free}.",
                            text::bytes(size)
                        ),
                    );
                }
            });
        }

        let blockers = self.location_blockers();
        if !blockers.is_empty() {
            ui.add_space(6.0);
            for b in &blockers {
                self.blocker_callout(ui, b);
            }
        }

        ui.add_space(6.0);
        egui::CollapsingHeader::new(RichText::new("Advanced settings").strong())
            .id_salt("advanced")
            .default_open(false)
            .show(ui, |ui| self.advanced(ui));
    }

    fn advanced(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.imaging() {
            widgets::section(ui, "Reading strategy");
            let kind = self
                .device()
                .map(|e| e.info.kind)
                .unwrap_or(DeviceKind::Unknown);
            let auto = ImagingProfile::for_kind(kind).name;
            let selected_text = match &self.profile {
                None => format!("Automatic ({auto})"),
                Some(p) => p.clone(),
            };
            let label = ui.label("Profile");
            egui::ComboBox::from_id_salt("profile")
                .selected_text(selected_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.profile, None, format!("Automatic ({auto})"));
                    for n in ImagingProfile::names() {
                        ui.selectable_value(&mut self.profile, Some(n.to_string()), *n);
                    }
                })
                .response
                .labelled_by(label.id);
            let name = self.profile.clone().unwrap_or(auto);
            widgets::weak(ui, text::profile_blurb(&name));
            if let Some(p) = ImagingProfile::by_name(&name) {
                widgets::weak(ui, text::profile_details(&p));
            }

            ui.horizontal(|ui| {
                let mut auto_t = self.timeout_secs.is_none();
                if ui
                    .checkbox(&mut auto_t, "Automatic time limit for one read")
                    .changed()
                {
                    self.timeout_secs = if auto_t { None } else { Some(10) };
                }
                if let Some(t) = &mut self.timeout_secs {
                    let r = ui.add(egui::DragValue::new(t).range(1..=600).suffix(" s"));
                    widgets::accessible(r, WidgetType::DragValue, "Time limit for one read");
                }
            });
            ui.horizontal(|ui| {
                let mut auto_r = self.retries.is_none();
                if ui
                    .checkbox(&mut auto_r, "Automatic number of retry passes")
                    .changed()
                {
                    self.retries = if auto_r { None } else { Some(1) };
                }
                if let Some(r) = &mut self.retries {
                    let resp = ui.add(egui::DragValue::new(r).range(0..=20));
                    widgets::accessible(resp, WidgetType::DragValue, "Retry passes");
                }
            });
        }

        widgets::section(ui, "Searching the raw data");
        widgets::weak(
            ui,
            "Finds photos and videos that the file list no longer knows about (deleted long ago, \
             formatted cards).",
        );
        ui.radio_value(
            &mut self.carve,
            CarveMode::Auto,
            "Automatic (recommended): free space and anything the file list does not cover",
        );
        ui.radio_value(&mut self.carve, CarveMode::Free, "Free space only");
        ui.radio_value(&mut self.carve, CarveMode::All, "The whole drive");
        ui.radio_value(
            &mut self.carve,
            CarveMode::Off,
            "Do not search the raw data",
        );

        widgets::section(ui, "Reference files for repairs");
        widgets::weak(
            ui,
            "Optional: a healthy photo or video taken with the same camera or phone, with the same \
             settings. DiskRescue uses it to rebuild damaged files, for example videos that will not \
             play.",
        );
        let mut remove = None;
        for (i, d) in self.donors.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(d.display().to_string());
                let r = ui.small_button("Remove");
                let r = widgets::accessible(
                    r,
                    WidgetType::Button,
                    &format!("Remove {}", system::file_name(d)),
                );
                if r.clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.donors.remove(i);
        }
        if widgets::button(ui, self.picker.is_none(), "Add reference file...").clicked() {
            self.pick(&ctx, PickTarget::Donor);
        }
    }

    fn blockers_resolved(&self, c: &Check, ignore: Option<BlockerCode>) -> bool {
        c.blockers.iter().all(|b| {
            Some(b.code) == ignore
                || b.override_flag
                    .is_some_and(|o| supported(o) && self.acknowledged.contains(&o))
        })
    }

    fn can_start(&self, job_running: bool) -> (bool, Option<&'static str>) {
        if job_running {
            return (
                false,
                Some("A rescue is already running. Wait for it to finish, or stop it first."),
            );
        }
        if !self.paths_filled() || !self.any_category() {
            return (
                false,
                Some("Some choices are missing: go back and complete them."),
            );
        }
        match self.current_check() {
            None => (false, Some("Checking that everything is safe...")),
            Some(c) if !self.blockers_resolved(c, None) => {
                (false, Some("Solve the problems listed above first."))
            }
            Some(_) => (true, None),
        }
    }

    fn summary(&self, ui: &mut Ui) {
        let looking = if self.everything {
            "Every kind of file".to_string()
        } else {
            let mut kinds = Vec::new();
            if self.photos {
                kinds.push("photos");
            }
            if self.raw {
                kinds.push("camera RAW photos");
            }
            if self.videos {
                kinds.push("videos");
            }
            if self.audio {
                kinds.push("audio files");
            }
            let s = match kinds.len() {
                0 => String::new(),
                1 => kinds[0].to_string(),
                n => format!("{} and {}", kinds[..n - 1].join(", "), kinds[n - 1]),
            };
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => s,
            }
        };
        let which = match self.which {
            Which::Both => "Deleted files and files that are still there",
            Which::DeletedOnly => "Only deleted files",
            Which::ExistingOnly => "Only files that are still there",
        };
        let mut rows = vec![
            ("Rescue from", self.source.name()),
            ("Looking for", looking),
            ("Which files", which.to_string()),
            ("Save to", expand(&self.output).display().to_string()),
        ];
        if self.source.is_device() {
            rows.push((
                "Copy first",
                if self.imaging() {
                    format!("Yes, to {}", expand(&self.image_path).display())
                } else {
                    "No, read the drive directly".to_string()
                },
            ));
        }
        widgets::card(ui, |ui| widgets::facts(ui, "summary", &rows));
    }

    fn step_review(&mut self, ui: &mut Ui, job_running: bool) -> Option<Action> {
        let mut action = None;
        widgets::section(ui, "Your choices");
        self.summary(ui);

        widgets::section(ui, "Safety checks");
        let plan = self.plan();
        let Some(check) = self.check.get(&plan).cloned() else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking that everything is safe...");
            });
            return None;
        };

        if let Some(r) = &check.resolved {
            if let Some(img) = &r.image {
                if img.already_complete {
                    widgets::callout_text(
                        ui,
                        Kind::Info,
                        "The copy of this drive is already complete",
                        "DiskRescue goes straight to finding your files in it; the drive is not read again.",
                    );
                } else if let Some(st) = &img.resuming {
                    widgets::callout_text(
                        ui,
                        Kind::Info,
                        "Continuing an earlier copy",
                        &format!(
                            "An earlier copy of this drive was found in {}: {} of it was copied. \
                             DiskRescue continues from where it stopped.",
                            img.image.display(),
                            text::percent(st.rescued, st.total())
                        ),
                    );
                }
            }
        }

        // Administrator rights help a device; an unreadable image file is
        // explained like any other problem.
        let permission = check.has(BlockerCode::PermissionDenied) && self.source.is_device();
        if permission {
            if let Some(a) = self.permission_banner(ui, &check, job_running) {
                action = Some(a);
            }
        }
        for b in check
            .blockers
            .iter()
            .filter(|b| !(permission && b.code == BlockerCode::PermissionDenied))
        {
            self.blocker_callout(ui, b);
        }
        for w in &check.warnings {
            let (title, body) = warning_text(w, self.imaging());
            widgets::callout_text(ui, Kind::Warn, &title, &body);
        }
        if check.blockers.is_empty() {
            widgets::callout_text(
                ui,
                Kind::Good,
                "Everything is ready",
                "You can stop at any time. Everything found so far is kept.",
            );
        } else if self.blockers_resolved(&check, None) {
            widgets::callout_text(
                ui,
                Kind::Good,
                "Ready to start",
                "You accepted the risk above. You can stop at any time.",
            );
        }
        if let Some(e) = &self.start_error {
            widgets::callout_text(ui, Kind::Bad, "The rescue could not be started", e);
        }
        action
    }

    fn permission_banner(
        &mut self,
        ui: &mut Ui,
        check: &Check,
        job_running: bool,
    ) -> Option<Action> {
        let mut action = None;
        let others_ok = self.blockers_resolved(check, Some(BlockerCode::PermissionDenied));
        widgets::callout(
            ui,
            Kind::Warn,
            "DiskRescue needs administrator rights to read this drive",
            |ui| {
                ui.label(
                    "Your account is not allowed to read drives directly. This protects your \
                     computer; it does not mean anything is wrong with the drive.",
                );
                if cfg!(windows) {
                    ui.label(
                        "Close DiskRescue, then right-click it and choose \"Run as administrator\", \
                         and start the rescue again.",
                    );
                    return;
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Option 1 (easiest)").strong());
                ui.label(format!(
                    "Your computer asks for an administrator password. DiskRescue then copies the \
                     drive into {} with those rights, and finds your files in that copy with your \
                     normal rights.",
                    expand(&self.image_path).display()
                ));
                let enabled = others_ok && !job_running && self.paths_filled();
                let r = widgets::primary_button(ui, enabled, "Copy with administrator rights");
                let r = if !others_ok {
                    r.on_disabled_hover_text("Solve the other problems listed below first.")
                } else {
                    r
                };
                if r.clicked() {
                    let (image, then) = self.privileged_plans();
                    action = Some(Action::StartPrivileged {
                        image: Box::new(image),
                        then: Box::new(then),
                    });
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Option 2").strong());
                let user = system::user_name();
                let cmd = format!("sudo usermod -aG disk {user}");
                ui.label(
                    "Give your account permission to read drives: open a terminal, run the command \
                     below, then log out and log back in, and start DiskRescue again.",
                );
                ui.horizontal(|ui| {
                    ui.code(&cmd);
                    if ui.button("Copy command").clicked() {
                        ui.ctx().copy_text(cmd.clone());
                    }
                });
            },
        );
        action
    }

    fn fix_buttons(&mut self, ui: &mut Ui, fix: Fix) {
        match fix {
            Fix::None => {}
            Fix::ChangePlace => {
                if self.step != 1 && ui.button("Change where to save").clicked() {
                    self.step = 1;
                }
            }
            Fix::NewImageName => {
                if ui.button("Use a new name for the copy").clicked() {
                    let base = expand(&self.image_path);
                    self.image_path = image_name(&base, true).display().to_string();
                }
            }
            Fix::Advanced => {
                if self.step != 1 && ui.button("Change the settings").clicked() {
                    self.step = 1;
                }
            }
        }
    }

    fn blocker_callout(&mut self, ui: &mut Ui, b: &Blocker) {
        let (title, todo, fix) = blocker_help(b.code);
        let message = widgets::gui_text(&b.message);
        let overridable = b.override_flag.filter(|o| supported(*o));
        let kind = if overridable.is_some_and(|o| self.acknowledged.contains(&o)) {
            Kind::Warn
        } else {
            Kind::Bad
        };
        widgets::callout(ui, kind, title, |ui| {
            ui.label(message);
            if !todo.is_empty() {
                ui.label(RichText::new(format!("What to do: {todo}")).strong());
            }
            let recheck = matches!(
                b.code,
                BlockerCode::SourceMounted
                    | BlockerCode::SourceMissing
                    | BlockerCode::SourceUnreadable
                    | BlockerCode::PermissionDenied
                    | BlockerCode::NotEnoughSpace
                    | BlockerCode::OutputUnverified
            );
            if fix == Fix::None && !recheck {
                // Nothing to click.
            } else {
                ui.horizontal_wrapped(|ui| {
                    self.fix_buttons(ui, fix);
                    if recheck && ui.button("Check again").clicked() {
                        self.check.invalidate();
                        self.space.invalidate();
                        self.image_space.invalidate();
                    }
                });
            }
            if let Some(o) = overridable {
                let mut ack = self.acknowledged.contains(&o);
                if ui.checkbox(&mut ack, "I understand the risk").changed() {
                    if ack {
                        self.acknowledged.insert(o);
                    } else {
                        self.acknowledged.remove(&o);
                    }
                }
                widgets::weak(ui, override_text(o));
            }
        });
    }
}

fn kind_choice(ui: &mut Ui, value: &mut bool, label: &str, help: &str) {
    ui.horizontal(|ui| {
        ui.checkbox(value, RichText::new(label).strong());
        widgets::weak(ui, help);
    });
}

fn existing_ancestor(p: &Path) -> Option<PathBuf> {
    let mut cur = p.to_path_buf();
    loop {
        if cur.is_dir() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fix {
    None,
    ChangePlace,
    NewImageName,
    Advanced,
}

/// Title, what to do, and the quick fix for a blocker.
fn blocker_help(code: BlockerCode) -> (&'static str, &'static str, Fix) {
    use BlockerCode::*;
    match code {
        SourceMissing => (
            "The drive cannot be found",
            "Check that it is still connected (try another port or cable), then go back to the \
             list of drives.",
            Fix::None,
        ),
        SourceIsFolder => ("That is a folder", "Choose the image file itself.", Fix::None),
        PermissionDenied => (
            "This cannot be read with your account",
            "The file belongs to another user. Copy it to a folder of yours (or ask an \
             administrator to give you access), then choose that copy.",
            Fix::None,
        ),
        SourceUnreadable => (
            "The drive cannot be read",
            "Check that it is connected; try another port, cable or card reader.",
            Fix::None,
        ),
        ZeroCapacity => (
            "The drive reports a size of 0 bytes",
            "Its controller has most likely failed; software cannot read it. A data recovery lab can \
             often still get the files back.",
            Fix::None,
        ),
        SourceMounted => (
            "The drive is in use by the system (mounted)",
            "In your file manager, right-click the drive and choose \"Unmount\" (not \"Eject\" or \
             \"Safely remove\", which disconnect it), then press Check again. Or tick the box below \
             to read it while it is mounted.",
            Fix::None,
        ),
        OutputOnSource | OutputIsSource => (
            "That place is on the drive being rescued",
            "Choose a folder on a different disk. Writing to the drive being rescued would \
             overwrite the very files you want back.",
            Fix::ChangePlace,
        ),
        OutputUnverified => (
            "DiskRescue cannot tell which disk that folder is on",
            "Choose a folder on a local disk (not a network share) that is not the drive being \
             rescued.",
            Fix::ChangePlace,
        ),
        OutputNotFolder => (
            "That is a file, not a folder",
            "Choose a folder for the recovered files.",
            Fix::ChangePlace,
        ),
        ImageIsSource | ImageIsFolder | ImageNotAFile | MapIsImage => (
            "The image file cannot go there",
            "Choose another name or place for the copy.",
            Fix::ChangePlace,
        ),
        MapUnreadable | MapMissing => (
            "The progress map cannot be used",
            "Use a new name for the copy to start a new one.",
            Fix::NewImageName,
        ),
        ImageExists => (
            "A file with that name already exists",
            "Use a new name for the copy; DiskRescue never overwrites an existing file.",
            Fix::NewImageName,
        ),
        ImageMissing => (
            "The earlier copy is missing",
            "Put the image file back where it was to continue it, or use a new name to start over.",
            Fix::NewImageName,
        ),
        DeviceChanged => (
            "That copy belongs to a different drive",
            "Use a new name for this drive's copy, so the two never get mixed up.",
            Fix::NewImageName,
        ),
        NotEnoughSpace => (
            "Not enough free space for the copy",
            "Choose a place with more room, for example an external drive.",
            Fix::ChangePlace,
        ),
        InvalidOption => (
            "A setting is not valid",
            "Check the advanced settings.",
            Fix::Advanced,
        ),
        DonorMissing => (
            "A reference file cannot be used",
            "Remove it or choose another one in the advanced settings.",
            Fix::Advanced,
        ),
        InputInvalid => ("A file cannot be used", "", Fix::None),
    }
}

fn override_text(o: Override) -> &'static str {
    match o {
        Override::AllowMounted => {
            "DiskRescue will read the drive while the system uses it. Results may be incomplete if \
             something writes to it; nothing is written by DiskRescue."
        }
        Override::Force => "The existing file will be replaced.",
        Override::SkipSpaceCheck => "The copy may stop when the disk is full.",
    }
}

/// A preflight warning in desktop words: (title, text).
pub fn warning_text(w: &str, imaging: bool) -> (String, String) {
    if w.starts_with("Reading directly from the device") {
        let _ = imaging;
        return (
            "The drive will be read directly".into(),
            "That is fine for a healthy drive (for example after accidental deletion). If the drive \
             is failing (read errors, clicking, very slow, disconnects), go back and turn on \"Copy \
             the drive to an image first\"."
                .into(),
        );
    }
    if w.contains("already contains the report of an earlier run") {
        return (
            "That folder holds an earlier rescue".into(),
            "Its report is kept (renamed) and no file is overwritten: new copies get numbered names. \
             Choose a new folder to keep the rescues apart."
                .into(),
        );
    }
    ("Please note".into(), widgets::gui_text(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_names() {
        assert_eq!(
            numbered(Path::new("/a/card.img"), 2),
            PathBuf::from("/a/card (2).img")
        );
        assert_eq!(
            numbered(Path::new("/a/out dir"), 3),
            PathBuf::from("/a/out dir (3)")
        );
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("x.img");
        std::fs::write(&img, b"1").unwrap();
        assert_eq!(image_name(&img, false), dir.path().join("x (2).img"));
        std::fs::write(dir.path().join("x.img.map"), b"1").unwrap();
        assert_eq!(image_name(&img, false), img);
        assert_eq!(image_name(&img, true), dir.path().join("x (2).img"));
    }
}
