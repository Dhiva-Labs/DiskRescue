//! Synthetic media files: JPEG, PNG, GIF, BMP, WebP, TIFF/CR2-like, HEIC and
//! video containers.
//!
//! Still images are generated in pure Rust from a deterministic canvas
//! ([`canvas`]): same options and seed give byte-identical output. Video
//! containers need `ffmpeg`; progressive JPEG and JPEG restart intervals need
//! `python3` with Pillow. Those functions return
//! [`FixtureError::Missing`](crate::FixtureError::Missing) when the tool is
//! absent, which callers turn into a skip.

pub mod canvas;
pub mod exif;
pub mod heic;
pub mod jpeg;
pub mod png;
pub mod stills;
pub mod tiff;
pub mod video;

pub use heic::{heic, HeicOpts};
pub use jpeg::{baseline_jpeg, jpeg, Chroma, ExifOpts, JpegOpts};
pub use png::{png, PngOpts};
pub use stills::{bmp, gif, webp, StillOpts};
pub use tiff::{cr2_like, tiff, TiffOpts};
pub use video::{video, AudioCodec, Container, VideoCodec, VideoOpts};

/// Pixel layout of a generated PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngColor {
    /// 8-bit truecolour (colour type 2).
    Rgb,
    /// 8-bit truecolour with alpha (colour type 6).
    Rgba,
    /// 8-bit greyscale (colour type 0).
    Gray,
}
