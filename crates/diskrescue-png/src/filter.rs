//! PNG scanline filters (None, Sub, Up, Average, Paeth), forward and inverse.
//!
//! All arithmetic is modulo 256. `bpp` is the filter distance in bytes
//! (`max(1, bits_per_pixel / 8)`). Rows passed to these functions never
//! include the leading filter-type byte.

/// The Paeth predictor.
#[inline]
pub(crate) fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ai, bi, ci) = (a as i16, b as i16, c as i16);
    let p = ai + bi - ci;
    let pa = (p - ai).abs();
    let pb = (p - bi).abs();
    let pc = (p - ci).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Undo filter `ftype` for one row. `cur` is the filtered row, `prev` the
/// previous *unfiltered* row (zeros for the first row of a pass) and `out`
/// receives the unfiltered row. All three have the same length. Returns
/// `false` (leaving `out` unspecified) for an unknown filter type.
pub(crate) fn unfilter(ftype: u8, cur: &[u8], prev: &[u8], out: &mut [u8], bpp: usize) -> bool {
    let n = cur.len().min(prev.len()).min(out.len());
    match ftype {
        0 => out[..n].copy_from_slice(&cur[..n]),
        1 => {
            for i in 0..n {
                let a = if i >= bpp { out[i - bpp] } else { 0 };
                out[i] = cur[i].wrapping_add(a);
            }
        }
        2 => {
            for i in 0..n {
                out[i] = cur[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..n {
                let a = if i >= bpp { out[i - bpp] as u16 } else { 0 };
                let b = prev[i] as u16;
                out[i] = cur[i].wrapping_add(((a + b) / 2) as u8);
            }
        }
        4 => {
            for i in 0..n {
                let (a, c) = if i >= bpp {
                    (out[i - bpp], prev[i - bpp])
                } else {
                    (0, 0)
                };
                out[i] = cur[i].wrapping_add(paeth(a, prev[i], c));
            }
        }
        _ => return false,
    }
    true
}

/// Apply filter `ftype` to one unfiltered row. `raw` is the row, `prev` the
/// previous unfiltered row (zeros for the first row of a pass), `out`
/// receives the filtered bytes.
pub(crate) fn filter(ftype: u8, raw: &[u8], prev: &[u8], out: &mut [u8], bpp: usize) {
    let n = raw.len().min(prev.len()).min(out.len());
    match ftype {
        1 => {
            for i in 0..n {
                let a = if i >= bpp { raw[i - bpp] } else { 0 };
                out[i] = raw[i].wrapping_sub(a);
            }
        }
        2 => {
            for i in 0..n {
                out[i] = raw[i].wrapping_sub(prev[i]);
            }
        }
        3 => {
            for i in 0..n {
                let a = if i >= bpp { raw[i - bpp] as u16 } else { 0 };
                let b = prev[i] as u16;
                out[i] = raw[i].wrapping_sub(((a + b) / 2) as u8);
            }
        }
        4 => {
            for i in 0..n {
                let (a, c) = if i >= bpp {
                    (raw[i - bpp], prev[i - bpp])
                } else {
                    (0, 0)
                };
                out[i] = raw[i].wrapping_sub(paeth(a, prev[i], c));
            }
        }
        _ => out[..n].copy_from_slice(&raw[..n]),
    }
}

/// Pick the filter that minimises the sum of absolute signed residuals (the
/// usual libpng heuristic) and return `(filter type, filtered row)` in
/// `best`. `scratch` is a reusable buffer of the same length as the row.
pub(crate) fn filter_adaptive(
    raw: &[u8],
    prev: &[u8],
    bpp: usize,
    scratch: &mut Vec<u8>,
    best: &mut Vec<u8>,
) -> u8 {
    let n = raw.len();
    scratch.resize(n, 0);
    best.resize(n, 0);
    let mut best_type = 0u8;
    let mut best_score = u64::MAX;
    for ftype in 0..=4u8 {
        filter(ftype, raw, prev, scratch, bpp);
        let score: u64 = scratch
            .iter()
            .map(|&b| (b as i8).unsigned_abs() as u64)
            .sum();
        if score < best_score {
            best_score = score;
            best_type = ftype;
            std::mem::swap(scratch, best);
            scratch.resize(n, 0);
        }
    }
    best_type
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(len: usize, seed: u32) -> Vec<u8> {
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 8) as u8
            })
            .collect()
    }

    #[test]
    fn filter_unfilter_roundtrip_all_types() {
        for bpp in [1usize, 2, 3, 4, 6, 8] {
            let raw = noisy(97, bpp as u32);
            let prev = noisy(97, 100 + bpp as u32);
            for ftype in 0..=4u8 {
                let mut f = vec![0u8; raw.len()];
                filter(ftype, &raw, &prev, &mut f, bpp);
                let mut back = vec![0u8; raw.len()];
                assert!(unfilter(ftype, &f, &prev, &mut back, bpp));
                assert_eq!(back, raw, "bpp {bpp} filter {ftype}");
            }
        }
        let mut out = [0u8; 4];
        assert!(!unfilter(5, &[0; 4], &[0; 4], &mut out, 1));
    }

    #[test]
    fn paeth_matches_spec_examples() {
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(20, 10, 10), 20);
        assert_eq!(paeth(10, 10, 20), 10);
    }

    #[test]
    fn adaptive_picks_a_decodable_filter() {
        let raw: Vec<u8> = (0..120).map(|i| (i * 2) as u8).collect();
        let prev = vec![0u8; 120];
        let mut scratch = Vec::new();
        let mut best = Vec::new();
        let t = filter_adaptive(&raw, &prev, 3, &mut scratch, &mut best);
        // A linear ramp is best handled by Sub (constant residual).
        assert_ne!(t, 0);
        let mut back = vec![0u8; 120];
        assert!(unfilter(t, &best, &prev, &mut back, 3));
        assert_eq!(back, raw);
    }
}
