//! Climate of a realistic planet: temperature from latitude and ocean
//! currents, rainfall from the global wind belts and the mountains in
//! their way.
//!
//! The classic generator draws temperature and humidity from noise. Here
//! they follow the same physics that shapes Earth's climate zones:
//!
//! * **Temperature** falls from ~28 °C at the equator to −25 °C at the
//!   poles (Antarctica colder still), and cold or warm **ocean currents**
//!   shift it along coasts: cold water off subtropical west coasts (the
//!   Atacama and Namib are deserts by the sea), warm water off mid-latitude
//!   west coasts (Europe is mild for its latitude).
//! * **Rain** has a zonal base: the wet tropics (ITCZ), the dry subtropics
//!   at ~25° (Sahara, Australia), wet mid-latitudes (~50°) and dry poles.
//!   Then moisture is carried by the **prevailing winds**: easterly trade
//!   winds below 30°, westerlies from 30° to 60°, polar easterlies above.
//!   Air picks up moisture over the sea, loses it as it travels inland,
//!   rains it out when forced up a mountain range and arrives dry on the
//!   other side: the **rain shadow** (Patagonia, the Great Basin, Tibet).
//! * **Humidity** for the biome rules is the *aridity index*, rain divided
//!   by potential evaporation (which grows with temperature), so 400 mm a
//!   year is a desert in the tropics but a forest in the boreal zone.
//!
//! Altitude cooling (6.5 °C per km) is applied per block by the biome
//! code, so temperatures here are reduced to sea level.

use super::grid::Grid;
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::rng::derive_seed;
use crate::spline::{lerp, smoothstep, Curve};
use rayon::prelude::*;

pub struct Climate {
    /// Mean annual temperature at sea level, °C.
    pub temperature: Grid<f32>,
    /// Annual precipitation, mm.
    pub precipitation: Grid<f32>,
    /// Aridity index: precipitation / potential evapotranspiration.
    pub aridity: Grid<f32>,
}

/// Latitude in degrees of row `j` (north is up, at negative z).
#[inline]
pub fn latitude(j: usize, h: usize) -> f64 {
    90.0 - (j as f64 + 0.5) / h as f64 * 180.0
}

/// Zonal mean sea-level temperature, °C.
pub fn zonal_temperature(lat: f64) -> f64 {
    let a = lat.abs() / 90.0;
    let t = 28.0 - 52.0 * libm::pow(a, 1.6);
    // The south pole sits on a high ice continent: far colder.
    t - 10.0 * smoothstep(60.0, 90.0, -lat)
}

/// Zonal base precipitation, mm/yr.
pub fn zonal_precipitation(lat: f64) -> f64 {
    let a = lat.abs();
    let g = |c: f64, s: f64| {
        let t = (a - c) / s;
        libm::exp(-t * t)
    };
    450.0 + 2300.0 * g(0.0, 11.0) + 850.0 * g(50.0, 13.0) - 300.0 * smoothstep(62.0, 85.0, a)
}

/// Wind direction along x at a latitude: −1 easterlies (blowing towards
/// −x), +1 westerlies; strength near 0 at the belt edges (30°, 60°).
fn wind(lat: f64) -> (f64, f64) {
    let a = lat.abs();
    let dir = if (30.0..60.0).contains(&a) { 1.0 } else { -1.0 };
    let edge = [30.0, 60.0].iter().map(|&b| (a - b).abs()).fold(f64::INFINITY, f64::min);
    (dir, 0.35 + 0.65 * smoothstep(0.0, 12.0, edge))
}

/// `elevation` in metres; `km` is the cell size. `wobble` (degrees)
/// bends the climate belts north and south along their length, as
/// monsoons and the uneven heating of land and sea do on Earth, so a
/// procedural planet's deserts are not straight stripes (0 for the real
/// Earth, whose geography already does it).
pub fn simulate(elevation: &Grid<f32>, km: f64, seed: u64, wobble: f64) -> Climate {
    let (w, h) = (elevation.w, elevation.h);
    let belt_noise =
        OctaveNoise::new(derive_seed(seed, "realistic/climate_belts"), &OctaveConfig::new(-12, &[1.0, 0.6]));
    let wob = |i: usize, j: usize| {
        let (x, z) = ((i as f64 + 0.5) * km, (j as f64 + 0.5) * km);
        wobble * (belt_noise.sample_2d(x, z) * 2.2).clamp(-1.0, 1.0)
    };
    let land = |i: usize, j: usize| elevation.get(i, j) >= 0.0;

    // Rows (latitude circles) are independent: simulate them in parallel.
    let rows: Vec<(Vec<f32>, Vec<f32>)> = (0..h)
        .into_par_iter()
        .map(|j| {
            let mut row_t = vec![0f32; w];
            let mut row_p = vec![0f32; w];
            let lat = latitude(j, h);
            let a = lat.abs();
            // Distance (km) along the row to the nearest land to the east and
            // to the west, for the ocean currents.
            let mut to_east = vec![f64::INFINITY; w];
            let mut to_west = vec![f64::INFINITY; w];
            let mut d = f64::INFINITY;
            for i in (0..w).rev() {
                d = if land(i, j) { 0.0 } else { d + km };
                to_east[i] = d;
            }
            d = f64::INFINITY;
            for (i, tw) in to_west.iter_mut().enumerate() {
                d = if land(i, j) { 0.0 } else { d + km };
                *tw = d;
            }
            let sub = smoothstep(8.0, 18.0, a) * (1.0 - smoothstep(35.0, 45.0, a));
            let mid = smoothstep(38.0, 46.0, a) * (1.0 - smoothstep(62.0, 72.0, a));
            let current = |i: usize| {
                let (de, dw) = (to_east[i], to_west[i]);
                let e = |d: f64, l: f64| if d.is_finite() { libm::exp(-d / l) } else { 0.0 };
                sub * (-5.0 * e(de, 900.0) + 2.5 * e(dw, 900.0)) + mid * (5.0 * e(de, 1500.0) - 4.0 * e(dw, 1000.0))
            };

            let (dir, strength) = wind(lat);
            // How much rain falls even far from the sea: tropical forests
            // recycle their own moisture (half of the Amazon's rain), so the
            // tropics stay wet deep inland.
            // Under the subtropical highs the air sinks and stays dry instead
            // (the cores of the Sahara, Arabia and Australia).
            let floor_at = |l: f64| {
                let g = |c: f64, s: f64| libm::exp(-((l.abs() - c) / s) * ((l.abs() - c) / s));
                0.5 + 0.3 * g(0.0, 12.0) - 0.3 * g(25.0, 6.0)
            };
            let t0 = zonal_temperature(lat);
            // Sweep downwind twice: the first pass only warms up the air mass
            // (the row is treated as wrapping around, like a real latitude
            // circle), the second records.
            let cells: Vec<usize> = if dir > 0.0 { (0..w).collect() } else { (0..w).rev().collect() };
            let mut q: f64 = 1.0; // moisture, 1 = saturated sea air
            let mut anomaly: f64 = 0.0; // temperature carried from the sea
                                        // Share of the moisture left after the ranges crossed so far: air
                                        // rising over a range cools (~6 °C/km) and loses about a third of
                                        // its water vapour per km of ascent, which leaves the lee dry (the
                                        // rain shadow). Other weather systems refill it over ~1500 km.
            let mut shadow: f64 = 1.0;
            let shadow_recovery = 1.0 - libm::exp(-km / 1500.0);
            let ocean_recharge = 1.0 - libm::exp(-km / 600.0);
            let inland_loss = km / (3200.0 * strength);
            let anomaly_decay = libm::exp(-km / (700.0 * strength));
            for pass in 0..2 {
                let mut prev_h = elevation.get(cells[0], j).max(0.0) as f64;
                for &i in &cells {
                    let e = elevation.get(i, j) as f64;
                    let lat_i = lat + wob(i, j);
                    let p0 = zonal_precipitation(lat_i);
                    let floor = floor_at(lat_i);
                    if e < 0.0 {
                        q += (1.0 - q) * ocean_recharge;
                        shadow += (1.0 - shadow) * ocean_recharge;
                        anomaly = current(i);
                        prev_h = 0.0;
                        if pass == 1 {
                            // The sea stores summer heat: polar seas freeze
                            // only near the poles.
                            let sea_warmth = 4.0 * smoothstep(45.0, 75.0, a);
                            row_t[i] = (t0 + anomaly + sea_warmth) as f32;
                            row_p[i] = (p0 * (0.8 + 0.2 * q)) as f32;
                        }
                        continue;
                    }
                    // Rising air rains (orographic lift); sinking air dries.
                    let rise = (e - prev_h) / 1000.0 / (km / 50.0);
                    let prev_e = prev_h;
                    prev_h = e;
                    let oro = if rise > 0.0 { 1.0 + 3.0 * rise.min(3.0) } else { (1.0 + 2.0 * rise).max(0.3) };
                    // Rain grows with height up to a few km, then the thin cold
                    // air holds little water.
                    let altitude = libm::exp(-(e - 2000.0).max(0.0) / 3000.0);
                    // Trade winds bring warm, wet sea air onto subtropical east
                    // coasts (Florida, south-east China, eastern Brazil and
                    // Australia), which are green while the west coasts at the
                    // same latitude are deserts.
                    let a_i = lat_i.abs();
                    let trades =
                        if dir < 0.0 { smoothstep(8.0, 16.0, a_i) * (1.0 - smoothstep(30.0, 38.0, a_i)) } else { 0.0 };
                    let wet = q * shadow;
                    let rain = p0
                        * lerp(wet, floor * (0.4 + 0.6 * shadow), 1.0)
                        * oro
                        * altitude
                        * (1.0 + 2.2 * trades * wet * wet * wet);
                    q *= libm::exp(-inland_loss * (0.5 + 0.5 * oro));
                    if rise > 0.0 {
                        shadow *= libm::exp(-0.4 * (e - prev_e).max(0.0) / 1000.0);
                    } else {
                        shadow += (1.0 - shadow) * shadow_recovery;
                    }
                    anomaly *= anomaly_decay;
                    if pass == 1 {
                        row_t[i] = (t0 + anomaly) as f32;
                        row_p[i] = rain as f32;
                    }
                }
            }
            (row_t, row_p)
        })
        .collect();
    let temperature = Grid { w, h, data: rows.iter().flat_map(|r| r.0.iter().copied()).collect() };
    let mut precipitation = Grid { w, h, data: rows.iter().flat_map(|r| r.1.iter().copied()).collect() };
    add_monsoon(&mut precipitation, elevation, km);
    // Weather systems also mix north–south and over a few hundred km.
    let r = ((250.0 / km).round() as usize).max(1);
    let precipitation = precipitation.blur(r, 2);
    let temperature = temperature.blur(((150.0 / km).round() as usize).max(1), 1);
    let aridity = Grid {
        w,
        h,
        data: precipitation
            .data
            .iter()
            .zip(&temperature.data)
            .map(|(&p, &t)| (p as f64 / potential_evaporation(t as f64)) as f32)
            .collect(),
    };
    Climate { temperature, precipitation, aridity }
}

/// Summer monsoons. In summer the tropical rain belt follows the sun
/// poleward, and land heated by it draws moist sea air inland from the
/// equator side: India, the Sahel, South-East Asia, northern Australia and
/// Mexico get most of their rain this way, while a range in the way (the
/// Himalaya) keeps it out of the land behind. Swept along each column
/// from the equator to 35° in both hemispheres; the moisture refills over
/// the sea and rains out over land over ~1000 km, and faster uphill.
fn add_monsoon(precipitation: &mut Grid<f32>, elevation: &Grid<f32>, km: f64) {
    let (w, h) = (elevation.w, elevation.h);
    let recharge = 1.0 - libm::exp(-km / 400.0);
    let inland = libm::exp(-km / 1000.0);
    let add: Vec<Vec<(usize, f32)>> = (0..w)
        .into_par_iter()
        .map(|i| {
            let mut out = Vec::new();
            for north in [true, false] {
                // Rows from the equator towards the pole.
                let rows: Vec<usize> = if north {
                    (0..h).rev().filter(|&j| (0.0..35.0).contains(&latitude(j, h))).collect()
                } else {
                    (0..h).filter(|&j| (-35.0..0.0).contains(&latitude(j, h))).collect()
                };
                let mut q: f64 = 0.0;
                let mut prev = 0.0f64;
                for j in rows {
                    let e = elevation.get(i, j) as f64;
                    if e < 0.0 {
                        q += (1.0 - q) * recharge;
                        prev = 0.0;
                        continue;
                    }
                    let a = latitude(j, h).abs();
                    let season = smoothstep(4.0, 10.0, a) * (1.0 - smoothstep(20.0, 30.0, a));
                    let rise = (e - prev).max(0.0) / 1000.0;
                    prev = e;
                    let oro = 1.0 + 3.0 * (rise / (km / 50.0)).min(3.0);
                    out.push((i + j * w, (1100.0 * season * q * oro) as f32));
                    q *= inland * libm::exp(-0.5 * rise);
                }
            }
            out
        })
        .collect();
    for (k, v) in add.into_iter().flatten() {
        precipitation.data[k] += v;
    }
}

/// Potential evapotranspiration, mm/yr (a Thornthwaite-like fit).
pub fn potential_evaporation(t: f64) -> f64 {
    (250.0 + 50.0 * t).max(150.0)
}

/// Mean annual temperature (°C) → the biome code's −1..1 scale. The knots
/// are the band edges of `biome::TempBand`: frozen below −2 °C (tundra and
/// ice), cold to 6 °C (boreal), temperate to 15 °C, warm to 22 °C, hot above.
pub fn biome_temperature(t: f64) -> f64 {
    thread_local! {
        static C: Curve = Curve::new(&[(-40.0, -1.4), (-20.0, -1.0), (-2.0, -0.45), (6.0, -0.15), (15.0, 0.18), (22.0, 0.48), (30.0, 0.85), (40.0, 1.0)]);
    }
    C.with(|c| c.eval(t.clamp(-40.0, 40.0)))
}

/// Aridity index → the biome code's humidity (−1..1). Knots at the
/// `biome::HumidBand` edges: arid below 0.2 (deserts), dry to 0.5
/// (steppe, savanna), medium to 0.8, wet to 1.5, very wet above
/// (rainforest).
pub fn biome_humidity(aridity: f64) -> f64 {
    let l = libm::log10(aridity.max(1e-3));
    thread_local! {
        static C: Curve = Curve::new(&[
            (-3.0, -1.0),
            (-1.3, -0.8),
            (-0.699, -0.33),
            (-0.301, -0.1),
            (-0.097, 0.1),
            (0.176, 0.33),
            (0.477, 0.7),
            (1.0, 1.0),
        ]);
    }
    C.with(|c| c.eval(l.clamp(-3.0, 1.0)))
}

/// Inverse of a rising function by bisection on `[lo, hi]`.
fn invert(f: impl Fn(f64) -> f64, y: f64, mut lo: f64, mut hi: f64) -> f64 {
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if f(mid) < y {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The biome code's temperature → °C (inverse of [`biome_temperature`]).
pub fn temperature_from_biome(t: f64) -> f64 {
    invert(biome_temperature, t, -40.0, 40.0)
}

/// The biome code's humidity → aridity index (inverse of [`biome_humidity`]).
pub fn aridity_from_biome(h: f64) -> f64 {
    libm::pow(10.0, invert(|l| biome_humidity(libm::pow(10.0, l)), h, -3.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_follow_latitude() {
        assert!(zonal_temperature(0.0) > 25.0);
        assert!(zonal_temperature(45.0) > 8.0 && zonal_temperature(45.0) < 16.0);
        assert!(zonal_temperature(89.0) < -20.0);
        assert!(zonal_temperature(-89.0) < zonal_temperature(89.0));
        assert!(zonal_precipitation(0.0) > 2000.0);
        assert!(zonal_precipitation(25.0) < 500.0);
        assert!(zonal_precipitation(50.0) > zonal_precipitation(25.0) * 2.0);
    }

    #[test]
    fn rain_shadow_and_inland_drying() {
        // A westerly latitude (45° N): sea on the west, a range, then a
        // long plain. The windward slope is wetter than the lee.
        let (w, h) = (200, 3);
        let mut e = Grid::new(w, h, 200f32);
        for j in 0..h {
            for i in 0..20 {
                e.set(i, j, -3000.0);
            }
            for i in 40..50 {
                e.set(i, j, 200.0 + (i - 39) as f32 * 400.0);
            }
            for i in 50..60 {
                e.set(i, j, 4200.0 - (i - 49) as f32 * 400.0);
            }
        }
        // Every row has the profile; read the one at 45° N. (Rows of half
        // a degree, so the north–south mixing stays among similar rows.)
        let tall = Grid { w, h: 360, data: (0..120).flat_map(|_| e.data.clone()).collect() };
        let c = simulate(&tall, 40.0, 1, 0.0);
        let j = (0..tall.h).min_by_key(|&j| ((latitude(j, tall.h) - 45.0).abs() * 100.0) as i64).unwrap();
        let p = |i| c.precipitation.get(i, j);
        assert!(p(45) > p(70), "windward {} vs lee {}", p(45), p(70));
        assert!(p(25) > p(190), "coast {} vs interior {}", p(25), p(190));
    }

    #[test]
    fn biome_scales() {
        assert!(biome_temperature(-10.0) < -0.45 && biome_temperature(10.0) > -0.15);
        assert!(biome_humidity(0.1) < -0.33 && biome_humidity(2.0) > 0.33);
        assert!((biome_humidity(0.5) - -0.1).abs() < 0.01);
        for t in [-30.0, -5.0, 3.0, 12.0, 19.0, 27.0] {
            assert!((temperature_from_biome(biome_temperature(t)) - t).abs() < 0.01);
        }
        for a in [0.05, 0.3, 0.7, 1.2, 4.0] {
            assert!((aridity_from_biome(biome_humidity(a)) / a - 1.0).abs() < 0.01);
        }
    }
}
