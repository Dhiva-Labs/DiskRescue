//! Lenient decoding against ground truth.

mod common;

use common::*;
use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::MemSource;
use diskrescue_png::chunks;
use diskrescue_png::lenient::{self, LenientOptions};

fn idat_span(png: &[u8]) -> (usize, usize) {
    let cs: Vec<_> = chunks_of(png)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    (
        cs.first().unwrap().data().start,
        cs.last().unwrap().data().end,
    )
}

#[test]
fn clean_images_decode_exactly() {
    for (ct, depth) in all_modes() {
        for interlaced in [false, true] {
            for (w, h) in [(1u32, 1u32), (3, 5), (19, 23), (64, 48)] {
                let mut s = Spec::new(w, h, ct, depth);
                s.interlaced = interlaced;
                let b = build(&s);
                let img = lenient::decode(&b.png).unwrap();
                assert!(
                    img.inflate_error.is_none(),
                    "{s:?}: {:?}",
                    img.inflate_error
                );
                assert_eq!(img.rows_recovered, img.rows_total);
                assert_eq!(img.pixels, b.raw, "{s:?}");
                assert!(img.recovered_mask_rows.iter().all(|&m| m));
                assert_eq!(img.recovered_mask_rows.len(), h as usize);
                assert!(img.stream_complete);
                assert_eq!(img.interlaced, interlaced);
                assert_eq!((img.width, img.height), (w, h));
                assert_eq!((img.color_type, img.bit_depth), (ct, depth));
                assert_eq!(img.bytes_inflated, img.expected_bytes);
            }
        }
    }
}

#[test]
fn rows_total_counts_pass_rows_for_interlaced() {
    let b = build(&Spec::new(8, 8, 0, 8).interlaced());
    let img = lenient::decode(&b.png).unwrap();
    // 1+1+1+2+2+4+4 pass rows.
    assert_eq!(img.rows_total, 15);
    let b = build(&Spec::new(8, 8, 0, 8));
    assert_eq!(lenient::decode(&b.png).unwrap().rows_total, 8);
}

#[test]
fn truncation_recovers_proportional_rows_non_interlaced() {
    for (ct, depth) in [(2u8, 8u8), (6, 8), (0, 8), (3, 8), (2, 16)] {
        let mut s = Spec::new(200, 150, ct, depth).chunk(8192);
        s.seed = 21;
        let b = build(&s);
        let (lo, hi) = idat_span(&b.png);
        for pct in [25usize, 50, 75] {
            let cut = lo + (hi - lo) * pct / 100;
            let img = lenient::decode(&b.png[..cut]).unwrap();
            let frac = img.rows_recovered as f64 / img.rows_total as f64;
            let want = pct as f64 / 100.0;
            assert!(
                (frac - want).abs() < 0.12,
                "{ct}/{depth} at {pct}%: recovered {frac:.2}"
            );
            assert!(img.inflate_error.is_some());
            assert_eq!(img.dropped_rows, 0, "clean truncation drops nothing");
            // Every recovered row equals the original.
            let rb = s.row_bytes();
            assert_eq!(img.pixels.len(), img.rows_recovered as usize * rb);
            assert_eq!(&img.pixels[..], &b.raw[..img.pixels.len()]);
            // Mask: exactly the recovered prefix.
            let ok = img.recovered_mask_rows.iter().filter(|&&m| m).count();
            assert_eq!(ok, img.rows_recovered as usize);
        }
    }
}

#[test]
fn truncation_of_interlaced_images_keeps_complete_passes() {
    let s = Spec::new(160, 120, 2, 8).interlaced().chunk(4096);
    let b = build(&s);
    let (lo, hi) = idat_span(&b.png);
    let mut last = 0.0f32;
    for pct in [10usize, 25, 50, 75, 90] {
        let cut = lo + (hi - lo) * pct / 100;
        let img = lenient::decode(&b.png[..cut]).unwrap();
        assert_eq!(img.pixels.len(), b.raw.len());
        // Every pixel flagged known equals the original pixel.
        let rb = s.row_bytes();
        for y in 0..s.height {
            for x in 0..s.width {
                if img.pixel_known(x, y) {
                    let o = y as usize * rb + x as usize * 3;
                    assert_eq!(
                        &img.pixels[o..o + 3],
                        &b.raw[o..o + 3],
                        "px {x},{y} at {pct}%"
                    );
                }
            }
        }
        let f = img.intact_fraction();
        assert!(f >= last, "fraction must grow with the cut: {f} < {last}");
        assert!((f as f64 - pct as f64 / 100.0).abs() < 0.2, "{f} at {pct}%");
        last = f;
        // Pass 1 completes early: pixel (0,0) is known from 10% on.
        assert!(img.pixel_known(0, 0));
    }
}

#[test]
fn flipped_bytes_in_idat_recover_a_correct_prefix() {
    // Many (image, position) combinations: whatever is reported recovered
    // must equal the original decode, and a flip in the middle must not wipe
    // out the part before it.
    let mut checked = 0;
    let mut prefix_cases = 0;
    for (ct, depth, interlaced) in [
        (2u8, 8u8, false),
        (6, 8, false),
        (0, 8, false),
        (3, 8, false),
        (2, 16, false),
        (2, 8, true),
    ] {
        let mut s = Spec::new(220, 160, ct, depth).chunk(6000);
        s.interlaced = interlaced;
        s.seed = 99;
        let b = build(&s);
        let orig = png_decode(&b.png).unwrap();
        assert_eq!(orig.data, b.raw);
        let (lo, hi) = idat_span(&b.png);
        let rb = s.row_bytes();
        for pct in [20usize, 40, 60, 80] {
            for delta in 0..3usize {
                let pos = lo + (hi - lo) * pct / 100 + delta * 17;
                let mut bad = b.png.clone();
                bad[pos] ^= 0x5A;
                let img = lenient::decode(&bad).unwrap();
                assert!(img.inflate_error.is_some(), "flip must be noticed");
                prefix_cases += 1;
                if !interlaced {
                    let n = img.rows_recovered as usize * rb;
                    assert_eq!(img.pixels.len(), n);
                    assert_eq!(
                        &img.pixels[..],
                        &orig.data[..n],
                        "{ct}/{depth} flip at {pct}%+{delta}"
                    );
                    // Not everything lost: at least a third of the position.
                    let frac = img.rows_recovered as f64 / img.rows_total as f64;
                    assert!(
                        frac > pct as f64 / 100.0 * 0.5,
                        "{ct}/{depth}: {frac} at {pct}%"
                    );
                    assert!(frac <= 1.0);
                } else {
                    for y in 0..s.height {
                        for x in 0..s.width {
                            if img.pixel_known(x, y) {
                                let o = y as usize * rb + x as usize * 3;
                                assert_eq!(&img.pixels[o..o + 3], &orig.data[o..o + 3]);
                            }
                        }
                    }
                }
                checked += 1;
            }
        }
    }
    assert!(checked >= 60);
    assert!(prefix_cases >= 60);
}

#[test]
fn corruption_is_never_reported_as_recovered_rows() {
    // Replace a stretch of the compressed stream with noise of the same
    // size: a very different kind of damage from a single flip.
    let s = Spec::new(300, 200, 2, 8).chunk(10_000).seed(5);
    let b = build(&s);
    let orig = png_decode(&b.png).unwrap();
    let (lo, hi) = idat_span(&b.png);
    let rb = s.row_bytes();
    let mut rng = Rng::new(1234);
    for pct in [15usize, 35, 55, 75] {
        let at = lo + (hi - lo) * pct / 100;
        let mut bad = b.png.clone();
        let noise = rng.bytes(3000.min(hi - at));
        bad[at..at + noise.len()].copy_from_slice(&noise);
        let img = lenient::decode(&bad).unwrap();
        let n = img.rows_recovered as usize * rb;
        assert_eq!(&img.pixels[..], &orig.data[..n], "noise at {pct}%");
        assert!(img.rows_recovered < img.rows_total);
    }
}

#[test]
fn zlib_header_damage_is_worked_around() {
    let s = Spec::new(64, 40, 2, 8);
    let b = build(&s);
    let (lo, _) = idat_span(&b.png);
    let mut bad = b.png.clone();
    bad[lo] = 0xFF;
    bad[lo + 1] = 0xFF;
    // Fix the chunk CRC so only the zlib header is wrong.
    let c = find(&bad, b"IDAT", 0);
    fix_crc_at(&mut bad, &c);
    let img = lenient::decode(&bad).unwrap();
    assert!(!img.zlib_header_ok);
    assert!(img.inflate_error.as_ref().unwrap().contains("zlib header"));
    assert_eq!(img.rows_recovered, img.rows_total);
    assert_eq!(img.pixels, b.raw);
    assert!(!img.stream_complete);
}

#[test]
fn missing_adler_and_trailing_bytes() {
    let s = Spec::new(40, 30, 0, 8).chunk(0);
    let b = build(&s);
    let idat = find(&b.png, b"IDAT", 0);
    // Cut the last 4 bytes (Adler-32) off the IDAT payload and re-wrap.
    let mut z = b.png[idat.data()].to_vec();
    z.truncate(z.len() - 4);
    let mut png = b.png[..idat.offset].to_vec();
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.rows_recovered, img.rows_total);
    assert_eq!(img.pixels, b.raw);
    assert!(img.inflate_error.is_some());
    // Wrong Adler: all rows, error mentions the checksum.
    let mut z2 = b.png[idat.data()].to_vec();
    let n = z2.len();
    z2[n - 1] ^= 0x01;
    let mut png = b.png[..idat.offset].to_vec();
    png.extend(chunk(b"IDAT", &z2));
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.rows_recovered, img.rows_total);
    assert_eq!(img.pixels, b.raw);
    assert!(img.inflate_error.as_ref().unwrap().contains("Adler"));
    // Extra rows of data after the image.
    let spec2 = Spec::new(40, 30, 0, 8);
    let raw = gen_raw(&spec2);
    let mut stream = filtered_stream(&spec2, &raw);
    stream.extend_from_slice(&[0u8; 41 * 3]);
    let z3 = zlib(&stream, 6);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec2)));
    png.extend(chunk(b"IDAT", &z3));
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.rows_recovered, img.rows_total);
    assert_eq!(img.excess_bytes, 41 * 3);
    assert!(img.inflate_error.is_some());
}

#[test]
fn short_image_data_reports_rows() {
    // The zlib stream is complete but holds only part of the rows.
    let spec = Spec::new(40, 30, 0, 8);
    let raw = gen_raw(&spec);
    let stream = filtered_stream(&spec, &raw);
    let z = zlib(&stream[..stream.len() / 2], 6);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&spec)));
    png.extend(chunk(b"IDAT", &z));
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.rows_recovered, 15);
    assert!(img.inflate_error.as_ref().unwrap().contains("15 of 30"));
    assert_eq!(&img.pixels[..], &raw[..15 * 40]);
}

#[test]
fn bad_crc_chunks_do_not_stop_decoding() {
    let s = Spec::new(90, 70, 2, 8).chunk(1000);
    let b = build(&s);
    let mut bad = b.png.clone();
    for n in [0usize, 3, 5] {
        let c = find(&bad, b"IDAT", n);
        bad[c.crc_at()] ^= 0xFF;
    }
    let img = lenient::decode(&bad).unwrap();
    assert_eq!(img.rows_recovered, img.rows_total);
    assert_eq!(img.pixels, b.raw);
}

#[test]
fn idat_chunks_split_and_empty_chunks_are_fine() {
    let s = Spec::new(33, 21, 6, 8);
    let raw = gen_raw(&s);
    let z = zlib(&filtered_stream(&s, &raw), 6);
    let mut png = SIG.to_vec();
    png.extend(chunk(b"IHDR", &ihdr_bytes(&s)));
    for part in z.chunks(7) {
        png.extend(chunk(b"IDAT", part));
        png.extend(chunk(b"IDAT", &[]));
    }
    png.extend(chunk(b"IEND", &[]));
    let img = lenient::decode(&png).unwrap();
    assert_eq!(img.pixels, raw);
    assert!(img.inflate_error.is_none());
}

#[test]
fn unreadable_ranges_end_the_image_data_cleanly() {
    let s = Spec::new(200, 100, 2, 8).chunk(5000);
    let b = build(&s);
    let (lo, hi) = idat_span(&b.png);
    let hole_at = lo + (hi - lo) / 2;
    let mut holes = RangeSet::new();
    holes.insert(Range::new(hole_at as u64, hole_at as u64 + 2048));
    let src = MemSource::new(b.png.clone()).with_unreadable(holes);
    let walk = chunks::walk(&src, 0, src.size_hint()).unwrap();
    let img = lenient::decode_source(&src, &walk, &LenientOptions::default()).unwrap();
    assert!(img.rows_recovered > 0 && img.rows_recovered < img.rows_total);
    assert!(img.inflate_error.is_some());
    assert_eq!(img.dropped_rows, 0);
    let n = img.rows_recovered as usize * s.row_bytes();
    assert_eq!(&img.pixels[..], &b.raw[..n]);
    let frac = img.rows_recovered as f64 / img.rows_total as f64;
    assert!((frac - 0.5).abs() < 0.12, "{frac}");
}

trait SizeHint {
    fn size_hint(&self) -> u64;
}

impl SizeHint for MemSource {
    fn size_hint(&self) -> u64 {
        diskrescue_core::Source::size(self)
    }
}

#[test]
fn analyze_keeps_no_pixels() {
    let s = Spec::new(50, 50, 2, 8);
    let b = build(&s);
    let src = MemSource::new(b.png.clone());
    let walk = chunks::walk(&src, 0, u64::MAX).unwrap();
    let img = lenient::analyze(&src, &walk).unwrap();
    assert!(img.pixels.is_empty());
    assert!(img.recovered_mask_rows.is_empty());
    assert_eq!(img.rows_recovered, 50);
}

#[test]
fn pixel_limit_is_enforced() {
    let s = Spec::new(100, 100, 6, 8);
    let b = build(&s);
    let opts = LenientOptions {
        keep_pixels: true,
        max_pixel_bytes: 1000,
    };
    assert!(lenient::decode_with(&b.png, &opts).is_err());
}

#[test]
fn crc_evidence_caps_rows_even_when_inflate_survives() {
    // A flipped bit in a multi-chunk image: whatever the inflater makes of
    // the rest, rows are only vouched for up to the damaged chunk.
    let s = Spec::new(220, 160, 0, 8).chunk(3000).seed(5);
    let b = build(&s);
    let orig = png_decode(&b.png).unwrap();
    let rb = s.row_bytes();
    let chunks: Vec<_> = chunks_of(&b.png)
        .into_iter()
        .filter(|c| &c.kind == b"IDAT")
        .collect();
    let mut capped = 0;
    for k in 1..chunks.len() - 1 {
        for off in [3usize, 500, 1500] {
            let mut bad = b.png.clone();
            bad[chunks[k].data().start + off] ^= 0x10;
            let img = lenient::decode(&bad).unwrap();
            let n = img.rows_recovered as usize * rb;
            assert_eq!(&img.pixels[..], &orig.data[..n], "chunk {k} offset {off}");
            // Most of what precedes the damaged chunk is kept (rows are
            // roughly proportional to compressed bytes for this image).
            let before: usize = chunks[..k].iter().map(|c| c.len).sum();
            let total: usize = chunks.iter().map(|c| c.len).sum();
            let expect = 160.0 * before as f64 / total as f64;
            assert!(
                img.rows_recovered as f64 >= expect * 0.6 - 3.0,
                "chunk {k}: recovered {} of ~{expect:.0}",
                img.rows_recovered
            );
            if img.crc_capped {
                capped += 1;
            }
        }
    }
    assert!(capped > 10);
}

#[test]
fn single_chunk_images_fall_back_to_inflate_evidence() {
    // With one IDAT chunk the CRC cannot localise damage; the inflater's
    // error position is used instead and a bad Adler-32 keeps all rows.
    let s = Spec::new(220, 160, 2, 8).chunk(0).seed(8);
    let b = build(&s);
    let (lo, hi) = idat_span(&b.png);
    let mut seen_partial = 0;
    for pct in [20usize, 40, 60, 80] {
        for delta in 0..4usize {
            let mut bad = b.png.clone();
            bad[lo + (hi - lo) * pct / 100 + delta * 31] ^= 0x44;
            let img = lenient::decode(&bad).unwrap();
            assert!(img.inflate_error.is_some());
            assert!(!img.crc_capped);
            if img.rows_recovered < img.rows_total {
                seen_partial += 1;
            }
        }
    }
    assert!(seen_partial > 0);
}
