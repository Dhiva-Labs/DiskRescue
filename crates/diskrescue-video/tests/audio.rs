//! MP3, WAV and FLAC.

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, NestedRole, ValidationStatus};
use diskrescue_video::{FlacFormat, Mp3Format, WavFormat};

fn measure(fmt: &dyn FileFormat, data: &[u8]) -> Option<diskrescue_core::format::Measured> {
    fmt.measure(&mem_slice(data), 0, u64::MAX).unwrap()
}

// ---------------------------------------------------------------------------
// MP3
// ---------------------------------------------------------------------------

fn mp3_with_id3(extra: &[&str]) -> Vec<u8> {
    let mut a = vec!["-c:a", "libmp3lame", "-b:a", "128k", "-id3v2_version", "3"];
    a.extend_from_slice(extra);
    a.extend_from_slice(&["-f", "mp3"]);
    tone(44100, 3, &a, "o.mp3")
}

#[test]
fn mp3_bare_frames_are_unknown_length() {
    let Some(f) = mp3_plain() else { return };
    let data = with_trailer(&f, 30_000, 12);
    let m = measure(&Mp3Format, &data).expect("recognised");
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.confidence.value(), 60);
    assert_eq!(m.meta.audio_codec.as_deref(), Some("mp3"));
    assert_eq!(m.meta.extra["sample_rate"], "44100");
    assert_eq!(m.meta.extra["bitrate_kbps"], "128");
    assert_eq!(m.meta.extra["extension"], "mp3");
    let d = m.meta.duration_ms.unwrap();
    assert!((2950..=3100).contains(&d), "duration {d}");
    // Clean end of source: the same.
    let m2 = measure(&Mp3Format, &f).unwrap();
    assert_eq!(m2.len, f.len() as u64);
    assert_eq!(m2.status, ExtentStatus::UnknownLength);
}

#[test]
fn mp3_with_id3v2_xing_and_id3v1() {
    if !need_encoders(&["libmp3lame"]) {
        return;
    }
    let f = fixture("mp3_id3_both", || {
        mp3_with_id3(&[
            "-metadata",
            "title=Hello World",
            "-metadata",
            "artist=Tester",
            "-write_id3v1",
            "1",
        ])
    });
    assert_eq!(&f[..3], b"ID3");
    assert_eq!(&f[f.len() - 128..f.len() - 125], b"TAG");
    let data = with_trailer(&f, 9000, 4);
    let m = measure(&Mp3Format, &data).unwrap();
    // The ID3v1 tag ends the file.
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.meta.extra["title"], "Hello World");
    assert_eq!(m.meta.extra["artist"], "Tester");
    let d = m.meta.duration_ms.unwrap();
    assert!((2950..=3100).contains(&d), "duration {d}");
    let v = Mp3Format.validate(&mem_slice(&f)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    // Without the ID3v1 tag it is unknown length.
    let no_v1 = &f[..f.len() - 128];
    let m = measure(&Mp3Format, &with_trailer(no_v1, 5000, 1)).unwrap();
    assert_eq!(m.len, no_v1.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
}

#[test]
fn mp3_cover_art_is_nested() {
    let Some(jpg) = jpeg_image() else { return };
    if !need_encoders(&["libmp3lame"]) {
        return;
    }
    let f = fixture("mp3_cover", || {
        let dir = tempfile::tempdir().unwrap();
        let cover = dir.path().join("c.jpg");
        std::fs::write(&cover, &**jpg).unwrap();
        let out = dir.path().join("o.mp3");
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100",
                "-i",
            ])
            .arg(&cover)
            .args([
                "-t",
                "2",
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "libmp3lame",
                "-c:v",
                "copy",
                "-id3v2_version",
                "3",
                "-f",
                "mp3",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(&out).unwrap()
    });
    let m = measure(&Mp3Format, &f).unwrap();
    assert_eq!(m.nested.len(), 1, "{:?}", m.nested);
    let n = &m.nested[0];
    assert_eq!(n.role, NestedRole::CoverArt);
    assert_eq!(n.format.as_deref(), Some("jpeg"));
    assert_eq!(&f[n.range.start as usize..n.range.end as usize], &jpg[..]);
    assert!(Mp3Format.may_contain("jpeg") && Mp3Format.may_contain("png"));
}

#[test]
fn mp3_vbr_and_layer2() {
    if need_encoders(&["libmp3lame"]) {
        let f = fixture("mp3_vbr", || {
            tone(
                44100,
                3,
                &["-c:a", "libmp3lame", "-q:a", "2", "-f", "mp3"],
                "o.mp3",
            )
        });
        let m = measure(&Mp3Format, &with_trailer(&f, 2000, 1)).unwrap();
        assert_eq!(m.len, f.len() as u64);
        let d = m.meta.duration_ms.unwrap();
        assert!((2950..=3100).contains(&d), "duration {d}");
        assert!(
            m.notes.iter().all(|n| !n.contains("VBR header says")),
            "{:?}",
            m.notes
        );
    }
    if need_encoders(&["mp2"]) {
        let f = fixture("mp2", || {
            tone(
                44100,
                2,
                &["-c:a", "mp2", "-b:a", "192k", "-f", "mp2"],
                "o.mp2",
            )
        });
        let m = measure(&Mp3Format, &f).unwrap();
        assert_eq!(m.len, f.len() as u64);
        assert_eq!(m.meta.audio_codec.as_deref(), Some("mp2"));
        assert_eq!(m.meta.extra["extension"], "mp2");
    }
}

#[test]
fn mp3_cut_and_damaged() {
    let Some(f) = mp3_plain() else { return };
    let cut = &f[..20_000];
    let m = measure(&Mp3Format, cut).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= 20_000 && m.len > 20_000 - 420);
    let v = Mp3Format.validate(&mem_slice(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    // Garbage in the middle: the walk stops there.
    let mut d = f.to_vec();
    d[10_000..10_100].copy_from_slice(&Rng(9).bytes(100));
    let m = measure(&Mp3Format, &d).unwrap();
    assert!(m.len < 10_100 && m.len > 9_000, "len {}", m.len);
    let v = Mp3Format.validate(&mem_slice(&d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v.intact_fraction.unwrap() < 0.5);
}

#[test]
fn mp3_probe_and_rejections() {
    let Some(f) = mp3_plain() else { return };
    assert!(Mp3Format.probe(&f[..4096]));
    assert!(Mp3Format.signatures().iter().any(|s| s.matches(&f[..16])));
    assert!(!Mp3Format.probe(&Rng(1).bytes(4096)));
    assert!(!Mp3Format.probe(&[0xFF; 4096]));
    assert!(measure(&Mp3Format, &[0xFFu8; 20_000]).is_none());
    assert!(measure(&Mp3Format, &Rng(2).bytes(20_000)).is_none());
    // Seven frames are not enough.
    let one = 417usize; // 128 kbit/s at 44.1 kHz, no padding
    let _ = one;
    let m = measure(&Mp3Format, &f).unwrap();
    assert!(m.len > 8 * 400);
    // Cut to just under eight frames: rejected.
    assert!(measure(&Mp3Format, &f[..7 * 417 + 10]).is_none());
}

// ---------------------------------------------------------------------------
// WAV
// ---------------------------------------------------------------------------

#[test]
fn wav_finalised_is_exact() {
    let Some(f) = wav_pcm() else { return };
    let data = with_trailer(&f, 20_000, 31);
    let m = measure(&WavFormat, &data).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.confidence.value(), 90);
    assert_eq!(m.meta.audio_codec.as_deref(), Some("pcm_s16le"));
    assert_eq!(m.meta.duration_ms, Some(2000));
    assert_eq!(m.meta.extra["sample_rate"], "44100");
    assert_eq!(m.meta.extra["channels"], "1");
    assert_eq!(m.meta.extra["bits"], "16");
    assert!(m.meta.software.as_deref().unwrap_or("").starts_with("Lavf"));
    let v = WavFormat.validate(&mem_slice(&f)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn wav_streaming_recorder_sizes_unset() {
    if !need_encoders(&["pcm_s16le", "pcm_s24le", "pcm_f32le", "pcm_u8"]) {
        return;
    }
    for (name, codec, bits) in [
        ("s16", "pcm_s16le", 16),
        ("s24", "pcm_s24le", 24),
        ("f32", "pcm_f32le", 32),
        ("u8", "pcm_u8", 8),
    ] {
        let owned = sine_args(22050, 2);
        let mut args: Vec<&str> = owned.iter().map(String::as_str).collect();
        args.extend_from_slice(&["-c:a", codec, "-f", "wav"]);
        let f = fixture(&format!("wav_pipe_{name}"), || ffmpeg_pipe(&args));
        // ffmpeg could not seek back: the sizes are unset.
        assert_eq!(&f[4..8], &[0xFF, 0xFF, 0xFF, 0xFF], "{name}");
        let data = with_trailer(&f, 40_000, 17);
        let m = measure(&WavFormat, &data).unwrap_or_else(|| panic!("{name}: not recognised"));
        assert_eq!(m.status, ExtentStatus::UnknownLength, "{name}");
        assert_eq!(m.confidence.value(), 25, "{name}");
        assert!(
            m.len + 300 >= f.len() as u64 && m.len <= f.len() as u64 + 600,
            "{name}: len {} vs {}",
            m.len,
            f.len()
        );
        assert_eq!(m.meta.extra["bits"], bits.to_string());
        let v = WavFormat.validate(&mem_slice(&f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged, "{name}");
        assert!(
            v.problems[0].contains("never finalised"),
            "{:?}",
            v.problems
        );
    }
}

#[test]
fn rf64_is_measured_with_ds64_sizes() {
    if !need_encoders(&["pcm_s16le"]) {
        return;
    }
    let f = fixture("rf64", || {
        tone(
            44100,
            2,
            &["-c:a", "pcm_s16le", "-rf64", "always", "-f", "wav"],
            "o.wav",
        )
    });
    assert_eq!(&f[..4], b"RF64");
    let m = measure(&WavFormat, &with_trailer(&f, 10_000, 5)).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.meta.extra["rf64"], "true");
    assert_eq!(m.meta.duration_ms, Some(2000));
    assert!(WavFormat.signatures().iter().any(|s| s.matches(&f[..16])));
}

#[test]
fn wav_metadata_and_cut() {
    if !need_encoders(&["pcm_s16le"]) {
        return;
    }
    let f = fixture("wav_meta", || {
        tone(
            44100,
            1,
            &[
                "-c:a",
                "pcm_s16le",
                "-metadata",
                "title=Take 1",
                "-metadata",
                "date=2024-05-01",
                "-write_bext",
                "1",
                "-metadata",
                "origination_date=2024-05-01",
                "-metadata",
                "origination_time=12:30:00",
                "-f",
                "wav",
            ],
            "o.wav",
        )
    });
    let m = measure(&WavFormat, &f).unwrap();
    assert_eq!(
        m.meta.extra.get("title").map(String::as_str),
        Some("Take 1")
    );
    let c = m.meta.created.expect("creation date");
    assert!(c.to_rfc3339().starts_with("2024-05-01"), "{c}");
    // Cut in the middle of the data.
    let cut = &f[..f.len() - 5001];
    let m = measure(&WavFormat, cut).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut.len() as u64);
    assert_eq!(m.err_offset, Some(m.len));
    let v = WavFormat.validate(&mem_slice(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn wav_hostile_input() {
    let Some(f) = wav_pcm() else { return };
    let fmt = WavFormat;
    let mut rng = Rng(0x57);
    for i in 0..200 {
        let n = 12 + (rng.next() % 3000) as usize;
        let mut d = rng.bytes(n);
        d[..4].copy_from_slice(if i % 5 == 0 { b"RF64" } else { b"RIFF" });
        d[8..12].copy_from_slice(b"WAVE");
        if i % 2 == 0 && n > 40 {
            d[12..20].copy_from_slice(b"fmt \x10\x00\x00\x00");
            d[20..22].copy_from_slice(&1u16.to_le_bytes());
            d[22..24].copy_from_slice(&2u16.to_le_bytes());
            d[24..28].copy_from_slice(&44100u32.to_le_bytes());
            d[32..34].copy_from_slice(&4u16.to_le_bytes());
            d[34..36].copy_from_slice(&16u16.to_le_bytes());
        }
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    for off in 0..96 {
        for v in [0u8, 0xFF, 0x7F, 0x80] {
            let mut d = f[..8000].to_vec();
            d[off] = v;
            let s = mem(d);
            let _ = fmt.measure(&s, 0, u64::MAX);
            let _ = fmt.validate(&s);
        }
    }
    let mut len = 0;
    while len < 2000 {
        let _ = fmt.measure(&mem_slice(&f[..len]), 0, u64::MAX);
        len += 7;
    }
}

// ---------------------------------------------------------------------------
// FLAC
// ---------------------------------------------------------------------------

fn flac(name: &str, secs: u32, rate: u32, extra: &[&str]) -> Option<std::sync::Arc<Vec<u8>>> {
    need_encoders(&["flac"]).then(|| {
        fixture(name, || {
            let mut a = vec!["-c:a", "flac"];
            a.extend_from_slice(extra);
            a.extend_from_slice(&["-f", "flac"]);
            tone(rate, secs, &a, "o.flac")
        })
    })
}

#[test]
fn flac_is_exact_and_complete() {
    let Some(f) = flac_stereo() else { return };
    let data = with_trailer(&f, 25_000, 41);
    let m = measure(&FlacFormat, &data).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert_eq!(m.confidence.value(), 90);
    assert_eq!(m.meta.audio_codec.as_deref(), Some("flac"));
    assert_eq!(m.meta.duration_ms, Some(3000));
    assert_eq!(m.meta.extra["sample_rate"], "44100");
    assert_eq!(m.meta.extra["channels"], "2");
    assert_eq!(m.meta.extra["bits"], "16");
    let v = FlacFormat.validate(&mem_slice(&f)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    assert!(FlacFormat.probe(&f[..64]));
}

#[test]
fn flac_all_compression_levels_and_layouts() {
    // The frame walker steps through every subframe type the encoder emits.
    let cases: Vec<(&str, u32, Vec<&str>)> = vec![
        ("flac_l0", 44100, vec!["-compression_level", "0"]),
        ("flac_l5", 44100, vec!["-compression_level", "5"]),
        (
            "flac_l8",
            48000,
            vec!["-compression_level", "8", "-ac", "2"],
        ),
        (
            "flac_l12",
            48000,
            vec!["-compression_level", "12", "-ac", "2"],
        ),
        ("flac_mono_8k", 8000, vec!["-compression_level", "6"]),
        (
            "flac_24bit",
            96000,
            vec![
                "-sample_fmt",
                "s32",
                "-bits_per_raw_sample",
                "24",
                "-ac",
                "2",
            ],
        ),
        (
            "flac_bs1152",
            44100,
            vec!["-frame_size", "1152", "-ac", "2"],
        ),
        ("flac_bs256", 44100, vec!["-frame_size", "256"]),
        ("flac_5_1", 48000, vec!["-ac", "6"]),
    ];
    for (name, rate, extra) in cases {
        let Some(f) = flac(name, 2, rate, &extra) else {
            return;
        };
        let data = with_trailer(&f, 9000, 3);
        let m = measure(&FlacFormat, &data).unwrap_or_else(|| panic!("{name}: not recognised"));
        assert_eq!(m.len, f.len() as u64, "{name}");
        assert_eq!(m.status, ExtentStatus::Complete, "{name}: {:?}", m.notes);
        assert!(m.notes.is_empty(), "{name}: {:?}", m.notes);
        assert_eq!(m.meta.duration_ms, Some(2000), "{name}");
        let v = FlacFormat.validate(&mem_slice(&f)).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Valid,
            "{name}: {:?}",
            v.problems
        );
    }
}

#[test]
fn flac_noise_exercises_verbatim_and_escape_codes() {
    if !need_encoders(&["flac"]) {
        return;
    }
    let f = fixture("flac_noise", || {
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "anoisesrc=color=white:amplitude=0.9:sample_rate=44100",
                "-t",
                "2",
                "-c:a",
                "flac",
                "-f",
                "flac",
            ],
            "o.flac",
        )
    });
    let m = measure(&FlacFormat, &with_trailer(&f, 3000, 1)).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    let v = FlacFormat.validate(&mem_slice(&f)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn flac_silence_uses_constant_subframes() {
    if !need_encoders(&["flac"]) {
        return;
    }
    let f = fixture("flac_silence", || {
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=44100:cl=stereo",
                "-t",
                "2",
                "-c:a",
                "flac",
                "-f",
                "flac",
            ],
            "o.flac",
        )
    });
    let m = measure(&FlacFormat, &with_trailer(&f, 3000, 1)).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
}

#[test]
fn two_flac_files_back_to_back_are_separated() {
    let (Some(a), Some(b)) = (
        flac_stereo(),
        flac("flac_l5", 2, 44100, &["-compression_level", "5"]),
    ) else {
        return;
    };
    let mut d = a.to_vec();
    d.extend_from_slice(&b);
    let m = measure(&FlacFormat, &d).unwrap();
    assert_eq!(m.len, a.len() as u64);
    let m2 = FlacFormat
        .measure(&mem_slice(&d), a.len() as u64, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(m2.len, b.len() as u64);
}

#[test]
fn flac_cut_and_damaged() {
    let Some(f) = flac_stereo() else { return };
    let cut = &f[..f.len() * 2 / 3];
    let m = measure(&FlacFormat, cut).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut.len() as u64);
    assert_eq!(m.err_offset, Some(m.len));
    let v = FlacFormat.validate(&mem_slice(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    // One flipped bit in a frame: bridged, whole extent still found.
    let mut d = f.to_vec();
    let at = f.len() / 2;
    d[at] ^= 0x10;
    let m = measure(&FlacFormat, &d).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert!(
        m.notes.iter().any(|n| n.contains("damaged")),
        "{:?}",
        m.notes
    );
    assert!(m.confidence.value() < 90);
    let v = FlacFormat.validate(&mem_slice(&d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn flac_tags_and_cover() {
    let Some(jpg) = jpeg_image() else { return };
    if !need_encoders(&["flac", "mjpeg"]) {
        return;
    }
    let f = fixture("flac_cover", || {
        let dir = tempfile::tempdir().unwrap();
        let cover = dir.path().join("c.jpg");
        std::fs::write(&cover, &**jpg).unwrap();
        let out = dir.path().join("o.flac");
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100",
                "-i",
            ])
            .arg(&cover)
            .args([
                "-t",
                "1",
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "flac",
                "-c:v",
                "copy",
                "-disposition:v",
                "attached_pic",
                "-metadata",
                "title=Song",
                "-metadata",
                "artist=Band",
                "-metadata",
                "date=2024-05-01",
                "-f",
                "flac",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(&out).unwrap()
    });
    let m = measure(&FlacFormat, &with_trailer(&f, 5000, 2)).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.meta.extra["title"], "Song");
    assert_eq!(m.meta.extra["artist"], "Band");
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2024-05-01T00:00:00Z");
    assert_eq!(m.nested.len(), 1, "{:?}", m.nested);
    let n = &m.nested[0];
    assert_eq!(n.role, NestedRole::CoverArt);
    assert_eq!(&f[n.range.start as usize..n.range.end as usize], &jpg[..]);
}

#[test]
fn flac_hostile_input() {
    let Some(f) = flac_stereo() else { return };
    let fmt = FlacFormat;
    let mut rng = Rng(0xF1);
    let mut len = 0usize;
    while len < f.len().min(40_000) {
        let _ = fmt.measure(&mem_slice(&f[..len]), 0, u64::MAX);
        len += 1 + (rng.next() % 3000) as usize;
    }
    for off in 0..120 {
        for v in [0u8, 0xFF, 0x80, 0x01] {
            let mut d = f[..30_000].to_vec();
            d[off] = v;
            let s = mem(d);
            let _ = fmt.measure(&s, 0, u64::MAX);
            let _ = fmt.validate(&s);
        }
    }
    // Corrupt bytes throughout the frames.
    for k in 0..150 {
        let mut d = f[..30_000].to_vec();
        let at = 100 + (rng.next() % 29_000) as usize;
        d[at] = (rng.next() & 0xFF) as u8;
        d[at + 1] = (k & 0xFF) as u8;
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    // Random bytes after a valid signature and STREAMINFO.
    for _ in 0..200 {
        let mut d = rng.bytes(3000);
        d[..8].copy_from_slice(b"fLaC\x80\x00\x00\x22");
        d[8..12].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
        d[18..20].copy_from_slice(&[0x0A, 0xC4]);
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
}
