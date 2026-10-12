//! AVI: measuring, validation, metadata and repair, against files written by
//! ffmpeg (skipped when ffmpeg or an encoder is missing) and hand-built files.

mod common;

use std::sync::Arc;

use common::*;
use diskrescue_core::format::{
    ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_core::SharedSource;
use diskrescue_video::{AviFormat, AviStrategy};

fn mpeg4_pcm() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg4", "pcm_s16le"]).then(|| {
        fixture("avi_mpeg4_pcm", || {
            av(
                &["-t", "3", "-c:v", "mpeg4", "-q:v", "5", "-c:a", "pcm_s16le"],
                "o.avi",
            )
        })
    })
}

fn mjpeg_pcm() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mjpeg", "pcm_s16le"]).then(|| {
        fixture("avi_mjpeg_pcm", || {
            av(
                &["-t", "2", "-c:v", "mjpeg", "-q:v", "3", "-c:a", "pcm_s16le"],
                "o.avi",
            )
        })
    })
}

fn mpeg4_mp3() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg4", "libmp3lame"]).then(|| {
        fixture("avi_mpeg4_mp3", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "5",
                    "-c:a",
                    "libmp3lame",
                ],
                "o.avi",
            )
        })
    })
}

fn h264_aac() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libx264", "aac"]).then(|| {
        fixture("avi_h264_aac", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "15",
                    "-bf",
                    "0",
                    "-c:a",
                    "aac",
                ],
                "o.avi",
            )
        })
    })
}

/// Chunks of a RIFF/LIST payload: (id, size, data offset).
fn chunks(data: &[u8], mut pos: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut v = Vec::new();
    while pos + 8 <= end {
        let id = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        v.push((id, size, pos + 8));
        pos += 8 + size + (size & 1);
    }
    v
}

fn find_top(data: &[u8], id: &[u8; 4], list: Option<&[u8; 4]>) -> Option<(usize, usize)> {
    for (cid, size, d) in chunks(data, 12, data.len()) {
        if &cid == id && list.is_none_or(|l| &data[d..d + 4] == l) {
            return Some((d - 8, size));
        }
    }
    None
}

fn put_u32(data: &mut [u8], off: usize, v: u32) {
    data[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn measure(fmt: &AviFormat, data: &[u8]) -> Option<diskrescue_core::format::Measured> {
    fmt.measure(&mem_slice(data), 0, u64::MAX).unwrap()
}

#[test]
fn measures_exact_length_when_followed_by_random_data() {
    let fmt = AviFormat;
    for (name, f) in [
        ("mpeg4+pcm", mpeg4_pcm()),
        ("mjpeg+pcm", mjpeg_pcm()),
        ("mpeg4+mp3", mpeg4_mp3()),
        ("h264+aac", h264_aac()),
    ] {
        let Some(f) = f else { continue };
        let data = with_trailer(&f, 20_000, 99);
        let m = measure(&fmt, &data).unwrap_or_else(|| panic!("{name}: not recognised"));
        assert_eq!(m.len, f.len() as u64, "{name}: length");
        assert_eq!(m.status, ExtentStatus::Complete, "{name}");
        assert!(m.confidence.value() >= 85, "{name}: {:?}", m.confidence);
        assert_eq!(m.err_offset, None);
        assert_eq!(m.meta.width, Some(320), "{name}");
        assert_eq!(m.meta.height, Some(240), "{name}");
        assert!(m.meta.software.as_deref().unwrap_or("").starts_with("Lavf"));
        assert_eq!(m.meta.extra.get("fps").map(String::as_str), Some("30.000"));
        // Also exact when the limit is exactly the file size.
        let m2 = fmt
            .measure(&mem_slice(&f), 0, f.len() as u64)
            .unwrap()
            .unwrap();
        assert_eq!(m2.len, f.len() as u64);
        assert_eq!(m2.status, ExtentStatus::Complete);
    }
}

#[test]
fn metadata_codecs_and_duration() {
    let fmt = AviFormat;
    if let Some(f) = mpeg4_pcm() {
        let m = measure(&fmt, &f).unwrap().meta;
        assert_eq!(m.video_codec.as_deref(), Some("mpeg4"));
        assert_eq!(m.audio_codec.as_deref(), Some("pcm_s16le"));
        assert_eq!(m.duration_ms, Some(3000));
        assert_eq!(m.extra.get("fourcc").map(String::as_str), Some("FMP4"));
        assert_eq!(m.extra.get("extension").map(String::as_str), Some("avi"));
    }
    if let Some(f) = mjpeg_pcm() {
        let m = measure(&fmt, &f).unwrap().meta;
        assert_eq!(m.video_codec.as_deref(), Some("mjpeg"));
        assert_eq!(m.duration_ms, Some(2000));
    }
    if let Some(f) = mpeg4_mp3() {
        let m = measure(&fmt, &f).unwrap().meta;
        assert_eq!(m.audio_codec.as_deref(), Some("mp3"));
    }
    if let Some(f) = h264_aac() {
        let m = measure(&fmt, &f).unwrap().meta;
        assert_eq!(m.video_codec.as_deref(), Some("h264"));
        assert_eq!(m.audio_codec.as_deref(), Some("aac"));
    }
    assert!(fmt.may_contain("jpeg"));
    assert!(!fmt.may_contain("png"));
}

#[test]
fn healthy_files_validate() {
    let fmt = AviFormat;
    for f in [mpeg4_pcm(), mjpeg_pcm(), mpeg4_mp3(), h264_aac()]
        .into_iter()
        .flatten()
    {
        let v = fmt.validate(&mem_slice(&f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
        assert_eq!(v.intact_fraction, Some(1.0));
        assert!(v.repairable_by.is_empty());
        // A healthy file needs no repair and comes back unchanged.
        let mut out = Vec::new();
        let r = fmt
            .repair(&mem_slice(&f), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::NoRepairNeeded);
        assert_eq!(out, *f);
    }
}

/// Strip the idx1 and zero the RIFF size, as in a recorder that died.
fn strip_index(f: &[u8]) -> Vec<u8> {
    let pos = f.windows(4).rposition(|w| w == b"idx1").unwrap();
    let mut b = f[..pos].to_vec();
    put_u32(&mut b, 4, 0);
    b
}

#[test]
fn stripped_index_is_rebuilt_byte_identical() {
    let fmt = AviFormat;
    for (name, f) in [
        ("mpeg4+pcm", mpeg4_pcm()),
        ("mjpeg+pcm", mjpeg_pcm()),
        ("mpeg4+mp3", mpeg4_mp3()),
        ("h264+aac", h264_aac()),
    ] {
        let Some(f) = f else { continue };
        let damaged = strip_index(&f);
        let src = mem_slice(&damaged);
        // Measuring walks the movi list and reports the missing index.
        let m = fmt.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated, "{name}");
        assert_eq!(m.len, damaged.len() as u64, "{name}");
        let v = fmt.validate(&src).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged, "{name}");
        assert!(
            v.repairable_by.iter().any(|s| s == "rebuild_index"),
            "{name}: {:?}",
            v.repairable_by
        );
        let mut out = Vec::new();
        let r = fmt
            .repair(&src, &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::Repaired, "{name}: {:?}", r.notes);
        assert_eq!(r.output_len, out.len() as u64);
        // Same layout, same flags, same counts: identical to what ffmpeg wrote.
        assert_eq!(out.len(), f.len(), "{name}: length");
        if out != *f {
            let first = out.iter().zip(f.iter()).position(|(a, b)| a != b).unwrap();
            panic!("{name}: first difference at {first} of {}", f.len());
        }
        let val = r.validation.unwrap();
        assert_eq!(val.status, ValidationStatus::Valid);
    }
}

#[test]
fn rebuilt_file_is_accepted_and_decodes_fully() {
    if !need("ffprobe") {
        return;
    }
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let damaged = strip_index(&f);
    let mut out = Vec::new();
    fmt.repair(&mem_slice(&damaged), &mut out, &RepairContext::default())
        .unwrap();
    let (_d, p) = write_temp("fixed.avi", &out);
    let probe = ffprobe(&p).expect("ffprobe accepts the repaired file");
    let v = probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["codec_type"] == "video")
        .unwrap();
    assert_eq!(v["nb_read_frames"], "90");
    assert_eq!(v["nb_frames"], "90");
    decodes_cleanly(&p).expect("decodes without errors");
}

#[test]
fn truncated_file_repairs_with_each_strategy() {
    if !need("ffprobe") {
        return;
    }
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let cut = 300_000usize;
    let damaged = &f[..cut];
    let src = mem_slice(damaged);
    let m = fmt.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert!(m.len <= cut as u64 && m.len > cut as u64 - 40_000);
    assert_eq!(m.err_offset, Some(m.len));
    let v = fmt.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v
        .repairable_by
        .iter()
        .any(|s| s == "truncate_to_last_chunk"));
    assert!(v.intact_fraction.unwrap() < 1.0);

    for strategy in [AviStrategy::TruncateToLastChunk, AviStrategy::RebuildIndex] {
        let mut out = Vec::new();
        let r = fmt
            .repair_with(&src, &mut out, strategy, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::Repaired, "{strategy:?}: {:?}", r);
        let again = fmt.validate(&mem_slice(&out)).unwrap();
        assert_eq!(
            again.status,
            ValidationStatus::Valid,
            "{:?}",
            again.problems
        );
        let (_d, p) = write_temp("fixed.avi", &out);
        let probe = ffprobe(&p).expect("ffprobe accepts the file");
        let v = probe["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["codec_type"] == "video")
            .unwrap();
        let frames: u64 = v["nb_read_frames"].as_str().unwrap().parse().unwrap();
        assert!(frames > 20 && frames < 90, "frames {frames}");
        // The measured extent of the repaired file is its whole length.
        let mm = fmt.measure(&mem_slice(&out), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(mm.len, out.len() as u64);
        assert_eq!(mm.status, ExtentStatus::Complete);
        decodes_cleanly(&p).expect("decodes without errors");
        if strategy == AviStrategy::RebuildIndex {
            assert!(r.applied.iter().any(|a| a.contains("idx1")));
            assert!(out.windows(4).any(|w| w == b"idx1"));
        } else {
            assert!(!out.windows(4).rev().take(out.len()).any(|w| w == b"idx1"));
        }
    }
}

#[test]
fn fix_sizes_patches_only_size_fields() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let mut damaged = f.to_vec();
    put_u32(&mut damaged, 4, 12345); // RIFF size
    let (movi, _) = find_top(&damaged, b"LIST", Some(b"movi")).unwrap();
    put_u32(&mut damaged, movi + 4, 0); // movi size
    let (hdrl, _) = find_top(&damaged, b"LIST", Some(b"hdrl")).unwrap();
    put_u32(&mut damaged, hdrl + 8 + 4 + 8 + 16, 7); // avih.dwTotalFrames
    let src = mem_slice(&damaged);
    let v = fmt.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v.repairable_by.iter().any(|s| s == "fix_sizes"), "{:?}", v);
    let mut out = Vec::new();
    let r = fmt
        .repair_with(
            &src,
            &mut out,
            AviStrategy::FixSizes,
            &RepairContext::default(),
        )
        .unwrap();
    assert_eq!(r.result, RepairResult::Repaired, "{:?}", r);
    assert_eq!(out, *f);
}

#[test]
fn unknown_codec_marks_all_frames_keyframes_with_a_note() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    // Rename the codec fourcc in strh and strf so it is not recognised.
    let mut b = strip_index(&f);
    for w in 0..b.len() - 4 {
        if &b[w..w + 4] == b"FMP4" && w < 4096 {
            b[w..w + 4].copy_from_slice(b"ZZZ9");
        }
    }
    let mut out = Vec::new();
    let r = fmt
        .repair(&mem_slice(&b), &mut out, &RepairContext::default())
        .unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("flagged as a keyframe")),
        "{:?}",
        r.notes
    );
    // Every video entry carries the keyframe flag.
    let ix = out.windows(4).rposition(|w| w == b"idx1").unwrap();
    let n = u32::from_le_bytes(out[ix + 4..ix + 8].try_into().unwrap()) as usize / 16;
    for i in 0..n {
        let e = &out[ix + 8 + i * 16..ix + 8 + i * 16 + 16];
        assert_eq!(u32::from_le_bytes(e[4..8].try_into().unwrap()) & 0x10, 0x10);
    }
}

#[test]
fn garbage_in_the_middle_is_skipped_only_when_aggressive() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let mut damaged = strip_index(&f);
    // Overwrite 3 KiB inside the movi list with random bytes.
    let at = 60_000;
    damaged[at..at + 3000].copy_from_slice(&Rng(5).bytes(3000));
    let src = mem_slice(&damaged);
    // Default: stop at the damage.
    let mut out = Vec::new();
    let r = fmt
        .repair(&src, &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(r.result, RepairResult::Repaired);
    let plain_len = out.len();
    assert!(plain_len < at + 20_000);
    // Aggressive: resynchronise and keep the rest.
    let mut out2 = Vec::new();
    let ctx = RepairContext {
        aggressive: true,
        ..RepairContext::default()
    };
    let r2 = fmt.repair(&src, &mut out2, &ctx).unwrap();
    assert_eq!(r2.result, RepairResult::Repaired, "{:?}", r2);
    assert!(
        out2.len() > f.len() - 20_000,
        "{} vs {}",
        out2.len(),
        f.len()
    );
    assert!(r2.applied.iter().any(|a| a.contains("skipped")));
    let v = fmt.validate(&mem_slice(&out2)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn header_loss_is_unrepairable_and_writes_nothing() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let mut b = f.to_vec();
    b[..9000].fill(0);
    b[0..12].copy_from_slice(&f[0..12]);
    let mut out = Vec::new();
    let r = fmt
        .repair(&mem_slice(&b), &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(r.result, RepairResult::Unrepairable);
    assert!(out.is_empty());
    assert!(!r.notes.is_empty());
}

/// Build a small OpenDML-style AVI from a normal one: the chunks of `movi`
/// are split over a first `RIFF AVI ` (with idx1) and `n` `RIFF AVIX` chunks.
fn make_opendml(f: &[u8], parts: usize) -> Vec<u8> {
    let (movi_pos, movi_size) = find_top(f, b"LIST", Some(b"movi")).unwrap();
    let kids = chunks(f, movi_pos + 12, movi_pos + 8 + movi_size);
    let per = kids.len().div_ceil(parts);
    let mut groups: Vec<&[([u8; 4], usize, usize)]> = kids.chunks(per).collect();
    let first = groups.remove(0);
    let mut out = f[..movi_pos].to_vec();
    let body_of = |g: &[([u8; 4], usize, usize)]| -> Vec<u8> {
        let mut v = Vec::new();
        for (_, size, d) in g {
            v.extend_from_slice(&f[d - 8..d + size + (size & 1)]);
        }
        v
    };
    let b0 = body_of(first);
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&((4 + b0.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"movi");
    out.extend_from_slice(&b0);
    // idx1 for the first group.
    let mut idx = Vec::new();
    let mut off = 4u32;
    for (id, size, _) in first {
        idx.extend_from_slice(id);
        idx.extend_from_slice(&0x10u32.to_le_bytes());
        idx.extend_from_slice(&off.to_le_bytes());
        idx.extend_from_slice(&(*size as u32).to_le_bytes());
        off += 8 + *size as u32 + (*size as u32 & 1);
    }
    out.extend_from_slice(b"idx1");
    out.extend_from_slice(&(idx.len() as u32).to_le_bytes());
    out.extend_from_slice(&idx);
    let riff0 = (out.len() - 8) as u32;
    put_u32(&mut out, 4, riff0);
    for g in groups {
        let b = body_of(g);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((4 + 12 + b.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"AVIX");
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&((4 + b.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"movi");
        out.extend_from_slice(&b);
    }
    out
}

#[test]
fn opendml_avix_continuations_are_summed() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let odml = make_opendml(&f, 3);
    let data = with_trailer(&odml, 7000, 3);
    let m = measure(&fmt, &data).unwrap();
    assert_eq!(m.len, odml.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.notes.iter().any(|n| n.contains("AVIX")), "{:?}", m.notes);
    // Cut inside the last continuation: the walker reports where.
    let cut = &odml[..odml.len() - 3000];
    let m2 = measure(&fmt, cut).unwrap();
    assert_eq!(m2.status, ExtentStatus::Truncated);
    assert!(m2.len > odml.len() as u64 - 30_000 && m2.len < cut.len() as u64 + 1);
    assert_eq!(m2.err_offset, Some(m2.len));
    // A zeroed RIFF size field falls back to the walk and still finds the end.
    let mut z = odml.clone();
    put_u32(&mut z, 4, 0);
    let m3 = measure(&fmt, &z).unwrap();
    assert_eq!(m3.len, odml.len() as u64);
    // Repair of the truncated continuation keeps the AVIX structure.
    let mut out = Vec::new();
    let r = fmt
        .repair(&mem_slice(cut), &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(r.result, RepairResult::Repaired, "{:?}", r);
    let v = fmt.validate(&mem_slice(&out)).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    assert!(out.windows(4).any(|w| w == b"AVIX"));
}

#[test]
fn opendml_file_is_accepted_by_ffprobe() {
    if !need("ffprobe") {
        return;
    }
    let Some(f) = mpeg4_pcm() else { return };
    let odml = make_opendml(&f, 2);
    let (_d, p) = write_temp("odml.avi", &odml);
    // ffmpeg may or may not follow the continuation without an indx; only
    // check that the structure we built is something it can open.
    assert!(ffprobe(&p).is_some());
}

#[test]
fn rec_lists_and_missing_pad_bytes_are_tolerated() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    // Rebuild movi without pad bytes after odd-sized chunks.
    let (movi_pos, movi_size) = find_top(&f, b"LIST", Some(b"movi")).unwrap();
    let kids = chunks(&f, movi_pos + 12, movi_pos + 8 + movi_size);
    let mut body = Vec::new();
    for (_, size, d) in &kids {
        body.extend_from_slice(&f[d - 8..d + size]); // no pad
    }
    let mut b = f[..movi_pos].to_vec();
    b.extend_from_slice(b"LIST");
    b.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
    b.extend_from_slice(b"movi");
    b.extend_from_slice(&body);
    let total = b.len() as u32 - 8;
    put_u32(&mut b, 4, total);
    let odd = kids.iter().filter(|(_, s, _)| s & 1 == 1).count();
    if odd == 0 {
        eprintln!("fixture has no odd-sized chunks; skipped padding check");
        return;
    }
    let src = mem_slice(&b);
    let v = fmt.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(
        v.problems.iter().any(|p| p.contains("padded")),
        "{:?}",
        v.problems
    );
    let mut out = Vec::new();
    let r = fmt
        .repair(&src, &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(r.result, RepairResult::Repaired, "{:?}", r);
    assert_eq!(out.len(), f.len());
}

#[test]
fn hostile_input_never_panics() {
    let fmt = AviFormat;
    let mut rng = Rng(0xA11);
    // Random bytes behind a valid-looking signature and header.
    for i in 0..200 {
        let n = 12 + (rng.next() % 4000) as usize;
        let mut d = rng.bytes(n);
        d[0..4].copy_from_slice(b"RIFF");
        d[8..12].copy_from_slice(b"AVI ");
        if i % 2 == 0 {
            d[12..16].copy_from_slice(b"LIST");
            d[20..24].copy_from_slice(b"hdrl");
        }
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
        let mut out = Vec::new();
        let _ = fmt.repair(&s, &mut out, &RepairContext::default());
    }
    // Pure noise.
    for _ in 0..50 {
        let n = (rng.next() % 3000) as usize;
        let s = mem(rng.bytes(n));
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    let _ = fmt.measure(&mem(Vec::new()), 0, 100);
    let _ = fmt.measure(&mem_slice(b"RIFF"), 0, 100);
}

#[test]
fn truncated_and_corrupted_real_files_never_panic() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let mut rng = Rng(77);
    // Truncation at many lengths, with measure, validate and repair.
    let mut len = 0usize;
    while len < f.len() {
        let s: SharedSource = mem_slice(&f[..len]);
        let m = fmt.measure(&s, 0, u64::MAX).unwrap();
        if let Some(m) = &m {
            assert!(m.len <= len as u64);
        }
        let _ = fmt.validate(&s);
        if len % 7 == 0 {
            let mut out = Vec::new();
            let _ = fmt.repair(&s, &mut out, &RepairContext::default());
        }
        len += 1 + (rng.next() % 4093) as usize;
    }
    // Absurd sizes in every 4-byte field of the first 12 KiB.
    for off in (4..12_288).step_by(4) {
        for v in [0u32, 0xFFFF_FFFF, 0x8000_0000, 0x7FFF_FFFF] {
            let mut d = f[..30_000].to_vec();
            put_u32(&mut d, off, v);
            let s = mem(d);
            let _ = fmt.measure(&s, 0, u64::MAX);
            let _ = fmt.validate(&s);
        }
    }
    // Absurd chunk sizes inside movi, including with repair.
    let (movi_pos, _) = find_top(&f, b"LIST", Some(b"movi")).unwrap();
    let kids = chunks(&f, movi_pos + 12, movi_pos + 12 + 60_000);
    for (_, _, d) in kids.iter().take(12) {
        for v in [0xFFFF_FFFFu32, 0x7FFF_FFF0, 0x0100_0000] {
            let mut data = f.to_vec();
            put_u32(&mut data, d - 4, v);
            let s = mem(data);
            let _ = fmt.measure(&s, 0, u64::MAX);
            let _ = fmt.validate(&s);
            let mut out = Vec::new();
            let ctx = RepairContext {
                aggressive: true,
                ..RepairContext::default()
            };
            let _ = fmt.repair(&s, &mut out, &ctx);
        }
    }
}

#[test]
fn measure_respects_limit_and_start_offset() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    // Embedded at a non-zero offset behind other data.
    let mut data = Rng(1).bytes(4096);
    data.extend_from_slice(&f);
    data.extend(Rng(2).bytes(4096));
    let m = fmt.measure(&mem(data), 4096, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, f.len() as u64);
    // A limit smaller than the file must not read past it.
    let cap = (f.len() / 2) as u64;
    let src = Arc::new(Counting::new(mem_slice(&f)));
    let shared: SharedSource = src.clone();
    let m = fmt.measure(&shared, 0, cap).unwrap().unwrap();
    assert!(m.len <= cap);
    assert!(matches!(
        m.status,
        ExtentStatus::Capped | ExtentStatus::Truncated
    ));
}

#[test]
fn unreadable_ranges_are_reported() {
    use diskrescue_core::source::MemSource;
    use diskrescue_core::{Range, RangeSet};
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let mut bad = RangeSet::new();
    bad.insert(Range::new(100_000, 104_096));
    let src: SharedSource = Arc::new(MemSource::new(f.to_vec()).with_unreadable(bad));
    let m = fmt.measure(&src, 0, u64::MAX).unwrap().unwrap();
    // The size fields are trusted and the hole is inside a chunk payload.
    assert_eq!(m.len, f.len() as u64);
    assert!(
        m.notes.iter().any(|n| n.contains("unreadable")),
        "{:?}",
        m.notes
    );
}

/// More than 1 GiB so ffmpeg writes OpenDML (`RIFF AVIX`). Slow; run with
/// `cargo test -p diskrescue-video --test avi -- --ignored`.
#[test]
#[ignore]
fn opendml_over_one_gib_from_ffmpeg() {
    if !need("ffmpeg") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.avi");
    let st = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("color=c=gray:size=1920x1080:rate=30")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args([
            "-t",
            "12",
            "-c:v",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&p)
        .status()
        .unwrap();
    assert!(st.success());
    let size = std::fs::metadata(&p).unwrap().len();
    assert!(size > 1 << 30);
    let src: SharedSource = Arc::new(diskrescue_core::source::FileSource::open(&p).unwrap());
    let counting = Arc::new(Counting::new(src));
    let shared: SharedSource = counting.clone();
    let m = AviFormat.measure(&shared, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, size);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.notes.iter().any(|n| n.contains("AVIX")));
    // Header-only reads: far less than the file.
    let read = counting.total.load(std::sync::atomic::Ordering::Relaxed);
    assert!(read < size / 20, "read {read} of {size}");
    let v = AviFormat.validate(&shared).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn keyframe_flags_match_ffmpeg_for_more_codecs() {
    // Codecs whose keyframes come from the bitstream: the rebuilt index must
    // equal the one ffmpeg wrote.
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        (
            "msmpeg4",
            vec!["msmpeg4"],
            vec![
                "-c:v",
                "msmpeg4",
                "-q:v",
                "5",
                "-g",
                "10",
                "-c:a",
                "pcm_s16le",
            ],
        ),
        (
            "xvid",
            vec!["libxvid"],
            vec![
                "-c:v",
                "libxvid",
                "-q:v",
                "5",
                "-g",
                "10",
                "-c:a",
                "pcm_s16le",
            ],
        ),
        (
            "mpeg4 bframes",
            vec!["mpeg4"],
            vec![
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
                "-g",
                "12",
                "-bf",
                "2",
                "-c:a",
                "pcm_s16le",
            ],
        ),
        (
            "dv",
            vec!["dvvideo"],
            vec![
                "-c:v",
                "dvvideo",
                "-s",
                "720x576",
                "-pix_fmt",
                "yuv420p",
                "-r",
                "25",
                "-c:a",
                "pcm_s16le",
            ],
        ),
    ];
    let fmt = AviFormat;
    for (name, encs, extra) in cases {
        if !need_encoders(&encs) {
            continue;
        }
        let mut args: Vec<&str> = vec!["-t", "2"];
        args.extend_from_slice(&extra);
        let f = fixture(&format!("avi_{name}"), || av(&args, "o.avi"));
        let m = measure(&fmt, &f).unwrap_or_else(|| panic!("{name}: not recognised"));
        assert_eq!(m.len, f.len() as u64, "{name}");
        let damaged = strip_index(&f);
        let mut out = Vec::new();
        let r = fmt
            .repair(&mem_slice(&damaged), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::Repaired, "{name}: {:?}", r.notes);
        if out != *f {
            // Show where the index differs.
            let a = out.windows(4).rposition(|w| w == b"idx1").unwrap();
            let b = f.windows(4).rposition(|w| w == b"idx1").unwrap();
            let n = (out.len() - a - 8) / 16;
            let mut diff = Vec::new();
            for i in 0..n {
                let x = &out[a + 8 + i * 16..a + 24 + i * 16];
                let y = &f[b + 8 + i * 16..b + 24 + i * 16];
                if x != y {
                    diff.push((i, x[4..8].to_vec(), y[4..8].to_vec()));
                    if diff.len() > 3 {
                        break;
                    }
                }
            }
            panic!("{name}: rebuilt file differs; first index differences {diff:?}");
        }
    }
}

#[test]
fn crashed_recorder_header_with_nothing_finalised() {
    // RIFF size, movi size, frame counts and stream lengths all zero and no
    // index: what a capture program leaves behind when it dies.
    let fmt = AviFormat;
    for f in [mpeg4_pcm(), mjpeg_pcm(), h264_aac()].into_iter().flatten() {
        let mut d = strip_index(&f);
        let (movi, _) = find_top(&d, b"LIST", Some(b"movi")).unwrap();
        put_u32(&mut d, movi + 4, 0);
        let (hdrl, _) = find_top(&d, b"LIST", Some(b"hdrl")).unwrap();
        let kids = chunks(
            &d,
            hdrl + 12,
            hdrl + 8 + u32::from_le_bytes(d[hdrl + 4..hdrl + 8].try_into().unwrap()) as usize,
        );
        for (id, _, data) in &kids {
            if id == b"avih" {
                put_u32(&mut d, data + 16, 0);
            }
            if id == b"LIST" && &d[*data..*data + 4] == b"strl" {
                // strh is the first child: its dwLength is at +32 of the data.
                put_u32(&mut d, data + 4 + 8 + 32, 0);
            }
        }
        let src = mem_slice(&d);
        let m = fmt.measure(&src, 0, u64::MAX).unwrap().unwrap();
        // The walker finds the frames even though the header says none.
        assert!(
            m.meta.duration_ms.unwrap() >= 1900,
            "{:?}",
            m.meta.duration_ms
        );
        assert_eq!(m.len, d.len() as u64);
        let v = fmt.validate(&src).unwrap();
        assert_eq!(v.status, ValidationStatus::Damaged);
        let mut out = Vec::new();
        let r = fmt
            .repair(&src, &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(r.result, RepairResult::Repaired, "{:?}", r.notes);
        assert_eq!(out, *f, "rebuilt file differs from the original");
    }
}

#[test]
fn rec_lists_are_flattened_by_repair() {
    let fmt = AviFormat;
    let Some(f) = mpeg4_pcm() else { return };
    let (movi_pos, movi_size) = find_top(&f, b"LIST", Some(b"movi")).unwrap();
    let kids = chunks(&f, movi_pos + 12, movi_pos + 8 + movi_size);
    let mut body = Vec::new();
    for group in kids.chunks(3) {
        let mut inner = b"rec ".to_vec();
        for (_, size, d) in group {
            inner.extend_from_slice(&f[d - 8..d + size + (size & 1)]);
        }
        body.extend_from_slice(b"LIST");
        body.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        body.extend_from_slice(&inner);
    }
    let mut b = f[..movi_pos].to_vec();
    b.extend_from_slice(b"LIST");
    b.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
    b.extend_from_slice(b"movi");
    b.extend_from_slice(&body);
    let total = b.len() as u32 - 8;
    put_u32(&mut b, 4, total);
    let src = mem_slice(&b);
    // Complete by the size fields.
    let m = fmt.measure(&src, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m.len, b.len() as u64);
    assert_eq!(m.status, ExtentStatus::Complete);
    // No idx1: the file is damaged by the avih index flag and repair
    // flattens the groups into a plain movi with a fresh index.
    let v = fmt.validate(&src).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    let mut out = Vec::new();
    let r = fmt
        .repair(&src, &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(r.result, RepairResult::Repaired, "{:?}", r);
    assert!(r.applied.iter().any(|a| a.contains("rec")));
    assert_eq!(out, *f);
}

#[test]
fn every_codec_repairs_after_truncation() {
    if !need("ffprobe") {
        return;
    }
    let fmt = AviFormat;
    for f in [mpeg4_pcm(), mjpeg_pcm(), mpeg4_mp3(), h264_aac()]
        .into_iter()
        .flatten()
    {
        let cut = &f[..f.len() * 6 / 10];
        for strategy in [AviStrategy::RebuildIndex, AviStrategy::TruncateToLastChunk] {
            let mut out = Vec::new();
            let r = fmt
                .repair_with(
                    &mem_slice(cut),
                    &mut out,
                    strategy,
                    &RepairContext::default(),
                )
                .unwrap();
            assert_eq!(
                r.result,
                RepairResult::Repaired,
                "{strategy:?}: {:?}",
                r.notes
            );
            let (_d, p) = write_temp("fixed.avi", &out);
            assert!(ffprobe(&p).is_some());
            decodes_cleanly(&p).unwrap_or_else(|e| panic!("{strategy:?}: {e}"));
        }
    }
}
