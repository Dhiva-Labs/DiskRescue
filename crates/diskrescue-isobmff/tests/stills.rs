//! HEIF/HEIC/AVIF and Canon CR3: measurement, metadata, nesting.

mod common;

use std::process::Command;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, NestedRole, ValidationStatus};
use diskrescue_core::Source;
use diskrescue_isobmff::tiff::build_tiff;
use diskrescue_isobmff::writer::BoxBuf;
use diskrescue_isobmff::{Cr3Format, HeifFormat, Mp4Format};

fn jpeg(w: u32, h: u32, name: &str) -> Vec<u8> {
    if have("ffmpeg") {
        let p = scratch(&format!("{name}.jpg"));
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={w}x{h}"))
            .args(["-frames:v", "1"])
            .arg(&p)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return std::fs::read(p).unwrap();
        }
    }
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
    j.extend(noise(2000, w as u64));
    j.extend([0xFF, 0xD9]);
    j
}

fn ftyp(major: &[u8; 4], minor: u32, compat: &[&[u8; 4]]) -> Vec<u8> {
    let mut b = BoxBuf::new();
    b.start(b"ftyp");
    b.bytes(major);
    b.u32(minor);
    for c in compat {
        b.bytes(*c);
    }
    b.end();
    b.buf
}

/// Hand-built HEIC: ftyp + meta (hdlr, pitm, iinf, iprp, iloc) + mdat with
/// an Exif item, the primary HEVC item and a JPEG thumbnail item.
fn heic(mdat_size_zero: bool) -> Vec<u8> {
    let exif = {
        let mut e = 6u32.to_be_bytes().to_vec();
        e.extend_from_slice(b"Exif\0\0");
        e.extend(build_tiff(
            false,
            &[(0x010F, "Apple"), (0x0110, "iPhone 15 Pro")],
            &[(0x9003, "2024:03:05 10:20:30")],
        ));
        e
    };
    // A length-prefixed IDR slice NAL unit (measure checks that item data
    // looks like its type); the payload avoids the zero runs no encoder
    // writes inside a NAL unit.
    let image = {
        let body: Vec<u8> = noise(2994, 42).into_iter().map(|b| b.max(1)).collect();
        let mut v = ((body.len() + 2) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(&[19 << 1, 0x01]);
        v.extend(body);
        v
    };
    let thumb = jpeg(64, 48, "heic_thumb");
    let build_meta = |offs: [u32; 3]| -> Vec<u8> {
        let mut b = BoxBuf::new();
        b.start_full(b"meta", 0, 0);
        b.start_full(b"hdlr", 0, 0);
        b.u32(0);
        b.bytes(b"pict");
        b.zeros(12);
        b.u8(0);
        b.end();
        b.start_full(b"pitm", 0, 0);
        b.u16(1);
        b.end();
        b.start_full(b"iinf", 0, 0);
        b.u16(3);
        for (id, t) in [(1u16, b"hvc1"), (2, b"Exif"), (3, b"jpeg")] {
            b.start_full(b"infe", 2, 0);
            b.u16(id);
            b.u16(0);
            b.bytes(t);
            b.u8(0);
            b.end();
        }
        b.end();
        b.start(b"iprp");
        b.start(b"ipco");
        b.start(b"hvcC");
        b.u8(1);
        b.zeros(22);
        b.end();
        b.start_full(b"ispe", 0, 0);
        b.u32(640);
        b.u32(480);
        b.end();
        b.end();
        b.start_full(b"ipma", 0, 0);
        b.u32(1);
        b.u16(1);
        b.u8(2);
        b.u8(0x81);
        b.u8(0x02);
        b.end();
        b.end();
        b.start_full(b"iloc", 0, 0);
        b.u8(0x44);
        b.u8(0x00);
        b.u16(3);
        let lens = [image.len(), exif.len(), thumb.len()];
        for (i, id) in [1u16, 2, 3].iter().enumerate() {
            b.u16(*id);
            b.u16(0);
            b.u16(1);
            b.u32(offs[i]);
            b.u32(lens[i] as u32);
        }
        b.end();
        b.end();
        b.buf
    };
    let f = ftyp(b"heic", 0, &[b"mif1", b"heic"]);
    let meta_len = build_meta([0; 3]).len();
    let payload = (f.len() + meta_len + 8) as u32;
    let exif_at = payload;
    let image_at = exif_at + exif.len() as u32;
    let thumb_at = image_at + image.len() as u32;
    let meta = build_meta([image_at, exif_at, thumb_at]);
    let mut out = f;
    out.extend(meta);
    let mdat_len = 8 + exif.len() + image.len() + thumb.len();
    out.extend_from_slice(&(if mdat_size_zero { 0 } else { mdat_len as u32 }).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend(exif);
    out.extend(image);
    out.extend(thumb);
    out
}

#[test]
fn heic_measure_meta_and_extents() {
    let h = heic(false);
    let f = HeifFormat;
    assert!(f.probe(&h));
    assert!(!Mp4Format.probe(&h));
    let mut img = h.clone();
    img.extend(noise(5000, 1));
    for src in [mem(h.clone()), mem(img)] {
        let m = f.measure(&src, 0, src.size()).unwrap().unwrap();
        assert_eq!(m.len, h.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete, "{m:?}");
        assert_eq!(m.confidence.value(), 90, "{m:?}");
        assert_eq!(m.meta.dimensions().as_deref(), Some("640x480"));
        assert_eq!(m.meta.make.as_deref(), Some("Apple"));
        assert_eq!(m.meta.model.as_deref(), Some("iPhone 15 Pro"));
        assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-03-05T10:20:30Z");
        assert_eq!(m.meta.extra["extension"], "heic");
        assert_eq!(m.nested.len(), 1);
        assert_eq!(m.nested[0].role, NestedRole::Thumbnail);
        let r = m.nested[0].range;
        assert_eq!(&h[r.start as usize..r.start as usize + 2], &[0xFF, 0xD8]);
    }
    let v = f.validate(&mem(h.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    assert!(!f.can_repair());
    // mdat size 0: the iloc evidence gives the end.
    let z = heic(true);
    let mut zi = z.clone();
    zi.extend(noise(3000, 2));
    let m = f
        .measure(&mem(zi.clone()), 0, zi.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.len, z.len() as u64);
    // Truncated.
    let cut = h[..h.len() * 7 / 10].to_vec();
    let m = f
        .measure(&mem(cut.clone()), 0, cut.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    let v = f.validate(&mem(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    // Not a movie, not a CR3.
    assert!(Mp4Format
        .measure(&mem(h.clone()), 0, h.len() as u64)
        .unwrap()
        .is_none());
    assert!(Cr3Format
        .measure(&mem(h.clone()), 0, h.len() as u64)
        .unwrap()
        .is_none());
}

#[test]
fn avif_from_ffmpeg() {
    if !ffmpeg_or_skip() {
        return;
    }
    let p = scratch("still.avif");
    let ok = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=128x96",
            "-frames:v",
            "1",
        ])
        .args(["-c:v", "libaom-av1", "-still-picture", "1"])
        .arg(&p)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipped: ffmpeg cannot encode AVIF (libaom-av1 missing)");
        return;
    }
    let bytes = std::fs::read(&p).unwrap();
    let mut img = bytes.clone();
    img.extend(noise(4096, 77));
    let m = HeifFormat
        .measure(&mem(img), 0, (bytes.len() + 4096) as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.len, bytes.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.meta.extra["extension"], "avif");
    assert_eq!(m.meta.dimensions().as_deref(), Some("128x96"));
    assert!(Mp4Format
        .measure(&open(&p), 0, bytes.len() as u64)
        .unwrap()
        .is_none());
}

const CANON: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
const XMP: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];
const PRVW: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];

fn uuid_box(uuid: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 24) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(b"uuid");
    v.extend_from_slice(uuid);
    v.extend_from_slice(payload);
    v
}

/// Hand-built CR3 skeleton.
fn cr3() -> Vec<u8> {
    let thumb = jpeg(160, 120, "cr3_thmb");
    let preview = jpeg(320, 214, "cr3_prvw");
    let full = jpeg(640, 426, "cr3_full");
    let raw = noise(20_000, 9);
    let xmp = uuid_box(&XMP, b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>");
    let prvw_box = {
        let mut p = vec![0, 0, 0, 0, 0, 0, 0, 1];
        let mut b = BoxBuf::new();
        b.start(b"PRVW");
        b.u32(0);
        b.u16(1);
        b.u16(320);
        b.u16(214);
        b.u16(1);
        b.u32(preview.len() as u32);
        b.bytes(&preview);
        while b.buf.len() % 4 != 0 {
            b.u8(0);
        }
        b.end();
        p.extend(b.buf);
        uuid_box(&PRVW, &p)
    };
    let build_moov = |xmp_at: u64, prvw_at: u64, mdat_at: u64, mdat_len: u64| -> Vec<u8> {
        let mut canon = BoxBuf::new();
        canon.start(b"CNCV");
        canon.bytes(b"CanonCR3_001/00.09.00/00.00.00");
        canon.end();
        canon.start(b"CTBO");
        canon.u32(3);
        for (i, (o, l)) in [
            (xmp_at, xmp.len() as u64),
            (prvw_at, prvw_box.len() as u64),
            (mdat_at, mdat_len),
        ]
        .iter()
        .enumerate()
        {
            canon.u32(i as u32 + 1);
            canon.u64(*o);
            canon.u64(*l);
        }
        canon.end();
        canon.start(b"CMT1");
        canon.bytes(&build_tiff(
            true,
            &[(0x010F, "Canon"), (0x0110, "Canon EOS R5")],
            &[],
        ));
        canon.end();
        canon.start(b"CMT2");
        canon.bytes(&build_tiff(true, &[], &[(0x9003, "2023:07:14 09:05:59")]));
        canon.end();
        canon.start(b"THMB");
        canon.u8(0);
        canon.zeros(3);
        canon.u16(160);
        canon.u16(120);
        canon.u32(thumb.len() as u32);
        canon.u16(1);
        canon.u16(0);
        canon.bytes(&thumb);
        while canon.buf.len() % 4 != 0 {
            canon.u8(0);
        }
        canon.end();
        let mut b = BoxBuf::new();
        b.start(b"moov");
        b.bytes(&uuid_box(&CANON, &canon.buf));
        b.start_full(b"mvhd", 0, 0);
        b.u32(0);
        b.u32(0);
        b.u32(1000);
        b.u32(1);
        b.u32(0x10000);
        b.u16(0x100);
        b.zeros(10);
        for v in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000] {
            b.u32(v);
        }
        b.zeros(24);
        b.u32(2);
        b.end();
        b.start(b"trak");
        b.start_full(b"tkhd", 0, 3);
        b.u32(0);
        b.u32(0);
        b.u32(1);
        b.u32(0);
        b.u32(1);
        b.zeros(8);
        b.zeros(8);
        for v in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000] {
            b.u32(v);
        }
        b.u32(640 << 16);
        b.u32(426 << 16);
        b.end();
        b.start(b"mdia");
        b.start_full(b"mdhd", 0, 0);
        b.u32(0);
        b.u32(0);
        b.u32(1);
        b.u32(1);
        b.u16(0x55C4);
        b.u16(0);
        b.end();
        b.start_full(b"hdlr", 0, 0);
        b.u32(0);
        b.bytes(b"vide");
        b.zeros(12);
        b.u8(0);
        b.end();
        b.start(b"minf");
        b.start_full(b"vmhd", 0, 1);
        b.zeros(8);
        b.end();
        b.start(b"stbl");
        b.start_full(b"stsd", 0, 0);
        b.u32(1);
        b.start(b"CRAW");
        b.zeros(6);
        b.u16(1);
        b.zeros(16);
        b.u16(640);
        b.u16(426);
        b.u32(0x0048_0000);
        b.u32(0x0048_0000);
        b.u32(0);
        b.u16(1);
        b.zeros(32);
        b.u16(0x18);
        b.u16(0xFFFF);
        b.end();
        b.end();
        b.start_full(b"stts", 0, 0);
        b.u32(1);
        b.u32(1);
        b.u32(1);
        b.end();
        b.start_full(b"stsc", 0, 0);
        b.u32(1);
        b.u32(1);
        b.u32(1);
        b.u32(1);
        b.end();
        b.start_full(b"stsz", 0, 0);
        b.u32(0);
        b.u32(1);
        b.u32(full.len() as u32);
        b.end();
        b.start_full(b"co64", 0, 0);
        b.u32(1);
        b.u64(mdat_at + 16);
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        b.end();
        b.buf
    };
    let f = ftyp(b"crx ", 1, &[b"crx ", b"isom"]);
    let moov_len = build_moov(0, 0, 0, 0).len() as u64;
    let xmp_at = f.len() as u64 + moov_len;
    let prvw_at = xmp_at + xmp.len() as u64;
    let mdat_at = prvw_at + prvw_box.len() as u64;
    let mdat_len = 16 + full.len() as u64 + raw.len() as u64;
    let moov = build_moov(xmp_at, prvw_at, mdat_at, mdat_len);
    let mut out = f;
    out.extend(moov);
    out.extend(xmp);
    out.extend(prvw_box);
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&mdat_len.to_be_bytes());
    out.extend(full);
    out.extend(raw);
    out
}

#[test]
fn cr3_measure_meta_and_previews() {
    let c = cr3();
    let f = Cr3Format;
    assert!(f.probe(&c));
    assert!(!HeifFormat.probe(&c) && !Mp4Format.probe(&c));
    let mut img = c.clone();
    img.extend(noise(8000, 3));
    for src in [mem(c.clone()), mem(img)] {
        let m = f.measure(&src, 0, src.size()).unwrap().unwrap();
        assert_eq!(m.len, c.len() as u64, "{:?}", m.notes);
        assert_eq!(m.status, ExtentStatus::Complete, "{m:?}");
        assert_eq!(m.confidence.value(), 90, "{:?}", m.notes);
        assert_eq!(m.meta.make.as_deref(), Some("Canon"));
        assert_eq!(m.meta.model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
        assert_eq!(m.meta.extra["extension"], "cr3");
        assert_eq!(m.meta.dimensions().as_deref(), Some("640x426"));
        let roles: Vec<_> = m.nested.iter().map(|n| n.role).collect();
        assert_eq!(
            roles
                .iter()
                .filter(|r| **r == NestedRole::Thumbnail)
                .count(),
            1
        );
        assert_eq!(
            roles.iter().filter(|r| **r == NestedRole::Preview).count(),
            2
        );
        for n in &m.nested {
            let s = n.range.start as usize;
            assert_eq!(&c[s..s + 3], &[0xFF, 0xD8, 0xFF], "{n:?}");
            assert_eq!(
                &c[n.range.end as usize - 2..n.range.end as usize],
                &[0xFF, 0xD9]
            );
            assert_eq!(n.format.as_deref(), Some("jpeg"));
        }
    }
    let v = f.validate(&mem(c.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    // Truncated in the raw data: CTBO and the walk both say so.
    let cut = c[..c.len() - 5000].to_vec();
    let m = f
        .measure(&mem(cut.clone()), 0, cut.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(
        f.validate(&mem(cut)).unwrap().status,
        ValidationStatus::Damaged
    );
}
