//! Options, progress reporting, events and run control for the imager.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::Range;

use crate::status::Stats;

/// Everything the engine needs besides the source and the two paths.
#[derive(Debug, Clone)]
pub struct ImagingOptions {
    /// Strategy parameters (block size, skips, timeouts, phase toggles,
    /// error handling). Every field is honoured; `direct_io` is advisory
    /// because the [`diskrescue_core::Source`] decides how it reads.
    pub profile: ImagingProfile,
    /// Rescue domain: only these source bytes are read, written and have
    /// their status changed. `None` = the whole source. The end is clamped
    /// to the source size. Reads may extend into the sector that contains
    /// an unaligned domain edge (ddrescue does the same); those extra bytes
    /// are discarded.
    pub domain: Option<Range>,
    /// Run the first copying pass backwards (ddrescue `--reverse`); every
    /// later pass direction flips accordingly.
    pub start_reverse: bool,
    /// Number of copying passes (ddrescue's 5 by default, capped at 5; 0
    /// skips the copying phase). Pass 5 is the no-skip sweep.
    pub max_copy_passes: u32,
    /// Save the mapfile at least this often (0 = after every read). Saves
    /// also happen at every phase change, on cancel and at the end.
    pub save_interval: Duration,
    /// Do not write all-zero data where the output already reads as zero
    /// (keeps a new image sparse). Safe on existing outputs: the output is
    /// checked before a write is skipped.
    pub sparse_output: bool,
    /// fsync the output at least this often (it is also fsynced before
    /// every mapfile save, so the map never claims data that is not durable).
    pub fsync_interval: Duration,
    /// Program name written into the mapfile header.
    pub tool_name: String,
    /// Program version written into the mapfile header.
    pub tool_version: String,
    /// Written as `# Command line:` into the mapfile.
    pub command_line: Option<String>,
    /// Output position = source position + `output_offset` (ddrescue `-o`).
    pub output_offset: u64,
    /// Reserve the output's disk space up front (`posix_fallocate` on
    /// Linux/Unix; ignored with a warning elsewhere) so the rescue cannot
    /// run out of space half way.
    pub preallocate: bool,
    /// Accept a mapfile whose size differs from the source: the map is
    /// grown with non-tried bytes, or shrunk when no rescued data lies
    /// beyond the new end. Without it a size mismatch is an error (wrong
    /// mapfile, or a device reporting a bogus capacity). A mapfile whose
    /// last block ends at 2^63-1 (ddrescue's "size unknown") is always
    /// adapted.
    pub allow_size_mismatch: bool,
    /// Allow writing into an existing non-empty output when no mapfile
    /// exists yet. Off by default so an unrelated file is never
    /// overwritten by mistake.
    pub allow_existing_output: bool,
    /// How often to call `Source::reopen` while waiting for a vanished
    /// device to come back.
    pub reattach_poll: Duration,
    /// Minimum time between two [`Event::Progress`] events (0 = after every
    /// read). Phase changes and the end always report progress.
    pub progress_interval: Duration,
}

impl Default for ImagingOptions {
    fn default() -> Self {
        ImagingOptions {
            profile: ImagingProfile::default(),
            domain: None,
            start_reverse: false,
            max_copy_passes: 5,
            save_interval: Duration::from_secs(30),
            sparse_output: false,
            fsync_interval: Duration::from_secs(30),
            tool_name: "DiskRescue".to_string(),
            tool_version: diskrescue_core::VERSION.to_string(),
            command_line: None,
            output_offset: 0,
            preallocate: false,
            allow_size_mismatch: false,
            allow_existing_output: false,
            reattach_poll: Duration::from_secs(2),
            progress_interval: Duration::from_millis(100),
        }
    }
}

impl ImagingOptions {
    /// Default options with the given profile.
    pub fn with_profile(profile: ImagingProfile) -> Self {
        ImagingOptions {
            profile,
            ..ImagingOptions::default()
        }
    }
}

/// The phase the engine is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Phase {
    /// Copying non-tried areas in large reads (passes 1..=5).
    Copying {
        /// Pass number (1-based).
        pass: u32,
        /// True when the pass runs towards lower offsets.
        reverse: bool,
    },
    /// Probing the edges of failed areas sector by sector.
    Trimming,
    /// Re-reading failed areas in `block_size / 8` pieces (flash media).
    Dividing,
    /// Reading every remaining sector of failed areas individually.
    Scraping,
    /// Re-reading bad sectors.
    Retrying {
        /// Pass number (1-based).
        pass: u32,
        /// True when the pass runs towards lower offsets.
        reverse: bool,
    },
    /// Done.
    Finished,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let dir = |r: bool| if r { "backwards" } else { "forwards" };
        match self {
            Phase::Copying { pass, reverse } => {
                write!(
                    f,
                    "Copying non-tried blocks... Pass {pass} ({})",
                    dir(*reverse)
                )
            }
            Phase::Trimming => f.write_str("Trimming failed blocks..."),
            Phase::Dividing => f.write_str("Dividing failed blocks..."),
            Phase::Scraping => f.write_str("Scraping failed blocks..."),
            Phase::Retrying { pass, reverse } => {
                write!(f, "Retrying bad sectors... Pass {pass} ({})", dir(*reverse))
            }
            Phase::Finished => f.write_str("Finished"),
        }
    }
}

/// A progress snapshot for the CLI/TUI.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Progress {
    /// Current phase.
    pub phase: Phase,
    /// Totals inside the rescue domain.
    pub stats: Stats,
    /// Position being worked on.
    pub current_pos: u64,
    /// Rescued bytes per second over the last second.
    pub current_rate: f64,
    /// Rescued bytes per second since the run started.
    pub average_rate: f64,
    /// Time since the run started.
    pub elapsed: Duration,
    /// Estimated time to finish the current phase: bytes the phase still
    /// has to process divided by the rate at which the phase has been
    /// processing bytes so far (successful or not). `None` until there is
    /// at least a second of data.
    pub eta: Option<Duration>,
    /// Failed reads (including timeouts) in this run.
    pub errors: u64,
    /// Reads slower than the minimum rate in this run.
    pub slow_reads: u64,
    /// Reads that timed out in this run.
    pub timeouts: u64,
    /// Times the device vanished and came back in this run.
    pub reattaches: u64,
    /// Most recent read error, for display.
    pub last_error: Option<String>,
    /// Size of the rescue domain in bytes.
    pub size: u64,
}

/// What the engine reports while it runs.
#[derive(Debug, Clone)]
pub enum Event {
    /// Periodic progress.
    Progress(Progress),
    /// A new phase (or pass) started.
    Phase(Phase),
    /// A message worth showing (warnings about damage, decisions taken).
    Log(log::Level, String),
    /// The device disappeared while reading `at`; the engine is waiting
    /// for it to come back.
    DeviceLost {
        /// Position whose read reported the loss.
        at: u64,
    },
    /// The device is back after `after`.
    DeviceBack {
        /// How long the device was gone.
        after: Duration,
    },
    /// The engine sleeps for this long (pause on error, cooling pause).
    Paused(Duration),
    /// The mapfile was saved.
    Saved,
    /// The run ended (also returned by [`crate::Imager::run`]).
    Finished(Summary),
}

/// Shared run control: cancel or pause a running imager from another
/// thread (signal handler, UI).
#[derive(Debug, Default)]
pub struct Control {
    cancel: AtomicBool,
    pause: AtomicBool,
    save: AtomicBool,
}

impl Control {
    /// A new control block, not cancelled, not paused.
    pub fn new() -> Arc<Self> {
        Arc::new(Control::default())
    }

    /// Ask the engine to stop after the current read; it saves the mapfile
    /// and returns a [`Summary`] with `cancelled = true`.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Pause (`true`) or resume (`false`) between reads.
    pub fn pause(&self, paused: bool) {
        self.pause.store(paused, Ordering::SeqCst);
    }

    /// True once [`Control::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// True while paused.
    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    /// Ask for a mapfile save at the next opportunity (e.g. on SIGUSR1).
    pub fn request_save(&self) {
        self.save.store(true, Ordering::SeqCst);
    }

    pub(crate) fn take_save_request(&self) -> bool {
        self.save.swap(false, Ordering::SeqCst)
    }
}

/// Result of a run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Summary {
    /// Totals inside the rescue domain at the end.
    pub stats: Stats,
    /// Duration of the run.
    pub elapsed: Duration,
    /// Failed reads (including timeouts) in this run.
    pub errors: u64,
    /// True when every phase ran to completion and every byte of the
    /// domain is finished or a bad sector.
    pub finished: bool,
    /// True when the run stopped because of [`Control::cancel`].
    pub cancelled: bool,
    /// Times the device vanished and came back.
    pub reattaches: u64,
    /// Reads that timed out.
    pub timeouts: u64,
    /// Reads slower than the minimum rate.
    pub slow_reads: u64,
    /// The output image.
    pub output: PathBuf,
    /// The mapfile.
    pub mapfile: PathBuf,
    /// Warnings for the report: limits that stopped the run, stale data
    /// detected, a device that did not come back, and so on.
    pub warnings: Vec<String>,
}
