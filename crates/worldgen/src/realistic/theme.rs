//! The Mushoku planet: every continent has one dominant biome.
//!
//! The planet is built like Continents (the same planned continents,
//! plates, ranges and islands), then each continent is given a *theme*: a
//! dominant biome, two companion biomes of the same family (a desert with
//! thorn scrub and savanna, a taiga with snowy taiga and mixed forest), and
//! the range of climates the theme allows. The simulated climate is pulled
//! towards the theme before the rivers and lakes are worked out, so a
//! desert continent is dry all through: few rivers, salt lakes, sand seas.
//!
//! * In a continent's interior the dominant biome covers almost everything;
//!   companions appear in zones (a slow noise) and where the simulated
//!   climate pulls away from the theme, mostly near the coasts (the pull
//!   is weaker there, so the sea's influence shows).
//! * Across a strait the themes of the two continents blend, so islands
//!   between a desert and a jungle continent get something in between.
//! * The theme's climate range is also handed to the biome rules
//!   (`Shape::theme_t`, `theme_h`), which clamp the column's climate into
//!   it, altitude included: an opposite biome (snow on a desert continent,
//!   jungle on a frozen one) can never appear.

use super::climate::{self, Climate};
use super::grid::{distance_field, Grid, NEIGHBOURS8};
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::rng::{derive_seed, Xoroshiro128PlusPlus};
use crate::spline::{lerp, smoothstep};
use rayon::prelude::*;

/// A continent's theme, in the biome code's temperature and humidity
/// units (`biome::LOWLAND`).
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub name: &'static str,
    /// The dominant biome's climate.
    pub main: (f64, f64),
    /// Two companions of the same family.
    pub companions: [(f64, f64); 2],
    /// Allowed temperature and humidity (inclusive ranges).
    pub t: (f64, f64),
    pub h: (f64, f64),
}

pub const THEMES: [Theme; 8] = [
    // Sand desert, with thorn scrub and dry savanna.
    Theme {
        name: "desert",
        main: (0.75, -0.62),
        companions: [(0.72, -0.26), (0.62, -0.1)],
        t: (0.42, 1.2),
        h: (-1.0, -0.03),
    },
    // Tropical rainforest, with dry tropical forest and sparse jungle.
    Theme { name: "jungle", main: (0.72, 0.45), companions: [(0.72, 0.12), (0.4, 0.3)], t: (0.3, 1.2), h: (0.02, 1.0) },
    // Boreal forest, with snowy taiga and old-growth taiga.
    Theme {
        name: "taiga",
        main: (-0.32, 0.06),
        companions: [(-0.52, 0.15), (-0.3, 0.4)],
        t: (-0.62, -0.12),
        h: (-0.3, 0.7),
    },
    // Snow and ice, with tundra and ice sheets.
    Theme {
        name: "frost",
        main: (-0.86, 0.45),
        companions: [(-0.78, 0.0), (-1.2, 0.2)],
        t: (-1.5, -0.62),
        h: (-0.6, 1.0),
    },
    // Temperate broadleaf forest, with autumn forest and dark forest.
    Theme {
        name: "forest",
        main: (0.1, 0.02),
        companions: [(-0.05, 0.22), (0.13, 0.3)],
        t: (-0.18, 0.3),
        h: (-0.3, 0.5),
    },
    // Tropical savanna, with thorn scrub and tropical dry forest.
    Theme {
        name: "savanna",
        main: (0.66, -0.07),
        companions: [(0.72, -0.26), (0.72, 0.08)],
        t: (0.3, 1.2),
        h: (-0.45, 0.16),
    },
    // Temperate grassland, with steppe and chaparral.
    Theme {
        name: "grassland",
        main: (0.02, -0.24),
        companions: [(0.0, -0.56), (0.32, -0.24)],
        t: (-0.2, 0.42),
        h: (-0.75, -0.04),
    },
    // Temperate rainforest, with old-growth taiga and mixed forest.
    Theme {
        name: "rainforest",
        main: (-0.1, 0.83),
        companions: [(-0.3, 0.4), (-0.16, 0.3)],
        t: (-0.42, 0.15),
        h: (0.22, 1.0),
    },
];

/// The theme of every grid cell, blended between continents.
pub struct ThemeMap {
    /// How strongly the theme holds (0 in the open ocean far from land).
    pub k: Grid<f32>,
    /// Target climate (biome units).
    pub t: Grid<f32>,
    pub h: Grid<f32>,
    /// Allowed range, already widened where the theme is weak.
    pub t_lo: Grid<f32>,
    pub t_hi: Grid<f32>,
    pub h_lo: Grid<f32>,
    pub h_hi: Grid<f32>,
    /// Each continent's theme, west to east (for tests and the HUD):
    /// (theme index, cells).
    pub continents: Vec<(usize, usize)>,
}

/// Landmasses of at least this share of the land are continents.
const CONTINENT_SHARE: f64 = 0.015;
/// Below this distance (km) two continents' themes blend.
const BLEND_KM: f64 = 350.0;

impl ThemeMap {
    pub fn new(seed: u64, elevation: &Grid<f32>, km: f64) -> Self {
        let (w, h) = (elevation.w, elevation.h);
        let n = w * h;
        // Landmasses (8-connected), largest first.
        let land: Vec<bool> = elevation.data.iter().map(|&e| e >= 0.0).collect();
        let mut label = vec![u32::MAX; n];
        let mut masses: Vec<Vec<usize>> = Vec::new();
        for s in 0..n {
            if !land[s] || label[s] != u32::MAX {
                continue;
            }
            let id = masses.len() as u32;
            let mut cells = vec![s];
            label[s] = id;
            let mut k = 0;
            while k < cells.len() {
                let c = cells[k];
                k += 1;
                let (x, z) = ((c % w) as isize, (c / w) as isize);
                for &(dx, dz, _) in &NEIGHBOURS8 {
                    let (a, b) = (x + dx, z + dz);
                    if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                        continue;
                    }
                    let j = a as usize + b as usize * w;
                    if land[j] && label[j] == u32::MAX {
                        label[j] = id;
                        cells.push(j);
                    }
                }
            }
            masses.push(cells);
        }
        let total: usize = masses.iter().map(|m| m.len()).sum();
        let mut big: Vec<&Vec<usize>> =
            masses.iter().filter(|m| m.len() as f64 >= CONTINENT_SHARE * total.max(1) as f64).collect();
        // West to east by centroid: the same continents get the same themes
        // at every simulation quality (sizes, and so a size order, shift a
        // little with the grid).
        let centroid = |m: &Vec<usize>| m.iter().map(|&c| (c % w) as f64).sum::<f64>() / m.len() as f64 / w as f64;
        big.sort_by(|a, b| centroid(a).total_cmp(&centroid(b)).then(a[0].cmp(&b[0])));

        // Themes: a shuffled deck, so neighbours rarely repeat.
        let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/mushoku"));
        let mut deck: Vec<usize> = (0..THEMES.len()).collect();
        for i in (1..deck.len()).rev() {
            deck.swap(i, rng.next_bounded(i as u32 + 1) as usize);
        }
        let continents: Vec<(usize, usize)> =
            big.iter().enumerate().map(|(k, m)| (deck[k % deck.len()], m.len())).collect();

        // Distance (km) from every cell to each continent.
        let reach = 2500.0 / km;
        let dists: Vec<Vec<f32>> = big.iter().map(|m| distance_field(w, h, m, reach, |_, _| true).0).collect();
        // Distance to the sea (km) inside the land, for the coastal pull.
        let sea: Vec<usize> = (0..n).filter(|&i| !land[i]).collect();
        let (coast, _) = distance_field(w, h, &sea, 600.0 / km, |_, _| true);

        let zones = [
            OctaveNoise::new(derive_seed(seed, "realistic/mushoku_a"), &OctaveConfig::new(-8, &[1.0, 0.5])),
            OctaveNoise::new(derive_seed(seed, "realistic/mushoku_b"), &OctaveConfig::new(-8, &[1.0, 0.5])),
        ];
        let cell = |i: usize| -> [f32; 7] {
            let (x, z) = (((i % w) as f64 + 0.5) * km, ((i / w) as f64 + 0.5) * km);
            let (mut wsum, mut near) = (0.0, 0.0f64);
            let mut acc = [0.0f64; 6];
            for (c, d) in dists.iter().enumerate() {
                let d = d[i] as f64 * km;
                if !d.is_finite() {
                    continue;
                }
                let wt = libm::exp(-(d / BLEND_KM).powi(2));
                near = near.max(libm::exp(-(d / 1200.0).powi(2)));
                if wt < 1e-4 {
                    continue;
                }
                let th = &THEMES[continents[c].0];
                // Companion zones: each covers roughly a fifth of the land.
                let mut target = th.main;
                for (k, comp) in th.companions.iter().enumerate() {
                    let s = smoothstep(0.12, 0.35, zones[k].sample_2d(x, z)) * 0.9;
                    target = (lerp(s, target.0, comp.0), lerp(s, target.1, comp.1));
                }
                for (a, v) in acc.iter_mut().zip([target.0, target.1, th.t.0, th.t.1, th.h.0, th.h.1]) {
                    *a += wt * v;
                }
                wsum += wt;
            }
            if wsum < 1e-6 {
                return [0.0, 0.0, 0.0, -9.0, 9.0, -9.0, 9.0];
            }
            let v: Vec<f64> = acc.iter().map(|a| a / wsum).collect();
            // Weaker towards the sea, so coasts take some of the simulated
            // climate (within the theme's range).
            let shore = if land[i] { smoothstep(0.0, 400.0, coast[i] as f64 * km) } else { 0.0 };
            let k = near * lerp(shore, 0.7, 0.92);
            // The range opens up where the theme fades out.
            let hold = smoothstep(0.25, 0.6, near);
            [
                k as f32,
                v[0] as f32,
                v[1] as f32,
                lerp(hold, -9.0, v[2]) as f32,
                lerp(hold, 9.0, v[3]) as f32,
                lerp(hold, -9.0, v[4]) as f32,
                lerp(hold, 9.0, v[5]) as f32,
            ]
        };
        let cells: Vec<[f32; 7]> = (0..n).into_par_iter().map(cell).collect();
        let grid = |k: usize| Grid { w, h, data: cells.iter().map(|c| c[k]).collect() };
        ThemeMap {
            k: grid(0),
            t: grid(1),
            h: grid(2),
            t_lo: grid(3),
            t_hi: grid(4),
            h_lo: grid(5),
            h_hi: grid(6),
            continents,
        }
    }

    /// Pulls a simulated climate towards the themes: temperature and
    /// aridity, and the rain to match (aridity = rain / evaporation).
    pub fn apply(&self, c: &mut Climate) {
        // Inverse curves, tabulated once (the inverses bisect).
        let table = |f: &dyn Fn(f64) -> f64, lo: f64, hi: f64| -> Vec<f64> {
            (0..=1024).map(|k| f(lo + (hi - lo) * k as f64 / 1024.0)).collect()
        };
        let look = |t: &[f64], lo: f64, hi: f64, v: f64| {
            let p = ((v - lo) / (hi - lo)).clamp(0.0, 1.0) * 1024.0;
            let k = (p as usize).min(1023);
            lerp(p - k as f64, t[k], t[k + 1])
        };
        let to_c = table(&climate::temperature_from_biome, -1.4, 1.0);
        let to_a = table(&climate::aridity_from_biome, -1.0, 1.0);
        let out: Vec<(f32, f32, f32)> = (0..c.temperature.data.len())
            .into_par_iter()
            .map(|i| {
                let (t0, a0, p0) = (c.temperature.data[i], c.aridity.data[i], c.precipitation.data[i]);
                let k = self.k.data[i] as f64;
                if k <= 0.0 {
                    return (t0, a0, p0);
                }
                let tb = climate::biome_temperature(t0 as f64);
                let hb = climate::biome_humidity(a0 as f64);
                let t = lerp(k, tb, self.t.data[i] as f64)
                    .clamp(self.t_lo.data[i] as f64, self.t_hi.data[i] as f64)
                    .clamp(-1.4, 1.0);
                let hum = lerp(k, hb, self.h.data[i] as f64)
                    .clamp(self.h_lo.data[i] as f64, self.h_hi.data[i] as f64)
                    .clamp(-1.0, 1.0);
                let tc = look(&to_c, -1.4, 1.0, t);
                let ar = look(&to_a, -1.0, 1.0, hum);
                (tc as f32, ar as f32, (ar * climate::potential_evaporation(tc)) as f32)
            })
            .collect();
        for (i, (t, a, p)) in out.into_iter().enumerate() {
            c.temperature.data[i] = t;
            c.aridity.data[i] = a;
            c.precipitation.data[i] = p;
        }
    }
}
