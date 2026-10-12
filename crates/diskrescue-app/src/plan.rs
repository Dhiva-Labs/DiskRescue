//! Plain descriptions of every operation, mirroring the command-line flags.
//!
//! A front-end fills one of these (every field has a sensible default),
//! asks [`crate::preflight`] whether it is safe, and hands it to
//! [`crate::jobs`] to run in the background.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use diskrescue_core::media::MediaCategory;
use diskrescue_recover::{CarvePolicy, Selection};

use crate::recovery::{ImageToStep, RecoveryJob};
use crate::source::default_map_path;
use crate::text::shell_quote;

/// Read-strategy tuning shared by imaging and `recover --image-to`
/// (`--profile`, `--timeout`, `--block-size`, `--retries`, `--no-trim`,
/// `--no-scrape`, `--reverse`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tuning {
    /// Imaging profile name (see `ImagingProfile::names`); chosen from the
    /// device type when `None`.
    pub profile: Option<String>,
    /// Give up on a single read after this long (all passes).
    pub timeout: Option<Duration>,
    /// Size of one read.
    pub block_size: Option<u64>,
    /// Number of retry passes over unreadable sectors.
    pub retries: Option<u32>,
    /// Skip the trimming pass.
    pub no_trim: bool,
    /// Skip the scraping pass.
    pub no_scrape: bool,
    /// Copy from the end towards the start.
    pub reverse: bool,
}

/// Which kinds of files to recover (`--photos`, `--videos`, `--raw`,
/// `--audio`, `--all-files`, `--min-size`). No category means photos, RAW
/// and videos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Categories {
    /// Photos (JPEG, PNG, HEIC, ...).
    pub photos: bool,
    /// Videos (MP4, MOV, AVI, ...).
    pub videos: bool,
    /// Camera RAW photos.
    pub raw: bool,
    /// Audio files.
    pub audio: bool,
    /// Every file, not just media.
    pub all_files: bool,
    /// Skip files smaller than this many bytes.
    pub min_size: Option<u64>,
}

/// Existing versus deleted files (`--existing-only`, `--deleted-only`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateFilter {
    /// Only files that still exist.
    pub existing_only: bool,
    /// Only deleted files.
    pub deleted_only: bool,
}

/// How much of the raw data to search (`--carve`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CarveMode {
    /// Free space plus anything the filesystem does not cover (everything
    /// when no filesystem is readable).
    #[default]
    Auto,
    /// Free space of readable filesystems only.
    Free,
    /// The whole source, ignoring filesystems.
    All,
    /// Do not search the raw data.
    Off,
}

impl CarveMode {
    /// The pipeline's carving policy.
    pub fn policy(self) -> CarvePolicy {
        match self {
            CarveMode::Auto => CarvePolicy::Auto,
            CarveMode::Free => CarvePolicy::FreeSpace,
            CarveMode::All => CarvePolicy::Everything,
            CarveMode::Off => CarvePolicy::Off,
        }
    }
}

/// Repair behaviour (`--no-repair`, `--donor`, `--aggressive`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairOptions {
    /// Do not try to repair damaged files.
    pub no_repair: bool,
    /// Healthy files from the same camera or phone, used as references.
    pub donors: Vec<PathBuf>,
    /// Allow repairs that cut damaged parts out.
    pub aggressive: bool,
}

/// What the user asked to recover: no category flag means photos, RAW and
/// videos; `all_files` means every file; otherwise exactly the categories
/// named.
pub fn selection(categories: &Categories, state: &StateFilter) -> Selection {
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

/// `diskrescue image SOURCE IMAGE`: a careful, resumable copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImagePlan {
    /// Device, partition or image file to read.
    pub source: PathBuf,
    /// Image file to create or resume.
    pub image: PathBuf,
    /// Progress map (default `IMAGE.map`).
    pub map: Option<PathBuf>,
    /// Strategy overrides.
    pub tuning: Tuning,
    /// First byte to copy (`--start`).
    pub start: Option<u64>,
    /// Number of bytes to copy (`--size`).
    pub size: Option<u64>,
    /// Overwrite an existing image that has no progress map, or continue
    /// onto a device that differs from the one copied before (`--force`).
    pub force: bool,
    /// Read a mounted source (`--allow-mounted`).
    pub allow_mounted: bool,
    /// Do not check that the output disk has room for the image.
    pub no_space_check: bool,
    /// Command line recorded in the progress map and shown as the way to
    /// resume. `None`: the equivalent `diskrescue image ...` command.
    pub command_line: Option<String>,
}

impl ImagePlan {
    /// A plan with defaults for everything but the two paths.
    pub fn new(source: impl Into<PathBuf>, image: impl Into<PathBuf>) -> Self {
        ImagePlan {
            source: source.into(),
            image: image.into(),
            ..ImagePlan::default()
        }
    }

    /// The progress map in use.
    pub fn mapfile(&self) -> PathBuf {
        self.map
            .clone()
            .unwrap_or_else(|| default_map_path(&self.image))
    }

    /// Arguments of the equivalent `diskrescue image` command (without the
    /// program name and without display options).
    pub fn cli_args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![
            "image".into(),
            self.source.clone().into(),
            self.image.clone().into(),
        ];
        if let Some(m) = &self.map {
            a.push("--map".into());
            a.push(m.clone().into());
        }
        let t = &self.tuning;
        if let Some(p) = &t.profile {
            a.push("--profile".into());
            a.push(p.into());
        }
        if let Some(d) = t.timeout {
            a.push("--timeout".into());
            a.push(format!("{}", d.as_secs_f64()).into());
        }
        if let Some(b) = t.block_size {
            a.push("--block-size".into());
            a.push(b.to_string().into());
        }
        if let Some(r) = t.retries {
            a.push("--retries".into());
            a.push(r.to_string().into());
        }
        for (on, flag) in [
            (t.no_trim, "--no-trim"),
            (t.no_scrape, "--no-scrape"),
            (t.reverse, "--reverse"),
        ] {
            if on {
                a.push(flag.into());
            }
        }
        if let Some(s) = self.start {
            a.push("--start".into());
            a.push(s.to_string().into());
        }
        if let Some(s) = self.size {
            a.push("--size".into());
            a.push(s.to_string().into());
        }
        for (on, flag) in [
            (self.force, "--force"),
            (self.allow_mounted, "--allow-mounted"),
            (self.no_space_check, "--no-space-check"),
        ] {
            if on {
                a.push(flag.into());
            }
        }
        a
    }

    /// The command that runs (and resumes) this copy, ready to paste into a
    /// terminal: [`ImagePlan::command_line`] or the equivalent
    /// `diskrescue image ...`.
    pub fn command_text(&self) -> String {
        if let Some(c) = &self.command_line {
            return c.clone();
        }
        let mut parts = vec!["diskrescue".to_string()];
        parts.extend(
            self.cli_args()
                .iter()
                .map(|a| shell_quote(&a.to_string_lossy())),
        );
        parts.join(" ")
    }

    /// Paths the copy writes (image and its sidecars), for the ownership
    /// hand-back.
    pub fn written_paths(&self) -> Vec<PathBuf> {
        crate::ownership::image_and_sidecars(&self.image, self.map.as_deref())
    }
}

/// `diskrescue recover SOURCE -o DIR`: the whole pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverPlan {
    /// Device, partition or image file.
    pub source: PathBuf,
    /// Folder for the recovered files (`-o`).
    pub output: PathBuf,
    /// What kinds of files.
    pub categories: Categories,
    /// Existing and/or deleted files.
    pub state: StateFilter,
    /// How much of the raw data to search.
    pub carve: CarveMode,
    /// Repair behaviour.
    pub repair: RepairOptions,
    /// Worker threads (`-j`; `None` or 0 = one per CPU).
    pub jobs: Option<usize>,
    /// Copy the device to this image first, then recover from the image
    /// (`--image-to`).
    pub image_to: Option<PathBuf>,
    /// Mapfile of the image (`--map`).
    pub map: Option<PathBuf>,
    /// Read a mounted source (`--allow-mounted`).
    pub allow_mounted: bool,
    /// Strategy for the imaging step.
    pub tuning: Tuning,
    /// Command line for the report (`None`: none recorded).
    pub command_line: Option<String>,
}

impl RecoverPlan {
    /// A plan with defaults for everything but the source and the output.
    pub fn new(source: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        RecoverPlan {
            source: source.into(),
            output: output.into(),
            ..RecoverPlan::default()
        }
    }

    /// The recovery job this plan describes.
    pub fn job(&self) -> RecoveryJob {
        RecoveryJob {
            source: self.source.clone(),
            output: self.output.clone(),
            map: self.map.clone(),
            selection: selection(&self.categories, &self.state),
            carve: self.carve.policy(),
            use_filesystems: true,
            repair: self.repair.clone(),
            jobs: self.jobs.filter(|j| *j > 0),
            allow_mounted: self.allow_mounted,
            image_to: self.image_to.as_ref().map(|image| ImageToStep {
                image: image.clone(),
                tuning: self.tuning.clone(),
            }),
            command_line: self.command_line.clone(),
        }
    }
}

/// `diskrescue carve SOURCE -o DIR`: search the raw data only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarvePlan {
    /// Device, partition or image file.
    pub source: PathBuf,
    /// Folder for the recovered files.
    pub output: PathBuf,
    /// What kinds of files.
    pub categories: Categories,
    /// Search only the free space of readable filesystems (`--free-only`).
    pub free_only: bool,
    /// Repair behaviour.
    pub repair: RepairOptions,
    /// Worker threads.
    pub jobs: Option<usize>,
    /// Mapfile of the image.
    pub map: Option<PathBuf>,
    /// Read a mounted source.
    pub allow_mounted: bool,
    /// Command line for the report.
    pub command_line: Option<String>,
}

impl CarvePlan {
    /// A plan with defaults for everything but the source and the output.
    pub fn new(source: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        CarvePlan {
            source: source.into(),
            output: output.into(),
            ..CarvePlan::default()
        }
    }

    /// The recovery job this plan describes.
    pub fn job(&self) -> RecoveryJob {
        let mut sel = selection(&self.categories, &StateFilter::default());
        let (carve, use_filesystems) = if self.free_only {
            // Read the filesystems only to learn which space is free; do not
            // extract the files they list.
            sel.existing = false;
            sel.deleted = false;
            (CarvePolicy::FreeSpace, true)
        } else {
            (CarvePolicy::Everything, false)
        };
        RecoveryJob {
            source: self.source.clone(),
            output: self.output.clone(),
            map: self.map.clone(),
            selection: sel,
            carve,
            use_filesystems,
            repair: self.repair.clone(),
            jobs: self.jobs.filter(|j| *j > 0),
            allow_mounted: self.allow_mounted,
            image_to: None,
            command_line: self.command_line.clone(),
        }
    }
}

/// `diskrescue repair FILE... -o DIR`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairPlan {
    /// Files to repair (`*` and `?` in names are expanded).
    pub files: Vec<PathBuf>,
    /// Folder for the repaired copies.
    pub output: PathBuf,
    /// Healthy reference files.
    pub donors: Vec<PathBuf>,
    /// Allow repairs that cut damaged parts out.
    pub aggressive: bool,
}

/// `diskrescue verify FILE...`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyPlan {
    /// Files to check (`*` and `?` in names are expanded).
    pub files: Vec<PathBuf>,
}

/// `diskrescue scan SOURCE`: list what can be recovered, write nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanPlan {
    /// Device, partition or image file.
    pub source: PathBuf,
    /// What kinds of files.
    pub categories: Categories,
    /// Existing and/or deleted files.
    pub state: StateFilter,
    /// Also search the raw data (`--carve [MODE]`); `None` = no.
    pub carve: Option<CarveMode>,
    /// Mapfile of the image.
    pub map: Option<PathBuf>,
}

impl ScanPlan {
    /// The carving policy for this scan.
    pub fn carve_policy(&self) -> CarvePolicy {
        self.carve
            .map(CarveMode::policy)
            .unwrap_or(CarvePolicy::Off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_selection_is_photos_raw_videos() {
        let s = selection(&Categories::default(), &StateFilter::default());
        assert_eq!(s, Selection::photos_and_videos());
        let s = selection(
            &Categories {
                raw: true,
                ..Categories::default()
            },
            &StateFilter {
                deleted_only: true,
                ..StateFilter::default()
            },
        );
        assert_eq!(s.categories.len(), 1);
        assert!(!s.existing && s.deleted);
    }

    #[test]
    fn carve_plan_maps_like_the_cli() {
        let mut p = CarvePlan::new("a.img", "out");
        let j = p.job();
        assert_eq!(j.carve, CarvePolicy::Everything);
        assert!(!j.use_filesystems);
        p.free_only = true;
        let j = p.job();
        assert_eq!(j.carve, CarvePolicy::FreeSpace);
        assert!(j.use_filesystems && !j.selection.existing && !j.selection.deleted);
    }

    #[test]
    fn image_command_text_resumes_the_same_copy() {
        let mut p = ImagePlan::new("/dev/sdb", "my card.img");
        p.tuning.profile = Some("sd".into());
        p.start = Some(1 << 20);
        let t = p.command_text();
        assert!(t.starts_with("diskrescue image /dev/sdb"), "{t}");
        assert!(
            t.contains("--profile sd") && t.contains("--start 1048576"),
            "{t}"
        );
        assert_eq!(p.mapfile(), PathBuf::from("my card.img.map"));
        p.command_line = Some("diskrescue image x y".into());
        assert_eq!(p.command_text(), "diskrescue image x y");
    }
}
