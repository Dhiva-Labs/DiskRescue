//! The real terminal: raw mode, the alternate screen, the event loop, and
//! getting the terminal back whatever happens.

use std::io::{self, IsTerminal, Stdout};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::config::Config;
use crate::event::Event;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static MOUSE: AtomicBool = AtomicBool::new(false);
/// Requests to stop that came from outside the keyboard (SIGINT, SIGTERM,
/// SIGHUP) and have not been delivered to the app yet.
static SIGNALS: AtomicU32 = AtomicU32::new(0);

/// How often the running job is polled.
pub const TICK: Duration = Duration::from_millis(100);

/// Give the terminal back. Safe to call more than once, from a panic hook
/// or from any thread.
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let mut out = io::stdout();
        if MOUSE.load(Ordering::SeqCst) {
            let _ = execute!(out, DisableMouseCapture);
        }
        let _ = execute!(out, LeaveAlternateScreen, Show);
    }
}

/// An open full-screen session. Dropping it restores the terminal.
pub struct Session {
    /// The terminal.
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Session {
    /// Switch to the alternate screen in raw mode.
    pub fn start(mouse: bool) -> io::Result<Session> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(e) = execute!(out, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        ACTIVE.store(true, Ordering::SeqCst);
        if mouse && execute!(out, EnableMouseCapture).is_ok() {
            MOUSE.store(true, Ordering::SeqCst);
        }
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

/// Restore the terminal before a panic message is printed, so the message
/// is readable and the shell is usable afterwards. Panics on other threads
/// (jobs catch their own) do not touch the screen.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore();
            previous(info);
        } else {
            log::error!("a background thread panicked: {info}");
        }
    }));
}

/// Pass SIGINT, SIGTERM and SIGHUP on to the app as [`Event::Interrupt`].
pub fn install_signal_handler() {
    let _ = ctrlc::set_handler(|| {
        SIGNALS.fetch_add(1, Ordering::SeqCst);
    });
}

/// Run `app` on `terminal` until it wants to end.
pub fn run_loop(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> io::Result<()> {
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|f| app.draw(f))?;
        if app.should_quit() {
            return Ok(());
        }
        let wait = TICK.saturating_sub(last_tick.elapsed());
        if event::poll(wait)? {
            loop {
                if let Some(ev) = Event::from_terminal(event::read()?) {
                    app.handle_event(ev);
                }
                if app.should_quit() || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        for _ in 0..SIGNALS.swap(0, Ordering::SeqCst) {
            app.handle_event(Event::Interrupt);
        }
        if last_tick.elapsed() >= TICK {
            app.handle_event(Event::Tick);
            last_tick = Instant::now();
        }
    }
}

const USAGE: &str = if cfg!(windows) {
    "\
DiskRescue terminal app: recover photos and videos from failing drives.

Usage: diskrescue-tui [--no-mouse]

Reading a disk, card or USB drive directly needs administrator rights:
run it in a terminal opened as administrator (right-click Start, Terminal (Admin)).
Image files and repairs of loose files work without them.

Options:
    --no-mouse     do not use the mouse
    -h, --help     show this text
    -V, --version  show the version
"
} else {
    "\
DiskRescue terminal app: recover photos and videos from failing drives.

Usage: diskrescue-tui [--no-mouse]

Reading a disk, card or USB drive directly needs administrator rights:
    sudo diskrescue-tui
Image files and repairs of loose files work without them.

Options:
    --no-mouse     do not use the mouse
    -h, --help     show this text
    -V, --version  show the version
"
};

/// The program: returns the exit code.
pub fn main() -> i32 {
    let mut mouse = true;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "-V" | "--version" => {
                println!("diskrescue-tui {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }
            "--no-mouse" => mouse = false,
            other => {
                eprintln!("Unknown option '{other}'.\n\n{USAGE}");
                return 2;
            }
        }
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "diskrescue-tui is an interactive program and needs a terminal. For scripts, use the \
             command-line tool: diskrescue --help"
        );
        return 2;
    }
    install_panic_hook();
    install_signal_handler();
    // Internal hooks for the tests that run the real program in a
    // pseudo-terminal: no real device is ever looked at, and a panic can be
    // provoked to see that the terminal is given back.
    let hooks = std::env::var("DISKRESCUE_TUI_TEST").unwrap_or_default();
    let hook = |name: &str| hooks.split(',').any(|h| h == name);
    let mut cfg = Config::detect();
    if hook("no-devices") {
        cfg.devices = crate::config::DeviceSource::Fixed(Vec::new());
    }
    let mut app = App::new(cfg);
    let mut session = match Session::start(mouse) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open the full-screen view: {e}");
            return 1;
        }
    };
    if hook("panic") {
        let _ = session.terminal.draw(|f| app.draw(f));
        panic!("selftest: a panic while the terminal is in the alternate screen");
    }
    let result = run_loop(&mut app, &mut session.terminal);
    drop(session);
    if let Err(e) = result {
        eprintln!("The terminal stopped working ({e}). Anything already saved is kept.");
        return 1;
    }
    let code = app.exit_code();
    if code == 130 {
        eprintln!("Quit while a job was running. Whatever it had saved is kept; start the same operation again to continue.");
    }
    code
}
