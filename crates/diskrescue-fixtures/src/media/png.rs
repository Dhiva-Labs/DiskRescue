//! Minimal PNG writer.
//!
//! Written here rather than through the `image` crate because the fixtures
//! need Adam7-interlaced output (the `image` encoder cannot produce it) and
//! full control over the chunk layout (ancillary chunks, IDAT splitting) so
//! the PNG repair tests have known targets. Output: signature, IHDR, an
//! optional `tEXt`, one or more IDAT chunks (zlib, filter type per scanline
//! chosen as `Sub` for colour and `None` for grey), IEND.

use std::io::Write;

use crate::media::canvas;
use crate::media::PngColor;

/// Options for [`png`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PngOpts {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Content seed.
    pub seed: u64,
    /// Adam7 interlacing.
    pub interlaced: bool,
    /// Colour type.
    pub color: PngColor,
    /// Split the image data into IDAT chunks of at most this many bytes
    /// (`0` = one chunk). Several chunks make truncation tests realistic.
    pub idat_chunk: usize,
    /// Add a `tEXt` chunk (`Software=DiskRescue fixtures`) after IHDR.
    pub text_chunk: bool,
}

impl Default for PngOpts {
    fn default() -> Self {
        PngOpts {
            width: 320,
            height: 240,
            seed: 1,
            interlaced: false,
            color: PngColor::Rgb,
            idat_chunk: 8192,
            text_chunk: true,
        }
    }
}

/// Build a PNG chunk: length, type, data, CRC.
pub fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut h = crc32fast::Hasher::new();
    h.update(kind);
    h.update(data);
    out.extend_from_slice(&h.finalize().to_be_bytes());
    out
}

/// The 8-byte PNG signature.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const ADAM7: [(u32, u32, u32, u32); 7] = [
    // (x start, y start, x step, y step)
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn filter_sub(row: &[u8], bpp: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(row.len() + 1);
    out.push(1);
    for (i, &b) in row.iter().enumerate() {
        let left = if i >= bpp { row[i - bpp] } else { 0 };
        out.push(b.wrapping_sub(left));
    }
    out
}

/// Layout parameters for [`encode_raw`].
#[derive(Debug, Clone, Copy)]
pub struct RawLayout {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// PNG colour type byte (0 grey, 2 RGB, 6 RGBA).
    pub color_type: u8,
    /// Bytes per pixel (8-bit samples).
    pub bpp: usize,
    /// Adam7 interlacing.
    pub interlaced: bool,
    /// Maximum IDAT payload size (`0` = one chunk).
    pub idat_chunk: usize,
    /// Add a `tEXt` chunk.
    pub text_chunk: bool,
}

/// Encode raw pixels (`bpp` bytes per pixel, row-major, no padding) as a PNG.
pub fn encode_raw(layout: &RawLayout, pixels: &[u8]) -> Vec<u8> {
    let RawLayout {
        width,
        height,
        color_type,
        bpp,
        interlaced,
        idat_chunk,
        text_chunk,
    } = *layout;
    let stride = width as usize * bpp;
    let mut raw = Vec::with_capacity(pixels.len() + height as usize);
    let use_sub = bpp >= 3;
    let filter = |row: &[u8], raw: &mut Vec<u8>| {
        if use_sub {
            raw.extend_from_slice(&filter_sub(row, bpp));
        } else {
            raw.push(0);
            raw.extend_from_slice(row);
        }
    };
    if interlaced {
        for &(xs, ys, xstep, ystep) in &ADAM7 {
            if xs >= width || ys >= height {
                continue;
            }
            let pw = (width - xs).div_ceil(xstep) as usize;
            let mut row = Vec::with_capacity(pw * bpp);
            let mut y = ys;
            while y < height {
                row.clear();
                let mut x = xs;
                while x < width {
                    let off = y as usize * stride + x as usize * bpp;
                    row.extend_from_slice(&pixels[off..off + bpp]);
                    x += xstep;
                }
                filter(&row, &mut raw);
                y += ystep;
            }
        }
    } else {
        for y in 0..height as usize {
            filter(&pixels[y * stride..(y + 1) * stride], &mut raw);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(&raw).expect("in-memory write");
    let zdata = z.finish().expect("in-memory finish");

    let mut out = Vec::with_capacity(zdata.len() + 128);
    out.extend_from_slice(&SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color_type, 0, 0, u8::from(interlaced)]);
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    if text_chunk {
        out.extend_from_slice(&chunk(b"tEXt", b"Software\0DiskRescue fixtures"));
    }
    let piece = if idat_chunk == 0 {
        zdata.len().max(1)
    } else {
        idat_chunk
    };
    for part in zdata.chunks(piece) {
        out.extend_from_slice(&chunk(b"IDAT", part));
    }
    out.extend_from_slice(&chunk(b"IEND", &[]));
    out
}

/// Generate a PNG of the canvas.
pub fn png(opts: &PngOpts) -> Vec<u8> {
    let (w, h) = canvas::clamp_dims(opts.width, opts.height);
    let (pixels, color_type, bpp): (Vec<u8>, u8, usize) = match opts.color {
        PngColor::Rgb => (canvas::canvas_rgb(w, h, opts.seed), 2, 3),
        PngColor::Rgba => {
            let rgb = canvas::canvas_rgb(w, h, opts.seed);
            let mut v = Vec::with_capacity(rgb.len() / 3 * 4);
            for (i, p) in rgb.chunks_exact(3).enumerate() {
                v.extend_from_slice(p);
                // Alpha ramps across the width so it is observable.
                v.push(((i as u32 % w) * 255 / w.max(1)) as u8);
            }
            (v, 6, 4)
        }
        PngColor::Gray => (canvas::canvas_gray(w, h, opts.seed), 0, 1),
    };
    encode_raw(
        &RawLayout {
            width: w,
            height: h,
            color_type,
            bpp,
            interlaced: opts.interlaced,
            idat_chunk: opts.idat_chunk,
            text_chunk: opts.text_chunk,
        },
        &pixels,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageDecoder;

    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let dec = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let (w, h) = dec.dimensions();
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).unwrap();
        (w, h, buf)
    }

    #[test]
    fn roundtrip_all_variants() {
        for color in [PngColor::Rgb, PngColor::Rgba, PngColor::Gray] {
            for interlaced in [false, true] {
                let opts = PngOpts {
                    width: 37,
                    height: 23,
                    seed: 5,
                    interlaced,
                    color,
                    idat_chunk: 500,
                    text_chunk: true,
                };
                let bytes = png(&opts);
                assert_eq!(&bytes[..8], &SIGNATURE);
                let (w, h, pix) = decode(&bytes);
                assert_eq!((w, h), (37, 23));
                let expect = match color {
                    PngColor::Rgb => canvas::canvas_rgb(37, 23, 5),
                    PngColor::Gray => canvas::canvas_gray(37, 23, 5),
                    PngColor::Rgba => {
                        let p = png(&PngOpts {
                            interlaced: false,
                            ..opts.clone()
                        });
                        decode(&p).2
                    }
                };
                assert_eq!(pix, expect, "{color:?} interlaced={interlaced}");
            }
        }
    }

    #[test]
    fn chunk_crc_is_standard() {
        // IEND's CRC is the well-known AE 42 60 82.
        assert_eq!(chunk(b"IEND", &[])[8..], [0xAE, 0x42, 0x60, 0x82]);
        let one = png(&PngOpts {
            idat_chunk: 0,
            text_chunk: false,
            ..Default::default()
        });
        assert_eq!(one.windows(4).filter(|w| w == b"IDAT").count(), 1);
    }
}
