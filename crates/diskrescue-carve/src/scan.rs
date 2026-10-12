//! The [`Carver`]: scan loop, parallel measurement, ordered emission,
//! extraction and hashing.
//!
//! Threads during [`Carver::scan`]:
//!
//! * a reader thread fills chunk buffers (two buffers circulate, so one is
//!   read while the other is searched);
//! * the calling thread searches each chunk, probes candidates, dispatches
//!   measurements, reorders their results, runs the resolver and calls the
//!   callbacks (which therefore never need to be `Send`);
//! * the carver's rayon pool runs `FileFormat::measure`, one job per
//!   candidate start (all formats matching that start, best priority first).
//!
//! Jobs carry a sequence number assigned in start order; results go into a
//! small reorder buffer and are resolved strictly in sequence, so output is
//! deterministic and ascending by offset whatever the thread timing. The
//! number of jobs in flight is capped (four per worker); the scanner blocks
//! when the cap is reached, which keeps memory bounded on hostile inputs.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use diskrescue_core::format::{Confidence, ExtentStatus, FileFormat, Measured, Nested, NestedRole};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::{Error, Range, RangeSet, ReadError, Result, SharedSource, Source, SourceExt};

use crate::patterns::{Hit, PatternSet, SearchArea};
use crate::plan::{Planner, Window};
use crate::resolve::{Candidate, Child, Open, PendingState, Resolver, ResolverCfg, MAX_NESTED};
use crate::types::{
    CarveMode, CarveOptions, CarvedFile, ExtractReport, ScanProgress, ScanSummary, MAX_CHUNK_SIZE,
    MAX_PROBE_WINDOW, MIN_CHUNK_SIZE, PENALTY_ALLOCATED, PENALTY_CONFLICT, PENALTY_OFF_CLUSTER,
    PENALTY_UNREADABLE,
};
use crate::zerofill::ZeroFill;

/// Bytes searched per step inside a chunk; results are polled between steps.
const SUB_WINDOW: u64 = 256 << 10;

/// Nested objects measured per parent for promotion.
const MAX_CHILDREN: usize = 4096;

/// Notes kept from one measurement.
const MAX_FORMAT_NOTES: usize = 64;

/// Jobs in flight per worker before the scanner waits.
const IN_FLIGHT_PER_WORKER: usize = 4;

/// Read size used by `extract`.
const EXTRACT_CHUNK: u64 = 4 << 20;

/// A candidate waits for an unresolved candidate of a shared format that
/// started less than this many bytes earlier. If that one is a stream (or
/// a strong file), the new candidate can then be dropped without walking
/// the stream again; without the wait, every packet of a long stream that
/// was dispatched before the stream resolved would walk it to its end.
const WAIT_WINDOW: u64 = 64 << 10;

/// How far below `min_confidence` a measurement may be and still be kept
/// when its extent overlaps unreadable bytes. Formats lower their own
/// confidence for holes they see through `Source::unreadable` (JPEG by 10,
/// MP4 by 20), on top of the carver's [`PENALTY_UNREADABLE`]; without this
/// allowance a power-cut MP4 (no `moov`, confidence 25) with one bad sector
/// measures 5 and would be rejected outright.
const HOLE_ALLOWANCE: u8 = 20;

/// A dispatched candidate that is not resolved yet.
struct Pending {
    start: u64,
    fmts: Vec<usize>,
}

/// A registered format with its properties cached.
pub(crate) struct Fmt {
    pub f: Arc<dyn FileFormat>,
    pub id: &'static str,
    pub ext: &'static str,
    pub category: MediaCategory,
    pub priority: u8,
    pub min_size: u64,
    pub max_size: u64,
}

/// The allocation grid used for `aligned` and the R3/R4 rules.
#[derive(Debug, Clone, Copy)]
struct Grid {
    origin: u64,
    step: u64,
    cluster: bool,
}

impl Grid {
    fn new(opts: &CarveOptions, sector: u64) -> Self {
        match opts.cluster {
            Some((data_start, cluster)) => Grid {
                origin: data_start,
                step: cluster.max(1),
                cluster: true,
            },
            None => Grid {
                origin: 0,
                step: if opts.alignment > 1 {
                    opts.alignment
                } else {
                    sector
                },
                cluster: false,
            },
        }
    }

    fn on(&self, x: u64) -> bool {
        x >= self.origin && (x - self.origin) % self.step == 0
    }
}

/// Deterministic inputs to the confidence adjustments.
struct Env {
    known: RangeSet,
    allocated: Option<RangeSet>,
    grid: Grid,
}

impl Env {
    fn penalty(&self, start: u64, len: u64) -> u8 {
        let r = Range::with_len(start, len.max(1));
        let mut p = 0u8;
        if self.known.intersects(r) {
            p = p.saturating_add(PENALTY_UNREADABLE);
        }
        if self.allocated.as_ref().is_some_and(|a| a.intersects(r)) {
            p = p.saturating_add(PENALTY_ALLOCATED);
        }
        if self.grid.cluster && !self.grid.on(start) {
            p = p.saturating_add(PENALTY_OFF_CLUSTER);
        }
        p
    }
}

/// Shared, read-only context of measurement jobs.
struct JobCtx {
    src: SharedSource,
    fmts: Arc<Vec<Fmt>>,
    by_id: Arc<HashMap<&'static str, usize>>,
    min_confidence: Confidence,
    promote_nested: bool,
    abort: Arc<AtomicBool>,
}

impl JobCtx {
    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
}

struct Found {
    fmt: usize,
    m: Measured,
    children: Vec<Child>,
    children_measured: bool,
}

enum Outcome {
    Found(Box<Found>),
    Rejected,
    Aborted,
}

struct JobResult {
    seq: u64,
    start: u64,
    outcome: Outcome,
}

/// Call `measure` with panic protection and sanity checks on the result.
fn measure_guarded(ctx: &JobCtx, f: &Fmt, start: u64, limit: u64) -> Option<Measured> {
    let r = catch_unwind(AssertUnwindSafe(|| f.f.measure(&ctx.src, start, limit)));
    let mut m = match r {
        Ok(Ok(Some(m))) => m,
        Ok(Ok(None)) => return None,
        Ok(Err(e)) => {
            if !ctx.aborted() {
                log::debug!("{} measure at offset {start} failed: {e}", f.id);
            }
            return None;
        }
        Err(_) => {
            log::warn!(
                "{} measure panicked at offset {start}; candidate rejected",
                f.id
            );
            return None;
        }
    };
    if m.len == 0 {
        return None;
    }
    if m.len > limit {
        m.len = limit;
        m.status = ExtentStatus::Capped;
        m.notes
            .push("reported length exceeded the read limit; clamped".into());
    }
    m.confidence = Confidence::new(m.confidence.value());
    // Keep what a buggy or hostile format returns self-consistent and
    // bounded: nested ranges inside the file and well-formed (`Range` has
    // public fields, so `end < start` can be built), the error offset
    // inside the file, a bounded number of entries.
    let len = m.len;
    m.nested.truncate(MAX_NESTED);
    for n in &mut m.nested {
        n.range = Range::new(n.range.start.min(len), n.range.end.min(len));
    }
    m.nested.retain(|n| !n.range.is_empty());
    if let Some(e) = m.err_offset {
        m.err_offset = Some(e.min(len));
    }
    if m.notes.len() > MAX_FORMAT_NOTES {
        let extra = m.notes.len() - MAX_FORMAT_NOTES;
        m.notes.truncate(MAX_FORMAT_NOTES);
        m.notes.push(format!("{extra} further notes omitted"));
    }
    Some(m)
}

/// Measure the nested objects of a parent with their own formats.
fn measure_children(ctx: &JobCtx, pstart: u64, plen: u64, nested: &[Nested]) -> Vec<Child> {
    let size = ctx.src.size();
    let mut out = Vec::new();
    for n in nested.iter().take(MAX_CHILDREN) {
        if ctx.aborted() {
            break;
        }
        if n.role == NestedRole::Trailer || n.range.is_empty() || n.range.end > plen {
            continue;
        }
        let Some(&ci) = n.format.as_deref().and_then(|id| ctx.by_id.get(id)) else {
            continue;
        };
        let cstart = pstart + n.range.start;
        if cstart >= size {
            continue;
        }
        let cf = &ctx.fmts[ci];
        let limit = n.range.len().min(cf.max_size).min(size - cstart);
        if limit < cf.min_size.max(1) {
            continue;
        }
        if let Some(m) = measure_guarded(ctx, cf, cstart, limit) {
            out.push(Child {
                start: cstart,
                fmt: ci,
                role: n.role,
                m,
                conflict: false,
            });
        }
    }
    out
}

/// Body of one measurement job: formats in priority order, first acceptable
/// result wins (spec rule R1).
fn run_job(ctx: &JobCtx, start: u64, cands: &[(usize, u64)]) -> Outcome {
    for &(fi, limit) in cands {
        if ctx.aborted() {
            return Outcome::Aborted;
        }
        let f = &ctx.fmts[fi];
        let Some(m) = measure_guarded(ctx, f, start, limit) else {
            continue;
        };
        let floor = if ctx.src.overlaps_unreadable(start, m.len) {
            ctx.min_confidence.lowered(HOLE_ALLOWANCE)
        } else {
            ctx.min_confidence
        };
        if m.confidence < floor {
            log::trace!(
                "{} at {start}: confidence {} below minimum",
                f.id,
                m.confidence.value()
            );
            continue;
        }
        let want_children =
            !m.nested.is_empty() && (ctx.promote_nested || m.status != ExtentStatus::Complete);
        let children = if want_children {
            measure_children(ctx, start, m.len, &m.nested)
        } else {
            Vec::new()
        };
        return Outcome::Found(Box::new(Found {
            fmt: fi,
            m,
            children,
            children_measured: want_children,
        }));
    }
    if ctx.aborted() {
        Outcome::Aborted
    } else {
        Outcome::Rejected
    }
}

/// Signature scanner and extent resolver. See the crate docs.
pub struct Carver {
    opts: CarveOptions,
    fmts: Arc<Vec<Fmt>>,
    by_id: Arc<HashMap<&'static str, usize>>,
    patterns: PatternSet,
    pool: Arc<rayon::ThreadPool>,
    workers: usize,
    window_len: usize,
    /// `chunk_size` clamped to [`MAX_CHUNK_SIZE`].
    chunk: usize,
}

impl std::fmt::Debug for Carver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Carver")
            .field("opts", &self.opts)
            .field("workers", &self.workers)
            .field("chunk", &self.chunk)
            .field("probe_window", &self.window_len)
            .finish()
    }
}

impl Carver {
    /// Validate the options, compile the signature table and start the
    /// measurement pool.
    pub fn new(opts: CarveOptions) -> Result<Self> {
        if opts.registry.is_empty() {
            return Err(Error::InvalidArgument(
                "no formats registered for carving".into(),
            ));
        }
        if opts.chunk_size < MIN_CHUNK_SIZE {
            return Err(Error::InvalidArgument(format!(
                "chunk size {} is below the minimum of {MIN_CHUNK_SIZE} bytes",
                opts.chunk_size
            )));
        }
        if let Some((_, cluster)) = opts.cluster {
            if cluster == 0 {
                return Err(Error::InvalidArgument("cluster size 0".into()));
            }
        }
        let formats: Vec<Arc<dyn FileFormat>> = opts.registry.all().to_vec();
        let patterns = PatternSet::build(&formats, opts.alignment)?;
        let fmts: Vec<Fmt> = formats
            .iter()
            .map(|f| Fmt {
                f: f.clone(),
                id: f.id(),
                ext: f.extensions().first().copied().unwrap_or("bin"),
                category: f.category(),
                priority: f.priority(),
                min_size: f.min_size(),
                max_size: f.max_size(),
            })
            .collect();
        let by_id: HashMap<&'static str, usize> =
            fmts.iter().enumerate().map(|(i, f)| (f.id, i)).collect();
        let workers = if opts.workers == 0 {
            std::thread::available_parallelism().map_or(4, |n| n.get())
        } else {
            opts.workers
        }
        .clamp(1, 1024);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|i| format!("carve-measure-{i}"))
            .panic_handler(|_| log::error!("a carving measurement job panicked"))
            .build()
            .map_err(|e| Error::other(format!("cannot start measurement threads: {e}")))?;
        // Both sizes become allocations (two chunk buffers, each with the
        // window as overlap): clamp absurd values instead of trying them.
        let window_len = opts
            .probe_window
            .min(MAX_PROBE_WINDOW)
            .max(usize::try_from(patterns.max_probe_len()).unwrap_or(MAX_PROBE_WINDOW))
            .max(1);
        let chunk = opts.chunk_size.min(MAX_CHUNK_SIZE);
        log::debug!(
            "carver ready: {} formats, {workers} workers, chunk {chunk} bytes, probe window {window_len} bytes",
            fmts.len(),
        );
        Ok(Carver {
            opts,
            fmts: Arc::new(fmts),
            by_id: Arc::new(by_id),
            patterns,
            pool: Arc::new(pool),
            workers,
            window_len,
            chunk,
        })
    }

    /// The options this carver was built with.
    pub fn options(&self) -> &CarveOptions {
        &self.opts
    }

    /// Number of measurement threads.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Scan `src` and report every carved file, in ascending start order,
    /// through `on_file` (return `false` to stop). `on_progress` is called
    /// after every chunk and once at the end.
    ///
    /// Read errors never abort the scan: failing sectors are zero-filled,
    /// recorded and counted. Only an unrecoverable source failure (device
    /// gone and cannot be re-opened) returns an error. Cancellation returns
    /// `Ok` with `cancelled` set; the file the scan was inside of is still
    /// reported.
    pub fn scan(
        &self,
        src: &SharedSource,
        on_progress: &mut dyn FnMut(&ScanProgress),
        on_file: &mut dyn FnMut(CarvedFile) -> bool,
    ) -> Result<ScanSummary> {
        let started = Instant::now();
        let size = src.size();
        let whole = Range::new(0, size);
        let (scan_set, allocated) = match &self.opts.mode {
            CarveMode::Whole => (RangeSet::from_range(whole), None),
            CarveMode::Unallocated(set) => {
                let s = set.intersection_with_range(whole);
                let a = s.complement_within(whole);
                (s, Some(a))
            }
        };
        let known = src
            .unreadable()
            .map(|s| s.intersection_with_range(whole))
            .unwrap_or_default();
        let abort = Arc::new(AtomicBool::new(false));
        let fill = Arc::new(ZeroFill::new(src.clone(), known.clone(), abort.clone()));
        let fill_src: SharedSource = fill.clone();
        let sector = u64::from(src.sector_size().max(1));
        let grid = Grid::new(&self.opts, sector);
        let (res_tx, res_rx) = mpsc::channel();
        let job = Arc::new(JobCtx {
            src: fill_src,
            fmts: self.fmts.clone(),
            by_id: self.by_id.clone(),
            min_confidence: self.opts.min_confidence,
            promote_nested: self.opts.promote_nested,
            abort: abort.clone(),
        });
        let mut planner = Planner::new(
            &scan_set,
            size,
            self.chunk as u64,
            self.window_len as u64,
            sector,
        );
        let total = scan_set.total_len();
        log::debug!(
            "carving {}: {} bytes to scan of {size}, {} known unreadable",
            src.describe(),
            total,
            known.total_len()
        );
        let mut st = ScanState {
            c: self,
            src_size: size,
            fill: fill.clone(),
            job,
            scan_set,
            env: Env {
                known,
                allocated,
                grid,
            },
            cfg: ResolverCfg {
                min_confidence: self.opts.min_confidence,
                promote_nested: self.opts.promote_nested,
            },
            zero_skip: self.opts.zero_skip && self.patterns.zero_safe(),
            resolver: Resolver::default(),
            res_tx,
            res_rx,
            reorder: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_seq: 0,
            next_resolve: 0,
            in_flight: 0,
            max_in_flight: (self.workers * IN_FLIGHT_PER_WORKER).max(8),
            hits: Vec::new(),
            sum: ScanSummary::default(),
            total,
            next_id: 1,
            last_end: 0,
            stop: Stop::Running,
            fatal: None,
            abort,
            on_file,
            started,
        };
        st.run(&mut planner, on_progress);
        st.drain();
        if let Some(e) = st.fatal.take() {
            return Err(e);
        }
        if st.stop != Stop::Callback {
            let mut out = Vec::new();
            st.resolver.finish(&mut out);
            for o in out {
                st.emit(o);
            }
        }
        if st.stop == Stop::Running {
            st.sum.scanned = st.total;
        }
        on_progress(&st.progress());
        Ok(st.summary())
    }

    /// Copy the bytes of `file` from `src` to `sink`, zero-filling unreadable
    /// ranges (known from `src.unreadable()` or failing while reading).
    /// Stops early, with a short `bytes` count, if the source ends first.
    pub fn extract(
        src: &SharedSource,
        file: &CarvedFile,
        sink: &mut dyn Write,
    ) -> Result<ExtractReport> {
        let known = src.unreadable().unwrap_or_default();
        let fill = ZeroFill::new(src.clone(), known, Arc::new(AtomicBool::new(false)));
        let end = file.end().min(src.size());
        let mut buf = vec![0u8; EXTRACT_CHUNK.min(end.saturating_sub(file.start)).max(1) as usize];
        let mut pos = file.start;
        while pos < end {
            let want = (end - pos).min(buf.len() as u64) as usize;
            let n = fill.read_at(pos, &mut buf[..want])?;
            if n == 0 {
                break;
            }
            sink.write_all(&buf[..n])?;
            pos += n as u64;
        }
        sink.flush()?;
        let unreadable = fill
            .holes_within(Range::new(file.start, pos))
            .shifted_down(file.start);
        Ok(ExtractReport {
            bytes: pos.saturating_sub(file.start),
            unreadable,
        })
    }

    /// BLAKE3 (lowercase hex) of exactly the bytes [`Carver::extract`] would
    /// write for `file` (unreadable ranges hashed as zeros).
    pub fn hash(src: &SharedSource, file: &CarvedFile) -> Result<String> {
        // Through `extract` rather than a window onto the source: a window
        // clamps the sector-expanded reads of a direct-I/O device to the
        // file's (unaligned) end, which such a device refuses.
        let mut hasher = blake3::Hasher::new();
        Carver::extract(src, file, &mut hasher)?;
        Ok(hasher.finalize().to_hex().to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Running,
    /// The cancel flag was raised.
    Cancelled,
    /// `on_file` returned false.
    Callback,
    /// Unrecoverable source error.
    Failed,
}

struct ScanState<'a> {
    c: &'a Carver,
    src_size: u64,
    fill: Arc<ZeroFill>,
    job: Arc<JobCtx>,
    scan_set: RangeSet,
    env: Env,
    cfg: ResolverCfg,
    zero_skip: bool,
    resolver: Resolver,
    res_tx: mpsc::Sender<JobResult>,
    res_rx: mpsc::Receiver<JobResult>,
    reorder: BTreeMap<u64, JobResult>,
    pending: BTreeMap<u64, Pending>,
    next_seq: u64,
    next_resolve: u64,
    in_flight: usize,
    max_in_flight: usize,
    hits: Vec<Hit>,
    sum: ScanSummary,
    total: u64,
    next_id: u64,
    last_end: u64,
    stop: Stop,
    fatal: Option<Error>,
    abort: Arc<AtomicBool>,
    on_file: &'a mut dyn FnMut(CarvedFile) -> bool,
    started: Instant,
}

type Filled = (Window, std::result::Result<(Vec<u8>, usize), ReadError>);

fn reader_loop(
    fill: Arc<ZeroFill>,
    req_rx: mpsc::Receiver<Window>,
    free_rx: mpsc::Receiver<Vec<u8>>,
    full_tx: mpsc::Sender<Filled>,
) {
    for w in req_rx {
        let Ok(mut buf) = free_rx.recv() else {
            return;
        };
        let len = w.read.len() as usize;
        if buf.len() < len {
            buf.resize(len, 0);
        }
        let r = fill
            .read_at(w.read.start, &mut buf[..len])
            .map(|n| (buf, n));
        if full_tx.send((w, r)).is_err() {
            return;
        }
    }
}

impl ScanState<'_> {
    fn stopped(&self) -> bool {
        self.stop != Stop::Running
    }

    fn halt(&mut self, why: Stop) {
        if self.stop == Stop::Running {
            self.stop = why;
            self.abort.store(true, Ordering::Relaxed);
        }
    }

    fn check_cancel(&mut self) {
        if self
            .c
            .opts
            .cancel
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
        {
            self.halt(Stop::Cancelled);
        }
    }

    fn progress(&self) -> ScanProgress {
        let secs = self.started.elapsed().as_secs_f64();
        ScanProgress {
            scanned: self.sum.scanned,
            total: self.total,
            candidates: self.sum.candidates,
            files: self.sum.files,
            rate: if secs > 0.0 {
                self.sum.scanned as f64 / secs
            } else {
                0.0
            },
            current_format: self
                .resolver
                .open()
                .map(|o| self.c.fmts[o.fmt].id.to_string()),
        }
    }

    /// Reader thread + window loop.
    fn run(&mut self, planner: &mut Planner, on_progress: &mut dyn FnMut(&ScanProgress)) {
        std::thread::scope(|scope| {
            let (req_tx, req_rx) = mpsc::channel::<Window>();
            let (full_tx, full_rx) = mpsc::channel::<Filled>();
            let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>();
            for _ in 0..2 {
                let _ = free_tx.send(Vec::new());
            }
            let fill = self.fill.clone();
            scope.spawn(move || reader_loop(fill, req_rx, free_rx, full_tx));

            let mut outstanding = 0usize;
            if let Some(w) = planner.next(0) {
                if req_tx.send(w).is_ok() {
                    outstanding += 1;
                }
            }
            while outstanding > 0 && !self.stopped() {
                // Ask for the next window before searching this one so the
                // read overlaps the search. Nothing is skipped: even a
                // complete, confident file may hide another file whose
                // header its length field overran (resolver rules P2, R2c).
                if let Some(w) = planner.next(0) {
                    if req_tx.send(w).is_ok() {
                        outstanding += 1;
                    }
                }
                let Ok((w, r)) = full_rx.recv() else {
                    self.fatal = Some(Error::other("carving reader thread stopped"));
                    self.halt(Stop::Failed);
                    break;
                };
                outstanding -= 1;
                match r {
                    Ok((buf, n)) => {
                        self.process_window(&w, &buf[..n.min(buf.len())]);
                        let _ = free_tx.send(buf);
                    }
                    Err(e) => {
                        if !self.stopped() {
                            log::error!("carving stopped: {e}");
                            self.fatal = Some(e.into());
                            self.halt(Stop::Failed);
                        }
                        break;
                    }
                }
                if !self.stopped() {
                    self.sum.scanned = self.sum.scanned.max(w.covered_after);
                }
                self.check_cancel();
                on_progress(&self.progress());
            }
            // Dropping the channels (end of this closure) lets the reader
            // thread finish; an abort makes any read it is in return early.
            if self.stopped() {
                self.abort.store(true, Ordering::Relaxed);
            }
        });
    }

    /// Search one chunk buffer (`data` starts at `w.read.start`).
    fn process_window(&mut self, w: &Window, data: &[u8]) {
        let parts = self.scan_set.intersection_with_range(w.own);
        for part in parts.iter() {
            let mut a = part.start;
            while a < part.end {
                self.poll_results();
                self.check_cancel();
                if self.stopped() {
                    return;
                }
                let b = a.saturating_add(SUB_WINDOW).min(part.end);
                let own_start = a;
                if own_start < b {
                    let mut hits = std::mem::take(&mut self.hits);
                    hits.clear();
                    self.c.patterns.search(
                        data,
                        SearchArea {
                            buf_base: w.read.start,
                            own_start,
                            own_end: b,
                            zero_skip: self.zero_skip,
                        },
                        &mut hits,
                    );
                    let fmts = &self.c.fmts;
                    hits.sort_unstable_by_key(|h| (h.start, Reverse(fmts[h.fmt].priority), h.fmt));
                    hits.dedup();
                    self.handle_hits(&hits, w.read.start, data);
                    self.hits = hits;
                }
                a = b;
            }
        }
    }

    fn handle_hits(&mut self, hits: &[Hit], base: u64, data: &[u8]) {
        let mut group: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < hits.len() && !self.stopped() {
            let start = hits[i].start;
            group.clear();
            while i < hits.len() && hits[i].start == start {
                group.push(hits[i].fmt);
                i += 1;
            }
            self.candidate(start, &group, base, data);
        }
    }

    /// Probe one candidate start and dispatch its measurement.
    fn candidate(&mut self, start: u64, group: &[usize], base: u64, data: &[u8]) {
        if self.env.known.contains(start) {
            return;
        }
        self.sum.candidates += 1;
        let fmts = self.c.fmts.clone();
        if self.prefilter(start, group, &fmts) {
            return;
        }
        // Let a nearby unresolved candidate of a shared format resolve first
        // (see WAIT_WINDOW), then ask the prefilter again.
        let mut waited = false;
        loop {
            if self.stopped() {
                return;
            }
            let blocker = self
                .pending
                .iter()
                .rev()
                .find(|(_, p)| {
                    start.saturating_sub(p.start) < WAIT_WINDOW
                        && p.fmts.iter().any(|f| group.contains(f))
                })
                .map(|(&seq, _)| seq);
            let Some(seq) = blocker else {
                break;
            };
            waited = true;
            while self.pending.contains_key(&seq) && !self.stopped() {
                self.wait_one();
            }
        }
        if waited && self.prefilter(start, group, &fmts) {
            return;
        }
        let off = (start - base) as usize;
        let head = &data[off..data.len().min(off.saturating_add(self.c.window_len))];
        let mut jobs: Vec<(usize, u64)> = Vec::with_capacity(group.len());
        for &fi in group {
            let f = &fmts[fi];
            let ok = catch_unwind(AssertUnwindSafe(|| f.f.probe(head))).unwrap_or_else(|_| {
                log::warn!(
                    "{} probe panicked at offset {start}; candidate rejected",
                    f.id
                );
                false
            });
            if !ok {
                continue;
            }
            let mut limit = f.max_size.min(self.src_size - start);
            if let Some(cap) = self.c.opts.max_file_size {
                limit = limit.min(cap);
            }
            if limit < f.min_size.max(1) {
                continue;
            }
            jobs.push((fi, limit));
        }
        if jobs.is_empty() {
            self.sum.false_positives += 1;
            return;
        }
        log::trace!("candidate at {start}: {:?}", jobs);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(
            seq,
            Pending {
                start,
                fmts: jobs.iter().map(|j| j.0).collect(),
            },
        );
        let ctx = self.job.clone();
        let tx = self.res_tx.clone();
        self.c.pool.spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| run_job(&ctx, start, &jobs)))
                .unwrap_or(Outcome::Rejected);
            let _ = tx.send(JobResult {
                seq,
                start,
                outcome,
            });
        });
        self.in_flight += 1;
        while self.in_flight >= self.max_in_flight && !self.stopped() {
            self.wait_one();
        }
    }

    /// Drop a candidate without measuring it when the resolver would
    /// suppress it anyway (see `Resolver::prefilter_drops`).
    fn prefilter(&mut self, start: u64, group: &[usize], fmts: &[Fmt]) -> bool {
        let pending = match self.resolver.open() {
            Some(o) => PendingState {
                inside: self.pending.values().any(|p| {
                    p.start > o.start && p.start < o.end() && !o.nested_set.contains(p.start)
                }),
                other_format: self
                    .pending
                    .values()
                    .any(|p| p.fmts.iter().any(|&f| f != o.fmt)),
            },
            None => PendingState {
                inside: true,
                other_format: true,
            },
        };
        let aligned = self.env.grid.on(start);
        let drop = self
            .resolver
            .prefilter_drops(start, aligned, group, fmts, &self.cfg, pending);
        if drop {
            self.resolver.suppressed += 1;
        }
        drop
    }

    fn poll_results(&mut self) {
        while let Ok(r) = self.res_rx.try_recv() {
            self.in_flight -= 1;
            self.handle_result(r);
        }
    }

    /// Block until one result arrives (polling the cancel flag meanwhile).
    fn wait_one(&mut self) {
        loop {
            match self.res_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => {
                    self.in_flight -= 1;
                    self.handle_result(r);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.check_cancel();
                    if self.stopped() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Wait for every job still running (their results are resolved unless
    /// the scan was stopped).
    fn drain(&mut self) {
        while self.in_flight > 0 {
            match self.res_rx.recv() {
                Ok(r) => {
                    self.in_flight -= 1;
                    if !self.stopped() {
                        self.handle_result(r);
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn handle_result(&mut self, r: JobResult) {
        if self.stopped() {
            return;
        }
        self.reorder.insert(r.seq, r);
        while let Some(r) = self.reorder.remove(&self.next_resolve) {
            self.next_resolve += 1;
            self.resolve(r);
            if self.stopped() {
                return;
            }
        }
    }

    fn resolve(&mut self, r: JobResult) {
        self.pending.remove(&r.seq);
        let found = match r.outcome {
            Outcome::Found(f) => *f,
            Outcome::Rejected => {
                self.sum.false_positives += 1;
                return;
            }
            Outcome::Aborted => return,
        };
        let Found {
            fmt,
            m,
            children,
            children_measured,
        } = found;
        // `run_job` already applied `min_confidence` to the format's own
        // verdict. The carver's adjustments only lower the reported
        // confidence: a damaged file (unreadable sectors, a guessed extent
        // running into allocated space) is still a file, and dropping it
        // here would silently lose a power-cut video with one bad sector.
        let env_penalty = self.env.penalty(r.start, m.len);
        let cand = Candidate {
            start: r.start,
            fmt,
            m,
            aligned: self.env.grid.on(r.start),
            env_penalty,
            children,
            children_measured,
        };
        let mut out = Vec::new();
        let env = &self.env;
        self.resolver.push(
            cand,
            &self.c.fmts,
            &self.cfg,
            &|s, l| env.penalty(s, l),
            &mut out,
        );
        for o in out {
            self.emit(o);
        }
    }

    /// Finalise a closed file: confidence adjustments, promotions, callback.
    fn emit(&mut self, mut o: Open) {
        if self.stop == Stop::Callback {
            return;
        }
        let fmts = self.c.fmts.clone();
        let f = &fmts[o.fmt];
        let range = Range::new(o.start, o.end());
        let holes = self.fill.holes_within(range);
        let mut notes = o.take_notes();
        let mut penalty = o.res_penalty;
        if !holes.is_empty() {
            penalty = penalty.saturating_add(PENALTY_UNREADABLE);
            notes.push(format!(
                "{} unreadable bytes inside (zero-filled on extraction)",
                holes.total_len()
            ));
        }
        if self
            .env
            .allocated
            .as_ref()
            .is_some_and(|a| a.intersects(range))
        {
            penalty = penalty.saturating_add(PENALTY_ALLOCATED);
            notes.push("extends into allocated space".into());
        }
        if self.env.grid.cluster && !self.env.grid.on(o.start) {
            penalty = penalty.saturating_add(PENALTY_OFF_CLUSTER);
            notes.push("header is not on a cluster boundary".into());
        }
        if o.conflicts > 0 {
            // The note was added when the conflict was found.
            penalty = penalty.saturating_add(PENALTY_CONFLICT);
        }
        // Reported, never a reason to drop the file (see `resolve`).
        let confidence = o.conf_raw.lowered(penalty);
        if confidence < self.cfg.min_confidence {
            notes.push(format!(
                "confidence {} after the carver's adjustments (the format measured {})",
                confidence.value(),
                o.conf_raw.value()
            ));
        }
        let kids = self.promotions(&mut o);
        let id = self.next_id;
        self.next_id += 1;
        debug_assert!(o.start >= self.last_end, "overlapping output");
        self.last_end = o.end();
        self.sum.bytes_attributed += o.len;
        let len = o.len;
        let file = CarvedFile {
            id,
            format: f.id.to_string(),
            extension: f.ext.to_string(),
            category: f.category,
            start: o.start,
            len,
            status: o.status,
            confidence,
            err_offset: o.err_offset,
            meta: std::mem::take(&mut o.meta),
            nested: o.nested.drain(..).filter(|n| n.range.end <= len).collect(),
            notes,
            unreadable: holes.shifted_down(o.start),
            parent: None,
            aligned: o.aligned,
        };
        if !self.deliver(file) {
            return;
        }
        for ch in kids {
            if !self.emit_child(ch, (id, f.id, o.start)) {
                return;
            }
        }
    }

    /// Emit a promoted nested object or a conflict child of the emitted
    /// parent `(id, format, start)`. Returns false when the callback asked
    /// to stop.
    fn emit_child(&mut self, ch: Child, parent: (u64, &str, u64)) -> bool {
        let fmts = self.c.fmts.clone();
        let cf = &fmts[ch.fmt];
        let range = Range::with_len(ch.start, ch.m.len);
        let holes = self.fill.holes_within(range);
        let mut m = ch.m;
        let mut penalty = 0u8;
        if !holes.is_empty() {
            penalty = penalty.saturating_add(PENALTY_UNREADABLE);
            m.notes.push(format!(
                "{} unreadable bytes inside (zero-filled on extraction)",
                holes.total_len()
            ));
        }
        if ch.conflict {
            // A file in its own right: the same adjustments as a top-level
            // file.
            if self
                .env
                .allocated
                .as_ref()
                .is_some_and(|a| a.intersects(range))
            {
                penalty = penalty.saturating_add(PENALTY_ALLOCATED);
                m.notes.push("extends into allocated space".into());
            }
            if self.env.grid.cluster && !self.env.grid.on(ch.start) {
                penalty = penalty.saturating_add(PENALTY_OFF_CLUSTER);
                m.notes.push("header is not on a cluster boundary".into());
            }
        }
        let confidence = m.confidence.lowered(penalty);
        let (pid, pfmt, pstart) = parent;
        if ch.conflict {
            m.notes.push(format!(
                "found inside the extent of file #{pid} ({pfmt} at offset {pstart}), which cannot contain a {} file: that file's length is probably wrong",
                cf.id
            ));
        } else {
            m.notes.push(format!(
                "promoted from {} of file #{pid} ({pfmt} at offset {pstart})",
                role_name(ch.role)
            ));
        }
        let clen = m.len;
        let child = CarvedFile {
            id: self.next_id,
            format: cf.id.to_string(),
            extension: cf.ext.to_string(),
            category: cf.category,
            start: ch.start,
            len: clen,
            status: m.status,
            confidence,
            err_offset: m.err_offset,
            meta: m.meta,
            nested: m
                .nested
                .into_iter()
                .filter(|n| n.range.end <= clen)
                .collect(),
            notes: m.notes,
            unreadable: holes.shifted_down(ch.start),
            parent: Some(pid),
            aligned: self.env.grid.on(ch.start),
        };
        self.next_id += 1;
        self.deliver(child)
    }

    /// Nested objects of `o` to emit as files of their own: conflict
    /// children always, measured nested objects per spec rule R5.
    fn promotions(&mut self, o: &mut Open) -> Vec<Child> {
        let damaged = matches!(o.status, ExtentStatus::Truncated | ExtentStatus::Capped);
        let harvest = self.cfg.promote_nested || damaged;
        if harvest && !o.children_measured && !o.nested.is_empty() && !self.job.aborted() {
            // Conflict children are measured already.
            let known: std::collections::BTreeSet<u64> = o
                .children
                .iter()
                .map(|c| c.start.saturating_sub(o.start))
                .collect();
            let todo: Vec<Nested> = o
                .nested
                .iter()
                .filter(|n| !known.contains(&n.range.start))
                .cloned()
                .collect();
            let extra = measure_children(&self.job, o.start, o.len, &todo);
            o.children.extend(extra);
            o.children_measured = true;
        }
        let mut kids = std::mem::take(&mut o.children);
        // Stable: at equal starts the child recorded during resolution (a
        // conflict) comes first and is the one kept.
        kids.sort_by_key(|c| c.start);
        kids.dedup_by_key(|c| c.start);
        let end = o.end();
        let fmts = &self.c.fmts;
        let min = self.cfg.min_confidence;
        let all = self.cfg.promote_nested;
        kids.retain(|ch| {
            let cf = &fmts[ch.fmt];
            let inside = ch.start >= o.start && ch.start.saturating_add(ch.m.len) <= end;
            if ch.conflict {
                return inside;
            }
            let ok = if all {
                ch.m.confidence >= min
            } else {
                ch.m.status == ExtentStatus::Complete
                    && matches!(cf.category, MediaCategory::Photo | MediaCategory::RawPhoto)
                    && ch.m.confidence >= min
            };
            harvest && inside && ok && ch.role != NestedRole::Trailer
        });
        kids
    }

    fn deliver(&mut self, file: CarvedFile) -> bool {
        self.sum.files += 1;
        *self.sum.by_format.entry(file.format.clone()).or_insert(0) += 1;
        log::debug!(
            "carved #{} {} at {} len {} ({:?}, confidence {})",
            file.id,
            file.format,
            file.start,
            file.len,
            file.status,
            file.confidence.value()
        );
        let keep = (self.on_file)(file);
        if !keep {
            self.halt(Stop::Callback);
        }
        keep
    }

    fn summary(&mut self) -> ScanSummary {
        let mut bad = self.env.known.clone();
        let discovered = self.fill.discovered();
        for r in discovered.iter() {
            bad.insert(r);
        }
        if !discovered.is_empty() {
            log::warn!(
                "{} unreadable bytes discovered while carving (zero-filled)",
                discovered.total_len()
            );
        }
        let mut s = std::mem::take(&mut self.sum);
        s.unreadable_skipped = bad.intersection(&self.scan_set).total_len();
        s.elapsed = self.started.elapsed();
        s.cancelled = matches!(self.stop, Stop::Cancelled | Stop::Callback);
        log::debug!(
            "carving done: {} bytes scanned, {} candidates, {} files, {} rejected, {} suppressed, {:.1?}",
            s.scanned,
            s.candidates,
            s.files,
            s.false_positives,
            self.resolver.suppressed,
            s.elapsed
        );
        s
    }
}

fn role_name(r: NestedRole) -> &'static str {
    match r {
        NestedRole::Thumbnail => "the thumbnail",
        NestedRole::Preview => "a preview",
        NestedRole::CoverArt => "the cover art",
        NestedRole::Frame => "a frame",
        NestedRole::Trailer => "the trailer",
        NestedRole::Container => "an embedded object",
    }
}
