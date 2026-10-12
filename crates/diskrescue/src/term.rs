//! Terminal helpers: colour choice, tty detection, width and plain-text tables.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU8, Ordering};

use clap::ValueEnum;

/// `--color` choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    /// Colour when writing to a terminal (and `NO_COLOR` is not set).
    Auto,
    /// Always use colour.
    Always,
    /// Never use colour.
    Never,
}

static CHOICE: AtomicU8 = AtomicU8::new(0);

/// Remember the `--color` choice for the whole run.
pub fn set_color_choice(choice: ColorChoice) {
    CHOICE.store(
        match choice {
            ColorChoice::Auto => 0,
            ColorChoice::Always => 1,
            ColorChoice::Never => 2,
        },
        Ordering::Relaxed,
    );
}

fn color_enabled(is_tty: bool) -> bool {
    match CHOICE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            is_tty
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
        }
    }
}

/// True when standard output is a terminal.
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// True when standard error is a terminal.
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// True when standard input is a terminal.
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Whether the full-screen interface (which needs colour decisions of its
/// own) should use colours.
pub fn colors_for_tui() -> bool {
    color_enabled(stdout_is_tty())
}

/// Applies ANSI styles when enabled.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    on: bool,
}

/// Painter for text written to standard output.
pub fn out() -> Paint {
    Paint {
        on: color_enabled(stdout_is_tty()),
    }
}

/// Painter for text written to standard error.
pub fn err() -> Paint {
    Paint {
        on: color_enabled(stderr_is_tty()),
    }
}

impl Paint {
    /// A painter that never colours (tests and JSON paths).
    #[cfg(test)]
    pub fn plain() -> Self {
        Paint { on: false }
    }

    /// A painter that always colours (tests).
    #[cfg(test)]
    pub fn forced() -> Self {
        Paint { on: true }
    }

    /// Whether styles are applied.
    pub fn enabled(&self) -> bool {
        self.on
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Bold.
    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }

    /// Dim (secondary information).
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }

    /// Red (errors, damage).
    pub fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }

    /// Bold red.
    pub fn bold_red(&self, s: &str) -> String {
        self.wrap("1;31", s)
    }

    /// Green (good news).
    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }

    /// Yellow (warnings).
    pub fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
}

/// Terminal width in columns, clamped to a sane range. When standard output
/// is not a terminal (redirected to a file, piped into another program) and
/// `COLUMNS` is not set, nothing is shortened: such output usually ends up in
/// a file or a support request where every character matters.
pub fn width() -> usize {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() && std::env::var_os("COLUMNS").is_none() {
        return 400;
    }
    let w = crossterm::terminal::size()
        .ok()
        .map(|(w, _)| w as usize)
        .filter(|w| *w > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|c| c.trim().parse::<usize>().ok())
        })
        .unwrap_or(80);
    w.clamp(20, 400)
}

/// Number of visible characters in `s` (ANSI escape sequences are skipped).
pub fn visible_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        n += 1;
    }
    n
}

/// Shorten the middle of a path-like string so both ends stay readable.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 5 {
        return s.chars().take(max).collect();
    }
    let keep = max - 3;
    let head = keep / 4;
    let tail = keep - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}...{t}")
}

/// Column alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Pad on the right.
    Left,
    /// Pad on the left (numbers).
    Right,
}

/// A simple left-to-right text table with a header row.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    flexible: Vec<bool>,
    rows: Vec<Vec<String>>,
    indent: usize,
}

impl Table {
    /// New table with the given headers (all left aligned).
    pub fn new(headers: &[&str]) -> Self {
        Table {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            aligns: vec![Align::Left; headers.len()],
            flexible: vec![false; headers.len()],
            rows: Vec::new(),
            indent: 0,
        }
    }

    /// Set the alignment of column `col`.
    pub fn align(mut self, col: usize, a: Align) -> Self {
        if let Some(x) = self.aligns.get_mut(col) {
            *x = a;
        }
        self
    }

    /// Allow column `col` to be shortened (in the middle) when the table is
    /// wider than the terminal. Other columns are never truncated.
    pub fn flex(mut self, col: usize) -> Self {
        if let Some(x) = self.flexible.get_mut(col) {
            *x = true;
        }
        self
    }

    /// Indent every line by `n` spaces.
    pub fn indent(mut self, n: usize) -> Self {
        self.indent = n;
        self
    }

    /// Append a row (missing cells are empty, extra cells ignored).
    pub fn row(&mut self, cells: Vec<String>) {
        let mut cells = cells;
        cells.resize(self.headers.len(), String::new());
        self.rows.push(cells);
    }

    /// Render with two spaces between columns. Columns that would make a
    /// line longer than `max_width` are shrunk (the widest first) by
    /// truncating their text in the middle.
    pub fn render(&self, max_width: usize, paint_header: &Paint) -> String {
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| visible_width(h)).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(visible_width(c));
            }
        }
        let gaps = 2 * cols.saturating_sub(1);
        let budget = max_width.saturating_sub(self.indent + gaps);
        // Shorten only the flexible columns, widest first, never below 8.
        while widths.iter().sum::<usize>() > budget {
            let widest = widths
                .iter()
                .enumerate()
                .filter(|(i, w)| self.flexible[*i] && **w > 8)
                .max_by_key(|(_, w)| **w)
                .map(|(i, _)| i);
            match widest {
                Some(i) => widths[i] -= 1,
                None => break,
            }
        }
        let fit = |s: &str, w: usize| -> String {
            // Styled text is never cut: that would split an escape sequence.
            if visible_width(s) <= w || s.contains('\x1b') {
                s.to_string()
            } else {
                truncate_middle(s, w)
            }
        };
        let pad = |s: &str, w: usize, a: Align| -> String {
            let fitted = fit(s, w);
            let v = visible_width(&fitted);
            let fill = " ".repeat(w.saturating_sub(v));
            match a {
                Align::Left => format!("{fitted}{fill}"),
                Align::Right => format!("{fill}{fitted}"),
            }
        };
        let mut out = String::new();
        let ind = " ".repeat(self.indent);
        let head: Vec<String> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| paint_header.bold(&pad(h, widths[i], self.aligns[i])))
            .collect();
        out.push_str(&format!("{ind}{}\n", head.join("  ").trim_end()));
        for r in &self.rows {
            let cells: Vec<String> = r
                .iter()
                .enumerate()
                .map(|(i, c)| pad(c, widths[i], self.aligns[i]))
                .collect();
            let line = cells.join("  ");
            let trimmed = if self.aligns[cols - 1] == Align::Left {
                // Only trim padding, never styled text (it ends in a reset).
                line.trim_end().to_string()
            } else {
                line
            };
            out.push_str(&format!("{ind}{trimmed}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_skips_ansi() {
        assert_eq!(visible_width("abc"), 3);
        assert_eq!(visible_width("\x1b[31mabc\x1b[0m"), 3);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_middle("hello", 10), "hello");
        assert_eq!(
            truncate_middle("abcdefghijklmnopqrstuvwxyz", 12)
                .chars()
                .count(),
            12
        );
        assert!(truncate_middle("abcdefghijklmnopqrstuvwxyz", 12).contains("..."));
    }

    #[test]
    fn table_aligns_columns() {
        let mut t = Table::new(&["NAME", "SIZE"]).align(1, Align::Right);
        t.row(vec!["a".into(), "10".into()]);
        t.row(vec!["longer".into(), "5".into()]);
        let s = t.render(80, &Paint::plain());
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "NAME    SIZE");
        assert_eq!(lines[1], "a         10");
        assert_eq!(lines[2], "longer     5");
    }

    #[test]
    fn table_shrinks_only_flexible_columns() {
        let mut t = Table::new(&["PATH", "N"]).flex(0);
        t.row(vec!["x".repeat(100), "1".into()]);
        let s = t.render(40, &Paint::plain());
        for l in s.lines() {
            assert!(l.chars().count() <= 40, "{l}");
        }
        // A fixed column is never cut, even if the line gets long.
        let mut t = Table::new(&["PATH", "N"]);
        t.row(vec!["x".repeat(100), "1".into()]);
        let s = t.render(40, &Paint::plain());
        assert!(s.contains(&"x".repeat(100)));
    }

    #[test]
    fn paint_only_when_enabled() {
        assert_eq!(Paint::plain().red("x"), "x");
        assert_eq!(Paint::forced().red("x"), "\x1b[31mx\x1b[0m");
        assert_eq!(Paint::forced().red(""), "");
    }
}
