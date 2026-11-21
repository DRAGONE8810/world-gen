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
//! 1. **Continental crust.** A noise field picks which ~40 % of the surface
//!    is light continental crust (12 % for the Islands planet, one blob for
//!    Pangea). Its edge, ~100 km beyond the coast, is the shelf break.
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
        _ => {
            let k = 5 + rng.next_bounded(3) as usize;
            for _ in 0..k {
                blobs.push((
                    width_km * (0.1 + 0.8 * rng.next_f64()),
                    depth_km * (0.1 + 0.8 * rng.next_f64()),
                    (0.05 + 0.06 * rng.next_f64()) * width_km,
                ));
            }
            (0.40, 0.55, 0.15)
        }
    };
    let field_at = |i: usize| -> f32 {
        let (x, z) = pos(i);
        let wx = x + 1100.0 * warp_x.sample_2d(x, z);
        let wz = z + 1100.0 * warp_z.sample_2d(x, z);
        let mut f: f64 = -1.0;
        for &(bx, bz, r) in &blobs {
            let d = ((wx - bx) * (wx - bx) + (wz - bz) * (wz - bz)).sqrt();
            f = f.max(1.0 - d / r);
        }
        f += noise_weight * crust_noise.sample_2d(wx, wz) * 2.2 + fine_weight * fine_noise.sample_2d(wx, wz) * 2.2;
        // The world border is open ocean: keep continents off the edges so
        // no coast is cut by the border.
        let edge = (x.min(width_km - x).min(z).min(depth_km - z)) / width_km;
        f -= 3.0 * (1.0 - smoothstep(0.0, 0.08, edge));
        f as f32
    };
    let field = Grid { w, h, data: (0..n).into_par_iter().map(field_at).collect() };
    let threshold = field.quantile(1.0 - fraction) as f64;
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
}
