//! diskrescue-imaging: the resumable, device-aware imaging engine.
//!
//! A failing device gets one careful pass that copies it into an image
//! file, recording in a GNU ddrescue compatible mapfile which bytes were
//! rescued, which failed and which were not tried yet. Everything else in
//! DiskRescue then works on the image, never on the device again.
//!
//! * [`status`]: the block status map ([`StatusMap`], [`BlockStatus`]).
//! * [`mapfile`]: ddrescue mapfile parsing and byte-compatible writing,
//!   atomic saves ([`MapFile`]).
//! * [`Imager`]: the engine (copy passes with exponential skipping, trim,
//!   divide, scrape, retry; timeouts, device loss and re-attach, slow-read
//!   handling, check-on-error), driven by a
//!   [`diskrescue_core::profile::ImagingProfile`].
//! * [`simulate`]: [`simulate::FaultySource`], a failing device in memory
//!   for tests.
//! * [`render_map`]: a one-line text picture of a map for the CLI/TUI.
//! * [`open_rescued_image`] / [`bad_ranges_of`]: hand a finished (or
//!   partial) rescue to the parsers with its holes attached.
//!
//! The source is only ever read through the read-only
//! [`diskrescue_core::Source`] trait; the engine has no way to write to it.

pub mod engine;
pub mod mapfile;
pub mod options;
pub mod render;
pub mod simulate;
pub mod status;

use std::path::Path;

use diskrescue_core::source::FileSource;
use diskrescue_core::{Range, RangeSet, Result, Source};

pub use diskrescue_core as core;
pub use engine::{Geometry, Imager};
pub use mapfile::{CurrentStatus, MapFile};
pub use options::{Control, Event, ImagingOptions, Phase, Progress, Summary};
pub use render::{render_map, render_window};
pub use status::{BlockStatus, Direction, MapBlock, ResizePolicy, Stats, StatusMap};

/// Open a rescued image for the parsers, with every byte the mapfile does
/// not mark finished attached as unreadable (bad sectors and areas never
/// read or not yet trimmed/scraped all read back as zeros from the image).
/// Image bytes beyond the extent of the mapfile are unreadable too.
/// When the sidecar `<mapfile>.dr.json` records a non-zero
/// `output_offset`, the map is applied at that offset in the image (the
/// bytes before it were not written by the rescue and are left as they
/// are); without a sidecar the offset is taken to be 0.
pub fn open_rescued_image(image: &Path, mapfile: &Path) -> Result<FileSource> {
    let mf = MapFile::load(mapfile)?;
    let src = FileSource::open(image).map_err(|e| mapfile::io_ctx(e, image))?;
    let offset = engine::sidecar_output_offset(mapfile).unwrap_or(0);
    let mut unreadable = mf.map.unfinished_ranges().shifted_up(offset);
    let img_len = src.size();
    let map_end = mf.map.size().saturating_add(offset);
    if img_len > map_end {
        unreadable.insert(Range::new(map_end, img_len));
    }
    Ok(src.with_unreadable(unreadable))
}

/// The bytes a rescue did not recover according to `mapfile`: bad sectors
/// plus everything not finished (non-tried, non-trimmed, non-scraped).
/// These read as zeros in the image. A missing mapfile is an error, never
/// "no holes".
pub fn bad_ranges_of(mapfile: &Path) -> Result<RangeSet> {
    Ok(MapFile::load(mapfile)?.map.unfinished_ranges())
}
