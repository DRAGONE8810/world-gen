//! Regions: landscapes that exist in only a few places on a world.
//!
//! Some real landscapes are not a climate but a *place*: the sandstone
//! pillar forests of Zhangjiajie and the tower karst of Guilin, the baobab
//! savannas of Madagascar, the coast redwoods of California, the bamboo
//! forests of China and Japan, the cherry groves of Japan, the red sand of
//! the Australian outback, the salt pans of the Altiplano, the lavender
//! hills of Provence. The climate allows them in many places, but each
//! grows in one region.
//!
//! The world is therefore divided into large **regions** (a few km in a
//! classic world, ~500 km on a realistic planet): a jittered grid of sites,
//! each owning the area closest to it (a Voronoi diagram), with the lookup
//! position domain-warped so borders curve instead of running straight.
//! Most regions are ordinary. A few, chosen by a hash of the site, carry a
//! **flavour**, and inside them the biome code turns the matching host
//! biome into the regional one (a temperate forest into a cherry grove,
//! a savanna into a baobab savanna, …). A flavour without a fitting climate
//! in its region simply doesn't show, which makes them rarer still.
//!
//! Every column also gets the **strength** of its region's flavour: 1
//! inside, falling to 0 at the border with a region of another flavour
//! (the distance to the Voronoi edge between the two sites). The biome code
//! dithers against it, so a regional landscape thins out at its edges
//! instead of stopping on a line.
//!
//! Pillar regions also change the terrain (see [`Regions::pillar`]).

use crate::noise::{OctaveConfig, OctaveNoise};
use crate::rng::{derive_seed, hash_position};
use crate::spline::smoothstep;

/// What makes a region special.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum Flavour {
    #[default]
    None,
    /// Cherry groves (Japan).
    Sakura,
    /// Sandstone and limestone pillar forests (Zhangjiajie, Guilin).
    Pillars,
    /// Baobab savanna (Madagascar, East Africa).
    Baobab,
    /// Giant redwood and sequoia forest (California).
    Redwood,
    /// Bamboo forest (China, Japan).
    Bamboo,
    /// Red sand desert (the Australian outback).
    Outback,
    /// Salt pans (Salar de Uyuni, Etosha).
    Salar,
    /// Lavender hills (Provence).
    Lavender,
}

impl Flavour {
    /// Every flavour with its share of regions, per thousand. About one
    /// region in three has a flavour.
    const SHARES: [(Flavour, u64); 8] = [
        (Flavour::Sakura, 45),
        (Flavour::Pillars, 40),
        (Flavour::Baobab, 45),
        (Flavour::Redwood, 45),
        (Flavour::Bamboo, 45),
        (Flavour::Outback, 45),
        (Flavour::Salar, 30),
        (Flavour::Lavender, 40),
    ];

    fn from_hash(h: u64) -> Flavour {
        let mut roll = h % 1000;
        for (f, share) in Self::SHARES {
            if roll < share {
                return f;
            }
            roll -= share;
        }
        Flavour::None
    }
}

/// The region of a column.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Region {
    pub flavour: Flavour,
    /// 1 well inside the region, 0 at its border with another flavour.
    pub strength: f32,
}

impl Region {
    pub const NONE: Region = Region { flavour: Flavour::None, strength: 0.0 };
}

pub struct Regions {
    seed: u64,
    /// Region size in blocks (distance between sites).
    cell: f64,
    warp_x: OctaveNoise,
    warp_z: OctaveNoise,
    pillar_seed: u64,
}

/// Width of a region's fading edge, in cells.
const EDGE: f64 = 0.3;

impl Regions {
    /// `cell` is the region size in blocks.
    pub fn new(seed: u64, cell: f64) -> Self {
        // Warp in cell units: wavelengths of 2 cells and 1 cell.
        let warp = |salt| OctaveNoise::new(derive_seed(seed, salt), &OctaveConfig::new(-1, &[1.0, 0.5]));
        Self {
            seed: derive_seed(seed, "regions/sites"),
            cell,
            warp_x: warp("regions/warp_x"),
            warp_z: warp("regions/warp_z"),
            pillar_seed: derive_seed(seed, "regions/pillars"),
        }
    }

    pub fn cell(&self) -> f64 {
        self.cell
    }

    /// Site of grid cell (i, j), in cell units, and its flavour.
    #[inline]
    fn site(&self, i: i64, j: i64) -> (f64, f64, Flavour) {
        // Region grids stay far inside i32 (≤ 60,000 km / 1 km).
        let h = hash_position(self.seed, i as i32, 7, j as i32);
        let fx = 0.15 + 0.7 * ((h & 0xFFFF) as f64 / 65536.0);
        let fz = 0.15 + 0.7 * (((h >> 16) & 0xFFFF) as f64 / 65536.0);
        (i as f64 + fx, j as f64 + fz, Flavour::from_hash(h >> 32))
    }

    pub fn at(&self, x: i32, z: i32) -> Region {
        let (u, v) = (x as f64 / self.cell, z as f64 / self.cell);
        let (u, v) = (u + 0.45 * self.warp_x.sample_2d(u, v), v + 0.45 * self.warp_z.sample_2d(u, v));
        let (i0, j0) = (u.floor() as i64, v.floor() as i64);
        let mut sites = [(0.0, 0.0, Flavour::None, 0.0); 9];
        let mut n = 0;
        for j in j0 - 1..=j0 + 1 {
            for i in i0 - 1..=i0 + 1 {
                let (sx, sz, f) = self.site(i, j);
                sites[n] = (sx, sz, f, (sx - u).powi(2) + (sz - v).powi(2));
                n += 1;
            }
        }
        let near = *sites.iter().min_by(|a, b| a.3.total_cmp(&b.3)).expect("nine sites");
        if near.2 == Flavour::None {
            return Region::NONE;
        }
        // Distance to the nearest Voronoi edge with a site of another
        // flavour: for sites A (nearest) and B, (|PB|² − |PA|²) / 2|AB|.
        let mut edge = f64::INFINITY;
        for s in &sites {
            if s.2 != near.2 {
                let ab = ((s.0 - near.0).powi(2) + (s.1 - near.1).powi(2)).sqrt().max(1e-9);
                edge = edge.min((s.3 - near.3) / (2.0 * ab));
            }
        }
        Region { flavour: near.2, strength: smoothstep(0.0, EDGE, edge) as f32 }
    }

    /// Pillar terrain: how much a pillar raises this column (0..1 of its
    /// full height, steep-sided with a gently rounded top) and the
    /// pillar's hash, for a pillar forest where pillars stand with
    /// probability `density`. `size` is the pillar spacing in blocks.
    ///
    /// Pillars stand on their own jittered grid, one candidate per cell,
    /// with a radius between a fifth and a third of the spacing, so
    /// neighbours never merge and the gaps between them are gorges.
    pub fn pillar(&self, x: i32, z: i32, size: f64, density: f64) -> Option<(f64, u64)> {
        if density <= 0.0 {
            return None;
        }
        let (u, v) = (x as f64 / size, z as f64 / size);
        let (i0, j0) = (u.floor() as i32, v.floor() as i32);
        let mut best: Option<(f64, u64)> = None;
        for j in j0 - 1..=j0 + 1 {
            for i in i0 - 1..=i0 + 1 {
                let h = hash_position(self.pillar_seed, i, 0, j);
                if ((h >> 40) & 0xFFFF) as f64 / 65536.0 >= density {
                    continue;
                }
                let cx = i as f64 + 0.3 + 0.4 * ((h & 0xFFFF) as f64 / 65536.0);
                let cz = j as f64 + 0.3 + 0.4 * (((h >> 16) & 0xFFFF) as f64 / 65536.0);
                let r = 0.2 + 0.13 * (((h >> 32) & 0xFF) as f64 / 255.0);
                let d = ((u - cx).powi(2) + (v - cz).powi(2)).sqrt();
                if d < r {
                    // Vertical walls for the outer 15 %, a slight dome on top.
                    let p = smoothstep(r, r * 0.85, d) * (1.0 - 0.15 * (d / r).powi(2));
                    if best.is_none_or(|(b, _)| p > b) {
                        best = Some((p, h));
                    }
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_large_patches_with_soft_edges() {
        let r = Regions::new(42, 1000.0);
        let mut flavoured = 0;
        let mut kinds = std::collections::HashSet::new();
        let mut changes = 0;
        let mut prev = r.at(-100_000, 0);
        for x in (-100_000..100_000).step_by(10) {
            let c = r.at(x, 0);
            flavoured += (c.flavour != Flavour::None) as i32;
            kinds.insert(c.flavour);
            if c.flavour != prev.flavour {
                changes += 1;
                // At a change of flavour both sides have faded out.
                assert!(c.strength < 0.2 || c.flavour == Flavour::None, "hard edge at {x}: {c:?} after {prev:?}");
                assert!(prev.strength < 0.2 || prev.flavour == Flavour::None);
            }
            prev = c;
        }
        let share = flavoured as f64 / 20_000.0;
        assert!((0.15..0.55).contains(&share), "flavoured share {share}");
        assert!(kinds.len() >= 6, "{kinds:?}");
        // Regions ~1000 blocks wide: about 200 changes over 200 km, not
        // thousands.
        assert!((40..400).contains(&changes), "{changes} flavour changes");
    }

    #[test]
    fn pillars_have_steep_walls_and_gaps() {
        let r = Regions::new(7, 1000.0);
        let (mut inside, mut steep) = (0, 0);
        for z in 0..400 {
            for x in 0..400 {
                if let Some((p, _)) = r.pillar(x, z, 20.0, 0.6) {
                    inside += 1;
                    if p < 0.8 {
                        steep += 1;
                    }
                }
            }
        }
        let share = inside as f64 / 160_000.0;
        assert!((0.05..0.4).contains(&share), "pillar share {share}");
        // The wall band is thin: most of a pillar is its flat top.
        assert!((steep as f64) < 0.45 * inside as f64, "{steep} of {inside}");
        assert!(r.pillar(5, 5, 20.0, 0.0).is_none());
    }
}
