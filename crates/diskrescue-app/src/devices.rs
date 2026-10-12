//! Disks, cards and USB drives: listing, describing, plain-language advice
//! and the details page (health, partitions, filesystems).

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskrescue_core::device::{DeviceInfo, DeviceKind, HealthInfo};
use diskrescue_core::partition::{PartitionEntry, PartitionTable};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
use diskrescue_imaging::Stats;
use diskrescue_partition::FsProbe;

use crate::error::{AppError, Result};
use crate::source::{self, SourceRequest};

pub use crate::text::transport_label;
pub use diskrescue_device::device_label;

/// Disks, cards and USB drives connected to this computer, sorted by path.
/// Virtual devices (loop, device-mapper, RAM disks) are left out unless
/// `include_virtual`.
pub fn list(include_virtual: bool) -> Result<Vec<DeviceInfo>> {
    let devices = diskrescue_device::enumerate().map_err(AppError::from)?;
    Ok(visible_devices(devices, include_virtual))
}

/// Devices to show, sorted by path; virtual ones only when `all`.
pub fn visible_devices(mut devices: Vec<DeviceInfo>, all: bool) -> Vec<DeviceInfo> {
    if !all {
        devices.retain(|d| d.kind != DeviceKind::Virtual);
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    devices
}

/// Describe a device, partition or image file without opening it for
/// reading (see [`source::describe`]).
pub fn describe(path: &Path) -> Result<DeviceInfo> {
    source::describe(path)
}

/// Name of a kind of device (`USB flash drive`, `SD card`, `image file`).
pub fn kind_label(kind: DeviceKind) -> &'static str {
    kind.display_name()
}

fn system_mountpoints() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    #[cfg(unix)]
    {
        for p in ["/", "/boot", "/boot/efi", "/usr", "/var"] {
            v.push(PathBuf::from(p));
        }
    }
    #[cfg(windows)]
    {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        v.push(PathBuf::from(format!("{drive}\\")));
        v.push(PathBuf::from(drive));
    }
    v
}

/// True when the device holds the running operating system (`/`, `/boot`,
/// `/usr`, ... on Unix; the system drive on Windows). Recovering from it
/// while the system runs keeps writing to it.
pub fn is_system_disk(info: &DeviceInfo) -> bool {
    if info.kind == DeviceKind::ImageFile {
        return false;
    }
    let sys = system_mountpoints();
    let all = info
        .mountpoints
        .iter()
        .chain(info.partitions.iter().flat_map(|p| p.mountpoints.iter()));
    for m in all {
        let text = m.to_string_lossy();
        if sys.iter().any(|s| {
            let st = s.to_string_lossy();
            if cfg!(windows) {
                text.eq_ignore_ascii_case(&st)
            } else {
                text == st
            }
        }) {
            return true;
        }
    }
    false
}

/// Plain-language notes about a device, most important first: the system
/// disk, a failed controller (0 bytes), SSD TRIM, mounted, read-only, then
/// the device layer's own observations.
pub fn advice(info: &DeviceInfo) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if info.kind == DeviceKind::ImageFile {
        if info.size == 0 {
            out.push("This image file is empty: there is nothing to recover from it.".into());
        }
        out.extend(info.warnings.iter().cloned());
        return out;
    }
    if is_system_disk(info) {
        out.push(
            "THIS IS THE SYSTEM DISK: it holds the operating system that is running now, which writes to \
             it all the time. Recover from it only if you have no choice, and never write the results \
             to it. If your photos were deleted from this disk, shut the computer down and recover from \
             another system."
                .into(),
        );
    }
    if info.size == 0 {
        out.push(
            "This device reports a capacity of 0 bytes. Its controller has most likely failed; software \
             cannot read it. A data recovery lab can often still recover the data."
                .into(),
        );
    }
    if info.kind.is_flash() && !info.kind.is_card() && !info.kind.is_usb() {
        out.push(
            "This is an SSD: if the files were deleted, TRIM has usually erased them already. Files that \
             still exist, and files on a failing SSD, can be recovered."
                .into(),
        );
    }
    let mut mps: Vec<String> = info
        .mountpoints
        .iter()
        .map(|m| m.display().to_string())
        .collect();
    mps.sort();
    mps.dedup();
    if !mps.is_empty() {
        out.push(format!(
            "It is mounted at {}. Unmount (eject) it before copying it, so that nothing writes to it.",
            mps.join(", ")
        ));
    }
    if info.read_only {
        out.push(
            "The system has marked it read-only (a write-protect switch, or a worn-out card in permanent \
             read-only mode). Reading still works, and nothing can be written to it by accident."
                .into(),
        );
    }
    for w in &info.warnings {
        let covered = w.starts_with("device reports no capacity")
            || w.starts_with("device is marked read-only")
            || w.starts_with("mounted at ");
        if !covered && !out.contains(w) {
            out.push(w.clone());
        }
    }
    out
}

/// SMART data is only asked of real disks that did not report it already.
pub fn wants_smart(info: &DeviceInfo) -> bool {
    !matches!(
        info.kind,
        DeviceKind::ImageFile | DeviceKind::Virtual | DeviceKind::Optical
    ) && info.health.is_none()
}

/// SMART values as lines of text.
pub fn health_lines(h: &HealthInfo) -> Vec<String> {
    let mut v = Vec::new();
    match h.passed {
        Some(true) => v.push("the drive's own health check passed".to_string()),
        Some(false) => v.push("the drive's own health check FAILED: it is failing".to_string()),
        None => {}
    }
    let mut counters = Vec::new();
    if let Some(n) = h.reallocated_sectors {
        counters.push(format!("{n} reallocated sectors"));
    }
    if let Some(n) = h.pending_sectors {
        counters.push(format!("{n} pending sectors"));
    }
    if let Some(n) = h.uncorrectable_sectors {
        counters.push(format!("{n} uncorrectable sectors"));
    }
    if !counters.is_empty() {
        v.push(counters.join(", "));
    }
    let mut misc = Vec::new();
    if let Some(t) = h.temperature_c {
        misc.push(format!("{t} C"));
    }
    if let Some(n) = h.power_on_hours {
        misc.push(format!("{} hours powered on", human::count(n)));
    }
    if let Some(n) = h.percentage_used {
        misc.push(format!("{n}% of rated life used"));
    }
    if !misc.is_empty() {
        v.push(misc.join(", "));
    }
    v.extend(h.notes.iter().cloned());
    v
}

/// A partition and what the filesystem probe found in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// The table entry.
    pub entry: PartitionEntry,
    /// The filesystem probe, when something was recognised.
    pub probe: Option<FsProbe>,
}

/// Everything known about a source: the description, SMART health, the
/// mapfile of an image, the partition table and its filesystems.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceDetails {
    /// The description (from the opened source when it could be opened).
    pub info: DeviceInfo,
    /// SMART summary.
    pub health: Option<HealthInfo>,
    /// Mapfile attached to an image.
    pub mapfile: Option<PathBuf>,
    /// What that mapfile says.
    pub map_stats: Option<Stats>,
    /// The partition table, when the source could be read.
    pub table: Option<PartitionTable>,
    /// Partitions and their filesystems.
    pub volumes: Vec<VolumeInfo>,
    /// Notes from opening the source.
    pub notes: Vec<String>,
    /// Why the source or its partition table could not be read.
    pub open_error: Option<String>,
    /// The imaging profile recommended for the device kind.
    pub profile: ImagingProfile,
}

/// Describe `path` in depth (the `info` command). SMART is asked with an
/// 8 second limit. Opening can fail (permissions); the description is
/// still returned with `open_error` set.
pub fn details(path: &Path, map: Option<&Path>) -> Result<DeviceDetails> {
    let mut info = source::describe(path)?;
    let mut health = info.health.clone();
    if health.is_none() && wants_smart(&info) {
        health = diskrescue_device::smart::query_with_timeout(&info.path, Duration::from_secs(8));
    }

    let mut req = SourceRequest::reading(path);
    req.map = map;
    let mut open_error: Option<String> = None;
    let mut table: Option<PartitionTable> = None;
    let mut volumes: Vec<VolumeInfo> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut mapfile = None;
    let mut map_stats = None;
    match source::open(&req) {
        Ok(opened) => {
            info = opened.info.clone();
            notes.extend(opened.warnings.iter().cloned());
            mapfile = opened.mapfile.clone();
            map_stats = opened.map_stats;
            match diskrescue_partition::volumes(&opened.shared, opened.info.logical_sector) {
                Ok((t, v)) => {
                    table = Some(t);
                    volumes = v
                        .into_iter()
                        .map(|v| VolumeInfo {
                            entry: v.entry,
                            probe: v.probe,
                        })
                        .collect();
                }
                Err(e) => open_error = Some(format!("the partition table could not be read: {e}")),
            }
        }
        Err(AppError::Failed(m)) | Err(AppError::Refused(m)) => open_error = Some(m),
        Err(e) => return Err(e),
    }
    let profile = ImagingProfile::for_kind(info.kind);
    Ok(DeviceDetails {
        info,
        health,
        mapfile,
        map_stats,
        table,
        volumes,
        notes,
        open_error,
        profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::Transport;

    fn dev(kind: DeviceKind) -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from("/dev/sdb"),
            name: "sdb".into(),
            kind,
            transport: Transport::Ata,
            size: 1 << 30,
            logical_sector: 512,
            physical_sector: 512,
            model: None,
            vendor: None,
            serial: None,
            firmware: None,
            stable_id: None,
            removable: false,
            rotational: false,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![],
            health: None,
            warnings: vec![],
        }
    }

    #[test]
    fn advice_flags_what_matters() {
        let mut d = dev(DeviceKind::SataSsd);
        assert!(advice(&d).iter().any(|a| a.contains("TRIM")));
        d.size = 0;
        d.read_only = true;
        d.warnings
            .push("device reports no capacity: controller failure is likely".into());
        let a = advice(&d);
        assert!(a.iter().any(|x| x.contains("0 bytes")));
        assert!(a.iter().any(|x| x.contains("read-only")));
        assert!(!a
            .iter()
            .any(|x| x.starts_with("device reports no capacity")));
        #[cfg(unix)]
        {
            let mut sys = dev(DeviceKind::NvmeSsd);
            sys.mountpoints.push(PathBuf::from("/"));
            assert!(is_system_disk(&sys));
            assert!(advice(&sys)[0].contains("SYSTEM DISK"));
            sys.mountpoints = vec![PathBuf::from("/media/card")];
            assert!(!is_system_disk(&sys));
        }
    }

    #[test]
    fn virtual_devices_are_hidden() {
        let list = vec![dev(DeviceKind::Virtual), dev(DeviceKind::UsbFlash)];
        assert_eq!(visible_devices(list.clone(), false).len(), 1);
        assert_eq!(visible_devices(list, true).len(), 2);
    }

    #[test]
    fn details_of_an_image_file() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("blank.img");
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let d = details(&img, None).unwrap();
        assert_eq!(d.info.kind, DeviceKind::ImageFile);
        assert!(d.health.is_none());
        assert_eq!(d.profile.name, "generic");
    }
}
