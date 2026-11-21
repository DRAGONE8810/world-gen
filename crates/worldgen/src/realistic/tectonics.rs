//! Plate tectonics: where continents, mountain ranges, trenches, mid-ocean
//! ridges and volcanic islands are.
//!
//! This is a *kinematic* model in the spirit of "Procedural Tectonic
//! Planets" (Cortial et al., 2019): instead of simulating millions of years
//! of mantle flow, it lays out today's plates, gives each one a motion, and
//! derives the relief from what happens where two plates meet. That is
//! enough to put every landform where geology says it belongs:
//!
//! | Boundary | Crust on each side | Landform (Earth example) |
//! |---|---|---|
//! | converging | continent – continent | high range + plateau (Himalaya, Tibet) |
//! | converging | continent – ocean | trench offshore, volcanic cordillera inland (Andes) |
//! | converging | ocean – ocean | trench + arc of volcanic islands (Japan, Aleutians) |
//! | diverging | ocean – ocean | mid-ocean ridge; the floor deepens with age (Atlantic) |
//! | diverging | continent – continent | rift valley with raised shoulders (East Africa) |
//!
//! Steps:
//!
//! 1. **Continental crust.** A field picks which part of the surface is
//!    light continental crust. On the Continents planet it is *planned*:
//!    4–7 continents spread apart, each built from ellipses (a body of
//!    lobes, peninsulas, gulfs, offshore islands) in one of four
//!    characters taken from Earth (massive like Africa, a cordillera like
//!    western North America, fragmented like Europe, an island continent
//!    like Australia), then warped and roughened by noise in proportion to
//!    how rugged that continent is. Pangea and Islands use noise around a
//!    few blobs. The crust edge, ~100 km beyond the coast, is the shelf
//!    break.
//! 2. **Plates.** 10–14 plates grow from random seeds with Dijkstra's
//!    algorithm. Crossing continental crust is expensive, so boundaries
//!    tend to run through oceans and along coasts, as on Earth.
//! 3. **Motion.** Each plate gets a velocity (a drift plus a slow spin);
//!    oceanic plates move faster, like the Pacific plate.
//! 4. **Boundaries.** For every boundary cell the relative velocity of the
//!    two plates, projected on the boundary normal, says whether they
//!    converge, diverge or slide past each other.
//! 5. **Relief.** Each cell looks up the nearest boundary of its own plate
//!    (an exact Euclidean distance field) and adds the profile of that
//!    boundary type at that distance. The ocean floor deepens with its age
//!    as it cools, following the GDH1 plate model (Stein & Stein, 1992):
//!    2.6 km at the ridge + 0.365 km·√Myr for the first 20 Myr, then
//!    flattening towards 5.65 km. Age is the distance to the nearest ridge
//!    over the half spreading rate.
//! 6. **Old mountains** (Appalachians, Urals) from an older plate layout,
//!    and **hotspot chains** (Hawaii) trailing behind their plate.
//!
//! Heights are in real metres on a virtual planet 40,000 km across;
//! `RealisticWorld` scales them into blocks.

use super::grid::{distance_field, Grid};
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::rng::{derive_seed, Xoroshiro128PlusPlus};
use crate::settings::Planet;
use crate::spline::{lerp, smoothstep};
use rayon::prelude::*;

/// Everything the later stages need from the tectonic model.
pub struct Tectonics {
    /// Elevation in metres (sea level 0) before erosion.
    pub elevation: Grid<f32>,
    /// 0..1: how mountainous (young orogen, arc, rift shoulder).
    pub mountain: Grid<f32>,
    /// Doubled-angle orientation of the nearest range's axis, as
    /// (cos 2θ, sin 2θ): ridges run along θ. Doubling makes θ and θ + 180°
    /// the same, so the field can be interpolated.
    pub strike_c: Grid<f32>,
    pub strike_s: Grid<f32>,
    /// 1 on continental crust, 0 on oceanic, smooth across the margin.
    pub crust: Grid<f32>,
    /// Plate index per cell (for maps and tests).
    pub plate: Grid<u16>,
}

struct Plate {
    /// Drift, cm/yr (= km/Myr × 0.1), in x and z.
    vx: f64,
    vz: f64,
    /// Spin, cm/yr per 1000 km from the centroid.
    spin: f64,
    cx: f64,
    cz: f64,
    /// Breaks ties at ocean–ocean and continent–continent collisions: the
    /// denser plate dives.
    density: f64,
}

impl Plate {
    #[inline]
    fn velocity(&self, x: f64, z: f64) -> (f64, f64) {
        let (dx, dz) = ((x - self.cx) / 1000.0, (z - self.cz) / 1000.0);
        (self.vx - self.spin * dz, self.vz + self.spin * dx)
    }
}

/// An ellipse on the planet, in km: centre, rotation (cos, sin) and the two
/// half-axes, `rx` along the rotated x axis.
#[derive(Clone, Copy, Debug)]
struct Ellipse {
    x: f64,
    z: f64,
    c: f64,
    s: f64,
    rx: f64,
    rz: f64,
}

impl Ellipse {
    fn new(x: f64, z: f64, angle: f64, rx: f64, rz: f64) -> Self {
        Self { x, z, c: libm::cos(angle), s: libm::sin(angle), rx, rz }
    }
    /// 0 at the centre, 1 on the rim, growing outwards.
    fn dist(&self, x: f64, z: f64) -> f64 {
        let (dx, dz) = (x - self.x, z - self.z);
        let u = (dx * self.c + dz * self.s) / self.rx;
        let v = (dz * self.c - dx * self.s) / self.rz;
        (u * u + v * v).sqrt()
    }
    /// The rim point in direction `a` (relative to the ellipse's own axis).
    fn rim(&self, a: f64) -> (f64, f64) {
        let (u, v) = (libm::cos(a) * self.rx, libm::sin(a) * self.rz);
        (self.x + u * self.c - v * self.s, self.z + u * self.s + v * self.c)
    }
    /// The outward direction (world angle) at the rim point `a`.
    fn normal(&self, a: f64) -> f64 {
        libm::atan2(libm::sin(a) / self.rz, libm::cos(a) / self.rx) + libm::atan2(self.s, self.c)
    }
    fn scale(&mut self, ox: f64, oz: f64, k: f64) {
        self.x = ox + (self.x - ox) * k;
        self.z = oz + (self.z - oz) * k;
        self.rx *= k;
        self.rz *= k;
    }
}

/// The plan of one continent: overlapping land lobes (body, arms,
/// peninsulas, offshore islands) minus water ellipses (gulfs, inland seas),
/// and how rugged its coast is.
#[derive(Clone, Debug)]
struct ContinentPlan {
    /// Centre in km; the continent may not come closer than a strait's
    /// width to the border of its Voronoi cell around this point.
    cx: f64,
    cz: f64,
    land: Vec<Ellipse>,
    /// The first `body` land ellipses are the main body (the rest are
    /// peninsulas and islands).
    body: usize,
    water: Vec<Ellipse>,
    /// 0 = smooth coast (Africa), 1 = shattered (Europe, Canadian Arctic).
    rugged: f64,
}

impl ContinentPlan {
    /// Signed "inside-ness": 1 at a lobe centre, 0 on the planned coast,
    /// negative outside or inside a gulf.
    fn field(&self, x: f64, z: f64) -> f64 {
        let mut f = self.land.iter().fold(-1.0f64, |f, e| f.max(1.0 - e.dist(x, z)));
        // Water ellipses cut the land down to their rim.
        for e in &self.water {
            f = f.min(1.5 * (e.dist(x, z) - 1.0));
        }
        f
    }
}

/// Half the width (km) of the sea kept between two continents.
const STRAIT_KM: f64 = 260.0;

/// Distance (km) from a point to the border of continent `i`'s Voronoi
/// cell, positive inside.
fn voronoi_margin(plans: &[ContinentPlan], i: usize, x: f64, z: f64) -> f64 {
    let p = &plans[i];
    let mut m = f64::MAX;
    for (j, q) in plans.iter().enumerate() {
        if i != j {
            let (dx, dz) = (q.cx - p.cx, q.cz - p.cz);
            let len = dx.hypot(dz);
            let (mx, mz) = ((p.cx + q.cx) * 0.5, (p.cz + q.cz) * 0.5);
            m = m.min(-((x - mx) * dx + (z - mz) * dz) / len);
        }
    }
    m
}

/// The combined plan field at a point and the softmax-blended ruggedness.
/// Each continent is clipped to its Voronoi cell (around the plan
/// centres) shrunk by `gap` km, so neighbours are parted by a sea at least
/// `2 · gap` wide wherever their lobes would touch; the domain warp in
/// `build` bends those partings like real seaways. A negative `gap` lets
/// lobes cross over (a land bridge).
fn planned_field(plans: &[ContinentPlan], x: f64, z: f64, gap: f64) -> (f64, f64) {
    let (mut f, mut wsum, mut rug) = (-9.0f64, 0.0, 0.0);
    for (i, p) in plans.iter().enumerate() {
        let mut fc = p.field(x, z);
        // Distance to the Voronoi border: to each bisector with another
        // centre, positive on this continent's side.
        let m = voronoi_margin(plans, i, x, z);
        fc = fc.min((m - gap) / 500.0);
        f = f.max(fc);
        let wt = libm::exp(6.0 * fc.clamp(-3.0, 2.0));
        wsum += wt;
        rug += wt * p.rugged;
    }
    (f, rug / wsum.max(1e-12))
}

/// Lays out the Continents planet: 4–7 continents spread over the map, each
/// of one of four characters seen on Earth:
///
/// * **massive** (Africa, South America): one or two fat lobes, few
///   bays, smooth coasts;
/// * **cordillera** (western North America, Alaska to Baja California):
///   a long bent chain of lobes with a thin peninsula or two, a gulf and an
///   island arc offshore;
/// * **fragmented** (Europe, Southeast Asia): a branching body with many
///   peninsulas, inland seas and islands, and a rugged coast;
/// * **island continent** (Australia): one compact body with a deep bight,
///   a cape and an island across a strait.
///
/// The ellipses only plan the shape; noise and warping in `build` turn
/// them into coasts.
fn plan_continents(seed: u64, width: f64, depth: f64) -> Vec<ContinentPlan> {
    let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/continent_plan"));
    let mut r = move || rng.next_f64();
    let n = |r: &mut dyn FnMut() -> f64, k: usize| (r() * k as f64) as usize;
    let k = 4 + (r() * 4.0) as usize;
    // Spread the centres: best of several candidates (Mitchell's
    // best-candidate sampling), so continents don't pile into one mass.
    let mut centres: Vec<(f64, f64)> = Vec::new();
    for _ in 0..k {
        let mut best = (0.0, 0.0, -1.0);
        for _ in 0..12 {
            let (x, z) = (width * (0.17 + 0.66 * r()), depth * (0.17 + 0.66 * r()));
            let d = centres.iter().map(|&(a, b)| (x - a).hypot(z - b)).fold(f64::MAX, f64::min);
            if d > best.2 {
                best = (x, z, d);
            }
        }
        centres.push((best.0, best.1));
    }
    // Characters: at most two massive continents, at least one fragmented
    // and one cordillera, the rest drawn at random.
    let mut kinds: Vec<u8> = vec![2, 1];
    while kinds.len() < k {
        let massive = kinds.iter().filter(|&&c| c == 0).count();
        let pick = (r() * 4.0) as u8;
        kinds.push(if pick == 0 && massive >= 2 { 2 } else { pick });
    }
    // Shuffle so the character doesn't follow the placement order.
    for i in (1..k).rev() {
        let j = (r() * (i + 1) as f64) as usize;
        kinds.swap(i, j.min(i));
    }

    let tau = std::f64::consts::TAU;
    let mut plans: Vec<ContinentPlan> = Vec::new();
    for (&(cx, cz), &kind) in centres.iter().zip(&kinds) {
        // Relative size: a spread like Earth's (Asia ≈ 6 × Australia).
        let size = libm::exp(0.9 * (r() - 0.5)) * if kind == 3 { 0.65 } else { 1.0 };
        let base = 0.1 * width * size;
        let (lobes, aspect, taper, turn, peninsulas, gulfs, islands, rugged) = match kind {
            0 => (
                1 + n(&mut r, 2),
                1.0 + 0.5 * r(),
                0.6,
                0.5,
                n(&mut r, 3),
                n(&mut r, 2),
                n(&mut r, 2),
                0.15 + 0.2 * r(),
            ),
            1 => (
                4 + n(&mut r, 3),
                1.5 + 0.8 * r(),
                0.85,
                0.45,
                2 + n(&mut r, 2),
                1 + n(&mut r, 2),
                3 + n(&mut r, 4),
                0.5 + 0.3 * r(),
            ),
            2 => (
                3 + n(&mut r, 2),
                1.2 + 0.6 * r(),
                0.72,
                1.1,
                5 + n(&mut r, 4),
                3 + n(&mut r, 3),
                3 + n(&mut r, 5),
                0.75 + 0.25 * r(),
            ),
            _ => (1, 1.2 + 0.3 * r(), 0.6, 0.3, 1 + n(&mut r, 2), 1, 1 + n(&mut r, 2), 0.25 + 0.2 * r()),
        };
        let mut plan = ContinentPlan { cx, cz, land: Vec::new(), body: 0, water: Vec::new(), rugged };
        // Body: lobes along a bending spine, shrinking as it goes.
        let mut dir = r() * tau;
        let (mut x, mut z) = (cx, cz);
        let mut rad = base;
        for j in 0..lobes {
            if j > 0 {
                let prev = rad;
                rad *= taper * (0.85 + 0.3 * r());
                let step = (prev + rad) * 0.55 * if kind == 1 { aspect * 0.8 } else { 1.0 };
                // Of three bends, take the one heading into the most room
                // (farthest from the other continents' centres).
                let mut best = (dir, f64::MIN);
                for _ in 0..3 {
                    let d = dir + (r() - 0.5) * 2.0 * turn;
                    let (nx, nz) = (x + libm::cos(d) * step, z + libm::sin(d) * step);
                    let room = centres
                        .iter()
                        .filter(|&&c| c != (cx, cz))
                        .map(|&(a, b)| (nx - a).hypot(nz - b))
                        .fold(f64::MAX, f64::min);
                    if room > best.1 {
                        best = (d, room);
                    }
                }
                dir = best.0;
                x += libm::cos(dir) * step;
                z += libm::sin(dir) * step;
                // Bounce off the border zone, which is kept as ocean.
                if !(0.15 * width..0.85 * width).contains(&x) || !(0.15 * depth..0.85 * depth).contains(&z) {
                    x = x.clamp(0.15 * width, 0.85 * width);
                    z = z.clamp(0.15 * depth, 0.85 * depth);
                    dir += std::f64::consts::PI * (0.6 + 0.3 * r());
                }
            }
            let a = aspect * (0.85 + 0.3 * r());
            plan.land.push(Ellipse::new(x, z, dir + (r() - 0.5) * 0.5, rad * a.sqrt(), rad / a.sqrt()));
        }
        let body = plan.land.len();
        plan.body = body;
        // A point on the outer coast and the outward direction there:
        // start on a random body lobe's rim and walk out until no lobe
        // covers the point (a feature placed inside the union
        // would put a gulf in the middle of the land). Of a few tries, keep
        // the one with the shortest walk, where the coast is convex.
        let coast_point = |r: &mut dyn FnMut() -> f64, land: &[Ellipse]| -> (Ellipse, f64, f64, f64) {
            let union = |x: f64, z: f64| land.iter().fold(-1.0f64, |f, e| f.max(1.0 - e.dist(x, z)));
            let mut best = (land[0], 0.0, 0.0, 0.0, f64::MAX);
            for _ in 0..6 {
                let host = land[(r() * body as f64) as usize];
                let a = r() * tau;
                let (mut px, mut pz) = host.rim(a);
                let out = host.normal(a);
                let step = 0.05 * host.rx.min(host.rz);
                let mut walked = 0.0;
                while union(px, pz) > 0.0 && walked < 40.0 * step {
                    px += libm::cos(out) * step;
                    pz += libm::sin(out) * step;
                    walked += step;
                }
                if walked < best.4 {
                    best = (host, out, px, pz, walked);
                }
            }
            (best.0, best.1, best.2, best.3)
        };
        // Gulfs and inland seas (Gulf of California, Red Sea, Baltic,
        // Mediterranean): long water ellipses reaching in from the rim, so
        // they open onto the ocean.
        for _ in 0..gulfs {
            let (host, out, px, pz) = coast_point(&mut r, &plan.land);
            let inward = out + std::f64::consts::PI + (r() - 0.5) * 0.8;
            let reach = if kind == 2 { 0.6 + 0.5 * r() } else { 0.35 + 0.4 * r() };
            let len = host.rx.min(host.rz) * reach;
            let wide = len * (0.3 + 0.3 * r());
            let (gx, gz) = (px + libm::cos(inward) * len * 0.4, pz + libm::sin(inward) * len * 0.4);
            plan.water.push(Ellipse::new(gx, gz, inward, len, wide));
        }
        // Peninsulas (Iberia, Italy, Baja California, Cape York): thin
        // lobes leaving the rim outwards.
        for _ in 0..peninsulas {
            let (host, out, px, pz) = coast_point(&mut r, &plan.land);
            let out = out + (r() - 0.5) * 0.9;
            let len = host.rx.min(host.rz) * (0.7 + 0.7 * r());
            let thick = len * (0.2 + 0.2 * r());
            let (ox, oz) = (px + libm::cos(out) * len * 0.55, pz + libm::sin(out) * len * 0.55);
            plan.land.push(Ellipse::new(ox, oz, out, len, thick));
        }
        // Offshore islands (Britain, Tasmania, Vancouver Island).
        for _ in 0..islands {
            let (host, out, px, pz) = coast_point(&mut r, &plan.land);
            let size = host.rx.min(host.rz) * (0.12 + 0.18 * r());
            let gap = size * (1.3 + 0.8 * r());
            let stretch = 1.0 + 2.0 * r();
            plan.land.push(Ellipse::new(
                px + libm::cos(out) * gap,
                pz + libm::sin(out) * gap,
                out + std::f64::consts::FRAC_PI_2 + (r() - 0.5),
                size * stretch.sqrt(),
                size / stretch.sqrt(),
            ));
        }
        plans.push(plan);
    }
    // Scale every continent about its centre so the planned land covers
    // ~36 % of the map (measured on a coarse grid, since lobes overlap).
    // Each continent may grow only until its body would come within a
    // strait of its Voronoi border or near the world border, so shapes are
    // kept whole instead of being cut straight; the others grow to make up
    // the area.
    let target = 0.36;
    let room = |i: usize| -> f64 {
        let p = &plans[i];
        let (lo_x, hi_x, lo_z, hi_z) = (0.07 * width, 0.93 * width, 0.07 * depth, 0.93 * depth);
        let mut s = 8.0f64;
        for e in &p.land[..p.body] {
            for t in 0..16 {
                let (x, z) = e.rim(t as f64 * tau / 16.0);
                let (vx, vz) = (x - p.cx, z - p.cz);
                for (v, c, lo, hi) in [(vx, p.cx, lo_x, hi_x), (vz, p.cz, lo_z, hi_z)] {
                    if v > 1e-9 {
                        s = s.min((hi - c) / v);
                    } else if v < -1e-9 {
                        s = s.min((lo - c) / v);
                    }
                }
                for (j, q) in plans.iter().enumerate() {
                    if j != i {
                        let (nx, nz) = (q.cx - p.cx, q.cz - p.cz);
                        let len = nx.hypot(nz);
                        let reach = (vx * nx + vz * nz) / len;
                        if reach > 1e-9 {
                            s = s.min((0.5 * len - 1.2 * STRAIT_KM).max(0.0) / reach);
                        }
                    }
                }
            }
        }
        s.max(0.05)
    };
    let limits: Vec<f64> = (0..plans.len()).map(room).collect();
    let scaled = |sg: f64| -> Vec<ContinentPlan> {
        let mut out = plans.clone();
        for (p, &lim) in out.iter_mut().zip(&limits) {
            let (cx, cz, k) = (p.cx, p.cz, sg.min(lim));
            for e in p.land.iter_mut().chain(p.water.iter_mut()) {
                e.scale(cx, cz, k);
            }
        }
        out
    };
    let coverage = |plans: &[ContinentPlan]| -> f64 {
        let g = 128;
        let mut covered = 0;
        for j in 0..g {
            for i in 0..g {
                let (x, z) = ((i as f64 + 0.5) / g as f64 * width, (j as f64 + 0.5) / g as f64 * depth);
                covered += (planned_field(plans, x, z, STRAIT_KM).0 > 0.0) as usize;
            }
        }
        covered as f64 / (g * g) as f64
    };
    let (mut lo, mut hi) = (0.05f64, 8.0f64);
    for _ in 0..14 {
        let mid = (lo * hi).sqrt();
        if coverage(&scaled(mid)) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let mut plans = scaled(lo);
    // Peninsulas and islands that still reach across are shortened.
    for i in 0..plans.len() {
        for l in plans[i].body..plans[i].land.len() {
            let e = plans[i].land[l];
            let worst = (0..16)
                .map(|t| {
                    let (x, z) = e.rim(t as f64 * tau / 16.0);
                    voronoi_margin(&plans, i, x, z) - 1.2 * STRAIT_KM
                })
                .fold(f64::MAX, f64::min);
            if worst < 0.0 {
                let e = &mut plans[i].land[l];
                e.rx = (e.rx + worst).max(0.25 * e.rx);
                e.rz = (e.rz + worst).max(0.25 * e.rz);
            }
        }
    }
    plans
}

#[inline]
fn bump(d: f64, centre: f64, width: f64) -> f64 {
    let t = (d - centre) / width;
    libm::exp(-t * t)
}

pub fn build(seed: u64, planet: Planet, w: usize, h: usize, km: f64) -> Tectonics {
    let n = w * h;
    let pos = |i: usize| (((i % w) as f64 + 0.5) * km, ((i / w) as f64 + 0.5) * km);
    let (width_km, depth_km) = (w as f64 * km, h as f64 * km);
    let mut rng = Xoroshiro128PlusPlus::from_seed(derive_seed(seed, "realistic/tectonics"));

    // --- 1. Continental crust -------------------------------------------
    let fbm = |salt: &str, first: i32, octaves: usize| {
        OctaveNoise::new(derive_seed(seed, salt), &OctaveConfig::new(first, &vec![1.0; octaves]))
    };
    let warp_x = fbm("realistic/crust_warp_x", -12, 3);
    let warp_z = fbm("realistic/crust_warp_z", -12, 3);
    let crust_noise = fbm("realistic/crust_noise", -12, 5);
    let fine_noise = fbm("realistic/crust_fine", -10, 4);
    // (centre x, centre z, radius) in km.
    let mut blobs: Vec<(f64, f64, f64)> = Vec::new();
    let (fraction, noise_weight, fine_weight) = match planet {
        Planet::Pangea => {
            let (cx, cz) = (width_km * (0.4 + 0.2 * rng.next_f64()), depth_km * (0.4 + 0.2 * rng.next_f64()));
            blobs.push((cx, cz, 0.33 * width_km));
            for _ in 0..4 {
                let a = rng.next_f64() * std::f64::consts::TAU;
                let r = 0.16 * width_km;
                blobs.push((cx + libm::cos(a) * r, cz + libm::sin(a) * r, 0.17 * width_km));
            }
            (0.34, 0.35, 0.12)
        }
        Planet::Islands => {
            for _ in 0..14 {
                blobs.push((
                    width_km * (0.05 + 0.9 * rng.next_f64()),
                    depth_km * (0.05 + 0.9 * rng.next_f64()),
                    (0.012 + 0.03 * rng.next_f64()) * width_km,
                ));
            }
            (0.11, 0.9, 0.45)
        }
        _ => (0.32, 0.55, 0.15),
    };
    let plans = if planet == Planet::Continents { plan_continents(seed, width_km, depth_km) } else { Vec::new() };
    let mid_warp_x = fbm("realistic/crust_mid_warp_x", -10, 3);
    let mid_warp_z = fbm("realistic/crust_mid_warp_z", -10, 3);
    let strait_noise = fbm("realistic/crust_straits", -11, 2);
    let field_at = |i: usize| -> f32 {
        let (x, z) = pos(i);
        // Planned continents keep more of their outline: a smaller warp.
        let warp = if plans.is_empty() { 1100.0 } else { 650.0 };
        let wx = x + warp * warp_x.sample_2d(x, z);
        let wz = z + warp * warp_z.sample_2d(x, z);
        let mut f: f64 = -1.0;
        if plans.is_empty() {
            for &(bx, bz, r) in &blobs {
                let d = ((wx - bx) * (wx - bx) + (wz - bz) * (wz - bz)).sqrt();
                f = f.max(1.0 - d / r);
            }
            f += noise_weight * crust_noise.sample_2d(wx, wz) * 2.2 + fine_weight * fine_noise.sample_2d(wx, wz) * 2.2;
        } else {
            let bridge = smoothstep(0.15, 0.35, strait_noise.sample_2d(wx, wz));
            let gap = lerp(bridge, STRAIT_KM, -STRAIT_KM);
            let (f1, rug) = planned_field(&plans, wx, wz, gap);
            // A rugged coast is warped again at a finer scale, which bends
            // it into peninsulas and bays, then roughened by finer noise.
            let mx = wx + 550.0 * rug * mid_warp_x.sample_2d(wx, wz);
            let mz = wz + 550.0 * rug * mid_warp_z.sample_2d(wx, wz);
            let g = planned_field(&plans, mx, mz, gap).0;
            f = lerp(rug, f1, g);
            f += lerp(rug, 0.18, 0.3) * crust_noise.sample_2d(mx, mz) * 2.2
                + lerp(rug, 0.05, 0.3) * fine_noise.sample_2d(mx, mz) * 2.2;
        }
        // The world border is open ocean: keep continents off the edges so
        // no coast is cut by the border.
        let edge = (x.min(width_km - x).min(z).min(depth_km - z)) / width_km;
        if plans.is_empty() {
            f -= 3.0 * (1.0 - smoothstep(0.0, 0.08, edge));
        } else {
            // Measured on the warped point, the margin bends like any
            // other coast instead of running straight along the border;
            // a thin strict band still keeps the border itself at sea.
            let bent = (wx.min(width_km - wx).min(wz).min(depth_km - wz)) / width_km;
            f -= 3.0 * (1.0 - smoothstep(0.0, 0.08, bent)) + 3.0 * (1.0 - smoothstep(0.0, 0.025, edge));
        }
        f as f32
    };
    let field = Grid { w, h, data: (0..n).into_par_iter().map(field_at).collect() };
    // A planned layout already has about the right amount of land; an
    // unbounded quantile would inflate it (merging continents) whenever the
    // plan came out small.
    let threshold = field.quantile(1.0 - fraction) as f64;
    let threshold = if plans.is_empty() { threshold } else { threshold.clamp(-0.15, 0.15) };
    let continental: Vec<bool> = field.data.iter().map(|&f| f as f64 > threshold).collect();

    // Signed distance (km) to the crust edge: + on continental crust.
    let edge_cells: Vec<usize> = (0..n)
        .filter(|&i| {
            let (x, z) = ((i % w) as isize, (i / w) as isize);
            [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|&(dx, dz)| {
                let (a, b) = (x + dx, z + dz);
                a >= 0
                    && b >= 0
                    && a < w as isize
                    && b < h as isize
                    && continental[a as usize + b as usize * w] != continental[i]
            })
        })
        .collect();
    let (edge_dist, _) = distance_field(w, h, &edge_cells, 4000.0 / km, |_, _| true);
    let signed: Vec<f64> = (0..n)
        .map(|i| {
            let d = (edge_dist[i] as f64).min(4000.0 / km) * km;
            if continental[i] {
                d
            } else {
                -d
            }
        })
        .collect();
    let crust = Grid { w, h, data: signed.iter().map(|&s| smoothstep(-150.0, 150.0, s) as f32).collect() };

    // --- 2. Plates --------------------------------------------------------
    let plate_count = match planet {
        Planet::Pangea => 10,
        Planet::Islands => 14,
        _ => 12,
    };
    let boundary_noise = fbm("realistic/plate_noise", -10, 3);
    let (plate, centroids) = grow_plates(
        &mut rng,
        w,
        h,
        plate_count,
        |i| {
            let (x, z) = pos(i);
            1.0 + 1.6 * (continental[i] as i32 as f64) + 0.9 * (boundary_noise.sample_2d(x, z) * 2.0 + 1.0).max(0.0)
        },
        &pos,
    );

    // --- 3. Motion ---------------------------------------------------------
    let mut cont_share = vec![0f64; plate_count];
    let mut cells = vec![0f64; plate_count];
    for i in 0..n {
        cells[plate.data[i] as usize] += 1.0;
        cont_share[plate.data[i] as usize] += continental[i] as i32 as f64;
    }
    // Pangea: the world ocean's plates dive under the supercontinent all
    // around (a "ring of fire").
    let (pcx, pcz) = blobs.first().map(|b| (b.0, b.1)).unwrap_or((width_km / 2.0, depth_km / 2.0));
    let plates: Vec<Plate> = (0..plate_count)
        .map(|p| {
            let share = cont_share[p] / cells[p].max(1.0);
            let (cx, cz) = centroids[p];
            let speed = lerp(share, 6.5, 2.0) * (0.6 + 0.8 * rng.next_f64());
            let mut a = rng.next_f64() * std::f64::consts::TAU;
            if planet == Planet::Pangea && share < 0.3 {
                a = libm::atan2(pcz - cz, pcx - cx) + (rng.next_f64() - 0.5) * 0.8;
            }
            Plate {
                vx: libm::cos(a) * speed,
                vz: libm::sin(a) * speed,
                spin: (rng.next_f64() - 0.5) * 0.8,
                cx,
                cz,
                density: rng.next_f64() + 0.5 * (1.0 - share),
            }
        })
        .collect();

    // --- 4. Boundaries ---------------------------------------------------
    // Per boundary cell: closing speed (cm/yr, + converging), unit normal
    // towards the other plate, the other plate, and its crust.
    let mut conv = vec![0f32; n];
    let mut normal = vec![(0f32, 0f32); n];
    let mut other = vec![u16::MAX; n];
    let mut other_cont = vec![false; n];
    let mut is_boundary = vec![false; n];
    for i in 0..n {
        let (x, z) = ((i % w) as isize, (i / w) as isize);
        let pa = plate.data[i];
        let (mut sum, mut nx, mut nz, mut count) = (0.0, 0.0, 0.0, 0.0);
        for &(dx, dz) in &[(1isize, 0isize), (-1, 0), (0, 1), (0, -1)] {
            let (a, b) = (x + dx, z + dz);
            if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                continue;
            }
            let j = a as usize + b as usize * w;
            let pb = plate.data[j];
            if pb == pa {
                continue;
            }
            let (px, pz) = pos(i);
            let (va, vb) = (plates[pa as usize].velocity(px, pz), plates[pb as usize].velocity(px, pz));
            let (rx, rz) = (vb.0 - va.0, vb.1 - va.1);
            sum += -(rx * dx as f64 + rz * dz as f64);
            nx += dx as f64;
            nz += dz as f64;
            count += 1.0;
            other[i] = pb;
            other_cont[i] = continental[j];
        }
        if count > 0.0 {
            is_boundary[i] = true;
            conv[i] = (sum / count) as f32;
            let l = (nx * nx + nz * nz).sqrt().max(1e-9);
            normal[i] = ((nx / l) as f32, (nz / l) as f32);
        }
    }
    // Smooth the closing speed and the normal along the boundary, so one
    // boundary has one character instead of flickering cell by cell.
    let smooth_r = ((300.0 / km).round() as usize).max(1);
    let masked = |f: &dyn Fn(usize) -> f32| {
        let g = Grid { w, h, data: (0..n).map(|i| if is_boundary[i] { f(i) } else { 0.0 }).collect() };
        g.blur(smooth_r, 2)
    };
    let weight = masked(&|_| 1.0);
    let conv_s = masked(&|i| conv[i]);
    let nx_s = masked(&|i| normal[i].0);
    let nz_s = masked(&|i| normal[i].1);
    let boundary_cells: Vec<usize> = (0..n).filter(|&i| is_boundary[i]).collect();
    let conv_at = |i: usize| conv_s.data[i] as f64 / (weight.data[i] as f64).max(1e-9);

    // Nearest boundary of each cell's own plate.
    let reach = 2600.0 / km;
    let (bdist, bsrc) = distance_field(w, h, &boundary_cells, reach, |a, b| plate.data[a] == plate.data[b]);
    // Ocean age: distance to the nearest diverging boundary.
    let ridges: Vec<usize> = boundary_cells.iter().copied().filter(|&i| conv_at(i) < -0.5).collect();
    let (rdist, rsrc) = distance_field(w, h, &ridges, 1e9, |_, _| true);

    // --- 5. Relief ---------------------------------------------------------
    let cont_noise = fbm("realistic/continent_relief", -11, 4);
    let shelf_noise = fbm("realistic/shelf_width", -11, 2);
    let arc_noise = fbm("realistic/arc_breakup", -8, 3);
    // Ranges are not uniform along their length: high massifs alternate
    // with lower saddles over ~1000–2000 km.
    let range_noise = fbm("realistic/range_variation", -11, 2);
    // Per cell: (elevation, mountain intensity, strike cos 2θ, sin 2θ).
    let relief_at = |i: usize| -> (f32, f32, f32, f32) {
        let (x, z) = pos(i);
        let mut strike = (1f32, 0f32);
        // Ocean floor from its age.
        let ocean = if rdist[i].is_finite() && rsrc[i] != u32::MAX {
            // Half the spreading rate, cm/yr (= 10 km/Myr); real ridges
            // spread at 2–15 cm/yr in total.
            let half_rate = (-conv_at(rsrc[i] as usize)).max(1.5);
            let age = (rdist[i] as f64 * km) / (half_rate * 10.0);
            // Depth by age, GDH1 plate model (Stein & Stein 1992): the
            // floor sinks as it cools, fast at first, levelling off near
            // 5.7 km.
            if age < 20.0 {
                -(2600.0 + 365.0 * age.sqrt())
            } else {
                -(5651.0 - 2473.0 * libm::exp(-0.0278 * age))
            }
        } else {
            -5600.0
        };
        // Continental interior: low cratons, plains and plateaus.
        let interior = 380.0 + 520.0 * cont_noise.sample_2d(x, z) * 2.0;
        let sd = signed[i];
        let shelf = 300.0 * (0.6 + 0.8 * (shelf_noise.sample_2d(x, z) + 0.5).clamp(0.0, 1.0));
        let mut e = if sd >= 0.0 {
            -140.0 + (interior + 140.0) * smoothstep(0.0, shelf, sd)
        } else {
            lerp(smoothstep(0.0, 160.0, -sd), -140.0, ocean)
        };

        // The nearest boundary of this cell's plate.
        let mut m = 0.0f64;
        if bsrc[i] != u32::MAX && bdist[i].is_finite() {
            let s = bsrc[i] as usize;
            let d = bdist[i] as f64 * km;
            let k = conv_at(s);
            let (pa, pb) = (plate.data[i] as usize, other[s] as usize);
            let own_cont = continental[s];
            let their_cont = other_cont[s];
            let along = (0.55 + 1.1 * (range_noise.sample_2d(x, z) + 0.35)).clamp(0.35, 1.25);
            let f = smoothstep(0.4, 5.0, k) * along;
            let fd = smoothstep(0.4, 4.0, -k);
            let overriding = pb < plates.len() && plates[pa].density < plates[pb].density;
            if f > 0.0 {
                match (own_cont, their_cont) {
                    (true, true) => {
                        // Collision: a high front range, and on the
                        // overriding side a plateau hundreds of km wide.
                        let front = 5200.0 * bump(d, 90.0, 110.0);
                        let plateau =
                            if overriding { 4300.0 * (1.0 - smoothstep(250.0, 250.0 + 900.0 * f, d)) } else { 0.0 };
                        let foreland = if overriding { 0.0 } else { -250.0 * bump(d, 320.0, 150.0) };
                        e += f * (front.max(plateau) + foreland);
                        m = m.max(f * (bump(d, 90.0, 170.0) + 0.5 * plateau / 4300.0));
                    }
                    (true, false) => {
                        // Andes: steep coast into the trench, cordillera
                        // 100–300 km inland, a high plain behind.
                        e = lerp(smoothstep(0.0, 90.0, d), lerp(f, e, -2500.0), e);
                        e += f * (4600.0 * bump(d, 170.0, 100.0) + 1500.0 * bump(d, 330.0, 140.0));
                        m = m.max(f * bump(d, 190.0, 160.0));
                    }
                    (false, true) => {
                        // Diving under a continent: a trench and the outer rise.
                        e += f * (-3800.0 * bump(d, 45.0, 35.0) + 300.0 * bump(d, 160.0, 60.0));
                    }
                    (false, false) => {
                        if overriding {
                            // Island arc, broken into separate volcanoes.
                            let breakup = (0.45 + 1.1 * arc_noise.sample_2d(x, z)).clamp(0.0, 1.0);
                            e += f * 5600.0 * bump(d, 140.0, 45.0) * (0.35 + 0.65 * breakup);
                            m = m.max(f * 0.7 * bump(d, 140.0, 60.0));
                        } else {
                            e += f * (-4200.0 * bump(d, 40.0, 35.0) + 300.0 * bump(d, 160.0, 60.0));
                        }
                    }
                }
            } else if fd > 0.0 && own_cont && their_cont {
                // Continental rift: a valley with raised shoulders.
                e += fd * (-1400.0 * bump(d, 0.0, 45.0) + 900.0 * bump(d, 90.0, 60.0));
                m = m.max(fd * 0.5 * bump(d, 90.0, 80.0));
            }
            // Ranges run along the boundary: the axis is perpendicular to
            // its normal, so 2θ_axis = 2θ_normal + 180°.
            let (nxv, nzv) = (nx_s.data[s] as f64, nz_s.data[s] as f64);
            let l = (nxv * nxv + nzv * nzv).sqrt();
            if l > 1e-6 {
                let (c, sn) = (nxv / l, nzv / l);
                strike = (-((c * c - sn * sn) as f32), -((2.0 * c * sn) as f32));
            }
        }
        (e as f32, m.min(1.0) as f32, strike.0, strike.1)
    };
    let cells: Vec<(f32, f32, f32, f32)> = (0..n).into_par_iter().map(relief_at).collect();
    let mut elevation = Grid { w, h, data: cells.iter().map(|c| c.0).collect() };
    let mut mountain = Grid { w, h, data: cells.iter().map(|c| c.1).collect() };
    let mut strike_c = Grid { w, h, data: cells.iter().map(|c| c.2).collect() };
    let mut strike_s = Grid { w, h, data: cells.iter().map(|c| c.3).collect() };

    old_orogens(seed, &mut rng, w, h, km, &signed, &mut elevation, &mut mountain, &mut strike_c, &mut strike_s);
    hotspots(&mut rng, planet, w, h, km, &plate, &plates, &crust, &mut elevation, &mut mountain);

    Tectonics { elevation, mountain, strike_c, strike_s, crust, plate }
}

/// Grows `count` plates from random seeds. Each plate spreads at its own
/// speed and every cell costs `cost(i)` to cross, so plates come out in
/// irregular sizes with ragged, natural-looking boundaries.
fn grow_plates(
    rng: &mut Xoroshiro128PlusPlus,
    w: usize,
    h: usize,
    count: usize,
    cost: impl Fn(usize) -> f64 + Sync,
    pos: &impl Fn(usize) -> (f64, f64),
) -> (Grid<u16>, Vec<(f64, f64)>) {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let n = w * h;
    let cost: Vec<f64> = (0..n).into_par_iter().map(&cost).collect();
    let mut plate = Grid::new(w, h, u16::MAX);
    let mut best = vec![f64::INFINITY; n];
    let mut heap = BinaryHeap::new();
    let speeds: Vec<f64> = (0..count).map(|_| 0.55 + 1.05 * rng.next_f64()).collect();
    for (p, _) in speeds.iter().enumerate() {
        let i = loop {
            let i = rng.next_bounded(n as u32) as usize;
            if best[i].is_infinite() {
                break i;
            }
        };
        best[i] = 0.0;
        heap.push(Reverse((0u64, i as u32, p as u16)));
    }
    while let Some(Reverse((cbits, i, p))) = heap.pop() {
        let i = i as usize;
        let c = f64::from_bits(cbits);
        if plate.data[i] != u16::MAX {
            continue;
        }
        plate.data[i] = p;
        let (x, z) = ((i % w) as isize, (i / w) as isize);
        for &(dx, dz, step) in &super::grid::NEIGHBOURS8 {
            let (a, b) = (x + dx, z + dz);
            if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                continue;
            }
            let j = a as usize + b as usize * w;
            if plate.data[j] != u16::MAX {
                continue;
            }
            let nc = c + step * cost[j] / speeds[p as usize];
            if nc < best[j] {
                best[j] = nc;
                heap.push(Reverse((nc.to_bits(), j as u32, p)));
            }
        }
    }
    let mut sums = vec![(0.0, 0.0, 0.0); count];
    for i in 0..n {
        let (x, z) = pos(i);
        let s = &mut sums[plate.data[i] as usize];
        s.0 += x;
        s.1 += z;
        s.2 += 1.0;
    }
    let centroids = sums.iter().map(|&(x, z, c): &(f64, f64, f64)| (x / c.max(1.0), z / c.max(1.0))).collect();
    (plate, centroids)
}

/// Worn-down ranges inside continents where plates collided long ago
/// (Appalachians, Urals, Scandinavian mountains): the boundaries of an
/// older, random plate layout, kept where they cross continental crust.
#[allow(clippy::too_many_arguments)]
fn old_orogens(
    seed: u64,
    rng: &mut Xoroshiro128PlusPlus,
    w: usize,
    h: usize,
    km: f64,
    signed: &[f64],
    elevation: &mut Grid<f32>,
    mountain: &mut Grid<f32>,
    strike_c: &mut Grid<f32>,
    strike_s: &mut Grid<f32>,
) {
    let n = w * h;
    let pos = |i: usize| (((i % w) as f64 + 0.5) * km, ((i / w) as f64 + 0.5) * km);
    let noise = OctaveNoise::new(derive_seed(seed, "realistic/old_plates"), &OctaveConfig::new(-10, &[1.0, 1.0, 1.0]));
    let (old, _) = grow_plates(
        rng,
        w,
        h,
        8,
        |i| {
            let (x, z) = pos(i);
            1.0 + (noise.sample_2d(x, z) * 2.0 + 1.0).max(0.0)
        },
        &pos,
    );
    // Half of the old boundaries were collisions; the rest left nothing.
    let strength: Vec<f64> =
        (0..64).map(|_| if rng.next_f64() < 0.55 { 0.5 + 0.5 * rng.next_f64() } else { 0.0 }).collect();
    let mut seeds = Vec::new();
    for i in 0..n {
        let (x, z) = (i % w, i / w);
        if x + 1 < w && old.data[i] != old.data[i + 1] || z + 1 < h && old.data[i] != old.data[i + w] {
            seeds.push(i);
        }
    }
    let (dist, src) = distance_field(w, h, &seeds, 400.0 / km, |_, _| true);
    let variation =
        OctaveNoise::new(derive_seed(seed, "realistic/old_range_variation"), &OctaveConfig::new(-9, &[1.0, 1.0]));
    for i in 0..n {
        if !dist[i].is_finite() || signed[i] < 150.0 {
            continue;
        }
        let s = src[i] as usize;
        let (a, b) = (old.data[s] as usize, old.data[(s + 1).min(n - 1)] as usize);
        let pair = (a * 8 + b) % strength.len();
        let k = strength[pair] * smoothstep(150.0, 450.0, signed[i]);
        if k <= 0.0 {
            continue;
        }
        let d = dist[i] as f64 * km;
        let (x, z) = pos(i);
        let v = (0.5 + 1.2 * variation.sample_2d(x, z)).clamp(0.15, 1.0);
        let lift = k * v * 1700.0 * bump(d, 0.0, 110.0);
        elevation.data[i] += lift as f32;
        let m = (k * v * 0.55 * bump(d, 0.0, 140.0)) as f32;
        if m > mountain.data[i] {
            mountain.data[i] = m;
            // Axis along the old boundary: perpendicular to the direction
            // to the boundary cell.
            let (sx, sz) = pos(s);
            let (dx, dz) = (x - sx, z - sz);
            let l = (dx * dx + dz * dz).sqrt();
            if l > 1e-6 {
                let (c, sn) = (dx / l, dz / l);
                strike_c.data[i] = -((c * c - sn * sn) as f32);
                strike_s.data[i] = -((2.0 * c * sn) as f32);
            }
        }
    }
}

/// Hotspots: plumes under the plates. As a plate drifts over one, it leaves
/// a chain of volcanoes, youngest (and highest) over the plume, older ones
/// eroded and sunk further down the chain (Hawaii–Emperor).
#[allow(clippy::too_many_arguments)]
fn hotspots(
    rng: &mut Xoroshiro128PlusPlus,
    planet: Planet,
    w: usize,
    h: usize,
    km: f64,
    plate: &Grid<u16>,
    plates: &[Plate],
    crust: &Grid<f32>,
    elevation: &mut Grid<f32>,
    mountain: &mut Grid<f32>,
) {
    let count = match planet {
        Planet::Islands => 16,
        Planet::Pangea => 5,
        _ => 7,
    };
    for _ in 0..count {
        let (x, z) = (rng.next_f64() * w as f64 * km, rng.next_f64() * h as f64 * km);
        let (i, j) = (((x / km) as usize).min(w - 1), ((z / km) as usize).min(h - 1));
        let p = &plates[plate.get(i, j) as usize];
        let (vx, vz) = p.velocity(x, z);
        let l = (vx * vx + vz * vz).sqrt().max(1e-6);
        let (tx, tz) = (-vx / l, -vz / l);
        let length = 5 + rng.next_bounded(8) as usize;
        let top = 5300.0 + 900.0 * rng.next_f64();
        let radius = 55.0 + 50.0 * rng.next_f64();
        for k in 0..length {
            let along = k as f64 * (230.0 + 80.0 * rng.next_f64());
            let side = (rng.next_f64() - 0.5) * 90.0;
            let (sx, sz) = (x + tx * along - tz * side, z + tz * along + tx * side);
            let height = top * libm::pow(0.8, k as f64);
            let r = radius * (1.0 + 0.15 * k as f64);
            let reach = (3.0 * r / km).ceil() as isize;
            let (ci, cj) = ((sx / km) as isize, (sz / km) as isize);
            for b in cj - reach..=cj + reach {
                for a in ci - reach..=ci + reach {
                    if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                        continue;
                    }
                    let idx = a as usize + b as usize * w;
                    let (px, pz) = ((a as f64 + 0.5) * km, (b as f64 + 0.5) * km);
                    let d = ((px - sx) * (px - sx) + (pz - sz) * (pz - sz)).sqrt();
                    let oceanic = 1.0 - crust.data[idx] as f64;
                    // On a continent a plume makes a volcanic plateau
                    // (Yellowstone), not a 6 km cone.
                    let lift = height * bump(d, 0.0, r) * lerp(oceanic, 0.15, 1.0);
                    elevation.data[idx] += lift as f32;
                    mountain.data[idx] = mountain.data[idx].max((0.5 * bump(d, 0.0, r * 1.3) * oceanic) as f32);
                }
            }
        }
    }
}

/// Shape statistics of the land on a grid (elevation ≥ 0 is land), used to
/// compare generated continents with the real Earth.
#[derive(Clone, Debug)]
pub struct LandShape {
    /// Connected landmasses (4-neighbour) as (cells, coast cells), largest
    /// first.
    pub masses: Vec<(usize, usize)>,
    /// All cells of the grid.
    pub cells: usize,
}

impl LandShape {
    fn land(&self) -> usize {
        self.masses.iter().map(|m| m.0).sum()
    }
    /// Share of the grid that is land.
    pub fn land_share(&self) -> f64 {
        self.land() as f64 / self.cells.max(1) as f64
    }
    /// Landmasses of at least `share` of all land (continents, not islands).
    pub fn continents(&self, share: f64) -> usize {
        let land = self.land() as f64;
        self.masses.iter().filter(|m| m.0 as f64 >= share * land).count()
    }
    /// Mean coastline complexity of the continents (landmasses of at least
    /// `share` of the land): coast cells / √cells. A disc scores ~3.5;
    /// peninsulas, gulfs and inland seas score more.
    pub fn coast_index(&self, share: f64) -> f64 {
        let land = self.land() as f64;
        let c: Vec<f64> = self
            .masses
            .iter()
            .filter(|m| m.0 as f64 >= share * land)
            .map(|&(a, c)| c as f64 / (a as f64).sqrt())
            .collect();
        c.iter().sum::<f64>() / c.len().max(1) as f64
    }
    /// Share of the land in islands (landmasses under `share` of the land).
    pub fn island_share(&self, share: f64) -> f64 {
        let land = self.land() as f64;
        self.masses.iter().filter(|m| (m.0 as f64) < share * land).map(|m| m.0 as f64).sum::<f64>() / land.max(1.0)
    }
}

pub fn land_shape(elevation: &Grid<f32>) -> LandShape {
    let (w, h) = (elevation.w, elevation.h);
    let land = |i: usize, j: usize| elevation.get(i, j) >= 0.0;
    let nb = [(1isize, 0isize), (-1, 0), (0, 1), (0, -1)];
    let coastal = |i: usize, j: usize| {
        nb.iter().any(|&(a, b)| {
            let (x, y) = (i as isize + a, j as isize + b);
            x >= 0 && y >= 0 && x < w as isize && y < h as isize && !land(x as usize, y as usize)
        })
    };
    let mut seen = vec![false; w * h];
    let mut masses = Vec::new();
    for j in 0..h {
        for i in 0..w {
            if !land(i, j) || seen[i + j * w] {
                continue;
            }
            // Flood fill one landmass.
            let mut stack = vec![(i, j)];
            seen[i + j * w] = true;
            let (mut size, mut coast) = (0, 0);
            while let Some((x, y)) = stack.pop() {
                size += 1;
                coast += coastal(x, y) as usize;
                for (a, b) in nb {
                    let (nx, ny) = (x as isize + a, y as isize + b);
                    if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    let (nx, ny) = (nx as usize, ny as usize);
                    if land(nx, ny) && !seen[nx + ny * w] {
                        seen[nx + ny * w] = true;
                        stack.push((nx, ny));
                    }
                }
            }
            masses.push((size, coast));
        }
    }
    masses.sort_unstable_by(|a, b| b.cmp(a));
    LandShape { masses, cells: w * h }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The relief has the features of a real plate layout: continents and
    /// oceans in the planned shares, high ranges on the mountain belts,
    /// abyssal depths, and plates that cover everything.
    #[test]
    fn plates_build_continents_ranges_and_ocean_basins() {
        for (planet, lo, hi) in
            [(Planet::Continents, 0.25, 0.5), (Planet::Pangea, 0.22, 0.45), (Planet::Islands, 0.03, 0.2)]
        {
            let (w, h) = (256, 256);
            let t = build(3, planet, w, h, 40_000.0 / w as f64);
            let n = (w * h) as f64;
            let land = t.elevation.data.iter().filter(|&&e| e >= 0.0).count() as f64 / n;
            assert!((lo..hi).contains(&land), "{planet:?}: land share {land}");
            assert!(t.plate.data.iter().all(|&p| p != u16::MAX), "{planet:?}: cell without a plate");
            let max = t.elevation.data.iter().copied().fold(f32::MIN, f32::max);
            assert!(max > 3500.0, "{planet:?}: highest point {max} m");
            // Mountain belts stand high above the rest of the land.
            let mean = |f: &dyn Fn(f32) -> bool| {
                let v: Vec<f64> = t
                    .elevation
                    .data
                    .iter()
                    .zip(&t.mountain.data)
                    .filter(|&(&e, &m)| e >= 0.0 && f(m))
                    .map(|(&e, _)| e as f64)
                    .collect();
                v.iter().sum::<f64>() / v.len().max(1) as f64
            };
            if planet != Planet::Islands {
                let (ranges, plains) = (mean(&|m| m > 0.5), mean(&|m| m < 0.1));
                assert!(ranges > plains + 1000.0, "{planet:?}: ranges {ranges} m, plains {plains} m");
            }
            // Old ocean floor lies 4–6 km deep.
            let mut sea: Vec<f32> = t.elevation.data.iter().copied().filter(|&e| e < -1000.0).collect();
            sea.sort_by(f32::total_cmp);
            let median = sea[sea.len() / 2];
            assert!((-6000.0..-3500.0).contains(&median), "{planet:?}: median ocean depth {median}");
        }
    }

    /// The Continents planet looks like Earth's map, not like one or two
    /// blobs: several separate continents (Earth has 5 at this
    /// resolution), none holding most of the land, about Earth's share of
    /// land, and coasts at least as intricate as a disc's (3.5).
    #[test]
    fn continents_are_many_separate_and_intricate() {
        let (w, km) = (384, 40_000.0 / 384.0);
        let (mut total, mut largest) = (0, 0.0);
        for seed in 1..=8 {
            let t = build(seed, Planet::Continents, w, w, km);
            let s = land_shape(&t.elevation);
            let count = s.continents(0.02);
            let big = s.masses[0].0 as f64 / s.masses.iter().map(|m| m.0).sum::<usize>() as f64;
            let land = s.land_share();
            println!(
                "seed {seed}: {count} continents, largest {big:.2}, land {land:.2}, coast {:.1}",
                s.coast_index(0.02)
            );
            assert!(count >= 3, "seed {seed}: only {count} continents");
            assert!(big < 0.7, "seed {seed}: one landmass holds {big:.2} of the land");
            assert!((0.12..0.4).contains(&land), "seed {seed}: land share {land:.2}");
            assert!(s.coast_index(0.02) > 5.0, "seed {seed}: coast index {:.1}", s.coast_index(0.02));
            total += count;
            largest += big;
        }
        assert!(total as f64 / 8.0 >= 4.5, "mean {} continents", total as f64 / 8.0);
        assert!(largest / 8.0 < 0.5, "mean largest landmass {:.2}", largest / 8.0);
    }
}
