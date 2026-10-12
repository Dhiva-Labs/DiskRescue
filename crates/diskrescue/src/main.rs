//! DiskRescue command-line tool.
//!
//! `diskrescue image` copies a failing device into a file (careful and
//! resumable), `diskrescue recover` gets photos and videos out of a device
//! or image, `diskrescue repair` fixes damaged media files. See
//! `docs/USAGE.md` for the full user guide.
//!
//! The device is only ever read. Exit codes: 0 finished, 1 error, 2 invalid
//! command line, 3 refused for safety, 130 interrupted.

mod args;
mod cmd;
mod error;
mod logging;
mod profile_text;
mod progress;
mod signal;
mod source;
mod term;
mod tui;
mod util;

use std::io::Write;
use std::panic::PanicHookInfo;
use std::sync::Arc;

use clap::Parser;

use args::{Cli, Command};
use error::{CliError, CmdResult};
use signal::Interrupt;

fn main() {
    let code = real_main();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

fn real_main() -> i32 {
    reset_sigpipe();
    let cli = Cli::parse();
    term::set_color_choice(cli.color);
    logging::init(cli.verbose);
    install_panic_hook();
    // Only commands that can stop cleanly take over Ctrl-C; for the quick ones
    // the default behaviour (end at once) is what the user expects.
    let interrupt = if can_stop_cleanly(&cli.command) {
        signal::install()
    } else {
        signal::global()
    };
    match dispatch(cli.command, &interrupt) {
        Ok(code) => code,
        Err(e) => {
            report_error(&e);
            e.exit_code()
        }
    }
}

/// Commands that save their progress or report when asked to stop.
fn can_stop_cleanly(command: &Command) -> bool {
    matches!(
        command,
        Command::Image(_) | Command::Scan(_) | Command::Recover(_) | Command::Carve(_)
    )
}

fn dispatch(command: Command, interrupt: &Arc<Interrupt>) -> CmdResult {
    match command {
        Command::List(a) => cmd::list::run(&a),
        Command::Info(a) => cmd::info::run(&a),
        Command::Image(a) => cmd::image::run(&a, interrupt),
        Command::Scan(a) => cmd::scan::run(&a, interrupt),
        Command::Recover(a) => cmd::recover::run(&a, interrupt),
        Command::Carve(a) => cmd::carve::run(&a, interrupt),
        Command::Repair(a) => cmd::repair::run(&a, interrupt),
        Command::Verify(a) => cmd::verify::run(&a),
        Command::Map(a) => cmd::map::run(&a),
    }
}

/// Print an error the way a calm person would explain it.
fn report_error(e: &CliError) {
    let p = term::err();
    match e {
        CliError::Refused(m) => {
            eprintln!("{} {m}", p.bold_red("Refused for safety:"));
            eprintln!("{}", p.dim("Nothing was written to your device."));
        }
        CliError::Usage(m) => {
            eprintln!("{} {m}", p.bold_red("error:"));
            eprintln!("{}", p.dim("Run 'diskrescue --help' for usage."));
        }
        CliError::Failed(m) => eprintln!("{} {m}", p.bold_red("error:")),
    }
}

/// True for panics raised inside third-party decoders whose failures the
/// format crates deliberately catch (`catch_unwind`) on corrupt data.
fn is_decoder_panic(file: &str, on_main_thread: bool) -> bool {
    let f = file.replace('\\', "/");
    f.contains("/zune-") || (!on_main_thread && f.contains("/registry/src/"))
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        tui::restore();
        let on_main = std::thread::current().name() == Some("main");
        let from_decoder = info
            .location()
            .is_some_and(|l| is_decoder_panic(l.file(), on_main));
        if from_decoder {
            log::debug!("a media decoder panicked on damaged data (recovered): {info}");
            return;
        }
        default(info);
        eprintln!(
            "\nThis is a bug in DiskRescue. Nothing was written to your device. \
             Please report it, including the text above."
        );
    }));
}

/// Let `diskrescue list | head` end quietly instead of panicking.
fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: restoring the default disposition of SIGPIPE is async-signal
    // safe and has no memory-safety preconditions.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_long_running_commands_take_over_ctrl_c() {
        let parse = |args: &[&str]| {
            let mut v = vec!["diskrescue"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v).unwrap().command
        };
        assert!(can_stop_cleanly(&parse(&["image", "a", "b"])));
        assert!(can_stop_cleanly(&parse(&["recover", "a", "-o", "b"])));
        assert!(can_stop_cleanly(&parse(&["carve", "a", "-o", "b"])));
        assert!(can_stop_cleanly(&parse(&["scan", "a"])));
        assert!(!can_stop_cleanly(&parse(&["list"])));
        assert!(!can_stop_cleanly(&parse(&["info", "a"])));
        assert!(!can_stop_cleanly(&parse(&["verify", "a"])));
        assert!(!can_stop_cleanly(&parse(&["map", "a"])));
    }

    #[test]
    fn decoder_panics_are_recognised() {
        assert!(is_decoder_panic(
            "/home/u/.cargo/registry/src/index.crates.io-1/zune-jpeg-0.4.14/src/decoder.rs",
            true
        ));
        assert!(is_decoder_panic(
            r"C:\Users\u\.cargo\registry\src\index.crates.io-1\zune-core-0.4.12\src\x.rs",
            false
        ));
        assert!(is_decoder_panic(
            "/home/u/.cargo/registry/src/index.crates.io-1/image-0.25.0/src/x.rs",
            false
        ));
        assert!(!is_decoder_panic(
            "/home/u/.cargo/registry/src/index.crates.io-1/image-0.25.0/src/x.rs",
            true
        ));
        assert!(!is_decoder_panic(
            "crates/diskrescue-jpeg/src/scan.rs",
            false
        ));
        assert!(!is_decoder_panic("crates/diskrescue/src/main.rs", true));
    }
}
