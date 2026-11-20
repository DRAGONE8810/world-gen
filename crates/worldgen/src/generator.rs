//! The world generator: settings in, chunks out.
//!
//! The generator is a pure function of `(WorldSettings, ChunkPos)`. It keeps
//! no mutable state, is `Send + Sync`, and any chunk can be generated on any
//! thread in any order with the same result. That property is what lets a
//! 60,000 km world exist without ever being stored: we only compute the
//! chunks near the player.
//!
//! Step 1 terrain is deliberately simple (one fractal heightmap). Step 2
//! replaces `terrain_height` with the full continent/erosion/ridge model and
//! 3D density; the chunk-filling and surface pipeline below stays the same.

use crate::block::{blocks, BlockId};
use crate::chunk::{Chunk, ChunkPos, CHUNK_SIZE, SECTIONS_PER_CHUNK};
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::palette::{section_index, PalettedSection, SECTION_SIZE, SECTION_VOLUME};
use crate::rng::{derive_seed, hash_position, hash_position_f64};
use crate::settings::{WorldSettings, WorldType, MAX_Y, MIN_Y, SEA_LEVEL, WORLD_HEIGHT};

/// What the generator knows about a single (x, z) column without building
/// the chunk. Shared by chunk generation and the map tool, so maps always
/// match the world exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSample {
    /// Y of the top solid block.
    pub surface_y: i32,
    /// Block placed at `surface_y`.
    pub top_block: BlockId,
    /// Whether water covers this column (`surface_y < SEA_LEVEL`).
    pub underwater: bool,
    /// Inside the world border?
    pub inside: bool,
}

pub struct WorldGenerator {
    settings: WorldSettings,
    height_noise: OctaveNoise,
    detail_noise: OctaveNoise,
    bedrock_seed: u64,
    surface_seed: u64,
    /// Horizontal stretch: 4 for "Large biomes", else 1.
    scale: f64,
}

impl WorldGenerator {
    pub fn new(settings: WorldSettings) -> Self {
        let seed = settings.seed;
        let scale = if settings.world_type == WorldType::LargeBiomes { 4.0 } else { 1.0 };
        Self {
            height_noise: OctaveNoise::new(
                derive_seed(seed, "terrain/height"),
                // Lowest octave 2^-10: features ~1 km wide, down to ~8 blocks.
                &OctaveConfig::new(-10, &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
            ),
            detail_noise: OctaveNoise::new(
                derive_seed(seed, "terrain/detail"),
                &OctaveConfig::new(-5, &[1.0, 1.0, 1.0]),
            ),
            bedrock_seed: derive_seed(seed, "surface/bedrock"),
            surface_seed: derive_seed(seed, "surface/depth"),
            scale,
            settings,
        }
    }

    pub fn settings(&self) -> &WorldSettings {
        &self.settings
    }

    /// Terrain height for a column inside the border.
    fn terrain_height(&self, x: i32, z: i32) -> i32 {
        let (fx, fz) = (x as f64 / self.scale, z as f64 / self.scale);
        let h = match self.settings.world_type {
            WorldType::Flat => return MIN_Y + 3,
            WorldType::Normal | WorldType::LargeBiomes => {
                let n = self.height_noise.sample_2d(fx, fz);
                // Push values away from 0 a little so we get both real
                // oceans and real hills rather than a flat average.
                SEA_LEVEL as f64 + 4.0 + n * 70.0 + self.detail_noise.sample_2d(fx, fz) * 3.0
            }
            WorldType::Amplified => {
                let n = self.height_noise.sample_2d(fx, fz);
                SEA_LEVEL as f64 + 10.0 + n * 170.0 + self.detail_noise.sample_2d(fx, fz) * 6.0
            }
            WorldType::Islands => {
                let n = self.height_noise.sample_2d(fx, fz);
                // Shift down so only the peaks of the noise break the surface.
                SEA_LEVEL as f64 - 18.0 + n * 110.0 + self.detail_noise.sample_2d(fx, fz) * 3.0
            }
        };
        (h.round() as i32).clamp(MIN_Y + 5, MAX_Y - 1)
    }

    /// Top block for a column given its height.
    fn surface_block(&self, x: i32, surface_y: i32, z: i32) -> BlockId {
        if self.settings.world_type == WorldType::Flat {
            return blocks::GRASS;
        }
        if surface_y < SEA_LEVEL - 1 {
            // Sea floor: sand in the shallows, gravel/clay deeper down.
            let depth = SEA_LEVEL - surface_y;
            return if depth < 8 {
                blocks::SAND
            } else if hash_position(self.surface_seed, x, 0, z).is_multiple_of(8) {
                blocks::CLAY
            } else {
                blocks::GRAVEL
            };
        }
        if surface_y <= SEA_LEVEL + 2 {
            return blocks::SAND; // beaches
        }
        if surface_y >= 200 {
            return blocks::SNOW;
        }
        if surface_y >= 165 {
            return blocks::STONE; // bare rock above the tree line
        }
        blocks::GRASS
    }

    pub fn sample_column(&self, x: i32, z: i32) -> ColumnSample {
        if !self.settings.size.contains_column(x, z) {
            return ColumnSample { surface_y: MIN_Y - 1, top_block: blocks::AIR, underwater: false, inside: false };
        }
        let surface_y = self.terrain_height(x, z);
        ColumnSample {
            surface_y,
            top_block: self.surface_block(x, surface_y, z),
            underwater: surface_y < SEA_LEVEL && self.settings.world_type != WorldType::Flat,
            inside: true,
        }
    }

    /// Generates one full-height chunk.
    pub fn generate_chunk(&self, pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::empty(pos);
        let (bx0, bz0) = (pos.min_block_x(), pos.min_block_z());
        if !self.chunk_touches_world(pos) {
            return chunk;
        }

        // Fill a dense [section][16^3] buffer first: sequential writes into
        // plain arrays are much faster than palette writes block by block.
        let mut dense = vec![[blocks::AIR; SECTION_VOLUME]; SECTIONS_PER_CHUNK];
        let mut put = |lx: usize, y: i32, lz: usize, b: BlockId| {
            let ly = (y - MIN_Y) as usize;
            dense[ly / SECTION_SIZE][section_index(lx, ly % SECTION_SIZE, lz)] = b;
        };

        for lz in 0..CHUNK_SIZE as usize {
            for lx in 0..CHUNK_SIZE as usize {
                let (x, z) = (bx0 + lx as i32, bz0 + lz as i32);
                let col = self.sample_column(x, z);
                if !col.inside {
                    continue;
                }
                let top = col.surface_y;
                let flat = self.settings.world_type == WorldType::Flat;

                // Filler thickness varies 3–5 blocks so layer boundaries
                // aren't perfectly flat.
                let filler_depth = 3 + (hash_position(self.surface_seed, x, 1, z) % 3) as i32;
                let filler = match col.top_block {
                    blocks::GRASS => blocks::DIRT,
                    blocks::SAND => blocks::SAND,
                    blocks::SNOW => blocks::STONE,
                    other => other,
                };

                for y in MIN_Y..=top {
                    let b = if y == MIN_Y || (!flat && y < MIN_Y + 5 && self.is_bedrock(x, y, z)) {
                        blocks::BEDROCK
                    } else if y == top {
                        col.top_block
                    } else if y > top - filler_depth || (flat && y > MIN_Y) {
                        filler
                    } else {
                        blocks::STONE
                    };
                    put(lx, y, lz, b);
                }
                // Sand under a beach/sea floor is held up by sandstone.
                if col.top_block == blocks::SAND && top - filler_depth > MIN_Y {
                    put(lx, top - filler_depth, lz, blocks::SANDSTONE);
                }
                if col.underwater {
                    for y in (top + 1)..=SEA_LEVEL {
                        put(lx, y, lz, blocks::WATER);
                    }
                }
                let hm_top = if col.underwater { SEA_LEVEL } else { top };
                chunk.heightmap[lx + lz * 16] = hm_top as i16;
            }
        }

        for (section, blocks) in chunk.sections.iter_mut().zip(dense.iter()) {
            *section = PalettedSection::from_dense(blocks);
        }
        chunk
    }

    /// Bedrock floor: always at MIN_Y, then a ragged layer that thins out
    /// over the next 4 blocks.
    fn is_bedrock(&self, x: i32, y: i32, z: i32) -> bool {
        let layer = (y - MIN_Y) as f64; // 1..=4
        hash_position_f64(self.bedrock_seed, x, y, z) < 1.0 - layer / 5.0
    }

    pub fn chunk_touches_world(&self, pos: ChunkPos) -> bool {
        let s = self.settings.size;
        let (x0, z0) = (pos.min_block_x(), pos.min_block_z());
        s.contains_column(x0, z0)
            || s.contains_column(x0 + 15, z0)
            || s.contains_column(x0, z0 + 15)
            || s.contains_column(x0 + 15, z0 + 15)
    }

    /// A dry-land spawn point as close to the centre as possible, searched
    /// along an outward spiral. Falls back to (0, 0) if the world is all sea.
    pub fn find_spawn(&self) -> (i32, i32, i32) {
        let max_radius = (self.settings.size.half_width() - 8).clamp(0, 4096);
        let step = 8;
        let mut r = 0;
        while r <= max_radius {
            let n = if r == 0 { 1 } else { (r * 8) / step };
            for i in 0..n {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let (x, z) = ((a.cos() * r as f64) as i32, (a.sin() * r as f64) as i32);
                let c = self.sample_column(x, z);
                if c.inside && !c.underwater && c.surface_y < 160 {
                    return (x, c.surface_y + 1, z);
                }
            }
            r += step;
        }
        let c = self.sample_column(0, 0);
        (0, c.surface_y.max(SEA_LEVEL) + 1, 0)
    }
}

const _: () = assert!((WORLD_HEIGHT as usize).is_multiple_of(SECTION_SIZE));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::WorldSize;

    fn generator(seed: u64, world_type: WorldType) -> WorldGenerator {
        WorldGenerator::new(WorldSettings { seed, world_type, size: WorldSize::from_km(64.0), features: true })
    }

    fn chunks_equal(a: &Chunk, b: &Chunk) -> bool {
        a.sections == b.sections && a.heightmap == b.heightmap
    }

    #[test]
    fn same_seed_same_chunk_regardless_of_order() {
        let g1 = generator(123, WorldType::Normal);
        let g2 = generator(123, WorldType::Normal);
        let positions = [ChunkPos::new(0, 0), ChunkPos::new(-5, 9), ChunkPos::new(100, -100)];
        let a: Vec<_> = positions.iter().map(|&p| g1.generate_chunk(p)).collect();
        let b: Vec<_> = positions.iter().rev().map(|&p| g2.generate_chunk(p)).collect();
        for (x, y) in a.iter().zip(b.iter().rev()) {
            assert!(chunks_equal(x, y));
        }
    }

    #[test]
    fn different_seeds_differ() {
        let a = generator(1, WorldType::Normal).generate_chunk(ChunkPos::new(3, 3));
        let b = generator(2, WorldType::Normal).generate_chunk(ChunkPos::new(3, 3));
        assert!(!chunks_equal(&a, &b));
    }

    #[test]
    fn column_sample_matches_generated_chunk() {
        let g = generator(77, WorldType::Normal);
        let pos = ChunkPos::new(-3, 12);
        let c = g.generate_chunk(pos);
        for lz in 0..16 {
            for lx in 0..16 {
                let s = g.sample_column(pos.min_block_x() + lx as i32, pos.min_block_z() + lz as i32);
                assert_eq!(c.get(lx, s.surface_y, lz), s.top_block);
                assert!(
                    c.get(lx, s.surface_y + 1, lz) == blocks::AIR || c.get(lx, s.surface_y + 1, lz) == blocks::WATER
                );
                assert_eq!(c.get(lx, MIN_Y, lz), blocks::BEDROCK);
            }
        }
    }

    #[test]
    fn nothing_outside_the_border() {
        let g = WorldGenerator::new(WorldSettings { size: WorldSize::from_blocks(1000), ..Default::default() });
        // Border at ±500: chunk 31 covers 496..511, chunk 32 starts at 512.
        assert!(g.generate_chunk(ChunkPos::new(32, 0)).is_empty());
        assert!(g.generate_chunk(ChunkPos::new(-33, 0)).is_empty());
        let edge = g.generate_chunk(ChunkPos::new(31, 0));
        assert!(!edge.is_empty());
        assert_ne!(edge.get(3, MIN_Y, 0), blocks::AIR); // x=499 inside
        assert_eq!(edge.get(4, MIN_Y, 0), blocks::AIR); // x=500 outside
    }

    #[test]
    fn works_at_the_far_edge_of_a_full_size_world() {
        let g = WorldGenerator::new(WorldSettings { size: WorldSize::full(), ..Default::default() });
        let edge = ChunkPos::from_block(29_999_983, -29_999_984);
        let c = g.generate_chunk(edge);
        assert!(!c.is_empty());
        // Terrain near the border is ordinary terrain, not a glitch.
        let h = g.sample_column(29_999_983, -29_999_984).surface_y;
        assert!((MIN_Y + 5..MAX_Y).contains(&h));
    }

    #[test]
    fn flat_world_layers() {
        let g = generator(5, WorldType::Flat);
        let c = g.generate_chunk(ChunkPos::new(0, 0));
        assert_eq!(c.get(0, MIN_Y, 0), blocks::BEDROCK);
        assert_eq!(c.get(0, MIN_Y + 1, 0), blocks::DIRT);
        assert_eq!(c.get(0, MIN_Y + 2, 0), blocks::DIRT);
        assert_eq!(c.get(0, MIN_Y + 3, 0), blocks::GRASS);
        assert_eq!(c.get(0, MIN_Y + 4, 0), blocks::AIR);
    }

    #[test]
    fn has_both_land_and_sea() {
        let g = generator(2024, WorldType::Normal);
        let mut land = 0;
        let mut sea = 0;
        for i in 0..400 {
            let c = g.sample_column((i % 20) * 400 - 4000, (i / 20) * 400 - 4000);
            if c.underwater {
                sea += 1
            } else {
                land += 1
            }
        }
        assert!(land > 40 && sea > 40, "land {land} sea {sea}");
    }

    #[test]
    fn spawn_is_on_dry_land() {
        let g = generator(9, WorldType::Normal);
        let (x, y, z) = g.find_spawn();
        let c = g.sample_column(x, z);
        assert!(!c.underwater);
        assert_eq!(y, c.surface_y + 1);
    }
}
