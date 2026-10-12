//! Hostile input: no entry point may panic, hang or allocate absurdly.

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use common::*;
use diskrescue_core::format::{FileFormat, RepairContext, RepairResult, ValidationStatus};
use diskrescue_core::source::MemSource;
use diskrescue_png::repair::{self, Strategy};
use diskrescue_png::{chunks, lenient, PngFormat};

/// Run every public entry point on `bytes`. Returns repair outcome info so
/// callers can check invariants.
fn exercise(bytes: &[u8]) {
    let f = PngFormat;
    let src = mem(bytes.to_vec());
    let _ = f.probe(&bytes[..bytes.len().min(65536)]);
    let _ = f.probe(&[]);
    // Measure at several offsets and limits.
    for (start, limit) in [
        (0u64, u64::MAX),
        (0, 100),
        (0, 1),
        (1, u64::MAX),
        (7, 50),
        (bytes.len() as u64, 10),
    ] {
        if let Ok(Some(m)) = f.measure(&src, start, limit) {
            assert!(m.len >= 1);
            assert!(
                start + m.len <= bytes.len() as u64,
                "measure overruns: {m:?}"
            );
            if limit != u64::MAX {
                assert!(m.len <= limit, "{m:?}");
            }
            if let Some(e) = m.err_offset {
                assert!(e <= m.len, "{m:?}");
            }
        }
    }
    let v = f.validate(&src).unwrap();
    if v.status == ValidationStatus::Valid {
        // Anything the validator calls valid must decode with the png crate
        // (modulo trailing bytes, which it ignores).
        assert!(
            png_decode_pixels(bytes).is_ok(),
            "validator says valid, png crate disagrees"
        );
    }
    // Walk + lenient decode + plan.
    let ms = MemSource::new(bytes.to_vec());
    let w = chunks::walk(&ms, 0, u64::MAX).unwrap();
    let _ = lenient::decode(bytes);
    let img = w.ihdr.and_then(|_| lenient::analyze(&ms, &w).ok());
    let plan = repair::plan(&w, img.as_ref(), &RepairContext::default());
    // Repair through the trait and with every strategy applied explicitly.
    let mut out = Vec::new();
    let o = f.repair(&src, &mut out, &RepairContext::default()).unwrap();
    assert_eq!(o.output_len as usize, out.len());
    match o.result {
        RepairResult::Unrepairable => assert!(out.is_empty()),
        RepairResult::NoRepairNeeded => assert_eq!(out, bytes),
        RepairResult::Repaired | RepairResult::PartiallyRepaired => {
            let again = f.validate(&mem(out.clone())).unwrap();
            if o.result == RepairResult::Repaired {
                assert_eq!(
                    again.status,
                    ValidationStatus::Valid,
                    "{:?}",
                    again.problems
                );
            }
            // Whatever the validator accepts, the strict decoder must too.
            if again.status == ValidationStatus::Valid {
                assert!(
                    png_decode_pixels(&out).is_ok(),
                    "repaired output rejected by the png crate"
                );
            }
        }
    }
    for s in [
        Strategy::FixCrc,
        Strategy::ReconstructIend,
        Strategy::TruncatedIdat {
            keep_rows: u32::MAX,
        },
        Strategy::TruncatedIdat { keep_rows: 3 },
        Strategy::DropBadAncillary,
        Strategy::ResyncChunks,
    ] {
        let mut out = Vec::new();
        let o = repair::apply(&ms, &mut out, &[s], &RepairContext::default()).unwrap();
        assert_eq!(o.output_len as usize, out.len());
        if o.result == RepairResult::Unrepairable {
            assert!(out.is_empty());
        }
    }
    let mut out = Vec::new();
    let _ = repair::apply(&ms, &mut out, &plan, &RepairContext::default()).unwrap();
}

/// Iteration multiplier for local stress runs: `DISKRESCUE_FUZZ_ROUNDS=20`.
fn rounds(n: usize) -> usize {
    let mult = std::env::var("DISKRESCUE_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    n * mult
}

fn guarded(label: &str, bytes: &[u8]) {
    let t = Instant::now();
    let r = catch_unwind(AssertUnwindSafe(|| exercise(bytes)));
    if let Err(e) = r {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        let name: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let dump = std::env::temp_dir().join(format!("diskrescue-png-fuzz-{name}.bin"));
        let _ = std::fs::write(&dump, bytes);
        panic!(
            "{label}: {} bytes, panic: {msg} (input saved to {})",
            bytes.len(),
            dump.display()
        );
    }
    if t.elapsed() >= Duration::from_secs(20) {
        let name: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let dump = std::env::temp_dir().join(format!("diskrescue-png-slow-{name}.bin"));
        let _ = std::fs::write(&dump, bytes);
        panic!(
            "{label}: too slow ({:?}), input saved to {}",
            t.elapsed(),
            dump.display()
        );
    }
}

#[test]
fn random_bytes() {
    let mut rng = Rng::new(0xDEAD ^ rounds(1) as u64);
    for i in 0..rounds(300) {
        let n = rng.below(3000) as usize;
        guarded(&format!("random #{i}"), &rng.bytes(n));
    }
    guarded("empty", &[]);
    guarded("one byte", &[0x89]);
}

#[test]
fn signature_prefixed_random_bytes() {
    let mut rng = Rng::new(0xBEEF);
    for i in 0..rounds(200) {
        let mut d = SIG.to_vec();
        d.extend(rng.bytes_upto(3000));
        guarded(&format!("sig+random #{i}"), &d);
    }
    guarded("signature only", &SIG);
    // Signature + a plausible IHDR chunk + random.
    for i in 0..rounds(100) {
        let mut d = SIG.to_vec();
        let mode = all_modes()[i % 15];
        let s = Spec::new(
            1 + rng.below(40) as u32,
            1 + rng.below(40) as u32,
            mode.0,
            mode.1,
        );
        d.extend(chunk(b"IHDR", &ihdr_bytes(&s)));
        d.extend(rng.bytes_upto(500));
        guarded(&format!("sig+ihdr+random #{i}"), &d);
    }
}

#[test]
fn every_prefix_of_every_mode() {
    for (ct, depth) in all_modes() {
        for interlaced in [false, true] {
            let mut s = Spec::new(7, 5, ct, depth).chunk(40);
            s.interlaced = interlaced;
            s.ancillary = true;
            let b = build(&s);
            for k in 0..=b.png.len() {
                guarded(
                    &format!("{ct}/{depth} i={interlaced} prefix {k}"),
                    &b.png[..k],
                );
            }
        }
    }
}

#[test]
fn random_byte_flips() {
    let mut rng = Rng::new(0xF11B);
    for (ct, depth) in [(2u8, 8u8), (0, 1), (3, 4), (6, 16), (4, 8)] {
        for interlaced in [false, true] {
            let mut s = Spec::new(23, 17, ct, depth).chunk(60);
            s.interlaced = interlaced;
            s.ancillary = true;
            let b = build(&s);
            for i in 0..rounds(80) {
                let mut d = b.png.clone();
                for _ in 0..1 + rng.below(6) {
                    let at = rng.below(d.len() as u64) as usize;
                    d[at] ^= 1 << rng.below(8);
                }
                guarded(&format!("{ct}/{depth} i={interlaced} flip #{i}"), &d);
            }
        }
    }
}

/// Damage the data of a chunk but keep its CRC valid, so the structure walks
/// fine and the lenient decoder gets to deal with the pixels.
#[test]
fn damaged_data_with_valid_crcs() {
    let mut rng = Rng::new(0xC4C);
    for (ct, depth) in [(2u8, 8u8), (0, 2), (3, 8), (6, 8)] {
        for interlaced in [false, true] {
            let mut s = Spec::new(40, 30, ct, depth).chunk(200);
            s.interlaced = interlaced;
            let b = build(&s);
            let cs = chunks_of(&b.png);
            for i in 0..rounds(60) {
                let mut d = b.png.clone();
                let c = cs[rng.below(cs.len() as u64) as usize];
                if c.len == 0 {
                    continue;
                }
                for _ in 0..1 + rng.below(4) {
                    let at = c.data().start + rng.below(c.len as u64) as usize;
                    d[at] = rng.next_u64() as u8;
                }
                fix_crc_at(&mut d, &c);
                guarded(
                    &format!(
                        "{ct}/{depth} i={interlaced} data #{i} in {}",
                        String::from_utf8_lossy(&c.kind)
                    ),
                    &d,
                );
            }
        }
    }
}

#[test]
fn absurd_sizes_in_headers_and_lengths() {
    let b = build(&Spec::new(10, 10, 2, 8));
    let cs = chunks_of(&b.png);
    // IHDR dimensions, with a valid CRC.
    for (w, h) in [
        (u32::MAX, 1u32),
        (0x7FFF_FFFF, 0x7FFF_FFFF),
        (1, 0x7FFF_FFFF),
        (0x7FFF_FFFF, 1),
        (1_000_000, 1_000_000),
        (65_535, 65_535),
        (0, 0),
        (1, 0),
    ] {
        for (ct, depth, il) in [(2u8, 8u8, 0u8), (6, 16, 1), (0, 1, 0), (3, 8, 1)] {
            let mut d = b.png.clone();
            d[16..20].copy_from_slice(&w.to_be_bytes());
            d[20..24].copy_from_slice(&h.to_be_bytes());
            d[24] = depth;
            d[25] = ct;
            d[28] = il;
            let c = cs[0];
            fix_crc_at(&mut d, &c);
            guarded(&format!("dims {w}x{h} {ct}/{depth}"), &d);
        }
    }
    // Chunk lengths.
    for target in 0..cs.len() {
        for len in [
            0u32,
            1,
            0x7FFF_FFFF,
            0x8000_0000,
            u32::MAX,
            0x0100_0000,
            12,
            13,
        ] {
            let mut d = b.png.clone();
            d[cs[target].offset..cs[target].offset + 4].copy_from_slice(&len.to_be_bytes());
            guarded(&format!("length {len:#x} of chunk {target}"), &d);
        }
    }
}

#[test]
fn shuffled_duplicated_and_spliced_chunks() {
    let mut rng = Rng::new(0x5EED);
    let b = build(&Spec::new(30, 20, 2, 8).chunk(150));
    let cs = chunks_of(&b.png);
    let pieces: Vec<&[u8]> = cs.iter().map(|c| &b.png[c.offset..c.end()]).collect();
    for i in 0..rounds(150) {
        let mut d = SIG.to_vec();
        let n = 1 + rng.below(12) as usize;
        for _ in 0..n {
            let p = pieces[rng.below(pieces.len() as u64) as usize];
            d.extend_from_slice(p);
            if rng.below(5) == 0 {
                d.extend(rng.bytes_upto(40));
            }
        }
        guarded(&format!("shuffle #{i}"), &d);
    }
    // Two PNGs glued, with and without a gap, cut at random points.
    let other = build(&Spec::new(25, 25, 0, 8).seed(2));
    for i in 0..rounds(100) {
        let cut_a = rng.below(b.png.len() as u64) as usize;
        let cut_b = rng.below(other.png.len() as u64) as usize;
        let mut d = b.png[..cut_a].to_vec();
        if rng.below(2) == 0 {
            d.extend(rng.bytes_upto(50));
        }
        d.extend_from_slice(&other.png[cut_b..]);
        guarded(&format!("splice #{i}"), &d);
    }
}

#[test]
fn zero_filled_and_ff_filled_holes() {
    let b = build(&Spec::new(120, 80, 2, 8).chunk(900));
    let mut rng = Rng::new(3);
    for i in 0..rounds(60) {
        let mut d = b.png.clone();
        let at = rng.below(d.len() as u64 - 600) as usize;
        let len = 1 + rng.below(512) as usize;
        let fill = if i % 2 == 0 { 0x00 } else { 0xFF };
        d[at..at + len].fill(fill);
        guarded(&format!("hole #{i}"), &d);
    }
}

#[test]
fn deflate_bombs_are_bounded() {
    // 40 MB of zeros compress to ~40 KB; the image claims 4 rows.
    let spec = Spec::new(2, 4, 0, 8);
    let z = zlib(&vec![0u8; 40 << 20], 9);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let t = Instant::now();
    let img = lenient::decode(&png).unwrap();
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(img.rows_recovered, 4);
    assert!(img.inflate_error.is_some());
    assert!(img.excess_bytes <= 17 << 20, "excess {}", img.excess_bytes);
    let v = PngFormat.validate(&mem(png)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
}

#[test]
fn measure_with_garbage_around_a_valid_file() {
    let f = PngFormat;
    let b = build(&Spec::new(40, 30, 2, 8));
    let mut rng = Rng::new(8);
    for _ in 0..50 {
        let pre = rng.below(300) as usize;
        let mut d = rng.bytes(pre);
        d.extend_from_slice(&b.png);
        d.extend(rng.bytes_upto(300));
        let src = mem(d);
        let m = f.measure(&src, pre as u64, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, b.png.len() as u64);
    }
}
