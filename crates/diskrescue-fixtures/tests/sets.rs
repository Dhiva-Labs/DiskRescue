mod common;

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use common::*;
use diskrescue_core::util::hash::blake3_hex;
use diskrescue_fixtures::sets::{self, Recoverability, Scenario, TruthFile};
use diskrescue_fixtures::tools;

/// Every extent of every file with extents hashes to the recorded value.
fn check_extents(sc: &Scenario) {
    let img = sc.disk_image.as_ref().unwrap();
    let mut n = 0;
    for f in &sc.files {
        if f.extents.is_empty() {
            continue;
        }
        let data = read_extents(img, &f.extents);
        assert_eq!(data.len() as u64, f.size, "{}", f.path);
        assert_eq!(blake3_hex(&data), f.blake3, "{}", f.path);
        n += 1;
    }
    assert!(n >= 3, "at least three files with extents");
}

fn partition_copy(sc: &Scenario, dir: &Path, idx: usize) -> PathBuf {
    let (start, size) = sc.partitions[idx];
    let mut src = std::fs::File::open(sc.disk_image.as_ref().unwrap()).unwrap();
    src.seek(SeekFrom::Start(start)).unwrap();
    let out = dir.join(format!("part{idx}.img"));
    let mut dst = std::fs::File::create(&out).unwrap();
    std::io::copy(&mut (&mut src).take(size), &mut dst).unwrap();
    out
}

fn roundtrip_json(sc: &Scenario) {
    let loaded = Scenario::load(&sc.root).unwrap();
    assert_eq!(&loaded, sc);
    assert!(sc.to_json().contains("\"expect\""));
}

fn find<'a>(sc: &'a Scenario, p: &str) -> &'a TruthFile {
    sc.file(p).unwrap_or_else(|| panic!("truth lacks {p}"))
}

#[test]
fn camera_sd_card_scenario() {
    if skip_unless(&["ffmpeg", "fsck.vfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::camera_sd_card(dir.path()).unwrap();
    roundtrip_json(&sc);
    let img = sc.disk_image.clone().unwrap();
    let (ok, text) = check("fsck.vfat", &["-n", "-l", img.to_str().unwrap()]);
    assert!(ok, "{text}");
    check_extents(&sc);
    let deleted: Vec<&str> = sc
        .files
        .iter()
        .filter(|f| f.deleted)
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(
        deleted,
        ["DCIM/100CANON/IMG_0004.JPG", "DCIM/100CANON/MVI_0009.MP4"]
    );
    for f in &sc.files {
        assert_eq!(
            f.expect,
            if f.deleted {
                Recoverability::DeletedIntact
            } else {
                Recoverability::Live
            }
        );
        assert_eq!(
            f.deleted,
            !text.contains(f.path.rsplit('/').next().unwrap()),
            "{}",
            f.path
        );
        assert!(f.contiguous, "{} is contiguous", f.path);
        assert_eq!(f.extents[0].0 % 4096, 0, "4 KiB aligned");
    }
    let p = find(&sc, "DCIM/100CANON/IMG_0001.JPG");
    assert_eq!(p.exif_date.as_deref(), Some("2024:07:11 10:21:31"));
    let bytes = read_extents(&img, &p.extents);
    assert_eq!(&bytes[..4], &[0xFF, 0xD8, 0xFF, 0xE1]);
    // The videos probe cleanly straight out of the image.
    let v = read_extents(&img, &find(&sc, "DCIM/100CANON/MVI_0003.MP4").extents);
    assert!(ffprobe_streams(&v, "v.mp4").is_some());
    let del_v = read_extents(&img, &find(&sc, "DCIM/100CANON/MVI_0009.MP4").extents);
    assert!(
        ffprobe_streams(&del_v, "d.mp4").is_some(),
        "deleted video data is intact"
    );
    let cr2 = read_extents(&img, &find(&sc, "DCIM/100CANON/CRW_0006.CR2").extents);
    assert_eq!(&cr2[8..10], b"CR");
}

#[test]
fn phone_exfat_scenario() {
    if skip_unless(&["ffmpeg", "mkfs.exfat", "fsck.exfat", "sgdisk"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::phone_exfat(dir.path()).unwrap();
    roundtrip_json(&sc);
    check_extents(&sc);
    let img = sc.disk_image.clone().unwrap();
    let (ok, t) = check("sgdisk", &["-v", img.to_str().unwrap()]);
    assert!(ok && t.contains("No problems"), "{t}");
    let part = partition_copy(&sc, dir.path(), 0);
    let (ok, t) = check("fsck.exfat", &["-n", part.to_str().unwrap()]);
    assert!(ok && t.contains("clean"), "{t}");
    let heic = find(&sc, "DCIM/Camera/IMG_20240503_090000.HEIC");
    let hbytes = read_extents(&img, &heic.extents);
    if heic.kind == "heic" {
        assert_eq!(&hbytes[4..12], b"ftypheic");
    } else {
        assert_eq!(&hbytes[..2], &[0xFF, 0xD8]);
    }
    assert!(sc.files.iter().any(|f| f.deleted && f.kind == "jpeg"));
    assert!(sc.files.iter().any(|f| f.deleted && f.kind == "png"));
    let mp4 = read_extents(&img, &find(&sc, "Movies/VID_20240502_120000.mp4").extents);
    let streams = ffprobe_streams(&mp4, "m.mp4").unwrap();
    if tools::ffmpeg_has_encoder("libx265") {
        assert!(streams.contains(&"hevc,video".to_string()), "{streams:?}");
    }
}

#[test]
fn ntfs_disk_scenario() {
    if skip_unless(&[
        "ffmpeg",
        "mkntfs",
        "ntfsfix",
        "ntfsls",
        "ntfscat",
        "ntfsundelete",
        "sfdisk",
    ]) || !diskrescue_fixtures::fs::ntfs_mount_available()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::ntfs_disk(dir.path()).unwrap();
    roundtrip_json(&sc);
    check_extents(&sc);
    let img = sc.disk_image.clone().unwrap();
    let (ok, t) = check("sfdisk", &["--list", img.to_str().unwrap()]);
    assert!(ok && t.contains("dos"), "{t}");
    let part = partition_copy(&sc, dir.path(), 0);
    let (ok, t) = check("ntfsfix", &["-n", part.to_str().unwrap()]);
    assert!(ok && t.contains("processed successfully"), "{t}");
    // Live files via ntfscat equal the truth hashes.
    for f in sc.files.iter().filter(|f| !f.deleted && f.kind != "stream") {
        let out = tools::run(
            "ntfscat",
            &[part.to_str().unwrap(), &format!("/{}", f.path)],
        )
        .unwrap();
        assert_eq!(blake3_hex(&out.stdout), f.blake3, "{}", f.path);
    }
    let del: Vec<&TruthFile> = sc.files.iter().filter(|f| f.deleted).collect();
    assert_eq!(del.len(), 1);
    let (_, t) = check("ntfsundelete", &["-s", part.to_str().unwrap()]);
    assert!(t.contains(del[0].path.rsplit('/').next().unwrap()), "{t}");
    let resident = find(&sc, "Documents/readme.txt");
    assert!(resident.extents.is_empty() && !resident.contiguous);
    assert_eq!(
        find(&sc, "Pictures/2023/Summer/IMG_0100.jpg:Zone.Identifier").kind,
        "stream"
    );
}

#[test]
fn ext4_disk_scenario() {
    if skip_unless(&["ffmpeg", "mkfs.ext4", "debugfs", "e2fsck", "sgdisk"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::ext4_disk(dir.path()).unwrap();
    roundtrip_json(&sc);
    check_extents(&sc);
    let img = sc.disk_image.clone().unwrap();
    let (ok, t) = check("sgdisk", &["-v", img.to_str().unwrap()]);
    assert!(ok && t.contains("No problems"), "{t}");
    let part = partition_copy(&sc, dir.path(), 0);
    let (ok, t) = check("e2fsck", &["-fn", part.to_str().unwrap()]);
    assert!(ok, "{t}");
    assert_eq!(sc.files.iter().filter(|f| f.deleted).count(), 2);
    let (_, lsdel) = check("debugfs", &["-R", "lsdel", part.to_str().unwrap()]);
    assert!(lsdel.contains("2 deleted inodes found"), "{lsdel}");
}

#[test]
fn damaged_media_scenario() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::damaged_media(dir.path()).unwrap();
    roundtrip_json(&sc);
    for f in &sc.files {
        let bytes = std::fs::read(dir.path().join(&f.path)).unwrap();
        assert_eq!(bytes.len() as u64, f.size);
        assert_eq!(blake3_hex(&bytes), f.blake3);
        if let Some(o) = &f.original {
            let orig = sc.file(o).unwrap_or_else(|| panic!("original {o} listed"));
            assert_eq!(orig.expect, Recoverability::Original);
            assert!(orig.size >= f.size || f.kind == "mp4" || f.kind == "avi" || f.kind == "jpeg");
            assert_ne!(orig.blake3, f.blake3, "{} must actually be damaged", f.path);
        }
        if let Some(d) = &f.donor {
            assert_eq!(sc.file(d).unwrap().expect, Recoverability::Donor);
        }
    }
    let read = |p: &str| std::fs::read(dir.path().join(p)).unwrap();
    // Healthy twins and donors are valid; the broken files are not (where a tool can tell).
    for p in [
        "originals/no_moov.mp4",
        "donors/donor.mp4",
        "originals/truncated.mp4",
        "originals/no_idx1.avi",
    ] {
        assert!(
            ffprobe_streams(&read(p), p.rsplit('/').next().unwrap()).is_some(),
            "{p}"
        );
    }
    assert!(ffprobe_streams(&read("no_moov.mp4"), "n.mp4").is_none());
    // JPEG: SOI destroyed, scan data intact.
    let z = read("zeroed_header.jpg");
    assert_eq!(&z[..8], &[0u8; 8]);
    let o = read("originals/zeroed_header.jpg");
    assert_eq!(
        z[z.len() - 1000..],
        o[o.len() - 1000..],
        "scan data untouched"
    );
    // PNG: only the CRC differs.
    let (b, bo) = (read("bad_crc.png"), read("originals/bad_crc.png"));
    assert_eq!(b.len(), bo.len());
    assert_eq!(b.iter().zip(&bo).filter(|(x, y)| x != y).count(), 1);
    // Donor shares the stream parameters of its damaged twin.
    let t = |p: &str| {
        tools::run_stdout(
            "ffprobe",
            &[
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,profile,width,height,pix_fmt,sample_rate",
                "-of",
                "csv=p=0",
                dir.path().join(p).to_str().unwrap(),
            ],
        )
        .unwrap()
    };
    assert_eq!(t("donors/donor.mp4"), t("originals/no_moov.mp4"));
    assert_ne!(read("donors/donor.mp4"), read("originals/no_moov.mp4"));
}

#[test]
fn fragmented_scenario() {
    if skip_unless(&["fsck.vfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sc = sets::fragmented_fat32(dir.path()).unwrap();
    let img = sc.disk_image.clone().unwrap();
    let (ok, t) = check("fsck.vfat", &["-n", img.to_str().unwrap()]);
    assert!(ok, "{t}");
    let a = find(&sc, "DCIM/IMG_A.JPG");
    let b = find(&sc, "DCIM/IMG_B.JPG");
    assert!(a.deleted && !a.contiguous && a.extents.len() == 2);
    assert_eq!(a.expect, Recoverability::PartialOnly);
    assert_eq!(
        b.extents[0].0,
        a.extents[0].0 + a.extents[0].1.div_ceil(4096) * 4096
    );
    assert_eq!(blake3_hex(&read_extents(&img, &a.extents)), a.blake3);
    // The first fragment alone is a truncated JPEG.
    let first = read_extents(&img, &a.extents[..1]);
    assert_eq!(&first[..2], &[0xFF, 0xD8]);
    assert_ne!(&first[first.len() - 2..], &[0xFF, 0xD9]);
}

#[test]
fn unknown_set_name_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let e = sets::build_named("nope", dir.path()).unwrap_err();
    assert!(e.to_string().contains("unknown set"));
    assert!(!e.is_missing());
}

#[test]
fn gen_fixtures_binary_runs() {
    if skip_unless(&["ffmpeg"]) {
        return;
    }
    let exe = env!("CARGO_BIN_EXE_gen-fixtures");
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(exe)
        .arg(dir.path())
        .args(["--set", "damaged-media"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("damaged-media/truth.json").exists());
    let bad = std::process::Command::new(exe).output().unwrap();
    assert!(!bad.status.success());
    let bad = std::process::Command::new(exe)
        .arg(dir.path())
        .args(["--set", "bogus"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}
