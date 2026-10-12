//! Full-screen dashboard for `diskrescue image`.
//!
//! The imaging engine runs on its own thread and sends events through a
//! channel; this module drains them four times a second, keeps the latest
//! state and redraws. It never blocks the engine. The terminal is restored
//! on every way out, including panics and a forced quit. The widgets
//! themselves (block map, statistics, log) are in `diskrescue_tui`.
//!
//! Keys: `q` or `Esc` stop and save (resume later), `p` pause or resume,
//! `Ctrl-C` behaves like `q` (twice within three seconds quits at once).

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use diskrescue_imaging::{Control, Event};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

pub use diskrescue_app::text::problems_text;
/// The dashboard widgets live in the terminal app's library, so that there
/// is one implementation of them.
pub use diskrescue_tui::dashboard::{Dashboard, Header};

use crate::signal::{Interrupt, Step};

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
