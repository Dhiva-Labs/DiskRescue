//! The home screen: the disks, cards and USB drives that are connected,
//! the other things to open, and what is known about the selection.

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use diskrescue_app::devices::{self, DeviceDetails};
use diskrescue_app::helper;
use diskrescue_app::text::{self, size_label, transport_label};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config::{Config, DeviceSource};
use crate::model::DeviceEntry;
use crate::pending::Pending;
use crate::widgets::{ellipsize_end, wrap_words, ListNav, Theme};

/// One selectable row of the home screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeItem {
    /// A device (index into the entries).
    Device(usize),
    /// Open an image file.
    OpenImage,
    /// Repair or check files.
    RepairFiles,
    /// Open the results of an earlier run.
    OpenResults,
}

/// What is known about a device.
#[derive(Debug, Clone)]
pub enum Detail {
    /// Being read.
    Loading,
    /// Read.
    Ready(Box<DeviceDetails>),
    /// Could not be read.
    Failed(String),
}

/// What the home screen wants the app to do.
#[derive(Debug, Clone)]
pub enum HomeAction {
    /// Nothing.
    None,
    /// Quit (the app asks first if a job runs).
    Quit,
    /// Show the help.
    Help,
    /// Open the item.
    Open(HomeItem),
    /// Go back to the running job.
    ToJob,
    /// Say something.
    Status(String),
}

enum ListState {
    Loading,
    Ready,
    Failed(String),
}

/// The home screen.
pub struct Home {
    /// The devices.
    pub entries: Vec<DeviceEntry>,
    state: ListState,
    loading: Option<Pending<Result<Vec<DeviceEntry>, String>>>,
    /// Selection among [`Home::items`].
    pub nav: ListNav,
    /// What is known about each device.
    pub details: HashMap<PathBuf, Detail>,
    jobs: Vec<(PathBuf, Pending<Result<DeviceDetails, String>>)>,
    /// Scroll of the detail pane.
    pub detail_scroll: u16,
    fixed: bool,
}

impl Home {
    /// The home screen; the list of devices is read in the background.
    pub fn new(cfg: &Config) -> Home {
        let mut h = Home {
            entries: Vec::new(),
            state: ListState::Loading,
            loading: None,
            nav: ListNav::new(),
            details: HashMap::new(),
            jobs: Vec::new(),
            detail_scroll: 0,
            fixed: matches!(cfg.devices, DeviceSource::Fixed(_)),
        };
        h.refresh(cfg);
        h
    }

    /// Look for devices again.
    pub fn refresh(&mut self, cfg: &Config) {
        self.details.clear();
        self.jobs.clear();
        match &cfg.devices {
            DeviceSource::Fixed(list) => {
                self.entries = list
                    .iter()
                    .map(|d| DeviceEntry::new(d.info.clone(), d.needs_root))
                    .collect();
                self.state = ListState::Ready;
                self.loading = None;
                self.nav.clamp(self.items().len());
                self.load_wanted(cfg, true);
            }
            DeviceSource::System => {
                self.state = ListState::Loading;
                let is_root = cfg.is_root;
                self.loading = Some(Pending::spawn(move || {
                    let list = devices::list(false).map_err(|e| e.message().to_string())?;
                    Ok(list
                        .into_iter()
                        .map(|info| {
                            let needs = !is_root && helper::needs_privilege(&info.path);
                            DeviceEntry::new(info, needs)
                        })
                        .collect())
                }));
            }
        }
    }

    /// The rows in order.
    pub fn items(&self) -> Vec<HomeItem> {
        let mut v: Vec<HomeItem> = (0..self.entries.len()).map(HomeItem::Device).collect();
        v.push(HomeItem::OpenImage);
        v.push(HomeItem::RepairFiles);
        v.push(HomeItem::OpenResults);
        v
    }

    /// The selected row.
    pub fn selected(&self) -> HomeItem {
        let items = self.items();
        items[self.nav.sel.min(items.len() - 1)]
    }

    /// The selected device.
    pub fn selected_device(&self) -> Option<&DeviceEntry> {
        match self.selected() {
            HomeItem::Device(i) => self.entries.get(i),
            _ => None,
        }
    }

    /// True while the device list is being read.
    pub fn is_loading(&self) -> bool {
        matches!(self.state, ListState::Loading)
    }

    /// Pick up finished work; start reading the selected device's details
    /// once the selection has rested on it for a moment.
    pub fn tick(&mut self, cfg: &Config) {
        if let Some(p) = &mut self.loading {
            match p.poll() {
                Some(Ok(Ok(list))) => {
                    self.entries = list;
                    self.state = ListState::Ready;
                    self.loading = None;
                    self.nav.clamp(self.items().len());
                }
                Some(Ok(Err(e))) | Some(Err(e)) => {
                    self.state = ListState::Failed(e);
                    self.loading = None;
                }
                None => {}
            }
        }
        let mut done = Vec::new();
        for (i, (path, p)) in self.jobs.iter_mut().enumerate() {
            if let Some(r) = p.poll() {
                done.push((i, path.clone(), r));
            }
        }
        for (i, path, r) in done.into_iter().rev() {
            self.jobs.remove(i);
            let d = match r {
                Ok(Ok(d)) => {
                    // What the drive says about itself may change how it is described.
                    if let Some(e) = self.entries.iter_mut().find(|e| e.info.path == path) {
                        e.info.health = d.health.clone();
                        e.failing = crate::model::is_failing(&e.info);
                    }
                    Detail::Ready(Box::new(d))
                }
                Ok(Err(e)) | Err(e) => Detail::Failed(e),
            };
            self.details.insert(path, d);
        }
        self.load_wanted(cfg, false);
    }

    /// With a fixed list (tests) every device is described from what is
    /// already known; nothing is opened.
    fn load_wanted(&mut self, _cfg: &Config, _now: bool) {
        if !self.fixed {
            return;
        }
        let Some(entry) = self.selected_device().cloned() else {
            return;
        };
        let path = entry.info.path.clone();
        if self.details.contains_key(&path) {
            return;
        }
        let d = DeviceDetails {
            health: entry.info.health.clone(),
            mapfile: None,
            map_stats: None,
            table: None,
            volumes: Vec::new(),
            notes: Vec::new(),
            open_error: None,
            profile: diskrescue_core::profile::ImagingProfile::for_kind(entry.info.kind),
            info: entry.info,
        };
        self.details.insert(path, Detail::Ready(Box::new(d)));
    }

    /// Read the partition table of the selected device and ask it for its
    /// health. Only on request: a failing drive should not be read more
    /// than it has to be.
    pub fn look_closer(&mut self) -> bool {
        let Some(entry) = self.selected_device() else {
            return false;
        };
        let path = entry.info.path.clone();
        if self.fixed || self.details.contains_key(&path) {
            return false;
        }
        self.details.insert(path.clone(), Detail::Loading);
        let p = path.clone();
        self.jobs.push((
            path,
            Pending::spawn(move || devices::details(&p, None).map_err(|e| e.message().to_string())),
        ));
        true
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent, cfg: &Config, job_running: bool) -> HomeAction {
        let len = self.items().len();
        let mut moved = false;
        match key.code {
            KeyCode::Char('q') => return HomeAction::Quit,
            KeyCode::Char('?') | KeyCode::F(1) => return HomeAction::Help,
            KeyCode::Up | KeyCode::Char('k') => {
                self.nav.move_by(-1, len);
                moved = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.nav.move_by(1, len);
                moved = true;
            }
            KeyCode::PageUp => {
                self.nav.move_by(-5, len);
                moved = true;
            }
            KeyCode::PageDown => {
                self.nav.move_by(5, len);
                moved = true;
            }
            KeyCode::Home => {
                self.nav.select(0, len);
                moved = true;
            }
            KeyCode::End => {
                self.nav.select(len - 1, len);
                moved = true;
            }
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Char('r') => {
                self.refresh(cfg);
                return HomeAction::Status("Looking for devices again...".into());
            }
            KeyCode::Char('d') => {
                return if self.look_closer() {
                    HomeAction::Status(
                        "Reading the partition table and asking the drive for its health...".into(),
                    )
                } else {
                    HomeAction::None
                };
            }
            KeyCode::Char('i') => return HomeAction::Open(HomeItem::OpenImage),
            KeyCode::Char('f') => return HomeAction::Open(HomeItem::RepairFiles),
            KeyCode::Char('p') => return HomeAction::Open(HomeItem::OpenResults),
            KeyCode::Char('g') if job_running => return HomeAction::ToJob,
            KeyCode::Enter => return HomeAction::Open(self.selected()),
            _ => {}
        }
        if moved {
            self.detail_scroll = 0;
        }
        HomeAction::None
    }

    /// Select a row by clicking; a second click opens it.
    pub fn click_row(&mut self, index: usize) -> HomeAction {
        let len = self.items().len();
        if index >= len {
            return HomeAction::None;
        }
        if self.nav.sel == index {
            return HomeAction::Open(self.selected());
        }
        self.nav.sel = index;
        self.detail_scroll = 0;
        HomeAction::None
    }

    /// Draw the screen body. Returns the list area, the first visible row
    /// and the height of one row (for mouse hit-testing).
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme, cfg: &Config) -> (Rect, usize) {
        let [left, right] = if area.width >= 90 {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area)
        } else {
            Layout::horizontal([Constraint::Length(42), Constraint::Min(30)]).areas(area)
        };
        // The list: two lines per row.
        let block = theme.block(" Disks, cards and USB drives ");
        let inner = block.inner(left);
        f.render_widget(block, left);
        let w = inner.width as usize;
        let mut rows: Vec<[Line<'static>; 2]> = Vec::new();
        for item in self.items() {
            rows.push(match item {
                HomeItem::Device(i) => device_row(&self.entries[i], w, theme),
                HomeItem::OpenImage => {
                    special_row("Open an image file", "Recover from a disk image", theme)
                }
                HomeItem::RepairFiles => special_row(
                    "Repair or check files",
                    "Fix photos/videos that will not open",
                    theme,
                ),
                HomeItem::OpenResults => {
                    special_row("Open previous results", "Browse an earlier recovery", theme)
                }
            });
        }
        let mut list_area = inner;
        let mut status: Option<Line<'static>> = None;
        match &self.state {
            ListState::Loading => {
                status = Some(Line::from(Span::styled(
                    "Looking for disks, cards and USB drives...",
                    theme.dim(),
                )))
            }
            ListState::Failed(e) => {
                status = Some(Line::from(Span::styled(
                    format!("Could not list the devices: {e}"),
                    theme.bad(),
                )))
            }
            ListState::Ready if self.entries.is_empty() => {
                status = Some(Line::from(Span::styled(
                    "No disks, cards or USB drives found. Plug one in and press r.",
                    theme.warn(),
                )))
            }
            ListState::Ready => {}
        }
        if let Some(s) = status {
            let [a, b] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
            let wrapped: Vec<Line<'static>> = wrap_words(
                &s.spans
                    .iter()
                    .map(|x| x.content.to_string())
                    .collect::<String>(),
                w,
            )
            .into_iter()
            .map(|l| Line::from(Span::styled(l, s.spans[0].style)))
            .collect();
            f.render_widget(Paragraph::new(wrapped), a);
            list_area = b;
        }
        let off = crate::widgets::render_items(f, list_area, &rows, &self.nav, theme, true);
        // The details.
        let block = theme.block(" About the selection ");
        let inner = pad(block.inner(right));
        f.render_widget(block, right);
        let lines = self.detail_lines(inner.width as usize, theme, cfg);
        let max = lines.len().saturating_sub(inner.height as usize) as u16;
        let scroll = self.detail_scroll.min(max);
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
        (list_area, off)
    }

    fn detail_lines(&self, w: usize, theme: &Theme, cfg: &Config) -> Vec<Line<'static>> {
        let mut v: Vec<Line<'static>> = Vec::new();
        let para = |v: &mut Vec<Line<'static>>, text: &str, style: ratatui::style::Style| {
            for l in wrap_words(text, w) {
                v.push(Line::from(Span::styled(l, style)));
            }
        };
        match self.selected() {
            HomeItem::OpenImage => {
                v.push(Line::from(Span::styled("Open an image file", theme.bold())));
                v.push(Line::raw(""));
                para(
                    &mut v,
                    "A disk image is a file that holds a copy of a whole disk or card (usually ending in .img, .dd or .raw). Choose one to recover photos and videos from it, or to preview what it holds. The image is only read, never changed.",
                    ratatui::style::Style::default(),
                );
                v.push(Line::raw(""));
                para(&mut v, "No administrator rights needed.", theme.dim());
            }
            HomeItem::RepairFiles => {
                v.push(Line::from(Span::styled(
                    "Repair or check files",
                    theme.bold(),
                )));
                v.push(Line::raw(""));
                para(
                    &mut v,
                    "Choose photos or videos you already have: a video that will not play, a photo that shows half grey. DiskRescue checks them and repairs what can be repaired into a new folder. Your originals are never changed.",
                    ratatui::style::Style::default(),
                );
                v.push(Line::raw(""));
                para(&mut v, "No administrator rights needed.", theme.dim());
            }
            HomeItem::OpenResults => {
                v.push(Line::from(Span::styled(
                    "Open previous results",
                    theme.bold(),
                )));
                v.push(Line::raw(""));
                para(
                    &mut v,
                    "Choose the output folder of an earlier recovery (the one with report.json in it) to browse its files again: damaged ones, repaired ones, deleted ones.",
                    ratatui::style::Style::default(),
                );
            }
            HomeItem::Device(i) => {
                let Some(e) = self.entries.get(i) else {
                    return v;
                };
                self.device_lines(e, w, theme, cfg, &mut v);
            }
        }
        v
    }

    fn device_lines(
        &self,
        e: &DeviceEntry,
        w: usize,
        theme: &Theme,
        cfg: &Config,
        v: &mut Vec<Line<'static>>,
    ) {
        let kv = |v: &mut Vec<Line<'static>>, k: &str, val: &str| {
            for (n, l) in wrap_words(val, w.saturating_sub(10).max(8))
                .into_iter()
                .enumerate()
            {
                v.push(Line::from(vec![
                    Span::styled(
                        if n == 0 {
                            format!("{k:<9} ")
                        } else {
                            " ".repeat(10)
                        },
                        theme.dim(),
                    ),
                    Span::raw(l),
                ]));
            }
        };
        let info = &e.info;
        v.push(Line::from(Span::styled(
            format!("{}  {}", info.path.display(), e.name()),
            theme.bold(),
        )));
        kv(
            v,
            "Type",
            &format!(
                "{} ({})",
                info.kind.display_name(),
                transport_label(info.transport)
            ),
        );
        kv(v, "Size", &size_label(info.size));
        if let Some(s) = &info.serial {
            kv(v, "Serial", s);
        }
        let flags = e.flags();
        if !flags.is_empty() {
            let mut spans = vec![Span::styled(format!("{:<9} ", "Flags"), theme.dim())];
            for f in flags {
                let st = match f {
                    "system disk" | "failing" => theme.bad(),
                    "mounted" | "needs root" => theme.warn(),
                    _ => theme.dim(),
                };
                spans.push(Span::styled(format!("[{f}] "), st));
            }
            v.push(Line::from(spans));
        }
        v.push(Line::raw(""));
        if e.needs_root {
            for l in wrap_words(
                "Reading this device needs administrator rights. To recover from it, close DiskRescue and start it again with: sudo diskrescue-tui. 'Copy to an image only' and 'Rescue' can ask for your password from here.",
                w,
            ) {
                v.push(Line::from(Span::styled(l, theme.warn())));
            }
            v.push(Line::raw(""));
        }
        let advice = devices::advice(info);
        if e.failing && !e.system {
            for l in wrap_words(
                "This drive looks like it is failing. Rescue is the safest choice: it copies the drive first, reading it as gently as possible, and recovers from the copy.",
                w,
            ) {
                v.push(Line::from(Span::styled(l, theme.warn())));
            }
            v.push(Line::raw(""));
        }
        if advice.is_empty() && e.failing {
            // The warning above says it.
        } else if advice.is_empty() {
            for l in wrap_words("Nothing unusual was found about this device.", w) {
                v.push(Line::from(Span::styled(l, theme.good())));
            }
        } else {
            v.push(Line::from(Span::styled("What to know", theme.bold())));
            for a in advice {
                let st = if e.system || e.failing {
                    theme.warn()
                } else {
                    ratatui::style::Style::default()
                };
                let mut first = true;
                for l in wrap_words(&a, w.saturating_sub(2)) {
                    v.push(Line::from(Span::styled(
                        format!("{}{l}", if first { "- " } else { "  " }),
                        st,
                    )));
                    first = false;
                }
            }
        }
        // Health.
        let health = self
            .details
            .get(&info.path)
            .and_then(|d| match d {
                Detail::Ready(d) => d.health.clone(),
                _ => None,
            })
            .or_else(|| info.health.clone());
        if let Some(h) = health {
            v.push(Line::raw(""));
            v.push(Line::from(Span::styled("Health", theme.bold())));
            for line in devices::health_lines(&h) {
                for l in wrap_words(&line, w.saturating_sub(2)) {
                    v.push(Line::raw(format!("  {l}")));
                }
            }
        }
        // Partitions and filesystems.
        v.push(Line::raw(""));
        v.push(Line::from(Span::styled(
            "Partitions and filesystems",
            theme.bold(),
        )));
        let detail = self.details.get(&info.path);
        match detail {
            Some(Detail::Ready(d)) if !d.volumes.is_empty() => {
                for vol in &d.volumes {
                    let fs = vol
                        .probe
                        .as_ref()
                        .map(|p| {
                            let label = p
                                .label
                                .as_deref()
                                .map(|l| format!(" \"{l}\""))
                                .unwrap_or_default();
                            format!("{}{label}", p.kind.name())
                        })
                        .unwrap_or_else(|| "no filesystem recognised".to_string());
                    let line = format!(
                        "{}. {}, {} - {}",
                        vol.entry.index,
                        text::bytes(vol.entry.size),
                        fs,
                        vol.entry.type_desc
                    );
                    for l in wrap_words(&line, w.saturating_sub(2)) {
                        v.push(Line::raw(format!("  {l}")));
                    }
                }
                if let Some(t) = &d.table {
                    v.push(Line::from(Span::styled(
                        format!("  partitioning: {}", text::scheme_label(t.scheme)),
                        theme.dim(),
                    )));
                }
            }
            _ => {
                if info.partitions.is_empty() {
                    v.push(Line::from(Span::styled(
                        "  none reported by the system",
                        theme.dim(),
                    )));
                }
                for p in &info.partitions {
                    let mounted = if p.mountpoints.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", mounted at {}",
                            p.mountpoints
                                .iter()
                                .map(|m| m.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    let line = format!(
                        "{}. {}, {}{mounted}",
                        p.number,
                        text::bytes(p.size),
                        p.fs_type.as_deref().unwrap_or("unknown filesystem")
                    );
                    for l in wrap_words(&line, w.saturating_sub(2)) {
                        v.push(Line::raw(format!("  {l}")));
                    }
                }
            }
        }
        match detail {
            Some(Detail::Loading) => v.push(Line::from(Span::styled(
                "  reading the partition table...",
                theme.dim(),
            ))),
            Some(Detail::Failed(m)) => {
                for l in wrap_words(m, w.saturating_sub(2)) {
                    v.push(Line::from(Span::styled(format!("  {l}"), theme.warn())));
                }
            }
            Some(Detail::Ready(d)) => {
                if let Some(m) = &d.open_error {
                    let msg = if e.needs_root && !cfg.is_root {
                        "The partition layout needs administrator rights to read.".to_string()
                    } else {
                        m.clone()
                    };
                    for l in wrap_words(&msg, w.saturating_sub(2)) {
                        v.push(Line::from(Span::styled(format!("  {l}"), theme.dim())));
                    }
                }
            }
            None => {
                if !self.fixed {
                    for l in wrap_words(
                        "Not read yet. Press d to read its partition table and ask the drive for its health. Skip this for a drive that clicks or is very slow: Rescue reads it as gently as possible.",
                        w.saturating_sub(2),
                    ) {
                        v.push(Line::from(Span::styled(format!("  {l}"), theme.dim())));
                    }
                }
            }
        }
    }
}

/// One column of space inside a box.
fn pad(r: Rect) -> Rect {
    Rect {
        x: r.x + 1,
        width: r.width.saturating_sub(2),
        ..r
    }
}

fn device_row(e: &DeviceEntry, w: usize, theme: &Theme) -> [Line<'static>; 2] {
    let info = &e.info;
    let first = format!(
        "{}  {}  {}",
        info.path.display(),
        text::bytes(info.size),
        info.kind.display_name()
    );
    let mut second = vec![Span::styled(
        format!(
            "   {}",
            ellipsize_end(&e.name(), w.saturating_sub(4).min(24))
        ),
        theme.dim(),
    )];
    for f in e.flags() {
        if f == "mounted" && e.system {
            continue;
        }
        let st = match f {
            "system disk" | "failing" => theme.bad(),
            "mounted" | "needs root" => theme.warn(),
            _ => theme.dim(),
        };
        second.push(Span::styled(format!(" [{f}]"), st));
    }
    [
        Line::from(Span::raw(ellipsize_end(&format!(" {first}"), w))),
        Line::from(second),
    ]
}

fn special_row(title: &str, hint: &str, theme: &Theme) -> [Line<'static>; 2] {
    [
        Line::from(Span::styled(format!(" {title}"), theme.accent())),
        Line::from(Span::styled(format!("   {hint}"), theme.dim())),
    ]
}
