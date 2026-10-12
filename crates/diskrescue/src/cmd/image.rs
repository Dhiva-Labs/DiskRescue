//! `diskrescue image`: a careful, resumable copy of a failing device.
//!
//! This is the only command that reads a device in earnest, and the code
//! path shared with `recover --image-to`. The checks before copying and the
//! engine set-up are shared with the other front-ends
//! (`diskrescue_app::imaging`); this module shows the progress (full-screen
//! view, plain lines or JSON lines) and the summary.

use std::io::{Read, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_app::imaging::{self as app_imaging, Prepared, Setup};
#[cfg(test)]
use diskrescue_app::imaging::{read_identity, write_identity};
use diskrescue_app::json::StartInfo;
use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
#[cfg(test)]
use diskrescue_core::Range;
#[cfg(test)]
use diskrescue_device::Fingerprint;
use diskrescue_imaging::{Control, Event, Phase, Progress, Stats};
#[cfg(test)]
use diskrescue_imaging::{Imager, ImagingOptions, MapFile, Summary};

#[cfg(test)]
pub use diskrescue_app::imaging::identity_path;
pub use diskrescue_app::imaging::ImagingDone;
pub use diskrescue_app::json::event_json;
pub use diskrescue_app::text::{is_engine_chatter, phase_label as phase_short};

use crate::args::ImageArgs;
#[cfg(test)]
use crate::args::Tuning;
use crate::error::{CliError, CmdResult};
use crate::signal::Interrupt;
use crate::source::default_map_path;
use crate::term;
use crate::tui::{self, Dashboard, Header};
use crate::util::{self, percent, plural, shell_quote};
use crate::{logging, profile_text};

/// How progress is shown while imaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    /// Full-screen dashboard.
    Tui,
    /// A text line every few seconds.
    Plain,
    /// JSON objects on standard output.
    Json,
}

impl UiMode {
    /// The mode for the user's flags: the dashboard needs a terminal on
    /// both input and output.
    pub fn choose(no_tui: bool, json: bool) -> UiMode {
        if json {
            UiMode::Json
        } else if no_tui || !term::stdout_is_tty() || !term::stdin_is_tty() {
            UiMode::Plain
        } else {
            UiMode::Tui
        }
    }
}

/// Everything an imaging run needs.
pub struct ImagePlan<'a> {
    /// Device, partition or image file to read.
    pub source: &'a Path,
    /// Image file to write.
    pub image: &'a Path,
    /// Mapfile (default `IMAGE.map`).
    pub map: Option<&'a Path>,
    /// Strategy overrides.
    pub tuning: &'a diskrescue_app::plan::Tuning,
    /// First byte to copy.
    pub start: Option<u64>,
    /// Number of bytes to copy.
    pub size: Option<u64>,
    /// Overwrite an existing image that has no mapfile.
    pub force: bool,
    /// Allow a mounted source.
    pub allow_mounted: bool,
    /// Check free space on the output disk.
    pub space_check: bool,
    /// How to show progress.
    pub ui: UiMode,
    /// The command line, for the mapfile header and "run it again" advice.
    pub command_line: String,
}

impl ImagePlan<'_> {
    /// The same plan for the app crate.
    fn to_app(&self) -> diskrescue_app::plan::ImagePlan {
        diskrescue_app::plan::ImagePlan {
            source: self.source.to_path_buf(),
            image: self.image.to_path_buf(),
            map: self.map.map(Path::to_path_buf),
            tuning: self.tuning.clone(),
            start: self.start,
            size: self.size,
            force: self.force,
            allow_mounted: self.allow_mounted,
            no_space_check: !self.space_check,
            command_line: Some(self.command_line.clone()),
        }
    }
}

/// Apply `--profile`, `--block-size`, `--timeout`, `--retries`, `--no-trim`
/// and `--no-scrape` to the profile recommended for `kind` (kept for the
/// tests; runs use `diskrescue_app::imaging::build_profile`).
#[cfg(test)]
pub fn build_profile(kind: DeviceKind, t: &Tuning) -> Result<ImagingProfile, CliError> {
    app_imaging::build_profile(kind, &super::tuning(t)).map_err(CliError::from)
}

/// The rescue domain for `--start` / `--size` (kept for the tests).
#[cfg(test)]
pub fn build_domain(
    start: Option<u64>,
    size: Option<u64>,
    source_size: u64,
) -> Result<Option<Range>, CliError> {
    app_imaging::build_domain(start, size, source_size).map_err(CliError::from)
}

/// Refuse to continue a copy onto a device that is not the one it started
/// on (kept for the tests).
#[cfg(test)]
pub fn check_same_device(
    before: &Fingerprint,
    now: &DeviceInfo,
    map: &Path,
) -> Result<(), CliError> {
    app_imaging::check_same_device(before, now, map).map_err(CliError::from)
}

/// Run an imaging job. Prints its own progress; the caller prints the
/// summary with [`print_summary`].
pub fn run_imaging(
    plan: &ImagePlan<'_>,
    interrupt: &Arc<Interrupt>,
) -> Result<ImagingDone, CliError> {
    // Every check before copying, and opening the source, are shared with
    // the other front-ends. Warnings are printed as soon as the source is
    // open, before anything else can fail.
    let setup = Setup {
        cancel: Some(interrupt.flag()),
        map_snapshot_interval: (plan.ui == UiMode::Tui).then(|| Duration::from_millis(250)),
    };
    let prepared = app_imaging::prepare(&plan.to_app(), &setup, &mut |w| super::print_warnings(w))?;
    let ready = match prepared {
        Prepared::AlreadyComplete(done) => return Ok(*done),
        Prepared::Ready(r) => *r,
    };
    let imager = ready.imager;
    let info = ready.info;
    let profile = ready.profile;
    let map_path = ready.mapfile;
    let resumed = ready.resumed;
    let before = ready.before;
    let domain_len = ready.copy_size;
    let header = Header {
        source: format!(
            "{}{}",
            info.path.display(),
            if info.kind == DeviceKind::ImageFile {
                String::new()
            } else {
                format!(" ({})", diskrescue_device::device_label(&info))
            }
        ),
        detail: format!("{} -> {}", info.kind.display_name(), plan.image.display()),
        profile: format!(
            "profile {}: {}",
            profile.name,
            profile_text::blurb(&profile.name)
        ),
    };
    if plan.ui != UiMode::Json {
        print_preface(&info, &profile, plan, &map_path, resumed.then_some(before));
    } else {
        println!(
            "{}",
            diskrescue_app::json::start_json(&StartInfo {
                source: info.path.clone(),
                image: plan.image.to_path_buf(),
                mapfile: map_path.clone(),
                profile: profile.name.clone(),
                size: domain_len,
                resumed,
                command_line: plan.command_line.clone(),
            })
        );
    }

    let control = Control::new();
    interrupt.attach(control.clone());
    let (tx, rx) = mpsc::channel::<Event>();
    let engine_control = control.clone();
    let mut imager = imager;
    let handle = std::thread::Builder::new()
        .name("imaging".into())
        .spawn(move || {
            imager.run(engine_control, &mut |e| {
                let _ = tx.send(e);
            })
        })
        .map_err(|e| CliError::failed(format!("cannot start the imaging thread: {e}")))?;

    match plan.ui {
        UiMode::Tui => {
            logging::start_capture();
            match tui::Session::start() {
                Ok(mut session) => {
                    let mut dash = Dashboard::new(header, term::colors_for_tui());
                    let res = tui::run(&mut session, &mut dash, &rx, &control, interrupt);
                    drop(session);
                    for l in logging::stop_capture() {
                        eprintln!("{l}");
                    }
                    if let Err(e) = res {
                        // The terminal failed: keep the run going without a screen.
                        eprintln!("The full-screen view stopped ({e}); continuing with plain progress lines.");
                        plain_loop(&rx, Duration::from_secs(2));
                    }
                }
                Err(e) => {
                    let _ = logging::stop_capture();
                    eprintln!(
                        "Cannot open the full-screen view ({e}); using plain progress lines."
                    );
                    plain_loop(&rx, Duration::from_secs(2));
                }
            }
        }
        UiMode::Plain => plain_loop(&rx, Duration::from_secs(2)),
        UiMode::Json => json_loop(&rx),
    }

    let joined = handle.join();
    interrupt.detach_all();
    let summary = match joined {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(app_imaging::engine_error(&map_path, e).into()),
        Err(_) => return Err(app_imaging::engine_panicked(&map_path).into()),
    };
    Ok(ImagingDone::from_summary(
        summary,
        info,
        plan.image.to_path_buf(),
        map_path,
    ))
}

fn print_preface(
    info: &DeviceInfo,
    profile: &ImagingProfile,
    plan: &ImagePlan<'_>,
    map: &Path,
    resumed: Option<Stats>,
) {
    let p = term::out();
    println!(
        "{} {} ({}) to {}",
        p.bold("Copying"),
        info.path.display(),
        if info.kind == DeviceKind::ImageFile {
            human::bytes(info.size)
        } else {
            diskrescue_device::device_label(info)
        },
        plan.image.display()
    );
    println!(
        "  Profile:      {} - {}",
        profile.name,
        profile_text::blurb(&profile.name)
    );
    println!("  Progress map: {}", map.display());
    if let Some(st) = resumed {
        println!(
            "  Resuming:     {} of {} already copied",
            percent(st.rescued, st.total()),
            human::bytes(st.total())
        );
    }
    println!("  Press Ctrl-C to stop. Progress is saved; run the same command again to continue.");
    println!();
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------- plain

/// One progress line for plain output.
pub fn plain_line(p: &Progress) -> String {
    let s = &p.stats;
    let eta = match p.eta {
        Some(d) if d > Duration::ZERO => format!("about {} left in this phase", human::duration(d)),
        Some(_) => "phase almost done".to_string(),
        None => "estimating time".to_string(),
    };
    format!(
        "{}: {} copied ({} of {}), unreadable {}, {} now, {}, {}",
        phase_short(&p.phase),
        percent(s.rescued, s.total()),
        human::bytes(s.rescued),
        human::bytes(s.total()),
        human::bytes(s.bad_sectors),
        human::rate(p.current_rate),
        eta,
        plural(p.errors, "read error", "read errors")
    )
}

/// Turns engine events into text lines, printing progress at most every `every`.
pub struct PlainPrinter {
    every: Duration,
    last: Option<Instant>,
    latest: Option<Progress>,
}

impl PlainPrinter {
    /// A printer that shows a progress line every `every`.
    pub fn new(every: Duration) -> Self {
        PlainPrinter {
            every,
            // The first line comes after one interval, so short runs only
            // print the summary.
            last: Some(Instant::now()),
            latest: None,
        }
    }

    /// Feed an event; lines to show are passed to `out`.
    pub fn event(&mut self, ev: &Event, out: &mut dyn FnMut(String)) {
        match ev {
            Event::Progress(p) => self.latest = Some(p.clone()),
            Event::Phase(Phase::Finished) => {}
            Event::Phase(p) => out(format!("-- {}", phase_short(p))),
            Event::Log(level, msg) => match level {
                log::Level::Error => out(format!("error: {msg}")),
                log::Level::Warn => out(format!("warning: {msg}")),
                _ if is_engine_chatter(msg) => {}
                _ => out(format!("note: {msg}")),
            },
            Event::DeviceLost { at } => out(format!(
                "warning: the device stopped responding at {}. Waiting for it to come back \
                 (Ctrl-C stops and saves)...",
                human::bytes(*at)
            )),
            Event::DeviceBack { after } => out(format!(
                "the device is back after {}; continuing",
                human::duration(*after)
            )),
            Event::Paused(d) => out(format!(
                "pausing for {} to let the device recover",
                human::duration(*d)
            )),
            Event::Map(_) | Event::Saved | Event::Finished(_) => {}
        }
    }

    /// Show the latest progress if it is due.
    pub fn tick(&mut self, now: Instant, out: &mut dyn FnMut(String)) {
        let due = self
            .last
            .is_none_or(|t| now.duration_since(t) >= self.every);
        if due {
            if let Some(p) = &self.latest {
                out(plain_line(p));
                self.last = Some(now);
            }
        }
    }
}

fn plain_loop(rx: &Receiver<Event>, every: Duration) {
    let mut printer = PlainPrinter::new(every);
    let mut emit = |line: String| eprintln!("{line}");
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Finished(_)) => break,
            Ok(ev) => printer.event(&ev, &mut emit),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        printer.tick(Instant::now(), &mut emit);
    }
}

// ---------------------------------------------------------------- json

fn json_loop(rx: &Receiver<Event>) {
    let mut last_progress: Option<Instant> = None;
    while let Ok(ev) = rx.recv() {
        let done = matches!(ev, Event::Finished(_));
        let is_progress = matches!(ev, Event::Progress(_));
        let throttled =
            is_progress && last_progress.is_some_and(|t| t.elapsed() < Duration::from_secs(1));
        if !throttled {
            if is_progress {
                last_progress = Some(Instant::now());
            }
            if let Some(v) = event_json(&ev) {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{v}");
                let _ = out.flush();
            }
        }
        if done {
            break;
        }
    }
}

// ---------------------------------------------------------------- summary

/// The text printed when imaging ends (without the "next step" advice).
pub fn summary_text(done: &ImagingDone, resume_command: &str) -> String {
    let p = term::out();
    let st = &done.stats;
    let mut out = String::new();
    if done.already_complete {
        out.push_str(&format!(
            "{}\n",
            p.green(&format!(
                "{} is already completely copied; there is nothing more to do.",
                done.image.display()
            ))
        ));
        out.push_str(&format!(
            "  Image:         {} ({})\n  Progress map:  {}\n",
            done.image.display(),
            human::bytes(st.total()),
            done.mapfile.display()
        ));
        return out;
    }
    let Some(sum) = &done.summary else {
        return out;
    };
    if sum.cancelled {
        out.push_str(&format!(
            "{}\n",
            p.yellow(&format!(
                "Stopped. Progress is saved: {} copied so far.",
                percent(st.rescued, st.total())
            ))
        ));
        out.push_str("To continue where it left off, run the same command again:\n");
        out.push_str(&format!("  {resume_command}\n"));
    } else if sum.finished {
        out.push_str(&format!(
            "{}\n",
            p.green(&format!(
                "Finished copying in {}.",
                human::duration(sum.elapsed)
            ))
        ));
    } else {
        out.push_str(&format!(
            "{}\n",
            p.yellow(&format!(
                "The copy stopped before it was complete ({} copied).",
                percent(st.rescued, st.total())
            ))
        ));
    }
    out.push_str(&format!(
        "  Copied:        {} of {} ({})\n",
        human::bytes(st.rescued),
        human::bytes(st.total()),
        percent(st.rescued, st.total())
    ));
    if st.bad_sectors > 0 {
        out.push_str(&format!(
            "  Unreadable:    {}\n",
            p.red(&format!(
                "{} in {}",
                human::bytes(st.bad_sectors),
                plural(st.bad_areas, "area", "areas")
            ))
        ));
    } else if sum.finished {
        out.push_str("  Unreadable:    none\n");
    }
    if st.pending() > 0 {
        out.push_str(&format!(
            "  Not finished:  {} still to copy or retry\n",
            human::bytes(st.pending())
        ));
    }
    if sum.errors + sum.timeouts + sum.slow_reads + sum.reattaches > 0 {
        out.push_str(&format!(
            "  Problems met:  {}\n",
            crate::tui::problems_text(sum.errors, sum.timeouts, sum.slow_reads, sum.reattaches)
        ));
    }
    out.push_str(&format!(
        "  Image:         {}\n  Progress map:  {}\n",
        done.image.display(),
        done.mapfile.display()
    ));
    if !sum.warnings.is_empty() {
        out.push_str("\nNotes:\n");
        let n = sum.warnings.len();
        for w in sum.warnings.iter().skip(n.saturating_sub(6)) {
            out.push_str(&format!("  - {w}\n"));
        }
        if n > 6 {
            out.push_str(&format!(
                "  ({} earlier notes are in the log; use -v)\n",
                n - 6
            ));
        }
    }
    if sum.finished && st.bad_sectors > 0 {
        out.push_str(
            "\nFiles stored in the unreadable areas will be damaged; everything else is intact. \
             Running the same command again tries the unreadable areas once more.\n",
        );
    }
    if !sum.finished && !sum.cancelled {
        out.push_str(&format!(
            "\nTo continue, run the same command again:\n  {resume_command}\n\
             You can also recover what has been copied so far: diskrescue recover {} -o <folder>\n",
            shell_quote(&done.image.display().to_string())
        ));
    }
    out
}

/// The command that recovers files from a finished image.
pub fn recover_hint(done: &ImagingDone) -> String {
    let default_map = default_map_path(&done.image);
    let map_arg = if done.mapfile == default_map {
        String::new()
    } else {
        format!(
            " --map {}",
            shell_quote(&done.mapfile.display().to_string())
        )
    };
    format!(
        "diskrescue recover {}{map_arg} -o <folder for the recovered files>",
        shell_quote(&done.image.display().to_string())
    )
}

/// Print the summary and the next step; returns the exit code.
pub fn print_summary(done: &ImagingDone, resume_command: &str, json: bool, show_next: bool) -> i32 {
    if !json {
        print!("{}", summary_text(done, resume_command));
        if done.finished() && show_next {
            println!();
            println!("Next step: {}", recover_hint(done));
        }
    }
    if done.cancelled() {
        crate::error::exit::INTERRUPTED
    } else if done.finished() {
        crate::error::exit::OK
    } else {
        crate::error::exit::ERROR
    }
}

/// Stop cleanly (save the progress map, exit 130) when standard input is
/// closed: for programs that start the command with elevated rights and
/// may not be allowed to signal it (`--stop-on-stdin-close`).
fn stop_when_stdin_closes(interrupt: Arc<Interrupt>) {
    let spawned = std::thread::Builder::new()
        .name("stdin-watch".into())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 512];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            if !interrupt.is_set() {
                eprintln!(
                    "Stopping: standard input was closed. Finishing the current step and saving what \
                     has been done so far."
                );
            }
            interrupt.trigger();
        });
    if let Err(e) = spawned {
        log::warn!("cannot watch standard input: {e}");
    }
}

/// Run the command.
pub fn run(args: &ImageArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    if args.stop_on_stdin_close {
        stop_when_stdin_closes(interrupt.clone());
    }
    let command_line = util::command_line();
    let tuning = super::tuning(&args.tuning);
    let plan = ImagePlan {
        source: &args.source,
        image: &args.image,
        map: args.map.as_deref(),
        tuning: &tuning,
        start: args.start,
        size: args.size,
        force: args.force,
        allow_mounted: args.allow_mounted,
        space_check: !args.no_space_check,
        ui: UiMode::choose(args.no_tui || args.stop_on_stdin_close, args.json_progress),
        command_line: command_line.clone(),
    };
    let done = run_imaging(&plan, interrupt)?;
    Ok(print_summary(
        &done,
        &command_line,
        args.json_progress,
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::Transport;

    fn tuning() -> Tuning {
        Tuning::default()
    }

    #[test]
    fn profile_follows_the_device_kind_and_overrides() {
        let p = build_profile(DeviceKind::UsbFlash, &tuning()).unwrap();
        assert_eq!(p.name, "usb-flash");
        let t = Tuning {
            profile: Some("hdd".into()),
            timeout: Some(Duration::from_secs(3)),
            block_size: Some(1 << 20),
            retries: Some(7),
            no_trim: true,
            no_scrape: true,
            reverse: false,
        };
        let p = build_profile(DeviceKind::UsbFlash, &t).unwrap();
        assert_eq!(p.name, "hdd");
        assert_eq!(p.timeout_copy, Duration::from_secs(3));
        assert_eq!(p.timeout_scrape, Duration::from_secs(3));
        assert_eq!(p.timeout_retry, Duration::from_secs(3));
        assert_eq!(p.block_size, 1 << 20);
        assert_eq!(p.retry_passes, 7);
        assert!(!p.trim && !p.scrape);
        let bad = Tuning {
            profile: Some("warp".into()),
            ..Tuning::default()
        };
        assert_eq!(
            build_profile(DeviceKind::Hdd, &bad)
                .unwrap_err()
                .exit_code(),
            2
        );
        let zero = Tuning {
            block_size: Some(0),
            ..Tuning::default()
        };
        assert!(build_profile(DeviceKind::Hdd, &zero).is_err());
    }

    #[test]
    fn domain_from_start_and_size() {
        assert_eq!(build_domain(None, None, 1000).unwrap(), None);
        assert_eq!(
            build_domain(Some(100), Some(200), 1000).unwrap(),
            Some(Range::new(100, 300))
        );
        assert_eq!(
            build_domain(Some(900), Some(500), 1000).unwrap(),
            Some(Range::new(900, 1000))
        );
        assert_eq!(
            build_domain(None, Some(10), 1000).unwrap(),
            Some(Range::new(0, 10))
        );
        assert_eq!(
            build_domain(Some(10), None, 1000).unwrap(),
            Some(Range::new(10, 1000))
        );
        assert!(build_domain(Some(1000), None, 1000).is_err());
        assert!(build_domain(Some(0), Some(0), 1000).is_err());
    }

    fn progress(rescued: u64, total: u64) -> Progress {
        Progress {
            phase: Phase::Copying {
                pass: 1,
                reverse: false,
            },
            stats: Stats {
                rescued,
                non_tried: total - rescued,
                ..Stats::default()
            },
            current_pos: rescued,
            current_rate: 10.0 * 1048576.0,
            average_rate: 9.0 * 1048576.0,
            elapsed: Duration::from_secs(30),
            eta: Some(Duration::from_secs(90)),
            errors: 2,
            slow_reads: 0,
            timeouts: 0,
            reattaches: 0,
            last_error: None,
            size: total,
        }
    }

    #[test]
    fn plain_line_has_the_facts() {
        let l = plain_line(&progress(1 << 29, 1 << 30));
        for needle in [
            "copying pass 1 (forwards)",
            "50.0% copied",
            "512 MiB of 1.00 GiB",
            "unreadable 0 B",
            "10.0 MiB/s",
            "1m 30s",
            "2 read errors",
        ] {
            assert!(l.contains(needle), "missing {needle:?} in {l}");
        }
    }

    #[test]
    fn plain_printer_throttles_progress_and_reports_events() {
        let mut pr = PlainPrinter::new(Duration::from_secs(2));
        let mut lines = Vec::new();
        let t0 = Instant::now();
        pr.tick(t0, &mut |l| lines.push(l));
        assert!(
            lines.is_empty(),
            "nothing to show before the first progress"
        );
        pr.event(&Event::Progress(progress(10, 100)), &mut |l| lines.push(l));
        pr.tick(t0 + Duration::from_millis(500), &mut |l| lines.push(l));
        assert!(lines.is_empty(), "the first line waits one interval");
        pr.tick(t0 + Duration::from_secs(3), &mut |l| lines.push(l));
        assert_eq!(lines.len(), 1);
        pr.tick(t0 + Duration::from_secs(4), &mut |l| lines.push(l));
        assert_eq!(lines.len(), 1, "throttled");
        pr.tick(t0 + Duration::from_secs(6), &mut |l| lines.push(l));
        assert_eq!(lines.len(), 2);
        pr.event(&Event::DeviceLost { at: 4096 }, &mut |l| lines.push(l));
        pr.event(
            &Event::DeviceBack {
                after: Duration::from_secs(5),
            },
            &mut |l| lines.push(l),
        );
        pr.event(
            &Event::Log(log::Level::Warn, "read error".into()),
            &mut |l| lines.push(l),
        );
        pr.event(&Event::Phase(Phase::Trimming), &mut |l| lines.push(l));
        let all = lines.join("\n");
        assert!(all.contains("stopped responding") && all.contains("is back after 5s"));
        assert!(all.contains("warning: read error") && all.contains("-- trimming failed areas"));
    }

    #[test]
    fn json_events_are_valid_and_tagged() {
        let v = event_json(&Event::Progress(progress(5, 10))).unwrap();
        assert_eq!(v["event"], "progress");
        assert_eq!(v["phase"]["name"], "copying");
        assert_eq!(v["stats"]["total"], 10);
        assert!(
            event_json(&Event::Map(Arc::new(diskrescue_imaging::StatusMap::new(
                10
            ))))
            .is_none()
        );
        let v = event_json(&Event::Log(log::Level::Warn, "x".into())).unwrap();
        assert_eq!(v["level"], "warn");
        let v = event_json(&Event::DeviceBack {
            after: Duration::from_secs(2),
        })
        .unwrap();
        assert_eq!(v["event"], "device_back");
    }

    fn info() -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from("/dev/sdb"),
            name: "sdb".into(),
            kind: DeviceKind::UsbFlash,
            transport: Transport::Usb,
            size: 1 << 30,
            logical_sector: 512,
            physical_sector: 512,
            model: None,
            vendor: None,
            serial: None,
            firmware: None,
            stable_id: None,
            removable: true,
            rotational: false,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![],
            health: None,
            warnings: vec![],
        }
    }

    fn done(finished: bool, cancelled: bool, bad: u64) -> ImagingDone {
        let stats = Stats {
            rescued: (1 << 30) - bad,
            bad_sectors: bad,
            bad_areas: u64::from(bad > 0),
            non_tried: 0,
            ..Stats::default()
        };
        let stats = if finished {
            stats
        } else {
            Stats {
                rescued: 1 << 29,
                non_tried: 1 << 29,
                bad_sectors: 0,
                bad_areas: 0,
                ..Stats::default()
            }
        };
        ImagingDone {
            stats,
            summary: Some(Summary {
                stats,
                elapsed: Duration::from_secs(724),
                errors: 5,
                finished,
                cancelled,
                reattaches: 1,
                timeouts: 2,
                slow_reads: 0,
                output: PathBuf::from("card.img"),
                mapfile: PathBuf::from("card.img.map"),
                warnings: vec!["a warning".into()],
            }),
            info: info(),
            image: PathBuf::from("card.img"),
            mapfile: PathBuf::from("card.img.map"),
            already_complete: false,
        }
    }

    #[test]
    fn summaries_tell_the_user_what_to_do() {
        let cmd = "diskrescue image /dev/sdb card.img";
        let s = summary_text(&done(true, false, 0), cmd);
        assert!(s.contains("Finished copying in 12m 04s"), "{s}");
        assert!(s.contains("Unreadable:    none"), "{s}");
        assert!(s.contains("5 read errors, 2 timeouts"), "{s}");
        assert!(s.contains("1 reconnect"), "{s}");

        let s = summary_text(&done(true, false, 131072), cmd);
        assert!(s.contains("128 KiB in 1 area"), "{s}");
        assert!(s.contains("will be damaged"), "{s}");

        let s = summary_text(&done(false, true, 0), cmd);
        assert!(s.contains("Stopped. Progress is saved"), "{s}");
        assert!(s.contains(cmd), "{s}");

        let s = summary_text(&done(false, false, 0), cmd);
        assert!(s.contains("stopped before it was complete"), "{s}");
        assert!(s.contains("diskrescue recover card.img"), "{s}");
    }

    #[test]
    fn exit_codes_follow_the_outcome() {
        assert_eq!(print_summary(&done(true, false, 0), "x", true, true), 0);
        assert_eq!(print_summary(&done(false, true, 0), "x", true, true), 130);
        assert_eq!(print_summary(&done(false, false, 0), "x", true, true), 1);
        let mut d = done(true, false, 0);
        d.already_complete = true;
        d.summary = None;
        assert_eq!(print_summary(&d, "x", true, true), 0);
        assert!(summary_text(&d, "x").contains("already completely copied"));
    }

    /// Runs the real engine over a simulated failing device and pushes every
    /// event through the code that shows it.
    #[test]
    fn a_failing_device_flows_through_every_display_path() {
        use diskrescue_core::source::SharedSource;
        use diskrescue_imaging::simulate::FaultySource;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let dir = tempfile::tempdir().unwrap();
        let mut data = vec![0u8; 1 << 20];
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for b in data.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let bad = Range::new(256 << 10, (256 << 10) + 4096);
        let src: SharedSource = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
        let img = dir.path().join("sim.img");
        let map = dir.path().join("sim.img.map");
        let mut opts = ImagingOptions::with_profile(ImagingProfile::generic());
        opts.map_snapshot_interval = Some(Duration::from_millis(1));
        opts.progress_interval = Duration::ZERO;
        let mut imager = Imager::new(src, &img, &map, opts).unwrap();
        let mut events = Vec::new();
        let summary = imager.run(Control::new(), &mut |e| events.push(e)).unwrap();
        assert!(summary.finished && !summary.cancelled);
        assert_eq!(summary.stats.bad_sectors, 4096);

        // The image has the good data and zeros where the device failed.
        let image = std::fs::read(&img).unwrap();
        assert_eq!(&image[..256 << 10], &data[..256 << 10]);
        assert!(image[bad.start as usize..bad.end as usize]
            .iter()
            .all(|b| *b == 0));
        assert_eq!(&image[bad.end as usize..], &data[bad.end as usize..]);

        // Plain lines, JSON lines and the dashboard all cope with the stream.
        let mut printer = PlainPrinter::new(Duration::ZERO);
        let mut lines = Vec::new();
        let mut dash = Dashboard::new(Header::default(), true);
        for ev in &events {
            printer.event(ev, &mut |l| lines.push(l));
            printer.tick(Instant::now(), &mut |l| lines.push(l));
            if let Some(v) = event_json(ev) {
                assert!(v["event"].is_string());
            }
            dash.apply(ev);
        }
        let all = lines.join("\n");
        assert!(all.contains("copying pass 1"), "{all}");
        assert!(all.contains("read errors"), "{all}");
        let mut term = Terminal::new(TestBackend::new(120, 36)).unwrap();
        term.draw(|f| dash.render(f)).unwrap();

        // The summary tells the user exactly what was lost.
        let done = ImagingDone {
            stats: summary.stats,
            summary: Some(summary),
            info: info(),
            image: img.clone(),
            mapfile: map.clone(),
            already_complete: false,
        };
        let text = summary_text(&done, "diskrescue image /dev/sdb sim.img");
        assert!(text.contains("Finished copying"), "{text}");
        assert!(text.contains("4.00 KiB in 1 area"), "{text}");
        assert!(text.contains("will be damaged"), "{text}");
        assert_eq!(print_summary(&done, "x", true, false), 0);
        // And the mapfile lists the unreadable area for `diskrescue map`.
        let mf = MapFile::load(&map).unwrap();
        assert_eq!(mf.map.stats().bad_sectors, 4096);
    }

    #[test]
    fn a_different_device_is_not_mixed_into_the_image() {
        let mut dev = info();
        dev.serial = Some("SERIAL-ONE-1234".into());
        dev.model = Some("Ultra".into());
        let before = Fingerprint::of(&dev, None);
        assert!(check_same_device(&before, &dev, Path::new("a.map")).is_ok());

        // Same card after a re-plug under another name: fine.
        let mut again = dev.clone();
        again.path = PathBuf::from("/dev/sdc");
        assert!(check_same_device(&before, &again, Path::new("a.map")).is_ok());

        // Another card of the same model and size: refused.
        let mut other = dev.clone();
        other.serial = Some("SERIAL-TWO-5678".into());
        let err = check_same_device(&before, &other, Path::new("a.map")).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        let m = err.to_string();
        assert!(
            m.contains("different device") && m.contains("--force"),
            "{m}"
        );

        // A device that merely reports no serial this time is accepted.
        let mut quiet = dev.clone();
        quiet.serial = None;
        assert!(check_same_device(&before, &quiet, Path::new("a.map")).is_ok());
    }

    #[test]
    fn identity_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = identity_path(&dir.path().join("card.img.map"));
        assert!(p.to_string_lossy().ends_with("card.img.map.device.json"));
        assert!(read_identity(&p).is_none());
        let fp = Fingerprint::of(&info(), None);
        write_identity(&p, &fp);
        assert_eq!(read_identity(&p), Some(fp));
        std::fs::write(&p, "garbage").unwrap();
        assert!(read_identity(&p).is_none());
    }

    #[test]
    fn recover_hint_mentions_a_custom_map() {
        let mut d = done(true, false, 0);
        assert!(!recover_hint(&d).contains("--map"));
        d.mapfile = PathBuf::from("other.map");
        assert!(recover_hint(&d).contains("--map other.map"));
    }
}
