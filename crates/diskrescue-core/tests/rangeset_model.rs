//! Randomised check of RangeSet against a per-byte boolean model.

use diskrescue_core::ranges::{Range, RangeSet};

const N: u64 = 256;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn range(&mut self) -> Range {
        let a = self.next() % (N + 1);
        let b = self.next() % (N + 1);
        Range::new(a.min(b), a.max(b))
    }
}

fn model_of(s: &RangeSet) -> Vec<bool> {
    let mut m = vec![false; N as usize];
    for r in s.iter() {
        for i in r.start..r.end {
            m[i as usize] = true;
        }
    }
    m
}

fn check_invariants(s: &RangeSet) {
    let v: Vec<Range> = s.iter().collect();
    for r in &v {
        assert!(!r.is_empty(), "empty range stored: {v:?}");
    }
    for w in v.windows(2) {
        assert!(
            w[0].end < w[1].start,
            "overlapping or adjacent ranges: {v:?}"
        );
    }
}

fn random_set(rng: &mut Lcg) -> (RangeSet, Vec<bool>) {
    let mut s = RangeSet::new();
    let mut m = vec![false; N as usize];
    for _ in 0..(rng.next() % 8) {
        let r = rng.range();
        if rng.next() % 3 == 0 {
            s.remove(r);
            for i in r.start..r.end {
                m[i as usize] = false;
            }
        } else {
            s.insert(r);
            for i in r.start..r.end {
                m[i as usize] = true;
            }
        }
        check_invariants(&s);
        assert_eq!(model_of(&s), m);
    }
    (s, m)
}

#[test]
fn rangeset_matches_bitmap_model() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..20_000 {
        let (a, ma) = random_set(&mut rng);
        let (b, mb) = random_set(&mut rng);
        let w = rng.range();

        let inter = a.intersection(&b);
        check_invariants(&inter);
        let exp: Vec<bool> = ma.iter().zip(&mb).map(|(x, y)| *x && *y).collect();
        assert_eq!(model_of(&inter), exp, "intersection {a:?} {b:?}");

        let uni = a.union(&b);
        check_invariants(&uni);
        let exp: Vec<bool> = ma.iter().zip(&mb).map(|(x, y)| *x || *y).collect();
        assert_eq!(model_of(&uni), exp);

        let iw = a.intersection_with_range(w);
        check_invariants(&iw);
        let exp: Vec<bool> = (0..N).map(|i| ma[i as usize] && w.contains(i)).collect();
        assert_eq!(model_of(&iw), exp);

        let comp = a.complement_within(w);
        check_invariants(&comp);
        let exp: Vec<bool> = (0..N).map(|i| !ma[i as usize] && w.contains(i)).collect();
        assert_eq!(model_of(&comp), exp);

        let total: u64 = ma.iter().filter(|x| **x).count() as u64;
        assert_eq!(a.total_len(), total);

        let covers_exp = (w.start..w.end).all(|i| ma[i as usize]);
        assert_eq!(a.covers(w), covers_exp, "covers {a:?} {w:?}");
        let inter_exp = (w.start..w.end).any(|i| ma[i as usize]);
        assert_eq!(a.intersects(w), inter_exp, "intersects {a:?} {w:?}");
        for p in [w.start, w.end.saturating_sub(1)] {
            if p < N {
                assert_eq!(a.contains(p), ma[p as usize]);
            }
        }
        let d = rng.next() % 64;
        let down = a.shifted_down(d);
        check_invariants(&down);
        let exp: Vec<bool> = (0..N).map(|i| i + d < N && ma[(i + d) as usize]).collect();
        assert_eq!(model_of(&down), exp);
    }
}
