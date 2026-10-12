//! The real program in a pseudo-terminal: it takes the screen, draws, and
//! gives the terminal back on every way out.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ENTER_ALT: &str = "\x1b[?1049h";
const LEAVE_ALT: &str = "\x1b[?1049l";
const SHOW_CURSOR: &str = "\x1b[?25h";

struct Pty {
    master: std::fs::File,
    slave: OwnedFd,
}

fn open_pty(cols: u16, rows: u16) -> Pty {
    let (mut m, mut s) = (0, 0);
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty fills the two descriptors on success; `ws` outlives
    // the call; the null pointers mean "no name, no termios".
    let rc = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), &ws) };
    assert_eq!(rc, 0, "openpty failed");
    // SAFETY: both descriptors are fresh and owned by nobody else.
    unsafe {
        Pty {
            master: std::fs::File::from_raw_fd(m),
            slave: OwnedFd::from_raw_fd(s),
        }
    }
}

struct Session {
    child: std::process::Child,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    master: std::fs::File,
    _home: tempfile::TempDir,
}

fn start(hooks: &str, cols: u16, rows: u16) -> Session {
    let pty = open_pty(cols, rows);
    let home = tempfile::tempdir().unwrap();
    let dup = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    let child = Command::new(env!("CARGO_BIN_EXE_diskrescue-tui"))
        .env("TERM", "xterm-256color")
        .env("HOME", home.path())
        .env_remove("NO_COLOR")
        .env("DISKRESCUE_TUI_TEST", hooks)
        .stdin(dup(&pty.slave))
        .stdout(dup(&pty.slave))
        .stderr(dup(&pty.slave))
        .spawn()
        .expect("cannot run diskrescue-tui");
    drop(pty.slave);
    let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = output.clone();
    let mut reader = pty.master.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    Session {
        child,
        output,
        master: pty.master,
        _home: home,
    }
}

impl Session {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, needle: &str) {
        let t = Instant::now();
        while !self.text().contains(needle) {
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "never saw {needle:?} in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn finish(mut self) -> (Option<i32>, String) {
        let t = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                // Let the reader drain what was written last.
                std::thread::sleep(Duration::from_millis(100));
                return (
                    std::os::unix::process::ExitStatusExt::signal(&st)
                        .map(|s| -s)
                        .or(st.code()),
                    self.text(),
                );
            }
            if t.elapsed() > Duration::from_secs(20) {
                let _ = self.child.kill();
                panic!("the program did not end; output: {:?}", self.text());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn restored_after(out: &str, marker: &str) {
    let enter = out
        .find(ENTER_ALT)
        .expect("the alternate screen was entered");
    let leave = out.rfind(LEAVE_ALT).expect("the alternate screen was left");
    assert!(enter < leave, "{out:?}");
    assert!(
        out[leave..].contains(SHOW_CURSOR),
        "the cursor was not shown again: {out:?}"
    );
    if !marker.is_empty() {
        let at = out
            .find(marker)
            .unwrap_or_else(|| panic!("no {marker:?} in {out:?}"));
        assert!(
            at > leave,
            "{marker:?} was printed before the terminal was restored"
        );
    }
}

#[test]
fn it_draws_the_home_screen_and_q_gives_the_terminal_back() {
    let mut s = start("no-devices", 100, 30);
    s.wait_for("Open an image");
    let out = s.text();
    assert!(out.contains(ENTER_ALT));
    assert!(out.contains("DiskRescue"));
    assert!(out.contains("Disks,"));
    // Mouse reporting is on while the screen is ours.
    assert!(out.contains("\x1b[?1000h"), "{out:?}");
    s.send(b"q");
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    restored_after(&out, "");
    assert!(out.contains("\x1b[?1000l"), "mouse reporting left on");
}

#[test]
fn a_termination_signal_gives_the_terminal_back() {
    let s = start("no-devices", 100, 30);
    s.wait_for("Open an image file");
    // SAFETY: plain kill(2) on the child we started.
    unsafe {
        libc::kill(s.child.id() as i32, libc::SIGTERM);
    }
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    restored_after(&out, "");
}

#[test]
fn a_hangup_gives_the_terminal_back() {
    let s = start("no-devices", 100, 30);
    s.wait_for("Open an image file");
    // SAFETY: plain kill(2) on the child we started.
    unsafe {
        libc::kill(s.child.id() as i32, libc::SIGHUP);
    }
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    restored_after(&out, "");
}

#[test]
fn ctrl_c_typed_in_the_terminal_quits_when_nothing_runs() {
    let mut s = start("no-devices", 100, 30);
    s.wait_for("Open an image file");
    s.send(&[0x03]);
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    restored_after(&out, "");
}

#[test]
fn a_panic_gives_the_terminal_back_before_the_message() {
    let s = start("no-devices,panic", 100, 30);
    let (code, out) = s.finish();
    assert_eq!(code, Some(101), "{out:?}");
    restored_after(&out, "selftest: a panic");
}

#[test]
fn a_small_window_gets_the_message_and_a_resize_brings_the_screen_back() {
    let mut s = start("no-devices", 60, 20);
    // The screen is painted cell by cell, so spaces between words are cursor
    // movements: look for single words.
    s.wait_for("bigger");
    assert!(s.text().contains("columns"));
    assert!(!s.text().contains("Disks,"));
    // Make the window big enough: the program is told with SIGWINCH.
    let ws = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    {
        use std::os::fd::AsRawFd;
        // SAFETY: TIOCSWINSZ takes a pointer to a winsize; the descriptor is
        // the master of the pseudo-terminal.
        let rc = unsafe { libc::ioctl(s.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        assert_eq!(rc, 0);
    }
    s.wait_for("Disks,");
    s.send(b"q");
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    restored_after(&out, "");
}

#[test]
fn without_a_terminal_it_explains_and_scripts_are_pointed_to_the_cli() {
    let out = Command::new(env!("CARGO_BIN_EXE_diskrescue-tui"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("needs a terminal"), "{err}");
    assert!(err.contains("diskrescue --help"), "{err}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\x1b["));
}

#[test]
fn help_and_version_and_unknown_options() {
    let out = Command::new(env!("CARGO_BIN_EXE_diskrescue-tui"))
        .arg("--help")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("sudo diskrescue-tui") && text.contains("--no-mouse"),
        "{text}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_diskrescue-tui"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("diskrescue-tui 0."));
    let out = Command::new(env!("CARGO_BIN_EXE_diskrescue-tui"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown option '--bogus'"));
}
