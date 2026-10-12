//! Deterministic picture content.
//!
//! The canvas is a smooth colour gradient with per-pixel noise (so JPEG and
//! PNG output has realistic entropy and size), a frame, and a handful of
//! "text-like" striped blocks whose positions depend on the seed. The content
//! is visually distinct from top to bottom, so a truncated or corrupted copy
//! is obviously different from the original when decoded.

use image::{Rgb, RgbImage};

use crate::rng::{hash3, Rng};

/// Noise amplitude in 8-bit levels.
const NOISE: i32 = 36;

/// Largest edge (and, squared, pixel budget) the generators accept. Larger
/// requests are clamped so absurd sizes cannot exhaust memory.
pub const MAX_EDGE: u32 = 16_384;

/// Largest number of pixels in one canvas (64 Mpixel).
pub const MAX_PIXELS: u64 = 1 << 26;

/// Clamp requested dimensions to `1..=MAX_EDGE` and at most `MAX_PIXELS`
/// pixels (the height is reduced first).
pub fn clamp_dims(width: u32, height: u32) -> (u32, u32) {
    let w = width.clamp(1, MAX_EDGE);
    let mut h = height.clamp(1, MAX_EDGE);
    if (w as u64) * (h as u64) > MAX_PIXELS {
        h = (MAX_PIXELS / w as u64).max(1) as u32;
    }
    (w, h)
}

/// Generate the canvas. Equal `(width, height, seed)` gives identical pixels.
/// Dimensions are clamped with [`clamp_dims`].
pub fn canvas(width: u32, height: u32, seed: u64) -> RgbImage {
    let (w, h) = clamp_dims(width, height);
    let mut img = RgbImage::new(w, h);
    let mut rng = Rng::new(seed ^ 0xC0FF_EE00);
    // Text-like blocks: rectangles filled with alternating bars.
    let nblocks = 3 + rng.below(4) as usize;
    let mut blocks = Vec::with_capacity(nblocks);
    for _ in 0..nblocks {
        let bw = (w / 4).max(4) + rng.below((w / 4).max(1) as u64) as u32;
        let bh = (h / 10).max(3) + rng.below((h / 10).max(1) as u64) as u32;
        let bx = rng.below(w.saturating_sub(bw).max(1) as u64) as u32;
        let by = rng.below(h.saturating_sub(bh).max(1) as u64) as u32;
        let dark = rng.next_u32() & 1 == 0;
        blocks.push((bx, by, bw, bh, dark));
    }
    let hue = (seed % 7) as u32;
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            // Base gradient, rotated per seed so different seeds differ.
            let (mut r, mut g, mut b) = match hue {
                0 => (fx, fy, 1.0 - fx * fy),
                1 => (1.0 - fx, fy, fx),
                2 => (fy, 1.0 - fx, fx * fy),
                3 => (fx * fx, fy * fy, 0.5),
                4 => (0.5 + 0.5 * fy, fx, 1.0 - fy),
                5 => (1.0 - fy, 1.0 - fx, fx),
                _ => (fx, 0.5, fy),
            };
            // A diagonal band so rows are distinguishable.
            let band = ((fx + fy) * 6.0).fract();
            if band < 0.15 {
                r = (r + 0.25).min(1.0);
                g = (g + 0.25).min(1.0);
                b = (b + 0.25).min(1.0);
            }
            let mut px = [(r * 255.0) as i32, (g * 255.0) as i32, (b * 255.0) as i32];
            // Text-like blocks.
            for &(bx, by, bw, bh, dark) in &blocks {
                if x >= bx && x < bx + bw && y >= by && y < by + bh {
                    let bar = ((x - bx) / 3 + (y - by) / 5) % 2 == 0;
                    let v = if bar == dark { 20 } else { 235 };
                    px = [v, v, v];
                }
            }
            // Frame.
            if x < 2 || y < 2 || x + 3 > w || y + 3 > h {
                px = [250, 250, 250];
            }
            // Noise.
            let n = hash3(x as u64, y as u64, seed);
            for (i, c) in px.iter_mut().enumerate() {
                let noise = ((n >> (i * 8)) & 0xFF) as i32 * (2 * NOISE) / 255 - NOISE;
                *c = (*c + noise).clamp(0, 255);
            }
            img.put_pixel(x, y, Rgb([px[0] as u8, px[1] as u8, px[2] as u8]));
        }
    }
    img
}

/// Raw interleaved RGB bytes of the canvas.
pub fn canvas_rgb(width: u32, height: u32, seed: u64) -> Vec<u8> {
    canvas(width, height, seed).into_raw()
}

/// 8-bit grey version (ITU-R 601 luma).
pub fn canvas_gray(width: u32, height: u32, seed: u64) -> Vec<u8> {
    canvas(width, height, seed)
        .pixels()
        .map(|p| {
            let [r, g, b] = p.0;
            ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_distinct() {
        let a = canvas(64, 48, 1);
        let b = canvas(64, 48, 1);
        let c = canvas(64, 48, 2);
        assert_eq!(a.as_raw(), b.as_raw());
        assert_ne!(a.as_raw(), c.as_raw());
        // Top and bottom rows differ (truncation is observable).
        let top: Vec<u8> = a.as_raw()[..64 * 3].to_vec();
        let bottom: Vec<u8> = a.as_raw()[a.as_raw().len() - 64 * 3..].to_vec();
        assert_ne!(top, bottom);
        assert_eq!(canvas_gray(8, 8, 3).len(), 64);
        assert_eq!(canvas(0, 0, 0).dimensions(), (1, 1));
        assert_eq!(clamp_dims(u32::MAX, u32::MAX), (MAX_EDGE, 4096));
        assert_eq!(clamp_dims(100, 50), (100, 50));
    }
}
