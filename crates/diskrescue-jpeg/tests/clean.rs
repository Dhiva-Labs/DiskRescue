//! Clean files: exact extents, complete scan analysis, validation,
//! metadata, nested thumbnails/MPF/trailers and back-to-back data.

mod common;

use common::*;
use diskrescue_jpeg::core::format::{
    Confidence, ExtentStatus, FileFormat, NestedRole, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_jpeg::scan::AnalysisDepth;
use diskrescue_jpeg::*;

/// `(width, height)` from a fixture name such as `base_420_640x480.jpg`.
fn name_dims(name: &str) -> Option<(u32, u32)> {
    let stem = name.trim_end_matches(".jpg");
    stem.split('_').find_map(|tok| {
        let (w, h) = tok.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    })
}

fn all_clean() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = COMMITTED
        .iter()
        .map(|n| (n.to_string(), committed(n)))
        .collect();
    if let Some(dir) = fixtures() {
        for n in jpeg_names(dir) {
            let d = read(dir, &n);
            v.push((n, d));
        }
    }
    v
}

#[test]
fn measure_length_equals_file_length() {
    let f = JpegFormat::new();
    for (n, d) in all_clean() {
        let len = d.len() as u64;
        let m = f
            .measure(&mem(d), 0, u64::MAX)
            .unwrap()
            .unwrap_or_else(|| panic!("{n}: rejected"));
        assert_eq!(m.len, len, "{n}: {:?}", m.notes);
        assert_eq!(m.status, ExtentStatus::Complete, "{n}");
        assert!(m.confidence >= Confidence::HIGH, "{n}: {:?}", m.confidence);
        assert_eq!(m.err_offset, None, "{n}");
        for nested in &m.nested {
            assert!(nested.range.end <= len, "{n}: nested past the end");
        }
    }
}

#[test]
fn analyzer_decodes_every_clean_file_completely() {
    for (n, d) in all_clean() {
        let a = analyze(&d).unwrap();
        assert!(a.is_clean(), "{n}: {:?} {:?}", a.first_error, a.notes);
        assert_eq!(a.depth, AnalysisDepth::Full, "{n}");
        assert_eq!(a.mcus_decoded, a.mcus_total, "{n}");
        assert_eq!(a.mcu_rows_decoded, a.mcu_rows_total, "{n}");
        assert!(a.notes.is_empty(), "{n}: {:?}", a.notes);
        let h = walk_headers(&*mem(d.clone()), 0, d.len() as u64).unwrap();
        let sof = h.sof.as_ref().unwrap();
        let expected_rows = (sof.height as u32).div_ceil(8 * sof.vmax() as u32);
        assert_eq!(a.mcu_rows_total, expected_rows, "{n}");
        if let Some((w, hh)) = name_dims(&n) {
            assert_eq!((sof.width as u32, sof.height as u32), (w, hh), "{n}");
        }
        let restarts = rst_offsets(&d[..a.eoi_offset.unwrap() as usize]).len() as u32;
        assert_eq!(a.restart_intervals_seen, restarts, "{n}");
    }
}

#[test]
fn validate_accepts_every_clean_file() {
    let f = JpegFormat::new();
    for (n, d) in all_clean() {
        let v = f.validate(&mem(d.clone())).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{n}: {:?}", v.problems);
        assert_eq!(v.intact_fraction, Some(1.0), "{n}");
        assert!(v.problems.is_empty(), "{n}: {:?}", v.problems);
        assert!(v.repairable_by.is_empty(), "{n}: {:?}", v.repairable_by);
        let z = zune_check(&d);
        // zune-jpeg 0.4 cannot decode progressive files with restart
        // markers (libjpeg can); everything else must decode strictly.
        if n.contains("prog_rst") {
            continue;
        }
        assert!(
            z.strict_ok,
            "{n}: zune-jpeg rejects the fixture: {:?}",
            z.error
        );
        assert_eq!(Some(z.width), v.meta.width, "{n}");
        assert_eq!(Some(z.height), v.meta.height, "{n}");
    }
}

#[test]
fn repair_of_a_clean_file_changes_nothing() {
    let f = JpegFormat::new();
    for (n, d) in all_clean().into_iter().take(12) {
        let mut out = Vec::new();
        let r = f
            .repair(&mem(d.clone()), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::NoRepairNeeded, "{n}: {r:?}");
        assert_eq!(out, d, "{n}");
        assert_eq!(r.output_len, d.len() as u64);
    }
}

#[test]
fn exif_metadata_drives_meta() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    for n in ["base_420_640x480.jpg", "prog_640x480.jpg"] {
        let m = f.measure(&mem(read(dir, n)), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.meta.make.as_deref(), Some("DiskRescue"), "{n}");
        assert_eq!(m.meta.model.as_deref(), Some("FixtureCam"), "{n}");
        assert_eq!(m.meta.orientation, Some(6), "{n}");
        assert_eq!(
            m.meta.created.unwrap().to_rfc3339(),
            "2024-05-06T07:08:09Z",
            "{n}"
        );
        assert_eq!(m.meta.width, Some(640));
        assert_eq!(m.meta.height, Some(480));
    }
    let m = f
        .measure(&mem(read(dir, "base_420_640x480.jpg")), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        m.meta.extra.get("jpeg.subsampling").map(String::as_str),
        Some("4:2:0")
    );
    assert_eq!(
        m.meta.extra.get("jpeg.process").map(String::as_str),
        Some("baseline")
    );
    let m = f
        .measure(&mem(read(dir, "prog_444_320x240.jpg")), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        m.meta.extra.get("jpeg.subsampling").map(String::as_str),
        Some("4:4:4")
    );
    assert_eq!(
        m.meta.extra.get("jpeg.process").map(String::as_str),
        Some("progressive")
    );
    let h = walk_headers(&*mem(read(dir, "thumb_exif_1024x768.jpg")), 0, u64::MAX).unwrap();
    let exif = h.exif.unwrap();
    assert_eq!(exif.created().unwrap().to_rfc3339(), "2024-07-14T10:22:31Z");
    assert_eq!(exif.model.as_deref(), Some("ThumbCam"));
}

/// The Exif thumbnail is one nested object, and measuring at its own SOI
/// gives exactly its length.
fn check_thumbnail(name: &str, d: &[u8]) {
    let f = JpegFormat::new();
    let m = f.measure(&mem(d.to_vec()), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, d.len() as u64, "{name}");
    let thumbs: Vec<_> = m
        .nested
        .iter()
        .filter(|n| n.role == NestedRole::Thumbnail)
        .collect();
    assert_eq!(thumbs.len(), 1, "{name}: {:?}", m.nested);
    assert_eq!(m.nested.len(), 1, "{name}");
    let r = thumbs[0].range;
    assert_eq!(thumbs[0].format.as_deref(), Some("jpeg"));
    assert_eq!(
        &d[r.start as usize..r.start as usize + 3],
        &[0xFF, 0xD8, 0xFF]
    );
    assert_eq!(&d[r.end as usize - 2..r.end as usize], &[0xFF, 0xD9]);
    let inner = f
        .measure(&mem(d.to_vec()), r.start, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(inner.len, r.len(), "{name}: thumbnail measured on its own");
    assert_eq!(inner.status, ExtentStatus::Complete);
    assert!(f.may_contain("jpeg"));
}

#[test]
fn exif_thumbnail_is_nested_not_the_end() {
    check_thumbnail("small_exif_thumb.jpg", &committed("small_exif_thumb.jpg"));
    if let Some(dir) = fixtures() {
        check_thumbnail(
            "thumb_exif_1024x768.jpg",
            &read(dir, "thumb_exif_1024x768.jpg"),
        );
    }
}

#[test]
fn jpeg_embedded_in_other_data_is_one_file_plus_one_nested() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let jpg = read(dir, "thumb_exif_1024x768.jpg");
    let mut rng = Rng(77);
    let mut buf = rng.bytes(777);
    let start = buf.len() as u64;
    buf.extend_from_slice(&jpg);
    buf.extend_from_slice(&rng.bytes(5000));
    let src = mem(buf);
    let m = f.measure(&src, start, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, jpg.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.nested.len(), 1);
    assert_eq!(m.nested[0].role, NestedRole::Thumbnail);
    // Ranges are relative to the enclosing file.
    let r = m.nested[0].range;
    assert_eq!(&jpg[r.start as usize..r.start as usize + 2], &[0xFF, 0xD8]);
}

#[test]
fn trailing_data_and_following_jpegs_are_not_absorbed() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let other = read(dir, "tiny_64x48.jpg");
    let mut rng = Rng(5);
    for n in jpeg_names(dir) {
        if n.starts_with("mpf") || n.starts_with("motion") {
            continue;
        }
        let d = read(dir, &n);
        for tail in [
            rng.bytes(4096),
            other.clone(),
            vec![0u8; 3000],
            vec![0xFF; 3000],
        ] {
            let mut buf = d.clone();
            buf.extend_from_slice(&tail);
            let m = f.measure(&mem(buf), 0, u64::MAX).unwrap().unwrap();
            assert_eq!(m.len, d.len() as u64, "{n} + {} bytes", tail.len());
            assert_eq!(m.status, ExtentStatus::Complete, "{n}");
        }
    }
}

#[test]
fn mpf_images_extend_the_file() {
    let mut cases = vec![("small_mpf.jpg".to_string(), committed("small_mpf.jpg"))];
    if let Some(dir) = fixtures() {
        cases.push(("mpf2.jpg".into(), read(dir, "mpf2.jpg")));
    }
    let f = JpegFormat::new();
    for (n, d) in cases {
        let a = analyze(&d).unwrap();
        let main_end = a.eoi_offset.unwrap() + 2;
        assert!(main_end < d.len() as u64);
        let mut buf = d.clone();
        buf.extend_from_slice(&Rng(9).bytes(2048));
        let m = f.measure(&mem(buf), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, d.len() as u64, "{n}: {:?}", m.notes);
        let second: Vec<_> = m
            .nested
            .iter()
            .filter(|x| x.role == NestedRole::Preview)
            .collect();
        assert_eq!(second.len(), 1, "{n}: {:?}", m.nested);
        assert_eq!(second[0].range.start, main_end);
        assert_eq!(second[0].range.end, d.len() as u64);
        assert_eq!(
            m.meta.extra.get("jpeg.trailer").map(String::as_str),
            Some("mpf")
        );
        let v = f.validate(&mem(d.clone())).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{n}: {:?}", v.problems);
        assert!(v.problems.is_empty(), "{n}: {:?}", v.problems);
    }
}

#[test]
fn motion_photo_trailers_are_included() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    for n in ["motion_google.jpg", "motion_samsung.jpg"] {
        let d = read(dir, n);
        let eoi_end = analyze(&d).unwrap().eoi_offset.unwrap() + 2;
        let mut buf = d.clone();
        buf.extend_from_slice(&Rng(3).bytes(4096));
        let m = f.measure(&mem(buf), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, d.len() as u64, "{n}: {:?}", m.notes);
        assert_eq!(m.status, ExtentStatus::Complete);
        let mp4: Vec<_> = m
            .nested
            .iter()
            .filter(|x| x.role == NestedRole::Trailer && x.format.as_deref() == Some("mp4"))
            .collect();
        assert_eq!(mp4.len(), 1, "{n}: {:?}", m.nested);
        assert!(mp4[0].range.start >= eoi_end);
        assert_eq!(
            &d[mp4[0].range.start as usize + 4..mp4[0].range.start as usize + 8],
            b"ftyp"
        );
        let v = f.validate(&mem(d)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{n}: {:?}", v.problems);
        assert!(v.problems.is_empty(), "{n}: {:?}", v.problems);
    }
    // Without the XMP hint an MP4 right after the EOI is the next file.
    let jpg = read(dir, "tiny_64x48.jpg");
    let mut buf = jpg.clone();
    buf.extend_from_slice(&read(dir, "fake.mp4"));
    let m = f.measure(&mem(buf), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, jpg.len() as u64);
}

#[test]
fn measure_respects_the_limit() {
    let Some(dir) = fixtures() else { return };
    let f = JpegFormat::new();
    let d = read(dir, "photo_1024x768.jpg");
    let len = d.len() as u64;
    let src = mem(d);
    let m = f.measure(&src, 0, len / 2).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Capped, "{:?}", m.notes);
    assert!(m.len <= len / 2);
    let m = f.measure(&src, 0, len).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Complete);
    let m = f.measure(&src, 0, 1000).unwrap().unwrap();
    assert!(m.len <= 1000);
    assert_eq!(m.status, ExtentStatus::Capped);
    assert_eq!(f.max_size(), 128 << 20);
    // The unit struct is usable directly.
    let g = JpegFormat;
    assert_eq!(g.id(), "jpeg");
}

#[test]
fn decode_budget_samples_then_walks_to_the_end() {
    let Some(dir) = fixtures() else { return };
    for n in [
        "noise_1024x768.jpg",
        "prog_noise_1024x768.jpg",
        "noise_rst_1024x768.jpg",
    ] {
        let d = read(dir, n);
        let opts = AnalyzeOptions {
            max_decode_bytes: Some(50_000),
            ..Default::default()
        };
        let (_, a) = analyze_source(&*mem(d.clone()), 0, u64::MAX, &opts).unwrap();
        assert_eq!(a.depth, AnalysisDepth::Sampled, "{n}");
        assert!(a.first_error.is_none(), "{n}: {:?}", a.first_error);
        assert_eq!(a.eoi_offset, Some(d.len() as u64 - 2), "{n}");
        assert!(a.mcus_decoded < a.mcus_total, "{n}");
        // Damage beyond the sampled part is not seen, but the extent is.
        let mut x = d.clone();
        let at = d.len() * 3 / 4;
        x[at..at + 64].fill(0x5A);
        let (_, b) = analyze_source(&*mem(x), 0, u64::MAX, &opts).unwrap();
        assert_eq!(b.eoi_offset, a.eoi_offset, "{n}");
        // Scoring mode stops outright.
        let opts = AnalyzeOptions {
            max_mcus: Some(10),
            ..Default::default()
        };
        let (_, c) = analyze_source(&*mem(d), 0, u64::MAX, &opts).unwrap();
        assert_eq!(c.mcus_decoded, 10, "{n}");
        assert!(c.eoi_offset.is_none());
    }
}
