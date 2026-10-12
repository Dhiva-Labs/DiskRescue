//! "Rescue from a drive": one card per disk, card and USB drive, refreshed
//! every few seconds so that a drive plugged in shows up by itself.

use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_app::{devices, text};
use diskrescue_core::device::DeviceInfo;
use egui::{vec2, Align, Layout, RichText, Sense, Ui, WidgetType};

use crate::backend::Backend;
use crate::icons::{self, DriveIcon};
use crate::tasks::Task;
use crate::theme::{self, Kind};
use crate::widgets;

/// How often the list is refreshed while the page is shown.
pub const REFRESH: Duration = Duration::from_secs(3);

/// A drive as shown on its card.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceEntry {
    /// What the system says about it.
    pub info: DeviceInfo,
    /// Its name ("SanDisk Ultra").
    pub title: String,
    /// Plain-language notes, most important first.
    pub advice: Vec<String>,
    /// It holds the running operating system: never rescued from here.
    pub system: bool,
    /// Signs that it is failing (0 bytes, SMART failed, pending sectors).
    pub failing: bool,
    /// Reading it needs administrator rights.
    pub needs_privilege: bool,
}

impl DeviceEntry {
    /// Describe `info`, asking `backend` whether it can be read.
    pub fn new(info: DeviceInfo, backend: &dyn Backend) -> Self {
        Self::with_privilege(info, |i| backend.needs_privilege(&i.path))
    }

    fn with_privilege(info: DeviceInfo, needs: impl FnOnce(&DeviceInfo) -> bool) -> Self {
        let mut advice = devices::advice(&info);
        if let Some(h) = &info.health {
            for l in devices::health_lines(h) {
                let mut c = l.chars();
                let l = match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => continue,
                };
                if !advice.contains(&l) {
                    advice.push(l);
                }
            }
        }
        let system = devices::is_system_disk(&info);
        let failing = info.size == 0
            || info.health.as_ref().is_some_and(|h| {
                h.passed == Some(false)
                    || h.pending_sectors.unwrap_or(0) > 0
                    || h.uncorrectable_sectors.unwrap_or(0) > 0
            });
        let needs_privilege = !system && needs(&info);
        DeviceEntry {
            title: title(&info),
            advice,
            system,
            failing,
            needs_privilege,
            info,
        }
    }

    /// Size, type and path in one line.
    pub fn subtitle(&self) -> String {
        format!(
            "{}  ·  {}  ·  {}",
            text::size_label(self.info.size),
            devices::kind_label(self.info.kind),
            self.info.path.display()
        )
    }

    /// Mounted (in use by the system).
    pub fn mounted(&self) -> bool {
        self.info.is_mounted()
    }
}

/// "Vendor Model", or the kind of device when it has no name.
pub fn title(info: &DeviceInfo) -> String {
    let clean = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let name = match (clean(&info.vendor), clean(&info.model)) {
        (Some(v), Some(m)) if m.to_lowercase().starts_with(&v.to_lowercase()) => m,
        (Some(v), Some(m)) => format!("{v} {m}"),
        (None, Some(m)) => m,
        (Some(v), None) => v,
        (None, None) => String::new(),
    };
    if name.is_empty() {
        let k = devices::kind_label(info.kind);
        let mut c = k.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => "Drive".into(),
        }
    } else {
        name
    }
}

/// The drive list and its refresh.
#[derive(Default)]
pub struct Drives {
    /// Drives found by the last refresh.
    pub entries: Vec<DeviceEntry>,
    /// Why the last refresh failed.
    pub error: Option<String>,
    /// At least one refresh finished.
    pub loaded: bool,
    task: Option<Task<Result<Vec<DeviceEntry>, String>>>,
    last: Option<Instant>,
}

impl Drives {
    /// Take finished refreshes and start a new one every [`REFRESH`] while
    /// the page is shown.
    pub fn update(&mut self, ctx: &egui::Context, backend: &Arc<dyn Backend>, visible: bool) {
        if let Some(t) = &mut self.task {
            if let Some(r) = t.poll() {
                self.task = None;
                self.loaded = true;
                match r {
                    Ok(v) => {
                        self.entries = v;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if !visible {
            return;
        }
        let due = self.last.is_none_or(|t| t.elapsed() >= REFRESH);
        if self.task.is_none() && due {
            self.refresh(ctx, backend);
        }
        let wait = self
            .last
            .map(|t| REFRESH.saturating_sub(t.elapsed()))
            .unwrap_or(REFRESH);
        ctx.request_repaint_after(wait.max(Duration::from_millis(200)));
    }

    /// Refresh now (unless a refresh is running).
    pub fn refresh(&mut self, ctx: &egui::Context, backend: &Arc<dyn Backend>) {
        if self.task.is_some() {
            return;
        }
        self.last = Some(Instant::now());
        let backend = backend.clone();
        // A drive seen before keeps its answer: its node is not opened
        // again every few seconds.
        let known: Vec<(Identity, bool)> = self
            .entries
            .iter()
            .map(|e| (identity(&e.info), e.needs_privilege))
            .collect();
        self.task = Some(Task::spawn(ctx, "diskrescue-list", move || {
            let list = backend.list_devices()?;
            Ok(list
                .into_iter()
                .map(|i| {
                    let id = identity(&i);
                    match known.iter().find(|(k, _)| *k == id) {
                        Some((_, p)) => DeviceEntry::with_privilege(i, |_| *p),
                        None => DeviceEntry::new(i, backend.as_ref()),
                    }
                })
                .collect())
        }));
    }

    /// True while a refresh runs.
    pub fn refreshing(&self) -> bool {
        self.task.is_some()
    }
}

type Identity = (std::path::PathBuf, u64, Option<String>);

fn identity(i: &DeviceInfo) -> Identity {
    (i.path.clone(), i.size, i.serial.clone())
}

/// What the user chose on the page.
pub enum Action {
    /// Start the rescue wizard for this drive.
    Rescue(Box<DeviceEntry>),
    /// Refresh the list now.
    Refresh,
    /// Go to the image file page.
    ImageFile,
}

/// Draw the page.
pub fn show(ui: &mut Ui, drives: &Drives, job_running: bool) -> Option<Action> {
    let mut action = None;
    widgets::sides(
        ui,
        |ui| {
            ui.vertical(|ui| {
                widgets::page_title(
                    ui,
                    "Rescue from a drive",
                    "Choose the card, USB drive or disk your photos and videos were on.",
                );
            });
        },
        |ui| {
            let label = if drives.refreshing() {
                "Refreshing..."
            } else {
                "Refresh"
            };
            if widgets::button(ui, !drives.refreshing(), label).clicked() {
                action = Some(Action::Refresh);
            }
        },
    );

    widgets::callout(ui, Kind::Info, "Before you start", |ui| {
        ui.label(
            "Stop using the drive: do not take more photos with it, copy files onto it, or let the \
             computer \"repair\" or format it. DiskRescue only reads the drive; it never writes to it.",
        );
    });
    ui.add_space(8.0);

    if let Some(e) = &drives.error {
        widgets::callout_text(
            ui,
            Kind::Bad,
            "The list of drives could not be read",
            &widgets::gui_text(e),
        );
    }

    if !drives.loaded && drives.entries.is_empty() {
        ui.horizontal(|ui| {
            widgets::busy(ui, 18.0);
            ui.label("Looking for drives...");
        });
        return action;
    }

    if drives.entries.is_empty() {
        widgets::card(ui, |ui| {
            ui.label(RichText::new("No drives found").strong().size(17.0));
            ui.label(
                "Connect the card, USB drive or disk. It appears here by itself after a few seconds. \
                 If it does not, try another USB port, cable or card reader.",
            );
        });
    }

    for (i, entry) in drives.entries.iter().enumerate() {
        ui.push_id(i, |ui| {
            if let Some(a) = device_card(ui, entry, job_running) {
                action = Some(a);
            }
        });
        ui.add_space(4.0);
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        widgets::weak(ui, "Already have an image file of the drive?");
        if ui.link("Use an image file instead").clicked() {
            action = Some(Action::ImageFile);
        }
    });
    widgets::weak(
        ui,
        "This list updates by itself every few seconds when a drive is plugged in or removed.",
    );
    action
}

fn device_card(ui: &mut Ui, e: &DeviceEntry, job_running: bool) -> Option<Action> {
    let mut action = None;
    widgets::card(ui, |ui| {
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(60.0, 60.0), Sense::hover());
            let color = if e.failing {
                theme::palette(ui.visuals(), Kind::Bad).fg
            } else {
                theme::accent(ui.visuals())
            };
            icons::drive(ui.painter(), rect, DriveIcon::for_kind(e.info.kind), color);
            ui.add_space(6.0);

            let (can, why) = can_rescue(e, job_running);
            let button_w = 130.0;
            let text_w = (ui.available_width() - button_w - 16.0).max(200.0);
            ui.vertical(|ui| {
                ui.set_width(text_w);
                ui.label(RichText::new(&e.title).strong().size(17.0));
                widgets::weak(ui, e.subtitle());
                badges(ui, e);
                let limit = if e.system { 1 } else { 2 };
                for a in e.advice.iter().take(limit) {
                    ui.label(RichText::new(a).small());
                }
                if e.advice.len() > limit {
                    egui::CollapsingHeader::new("More about this drive")
                        .id_salt("more")
                        .show(ui, |ui| {
                            for a in e.advice.iter().skip(limit) {
                                ui.label(RichText::new(a).small());
                            }
                        });
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let r = widgets::primary_button(ui, can, "Rescue");
                let r = widgets::accessible(r, WidgetType::Button, &format!("Rescue {}", e.title));
                let r = match &why {
                    Some(w) => r.on_disabled_hover_text(w.as_str()),
                    None => r,
                };
                if r.clicked() {
                    action = Some(Action::Rescue(Box::new(e.clone())));
                }
            });
        });
    });
    action
}

/// Whether the Rescue button is enabled, and why not.
pub fn can_rescue(e: &DeviceEntry, job_running: bool) -> (bool, Option<String>) {
    if e.system {
        return (
            false,
            Some(
                "Your computer is running from this disk and keeps writing to it, which can overwrite \
                 deleted photos. Shut the computer down and rescue the disk from another computer."
                    .into(),
            ),
        );
    }
    if e.info.size == 0 {
        return (
            false,
            Some(
                "This drive reports a size of 0 bytes: its controller has most likely failed and \
                 software cannot read it. A data recovery lab can often still get the files back."
                    .into(),
            ),
        );
    }
    if job_running {
        return (
            false,
            Some("A rescue is already running. Wait for it to finish, or stop it first.".into()),
        );
    }
    (true, None)
}

fn badges(ui: &mut Ui, e: &DeviceEntry) {
    let mut list: Vec<(&str, Kind, Option<&str>)> = Vec::new();
    if e.system {
        list.push(("This is the disk your computer runs from", Kind::Bad, None));
    }
    if e.failing {
        list.push((
            "Failing",
            Kind::Bad,
            Some("Copy it to an image first, and do not use it for anything else."),
        ));
    }
    if e.mounted() && !e.system {
        list.push((
            "Mounted",
            Kind::Warn,
            Some(
                "The system is using this drive. Unmount it in your file manager before rescuing, \
                 so that nothing writes to it.",
            ),
        ));
    }
    if e.info.read_only {
        list.push(("Read-only", Kind::Info, None));
    }
    if e.needs_privilege {
        list.push((
            "Needs administrator rights",
            Kind::Neutral,
            Some("DiskRescue asks for an administrator password to copy it."),
        ));
    }
    if list.is_empty() && e.info.removable {
        list.push(("Removable", Kind::Neutral, None));
    }
    if list.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (text, kind, hover) in list {
            let r = widgets::badge(ui, text, kind);
            if let Some(h) = hover {
                r.on_hover_text(h);
            }
        }
    });
}
