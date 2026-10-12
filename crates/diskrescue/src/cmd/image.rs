//! `diskrescue image`: a careful, resumable copy of a failing device.
//!
//! This is the only command that reads a device in earnest, and the code
//! path shared with `recover --image-to`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
use diskrescue_core::Range;
use diskrescue_device::{Fingerprint, Match};
use diskrescue_imaging::{
    Control, Event, Imager, ImagingOptions, MapFile, Phase, Progress, Stats, Summary,
};
use serde_json::{json, Value};

use crate::args::{ImageArgs, Tuning};
use crate::cmd::parent_dir;
use crate::error::{CliError, CmdResult};
use crate::signal::Interrupt;
use crate::source::{self, default_map_path, SourceRequest};
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
    pub tuning: &'a Tuning,
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

/// What an imaging run produced.
#[derive(Debug, Clone)]
pub struct ImagingDone {
    /// Totals at the end.
    pub stats: Stats,
    /// The engine's summary (None when the image was already complete).
    pub summary: Option<Summary>,
    /// The device that was copied.
    pub info: DeviceInfo,
    /// The image.
    pub image: PathBuf,
    /// The mapfile.
    pub mapfile: PathBuf,
    /// Nothing had to be done.
    pub already_complete: bool,
}

impl ImagingDone {
    /// All requested bytes were handled (copied or confirmed unreadable).
    pub fn finished(&self) -> bool {
        self.already_complete || self.summary.as_ref().is_some_and(|s| s.finished)
    }

    /// The run was stopped by the user.
    pub fn cancelled(&self) -> bool {
        self.summary.as_ref().is_some_and(|s| s.cancelled)
    }
}

/// Apply `--profile`, `--block-size`, `--timeout`, `--retries`, `--no-trim`
/// and `--no-scrape` to the profile recommended for `kind`.
pub fn build_profile(kind: DeviceKind, t: &Tuning) -> Result<ImagingProfile, CliError> {
    let mut p = match &t.profile {
        Some(name) => ImagingProfile::by_name(name).ok_or_else(|| {
            CliError::usage(format!(
                "unknown profile '{name}'. Choose one of: {}",
                ImagingProfile::names().join(", ")
            ))
        })?,
        None => ImagingProfile::for_kind(kind),
    };
    if let Some(bs) = t.block_size {
        if bs == 0 {
            return Err(CliError::usage("--block-size must be larger than zero"));
        }
        p.block_size = bs;
    }
    if let Some(d) = t.timeout {
        p.timeout_copy = d;
        p.timeout_scrape = d;
        p.timeout_retry = d;
    }
    if let Some(n) = t.retries {
        p.retry_passes = n;
    }
    if t.no_trim {
        p.trim = false;
    }
    if t.no_scrape {
        p.scrape = false;
    }
    Ok(p)
}

/// The rescue domain for `--start` / `--size`.
pub fn build_domain(
    start: Option<u64>,
    size: Option<u64>,
    source_size: u64,
) -> Result<Option<Range>, CliError> {
    if start.is_none() && size.is_none() {
        return Ok(None);
    }
    let s = start.unwrap_or(0);
    if s >= source_size {
        return Err(CliError::usage(format!(
            "--start {} is beyond the end of the source, which is {} long",
            human::bytes(s),
            human::bytes(source_size)
        )));
    }
    let e = match size {
        Some(0) => return Err(CliError::usage("--size must be larger than zero")),
        Some(n) => s.saturating_add(n).min(source_size),
        None => source_size,
    };
    Ok(Some(Range::new(s, e)))
}

/// File next to the mapfile that remembers which device was being copied.
pub fn identity_path(mapfile: &Path) -> PathBuf {
    let mut s = mapfile.as_os_str().to_os_string();
    s.push(".device.json");
    PathBuf::from(s)
}

/// Refuse to continue a copy onto a device that is not the one it started on.
///
/// Two cards of the same model and size would otherwise be mixed into one
/// image without any error. Only a definite difference (serial number,
/// model, size) counts; weak identities are accepted.
pub fn check_same_device(
    before: &Fingerprint,
    now: &DeviceInfo,
    map: &Path,
) -> Result<(), CliError> {
    match before.matches(now) {
        Match::Mismatch(why) => Err(CliError::refused(format!(
            "the progress map {} was made while copying a different device ({why}). Copying a second \
             device into the same image would mix the two and ruin both. Use a new image name for this \
             device, or add --force if it really is the same device (for example if its serial number \
             changed after a reset).",
            map.display()
        ))),
        _ => Ok(()),
    }
}

fn read_identity(path: &Path) -> Option<Fingerprint> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_identity(path: &Path, fp: &Fingerprint) {
    if let Ok(text) = serde_json::to_string_pretty(fp) {
        if let Err(e) = std::fs::write(path, text) {
            log::debug!("could not write {}: {e}", path.display());
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Bytes already allocated on disk for `meta` (sparse files count less).
fn allocated_bytes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512).min(meta.len())
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// Run an imaging job. Prints its own progress; the caller prints the
/// summary with [`print_summary`].
pub fn run_imaging(
    plan: &ImagePlan<'_>,
    interrupt: &Arc<Interrupt>,
) -> Result<ImagingDone, CliError> {
    // Where the image goes must not be the source.
    if same_file(plan.source, plan.image) {
        return Err(CliError::refused(format!(
            "{} is the source itself; the image must be a different file, on a different disk.",
            plan.image.display()
        )));
    }
    let image_meta = std::fs::metadata(plan.image).ok();
    if let Some(m) = &image_meta {
        if m.is_dir() {
            return Err(CliError::usage(format!(
                "{} is a folder. Give the image a file name inside it, for example {}.",
                plan.image.display(),
                plan.image.join("card.img").display()
            )));
        }
        if !m.is_file() {
            return Err(CliError::refused(format!(
                "{} exists and is not a regular file (is it a device?). The image must be a file; \
                 DiskRescue never copies one device directly onto another.",
                plan.image.display()
            )));
        }
    }
    let map_path: PathBuf = plan
        .map
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_map_path(plan.image));
    if same_file(&map_path, plan.image) || map_path == plan.image {
        return Err(CliError::usage(
            "the mapfile and the image must be different files",
        ));
    }

    // Resume or start: make sure we never clobber something by accident.
    let map_exists = std::fs::metadata(&map_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    let prior: Option<Stats> = if map_exists {
        MapFile::load(&map_path).ok().map(|m| m.map.stats())
    } else {
        None
    };
    if map_exists && prior.is_none() {
        return Err(CliError::failed(format!(
            "the progress map {} exists but cannot be read, so the copy cannot be resumed. If it is not \
             needed, move it away to start a new copy; if it matters, keep it and use a different image name.",
            map_path.display()
        )));
    }
    if prior.is_none() && image_meta.as_ref().is_some_and(|m| m.len() > 0) && !plan.force {
        return Err(CliError::refused(format!(
            "{} already exists, but there is no progress map ({}) to resume from. DiskRescue will not \
             overwrite it. Choose a new image name, or add --force if the file can be replaced.",
            plan.image.display(),
            map_path.display()
        )));
    }
    if let Some(st) = &prior {
        if image_meta.is_none() && st.rescued > 0 {
            return Err(CliError::refused(format!(
                "the progress map {} records {} already copied, but the image {} does not exist. \
                 Put the image back where it was to resume, or move the map away to start over.",
                map_path.display(),
                human::bytes(st.rescued),
                plan.image.display()
            )));
        }
    }

    // Nothing to do? Do not even touch the device.
    let domain_requested = plan.start.is_some() || plan.size.is_some();
    if let (Some(st), false) = (&prior, domain_requested) {
        let image_len = image_meta.as_ref().map(|m| m.len()).unwrap_or(0);
        if st.pending() == 0 && st.bad_sectors == 0 && image_len >= st.total() && st.total() > 0 {
            let info = source::describe(plan.source)?;
            return Ok(ImagingDone {
                stats: *st,
                summary: None,
                info,
                image: plan.image.to_path_buf(),
                mapfile: map_path,
                already_complete: true,
            });
        }
    }

    // Open the source (refuses output on the source and mounted sources).
    let out_dir = parent_dir(plan.image);
    let mut req = SourceRequest::reading(plan.source);
    req.use_map = false;
    req.direct = true;
    req.allow_mounted = plan.allow_mounted;
    req.output = Some(&out_dir);
    req.interrupt = Some(interrupt);
    req.timeout = Some(Duration::from_secs(30));
    let opened = source::open(&req)?;
    super::print_warnings(&opened.warnings);
    let info = opened.info.clone();
    let profile = build_profile(info.kind, plan.tuning)?;
    let domain = build_domain(plan.start, plan.size, opened.shared.size())?;
    let domain_len = domain
        .map(|d| d.len())
        .unwrap_or_else(|| opened.shared.size());
    if let Some(dev) = &opened.device {
        dev.set_reattach_deadline(profile.wait_for_reattach);
    }

    // Room for the image.
    if plan.space_check {
        let have_on_disk = image_meta.as_ref().map(allocated_bytes).unwrap_or(0);
        let need = domain_len.saturating_sub(have_on_disk);
        if let Some(free) = util::free_space(&out_dir) {
            if free < need {
                return Err(CliError::refused(format!(
                    "there is not enough free space for the image. {} will hold {} but only {} is free \
                     on that disk. Choose a location with more room, for example an external drive: \
                     diskrescue image {} /path/on/another/disk/{}",
                    plan.image.display(),
                    human::bytes(need),
                    human::bytes(free),
                    shell_quote(&plan.source.display().to_string()),
                    plan.image
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "image.img".into())
                )));
            }
        }
    }

    // Resuming must continue on the same device.
    let id_path = identity_path(&map_path);
    let device_identity =
        (info.kind != DeviceKind::ImageFile).then(|| Fingerprint::of(&info, None));
    if prior.is_some() && !plan.force {
        if let Some(before) = read_identity(&id_path) {
            check_same_device(&before, &info, &map_path)?;
        }
    }

    if !out_dir.exists() {
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| CliError::failed(crate::error::explain_io(&e, Some(&out_dir))))?;
    }
    if prior.is_none() && plan.force && image_meta.as_ref().is_some_and(|m| m.len() > 0) {
        // Replacing a file: unreadable areas must read back as zeros, not as old data.
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(plan.image)
            .map_err(|e| CliError::failed(crate::error::explain_io(&e, Some(plan.image))))?;
    }

    let mut opts = ImagingOptions::with_profile(profile.clone());
    opts.domain = domain;
    opts.start_reverse = plan.tuning.reverse;
    opts.command_line = Some(plan.command_line.clone());
    opts.allow_existing_output = plan.force;
    opts.preallocate = domain.is_none() && opened.shared.size() >= (1 << 20);
    if plan.ui == UiMode::Tui {
        opts.map_snapshot_interval = Some(Duration::from_millis(250));
    }
    let mut imager = Imager::new(opened.shared.clone(), plan.image, &map_path, opts).map_err(|e| match e {
        diskrescue_core::Error::InvalidArgument(m) if m.contains("mapfile") => CliError::failed(format!(
            "{m}. The progress map seems to belong to a different device or image: use another image \
             name to start a new copy."
        )),
        other => CliError::from(other),
    })?;
    let resumed = imager.is_resumed();
    let before = imager.stats();
    if let Some(fp) = &device_identity {
        if !resumed || plan.force {
            write_identity(&id_path, fp);
        }
    }
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
            json!({
                "event": "start",
                "source": info.path,
                "image": plan.image,
                "mapfile": map_path,
                "profile": profile.name,
                "size": domain_len,
                "resumed": resumed,
                "command_line": plan.command_line,
            })
        );
    }

    let control = Control::new();
    interrupt.attach(control.clone());
    let (tx, rx) = mpsc::channel::<Event>();
    let engine_control = control.clone();
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
        Ok(Err(e)) => {
            return Err(CliError::failed(format!(
                "{}. Progress up to this point is saved in {}; run the same command again to continue.",
                crate::error::user_message(&CliError::from(e)),
                map_path.display()
            )))
        }
        Err(_) => {
            return Err(CliError::failed(format!(
                "the imaging engine stopped unexpectedly (this is a bug). Progress up to the last save is in {}; \
                 run the same command again to continue.",
                map_path.display()
            )))
        }
    };
    Ok(ImagingDone {
        stats: summary.stats,
        summary: Some(summary),
        info,
        image: plan.image.to_path_buf(),
        mapfile: map_path,
        already_complete: false,
    })
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

fn dir_text(reverse: bool) -> &'static str {
    if reverse {
        "backwards"
    } else {
        "forwards"
    }
}

/// Short phase name for progress lines.
pub fn phase_short(p: &Phase) -> String {
    match p {
        Phase::Copying { pass, reverse } => format!("copying pass {pass} ({})", dir_text(*reverse)),
        Phase::Trimming => "trimming failed areas".into(),
        Phase::Dividing => "dividing failed areas".into(),
        Phase::Scraping => "scraping failed areas".into(),
        Phase::Retrying { pass, reverse } => {
            format!("retrying pass {pass} ({})", dir_text(*reverse))
        }
        Phase::Finished => "finished".into(),
    }
}

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

/// Engine messages that only matter in a log file.
pub fn is_engine_chatter(msg: &str) -> bool {
    msg.starts_with("resuming: status")
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

/// JSON for a phase.
pub fn phase_json(p: &Phase) -> Value {
    match p {
        Phase::Copying { pass, reverse } => {
            json!({"name": "copying", "pass": pass, "reverse": reverse, "text": p.to_string()})
        }
        Phase::Retrying { pass, reverse } => {
            json!({"name": "retrying", "pass": pass, "reverse": reverse, "text": p.to_string()})
        }
        Phase::Trimming => json!({"name": "trimming", "text": p.to_string()}),
        Phase::Dividing => json!({"name": "dividing", "text": p.to_string()}),
        Phase::Scraping => json!({"name": "scraping", "text": p.to_string()}),
        Phase::Finished => json!({"name": "finished", "text": p.to_string()}),
    }
}

fn stats_json(s: &Stats) -> Value {
    json!({
        "rescued": s.rescued,
        "non_tried": s.non_tried,
        "non_trimmed": s.non_trimmed,
        "non_scraped": s.non_scraped,
        "bad": s.bad_sectors,
        "bad_areas": s.bad_areas,
        "total": s.total(),
        "percent_rescued": s.rescued_fraction() * 100.0,
    })
}

/// JSON for a summary.
pub fn summary_json(s: &Summary) -> Value {
    json!({
        "event": "finished",
        "finished": s.finished,
        "cancelled": s.cancelled,
        "elapsed_secs": s.elapsed.as_secs_f64(),
        "errors": s.errors,
        "timeouts": s.timeouts,
        "slow_reads": s.slow_reads,
        "reattaches": s.reattaches,
        "image": s.output,
        "mapfile": s.mapfile,
        "stats": stats_json(&s.stats),
        "warnings": s.warnings,
    })
}

/// JSON for an engine event (map snapshots are not reported).
pub fn event_json(ev: &Event) -> Option<Value> {
    Some(match ev {
        Event::Progress(p) => json!({
            "event": "progress",
            "phase": phase_json(&p.phase),
            "position": p.current_pos,
            "size": p.size,
            "stats": stats_json(&p.stats),
            "rate": p.current_rate,
            "average_rate": p.average_rate,
            "elapsed_secs": p.elapsed.as_secs_f64(),
            "eta_secs": p.eta.map(|d| d.as_secs_f64()),
            "errors": p.errors,
            "timeouts": p.timeouts,
            "slow_reads": p.slow_reads,
            "reattaches": p.reattaches,
            "last_error": p.last_error,
        }),
        Event::Phase(p) => json!({"event": "phase", "phase": phase_json(p)}),
        Event::Log(level, msg) => json!({
            "event": "log",
            "level": level.to_string().to_lowercase(),
            "message": msg,
        }),
        Event::DeviceLost { at } => json!({"event": "device_lost", "position": at}),
        Event::DeviceBack { after } => {
            json!({"event": "device_back", "after_secs": after.as_secs_f64()})
        }
        Event::Paused(d) => json!({"event": "paused", "secs": d.as_secs_f64()}),
        Event::Saved => json!({"event": "saved"}),
        Event::Finished(s) => summary_json(s),
        Event::Map(_) => return None,
    })
}

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

/// Run the command.
pub fn run(args: &ImageArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let command_line = util::command_line();
    let plan = ImagePlan {
        source: &args.source,
        image: &args.image,
        map: args.map.as_deref(),
        tuning: &args.tuning,
        start: args.start,
        size: args.size,
        force: args.force,
        allow_mounted: args.allow_mounted,
        space_check: !args.no_space_check,
        ui: UiMode::choose(args.no_tui, args.json_progress),
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
