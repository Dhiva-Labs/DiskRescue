//! Thumbnails of JPEG, PNG and a CR2-like RAW with embedded previews.

use diskrescue_app::results::thumbnail;
use diskrescue_fixtures::media::{self, ExifOpts, JpegOpts, PngOpts, TiffOpts};

#[test]
fn jpeg_and_png_are_decoded_and_shrunk() {
    let dir = tempfile::tempdir().unwrap();
    let jpg = dir.path().join("photo.jpg");
    std::fs::write(
        &jpg,
        media::jpeg(&JpegOpts {
            width: 640,
            height: 480,
            ..JpegOpts::default()
        })
        .unwrap(),
    )
    .unwrap();
    let t = thumbnail(&jpg, 128).expect("JPEG thumbnail");
    assert_eq!((t.width(), t.height()), (128, 96));

    let png = dir.path().join("shot.png");
    std::fs::write(
        &png,
        media::png(&PngOpts {
            width: 300,
            height: 600,
            ..PngOpts::default()
        }),
    )
    .unwrap();
    let t = thumbnail(&png, 100).expect("PNG thumbnail");
    assert_eq!((t.width(), t.height()), (50, 100));
    // Small pictures are not enlarged.
    let t = thumbnail(&png, 1000).unwrap();
    assert_eq!((t.width(), t.height()), (300, 600));
}

#[test]
fn raw_files_use_their_embedded_preview() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("IMG_0006.CR2");
    std::fs::write(
        &raw,
        media::cr2_like(&TiffOpts {
            width: 512,
            height: 384,
            seed: 6,
            ..TiffOpts::default()
        }),
    )
    .unwrap();
    // The 512x384 preview for a large thumbnail ...
    let t = thumbnail(&raw, 400).expect("preview of the RAW file");
    assert_eq!((t.width(), t.height()), (400, 300));
    // ... and the 128x96 one is enough for a small one.
    let t = thumbnail(&raw, 64).expect("thumbnail of the RAW file");
    assert_eq!((t.width(), t.height()), (64, 48));
}

#[test]
fn a_damaged_jpeg_still_gets_a_thumbnail() {
    let dir = tempfile::tempdir().unwrap();
    let data = media::jpeg(&JpegOpts {
        width: 640,
        height: 480,
        exif: Some(ExifOpts {
            thumbnail: true,
            ..ExifOpts::default()
        }),
        ..JpegOpts::default()
    })
    .unwrap();
    // Cut short: either the decoder keeps the rows it has, or the EXIF
    // thumbnail found by the JPEG measurement is used.
    let p = dir.path().join("cut.jpg");
    std::fs::write(&p, &data[..data.len() * 6 / 10]).unwrap();
    let t = thumbnail(&p, 64).expect("a thumbnail of the damaged JPEG");
    assert!(t.width() <= 64 && t.height() <= 64);
}

#[test]
fn videos_and_junk_have_no_thumbnail() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("notes.txt");
    std::fs::write(&junk, b"just some text").unwrap();
    assert!(thumbnail(&junk, 64).is_none());
    assert!(thumbnail(&dir.path().join("missing.jpg"), 64).is_none());
    let empty = dir.path().join("empty.jpg");
    std::fs::write(&empty, b"").unwrap();
    assert!(thumbnail(&empty, 64).is_none());
}
