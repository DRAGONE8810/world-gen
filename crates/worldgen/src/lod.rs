//! Level-of-detail sampling: what the world looks like from far away.
//!
//! Far terrain is drawn the way Distant Horizons and Voxy do it: instead of
//! generating every block, the renderer asks for a grid of *LOD cells*, each
//! `s × s` blocks wide (`s` = 1, 2, 4, … 256), and draws every cell as a
//! single column: a flat top at the height of whatever you would see from
//! the sky (terrain, tree canopy, water or ice) and walls down to its
//! neighbours. A 16 × 16 grid of 32-block cells covers 512 × 512 blocks for
//! the cost of 256 column samples instead of 1,024 chunks.
//!
//! The ground of each cell comes from [`WorldGenerator::sample_column`] at the
//! cell's centre, so it is the real surface there, not an approximation.
//! Trees are added in two ways:
//!
//! - **Small cells** (`s ≤ EXACT_TREE_CELL`): every tree near the grid is
//!   resolved and built with the same code as chunk generation, and its
//!   blocks are rasterised into the cells they fall in (the highest block
//!   wins). With `s = 1` the result is exactly the top block of every column
//!   of the real chunk; a test checks that.
//! - **Large cells**: a cell holds many tree-grid cells, so it looks at the
//!   one tree candidate in the middle.
//!
//! Either way a cell shows canopy only with the probability that the canopy
//! covers it: a small cell counts the columns its trees cover, a large cell
//! uses the crown area of its tree over the area of a tree-grid cell, and
//! a hash of the cell decides. Otherwise one leaf anywhere in an 8-block
//! cell would paint all of it green, and a forest would look much darker
//! (and its edge would jump) where the LOD switches between cell sizes.
//! The share of canopy cells then matches what the real blocks show at
//! every cell size.

use crate::biome::Biome;
use crate::block::{blocks, BlockId};
use crate::generator::{ColumnSample, WorldGenerator};
use crate::rng::hash_position;
use crate::tint::Tint;
use crate::trees::{self, TREE_CELL, TREE_REACH};

/// Cells up to this size get exact tree rasterisation.
pub const EXACT_TREE_CELL: i32 = 8;

/// One LOD cell as seen from above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LodColumn {
    /// Outside the world border: draw nothing.
    pub inside: bool,
    /// Y of the highest visible block: terrain, canopy, or the water/ice
    /// surface at sea level.
    pub top_y: i32,
    /// Block at `top_y`, for its colour.
    pub block: BlockId,
    /// What the cell's ground shows in a cliff below its top: the rock
    /// under the surface (the water itself over water).
    pub side: BlockId,
    /// The terrain's top block (under any tree canopy), for the walls
    /// below a canopy.
    pub ground: BlockId,
    /// Biome of the ground.
    pub biome: Biome,
    /// Grass and leaf colour of the ground.
    pub tint: Tint,
    /// Y of the terrain surface (below water or trees).
    pub ground_y: i32,
}

impl LodColumn {
    fn from_sample(s: &ColumnSample) -> Self {
        let (top_y, block) = if !s.inside {
            (s.surface_y, blocks::AIR)
        } else if s.underwater {
            (s.shape.water, if s.ice { blocks::ICE } else { blocks::WATER })
        } else {
            (s.surface_y, s.top_block)
        };
        let side = if s.underwater { block } else { s.side_block };
        Self {
            inside: s.inside,
            top_y,
            block,
            side,
            ground: block,
            biome: s.biome,
            tint: s.tint,
            ground_y: s.surface_y,
        }
    }

    /// Applies one tree block the way chunk generation does: above the top
    /// is air, so anything goes; at the top, logs grow through water and
    /// leaves; leaves never replace anything.
    fn add_tree_block(&mut self, y: i32, b: BlockId, is_log: bool) {
        if y > self.top_y || (y == self.top_y && is_log && (self.block == blocks::WATER || self.block.is_leaves())) {
            self.top_y = y;
            self.block = b;
        }
    }
}

impl WorldGenerator {
    /// Samples an `n × n` grid of LOD cells of `cell` blocks each, whose
    /// first cell starts at block (`x0`, `z0`). Row-major, x fastest.
    /// `cell` must be a power of two.
    pub fn lod_grid(&self, x0: i32, z0: i32, cell: i32, n: usize) -> Vec<LodColumn> {
        debug_assert!(cell > 0 && (cell & (cell - 1)) == 0);
        let half = cell / 2;
        let samples: Vec<ColumnSample> = (0..n * n)
            .map(|i| {
                let (ix, iz) = ((i % n) as i32, (i / n) as i32);
                self.sample_column_for_cell(x0 + ix * cell + half, z0 + iz * cell + half, cell)
            })
            .collect();
        let mut out: Vec<LodColumn> = samples.iter().map(LodColumn::from_sample).collect();
        if !self.settings().features {
            return out;
        }
        if cell <= EXACT_TREE_CELL {
            self.rasterise_trees(x0, z0, cell, n, &samples, &mut out);
        } else {
            // The one candidate in the tree-grid cell under each LOD cell's centre.
            for i in 0..n * n {
                let s = &samples[i];
                if !s.inside {
                    continue;
                }
                let (ix, iz) = ((i % n) as i32, (i / n) as i32);
                let (x, z) = (x0 + ix * cell + half, z0 + iz * cell + half);
                let (tx, tz, hash) =
                    trees::cell_candidate(self.tree_seed(), x.div_euclid(TREE_CELL), z.div_euclid(TREE_CELL));
                if !self.settings().size.contains_column(tx, tz) {
                    continue;
                }
                if let Some(t) = self.resolve_tree(tx, tz, hash, s.surface_y, s.top_block, &s.shape, cell >= 32) {
                    let mut c = out[i];
                    // Crown footprint: columns within TREE_REACH of the trunk.
                    let mut footprint = [0u64; 2];
                    trees::build(&t, &mut |wx, y, wz, b, is_log| {
                        let bit = (wx - tx + TREE_REACH) + (wz - tz + TREE_REACH) * (2 * TREE_REACH + 1);
                        footprint[bit as usize / 64] |= 1 << (bit % 64);
                        if y <= self.max_y() {
                            c.add_tree_block(y, b, is_log);
                        }
                    });
                    let area = (footprint[0].count_ones() + footprint[1].count_ones()) as f64;
                    if area / (TREE_CELL * TREE_CELL) as f64 > self.cover_roll(x, z, cell) {
                        out[i] = c;
                    }
                }
            }
        }
        out
    }

    /// Builds every tree that can reach the grid and keeps the highest
    /// block per cell. Trees are visited in the same global cell order as
    /// chunk generation so equal-height overlaps resolve the same way.
    fn rasterise_trees(&self, x0: i32, z0: i32, cell: i32, n: usize, samples: &[ColumnSample], out: &mut [LodColumn]) {
        let span = cell * n as i32;
        // Columns of each cell covered by a tree (cell² ≤ 64 bits).
        let mut cover = vec![0u64; if cell > 1 { n * n } else { 0 }];
        let (x_lo, x_hi) = (x0 - TREE_REACH, x0 + span - 1 + TREE_REACH);
        let (z_lo, z_hi) = (z0 - TREE_REACH, z0 + span - 1 + TREE_REACH);
        let size = self.settings().size;
        for cz in z_lo.div_euclid(TREE_CELL)..=z_hi.div_euclid(TREE_CELL) {
            for cx in x_lo.div_euclid(TREE_CELL)..=x_hi.div_euclid(TREE_CELL) {
                let (x, z, hash) = trees::cell_candidate(self.tree_seed(), cx, cz);
                if x < x_lo || x > x_hi || z < z_lo || z > z_hi || !size.contains_column(x, z) {
                    continue;
                }
                // Ground under the trunk: the cell sample when the trunk is
                // inside the grid (exact for 1-block cells), otherwise a
                // fresh sample.
                let (gx, gz) = ((x - x0).div_euclid(cell), (z - z0).div_euclid(cell));
                let fresh;
                let s = if (0..n as i32).contains(&gx) && (0..n as i32).contains(&gz) {
                    &samples[gx as usize + gz as usize * n]
                } else {
                    fresh = self.sample_column(x, z);
                    &fresh
                };
                let Some(t) = self.resolve_tree(x, z, hash, s.surface_y, s.top_block, &s.shape, false) else {
                    continue;
                };
                trees::build(&t, &mut |wx, wy, wz, b, is_log| {
                    if wy > self.max_y() || !size.contains_column(wx, wz) {
                        return;
                    }
                    let (ix, iz) = ((wx - x0).div_euclid(cell), (wz - z0).div_euclid(cell));
                    if (0..n as i32).contains(&ix) && (0..n as i32).contains(&iz) {
                        let i = ix as usize + iz as usize * n;
                        // A crown hanging from a pillar or cliff top over a
                        // cell whose ground is far below would make a
                        // column of leaves down to that ground: leave it
                        // out (1-block cells stay exact).
                        if cell > 1 && wy > out[i].ground_y + trees::TREE_HEADROOM {
                            return;
                        }
                        out[i].add_tree_block(wy, b, is_log);
                        if cell > 1 {
                            let (bx, bz) = (wx - x0 - ix * cell, wz - z0 - iz * cell);
                            cover[i] |= 1 << (bx + bz * cell);
                        }
                    }
                });
            }
        }
        // A cell shows its trees with the share of its columns they cover.
        // (1-block cells are covered entirely or not at all: exact.)
        let area = (cell * cell) as f64;
        for (i, mask) in cover.iter().enumerate() {
            let (ix, iz) = ((i % n) as i32, (i / n) as i32);
            let (x, z) = (x0 + ix * cell, z0 + iz * cell);
            if *mask != 0 && mask.count_ones() as f64 / area <= self.cover_roll(x, z, cell) {
                out[i] = LodColumn::from_sample(&samples[i]);
            }
        }
    }

    /// Uniform 0..1 per LOD cell (by its position and size) that decides
    /// whether the cell shows the canopy covering part of it.
    fn cover_roll(&self, x: i32, z: i32, cell: i32) -> f64 {
        (hash_position(self.tree_seed() ^ 0x10D_C0FE, x.div_euclid(cell), cell, z.div_euclid(cell)) >> 11) as f64
            / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkPos;
    use crate::settings::{Generator, Planet, SimQuality, WorldHeight, WorldSize, WorldType, SEA_LEVEL};
    use crate::WorldSettings;

    /// Highest non-air block of a real chunk column.
    fn chunk_top(c: &crate::Chunk, lx: i32, lz: i32) -> (i32, BlockId) {
        for y in (c.min_y()..=c.max_y()).rev() {
            let b = c.get(lx as usize, y, lz as usize);
            if !b.is_air() {
                return (y, b);
            }
        }
        (c.min_y() - 1, blocks::AIR)
    }

    #[test]
    fn one_block_cells_match_generated_chunks_exactly() {
        for (seed, world_type, height) in [
            (42, WorldType::Normal, 384),
            (7, WorldType::Normal, 384),
            (3, WorldType::Amplified, 384),
            (42, WorldType::Normal, 2000),
        ] {
            let g = WorldGenerator::new(WorldSettings {
                seed,
                world_type,
                height: WorldHeight::from_blocks(height),
                size: WorldSize::full(),
                ..Default::default()
            });
            let (sx, _, sz) = g.find_spawn();
            let mut trees_seen = 0;
            for (dx, dz) in [(0, 0), (5, -3), (-7, 9), (40, 40)] {
                let pos = ChunkPos::from_block(sx + dx * 16, sz + dz * 16);
                let chunk = g.generate_chunk(pos);
                let lod = g.lod_grid(pos.min_block_x(), pos.min_block_z(), 1, 16);
                for lz in 0..16 {
                    for lx in 0..16 {
                        let l = lod[(lx + lz * 16) as usize];
                        let (y, b) = chunk_top(&chunk, lx, lz);
                        assert_eq!((l.top_y, l.block), (y, b), "seed {seed} {pos:?} column {lx},{lz}");
                        trees_seen += b.is_tree_part() as i32;
                    }
                }
            }
            assert!(seed != 42 || height != 384 || trees_seen > 0, "test area has no trees");
        }
    }

    /// The same for realistic worlds, including columns under rivers and
    /// lakes above sea level.
    #[test]
    fn one_block_cells_match_realistic_chunks() {
        for planet in [Planet::Continents, Planet::Earth] {
            let g = WorldGenerator::new(WorldSettings {
                seed: 12,
                generator: Generator::Realistic,
                planet,
                quality: SimQuality::Fast,
                height: WorldHeight::from_blocks(2048),
                size: WorldSize::from_km(4000.0),
                ..Default::default()
            });
            let (sx, _, sz) = g.find_spawn();
            let mut spots: Vec<(i32, i32)> =
                [(0, 0), (5, -3), (-7, 9), (40, 40)].iter().map(|&(dx, dz)| (sx + dx * 16, sz + dz * 16)).collect();
            // A river or lake above the sea, found on a coarse scan.
            let half = g.settings().size.half_width().min(g.settings().size.half_depth()) - 64;
            'scan: for j in 0..300 {
                for i in 0..300 {
                    let (x, z) = (-half + 2 * half / 300 * i, -half + 2 * half / 300 * j);
                    let s = g.sample_column(x, z);
                    if s.underwater && s.shape.water > SEA_LEVEL + 10 {
                        spots.push((x, z));
                        break 'scan;
                    }
                }
            }
            assert!(spots.len() == 5, "{planet:?}: no river or lake found");
            for (x, z) in spots {
                let pos = ChunkPos::from_block(x, z);
                let chunk = g.generate_chunk(pos);
                let lod = g.lod_grid(pos.min_block_x(), pos.min_block_z(), 1, 16);
                for lz in 0..16 {
                    for lx in 0..16 {
                        let l = lod[(lx + lz * 16) as usize];
                        let (y, b) = chunk_top(&chunk, lx, lz);
                        assert_eq!((l.top_y, l.block), (y, b), "{planet:?} {pos:?} column {lx},{lz}");
                    }
                }
            }
        }
    }

    fn canopy_share(grid: &[LodColumn]) -> f64 {
        grid.iter().filter(|c| c.block.is_tree_part()).count() as f64 / grid.len() as f64
    }

    #[test]
    fn coarse_cells_follow_the_terrain() {
        let g = WorldGenerator::new(WorldSettings { seed: 42, size: WorldSize::full(), ..Default::default() });
        // Every cell's ground is the real surface at its centre, and the
        // canopy is at most a tree above it.
        let (x0, z0, cell, n) = (-512, -512, 16, 64);
        let grid = g.lod_grid(x0, z0, cell, n);
        for (i, c) in grid.iter().enumerate() {
            let (ix, iz) = ((i % n) as i32, (i / n) as i32);
            let s = g.sample_column(x0 + ix * cell + cell / 2, z0 + iz * cell + cell / 2);
            assert_eq!(c.ground_y, s.surface_y);
            assert!(c.top_y >= c.ground_y.max(if s.underwater { SEA_LEVEL } else { i32::MIN }));
            assert!(c.top_y <= c.ground_y.max(SEA_LEVEL) + trees::TREE_HEADROOM);
        }
    }

    #[test]
    fn coarse_canopy_matches_forest_density() {
        // Seed 42 has a forest around (96, −672) and a desert around
        // (−1984, −2208) (see the find_biome example). From far away the
        // share of cells showing canopy should match what the real blocks
        // show, within the noise of a small sample.
        let g = WorldGenerator::new(WorldSettings { seed: 42, size: WorldSize::full(), ..Default::default() });
        for (x, z) in [(96, -672), (-1984, -2208)] {
            let exact = canopy_share(&g.lod_grid(x - 48, z - 48, 1, 96));
            let coarse = canopy_share(&g.lod_grid(x - 256, z - 256, 16, 32));
            // Small cells rasterise real trees but must not paint a whole
            // cell green for one leaf (the forest would darken where the
            // LOD switches cell size).
            let small = canopy_share(&g.lod_grid(x - 48, z - 48, 8, 12));
            eprintln!("({x}, {z}): exact {exact:.3}, 8-block cells {small:.3}, 16-block cells {coarse:.3}");
            assert!((exact - coarse).abs() < 0.2, "({x}, {z}): exact {exact:.3} vs coarse {coarse:.3}");
            assert!((exact - small).abs() < 0.1, "({x}, {z}): exact {exact:.3} vs 8-block cells {small:.3}");
        }
    }

    #[test]
    fn outside_the_border_is_empty() {
        let g = WorldGenerator::new(WorldSettings { size: WorldSize::from_blocks(1000), ..Default::default() });
        let grid = g.lod_grid(400, 0, 32, 8); // 400..656: border at 500
        assert!(grid[0].inside && grid[1].inside && grid[2].inside);
        assert!(grid[3..8].iter().all(|c| !c.inside && c.block == blocks::AIR));
    }
}
