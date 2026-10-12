//! Is it safe to start? Every check the command line makes before it
//! writes a byte, collected instead of stopping at the first problem.
//!
//! A [`Preflight`] lists the [`Blocker`]s (each with a [`BlockerCode`] and,
//! when a plan field can override it, the [`Override`] that does), the
//! warnings to show, and what was resolved (the device, the profile, the
//! mapfile, how much space is needed). Nothing here writes anything and no
//! device is opened for reading; at most a device node is opened and
//! closed again to learn whether the current user may read it.
//!
//! The messages are the command line's own, so they mention its flags
//! (`--force`, `--allow-mounted`); front-ends can offer the matching
//! [`Override`] instead.

use std::path::{Path, PathBuf};

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
use diskrescue_core::Range;
use diskrescue_imaging::Stats;

use crate::error::{explain_io, is_permission_error, AppError};
use crate::imaging;
use crate::plan::{CarvePlan, ImagePlan, RecoverPlan, RepairPlan, ScanPlan, VerifyPlan};
use crate::recovery::{self, RecoveryJob};
use crate::source::{self, SourceRequest};
use crate::util::{expand_file_args, parent_dir};

/// What kind of problem stops an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockerCode {
    /// The source does not exist.
    SourceMissing,
    /// The source is a folder.
    SourceIsFolder,
    /// The current user may not read the source (needs administrator rights).
    PermissionDenied,
    /// The source cannot be described or opened for another reason.
    SourceUnreadable,
    /// The device reports a capacity of 0 bytes (controller failure), or
    /// the image file is empty. Never overridable.
    ZeroCapacity,
    /// The source is mounted. Overridable with [`Override::AllowMounted`].
    SourceMounted,
    /// The output (folder or image) is on the source device, or is the
    /// source image itself. Never overridable.
    OutputOnSource,
    /// It could not be verified that the output is on another device.
    OutputUnverified,
    /// The output folder is the source itself.
    OutputIsSource,
    /// The output exists and is not a folder.
    OutputNotFolder,
    /// The image is the source itself.
    ImageIsSource,
    /// The image path is a folder.
    ImageIsFolder,
    /// The image path is a device node or another non-regular file.
    ImageNotAFile,
    /// The progress map and the image are the same file.
    MapIsImage,
    /// The progress map (or a given mapfile) exists but cannot be read.
    MapUnreadable,
    /// A mapfile was given but does not exist.
    MapMissing,
    /// The image exists without a progress map. Overridable with
    /// [`Override::Force`].
    ImageExists,
    /// The progress map records copied data but the image is gone.
    ImageMissing,
    /// The progress map was made while copying a different device.
    /// Overridable with [`Override::Force`].
    DeviceChanged,
    /// Not enough free space for the image. Overridable with
    /// [`Override::SkipSpaceCheck`].
    NotEnoughSpace,
    /// An option is out of range (unknown profile, zero block size, a start
    /// beyond the end).
    InvalidOption,
    /// A donor file does not exist or is not a file.
    DonorMissing,
    /// An input file of repair or verify does not exist, is a folder, or a
    /// pattern matches nothing.
    InputInvalid,
}

/// The plan field that overrides a blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Override {
    /// `allow_mounted` (`--allow-mounted`).
    AllowMounted,
    /// `force` (`--force`).
    Force,
    /// `no_space_check` (`--no-space-check`).
    SkipSpaceCheck,
}

/// One reason not to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    /// What kind of problem.
    pub code: BlockerCode,
    /// The explanation for the user.
    pub message: String,
    /// The plan field that would override it; `None` when it can never be
    /// overridden.
    pub override_flag: Option<Override>,
}

/// The result of a preflight check.
#[derive(Debug, Clone, PartialEq)]
pub struct Preflight<R> {
    /// Problems that stop the operation. Overridden problems are not listed
    /// here (they appear in `warnings`).
    pub blockers: Vec<Blocker>,
    /// Things to tell the user before starting.
    pub warnings: Vec<String>,
    /// What was resolved (`None` when the source could not be described).
    pub resolved: Option<R>,
}

impl<R> Default for Preflight<R> {
    fn default() -> Self {
        Preflight {
            blockers: Vec::new(),
            warnings: Vec::new(),
            resolved: None,
        }
    }
}

impl<R> Preflight<R> {
    /// True when nothing blocks the operation.
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty()
    }

    /// The blockers as one text (`None` when clear).
    pub fn refusal(&self) -> Option<String> {
        (!self.blockers.is_empty()).then(|| {
            self.blockers
                .iter()
                .map(|b| b.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        })
    }

    /// True when some blocker has `code`.
    pub fn has(&self, code: BlockerCode) -> bool {
        self.blockers.iter().any(|b| b.code == code)
    }

    fn block(&mut self, code: BlockerCode, message: impl Into<String>, over: Option<Override>) {
        let message = message.into();
        if !self.blockers.iter().any(|b| b.message == message) {
            self.blockers.push(Blocker {
                code,
                message,
                override_flag: over,
            });
        }
    }

    fn warn(&mut self, w: impl Into<String>) {
        let w = w.into();
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    fn absorb<T>(&mut self, other: Preflight<T>) -> Option<T> {
        for b in other.blockers {
            self.block(b.code, b.message, b.override_flag);
        }
        for w in other.warnings {
            self.warn(w);
        }
        other.resolved
    }
}

/// What a clear imaging preflight resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedImage {
    /// The source as described (not opened).
    pub source: DeviceInfo,
    /// The image file.
    pub image: PathBuf,
    /// The progress map.
    pub mapfile: PathBuf,
    /// The strategy that will be used (`None` when the options are invalid).
    pub profile: Option<ImagingProfile>,
    /// The part of the source to copy (`None` = all of it).
    pub domain: Option<Range>,
    /// Bytes the copy covers.
    pub copy_size: u64,
    /// Totals of an earlier copy that will be continued.
    pub resuming: Option<Stats>,
    /// The progress map says the image is complete: nothing to do.
    pub already_complete: bool,
    /// Bytes still to be allocated on the output disk.
    pub space_needed: u64,
    /// Free space on the output disk, when known.
    pub free_space: Option<u64>,
}

/// What a clear recovery preflight resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRecover {
    /// The source as described.
    pub source: DeviceInfo,
    /// The source is a device read directly (no image first).
    pub reading_device: bool,
    /// The imaging step, when the plan images first.
    pub image: Option<ResolvedImage>,
    /// The output folder already holds a report (it will be kept as
    /// `report.previous.*`).
    pub previous_report: bool,
}

fn describe_or_block<R>(path: &Path, pf: &mut Preflight<R>) -> Option<DeviceInfo> {
    match source::describe(path) {
        Ok(i) => Some(i),
        Err(e) => {
            let code = match std::fs::metadata(path) {
                Ok(m) if m.is_dir() => BlockerCode::SourceIsFolder,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    BlockerCode::SourceMissing
                }
                Err(err) if is_permission_error(&err) => BlockerCode::PermissionDenied,
                _ => BlockerCode::SourceUnreadable,
            };
            pf.block(code, e.message(), None);
            None
        }
    }
}

/// Open `path` read-only and close it again, to learn whether this user
/// may read it. Never reads data; on Unix the node is opened non-blocking
/// so that drives with removable media are not woken up.
pub(crate) fn probe_read_access(path: &Path) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NONBLOCK);
    }
    o.open(path).map(drop)
}

fn access_or_block<R>(path: &Path, pf: &mut Preflight<R>) {
    if let Err(e) = probe_read_access(path) {
        let code = if is_permission_error(&e) {
            BlockerCode::PermissionDenied
        } else {
            BlockerCode::SourceUnreadable
        };
        pf.block(code, explain_io(&e, Some(path)), None);
    }
}

/// The checks of `diskrescue_recover::safety::check` with codes: output on
/// the device, an unverifiable output, a 0-byte device, a mounted source.
fn safety_or_block<R>(
    source: &Path,
    info: &DeviceInfo,
    output: &Path,
    allow_mounted: bool,
    pf: &mut Preflight<R>,
) {
    let strict = diskrescue_recover::safety::check(source, info, output, false);
    let lenient = diskrescue_recover::safety::check(source, info, output, true);
    for b in &strict.blockers {
        if lenient.blockers.contains(b) {
            let code = if info.kind != DeviceKind::ImageFile
                && info.size == 0
                && b.contains("capacity of 0 bytes")
            {
                BlockerCode::ZeroCapacity
            } else if b.starts_with("could not verify") {
                BlockerCode::OutputUnverified
            } else {
                BlockerCode::OutputOnSource
            };
            pf.block(code, b.clone(), None);
        } else if !allow_mounted {
            pf.block(
                BlockerCode::SourceMounted,
                b.clone(),
                Some(Override::AllowMounted),
            );
        }
    }
    let warnings = if allow_mounted {
        lenient.warnings
    } else {
        strict.warnings
    };
    for w in warnings {
        pf.warn(w);
    }
}

/// Check an imaging plan. See the module documentation.
pub fn preflight_image(plan: &ImagePlan) -> Preflight<ResolvedImage> {
    let mut pf = Preflight::default();
    if let Err(e) = imaging::check_image_not_source(plan) {
        pf.block(BlockerCode::ImageIsSource, e.message(), None);
    }
    let image_meta = match imaging::image_metadata(plan) {
        Ok(m) => m,
        Err(e) => {
            let code = match e {
                AppError::Invalid(_) => BlockerCode::ImageIsFolder,
                _ => BlockerCode::ImageNotAFile,
            };
            pf.block(code, e.message(), None);
            None
        }
    };
    let map_path = plan.mapfile();
    if let Err(e) = imaging::check_map_path(plan, &map_path) {
        pf.block(BlockerCode::MapIsImage, e.message(), None);
    }
    let prior = match imaging::prior_stats(&map_path) {
        Ok(p) => p,
        Err(e) => {
            pf.block(BlockerCode::MapUnreadable, e.message(), None);
            None
        }
    };
    let unforced = ImagePlan {
        force: false,
        ..plan.clone()
    };
    if let Err(e) =
        imaging::check_no_clobber(&unforced, prior.as_ref(), image_meta.as_ref(), &map_path)
    {
        if plan.force {
            pf.warn(format!(
                "{} already exists and will be replaced.",
                plan.image.display()
            ));
        } else {
            pf.block(BlockerCode::ImageExists, e.message(), Some(Override::Force));
        }
    }
    if let Err(e) =
        imaging::check_image_present(plan, prior.as_ref(), image_meta.as_ref(), &map_path)
    {
        pf.block(BlockerCode::ImageMissing, e.message(), None);
    }
    let complete = imaging::already_complete(plan, prior.as_ref(), image_meta.as_ref());

    let Some(info) = describe_or_block(&plan.source, &mut pf) else {
        return pf;
    };
    let mut resolved = ResolvedImage {
        source: info.clone(),
        image: plan.image.clone(),
        mapfile: map_path.clone(),
        profile: None,
        domain: None,
        copy_size: 0,
        resuming: prior,
        already_complete: complete.is_some(),
        space_needed: 0,
        free_space: None,
    };
    if let Some(st) = complete {
        // Nothing to copy: the device will not even be opened.
        resolved.copy_size = st.total();
        resolved.profile = imaging::build_profile(info.kind, &plan.tuning).ok();
        pf.resolved = Some(resolved);
        return pf;
    }

    if info.kind == DeviceKind::ImageFile && info.size == 0 {
        pf.block(
            BlockerCode::ZeroCapacity,
            format!(
                "{} is empty: there is nothing to copy.",
                plan.source.display()
            ),
            None,
        );
    }
    access_or_block(&plan.source, &mut pf);
    let out_dir = parent_dir(&plan.image);
    safety_or_block(&plan.source, &info, &out_dir, plan.allow_mounted, &mut pf);

    match imaging::build_profile(info.kind, &plan.tuning) {
        Ok(p) => resolved.profile = Some(p),
        Err(e) => pf.block(BlockerCode::InvalidOption, e.message(), None),
    }
    match imaging::build_domain(plan.start, plan.size, info.size) {
        Ok(d) => {
            resolved.domain = d;
            resolved.copy_size = d.map(|r| r.len()).unwrap_or(info.size);
        }
        Err(e) => pf.block(BlockerCode::InvalidOption, e.message(), None),
    }

    let have = image_meta
        .as_ref()
        .map(crate::util::allocated_bytes)
        .unwrap_or(0);
    resolved.space_needed = resolved.copy_size.saturating_sub(have);
    resolved.free_space = crate::util::free_space(&out_dir);
    if let Err(e) = imaging::check_space(plan, image_meta.as_ref(), resolved.copy_size, &out_dir) {
        if plan.no_space_check {
            pf.warn(format!(
                "The free-space check is off: {} needs {} but only {} is free on that disk.",
                plan.image.display(),
                human::bytes(resolved.space_needed),
                human::bytes(resolved.free_space.unwrap_or(0))
            ));
        } else {
            pf.block(
                BlockerCode::NotEnoughSpace,
                e.message(),
                Some(Override::SkipSpaceCheck),
            );
        }
    }

    if let Err(e) = imaging::check_identity(&unforced, prior.as_ref(), &info, &map_path) {
        if plan.force {
            pf.warn(format!(
                "The progress map {} was made while copying a different device; the copy continues \
                 anyway because it was forced.",
                map_path.display()
            ));
        } else {
            pf.block(
                BlockerCode::DeviceChanged,
                e.message(),
                Some(Override::Force),
            );
        }
    }
    pf.resolved = Some(resolved);
    pf
}

/// Check a recovery plan (including its imaging step). See the module
/// documentation.
pub fn preflight_recover(plan: &RecoverPlan) -> Preflight<ResolvedRecover> {
    preflight_job(&plan.job())
}

/// Check a carving plan.
pub fn preflight_carve(plan: &CarvePlan) -> Preflight<ResolvedRecover> {
    preflight_job(&plan.job())
}

/// Check a recovery job (what [`preflight_recover`] and
/// [`preflight_carve`] resolve to).
pub fn preflight_job(job: &RecoveryJob) -> Preflight<ResolvedRecover> {
    let mut pf = Preflight::default();
    if let Err(e) = recovery::check_output_dir(&job.output) {
        pf.block(BlockerCode::OutputNotFolder, e.message(), None);
    }
    for d in &job.repair.donors {
        if let Err(e) = recovery::check_donors(std::slice::from_ref(d)) {
            pf.block(BlockerCode::DonorMissing, e.message(), None);
        }
    }
    if let Err(e) = recovery::check_output_not_source(&job.source, &job.output) {
        pf.block(BlockerCode::OutputIsSource, e.message(), None);
    }
    let previous_report =
        job.output.join("report.json").is_file() || job.output.join("report.txt").is_file();
    if previous_report {
        pf.warn(format!(
            "{} already contains the report of an earlier run; it will be kept as report.previous.txt \
             and report.previous.json.",
            job.output.display()
        ));
    }

    let mut image = None;
    if let Some(step) = job.image_plan() {
        image = pf.absorb(preflight_image(&step));
    }
    let Some(info) = describe_or_block(&job.source, &mut pf) else {
        return pf;
    };
    let reading_device = info.kind != DeviceKind::ImageFile && job.image_to.is_none();
    if job.image_to.is_none() {
        if info.kind == DeviceKind::ImageFile {
            map_or_block(job, &mut pf);
        } else {
            access_or_block(&job.source, &mut pf);
        }
    }
    safety_or_block(&job.source, &info, &job.output, job.allow_mounted, &mut pf);
    if reading_device {
        pf.warn(
            "Reading directly from the device. That is fine when the device is healthy (for example \
             after accidental deletion). If it shows read errors, slows down or disconnects, stop and \
             copy it to an image first with: diskrescue image <device> <image file>",
        );
    }
    pf.resolved = Some(ResolvedRecover {
        source: info,
        reading_device,
        image,
        previous_report,
    });
    pf
}

/// An image source: open it (a regular file, read-only) with its mapfile to
/// learn whether the mapfile is usable and what it says.
fn map_or_block<R>(job: &RecoveryJob, pf: &mut Preflight<R>) {
    let mut req = SourceRequest::reading(&job.source);
    req.map = job.map.as_deref();
    match source::open(&req) {
        Ok(opened) => {
            for w in opened.warnings {
                pf.warn(w);
            }
        }
        Err(e) => {
            let code = match &job.map {
                Some(m) if !m.exists() => BlockerCode::MapMissing,
                Some(_) if e.message().contains("mapfile") => BlockerCode::MapUnreadable,
                _ => match probe_read_access(&job.source) {
                    Err(io) if is_permission_error(&io) => BlockerCode::PermissionDenied,
                    _ => BlockerCode::SourceUnreadable,
                },
            };
            pf.block(code, e.message(), None);
        }
    }
}

/// Check a scan plan: the source can be described and read.
pub fn preflight_scan(plan: &ScanPlan) -> Preflight<DeviceInfo> {
    let mut pf = Preflight::default();
    let Some(info) = describe_or_block(&plan.source, &mut pf) else {
        return pf;
    };
    if info.kind == DeviceKind::ImageFile {
        let job = RecoveryJob {
            source: plan.source.clone(),
            output: PathBuf::new(),
            map: plan.map.clone(),
            selection: crate::plan::selection(&plan.categories, &plan.state),
            carve: plan.carve_policy(),
            use_filesystems: true,
            repair: Default::default(),
            jobs: None,
            allow_mounted: true,
            image_to: None,
            command_line: None,
        };
        map_or_block(&job, &mut pf);
    } else {
        access_or_block(&plan.source, &mut pf);
        if diskrescue_device::is_mounted(&info) {
            pf.warn(source::mounted_warning(&plan.source, &info));
        }
    }
    pf.resolved = Some(info);
    pf
}

/// Check a repair plan: inputs, donors and the output folder.
pub fn preflight_repair(plan: &RepairPlan) -> Preflight<Vec<PathBuf>> {
    let mut pf = Preflight::default();
    match expand_file_args(&plan.files) {
        Ok(v) if v.is_empty() => pf.block(BlockerCode::InputInvalid, "no files were given", None),
        Ok(v) => pf.resolved = Some(v),
        Err(m) => pf.block(BlockerCode::InputInvalid, m, None),
    }
    for d in &plan.donors {
        if let Err(e) = recovery::check_donors(std::slice::from_ref(d)) {
            pf.block(BlockerCode::DonorMissing, e.message(), None);
        }
    }
    if let Err(e) = recovery::check_output_dir(&plan.output) {
        pf.block(BlockerCode::OutputNotFolder, e.message(), None);
    }
    pf
}

/// Check a verify plan: every input exists and is a file.
pub fn preflight_verify(plan: &VerifyPlan) -> Preflight<Vec<PathBuf>> {
    let mut pf = Preflight::default();
    match expand_file_args(&plan.files) {
        Ok(v) if v.is_empty() => pf.block(BlockerCode::InputInvalid, "no files were given", None),
        Ok(v) => pf.resolved = Some(v),
        Err(m) => pf.block(BlockerCode::InputInvalid, m, None),
    }
    pf
}
