//! The application: a state machine that takes [`Event`]s and draws
//! itself, so that tests can drive it without a terminal.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use diskrescue_app::devices;
use diskrescue_app::helper;
use diskrescue_app::plan::{RepairPlan, VerifyPlan};
use diskrescue_app::JobOutcome;
use diskrescue_core::device::DeviceKind;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::config::{Config, DeviceSource, Opener};
use crate::event::Event;
use crate::fsbrowse::{PickMode, Picker};
use crate::model::{DeviceEntry, Overrides, Plan};
use crate::pending::Pending;
use crate::screens::form::{FieldId, Form, FormAction, FormKind};
use crate::screens::help::{footer_for, help_lines, ScreenId};
use crate::screens::home::{Home, HomeAction, HomeItem};
use crate::screens::menu::{Menu, MenuAction};
use crate::screens::pick::{PickAction, PickPurpose, PickScreen};
use crate::screens::preflight::{PfAction, PreflightScreen};
use crate::screens::results::{DonorAction, DonorScreen, ResAction, ResultsData, ResultsScreen};
use crate::screens::run::{Next, Run};
use crate::screens::survey::{ListAction, ScanScreen, VerifyScreen};
use crate::widgets::{centered, key_hints, wrap_words, Theme};

/// Narrowest window the app draws in.
pub const MIN_WIDTH: u16 = 80;
/// Lowest window the app draws in.
pub const MIN_HEIGHT: u16 = 24;

const STATUS_TIME: Duration = Duration::from_secs(8);

/// The screen shown.
pub enum Screen {
    /// The list of devices.
    Home,
    /// What to do with a device.
    Menu(Box<Menu>),
    /// A form.
    Form(Box<Form>),
    /// The file browser.
    Pick(Box<PickScreen>),
    /// The check before starting.
    Preflight(Box<PreflightScreen>),
    /// The running job (held by the app).
    Run,
    /// The results browser.
    Results(Box<ResultsScreen>),
    /// The reference file chooser.
    Donor(Box<DonorScreen>),
    /// The preview.
    Scan(Box<ScanScreen>),
    /// The verdicts of a check.
    Verify(Box<VerifyScreen>),
}

impl Screen {
    fn id(&self) -> ScreenId {
        match self {
            Screen::Home => ScreenId::Home,
            Screen::Menu(_) => ScreenId::Menu,
            Screen::Form(_) => ScreenId::Form,
            Screen::Pick(_) => ScreenId::Pick,
            Screen::Preflight(_) => ScreenId::Preflight,
            Screen::Run => ScreenId::Run,
            Screen::Results(_) => ScreenId::Results,
            Screen::Donor(_) => ScreenId::Donor,
            Screen::Scan(_) => ScreenId::Scan,
            Screen::Verify(_) => ScreenId::Verify,
        }
    }

    fn typing(&self) -> bool {
        match self {
            Screen::Form(f) => f.typing(),
            Screen::Pick(p) => p.typing(),
            Screen::Preflight(p) => p.typing(),
            _ => false,
        }
    }
}

/// A question with yes and no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Stop the job and keep what it has.
    StopAndSave,
    /// Quit while a job runs: stop it, save, then leave.
    QuitWithJob,
    /// Quit.
    Quit,
}

/// A question with a few answers.
#[derive(Debug, Clone)]
pub enum ChoiceKind {
    /// Loose files were chosen: repair them or only check them.
    RepairOrCheck(Vec<PathBuf>),
}

/// What is drawn over the screen.
pub enum Overlay {
    /// The keys.
    Help,
    /// A yes or no question.
    Confirm {
        /// What it is about.
        kind: ConfirmKind,
        /// The question.
        text: String,
    },
    /// A choice between a few things.
    Choice {
        /// The heading.
        title: String,
        /// The answers.
        options: Vec<String>,
        /// The highlighted answer.
        sel: usize,
        /// What the answer is for.
        kind: ChoiceKind,
    },
    /// Something to read.
    Message {
        /// The heading.
        title: String,
        /// The text.
        lines: Vec<String>,
    },
}

#[derive(Default)]
struct Hits {
    list: Option<(Rect, usize, usize)>,
    tabs: Vec<Rect>,
}

enum Loading {
    Results(Pending<Result<ResultsData, String>>),
}

/// The application.
pub struct App {
    cfg: Config,
    home: Home,
    screen: Screen,
    stack: Vec<Screen>,
    run: Option<Run>,
    overlay: Option<Overlay>,
    status: Option<(String, Instant)>,
    size: (u16, u16),
    quit: bool,
    exit_code: i32,
    interrupts: u32,
    quit_when_done: bool,
    loading: Option<Loading>,
    hits: RefCell<Hits>,
    last_folder: Option<PathBuf>,
}

impl App {
    /// The app for the machine described by `cfg`.
    pub fn new(cfg: Config) -> App {
        let home = Home::new(&cfg);
        App {
            cfg,
            home,
            screen: Screen::Home,
            stack: Vec::new(),
            run: None,
            overlay: None,
            status: None,
            size: (MIN_WIDTH, MIN_HEIGHT),
            quit: false,
            exit_code: 0,
            interrupts: 0,
            quit_when_done: false,
            loading: None,
            hits: RefCell::new(Hits::default()),
            last_folder: None,
        }
    }

    // ------------------------------------------------------------ queries

    /// The app wants to end.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The exit code: 0, or 130 after a forced quit.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// Which screen is shown.
    pub fn screen_id(&self) -> ScreenId {
        self.screen.id()
    }

    /// The current screen.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The job, if one was started.
    pub fn run(&self) -> Option<&Run> {
        self.run.as_ref()
    }

    /// The home screen's state.
    pub fn home(&self) -> &Home {
        &self.home
    }

    /// The open overlay.
    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// The folder "Open folder" was last asked for.
    pub fn last_folder(&self) -> Option<&Path> {
        self.last_folder.as_deref()
    }

    /// The message shown at the bottom, if any.
    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().map(|(s, _)| s.as_str())
    }

    /// A job is running.
    pub fn job_running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.is_running())
    }

    /// The window is smaller than the app can draw in.
    pub fn too_small(&self) -> bool {
        self.size.0 < MIN_WIDTH || self.size.1 < MIN_HEIGHT
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    // ------------------------------------------------------------- events

    /// Tell the app the size of the window.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.size = (w, h);
    }

    /// React to an event.
    pub fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Resize(w, h) => self.size = (w, h),
            Event::Tick => self.on_tick(),
            Event::Interrupt => self.interrupt(),
            Event::Key(k) => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
        }
    }

    fn interrupt(&mut self) {
        if self.job_running() {
            self.interrupts += 1;
            if self.interrupts == 1 {
                if let Some(r) = self.run.as_mut() {
                    r.stop();
                }
                self.set_status(
                    "Stopping and saving what has been done... Press Ctrl-C again to quit right now.",
                );
            } else {
                self.quit = true;
                self.exit_code = 130;
            }
        } else {
            self.quit = true;
        }
    }

    fn on_tick(&mut self) {
        self.home.tick(&self.cfg);
        let mut auto: Option<(Plan, bool)> = None;
        if let Screen::Preflight(p) = &mut self.screen {
            p.poll();
            if let Some(r) = p.result() {
                if r.is_clear()
                    && r.warnings.is_empty()
                    && matches!(p.plan, Plan::Scan(_) | Plan::Verify(_))
                {
                    auto = Some((p.plan.clone(), p.privileged));
                }
            }
        }
        if let Some((plan, privileged)) = auto {
            self.begin_run(plan, Overrides::default(), privileged);
        }
        let mut finished = false;
        if let Some(r) = self.run.as_mut() {
            if r.is_running() {
                r.tick();
                finished = !r.is_running();
            }
        }
        if finished {
            self.on_job_finished();
        }
        if let Some(Loading::Results(p)) = &mut self.loading {
            if let Some(res) = p.poll() {
                self.loading = None;
                match res {
                    Ok(Ok(data)) => {
                        let title = data.output.display().to_string();
                        self.show_results(Arc::new(data), title);
                    }
                    Ok(Err(m)) | Err(m) => self.message("These results cannot be opened", vec![m]),
                }
            }
        }
        if self.quit_when_done && !self.job_running() {
            self.quit = true;
        }
        if let Some((_, t)) = &self.status {
            if t.elapsed() > STATUS_TIME {
                self.status = None;
            }
        }
    }

    fn on_job_finished(&mut self) {
        let Some(r) = &self.run else { return };
        let msg = match &r.outcome {
            Some(JobOutcome::Finished) => format!("{} is finished.", r.title),
            Some(JobOutcome::Cancelled) => {
                format!("{} was stopped; what was done is saved.", r.title)
            }
            Some(JobOutcome::Refused(_)) => format!("{} was not started.", r.title),
            Some(JobOutcome::Failed(_)) => format!("{} did not finish.", r.title),
            None => return,
        };
        if !matches!(self.screen, Screen::Run) {
            self.set_status(format!("{msg} Press g on the home screen to look."));
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            self.interrupt();
            return;
        }
        if self.too_small() {
            return;
        }
        if let Some(ov) = self.overlay.take() {
            self.overlay_key(ov, key);
            return;
        }
        self.status = None;
        let typing = self.screen.typing();
        if !typing {
            match key.code {
                KeyCode::Char('?') | KeyCode::F(1) => {
                    self.overlay = Some(Overlay::Help);
                    return;
                }
                KeyCode::Char('q') if !matches!(self.screen, Screen::Home) => {
                    self.quit_request();
                    return;
                }
                _ => {}
            }
        } else if key.code == KeyCode::F(1) {
            self.overlay = Some(Overlay::Help);
            return;
        }
        match &mut self.screen {
            Screen::Home => {
                let running = self.run.is_some();
                let a = self.home.handle_key(&key, &self.cfg, running);
                self.on_home(a);
            }
            Screen::Menu(m) => {
                let a = m.handle_key(&key);
                self.on_menu(a);
            }
            Screen::Form(f) => {
                let a = f.handle_key(&key);
                self.on_form(a);
            }
            Screen::Pick(p) => {
                let a = p.handle_key(&key);
                self.on_pick(a);
            }
            Screen::Preflight(p) => {
                let a = p.handle_key(&key);
                self.on_preflight(a);
            }
            Screen::Run => self.on_run_key(key),
            Screen::Results(r) => {
                let a = r.handle_key(&key);
                self.on_results(a);
            }
            Screen::Donor(d) => {
                let a = d.handle_key(&key);
                self.on_donor(a);
            }
            Screen::Scan(s) => {
                let a = s.handle_key(&key);
                self.on_list(a);
            }
            Screen::Verify(v) => {
                let a = v.handle_key(&key);
                self.on_list(a);
            }
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if self.too_small() || self.overlay.is_some() {
            return;
        }
        let (col, row) = (m.column, m.row);
        let (list, tabs) = {
            let h = self.hits.borrow();
            (h.list, h.tabs.clone())
        };
        let inside =
            |r: Rect| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Screen::Results(r) = &mut self.screen {
                    if let Some(i) = tabs.iter().position(|t| inside(*t)) {
                        r.click_tab(i);
                        return;
                    }
                }
                let Some((area, offset, item_h)) = list else {
                    return;
                };
                if !inside(area) {
                    return;
                }
                let index = offset + (row - area.y) as usize / item_h.max(1);
                match &mut self.screen {
                    Screen::Home => {
                        let a = self.home.click_row(index);
                        self.on_home(a);
                    }
                    Screen::Menu(m) => {
                        let a = m.click_row(index);
                        self.on_menu(a);
                    }
                    Screen::Pick(p) => {
                        let a = p.click_row(index);
                        self.on_pick(a);
                    }
                    Screen::Results(r) => r.click_row(index),
                    Screen::Donor(d) => {
                        let a = d.click_row(index);
                        self.on_donor(a);
                    }
                    Screen::Scan(s) => s.click_row(index),
                    Screen::Verify(v) => v.click_row(index),
                    _ => {}
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta: isize = if m.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                let Some((area, _, item_h)) = list else {
                    return;
                };
                let height = area.height as usize / item_h.max(1);
                match &mut self.screen {
                    Screen::Home => self.home.nav.scroll(delta, height, self.home.items().len()),
                    Screen::Pick(p) => p.wheel(delta, height),
                    Screen::Results(r) => r.wheel(delta, height),
                    Screen::Scan(s) => s.wheel(delta, height),
                    Screen::Verify(v) => v.wheel(delta, height),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ overlays

    fn message(&mut self, title: &str, lines: Vec<String>) {
        self.overlay = Some(Overlay::Message {
            title: title.to_string(),
            lines,
        });
    }

    fn overlay_key(&mut self, ov: Overlay, key: KeyEvent) {
        match ov {
            Overlay::Help | Overlay::Message { .. } => {
                // Any key closes it.
            }
            Overlay::Confirm { kind, text } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirmed(kind),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {}
                _ => self.overlay = Some(Overlay::Confirm { kind, text }),
            },
            Overlay::Choice {
                title,
                options,
                mut sel,
                kind,
            } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.saturating_sub(1);
                    self.overlay = Some(Overlay::Choice {
                        title,
                        options,
                        sel,
                        kind,
                    });
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(options.len() - 1);
                    self.overlay = Some(Overlay::Choice {
                        title,
                        options,
                        sel,
                        kind,
                    });
                }
                KeyCode::Enter => self.chosen(kind, sel),
                KeyCode::Char(c @ '1'..='9') if ((c as u8 - b'1') as usize) < options.len() => {
                    self.chosen(kind, (c as u8 - b'1') as usize)
                }
                KeyCode::Esc => {}
                _ => {
                    self.overlay = Some(Overlay::Choice {
                        title,
                        options,
                        sel,
                        kind,
                    })
                }
            },
        }
    }

    fn confirmed(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::StopAndSave => {
                if let Some(r) = self.run.as_mut() {
                    r.stop();
                }
                self.set_status("Stopping and saving what has been done...");
            }
            ConfirmKind::QuitWithJob => {
                if let Some(r) = self.run.as_mut() {
                    r.stop();
                }
                self.quit_when_done = true;
                self.set_status(
                    "Stopping and saving. DiskRescue closes by itself when that is done (Ctrl-C quits right now).",
                );
            }
            ConfirmKind::Quit => self.quit = true,
        }
    }

    fn chosen(&mut self, kind: ChoiceKind, sel: usize) {
        match kind {
            ChoiceKind::RepairOrCheck(files) => {
                if sel == 0 {
                    let form = Form::new(FormKind::Repair, None, files, &self.cfg);
                    self.screen = Screen::Form(Box::new(form));
                } else {
                    self.start_checks(Plan::Verify(VerifyPlan { files }), false);
                }
            }
        }
    }

    fn quit_request(&mut self) {
        if self.job_running() {
            self.overlay = Some(Overlay::Confirm {
                kind: ConfirmKind::QuitWithJob,
                text: "A job is still running. Stop it, save what it has done, and quit?".into(),
            });
        } else if matches!(self.screen, Screen::Home) {
            self.quit = true;
        } else {
            self.overlay = Some(Overlay::Confirm {
                kind: ConfirmKind::Quit,
                text: "Quit DiskRescue?".into(),
            });
        }
    }

    // --------------------------------------------------------- navigation

    fn push(&mut self, next: Screen) {
        let old = std::mem::replace(&mut self.screen, next);
        self.stack.push(old);
    }

    fn back(&mut self) {
        self.screen = self.stack.pop().unwrap_or(Screen::Home);
    }

    fn go_home(&mut self) {
        self.stack.clear();
        self.screen = Screen::Home;
    }

    fn after_run_nav(&mut self, next: Screen) {
        self.stack
            .retain(|s| matches!(s, Screen::Results(_) | Screen::Scan(_) | Screen::Verify(_)));
        self.screen = next;
    }

    fn show_results(&mut self, data: Arc<ResultsData>, title: String) {
        let screen = ResultsScreen::new(data, title);
        if matches!(self.screen, Screen::Run) {
            self.after_run_nav(Screen::Results(Box::new(screen)));
        } else {
            self.screen = Screen::Results(Box::new(screen));
        }
    }

    // ------------------------------------------------------------ handlers

    fn on_home(&mut self, a: HomeAction) {
        match a {
            HomeAction::None => {}
            HomeAction::Quit => self.quit_request(),
            HomeAction::Help => self.overlay = Some(Overlay::Help),
            HomeAction::Status(s) => self.set_status(s),
            HomeAction::ToJob => {
                if self.run.is_some() {
                    self.screen = Screen::Run;
                }
            }
            HomeAction::Open(item) => self.open_home_item(item),
        }
    }

    fn open_home_item(&mut self, item: HomeItem) {
        match item {
            HomeItem::Device(i) => {
                if let Some(e) = self.home.entries.get(i).cloned() {
                    self.push(Screen::Menu(Box::new(Menu::new(e))));
                }
            }
            HomeItem::OpenImage => {
                let picker = Picker::new(
                    PickMode::File,
                    "Choose a disk image file (press / to type its path)",
                    &self.cfg.cwd,
                    &self.cfg.home,
                    &self.cfg.cwd,
                );
                self.push(Screen::Pick(Box::new(PickScreen::new(
                    picker,
                    PickPurpose::OpenImage,
                ))));
            }
            HomeItem::RepairFiles => {
                let picker = Picker::new(
                    PickMode::Files,
                    "Choose the photos or videos to repair or check",
                    &self.cfg.cwd,
                    &self.cfg.home,
                    &self.cfg.cwd,
                );
                self.push(Screen::Pick(Box::new(PickScreen::new(
                    picker,
                    PickPurpose::RepairFiles,
                ))));
            }
            HomeItem::OpenResults => {
                let start = self.cfg.home.join("DiskRescue");
                let picker = Picker::new(
                    PickMode::Folder,
                    "Choose the output folder of an earlier recovery (it holds report.json)",
                    &start,
                    &self.cfg.home,
                    &self.cfg.cwd,
                );
                self.push(Screen::Pick(Box::new(PickScreen::new(
                    picker,
                    PickPurpose::OpenResults,
                ))));
            }
        }
    }

    fn on_menu(&mut self, a: MenuAction) {
        match a {
            MenuAction::None => {}
            MenuAction::Back => self.back(),
            MenuAction::Help => self.overlay = Some(Overlay::Help),
            MenuAction::Status(s) => self.message("Not available", vec![s]),
            MenuAction::Form(kind) => {
                let target = match &self.screen {
                    Screen::Menu(m) => m.target.clone(),
                    _ => return,
                };
                let form = Form::new(kind, Some(target), vec![], &self.cfg);
                self.push(Screen::Form(Box::new(form)));
            }
        }
    }

    fn on_form(&mut self, a: FormAction) {
        match a {
            FormAction::None => {}
            FormAction::Back => self.back(),
            FormAction::Help => self.overlay = Some(Overlay::Help),
            FormAction::Browse { id, folder } => self.browse_for(id, folder),
            FormAction::Start(plan) => {
                let plan = *plan;
                let needs_root = match &self.screen {
                    Screen::Form(f) => f.target.as_ref().is_some_and(|t| t.needs_root),
                    _ => false,
                };
                self.start_checks(plan, needs_root);
            }
        }
    }

    fn browse_for(&mut self, id: FieldId, folder: bool) {
        let Screen::Form(f) = &self.screen else {
            return;
        };
        let typed = match id {
            FieldId::Output | FieldId::Image => f.text(id).unwrap_or_default(),
            _ => String::new(),
        };
        let typed_path = crate::fsbrowse::absolute_from(&typed, &self.cfg.home, &self.cfg.cwd);
        let (mode, start, title) = match id {
            FieldId::Donors => (
                PickMode::File,
                self.cfg.cwd.clone(),
                "Choose a healthy photo or video from the same camera or phone",
            ),
            FieldId::Image => (
                PickMode::Folder,
                typed_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default(),
                "Choose the folder for the image file",
            ),
            _ if folder => (
                PickMode::Folder,
                typed_path,
                "Choose the folder for the results (on a different disk than the device)",
            ),
            _ => (PickMode::File, typed_path, "Choose a file"),
        };
        let picker = Picker::new(mode, title, &start, &self.cfg.home, &self.cfg.cwd);
        self.push(Screen::Pick(Box::new(PickScreen::new(
            picker,
            PickPurpose::Field(id),
        ))));
    }

    fn on_pick(&mut self, a: PickAction) {
        match a {
            PickAction::None => {}
            PickAction::Help => self.overlay = Some(Overlay::Help),
            PickAction::Cancel => self.back(),
            PickAction::Chosen(paths) => {
                let purpose = match &self.screen {
                    Screen::Pick(p) => p.purpose.clone(),
                    _ => return,
                };
                self.picked(purpose, paths);
            }
        }
    }

    fn picked(&mut self, purpose: PickPurpose, paths: Vec<PathBuf>) {
        let Some(first) = paths.first().cloned() else {
            return;
        };
        match purpose {
            PickPurpose::Field(id) => {
                self.back();
                if let Screen::Form(f) = &mut self.screen {
                    match id {
                        FieldId::Output => {
                            f.set_text(FieldId::Output, &first.display().to_string())
                        }
                        FieldId::Image => {
                            let name = f
                                .text(FieldId::Image)
                                .and_then(|t| {
                                    Path::new(&t)
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                })
                                .unwrap_or_else(|| "image.img".into());
                            f.set_text(FieldId::Image, &first.join(name).display().to_string());
                        }
                        FieldId::Donors => f.add_donor(first),
                        _ => {}
                    }
                }
            }
            PickPurpose::OpenImage => self.open_target(&first),
            PickPurpose::OpenResults => {
                self.set_status("Reading the report...");
                let dir = first;
                self.loading = Some(Loading::Results(Pending::spawn(move || {
                    ResultsData::load(dir)
                })));
            }
            PickPurpose::RepairFiles => {
                let n = paths.len();
                self.overlay = Some(Overlay::Choice {
                    title: format!(
                        "{} chosen. What now?",
                        diskrescue_app::text::plural(n as u64, "file", "files")
                    ),
                    options: vec![
                        "Repair them (recommended)".into(),
                        "Only check whether they are intact".into(),
                    ],
                    sel: 0,
                    kind: ChoiceKind::RepairOrCheck(paths),
                });
            }
            PickPurpose::Donor { data, target } => {
                self.back();
                self.repair_with_donor(&data, target, first);
            }
        }
    }

    /// Open an image file or device by path: describe it and show its menu.
    pub fn open_target(&mut self, path: &Path) {
        match devices::describe(path) {
            Ok(info) => {
                let needs_root = !self.cfg.is_root
                    && matches!(self.cfg.devices, DeviceSource::System)
                    && info.kind != DeviceKind::ImageFile
                    && helper::needs_privilege(path);
                let entry = DeviceEntry::new(info, needs_root);
                self.screen = Screen::Menu(Box::new(Menu::new(entry)));
            }
            Err(e) => self.message("This cannot be opened", vec![e.message().to_string()]),
        }
    }

    fn start_checks(&mut self, plan: Plan, needs_root: bool) {
        if self.job_running() {
            self.message(
                "Another job is running",
                vec![
                    "Only one job can run at a time. Press b to leave this screen while it keeps running, or stop it first."
                        .into(),
                ],
            );
            return;
        }
        let copies_device = match &plan {
            Plan::Image(_) => true,
            Plan::Recover(p) => p.image_to.is_some(),
            _ => false,
        };
        let privileged =
            self.cfg.allow_elevation && !self.cfg.is_root && needs_root && copies_device;
        let pf = PreflightScreen::new(plan, privileged, self.cfg.is_root);
        self.push(Screen::Preflight(Box::new(pf)));
    }

    fn on_preflight(&mut self, a: PfAction) {
        match a {
            PfAction::None => {}
            PfAction::Back => self.back(),
            PfAction::Help => self.overlay = Some(Overlay::Help),
            PfAction::Start(plan, ov) => {
                let plan = *plan;
                let privileged = match &self.screen {
                    Screen::Preflight(p) => p.privileged,
                    _ => false,
                };
                self.begin_run(plan, ov, privileged);
            }
        }
    }

    fn begin_run(&mut self, plan: Plan, ov: Overrides, privileged: bool) {
        if self.job_running() {
            self.set_status("Another job is still running.");
            return;
        }
        match Run::start(plan, ov, privileged) {
            Ok(run) => {
                self.interrupts = 0;
                self.quit_when_done = false;
                self.run = Some(run);
                self.screen = Screen::Run;
                if let Some(r) = self.run.as_mut() {
                    r.tick();
                }
            }
            Err(msg) => {
                if let Screen::Preflight(p) = &mut self.screen {
                    p.notice = Some(msg);
                } else {
                    self.message("It could not be started", vec![msg]);
                }
            }
        }
    }

    fn on_run_key(&mut self, key: KeyEvent) {
        let Some(run) = self.run.as_mut() else {
            self.go_home();
            return;
        };
        let running = run.is_running();
        match key.code {
            KeyCode::Char('p') | KeyCode::Char(' ') if running => {
                if run.can_pause() {
                    run.toggle_pause();
                } else {
                    self.set_status(
                        "This cannot be paused here. Press s to stop and save; the copy can be continued later.",
                    );
                }
            }
            KeyCode::Char('s') if running => {
                self.overlay = Some(Overlay::Confirm {
                    kind: ConfirmKind::StopAndSave,
                    text: "Stop now and save what has been done? A copy can be continued later from where it stopped."
                        .into(),
                });
            }
            KeyCode::Char('b') => self.go_home(),
            KeyCode::Char('h') if !running => self.go_home(),
            KeyCode::Enter if !running => self.run_next(),
            KeyCode::Char('o') if !running => {
                let out = run.output.clone();
                match out {
                    Some(p) => self.open_folder(&p),
                    None => self.set_status("This job did not write results to a folder."),
                }
            }
            KeyCode::Esc => {
                if running {
                    self.set_status(
                        "Press s to stop and save, or b to leave this screen while the job keeps running.",
                    );
                } else if matches!(
                    run.outcome,
                    Some(JobOutcome::Refused(_)) | Some(JobOutcome::Failed(_))
                ) {
                    self.back();
                } else {
                    self.go_home();
                }
            }
            _ => {}
        }
    }

    fn run_next(&mut self) {
        let Some(run) = self.run.as_ref() else { return };
        match run.next() {
            Next::None => {
                self.set_status("There is nothing to look at. Press h for the home screen.")
            }
            Next::Results { output, report } => {
                let title = output.display().to_string();
                self.show_results(Arc::new(ResultsData::new(output, report)), title);
            }
            Next::Scan { survey, source } => {
                let s = ScanScreen::new(survey, source);
                self.after_run_nav(Screen::Scan(Box::new(s)));
            }
            Next::Verify => {
                if let Some(v) = run.verify_state() {
                    let s = VerifyScreen::new(v.results.clone());
                    self.after_run_nav(Screen::Verify(Box::new(s)));
                }
            }
            Next::Image(path) => {
                self.stack.clear();
                self.open_target(&path);
            }
        }
    }

    fn on_results(&mut self, a: ResAction) {
        match a {
            ResAction::None => {}
            ResAction::Back => self.back(),
            ResAction::Home => self.go_home(),
            ResAction::Help => self.overlay = Some(Overlay::Help),
            ResAction::OpenFolder(p) => self.open_folder(&p),
            ResAction::Status(s) => self.set_status(s),
            ResAction::Donor(i) => {
                let data = match &self.screen {
                    Screen::Results(r) => r.data.clone(),
                    _ => return,
                };
                self.push(Screen::Donor(Box::new(DonorScreen::new(data, i))));
            }
        }
    }

    fn on_donor(&mut self, a: DonorAction) {
        match a {
            DonorAction::None => {}
            DonorAction::Back => self.back(),
            DonorAction::Help => self.overlay = Some(Overlay::Help),
            DonorAction::Chosen(path) => {
                let (data, target) = match &self.screen {
                    Screen::Donor(d) => (d.data.clone(), d.target),
                    _ => return,
                };
                self.repair_with_donor(&data, target, path);
            }
            DonorAction::Browse => {
                let (data, target) = match &self.screen {
                    Screen::Donor(d) => (d.data.clone(), d.target),
                    _ => return,
                };
                let picker = Picker::new(
                    PickMode::File,
                    "Choose a healthy photo or video from the same camera or phone",
                    &data.output,
                    &self.cfg.home,
                    &self.cfg.cwd,
                );
                self.push(Screen::Pick(Box::new(PickScreen::new(
                    picker,
                    PickPurpose::Donor { data, target },
                ))));
            }
        }
    }

    fn repair_with_donor(&mut self, data: &ResultsData, target: usize, donor: PathBuf) {
        let Some(file) = data.items.get(target).and_then(|i| i.path.clone()) else {
            self.set_status("This file was not written, so it cannot be repaired.");
            return;
        };
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let base = data.output.join("manual-repair");
        let mut n = 1;
        let mut out = base.join(format!("{stem}-{n}"));
        while out.exists() {
            n += 1;
            out = base.join(format!("{stem}-{n}"));
        }
        let plan = Plan::Repair(RepairPlan {
            files: vec![file],
            output: out,
            donors: vec![donor],
            aggressive: false,
        });
        self.start_checks(plan, false);
    }

    fn on_list(&mut self, a: ListAction) {
        match a {
            ListAction::None => {}
            ListAction::Back => self.back(),
            ListAction::Home => self.go_home(),
            ListAction::Help => self.overlay = Some(Overlay::Help),
            ListAction::Status(s) => self.set_status(s),
            ListAction::Recover(source) => match devices::describe(&source) {
                Ok(info) => {
                    let entry = DeviceEntry::new(info, false);
                    let form = Form::new(FormKind::Recover, Some(entry), vec![], &self.cfg);
                    self.push(Screen::Form(Box::new(form)));
                }
                Err(e) => self.message("This cannot be opened", vec![e.message().to_string()]),
            },
            ListAction::Repair(files) => {
                let form = Form::new(FormKind::Repair, None, files, &self.cfg);
                self.push(Screen::Form(Box::new(form)));
            }
        }
    }

    /// Show a folder in the file manager, or its path when that cannot be
    /// done (no opener, or running as administrator).
    pub fn open_folder(&mut self, path: &Path) {
        self.last_folder = Some(path.to_path_buf());
        let can_open = self.cfg.opener == Opener::System && !self.cfg.is_root;
        if can_open && open_with_desktop(path) {
            self.set_status(format!("Opened {}", path.display()));
            return;
        }
        let why = if self.cfg.is_root {
            "DiskRescue is running as administrator, so it does not start your file manager."
        } else {
            "No file manager could be started from here."
        };
        self.message(
            "The files are in this folder",
            vec![
                path.display().to_string(),
                String::new(),
                format!(
                    "{why} Open the folder yourself with your file manager, or from a terminal."
                ),
            ],
        );
    }

    // -------------------------------------------------------------- drawing

    /// Draw the whole screen.
    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let theme = self.cfg.theme;
        *self.hits.borrow_mut() = Hits::default();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            self.draw_too_small(f, area, &theme);
            return;
        }
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, head, &theme);
        let body = Rect {
            x: body.x + 1,
            width: body.width.saturating_sub(2),
            ..body
        };
        self.draw_body(f, body, &theme);
        self.draw_footer(f, foot, &theme);
        if let Some(ov) = &self.overlay {
            self.draw_overlay(f, area, ov, &theme);
        }
    }

    fn draw_too_small(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                "DiskRescue needs a bigger window.",
                theme.bold(),
            )),
            Line::raw(format!(
                "It needs at least {MIN_WIDTH} columns by {MIN_HEIGHT} rows."
            )),
            Line::raw(format!("This window is {} by {}.", area.width, area.height)),
            Line::raw("Make the window larger. Nothing is lost while you do."),
        ];
        f.render_widget(Clear, area);
        let w = area.width.max(1) as usize;
        let mut out: Vec<Line<'static>> = Vec::new();
        for l in lines {
            let text: String = l.spans.iter().map(|s| s.content.to_string()).collect();
            for part in wrap_words(&text, w) {
                out.push(Line::from(Span::styled(part, l.spans[0].style)));
            }
        }
        f.render_widget(Paragraph::new(out), area);
    }

    fn title(&self) -> String {
        match &self.screen {
            Screen::Home => "Home".into(),
            Screen::Menu(m) => format!("{}", m.target.info.path.display()),
            Screen::Form(f) => f.kind.title().to_string(),
            Screen::Pick(_) => "Choose".into(),
            Screen::Preflight(_) => "Safety check".into(),
            Screen::Run => self
                .run
                .as_ref()
                .map(|r| r.title.clone())
                .unwrap_or_else(|| "Job".into()),
            Screen::Results(_) => "Results".into(),
            Screen::Donor(_) => "Reference file".into(),
            Screen::Scan(_) => "Preview".into(),
            Screen::Verify(_) => "Check".into(),
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let right = if self.cfg.is_root {
            "running as administrator"
        } else {
            "normal user"
        };
        let left = format!(" DiskRescue  >  {}", self.title());
        let pad =
            (area.width as usize).saturating_sub(left.chars().count() + right.chars().count() + 2);
        let line = Line::from(vec![
            Span::styled(" DiskRescue ", theme.key()),
            Span::styled(format!(" {}", self.title()), theme.bold()),
            Span::raw(" ".repeat(pad.saturating_sub(10))),
            Span::styled(
                format!("{right} "),
                if self.cfg.is_root {
                    theme.warn()
                } else {
                    theme.dim()
                },
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        if let Some((s, _)) = &self.status {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {s}"), theme.warn()))),
                area,
            );
            return;
        }
        let hints: Vec<(&str, &str)> = match &self.screen {
            Screen::Run => match &self.run {
                Some(r) if r.is_running() => {
                    let mut v = Vec::new();
                    if r.can_pause() {
                        v.push(("p", if r.is_paused() { "continue" } else { "pause" }));
                    }
                    v.push(("s", "stop and save"));
                    v.push(("b", "leave it running"));
                    v.push(("?", "help"));
                    v
                }
                Some(_) => vec![
                    ("Enter", "next"),
                    ("o", "open folder"),
                    ("h", "home"),
                    ("Esc", "back"),
                ],
                None => vec![("h", "home")],
            },
            other => footer_for(other.id()),
        };
        f.render_widget(Paragraph::new(key_hints(theme, &hints)), area);
    }

    fn draw_body(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        match &self.screen {
            Screen::Home => {
                let mut area = area;
                if let Some(r) = &self.run {
                    let [banner, rest] =
                        Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);
                    let st = if r.is_running() {
                        theme.warn()
                    } else {
                        theme.good()
                    };
                    f.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(format!(" {} ", r.one_line()), st),
                            Span::styled(" press g to go back to it ", theme.dim()),
                        ])),
                        banner,
                    );
                    area = rest;
                }
                let (list, off) = self.home.draw(f, area, theme, &self.cfg);
                self.hits.borrow_mut().list = Some((list, off, 2));
            }
            Screen::Menu(m) => {
                let (list, off) = m.draw(f, area, theme);
                self.hits.borrow_mut().list = Some((list, off, 1));
            }
            Screen::Form(form) => form.draw(f, area, theme),
            Screen::Pick(p) => {
                let (list, off) = p.draw(f, area, theme);
                self.hits.borrow_mut().list = Some((list, off, 1));
            }
            Screen::Preflight(p) => p.draw(f, area, theme),
            Screen::Run => match &self.run {
                Some(r) => r.draw(f, area, theme),
                None => f.render_widget(Paragraph::new("No job is running."), area),
            },
            Screen::Results(r) => {
                let l = r.draw(f, area, theme);
                let mut h = self.hits.borrow_mut();
                h.list = Some((l.rows, l.offset, 1));
                h.tabs = l.tabs;
            }
            Screen::Donor(d) => {
                let (list, off) = d.draw(f, area, theme);
                self.hits.borrow_mut().list = Some((list, off, 1));
            }
            Screen::Scan(s) => {
                let (list, off) = s.draw(f, area, theme);
                self.hits.borrow_mut().list = Some((list, off, 1));
            }
            Screen::Verify(v) => {
                let (list, off) = v.draw(f, area, theme);
                self.hits.borrow_mut().list = Some((list, off, 1));
            }
        }
    }

    fn draw_overlay(&self, f: &mut Frame, area: Rect, ov: &Overlay, theme: &Theme) {
        match ov {
            Overlay::Help => {
                let lines = help_lines(self.screen.id(), theme);
                let h = (lines.len() as u16 + 2).min(area.height);
                let r = centered(area, 76.min(area.width), h);
                f.render_widget(Clear, r);
                let block = theme.block_focus(" Help ", true);
                let inner = block.inner(r);
                f.render_widget(block, r);
                f.render_widget(Paragraph::new(lines), inner);
            }
            Overlay::Confirm { text, .. } => {
                let w = 62.min(area.width);
                let mut lines: Vec<Line<'static>> = wrap_words(text, w as usize - 4)
                    .into_iter()
                    .map(|l| Line::raw(format!(" {l}")))
                    .collect();
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(" y ", theme.key()),
                    Span::raw(" yes    "),
                    Span::styled(" n ", theme.key()),
                    Span::raw(" no (stay)"),
                ]));
                crate::widgets::modal(f, area, theme, "Are you sure?", w, lines);
            }
            Overlay::Choice {
                title,
                options,
                sel,
                ..
            } => {
                let w = 62.min(area.width);
                let mut lines: Vec<Line<'static>> = Vec::new();
                for (i, o) in options.iter().enumerate() {
                    let text = format!(" {}. {o}", i + 1);
                    lines.push(Line::from(Span::styled(
                        format!("{text:<width$}", width = w as usize - 4),
                        if i == *sel {
                            theme.selected()
                        } else {
                            ratatui::style::Style::default()
                        },
                    )));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    Span::styled(" Enter ", theme.key()),
                    Span::raw(" choose    "),
                    Span::styled(" Esc ", theme.key()),
                    Span::raw(" cancel"),
                ]));
                crate::widgets::modal(f, area, theme, title, w, lines);
            }
            Overlay::Message { title, lines } => {
                let w = 70.min(area.width);
                let mut out: Vec<Line<'static>> = Vec::new();
                for l in lines {
                    if l.is_empty() {
                        out.push(Line::raw(""));
                    }
                    for p in wrap_words(l, w as usize - 4) {
                        out.push(Line::raw(format!(" {p}")));
                    }
                }
                out.push(Line::raw(""));
                out.push(Line::from(Span::styled(
                    " Press any key to close.",
                    theme.dim(),
                )));
                crate::widgets::modal(f, area, theme, title, w, out);
            }
        }
    }
}

/// Start the desktop's file manager on `path`. True when a program was
/// started.
fn open_with_desktop(path: &Path) -> bool {
    use std::process::{Command, Stdio};
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    match Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap it in the background so no zombie is left behind.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}
