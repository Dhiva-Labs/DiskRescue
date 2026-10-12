//! A small PNG writer used by repair: chunk framing, a chunking IDAT sink and
//! the "unknown pixel" fill colour.

use std::io::{self, Write};

use crate::chunks::SIGNATURE;
use crate::ihdr::Ihdr;
use crate::io::chunk_crc;

/// Payload size of the IDAT chunks the encoder emits.
pub(crate) const IDAT_CHUNK: usize = 64 * 1024;

/// Write the PNG signature.
pub(crate) fn write_signature(w: &mut dyn Write) -> io::Result<()> {
    w.write_all(&SIGNATURE)
}

/// Write one complete chunk (length, type, data, CRC).
pub(crate) fn write_chunk(w: &mut dyn Write, kind: &[u8; 4], data: &[u8]) -> io::Result<()> {
    w.write_all(&(data.len() as u32).to_be_bytes())?;
    w.write_all(kind)?;
    w.write_all(data)?;
    w.write_all(&chunk_crc(kind, data).to_be_bytes())
}

/// Build one complete chunk in memory.
pub(crate) fn chunk_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + data.len());
    // Writing to a Vec cannot fail.
    let _ = write_chunk(&mut v, kind, data);
    v
}

/// A `Write` adapter that cuts its input into IDAT chunks.
pub(crate) struct IdatWriter<'a> {
    out: &'a mut dyn Write,
    buf: Vec<u8>,
    chunks: u32,
}

impl<'a> IdatWriter<'a> {
    pub(crate) fn new(out: &'a mut dyn Write) -> Self {
        IdatWriter {
            out,
            buf: Vec::with_capacity(IDAT_CHUNK),
            chunks: 0,
        }
    }

    fn emit(&mut self) -> io::Result<()> {
        write_chunk(self.out, b"IDAT", &self.buf)?;
        self.buf.clear();
        self.chunks += 1;
        Ok(())
    }

    /// Flush the final chunk. At least one IDAT is always written.
    pub(crate) fn finish(mut self) -> io::Result<u32> {
        if !self.buf.is_empty() || self.chunks == 0 {
            self.emit()?;
        }
        Ok(self.chunks)
    }
}

impl Write for IdatWriter<'_> {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let total = data.len();
        while !data.is_empty() {
            let room = IDAT_CHUNK - self.buf.len();
            let take = room.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == IDAT_CHUNK {
                self.emit()?;
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The colour used for pixels that could not be recovered, expressed as one
/// packed pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fill {
    /// Pixel bytes for bit depths of 8 and more, or the sample value in the
    /// low bits of byte 0 for smaller depths.
    pub pixel: [u8; 8],
    /// Human description for the repair notes.
    pub describe: &'static str,
}

/// Choose the fill colour: mid-grey for opaque colour types, transparent for
/// types with alpha, and the palette entry closest to mid-grey for indexed
/// images (`plte` is the PLTE chunk data when known).
pub(crate) fn fill_for(ihdr: &Ihdr, plte: Option<&[u8]>) -> Fill {
    let mut pixel = [0u8; 8];
    let depth = ihdr.bit_depth;
    let describe;
    match ihdr.color_type {
        0 => {
            describe = "mid-grey";
            pixel[0] = if depth >= 8 { 0x80 } else { 1u8 << (depth - 1) };
        }
        2 => {
            describe = "mid-grey";
            if depth == 16 {
                for c in 0..3 {
                    pixel[c * 2] = 0x80;
                }
            } else {
                pixel[..3].fill(0x80);
            }
        }
        3 => {
            describe = "mid-grey";
            if let Some(p) = plte {
                let mut best = (u32::MAX, 0u8);
                for (i, e) in p.chunks_exact(3).enumerate().take(1usize << depth.min(8)) {
                    let d: i32 = e.iter().map(|&c| (c as i32 - 128).abs()).sum();
                    if (d as u32) < best.0 {
                        best = (d as u32, i as u8);
                    }
                }
                pixel[0] = best.1;
            }
        }
        4 => {
            // Fully transparent grey.
            describe = "transparent";
            pixel[0] = 0x80;
        }
        _ => {
            // RGBA: transparent grey.
            describe = "transparent";
            if depth == 16 {
                for c in 0..3 {
                    pixel[c * 2] = 0x80;
                }
            } else {
                pixel[..3].fill(0x80);
            }
        }
    }
    Fill { pixel, describe }
}

/// Fill a whole row with `fill`.
pub(crate) fn fill_row(ihdr: &Ihdr, fill: &Fill, width: u32, row: &mut [u8]) {
    let bits = ihdr.bits_per_pixel();
    row.fill(0);
    for x in 0..width as usize {
        crate::lenient::set_pixel(row, x, bits, &fill.pixel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ihdr(color: u8, depth: u8) -> Ihdr {
        Ihdr {
            width: 5,
            height: 2,
            bit_depth: depth,
            color_type: color,
            compression: 0,
            filter: 0,
            interlace: 0,
        }
    }

    #[test]
    fn idat_writer_chunks_and_always_emits_one() {
        let mut out = Vec::new();
        let mut w = IdatWriter::new(&mut out);
        w.write_all(&vec![7u8; IDAT_CHUNK * 2 + 10]).unwrap();
        assert_eq!(w.finish().unwrap(), 3);
        let mut empty = Vec::new();
        assert_eq!(IdatWriter::new(&mut empty).finish().unwrap(), 1);
        assert_eq!(empty.len(), 12);
    }

    #[test]
    fn fill_colours() {
        let h = ihdr(2, 8);
        let f = fill_for(&h, None);
        let mut row = vec![0u8; 15];
        fill_row(&h, &f, 5, &mut row);
        assert!(row.iter().all(|&b| b == 0x80));

        let h = ihdr(6, 8);
        let f = fill_for(&h, None);
        let mut row = vec![0u8; 20];
        fill_row(&h, &f, 5, &mut row);
        assert_eq!(&row[..4], &[0x80, 0x80, 0x80, 0]);
        assert_eq!(f.describe, "transparent");

        let h = ihdr(0, 1);
        let f = fill_for(&h, None);
        let mut row = vec![0u8; 1];
        fill_row(&h, &f, 5, &mut row);
        assert_eq!(row[0], 0b1111_1000);

        let h = ihdr(3, 8);
        let plte = [0, 0, 0, 120, 130, 125, 255, 255, 255];
        let f = fill_for(&h, Some(&plte));
        assert_eq!(f.pixel[0], 1);

        let h = ihdr(0, 16);
        let f = fill_for(&h, None);
        let mut row = vec![0u8; 10];
        fill_row(&h, &f, 5, &mut row);
        assert_eq!(&row[..4], &[0x80, 0, 0x80, 0]);
    }
}
