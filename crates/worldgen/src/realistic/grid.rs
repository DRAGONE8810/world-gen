//! A 2D raster over the whole planet, the canvas of the realistic
//! simulation, plus the few image operations it needs.
//!
//! Values are stored as `f32` to keep a 2048 × 2048 planet small; all
//! arithmetic that decides anything is done in `f64`. Only `+ − × ÷` and
//! `sqrt` are used (exactly rounded by IEEE 754 everywhere), so the same
//! seed gives bit-identical grids on every platform.

/// Row-major grid, `x` fastest. Cell (i, j) covers
/// `[i, i+1) × [j, j+1)` in grid units; its value sits at the centre.
#[derive(Clone, Debug)]
pub struct Grid<T> {
    pub w: usize,
    pub h: usize,
    pub data: Vec<T>,
}

impl<T: Copy> Grid<T> {
    pub fn new(w: usize, h: usize, v: T) -> Self {
        Self { w, h, data: vec![v; w * h] }
    }
    #[inline]
    pub fn idx(&self, i: usize, j: usize) -> usize {
        i + j * self.w
    }
    #[inline]
    pub fn get(&self, i: usize, j: usize) -> T {
        self.data[i + j * self.w]
    }
    #[inline]
    pub fn set(&mut self, i: usize, j: usize, v: T) {
        let w = self.w;
        self.data[i + j * w] = v;
    }
    /// Clamped lookup with signed coordinates.
    #[inline]
    pub fn at(&self, i: isize, j: isize) -> T {
        let i = i.clamp(0, self.w as isize - 1) as usize;
        let j = j.clamp(0, self.h as isize - 1) as usize;
        self.data[i + j * self.w]
    }
    pub fn map<U: Copy>(&self, f: impl Fn(T) -> U) -> Grid<U> {
        Grid { w: self.w, h: self.h, data: self.data.iter().map(|&v| f(v)).collect() }
    }
}

/// The 8 neighbours: (dx, dy, distance in cells).
pub const NEIGHBOURS8: [(isize, isize, f64); 8] = [
    (1, 0, 1.0),
    (-1, 0, 1.0),
    (0, 1, 1.0),
    (0, -1, 1.0),
    (1, 1, std::f64::consts::SQRT_2),
    (-1, 1, std::f64::consts::SQRT_2),
    (1, -1, std::f64::consts::SQRT_2),
    (-1, -1, std::f64::consts::SQRT_2),
];

impl Grid<f32> {
    /// Bilinear sample at grid coordinates (cell centres at `i + 0.5`).
    #[inline]
    pub fn bilinear(&self, u: f64, v: f64) -> f64 {
        let (u, v) = (u - 0.5, v - 0.5);
        let (fu, fv) = (u.floor(), v.floor());
        let (i, j) = (fu as isize, fv as isize);
        let (tx, ty) = (u - fu, v - fv);
        let a = self.at(i, j) as f64;
        let b = self.at(i + 1, j) as f64;
        let c = self.at(i, j + 1) as f64;
        let d = self.at(i + 1, j + 1) as f64;
        let top = a + (b - a) * tx;
        let bottom = c + (d - c) * tx;
        top + (bottom - top) * ty
    }

    /// Bicubic (Catmull–Rom) sample: smooth slopes across cell edges, so
    /// the coarse grid never shows as facets in the terrain.
    #[inline]
    pub fn bicubic(&self, u: f64, v: f64) -> f64 {
        let (u, v) = (u - 0.5, v - 0.5);
        let (fu, fv) = (u.floor(), v.floor());
        let (i, j) = (fu as isize, fv as isize);
        let wx = catmull_rom_weights(u - fu);
        let wy = catmull_rom_weights(v - fv);
        let mut sum = 0.0;
        for (dy, wyv) in wy.iter().enumerate() {
            let mut row = 0.0;
            for (dx, wxv) in wx.iter().enumerate() {
                row += wxv * self.at(i - 1 + dx as isize, j - 1 + dy as isize) as f64;
            }
            sum += wyv * row;
        }
        sum
    }

    /// Box blur of radius `r` cells applied `passes` times (three passes
    /// approximate a Gaussian). Edges are clamped.
    pub fn blur(&self, r: usize, passes: usize) -> Grid<f32> {
        let mut g = self.clone();
        if r == 0 {
            return g;
        }
        let mut tmp = vec![0f32; g.data.len()];
        let inv = 1.0 / (2 * r + 1) as f64;
        for _ in 0..passes {
            // Horizontal, running sum per row.
            for j in 0..g.h {
                let row = &g.data[j * g.w..(j + 1) * g.w];
                let at = |i: isize| row[i.clamp(0, g.w as isize - 1) as usize] as f64;
                let mut s: f64 = (-(r as isize)..=r as isize).map(at).sum();
                for i in 0..g.w {
                    tmp[i + j * g.w] = (s * inv) as f32;
                    s += at(i as isize + r as isize + 1) - at(i as isize - r as isize);
                }
            }
            // Vertical.
            for i in 0..g.w {
                let at = |j: isize| tmp[i + j.clamp(0, g.h as isize - 1) as usize * g.w] as f64;
                let mut s: f64 = (-(r as isize)..=r as isize).map(at).sum();
                for j in 0..g.h {
                    g.data[i + j * g.w] = (s * inv) as f32;
                    s += at(j as isize + r as isize + 1) - at(j as isize - r as isize);
                }
            }
        }
        g
    }

    /// The value below which a fraction `q` of the cells lie (exact
    /// order statistic, so it is deterministic).
    pub fn quantile(&self, q: f64) -> f32 {
        let mut v = self.data.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let k = ((v.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
        v[k]
    }

    /// Bilinear resampling to another resolution (same extent).
    pub fn resample(&self, w: usize, h: usize) -> Grid<f32> {
        let mut out = Grid::new(w, h, 0f32);
        let (sx, sy) = (self.w as f64 / w as f64, self.h as f64 / h as f64);
        for j in 0..h {
            for i in 0..w {
                // Average over the source footprint when shrinking, so
                // narrow peaks and trenches are not missed or aliased.
                let (u0, v0) = (i as f64 * sx, j as f64 * sy);
                let n = sx.max(1.0).ceil() as usize;
                let m = sy.max(1.0).ceil() as usize;
                let mut s = 0.0;
                for b in 0..m {
                    for a in 0..n {
                        s +=
                            self.bilinear(u0 + (a as f64 + 0.5) * sx / n as f64, v0 + (b as f64 + 0.5) * sy / m as f64);
                    }
                }
                out.set(i, j, (s / (n * m) as f64) as f32);
            }
        }
        out
    }
}

#[inline]
fn catmull_rom_weights(t: f64) -> [f64; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [0.5 * (-t3 + 2.0 * t2 - t), 0.5 * (3.0 * t3 - 5.0 * t2 + 2.0), 0.5 * (-3.0 * t3 + 4.0 * t2 + t), 0.5 * (t3 - t2)]
}

/// Exact Euclidean distance (in cells) from every cell to the nearest
/// seed cell, and that seed's index. Seeds are spread with Dijkstra's
/// algorithm, but each cell stores the *position* of its seed and its
/// distance is measured straight to it, so the result has no 8-direction
/// "octagon" bias. `passable(from, to)` can stop the spread (e.g. at a
/// plate boundary); `max` stops it at a distance.
pub fn distance_field(
    w: usize,
    h: usize,
    seeds: &[usize],
    max: f64,
    passable: impl Fn(usize, usize) -> bool,
) -> (Vec<f32>, Vec<u32>) {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut dist = vec![f64::INFINITY; w * h];
    let mut src = vec![u32::MAX; w * h];
    // Heap key: distance as ordered bits (non-negative f64 orders like its
    // bit pattern), then cell index for deterministic ties.
    let mut heap = BinaryHeap::new();
    for &s in seeds {
        if dist[s] > 0.0 {
            dist[s] = 0.0;
            src[s] = s as u32;
            heap.push(Reverse((0u64, s as u32)));
        }
    }
    while let Some(Reverse((dbits, c))) = heap.pop() {
        let c = c as usize;
        let d = f64::from_bits(dbits);
        if d > dist[c] {
            continue;
        }
        let s = src[c] as usize;
        let (sx, sy) = ((s % w) as f64, (s / w) as f64);
        let (cx, cy) = ((c % w) as isize, (c / w) as isize);
        for &(dx, dy, _) in &NEIGHBOURS8 {
            let (nx, ny) = (cx + dx, cy + dy);
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                continue;
            }
            let n = nx as usize + ny as usize * w;
            if !passable(c, n) {
                continue;
            }
            let (ex, ey) = (nx as f64 - sx, ny as f64 - sy);
            let nd = (ex * ex + ey * ey).sqrt();
            if nd < dist[n] - 1e-9 && nd <= max {
                dist[n] = nd;
                src[n] = s as u32;
                heap.push(Reverse((nd.to_bits(), n as u32)));
            }
        }
    }
    (dist.into_iter().map(|d| d as f32).collect(), src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_hits_cell_centres() {
        let mut g = Grid::new(4, 3, 0f32);
        for j in 0..3 {
            for i in 0..4 {
                g.set(i, j, (i * 10 + j) as f32);
            }
        }
        for j in 0..3 {
            for i in 0..4 {
                let (u, v) = (i as f64 + 0.5, j as f64 + 0.5);
                assert!((g.bilinear(u, v) - g.get(i, j) as f64).abs() < 1e-9);
                assert!((g.bicubic(u, v) - g.get(i, j) as f64).abs() < 1e-9);
            }
        }
        // Linear data is reproduced exactly between centres too.
        assert!((g.bilinear(1.0, 1.5) - 5.0 - 1.0).abs() < 1e-9);
        assert!((g.bicubic(2.0, 1.5) - 15.0 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blur_keeps_the_mean_of_a_constant() {
        let g = Grid::new(16, 9, 3.5f32).blur(3, 3);
        assert!(g.data.iter().all(|&v| (v - 3.5).abs() < 1e-5));
    }

    #[test]
    fn distance_field_is_euclidean() {
        let (w, h) = (64, 64);
        let seeds = [10 + 20 * w];
        let (d, s) = distance_field(w, h, &seeds, f64::INFINITY, |_, _| true);
        for j in 0..h {
            for i in 0..w {
                let e = ((i as f64 - 10.0).powi(2) + (j as f64 - 20.0).powi(2)).sqrt();
                assert!((d[i + j * w] as f64 - e).abs() < 1e-4, "({i},{j}) {} vs {e}", d[i + j * w]);
                assert_eq!(s[i + j * w] as usize, seeds[0]);
            }
        }
    }
}
