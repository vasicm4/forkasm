//! Deterministic RNG for `random_get`: xoshiro256** seeded through splitmix64.

/// Cloned with the sandbox, so a branch's random stream replays identically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng([u64; 4]);

impl Default for Rng {
    fn default() -> Self {
        Rng::new(0)
    }
}

fn splitmix64(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    /// A stream seeded with `seed`.
    pub fn new(seed: u64) -> Rng {
        let mut x = seed;
        Rng([splitmix64(&mut x), splitmix64(&mut x), splitmix64(&mut x), splitmix64(&mut x)])
    }

    /// A child stream: deterministic in (this stream's state, `branch`).
    pub fn fork(&self, branch: u64) -> Rng {
        let mut x = self.0.iter().fold(branch, |h, &w| splitmix64(&mut (h ^ w)));
        Rng::new(splitmix64(&mut x))
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.0;
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

    /// Fills `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_forks_differ() {
        let a = Rng::new(1);
        let (mut x, mut y) = (a.clone(), a.clone());
        assert_eq!(x.next_u64(), y.next_u64());
        let (mut c0, mut c0b, mut c1) = (a.fork(0), a.fork(0), a.fork(1));
        assert_eq!(c0.next_u64(), c0b.next_u64());
        assert_ne!(c0.next_u64(), c1.next_u64());
    }
}
