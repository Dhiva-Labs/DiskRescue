//! The imaging dashboard: block map, statistics and message log.
//!
//! One implementation for both programs. The command line (`diskrescue
//! image`) feeds a [`Dashboard`] with engine events; the terminal app draws
//! the [`ImageState`] of a background job with [`render_image_dashboard`].
//! Both look the same.

use std::time::{Duration, Instant};

use diskrescue_app::jobs::{ImageState, LogLine};
use diskrescue_app::text::{self, bytes, duration, fraction, percent, plural, rate};
use diskrescue_core::Range;
use diskrescue_imaging::{render_window, BlockStatus, Event};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Widget};
use ratatui::Frame;

use crate::widgets::wrap_words;

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

/// Lines the log panel shows.
const LOG_SHOWN: usize = 8;

/// Everything [`render_image_dashboard`] needs.
#[derive(Debug, Clone, Copy)]
pub struct DashboardView<'a> {
    /// Title lines.
    pub header: &'a Header,
    /// The job's state.
    pub state: &'a ImageState,
    /// Use colours.
    pub color: bool,
    /// The key that stops and saves, for the footer (`q` or `s`).
    pub stop_key: &'a str,
    /// The copy can be paused (a copy run by the privileged helper cannot).
    pub can_pause: bool,
    /// Draw the footer with the keys (the terminal app draws its own).
    pub footer: bool,
}

/// An imaging dashboard fed with engine events (what the command line
/// uses).
pub struct Dashboard {
    header: Header,
    state: ImageState,
    color: bool,
}

impl Dashboard {
    /// A dashboard with nothing received yet.
    pub fn new(header: Header, color: bool) -> Self {
        let state = ImageState {
            started: Some(Instant::now()),
            ..ImageState::default()
        };
        Dashboard {
            header,
            state,
            color,
        }
    }

    /// Append a line to the log panel.
    pub fn push_log(&mut self, text: String) {
        self.state.log(log::Level::Info, text);
    }

    /// Take an engine event into account.
    pub fn apply(&mut self, ev: &Event) {
        self.state.apply(ev);
    }

    /// Show the "saving" notice (after the user asked to stop).
    pub fn set_saving(&mut self) {
        self.state.stopping = true;
    }

    /// Mirror the engine's pause state.
    pub fn set_paused(&mut self, paused: bool) {
        self.state.paused = paused;
    }

    /// Draw the whole screen.
    pub fn render(&self, f: &mut Frame) {
        let area = f.area();
        render_image_dashboard(
            f,
            area,
            &DashboardView {
                header: &self.header,
                state: &self.state,
                color: self.color,
                stop_key: "q",
                can_pause: true,
                footer: true,
            },
        );
    }
}

struct Look {
    color: bool,
}

impl Look {
    fn style(&self, s: Style) -> Style {
        if self.color {
            s
        } else {
            Style::default()
        }
    }
}

/// Draw the dashboard into `area`.
pub fn render_image_dashboard(f: &mut Frame, area: Rect, v: &DashboardView<'_>) {
    let look = Look { color: v.color };
    if area.width < 52 || area.height < 15 {
        render_compact(f, area, v);
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
    let banner_h: u16 = u16::from(v.state.device_lost);
    let foot_h: u16 = u16::from(v.footer);
    let [head, banner, map, bottom, foot] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(banner_h),
        Constraint::Min(4),
        Constraint::Length(bottom_h),
        Constraint::Length(foot_h),
    ])
    .areas(area);
    render_header(f, head, v, &look);
    if v.state.device_lost {
        render_banner(f, banner, v, &look);
    }
    render_map(f, map, v, &look);
    if side_by_side {
        let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(bottom);
        render_stats(f, l, v, &look);
        render_log(f, r, v, &look);
    } else if stacked_fits {
        let [s, l] =
            Layout::vertical([Constraint::Length(11), Constraint::Length(10)]).areas(bottom);
        render_stats(f, s, v, &look);
        render_log(f, l, v, &look);
    } else {
        render_stats(f, bottom, v, &look);
    }
    if v.footer {
        render_footer(f, foot, v, &look);
    }
}

fn render_header(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let title = look.style(Style::new().add_modifier(Modifier::BOLD));
    let dim = look.style(Style::new().fg(Color::Gray));
    let lines = vec![
        Line::from(vec![
            Span::styled("DiskRescue ", title),
            Span::raw(format!("copying {}", v.header.source)),
        ]),
        Line::from(Span::styled(v.header.detail.clone(), dim)),
        Line::from(Span::styled(v.header.profile.clone(), dim)),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

/// The "waiting for the device" banner.
fn render_banner(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let at = v
        .state
        .device_lost_at
        .map(|a| format!(" (at {})", bytes(a)))
        .unwrap_or_default();
    let text = format!(
        " The device stopped responding{at}. Waiting for it to come back: check the cable or \
         take the card out and put it in again. The copy continues by itself. "
    );
    let style = look.style(
        Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );
    let style = if look.color {
        style
    } else {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    };
    f.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

fn render_map(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let map = v.state.map.as_deref();
    let title = match map {
        Some(m) if m.size() > 0 => {
            let cells =
                u64::from(area.width.saturating_sub(2)) * u64::from(area.height.saturating_sub(3));
            match m.size().checked_div(cells) {
                Some(per) => format!(" Map: each square is about {} ", bytes(per)),
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
            map,
            current: v.state.progress.as_ref().map(|p| p.current_pos),
            color: look.color,
        },
        map_area,
    );
    f.render_widget(Paragraph::new(legend(look)), legend_area);
}

fn legend(look: &Look) -> Line<'static> {
    let mut spans = Vec::new();
    for (st, name) in [
        (BlockStatus::Finished, "copied"),
        (BlockStatus::NonTried, "not read"),
        (BlockStatus::NonTrimmed, "to examine"),
        (BlockStatus::NonScraped, "to retry"),
        (BlockStatus::BadSector, "unreadable"),
    ] {
        let (sym, style) = cell_look(Some(st), false, look.color);
        spans.push(Span::styled(sym.to_string(), style));
        spans.push(Span::raw(format!(" {name}  ")));
    }
    let (sym, style) = cell_look(None, true, look.color);
    spans.push(Span::styled(sym.to_string(), style));
    spans.push(Span::raw(" now"));
    Line::from(spans)
}

fn render_stats(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let block = Block::default().borders(Borders::ALL).title(" Progress ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let [gauge_area, text_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let (ratio, label) = match v.state.stats() {
        Some(s) => (
            fraction(s.rescued, s.total()),
            format!("{} copied", percent(s.rescued, s.total())),
        ),
        None => (0.0, "starting...".to_string()),
    };
    let gauge_style = look.style(Style::new().fg(Color::Green).bg(Color::DarkGray));
    f.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(label)
            .gauge_style(gauge_style),
        gauge_area,
    );
    f.render_widget(Paragraph::new(stat_lines(v, look)), text_area);
}

fn stat_lines(v: &DashboardView<'_>, look: &Look) -> Vec<Line<'static>> {
    let key = look.style(Style::new().fg(Color::Cyan));
    let bad = look.style(Style::new().fg(Color::Red));
    let kv = |k: &str, val: String| -> Line<'static> {
        Line::from(vec![Span::styled(format!("{k:<11}"), key), Span::raw(val)])
    };
    let Some(p) = &v.state.progress else {
        return vec![kv("Phase", "starting".into())];
    };
    let s = &p.stats;
    let phase = v.state.phase.unwrap_or(p.phase).to_string();
    let mut lines = vec![
        kv("Phase", phase),
        kv(
            "Position",
            format!("{} of {}", bytes(p.current_pos), bytes(p.size)),
        ),
        kv(
            "Copied",
            format!("{} ({})", bytes(s.rescued), percent(s.rescued, s.total())),
        ),
    ];
    let unreadable = if s.bad_sectors > 0 {
        Line::from(vec![
            Span::styled(format!("{:<11}", "Unreadable"), key),
            Span::styled(
                format!(
                    "{} in {}",
                    bytes(s.bad_sectors),
                    plural(s.bad_areas, "area", "areas")
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
            bytes(s.non_tried),
            bytes(s.non_trimmed + s.non_scraped)
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
            rate(p.current_rate),
            rate(p.average_rate)
        ),
    ));
    let eta = match p.eta {
        Some(d) if d > Duration::ZERO => format!("~{} left in phase", duration(d)),
        Some(_) => "phase almost done".to_string(),
        None => "estimating...".to_string(),
    };
    lines.push(kv(
        "Time",
        format!("{} elapsed, {eta}", duration(p.elapsed)),
    ));
    lines
}

/// The text of one log line, with a marker for warnings and errors.
fn log_text(l: &LogLine) -> (String, bool) {
    let tag = match l.level {
        log::Level::Error => "error: ",
        log::Level::Warn => "warning: ",
        _ => "",
    };
    let loud = !tag.is_empty() || l.text.contains("stopped responding");
    (format!("{}{}", tag, l.text), loud)
}

fn render_log(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let block = Block::default().borders(Borders::ALL).title(" Messages ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let width = (inner.width as usize).max(8);
    let warn = look.style(Style::new().fg(Color::Yellow));
    // The latest messages, wrapped to the panel, newest at the bottom.
    let mut visual: Vec<(String, bool)> = Vec::new();
    for l in v.state.logs.iter().rev().take(LOG_SHOWN).rev() {
        let (text_, loud) = log_text(l);
        let stamp = format!("{}  ", text::log_stamp(l.at));
        let indent = " ".repeat(stamp.chars().count());
        for (n, part) in wrap_words(&text_, width.saturating_sub(indent.len()))
            .into_iter()
            .enumerate()
        {
            let lead = if n == 0 {
                stamp.clone()
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

fn render_footer(f: &mut Frame, area: Rect, v: &DashboardView<'_>, look: &Look) {
    let hot = look.style(Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED));
    let hot = if look.color {
        hot
    } else {
        Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    };
    let warn = look.style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD));
    let warn = if look.color {
        warn
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    };
    let st = v.state;
    let stop = format!(" {} ", v.stop_key);
    let line = if st.summary.is_some() {
        Line::from(Span::styled(" Finished. ", warn))
    } else if st.stopping {
        Line::from(Span::styled(
            " Saving progress and stopping... (Ctrl-C twice to quit now) ",
            warn,
        ))
    } else if st.paused {
        Line::from(vec![
            Span::styled(" PAUSED ", warn),
            Span::raw("  press "),
            Span::styled(" p ", hot),
            Span::raw(" to continue, "),
            Span::styled(stop, hot),
            Span::raw(" to stop and save"),
        ])
    } else {
        let saved = match st.last_saved {
            Some(t) => format!("   last saved {} ago", duration(t.elapsed())),
            None => String::new(),
        };
        let mut spans = vec![
            Span::styled(stop, hot),
            Span::raw(" stop and save (you can resume later)   "),
        ];
        if v.can_pause {
            spans.push(Span::styled(" p ", hot));
            spans.push(Span::raw(format!(" pause{saved}")));
        } else if !saved.is_empty() {
            spans.push(Span::raw(saved.trim_start().to_string()));
        }
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Text-only view for terminals that are too small for the dashboard.
fn render_compact(f: &mut Frame, area: Rect, v: &DashboardView<'_>) {
    let mut lines = vec![Line::from(format!("DiskRescue: {}", v.header.source))];
    if let Some(p) = &v.state.progress {
        let s = &p.stats;
        lines.push(Line::from(format!(
            "{} copied ({})",
            percent(s.rescued, s.total()),
            bytes(s.rescued)
        )));
        lines.push(Line::from(format!("unreadable: {}", bytes(s.bad_sectors))));
        lines.push(Line::from(format!("speed: {}", rate(p.current_rate))));
        if let Some(eta) = p.eta {
            lines.push(Line::from(format!("left: {}", duration(eta))));
        }
    } else {
        lines.push(Line::from("starting..."));
    }
    if v.state.device_lost {
        lines.push(Line::from("DEVICE LOST: waiting for it to come back"));
    }
    lines.push(Line::from(if v.state.paused {
        format!("PAUSED - p continues, {} stops", v.stop_key)
    } else if v.state.stopping {
        "saving...".to_string()
    } else {
        format!("{} stop and save, p pause", v.stop_key)
    }));
    f.render_widget(Paragraph::new(lines), area);
}

/// Symbol and style of one cell of the block map. Without colour, the
/// characters of the mapfile format are used instead of coloured blocks.
pub fn cell_look(st: Option<BlockStatus>, current: bool, color: bool) -> (&'static str, Style) {
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
pub struct MapWidget<'a> {
    /// The status map (`None`: nothing known yet).
    pub map: Option<&'a diskrescue_imaging::StatusMap>,
    /// The position being read now.
    pub current: Option<u64>,
    /// Use colours.
    pub color: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_imaging::{Event as Ev, Phase, Progress, Stats, StatusMap};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

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
    fn a_lost_device_shows_a_banner() {
        let mut d = dashboard();
        d.apply(&Ev::DeviceLost { at: 8192 });
        let s = screen(&d, 120, 36);
        assert!(s.contains("Waiting for it to come back"), "{s}");
        assert!(s.contains("check the cable"), "{s}");
        let s = screen(&d, 40, 10);
        assert!(s.contains("DEVICE LOST"), "{s}");
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
        assert_eq!(d.state.logs.len(), diskrescue_app::jobs::RECENT);
        let s = screen(&d, 120, 36);
        assert!(s.contains("line 499") && !s.contains("line 100"), "{s}");
    }
}
