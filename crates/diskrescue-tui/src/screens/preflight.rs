//! The check before anything starts: what will happen, what is wrong
//! (red), what to be careful about (yellow), and the confirmation that an
//! overridable problem needs.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use diskrescue_app::devices;
use diskrescue_app::preflight::{
    self, Blocker, BlockerCode, Preflight, ResolvedImage, ResolvedRecover,
};
use diskrescue_app::text::{bytes, percent, plural, size_label};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::input::TextInput;
use crate::model::{guidance, plain_words, Overrides, Plan};
use crate::pending::Pending;
use crate::widgets::{wrap_words, Theme};

/// What the check found.
#[derive(Debug, Clone, Default)]
pub struct PfResult {
    /// Problems that stop the operation.
    pub blockers: Vec<Blocker>,
    /// Things to be careful about.
    pub warnings: Vec<String>,
    /// What was found out (sizes, free space, resuming).
    pub facts: Vec<String>,
}

impl PfResult {
    /// Nothing stops the operation.
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Every blocker can be overridden.
    pub fn only_overridable(&self) -> bool {
        !self.blockers.is_empty() && self.blockers.iter().all(|b| b.override_flag.is_some())
    }
}

fn image_facts(r: &ResolvedImage, facts: &mut Vec<String>) {
    facts.push(format!(
        "Source: {} ({})",
        r.source.path.display(),
        devices::device_label(&r.source)
    ));
    if r.already_complete {
        facts.push("The image is already complete: there is nothing left to copy.".into());
        return;
    }
    if let Some(p) = &r.profile {
        facts.push(format!(
            "Reading strategy: {} ({})",
            p.name,
            diskrescue_app::text::profile_blurb(&p.name)
        ));
    }
    facts.push(format!(
        "To copy: {} into {}",
        size_label(r.copy_size),
        r.image.display()
    ));
    if let Some(st) = &r.resuming {
        facts.push(format!(
            "Continuing an earlier copy: {} of {} is already done.",
            percent(st.rescued, st.total()),
            bytes(st.total())
        ));
    }
    match r.free_space {
        Some(free) => facts.push(format!(
            "Room needed on the destination disk: {} ({} free)",
            bytes(r.space_needed),
            bytes(free)
        )),
        None => facts.push(format!(
            "Room needed on the destination disk: {} (free space unknown)",
            bytes(r.space_needed)
        )),
    }
}

/// Run the checks of `plan` (on the calling thread).
///
/// With `privileged`, "permission denied" on the device is not a problem:
/// the copy is made by a helper that asks for administrator rights.
pub fn run_checks(plan: &Plan, privileged: bool) -> PfResult {
    let mut res = PfResult::default();
    let take = |blockers: Vec<Blocker>, res: &mut PfResult| {
        for b in blockers {
            if privileged && b.code == BlockerCode::PermissionDenied {
                continue;
            }
            res.blockers.push(b);
        }
    };
    match plan {
        Plan::Image(p) => {
            let pf: Preflight<ResolvedImage> = preflight::preflight_image(p);
            take(pf.blockers, &mut res);
            res.warnings = pf.warnings;
            if let Some(r) = &pf.resolved {
                image_facts(r, &mut res.facts);
            }
        }
        Plan::Recover(p) => {
            let pf: Preflight<ResolvedRecover> = preflight::preflight_recover(p);
            take(pf.blockers, &mut res);
            res.warnings = pf.warnings;
            if let Some(r) = &pf.resolved {
                match &r.image {
                    Some(img) => image_facts(img, &mut res.facts),
                    None => res.facts.push(format!(
                        "Source: {} ({})",
                        r.source.path.display(),
                        devices::device_label(&r.source)
                    )),
                }
            }
        }
        Plan::Scan(p) => {
            let pf = preflight::preflight_scan(p);
            take(pf.blockers, &mut res);
            res.warnings = pf.warnings;
            if let Some(info) = &pf.resolved {
                res.facts.push(format!(
                    "Source: {} ({})",
                    info.path.display(),
                    devices::device_label(info)
                ));
            }
        }
        Plan::Repair(p) => {
            let pf = preflight::preflight_repair(p);
            take(pf.blockers, &mut res);
            res.warnings = pf.warnings;
            if let Some(files) = &pf.resolved {
                res.facts.push(format!(
                    "Files to repair: {}",
                    plural(files.len() as u64, "file", "files")
                ));
            }
        }
        Plan::Verify(p) => {
            let pf = preflight::preflight_verify(p);
            take(pf.blockers, &mut res);
            res.warnings = pf.warnings;
            if let Some(files) = &pf.resolved {
                res.facts.push(format!(
                    "Files to check: {}",
                    plural(files.len() as u64, "file", "files")
                ));
            }
        }
    }
    res
}

/// Where the check is.
#[derive(Debug)]
pub enum PfState {
    /// Running on another thread.
    Checking(Pending<PfResult>),
    /// Done.
    Ready(PfResult),
}

/// What the screen wants the app to do.
#[derive(Debug, Clone)]
pub enum PfAction {
    /// Nothing.
    None,
    /// Back to the form.
    Back,
    /// Open the help.
    Help,
    /// Start, lifting these overrides.
    Start(Box<Plan>, Overrides),
}

/// The check screen.
#[derive(Debug)]
pub struct PreflightScreen {
    /// The operation.
    pub plan: Plan,
    /// The copy will be made through the privileged helper.
    pub privileged: bool,
    /// The check.
    pub state: PfState,
    /// What is typed to confirm an override.
    pub confirm: TextInput,
    /// Why starting failed after the check.
    pub notice: Option<String>,
    is_root: bool,
    scroll: Cell<usize>,
}

impl PreflightScreen {
    /// Start checking `plan` in the background.
    pub fn new(plan: Plan, privileged: bool, is_root: bool) -> Self {
        let p = plan.clone();
        let pending = Pending::spawn(move || run_checks(&p, privileged));
        PreflightScreen {
            plan,
            privileged,
            state: PfState::Checking(pending),
            confirm: TextInput::new(""),
            notice: None,
            is_root,
            scroll: Cell::new(0),
        }
    }

    /// Pick up the result when it is ready.
    pub fn poll(&mut self) {
        if let PfState::Checking(p) = &mut self.state {
            match p.poll() {
                Some(Ok(r)) => self.state = PfState::Ready(r),
                Some(Err(e)) => {
                    self.state = PfState::Ready(PfResult {
                        blockers: vec![Blocker {
                            code: BlockerCode::SourceUnreadable,
                            message: e,
                            override_flag: None,
                        }],
                        ..PfResult::default()
                    })
                }
                None => {}
            }
        }
    }

    /// The result, once there is one.
    pub fn result(&self) -> Option<&PfResult> {
        match &self.state {
            PfState::Ready(r) => Some(r),
            PfState::Checking(_) => None,
        }
    }

    /// Text is being typed (the confirmation).
    pub fn typing(&self) -> bool {
        self.result().is_some_and(|r| r.only_overridable())
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PfAction {
        match key.code {
            KeyCode::Esc => return PfAction::Back,
            KeyCode::F(1) => return PfAction::Help,
            KeyCode::PageDown => self.scroll_by(5),
            KeyCode::PageUp => self.scroll_by(-5),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::Up => self.scroll_by(-1),
            _ => {}
        }
        let Some(r) = self.result() else {
            return PfAction::None;
        };
        let clear = r.is_clear();
        let only_overridable = r.only_overridable();
        let overrides = Overrides::lifting(&r.blockers);
        match key.code {
            KeyCode::Enter if clear => {
                return PfAction::Start(Box::new(self.plan.clone()), Overrides::default())
            }
            KeyCode::Enter if only_overridable => {
                if self.confirm.value().trim().eq_ignore_ascii_case("yes") {
                    return PfAction::Start(Box::new(self.plan.clone()), overrides);
                }
                self.notice =
                    Some("Type the word yes, then press Enter, to continue anyway.".into());
            }
            KeyCode::Char('?') if !only_overridable => return PfAction::Help,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {}
            _ if only_overridable => {
                self.confirm.handle_key(key);
                self.notice = None;
            }
            _ => {}
        }
        PfAction::None
    }

    /// Draw the screen.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut bottom: Vec<Line<'static>> = Vec::new();
        let head = |t: &str| Line::from(Span::styled(t.to_string(), theme.bold()));
        match &self.state {
            PfState::Checking(_) => {
                lines.push(head(&format!("Checking: {}", self.plan.title())));
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "Looking at the source, the destination and the free space. Nothing is written yet.",
                ));
            }
            PfState::Ready(r) => {
                if r.is_clear() {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}: ", self.plan.title()), theme.bold()),
                        Span::styled("ready to start", theme.good()),
                    ]));
                } else if r.only_overridable() {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}: ", self.plan.title()), theme.bold()),
                        Span::styled("needs your decision", theme.warn()),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}: ", self.plan.title()), theme.bold()),
                        Span::styled("cannot start as it is", theme.bad()),
                    ]));
                }
                lines.push(Line::raw(""));
                // Problems first: they decide what happens next.
                for b in &r.blockers {
                    let tag = if b.override_flag.is_some() {
                        "PROBLEM (you can continue anyway)"
                    } else {
                        "PROBLEM"
                    };
                    lines.push(Line::from(Span::styled(format!("x {tag}"), theme.bad())));
                    for l in wrap_words(&plain_words(&b.message), width.saturating_sub(3)) {
                        lines.push(Line::from(Span::styled(format!("    {l}"), theme.bad())));
                    }
                    let g = guidance(b, self.is_root);
                    for (n, l) in wrap_words(&g, width.saturating_sub(15))
                        .into_iter()
                        .enumerate()
                    {
                        let lead = if n == 0 {
                            "    What to do: "
                        } else {
                            "                "
                        };
                        lines.push(Line::raw(format!("{lead}{l}")));
                    }
                    lines.push(Line::raw(""));
                }
                for w in &r.warnings {
                    lines.push(Line::from(Span::styled(
                        "! Be careful".to_string(),
                        theme.warn(),
                    )));
                    for l in wrap_words(&plain_words(w), width.saturating_sub(3)) {
                        lines.push(Line::from(Span::styled(format!("    {l}"), theme.warn())));
                    }
                    lines.push(Line::raw(""));
                }
                lines.push(head("What will happen"));
                for d in self.plan.describe() {
                    for (n, l) in wrap_words(&d, width.saturating_sub(4))
                        .into_iter()
                        .enumerate()
                    {
                        lines.push(Line::raw(format!(
                            "{}{l}",
                            if n == 0 { "  - " } else { "    " }
                        )));
                    }
                }
                if self.privileged {
                    lines.push(Line::raw(
                        "  - Your password will be asked for, to read the device. DiskRescue itself keeps its normal rights.",
                    ));
                }
                if !r.facts.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(head("What was found"));
                    for d in &r.facts {
                        for (n, l) in wrap_words(d, width.saturating_sub(4))
                            .into_iter()
                            .enumerate()
                        {
                            lines.push(Line::raw(format!(
                                "{}{l}",
                                if n == 0 { "  - " } else { "    " }
                            )));
                        }
                    }
                }
                if r.is_clear() {
                    bottom.push(Line::from(vec![
                        Span::styled(" Enter ", theme.key()),
                        Span::raw(" start      "),
                        Span::styled(" Esc ", theme.key()),
                        Span::raw(" go back and change something"),
                    ]));
                } else if r.only_overridable() {
                    let mut spans = vec![Span::styled(
                        "To continue anyway, type yes and press Enter:  ",
                        theme.warn(),
                    )];
                    spans.push(Span::raw("["));
                    spans.extend(self.confirm.line(8, true, theme).spans);
                    spans.push(Span::raw("]"));
                    bottom.push(Line::from(spans));
                    bottom.push(Line::from(vec![
                        Span::styled(" Esc ", theme.key()),
                        Span::raw(" go back instead (recommended if you are not sure)"),
                    ]));
                } else {
                    bottom.push(Line::from(vec![
                        Span::styled(" Esc ", theme.key()),
                        Span::raw(" go back and change the settings"),
                    ]));
                }
                if let Some(n) = &self.notice {
                    for l in wrap_words(n, width) {
                        bottom.push(Line::from(Span::styled(l, theme.bad())));
                    }
                }
            }
        }
        // The question and the keys stay in view while the text scrolls.
        let bottom_h = if bottom.is_empty() {
            0
        } else {
            bottom.len() as u16 + 1
        };
        let [body, foot] = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Min(3),
            ratatui::layout::Constraint::Length(bottom_h),
        ])
        .areas(area);
        let foot = Rect {
            y: foot.y + foot.height.saturating_sub(bottom.len() as u16),
            height: (bottom.len() as u16).min(foot.height),
            ..foot
        };
        f.render_widget(Paragraph::new(bottom), foot);
        let area = body;
        let h = area.height as usize;
        let max = lines.len().saturating_sub(h);
        let off = self.scroll.get().min(max);
        self.scroll.set(off);
        let visible: Vec<Line<'static>> = lines.into_iter().skip(off).take(h).collect();
        f.render_widget(Paragraph::new(visible), area);
    }

    /// Scroll the text.
    pub fn scroll_by(&self, delta: i32) {
        let n = (self.scroll.get() as i32 + delta).max(0);
        self.scroll.set(n as usize);
    }
}
