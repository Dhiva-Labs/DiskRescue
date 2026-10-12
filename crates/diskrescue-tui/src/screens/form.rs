//! The forms: where to save, what to recover, how, and the advanced
//! settings. Every default is meant to be right, so that most people only
//! press Enter on "Start".

use std::cell::Cell;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use diskrescue_app::plan::{
    CarveMode, Categories, ImagePlan, RecoverPlan, RepairOptions, RepairPlan, ScanPlan,
    StateFilter, Tuning,
};
use diskrescue_app::text;
use diskrescue_core::profile::ImagingProfile;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::config::Config;
use crate::fsbrowse::{absolute_from, tab_complete};
use crate::input::{Edit, TextInput};
use crate::model::{default_output_name, DeviceEntry, Plan};
use crate::parse::{parse_count, parse_seconds, parse_size};
use crate::widgets::{ellipsize_start, wrap_words, Theme};

/// Which form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    /// Copy a device, then recover and repair.
    Rescue,
    /// Copy a device to an image and stop.
    Image,
    /// Recover from a device or an image.
    Recover,
    /// Preview what can be recovered.
    Scan,
    /// Repair loose files.
    Repair,
}

impl FormKind {
    /// The heading.
    pub fn title(self) -> &'static str {
        match self {
            FormKind::Rescue => "Rescue: copy, then recover and repair",
            FormKind::Image => "Copy to an image only",
            FormKind::Recover => "Recover files",
            FormKind::Scan => "Preview what can be recovered",
            FormKind::Repair => "Repair damaged files",
        }
    }
}

/// Which setting a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldId {
    /// The folder for the results (or for the image).
    Output,
    /// The image file.
    Image,
    /// Photos, RAW, videos, audio, everything.
    What,
    /// Existing and/or deleted files.
    State,
    /// Repair on or off.
    Repair,
    /// Aggressive repair.
    Aggressive,
    /// How much raw data to search.
    Carve,
    /// Reference files.
    Donors,
    /// The imaging profile.
    Profile,
    /// Timeout of one read.
    Timeout,
    /// Retry passes.
    Retries,
    /// Size of one read.
    BlockSize,
}

/// One option of a choice.
#[derive(Debug, Clone)]
pub struct Choice {
    /// Short name.
    pub label: String,
    /// One sentence about it.
    pub hint: String,
}

/// What a field edits.
#[derive(Debug, Clone)]
pub enum FieldKind {
    /// A folder or file path.
    Path {
        /// The text.
        input: TextInput,
        /// A folder (not a file).
        folder: bool,
    },
    /// A number typed as text; empty means automatic.
    Number {
        /// The text.
        input: TextInput,
        /// What to type.
        hint: &'static str,
    },
    /// Tick boxes in a row.
    Checks {
        /// Names.
        labels: Vec<&'static str>,
        /// Ticked.
        on: Vec<bool>,
        /// Box under the cursor.
        cursor: usize,
    },
    /// One of several options.
    Choice {
        /// The options.
        options: Vec<Choice>,
        /// The chosen one.
        sel: usize,
    },
    /// On or off.
    Toggle {
        /// Switched on.
        on: bool,
        /// Words for on.
        yes: &'static str,
        /// Words for off.
        no: &'static str,
    },
    /// A list of files.
    List {
        /// The files.
        items: Vec<PathBuf>,
        /// The one under the cursor.
        cursor: usize,
    },
}

/// A labelled setting.
#[derive(Debug, Clone)]
pub struct Field {
    /// Which setting.
    pub id: FieldId,
    /// The question.
    pub label: String,
    /// A sentence of help, shown while the field has the focus.
    pub help: String,
    /// What it edits.
    pub kind: FieldKind,
    /// Why the value was refused.
    pub error: Option<String>,
    /// Shown only after "More options".
    pub advanced: bool,
}

/// A row of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Field number `n`.
    Field(usize),
    /// "More options" toggle.
    More,
    /// The Start button.
    Start,
    /// The Back button.
    Back,
}

/// What the form wants the app to do.
#[derive(Debug, Clone)]
pub enum FormAction {
    /// Nothing.
    None,
    /// Go back.
    Back,
    /// Open the help.
    Help,
    /// Check and start this.
    Start(Box<Plan>),
    /// Open the browser to choose a folder or file for a field.
    Browse {
        /// The field.
        id: FieldId,
        /// Choose a folder.
        folder: bool,
    },
}

/// A form.
#[derive(Debug, Clone)]
pub struct Form {
    /// Which form.
    pub kind: FormKind,
    /// The device or image (none when repairing loose files).
    pub target: Option<DeviceEntry>,
    /// The files to repair.
    pub files: Vec<PathBuf>,
    /// The fields.
    pub fields: Vec<Field>,
    /// Row with the focus (index into [`Form::rows`]).
    pub focus: usize,
    /// "More options" are open.
    pub more_open: bool,
    /// The image path still follows the output folder.
    image_auto: bool,
    /// Names that match the path being typed.
    pub candidates: Vec<String>,
    offset: Cell<usize>,
    /// Message at the bottom (a refusal).
    pub notice: Option<String>,
    home: PathBuf,
    cwd: PathBuf,
}

fn choice(label: &str, hint: &str) -> Choice {
    Choice {
        label: label.to_string(),
        hint: hint.to_string(),
    }
}

impl Form {
    /// A form with the right defaults for `kind`.
    pub fn new(
        kind: FormKind,
        target: Option<DeviceEntry>,
        files: Vec<PathBuf>,
        cfg: &Config,
    ) -> Form {
        let name = target
            .as_ref()
            .map(|t| default_output_name(&t.info))
            .unwrap_or_else(|| "repaired".to_string());
        let out_dir = cfg.home.join("DiskRescue").join(&name);
        let image = out_dir.join(format!("{name}.img"));
        let kind_of_device = target
            .as_ref()
            .map(|t| t.info.kind)
            .unwrap_or(diskrescue_core::device::DeviceKind::Unknown);

        let mut fields: Vec<Field> = Vec::new();
        let mut add = |id, label: &str, help: &str, kind, advanced| {
            fields.push(Field {
                id,
                label: label.to_string(),
                help: help.to_string(),
                kind,
                error: None,
                advanced,
            });
        };

        let output_label = match kind {
            FormKind::Image => "Folder for the copy",
            FormKind::Repair => "Folder for the repaired copies",
            _ => "Where should the recovered files be saved?",
        };
        let output_help = "Use a folder on a different disk than the one you recover from. Tab completes the name, F2 lets you browse.";
        if kind != FormKind::Scan {
            add(
                FieldId::Output,
                output_label,
                output_help,
                FieldKind::Path {
                    input: TextInput::new(out_dir.display().to_string()),
                    folder: true,
                },
                false,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Image) {
            add(
                FieldId::Image,
                "Image file (the copy of the device)",
                "Made first, so the device is read as little as possible. It is as big as the device. Stop and start again at any time: the copy continues.",
                FieldKind::Path {
                    input: TextInput::new(image.display().to_string()),
                    folder: false,
                },
                false,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Recover | FormKind::Scan) {
            add(
                FieldId::What,
                "What do you want back?",
                "Left and Right choose, Space ticks. Nothing ticked means photos, RAW photos and videos.",
                FieldKind::Checks {
                    labels: vec!["Photos", "RAW photos", "Videos", "Audio", "Every file"],
                    on: vec![true, true, true, false, false],
                    cursor: 0,
                },
                false,
            );
            add(
                FieldId::State,
                "Which files?",
                "Deleted files are found through the filesystem's leftovers; they are often still whole.",
                FieldKind::Choice {
                    options: vec![
                        choice("Existing and deleted", "everything that can be found"),
                        choice("Existing only", "files that are still in the folders"),
                        choice("Deleted only", "files that were deleted"),
                    ],
                    sel: 0,
                },
                false,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Recover) {
            add(
                FieldId::Repair,
                "Repair damaged files?",
                "Damaged photos and videos are repaired when possible. The damaged originals are always kept.",
                FieldKind::Toggle {
                    on: true,
                    yes: "yes, repair what can be repaired",
                    no: "no, keep damaged files as they are",
                },
                false,
            );
        }
        if kind == FormKind::Repair {
            add(
                FieldId::Donors,
                "Reference files (donors)",
                "A healthy photo or video from the same camera or phone, with the same settings, helps to repair a video that will not play. Enter adds one, d removes.",
                FieldKind::List {
                    items: Vec::new(),
                    cursor: 0,
                },
                false,
            );
            add(
                FieldId::Aggressive,
                "Allow repairs that cut damaged parts out?",
                "Usually not needed. Only try it when the normal repair did not work.",
                FieldKind::Toggle {
                    on: false,
                    yes: "yes, cut out what is damaged",
                    no: "no, normal repair only",
                },
                false,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Recover | FormKind::Scan) {
            let (options, sel) = if kind == FormKind::Scan {
                (
                    vec![
                        choice("Off", "only look through the filesystem (fast)"),
                        choice(
                            "Automatic",
                            "also free space and anything the filesystem does not cover",
                        ),
                        choice("Free space only", "free space of readable filesystems"),
                        choice(
                            "Everything",
                            "the whole source, ignoring filesystems (slow)",
                        ),
                    ],
                    0,
                )
            } else {
                (
                    vec![
                        choice(
                            "Automatic",
                            "free space plus anything the filesystem does not cover",
                        ),
                        choice("Free space only", "free space of readable filesystems"),
                        choice(
                            "Everything",
                            "the whole source, ignoring filesystems (slow)",
                        ),
                        choice("Off", "only files the filesystem still lists"),
                    ],
                    0,
                )
            };
            add(
                FieldId::Carve,
                "Search the raw data for lost files?",
                "Finds photos and videos that the filesystem no longer lists, by their content.",
                FieldKind::Choice { options, sel },
                true,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Recover) {
            add(
                FieldId::Donors,
                "Reference files (donors)",
                "A healthy photo or video from the same camera or phone helps to repair videos that will not play. Enter adds one, d removes.",
                FieldKind::List {
                    items: Vec::new(),
                    cursor: 0,
                },
                true,
            );
        }
        if matches!(kind, FormKind::Rescue | FormKind::Image) {
            let auto = ImagingProfile::for_kind(kind_of_device);
            let mut options = vec![choice(
                &format!("Automatic ({})", auto.name),
                text::profile_blurb(&auto.name),
            )];
            for n in ImagingProfile::names() {
                options.push(choice(n, text::profile_blurb(n)));
            }
            add(
                FieldId::Profile,
                "How to read the device (profile)",
                "Chosen from the kind of device. Change it only if you know better.",
                FieldKind::Choice { options, sel: 0 },
                true,
            );
            add(
                FieldId::Timeout,
                "Give up on one read after (seconds)",
                "",
                FieldKind::Number {
                    input: TextInput::new(""),
                    hint: "empty = automatic",
                },
                true,
            );
            add(
                FieldId::Retries,
                "Retry passes over unreadable areas",
                "",
                FieldKind::Number {
                    input: TextInput::new(""),
                    hint: "empty = automatic",
                },
                true,
            );
            add(
                FieldId::BlockSize,
                "Size of one read",
                "",
                FieldKind::Number {
                    input: TextInput::new(""),
                    hint: "empty = automatic, for example 64k",
                },
                true,
            );
        }

        let mut form = Form {
            kind,
            target,
            files,
            fields,
            focus: 0,
            more_open: false,
            image_auto: true,
            candidates: Vec::new(),
            offset: Cell::new(0),
            notice: None,
            home: cfg.home.clone(),
            cwd: cfg.cwd.clone(),
        };
        // Start on "Start": most people only press Enter.
        form.focus = form.rows().len() - 2;
        form
    }

    /// The rows in order: fields (advanced ones only when open), "More
    /// options" when there are any, Start, Back.
    pub fn rows(&self) -> Vec<RowKind> {
        let mut v = Vec::new();
        let mut any_advanced = false;
        for (i, f) in self.fields.iter().enumerate() {
            if f.advanced {
                any_advanced = true;
                continue;
            }
            v.push(RowKind::Field(i));
        }
        if any_advanced {
            v.push(RowKind::More);
            if self.more_open {
                for (i, f) in self.fields.iter().enumerate() {
                    if f.advanced {
                        v.push(RowKind::Field(i));
                    }
                }
            }
        }
        v.push(RowKind::Start);
        v.push(RowKind::Back);
        v
    }

    fn field(&self, id: FieldId) -> Option<&Field> {
        self.fields.iter().find(|f| f.id == id)
    }

    fn field_mut(&mut self, id: FieldId) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.id == id)
    }

    /// The text of a path or number field.
    pub fn text(&self, id: FieldId) -> Option<String> {
        match &self.field(id)?.kind {
            FieldKind::Path { input, .. } | FieldKind::Number { input, .. } => {
                Some(input.value().trim().to_string())
            }
            _ => None,
        }
    }

    /// Set the text of a path or number field.
    pub fn set_text(&mut self, id: FieldId, value: &str) {
        if let Some(f) = self.field_mut(id) {
            if let FieldKind::Path { input, .. } | FieldKind::Number { input, .. } = &mut f.kind {
                input.set(value);
            }
        }
        if id == FieldId::Output {
            self.follow_output();
        }
        if id == FieldId::Image {
            self.image_auto = false;
        }
    }

    /// The chosen option of a choice field.
    pub fn choice(&self, id: FieldId) -> Option<usize> {
        match &self.field(id)?.kind {
            FieldKind::Choice { sel, .. } => Some(*sel),
            _ => None,
        }
    }

    /// Choose an option.
    pub fn set_choice(&mut self, id: FieldId, n: usize) {
        if let Some(Field {
            kind: FieldKind::Choice { options, sel },
            ..
        }) = self.field_mut(id)
        {
            *sel = n.min(options.len().saturating_sub(1));
        }
    }

    /// Tick or untick box `n` of the "what" field.
    pub fn set_check(&mut self, id: FieldId, n: usize, value: bool) {
        if let Some(Field {
            kind: FieldKind::Checks { on, .. },
            ..
        }) = self.field_mut(id)
        {
            if let Some(b) = on.get_mut(n) {
                *b = value;
            }
        }
    }

    /// Switch a toggle.
    pub fn set_toggle(&mut self, id: FieldId, value: bool) {
        if let Some(Field {
            kind: FieldKind::Toggle { on, .. },
            ..
        }) = self.field_mut(id)
        {
            *on = value;
        }
    }

    /// Add a donor file.
    pub fn add_donor(&mut self, p: PathBuf) {
        if let Some(Field {
            kind: FieldKind::List { items, cursor },
            ..
        }) = self.field_mut(FieldId::Donors)
        {
            if !items.contains(&p) {
                items.push(p);
                *cursor = items.len() - 1;
            }
        }
    }

    /// The donor files.
    pub fn donors(&self) -> Vec<PathBuf> {
        match self.field(FieldId::Donors).map(|f| &f.kind) {
            Some(FieldKind::List { items, .. }) => items.clone(),
            _ => Vec::new(),
        }
    }

    fn toggle_on(&self, id: FieldId) -> bool {
        matches!(
            self.field(id).map(|f| &f.kind),
            Some(FieldKind::Toggle { on: true, .. })
        )
    }

    fn follow_output(&mut self) {
        if !self.image_auto {
            return;
        }
        let Some(out) = self.text(FieldId::Output) else {
            return;
        };
        let name = self
            .target
            .as_ref()
            .map(|t| default_output_name(&t.info))
            .unwrap_or_else(|| "image".into());
        let image = Path::new(&out).join(format!("{name}.img"));
        if let Some(Field {
            kind: FieldKind::Path { input, .. },
            ..
        }) = self.field_mut(FieldId::Image)
        {
            input.set(image.display().to_string());
        }
    }

    /// The field under the focus.
    pub fn focused_field(&self) -> Option<&Field> {
        match self.rows().get(self.focus)? {
            RowKind::Field(i) => self.fields.get(*i),
            _ => None,
        }
    }

    /// The row under the focus.
    pub fn focused_row(&self) -> RowKind {
        self.rows()
            .get(self.focus)
            .copied()
            .unwrap_or(RowKind::Start)
    }

    /// True while a text field has the focus (letters are typed, not
    /// commands).
    pub fn typing(&self) -> bool {
        matches!(
            self.focused_field().map(|f| &f.kind),
            Some(FieldKind::Path { .. } | FieldKind::Number { .. })
        )
    }

    fn move_focus(&mut self, delta: isize) {
        let n = self.rows().len() as isize;
        self.focus = (self.focus as isize + delta).clamp(0, n - 1) as usize;
        self.candidates.clear();
        self.notice = None;
    }

    fn focus_row(&mut self, row: RowKind) {
        if let Some(i) = self.rows().iter().position(|r| *r == row) {
            self.focus = i;
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> FormAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormAction::Back,
            KeyCode::F(1) => return FormAction::Help,
            KeyCode::F(10) => return self.try_start(),
            KeyCode::Char('s') if ctrl => return self.try_start(),
            KeyCode::Up => {
                self.move_focus(-1);
                return FormAction::None;
            }
            KeyCode::Down => {
                self.move_focus(1);
                return FormAction::None;
            }
            KeyCode::PageUp => {
                self.move_focus(-5);
                return FormAction::None;
            }
            KeyCode::PageDown => {
                self.move_focus(5);
                return FormAction::None;
            }
            _ => {}
        }
        match self.focused_row() {
            RowKind::Start => match key.code {
                KeyCode::Enter => return self.try_start(),
                KeyCode::Tab => self.move_focus(1),
                KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            RowKind::Back => match key.code {
                KeyCode::Enter => return FormAction::Back,
                KeyCode::Tab => self.move_focus(1),
                KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            RowKind::More => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left => {
                    self.more_open = !self.more_open;
                }
                KeyCode::Tab => self.move_focus(1),
                KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            RowKind::Field(i) => return self.field_key(i, key),
        }
        FormAction::None
    }

    fn field_key(&mut self, i: usize, key: &KeyEvent) -> FormAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let home = self.home.clone();
        let cwd = self.cwd.clone();
        let id = self.fields[i].id;
        // Generic movement keys for every kind of field.
        let next = |s: &mut Form| s.move_focus(1);
        let prev = |s: &mut Form| s.move_focus(-1);
        let kind = self.fields[i].kind.clone();
        match kind {
            FieldKind::Path { folder, .. } => match key.code {
                KeyCode::F(2) => return FormAction::Browse { id, folder },
                KeyCode::Char('o') if ctrl => return FormAction::Browse { id, folder },
                KeyCode::Enter => next(self),
                KeyCode::BackTab => prev(self),
                KeyCode::Tab => {
                    if let FieldKind::Path { input, .. } = &mut self.fields[i].kind {
                        self.candidates = tab_complete(input, &home, &cwd, folder);
                    }
                    if id == FieldId::Output {
                        self.follow_output();
                    }
                }
                _ => {
                    let mut edit = Edit::Ignored;
                    if let FieldKind::Path { input, .. } = &mut self.fields[i].kind {
                        edit = input.handle_key(key);
                    }
                    if edit == Edit::Changed {
                        self.fields[i].error = None;
                        self.candidates.clear();
                        if id == FieldId::Output {
                            self.follow_output();
                        }
                        if id == FieldId::Image {
                            self.image_auto = false;
                        }
                    }
                }
            },
            FieldKind::Number { .. } => match key.code {
                KeyCode::Enter | KeyCode::Tab => next(self),
                KeyCode::BackTab => prev(self),
                _ => {
                    if let FieldKind::Number { input, .. } = &mut self.fields[i].kind {
                        if input.handle_key(key) == Edit::Changed {
                            self.fields[i].error = None;
                        }
                    }
                }
            },
            FieldKind::Checks { .. } => match key.code {
                KeyCode::Left => {
                    if let FieldKind::Checks { cursor, .. } = &mut self.fields[i].kind {
                        *cursor = cursor.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let FieldKind::Checks { cursor, labels, .. } = &mut self.fields[i].kind {
                        *cursor = (*cursor + 1).min(labels.len() - 1);
                    }
                }
                KeyCode::Char(' ') => {
                    if let FieldKind::Checks { cursor, on, .. } = &mut self.fields[i].kind {
                        let c = *cursor;
                        on[c] = !on[c];
                        // "Every file" makes the other boxes moot.
                        if c == 4 && on[4] {
                            for b in on.iter_mut().take(4) {
                                *b = false;
                            }
                        } else if c < 4 && on[c] {
                            on[4] = false;
                        }
                    }
                }
                KeyCode::Enter | KeyCode::Tab => next(self),
                KeyCode::BackTab => prev(self),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            FieldKind::Choice { .. } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    if let FieldKind::Choice { options, sel } = &mut self.fields[i].kind {
                        let n = options.len();
                        *sel = if key.code == KeyCode::Left {
                            (*sel + n - 1) % n
                        } else {
                            (*sel + 1) % n
                        };
                    }
                }
                KeyCode::Enter | KeyCode::Tab => next(self),
                KeyCode::BackTab => prev(self),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            FieldKind::Toggle { .. } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    if let FieldKind::Toggle { on, .. } = &mut self.fields[i].kind {
                        *on = !*on;
                    }
                }
                KeyCode::Enter | KeyCode::Tab => next(self),
                KeyCode::BackTab => prev(self),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
            FieldKind::List { .. } => match key.code {
                KeyCode::Enter | KeyCode::F(2) | KeyCode::Char('a') | KeyCode::Char('+') => {
                    return FormAction::Browse { id, folder: false };
                }
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('-') => {
                    if let FieldKind::List { items, cursor } = &mut self.fields[i].kind {
                        if *cursor < items.len() {
                            items.remove(*cursor);
                            *cursor = cursor.saturating_sub(1).min(items.len().saturating_sub(1));
                        }
                    }
                }
                KeyCode::Left => {
                    if let FieldKind::List { cursor, .. } = &mut self.fields[i].kind {
                        *cursor = cursor.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let FieldKind::List { cursor, items } = &mut self.fields[i].kind {
                        *cursor = (*cursor + 1).min(items.len().saturating_sub(1));
                    }
                }
                KeyCode::Tab => next(self),
                KeyCode::BackTab => prev(self),
                KeyCode::Char('?') => return FormAction::Help,
                _ => {}
            },
        }
        FormAction::None
    }

    /// Check the values and build the operation.
    fn try_start(&mut self) -> FormAction {
        match self.build() {
            Ok(plan) => FormAction::Start(Box::new(plan)),
            Err((id, msg)) => {
                if let Some(f) = self.field_mut(id) {
                    f.error = Some(msg.clone());
                    let advanced = f.advanced;
                    if advanced {
                        self.more_open = true;
                    }
                }
                if let Some(i) = self.fields.iter().position(|f| f.id == id) {
                    self.focus_row(RowKind::Field(i));
                }
                self.notice = Some(msg);
                FormAction::None
            }
        }
    }

    fn path_value(&self, id: FieldId, what: &str) -> Result<PathBuf, (FieldId, String)> {
        let t = self.text(id).unwrap_or_default();
        if t.is_empty() {
            return Err((id, format!("Type {what}.")));
        }
        Ok(absolute_from(&t, &self.home, &self.cwd))
    }

    fn categories(&self) -> Categories {
        let mut c = Categories::default();
        if let Some(FieldKind::Checks { on, .. }) = self.field(FieldId::What).map(|f| &f.kind) {
            c.photos = on[0];
            c.raw = on[1];
            c.videos = on[2];
            c.audio = on[3];
            c.all_files = on[4];
            // All three defaults ticked is the same as nothing ticked.
            if c.photos && c.raw && c.videos && !c.audio && !c.all_files {
                c = Categories::default();
            }
        }
        c
    }

    fn state_filter(&self) -> StateFilter {
        match self.choice(FieldId::State) {
            Some(1) => StateFilter {
                existing_only: true,
                ..StateFilter::default()
            },
            Some(2) => StateFilter {
                deleted_only: true,
                ..StateFilter::default()
            },
            _ => StateFilter::default(),
        }
    }

    fn carve_mode(&self) -> Option<CarveMode> {
        let sel = self.choice(FieldId::Carve)?;
        Some(if self.kind == FormKind::Scan {
            [
                CarveMode::Off,
                CarveMode::Auto,
                CarveMode::Free,
                CarveMode::All,
            ][sel]
        } else {
            [
                CarveMode::Auto,
                CarveMode::Free,
                CarveMode::All,
                CarveMode::Off,
            ][sel]
        })
    }

    fn tuning(&self) -> Result<Tuning, (FieldId, String)> {
        let mut t = Tuning::default();
        if let Some(sel) = self.choice(FieldId::Profile) {
            if sel > 0 {
                t.profile = ImagingProfile::names().get(sel - 1).map(|s| s.to_string());
            }
        }
        if let Some(s) = self.text(FieldId::Timeout).filter(|s| !s.is_empty()) {
            t.timeout = Some(parse_seconds(&s).map_err(|e| (FieldId::Timeout, e))?);
        }
        if let Some(s) = self.text(FieldId::Retries).filter(|s| !s.is_empty()) {
            t.retries = Some(parse_count(&s).map_err(|e| (FieldId::Retries, e))?);
        }
        if let Some(s) = self.text(FieldId::BlockSize).filter(|s| !s.is_empty()) {
            let n = parse_size(&s).map_err(|e| (FieldId::BlockSize, e))?;
            if n == 0 {
                return Err((FieldId::BlockSize, "A read cannot be 0 bytes.".into()));
            }
            t.block_size = Some(n);
        }
        Ok(t)
    }

    /// The operation the form describes, or the field that is wrong.
    pub fn build(&self) -> Result<Plan, (FieldId, String)> {
        let source = self.target.as_ref().map(|t| t.info.path.clone());
        match self.kind {
            FormKind::Rescue => {
                let output = self.path_value(FieldId::Output, "a folder for the results")?;
                let image = self.path_value(FieldId::Image, "a file name for the image")?;
                let mut plan = RecoverPlan::new(source.unwrap_or_default(), output);
                plan.categories = self.categories();
                plan.state = self.state_filter();
                plan.carve = self.carve_mode().unwrap_or_default();
                plan.repair = RepairOptions {
                    no_repair: !self.toggle_on(FieldId::Repair),
                    donors: self.donors(),
                    aggressive: false,
                };
                plan.image_to = Some(image);
                plan.tuning = self.tuning()?;
                Ok(Plan::Recover(plan))
            }
            FormKind::Image => {
                let image = self.path_value(FieldId::Image, "a file name for the image")?;
                let mut plan = ImagePlan::new(source.unwrap_or_default(), image);
                plan.tuning = self.tuning()?;
                Ok(Plan::Image(plan))
            }
            FormKind::Recover => {
                let output = self.path_value(FieldId::Output, "a folder for the results")?;
                let mut plan = RecoverPlan::new(source.unwrap_or_default(), output);
                plan.categories = self.categories();
                plan.state = self.state_filter();
                plan.carve = self.carve_mode().unwrap_or_default();
                plan.repair = RepairOptions {
                    no_repair: !self.toggle_on(FieldId::Repair),
                    donors: self.donors(),
                    aggressive: false,
                };
                Ok(Plan::Recover(plan))
            }
            FormKind::Scan => {
                let plan = ScanPlan {
                    source: source.unwrap_or_default(),
                    categories: self.categories(),
                    state: self.state_filter(),
                    carve: self.carve_mode().filter(|m| *m != CarveMode::Off),
                    map: None,
                };
                Ok(Plan::Scan(plan))
            }
            FormKind::Repair => {
                let output =
                    self.path_value(FieldId::Output, "a folder for the repaired copies")?;
                Ok(Plan::Repair(RepairPlan {
                    files: self.files.clone(),
                    output,
                    donors: self.donors(),
                    aggressive: self.toggle_on(FieldId::Aggressive),
                }))
            }
        }
    }

    /// Words for the "More options" row: what is set there.
    fn more_summary(&self) -> String {
        let mut parts = Vec::new();
        for f in self.fields.iter().filter(|f| f.advanced) {
            match &f.kind {
                FieldKind::Choice { options, sel } => {
                    parts.push(format!(
                        "{}: {}",
                        short_label(f.id),
                        options[*sel].label.to_lowercase()
                    ));
                }
                FieldKind::List { items, .. } if !items.is_empty() => {
                    parts.push(format!("{} donor file(s)", items.len()));
                }
                FieldKind::Number { input, .. } if !input.is_blank() => {
                    parts.push(format!("{}: {}", short_label(f.id), input.value()));
                }
                _ => {}
            }
        }
        parts.join(", ")
    }

    /// Draw the form into `area`.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        // Heading and a summary of the target.
        let mut head: Vec<Line<'static>> = Vec::new();
        if let Some(t) = &self.target {
            head.push(Line::from(vec![
                Span::styled("Source: ", theme.dim()),
                Span::raw(format!(
                    "{} - {}",
                    t.info.path.display(),
                    diskrescue_app::devices::device_label(&t.info)
                )),
            ]));
        } else if !self.files.is_empty() {
            head.push(Line::from(vec![
                Span::styled("Files: ", theme.dim()),
                Span::raw(ellipsize_start(
                    &if self.files.len() == 1 {
                        self.files[0].display().to_string()
                    } else {
                        format!("{} files chosen", self.files.len())
                    },
                    width.saturating_sub(8),
                )),
            ]));
        }
        head.push(Line::raw(""));
        let head_h = head.len();
        lines.extend(head);

        let rows = self.rows();
        let mut focus_range = (0usize, 0usize);
        for (ri, row) in rows.iter().enumerate() {
            let focused = ri == self.focus;
            let start = lines.len();
            match row {
                RowKind::Field(i) => lines.extend(self.field_lines(*i, focused, width, theme)),
                RowKind::More => {
                    let arrow = if self.more_open { "v" } else { ">" };
                    let mut spans = vec![
                        Span::styled(marker(focused), theme.accent()),
                        Span::styled(
                            format!("{arrow} More options"),
                            if focused { theme.bold() } else { theme.dim() },
                        ),
                    ];
                    if !self.more_open {
                        let s = self.more_summary();
                        if !s.is_empty() {
                            spans.push(Span::styled(format!("  ({s})"), theme.dim()));
                        }
                    }
                    lines.push(Line::from(spans));
                }
                RowKind::Start => {
                    lines.push(Line::raw(""));
                    lines.push(Line::from(vec![
                        Span::styled(marker(focused), theme.accent()),
                        Span::styled(
                            " Start ",
                            if focused {
                                theme.selected()
                            } else {
                                theme.key()
                            },
                        ),
                        Span::styled(
                            if focused {
                                "  press Enter to check and start"
                            } else {
                                ""
                            },
                            theme.dim(),
                        ),
                    ]));
                }
                RowKind::Back => {
                    lines.push(Line::from(vec![
                        Span::styled(marker(focused), theme.accent()),
                        Span::styled(
                            " Back ",
                            if focused {
                                theme.selected()
                            } else {
                                theme.dim()
                            },
                        ),
                    ]));
                }
            }
            if focused {
                focus_range = (start, lines.len());
            }
        }
        if let Some(n) = &self.notice {
            lines.push(Line::raw(""));
            for l in wrap_words(n, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(l, theme.bad())));
            }
        }
        // Keep the focused row in view; the heading scrolls away first.
        let h = area.height as usize;
        let mut off = self.offset.get();
        if focus_range.1 > off + h {
            off = focus_range.1 - h;
        }
        if focus_range.0 < off {
            off = focus_range.0;
        }
        if lines.len() <= h {
            off = 0;
        }
        off = off.min(lines.len().saturating_sub(1));
        let _ = head_h;
        self.offset.set(off);
        let visible: Vec<Line<'static>> = lines.into_iter().skip(off).take(h).collect();
        f.render_widget(Paragraph::new(visible), area);
    }

    fn field_lines(
        &self,
        i: usize,
        focused: bool,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let fld = &self.fields[i];
        let mut out: Vec<Line<'static>> = Vec::new();
        let label_style = if focused {
            theme.bold()
        } else {
            ratatui::style::Style::default()
        };
        let inner = width.saturating_sub(4);
        let value_line = |spans: Vec<Span<'static>>| -> Line<'static> {
            let mut s = vec![Span::raw("    ")];
            s.extend(spans);
            Line::from(s)
        };
        match &fld.kind {
            FieldKind::Path { input, .. } => {
                out.push(Line::from(vec![
                    Span::styled(marker(focused), theme.accent()),
                    Span::styled(fld.label.clone(), label_style),
                ]));
                let mut spans = vec![Span::styled("[", theme.dim())];
                spans.extend(input.line(inner.saturating_sub(2), focused, theme).spans);
                // Pad so the closing bracket lines up.
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                if used < inner.saturating_sub(1) {
                    spans.push(Span::raw(" ".repeat(inner - 1 - used)));
                }
                spans.push(Span::styled("]", theme.dim()));
                out.push(value_line(spans));
                if focused && !self.candidates.is_empty() {
                    let shown: Vec<String> = self.candidates.iter().take(6).cloned().collect();
                    let more = if self.candidates.len() > 6 {
                        format!("  (+{} more)", self.candidates.len() - 6)
                    } else {
                        String::new()
                    };
                    out.push(value_line(vec![Span::styled(
                        format!("Matches: {}{more}", shown.join("  ")),
                        theme.accent(),
                    )]));
                }
            }
            FieldKind::Number { input, hint } => {
                let mut spans = vec![
                    Span::styled(marker(focused), theme.accent()),
                    Span::styled(format!("{}  ", fld.label), label_style),
                    Span::styled("[", theme.dim()),
                ];
                let mut inl = input.line(12, focused, theme).spans;
                let used: usize = inl.iter().map(|s| s.content.chars().count()).sum();
                if used < 12 {
                    inl.push(Span::raw(" ".repeat(12 - used)));
                }
                spans.extend(inl);
                spans.push(Span::styled("]", theme.dim()));
                spans.push(Span::styled(format!("  {hint}"), theme.dim()));
                out.push(Line::from(spans));
            }
            FieldKind::Checks {
                labels, on, cursor, ..
            } => {
                out.push(Line::from(vec![
                    Span::styled(marker(focused), theme.accent()),
                    Span::styled(fld.label.clone(), label_style),
                ]));
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (n, l) in labels.iter().enumerate() {
                    let text = format!("[{}] {l}", if on[n] { "x" } else { " " });
                    let st = if focused && n == *cursor {
                        theme.selected()
                    } else {
                        ratatui::style::Style::default()
                    };
                    spans.push(Span::styled(text, st));
                    spans.push(Span::raw("  "));
                }
                out.push(value_line(spans));
            }
            FieldKind::Choice { options, sel } => {
                if focused {
                    out.push(Line::from(vec![
                        Span::styled(marker(true), theme.accent()),
                        Span::styled(fld.label.clone(), label_style),
                        Span::styled("   (Left/Right or Space to change)", theme.dim()),
                    ]));
                    for (n, o) in options.iter().enumerate() {
                        let on = n == *sel;
                        let text = format!("({}) {}", if on { "*" } else { " " }, o.label);
                        let mut spans = vec![Span::styled(
                            text,
                            if on {
                                theme.bold()
                            } else {
                                ratatui::style::Style::default()
                            },
                        )];
                        if !o.hint.is_empty() {
                            spans.push(Span::styled(format!(" - {}", o.hint), theme.dim()));
                        }
                        out.push(value_line(spans));
                    }
                } else {
                    out.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{}  ", fld.label), label_style),
                        Span::styled(options[*sel].label.clone(), theme.accent()),
                    ]));
                }
            }
            FieldKind::Toggle { on, yes, no } => {
                out.push(Line::from(vec![
                    Span::styled(marker(focused), theme.accent()),
                    Span::styled(format!("{}  ", fld.label), label_style),
                    Span::styled(
                        format!(
                            "[{}] {}",
                            if *on { "x" } else { " " },
                            if *on { yes } else { no }
                        ),
                        if *on { theme.accent() } else { theme.warn() },
                    ),
                ]));
            }
            FieldKind::List { items, cursor } => {
                out.push(Line::from(vec![
                    Span::styled(marker(focused), theme.accent()),
                    Span::styled(fld.label.clone(), label_style),
                    Span::styled(if items.is_empty() { "  (none)" } else { "" }, theme.dim()),
                ]));
                for (n, p) in items.iter().enumerate() {
                    let st = if focused && n == *cursor {
                        theme.selected()
                    } else {
                        ratatui::style::Style::default()
                    };
                    out.push(value_line(vec![Span::styled(
                        ellipsize_start(&p.display().to_string(), inner.saturating_sub(2)),
                        st,
                    )]));
                }
                if focused {
                    out.push(value_line(vec![Span::styled(
                        "Enter adds a file, d removes the highlighted one",
                        theme.dim(),
                    )]));
                }
            }
        }
        if let Some(e) = &fld.error {
            for l in wrap_words(e, inner) {
                out.push(value_line(vec![Span::styled(l, theme.bad())]));
            }
        }
        if focused && !fld.help.is_empty() {
            for l in wrap_words(&fld.help, inner) {
                out.push(value_line(vec![Span::styled(l, theme.dim())]));
            }
        }
        if focused && matches!(fld.id, FieldId::Profile) {
            if let FieldKind::Choice { sel, .. } = &fld.kind {
                let name = if *sel == 0 {
                    self.target
                        .as_ref()
                        .map(|t| ImagingProfile::for_kind(t.info.kind).name)
                } else {
                    ImagingProfile::names().get(*sel - 1).map(|s| s.to_string())
                };
                if let Some(p) = name.and_then(|n| ImagingProfile::by_name(&n)) {
                    for l in wrap_words(&text::profile_details(&p), inner) {
                        out.push(value_line(vec![Span::styled(l, theme.dim())]));
                    }
                }
            }
        }
        out
    }
}

fn short_label(id: FieldId) -> &'static str {
    match id {
        FieldId::Carve => "raw search",
        FieldId::Profile => "profile",
        FieldId::Timeout => "timeout",
        FieldId::Retries => "retries",
        FieldId::BlockSize => "read size",
        _ => "",
    }
}

fn marker(focused: bool) -> &'static str {
    if focused {
        "> "
    } else {
        "  "
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::failing_usb_stick;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use std::time::Duration;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn rescue_form(home: &Path) -> Form {
        let cfg = Config::for_tests(home, vec![]);
        let entry = DeviceEntry::new(failing_usb_stick().info, false);
        Form::new(FormKind::Rescue, Some(entry), vec![], &cfg)
    }

    #[test]
    fn the_defaults_build_a_sensible_rescue() {
        let home = Path::new("/home/me");
        let f = rescue_form(home);
        // Focus starts on Start.
        assert_eq!(f.focused_row(), RowKind::Start);
        let Plan::Recover(p) = f.build().unwrap() else {
            panic!("expected a recovery plan");
        };
        assert_eq!(p.source, PathBuf::from("/dev/fixture-sdb"));
        assert_eq!(p.output, home.join("DiskRescue/fixture-sdb"));
        assert_eq!(
            p.image_to,
            Some(home.join("DiskRescue/fixture-sdb/fixture-sdb.img"))
        );
        assert_eq!(p.categories, Categories::default());
        assert_eq!(p.carve, CarveMode::Auto);
        assert!(!p.repair.no_repair && p.repair.donors.is_empty());
        assert_eq!(p.tuning, Tuning::default());
    }

    #[test]
    fn the_image_follows_the_output_until_it_is_edited() {
        let mut f = rescue_form(Path::new("/home/me"));
        f.set_text(FieldId::Output, "/mnt/big/rescue");
        assert_eq!(
            f.text(FieldId::Image).unwrap(),
            "/mnt/big/rescue/fixture-sdb.img"
        );
        f.set_text(FieldId::Image, "/mnt/other/card.img");
        f.set_text(FieldId::Output, "/mnt/elsewhere");
        assert_eq!(f.text(FieldId::Image).unwrap(), "/mnt/other/card.img");
    }

    #[test]
    fn choices_and_boxes_end_up_in_the_plan() {
        let mut f = rescue_form(Path::new("/home/me"));
        f.set_check(FieldId::What, 0, false);
        f.set_check(FieldId::What, 1, false);
        f.set_check(FieldId::What, 3, true);
        f.set_choice(FieldId::State, 2);
        f.set_choice(FieldId::Carve, 3);
        f.set_toggle(FieldId::Repair, false);
        f.set_choice(FieldId::Profile, 2);
        f.set_text(FieldId::Timeout, "15");
        f.set_text(FieldId::Retries, "4");
        f.set_text(FieldId::BlockSize, "64k");
        f.add_donor(PathBuf::from("/x/good.mp4"));
        let Plan::Recover(p) = f.build().unwrap() else {
            panic!()
        };
        assert!(p.categories.videos && p.categories.audio && !p.categories.photos);
        assert!(p.state.deleted_only);
        assert_eq!(p.carve, CarveMode::Off);
        assert!(p.repair.no_repair);
        assert_eq!(p.repair.donors, vec![PathBuf::from("/x/good.mp4")]);
        assert_eq!(
            p.tuning.profile.as_deref(),
            Some(ImagingProfile::names()[1])
        );
        assert_eq!(p.tuning.timeout, Some(Duration::from_secs(15)));
        assert_eq!(p.tuning.retries, Some(4));
        assert_eq!(p.tuning.block_size, Some(65_536));
    }

    #[test]
    fn a_wrong_number_points_at_its_field() {
        let mut f = rescue_form(Path::new("/home/me"));
        f.set_text(FieldId::Timeout, "soon");
        let (id, msg) = f.build().unwrap_err();
        assert_eq!(id, FieldId::Timeout);
        assert!(msg.contains("seconds"));
        // Starting opens "More options" and puts the focus on the field.
        f.handle_key(&key(KeyCode::Enter));
        assert!(f.more_open);
        assert!(f.focused_field().is_some_and(|x| x.id == FieldId::Timeout));
        assert!(f.fields.iter().any(|x| x.error.is_some()));
        // Empty output is refused.
        let mut f = rescue_form(Path::new("/home/me"));
        f.set_text(FieldId::Output, "");
        assert_eq!(f.build().unwrap_err().0, FieldId::Output);
    }

    #[test]
    fn every_ticked_default_means_the_default_selection() {
        let mut f = rescue_form(Path::new("/home/me"));
        assert_eq!(f.categories(), Categories::default());
        f.set_check(FieldId::What, 4, true);
        let Plan::Recover(p) = f.build().unwrap() else {
            panic!()
        };
        assert!(p.categories.all_files);
    }

    #[test]
    fn keys_move_through_the_form() {
        let mut f = rescue_form(Path::new("/home/me"));
        // Up from Start: More options, then the repair toggle.
        f.handle_key(&key(KeyCode::Up));
        assert_eq!(f.focused_row(), RowKind::More);
        f.handle_key(&key(KeyCode::Up));
        assert!(f.focused_field().is_some_and(|x| x.id == FieldId::Repair));
        f.handle_key(&key(KeyCode::Char(' ')));
        assert!(!f.toggle_on(FieldId::Repair));
        // Enter moves on.
        f.handle_key(&key(KeyCode::Enter));
        assert_eq!(f.focused_row(), RowKind::More);
        // Open the extra options and walk into them.
        f.handle_key(&key(KeyCode::Enter));
        assert!(f.more_open);
        f.handle_key(&key(KeyCode::Down));
        assert!(f.focused_field().is_some_and(|x| x.id == FieldId::Carve));
        f.handle_key(&key(KeyCode::Right));
        assert_eq!(f.choice(FieldId::Carve), Some(1));
        assert!(matches!(f.handle_key(&key(KeyCode::Esc)), FormAction::Back));
    }

    #[test]
    fn the_tick_boxes_know_that_every_file_replaces_the_others() {
        let mut f = rescue_form(Path::new("/home/me"));
        f.focus_row(RowKind::Field(
            f.fields.iter().position(|x| x.id == FieldId::What).unwrap(),
        ));
        for _ in 0..4 {
            f.handle_key(&key(KeyCode::Right));
        }
        f.handle_key(&key(KeyCode::Char(' ')));
        let FieldKind::Checks { on, .. } = &f.field(FieldId::What).unwrap().kind else {
            panic!()
        };
        assert_eq!(on, &vec![false, false, false, false, true]);
        f.handle_key(&key(KeyCode::Left));
        f.handle_key(&key(KeyCode::Left));
        f.handle_key(&key(KeyCode::Left));
        f.handle_key(&key(KeyCode::Left));
        f.handle_key(&key(KeyCode::Char(' ')));
        let FieldKind::Checks { on, .. } = &f.field(FieldId::What).unwrap().kind else {
            panic!()
        };
        assert_eq!(on, &vec![true, false, false, false, false]);
    }

    #[test]
    fn other_forms_have_their_own_fields() {
        let cfg = Config::for_tests(Path::new("/home/me"), vec![]);
        let entry = DeviceEntry::new(failing_usb_stick().info, false);
        let f = Form::new(FormKind::Image, Some(entry.clone()), vec![], &cfg);
        assert!(f.field(FieldId::Image).is_some() && f.field(FieldId::What).is_none());
        let Plan::Image(p) = f.build().unwrap() else {
            panic!()
        };
        assert!(p.image.ends_with("fixture-sdb.img"));
        let f = Form::new(FormKind::Scan, Some(entry.clone()), vec![], &cfg);
        assert!(f.field(FieldId::Output).is_none());
        let Plan::Scan(p) = f.build().unwrap() else {
            panic!()
        };
        assert!(
            p.carve.is_none(),
            "scans do not search the raw data by default"
        );
        let f = Form::new(
            FormKind::Repair,
            None,
            vec![PathBuf::from("/a/b.mp4")],
            &cfg,
        );
        let Plan::Repair(p) = f.build().unwrap() else {
            panic!()
        };
        assert_eq!(p.files, vec![PathBuf::from("/a/b.mp4")]);
        assert!(p.output.ends_with("DiskRescue/repaired") && !p.aggressive);
        let f = Form::new(FormKind::Recover, Some(entry), vec![], &cfg);
        assert!(f.field(FieldId::Image).is_none());
    }
}
