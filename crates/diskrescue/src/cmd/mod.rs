//! The subcommands.

pub mod carve;
pub mod image;
pub mod info;
pub mod list;
pub mod map;
pub mod recover;
pub mod repair;
pub mod scan;
pub mod verify;

use std::path::{Path, PathBuf};

use diskrescue_core::media::MediaCategory;
use diskrescue_recover::{CarvePolicy, Selection};

use crate::args::{CarveArg, CategoryArgs, StateArgs};
use crate::error::CliError;
use crate::term;

/// What the user asked to recover.
///
/// No category flag means photos, RAW and videos; `--all-files` means every
/// file; otherwise exactly the categories named.
pub fn selection(categories: &CategoryArgs, state: &StateArgs) -> Selection {
    let mut sel = if categories.all_files {
        Selection::everything()
    } else if categories.photos || categories.videos || categories.raw || categories.audio {
        let mut s = Selection::photos_and_videos();
        s.categories.clear();
        if categories.photos {
            s.categories.insert(MediaCategory::Photo);
        }
        if categories.raw {
            s.categories.insert(MediaCategory::RawPhoto);
        }
        if categories.videos {
            s.categories.insert(MediaCategory::Video);
        }
        if categories.audio {
            s.categories.insert(MediaCategory::Audio);
        }
        s
    } else {
        Selection::photos_and_videos()
    };
    if state.existing_only {
        sel.deleted = false;
    }
    if state.deleted_only {
        sel.existing = false;
    }
    if let Some(min) = categories.min_size {
        sel.min_size = min;
    }
    sel
}

/// The pipeline's carving policy for the `--carve` option.
pub fn carve_policy(arg: CarveArg) -> CarvePolicy {
    match arg {
        CarveArg::Auto => CarvePolicy::Auto,
        CarveArg::Free => CarvePolicy::FreeSpace,
        CarveArg::All => CarvePolicy::Everything,
        CarveArg::Off => CarvePolicy::Off,
    }
}

/// Print warnings to standard error.
pub fn print_warnings(warnings: &[String]) {
    let p = term::err();
    for w in warnings {
        eprintln!("{} {w}", p.yellow("warning:"));
    }
}

/// Check that every donor is an existing file.
pub fn check_donors(donors: &[PathBuf]) -> Result<(), CliError> {
    for d in donors {
        match std::fs::metadata(d) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(CliError::failed(format!(
                    "the donor {} is not a file. A donor is a healthy photo or video from the same \
                     camera or phone.",
                    d.display()
                )))
            }
            Err(_) => {
                return Err(CliError::failed(format!(
                "the donor {} does not exist. A donor is a healthy photo or video from the same \
                     camera or phone.",
                d.display()
            )))
            }
        }
    }
    Ok(())
}

/// Refuse an output location that cannot hold results.
pub fn check_output_dir(output: &Path) -> Result<(), CliError> {
    if output.exists() && !output.is_dir() {
        return Err(CliError::refused(format!(
            "{} exists and is not a folder. Choose a folder for the recovered files, for example \
             a new folder on another disk.",
            output.display()
        )));
    }
    Ok(())
}

/// The folder that will contain `file` (the current folder when none is given).
pub fn parent_dir(file: &Path) -> PathBuf {
    match file.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats() -> CategoryArgs {
        CategoryArgs::default()
    }

    #[test]
    fn default_selection_is_photos_raw_videos() {
        let s = selection(&cats(), &StateArgs::default());
        assert_eq!(s, Selection::photos_and_videos());
    }

    #[test]
    fn category_flags_narrow_the_selection() {
        let mut c = cats();
        c.raw = true;
        let s = selection(&c, &StateArgs::default());
        assert_eq!(s.categories.len(), 1);
        assert!(s.categories.contains(&MediaCategory::RawPhoto));
        c.audio = true;
        c.videos = true;
        let s = selection(&c, &StateArgs::default());
        assert!(s.categories.contains(&MediaCategory::Audio));
        assert!(s.categories.contains(&MediaCategory::Video));
        assert!(!s.categories.contains(&MediaCategory::Photo));
    }

    #[test]
    fn all_files_and_state_flags() {
        let mut c = cats();
        c.all_files = true;
        c.min_size = Some(1024);
        let s = selection(
            &c,
            &StateArgs {
                existing_only: false,
                deleted_only: true,
            },
        );
        assert!(s.wants_everything());
        assert!(!s.existing && s.deleted);
        assert_eq!(s.min_size, 1024);
        let s = selection(
            &cats(),
            &StateArgs {
                existing_only: true,
                deleted_only: false,
            },
        );
        assert!(s.existing && !s.deleted);
    }

    #[test]
    fn carve_modes_map_to_policies() {
        assert_eq!(carve_policy(CarveArg::Auto), CarvePolicy::Auto);
        assert_eq!(carve_policy(CarveArg::Free), CarvePolicy::FreeSpace);
        assert_eq!(carve_policy(CarveArg::All), CarvePolicy::Everything);
        assert_eq!(carve_policy(CarveArg::Off), CarvePolicy::Off);
    }

    #[test]
    fn parent_of_bare_name_is_current_dir() {
        assert_eq!(parent_dir(Path::new("card.img")), PathBuf::from("."));
        assert_eq!(parent_dir(Path::new("/x/card.img")), PathBuf::from("/x"));
    }
}
