//! Hostile input: random bytes, random bytes behind valid signatures,
//! truncations, mutated real files, absurd sizes and unreadable heads must
//! never panic, never read outside the window and never return a result that
//! contradicts itself.

mod common;

use common::raws;
use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, Measured, ValidationStatus};
use diskrescue_core::source::{MemSource, SharedSource};
use diskrescue_core::{Range, RangeSet};
use diskrescue_stills::tiff::{self, Magic};
use diskrescue_stills::{
    formats, BmpFormat, GifFormat, PsdFormat, RafFormat, TiffFormat, WebpFormat,
};
use std::sync::Arc;

fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = raws::all()
        .into_iter()
        .map(|s| (s.name.to_string(), s.bytes))
        .collect();
    if let Some(dir) = pillow_dir() {
        for name in [
            "gif_static.gif",
            "gif_anim.gif",
            "bmp_rgb24.bmp",
            "bmp_pal8.bmp",
            "webp_lossy.webp",
            "webp_exif.webp",
            "webp_anim.webp",
            "tiff_strips.tif",
            "tiff_pages.tif",
            "tiff_meta.tif",
        ] {
            v.push((name.to_string(), std::fs::read(dir.join(name)).unwrap()));
        }
    }
    v
}

/// The contract every result must satisfy.
fn invariants(
    fmt: &dyn FileFormat,
    m: &Measured,
    src_len: usize,
    start: u64,
    limit: u64,
    ctx: &str,
) {
    let avail = (src_len as u64).saturating_sub(start).min(limit);
    assert!(
        m.len <= avail,
        "{}: len {} > avail {avail} ({ctx})",
        fmt.id(),
        m.len
    );
    assert!(m.len <= fmt.max_size(), "{} ({ctx})", fmt.id());
    assert!(m.confidence.value() <= 100);
    match m.status {
        ExtentStatus::Complete => assert!(
            m.err_offset.is_none(),
            "{} complete with err ({ctx})",
            fmt.id()
        ),
        ExtentStatus::Truncated => {
            let e = m
                .err_offset
                .unwrap_or_else(|| panic!("{} truncated without err_offset ({ctx})", fmt.id()));
            assert!(
                e <= avail,
                "{}: err_offset {e} > avail {avail} ({ctx})",
                fmt.id()
            );
        }
        ExtentStatus::Capped => {
            assert!(m.len <= limit, "{}: capped beyond limit ({ctx})", fmt.id());
        }
        ExtentStatus::UnknownLength => {}
    }
    for n in &m.nested {
        assert!(
            n.range.start < n.range.end && n.range.end <= m.len,
            "{}: nested {:?} outside len {} ({ctx})",
            fmt.id(),
            n,
            m.len
        );
    }
}

fn run(fmt: &dyn FileFormat, bytes: &[u8], start: u64, limit: u64, ctx: &str) {
    let src = mem(bytes.to_vec());
    let head_from = (start as usize).min(bytes.len());
    let _ = fmt.probe(&bytes[head_from..bytes.len().min(head_from + 65_536)]);
    match fmt.measure(&src, start, limit) {
        Ok(Some(m)) => invariants(fmt, &m, bytes.len(), start, limit, ctx),
        Ok(None) => {}
        Err(e) => panic!("{}: unexpected error {e} ({ctx})", fmt.id()),
    }
}

fn run_validate(fmt: &dyn FileFormat, bytes: &[u8]) {
    let v = fmt.validate(&mem(bytes.to_vec())).unwrap();
    if v.status == ValidationStatus::Valid {
        let m = fmt
            .measure(&mem(bytes.to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                m.status,
                ExtentStatus::Complete | ExtentStatus::UnknownLength
            ),
            "{}: valid but measured {:?}",
            fmt.id(),
            m.status
        );
        assert!(m.len <= bytes.len() as u64);
    }
    if let Some(f) = v.intact_fraction {
        assert!((0.0..=1.0).contains(&f), "{}: fraction {f}", fmt.id());
    }
}

#[test]
fn random_bytes_with_and_without_valid_signatures() {
    let mut rng = Rng(0xFEED);
    for fmt in formats() {
        for round in 0..300 {
            let size = [
                0usize, 1, 2, 3, 4, 7, 8, 12, 16, 20, 27, 40, 100, 160, 500, 3000, 70_000,
            ][round % 17];
            let mut buf = rng.bytes(size);
            // Plant a valid signature of this format at its offset.
            if round % 2 == 0 {
                let sigs = fmt.signatures();
                let s = &sigs[round / 2 % sigs.len()];
                let need = s.probe_len() as usize;
                if buf.len() < need {
                    buf.resize(need, 0);
                }
                buf[s.offset as usize..s.offset as usize + s.bytes.len()].copy_from_slice(s.bytes);
                if let Some((b, o)) = s.also {
                    buf[o as usize..o as usize + b.len()].copy_from_slice(b);
                }
                // Make the next few words small and plausible.
                if buf.len() > 40 && round % 4 == 0 {
                    for i in (8..40).step_by(4) {
                        buf[i] = (rng.below(3)) as u8;
                    }
                }
            }
            run(fmt.as_ref(), &buf, 0, u64::MAX, "random");
            run_validate(fmt.as_ref(), &buf);
        }
    }
}

#[test]
fn zero_and_ff_buffers_are_rejected_quickly() {
    for fmt in formats() {
        for n in [0usize, 1, 100, 4096, 70_000] {
            for fill in [0u8, 0xFF] {
                let b = vec![fill; n];
                let src = mem(b.clone());
                assert!(
                    fmt.measure(&src, 0, u64::MAX).unwrap().is_none(),
                    "{} {n} {fill:#x}",
                    fmt.id()
                );
                assert!(!fmt.probe(&b));
                assert_eq!(
                    fmt.validate(&src).unwrap().status,
                    ValidationStatus::Invalid
                );
            }
        }
    }
}

#[test]
fn mutated_real_files_never_break_the_contract() {
    let mut rng = Rng(0xBEEF);
    let all = formats();
    for (name, orig) in corpus() {
        for (fi, fmt) in all.iter().enumerate() {
            // Own format gets many mutations; the others a few (they mostly
            // reject, but a mutated header might make them accept).
            let own = {
                let m = fmt.measure(&mem(orig.clone()), 0, u64::MAX).unwrap();
                m.is_some()
            };
            let rounds = if own { 250 } else { 6 };
            for r in 0..rounds {
                let mut b = orig.clone();
                let k = 1 + rng.below(6) as usize;
                for _ in 0..k {
                    let limit = if rng.below(4) < 3 {
                        b.len().min(300)
                    } else {
                        b.len()
                    };
                    let pos = rng.below(limit as u64) as usize;
                    b[pos] = match rng.below(4) {
                        0 => 0,
                        1 => 0xFF,
                        2 => b[pos] ^ (1 << rng.below(8)),
                        _ => rng.next() as u8,
                    };
                }
                if r % 5 == 0 {
                    let cut = rng.below(b.len() as u64) as usize;
                    b.truncate(cut);
                }
                let ctx = format!("{name} fmt#{fi} round {r}");
                run(fmt.as_ref(), &b, 0, u64::MAX, &ctx);
                if r % 10 == 0 {
                    run_validate(fmt.as_ref(), &b);
                }
            }
        }
    }
}

#[test]
fn every_prefix_of_small_files_and_sampled_prefixes_of_large_ones() {
    let all = formats();
    for (name, orig) in corpus() {
        let cuts: Vec<usize> = if orig.len() <= 2_500 {
            (0..=orig.len()).collect()
        } else {
            let mut c: Vec<usize> = (0..400).collect();
            c.extend((400..orig.len()).step_by(orig.len() / 60));
            c
        };
        for fmt in &all {
            for &cut in &cuts {
                run(
                    fmt.as_ref(),
                    &orig[..cut],
                    0,
                    u64::MAX,
                    &format!("{name} cut {cut}"),
                );
            }
        }
    }
}

#[test]
fn limits_and_start_offsets() {
    let mut rng = Rng(77);
    let all = formats();
    for (name, orig) in corpus() {
        let own: Vec<_> = all
            .iter()
            .filter(|f| {
                f.measure(&mem(orig.clone()), 0, u64::MAX)
                    .unwrap()
                    .is_some()
            })
            .collect();
        assert!(!own.is_empty(), "{name} recognised by nothing");
        for fmt in own {
            let len = orig.len() as u64;
            for &limit in &[
                0u64,
                1,
                2,
                3,
                7,
                8,
                13,
                26,
                64,
                100,
                1000,
                len - 1,
                len,
                len + 1,
                u64::MAX,
            ] {
                for &(before, after) in &[(0usize, 0usize), (0, 5_000), (3, 3), (4096, 100)] {
                    let mut b = rng.bytes(before);
                    b.extend_from_slice(&orig);
                    b.extend(rng.bytes(after));
                    run(
                        fmt.as_ref(),
                        &b,
                        before as u64,
                        limit,
                        &format!("{name} limit {limit}"),
                    );
                }
            }
            // Start beyond the source, and exactly at its end.
            run(fmt.as_ref(), &orig, len, u64::MAX, "start at end");
            run(fmt.as_ref(), &orig, len + 10, u64::MAX, "start past end");
        }
    }
}

#[test]
fn unreadable_head_is_an_error_not_a_panic() {
    for (name, orig) in corpus() {
        for fmt in formats() {
            if fmt
                .measure(&mem(orig.clone()), 0, u64::MAX)
                .unwrap()
                .is_none()
            {
                continue;
            }
            let mut set = RangeSet::new();
            set.insert(Range::new(0, 16));
            let src: SharedSource = Arc::new(MemSource::new(orig.clone()).with_unreadable(set));
            assert!(
                fmt.measure(&src, 0, u64::MAX).is_err(),
                "{} on {name}",
                fmt.id()
            );
        }
    }
}

#[test]
fn a_hole_anywhere_never_panics_and_is_reported_consistently() {
    let mut rng = Rng(31337);
    let all = formats();
    for (name, orig) in corpus() {
        for fmt in &all {
            if fmt
                .measure(&mem(orig.clone()), 0, u64::MAX)
                .unwrap()
                .is_none()
            {
                continue;
            }
            for _ in 0..20 {
                let at = 16 + rng.below(orig.len() as u64 - 16);
                let len = 1 + rng.below(2_000);
                let mut set = RangeSet::new();
                set.insert(Range::new(at, at + len));
                let src: SharedSource = Arc::new(MemSource::new(orig.clone()).with_unreadable(set));
                match fmt.measure(&src, 0, u64::MAX) {
                    Ok(Some(m)) => invariants(
                        fmt.as_ref(),
                        &m,
                        orig.len(),
                        0,
                        u64::MAX,
                        &format!("{name} hole {at}+{len}"),
                    ),
                    Ok(None) => {}
                    // A hole in the header or IFD0 is reported as a read error.
                    Err(e) => assert!(e.is_read_error(), "{} {name}: {e}", fmt.id()),
                }
                let _ = fmt.validate(&src);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Absurd sizes and counts
// ---------------------------------------------------------------------------

#[test]
fn tiff_with_maximal_counts_and_offsets() {
    // IFD0 with 4096 entries, every count and value 0xFFFFFFFF.
    for be in [false, true] {
        let mut b = tiff_header(be, 8);
        let n = 4096u16;
        b.extend_from_slice(&if be { n.to_be_bytes() } else { n.to_le_bytes() });
        for i in 0..n {
            // Duplicated tags on purpose.
            let tag = [
                0x0100u16, 0x0101, 0x0111, 0x0117, 0x0144, 0x0145, 0x014A, 0x8769, 0x0201, 0x0202,
            ][(i % 10) as usize];
            b.extend_from_slice(&if be {
                tag.to_be_bytes()
            } else {
                tag.to_le_bytes()
            });
            b.extend_from_slice(&if be {
                4u16.to_be_bytes()
            } else {
                4u16.to_le_bytes()
            });
            b.extend_from_slice(&[0xFF; 8]);
        }
        b.extend_from_slice(&[0; 4]);
        for fmt in formats() {
            run(fmt.as_ref(), &b, 0, u64::MAX, "max counts");
        }
        let w = tiff::walk(&mem(b), 0, u64::MAX, Magic::Classic).unwrap();
        if let Some(w) = w {
            assert!(w.ifd_count() <= 64);
        }
    }
    // 64 IFDs of 4096 entries each in a chain.
    let mut b = tiff_header(false, 8);
    let ifd_len = 2 + 12 * 4096 + 4;
    for k in 0..70u32 {
        b.extend_from_slice(&4096u16.to_le_bytes());
        for i in 0..4096u32 {
            let tag = 0x0100 + (i % 8) as u16;
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&3u16.to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes());
            b.extend_from_slice(&[1, 0, 0, 0]);
        }
        let next = 8 + (k + 1) * ifd_len as u32;
        b.extend_from_slice(&next.to_le_bytes());
    }
    let w = tiff::walk(&mem(b.clone()), 0, u64::MAX, Magic::Classic)
        .unwrap()
        .unwrap();
    assert!(w.ifds.len() <= 16);
    run(&TiffFormat, &b, 0, u64::MAX, "long chain");
}

#[test]
fn bigtiff_with_maximal_counts() {
    let mut b = b"II+\0\x08\0\0\0".to_vec();
    b.extend_from_slice(&16u64.to_le_bytes());
    b.extend_from_slice(&3u64.to_le_bytes());
    for tag in [0x0100u16, 0x0111, 0x0117] {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(&u64::MAX.to_le_bytes());
        b.extend_from_slice(&(u64::MAX - 7).to_le_bytes());
    }
    b.extend_from_slice(&0u64.to_le_bytes());
    run(&TiffFormat, &b, 0, u64::MAX, "bigtiff max");
    let _ = tiff::walk(&mem(b), 0, u64::MAX, Magic::BigTiff).unwrap();
    // IFD0 offset near u64::MAX.
    let mut c = b"II+\0\x08\0\0\0".to_vec();
    c.extend_from_slice(&(u64::MAX - 3).to_le_bytes());
    run(&TiffFormat, &c, 0, u64::MAX, "bigtiff offset");
}

#[test]
fn bmp_dimensions_that_overflow_every_size_calculation() {
    let mut b = vec![0u8; 200];
    b[..2].copy_from_slice(b"BM");
    b[10..14].copy_from_slice(&54u32.to_le_bytes());
    b[14..18].copy_from_slice(&40u32.to_le_bytes());
    b[18..22].copy_from_slice(&65535i32.to_le_bytes());
    b[22..26].copy_from_slice(&(-65535i32).to_le_bytes());
    b[26..28].copy_from_slice(&1u16.to_le_bytes());
    b[28..30].copy_from_slice(&32u16.to_le_bytes());
    // 65535 x 65535 x 32 bit = 16 GiB: beyond the format's 1 GiB cap.
    assert!(BmpFormat
        .measure(&mem(b.clone()), 0, u64::MAX)
        .unwrap()
        .is_none());
    // Huge biSizeImage with RLE.
    b[18..22].copy_from_slice(&10i32.to_le_bytes());
    b[22..26].copy_from_slice(&10i32.to_le_bytes());
    b[28..30].copy_from_slice(&8u16.to_le_bytes());
    b[30..34].copy_from_slice(&1u32.to_le_bytes());
    b[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(BmpFormat.measure(&mem(b), 0, u64::MAX).unwrap().is_none());
}

#[test]
fn webp_with_a_huge_riff_size_and_a_million_chunks() {
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&u32::MAX.to_le_bytes());
    b.extend_from_slice(b"WEBPVP8 ");
    b.extend_from_slice(&10u32.to_le_bytes());
    b.extend_from_slice(&[0x10, 0x02, 0x00, 0x9D, 0x01, 0x2A, 8, 0, 8, 0]);
    for _ in 0..200_000 {
        b.extend_from_slice(b"JUNK\x00\x00\x00\x00");
    }
    run(&WebpFormat, &b, 0, u64::MAX, "many chunks");
    let m = WebpFormat
        .measure(&mem(b.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.err_offset, Some(b.len() as u64));
}

#[test]
fn gif_with_millions_of_tiny_sub_blocks() {
    let mut g = b"GIF89a".to_vec();
    g.extend_from_slice(&[4, 0, 4, 0, 0, 0, 0]);
    g.extend_from_slice(&[0x2C, 0, 0, 0, 0, 4, 0, 4, 0, 0, 2]);
    for _ in 0..1_000_000 {
        g.extend_from_slice(&[1, 0x55]);
    }
    g.extend_from_slice(&[0, 0x3B]);
    let m = GifFormat
        .measure(&mem(g.clone()), 0, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(m.len, g.len() as u64);
    // validate decodes LZW over all those blocks without issue (and flags
    // the frame as damaged because the data is not a valid code stream).
    let v = GifFormat.validate(&mem(g)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn psd_with_extreme_header_values() {
    let mut p = raws::psd(0, false).bytes;
    // channels 56, 30000 x 30000, depth 32: raw size ~ 200 GB.
    p[12..14].copy_from_slice(&56u16.to_be_bytes());
    p[14..18].copy_from_slice(&30_000u32.to_be_bytes());
    p[18..22].copy_from_slice(&30_000u32.to_be_bytes());
    p[22..24].copy_from_slice(&32u16.to_be_bytes());
    run(&PsdFormat, &p, 0, u64::MAX, "psd huge raw");
    p[40..42].copy_from_slice(&1u16.to_be_bytes()); // RLE: table of 1.68 M rows
    run(&PsdFormat, &p, 0, u64::MAX, "psd huge rle");
    p[40..42].copy_from_slice(&2u16.to_be_bytes()); // ZIP over junk
    run(&PsdFormat, &p, 0, u64::MAX, "psd huge zip");
}

#[test]
fn raf_with_extreme_offsets() {
    let mut f = raws::raf(true).bytes;
    for (a, b) in [(84usize, 88usize), (92, 96), (100, 104)] {
        let mut g = f.clone();
        g[a..a + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        g[b..b + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        run(&RafFormat, &g, 0, u64::MAX, "raf max");
    }
    f[120..124].copy_from_slice(&u32::MAX.to_be_bytes());
    f[124..128].copy_from_slice(&u32::MAX.to_be_bytes());
    f[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
    f[132..136].copy_from_slice(&u32::MAX.to_be_bytes());
    run(&RafFormat, &f, 0, u64::MAX, "raf second max");
}

/// Overwrite 2..8 byte windows with the values that break size arithmetic
/// (all ones, zero, sign bits, one) at structure-heavy offsets.
#[test]
fn length_field_stamping() {
    let stamps: [&[u8]; 8] = [
        &[0xFF; 4],
        &[0; 4],
        &[0xFF, 0xFF, 0xFF, 0x7F],
        &[0, 0, 0, 0x80],
        &[0xFF; 8],
        &[1, 0, 0, 0],
        &[0xF0, 0xFF, 0xFF, 0xFF],
        &[0xFE, 0xFF],
    ];
    let mut rng = Rng(0x57A3);
    let all = formats();
    for (name, orig) in corpus() {
        for fmt in &all {
            if fmt
                .measure(&mem(orig.clone()), 0, u64::MAX)
                .unwrap()
                .is_none()
            {
                continue;
            }
            for r in 0..120 {
                let mut b = orig.clone();
                for _ in 0..1 + rng.below(4) {
                    let lim = match rng.below(3) {
                        0 => b.len().min(64),
                        1 => b.len().min(600),
                        _ => b.len(),
                    };
                    let pos = rng.below(lim as u64) as usize;
                    let st = stamps[rng.below(stamps.len() as u64) as usize];
                    for (i, x) in st.iter().enumerate() {
                        if pos + i < b.len() {
                            b[pos + i] = *x;
                        }
                    }
                }
                let start = if r % 9 == 0 {
                    rng.below(b.len() as u64 + 2)
                } else {
                    0
                };
                let limit = if r % 5 == 0 {
                    rng.below(b.len() as u64 + 2)
                } else {
                    u64::MAX
                };
                run(fmt.as_ref(), &b, start, limit, &format!("{name} stamp {r}"));
                if r % 6 == 0 {
                    run_validate(fmt.as_ref(), &b);
                }
            }
        }
    }
}
