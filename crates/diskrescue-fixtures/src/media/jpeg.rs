//! JPEG generator.
//!
//! * Plain baseline JPEGs come from the `image` crate's encoder and need no
//!   external tool ([`baseline_jpeg`]).
//! * Progressive output, restart intervals and chroma subsampling other than
//!   4:4:4 are produced by `python3` + Pillow (libjpeg); without them
//!   [`jpeg`] returns [`FixtureError::Missing`].
//! * The EXIF APP1 segment is assembled here (see [`crate::media::exif`]) and
//!   spliced directly after SOI, replacing any JFIF APP0 segment, which is
//!   the layout most cameras write (`SOI APP1(Exif) DQT ...`).

use std::path::Path;

use crate::error::{FixtureError, Result};
use crate::media::canvas;
use crate::media::exif;
use crate::tools;

/// Chroma subsampling of the generated JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:4:4, what the pure-Rust encoder writes.
    S444,
    /// 4:2:2 (needs Pillow).
    S422,
    /// 4:2:0, what most cameras write (needs Pillow).
    S420,
}

/// EXIF contents written by [`jpeg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExifOpts {
    /// `DateTimeOriginal` (tag 0x9003) and `DateTimeDigitized` (0x9004),
    /// `YYYY:MM:DD HH:MM:SS`.
    pub date_time_original: String,
    /// Camera make (tag 0x010F).
    pub make: String,
    /// Camera model (tag 0x0110).
    pub model: String,
    /// Orientation 1..=8 (tag 0x0112).
    pub orientation: u16,
    /// Embed an IFD1 thumbnail (a small JPEG at `JPEGInterchangeFormat`).
    pub thumbnail: bool,
}

impl Default for ExifOpts {
    fn default() -> Self {
        ExifOpts {
            date_time_original: "2024:05:06 07:08:09".into(),
            make: "DiskRescue".into(),
            model: "FixtureCam".into(),
            orientation: 1,
            thumbnail: true,
        }
    }
}

/// Options for [`jpeg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpegOpts {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Quality 1..=100.
    pub quality: u8,
    /// Content seed.
    pub seed: u64,
    /// EXIF block, if any.
    pub exif: Option<ExifOpts>,
    /// Restart interval in MCUs (`DRI`); `None` = no restart markers.
    /// Needs Pillow.
    pub restart_interval: Option<u16>,
    /// Progressive (multi-scan) encoding. Needs Pillow.
    pub progressive: bool,
    /// Chroma subsampling.
    pub chroma: Chroma,
}

impl Default for JpegOpts {
    fn default() -> Self {
        JpegOpts {
            width: 640,
            height: 480,
            quality: 85,
            seed: 1,
            exif: None,
            restart_interval: None,
            progressive: false,
            chroma: Chroma::S444,
        }
    }
}

/// A plain baseline sequential JPEG (4:4:4, JFIF APP0, no EXIF) of the
/// canvas. Pure Rust, never fails.
pub fn baseline_jpeg(width: u32, height: u32, quality: u8, seed: u64) -> Vec<u8> {
    let img = canvas::canvas(width, height, seed);
    let mut out = Vec::new();
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    enc.encode(
        img.as_raw(),
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )
    .expect("encoding into a Vec cannot fail");
    out
}

const PILLOW_SCRIPT: &str = r#"
import sys
from PIL import Image
src, dst, quality, progressive, rst, sub = sys.argv[1:7]
im = Image.open(src).convert("RGB")
kw = dict(quality=int(quality), progressive=bool(int(progressive)), subsampling=int(sub))
if int(rst):
    kw["restart_marker_blocks"] = int(rst)
im.save(dst, "JPEG", **kw)
"#;

/// Encode through Pillow (libjpeg): progressive, restart markers, chroma.
fn pillow_jpeg(opts: &JpegOpts) -> Result<Vec<u8>> {
    tools::require_pillow()?;
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("in.ppm");
    let dst = dir.path().join("out.jpg");
    let (w, h) = canvas::clamp_dims(opts.width, opts.height);
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.extend_from_slice(&canvas::canvas_rgb(w, h, opts.seed));
    std::fs::write(&src, ppm)?;
    let sub = match opts.chroma {
        Chroma::S444 => "0",
        Chroma::S422 => "1",
        Chroma::S420 => "2",
    };
    let q = opts.quality.clamp(1, 100).to_string();
    let rst = opts.restart_interval.unwrap_or(0).to_string();
    let prog = if opts.progressive { "1" } else { "0" };
    let path_str = |p: &Path| p.to_string_lossy().into_owned();
    let (s, d) = (path_str(&src), path_str(&dst));
    tools::python(PILLOW_SCRIPT, &[&s, &d, &q, prog, &rst, sub])?;
    Ok(std::fs::read(&dst)?)
}

/// Generate a JPEG.
///
/// Plain baseline 4:4:4 output (optionally with EXIF) is pure Rust. Setting
/// `progressive`, `restart_interval` or a chroma other than 4:4:4 routes the
/// encode through Pillow and yields `Err(Missing)` when it is unavailable.
pub fn jpeg(opts: &JpegOpts) -> Result<Vec<u8>> {
    let (cw, ch) = canvas::clamp_dims(opts.width, opts.height);
    let clamped = JpegOpts {
        width: cw,
        height: ch,
        ..opts.clone()
    };
    let opts = &clamped;
    let needs_pillow =
        opts.progressive || opts.restart_interval.is_some() || opts.chroma != Chroma::S444;
    let mut data = if needs_pillow {
        pillow_jpeg(opts)?
    } else {
        baseline_jpeg(opts.width, opts.height, opts.quality, opts.seed)
    };
    if let Some(ex) = &opts.exif {
        let app1 = exif::app1_segment(ex, opts.width, opts.height, opts.seed)?;
        data = splice_after_soi(&data, &app1)?;
    }
    Ok(data)
}

/// Insert `segment` (a complete marker segment) directly after SOI,
/// dropping a JFIF APP0 segment that would otherwise precede it.
pub fn splice_after_soi(jpeg: &[u8], segment: &[u8]) -> Result<Vec<u8>> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return Err(FixtureError::invalid("not a JPEG (no SOI)"));
    }
    let mut rest = &jpeg[2..];
    // Drop a leading JFIF APP0.
    if rest.len() >= 4 && rest[0] == 0xFF && rest[1] == 0xE0 {
        let len = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        if len >= 2 && rest.len() >= 2 + len && rest[4..].starts_with(b"JFIF\0") {
            rest = &rest[2 + len..];
        }
    }
    let mut out = Vec::with_capacity(jpeg.len() + segment.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(segment);
    out.extend_from_slice(rest);
    Ok(out)
}

/// A flat list of the marker segments of a JPEG up to SOS: `(marker,
/// offset, total length including the marker bytes)`. Test helper.
pub fn segments(jpeg: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 2;
    while pos + 4 <= jpeg.len() && jpeg[pos] == 0xFF {
        let m = jpeg[pos + 1];
        if m == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        out.push((m, pos, 2 + len));
        if m == 0xDA {
            break;
        }
        pos += 2 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_decodes_and_is_deterministic() {
        let a = baseline_jpeg(64, 48, 80, 3);
        let b = baseline_jpeg(64, 48, 80, 3);
        let c = baseline_jpeg(64, 48, 80, 4);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(&a[..3], &[0xFF, 0xD8, 0xFF]);
        assert_eq!(&a[a.len() - 2..], &[0xFF, 0xD9]);
        let img = image::load_from_memory_with_format(&a, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (64, 48));
    }

    #[test]
    fn exif_is_spliced_after_soi() {
        let data = jpeg(&JpegOpts {
            width: 200,
            height: 150,
            exif: Some(ExifOpts::default()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(&data[..4], &[0xFF, 0xD8, 0xFF, 0xE1]);
        assert_eq!(&data[6..12], b"Exif\0\0");
        let segs = segments(&data);
        assert_eq!(segs[0].0, 0xE1);
        assert!(segs.iter().all(|s| s.0 != 0xE0), "JFIF dropped");
        // Still decodes.
        image::load_from_memory_with_format(&data, image::ImageFormat::Jpeg).unwrap();
    }

    #[test]
    fn splice_rejects_non_jpeg() {
        assert!(splice_after_soi(b"nope", &[]).is_err());
    }
}
