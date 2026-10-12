//! moov reconstruction from a reference clip, verified with ffprobe/ffmpeg.

mod common;

use std::path::Path;
use std::time::Instant;

use common::*;
use diskrescue_isobmff::rebuild::{learn, rebuild, RebuildOptions, RebuildReport};

/// Rebuild `damaged` bytes with the reference at `reference`; returns the
/// output path and the report.
fn run(name: &str, damaged: Vec<u8>, reference: &Path) -> (std::path::PathBuf, RebuildReport) {
    let dmg = write_scratch(&format!("{name}.damaged"), &damaged);
    let r = learn(&open(reference)).expect("learn reference");
    let out_path = scratch(&format!("{name}.rebuilt.mp4"));
    let mut out = std::fs::File::create(&out_path).unwrap();
    let rep = rebuild(&open(&dmg), &r, &mut out, &RebuildOptions::default()).expect("rebuild");
    drop(out);
    (out_path, rep)
}

/// Expected counts for an original file cut at `cut` bytes (None = whole).
fn expected(original: &Path, cut: Option<u64>) -> (usize, f64) {
    let inside = |p: &Packet| cut.is_none_or(|c| p.pos + p.size <= c);
    let v = packets(original, "v").iter().filter(|p| inside(p)).count();
    let a = packets(original, "a").iter().filter(|p| inside(p)).count();
    (v, a as f64 * 1024.0 / 48000.0)
}

fn check(out: &Path, original: &Path, video: usize, audio_s: Option<f64>, bframes: bool) {
    ffprobe_clean(out).unwrap_or_else(|e| panic!("ffprobe rejects {}: {e}", out.display()));
    decodes_clean(out).unwrap_or_else(|e| panic!("decode errors in {}: {e}", out.display()));
    let o = streams(original);
    let r = streams(out);
    let ov = o.iter().find(|s| s.kind == "video");
    let rv = r.iter().find(|s| s.kind == "video");
    if let (Some(ov), Some(rv)) = (ov, rv) {
        assert_eq!(ov.codec, rv.codec);
        assert_eq!((ov.width, ov.height), (rv.width, rv.height));
        assert!(
            (rv.frames as i64 - video as i64).abs() <= 1,
            "video frames {} expected {video}",
            rv.frames
        );
    } else {
        assert_eq!(ov.is_some(), rv.is_some(), "video stream presence");
    }
    if let Some(expect) = audio_s {
        let ra = r.iter().find(|s| s.kind == "audio").expect("audio stream");
        let oa = o.iter().find(|s| s.kind == "audio").unwrap();
        assert_eq!(ra.codec, oa.codec);
        assert!(
            (ra.duration - expect).abs() <= 0.1,
            "audio duration {} expected {expect}",
            ra.duration
        );
    }
    if bframes {
        let pts = frame_pts(out);
        assert!(!pts.is_empty());
        assert!(
            pts.windows(2).all(|w| w[1] > w[0]),
            "presentation order broken: {:?}",
            &pts[..pts.len().min(20)]
        );
    }
}

#[test]
fn h264_bframes_stripped_moov() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run("h264_strip", dmg, &fixture("h264_ref"));
    let (v, a) = expected(&orig, None);
    assert_eq!(rep.video_samples as usize, v);
    assert_eq!(rep.bytes_unclassified, 0, "{rep:?}");
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn h264_mdat_size_zeroed() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let dmg = zero_box_size(&strip_box(&std::fs::read(&orig).unwrap(), b"moov"), b"mdat");
    let (out, _) = run("h264_zero", dmg, &fixture("h264_ref"));
    let (v, a) = expected(&orig, None);
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn h264_truncated_at_70_percent() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let bytes = std::fs::read(&orig).unwrap();
    let cut = bytes.len() * 7 / 10;
    let (out, rep) = run("h264_cut", bytes[..cut].to_vec(), &fixture("h264_ref"));
    let (v, a) = expected(&orig, Some(cut as u64));
    assert!(rep.stopped_at.is_none());
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn hevc_stripped_zeroed_and_truncated() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h265");
    let bytes = std::fs::read(&orig).unwrap();
    let reference = fixture("h265_ref");
    let stripped = strip_box(&bytes, b"moov");
    let (out, _) = run("h265_strip", stripped.clone(), &reference);
    let (v, a) = expected(&orig, None);
    check(&out, &orig, v, Some(a), true);
    let (out, _) = run("h265_zero", zero_box_size(&stripped, b"mdat"), &reference);
    check(&out, &orig, v, Some(a), true);
    let cut = bytes.len() * 7 / 10;
    let (out, _) = run("h265_cut", bytes[..cut].to_vec(), &reference);
    let (v, a) = expected(&orig, Some(cut as u64));
    check(&out, &orig, v, Some(a), true);
}

/// With the clip itself as reference the rebuilt tables must equal the
/// original ones (except the final short AAC frame duration).
fn same_clip_is_perfect(name: &str) {
    let orig = fixture(name);
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run(&format!("{name}_self"), dmg, &orig);
    assert_eq!(rep.bytes_unclassified, 0);
    let (ov, rv) = (packets(&orig, "v"), packets(&out, "v"));
    assert_eq!(ov.len(), rv.len());
    for (a, b) in ov.iter().zip(&rv) {
        assert_eq!((a.size, a.pts, a.dts, a.key), (b.size, b.pts, b.dts, b.key));
    }
    let (oa, ra) = (packets(&orig, "a"), packets(&out, "a"));
    assert_eq!(oa.len(), ra.len());
    for (a, b) in oa.iter().zip(&ra) {
        assert_eq!(a.size, b.size);
    }
    let (v, a) = expected(&orig, None);
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn same_clip_reference_h264() {
    if ffmpeg_or_skip() {
        same_clip_is_perfect("h264");
    }
}

#[test]
fn same_clip_reference_hevc() {
    if ffmpeg_or_skip() {
        same_clip_is_perfect("h265");
    }
}

#[test]
fn damaged_longer_than_reference() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264_6s");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run("h264_6s_strip", dmg, &fixture("h264_ref"));
    let (v, a) = expected(&orig, None);
    assert_eq!(rep.video_samples as usize, v);
    assert!(rep.duration_ms >= 5900, "{rep:?}");
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn faststart_source_and_no_bframes() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264_faststart");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, _) = run("fs_strip", dmg, &fixture("h264_ref"));
    let (v, a) = expected(&orig, None);
    check(&out, &orig, v, Some(a), true);

    let orig = fixture("h264_bf0");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, _) = run("bf0_strip", dmg, &fixture("h264_bf0_ref"));
    let (v, a) = expected(&orig, None);
    check(&out, &orig, v, Some(a), true);
    // No reordering: no ctts needed, pts == dts.
    for p in packets(&out, "v") {
        assert_eq!(p.pts, p.dts);
    }
}

#[test]
fn video_only_and_audio_only() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("video_only");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run("vo_strip", dmg, &fixture("video_only_ref"));
    assert_eq!(rep.audio_samples, 0);
    let (v, _) = expected(&orig, None);
    check(&out, &orig, v, None, true);

    let orig = fixture("m4a");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run("m4a_strip", dmg, &fixture("m4a_ref"));
    assert_eq!(rep.video_samples, 0);
    let (_, a) = expected(&orig, None);
    check(&out, &orig, 0, Some(a), false);
}

#[test]
fn quicktime_pcm_audio() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("mov_pcm");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = run("pcm_strip", dmg, &fixture("mov_pcm_ref"));
    let (v, _) = expected(&orig, None);
    assert_eq!(rep.audio_samples, 144_000, "{rep:?}");
    ffprobe_clean(&out).unwrap();
    decodes_clean(&out).unwrap();
    let r = streams(&out);
    let a = r.iter().find(|s| s.kind == "audio").unwrap();
    assert!((a.duration - 3.0).abs() <= 0.1, "{a:?}");
    let rv = r.iter().find(|s| s.kind == "video").unwrap();
    assert!((rv.frames as i64 - v as i64).abs() <= 1);
}

#[test]
fn quicktime_and_3gp_aac() {
    if !ffmpeg_or_skip() {
        return;
    }
    for (name, reference) in [("mov", "mov_ref"), ("3gp", "3gp")] {
        let orig = fixture(name);
        let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
        let (out, _) = run(&format!("{name}_strip"), dmg, &fixture(reference));
        let (v, _) = expected(&orig, None);
        let a = streams(&orig)
            .iter()
            .find(|s| s.kind == "audio")
            .map(|s| s.duration)
            .unwrap();
        check(&out, &orig, v, Some(a), true);
    }
}

#[test]
fn hole_in_mdat_is_skipped() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let mut dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let hole = 60_000..64_096;
    for b in &mut dmg[hole.clone()] {
        *b = 0;
    }
    let (out, rep) = run("hole", dmg, &fixture("h264_ref"));
    assert!(!rep.unclassified_ranges.is_empty());
    let covered = rep
        .unclassified_ranges
        .iter()
        .any(|r| r.start <= hole.start as u64 + 4096 && r.end >= hole.end as u64 - 4096);
    assert!(covered, "{:?}", rep.unclassified_ranges);
    ffprobe_clean(&out).unwrap();
    // The pictures that depend on the frames lost in the hole are dropped
    // up to the next keyframe (one GOP of 30), so the output decodes
    // without errors.
    decodes_clean(&out).unwrap_or_else(|e| panic!("decode errors after a hole: {e}"));
    let (v, _) = expected(&orig, None);
    assert!(rep.video_samples as usize >= v - 32, "{rep:?}");
    assert!(
        rep.notes.iter().any(|n| n.contains("cannot reconstruct")),
        "{rep:?}"
    );
}

#[test]
fn head_lost_rebuild_starts_at_a_keyframe() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let bytes = std::fs::read(&orig).unwrap();
    let dmg = strip_box(&bytes, b"moov")[4096..].to_vec();
    let (out, rep) = run("headless", dmg, &fixture("h264_ref"));
    assert!(rep.keyframes >= 1);
    ffprobe_clean(&out).unwrap();
    decodes_clean(&out).unwrap();
}

#[test]
fn wrong_codec_reference_is_rejected() {
    if !ffmpeg_or_skip() {
        return;
    }
    let dmg = strip_box(&std::fs::read(fixture("h264")).unwrap(), b"moov");
    let d = write_scratch("wrong_ref.damaged", &dmg);
    // m4a reference: audio-only AAC finds audio but no video; a video-only
    // reference of another codec must fail or recover nothing useful.
    let r = learn(&open(&fixture("h265_ref"))).unwrap();
    let mut sink = Vec::new();
    // Either nothing matches (error) or no video sample survives.
    if let Ok(rep) = rebuild(&open(&d), &r, &mut sink, &RebuildOptions::default()) {
        assert_eq!(rep.video_samples, 0, "{rep:?}");
    }
}

#[test]
fn long_recording_stress() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("long");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let t = Instant::now();
    let (out, rep) = run("long_strip", dmg, &fixture("long_ref"));
    let elapsed = t.elapsed();
    assert!(elapsed.as_secs() < 30, "rebuild took {elapsed:?}");
    let (v, a) = expected(&orig, None);
    assert_eq!(rep.video_samples as usize, v);
    check(&out, &orig, v, Some(a), true);
}

#[test]
fn duration_limit_and_report_fields() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let d = write_scratch("limit.damaged", &dmg);
    let r = learn(&open(&fixture("h264_ref"))).unwrap();
    assert!(r.video.is_some() && r.audio.is_some());
    assert_eq!(r.frame_duration, 512);
    assert_eq!(r.audio_frame_duration, 1024);
    assert!(!r.interleave_pattern.is_empty());
    let mut sink = Vec::new();
    let opts = RebuildOptions {
        max_duration_s: Some(1),
        ..Default::default()
    };
    let rep = rebuild(&open(&d), &r, &mut sink, &opts).unwrap();
    assert!(
        rep.video_samples <= 31 && rep.video_samples >= 29,
        "{rep:?}"
    );
    assert_eq!(rep.output_len as usize, sink.len());
}
