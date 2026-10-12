//! Carving measurement of MP4/MOV/3GP/M4A files.

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, NestedRole};
use diskrescue_core::Source;
use diskrescue_isobmff::{formats, Mp4Format};

const CLEAN: &[&str] = &[
    "h264",
    "h264_faststart",
    "h265",
    "mov",
    "mov_pcm",
    "3gp",
    "m4a",
    "video_only",
    "h264_bf0",
];

#[test]
fn exact_length_of_clean_files() {
    if !ffmpeg_or_skip() {
        return;
    }
    let f = Mp4Format;
    for name in CLEAN {
        let path = fixture(name);
        let bytes = std::fs::read(&path).unwrap();
        let len = bytes.len() as u64;
        // Alone, followed by random data, inside an image at an offset, and
        // followed by another MP4.
        let mut img = vec![0u8; 4096];
        img.extend_from_slice(&bytes);
        img.extend(noise(70_000, 7));
        let mut two = bytes.clone();
        two.extend_from_slice(&std::fs::read(fixture("h264_faststart")).unwrap());
        for (src, start) in [(mem(bytes.clone()), 0u64), (mem(img), 4096), (mem(two), 0)] {
            let m = f
                .measure(&src, start, src.size() - start)
                .unwrap()
                .unwrap_or_else(|| panic!("{name} not recognised"));
            assert_eq!(m.len, len, "{name}: {m:?}");
            assert_eq!(m.status, ExtentStatus::Complete, "{name}: {m:?}");
            assert!(m.confidence.value() >= 60, "{name}: {m:?}");
        }
    }
}

#[test]
fn metadata_and_extension() {
    if !ffmpeg_or_skip() {
        return;
    }
    let f = Mp4Format;
    let expect = [
        ("h264", "mp4", Some("h264"), Some("aac")),
        ("h265", "mp4", Some("hevc"), Some("aac")),
        ("mov", "mov", Some("h264"), Some("aac")),
        ("mov_pcm", "mov", Some("h264"), Some("pcm_s16le")),
        ("3gp", "3gp", Some("h264"), Some("aac")),
        ("m4a", "m4a", None, Some("aac")),
    ];
    for (name, ext, v, a) in expect {
        let src = open(&fixture(name));
        let m = f.measure(&src, 0, src.size()).unwrap().unwrap();
        assert_eq!(
            m.meta.extra.get("extension").map(String::as_str),
            Some(ext),
            "{name}"
        );
        assert_eq!(m.meta.video_codec.as_deref(), v, "{name}");
        assert_eq!(m.meta.audio_codec.as_deref(), a, "{name}");
        let d = m.meta.duration_ms.unwrap();
        assert!((2900..=3100).contains(&d), "{name}: duration {d}");
        if v.is_some() {
            assert_eq!(m.meta.dimensions().as_deref(), Some("320x240"), "{name}");
        }
        assert!(
            m.meta.software.is_some() || name == "3gp" || name == "mov" || name == "mov_pcm",
            "{name}: {:?}",
            m.meta
        );
    }
}

#[test]
fn mdat_size_zero_and_truncation() {
    if !ffmpeg_or_skip() {
        return;
    }
    let f = Mp4Format;
    // moov first, mdat size 0: the tables give the real end even with
    // random data after the file.
    let fs = std::fs::read(fixture("h264_faststart")).unwrap();
    let mut z = zero_box_size(&fs, b"mdat");
    z.extend(noise(50_000, 3));
    let m = f
        .measure(&mem(z), 0, (fs.len() + 50_000) as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.len, fs.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);

    // moov at the end swallowed by a size-0 mdat: found by scanning.
    let tail = std::fs::read(fixture("h264")).unwrap();
    let z = zero_box_size(&tail, b"mdat");
    let m = f.measure(&mem(z), 0, tail.len() as u64).unwrap().unwrap();
    assert_eq!(m.len, tail.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);

    // No moov at all and mdat size 0: unknown length inside a bigger
    // source, truncated when the data ends.
    let no_moov = zero_box_size(&strip_box(&tail, b"moov"), b"mdat");
    let mut big = no_moov.clone();
    big.extend(vec![0u8; 100_000]);
    let src = mem(big);
    let window = no_moov.len() as u64 + 10_000;
    let m = f.measure(&src, 0, window).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.len, window);
    let m = f
        .measure(&mem(no_moov.clone()), 0, no_moov.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);

    // Truncated moov-at-end file: truncated; capped when the window ends.
    let cut = tail[..tail.len() / 2].to_vec();
    let m = f
        .measure(&mem(cut.clone()), 0, cut.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.len, cut.len() as u64);
    let m = f.measure(&mem(tail.clone()), 0, 20_000).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Capped);
    assert_eq!(m.len, 20_000);

    // Faststart file truncated: the moov says where the data should end.
    let cut = fs[..fs.len() * 2 / 3].to_vec();
    let m = f
        .measure(&mem(cut.clone()), 0, cut.len() as u64)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
}

#[test]
fn false_positives_are_rejected() {
    let f = Mp4Format;
    let mut decoy = b"\0\0\0\x20ftypjunk\0\0\0\0junkjunkjunkjunk".to_vec();
    decoy.extend(noise(5000, 9));
    assert!(f
        .measure(&mem(decoy.clone()), 0, decoy.len() as u64)
        .unwrap()
        .is_none());
    assert!(!f.probe(&decoy));
    // A plausible brand followed by garbage instead of boxes.
    let mut d2 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2".to_vec();
    d2.extend(noise(5000, 11));
    assert!(f
        .measure(&mem(d2.clone()), 0, d2.len() as u64)
        .unwrap()
        .is_none());
    // Only an ftyp.
    let d3 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2".to_vec();
    assert!(f
        .measure(&mem(d3.clone()), 0, d3.len() as u64)
        .unwrap()
        .is_none());
    // HEIC and CR3 brands are not movies.
    let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic\0\0\0\x08free".to_vec();
    assert!(f
        .measure(&mem(heic.clone()), 0, heic.len() as u64)
        .unwrap()
        .is_none());
    assert!(!f.probe(&heic));
    for n in [0usize, 1, 7, 8, 12, 100] {
        let b = noise(n, n as u64 + 1);
        let _ = f.measure(&mem(b.clone()), 0, n as u64).unwrap();
    }
}

#[test]
fn registry_and_nesting() {
    let fs = formats();
    let ids: Vec<_> = fs.iter().map(|f| f.id()).collect();
    assert_eq!(ids, vec!["mp4", "heic", "cr3"]);
    let mp4 = &fs[0];
    assert!(mp4.may_contain("jpeg") && mp4.may_contain("png") && mp4.may_contain("heic"));
    assert_eq!(mp4.priority(), 70);
    assert_eq!(fs[1].priority(), 70);
    assert_eq!(fs[2].priority(), 75);
    assert!(mp4.can_repair() && !fs[1].can_repair() && !fs[2].can_repair());
}

#[test]
fn cover_art_is_nested() {
    if !ffmpeg_or_skip() {
        return;
    }
    // Build a cover image with ffmpeg and attach it as covr.
    let jpg = scratch("cover.jpg");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64",
            "-frames:v",
            "1",
        ])
        .arg(&jpg)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let out = scratch("cover.m4a");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(fixture("m4a"))
        .arg("-i")
        .arg(&jpg)
        .args([
            "-map",
            "0",
            "-map",
            "1",
            "-c",
            "copy",
            "-disposition:v",
            "attached_pic",
            "-f",
            "ipod",
        ])
        .arg(&out)
        .status()
        .unwrap()
        .success();
    if !ok {
        eprintln!("skipped: ffmpeg cannot attach cover art");
        return;
    }
    let src = open(&out);
    let m = Mp4Format.measure(&src, 0, src.size()).unwrap().unwrap();
    assert_eq!(m.len, src.size());
    let cover: Vec<_> = m
        .nested
        .iter()
        .filter(|n| n.role == NestedRole::CoverArt)
        .collect();
    assert_eq!(cover.len(), 1, "{:?}", m.nested);
    let bytes = std::fs::read(&out).unwrap();
    let r = cover[0].range;
    assert_eq!(
        &bytes[r.start as usize..r.start as usize + 3],
        &[0xFF, 0xD8, 0xFF]
    );
    assert_eq!(cover[0].format.as_deref(), Some("jpeg"));
}
