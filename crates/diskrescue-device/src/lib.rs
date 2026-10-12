//! Block-device layer of DiskRescue: enumerate and classify devices, open
//! them read-only with direct I/O and per-read timeouts, and re-find them
//! after a USB reset.
//!
//! This is the only crate that touches a physical source. Its rules:
//!
//! * **Read-only.** Devices are opened `O_RDONLY` (Windows: `GENERIC_READ`)
//!   and no state-changing ioctl is ever issued (no `BLKROSET`,
//!   `BLKRRPART`, `BLKFLSBUF`, volume locks or dismounts).
//! * **Direct I/O by default** so cached data never masks read errors,
//!   with a logged fallback to buffered reads when the OS refuses it.
//! * **Every read has a timeout.** Reads run on abandonable worker threads
//!   ([`worker`]): a read stuck in the kernel is reported as a timeout and
//!   the next read gets a fresh worker ([`TimeoutSource`] offers the same
//!   for any [`Source`]).
//! * **Device loss is survivable.** `ENODEV`/`ENXIO`/`ENOMEDIUM` and
//!   end-of-data before the device size become
//!   [`ReadErrorKind::DeviceGone`](diskrescue_core::ReadErrorKind::DeviceGone);
//!   [`Source::reopen`] then waits for the same medium (by [`Fingerprint`])
//!   to come back, possibly under a new name, and refuses to resume on a
//!   different one.
//! * **Output guard and mount checks.** [`path_is_on_device`] tells whether
//!   an output path lives on the source (directly, through a partition, or
//!   through LVM/dm-crypt/md stacked on it); [`open_device`] refuses
//!   mounted sources unless [`OpenOptions::allow_mounted`].
//!
//! Platforms: Linux is first class (`sysfs`: sysfs, udev database,
//! mountinfo, block ioctls). Windows is best effort and UNVERIFIED on
//! hardware (`\\.\PhysicalDriveN` enumeration through storage IOCTLs,
//! unbuffered reads). Other systems can open image files only; device
//! functions return [`Error::Unsupported`].
//!
//! Classification ([`classify`]) follows `docs/specs/device-layer.md`
//! §3.2 and records a confidence and reasons; USB bridges misreport, so
//! callers may override the class ([`override_kind`]).

#![warn(missing_docs)]

pub mod aligned;
pub mod classify;
mod device_source;
mod fingerprint;
pub mod parse;
mod platform;
mod privilege;
pub mod smart;
#[cfg(target_os = "linux")]
pub mod sysfs;
mod timeout;
#[cfg(windows)]
mod windows;
pub mod winparse;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use diskrescue_core as core;
use diskrescue_core::device::{DeviceInfo, DeviceKind, PartitionInfo, Transport};
use diskrescue_core::source::{FileSource, SharedSource, Source};
use diskrescue_core::{Error, Result};

pub use classify::{Classification, Confidence};
pub use device_source::{
    device_label, label_size, narrow_first_failure, DeviceSource, OpenOptions,
    DEFAULT_REATTACH_WAIT, MAX_NARROW_PIECES, REATTACH_POLL,
};
pub use fingerprint::{hash_first_sector, is_generic_serial, Fingerprint, Match};
pub use privilege::privilege_hint;
pub use timeout::TimeoutSource;

/// Enumerate whole disks (partitions are listed inside each
/// [`DeviceInfo`]). Virtual devices (loop, dm, md, zram, ram, nbd) are
/// included and marked [`DeviceKind::Virtual`]; optical drives are
/// [`DeviceKind::Optical`].
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    #[cfg(target_os = "linux")]
    {
        sysfs::enumerate_with(&sysfs::Roots::system())
    }
    #[cfg(windows)]
    {
        windows::enumerate()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Err(Error::unsupported(
            "device enumeration is not implemented on this platform",
        ))
    }
}

/// Describe a device node (a partition node is reported with its parent
/// disk's class and identity, see [`partition_view`]) or a regular file
/// ([`DeviceKind::ImageFile`], size from its metadata).
pub fn describe(path: &Path) -> Result<DeviceInfo> {
    #[cfg(target_os = "linux")]
    {
        sysfs::describe_with(&sysfs::Roots::system(), path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Ok(m) = std::fs::metadata(path) {
            if m.is_file() {
                return Ok(image_file_info(path, m.len(), m.permissions().readonly()));
            }
        }
        #[cfg(windows)]
        {
            windows::describe(path)
        }
        #[cfg(not(windows))]
        {
            Err(Error::unsupported(
                "only image files are supported on this platform",
            ))
        }
    }
}

/// Re-find a device after re-enumeration (USB reset, re-plug): a unique
/// serial match (refused when the first sector reads back differently from
/// the fingerprint's hash), or, for several devices with the same serial
/// number and for weak identities, the device whose first sector has the
/// fingerprint's hash. Weak identities without a hash are never guessed
/// (`Ok(None)`). Partitions are candidates too.
pub fn find_by_fingerprint(fp: &Fingerprint) -> Result<Option<DeviceInfo>> {
    #[cfg(target_os = "linux")]
    {
        sysfs::find_by_fingerprint_with(&sysfs::Roots::system(), fp)
    }
    #[cfg(windows)]
    {
        let disks = windows::enumerate()?;
        let sel = device_source::select_candidate(
            fp,
            &device_source::Lost::default(),
            &disks,
            true,
            &|_| false,
        );
        Ok(device_source::resolve_selection(sel, fp))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = fp;
        Err(Error::unsupported(
            "device re-identification is not implemented on this platform",
        ))
    }
}

/// Open a block device (or a regular file, which then behaves as a plain
/// file: useful to exercise the imaging engine on image files) read-only.
/// Refuses mounted sources unless [`OpenOptions::allow_mounted`].
pub fn open_device(path: &Path, opts: &OpenOptions) -> Result<DeviceSource> {
    DeviceSource::open_with_ctx(path, opts, device_source::Ctx::system())
}

/// Open anything usable as a source: a device becomes a [`DeviceSource`],
/// a regular file a core [`FileSource`].
pub fn open_any(path: &Path, opts: &OpenOptions) -> Result<(SharedSource, DeviceInfo)> {
    let is_file = std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false);
    if is_file {
        let info = describe(path)?;
        if info.is_mounted() && !opts.allow_mounted {
            return Err(Error::Refused(format!(
                "{} is loop-mounted; unmount it first or allow mounted sources explicitly",
                path.display()
            )));
        }
        let src = FileSource::open(path).map_err(|e| match privilege_hint(&e, path) {
            Some(h) => Error::Io(std::io::Error::new(
                e.kind(),
                format!("cannot open {}: {e}. {h}", path.display()),
            )),
            None => Error::Io(e),
        })?;
        let shared: SharedSource = Arc::new(src);
        return Ok((shared, info));
    }
    let dev = open_device(path, opts)?;
    let info = dev.info();
    let shared: SharedSource = Arc::new(dev);
    Ok((shared, info))
}

/// Whether the device (any partition, anything stacked on it, a swap area
/// on it) or, for an image file, a loop device backed by it is mounted.
/// Re-checks the live mount table (automounters may have acted since the
/// description was taken).
pub fn is_mounted(info: &DeviceInfo) -> bool {
    #[cfg(target_os = "linux")]
    {
        sysfs::is_mounted_with(&sysfs::Roots::system(), info)
    }
    #[cfg(not(target_os = "linux"))]
    {
        info.is_mounted()
    }
}

/// Mountpoints of a device node (including its partitions and stacked
/// volumes), from `/proc/self/mountinfo`. For an image file: mountpoints of
/// loop devices backed by it.
pub fn mountpoints_of(path: &Path) -> Result<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        sysfs::mountpoints_of_with(&sysfs::Roots::system(), path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(Error::unsupported(
            "mount information is only available on Linux",
        ))
    }
}

/// Whether `path` (or its nearest existing ancestor, after following every
/// symbolic link, including a dangling one) is stored on the device
/// `info`: the output guard. For an image file: whether `path` is the
/// image itself or inside a filesystem mounted from it. On Linux see
/// `sysfs::path_is_on_device_with` for the rules; on Windows the volume's
/// disk extents are compared with the drive number. An error means the
/// question could not be answered; callers should treat it as "refuse".
pub fn path_is_on_device(path: &Path, info: &DeviceInfo) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        sysfs::path_is_on_device_with(&sysfs::Roots::system(), path, info)
    }
    #[cfg(windows)]
    {
        windows::path_is_on_device(path, info)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (path, info);
        Err(Error::unsupported(
            "the output-location check is not implemented on this platform",
        ))
    }
}

/// Override the detected class (USB bridges misreport); records a warning.
pub fn override_kind(info: &mut DeviceInfo, kind: DeviceKind) {
    if info.kind != kind {
        info.warnings.push(format!(
            "device class overridden: {} (detected as {})",
            kind.display_name(),
            info.kind.display_name()
        ));
        info.kind = kind;
    }
}

/// Description of a regular file used as a source.
pub fn image_file_info(path: &Path, size: u64, read_only: bool) -> DeviceInfo {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut warnings = Vec::new();
    if size == 0 {
        warnings.push("image file is empty".to_string());
    } else if size % 512 != 0 {
        warnings.push(
            "image size is not a multiple of 512 bytes (truncated or partial image?)".to_string(),
        );
    }
    DeviceInfo {
        path: path.to_path_buf(),
        name,
        kind: DeviceKind::ImageFile,
        transport: Transport::Unknown,
        size,
        logical_sector: 512,
        physical_sector: 512,
        model: None,
        vendor: None,
        serial: None,
        firmware: None,
        stable_id: None,
        removable: false,
        rotational: false,
        read_only,
        partitions: Vec::new(),
        mountpoints: Vec::new(),
        health: None,
        warnings,
    }
}

/// A partition presented as a source of its own: the parent disk's class,
/// transport and identity (model, vendor, serial, sector sizes) with the
/// partition's path, name, size, mountpoints and stable id, and
/// `partitions` holding exactly this partition.
pub fn partition_view(
    disk: &DeviceInfo,
    part: &PartitionInfo,
    stable_id: Option<String>,
) -> DeviceInfo {
    let path = part
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}{}", disk.path.display(), part.number)));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{}{}", disk.name, part.number));
    let mut warnings: Vec<String> = disk
        .warnings
        .iter()
        .filter(|w| !w.starts_with("mounted at"))
        .cloned()
        .collect();
    warnings.push(format!(
        "partition {} of {}: only this partition is read; imaging the whole disk also recovers files \
         from deleted or damaged partitions",
        part.number,
        disk.path.display()
    ));
    if !part.mountpoints.is_empty() {
        let list: Vec<String> = part
            .mountpoints
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        warnings.push(format!(
            "mounted at {}: unmount before imaging (a mounted filesystem may write to the device, and \
             reads can be inconsistent)",
            list.join(", ")
        ));
    }
    DeviceInfo {
        path,
        name,
        kind: disk.kind,
        transport: disk.transport,
        size: part.size,
        logical_sector: disk.logical_sector,
        physical_sector: disk.physical_sector,
        model: disk.model.clone(),
        vendor: disk.vendor.clone(),
        serial: disk.serial.clone(),
        firmware: disk.firmware.clone(),
        stable_id,
        removable: disk.removable,
        rotational: disk.rotational,
        read_only: disk.read_only,
        partitions: vec![part.clone()],
        mountpoints: part.mountpoints.clone(),
        health: disk.health.clone(),
        warnings,
    }
}

/// True when `info` is a [`partition_view`] rather than a whole disk.
pub fn is_partition_view(info: &DeviceInfo) -> bool {
    info.partitions.len() == 1 && info.partitions[0].path.as_deref() == Some(info.path.as_path())
}

/// Compile-time check that the sources implement the core trait.
#[allow(dead_code)]
fn assert_source_impls() {
    fn is_source<T: Source>() {}
    is_source::<DeviceSource>();
    is_source::<TimeoutSource<diskrescue_core::source::MemSource>>();
}
