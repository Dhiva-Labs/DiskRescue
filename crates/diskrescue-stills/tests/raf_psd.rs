//! Fujifilm RAF and Photoshop PSD: header-driven extents, embedded previews,
//! metadata, and the checks that reject or flag inconsistent headers.

mod common;

use common::raws::{self, PSD_CH, PSD_H, PSD_W};
use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, NestedRole, ValidationStatus};
use diskrescue_stills::{PsdFormat, RafFormat};

fn set32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

#[test]
fn raf_extent_is_the_largest_offset_plus_length_in_either_order() {
    for first in [true, false] {
        let s = raws::raf(first);
        let m = check_exact(&RafFormat, &s.bytes);
        assert_eq!(m.len, s.bytes.len() as u64);
        assert_eq!(m.meta.make.as_deref(), Some("FUJIFILM"));
        assert_eq!(m.meta.model.as_deref(), Some("X-T3"));
        assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
        assert_eq!(m.meta.orientation, Some(1));
        // Raw dimensions come from the CFA header record 0x0100.
        assert_eq!((m.meta.width, m.meta.height), (Some(6000), Some(4000)));
        assert_eq!(m.meta.extra["raf_version"], "0201");
        assert_eq!(m.meta.extra["raf_compression"], "lossless");
        assert_eq!(m.meta.extra["extension"], "raf");
        assert_eq!(m.nested.len(), 1);
        assert_eq!(m.nested[0].role, NestedRole::Preview);
        assert_eq!(m.nested[0].format.as_deref(), Some("jpeg"));
        let jpeg_off = u32::from_be_bytes(s.bytes[84..88].try_into().unwrap()) as u64;
        let jpeg_len = u32::from_be_bytes(s.bytes[88..92].try_into().unwrap()) as u64;
        assert_eq!(m.nested[0].range.start, jpeg_off);
        assert_eq!(m.nested[0].range.len(), jpeg_len);
        assert_eq!(
            s.bytes[jpeg_off as usize..jpeg_off as usize + 2],
            [0xFF, 0xD8]
        );
        if !first {
            assert_eq!(
                jpeg_off + jpeg_len,
                s.bytes.len() as u64,
                "JPEG is the last region"
            );
        }
        let cuts: Vec<usize> = (1..10)
            .map(|i| s.bytes.len() * i / 10)
            .chain([s.bytes.len() - 1, 120, 100])
            .collect();
        check_truncation(&RafFormat, &s.bytes, &cuts);
    }
}

#[test]
fn raf_with_a_real_pillow_jpeg() {
    let j = need_pillow!("jpeg_exif.jpg");
    for first in [true, false] {
        let s = raws::raf_with_jpeg(&j, first);
        let m = check_exact(&RafFormat, &s.bytes);
        assert_eq!(m.meta.make.as_deref(), Some("FUJIFILM"));
        assert_eq!(m.meta.model.as_deref(), Some("X-T3"));
        assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-05-06T07:08:09Z");
        assert_eq!(m.meta.orientation, Some(6));
        assert_eq!(m.meta.extra["preview_size"], "96x64");
    }
}

#[test]
fn raf_header_validation() {
    let good = raws::raf(true).bytes;
    // Offsets that point into the header, or reach past the cap, are rejected.
    for (at, v) in [
        (84usize, 100u32),
        (100, 50),
        (88, 0xFFFF_FFF0),
        (104, 0x4000_0000),
    ] {
        let mut b = good.clone();
        set32(&mut b, at, v);
        assert!(
            RafFormat
                .measure(&mem(b.clone()), 0, u64::MAX)
                .unwrap()
                .is_none(),
            "offset field {at}"
        );
        assert!(!RafFormat.probe(&b[..160]), "probe {at}");
    }
    // Non-numeric version.
    let mut b = good.clone();
    b[16] = b'x';
    assert!(RafFormat.measure(&mem(b), 0, u64::MAX).unwrap().is_none());
    // No JPEG and no raw data at all.
    let mut b = good.clone();
    for at in [88, 104] {
        set32(&mut b, at, 0);
    }
    assert!(RafFormat.measure(&mem(b), 0, u64::MAX).unwrap().is_none());
    // JPEG offset not at FF D8: still measured, with lower confidence.
    let mut b = good.clone();
    let jo = u32::from_be_bytes(good[84..88].try_into().unwrap()) as usize;
    b[jo] = 0;
    let m = measure(&RafFormat, &b);
    assert_eq!(m.len, good.len() as u64);
    assert!(m.confidence.value() < measure(&RafFormat, &good).confidence.value());
    assert!(m.nested.is_empty());
    assert!(m.notes.iter().any(|n| n.contains("FF D8")));
    // Truncated inside the 108-byte header: not enough to judge.
    assert!(RafFormat
        .measure(&mem(good[..100].to_vec()), 0, u64::MAX)
        .unwrap()
        .is_none());
    assert!(RafFormat.probe(&good[..100]));
}

#[test]
fn raf_second_quadruple_for_dual_shot_bodies() {
    let good = raws::raf(true).bytes;
    // Append a second CFA region and describe it at 120..136.
    let mut b = good.clone();
    let extra_off = b.len();
    b.extend(vec![0x77u8; 50_000]);
    set32(&mut b, 120, 160); // second header offset (re-uses the first header)
    set32(&mut b, 124, 16);
    set32(&mut b, 128, extra_off as u32);
    set32(&mut b, 132, 50_000);
    let m = measure(&RafFormat, &b);
    assert_eq!(m.len, b.len() as u64);
    // If it is not fully present the extra set is ignored.
    let cut = b.len() - 100;
    let m = measure(&RafFormat, &b[..cut]);
    assert_eq!(m.len, good.len() as u64);
    // A second set that starts before the first CFA is ignored.
    let mut c = good.clone();
    set32(&mut c, 120, 160);
    set32(&mut c, 124, 16);
    set32(&mut c, 128, 200);
    set32(&mut c, 132, 100);
    assert_eq!(measure(&RafFormat, &c).len, good.len() as u64);
}

#[test]
fn psd_raw_rle_zip_have_exact_extents() {
    for (comp, name) in [(0u16, "raw"), (1, "rle"), (2, "zip")] {
        let s = raws::psd(comp, comp != 0);
        let m = check_exact(&PsdFormat, &s.bytes);
        assert_eq!((m.meta.width, m.meta.height), (Some(PSD_W), Some(PSD_H)));
        assert_eq!(m.meta.extra["channels"], PSD_CH.to_string());
        assert_eq!(m.meta.extra["compression"], name);
        assert_eq!(m.meta.extra["color_mode"], "rgb");
        let cuts: Vec<usize> = (1..9)
            .map(|i| s.bytes.len() * i / 9)
            .chain([s.bytes.len() - 1, 30, 27])
            .collect();
        check_truncation(&PsdFormat, &s.bytes, &cuts);
    }
}

#[test]
fn psd_resources_give_thumbnail_and_exif() {
    let s = raws::psd(1, true);
    let m = measure(&PsdFormat, &s.bytes);
    assert_eq!(m.meta.make.as_deref(), Some("Adobe"));
    assert_eq!(m.meta.model.as_deref(), Some("Photoshop Test"));
    assert_eq!(m.meta.orientation, Some(3));
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
    assert_eq!(m.nested.len(), 1);
    let t = &m.nested[0];
    assert_eq!(t.role, NestedRole::Thumbnail);
    assert_eq!(t.range.len(), 1_200);
    assert_eq!(
        s.bytes[t.range.start as usize..t.range.start as usize + 3],
        [0xFF, 0xD8, 0xFF]
    );
    assert!(PsdFormat.may_contain("jpeg"));
}

#[test]
fn psd_inconsistencies_are_reported_at_the_right_place() {
    let s = raws::psd(1, false);
    // Unknown compression method.
    let comp_at = s.bytes.len()
        - (PSD_CH as usize * PSD_H as usize * 2)
        - (0..PSD_CH as usize * PSD_H as usize)
            .map(|i| 4 + (i % 14))
            .sum::<usize>()
        - 2;
    let mut b = s.bytes.clone();
    b[comp_at + 1] = 9;
    let m = measure(&PsdFormat, &b);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.err_offset, Some(comp_at as u64));
    // Absurd layer-and-mask length.
    let lm_at = 26 + 4 + 4;
    let mut b = s.bytes.clone();
    set32(&mut b, lm_at, 0xFFFF_FFF0);
    let m = measure(&PsdFormat, &b);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.err_offset, Some(lm_at as u64));
    // Header range checks.
    for (at, v) in [(13usize, 0u8), (23, 7), (25, 11), (7, 1)] {
        let mut b = s.bytes.clone();
        b[at] = v;
        assert!(
            PsdFormat.measure(&mem(b), 0, u64::MAX).unwrap().is_none(),
            "byte {at}"
        );
    }
    // Raw image data too large for the format: flagged, no overflow.
    let mut huge = raws::psd(0, false).bytes;
    huge[12..14].copy_from_slice(&56u16.to_be_bytes());
    set32(&mut huge, 14, 30_000);
    set32(&mut huge, 18, 30_000);
    huge[22..24].copy_from_slice(&32u16.to_be_bytes());
    let m = measure(&PsdFormat, &huge);
    assert_eq!(m.status, ExtentStatus::Truncated);
    let v = PsdFormat.validate(&mem(huge)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn psd_is_capped_by_the_limit() {
    let s = raws::psd(0, true);
    let mut v = s.bytes.clone();
    v.extend(vec![0u8; 5_000]);
    let m = PsdFormat.measure(&mem(v), 0, 100).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Capped);
    assert_eq!(m.len, 100);
}
