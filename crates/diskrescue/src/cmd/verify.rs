//! `diskrescue verify`: check whether files are intact.

#[cfg(test)]
use diskrescue_core::format::ValidationStatus;
use diskrescue_recover::pipeline::VerifyResult;

pub use diskrescue_app::results::verify_verdict as verdict;

use crate::args::VerifyArgs;
use crate::error::{CliError, CmdResult};
use crate::term::{self, Align, Paint, Table};
use crate::util::plural;

fn intact_text(r: &VerifyResult) -> String {
    match r.validation.as_ref().and_then(|v| v.intact_fraction) {
        Some(f) => format!("{:.0}%", (f * 100.0).clamp(0.0, 100.0)),
        None => "-".into(),
    }
}

/// Render the results as a table with the problems below.
pub fn render(results: &[VerifyResult], width: usize, p: &Paint) -> String {
    let mut t = Table::new(&["FILE", "FORMAT", "STATUS", "INTACT"])
        .align(3, Align::Right)
        .flex(0);
    for r in results {
        let (label, bad) = verdict(r);
        let colored = if bad {
            p.bold_red(label)
        } else {
            p.green(label)
        };
        t.row(vec![
            r.path.display().to_string(),
            r.format.clone().unwrap_or_else(|| "-".into()),
            colored,
            intact_text(r),
        ]);
    }
    let mut out = t.render(width, p);
    let mut any_problem = false;
    for r in results {
        let mut lines: Vec<String> = Vec::new();
        if let Some(v) = &r.validation {
            lines.extend(v.problems.iter().take(4).cloned());
            if v.problems.len() > 4 {
                lines.push(format!("... and {} more", v.problems.len() - 4));
            }
        }
        if let Some(e) = &r.error {
            lines.push(e.clone());
        }
        if !lines.is_empty() {
            if !any_problem {
                out.push('\n');
                any_problem = true;
            }
            out.push_str(&format!("{}\n", p.bold(&r.path.display().to_string())));
            for l in lines {
                out.push_str(&format!("  - {l}\n"));
            }
        }
    }
    out
}

/// Run the command.
pub fn run(args: &VerifyArgs) -> CmdResult {
    let results = diskrescue_app::recovery::run_verify(&args.files)?;
    let bad = results.iter().filter(|r| verdict(r).1).count();
    if args.json {
        let items: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "file": r.path,
                    "format": r.format,
                    "status": r.validation.as_ref().map(|v| v.status),
                    "ok": !verdict(r).1,
                    "intact_fraction": r.validation.as_ref().and_then(|v| v.intact_fraction),
                    "problems": r.validation.as_ref().map(|v| v.problems.clone()).unwrap_or_default(),
                    "error": r.error,
                    "meta": r.meta,
                })
            })
            .collect();
        let text = serde_json::to_string_pretty(&items)
            .map_err(|e| CliError::failed(format!("cannot format the result: {e}")))?;
        println!("{text}");
    } else {
        print!("{}", render(&results, term::width(), &term::out()));
        println!();
        if bad == 0 {
            println!(
                "All {} intact.",
                plural(results.len() as u64, "file is", "files are")
            );
        } else {
            println!(
                "{} of {} need attention.",
                bad,
                plural(results.len() as u64, "file", "files")
            );
            println!(
                "Damaged photos and videos can often be repaired: diskrescue repair <file> -o <folder>\n\
                 (for a video that will not open, add --donor <a healthy video from the same camera or phone>)"
            );
        }
    }
    Ok(if bad == 0 { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_recover::report::ValidationRecord;
    use std::path::PathBuf;

    fn result(status: Option<ValidationStatus>, error: Option<&str>) -> VerifyResult {
        VerifyResult {
            path: PathBuf::from("a.jpg"),
            format: status.map(|_| "jpeg".to_string()),
            validation: status.map(|s| ValidationRecord {
                status: s,
                intact_fraction: Some(0.55),
                problems: vec!["ends early".into()],
            }),
            meta: Default::default(),
            error: error.map(String::from),
        }
    }

    #[test]
    fn verdicts() {
        assert_eq!(
            verdict(&result(Some(ValidationStatus::Valid), None)),
            ("ok", false)
        );
        assert_eq!(
            verdict(&result(Some(ValidationStatus::Damaged), None)),
            ("DAMAGED", true)
        );
        assert_eq!(
            verdict(&result(Some(ValidationStatus::Invalid), None)),
            ("INVALID", true)
        );
        assert_eq!(
            verdict(&result(None, Some("not a photo"))),
            ("UNRECOGNISED", true)
        );
    }

    #[test]
    fn table_shows_problems() {
        let s = render(
            &[result(Some(ValidationStatus::Damaged), None)],
            100,
            &Paint::plain(),
        );
        assert!(
            s.contains("DAMAGED") && s.contains("55%") && s.contains("ends early"),
            "{s}"
        );
    }
}
