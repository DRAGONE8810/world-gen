//! The real Earth's relief, for the "Earth" planet.
//!
//! `data/earth_relief.bin` holds NOAA's ETOPO 2022 global relief model
//! (land topography and ocean bathymetry, public domain) averaged to
//! 0.1° cells: 3600 × 1800 heights, ~11 km apart at the equator. Heights
//! are rounded to 4 m on land and 16 m at sea and stored as the difference
//! from a prediction made from the cells to the west, north and north-west
//! (the LOCO-I "median edge detector" of lossless JPEG), then deflated:
//! 3.9 MB for the whole planet. `make_earth_relief.py` rebuilds it.
//!
//! The file is compiled into the library, so the generator still does no
//! I/O, and decoding it takes a few tens of milliseconds.

use super::grid::Grid;

static DATA: &[u8] = include_bytes!("../../data/earth_relief.bin");

/// Decodes the whole relief: metres, north-west corner first.
pub fn relief() -> Grid<f32> {
    let header = |k: usize| u32::from_le_bytes([DATA[k], DATA[k + 1], DATA[k + 2], DATA[k + 3]]);
    assert_eq!(&DATA[0..4], b"WGER", "earth_relief.bin: bad magic");
    assert_eq!(header(4), 1, "earth_relief.bin: unknown version");
    let (w, h) = (header(8) as usize, header(12) as usize);
    let land_step = header(16) as i32;
    let sea_step = header(20) as i32;
    let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(&DATA[24..]).expect("earth_relief.bin: corrupt");
    let mut values = vec![0i32; w * h];
    let mut p = 0;
    for j in 0..h {
        for i in 0..w {
            let mut x: u64 = 0;
            let mut shift = 0;
            loop {
                let b = bytes[p];
                p += 1;
                x |= ((b & 127) as u64) << shift;
                if b < 128 {
                    break;
                }
                shift += 7;
            }
            let r = ((x >> 1) as i64 ^ -((x & 1) as i64)) as i32;
            let a = if i > 0 { values[j * w + i - 1] } else { 0 };
            let b = if j > 0 { values[(j - 1) * w + i] } else { 0 };
            let c = if i > 0 && j > 0 { values[(j - 1) * w + i - 1] } else { 0 };
            let pred = if c >= a.max(b) {
                a.min(b)
            } else if c <= a.min(b) {
                a.max(b)
            } else {
                a + b - c
            };
            values[j * w + i] = pred + r;
        }
    }
    Grid {
        w,
        h,
        data: values.iter().map(|&v| if v >= 0 { (v * land_step) as f32 } else { (v * sea_step) as f32 }).collect(),
    }
}

/// The world's large lakes: a point on each (latitude, longitude) and its
/// surface level (m). At a few tens of km per cell the relief grid also has
/// hollows that are not lakes (a gorge narrower than a cell looks like a
/// dam, as on the Congo), so on the real Earth only the hollows holding a
/// known lake fill, and only up to the lake's real level: filled to the
/// rim, the Caspian basin would spread far over the steppe, as it did in
/// the Ice Age. (Not Lake Chad: a few metres deep, it is too shallow for
/// this grid.)
const LAKES: [(f64, f64, f64); 26] = [
    (42.0, 50.5, -28.0),    // Caspian Sea
    (-1.0, 33.0, 1134.0),   // Victoria
    (-6.0, 29.5, 773.0),    // Tanganyika
    (-12.0, 34.5, 500.0),   // Malawi
    (3.6, 36.0, 360.0),     // Turkana
    (53.5, 108.0, 456.0),   // Baikal
    (46.5, 74.5, 341.0),    // Balkhash
    (42.4, 77.3, 1607.0),   // Issyk-Kul
    (36.9, 100.2, 3196.0),  // Qinghai
    (38.6, 42.9, 1640.0),   // Van
    (37.7, 45.3, 1270.0),   // Urmia
    (61.0, 31.5, 5.0),      // Ladoga
    (61.7, 35.5, 33.0),     // Onega
    (58.9, 13.3, 44.0),     // Vänern
    (47.7, -87.5, 183.0),   // Superior
    (44.0, -87.0, 176.0),   // Michigan
    (44.8, -82.4, 176.0),   // Huron
    (42.2, -81.2, 174.0),   // Erie
    (43.7, -77.9, 75.0),    // Ontario
    (66.0, -121.0, 156.0),  // Great Bear
    (61.7, -114.0, 156.0),  // Great Slave
    (59.2, -109.5, 213.0),  // Athabasca
    (57.3, -102.3, 337.0),  // Reindeer
    (52.5, -97.5, 217.0),   // Winnipeg
    (11.6, -85.4, 32.0),    // Nicaragua
    (-15.8, -69.4, 3812.0), // Titicaca
];

/// Lake level (m) per cell for the real Earth: the cells of each hollow
/// of the flooded relief (`filled` above `elevation`) holding one of
/// `LAKES` that lie below its real level; `NO_LAKE` elsewhere.
pub fn lakes(elevation: &Grid<f32>, filled: &[f32], sea: &[f32]) -> Grid<f32> {
    let (w, h) = (elevation.w, elevation.h);
    let mut lake = Grid::new(w, h, super::NO_LAKE);
    let hollow = |k: usize| sea[k] < 0.5 && filled[k] as f64 - elevation.data[k] as f64 > 2.0;
    for &(lat, lon, surface) in &LAKES {
        let ci = (((lon + 180.0) / 360.0 * w as f64) as isize).clamp(0, w as isize - 1);
        let cj = (((90.0 - lat) / 180.0 * h as f64) as isize).clamp(0, h as isize - 1);
        // The lake's point may fall just outside its hollow at this
        // resolution: take the nearest hollow cell within two cells.
        let mut start = None;
        for r in 0..=2isize {
            for dj in -r..=r {
                for di in -r..=r {
                    let (i, j) = (ci + di, cj + dj);
                    if start.is_none() && i >= 0 && j >= 0 && i < w as isize && j < h as isize {
                        let k = i as usize + j as usize * w;
                        if hollow(k) {
                            start = Some(k);
                        }
                    }
                }
            }
        }
        let Some(start) = start else { continue };
        // Its hollow (connected cells filled to the same rim), as far as
        // it lies below the lake's surface.
        let rim = filled[start];
        let level = (surface as f32).min(rim);
        let wet = |k: usize| hollow(k) && (filled[k] - rim).abs() < 1.0 && elevation.data[k] < level;
        if !wet(start) {
            continue;
        }
        let mut stack = vec![start];
        lake.data[start] = level;
        while let Some(c) = stack.pop() {
            let (x, z) = ((c % w) as isize, (c / w) as isize);
            for &(dx, dz, _) in &super::grid::NEIGHBOURS8 {
                let (a, b) = (x + dx, z + dz);
                if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                    continue;
                }
                let k = a as usize + b as usize * w;
                if lake.data[k] <= super::NO_LAKE && wet(k) {
                    lake.data[k] = level;
                    stack.push(k);
                }
            }
        }
    }
    lake
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Height at a latitude/longitude (nearest 0.1° cell).
    fn at(g: &Grid<f32>, lat: f64, lon: f64) -> f32 {
        let i = (((lon + 180.0) / 360.0 * g.w as f64) as usize).min(g.w - 1);
        let j = (((90.0 - lat) / 180.0 * g.h as f64) as usize).min(g.h - 1);
        g.get(i, j)
    }

    #[test]
    fn real_places_have_real_heights() {
        let g = relief();
        assert_eq!((g.w, g.h), (3600, 1800));
        // Tibetan plateau, Himalaya, Andes, Mariana trench, open ocean,
        // Sahara, Netherlands coast, Antarctic ice sheet.
        assert!(at(&g, 32.0, 88.0) > 4000.0);
        let everest_area = (0..10)
            .flat_map(|a| (0..10).map(move |b| (27.5 + a as f64 * 0.1, 86.5 + b as f64 * 0.1)))
            .map(|(la, lo)| at(&g, la, lo))
            .fold(f32::MIN, f32::max);
        assert!(everest_area > 6000.0, "{everest_area}");
        assert!(at(&g, -16.0, -69.0) > 3500.0, "Altiplano");
        let mariana = (0..20).map(|k| at(&g, 11.35, 142.0 + k as f64 * 0.05)).fold(f32::MAX, f32::min);
        assert!(mariana < -8000.0, "{mariana}");
        // Abyssal plain between Hawaii and Mexico; the Mid-Atlantic Ridge
        // stands well above it.
        assert!(at(&g, 10.0, -140.0) < -4500.0);
        assert!(at(&g, 30.0, -42.0) > -3800.0 && at(&g, 30.0, -42.0) < -2000.0);
        assert!(at(&g, 23.0, 10.0) > 200.0);
        assert!(at(&g, -80.0, 30.0) > 2000.0);
        // 29% of the Earth's surface is land (cells weighted by their area).
        let (mut land, mut total) = (0.0, 0.0);
        for j in 0..g.h {
            let a = ((90.0 - (j as f64 + 0.5) * 0.1).to_radians()).cos();
            total += a * g.w as f64;
            land += a * (0..g.w).filter(|&i| g.get(i, j) >= 0.0).count() as f64;
        }
        let land = land / total;
        assert!((0.27..0.31).contains(&land), "land share {land}");
    }
}
