//! `diskrescue repair`: repair damaged photos and videos.

use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
use diskrescue_core::format::ValidationStatus;
use diskrescue_recover::report::{FileRecord, Origin, Outcome};
use diskrescue_recover::Report;

use crate::args::RepairArgs;
use crate::error::{CliError, CmdResult};
use crate::progress::{Mode, ProgressUi};
use crate::signal::Interrupt;
use crate::term::{self, Paint, Table};
use crate::util::plural;

fn before_text(f: &FileRecord) -> String {
    diskrescue_app::text::validation_label(f.validation.as_ref().map(|v| v.status)).to_string()
}

/// One-line result for a file.
pub fn result_text(f: &FileRecord) -> &'static str {
    diskrescue_app::text::repair_result_label(f.outcome)
}

/// True when the file ended up usable (intact or repaired at least partly).
pub fn is_ok(f: &FileRecord) -> bool {
    diskrescue_app::results::is_usable(f.outcome)
}

fn display_name(f: &FileRecord) -> String {
    match &f.origin {
        Origin::Input { path } => path.display().to_string(),
        other => format!("{other:?}"),
    }
}

/// Render the report of a repair run.
pub fn render(report: &Report, output: &Path, width: usize, p: &Paint) -> String {
    let mut t = Table::new(&["FILE", "BEFORE", "RESULT", "REPAIRED COPY"])
        .flex(0)
        .flex(3);
    for f in &report.files {
        let res = result_text(f);
        let colored = match f.outcome {
            Outcome::Repaired => p.green(res),
            Outcome::PartiallyRepaired => p.yellow(res),
            Outcome::Damaged | Outcome::Failed => p.bold_red(res),
            _ => res.to_string(),
        };
        let out = f
            .repair
            .as_ref()
            .and_then(|r| r.output.as_ref())
            .map(|o| output.join(o).display().to_string())
            .unwrap_or_else(|| "-".into());
        t.row(vec![display_name(f), before_text(f), colored, out]);
    }
    let mut s = t.render(width, p);
    let mut first = true;
    for f in &report.files {
        let mut lines: Vec<String> = Vec::new();
        if let Some(v) = &f.validation {
            for pr in v.problems.iter().take(3) {
                lines.push(format!("problem: {pr}"));
            }
        }
        if let Some(r) = &f.repair {
            if !r.applied.is_empty() {
                lines.push(format!("done: {}", r.applied.join(", ")));
            }
            if let Some(d) = &r.donor {
                lines.push(format!("used {} as the reference", d.display()));
            }
            for n in &r.notes {
                lines.push(format!("note: {n}"));
            }
        }
        for n in &f.notes {
            lines.push(format!("note: {n}"));
        }
        if lines.is_empty() {
            continue;
        }
        if first {
            s.push('\n');
            first = false;
        }
        s.push_str(&format!("{}\n", p.bold(&display_name(f))));
        for l in lines {
            s.push_str(&format!("  - {l}\n"));
        }
    }
    s
}

/// Run the command.
pub fn run(args: &RepairArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let inputs = diskrescue_app::recovery::check_repair(&args.files, &args.output, &args.donor)?;
    let _ = interrupt;
    let ui = ProgressUi::new(Mode::choose(false));
    let result = diskrescue_app::recovery::run_repair(
        &inputs,
        &args.output,
        &args.donor,
        args.aggressive,
        &ui,
    );
    ui.finish();
    let report = result?;
    let failed = report.files.iter().filter(|f| !is_ok(f)).count();
    if args.json {
        let text = serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::failed(format!("cannot format the report: {e}")))?;
        println!("{text}");
        return Ok(if failed == 0 { 0 } else { 1 });
    }
    print!(
        "{}",
        render(&report, &args.output, term::width(), &term::out())
    );
    println!();
    let repaired = report
        .files
        .iter()
        .filter(|f| matches!(f.outcome, Outcome::Repaired | Outcome::PartiallyRepaired))
        .count();
    if repaired > 0 {
        println!(
            "{} written to {}. Your original files were not changed.",
            plural(repaired as u64, "repaired copy was", "repaired copies were"),
            args.output.join("repaired").display()
        );
        println!("Open them to check the result before you delete anything.");
    }
    if failed > 0 {
        println!(
            "{} could not be repaired.",
            plural(failed as u64, "file", "files")
        );
        if args.donor.is_empty() {
            println!(
                "A healthy photo or video recorded by the same camera or phone with the same settings \
                 often makes the difference: add --donor <healthy file>."
            );
        }
    }
    println!("Report: {}", args.output.join("report.txt").display());
    Ok(if failed == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::RepairResult;
    use diskrescue_core::media::MediaCategory;
    use diskrescue_recover::report::{RepairRecord, ValidationRecord};
    use std::path::PathBuf;

    fn rec(name: &str, outcome: Outcome) -> FileRecord {
        let mut f = FileRecord::new(
            Origin::Input {
                path: PathBuf::from(name),
            },
            MediaCategory::Photo,
            1000,
        );
        f.outcome = outcome;
        f.validation = Some(ValidationRecord {
            status: if outcome == Outcome::Recovered {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Damaged
            },
            intact_fraction: Some(0.5),
            problems: vec!["image data ends early".into()],
        });
        f
    }

    #[test]
    fn outcomes_in_plain_words() {
        assert_eq!(result_text(&rec("a", Outcome::Repaired)), "repaired");
        assert_eq!(
            result_text(&rec("a", Outcome::Recovered)),
            "no repair needed"
        );
        assert!(is_ok(&rec("a", Outcome::PartiallyRepaired)));
        assert!(!is_ok(&rec("a", Outcome::Damaged)));
        assert!(!is_ok(&rec("a", Outcome::Failed)));
    }

    #[test]
    fn report_text_has_table_and_details() {
        let mut report = Report::new(None);
        let mut f = rec("IMG_1.JPG", Outcome::Repaired);
        f.repair = Some(RepairRecord {
            result: RepairResult::Repaired,
            applied: vec!["appended EOI".into()],
            notes: vec!["colours may differ".into()],
            donor: Some(PathBuf::from("good.jpg")),
            output: Some(PathBuf::from("repaired/IMG_1.JPG")),
        });
        report.add_file(f);
        report.add_file(rec("IMG_2.JPG", Outcome::Damaged));
        let s = render(&report, Path::new("fixed"), 120, &Paint::plain());
        for needle in [
            "FILE",
            "IMG_1.JPG",
            "repaired",
            "fixed/repaired/IMG_1.JPG",
            "appended EOI",
            "good.jpg",
            "colours may differ",
            "could not be repaired",
            "image data ends early",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }
}
