//! Recovery, scanning, repair and verification, up to the point where the
//! library pipeline runs: the checks before anything is written, opening
//! the source for reading, the pipeline options, and plain-language errors.
//!
//! The command line and the background jobs share this code.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::SharedSource;
use diskrescue_recover::pipeline::{Survey, VerifyResult};
use diskrescue_recover::report::{Report, SourceRecord};
use diskrescue_recover::{CarvePolicy, Observer, RecoverOptions, Selection};

use crate::error::{explain_io, AppError, Result};
use crate::plan::{ImagePlan, RepairOptions, Tuning};
use crate::source::{self, Opened, SourceRequest};
use crate::util::same_file;

/// The imaging step of a recovery that images the device first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageToStep {
    /// Image file to create or resume.
    pub image: PathBuf,
    /// Strategy overrides.
    pub tuning: Tuning,
}

/// A recovery job (`recover` and `carve`), as resolved from a
/// [`crate::plan::RecoverPlan`] or [`crate::plan::CarvePlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryJob {
    /// Device, partition or image file.
    pub source: PathBuf,
    /// Output folder.
    pub output: PathBuf,
    /// Mapfile of the image.
    pub map: Option<PathBuf>,
    /// What to recover.
    pub selection: Selection,
    /// Carving policy.
    pub carve: CarvePolicy,
    /// Read filesystems (false for carving only).
    pub use_filesystems: bool,
    /// Repair options.
    pub repair: RepairOptions,
    /// Worker threads (`None` = one per CPU).
    pub jobs: Option<usize>,
    /// Allow a mounted source.
    pub allow_mounted: bool,
    /// Image the device first.
    pub image_to: Option<ImageToStep>,
    /// Command line for the report and the mapfile.
    pub command_line: Option<String>,
}

impl RecoveryJob {
    /// The checks that need nothing but the paths: the output is a folder
    /// (or does not exist yet), every donor is a file, and the output is not
    /// the source itself.
    pub fn check(&self) -> Result<()> {
        check_output_dir(&self.output)?;
        check_donors(&self.repair.donors)?;
        check_output_not_source(&self.source, &self.output)
    }

    /// The imaging step as an image plan (never forced, space checked).
    pub fn image_plan(&self) -> Option<ImagePlan> {
        self.image_to.as_ref().map(|step| ImagePlan {
            source: self.source.clone(),
            image: step.image.clone(),
            map: self.map.clone(),
            tuning: step.tuning.clone(),
            start: None,
            size: None,
            force: false,
            allow_mounted: self.allow_mounted,
            no_space_check: false,
            command_line: self.command_line.clone(),
        })
    }

    /// Before imaging: the folder for the results must be safe too (the
    /// image's folder is checked while imaging).
    pub fn check_output_against_device(&self) -> Result<DeviceInfo> {
        let info = source::describe(&self.source)?;
        let check = diskrescue_recover::safety::check(
            &self.source,
            &info,
            &self.output,
            self.allow_mounted,
        );
        check.into_result().map_err(AppError::from)?;
        Ok(info)
    }

    /// Open `source` (the job's source, or the image made from it) for
    /// reading, with its mapfile, after the safety checks against the output.
    pub fn open(
        &self,
        source: &Path,
        map: Option<&Path>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Opened> {
        let mut req = SourceRequest::reading(source);
        req.map = map;
        req.use_map = true;
        req.allow_mounted = self.allow_mounted;
        req.output = Some(&self.output);
        req.cancel = cancel;
        source::open(&req)
    }

    /// The warnings to show before recovering from `opened`: the source's
    /// own, plus advice when a device is read directly.
    pub fn warnings(&self, opened: &Opened) -> Vec<String> {
        let reading_device = opened.info.kind != DeviceKind::ImageFile;
        let mut warnings = opened.warnings.clone();
        if reading_device && self.image_to.is_none() {
            warnings.push(
                "Reading directly from the device. That is fine when the device is healthy (for example \
                 after accidental deletion). If it shows read errors, slows down or disconnects, stop and \
                 copy it to an image first with: diskrescue image <device> <image file>"
                    .to_string(),
            );
        }
        warnings
    }

    /// The pipeline options for this job; `cancel` stops it early (the
    /// partial result is still reported).
    pub fn options(&self, cancel: Arc<AtomicBool>) -> RecoverOptions {
        let mut opts = RecoverOptions::new(&self.output);
        opts.selection = self.selection.clone();
        opts.carve = self.carve;
        opts.use_filesystems = self.use_filesystems;
        opts.repair = !self.repair.no_repair;
        opts.donors = self.repair.donors.clone();
        opts.aggressive_repair = self.repair.aggressive;
        opts.workers = self.jobs.unwrap_or(0);
        opts.cancel = cancel;
        opts.command_line = self.command_line.clone();
        opts
    }

    /// Paths the job writes, for the ownership hand-back.
    pub fn written_paths(&self) -> Vec<PathBuf> {
        let mut v = vec![self.output.clone()];
        if let Some(step) = &self.image_to {
            v.extend(crate::ownership::image_and_sidecars(
                &step.image,
                self.map.as_deref(),
            ));
        }
        v
    }
}

/// Refuse an output location that cannot hold results.
pub fn check_output_dir(output: &Path) -> Result<()> {
    if output.exists() && !output.is_dir() {
        return Err(AppError::refused(format!(
            "{} exists and is not a folder. Choose a folder for the recovered files, for example \
             a new folder on another disk.",
            output.display()
        )));
    }
    Ok(())
}

/// Check that every donor is an existing file.
pub fn check_donors(donors: &[PathBuf]) -> Result<()> {
    for d in donors {
        match std::fs::metadata(d) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(AppError::failed(format!(
                    "the donor {} is not a file. A donor is a healthy photo or video from the same \
                     camera or phone.",
                    d.display()
                )))
            }
            Err(_) => {
                return Err(AppError::failed(format!(
                "the donor {} does not exist. A donor is a healthy photo or video from the same \
                     camera or phone.",
                d.display()
            )))
            }
        }
    }
    Ok(())
}

/// The output folder must not be the source itself.
pub fn check_output_not_source(source: &Path, output: &Path) -> Result<()> {
    if same_file(source, output) {
        return Err(AppError::refused(format!(
            "the output location {} is the source itself. Choose a new folder for the recovered files, \
             preferably on another disk.",
            output.display()
        )));
    }
    Ok(())
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

/// Run the recovery pipeline on an opened source.
pub fn run_pipeline(
    shared: SharedSource,
    record: SourceRecord,
    opts: &RecoverOptions,
    obs: &dyn Observer,
) -> Result<Report> {
    diskrescue_recover::recover(shared, record, opts, obs).map_err(|e| match e {
        diskrescue_core::Error::Io(io) => AppError::failed(explain_io(&io, Some(&opts.output))),
        other => AppError::from(other),
    })
}

/// List what could be recovered from an opened source, writing nothing.
pub fn run_survey(
    shared: SharedSource,
    selection: &Selection,
    carve: CarvePolicy,
    cancel: Arc<AtomicBool>,
    obs: &dyn Observer,
) -> Result<Survey> {
    diskrescue_recover::survey(shared, selection, carve, cancel, obs).map_err(AppError::from)
}

/// The checks of a repair run, in order: inputs exist (wildcards expanded),
/// donors are files, the output is a folder. Returns the expanded inputs.
pub fn check_repair(files: &[PathBuf], output: &Path, donors: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let inputs = crate::util::expand_file_args(files).map_err(AppError::failed)?;
    check_donors(donors)?;
    check_output_dir(output)?;
    Ok(inputs)
}

/// Repair files given by the user into `output/repaired/` (inputs are never
/// modified) and write the report.
pub fn run_repair(
    inputs: &[PathBuf],
    output: &Path,
    donors: &[PathBuf],
    aggressive: bool,
    obs: &dyn Observer,
) -> Result<Report> {
    diskrescue_recover::repair_files(inputs, output, donors, aggressive, obs).map_err(|e| match e {
        diskrescue_core::Error::Io(io) => AppError::failed(explain_io(&io, Some(output))),
        other => AppError::from(other),
    })
}

/// Check files given by the user (wildcards expanded) and return one
/// result per file.
pub fn run_verify(files: &[PathBuf]) -> Result<Vec<VerifyResult>> {
    let files = crate::util::expand_file_args(files).map_err(AppError::failed)?;
    Ok(diskrescue_recover::verify_files(&files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earlier_report_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        assert!(keep_previous_report(dir.path()).is_none());
        std::fs::write(dir.path().join("report.txt"), "old").unwrap();
        let note = keep_previous_report(dir.path()).unwrap();
        assert!(note.contains("report.previous.txt"), "{note}");
        assert!(dir.path().join("report.previous.txt").is_file());
    }

    #[test]
    fn job_checks_refuse_unsafe_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, [0u8; 512]).unwrap();
        let job = crate::plan::RecoverPlan::new(&img, &img).job();
        assert!(job.check().unwrap_err().is_refusal());
        let mut job = crate::plan::RecoverPlan::new(&img, dir.path().join("out")).job();
        assert!(job.check().is_ok());
        job.repair.donors.push(dir.path().join("missing.jpg"));
        assert!(job.check().unwrap_err().message().contains("donor"));
    }
}
