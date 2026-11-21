//! Terrain shape: climate parameters → height, roughness and rivers.
//!
//! This is the layered model that modern Minecraft popularised, built
//! from our own curves and noise settings. Each column (x, z) gets a few
//! large-scale noise values, the "climate parameters":
//!
//! | Parameter | Low values | High values |
//! |---|---|---|
//! | **continentalness** `c` | deep ocean | far inland |
//! | **erosion** `e` | rugged, mountainous | flat, worn down |
//! | **weirdness** `w` → **peaks & valleys** `pv` | valleys (pv < 0) | ridges, peaks (pv > 0) |
//!
//! Spline [`Curve`]s turn those into:
//!
//! * `height`: the 2D base surface height in blocks,
//! * `roughness`: amplitude in blocks of the 3D noise added on top (that
//!   noise makes cliffs, overhangs and arches; see `generator.rs`),
//! * `river`: how strongly a river channel is carved here (0..1).
//!
//! Because the parameters are independent, every combination occurs
//! somewhere: flat inland plains (high `c`, high `e`), coastal mountains
//! (low `c`, low `e`, high `pv`), eroded badlands-style plateaus, and so on.
//! Two more parameters, **temperature** and **humidity**, don't change the
//! shape at all. Together with the four above they pick the biome (see
//! `biome.rs`), so biomes automatically agree with the land they sit on.
//!
//! Finally, a rare **island** noise raises cone-shaped oceanic islands out of
//! the deep ocean (think Hawaii or the Azores). They carry the volcanic and
//! mushroom-field biomes.

use crate::noise::{DoubleNoise, OctaveConfig};
use crate::rng::derive_seed;
use crate::settings::{WorldType, SEA_LEVEL};
use crate::spline::{lerp, smoothstep, Curve};

/// Climate parameters of one column, each roughly in [-1, 1].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Climate {
    pub continentalness: f64,
    pub erosion: f64,
    pub weirdness: f64,
    /// "Peaks and valleys": weirdness folded so that both large positive and
    /// large negative weirdness give peaks, and |weirdness| ≈ 2/3 gives
    /// valleys. `pv = 1 − |3|w| − 2|`.
    pub peaks_valleys: f64,
    /// Sea-level temperature, −1 (polar) … +1 (tropical). The biome code
    /// cools it further with altitude.
    pub temperature: f64,
    /// −1 (arid) … +1 (very wet).
    pub humidity: f64,
}

/// Output of the shaping stage for one column.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shape {
    pub climate: Climate,
    /// Base surface height (absolute y, before 3D noise).
    pub height: f64,
    /// Amplitude (blocks) of the 3D density noise. ~1–2 in lowlands, up to
    /// ~13 in rugged mountains (×1.6 when Amplified).
    pub roughness: f64,
    /// River channel strength 0..1 (already applied to `height`).
    pub river: f64,
    /// Oceanic island strength 0..1 (0 almost everywhere).
    pub island: f64,
    /// Terrain magnification of the world (`WorldHeight::terrain_scale`):
    /// 1 for 384-block worlds. Altitude rules (snow line, peaks, lapse
    /// rate) divide heights above sea level by it. 0 (the `Default`) is
    /// treated as 1.
    pub vscale: f64,
}

impl Shape {
    /// The terrain scale, with the `Default` 0 read as 1.
    #[inline]
    pub fn scale(&self) -> f64 {
        if self.vscale > 0.0 {
            self.vscale
        } else {
            1.0
        }
    }
    /// Converts an absolute height into the height it would have in a
    /// default 384-block world, so altitude thresholds work at any scale.
    #[inline]
    pub fn normalized_y(&self, y: f64) -> f64 {
        SEA_LEVEL as f64 + (y - SEA_LEVEL as f64) / self.scale()
    }
}

pub struct TerrainShaper {
    continentalness: DoubleNoise,
    erosion: DoubleNoise,
    weirdness: DoubleNoise,
    river: DoubleNoise,
    jagged: DoubleNoise,
    temperature: DoubleNoise,
    humidity: DoubleNoise,
    islands: DoubleNoise,
    warp_x: DoubleNoise,
    warp_z: DoubleNoise,
    /// Horizontal stretch of all climate noise: 4 for Large biomes, times
    /// the world's terrain scale.
    scale: f64,
    /// Vertical magnification (the world's terrain scale).
    vscale: f64,
    world_type: WorldType,

    base_height: Curve,
    inland: Curve,
    mountain_factor: Curve,
    ridge_height: Curve,
    hill_height: Curve,
    roughness_curve: Curve,
}

/// Each climate noise is rescaled so its 1st/99th percentiles land near ±0.9
/// (measured over 200,000 samples, see devlog 02).
const GAIN_CONTINENTALNESS: f64 = 2.45;
const GAIN_EROSION: f64 = 2.4;
const GAIN_WEIRDNESS: f64 = 2.3;
const GAIN_RIVER: f64 = 2.3;
const GAIN_TEMPERATURE: f64 = 2.3;
const GAIN_HUMIDITY: f64 = 2.3;
const GAIN_ISLAND: f64 = 1.9;

impl TerrainShaper {
    /// `terrain_scale` magnifies the terrain in all three directions (see
    /// `WorldHeight`); 1.0 for the default 384-block world.
    pub fn new(seed: u64, world_type: WorldType, terrain_scale: f64) -> Self {
        let n = |salt: &str, first: i32, amps: &[f64]| {
            DoubleNoise::new(derive_seed(seed, salt), &OctaveConfig::new(first, amps))
        };
        Self {
            // Continents: lowest octave ~4 km wide, nine octaves down to
            // 16 blocks, so land masses are several km across with detailed
            // coastlines.
            continentalness: n("climate/continentalness", -12, &[1.0; 9]),
            // Mountain/plain regions ~2 km, ridges and valleys ~0.5 km.
            erosion: n("climate/erosion", -11, &[1.0; 5]),
            weirdness: n("climate/weirdness", -9, &[1.0; 4]),
            river: n("terrain/river", -11, &[1.0; 4]),
            jagged: n("terrain/jagged", -6, &[1.0, 1.0, 1.0]),
            // Temperature changes slowly (zones ~4 km across, lowest octave
            // 8 km) so a glacier never sits next to a jungle; humidity
            // twice as fast, so each climate zone holds several biomes.
            temperature: n("climate/temperature", -13, &[1.0; 4]),
            humidity: n("climate/humidity", -12, &[1.0; 4]),
            // Island-sized blobs (~250 m); only the rare strongest peaks of
            // this noise, and only over deep ocean, become islands.
            islands: n("terrain/islands", -9, &[1.0, 1.0]),
            warp_x: n("terrain/warp_x", -10, &[1.0, 1.0, 1.0]),
            warp_z: n("terrain/warp_z", -10, &[1.0, 1.0, 1.0]),
            scale: if world_type == WorldType::LargeBiomes { 4.0 } else { 1.0 } * terrain_scale,
            vscale: terrain_scale,
            world_type,

            // Height relative to sea level by continentalness: abyss, deep
            // ocean, continental shelf, coast, beach, lowland, inland plateau.
            base_height: Curve::new(&[
                (-1.0, -48.0),
                (-0.55, -42.0),
                (-0.32, -30.0),
                (-0.19, -14.0),
                (-0.11, -4.0),
                (-0.06, 1.5),
                (0.0, 4.0),
                (0.25, 12.0),
                (0.55, 24.0),
                (1.0, 34.0),
            ]),
            // How "inland" a column is: mountains only rise away from the sea.
            inland: Curve::new(&[(-0.2, 0.0), (-0.08, 0.15), (0.05, 0.6), (0.3, 1.0)]),
            // Low erosion → mountainous, high erosion → flat.
            mountain_factor: Curve::new(&[
                (-1.0, 1.0),
                (-0.55, 0.9),
                (-0.25, 0.45),
                (0.05, 0.12),
                (0.4, 0.02),
                (1.0, 0.0),
            ]),
            // Mountain relief by peaks & valleys.
            ridge_height: Curve::new(&[
                (-1.0, -5.0),
                (-0.4, 0.0),
                (0.0, 10.0),
                (0.3, 36.0),
                (0.6, 95.0),
                (0.85, 150.0),
                (1.0, 170.0),
            ]),
            // Gentle hills where erosion is high.
            hill_height: Curve::new(&[(-1.0, -2.0), (-0.3, 0.0), (0.0, 2.0), (0.5, 9.0), (1.0, 14.0)]),
            // 3D noise amplitude by mountain factor.
            roughness_curve: Curve::new(&[(0.0, 1.0), (0.1, 1.6), (0.4, 4.5), (0.7, 9.0), (1.0, 13.0)]),
        }
    }

    /// Climate parameters at a block column.
    pub fn climate(&self, x: i32, z: i32) -> Climate {
        let (fx, fz) = (x as f64 / self.scale, z as f64 / self.scale);
        // Domain warping: offset the lookup position by another noise, so
        // coastlines and ranges curve organically instead of looking like
        // blurred blobs.
        let wx = fx + self.warp_x.sample_2d(fx, fz) * 300.0;
        let wz = fz + self.warp_z.sample_2d(fx, fz) * 300.0;

        let mut c = (self.continentalness.sample_2d(wx, wz) * GAIN_CONTINENTALNESS).clamp(-1.0, 1.0);
        if self.world_type == WorldType::Islands {
            // Push most of the world below the coast threshold.
            c = (c * 0.8 - 0.42).clamp(-1.0, 1.0);
        }
        let e = (self.erosion.sample_2d(wx, wz) * GAIN_EROSION).clamp(-1.0, 1.0);
        let w = (self.weirdness.sample_2d(wx, wz) * GAIN_WEIRDNESS).clamp(-1.0, 1.0);
        let pv = 1.0 - ((3.0 * w.abs()) - 2.0).abs();
        let temperature = (self.temperature.sample_2d(wx, wz) * GAIN_TEMPERATURE).clamp(-1.0, 1.0);
        let humidity = (self.humidity.sample_2d(wx, wz) * GAIN_HUMIDITY).clamp(-1.0, 1.0);
        Climate { continentalness: c, erosion: e, weirdness: w, peaks_valleys: pv, temperature, humidity }
    }

    /// Full shaping for one column.
    pub fn shape(&self, x: i32, z: i32) -> Shape {
        let climate = self.climate(x, z);
        let Climate { continentalness: c, erosion: e, peaks_valleys: pv, .. } = climate;

        let inland = self.inland.eval(c);
        let mtn = self.mountain_factor.eval(e) * inland;
        let mut relief = mtn * self.ridge_height.eval(pv) + (1.0 - mtn) * inland * self.hill_height.eval(pv);

        // Jagged summits: extra high-frequency detail only on high ridges.
        let peakness = mtn * smoothstep(0.35, 0.9, pv);
        if peakness > 0.0 {
            let (fx, fz) = (x as f64 / self.scale, z as f64 / self.scale);
            relief += peakness * self.jagged.sample_2d(fx, fz).abs() * 45.0;
        }

        let mut roughness = self.roughness_curve.eval(mtn);
        if self.world_type == WorldType::Amplified {
            relief = if relief > 0.0 { relief * 2.2 } else { relief * 1.4 };
            roughness *= 1.6;
        }

        let mut height = SEA_LEVEL as f64 + self.base_height.eval(c) + relief;

        // Oceanic islands: only over deep ocean, where the island noise has
        // one of its rare high peaks. The noise rises smoothly towards the
        // peak, so the island is a cone: a volcano-like summit up to ~70
        // blocks above the sea with gentle flanks running into the sea floor.
        let mut island = 0.0;
        let deep_ocean = 1.0 - smoothstep(-0.45, -0.3, c);
        if deep_ocean > 0.0 {
            let (fx, fz) = (x as f64 / self.scale, z as f64 / self.scale);
            let v = (self.islands.sample_2d(fx, fz) * GAIN_ISLAND).clamp(-1.0, 1.0);
            island = smoothstep(0.62, 0.95, v) * deep_ocean;
            if island > 0.0 {
                height = height.max(SEA_LEVEL as f64 - 14.0 + 85.0 * island);
                roughness = lerp(island.min(0.5) * 2.0, roughness, 1.5);
            }
        }

        // Rivers: a thin band where the river noise crosses zero. They are
        // carved down to just below sea level, fade out in high terrain and
        // naturally join the sea because the band continues into the ocean.
        let (fx, fz) = (x as f64 / self.scale, z as f64 / self.scale);
        let r = (self.river.sample_2d(fx, fz) * GAIN_RIVER).abs();
        let channel = 1.0 - smoothstep(0.008, 0.03, r);
        let valley = 1.0 - smoothstep(0.02, 0.1, r);
        let above_sea = height - SEA_LEVEL as f64;
        let allowed = smoothstep(-0.16, -0.06, c) * (1.0 - smoothstep(30.0, 70.0, above_sea));
        let river = (channel * allowed).clamp(0.0, 1.0);
        if allowed > 0.0 && above_sea > -3.0 {
            // A wide gentle valley, then the channel itself.
            let valley_floor = SEA_LEVEL as f64 + 1.0 + above_sea * 0.35;
            height = lerp(valley * allowed * 0.8, height, valley_floor.min(height));
            height = lerp(river, height, SEA_LEVEL as f64 - 3.0);
            roughness = lerp(valley * allowed, roughness, 1.0);
        }

        // Tall worlds: the whole shape is computed at default scale (on
        // coordinates divided by the scale) and then stretched vertically
        // around sea level by the same factor.
        if self.vscale != 1.0 {
            height = SEA_LEVEL as f64 + (height - SEA_LEVEL as f64) * self.vscale;
            roughness *= self.vscale;
        }

        Shape { climate, height, roughness, river, island, vscale: self.vscale }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn climate_parameters_in_range_and_spread() {
        let t = TerrainShaper::new(1, WorldType::Normal, 1.0);
        let mut c_min: f64 = 1.0;
        let mut c_max: f64 = -1.0;
        for i in 0..4000 {
            let cl = t.climate((i % 64) * 701 - 22_000, (i / 64) * 677 - 21_000);
            for v in [cl.continentalness, cl.erosion, cl.weirdness, cl.peaks_valleys] {
                assert!((-1.0..=1.0).contains(&v));
            }
            c_min = c_min.min(cl.continentalness);
            c_max = c_max.max(cl.continentalness);
        }
        assert!(c_min < -0.6 && c_max > 0.6, "continentalness spread {c_min}..{c_max}");
    }

    #[test]
    fn peaks_valleys_folding() {
        let pv = |w: f64| 1.0 - ((3.0 * f64::abs(w)) - 2.0).abs();
        assert!((pv(0.0) - -1.0).abs() < 1e-12);
        assert!((pv(2.0 / 3.0) - 1.0).abs() < 1e-12);
        assert!((pv(-2.0 / 3.0) - 1.0).abs() < 1e-12);
        assert!((pv(1.0) - 0.0).abs() < 1e-12);
    }
}
