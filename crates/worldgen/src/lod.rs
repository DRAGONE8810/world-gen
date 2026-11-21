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
//!   one tree candidate in the middle. The share of cells that show canopy
//!   then matches the forest's density, which is what you see from far
//!   away, at a fraction of the cost.

use crate::biome::Biome;
use crate::block::{blocks, BlockId};
use crate::generator::{ColumnSample, WorldGenerator};
use crate::settings::SEA_LEVEL;
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
    /// Biome of the ground, for grass and leaf tints.
    pub biome: Biome,
    /// Y of the terrain surface (below water or trees).
    pub ground_y: i32,
}

impl LodColumn {
    fn from_sample(s: &ColumnSample) -> Self {
        let (top_y, block) = if !s.inside {
            (s.surface_y, blocks::AIR)
        } else if s.surface_y < SEA_LEVEL {
            (SEA_LEVEL, if s.ice { blocks::ICE } else { blocks::WATER })
        } else {
            (s.surface_y, s.top_block)
        };
        Self { inside: s.inside, top_y, block, biome: s.biome, ground_y: s.surface_y }
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
                self.sample_column(x0 + ix * cell + half, z0 + iz * cell + half)
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
                if let Some(t) = trees::resolve(tx, tz, hash, s.biome, s.surface_y, s.top_block, self.max_y()) {
                    let c = &mut out[i];
                    trees::build(&t, &mut |_, y, _, b, is_log| {
                        if y <= self.max_y() {
                            c.add_tree_block(y, b, is_log);
                        }
                    });
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
                let Some(t) = trees::resolve(x, z, hash, s.biome, s.surface_y, s.top_block, self.max_y()) else {
                    continue;
                };
                trees::build(&t, &mut |wx, wy, wz, b, is_log| {
                    if wy > self.max_y() || !size.contains_column(wx, wz) {
                        return;
                    }
                    let (ix, iz) = ((wx - x0).div_euclid(cell), (wz - z0).div_euclid(cell));
                    if (0..n as i32).contains(&ix) && (0..n as i32).contains(&iz) {
                        out[ix as usize + iz as usize * n].add_tree_block(wy, b, is_log);
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkPos;
    use crate::settings::{WorldHeight, WorldSize, WorldType};
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
            eprintln!("({x}, {z}): exact {exact:.3}, 16-block cells {coarse:.3}");
            assert!((exact - coarse).abs() < 0.2, "({x}, {z}): exact {exact:.3} vs coarse {coarse:.3}");
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
