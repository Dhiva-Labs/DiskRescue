//! PNG support for DiskRescue: carving extent, validation with a lenient
//! decoder, and repair.
//!
//! * [`PngFormat`] implements [`diskrescue_core::format::FileFormat`] so the
//!   carver can measure candidates (walk chunks to IEND), the pipeline can
//!   validate recovered files and repair damaged ones.
//! * [`chunks`] is the streaming chunk walker with CRC verification and
//!   resynchronisation after damage.
//! * [`lenient`] salvages every trustworthy scanline from damaged image data.
//! * [`repair`] plans and applies repairs (CRC fixes, IEND reconstruction,
//!   re-encoding of truncated images, donor headers, garbage removal).
//! * [`Ihdr`] describes the image geometry (strides, Adam7 passes).
//!
//! Nothing here reads a whole file into memory except the in-memory
//! convenience API [`lenient::decode`]; walking, validation and repair stream
//! through 64 KiB windows. Interlaced images are the exception when they are
//! re-encoded (the Adam7 passes have to be assembled in a buffer, capped at
//! 64 Mpx / 128 MiB).
//!
//! # Trust model
//!
//! PNG has no per-row checksum, so the lenient decoder only reports rows it
//! can vouch for: complete rows before a truncation, or before the first
//! corruption it can detect (invalid filter byte, inflate error, an IDAT chunk
//! whose CRC fails while the zlib stream decodes on regardless). See
//! [`lenient`] for the details and the limits.

#![deny(missing_docs)]

pub mod chunks;
mod encode;
mod filter;
mod format;
mod ihdr;
mod io;
pub mod lenient;
mod meta;
pub mod repair;
mod sanity;

pub use format::{PngFormat, MAX_PROBE_DIM, MAX_SIZE, MIN_SIZE};
pub use ihdr::{Ihdr, Pass, ADAM7};
