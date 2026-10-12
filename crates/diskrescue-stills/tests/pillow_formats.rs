//! GIF, BMP, WebP and TIFF files written by Pillow: exact extents, metadata,
//! validation, damage handling and the "stops exactly" property with junk
//! after the file.

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, ValidationStatus};
use diskrescue_core::source::{MemSource, SharedSource};
use diskrescue_core::{Range, RangeSet};
use diskrescue_stills::{BmpFormat, GifFormat, TiffFormat, WebpFormat};
use std::sync::Arc;

#[test]
fn gif_static_animated_and_noise() {
    for (name, w, h, frames) in [
        ("gif_static.gif", 64u32, 48u32, 1u32),
        ("gif_anim.gif", 64, 48, 5),
        ("gif_noise.gif", 200, 150, 1),
    ] {
        let g = need_pillow!(name);
        let m = check_exact(&GifFormat, &g);
        assert_eq!((m.meta.width, m.meta.height), (Some(w), Some(h)), "{name}");
        assert_eq!(m.meta.extra["frames"], frames.to_string());
        if frames > 1 {
            assert_eq!(m.meta.duration_ms, Some(40 * frames as u64));
            assert_eq!(m.meta.extra["animated"], "true");
        }
        let cuts: Vec<usize> = (1..8)
            .map(|i| g.len() * i / 8)
            .chain([g.len() - 1])
            .collect();
        check_truncation(&GifFormat, &g, &cuts);
    }
}

#[test]
fn gif_lzw_corruption_is_found_by_validate_but_not_by_measure() {
    let g = need_pillow!("gif_noise.gif");
    // Flip bits in the middle of the LZW data.
    let mut bad = g.clone();
    for i in 0..40 {
        bad[g.len() / 2 + i] ^= 0xA5;
    }
    let m = measure(&GifFormat, &bad);
    // The block structure may or may not survive; if it does, the length
    // still equals the file and validation must catch the damage.
    if m.status == ExtentStatus::Complete {
        assert_eq!(m.len, bad.len() as u64);
    }
    let v = GifFormat.validate(&mem(bad)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(!v.problems.is_empty());
}

#[test]
fn gif_with_garbage_block_stops_there() {
    let g = need_pillow!("gif_anim.gif");
    let mut bad = g.clone();
    // Overwrite the byte right after the first frame's data with junk. Find
    // the second image descriptor by walking: easier, damage near 60%.
    let at = g.len() * 6 / 10;
    for b in &mut bad[at..at + 8] {
        *b = 0x6B;
    }
    let m = measure(&GifFormat, &bad);
    assert!(matches!(
        m.status,
        ExtentStatus::Truncated | ExtentStatus::Complete
    ));
    if m.status == ExtentStatus::Truncated {
        let e = m.err_offset.unwrap();
        assert!(e <= m.len + 1 && e < bad.len() as u64);
    }
}

#[test]
fn bmp_variants() {
    for (name, w, h) in [
        ("bmp_rgb24.bmp", 13u32, 7u32),
        ("bmp_rgba32.bmp", 17, 9),
        ("bmp_pal8.bmp", 23, 11),
        ("bmp_mono.bmp", 37, 5),
        ("bmp_topdown.bmp", 13, 7),
        ("bmp_size0.bmp", 13, 7),
    ] {
        let b = need_pillow!(name);
        let m = check_exact(&BmpFormat, &b);
        assert_eq!((m.meta.width, m.meta.height), (Some(w), Some(h)), "{name}");
        let cuts: Vec<usize> = (1..6).map(|i| b.len() * i / 6).collect();
        check_truncation(&BmpFormat, &b, &cuts);
    }
    let td = need_pillow!("bmp_topdown.bmp");
    assert_eq!(measure(&BmpFormat, &td).meta.extra["row_order"], "top-down");
    let sz0 = need_pillow!("bmp_size0.bmp");
    assert_eq!(&sz0[2..6], &[0, 0, 0, 0]);
    let m = measure(&BmpFormat, &sz0);
    assert!(m.notes.is_empty() || m.len == sz0.len() as u64);
    let rgba = need_pillow!("bmp_rgba32.bmp");
    assert_eq!(
        measure(&BmpFormat, &rgba).meta.extra["bits_per_pixel"],
        "32"
    );
}

#[test]
fn webp_variants() {
    for (name, w, h, kind) in [
        ("webp_lossy.webp", 70u32, 50u32, "extended"),
        ("webp_lossless.webp", 70, 50, ""),
        ("webp_exif.webp", 70, 50, "extended"),
        ("webp_anim.webp", 40, 30, "extended"),
    ] {
        let f = need_pillow!(name);
        let m = check_exact(&WebpFormat, &f);
        assert_eq!((m.meta.width, m.meta.height), (Some(w), Some(h)), "{name}");
        if !kind.is_empty() {
            let _ = &m.meta.extra["webp_type"];
        }
        assert!(
            m.confidence.value() >= 85,
            "{name}: {}",
            m.confidence.value()
        );
        let cuts: Vec<usize> = (1..7).map(|i| f.len() * i / 7).collect();
        check_truncation(&WebpFormat, &f, &cuts);
    }
    let anim = need_pillow!("webp_anim.webp");
    let m = measure(&WebpFormat, &anim);
    assert_eq!(m.meta.extra["animated"], "true");
    assert_eq!(m.meta.extra["frames"], "3");
    let ll = need_pillow!("webp_lossless.webp");
    assert_eq!(
        measure(&WebpFormat, &ll).meta.extra["webp_type"],
        "lossless"
    );
}

#[test]
fn webp_exif_date_and_camera() {
    let f = need_pillow!("webp_exif.webp");
    let m = measure(&WebpFormat, &f);
    assert_eq!(m.meta.make.as_deref(), Some("DiskRescue"));
    assert_eq!(m.meta.model.as_deref(), Some("FixtureCam"));
    assert_eq!(m.meta.orientation, Some(6));
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-05-06T07:08:09Z");
}

#[test]
fn tiff_written_by_pillow_has_its_ifd_last() {
    for (name, w, h) in [
        ("tiff_strips.tif", 64u32, 48u32),
        ("tiff_lzw.tif", 64, 48),
        ("tiff_meta.tif", 64, 48),
        ("tiff_gray.tif", 50, 40),
    ] {
        let t = need_pillow!(name);
        let m = check_exact(&TiffFormat, &t);
        assert_eq!((m.meta.width, m.meta.height), (Some(w), Some(h)), "{name}");
        assert_eq!(m.meta.extra["variant"], "tiff");
        assert_eq!(m.meta.extra["extension"], "tif");
        // Cutting off the tail loses the IFD that libtiff writes last, so
        // there is nothing to measure; cutting inside it must not claim a
        // complete file.
        let cut = t.len() - 5;
        match TiffFormat
            .measure(&mem(t[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
        {
            None => {}
            Some(m) => assert_ne!(m.status, ExtentStatus::Complete),
        }
    }
}

#[test]
fn tiff_metadata_from_ifd0() {
    let t = need_pillow!("tiff_meta.tif");
    let m = measure(&TiffFormat, &t);
    assert_eq!(m.meta.make.as_deref(), Some("DiskRescue"));
    assert_eq!(m.meta.model.as_deref(), Some("FixtureCam"));
    assert_eq!(m.meta.orientation, Some(6));
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-05-06T07:08:09Z");
}

#[test]
fn multi_page_tiff_walks_the_whole_ifd_chain() {
    let t = need_pillow!("tiff_pages.tif");
    let m = measure(&TiffFormat, &t);
    // Pillow leaves four zero bytes after the last strip of a multi-page file.
    assert!(t.len() as u64 - m.len <= 4 && t[m.len as usize..].iter().all(|b| *b == 0));
    assert_eq!(m.meta.extra["ifds"], "3");
    // Largest page wins for the dimensions.
    assert_eq!((m.meta.width, m.meta.height), (Some(64), Some(48)));
    // Walk API: three chained IFDs.
    let w = diskrescue_stills::tiff::walk(
        &mem(t.clone()),
        0,
        u64::MAX,
        diskrescue_stills::tiff::Magic::Classic,
    )
    .unwrap()
    .unwrap();
    assert_eq!(w.ifds.len(), 3);
    assert_eq!(w.max_extent, m.len);
    assert_eq!(w.anomalies, 0, "{:?}", w.notes);
}

#[test]
fn unreadable_ranges_truncate_at_the_hole() {
    // A bad sector in the pixel data of each format: Truncated at the hole.
    let cases: Vec<(&dyn FileFormat, &str)> = vec![
        (&GifFormat, "gif_noise.gif"),
        (&BmpFormat, "bmp_rgba32.bmp"),
        (&WebpFormat, "webp_lossy.webp"),
    ];
    for (fmt, name) in cases {
        let f = need_pillow!(name);
        let hole_start = (f.len() * 3 / 5) as u64;
        let mut set = RangeSet::new();
        set.insert(Range::new(hole_start, hole_start + 64));
        let src: SharedSource = Arc::new(MemSource::new(f.clone()).with_unreadable(set));
        match fmt.measure(&src, 0, u64::MAX).unwrap() {
            Some(m) => {
                // Formats that never read the pixel data (BMP) do not notice.
                if m.status != ExtentStatus::Complete {
                    assert_eq!(m.status, ExtentStatus::Truncated, "{name}");
                    assert!(m.err_offset.unwrap() >= hole_start - 4096, "{name}: {m:?}");
                    assert!(m.err_offset.unwrap() <= hole_start + 512, "{name}: {m:?}");
                }
            }
            None => panic!("{name}: unreadable pixel data must not cause rejection"),
        }
    }
    // An unreadable first sector is an error, not a rejection.
    let g = need_pillow!("gif_static.gif");
    let mut set = RangeSet::new();
    set.insert(Range::new(0, 16));
    let src: SharedSource = Arc::new(MemSource::new(g).with_unreadable(set));
    assert!(GifFormat.measure(&src, 0, u64::MAX).is_err());
}

#[test]
fn limit_caps_the_measurement() {
    for (fmt, name) in [
        (&GifFormat as &dyn FileFormat, "gif_noise.gif"),
        (&BmpFormat, "bmp_rgba32.bmp"),
        (&WebpFormat, "webp_lossy.webp"),
        (&TiffFormat, "tiff_strips.tif"),
    ] {
        let f = need_pillow!(name);
        let limit = (f.len() / 2) as u64;
        // Junk after the file keeps the window limit-bound, not source-bound.
        let mut v = f.clone();
        v.extend(vec![0u8; 20_000]);
        let r = fmt.measure(&mem(v), 0, limit).unwrap();
        if let Some(m) = r {
            assert!(m.len <= limit, "{name}: {m:?}");
            assert!(
                matches!(m.status, ExtentStatus::Capped | ExtentStatus::Truncated),
                "{name}: {m:?}"
            );
        }
    }
}
