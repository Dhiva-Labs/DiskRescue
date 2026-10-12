//! What to do with a device or an image: the action menu.

use crossterm::event::{KeyCode, KeyEvent};
use diskrescue_app::devices;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::DeviceEntry;
use crate::screens::form::FormKind;
use crate::widgets::{ellipsize_end, render_list, wrap_words, ListNav, Theme};

/// One choice.
#[derive(Debug, Clone)]
pub struct MenuItem {
    /// The words on the menu.
    pub label: String,
    /// What it does, in a few sentences.
    pub detail: String,
    /// The form it opens.
    pub form: FormKind,
    /// Why it cannot be chosen, when it cannot.
    pub disabled: Option<String>,
}

/// What the menu wants the app to do.
#[derive(Debug, Clone)]
pub enum MenuAction {
    /// Nothing.
    None,
    /// One screen back.
    Back,
    /// Show the help.
    Help,
    /// Open a form.
    Form(FormKind),
    /// Show why an item cannot be chosen.
    Status(String),
}

/// The action menu of a device or image.
#[derive(Debug, Clone)]
pub struct Menu {
    /// The device or image.
    pub target: DeviceEntry,
    /// The choices.
    pub items: Vec<MenuItem>,
    /// Selection.
    pub nav: ListNav,
}

const SYSTEM_REASON: &str = "This is the disk the computer is running from. Recovering from it while the system runs keeps writing to it and can destroy what you want back. If photos were deleted from it, shut the computer down and recover from another system, such as a live USB stick.";

impl Menu {
    /// The menu for `target`.
    pub fn new(target: DeviceEntry) -> Menu {
        let block = if target.system {
            Some(SYSTEM_REASON.to_string())
        } else if target.info.size == 0 && !target.is_image() {
            Some("This device reports a capacity of 0 bytes. Its controller has most likely failed, and software cannot read it. A data recovery lab can often still recover the data.".to_string())
        } else {
            None
        };
        let item = |label: &str, detail: &str, form| MenuItem {
            label: label.to_string(),
            detail: detail.to_string(),
            form,
            disabled: block.clone(),
        };
        let items = if target.is_image() {
            vec![
                item(
                    "Recover files from this image",
                    "Finds photos and videos, including deleted ones, and repairs damaged ones. The image is only read. Results are saved in a folder you choose.",
                    FormKind::Recover,
                ),
                item(
                    "Preview what can be recovered (scan)",
                    "Lists what could be recovered without saving anything. A good first look.",
                    FormKind::Scan,
                ),
            ]
        } else {
            vec![
                item(
                    "Rescue (copy, then recover and repair) - recommended for failing drives",
                    "Copies the whole device into an image file first, reading the healthy parts first and coming back for the damaged ones, so the device is stressed as little as possible. Then recovers and repairs photos and videos from the copy. You can stop at any time and continue later.",
                    FormKind::Rescue,
                ),
                item(
                    "Copy to an image only",
                    "Makes the image and stops. Use it when you want to keep a copy before doing anything else; recover from the image later.",
                    FormKind::Image,
                ),
                item(
                    "Recover directly (healthy drives, accidental deletion)",
                    "Reads the device itself, without a copy. Fine when the drive is healthy, for example after deleting photos by accident. If it shows read errors or disconnects, stop and use Rescue instead.",
                    FormKind::Recover,
                ),
                item(
                    "Preview what can be recovered (scan)",
                    "Lists what could be recovered without saving anything.",
                    FormKind::Scan,
                ),
            ]
        };
        Menu {
            target,
            items,
            nav: ListNav::new(),
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> MenuAction {
        let len = self.items.len();
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                return MenuAction::Back
            }
            KeyCode::Char('?') | KeyCode::F(1) => return MenuAction::Help,
            KeyCode::Up | KeyCode::Char('k') => self.nav.move_by(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.nav.move_by(1, len),
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len - 1, len),
            KeyCode::Char(c @ '1'..='9') => {
                let n = (c as u8 - b'1') as usize;
                if n < len {
                    self.nav.select(n, len);
                    return self.activate();
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => return self.activate(),
            _ => {}
        }
        MenuAction::None
    }

    fn activate(&self) -> MenuAction {
        let it = &self.items[self.nav.sel];
        match &it.disabled {
            Some(reason) => MenuAction::Status(reason.clone()),
            None => MenuAction::Form(it.form),
        }
    }

    /// Click a row; a second click activates it.
    pub fn click_row(&mut self, index: usize) -> MenuAction {
        if index >= self.items.len() {
            return MenuAction::None;
        }
        if self.nav.sel == index {
            return self.activate();
        }
        self.nav.sel = index;
        MenuAction::None
    }

    /// Draw the menu. Returns the list area and first visible row.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (Rect, usize) {
        let t = &self.target;
        let mut head = vec![Line::from(Span::styled(
            format!(
                "{}  {}",
                t.info.path.display(),
                devices::device_label(&t.info)
            ),
            theme.bold(),
        ))];
        let flags = t.flags();
        if !flags.is_empty() {
            head.push(Line::from(
                flags
                    .iter()
                    .map(|f| {
                        let st = match *f {
                            "system disk" | "failing" => theme.bad(),
                            _ => theme.warn(),
                        };
                        Span::styled(format!("[{f}] "), st)
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        head.push(Line::raw(""));
        head.push(Line::from(Span::styled(
            "What do you want to do?",
            theme.bold(),
        )));
        let head_h = head.len() as u16;
        let n = self.items.len() as u16;
        let [h, list, detail] = Layout::vertical([
            Constraint::Length(head_h),
            Constraint::Length(n + 1),
            Constraint::Min(3),
        ])
        .areas(area);
        f.render_widget(Paragraph::new(head), h);
        let w = list.width as usize;
        let rows: Vec<Line<'static>> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let style = if it.disabled.is_some() {
                    theme.dim()
                } else {
                    ratatui::style::Style::default()
                };
                let mut spans = vec![Span::styled(
                    format!(
                        " {}. {}",
                        i + 1,
                        ellipsize_end(&it.label, w.saturating_sub(6))
                    ),
                    style,
                )];
                if it.disabled.is_some() {
                    spans.push(Span::styled(" (not available)", theme.dim()));
                }
                Line::from(spans)
            })
            .collect();
        let off = render_list(f, list, &rows, &self.nav, theme, true);
        // What the selected choice does.
        let it = &self.items[self.nav.sel];
        let mut lines: Vec<Line<'static>> = Vec::new();
        for l in wrap_words(&it.detail, area.width as usize) {
            lines.push(Line::raw(l));
        }
        if let Some(r) = &it.disabled {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Why it is not available",
                theme.bad(),
            )));
            for l in wrap_words(r, area.width as usize) {
                lines.push(Line::from(Span::styled(l, theme.bad())));
            }
        } else if t.needs_root && !it.label.starts_with("Copy") && !it.label.starts_with("Rescue") {
            lines.push(Line::raw(""));
            for l in wrap_words(
                "This device needs administrator rights to read. Close DiskRescue and start it again with: sudo diskrescue-tui",
                area.width as usize,
            ) {
                lines.push(Line::from(Span::styled(l, theme.warn())));
            }
        } else if t.needs_root {
            lines.push(Line::raw(""));
            for l in wrap_words(
                "This device needs administrator rights to read. Your password will be asked for when the copy starts.",
                area.width as usize,
            ) {
                lines.push(Line::from(Span::styled(l, theme.warn())));
            }
        }
        f.render_widget(Paragraph::new(lines), detail);
        (list, off)
    }
}
