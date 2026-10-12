//! Streaming: files of a gigabyte or more are measured from their headers
//! (or a bounded walk) without reading them. Sparse files keep the disk and
//! the clock out of it.

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat};
use diskrescue_core::source::{FileSource, SharedSource};
use diskrescue_stills::{BmpFormat, GifFormat, PsdFormat, TiffFormat};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn sparse(name: &str, head: &[u8], total: u64) -> (SharedSource, std::path::PathBuf) {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(head).unwrap();
    f.set_len(total).unwrap();
    drop(f);
    (Arc::new(FileSource::open(&path).unwrap()), path)
}

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    let d = t.elapsed();
    assert!(d < Duration::from_secs(10), "{label} took {d:?}");
    r
}

#[test]
fn gigabyte_bmp_psd_and_tiff_are_measured_without_reading_the_pixels() {
    // BMP: 16000 x 16000 x 24 bit = 768 MB of pixels.
    let (w, h) = (16_000u32, 16_000i32);
    let stride = (w as u64 * 24).div_ceil(32) * 4;
    let total = 54 + stride * h as u64;
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&(total as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&w.to_le_bytes());
    bmp.extend_from_slice(&h.to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.resize(54, 0);
    let (src, path) = sparse("sparse.bmp", &bmp, total);
    let m = timed("bmp", || {
        BmpFormat.measure(&src, 0, u64::MAX).unwrap().unwrap()
    });
    assert_eq!((m.len, m.status), (total, ExtentStatus::Complete));
    let _ = std::fs::remove_file(path);

    // PSD: 3 channels, 20000 x 20000, 8 bit raw = 1.2 GB.
    let mut psd = b"8BPS\0\x01\0\0\0\0\0\0\0\x03".to_vec();
    psd.extend_from_slice(&20_000u32.to_be_bytes());
    psd.extend_from_slice(&20_000u32.to_be_bytes());
    psd.extend_from_slice(&[0, 8, 0, 3]);
    psd.extend_from_slice(&[0; 12]); // three empty sections
    psd.extend_from_slice(&[0, 0]); // raw
    let total = psd.len() as u64 + 3 * 20_000 * 20_000;
    let (src, path) = sparse("sparse.psd", &psd, total);
    let m = timed("psd", || {
        PsdFormat.measure(&src, 0, u64::MAX).unwrap().unwrap()
    });
    assert_eq!((m.len, m.status), (total, ExtentStatus::Complete));
    let _ = std::fs::remove_file(path);

    // TIFF: one 1.5 GB strip.
    let strip: u64 = 1_500_000_000;
    let (bytes, _) = build(false, |o| {
        let _ = o;
        (
            tiff_header(false, 8),
            vec![Item::ifd(
                vec![
                    long(0x0100, 30_000),
                    long(0x0101, 50_000),
                    short(0x0102, 8),
                    short(0x0103, 1),
                    short(0x0106, 1),
                    long(0x0111, 200),
                    long(0x0117, strip as u32),
                ],
                0,
            )],
        )
    });
    let (src, path) = sparse("sparse.tif", &bytes, 200 + strip);
    let m = timed("tiff", || {
        TiffFormat.measure(&src, 0, u64::MAX).unwrap().unwrap()
    });
    assert_eq!((m.len, m.status), (200 + strip, ExtentStatus::Complete));
    // A limit below the strip end caps the result without reading it.
    let m = TiffFormat.measure(&src, 0, 1 << 20).unwrap().unwrap();
    assert_eq!((m.len, m.status), (1 << 20, ExtentStatus::Capped));
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_hundred_megabyte_gif_streams_through_in_bounded_memory() {
    // 100 MB of 255-byte sub-blocks in one frame.
    let mut g = b"GIF89a".to_vec();
    g.extend_from_slice(&[10, 0, 10, 0, 0, 0, 0]);
    g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 10, 0, 10, 0, 0, 8]);
    let block: Vec<u8> = std::iter::once(255u8)
        .chain((0..255).map(|i| i as u8))
        .collect();
    let n = 100 * 1024 * 1024 / 256;
    let mut f =
        std::fs::File::create(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("big.gif"))
            .unwrap();
    f.write_all(&g).unwrap();
    for _ in 0..n {
        f.write_all(&block).unwrap();
    }
    f.write_all(&[0, 0x3B]).unwrap();
    drop(f);
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("big.gif");
    let src: SharedSource = Arc::new(FileSource::open(&path).unwrap());
    let size = src.size();
    let m = timed("gif", || {
        GifFormat.measure(&src, 0, u64::MAX).unwrap().unwrap()
    });
    assert_eq!((m.len, m.status), (size, ExtentStatus::Complete));
    // The limit stops a walk through a huge file early.
    let m = GifFormat.measure(&src, 0, 10 << 20).unwrap().unwrap();
    assert_eq!((m.len, m.status), (10 << 20, ExtentStatus::Capped));
    let _ = std::fs::remove_file(path);
}
