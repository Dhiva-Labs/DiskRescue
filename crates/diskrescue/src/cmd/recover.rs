//! `diskrescue recover`: the whole pipeline, optionally preceded by imaging.
//!
//! [`run_recovery`] is shared with `diskrescue carve`. The checks, opening
//! the source and the pipeline options are shared with the other front-ends
//! (`diskrescue_app::recovery`); this module prints.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use diskrescue_app::recovery::RecoveryJob as AppJob;
use diskrescue_app::results::has_files;
use diskrescue_core::device::DeviceKind;
use diskrescue_core::util::human;
use diskrescue_recover::report::Report;
use serde_json::json;

pub use diskrescue_app::recovery::keep_previous_report;

use crate::args::RecoverArgs;
use crate::cmd::image::{self, ImagePlan, UiMode};
use crate::cmd::print_warnings;
use crate::error::{exit, CmdResult};
use crate::progress::{Mode, ProgressUi};
use crate::signal::Interrupt;
use crate::term;
use crate::util::{self, plural};

/// A recovery job (`recover` and `carve`) and how to show it.
pub struct RecoveryJob {
    /// What to do (shared with the other front-ends).
    pub job: AppJob,
    /// Plain progress lines while imaging instead of the dashboard.
    pub no_tui: bool,
    /// JSON lines instead of human text.
    pub json: bool,
    /// What the user typed.
    pub command_line: String,
}

/// How many lines of the report to print.
const SUMMARY_LINES: usize = 40;

/// Run a recovery job end to end.
pub fn run_recovery(rj: &RecoveryJob, interrupt: &Arc<Interrupt>) -> CmdResult {
    let job = &rj.job;
    job.check()?;
    let paint = term::out();

    // Step 1 (optional): copy the device into an image.
    let mut source_path: PathBuf = job.source.clone();
    let mut map_path: Option<PathBuf> = job.map.clone();
    let mut origin = None;
    if let Some(step) = &job.image_to {
        // The folder for the results must be safe too (the image folder is checked while imaging).
        job.check_output_against_device()?;
        let plan = ImagePlan {
            source: &job.source,
            image: &step.image,
            map: job.map.as_deref(),
            tuning: &step.tuning,
            start: None,
            size: None,
            force: false,
            allow_mounted: job.allow_mounted,
            space_check: true,
            ui: UiMode::choose(rj.no_tui, rj.json),
            command_line: rj.command_line.clone(),
        };
        let done = image::run_imaging(&plan, interrupt)?;
        let code = image::print_summary(&done, &rj.command_line, rj.json, false);
        if !done.finished() {
            return Ok(code);
        }
        if !rj.json {
            println!();
            println!("{}", paint.bold("Now recovering files from the image."));
        }
        source_path = done.image.clone();
        map_path = Some(done.mapfile.clone());
        origin = Some(done.info.clone());
    }

    // Step 2: open and recover.
    let opened = job.open(&source_path, map_path.as_deref(), Some(interrupt.flag()))?;
    let reading_device = opened.info.kind != DeviceKind::ImageFile;
    let warnings = job.warnings(&opened);
    if rj.json {
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
    if let Some(note) = keep_previous_report(&job.output) {
        if rj.json {
            println!("{}", json!({"event": "warning", "message": note}));
        } else {
            print_warnings(&[note]);
        }
    }

    let opts = job.options(interrupt.flag());
    let record = opened.record(origin.as_ref());
    let ui = ProgressUi::new(Mode::choose(rj.json));
    let result = diskrescue_app::recovery::run_pipeline(opened.shared.clone(), record, &opts, &ui);
    ui.finish();
    let report = result?;

    let interrupted = report.interrupted || interrupt.is_set();
    if rj.json {
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
            finish_text(&report, &job.output, interrupted, &rj.command_line)
        );
    }
    Ok(if interrupted {
        exit::INTERRUPTED
    } else {
        exit::OK
    })
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

/// Run the command.
pub fn run(args: &RecoverArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let command_line = util::command_line();
    let plan = diskrescue_app::plan::RecoverPlan {
        source: args.source.clone(),
        output: args.output.clone(),
        categories: super::categories(&args.categories),
        state: super::state_filter(&args.state),
        carve: super::carve_mode(args.carve),
        repair: super::repair_options(&args.repair),
        jobs: args.jobs,
        image_to: args.image_to.clone(),
        map: args.map.clone(),
        allow_mounted: args.allow_mounted,
        tuning: super::tuning(&args.tuning),
        command_line: Some(command_line.clone()),
    };
    let rj = RecoveryJob {
        job: plan.job(),
        no_tui: args.no_tui,
        json: args.json,
        command_line,
    };
    run_recovery(&rj, interrupt)
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
