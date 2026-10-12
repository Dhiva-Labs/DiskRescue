//! Sample tables against ffprobe, and the H.264 / HEVC / AAC parsers
//! against real encoder output.

mod common;

use common::*;
use diskrescue_core::{Source, SourceExt};
use diskrescue_isobmff::boxes::{self, parse_moov, sample_table, Moov};
use diskrescue_isobmff::{aac, h264, hevc};

fn load_moov(path: &std::path::Path) -> (diskrescue_core::SharedSource, Moov) {
    let src = open(path);
    let (tops, _) =
        boxes::walk_top_level(&*src, 0, src.size(), &|k| boxes::is_top_level_type(k)).unwrap();
    let mb = tops.iter().find(|t| t.header.is(b"moov")).unwrap();
    let m = parse_moov(&*src, &mb.header).unwrap();
    (src, m)
}

#[test]
fn sample_tables_match_ffprobe() {
    if !ffmpeg_or_skip() {
        return;
    }
    for name in [
        "h264",
        "h264_faststart",
        "h265",
        "mov",
        "mov_pcm",
        "3gp",
        "m4a",
    ] {
        let path = fixture(name);
        let (_, m) = load_moov(&path);
        for (sel, trak) in [("v", m.video()), ("a", m.audio())] {
            let Some(trak) = trak else { continue };
            let ours = sample_table(trak).unwrap();
            let theirs = packets(&path, sel);
            if trak.entry().is_some_and(|e| boxes::is_pcm_kind(&e.kind)) {
                // ffprobe groups PCM samples into packets per chunk; compare
                // total bytes and the first offset instead.
                let total: u64 = ours.iter().map(|s| s.size as u64).sum();
                let ptotal: u64 = theirs.iter().map(|p| p.size).sum();
                assert_eq!(total, ptotal, "{name}");
                assert_eq!(ours[0].offset, theirs[0].pos, "{name}");
                continue;
            }
            assert_eq!(ours.len(), theirs.len(), "{name} {sel}");
            for (o, t) in ours.iter().zip(&theirs) {
                assert_eq!((o.offset, o.size as u64), (t.pos, t.size), "{name} {sel}");
                assert_eq!(o.keyframe, t.key, "{name} {sel} sample {}", o.index);
            }
        }
    }
}

/// Split a length-prefixed sample into NAL units.
fn nals(sample: &[u8], l: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + l <= sample.len() {
        let n = sample[p..p + l]
            .iter()
            .fold(0usize, |a, &b| (a << 8) | b as usize);
        p += l;
        if n == 0 || p + n > sample.len() {
            break;
        }
        out.push(sample[p..p + n].to_vec());
        p += n;
    }
    out
}

/// Split an Annex B stream into NAL units.
fn annexb(b: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map(|&n| n - 3).unwrap_or(b.len());
        while e > s && b[e - 1] == 0 {
            e -= 1;
        }
        out.push(b[s..e].to_vec());
    }
    out
}

/// Sorting by (epoch, POC) gives the same order as sorting by cts.
fn assert_same_order(pocs: &[(u32, i32)], cts: &[i64]) {
    let mut a: Vec<usize> = (0..cts.len()).collect();
    a.sort_by_key(|&i| (cts[i], i));
    let mut b: Vec<usize> = (0..pocs.len()).collect();
    b.sort_by_key(|&i| (pocs[i], i));
    assert_eq!(a, b);
}

#[test]
fn h264_parameter_sets_slices_and_poc() {
    if !ffmpeg_or_skip() {
        return;
    }
    let (src, m) = load_moov(&fixture("h264"));
    let v = m.video().unwrap();
    let avcc = v.entry().unwrap().avcc.clone().unwrap();
    let mut sps = h264::SpsMap::new();
    let mut pps = h264::PpsMap::new();
    for n in &avcc.sps {
        let s = h264::parse_sps(n).unwrap();
        assert_eq!((s.width, s.height), (320, 240));
        assert_eq!(s.pic_order_cnt_type, 0);
        sps.insert(s.sps_id, s);
    }
    for n in &avcc.pps {
        let p = h264::parse_pps(n).unwrap();
        pps.insert(p.pps_id, p);
    }
    // In-band parameter sets of an Annex B stream with the same settings.
    let es = std::fs::read(fixture("h264_annexb")).unwrap();
    let units = annexb(&es);
    let in_sps = units.iter().find(|n| h264::nal_type(n[0]) == 7).unwrap();
    let in_pps = units.iter().find(|n| h264::nal_type(n[0]) == 8).unwrap();
    assert_eq!(h264::parse_sps(in_sps).unwrap(), sps[&0]);
    assert_eq!(h264::parse_pps(in_pps).unwrap(), pps[&0]);
    let in_slices: Vec<_> = units
        .iter()
        .filter(|n| h264::has_slice_header(h264::nal_type(n[0])))
        .take(10)
        .map(|n| h264::parse_slice_header(n, &sps, &pps).unwrap())
        .collect();
    assert!(in_slices[0].is_idr());

    let samples = sample_table(v).unwrap();
    let mut st = h264::PocState::new();
    let mut pocs = Vec::new();
    let mut cts = Vec::new();
    let mut epoch = 0u32;
    let mut prev_ref_fn: Option<u32> = None;
    for (i, s) in samples.iter().enumerate() {
        let data = src.read_exact_vec(s.offset, s.size as usize).unwrap();
        let first = nals(&data, avcc.nal_length_size as usize)
            .into_iter()
            .find(|n| h264::has_slice_header(h264::nal_type(n[0])))
            .unwrap();
        let h = h264::parse_slice_header(&first, &sps, &pps).unwrap();
        assert_eq!(h.first_mb_in_slice, 0);
        assert_eq!(h.is_idr(), s.keyframe, "sample {i}");
        if i < 10 {
            if let Some(p) = prev_ref_fn {
                let max = sps[&0].max_frame_num();
                assert!(h.is_idr() || h.frame_num == p || h.frame_num == (p + 1) % max);
            }
        }
        if h.nal_ref_idc != 0 {
            prev_ref_fn = Some(h.frame_num);
        }
        if h.is_idr() && i > 0 {
            epoch += 1;
        }
        pocs.push((epoch, h264::poc(&sps[&0], &h, &mut st)));
        cts.push(s.dts as i64 + s.cts_offset as i64);
    }
    assert_same_order(&pocs, &cts);
}

#[test]
fn hevc_parameter_sets_slices_and_poc() {
    if !ffmpeg_or_skip() {
        return;
    }
    let (src, m) = load_moov(&fixture("h265"));
    let v = m.video().unwrap();
    let hvcc = v.entry().unwrap().hvcc.clone().unwrap();
    let mut sps = hevc::SpsMap::new();
    let mut pps = hevc::PpsMap::new();
    assert!(hevc::parse_vps(&hvcc.vps[0]).is_some());
    for n in &hvcc.sps {
        let s = hevc::parse_sps(n).unwrap();
        assert_eq!((s.width, s.height), (320, 240));
        sps.insert(s.sps_id, s);
    }
    for n in &hvcc.pps {
        let p = hevc::parse_pps(n).unwrap();
        pps.insert(p.pps_id, p);
    }
    let es = std::fs::read(fixture("h265_annexb")).unwrap();
    let units = annexb(&es);
    let in_sps = units.iter().find(|n| hevc::nal_type(n[0]) == 33).unwrap();
    let in_pps = units.iter().find(|n| hevc::nal_type(n[0]) == 34).unwrap();
    assert_eq!(hevc::parse_sps(in_sps).unwrap(), sps[&0]);
    assert_eq!(hevc::parse_pps(in_pps).unwrap(), pps[&0]);

    let samples = sample_table(v).unwrap();
    let mut st = hevc::PocState::new();
    let mut pocs = Vec::new();
    let mut cts = Vec::new();
    let mut epoch = 0u32;
    for (i, s) in samples.iter().enumerate() {
        let data = src.read_exact_vec(s.offset, s.size as usize).unwrap();
        let first = nals(&data, hvcc.nal_length_size as usize)
            .into_iter()
            .find(|n| hevc::is_slice_type(hevc::nal_type(n[0])))
            .unwrap();
        let h = hevc::parse_slice_header(&first, &sps, &pps).unwrap();
        assert!(h.first_slice_segment_in_pic);
        assert_eq!(hevc::is_keyframe_type(h.nal_type), s.keyframe, "sample {i}");
        if hevc::is_irap(h.nal_type) && hevc::no_rasl_output(h.nal_type, &st) && i > 0 {
            epoch += 1;
        }
        pocs.push((epoch, hevc::poc(&sps[&0], &h, &mut st)));
        cts.push(s.dts as i64 + s.cts_offset as i64);
    }
    assert_same_order(&pocs, &cts);
}

#[test]
fn aac_frames_walk_to_their_table_sizes() {
    if !ffmpeg_or_skip() {
        return;
    }
    for name in ["h264", "h265", "m4a", "3gp"] {
        let (src, m) = load_moov(&fixture(name));
        let a = m.audio().unwrap();
        let es = a.entry().unwrap().esds.clone().unwrap();
        let cfg = aac::parse_audio_specific_config(&es.audio_specific_config).unwrap();
        for s in sample_table(a).unwrap() {
            let mut data = src.read_exact_vec(s.offset, s.size as usize).unwrap();
            assert_eq!(
                aac::frame_length(&data, &cfg),
                Some(s.size as usize),
                "{name}"
            );
            assert_eq!(aac::looks_like_raw_frame(&data, &cfg), aac::Score::High);
            data.push(0);
            assert_eq!(aac::looks_like_raw_frame(&data, &cfg), aac::Score::Low);
        }
    }
}
