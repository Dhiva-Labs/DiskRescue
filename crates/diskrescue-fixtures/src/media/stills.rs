//! GIF, BMP and lossless WebP generators (pure Rust, `image` crate).

use image::ExtendedColorType;

use crate::media::canvas;

/// Options shared by [`gif`], [`bmp`] and [`webp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StillOpts {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Content seed.
    pub seed: u64,
}

impl Default for StillOpts {
    fn default() -> Self {
        StillOpts {
            width: 160,
            height: 120,
            seed: 1,
        }
    }
}

/// A single-frame GIF89a (NeuQuant palette).
pub fn gif(opts: &StillOpts) -> Vec<u8> {
    let img = canvas::canvas(opts.width, opts.height, opts.seed);
    let rgba = image::DynamicImage::ImageRgb8(img).to_rgba8();
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut out);
        enc.encode(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .expect("encoding into a Vec cannot fail");
    }
    out
}

/// A 24-bit uncompressed BMP.
pub fn bmp(opts: &StillOpts) -> Vec<u8> {
    let img = canvas::canvas(opts.width, opts.height, opts.seed);
    let mut out = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut out)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .expect("encoding into a Vec cannot fail");
    out
}

/// A lossless WebP (`VP8L`).
pub fn webp(opts: &StillOpts) -> Vec<u8> {
    let img = canvas::canvas(opts.width, opts.height, opts.seed);
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .expect("encoding into a Vec cannot fail");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_and_decode() {
        let o = StillOpts {
            width: 33,
            height: 21,
            seed: 2,
        };
        let g = gif(&o);
        assert_eq!(&g[..6], b"GIF89a");
        assert_eq!(*g.last().unwrap(), 0x3B);
        let b = bmp(&o);
        assert_eq!(&b[..2], b"BM");
        let w = webp(&o);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..12], b"WEBP");
        for (bytes, fmt) in [
            (&g, image::ImageFormat::Gif),
            (&b, image::ImageFormat::Bmp),
            (&w, image::ImageFormat::WebP),
        ] {
            let im = image::load_from_memory_with_format(bytes, fmt).unwrap();
            assert_eq!((im.width(), im.height()), (33, 21), "{fmt:?}");
        }
        // BMP and WebP are lossless.
        let bm = image::load_from_memory_with_format(&b, image::ImageFormat::Bmp).unwrap();
        assert_eq!(bm.to_rgb8().as_raw(), canvas::canvas(33, 21, 2).as_raw());
    }
}
