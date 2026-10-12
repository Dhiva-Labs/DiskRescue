//! Progress display for the recovery pipeline (`recover`, `carve`, `scan`,
//! `repair`): progress bars on a terminal, plain lines in logs and pipes,
//! JSON lines for programs.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use diskrescue_core::util::human;
use diskrescue_recover::report::Outcome;
#[cfg(test)]
use diskrescue_recover::report::VolumeRecord;
use diskrescue_recover::{Event, Observer, Stage};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde_json::json;

pub use diskrescue_app::text::{
    stage_id, stage_label, stage_progress_label as progress_label, volume_line,
};

use crate::term;
use crate::util::percent;

static MULTI: OnceLock<MultiProgress> = OnceLock::new();

/// The shared progress-bar group (drawn on standard error; hidden when that
/// is not a terminal).
pub fn multi() -> &'static MultiProgress {
    MULTI.get_or_init(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10)))
}

/// How progress is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Bars on a terminal.
    Bars,
    /// One text line now and then.
    Plain,
    /// One JSON object per line on standard output.
    Json,
}

impl Mode {
    /// The mode to use given the user's `--json` choice and the terminal.
    pub fn choose(json: bool) -> Mode {
        if json {
            Mode::Json
        } else if term::stderr_is_tty() {
            Mode::Bars
        } else {
            Mode::Plain
        }
    }
}

#[derive(Default)]
struct Counts {
    recovered: AtomicU64,
    damaged: AtomicU64,
    repaired: AtomicU64,
    duplicates: AtomicU64,
    failed: AtomicU64,
}

struct State {
    bar: Option<ProgressBar>,
    stage: Option<Stage>,
    stage_started: Instant,
    last_emit: Instant,
    last_done: u64,
    last_total: u64,
    last_bytes: u64,
    last_carve_files: u64,
    bar_is_counter: bool,
    plain_done_shown: bool,
}

/// Observer that shows progress in the chosen [`Mode`].
pub struct ProgressUi {
    mode: Mode,
    state: Mutex<State>,
    counts: Counts,
    color: bool,
}

fn spinner_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {msg}"
    } else {
        "{spinner} {msg}"
    })
    .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn counter_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {msg}\n    [{bar:30.cyan/blue}] {pos}/{len}  {prefix}"
    } else {
        "{spinner} {msg}\n    [{bar:30}] {pos}/{len}  {prefix}"
    })
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=> ")
}

fn bytes_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {msg}\n    [{bar:30.cyan/blue}] {percent}%  {bytes}/{total_bytes}  {prefix}"
    } else {
        "{spinner} {msg}\n    [{bar:30}] {percent}%  {bytes}/{total_bytes}  {prefix}"
    })
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=> ")
}

impl ProgressUi {
    /// Create an observer for `mode`.
    pub fn new(mode: Mode) -> Self {
        ProgressUi {
            mode,
            state: Mutex::new(State {
                bar: None,
                stage: None,
                stage_started: Instant::now(),
                last_emit: Instant::now() - Duration::from_secs(60),
                last_done: 0,
                last_total: 0,
                last_bytes: 0,
                last_carve_files: 0,
                bar_is_counter: false,
                plain_done_shown: false,
            }),
            counts: Counts::default(),
            color: term::err().enabled(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// Print a line without disturbing the bars.
    fn say(&self, line: &str) {
        match self.mode {
            Mode::Bars => {
                let _ = multi().println(line);
            }
            Mode::Plain => eprintln!("{line}"),
            Mode::Json => {}
        }
    }

    fn json_line(&self, v: serde_json::Value) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    /// One-line summary of a finished stage, if it is worth keeping.
    fn stage_summary(&self, st: &State) -> Option<String> {
        let took = st.stage_started.elapsed();
        let t = if took >= Duration::from_secs(2) {
            format!(" in {}", human::duration(took))
        } else {
            String::new()
        };
        match st.stage.as_ref()? {
            Stage::Extracting if st.last_total > 0 => Some(format!(
                "Copied {} ({}){t}",
                crate::util::plural(st.last_done, "file", "files"),
                human::bytes(st.last_bytes)
            )),
            Stage::Carving if st.last_total > 0 => Some(format!(
                "Searched {}, found {}{t}",
                human::bytes(st.last_done),
                crate::util::plural(st.last_carve_files, "file", "files")
            )),
            Stage::Validating if st.last_total > 0 => Some(format!(
                "Checked {}{t}",
                crate::util::plural(st.last_total, "file", "files")
            )),
            Stage::Repairing if st.last_total > 0 => Some(format!(
                "Tried to repair {}{t}",
                crate::util::plural(st.last_total, "file", "files")
            )),
            _ => None,
        }
    }

    fn start_stage(&self, stage: Stage) {
        let label = stage_label(&stage);
        let mut st = self.lock();
        let summary = self.stage_summary(&st);
        if let Some(b) = st.bar.take() {
            b.finish_and_clear();
            multi().remove(&b);
        }
        if let Some(s) = summary {
            self.say(&format!("  {s}"));
        }
        st.stage = Some(stage.clone());
        st.stage_started = Instant::now();
        st.last_done = 0;
        st.last_total = 0;
        st.last_bytes = 0;
        st.last_carve_files = 0;
        st.bar_is_counter = false;
        st.plain_done_shown = false;
        st.last_emit = Instant::now() - Duration::from_secs(60);
        match self.mode {
            Mode::Bars => {
                let bar = multi().add(ProgressBar::new_spinner());
                bar.set_style(spinner_style(self.color));
                bar.set_message(label);
                bar.enable_steady_tick(Duration::from_millis(120));
                st.bar = Some(bar);
            }
            Mode::Plain => eprintln!("{label}..."),
            Mode::Json => self.json_line(json!({
                "event": "stage",
                "stage": stage_id(&stage),
                "label": label,
            })),
        }
    }

    fn counter_progress(&self, done: u64, total: u64, bytes: u64, bytes_bar: bool) {
        let mut st = self.lock();
        // Worker threads report in any order: never let the display go backwards.
        let (done, bytes) = if total == st.last_total {
            (done.max(st.last_done), bytes.max(st.last_bytes))
        } else {
            (done, bytes)
        };
        st.last_done = done;
        st.last_total = total;
        st.last_bytes = bytes;
        match self.mode {
            Mode::Bars => {
                if total == 0 {
                    return;
                }
                let label = st.stage.as_ref().map(progress_label).unwrap_or_default();
                let color = self.color;
                if let Some(b) = st.bar.clone() {
                    if !st.bar_is_counter {
                        b.set_style(if bytes_bar {
                            bytes_style(color)
                        } else {
                            counter_style(color)
                        });
                        b.set_message(label);
                    }
                    b.set_length(total);
                    b.set_position(done.min(total));
                    if bytes > 0 && !bytes_bar {
                        b.set_prefix(format!("{} so far", human::bytes(bytes)));
                    }
                }
                st.bar_is_counter = true;
            }
            Mode::Plain => {
                let complete = total > 0 && done >= total;
                if (complete && !st.plain_done_shown)
                    || (!complete && st.last_emit.elapsed() >= Duration::from_secs(5))
                {
                    st.last_emit = Instant::now();
                    st.plain_done_shown = complete;
                    let label = st.stage.as_ref().map(progress_label).unwrap_or_default();
                    if total > 0 {
                        let (d, t) = if bytes_bar {
                            (human::bytes(done), human::bytes(total))
                        } else {
                            (human::count(done), human::count(total))
                        };
                        eprintln!("  {label}: {d} of {t} ({})", percent(done, total));
                    } else {
                        eprintln!("  {label}: {}", human::count(done));
                    }
                }
            }
            Mode::Json => {
                if st.last_emit.elapsed() >= Duration::from_millis(200)
                    || (total > 0 && done >= total)
                {
                    st.last_emit = Instant::now();
                    let stage = st.stage.as_ref().map(stage_id).unwrap_or("");
                    drop(st);
                    self.json_line(json!({
                        "event": "progress",
                        "stage": stage,
                        "done": done,
                        "total": total,
                        "bytes": bytes,
                    }));
                }
            }
        }
    }

    fn carve_progress(&self, scanned: u64, total: u64, files: u64) {
        let mut st = self.lock();
        st.last_carve_files = files;
        drop(st);
        match self.mode {
            Mode::Json => {
                let mut st = self.lock();
                st.last_done = scanned;
                st.last_total = total;
                if st.last_emit.elapsed() >= Duration::from_millis(200) || scanned >= total {
                    st.last_emit = Instant::now();
                    drop(st);
                    self.json_line(json!({
                        "event": "carve",
                        "scanned": scanned,
                        "total": total,
                        "files": files,
                    }));
                }
            }
            _ => {
                self.counter_progress(scanned, total, 0, true);
                if self.mode == Mode::Bars {
                    if let Some(b) = self.lock().bar.clone() {
                        b.set_prefix(format!(
                            "{} found",
                            crate::util::plural(files, "file", "files")
                        ));
                    }
                }
            }
        }
    }

    /// Remove the bar and print the summary of the last stage.
    pub fn finish(&self) {
        let mut st = self.lock();
        let summary = self.stage_summary(&st);
        if let Some(b) = st.bar.take() {
            b.finish_and_clear();
            multi().remove(&b);
        }
        st.stage = None;
        drop(st);
        if let Some(s) = summary {
            self.say(&format!("  {s}"));
        }
    }

    /// Outcome counters seen so far: recovered, damaged, repaired, duplicates, failed.
    #[cfg(test)]
    pub fn counts(&self) -> (u64, u64, u64, u64, u64) {
        let c = &self.counts;
        (
            c.recovered.load(Ordering::Relaxed),
            c.damaged.load(Ordering::Relaxed),
            c.repaired.load(Ordering::Relaxed),
            c.duplicates.load(Ordering::Relaxed),
            c.failed.load(Ordering::Relaxed),
        )
    }
}

impl Observer for ProgressUi {
    fn event(&self, e: Event) {
        match e {
            Event::Stage(s) => self.start_stage(s),
            Event::Volume(v) => match self.mode {
                Mode::Json => self.json_line(json!({"event": "volume", "volume": v})),
                _ => self.say(&volume_line(&v)),
            },
            Event::Progress { done, total, bytes } => {
                let in_partitions = matches!(self.lock().stage, Some(Stage::Partitions));
                self.counter_progress(done, total, bytes, in_partitions);
            }
            Event::Carve {
                scanned,
                total,
                files,
            } => self.carve_progress(scanned, total, files),
            Event::File { name, outcome } => {
                let c = &self.counts;
                match outcome {
                    Outcome::Recovered | Outcome::RecoveredUnverified => {
                        c.recovered.fetch_add(1, Ordering::Relaxed)
                    }
                    Outcome::Damaged => c.damaged.fetch_add(1, Ordering::Relaxed),
                    Outcome::Repaired | Outcome::PartiallyRepaired => {
                        c.repaired.fetch_add(1, Ordering::Relaxed)
                    }
                    Outcome::Duplicate => c.duplicates.fetch_add(1, Ordering::Relaxed),
                    Outcome::Failed => c.failed.fetch_add(1, Ordering::Relaxed),
                };
                if self.mode == Mode::Json {
                    self.json_line(json!({"event": "file", "name": name, "outcome": outcome}));
                } else {
                    log::debug!("{}: {}", outcome.label(), name);
                }
            }
            Event::Warning(w) => match self.mode {
                Mode::Json => self.json_line(json!({"event": "warning", "message": w})),
                _ => self.say(&format!("{} {w}", term::err().yellow("warning:"))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_outcomes_and_survives_odd_event_order() {
        let ui = ProgressUi::new(Mode::Plain);
        ui.event(Event::Progress {
            done: 1,
            total: 0,
            bytes: 0,
        });
        ui.event(Event::Stage(Stage::Extracting));
        ui.event(Event::Progress {
            done: 2,
            total: 4,
            bytes: 100,
        });
        ui.event(Event::File {
            name: "a".into(),
            outcome: Outcome::Recovered,
        });
        ui.event(Event::File {
            name: "b".into(),
            outcome: Outcome::Damaged,
        });
        ui.event(Event::Carve {
            scanned: 5,
            total: 10,
            files: 1,
        });
        ui.event(Event::Warning("careful".into()));
        ui.finish();
        assert_eq!(ui.counts(), (1, 1, 0, 0, 0));
    }

    #[test]
    fn progress_never_goes_backwards() {
        let ui = ProgressUi::new(Mode::Plain);
        ui.event(Event::Stage(Stage::Extracting));
        ui.event(Event::Progress {
            done: 8,
            total: 11,
            bytes: 800,
        });
        ui.event(Event::Progress {
            done: 6,
            total: 11,
            bytes: 600,
        });
        let st = ui.lock();
        assert_eq!((st.last_done, st.last_bytes), (8, 800));
    }

    #[test]
    fn stage_labels_are_plain_language() {
        assert!(stage_label(&Stage::Carving).contains("photos"));
        assert!(stage_label(&Stage::Partitions).contains("partition"));
    }

    #[test]
    fn volume_line_mentions_problem() {
        let v = VolumeRecord {
            index: 2,
            start: 1 << 20,
            size: 10 << 20,
            fs: None,
            label: None,
            origin: "scan".into(),
            files_listed: 0,
            deleted_listed: 0,
            warnings: vec![],
            error: Some("no filesystem recognised; its space will be carved".into()),
        };
        let s = volume_line(&v);
        assert!(
            s.contains("Volume 2") && s.contains("will be carved"),
            "{s}"
        );
    }
}
