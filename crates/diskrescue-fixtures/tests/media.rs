mod common;

use common::*;
use diskrescue_fixtures::damage;
use diskrescue_fixtures::media::{
    self, AudioCodec, Chroma, Container, ExifOpts, HeicOpts, JpegOpts, PngColor, PngOpts,
    StillOpts, TiffOpts, VideoCodec, VideoOpts,
};
use diskrescue_fixtures::tools;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn jpeg_baseline_and_exif_open_in_pillow() {
    if skip_unless(&["python3"]) || !tools::have_pillow() {
        eprintln!("skipped: Pillow missing");
        return;
    }
    let plain = media::jpeg(&JpegOpts::default()).unwrap();
    let (fmt, w, h, _) = pillow_open(&plain, "jpg");
    assert_eq!((fmt.as_str(), w, h), ("JPEG", 640, 480));

    let with_exif = media::jpeg(&JpegOpts {
        width: 320,
        height: 200,
        exif: Some(ExifOpts {
            date_time_original: "2023:12:31 23:59:58".into(),
            make: "TestMake".into(),
            model: "TestModel".into(),
            orientation: 6,
            thumbnail: true,
        }),
        ..Default::default()
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("e.jpg");
    std::fs::write(&p, &with_exif).unwrap();
    let script = r#"
import sys
from PIL import Image
im = Image.open(sys.argv[1]); im.load()
ex = im.getexif()
sub = ex.get_ifd(0x8769)
print(ex.get(0x010F), ex.get(0x0110), ex.get(0x0112), sub.get(0x9003), sub.get(0x9004))
"#;
    let out = tools::python(script, &[p.to_str().unwrap()]).unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(
        text,
        "TestMake TestModel 6 2023:12:31 23:59:58 2023:12:31 23:59:58"
    );
    // The embedded thumbnail is a complete JPEG inside APP1.
    let thumb_marker = with_exif.windows(2).filter(|w| w == &[0xFF, 0xD8]).count();
    assert!(thumb_marker >= 2, "thumbnail SOI inside the file");
}

#[test]
fn jpeg_pillow_variants() {
    if skip_unless(&["python3"]) || !tools::have_pillow() {
        eprintln!("skipped: Pillow missing");
        return;
    }
    let prog = media::jpeg(&JpegOpts {
        progressive: true,
        width: 300,
        height: 200,
        ..Default::default()
    })
    .unwrap();
    assert!(contains(&prog, &[0xFF, 0xC2]), "SOF2 progressive marker");
    pillow_open(&prog, "jpg");

    let rst = media::jpeg(&JpegOpts {
        restart_interval: Some(8),
        width: 300,
        height: 200,
        ..Default::default()
    })
    .unwrap();
    assert!(
        contains(&rst, &[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x08]),
        "DRI = 8"
    );
    assert!(contains(&rst, &[0xFF, 0xD0]), "RST0 present");
    pillow_open(&rst, "jpg");

    let sub = media::jpeg(&JpegOpts {
        chroma: Chroma::S420,
        exif: Some(ExifOpts::default()),
        width: 320,
        height: 240,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(&sub[..4], &[0xFF, 0xD8, 0xFF, 0xE1]);
    pillow_open(&sub, "jpg");
}

#[test]
fn stills_open_in_pillow() {
    if skip_unless(&["python3"]) || !tools::have_pillow() {
        eprintln!("skipped: Pillow missing");
        return;
    }
    for color in [PngColor::Rgb, PngColor::Rgba, PngColor::Gray] {
        for interlaced in [false, true] {
            let p = media::png(&PngOpts {
                width: 123,
                height: 77,
                interlaced,
                color,
                ..Default::default()
            });
            let (fmt, w, h, _) = pillow_open(&p, "png");
            assert_eq!(
                (fmt.as_str(), w, h),
                ("PNG", 123, 77),
                "{color:?} {interlaced}"
            );
        }
    }
    let o = StillOpts {
        width: 90,
        height: 60,
        seed: 3,
    };
    assert_eq!(pillow_open(&media::gif(&o), "gif").0, "GIF");
    assert_eq!(pillow_open(&media::bmp(&o), "bmp").0, "BMP");
    assert_eq!(pillow_open(&media::webp(&o), "webp").0, "WEBP");
    let t = media::tiff(&TiffOpts {
        width: 50,
        height: 40,
        ..Default::default()
    });
    let (fmt, w, h, mode) = pillow_open(&t, "tif");
    assert_eq!((fmt.as_str(), w, h, mode.as_str()), ("TIFF", 50, 40, "RGB"));
    let be = media::tiff(&TiffOpts {
        width: 50,
        height: 40,
        big_endian: true,
        rows_per_strip: 7,
        ..Default::default()
    });
    assert_eq!(pillow_open(&be, "tif").0, "TIFF");
}

#[test]
fn video_matrix_probes_clean() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    use AudioCodec as A;
    use Container as C;
    use VideoCodec as V;
    let cases = [
        (
            C::Mp4,
            V::H264,
            A::Aac,
            "mp4",
            vec!["aac,audio", "h264,video"],
        ),
        (
            C::Mp4,
            V::Hevc,
            A::Aac,
            "mp4",
            vec!["aac,audio", "hevc,video"],
        ),
        (
            C::Mp4,
            V::Mpeg4,
            A::Mp3,
            "mp4",
            vec!["mp3,audio", "mpeg4,video"],
        ),
        (
            C::Mov,
            V::H264,
            A::Pcm,
            "mov",
            vec!["h264,video", "pcm_s16le,audio"],
        ),
        (C::Mov, V::Mjpeg, A::None, "mov", vec!["mjpeg,video"]),
        (
            C::ThreeGp,
            V::H264,
            A::Aac,
            "3gp",
            vec!["aac,audio", "h264,video"],
        ),
        (
            C::Avi,
            V::Mpeg4,
            A::Pcm,
            "avi",
            vec!["mpeg4,video", "pcm_s16le,audio"],
        ),
        (
            C::Avi,
            V::Mjpeg,
            A::Mp3,
            "avi",
            vec!["mjpeg,video", "mp3,audio"],
        ),
        (
            C::Mkv,
            V::H264,
            A::Aac,
            "mkv",
            vec!["aac,audio", "h264,video"],
        ),
        (C::WebM, V::Vp8, A::None, "webm", vec!["vp8,video"]),
        (
            C::WebM,
            V::Vp9,
            A::Aac,
            "webm",
            vec!["opus,audio", "vp9,video"],
        ),
        (
            C::MpegTs,
            V::H264,
            A::Aac,
            "ts",
            vec!["aac,audio", "h264,video"],
        ),
        (C::M2ts, V::Hevc, A::None, "m2ts", vec!["hevc,video"]),
        (
            C::MpegPs,
            V::Mpeg2,
            A::Mp3,
            "mpg",
            vec!["mp3,audio", "mpeg2video,video"],
        ),
    ];
    for (container, video, audio, ext, expect) in cases {
        if let Err(e) = (VideoOpts {
            container,
            video,
            audio,
            ..Default::default()
        })
        .check()
        {
            panic!("table says unsupported: {e}");
        }
        let v = match media::video(&VideoOpts {
            container,
            video,
            audio,
            seconds: 1.0,
            fps: 15,
            width: 160,
            height: 120,
            ..Default::default()
        }) {
            Ok(v) => v,
            Err(e) if e.is_missing() => {
                eprintln!("skipped: {container:?}/{video:?}: {e}");
                continue;
            }
            Err(e) => panic!("{container:?}/{video:?}/{audio:?}: {e}"),
        };
        assert!(
            v.len() > 1000 && v.len() < 1 << 20,
            "{container:?}: {} bytes",
            v.len()
        );
        let streams = ffprobe_streams(&v, &format!("x.{ext}"))
            .unwrap_or_else(|| panic!("ffprobe rejected {container:?}/{video:?}/{audio:?}"));
        assert_eq!(streams, expect, "{container:?}/{video:?}/{audio:?}");
        if container == C::M2ts {
            assert_eq!(v[4], 0x47, "m2ts has 4-byte timecodes");
            assert_eq!(v[4 + 192], 0x47);
        }
        if container == C::MpegTs {
            assert_eq!(v[0], 0x47);
            assert_eq!(v[188], 0x47);
        }
    }
    // Unsupported pairs are refused up front.
    assert!(media::video(&VideoOpts {
        container: C::WebM,
        video: V::H264,
        ..Default::default()
    })
    .is_err());
}

#[test]
fn mp4_layout_options_and_determinism() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let base = VideoOpts {
        seconds: 1.0,
        fps: 15,
        width: 160,
        height: 120,
        ..Default::default()
    };
    let order = |v: &[u8]| -> Vec<String> {
        damage::top_level_boxes(v)
            .iter()
            .map(|b| String::from_utf8_lossy(&b.kind).into_owned())
            .collect()
    };
    let slow = media::video(&base).unwrap();
    let fast = media::video(&VideoOpts {
        faststart: true,
        ..base.clone()
    })
    .unwrap();
    let frag = media::video(&VideoOpts {
        fragmented: true,
        ..base.clone()
    })
    .unwrap();
    let pos = |o: &[String], k: &str| o.iter().position(|x| x == k).unwrap();
    assert!(
        pos(&order(&slow), "moov") > pos(&order(&slow), "mdat"),
        "moov last"
    );
    assert!(
        pos(&order(&fast), "moov") < pos(&order(&fast), "mdat"),
        "moov first"
    );
    assert!(order(&frag).iter().any(|k| k == "moof"), "fragmented");

    // Same options, same bytes; other seed, other bytes but same parameters.
    assert_eq!(slow, media::video(&base).unwrap());
    let other = media::video(&VideoOpts {
        seed: 2,
        ..base.clone()
    })
    .unwrap();
    assert_ne!(slow, other);
    let probe = |v: &[u8]| {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.mp4");
        std::fs::write(&p, v).unwrap();
        tools::run_stdout(
            "ffprobe",
            &[
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,profile,width,height,pix_fmt,r_frame_rate,sample_rate,channels",
                "-of",
                "csv=p=0",
                p.to_str().unwrap(),
            ],
        )
        .unwrap()
    };
    assert_eq!(probe(&slow), probe(&other), "donor-compatible parameters");
}

#[test]
fn heic_is_a_readable_heif() {
    if skip_unless(&["ffmpeg", "ffprobe"]) || !tools::ffmpeg_has_encoder("libx265") {
        return;
    }
    let h = media::heic(&HeicOpts::default()).unwrap();
    assert_eq!(&h[4..12], b"ftypheic");
    let boxes = damage::top_level_boxes(&h);
    let kinds: Vec<&[u8; 4]> = boxes.iter().map(|b| &b.kind).collect();
    assert_eq!(kinds, [b"ftyp", b"meta", b"mdat"]);
    assert_eq!(boxes.iter().map(|b| b.size).sum::<usize>(), h.len());
    for k in [&b"hvcC"[..], b"ispe", b"iloc", b"pitm", b"iinf", b"ipma"] {
        assert!(contains(&h, k), "{}", String::from_utf8_lossy(k));
    }
    let streams = ffprobe_streams(&h, "x.heic").expect("ffprobe reads the HEIC");
    assert_eq!(streams, ["hevc,video"]);
    // ffmpeg can decode the picture.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.heic");
    std::fs::write(&p, &h).unwrap();
    let (ok, text) = check(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            p.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "null",
            "-",
        ],
    );
    assert!(ok && text.trim().is_empty(), "decode: {text}");
}

#[test]
fn strip_moov_breaks_ffprobe_and_truncation_is_detected() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let v = media::video(&VideoOpts {
        seconds: 1.0,
        width: 160,
        height: 120,
        ..Default::default()
    })
    .unwrap();
    assert!(ffprobe_streams(&v, "ok.mp4").is_some());
    let stripped = damage::strip_box(&v, b"moov");
    assert!(stripped.len() < v.len());
    assert!(!damage::top_level_boxes(&stripped)
        .iter()
        .any(|b| &b.kind == b"moov"));
    assert!(
        ffprobe_streams(&stripped, "no_moov.mp4").is_none(),
        "ffprobe must fail without moov"
    );
    // A moov-last file cut in half loses its moov too.
    assert!(ffprobe_streams(&damage::truncate(&v, v.len() / 2), "cut.mp4").is_none());
    // Garbage header: not parsable either.
    let mut hdr = v.clone();
    damage::overwrite_prefix(&mut hdr, 64, 9);
    assert!(ffprobe_streams(&hdr, "hdr.mp4").is_none());
}

#[test]
fn avi_without_idx1_matches_the_original_structure() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let avi = media::video(&VideoOpts {
        container: Container::Avi,
        video: VideoCodec::Mpeg4,
        audio: AudioCodec::Pcm,
        seconds: 1.0,
        width: 160,
        height: 120,
        ..Default::default()
    })
    .unwrap();
    assert!(contains(&avi, b"idx1"));
    let cut = damage::avi_strip_idx1(&avi);
    assert!(!contains(&cut, b"idx1"));
    assert!(cut.len() < avi.len());
    assert_eq!(
        u32::from_le_bytes(cut[4..8].try_into().unwrap()) as usize,
        cut.len() - 8
    );
    assert!(contains(&cut, b"movi"));
}
