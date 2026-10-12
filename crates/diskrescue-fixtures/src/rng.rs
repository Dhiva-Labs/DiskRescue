//! Small deterministic PRNG so fixtures are byte-identical across runs,
//! platforms and `rand` versions (xoshiro256** seeded through splitmix64).

/// Deterministic generator.
#[derive(Debug, Clone)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Seed the generator. Equal seeds give equal streams forever.
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Rng {
            s: [next(), next(), next(), next()],
        }
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform value in `0..n` (`0` when `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Rejection sampling keeps it unbiased; the loop ends quickly.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }

    /// Uniform `usize` in `0..n`.
    pub fn below_usize(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }

    /// Uniform `f32` in `[0, 1)`.
    pub fn f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Fill a buffer with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let v = self.next_u64().to_le_bytes();
            rem.copy_from_slice(&v[..rem.len()]);
        }
    }

    /// A fresh vector of `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }
}

/// Stateless 64-bit hash of a few integers, used for per-pixel noise so the
/// canvas generator never has to keep a stream position.
pub fn hash3(a: u64, b: u64, c: u64) -> u64 {
    let mut z = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .wrapping_add(c.wrapping_mul(0x1656_67B1_9E37_79F9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_seed_sensitive() {
        let a: Vec<u64> = (0..8).map(|_| 0).collect::<Vec<_>>();
        let mut r1 = Rng::new(7);
        let mut r2 = Rng::new(7);
        let mut r3 = Rng::new(8);
        let v1: Vec<u64> = a.iter().map(|_| r1.next_u64()).collect();
        let v2: Vec<u64> = a.iter().map(|_| r2.next_u64()).collect();
        let v3: Vec<u64> = a.iter().map(|_| r3.next_u64()).collect();
        assert_eq!(v1, v2);
        assert_ne!(v1, v3);
        assert_ne!(hash3(1, 2, 3), hash3(1, 2, 4));
        assert_eq!(hash3(1, 2, 3), hash3(1, 2, 3));
    }

    #[test]
    fn below_is_in_range() {
        let mut r = Rng::new(3);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
            assert!(r.f32() < 1.0);
        }
        assert_eq!(r.below(0), 0);
        let mut b = [0u8; 13];
        r.fill(&mut b);
        assert!(b.iter().any(|&x| x != 0));
    }
}
