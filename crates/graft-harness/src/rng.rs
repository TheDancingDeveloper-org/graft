//! A small seeded generator and a stable hash, so every run is a function
//! of its seed.

/// SplitMix64: small, fast, and good enough to drive a test.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }

    /// Uniform in `0..n` (`n` > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        unit(self.next_u64()) < p
    }

    /// A generator split off this one.
    pub fn fork(&mut self) -> Rng {
        Rng(self.next_u64())
    }
}

/// The SplitMix64 finaliser.
pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `bits` as a fraction in `[0, 1)`.
pub fn unit(bits: u64) -> f64 {
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// FNV-1a over `bytes`: stable across runs, toolchains and platforms.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_from_the_seed() {
        let (mut a, mut b) = (Rng::new(9), Rng::new(9));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert!((0..1000).all(|_| a.below(7) < 7));
    }
}
