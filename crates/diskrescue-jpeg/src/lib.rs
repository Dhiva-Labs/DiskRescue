//! JPEG support for DiskRescue: carving measurement, Exif, a Huffman scan
//! validator that pinpoints corruption, whole-file validation and repair.
//!
//! * [`JpegFormat`] implements [`diskrescue_core::format::FileFormat`].
//! * [`markers`] walks the header segments (tolerant of fill bytes, stray
//!   bytes and damaged lengths) and parses SOF/DHT/DQT/DRI/SOS/APPn.
//! * [`exif`] reads the TIFF structures inside APP1 (Exif) and APP2 (MPF).
//! * [`scan`] decodes the entropy-coded data symbol by symbol (baseline and
//!   progressive, with restart-marker resynchronisation) and reports where
//!   it stops decoding, without IDCT.
//! * [`repair`] plans and applies repairs (structural fixes, EOI, standard
//!   Huffman tables, donor headers, truncation at the last good restart
//!   interval or MCU) and re-validates the result before writing it.
//! * [`tables`] holds the ITU-T T.81 Annex K tables.
//!
//! # How the extent of a JPEG is found
//!
//! A blind search for `FF D9` stops at the end of the Exif thumbnail, and
//! a blind search for the last `FF D9` swallows the next file. The measure
//! instead hops over header segments by their lengths, decodes (or walks)
//! the entropy-coded data to the EOI that really ends the scans, then
//! accepts data after it only when a structure says so (MPF images,
//! Samsung SEF trailers, Google motion-photo videos). Thumbnails, MPF
//! images and trailers are reported as nested objects so the carver does
//! not emit them twice.
//!
//! Everything reads through a bounded window over the
//! [`Source`](diskrescue_core::Source): large files are measured and
//! validated in constant memory (repair re-validation and the zune-jpeg
//! reference decode are capped at 64 MiB / 50 MP).

#![warn(missing_docs)]

use std::io::Write;

use diskrescue_core::format::{
    FileFormat, Measured, RepairContext, RepairOutcome, RepairResult, Signature, Validation,
    ValidationStatus,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::{SharedSource, SourceExt};
use diskrescue_core::Result;

pub use diskrescue_core as core;

mod cursor;
pub mod exif;
mod huffman;
pub mod markers;
mod measure;
mod pad;
pub mod repair;
pub mod scan;
pub mod tables;
mod tiff;
mod trailer;
mod validate;

pub use exif::{parse_app1, ExifInfo};
pub use markers::{walk_headers, Headers};
pub use measure::MEASURE_DECODE_BUDGET;
pub use repair::{apply, plan, Strategy};
pub use scan::{
    analyze, analyze_source, AnalyzeOptions, CutPoint, DamagedRange, ScanAnalysis, ScanError,
    ScanErrorKind,
};
pub use validate::{zune_check, ZuneCheck, GREY_FILL_COMMENT};

/// Default largest JPEG the carver measures (128 MiB).
pub const DEFAULT_MAX_SIZE: u64 = 128 << 20;

static SIGNATURES: [Signature; 1] = [Signature::at_start(b"\xFF\xD8\xFF")];

/// The JPEG file format (`jpeg`, extensions `jpg`/`jpeg`). Stateless; the
/// carver's `limit` bounds each measurement and [`DEFAULT_MAX_SIZE`] caps it.
#[derive(Debug, Clone, Copy, Default)]
pub struct JpegFormat;

impl JpegFormat {
    /// The format (same as `JpegFormat`).
    pub fn new() -> Self {
        JpegFormat
    }
}

/// Markers accepted right after the SOI by [`JpegFormat::probe`].
fn first_marker_ok(m: u8) -> bool {
    matches!(m, 0xE0..=0xEF | 0xDB | 0xC4 | 0xFE | 0xDD) || (markers::is_sof(m) || m == 0xCC)
}

impl FileFormat for JpegFormat {
    fn id(&self) -> &'static str {
        "jpeg"
    }

    fn name(&self) -> &'static str {
        "JPEG image"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["jpg", "jpeg"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Photo
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        256
    }

    fn max_size(&self) -> u64 {
        DEFAULT_MAX_SIZE
    }

    fn priority(&self) -> u8 {
        50
    }

    /// SOI, optional fill bytes, then a marker that can start a header
    /// (APPn, DQT, DHT, COM, DRI, SOFn, DAC) with a plausible length; when
    /// the whole first segment is in `head`, it must be followed by
    /// another marker.
    fn probe(&self, head: &[u8]) -> bool {
        if head.len() < 4 || head[..3] != [0xFF, 0xD8, 0xFF] {
            return false;
        }
        let mut p = 2usize;
        while head.get(p + 1) == Some(&0xFF) {
            p += 1;
            if p > 16 {
                return false;
            }
        }
        let Some(&m) = head.get(p + 1) else {
            return true;
        };
        if !first_marker_ok(m) {
            return false;
        }
        let Some(len) = head
            .get(p + 2..p + 4)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
        else {
            return true;
        };
        let sane = match m {
            0xDD => len == 4,
            0xDB => len >= 67,
            0xC4 => len >= 19,
            m if markers::is_sof(m) => len >= 11,
            _ => len >= 2,
        };
        if !sane {
            return false;
        }
        let next = p + 2 + len as usize;
        match (head.get(next), head.get(next + 1)) {
            (Some(&0xFF), Some(&n)) => n != 0x00,
            (Some(_), Some(_)) => false,
            _ => true,
        }
    }

    fn may_contain(&self, inner: &str) -> bool {
        matches!(inner, "jpeg" | "jpg" | "mp4" | "mpo")
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        measure::measure(src, start, limit.min(DEFAULT_MAX_SIZE))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        Ok(validate::validate_source(&**src, true)?.validation)
    }

    fn repair(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        let detail = validate::validate_source(&**src, true)?;
        let v = detail.validation;
        if v.status == ValidationStatus::Invalid {
            return Ok(RepairOutcome {
                result: RepairResult::Unrepairable,
                applied: Vec::new(),
                output_len: 0,
                validation: Some(v.clone()),
                notes: v.problems,
            });
        }
        let headers = detail.headers.unwrap_or_else(Headers::empty);
        let analysis = detail.analysis.unwrap_or_default();
        let mut strategies = plan(&headers, &analysis, ctx);
        // Only `validate` knows the full extent (MPF images, SEF and
        // motion-photo trailers); strip exactly when it found extra bytes.
        strategies.retain(|s| *s != Strategy::StripTrailingGarbage);
        if detail.extra_trailing > 0 {
            strategies.push(Strategy::StripTrailingGarbage);
        }
        if v.status == ValidationStatus::Valid && strategies.is_empty() {
            let mut buf = vec![0u8; 1 << 20];
            let size = src.size();
            let mut pos = 0u64;
            while pos < size {
                let n = ((size - pos) as usize).min(buf.len());
                src.read_exact_at(pos, &mut buf[..n])?;
                out.write_all(&buf[..n])?;
                pos += n as u64;
            }
            return Ok(RepairOutcome {
                result: RepairResult::NoRepairNeeded,
                applied: Vec::new(),
                output_len: size,
                validation: Some(v),
                notes: Vec::new(),
            });
        }
        if strategies.is_empty() {
            let mut notes = v.problems.clone();
            if !ctx.aggressive && analysis.first_error.is_some() {
                notes.push(
                    "the only possible repair discards damaged data; enable aggressive repair"
                        .into(),
                );
            }
            if !headers.reached_scan() && ctx.donors.is_empty() {
                notes.push(
                    "the header is damaged; a donor JPEG from the same camera is needed".into(),
                );
            }
            return Ok(RepairOutcome {
                result: RepairResult::Unrepairable,
                applied: Vec::new(),
                output_len: 0,
                validation: Some(v),
                notes,
            });
        }
        apply(src, out, &strategies, ctx)
    }

    fn can_repair(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity() {
        let f = JpegFormat::new();
        assert_eq!(f.id(), "jpeg");
        assert_eq!(f.extensions(), &["jpg", "jpeg"]);
        assert_eq!(f.category(), MediaCategory::Photo);
        assert_eq!(f.min_size(), 256);
        assert_eq!(f.max_size(), 128 << 20);
        assert_eq!(f.priority(), 50);
        assert!(f.may_contain("jpeg"));
        assert!(f.can_repair());
    }

    #[test]
    fn probe_rules() {
        let f = JpegFormat::new();
        let ok = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert!(f.probe(&ok));
        assert!(f.probe(&[0xFF, 0xD8, 0xFF, 0xFF, 0xE1, 0x00, 0x20]));
        assert!(!f.probe(&[0xFF, 0xD8, 0xFF, 0x00, 0x00, 0x10]));
        assert!(!f.probe(&[0xFF, 0xD8, 0xFF, 0xD9]));
        assert!(!f.probe(&[0xFF, 0xD8, 0xFF, 0xDD, 0x00, 0x05]));
        assert!(!f.probe(&[0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x10]));
        assert!(!f.probe(&[0xFF, 0xD8]));
        // Whole APP0 visible: must be followed by a marker.
        let mut seg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 1, 2, 0xFF, 0xDB];
        assert!(f.probe(&seg));
        seg[8] = 0x12;
        assert!(!f.probe(&seg));
    }
}
