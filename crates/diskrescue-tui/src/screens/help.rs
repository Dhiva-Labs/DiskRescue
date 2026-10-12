//! The help overlay: the keys of the current screen and the keys that work
//! everywhere.

use ratatui::text::{Line, Span};

use crate::widgets::Theme;

/// The screens that have their own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenId {
    /// The list of devices.
    Home,
    /// What to do with a device.
    Menu,
    /// A form.
    Form,
    /// The file browser.
    Pick,
    /// The check before starting.
    Preflight,
    /// A running job.
    Run,
    /// The results browser.
    Results,
    /// The reference file chooser.
    Donor,
    /// The preview.
    Scan,
    /// The verdicts.
    Verify,
}

impl ScreenId {
    /// Name for the title of the help.
    pub fn name(self) -> &'static str {
        match self {
            ScreenId::Home => "the home screen",
            ScreenId::Menu => "the action menu",
            ScreenId::Form => "a form",
            ScreenId::Pick => "the file browser",
            ScreenId::Preflight => "the safety check",
            ScreenId::Run => "a running job",
            ScreenId::Results => "the results",
            ScreenId::Donor => "the reference file chooser",
            ScreenId::Scan => "the preview",
            ScreenId::Verify => "the check results",
        }
    }
}

/// Keys of one screen as `(keys, what it does)`.
pub fn keys_for(id: ScreenId) -> Vec<(&'static str, &'static str)> {
    match id {
        ScreenId::Home => vec![
            ("Up Down  j k", "move through the list"),
            ("Enter", "open the selection"),
            ("r", "look for devices again"),
            ("i", "open an image file"),
            ("f", "repair or check files"),
            ("p", "open previous results"),
            ("g", "go back to the running job"),
            ("J K", "scroll the details on the right"),
            ("q", "quit"),
        ],
        ScreenId::Menu => vec![
            ("Up Down", "move"),
            ("Enter  1-4", "choose"),
            ("Esc", "back"),
        ],
        ScreenId::Form => vec![
            ("Up Down", "move between the settings"),
            ("Enter", "next setting (on Start: check and start)"),
            ("Left Right  Space", "change a choice, tick a box"),
            ("Tab", "complete a path you are typing"),
            ("F2  Ctrl-O", "browse for a folder or file"),
            ("Ctrl-S", "check and start from anywhere"),
            ("Esc", "back"),
        ],
        ScreenId::Pick => vec![
            ("Up Down", "move"),
            ("Enter  Right", "open a folder, choose a file"),
            ("Left  Backspace", "up one folder"),
            ("Space", "tick a file (when choosing several)"),
            ("a  x", "tick all photos and videos  /  clear"),
            ("/  ~", "type a path (Tab completes it)"),
            (".", "show or hide hidden files"),
            ("Esc", "cancel"),
        ],
        ScreenId::Preflight => vec![
            ("Enter", "start (when everything is fine)"),
            (
                "yes + Enter",
                "continue past a problem you decided to accept",
            ),
            ("Up Down  PgUp PgDn", "scroll"),
            ("Esc", "go back and change something"),
        ],
        ScreenId::Run => vec![
            ("p", "pause or continue a copy"),
            ("s", "stop and save (a copy can be continued later)"),
            ("b", "leave this screen; the job keeps running"),
            ("Enter", "when done: look at the results"),
            ("o", "when done: open the folder"),
            ("h", "when done: home screen"),
            ("Ctrl-C", "stop and save; twice: quit now"),
        ],
        ScreenId::Results => vec![
            ("Up Down  PgUp PgDn", "move through the files"),
            ("Left Right  Tab  1-6", "choose the filter"),
            ("s  S", "change the order  /  reverse it"),
            ("r", "repair a damaged file with a reference file"),
            ("o  Enter", "open the folder of the file"),
            ("O", "open the output folder"),
            ("J K", "scroll the details"),
            ("h", "home screen"),
            ("Esc", "back"),
        ],
        ScreenId::Donor => vec![
            ("Up Down", "move"),
            ("Enter", "repair using the selected file as reference"),
            ("b", "browse for another file"),
            ("Esc", "back"),
        ],
        ScreenId::Scan => vec![
            ("Up Down  PgUp PgDn", "move"),
            ("Tab  1 2", "switch between the two lists"),
            ("r  Enter", "recover these files"),
            ("h", "home screen"),
            ("Esc", "back"),
        ],
        ScreenId::Verify => vec![
            ("Up Down  PgUp PgDn", "move"),
            ("r  Enter", "repair the files that need attention"),
            ("h", "home screen"),
            ("Esc", "back"),
        ],
    }
}

/// The lines of the help box.
pub fn help_lines(id: ScreenId, theme: &Theme) -> Vec<Line<'static>> {
    let mut v: Vec<Line<'static>> = Vec::new();
    v.push(Line::from(Span::styled(
        format!("Keys for {}", id.name()),
        theme.bold(),
    )));
    v.push(Line::raw(""));
    for (k, d) in keys_for(id) {
        v.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), theme.accent()),
            Span::raw(d.to_string()),
        ]));
    }
    v.push(Line::raw(""));
    v.push(Line::from(Span::styled("Everywhere", theme.bold())));
    for (k, d) in [
        ("?  F1", "this help"),
        ("Ctrl-C", "stop what is running and save; again: quit"),
        ("q", "quit (asks first when something is running)"),
        (
            "mouse",
            "click a row to select it, again to open it; wheel scrolls",
        ),
    ] {
        v.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), theme.accent()),
            Span::raw(d.to_string()),
        ]));
    }
    v.push(Line::raw(""));
    v.push(Line::from(Span::styled(
        "Nothing is ever written to the device you recover from.",
        theme.dim(),
    )));
    v.push(Line::from(Span::styled(
        "Press any key to close this help.",
        theme.dim(),
    )));
    v
}

/// Footer hints of a screen as `(key, words)`.
pub fn footer_for(id: ScreenId) -> Vec<(&'static str, &'static str)> {
    match id {
        ScreenId::Home => vec![
            ("Enter", "open"),
            ("r", "refresh"),
            ("i", "image file"),
            ("?", "help"),
            ("q", "quit"),
        ],
        ScreenId::Menu => vec![("Enter", "choose"), ("Esc", "back"), ("?", "help")],
        ScreenId::Form => vec![
            ("Enter", "next / start"),
            ("Tab", "complete"),
            ("F2", "browse"),
            ("Esc", "back"),
            ("?", "help"),
        ],
        ScreenId::Pick => vec![
            ("Enter", "open / choose"),
            ("/", "type path"),
            ("Esc", "cancel"),
        ],
        ScreenId::Preflight => vec![("Enter", "start"), ("Esc", "go back")],
        ScreenId::Run => vec![
            ("p", "pause"),
            ("s", "stop and save"),
            ("b", "leave it running"),
            ("?", "help"),
        ],
        ScreenId::Results => vec![
            ("Tab", "filter"),
            ("s", "sort"),
            ("r", "repair"),
            ("o", "open folder"),
            ("Esc", "back"),
            ("?", "help"),
        ],
        ScreenId::Donor => vec![
            ("Enter", "use as reference"),
            ("b", "browse"),
            ("Esc", "back"),
        ],
        ScreenId::Scan => vec![
            ("r", "recover these files"),
            ("Tab", "other list"),
            ("Esc", "back"),
        ],
        ScreenId::Verify => vec![("r", "repair"), ("Esc", "back")],
    }
}
