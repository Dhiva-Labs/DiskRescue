//! What the screens talk about: devices with their flags, the operation
//! about to run, and the words that explain a refusal.

use std::path::{Path, PathBuf};

use diskrescue_app::devices;
use diskrescue_app::plan::{
    CarveMode, Categories, ImagePlan, RecoverPlan, RepairPlan, ScanPlan, StateFilter, VerifyPlan,
};
use diskrescue_app::preflight::{Blocker, BlockerCode, Override};
use diskrescue_app::text::plural;
use diskrescue_core::device::{DeviceInfo, DeviceKind};

/// A device (or image file) with the flags the screens show.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceEntry {
    /// What the system reports.
    pub info: DeviceInfo,
    /// It holds the running operating system.
    pub system: bool,
    /// It, or a partition of it, is mounted.
    pub mounted: bool,
    /// Its own health check failed, it reports unreadable sectors, or it
    /// reports no capacity.
    pub failing: bool,
    /// Reading it needs administrator rights that this program lacks.
    pub needs_root: bool,
}

impl DeviceEntry {
    /// Work out the flags of `info`.
    pub fn new(info: DeviceInfo, needs_root: bool) -> Self {
        DeviceEntry {
            system: devices::is_system_disk(&info),
            mounted: info.is_mounted(),
            failing: is_failing(&info),
            needs_root,
            info,
        }
    }

    /// An image file.
    pub fn is_image(&self) -> bool {
        self.info.kind == DeviceKind::ImageFile
    }

    /// The flags as words, most important first.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.system {
            v.push("system disk");
        }
        if self.failing {
            v.push("failing");
        }
        if self.mounted {
            v.push("mounted");
        }
        if self.info.read_only {
            v.push("read-only");
        }
        // Nothing can be done with the system disk anyway.
        if self.needs_root && !self.system {
            v.push("needs root");
        }
        v
    }

    /// Model and vendor, or the kind of device.
    pub fn name(&self) -> String {
        let vendor = self.info.vendor.as_deref().map(str::trim).unwrap_or("");
        let model = self.info.model.as_deref().map(str::trim).unwrap_or("");
        let both = if model.to_lowercase().starts_with(&vendor.to_lowercase()) {
            model.to_string()
        } else {
            format!("{vendor} {model}")
        };
        let both = both.trim().to_string();
        if both.is_empty() {
            self.info.kind.display_name().to_string()
        } else {
            both
        }
    }
}

/// The drive's own health check failed, it counts unreadable sectors, or it
/// reports a capacity of 0 bytes.
pub fn is_failing(info: &DeviceInfo) -> bool {
    if info.kind == DeviceKind::ImageFile {
        return false;
    }
    if info.size == 0 {
        return true;
    }
    match &info.health {
        Some(h) => {
            h.passed == Some(false)
                || h.pending_sectors.unwrap_or(0) > 0
                || h.uncorrectable_sectors.unwrap_or(0) > 0
        }
        None => false,
    }
}

/// An operation ready to start.
#[derive(Debug, Clone)]
pub enum Plan {
    /// Copy a device into an image.
    Image(ImagePlan),
    /// Recover files; with `image_to`, copy the device first (a rescue).
    Recover(RecoverPlan),
    /// Preview what can be recovered.
    Scan(ScanPlan),
    /// Repair loose files.
    Repair(RepairPlan),
    /// Check loose files.
    Verify(VerifyPlan),
}

/// The blockers the person agreed to override.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    /// Read a mounted source.
    pub allow_mounted: bool,
    /// Replace an image or continue onto another device.
    pub force: bool,
    /// Skip the free-space check.
    pub skip_space: bool,
}

impl Overrides {
    /// The overrides that lift `blockers`.
    pub fn lifting(blockers: &[Blocker]) -> Self {
        let mut o = Overrides::default();
        for b in blockers {
            match b.override_flag {
                Some(Override::AllowMounted) => o.allow_mounted = true,
                Some(Override::Force) => o.force = true,
                Some(Override::SkipSpaceCheck) => o.skip_space = true,
                None => {}
            }
        }
        o
    }

    /// Nothing is overridden.
    pub fn none(&self) -> bool {
        *self == Overrides::default()
    }
}

impl Plan {
    /// A short heading (`Rescue of /dev/sdb`).
    pub fn title(&self) -> String {
        match self {
            Plan::Image(p) => format!("Copy of {}", short_path(&p.source)),
            Plan::Recover(p) if p.image_to.is_some() => {
                format!("Rescue of {}", short_path(&p.source))
            }
            Plan::Recover(p) => format!("Recovery from {}", short_path(&p.source)),
            Plan::Scan(p) => format!("Preview of {}", short_path(&p.source)),
            Plan::Repair(p) => format!(
                "Repair of {}",
                plural(p.files.len() as u64, "file", "files")
            ),
            Plan::Verify(p) => {
                format!("Check of {}", plural(p.files.len() as u64, "file", "files"))
            }
        }
    }

    /// The source device or image, when there is one.
    pub fn source(&self) -> Option<&Path> {
        match self {
            Plan::Image(p) => Some(&p.source),
            Plan::Recover(p) => Some(&p.source),
            Plan::Scan(p) => Some(&p.source),
            Plan::Repair(_) | Plan::Verify(_) => None,
        }
    }

    /// The folder the results go to.
    pub fn output(&self) -> Option<PathBuf> {
        match self {
            Plan::Recover(p) => Some(p.output.clone()),
            Plan::Repair(p) => Some(p.output.clone()),
            _ => None,
        }
    }

    /// What will happen, in plain words, one line each.
    pub fn describe(&self) -> Vec<String> {
        let mut v = Vec::new();
        match self {
            Plan::Image(p) => {
                v.push(format!(
                    "Copy {} to the image file {}.",
                    p.source.display(),
                    p.image.display()
                ));
                v.push(format!(
                    "The progress is saved in {}, so the copy can be stopped and continued at any time.",
                    p.mapfile().display()
                ));
                v.push(never_written(&p.source));
            }
            Plan::Recover(p) => {
                if let Some(img) = &p.image_to {
                    v.push(format!(
                        "Step 1: copy {} to the image file {}.",
                        p.source.display(),
                        img.display()
                    ));
                    v.push(
                        "Step 2: recover files from the image (the device is no longer used)."
                            .into(),
                    );
                } else if is_image_path(&p.source) {
                    v.push(format!(
                        "Recover files from the image {} (it is only read, never changed).",
                        p.source.display()
                    ));
                } else {
                    v.push(format!(
                        "Recover files directly from {} (no copy is made first).",
                        p.source.display()
                    ));
                }
                v.push(format!(
                    "Looking for: {}.",
                    selection_text(&p.categories, &p.state)
                ));
                v.push(format!("Raw data search: {}.", carve_text(p.carve)));
                v.push(if p.repair.no_repair {
                    "Damaged files are kept as they are (repair is off).".to_string()
                } else if p.repair.donors.is_empty() {
                    "Damaged files are repaired when possible.".to_string()
                } else {
                    format!(
                        "Damaged files are repaired when possible, using {} as references.",
                        plural(p.repair.donors.len() as u64, "donor file", "donor files")
                    )
                });
                v.push(format!("The results go to {}.", p.output.display()));
                if !is_image_path(&p.source) || p.image_to.is_some() {
                    v.push(never_written(&p.source));
                }
            }
            Plan::Scan(p) => {
                v.push(format!(
                    "Look at {} and list what could be recovered.",
                    p.source.display()
                ));
                v.push(format!(
                    "Looking for: {}.",
                    selection_text(&p.categories, &p.state)
                ));
                v.push(format!(
                    "Raw data search: {}.",
                    carve_text(p.carve.unwrap_or(CarveMode::Off))
                ));
                v.push("Nothing is written anywhere.".into());
            }
            Plan::Repair(p) => {
                v.push(format!(
                    "Try to repair {}.",
                    plural(p.files.len() as u64, "file", "files")
                ));
                if !p.donors.is_empty() {
                    v.push(format!(
                        "Healthy reference files: {}.",
                        p.donors
                            .iter()
                            .map(|d| d.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                v.push(format!(
                    "Repaired copies go to {}. The original files are never changed.",
                    p.output.join("repaired").display()
                ));
            }
            Plan::Verify(p) => {
                v.push(format!(
                    "Check whether {} are intact.",
                    plural(p.files.len() as u64, "file", "files")
                ));
                v.push("Nothing is written anywhere.".into());
            }
        }
        v
    }
}

/// A device path as it is; a file by its name only.
pub fn short_path(p: &Path) -> String {
    if is_image_path(p) {
        if let Some(n) = p.file_name() {
            return n.to_string_lossy().into_owned();
        }
    }
    p.display().to_string()
}

fn is_image_path(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn never_written(source: &Path) -> String {
    if is_image_path(source) {
        format!("{} is only read, never changed.", source.display())
    } else {
        format!(
            "Nothing is ever written to {}: it is only read.",
            source.display()
        )
    }
}

/// The kinds of files wanted, in words.
pub fn selection_text(c: &Categories, s: &StateFilter) -> String {
    let what = if c.all_files {
        "every kind of file".to_string()
    } else {
        let mut v = Vec::new();
        if c.photos || !(c.videos || c.raw || c.audio) {
            v.push("photos");
        }
        if c.raw || !(c.photos || c.videos || c.audio) {
            v.push("RAW photos");
        }
        if c.videos || !(c.photos || c.raw || c.audio) {
            v.push("videos");
        }
        if c.audio {
            v.push("audio");
        }
        v.join(", ")
    };
    let which = if s.existing_only {
        "files that still exist"
    } else if s.deleted_only {
        "deleted files"
    } else {
        "existing and deleted files"
    };
    format!("{what} ({which})")
}

/// A carving mode in words.
pub fn carve_text(m: CarveMode) -> &'static str {
    match m {
        CarveMode::Auto => "automatic (free space, plus anything the filesystem does not cover)",
        CarveMode::Free => "free space of readable filesystems only",
        CarveMode::All => "the whole source, ignoring filesystems (slow)",
        CarveMode::Off => "off",
    }
}

/// A preflight message without the command-line flags it mentions and
/// without the example commands, which mean nothing in this app.
pub fn plain_words(msg: &str) -> String {
    let mut text = msg.to_string();
    if let Some(i) = text.find(": diskrescue ") {
        text.truncate(i);
        text.push('.');
    }
    let mut out: Vec<String> = Vec::new();
    for sentence in split_sentences(&text) {
        if !sentence.contains("--") {
            out.push(sentence);
            continue;
        }
        // Keep what comes before ", or add --force ..." / "(or pass --x ...)".
        let flag = sentence.find("--").unwrap_or(0);
        let head = &sentence[..flag];
        let cut = [", or ", " (or "]
            .iter()
            .filter_map(|m| head.rfind(m))
            .max();
        if let Some(c) = cut {
            let kept = sentence[..c].trim_end_matches([',', ' ']).to_string();
            if !kept.is_empty() {
                out.push(format!("{kept}."));
            }
        }
    }
    out.join(" ")
}

/// Split into sentences, keeping the final full stop of each.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut depth = 0i32;
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        let end = matches!(c, '.' | '?' | '!')
            && depth <= 0
            && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if end {
            out.push(cur.trim().to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// What to do instead, for a blocker.
pub fn guidance(b: &Blocker, is_root: bool) -> String {
    use BlockerCode::*;
    let s = match b.code {
        SourceMissing => "Check the spelling, or choose the device again from the home screen.",
        SourceIsFolder => "Choose a device, a partition or an image file, not a folder.",
        PermissionDenied if is_root => {
            "Even with administrator rights this device cannot be opened. Check that nothing else is using it."
        }
        PermissionDenied => {
            "Reading a raw device needs administrator rights. Close DiskRescue and start it again with: sudo diskrescue-tui"
        }
        SourceUnreadable => {
            "Check that the device is still connected, then press r on the home screen to look again."
        }
        ZeroCapacity => {
            "Nothing can be done in software. A data recovery lab can often still get the data back."
        }
        SourceMounted => {
            "Safest: eject (unmount) it and try again, so nothing writes to it. Or continue anyway; the results may then be incomplete."
        }
        OutputOnSource => {
            "Choose a folder on a different disk. Saving onto the drive you recover from can destroy the very photos you are trying to save."
        }
        OutputUnverified => {
            "Choose an existing folder on a disk DiskRescue can identify, for example a folder in your home folder."
        }
        OutputIsSource => "Choose a new folder for the results.",
        OutputNotFolder => "That path is a file. Choose a folder for the results.",
        ImageIsSource => "Choose a different file name for the image, on a different disk.",
        ImageIsFolder => "That is a folder. Type a file name for the image, for example card.img.",
        ImageNotAFile => "Choose an ordinary file name for the image, not a device.",
        MapIsImage => "The progress map must be a different file than the image.",
        MapUnreadable => {
            "Move the damaged map away to start a new copy, or choose another image name."
        }
        MapMissing => "Check the path of the map file, or leave it out.",
        ImageExists => {
            "Choose a new image name. Or continue: the existing file is then replaced."
        }
        ImageMissing => {
            "Put the image back where it was to continue, or move the .map file away to start over."
        }
        DeviceChanged => {
            "Choose a new image name for this device. Continue only if it really is the same device."
        }
        NotEnoughSpace => {
            "Choose a folder on a disk with more free space. Continuing anyway can fill the disk and stop the copy."
        }
        InvalidOption => "Correct the setting in the form (under More options).",
        DonorMissing => "Remove that file from the list of donors, or choose another.",
        InputInvalid => "Choose existing files.",
    };
    s.to_string()
}

/// Words for the state of a path chosen as output, for the form.
pub fn default_output_name(info: &DeviceInfo) -> String {
    let n = if info.kind == DeviceKind::ImageFile {
        info.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| info.name.clone())
    } else {
        info.name.clone()
    };
    let cleaned: String = n
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "device".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_are_not_shown_to_people() {
        let m = "/dev/sdb is mounted at /media/x. Unmount it first so nothing writes to it while recovering (or pass --allow-mounted to continue anyway).";
        assert_eq!(
            plain_words(m),
            "/dev/sdb is mounted at /media/x. Unmount it first so nothing writes to it while recovering."
        );
        let m = "the progress map a.map was made while copying a different device (size differs). Copying a second device into the same image would mix the two and ruin both. Use a new image name for this device, or add --force if it really is the same device (for example if its serial number changed after a reset).";
        let p = plain_words(m);
        assert!(!p.contains("--"), "{p}");
        assert!(p.ends_with("Use a new image name for this device."), "{p}");
        let m = "a.img already exists, but there is no progress map (a.img.map) to resume from. DiskRescue will not overwrite it. Choose a new image name, or add --force if the file can be replaced.";
        assert!(plain_words(m).ends_with("Choose a new image name."));
        let m = "there is not enough free space for the image. a.img will hold 1 GiB but only 1 MiB is free on that disk. Choose a location with more room, for example an external drive: diskrescue image /dev/sdb /path/on/another/disk/a.img";
        let p = plain_words(m);
        assert!(p.ends_with("for example an external drive."), "{p}");
        assert!(!p.contains("diskrescue image"));
        assert_eq!(plain_words("nothing special."), "nothing special.");
    }

    #[test]
    fn what_to_look_for_in_words() {
        let none = Categories::default();
        assert_eq!(
            selection_text(&none, &StateFilter::default()),
            "photos, RAW photos, videos (existing and deleted files)"
        );
        let c = Categories {
            videos: true,
            audio: true,
            ..Categories::default()
        };
        let s = StateFilter {
            deleted_only: true,
            ..StateFilter::default()
        };
        assert_eq!(selection_text(&c, &s), "videos, audio (deleted files)");
        let c = Categories {
            all_files: true,
            ..Categories::default()
        };
        assert!(selection_text(&c, &StateFilter::default()).starts_with("every kind"));
    }

    #[test]
    fn output_names_are_safe() {
        let mut info = crate::testing::device("/dev/sdb", DeviceKind::UsbFlash, 1 << 30);
        assert_eq!(default_output_name(&info), "sdb");
        info.name = "my card!".into();
        assert_eq!(default_output_name(&info), "my_card_");
        info.kind = DeviceKind::ImageFile;
        info.path = PathBuf::from("/x/card one.img");
        assert_eq!(default_output_name(&info), "card_one");
    }
}
