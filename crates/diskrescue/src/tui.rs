//! Full-screen dashboard for `diskrescue image`.
//!
//! The imaging engine runs on its own thread and sends events through a
//! channel; this module drains them four times a second, keeps the latest
//! state and redraws. It never blocks the engine. The terminal is restored
//! on every way out, including panics and a forced quit.
//!
//! Keys: `q` or `Esc` stop and save (resume later), `p` pause or resume,
//! `Ctrl-C` behaves like `q` (twice within three seconds quits at once).

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use diskrescue_core::util::human;
use diskrescue_core::Range;
use diskrescue_imaging::{
    render_window, BlockStatus, Control, Event, Phase, Progress, StatusMap, Summary,
};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Widget};
use ratatui::{Frame, Terminal};

use crate::signal::{Interrupt, Step};
use crate::util::{percent, plural};

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while the alternate screen is shown.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Give the terminal back (idempotent; safe to call from a panic hook or a
/// signal handler thread).
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

/// An open full-screen terminal session.
pub struct Session {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Session {
    /// Switch the terminal to the alternate screen in raw mode.
    pub fn start() -> io::Result<Session> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        ACTIVE.store(true, Ordering::SeqCst);
        match Terminal::new(CrosstermBackend::new(out)) {
            Ok(terminal) => Ok(Session { terminal }),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore();
    }
}

/// What the header shows.
#[derive(Debug, Clone, Default)]
pub struct Header {
    /// Device or image being read.
    pub source: String,
    /// Model, kind and size.
    pub detail: String,
    /// Imaging profile name and the destination image.
    pub profile: String,
}

/// Everything the dashboard shows.
pub struct Dashboard {
    header: Header,
    phase: Option<Phase>,
    progress: Option<Progress>,
    map: Option<Arc<StatusMap>>,
    logs: VecDeque<String>,
    paused: bool,
    saving: bool,
    color: bool,
    finished: Option<Summary>,
    started: Instant,
    last_saved: Option<Instant>,
}

/// Lines kept for the log panel.
const LOG_KEEP: usize = 200;
/// Lines the log panel shows.
const LOG_SHOWN: usize = 8;

impl Dashboard {
    /// A dashboard with nothing received yet.
    pub fn new(header: Header, color: bool) -> Self {
        Dashboard {
            header,
            phase: None,
            progress: None,
            map: None,
            logs: VecDeque::new(),
            paused: false,
            saving: false,
            color,
            finished: None,
            started: Instant::now(),
            last_saved: None,
        }
    }

    /// Append a line to the log panel.
    pub fn push_log(&mut self, text: String) {
        let t = self.started.elapsed().as_secs();
        let line = format!("{:02}:{:02}  {text}", t / 60, t % 60);
        if self.logs.len() >= LOG_KEEP {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    /// Take an engine event into account.
    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Progress(p) => {
                self.phase = Some(p.phase);
                self.progress = Some(p.clone());
            }
            Event::Phase(p) => {
                self.phase = Some(*p);
                self.push_log(p.to_string());
            }
            Event::Log(_, msg) if crate::cmd::image::is_engine_chatter(msg) => {}
            Event::Log(level, msg) => {
                let tag = match level {
                    log::Level::Error => "error: ",
                    log::Level::Warn => "warning: ",
                    _ => "",
                };
                self.push_log(format!("{tag}{msg}"));
            }
            Event::DeviceLost { at } => self.push_log(format!(
                "The device stopped responding at {}. Waiting for it to come back...",
                human::bytes(*at)
            )),
            Event::DeviceBack { after } => self.push_log(format!(
                "The device is back after {}; continuing.",
                human::duration(*after)
            )),
            Event::Paused(d) => self.push_log(format!(
                "Pausing for {} to let the device recover.",
                human::duration(*d)
            )),
            Event::Map(m) => self.map = Some(m.clone()),
            Event::Saved => self.last_saved = Some(Instant::now()),
            Event::Finished(s) => self.finished = Some(s.clone()),
        }
    }

    /// Show the "saving" notice (after the user asked to stop).
    pub fn set_saving(&mut self) {
        self.saving = true;
    }

    /// Mirror the engine's pause state.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn style(&self, s: Style) -> Style {
        if self.color {
            s
        } else {
            Style::default()
        }
    }

    /// Draw the whole screen.
    pub fn render(&self, f: &mut Frame) {
        let area = f.area();
        if area.width < 52 || area.height < 15 {
            self.render_compact(f, area);
            return;
        }
        let side_by_side = area.width >= 104;
        let stacked_fits = area.height >= 16 + 11 + 10;
        let bottom_h: u16 = if side_by_side {
            11
        } else if stacked_fits {
            11 + 10
        } else {
            11
        };
        let [head, map, bottom, foot] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(4),
            Constraint::Length(bottom_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.render_header(f, head);
        self.render_map(f, map);
        if side_by_side {
            let [l, r] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(bottom);
            self.render_stats(f, l);
            self.render_log(f, r);
        } else if stacked_fits {
            let [s, l] =
                Layout::vertical([Constraint::Length(11), Constraint::Length(10)]).areas(bottom);
            self.render_stats(f, s);
            self.render_log(f, l);
        } else {
            self.render_stats(f, bottom);
        }
        self.render_footer(f, foot);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let title = self.style(Style::new().add_modifier(Modifier::BOLD));
        let dim = self.style(Style::new().fg(Color::Gray));
        let lines = vec![
            Line::from(vec![
                Span::styled("DiskRescue ", title),
                Span::raw(format!("copying {}", self.header.source)),
            ]),
            Line::from(Span::styled(self.header.detail.clone(), dim)),
            Line::from(Span::styled(self.header.profile.clone(), dim)),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_map(&self, f: &mut Frame, area: Rect) {
        let title = match &self.map {
            Some(m) if m.size() > 0 => {
                let cells = u64::from(area.width.saturating_sub(2))
                    * u64::from(area.height.saturating_sub(3));
                match m.size().checked_div(cells) {
                    Some(per) => format!(" Map: each square is about {} ", human::bytes(per)),
                    None => " Map ".to_string(),
                }
            }
            _ => " Map ".to_string(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 2 || inner.width < 4 {
            return;
        }
        let map_area = Rect {
            height: inner.height - 1,
            ..inner
        };
        let legend_area = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        f.render_widget(
            MapWidget {
                map: self.map.as_deref(),
                current: self.progress.as_ref().map(|p| p.current_pos),
                color: self.color,
            },
            map_area,
        );
        f.render_widget(Paragraph::new(self.legend()), legend_area);
    }

    fn legend(&self) -> Line<'static> {
        let mut spans = Vec::new();
        for (st, name) in [
            (BlockStatus::Finished, "copied"),
            (BlockStatus::NonTried, "not read"),
            (BlockStatus::NonTrimmed, "to examine"),
            (BlockStatus::NonScraped, "to retry"),
            (BlockStatus::BadSector, "unreadable"),
        ] {
            spans.push(self.cell_span(Some(st), false));
            spans.push(Span::raw(format!(" {name}  ")));
        }
        spans.push(self.cell_span(None, true));
        spans.push(Span::raw(" now"));
        Line::from(spans)
    }

    fn cell_span(&self, st: Option<BlockStatus>, current: bool) -> Span<'static> {
        let (sym, style) = cell_look(st, current, self.color);
        Span::styled(sym.to_string(), style)
    }

    fn render_stats(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Progress ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 2 {
            return;
        }
        let [gauge_area, text_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        let (ratio, label) = match &self.progress {
            Some(p) => {
                let total = p.stats.total();
                (
                    crate::util::fraction(p.stats.rescued, total),
                    format!("{} copied", percent(p.stats.rescued, total)),
                )
            }
            None => (0.0, "starting...".to_string()),
        };
        let gauge_style = self.style(Style::new().fg(Color::Green).bg(Color::DarkGray));
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .label(label)
                .gauge_style(gauge_style),
            gauge_area,
        );
        f.render_widget(Paragraph::new(self.stat_lines()), text_area);
    }

    fn stat_lines(&self) -> Vec<Line<'static>> {
        let key = self.style(Style::new().fg(Color::Cyan));
        let bad = self.style(Style::new().fg(Color::Red));
        let kv = |k: &str, v: String| -> Line<'static> {
            Line::from(vec![Span::styled(format!("{k:<11}"), key), Span::raw(v)])
        };
        let Some(p) = &self.progress else {
            return vec![kv("Phase", "starting".into())];
        };
        let s = &p.stats;
        let phase = self.phase.unwrap_or(p.phase).to_string();
        let mut lines = vec![
            kv("Phase", phase),
            kv(
                "Position",
                format!(
                    "{} of {}",
                    human::bytes(p.current_pos),
                    human::bytes(p.size)
                ),
            ),
            kv(
                "Copied",
                format!(
                    "{} ({})",
                    human::bytes(s.rescued),
                    percent(s.rescued, s.total())
                ),
            ),
        ];
        let unreadable = if s.bad_sectors > 0 {
            Line::from(vec![
                Span::styled(format!("{:<11}", "Unreadable"), key),
                Span::styled(
                    format!(
                        "{} in {}",
                        human::bytes(s.bad_sectors),
                        crate::util::plural(s.bad_areas, "area", "areas")
                    ),
                    bad,
                ),
            ])
        } else {
            kv("Unreadable", "none so far".into())
        };
        lines.push(unreadable);
        lines.push(kv(
            "Left to do",
            format!(
                "{} not read, {} to re-examine",
                human::bytes(s.non_tried),
                human::bytes(s.non_trimmed + s.non_scraped)
            ),
        ));
        lines.push(kv(
            "Problems",
            format!(
                "{}, {}, {} slow, {}",
                plural(p.errors, "error", "errors"),
                plural(p.timeouts, "timeout", "timeouts"),
                p.slow_reads,
                plural(p.reattaches, "reconnect", "reconnects")
            ),
        ));
        lines.push(kv(
            "Speed",
            format!(
                "{} now, {} average",
                human::rate(p.current_rate),
                human::rate(p.average_rate)
            ),
        ));
        let eta = match p.eta {
            Some(d) if d > Duration::ZERO => format!("~{} left in phase", human::duration(d)),
            Some(_) => "phase almost done".to_string(),
            None => "estimating...".to_string(),
        };
        lines.push(kv(
            "Time",
            format!("{} elapsed, {eta}", human::duration(p.elapsed)),
        ));
        lines
    }

    fn render_log(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Messages ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let width = (inner.width as usize).max(8);
        let warn = self.style(Style::new().fg(Color::Yellow));
        // The latest messages, wrapped to the panel, newest at the bottom.
        let mut visual: Vec<(String, bool)> = Vec::new();
        for l in self.logs.iter().rev().take(LOG_SHOWN).rev() {
            let loud =
                l.contains("warning:") || l.contains("stopped responding") || l.contains("error:");
            // "mm:ss  " prefix: continuation lines are indented under the text.
            let (stamp, text) = l.split_at(l.find("  ").map(|i| i + 2).unwrap_or(0));
            let indent = " ".repeat(stamp.chars().count());
            for (n, part) in crate::term::wrap_words(text, width.saturating_sub(indent.len()))
                .into_iter()
                .enumerate()
            {
                let lead = if n == 0 {
                    stamp.to_string()
                } else {
                    indent.clone()
                };
                visual.push((format!("{lead}{part}"), loud));
            }
        }
        let skip = visual.len().saturating_sub(inner.height as usize);
        let lines: Vec<Line<'static>> = visual
            .into_iter()
            .skip(skip)
            .map(|(text, loud)| {
                if loud {
                    Line::from(Span::styled(text, warn))
                } else {
                    Line::from(text)
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let hot = self.style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        let warn = self.style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        let line = if self.finished.is_some() {
            Line::from(Span::styled(" Finished. ", warn))
        } else if self.saving {
            Line::from(Span::styled(
                " Saving progress and stopping... (Ctrl-C twice to quit now) ",
                warn,
            ))
        } else if self.paused {
            Line::from(vec![
                Span::styled(" PAUSED ", warn),
                Span::raw("  press "),
                Span::styled(" p ", hot),
                Span::raw(" to continue, "),
                Span::styled(" q ", hot),
                Span::raw(" to stop and save"),
            ])
        } else {
            let saved = match self.last_saved {
                Some(t) => format!("   last saved {} ago", human::duration(t.elapsed())),
                None => String::new(),
            };
            Line::from(vec![
                Span::styled(" q ", hot),
                Span::raw(" stop and save (you can resume later)   "),
                Span::styled(" p ", hot),
                Span::raw(format!(" pause{saved}")),
            ])
        };
        f.render_widget(Paragraph::new(line), area);
    }

    /// Text-only view for terminals that are too small for the dashboard.
    fn render_compact(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(format!("DiskRescue: {}", self.header.source))];
        if let Some(p) = &self.progress {
            let s = &p.stats;
            lines.push(Line::from(format!(
                "{} copied ({})",
                percent(s.rescued, s.total()),
                human::bytes(s.rescued)
            )));
            lines.push(Line::from(format!(
                "unreadable: {}",
                human::bytes(s.bad_sectors)
            )));
            lines.push(Line::from(format!(
                "speed: {}",
                human::rate(p.current_rate)
            )));
            if let Some(eta) = p.eta {
                lines.push(Line::from(format!("left: {}", human::duration(eta))));
            }
        } else {
            lines.push(Line::from("starting..."));
        }
        lines.push(Line::from(if self.paused {
            "PAUSED - p continues, q stops"
        } else if self.saving {
            "saving..."
        } else {
            "q stop and save, p pause"
        }));
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// "3 read errors, 1 timeout, 0 slow reads, 0 reconnects".
pub fn problems_text(errors: u64, timeouts: u64, slow: u64, reconnects: u64) -> String {
    format!(
        "{}, {}, {}, {}",
        plural(errors, "read error", "read errors"),
        plural(timeouts, "timeout", "timeouts"),
        plural(slow, "slow read", "slow reads"),
        plural(reconnects, "reconnect", "reconnects")
    )
}

fn cell_look(st: Option<BlockStatus>, current: bool, color: bool) -> (&'static str, Style) {
    if current {
        return if color {
            (
                "\u{25c9}",
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            )
        } else {
            (">", Style::default())
        };
    }
    let Some(st) = st else {
        return ("\u{b7}", Style::default());
    };
    if !color {
        return (
            match st {
                BlockStatus::Finished => "+",
                BlockStatus::NonTried => "?",
                BlockStatus::NonTrimmed => "*",
                BlockStatus::NonScraped => "/",
                BlockStatus::BadSector => "-",
            },
            Style::default(),
        );
    }
    let fg = match st {
        BlockStatus::Finished => Color::Green,
        BlockStatus::NonTried => Color::DarkGray,
        BlockStatus::NonTrimmed => Color::Yellow,
        BlockStatus::NonScraped => Color::Indexed(208),
        BlockStatus::BadSector => Color::Red,
    };
    ("\u{2588}", Style::new().fg(fg))
}

/// Block map: one cell per slice of the device, coloured by the worst
/// status inside the slice, filled row by row.
struct MapWidget<'a> {
    map: Option<&'a StatusMap>,
    current: Option<u64>,
    color: bool,
}

impl Widget for MapWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (w, h) = (area.width as u64, area.height as u64);
        if w == 0 || h == 0 {
            return;
        }
        let Some(map) = self.map.filter(|m| m.size() > 0) else {
            for y in 0..area.height {
                for x in 0..area.width {
                    if let Some(c) = buf.cell_mut((area.x + x, area.y + y)) {
                        c.set_symbol("\u{b7}");
                    }
                }
            }
            return;
        };
        let size = map.size();
        for row in 0..h {
            let start = (size as u128 * row as u128 / h as u128) as u64;
            let end = (size as u128 * (row as u128 + 1) / h as u128) as u64;
            let text = render_window(map, Range::new(start, end.max(start + 1)), w as usize);
            for (col, ch) in text.chars().enumerate() {
                let (sym, style) = cell_look(BlockStatus::from_char(ch), false, self.color);
                if let Some(c) = buf.cell_mut((area.x + col as u16, area.y + row as u16)) {
                    c.set_symbol(sym);
                    c.set_style(style);
                }
            }
        }
        if let Some(pos) = self.current {
            let cells = w * h;
            let idx = ((pos.min(size - 1) as u128 * cells as u128) / size as u128) as u64;
            let (cx, cy) = (idx % w, idx / w);
            let (sym, style) = cell_look(None, true, self.color);
            if let Some(c) = buf.cell_mut((area.x + cx as u16, area.y + cy as u16)) {
                c.set_symbol(sym);
                c.set_style(style);
            }
        }
    }
}

/// Drive the dashboard until the engine is done.
///
/// Drains `rx`, redraws about four times a second and handles keys. Returns
/// when the engine sent [`Event::Finished`] or hung up.
pub fn run(
    session: &mut Session,
    dash: &mut Dashboard,
    rx: &Receiver<Event>,
    control: &Control,
    interrupt: &Interrupt,
) -> io::Result<()> {
    loop {
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(ev) => {
                    if matches!(ev, Event::Finished(_)) {
                        finished = true;
                    }
                    dash.apply(&ev);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        for line in crate::logging::drain_capture() {
            dash.push_log(line);
        }
        dash.set_paused(control.is_paused());
        session.terminal.draw(|f| dash.render(f))?;
        if finished {
            return Ok(());
        }
        if event::poll(Duration::from_millis(250))? {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'));
                        match k.code {
                            _ if ctrl_c => stop_or_quit(dash, interrupt),
                            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                                stop_or_quit(dash, interrupt)
                            }
                            KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Char(' ') => {
                                control.pause(!control.is_paused());
                                dash.set_paused(control.is_paused());
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn stop_or_quit(dash: &mut Dashboard, interrupt: &Interrupt) {
    match interrupt.request() {
        Step::Stop => dash.set_saving(),
        Step::Force => crate::signal::force_exit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_imaging::{Event as Ev, Stats};
    use ratatui::backend::TestBackend;

    fn progress(rescued: u64, bad: u64, total: u64) -> Progress {
        Progress {
            phase: Phase::Copying {
                pass: 1,
                reverse: false,
            },
            stats: Stats {
                rescued,
                non_tried: total - rescued - bad,
                non_trimmed: 0,
                non_scraped: 0,
                bad_sectors: bad,
                bad_areas: u64::from(bad > 0),
                errsize: bad,
            },
            current_pos: rescued,
            current_rate: 20.0 * 1048576.0,
            average_rate: 18.0 * 1048576.0,
            elapsed: Duration::from_secs(125),
            eta: Some(Duration::from_secs(600)),
            errors: 3,
            slow_reads: 1,
            timeouts: 2,
            reattaches: 1,
            last_error: None,
            size: total,
        }
    }

    fn sample_map(total: u64) -> Arc<StatusMap> {
        let mut m = StatusMap::new(total);
        m.set(Range::new(0, total / 2), BlockStatus::Finished);
        m.set(
            Range::new(total / 4, total / 4 + 512),
            BlockStatus::BadSector,
        );
        m.set(
            Range::new(total / 2, total / 2 + 4096),
            BlockStatus::NonTrimmed,
        );
        Arc::new(m)
    }

    fn dashboard() -> Dashboard {
        let mut d = Dashboard::new(
            Header {
                source: "/dev/sdb".into(),
                detail: "SanDisk Ultra 32 GB, USB flash drive".into(),
                profile: "profile usb-flash, writing to card.img".into(),
            },
            true,
        );
        let total = 1 << 30;
        d.apply(&Ev::Progress(progress(total / 2, 512, total)));
        d.apply(&Ev::Map(sample_map(total)));
        d.apply(&Ev::Log(log::Level::Warn, "read error at 0x1000".into()));
        d.apply(&Ev::DeviceLost { at: 4096 });
        d.apply(&Ev::DeviceBack {
            after: Duration::from_secs(7),
        });
        d
    }

    fn screen(d: &Dashboard, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| d.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn wide_screen_shows_everything_asked_for() {
        let s = screen(&dashboard(), 120, 36);
        for needle in [
            "/dev/sdb",
            "SanDisk Ultra",
            "usb-flash",
            "Position",
            "Copied",
            "50.0%",
            "Unreadable",
            "512 B in 1 area",
            "3 errors",
            "2 timeouts",
            "1 slow",
            "1 reconnect",
            "20.0 MiB/s now",
            "2m 05s elapsed",
            "~10m 00s left in phase",
            "The device stopped responding at 4.00 KiB",
            "for it to come back",
            "The device is back after 7s",
            "warning: read error",
            "q ",
            "pause",
            "copied",
            "unreadable",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
        // Map cells are drawn.
        assert!(s.contains('\u{2588}'));
        assert!(s.contains('\u{25c9}'));
    }

    #[test]
    fn narrow_screen_stacks_panels() {
        let s = screen(&dashboard(), 80, 40);
        assert!(s.contains("Progress") && s.contains("Messages"), "{s}");
    }

    #[test]
    fn tiny_screen_falls_back_to_text() {
        let s = screen(&dashboard(), 40, 10);
        assert!(s.contains("50.0% copied"), "{s}");
        assert!(s.contains("q stop and save"), "{s}");
    }

    #[test]
    fn no_color_uses_mapfile_characters() {
        let mut d = dashboard();
        d.color = false;
        let s = screen(&d, 100, 30);
        assert!(s.contains('+') && s.contains('?') && s.contains('-'), "{s}");
        // Map cells use the mapfile characters; only the gauge keeps its block.
        assert!(!s.contains("\u{2588} copied"), "{s}");
    }

    #[test]
    fn footer_reflects_state() {
        let mut d = dashboard();
        d.set_paused(true);
        assert!(screen(&d, 100, 30).contains("PAUSED"));
        d.set_paused(false);
        d.set_saving();
        assert!(screen(&d, 100, 30).contains("Saving progress"));
    }

    #[test]
    fn works_before_any_event_and_with_empty_map() {
        let d = Dashboard::new(Header::default(), true);
        let s = screen(&d, 100, 30);
        assert!(s.contains("starting"));
        let mut d = Dashboard::new(Header::default(), true);
        d.apply(&Ev::Map(Arc::new(StatusMap::new(0))));
        let _ = screen(&d, 100, 30);
    }

    #[test]
    fn log_keeps_only_the_recent_lines() {
        let mut d = Dashboard::new(Header::default(), true);
        for i in 0..500 {
            d.push_log(format!("line {i}"));
        }
        assert_eq!(d.logs.len(), LOG_KEEP);
        let s = screen(&d, 120, 36);
        assert!(s.contains("line 499") && !s.contains("line 100"), "{s}");
    }
}
