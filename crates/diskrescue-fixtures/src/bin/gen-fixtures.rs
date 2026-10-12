//! `gen-fixtures <out-dir> [--set NAME]`: write the synthetic scenarios.
//!
//! Without `--set` (or with `--set all`) every scenario is built into its own
//! subdirectory of `<out-dir>`. Scenarios whose tools are missing are
//! skipped with a message; the exit status is non-zero if any scenario fails
//! for another reason.

use std::path::PathBuf;
use std::process::ExitCode;

use diskrescue_fixtures::raw;
use diskrescue_fixtures::sets::{self, SET_NAMES};

/// Name of the extra set holding the filesystem-less carving images.
const RAW_SET: &str = "carving-raw";

fn usage() -> ExitCode {
    eprintln!(
        "usage: gen-fixtures <out-dir> [--set NAME]\n\nsets: all, {}, {RAW_SET}",
        SET_NAMES.join(", ")
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out: Option<PathBuf> = None;
    let mut set = "all".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--set" => match args.next() {
                Some(v) => set = v,
                None => return usage(),
            },
            "-h" | "--help" => return usage(),
            _ if a.starts_with("--") => return usage(),
            _ => {
                if out.is_some() {
                    return usage();
                }
                out = Some(PathBuf::from(a));
            }
        }
    }
    let Some(out) = out else {
        return usage();
    };
    let names: Vec<&str> = if set == "all" {
        let mut v = SET_NAMES.to_vec();
        v.push(RAW_SET);
        v
    } else {
        vec![set.as_str()]
    };
    let mut failed = false;
    for name in names {
        let dir = out.join(name);
        // Start from a clean directory so stale files cannot leak into the truth.
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                eprintln!("{name}: cannot clean {}: {e}", dir.display());
                failed = true;
                continue;
            }
        }
        let built = if name == RAW_SET {
            raw::all(&dir, 1).map(|v| v.len())
        } else {
            sets::build_named(name, &dir).map(|sc| sc.files.len())
        };
        match built {
            Ok(n) => println!("{name}: {} ({n} entries of truth)", dir.display()),
            Err(e) if e.is_missing() => eprintln!("{name}: skipped ({e})"),
            Err(e) => {
                eprintln!("{name}: FAILED: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
