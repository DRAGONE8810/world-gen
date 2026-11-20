//! Deterministic random numbers.
//!
//! World generation must give *exactly* the same world for the same seed on
//! every machine and every run. Rust's `rand` crate makes no promise that its
//! algorithms stay the same between versions, so we ship our own small,
//! well-known generators:
//!
//! * [`SplitMix64`] – turns any 64-bit number into a well-mixed stream. Used to
//!   expand one world seed into many independent sub-seeds.
//! * [`Xoroshiro128PlusPlus`] – fast, high-quality generator (the same family
//!   modern Minecraft uses). Used to build noise permutation tables.
//! * [`hash_position`] – a stateless hash of `(seed, x, y, z)`. Lets any thread
//!   ask "what is the random value at this block?" without generating
//!   anything else first, which is what makes chunk generation order-free and
//!   parallel.

/// SplitMix64 by Sebastiano Vigna. One multiply-xorshift round per output.
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix64(self.state)
    }
}

/// The SplitMix64 finaliser: a bijective "avalanche" function on u64.
#[inline]
pub const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Xoroshiro128++ (Blackman & Vigna, 2019).
#[derive(Clone, Debug)]
pub struct Xoroshiro128PlusPlus {
    lo: u64,
    hi: u64,
}

impl Xoroshiro128PlusPlus {
    /// Seeds the 128-bit state from a 64-bit seed via SplitMix64, as the
    /// authors recommend (an all-zero state would be invalid).
    pub fn from_seed(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        let lo = sm.next_u64();
        let hi = sm.next_u64();
        if lo == 0 && hi == 0 {
            Self { lo: 0x9E37_79B9_7F4A_7C15, hi: 0x6A09_E667_F3BC_C909 }
        } else {
            Self { lo, hi }
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let (s0, mut s1) = (self.lo, self.hi);
        let result = s0.wrapping_add(s1).rotate_left(17).wrapping_add(s0);
        s1 ^= s0;
        self.lo = s0.rotate_left(49) ^ s1 ^ (s1 << 21);
        self.hi = s1.rotate_left(28);
        result
    }

    /// Uniform `f64` in `[0, 1)` using the top 53 bits.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform integer in `[0, bound)` without modulo bias (Lemire's method).
    #[inline]
    pub fn next_bounded(&mut self, bound: u32) -> u32 {
        debug_assert!(bound > 0);
        let mut m = (self.next_u64() as u32 as u64) * bound as u64;
        let mut low = m as u32;
        if low < bound {
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                m = (self.next_u64() as u32 as u64) * bound as u64;
                low = m as u32;
            }
        }
        (m >> 32) as u32
    }
}

/// Derives an independent seed for one purpose ("continentalness noise",
/// "tree placement", ...) from the world seed. Different `salt` strings give
/// statistically unrelated streams, so adding a new feature never changes the
/// output of existing ones.
pub fn derive_seed(world_seed: u64, salt: &str) -> u64 {
    // FNV-1a over the salt, then mixed with the world seed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in salt.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    mix64(world_seed ^ mix64(h))
}

/// Stateless positional hash: a random-looking u64 for a block position.
#[inline]
pub fn hash_position(seed: u64, x: i32, y: i32, z: i32) -> u64 {
    let mut h = seed;
    h = mix64(h ^ (x as u32 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    h = mix64(h ^ (y as u32 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    mix64(h ^ (z as u32 as u64).wrapping_mul(0x1656_67B1_9E37_79F9))
}

/// Same as [`hash_position`] but mapped to `[0, 1)`.
#[inline]
pub fn hash_position_f64(seed: u64, x: i32, y: i32, z: i32) -> f64 {
    (hash_position(seed, x, y, z) >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_reference_values() {
        // Reference output for seed 1234567 from Vigna's C implementation.
        let mut s = SplitMix64::new(1234567);
        assert_eq!(s.next_u64(), 6457827717110365317);
        assert_eq!(s.next_u64(), 3203168211198807973);
    }

    #[test]
    fn xoroshiro_is_deterministic() {
        let mut a = Xoroshiro128PlusPlus::from_seed(42);
        let mut b = Xoroshiro128PlusPlus::from_seed(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn f64_in_unit_range_and_bounded_is_fair() {
        let mut r = Xoroshiro128PlusPlus::from_seed(7);
        let mut buckets = [0u32; 10];
        for _ in 0..100_000 {
            let f = r.next_f64();
            assert!((0.0..1.0).contains(&f));
            buckets[r.next_bounded(10) as usize] += 1;
        }
        for b in buckets {
            assert!((9_000..11_000).contains(&b), "bucket {b} far from 10000");
        }
    }

    #[test]
    fn derived_seeds_differ() {
        assert_ne!(derive_seed(1, "a"), derive_seed(1, "b"));
        assert_ne!(derive_seed(1, "a"), derive_seed(2, "a"));
    }

    #[test]
    fn position_hash_changes_with_every_axis() {
        let h = hash_position(9, 1, 2, 3);
        assert_ne!(h, hash_position(9, 2, 2, 3));
        assert_ne!(h, hash_position(9, 1, 3, 3));
        assert_ne!(h, hash_position(9, 1, 2, 4));
        assert_ne!(h, hash_position(10, 1, 2, 3));
    }
}
