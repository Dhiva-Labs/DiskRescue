//! Regressions for over-claimed extents: a file whose size fields outlive
//! its data (cut short, then other files written after it) must not be
//! measured as one complete file over the files that follow, or the carver
//! swallows them. Also: what each format's own structure legitimately
//! contains (Exif blocks, pointed-to JPEG strips) must not be taken for
//! another file, and a bare Exif block is not a photo.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_core::format::{Confidence, ExtentStatus, FileFormat, Measured};
use diskrescue_fixtures::damage;
use diskrescue_fixtures::media::{self, StillOpts, TiffOpts};
use diskrescue_stills::*;

// ---------------------------------------------------------------------------
// Carving-image layout (as `diskrescue_fixtures::raw::truncated`)
// ---------------------------------------------------------------------------

fn samples() -> Vec<(&'static str, Arc<dyn FileFormat>, Vec<u8>)> {
    let still = StillOpts {
        width: 160,
        height: 120,
        seed: 7,
    };
    vec![
        ("image.bmp", Arc::new(BmpFormat), media::bmp(&still)),
        ("image.webp", Arc::new(WebpFormat), media::webp(&still)),
        (
            "image.tif",
            Arc::new(TiffFormat),
            media::tiff(&TiffOpts {
                width: 160,
                height: 120,
                seed: 8,
                ..Default::default()
            }),
        ),
        (
            "raw.cr2",
            Arc::new(Cr2Format),
            media::cr2_like(&TiffOpts {
                width: 256,
                height: 192,
                seed: 9,
                ..Default::default()
            }),
        ),
    ]
}

/// Lay `files` out at 4 KiB alignment, each followed by a fill up to the
/// next 4 KiB boundary (zeros, 0xFF, random, text in turn) and 8 KiB of
/// random data; a JPEG closes the image so that every file has a successor.
/// Returns the image and each file's start.
fn layout(files: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
    let mut rng = Rng(0xF111);
    let mut img = vec![0u8; 4096];
    let mut starts = Vec::new();
    for (i, f) in files.iter().enumerate() {
        starts.push(img.len() as u64);
        img.extend_from_slice(f);
        let pad = (4096 - img.len() % 4096) % 4096;
        match i % 4 {
            0 => img.extend(std::iter::repeat_n(0u8, pad)),
            1 => img.extend(std::iter::repeat_n(0xFFu8, pad)),
            2 => img.extend(rng.bytes(pad)),
            _ => {
                let line = b"The quick brown fox jumps over the lazy dog. 0123456789\n";
                img.extend((0..pad).map(|k| line[k % line.len()]));
            }
        }
        img.extend(rng.bytes(8192));
    }
    starts.push(img.len() as u64);
    img.extend(media::baseline_jpeg(64, 48, 80, 3));
    img.extend(vec![0u8; 4096]);
    (img, starts)
}

#[test]
fn cut_files_followed_by_other_files_do_not_swallow_them() {
    let s = samples();
    let cut: Vec<Vec<u8>> = s
        .iter()
        .map(|(_, _, d)| damage::truncate(d, d.len() * 60 / 100))
        .collect();
    let (img, starts) = layout(&cut);
    let src = mem(img);
    for (i, (name, f, full)) in s.iter().enumerate() {
        let start = starts[i];
        let stored = cut[i].len() as u64;
        let next = starts[i + 1];
        let m = f.measure(&src, start, u64::MAX).unwrap().unwrap();
        eprintln!(
            "{name}: declared {} stored {stored} measured {} ({:?}, confidence {})",
            full.len(),
            m.len,
            m.status,
            m.confidence.value()
        );
        eprintln!("  {:?}", m.notes);
        assert_eq!(m.status, ExtentStatus::Truncated, "{name}: {:?}", m.notes);
        assert!(m.len >= stored, "{name}: the stored prefix is kept");
        assert!(
            start + m.len <= next,
            "{name}: swallows the next file ({} > {next})",
            start + m.len
        );
        assert_eq!(m.err_offset, Some(m.len), "{name}");
        assert!(m.confidence.value() < 80, "{name}: {:?}", m.confidence);
        assert!(m.confidence >= Confidence::LOW, "{name}: still carved");
        assert!(
            m.notes.iter().any(|n| n.contains("file starts at offset")),
            "{name}: {:?}",
            m.notes
        );
        assert!(m.nested.iter().all(|n| n.range.end <= m.len));
    }
}

#[test]
fn intact_files_in_the_same_layout_are_exact() {
    let s = samples();
    let files: Vec<Vec<u8>> = s.iter().map(|(_, _, d)| d.clone()).collect();
    let (img, starts) = layout(&files);
    let src = mem(img);
    for (i, (name, f, d)) in s.iter().enumerate() {
        let m = f.measure(&src, starts[i], u64::MAX).unwrap().unwrap();
        let alone = measure(f.as_ref(), d);
        assert_eq!(m.status, ExtentStatus::Complete, "{name}: {:?}", m.notes);
        assert_eq!(m.len, d.len() as u64, "{name}");
        assert_eq!(m.confidence, alone.confidence, "{name}");
    }
}

#[test]
fn a_hole_in_the_tail_is_not_sampled_and_does_not_cut() {
    // The sector holding the next file's header is known to be unreadable:
    // it is not read, so it is no evidence either way.
    let s = samples();
    let (name, f, d) = &s[1];
    let cut = damage::truncate(d, d.len() * 60 / 100);
    let (img, starts) = layout(std::slice::from_ref(&cut));
    let holes =
        diskrescue_core::RangeSet::from_range(diskrescue_core::Range::with_len(starts[1], 512));
    let src: diskrescue_core::SharedSource =
        Arc::new(diskrescue_core::source::MemSource::new(img).with_unreadable(holes));
    let m = f.measure(&src, starts[0], u64::MAX).unwrap().unwrap();
    assert_ne!(
        m.len,
        starts[1] - starts[0],
        "{name}: cut at an unread sector"
    );
    // Readable, the same sector does end the file.
    let (img, _) = layout(std::slice::from_ref(&cut));
    let m = f.measure(&mem(img), starts[0], u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, starts[1] - starts[0], "{name}");
}

// ---------------------------------------------------------------------------
// WebP
// ---------------------------------------------------------------------------

fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    c.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.iter().flatten().copied().collect();
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    f.extend_from_slice(b"WEBP");
    f.extend_from_slice(&body);
    f
}

/// A VP8 key frame payload of `len` bytes (first partition 16 bytes).
fn vp8_payload(len: usize) -> Vec<u8> {
    let mut p = vec![0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A, 64, 0, 48, 0];
    p.resize(len, 0x33);
    p
}

fn jfif() -> Vec<u8> {
    fake_jpeg(16, 16, 600, &mut Rng(1))
}

fn png_head() -> Vec<u8> {
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR\x00\x00\x00\x10\x00\x00\x00\x10\x08\x02\0\0\0".to_vec()
}

#[test]
fn webp_whose_bitstream_holds_another_file_is_cut_there() {
    let mut payload = vp8_payload(6000);
    // 20 bytes of RIFF and chunk header precede the payload.
    let j = jfif();
    payload[4096 - 20..4096 - 20 + j.len()].copy_from_slice(&j);
    let f = riff(&[chunk(b"VP8 ", &payload)]);
    let m = measure(&WebpFormat, &f);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!((m.len, m.err_offset), (4096, Some(4096)));
    assert_eq!(m.confidence, Confidence::HIGH.lowered(30));
    assert!(m.notes.iter().any(|n| n.contains("JPEG file starts")));
    // Off the sector grid it is no evidence.
    let mut payload = vp8_payload(6000);
    payload[4000..4000 + j.len()].copy_from_slice(&j);
    let f = riff(&[chunk(b"VP8 ", &payload)]);
    assert_eq!(check_exact(&WebpFormat, &f).len, f.len() as u64);
}

#[test]
fn webp_exif_chunk_may_hold_tiff_and_jpeg_headers_on_the_grid() {
    // The EXIF payload starts at 512: its TIFF header and a thumbnail JPEG
    // sit on the sector grid.
    let n = 512 - 12 - 8 - 8;
    let mut exif = exif_tiff(false, "Canon", "EOS R5", 1, "2024:01:02 03:04:05");
    exif.resize(512, 0);
    exif.extend(jfif());
    let f = riff(&[chunk(b"VP8 ", &vp8_payload(n)), chunk(b"EXIF", &exif)]);
    assert_eq!(&f[512..516], b"II*\0");
    assert_eq!(&f[1024..1027], [0xFF, 0xD8, 0xFF]);
    let m = check_exact(&WebpFormat, &f);
    assert_eq!(m.confidence, Confidence::HIGH);
    // A PNG there is not an Exif block's.
    let mut bad = exif.clone();
    bad[512..512 + png_head().len()].copy_from_slice(&png_head());
    let f = riff(&[chunk(b"VP8 ", &vp8_payload(n)), chunk(b"EXIF", &bad)]);
    let m = measure(&WebpFormat, &f);
    assert_eq!((m.status, m.len), (ExtentStatus::Truncated, 1024));
}

#[test]
fn webp_image_chunks_after_vp8x_need_a_valid_bitstream_header() {
    let mut x = vec![0x00, 0, 0, 0];
    x.extend_from_slice(&[63, 0, 0, 47, 0, 0]);
    let good = riff(&[chunk(b"VP8X", &x), chunk(b"VP8 ", &vp8_payload(100))]);
    check_exact(&WebpFormat, &good);
    let mut broken = vp8_payload(100);
    broken[3] = 0; // start code gone
    let bad = riff(&[chunk(b"VP8X", &x), chunk(b"VP8 ", &broken)]);
    let m = measure(&WebpFormat, &bad);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.err_offset, Some(30));
    // A frame libwebp refuses (hidden) only costs confidence.
    let mut hidden = vp8_payload(100);
    hidden[0] &= !0x10;
    let m = measure(&WebpFormat, &riff(&[chunk(b"VP8 ", &hidden)]));
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.confidence < Confidence::HIGH);
}

#[test]
fn exif_carrying_formats_declare_tiff_and_jpeg() {
    for f in [
        &WebpFormat as &dyn FileFormat,
        &PsdFormat,
        &RafFormat,
        &TiffFormat,
        &Cr2Format,
        &NefFormat,
        &ArwFormat,
        &DngFormat,
        &PefFormat,
        &OrfFormat,
        &Rw2Format,
    ] {
        assert!(f.may_contain("tiff") && f.may_contain("jpeg"), "{}", f.id());
        assert!(!f.may_contain("png") && !f.may_contain("mp4"), "{}", f.id());
    }
}

// ---------------------------------------------------------------------------
// BMP
// ---------------------------------------------------------------------------

fn bmp(w: u32, h: u32, compression: u32, off: u32, payload: &[u8]) -> Vec<u8> {
    let stride = (w as usize * 24).div_ceil(32) * 4;
    let data = if compression == 0 {
        stride * h as usize
    } else {
        payload.len()
    };
    let total = off as usize + data;
    let mut b = vec![0x40u8; total];
    b[..2].copy_from_slice(b"BM");
    b[2..6].copy_from_slice(&(total as u32).to_le_bytes());
    b[6..10].fill(0);
    b[10..14].copy_from_slice(&off.to_le_bytes());
    let mut dib = vec![0u8; 40];
    dib[0..4].copy_from_slice(&40u32.to_le_bytes());
    dib[4..8].copy_from_slice(&w.to_le_bytes());
    dib[8..12].copy_from_slice(&h.to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&if compression == 0 { 24u16 } else { 0 }.to_le_bytes());
    dib[16..20].copy_from_slice(&compression.to_le_bytes());
    dib[20..24].copy_from_slice(&(data as u32).to_le_bytes());
    b[14..54].copy_from_slice(&dib);
    if compression != 0 {
        b[off as usize..].copy_from_slice(payload);
    }
    b
}

#[test]
fn bmp_pixels_holding_another_file_end_the_bitmap() {
    let mut b = bmp(100, 100, 0, 54, &[]);
    let p = png_head();
    b[8192..8192 + p.len()].copy_from_slice(&p);
    let m = measure(&BmpFormat, &b);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!((m.len, m.err_offset), (8192, Some(8192)));
    assert!(m.confidence >= Confidence::LOW && m.confidence < Confidence::MEDIUM);
    assert!(m.notes.iter().any(|n| n.contains("PNG file starts")));
    // Clean, the size is backed by the header only: MEDIUM at best.
    let clean = bmp(100, 100, 0, 54, &[]);
    let m = check_exact(&BmpFormat, &clean);
    assert!(m.confidence <= Confidence::MEDIUM);
}

#[test]
fn bmp_png_payload_on_the_grid_is_its_own() {
    let mut png = png_head();
    png.resize(2000, 0x21);
    let b = bmp(16, 16, 5, 512, &png);
    let m = check_exact(&BmpFormat, &b);
    assert_eq!(m.nested.len(), 1);
}

// ---------------------------------------------------------------------------
// TIFF family
// ---------------------------------------------------------------------------

/// A little-endian TIFF of `total` bytes: IFD0 at 8 with `entries`
/// (tag, type, count, value), `blobs` at fixed offsets, 0x5A elsewhere.
fn le_tiff(entries: &[(u16, u16, u32, u32)], blobs: &[(usize, Vec<u8>)], total: usize) -> Vec<u8> {
    let mut b = vec![0x5Au8; total];
    b[..8].copy_from_slice(b"II*\0\x08\0\0\0");
    let mut e = entries.to_vec();
    e.sort_by_key(|x| x.0);
    b[8..10].copy_from_slice(&(e.len() as u16).to_le_bytes());
    let mut p = 10;
    for (tag, typ, count, val) in e {
        b[p..p + 2].copy_from_slice(&tag.to_le_bytes());
        b[p + 2..p + 4].copy_from_slice(&typ.to_le_bytes());
        b[p + 4..p + 8].copy_from_slice(&count.to_le_bytes());
        b[p + 8..p + 12].copy_from_slice(&val.to_le_bytes());
        p += 12;
    }
    b[p..p + 4].fill(0);
    for (off, blob) in blobs {
        b[*off..off + blob.len()].copy_from_slice(blob);
    }
    b
}

fn longs(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn image_tags(compression: u16) -> Vec<(u16, u16, u32, u32)> {
    vec![
        (0x0100, 3, 1, 32),
        (0x0101, 3, 1, 32),
        (0x0102, 3, 1, 8),
        (0x0103, 3, 1, compression as u32),
        (0x0106, 3, 1, 1),
        (0x0115, 3, 1, 1),
    ]
}

#[test]
fn tiff_jpeg_strips_on_the_grid_are_the_files_own() {
    // Three JPEG-compressed strips, each a JFIF stream on a sector boundary.
    let mut t = image_tags(7);
    t.push((0x0111, 4, 3, 400));
    t.push((0x0117, 4, 3, 420));
    let strips: Vec<(usize, Vec<u8>)> = [1024usize, 2048, 3072]
        .iter()
        .map(|&o| (o, fake_jpeg(16, 16, 1024, &mut Rng(o as u64))))
        .collect();
    let mut blobs = vec![
        (400, longs(&[1024, 2048, 3072])),
        (420, longs(&[1024, 1024, 1024])),
    ];
    blobs.extend(strips);
    let f = le_tiff(&t, &blobs, 4096);
    let m = check_exact(&TiffFormat, &f);
    assert_eq!(m.confidence, Confidence::new(80));
}

#[test]
fn tiff_jpeg_strips_missing_their_soi_only_count_once_the_group_is_jpeg() {
    let mut t = image_tags(7);
    t.push((0x0111, 4, 3, 400));
    t.push((0x0117, 4, 3, 420));
    let table = vec![
        (400, longs(&[1024, 2048, 3072])),
        (420, longs(&[1024, 1024, 1024])),
    ];
    // The middle strip lost its SOI but the last one is intact: the
    // structure goes on past it, so it is noted, not cut.
    let mut blobs = table.clone();
    blobs.push((1024, fake_jpeg(16, 16, 1024, &mut Rng(1))));
    blobs.push((3072, fake_jpeg(16, 16, 1024, &mut Rng(3))));
    let m = measure(&TiffFormat, &le_tiff(&t, &blobs, 4096));
    assert_eq!((m.status, m.len), (ExtentStatus::Complete, 4096));
    assert_eq!(m.confidence, Confidence::new(70));
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("no JPEG data (FF D8) at 1 offset(s)")),
        "{:?}",
        m.notes
    );
    // The last strips are gone: the file ends at the first of them.
    let mut blobs = table.clone();
    blobs.push((1024, fake_jpeg(16, 16, 1024, &mut Rng(1))));
    let m = measure(&TiffFormat, &le_tiff(&t, &blobs, 4096));
    assert_eq!((m.status, m.len), (ExtentStatus::Truncated, 2048));
    // A group whose first strip is not a JPEG stream says nothing.
    let m = measure(&TiffFormat, &le_tiff(&t, &table, 4096));
    assert_eq!((m.status, m.len), (ExtentStatus::Complete, 4096));
    assert_eq!(m.confidence, Confidence::new(80));
}

#[test]
fn tiff_strip_holding_an_unreferenced_file_start_is_cut_there() {
    let mut t = image_tags(1);
    t.push((0x0111, 4, 1, 1024));
    t.push((0x0117, 4, 1, 3072));
    for (what, blob) in [
        ("JPEG", jfif()),
        ("TIFF", b"II*\0\x08\0\0\0\x0C\0".to_vec()),
        ("MP4/MOV/HEIF", b"\0\0\0\x18ftypheic\0\0\0\0".to_vec()),
    ] {
        let f = le_tiff(&t, &[(2048, blob)], 4096);
        let m = measure(&TiffFormat, &f);
        assert_eq!(m.status, ExtentStatus::Truncated, "{what}");
        assert_eq!((m.len, m.err_offset), (2048, Some(2048)), "{what}");
        assert_eq!(m.confidence, Confidence::new(50), "{what}");
        assert!(
            m.notes
                .iter()
                .any(|n| n.contains(&format!("a {what} file starts at offset 2048"))),
            "{what}: {:?}",
            m.notes
        );
    }
}

#[test]
fn tiff_jpeg_start_inside_a_strip_is_fine_when_an_entry_points_at_it() {
    // Uncompressed strips whose pixel bytes happen to begin like a JFIF
    // stream: StripOffsets points there, so it is the file's own data.
    let mut t = image_tags(1);
    t.push((0x0111, 4, 2, 400));
    t.push((0x0117, 4, 2, 420));
    let blobs = vec![
        (400, longs(&[1024, 2048])),
        (420, longs(&[1024, 2048])),
        (2048, jfif()),
    ];
    let f = le_tiff(&t, &blobs, 4096);
    check_exact(&TiffFormat, &f);
    // Old-style JPEG: the strip holds the stream, JPEGInterchangeFormat
    // points at its SOI on the sector grid.
    let mut t = image_tags(6);
    t.push((0x0111, 4, 1, 1024));
    t.push((0x0117, 4, 1, 3072));
    t.push((0x0201, 4, 1, 2048));
    t.push((0x0202, 4, 1, 600));
    let f = le_tiff(&t, &[(2048, jfif())], 4096);
    check_exact(&TiffFormat, &f);
}

#[test]
fn tiff_jpeg_pointer_without_jpeg_data_ends_the_file() {
    // JPEGInterchangeFormat at 3072 holds filler: the file was cut short
    // before it.
    let mut t = image_tags(1);
    t.push((0x0111, 4, 1, 1024));
    t.push((0x0117, 4, 1, 1024));
    t.push((0x0201, 4, 1, 3072));
    t.push((0x0202, 4, 1, 1024));
    let f = le_tiff(&t, &[], 4096);
    let m = measure(&TiffFormat, &f);
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!((m.len, m.err_offset), (3072, Some(3072)));
    assert!(m.notes.iter().any(|n| n.contains("no JPEG data (FF D8)")));
    assert!(m.nested.is_empty());
}

#[test]
fn tiff_whose_directory_follows_the_data_keeps_its_extent() {
    // libtiff layout: strip first, IFD0 last. A file start inside the strip
    // lies before the file's own directory, so the bytes are still this
    // file's structure (an overwritten middle is the carver's conflict to
    // resolve); the extent is not cut.
    let ifd_at = 3584u32;
    let mut b = vec![0x5Au8; 4096];
    b[..4].copy_from_slice(b"II*\0");
    b[4..8].copy_from_slice(&ifd_at.to_le_bytes());
    let mut e = image_tags(1);
    e.push((0x0111, 4, 1, 512));
    e.push((0x0117, 4, 1, 3072));
    e.sort_by_key(|x| x.0);
    let mut p = ifd_at as usize;
    b[p..p + 2].copy_from_slice(&(e.len() as u16).to_le_bytes());
    p += 2;
    for (tag, typ, count, val) in e {
        b[p..p + 2].copy_from_slice(&tag.to_le_bytes());
        b[p + 2..p + 4].copy_from_slice(&typ.to_le_bytes());
        b[p + 4..p + 8].copy_from_slice(&count.to_le_bytes());
        b[p + 8..p + 12].copy_from_slice(&val.to_le_bytes());
        p += 12;
    }
    b[p..p + 4].fill(0);
    b.truncate(p + 4);
    let png = png_head();
    b[2048..2048 + png.len()].copy_from_slice(&png);
    let m = check_exact(&TiffFormat, &b);
    assert_eq!(m.len, b.len() as u64);
}

fn exif_with_thumbnail() -> Vec<u8> {
    let thumb = fake_jpeg(160, 120, 3000, &mut Rng(4));
    build(false, |o| {
        (
            tiff_header(false, o[0] as u32),
            vec![
                Item::ifd(
                    vec![
                        ascii(0x010F, "NIKON CORPORATION"),
                        ascii(0x0110, "NIKON Z 6"),
                        short(0x0112, 1),
                        long(0x8769, o[1] as u32),
                    ],
                    o[2] as u32,
                ),
                Item::ifd(vec![ascii(0x9003, "2024:01:02 03:04:05")], 0),
                Item::ifd(
                    vec![
                        short(0x0103, 6),
                        long(0x0201, o[3] as u32),
                        long(0x0202, thumb.len() as u32),
                    ],
                    0,
                ),
                Item::Blob(thumb.clone()),
            ],
        )
    })
    .0
}

fn assert_exif_block(m: &Measured, what: &str) {
    assert_eq!(m.confidence, Confidence::LOW, "{what}");
    assert!(
        m.notes.iter().any(|n| n == "EXIF block without image data"),
        "{what}: {:?}",
        m.notes
    );
}

#[test]
fn a_bare_exif_block_is_not_a_photo() {
    let plain = exif_tiff(false, "Canon", "Canon EOS R5", 6, "2024:01:02 03:04:05");
    let m = measure(&TiffFormat, &plain);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.len, plain.len() as u64);
    assert_exif_block(&m, "plain");
    let be = exif_tiff(true, "Canon", "Canon EOS R5", 6, "2024:01:02 03:04:05");
    assert_exif_block(&measure(&TiffFormat, &be), "big-endian");
    // An IFD1 thumbnail does not make it a picture, under any variant.
    let thumb = exif_with_thumbnail();
    let m = measure(&TiffFormat, &thumb);
    assert_eq!(m.len, thumb.len() as u64);
    assert_eq!(m.nested.len(), 1);
    assert_exif_block(&m, "thumbnail");
    assert_exif_block(&measure(&NefFormat, &thumb), "as NEF");
    // Real TIFFs and RAWs keep their scores.
    for s in [
        raws::tiled_tiff(),
        raws::nested_tiff(),
        raws::cr2(),
        raws::nef(false),
    ] {
        let f = formats().into_iter().find(|f| f.id() == s.id).unwrap();
        let m = measure(f.as_ref(), &s.bytes);
        assert!(m.confidence.value() >= 80, "{}: {:?}", s.name, m.notes);
        assert!(
            !m.notes.iter().any(|n| n.contains("EXIF block")),
            "{}",
            s.name
        );
    }
}
