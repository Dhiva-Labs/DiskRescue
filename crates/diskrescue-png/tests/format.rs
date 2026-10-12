//! `PngFormat` through the `FileFormat` trait: probe, measure, validate.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, FormatRegistry, ValidationStatus,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_png::PngFormat;

fn small_specs() -> Vec<Spec> {
    let mut v = Vec::new();
    for (ct, depth) in all_modes() {
        for interlaced in [false, true] {
            for (w, h) in [(1, 1), (2, 3), (17, 13), (100, 75)] {
                let mut s = Spec::new(w, h, ct, depth);
                s.interlaced = interlaced;
                s.idat_chunk = 512;
                v.push(s);
            }
        }
    }
    v
}

#[test]
fn trait_constants() {
    let f = PngFormat;
    assert_eq!(f.id(), "png");
    assert_eq!(f.extensions(), &["png"]);
    assert_eq!(f.category(), MediaCategory::Photo);
    assert_eq!(f.min_size(), 67);
    assert_eq!(f.max_size(), 256 << 20);
    assert!(f.can_repair());
    assert_eq!(
        f.signatures()[0].bytes,
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    );
    assert_eq!(f.signatures()[0].offset, 0);
    let reg = FormatRegistry::new().with(Arc::new(PngFormat));
    let b = build(&Spec::new(4, 4, 2, 8));
    let m = reg.matching(&b.png);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].id(), "png");
}

#[test]
fn generator_agrees_with_the_png_crate() {
    // Guards the test generator itself: every mode, interlaced or not.
    for spec in small_specs() {
        let b = build(&spec);
        let d = png_decode(&b.png).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
        assert_eq!((d.width, d.height), (spec.width, spec.height));
        assert_eq!(d.data, b.raw, "generator mismatch for {spec:?}");
    }
}

#[test]
fn smallest_png_is_67_bytes() {
    // The smallest valid PNG: 1x1 grey 1-bit.
    let mut s = Spec::new(1, 1, 0, 1);
    s.text = false;
    s.filter = Some(0);
    let b = build(&s);
    assert!(
        b.png.len() >= 67 - 4 && b.png.len() <= 80,
        "{}",
        b.png.len()
    );
    assert_eq!(png_decode(&b.png).unwrap().data.len(), 1);
}

#[test]
fn every_mode_measures_validates_and_probes() {
    let f = PngFormat;
    for spec in small_specs() {
        let b = build(&spec);
        let src = mem(b.png.clone());
        assert!(f.probe(&b.png[..b.png.len().min(65536)]), "{spec:?}");
        let m = f.measure(&src, 0, u64::MAX).unwrap().expect("measure");
        assert_eq!(m.len, b.png.len() as u64, "{spec:?}");
        assert_eq!(m.status, ExtentStatus::Complete);
        assert!(m.confidence >= Confidence::HIGH, "{:?} {:?}", m, spec);
        assert_eq!(m.meta.width, Some(spec.width));
        assert_eq!(m.meta.height, Some(spec.height));
        assert!(m.nested.is_empty());
        let v = f.validate(&src).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Valid,
            "{spec:?}: {:?}",
            v.problems
        );
        assert_eq!(v.intact_fraction, Some(1.0));
        assert!(v.problems.is_empty());
        assert!(v.repairable_by.is_empty());
    }
}

#[test]
fn larger_images_validate() {
    let f = PngFormat;
    for (ct, depth, interlaced) in [
        (2u8, 8u8, false),
        (6, 16, true),
        (3, 4, true),
        (0, 1, false),
    ] {
        let mut s = Spec::new(800, 600, ct, depth);
        s.interlaced = interlaced;
        s.idat_chunk = 32 * 1024;
        let b = build(&s);
        let src = mem(b.png.clone());
        let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, b.png.len() as u64);
        let v = f.validate(&src).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
        assert_eq!(png_decode(&b.png).unwrap().data, b.raw);
    }
}

#[test]
fn measure_stops_exactly_at_iend() {
    let f = PngFormat;
    let b = build(&Spec::new(50, 40, 2, 8));
    let n = b.png.len();
    let mut rng = Rng::new(3);
    // Followed by garbage, zeros, 0xFF, text.
    for tail in [
        rng.bytes(1 << 20),
        vec![0u8; 5000],
        vec![0xFF; 777],
        b"hello".to_vec(),
    ] {
        let mut data = b.png.clone();
        data.extend_from_slice(&tail);
        let src = mem(data);
        let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, n as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
    }
    // Followed by another PNG.
    let b2 = build(&Spec::new(30, 30, 0, 8).seed(9));
    let mut data = b.png.clone();
    data.extend_from_slice(&b2.png);
    let src = mem(data);
    assert_eq!(f.measure(&src, 0, u64::MAX).unwrap().unwrap().len, n as u64);
    assert_eq!(
        f.measure(&src, n as u64, u64::MAX).unwrap().unwrap().len,
        b2.png.len() as u64
    );
}

#[test]
fn measure_at_an_offset_with_a_limit() {
    let f = PngFormat;
    let b = build(&Spec::new(64, 64, 2, 8).chunk(1024));
    let mut data = vec![0xA5u8; 4096];
    data.extend_from_slice(&b.png);
    data.extend_from_slice(&[0x5A; 4096]);
    let src = mem(data);
    let m = f.measure(&src, 4096, 1 << 20).unwrap().unwrap();
    assert_eq!(m.len, b.png.len() as u64);
    // A limit below the file size caps the measurement.
    let m = f.measure(&src, 4096, 1000).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Capped);
    assert!(m.len <= 1000);
    // Start beyond the end.
    assert!(f.measure(&src, 1 << 30, u64::MAX).unwrap().is_none());
}

#[test]
fn truncated_files_measure_as_truncated() {
    let f = PngFormat;
    let b = build(&Spec::new(120, 90, 2, 8).chunk(2048));
    let n = b.png.len();
    let mut rng = Rng::new(11);
    let mut cuts: Vec<usize> = (0..10)
        .map(|_| 40 + rng.below((n - 41) as u64) as usize)
        .collect();
    cuts.extend([n - 1, n - 12, n - 13, n / 2, 34, 41, 100]);
    for k in cuts {
        let src = mem(b.png[..k].to_vec());
        let m = f.measure(&src, 0, u64::MAX).unwrap();
        let m = m.unwrap_or_else(|| panic!("cut at {k} gave None"));
        assert_eq!(m.status, ExtentStatus::Truncated, "cut {k}");
        assert!(m.len <= k as u64, "cut {k}: len {}", m.len);
        assert!(m.err_offset.unwrap() <= k as u64);
        assert!(m.confidence < Confidence::HIGH);
        // The cut file has at least what was readable.
        assert!(m.len + 64 * 1024 >= k as u64);
    }
}

#[test]
fn truncated_file_followed_by_other_data_is_not_complete() {
    // The first image is cut and something else is written after it.
    let f = PngFormat;
    let a = build(&Spec::new(120, 90, 2, 8).chunk(2048));
    let other = build(&Spec::new(60, 60, 0, 8).seed(2));
    for fraction in [0.25, 0.5, 0.75] {
        let cut = (a.png.len() as f64 * fraction) as usize;
        let mut data = a.png[..cut].to_vec();
        data.extend_from_slice(&other.png);
        let src = mem(data);
        let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_ne!(m.status, ExtentStatus::Complete, "fraction {fraction}");
        assert!(m.len <= (cut + 2048 + 12) as u64, "len {}", m.len);
    }
    // Random data after the cut.
    let mut rng = Rng::new(5);
    let mut data = a.png[..a.png.len() / 2].to_vec();
    data.extend(rng.bytes(200_000));
    let m = f.measure(&mem(data), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
}

#[test]
fn damaged_but_complete_files_lose_confidence() {
    let f = PngFormat;
    let b = build(&Spec::new(80, 60, 2, 8).chunk(1024));
    // Bad CRC in an IDAT.
    let mut bad = b.png.clone();
    let c = find(&bad, b"IDAT", 2);
    bad[c.data().start + 5] ^= 0xFF;
    let m = f.measure(&mem(bad), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.len, b.png.len() as u64);
    assert!(m.confidence < Confidence::HIGH);
    assert!(m.notes.iter().any(|n| n.contains("CRC")));
    // Bad CRC in an ancillary chunk only: still HIGH-ish.
    let mut anc = b.png.clone();
    let c = find(&anc, b"tEXt", 0);
    anc[c.data().start] ^= 0x20;
    let m = f.measure(&mem(anc), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.confidence >= Confidence(80));
}

#[test]
fn false_positives_are_rejected() {
    let f = PngFormat;
    let mut rng = Rng::new(77);
    // Signature only.
    assert!(f
        .measure(&mem(SIG.to_vec()), 0, u64::MAX)
        .unwrap()
        .is_none());
    assert!(!f.probe(&SIG));
    // Signature + garbage.
    let mut data = SIG.to_vec();
    data.extend(rng.bytes(4096));
    assert!(f
        .measure(&mem(data.clone()), 0, u64::MAX)
        .unwrap()
        .is_none());
    assert!(!f.probe(&data));
    // Pure random.
    for n in [0usize, 1, 7, 8, 9, 33, 100, 5000] {
        let d = rng.bytes(n);
        assert!(f.measure(&mem(d.clone()), 0, u64::MAX).unwrap().is_none());
        assert!(!f.probe(&d));
    }
    // IHDR with an impossible combination.
    let b = build(&Spec::new(10, 10, 2, 8));
    for (off, val) in [
        (8 + 8 + 8, 5u8),
        (8 + 8 + 9, 9),
        (8 + 8 + 10, 1),
        (8 + 8 + 11, 1),
        (8 + 8 + 12, 2),
    ] {
        let mut d = b.png.clone();
        d[off] = val;
        assert!(!f.probe(&d), "off {off}");
        assert!(
            f.measure(&mem(d), 0, u64::MAX).unwrap().is_none(),
            "off {off}"
        );
    }
    // Zero and huge dimensions.
    for dims in [(0u32, 10u32), (10, 0), (70_000, 10), (10, 70_000)] {
        let mut d = b.png.clone();
        d[16..20].copy_from_slice(&dims.0.to_be_bytes());
        d[20..24].copy_from_slice(&dims.1.to_be_bytes());
        assert!(!f.probe(&d), "{dims:?}");
    }
    // Wrong first chunk.
    let mut d = b.png.clone();
    d[12..16].copy_from_slice(b"IDAT");
    assert!(!f.probe(&d));
    // Exactly 65535 is fine.
    let mut d = b.png.clone();
    d[16..20].copy_from_slice(&65_535u32.to_be_bytes());
    assert!(f.probe(&d));
}

#[test]
fn metadata_from_text_and_exif() {
    let f = PngFormat;
    // tEXt Creation Time (RFC 1123) + Software.
    let spec = Spec::new(8, 8, 2, 8);
    let b = build(&spec);
    let z = {
        let raw = gen_raw(&spec);
        zlib(&filtered_stream(&spec, &raw), 6)
    };
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"tEXt", b"Software\0MyTool 1.2"));
    png.extend(chunk(
        b"tEXt",
        b"Creation Time\0Tue, 07 Nov 2023 08:12:31 GMT",
    ));
    png.extend(chunk(b"tIME", &[0x07, 0xE7, 11, 8, 9, 10, 11]));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    assert!(png_decode(&png).is_ok());
    let m = f.measure(&mem(png.clone()), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-11-07T08:12:31Z");
    assert_eq!(m.meta.software.as_deref(), Some("MyTool 1.2"));
    assert_eq!(
        m.meta.extra.get("modified").map(String::as_str),
        Some("2023-11-08T09:10:11Z")
    );
    let v = f.validate(&mem(png)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid);
    assert_eq!(v.meta.created.unwrap().to_rfc3339(), "2023-11-07T08:12:31Z");
    let _ = b;
}

#[test]
fn exif_chunk_gives_date_make_model_orientation() {
    let f = PngFormat;
    let spec = Spec::new(8, 8, 2, 8);
    let raw = gen_raw(&spec);
    let z = zlib(&filtered_stream(&spec, &raw), 6);
    // Little-endian TIFF with IFD0 {Make, Model, Orientation, ExifIFD} and
    // an Exif IFD {DateTimeOriginal}.
    let make = b"DiskRescue\0";
    let model = b"FixtureCam\0";
    let date = b"2024:05:06 07:08:09\0";
    let ifd0_entries = 4u32;
    let ifd0_size = 2 + ifd0_entries * 12 + 4;
    let make_off = 8 + ifd0_size;
    let model_off = make_off + make.len() as u32;
    let sub_off = model_off + model.len() as u32;
    let date_off = sub_off + 2 + 12 + 4;
    let mut t = Vec::new();
    t.extend_from_slice(b"II");
    t.extend_from_slice(&42u16.to_le_bytes());
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&(ifd0_entries as u16).to_le_bytes());
    let mut entry = |tag: u16, kind: u16, count: u32, val: [u8; 4]| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&kind.to_le_bytes());
        t.extend_from_slice(&count.to_le_bytes());
        t.extend_from_slice(&val);
    };
    entry(0x010F, 2, make.len() as u32, make_off.to_le_bytes());
    entry(0x0110, 2, model.len() as u32, model_off.to_le_bytes());
    entry(0x0112, 3, 1, [6, 0, 0, 0]);
    entry(0x8769, 4, 1, sub_off.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(make);
    t.extend_from_slice(model);
    t.extend_from_slice(&1u16.to_le_bytes());
    t.extend_from_slice(&0x9003u16.to_le_bytes());
    t.extend_from_slice(&2u16.to_le_bytes());
    t.extend_from_slice(&(date.len() as u32).to_le_bytes());
    t.extend_from_slice(&date_off.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(date);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"eXIf", &t));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let m = f.measure(&mem(png), 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-05-06T07:08:09Z");
    assert_eq!(m.meta.make.as_deref(), Some("DiskRescue"));
    assert_eq!(m.meta.model.as_deref(), Some("FixtureCam"));
    assert_eq!(m.meta.orientation, Some(6));
}

#[test]
fn validate_reports_damage_with_fractions() {
    let f = PngFormat;
    let b = build(&Spec::new(100, 80, 2, 8).chunk(1500));
    // Truncated: ~50% of the rows.
    let cut = b.png.len() / 2;
    let v = f.validate(&mem(b.png[..cut].to_vec())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    let frac = v.intact_fraction.unwrap();
    assert!((0.3..0.7).contains(&frac), "{frac}");
    assert!(v.repairable_by.contains(&"truncated_idat".to_string()));
    assert!(!v.problems.is_empty());
    // Bad IDAT CRC only: pixel data still decodes, so fraction is 1.0.
    let mut bad = b.png.clone();
    let c = find(&bad, b"IDAT", 1);
    bad[c.crc_at()] ^= 1;
    let v = f.validate(&mem(bad)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert_eq!(v.intact_fraction, Some(1.0));
    assert_eq!(v.repairable_by, vec!["fix_crc".to_string()]);
    // Missing IEND.
    let v = f
        .validate(&mem(b.png[..b.png.len() - 12].to_vec()))
        .unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert_eq!(v.intact_fraction, Some(1.0));
    assert_eq!(v.repairable_by, vec!["reconstruct_iend".to_string()]);
    // Not a PNG at all.
    let v = f
        .validate(&mem(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))
        .unwrap();
    assert_eq!(v.status, ValidationStatus::Invalid);
    let v = f.validate(&mem(SIG.to_vec())).unwrap();
    assert_eq!(v.status, ValidationStatus::Invalid);
    let v = f.validate(&mem(Vec::new())).unwrap();
    assert_eq!(v.status, ValidationStatus::Invalid);
}

#[test]
fn trailing_bytes_after_iend_do_not_invalidate() {
    let f = PngFormat;
    let b = build(&Spec::new(20, 20, 6, 8));
    let mut d = b.png.clone();
    d.extend_from_slice(&[0u8; 100]);
    let v = f.validate(&mem(d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid);
    assert_eq!(
        v.meta.extra.get("trailing_bytes").map(String::as_str),
        Some("100")
    );
}

#[test]
fn apng_is_a_normal_png() {
    // acTL/fcTL/fdAT chunks around a default image: nothing nested, still
    // valid, frame count reported.
    let f = PngFormat;
    let spec = Spec::new(16, 16, 6, 8);
    let raw = gen_raw(&spec);
    let z = zlib(&filtered_stream(&spec, &raw), 6);
    let mut fctl = Vec::new();
    fctl.extend_from_slice(&0u32.to_be_bytes());
    fctl.extend_from_slice(&16u32.to_be_bytes());
    fctl.extend_from_slice(&16u32.to_be_bytes());
    fctl.extend_from_slice(&[0; 8]);
    fctl.extend_from_slice(&1u16.to_be_bytes());
    fctl.extend_from_slice(&10u16.to_be_bytes());
    fctl.extend_from_slice(&[0, 0]);
    let mut actl = Vec::new();
    actl.extend_from_slice(&2u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    let mut fdat = 2u32.to_be_bytes().to_vec();
    fdat.extend_from_slice(&z);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"acTL", &actl));
    png.extend(chunk(b"fcTL", &fctl));
    png.extend(chunk(b"IDAT", &z));
    let mut fctl2 = fctl.clone();
    fctl2[..4].copy_from_slice(&1u32.to_be_bytes());
    png.extend(chunk(b"fcTL", &fctl2));
    png.extend(chunk(b"fdAT", &fdat));
    png.extend(chunk(b"IEND", &[]));
    let src = mem(png.clone());
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, png.len() as u64);
    assert!(m.nested.is_empty());
    assert_eq!(
        m.meta.extra.get("apng_frames").map(String::as_str),
        Some("2")
    );
    assert_eq!(f.validate(&src).unwrap().status, ValidationStatus::Valid);
}

#[test]
fn cgbi_files_are_walked_and_inflated_raw() {
    // Apple CgBI: CgBI chunk first, raw deflate (no zlib wrapper).
    let f = PngFormat;
    let spec = Spec::new(24, 20, 6, 8);
    let raw = gen_raw(&spec);
    let filtered = filtered_stream(&spec, &raw);
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    std::io::Write::write_all(&mut e, &filtered).unwrap();
    let deflate = e.finish().unwrap();
    let mut png = SIG.to_vec();
    png.extend(chunk(b"CgBI", &[0x50, 0x00, 0x20, 0x06]));
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &deflate));
    png.extend(chunk(b"IEND", &[]));
    let src = mem(png.clone());
    assert!(f.probe(&png));
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, png.len() as u64);
    assert_eq!(
        m.meta.extra.get("variant").map(String::as_str),
        Some("cgbi")
    );
    let v = f.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn registry_style_use_through_a_trait_object() {
    let f: Arc<dyn FileFormat> = Arc::new(PngFormat);
    let b = build(&Spec::new(9, 9, 4, 8));
    let src = mem(b.png.clone());
    let m = f.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert!(m.is_complete());
}
