//! The imaging step, up to the point where the engine runs.
//!
//! [`prepare`] performs every check before a byte is copied (the image is
//! not the source, never overwritten without a progress map, resumed only
//! onto the same device, there is room for it, the output is not on the
//! source device) and opens the source read-only. It returns a ready
//! [`Imager`]; the caller runs it on a thread of its choice and turns the
//! result into an [`ImagingDone`].
//!
//! The command line and the background jobs share this code, so both refuse
//! exactly the same things with exactly the same words.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
use diskrescue_core::Range;
use diskrescue_device::{DeviceSource, Fingerprint, Match};
use diskrescue_imaging::{Imager, ImagingOptions, MapFile, Stats, Summary};

use crate::error::{explain_io, AppError, Result};
use crate::plan::{ImagePlan, Tuning};
use crate::source::{self, SourceRequest};
use crate::text::shell_quote;
use crate::util::{allocated_bytes, free_space, parent_dir, same_file};

/// What an imaging run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ImagingDone {
    /// Totals at the end.
    pub stats: Stats,
    /// The engine's summary (None when the image was already complete).
    pub summary: Option<Summary>,
    /// The device that was copied.
    pub info: DeviceInfo,
    /// The image.
    pub image: PathBuf,
    /// The mapfile.
    pub mapfile: PathBuf,
    /// Nothing had to be done.
    pub already_complete: bool,
}

impl ImagingDone {
    /// All requested bytes were handled (copied or confirmed unreadable).
    pub fn finished(&self) -> bool {
        self.already_complete || self.summary.as_ref().is_some_and(|s| s.finished)
    }

    /// The run was stopped by the user.
    pub fn cancelled(&self) -> bool {
        self.summary.as_ref().is_some_and(|s| s.cancelled)
    }

    /// The result of a run that returned `summary`.
    pub fn from_summary(
        summary: Summary,
        info: DeviceInfo,
        image: PathBuf,
        mapfile: PathBuf,
    ) -> Self {
        ImagingDone {
            stats: summary.stats,
            summary: Some(summary),
            info,
            image,
            mapfile,
            already_complete: false,
        }
    }
}

/// Apply the profile name, block size, timeout, retries, `no_trim` and
/// `no_scrape` of `t` to the profile recommended for `kind`.
pub fn build_profile(kind: DeviceKind, t: &Tuning) -> Result<ImagingProfile> {
    let mut p = match &t.profile {
        Some(name) => ImagingProfile::by_name(name).ok_or_else(|| {
            AppError::invalid(format!(
                "unknown profile '{name}'. Choose one of: {}",
                ImagingProfile::names().join(", ")
            ))
        })?,
        None => ImagingProfile::for_kind(kind),
    };
    if let Some(bs) = t.block_size {
        if bs == 0 {
            return Err(AppError::invalid("--block-size must be larger than zero"));
        }
        p.block_size = bs;
    }
    if let Some(d) = t.timeout {
        p.timeout_copy = d;
        p.timeout_scrape = d;
        p.timeout_retry = d;
    }
    if let Some(n) = t.retries {
        p.retry_passes = n;
    }
    if t.no_trim {
        p.trim = false;
    }
    if t.no_scrape {
        p.scrape = false;
    }
    Ok(p)
}

/// The rescue domain for a start offset and a length.
pub fn build_domain(
    start: Option<u64>,
    size: Option<u64>,
    source_size: u64,
) -> Result<Option<Range>> {
    if start.is_none() && size.is_none() {
        return Ok(None);
    }
    let s = start.unwrap_or(0);
    if s >= source_size {
        return Err(AppError::invalid(format!(
            "--start {} is beyond the end of the source, which is {} long",
            human::bytes(s),
            human::bytes(source_size)
        )));
    }
    let e = match size {
        Some(0) => return Err(AppError::invalid("--size must be larger than zero")),
        Some(n) => s.saturating_add(n).min(source_size),
        None => source_size,
    };
    Ok(Some(Range::new(s, e)))
}

/// File next to the mapfile that remembers which device was being copied.
pub fn identity_path(mapfile: &Path) -> PathBuf {
    let mut s = mapfile.as_os_str().to_os_string();
    s.push(".device.json");
    PathBuf::from(s)
}

/// Refuse to continue a copy onto a device that is not the one it started on.
///
/// Two cards of the same model and size would otherwise be mixed into one
/// image without any error. Only a definite difference (serial number,
/// model, size) counts; weak identities are accepted.
pub fn check_same_device(before: &Fingerprint, now: &DeviceInfo, map: &Path) -> Result<()> {
    match before.matches(now) {
        Match::Mismatch(why) => Err(AppError::refused(format!(
            "the progress map {} was made while copying a different device ({why}). Copying a second \
             device into the same image would mix the two and ruin both. Use a new image name for this \
             device, or add --force if it really is the same device (for example if its serial number \
             changed after a reset).",
            map.display()
        ))),
        _ => Ok(()),
    }
}

/// The device identity stored next to a mapfile, if readable.
pub fn read_identity(path: &Path) -> Option<Fingerprint> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Store a device identity next to a mapfile (best effort).
pub fn write_identity(path: &Path, fp: &Fingerprint) {
    if let Ok(text) = serde_json::to_string_pretty(fp) {
        if let Err(e) = std::fs::write(path, text) {
            log::debug!("could not write {}: {e}", path.display());
        }
    }
}

// ---------------------------------------------------------------- checks
//
// Each check is one function so that `prepare` (stop at the first problem)
// and `preflight` (collect every problem) say exactly the same thing.

/// The image must not be the source itself.
pub(crate) fn check_image_not_source(plan: &ImagePlan) -> Result<()> {
    if same_file(&plan.source, &plan.image) {
        return Err(AppError::refused(format!(
            "{} is the source itself; the image must be a different file, on a different disk.",
            plan.image.display()
        )));
    }
    Ok(())
}

/// What is at the image path now: nothing, or a regular file.
pub(crate) fn image_metadata(plan: &ImagePlan) -> Result<Option<std::fs::Metadata>> {
    let image_meta = std::fs::metadata(&plan.image).ok();
    if let Some(m) = &image_meta {
        if m.is_dir() {
            return Err(AppError::invalid(format!(
                "{} is a folder. Give the image a file name inside it, for example {}.",
                plan.image.display(),
                plan.image.join("card.img").display()
            )));
        }
        if !m.is_file() {
            return Err(AppError::refused(format!(
                "{} exists and is not a regular file (is it a device?). The image must be a file; \
                 DiskRescue never copies one device directly onto another.",
                plan.image.display()
            )));
        }
    }
    Ok(image_meta)
}

/// The mapfile and the image must be two files.
pub(crate) fn check_map_path(plan: &ImagePlan, map_path: &Path) -> Result<()> {
    if same_file(map_path, &plan.image) || map_path == plan.image {
        return Err(AppError::invalid(
            "the mapfile and the image must be different files",
        ));
    }
    Ok(())
}

/// The statistics of an existing progress map (`None`: start a new copy).
pub(crate) fn prior_stats(map_path: &Path) -> Result<Option<Stats>> {
    let map_exists = std::fs::metadata(map_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    let prior: Option<Stats> = if map_exists {
        MapFile::load(map_path).ok().map(|m| m.map.stats())
    } else {
        None
    };
    if map_exists && prior.is_none() {
        return Err(AppError::failed(format!(
            "the progress map {} exists but cannot be read, so the copy cannot be resumed. If it is not \
             needed, move it away to start a new copy; if it matters, keep it and use a different image name.",
            map_path.display()
        )));
    }
    Ok(prior)
}

/// Never overwrite an existing image that has no progress map (unless forced).
pub(crate) fn check_no_clobber(
    plan: &ImagePlan,
    prior: Option<&Stats>,
    image_meta: Option<&std::fs::Metadata>,
    map_path: &Path,
) -> Result<()> {
    if prior.is_none() && image_meta.is_some_and(|m| m.len() > 0) && !plan.force {
        return Err(AppError::refused(format!(
            "{} already exists, but there is no progress map ({}) to resume from. DiskRescue will not \
             overwrite it. Choose a new image name, or add --force if the file can be replaced.",
            plan.image.display(),
            map_path.display()
        )));
    }
    Ok(())
}

/// A progress map that records copied data needs its image.
pub(crate) fn check_image_present(
    plan: &ImagePlan,
    prior: Option<&Stats>,
    image_meta: Option<&std::fs::Metadata>,
    map_path: &Path,
) -> Result<()> {
    if let Some(st) = prior {
        if image_meta.is_none() && st.rescued > 0 {
            return Err(AppError::refused(format!(
                "the progress map {} records {} already copied, but the image {} does not exist. \
                 Put the image back where it was to resume, or move the map away to start over.",
                map_path.display(),
                human::bytes(st.rescued),
                plan.image.display()
            )));
        }
    }
    Ok(())
}

/// The progress map says everything was copied already (and no part of the
/// source was asked for): nothing to do, the device need not be touched.
pub(crate) fn already_complete(
    plan: &ImagePlan,
    prior: Option<&Stats>,
    image_meta: Option<&std::fs::Metadata>,
) -> Option<Stats> {
    let domain_requested = plan.start.is_some() || plan.size.is_some();
    let st = prior?;
    if domain_requested {
        return None;
    }
    let image_len = image_meta.map(|m| m.len()).unwrap_or(0);
    (st.pending() == 0 && st.bad_sectors == 0 && image_len >= st.total() && st.total() > 0)
        .then_some(*st)
}

/// Room for the image on the output disk: `(needed, free)` when known.
pub(crate) fn check_space(
    plan: &ImagePlan,
    image_meta: Option<&std::fs::Metadata>,
    domain_len: u64,
    out_dir: &Path,
) -> Result<(u64, Option<u64>)> {
    let have_on_disk = image_meta.map(allocated_bytes).unwrap_or(0);
    let need = domain_len.saturating_sub(have_on_disk);
    let free = free_space(out_dir);
    if let Some(free) = free {
        if free < need {
            return Err(AppError::refused(format!(
                "there is not enough free space for the image. {} will hold {} but only {} is free \
                 on that disk. Choose a location with more room, for example an external drive: \
                 diskrescue image {} /path/on/another/disk/{}",
                plan.image.display(),
                human::bytes(need),
                human::bytes(free),
                shell_quote(&plan.source.display().to_string()),
                plan.image
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image.img".into())
            )));
        }
    }
    Ok((need, free))
}

/// Resuming must continue on the same device (unless forced).
pub(crate) fn check_identity(
    plan: &ImagePlan,
    prior: Option<&Stats>,
    info: &DeviceInfo,
    map_path: &Path,
) -> Result<()> {
    if prior.is_some() && !plan.force {
        if let Some(before) = read_identity(&identity_path(map_path)) {
            check_same_device(&before, info, map_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- prepare

/// How the caller wants the run set up.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// Stop flag for the device's re-attach wait (the engine itself stops
    /// through its [`diskrescue_imaging::Control`]).
    pub cancel: Option<Arc<AtomicBool>>,
    /// Ask the engine for block-map snapshots this often (live map displays).
    pub map_snapshot_interval: Option<Duration>,
}

/// An imaging run ready to start.
pub struct Ready {
    /// The engine, positioned to start or resume. Run it with
    /// [`Imager::run`] and turn the result into an [`ImagingDone`] with
    /// [`ImagingDone::from_summary`] or [`engine_error`].
    pub imager: Imager,
    /// The source as opened.
    pub info: DeviceInfo,
    /// The strategy in use.
    pub profile: ImagingProfile,
    /// The image file.
    pub image: PathBuf,
    /// The progress map.
    pub mapfile: PathBuf,
    /// True when an earlier copy is being continued.
    pub resumed: bool,
    /// Totals before this run.
    pub before: Stats,
    /// Bytes this copy covers (the whole source or the requested part).
    pub copy_size: u64,
    /// Things worth telling the user (already passed to `on_warnings`).
    pub warnings: Vec<String>,
    /// The device handle when the source is a block device.
    pub device: Option<Arc<DeviceSource>>,
    /// The command line written into the progress map.
    pub command_line: String,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready")
            .field("info", &self.info)
            .field("profile", &self.profile.name)
            .field("image", &self.image)
            .field("mapfile", &self.mapfile)
            .field("resumed", &self.resumed)
            .field("before", &self.before)
            .field("copy_size", &self.copy_size)
            .finish_non_exhaustive()
    }
}

/// The outcome of [`prepare`].
#[derive(Debug)]
pub enum Prepared {
    /// The progress map says the image is complete: nothing to do, the
    /// device was not opened.
    AlreadyComplete(Box<ImagingDone>),
    /// Everything checked; the engine is ready.
    Ready(Box<Ready>),
}

/// Check everything and open the source for an imaging run.
///
/// `on_warnings` receives the source's warnings as soon as it is open
/// (before any later check can fail), so that a front-end can show them
/// in the same order as the command line does.
pub fn prepare(
    plan: &ImagePlan,
    setup: &Setup,
    on_warnings: &mut dyn FnMut(&[String]),
) -> Result<Prepared> {
    // Where the image goes must not be the source.
    check_image_not_source(plan)?;
    let image_meta = image_metadata(plan)?;
    let map_path = plan.mapfile();
    check_map_path(plan, &map_path)?;

    // Resume or start: make sure we never clobber something by accident.
    let prior = prior_stats(&map_path)?;
    check_no_clobber(plan, prior.as_ref(), image_meta.as_ref(), &map_path)?;
    check_image_present(plan, prior.as_ref(), image_meta.as_ref(), &map_path)?;

    // Nothing to do? Do not even touch the device.
    if let Some(st) = already_complete(plan, prior.as_ref(), image_meta.as_ref()) {
        let info = source::describe(&plan.source)?;
        return Ok(Prepared::AlreadyComplete(Box::new(ImagingDone {
            stats: st,
            summary: None,
            info,
            image: plan.image.clone(),
            mapfile: map_path,
            already_complete: true,
        })));
    }

    // Open the source (refuses output on the source and mounted sources).
    let out_dir = parent_dir(&plan.image);
    let mut req = SourceRequest::reading(&plan.source);
    req.use_map = false;
    req.direct = true;
    req.allow_mounted = plan.allow_mounted;
    req.output = Some(&out_dir);
    req.cancel = setup.cancel.clone();
    req.timeout = Some(Duration::from_secs(30));
    let opened = source::open(&req)?;
    on_warnings(&opened.warnings);
    let info = opened.info.clone();
    let profile = build_profile(info.kind, &plan.tuning)?;
    let domain = build_domain(plan.start, plan.size, opened.shared.size())?;
    let domain_len = domain
        .map(|d| d.len())
        .unwrap_or_else(|| opened.shared.size());
    if let Some(dev) = &opened.device {
        dev.set_reattach_deadline(profile.wait_for_reattach);
    }

    // Room for the image.
    if !plan.no_space_check {
        check_space(plan, image_meta.as_ref(), domain_len, &out_dir)?;
    }

    // Resuming must continue on the same device.
    let id_path = identity_path(&map_path);
    let device_identity =
        (info.kind != DeviceKind::ImageFile).then(|| Fingerprint::of(&info, None));
    check_identity(plan, prior.as_ref(), &info, &map_path)?;

    if !out_dir.exists() {
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| AppError::failed(explain_io(&e, Some(&out_dir))))?;
    }
    if prior.is_none() && plan.force && image_meta.as_ref().is_some_and(|m| m.len() > 0) {
        // Replacing a file: unreadable areas must read back as zeros, not as old data.
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&plan.image)
            .map_err(|e| AppError::failed(explain_io(&e, Some(&plan.image))))?;
    }

    let command_line = plan.command_text();
    let mut opts = ImagingOptions::with_profile(profile.clone());
    opts.domain = domain;
    opts.start_reverse = plan.tuning.reverse;
    opts.command_line = Some(command_line.clone());
    opts.allow_existing_output = plan.force;
    opts.preallocate = domain.is_none() && opened.shared.size() >= (1 << 20);
    opts.map_snapshot_interval = setup.map_snapshot_interval;
    let imager = Imager::new(opened.shared.clone(), &plan.image, &map_path, opts).map_err(|e| match e {
        diskrescue_core::Error::InvalidArgument(m) if m.contains("mapfile") => AppError::failed(format!(
            "{m}. The progress map seems to belong to a different device or image: use another image \
             name to start a new copy."
        )),
        other => AppError::from(other),
    })?;
    let resumed = imager.is_resumed();
    let before = imager.stats();
    if let Some(fp) = &device_identity {
        if !resumed || plan.force {
            write_identity(&id_path, fp);
        }
    }
    Ok(Prepared::Ready(Box::new(Ready {
        imager,
        info,
        profile,
        image: plan.image.clone(),
        mapfile: map_path,
        resumed,
        before,
        copy_size: domain_len,
        warnings: opened.warnings,
        device: opened.device,
        command_line,
    })))
}

/// The error for an engine run that failed: what went wrong, and that the
/// progress so far is saved.
pub fn engine_error(map: &Path, e: diskrescue_core::Error) -> AppError {
    AppError::failed(format!(
        "{}. Progress up to this point is saved in {}; run the same command again to continue.",
        AppError::from(e).message(),
        map.display()
    ))
}

/// The error for an engine thread that panicked.
pub fn engine_panicked(map: &Path) -> AppError {
    AppError::failed(format!(
        "the imaging engine stopped unexpectedly (this is a bug). Progress up to the last save is in {}; \
         run the same command again to continue.",
        map.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_and_domain() {
        let t = Tuning {
            profile: Some("hdd".into()),
            block_size: Some(1 << 20),
            ..Tuning::default()
        };
        let p = build_profile(DeviceKind::UsbFlash, &t).unwrap();
        assert_eq!((p.name.as_str(), p.block_size), ("hdd", 1 << 20));
        let bad = Tuning {
            profile: Some("warp".into()),
            ..Tuning::default()
        };
        assert!(matches!(
            build_profile(DeviceKind::Hdd, &bad),
            Err(AppError::Invalid(_))
        ));
        assert_eq!(
            build_domain(Some(900), Some(500), 1000).unwrap(),
            Some(Range::new(900, 1000))
        );
        assert!(build_domain(Some(1000), None, 1000).is_err());
    }

    #[test]
    fn engine_errors_say_progress_is_saved() {
        let e = engine_error(
            Path::new("card.img.map"),
            diskrescue_core::Error::other("the device is gone"),
        );
        assert!(e.message().starts_with("the device is gone. Progress"));
        assert!(engine_panicked(Path::new("m")).message().contains("bug"));
    }
}
