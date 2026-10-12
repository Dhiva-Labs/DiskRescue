//! ISO base media file format (ISO/IEC 14496-12) for DiskRescue: MP4, MOV,
//! M4V, 3GP/3G2 and M4A movies, the still-image users of the same container
//! (HEIF/HEIC/AVIF) and Canon CR3 raws.
//!
//! What it provides:
//!
//! * [`boxes`]: a damage-tolerant box walker, the parsed `moov` tree and the
//!   exact sample-table arithmetic.
//! * [`h264`], [`hevc`], [`aac`]: bitstream header parsers used to find
//!   sample boundaries in raw media data; the AAC module walks the full frame
//!   syntax so frames without a length field can be delimited exactly.
//! * [`rebuild`]: reconstruction of a missing `moov` from a healthy reference
//!   recording of the same device (the most valuable repair: phones and
//!   cameras that die while recording leave an `mdat` with no index).
//! * [`repair`]: the repair strategies (rebuild, table clipping, size
//!   fixing, offset relocation, fast-start, trailing-garbage removal).
//! * [`formats()`]: the [`FileFormat`] implementations for carving,
//!   validation and repair: [`mp4::Mp4Format`] (`mp4`),
//!   [`heif::HeifFormat`] (`heic`) and [`cr3::Cr3Format`] (`cr3`).
//!
//! The per-file extension (`mov` for QuickTime, `3gp`, `m4v`, `m4a`, `heic`,
//! `avif`, `cr3`) is reported in `MediaMeta::extra["extension"]`, because
//! [`FileFormat::extensions`] is static.
//!
//! Every parser is bounded by its input and never panics on hostile data;
//! media data is streamed, only indexes (`moov`, HEIF `meta`) are loaded
//! into memory, with size caps.

use std::sync::Arc;

use diskrescue_core::format::FileFormat;

pub mod aac;
pub mod analysis;
pub mod boxes;
pub mod cr3;
pub mod h264;
pub mod heif;
pub mod hevc;
pub mod mp4;
pub mod rebuild;
pub mod repair;
pub mod tiff;
pub mod writer;

pub use cr3::Cr3Format;
pub use diskrescue_core as core;
pub use heif::HeifFormat;
pub use mp4::Mp4Format;

/// The formats implemented by this crate: MP4/MOV/3GP (`mp4`), HEIF/HEIC/
/// AVIF (`heic`) and Canon CR3 (`cr3`).
pub fn formats() -> Vec<Arc<dyn FileFormat>> {
    vec![
        Arc::new(Mp4Format),
        Arc::new(HeifFormat),
        Arc::new(Cr3Format),
    ]
}
