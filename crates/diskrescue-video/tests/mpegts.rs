//! MPEG-TS and M2TS: run walking, PSI parsing, metadata, streaming behaviour.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use diskrescue_core::format::{Confidence, ExtentStatus, FileFormat, ValidationStatus};
use diskrescue_core::SharedSource;
use diskrescue_video::MpegTsFormat;

fn measure(data: &[u8]) -> Option<diskrescue_core::format::Measured> {
    MpegTsFormat.measure(&mem_slice(data), 0, u64::MAX).unwrap()
}

#[test]
fn ts_run_is_exact_before_random_data_and_unknown_length() {
    let Some(f) = ts_h264_aac() else { return };
    assert_eq!(f.len() % 188, 0);
    let data = with_trailer(&f, 50_000, 3);
    let m = measure(&data).expect("recognised");
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.err_offset, None);
    assert_eq!(m.confidence, Confidence::HIGH);
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.height, Some(240));
    assert_eq!(m.meta.video_codec.as_deref(), Some("h264"));
    assert_eq!(m.meta.audio_codec.as_deref(), Some("aac"));
    let d = m.meta.duration_ms.expect("duration from PCR");
    assert!((2800..=3300).contains(&d), "duration {d}");
    assert_eq!(m.meta.extra["extension"], "ts");
    assert_eq!(m.meta.extra["packet_size"], "188");
    assert_eq!(m.meta.extra["programs"], "1");
    assert!(m.nested.is_empty());
    assert!(!MpegTsFormat.may_contain("jpeg"));
}

#[test]
fn m2ts_is_detected_by_its_192_byte_stride() {
    let Some(f) = ts_h264_aac() else { return };
    let m2 = to_m2ts(&f);
    let data = with_trailer(&m2, 40_000, 8);
    let m = measure(&data).expect("recognised");
    assert_eq!(m.len, m2.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.meta.extra["extension"], "mts");
    assert_eq!(m.meta.extra["packet_size"], "192");
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.video_codec.as_deref(), Some("h264"));
    assert_eq!(m.confidence, Confidence::HIGH);
    let d = m.meta.duration_ms.unwrap();
    assert!((2800..=3300).contains(&d), "duration {d}");
}

#[test]
fn ffmpeg_native_m2ts_mode() {
    let Some(f) = m2ts_native() else { return };
    assert_eq!(f.len() % 192, 0);
    let m = measure(&with_trailer(&f, 10_000, 1)).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.meta.extra["extension"], "mts");
}

#[test]
fn mpeg2_video_dimensions_and_rate() {
    let Some(f) = ts_mpeg2_mp2() else { return };
    let m = measure(&f).unwrap();
    assert_eq!(m.meta.video_codec.as_deref(), Some("mpeg2video"));
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.height, Some(240));
    assert_eq!(m.meta.extra["fps"], "30.000");
    assert_eq!(m.meta.audio_codec.as_deref(), Some("mpeg_audio"));
}

#[test]
fn a_junk_run_that_breaks_the_stride_ends_the_run() {
    let Some(f) = ts_h264_aac() else { return };
    let mut d = f.to_vec();
    d[100_000..101_500].copy_from_slice(&Rng(2).bytes(1500));
    let m = measure(&d).unwrap();
    // The packet whose sync byte is the last one before the junk still counts.
    assert_eq!(m.len, 100_016);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    let v = MpegTsFormat.validate(&mem_slice(&d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems[0].contains("sync lost at offset 100016"),
        "{:?}",
        v.problems
    );
    assert!(v.intact_fraction.unwrap() < 0.5);
}

#[test]
fn sixteen_packets_are_enough_fifteen_are_not() {
    let Some(f) = ts_h264_aac() else { return };
    let m = measure(&f[..16 * 188]).expect("16 packets");
    assert_eq!(m.len, 16 * 188);
    assert_eq!(m.confidence, Confidence::HIGH);
    assert!(measure(&f[..15 * 188]).is_none());
    // Trailing partial packet is not part of the run.
    let m = measure(&f[..16 * 188 + 100]).unwrap();
    assert_eq!(m.len, 16 * 188);
}

#[test]
fn stream_without_pat_is_medium_confidence_and_has_no_stream_list() {
    let Some(f) = ts_h264_aac() else { return };
    // Remove every PID 0 packet: the PAT never appears.
    let mut d = Vec::new();
    for p in f.chunks_exact(188) {
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        if pid != 0 {
            d.extend_from_slice(p);
        }
    }
    let m = measure(&d).unwrap();
    assert_eq!(m.confidence, Confidence::MEDIUM);
    assert_eq!(m.meta.extra["programs"], "0");
    assert_eq!(m.meta.video_codec, None);
    assert_eq!(m.len, d.len() as u64);
    assert!(m.notes.iter().any(|n| n.contains("PAT")));
    let v = MpegTsFormat.validate(&mem_slice(&d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn continuity_gaps_are_reported_but_do_not_end_the_run() {
    let Some(f) = ts_h264_aac() else { return };
    // Remove one video packet from the middle.
    let mut d = Vec::new();
    let drop = 600usize;
    let mut dropped = false;
    for (i, p) in f.chunks_exact(188).enumerate() {
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        if i >= drop && !dropped && pid != 0 && pid != 0x1000 {
            dropped = true; // drop the first non-PSI packet after `drop`
            continue;
        }
        d.extend_from_slice(p);
    }
    assert_eq!(d.len(), f.len() - 188);
    let m = measure(&d).unwrap();
    assert_eq!(m.len, d.len() as u64);
    assert!(
        m.notes.iter().any(|n| n.contains("continuity")),
        "{:?}",
        m.notes
    );
    let v = MpegTsFormat.validate(&mem_slice(&d)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems.iter().any(|p| p.contains("continuity")),
        "{:?}",
        v.problems
    );
}

#[test]
fn healthy_streams_validate() {
    for f in [ts_h264_aac(), m2ts_native(), ts_mpeg2_mp2()]
        .into_iter()
        .flatten()
    {
        let v = MpegTsFormat.validate(&mem_slice(&f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
        assert_eq!(v.intact_fraction, Some(1.0));
    }
    let Some(f) = ts_h264_aac() else { return };
    // Cut inside a packet.
    let v = MpegTsFormat
        .validate(&mem_slice(&f[..f.len() - 50]))
        .unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems[0].contains("inside a packet"),
        "{:?}",
        v.problems
    );
    // Transport error indicator.
    let mut d = f.to_vec();
    d[188 * 40 + 1] |= 0x80;
    let v = MpegTsFormat.validate(&mem_slice(&d)).unwrap();
    assert!(v.problems.iter().any(|p| p.contains("transport error")));
    // Not a transport stream at all.
    let v = MpegTsFormat.validate(&mem(Rng(1).bytes(5000))).unwrap();
    assert_eq!(v.status, ValidationStatus::Invalid);
}

#[test]
fn three_hundred_megabytes_stream_with_bounded_memory() {
    let Some(f) = ts_h264_aac() else { return };
    // A virtual 300 MB stream: the real fixture repeated, never materialised.
    let reps = 300_000_000u64 / f.len() as u64;
    let size = reps * f.len() as u64;
    assert_eq!(size % 188, 0);
    let counting = Arc::new(Counting::new(Arc::new(Repeating::new(f.to_vec(), size))));
    let shared: SharedSource = counting.clone();
    let t = std::time::Instant::now();
    let m = MpegTsFormat.measure(&shared, 0, u64::MAX).unwrap().unwrap();
    let elapsed = t.elapsed();
    assert_eq!(m.len, size);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    // The repeated fixture has a continuity break at every join, which
    // costs a few points; the PAT is still found.
    assert!(m.confidence >= Confidence::HIGH.lowered(5));
    // Streamed: no single request is larger than a block, and the whole
    // stream is read about once (plus the head and tail windows).
    let max = counting.max_request.load(Ordering::Relaxed);
    let total = counting.total.load(Ordering::Relaxed);
    assert!(max <= 1 << 20, "largest request {max}");
    assert!(
        total >= size && total < size + (4 << 20),
        "read {total} of {size}"
    );
    eprintln!(
        "300 MB TS measured in {elapsed:?}, {} calls",
        counting.calls.load(Ordering::Relaxed)
    );
    assert!(elapsed.as_secs() < 60, "{elapsed:?}");
}

#[test]
fn limit_caps_the_run() {
    let Some(f) = ts_h264_aac() else { return };
    let m = MpegTsFormat
        .measure(&mem_slice(&f), 0, 100_000)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Capped);
    assert!(m.len <= 100_000 && m.len > 100_000 - 188);
}

#[test]
fn probe_matches_signatures() {
    let Some(f) = ts_h264_aac() else { return };
    let fmt = MpegTsFormat;
    assert!(fmt.probe(&f[..65536.min(f.len())]));
    assert!(fmt.signatures().iter().any(|s| s.matches(&f[..400])));
    let m2 = to_m2ts(&f);
    assert!(fmt.probe(&m2[..65536]));
    assert!(fmt.signatures().iter().any(|s| s.matches(&m2[..400])));
    assert!(!fmt.probe(&Rng(4).bytes(65536)));
    assert!(!fmt.probe(&[]));
}

#[test]
fn hostile_input_never_panics() {
    let fmt = MpegTsFormat;
    let mut rng = Rng(0x7E);
    for i in 0..200 {
        // A syntactically plausible run of packets with random content.
        let n = (2 + rng.next() % 60) as usize;
        let stride = if i % 2 == 0 { 188 } else { 192 };
        let mut d = Vec::new();
        for _ in 0..n {
            let mut p = rng.bytes(stride);
            let off = if stride == 192 { 4 } else { 0 };
            p[off] = 0x47;
            if rng.next() % 10 == 0 {
                p[off] = 0;
            }
            d.extend_from_slice(&p);
        }
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    let _ = fmt.measure(&mem(Vec::new()), 0, 5);
    let Some(f) = ts_h264_aac() else { return };
    let mut len = 0usize;
    while len < 6000 {
        let s = mem_slice(&f[..len]);
        if let Some(m) = fmt.measure(&s, 0, u64::MAX).unwrap() {
            assert!(m.len <= len as u64);
            assert_eq!(m.len % 188, 0);
        }
        let _ = fmt.validate(&s);
        len += 17;
    }
    // Corrupt PSI and adaptation fields.
    for off in (0..1200).step_by(3) {
        let mut d = f[..188 * 40].to_vec();
        d[off] ^= 0xFF;
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
}

#[test]
fn hevc_transport_stream_dimensions() {
    if !need_encoders(&["libx265", "aac"]) {
        return;
    }
    let f = fixture("ts_hevc", || {
        av(
            &[
                "-t",
                "2",
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-x265-params",
                "log-level=none",
                "-c:a",
                "aac",
                "-f",
                "mpegts",
            ],
            "o.ts",
        )
    });
    let m = measure(&f).unwrap();
    assert_eq!(m.meta.video_codec.as_deref(), Some("hevc"));
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.height, Some(240));
    assert_eq!(m.len, f.len() as u64);
}

#[test]
fn full_hd_h264_dimensions_use_the_cropping_rectangle() {
    if !need_encoders(&["libx264", "aac"]) {
        return;
    }
    let f = fixture("ts_1080", || {
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1920x1080:rate=30",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-f",
                "mpegts",
            ],
            "o.ts",
        )
    });
    let m = measure(&f).unwrap();
    assert_eq!((m.meta.width, m.meta.height), (Some(1920), Some(1080)));
}
