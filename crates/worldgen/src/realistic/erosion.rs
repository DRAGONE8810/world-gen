//! Rivers, lakes and fluvial erosion on the planet grid.
//!
//! Tectonics lifts mountains; rain and rivers wear them down. This is the
//! "stream power" model used in landscape-evolution research and by
//! Cordonnier et al. (2016, "Large Scale Terrain Generation from Tectonic
//! Uplift and Fluvial Erosion"): a river cuts into its bed at a rate
//! proportional to its slope times its discharge to a power,
//! `∂h/∂t = U − K·Q^m·S`. Big rivers carve deep valleys, ridges between
//! them stay high, and the result is the branching (dendritic) valley
//! pattern of real mountains.
//!
//! Each step:
//!
//! 1. **Priority-Flood** (Barnes et al., 2014) floods the terrain from the
//!    open sea inwards, lowest first. Only the open sea counts as an
//!    outlet, so a basin below sea level far from it (the Caspian, the
//!    Dead Sea) is a closed hollow, not ocean. Every cell learns which neighbour the
//!    water reached it from (its *receiver*, the way downhill) and, if it
//!    sits in a hollow, the level the hollow fills to (a lake). Because
//!    hollows are filled, every drop of rain reaches the sea.
//! 2. **Discharge**: walking the flood order backwards (from the sources
//!    down to the sea) adds up the rain falling on everything upstream.
//! 3. **Erosion**, solved implicitly (Braun & Willett, 2013) from the sea
//!    upwards, so a step can never dig below the river's own outlet.
//! 4. **Sediment**: a hollow is partly filled each step, as rivers drop
//!    their load where the water stands still, so basins become plains
//!    instead of huge lakes.
//! 5. **Uplift**: heights relax towards the tectonic heights, standing in
//!    for the mountains still rising.

use super::grid::NEIGHBOURS8;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Result of flooding a heightmap.
pub struct Drainage {
    /// Cells in flood order: sea and border first, then upstream. Every
    /// cell comes after its receiver.
    pub order: Vec<u32>,
    /// Downhill neighbour, or the cell itself for the sea and the border.
    pub receiver: Vec<u32>,
    /// Height with every hollow filled to its spill level (the lake level).
    pub filled: Vec<f32>,
}

/// Sortable key for an `f64` (total order, including negatives).
#[inline]
fn key(v: f64) -> u64 {
    let b = v.to_bits();
    if b >> 63 == 1 {
        !b
    } else {
        b | (1 << 63)
    }
}

/// The open sea: cells below sea level connected to the world ocean (the
/// border, or the deep ocean floor below 3000 m). Low ground inland (the
/// Caspian, the Dead Sea) is not sea: it fills like any other hollow.
pub fn open_sea(height: &[f32], w: usize, h: usize) -> Vec<bool> {
    let mut sea = vec![false; w * h];
    let mut stack: Vec<usize> = Vec::new();
    for (i, &v) in height.iter().enumerate() {
        let (x, z) = (i % w, i / w);
        let border = x == 0 || z == 0 || x == w - 1 || z == h - 1;
        if (border && v < 0.0) || v < -3000.0 {
            sea[i] = true;
            stack.push(i);
        }
    }
    while let Some(c) = stack.pop() {
        let (x, z) = ((c % w) as isize, (c / w) as isize);
        for &(dx, dz, _) in &NEIGHBOURS8 {
            let (a, b) = (x + dx, z + dz);
            if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                continue;
            }
            let j = a as usize + b as usize * w;
            if !sea[j] && height[j] < 0.0 {
                sea[j] = true;
                stack.push(j);
            }
        }
    }
    sea
}

/// Floods from every sea cell (height < 0) and every border cell.
pub fn priority_flood(height: &[f32], w: usize, h: usize) -> Drainage {
    priority_flood_from(height, w, h, &|i| height[i] < 0.0)
}

/// Floods from the cells where `outlet` holds (the sea) and every border
/// cell. Low ground that is not an outlet, like the Caspian basin, fills
/// up as a hollow.
pub fn priority_flood_from(height: &[f32], w: usize, h: usize, outlet: &dyn Fn(usize) -> bool) -> Drainage {
    let n = w * h;
    let mut filled = vec![0f32; n];
    let mut receiver = vec![u32::MAX; n];
    let mut order = Vec::with_capacity(n);
    let mut done = vec![false; n];
    let mut heap = BinaryHeap::new();
    for i in 0..n {
        let (x, z) = (i % w, i / w);
        let border = x == 0 || z == 0 || x == w - 1 || z == h - 1;
        if border || outlet(i) {
            filled[i] = height[i];
            receiver[i] = i as u32;
            done[i] = true;
            // Only sea cells next to land can pass the flood on; the open
            // ocean (most of the planet) goes straight into the order
            // without costing a trip through the heap.
            let coastal = border
                || NEIGHBOURS8.iter().any(|&(dx, dz, _)| {
                    let (a, b) = (x as isize + dx, z as isize + dz);
                    !outlet(a as usize + b as usize * w)
                });
            if coastal {
                heap.push(Reverse((key(height[i] as f64), i as u32)));
            } else {
                order.push(i as u32);
            }
        }
    }
    while let Some(Reverse((_, c))) = heap.pop() {
        let c = c as usize;
        order.push(c as u32);
        let (x, z) = ((c % w) as isize, (c / w) as isize);
        let fc = filled[c] as f64;
        for &(dx, dz, step) in &NEIGHBOURS8 {
            let (a, b) = (x + dx, z + dz);
            if a < 0 || b < 0 || a >= w as isize || b >= h as isize {
                continue;
            }
            let j = a as usize + b as usize * w;
            if done[j] {
                continue;
            }
            done[j] = true;
            receiver[j] = c as u32;
            // A hollow fills to just above its outlet, so even lakes drain
            // (a tiny slope keeps the flow direction defined).
            let f = (height[j] as f64).max(fc + 1e-3 * step);
            filled[j] = f as f32;
            heap.push(Reverse((key(f), j as u32)));
        }
    }
    Drainage { order, receiver, filled }
}

impl Drainage {
    /// Sum of `local` over each cell and everything upstream of it.
    pub fn accumulate(&self, local: &[f32]) -> Vec<f32> {
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

/// Parameters of the erosion run.
pub struct ErosionParams {
    pub steps: usize,
    /// Cell size, km.
    pub km: f64,
    /// Erodibility: with discharge in km² of rain-equivalent area and
    /// distances in km, a river draining `A` erodes with
    /// `F = k·A^0.5/L` per step.
    pub k: f64,
    /// Share of the gap to the tectonic height recovered per step.
    pub uplift: f64,
    /// Share of a hollow's depth filled with sediment per step.
    pub sediment: f64,
}

/// Erodes `height` (metres) in place. `target` is the tectonic height the
/// uplift pulls towards; `rain` is the relative rainfall per cell (1 =
/// average), so wet mountains are carved faster than dry ones.
pub fn erode(height: &mut [f32], target: &[f32], rain: &[f32], w: usize, h: usize, p: &ErosionParams) {
    let n = w * h;
    let area = p.km * p.km;
    let local: Vec<f32> = rain.iter().map(|&r| (r as f64 * area) as f32).collect();
    let mut eroded = vec![0f64; n];
    for _ in 0..p.steps {
        let sea = open_sea(height, w, h);
        let d = priority_flood_from(height, w, h, &|i| sea[i]);
        let acc = d.accumulate(&local);
        for (i, e) in eroded.iter_mut().enumerate() {
            *e = height[i] as f64;
        }
        for &c in &d.order {
            let c = c as usize;
            let r = d.receiver[c] as usize;
            if r == c || sea[c] {
                continue;
            }
            let hr = eroded[r];
            let hc = height[c] as f64;
            let fc = d.filled[c] as f64;
            if fc > hc + 0.01 {
                // In a hollow: the lake protects its floor, and the silt the
                // rivers bring settles on it, so over time most basins fill
                // up into plains instead of staying lakes.
                eroded[c] = hc + p.sediment * (fc - hc);
                continue;
            }
            if hc <= hr {
                continue;
            }
            let (dx, dz) = ((c % w) as f64 - (r % w) as f64, (c / w) as f64 - (r / w) as f64);
            let l = (dx * dx + dz * dz).sqrt() * p.km;
            let f = p.k * (acc[c] as f64).sqrt() / l;
            eroded[c] = (hc + f * hr) / (1.0 + f);
        }
        for i in 0..n {
            if !sea[i] {
                let e = eroded[i];
                // Uplift towards the tectonic height, never pushing land
                // below the sea or the sea floor up.
                let floor = if height[i] >= 0.0 { 0.0 } else { f64::MIN };
                height[i] = (e + p.uplift * (target[i] as f64 - e).max(0.0)).max(floor) as f32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flood_drains_every_cell_to_the_sea() {
        // A bowl with a notch: the hollow fills to the notch, water leaves
        // through it.
        let (w, h) = (9, 9);
        let mut height = vec![10f32; w * h];
        for z in 2..7 {
            for x in 2..7 {
                height[x + z * w] = 2.0;
            }
        }
        height[w / 2] = -5.0; // sea at the top edge
        height[w / 2 + w] = 4.0; // the notch
        let d = priority_flood(&height, w, h);
        assert_eq!(d.order.len(), w * h);
        for i in 0..w * h {
            // Following receivers always ends at a sea or border cell.
            let mut c = i;
            for _ in 0..w * h {
                if d.receiver[c] as usize == c {
                    break;
                }
                assert!(d.filled[d.receiver[c] as usize] <= d.filled[c]);
                c = d.receiver[c] as usize;
            }
            assert_eq!(d.receiver[c] as usize, c);
        }
        // The bowl's floor is flooded to just above the notch.
        let floor = d.filled[4 + 4 * w];
        assert!(floor > 4.0 && floor < 10.0, "{floor}");
        // Discharge of the notch includes the whole bowl.
        let acc = d.accumulate(&vec![1.0; w * h]);
        assert!(acc[w / 2 + w] >= 25.0);
    }

    #[test]
    fn erosion_cuts_valleys_but_keeps_land() {
        let (w, h) = (40, 40);
        let mut height: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, z) = ((i % w) as f32, (i / w) as f32);
                if z < 2.0 {
                    -100.0
                } else {
                    z * 100.0 + ((x * 0.7).sin() * 50.0)
                }
            })
            .collect();
        let target = height.clone();
        let rain = vec![1f32; w * h];
        let before: f64 = height.iter().map(|&v| v as f64).sum();
        erode(
            &mut height,
            &target,
            &rain,
            w,
            h,
            &ErosionParams { steps: 5, km: 10.0, k: 0.02, uplift: 0.1, sediment: 0.25 },
        );
        let after: f64 = height.iter().map(|&v| v as f64).sum();
        assert!(after < before);
        assert!(height.iter().zip(&target).all(|(&a, &t)| a <= t + 1e-3));
        assert!(height[(w / 2) + (h - 3) * w] > 0.0);
    }
}
