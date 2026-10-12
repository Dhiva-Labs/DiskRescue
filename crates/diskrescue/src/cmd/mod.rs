//! The subcommands.
//!
//! Argument types are converted here into the plain plans of
//! `diskrescue_app::plan`, which does the work shared with the other
//! front-ends.

pub mod carve;
pub mod image;
pub mod info;
pub mod list;
pub mod map;
pub mod recover;
pub mod repair;
pub mod scan;
pub mod verify;

#[cfg(test)]
use std::path::{Path, PathBuf};

use diskrescue_app::plan;
#[cfg(test)]
use diskrescue_app::util::parent_dir;
#[cfg(test)]
use diskrescue_core::media::MediaCategory;
use diskrescue_recover::{CarvePolicy, Selection};

use crate::args::{CarveArg, CategoryArgs, RepairOpts, StateArgs, Tuning};
use crate::term;

/// The categories as the app crate takes them.
pub fn categories(c: &CategoryArgs) -> plan::Categories {
    plan::Categories {
        photos: c.photos,
        videos: c.videos,
        raw: c.raw,
        audio: c.audio,
        all_files: c.all_files,
        min_size: c.min_size,
    }
}

/// The existing/deleted filter as the app crate takes it.
pub fn state_filter(s: &StateArgs) -> plan::StateFilter {
    plan::StateFilter {
        existing_only: s.existing_only,
        deleted_only: s.deleted_only,
    }
}

/// The carving mode as the app crate takes it.
pub fn carve_mode(arg: CarveArg) -> plan::CarveMode {
    match arg {
        CarveArg::Auto => plan::CarveMode::Auto,
        CarveArg::Free => plan::CarveMode::Free,
        CarveArg::All => plan::CarveMode::All,
        CarveArg::Off => plan::CarveMode::Off,
    }
}

/// The repair options as the app crate takes them.
pub fn repair_options(r: &RepairOpts) -> plan::RepairOptions {
    plan::RepairOptions {
        no_repair: r.no_repair,
        donors: r.donor.clone(),
        aggressive: r.aggressive,
    }
}

/// The read-strategy overrides as the app crate takes them.
pub fn tuning(t: &Tuning) -> plan::Tuning {
    plan::Tuning {
        profile: t.profile.clone(),
        timeout: t.timeout,
        block_size: t.block_size,
        retries: t.retries,
        no_trim: t.no_trim,
        no_scrape: t.no_scrape,
        reverse: t.reverse,
    }
}

/// What the user asked to recover.
///
/// No category flag means photos, RAW and videos; `--all-files` means every
/// file; otherwise exactly the categories named.
pub fn selection(categories_: &CategoryArgs, state: &StateArgs) -> Selection {
    plan::selection(&categories(categories_), &state_filter(state))
}

/// The pipeline's carving policy for the `--carve` option.
pub fn carve_policy(arg: CarveArg) -> CarvePolicy {
    carve_mode(arg).policy()
}

/// Print warnings to standard error.
pub fn print_warnings(warnings: &[String]) {
    let p = term::err();
    for w in warnings {
        eprintln!("{} {w}", p.yellow("warning:"));
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
