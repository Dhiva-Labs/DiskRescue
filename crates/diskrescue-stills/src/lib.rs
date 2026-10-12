//! Still-image formats: GIF, BMP, WebP, TIFF, the TIFF-based camera RAW
//! formats, Fujifilm RAF and Photoshop PSD.
//!
//! Every format implements [`diskrescue_core::format::FileFormat`]: it
//! measures the exact extent of a candidate file from a signature hit,
//! validates complete files and reports metadata (dimensions, camera make
//! and model, capture time, orientation) plus the embedded JPEG previews that
//! the carver must not claim as separate files. None of them repairs files;
//! [`FileFormat::can_repair`] is `false` for all.
//!
//! | Format | Object | Extent rule |
//! |---|---|---|
//! | GIF | [`GifFormat`] | block walk to the `3B` trailer |
//! | BMP | [`BmpFormat`] | file-size field checked against the pixel array |
//! | WebP | [`WebpFormat`] | RIFF size, confirmed by a chunk walk |
//! | TIFF, SRW, ERF, ... | [`TiffFormat`] | `max(offset + length)` over the IFD graph |
//! | CR2, NEF, ARW, DNG, PEF, ORF, RW2 | [`Cr2Format`] ... [`Rw2Format`] | same, plus maker-note previews |
//! | RAF | [`RafFormat`] | largest of the three offset/length pairs in the header |
//! | PSD / PSB | [`PsdFormat`] | section lengths plus the computed image data |
//!
//! [`formats`] returns all of them with carving priorities set so that the
//! specific format wins over generic TIFF at the same start offset: RAW
//! variants 60 (DNG 62), TIFF 50, PSD 55, BMP 30, the rest 50. The TIFF
//! family shares one walker, exposed as [`tiff::walk`].
//!
//! # Guarantees
//!
//! * No parser panics on hostile input: every read is bounds-checked, loops
//!   are bounded, there is no recursion and nothing is allocated from an
//!   untrusted size without a cap.
//! * Measuring never reads outside `start..start + limit` and never reads a
//!   whole file into memory; reads go through a block cache.
//! * Truncated input yields `ExtentStatus::Truncated` with `err_offset` at
//!   the first missing or unreadable byte; a limit shorter than the file
//!   yields `Capped`.
//! * Results are file-relative: `len`, `err_offset` and nested ranges count
//!   from the signature start.
//!
//! The suggested file extension of a TIFF-family hit (`srw`, `erf`, `sr2`,
//! `nrw`, `rwl`, `3fr`, `mef`, `mos`, `iiq`, ...) is in
//! `meta.extra["extension"]`, and the classification in `meta.extra["variant"]`.

#![warn(missing_docs)]

use std::sync::Arc;

use diskrescue_core::format::FileFormat;

mod bmp;
mod common;
mod gif;
mod jpegmini;
mod makernote;
mod psd;
mod raf;
mod raw;
pub mod tiff;
mod webp;
mod win;

pub use bmp::BmpFormat;
pub use gif::GifFormat;
pub use psd::PsdFormat;
pub use raf::RafFormat;
pub use raw::{
    ArwFormat, Cr2Format, DngFormat, NefFormat, OrfFormat, PefFormat, Rw2Format, TiffFormat,
};
pub use webp::WebpFormat;

/// All still-image formats of this crate, ready to register.
///
/// Priorities make the more specific format win at the same start offset:
/// DNG (62) beats the other RAW variants (60), which beat generic TIFF (50).
pub fn formats() -> Vec<Arc<dyn FileFormat>> {
    vec![
        Arc::new(GifFormat),
        Arc::new(BmpFormat),
        Arc::new(WebpFormat),
        Arc::new(TiffFormat),
        Arc::new(Cr2Format),
        Arc::new(NefFormat),
        Arc::new(ArwFormat),
        Arc::new(DngFormat),
        Arc::new(PefFormat),
        Arc::new(OrfFormat),
        Arc::new(Rw2Format),
        Arc::new(RafFormat),
        Arc::new(PsdFormat),
    ]
}

pub use diskrescue_core as core;
