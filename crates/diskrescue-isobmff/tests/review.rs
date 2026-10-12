//! Regressions from the review: carving measurements that over-claim,
//! ftyp-less QuickTime, holes in the media data (rebuild and repair with
//! an intact index), extra audio tracks, gaps from tracks the reference
//! does not rebuild, variable frame rate, unreadable sectors and long
//! PCM tracks.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use diskrescue_core::format::{
    ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_core::source::MemSource;
use diskrescue_core::{RangeSet, SharedSource};
use diskrescue_isobmff::rebuild::{learn, rebuild, RebuildOptions, RebuildReport};
use diskrescue_isobmff::writer::BoxBuf;
use diskrescue_isobmff::{formats, Cr3Format, HeifFormat, Mp4Format};

/// Rebuild `damaged` with the named reference fixture.
fn rebuild_with(
    name: &str,
    damaged: SharedSource,
    reference: &Path,
) -> (std::path::PathBuf, RebuildReport) {
    let r = learn(&open(reference)).expect("learn reference");
    let out_path = scratch(&format!("{name}.rebuilt.mp4"));
    let mut out = std::fs::File::create(&out_path).unwrap();
    let rep = rebuild(&damaged, &r, &mut out, &RebuildOptions::default()).expect("rebuild");
    drop(out);
    (out_path, rep)
}

/// Zero `n` holes of `len` bytes evenly inside the first mdat payload.
fn zero_holes(b: &mut [u8], n: usize, len: usize) -> Vec<std::ops::Range<usize>> {
    let (_, p, s) = top_boxes(b)
        .into_iter()
        .find(|t| &t.0 == b"mdat")
        .expect("mdat");
    let (start, end) = (p + 8, p + s);
    let mut out = Vec::new();
    for i in 0..n {
        let at = start + (end - start) * (2 * i + 2) / (2 * n + 1);
        let r = at..(at + len).min(end);
        b[r.clone()].iter_mut().for_each(|x| *x = 0);
        out.push(r);
    }
    out
}

fn audio_tracks(p: &Path) -> Vec<StreamInfo> {
    streams(p)
        .into_iter()
        .filter(|s| s.kind == "audio")
        .collect()
}

// ---------------------------------------------------------------------------
// measure()
// ---------------------------------------------------------------------------

#[test]
fn cut_faststart_movie_followed_by_other_data_is_not_complete() {
    if !ffmpeg_or_skip() {
        return;
    }
    let bytes = std::fs::read(fixture("h264_faststart")).unwrap();
    let other = std::fs::read(fixture("h264_ref")).unwrap();
    let cut = bytes.len() * 6 / 10;
    for (what, tail) in [("noise", noise(bytes.len(), 9)), ("another file", other)] {
        let mut img = bytes[..cut].to_vec();
        img.extend_from_slice(&tail);
        let m = Mp4Format
            .measure(&mem(img), 0, u64::MAX)
            .unwrap()
            .expect("measured");
        assert_ne!(m.status, ExtentStatus::Complete, "{what}: {m:?}");
        // At most the sample spanning the cut is over-claimed (random bytes
        // after its start cannot be told from its payload).
        assert!(
            m.len < cut as u64 + 32 * 1024 && m.len + 64 * 1024 > cut as u64,
            "{what}: claimed {} of {cut} real bytes",
            m.len
        );
    }
    // The intact file is still Complete at full length.
    let m = Mp4Format
        .measure(&mem(bytes.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        (m.status, m.len),
        (ExtentStatus::Complete, bytes.len() as u64)
    );
}

/// Patch the size of the first top-level box of type `kind`.
fn set_box_size(b: &mut [u8], kind: &[u8; 4], size: u32) {
    let (_, p, _) = top_boxes(b).into_iter().find(|t| &t.0 == kind).unwrap();
    b[p..p + 4].copy_from_slice(&size.to_be_bytes());
}

#[test]
fn sample_tables_past_eof_are_never_complete() {
    if !ffmpeg_or_skip() {
        return;
    }
    let bytes = std::fs::read(fixture("h264_faststart")).unwrap();
    let cut = bytes.len() * 7 / 10;
    // The mdat size is patched to end exactly at the cut: the boxes are
    // consistent, the tables are not.
    let mut b = bytes[..cut].to_vec();
    let (_, p, _) = top_boxes(&bytes)
        .into_iter()
        .find(|t| &t.0 == b"mdat")
        .unwrap();
    set_box_size(&mut b, b"mdat", (cut - p) as u32);
    let m = Mp4Format
        .measure(&mem(b.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_ne!(m.status, ExtentStatus::Complete, "{m:?}");
    // Same with an unusable stsc (first_chunk 0) in every track: the
    // chunk offsets alone still show data past the end.
    let mut bad = b.clone();
    let mut i = 0;
    while let Some(k) = bad[i..].windows(4).position(|w| w == b"stsc") {
        let at = i + k + 4 + 4 + 4; // type, version/flags, entry_count
        bad[at..at + 4].copy_from_slice(&0u32.to_be_bytes());
        i = at;
    }
    let m = Mp4Format.measure(&mem(bad), 0, u64::MAX).unwrap().unwrap();
    assert_ne!(m.status, ExtentStatus::Complete, "{m:?}");
}

#[test]
fn unreadable_sectors_are_noted_without_a_second_penalty() {
    if !ffmpeg_or_skip() {
        return;
    }
    let bytes = std::fs::read(fixture("h264_faststart")).unwrap();
    let clean = Mp4Format
        .measure(&mem(bytes.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    let mid = bytes.len() as u64 / 2;
    let holed: SharedSource = Arc::new(MemSource::new(bytes).with_unreadable(
        RangeSet::from_range(diskrescue_core::Range::new(mid, mid + 4096)),
    ));
    let m = Mp4Format.measure(&holed, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.confidence, clean.confidence);
    assert!(m.notes.iter().any(|n| n.contains("unreadable")), "{m:?}");
}

#[test]
fn containers_may_contain_tiff() {
    for f in formats() {
        assert!(f.may_contain("tiff"), "{}", f.id());
    }
    assert!(Cr3Format.may_contain("jpeg") && HeifFormat.may_contain("jpeg"));
}

#[test]
fn quicktime_without_ftyp_is_carved() {
    if !ffmpeg_or_skip() {
        return;
    }
    let f = Mp4Format;
    assert!(f.signatures().iter().any(|s| s.bytes == b"moov"));
    // MOV: ftyp wide mdat moov -> wide mdat moov; faststart MP4 -> moov first.
    for name in ["mov", "h264_faststart"] {
        let orig = std::fs::read(fixture(name)).unwrap();
        let ftyp_len = top_boxes(&orig)[0].2 as u64;
        let mut b = strip_box(&orig, b"ftyp");
        // Without the ftyp every chunk offset moves down.
        let (_, p, sz) = top_boxes(&b).into_iter().find(|t| &t.0 == b"moov").unwrap();
        let mut moov = b[p..p + sz].to_vec();
        diskrescue_isobmff::repair::patch_chunk_offsets(&mut moov, &|o| o.checked_sub(ftyp_len))
            .unwrap();
        b[p..p + sz].copy_from_slice(&moov);
        assert!(f.probe(&b[..b.len().min(64 << 10)]), "{name}");
        let mut img = b.clone();
        img.extend(noise(10_000, 3));
        let m = f.measure(&mem(img), 0, u64::MAX).unwrap().expect(name);
        assert_eq!(m.len, b.len() as u64, "{name}: {m:?}");
        assert_eq!(m.status, ExtentStatus::Complete, "{name}: {m:?}");
    }
    // Random hits are rejected by probe.
    for kind in [b"moov", b"mdat", b"wide", b"free", b"skip"] {
        for seed in 0..50u64 {
            let mut h = noise(4096, seed + 100);
            h[4..8].copy_from_slice(kind);
            h[0..4].copy_from_slice(&(512u32 + seed as u32).to_be_bytes());
            assert!(
                !f.probe(&h),
                "{} seed {seed}",
                String::from_utf8_lossy(kind)
            );
        }
    }
}

/// Encode one HEVC intra frame and wrap it as a HEIC still: ftyp, meta
/// (hdlr, pitm, iinf, iprp with hvcC/ispe, iloc) and mdat with the
/// length-prefixed slice NAL units.
fn real_heic() -> Option<Vec<u8>> {
    let out = scratch("still.hevc");
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=320x240:rate=1")
        .args([
            "-frames:v",
            "1",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=none:keyint=1",
            "-f",
            "hevc",
        ])
        .arg(&out)
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let annexb = std::fs::read(out).ok()?;
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(annexb.len(), |&n| n - 3);
        while e > s && annexb[e - 1] == 0 {
            e -= 1;
        }
        nals.push(&annexb[s..e]);
    }
    let (params, slices): (Vec<&[u8]>, Vec<&[u8]>) = nals
        .into_iter()
        .partition(|n| (32..=34).contains(&((n[0] >> 1) & 0x3F)));
    let mut data = Vec::new();
    for s in slices.iter().filter(|n| ((n[0] >> 1) & 0x3F) <= 21) {
        data.extend((s.len() as u32).to_be_bytes());
        data.extend_from_slice(s);
    }
    let meta = |offset: u32| -> Vec<u8> {
        let mut b = BoxBuf::new();
        b.start_full(b"meta", 0, 0);
        b.start_full(b"hdlr", 0, 0);
        b.u32(0);
        b.bytes(b"pict");
        b.zeros(13);
        b.end();
        b.start_full(b"pitm", 0, 0);
        b.u16(1);
        b.end();
        b.start_full(b"iinf", 0, 0);
        b.u16(1);
        b.start_full(b"infe", 2, 0);
        b.u16(1);
        b.u16(0);
        b.bytes(b"hvc1");
        b.u8(0);
        b.end();
        b.end();
        b.start(b"iprp");
        b.start(b"ipco");
        b.start(b"hvcC");
        b.u8(1);
        b.u8(1);
        b.zeros(10);
        b.u8(60);
        b.bytes(&[0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0, 0x0F]);
        b.u8(params.len() as u8);
        for p in &params {
            b.u8(0x80 | ((p[0] >> 1) & 0x3F));
            b.u16(1);
            b.u16(p.len() as u16);
            b.bytes(p);
        }
        b.end();
        b.start_full(b"ispe", 0, 0);
        b.u32(320);
        b.u32(240);
        b.end();
        b.end();
        b.start_full(b"ipma", 0, 0);
        b.u32(1);
        b.u16(1);
        b.u8(2);
        b.u8(0x81);
        b.u8(0x82);
        b.end();
        b.end();
        b.start_full(b"iloc", 0, 0);
        b.u8(0x44);
        b.u8(0);
        b.u16(1);
        b.u16(1);
        b.u16(0);
        b.u16(1);
        b.u32(offset);
        b.u32(data.len() as u32);
        b.end();
        b.end();
        b.buf
    };
    let mut ftyp = BoxBuf::new();
    ftyp.start(b"ftyp");
    ftyp.bytes(b"heic");
    ftyp.u32(0);
    ftyp.bytes(b"mif1heic");
    ftyp.end();
    let probe = meta(0);
    let offset = (ftyp.buf.len() + probe.len() + 8) as u32;
    let mut file = ftyp.buf.clone();
    file.extend(meta(offset));
    file.extend(((data.len() + 8) as u32).to_be_bytes());
    file.extend(b"mdat");
    file.extend(&data);
    Some(file)
}

#[test]
fn cut_heic_followed_by_other_data_is_not_complete() {
    if !ffmpeg_or_skip() {
        return;
    }
    let Some(heic) = real_heic() else {
        eprintln!("skipped: libx265 missing");
        return;
    };
    let full = HeifFormat
        .measure(&mem(heic.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        (full.status, full.len),
        (ExtentStatus::Complete, heic.len() as u64)
    );
    let cut = heic.len() * 6 / 10;
    // Followed by zeroed free space and by another file.
    let tails = [
        vec![0u8; heic.len()],
        std::fs::read(fixture("h264")).unwrap(),
    ];
    for tail in tails {
        let mut img = heic[..cut].to_vec();
        img.extend_from_slice(&tail);
        let m = HeifFormat.measure(&mem(img), 0, u64::MAX).unwrap().unwrap();
        assert_ne!(m.status, ExtentStatus::Complete, "{m:?}");
        assert!(m.len <= cut as u64, "claimed {} of {cut}", m.len);
    }
}

#[test]
fn iloc_bomb_is_bounded() {
    // iloc v1 with all field sizes 0: every extent takes no bytes; 65 535
    // items each declaring 65 535 extents.
    let mut b = BoxBuf::new();
    b.start(b"ftyp");
    b.bytes(b"heic");
    b.u32(0);
    b.bytes(b"mif1heic");
    b.end();
    b.start_full(b"meta", 0, 0);
    b.start_full(b"iloc", 1, 0);
    b.u8(0);
    b.u8(0);
    b.u16(0xFFFF);
    for id in 0..0xFFFFu16 {
        b.u16(id);
        b.u16(0); // construction method
        b.u16(0); // data reference index
        b.u16(0xFFFF);
    }
    b.end();
    b.end();
    b.start(b"mdat");
    b.zeros(64);
    b.end();
    let started = Instant::now();
    let src = mem(b.buf);
    let _ = HeifFormat.measure(&src, 0, u64::MAX).unwrap();
    let _ = HeifFormat.validate(&src).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    // Extents at absurd offsets: no overflow, reported outside the file.
    let mut b = BoxBuf::new();
    b.start(b"ftyp");
    b.bytes(b"heic");
    b.u32(0);
    b.bytes(b"mif1heic");
    b.end();
    b.start_full(b"meta", 0, 0);
    b.start_full(b"pitm", 0, 0);
    b.u16(1);
    b.end();
    b.start_full(b"iinf", 0, 0);
    b.u16(1);
    b.start_full(b"infe", 2, 0);
    b.u16(1);
    b.u16(0);
    b.bytes(b"Exif");
    b.u8(0);
    b.end();
    b.end();
    b.start_full(b"iloc", 0, 0);
    b.u8(0x88);
    b.u8(0);
    b.u16(1);
    b.u16(1);
    b.u16(0);
    b.u16(1);
    b.u64(u64::MAX - 5);
    b.u64(100);
    b.end();
    b.end();
    b.start(b"mdat");
    b.zeros(64);
    b.end();
    let src = mem(b.buf);
    let v = HeifFormat.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged, "{v:?}");
    let _ = HeifFormat.measure(&src, 0, u64::MAX).unwrap();
}

// ---------------------------------------------------------------------------
// rebuild()
// ---------------------------------------------------------------------------

#[test]
fn hevc_open_gop_with_holes_decodes_clean_and_in_sync() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("hevc_open");
    let mut dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    zero_holes(&mut dmg, 2, 6000);
    let (out, rep) = rebuild_with("hevc_open_holes", mem(dmg), &fixture("hevc_open_ref"));
    decodes_clean(&out).unwrap_or_else(|e| panic!("{e}\n{rep:?}"));
    let (worst, median, v, a) = av_sync_error(&orig, &out);
    assert!(
        v > 30 && a > 60,
        "{v} video / {a} audio packets matched; {rep:?}"
    );
    // Right at a hole the tracks may have lost slightly different amounts
    // (which cannot be known); elsewhere they stay in sync.
    assert!(
        median <= 0.04 && worst <= 0.2,
        "A/V sync off by {median:.3} s (worst {worst:.3} s); {rep:?}"
    );
}

#[test]
fn h264_with_holes_decodes_clean_and_in_sync() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let mut dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    zero_holes(&mut dmg, 2, 5000);
    let (out, rep) = rebuild_with("h264_holes2", mem(dmg), &fixture("h264_ref"));
    decodes_clean(&out).unwrap_or_else(|e| panic!("{e}\n{rep:?}"));
    let (worst, median, _, _) = av_sync_error(&orig, &out);
    assert!(
        median <= 0.04 && worst <= 0.2,
        "A/V sync off by {median:.3} s (worst {worst:.3} s); {rep:?}"
    );
}

#[test]
fn unreadable_sectors_in_the_mdat_are_skipped() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let mid = dmg.len() as u64 / 2;
    let bad = RangeSet::from_range(diskrescue_core::Range::new(mid, mid + 4096));
    let src: SharedSource = Arc::new(MemSource::new(dmg).with_unreadable(bad));
    let (out, rep) = rebuild_with("h264_unreadable", src, &fixture("h264_ref"));
    decodes_clean(&out).unwrap_or_else(|e| panic!("{e}\n{rep:?}"));
    assert!(rep.video_samples > 30, "{rep:?}");
}

#[test]
fn second_aac_track_is_rebuilt_not_merged() {
    if !ffmpeg_or_skip() {
        return;
    }
    for name in ["two_aac", "two_aac_mono"] {
        let orig = fixture(name);
        let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
        let (out, rep) = rebuild_with(name, mem(dmg), &fixture(&format!("{name}_ref")));
        decodes_clean(&out).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (o, r) = (audio_tracks(&orig), audio_tracks(&out));
        assert_eq!(r.len(), 2, "{name}: {rep:?}");
        for (a, b) in o.iter().zip(&r) {
            assert!(
                (a.duration - b.duration).abs() <= 0.05,
                "{name}: audio {} s, original {} s",
                b.duration,
                a.duration
            );
        }
        let (worst, _, _, _) = av_sync_error(&orig, &out);
        assert!(worst <= 0.05, "{name}: A/V sync off by {worst:.3} s");
    }
}

#[test]
fn text_track_samples_do_not_break_b_frame_order() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("subs");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = rebuild_with("subs", mem(dmg), &fixture("subs_ref"));
    decodes_clean(&out).unwrap_or_else(|e| panic!("{e}\n{rep:?}"));
    let pts = frame_pts(&out);
    assert!(
        pts.windows(2).all(|w| w[1] > w[0]),
        "{:?}",
        &pts[..pts.len().min(20)]
    );
    assert_eq!(
        rep.video_samples,
        packets(&orig, "v").len() as u64,
        "{rep:?}"
    );
}

#[test]
fn pcm_hole_does_not_turn_video_into_audio() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("mov_pcm");
    let mut dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    zero_holes(&mut dmg, 1, 8000);
    let (out, rep) = rebuild_with("mov_pcm_hole", mem(dmg), &fixture("mov_pcm_ref"));
    decodes_clean(&out).unwrap_or_else(|e| panic!("{e}\n{rep:?}"));
    let o = audio_tracks(&orig);
    let r = audio_tracks(&out);
    // The hole may come back as silence, never as seconds of video bytes.
    assert!(
        r[0].duration <= o[0].duration + 0.25,
        "{} vs {}",
        r[0].duration,
        o[0].duration
    );
}

#[test]
fn variable_frame_rate_follows_the_audio_clock() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("vfr_hevc");
    let dmg = strip_box(&std::fs::read(&orig).unwrap(), b"moov");
    let (out, rep) = rebuild_with("vfr_hevc", mem(dmg), &fixture("vfr_hevc_ref"));
    decodes_clean(&out).unwrap();
    let (worst, median, v, _) = av_sync_error(&orig, &out);
    assert!(v > 80, "{rep:?}");
    assert!(
        median <= 0.05 && worst <= 0.1,
        "A/V sync off by {median:.3} s (worst {worst:.3} s); {rep:?}"
    );
}

// ---------------------------------------------------------------------------
// repair() with the index intact
// ---------------------------------------------------------------------------

#[test]
fn holes_with_an_intact_index_drop_only_damaged_samples() {
    if !ffmpeg_or_skip() {
        return;
    }
    let orig = fixture("h264_faststart");
    let mut b = std::fs::read(&orig).unwrap();
    zero_holes(&mut b, 2, 5000);
    let src = mem(b);
    let v = Mp4Format.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged, "{v:?}");
    assert!(
        v.repairable_by.iter().any(|r| r == "drop_damaged_samples"),
        "{v:?}"
    );
    let out_path = scratch("faststart_holes.repaired.mp4");
    let mut out = std::fs::File::create(&out_path).unwrap();
    let o = Mp4Format
        .repair(&src, &mut out, &RepairContext::default())
        .unwrap();
    drop(out);
    assert_eq!(o.result, RepairResult::PartiallyRepaired, "{o:?}");
    decodes_clean(&out_path).unwrap_or_else(|e| panic!("{e}\n{o:?}"));
    let (worst, _, vids, _) = av_sync_error(&orig, &out_path);
    assert!(worst <= 0.04, "timing changed by {worst:.3} s");
    assert!(vids > 40, "{o:?}");
}

#[test]
fn long_pcm_track_is_not_damaged_and_clips() {
    if !ffmpeg_or_skip() {
        return;
    }
    // 25 s of 8-bit mono PCM: 1.2 million one-byte samples.
    let path = fixture("long_pcm");
    let bytes = std::fs::read(&path).unwrap();
    let v = Mp4Format.validate(&mem(bytes.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{v:?}");
    let m = Mp4Format
        .measure(&mem(bytes.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        (m.status, m.len),
        (ExtentStatus::Complete, bytes.len() as u64)
    );
    // Moved to the front and cut: the tables are clipped chunk by chunk.
    let moov = top_boxes(&bytes)
        .into_iter()
        .find(|t| &t.0 == b"moov")
        .unwrap();
    let mdat = top_boxes(&bytes)
        .into_iter()
        .find(|t| &t.0 == b"mdat")
        .unwrap();
    assert!(moov.1 > mdat.1, "fixture has moov at the end");
    let out_path = scratch("long_pcm.repaired.mov");
    let mut out = std::fs::File::create(&out_path).unwrap();
    let cut = mem(bytes[..bytes.len() * 6 / 10].to_vec());
    let ctx = RepairContext {
        donors: vec![fixture("long_pcm_ref")],
        ..Default::default()
    };
    let o = Mp4Format.repair(&cut, &mut out, &ctx).unwrap();
    drop(out);
    assert!(
        matches!(
            o.result,
            RepairResult::Repaired | RepairResult::PartiallyRepaired
        ),
        "{o:?}"
    );
    decodes_clean(&out_path).unwrap_or_else(|e| panic!("{e}\n{o:?}"));
    // Index first and cut: the PCM table is clipped chunk by chunk.
    let fs = std::fs::read(fixture("long_pcm_fs")).unwrap();
    let cut = mem(fs[..fs.len() * 6 / 10].to_vec());
    let out_path = scratch("long_pcm_fs.clipped.mov");
    let mut out = std::fs::File::create(&out_path).unwrap();
    let o = Mp4Format
        .repair(&cut, &mut out, &RepairContext::default())
        .unwrap();
    drop(out);
    assert!(
        o.applied
            .iter()
            .any(|a| a.starts_with("clip_tables_to_file")),
        "{o:?}"
    );
    decodes_clean(&out_path).unwrap_or_else(|e| panic!("{e}\n{o:?}"));
}
