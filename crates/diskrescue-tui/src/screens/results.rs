//! The results browser: every file of a report, with filters, sorting and
//! a detail pane; and the chooser of a reference file for a repair.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use diskrescue_app::results::{self, ResultItem};
use diskrescue_app::text::{self, bytes, date_text, outcome_label, outcome_tone, plural};
use diskrescue_core::media::MediaCategory;
use diskrescue_recover::report::{Outcome, Report};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::model::plain_words;
use crate::widgets::{ellipsize_start, fit, fit_right, render_list, wrap_words, ListNav, Theme};

/// A report and its files ready to show.
#[derive(Debug)]
pub struct ResultsData {
    /// The output folder the report's paths are relative to.
    pub output: PathBuf,
    /// One item per file.
    pub items: Vec<ResultItem>,
    /// The report.
    pub report: Arc<Report>,
    /// For each item: it is a file the person gave (a repair), not one
    /// that was recovered.
    pub is_input: Vec<bool>,
    /// For each item: the file to act on. A recovered file as written to
    /// the output folder; for a repair, the file that was given (the
    /// repaired copy is `ResultItem::repaired`).
    pub sources: Vec<Option<PathBuf>>,
}

impl ResultsData {
    /// Items of `report`, with absolute paths below `output`.
    pub fn new(output: PathBuf, report: Arc<Report>) -> Self {
        let items = results::items(&report, &output);
        let mut is_input = Vec::with_capacity(items.len());
        let mut sources = Vec::with_capacity(items.len());
        for (item, rec) in items.iter().zip(report.files.iter()) {
            match &rec.origin {
                diskrescue_recover::report::Origin::Input { path } => {
                    is_input.push(true);
                    sources.push(Some(
                        std::path::absolute(path).unwrap_or_else(|_| path.clone()),
                    ));
                }
                _ => {
                    is_input.push(false);
                    sources.push(item.path.clone());
                }
            }
        }
        ResultsData {
            output,
            items,
            report,
            is_input,
            sources,
        }
    }

    /// Load `output/report.json`.
    pub fn load(output: PathBuf) -> Result<Self, String> {
        let report = results::load(&output).map_err(|e| e.message().to_string())?;
        Ok(ResultsData::new(output, Arc::new(report)))
    }
}

/// Which files are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Every file.
    All,
    /// Damaged, partly repaired or failed.
    Damaged,
    /// Repaired (wholly or partly).
    Repaired,
    /// Found as deleted files.
    Deleted,
    /// Photos and RAW photos.
    Photos,
    /// Videos.
    Videos,
}

const FILTERS: [Filter; 6] = [
    Filter::All,
    Filter::Damaged,
    Filter::Repaired,
    Filter::Deleted,
    Filter::Photos,
    Filter::Videos,
];

impl Filter {
    /// Name for the tab.
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Damaged => "Damaged",
            Filter::Repaired => "Repaired",
            Filter::Deleted => "Deleted",
            Filter::Photos => "Photos",
            Filter::Videos => "Videos",
        }
    }

    /// Does `it` pass?
    pub fn matches(self, it: &ResultItem) -> bool {
        match self {
            Filter::All => true,
            Filter::Damaged => matches!(
                it.outcome,
                Outcome::Damaged | Outcome::Failed | Outcome::PartiallyRepaired
            ),
            Filter::Repaired => {
                matches!(it.outcome, Outcome::Repaired | Outcome::PartiallyRepaired)
            }
            Filter::Deleted => it.deleted,
            Filter::Photos => matches!(it.category, MediaCategory::Photo | MediaCategory::RawPhoto),
            Filter::Videos => it.category == MediaCategory::Video,
        }
    }
}

/// How the list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// By name.
    Name,
    /// Biggest first.
    Size,
    /// By capture date.
    Date,
    /// Problems first.
    Result,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Date => "date",
            Sort::Result => "result",
        }
    }

    fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Size,
            Sort::Size => Sort::Date,
            Sort::Date => Sort::Result,
            Sort::Result => Sort::Name,
        }
    }
}

fn badness(o: Outcome) -> u8 {
    match o {
        Outcome::Failed => 0,
        Outcome::Damaged => 1,
        Outcome::PartiallyRepaired => 2,
        Outcome::RecoveredUnverified => 3,
        Outcome::Repaired => 4,
        Outcome::Recovered => 5,
        Outcome::Duplicate => 6,
    }
}

/// What the screen wants the app to do.
#[derive(Debug, Clone)]
pub enum ResAction {
    /// Nothing.
    None,
    /// One screen back.
    Back,
    /// To the home screen.
    Home,
    /// Show the help.
    Help,
    /// Open this folder.
    OpenFolder(PathBuf),
    /// Choose a reference file for item number `n` of the data.
    Donor(usize),
    /// Show a message.
    Status(String),
}

/// The results browser.
#[derive(Debug)]
pub struct ResultsScreen {
    /// The files.
    pub data: Arc<ResultsData>,
    /// The title (what the report is of).
    pub title: String,
    /// Active filter.
    pub filter: Filter,
    /// Active order.
    pub sort: Sort,
    /// Order reversed.
    pub reversed: bool,
    /// Items passing the filter, in order (indices into `data.items`).
    pub view: Vec<usize>,
    /// Selection.
    pub nav: ListNav,
    /// Scroll of the detail pane.
    pub detail_scroll: u16,
}

impl ResultsScreen {
    /// Show `data`.
    pub fn new(data: Arc<ResultsData>, title: impl Into<String>) -> Self {
        let mut s = ResultsScreen {
            data,
            title: title.into(),
            filter: Filter::All,
            sort: Sort::Name,
            reversed: false,
            view: Vec::new(),
            nav: ListNav::new(),
            detail_scroll: 0,
        };
        // Problems first is the useful default when there are any.
        s.refresh();
        s
    }

    /// Number of files that pass `f`.
    pub fn count(&self, f: Filter) -> usize {
        self.data.items.iter().filter(|i| f.matches(i)).count()
    }

    fn refresh(&mut self) {
        let items = &self.data.items;
        let mut v: Vec<usize> = (0..items.len())
            .filter(|i| self.filter.matches(&items[*i]))
            .collect();
        let sort = self.sort;
        v.sort_by(|a, b| {
            let (x, y) = (&items[*a], &items[*b]);
            let name = |i: &ResultItem| {
                // Files that were not written (duplicates) come last.
                i.path
                    .as_ref()
                    .map(|p| format!("0{}", p.display().to_string().to_lowercase()))
                    .unwrap_or_else(|| "1".to_string())
            };
            let o = match sort {
                Sort::Name => name(x).cmp(&name(y)),
                Sort::Size => y.size.cmp(&x.size),
                Sort::Date => x
                    .captured
                    .map(|t| t.to_rfc3339())
                    .cmp(&y.captured.map(|t| t.to_rfc3339()))
                    .then_with(|| name(x).cmp(&name(y))),
                Sort::Result => badness(x.outcome)
                    .cmp(&badness(y.outcome))
                    .then_with(|| name(x).cmp(&name(y))),
            };
            if o == Ordering::Equal {
                a.cmp(b)
            } else {
                o
            }
        });
        if self.reversed {
            v.reverse();
        }
        self.view = v;
        self.nav.clamp(self.view.len());
        self.detail_scroll = 0;
    }

    /// Select the filter `f`.
    pub fn set_filter(&mut self, f: Filter) {
        self.filter = f;
        self.nav.sel = 0;
        self.refresh();
    }

    /// The item under the selection.
    pub fn selected(&self) -> Option<(usize, &ResultItem)> {
        let i = *self.view.get(self.nav.sel)?;
        Some((i, &self.data.items[i]))
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ResAction {
        let len = self.view.len();
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => return ResAction::Back,
            KeyCode::Char('h') => return ResAction::Home,
            KeyCode::Char('?') | KeyCode::F(1) => return ResAction::Help,
            KeyCode::Up | KeyCode::Char('k') => {
                self.nav.move_by(-1, len);
                self.detail_scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.nav.move_by(1, len);
                self.detail_scroll = 0;
            }
            KeyCode::PageUp => {
                self.nav.move_by(-10, len);
                self.detail_scroll = 0;
            }
            KeyCode::PageDown => {
                self.nav.move_by(10, len);
                self.detail_scroll = 0;
            }
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len.saturating_sub(1), len),
            KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                let n = FILTERS.iter().position(|f| *f == self.filter).unwrap_or(0);
                self.set_filter(FILTERS[(n + 1) % FILTERS.len()]);
            }
            KeyCode::Left | KeyCode::BackTab => {
                let n = FILTERS.iter().position(|f| *f == self.filter).unwrap_or(0);
                self.set_filter(FILTERS[(n + FILTERS.len() - 1) % FILTERS.len()]);
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.set_filter(FILTERS[(c as u8 - b'1') as usize]);
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.reversed = false;
                self.refresh();
            }
            KeyCode::Char('S') => {
                self.reversed = !self.reversed;
                self.refresh();
            }
            KeyCode::Char('J') => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('K') => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Char('o') | KeyCode::Enter => {
                let dir = match self.selected().and_then(|(i, it)| {
                    it.repaired
                        .clone()
                        .or_else(|| it.path.clone())
                        .or_else(|| self.data.sources[i].clone())
                }) {
                    Some(p) => p.parent().map(PathBuf::from).unwrap_or(p),
                    None => self.data.output.clone(),
                };
                return ResAction::OpenFolder(dir);
            }
            KeyCode::Char('O') => return ResAction::OpenFolder(self.data.output.clone()),
            KeyCode::Char('r') => {
                let Some((i, it)) = self.selected() else {
                    return ResAction::None;
                };
                let damaged = matches!(it.outcome, Outcome::Damaged | Outcome::PartiallyRepaired);
                if !damaged {
                    return ResAction::Status(
                        "Only damaged files can be repaired with a reference file. Select one marked damaged."
                            .into(),
                    );
                }
                if self.data.sources[i].is_none() {
                    return ResAction::Status(
                        "This file was not written, so it cannot be repaired.".into(),
                    );
                }
                return ResAction::Donor(i);
            }
            _ => {}
        }
        ResAction::None
    }

    /// Select a row by clicking.
    pub fn click_row(&mut self, index: usize) {
        if index < self.view.len() {
            self.nav.sel = index;
            self.detail_scroll = 0;
        }
    }

    /// Choose a filter by clicking its tab (`index` into the tabs).
    pub fn click_tab(&mut self, index: usize) {
        if let Some(f) = FILTERS.get(index) {
            self.set_filter(*f);
        }
    }

    /// Scroll the list with the wheel.
    pub fn wheel(&mut self, delta: isize, height: usize) {
        self.nav.scroll(delta, height, self.view.len());
    }

    /// Title and summary lines for the top of the screen.
    pub fn summary(&self) -> String {
        let t = &self.data.report.totals;
        let c = |name: &str| t.by_outcome.get(name).copied().unwrap_or(0);
        let items = &self.data.items;
        let n = |o: Outcome| items.iter().filter(|i| i.outcome == o).count();
        let _ = c;
        format!(
            "{} - {} ok, {} damaged, {} repaired, {} deleted",
            plural(items.len() as u64, "file", "files"),
            n(Outcome::Recovered) + n(Outcome::RecoveredUnverified),
            n(Outcome::Damaged) + n(Outcome::Failed),
            n(Outcome::Repaired) + n(Outcome::PartiallyRepaired),
            items.iter().filter(|i| i.deleted).count()
        )
    }

    /// Draw the browser. Returns the list area and its first visible row,
    /// and the tab areas (for the mouse).
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> ResultsLayout {
        let [head, tabs, body] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("Results in ", theme.bold()),
                    Span::raw(ellipsize_start(
                        &self.title,
                        (area.width as usize).saturating_sub(12),
                    )),
                ]),
                Line::from(Span::styled(
                    format!(
                        "{}   sorted by {}{}",
                        self.summary(),
                        self.sort.label(),
                        if self.reversed { " (reversed)" } else { "" }
                    ),
                    theme.dim(),
                )),
            ]),
            head,
        );
        // Filter tabs.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut tab_areas = Vec::new();
        let mut x = tabs.x;
        for fl in FILTERS {
            let text = format!(" {} ({}) ", fl.label(), self.count(fl));
            let w = text.chars().count() as u16;
            tab_areas.push(Rect {
                x,
                y: tabs.y,
                width: w,
                height: 1,
            });
            x += w + 1;
            spans.push(Span::styled(
                text,
                if fl == self.filter {
                    theme.selected()
                } else {
                    theme.dim()
                },
            ));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), tabs);

        let wide = area.width >= 100;
        let (list_area, detail_area) = if wide {
            let [l, r] =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .areas(body);
            (l, r)
        } else {
            let h = (body.height * 45 / 100).max(4);
            let [l, r] = Layout::vertical([Constraint::Min(4), Constraint::Length(h)]).areas(body);
            (l, r)
        };
        // The list.
        let block = theme.block(" Files ");
        let inner = block.inner(list_area);
        f.render_widget(block, list_area);
        let [colhead, rows_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        let w = inner.width as usize;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "  {} {} {} {} FILE",
                    fit("RESULT", 15),
                    fit("", 4),
                    fit_right("SIZE", 9),
                    fit("TYPE", 5)
                ),
                theme.dim(),
            ))),
            colhead,
        );
        let rows: Vec<Line<'static>> = self
            .view
            .iter()
            .map(|&i| item_row(&self.data, i, w, theme))
            .collect();
        let off = if rows.is_empty() {
            let msg = if self.data.items.is_empty() {
                "No files were recovered."
            } else {
                "No files match this filter. Press Tab for the next one."
            };
            f.render_widget(Paragraph::new(Span::styled(msg, theme.dim())), rows_area);
            0
        } else {
            render_list(f, rows_area, &rows, &self.nav, theme, true)
        };
        // The details.
        let block = theme.block(" Details ");
        let inner = block.inner(detail_area);
        f.render_widget(block, detail_area);
        let lines = match self.selected() {
            Some((i, _)) => detail_lines(&self.data, i, inner.width as usize, theme),
            None => vec![Line::from(Span::styled("Nothing selected.", theme.dim()))],
        };
        let max = lines.len().saturating_sub(inner.height as usize) as u16;
        let scroll = self.detail_scroll.min(max);
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
        ResultsLayout {
            rows: rows_area,
            offset: off,
            tabs: tab_areas,
        }
    }
}

/// Where things were drawn, for the mouse.
#[derive(Debug, Clone, Default)]
pub struct ResultsLayout {
    /// The rows of the list.
    pub rows: Rect,
    /// First visible row.
    pub offset: usize,
    /// The filter tabs.
    pub tabs: Vec<Rect>,
}

fn kind_word(c: MediaCategory) -> &'static str {
    match c {
        MediaCategory::Photo => "photo",
        MediaCategory::RawPhoto => "raw",
        MediaCategory::Video => "video",
        MediaCategory::Audio => "audio",
        MediaCategory::Document => "doc",
        MediaCategory::Other => "other",
    }
}

fn display_name(data: &ResultsData, i: usize) -> String {
    let it = &data.items[i];
    match it.repaired.as_ref().or(it.path.as_ref()) {
        Some(p) => p
            .strip_prefix(&data.output)
            .unwrap_or(p)
            .display()
            .to_string(),
        None => match &data.sources[i] {
            // A file that needed no repair, or could not be repaired, stays
            // where it is.
            Some(p) if data.is_input[i] => p.display().to_string(),
            _ => "(not written)".to_string(),
        },
    }
}

/// The outcome in few letters, for the table.
fn short_outcome(o: Outcome) -> &'static str {
    match o {
        Outcome::Recovered => "recovered",
        Outcome::RecoveredUnverified => "unverified",
        Outcome::Damaged => "DAMAGED",
        Outcome::Repaired => "repaired",
        Outcome::PartiallyRepaired => "partly repaired",
        Outcome::Duplicate => "duplicate",
        Outcome::Failed => "FAILED",
    }
}

fn item_row(data: &ResultsData, i: usize, width: usize, theme: &Theme) -> Line<'static> {
    let it = &data.items[i];
    let tone = outcome_tone(it.outcome);
    let name_w = width.saturating_sub(2 + 16 + 5 + 10 + 6);
    Line::from(vec![
        Span::styled(format!("{} ", theme.tone_mark(tone)), theme.tone(tone)),
        Span::styled(
            format!("{} ", fit(short_outcome(it.outcome), 15)),
            theme.tone(tone),
        ),
        Span::styled(if it.deleted { "del  " } else { "     " }, theme.warn()),
        Span::raw(format!("{} ", fit_right(&bytes(it.size), 9))),
        Span::raw(format!("{} ", fit(kind_word(it.category), 5))),
        Span::raw(ellipsize_start(&display_name(data, i), name_w)),
    ])
}

fn kv(
    k: &str,
    v: &str,
    width: usize,
    theme: &Theme,
    style: Option<ratatui::style::Style>,
) -> Vec<Line<'static>> {
    let lead = 12usize;
    let parts = wrap_words(v, width.saturating_sub(lead).max(8));
    parts
        .into_iter()
        .enumerate()
        .map(|(n, p)| {
            let key = if n == 0 {
                format!("{k:<lead$}")
            } else {
                " ".repeat(lead)
            };
            Line::from(vec![
                Span::styled(key, theme.dim()),
                match style {
                    Some(s) => Span::styled(p, s),
                    None => Span::raw(p),
                },
            ])
        })
        .collect()
}

/// The detail pane of a file.
pub fn detail_lines(
    data: &ResultsData,
    i: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let it = &data.items[i];
    let output = data.output.as_path();
    let is_input = data.is_input[i];
    let mut v: Vec<Line<'static>> = Vec::new();
    let tone = outcome_tone(it.outcome);
    let name = it
        .path
        .as_ref()
        .or(data.sources[i].as_ref())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(not written)".to_string());
    v.push(Line::from(Span::styled(name, theme.bold())));
    let mut result = outcome_label(it.outcome).to_string();
    if it.deleted {
        result.push_str(", found as a deleted file");
    }
    v.extend(kv("Result", &result, width, theme, Some(theme.tone(tone))));
    if matches!(it.outcome, Outcome::Damaged | Outcome::PartiallyRepaired)
        && data.sources[i].is_some()
    {
        v.push(Line::from(Span::styled(
            "Press r to repair it with a healthy reference file.",
            theme.accent(),
        )));
    }
    if is_input {
        if let Some(p) = &data.sources[i] {
            v.extend(kv("Original", &p.display().to_string(), width, theme, None));
        }
        if let Some(p) = &it.repaired {
            v.extend(kv(
                "Repaired as",
                &p.strip_prefix(output).unwrap_or(p).display().to_string(),
                width,
                theme,
                None,
            ));
        }
    } else {
        if let Some(p) = &it.path {
            v.extend(kv(
                "Saved as",
                &p.strip_prefix(output).unwrap_or(p).display().to_string(),
                width,
                theme,
                None,
            ));
        }
        if let Some(p) = &it.repaired {
            v.extend(kv(
                "Repaired as",
                &p.strip_prefix(output).unwrap_or(p).display().to_string(),
                width,
                theme,
                None,
            ));
        }
        v.extend(kv("Found at", &it.origin, width, theme, None));
    }
    let mut kind = vec![kind_word(it.category).to_string()];
    if let Some(f) = &it.format {
        kind.push(f.clone());
    }
    if let Some(d) = it.meta.dimensions() {
        kind.push(d);
    }
    if let Some(ms) = it.meta.duration_ms {
        kind.push(format!(
            "{} long",
            text::duration(std::time::Duration::from_millis(ms))
        ));
    }
    kind.push(bytes(it.size));
    v.extend(kv("Type", &kind.join(", "), width, theme, None));
    let camera = [it.meta.make.as_deref(), it.meta.model.as_deref()]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    v.extend(kv(
        "Taken",
        &if camera.is_empty() {
            date_text(it.captured)
        } else {
            format!("{}  with {camera}", date_text(it.captured))
        },
        width,
        theme,
        None,
    ));
    if it.problems.is_empty() {
        v.extend(kv(
            "Problems",
            "none found",
            width,
            theme,
            Some(theme.good()),
        ));
    } else {
        for (n, p) in it.problems.iter().enumerate() {
            v.extend(kv(
                if n == 0 { "Problems" } else { "" },
                &plain_words(p),
                width,
                theme,
                Some(theme.warn()),
            ));
        }
    }
    if it.unreadable_bytes > 0 {
        v.extend(kv(
            "Unreadable",
            &format!(
                "{} of this file could not be read from the device and are filled with zeros",
                bytes(it.unreadable_bytes)
            ),
            width,
            theme,
            Some(theme.warn()),
        ));
    }
    for (n, note) in it.notes.iter().enumerate() {
        v.extend(kv(
            if n == 0 { "Notes" } else { "" },
            note,
            width,
            theme,
            None,
        ));
    }
    v
}

// ---------------------------------------------------------------- donors

/// How well a file suits as a reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Donor {
    /// The file.
    pub path: PathBuf,
    /// Why it was suggested.
    pub note: String,
    /// Same format as the damaged file.
    pub same_format: bool,
    /// Size of the file.
    pub size: u64,
    score: u32,
}

/// Healthy files of `data` ranked as references for `damaged` (an index
/// into the items): the same format first, then the same camera, then the
/// same picture size.
pub fn rank_donors(data: &ResultsData, damaged: usize) -> Vec<Donor> {
    let target = &data.items[damaged];
    let mut out = Vec::new();
    for (i, it) in data.items.iter().enumerate() {
        if i == damaged
            || !matches!(
                it.outcome,
                Outcome::Recovered | Outcome::RecoveredUnverified
            )
        {
            continue;
        }
        let Some(path) = data.sources[i].clone() else {
            continue;
        };
        let same_format = it.format.is_some() && it.format == target.format;
        let mut score = 0u32;
        let mut why = Vec::new();
        if same_format {
            score += 100;
            why.push(format!(
                "same format ({})",
                it.format.clone().unwrap_or_default()
            ));
        } else {
            why.push(format!(
                "different format ({})",
                it.format.clone().unwrap_or_else(|| "?".into())
            ));
        }
        if it.meta.model.is_some()
            && it.meta.model == target.meta.model
            && it.meta.make == target.meta.make
        {
            score += 10;
            why.push("same camera".to_string());
        }
        if it.meta.dimensions().is_some() && it.meta.dimensions() == target.meta.dimensions() {
            score += 5;
            why.push("same picture size".to_string());
        }
        if it.category == target.category {
            score += 1;
        }
        out.push(Donor {
            path,
            note: why.join(", "),
            same_format,
            size: it.size,
            score,
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    out.truncate(200);
    out
}

/// What the donor chooser wants the app to do.
#[derive(Debug, Clone)]
pub enum DonorAction {
    /// Nothing.
    None,
    /// Back to the results.
    Back,
    /// Show the help.
    Help,
    /// Repair with this reference.
    Chosen(PathBuf),
    /// Browse for another file.
    Browse,
}

/// The chooser of a reference file.
#[derive(Debug)]
pub struct DonorScreen {
    /// The results the damaged file is from.
    pub data: Arc<ResultsData>,
    /// The damaged item.
    pub target: usize,
    /// The suggestions.
    pub donors: Vec<Donor>,
    /// Selection.
    pub nav: ListNav,
}

impl DonorScreen {
    /// Suggest references for item `target`.
    pub fn new(data: Arc<ResultsData>, target: usize) -> Self {
        let donors = rank_donors(&data, target);
        DonorScreen {
            data,
            target,
            donors,
            nav: ListNav::new(),
        }
    }

    fn rows(&self) -> usize {
        self.donors.len() + 1
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> DonorAction {
        let len = self.rows();
        match key.code {
            KeyCode::Esc => return DonorAction::Back,
            KeyCode::Char('?') | KeyCode::F(1) => return DonorAction::Help,
            KeyCode::Up | KeyCode::Char('k') => self.nav.move_by(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.nav.move_by(1, len),
            KeyCode::PageUp => self.nav.move_by(-8, len),
            KeyCode::PageDown => self.nav.move_by(8, len),
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len - 1, len),
            KeyCode::Char('b') => return DonorAction::Browse,
            KeyCode::Enter => return self.activate(),
            _ => {}
        }
        DonorAction::None
    }

    fn activate(&self) -> DonorAction {
        match self.donors.get(self.nav.sel) {
            Some(d) => DonorAction::Chosen(d.path.clone()),
            None => DonorAction::Browse,
        }
    }

    /// Click a row; a second click on the same row chooses it.
    pub fn click_row(&mut self, index: usize) -> DonorAction {
        if index >= self.rows() {
            return DonorAction::None;
        }
        if self.nav.sel == index {
            return self.activate();
        }
        self.nav.sel = index;
        DonorAction::None
    }

    /// Draw the chooser. Returns the rows' area and first visible row.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (Rect, usize) {
        let [head, list] =
            Layout::vertical([Constraint::Length(5), Constraint::Min(2)]).areas(area);
        let name = self.data.sources[self.target]
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut lines = vec![
            Line::from(Span::styled("Repair with a reference file", theme.bold())),
            Line::raw(format!(
                "Damaged file: {}",
                ellipsize_start(&name, area.width.saturating_sub(15) as usize)
            )),
        ];
        for l in wrap_words(
            "Choose a healthy photo or video made by the same camera or phone with the same settings. DiskRescue borrows what the damaged file is missing from it.",
            area.width as usize,
        ) {
            lines.push(Line::from(Span::styled(l, theme.dim())));
        }
        f.render_widget(Paragraph::new(lines), head);
        let mut rows: Vec<Line<'static>> = self
            .donors
            .iter()
            .map(|d| {
                let name = d
                    .path
                    .strip_prefix(&self.data.output)
                    .unwrap_or(&d.path)
                    .display()
                    .to_string();
                Line::from(vec![
                    Span::styled(
                        if d.same_format { "best  " } else { "other " },
                        if d.same_format {
                            theme.good()
                        } else {
                            theme.dim()
                        },
                    ),
                    Span::raw(format!(
                        "{} ({}; {})",
                        ellipsize_start(&name, (area.width as usize).saturating_sub(45).max(12)),
                        bytes(d.size),
                        d.note
                    )),
                ])
            })
            .collect();
        rows.push(Line::from(Span::styled(
            "Browse for another file...",
            theme.accent(),
        )));
        let off = render_list(f, list, &rows, &self.nav, theme, true);
        (list, off)
    }
}
