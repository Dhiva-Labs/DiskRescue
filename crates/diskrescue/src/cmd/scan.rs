//! `diskrescue scan`: list what can be recovered, write nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use diskrescue_core::format::ExtentStatus;
use diskrescue_core::fs::EntryConfidence;
use diskrescue_core::partition::PartitionScheme;
use diskrescue_core::time::Timestamp;
use diskrescue_core::util::human;
use diskrescue_recover::pipeline::Survey;
use diskrescue_recover::CarvePolicy;

use crate::args::ScanArgs;
use crate::cmd::{carve_policy, print_warnings, selection};
use crate::error::{CliError, CmdResult};
use crate::progress::{self, Mode, ProgressUi};
use crate::signal::Interrupt;
use crate::source::{self, SourceRequest};
use crate::term::{self, Align, Table};
use crate::util::plural;

fn scheme_text(s: PartitionScheme) -> &'static str {
    match s {
        PartitionScheme::Mbr => "MBR",
        PartitionScheme::Gpt => "GPT",
        PartitionScheme::None => {
            "none (the filesystem starts at the beginning, like a camera card)"
        }
        PartitionScheme::Unknown => "none found",
    }
}

fn state_text(c: EntryConfidence, deleted: bool) -> &'static str {
    if !deleted {
        return "existing";
    }
    match c {
        EntryConfidence::Allocated => "existing",
        EntryConfidence::DeletedIntact => "deleted",
        EntryConfidence::DeletedGuessed => "deleted, guessed",
        EntryConfidence::DeletedNoData => "deleted, data gone",
    }
}

fn extent_text(s: ExtentStatus) -> &'static str {
    match s {
        ExtentStatus::Complete => "complete",
        ExtentStatus::Truncated => "cut short",
        ExtentStatus::Capped => "very large",
        ExtentStatus::UnknownLength => "length guessed",
    }
}

fn date_text(t: Option<Timestamp>) -> String {
    match t {
        Some(ts) => {
            let (y, mo, d, h, mi, _) = ts.civil();
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
        }
        None => "-".into(),
    }
}

/// Render a survey as text. `limit == 0` shows every file.
pub fn render(survey: &Survey, limit: usize, carve_requested: bool, width: usize) -> String {
    let p = term::out();
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n",
        p.bold("Partitioning:"),
        scheme_text(survey.scheme)
    ));
    out.push('\n');
    if survey.volumes.is_empty() {
        out.push_str("No volumes were found.\n");
    } else {
        out.push_str(&p.bold("Volumes"));
        out.push('\n');
        for v in &survey.volumes {
            let fs =
                v.fs.map(|f| f.name().to_string())
                    .unwrap_or_else(|| "no filesystem recognised".into());
            let label = v
                .label
                .as_deref()
                .map(|l| format!(" \"{l}\""))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}. {fs}{label}, {} at offset {} (found via {}): {}, {} deleted\n",
                v.index,
                human::bytes(v.size),
                human::bytes(v.start),
                v.origin,
                plural(v.files_listed, "file", "files"),
                v.deleted_listed
            ));
            if let Some(e) = &v.error {
                out.push_str(&format!("     {} {e}\n", p.yellow("note:")));
            }
            for w in &v.warnings {
                out.push_str(&format!("     {} {w}\n", p.dim("note:")));
            }
        }
    }
    out.push('\n');

    let files = &survey.files;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let deleted = files.iter().filter(|f| f.deleted).count();
    out.push_str(&format!(
        "{} {} ({}): {} existing, {} deleted\n",
        p.bold("Recoverable through the filesystem:"),
        plural(files.len() as u64, "file", "files"),
        human::bytes(total_bytes),
        files.len() - deleted,
        deleted
    ));
    if !files.is_empty() {
        let mut by_cat: BTreeMap<&str, u64> = BTreeMap::new();
        for f in files {
            *by_cat.entry(f.category.dir_name()).or_default() += 1;
        }
        let cats: Vec<String> = by_cat.iter().map(|(k, v)| format!("{k} {v}")).collect();
        out.push_str(&format!("  By type: {}\n\n", cats.join(", ")));
        let mut sorted: Vec<&_> = files.iter().collect();
        sorted.sort_by(|a, b| (a.volume, &a.path).cmp(&(b.volume, &b.path)));
        let shown = if limit == 0 {
            sorted.len()
        } else {
            limit.min(sorted.len())
        };
        let mut t = Table::new(&["VOL", "FILE", "SIZE", "STATE", "MODIFIED"])
            .align(0, Align::Right)
            .align(2, Align::Right)
            .flex(1)
            .indent(2);
        for f in sorted.iter().take(shown) {
            t.row(vec![
                f.volume.to_string(),
                f.path.clone(),
                human::bytes(f.size),
                state_text(f.confidence, f.deleted).to_string(),
                date_text(f.modified),
            ]);
        }
        out.push_str(&t.render(width, &p));
        if shown < sorted.len() {
            out.push_str(&format!(
                "  ... and {} more (use --limit 0 to list them all, or --json)\n",
                sorted.len() - shown
            ));
        }
    }
    out.push('\n');

    if carve_requested {
        let carved = &survey.carved;
        let bytes: u64 = carved.iter().map(|c| c.len).sum();
        out.push_str(&format!(
            "{} {} ({})\n",
            p.bold("Found by searching the raw data:"),
            plural(carved.len() as u64, "file", "files"),
            human::bytes(bytes)
        ));
        if !carved.is_empty() {
            let mut by_fmt: BTreeMap<&str, u64> = BTreeMap::new();
            for c in carved {
                *by_fmt.entry(c.format.as_str()).or_default() += 1;
            }
            let fmts: Vec<String> = by_fmt.iter().map(|(k, v)| format!("{k} {v}")).collect();
            out.push_str(&format!("  By format: {}\n\n", fmts.join(", ")));
            let shown = if limit == 0 {
                carved.len()
            } else {
                limit.min(carved.len())
            };
            let mut t = Table::new(&["OFFSET", "SIZE", "FORMAT", "STATE", "DATE"])
                .align(0, Align::Right)
                .align(1, Align::Right)
                .indent(2);
            for c in carved.iter().take(shown) {
                t.row(vec![
                    format!("{:#x}", c.offset),
                    human::bytes(c.len),
                    c.format.clone(),
                    extent_text(c.status).to_string(),
                    date_text(c.created),
                ]);
            }
            out.push_str(&t.render(width, &p));
            if shown < carved.len() {
                out.push_str(&format!(
                    "  ... and {} more (use --limit 0 to list them all, or --json)\n",
                    carved.len() - shown
                ));
            }
        }
        out.push('\n');
    } else {
        out.push_str("The raw data was not searched. Add --carve to also look for files the filesystem no longer lists (slower).\n\n");
    }
    for w in &survey.warnings {
        out.push_str(&format!("{} {w}\n", p.yellow("warning:")));
    }
    out
}

/// Run the command.
pub fn run(args: &ScanArgs, interrupt: &Arc<Interrupt>) -> CmdResult {
    let mut req = SourceRequest::reading(&args.source);
    req.map = args.map.as_deref();
    req.interrupt = Some(interrupt);
    let opened = source::open(&req)?;
    print_warnings(&opened.warnings);
    let sel = selection(&args.categories, &args.state);
    let policy = args.carve.map(carve_policy).unwrap_or(CarvePolicy::Off);
    let ui = ProgressUi::new(Mode::choose(false));
    if !args.json {
        eprintln!(
            "Scanning {} ({}). Nothing is written.",
            opened.info.path.display(),
            human::bytes(opened.info.size)
        );
    }
    let result =
        diskrescue_recover::survey(opened.shared.clone(), &sel, policy, interrupt.flag(), &ui);
    ui.finish();
    let _ = progress::multi().clear();
    let survey = result.map_err(CliError::from)?;
    if args.json {
        let text = serde_json::to_string_pretty(&survey)
            .map_err(|e| CliError::failed(format!("cannot format the result: {e}")))?;
        println!("{text}");
    } else {
        print!(
            "{}",
            render(
                &survey,
                args.limit,
                policy != CarvePolicy::Off,
                term::width()
            )
        );
        if survey.files.is_empty() && survey.carved.is_empty() {
            println!("Nothing to recover was found with these settings.");
            println!("Try: diskrescue scan <source> --carve   (search the raw data too)");
            println!("     diskrescue scan <source> --all-files   (not just photos and videos)");
        } else {
            println!("Next: diskrescue recover <source> -o <folder for the recovered files>");
        }
    }
    if interrupt.is_set() {
        return Ok(crate::error::exit::INTERRUPTED);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::media::MediaCategory;
    use diskrescue_recover::pipeline::{SurveyCarved, SurveyFile};
    use diskrescue_recover::report::VolumeRecord;

    fn survey() -> Survey {
        Survey {
            scheme: PartitionScheme::None,
            volumes: vec![VolumeRecord {
                index: 1,
                start: 0,
                size: 272 << 20,
                fs: Some(diskrescue_core::fs::FsKind::Fat32),
                label: Some("EOS_DIGITAL".into()),
                origin: "mbr".into(),
                files_listed: 14,
                deleted_listed: 2,
                warnings: vec![],
                error: None,
            }],
            files: (0..5)
                .map(|i| SurveyFile {
                    volume: 1,
                    path: format!("DCIM/100CANON/IMG_{i:04}.JPG"),
                    size: 100_000 + i,
                    deleted: i == 3,
                    confidence: if i == 3 {
                        EntryConfidence::DeletedGuessed
                    } else {
                        EntryConfidence::Allocated
                    },
                    category: MediaCategory::Photo,
                    modified: Some(Timestamp::from_unix(1_720_000_000)),
                })
                .collect(),
            carved: vec![SurveyCarved {
                offset: 0x2c41000,
                len: 4096,
                format: "jpeg".into(),
                category: MediaCategory::Photo,
                status: ExtentStatus::Truncated,
                created: None,
            }],
            warnings: vec!["something odd".into()],
        }
    }

    #[test]
    fn text_lists_volumes_files_and_counts() {
        let s = render(&survey(), 50, true, 120);
        for needle in [
            "Partitioning: none",
            "fat32 \"EOS_DIGITAL\"",
            "14 files, 2 deleted",
            "5 files",
            "4 existing, 1 deleted",
            "photos 5",
            "IMG_0003.JPG",
            "deleted, guessed",
            "2024-07-03",
            "Found by searching the raw data: 1 file",
            "0x2c41000",
            "cut short",
            "something odd",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    #[test]
    fn limit_cuts_the_table_and_zero_means_all() {
        let s = render(&survey(), 2, false, 120);
        assert!(s.contains("... and 3 more"), "{s}");
        assert!(s.contains("raw data was not searched"), "{s}");
        assert!(!s.contains("IMG_0004"), "{s}");
        let s = render(&survey(), 0, false, 120);
        assert!(s.contains("IMG_0004") && !s.contains("more (use"), "{s}");
    }
}
