//! Hand-built TIFF, BigTIFF and camera-RAW files. Each builder lays out its
//! IFDs and payloads so that the last payload ends the file; the extent the
//! walker computes must therefore equal the file length, to the byte.

mod common;

use common::raws::{self, Sample};
use common::*;
use diskrescue_core::format::{
    ExtentStatus, FileFormat, FormatRegistry, NestedRole, ValidationStatus,
};
use diskrescue_stills::tiff::{self, Magic};
use diskrescue_stills::*;
use std::sync::Arc;

fn dedicated() -> Vec<(&'static str, Arc<dyn FileFormat>)> {
    vec![
        ("cr2", Arc::new(Cr2Format)),
        ("nef", Arc::new(NefFormat)),
        ("arw", Arc::new(ArwFormat)),
        ("dng", Arc::new(DngFormat)),
        ("pef", Arc::new(PefFormat)),
        ("orf", Arc::new(OrfFormat)),
        ("rw2", Arc::new(Rw2Format)),
    ]
}

fn fmt_for(id: &str) -> Arc<dyn FileFormat> {
    formats().into_iter().find(|f| f.id() == id).unwrap()
}

/// Truncation points: dense near the start, sparse after.
fn cuts(len: usize) -> Vec<usize> {
    let mut c: Vec<usize> = (1..len.min(400)).step_by(3).collect();
    c.extend((400..len).step_by((len / 40).max(1)));
    c.push(len - 1);
    c
}

#[test]
fn every_raw_sample_is_measured_to_the_byte() {
    for s in raws::all() {
        if s.id == "rw2" || s.id == "tiff" && s.name == "bigtiff" {
            continue; // covered by dedicated tests below
        }
        let f = fmt_for(s.id);
        let m = check_exact(f.as_ref(), &s.bytes);
        assert_eq!(m.len, s.bytes.len() as u64, "{}", s.name);
        if matches!(s.id, "psd" | "raf") {
            continue;
        }
        assert_eq!(m.meta.extra["extension"], s.ext, "{}", s.name);
    }
}

#[test]
fn generic_tiff_accepts_every_tiff_structured_sample_with_the_same_extent() {
    for s in raws::all() {
        if matches!(s.id, "psd" | "raf" | "orf" | "rw2") {
            continue;
        }
        let m = TiffFormat
            .measure(&mem(s.bytes.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap_or_else(|| panic!("{} not accepted by TiffFormat", s.name));
        assert_eq!(m.len, s.bytes.len() as u64, "{}", s.name);
        assert_eq!(m.status, ExtentStatus::Complete, "{}", s.name);
        assert_eq!(m.meta.extra["extension"], s.ext, "{}", s.name);
    }
}

#[test]
fn formats_are_mutually_exclusive() {
    for s in raws::all() {
        if matches!(s.id, "psd" | "raf") {
            continue;
        }
        for (id, f) in dedicated() {
            let r = f.measure(&mem(s.bytes.clone()), 0, u64::MAX).unwrap();
            let probed = f.probe(&s.bytes[..s.bytes.len().min(65536)]);
            if id == s.id {
                assert!(r.is_some(), "{} must accept {}", id, s.name);
                assert!(probed, "{} probe of {}", id, s.name);
            } else {
                assert!(r.is_none(), "{} must reject {}", id, s.name);
                assert!(!probed, "{} probe must reject {}", id, s.name);
            }
        }
    }
}

#[test]
fn registry_priority_picks_the_specific_format() {
    let mut reg = FormatRegistry::new();
    for f in formats() {
        reg.register(f);
    }
    assert_eq!(reg.len(), 13);
    for s in raws::all() {
        let head = &s.bytes[..s.bytes.len().min(65536)];
        let first = reg
            .matching(head)
            .into_iter()
            .find(|f| f.probe(head))
            .unwrap_or_else(|| panic!("no format for {}", s.name));
        assert_eq!(first.id(), s.id, "{}", s.name);
    }
    // Priorities as documented.
    let prio = |id: &str| reg.by_id(id).unwrap().priority();
    assert!(prio("cr2") > prio("tiff") && prio("nef") > prio("tiff"));
    assert!(prio("dng") > prio("nef") && prio("dng") > prio("pef"));
    assert_eq!(prio("tiff"), 50);
    assert_eq!(prio("orf"), 60);
}

#[test]
fn cr2_structure_and_metadata() {
    let s = raws::cr2();
    let m = check_exact(&Cr2Format, &s.bytes);
    assert_eq!(m.meta.make.as_deref(), Some("Canon"));
    assert_eq!(m.meta.model.as_deref(), Some("Canon EOS 5D Mark III"));
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
    assert_eq!(m.meta.orientation, Some(1));
    assert_eq!((m.meta.width, m.meta.height), (Some(5616), Some(3744)));
    assert_eq!(m.meta.extra["cr2_version"], "2.0");
    assert_eq!(m.meta.extra["cr2_slices"], "1,2960,2960");
    assert_eq!(m.confidence.value(), 90);
    // IFD0 full-size preview and IFD1 thumbnail are nested; the lossless
    // JPEG that holds the sensor data is not.
    let roles: Vec<_> = m
        .nested
        .iter()
        .map(|n| (n.role, n.format.clone()))
        .collect();
    assert_eq!(roles.len(), 2, "{:?}", m.nested);
    assert!(roles.contains(&(NestedRole::Preview, Some("jpeg".into()))));
    assert!(roles.contains(&(NestedRole::Thumbnail, Some("jpeg".into()))));
    let w = tiff::walk(&mem(s.bytes.clone()), 0, u64::MAX, Magic::Classic)
        .unwrap()
        .unwrap();
    assert_eq!(w.ifds.len(), 4, "IFD0..IFD3");
    assert!(w.exif_ifd.is_some());
    assert_eq!(w.cr2.unwrap().0, 2);
    assert_eq!(w.anomalies, 0, "{:?}", w.notes);
    // Last IFD holds the raw strip; its end is the file end.
    let raw_ifd = &w.ifds[3];
    let off = raw_ifd.find(0x0111).unwrap().scalar(w.endian).unwrap();
    let len = raw_ifd.find(0x0117).unwrap().scalar(w.endian).unwrap();
    assert_eq!(off + len, s.bytes.len() as u64);
    // CR2 needs the "CR" marker with major version 2.
    let mut other = s.bytes.clone();
    other[10] = 3;
    assert!(Cr2Format
        .measure(&mem(other.clone()), 0, u64::MAX)
        .unwrap()
        .is_none());
    assert!(!Cr2Format.probe(&other));
}

#[test]
fn nef_includes_the_makernote_preview_beyond_all_strips() {
    let s = raws::nef(false);
    let m = check_exact(&NefFormat, &s.bytes);
    assert_eq!(m.meta.make.as_deref(), Some("NIKON CORPORATION"));
    assert_eq!(m.meta.model.as_deref(), Some("NIKON D850"));
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
    // IFD0 thumbnail strips (uncompressed RGB), SubIFD preview, maker-note
    // preview: two JPEG previews are listed.
    let previews: Vec<_> = m
        .nested
        .iter()
        .filter(|n| n.role == NestedRole::Preview)
        .collect();
    assert_eq!(previews.len(), 2, "{:?}", m.nested);
    // The last bytes of the file are only reachable through the maker note.
    let last = previews.iter().map(|n| n.range.end).max().unwrap();
    assert_eq!(last, s.bytes.len() as u64);
    // Without the maker note being followed the extent would be shorter:
    let mut no_mn = s.bytes.clone();
    // Corrupt the Nikon header so the maker note is not recognised.
    let at = no_mn.windows(6).position(|w| w == b"Nikon\0").unwrap();
    no_mn[at] = b'X';
    let short = measure(&NefFormat, &no_mn);
    assert!(short.len < s.bytes.len() as u64, "maker note must matter");
    // COOLPIX bodies write NRW.
    let n = raws::nef(true);
    let m = check_exact(&NefFormat, &n.bytes);
    assert_eq!(m.meta.extra["extension"], "nrw");
}

#[test]
fn nef_maker_note_pointer_to_a_non_jpeg_is_ignored() {
    let s = raws::nef(false);
    // Point the Nikon preview at garbage: zero its SOI.
    let mut b = s.bytes.clone();
    let tail = b.len() - 25_000;
    b[tail] = 0;
    let m = measure(&NefFormat, &b);
    assert!(m.len < b.len() as u64);
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("does not point at a JPEG")),
        "{:?}",
        m.notes
    );
}

#[test]
fn arw_follows_sr2_private_data_placed_after_the_raw_strip() {
    let s = raws::arw(false);
    let m = check_exact(&ArwFormat, &s.bytes);
    assert_eq!(m.meta.make.as_deref(), Some("SONY"));
    assert_eq!(m.meta.extra["extension"], "arw");
    // Hide the SR2 pointer: the extent must shrink by the SR2 area.
    let mut b = s.bytes.clone();
    // DNGPrivateData is the last IFD0 entry (tag 0xC634).
    let pos = b
        .windows(4)
        .position(|w| w == [0x34, 0xC6, 0x04, 0x00])
        .unwrap();
    b[pos] = 0x35; // different tag
    let shorter = measure(&ArwFormat, &b);
    assert!(shorter.len < s.bytes.len() as u64);
    // The pointer stored as four BYTEs is read the same way.
    let b = raws::arw_with(false, true);
    let m = check_exact(&ArwFormat, &b.bytes);
    assert_eq!(m.len, b.bytes.len() as u64);
    // SR2 files are told apart by the maker note's FileFormat.
    let sr2 = raws::arw(true);
    let m = check_exact(&ArwFormat, &sr2.bytes);
    assert_eq!(m.meta.extra["extension"], "sr2");
}

#[test]
fn dng_wins_over_pentax_and_includes_original_raw_data() {
    let s = raws::dng();
    let m = check_exact(&DngFormat, &s.bytes);
    assert_eq!(m.meta.extra["dng_version"], "1.4.0.0");
    assert_eq!(m.meta.extra["variant"], "dng");
    // Make is PENTAX but the DNGVersion tag decides.
    assert!(PefFormat
        .measure(&mem(s.bytes.clone()), 0, u64::MAX)
        .unwrap()
        .is_none());
    // The IFD0 JPEG preview (compression 7) is nested.
    assert!(m
        .nested
        .iter()
        .any(|n| n.role == NestedRole::Thumbnail || n.role == NestedRole::Preview));
    // OriginalRawFileData is the last thing in the file.
    let w = tiff::walk(&mem(s.bytes.clone()), 0, u64::MAX, Magic::Classic)
        .unwrap()
        .unwrap();
    assert_eq!(w.sub_ifds.len(), 1);
    assert_eq!(w.dng_version, Some([1, 4, 0, 0]));
    assert_eq!(w.max_extent, s.bytes.len() as u64);
    // The last tile ends where the 60000-byte original copy begins.
    let sub = &w.sub_ifds[0];
    let offs = sub.find(0x0144).unwrap();
    assert_eq!(offs.count, 4);
    let tile_off = |i: usize| {
        let at = offs.value_or_offset as usize + 4 * i;
        u32::from_le_bytes(s.bytes[at..at + 4].try_into().unwrap()) as u64
    };
    assert_eq!(tile_off(3) + 20_000 + 60_000, s.bytes.len() as u64);
}

#[test]
fn pef_pentax_and_ricoh_with_makernote_preview() {
    for ricoh in [false, true] {
        let s = raws::pef(ricoh);
        let m = check_exact(&PefFormat, &s.bytes);
        assert_eq!(m.meta.model.as_deref(), Some("PENTAX K-5"));
        let last = m.nested.iter().map(|n| n.range.end).max().unwrap();
        assert_eq!(last, s.bytes.len() as u64, "maker-note preview is last");
    }
}

#[test]
fn orf_both_byte_orders_and_the_olympus_makernote_preview() {
    for be in [false, true] {
        let s = raws::orf(be);
        assert_eq!(&s.bytes[..4], if be { b"MMOR" } else { b"IIRO" });
        let m = check_exact(&OrfFormat, &s.bytes);
        assert_eq!(m.meta.make.as_deref(), Some("OLYMPUS IMAGING CORP."));
        // The raw strip is not the end: the maker-note preview and the IFD1
        // thumbnail follow it.
        assert!(m
            .nested
            .iter()
            .any(|n| n.range.end == s.bytes.len() as u64 || n.role == NestedRole::Thumbnail));
        // The plain TIFF format does not claim ORF magic.
        assert!(TiffFormat
            .measure(&mem(s.bytes.clone()), 0, u64::MAX)
            .unwrap()
            .is_none());
    }
    // IIRS magic is accepted too.
    let mut s = raws::orf(false).bytes;
    s[3] = b'S';
    assert!(OrfFormat.measure(&mem(s), 0, u64::MAX).unwrap().is_some());
}

#[test]
fn rw2_estimates_the_unstored_raw_length() {
    let s = raws::rw2(4);
    let (sw, sh) = (1000usize, 700usize);
    let exact = (sw * sh * 8).div_ceil(7);
    let raw_off = s.bytes.len() - exact.div_ceil(0x4000) * 0x4000;
    // Followed by unrelated data the end is only an estimate.
    let mut rng = Rng(99);
    let (src, start) = embed(&s.bytes, 0, 100_000, &mut rng);
    let m = Rw2Format.measure(&src, start, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert!(m.confidence.value() <= 35);
    assert!(m.len >= (raw_off + exact) as u64 && m.len < (raw_off + exact + 0x4000) as u64);
    assert_eq!(
        m.len,
        s.bytes.len() as u64,
        "padded estimate lands on the block end"
    );
    assert!(m.notes.iter().any(|n| n.contains("no stored length")));
    assert_eq!(m.meta.extra["variant"], "rw2");
    assert_eq!((m.meta.width, m.meta.height), (Some(1000), Some(700)));
    assert_eq!(m.meta.make.as_deref(), Some("Panasonic"));
    // The embedded JpgFromRaw is listed as a nested preview.
    assert!(m.nested.iter().any(|n| n.format.as_deref() == Some("jpeg")));
    // Alone in the source the file runs to the end of the source.
    let m = measure(&Rw2Format, &s.bytes);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.len, s.bytes.len() as u64);
    // Cut before the estimated end: Truncated.
    let cut = &s.bytes[..raw_off + exact / 2];
    let m = measure(&Rw2Format, cut);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.err_offset, Some(cut.len() as u64));
}

#[test]
fn rw2_other_raw_formats_report_only_what_is_known() {
    let s = raws::rw2(5);
    let mut rng = Rng(98);
    let (src, start) = embed(&s.bytes, 0, 10_000, &mut rng);
    let m = Rw2Format.measure(&src, start, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert!(m.confidence.value() <= 35);
    // Known structure: IFD0, JPEG, and the start of the raw data.
    let raw_off = s.bytes.len() - 50_000;
    assert_eq!(m.len, raw_off as u64);
    // A whole RW2 file validates: the structure is sound, the raw length
    // cannot be verified, which validation says so.
    let v = Rw2Format.validate(&mem(s.bytes.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid);
    assert_eq!(v.intact_fraction, None);
    assert!(v.problems.iter().any(|p| p.contains("not stored")));
    let s4 = raws::rw2(4);
    let v = Rw2Format.validate(&mem(s4.bytes.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    // Cut before the estimated end: damaged.
    let cut = s4.bytes[..s4.bytes.len() - 500_000].to_vec();
    let v = Rw2Format.validate(&mem(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn srw_and_erf_are_served_by_generic_tiff_with_the_right_extension() {
    for (make, comp, ext) in [
        ("SAMSUNG", 32770u16, "srw"),
        ("SEIKO EPSON CORP.", 1, "erf"),
    ] {
        let s = raws::generic_raw(make, comp, ext);
        let m = check_exact(&TiffFormat, &s.bytes);
        assert_eq!(m.meta.extra["extension"], ext);
        assert_eq!(m.meta.extra["variant"], ext);
        assert_eq!(m.meta.make.as_deref(), Some(make));
    }
}

#[test]
fn tiled_tiff_and_nested_previews() {
    let t = raws::tiled_tiff();
    let m = check_exact(&TiffFormat, &t.bytes);
    assert_eq!((m.meta.width, m.meta.height), (Some(32), Some(32)));
    let n = raws::nested_tiff();
    let m = check_exact(&TiffFormat, &n.bytes);
    assert_eq!(m.meta.make.as_deref(), Some("TestCam"));
    assert_eq!(m.meta.orientation, Some(8));
    assert_eq!(m.meta.extra["ifds"], "3");
    let roles: Vec<_> = m.nested.iter().map(|x| x.role).collect();
    assert!(roles.contains(&NestedRole::Preview), "{:?}", m.nested);
    assert!(roles.contains(&NestedRole::Thumbnail), "{:?}", m.nested);
    // The preview range is exact.
    let p = m
        .nested
        .iter()
        .find(|x| x.role == NestedRole::Preview)
        .unwrap();
    assert_eq!(p.range.len(), 12_000);
    assert_eq!(
        n.bytes[p.range.start as usize..p.range.start as usize + 2],
        [0xFF, 0xD8]
    );
    assert!(TiffFormat.may_contain("jpeg") && Cr2Format.may_contain("jpeg"));
}

#[test]
fn bigtiff_is_accepted_by_the_walker_and_rejected_as_classic() {
    let s = raws::bigtiff();
    let m = measure(&TiffFormat, &s.bytes);
    assert_eq!(m.len, s.bytes.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.meta.make.as_deref(), Some("Cam01"));
    assert_eq!((m.meta.width, m.meta.height), (Some(32), Some(20)));
    let w = tiff::walk(&mem(s.bytes.clone()), 0, u64::MAX, Magic::BigTiff)
        .unwrap()
        .unwrap();
    assert_eq!(w.max_extent, s.bytes.len() as u64);
    assert!(
        tiff::walk(&mem(s.bytes.clone()), 0, u64::MAX, Magic::Classic)
            .unwrap()
            .is_none()
    );
    // Dedicated RAW formats do not take BigTIFF.
    assert!(NefFormat
        .measure(&mem(s.bytes.clone()), 0, u64::MAX)
        .unwrap()
        .is_none());
    // A classic file is not BigTIFF.
    let c = raws::tiled_tiff();
    assert!(tiff::walk(&mem(c.bytes), 0, u64::MAX, Magic::BigTiff)
        .unwrap()
        .is_none());
    // Truncated BigTIFF.
    check_truncation(
        &TiffFormat,
        &s.bytes,
        &[s.bytes.len() - 1, s.bytes.len() - 100, 400],
    );
}

#[test]
fn truncated_raws_report_the_cut() {
    for s in [
        raws::cr2(),
        raws::nef(false),
        raws::arw(false),
        raws::dng(),
        raws::pef(false),
        raws::orf(true),
        raws::nested_tiff(),
        raws::tiled_tiff(),
    ] {
        let f = fmt_for(s.id);
        for cut in cuts(s.bytes.len()) {
            if let Some(m) = f
                .measure(&mem(s.bytes[..cut].to_vec()), 0, u64::MAX)
                .unwrap()
            {
                assert_eq!(m.status, ExtentStatus::Truncated, "{} cut {cut}", s.name);
                assert_eq!(m.err_offset, Some(cut as u64), "{} cut {cut}", s.name);
                assert_eq!(m.len, cut as u64, "{} cut {cut}", s.name);
            }
        }
        // Truncation inside the payload is always recognised.
        let cut = s.bytes.len() - 10;
        let m = f
            .measure(&mem(s.bytes[..cut].to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated, "{}", s.name);
    }
}

#[test]
fn a_limit_caps_the_walk() {
    for s in [raws::cr2(), raws::nef(false), raws::dng()] {
        let f = fmt_for(s.id);
        let mut v = s.bytes.clone();
        v.extend(vec![0u8; 100_000]);
        let limit = (s.bytes.len() / 2) as u64;
        let m = f.measure(&mem(v), 0, limit).unwrap().unwrap();
        assert_eq!(m.status, ExtentStatus::Capped, "{}", s.name);
        assert_eq!(m.len, limit);
        assert_eq!(m.err_offset, None);
    }
}

#[test]
fn walk_is_bounded_on_cyclic_and_deep_structures() {
    // SubIFD pointing back at IFD0, next pointer pointing at itself.
    let (bytes, _) = build(false, |o| {
        (
            tiff_header(false, 8),
            vec![
                Item::ifd(
                    vec![
                        short(0x0100, 4),
                        short(0x0101, 4),
                        short(0x0103, 1),
                        e(0x014A, V::Long(vec![o[0] as u32, o[1] as u32])),
                        long(0x8769, o[0] as u32),
                    ],
                    o[0] as u32,
                ),
                Item::ifd(
                    vec![
                        short(0x0100, 2),
                        short(0x0101, 2),
                        e(0x014A, V::Long(vec![o[0] as u32])),
                    ],
                    o[1] as u32,
                ),
            ],
        )
    });
    let w = tiff::walk(&mem(bytes.clone()), 0, u64::MAX, Magic::Classic)
        .unwrap()
        .unwrap();
    assert!(w.ifd_count() <= 4, "{}", w.ifd_count());
    assert_eq!(w.max_extent, bytes.len() as u64);

    // A chain of 200 IFDs ends at the chain limit.
    let ifd_len = 2 + 36 + 4;
    let mut chain = tiff_header(false, 8);
    for i in 0..200u32 {
        chain.extend_from_slice(&3u16.to_le_bytes());
        for (tag, v) in [(0x0100u16, 1u16), (0x0101, 1), (0x0103, 1)] {
            chain.extend_from_slice(&tag.to_le_bytes());
            chain.extend_from_slice(&3u16.to_le_bytes());
            chain.extend_from_slice(&1u32.to_le_bytes());
            chain.extend_from_slice(&[v as u8, 0, 0, 0]);
        }
        let next = if i < 199 {
            8 + (i + 1) * ifd_len as u32
        } else {
            0
        };
        chain.extend_from_slice(&next.to_le_bytes());
    }
    let w = tiff::walk(&mem(chain), 0, u64::MAX, Magic::Classic)
        .unwrap()
        .unwrap();
    assert!(w.ifds.len() <= 16, "chain limited, got {}", w.ifds.len());
}

#[test]
fn start_offset_and_embedded_in_noise() {
    // Several different files back to back with junk between: each is
    // measured independently from its own start.
    let samples: Vec<Sample> = vec![raws::cr2(), raws::nef(false), raws::dng(), raws::raf(true)];
    let mut rng = Rng(5);
    let mut img = rng.bytes(4096);
    let mut starts = Vec::new();
    for s in &samples {
        starts.push(img.len() as u64);
        img.extend_from_slice(&s.bytes);
        img.extend(rng.bytes(4096 - (s.bytes.len() % 4096)));
    }
    let src = mem(img);
    for (s, start) in samples.iter().zip(starts) {
        let f = fmt_for(s.id);
        let m = f.measure(&src, start, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, s.bytes.len() as u64, "{}", s.name);
        assert_eq!(m.status, ExtentStatus::Complete);
    }
}
