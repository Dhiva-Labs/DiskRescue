//! A tiny EXIF writer.
//!
//! Builds the APP1 segment (`FF E1 len "Exif\0\0" TIFF`) with:
//!
//! * IFD0: Make, Model, Orientation, XResolution, YResolution,
//!   ResolutionUnit, Software, DateTime, ExifIFD pointer (0x8769) and, when a
//!   thumbnail is requested, the link to IFD1;
//! * ExifIFD: ExposureTime, FNumber, ISO, ExifVersion, `DateTimeOriginal`
//!   (0x9003), `DateTimeDigitized` (0x9004), PixelX/YDimension;
//! * IFD1: Compression = 6, `JPEGInterchangeFormat` (0x0201) and
//!   `JPEGInterchangeFormatLength` (0x0202) pointing at an embedded JPEG
//!   thumbnail (no EXIF of its own).
//!
//! The TIFF structure comes from the generic layout engine in
//! [`mod@crate::media::tiff`]; offsets are relative to the TIFF header as EXIF
//! requires.

use crate::error::{FixtureError, Result};
use crate::media::jpeg::{baseline_jpeg, ExifOpts};
use crate::media::tiff::{Ifd, TiffBuilder, Value};

/// Build the TIFF blob (header onwards, without the `Exif\0\0` prefix).
pub fn exif_tiff(opts: &ExifOpts, width: u32, height: u32, seed: u64) -> Vec<u8> {
    let mut b = TiffBuilder::new();
    let thumb_blob = opts.thumbnail.then(|| {
        let tw = 160u32;
        let th = ((height as u64 * tw as u64) / width.max(1) as u64).clamp(1, 240) as u32;
        b.add_blob(baseline_jpeg(tw, th, 50, seed))
    });

    let mut ifd0 = Ifd::default();
    ifd0.push(0x010F, Value::Ascii(opts.make.clone()));
    ifd0.push(0x0110, Value::Ascii(opts.model.clone()));
    ifd0.push(0x0112, Value::Shorts(vec![opts.orientation.clamp(1, 8)]));
    ifd0.push(0x011A, Value::Rational(72, 1));
    ifd0.push(0x011B, Value::Rational(72, 1));
    ifd0.push(0x0128, Value::Shorts(vec![2]));
    ifd0.push(0x0131, Value::Ascii("DiskRescue fixtures".into()));
    ifd0.push(0x0132, Value::Ascii(opts.date_time_original.clone()));
    ifd0.push(0x8769, Value::IfdOffset(1));
    if thumb_blob.is_some() {
        ifd0.next = Some(2);
    }

    let mut exif = Ifd::default();
    exif.push(0x829A, Value::Rational(1, 125));
    exif.push(0x829D, Value::Rational(28, 10));
    exif.push(0x8827, Value::Shorts(vec![200]));
    exif.push(
        0x9000,
        Value::Bytes(crate::media::tiff::FieldType::Undefined, b"0230".to_vec()),
    );
    exif.push(0x9003, Value::Ascii(opts.date_time_original.clone()));
    exif.push(0x9004, Value::Ascii(opts.date_time_original.clone()));
    exif.push(0xA002, Value::Longs(vec![width]));
    exif.push(0xA003, Value::Longs(vec![height]));

    b.add_ifd(ifd0);
    b.add_ifd(exif);
    if let Some(blob) = thumb_blob {
        let mut ifd1 = Ifd::default();
        ifd1.push(0x0103, Value::Shorts(vec![6]));
        ifd1.push(0x011A, Value::Rational(72, 1));
        ifd1.push(0x011B, Value::Rational(72, 1));
        ifd1.push(0x0128, Value::Shorts(vec![2]));
        ifd1.push(0x0201, Value::BlobOffset(blob));
        ifd1.push(0x0202, Value::BlobLen(blob));
        b.add_ifd(ifd1);
    }
    b.build().0
}

/// The complete APP1 marker segment, ready to splice after SOI.
///
/// Fails when the payload would not fit the 16-bit segment length.
pub fn app1_segment(opts: &ExifOpts, width: u32, height: u32, seed: u64) -> Result<Vec<u8>> {
    let tiff = exif_tiff(opts, width, height, seed);
    let len = 2 + 6 + tiff.len();
    if len > 0xFFFF {
        return Err(FixtureError::invalid(format!(
            "EXIF block too large for one APP1 segment ({len} bytes)"
        )));
    }
    let mut out = Vec::with_capacity(2 + len);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&tiff);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::util::bytes::{u16le, u32le};

    #[test]
    fn layout_matches_exif_rules() {
        let t = exif_tiff(&ExifOpts::default(), 640, 480, 1);
        assert_eq!(&t[..4], b"II*\0");
        let ifd0 = u32le(&t, 4).unwrap() as usize;
        let n = u16le(&t, ifd0).unwrap() as usize;
        let mut exif_off = 0;
        let mut tags = Vec::new();
        for i in 0..n {
            let e = ifd0 + 2 + i * 12;
            let tag = u16le(&t, e).unwrap();
            tags.push(tag);
            if tag == 0x8769 {
                exif_off = u32le(&t, e + 8).unwrap() as usize;
            }
        }
        assert!(tags.windows(2).all(|w| w[0] < w[1]));
        assert!(tags.contains(&0x010F) && tags.contains(&0x0112));
        // ExifIFD holds both dates.
        let en = u16le(&t, exif_off).unwrap() as usize;
        let etags: Vec<u16> = (0..en)
            .map(|i| u16le(&t, exif_off + 2 + i * 12).unwrap())
            .collect();
        assert!(etags.contains(&0x9003) && etags.contains(&0x9004));
        // IFD1 follows and its thumbnail is a JPEG.
        let ifd1 = u32le(&t, ifd0 + 2 + n * 12).unwrap() as usize;
        assert_ne!(ifd1, 0);
        let n1 = u16le(&t, ifd1).unwrap() as usize;
        let mut off = 0;
        let mut len = 0;
        for i in 0..n1 {
            let e = ifd1 + 2 + i * 12;
            match u16le(&t, e).unwrap() {
                0x0201 => off = u32le(&t, e + 8).unwrap() as usize,
                0x0202 => len = u32le(&t, e + 8).unwrap() as usize,
                _ => {}
            }
        }
        assert!(len > 100);
        assert_eq!(&t[off..off + 2], &[0xFF, 0xD8]);
        assert_eq!(&t[off + len - 2..off + len], &[0xFF, 0xD9]);
        // No thumbnail variant has no IFD1.
        let t2 = exif_tiff(
            &ExifOpts {
                thumbnail: false,
                ..Default::default()
            },
            640,
            480,
            1,
        );
        let n0 = u16le(&t2, 8).unwrap() as usize;
        assert_eq!(u32le(&t2, 8 + 2 + n0 * 12).unwrap(), 0);
    }
}
