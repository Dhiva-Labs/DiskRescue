//! Windows backend (best effort, UNVERIFIED on real hardware).
//!
//! Enumeration probes `\\.\PhysicalDrive0` .. `\\.\PhysicalDrive63` with
//! zero desired access (works without elevation for `FILE_ANY_ACCESS`
//! IOCTLs) and queries:
//!
//! * `IOCTL_STORAGE_QUERY_PROPERTY` / `StorageDeviceProperty`: bus type,
//!   removable flag, vendor/product/revision/serial strings;
//! * `StorageDeviceSeekPenaltyProperty`: `IncursSeekPenalty` -> rotational
//!   (unknown when the device does not answer, typical for USB bridges);
//! * `StorageAccessAlignmentProperty`: logical/physical sector sizes;
//! * `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`: size (works with zero access);
//!   `IOCTL_DISK_GET_LENGTH_INFO` once the disk is opened for reading.
//!
//! Mount points are the drive letters (`E:\`) of the volumes that lie on
//! the drive (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` on every `\\.\X:`,
//! zero access), so the system drive and drives Windows has in use are
//! recognised. Partitions and folder mount points are not collected (a note
//! says so).
//!
//! Reading uses `FILE_FLAG_NO_BUFFERING` with page-aligned buffers and positional
//! `seek_read`; opening for reading needs an elevated process.
//!
//! Only `DeviceIoControl` and `GetVolumePathNameW` come from `windows-sys`;
//! handles are opened through `std::fs::OpenOptions`.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use diskrescue_core::device::{DeviceInfo, DeviceKind, Transport};
use diskrescue_core::{Error, Result};
use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumePathNameW};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::winparse;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_ACCESS_ALIGNMENT_PROPERTY: u32 = 6;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;

const FILE_SHARE_READ: u32 = 1;
const FILE_SHARE_WRITE: u32 = 2;

/// Open a device for IOCTL queries only (no read access).
fn open_query(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

fn ioctl(file: &File, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<usize> {
    let mut returned: u32 = 0;
    // SAFETY: the handle is valid for the lifetime of `file`; the input and
    // output pointers and lengths describe live buffers; no OVERLAPPED is
    // used, so the call completes before returning.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            code,
            input.as_ptr() as *const c_void,
            input.len() as u32,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok((returned as usize).min(out.len()))
    }
}

fn query_property(file: &File, id: u32) -> io::Result<Vec<u8>> {
    let mut q = [0u8; 12];
    q[0..4].copy_from_slice(&id.to_le_bytes());
    // QueryType = PropertyStandardQuery (0).
    let mut header = [0u8; 8];
    ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut header)?;
    let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let size = size.clamp(8, 64 * 1024);
    let mut buf = vec![0u8; size];
    let n = ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, &q, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Size of an opened disk in bytes.
pub(crate) fn disk_length(file: &File) -> io::Result<u64> {
    let mut b = [0u8; 8];
    match ioctl(file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut b) {
        Ok(_) => winparse::parse_length_info(&b)
            .ok_or_else(|| io::Error::other("invalid GET_LENGTH_INFORMATION")),
        Err(e) => {
            let mut g = vec![0u8; 256];
            let n = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g).map_err(|_| e)?;
            winparse::parse_disk_geometry_ex(&g[..n])
                .map(|(s, _, _)| s)
                .ok_or_else(|| io::Error::other("invalid DISK_GEOMETRY_EX"))
        }
    }
}

/// `(logical, physical)` sector sizes of an opened disk.
pub(crate) fn sector_sizes(file: &File) -> Option<(u32, u32)> {
    if let Ok(b) = query_property(file, STORAGE_ACCESS_ALIGNMENT_PROPERTY) {
        if let Some((l, p)) = winparse::parse_access_alignment(&b) {
            if let Some(l) = crate::platform::sane_sector(l as u64) {
                let p = crate::platform::sane_sector(p as u64)
                    .filter(|p| *p >= l)
                    .unwrap_or(l);
                return Some((l, p));
            }
        }
    }
    let mut g = vec![0u8; 256];
    let n = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g).ok()?;
    let (_, bps, _) = winparse::parse_disk_geometry_ex(&g[..n])?;
    let l = crate::platform::sane_sector(bps as u64)?;
    Some((l, l))
}

fn drive_path(n: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\PhysicalDrive{n}"))
}

fn drive_number(info: &DeviceInfo) -> Option<u32> {
    info.name
        .strip_prefix("PhysicalDrive")
        .and_then(|d| d.parse().ok())
}

/// Physical drive numbers of the volume at `\\.\X:`.
fn volume_disks(letter: char) -> io::Result<Vec<u32>> {
    let file = open_query(Path::new(&format!(r"\\.\{letter}:")))?;
    let mut buf = vec![0u8; 8 + 24 * 64];
    let got = ioctl(&file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut buf)?;
    Ok(winparse::parse_volume_disk_extents(&buf[..got]))
}

/// `(drive number, X:\)` for every drive letter whose volume lies on a
/// physical drive. Letters without one (optical drives without a disc,
/// network drives, `subst`) are skipped.
fn drive_letters() -> Vec<(u32, PathBuf)> {
    // SAFETY: GetLogicalDrives takes no arguments and only returns a mask.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i);
        match volume_disks(letter) {
            Ok(disks) => {
                for n in disks {
                    out.push((n, PathBuf::from(format!(r"{letter}:\"))));
                }
            }
            Err(e) => log::debug!("{letter}: {e}"),
        }
    }
    out
}

fn describe_drive(path: &Path, name: &str, letters: &[(u32, PathBuf)]) -> io::Result<DeviceInfo> {
    let file = open_query(path)?;
    let desc = query_property(&file, STORAGE_DEVICE_PROPERTY)
        .ok()
        .and_then(|b| winparse::parse_storage_device_descriptor(&b))
        .unwrap_or_default();
    let seek = query_property(&file, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY)
        .ok()
        .and_then(|b| winparse::parse_seek_penalty(&b));
    let (logical, physical) = sector_sizes(&file).unwrap_or((512, 512));
    let mut g = vec![0u8; 256];
    let geo = ioctl(&file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut g)
        .ok()
        .and_then(|n| winparse::parse_disk_geometry_ex(&g[..n]));
    let size = geo.map(|(s, _, _)| s).unwrap_or(0);
    let (mut kind, transport, confidence) =
        winparse::classify_bus(desc.bus_type, seek, desc.removable);
    if desc.device_type == 5 {
        kind = DeviceKind::Optical;
    }
    let mut warnings = vec![
        "Windows backend (best effort): partitions are not listed; volumes are shown by drive letter"
            .to_string(),
    ];
    if size == 0 {
        let mut w = String::from("device reports no capacity: controller failure is likely");
        if desc.removable {
            w.push_str(" (or no card/medium is inserted)");
        }
        warnings.push(w);
    }
    if transport == Transport::Usb {
        warnings.push(format!(
            "USB bridges often misreport the medium type: detected as {} ({confidence} confidence); \
             override the device class if this is wrong",
            kind.display_name()
        ));
    }
    if let Some(s) = &desc.serial {
        if crate::fingerprint::is_generic_serial(s) {
            warnings.push(format!(
                "serial number '{s}' looks generic: after a reset the device is re-identified by model, size and first sector"
            ));
        }
    }
    let number: Option<u32> = name
        .strip_prefix("PhysicalDrive")
        .and_then(|d| d.parse().ok());
    let mountpoints: Vec<PathBuf> = letters
        .iter()
        .filter(|(n, _)| Some(*n) == number)
        .map(|(_, p)| p.clone())
        .collect();
    Ok(DeviceInfo {
        path: path.to_path_buf(),
        name: name.to_string(),
        kind,
        transport,
        size,
        logical_sector: logical,
        physical_sector: physical,
        model: desc.product,
        vendor: desc.vendor,
        serial: desc.serial,
        firmware: desc.revision,
        stable_id: None,
        removable: desc.removable,
        rotational: seek.unwrap_or(false),
        read_only: false,
        partitions: Vec::new(),
        mountpoints,
        health: None,
        warnings,
    })
}

/// Enumerate `\\.\PhysicalDrive0..63`.
pub(crate) fn enumerate() -> Result<Vec<DeviceInfo>> {
    let letters = drive_letters();
    let mut out = Vec::new();
    for n in 0..64u32 {
        let path = drive_path(n);
        match describe_drive(&path, &format!("PhysicalDrive{n}"), &letters) {
            Ok(i) => out.push(i),
            Err(e) if e.raw_os_error() == Some(2) => {}
            Err(e) => log::debug!("{}: {e}", path.display()),
        }
    }
    Ok(out)
}

/// Describe a `\\.\PhysicalDriveN` path.
pub(crate) fn describe(path: &Path) -> Result<DeviceInfo> {
    let s = path.to_string_lossy();
    let name = s.rsplit('\\').next().unwrap_or(&s).to_string();
    describe_drive(path, &name, &drive_letters()).map_err(|e| {
        if e.raw_os_error() == Some(2) {
            Error::NotFound(format!("{}: no such device", path.display()))
        } else {
            Error::Io(e)
        }
    })
}

fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated; `buf` has the length we pass.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf[..end])))
}

/// Whether `path` lives on the physical drive `info` (via the volume's
/// disk extents).
pub(crate) fn path_is_on_device(path: &Path, info: &DeviceInfo) -> Result<bool> {
    if info.kind == DeviceKind::ImageFile {
        // Writing next to an image is fine; writing onto it is not.
        let same = match (
            std::fs::canonicalize(path),
            std::fs::canonicalize(&info.path),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        return Ok(same);
    }
    let Some(n) = drive_number(info) else {
        return Err(Error::InvalidArgument(format!(
            "{} is not a \\\\.\\PhysicalDriveN device",
            info.path.display()
        )));
    };
    let mut cur = std::path::absolute(path).map_err(Error::Io)?;
    while !cur.exists() {
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    let root = volume_root(&cur).map_err(Error::Io)?;
    let letter = root.to_string_lossy().trim_end_matches('\\').to_string();
    let vol = PathBuf::from(format!(r"\\.\{letter}"));
    let file = open_query(&vol).map_err(Error::Io)?;
    let mut buf = vec![0u8; 8 + 24 * 64];
    let got =
        ioctl(&file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut buf).map_err(Error::Io)?;
    Ok(winparse::parse_volume_disk_extents(&buf[..got]).contains(&n))
}
