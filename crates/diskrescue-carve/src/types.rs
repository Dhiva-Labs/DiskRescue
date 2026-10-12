//! Public option, result and progress types of the carver.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::format::{Confidence, ExtentStatus, FormatRegistry, Nested};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Range, RangeSet};

/// Default read size of the scanner (64 MiB).
pub const DEFAULT_CHUNK_SIZE: usize = 64 << 20;

/// Default number of bytes handed to [`FileFormat::probe`](diskrescue_core::format::FileFormat::probe) (64 KiB).
pub const DEFAULT_PROBE_WINDOW: usize = 64 << 10;

/// Default alignment of candidate file starts (one 512-byte sector).
pub const DEFAULT_ALIGNMENT: u64 = 512;

/// Smallest accepted `chunk_size`.
pub const MIN_CHUNK_SIZE: usize = 4096;

/// Size of the blocks the zero-skip optimisation tests for "all zero".
pub const ZERO_BLOCK: usize = 4096;

/// Confidence points removed from a file that contains unreadable bytes.
pub const PENALTY_UNREADABLE: u8 = 10;

/// Confidence points removed from a file found in unallocated mode whose
/// extent runs into allocated space.
pub const PENALTY_ALLOCATED: u8 = 20;

/// Confidence points removed from a file whose header is not on the cluster
/// grid when [`CarveOptions::cluster`] is known.
pub const PENALTY_OFF_CLUSTER: u8 = 20;

/// Confidence points removed from a complete file that the resolver had to
/// cut short because a much stronger file starts inside it.
pub const PENALTY_SPLIT: u8 = 30;

/// Which part of the source the carver scans for file headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarveMode {
    /// Every byte of the source.
    Whole,
    /// Only headers that start inside these source-relative (volume-relative)
    /// ranges are considered. Measurement still reads through the source
    /// freely; a file that runs past the end of its range into allocated
    /// space is kept, noted and its confidence lowered by
    /// [`PENALTY_ALLOCATED`].
    Unallocated(RangeSet),
}

/// Everything that controls a carving run.
///
/// Build it with [`CarveOptions::new`] (defaults for everything but the
/// registry) and adjust fields, or write the struct literally.
#[derive(Clone)]
pub struct CarveOptions {
    /// Formats to look for. Must not be empty.
    pub registry: FormatRegistry,
    /// Whole source or unallocated ranges only.
    pub mode: CarveMode,
    /// Candidate file starts must be multiples of this (absolute source
    /// offsets). Default 512; 1 disables the filter ("deep" scan); 0 is
    /// treated as 1. Weak signatures (fewer than three fixed bytes: MPEG-TS,
    /// M2TS, bare MP3 frames, BMP) are only accepted at multiples of 512 as
    /// well, even in a deep scan (carving spec 3.1).
    pub alignment: u64,
    /// `(data_start, cluster_size)` of the filesystem, when known. Headers at
    /// `data_start + k * cluster_size` are preferred; others lose
    /// [`PENALTY_OFF_CLUSTER`] confidence points and are not `aligned`.
    pub cluster: Option<(u64, u64)>,
    /// Bytes read per scanner step. Default 64 MiB; at least
    /// [`MIN_CHUNK_SIZE`].
    pub chunk_size: usize,
    /// Bytes handed to `FileFormat::probe` (default 64 KiB). Raised
    /// automatically to the longest signature probe length.
    pub probe_window: usize,
    /// Threads measuring candidates in parallel. 0 means
    /// `std::thread::available_parallelism()`.
    pub workers: usize,
    /// Results whose final confidence is below this are dropped
    /// (default [`Confidence::LOW`]).
    pub min_confidence: Confidence,
    /// Global size cap; overrides a format's `max_size` when smaller.
    pub max_file_size: Option<u64>,
    /// Emit every nested object (thumbnails, previews, ...) as its own file,
    /// whatever the state of the parent. Without it, only valid still images
    /// nested in truncated or capped parents are promoted.
    pub promote_nested: bool,
    /// Skip 4 KiB blocks that are entirely zero without searching them.
    pub zero_skip: bool,
    /// Set to `true` to stop the scan early; [`Carver::scan`](crate::Carver::scan)
    /// then returns the partial summary with `cancelled` set.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl CarveOptions {
    /// Defaults for everything except the registry.
    pub fn new(registry: FormatRegistry) -> Self {
        CarveOptions {
            registry,
            mode: CarveMode::Whole,
            alignment: DEFAULT_ALIGNMENT,
            cluster: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            probe_window: DEFAULT_PROBE_WINDOW,
            workers: 0,
            min_confidence: Confidence::LOW,
            max_file_size: None,
            promote_nested: false,
            zero_skip: true,
            cancel: None,
        }
    }
}

impl Default for CarveOptions {
    fn default() -> Self {
        CarveOptions::new(FormatRegistry::new())
    }
}

impl fmt::Debug for CarveOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<&str> = self.registry.all().iter().map(|x| x.id()).collect();
        f.debug_struct("CarveOptions")
            .field("registry", &ids)
            .field("mode", &self.mode)
            .field("alignment", &self.alignment)
            .field("cluster", &self.cluster)
            .field("chunk_size", &self.chunk_size)
            .field("probe_window", &self.probe_window)
            .field("workers", &self.workers)
            .field("min_confidence", &self.min_confidence)
            .field("max_file_size", &self.max_file_size)
            .field("promote_nested", &self.promote_nested)
            .field("zero_skip", &self.zero_skip)
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}

/// One file found by the carver.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CarvedFile {
    /// 1-based id, unique within one scan, in output order.
    pub id: u64,
    /// Format id (`jpeg`, `mp4`, ...).
    pub format: String,
    /// Preferred extension of the format, without a dot.
    pub extension: String,
    /// Output category.
    pub category: MediaCategory,
    /// Absolute offset of the first byte in the scanned source.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    /// How the extent ended (after overlap resolution).
    pub status: ExtentStatus,
    /// Final confidence, after the carver's own adjustments.
    pub confidence: Confidence,
    /// File-relative offset of the first byte the parser rejected, or where
    /// the resolver cut the file.
    pub err_offset: Option<u64>,
    /// Metadata collected by `measure`.
    pub meta: MediaMeta,
    /// Embedded objects inside this file (file-relative ranges).
    pub nested: Vec<Nested>,
    /// Observations from the format and the carver.
    pub notes: Vec<String>,
    /// File-relative byte ranges that could not be read from the source
    /// (zero-filled by [`Carver::extract`](crate::Carver::extract)).
    pub unreadable: RangeSet,
    /// For a promoted nested object: id of the file it was found in. Its
    /// bytes are also part of that parent.
    pub parent: Option<u64>,
    /// True when the start lies on the allocation grid: the cluster grid when
    /// [`CarveOptions::cluster`] is set, otherwise the `alignment` grid (the
    /// sector grid when alignment is 1).
    pub aligned: bool,
}

impl CarvedFile {
    /// One past the last byte (absolute).
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// Absolute byte range of the file.
    pub fn range(&self) -> Range {
        Range::with_len(self.start, self.len)
    }
}

/// Progress report passed to the `on_progress` callback after every chunk.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScanProgress {
    /// Bytes of the scan area covered so far.
    pub scanned: u64,
    /// Total bytes of the scan area.
    pub total: u64,
    /// Candidate headers seen so far.
    pub candidates: u64,
    /// Files emitted so far.
    pub files: u64,
    /// Average scan rate in bytes per second.
    pub rate: f64,
    /// Format of the file currently open in the resolver (the one the scan
    /// position is inside), if any.
    pub current_format: Option<String>,
}

/// Totals of a finished (or cancelled) scan.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScanSummary {
    /// Bytes of the scan area covered.
    pub scanned: u64,
    /// Distinct candidate starts that passed the signature and alignment
    /// filters in the searched regions.
    pub candidates: u64,
    /// Files emitted, including promoted nested objects.
    pub files: u64,
    /// Candidates rejected by `probe`, by `measure` or for low confidence.
    pub false_positives: u64,
    /// Bytes attributed to top-level files (promoted nested objects are not
    /// counted again).
    pub bytes_attributed: u64,
    /// Files emitted per format id.
    pub by_format: BTreeMap<String, u64>,
    /// Wall-clock time of the scan.
    pub elapsed: Duration,
    /// Unreadable bytes inside the scan area (known from the source plus
    /// discovered while reading); they were zero-filled and not searched.
    pub unreadable_skipped: u64,
    /// True when the scan stopped early: the cancel flag was raised or
    /// `on_file` returned `false`.
    pub cancelled: bool,
}

/// Outcome of [`Carver::extract`](crate::Carver::extract).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractReport {
    /// Bytes written to the sink.
    pub bytes: u64,
    /// File-relative ranges that could not be read and were written as zeros.
    pub unreadable: RangeSet,
}
