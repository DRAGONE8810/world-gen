//! The world generator: settings in, chunks out.
//!
//! The generator is a pure function of `(WorldSettings, ChunkPos)`. It keeps
//! no mutable state, is `Send + Sync`, and any chunk can be generated on any
//! thread in any order with the same result. That property is what lets a
//! 60,000 km world exist without ever being stored: we only compute the
//! chunks near the player.
//!
//! ## Pipeline for one column
//!
//! 1. **Shape** ([`TerrainShaper`]): climate noise → base height `H`,
//!    roughness `A` and river strength.
//! 2. **Density**: a block at height `y` is solid when
//!    `d(x, y, z) = (H − y) + A · N(x, y, z) > 0`, where `N` is 3D fractal
//!    noise. With `A ≈ 1` this is just a heightmap; with large `A` (mountains)
//!    the 3D term carves cliffs, overhangs and arches.
//! 3. **Surface rules**: each solid block that has air or water above it
//!    becomes grass/sand/gravel/snow…, the next few blocks become filler
//!    (dirt, sand), the rest stone. Water fills air at or below sea level.
//!
//! `N` is expensive (3D, several octaves), so like most voxel engines we
//! sample it on a coarse grid of 4×8×4-block cells and interpolate
//! trilinearly. Because `|N| ≤ NOISE_BOUND`, every block more than
//! `A · NOISE_BOUND` above `H` is certainly air and every block that far below
//! is certainly solid, so noise is only evaluated in a thin band around
//! the surface.

use crate::block::{blocks, BlockId};
use crate::chunk::{Chunk, ChunkPos, CHUNK_SIZE, SECTIONS_PER_CHUNK};
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::palette::{section_index, PalettedSection, SECTION_SIZE, SECTION_VOLUME};
use crate::rng::{derive_seed, hash_position, hash_position_f64};
use crate::settings::{WorldSettings, WorldType, MAX_Y, MIN_Y, SEA_LEVEL, WORLD_HEIGHT};
use crate::terrain::{Shape, TerrainShaper};

/// Horizontal size of a density-noise cell, in blocks.
const CELL_XZ: i32 = 4;
/// Vertical size of a density-noise cell, in blocks.
const CELL_Y: i32 = 8;
/// Number of cell corners stacked vertically over the world height.
const GRID_Y: usize = (WORLD_HEIGHT / CELL_Y) as usize + 1;
/// Scales the raw fBm (std ≈ 0.25) so `A` is roughly "blocks of variation".
const NOISE_GAIN: f64 = 2.5;
/// Upper bound of `|N|` after gain (fBm is bounded by ~1.04 · gain).
const NOISE_BOUND: f64 = 2.7;

/// What the generator knows about a single (x, z) column without building
/// the chunk. Shared by chunk generation, the map tool, the menu preview and
/// the HUD; a test guarantees it matches the generated chunk exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSample {
    /// Y of the highest solid block.
    pub surface_y: i32,
    /// Block placed at `surface_y`.
    pub top_block: BlockId,
    /// Whether water covers this column (`surface_y < SEA_LEVEL`).
    pub underwater: bool,
    /// Inside the world border?
    pub inside: bool,
    /// Climate, base height and roughness (default for Flat worlds).
    pub shape: Shape,
}

impl ColumnSample {
    const OUTSIDE: ColumnSample = ColumnSample {
        surface_y: MIN_Y - 1,
        top_block: blocks::AIR,
        underwater: false,
        inside: false,
        shape: Shape {
            climate: crate::terrain::Climate { continentalness: 0.0, erosion: 0.0, weirdness: 0.0, peaks_valleys: 0.0 },
            height: 0.0,
            roughness: 0.0,
            river: 0.0,
        },
    };
}

pub struct WorldGenerator {
    settings: WorldSettings,
    shaper: TerrainShaper,
    density_noise: OctaveNoise,
    bedrock_seed: u64,
    surface_seed: u64,
    /// Horizontal stretch: 4 for "Large biomes", else 1.
    scale: f64,
}

/// Height range of a column where the density noise matters.
#[derive(Clone, Copy)]
struct Band {
    /// Lowest y where the block might be air (everything below is solid).
    bottom: i32,
    /// Highest y where the block might be solid (everything above is air).
    top: i32,
}

impl WorldGenerator {
    pub fn new(settings: WorldSettings) -> Self {
        let seed = settings.seed;
        let scale = if settings.world_type == WorldType::LargeBiomes { 4.0 } else { 1.0 };
        Self {
            shaper: TerrainShaper::new(seed, settings.world_type),
            // Features 64 blocks down to 8 blocks: blob-sized overhangs and
            // ledges rather than single-block speckle.
            density_noise: OctaveNoise::new(
                derive_seed(seed, "terrain/density"),
                &OctaveConfig::new(-6, &[1.0, 1.0, 1.0, 1.0]),
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

    fn is_flat(&self) -> bool {
        self.settings.world_type == WorldType::Flat
    }

    /// Shape with height clamped so the noise band stays inside the world.
    fn column_shape(&self, x: i32, z: i32) -> Shape {
        let mut s = self.shaper.shape(x, z);
        let margin = s.roughness * NOISE_BOUND;
        s.height = s.height.clamp(MIN_Y as f64 + 8.0 + margin, MAX_Y as f64 - 2.0 - margin);
        s
    }

    fn band(shape: &Shape) -> Band {
        let m = shape.roughness * NOISE_BOUND;
        Band { bottom: (shape.height - m).ceil() as i32, top: (shape.height + m).floor() as i32 }
    }

    /// Density noise at grid corner (gx, gy, gz) = block (4gx, MIN_Y + 8gy, 4gz).
    #[inline]
    fn corner(&self, gx: i32, gy: usize, gz: i32) -> f64 {
        let x = (gx * CELL_XZ) as f64 / self.scale;
        let y = (MIN_Y + gy as i32 * CELL_Y) as f64;
        let z = (gz * CELL_XZ) as f64 / self.scale;
        self.density_noise.sample(x, y, z) * NOISE_GAIN
    }

    /// The single source of truth for "is this block solid?" given the 8
    /// corners of its cell (order: bit0 = +x, bit1 = +y, bit2 = +z).
    #[inline]
    fn solid(shape: &Shape, y: i32, c: &[f64; 8], fx: f64, fy: f64, fz: f64) -> bool {
        let x00 = c[0] + fx * (c[1] - c[0]);
        let x10 = c[2] + fx * (c[3] - c[2]);
        let x01 = c[4] + fx * (c[5] - c[4]);
        let x11 = c[6] + fx * (c[7] - c[6]);
        let y0 = x00 + fy * (x10 - x00);
        let y1 = x01 + fy * (x11 - x01);
        let n = y0 + fz * (y1 - y0);
        (shape.height - y as f64) + shape.roughness * n > 0.0
    }

    /// Top block for a solid block exposed to air/water at height `y`.
    fn top_block(&self, x: i32, y: i32, z: i32, shape: &Shape) -> BlockId {
        if self.is_flat() {
            return blocks::GRASS;
        }
        if y < SEA_LEVEL - 1 {
            // Sea and river beds: sand in the shallows, gravel/clay deeper.
            let depth = SEA_LEVEL - y;
            return if depth < 7 || shape.river > 0.3 {
                blocks::SAND
            } else if hash_position(self.surface_seed, x, 0, z).is_multiple_of(8) {
                blocks::CLAY
            } else {
                blocks::GRAVEL
            };
        }
        let coast = shape.climate.continentalness < -0.03;
        if y <= SEA_LEVEL + 1 && (coast || shape.river > 0.2) {
            return blocks::SAND;
        }
        // Jitter the snow and tree lines by a few blocks per column.
        let jitter = (hash_position(self.surface_seed, x, 2, z) % 9) as i32 - 4;
        if y >= 195 + jitter {
            return blocks::SNOW;
        }
        if y >= 160 + jitter || shape.roughness > 9.0 && y > 120 + jitter {
            return blocks::STONE; // bare rock above the tree line / on crags
        }
        blocks::GRASS
    }

    fn filler_for(top: BlockId) -> BlockId {
        match top {
            blocks::GRASS => blocks::DIRT,
            blocks::SNOW => blocks::STONE,
            other => other,
        }
    }

    pub fn sample_column(&self, x: i32, z: i32) -> ColumnSample {
        if !self.settings.size.contains_column(x, z) {
            return ColumnSample::OUTSIDE;
        }
        if self.is_flat() {
            let y = MIN_Y + 3;
            return ColumnSample {
                surface_y: y,
                top_block: blocks::GRASS,
                underwater: false,
                inside: true,
                shape: ColumnSample::OUTSIDE.shape,
            };
        }
        let shape = self.column_shape(x, z);
        let band = Self::band(&shape);
        let (gx, gz) = (x.div_euclid(CELL_XZ), z.div_euclid(CELL_XZ));
        let fx = x.rem_euclid(CELL_XZ) as f64 / CELL_XZ as f64;
        let fz = z.rem_euclid(CELL_XZ) as f64 / CELL_XZ as f64;
        let level = |gy: usize| -> [f64; 4] {
            [
                self.corner(gx, gy, gz),
                self.corner(gx + 1, gy, gz),
                self.corner(gx, gy, gz + 1),
                self.corner(gx + 1, gy, gz + 1),
            ]
        };

        let mut surface = band.bottom - 1; // everything below the band is solid
        let mut cached: Option<(usize, [f64; 8])> = None;
        for y in (band.bottom..=band.top).rev() {
            let ly = y - MIN_Y;
            let gy = (ly / CELL_Y) as usize;
            let fy = (ly % CELL_Y) as f64 / CELL_Y as f64;
            let c = match cached {
                Some((g, c)) if g == gy => c,
                _ => {
                    let (lo, hi) = (level(gy), level(gy + 1));
                    let c = [lo[0], lo[1], hi[0], hi[1], lo[2], lo[3], hi[2], hi[3]];
                    cached = Some((gy, c));
                    c
                }
            };
            if Self::solid(&shape, y, &c, fx, fy, fz) {
                surface = y;
                break;
            }
        }
        ColumnSample {
            surface_y: surface,
            top_block: self.top_block(x, surface, z, &shape),
            underwater: surface < SEA_LEVEL,
            inside: true,
            shape,
        }
    }

    /// Generates one full-height chunk.
    pub fn generate_chunk(&self, pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::empty(pos);
        if !self.chunk_touches_world(pos) {
            return chunk;
        }
        let (bx0, bz0) = (pos.min_block_x(), pos.min_block_z());

        // Per-column shape and noise band.
        let mut shapes = [Shape::default(); 256];
        let mut bands = [Band { bottom: 0, top: 0 }; 256];
        let mut inside = [false; 256];
        let (mut lo_y, mut hi_y) = (i32::MAX, i32::MIN);
        for lz in 0..16 {
            for lx in 0..16 {
                let i = lx + lz * 16;
                let (x, z) = (bx0 + lx as i32, bz0 + lz as i32);
                inside[i] = self.settings.size.contains_column(x, z);
                if !inside[i] || self.is_flat() {
                    continue;
                }
                shapes[i] = self.column_shape(x, z);
                bands[i] = Self::band(&shapes[i]);
                lo_y = lo_y.min(bands[i].bottom);
                hi_y = hi_y.max(bands[i].top);
            }
        }

        // Density-noise corners for this chunk: 5 × GRID_Y × 5, filled only
        // for the vertical range some column actually needs.
        let mut grid = vec![0.0f64; 5 * GRID_Y * 5];
        let gidx = |i: usize, gy: usize, k: usize| (gy * 5 + k) * 5 + i;
        if lo_y <= hi_y {
            let gy_lo = ((lo_y - MIN_Y) / CELL_Y) as usize;
            let gy_hi = (((hi_y - MIN_Y) / CELL_Y) as usize + 1).min(GRID_Y - 1);
            let (gx0, gz0) = (bx0.div_euclid(CELL_XZ), bz0.div_euclid(CELL_XZ));
            for gy in gy_lo..=gy_hi {
                for k in 0..5 {
                    for i in 0..5 {
                        grid[gidx(i, gy, k)] = self.corner(gx0 + i as i32, gy, gz0 + k as i32);
                    }
                }
            }
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
                let i = lx + lz * 16;
                if !inside[i] {
                    continue;
                }
                let (x, z) = (bx0 + lx as i32, bz0 + lz as i32);

                if self.is_flat() {
                    put(lx, MIN_Y, lz, blocks::BEDROCK);
                    put(lx, MIN_Y + 1, lz, blocks::DIRT);
                    put(lx, MIN_Y + 2, lz, blocks::DIRT);
                    put(lx, MIN_Y + 3, lz, blocks::GRASS);
                    chunk.heightmap[i] = (MIN_Y + 3) as i16;
                    continue;
                }

                let shape = &shapes[i];
                let band = bands[i];
                let (ci, ck) = (lx / CELL_XZ as usize, lz / CELL_XZ as usize);
                let fx = (lx % CELL_XZ as usize) as f64 / CELL_XZ as f64;
                let fz = (lz % CELL_XZ as usize) as f64 / CELL_XZ as f64;
                // Filler thickness varies 3–5 blocks so layer boundaries
                // aren't perfectly flat.
                let filler_depth = 3 + (hash_position(self.surface_seed, x, 1, z) % 3) as i32;

                let mut highest = MIN_Y - 1;
                // Depth below the most recent exposed surface, and the
                // block types chosen for that surface.
                let mut depth = i32::MAX;
                let (mut top, mut filler) = (blocks::STONE, blocks::STONE);
                let mut solid_above = false;
                for y in (MIN_Y..=band.top.max(SEA_LEVEL)).rev() {
                    let solid = if y > band.top {
                        false
                    } else if y < band.bottom {
                        true
                    } else {
                        let ly = y - MIN_Y;
                        let gy = (ly / CELL_Y) as usize;
                        let fy = (ly % CELL_Y) as f64 / CELL_Y as f64;
                        let c = [
                            grid[gidx(ci, gy, ck)],
                            grid[gidx(ci + 1, gy, ck)],
                            grid[gidx(ci, gy + 1, ck)],
                            grid[gidx(ci + 1, gy + 1, ck)],
                            grid[gidx(ci, gy, ck + 1)],
                            grid[gidx(ci + 1, gy, ck + 1)],
                            grid[gidx(ci, gy + 1, ck + 1)],
                            grid[gidx(ci + 1, gy + 1, ck + 1)],
                        ];
                        Self::solid(shape, y, &c, fx, fy, fz)
                    };

                    if !solid {
                        if y <= SEA_LEVEL {
                            put(lx, y, lz, blocks::WATER);
                            highest = highest.max(y);
                        }
                        solid_above = false;
                        continue;
                    }
                    if !solid_above {
                        // A newly exposed surface (the top, or under an overhang).
                        depth = 0;
                        top = self.top_block(x, y, z, shape);
                        filler = Self::filler_for(top);
                    }
                    solid_above = true;
                    highest = highest.max(y);

                    let b = if y == MIN_Y || (y < MIN_Y + 5 && self.is_bedrock(x, y, z)) {
                        blocks::BEDROCK
                    } else if depth == 0 {
                        top
                    } else if depth < filler_depth {
                        filler
                    } else if depth == filler_depth && top == blocks::SAND {
                        blocks::SANDSTONE // sand is held up by sandstone
                    } else {
                        blocks::STONE
                    };
                    put(lx, y, lz, b);
                    depth = depth.saturating_add(1);
                }
                chunk.heightmap[i] = highest as i16;
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
        let max_radius = (self.settings.size.half_width() - 8).clamp(0, 6000);
        let step = 16;
        let mut r = 0;
        while r <= max_radius {
            let n = if r == 0 { 1 } else { (r * 8) / step };
            for i in 0..n {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let (x, z) = ((a.cos() * r as f64) as i32, (a.sin() * r as f64) as i32);
                let c = self.sample_column(x, z);
                if c.inside && !c.underwater && c.surface_y < 140 && c.shape.river < 0.1 {
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
const _: () = assert!((WORLD_HEIGHT as usize).is_multiple_of(CELL_Y as usize));
const _: () = assert!(CHUNK_SIZE % CELL_XZ == 0);

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

    /// The map tool, menu preview and HUD rely on `sample_column`; it must
    /// describe the generated chunk exactly, including in rugged mountains
    /// where 3D noise creates overhangs.
    #[test]
    fn column_sample_matches_generated_chunk() {
        for t in [WorldType::Normal, WorldType::Amplified, WorldType::Islands, WorldType::LargeBiomes, WorldType::Flat]
        {
            let g = generator(77, t);
            for pos in [ChunkPos::new(-3, 12), ChunkPos::new(40, -7), ChunkPos::new(-90, -60), ChunkPos::new(150, 151)]
            {
                let c = g.generate_chunk(pos);
                for lz in 0..16 {
                    for lx in 0..16 {
                        let s = g.sample_column(pos.min_block_x() + lx as i32, pos.min_block_z() + lz as i32);
                        assert_eq!(c.get(lx, s.surface_y, lz), s.top_block, "{t:?} {pos:?} {lx},{lz}");
                        for y in s.surface_y + 1..=MAX_Y {
                            let b = c.get(lx, y, lz);
                            assert!(b == blocks::AIR || b == blocks::WATER, "{t:?} solid above surface at y={y}");
                        }
                        assert_eq!(c.get(lx, MIN_Y, lz), blocks::BEDROCK);
                        assert_eq!(c.height_at(lx, lz), s.surface_y.max(if s.underwater { SEA_LEVEL } else { MIN_Y }));
                    }
                }
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
        // Terrain near the border is ordinary terrain, not a glitch: heights
        // vary smoothly between neighbouring columns.
        let mut prev = g.sample_column(29_999_900, -29_999_984).surface_y;
        for x in 29_999_901..29_999_984 {
            let h = g.sample_column(x, -29_999_984).surface_y;
            assert!((MIN_Y + 5..MAX_Y).contains(&h));
            assert!((h - prev).abs() < 24, "jump {prev} -> {h} at x={x}");
            prev = h;
        }
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
    fn spawn_is_on_dry_land() {
        let g = generator(9, WorldType::Normal);
        let (x, y, z) = g.find_spawn();
        let c = g.sample_column(x, z);
        assert!(!c.underwater);
        assert_eq!(y, c.surface_y + 1);
    }

    /// Statistics over a 40 km × 40 km grid. These are the "is the world
    /// believable?" checks; the exact numbers are recorded in devlog 02.
    fn stats(t: WorldType, seed: u64) -> (f64, i32, i32, f64) {
        let g =
            WorldGenerator::new(WorldSettings { seed, world_type: t, size: WorldSize::from_km(64.0), features: true });
        let (mut land, mut n, mut max_h, mut min_h, mut mountains) = (0, 0, MIN_Y, MAX_Y, 0);
        for iz in 0..80 {
            for ix in 0..80 {
                let s = g.sample_column(ix * 500 - 20_000, iz * 500 - 20_000);
                n += 1;
                land += !s.underwater as i32;
                mountains += (s.surface_y > 140) as i32;
                max_h = max_h.max(s.surface_y);
                min_h = min_h.min(s.surface_y);
            }
        }
        (land as f64 / n as f64, min_h, max_h, mountains as f64 / n as f64)
    }

    #[test]
    fn normal_world_has_continents_oceans_and_mountains() {
        for seed in [1, 2, 3] {
            let (land, min_h, max_h, mountains) = stats(WorldType::Normal, seed);
            assert!((0.40..=0.80).contains(&land), "seed {seed}: land fraction {land}");
            assert!(min_h < SEA_LEVEL - 30, "seed {seed}: no deep ocean (min {min_h})");
            assert!(max_h > 170, "seed {seed}: no high mountains (max {max_h})");
            assert!((0.005..0.25).contains(&mountains), "seed {seed}: mountain share {mountains}");
        }
    }

    #[test]
    fn islands_world_is_mostly_ocean() {
        let (land, ..) = stats(WorldType::Islands, 4);
        assert!((0.03..0.35).contains(&land), "islands land fraction {land}");
    }

    #[test]
    fn amplified_is_taller_than_normal() {
        let (_, _, normal_max, normal_mtn) = stats(WorldType::Normal, 6);
        let (_, _, amp_max, amp_mtn) = stats(WorldType::Amplified, 6);
        assert!(amp_max > normal_max && amp_mtn > normal_mtn * 1.5, "{normal_max}/{amp_max} {normal_mtn}/{amp_mtn}");
    }

    #[test]
    fn rugged_terrain_has_overhangs_flat_terrain_does_not() {
        // Count columns where a solid block has air below it before the
        // ground (an overhang), in rough vs smooth areas.
        let g = generator(11, WorldType::Normal);
        let (mut rough_cols, mut rough_overhangs) = (0, 0);
        for cz in -40..40 {
            for cx in -40..40 {
                let pos = ChunkPos::new(cx * 5, cz * 5);
                let s = g.sample_column(pos.min_block_x(), pos.min_block_z());
                if s.shape.roughness < 8.0 {
                    continue;
                }
                rough_cols += 1;
                let c = g.generate_chunk(pos);
                let mut found = false;
                'col: for lz in 0..16 {
                    for lx in 0..16 {
                        let top = c.height_at(lx, lz);
                        let mut seen_air = false;
                        for y in (top - 60..top).rev() {
                            let b = c.get(lx, y, lz);
                            if b.is_air() {
                                seen_air = true;
                            } else if seen_air && b != blocks::WATER {
                                found = true;
                                break 'col;
                            }
                        }
                    }
                }
                rough_overhangs += found as i32;
            }
        }
        assert!(rough_cols > 5, "test found too few rugged areas ({rough_cols})");
        assert!(rough_overhangs > 0, "no overhangs in {rough_cols} rugged chunks");
    }
}
