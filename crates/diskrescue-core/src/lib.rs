//! Shared vocabulary for every DiskRescue crate.
//!
//! The crate deliberately contains no I/O strategy, no format parsers and no
//! user interface. It defines:
//!
//! * [`source::Source`]: positional, read-only access to bytes, whether they
//!   come from a block device, an image file, a partition view or a test
//!   buffer. Everything that reads data goes through it.
//! * [`ranges::RangeSet`]: sorted, non-overlapping byte ranges used for bad
//!   block maps, allocation bitmaps and carving bookkeeping.
//! * [`device`] and [`profile`]: what we know about a physical device and the
//!   imaging strategy recommended for its kind.
//! * [`partition`] and [`fs`]: partition tables and the [`fs::FileSystem`]
//!   trait that every filesystem parser implements.
//! * [`format`] and [`media`]: the [`format::FileFormat`] trait that every
//!   photo/video/audio format implements for carving, validation and repair.
//! * [`time`] and [`util`]: timestamp conversions, byte/bit readers, hashing
//!   and human-readable formatting.
//!
//! Invariant shared by all crates: **nothing ever writes to a [`Source`]**.

pub mod device;
pub mod error;
pub mod format;
pub mod fs;
pub mod media;
pub mod partition;
pub mod profile;
pub mod ranges;
pub mod source;
pub mod time;
pub mod util;

pub use error::{Error, Result};
pub use ranges::{Range, RangeSet};
pub use source::{ReadError, ReadErrorKind, SharedSource, Source, SourceExt};

/// Crate version, re-exported for reports and the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
