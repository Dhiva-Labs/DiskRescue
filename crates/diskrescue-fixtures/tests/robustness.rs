//! Hostile and absurd inputs: nothing may panic, hang or exhaust memory.

mod common;

use common::*;
use diskrescue_fixtures::damage;
use diskrescue_fixtures::disk::guid_from_str;
use diskrescue_fixtures::fs::{build, FileSpec, FsSpec, FsType};
use diskrescue_fixtures::media::{
    self, heic, jpeg, AudioCodec, Container, JpegOpts, PngOpts, StillOpts, TiffOpts, VideoCodec,
    VideoOpts,
};
use diskrescue_fixtures::rng::Rng;

fn random_buffers() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(99);
    let mut v = vec![
        vec![],
        vec![0],
        vec![0xFF; 3],
        vec![0; 100],
        vec![0xFF; 100],
    ];
    for len in [1usize, 7, 8, 11, 12, 15, 16, 31, 64, 200, 1000, 5000] {
        for _ in 0..20 {
            v.push(rng.bytes(len));
        }
    }
    v
}

#[test]
fn damage_helpers_survive_random_input() {
    let mut rng = Rng::new(1);
    for buf in random_buffers() {
        let _ = damage::top_level_boxes(&buf);
        let out = damage::strip_box(&buf, b"moov");
        assert!(out.len() <= buf.len());
        let _ = damage::png_chunks(&buf);
        let mut b2 = buf.clone();
        let _ = damage::png_corrupt_crc(&mut b2, b"IDAT", 0);
        let _ = damage::png_strip_chunk(&buf, b"IDAT", 0);
        let _ = damage::avi_strip_idx1(&buf);
        let _ = media::jpeg::segments(&buf);
        let _ = media::jpeg::splice_after_soi(&buf, &[0xFF, 0xE1, 0, 2]);
        // Absurd ranges.
        let mut b3 = buf.clone();
        damage::zero_range(&mut b3, usize::MAX, usize::MAX);
        damage::zero_range(&mut b3, 3, usize::MAX);
        damage::flip_bytes(&mut b3, rng.next_u64(), usize::MAX, usize::MAX);
        damage::flip_bytes(&mut b3, 5, 1_000_000, 0);
        damage::overwrite_prefix(&mut b3, usize::MAX, 4);
        let _ = damage::remove_range(&buf, usize::MAX, usize::MAX);
        let _ = damage::remove_range(&buf, 2, usize::MAX);
        let _ = damage::truncate(&buf, usize::MAX);
    }
    // Real files with mutations: the box walker never indexes out of bounds.
    let mut mp4 = vec![0u8; 8];
    mp4[3] = 8;
    mp4[4..8].copy_from_slice(b"free");
    for seed in 0..200u64 {
        let mut m = mp4.clone();
        m.extend(Rng::new(seed).bytes(64));
        let _ = damage::top_level_boxes(&m);
    }
}

#[test]
fn hevc_wrapper_survives_random_and_truncated_streams() {
    for buf in random_buffers() {
        let _ = heic::split_annex_b(&buf);
        assert!(heic::wrap_hevc_still(&buf, 16, 16).is_err() || buf.len() > 20);
    }
    // Start codes with absurd content.
    let nasty: Vec<u8> = [0u8, 0, 1].iter().cycle().take(300).copied().collect();
    let _ = heic::split_annex_b(&nasty);
    let _ = heic::wrap_hevc_still(
        &[0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1],
        8,
        8,
    );
    if skip_unless(&["ffmpeg"]) || !diskrescue_fixtures::tools::ffmpeg_has_encoder("libx265") {
        return;
    }
    let good = media::heic(&media::HeicOpts::default()).unwrap();
    // Cut the real HEIC anywhere: still a byte vector, walker is safe.
    for cut in (0..good.len()).step_by(97) {
        let _ = damage::top_level_boxes(&good[..cut]);
    }
}

#[test]
fn guid_parser_rejects_junk() {
    for s in [
        "",
        "x",
        "EBD0A0A2",
        "ZZZZZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZZZZZZZZZ",
        "é".repeat(40).as_str(),
    ] {
        assert!(guid_from_str(s).is_none(), "{s:?}");
    }
}

#[test]
fn generators_clamp_absurd_dimensions() {
    // 100000 x 100000 would need tens of gigabytes; the canvas clamps.
    let (w, h) = media::canvas::clamp_dims(100_000, 100_000);
    assert!(w as u64 * h as u64 <= media::canvas::MAX_PIXELS);
    let p = media::png(&PngOpts {
        width: u32::MAX,
        height: 1,
        ..Default::default()
    });
    assert_eq!(&p[..4], &[0x89, b'P', b'N', b'G']);
    let g = media::gif(&StillOpts {
        width: 0,
        height: 0,
        seed: 0,
    });
    assert_eq!(&g[..3], b"GIF");
    let t = media::tiff(&TiffOpts {
        width: 0,
        height: u32::MAX,
        rows_per_strip: 0,
        ..Default::default()
    });
    assert_eq!(&t[..2], b"II");
    let j = jpeg(&JpegOpts {
        width: 0,
        height: 0,
        quality: 0,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(&j[..2], &[0xFF, 0xD8]);
    let j = jpeg(&JpegOpts {
        width: 1,
        height: 1,
        quality: 255,
        exif: Some(Default::default()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(&j[..2], &[0xFF, 0xD8]);
}

#[test]
fn video_options_are_validated_not_executed_blindly() {
    let bad_fps = VideoOpts {
        fps: 0,
        ..Default::default()
    };
    assert!(media::video(&bad_fps).is_err());
    let bad_len = VideoOpts {
        seconds: -1.0,
        ..Default::default()
    };
    assert!(media::video(&bad_len).is_err());
    let nan = VideoOpts {
        seconds: f32::NAN,
        ..Default::default()
    };
    // NaN is not positive: rejected before ffmpeg runs.
    assert!(media::video(&nan).is_err());
    assert!(media::video(&VideoOpts {
        container: Container::MpegPs,
        video: VideoCodec::H264,
        audio: AudioCodec::Aac,
        ..Default::default()
    })
    .is_err());
}

#[test]
fn filesystem_specs_with_absurd_sizes_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("x.img");
    for kind in [
        FsType::Fat12,
        FsType::Fat16,
        FsType::Fat32,
        FsType::ExFat,
        FsType::Ntfs,
        FsType::Ext4,
    ] {
        for size in [
            512u64,
            4096,
            100 << 10,
            1 << 50,
            i64::MAX as u64 / 512 * 512,
            (u64::MAX / 512) * 512,
        ] {
            let mut s = FsSpec::new(kind, size);
            s.files.push(FileSpec::new("a.txt", vec![1, 2, 3]));
            // Either a clean error or success; never a panic or a hang. A
            // size beyond what a file can be is always an error.
            let r = build(&s, &img);
            if size > diskrescue_fixtures::fs::MAX_IMAGE_SIZE {
                assert!(r.is_err(), "{kind:?} {size}");
            }
            let _ = std::fs::remove_file(&img);
        }
        // Absurd cluster sizes.
        for cs in [1u32, 3, 511, 1 << 31, u32::MAX] {
            let mut s = FsSpec::new(kind, 64 << 20).cluster_size(cs);
            s.files.push(FileSpec::new("a.txt", vec![1]));
            let _ = build(&s, &img);
        }
        // Absurd paths.
        for path in [
            "",
            "/",
            "//",
            ".",
            "..",
            &"a/".repeat(60),
            &"x".repeat(5000),
            "a\0b",
        ] {
            let mut s = FsSpec::new(kind, 64 << 20);
            s.files.push(FileSpec::new(path, vec![]));
            let _ = build(&s, &img);
        }
    }
}

#[test]
fn deep_and_wide_trees() {
    if skip_unless(&["fsck.vfat", "fsck.exfat"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // 40 levels deep, then one file.
    let deep = (0..40)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    for kind in [FsType::Fat32, FsType::ExFat] {
        let img = dir.path().join("deep.img");
        let mut s = FsSpec::new(kind, 64 << 20);
        s.files
            .push(FileSpec::new(format!("{deep}/leaf.bin"), data(5000, 3)));
        let rep = build(&s, &img).unwrap();
        let tool = if kind == FsType::Fat32 {
            "fsck.vfat"
        } else {
            "fsck.exfat"
        };
        let (ok, t) = check(tool, &["-n", img.to_str().unwrap()]);
        assert!(ok, "{kind:?}: {t}");
        assert_eq!(
            read_extents(
                &img,
                &rep.find(&format!("{deep}/leaf.bin")).unwrap().extents
            ),
            data(5000, 3)
        );
    }
}
