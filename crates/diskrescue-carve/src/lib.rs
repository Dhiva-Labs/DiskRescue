//! diskrescue-carve: the signature scanner and extent resolver.
//!
//! The carver finds photos and videos by content alone, when the filesystem
//! that described them is damaged or gone. It knows no file format itself:
//! every format comes from a [`FormatRegistry`](diskrescue_core::format::FormatRegistry)
//! of [`FileFormat`](diskrescue_core::format::FileFormat) implementations,
//! which say what their headers look like (`signatures`, `probe`) and how
//! long a file starting at a given offset is (`measure`).
//!
//! # Algorithm
//!
//! 1. **Scan** (the calling thread plus a reader thread). The scan area (the
//!    whole source, or the unallocated ranges) is read in chunks of
//!    `chunk_size` bytes plus an overlap of `max(probe_window, longest
//!    signature)` bytes, so a header that straddles a chunk edge is seen
//!    whole and every candidate has its full probe window without another
//!    read. One Aho-Corasick automaton (leftmost-first DFA with its
//!    prefilter) holds the primary bytes of every signature; the search
//!    resumes one byte after each match and compares all patterns at a
//!    matching position, so overlapping signatures and signatures at an
//!    offset inside the file (`ftyp` at 4) are all seen. One- and two-byte
//!    patterns (MPEG-TS sync, MP3 frame sync, `BM`) are compared directly at
//!    each grid position instead, which keeps the automaton fast. All-zero
//!    4 KiB blocks are skipped (with a margin so patterns straddling them are
//!    not lost). Bytes the source reports as unreadable are zero-filled and
//!    never searched; read errors are retried at sector granularity and the
//!    failing sectors are skipped, so a bad sector never aborts the scan.
//! 2. **Filter.** A candidate start must be a multiple of `alignment` (and of
//!    512 for weak signatures with fewer than three fixed bytes, whatever the
//!    alignment: spec 3.1), lie in the scan area, match its full signature
//!    (including the `also` pattern) and pass `FileFormat::probe` on the
//!    probe window.
//! 3. **Measure** (rayon pool of `workers` threads). One job per candidate
//!    start measures the matching formats in priority order with a read
//!    limit of `min(format max_size, max_file_size, bytes to source end)`;
//!    the first result at or above `min_confidence` wins. Measurement reads
//!    through a hole-filling view of the source, so holes read as zeros, and
//!    a panicking format only loses its candidate.
//! 4. **Resolve** (calling thread, strictly in start order through a reorder
//!    buffer). Overlaps follow the carving spec, section 3.3: embedded
//!    objects become `nested`, a new aligned header ends a weak-length file
//!    (truncated, capped or unknown length), a much stronger aligned header
//!    splits a file, and a partial overlap goes to the more confident file.
//!    Each byte is attributed to at most one top-level file.
//! 5. **Emit.** Confidence is lowered for unreadable bytes inside the file
//!    ([`PENALTY_UNREADABLE`]), for extending into allocated space
//!    ([`PENALTY_ALLOCATED`]) and for a header off the cluster grid
//!    ([`PENALTY_OFF_CLUSTER`]). Valid still images nested in a truncated or
//!    capped parent (or every nested object with `promote_nested`) are
//!    emitted as files of their own right after their parent, with `parent`
//!    set.
//!
//! Candidates that start inside a file already known to be complete and
//! highly confident are dropped before measuring, and the scanner skips the
//! rest of such a file (no search, and no read once the planner knows);
//! the resolver rules guarantee this shortcut gives the same output as
//! measuring them, so results never depend on thread timing.
//!
//! # Limits
//!
//! * Memory: two chunk buffers (`2 x (chunk_size + overlap)`), at most
//!   `4 x workers` candidates in flight and their results, plus whatever
//!   the formats' `measure` allocate. Nothing grows with the source size or
//!   with the number of files found.
//! * Files are assumed contiguous; a fragmented file comes back truncated at
//!   the first foreign cluster (or wrong after it). Bifragment gap carving
//!   (spec 3.9) and block-size inference (spec 3.1) are not implemented.
//! * Unallocated mode considers only headers inside the given ranges but
//!   lets `measure` read across them; crossing into allocated space costs
//!   confidence. PhotoRec-style "skip" mode (a file reassembled from free
//!   runs only) is not implemented.
//! * Unreadable sectors not known in advance are listed on a file only if
//!   they were discovered (by the scanner or a measurement) before the file
//!   was emitted.
//! * At most 4096 nested objects per file are measured for promotion, and
//!   motion-photo trailers are never promoted.

#![warn(missing_docs)]

mod patterns;
mod plan;
mod resolve;
mod scan;
mod types;
mod zerofill;

pub use diskrescue_core as core;
pub use scan::Carver;
pub use types::{
    CarveMode, CarveOptions, CarvedFile, ExtractReport, ScanProgress, ScanSummary,
    DEFAULT_ALIGNMENT, DEFAULT_CHUNK_SIZE, DEFAULT_PROBE_WINDOW, MIN_CHUNK_SIZE, PENALTY_ALLOCATED,
    PENALTY_OFF_CLUSTER, PENALTY_SPLIT, PENALTY_UNREADABLE, ZERO_BLOCK,
};
