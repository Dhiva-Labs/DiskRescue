//! What a preview found, and the verdicts of a check of loose files.

use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use diskrescue_app::results::verify_verdict;
use diskrescue_app::text::{
    self, bytes, date_text, entry_state_label, extent_label, plural, scheme_label,
};
use diskrescue_recover::pipeline::{Survey, VerifyResult};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::widgets::{ellipsize_start, fit, fit_right, render_list, wrap_words, ListNav, Theme};

/// What the screen wants the app to do.
#[derive(Debug, Clone)]
pub enum ListAction {
    /// Nothing.
    None,
    /// One screen back.
    Back,
    /// To the home screen.
    Home,
    /// Show the help.
    Help,
    /// Recover from the previewed source.
    Recover(PathBuf),
    /// Repair these files.
    Repair(Vec<PathBuf>),
    /// Show a message.
    Status(String),
}

/// The preview of a source.
#[derive(Debug)]
pub struct ScanScreen {
    /// What was found.
    pub survey: Arc<Survey>,
    /// The source.
    pub source: PathBuf,
    /// 0: files found through the filesystem; 1: found in the raw data.
    pub tab: usize,
    /// Selection.
    pub nav: ListNav,
    sorted: Vec<usize>,
}

impl ScanScreen {
    /// Show `survey`.
    pub fn new(survey: Arc<Survey>, source: PathBuf) -> Self {
        let mut sorted: Vec<usize> = (0..survey.files.len()).collect();
        sorted.sort_by(|a, b| {
            let (x, y) = (&survey.files[*a], &survey.files[*b]);
            (x.volume, &x.path).cmp(&(y.volume, &y.path))
        });
        let tab = if survey.files.is_empty() && !survey.carved.is_empty() {
            1
        } else {
            0
        };
        ScanScreen {
            survey,
            source,
            tab,
            nav: ListNav::new(),
            sorted,
        }
    }

    fn len(&self) -> usize {
        if self.tab == 0 {
            self.sorted.len()
        } else {
            self.survey.carved.len()
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ListAction {
        let len = self.len();
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => return ListAction::Back,
            KeyCode::Char('h') => return ListAction::Home,
            KeyCode::Char('?') | KeyCode::F(1) => return ListAction::Help,
            KeyCode::Up | KeyCode::Char('k') => self.nav.move_by(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.nav.move_by(1, len),
            KeyCode::PageUp => self.nav.move_by(-10, len),
            KeyCode::PageDown => self.nav.move_by(10, len),
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len.saturating_sub(1), len),
            KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::BackTab => {
                self.tab = 1 - self.tab;
                self.nav = ListNav::new();
            }
            KeyCode::Char('1') => {
                self.tab = 0;
                self.nav = ListNav::new();
            }
            KeyCode::Char('2') => {
                self.tab = 1;
                self.nav = ListNav::new();
            }
            KeyCode::Char('r') | KeyCode::Enter => return ListAction::Recover(self.source.clone()),
            _ => {}
        }
        ListAction::None
    }

    /// Scroll with the wheel.
    pub fn wheel(&mut self, delta: isize, height: usize) {
        self.nav.scroll(delta, height, self.len());
    }

    /// Select a row by clicking.
    pub fn click_row(&mut self, index: usize) {
        if index < self.len() {
            self.nav.sel = index;
        }
    }

    /// Draw the preview. Returns the rows' area and first visible row.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (Rect, usize) {
        let s = &self.survey;
        let mut head: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                "Preview: this is what could be recovered; nothing was written",
                theme.bold(),
            )),
            Line::from(vec![
                Span::styled("Source: ", theme.dim()),
                Span::raw(ellipsize_start(
                    &self.source.display().to_string(),
                    (area.width as usize).saturating_sub(8),
                )),
            ]),
        ];
        head.push(Line::raw(format!(
            "Partitioning: {}",
            scheme_label(s.scheme)
        )));
        if s.volumes.is_empty() {
            head.push(Line::raw("No volumes were found."));
        }
        for v in s.volumes.iter().take(3) {
            let fs =
                v.fs.map(|f| f.name().to_string())
                    .unwrap_or_else(|| "no filesystem recognised".into());
            head.push(Line::raw(ellipsize_start(
                &format!(
                    "Volume {}: {fs}, {}; {}, {} deleted",
                    v.index,
                    bytes(v.size),
                    plural(v.files_listed, "file", "files"),
                    v.deleted_listed
                ),
                area.width as usize,
            )));
        }
        for w in s.warnings.iter().take(2) {
            head.push(Line::from(Span::styled(
                ellipsize_start(&format!("! {w}"), area.width as usize),
                theme.warn(),
            )));
        }
        let head_h = head.len() as u16 + 1;
        let [h, tabs, list] = Layout::vertical([
            Constraint::Length(head_h),
            Constraint::Length(1),
            Constraint::Min(2),
        ])
        .areas(area);
        f.render_widget(Paragraph::new(head), h);
        let t = |n: usize, label: String| {
            Span::styled(
                format!(" {label} "),
                if self.tab == n {
                    theme.selected()
                } else {
                    theme.dim()
                },
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                t(0, format!("1 Through the filesystem ({})", s.files.len())),
                Span::raw(" "),
                t(1, format!("2 Found in the raw data ({})", s.carved.len())),
            ])),
            tabs,
        );
        let w = list.width as usize;
        let rows: Vec<Line<'static>> = if self.tab == 0 {
            self.sorted
                .iter()
                .map(|&i| {
                    let f = &s.files[i];
                    let state = entry_state_label(f.confidence, f.deleted);
                    Line::from(vec![
                        Span::styled(
                            format!("{} ", fit(state, 17)),
                            if f.deleted {
                                theme.warn()
                            } else {
                                theme.good()
                            },
                        ),
                        Span::raw(format!(
                            "{} {} ",
                            fit_right(&bytes(f.size), 9),
                            fit(&date_text(f.modified), 16)
                        )),
                        Span::raw(ellipsize_start(
                            &format!("vol{}:/{}", f.volume, f.path),
                            w.saturating_sub(46),
                        )),
                    ])
                })
                .collect()
        } else {
            s.carved
                .iter()
                .map(|c| {
                    Line::from(vec![
                        Span::raw(format!(
                            "{} {} {} {} ",
                            fit(&c.format, 8),
                            fit_right(&bytes(c.len), 9),
                            fit(extent_label(c.status), 14),
                            fit(&date_text(c.created), 16)
                        )),
                        Span::styled(format!("at offset {:#x}", c.offset), theme.dim()),
                    ])
                })
                .collect()
        };
        let off = if rows.is_empty() {
            let msg = if self.tab == 1 {
                "The raw data was not searched, or nothing was found there. Choose 'Search the raw data' under More options to look."
            } else {
                "No files were found through the filesystem."
            };
            f.render_widget(
                Paragraph::new(
                    wrap_words(msg, w)
                        .into_iter()
                        .map(|l| Line::from(Span::styled(l, theme.dim())))
                        .collect::<Vec<_>>(),
                ),
                list,
            );
            0
        } else {
            render_list(f, list, &rows, &self.nav, theme, true)
        };
        (list, off)
    }
}

/// The verdicts of a check.
#[derive(Debug)]
pub struct VerifyScreen {
    /// One result per file.
    pub results: Arc<Vec<VerifyResult>>,
    /// Selection.
    pub nav: ListNav,
}

impl VerifyScreen {
    /// Show `results`.
    pub fn new(results: Arc<Vec<VerifyResult>>) -> Self {
        // Start on the first file that needs attention.
        let first = results
            .iter()
            .position(|r| verify_verdict(r).1)
            .unwrap_or(0);
        let mut nav = ListNav::new();
        nav.select(first, results.len());
        VerifyScreen { results, nav }
    }

    /// Files that need attention.
    pub fn problems(&self) -> Vec<PathBuf> {
        self.results
            .iter()
            .filter(|r| verify_verdict(r).1)
            .map(|r| r.path.clone())
            .collect()
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ListAction {
        let len = self.results.len();
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => return ListAction::Back,
            KeyCode::Char('h') => return ListAction::Home,
            KeyCode::Char('?') | KeyCode::F(1) => return ListAction::Help,
            KeyCode::Up | KeyCode::Char('k') => self.nav.move_by(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.nav.move_by(1, len),
            KeyCode::PageUp => self.nav.move_by(-10, len),
            KeyCode::PageDown => self.nav.move_by(10, len),
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len.saturating_sub(1), len),
            KeyCode::Char('r') | KeyCode::Enter => {
                let p = self.problems();
                return if p.is_empty() {
                    ListAction::Status("Every file is intact: nothing needs repair.".into())
                } else {
                    ListAction::Repair(p)
                };
            }
            _ => {}
        }
        ListAction::None
    }

    /// Scroll with the wheel.
    pub fn wheel(&mut self, delta: isize, height: usize) {
        self.nav.scroll(delta, height, self.results.len());
    }

    /// Select a row by clicking.
    pub fn click_row(&mut self, index: usize) {
        if index < self.results.len() {
            self.nav.sel = index;
        }
    }

    /// Draw the verdicts. Returns the rows' area and first visible row.
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (Rect, usize) {
        let problems = self.problems().len();
        let [head, list, detail] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(5),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!(
                        "Check of {}",
                        plural(self.results.len() as u64, "file", "files")
                    ),
                    theme.bold(),
                )),
                Line::from(Span::styled(
                    if problems == 0 {
                        "Every file is intact.".to_string()
                    } else {
                        format!("{problems} need attention. Press r to try to repair them.")
                    },
                    if problems == 0 {
                        theme.good()
                    } else {
                        theme.warn()
                    },
                )),
            ]),
            head,
        );
        let w = list.width as usize;
        let rows: Vec<Line<'static>> = self
            .results
            .iter()
            .map(|r| {
                let (verdict, bad) = verify_verdict(r);
                let st = if bad { theme.bad() } else { theme.good() };
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{} {} ",
                            theme.tone_mark(if bad {
                                text::Tone::Bad
                            } else {
                                text::Tone::Good
                            }),
                            fit(verdict, 12)
                        ),
                        st,
                    ),
                    Span::raw(ellipsize_start(
                        &r.path.display().to_string(),
                        w.saturating_sub(16),
                    )),
                ])
            })
            .collect();
        let off = render_list(f, list, &rows, &self.nav, theme, true);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(r) = self.results.get(self.nav.sel) {
            if let Some(e) = &r.error {
                lines.extend(
                    wrap_words(e, w)
                        .into_iter()
                        .map(|l| Line::from(Span::styled(l, theme.bad()))),
                );
            }
            if let Some(v) = &r.validation {
                for p in v.problems.iter().take(3) {
                    lines.extend(
                        wrap_words(p, w)
                            .into_iter()
                            .take(1)
                            .map(|l| Line::from(Span::styled(l, theme.warn()))),
                    );
                }
                if let Some(fr) = v.intact_fraction {
                    lines.push(Line::raw(format!(
                        "About {:.0}% of the picture or video is intact.",
                        fr * 100.0
                    )));
                }
            }
        }
        f.render_widget(Paragraph::new(lines), detail);
        (list, off)
    }
}
