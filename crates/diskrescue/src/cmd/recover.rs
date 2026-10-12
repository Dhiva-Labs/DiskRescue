//! `diskrescue recover`: the whole pipeline, optionally preceded by imaging.
//!
//! [`run_recovery`] is shared with `diskrescue carve`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_core::device::DeviceKind;
use diskrescue_core::util::human;
use diskrescue_recover::report::Report;
use diskrescue_recover::{CarvePolicy, RecoverOptions, Selection};
use serde_json::json;

use crate::args::{RecoverArgs, RepairOpts, Tuning};
use crate::cmd::image::{self, ImagePlan, UiMode};
use crate::cmd::{carve_policy, check_donors, check_output_dir, print_warnings, selection};
use crate::error::{exit, CliError, CmdResult};
use crate::progress::{Mode, ProgressUi};
use crate::signal::Interrupt;
use crate::source::{self, SourceRequest};
use crate::term;
use crate::util::{self, plural};

/// The imaging step of `recover --image-to`.
pub struct ImageStep<'a> {
    /// Image file to create or resume.
    pub image: &'a Path,
    /// Strategy overrides.
    pub tuning: &'a Tuning,
    /// Plain progress lines instead of the dashboard.
    pub no_tui: bool,
}

/// A recovery job (`recover` and `carve`).
pub struct RecoveryJob<'a> {
    /// Device, partition or image file.
    pub source: &'a Path,
    /// Output folder.
    pub output: &'a Path,
    /// Mapfile of the image.
    pub map: Option<&'a Path>,
    /// What to recover.
    pub selection: Selection,
    /// Carving policy.
    pub carve: CarvePolicy,
    /// Read filesystems (false for `carve`).
    pub use_filesystems: bool,
    /// Repair options.
    pub repair: &'a RepairOpts,
    /// Worker threads.
    pub jobs: Option<usize>,
    /// Allow a mounted source.
    pub allow_mounted: bool,
    /// JSON lines instead of human text.
    pub json: bool,
    /// Image the device first.
    pub image_to: Option<ImageStep<'a>>,
    /// What the user typed.
    pub command_line: String,
}

/// How many lines of the report to print.
const SUMMARY_LINES: usize = 40;

/// Run a recovery job end to end.
pub fn run_recovery(job: &RecoveryJob<'_>, interrupt: &Arc<Interrupt>) -> CmdResult {
    check_output_dir(job.output)?;
    check_donors(&job.repair.donor)?;
    if same_path(job.source, job.output) {
        return Err(CliError::refused(format!(
            "the output location {} is the source itself. Choose a new folder for the recovered files, \
             preferably on another disk.",
            job.output.display()
        )));
    }
    let paint = term::out();

    // Step 1 (optional): copy the device into an image.
    let mut source_path: PathBuf = job.source.to_path_buf();
    let mut map_path: Option<PathBuf> = job.map.map(Path::to_path_buf);
    let mut origin = None;
    if let Some(step) = &job.image_to {
        let info = source::describe(job.source)?;
        // The folder for the results must be safe too (the image folder is checked while imaging).
        let check =
            diskrescue_recover::safety::check(job.source, &info, job.output, job.allow_mounted);
        check.into_result().map_err(CliError::from)?;
        let plan = ImagePlan {
            source: job.source,
            image: step.image,
            map: job.map,
            tuning: step.tuning,
            start: None,
            size: None,
            force: false,
            allow_mounted: job.allow_mounted,
            space_check: true,
            ui: UiMode::choose(step.no_tui, job.json),
            command_line: job.command_line.clone(),
        };
        let done = image::run_imaging(&plan, interrupt)?;
        let code = image::print_summary(&done, &job.command_line, job.json, false);
        if !done.finished() {
            return Ok(code);
        }
        if !job.json {
            println!();
            println!("{}", paint.bold("Now recovering files from the image."));
        }
        source_path = done.image.clone();
        map_path = Some(done.mapfile.clone());
        origin = Some(done.info.clone());
    }

    // Step 2: open and recover.
    let mut req = SourceRequest::reading(&source_path);
    req.map = map_path.as_deref();
    req.use_map = true;
    req.allow_mounted = job.allow_mounted;
    req.output = Some(job.output);
    req.interrupt = Some(interrupt);
    let opened = source::open(&req)?;
    let reading_device = opened.info.kind != DeviceKind::ImageFile;
    let mut warnings = opened.warnings.clone();
    if reading_device && job.image_to.is_none() {
        warnings.push(
            "Reading directly from the device. That is fine when the device is healthy (for example \
             after accidental deletion). If it shows read errors, slows down or disconnects, stop and \
             copy it to an image first with: diskrescue image <device> <image file>"
                .to_string(),
        );
    }
    if job.json {
        for w in &warnings {
            println!("{}", json!({"event": "warning", "message": w}));
        }
    } else {
        print_warnings(&warnings);
        println!(
            "{} {} into {}",
            paint.bold("Recovering from"),
            if reading_device {
                format!(
                    "{} ({})",
                    opened.info.path.display(),
                    diskrescue_device::device_label(&opened.info)
                )
            } else {
                format!(
                    "{} ({})",
                    opened.info.path.display(),
                    human::bytes(opened.info.size)
                )
            },
            job.output.display()
        );
        println!("  Press Ctrl-C to stop; what has been recovered so far is kept and reported.");
    }

    // A report from an earlier run in the same folder would be replaced: keep it.
    if let Some(note) = keep_previous_report(job.output) {
        if job.json {
            println!("{}", json!({"event": "warning", "message": note}));
        } else {
            print_warnings(&[note]);
        }
    }

    let mut opts = RecoverOptions::new(job.output);
    opts.selection = job.selection.clone();
    opts.carve = job.carve;
    opts.use_filesystems = job.use_filesystems;
    opts.repair = !job.repair.no_repair;
    opts.donors = job.repair.donor.clone();
    opts.aggressive_repair = job.repair.aggressive;
    opts.workers = job.jobs.unwrap_or(0);
    opts.cancel = interrupt.flag();
    opts.command_line = Some(job.command_line.clone());

    let record = opened.record(origin.as_ref());
    let ui = ProgressUi::new(Mode::choose(job.json));
    let result = diskrescue_recover::recover(opened.shared.clone(), record, &opts, &ui);
    ui.finish();
    let report = result.map_err(|e| match e {
        diskrescue_core::Error::Io(io) => {
            CliError::failed(crate::error::explain_io(&io, Some(job.output)))
        }
        other => CliError::from(other),
    })?;

    let interrupted = report.interrupted || interrupt.is_set();
    if job.json {
        println!(
            "{}",
            json!({
                "event": "finished",
                "interrupted": interrupted,
                "output": job.output,
                "report_json": job.output.join("report.json"),
                "report_txt": job.output.join("report.txt"),
                "totals": report.totals,
                "warnings": report.warnings,
            })
        );
    } else {
        println!();
        print!(
            "{}",
            finish_text(&report, job.output, interrupted, &job.command_line)
        );
    }
    Ok(if interrupted {
        exit::INTERRUPTED
    } else {
        exit::OK
    })
}

/// If `output` already holds a report, rename it to `report.previous.*` so
/// the new run does not replace it. Returns the note to show the user.
pub fn keep_previous_report(output: &Path) -> Option<String> {
    let json = output.join("report.json");
    let txt = output.join("report.txt");
    if !json.is_file() && !txt.is_file() {
        return None;
    }
    let mut kept = false;
    for (from, to) in [
        (json, output.join("report.previous.json")),
        (txt, output.join("report.previous.txt")),
    ] {
        if from.is_file() && std::fs::rename(&from, &to).is_ok() {
            kept = true;
        }
    }
    Some(if kept {
        format!(
            "{} already contains the report of an earlier run; it was kept as report.previous.txt and \
             report.previous.json. Files recovered before are never overwritten: new copies get numbered \
             names. Use a new folder to keep the runs apart.",
            output.display()
        )
    } else {
        format!(
            "{} already contains the report of an earlier run, which will be replaced. Use a new folder \
             to keep both.",
            output.display()
        )
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// What to print after a recovery: the start of the report and what to look at.
pub fn finish_text(
    report: &Report,
    output: &Path,
    interrupted: bool,
    command_line: &str,
) -> String {
    let paint = term::out();
    let mut out = String::new();
    let summary = report.text_summary();
    let lines: Vec<&str> = summary.lines().collect();
    for l in lines.iter().take(SUMMARY_LINES) {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > SUMMARY_LINES {
        out.push_str(&format!(
            "... {} more lines in {}\n",
            lines.len() - SUMMARY_LINES,
            output.join("report.txt").display()
        ));
    }
    out.push('\n');
    let t = &report.totals;
    if interrupted {
        out.push_str(&paint.yellow(&format!(
            "Stopped early. {} recovered so far are kept in {}.\n",
            plural(t.files_written, "file", "files"),
            output.display()
        )));
        out.push_str(
            "Run the same command again to start over; existing files are never overwritten:\n",
        );
        out.push_str(&format!("  {command_line}\n"));
    } else if t.files_written == 0 {
        out.push_str(&paint.yellow("No photos or videos were recovered.\n"));
        out.push_str(
            "What you can try:\n\
             \x20 - If the device was formatted or its filesystem is badly damaged: diskrescue carve <source> -o <folder>\n\
             \x20 - To include every kind of file: add --all-files\n\
             \x20 - If the image has unreadable areas, check how much was copied: diskrescue map <image>.map\n",
        );
    }
    out.push_str(&format!("Results are in {}\n", output.display()));
    let mut parts = Vec::new();
    for (dir, what) in [
        (
            "files",
            "files that still existed, with their original names",
        ),
        ("deleted", "deleted files found through the filesystem"),
        ("carved", "files found by searching the raw data"),
        (
            "repaired",
            "repaired copies of damaged files (the originals are kept)",
        ),
    ] {
        if has_files(&output.join(dir)) {
            parts.push(format!(
                "  {dir}/{}{what}",
                " ".repeat(9usize.saturating_sub(dir.len()))
            ));
        }
    }
    for p in parts {
        out.push_str(&p);
        out.push('\n');
    }
    out.push_str(&format!(
        "Full report: {} and {}\n",
        output.join("report.txt").display(),
        output.join("report.json").display()
    ));
    let attention = report
        .files
        .iter()
        .filter(|f| {
            matches!(
                f.outcome,
                diskrescue_recover::Outcome::Damaged
                    | diskrescue_recover::Outcome::PartiallyRepaired
                    | diskrescue_recover::Outcome::Failed
            )
        })
        .count();
    if attention > 0 {
        out.push_str(&format!(
            "Files needing attention: {attention} (damaged or only partly repaired). They are listed in report.txt.\n"
        ));
    }
    if t.deleted > 0 {
        out.push_str("Look in deleted/ first for photos you removed by mistake.\n");
    }
    out
}

/// True when `dir` holds at least one file (looking a few levels down).
fn has_files(dir: &Path) -> bool {
    fn walk(dir: &Path, depth: u32) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        for e in rd.flatten().take(10_000) {
            match e.file_type() {
                Ok(t) if t.is_file() => return true,
                Ok(t) if t.is_dir() && depth < 8 && walk(&e.path(), depth + 1) => return true,
                _ => {}
            }
        }
        false
    }
    walk(dir, 0)
}

/// Run the command.
pub fn run(args: &RecoverArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let job = RecoveryJob {
        source: &args.source,
        output: &args.output,
        map: args.map.as_deref(),
        selection: selection(&args.categories, &args.state),
        carve: carve_policy(args.carve),
        use_filesystems: true,
        repair: &args.repair,
        jobs: args.jobs.filter(|j| *j > 0),
        allow_mounted: args.allow_mounted,
        json: args.json,
        image_to: args.image_to.as_deref().map(|image| ImageStep {
            image,
            tuning: &args.tuning,
            no_tui: args.no_tui,
        }),
        command_line: util::command_line(),
    };
    run_recovery(&job, interrupt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::media::MediaCategory;
    use diskrescue_recover::report::{FileRecord, Origin, Outcome};
    use diskrescue_recover::Report;

    fn report_with(outcomes: &[Outcome]) -> Report {
        let mut r = Report::new(Some("diskrescue recover x -o y".into()));
        for (i, o) in outcomes.iter().enumerate() {
            let mut f = FileRecord::new(
                Origin::Input {
                    path: PathBuf::from(format!("f{i}.jpg")),
                },
                MediaCategory::Photo,
                1000,
            );
            f.outcome = *o;
            r.add_file(f);
        }
        r.finalize();
        r
    }

    #[test]
    fn success_text_points_at_the_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("deleted")).unwrap();
        std::fs::write(dir.path().join("deleted/a.jpg"), b"x").unwrap();
        let mut r = report_with(&[Outcome::Recovered, Outcome::Damaged]);
        r.totals.deleted = 1;
        let s = finish_text(&r, dir.path(), false, "diskrescue recover x -o y");
        assert!(s.contains("Results are in"), "{s}");
        assert!(s.contains("deleted/"), "{s}");
        assert!(!s.contains("carved/"), "no empty folders listed: {s}");
        assert!(s.contains("report.txt") && s.contains("report.json"), "{s}");
        assert!(s.contains("Files needing attention: 1"), "{s}");
        assert!(s.contains("Look in deleted/ first"), "{s}");
    }

    #[test]
    fn an_earlier_report_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        assert!(keep_previous_report(dir.path()).is_none());
        std::fs::write(dir.path().join("report.json"), "{}").unwrap();
        std::fs::write(dir.path().join("report.txt"), "old").unwrap();
        let note = keep_previous_report(dir.path()).unwrap();
        assert!(note.contains("report.previous.txt"), "{note}");
        assert!(!dir.path().join("report.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("report.previous.txt")).unwrap(),
            "old"
        );
        assert!(dir.path().join("report.previous.json").is_file());
    }

    #[test]
    fn empty_result_gives_next_steps() {
        let dir = tempfile::tempdir().unwrap();
        let r = report_with(&[]);
        let s = finish_text(&r, dir.path(), false, "x");
        assert!(s.contains("No photos or videos were recovered"), "{s}");
        assert!(s.contains("diskrescue carve"), "{s}");
    }

    #[test]
    fn interrupted_text_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let r = report_with(&[Outcome::Recovered]);
        let s = finish_text(&r, dir.path(), true, "diskrescue recover x -o y");
        assert!(s.contains("Stopped early"), "{s}");
        assert!(s.contains("never overwritten"), "{s}");
    }

    #[test]
    fn long_reports_are_cut() {
        let dir = tempfile::tempdir().unwrap();
        let r = report_with(&[Outcome::Damaged; 100]);
        let s = finish_text(&r, dir.path(), false, "x");
        assert!(s.contains("more lines in"), "{s}");
    }
}
