//! The realistic generator: a whole Earth-like planet.
//!
//! The classic generator (`terrain.rs`) decides every column from local
//! noise, which is fast and endless but has no idea where it is on a
//! planet: mountains are where a noise value happens to be high. Real
//! terrain is organised at the scale of the whole planet: ranges follow
//! plate boundaries, rivers drain whole continents to the sea, deserts sit
//! at 25° latitude and in rain shadows. So this generator works in two
//! stages:
//!
//! 1. **Planet simulation** when the world is created (a few seconds):
//!    on a grid covering the whole world (384² to 1536² cells, or the
//!    real Earth's relief) it runs [`tectonics`] (or loads [`earth`]),
//!    [`climate`], [`erosion`] by rivers, and extracts the river network
//!    and lakes. Deterministic: same settings, same planet.
//! 2. **Column shaping** at any block, on any thread: the grid is
//!    interpolated (bicubic, with a domain warp so no grid lines show) and
//!    refined with fractal detail whose amplitude and grain follow the
//!    grid: ridged, strike-aligned noise in mountains (long parallel ridges
//!    and a main divide along the range, side valleys across it), gentle
//!    noise on plains, abyssal hills at sea. Rivers are carved along the
//!    simulated network with their own water level, so they flow downhill
//!    from the mountains to the sea, and lakes fill basins to their rim.
//!
//! The output is the same [`Shape`] the classic generator produces, so
//! biomes, surface blocks, trees, chunks, LODs and maps work unchanged.
//!
//! ## Scale
//!
//! The planet is simulated as if it were Earth-sized (40,000 km across),
//! so plate and climate numbers are real ones, and mapped onto the world:
//! in a 40,000 km world one block is one real metre. Heights use the
//! room the world height gives: Everest-sized peaks reach ~92 % of the
//! space above the sea, but never more than true scale in Earth-sized
//! worlds, so a 40,000 km × 17,696-block world is the real Earth at 1:1.
//! Ocean depths are compressed smoothly into the room below the sea.

pub mod climate;
pub mod earth;
pub mod erosion;
pub mod grid;
pub mod tectonics;

use crate::noise::{wrap, ImprovedPerlin, OctaveConfig, OctaveNoise};
use crate::rng::{derive_seed, hash_position, Xoroshiro128PlusPlus};
use crate::settings::{Planet, SimQuality, WorldSettings, SEA_LEVEL};
use crate::spline::{lerp, smoothstep, Curve};
use crate::terrain::{Climate, Shape};
use grid::Grid;

/// The virtual planet's width, km (Earth's circumference, rounded).
pub const PLANET_KM: f64 = 40_000.0;
/// Highest real peak the vertical scale is fitted to (Everest), m.
const PEAK_M: f64 = 8_849.0;
/// Real metres per "normalized block" (see `Shape::normalized_y`): the
/// classic world's altitude rules (tree line ~3.6 km, snow line ~5 km,
/// summits above ~4.6 km) are tuned for peaks ~240 blocks above the sea.
const M_PER_NORMALIZED_BLOCK: f64 = 37.3;

/// One river reach between two nodes, upstream `a` to downstream `b`.
#[derive(Clone, Copy, Debug)]
struct Reach {
    a: u32,
    b: u32,
}

#[derive(Clone, Copy, Debug)]
struct RiverNode {
    /// Grid coordinates (cell centres at i + 0.5).
    u: f64,
    v: f64,
    /// Water surface, real metres.
    level: f64,
    /// Discharge, m³/s.
    q: f64,
}

/// Simulation sizes per quality: grid cells across, erosion steps, and
/// the smallest river (m³/s) kept.
fn quality_params(q: SimQuality, earth: bool) -> (usize, usize, f64) {
    match (q, earth) {
        (SimQuality::Fast, false) => (384, 6, 600.0),
        (SimQuality::Balanced, false) => (768, 10, 250.0),
        (SimQuality::Quality, false) => (1536, 14, 100.0),
        (SimQuality::Fast, true) => (1200, 0, 600.0),
        (SimQuality::Balanced, true) => (1800, 0, 250.0),
        (SimQuality::Quality, true) => (3600, 0, 100.0),
    }
}

pub struct RealisticWorld {
    w: usize,
    h: usize,
    /// Cell size, km of the virtual planet.
    km: f64,
    /// Cell size in blocks.
    cell_blocks: f64,
    /// Block coordinates of the grid's north-west corner.
    x0: f64,
    z0: f64,
    /// Blocks per real metre, horizontally.
    h_scale: f64,
    /// Blocks per real metre above the sea.
    v_scale: f64,
    /// Blocks of room below the sea.
    ocean_room: f64,
    /// `Shape::vscale` for the biome rules.
    norm_scale: f64,
    /// The world height's terrain scale (the density noise is stretched by it).
    world_scale: f64,

    elevation: Grid<f32>,
    mountain: Grid<f32>,
    strike_c: Grid<f32>,
    strike_s: Grid<f32>,
    relief: Grid<f32>,
    temperature: Grid<f32>,
    humidity: Grid<f32>,
    /// Lake surface (m), or `NO_LAKE`.
    lake: Grid<f32>,
    /// 1 where a cell is the open sea (connected to the world ocean).
    sea: Grid<f32>,
    /// 1 where a cell is land below sea level cut off from the open sea
    /// (Dead Sea, Caspian depression): it stays dry unless it is a lake.
    basin: Grid<f32>,

    nodes: Vec<RiverNode>,
    reaches: Vec<Reach>,
    /// Reaches near each cell (indices into `reaches`).
    reach_index: Vec<Vec<u32>>,
    /// Widest valley half-width of any reach, in cells.
    max_reach_cells: f64,

    octaves: Vec<ImprovedPerlin>,
    warp_u: OctaveNoise,
    warp_v: OctaveNoise,
    meander_u: OctaveNoise,
    meander_v: OctaveNoise,
    variety: OctaveNoise,
    spawn: (i32, i32),
}

const NO_LAKE: f32 = -1.0e9;

impl RealisticWorld {
    pub fn new(settings: &WorldSettings) -> Self {
        let settings = settings.clone().normalized();
        let earth = settings.planet == Planet::Earth;
        let seed = settings.seed;
        let (res, steps, q_min) = quality_params(settings.quality, earth);
        let (w, h) = if earth { (res, res / 2) } else { (res, res) };
        let km = PLANET_KM / w as f64;

        // --- Relief ------------------------------------------------------
        let (mut elevation, target, mountain, strike_c, strike_s) = if earth {
            let full = earth::relief();
            let e = if (full.w, full.h) == (w, h) { full } else { full.resample(w, h) };
            let (m, sc, ss) = mountains_from_relief(&e, km);
            (e.clone(), e, m, sc, ss)
        } else {
            let t = tectonics::build(seed, settings.planet, w, h, km);
            (t.elevation.clone(), t.elevation, t.mountain, t.strike_c, t.strike_s)
        };

        // --- Erosion, driven by a first estimate of the rain --------------
        if steps > 0 {
            let c = climate::simulate(&elevation, km, seed, 6.0);
            let land: Vec<f64> = c
                .precipitation
                .data
                .iter()
                .zip(&elevation.data)
                .filter(|(_, &e)| e >= 0.0)
                .map(|(&p, _)| p as f64)
                .collect();
            let mean = (land.iter().sum::<f64>() / land.len().max(1) as f64).max(1.0);
            let rain: Vec<f32> = c.precipitation.data.iter().map(|&p| (p as f64 / mean) as f32).collect();
            let params = erosion::ErosionParams { steps, km, k: 0.9 * km.sqrt() / 50.0, uplift: 0.25, sediment: 0.25 };
            erosion::erode(&mut elevation.data, &target.data, &rain, w, h, &params);
        }

        // --- Climate, drainage, rivers, lakes -----------------------------
        let clim = climate::simulate(&elevation, km, seed, if earth { 0.0 } else { 6.0 });
        // Only the open sea drains the land: basins below sea level inland
        // (the Caspian) fill up like any other hollow.
        let sea =
            Grid { w, h, data: erosion::open_sea(&elevation.data, w, h).iter().map(|&b| b as u8 as f32).collect() };
        let drainage = erosion::priority_flood_from(&elevation.data, w, h, &|i| sea.data[i] > 0.5);
        let area_m2 = km * km * 1e6;
        let runoff: Vec<f32> = clim
            .precipitation
            .data
            .iter()
            .zip(&clim.temperature.data)
            .map(|(&p, &t)| {
                let p = p as f64;
                let et = p.min(0.8 * climate::potential_evaporation(t as f64));
                ((p - et).max(0.0) / 1000.0 * area_m2 / 3.156e7) as f32
            })
            .collect();
        let discharge = drainage.accumulate(&runoff);

        let lake = if earth {
            earth::lakes(&elevation, &drainage.filled, &sea.data)
        } else {
            water_balance_lakes(&elevation, &drainage, &sea, &discharge, &clim, area_m2)
        };

        let basin = Grid {
            w,
            h,
            data: (0..w * h).map(|i| (sea.data[i] < 0.5 && elevation.data[i] < 0.0) as u8 as f32).collect(),
        };
        let mut world = Self {
            w,
            h,
            km,
            cell_blocks: settings.size.width_blocks() as f64 / w as f64,
            x0: -(settings.size.half_width() as f64),
            z0: -(settings.size.half_depth() as f64),
            h_scale: 0.0,
            v_scale: 0.0,
            ocean_room: 0.0,
            norm_scale: 1.0,
            world_scale: settings.height.terrain_scale(),
            relief: mountain.map(|m| (90.0 + 2800.0 * (m as f64).powf(1.3)) as f32),
            elevation,
            mountain,
            strike_c,
            strike_s,
            temperature: clim.temperature,
            humidity: clim.aridity.map(|a| climate::biome_humidity(a as f64) as f32),
            lake,
            basin,
            sea,
            nodes: Vec::new(),
            reaches: Vec::new(),
            reach_index: Vec::new(),
            max_reach_cells: 0.0,
            octaves: {
                let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/detail"));
                (0..24).map(|_| ImprovedPerlin::new(&mut rng)).collect()
            },
            warp_u: OctaveNoise::new(derive_seed(seed, "realistic/warp_u"), &OctaveConfig::new(0, &[1.0, 0.5, 0.25])),
            warp_v: OctaveNoise::new(derive_seed(seed, "realistic/warp_v"), &OctaveConfig::new(0, &[1.0, 0.5, 0.25])),
            meander_u: OctaveNoise::new(derive_seed(seed, "realistic/meander_u"), &OctaveConfig::new(0, &[1.0, 1.0])),
            meander_v: OctaveNoise::new(derive_seed(seed, "realistic/meander_v"), &OctaveConfig::new(0, &[1.0, 1.0])),
            variety: OctaveNoise::new(derive_seed(seed, "realistic/variety"), &OctaveConfig::new(0, &[1.0, 1.0, 1.0])),
            spawn: (0, 0),
        };
        world.set_vertical_scale(&settings);
        world.build_rivers(seed, &drainage, &discharge, q_min);
        world.spawn = world.pick_spawn();
        world
    }

    fn set_vertical_scale(&mut self, settings: &WorldSettings) {
        let h_scale = settings.size.width_blocks() as f64 / (PLANET_KM * 1000.0);
        let above = (settings.height.max_y() - SEA_LEVEL) as f64;
        let fit = above * 0.92 / PEAK_M;
        // Small worlds: exaggerate to use the height. Earth-sized worlds:
        // true scale when it fits.
        let v = fit.min(h_scale.max(fit * (1.0 - smoothstep(0.25, 1.0, h_scale))));
        self.h_scale = h_scale;
        self.v_scale = v;
        self.ocean_room = (SEA_LEVEL - settings.height.min_y() - 10) as f64;
        self.norm_scale = (v * M_PER_NORMALIZED_BLOCK).max(1e-6);
    }

    /// Real metres → blocks above (or below) the sea.
    #[inline]
    fn to_blocks(&self, m: f64) -> f64 {
        if m >= 0.0 {
            m * self.v_scale
        } else {
            -self.ocean_room * (1.0 - libm::exp(m * self.v_scale / self.ocean_room))
        }
    }

    /// The river network: every cell whose discharge passes `q_min`
    /// becomes a node joined to its receiver. Nodes are jittered inside
    /// their cell so rivers don't run along grid lines.
    fn build_rivers(&mut self, seed: u64, d: &erosion::Drainage, q: &[f32], q_min: f64) {
        let (w, h) = (self.w, self.h);
        let mut node_of = vec![u32::MAX; w * h];
        let jitter = derive_seed(seed, "realistic/river_jitter");
        let mut node = |i: usize, nodes: &mut Vec<RiverNode>| -> u32 {
            if node_of[i] == u32::MAX {
                let (ci, cj) = ((i % w) as i32, (i / w) as i32);
                let r = hash_position(jitter, ci, 0, cj);
                let ju = ((r & 0xffff) as f64 / 65535.0 - 0.5) * 0.6;
                let jv = (((r >> 16) & 0xffff) as f64 / 65535.0 - 0.5) * 0.6;
                let level = if self.sea.data[i] > 0.5 { 0.0 } else { (d.filled[i] as f64).max(0.0) };
                nodes.push(RiverNode { u: ci as f64 + 0.5 + ju, v: cj as f64 + 0.5 + jv, level, q: q[i] as f64 });
                node_of[i] = (nodes.len() - 1) as u32;
            }
            node_of[i]
        };
        let mut nodes = Vec::new();
        let mut reaches = Vec::new();
        for &c in &d.order {
            let c = c as usize;
            let r = d.receiver[c] as usize;
            if r == c || (q[c] as f64) < q_min || self.sea.data[c] > 0.5 {
                continue;
            }
            let a = node(c, &mut nodes);
            let b = node(r, &mut nodes);
            reaches.push(Reach { a, b });
        }
        // A downstream node's level never exceeds its upstream one (the
        // flood guarantees it for cells; jitter can't change it).
        self.nodes = nodes;
        self.reaches = reaches;
        // Index reaches by the cells their valley can touch.
        let mut index = vec![Vec::new(); w * h];
        let mut max_cells: f64 = 0.0;
        for (k, r) in self.reaches.iter().enumerate() {
            let (a, b) = (self.nodes[r.a as usize], self.nodes[r.b as usize]);
            let reach = self.valley_half_width(a.q.max(b.q)) / self.cell_blocks + 0.6;
            max_cells = max_cells.max(reach);
            let (lo_u, hi_u) = ((a.u.min(b.u) - reach).floor() as isize, (a.u.max(b.u) + reach).floor() as isize);
            let (lo_v, hi_v) = ((a.v.min(b.v) - reach).floor() as isize, (a.v.max(b.v) + reach).floor() as isize);
            for j in lo_v.max(0)..=hi_v.min(h as isize - 1) {
                for i in lo_u.max(0)..=hi_u.min(w as isize - 1) {
                    index[i as usize + j as usize * w].push(k as u32);
                }
            }
        }
        self.reach_index = index;
        self.max_reach_cells = max_cells;
    }

    /// River channel width (blocks) for a discharge (m³/s): hydraulic
    /// geometry, w ≈ 4·Q^0.5 m (the Amazon ~1.8 km, a 200 m³/s river ~55 m).
    fn channel_width(&self, q: f64) -> f64 {
        (4.0 * q.sqrt() * self.h_scale).clamp(5.0, 2500.0)
    }
    /// Channel depth (blocks): d ≈ 0.3·Q^0.4 m.
    fn channel_depth(&self, q: f64) -> f64 {
        (0.3 * libm::pow(q, 0.4) * self.v_scale).clamp(2.0, 30.0)
    }
    fn valley_half_width(&self, q: f64) -> f64 {
        let hw = self.channel_width(q) / 2.0;
        hw * 3.0 + 24.0
    }

    /// A land column near the centre with a mild climate.
    /// The spawn cell: low, temperate, humid land (forest or grassland)
    /// within sight of a mountain range, nearest the middle of the map.
    /// Falls back to any temperate lowland, then to the origin.
    fn pick_spawn(&self) -> (i32, i32) {
        let (cw, ch) = (self.w as f64 / 2.0, self.h as f64 / 2.0);
        let near_mountains = self.mountain.blur(((600.0 / self.km).round() as usize).max(1), 1);
        let mut best = [(f64::INFINITY, 0usize); 2];
        for j in 0..self.h {
            for i in 0..self.w {
                let k = i + j * self.w;
                let e = self.elevation.data[k] as f64;
                let t = self.temperature.data[k] as f64;
                if !(20.0..1500.0).contains(&e) || !(4.0..24.0).contains(&t) || self.mountain.data[k] > 0.3 {
                    continue;
                }
                let d = (i as f64 + 0.5 - cw).powi(2) + (j as f64 + 0.5 - ch).powi(2);
                let scenic = (6.0..20.0).contains(&t)
                    && self.humidity.data[k] >= 0.0
                    && near_mountains.data[k] > 0.12
                    && self.lake.data[k] <= NO_LAKE;
                for (b, ok) in best.iter_mut().zip([scenic, true]) {
                    if ok && d < b.0 {
                        *b = (d, k);
                    }
                }
            }
        }
        let Some(&(_, k)) = best.iter().find(|b| b.0.is_finite()) else { return (0, 0) };
        let (i, j) = (k % self.w, k / self.w);
        ((self.x0 + (i as f64 + 0.5) * self.cell_blocks) as i32, (self.z0 + (j as f64 + 0.5) * self.cell_blocks) as i32)
    }

    /// Where `find_spawn` starts looking.
    pub fn spawn_hint(&self) -> (i32, i32) {
        self.spawn
    }
    /// Size of a grid cell in blocks (how far apart features of the
    /// simulation are).
    pub fn cell_blocks(&self) -> f64 {
        self.cell_blocks
    }
    /// Latitude (degrees, + north) of a block row.
    pub fn latitude(&self, z: i32) -> f64 {
        90.0 - (z as f64 - self.z0) / (self.cell_blocks * self.h as f64) * 180.0
    }
    /// Number of river reaches (for tests and statistics).
    pub fn river_count(&self) -> usize {
        self.reaches.len()
    }
    /// The simulation grid's size.
    pub fn grid_size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// Sum of detail octaves, metres. `amp` is the relief at the cell
    /// scale; `ridged` (0..1) blends from smooth fBm to ridged noise.
    /// `(c2, s2)` is the doubled-angle range axis: the largest octaves are
    /// stretched along it, giving long parallel ridges.
    ///
    /// Rotating the coordinates by the local axis would swirl the noise
    /// wherever the axis turns (a small angle change moves points far from
    /// the origin a long way), so instead the noise is evaluated stretched
    /// along four fixed directions and blended by how well each matches
    /// the local axis.
    #[allow(clippy::too_many_arguments)]
    fn detail(&self, xk: f64, zk: f64, amp: f64, ridged: f64, c2: f64, s2: f64, aniso: f64, min_km: f64) -> f64 {
        use std::f64::consts::FRAC_1_SQRT_2;
        const STRETCH: [f64; 4] = [3.0, 2.4, 1.6, 1.0];
        const DIRS: [(f64, f64); 4] =
            [(1.0, 0.0), (FRAC_1_SQRT_2, FRAC_1_SQRT_2), (0.0, 1.0), (-FRAC_1_SQRT_2, FRAC_1_SQRT_2)];
        let dots = [c2, s2, -c2, -s2];
        let mut wsum = 0.0;
        let mut wts = [0.0; 4];
        for (w, d) in wts.iter_mut().zip(dots) {
            *w = d.max(0.0) * d.max(0.0);
            wsum += *w * *w;
        }
        let norm = if wsum > 1e-9 { 1.0 / wsum.sqrt() } else { 0.0 };
        let mut sum = 0.0;
        let mut lambda = self.km; // km
        let mut a = amp;
        let mut weight: f64 = 1.0;
        let mut k = 0;
        while lambda >= min_km && k < self.octaves.len() {
            let f = 1.0 / lambda;
            let y = 0.37 + k as f64 * 0.11;
            // The stretch factor is fixed per octave: scaling coordinates by
            // a factor that varies across the map would sweep the noise
            // through many periods (we are far from its origin) and draw
            // stripes along the iso-lines of that factor. The local range
            // strength only sets how much of the stretched noise is used.
            let s = STRETCH[k.min(STRETCH.len() - 1)];
            let t = if s > 1.0 { aniso } else { 0.0 };
            let iso = if t < 1.0 { self.octaves[k].sample(wrap(xk * f), y, wrap(zk * f)) } else { 0.0 };
            let n = if t > 0.0 && norm > 0.0 {
                let mut v = 0.0;
                for (o, &(dx, dz)) in DIRS.iter().enumerate() {
                    if wts[o] == 0.0 {
                        continue;
                    }
                    let along = (xk * dx + zk * dz) / s;
                    let across = -xk * dz + zk * dx;
                    v += wts[o] * self.octaves[k].sample(wrap(along * f), y + o as f64 * 3.3, wrap(across * f));
                }
                // Blend keeping the variance of one noise.
                (lerp(t, iso, v * norm)) / ((1.0 - t) * (1.0 - t) + t * t).sqrt()
            } else {
                iso
            };
            // Ridged multifractal (Musgrave): sharp crests, smooth valley
            // floors, and finer octaves weighted by the coarser signal so
            // detail gathers on the ridges, the way erosion leaves it.
            let r = 1.0 - n.abs() * 1.4;
            let ridge = r * r * weight;
            weight = (ridge * 1.6).clamp(0.0, 1.0);
            let v = lerp(ridged, n, ridge * 1.25 - 0.4);
            sum += a * v;
            lambda *= 0.5;
            // Amplitude ∝ λ^0.8 on plains and hills, ∝ λ^0.55 in high
            // mountains, whose slopes stay steep down to small scales
            // (fractal analyses of real DEMs give Hurst exponents of about
            // 0.5–0.8 between 1 and 50 km).
            a *= lerp(ridged, 0.574, 0.683);
            k += 1;
        }
        sum
    }

    /// Distance (blocks) to the nearest river and its water level (blocks
    /// above the sea), width and depth, if any valley reaches this column.
    fn nearest_river(&self, u: f64, v: f64) -> Option<(f64, f64, f64, f64, f64)> {
        let (i, j) = (u.floor() as isize, v.floor() as isize);
        if i < 0 || j < 0 || i >= self.w as isize || j >= self.h as isize {
            return None;
        }
        let list = &self.reach_index[i as usize + j as usize * self.w];
        if list.is_empty() {
            return None;
        }
        // Meanders: the river follows a warped copy of the straight reach.
        let m = 0.18;
        let (mu, mv) =
            (u + m * self.meander_u.sample_2d(u * 3.0, v * 3.0), v + m * self.meander_v.sample_2d(u * 3.0, v * 3.0));
        let mut best: Option<(f64, f64, f64, f64, f64)> = None;
        for &k in list {
            let r = self.reaches[k as usize];
            let (a, b) = (self.nodes[r.a as usize], self.nodes[r.b as usize]);
            let (dx, dv) = (b.u - a.u, b.v - a.v);
            let len2 = dx * dx + dv * dv;
            let t = if len2 > 0.0 { (((mu - a.u) * dx + (mv - a.v) * dv) / len2).clamp(0.0, 1.0) } else { 0.0 };
            let (pu, pv) = (a.u + t * dx - mu, a.v + t * dv - mv);
            let dist = (pu * pu + pv * pv).sqrt() * self.cell_blocks;
            let q = lerp(t, a.q, b.q);
            let level = lerp(t, a.level, b.level);
            if best.is_none_or(|bst| dist < bst.0) {
                best = Some((dist, level, q, 0.0, 0.0));
            }
        }
        best.map(|(dist, level, q, _, _)| {
            (dist, self.to_blocks(level), self.channel_width(q), self.channel_depth(q), q)
        })
    }

    pub fn shape(&self, x: i32, z: i32) -> Shape {
        let (bx, bz) = (x as f64 + 0.5, z as f64 + 0.5);
        let (u0, v0) = ((bx - self.x0) / self.cell_blocks, (bz - self.z0) / self.cell_blocks);
        // Domain warp of ~0.7 cell hides the grid in coastlines and ranges.
        let wu = u0 + 0.7 * self.warp_u.sample_2d(u0 * 0.5, v0 * 0.5);
        let wv = v0 + 0.7 * self.warp_v.sample_2d(u0 * 0.5, v0 * 0.5);

        let base = self.elevation.bicubic(wu, wv);
        // Bicubic, not bilinear: the detail amplitude follows these, and a
        // kink in it at cell edges would show as a straight crease.
        let m = self.mountain.bicubic(wu, wv).clamp(0.0, 1.0);
        let relief = self.relief.bicubic(wu, wv).max(60.0);
        let (sc, ss) = (self.strike_c.bilinear(wu, wv), self.strike_s.bilinear(wu, wv));
        let sl = (sc * sc + ss * ss).sqrt().max(1e-9);
        let (c2, s2) = (sc / sl, ss / sl);
        // Ranges are stretched along their axis, but only where the axis is
        // well defined: where axes from different ranges cancel out (`sl`
        // near 0) its direction flips, and stretched noise would jump.
        let aniso = smoothstep(0.15, 0.7, m) * smoothstep(0.1, 0.4, sl);

        // Detail from the cell size down to ~6 blocks.
        let (xk, zk) = (u0 * self.km, v0 * self.km);
        let min_km = (6.0 / (self.h_scale * 1000.0)).max(1e-4);
        let ocean_floor = 1.0 - smoothstep(-400.0, 0.0, base);
        let amp = lerp(ocean_floor, relief, 120.0);
        // Valley floors are smooth: the detail fades out towards a river,
        // so the river runs on the simulated terrain it was traced on
        // instead of floating over a dip in the noise.
        let near_river = self.nearest_river(wu, wv);
        let fade = near_river.map_or(1.0, |(dist, _, width, _, q)| {
            let hw = width / 2.0;
            smoothstep(hw, hw + (self.valley_half_width(q) * 6.0).max(0.03 * self.cell_blocks), dist)
        });
        let mut e = base + fade * self.detail(xk, zk, amp, m, c2, s2, aniso, min_km);
        // Peaks taper off above 7 km and never pass ~9.5 km: Everest is
        // 8.8 km, and the world height is sized for it.
        if e > 7000.0 {
            e = 7000.0 + 2500.0 * libm::tanh((e - 7000.0) / 2500.0);
        }

        // Lakes fill their basin up to the rim. The lake cells are blended
        // bilinearly into a 0..1 mask whose 0.5 contour, roughened by a
        // little noise, is the shoreline: smooth and irregular instead of
        // the square outline of the grid cells.
        let lake = {
            let (u, v) = (wu - 0.5, wv - 0.5);
            let (i, j) = (u.floor() as isize, v.floor() as isize);
            let (tx, ty) = (u - u.floor(), v - v.floor());
            let mut level = NO_LAKE as f64;
            let mut mask = 0.0;
            for (di, dj, wt) in
                [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)]
            {
                let l = self.lake.at(i + di, j + dj);
                if l > NO_LAKE {
                    level = level.max(l as f64);
                    mask += wt;
                }
            }
            (mask > 0.0).then(|| {
                // Bays and headlands at a few scales below the cell size.
                let jag = 0.3 * self.variety.sample_2d(u0 * 1.3 + 17.0, v0 * 1.3 - 5.0)
                    + 0.12 * self.variety.sample_2d(u0 * 4.1 - 3.0, v0 * 4.1 + 11.0)
                    + 0.05 * self.variety.sample_2d(u0 * 13.0 + 7.0, v0 * 13.0 + 2.0);
                (level, (mask + jag * (4.0 * mask * (1.0 - mask)).min(1.0)).clamp(0.0, 1.0))
            })
        };

        let mut roughness = 1.0 + 2.5 * m;
        let mut river: f64 = 0.0;
        let mut water = SEA_LEVEL as f64;
        let mut height = SEA_LEVEL as f64 + self.to_blocks(e);
        if let Some((level, mask)) = lake {
            let ly = (SEA_LEVEL as f64 + self.to_blocks(level)).floor();
            if mask >= 0.5 {
                // Lake bed: deepens away from the shore, down to the depth
                // the basin had below its rim.
                let depth = self.to_blocks(level - base).clamp(2.0, 40.0);
                water = ly;
                height = ly - 1.0 - (depth - 1.0) * smoothstep(0.5, 0.9, mask);
            } else {
                // Shore: the land eases to just above the water, so the
                // lake never shows as a wall or spills.
                height = lerp(smoothstep(0.1, 0.5, mask), height, ly + 1.0);
            }
            roughness = lerp(smoothstep(0.1, 0.5, mask), roughness, roughness.min(1.0));
        }

        // Land below sea level inside a closed depression (Dead Sea,
        // Caspian depression) stays dry unless it is a lake. Everywhere
        // else the sea fills anything below its level. The depression mask
        // is sampled through the same warp as the terrain: a basin cell
        // never touches an open-sea cell, so its 0.5 contour lies inland,
        // and coasts follow the terrain instead of the grid. (A per-cell
        // sea mask used here once cut the sea off in straight lines.)
        let basin = self.basin.bilinear(wu, wv);
        if basin > 0.5 && lake.is_none_or(|(_, m)| m < 0.5) && height < SEA_LEVEL as f64 {
            water = f64::NEG_INFINITY;
        }

        if let Some((dist, level, width, depth, q)) = near_river {
            let hw = width / 2.0;
            let valley = self.valley_half_width(q);
            if dist < valley {
                // The traced level, or the land here if that is lower (the
                // simulation grid is coarse: on a steep slope its level can
                // sit above the actual valley floor).
                let wy = (SEA_LEVEL as f64 + level.min(self.to_blocks(base) - 1.0)).floor();
                // Valley sides: the land falls towards the river; banks
                // stay just above the water so it never spills.
                let bank = wy + 1.0 + (dist - hw).max(0.0) * (0.15 + 0.6 * m);
                let t = smoothstep(valley, hw, dist);
                height = lerp(t, height, height.min(bank).max(if dist > hw { wy + 1.0 } else { f64::MIN }));
                roughness = lerp(t, roughness, 0.8);
                if dist <= hw {
                    let bed = wy - depth * (1.0 - (dist / hw).powi(2)).max(0.25);
                    height = height.min(bed);
                    water = water.max(wy);
                    river = 1.0;
                    e = e.min((wy - SEA_LEVEL as f64) / self.v_scale.max(1e-9));
                } else {
                    river = river.max(0.4 * (1.0 - (dist - hw) / (valley - hw)).max(0.0));
                }
            }
        }

        // Climate: sea-level temperature, cooled further by altitude so
        // the total lapse rate is the real 6.5 °C/km (the biome code
        // applies part of it).
        // Local climates differ by a few °C over ~1000 km (currents,
        // winds, cloud), which keeps the sea-ice edge and the biome borders
        // from following the latitude lines exactly.
        let t_noise = (self.variety.sample_2d(xk / 900.0 + 31.0, zk / 900.0 - 7.0) * 2.2).clamp(-1.0, 1.0);
        let t_c = self.temperature.bilinear(wu, wv) + 3.5 * t_noise;
        let alt_norm = (e.max(0.0) / M_PER_NORMALIZED_BLOCK - 15.0).max(0.0);
        let temperature = climate::biome_temperature(t_c) - 0.49 * alt_norm / 160.0;
        let mut humidity = self.humidity.bilinear(wu, wv);
        // Bitter cold land is buried under snow and ice (Antarctica,
        // Greenland) instead of bare tundra: snowy biomes need some humidity.
        let deep_cold = smoothstep(-0.85, -1.05, temperature);
        humidity = lerp(deep_cold, humidity, humidity.max(0.0));
        let w = (self.variety.sample_2d(xk / 30.0, zk / 30.0) * 2.2).clamp(-1.0, 1.0);
        let pv = 1.0 - ((3.0 * w.abs()) - 2.0).abs();
        thread_local! {
            static CONT: Curve = Curve::new(&[
                (-7000.0, -1.0), (-3000.0, -0.7), (-1000.0, -0.45), (-150.0, -0.2),
                (0.0, -0.05), (100.0, 0.0), (400.0, 0.3), (1500.0, 0.7), (5000.0, 1.0),
            ]);
        }
        let continentalness = CONT.with(|c| c.eval(base.clamp(-7000.0, 5000.0)));
        let erosion = (0.5 - 1.5 * m + 0.25 * w).clamp(-1.0, 1.0);

        let water_y = if water.is_finite() { water as i32 } else { i32::MIN / 2 };
        Shape {
            climate: Climate {
                continentalness,
                erosion,
                weirdness: w,
                peaks_valleys: pv,
                temperature: temperature.clamp(-1.5, 1.2),
                humidity: humidity.clamp(-1.0, 1.0),
            },
            height,
            roughness: roughness * self.world_scale.sqrt(),
            river,
            island: 0.0,
            vscale: self.norm_scale,
            water: water_y,
        }
    }
}

/// Lakes of a procedural planet. Each hollow of the flooded terrain (cells
/// filled to the same rim) holds a lake whose size balances the water
/// flowing in against evaporation from its surface (open water evaporates
/// at the potential rate, less the rain on it): in a wet climate it fills
/// to the rim and spills, in a dry one it shrinks to a salt lake in the
/// deepest part, or dries out (the Caspian, the Dead Sea, Death Valley).
fn water_balance_lakes(
    elevation: &Grid<f32>,
    drainage: &erosion::Drainage,
    sea: &Grid<f32>,
    discharge: &[f32],
    clim: &climate::Climate,
    area_m2: f64,
) -> Grid<f32> {
    let (w, h) = (elevation.w, elevation.h);
    let filled = &drainage.filled;
    let mut lake = Grid::new(w, h, NO_LAKE);
    let hollow = |k: usize| sea.data[k] < 0.5 && filled[k] as f64 - elevation.data[k] as f64 > 2.0;
    let mut seen = vec![false; w * h];
    for start in 0..w * h {
        if seen[start] || !hollow(start) {
            continue;
        }
        // The hollow's cells.
        let rim = filled[start];
        let mut cells = vec![start];
        seen[start] = true;
        let mut k = 0;
        while k < cells.len() {
            let c = cells[k];
            k += 1;
            let (x, z) = ((c % w) as isize, (c / w) as isize);
            for &(dx, dz, _) in &grid::NEIGHBOURS8 {
                let (a, b) = (x + dx, z + dz);
                if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                    continue;
                }
                let j = a as usize + b as usize * w;
                if !seen[j] && hollow(j) && (filled[j] - rim).abs() < 1.0 {
                    seen[j] = true;
                    cells.push(j);
                }
            }
        }
        // Everything that drains into it passes its lowest outflow cell,
        // which carries the largest discharge (m³/s).
        let inflow = cells.iter().map(|&c| discharge[c] as f64).fold(0.0, f64::max);
        // Fill from the deepest cell up while the inflow covers the
        // evaporation of the growing surface.
        cells.sort_by(|&a, &b| elevation.data[a].total_cmp(&elevation.data[b]).then(a.cmp(&b)));
        let mut evaporation = 0.0;
        let mut level = rim as f64;
        let mut count = 0;
        for &c in &cells {
            let (p, t) = (clim.precipitation.data[c] as f64, clim.temperature.data[c] as f64);
            let net = (climate::potential_evaporation(t) - p).max(0.0) / 1000.0 * area_m2 / 3.156e7;
            if evaporation + net > inflow {
                level = elevation.data[c] as f64;
                break;
            }
            evaporation += net;
            count += 1;
        }
        // Too small to see on this grid, or too shallow to matter.
        if count < 2 || level - (elevation.data[cells[0]] as f64) < 10.0 {
            continue;
        }
        for &c in &cells[..count] {
            lake.data[c] = level as f32;
        }
    }
    lake
}

/// For the real Earth: how mountainous each cell is (local relief) and
/// the axis of its ranges (perpendicular to the slope of the smoothed
/// relief), since there are no plate boundaries to read them from.
fn mountains_from_relief(e: &Grid<f32>, km: f64) -> (Grid<f32>, Grid<f32>, Grid<f32>) {
    let (w, h) = (e.w, e.h);
    let land = e.map(|v| v.max(0.0));
    let r = ((60.0 / km).round() as usize).max(1);
    let mean = land.blur(r, 2);
    let sq = land.map(|v| v * v).blur(r, 2);
    let mut m = Grid::new(w, h, 0f32);
    for i in 0..w * h {
        let var = (sq.data[i] as f64 - (mean.data[i] as f64).powi(2)).max(0.0);
        let sd = var.sqrt();
        m.data[i] = (smoothstep(150.0, 900.0, sd) * 0.7 + 0.3 * smoothstep(1500.0, 4500.0, land.data[i] as f64)) as f32;
    }
    let smooth = land.blur(((150.0 / km).round() as usize).max(1), 3);
    let mut sc = Grid::new(w, h, 1f32);
    let mut ss = Grid::new(w, h, 0f32);
    for j in 0..h {
        for i in 0..w {
            let gx = smooth.at(i as isize + 1, j as isize) as f64 - smooth.at(i as isize - 1, j as isize) as f64;
            let gz = smooth.at(i as isize, j as isize + 1) as f64 - smooth.at(i as isize, j as isize - 1) as f64;
            let l = (gx * gx + gz * gz).sqrt();
            if l > 1e-6 {
                let (c, s) = (gx / l, gz / l);
                // Axis ⟂ gradient: 2θ_axis = 2θ_grad + 180°.
                sc.set(i, j, -((c * c - s * s) as f32));
                ss.set(i, j, -((2.0 * c * s) as f32));
            }
        }
    }
    (m, sc, ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{SimQuality, WorldSize};

    fn earth() -> RealisticWorld {
        RealisticWorld::new(&WorldSettings {
            generator: crate::settings::Generator::Realistic,
            planet: Planet::Earth,
            quality: SimQuality::Fast,
            size: WorldSize::from_km(40_000.0),
            ..Default::default()
        })
    }

    fn cell(r: &RealisticWorld, lat: f64, lon: f64) -> (isize, isize) {
        (((lon + 180.0) / 360.0 * r.w as f64) as isize, ((90.0 - lat) / 180.0 * r.h as f64) as isize)
    }

    /// Near the open sea, every column below sea level is under water:
    /// the coast follows the terrain, never a straight "wall" where the sea
    /// stops (the dry-depression rule once used a per-cell sea mask, so
    /// dry land below sea level appeared in rectangles along coasts).
    #[test]
    fn coasts_never_hold_back_the_sea() {
        for planet in [Planet::Continents, Planet::Islands, Planet::Earth] {
            let r = RealisticWorld::new(&WorldSettings {
                generator: crate::settings::Generator::Realistic,
                planet,
                quality: SimQuality::Fast,
                size: WorldSize::from_km(4_000.0),
                ..Default::default()
            });
            let (mut dry, mut checked) = (0, 0);
            let n = 700;
            let half = (r.w as f64 * r.cell_blocks / 2.0) as i64;
            let halfz = (r.h as f64 * r.cell_blocks / 2.0) as i64;
            for j in 0..n {
                for i in 0..n {
                    let x = (-half + 2 * half * i / n) as i32;
                    let z = (-halfz + 2 * halfz * j / n) as i32;
                    let s = r.shape(x, z);
                    if s.height >= SEA_LEVEL as f64 - 1.0 {
                        continue;
                    }
                    // Within one cell of the open sea.
                    let (u, v) = ((x as f64 - r.x0) / r.cell_blocks, (z as f64 - r.z0) / r.cell_blocks);
                    let near_sea = (-1..=1)
                        .any(|a| (-1..=1).any(|b| r.sea.at(u.floor() as isize + a, v.floor() as isize + b) > 0.5));
                    if near_sea {
                        checked += 1;
                        dry += (s.water < SEA_LEVEL) as usize;
                    }
                }
            }
            assert!(checked > 1000, "{planet:?}: only {checked} coastal columns");
            assert_eq!(dry, 0, "{planet:?}: {dry} of {checked} coastal columns below sea level are dry");
        }
    }

    /// The real Earth gets its real lakes at their real levels (the
    /// Caspian below sea level), and no lakes where the coarse relief only
    /// looks closed (the Congo basin).
    #[test]
    fn earth_has_its_real_lakes() {
        let r = earth();
        let lake_near = |lat: f64, lon: f64| {
            let (i, j) = cell(&r, lat, lon);
            (-2..=2)
                .flat_map(|a| (-2..=2).map(move |b| (a, b)))
                .map(|(a, b)| r.lake.at(i + a, j + b))
                .fold(NO_LAKE, f32::max)
        };
        let caspian = lake_near(42.0, 50.5);
        assert!((-40.0..-20.0).contains(&caspian), "Caspian level {caspian}");
        let superior = lake_near(47.7, -87.5);
        assert!((150.0..200.0).contains(&superior), "Lake Superior level {superior}");
        assert!(lake_near(-1.0, 33.0) > 1000.0, "Lake Victoria");
        assert_eq!(lake_near(0.0, 18.0), NO_LAKE, "no lake in the Congo basin");
        // Rivers: the simulation finds a large network.
        assert!(r.river_count() > 500, "{} river reaches", r.river_count());
    }

    /// Latitude runs from 90° N at the top edge to 90° S at the bottom.
    #[test]
    fn latitude_spans_the_planet() {
        let r = earth();
        let half = (r.cell_blocks * r.h as f64 / 2.0) as i32;
        assert!((r.latitude(-half) - 90.0).abs() < 1e-6);
        assert!(r.latitude(0).abs() < 1e-6);
        assert!((r.latitude(half) + 90.0).abs() < 1e-6);
    }
}
