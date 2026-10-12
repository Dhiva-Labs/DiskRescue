//! What can happen to the app: a key, the mouse, a resize, the clock or a
//! request to stop from outside.

use crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};

/// An input to [`crate::App::handle_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A key was pressed.
    Key(KeyEvent),
    /// The mouse did something.
    Mouse(MouseEvent),
    /// The terminal changed size (columns, rows).
    Resize(u16, u16),
    /// About a tenth of a second passed: poll the running job.
    Tick,
    /// Ctrl-C, or a request to end the program (SIGINT, SIGTERM, SIGHUP).
    Interrupt,
}

impl Event {
    /// Convert a terminal event; key releases and other events are dropped.
    pub fn from_terminal(e: crossterm::event::Event) -> Option<Event> {
        use crossterm::event::Event as T;
        match e {
            T::Key(k) if k.kind != KeyEventKind::Release => Some(Event::Key(k)),
            T::Mouse(m) => Some(Event::Mouse(m)),
            T::Resize(w, h) => Some(Event::Resize(w, h)),
            _ => None,
        }
    }
}
