//! Hostile input: random bytes, truncations, absurd sizes and nesting
//! bombs must never panic and must finish quickly.

mod common;

use std::time::{Duration, Instant};

use common::*;
use diskrescue_core::format::{FileFormat, RepairContext};
use diskrescue_core::Source;
use diskrescue_isobmff::boxes::{self, parse_moov_bytes, sample_table, Trak};
use diskrescue_isobmff::rebuild::{learn, rebuild, RebuildOptions};
use diskrescue_isobmff::{aac, analysis, formats, h264, hevc, tiff};

fn all_entry_points(bytes: &[u8]) {
    let src = mem(bytes.to_vec());
    let n = bytes.len() as u64;
    for f in formats() {
        let _ = f.probe(bytes);
        let _ = f.measure(&src, 0, n);
        if n > 3 {
            let _ = f.measure(&src, 3, n - 3);
        }
        let _ = f.measure(&src, 0, u64::MAX);
        let _ = f.validate(&src);
        let mut sink = Vec::new();
        let _ = f.repair(&src, &mut sink, &RepairContext::default());
    }
    let _ = analysis::analyze(&*src);
    let _ = learn(&src);
    if let Ok(m) = parse_moov_bytes(bytes, 0) {
        for t in &m.tracks {
            let _ = sample_table(t);
        }
    }
    let _ = h264::parse_sps(bytes);
    let _ = h264::parse_pps(bytes);
    let _ = hevc::parse_sps(bytes);
    let _ = hevc::parse_pps(bytes);
    let _ = hevc::parse_vps(bytes);
    let _ = tiff::parse_tiff(bytes);
    let _ = aac::parse_audio_specific_config(bytes);
    let _ = boxes::parse_avcc(bytes);
    let _ = boxes::parse_hvcc(bytes);
    let _ = boxes::parse_esds(bytes);
}

#[test]
fn random_bytes() {
    let t = Instant::now();
    for seed in 0..300u64 {
        let len = (seed as usize * 37) % 4000;
        let mut b = noise(len, seed + 1);
        // Half of the inputs get a plausible ftyp head to go deeper.
        if seed % 2 == 0 && b.len() >= 24 {
            let brands: [&[u8; 4]; 4] = [b"isom", b"qt  ", b"heic", b"crx "];
            b[..8].copy_from_slice(b"\0\0\0\x18ftyp");
            b[8..12].copy_from_slice(brands[(seed / 2 % 4) as usize]);
        }
        all_entry_points(&b);
    }
    assert!(t.elapsed() < Duration::from_secs(60));
}

#[test]
fn truncations_of_real_files() {
    if !ffmpeg_or_skip() {
        return;
    }
    let t = Instant::now();
    for name in ["h264", "h264_faststart", "h265", "mov_pcm"] {
        let b = std::fs::read(fixture(name)).unwrap();
        let step = (b.len() / 23).max(1);
        let mut cut = 0usize;
        while cut < b.len() {
            all_entry_points(&b[..cut]);
            cut += step;
        }
        // Bit flips in the moov.
        let moov = top_boxes(&b).into_iter().find(|x| &x.0 == b"moov").unwrap();
        for k in 0..40usize {
            let mut c = b.clone();
            let at = moov.1 + (k * 7919) % moov.2;
            c[at] ^= 0x5A;
            let src = mem(c);
            let _ = diskrescue_isobmff::Mp4Format.validate(&src);
            let _ = diskrescue_isobmff::Mp4Format.measure(&src, 0, src.size());
        }
    }
    assert!(t.elapsed() < Duration::from_secs(120));
}

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

#[test]
fn absurd_sizes() {
    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2");
    // mdat claiming 2^60 bytes via largesize.
    let mut b = ftyp.clone();
    b.extend_from_slice(&1u32.to_be_bytes());
    b.extend_from_slice(b"mdat");
    b.extend_from_slice(&(1u64 << 60).to_be_bytes());
    b.extend(noise(1000, 1));
    all_entry_points(&b);
    // moov claiming 4 GiB.
    let mut b = ftyp.clone();
    b.extend_from_slice(&u32::MAX.to_be_bytes());
    b.extend_from_slice(b"moov");
    b.extend(noise(1000, 2));
    all_entry_points(&b);
    // Tables with huge entry counts.
    let mut stbl = Vec::new();
    for k in [
        b"stts", b"stsc", b"stsz", b"stco", b"co64", b"stss", b"ctts",
    ] {
        let mut p = vec![0u8; 4];
        p.extend_from_slice(&u32::MAX.to_be_bytes());
        p.extend_from_slice(&u32::MAX.to_be_bytes());
        stbl.extend(bx(k, &p));
    }
    let trak = bx(
        b"trak",
        &[
            bx(b"tkhd", &[0u8; 84]),
            bx(
                b"mdia",
                &[bx(b"mdhd", &[0u8; 24]), bx(b"minf", &bx(b"stbl", &stbl))].concat(),
            ),
        ]
        .concat(),
    );
    let mut mvhd = vec![0u8; 100];
    mvhd[15] = 1; // timescale 1
    let moov = bx(b"moov", &[bx(b"mvhd", &mvhd), trak].concat());
    let mut b = ftyp.clone();
    b.extend(moov.clone());
    b.extend(bx(b"mdat", &noise(500, 3)));
    let t = Instant::now();
    all_entry_points(&b);
    assert!(t.elapsed() < Duration::from_secs(10));
    let m = parse_moov_bytes(&moov[8..], 8).unwrap();
    let _ = sample_table(&m.tracks[0]);
    // A table claiming more samples than allowed is refused.
    let t = Trak {
        stsz: boxes::SampleSizes::Constant {
            size: 1,
            count: u32::MAX,
        },
        ..Default::default()
    };
    assert!(sample_table(&t).is_err());
}

#[test]
fn nesting_bombs() {
    let mut b = bx(b"free", &[]);
    for i in 0..5000 {
        let k: &[u8; 4] = match i % 4 {
            0 => b"trak",
            1 => b"mdia",
            2 => b"minf",
            _ => b"udta",
        };
        b = bx(k, &b);
    }
    let mut file = bx(b"ftyp", b"isom\0\0\x02\0isom");
    file.extend(bx(b"moov", &[bx(b"mvhd", &[0u8; 100]), b.clone()].concat()));
    file.extend(bx(b"mdat", &[0u8; 64]));
    let t = Instant::now();
    all_entry_points(&file);
    let src = mem(b.clone());
    let root = boxes::read_header(&*src, 0, src.size()).unwrap().unwrap();
    let mut n = 0;
    boxes::walk_tree(&*src, &root, &mut |_, _| {
        n += 1;
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert!(n <= boxes::MAX_DEPTH + 1);
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[test]
fn rebuild_on_garbage_terminates() {
    if !ffmpeg_or_skip() {
        return;
    }
    let r = learn(&open(&fixture("h264_ref"))).unwrap();
    let t = Instant::now();
    for (i, len) in [0usize, 10, 4096, 3 << 20].into_iter().enumerate() {
        let src = mem(noise(len, i as u64 + 100));
        let mut sink = Vec::new();
        let opts = RebuildOptions {
            stop_at_first_unclassified_mb: 1,
            ..Default::default()
        };
        let _ = rebuild(&src, &r, &mut sink, &opts);
    }
    // All zeros (an unwritten card area).
    let src = mem(vec![0u8; 2 << 20]);
    let mut sink = Vec::new();
    assert!(rebuild(&src, &r, &mut sink, &RebuildOptions::default()).is_err());
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
}
