//! Operating-system specifics of opening and reading a source.
//!
//! * Linux: `open(O_RDONLY | O_CLOEXEC | O_DIRECT)` with a buffered
//!   fallback when the kernel answers `EINVAL`; `BLKGETSIZE64`
//!   (fallback `lseek(SEEK_END)`), `BLKSSZGET`, `BLKPBSZGET`; `pread`;
//!   `posix_fadvise(DONTNEED)` after errors in buffered mode so retries
//!   reach the medium instead of the page cache.
//! * Windows (best effort, UNVERIFIED on hardware): `CreateFileW` through
//!   `std` with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN` and
//!   `FILE_SHARE_READ | FILE_SHARE_WRITE`; size from
//!   `IOCTL_DISK_GET_LENGTH_INFO`; sector sizes from
//!   `StorageAccessAlignmentProperty`; positional reads with
//!   `FileExt::seek_read`.
//! * Elsewhere: regular files only; devices are `Unsupported`.
//!
//! No call here ever requests write access or issues a state-changing
//! ioctl.

use std::fs::File;
use std::io;
use std::path::Path;

/// An opened source handle with its geometry.
#[derive(Debug)]
pub struct Opened {
    /// The read-only handle.
    pub file: File,
    /// Direct (unbuffered) I/O is in effect.
    pub direct: bool,
    /// The handle is a block device (as opposed to a regular file).
    pub is_block: bool,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size (alignment unit for direct reads).
    pub sector: u32,
    /// Physical sector size.
    pub physical_sector: u32,
}

/// How a failed read should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrClass {
    /// Interrupted: repeat the same call.
    Retry,
    /// The device or medium disappeared.
    Gone,
    /// The transport reported a timeout.
    Timeout,
    /// Direct I/O alignment rejected by the OS.
    Unaligned,
    /// Medium or transport error (EIO and everything unknown).
    Media,
}

/// A sector size the rest of the code can rely on: a power of two between
/// 512 and 64 KiB.
pub fn sane_sector(v: u64) -> Option<u32> {
    if (512..=65536).contains(&v) && v.is_power_of_two() {
        Some(v as u32)
    } else {
        None
    }
}

fn is_einval(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EINVAL)
    }
    #[cfg(windows)]
    {
        // ERROR_INVALID_PARAMETER
        e.raw_os_error() == Some(87)
    }
    #[cfg(not(any(unix, windows)))]
    {
        e.kind() == io::ErrorKind::InvalidInput
    }
}

/// Positional read on a handle (one system call; no retry loop).
pub fn pread(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional reads are not supported on this platform",
        ))
    }
}

/// Classify an OS error from a read. Conservative: anything unknown is a
/// media error (the engine marks the block bad and moves on).
pub fn classify_os_error(e: &io::Error, direct: bool) -> ErrClass {
    #[cfg(target_os = "linux")]
    {
        match e.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => return ErrClass::Retry,
            Some(libc::ENODEV) | Some(libc::ENXIO) | Some(libc::ENOMEDIUM) | Some(libc::ENOENT) => {
                return ErrClass::Gone
            }
            Some(libc::ETIMEDOUT) => return ErrClass::Timeout,
            Some(libc::EINVAL) if direct => return ErrClass::Unaligned,
            _ => {}
        }
    }
    #[cfg(windows)]
    {
        match e.raw_os_error() {
            // ERROR_DEVICE_NOT_CONNECTED, ERROR_NO_SUCH_DEVICE,
            // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_NOT_READY,
            // ERROR_NO_MEDIA_IN_DRIVE, ERROR_DEV_NOT_EXIST
            Some(1167) | Some(433) | Some(2) | Some(3) | Some(21) | Some(1112) | Some(55) => {
                return ErrClass::Gone
            }
            // ERROR_SEM_TIMEOUT
            Some(121) => return ErrClass::Timeout,
            Some(87) if direct => return ErrClass::Unaligned,
            _ => {}
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = direct;
    }
    match e.kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => ErrClass::Retry,
        io::ErrorKind::TimedOut => ErrClass::Timeout,
        io::ErrorKind::NotFound => ErrClass::Gone,
        _ => ErrClass::Media,
    }
}

/// Ask the kernel to drop cached pages of a range so the next read goes to
/// the medium (buffered mode only; a no-op where unsupported).
pub fn drop_cache(file: &File, offset: u64, len: u64) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: posix_fadvise only takes integers and an fd we own; it
        // does not touch memory.
        let r = unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                offset.min(i64::MAX as u64) as libc::off_t,
                len.min(i64::MAX as u64) as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            )
        };
        if r != 0 {
            log::trace!("posix_fadvise(DONTNEED) failed with {r}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, offset, len);
    }
}

/// Read one aligned sector at offset 0 to find out whether direct reads
/// with this alignment are accepted. Retries `EINTR`.
fn probe_direct(file: &File, sector: usize) -> io::Result<()> {
    let mut buf = crate::aligned::AlignedBuf::new(sector, crate::aligned::PAGE.max(sector));
    for _ in 0..16 {
        match pread(file, buf.slice_mut(sector), 0) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// For a regular file opened for direct I/O, find the alignment the
/// filesystem accepts (512, then 4096). Returns `None` when neither works.
fn file_direct_sector(file: &File) -> io::Result<Option<u32>> {
    for sector in [512usize, 4096] {
        match probe_direct(file, sector) {
            Ok(()) => return Ok(Some(sector as u32)),
            Err(e) if is_einval(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;

    #[cfg(any(
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))]
    const IOC_READ: u64 = 2 << 29;
    #[cfg(not(any(
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )))]
    const IOC_READ: u64 = 2 << 30;

    /// `_IOR(0x12, 114, size_t)`: device size in bytes (not exported by
    /// the `libc` crate).
    pub const BLKGETSIZE64: u64 =
        IOC_READ | ((std::mem::size_of::<usize>() as u64) << 16) | (0x12 << 8) | 114;

    fn ioctl_u64(file: &File, req: u64) -> io::Result<u64> {
        let mut v: u64 = 0;
        // SAFETY: the request writes exactly one u64 into `v`, which lives
        // for the duration of the call.
        let r = unsafe { libc::ioctl(file.as_raw_fd(), req as libc::Ioctl, &mut v as *mut u64) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(v)
        }
    }

    fn ioctl_int(file: &File, req: libc::Ioctl) -> io::Result<u64> {
        let mut v: libc::c_int = 0;
        // SAFETY: BLKSSZGET/BLKPBSZGET write one int (unsigned int for
        // BLKPBSZGET, same size) into `v`.
        let r = unsafe { libc::ioctl(file.as_raw_fd(), req, &mut v as *mut libc::c_int) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(v as u32 as u64)
        }
    }

    fn open_mode(path: &Path, want_direct: bool, fallback: bool) -> io::Result<(File, bool)> {
        if want_direct {
            match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(path)
            {
                Ok(f) => return Ok((f, true)),
                Err(e) if is_einval(&e) && fallback => {
                    log::warn!(
                        "direct I/O is not available on {} ({e}); falling back to buffered reads (the page cache may mask errors)",
                        path.display()
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok((std::fs::OpenOptions::new().read(true).open(path)?, false))
    }

    pub fn open_for_read(path: &Path, want_direct: bool, fallback: bool) -> io::Result<Opened> {
        let meta = std::fs::metadata(path)?;
        let ft = meta.file_type();
        let is_block = ft.is_block_device();
        if !is_block && !ft.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is neither a block device nor a regular file",
                    path.display()
                ),
            ));
        }
        let (mut file, mut direct) = open_mode(path, want_direct, fallback)?;
        let m2 = file.metadata()?;
        if m2.file_type().is_block_device() != is_block {
            return Err(io::Error::other(format!(
                "{} changed while it was being opened",
                path.display()
            )));
        }
        if is_block {
            let size = match ioctl_u64(&file, BLKGETSIZE64) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!(
                        "BLKGETSIZE64 failed on {}: {e}; using lseek",
                        path.display()
                    );
                    file.seek(SeekFrom::End(0))?
                }
            };
            let sector = ioctl_int(&file, libc::BLKSSZGET)
                .ok()
                .and_then(sane_sector)
                .unwrap_or(512);
            let physical_sector = ioctl_int(&file, libc::BLKPBSZGET)
                .ok()
                .and_then(sane_sector)
                .filter(|p| *p >= sector)
                .unwrap_or(sector);
            Ok(Opened {
                file,
                direct,
                is_block,
                size,
                sector,
                physical_sector,
            })
        } else {
            let mut sector = 512u32;
            if direct {
                match file_direct_sector(&file)? {
                    Some(s) => sector = s,
                    None if fallback => {
                        log::warn!(
                            "{}: the filesystem rejects aligned direct reads; using buffered reads",
                            path.display()
                        );
                        file = std::fs::OpenOptions::new().read(true).open(path)?;
                        direct = false;
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{}: direct I/O alignment not accepted", path.display()),
                        ))
                    }
                }
            }
            Ok(Opened {
                file,
                direct,
                is_block,
                size: m2.len(),
                sector,
                physical_sector: sector,
            })
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

    /// True for `\\.\PhysicalDriveN`, `\\.\X:` and other device-namespace paths.
    fn is_device_path(path: &Path) -> bool {
        let s = path.to_string_lossy();
        s.starts_with(r"\\.\") || s.starts_with(r"\\?\GLOBALROOT")
    }

    fn open_mode(path: &Path, direct: bool) -> io::Result<File> {
        let mut flags = FILE_FLAG_SEQUENTIAL_SCAN;
        if direct {
            flags |= FILE_FLAG_NO_BUFFERING;
        }
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)
    }

    pub fn open_for_read(path: &Path, want_direct: bool, fallback: bool) -> io::Result<Opened> {
        let is_file = std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        let is_block = !is_file && is_device_path(path);
        if !is_file && !is_block {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is neither a device path (\\\\.\\PhysicalDriveN) nor a regular file",
                    path.display()
                ),
            ));
        }
        let (mut file, mut direct) = match open_mode(path, want_direct) {
            Ok(f) => (f, want_direct),
            Err(e) if want_direct && is_einval(&e) && fallback => {
                log::warn!(
                    "unbuffered I/O is not available on {} ({e}); using buffered reads",
                    path.display()
                );
                (open_mode(path, false)?, false)
            }
            Err(e) => return Err(e),
        };
        if is_block {
            let size = crate::windows::disk_length(&file)?;
            let (sector, physical_sector) =
                crate::windows::sector_sizes(&file).unwrap_or((512, 512));
            Ok(Opened {
                file,
                direct,
                is_block,
                size,
                sector,
                physical_sector,
            })
        } else {
            let size = file.metadata()?.len();
            let mut sector = 512u32;
            if direct {
                match file_direct_sector(&file)? {
                    Some(s) => sector = s,
                    None if fallback => {
                        file = open_mode(path, false)?;
                        direct = false;
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "unbuffered I/O alignment not accepted",
                        ))
                    }
                }
            }
            Ok(Opened {
                file,
                direct,
                is_block,
                size,
                sector,
                physical_sector: sector,
            })
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    pub fn open_for_read(path: &Path, want_direct: bool, fallback: bool) -> io::Result<Opened> {
        let _ = (want_direct, fallback);
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "block devices are not supported on this platform; image the device with another tool and use the image file",
            ));
        }
        let file = std::fs::File::open(path)?;
        Ok(Opened {
            file,
            direct: false,
            is_block: false,
            size: meta.len(),
            sector: 512,
            physical_sector: 512,
        })
    }
}

pub use imp::open_for_read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_sanity() {
        assert_eq!(sane_sector(512), Some(512));
        assert_eq!(sane_sector(4096), Some(4096));
        assert_eq!(sane_sector(0), None);
        assert_eq!(sane_sector(520), None);
        assert_eq!(sane_sector(1 << 20), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ioctl_numbers() {
        // x86-64 and Arm64 share the generic ioctl encoding.
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert_eq!(imp::BLKGETSIZE64, 0x8008_1272);
        let e = io::Error::from_raw_os_error(libc::ENXIO);
        assert_eq!(classify_os_error(&e, true), ErrClass::Gone);
        let e = io::Error::from_raw_os_error(libc::ENOMEDIUM);
        assert_eq!(classify_os_error(&e, false), ErrClass::Gone);
        let e = io::Error::from_raw_os_error(libc::EIO);
        assert_eq!(classify_os_error(&e, true), ErrClass::Media);
        let e = io::Error::from_raw_os_error(libc::EINVAL);
        assert_eq!(classify_os_error(&e, true), ErrClass::Unaligned);
        assert_eq!(classify_os_error(&e, false), ErrClass::Media);
        let e = io::Error::from_raw_os_error(libc::EINTR);
        assert_eq!(classify_os_error(&e, false), ErrClass::Retry);
        let e = io::Error::from_raw_os_error(libc::ETIMEDOUT);
        assert_eq!(classify_os_error(&e, false), ErrClass::Timeout);
    }

    #[test]
    fn opens_regular_files_buffered_and_direct() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        std::fs::write(&p, vec![9u8; 5000]).unwrap();
        let o = open_for_read(&p, false, true).unwrap();
        assert!(!o.is_block && !o.direct);
        assert_eq!(o.size, 5000);
        // Direct may or may not be available on the test filesystem; either
        // way a valid mode must be chosen.
        let o = open_for_read(&p, true, true).unwrap();
        assert_eq!(o.size, 5000);
        assert!(o.sector == 512 || o.sector == 4096);
        assert!(open_for_read(dir.path(), false, true).is_err());
        assert!(open_for_read(&dir.path().join("missing"), false, true).is_err());
    }
}
