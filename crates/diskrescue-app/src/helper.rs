//! Imaging with administrator rights: the copy runs in the `diskrescue`
//! command-line program, started through `pkexec` on Linux, while this
//! process (the user interface) keeps its normal rights.
//!
//! The command runs as `diskrescue image SOURCE IMAGE ... --no-tui
//! --json-progress --stop-on-stdin-close`. Its JSON lines (see
//! [`crate::json`]) fill an ordinary [`ImageState`], so a front-end shows a
//! privileged copy exactly like one it runs itself; the block map is read
//! from the progress map every few seconds. Stopping closes the command's
//! standard input (it then saves its progress map and exits with code 130)
//! and also sends it SIGINT when that is permitted (it is not once pkexec
//! has switched to root, which is why the input is closed too).
//!
//! Files the command creates as root are given back to the user by the
//! command itself (pkexec sets `PKEXEC_UID`).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diskrescue_core::device::DeviceKind;
use diskrescue_imaging::MapFile;

use crate::error::{is_permission_error, AppError, Result};
use crate::imaging::build_profile;
use crate::jobs::{lock, spawn, Ctx, ImageState, ImageStatus, JobHandle, JobOutcome};
use crate::json::{parse_line, JsonLine};
use crate::plan::{ImagePlan, Tuning};
use crate::preflight::{preflight_image, probe_read_access, BlockerCode};
use crate::text::percent;

/// How often the block map is read from the progress map.
pub const MAP_POLL: Duration = Duration::from_secs(3);

/// The `diskrescue` program: next to the current executable, else on
/// `PATH`. Always an absolute path.
pub fn cli_path() -> Option<PathBuf> {
    let name = format!("diskrescue{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(&name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    find_on_path(&name)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// True when the current user cannot open `path` for reading (a device
/// node that needs administrator rights). The node is opened read-only and
/// closed again; nothing is read.
pub fn needs_privilege(path: &Path) -> bool {
    matches!(probe_read_access(path), Err(e) if is_permission_error(&e))
}

/// How the command-line program is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// With the rights of this process.
    None,
    /// Through `pkexec` (Linux): the user is asked for an administrator
    /// password by the desktop's polkit agent.
    Pkexec,
}

/// Image a device that needs administrator rights (Linux: through
/// `pkexec`). The job checks the plan first (ignoring "permission denied",
/// which is the point) and is refused when anything else blocks it.
///
/// Fails at once when the `diskrescue` program or `pkexec` cannot be
/// found, and on systems other than Linux (run the whole application as
/// administrator there).
pub fn start_image_privileged(plan: ImagePlan) -> Result<JobHandle<ImageState>> {
    #[cfg(target_os = "linux")]
    {
        let cli = cli_path().ok_or_else(|| {
            AppError::failed(
                "the diskrescue command-line program was not found next to this application or on \
                 PATH; it is needed to read devices with administrator rights",
            )
        })?;
        if find_on_path("pkexec").is_none() {
            return Err(AppError::failed(
                "pkexec (polkit) is not installed, so administrator rights cannot be asked for. Run \
                 the copy from a terminal instead: sudo diskrescue image <device> <image file>",
            ));
        }
        Ok(start_image_via_cli(plan, cli, Elevation::Pkexec))
    }
    #[cfg(windows)]
    {
        let _ = plan;
        Err(AppError::failed(
            "reading a disk needs administrator rights: close DiskRescue and start it again with \
             right-click, \"Run as administrator\"",
        ))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = plan;
        Err(AppError::failed(
            "reading a disk needs administrator rights: start DiskRescue again as an administrator",
        ))
    }
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// What the program printed on standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StderrInfo {
    /// The reason it refused to start, if it did.
    pub refused: Option<String>,
    /// Error messages.
    pub errors: Vec<String>,
}

/// Take one line of the program's standard output into account.
pub fn apply_stdout_line(state: &mut ImageState, line: &str) {
    match parse_line(line) {
        Some(JsonLine::Start(st)) => {
            state.image = st.image;
            state.mapfile = st.mapfile;
            state.copy_size = st.size;
            if state.resume_command.is_empty() {
                state.resume_command = st.command_line;
            }
            let tuning = Tuning {
                profile: Some(st.profile.clone()),
                ..Tuning::default()
            };
            if state.profile.as_ref().is_none_or(|p| p.name != st.profile) {
                state.profile = build_profile(DeviceKind::Unknown, &tuning).ok();
            }
            if st.resumed && state.resumed_from.is_none() {
                state.resumed_from = MapFile::load(&state.mapfile).ok().map(|m| m.map.stats());
            }
            state.status = ImageStatus::Copying;
        }
        Some(JsonLine::Event(ev)) => state.apply(&ev),
        None => {
            let t = line.trim();
            if !t.is_empty() {
                state.log(log::Level::Info, t.to_string());
            }
        }
    }
}

/// Take one line of the program's standard error into account.
pub fn apply_stderr_line(state: &mut ImageState, info: &mut StderrInfo, line: &str) {
    let t = line.trim();
    if t.is_empty() || t == "Nothing was written to your device." {
        return;
    }
    if let Some(m) = t.strip_prefix("Refused for safety:") {
        let m = m.trim().to_string();
        state.log(log::Level::Error, m.clone());
        info.refused = Some(m);
    } else if let Some(m) = t.strip_prefix("error:") {
        let m = m.trim().to_string();
        state.log(log::Level::Error, m.clone());
        info.errors.push(m);
    } else if let Some(m) = t.strip_prefix("warning:") {
        let m = m.trim().to_string();
        if !state.warnings.contains(&m) {
            state.warnings.push(m.clone());
        }
        state.log(log::Level::Warn, m);
    } else if t.starts_with("Run 'diskrescue --help'") {
    } else {
        state.log(log::Level::Info, t.to_string());
    }
}

/// The job outcome for the program's exit code.
pub fn outcome_for_exit(
    code: Option<i32>,
    state: &ImageState,
    info: &StderrInfo,
    elevation: Elevation,
    cancelled: bool,
) -> JobOutcome {
    let last_error = || {
        info.errors
            .last()
            .cloned()
            .unwrap_or_else(|| "the copy did not finish".to_string())
    };
    match code {
        Some(0) => JobOutcome::Finished,
        Some(130) => JobOutcome::Cancelled,
        Some(3) => JobOutcome::Refused(
            info.refused
                .clone()
                .unwrap_or_else(|| "refused for safety".to_string()),
        ),
        Some(126) | Some(127) if elevation == Elevation::Pkexec && state.progress.is_none() => {
            if cancelled {
                JobOutcome::Cancelled
            } else if code == Some(126) {
                JobOutcome::Failed(
                    "administrator rights were not granted, so the device could not be read. Nothing \
                     was written."
                        .into(),
                )
            } else {
                JobOutcome::Failed(
                    "administrator rights could not be asked for (is a polkit authentication agent \
                     running?). Nothing was written."
                        .into(),
                )
            }
        }
        Some(1) => match &state.summary {
            Some(s) if !s.finished && !s.cancelled => {
                let mut m = format!(
                    "The copy stopped before it was complete ({} copied). Start it again to continue \
                     where it left off.",
                    percent(s.stats.rescued, s.stats.total())
                );
                if let Some(w) = s.warnings.last() {
                    m.push(' ');
                    m.push_str(w);
                }
                JobOutcome::Failed(m)
            }
            _ => JobOutcome::Failed(last_error()),
        },
        Some(_) => JobOutcome::Failed(last_error()),
        None if cancelled => JobOutcome::Cancelled,
        None => JobOutcome::Failed(
            "the imaging program ended unexpectedly; progress up to its last save is in the \
             progress map, start the copy again to continue"
                .into(),
        ),
    }
}

/// Image `plan` by running the `diskrescue` program `cli`, optionally with
/// elevated rights. See the module documentation; [`start_image_privileged`]
/// is the usual entry point.
pub fn start_image_via_cli(
    plan: ImagePlan,
    cli: PathBuf,
    elevation: Elevation,
) -> JobHandle<ImageState> {
    // pkexec starts the program in root's home folder: every path must be absolute.
    let plan = ImagePlan {
        source: absolute(&plan.source),
        image: absolute(&plan.image),
        map: plan.map.as_deref().map(absolute),
        ..plan
    };
    let written_plan = plan.clone();
    let mut initial = ImageState::for_plan(&plan);
    initial.resume_command = {
        let mut p = plan.clone();
        p.command_line = None;
        if elevation == Elevation::Pkexec {
            format!("sudo {}", p.command_text())
        } else {
            p.command_text()
        }
    };
    spawn(
        "diskrescue-image-helper",
        false,
        initial,
        move || written_plan.written_paths(),
        move |ctx| {
            let outcome = run_cli(&plan, &cli, elevation, ctx);
            ctx.update(|s| s.status = ImageStatus::Done);
            outcome
        },
    )
}

fn run_cli(
    plan: &ImagePlan,
    cli: &Path,
    elevation: Elevation,
    ctx: &Ctx<ImageState>,
) -> JobOutcome {
    let pf = preflight_image(plan);
    ctx.update(|s| {
        if let Some(r) = &pf.resolved {
            s.source = Some(r.source.clone());
            s.profile = r.profile.clone();
            s.copy_size = r.copy_size;
        }
    });
    let blockers: Vec<&str> = pf
        .blockers
        .iter()
        .filter(|b| elevation == Elevation::None || b.code != BlockerCode::PermissionDenied)
        .map(|b| b.message.as_str())
        .collect();
    if !blockers.is_empty() {
        return JobOutcome::Refused(blockers.join("; "));
    }
    if ctx.shared.is_cancelled() {
        return JobOutcome::Cancelled;
    }

    let mut args = plan.cli_args();
    args.push("--no-tui".into());
    args.push("--json-progress".into());
    args.push("--stop-on-stdin-close".into());
    let mut cmd = match elevation {
        Elevation::None => Command::new(cli),
        Elevation::Pkexec => {
            let mut c = Command::new("pkexec");
            c.arg(cli);
            c
        }
    };
    cmd.args(&args)
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    ctx.update(|s| {
        s.status = ImageStatus::Opening;
        if elevation == Elevation::Pkexec {
            s.log(
                log::Level::Info,
                "Asking for administrator rights to read the device...",
            );
        }
    });
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return JobOutcome::Failed(format!(
                "cannot start {}: {e}",
                match elevation {
                    Elevation::None => cli.display().to_string(),
                    Elevation::Pkexec => "pkexec".to_string(),
                }
            ))
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let signalled = Arc::new(AtomicBool::new(false));
    {
        let child = child.clone();
        let signalled = signalled.clone();
        ctx.shared
            .on_cancel(Box::new(move || stop_child(&child, &signalled)));
    }

    let out_reader = stdout.map(|out| {
        let state = ctx.state.clone();
        std::thread::spawn(move || {
            for_each_line(out, |line| apply_stdout_line(&mut lock(&state), line))
        })
    });
    let err_info = Arc::new(Mutex::new(StderrInfo::default()));
    let err_reader = stderr.map(|err| {
        let state = ctx.state.clone();
        let info = err_info.clone();
        std::thread::spawn(move || {
            for_each_line(err, |line| {
                apply_stderr_line(&mut lock(&state), &mut lock(&info), line)
            })
        })
    });

    let mapfile = plan.mapfile();
    let mut last_map = Instant::now();
    let status = loop {
        let polled = lock(&child).try_wait();
        match polled {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if last_map.elapsed() >= MAP_POLL {
            last_map = Instant::now();
            reload_map(ctx, &mapfile);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    ctx.shared.clear_on_cancel();
    if let Some(h) = out_reader {
        let _ = h.join();
    }
    if let Some(h) = err_reader {
        let _ = h.join();
    }
    reload_map(ctx, &mapfile);
    let status = match status {
        Ok(s) => s,
        Err(e) => return JobOutcome::Failed(format!("lost track of the imaging program: {e}")),
    };
    let code = status.code();
    let info = lock(&err_info).clone();
    let state = lock(&ctx.state).clone();
    if code == Some(0) && state.summary.is_none() {
        // Exit 0 without a summary: the progress map said the image was
        // complete already.
        ctx.update(|s| {
            s.already_complete = true;
            s.final_stats = MapFile::load(&mapfile).ok().map(|m| m.map.stats());
        });
    }
    outcome_for_exit(code, &state, &info, elevation, ctx.shared.is_cancelled())
}

/// Read `r` to the end, one line at a time (invalid UTF-8 is replaced, so
/// the pipe is always drained and the program never blocks on it).
fn for_each_line(r: impl std::io::Read, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => f(String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r'])),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn reload_map(ctx: &Ctx<ImageState>, mapfile: &Path) {
    // The map is replaced atomically while it is saved; a failed read is
    // simply tried again next time.
    if let Ok(mf) = MapFile::load(mapfile) {
        let map = Arc::new(mf.map);
        ctx.update(|s| s.map = Some(map));
    }
}

/// Close the program's input (it then stops cleanly) and, once, send it
/// SIGINT where permitted. Never signals a process that has been reaped.
fn stop_child(child: &Mutex<Child>, signalled: &AtomicBool) {
    let mut c = lock(child);
    drop(c.stdin.take());
    if signalled.swap(true, Ordering::SeqCst) {
        return;
    }
    if !matches!(c.try_wait(), Ok(None)) {
        return;
    }
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(c.id()) {
            // SAFETY: kill(2) on the pid of our own child, which has not been
            // reaped (checked under the same lock that guards reaping).
            let rc = unsafe { libc::kill(pid, libc::SIGINT) };
            if rc != 0 {
                log::debug!(
                    "could not signal the imaging program: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_imaging::{Phase, Stats, Summary};

    #[test]
    fn stderr_lines_are_sorted_out() {
        let mut st = ImageState::default();
        let mut info = StderrInfo::default();
        for l in [
            "warning: /dev/sdb is mounted",
            "Refused for safety: the output is on the source",
            "Nothing was written to your device.",
            "error: something broke",
            "",
        ] {
            apply_stderr_line(&mut st, &mut info, l);
        }
        assert_eq!(st.warnings, vec!["/dev/sdb is mounted".to_string()]);
        assert_eq!(info.refused.as_deref(), Some("the output is on the source"));
        assert_eq!(info.errors, vec!["something broke".to_string()]);
        assert_eq!(st.logs.len(), 3);
    }

    #[test]
    fn exit_codes_become_outcomes() {
        let mut st = ImageState::default();
        let info = StderrInfo {
            refused: Some("no".into()),
            errors: vec!["bad".into()],
        };
        let e = Elevation::Pkexec;
        assert_eq!(
            outcome_for_exit(Some(0), &st, &info, e, false),
            JobOutcome::Finished
        );
        assert_eq!(
            outcome_for_exit(Some(130), &st, &info, e, true),
            JobOutcome::Cancelled
        );
        assert_eq!(
            outcome_for_exit(Some(3), &st, &info, e, false),
            JobOutcome::Refused("no".into())
        );
        assert!(matches!(
            outcome_for_exit(Some(126), &st, &info, e, false),
            JobOutcome::Failed(m) if m.contains("not granted")
        ));
        assert_eq!(
            outcome_for_exit(Some(2), &st, &info, e, false),
            JobOutcome::Failed("bad".into())
        );
        st.summary = Some(Summary {
            stats: Stats {
                rescued: 1,
                non_tried: 1,
                ..Stats::default()
            },
            elapsed: Duration::from_secs(1),
            errors: 0,
            finished: false,
            cancelled: false,
            reattaches: 0,
            timeouts: 0,
            slow_reads: 0,
            output: PathBuf::new(),
            mapfile: PathBuf::new(),
            warnings: vec![],
        });
        let none = StderrInfo::default();
        assert!(matches!(
            outcome_for_exit(Some(1), &st, &none, e, false),
            JobOutcome::Failed(m) if m.contains("50.0% copied")
        ));
        assert_eq!(
            outcome_for_exit(None, &st, &none, e, true),
            JobOutcome::Cancelled
        );
        st.apply(&diskrescue_imaging::Event::Phase(Phase::Trimming));
        assert_eq!(st.phase, Some(Phase::Trimming));
    }
}
