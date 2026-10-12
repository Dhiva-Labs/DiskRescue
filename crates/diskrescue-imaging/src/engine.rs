//! The imaging engine: ddrescue 1.28's algorithm over a [`diskrescue_core::Source`].
//!
//! # Phases
//!
//! 1. **Copying** (passes 1..=5, `max_copy_passes`): read non-tried areas
//!    in `block_size` reads aligned to the block size. Pass 1 runs forwards
//!    (backwards with `start_reverse`); with `reverse_second_pass` the
//!    direction alternates every pass, otherwise every pass uses the same
//!    direction. A failed read marks the failed part non-trimmed (a single
//!    failed sector directly bad, but a timeout always non-trimmed so it is
//!    read again with the longer trimming timeout) and, in passes 1..=4,
//!    skips ahead: pass 1
//!    by the error skip (`initial_skip(size)`, multiplied by `skip_growth`
//!    after every consecutive error up to `max_skip(size)`, reset after a
//!    good read), passes 2..=4 by the rest of the non-tried chunk. Passes 3
//!    and 4 only visit chunks next to finished data on the side they are
//!    approached from (areas skipped for slowness); pass 5 reads everything
//!    left without skipping. A pass that tries chunks and rescues nothing
//!    jumps straight to the final no-skip sweep (pass 5) so every byte is
//!    still tried once. Slow reads (passes 1..=2, `min_read_rate`) are
//!    kept, and after `slow_reads_before_skip` in a row the engine skips as
//!    for an error; skipped bytes stay non-tried for the later passes.
//! 2. **Trimming** (`trim`): for each non-trimmed block read sector by
//!    sector forwards from its start until a read fails, then backwards
//!    from its end until a read fails; failed sectors become bad, the
//!    untouched middle becomes non-scraped.
//! 3. **Dividing** (`divide`): read non-scraped (and non-trimmed) areas in
//!    `max(block_size / 8, 4 KiB)` pieces; failed pieces stay non-scraped.
//! 4. **Scraping** (`scrape`): read every remaining non-scraped (and
//!    non-trimmed) sector individually (`min_read_size`); failures become bad.
//! 5. **Retrying** (`retry_passes`): re-read bad sectors, alternating
//!    direction each pass, stopping early when a pass recovers nothing.
//!
//! Blocks re-marked non-trimmed by `check_on_error` after the phases that
//! handle them get up to two extra trim/divide/scrape rounds (bad sectors
//! are not retried again in those rounds).
//! Every read uses the timeout of its phase (`timeout_copy`,
//! `timeout_scrape`, `timeout_retry`); a timeout is an error for the map
//! and skipping, and is counted separately.
//!
//! # Safety properties
//!
//! * Data is written to the output (`pwrite` at `output_offset + pos`)
//!   before the map marks it finished, and the output is fsynced before
//!   every mapfile save, so the map never claims data that is not durable.
//! * Finished data is never overwritten and never demoted to a failure
//!   status, except by `check_on_error` when the device returns different
//!   data for the same block twice (then it is re-marked non-trimmed so it
//!   is read again; the data is not trusted either way).
//! * Writes stay inside `output_offset + domain`; status changes stay
//!   inside the domain; reads never go past the source size and are always
//!   aligned to the source's sector size (reads that start or end inside a
//!   sector are widened to whole sectors and the extra bytes discarded).
//!   No read crosses a boundary between map blocks of different status
//!   (apart from that sector widening).
//! * A vanished device (`DeviceGone`, or a read that returns no data before
//!   the end) never marks anything bad: the mapfile is saved and
//!   `Source::reopen` is polled until the device is back (or the wait,
//!   `max_reattaches`, or the user ends it); then the same read is retried.
//! * `Source` errors only count bytes before the reported failing offset as
//!   good after reading them again on their own (the trait does not promise
//!   that a failed read filled the buffer). A short read before the end of
//!   the source is a partial success: the prefix is kept and the rest
//!   counts as failed (it is not read again straight away, which would
//!   hammer the bad area).
//! * `check_on_error`: right after every failed read, before any other
//!   read, the last good block is read again and compared with what was
//!   written (ddrescue `-J`); a mismatch is read once more and, if it still
//!   differs, the block is re-marked non-trimmed ("stale data after
//!   error"); a check read that merely fails proves nothing and changes
//!   nothing. The first good read after an error that no check read
//!   absorbed, and any all-0x00/0xFF block while errors persist, is only
//!   accepted after a second read returns the same bytes (or, when it
//!   differs, after a third read agrees with the second).
//!
//! # Persistence
//!
//! The mapfile is saved every `save_interval`, at every phase change, on
//! cancel, before waiting for a vanished device and at the end. A small
//! JSON sidecar `<mapfile>.dr.json` (not read by ddrescue) records the
//! profile, the domain, counters and warnings.
//!
//! # Limits
//!
//! The per-read timeout is enforced by the `Source`
//! (`Source::read_at_timeout`); file and memory sources ignore it. The
//! engine keeps one read in flight (queue depth 1, as failing drives need).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::source::{ReadErrorKind, SharedSource};
use diskrescue_core::time::Timestamp;
use diskrescue_core::{Error, Range, Result};

use crate::mapfile::{self, CurrentStatus, MapFile, UNKNOWN_SIZE_END};
use crate::options::{Control, Event, ImagingOptions, Phase, Progress, Summary};
use crate::status::{BlockStatus, Direction, MapBlock, ResizePolicy, Stats};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// Largest single read the engine issues, whatever the profile says.
const MAX_BLOCK_SIZE: u64 = 64 * MIB;
/// Warnings kept for the summary (the rest are only logged).
const MAX_WARNINGS: usize = 200;
/// Read errors logged at `warn` level before switching to `debug`.
const LOUD_ERRORS: u64 = 20;
const NO_SKIP: u64 = u64::MAX;

/// Read sizes and skip distances the engine derived from the profile and
/// the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Geometry {
    /// Source size in bytes.
    pub size: u64,
    /// Source sector size; every read is aligned to it.
    pub sector: u64,
    /// Read size of the copying passes (a sector multiple).
    pub block_size: u64,
    /// Read size of trimming, scraping and retrying (`min_read_size`).
    pub unit: u64,
    /// Read size of the dividing pass.
    pub piece: u64,
    /// First skip after an error in pass 1 (0 = skipping disabled).
    pub skip_initial: u64,
    /// Largest skip.
    pub skip_max: u64,
}

/// The resumable imaging engine. See the module documentation.
pub struct Imager {
    source: SharedSource,
    output_path: PathBuf,
    mapfile_path: PathBuf,
    opts: ImagingOptions,
    out: File,
    mf: MapFile,
    geo: Geometry,
    domain: Range,
    whole_domain: bool,
    resumed: bool,
    setup_warnings: Vec<String>,
}

impl std::fmt::Debug for Imager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Imager")
            .field("source", &self.source.describe())
            .field("output", &self.output_path)
            .field("mapfile", &self.mapfile_path)
            .field("geometry", &self.geo)
            .field("domain", &self.domain)
            .field("resumed", &self.resumed)
            .finish()
    }
}

#[derive(Debug, Clone)]
enum StopReason {
    Cancelled,
    Halt(String),
}

#[derive(Debug)]
enum Flow {
    Done,
    Stop(StopReason),
}

#[derive(Debug)]
enum PassOutcome {
    /// The pass found chunks to read; `rescued` bytes were rescued.
    Done {
        rescued: u64,
    },
    /// No eligible chunk existed.
    NothingFound,
    Stopped(StopReason),
}

#[derive(Debug, Clone, Copy)]
struct Resume {
    status: CurrentStatus,
    pass: u32,
    pos: u64,
    active: bool,
}

impl Resume {
    fn take(&mut self, status: CurrentStatus) -> Option<(u32, u64)> {
        if self.active && self.status == status {
            self.active = false;
            Some((self.pass, self.pos))
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct Failure {
    at: u64,
    kind: ReadErrorKind,
    detail: String,
}

#[derive(Debug, Default)]
struct Attempt {
    rescued: u64,
    failed: Option<Failure>,
    slow: bool,
    stop: Option<StopReason>,
}

enum Raw {
    /// `len` wanted bytes, starting at the requested position, are in the
    /// buffer at `off`. `short` = the source returned less than the
    /// requested range although it lies inside the source.
    Data {
        off: usize,
        len: usize,
        latency: Duration,
        short: bool,
    },
    /// The read failed; `at` is the source-reported failing offset.
    Failed {
        at: u64,
        kind: ReadErrorKind,
        detail: String,
    },
    /// The source returned no data although the position is below its size.
    Eof,
}

/// Per-run state that is not part of the persistent map.
struct Ctx<'a> {
    control: Arc<Control>,
    on_event: &'a mut dyn FnMut(Event),
    start: Instant,
    phase: Phase,
    phase_start: Instant,
    phase_processed: u64,
    last_save: Instant,
    last_fsync: Instant,
    last_progress: Option<Instant>,
    last_map: Option<Instant>,
    rescued_run: u64,
    rate_samples: VecDeque<(Instant, u64)>,
    errors: u64,
    copy_errors: u64,
    timeouts: u64,
    slow_reads: u64,
    reattaches: u64,
    stale_detections: u64,
    consecutive_errors: u64,
    consecutive_slow: u32,
    last_error: Option<String>,
    warnings: Vec<String>,
    suppressed_warnings: u64,
    last_good: Option<(u64, Vec<u8>)>,
    /// A read failed and no good read has happened since.
    after_error: bool,
    /// A read failed and no check read has been issued since: the next
    /// read is the one a misbehaving bridge may answer with stale data.
    unverified_error: bool,
    slow_active: bool,
    buf: Vec<u8>,
    check_buf: Vec<u8>,
}

impl<'a> Ctx<'a> {
    fn new(control: Arc<Control>, on_event: &'a mut dyn FnMut(Event)) -> Self {
        let now = Instant::now();
        Ctx {
            control,
            on_event,
            start: now,
            phase: Phase::Copying {
                pass: 1,
                reverse: false,
            },
            phase_start: now,
            phase_processed: 0,
            last_save: now,
            last_fsync: now,
            last_progress: None,
            last_map: None,
            rescued_run: 0,
            rate_samples: VecDeque::new(),
            errors: 0,
            copy_errors: 0,
            timeouts: 0,
            slow_reads: 0,
            reattaches: 0,
            stale_detections: 0,
            consecutive_errors: 0,
            consecutive_slow: 0,
            last_error: None,
            warnings: Vec::new(),
            suppressed_warnings: 0,
            last_good: None,
            after_error: false,
            unverified_error: false,
            slow_active: false,
            buf: Vec::new(),
            check_buf: Vec::new(),
        }
    }

    fn emit(&mut self, ev: Event) {
        (self.on_event)(ev);
    }

    fn info(&mut self, msg: String) {
        log::info!("{msg}");
        self.emit(Event::Log(log::Level::Info, msg));
    }

    /// Log a warning, report it as an event and keep it for the summary.
    fn warn(&mut self, msg: String) {
        log::warn!("{msg}");
        self.emit(Event::Log(log::Level::Warn, msg.clone()));
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(msg);
        } else {
            self.suppressed_warnings += 1;
        }
    }

    fn note_rescued(&mut self, bytes: u64) {
        self.rescued_run += bytes;
        let now = Instant::now();
        let push = self
            .rate_samples
            .back()
            .is_none_or(|(t, _)| now.duration_since(*t) >= Duration::from_millis(50));
        if push {
            self.rate_samples.push_back((now, self.rescued_run));
        }
        while let Some((t, _)) = self.rate_samples.front() {
            if now.duration_since(*t) > Duration::from_secs(2) && self.rate_samples.len() > 1 {
                self.rate_samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Rescued bytes per second over (about) the last second.
    fn current_rate(&self, now: Instant) -> f64 {
        let window = now.checked_sub(Duration::from_secs(1));
        let base = self
            .rate_samples
            .iter()
            .find(|(t, _)| window.is_none_or(|w| *t >= w))
            .or(self.rate_samples.front());
        match base {
            Some((t, v)) => {
                let dt = now.duration_since(*t).as_secs_f64();
                if dt >= 0.05 {
                    (self.rescued_run - v) as f64 / dt
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    }

    fn average_rate(&self, now: Instant) -> f64 {
        let dt = now.duration_since(self.start).as_secs_f64();
        if dt > 0.0 {
            self.rescued_run as f64 / dt
        } else {
            0.0
        }
    }

    fn remember_good(&mut self, pos: u64, data: &[u8]) {
        match &mut self.last_good {
            Some((p, v)) => {
                *p = pos;
                v.clear();
                v.extend_from_slice(data);
            }
            None => self.last_good = Some((pos, data.to_vec())),
        }
    }

    /// Sleep up to `d`, waking early on cancel. Returns true if cancelled.
    fn sleep(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            if self.control.is_cancelled() {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            std::thread::sleep((end - now).min(Duration::from_millis(50)));
        }
    }
}

fn align_down(x: u64, a: u64) -> u64 {
    if a <= 1 {
        x
    } else {
        x - x % a
    }
}

fn align_up(x: u64, a: u64) -> u64 {
    if a <= 1 {
        return x;
    }
    match x % a {
        0 => x,
        r => x.saturating_add(a - r),
    }
}

/// End of a forward read of at most `unit` bytes starting at `start`
/// inside a chunk ending at `end`: aligned down to a multiple of `unit`
/// unless that is the chunk end or would make the read empty (ddrescue's
/// `align_end`).
fn chunk_end_fwd(start: u64, end: u64, unit: u64) -> u64 {
    let e = start.saturating_add(unit).min(end);
    if e == end {
        return e;
    }
    let a = align_down(e, unit);
    if a > start {
        a
    } else {
        e
    }
}

/// Mirror of [`chunk_end_fwd`] for backward reads ending at `end`.
fn chunk_start_bwd(start: u64, end: u64, unit: u64) -> u64 {
    let s = end.saturating_sub(unit).max(start);
    if s == start {
        return s;
    }
    let a = align_up(s, unit);
    if a < end {
        a
    } else {
        s
    }
}

/// Result of reading a block again for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verify {
    Same,
    Different,
    /// The second read failed: proves nothing about the data.
    Failed,
}

/// Outcome of verifying the first read after an error.
#[derive(Debug)]
enum Fresh {
    /// A second read returned the same bytes.
    Confirmed,
    /// The second read differed, a third agreed with the second: use it.
    Replaced(Vec<u8>),
    /// No two reads agreed.
    Inconsistent,
    /// A verification read failed.
    Unverified,
}

fn is_uniform(data: &[u8]) -> bool {
    match data.first() {
        Some(&b) if b == 0 || b == 0xFF => data.iter().all(|&x| x == b),
        _ => false,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(ca), Ok(cb)) = (fs::canonicalize(a), fs::canonicalize(b)) {
        if ca == cb {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    false
}

fn write_all_at(file: &File, data: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < data.len() {
            match file.seek_write(&data[done..], offset + done as u64) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write the whole buffer",
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
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
}

/// Reserve `len` bytes for `file`. `Ok(false)` means "not supported here".
fn preallocate(file: &File, len: u64) -> std::io::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        use std::os::unix::io::AsRawFd;
        let len = libc::off_t::try_from(len)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "size too large"))?;
        // SAFETY: the descriptor is valid for the lifetime of `file`;
        // posix_fallocate does not touch memory we own.
        let r = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len) };
        match r {
            0 => Ok(true),
            libc::EOPNOTSUPP | libc::EINVAL | libc::ENODEV | libc::ESPIPE => Ok(false),
            e => Err(std::io::Error::from_raw_os_error(e)),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    {
        let _ = (file, len);
        Ok(false)
    }
}

#[derive(serde::Serialize)]
struct Sidecar<'a> {
    format: u32,
    tool: &'a str,
    version: &'a str,
    source: String,
    geometry: Geometry,
    domain: Range,
    output_offset: u64,
    profile: &'a ImagingProfile,
    phase: Phase,
    stats: Stats,
    errors: u64,
    timeouts: u64,
    slow_reads: u64,
    reattaches: u64,
    stale_detections: u64,
    warnings: &'a [String],
    updated: String,
}

impl Imager {
    /// Prepare a rescue of `source` into `output`, recording progress in
    /// `mapfile`.
    ///
    /// * An existing, non-empty `mapfile` is resumed. Its size must equal
    ///   the source size unless `opts.allow_size_mismatch` is set (or the
    ///   map says "size unknown"), in which case it is grown with non-tried
    ///   bytes or shrunk when no rescued data lies beyond the source end.
    ///   Boundaries that do not fall on the source's sector size are moved
    ///   into the less processed neighbour (ddrescue `join_subsectors`).
    /// * A missing (or empty) mapfile starts a new rescue; an initial
    ///   mapfile is written immediately. If `output` already exists and is
    ///   not empty this is refused unless `opts.allow_existing_output`.
    /// * The output is opened read-write (created when missing) and
    ///   extended sparsely to `output_offset + size` when it is a shorter
    ///   regular file. Resuming is refused when the output is missing or
    ///   shorter than the rescued data the map records.
    /// * Refused outright: output, mapfile and source paths that refer to
    ///   the same file; a source of size 0; an empty domain.
    pub fn new(
        source: SharedSource,
        output: &Path,
        mapfile: &Path,
        opts: ImagingOptions,
    ) -> Result<Self> {
        let size = source.size();
        if size == 0 {
            return Err(Error::InvalidArgument(format!(
                "{} reports a size of 0 bytes (controller failure or no medium?)",
                source.describe()
            )));
        }
        if size > UNKNOWN_SIZE_END {
            return Err(Error::InvalidArgument(format!(
                "source size {size} exceeds what a mapfile can describe"
            )));
        }
        let sector = source.sector_size() as u64;
        if sector == 0 {
            return Err(Error::InvalidArgument(format!(
                "{} reports a sector size of 0",
                source.describe()
            )));
        }
        let needed = opts
            .output_offset
            .checked_add(size)
            .filter(|n| *n <= UNKNOWN_SIZE_END)
            .ok_or_else(|| {
                Error::InvalidArgument("output_offset + source size is too large".into())
            })?;
        let domain = match opts.domain {
            None => Range::new(0, size),
            Some(d) => {
                let d = Range::new(d.start, d.end.min(size));
                if d.is_empty() {
                    return Err(Error::InvalidArgument(format!(
                        "the rescue domain {}..{} does not overlap the source (0..{size})",
                        opts.domain.map_or(0, |d| d.start),
                        opts.domain.map_or(0, |d| d.end)
                    )));
                }
                d
            }
        };

        // Never let output, mapfile and source alias each other.
        if same_file(output, mapfile) {
            return Err(Error::Refused(
                "the output image and the mapfile are the same file".into(),
            ));
        }
        if let Some(sp) = source.path() {
            for (what, p) in [("output image", output), ("mapfile", mapfile)] {
                if same_file(sp, p) {
                    return Err(Error::Refused(format!(
                        "the {what} {} is the source itself",
                        p.display()
                    )));
                }
            }
        }

        let geo = Self::geometry_for(&opts.profile, size, sector);
        let mut setup_warnings = Vec::new();
        if geo.block_size != align_up(opts.profile.block_size.max(sector), sector) {
            setup_warnings.push(format!(
                "block size {} capped to {} bytes",
                opts.profile.block_size, geo.block_size
            ));
        }

        // Mapfile: resume or start fresh.
        let mut resumed = false;
        let mut mf = if mapfile.exists() {
            let m = MapFile::load(mapfile)?;
            if m.map.is_empty() {
                MapFile::new(size)
            } else {
                resumed = true;
                m
            }
        } else {
            MapFile::new(size)
        };
        if resumed {
            let ms = mf.map.size();
            if ms != size {
                if mf.size_is_unknown() || opts.allow_size_mismatch {
                    mf.map.resize(size, ResizePolicy::KeepFinished)?;
                    if !mf.size_is_unknown() {
                        setup_warnings.push(format!(
                            "mapfile covered {ms} bytes, source has {size}; map adapted"
                        ));
                    }
                } else {
                    return Err(Error::InvalidArgument(format!(
                        "mapfile {} covers {ms} bytes but the source has {size} bytes: wrong \
                         mapfile, or the device reports a different capacity than before \
                         (allow_size_mismatch adapts the map)",
                        mapfile.display()
                    )));
                }
            }
            let fixed = mf.map.join_subsectors(sector);
            if fixed > 0 {
                setup_warnings.push(format!(
                    "{fixed} mapfile boundaries were not multiples of the {sector}-byte sector \
                     size and were moved (the affected sectors will be read again)"
                ));
            }
            mf.current_pos = mf.current_pos.min(size);
            if opts.command_line.is_some() {
                mf.command_line = opts.command_line.clone();
            }
        } else {
            mf.command_line = opts.command_line.clone();
        }
        mf.size_hint = Some(size);

        // Output image.
        let finished_end = mf
            .map
            .blocks()
            .iter()
            .rev()
            .find(|b| b.status == BlockStatus::Finished)
            .map_or(0, |b| b.end());
        let existing = fs::metadata(output).ok();
        match &existing {
            Some(meta) if meta.is_file() => {
                let len = meta.len();
                if !resumed && len > 0 && !opts.allow_existing_output {
                    return Err(Error::Refused(format!(
                        "output {} already exists ({len} bytes) and there is no mapfile to \
                         resume from; refusing to overwrite it (allow_existing_output)",
                        output.display()
                    )));
                }
                if resumed && finished_end > 0 && len < opts.output_offset + finished_end {
                    return Err(Error::Refused(format!(
                        "output {} is {len} bytes but the mapfile records rescued data up to \
                         byte {}; wrong output file?",
                        output.display(),
                        opts.output_offset + finished_end
                    )));
                }
            }
            Some(_) => {}
            None => {
                if resumed && finished_end > 0 {
                    return Err(Error::Refused(format!(
                        "the mapfile records rescued data but the output {} does not exist",
                        output.display()
                    )));
                }
            }
        }
        let out = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(output)
            .map_err(|e| mapfile::io_ctx(e, output))?;
        let meta = out.metadata().map_err(|e| mapfile::io_ctx(e, output))?;
        if meta.is_file() {
            if meta.len() < needed {
                out.set_len(needed)
                    .map_err(|e| mapfile::io_ctx(e, output))?;
            }
        } else {
            use std::io::{Seek, SeekFrom};
            let mut f = &out;
            let len = f
                .seek(SeekFrom::End(0))
                .map_err(|e| mapfile::io_ctx(e, output))?;
            if len < needed {
                return Err(Error::InvalidArgument(format!(
                    "output {} holds {len} bytes but {needed} are needed",
                    output.display()
                )));
            }
        }
        if opts.preallocate && meta.is_file() {
            match preallocate(&out, needed) {
                Ok(true) => {}
                Ok(false) => setup_warnings.push(
                    "preallocation is not supported for this output; continuing without".into(),
                ),
                Err(e) => {
                    return Err(Error::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "cannot reserve {needed} bytes for {}: {e}",
                            output.display()
                        ),
                    )))
                }
            }
        }

        let im = Imager {
            source,
            output_path: output.to_path_buf(),
            mapfile_path: mapfile.to_path_buf(),
            whole_domain: domain == Range::new(0, size),
            opts,
            out,
            mf,
            geo,
            domain,
            resumed,
            setup_warnings,
        };
        if !resumed {
            // Make the rescue resumable from the very first read on.
            im.out.sync_all().map_err(|e| mapfile::io_ctx(e, output))?;
            im.write_map()?;
        }
        Ok(im)
    }

    fn geometry_for(p: &ImagingProfile, size: u64, sector: u64) -> Geometry {
        let max_bs = align_down(MAX_BLOCK_SIZE, sector).max(sector);
        let block_size = align_up(p.block_size.max(sector), sector).min(max_bs);
        let unit = align_up(p.min_read_size.max(sector), sector).min(block_size);
        let piece = align_up((block_size / 8).max(4 * KIB).max(sector), sector).min(block_size);
        let initial = p.initial_skip(size);
        let skip_initial = if initial == 0 {
            0
        } else {
            align_up(initial, sector)
        };
        let skip_max = align_up(p.max_skip(size), sector).max(skip_initial);
        Geometry {
            size,
            sector,
            block_size,
            unit,
            piece,
            skip_initial,
            skip_max,
        }
    }

    /// The mapfile state (current map, status line).
    pub fn mapfile(&self) -> &MapFile {
        &self.mf
    }

    /// The rescue domain in use (clamped to the source).
    pub fn domain(&self) -> Range {
        self.domain
    }

    /// Read sizes and skip distances in use.
    pub fn geometry(&self) -> Geometry {
        self.geo
    }

    /// True when an existing mapfile was resumed.
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Totals inside the domain.
    pub fn stats(&self) -> Stats {
        if self.whole_domain {
            self.mf.map.stats()
        } else {
            self.mf.map.stats_in(self.domain)
        }
    }

    /// fsync the output and save the mapfile now.
    pub fn save(&mut self) -> Result<()> {
        self.flush_output()?;
        self.write_map()
    }

    fn flush_output(&self) -> Result<()> {
        self.out.sync_data().map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "cannot flush the output {}: {e}",
                    self.output_path.display()
                ),
            ))
        })
    }

    fn write_map(&self) -> Result<()> {
        self.mf.save_atomic(
            &self.mapfile_path,
            &self.opts.tool_name,
            &self.opts.tool_version,
            Timestamp::now(),
        )
    }

    fn copy_direction(&self, pass: u32) -> Direction {
        let base = if self.opts.start_reverse {
            Direction::Backward
        } else {
            Direction::Forward
        };
        if self.opts.profile.reverse_second_pass && pass % 2 == 0 {
            base.reversed()
        } else {
            base
        }
    }

    fn retry_direction(&self, pass: u32) -> Direction {
        let base = if self.opts.start_reverse {
            Direction::Backward
        } else {
            Direction::Forward
        };
        if pass % 2 == 0 {
            base.reversed()
        } else {
            base
        }
    }

    fn phase_timeout(&self, phase: Phase) -> Duration {
        let p = &self.opts.profile;
        match phase {
            Phase::Copying { .. } | Phase::Finished => p.timeout_copy,
            Phase::Trimming | Phase::Dividing | Phase::Scraping => p.timeout_scrape,
            Phase::Retrying { .. } => p.timeout_retry,
        }
    }

    /// Run the rescue until every phase is done, a limit stops it, or
    /// `control` cancels it. The mapfile is saved on every way out; on an
    /// error it is saved too unless the output could not be flushed (then
    /// the previous mapfile, which matches durable data, is kept).
    pub fn run(
        &mut self,
        control: Arc<Control>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Summary> {
        let mut ctx = Ctx::new(control, on_event);
        for w in self.setup_warnings.clone() {
            ctx.warn(w);
        }
        if self.opts.profile.direct_io && !self.source.requires_alignment() {
            log::debug!(
                "profile asks for direct I/O; {} decides how it reads",
                self.source.describe()
            );
        }
        self.mf.start_time = Some(Timestamp::now());
        let resume = Resume {
            status: self.mf.current_status,
            pass: self.mf.current_pass,
            pos: self.mf.current_pos,
            active: self.resumed,
        };
        if self.resumed {
            ctx.info(format!(
                "resuming: status '{}' pass {} at 0x{:X}",
                self.mf.current_status.as_char(),
                self.mf.current_pass,
                self.mf.current_pos
            ));
        }
        let flow = match self.run_phases(&mut ctx, resume) {
            Ok(f) => f,
            Err(e) => {
                log::error!("imaging stopped: {e}");
                match self.flush_output() {
                    Ok(()) => {
                        if let Err(se) = self.write_map() {
                            log::error!("could not save the mapfile: {se}");
                        }
                    }
                    Err(fe) => log::error!("{fe}; keeping the previous mapfile"),
                }
                return Err(e);
            }
        };
        let (cancelled, stopped) = match flow {
            Flow::Done => (false, false),
            Flow::Stop(StopReason::Cancelled) => {
                ctx.info("cancelled; mapfile saved, the rescue can be resumed".into());
                (true, true)
            }
            Flow::Stop(StopReason::Halt(msg)) => {
                ctx.warn(msg);
                (false, true)
            }
        };
        if !stopped {
            ctx.phase = Phase::Finished;
            self.mf.current_status = CurrentStatus::Finished;
            ctx.emit(Event::Phase(Phase::Finished));
        }
        if ctx.suppressed_warnings > 0 {
            let n = ctx.suppressed_warnings;
            ctx.warnings
                .push(format!("{n} further warnings were only written to the log"));
        }
        self.flush_output()?;
        self.write_map()?;
        self.write_sidecar(&ctx);
        ctx.emit(Event::Saved);
        let stats = self.stats();
        let summary = Summary {
            stats,
            elapsed: ctx.start.elapsed(),
            errors: ctx.errors,
            finished: !stopped && stats.pending() == 0,
            cancelled,
            reattaches: ctx.reattaches,
            timeouts: ctx.timeouts,
            slow_reads: ctx.slow_reads,
            output: self.output_path.clone(),
            mapfile: self.mapfile_path.clone(),
            warnings: ctx.warnings.clone(),
        };
        self.maybe_progress(&mut ctx, true);
        ctx.emit(Event::Finished(summary.clone()));
        Ok(summary)
    }

    fn run_phases(&mut self, ctx: &mut Ctx, mut resume: Resume) -> Result<Flow> {
        if let Flow::Stop(r) = self.copy_phase(ctx, &mut resume)? {
            return Ok(Flow::Stop(r));
        }
        if let Flow::Stop(r) = self.trim_phase(ctx, &mut resume)? {
            return Ok(Flow::Stop(r));
        }
        if let Flow::Stop(r) = self.divide_phase(ctx, &mut resume)? {
            return Ok(Flow::Stop(r));
        }
        if let Flow::Stop(r) = self.scrape_phase(ctx, &mut resume)? {
            return Ok(Flow::Stop(r));
        }
        if let Flow::Stop(r) = self.retry_phase(ctx, &mut resume)? {
            return Ok(Flow::Stop(r));
        }
        // check_on_error may have re-marked blocks non-trimmed after the
        // phases that handle them; give them (a bounded number of) more
        // rounds. Bad sectors are not retried again here.
        let p = &self.opts.profile;
        let fine_work = p.trim || p.divide || p.scrape;
        for _ in 0..2 {
            if !fine_work || !self.domain_has(&[BlockStatus::NonTrimmed]) {
                break;
            }
            let mut none = resume;
            none.active = false;
            ctx.info("re-reading blocks re-marked after inconsistent data was detected".into());
            for phase in [Self::trim_phase, Self::divide_phase, Self::scrape_phase] {
                if let Flow::Stop(r) = phase(self, ctx, &mut none)? {
                    return Ok(Flow::Stop(r));
                }
            }
        }
        Ok(Flow::Done)
    }

    // ----------------------------------------------------------------
    // Bookkeeping shared by all phases
    // ----------------------------------------------------------------

    /// Start a phase (or pass) at `pos`. The status line and the position
    /// are updated together, so a map saved before the first read of the
    /// pass never pairs the new pass with the previous pass's position.
    fn enter_phase(&mut self, ctx: &mut Ctx, phase: Phase, pos: u64) -> Result<()> {
        self.mf.current_pos = pos;
        ctx.phase = phase;
        ctx.phase_start = Instant::now();
        ctx.phase_processed = 0;
        let (status, pass) = match phase {
            Phase::Copying { pass, .. } => (CurrentStatus::Copying, pass),
            Phase::Trimming => (CurrentStatus::Trimming, 1),
            Phase::Dividing => (CurrentStatus::Scraping, 1),
            Phase::Scraping => (CurrentStatus::Scraping, 2),
            Phase::Retrying { pass, .. } => (CurrentStatus::Retrying, pass),
            Phase::Finished => (CurrentStatus::Finished, self.mf.current_pass),
        };
        self.mf.current_status = status;
        self.mf.current_pass = pass;
        log::info!("{phase}");
        ctx.emit(Event::Phase(phase));
        self.save_map(ctx)?;
        self.maybe_progress(ctx, true);
        Ok(())
    }

    /// fsync the output, then save the mapfile. A failed fsync is fatal (the
    /// map must never claim data that may not be on disk); a failed map save
    /// is a warning and is retried at the next save point.
    fn save_map(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.flush_output()?;
        let now = Instant::now();
        ctx.last_fsync = now;
        ctx.last_save = now;
        match self.write_map() {
            Ok(()) => {
                self.write_sidecar(ctx);
                ctx.emit(Event::Saved);
            }
            Err(e) => ctx.warn(format!(
                "could not save the mapfile {}: {e}; will retry",
                self.mapfile_path.display()
            )),
        }
        Ok(())
    }

    fn write_sidecar(&self, ctx: &Ctx) {
        let sc = Sidecar {
            format: 1,
            tool: &self.opts.tool_name,
            version: &self.opts.tool_version,
            source: self.source.describe(),
            geometry: self.geo,
            domain: self.domain,
            output_offset: self.opts.output_offset,
            profile: &self.opts.profile,
            phase: ctx.phase,
            stats: self.stats(),
            errors: ctx.errors,
            timeouts: ctx.timeouts,
            slow_reads: ctx.slow_reads,
            reattaches: ctx.reattaches,
            stale_detections: ctx.stale_detections,
            warnings: &ctx.warnings,
            updated: Timestamp::now().to_rfc3339(),
        };
        let path = mapfile::with_suffix(&self.mapfile_path, ".dr.json");
        let res = serde_json::to_vec_pretty(&sc)
            .map_err(|e| Error::other(e.to_string()))
            .and_then(|bytes| mapfile::write_atomic(&path, &bytes, false));
        if let Err(e) = res {
            log::debug!("could not write {}: {e}", path.display());
        }
    }

    /// Between reads: honour cancel/pause, save and fsync when due, report
    /// progress.
    fn checkpoint(&mut self, ctx: &mut Ctx) -> Result<Option<StopReason>> {
        if ctx.control.is_cancelled() {
            return Ok(Some(StopReason::Cancelled));
        }
        if ctx.control.is_paused() {
            ctx.info("paused".into());
            self.save_map(ctx)?;
            while ctx.control.is_paused() {
                if ctx.control.is_cancelled() {
                    return Ok(Some(StopReason::Cancelled));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            ctx.info("resumed".into());
        }
        let now = Instant::now();
        if ctx.control.take_save_request()
            || now.duration_since(ctx.last_save) >= self.opts.save_interval
        {
            self.save_map(ctx)?;
        } else if now.duration_since(ctx.last_fsync) >= self.opts.fsync_interval {
            self.flush_output()?;
            ctx.last_fsync = now;
        }
        self.maybe_progress(ctx, false);
        Ok(None)
    }

    fn maybe_progress(&self, ctx: &mut Ctx, force: bool) {
        let now = Instant::now();
        if !force {
            if let Some(t) = ctx.last_progress {
                if now.duration_since(t) < self.opts.progress_interval {
                    return;
                }
            }
        }
        ctx.last_progress = Some(now);
        let p = self.progress(ctx, now);
        ctx.emit(Event::Progress(p));
        if let Some(every) = self.opts.map_snapshot_interval {
            let due = force || ctx.last_map.is_none_or(|t| now.duration_since(t) >= every);
            if due {
                ctx.last_map = Some(now);
                ctx.emit(Event::Map(std::sync::Arc::new(self.mf.map.clone())));
            }
        }
    }

    fn progress(&self, ctx: &Ctx, now: Instant) -> Progress {
        let stats = self.stats();
        let remaining = match ctx.phase {
            Phase::Copying { .. } => stats.non_tried,
            Phase::Trimming => stats.non_trimmed,
            Phase::Dividing | Phase::Scraping => stats.non_trimmed + stats.non_scraped,
            Phase::Retrying { pass, .. } => stats
                .bad_sectors
                .saturating_mul((self.opts.profile.retry_passes.saturating_sub(pass) as u64) + 1),
            Phase::Finished => 0,
        };
        let phase_elapsed = now.duration_since(ctx.phase_start).as_secs_f64();
        let eta = if remaining == 0 {
            Some(Duration::ZERO)
        } else if phase_elapsed >= 1.0 && ctx.phase_processed > 0 {
            let rate = ctx.phase_processed as f64 / phase_elapsed;
            let secs = remaining as f64 / rate;
            (secs.is_finite() && secs < 1e10).then(|| Duration::from_secs_f64(secs))
        } else {
            None
        };
        Progress {
            phase: ctx.phase,
            stats,
            current_pos: self.mf.current_pos,
            current_rate: ctx.current_rate(now),
            average_rate: ctx.average_rate(now),
            elapsed: now.duration_since(ctx.start),
            eta,
            errors: ctx.errors,
            slow_reads: ctx.slow_reads,
            timeouts: ctx.timeouts,
            reattaches: ctx.reattaches,
            last_error: ctx.last_error.clone(),
            size: self.domain.len(),
        }
    }

    // ----------------------------------------------------------------
    // Map queries restricted to the domain
    // ----------------------------------------------------------------

    /// The next stretch (forward from `pos`) with one of `statuses` that
    /// starts before `limit`, cropped to the domain (but not to `limit`).
    fn next_fwd(&self, pos: u64, limit: u64, statuses: &[BlockStatus]) -> Option<MapBlock> {
        let from = pos.max(self.domain.start);
        let b = self
            .mf
            .map
            .find_next_any(from, statuses, Direction::Forward)?;
        if b.pos >= limit.min(self.domain.end) {
            return None;
        }
        let end = b.end().min(self.domain.end);
        Some(MapBlock {
            pos: b.pos,
            size: end - b.pos,
            status: b.status,
        })
    }

    /// The previous stretch (bytes below `pos`) with one of `statuses`,
    /// cropped to the domain.
    fn next_bwd(&self, pos: u64, statuses: &[BlockStatus]) -> Option<MapBlock> {
        let from = pos.min(self.domain.end);
        let b = self
            .mf
            .map
            .find_next_any(from, statuses, Direction::Backward)?;
        if b.end() <= self.domain.start {
            return None;
        }
        let start = b.pos.max(self.domain.start);
        Some(MapBlock {
            pos: start,
            size: b.end() - start,
            status: b.status,
        })
    }

    fn domain_has(&self, statuses: &[BlockStatus]) -> bool {
        self.next_fwd(self.domain.start, self.domain.end, statuses)
            .is_some()
    }

    /// Non-tried chunk for a forward copying pass. With `neighbour_rule`
    /// only chunks whose left neighbour is finished qualify (passes 3/4).
    fn copy_chunk_fwd(&self, pos: u64, neighbour_rule: bool) -> Option<Range> {
        let blocks = self.mf.map.blocks();
        let mut from = pos.max(self.domain.start);
        loop {
            let i =
                self.mf
                    .map
                    .find_index_any(from, &[BlockStatus::NonTried], Direction::Forward)?;
            let b = blocks[i];
            let start = b.pos.max(from);
            if start >= self.domain.end {
                return None;
            }
            if neighbour_rule && !(i > 0 && blocks[i - 1].status == BlockStatus::Finished) {
                from = b.end();
                continue;
            }
            return Some(Range::new(start, b.end().min(self.domain.end)));
        }
    }

    /// Mirror of [`Imager::copy_chunk_fwd`]: chunk with bytes below `pos`;
    /// with `neighbour_rule` its right neighbour must be finished.
    fn copy_chunk_bwd(&self, pos: u64, neighbour_rule: bool) -> Option<Range> {
        let blocks = self.mf.map.blocks();
        let mut from = pos.min(self.domain.end);
        loop {
            let i =
                self.mf
                    .map
                    .find_index_any(from, &[BlockStatus::NonTried], Direction::Backward)?;
            let b = blocks[i];
            let end = b.end().min(from);
            if end <= self.domain.start {
                return None;
            }
            if neighbour_rule
                && !(i + 1 < blocks.len() && blocks[i + 1].status == BlockStatus::Finished)
            {
                from = b.pos;
                if from <= self.domain.start {
                    return None;
                }
                continue;
            }
            return Some(Range::new(b.pos.max(self.domain.start), end));
        }
    }

    fn grow_skip(&self, s: u64) -> u64 {
        let g = self.opts.profile.skip_growth as f64;
        if !g.is_finite() || g <= 1.0 {
            return s.min(self.geo.skip_max).max(self.geo.sector);
        }
        let next = (s as f64 * g).min(self.geo.skip_max as f64) as u64;
        align_up(next, self.geo.sector)
            .min(self.geo.skip_max)
            .max(s.min(self.geo.skip_max))
    }

    /// Skip up to `skip` bytes of the non-tried chunk adjacent to `pos` in
    /// the direction of travel; returns the new position. The skipped bytes
    /// stay non-tried.
    fn skip_from(&self, pos: u64, skip: u64, dir: Direction) -> u64 {
        let sector = self.geo.sector;
        match dir {
            Direction::Forward => {
                let Some(c) = self.next_fwd(pos, self.domain.end, &[BlockStatus::NonTried]) else {
                    return pos;
                };
                if c.pos != pos {
                    return pos;
                }
                let mut target = pos.saturating_add(skip).min(c.end());
                if target < c.end() {
                    let a = align_down(target, sector);
                    if a > pos {
                        target = a;
                    }
                }
                if target > pos {
                    log::debug!("skipping 0x{:X}..0x{target:X}", pos);
                }
                target
            }
            Direction::Backward => {
                let Some(c) = self.next_bwd(pos, &[BlockStatus::NonTried]) else {
                    return pos;
                };
                if c.end() != pos {
                    return pos;
                }
                let mut target = pos.saturating_sub(skip).max(c.pos);
                if target > c.pos {
                    let a = align_up(target, sector);
                    if a < pos {
                        target = a;
                    }
                }
                if target < pos {
                    log::debug!("skipping 0x{target:X}..0x{:X}", pos);
                }
                target
            }
        }
    }

    /// The block containing `pos` (cropped to the domain), as a sweep start.
    fn sweep_cursor(&self, pos: Option<u64>) -> u64 {
        match pos {
            Some(p) if p > self.domain.start && p < self.domain.end => self
                .mf
                .map
                .block_at(p)
                .map_or(self.domain.start, |b| b.pos.max(self.domain.start)),
            _ => self.domain.start,
        }
    }

    fn segments(&self, cursor: u64) -> Vec<(u64, u64)> {
        if cursor <= self.domain.start {
            vec![(self.domain.start, self.domain.end)]
        } else {
            vec![(cursor, self.domain.end), (self.domain.start, cursor)]
        }
    }

    // ----------------------------------------------------------------
    // Phases
    // ----------------------------------------------------------------

    fn copy_phase(&mut self, ctx: &mut Ctx, resume: &mut Resume) -> Result<Flow> {
        let max = self.opts.max_copy_passes.min(5);
        if max == 0 || !self.domain_has(&[BlockStatus::NonTried]) {
            return Ok(Flow::Done);
        }
        let (mut pass, mut resume_pos) = match resume.take(CurrentStatus::Copying) {
            Some((p, pos)) if (1..=max).contains(&p) => (p, Some(pos)),
            _ => (1, None),
        };
        while pass <= max {
            let dir = self.copy_direction(pass);
            let resumed_pass = resume_pos.is_some();
            let start = self.pass_start(dir, resume_pos.take());
            self.enter_phase(
                ctx,
                Phase::Copying {
                    pass,
                    reverse: !dir.is_forward(),
                },
                start,
            )?;
            let out = self.copy_pass(ctx, pass, dir, start)?;
            let rescued = match out {
                PassOutcome::Stopped(r) => return Ok(Flow::Stop(r)),
                PassOutcome::NothingFound => None,
                PassOutcome::Done { rescued } => Some(rescued),
            };
            if !self.domain_has(&[BlockStatus::NonTried]) {
                break;
            }
            if rescued == Some(0) && pass < 4 && max >= 5 && !resumed_pass {
                ctx.info(format!(
                    "copying pass {pass} rescued nothing; going to the final sweep (pass 5)"
                ));
                pass = 5;
            } else {
                pass += 1;
            }
        }
        Ok(Flow::Done)
    }

    /// Where a forward (backward) sweep starts: the resume position when
    /// it lies in the domain, else the domain start (end).
    fn pass_start(&self, dir: Direction, resume_pos: Option<u64>) -> u64 {
        let d = self.domain;
        match (dir.is_forward(), resume_pos) {
            (true, Some(p)) if p >= d.start && p < d.end => p,
            (true, _) => d.start,
            (false, Some(p)) if p > d.start && p <= d.end => p,
            (false, _) => d.end,
        }
    }

    fn copy_pass(
        &mut self,
        ctx: &mut Ctx,
        pass: u32,
        dir: Direction,
        start: u64,
    ) -> Result<PassOutcome> {
        let neighbour_rule = pass == 3 || pass == 4;
        let skipping = pass <= 4 && self.geo.skip_initial > 0;
        ctx.slow_active = pass <= 2 && self.opts.profile.min_read_rate.is_some();
        ctx.consecutive_slow = 0;
        let fwd = dir.is_forward();
        let bs = self.geo.block_size;
        let mut eskip = self.geo.skip_initial;
        let mut sskip = 0u64;
        let mut prev_slow_skip = false;
        let mut rescued = 0u64;
        let mut found = false;
        let mut pos = start;
        let outcome = loop {
            if let Some(stop) = self.checkpoint(ctx)? {
                break PassOutcome::Stopped(stop);
            }
            let chunk = if fwd {
                self.copy_chunk_fwd(pos, neighbour_rule)
            } else {
                self.copy_chunk_bwd(pos, neighbour_rule)
            };
            let Some(chunk) = chunk else {
                break if found {
                    PassOutcome::Done { rescued }
                } else {
                    PassOutcome::NothingFound
                };
            };
            found = true;
            let b = if fwd {
                if chunk.start != pos {
                    eskip = self.geo.skip_initial;
                    prev_slow_skip = false;
                }
                Range::new(chunk.start, chunk_end_fwd(chunk.start, chunk.end, bs))
            } else {
                if chunk.end != pos {
                    eskip = self.geo.skip_initial;
                    prev_slow_skip = false;
                }
                Range::new(chunk_start_bwd(chunk.start, chunk.end, bs), chunk.end)
            };
            pos = if fwd { b.end } else { b.start };
            self.mf.current_pos = if fwd { b.start } else { b.end };
            let a = self.attempt(ctx, b, BlockStatus::NonTrimmed)?;
            rescued += a.rescued;
            if let Some(stop) = a.stop {
                break PassOutcome::Stopped(stop);
            }
            let p = &self.opts.profile;
            let mut slow_trigger = false;
            if a.slow {
                ctx.slow_reads += 1;
                ctx.consecutive_slow += 1;
                if p.max_slow_reads > 0 && ctx.slow_reads >= p.max_slow_reads {
                    break PassOutcome::Stopped(StopReason::Halt(format!(
                        "stopped after {} slow reads (max_slow_reads); the device may read \
                         faster after a rest or a power cycle; resume later",
                        ctx.slow_reads
                    )));
                }
                if ctx.consecutive_slow >= p.slow_reads_before_skip.max(1) {
                    slow_trigger = true;
                    ctx.consecutive_slow = 0;
                }
            } else if a.failed.is_none() && a.rescued > 0 {
                ctx.consecutive_slow = 0;
            }
            if a.failed.is_some() {
                ctx.copy_errors += 1;
                if p.max_errors > 0 && ctx.copy_errors >= p.max_errors {
                    break PassOutcome::Stopped(StopReason::Halt(format!(
                        "stopped after {} read errors in the copying passes (max_errors)",
                        ctx.copy_errors
                    )));
                }
            }
            if a.failed.is_some() || slow_trigger {
                if skipping {
                    let skip = if pass >= 2 {
                        NO_SKIP
                    } else if a.failed.is_some() {
                        let s = eskip;
                        eskip = self.grow_skip(eskip);
                        s
                    } else {
                        sskip = if prev_slow_skip {
                            self.grow_skip(sskip.max(self.geo.skip_initial))
                        } else {
                            // About one second worth of data at the current rate.
                            let rate = ctx.current_rate(Instant::now()) as u64;
                            align_up(rate.min(self.geo.skip_max), self.geo.sector)
                                .max(self.geo.skip_initial)
                        };
                        prev_slow_skip = true;
                        sskip
                    };
                    pos = self.skip_from(pos, skip, dir);
                }
            } else if a.rescued > 0 {
                eskip = self.geo.skip_initial;
                prev_slow_skip = false;
            }
        };
        ctx.slow_active = false;
        Ok(outcome)
    }

    fn trim_phase(&mut self, ctx: &mut Ctx, resume: &mut Resume) -> Result<Flow> {
        if !self.opts.profile.trim || !self.domain_has(&[BlockStatus::NonTrimmed]) {
            return Ok(Flow::Done);
        }
        let cursor = self.sweep_cursor(resume.take(CurrentStatus::Trimming).map(|(_, p)| p));
        self.enter_phase(ctx, Phase::Trimming, cursor)?;
        for (seg_start, seg_end) in self.segments(cursor) {
            let mut p = seg_start;
            while let Some(blk) = self.next_fwd(p, seg_end, &[BlockStatus::NonTrimmed]) {
                p = blk.end();
                if let Some(stop) = self.trim_block(ctx, blk.range())? {
                    return Ok(Flow::Stop(stop));
                }
            }
        }
        Ok(Flow::Done)
    }

    fn trim_block(&mut self, ctx: &mut Ctx, r: Range) -> Result<Option<StopReason>> {
        let unit = self.geo.unit;
        let mut pos = r.start;
        let mut lo = None;
        while pos < r.end {
            if let Some(stop) = self.checkpoint(ctx)? {
                return Ok(Some(stop));
            }
            let s = Range::new(pos, chunk_end_fwd(pos, r.end, unit));
            self.mf.current_pos = s.start;
            let a = self.attempt(ctx, s, BlockStatus::BadSector)?;
            if let Some(stop) = a.stop {
                return Ok(Some(stop));
            }
            if a.failed.is_some() {
                lo = Some(s.end);
                break;
            }
            pos = s.end;
        }
        let Some(lo) = lo else {
            return Ok(None);
        };
        let mut end = r.end;
        while end > lo {
            if let Some(stop) = self.checkpoint(ctx)? {
                return Ok(Some(stop));
            }
            let s = Range::new(chunk_start_bwd(lo, end, unit), end);
            self.mf.current_pos = s.end;
            let a = self.attempt(ctx, s, BlockStatus::BadSector)?;
            if let Some(stop) = a.stop {
                return Ok(Some(stop));
            }
            end = s.start;
            if a.failed.is_some() {
                break;
            }
        }
        if lo < end {
            self.mf.map.set_where(
                Range::new(lo, end),
                &[BlockStatus::NonTrimmed],
                BlockStatus::NonScraped,
            );
        }
        Ok(None)
    }

    /// Shared loop of dividing and scraping: read every stretch with one
    /// of `statuses` in `unit`-sized pieces, forwards, starting at the
    /// resume cursor and wrapping around once.
    fn piecewise_sweep(
        &mut self,
        ctx: &mut Ctx,
        cursor: u64,
        unit: u64,
        fail_status: BlockStatus,
    ) -> Result<Flow> {
        const TARGETS: [BlockStatus; 2] = [BlockStatus::NonTrimmed, BlockStatus::NonScraped];
        for (seg_start, seg_end) in self.segments(cursor) {
            let mut p = seg_start;
            loop {
                if let Some(stop) = self.checkpoint(ctx)? {
                    return Ok(Flow::Stop(stop));
                }
                let Some(blk) = self.next_fwd(p, seg_end, &TARGETS) else {
                    break;
                };
                let piece = Range::new(blk.pos, chunk_end_fwd(blk.pos, blk.end(), unit));
                p = piece.end;
                self.mf.current_pos = piece.start;
                let a = self.attempt(ctx, piece, fail_status)?;
                if let Some(stop) = a.stop {
                    return Ok(Flow::Stop(stop));
                }
            }
        }
        Ok(Flow::Done)
    }

    fn divide_phase(&mut self, ctx: &mut Ctx, resume: &mut Resume) -> Result<Flow> {
        const TARGETS: [BlockStatus; 2] = [BlockStatus::NonTrimmed, BlockStatus::NonScraped];
        if !self.opts.profile.divide || !self.domain_has(&TARGETS) {
            return Ok(Flow::Done);
        }
        if resume.active && resume.status == CurrentStatus::Scraping && resume.pass >= 2 {
            // The previous run had finished dividing and was scraping.
            return Ok(Flow::Done);
        }
        let cursor = self.sweep_cursor(resume.take(CurrentStatus::Scraping).map(|(_, p)| p));
        self.enter_phase(ctx, Phase::Dividing, cursor)?;
        let piece = self.geo.piece;
        self.piecewise_sweep(ctx, cursor, piece, BlockStatus::NonScraped)
    }

    fn scrape_phase(&mut self, ctx: &mut Ctx, resume: &mut Resume) -> Result<Flow> {
        const TARGETS: [BlockStatus; 2] = [BlockStatus::NonTrimmed, BlockStatus::NonScraped];
        if !self.opts.profile.scrape || !self.domain_has(&TARGETS) {
            return Ok(Flow::Done);
        }
        let cursor = self.sweep_cursor(resume.take(CurrentStatus::Scraping).map(|(_, p)| p));
        self.enter_phase(ctx, Phase::Scraping, cursor)?;
        let unit = self.geo.unit;
        self.piecewise_sweep(ctx, cursor, unit, BlockStatus::BadSector)
    }

    fn retry_phase(&mut self, ctx: &mut Ctx, resume: &mut Resume) -> Result<Flow> {
        let n = self.opts.profile.retry_passes;
        if n == 0 || !self.domain_has(&[BlockStatus::BadSector]) {
            return Ok(Flow::Done);
        }
        let (first, mut rpos) = match resume.take(CurrentStatus::Retrying) {
            Some((p, pos)) if (1..=n).contains(&p) => (p, Some(pos)),
            _ => (1, None),
        };
        let unit = self.geo.unit;
        let d = self.domain;
        for pass in first..=n {
            let dir = self.retry_direction(pass);
            let mut p = self.pass_start(dir, rpos.take());
            self.enter_phase(
                ctx,
                Phase::Retrying {
                    pass,
                    reverse: !dir.is_forward(),
                },
                p,
            )?;
            let mut recovered = 0u64;
            if dir.is_forward() {
                loop {
                    if let Some(stop) = self.checkpoint(ctx)? {
                        return Ok(Flow::Stop(stop));
                    }
                    let Some(blk) = self.next_fwd(p, d.end, &[BlockStatus::BadSector]) else {
                        break;
                    };
                    let piece = Range::new(blk.pos, chunk_end_fwd(blk.pos, blk.end(), unit));
                    p = piece.end;
                    self.mf.current_pos = piece.start;
                    let a = self.attempt(ctx, piece, BlockStatus::BadSector)?;
                    recovered += a.rescued;
                    if let Some(stop) = a.stop {
                        return Ok(Flow::Stop(stop));
                    }
                }
            } else {
                loop {
                    if let Some(stop) = self.checkpoint(ctx)? {
                        return Ok(Flow::Stop(stop));
                    }
                    let Some(blk) = self.next_bwd(p, &[BlockStatus::BadSector]) else {
                        break;
                    };
                    let piece = Range::new(chunk_start_bwd(blk.pos, blk.end(), unit), blk.end());
                    p = piece.start;
                    self.mf.current_pos = piece.end;
                    let a = self.attempt(ctx, piece, BlockStatus::BadSector)?;
                    recovered += a.rescued;
                    if let Some(stop) = a.stop {
                        return Ok(Flow::Stop(stop));
                    }
                }
            }
            if !self.domain_has(&[BlockStatus::BadSector]) {
                break;
            }
            if recovered == 0 {
                if pass < n {
                    ctx.info(format!(
                        "retry pass {pass} recovered nothing; not retrying further"
                    ));
                }
                break;
            }
        }
        Ok(Flow::Done)
    }

    // ----------------------------------------------------------------
    // Reading
    // ----------------------------------------------------------------

    /// One physical read of `range`, widened to whole sectors (clamped to
    /// the source size). `range` must be inside the source.
    fn raw_read(&self, buf: &mut Vec<u8>, range: Range, timeout: Duration) -> Raw {
        let sector = self.geo.sector;
        let a0 = align_down(range.start, sector);
        // Never past the source end (the map never extends beyond it).
        let a1 = align_up(range.end, sector).min(self.geo.size);
        if a1 <= a0 {
            return Raw::Eof;
        }
        let len = (a1 - a0) as usize;
        if buf.len() < len {
            buf.resize(len, 0);
        }
        let t0 = Instant::now();
        let r = if timeout.is_zero() {
            self.source.read_at(a0, &mut buf[..len])
        } else {
            self.source.read_at_timeout(a0, &mut buf[..len], timeout)
        };
        let latency = t0.elapsed();
        match r {
            Ok(n) => {
                let got_end = a0 + n.min(len) as u64;
                if got_end <= range.start {
                    if n == 0 {
                        Raw::Eof
                    } else {
                        Raw::Failed {
                            at: range.start,
                            kind: ReadErrorKind::Io,
                            detail: "short read ended before the requested range".into(),
                        }
                    }
                } else {
                    let end = got_end.min(range.end);
                    Raw::Data {
                        off: (range.start - a0) as usize,
                        len: (end - range.start) as usize,
                        latency,
                        short: end < range.end,
                    }
                }
            }
            Err(e) => Raw::Failed {
                at: e.offset,
                kind: e.kind,
                detail: e.detail,
            },
        }
    }

    fn is_slow(&self, ctx: &Ctx, len: u64, latency: Duration) -> bool {
        let threshold = match self.opts.profile.min_read_rate {
            None => return false,
            Some(0) => {
                if ctx.rescued_run < MIB {
                    return false;
                }
                let avg = ctx.average_rate(Instant::now());
                if avg <= 0.0 {
                    return false;
                }
                avg / 10.0
            }
            Some(r) => r as f64,
        };
        // Judge a short read by the time a full block may take, so the
        // fixed per-command latency of small reads does not count as slow.
        let allowed = len.max(self.geo.block_size) as f64 / threshold;
        latency.as_secs_f64() > allowed
    }

    /// Read `range` (inside one map block) and update the map: good data is
    /// written and marked finished; on failure the failed part becomes
    /// `fail_status` (or bad when it is at most one read unit). Handles
    /// short reads, prefix re-reads, device loss and the post-error actions.
    fn attempt(
        &mut self,
        ctx: &mut Ctx,
        range: Range,
        fail_status: BlockStatus,
    ) -> Result<Attempt> {
        let mut buf = std::mem::take(&mut ctx.buf);
        let r = self.attempt_inner(ctx, &mut buf, range, fail_status);
        ctx.buf = buf;
        r
    }

    fn attempt_inner(
        &mut self,
        ctx: &mut Ctx,
        buf: &mut Vec<u8>,
        range: Range,
        fail_status: BlockStatus,
    ) -> Result<Attempt> {
        let timeout = self.phase_timeout(ctx.phase);
        let check = self.opts.profile.check_on_error;
        let mut res = Attempt::default();
        let mut cur = range.start;
        let mut limit = range.end;
        let mut pending: Option<Failure> = None;
        let mut failure: Option<Failure> = None;
        loop {
            if cur >= limit {
                if let Some(p) = pending.take() {
                    failure = Some(p);
                }
                break;
            }
            match self.raw_read(buf, Range::new(cur, limit), timeout) {
                Raw::Data {
                    off,
                    len,
                    latency,
                    short,
                } => {
                    let here = Range::with_len(cur, len as u64);
                    let mut replacement: Option<Vec<u8>> = None;
                    if check
                        && ctx.after_error
                        && (ctx.unverified_error || is_uniform(&buf[off..off + len]))
                    {
                        // Bridges may return zeros/stale data right after an
                        // error. The first read after an error that no check
                        // read absorbed, and any all-0x00/0xFF block while
                        // errors persist, must be confirmed by a second read
                        // (or, if that differs, by two agreeing later reads).
                        ctx.unverified_error = false;
                        match self.verify_fresh_read(ctx, here, &buf[off..off + len], timeout) {
                            Fresh::Confirmed => {}
                            Fresh::Replaced(v) => replacement = Some(v),
                            Fresh::Inconsistent => {
                                ctx.warn(format!(
                                    "inconsistent data at 0x{cur:X} right after a read error; \
                                     not trusting it"
                                ));
                                failure = Some(Failure {
                                    at: cur,
                                    kind: ReadErrorKind::Io,
                                    detail: "inconsistent data after a read error".into(),
                                });
                                break;
                            }
                            Fresh::Unverified => {
                                failure = Some(Failure {
                                    at: cur,
                                    kind: ReadErrorKind::Io,
                                    detail: "data read right after an error could not be \
                                             verified (the second read failed)"
                                        .into(),
                                });
                                break;
                            }
                        }
                    }
                    let data: &[u8] = match &replacement {
                        Some(v) => v,
                        None => &buf[off..off + len],
                    };
                    if ctx.slow_active && self.is_slow(ctx, len as u64, latency) {
                        res.slow = true;
                    }
                    self.commit_good(ctx, cur, data)?;
                    if check {
                        ctx.remember_good(cur, data);
                    }
                    ctx.after_error = false;
                    ctx.consecutive_errors = 0;
                    ctx.phase_processed += len as u64;
                    res.rescued += len as u64;
                    cur += len as u64;
                    if short && cur < limit {
                        // A partial success: the source stopped at a
                        // failing sector. Keep the prefix and treat the
                        // rest as failed instead of reading straight into
                        // the bad area again (the rest is trimmed later).
                        ctx.after_error = true;
                        ctx.unverified_error = true;
                        if check {
                            self.check_last_good(ctx);
                        }
                        failure = Some(Failure {
                            at: cur,
                            kind: ReadErrorKind::Io,
                            detail: format!(
                                "the source returned {len} bytes and stopped at 0x{cur:X}"
                            ),
                        });
                        break;
                    }
                }
                Raw::Failed { at, kind, detail } => match kind {
                    ReadErrorKind::DeviceGone => match self.device_gone(ctx, cur, &detail)? {
                        None => continue,
                        Some(stop) => {
                            res.stop = Some(stop);
                            break;
                        }
                    },
                    ReadErrorKind::Io | ReadErrorKind::Timeout => {
                        if kind == ReadErrorKind::Timeout {
                            ctx.timeouts += 1;
                        }
                        // Everything read from now on follows an error.
                        ctx.after_error = true;
                        ctx.unverified_error = true;
                        if check {
                            // Re-read the last good block before anything
                            // else, so a stale answer lands on the check.
                            self.check_last_good(ctx);
                        }
                        let at = if kind == ReadErrorKind::Io {
                            align_down(at, self.geo.sector).clamp(cur, limit)
                        } else {
                            cur
                        };
                        if at > cur && pending.is_none() {
                            // The source says the bytes before `at` were
                            // fine, but a failed read does not promise they
                            // reached the buffer: read them again alone.
                            pending = Some(Failure { at, kind, detail });
                            limit = at;
                            continue;
                        }
                        failure = Some(Failure {
                            at: cur,
                            kind,
                            detail,
                        });
                        break;
                    }
                    ReadErrorKind::Unaligned => {
                        return Err(Error::Other(format!(
                            "unaligned read error at 0x{cur:X} ({detail}); is the sector size \
                             ({}) correct?",
                            self.geo.sector
                        )))
                    }
                    ReadErrorKind::OutOfRange | ReadErrorKind::Unsupported => {
                        return Err(Error::Other(format!(
                            "fatal error reading {} at 0x{cur:X}: {detail}",
                            self.source.describe()
                        )))
                    }
                },
                Raw::Eof => {
                    let detail = "the source returned no data before its end (device removed or \
                                  shrunk)";
                    match self.device_gone(ctx, cur, detail)? {
                        None => continue,
                        Some(stop) => {
                            res.stop = Some(stop);
                            break;
                        }
                    }
                }
            }
        }
        if let Some(f) = failure {
            let bad = Range::new(f.at, range.end);
            // A failed read of at most one unit is final (ddrescue), except
            // a timeout while copying: that only shows the device was slow,
            // so the block goes to trimming with its longer timeout.
            let copy_timeout =
                f.kind == ReadErrorKind::Timeout && fail_status == BlockStatus::NonTrimmed;
            let status = if bad.len() > self.geo.unit || copy_timeout {
                fail_status
            } else {
                BlockStatus::BadSector
            };
            self.mf.map.set_where(bad, &BlockStatus::UNFINISHED, status);
            ctx.phase_processed += bad.len();
            ctx.errors += 1;
            ctx.consecutive_errors += 1;
            ctx.after_error = true;
            let kind = match f.kind {
                ReadErrorKind::Timeout => "timeout",
                _ => "read error",
            };
            let msg = format!(
                "{kind} at 0x{:X} ({} bytes marked {}): {}",
                f.at,
                bad.len(),
                status.name(),
                f.detail
            );
            if ctx.errors <= LOUD_ERRORS {
                log::warn!("{msg}");
            } else {
                log::debug!("{msg}");
            }
            ctx.last_error = Some(msg);
            res.failed = Some(f);
            if let Some(stop) = self.after_error(ctx)? {
                res.stop = Some(stop);
            }
        }
        Ok(res)
    }

    /// Write good data and mark it finished. Bytes that are already
    /// finished are never overwritten (that would only happen through an
    /// internal bug; it is logged).
    fn commit_good(&mut self, ctx: &mut Ctx, pos: u64, data: &[u8]) -> Result<()> {
        let r = Range::with_len(pos, data.len() as u64);
        let whole = matches!(
            self.mf.map.block_at(pos),
            Some(b) if b.status != BlockStatus::Finished && b.end() >= r.end
        );
        if whole {
            self.write_out(pos, data)?;
            self.mf.map.set(r, BlockStatus::Finished);
            ctx.note_rescued(r.len());
            return Ok(());
        }
        log::error!(
            "internal: a read overlapped finished data at 0x{:X}..0x{:X}; keeping the existing data",
            r.start,
            r.end
        );
        let parts: Vec<Range> = self
            .mf
            .map
            .ranges_with(&BlockStatus::UNFINISHED)
            .intersection_with_range(r)
            .iter()
            .collect();
        for s in parts {
            let lo = (s.start - pos) as usize;
            let hi = (s.end - pos) as usize;
            self.write_out(s.start, &data[lo..hi])?;
            self.mf.map.set(s, BlockStatus::Finished);
            ctx.note_rescued(s.len());
        }
        Ok(())
    }

    fn write_out(&self, pos: u64, data: &[u8]) -> Result<()> {
        let off = self.opts.output_offset + pos;
        if self.opts.sparse_output
            && data.iter().all(|&b| b == 0)
            && self.output_is_zero(off, data.len())?
        {
            return Ok(());
        }
        write_all_at(&self.out, data, off).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!(
                    "writing {} bytes to {} at offset {off}: {e}",
                    data.len(),
                    self.output_path.display()
                ),
            ))
        })
    }

    /// True when the output reads as zeros (or lies past its end) in
    /// `off..off+len`.
    fn output_is_zero(&self, off: u64, len: usize) -> Result<bool> {
        let mut tmp = [0u8; 16 * 1024];
        let mut done = 0usize;
        while done < len {
            let want = (len - done).min(tmp.len());
            let n = match read_at(&self.out, &mut tmp[..want], off + done as u64) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(mapfile::io_ctx(e, &self.output_path)),
            };
            if n == 0 {
                return Ok(true);
            }
            if tmp[..n].iter().any(|&b| b != 0) {
                return Ok(false);
            }
            done += n;
        }
        Ok(true)
    }

    /// Read `range` again and compare with `data`.
    fn confirm_read(&self, ctx: &mut Ctx, range: Range, data: &[u8], timeout: Duration) -> Verify {
        let mut cb = std::mem::take(&mut ctx.check_buf);
        let v = match self.raw_read(&mut cb, range, timeout) {
            Raw::Data {
                off, len, short, ..
            } => {
                if !short && len == data.len() && cb[off..off + len] == *data {
                    Verify::Same
                } else if short {
                    Verify::Failed
                } else {
                    Verify::Different
                }
            }
            _ => Verify::Failed,
        };
        ctx.check_buf = cb;
        v
    }

    /// Confirm `data` (just read from `range` right after an error) with a
    /// second read; when that differs, a third read must agree with the
    /// second before its bytes are used.
    fn verify_fresh_read(
        &self,
        ctx: &mut Ctx,
        range: Range,
        data: &[u8],
        timeout: Duration,
    ) -> Fresh {
        match self.confirm_read(ctx, range, data, timeout) {
            Verify::Same => return Fresh::Confirmed,
            Verify::Failed => return Fresh::Unverified,
            Verify::Different => {}
        }
        // The second read is in check_buf; keep it and read a third time.
        let second = match self.last_confirm_data(ctx, range) {
            Some(v) => v,
            None => return Fresh::Unverified,
        };
        match self.confirm_read(ctx, range, &second, timeout) {
            Verify::Same => {
                log::info!(
                    "data at 0x{:X} differed on the first read after an error and agreed on \
                     the next two; using the agreeing data",
                    range.start
                );
                Fresh::Replaced(second)
            }
            Verify::Different => Fresh::Inconsistent,
            Verify::Failed => Fresh::Unverified,
        }
    }

    /// The bytes of the last `confirm_read` of `range` (still in check_buf).
    fn last_confirm_data(&self, ctx: &Ctx, range: Range) -> Option<Vec<u8>> {
        let a0 = align_down(range.start, self.geo.sector);
        let off = (range.start - a0) as usize;
        ctx.check_buf
            .get(off..off + range.len() as usize)
            .map(|s| s.to_vec())
    }

    /// `check_on_error`: re-read the last good block and compare it with
    /// what was written. A mismatch is re-read once; if it still differs
    /// the block is re-marked non-trimmed ("stale data after error").
    fn check_last_good(&mut self, ctx: &mut Ctx) {
        let Some((pos, data)) = ctx.last_good.take() else {
            return;
        };
        let r = Range::with_len(pos, data.len() as u64);
        let still_finished = matches!(
            self.mf.map.block_at(pos),
            Some(b) if b.status == BlockStatus::Finished && b.end() >= r.end
        );
        if !still_finished {
            return;
        }
        let timeout = self.phase_timeout(ctx.phase);
        ctx.unverified_error = false;
        match self.confirm_read(ctx, r, &data, timeout) {
            Verify::Same => {
                ctx.last_good = Some((pos, data));
                return;
            }
            Verify::Failed => {
                // A failed re-read says nothing about data that was read
                // successfully before (flaky media); keep it.
                log::debug!("check read of the last good block at 0x{pos:X} failed");
                ctx.last_good = Some((pos, data));
                return;
            }
            Verify::Different => {}
        }
        let msg = format!(
            "check read of the last good block at 0x{pos:X} returned different data; reading it \
             again"
        );
        log::warn!("{msg}");
        ctx.emit(Event::Log(log::Level::Warn, msg));
        if self.confirm_read(ctx, r, &data, timeout) != Verify::Different {
            ctx.last_good = Some((pos, data));
            return;
        }
        self.mf
            .map
            .set_where(r, &[BlockStatus::Finished], BlockStatus::NonTrimmed);
        ctx.stale_detections += 1;
        ctx.warn(format!(
            "stale data after error: the block at 0x{pos:X} ({} bytes) read back differently \
             twice; re-marked non-trimmed so it is read again (the device or bridge may return \
             stale data after errors)",
            data.len()
        ));
    }

    /// Actions after every failed attempt: pauses and re-open (the
    /// check-on-error re-read already happened right after the failing
    /// read).
    fn after_error(&mut self, ctx: &mut Ctx) -> Result<Option<StopReason>> {
        let p = &self.opts.profile;
        let pause_on_error = p.pause_on_error;
        let pause_after = p.pause_after_errors.filter(|n| *n > 0);
        let pause_duration = p.pause_duration;
        let reopen_after = p.reopen_after_errors.filter(|n| *n > 0);
        if !pause_on_error.is_zero() {
            ctx.emit(Event::Paused(pause_on_error));
            if ctx.sleep(pause_on_error) {
                return Ok(Some(StopReason::Cancelled));
            }
        }
        if let Some(n) = pause_after {
            if ctx.consecutive_errors % n as u64 == 0 && !pause_duration.is_zero() {
                ctx.info(format!(
                    "{} consecutive errors; pausing {:?} to let the device recover",
                    ctx.consecutive_errors, pause_duration
                ));
                ctx.emit(Event::Paused(pause_duration));
                if ctx.sleep(pause_duration) {
                    return Ok(Some(StopReason::Cancelled));
                }
            }
        }
        if let Some(n) = reopen_after {
            if ctx.consecutive_errors % n as u64 == 0 {
                match self.source.reopen() {
                    Ok(()) => ctx.info(format!(
                        "re-opened {} after {} consecutive errors",
                        self.source.describe(),
                        ctx.consecutive_errors
                    )),
                    Err(e) if e.kind == ReadErrorKind::Unsupported => {
                        log::debug!("re-open not supported: {e}")
                    }
                    Err(e) => log::info!("re-open after errors failed: {e}"),
                }
            }
        }
        Ok(None)
    }

    /// The device vanished while reading `at`: save, then poll
    /// `Source::reopen` until it is back. `Ok(None)` = back (retry the
    /// read); `Ok(Some(_))` = give up or cancelled.
    fn device_gone(&mut self, ctx: &mut Ctx, at: u64, detail: &str) -> Result<Option<StopReason>> {
        let p = &self.opts.profile;
        let max_reattaches = p.max_reattaches as u64;
        let wait = p.wait_for_reattach;
        let poll = self.opts.reattach_poll.max(Duration::from_millis(1));
        ctx.emit(Event::DeviceLost { at });
        let msg = format!("device lost while reading 0x{at:X}: {detail}");
        ctx.warn(msg);
        self.save_map(ctx)?;
        if max_reattaches > 0 && ctx.reattaches >= max_reattaches {
            return Ok(Some(StopReason::Halt(format!(
                "the device was lost {} times; giving up. Try another port, cable or card \
                 reader, then resume",
                ctx.reattaches + 1
            ))));
        }
        let t0 = Instant::now();
        loop {
            if ctx.control.is_cancelled() {
                return Ok(Some(StopReason::Cancelled));
            }
            match self.source.reopen() {
                Ok(()) => {
                    let now_size = self.source.size();
                    if now_size != self.geo.size {
                        return Ok(Some(StopReason::Halt(format!(
                            "the device came back with a different size ({now_size} bytes \
                             instead of {}); refusing to continue on what may be a different \
                             or failing device",
                            self.geo.size
                        ))));
                    }
                    ctx.reattaches += 1;
                    let after = t0.elapsed();
                    ctx.emit(Event::DeviceBack { after });
                    ctx.info(format!(
                        "device is back after {after:?}; continuing at 0x{at:X}"
                    ));
                    return Ok(None);
                }
                Err(e) if e.kind == ReadErrorKind::Unsupported => {
                    return Ok(Some(StopReason::Halt(format!(
                        "the source vanished and cannot be re-opened ({e}); resume when it is \
                         back"
                    ))));
                }
                Err(e) => log::debug!("re-open failed: {e}"),
            }
            if let Some(w) = wait {
                if t0.elapsed() >= w {
                    return Ok(Some(StopReason::Halt(format!(
                        "the device did not come back within {w:?}; resume when it is back"
                    ))));
                }
            }
            let mut nap = poll;
            if let Some(w) = wait {
                nap = nap.min(w.saturating_sub(t0.elapsed()).max(Duration::from_millis(1)));
            }
            if ctx.sleep(nap) {
                return Ok(Some(StopReason::Cancelled));
            }
        }
    }
}
