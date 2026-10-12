//! The IHDR header and the geometry derived from it.
//!
//! Everything the decoder needs to know about the shape of the pixel data
//! lives here: bits per pixel, scanline strides, the Adam7 pass layout and the
//! exact number of bytes a well formed zlib stream inflates to.

use serde::{Deserialize, Serialize};

/// Largest dimension the PNG specification allows (`2^31 - 1`).
pub const MAX_SPEC_DIMENSION: u32 = 0x7FFF_FFFF;

/// Upper bound on the bytes of one decoded scanline we are willing to
/// buffer. A 64 MiB row is already far beyond any real image.
pub const MAX_ROW_BYTES: u64 = 1 << 26;

/// Adam7 passes as `(start column, start row, column step, row step)`.
pub const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// The parsed fields of an IHDR chunk. Construction does not validate; call
/// [`Ihdr::check`] to find out whether the combination is legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ihdr {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Bits per sample (1, 2, 4, 8 or 16).
    pub bit_depth: u8,
    /// Colour type (0 grey, 2 RGB, 3 palette, 4 grey+alpha, 6 RGBA).
    pub color_type: u8,
    /// Compression method (always 0).
    pub compression: u8,
    /// Filter method (always 0).
    pub filter: u8,
    /// Interlace method (0 none, 1 Adam7).
    pub interlace: u8,
}

/// One Adam7 pass (or the single pass of a non-interlaced image).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pass {
    /// Adam7 pass number `0..7` (always 0 for non-interlaced images).
    pub index: usize,
    /// First column of the pass in the full image.
    pub x0: u32,
    /// First row of the pass in the full image.
    pub y0: u32,
    /// Column step.
    pub dx: u32,
    /// Row step.
    pub dy: u32,
    /// Pixels per row of the pass.
    pub width: u32,
    /// Rows in the pass.
    pub height: u32,
}

impl Ihdr {
    /// Length of the IHDR chunk data.
    pub const LEN: usize = 13;

    /// Parse the 13 data bytes. Returns `None` when `data` is not exactly 13
    /// bytes; the field values themselves are not validated.
    pub fn parse(data: &[u8]) -> Option<Ihdr> {
        use diskrescue_core::util::bytes::{u32be, u8_at};
        if data.len() != Self::LEN {
            return None;
        }
        Some(Ihdr {
            width: u32be(data, 0)?,
            height: u32be(data, 4)?,
            bit_depth: u8_at(data, 8)?,
            color_type: u8_at(data, 9)?,
            compression: u8_at(data, 10)?,
            filter: u8_at(data, 11)?,
            interlace: u8_at(data, 12)?,
        })
    }

    /// Serialise back to the 13 data bytes.
    pub fn to_bytes(&self) -> [u8; 13] {
        let mut b = [0u8; 13];
        b[0..4].copy_from_slice(&self.width.to_be_bytes());
        b[4..8].copy_from_slice(&self.height.to_be_bytes());
        b[8] = self.bit_depth;
        b[9] = self.color_type;
        b[10] = self.compression;
        b[11] = self.filter;
        b[12] = self.interlace;
        b
    }

    /// Validate against the PNG specification: dimensions `1..=2^31-1`, a
    /// legal colour type / bit depth combination, compression and filter
    /// method 0 and interlace method 0 or 1. The error names the first
    /// offending field.
    pub fn check(&self) -> Result<(), String> {
        if self.width == 0 || self.width > MAX_SPEC_DIMENSION {
            return Err(format!("width {} out of range", self.width));
        }
        if self.height == 0 || self.height > MAX_SPEC_DIMENSION {
            return Err(format!("height {} out of range", self.height));
        }
        let depth_ok = match self.color_type {
            0 => matches!(self.bit_depth, 1 | 2 | 4 | 8 | 16),
            3 => matches!(self.bit_depth, 1 | 2 | 4 | 8),
            2 | 4 | 6 => matches!(self.bit_depth, 8 | 16),
            other => return Err(format!("unknown colour type {other}")),
        };
        if !depth_ok {
            return Err(format!(
                "bit depth {} is not allowed for colour type {}",
                self.bit_depth, self.color_type
            ));
        }
        if self.compression != 0 {
            return Err(format!("compression method {}", self.compression));
        }
        if self.filter != 0 {
            return Err(format!("filter method {}", self.filter));
        }
        if self.interlace > 1 {
            return Err(format!("interlace method {}", self.interlace));
        }
        Ok(())
    }

    /// True when [`Ihdr::check`] passes and both dimensions are at most
    /// `max_dim`.
    pub fn is_plausible(&self, max_dim: u32) -> bool {
        self.check().is_ok() && self.width <= max_dim && self.height <= max_dim
    }

    /// Samples per pixel (0 for an invalid colour type).
    pub fn channels(&self) -> u32 {
        match self.color_type {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 0,
        }
    }

    /// Bits per pixel (`channels * bit_depth`).
    pub fn bits_per_pixel(&self) -> u32 {
        self.channels() * self.bit_depth as u32
    }

    /// Distance in bytes to the "left" neighbour used by the scanline
    /// filters: `max(1, bits_per_pixel / 8)`.
    pub fn filter_bpp(&self) -> usize {
        ((self.bits_per_pixel() / 8) as usize).max(1)
    }

    /// True for Adam7 interlaced images.
    pub fn is_interlaced(&self) -> bool {
        self.interlace == 1
    }

    /// True when the colour type carries an alpha channel.
    pub fn has_alpha(&self) -> bool {
        matches!(self.color_type, 4 | 6)
    }

    /// Bytes in one unfiltered scanline of `width` pixels (no filter byte).
    pub fn row_bytes_for(&self, width: u32) -> u64 {
        (width as u64 * self.bits_per_pixel() as u64).div_ceil(8)
    }

    /// Bytes in one unfiltered scanline of the full image.
    pub fn row_bytes(&self) -> u64 {
        self.row_bytes_for(self.width)
    }

    /// The non-empty passes in decoding order. A non-interlaced image has
    /// exactly one pass covering the whole image.
    pub fn passes(&self) -> Vec<Pass> {
        if !self.is_interlaced() {
            return vec![Pass {
                index: 0,
                x0: 0,
                y0: 0,
                dx: 1,
                dy: 1,
                width: self.width,
                height: self.height,
            }];
        }
        let mut v = Vec::with_capacity(7);
        for (index, &(x0, y0, dx, dy)) in ADAM7.iter().enumerate() {
            let w = if self.width > x0 {
                (self.width - x0).div_ceil(dx)
            } else {
                0
            };
            let h = if self.height > y0 {
                (self.height - y0).div_ceil(dy)
            } else {
                0
            };
            if w > 0 && h > 0 {
                v.push(Pass {
                    index,
                    x0,
                    y0,
                    dx,
                    dy,
                    width: w,
                    height: h,
                });
            }
        }
        v
    }

    /// Total scanlines across all passes.
    pub fn rows_total(&self) -> u64 {
        self.passes().iter().map(|p| p.height as u64).sum()
    }

    /// Exact size of the inflated image data: every scanline plus its filter
    /// byte, summed over the passes.
    pub fn expected_inflated_len(&self) -> u64 {
        self.passes()
            .iter()
            .map(|p| p.height as u64 * (1 + self.row_bytes_for(p.width)))
            .sum()
    }

    /// Human name of the colour type.
    pub fn color_name(&self) -> &'static str {
        match self.color_type {
            0 => "grayscale",
            2 => "rgb",
            3 => "palette",
            4 => "grayscale+alpha",
            6 => "rgba",
            _ => "unknown",
        }
    }
}

impl Pass {
    /// Bytes in one unfiltered scanline of this pass.
    pub fn row_bytes(&self, ihdr: &Ihdr) -> u64 {
        ihdr.row_bytes_for(self.width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(w: u32, h: u32, depth: u8, color: u8, interlace: u8) -> Ihdr {
        Ihdr {
            width: w,
            height: h,
            bit_depth: depth,
            color_type: color,
            compression: 0,
            filter: 0,
            interlace,
        }
    }

    #[test]
    fn parse_roundtrip() {
        let h = hdr(640, 480, 8, 2, 1);
        assert_eq!(Ihdr::parse(&h.to_bytes()), Some(h));
        assert_eq!(Ihdr::parse(&[0; 12]), None);
        assert_eq!(Ihdr::parse(&[0; 14]), None);
    }

    #[test]
    fn check_rejects_illegal_combinations() {
        assert!(hdr(1, 1, 8, 2, 0).check().is_ok());
        assert!(hdr(0, 1, 8, 2, 0).check().is_err());
        assert!(hdr(1, 0, 8, 2, 0).check().is_err());
        assert!(hdr(1, 1, 4, 2, 0).check().is_err());
        assert!(hdr(1, 1, 16, 3, 0).check().is_err());
        assert!(hdr(1, 1, 8, 5, 0).check().is_err());
        assert!(hdr(1, 1, 1, 0, 0).check().is_ok());
        assert!(hdr(1, 1, 8, 2, 2).check().is_err());
        assert!(hdr(u32::MAX, 1, 8, 2, 0).check().is_err());
        let mut h = hdr(1, 1, 8, 2, 0);
        h.compression = 1;
        assert!(h.check().is_err());
        h.compression = 0;
        h.filter = 1;
        assert!(h.check().is_err());
    }

    #[test]
    fn geometry() {
        let h = hdr(10, 10, 1, 0, 0);
        assert_eq!(h.row_bytes(), 2);
        assert_eq!(h.expected_inflated_len(), 30);
        assert_eq!(h.filter_bpp(), 1);
        let rgb16 = hdr(3, 2, 16, 2, 0);
        assert_eq!(rgb16.bits_per_pixel(), 48);
        assert_eq!(rgb16.filter_bpp(), 6);
        assert_eq!(rgb16.row_bytes(), 18);
    }

    #[test]
    fn adam7_pass_sizes() {
        // 1x1: only pass 1 has pixels.
        let p = hdr(1, 1, 8, 0, 1).passes();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].index, p[0].width, p[0].height), (0, 1, 1));
        // 8x8: 1,1 / 1,1 / 2,1 / 2,2 / 4,2 / 4,4 / 8,4
        let p = hdr(8, 8, 8, 0, 1).passes();
        let dims: Vec<_> = p.iter().map(|p| (p.width, p.height)).collect();
        assert_eq!(
            dims,
            vec![(1, 1), (1, 1), (2, 1), (2, 2), (4, 2), (4, 4), (8, 4)]
        );
        // The passes tile the image exactly.
        let total: u64 = p.iter().map(|p| p.width as u64 * p.height as u64).sum();
        assert_eq!(total, 64);
        // A 5x3 image skips pass 2 (column 4 exists, row 0 exists) - check
        // the pixel total instead of hand-counting.
        let h = hdr(5, 3, 8, 0, 1);
        let total: u64 = h
            .passes()
            .iter()
            .map(|p| p.width as u64 * p.height as u64)
            .sum();
        assert_eq!(total, 15);
        assert_eq!(
            h.rows_total(),
            h.passes().iter().map(|p| p.height as u64).sum()
        );
    }
}
