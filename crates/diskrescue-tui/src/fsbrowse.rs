//! Folders and files: listing, path completion and the browser used to
//! choose output folders, images, donors and files to repair.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::Frame;

use diskrescue_app::text::bytes;

use crate::input::TextInput;
use crate::widgets::{ellipsize_end, ellipsize_start, fit, fit_right, render_list, ListNav, Theme};

/// One entry of a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// File name.
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// A folder (links to folders count as folders).
    pub is_dir: bool,
    /// Size in bytes (0 for folders).
    pub size: u64,
}

/// File name extensions of photos, RAW photos and videos.
const MEDIA_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif",
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "dng", "pef",
    "srw", "x3f", "mp4", "m4v", "mov", "3gp", "3g2", "avi", "mkv", "mts", "m2ts", "mpg", "mpeg",
    "wmv", "webm", "mp3", "wav", "m4a", "aac", "flac",
];

/// True when the file name looks like a photo, a video or a sound.
pub fn looks_like_media(name: &str) -> bool {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| MEDIA_EXTENSIONS.contains(&e.as_str()))
}

/// The entries of `dir`, folders first, then files, each sorted by name
/// ignoring case. Hidden entries (starting with a dot) only when
/// `show_hidden`.
pub fn list_dir(dir: &Path, show_hidden: bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = e.path();
        // Follow links so that a link to a folder can be entered.
        let meta = std::fs::metadata(&path).or_else(|_| e.metadata());
        let Ok(meta) = meta else { continue };
        out.push(Entry {
            name,
            path,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

/// `~` and `~/x` to the home folder; anything else unchanged.
pub fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(s)
}

/// The path as typed, made absolute against `cwd` (nothing is looked up on
/// disk).
pub fn absolute_from(s: &str, home: &Path, cwd: &Path) -> PathBuf {
    let p = expand_tilde(s.trim(), home);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

/// The result of asking for a completion of a typed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The text to put in the input (equal to the input when there is
    /// nothing to add).
    pub text: String,
    /// Every name that matches, folders with a trailing separator.
    pub candidates: Vec<String>,
}

/// Complete a path typed by the user: extend it to the longest common
/// prefix of the matching entries of its folder. `folders_only` leaves files
/// out. A single folder match gets a trailing separator.
pub fn complete_path(input: &str, home: &Path, cwd: &Path, folders_only: bool) -> Completion {
    let sep = std::path::MAIN_SEPARATOR;
    let split = input.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    let (dir_part, name_part) = input.split_at(split);
    // What to list: the folder typed so far (or the current folder).
    let dir = if dir_part.is_empty() {
        cwd.to_path_buf()
    } else {
        absolute_from(dir_part, home, cwd)
    };
    let hidden = name_part.starts_with('.');
    let Ok(entries) = list_dir(&dir, hidden) else {
        return Completion {
            text: input.to_string(),
            candidates: Vec::new(),
        };
    };
    let lower = name_part.to_lowercase();
    let usable = |e: &&Entry| e.is_dir || !folders_only;
    // Exact-case matches win: on most systems "photos" and "Photos" differ.
    let mut matches: Vec<&Entry> = entries
        .iter()
        .filter(usable)
        .filter(|e| e.name.starts_with(name_part))
        .collect();
    if matches.is_empty() {
        matches = entries
            .iter()
            .filter(usable)
            .filter(|e| e.name.to_lowercase().starts_with(&lower))
            .collect();
    }
    let candidates: Vec<String> = matches
        .iter()
        .map(|e| {
            if e.is_dir {
                format!("{}{sep}", e.name)
            } else {
                e.name.clone()
            }
        })
        .collect();
    if matches.is_empty() {
        return Completion {
            text: input.to_string(),
            candidates,
        };
    }
    // Longest common prefix (case-sensitive, so the result is a real name).
    let mut prefix: String = matches[0].name.clone();
    for m in &matches[1..] {
        let common: usize = prefix
            .chars()
            .zip(m.name.chars())
            .take_while(|(a, b)| a == b)
            .count();
        prefix = prefix.chars().take(common).collect();
    }
    let mut text = format!("{dir_part}{prefix}");
    if matches.len() == 1 && matches[0].is_dir {
        text.push(sep);
    }
    // Do not shorten what the user typed (case differences aside).
    if text.chars().count() < input.chars().count() {
        text = input.to_string();
    }
    Completion { text, candidates }
}

/// What the browser is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickMode {
    /// Choose a folder.
    Folder,
    /// Choose one file.
    File,
    /// Choose any number of files.
    Files,
}

/// What a key did to the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickEvent {
    /// Still browsing.
    None,
    /// Cancelled.
    Cancel,
    /// Done: the chosen folder, file or files.
    Chosen(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// "Use this folder" / "Use the N chosen files".
    Action,
    /// The parent folder.
    Up,
    /// A folder or file of the listing (index into `entries`).
    Item(usize),
}

/// A small file browser: folders, files, multi-select.
#[derive(Debug, Clone)]
pub struct Picker {
    /// What it picks.
    pub mode: PickMode,
    /// Heading.
    pub title: String,
    /// The folder shown.
    pub dir: PathBuf,
    entries: Vec<Entry>,
    rows: Vec<Row>,
    /// Row selection and scroll.
    pub nav: ListNav,
    /// Files chosen so far (multi-select).
    pub chosen: BTreeSet<PathBuf>,
    /// Hidden files shown.
    pub show_hidden: bool,
    /// Why the folder could not be read.
    pub error: Option<String>,
    /// The "go to" path input, while it is open.
    pub goto: Option<TextInput>,
    home: PathBuf,
    cwd: PathBuf,
}

impl Picker {
    /// A browser starting in `start` (or the nearest existing parent, or
    /// `fallback`).
    pub fn new(
        mode: PickMode,
        title: impl Into<String>,
        start: &Path,
        home: &Path,
        cwd: &Path,
    ) -> Self {
        let mut dir = start.to_path_buf();
        while !dir.is_dir() {
            if !dir.pop() {
                dir = home.to_path_buf();
                break;
            }
        }
        if !dir.is_dir() {
            dir = cwd.to_path_buf();
        }
        let mut p = Picker {
            mode,
            title: title.into(),
            dir,
            entries: Vec::new(),
            rows: Vec::new(),
            nav: ListNav::new(),
            chosen: BTreeSet::new(),
            show_hidden: false,
            error: None,
            goto: None,
            home: home.to_path_buf(),
            cwd: cwd.to_path_buf(),
        };
        p.reload(None);
        p
    }

    /// Preselect files (they are shown ticked when their folder is open).
    pub fn with_chosen(mut self, files: impl IntoIterator<Item = PathBuf>) -> Self {
        self.chosen.extend(files);
        self.reload(None);
        self
    }

    /// The listing of the current folder.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn reload(&mut self, keep: Option<&str>) {
        self.error = None;
        match list_dir(&self.dir, self.show_hidden) {
            Ok(mut e) => {
                if self.mode == PickMode::Folder {
                    e.retain(|x| x.is_dir);
                }
                self.entries = e;
            }
            Err(err) => {
                self.entries.clear();
                self.error = Some(crate::fsbrowse::explain(&err, &self.dir));
            }
        }
        self.rows.clear();
        if self.mode != PickMode::File {
            self.rows.push(Row::Action);
        }
        if self.dir.parent().is_some() {
            self.rows.push(Row::Up);
        }
        for i in 0..self.entries.len() {
            self.rows.push(Row::Item(i));
        }
        let want = keep.and_then(|k| {
            self.rows.iter().position(|r| match r {
                Row::Item(i) => self.entries[*i].name == k,
                _ => false,
            })
        });
        match want {
            Some(i) => self.nav.select(i, self.rows.len()),
            None => {
                // Start on the first useful row: the action row, or the first entry.
                let first = self
                    .rows
                    .iter()
                    .position(|r| matches!(r, Row::Action | Row::Item(_)))
                    .unwrap_or(0);
                self.nav.select(first, self.rows.len());
            }
        }
    }

    /// Open `dir`.
    pub fn go(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.reload(None);
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
            let came_from = self
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            self.dir = parent;
            self.reload(came_from.as_deref());
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        match self.rows.get(self.nav.sel)? {
            Row::Item(i) => self.entries.get(*i),
            _ => None,
        }
    }

    /// The label of the action row.
    pub fn action_label(&self) -> String {
        match self.mode {
            PickMode::Folder => format!("Use this folder: {}", self.dir.display()),
            PickMode::Files if self.chosen.is_empty() => "Done (no files chosen yet)".to_string(),
            PickMode::Files => format!("Done: use the {} chosen file(s)", self.chosen.len()),
            PickMode::File => String::new(),
        }
    }

    fn activate(&mut self) -> PickEvent {
        let Some(row) = self.rows.get(self.nav.sel).cloned() else {
            return PickEvent::None;
        };
        match row {
            Row::Action => match self.mode {
                PickMode::Folder => PickEvent::Chosen(vec![self.dir.clone()]),
                PickMode::Files if !self.chosen.is_empty() => {
                    PickEvent::Chosen(self.chosen.iter().cloned().collect())
                }
                _ => PickEvent::None,
            },
            Row::Up => {
                self.go_up();
                PickEvent::None
            }
            Row::Item(i) => {
                let e = self.entries[i].clone();
                if e.is_dir {
                    self.go(e.path);
                    PickEvent::None
                } else if self.mode == PickMode::File {
                    PickEvent::Chosen(vec![e.path])
                } else {
                    self.toggle(&e.path);
                    PickEvent::None
                }
            }
        }
    }

    fn toggle(&mut self, p: &Path) {
        if !self.chosen.remove(p) {
            self.chosen.insert(p.to_path_buf());
        }
    }

    /// Handle a key.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PickEvent {
        if let Some(g) = self.goto.as_mut() {
            match key.code {
                KeyCode::Esc => self.goto = None,
                KeyCode::Enter => {
                    let typed = g.value().to_string();
                    self.goto = None;
                    let p = absolute_from(&typed, &self.home, &self.cwd);
                    if p.is_dir() {
                        self.go(p);
                    } else if p.exists() && self.mode != PickMode::Folder {
                        // A file, or a device such as /dev/sdb1.
                        return PickEvent::Chosen(vec![p]);
                    } else {
                        self.error = Some(format!("{} is not a folder.", p.display()));
                    }
                }
                KeyCode::Tab => {
                    let c = complete_path(
                        g.value(),
                        &self.home,
                        &self.cwd,
                        self.mode == PickMode::Folder,
                    );
                    g.set(c.text);
                }
                _ => {
                    g.handle_key(key);
                }
            }
            return PickEvent::None;
        }
        let len = self.rows.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickEvent::Cancel,
            KeyCode::Up | KeyCode::Char('k') => self.nav.move_by(-1, len),
            KeyCode::Down | KeyCode::Char('j') => self.nav.move_by(1, len),
            KeyCode::PageUp => self.nav.move_by(-10, len),
            KeyCode::PageDown => self.nav.move_by(10, len),
            KeyCode::Home => self.nav.select(0, len),
            KeyCode::End => self.nav.select(len.saturating_sub(1), len),
            KeyCode::Enter => return self.activate(),
            KeyCode::Right | KeyCode::Char('l') => {
                if self.selected_entry().is_some_and(|e| e.is_dir) {
                    return self.activate();
                }
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char(' ') if self.mode == PickMode::Files => {
                if let Some(e) = self.selected_entry().cloned() {
                    if !e.is_dir {
                        self.toggle(&e.path);
                        self.nav.move_by(1, len);
                    }
                } else if matches!(self.rows.get(self.nav.sel), Some(Row::Action)) {
                    return self.activate();
                }
            }
            KeyCode::Char('a') if self.mode == PickMode::Files => {
                let media: Vec<PathBuf> = self
                    .entries
                    .iter()
                    .filter(|e| !e.is_dir && looks_like_media(&e.name))
                    .map(|e| e.path.clone())
                    .collect();
                // Everything in the folder when it holds no recognisable media.
                let all: Vec<PathBuf> = if media.is_empty() {
                    self.entries
                        .iter()
                        .filter(|e| !e.is_dir)
                        .map(|e| e.path.clone())
                        .collect()
                } else {
                    media
                };
                self.chosen.extend(all);
            }
            KeyCode::Char('x') if self.mode == PickMode::Files => self.chosen.clear(),
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                let keep = self.selected_entry().map(|e| e.name.clone());
                self.reload(keep.as_deref());
            }
            KeyCode::Char('/') | KeyCode::Char('g') | KeyCode::Char('~') => {
                let start = if key.code == KeyCode::Char('~') {
                    "~/".to_string()
                } else {
                    format!("{}{}", self.dir.display(), std::path::MAIN_SEPARATOR)
                };
                self.goto = Some(TextInput::new(start));
            }
            KeyCode::Char('r') if ctrl => self.reload(None),
            _ => {}
        }
        PickEvent::None
    }

    /// Select the row at `index` (a click); a second click on the same row
    /// activates it.
    pub fn click(&mut self, index: usize) -> PickEvent {
        if index >= self.rows.len() {
            return PickEvent::None;
        }
        if self.nav.sel == index {
            return self.activate();
        }
        self.nav.sel = index;
        PickEvent::None
    }

    /// Scroll with the mouse wheel.
    pub fn wheel(&mut self, delta: isize, height: usize) {
        self.nav.scroll(delta, height, self.rows.len());
    }

    /// Draw the browser into `area`. Returns the first visible row and the
    /// area the rows were drawn in (for mouse hit-testing).
    pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) -> (usize, Rect) {
        use ratatui::layout::{Constraint, Layout};
        let goto_h = u16::from(self.goto.is_some());
        let err_h = u16::from(self.error.is_some());
        let [head, list, err, goto] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(err_h),
            Constraint::Length(goto_h),
        ])
        .areas(area);
        let width = area.width as usize;
        let head_lines = vec![
            Line::from(Span::styled(self.title.clone(), theme.bold())),
            Line::from(vec![
                Span::styled("Folder: ", theme.dim()),
                Span::raw(ellipsize_start(
                    &self.dir.display().to_string(),
                    width.saturating_sub(9),
                )),
            ]),
        ];
        f.render_widget(ratatui::widgets::Paragraph::new(head_lines), head);
        let rows: Vec<Line<'static>> = self
            .rows
            .iter()
            .map(|r| self.row_line(r, width, theme))
            .collect();
        let off = render_list(f, list, &rows, &self.nav, theme, true);
        if let Some(e) = &self.error {
            f.render_widget(
                ratatui::widgets::Paragraph::new(Span::styled(e.clone(), theme.bad())),
                err,
            );
        }
        if let Some(g) = &self.goto {
            let mut spans = vec![Span::styled("Go to: ", theme.accent())];
            spans.extend(g.line(width.saturating_sub(8), true, theme).spans);
            f.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), goto);
        }
        (off, list)
    }

    fn row_line(&self, r: &Row, width: usize, theme: &Theme) -> Line<'static> {
        match r {
            Row::Action => {
                let ok = self.mode == PickMode::Folder || !self.chosen.is_empty();
                let label = match self.mode {
                    PickMode::Folder => {
                        let lead = "  >> Use this folder: ";
                        format!(
                            "{lead}{}",
                            ellipsize_start(
                                &self.dir.display().to_string(),
                                width.saturating_sub(lead.chars().count())
                            )
                        )
                    }
                    _ => format!("  >> {}", self.action_label()),
                };
                Line::from(Span::styled(
                    ellipsize_end(&label, width),
                    if ok { theme.good() } else { theme.dim() },
                ))
            }
            Row::Up => Line::from(Span::styled("  ../  (up one folder)", theme.dim())),
            Row::Item(i) => {
                let e = &self.entries[*i];
                if e.is_dir {
                    Line::from(vec![Span::styled(
                        format!("  {}{}", e.name, std::path::MAIN_SEPARATOR),
                        theme.accent(),
                    )])
                } else {
                    let mark = if self.mode == PickMode::Files {
                        if self.chosen.contains(&e.path) {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    } else {
                        ""
                    };
                    let size = fit_right(&bytes(e.size), 10);
                    let name_w = width.saturating_sub(2 + mark.len() + 11);
                    let name = fit(&crate::widgets::ellipsize_end(&e.name, name_w), name_w);
                    let style = if looks_like_media(&e.name) || self.mode == PickMode::File {
                        ratatui::style::Style::default()
                    } else {
                        theme.dim()
                    };
                    Line::from(Span::styled(format!("  {mark}{name} {size}"), style))
                }
            }
        }
    }

    /// Number of rows (for hit-testing).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}

/// A folder-reading error in plain words.
pub fn explain(e: &io::Error, dir: &Path) -> String {
    diskrescue_app::error::explain_io(e, Some(dir))
}

/// Edit a path input: Tab completes; returns the completion candidates to
/// show below the input.
pub fn tab_complete(
    input: &mut TextInput,
    home: &Path,
    cwd: &Path,
    folders_only: bool,
) -> Vec<String> {
    let c = complete_path(input.value(), home, cwd, folders_only);
    let same = c.text == input.value();
    input.set(c.text);
    if c.candidates.len() > 1 || same {
        c.candidates
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for dir in ["Photos", "photos-old", "Docs", ".hidden"] {
            std::fs::create_dir(d.path().join(dir)).unwrap();
        }
        for f in ["a.jpg", "b.MP4", "notes.txt", ".dot.jpg"] {
            std::fs::write(d.path().join(f), b"x").unwrap();
        }
        d
    }

    #[test]
    fn listing_puts_folders_first_and_hides_dot_files() {
        let d = tree();
        let l = list_dir(d.path(), false).unwrap();
        let names: Vec<&str> = l.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Docs",
                "Photos",
                "photos-old",
                "a.jpg",
                "b.MP4",
                "notes.txt"
            ]
        );
        assert_eq!(list_dir(d.path(), true).unwrap().len(), 8);
        assert!(looks_like_media("x.CR2") && looks_like_media("clip.mp4"));
        assert!(!looks_like_media("notes.txt") && !looks_like_media("noext"));
    }

    #[test]
    fn tab_completion_extends_to_the_common_prefix() {
        let d = tree();
        let home = d.path();
        let cwd = Path::new("/");
        let base = format!("{}/", d.path().display());
        let c = complete_path(&format!("{base}Ph"), home, cwd, true);
        assert_eq!(c.text, format!("{base}Photos/"));
        assert_eq!(c.candidates, vec!["Photos/".to_string()]);
        // Exact case wins over a different case.
        let c = complete_path(&format!("{base}ph"), home, cwd, true);
        assert_eq!(c.text, format!("{base}photos-old/"));
        // Another case is used when nothing matches exactly.
        let c = complete_path(&format!("{base}PHOTOS-"), home, cwd, true);
        assert_eq!(c.candidates, vec!["photos-old/".to_string()]);
        // Several matches: extend to what they share.
        std::fs::create_dir(d.path().join("photos-new")).unwrap();
        let c = complete_path(&format!("{base}pho"), home, cwd, true);
        assert_eq!(c.candidates.len(), 2);
        assert_eq!(c.text, format!("{base}photos-"));
        // Files only count when files are wanted.
        let c = complete_path(&format!("{base}no"), home, cwd, true);
        assert!(c.candidates.is_empty());
        let c = complete_path(&format!("{base}no"), home, cwd, false);
        assert_eq!(c.text, format!("{base}notes.txt"));
        // Home folder.
        let c = complete_path("~/Do", home, cwd, true);
        assert_eq!(c.text, "~/Docs/");
        assert_eq!(
            absolute_from("~/x", home, cwd),
            home.join("x"),
            "tilde expands"
        );
        assert_eq!(
            absolute_from("rel", home, Path::new("/w")),
            Path::new("/w/rel")
        );
    }

    #[test]
    fn the_browser_chooses_a_folder() {
        let d = tree();
        let mut p = Picker::new(PickMode::Folder, "Choose", d.path(), d.path(), d.path());
        assert_eq!(p.entries().len(), 3, "folders only, no hidden ones");
        // First row is "use this folder".
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickEvent::Chosen(vec![d.path().to_path_buf()])
        );
        // Go into Docs (rows: action, up, Docs, ...).
        p.handle_key(&key(KeyCode::Down));
        p.handle_key(&key(KeyCode::Down));
        p.handle_key(&key(KeyCode::Enter));
        assert_eq!(p.dir, d.path().join("Docs"));
        // Left goes back and remembers Docs.
        p.handle_key(&key(KeyCode::Left));
        assert_eq!(p.dir, d.path());
        assert_eq!(p.entries()[p.nav.sel.saturating_sub(2)].name, "Docs");
        assert_eq!(p.handle_key(&key(KeyCode::Esc)), PickEvent::Cancel);
    }

    #[test]
    fn the_browser_chooses_files() {
        let d = tree();
        let mut p = Picker::new(PickMode::Files, "Pick", d.path(), d.path(), d.path());
        // Nothing chosen: the action row does nothing.
        assert_eq!(p.handle_key(&key(KeyCode::Enter)), PickEvent::None);
        p.handle_key(&key(KeyCode::Char('a')));
        assert_eq!(p.chosen.len(), 2, "only photos and videos");
        p.handle_key(&key(KeyCode::Char('x')));
        assert!(p.chosen.is_empty());
        // Move to a.jpg and tick it with space.
        let idx = p.rows.len() - 3; // a.jpg
        p.nav.select(idx, p.rows.len());
        p.handle_key(&key(KeyCode::Char(' ')));
        assert!(p.chosen.contains(&d.path().join("a.jpg")));
        p.nav.select(0, p.rows.len());
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickEvent::Chosen(vec![d.path().join("a.jpg")])
        );
    }

    #[test]
    fn a_single_file_is_chosen_with_enter_and_goto_works() {
        let d = tree();
        let mut p = Picker::new(PickMode::File, "Pick", d.path(), d.path(), d.path());
        assert!(!p.rows.contains(&Row::Action));
        let idx = p
            .rows
            .iter()
            .position(|r| matches!(r, Row::Item(i) if p.entries[*i].name == "notes.txt"))
            .unwrap();
        p.nav.select(idx, p.rows.len());
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickEvent::Chosen(vec![d.path().join("notes.txt")])
        );
        // "Go to" a folder by typing its path.
        p.handle_key(&key(KeyCode::Char('/')));
        assert!(p.goto.is_some());
        let docs = d.path().join("Docs").display().to_string();
        p.goto.as_mut().unwrap().set(docs);
        p.handle_key(&key(KeyCode::Enter));
        assert_eq!(p.dir, d.path().join("Docs"));
        // A path that is no folder is explained.
        p.handle_key(&key(KeyCode::Char('/')));
        p.goto.as_mut().unwrap().set("/definitely/not/here");
        p.handle_key(&key(KeyCode::Enter));
        assert!(p.error.as_deref().unwrap().contains("not a folder"));
    }

    #[test]
    fn a_missing_start_folder_falls_back_to_an_existing_parent() {
        let d = tree();
        let p = Picker::new(
            PickMode::Folder,
            "x",
            &d.path().join("no/such/place"),
            d.path(),
            d.path(),
        );
        assert_eq!(p.dir, d.path());
    }
}
