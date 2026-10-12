//! Small drawing helpers shared by every screen: word wrapping, centred
//! boxes, scrolling lists and the dim/bold styles of the two colour modes.

use std::cell::Cell;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use diskrescue_app::text::Tone;

/// How things look. With colours off (`NO_COLOR`, a dumb terminal) only
/// bold, dim and reverse video are used, and every state also has a word
/// or a symbol, so nothing depends on colour alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Use colours.
    pub color: bool,
}

impl Theme {
    /// A theme with colours on or off.
    pub fn new(color: bool) -> Self {
        Theme { color }
    }

    /// Colours unless `NO_COLOR` is set (to anything non-empty) or the
    /// terminal is `dumb`.
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
        Theme::new(!(no_color || dumb))
    }

    fn paint(&self, fg: Color, extra: Modifier) -> Style {
        if self.color {
            Style::new().fg(fg).add_modifier(extra)
        } else {
            Style::new().add_modifier(extra)
        }
    }

    /// Bold text.
    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    /// Secondary information.
    pub fn dim(&self) -> Style {
        if self.color {
            Style::new().fg(Color::Gray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }

    /// Labels and keys that stand out a little.
    pub fn accent(&self) -> Style {
        self.paint(Color::Cyan, Modifier::empty())
    }

    /// A key name in the footer.
    pub fn key(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    }

    /// The selected row.
    pub fn selected(&self) -> Style {
        if self.color {
            Style::new()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// The selected row of a list that does not have the focus.
    pub fn selected_inactive(&self) -> Style {
        if self.color {
            Style::new().fg(Color::White).bg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::UNDERLINED)
        }
    }

    /// Good news.
    pub fn good(&self) -> Style {
        self.paint(Color::Green, Modifier::empty())
    }

    /// Needs attention.
    pub fn warn(&self) -> Style {
        self.paint(Color::Yellow, Modifier::BOLD)
    }

    /// Wrong or damaged.
    pub fn bad(&self) -> Style {
        self.paint(Color::Red, Modifier::BOLD)
    }

    /// The style for a tone of [`diskrescue_app::text`].
    pub fn tone(&self, t: Tone) -> Style {
        match t {
            Tone::Good => self.good(),
            Tone::Warn => self.warn(),
            Tone::Bad => self.bad(),
            Tone::Neutral => self.dim(),
        }
    }

    /// A word-free marker for a tone, so colour is never the only cue.
    pub fn tone_mark(&self, t: Tone) -> &'static str {
        match t {
            Tone::Good => "+",
            Tone::Warn => "!",
            Tone::Bad => "x",
            Tone::Neutral => "-",
        }
    }

    /// A bordered block with a title.
    pub fn block<'a>(&self, title: impl Into<Line<'a>>) -> Block<'a> {
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(self.dim())
    }

    /// The same, with a stronger border for the focused panel.
    pub fn block_focus<'a>(&self, title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
        let b = Block::default().borders(Borders::ALL).title(title);
        if focused {
            b.border_style(self.accent().add_modifier(Modifier::BOLD))
        } else {
            b.border_style(self.dim())
        }
    }
}

/// Wrap `text` to lines of at most `width` characters. Words longer than a
/// line are split, after a `/` when there is one (so paths break between
/// folders). Always returns at least one line.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        while word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            // Prefer a break after a folder separator.
            let cut = chars[..width]
                .iter()
                .rposition(|c| *c == '/' || *c == '\\')
                .map(|i| i + 1)
                .filter(|i| *i > width / 3)
                .unwrap_or(width);
            let head: String = chars[..cut].iter().collect();
            word = chars[cut..].iter().collect();
            lines.push(head);
        }
        let need = if cur.is_empty() { 0 } else { 1 } + word.chars().count();
        if cur.chars().count() + need > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Wrap a paragraph that may contain newlines (each one starts a new line).
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for l in text.split('\n') {
        out.extend(wrap_words(l, width));
    }
    out
}

/// Shorten `s` to `max` characters, with `...` in the middle-left
/// (`...ards/DCIM/IMG_0001.JPG`), so the end of a path stays visible.
pub fn ellipsize_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 3 {
        return s.chars().skip(n - max).collect();
    }
    let tail: String = s.chars().skip(n - (max - 3)).collect();
    format!("...{tail}")
}

/// Shorten `s` to `max` characters, with `...` at the end.
pub fn ellipsize_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 3 {
        return s.chars().take(max).collect();
    }
    let head: String = s.chars().take(max - 3).collect();
    format!("{head}...")
}

/// Pad or cut `s` to exactly `w` characters.
pub fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.chars().take(w).collect()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

/// Pad on the left or cut `s` to exactly `w` characters.
pub fn fit_right(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.chars().skip(n - w).collect()
    } else {
        format!("{}{s}", " ".repeat(w - n))
    }
}

/// A rectangle of the given size centred in `area` (clamped to it).
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let [row] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    let [col] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(row);
    col
}

/// Clear a box in the middle of `area` and draw `lines` in it. Returns the
/// inner area.
pub fn modal(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    title: &str,
    width: u16,
    lines: Vec<Line<'static>>,
) -> Rect {
    let height = (lines.len() as u16).saturating_add(2);
    let rect = centered(area, width, height);
    f.render_widget(Clear, rect);
    let block = theme
        .block_focus(format!(" {title} "), true)
        .style(Style::default());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(Paragraph::new(lines), inner);
    inner
}

/// Selection and scroll position of a list. The offset lives in a `Cell`
/// so that drawing (which only borrows the app) can keep the selection in
/// view.
#[derive(Debug, Clone, Default)]
pub struct ListNav {
    /// Selected row.
    pub sel: usize,
    offset: Cell<usize>,
}

impl ListNav {
    /// A list with the first row selected.
    pub fn new() -> Self {
        ListNav::default()
    }

    /// Move the selection by `delta` rows, staying inside `0..len`.
    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.sel = 0;
            return;
        }
        let max = len - 1;
        let n = self.sel as isize + delta;
        self.sel = n.clamp(0, max as isize) as usize;
    }

    /// Select `i`, staying inside `0..len`.
    pub fn select(&mut self, i: usize, len: usize) {
        self.sel = if len == 0 { 0 } else { i.min(len - 1) };
    }

    /// Keep the selection inside `0..len` after the list changed.
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.sel = 0;
            self.offset.set(0);
        } else if self.sel >= len {
            self.sel = len - 1;
        }
    }

    /// First visible row for a window of `height` rows over `len` rows.
    pub fn window(&self, height: usize, len: usize) -> usize {
        if height == 0 || len == 0 {
            return 0;
        }
        let mut off = self.offset.get().min(len.saturating_sub(1));
        if self.sel < off {
            off = self.sel;
        } else if self.sel >= off + height {
            off = self.sel + 1 - height;
        }
        if off + height > len {
            off = len.saturating_sub(height);
        }
        self.offset.set(off);
        off
    }

    /// Scroll the window without moving the selection further than needed
    /// (mouse wheel).
    pub fn scroll(&mut self, delta: isize, height: usize, len: usize) {
        if len == 0 {
            return;
        }
        let max_off = len.saturating_sub(height.max(1)) as isize;
        let off = (self.offset.get() as isize + delta).clamp(0, max_off) as usize;
        self.offset.set(off);
        if self.sel < off {
            self.sel = off;
        } else if height > 0 && self.sel >= off + height {
            self.sel = off + height - 1;
        }
    }

    /// The current first visible row (without adjusting).
    pub fn offset(&self) -> usize {
        self.offset.get()
    }
}

/// Draw `rows` into `area` as a scrolling list with row `nav.sel`
/// highlighted. Returns the index of the first visible row.
pub fn render_list(
    f: &mut Frame,
    area: Rect,
    rows: &[Line<'static>],
    nav: &ListNav,
    theme: &Theme,
    focused: bool,
) -> usize {
    let height = area.height as usize;
    let off = nav.window(height, rows.len());
    let sel_style = if focused {
        theme.selected()
    } else {
        theme.selected_inactive()
    };
    let visible: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(off)
        .take(height)
        .map(|(i, l)| {
            if i == nav.sel {
                // Fill the whole row so the highlight is a bar.
                let mut spans = l.spans.clone();
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                if used < area.width as usize {
                    spans.push(Span::raw(" ".repeat(area.width as usize - used)));
                }
                for s in &mut spans {
                    s.style = sel_style;
                }
                Line::from(spans)
            } else {
                l.clone()
            }
        })
        .collect();
    f.render_widget(Paragraph::new(visible), area);
    off
}

/// Like [`render_list`], for rows of two lines each. Returns the index of
/// the first visible row.
pub fn render_items(
    f: &mut Frame,
    area: Rect,
    rows: &[[Line<'static>; 2]],
    nav: &ListNav,
    theme: &Theme,
    focused: bool,
) -> usize {
    let per_page = (area.height / 2) as usize;
    let off = nav.window(per_page, rows.len());
    let sel_style = if focused {
        theme.selected()
    } else {
        theme.selected_inactive()
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, pair) in rows.iter().enumerate().skip(off).take(per_page) {
        for l in pair {
            if i == nav.sel {
                let mut spans = l.spans.clone();
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                if used < area.width as usize {
                    spans.push(Span::raw(" ".repeat(area.width as usize - used)));
                }
                for s in &mut spans {
                    s.style = sel_style;
                }
                lines.push(Line::from(spans));
            } else {
                lines.push(l.clone());
            }
        }
    }
    f.render_widget(Paragraph::new(lines), area);
    off
}

/// A line of `key  description` pairs for the footer.
pub fn key_hints(theme: &Theme, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (k, d) in pairs {
        spans.push(Span::styled(format!(" {k} "), theme.key()));
        spans.push(Span::raw(format!(" {d}   ")));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_and_shortening() {
        assert_eq!(wrap_words("hello world", 20), vec!["hello world"]);
        assert_eq!(wrap_words("hello world", 8), vec!["hello", "world"]);
        assert_eq!(wrap_words("a b c d e", 3), vec!["a b", "c d", "e"]);
        assert_eq!(wrap_words("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(
            wrap_words("/home/me/photos/card.img", 14),
            vec!["/home/me/", "photos/", "card.img"]
        );
        assert_eq!(wrap_words("", 5), vec![""]);
        for l in wrap_words(
            "The device stopped responding at 4.00 KiB. Waiting for it to come back.",
            20,
        ) {
            assert!(l.chars().count() <= 20, "{l}");
        }
        assert_eq!(wrap_text("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(
            ellipsize_start("/home/x/DCIM/IMG_1.JPG", 12),
            "...IMG_1.JPG"
        );
        assert_eq!(ellipsize_end("abcdefghij", 6), "abc...");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit_right("ab", 4), "  ab");
    }

    #[test]
    fn list_navigation_keeps_the_selection_in_view() {
        let mut nav = ListNav::new();
        nav.move_by(7, 10);
        assert_eq!(nav.sel, 7);
        assert_eq!(nav.window(4, 10), 4);
        nav.move_by(100, 10);
        assert_eq!(nav.sel, 9);
        assert_eq!(nav.window(4, 10), 6);
        nav.move_by(-100, 10);
        assert_eq!(nav.window(4, 10), 0);
        nav.scroll(3, 4, 10);
        assert_eq!(nav.sel, 3);
        nav.clamp(2);
        assert_eq!(nav.sel, 1);
    }

    #[test]
    fn monochrome_theme_has_no_colours() {
        let t = Theme::new(false);
        for s in [
            t.dim(),
            t.accent(),
            t.good(),
            t.warn(),
            t.bad(),
            t.selected(),
        ] {
            assert_eq!(s.fg, None);
            assert_eq!(s.bg, None);
        }
        assert!(t.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(Theme::new(true).bad().fg.is_some());
    }
}
