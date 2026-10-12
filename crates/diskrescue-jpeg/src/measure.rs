//! Carving measurement: the exact extent of a JPEG at a signature hit.
//!
//! 1. Walk the header segments by their lengths (embedded thumbnails are
//!    skipped as payload and reported as nested objects).
//! 2. Decode the entropy-coded data with the scan analyzer (Huffman
//!    validation of the first [`MEASURE_DECODE_BUDGET`] bytes, marker walk
//!    for the rest), following progressive scans through their DHT/SOS
//!    segments, to the real EOI.
//! 3. Extend past the EOI only for structures that say so: MPF images,
//!    Samsung SEF trailers, Google motion-photo videos.
//!
//! Everything is read through a bounded window over the source; nothing
//! past `start + limit` is touched.

use diskrescue_core::format::{Confidence, ExtentStatus, Measured, Nested, NestedRole};
use diskrescue_core::source::{SharedSource, SourceExt};
use diskrescue_core::{Range, Result};

use crate::cursor::{Cursor, SCAN_CHUNK};
use crate::markers::{self, HeaderEnd, Headers};
use crate::scan::{analyze_after_headers, AnalysisDepth, AnalyzeOptions};
use crate::trailer::extend_after_eoi;
use crate::validate::{build_meta, probable_thumbnail};

/// Bytes of entropy-coded data Huffman-decoded during measurement; the
/// rest of larger files is walked marker by marker.
pub const MEASURE_DECODE_BUDGET: u64 = 32 << 20;

fn thumbnails_within(h: &Headers, end: u64) -> Vec<Nested> {
    h.thumbnails
        .iter()
        .filter(|r| r.end <= end && !r.is_empty())
        .map(|r| Nested {
            range: *r,
            format: Some("jpeg".into()),
            role: NestedRole::Thumbnail,
        })
        .collect()
}

/// Measure the JPEG at `start`. See the module docs.
pub(crate) fn measure(src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
    let remaining = src.size().saturating_sub(start);
    let avail = limit.min(remaining);
    if avail < 4 {
        return Ok(None);
    }
    let mut cur = Cursor::new(&**src, start, avail, SCAN_CHUNK);
    if cur.byte(0) != Some(0xFF) || cur.byte(1) != Some(0xD8) || cur.byte(2) != Some(0xFF) {
        return Ok(None);
    }
    let Ok(h) = markers::walk(&mut cur) else {
        return Ok(None);
    };
    let capped_by_limit = |end: u64| end >= avail && avail < remaining;

    if !h.reached_scan() {
        // Only a header remains. Keep it when the data simply ran out and
        // the header is unmistakably a JPEG (its thumbnail may be the only
        // surviving picture); everything else is a false positive.
        let strong = h.segments.len() >= 2
            && (h.exif.is_some() || h.sof.is_some() || !h.dqt.is_empty())
            && h.issues.is_empty();
        return Ok(match h.end {
            HeaderEnd::Truncated { offset } | HeaderEnd::Unreadable { offset } if strong => {
                let mut m = Measured::truncated(
                    offset,
                    Confidence::LOW,
                    offset,
                    "data ends inside the JPEG header, before the first scan",
                );
                if capped_by_limit(offset) {
                    m.status = ExtentStatus::Capped;
                }
                m.nested = thumbnails_within(&h, offset);
                m.meta = build_meta(&h);
                Some(m)
            }
            _ => None,
        });
    }
    let sof = h.sof.as_ref().expect("reached_scan implies SOF");
    if sof.check().is_err() || sof.lossless {
        return Ok(None);
    }
    let scan = &h.scans[0];
    if scan
        .components
        .iter()
        .any(|c| sof.component_index(c.selector).is_none())
    {
        return Ok(None);
    }
    let opts = AnalyzeOptions {
        max_decode_bytes: Some(MEASURE_DECODE_BUDGET),
        unreadable: src
            .unreadable()
            .map(|set| {
                set.intersection_with_range(Range::with_len(start, avail))
                    .shifted_down(start)
            })
            .filter(|set| !set.is_empty()),
        ..Default::default()
    };
    let a = analyze_after_headers(&mut cur, &h, &opts);
    let decoded = a.depth != AnalysisDepth::Structural;
    if decoded && a.mcus_decoded == 0 {
        // The scan ends before a single MCU: random bytes with a plausible
        // header, or a header with nothing after it.
        let corrupt = a.first_error.is_some_and(|e| e.kind.is_corruption());
        if corrupt || h.thumbnails.is_empty() {
            return Ok(None);
        }
    }

    let mut meta = build_meta(&h);
    let mut nested = thumbnails_within(&h, u64::MAX);
    let mut notes: Vec<String> = h.issues.iter().map(|i| i.describe()).collect();
    notes.extend(h.notes.iter().cloned());
    notes.extend(a.notes.iter().take(16).cloned());
    let mut confidence;
    let (len, status, err_offset) = if let Some(eoi) = a.eoi_offset {
        let after = extend_after_eoi(&mut cur, &h, eoi + 2);
        nested.extend(after.nested.iter().cloned());
        notes.extend(after.notes.iter().cloned());
        if !after.kinds.is_empty() {
            meta.extra
                .insert("jpeg.trailer".into(), after.kinds.join(","));
        }
        let len = after.end;
        match a.first_error {
            None => {
                confidence = if decoded {
                    Confidence::HIGH
                } else {
                    Confidence::MEDIUM
                };
                if after.truncated {
                    let st = if capped_by_limit(len) {
                        ExtentStatus::Capped
                    } else {
                        ExtentStatus::Truncated
                    };
                    (len, st, Some(len))
                } else {
                    (len, ExtentStatus::Complete, None)
                }
            }
            Some(e) if e.kind.is_corruption() && a.corrupt_intervals > 0 => {
                // Restart markers let decoding resume: a complete file with
                // a damaged stretch.
                confidence = Confidence(65);
                (len, ExtentStatus::Complete, Some(e.byte_offset))
            }
            Some(e) => {
                confidence = Confidence(45);
                (len, ExtentStatus::Truncated, Some(e.byte_offset))
            }
        }
    } else {
        let len = a.data_end.max(h.scan_data_offset.unwrap_or(0)).min(avail);
        let err = a.first_error.map(|e| e.byte_offset).unwrap_or(len);
        confidence = if a.first_error.is_none() {
            notes.push("all MCUs decoded but the EOI marker is missing".into());
            Confidence::MEDIUM
        } else {
            Confidence(40)
        };
        let st = if capped_by_limit(len) {
            notes.push("stopped at the measurement limit".into());
            ExtentStatus::Capped
        } else {
            ExtentStatus::Truncated
        };
        (len, st, Some(err))
    };
    if !h.issues.is_empty() {
        confidence = confidence.lowered(10);
    }
    if let Some(off) = cur.read_error_offset() {
        notes.push(format!("unreadable data at file offset {off}"));
    }
    if src.overlaps_unreadable(start, len) {
        confidence = confidence.lowered(10);
        let bad = src
            .unreadable()
            .map(|s| {
                s.intersection_with_range(Range::with_len(start, len))
                    .total_len()
            })
            .unwrap_or(0);
        meta.extra
            .insert("unreadable_bytes".into(), bad.to_string());
        notes.push(format!("{bad} unreadable bytes inside the file"));
    }
    if probable_thumbnail(&h, len) {
        meta.extra
            .insert("jpeg.probable_thumbnail".into(), "true".into());
    }
    nested.retain(|n| n.range.end <= len);
    Ok(Some(Measured {
        len,
        status,
        confidence,
        err_offset,
        meta,
        nested,
        notes,
    }))
}
