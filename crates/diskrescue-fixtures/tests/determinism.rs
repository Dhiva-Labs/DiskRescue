//! Equal specs give byte-identical images (except NTFS, whose tools stamp
//! the current time into the MFT) and equal media bytes.

mod common;

use common::*;
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::media::{self, ExifOpts, JpegOpts, PngOpts};

fn spec(kind: FsType, size: u64) -> FsSpec {
    let mut s = FsSpec::new(kind, size);
    s.label = Some("DET".into());
    s.files
        .push(FileSpec::new("a/IMG_0001.JPG", data(30_000, 1)));
    s.files
        .push(FileSpec::new("b.bin", data(3_000, 2)).mtime(1_600_000_000));
    s.files.push(FileSpec::new("c.bin", data(3_000, 3)));
    s.deleted.push("c.bin".into());
    s
}

#[test]
fn filesystem_images_are_reproducible() {
    let cases = [
        (FsType::Fat12, 8u64 << 20, "mkfs.vfat"),
        (FsType::Fat16, 40 << 20, "mkfs.vfat"),
        (FsType::Fat32, 96 << 20, "mkfs.vfat"),
        (FsType::ExFat, 64 << 20, "mkfs.exfat"),
        (FsType::Ext4, 32 << 20, "mkfs.ext4"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let mut done = Vec::new();
    for (kind, size, tool) in cases {
        if skip_unless(&[tool, "debugfs"]) {
            continue;
        }
        build(
            &spec(kind, size),
            &dir.path().join(format!("{kind:?}-1.img")),
        )
        .unwrap();
        done.push((kind, size));
    }
    // Real time must pass between the builds for the test to mean anything.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    for (kind, size) in done {
        build(
            &spec(kind, size),
            &dir.path().join(format!("{kind:?}-2.img")),
        )
        .unwrap();
        let a = std::fs::read(dir.path().join(format!("{kind:?}-1.img"))).unwrap();
        let b = std::fs::read(dir.path().join(format!("{kind:?}-2.img"))).unwrap();
        assert!(a == b, "{kind:?} images differ");
    }
    // The built-in FAT formatter is reproducible as well.
    let mut s = spec(FsType::Fat32, 96 << 20);
    s.native_format = true;
    build(&s, &dir.path().join("n1.img")).unwrap();
    build(&s, &dir.path().join("n2.img")).unwrap();
    assert!(
        std::fs::read(dir.path().join("n1.img")).unwrap()
            == std::fs::read(dir.path().join("n2.img")).unwrap()
    );
}

#[test]
fn media_generators_are_deterministic() {
    let o = JpegOpts {
        width: 200,
        height: 100,
        exif: Some(ExifOpts::default()),
        seed: 5,
        ..Default::default()
    };
    assert_eq!(media::jpeg(&o).unwrap(), media::jpeg(&o).unwrap());
    let p = PngOpts {
        seed: 5,
        interlaced: true,
        ..Default::default()
    };
    assert_eq!(media::png(&p), media::png(&p));
    assert_ne!(
        media::png(&p),
        media::png(&PngOpts {
            seed: 6,
            ..p.clone()
        })
    );
}
