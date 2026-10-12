//! Matroska / WebM: measuring, validation and metadata.

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, NestedRole, ValidationStatus};
use diskrescue_video::MkvFormat;

fn measure(data: &[u8]) -> Option<diskrescue_core::format::Measured> {
    MkvFormat.measure(&mem_slice(data), 0, u64::MAX).unwrap()
}

#[test]
fn matroska_known_size_is_exact_before_random_data() {
    let Some(f) = mkv_h264_aac() else { return };
    let data = with_trailer(&f, 30_000, 5);
    let m = measure(&data).expect("recognised");
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.confidence.value() >= 90);
    assert_eq!(m.err_offset, None);
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.height, Some(240));
    assert_eq!(m.meta.video_codec.as_deref(), Some("h264"));
    assert_eq!(m.meta.audio_codec.as_deref(), Some("aac"));
    let d = m.meta.duration_ms.unwrap();
    assert!((2900..=3200).contains(&d), "duration {d}");
    assert_eq!(m.meta.extra["extension"], "mkv");
    assert!(m.meta.software.as_deref().unwrap_or("").starts_with("Lavf"));
    assert_eq!(m.meta.extra["audio_sample_rate"], "48000");
}

#[test]
fn creation_time_comes_from_dateutc() {
    let Some(f) = mkv_h264_aac() else { return };
    let m = measure(&f).unwrap();
    // ffmpeg writes DateUTC when creation_time metadata is given.
    if let Some(t) = m.meta.created {
        assert_eq!(t.to_rfc3339(), "2024-05-01T12:30:00Z");
    } else {
        eprintln!("ffmpeg did not write DateUTC; skipping the date assertion");
    }
}

#[test]
fn webm_files_get_the_webm_extension() {
    for (f, vc, ac) in [
        (webm_vp8_vorbis(), "vp8", "vorbis"),
        (webm_vp9_opus(), "vp9", "opus"),
    ] {
        let Some(f) = f else { continue };
        let data = with_trailer(&f, 4096, 9);
        let m = measure(&data).unwrap();
        assert_eq!(m.len, f.len() as u64);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert_eq!(m.meta.extra["extension"], "webm");
        assert_eq!(m.meta.video_codec.as_deref(), Some(vc));
        assert_eq!(m.meta.audio_codec.as_deref(), Some(ac));
        assert_eq!(m.meta.width, Some(320));
        let v = MkvFormat.validate(&mem_slice(&f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    }
}

#[test]
fn live_files_with_unknown_sizes_are_unknown_length() {
    let Some(f) = mkv_live() else { return };
    // Sanity: the segment size really is unknown (01 FF FF FF FF FF FF FF).
    assert!(f
        .windows(8)
        .take(200)
        .any(|w| w == [1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]));
    let data = with_trailer(&f, 20_000, 11);
    let m = measure(&data).expect("recognised");
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.err_offset, None);
    assert_eq!(m.meta.video_codec.as_deref(), Some("h264"));
    // Live files have no Duration; it is estimated from the clusters.
    let d = m.meta.duration_ms.expect("estimated duration");
    assert!((2000..=3300).contains(&d), "duration {d}");
    assert_eq!(
        m.meta.extra.get("duration_source").map(String::as_str),
        Some("last cluster")
    );
    assert!(m.notes.iter().any(|n| n.contains("unknown size")));
    // Source ends cleanly: the same.
    let m2 = measure(&f).unwrap();
    assert_eq!(m2.len, f.len() as u64);
    assert_eq!(m2.status, ExtentStatus::UnknownLength);
    // The file validates (a live file is a legal file).
    let v = MkvFormat.validate(&mem_slice(&f)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn truncated_live_file_ends_at_the_last_complete_block() {
    let Some(f) = mkv_live() else { return };
    for cut in [60_000usize, 100_001, 150_000, 150_001, 150_002] {
        let part = &f[..cut];
        let m = measure(part).expect("recognised");
        assert!(
            m.len <= cut as u64 && m.len > cut as u64 - 40_000,
            "len {} cut {cut}",
            m.len
        );
        // The prefix [..len] measures to itself.
        let again = measure(&part[..m.len as usize]).unwrap();
        assert_eq!(again.len, m.len, "cut {cut}");
        // The clusters of this file have sizes, so the cut cluster is
        // reported as truncated.
        assert_eq!(m.status, ExtentStatus::Truncated, "cut {cut}");
        assert_eq!(m.err_offset, Some(m.len));
    }
}

#[test]
fn truncated_sized_file_is_truncated_not_complete() {
    let Some(f) = mkv_h264_aac() else { return };
    let cut = f.len() * 2 / 3;
    let m = measure(&f[..cut]).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut as u64);
    assert_eq!(m.err_offset, Some(m.len));
    let v = MkvFormat.validate(&mem_slice(&f[..cut])).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v.intact_fraction.unwrap() < 1.0);
    assert!(!v.problems.is_empty());
    // Cut exactly at a cluster boundary is still truncated (segment size lies).
    let m2 = measure(&f[..m.len as usize]).unwrap();
    assert_eq!(m2.status, ExtentStatus::Truncated);
    assert_eq!(m2.len, m.len);
}

#[test]
fn cover_art_is_reported_as_nested() {
    let Some((f, jpg)) = mkv_with_cover() else {
        return;
    };
    let m = measure(&f).unwrap();
    assert_eq!(m.nested.len(), 1, "{:?}", m.nested);
    let n = &m.nested[0];
    assert_eq!(n.role, NestedRole::CoverArt);
    assert_eq!(n.format.as_deref(), Some("jpeg"));
    assert_eq!(&f[n.range.start as usize..n.range.end as usize], &jpg[..]);
    assert!(MkvFormat.may_contain("jpeg"));
    assert!(MkvFormat.may_contain("png"));
    assert!(!MkvFormat.may_contain("gif"));
}

#[test]
fn mka_extension_for_audio_only() {
    if !need_encoders(&["libopus"]) {
        return;
    }
    let f = fixture("mka_opus", || {
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:a",
                "libopus",
                "-f",
                "matroska",
            ],
            "o.mka",
        )
    });
    let m = measure(&f).unwrap();
    assert_eq!(m.meta.extra["extension"], "mka");
    assert_eq!(m.meta.video_codec, None);
    assert_eq!(m.meta.audio_codec.as_deref(), Some("opus"));
    assert_eq!(m.len, f.len() as u64);
}

#[test]
fn probe_and_rejection() {
    let Some(f) = mkv_h264_aac() else { return };
    assert!(MkvFormat.probe(&f[..4096]));
    assert!(!MkvFormat.probe(b"\x1A\x45\xDF\xA3"));
    assert!(!MkvFormat.probe(&[0u8; 4096]));
    // An EBML header with a foreign DocType is not ours.
    let mut other = f[..200].to_vec();
    if let Some(p) = other.windows(8).position(|w| w == b"matroska") {
        other[p..p + 8].copy_from_slice(b"mkvother");
        assert!(!MkvFormat.probe(&other));
        assert!(measure(&other).is_none());
    }
    // A header and nothing else is not worth carving.
    let hdr_only = &f[..f
        .windows(4)
        .position(|w| w == [0x18, 0x53, 0x80, 0x67])
        .unwrap()];
    assert!(measure(hdr_only).is_none());
}

#[test]
fn hostile_input_never_panics() {
    let mut rng = Rng(0x3C);
    let fmt = MkvFormat;
    for i in 0..300 {
        let n = 12 + (rng.next() % 3000) as usize;
        let mut d = rng.bytes(n);
        // A plausible EBML header + Segment start in front of noise.
        let head: &[u8] = &[
            0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81, 0x01, 0x42, 0xF7, 0x81, 0x01, 0x42,
            0xF2, 0x81, 0x04, 0x42, 0xF3, 0x81, 0x08, 0x42, 0x82, 0x88, b'm', b'a', b't', b'r',
            b'o', b's', b'k', b'a', 0x42, 0x87, 0x81, 0x04, 0x42, 0x85, 0x81, 0x02,
        ];
        if i % 3 != 0 && d.len() > head.len() {
            d[..head.len()].copy_from_slice(head);
            if d.len() > head.len() + 12 {
                d[head.len()..head.len() + 4].copy_from_slice(&[0x18, 0x53, 0x80, 0x67]);
                if i % 2 == 0 {
                    d[head.len() + 4..head.len() + 12]
                        .copy_from_slice(&[1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
                }
            }
        }
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    let _ = fmt.measure(&mem(Vec::new()), 0, 10);
}

#[test]
fn truncation_and_absurd_sizes_never_panic() {
    let Some(f) = mkv_live().or_else(mkv_h264_aac) else {
        return;
    };
    let fmt = MkvFormat;
    let mut rng = Rng(21);
    let mut len = 0usize;
    while len < f.len() {
        let s = mem_slice(&f[..len]);
        if let Some(m) = fmt.measure(&s, 0, u64::MAX).unwrap() {
            assert!(m.len <= len as u64);
        }
        let _ = fmt.validate(&s);
        len += 1 + (rng.next() % 5000) as usize;
    }
    // Corrupt every byte of the first 600 bytes.
    for off in 0..600 {
        for v in [0u8, 0xFF, 0x7F, 0x01, 0x80] {
            let mut d = f[..20_000].to_vec();
            d[off] = v;
            let s = mem(d);
            let _ = fmt.measure(&s, 0, u64::MAX);
            let _ = fmt.validate(&s);
        }
    }
}

/// Top-level children of the Segment: (id, header position, header length).
fn segment_children(f: &[u8]) -> Vec<(u32, usize, usize, Option<u64>)> {
    fn vint(f: &[u8], p: usize, keep: bool) -> (u64, usize, bool) {
        let n = f[p].leading_zeros() as usize + 1;
        let mut v = 0u64;
        for &b in &f[p..p + n] {
            v = (v << 8) | u64::from(b);
        }
        if !keep {
            v &= (1u64 << (7 * n)) - 1;
            return (v, n, v == (1u64 << (7 * n)) - 1);
        }
        (v, n, false)
    }
    let (_, il, _) = vint(f, 0, true);
    let (esz, sl, _) = vint(f, il, false);
    let mut p = il + sl + esz as usize;
    let (_, il, _) = vint(f, p, true);
    let (_, sl, _) = vint(f, p + il, false);
    p += il + sl;
    let mut out = Vec::new();
    while p + 2 < f.len() {
        let (id, il, _) = vint(f, p, true);
        let (sz, sl, unk) = vint(f, p + il, false);
        out.push((id as u32, p, il + sl, if unk { None } else { Some(sz) }));
        if unk {
            break;
        }
        p += il + sl + sz as usize;
    }
    out
}

/// Rewrite every 8-byte Cluster size as "unknown", like a recorder that
/// never goes back to patch sizes.
fn with_unknown_cluster_sizes(f: &[u8]) -> Vec<u8> {
    let mut d = f.to_vec();
    for (id, p, hdr, size) in segment_children(f) {
        if id == 0x1F43_B675 && size.is_some() {
            let n = hdr - 4;
            d[p + 4] = ((2u32 << (8 - n)) - 1) as u8;
            for b in &mut d[p + 5..p + 4 + n] {
                *b = 0xFF;
            }
        }
    }
    d
}

#[test]
fn unknown_size_clusters_are_walked_block_by_block() {
    let Some(f) = mkv_live() else { return };
    let u = with_unknown_cluster_sizes(&f);
    assert!(u != *f);
    let data = with_trailer(&u, 10_000, 4);
    let m = measure(&data).unwrap();
    assert_eq!(m.len, f.len() as u64);
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    let d = m.meta.duration_ms.unwrap();
    assert!((2000..=3300).contains(&d), "duration {d}");
    // Cut mid-block: ends at the last complete block, truncated.
    let cut = 150_000usize;
    let m = measure(&u[..cut]).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut as u64 && m.len > cut as u64 - 20_000);
    // The prefix up to the last complete block is a clean live file.
    let again = measure(&u[..m.len as usize]).unwrap();
    assert_eq!(again.len, m.len);
    assert_eq!(again.status, ExtentStatus::UnknownLength);
    if need("ffmpeg") {
        let (_d, p) = write_temp("t.mkv", &u[..m.len as usize]);
        decodes_cleanly(&p).expect("decodes without errors");
    }
}
