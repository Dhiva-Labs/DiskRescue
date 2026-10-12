//! [`DeviceSource`]: a block device (or an image file) opened read-only
//! with direct I/O, per-read timeouts and re-attach handling.
//!
//! # Reading
//!
//! Every read runs on an abandonable worker thread (see [`crate::worker`])
//! that owns a page-aligned buffer, so `O_DIRECT` alignment of the memory
//! is always satisfied and a hung `pread` costs a thread, not the caller.
//! The caller's offset and length must be multiples of
//! [`Source::sector_size`] when direct I/O is in effect
//! ([`Source::requires_alignment`]); misaligned requests are rejected with
//! `Unaligned` before reaching the kernel.
//!
//! Errors are classified as follows:
//!
//! | OS condition | [`ReadErrorKind`] |
//! |---|---|
//! | `EINTR` / `EAGAIN` | retried (bounded) |
//! | `ENODEV`, `ENXIO`, `ENOMEDIUM`, `ENOENT` | `DeviceGone` |
//! | end of data before the device size (capacity dropped to 0) | `DeviceGone` |
//! | `ETIMEDOUT`, or no answer within the timeout | `Timeout` |
//! | `EINVAL` under direct I/O | `Unaligned` |
//! | `EIO` and everything else | `Io`, first failing offset narrowed |
//!
//! For `Io` the failed tail of the request is re-read in sector-sized
//! pieces (at most 128 pieces; larger requests use proportionally larger
//! pieces) and the error names the first piece that fails, so the caller
//! can keep the good prefix. The imaging engine does the finer
//! trimming/scraping itself. In buffered mode the cached pages of a failed
//! range are dropped (`posix_fadvise(DONTNEED)`) before re-reading.
//!
//! After a `DeviceGone` every read fails fast with `DeviceGone` until
//! [`Source::reopen`] succeeds.
//!
//! # Re-attach
//!
//! [`Source::reopen`] releases the handle (a worker still blocked in the
//! kernel keeps its own reference until it returns), then polls every
//! 500 ms, up to the deadline set with
//! [`DeviceSource::set_reattach_deadline`], for the same medium. The BLAKE3
//! hash of LBA 0 is recorded when the source is opened. A candidate is
//! accepted when:
//!
//! * it is still the very attach the source was opened on (Linux: same
//!   `diskseq`; the kernel bumps it on every re-attach and media change),
//!   e.g. a re-open after errors; or
//! * it is the only device with our unique serial number, size and sector
//!   size, and its first sector does not read back differently (a card
//!   reader keeps its serial number when another card is inserted); or
//! * its first sector has the recorded hash (several devices with the
//!   same serial number, or no usable serial number at all).
//!
//! Weak identities without a recorded hash are never guessed. A partition
//! source only matches the same partition number and start offset (sibling
//! partitions share the disk's serial number and may have equal sizes). It
//! refuses to resume (with `DeviceGone` and an explanation) when the same
//! serial number comes back with a different size or sector size or a
//! different first sector, or when a re-opened image file changed. The new
//! device must have exactly the original size and sector size.
//!
//! [`Source::path`] stays the path the source was opened from;
//! [`DeviceSource::current_path`] and [`Source::describe`] show where the
//! device lives now.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::source::{ReadError, ReadErrorKind, Source};
use diskrescue_core::{Error, Result};

use crate::aligned::PAGE;
use crate::fingerprint::{hash_first_sector, Fingerprint, Match};
use crate::platform::{self, ErrClass, Opened};
use crate::worker::{ReadBackend, WorkerPool};

/// Bounded `EINTR`/`EAGAIN` retries per system call.
const MAX_RETRIES: u32 = 64;

/// Upper bound of piece reads when narrowing an `EIO` down.
pub const MAX_NARROW_PIECES: usize = 128;

/// Poll interval while waiting for a device to come back.
pub const REATTACH_POLL: Duration = Duration::from_millis(500);

/// Default time to wait for a vanished device (USB resets take 2-10 s).
pub const DEFAULT_REATTACH_WAIT: Duration = Duration::from_secs(30);

/// Cap for reading LBA 0 to fingerprint a device.
const FIRST_SECTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// How to open a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOptions {
    /// Bypass the page cache (`O_DIRECT` / `FILE_FLAG_NO_BUFFERING`).
    /// Needed to see real device errors instead of cached data.
    pub direct: bool,
    /// Timeout of plain [`Source::read_at`] calls (and of `read_at_timeout`
    /// called with a zero timeout).
    pub default_timeout: Duration,
    /// Open even though the device (or a partition of it) is mounted.
    pub allow_mounted: bool,
    /// Fall back to buffered reads when the OS refuses direct I/O.
    pub fallback_buffered: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            direct: true,
            default_timeout: Duration::from_secs(30),
            allow_mounted: false,
            fallback_buffered: true,
        }
    }
}

/// Size as printed on a product label: decimal units, `32 GB`, `7.8 GB`.
pub fn label_size(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v >= 9.95 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// One-line label of a device: `SanDisk Ultra 32 GB, USB flash drive`.
pub fn device_label(info: &DeviceInfo) -> String {
    let vendor = info
        .vendor
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let model = info
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let name = match (vendor, model) {
        (Some(v), Some(m)) if m.to_ascii_lowercase().starts_with(&v.to_ascii_lowercase()) => {
            m.to_string()
        }
        (Some(v), Some(m)) => format!("{v} {m}"),
        (None, Some(m)) => m.to_string(),
        (Some(v), None) => v.to_string(),
        (None, None) => String::new(),
    };
    let size = label_size(info.size);
    let mut s = if name.is_empty() {
        format!("{size}, {}", info.kind.display_name())
    } else {
        format!("{name} {size}, {}", info.kind.display_name())
    };
    if crate::is_partition_view(info) {
        if let Some(p) = info.partitions.first() {
            s.push_str(&format!(", partition {}", p.number));
        }
    }
    s
}

/// Re-read `buf` (which starts at absolute offset `start`) in pieces of at
/// most one sector each, or larger pieces when that would exceed
/// `max_pieces` reads, and stop at the first piece that fails. Returns the
/// failing piece's error (its offset is the first byte that could not be
/// read), or `Ok(n)` when every piece read fine (`n < buf.len()` only at
/// end of data).
pub fn narrow_first_failure<F>(
    mut read: F,
    start: u64,
    buf: &mut [u8],
    sector: usize,
    max_pieces: usize,
) -> std::result::Result<usize, ReadError>
where
    F: FnMut(u64, &mut [u8]) -> std::result::Result<usize, ReadError>,
{
    let len = buf.len();
    if len == 0 {
        return Ok(0);
    }
    let sector = sector.max(1);
    let max_pieces = max_pieces.max(1);
    let mut piece = sector;
    if len.div_ceil(piece) > max_pieces {
        piece = len.div_ceil(max_pieces).div_ceil(sector) * sector;
    }
    let mut pos = 0usize;
    while pos < len {
        let end = (pos + piece).min(len);
        let want = end - pos;
        let n = read(start + pos as u64, &mut buf[pos..end])?;
        let n = n.min(want);
        pos += n;
        if n < want {
            return Ok(pos);
        }
    }
    Ok(len)
}

/// The handle a worker reads from.
pub(crate) struct DeviceBackend {
    opened: Opened,
    label: String,
}

impl DeviceBackend {
    pub(crate) fn new(opened: Opened, label: String) -> Self {
        DeviceBackend { opened, label }
    }

    fn error(&self, at: u64, len: usize, e: &std::io::Error, class: ErrClass) -> ReadError {
        let mut err = ReadError::from_io(at, len, e);
        err.kind = match class {
            ErrClass::Gone => ReadErrorKind::DeviceGone,
            ErrClass::Timeout => ReadErrorKind::Timeout,
            ErrClass::Unaligned => ReadErrorKind::Unaligned,
            ErrClass::Retry | ErrClass::Media => ReadErrorKind::Io,
        };
        err.detail = match class {
            ErrClass::Unaligned => format!(
                "{}: unaligned read rejected by the OS ({e}); is the sector size ({}) correct?",
                self.label, self.opened.sector
            ),
            ErrClass::Gone => format!("{}: device gone ({e})", self.label),
            _ => format!("{}: {e}", self.label),
        };
        err
    }

    /// Read until `buf` is full, end of data, or an error; retries
    /// interrupted calls. The error offset is the absolute offset of the
    /// failing system call.
    fn read_span(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        let sector = self.opened.sector.max(1) as usize;
        let mut done = 0usize;
        let mut retries = 0u32;
        while done < buf.len() {
            match platform::pread(&self.opened.file, &mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => {
                    done += n.min(buf.len() - done);
                    if self.opened.direct && done % sector != 0 && done < buf.len() {
                        // A partial direct read that is not sector aligned
                        // cannot be continued; report what we have.
                        break;
                    }
                }
                Err(e) => {
                    let class = platform::classify_os_error(&e, self.opened.direct);
                    if class == ErrClass::Retry && retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }
                    return Err(self.error(offset + done as u64, buf.len() - done, &e, class));
                }
            }
        }
        Ok(done)
    }
}

impl ReadBackend for DeviceBackend {
    fn read(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        let total = buf.len();
        match self.read_span(offset, buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind == ReadErrorKind::Io => {
                let rel = (e.offset.saturating_sub(offset) as usize).min(total);
                if !self.opened.direct {
                    platform::drop_cache(&self.opened.file, offset, total as u64);
                }
                let tail = &mut buf[rel..];
                let at = offset + rel as u64;
                let narrowed = narrow_first_failure(
                    |o, b| {
                        // The caller gave up on this read: stop hitting the
                        // device for a result nobody will use.
                        if crate::worker::current_read_abandoned() {
                            return Err(ReadError::new(
                                o,
                                b.len(),
                                ReadErrorKind::Timeout,
                                "read abandoned by its caller",
                            ));
                        }
                        self.read_span(o, b)
                    },
                    at,
                    tail,
                    self.opened.sector as usize,
                    MAX_NARROW_PIECES,
                );
                match narrowed {
                    Ok(n) => {
                        log::warn!(
                            "{}: read error at offset {} did not repeat when re-read piece by piece; using the re-read data",
                            self.label,
                            e.offset
                        );
                        Ok(rel + n)
                    }
                    Err(mut e2) => {
                        if !self.opened.direct {
                            platform::drop_cache(&self.opened.file, offset, total as u64);
                        }
                        e2.len = (offset + total as u64).saturating_sub(e2.offset) as usize;
                        log::debug!(
                            "{}: read error narrowed from offset {} to {}",
                            self.label,
                            e.offset,
                            e2.offset
                        );
                        Err(e2)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Where device descriptions come from (the sysfs roots on Linux).
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    #[cfg(target_os = "linux")]
    pub roots: crate::sysfs::Roots,
}

impl Ctx {
    pub(crate) fn system() -> Self {
        Ctx {
            #[cfg(target_os = "linux")]
            roots: crate::sysfs::Roots::system(),
        }
    }

    /// Per-attach sequence number of a device by kernel name (the parent
    /// disk's for a partition): changes whenever the device is re-attached
    /// or its medium changes. `None` where unknown.
    fn attach_seq(&self, name: &str) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            crate::sysfs::diskseq_of(&self.roots, name)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            None
        }
    }

    fn describe(&self, path: &Path) -> Result<DeviceInfo> {
        #[cfg(target_os = "linux")]
        {
            crate::sysfs::describe_with(&self.roots, path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::describe(path)
        }
    }

    /// Re-attach candidates: whole disks, or partition views when the
    /// source was a partition.
    fn candidates(&self, partitions: bool, include_virtual: bool) -> Result<Vec<DeviceInfo>> {
        #[cfg(target_os = "linux")]
        let disks = crate::sysfs::enumerate_with(&self.roots)?;
        #[cfg(windows)]
        let disks = crate::windows::enumerate()?;
        #[cfg(not(any(target_os = "linux", windows)))]
        let disks: Vec<DeviceInfo> = Vec::new();
        let disks: Vec<DeviceInfo> = disks
            .into_iter()
            .filter(|d| include_virtual || d.kind != DeviceKind::Virtual)
            .collect();
        if !partitions {
            return Ok(disks);
        }
        #[cfg(target_os = "linux")]
        {
            Ok(disks
                .iter()
                .flat_map(|d| crate::sysfs::partition_views_with(&self.roots, d))
                .collect())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(disks
                .iter()
                .flat_map(|d| {
                    d.partitions
                        .iter()
                        .map(|p| crate::partition_view(d, p, None))
                        .collect::<Vec<_>>()
                })
                .collect())
        }
    }
}

/// What to do with the candidates seen during one re-attach poll.
#[derive(Debug, Clone)]
pub(crate) enum Selection {
    /// This device is ours.
    Found(Box<DeviceInfo>),
    /// These devices could be ours; accept the one whose first sector has
    /// the recorded hash (and nothing else).
    Verify(Vec<DeviceInfo>),
    /// This device carries our unique serial number; refuse it when its
    /// first sector reads back with a different hash (a different medium
    /// behind the same reader or bridge), accept it otherwise.
    Confirm(Box<DeviceInfo>),
    /// A device came back in a state we must not resume on.
    Refuse(String),
    /// Nothing yet; keep waiting (with a note for the final message).
    Wait(Option<String>),
}

/// What is known about the lost source besides its [`Fingerprint`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Lost {
    /// Kernel name it had (`""` when unknown).
    pub name: String,
    /// For a partition source: its partition number and start offset on
    /// the disk. Sibling partitions share the disk's serial number and may
    /// have the same size, so these must match too.
    pub partition: Option<(u32, u64)>,
}

impl Lost {
    /// The lost source as described by `info`.
    pub(crate) fn of(info: &DeviceInfo) -> Self {
        Lost {
            name: info.name.clone(),
            partition: crate::is_partition_view(info)
                .then(|| info.partitions.first().map(|p| (p.number, p.start)))
                .flatten(),
        }
    }
}

fn serial_eq(fp: &Fingerprint, info: &DeviceInfo) -> bool {
    let a = fp.serial.as_deref().map(|s| s.trim().to_ascii_lowercase());
    let b = info
        .serial
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase());
    a.is_some() && a == b
}

/// Decide which candidate (if any) is the lost device. Pure apart from
/// `same_attach`, which says whether a candidate is still the very attach
/// the source was opened on (Linux: the same `diskseq`), i.e. the device
/// never went away. Rules:
///
/// * a candidate that is a different partition slot than a lost partition
///   is never considered;
/// * one exact match (unique serial, size, sector size) is taken when it
///   never went away, otherwise it must be confirmed by its first sector
///   when a hash was recorded ([`Selection::Confirm`]);
/// * several exact matches (cloned serial numbers) need the first-sector
///   hash to tell them apart;
/// * weak matches (no or generic serial) are taken only when the device
///   never went away or the first-sector hash confirms them; without a
///   hash they are never guessed;
/// * the same unique serial with another size or sector size is refused
///   (not for partitions: siblings have other sizes).
pub(crate) fn select_candidate(
    fp: &Fingerprint,
    lost: &Lost,
    candidates: &[DeviceInfo],
    partition_level: bool,
    same_attach: &dyn Fn(&DeviceInfo) -> bool,
) -> Selection {
    let original_name = lost.name.as_str();
    let mut exact: Vec<&DeviceInfo> = Vec::new();
    let mut probable: Vec<&DeviceInfo> = Vec::new();
    let mut refuse: Option<String> = None;
    let mut note: Option<String> = None;
    for c in candidates {
        if let Some(slot) = lost.partition {
            let theirs = c.partitions.first().map(|p| (p.number, p.start));
            if !crate::is_partition_view(c) || theirs != Some(slot) {
                if c.name == original_name {
                    note = Some(format!(
                        "{} is now a different partition (number/start changed)",
                        c.path.display()
                    ));
                }
                continue;
            }
        }
        match fp.matches(c) {
            Match::Exact => exact.push(c),
            Match::Probable(reason) => {
                log::debug!(
                    "re-attach candidate {}: probable ({reason})",
                    c.path.display()
                );
                probable.push(c);
            }
            Match::Mismatch(reason) => {
                if !partition_level && fp.has_strong_serial() && serial_eq(fp, c) {
                    refuse = Some(format!(
                        "{} came back as {} with the same serial number, but {reason}; refusing to resume on it \
                         (the controller may be failing or a different medium is inserted). Check the device and \
                         restart imaging: the mapfile keeps the progress",
                        original_name,
                        c.path.display()
                    ));
                } else if c.name == original_name {
                    note = Some(format!(
                        "{} now belongs to a different device ({reason})",
                        c.path.display()
                    ));
                }
            }
        }
    }
    let has_hash = fp.first_sector_hash.is_some();
    if !exact.is_empty() {
        let unchanged: Vec<&&DeviceInfo> = exact.iter().filter(|c| same_attach(c)).collect();
        if unchanged.len() == 1 {
            return Selection::Found(Box::new((*unchanged[0]).clone()));
        }
        if exact.len() == 1 {
            return if has_hash {
                Selection::Confirm(Box::new(exact[0].clone()))
            } else {
                Selection::Found(Box::new(exact[0].clone()))
            };
        }
        if has_hash {
            return Selection::Verify(exact.into_iter().cloned().collect());
        }
        return Selection::Wait(Some(format!(
            "{} devices carry the same serial number and size and no first-sector hash was recorded to tell them apart",
            exact.len()
        )));
    }
    if let Some(r) = refuse {
        return Selection::Refuse(r);
    }
    if !probable.is_empty() {
        let unchanged: Vec<&&DeviceInfo> = probable.iter().filter(|c| same_attach(c)).collect();
        if unchanged.len() == 1 {
            return Selection::Found(Box::new((*unchanged[0]).clone()));
        }
        if has_hash {
            return Selection::Verify(probable.into_iter().cloned().collect());
        }
        return Selection::Wait(Some(format!(
            "{} device(s) look like the lost one, but without a unique serial number or a readable first \
             sector the identity cannot be confirmed; check the device and restart (the mapfile keeps the \
             progress)",
            probable.len()
        )));
    }
    Selection::Wait(note)
}

/// Open `path` and read its first `sector` bytes with a timeout.
pub(crate) fn read_first_sector(
    path: &Path,
    sector: u32,
    direct: bool,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let opened = platform::open_for_read(path, direct, true).ok()?;
    let want = (sector.max(1) as u64).min(opened.size) as usize;
    if want == 0 {
        return None;
    }
    let job = if opened.direct {
        want.div_ceil(opened.sector as usize) * opened.sector as usize
    } else {
        want
    };
    let align = PAGE.max(opened.sector as usize);
    // Probe pools are short-lived; they share one hung-read counter so that
    // candidates whose first sector never answers cannot pile up threads
    // while a re-attach waits (possibly forever).
    let pool = WorkerPool::new(
        "probe",
        Arc::new(DeviceBackend::new(opened, path.display().to_string())),
        align,
    )
    .with_shared_hung_counter(probe_hung_counter())
    .with_max_hung(MAX_HUNG_PROBES);
    let mut buf = vec![0u8; want];
    match pool.read(0, job, &mut buf, timeout) {
        Ok(n) if n == want => Some(buf),
        _ => None,
    }
}

/// Probe reads (first sectors of re-attach candidates) that may be hung in
/// the kernel at the same time, process-wide.
const MAX_HUNG_PROBES: u64 = 4;

fn probe_hung_counter() -> Arc<AtomicU64> {
    static COUNTER: std::sync::OnceLock<Arc<AtomicU64>> = std::sync::OnceLock::new();
    COUNTER.get_or_init(|| Arc::new(AtomicU64::new(0))).clone()
}

/// What reading a candidate's first sector said about it.
enum Probe {
    /// The hash matches the recorded one.
    Same,
    /// The first sector reads back differently.
    Different,
    /// It could not be read (or no hash was recorded).
    Unknown,
}

fn probe(c: &DeviceInfo, fp: &Fingerprint) -> Probe {
    let Some(want) = fp.first_sector_hash.as_deref() else {
        return Probe::Unknown;
    };
    match read_first_sector(&c.path, fp.logical_sector, true, FIRST_SECTOR_TIMEOUT) {
        Some(b) if hash_first_sector(&b, fp.logical_sector as usize) == want => Probe::Same,
        Some(_) => Probe::Different,
        None => Probe::Unknown,
    }
}

/// Resolve a [`Selection`] into a device (reading first sectors for
/// [`Selection::Verify`] and [`Selection::Confirm`]). Refusals and waits
/// yield `None`.
pub(crate) fn resolve_selection(sel: Selection, fp: &Fingerprint) -> Option<DeviceInfo> {
    match sel {
        Selection::Found(i) => Some(*i),
        Selection::Verify(list) => list
            .into_iter()
            .take(8)
            .find(|c| matches!(probe(c, fp), Probe::Same)),
        Selection::Confirm(c) => match probe(&c, fp) {
            Probe::Different => None,
            Probe::Same | Probe::Unknown => Some(*c),
        },
        Selection::Refuse(_) | Selection::Wait(_) => None,
    }
}

struct State {
    info: DeviceInfo,
    fingerprint: Fingerprint,
    current_path: PathBuf,
    /// Attach sequence number (`diskseq`) of the device the handle is open
    /// on, when the platform has one.
    attach_seq: Option<u64>,
}

enum Attempt {
    Found(Box<(Opened, DeviceInfo, PathBuf)>),
    Wait(Option<String>),
    Refuse(String),
}

fn open_error(path: &Path, e: std::io::Error) -> Error {
    if let Some(hint) = crate::privilege_hint(&e, path) {
        return Error::Io(std::io::Error::new(
            e.kind(),
            format!("cannot open {}: {e}. {hint}", path.display()),
        ));
    }
    let gone = match e.raw_os_error() {
        #[cfg(target_os = "linux")]
        Some(libc::ENXIO) | Some(libc::ENODEV) | Some(libc::ENOMEDIUM) => true,
        _ => e.kind() == std::io::ErrorKind::NotFound,
    };
    if gone {
        return Error::NotFound(format!(
            "{}: {e} (device or medium not present)",
            path.display()
        ));
    }
    Error::Io(e)
}

/// A device opened read-only for imaging. See the module docs.
pub struct DeviceSource {
    opened_path: PathBuf,
    ctx: Ctx,
    opts: OpenOptions,
    size: u64,
    sector: u32,
    is_block: bool,
    partition_view: bool,
    direct: AtomicBool,
    pool: WorkerPool,
    state: Mutex<State>,
    gone: AtomicBool,
    reattach_wait: Mutex<Option<Duration>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    reattaches: AtomicU64,
    reopen_lock: Mutex<()>,
}

impl std::fmt::Debug for DeviceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSource")
            .field("path", &self.opened_path)
            .field("size", &self.size)
            .field("sector", &self.sector)
            .field("direct", &self.direct_io())
            .field("pool", &self.pool)
            .finish()
    }
}

impl DeviceSource {
    /// Open with an explicit description context (sysfs roots on Linux).
    pub(crate) fn open_with_ctx(path: &Path, opts: &OpenOptions, ctx: Ctx) -> Result<Self> {
        let mut info = ctx.describe(path)?;
        if info.is_mounted() && !opts.allow_mounted {
            let list: Vec<String> = info
                .mountpoints
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            return Err(Error::Refused(format!(
                "{} is mounted ({}): unmount it before imaging (a mounted filesystem can write to the device \
                 and reads may be inconsistent), or allow mounted sources explicitly",
                path.display(),
                list.join(", ")
            )));
        }
        let opened = platform::open_for_read(path, opts.direct, opts.fallback_buffered)
            .map_err(|e| open_error(path, e))?;
        if opened.is_block {
            if opened.size == 0 {
                return Err(Error::NotFound(format!(
                    "{}: device reports no capacity (no medium inserted, or the controller failed); nothing can be read",
                    path.display()
                )));
            }
            if info.size != opened.size {
                if info.size != 0 {
                    info.warnings.push(format!(
                        "the block device reports {} bytes but sysfs said {}; using {}",
                        opened.size, info.size, opened.size
                    ));
                }
                info.size = opened.size;
            }
        } else {
            info.size = opened.size;
        }
        // The alignment unit actually in effect (4 KiB for an image file on
        // a filesystem that needs it for direct I/O).
        info.logical_sector = opened.sector;
        info.physical_sector = opened.physical_sector.max(opened.sector);
        if opts.direct && !opened.direct {
            info.warnings.push(
                "direct I/O is unavailable: reads go through the page cache, which can hide read errors \
                 and serve stale data on retries"
                    .into(),
            );
        }
        let size = opened.size;
        let sector = opened.sector.max(1);
        let is_block = opened.is_block;
        let direct = opened.direct;
        let align = PAGE.max(sector as usize);
        let label = path.display().to_string();
        let pool = WorkerPool::new(
            info.name.clone(),
            Arc::new(DeviceBackend::new(opened, label)),
            align,
        );
        let fingerprint = Fingerprint::of(&info, None);
        let attach_seq = if is_block {
            ctx.attach_seq(&info.name)
        } else {
            None
        };
        let src = DeviceSource {
            opened_path: path.to_path_buf(),
            ctx,
            opts: opts.clone(),
            size,
            sector,
            is_block,
            partition_view: crate::is_partition_view(&info),
            direct: AtomicBool::new(direct),
            pool,
            state: Mutex::new(State {
                current_path: path.to_path_buf(),
                info,
                fingerprint,
                attach_seq,
            }),
            gone: AtomicBool::new(false),
            reattach_wait: Mutex::new(Some(DEFAULT_REATTACH_WAIT)),
            cancel: Mutex::new(None),
            reattaches: AtomicU64::new(0),
            reopen_lock: Mutex::new(()),
        };
        // The first-sector hash confirms the medium after a re-attach: it
        // tells apart devices without a unique serial number, and a
        // different card behind the same reader (whose serial number is the
        // reader's).
        if size > 0 {
            let t = src.opts.default_timeout.min(FIRST_SECTOR_TIMEOUT);
            if src.capture_first_sector(t).is_none() {
                log::warn!(
                    "{}: LBA 0 could not be read; after a reset the device can only be re-identified by a \
                     unique serial number",
                    path.display()
                );
            }
        }
        Ok(src)
    }

    /// Current description of the device (updated after a re-attach).
    pub fn info(&self) -> DeviceInfo {
        self.state
            .lock()
            .map(|s| s.info.clone())
            .unwrap_or_else(|p| p.into_inner().info.clone())
    }

    /// The identity used to re-find the device.
    pub fn fingerprint(&self) -> Fingerprint {
        self.state
            .lock()
            .map(|s| s.fingerprint.clone())
            .unwrap_or_else(|p| p.into_inner().fingerprint.clone())
    }

    /// Where the device lives now (differs from [`Source::path`] after a
    /// re-attach under a new name).
    pub fn current_path(&self) -> PathBuf {
        self.state
            .lock()
            .map(|s| s.current_path.clone())
            .unwrap_or_else(|p| p.into_inner().current_path.clone())
    }

    /// Whether direct I/O is in effect.
    pub fn direct_io(&self) -> bool {
        self.direct.load(Ordering::SeqCst)
    }

    /// True for block devices, false for image files opened through
    /// [`crate::open_device`].
    pub fn is_block_device(&self) -> bool {
        self.is_block
    }

    /// Timeout used by plain [`Source::read_at`].
    pub fn default_timeout(&self) -> Duration {
        self.opts.default_timeout
    }

    /// How long [`Source::reopen`] waits for the device to come back.
    /// `None` waits until cancelled (see [`DeviceSource::set_cancel_flag`]).
    /// The default is 30 seconds.
    pub fn set_reattach_deadline(&self, wait: Option<Duration>) {
        if let Ok(mut w) = self.reattach_wait.lock() {
            *w = wait;
        }
    }

    /// A flag that, when set, makes a waiting [`Source::reopen`] give up
    /// within ~50 ms (wire it to Ctrl-C).
    pub fn set_cancel_flag(&self, flag: Arc<AtomicBool>) {
        if let Ok(mut c) = self.cancel.lock() {
            *c = Some(flag);
        }
    }

    /// Reads abandoned because they exceeded their timeout (total).
    pub fn abandoned_workers(&self) -> u64 {
        self.pool.abandoned()
    }

    /// Abandoned reads still blocked in the kernel.
    pub fn hung_workers(&self) -> u64 {
        self.pool.hung()
    }

    /// Successful re-attaches so far.
    pub fn reattach_count(&self) -> u64 {
        self.reattaches.load(Ordering::SeqCst)
    }

    /// True after the device was found gone and before a successful reopen.
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }

    /// Read LBA 0 (guarded by `timeout`) and record its hash in the
    /// fingerprint. Returns the hash.
    pub fn capture_first_sector(&self, timeout: Duration) -> Option<String> {
        let want = (self.sector as u64).min(self.size) as usize;
        if want == 0 {
            return None;
        }
        let job = if self.direct_io() {
            self.sector as usize
        } else {
            want
        };
        let mut buf = vec![0u8; want];
        match self.pool.read(0, job, &mut buf, timeout) {
            Ok(n) if n == want => {
                let h = hash_first_sector(&buf, self.sector as usize);
                if let Ok(mut s) = self.state.lock() {
                    s.fingerprint.first_sector_hash = Some(h.clone());
                }
                Some(h)
            }
            _ => None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .lock()
            .ok()
            .and_then(|c| c.as_ref().map(|f| f.load(Ordering::SeqCst)))
            .unwrap_or(false)
    }

    fn gone_error(&self, offset: u64, len: usize, detail: String) -> ReadError {
        ReadError::new(offset, len, ReadErrorKind::DeviceGone, detail)
    }

    fn verify_opened(&self, opened: &Opened, path: &Path) -> std::result::Result<(), String> {
        if opened.size != self.size {
            return Err(format!(
                "{} came back with {} bytes instead of {}: refusing to resume on a different medium",
                path.display(),
                opened.size,
                self.size
            ));
        }
        if opened.is_block && opened.sector != self.sector {
            return Err(format!(
                "{} came back with {}-byte sectors instead of {}: refusing to resume",
                path.display(),
                opened.sector,
                self.sector
            ));
        }
        Ok(())
    }

    fn try_reattach_file(&self, fp: &Fingerprint) -> Attempt {
        let path = &self.opened_path;
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Attempt::Wait(Some(format!(
                    "{} is no longer a regular file",
                    path.display()
                )))
            }
            Err(e) => return Attempt::Wait(Some(format!("{}: {e}", path.display()))),
        }
        let opened =
            match platform::open_for_read(path, self.opts.direct, self.opts.fallback_buffered) {
                Ok(o) => o,
                Err(e) => return Attempt::Wait(Some(format!("{}: {e}", path.display()))),
            };
        if let Err(msg) = self.verify_opened(&opened, path) {
            return Attempt::Refuse(msg);
        }
        if let Some(want) = &fp.first_sector_hash {
            let t = self.opts.default_timeout.min(FIRST_SECTOR_TIMEOUT);
            match read_first_sector(path, self.sector, self.opts.direct, t) {
                Some(b) if hash_first_sector(&b, self.sector as usize) == *want => {}
                Some(_) => {
                    return Attempt::Refuse(format!(
                        "{} came back with a different first sector: refusing to resume on a different file",
                        path.display()
                    ))
                }
                None => return Attempt::Wait(Some("the first sector could not be read".into())),
            }
        }
        let mut info = self.info();
        info.size = opened.size;
        Attempt::Found(Box::new((opened, info, path.clone())))
    }

    fn try_reattach_device(&self, fp: &Fingerprint, original: &DeviceInfo) -> Attempt {
        let include_virtual = original.kind == DeviceKind::Virtual;
        let candidates = match self.ctx.candidates(self.partition_view, include_virtual) {
            Ok(c) => c,
            Err(e) => return Attempt::Wait(Some(format!("device scan failed: {e}"))),
        };
        let lost = Lost::of(original);
        let attach = self
            .state
            .lock()
            .map(|s| s.attach_seq)
            .unwrap_or_else(|p| p.into_inner().attach_seq);
        let same_attach =
            |c: &DeviceInfo| attach.is_some() && self.ctx.attach_seq(&c.name) == attach;
        let sel = select_candidate(fp, &lost, &candidates, self.partition_view, &same_attach);
        let found = match sel {
            Selection::Refuse(msg) => return Attempt::Refuse(msg),
            Selection::Wait(note) => return Attempt::Wait(note),
            Selection::Verify(list) => {
                let note = format!(
                    "{} candidate device(s) have a different (or unreadable) first sector",
                    list.len()
                );
                match resolve_selection(Selection::Verify(list), fp) {
                    Some(i) => i,
                    None => return Attempt::Wait(Some(note)),
                }
            }
            Selection::Confirm(c) => match probe(&c, fp) {
                Probe::Different => {
                    return Attempt::Refuse(format!(
                        "{} carries the serial number of {} but its first sector is different: a different \
                         medium (another card in the same reader?) or a controller returning wrong data; \
                         refusing to resume on it. Check the device and restart imaging: the mapfile keeps \
                         the progress",
                        c.path.display(),
                        self.opened_path.display()
                    ))
                }
                Probe::Same => *c,
                Probe::Unknown => {
                    log::warn!(
                        "{}: re-attaching by serial number; its first sector could not be read to confirm",
                        c.path.display()
                    );
                    *c
                }
            },
            Selection::Found(i) => *i,
        };
        let opened = match platform::open_for_read(
            &found.path,
            self.opts.direct,
            self.opts.fallback_buffered,
        ) {
            Ok(o) => o,
            Err(e) => {
                return Attempt::Wait(Some(format!(
                    "{} matches but cannot be opened yet: {e}",
                    found.path.display()
                )))
            }
        };
        if let Err(msg) = self.verify_opened(&opened, &found.path) {
            return Attempt::Refuse(msg);
        }
        let path = found.path.clone();
        Attempt::Found(Box::new((opened, found, path)))
    }

    fn install(&self, opened: Opened, info: DeviceInfo, path: PathBuf, waited: Duration) {
        let direct = opened.direct;
        let label = path.display().to_string();
        self.pool
            .attach(Arc::new(DeviceBackend::new(opened, label)));
        self.direct.store(direct, Ordering::SeqCst);
        let attach_seq = if self.is_block {
            self.ctx.attach_seq(&info.name)
        } else {
            None
        };
        if let Ok(mut s) = self.state.lock() {
            s.info = info;
            s.current_path = path.clone();
            s.attach_seq = attach_seq;
        }
        self.gone.store(false, Ordering::SeqCst);
        self.reattaches.fetch_add(1, Ordering::SeqCst);
        log::warn!(
            "{}: re-attached as {} after {:.1}s",
            self.opened_path.display(),
            path.display(),
            waited.as_secs_f64()
        );
    }
}

impl Source for DeviceSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        self.read_at_timeout(offset, buf, self.opts.default_timeout)
    }

    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> std::result::Result<usize, ReadError> {
        if buf.is_empty() || offset >= self.size {
            return Ok(0);
        }
        if self.is_gone() {
            return Err(self.gone_error(
                offset,
                buf.len(),
                format!(
                    "{} is gone; reopen() must succeed before reading again",
                    self.current_path().display()
                ),
            ));
        }
        let direct = self.direct_io();
        let sector = self.sector as u64;
        if direct && (offset % sector != 0 || buf.len() as u64 % sector != 0) {
            return Err(ReadError::unaligned(offset, buf.len(), self.sector));
        }
        let want = (self.size - offset).min(buf.len() as u64) as usize;
        let job = if direct {
            (want as u64).div_ceil(sector) as usize * sector as usize
        } else {
            want
        };
        let timeout = if timeout.is_zero() {
            self.opts.default_timeout
        } else {
            timeout
        };
        // Only a failure on the current handle may mark the device gone: a
        // read that started before a successful reopen used the old one.
        let generation = self.pool.generation();
        let mark_gone = || {
            if self.pool.generation() == generation {
                self.gone.store(true, Ordering::SeqCst);
            }
        };
        match self.pool.read(offset, job, &mut buf[..want], timeout) {
            Ok(n) => {
                if n < want && self.is_block {
                    mark_gone();
                    return Err(self.gone_error(
                        offset + n as u64,
                        want - n,
                        format!(
                            "{} returned end of data at offset {} although its size is {}: the device was probably removed",
                            self.current_path().display(),
                            offset + n as u64,
                            self.size
                        ),
                    ));
                }
                if offset == 0 && n as u64 >= sector.min(self.size) {
                    if let Ok(mut s) = self.state.lock() {
                        if s.fingerprint.first_sector_hash.is_none() {
                            s.fingerprint.first_sector_hash =
                                Some(hash_first_sector(&buf[..n], self.sector as usize));
                        }
                    }
                }
                Ok(n)
            }
            Err(e) => {
                if e.kind == ReadErrorKind::DeviceGone {
                    mark_gone();
                }
                Err(e)
            }
        }
    }

    fn describe(&self) -> String {
        let (info, cur) = match self.state.lock() {
            Ok(s) => (s.info.clone(), s.current_path.clone()),
            Err(p) => {
                let s = p.into_inner();
                (s.info.clone(), s.current_path.clone())
            }
        };
        let label = device_label(&info);
        if cur != self.opened_path {
            format!(
                "{} ({label}; re-attached, opened as {})",
                cur.display(),
                self.opened_path.display()
            )
        } else {
            format!("{} ({label})", cur.display())
        }
    }

    fn is_device(&self) -> bool {
        true
    }

    fn requires_alignment(&self) -> bool {
        self.direct_io()
    }

    fn reopen(&self) -> std::result::Result<(), ReadError> {
        // One re-attach at a time. A caller that waited while another
        // thread re-attached successfully is done.
        let generation = self.pool.generation();
        let _guard = self.reopen_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.pool.generation() != generation && !self.is_gone() {
            return Ok(());
        }
        self.pool.detach();
        self.gone.store(true, Ordering::SeqCst);
        let wait = self
            .reattach_wait
            .lock()
            .map(|w| *w)
            .unwrap_or(Some(DEFAULT_REATTACH_WAIT));
        let started = Instant::now();
        let deadline = wait.and_then(|w| started.checked_add(w));
        let fp = self.fingerprint();
        let original = self.info();
        let mut last_note: Option<String> = None;
        log::info!(
            "{}: closing and waiting for the device to answer again{}",
            self.opened_path.display(),
            match wait {
                Some(w) => format!(" (up to {:.0}s)", w.as_secs_f64()),
                None => " (until cancelled)".into(),
            }
        );
        loop {
            if self.cancelled() {
                return Err(self.gone_error(
                    0,
                    0,
                    "waiting for the device to come back was cancelled".into(),
                ));
            }
            let attempt = if self.is_block {
                self.try_reattach_device(&fp, &original)
            } else {
                self.try_reattach_file(&fp)
            };
            match attempt {
                Attempt::Found(found) => {
                    let (opened, info, path) = *found;
                    self.install(opened, info, path, started.elapsed());
                    return Ok(());
                }
                Attempt::Refuse(msg) => {
                    log::error!("{msg}");
                    return Err(self.gone_error(0, 0, msg));
                }
                Attempt::Wait(note) => {
                    if note.is_some() {
                        last_note = note;
                    }
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                let what = match wait {
                    Some(w) => format!("{:.1}s", w.as_secs_f64()),
                    None => "the allowed time".into(),
                };
                return Err(self.gone_error(
                    0,
                    0,
                    format!(
                        "{} did not come back within {what}{}",
                        self.opened_path.display(),
                        last_note.map(|n| format!(" ({n})")).unwrap_or_default()
                    ),
                ));
            }
            let mut slept = Duration::ZERO;
            while slept < REATTACH_POLL {
                if self.cancelled() {
                    break;
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
                slept += Duration::from_millis(50);
            }
        }
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.opened_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::Transport;

    #[test]
    fn labels() {
        assert_eq!(label_size(0), "0 B");
        assert_eq!(label_size(31_914_983_424), "32 GB");
        assert_eq!(label_size(7_800_000_000), "7.8 GB");
        assert_eq!(label_size(2_000_398_934_016), "2.0 TB");
        assert_eq!(label_size(500_107_862_016), "500 GB");
    }

    fn dev(name: &str, serial: Option<&str>, model: &str, size: u64) -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from(format!("/dev/{name}")),
            name: name.into(),
            kind: DeviceKind::UsbFlash,
            transport: Transport::Usb,
            size,
            logical_sector: 512,
            physical_sector: 512,
            model: Some(model.into()),
            vendor: Some("SanDisk".into()),
            serial: serial.map(String::from),
            firmware: None,
            stable_id: None,
            removable: true,
            rotational: true,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![],
            health: None,
            warnings: vec![],
        }
    }

    #[test]
    fn device_labels() {
        let d = dev("sdb", None, "Ultra", 32_000_000_000);
        assert_eq!(device_label(&d), "SanDisk Ultra 32 GB, USB flash drive");
        let mut d = dev("sdb", None, "SanDisk 3.2Gen1", 32_000_000_000);
        assert_eq!(device_label(&d), "SanDisk 3.2Gen1 32 GB, USB flash drive");
        d.model = None;
        d.vendor = None;
        assert_eq!(device_label(&d), "32 GB, USB flash drive");
    }

    #[test]
    fn narrowing_finds_first_bad_sector() {
        let bad = 512 * 37u64;
        let mut calls = 0;
        let mut buf = vec![0u8; 64 * 1024];
        let e = narrow_first_failure(
            |o, b| {
                calls += 1;
                if o <= bad && bad < o + b.len() as u64 {
                    Err(ReadError::new(o, b.len(), ReadErrorKind::Io, "EIO"))
                } else {
                    Ok(b.len())
                }
            },
            0,
            &mut buf,
            512,
            MAX_NARROW_PIECES,
        )
        .unwrap_err();
        assert_eq!(e.offset, bad);
        assert_eq!(calls, 38);
        // A large request is narrowed with bigger pieces, bounded.
        let mut calls = 0;
        let mut big = vec![0u8; 1 << 20];
        let e = narrow_first_failure(
            |o, b| {
                calls += 1;
                if o + b.len() as u64 > 900_000 {
                    Err(ReadError::new(o, b.len(), ReadErrorKind::Io, "EIO"))
                } else {
                    Ok(b.len())
                }
            },
            4096,
            &mut big,
            512,
            MAX_NARROW_PIECES,
        )
        .unwrap_err();
        assert!(calls <= MAX_NARROW_PIECES);
        assert!(e.offset <= 900_000 && e.offset % 512 == 0);
        // Transient error: everything reads fine on retry.
        let mut b = vec![0u8; 4096];
        assert_eq!(
            narrow_first_failure(|_, b| Ok(b.len()), 0, &mut b, 512, 128).unwrap(),
            4096
        );
        // End of data inside the range.
        assert_eq!(
            narrow_first_failure(
                |o, b| Ok(if o >= 1024 { 0 } else { b.len() }),
                0,
                &mut b,
                512,
                128
            )
            .unwrap(),
            1024
        );
        assert_eq!(
            narrow_first_failure(|_, _| Ok(0), 0, &mut [], 512, 128).unwrap(),
            0
        );
    }

    fn lost(name: &str) -> Lost {
        Lost {
            name: name.into(),
            partition: None,
        }
    }

    fn never(_: &DeviceInfo) -> bool {
        false
    }

    fn sel(fp: &Fingerprint, c: &[DeviceInfo], part: bool) -> Selection {
        select_candidate(fp, &lost("sdb"), c, part, &never)
    }

    #[test]
    fn selection_rules() {
        let orig = dev("sdb", Some("4C530001231012345678"), "Cruzer Blade", 1 << 30);
        let fp = Fingerprint::of(&orig, None);
        let moved = dev("sdc", Some("4C530001231012345678"), "Cruzer Blade", 1 << 30);
        let other = dev("sdb", Some("ZZZZ0001231012345678"), "Cruzer Blade", 1 << 30);
        match sel(&fp, &[other.clone(), moved.clone()], false) {
            Selection::Found(i) => assert_eq!(i.name, "sdc"),
            s => panic!("{s:?}"),
        }
        match sel(&fp, std::slice::from_ref(&other), false) {
            Selection::Wait(Some(n)) => assert!(n.contains("different device"), "{n}"),
            s => panic!("{s:?}"),
        }
        let shrunk = dev("sdc", Some("4C530001231012345678"), "Cruzer Blade", 0);
        match sel(&fp, std::slice::from_ref(&shrunk), false) {
            Selection::Refuse(m) => assert!(m.contains("size changed"), "{m}"),
            s => panic!("{s:?}"),
        }
        // Sibling partitions have other sizes: no refusal there.
        assert!(matches!(sel(&fp, &[shrunk], true), Selection::Wait(_)));
        // With a recorded first sector, a unique serial is still confirmed
        // (another card behind the same reader has the reader's serial).
        let mut fph = fp.clone();
        fph.first_sector_hash = Some("abc".into());
        assert!(matches!(
            sel(&fph, std::slice::from_ref(&moved), false),
            Selection::Confirm(_)
        ));
        // ...unless it is still the very same attach.
        let same = |c: &DeviceInfo| c.name == "sdc";
        assert!(matches!(
            select_candidate(
                &fph,
                &lost("sdb"),
                std::slice::from_ref(&moved),
                false,
                &same
            ),
            Selection::Found(_)
        ));
        // Two devices with the same serial and size: never a guess.
        let twin = dev("sdd", Some("4C530001231012345678"), "Cruzer Blade", 1 << 30);
        match sel(&fp, &[moved.clone(), twin.clone()], false) {
            Selection::Wait(Some(n)) => assert!(n.contains("same serial"), "{n}"),
            s => panic!("{s:?}"),
        }
        match sel(&fph, &[moved.clone(), twin.clone()], false) {
            Selection::Verify(v) => assert_eq!(v.len(), 2),
            s => panic!("{s:?}"),
        }
        // Weak identity: without a hash nothing is guessed, not even a
        // single candidate; with a hash candidates are verified; a device
        // that never went away is taken as is.
        let weak = dev("sdb", None, "USB DISK", 8 << 30);
        let mut fp = Fingerprint::of(&weak, None);
        let c1 = dev("sdd", None, "USB DISK", 8 << 30);
        let c2 = dev("sde", None, "USB DISK", 8 << 30);
        assert!(matches!(
            sel(&fp, std::slice::from_ref(&c1), false),
            Selection::Wait(Some(_))
        ));
        assert!(matches!(
            sel(&fp, &[c1.clone(), c2.clone()], false),
            Selection::Wait(Some(_))
        ));
        let unchanged = |c: &DeviceInfo| c.name == "sdd";
        assert!(matches!(
            select_candidate(&fp, &lost("sdb"), &[c1.clone(), c2.clone()], false, &unchanged),
            Selection::Found(i) if i.name == "sdd"
        ));
        fp.first_sector_hash = Some("abc".into());
        match sel(&fp, &[c1, c2], false) {
            Selection::Verify(v) => assert_eq!(v.len(), 2),
            s => panic!("{s:?}"),
        }
        assert!(matches!(sel(&fp, &[], false), Selection::Wait(None)));
    }

    fn part(disk: &DeviceInfo, n: u32, start: u64, size: u64) -> DeviceInfo {
        let p = diskrescue_core::device::PartitionInfo {
            path: Some(PathBuf::from(format!("{}{n}", disk.path.display()))),
            number: n,
            start,
            size,
            fs_type: None,
            mountpoints: vec![],
        };
        crate::partition_view(disk, &p, None)
    }

    #[test]
    fn partition_sources_never_switch_to_a_sibling() {
        // Two partitions of the same size: the first one (sdc1) used to be
        // taken for a lost sdb2.
        let disk = dev("sdb", Some("4C530001231012345678"), "Cruzer Blade", 4 << 30);
        let lost_p2 = part(&disk, 2, 2 << 30, 1 << 30);
        let fp = Fingerprint::of(&lost_p2, None);
        let lost2 = Lost::of(&lost_p2);
        assert_eq!(lost2.partition, Some((2, 2 << 30)));
        let back = dev("sdc", Some("4C530001231012345678"), "Cruzer Blade", 4 << 30);
        let c1 = part(&back, 1, 1 << 30, 1 << 30);
        let c2 = part(&back, 2, 2 << 30, 1 << 30);
        match select_candidate(&fp, &lost2, &[c1.clone(), c2.clone()], true, &never) {
            Selection::Found(i) => assert_eq!(i.name, "sdc2"),
            s => panic!("{s:?}"),
        }
        // Only the wrong slot is there: wait.
        assert!(matches!(
            select_candidate(&fp, &lost2, std::slice::from_ref(&c1), true, &never),
            Selection::Wait(_)
        ));
        // The slot moved on the disk (re-partitioned): not ours.
        let moved = part(&back, 2, 3 << 30, 1 << 30);
        assert!(matches!(
            select_candidate(&fp, &lost2, &[moved], true, &never),
            Selection::Wait(_)
        ));
        // Without slot information (public find_by_fingerprint) the twins
        // are ambiguous and are never guessed.
        assert!(matches!(
            select_candidate(&fp, &Lost::default(), &[c1, c2], true, &never),
            Selection::Wait(Some(_))
        ));
    }

    /// A minimal fake `/sys/block/<name>` with a SCSI-like `device/` and a
    /// regular file standing in for the device node (`dev/<name>`).
    #[cfg(target_os = "linux")]
    mod fake {
        use std::fs;
        use std::path::Path;

        pub const SECTORS: u64 = 64;

        pub fn data(fill: u8) -> Vec<u8> {
            let mut v: Vec<u8> = (0..SECTORS as usize * 512)
                .map(|i| (i % 251) as u8)
                .collect();
            v[..512].fill(fill);
            v
        }

        pub fn disk(root: &Path, name: &str, serial: Option<&str>, seq: u64, content: &[u8]) {
            let b = root.join("sys/block").join(name);
            fs::create_dir_all(b.join("queue")).unwrap();
            fs::write(b.join("size"), format!("{SECTORS}\n")).unwrap();
            fs::write(b.join("dev"), "8:16\n").unwrap();
            fs::write(b.join("diskseq"), format!("{seq}\n")).unwrap();
            fs::write(b.join("removable"), "1\n").unwrap();
            fs::write(b.join("queue/logical_block_size"), "512\n").unwrap();
            let d = root.join("sys/devices/scsi").join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("vendor"), "SanDisk \n").unwrap();
            fs::write(d.join("model"), "Cruzer Blade    \n").unwrap();
            if let Some(s) = serial {
                let mut p = vec![0u8, 0x80, 0, s.len() as u8];
                p.extend_from_slice(s.as_bytes());
                fs::write(d.join("vpd_pg80"), p).unwrap();
            }
            let _ = fs::remove_file(b.join("device"));
            std::os::unix::fs::symlink(&d, b.join("device")).unwrap();
            fs::create_dir_all(root.join("dev")).unwrap();
            fs::write(root.join("dev").join(name), content).unwrap();
        }

        /// Two partitions of 16 sectors each, at sectors 16 and 32.
        pub fn partitions(root: &Path, disk: &str, content: &[u8]) {
            for (n, start) in [(1u32, 16u64), (2, 32)] {
                let pname = format!("{disk}{n}");
                let p = root.join("sys/block").join(disk).join(&pname);
                fs::create_dir_all(&p).unwrap();
                fs::write(p.join("partition"), format!("{n}\n")).unwrap();
                fs::write(p.join("start"), format!("{start}\n")).unwrap();
                fs::write(p.join("size"), "16\n").unwrap();
                fs::write(p.join("dev"), format!("8:{}\n", 16 + n)).unwrap();
                let s = start as usize * 512;
                fs::write(root.join("dev").join(&pname), &content[s..s + 16 * 512]).unwrap();
            }
        }

        pub fn remove(root: &Path, name: &str) {
            fs::remove_dir_all(root.join("sys/block").join(name)).unwrap();
            let _ = fs::remove_file(root.join("dev").join(name));
            for n in 1..=2 {
                let _ = fs::remove_file(root.join("dev").join(format!("{name}{n}")));
            }
        }
    }

    /// Open `name` under the fake roots as if it were a block device.
    #[cfg(target_os = "linux")]
    fn open_fake(root: &Path, name: &str) -> DeviceSource {
        let roots = crate::sysfs::Roots::under(root);
        let opts = OpenOptions {
            direct: false,
            default_timeout: Duration::from_secs(5),
            allow_mounted: false,
            fallback_buffered: true,
        };
        let mut src = DeviceSource::open_with_ctx(
            &roots.dev.join(name),
            &opts,
            Ctx {
                roots: roots.clone(),
            },
        )
        .unwrap();
        let info = crate::sysfs::describe_name_with(&roots, name).unwrap();
        src.is_block = true;
        src.partition_view = crate::is_partition_view(&info);
        let first = std::fs::read(roots.dev.join(name)).unwrap();
        let mut s = src.state.lock().unwrap();
        s.fingerprint = Fingerprint::of(&info, Some(&first[..512]));
        s.attach_seq = crate::sysfs::diskseq_of(&roots, name);
        s.info = info;
        drop(s);
        src.set_reattach_deadline(Some(Duration::from_millis(1500)));
        src
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn device_reattach_follows_the_medium_not_the_name() {
        use diskrescue_core::source::SourceExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ours = fake::data(0x11);
        fake::disk(root, "sdb", Some("4C530001231012345678"), 5, &ours);
        let src = open_fake(root, "sdb");
        assert!(src.fingerprint().has_strong_serial());

        // Still there, same attach: a plain re-open.
        src.reopen().unwrap();
        assert_eq!(src.current_path(), root.join("dev/sdb"));

        // Reset: gone from sdb, back as sdc with the same medium.
        fake::remove(root, "sdb");
        fake::disk(root, "sdc", Some("4C530001231012345678"), 9, &ours);
        src.reopen().unwrap();
        assert_eq!(src.current_path(), root.join("dev/sdc"));
        assert_eq!(src.read_exact_vec(512, 8).unwrap(), ours[512..520].to_vec());

        // Another card in the same reader (same serial, same size, other
        // content): refused, not resumed on.
        fake::remove(root, "sdc");
        fake::disk(
            root,
            "sdd",
            Some("4C530001231012345678"),
            12,
            &fake::data(0x22),
        );
        let e = src.reopen().unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::DeviceGone);
        assert!(
            e.detail.contains("first sector is different"),
            "{}",
            e.detail
        );
        assert!(src.is_gone());

        // Two sticks with the same "unique" serial: only the one with our
        // first sector is taken.
        fake::disk(root, "sde", Some("4C530001231012345678"), 13, &ours);
        src.reopen().unwrap();
        assert_eq!(src.current_path(), root.join("dev/sde"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partition_reattach_keeps_the_partition() {
        use diskrescue_core::source::SourceExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = fake::data(0x33);
        fake::disk(root, "sdb", Some("4C530001231012345678"), 5, &content);
        fake::partitions(root, "sdb", &content);
        let src = open_fake(root, "sdb2");
        assert!(crate::is_partition_view(&src.info()));
        fake::remove(root, "sdb");
        fake::disk(root, "sdc", Some("4C530001231012345678"), 8, &content);
        fake::partitions(root, "sdc", &content);
        src.reopen().unwrap();
        assert_eq!(src.current_path(), root.join("dev/sdc2"));
        assert_eq!(
            src.read_exact_vec(0, 16).unwrap(),
            content[32 * 512..32 * 512 + 16].to_vec()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn weak_identity_without_hash_reattaches_only_to_the_same_attach() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = fake::data(0x44);
        fake::disk(root, "sdb", None, 5, &content);
        let src = open_fake(root, "sdb");
        // LBA 0 was never readable: no hash.
        src.state.lock().unwrap().fingerprint.first_sector_hash = None;
        // Re-open after errors while the stick never went away: fine.
        src.reopen().unwrap();
        // It was re-plugged (new diskseq): a stick of the same model and
        // size is not assumed to be ours.
        fake::remove(root, "sdb");
        fake::disk(root, "sdb", None, 6, &content);
        let e = src.reopen().unwrap_err();
        assert!(e.detail.contains("cannot be confirmed"), "{}", e.detail);
    }
}
