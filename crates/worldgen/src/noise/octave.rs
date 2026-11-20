//! Fractal noise built from several octaves of [`ImprovedPerlin`].
//!
//! One octave of Perlin noise has a single "feature size". Real terrain has
//! detail at every scale: continents, mountain ranges, hills, bumps. Fractal
//! Brownian motion (fBm) sums octaves where each one has double the frequency
//! (half the feature size) and a smaller amplitude than the previous.

use super::perlin::ImprovedPerlin;
use crate::rng::{derive_seed, Xoroshiro128PlusPlus};

/// Coordinates are wrapped into a ±2^25 window before sampling. Perlin noise
/// only needs the fractional part and the low 8 bits of the integer part, so
/// wrapping by a multiple of 256 changes nothing visually while keeping `f64`
/// precision perfect even at the ±30,000,000 world border. (Without this, very
/// high octave frequencies × huge coordinates lose precision, which is the
/// root cause of classic "Far Lands" style glitches.)
const WRAP: f64 = 33_554_432.0; // 2^25, a multiple of 256

#[inline]
pub fn wrap(v: f64) -> f64 {
    v - (v / WRAP + 0.5).floor() * WRAP
}

/// Parameters describing an fBm noise.
#[derive(Clone, Debug)]
pub struct OctaveConfig {
    /// Frequency of the lowest octave is `2^first_octave` cycles per block.
    /// E.g. `-10` means features around 1024 blocks wide.
    pub first_octave: i32,
    /// Relative amplitude of each octave, starting at `first_octave`.
    /// A 0.0 entry skips that octave entirely (cheap way to shape the spectrum).
    pub amplitudes: Vec<f64>,
}

impl OctaveConfig {
    pub fn new(first_octave: i32, amplitudes: &[f64]) -> Self {
        Self { first_octave, amplitudes: amplitudes.to_vec() }
    }
}

#[derive(Clone)]
pub struct OctaveNoise {
    octaves: Vec<(ImprovedPerlin, f64 /* amplitude */, f64 /* frequency */)>,
    /// Scales the sum so the result stays roughly within [-1, 1].
    normaliser: f64,
}

impl OctaveNoise {
    pub fn new(seed: u64, cfg: &OctaveConfig) -> Self {
        let mut octaves = Vec::new();
        let n = cfg.amplitudes.len() as i32;
        let mut amp_sum = 0.0;
        for (i, &amp) in cfg.amplitudes.iter().enumerate() {
            if amp == 0.0 {
                continue;
            }
            // Each octave gets its own independent permutation table.
            let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, &format!("octave{i}")));
            let freq = 2f64.powi(cfg.first_octave + i as i32);
            // Higher octaves contribute half as much as the previous one
            // (persistence 0.5), times the configured relative amplitude.
            let weight = amp * 2f64.powi(n - 1 - i as i32) / (2f64.powi(n) - 1.0);
            amp_sum += weight;
            octaves.push((ImprovedPerlin::new(&mut rng), weight, freq));
        }
        let normaliser = if amp_sum > 0.0 { 1.0 / amp_sum } else { 0.0 };
        Self { octaves, normaliser }
    }

    #[inline]
    pub fn sample(&self, x: f64, y: f64, z: f64) -> f64 {
        let mut sum = 0.0;
        for (noise, weight, freq) in &self.octaves {
            sum += weight * noise.sample(wrap(x * freq), wrap(y * freq), wrap(z * freq));
        }
        sum * self.normaliser
    }

    #[inline]
    pub fn sample_2d(&self, x: f64, z: f64) -> f64 {
        self.sample(x, 0.0, z)
    }
}

/// Two independent [`OctaveNoise`] instances sampled at slightly different
/// scales and averaged. A single Perlin field has faint axis-aligned
/// structure; adding a second one at a non-integer scale ratio cancels it and
/// gives a value distribution closer to a bell curve. Minecraft uses the same
/// idea for its climate maps; we use it for every large-scale "climate"
/// parameter (continentalness, erosion, temperature, ...).
#[derive(Clone)]
pub struct DoubleNoise {
    a: OctaveNoise,
    b: OctaveNoise,
}

impl DoubleNoise {
    /// Irrational-ish ratio so the two fields never line up.
    const SCALE: f64 = 1.018_126_888_217_522_7;

    pub fn new(seed: u64, cfg: &OctaveConfig) -> Self {
        Self {
            a: OctaveNoise::new(derive_seed(seed, "double/a"), cfg),
            b: OctaveNoise::new(derive_seed(seed, "double/b"), cfg),
        }
    }

    #[inline]
    pub fn sample(&self, x: f64, y: f64, z: f64) -> f64 {
        let s = Self::SCALE;
        // sqrt(2) restores the spread lost by averaging two fields.
        (self.a.sample(x, y, z) + self.b.sample(x * s, y * s, z * s)) * std::f64::consts::FRAC_1_SQRT_2
    }

    #[inline]
    pub fn sample_2d(&self, x: f64, z: f64) -> f64 {
        self.sample(x, 0.0, z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_fraction() {
        for v in [0.25, 123.75, -9_999_999.5, 29_999_999.125] {
            let w = wrap(v);
            assert!(w.abs() <= WRAP / 2.0);
            assert!(((w - v) / 256.0).fract().abs() < 1e-9, "{v} -> {w}");
        }
    }

    #[test]
    fn octave_noise_is_finite_at_world_border() {
        let n = OctaveNoise::new(1, &OctaveConfig::new(-8, &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]));
        for x in [-30_000_000.0, -29_999_999.5, 0.0, 29_999_999.5, 30_000_000.0] {
            let v = n.sample_2d(x, x * 0.5);
            assert!(v.is_finite() && v.abs() <= 1.5);
        }
    }

    #[test]
    fn detail_is_preserved_far_from_origin() {
        // Neighbouring blocks near the border must still differ smoothly,
        // i.e. no precision "stair-stepping".
        let n = OctaveNoise::new(5, &OctaveConfig::new(-4, &[1.0, 1.0, 1.0]));
        let base = 29_999_000.0;
        let mut distinct = std::collections::HashSet::new();
        for i in 0..64 {
            distinct.insert((n.sample_2d(base + i as f64, base) * 1e9) as i64);
        }
        assert!(distinct.len() > 60);
    }

    #[test]
    fn double_noise_spread() {
        let n = DoubleNoise::new(9, &OctaveConfig::new(-7, &[1.0, 1.0, 1.0, 1.0]));
        let vals: Vec<f64> = (0..10_000).map(|i| n.sample_2d(i as f64 * 13.7, i as f64 * 7.3)).collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
        assert!(mean.abs() < 0.1, "mean {mean}");
        assert!(var.sqrt() > 0.1, "std {}", var.sqrt());
    }
}
