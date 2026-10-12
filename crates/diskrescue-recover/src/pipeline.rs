//! The recovery pipeline.
//!
//! ```text
//! source ─► partitions/volumes ─► filesystem parsers ─► select ─► extract (parallel)
//!    │                                   │
//!    │                                   └─► free space ─┐
//!    └──────────── areas no parser understood ───────────┴─► carver ─► extract + dedup
//!                                                                         │
//!            report ◄── repair (donors) ◄── validate (parallel) ◄─────────┘
//! ```
//!
//! Nothing here writes to the source. Every output file is created new
//! inside the output tree; damaged originals are kept next to their repaired
//! copies so a bad repair can never cost data.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use diskrescue_core::format::{
    FileFormat, FormatRegistry, RepairContext, RepairOutcome, RepairResult, Validation,
    ValidationStatus,
};
use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem, FsKind, WalkControl};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::partition::{PartitionScheme, PartitionTable};
use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::{FileSource, SubSource};
use diskrescue_core::{Error, Result, SharedSource, Source, SourceExt};

use crate::dedup::Dedup;
use crate::donors::{DonorCandidate, DonorPool};
use crate::filters::{category_for_extension, Selection};
use crate::fsopen::open_filesystem;
use crate::naming::extension_of;
use crate::output::{set_file_times, OutputTree};
use crate::registry;
use crate::report::{
    CarveRecord, FileRecord, Origin, Outcome, RepairRecord, Report, SourceRecord, ValidationRecord,
    VolumeRecord,
};
use crate::sink::{HashingWriter, HeadSink};

/// How much of the source to carve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarvePolicy {
    /// Do not carve.
    Off,
    /// Free space of every parsed filesystem only.
    FreeSpace,
    /// The whole source, ignoring filesystems.
    Everything,
    /// Free space of parsed filesystems plus every area no parser understood
    /// (unpartitioned gaps, damaged or unknown volumes). The whole source when
    /// nothing could be parsed. The default.
    Auto,
}

/// Options for [`recover`].
#[derive(Debug, Clone)]
pub struct RecoverOptions {
    /// Output directory (must not be on the source device; the caller checks).
    pub output: PathBuf,
    /// What to recover.
    pub selection: Selection,
    /// Carving policy.
    pub carve: CarvePolicy,
    /// Validate recovered files.
    pub validate: bool,
    /// Attempt repairs on damaged files.
    pub repair: bool,
    /// Healthy reference files supplied by the user for repairs.
    pub donors: Vec<PathBuf>,
    /// Allow repairs that cut or pad content.
    pub aggressive_repair: bool,
    /// Skip carved files whose content was already recovered.
    pub dedup: bool,
    /// Worker threads (0 = number of CPUs).
    pub workers: usize,
    /// Logical sector size for partition tables (0 = ask the source).
    pub sector_size: u32,
    /// Scan for lost partitions when no table/filesystem is found.
    pub scan_lost_partitions: bool,
    /// Read filesystems. When false, only carving runs (over the whole
    /// source unless `carve` is `Off`).
    pub use_filesystems: bool,
    /// Set to stop early; partial results are still reported.
    pub cancel: Arc<AtomicBool>,
    /// Command line for the report.
    pub command_line: Option<String>,
}

impl RecoverOptions {
    /// Defaults: photos and videos, auto carving, validate, repair, dedup.
    pub fn new(output: impl Into<PathBuf>) -> Self {
        RecoverOptions {
            output: output.into(),
            selection: Selection::default(),
            carve: CarvePolicy::Auto,
            validate: true,
            repair: true,
            donors: Vec::new(),
            aggressive_repair: false,
            dedup: true,
            workers: 0,
            sector_size: 0,
            scan_lost_partitions: true,
            use_filesystems: true,
            cancel: Arc::new(AtomicBool::new(false)),
            command_line: None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Pipeline stage, for progress display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Reading partition tables / scanning for volumes.
    Partitions,
    /// Listing a filesystem.
    Listing {
        /// Volume index.
        volume: u32,
        /// Filesystem.
        fs: FsKind,
    },
    /// Extracting files found through filesystems.
    Extracting,
    /// Carving.
    Carving,
    /// Validating recovered files.
    Validating,
    /// Repairing damaged files.
    Repairing,
    /// Writing the report.
    Report,
}

/// Progress and status events.
#[derive(Debug, Clone)]
pub enum Event {
    /// A stage started.
    Stage(Stage),
    /// A volume was examined.
    Volume(VolumeRecord),
    /// Counter progress within the current stage.
    Progress {
        /// Items done.
        done: u64,
        /// Items in total (0 = unknown).
        total: u64,
        /// Bytes processed, when meaningful.
        bytes: u64,
    },
    /// Carver progress.
    Carve {
        /// Bytes scanned.
        scanned: u64,
        /// Bytes to scan.
        total: u64,
        /// Files found so far.
        files: u64,
    },
    /// A file finished (output path relative to the output root).
    File {
        /// Output path or origin text.
        name: String,
        /// Outcome.
        outcome: Outcome,
    },
    /// Something the user should know.
    Warning(String),
}

/// Receives events. Must be cheap: called from worker threads.
pub trait Observer: Send + Sync {
    /// Handle one event.
    fn event(&self, e: Event);
}

/// Ignores everything.
pub struct NoopObserver;

impl Observer for NoopObserver {
    fn event(&self, _e: Event) {}
}

/// A volume whose filesystem opened.
pub struct ParsedVolume {
    /// 1-based index (matches the report).
    pub index: u32,
    /// Byte offset of the volume in the source.
    pub start: u64,
    /// Size in bytes.
    pub size: u64,
    /// Output directory name.
    pub dir: String,
    /// The parser.
    pub fs: Box<dyn FileSystem>,
}

/// What [`discover`] found.
pub struct Discovery {
    /// Partition table (possibly empty).
    pub table: PartitionTable,
    /// One record per volume examined.
    pub volumes: Vec<VolumeRecord>,
    /// Volumes with a working parser.
    pub parsed: Vec<ParsedVolume>,
    /// Warnings.
    pub warnings: Vec<String>,
}

/// Read partition tables, probe and open every volume.
pub fn discover(
    src: &SharedSource,
    sector_size: u32,
    scan_lost: bool,
    cancel: &Arc<AtomicBool>,
    obs: &dyn Observer,
) -> Discovery {
    obs.event(Event::Stage(Stage::Partitions));
    let sector = if sector_size > 0 {
        sector_size
    } else {
        src.sector_size().max(512)
    };
    let mut warnings = Vec::new();
    let (table, mut vols) = match diskrescue_partition::volumes(src, sector) {
        Ok(x) => x,
        Err(e) => {
            warnings.push(format!("partition table could not be read: {e}"));
            (PartitionTable::empty(PartitionScheme::Unknown), Vec::new())
        }
    };
    warnings.extend(table.warnings.iter().cloned());

    let nothing_usable = vols.iter().all(|v| v.probe.is_none());
    if nothing_usable && scan_lost && !cancel.load(Ordering::Relaxed) {
        obs.event(Event::Warning(
            "no readable partition table or filesystem; scanning the whole source for lost volumes"
                .into(),
        ));
        let opts = diskrescue_partition::ScanOptions {
            step: 4096,
            start: 0,
            end: None,
            cancel: Some(cancel.clone()),
            max_results: 64,
        };
        let total = src.size();
        let mut progress = |done: u64, _total: u64| {
            obs.event(Event::Progress {
                done,
                total,
                bytes: done,
            })
        };
        match diskrescue_partition::scan_lost_partitions(src, &opts, &mut progress) {
            Ok(found) => {
                for entry in found {
                    let probe = diskrescue_partition::probe_filesystem(src, entry.start);
                    let source: SharedSource =
                        Arc::new(SubSource::new(src.clone(), entry.start, entry.size));
                    vols.push(diskrescue_partition::Volume {
                        entry,
                        source,
                        probe,
                    });
                }
            }
            Err(e) => warnings.push(format!("lost-partition scan failed: {e}")),
        }
    }

    let mut records = Vec::new();
    let mut parsed = Vec::new();
    for (i, vol) in vols.into_iter().enumerate() {
        let index = i as u32 + 1;
        let kind = vol.probe.as_ref().map(|p| p.kind).or(vol.entry.fs);
        let mut rec = VolumeRecord {
            index,
            start: vol.entry.start,
            size: vol.entry.size,
            fs: kind,
            label: vol.probe.as_ref().and_then(|p| p.label.clone()),
            origin: vol.entry.origin.clone(),
            files_listed: 0,
            deleted_listed: 0,
            warnings: vol
                .probe
                .as_ref()
                .map(|p| p.notes.clone())
                .unwrap_or_default(),
            error: None,
        };
        match kind {
            Some(k) if k != FsKind::Other => {
                obs.event(Event::Stage(Stage::Listing {
                    volume: index,
                    fs: k,
                }));
                match open_filesystem(k, vol.source.clone()) {
                    Ok(fs) => {
                        let info = fs.info();
                        if info.label.is_some() {
                            rec.label = info.label.clone();
                        }
                        rec.fs = Some(info.kind);
                        rec.warnings.extend(info.warnings.iter().cloned());
                        let dir =
                            OutputTree::volume_dir(index, Some(info.kind), rec.label.as_deref());
                        parsed.push(ParsedVolume {
                            index,
                            start: vol.entry.start,
                            size: vol.entry.size,
                            dir,
                            fs,
                        });
                    }
                    Err(e) => rec.error = Some(e.to_string()),
                }
            }
            Some(_) => {
                rec.error =
                    Some("filesystem recognised but not supported; its space will be carved".into())
            }
            None => rec.error = Some("no filesystem recognised; its space will be carved".into()),
        }
        obs.event(Event::Volume(rec.clone()));
        records.push(rec);
    }
    Discovery {
        table,
        volumes: records,
        parsed,
        warnings,
    }
}

/// A filesystem entry chosen for extraction.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Index into `Discovery::parsed`.
    pub volume: usize,
    /// The entry.
    pub entry: FileEntry,
    /// Category (from extension or sniffing).
    pub category: MediaCategory,
    /// Format id when sniffed.
    pub format: Option<String>,
}

/// Walk every parsed volume and pick entries the selection wants.
/// Updates the per-volume listing counters in `records`.
pub fn select_entries(
    parsed: &[ParsedVolume],
    records: &mut [VolumeRecord],
    sel: &Selection,
    formats: &FormatRegistry,
    cancel: &Arc<AtomicBool>,
    obs: &dyn Observer,
) -> Vec<Candidate> {
    let mut picked = Vec::new();
    let mut to_sniff: Vec<(usize, FileEntry)> = Vec::new();
    for (vi, pv) in parsed.iter().enumerate() {
        let mut listed = 0u64;
        let mut deleted = 0u64;
        let res = pv.fs.walk(&mut |e: &FileEntry| {
            if cancel.load(Ordering::Relaxed) {
                return WalkControl::Stop;
            }
            if e.is_dir {
                return WalkControl::Continue;
            }
            listed += 1;
            if e.deleted {
                deleted += 1;
            }
            if !sel.wants_entry_state(e) || !sel.wants_size(e.size) {
                return WalkControl::Continue;
            }
            match e.extension().and_then(|x| category_for_extension(&x)) {
                Some(cat) if sel.wants_category(cat) => picked.push(Candidate {
                    volume: vi,
                    entry: e.clone(),
                    category: cat,
                    format: None,
                }),
                Some(_) => {}
                None if sel.wants_everything() => picked.push(Candidate {
                    volume: vi,
                    entry: e.clone(),
                    category: MediaCategory::Other,
                    format: None,
                }),
                None if sel.sniff_unknown && e.size >= 16 => to_sniff.push((vi, e.clone())),
                None => {}
            }
            WalkControl::Continue
        });
        if let Some(rec) = records.iter_mut().find(|r| r.index == pv.index) {
            rec.files_listed = listed;
            rec.deleted_listed = deleted;
            if let Err(e) = res {
                rec.warnings
                    .push(format!("directory walk stopped early: {e}"));
            }
        }
    }

    // Files without a known extension: look at their first bytes.
    let total = to_sniff.len() as u64;
    let done = AtomicU64::new(0);
    let sniffed: Vec<Candidate> = to_sniff
        .into_par_iter()
        .filter_map(|(vi, entry)| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut head = HeadSink::new(64 * 1024);
            let _ = parsed[vi].fs.read_file(&entry, &mut head);
            let head = head.into_inner();
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 256 == 0 {
                obs.event(Event::Progress {
                    done: n,
                    total,
                    bytes: 0,
                });
            }
            let fmt = formats
                .matching(&head)
                .into_iter()
                .find(|f| f.probe(&head))?;
            if !sel.wants_category(fmt.category()) {
                return None;
            }
            Some(Candidate {
                volume: vi,
                category: fmt.category(),
                format: Some(fmt.id().to_string()),
                entry,
            })
        })
        .collect();
    picked.extend(sniffed);
    picked
}

/// Shared state while the pipeline runs.
struct Ctx<'a> {
    opts: &'a RecoverOptions,
    tree: OutputTree,
    formats: FormatRegistry,
    dedup: Dedup,
    donors: DonorPool,
    obs: &'a dyn Observer,
}

/// Run the whole pipeline on `src`.
pub fn recover(
    src: SharedSource,
    source: SourceRecord,
    opts: &RecoverOptions,
    obs: &dyn Observer,
) -> Result<Report> {
    let workers = if opts.workers == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    } else {
        opts.workers
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("recover-{i}"))
        .build()
        .map_err(|e| Error::other(format!("thread pool: {e}")))?;

    let ctx = Ctx {
        opts,
        tree: OutputTree::create(&opts.output)?,
        formats: registry::all_formats(),
        dedup: Dedup::new(),
        donors: DonorPool::new(),
        obs,
    };
    let mut report = Report::new(opts.command_line.clone());
    report.source = source;
    if let Some(un) = src.unreadable() {
        report.source.unreadable_bytes = un.total_len();
    }
    if report.source.size == 0 {
        report.source.size = src.size();
    }
    if report.source.sector_size == 0 {
        report.source.sector_size = src.sector_size();
    }

    // Donors supplied by the user.
    for d in &opts.donors {
        match identify_file(&ctx.formats, d) {
            Some(fmt) => {
                let meta = FileSource::open(d)
                    .ok()
                    .and_then(|s| fmt.validate(&(Arc::new(s) as SharedSource)).ok())
                    .map(|v| v.meta)
                    .unwrap_or_default();
                ctx.donors.add(DonorCandidate {
                    path: d.clone(),
                    format: fmt.id().to_string(),
                    meta,
                    size: std::fs::metadata(d).map(|m| m.len()).unwrap_or(0),
                    user_supplied: true,
                });
            }
            None => report.warnings.push(format!(
                "donor {} is not a recognised media file; ignored",
                d.display()
            )),
        }
    }

    let disc = if opts.use_filesystems {
        discover(
            &src,
            opts.sector_size,
            opts.scan_lost_partitions,
            &opts.cancel,
            obs,
        )
    } else {
        Discovery {
            table: PartitionTable::empty(PartitionScheme::Unknown),
            volumes: Vec::new(),
            parsed: Vec::new(),
            warnings: Vec::new(),
        }
    };
    report.scheme = Some(disc.table.scheme);
    report.warnings.extend(disc.warnings.iter().cloned());
    let mut volume_records = disc.volumes;
    let parsed = disc.parsed;

    // Filesystem extraction.
    let candidates = select_entries(
        &parsed,
        &mut volume_records,
        &opts.selection,
        &ctx.formats,
        &opts.cancel,
        obs,
    );
    report.volumes = volume_records;
    obs.event(Event::Stage(Stage::Extracting));
    let total = candidates.len() as u64;
    let done = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let fs_records: Vec<FileRecord> = pool.install(|| {
        candidates
            .par_iter()
            .filter_map(|c| {
                if opts.cancelled() {
                    return None;
                }
                let rec = extract_fs_entry(&ctx, &parsed[c.volume], c);
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                let b = bytes.fetch_add(rec.size, Ordering::Relaxed) + rec.size;
                obs.event(Event::Progress {
                    done: n,
                    total,
                    bytes: b,
                });
                Some(rec)
            })
            .collect()
    });
    for rec in fs_records {
        let hash = rec.blake3.clone();
        let id = report.add_file(rec);
        if let Some(h) = hash {
            // Record content so the carver can skip copies of it. Two
            // filesystem files with identical content are both kept.
            let _ = ctx.dedup.check_insert(&h, id);
        }
    }

    // Carving.
    if opts.carve != CarvePolicy::Off && !opts.cancelled() {
        if let Err(e) = carve_stage(&ctx, &src, &parsed, &mut report) {
            report.warnings.push(format!("carving stopped: {e}"));
        }
    }

    // Validation, then repair.
    if opts.validate && !opts.cancelled() {
        validate_stage(&ctx, &pool, &mut report);
    }
    if opts.repair && opts.validate && !opts.cancelled() {
        repair_stage(&ctx, &pool, &mut report);
    }

    obs.event(Event::Stage(Stage::Report));
    report.interrupted = opts.cancelled();
    report.finalize();
    report.write_json(&ctx.tree.root().join("report.json"))?;
    report.write_text(&ctx.tree.root().join("report.txt"))?;
    Ok(report)
}

/// Extract one filesystem entry into the output tree.
fn extract_fs_entry(ctx: &Ctx<'_>, pv: &ParsedVolume, c: &Candidate) -> FileRecord {
    let e = &c.entry;
    let mut rec = FileRecord::new(
        Origin::Filesystem {
            volume: pv.index,
            fs: pv.fs.info().kind,
            path: e.path.clone(),
            deleted: e.deleted,
            confidence: e.confidence,
        },
        c.category,
        e.size,
    );
    rec.format = c.format.clone();
    let rel = OutputTree::fs_relative(&pv.dir, e);
    let (file, rel) = match ctx.tree.create_new(&rel) {
        Ok(x) => x,
        Err(err) => {
            rec.outcome = Outcome::Failed;
            rec.notes
                .push(format!("could not create output file: {err}"));
            return rec;
        }
    };
    let mut w = HashingWriter::new(BufWriter::with_capacity(1 << 20, file));
    let res = pv.fs.read_file(e, &mut w);
    let finished = w.finish().and_then(|(bw, hash, n)| {
        let f = bw.into_inner().map_err(|e| e.into_error())?;
        Ok((f, hash, n))
    });
    let (file, hash, written) = match finished {
        Ok(x) => x,
        Err(err) => {
            rec.outcome = Outcome::Failed;
            rec.notes.push(format!("writing the output failed: {err}"));
            rec.output = Some(rel);
            return rec;
        }
    };
    set_file_times(&file, e.modified, e.accessed);
    drop(file);
    rec.output = Some(rel.clone());
    rec.size = written;
    rec.blake3 = Some(hash);
    match res {
        Ok(rr) => {
            rec.unreadable = rr.unreadable.iter().collect();
            rec.unreadable_bytes = rr.unreadable.total_len();
            if rec.unreadable_bytes > 0 {
                rec.notes.push(format!(
                    "{} could not be read from the source and were filled with zeros",
                    diskrescue_core::util::human::bytes(rec.unreadable_bytes)
                ));
            }
            if !rr.reused.is_empty() {
                rec.notes.push(format!(
                    "{} of this deleted file's space is now used by another file; that part is probably overwritten",
                    diskrescue_core::util::human::bytes(rr.reused.total_len())
                ));
            }
            rec.notes.extend(rr.notes);
            rec.outcome = Outcome::RecoveredUnverified;
        }
        Err(err) => {
            rec.outcome = Outcome::Failed;
            rec.notes.push(format!("reading the file failed: {err}"));
            if written == 0 {
                let _ = std::fs::remove_file(ctx.tree.absolute(&rel));
                rec.output = None;
            }
        }
    }
    if e.confidence == EntryConfidence::DeletedGuessed {
        rec.notes
            .push("deleted file: its location on disk was reconstructed and may be incomplete if it was fragmented".into());
    }
    ctx.obs.event(Event::File {
        name: rel.display().to_string(),
        outcome: rec.outcome,
    });
    rec
}

/// Byte ranges of the source to carve, per the policy.
fn carve_regions(
    policy: CarvePolicy,
    size: u64,
    parsed: &[ParsedVolume],
    warnings: &mut Vec<String>,
) -> Option<RangeSet> {
    let whole = RangeSet::from_range(Range::new(0, size));
    match policy {
        CarvePolicy::Off => None,
        CarvePolicy::Everything => Some(whole),
        CarvePolicy::FreeSpace | CarvePolicy::Auto => {
            if parsed.is_empty() {
                return Some(whole);
            }
            let mut set = RangeSet::new();
            for pv in parsed {
                match pv.fs.unallocated() {
                    Ok(free) => {
                        let free = free.intersection_with_range(Range::new(0, pv.size));
                        set = set.union(&free.shifted_up(pv.start));
                    }
                    Err(e) => {
                        warnings.push(format!(
                            "volume {}: free space unknown ({e}); carving the whole volume",
                            pv.index
                        ));
                        set.insert(Range::with_len(pv.start, pv.size));
                    }
                }
            }
            if policy == CarvePolicy::Auto {
                let covered: RangeSet = parsed
                    .iter()
                    .map(|p| Range::with_len(p.start, p.size))
                    .collect();
                set = set.union(&covered.complement_within(Range::new(0, size)));
            }
            Some(set)
        }
    }
}

fn carve_stage(
    ctx: &Ctx<'_>,
    src: &SharedSource,
    parsed: &[ParsedVolume],
    report: &mut Report,
) -> Result<()> {
    let opts = ctx.opts;
    let mut warnings = Vec::new();
    let Some(regions) = carve_regions(opts.carve, src.size(), parsed, &mut warnings) else {
        return Ok(());
    };
    report.warnings.extend(warnings);
    if regions.is_empty() {
        return Ok(());
    }
    ctx.obs.event(Event::Stage(Stage::Carving));
    let whole = regions.covers(Range::new(0, src.size()));
    let cluster = match parsed {
        [only] => {
            let info = only.fs.info();
            Some((only.start + info.data_start, info.cluster_size as u64))
        }
        _ => None,
    };
    let carve_opts = diskrescue_carve::CarveOptions {
        registry: registry::carving_formats(&opts.selection),
        mode: if whole {
            diskrescue_carve::CarveMode::Whole
        } else {
            diskrescue_carve::CarveMode::Unallocated(regions.clone())
        },
        alignment: 512,
        cluster,
        chunk_size: 64 << 20,
        probe_window: 64 << 10,
        workers: opts.workers,
        min_confidence: diskrescue_core::format::Confidence::LOW,
        max_file_size: None,
        promote_nested: false,
        zero_skip: true,
        cancel: Some(opts.cancel.clone()),
    };
    let carver = diskrescue_carve::Carver::new(carve_opts)?;
    let total = regions.total_len();
    let mut new_records = Vec::new();
    let summary = carver.scan(
        src,
        &mut |p: &diskrescue_carve::ScanProgress| {
            ctx.obs.event(Event::Carve {
                scanned: p.scanned,
                total,
                files: p.files,
            })
        },
        &mut |cf: diskrescue_carve::CarvedFile| {
            if opts.cancelled() {
                return false;
            }
            if opts.selection.wants_category(cf.category) && opts.selection.wants_size(cf.len) {
                new_records.push(extract_carved(ctx, src, &cf));
            }
            true
        },
    )?;
    report.carve = Some(CarveRecord {
        scanned: summary.scanned,
        candidates: summary.candidates,
        files: summary.files,
        false_positives: summary.false_positives,
        unallocated_only: !whole,
    });
    for mut rec in new_records {
        let hash = rec.blake3.clone();
        if opts.dedup {
            if let Some(h) = &hash {
                if ctx.dedup.contains(h) {
                    if let Some(out) = rec.output.take() {
                        let _ = std::fs::remove_file(ctx.tree.absolute(&out));
                    }
                    rec.outcome = Outcome::Duplicate;
                    rec.duplicate_of = report
                        .files
                        .iter()
                        .find(|f| f.blake3.as_deref() == Some(h.as_str()))
                        .map(|f| f.id);
                }
            }
        }
        let id = report.add_file(rec);
        if let Some(h) = hash {
            let _ = ctx.dedup.check_insert(&h, id);
        }
    }
    Ok(())
}

fn extract_carved(
    ctx: &Ctx<'_>,
    src: &SharedSource,
    cf: &diskrescue_carve::CarvedFile,
) -> FileRecord {
    let mut rec = FileRecord::new(
        Origin::Carved {
            offset: cf.start,
            length: cf.len,
            status: cf.status,
            confidence: cf.confidence,
            parent: cf.parent,
        },
        cf.category,
        cf.len,
    );
    rec.format = Some(cf.format.clone());
    rec.meta = cf.meta.clone();
    rec.notes.extend(cf.notes.iter().cloned());
    // TIFF-family formats (RAW variants such as SRW, ERF, NRW) and RAF put
    // the precise extension in their metadata.
    let ext = cf
        .meta
        .extra
        .get("extension")
        .cloned()
        .unwrap_or_else(|| cf.extension.clone());
    let rel = OutputTree::carved_relative(cf.category, cf.meta.created, cf.start, &ext);
    let (file, rel) = match ctx.tree.create_new(&rel) {
        Ok(x) => x,
        Err(e) => {
            rec.outcome = Outcome::Failed;
            rec.notes.push(format!("could not create output file: {e}"));
            return rec;
        }
    };
    let mut w = HashingWriter::new(BufWriter::with_capacity(1 << 20, file));
    let res = diskrescue_carve::Carver::extract(src, cf, &mut w);
    let fin = w.finish();
    let failure = match (res, fin) {
        (Ok(er), Ok((bw, hash, n))) => {
            drop(bw);
            rec.output = Some(rel.clone());
            rec.blake3 = Some(hash);
            rec.size = n;
            rec.unreadable = er.unreadable.iter().collect();
            rec.unreadable_bytes = er.unreadable.total_len();
            if rec.unreadable_bytes > 0 {
                rec.notes.push(format!(
                    "{} could not be read from the source and were filled with zeros",
                    diskrescue_core::util::human::bytes(rec.unreadable_bytes)
                ));
            }
            rec.outcome = Outcome::RecoveredUnverified;
            None
        }
        (Err(e), _) => Some(e.to_string()),
        (Ok(_), Err(e)) => Some(e.to_string()),
    };
    if let Some(e) = failure {
        let _ = std::fs::remove_file(ctx.tree.absolute(&rel));
        rec.outcome = Outcome::Failed;
        rec.notes.push(format!("extraction failed: {e}"));
    }
    ctx.obs.event(Event::File {
        name: rec
            .output
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| rec.origin_text()),
        outcome: rec.outcome,
    });
    rec
}

/// Determine the format of a file on disk: by content first, then by
/// extension (a file whose header was destroyed has no signature left).
pub fn identify_file(formats: &FormatRegistry, path: &Path) -> Option<Arc<dyn FileFormat>> {
    let head = FileSource::open(path)
        .ok()
        .and_then(|s| s.read_vec(0, 64 * 1024).ok())
        .unwrap_or_default();
    if let Some(f) = formats.matching(&head).into_iter().find(|f| f.probe(&head)) {
        return Some(f);
    }
    let name = path.file_name()?.to_string_lossy();
    formats.by_extension(&extension_of(&name)?)
}

fn validation_record(v: &Validation) -> ValidationRecord {
    ValidationRecord {
        status: v.status,
        intact_fraction: v.intact_fraction,
        problems: v.problems.clone(),
    }
}

fn validate_stage(ctx: &Ctx<'_>, pool: &rayon::ThreadPool, report: &mut Report) {
    ctx.obs.event(Event::Stage(Stage::Validating));
    let total = report.files.len() as u64;
    let done = AtomicU64::new(0);
    let root = ctx.tree.root().to_path_buf();
    pool.install(|| {
        report.files.par_iter_mut().for_each(|rec| {
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 64 == 0 || n == total {
                ctx.obs.event(Event::Progress { done: n, total, bytes: 0 });
            }
            if ctx.opts.cancelled() || matches!(rec.outcome, Outcome::Duplicate | Outcome::Failed) {
                return;
            }
            let Some(out) = rec.output.clone() else { return };
            let path = root.join(&out);
            let fmt = rec
                .format
                .as_deref()
                .and_then(|id| ctx.formats.by_id(id))
                .or_else(|| identify_file(&ctx.formats, &path));
            let Some(fmt) = fmt else {
                rec.notes.push("format not recognised; content not verified".into());
                return;
            };
            rec.format = Some(fmt.id().to_string());
            if rec.category == MediaCategory::Other {
                rec.category = fmt.category();
            }
            let src: SharedSource = match FileSource::open(&path) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    rec.notes.push(format!("could not reopen for validation: {e}"));
                    return;
                }
            };
            match fmt.validate(&src) {
                Ok(v) => {
                    rec.meta.merge_missing(&v.meta);
                    rec.outcome = match v.status {
                        ValidationStatus::Valid if rec.unreadable_bytes == 0 => Outcome::Recovered,
                        ValidationStatus::Valid => Outcome::Damaged,
                        ValidationStatus::Damaged => Outcome::Damaged,
                        ValidationStatus::Invalid => {
                            let deleted = matches!(rec.origin, Origin::Filesystem { deleted: true, .. });
                            rec.notes.push(if deleted {
                                "content does not match its type: the deleted file was probably overwritten".into()
                            } else {
                                "content does not match its type".into()
                            });
                            Outcome::Damaged
                        }
                    };
                    if rec.outcome == Outcome::Recovered {
                        ctx.donors.add(DonorCandidate {
                            path: path.clone(),
                            format: fmt.id().to_string(),
                            meta: rec.meta.clone(),
                            size: rec.size,
                            user_supplied: false,
                        });
                    }
                    rec.validation = Some(validation_record(&v));
                }
                Err(e) => rec.notes.push(format!("validation error: {e}")),
            }
        })
    });
}

fn rank(r: RepairResult) -> u8 {
    match r {
        RepairResult::Repaired => 3,
        RepairResult::PartiallyRepaired => 2,
        RepairResult::NoRepairNeeded => 1,
        RepairResult::Unrepairable => 0,
    }
}

/// Try to repair the file at `input` into the output tree under `rel`,
/// first without a donor, then with up to three donors, keeping the best
/// result. Returns the record of the best attempt (None when nothing helped).
pub fn repair_one(
    tree: &OutputTree,
    fmt: &dyn FileFormat,
    input: &Path,
    rel: &Path,
    donors: &[PathBuf],
    aggressive: bool,
) -> Option<RepairRecord> {
    let src: SharedSource = Arc::new(FileSource::open(input).ok()?);
    let mut attempts: Vec<Option<PathBuf>> = vec![None];
    attempts.extend(donors.iter().take(3).cloned().map(Some));
    let mut best: Option<(u8, RepairOutcome, PathBuf, Option<PathBuf>)> = None;
    for donor in attempts {
        let ctx = RepairContext {
            donors: donor.iter().cloned().collect(),
            aggressive,
            scratch_dir: None,
        };
        let (file, out_rel) = tree.create_new(rel).ok()?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let result = fmt.repair(&src, &mut w, &ctx);
        let flushed = w.flush().is_ok();
        drop(w);
        let out_abs = tree.absolute(&out_rel);
        let outcome = match result {
            Ok(o) if flushed => o,
            _ => {
                let _ = std::fs::remove_file(&out_abs);
                continue;
            }
        };
        let score = rank(outcome.result);
        let better = best.as_ref().is_none_or(|b| score > b.0);
        if better && score >= 2 {
            if let Some((_, _, old, _)) = best.take() {
                let _ = std::fs::remove_file(tree.absolute(&old));
            }
            let done = score == 3;
            best = Some((score, outcome, out_rel, donor));
            if done {
                break;
            }
        } else {
            let _ = std::fs::remove_file(&out_abs);
        }
    }
    best.map(|(_, o, out_rel, donor)| RepairRecord {
        result: o.result,
        applied: o.applied,
        notes: o.notes,
        donor,
        output: Some(out_rel),
    })
}

fn repair_stage(ctx: &Ctx<'_>, pool: &rayon::ThreadPool, report: &mut Report) {
    ctx.obs.event(Event::Stage(Stage::Repairing));
    let todo: Vec<usize> = report
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.outcome == Outcome::Damaged && f.output.is_some())
        .filter(|(_, f)| {
            f.format
                .as_deref()
                .and_then(|id| ctx.formats.by_id(id))
                .is_some_and(|fmt| fmt.can_repair())
        })
        .map(|(i, _)| i)
        .collect();
    let total = todo.len() as u64;
    let done = AtomicU64::new(0);
    let results: Vec<(usize, Option<RepairRecord>)> = pool.install(|| {
        todo.par_iter()
            .map(|&i| {
                let rec = &report.files[i];
                let out = rec.output.clone().unwrap_or_default();
                let fmt = rec.format.as_deref().and_then(|id| ctx.formats.by_id(id));
                let r = fmt.and_then(|fmt| {
                    let donors = ctx.donors.pick(fmt.id(), &rec.meta, 3);
                    repair_one(
                        &ctx.tree,
                        fmt.as_ref(),
                        &ctx.tree.absolute(&out),
                        &OutputTree::repaired_relative(&out),
                        &donors,
                        ctx.opts.aggressive_repair,
                    )
                });
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                ctx.obs.event(Event::Progress {
                    done: n,
                    total,
                    bytes: 0,
                });
                (i, r)
            })
            .collect()
    });
    for (i, r) in results {
        let rec = &mut report.files[i];
        match r {
            Some(rr) => {
                rec.outcome = match rr.result {
                    RepairResult::Repaired => Outcome::Repaired,
                    RepairResult::PartiallyRepaired => Outcome::PartiallyRepaired,
                    _ => rec.outcome,
                };
                ctx.obs.event(Event::File {
                    name: rr
                        .output
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    outcome: rec.outcome,
                });
                rec.repair = Some(rr);
            }
            None => rec.notes.push("automatic repair did not succeed".into()),
        }
    }
}

/// Result of [`verify_files`] for one file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyResult {
    /// The file.
    pub path: PathBuf,
    /// Format detected.
    pub format: Option<String>,
    /// Validation, when the format is known.
    pub validation: Option<ValidationRecord>,
    /// Metadata.
    pub meta: diskrescue_core::media::MediaMeta,
    /// Error text when the file could not be checked.
    pub error: Option<String>,
}

/// Validate files given by the user.
pub fn verify_files(paths: &[PathBuf]) -> Vec<VerifyResult> {
    let formats = registry::all_formats();
    paths
        .par_iter()
        .map(|p| {
            let mut r = VerifyResult {
                path: p.clone(),
                format: None,
                validation: None,
                meta: Default::default(),
                error: None,
            };
            let Some(fmt) = identify_file(&formats, p) else {
                r.error = Some("not a recognised photo or video format".into());
                return r;
            };
            r.format = Some(fmt.id().to_string());
            match FileSource::open(p) {
                Ok(s) => match fmt.validate(&(Arc::new(s) as SharedSource)) {
                    Ok(v) => {
                        r.meta = v.meta.clone();
                        r.validation = Some(validation_record(&v));
                    }
                    Err(e) => r.error = Some(e.to_string()),
                },
                Err(e) => r.error = Some(e.to_string()),
            }
            r
        })
        .collect()
}

/// Repair files given by the user (the `repair` command). Each input is
/// validated; damaged ones are repaired into `output/repaired/`, trying the
/// user's donors (and, for each input, the other healthy inputs of the same
/// format) as references. Inputs are never modified.
pub fn repair_files(
    inputs: &[PathBuf],
    output: &Path,
    donors: &[PathBuf],
    aggressive: bool,
    obs: &dyn Observer,
) -> Result<Report> {
    let tree = OutputTree::create(output)?;
    let formats = registry::all_formats();
    let pool = DonorPool::new();
    for d in donors {
        if let Some(fmt) = identify_file(&formats, d) {
            pool.add(DonorCandidate {
                path: d.clone(),
                format: fmt.id().to_string(),
                meta: Default::default(),
                size: 0,
                user_supplied: true,
            });
        }
    }
    let mut report = Report::new(None);
    report.source.description = format!("{} file(s) given for repair", inputs.len());
    obs.event(Event::Stage(Stage::Validating));
    let checked = verify_files(inputs);
    for v in &checked {
        if let (Some(fmt), Some(val)) = (&v.format, &v.validation) {
            if val.status == ValidationStatus::Valid {
                pool.add(DonorCandidate {
                    path: v.path.clone(),
                    format: fmt.clone(),
                    meta: v.meta.clone(),
                    size: 0,
                    user_supplied: false,
                });
            }
        }
    }
    obs.event(Event::Stage(Stage::Repairing));
    let records: Vec<FileRecord> = checked
        .par_iter()
        .map(|v| {
            let size = std::fs::metadata(&v.path).map(|m| m.len()).unwrap_or(0);
            let fmt = v.format.as_deref().and_then(|id| formats.by_id(id));
            let category = fmt.as_ref().map(|f| f.category()).unwrap_or(MediaCategory::Other);
            let mut rec = FileRecord::new(Origin::Input { path: v.path.clone() }, category, size);
            rec.format = v.format.clone();
            rec.meta = v.meta.clone();
            rec.validation = v.validation.clone();
            if let Some(e) = &v.error {
                rec.notes.push(e.clone());
            }
            let status = v.validation.as_ref().map(|x| x.status);
            rec.outcome = match status {
                Some(ValidationStatus::Valid) => Outcome::Recovered,
                Some(_) => Outcome::Damaged,
                None => Outcome::Failed,
            };
            if rec.outcome == Outcome::Damaged {
                match fmt {
                    Some(f) if f.can_repair() => {
                        let donors: Vec<PathBuf> = pool
                            .pick(f.id(), &rec.meta, 4)
                            .into_iter()
                            .filter(|d| d != &v.path)
                            .take(3)
                            .collect();
                        let rel = OutputTree::repaired_relative(&v.path);
                        match repair_one(&tree, f.as_ref(), &v.path, &rel, &donors, aggressive) {
                            Some(rr) => {
                                rec.outcome = match rr.result {
                                    RepairResult::Repaired => Outcome::Repaired,
                                    RepairResult::PartiallyRepaired => Outcome::PartiallyRepaired,
                                    _ => Outcome::Damaged,
                                };
                                rec.output = rr.output.clone();
                                rec.repair = Some(rr);
                            }
                            None => {
                                let hint = if donors.is_empty() {
                                    " (try again with --donor <a healthy file from the same camera>)"
                                } else {
                                    ""
                                };
                                rec.notes.push(format!("could not be repaired automatically{hint}"));
                            }
                        }
                    }
                    Some(f) => rec.notes.push(format!("{} files cannot be repaired by this version", f.name())),
                    None => {}
                }
            }
            obs.event(Event::File {
                name: v.path.display().to_string(),
                outcome: rec.outcome,
            });
            rec
        })
        .collect();
    for r in records {
        report.add_file(r);
    }
    report.finalize();
    report.write_json(&tree.root().join("report.json"))?;
    report.write_text(&tree.root().join("report.txt"))?;
    Ok(report)
}

/// A lighter pass for the `scan` command: what would be recovered, without
/// writing anything.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Survey {
    /// Partitioning scheme.
    pub scheme: PartitionScheme,
    /// Volumes.
    pub volumes: Vec<VolumeRecord>,
    /// Files selectable through filesystems.
    pub files: Vec<SurveyFile>,
    /// Files found by carving (when requested).
    pub carved: Vec<SurveyCarved>,
    /// Warnings.
    pub warnings: Vec<String>,
}

/// One filesystem file in a [`Survey`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SurveyFile {
    /// Volume index.
    pub volume: u32,
    /// Path in the volume.
    pub path: String,
    /// Size.
    pub size: u64,
    /// Deleted.
    pub deleted: bool,
    /// Confidence.
    pub confidence: EntryConfidence,
    /// Category.
    pub category: MediaCategory,
    /// Modification time.
    pub modified: Option<diskrescue_core::time::Timestamp>,
}

/// One carved file in a [`Survey`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct SurveyCarved {
    /// Offset in the source.
    pub offset: u64,
    /// Length.
    pub len: u64,
    /// Format id.
    pub format: String,
    /// Category.
    pub category: MediaCategory,
    /// Measurement status.
    pub status: diskrescue_core::format::ExtentStatus,
    /// Capture time.
    pub created: Option<diskrescue_core::time::Timestamp>,
}

/// Survey `src` without writing anything.
pub fn survey(
    src: SharedSource,
    sel: &Selection,
    carve: CarvePolicy,
    cancel: Arc<AtomicBool>,
    obs: &dyn Observer,
) -> Result<Survey> {
    let formats = registry::all_formats();
    let disc = discover(&src, 0, true, &cancel, obs);
    let mut volumes = disc.volumes;
    let cands = select_entries(&disc.parsed, &mut volumes, sel, &formats, &cancel, obs);
    let files = cands
        .iter()
        .map(|c| SurveyFile {
            volume: disc.parsed[c.volume].index,
            path: c.entry.path.clone(),
            size: c.entry.size,
            deleted: c.entry.deleted,
            confidence: c.entry.confidence,
            category: c.category,
            modified: c.entry.modified,
        })
        .collect();
    let mut warnings = disc.warnings;
    let mut carved = Vec::new();
    if let Some(regions) = carve_regions(carve, src.size(), &disc.parsed, &mut warnings) {
        if !regions.is_empty() && !cancel.load(Ordering::Relaxed) {
            obs.event(Event::Stage(Stage::Carving));
            let whole = regions.covers(Range::new(0, src.size()));
            let total = regions.total_len();
            let carver = diskrescue_carve::Carver::new(diskrescue_carve::CarveOptions {
                registry: registry::carving_formats(sel),
                mode: if whole {
                    diskrescue_carve::CarveMode::Whole
                } else {
                    diskrescue_carve::CarveMode::Unallocated(regions)
                },
                alignment: 512,
                cluster: None,
                chunk_size: 64 << 20,
                probe_window: 64 << 10,
                workers: 0,
                min_confidence: diskrescue_core::format::Confidence::LOW,
                max_file_size: None,
                promote_nested: false,
                zero_skip: true,
                cancel: Some(cancel.clone()),
            })?;
            let found = Mutex::new(Vec::new());
            carver.scan(
                &src,
                &mut |p: &diskrescue_carve::ScanProgress| {
                    obs.event(Event::Carve {
                        scanned: p.scanned,
                        total,
                        files: p.files,
                    })
                },
                &mut |cf: diskrescue_carve::CarvedFile| {
                    if sel.wants_category(cf.category) && sel.wants_size(cf.len) {
                        found
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(SurveyCarved {
                                offset: cf.start,
                                len: cf.len,
                                format: cf.format.clone(),
                                category: cf.category,
                                status: cf.status,
                                created: cf.meta.created,
                            });
                    }
                    !cancel.load(Ordering::Relaxed)
                },
            )?;
            carved = found.into_inner().unwrap_or_else(|p| p.into_inner());
        }
    }
    Ok(Survey {
        scheme: disc.table.scheme,
        volumes,
        files,
        carved,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_order() {
        assert!(rank(RepairResult::Repaired) > rank(RepairResult::PartiallyRepaired));
        assert!(rank(RepairResult::PartiallyRepaired) > rank(RepairResult::NoRepairNeeded));
        assert!(rank(RepairResult::NoRepairNeeded) > rank(RepairResult::Unrepairable));
    }
}
