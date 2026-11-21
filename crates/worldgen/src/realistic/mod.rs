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
//! Beyond the plates, the column shaping adds landforms with a reason of
//! their own:
//!
//! * **Hills** away from the active ranges (`build_hills`): foothill
//!   belts, worn-down old ranges, rolling uplands, and dune fields in hot
//!   dry interiors, asymmetric ridges across the trade winds or the
//!   westerlies.
//! * **Islands** in rounds: the plates make continents and arcs, then
//!   [`tectonics`] adds large islands off the coasts (Japan, Cuba,
//!   Greenland) and medium ones with their own small cordillera (Taiwan,
//!   Puerto Rico, the Falklands), and `build_islets` the small islands of
//!   the open ocean below the grid's resolution (hotspot volcanoes, worn
//!   hills, coral atolls).
//! * **Themes** on the Mushoku planet ([`theme`]): every continent ruled
//!   by one biome.
//!
//! The river network is a tree: every cell drains to one receiver, so
//! tributaries join their main river, rivers never cross (crossing
//! diagonals are rerouted), every river ends at the sea or in a lake, and
//! discharges range from creeks to Amazon-sized rivers.
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
pub mod theme;

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

/// A small island of the open ocean, below the grid's resolution.
#[derive(Clone, Copy, Debug)]
struct Islet {
    /// Centre, grid coordinates.
    u: f64,
    v: f64,
    /// Radius at sea level (the rim, for an atoll), km.
    r: f64,
    /// Summit, m.
    top: f64,
    kind: IsletKind,
    /// Offset into the coast noise, so no two islets share an outline.
    salt: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum IsletKind {
    /// A young volcano; `true` with a summit crater.
    Volcano(bool),
    /// An older, worn island: a gentle hill.
    Hill,
    /// A ring of coral reef and sandy islets around a lagoon.
    Atoll,
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
        (SimQuality::Fast, false) => (384, 6, 150.0),
        (SimQuality::Balanced, false) => (768, 10, 40.0),
        (SimQuality::Quality, false) => (1536, 14, 12.0),
        (SimQuality::Fast, true) => (1200, 0, 150.0),
        (SimQuality::Balanced, true) => (1800, 0, 40.0),
        (SimQuality::Quality, true) => (3600, 0, 12.0),
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
    /// How fast the biome temperature and humidity change, per km (from
    /// the grid; the finer weather noise is added in `shape`).
    grad_t: Grid<f32>,
    grad_h: Grid<f32>,
    /// Lake surface (m), or `NO_LAKE`.
    lake: Grid<f32>,
    /// 1 where a cell is the open sea (connected to the world ocean).
    sea: Grid<f32>,
    /// 1 where a cell is land below sea level cut off from the open sea
    /// (Dead Sea, Caspian depression): it stays dry unless it is a lake.
    basin: Grid<f32>,
    /// Hills independent of the active ranges (m, at most): foothills,
    /// worn-down old ranges and rolling uplands. See `build_hills`.
    hill_amp: Grid<f32>,
    /// Share of those hills that are ridged (old ranges) rather than rounded.
    hill_ridge: Grid<f32>,
    /// Height of the dunes of sand seas (m).
    dune_amp: Grid<f32>,
    /// Small islands of the open ocean, bucketed by `ISLET_BUCKET` cells
    /// (compressed rows: `islet_start[b]..islet_start[b + 1]` index
    /// `islet_list`).
    islets: Vec<Islet>,
    islet_start: Vec<u32>,
    islet_list: Vec<u32>,
    /// Mushoku: the climate range each column allows (t_lo, t_hi, h_lo,
    /// h_hi), and each continent's theme.
    theme: Option<ThemeRanges>,

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
    /// Noise of the hills and dunes (their own octaves, so they don't
    /// repeat the mountains' detail).
    hill_octaves: Vec<ImprovedPerlin>,
    spawn: (i32, i32),
}

const NO_LAKE: f32 = -1.0e9;
/// Mushoku: the climate range of every cell (t_lo, t_hi, h_lo, h_hi) and
/// each continent's (theme, cells).
type ThemeRanges = ([Grid<f32>; 4], Vec<(usize, usize)>);
/// Cells per side of an islet bucket.
const ISLET_BUCKET: usize = 8;

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

        // Mushoku: each continent's theme (from the tectonic relief, whose
        // coasts erosion hardly moves).
        let themes = (settings.planet == Planet::Mushoku).then(|| theme::ThemeMap::new(seed, &target, km));

        // --- Erosion, driven by a first estimate of the rain --------------
        if steps > 0 {
            let mut c = climate::simulate(&elevation, km, seed, 6.0);
            if let Some(t) = &themes {
                t.apply(&mut c);
            }
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
        let mut clim = climate::simulate(&elevation, km, seed, if earth { 0.0 } else { 6.0 });
        if let Some(t) = &themes {
            t.apply(&mut clim);
        }
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
        let lake = if earth {
            earth::lakes(&elevation, &drainage.filled, &sea.data)
        } else {
            let inflow = drainage.accumulate(&runoff);
            water_balance_lakes(&elevation, &drainage, &sea, &inflow, &clim, area_m2, q_min)
        };
        // Rivers follow the drainage once the lakes are known: a lake that
        // doesn't fill its hollow keeps the water (the Caspian, Lake Chad).
        let routing = route_water(&elevation, &drainage, &lake, &sea);
        let discharge = routing.accumulate(&runoff);

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
            grad_t: gradient(&clim.temperature.map(|t| climate::biome_temperature(t as f64) as f32), km),
            grad_h: gradient(&clim.aridity.map(|a| climate::biome_humidity(a as f64) as f32), km),
            temperature: clim.temperature,
            humidity: clim.aridity.map(|a| climate::biome_humidity(a as f64) as f32),
            lake,
            basin,
            sea,
            hill_amp: Grid::new(w, h, 0.0),
            hill_ridge: Grid::new(w, h, 0.0),
            dune_amp: Grid::new(w, h, 0.0),
            islets: Vec::new(),
            islet_start: Vec::new(),
            islet_list: Vec::new(),
            theme: themes.map(|t| ([t.t_lo, t.t_hi, t.h_lo, t.h_hi], t.continents)),
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
            hill_octaves: {
                let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/hills"));
                (0..10).map(|_| ImprovedPerlin::new(&mut rng)).collect()
            },
            spawn: (0, 0),
        };
        world.set_vertical_scale(&settings);
        world.build_rivers(seed, &routing, &discharge, q_min);
        world.build_hills(seed);
        world.build_islets(seed, settings.planet);
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
    /// becomes a node joined to its receiver, so tributaries meet their
    /// main river at a shared node and the network is a tree from the
    /// smallest creeks to the mouth. Nodes are jittered inside their cell
    /// so rivers don't run along grid lines. Inside a lake there is no
    /// river (it enters at one shore and leaves at the outlet); at the
    /// sea the last reach runs on past the coast cell, so the river
    /// always meets the actual (warped) shoreline.
    fn build_rivers(&mut self, seed: u64, routing: &Routing, q: &[f32], q_min: f64) {
        let (w, h) = (self.w, self.h);
        let mut node_of = vec![u32::MAX; w * h];
        let jitter = derive_seed(seed, "realistic/river_jitter");
        let mut node = |i: usize, nodes: &mut Vec<RiverNode>| -> u32 {
            if node_of[i] == u32::MAX {
                let (ci, cj) = ((i % w) as i32, (i / w) as i32);
                let r = hash_position(jitter, ci, 0, cj);
                // ±0.2 cells: enough to break the grid lines, too little
                // for neighbouring parallel rivers to cross.
                let ju = ((r & 0xffff) as f64 / 65535.0 - 0.5) * 0.4;
                let jv = (((r >> 16) & 0xffff) as f64 / 65535.0 - 0.5) * 0.4;
                let level = routing.level[i] as f64;
                nodes.push(RiverNode { u: ci as f64 + 0.5 + ju, v: cj as f64 + 0.5 + jv, level, q: q[i] as f64 });
                node_of[i] = (nodes.len() - 1) as u32;
            }
            node_of[i]
        };
        let lake = |i: usize| self.lake.data[i] > NO_LAKE;
        let mut nodes = Vec::new();
        let mut reaches = Vec::new();
        let mut has_inflow = Vec::new();
        let mut extended = vec![false; w * h];
        for &c in &routing.order {
            let c = c as usize;
            let r = routing.receiver[c] as usize;
            if r == c || (q[c] as f64) < q_min || self.sea.data[c] > 0.5 || (lake(c) && lake(r)) {
                continue;
            }
            let a = node(c, &mut nodes);
            let b = node(r, &mut nodes);
            has_inflow.resize(nodes.len(), false);
            has_inflow[b as usize] = true;
            reaches.push(Reach { a, b });
            if self.sea.data[r] > 0.5 && !extended[r] {
                // Out to sea: one more cell along the river's direction
                // (once per mouth: rivers meeting at the coast share it).
                extended[r] = true;
                let (na, nb) = (nodes[a as usize], nodes[b as usize]);
                let (du, dv) = (nb.u - na.u, nb.v - na.v);
                let l = (du * du + dv * dv).sqrt().max(1e-6);
                nodes.push(RiverNode { u: nb.u + du / l, v: nb.v + dv / l, level: 0.0, q: nb.q });
                has_inflow.push(true);
                reaches.push(Reach { a: b, b: (nodes.len() - 1) as u32 });
            }
        }
        // Sources start as creeks and widen downstream.
        has_inflow.resize(nodes.len(), false);
        for (n, inflow) in nodes.iter_mut().zip(&has_inflow) {
            if !inflow {
                n.q = n.q.min(0.08 * q_min);
            }
        }
        // A downstream node's level never exceeds its upstream one (the
        // routing guarantees it for cells; jitter can't change it).
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

    /// Where hills and dunes rise away from the active mountain ranges,
    /// each for a geological reason:
    ///
    /// * **foothills**: the outer belt of a range, where the crust is
    ///   folded and thrust but not lifted far (the Siwaliks, the Alpine
    ///   foreland);
    /// * **old ranges**: long belts of mountains worn down over hundreds of
    ///   millions of years to ridged hills a few hundred metres high (the
    ///   Appalachians, the Urals, the Scottish Highlands);
    /// * **rolling uplands**: dissected plateaus and old eroded surfaces
    ///   (the Ozarks, Tuscany, the Palouse);
    /// * **sand seas**: in hot, dry interiors the wind piles sand into
    ///   dunes (the Sahara's ergs, the Rub' al Khali).
    ///
    /// Hills fade out towards the coast, so they never move the shoreline.
    fn build_hills(&mut self, seed: u64) {
        let (w, h, km) = (self.w, self.h, self.km);
        let zones = OctaveNoise::new(derive_seed(seed, "realistic/hill_zones"), &OctaveConfig::new(0, &[1.0, 0.5]));
        let sea: Vec<usize> = (0..w * h).filter(|&i| self.sea.data[i] > 0.5).collect();
        let (coast, _) = grid::distance_field(w, h, &sea, 400.0 / km + 2.0, |_, _| true);
        // How close the active ranges are, ~400 km around them.
        let upland = self.mountain.blur((300.0 / km).round().max(1.0) as usize, 3);
        for (i, &coast) in coast.iter().enumerate() {
            let (xk, zk) = (((i % w) as f64 + 0.5) * km, ((i / w) as f64 + 0.5) * km);
            let base = self.elevation.data[i] as f64;
            if self.sea.data[i] > 0.5 || self.lake.data[i] > NO_LAKE || base < 0.0 {
                continue;
            }
            let inland = smoothstep(40.0, 160.0, coast as f64 * km);
            let m = self.mountain.data[i] as f64;
            let calm = 1.0 - smoothstep(0.2, 0.5, m);
            let foot = smoothstep(0.02, 0.12, upland.data[i] as f64) * calm;
            // Old ranges: the zero lines of a large noise, where a second
            // noise allows (so belts start and end).
            let belt_n = zones.sample_2d(xk / 2600.0 + 3.1, zk / 2600.0 - 7.7);
            let belt = (1.0 - smoothstep(0.02, 0.1, belt_n.abs()))
                * smoothstep(-0.05, 0.2, zones.sample_2d(xk / 1800.0 - 11.0, zk / 1800.0 + 5.0))
                * calm;
            let rolling = smoothstep(0.1, 0.4, zones.sample_2d(xk / 900.0 + 21.0, zk / 900.0 + 13.0));
            // Sand seas: hot and dry lowlands, in patches.
            let hum = self.humidity.data[i] as f64;
            let temp = climate::biome_temperature(self.temperature.data[i] as f64);
            let erg = smoothstep(-0.48, -0.62, hum)
                * smoothstep(0.15, 0.3, temp)
                * (1.0 - smoothstep(600.0, 1500.0, base))
                * calm
                * smoothstep(-0.25, 0.1, zones.sample_2d(xk / 700.0 - 31.0, zk / 700.0 + 17.0));
            let (f, o, r) = (280.0 * foot, 420.0 * belt, 160.0 * rolling);
            let amp = f.max(o).max(r) * (1.0 - 0.8 * erg);
            self.hill_amp.data[i] = (inland * amp) as f32;
            self.hill_ridge.data[i] = (o / (f + o + r).max(1e-6)) as f32;
            self.dune_amp.data[i] = (inland * 80.0 * erg) as f32;
        }
    }

    /// The last round of islands: the small islands of the open ocean, far
    /// below the grid's resolution, in the two patterns the Pacific shows.
    ///
    /// * **Hotspot chains** (Hawaii, the Society Islands): a plume under a
    ///   moving plate leaves a line of volcanoes. The youngest, over the
    ///   plume, is a tall cone (some with a crater); down the chain they
    ///   are older, worn to gentle hills, and in the tropics the oldest
    ///   have sunk until only their coral reef is left: atolls (Darwin's
    ///   sequence).
    /// * **Coral archipelagos** (the Marshall Islands, Tuvalu, Kiribati):
    ///   clusters of atolls, rings of sandy islets around a lagoon, only a
    ///   few metres above the sea. Coral needs warm water, so they lie
    ///   within ~28° of the equator.
    /// * Outside the tropics, **volcanic clusters** (the Azores, the
    ///   Canaries): volcanoes and worn hills.
    ///
    /// All stand in deep open water, away from the continents and their
    /// shelves.
    fn build_islets(&mut self, seed: u64, planet: Planet) {
        use std::f64::consts::TAU;
        let groups = match planet {
            Planet::Earth => return,
            Planet::Islands => 40,
            Planet::Pangea => 30,
            _ => 26,
        };
        let (w, h, km) = (self.w, self.h, self.km);
        let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/islets"));
        let shallow: Vec<usize> = (0..w * h).filter(|&i| self.elevation.data[i] > -1500.0).collect();
        let (far, _) = grid::distance_field(w, h, &shallow, 600.0 / km + 1.0, |_, _| true);
        let ok = |u: f64, v: f64| {
            let (i, j) = (u.floor() as isize, v.floor() as isize);
            if i < 2 || j < 2 || i >= w as isize - 2 || j >= h as isize - 2 {
                return false;
            }
            let c = i as usize + j as usize * w;
            self.sea.data[c] > 0.5 && far[c] as f64 * km >= 150.0
        };
        let r = |rng: &mut Xoroshiro128PlusPlus, a: f64, b: f64| a + (b - a) * rng.next_f64();
        let mut islets = Vec::new();
        for _ in 0..groups {
            let Some((u, v)) = (0..40).find_map(|_| {
                let (u, v) = (rng.next_f64() * w as f64, rng.next_f64() * h as f64);
                ok(u, v).then_some((u, v))
            }) else {
                continue;
            };
            let lat = 90.0 - v / h as f64 * 180.0;
            let tropical = lat.abs() < 28.0;
            let a = rng.next_f64() * TAU;
            let (dx, dz) = (libm::cos(a), libm::sin(a));
            let mut add = |u: f64, v: f64, r: f64, top: f64, kind: IsletKind, rng: &mut Xoroshiro128PlusPlus| {
                if ok(u, v) {
                    islets.push(Islet { u, v, r, top, kind, salt: rng.next_f64() * 1000.0 });
                }
            };
            let style = rng.next_f64();
            if tropical && style < 0.55 {
                // Coral archipelago: atolls scattered along a band.
                let (len, count) = (r(&mut rng, 300.0, 1200.0), 6 + rng.next_bounded(15));
                for _ in 0..count {
                    let (t, s) = ((rng.next_f64() - 0.5) * len, (rng.next_f64() - 0.5) * 160.0);
                    let rad = libm::exp(r(&mut rng, libm::log(1.0), libm::log(18.0)));
                    add(u + (dx * t - dz * s) / km, v + (dz * t + dx * s) / km, rad, 3.0, IsletKind::Atoll, &mut rng);
                }
            } else if style < 0.8 {
                // Hotspot chain, youngest first.
                let count = 5 + rng.next_bounded(10);
                let mut top = r(&mut rng, 1200.0, 2600.0);
                let mut along = 0.0;
                for k in 0..count {
                    let (t, s) = (along, (rng.next_f64() - 0.5) * 50.0);
                    let (iu, iv) = (u + (dx * t - dz * s) / km, v + (dz * t + dx * s) / km);
                    if top > 350.0 {
                        let rad = r(&mut rng, 8.0, 30.0) * (top / 2600.0).sqrt().max(0.5);
                        add(iu, iv, rad, top, IsletKind::Volcano(rng.next_f64() < 0.4), &mut rng);
                    } else if top > 50.0 {
                        add(iu, iv, r(&mut rng, 4.0, 14.0), top.max(60.0), IsletKind::Hill, &mut rng);
                    } else if tropical {
                        add(iu, iv, r(&mut rng, 2.0, 12.0), 3.0, IsletKind::Atoll, &mut rng);
                    }
                    top *= if k == 0 { 0.75 } else { r(&mut rng, 0.45, 0.75) };
                    along += r(&mut rng, 40.0, 120.0);
                }
            } else {
                // Volcanic cluster.
                let count = 4 + rng.next_bounded(6);
                for _ in 0..count {
                    let (t, s) = ((rng.next_f64() - 0.5) * 450.0, (rng.next_f64() - 0.5) * 250.0);
                    let (iu, iv) = (u + (dx * t - dz * s) / km, v + (dz * t + dx * s) / km);
                    if rng.next_f64() < 0.5 {
                        let top = r(&mut rng, 500.0, 2300.0);
                        add(iu, iv, r(&mut rng, 6.0, 22.0), top, IsletKind::Volcano(rng.next_f64() < 0.3), &mut rng);
                    } else {
                        let top = r(&mut rng, 60.0, 300.0);
                        add(iu, iv, r(&mut rng, 3.0, 12.0), top, IsletKind::Hill, &mut rng);
                    }
                }
            }
        }
        // Buckets: every bucket a flank can reach.
        let (bw, bh) = (w.div_ceil(ISLET_BUCKET), h.div_ceil(ISLET_BUCKET));
        let mut lists = vec![Vec::new(); bw * bh];
        for (k, isl) in islets.iter().enumerate() {
            let reach = Self::islet_reach(isl) / km;
            let (b0, b1) = (
                ((isl.u - reach).max(0.0) as usize) / ISLET_BUCKET,
                ((isl.u + reach) as usize / ISLET_BUCKET).min(bw - 1),
            );
            let (c0, c1) = (
                ((isl.v - reach).max(0.0) as usize) / ISLET_BUCKET,
                ((isl.v + reach) as usize / ISLET_BUCKET).min(bh - 1),
            );
            for c in c0..=c1 {
                for b in b0..=b1 {
                    lists[b + c * bw].push(k as u32);
                }
            }
        }
        self.islet_start = std::iter::once(0)
            .chain(lists.iter().scan(0u32, |acc, l| {
                *acc += l.len() as u32;
                Some(*acc)
            }))
            .collect();
        self.islet_list = lists.concat();
        self.islets = islets;
    }

    /// How far (km) an islet's submarine flanks reach.
    fn islet_reach(isl: &Islet) -> f64 {
        isl.r * 3.5 + 4.0
    }

    /// The highest islet surface at a point (m) and whether it is an
    /// atoll's (whose low rim and lagoon must not be roughened).
    fn islet(&self, u: f64, v: f64) -> Option<(f64, bool)> {
        if self.islets.is_empty() || u < 0.0 || v < 0.0 {
            return None;
        }
        let (bw, bh) = (self.w.div_ceil(ISLET_BUCKET), self.h.div_ceil(ISLET_BUCKET));
        let (b, c) = (u as usize / ISLET_BUCKET, v as usize / ISLET_BUCKET);
        if b >= bw || c >= bh {
            return None;
        }
        let k = b + c * bw;
        let mut best: Option<(f64, bool)> = None;
        for &i in &self.islet_list[self.islet_start[k] as usize..self.islet_start[k + 1] as usize] {
            let isl = &self.islets[i as usize];
            let (dx, dz) = ((u - isl.u) * self.km, (v - isl.v) * self.km);
            let d = (dx * dx + dz * dz).sqrt();
            if d > Self::islet_reach(isl) {
                continue;
            }
            // Coast: bays and capes on the island's own scale.
            let (xk, zk) = (u * self.km / (0.7 * isl.r), v * self.km / (0.7 * isl.r));
            let r = isl.r * (1.0 + 0.22 * self.variety.sample_2d(xk + isl.salt, zk - isl.salt));
            // Submarine flank: down to the abyss within ~2.5 radii.
            let flank = |x: f64| -x.max(0.0) / (2.5 * isl.r) * 4500.0;
            let e = match isl.kind {
                IsletKind::Volcano(crater) => {
                    if d < r {
                        let x = 1.0 - d / r;
                        let mut e = isl.top * x.powf(1.6);
                        if crater {
                            e -= 0.12 * isl.top * crate::spline::smoothstep(0.88, 0.97, x);
                        }
                        // Lava ridges and gullies down the flanks.
                        e + 0.1
                            * isl.top
                            * x.sqrt()
                            * (1.0 - x)
                            * self.variety.sample_2d(xk * 4.0 - 3.0, zk * 4.0 + 5.0)
                            + 0.04 * isl.top * x * self.variety.sample_2d(xk * 12.0 + 7.0, zk * 12.0 - 2.0)
                    } else {
                        flank(d - r)
                    }
                }
                IsletKind::Hill => {
                    if d < r {
                        let x = d / r;
                        let n = self.variety.sample_2d(xk * 2.5 + 9.0, zk * 2.5 - 1.0);
                        isl.top * (1.0 - x * x) * (1.0 + 0.4 * n)
                    } else {
                        flank(d - r)
                    }
                }
                IsletKind::Atoll => {
                    // The rim: sandy islets (motus) where the noise allows,
                    // a reef just awash between them.
                    let wr = (0.12 * isl.r).max(0.35);
                    // A few metres, but at least a block or two in a world
                    // whose heights are scaled down.
                    let mb = 1.0 / self.v_scale.max(1e-9);
                    let (sand, awash) = (1.2f64.max(0.9 * mb), -(1.5f64.max(1.2 * mb)));
                    let x = d - isl.r * (1.0 + 0.06 * self.variety.sample_2d(xk + 41.0, zk));
                    let motu = smoothstep(-0.12, 0.05, self.variety.sample_2d(xk * 3.0 - 17.0, zk * 3.0 + 23.0));
                    let rim = lerp(motu, awash, sand * (1.0 + 0.6 * (1.0 - x.abs() / wr).max(0.0)));
                    let off = if x < 0.0 {
                        // Lagoon, deepening towards the middle.
                        -(8.0f64.max(2.0 * mb) + 32.0 * smoothstep(0.0, 0.5 * isl.r, -x - wr))
                    } else {
                        awash + flank(x - wr)
                    };
                    lerp(smoothstep(wr, 0.6 * wr, x.abs()), off, rim)
                }
            };
            if best.is_none_or(|(b, _)| e > b) {
                best = Some((e, isl.kind == IsletKind::Atoll));
            }
        }
        best
    }

    /// Hills (m, never below 0) on top of the simulated terrain: rounded
    /// or, on old ranges, ridged noise with wavelengths of ~8 km down to
    /// the block scale.
    fn hills(&self, xk: f64, zk: f64, amp: f64, ridged: f64, min_km: f64) -> f64 {
        // cos, sin of 0.5 + 1.1·k radians.
        const ROT: [(f64, f64); 7] = [
            (0.877_582_6, 0.479_425_5),
            (-0.029_199_5, 0.999_573_6),
            (-0.904_072_1, 0.427_379_9),
            (-0.790_967_7, -0.611_857_9),
            (0.186_512_4, -0.982_452_6),
            (0.960_170_3, -0.279_415_5),
            (0.684_546_7, 0.728_969_0),
        ];
        let (mut sum, mut norm, mut a, mut lambda) = (0.0, 0.0, 1.0, 8.0);
        let mut k = 0;
        while lambda >= min_km && k < 7 {
            // Each octave turned by its own fixed angle: ridged Perlin
            // noise otherwise shows the creases of its lattice.
            let (c, s) = ROT[k];
            let (x, z) = ((xk * c - zk * s) / lambda, (xk * s + zk * c) / lambda);
            let n = self.hill_octaves[k].sample(wrap(x), 0.31 + 0.17 * k as f64, wrap(z));
            let r = 1.0 - (n.abs() * 1.6).min(1.0);
            sum += a * lerp(ridged, n, 2.0 * r * r - 0.6);
            norm += a;
            a *= 0.55;
            lambda *= 0.5;
            k += 1;
        }
        if norm == 0.0 {
            return 0.0;
        }
        // Flat floors between rounded hills.
        let v = ((sum / norm + 0.3) / 0.9).clamp(0.0, 1.5);
        amp * v * v.sqrt()
    }

    /// Dunes (m): asymmetric ridges across the wind (a long windward
    /// slope, a steep slip face), wavy and broken into segments, on a
    /// swell of larger "draa" ridges. The crests keep a fixed set of four
    /// directions, blended by the local wind: turning the coordinates by
    /// an angle that varies across the map would swirl the pattern (see
    /// `detail`). Where two directions mix, the pattern crosses into
    /// star dunes.
    fn dunes(&self, xk: f64, zk: f64, amp: f64, lat: f64) -> f64 {
        use std::f64::consts::{FRAC_1_SQRT_2, PI};
        const DIRS: [(f64, f64); 4] =
            [(1.0, 0.0), (FRAC_1_SQRT_2, FRAC_1_SQRT_2), (0.0, 1.0), (-FRAC_1_SQRT_2, FRAC_1_SQRT_2)];
        // Trade winds blow from the north-east (south-east in the south)
        // below 30°, westerlies above; the noise turns them locally.
        let wind = if lat.abs() < 30.0 { PI + lat.signum() * 0.6 } else { 0.0 }
            + 0.9 * self.hill_octaves[7].sample(wrap(xk / 600.0), 0.5, wrap(zk / 600.0));
        let (wc, ws) = (libm::cos(wind), libm::sin(wind));
        // Ridge spacing: about 1.2 km, with mega-ridges five times wider.
        let lambda = 1.2;
        let (rx, rz) = (0.8 * xk - 0.6 * zk, 0.6 * xk + 0.8 * zk);
        let warp = 0.9 * self.hill_octaves[8].sample(wrap(rx / 3.0), 0.7, wrap(rz / 3.0));
        let segments = smoothstep(-0.35, 0.3, self.hill_octaves[9].sample(wrap(rz / 1.7), 0.2, wrap(rx / 1.7)));
        let (mut sum, mut wsum) = (0.0, 0.0);
        for (dx, dz) in DIRS {
            // Weight: how well this direction lines up with the wind
            // (crests are across it); direction is unsigned, so the angle
            // is doubled.
            let c2 = (dx * wc + dz * ws).powi(2) * 2.0 - 1.0;
            let wt = c2.max(0.0).powi(3);
            if wt < 1e-4 {
                continue;
            }
            let s = (xk * dx + zk * dz) / lambda + warp;
            // The wind blows along +direction; the slip face is downwind.
            let sign = if dx * wc + dz * ws >= 0.0 { 1.0 } else { -1.0 };
            let f = (sign * s).rem_euclid(1.0);
            let p = if f < 0.85 { f / 0.85 } else { (1.0 - f) / 0.15 };
            let big = (sign * (s / 5.0 + 0.37)).rem_euclid(1.0);
            let pb = if big < 0.8 { big / 0.8 } else { (1.0 - big) / 0.2 };
            sum += wt * (p * p * (3.0 - 2.0 * p) * (0.3 + 0.7 * segments) + 0.6 * pb * pb * (3.0 - 2.0 * pb));
            wsum += wt;
        }
        if wsum == 0.0 {
            return 0.0;
        }
        amp * sum / wsum / 1.6
    }

    /// River channel width (blocks) for a discharge (m³/s): hydraulic
    /// geometry, w ≈ 4·Q^0.5 m (the Amazon ~1.8 km, a 200 m³/s river ~55 m).
    fn channel_width(&self, q: f64) -> f64 {
        (4.0 * q.sqrt() * self.h_scale).clamp(3.0, 2500.0)
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
    /// Blocks per km of the virtual planet, horizontally.
    pub fn blocks_per_km(&self) -> f64 {
        self.cell_blocks / self.km
    }
    /// Latitude (degrees, + north) of a block row.
    pub fn latitude(&self, z: i32) -> f64 {
        90.0 - (z as f64 - self.z0) / (self.cell_blocks * self.h as f64) * 180.0
    }
    /// Mean annual temperature (°C, at the column's height) and rainfall
    /// (mm a year) of a column, in real units for the HUD and the map:
    /// the inverse of the scales the biome rules use.
    pub fn climate_at(&self, shape: &Shape, surface_y: i32) -> (f64, f64) {
        let t = climate::temperature_from_biome(crate::biome::effective_temperature(shape, surface_y));
        let aridity = climate::aridity_from_biome(shape.climate.humidity);
        (t, aridity * climate::potential_evaporation(t))
    }
    /// "18 °C · 850 mm/yr": the column's climate in real units.
    pub fn describe_climate(&self, shape: &Shape, surface_y: i32) -> String {
        let (t, p) = self.climate_at(shape, surface_y);
        format!("{t:.0} °C · {:.0} mm/yr", (p / 10.0).round() * 10.0)
    }
    /// Number of river reaches (for tests and statistics).
    pub fn river_count(&self) -> usize {
        self.reaches.len()
    }
    /// Summary of the river network: reaches, sources, the largest
    /// discharge and how many reaches fall in each size class.
    pub fn river_stats(&self) -> String {
        let max_q = self.nodes.iter().map(|n| n.q).fold(0.0, f64::max);
        let mut classes = [0usize; 6];
        for r in &self.reaches {
            let q = self.nodes[r.b as usize].q;
            let k = match q {
                q if q < 100.0 => 0,
                q if q < 1_000.0 => 1,
                q if q < 5_000.0 => 2,
                q if q < 20_000.0 => 3,
                q if q < 100_000.0 => 4,
                _ => 5,
            };
            classes[k] += 1;
        }
        format!(
            "{} reaches, largest {:.0} m³/s; reaches by m³/s <100: {}, <1k: {}, <5k: {}, <20k: {}, <100k: {}, more: {}",
            self.reaches.len(),
            max_q,
            classes[0],
            classes[1],
            classes[2],
            classes[3],
            classes[4],
            classes[5]
        )
    }
    /// Mushoku: each continent's theme name and size (grid cells), west
    /// to east.
    pub fn continent_themes(&self) -> Vec<(&'static str, usize)> {
        self.theme
            .as_ref()
            .map(|(_, c)| c.iter().map(|&(t, n)| (theme::THEMES[t].name, n)).collect())
            .unwrap_or_default()
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
        // Hills and dunes away from the ranges. Their slopes stay within
        // what the horizontal scale allows: in a small world (its height
        // exaggerated) a 1 km dune would otherwise be a spike.
        let steep = (self.h_scale / self.v_scale.max(1e-9)).min(1.0);
        let hill_amp = self.hill_amp.bicubic(wu, wv).max(0.0) * steep.sqrt();
        if hill_amp > 1.0 {
            let ridged = self.hill_ridge.bilinear(wu, wv).clamp(0.0, 1.0);
            e += fade * self.hills(xk, zk, hill_amp, ridged, min_km);
        }
        let dune_amp = self.dune_amp.bicubic(wu, wv).max(0.0) * steep;
        if dune_amp > 1.0 && min_km < 0.25 {
            let lat = 90.0 - v0 / self.h as f64 * 180.0;
            e += fade * smoothstep(0.25, 0.1, min_km) * self.dunes(xk, zk, dune_amp, lat);
        }
        // Small islands of the open ocean.
        let islet = self.islet(u0, v0).filter(|&(ie, _)| ie > e);
        if let Some((ie, _)) = islet {
            e = ie;
        }
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
        // Largest 3D-noise amplitude (final blocks) water allows here: the
        // noise must not lift a river or lake bed above its water, nor sink
        // a bank below it (the water would stop at a hole in the bank).
        // An atoll's rim stands a few blocks above the sea: no 3D noise.
        let mut rough_cap = if islet.is_some_and(|(_, atoll)| atoll) { 0.2 } else { f64::INFINITY };
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
                rough_cap = 0.3;
            } else {
                // Shore: the land eases to just above the water, so the
                // lake never shows as a wall or spills.
                height = lerp(smoothstep(0.1, 0.5, mask), height, ly + 1.0);
                rough_cap = 0.15 + 0.3 * (height - ly).max(0.0);
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
        if basin > 0.25 && lake.is_none_or(|(_, m)| m < 0.5) {
            // The terrain between grid cells can still dip below the sea
            // across that contour, so the basin gets a rim: just outside
            // it the land is lifted to sea level, and the sea and the dry
            // depression never meet.
            let rim = smoothstep(0.25, 0.4, basin) * (1.0 - smoothstep(0.5, 0.65, basin));
            height = height.max(lerp(rim, height, SEA_LEVEL as f64 + 1.0));
            if basin > 0.5 && height < SEA_LEVEL as f64 {
                water = f64::NEG_INFINITY;
            }
        }

        // A river ends where it meets standing water: in a lake or the sea
        // its banks would stand up out of the water as strips of land.
        let open_water = height < water;
        if let (Some((dist, level, width, depth, q)), false) = (near_river, open_water) {
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
                rough_cap = rough_cap.min(0.15 + 0.3 * (height - wy).max(0.0));
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
        // Finer weather (valleys, slopes facing the sun, lakes): ~1 °C and
        // a few % of rain over tens of km. Together with bicubic
        // interpolation it keeps biome borders, which follow these fields,
        // from running in the straight segments of the simulation grid.
        let fine = self.variety.sample_2d(xk / 150.0 + 5.0, zk / 150.0 - 3.0)
            + 0.5 * self.variety.sample_2d(xk / 18.0 - 11.0, zk / 18.0 + 7.0);
        let t_c = self.temperature.bicubic(wu, wv) + 3.5 * t_noise + 1.2 * fine;
        let alt_norm = (e.max(0.0) / M_PER_NORMALIZED_BLOCK - 15.0).max(0.0);
        let temperature = climate::biome_temperature(t_c) - 0.49 * alt_norm / 160.0;
        let h_noise = self.variety.sample_2d(xk / 1200.0 - 13.0, zk / 1200.0 + 29.0) + 0.8 * fine;
        let mut humidity = self.humidity.bicubic(wu, wv) + 0.09 * h_noise;
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
        // An islet's shallows are coastal water, not the deep ocean around.
        let shelf = islet.map_or(base, |(ie, _)| base.max(ie.min(0.0) - 50.0));
        let continentalness = CONT.with(|c| c.eval(shelf.clamp(-7000.0, 5000.0)));
        let mut erosion = (0.5 - 1.5 * m + 0.25 * w).clamp(-1.0, 1.0);
        if islet.is_some_and(|(_, atoll)| atoll) {
            // Coral sand on the motus, not the mangrove flats of a
            // sheltered continental coast.
            erosion = erosion.min(0.0);
        }

        // Biome transitions. The dither offsets the climate by up to
        // DITHER_T / DITHER_H, and a planet's climate changes slowly: that
        // would be zones ~100 km wide covering most of the land. Scale the
        // dither so a zone reaches at most ~8 km to each side of a border
        // (the generator varies it from there). The finer weather noise
        // above adds ~8e-4 per km to the grid's own gradient.
        let reach_km = 8.0;
        let g_t = (self.grad_t.bilinear(wu, wv).powi(2) + 8.0e-4f64.powi(2)).sqrt();
        let g_h = (self.grad_h.bilinear(wu, wv).powi(2) + 9.0e-4f64.powi(2)).sqrt();
        let blend_t = (reach_km * g_t / crate::biome::DITHER_T).min(1.0);
        let blend_h = (reach_km * g_h / crate::biome::DITHER_H).min(1.0);

        let (theme_t, theme_h) = match &self.theme {
            Some((g, _)) => {
                let at = |k: usize| g[k].bilinear(wu, wv);
                ([at(0), at(1)], [at(2), at(3)])
            }
            None => ([-9.0, 9.0], [-9.0, 9.0]),
        };
        let water_y = if water.is_finite() { water as i32 } else { i32::MIN / 2 };
        Shape {
            climate: Climate {
                continentalness,
                erosion,
                weirdness: w,
                peaks_valleys: pv,
                temperature: temperature.clamp(-1.5, 1.2).clamp(theme_t[0], theme_t[1]),
                humidity: humidity.clamp(-1.0, 1.0).clamp(theme_h[0], theme_h[1]),
            },
            height,
            roughness: (roughness * self.world_scale.sqrt()).min(rough_cap),
            river,
            island: 0.0,
            vscale: self.norm_scale,
            water: water_y,
            blend_t,
            blend_h,
            theme_t,
            theme_h,
            ..Shape::default()
        }
    }
}

/// Lakes of a procedural planet. Each hollow of the flooded terrain (cells
/// filled to the same rim) holds a lake whose size balances the water
/// flowing in against evaporation from its surface (open water evaporates
/// at the potential rate, less the rain on it): in a wet climate it fills
/// to the rim and spills, in a dry one it shrinks to a salt lake in the
/// deepest part, or dries out (the Caspian, the Dead Sea, Death Valley).
///
/// A hollow that a river of at least `q_min` flows into always keeps at
/// least a small terminal lake (Lake Eyre, Lake Chad), so no river simply
/// stops in the middle of dry land.
fn water_balance_lakes(
    elevation: &Grid<f32>,
    drainage: &erosion::Drainage,
    sea: &Grid<f32>,
    discharge: &[f32],
    clim: &climate::Climate,
    area_m2: f64,
    q_min: f64,
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
        // Too small to see on this grid, or too shallow to matter, unless
        // a river ends here.
        let floor = elevation.data[cells[0]] as f64;
        if inflow >= q_min {
            count = count.max(1);
            level = level.max(floor + 5.0).min(rim as f64);
        } else if count < 2 || level - floor < 10.0 {
            continue;
        }
        for &c in &cells[..count] {
            lake.data[c] = level as f32;
        }
    }
    lake
}

/// Where the water goes once the lakes are known.
struct Routing {
    /// Downhill neighbour; the cell itself for the sea, the border and the
    /// lakes that keep their water.
    receiver: Vec<u32>,
    /// Every cell after its receiver.
    order: Vec<u32>,
    /// Water level of a river through the cell (m): the lake surface in a
    /// lake, 0 at sea, the ground elsewhere, never below the level
    /// downstream.
    level: Vec<f32>,
}

impl Routing {
    /// Sum of `local` over each cell and everything upstream of it.
    fn accumulate(&self, local: &[f32]) -> Vec<f32> {
        let mut acc: Vec<f64> = local.iter().map(|&v| v as f64).collect();
        for &c in self.order.iter().rev() {
            let c = c as usize;
            let r = self.receiver[c] as usize;
            if r != c {
                acc[r] += acc[c];
            }
        }
        acc.into_iter().map(|v| v as f32).collect()
    }
}

/// River routing from the flood's drainage:
///
/// * a lake that doesn't fill its hollow to the rim (it evaporates what
///   flows in) is a sink: water stops there instead of flowing on, and the
///   dry slopes of its hollow drain into it rather than across to the rim
///   the flood found (that drew rivers straight through dry basins);
/// * where two diagonal flows would cross inside a square of four cells,
///   one joins the other: two rivers never cut through each other;
/// * water levels never rise downstream.
fn route_water(elevation: &Grid<f32>, drainage: &erosion::Drainage, lake: &Grid<f32>, sea: &Grid<f32>) -> Routing {
    let (w, h) = (elevation.w, elevation.h);
    let n = w * h;
    let filled = &drainage.filled;
    let mut receiver = drainage.receiver.clone();
    let is_lake = |i: usize| lake.data[i] > NO_LAKE;
    // Lakes below their rim keep their water.
    let mut heap = std::collections::BinaryHeap::new();
    let mut done = vec![false; n];
    for i in 0..n {
        if is_lake(i) && (lake.data[i] as f64) < filled[i] as f64 - 1.0 {
            receiver[i] = i as u32;
            done[i] = true;
            heap.push(std::cmp::Reverse((ord(elevation.data[i].max(lake.data[i])), i as u32, filled[i].to_bits())));
        }
    }
    // Their hollows drain into them: a flood from the lake over the rest of
    // the same hollow (same rim), lowest first.
    while let Some(std::cmp::Reverse((_, c, rim))) = heap.pop() {
        let (c, rim) = (c as usize, f32::from_bits(rim));
        let (x, z) = ((c % w) as isize, (c / w) as isize);
        for &(dx, dz, _) in &grid::NEIGHBOURS8 {
            let (a, b) = (x + dx, z + dz);
            if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                continue;
            }
            let j = a as usize + b as usize * w;
            if done[j] || is_lake(j) || sea.data[j] > 0.5 {
                continue;
            }
            if filled[j] - elevation.data[j] > 0.5 && (filled[j] - rim).abs() < 1.0 {
                done[j] = true;
                receiver[j] = c as u32;
                heap.push(std::cmp::Reverse((ord(elevation.data[j]), j as u32, rim.to_bits())));
            }
        }
    }
    let mut order = topological_order(&receiver);
    // Crossing diagonals: in the square (i, j)…(i+1, j+1), flows along
    // both diagonals cross. Send one of them into a cell of the other
    // river (downstream of it in `order`, so no loop forms).
    let mut pos = vec![0u32; n];
    for (k, &c) in order.iter().enumerate() {
        pos[c as usize] = k as u32;
    }
    let mut changed = false;
    for j in 0..h.saturating_sub(1) {
        for i in 0..w.saturating_sub(1) {
            let (a, b, c, d) = (i + j * w, i + 1 + j * w, i + (j + 1) * w, i + 1 + (j + 1) * w);
            let flows = |x: usize, y: usize| receiver[x] as usize == y;
            let diag1 = flows(a, d) || flows(d, a);
            let diag2 = flows(b, c) || flows(c, b);
            if !(diag1 && diag2) {
                continue;
            }
            // The upstream end of one diagonal joins a cell of the other
            // that lies downstream of it in `order`.
            let src2 = if flows(b, c) { b } else { c };
            let src1 = if flows(a, d) { a } else { d };
            let fix = [d + a - src1, src1]
                .iter()
                .find(|&&t| pos[t] < pos[src2])
                .map(|&t| (src2, t))
                .or_else(|| [c + b - src2, src2].iter().find(|&&t| pos[t] < pos[src1]).map(|&t| (src1, t)));
            if let Some((s, t)) = fix {
                receiver[s] = t as u32;
                changed = true;
            }
        }
    }
    if changed {
        order = topological_order(&receiver);
    }
    // Levels, from the outlets up.
    let mut level = vec![0f32; n];
    for &c in &order {
        let c = c as usize;
        let base = if sea.data[c] > 0.5 {
            0.0
        } else if is_lake(c) {
            lake.data[c]
        } else {
            elevation.data[c].max(0.0)
        };
        let r = receiver[c] as usize;
        level[c] = if r == c { base } else { base.max(level[r]) };
    }
    Routing { receiver, order, level }
}

/// Sortable key of an `f32` (total order, negatives included).
#[inline]
fn ord(v: f32) -> u32 {
    let b = v.to_bits();
    if b >> 31 == 1 {
        !b
    } else {
        b | (1 << 31)
    }
}

/// Cells ordered so that each comes after its receiver (roots first).
fn topological_order(receiver: &[u32]) -> Vec<u32> {
    let n = receiver.len();
    // Donors of each cell, in compressed rows.
    let mut start = vec![0u32; n + 1];
    for (c, &r) in receiver.iter().enumerate() {
        if r as usize != c {
            start[r as usize + 1] += 1;
        }
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut donors = vec![0u32; start[n] as usize];
    for (c, &r) in receiver.iter().enumerate() {
        if r as usize != c {
            donors[fill[r as usize] as usize] = c as u32;
            fill[r as usize] += 1;
        }
    }
    let mut order: Vec<u32> = (0..n as u32).filter(|&c| receiver[c as usize] == c).collect();
    let mut k = 0;
    while k < order.len() {
        let c = order[k] as usize;
        order.extend_from_slice(&donors[start[c] as usize..start[c + 1] as usize]);
        k += 1;
    }
    debug_assert_eq!(order.len(), n, "routing has a loop");
    order
}

/// Magnitude of a field's gradient, per km (central differences).
fn gradient(f: &Grid<f32>, km: f64) -> Grid<f32> {
    let (w, h) = (f.w, f.h);
    let mut g = Grid::new(w, h, 0f32);
    for j in 0..h {
        for i in 0..w {
            let (a, b) = (i as isize, j as isize);
            let gx = (f.at(a + 1, b) as f64 - f.at(a - 1, b) as f64) / (2.0 * km);
            let gz = (f.at(a, b + 1) as f64 - f.at(a, b - 1) as f64) / (2.0 * km);
            g.set(i, j, (gx * gx + gz * gz).sqrt() as f32);
        }
    }
    g
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

    /// Near the open sea, every column below sea level is under water
    /// unless land at or above sea level shuts it off (a basin rim): the
    /// coast follows the terrain, never a "wall" where the sea stops (the
    /// dry-depression rule once used a per-cell sea mask, so dry land below
    /// sea level appeared in rectangles along coasts).
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
            let (mut walls, mut checked) = (0, 0);
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
                        if s.water < SEA_LEVEL {
                            // A dry column below the sea must be cut off
                            // from it by land at or above sea level (a
                            // basin rim); open water first would be a wall.
                            let reach = (2.0 * r.cell_blocks) as i32;
                            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                                for k in 1..=reach {
                                    let t = r.shape(x + dx * k, z + dz * k);
                                    if t.height >= SEA_LEVEL as f64 {
                                        break;
                                    }
                                    if t.water >= SEA_LEVEL {
                                        walls += 1;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            assert!(checked > 1000, "{planet:?}: only {checked} coastal columns");
            assert_eq!(walls, 0, "{planet:?}: {walls} of {checked} coastal columns below sea level are walled off");
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

    /// The river network is a tree that ends at the sea or in a lake:
    /// every reach flows downhill into exactly one next reach, rivers
    /// never cross, tributaries join main rivers, and sizes range from
    /// creeks to Amazon-class rivers.
    #[test]
    fn rivers_form_networks_that_reach_the_sea_or_a_lake() {
        for planet in [Planet::Continents, Planet::Pangea] {
            let r = RealisticWorld::new(&WorldSettings {
                generator: crate::settings::Generator::Realistic,
                planet,
                quality: SimQuality::Fast,
                ..Default::default()
            });
            let n = r.nodes.len();
            let (mut out, mut inflow) = (vec![0u32; n], vec![0u32; n]);
            for e in &r.reaches {
                out[e.a as usize] += 1;
                inflow[e.b as usize] += 1;
                let (a, b) = (r.nodes[e.a as usize], r.nodes[e.b as usize]);
                assert!(b.level <= a.level + 1e-3, "{planet:?}: river climbs from {} to {}", a.level, b.level);
                assert!(b.q >= a.q - 1e-3, "{planet:?}: river loses water ({} to {})", a.q, b.q);
            }
            assert!(out.iter().all(|&k| k <= 1), "{planet:?}: a river splits");
            let cell = |k: usize| {
                let (u, v) = (r.nodes[k].u.floor() as isize, r.nodes[k].v.floor() as isize);
                (u.clamp(0, r.w as isize - 1) as usize) + (v.clamp(0, r.h as isize - 1) as usize) * r.w
            };
            let mut stranded = 0;
            for (k, &o) in out.iter().enumerate() {
                if o == 0 {
                    let c = cell(k);
                    let border = c % r.w == 0 || c / r.w == 0 || c % r.w == r.w - 1 || c / r.w == r.h - 1;
                    let ends_well = r.nodes[k].level <= 0.0 || r.lake.data[c] > NO_LAKE || r.sea.data[c] > 0.5;
                    if !ends_well && !border {
                        stranded += 1;
                    }
                }
            }
            assert_eq!(stranded, 0, "{planet:?}: {stranded} rivers end on dry land");
            // No two reaches cross (sharing a node is a confluence).
            let cross = |p: (f64, f64), q: (f64, f64), s: (f64, f64), t: (f64, f64)| {
                let o =
                    |a: (f64, f64), b: (f64, f64), c: (f64, f64)| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
                o(p, q, s) * o(p, q, t) < 0.0 && o(s, t, p) * o(s, t, q) < 0.0
            };
            let mut crossings = 0;
            for list in &r.reach_index {
                for (x, &i) in list.iter().enumerate() {
                    for &j in &list[x + 1..] {
                        let (e, f) = (r.reaches[i as usize], r.reaches[j as usize]);
                        if e.a == f.a || e.a == f.b || e.b == f.a || e.b == f.b {
                            continue;
                        }
                        // The last stretch out to sea isn't carved (open water).
                        if r.sea.data[cell(e.a as usize)] > 0.5 || r.sea.data[cell(f.a as usize)] > 0.5 {
                            continue;
                        }
                        let pt = |k: u32| (r.nodes[k as usize].u, r.nodes[k as usize].v);
                        if cross(pt(e.a), pt(e.b), pt(f.a), pt(f.b)) {
                            crossings += 1;
                        }
                    }
                }
            }
            assert_eq!(crossings, 0, "{planet:?}: rivers cross each other");
            // Networks: many confluences, and a wide range of sizes.
            let sources = inflow.iter().zip(&out).filter(|&(&i, &o)| i == 0 && o == 1).count();
            let confluences = inflow.iter().filter(|&&i| i >= 2).count();
            assert!(confluences * 3 > sources, "{planet:?}: {confluences} confluences for {sources} sources");
            let max_q = r.nodes.iter().map(|n| n.q).fold(0.0, f64::max);
            let min_q = r.nodes.iter().map(|n| n.q).fold(f64::MAX, f64::min);
            assert!(max_q > 20_000.0, "{planet:?}: largest river {max_q:.0} m³/s");
            assert!(max_q / min_q > 1_000.0, "{planet:?}: rivers {min_q:.0}..{max_q:.0} m³/s");
        }
    }

    fn full_planet(planet: Planet) -> RealisticWorld {
        RealisticWorld::new(&WorldSettings {
            generator: crate::settings::Generator::Realistic,
            planet,
            quality: SimQuality::Fast,
            size: WorldSize::full(),
            height: crate::settings::WorldHeight::from_blocks(17_696),
            ..Default::default()
        })
    }

    /// The open ocean gets its small islands: volcanoes, worn hills and,
    /// only in the tropics, atolls whose sandy rim rises just above the
    /// sea around a lagoon.
    #[test]
    fn open_ocean_has_volcanoes_hills_and_atolls() {
        let r = full_planet(Planet::Continents);
        let count = |f: &dyn Fn(IsletKind) -> bool| r.islets.iter().filter(|i| f(i.kind)).count();
        assert!(count(&|k| matches!(k, IsletKind::Volcano(_))) >= 10);
        assert!(count(&|k| k == IsletKind::Hill) >= 10);
        assert!(count(&|k| k == IsletKind::Atoll) >= 10);
        let block = |u: f64, v: f64| ((r.x0 + u * r.cell_blocks) as i32, (r.z0 + v * r.cell_blocks) as i32);
        let km_cells = 1.0 / r.km;
        let mut rims = 0;
        for isl in r.islets.iter().filter(|i| i.kind == IsletKind::Atoll) {
            let lat = 90.0 - isl.v / r.h as f64 * 180.0;
            assert!(lat.abs() < 28.0, "atoll at {lat:.1}°");
            // The lagoon is water; somewhere on the rim there is dry sand.
            if isl.r > 3.0 {
                let (x, z) = block(isl.u, isl.v);
                let s = r.shape(x, z);
                assert!(s.height < s.water as f64, "dry lagoon");
            }
            let dry = (0..64).any(|k| {
                let a = k as f64 / 64.0 * std::f64::consts::TAU;
                (0..12).any(|t| {
                    let d = isl.r * (0.85 + 0.025 * t as f64) * km_cells;
                    let (x, z) = block(isl.u + d * libm::cos(a), isl.v + d * libm::sin(a));
                    let s = r.shape(x, z);
                    s.height >= SEA_LEVEL as f64 && s.height < SEA_LEVEL as f64 + 5.0
                })
            });
            rims += dry as usize;
        }
        assert!(rims * 10 >= count(&|k| k == IsletKind::Atoll) * 8, "{rims} atolls with a dry rim");
        // A volcano rises far above the sea.
        let v = r.islets.iter().find(|i| matches!(i.kind, IsletKind::Volcano(false))).unwrap();
        let (x, z) = block(v.u, v.v);
        assert!(r.shape(x, z).height > SEA_LEVEL as f64 + 0.3 * v.top * r.v_scale);
    }

    /// Hills rise away from the ranges (foothills, old ranges, rolling
    /// uplands), and the hot deserts have sand seas whose dunes stand tens
    /// of blocks high.
    #[test]
    fn hills_and_dunes_away_from_the_mountains() {
        let r = full_planet(Planet::Continents);
        let land: Vec<usize> = (0..r.w * r.h).filter(|&i| r.sea.data[i] < 0.5 && r.elevation.data[i] >= 0.0).collect();
        let hilly = land.iter().filter(|&&i| r.hill_amp.data[i] > 80.0 && r.mountain.data[i] < 0.1).count();
        assert!(hilly * 10 > land.len(), "{hilly} hilly lowland cells of {}", land.len());
        let flat = land.iter().filter(|&&i| r.hill_amp.data[i] < 20.0 && r.mountain.data[i] < 0.1).count();
        assert!(flat * 10 > land.len(), "only {flat} flat cells: not everything is hills");
        for &i in &land {
            if r.dune_amp.data[i] > 10.0 {
                assert!(r.humidity.data[i] < -0.4, "dunes in a humidity of {}", r.humidity.data[i]);
            }
        }
        let best = (0..r.w * r.h).max_by(|&a, &b| r.dune_amp.data[a].total_cmp(&r.dune_amp.data[b])).unwrap();
        assert!(r.dune_amp.data[best] > 40.0);
        let (x, z) = (
            (r.x0 + ((best % r.w) as f64 + 0.5) * r.cell_blocks) as i32,
            (r.z0 + ((best / r.w) as f64 + 0.5) * r.cell_blocks) as i32,
        );
        // Along 6 km in two directions the dunes rise and fall.
        for (dx, dz) in [(1, 0), (0, 1)] {
            let hs: Vec<f64> = (0..600).map(|k| r.shape(x + dx * 15 * k, z + dz * 15 * k).height).collect();
            let (lo, hi) = hs.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            assert!(hi - lo > 25.0, "dunes only {:.0} blocks high", hi - lo);
        }
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
