//! Repair strategies against ground truth.

mod common;

use common::*;
use diskrescue_core::format::{FileFormat, RepairContext, RepairResult, ValidationStatus};
use diskrescue_core::source::MemSource;
use diskrescue_png::chunks;
use diskrescue_png::lenient;
use diskrescue_png::repair::{self, Strategy};
use diskrescue_png::PngFormat;

struct Repaired {
    result: RepairResult,
    bytes: Vec<u8>,
    applied: Vec<String>,
    notes: Vec<String>,
}

fn repair_with(bytes: &[u8], ctx: &RepairContext) -> Repaired {
    let src = mem(bytes.to_vec());
    let mut out = Vec::new();
    let o = PngFormat.repair(&src, &mut out, ctx).unwrap();
    assert_eq!(
        o.output_len as usize,
        out.len(),
        "output_len must match what was written"
    );
    if o.result == RepairResult::Unrepairable {
        assert!(
            out.is_empty(),
            "nothing may be written for an unrepairable file"
        );
    }
    if matches!(
        o.result,
        RepairResult::Repaired | RepairResult::NoRepairNeeded
    ) {
        let v = o.validation.as_ref().expect("validation of the output");
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    }
    Repaired {
        result: o.result,
        bytes: out,
        applied: o.applied,
        notes: o.notes,
    }
}

fn do_repair(bytes: &[u8]) -> Repaired {
    repair_with(bytes, &RepairContext::default())
}

fn validate(bytes: &[u8]) -> ValidationStatus {
    PngFormat.validate(&mem(bytes.to_vec())).unwrap().status
}

#[test]
fn valid_input_needs_no_repair() {
    let b = build(&Spec::new(40, 30, 2, 8));
    let r = do_repair(&b.png);
    assert_eq!(r.result, RepairResult::NoRepairNeeded);
    assert_eq!(r.bytes, b.png);
    assert!(r.applied.is_empty());
}

#[test]
fn bad_ihdr_crc_is_fixed() {
    for (ct, depth) in [(2u8, 8u8), (6, 16), (3, 4), (0, 1)] {
        let b = build(&Spec::new(31, 17, ct, depth));
        let mut bad = b.png.clone();
        let c = find(&bad, b"IHDR", 0);
        bad[c.crc_at() + 1] ^= 0x40;
        assert_eq!(validate(&bad), ValidationStatus::Damaged);
        let r = do_repair(&bad);
        assert_eq!(
            r.result,
            RepairResult::Repaired,
            "{ct}/{depth}: {:?}",
            r.notes
        );
        assert_eq!(r.bytes, b.png, "only the CRC may change");
        assert!(r.applied.iter().any(|a| a.contains("CRC")));
        assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    }
}

#[test]
fn bad_idat_crc_is_fixed_when_the_stream_decodes() {
    let b = build(&Spec::new(120, 90, 2, 8).chunk(2000));
    let n_idat = chunks_of(&b.png)
        .iter()
        .filter(|c| &c.kind == b"IDAT")
        .count();
    assert!(n_idat > 5);
    for which in [0usize, 3, n_idat - 1] {
        let mut bad = b.png.clone();
        let c = find(&bad, b"IDAT", which);
        bad[c.crc_at() + 3] ^= 0x01;
        let r = do_repair(&bad);
        assert_eq!(r.result, RepairResult::Repaired);
        assert_eq!(r.bytes, b.png);
        assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    }
    // Several at once, plus IHDR.
    let mut bad = b.png.clone();
    for (kind, n) in [(b"IHDR", 0usize), (b"IDAT", 1), (b"IDAT", 4), (b"tEXt", 0)] {
        let c = find(&bad, kind, n);
        bad[c.crc_at()] ^= 0x80;
    }
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::Repaired);
    assert_eq!(r.bytes, b.png);
}

#[test]
fn bad_crc_on_interlaced_and_single_chunk_images() {
    for spec in [
        Spec::new(50, 40, 6, 8).interlaced().chunk(900),
        Spec::new(50, 40, 0, 16).chunk(0),
        Spec::new(33, 33, 3, 2).interlaced().chunk(0),
    ] {
        let b = build(&spec);
        let mut bad = b.png.clone();
        let c = find(&bad, b"IDAT", 0);
        bad[c.crc_at() + 2] ^= 0x10;
        let r = do_repair(&bad);
        assert_eq!(r.result, RepairResult::Repaired, "{spec:?}");
        assert_eq!(r.bytes, b.png);
    }
}

#[test]
fn ancillary_chunk_policy() {
    let mut spec = Spec::new(40, 30, 2, 8);
    spec.ancillary = true;
    spec.trailing_text = true;
    let b = build(&spec);
    // tEXt data damaged: CRC rewritten, chunk kept.
    let mut bad = b.png.clone();
    let c = find(&bad, b"tEXt", 0);
    bad[c.data().start + 1] ^= 0x01;
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::Repaired);
    assert!(r.bytes.windows(4).any(|w| w == b"tEXt"));
    assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    // gAMA damaged: dropped (a wrong gamma would change the picture).
    let mut bad = b.png.clone();
    let c = find(&bad, b"gAMA", 0);
    bad[c.data().start] ^= 0x01;
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::Repaired);
    assert!(!r.bytes.windows(4).any(|w| w == b"gAMA"));
    assert!(r.bytes.windows(4).any(|w| w == b"pHYs"));
    assert!(r.applied.iter().any(|a| a.contains("gAMA")));
    assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    // Trailing tEXt (after the IDATs) is handled too.
    let mut bad = b.png.clone();
    let c = find(&bad, b"tEXt", 1);
    bad[c.crc_at()] ^= 1;
    let r = do_repair(&bad);
    assert_eq!(r.bytes, b.png);
}

#[test]
fn bad_iccp_is_kept_only_if_it_inflates() {
    let spec = Spec::new(20, 20, 2, 8);
    let raw = gen_raw(&spec);
    let z = zlib(&filtered_stream(&spec, &raw), 6);
    let profile = zlib(&[7u8; 300], 6);
    let mut iccp = b"profile\0\0".to_vec();
    iccp.extend_from_slice(&profile);
    let make = |iccp_data: &[u8]| {
        let mut png = SIG.to_vec();
        png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
        png.extend(chunk(b"iCCP", iccp_data));
        png.extend(chunk(b"IDAT", &z));
        png.extend(chunk(b"IEND", &[]));
        png
    };
    // Good payload, damaged CRC: rewritten.
    let mut a = make(&iccp);
    let c = find(&a, b"iCCP", 0);
    a[c.crc_at()] ^= 1;
    let r = do_repair(&a);
    assert_eq!(r.result, RepairResult::Repaired);
    assert!(r.bytes.windows(4).any(|w| w == b"iCCP"));
    // Garbage payload: dropped.
    let mut garbage = b"profile\0\0".to_vec();
    garbage.extend_from_slice(&[0xDE; 40]);
    let mut a = make(&garbage);
    let c = find(&a, b"iCCP", 0);
    a[c.crc_at()] ^= 1;
    let r = do_repair(&a);
    assert_eq!(r.result, RepairResult::Repaired);
    assert!(!r.bytes.windows(4).any(|w| w == b"iCCP"));
}

#[test]
fn missing_iend_is_reconstructed_byte_exact() {
    for spec in [
        Spec::new(60, 40, 2, 8),
        Spec::new(60, 40, 3, 4).interlaced(),
        Spec::new(1, 1, 0, 1),
    ] {
        let b = build(&spec);
        let cut = &b.png[..b.png.len() - 12];
        assert_eq!(validate(cut), ValidationStatus::Damaged);
        let r = do_repair(cut);
        assert_eq!(r.result, RepairResult::Repaired, "{spec:?}");
        assert_eq!(r.bytes, b.png);
        assert!(r.applied.iter().any(|a| a.contains("IEND")));
    }
    // IEND partly present.
    let b = build(&Spec::new(60, 40, 2, 8));
    for keep in 1..12usize {
        let cut = &b.png[..b.png.len() - 12 + keep];
        let r = do_repair(cut);
        assert_eq!(r.result, RepairResult::Repaired, "keep {keep}");
        assert_eq!(r.bytes, b.png, "keep {keep}");
    }
}

#[test]
fn garbage_between_chunks_is_dropped_byte_exact() {
    let mut spec = Spec::new(70, 50, 2, 8).chunk(1500);
    spec.ancillary = true;
    let b = build(&spec);
    let cs = chunks_of(&b.png);
    let mut rng = Rng::new(42);
    // After IHDR, in the middle of the IDATs, right before IEND.
    for idx in [1usize, 4, cs.len() - 1] {
        for glen in [5usize, 100, 5000] {
            let at = cs[idx].offset;
            let mut bad = b.png[..at].to_vec();
            let mut g = rng.bytes(glen);
            // Make the garbage unparseable as a chunk header.
            g[4] = 0x01;
            bad.extend_from_slice(&g);
            bad.extend_from_slice(&b.png[at..]);
            assert_eq!(validate(&bad), ValidationStatus::Damaged);
            let r = do_repair(&bad);
            assert_eq!(
                r.result,
                RepairResult::Repaired,
                "idx {idx} len {glen}: {:?}",
                r.notes
            );
            assert_eq!(r.bytes, b.png, "idx {idx} len {glen}");
            assert!(r.applied.iter().any(|a| a.contains("garbage")));
        }
    }
}

#[test]
fn damaged_length_field_is_rewritten() {
    let b = build(&Spec::new(70, 50, 2, 8).chunk(1500));
    let c = find(&b.png, b"IDAT", 3);
    for bad_len in [c.len as u32 + 77, c.len as u32 - 40, 0x7000_0000] {
        let mut bad = b.png.clone();
        bad[c.offset..c.offset + 4].copy_from_slice(&bad_len.to_be_bytes());
        assert_eq!(validate(&bad), ValidationStatus::Damaged);
        let r = do_repair(&bad);
        assert_eq!(r.result, RepairResult::Repaired, "{bad_len}: {:?}", r.notes);
        assert_eq!(r.bytes, b.png);
    }
}

fn expected_fill(spec: &Spec) -> Vec<u8> {
    // The fill row the repair writes, built independently of the crate.
    let rb = spec.row_bytes();
    let mut row = vec![0u8; rb];
    match (spec.color_type, spec.bit_depth) {
        (0, 8) | (2, 8) => row.fill(0x80),
        (0, 16) => {
            for p in row.chunks_mut(2) {
                p[0] = 0x80;
            }
        }
        (2, 16) => {
            for p in row.chunks_mut(2) {
                p[0] = 0x80;
            }
        }
        (6, 8) => {
            for p in row.chunks_mut(4) {
                p[..3].fill(0x80);
            }
        }
        (4, 8) => {
            for p in row.chunks_mut(2) {
                p[0] = 0x80;
            }
        }
        _ => panic!("no expectation for {spec:?}"),
    }
    row
}

#[test]
fn truncated_idat_keeps_recovered_rows_and_fills_the_rest() {
    for (ct, depth) in [(2u8, 8u8), (6, 8), (0, 8), (4, 8), (0, 16), (2, 16)] {
        let spec = Spec::new(160, 120, ct, depth).chunk(5000).seed(31);
        let b = build(&spec);
        let (lo, hi) = {
            let cs: Vec<_> = chunks_of(&b.png)
                .into_iter()
                .filter(|c| &c.kind == b"IDAT")
                .collect();
            (cs[0].data().start, cs.last().unwrap().data().end)
        };
        for pct in [25usize, 50, 75] {
            let cut = lo + (hi - lo) * pct / 100;
            let truncated = &b.png[..cut];
            assert_eq!(validate(truncated), ValidationStatus::Damaged);
            let rec = lenient::decode(truncated).unwrap().rows_recovered as usize;
            let r = do_repair(truncated);
            assert_eq!(
                r.result,
                RepairResult::PartiallyRepaired,
                "{ct}/{depth} {pct}%"
            );
            // The output decodes, but it carries the repair marker that says
            // the filled rows are not real, so it never validates as intact.
            assert_eq!(validate(&r.bytes), ValidationStatus::Damaged);
            assert!(
                chunks_of(&r.bytes).iter().any(|c| &c.kind == b"tEXt"
                    && r.bytes[c.data()].starts_with(b"DiskRescue\0partly recovered: intact=")),
                "{ct}/{depth} {pct}%: repair marker present"
            );
            let d = png_decode(&r.bytes).unwrap_or_else(|e| panic!("{ct}/{depth} {pct}%: {e}"));
            assert_eq!((d.width, d.height), (spec.width, spec.height));
            assert_eq!(d.color as u8, ct);
            let rb = spec.row_bytes();
            assert_eq!(
                &d.data[..rec * rb],
                &b.raw[..rec * rb],
                "recovered rows must be exact"
            );
            let fill = expected_fill(&spec);
            for y in rec..spec.height as usize {
                assert_eq!(
                    dec_row(&d, y),
                    &fill[..],
                    "row {y} should be filler ({ct}/{depth})"
                );
            }
            assert!(
                r.applied
                    .iter()
                    .any(|a| a.contains(&format!("{rec} of 120 rows"))),
                "{:?}",
                r.applied
            );
            assert!(r.notes.iter().any(|n| n.contains("rows recovered")));
            // Proportional.
            let frac = rec as f64 / 120.0;
            assert!((frac - pct as f64 / 100.0).abs() < 0.12, "{frac} vs {pct}%");
        }
    }
}

#[test]
fn truncated_palette_images_use_the_palette() {
    for depth in [1u8, 2, 4, 8] {
        let spec = Spec::new(90, 70, 3, depth).chunk(300).seed(4);
        let b = build(&spec);
        let cs: Vec<_> = chunks_of(&b.png)
            .into_iter()
            .filter(|c| &c.kind == b"IDAT")
            .collect();
        let cut = cs[0].data().start + (cs.last().unwrap().data().end - cs[0].data().start) / 2;
        let r = do_repair(&b.png[..cut]);
        assert_eq!(r.result, RepairResult::PartiallyRepaired, "depth {depth}");
        let d = png_decode(&r.bytes).unwrap();
        assert_eq!(d.color, png::ColorType::Indexed);
        let rec = lenient::decode(&b.png[..cut]).unwrap().rows_recovered as usize;
        let rb = spec.row_bytes();
        assert_eq!(&d.data[..rec * rb], &b.raw[..rec * rb]);
        // The filler uses one palette index everywhere: closest to grey.
        let pal = spec.palette();
        let want_idx = (0..(1usize << depth))
            .min_by_key(|&i| {
                (0..3)
                    .map(|c| (pal[i * 3 + c] as i32 - 128).abs())
                    .sum::<i32>()
            })
            .unwrap();
        let mut want_row = vec![0u8; rb];
        for x in 0..spec.width as usize {
            let bit = x * depth as usize;
            if depth == 8 {
                want_row[x] = want_idx as u8;
            } else {
                let shift = 8 - depth as usize - bit % 8;
                want_row[bit / 8] |= (want_idx as u8) << shift;
            }
        }
        for y in rec..70 {
            assert_eq!(dec_row(&d, y), &want_row[..], "depth {depth} row {y}");
        }
    }
}

#[test]
fn truncated_one_bit_gray_fills_and_decodes() {
    let spec = Spec::new(100, 80, 0, 1).chunk(40).seed(2);
    let b = build(&spec);
    let cs: Vec<_> = chunks_of(&b.png)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    let cut = cs[0].data().start + (cs.last().unwrap().data().end - cs[0].data().start) / 2;
    let r = do_repair(&b.png[..cut]);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    let d = png_decode(&r.bytes).unwrap();
    assert_eq!((d.width, d.height), (100, 80));
    let rec = lenient::decode(&b.png[..cut]).unwrap().rows_recovered as usize;
    assert!(rec > 0 && rec < 80);
    let rb = spec.row_bytes();
    assert_eq!(&d.data[..rec * rb], &b.raw[..rec * rb]);
}

#[test]
fn truncated_interlaced_gives_a_full_frame_preview() {
    let spec = Spec::new(128, 96, 2, 8).interlaced().chunk(4000).seed(13);
    let b = build(&spec);
    let cs: Vec<_> = chunks_of(&b.png)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    let (lo, hi) = (cs[0].data().start, cs.last().unwrap().data().end);
    let mut last_known = 0usize;
    for pct in [8usize, 30, 60, 85] {
        let cut = lo + (hi - lo) * pct / 100;
        let t = &b.png[..cut];
        let img = lenient::decode(t).unwrap();
        let r = do_repair(t);
        assert_eq!(r.result, RepairResult::PartiallyRepaired, "{pct}%");
        let d = png_decode(&r.bytes).unwrap();
        assert_eq!((d.width, d.height), (128, 96));
        // Not interlaced any more.
        assert_eq!(r.bytes[8 + 8 + 12], 0, "interlace byte must be cleared");
        let rb = spec.row_bytes();
        let mut known = 0usize;
        for y in 0..96u32 {
            for x in 0..128u32 {
                let o = y as usize * rb + x as usize * 3;
                if img.pixel_known(x, y) {
                    known += 1;
                    assert_eq!(
                        &d.data[o..o + 3],
                        &b.raw[o..o + 3],
                        "known pixel {x},{y} at {pct}%"
                    );
                }
            }
        }
        assert!(known >= last_known);
        last_known = known;
        // Oracle for the blocky preview: each pixel shows the latest
        // recovered Adam7 pass pixel whose display cell covers it.
        let cells = [
            (8usize, 8usize),
            (4, 8),
            (4, 4),
            (2, 4),
            (2, 2),
            (1, 2),
            (1, 1),
        ];
        let adam = [
            (0usize, 0usize, 8usize, 8usize),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        for y in 0..96usize {
            for x in 0..128usize {
                let mut want: Option<usize> = None;
                for p in (0..7).rev() {
                    let (x0, y0, dx, dy) = adam[p];
                    let (cw, ch) = cells[p];
                    if x < x0 || y < y0 {
                        continue;
                    }
                    let px = x0 + dx * ((x - x0) / dx);
                    let py = y0 + dy * ((y - y0) / dy);
                    if x - px >= cw || y - py >= ch {
                        continue;
                    }
                    let row_in_pass = (py - y0) / dy;
                    if (row_in_pass as u32) < img.pass_rows_recovered[p] {
                        want = Some(py * rb + px * 3);
                        break;
                    }
                }
                let o = y * rb + x * 3;
                match want {
                    Some(src) => {
                        assert_eq!(&d.data[o..o + 3], &b.raw[src..src + 3], "{x},{y} at {pct}%")
                    }
                    None => assert_eq!(
                        &d.data[o..o + 3],
                        &[0x80, 0x80, 0x80],
                        "{x},{y} should be filler at {pct}%"
                    ),
                }
            }
        }
    }
    assert!(last_known > 128 * 96 / 2);
}

#[test]
fn truncated_with_missing_chunks_and_apng_chunks_dropped() {
    // acTL/fcTL/fdAT do not survive re-encoding of the still.
    let spec = Spec::new(40, 40, 6, 8);
    let raw = gen_raw(&spec);
    let z = zlib(&filtered_stream(&spec, &raw), 6);
    let mut actl = Vec::new();
    actl.extend_from_slice(&1u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"acTL", &actl));
    let half = z.len() / 2;
    png.extend(chunk(b"IDAT", &z[..half]));
    let r = do_repair(&png);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    assert!(!r.bytes.windows(4).any(|w| w == b"acTL"));
    assert!(png_decode(&r.bytes).is_ok());
}

#[test]
fn crc_damage_inside_the_image_data_gives_a_verified_prefix() {
    let spec = Spec::new(200, 140, 2, 8).chunk(3000).seed(17);
    let b = build(&spec);
    let cs: Vec<_> = chunks_of(&b.png)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    let mut bad = b.png.clone();
    bad[cs[cs.len() / 2].data().start + 100] ^= 0x33;
    let v = PngFormat.validate(&mem(bad.clone())).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    assert!(v.intact_fraction.unwrap() < 1.0);
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    let d = png_decode(&r.bytes).unwrap();
    let rec = lenient::decode(&bad).unwrap().rows_recovered as usize;
    let rb = spec.row_bytes();
    assert!(rec > 20 && rec < 140);
    assert_eq!(&d.data[..rec * rb], &b.raw[..rec * rb]);
}

#[test]
fn donor_ihdr_restores_a_destroyed_header() {
    let dir = tempfile::tempdir().unwrap();
    for (ct, depth) in [(2u8, 8u8), (6, 8), (0, 8), (3, 4), (2, 16)] {
        let spec = Spec::new(64, 48, ct, depth).seed(3);
        let b = build(&spec);
        // A different picture from the same "camera": same IHDR, same palette.
        let donor_bytes = build(&Spec::new(64, 48, ct, depth).seed(77)).png;
        let donor = dir.path().join(format!("donor_{ct}_{depth}.png"));
        std::fs::write(&donor, &donor_bytes).unwrap();
        // Wrong donor (other dimensions).
        let wrong = dir.path().join(format!("wrong_{ct}_{depth}.png"));
        std::fs::write(&wrong, build(&Spec::new(65, 48, ct, depth)).png).unwrap();

        // 1. IHDR data scrambled (CRC now wrong too).
        let mut bad = b.png.clone();
        let c = find(&bad, b"IHDR", 0);
        for i in c.data() {
            bad[i] = 0xEE;
        }
        assert_eq!(validate(&bad), ValidationStatus::Damaged);
        let none = do_repair(&bad);
        assert_eq!(
            none.result,
            RepairResult::Unrepairable,
            "no donor => unrepairable"
        );
        let ctx = RepairContext {
            donors: vec![wrong.clone(), donor.clone()],
            ..Default::default()
        };
        let r = repair_with(&bad, &ctx);
        assert_eq!(
            r.result,
            RepairResult::Repaired,
            "{ct}/{depth}: {:?} {:?}",
            r.applied,
            r.notes
        );
        assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
        assert!(r.applied.iter().any(|a| a.contains("donor")));
        assert_eq!(r.bytes, b.png, "donor header equals the lost one");

        // 2. IHDR chunk gone entirely (and the palette with it).
        let mut gone = b.png[..8].to_vec();
        gone.extend_from_slice(&b.png[c.end()..]);
        if ct == 3 {
            let p = find(&gone, b"PLTE", 0);
            gone.drain(p.offset..p.end());
        }
        let r = repair_with(&gone, &ctx);
        assert_eq!(
            r.result,
            RepairResult::Repaired,
            "{ct}/{depth} missing: {:?} {:?}",
            r.applied,
            r.notes
        );
        assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    }
}

#[test]
fn donor_ihdr_when_data_is_also_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let spec = Spec::new(100, 80, 2, 8).chunk(2000).seed(3);
    let b = build(&spec);
    let donor = dir.path().join("donor.png");
    std::fs::write(&donor, build(&Spec::new(100, 80, 2, 8).seed(5)).png).unwrap();
    let mut bad = b.png.clone();
    let c = find(&bad, b"IHDR", 0);
    for i in c.data() {
        bad[i] = 0;
    }
    let cs: Vec<_> = chunks_of(&bad)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    let cut = cs[cs.len() / 2].data().start + 10;
    let ctx = RepairContext {
        donors: vec![donor],
        ..Default::default()
    };
    let r = repair_with(&bad[..cut], &ctx);
    assert_eq!(
        r.result,
        RepairResult::PartiallyRepaired,
        "{:?} {:?}",
        r.applied,
        r.notes
    );
    let d = png_decode(&r.bytes).unwrap();
    assert_eq!((d.width, d.height), (100, 80));
    let rb = spec.row_bytes();
    let rows_ok = (0..80)
        .take_while(|&y| dec_row(&d, y) != vec![0x80u8; rb].as_slice())
        .count();
    assert!(rows_ok > 20);
    assert_eq!(&d.data[..rows_ok * rb], &b.raw[..rows_ok * rb]);
}

#[test]
fn unrepairable_inputs_write_nothing() {
    let b = build(&Spec::new(80, 60, 2, 8));
    // Not a PNG.
    let r = do_repair(b"definitely not a png file, just some text");
    assert_eq!(r.result, RepairResult::Unrepairable);
    // Signature only.
    let r = do_repair(&SIG);
    assert_eq!(r.result, RepairResult::Unrepairable);
    // Image data is noise from the very first byte.
    let mut bad = SIG.to_vec();
    bad.extend(chunk(b"IHDR", &ihdr_bytes(&b.spec)));
    let mut rng = Rng::new(9);
    bad.extend(chunk(b"IDAT", &rng.bytes(3000)));
    bad.extend(chunk(b"IEND", &[]));
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::Unrepairable, "{:?}", r.notes);
    // No IDAT at all.
    let mut bad = SIG.to_vec();
    bad.extend(chunk(b"IHDR", &ihdr_bytes(&b.spec)));
    bad.extend(chunk(b"IEND", &[]));
    let r = do_repair(&bad);
    assert_eq!(r.result, RepairResult::Unrepairable);
    // Empty.
    let r = do_repair(&[]);
    assert_eq!(r.result, RepairResult::Unrepairable);
}

#[test]
fn plan_names_the_right_strategies() {
    let ctx = RepairContext::default();
    let plan_for = |bytes: &[u8]| {
        let src = MemSource::new(bytes.to_vec());
        let walk = chunks::walk(&src, 0, u64::MAX).unwrap();
        let img = walk.ihdr.map(|_| lenient::analyze(&src, &walk).unwrap());
        repair::plan(&walk, img.as_ref(), &ctx)
    };
    let b = build(&Spec::new(100, 80, 2, 8).chunk(2000));
    assert!(plan_for(&b.png).is_empty());
    assert_eq!(
        plan_for(&b.png[..b.png.len() - 12]),
        vec![Strategy::ReconstructIend]
    );
    let mut bad = b.png.clone();
    let c = find(&bad, b"IDAT", 2);
    bad[c.crc_at()] ^= 1;
    assert_eq!(plan_for(&bad), vec![Strategy::FixCrc]);
    let p = plan_for(&b.png[..b.png.len() / 2]);
    assert!(
        matches!(p.as_slice(), [Strategy::TruncatedIdat { keep_rows }] if *keep_rows > 10 && *keep_rows < 80),
        "{p:?}"
    );
    assert_eq!(Strategy::FixCrc.name(), "fix_crc");
    assert_eq!(Strategy::ResyncChunks.name(), "resync_chunks");
    assert_eq!(Strategy::DropBadAncillary.name(), "drop_bad_ancillary");
    assert_eq!(Strategy::ReconstructIend.name(), "reconstruct_iend");
    assert_eq!(
        Strategy::TruncatedIdat { keep_rows: 1 }.name(),
        "truncated_idat"
    );
    assert_eq!(
        Strategy::DonorIhdr { donor: "x".into() }.name(),
        "donor_ihdr"
    );
    // Garbage between chunks.
    let cs = chunks_of(&b.png);
    let mut g = b.png[..cs[3].offset].to_vec();
    g.extend_from_slice(&[0xFF; 50]);
    g.extend_from_slice(&b.png[cs[3].offset..]);
    assert_eq!(plan_for(&g), vec![Strategy::ResyncChunks]);
}

#[test]
fn apply_runs_explicit_strategies() {
    let b = build(&Spec::new(100, 80, 2, 8).chunk(2000));
    let src = MemSource::new(b.png[..b.png.len() - 12].to_vec());
    let mut out = Vec::new();
    let o = repair::apply(
        &src,
        &mut out,
        &[Strategy::ReconstructIend],
        &RepairContext::default(),
    )
    .unwrap();
    assert_eq!(o.result, RepairResult::Repaired);
    assert_eq!(out, b.png);
    // Asking for nothing useful changes nothing and writes nothing.
    let mut out = Vec::new();
    let o = repair::apply(&src, &mut out, &[], &RepairContext::default()).unwrap();
    assert_eq!(o.result, RepairResult::Unrepairable);
    assert!(out.is_empty());
    // TruncatedIdat with keep_rows limits what is kept.
    let src = MemSource::new(b.png[..b.png.len() / 2].to_vec());
    let rec = lenient::decode(&b.png[..b.png.len() / 2])
        .unwrap()
        .rows_recovered;
    let mut out = Vec::new();
    let o = repair::apply(
        &src,
        &mut out,
        &[Strategy::TruncatedIdat { keep_rows: 10 }],
        &RepairContext::default(),
    )
    .unwrap();
    assert_eq!(o.result, RepairResult::PartiallyRepaired);
    let d = png_decode(&out).unwrap();
    let rb = b.spec.row_bytes();
    assert_eq!(&d.data[..10 * rb], &b.raw[..10 * rb]);
    assert!(d.data[10 * rb..11 * rb].iter().all(|&x| x == 0x80));
    assert!(rec > 10);
}

#[test]
fn zlib_header_and_adler_damage_are_repaired_by_reencoding() {
    let spec = Spec::new(50, 40, 2, 8).chunk(0);
    let b = build(&spec);
    // Missing Adler: all rows present => Repaired (nothing lost).
    let idat = find(&b.png, b"IDAT", 0);
    let mut z = b.png[idat.data()].to_vec();
    z.truncate(z.len() - 4);
    let mut png = b.png[..idat.offset].to_vec();
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let r = do_repair(&png);
    assert_eq!(
        r.result,
        RepairResult::Repaired,
        "{:?} {:?}",
        r.applied,
        r.notes
    );
    assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
    // Bad zlib header.
    let mut bad = b.png.clone();
    bad[idat.data().start] = 0xFF;
    bad[idat.data().start + 1] = 0xFF;
    let c = find(&bad, b"IDAT", 0);
    fix_crc_at(&mut bad, &c);
    let r = do_repair(&bad);
    assert_eq!(
        r.result,
        RepairResult::Repaired,
        "{:?} {:?}",
        r.applied,
        r.notes
    );
    assert_eq!(png_decode(&r.bytes).unwrap().data, b.raw);
}

#[test]
fn checksum_failure_is_reported_as_partial() {
    // Single chunk: the CRC cannot localise; flip a byte, the stream often
    // decodes to the end with a wrong Adler. Whatever happens, the result is
    // never claimed as a clean repair unless the data is exactly right.
    let spec = Spec::new(150, 100, 2, 8).chunk(0).seed(6);
    let b = build(&spec);
    let idat = find(&b.png, b"IDAT", 0);
    let mut bad = b.png.clone();
    let mut hits = 0;
    for pct in [20usize, 35, 50, 65, 80] {
        let mut x = bad.clone();
        let pos = idat.data().start + idat.len * pct / 100;
        x[pos] ^= 0x04;
        let r = do_repair(&x);
        match r.result {
            RepairResult::Repaired => {
                assert_eq!(
                    png_decode(&r.bytes).unwrap().data,
                    b.raw,
                    "claimed repaired but differs"
                );
            }
            RepairResult::PartiallyRepaired => hits += 1,
            RepairResult::Unrepairable => hits += 1,
            RepairResult::NoRepairNeeded => panic!("damaged input"),
        }
        bad = b.png.clone();
    }
    assert!(hits >= 1);
}

#[test]
fn cgbi_is_only_repaired_structurally() {
    let spec = Spec::new(24, 20, 6, 8);
    let raw = gen_raw(&spec);
    let filtered = filtered_stream(&spec, &raw);
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    std::io::Write::write_all(&mut e, &filtered).unwrap();
    let deflate = e.finish().unwrap();
    let mut png = SIG.to_vec();
    png.extend(chunk(b"CgBI", &[0x50, 0x00, 0x20, 0x06]));
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &deflate));
    let r = do_repair(&png);
    assert_eq!(r.result, RepairResult::Repaired);
    assert!(r.bytes.windows(4).any(|w| w == b"CgBI"));
    assert!(r.bytes.ends_with(&chunk(b"IEND", &[])));
}

#[test]
fn repair_leaves_no_temp_files_and_handles_big_outputs() {
    // Output larger than the in-memory spool threshold (16 MiB).
    let dir = tempfile::tempdir().unwrap();
    let mut rng = Rng::new(1);
    let (w, h) = (3000u32, 2400u32);
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    rng.fill(&mut rgb);
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&rgb).unwrap();
    }
    assert!(png_bytes.len() > 17 << 20, "{}", png_bytes.len());
    // Cut at 90 %: the repaired file is far above 16 MiB.
    let cut = png_bytes.len() * 9 / 10;
    let ctx = RepairContext {
        scratch_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    };
    let r = repair_with(&png_bytes[..cut], &ctx);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    assert!(r.bytes.len() > 16 << 20);
    let d = png_decode(&r.bytes).unwrap();
    assert_eq!((d.width, d.height), (w, h));
    let rb = (w * 3) as usize;
    let rec = lenient::decode(&png_bytes[..cut]).unwrap().rows_recovered as usize;
    assert!(rec > 2000);
    assert_eq!(&d.data[..rec * rb], &rgb[..rec * rb]);
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(left.is_empty(), "temp files left behind: {left:?}");
}

#[test]
fn interlaced_with_only_the_first_pass_is_a_blocky_preview() {
    let spec = Spec::new(64, 48, 2, 8).interlaced();
    let raw = gen_raw(&spec);
    let stream = filtered_stream(&spec, &raw);
    // Pass 1 of 64x48: 8x6 pixels = 6 rows of (1 + 8*3) bytes.
    let pass1 = 6 * (1 + 8 * 3);
    let z = zlib(&stream[..pass1], 6);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.pass_rows_recovered, [6, 0, 0, 0, 0, 0, 0]);
    let r = do_repair(&png);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    let d = png_decode(&r.bytes).unwrap();
    let rb = spec.row_bytes();
    // Every 8x8 block is one flat colour: the pass-1 pixel.
    for by in 0..6usize {
        for bx in 0..8usize {
            let want = &raw[(by * 8) * rb + bx * 8 * 3..][..3];
            for y in 0..8usize {
                for x in 0..8usize {
                    let o = (by * 8 + y) * rb + (bx * 8 + x) * 3;
                    assert_eq!(&d.data[o..o + 3], want, "block {bx},{by} px {x},{y}");
                }
            }
        }
    }
}

#[test]
fn interlaced_partial_first_pass_leaves_the_rest_grey() {
    // Only 3 of the 6 pass-1 rows decode: image rows 0..24 are the preview,
    // below that is filler.
    let spec = Spec::new(64, 48, 2, 8).interlaced();
    let raw = gen_raw(&spec);
    let stream = filtered_stream(&spec, &raw);
    let z = zlib(&stream[..3 * (1 + 8 * 3)], 6);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let r = do_repair(&png);
    assert_eq!(r.result, RepairResult::PartiallyRepaired);
    let d = png_decode(&r.bytes).unwrap();
    let rb = spec.row_bytes();
    assert_eq!(&d.data[..3], &raw[..3]);
    for y in 24..48usize {
        assert!(dec_row(&d, y).iter().all(|&b| b == 0x80), "row {y}");
    }
    assert!(!dec_row(&d, 23).iter().all(|&b| b == 0x80));
    let _ = rb;
}
