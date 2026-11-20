//! Ken Perlin's "Improved Noise" (SIGGRAPH 2002).
//!
//! Gradient noise: random gradient vectors sit on an integer lattice and the
//! value at any point is a smooth blend of their dot products with the offset
//! to the point. Output is roughly in `[-1, 1]` and is exactly 0 on lattice
//! points, which is why every instance also gets a random sub-block offset.
//!
//! This is the same building block Minecraft's terrain uses. We implement it
//! from the published algorithm with a seeded permutation table.

use crate::rng::Xoroshiro128PlusPlus;

#[derive(Clone)]
pub struct ImprovedPerlin {
    perm: [u8; 512],
    /// Random offset so that lattice points (where noise is 0) don't line up
    /// across octaves or with the world origin.
    origin: [f64; 3],
}

impl ImprovedPerlin {
    pub fn new(rng: &mut Xoroshiro128PlusPlus) -> Self {
        let origin = [rng.next_f64() * 256.0, rng.next_f64() * 256.0, rng.next_f64() * 256.0];
        let mut p = [0u8; 256];
        for (i, v) in p.iter_mut().enumerate() {
            *v = i as u8;
        }
        // Fisher-Yates shuffle.
        for i in (1..256).rev() {
            let j = rng.next_bounded(i as u32 + 1) as usize;
            p.swap(i, j);
        }
        let mut perm = [0u8; 512];
        for i in 0..512 {
            perm[i] = p[i & 255];
        }
        Self { perm, origin }
    }

    /// 3D noise, approximately in `[-1, 1]`.
    #[inline]
    pub fn sample(&self, x: f64, y: f64, z: f64) -> f64 {
        let x = x + self.origin[0];
        let y = y + self.origin[1];
        let z = z + self.origin[2];
        let (xf, yf, zf) = (x.floor(), y.floor(), z.floor());
        let xi = (xf as i64 & 255) as usize;
        let yi = (yf as i64 & 255) as usize;
        let zi = (zf as i64 & 255) as usize;
        let (x, y, z) = (x - xf, y - yf, z - zf);
        let (u, v, w) = (fade(x), fade(y), fade(z));

        let p = &self.perm;
        let a = p[xi] as usize + yi;
        let aa = p[a] as usize + zi;
        let ab = p[a + 1] as usize + zi;
        let b = p[xi + 1] as usize + yi;
        let ba = p[b] as usize + zi;
        let bb = p[b + 1] as usize + zi;

        lerp(
            w,
            lerp(
                v,
                lerp(u, grad(p[aa], x, y, z), grad(p[ba], x - 1.0, y, z)),
                lerp(u, grad(p[ab], x, y - 1.0, z), grad(p[bb], x - 1.0, y - 1.0, z)),
            ),
            lerp(
                v,
                lerp(u, grad(p[aa + 1], x, y, z - 1.0), grad(p[ba + 1], x - 1.0, y, z - 1.0)),
                lerp(u, grad(p[ab + 1], x, y - 1.0, z - 1.0), grad(p[bb + 1], x - 1.0, y - 1.0, z - 1.0)),
            ),
        )
    }
}

/// 6t^5 - 15t^4 + 10t^3: zero first and second derivative at 0 and 1, which
/// removes the visible grid artifacts of Perlin's original 1985 cubic.
#[inline]
fn fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn lerp(t: f64, a: f64, b: f64) -> f64 {
    a + t * (b - a)
}

/// Dot product with one of 12 cube-edge gradients (4 repeated to fill 16).
#[inline]
fn grad(hash: u8, x: f64, y: f64, z: f64) -> f64 {
    match hash & 15 {
        0 => x + y,
        1 => -x + y,
        2 => x - y,
        3 => -x - y,
        4 => x + z,
        5 => -x + z,
        6 => x - z,
        7 => -x - z,
        8 => y + z,
        9 => -y + z,
        10 => y - z,
        11 => -y - z,
        12 => x + y,
        13 => -x + y,
        14 => -y + z,
        _ => -y - z,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: u64) -> ImprovedPerlin {
        ImprovedPerlin::new(&mut Xoroshiro128PlusPlus::from_seed(seed))
    }

    #[test]
    fn bounded_and_not_constant() {
        let n = noise(1);
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for i in 0..20_000 {
            let f = i as f64 * 0.137;
            let v = n.sample(f, f * 0.31, f * 0.77);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        assert!(lo >= -1.05 && hi <= 1.05, "range {lo}..{hi}");
        assert!(hi - lo > 0.8, "noise too flat: {lo}..{hi}");
    }

    #[test]
    fn continuous() {
        let n = noise(2);
        for i in 0..1000 {
            let x = i as f64 * 0.731;
            let d = (n.sample(x, 1.5, 2.5) - n.sample(x + 1e-6, 1.5, 2.5)).abs();
            assert!(d < 1e-4);
        }
    }

    #[test]
    fn seeds_give_different_noise() {
        let (a, b) = (noise(3), noise(4));
        let diff: f64 =
            (0..100).map(|i| (a.sample(i as f64 * 0.5, 0.0, 0.0) - b.sample(i as f64 * 0.5, 0.0, 0.0)).abs()).sum();
        assert!(diff > 1.0);
    }
}
