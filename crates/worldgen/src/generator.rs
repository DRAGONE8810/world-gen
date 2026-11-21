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
//! 3. **Biome** ([`crate::biome`]): chosen from the climate and the
//!    surface height found in step 2.
//! 4. **Surface rules**: each solid block that has air or water above it
//!    becomes the biome's top block (grass, sand, snow, podzol, red sand…),
//!    the next few blocks become filler (dirt, sand, terracotta bands), the
//!    rest stone. Water fills air at or below sea level, and freezes over
//!    in frozen biomes.
//! 5. **Trees** ([`crate::trees`]), if the world has features enabled.
//!
//! `N` is expensive (3D, several octaves), so like most voxel engines we
//! sample it on a coarse grid of 4×8×4-block cells and interpolate
//! trilinearly. Because `|N| ≤ NOISE_BOUND`, every block more than
//! `A · NOISE_BOUND` above `H` is certainly air and every block that far below
//! is certainly solid, so noise is only evaluated in a thin band around
//! the surface.

use crate::biome::{self, Biome, BiomeDither};
use crate::block::{blocks, BlockId};
use crate::chunk::{Chunk, ChunkPos, CHUNK_SIZE};
use crate::noise::{OctaveConfig, OctaveNoise};
use crate::palette::{section_index, PalettedSection, SECTION_SIZE, SECTION_VOLUME};
use crate::realistic::RealisticWorld;
use crate::region::{Flavour, Region, Regions};
use crate::rng::{derive_seed, hash_position, hash_position_f64, Xoroshiro128PlusPlus};
use crate::settings::{Generator, WorldSettings, WorldType, SEA_LEVEL};
use crate::spline::smoothstep;
use crate::terrain::{Shape, TerrainShaper};
use crate::tint::Tint;
use crate::trees::{self, Tree, TREE_CELL, TREE_REACH};

/// Horizontal size of a density-noise cell, in blocks.
const CELL_XZ: i32 = 4;
/// Vertical size of a density-noise cell, in blocks, in a default world.
/// Tall worlds use 8·k with k the largest power of two ≤ the terrain scale
/// (max 16): their noise is k times smoother, so coarser cells lose nothing.
const CELL_Y: i32 = 8;
/// Blocks below the lowest possible surface that can still differ from
/// stone (surface layers, 40 blocks of badlands terracotta, basalt, …).
const SUBSURFACE: i32 = 44;
/// Scales the raw fBm (std ≈ 0.25) so `A` is roughly "blocks of variation".
const NOISE_GAIN: f64 = 2.5;
/// Upper bound of `|N|` after gain (fBm is bounded by ~1.04 · gain).
const NOISE_BOUND: f64 = 2.7;

/// Vertical period of the badlands terracotta stripes.
const BAND_PERIOD: usize = 32;

/// What the generator knows about a single (x, z) column without building
/// the chunk. Shared by chunk generation, the map tool, the menu preview and
/// the HUD; a test guarantees it matches the generated chunk exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSample {
    /// Y of the highest solid block.
    pub surface_y: i32,
    /// Block placed at `surface_y`.
    pub top_block: BlockId,
    /// The rock a few blocks under the surface: what a cliff below this
    /// column shows (far terrain draws its steep walls with it).
    pub side_block: BlockId,
    /// Whether water covers this column (`surface_y < shape.water`: sea,
    /// lake or river).
    pub underwater: bool,
    /// Inside the world border?
    pub inside: bool,
    /// Climate, base height and roughness (default for Flat worlds).
    pub shape: Shape,
    pub biome: Biome,
    /// Grass and foliage colour (follows the climate smoothly).
    pub tint: Tint,
    /// The water surface above this column is frozen (ice at sea level).
    pub ice: bool,
}

impl ColumnSample {
    /// A column outside the world border (`surface_y` is the default
    /// world's floor; `WorldGenerator::sample_column` sets the real one).
    const OUTSIDE: ColumnSample = ColumnSample {
        surface_y: crate::settings::MIN_Y - 1,
        top_block: blocks::AIR,
        side_block: blocks::AIR,
        underwater: false,
        inside: false,
        shape: Shape {
            climate: crate::terrain::Climate {
                continentalness: 0.0,
                erosion: 0.0,
                weirdness: 0.0,
                peaks_valleys: 0.0,
                temperature: 0.0,
                humidity: 0.0,
            },
            height: 0.0,
            roughness: 0.0,
            river: 0.0,
            island: 0.0,
            vscale: 1.0,
            water: SEA_LEVEL,
            region: Region::NONE,
            pillar: 0.0,
        },
        biome: Biome::Plains,
        tint: Tint(0),
        ice: false,
    };

    /// Y of the top of the column, water included: the water surface
    /// over seas, lakes and rivers, the ground elsewhere.
    pub fn water_top(&self) -> i32 {
        if self.underwater {
            self.shape.water
        } else {
            self.surface_y
        }
    }

    /// Colour of the column seen from above, ignoring water depth: the top
    /// block, with grass tinted by biome and frozen water shown as ice.
    pub fn top_color(&self) -> [u8; 3] {
        if self.ice {
            blocks::ICE.info().color
        } else if self.top_block == blocks::GRASS {
            self.tint.color()
        } else {
            self.top_block.info().color
        }
    }

    /// Colour of the column seen from orbit: like `top_color`, with the
    /// biome's average tree canopy laid over the ground (a sampled column
    /// has no trees, and a forest seen from above is its leaves).
    pub fn orbit_color(&self) -> [u8; 3] {
        let ground = self.top_color();
        if !self.inside || self.ice || self.underwater {
            return ground;
        }
        let (cover, leaves) = crate::tint::canopy(self.biome, self.tint);
        std::array::from_fn(|c| (ground[c] as f64 + (leaves[c] as f64 - ground[c] as f64) * cover).round() as u8)
    }
}

/// Where column shapes come from. (One per generator, so the size
/// difference between the variants doesn't matter.)
#[allow(clippy::large_enum_variant)]
enum Shaper {
    Classic(TerrainShaper),
    Realistic(Box<RealisticWorld>),
}

impl Shaper {
    #[inline]
    fn shape(&self, x: i32, z: i32) -> Shape {
        match self {
            Shaper::Classic(s) => s.shape(x, z),
            Shaper::Realistic(r) => r.shape(x, z),
        }
    }
}

pub struct WorldGenerator {
    settings: WorldSettings,
    shaper: Shaper,
    density_noise: OctaveNoise,
    bedrock_seed: u64,
    surface_seed: u64,
    tree_seed: u64,
    /// Smooth 2D noise (~16-block blobs) for patchy ground: podzol in taiga,
    /// coarse dirt in savanna, gravel on stony shores, holes in sea ice.
    patch: OctaveNoise,
    /// Badlands terracotta colour stripes, one entry per y (mod 32).
    bands: [BlockId; BAND_PERIOD],
    /// Turns biome borders into transition zones (see `biome`).
    dither: BiomeDither,
    /// Rare regional landscapes (see `region`).
    regions: Regions,
    /// Flower colour patches (~10 blocks).
    flowers: OctaveNoise,
    /// Horizontal stretch: 4 for "Large biomes", times the terrain scale.
    scale: f64,
    /// Terrain scale of the world height (1 for 384 blocks).
    vscale: f64,
    min_y: i32,
    max_y: i32,
    /// Vertical density-noise cell size and number of grid corners.
    cell_y: i32,
    grid_y: usize,
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
    /// Creates the generator. Realistic worlds simulate their planet here,
    /// which takes from a fraction of a second to several seconds
    /// depending on `settings.quality`; classic worlds are instant.
    pub fn new(settings: WorldSettings) -> Self {
        let settings = settings.normalized();
        let seed = settings.seed;
        let vscale = settings.height.terrain_scale();
        let scale = if settings.world_type == WorldType::LargeBiomes { 4.0 } else { 1.0 } * vscale;
        let mut k = 1;
        while k * 2 <= 16 && (k * 2) as f64 <= vscale {
            k *= 2;
        }
        let cell_y = CELL_Y * k;
        let (min_y, max_y) = (settings.height.min_y(), settings.height.max_y());
        let shaper = match settings.generator {
            Generator::Classic => Shaper::Classic(TerrainShaper::new(seed, settings.world_type, vscale)),
            Generator::Realistic => Shaper::Realistic(Box::new(RealisticWorld::new(&settings))),
        };
        // Regions: ~3 km in a classic world (stretched like its climate),
        // six simulation cells (~500 km on a full-size planet) on a
        // realistic one.
        let region_cell = match &shaper {
            Shaper::Classic(_) => 3000.0 * scale,
            Shaper::Realistic(r) => 6.0 * r.cell_blocks(),
        };
        Self {
            dither: BiomeDither::new(seed),
            regions: Regions::new(seed, region_cell),
            flowers: OctaveNoise::new(derive_seed(seed, "surface/flowers"), &OctaveConfig::new(-3, &[1.0, 0.5])),
            shaper,
            // Features 64 blocks down to 8 blocks: blob-sized overhangs and
            // ledges rather than single-block speckle.
            density_noise: OctaveNoise::new(
                derive_seed(seed, "terrain/density"),
                &OctaveConfig::new(-6, &[1.0, 1.0, 1.0, 1.0]),
            ),
            bedrock_seed: derive_seed(seed, "surface/bedrock"),
            surface_seed: derive_seed(seed, "surface/depth"),
            tree_seed: derive_seed(seed, "features/trees"),
            patch: OctaveNoise::new(derive_seed(seed, "surface/patch"), &OctaveConfig::new(-4, &[1.0, 1.0])),
            bands: Self::make_bands(derive_seed(seed, "surface/badlands_bands")),
            scale,
            vscale,
            min_y,
            max_y,
            cell_y,
            grid_y: ((max_y - min_y + 1) as usize).div_ceil(cell_y as usize) + 1,
            settings,
        }
    }

    pub fn min_y(&self) -> i32 {
        self.min_y
    }
    pub fn max_y(&self) -> i32 {
        self.max_y
    }

    pub fn settings(&self) -> &WorldSettings {
        &self.settings
    }
    pub(crate) fn tree_seed(&self) -> u64 {
        self.tree_seed
    }

    fn is_flat(&self) -> bool {
        self.settings.generator == Generator::Classic && self.settings.world_type == WorldType::Flat
    }

    /// The simulated planet of a realistic world.
    pub fn realistic(&self) -> Option<&RealisticWorld> {
        match &self.shaper {
            Shaper::Realistic(r) => Some(r),
            Shaper::Classic(_) => None,
        }
    }

    /// Shape with its region and pillars, and the height clamped so the
    /// noise band stays inside the world.
    fn column_shape(&self, x: i32, z: i32) -> Shape {
        let mut s = self.shaper.shape(x, z);
        s.region = self.regions.at(x, z);
        if s.region.flavour == Flavour::Pillars {
            self.raise_pillars(&mut s, x, z);
        }
        let margin = s.roughness * NOISE_BOUND;
        s.height = s.height.clamp(self.min_y as f64 + 8.0 + margin, self.max_y as f64 - 2.0 - margin);
        s
    }

    /// Pillar forests (Zhangjiajie, Guilin): inside a pillar region, on
    /// warm, wet land away from rivers and the sea, sheer-sided towers
    /// rise from the ground, 35–90 blocks tall in a default world (taller
    /// and wider in tall worlds), packed close enough to leave gorges
    /// between them. Their density fades towards the region's edge.
    fn raise_pillars(&self, s: &mut Shape, x: i32, z: i32) {
        let cl = s.climate;
        // Not on beaches or in the sea (absolute blocks), nor high in the
        // mountains (default-world blocks).
        let land = s.height - s.water as f64;
        let suit = s.region.strength as f64
            * smoothstep(0.0, 0.15, cl.temperature)
            * smoothstep(-0.15, 0.05, cl.humidity)
            * smoothstep(3.0, 8.0, land)
            * (1.0 - smoothstep(60.0, 100.0, land / s.scale()))
            * (1.0 - smoothstep(0.02, 0.15, s.river));
        let size = 18.0 * s.scale().powf(0.4);
        if let Some((p, h)) = self.regions.pillar(x, z, size, 0.55 * suit) {
            let tall = (35.0 + 55.0 * ((h >> 48) & 0xFF) as f64 / 255.0) * s.scale().sqrt();
            s.pillar = tall * p;
            s.height += s.pillar;
            // Smooth walls: the 3D noise would fray them.
            s.roughness *= 1.0 - 0.8 * p;
        }
    }

    fn band(shape: &Shape) -> Band {
        let m = shape.roughness * NOISE_BOUND;
        Band { bottom: (shape.height - m).ceil() as i32, top: (shape.height + m).floor() as i32 }
    }

    /// Density noise at grid corner (gx, gy, gz) = block (4gx, min_y + cell_y·gy, 4gz).
    #[inline]
    fn corner(&self, gx: i32, gy: usize, gz: i32) -> f64 {
        let x = (gx * CELL_XZ) as f64 / self.scale;
        let wy = (self.min_y + gy as i32 * self.cell_y) as f64;
        // Heights are measured in default-world blocks, like x and z, so the
        // whole 3D noise field is magnified with the terrain.
        let y = if self.vscale == 1.0 { wy } else { SEA_LEVEL as f64 + (wy - SEA_LEVEL as f64) / self.vscale };
        let z = (gz * CELL_XZ) as f64 / self.scale;
        self.density_noise.sample(x, y, z) * NOISE_GAIN
    }

    /// The single source of truth for "is this block solid?" given the 8
    /// corners of its cell (order: bit0 = +x, bit1 = +y, bit2 = +z).
    #[inline]
    fn solid(shape: &Shape, y: i32, c: &[f64; 8], fx: f64, fy: f64, fz: f64) -> bool {
        Self::density(shape, y, c, fx, fy, fz) > 0.0
    }

    #[inline]
    fn density(shape: &Shape, y: i32, c: &[f64; 8], fx: f64, fy: f64, fz: f64) -> f64 {
        let x00 = c[0] + fx * (c[1] - c[0]);
        let x10 = c[2] + fx * (c[3] - c[2]);
        let x01 = c[4] + fx * (c[5] - c[4]);
        let x11 = c[6] + fx * (c[7] - c[6]);
        let y0 = x00 + fy * (x10 - x00);
        let y1 = x01 + fy * (x11 - x01);
        let n = y0 + fz * (y1 - y0);
        (shape.height - y as f64) + shape.roughness * n
    }

    /// Whether every block of a column between `y_lo` and `y_hi` (both in
    /// the same noise cell) is solid (`Some(true)`), air (`Some(false)`), or
    /// mixed (`None`).
    ///
    /// Inside one cell, trilinear interpolation is linear in y, and so is
    /// `H − y`, so the density along the column is a straight line: if it
    /// has the same sign at both ends, it has that sign everywhere in
    /// between. The small margin covers floating-point rounding, so the
    /// shortcut never disagrees with evaluating each block. Tall worlds
    /// have bands thousands of blocks thick; this turns them into a few
    /// evaluations per cell.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn span_state(
        &self,
        shape: &Shape,
        c: &[f64; 8],
        fx: f64,
        fz: f64,
        gy: usize,
        y_lo: i32,
        y_hi: i32,
    ) -> Option<bool> {
        const MARGIN: f64 = 1e-6;
        let cell_lo = self.min_y + gy as i32 * self.cell_y;
        let d = |y: i32| Self::density(shape, y, c, fx, (y - cell_lo) as f64 / self.cell_y as f64, fz);
        let (a, b) = (d(y_lo), d(y_hi));
        if a > MARGIN && b > MARGIN {
            Some(true)
        } else if a < -MARGIN && b < -MARGIN {
            Some(false)
        } else {
            None
        }
    }

    /// Badlands stripes: mostly plain terracotta with coloured bands 1–3
    /// blocks thick, shuffled per seed.
    fn make_bands(seed: u64) -> [BlockId; BAND_PERIOD] {
        let mut rng = Xoroshiro128PlusPlus::from_seed(seed);
        let colours = [
            blocks::ORANGE_TERRACOTTA,
            blocks::YELLOW_TERRACOTTA,
            blocks::RED_TERRACOTTA,
            blocks::WHITE_TERRACOTTA,
            blocks::BROWN_TERRACOTTA,
        ];
        let mut bands = [blocks::TERRACOTTA; BAND_PERIOD];
        let mut y = rng.next_bounded(3) as usize;
        while y < BAND_PERIOD {
            let colour = colours[rng.next_bounded(colours.len() as u32) as usize];
            let thick = 1 + rng.next_bounded(3) as usize;
            for b in bands.iter_mut().skip(y).take(thick) {
                *b = colour;
            }
            y += thick + 1 + rng.next_bounded(4) as usize;
        }
        bands
    }

    /// Patch noise in [-1, 1], ~16-block blobs.
    #[inline]
    fn patch_at(&self, x: i32, z: i32) -> f64 {
        (self.patch.sample_2d(x as f64, z as f64) * 2.1).clamp(-1.0, 1.0)
    }

    /// Terracotta stripe at height y. The stripes wobble a few blocks up and
    /// down over ~64 blocks, so they follow the land like real strata.
    fn stripe(&self, x: i32, y: i32, z: i32) -> BlockId {
        let wobble = (self.patch.sample_2d(x as f64 * 0.25, z as f64 * 0.25) * 6.0).round() as i32;
        self.bands[(y + wobble).rem_euclid(BAND_PERIOD as i32) as usize]
    }

    /// Top and filler blocks for a solid block exposed to air/water at
    /// height `y`, in a column of the given biome.
    fn surface_for(&self, x: i32, y: i32, z: i32, shape: &Shape, biome: Biome) -> (BlockId, BlockId) {
        use blocks::*;
        use Biome as B;
        if self.is_flat() {
            return (GRASS, DIRT);
        }
        let patch = self.patch_at(x, z);
        if y < shape.water - 1 {
            // Sea, lake and river beds.
            let depth = shape.water - y;
            if biome == B::MangroveSwamp {
                return (MUD, MUD);
            }
            if shape.island > 0.0 && shape.climate.humidity <= 0.0 && depth < 20 {
                return (BASALT, BASALT); // black volcanic sand around volcanic islands
            }
            let top = if depth < 7 || shape.river > 0.3 || biome == B::WarmOcean {
                SAND
            } else if hash_position(self.surface_seed, x, 0, z).is_multiple_of(8) {
                CLAY
            } else {
                GRAVEL
            };
            return (top, top);
        }
        let coast = shape.climate.continentalness < -0.03;
        if y <= shape.water + 1 && (coast || shape.river > 0.2) {
            // Shoreline and river banks.
            return match biome {
                B::SnowyBeach => (SNOW, SAND),
                B::StonyShore | B::Fjord if patch > 0.0 => (GRAVEL, GRAVEL),
                B::StonyShore | B::Fjord => (STONE, STONE),
                B::MangroveSwamp | B::Swamp => (MUD, MUD),
                B::SaltFlats => (SALT, SAND),
                B::Volcanic => (BASALT, BASALT),
                B::MushroomFields => (MYCELIUM, DIRT),
                B::ShingleBeach if patch > 0.55 => (STONE, STONE),
                B::ShingleBeach => (GRAVEL, GRAVEL),
                B::TropicalBeach => (WHITE_SAND, WHITE_SAND),
                B::TidalFlats if patch < -0.45 => (CLAY, CLAY),
                B::TidalFlats => (MUD, MUD),
                _ => (SAND, SAND),
            };
        }
        // Jitter the snow and tree lines by a few blocks per column. The
        // lines are defined for a default world; `ny` is the height this
        // block would have there, so they move up in tall worlds.
        let jitter = (hash_position(self.surface_seed, x, 2, z) % 9) as i32 - 4;
        let ny = shape.normalized_y(y as f64);
        if ny >= (200 + jitter) as f64 {
            return (SNOW, STONE);
        }
        let (top, filler) = match biome {
            B::IceSheet if patch > 0.45 => (PACKED_ICE, PACKED_ICE),
            B::IceSheet => (SNOW, PACKED_ICE),
            B::FrozenPeaks if patch > 0.25 => (PACKED_ICE, PACKED_ICE),
            B::FrozenPeaks | B::JaggedPeaks | B::SnowySlopes => (SNOW, STONE),
            B::Grove | B::SnowyPlains | B::SnowyTaiga | B::SnowyBeach => (SNOW, DIRT),
            B::StonyPeaks if patch > 0.4 => (GRAVEL, GRAVEL),
            B::StonyPeaks => (STONE, STONE),
            B::Tundra if patch > 0.2 => (COARSE_DIRT, DIRT),
            B::Taiga | B::TemperateRainforest if patch > 0.35 => (PODZOL, DIRT),
            B::OldGrowthTaiga if patch > 0.45 => (COARSE_DIRT, DIRT),
            B::OldGrowthTaiga => (PODZOL, DIRT),
            B::Desert | B::Beach => (SAND, SAND),
            // Marram grass holds the older dunes.
            B::CoastalDunes if patch > 0.5 => (GRASS, SAND),
            B::CoastalDunes => (SAND, SAND),
            B::ShingleBeach if patch > 0.55 => (STONE, STONE),
            B::ShingleBeach if patch < -0.6 => (SAND, SAND),
            B::ShingleBeach => (GRAVEL, GRAVEL),
            B::TropicalBeach => (WHITE_SAND, WHITE_SAND),
            // Salt marsh on the higher flats, mud and clay lower down.
            B::TidalFlats if patch > 0.25 => (GRASS, MUD),
            B::TidalFlats if patch < -0.45 => (CLAY, CLAY),
            B::TidalFlats => (MUD, MUD),
            B::RockyDesert if patch > 0.35 => (GRAVEL, GRAVEL),
            B::RockyDesert if patch < -0.35 => (STONE, STONE),
            B::RockyDesert => (RED_SAND, SANDSTONE),
            B::Bog if patch > 0.3 => (PODZOL, DIRT),
            B::Bog if patch < -0.25 => (MUD, MUD),
            B::Moor if patch > 0.2 => (COARSE_DIRT, DIRT),
            B::Moor if patch < -0.5 => (STONE, STONE),
            B::FloodedJungle if patch < -0.2 => (MUD, MUD),
            B::SaltFlats => (SALT, SAND),
            B::Badlands if patch > -0.2 => (RED_SAND, TERRACOTTA),
            B::Badlands => (self.stripe(x, y, z), TERRACOTTA),
            B::Savanna | B::Chaparral | B::Steppe if patch > 0.45 => (COARSE_DIRT, DIRT),
            B::MixedForest if patch > 0.45 => (PODZOL, DIRT),
            B::ThornScrub if patch > 0.2 => (COARSE_DIRT, DIRT),
            B::ThornScrub if patch < -0.4 => (RED_SAND, RED_SAND),
            B::TropicalDryForest | B::Paramo if patch > 0.5 => (COARSE_DIRT, DIRT),
            B::BaobabSavanna if patch > 0.4 => (COARSE_DIRT, DIRT),
            B::BaobabSavanna if patch < -0.6 => (RED_SAND, RED_SAND),
            B::BambooForest if patch > 0.45 => (PODZOL, DIRT),
            B::RedwoodForest if patch > -0.2 => (PODZOL, DIRT),
            B::PillarKarst if patch > 0.35 => (LIMESTONE, LIMESTONE),
            // Red sand with tussocks of spinifex grass.
            B::Outback if patch < -0.45 => (GRASS, RED_SAND),
            B::Outback if patch > 0.55 => (COARSE_DIRT, RED_SAND),
            B::Outback => (RED_SAND, RED_SAND),
            B::LavenderFields if patch < -0.55 => (COARSE_DIRT, DIRT),
            B::WindsweptHills if patch > 0.3 => (STONE, STONE),
            B::WindsweptHills if patch < -0.5 => (GRAVEL, GRAVEL),
            B::StonyShore | B::Fjord if patch > 0.0 => (GRAVEL, GRAVEL),
            B::StonyShore | B::Fjord => (STONE, STONE),
            B::MushroomFields => (MYCELIUM, DIRT),
            B::Volcanic => (BASALT, BASALT),
            B::Karst if patch > 0.25 => (LIMESTONE, LIMESTONE),
            B::Karst => (GRASS, DIRT),
            B::Swamp if patch < -0.35 => (MUD, MUD),
            B::MangroveSwamp => (MUD, MUD),
            _ => (GRASS, DIRT),
        };
        // Above the tree line, and on crags, soil gives way to bare rock.
        let soil = matches!(top, GRASS | PODZOL | COARSE_DIRT | MYCELIUM | MUD);
        let crag = shape.roughness > 9.0 * shape.scale() && ny > (120 + jitter) as f64;
        if soil && (ny >= (160 + jitter) as f64 || crag) {
            return (STONE, STONE);
        }
        if top == GRASS {
            return (self.flowers_at(x, z, biome).unwrap_or(GRASS), filler);
        }
        (top, filler)
    }

    /// Grass in flower: which flowers (if any) cover a grass column. Each
    /// biome has a density and its own flowers; colours come in patches a
    /// few blocks across, the way wild flowers spread.
    fn flowers_at(&self, x: i32, z: i32, biome: Biome) -> Option<BlockId> {
        use blocks::*;
        use Biome as B;
        const MEADOW: &[BlockId] = &[POPPIES, DANDELIONS, CORNFLOWERS, DAISIES];
        let (density, kinds): (f64, &[BlockId]) = match biome {
            B::FlowerPlains => (0.3, MEADOW),
            B::FlowerForest => (0.2, MEADOW),
            B::Meadow => (0.12, MEADOW),
            B::Plains => (0.02, &[DANDELIONS, DAISIES]),
            B::CherryGrove => (0.3, &[PINK_PETALS]),
            B::LavenderFields => (0.6, &[LAVENDER]),
            B::Moor => (0.3, &[HEATHER]),
            B::Paramo => (0.04, &[DANDELIONS]),
            B::AutumnForest => (0.02, &[DAISIES]),
            _ => return None,
        };
        // Denser in some places than others.
        let n = self.flowers.sample_2d(x as f64, z as f64) * 2.2;
        let local = density * (1.0 + 0.8 * n).clamp(0.2, 1.8);
        let roll = (hash_position(self.surface_seed, x, 5, z) & 0xFFFF) as f64 / 65536.0;
        if roll >= local {
            return None;
        }
        let k = ((n * 0.5 + 0.5).clamp(0.0, 0.999) * kinds.len() as f64) as usize;
        Some(kinds[k])
    }

    /// Block `depth` blocks below an exposed surface whose top/filler are
    /// known (depth ≥ 1).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn below_surface(
        &self,
        x: i32,
        y: i32,
        z: i32,
        depth: i32,
        filler_depth: i32,
        top: BlockId,
        filler: BlockId,
        biome: Biome,
    ) -> BlockId {
        match biome {
            // Badlands: 40 blocks of striped terracotta, so cliffs and
            // canyon walls show the colour bands.
            Biome::Badlands if depth < 40 && !self.is_flat() => return self.stripe(x, y, z),
            Biome::Karst if depth >= filler_depth && depth < 40 => return blocks::LIMESTONE,
            // Pillars are limestone (Guilin) or sandstone (Zhangjiajie) all
            // the way down their walls.
            Biome::PillarKarst if depth >= filler_depth && depth < 800 => return blocks::LIMESTONE,
            Biome::Outback if depth >= filler_depth && depth < 12 => return blocks::RED_SAND,
            Biome::Volcanic if depth < 12 => return blocks::BASALT,
            _ => {}
        }
        if depth < filler_depth {
            filler
        } else if depth == filler_depth && top == blocks::SAND {
            blocks::SANDSTONE // sand is held up by sandstone
        } else {
            blocks::STONE
        }
    }

    /// Whether the sea-level water above this column is frozen. Sea ice
    /// has a few holes (leads) where the patch noise is low.
    fn frozen_surface(&self, x: i32, z: i32, biome: Biome) -> bool {
        biome.freezes() && self.patch_at(x, z) > -0.55
    }

    pub fn sample_column(&self, x: i32, z: i32) -> ColumnSample {
        if !self.settings.size.contains_column(x, z) {
            return ColumnSample { surface_y: self.min_y - 1, ..ColumnSample::OUTSIDE };
        }
        if self.is_flat() {
            let y = self.min_y + 3;
            return ColumnSample {
                surface_y: y,
                top_block: blocks::GRASS,
                side_block: blocks::DIRT,
                underwater: false,
                inside: true,
                shape: ColumnSample::OUTSIDE.shape,
                biome: Biome::Plains,
                tint: Tint::of(Biome::Plains, &ColumnSample::OUTSIDE.shape, y),
                ice: false,
            };
        }
        let shape = self.column_shape(x, z);
        let band = Self::band(&shape);
        let (gx, gz) = (x.div_euclid(CELL_XZ), z.div_euclid(CELL_XZ));
        let fx = x.rem_euclid(CELL_XZ) as f64 / CELL_XZ as f64;
        let fz = z.rem_euclid(CELL_XZ) as f64 / CELL_XZ as f64;
        // A column on the 4-block grid line (fx or fz = 0) gives the far
        // corners zero weight, so they needn't be computed: copying the
        // near ones gives bit-identical densities (c + 0·(c' − c) = c).
        // Every LOD cell of 8 blocks or more is sampled on the grid, so
        // this makes far-view tiles about 3× cheaper.
        let (need_x, need_z) = (fx != 0.0, fz != 0.0);
        let level = |gy: usize| -> [f64; 4] {
            let c00 = self.corner(gx, gy, gz);
            let c10 = if need_x { self.corner(gx + 1, gy, gz) } else { c00 };
            let c01 = if need_z { self.corner(gx, gy, gz + 1) } else { c00 };
            let c11 = match (need_x, need_z) {
                (true, true) => self.corner(gx + 1, gy, gz + 1),
                (true, false) => c10,
                _ => c01,
            };
            [c00, c10, c01, c11]
        };

        // Scan down cell by cell; inside a cell, skip straight past spans
        // that are certainly air (see `span_state`).
        let mut surface = band.bottom - 1; // everything below the band is solid
        let mut y = band.top;
        // The bottom corners of one cell are the top corners of the next
        // one down: keep them instead of computing them twice.
        let mut below: Option<(usize, [f64; 4])> = None;
        'scan: while y >= band.bottom {
            let ly = y - self.min_y;
            let gy = (ly / self.cell_y) as usize;
            let cell_lo = (self.min_y + gy as i32 * self.cell_y).max(band.bottom);
            let hi = match below {
                Some((g, c)) if g == gy + 1 => c,
                _ => level(gy + 1),
            };
            let lo = level(gy);
            below = Some((gy, lo));
            let c = [lo[0], lo[1], hi[0], hi[1], lo[2], lo[3], hi[2], hi[3]];
            match self.span_state(&shape, &c, fx, fz, gy, cell_lo, y) {
                Some(false) => {}
                Some(true) => {
                    surface = y;
                    break 'scan;
                }
                None => {
                    for yy in (cell_lo..=y).rev() {
                        let fy = ((yy - self.min_y) % self.cell_y) as f64 / self.cell_y as f64;
                        if Self::solid(&shape, yy, &c, fx, fy, fz) {
                            surface = yy;
                            break 'scan;
                        }
                    }
                }
            }
            y = cell_lo - 1;
        }
        let biome = self.biome_at(&shape, surface, x, z);
        let underwater = surface < shape.water;
        let (top, filler) = self.surface_for(x, surface, z, &shape, biome);
        let filler_depth = 3 + (hash_position(self.surface_seed, x, 1, z) % 3) as i32;
        ColumnSample {
            surface_y: surface,
            top_block: top,
            side_block: self.below_surface(x, surface - 8, z, 8, filler_depth, top, filler, biome),
            underwater,
            inside: true,
            shape,
            biome,
            tint: Tint::of(biome, &shape, surface),
            ice: underwater && self.frozen_surface(x, z, biome),
        }
    }

    /// Generates one full-height chunk.
    pub fn generate_chunk(&self, pos: ChunkPos) -> Chunk {
        let height = self.settings.height;
        let (min_y, max_y) = (self.min_y, self.max_y);
        if !self.chunk_touches_world(pos) {
            return Chunk::empty(pos, height);
        }
        let (bx0, bz0) = (pos.min_block_x(), pos.min_block_z());

        // Per-column shape and noise band.
        let mut shapes = [Shape::default(); 256];
        let mut bands = [Band { bottom: 0, top: 0 }; 256];
        let mut inside = [false; 256];
        let (mut lo_y, mut hi_y) = (i32::MAX, i32::MIN);
        // Highest water surface in the chunk (rivers and lakes can be above
        // the sea).
        let mut water_top = SEA_LEVEL;
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
                water_top = water_top.max(shapes[i].water);
                lo_y = lo_y.min(bands[i].bottom);
                hi_y = hi_y.max(bands[i].top);
            }
        }

        // Which sections need to be generated block by block (the
        // "window"). Everything more than SUBSURFACE blocks below the lowest
        // possible surface is plain stone, and everything above the highest
        // terrain, the sea and the trees is air; those sections are never
        // touched. In a 17,696-block world this skips ~90% of the column.
        // Chunks cut by the world border are generated in full, because
        // their outside columns must be air all the way down.
        let section_of = |y: i32| ((y - min_y) / SECTION_SIZE as i32) as usize;
        let all_inside = inside.iter().all(|&b| b);
        let (s_lo, s_hi) = if self.is_flat() {
            (0, 0)
        } else {
            let deepest = lo_y - SUBSURFACE;
            let top = (hi_y.max(water_top) + if self.settings.features { trees::TREE_HEADROOM } else { 0 }).min(max_y);
            let s_lo = if all_inside && deepest >= min_y { section_of(deepest) } else { 0 };
            (s_lo, section_of(top).max(s_lo))
        };
        // The column loop stops here; below it (when s_lo > 0) only the
        // floor section is left, filled separately with stone and bedrock.
        let stop_y = min_y + (s_lo * SECTION_SIZE) as i32;

        // Density-noise corners for this chunk: 5 × grid_y × 5, filled only
        // for the vertical range some column actually needs.
        let cell_y = self.cell_y;
        let mut grid = vec![0.0f64; 5 * self.grid_y * 5];
        let gidx = |i: usize, gy: usize, k: usize| (gy * 5 + k) * 5 + i;
        if lo_y <= hi_y {
            let gy_lo = ((lo_y.max(stop_y) - min_y) / cell_y) as usize;
            let gy_hi = (((hi_y - min_y) / cell_y) as usize + 1).min(self.grid_y - 1);
            let (gx0, gz0) = (bx0.div_euclid(CELL_XZ), bz0.div_euclid(CELL_XZ));
            for gy in gy_lo..=gy_hi {
                for k in 0..5 {
                    for i in 0..5 {
                        grid[gidx(i, gy, k)] = self.corner(gx0 + i as i32, gy, gz0 + k as i32);
                    }
                }
            }
        }

        // Highest solid block and its top block per column, for trees.
        let mut surface = [min_y - 1; 256];
        let mut tops = [blocks::AIR; 256];
        let mut heightmap = [(min_y - 1) as i16; 256];
        let mut biomes = [Biome::default(); 256];
        let mut tints = [Tint::default(); 256];

        // Fill dense [section][16^3] buffers first: sequential writes into
        // plain arrays are much faster than palette writes block by block.
        let mut floor = [blocks::AIR; SECTION_VOLUME];
        let mut dense = vec![[blocks::AIR; SECTION_VOLUME]; s_hi - s_lo + 1];
        let mut put = |lx: usize, y: i32, lz: usize, b: BlockId| {
            let ly = (y - min_y) as usize;
            let si = ly / SECTION_SIZE;
            let idx = section_index(lx, ly % SECTION_SIZE, lz);
            if si == 0 && s_lo > 0 {
                floor[idx] = b;
            } else if si >= s_lo && si <= s_hi {
                dense[si - s_lo][idx] = b;
            }
        };

        for lz in 0..CHUNK_SIZE as usize {
            for lx in 0..CHUNK_SIZE as usize {
                let i = lx + lz * 16;
                if !inside[i] {
                    continue;
                }
                let (x, z) = (bx0 + lx as i32, bz0 + lz as i32);

                if self.is_flat() {
                    put(lx, min_y, lz, blocks::BEDROCK);
                    put(lx, min_y + 1, lz, blocks::DIRT);
                    put(lx, min_y + 2, lz, blocks::DIRT);
                    put(lx, min_y + 3, lz, blocks::GRASS);
                    heightmap[i] = (min_y + 3) as i16;
                    surface[i] = min_y + 3;
                    tops[i] = blocks::GRASS;
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

                let mut highest = min_y - 1;
                // Depth below the most recent exposed surface, and the
                // block types chosen for that surface.
                let mut depth = i32::MAX;
                let (mut top, mut filler) = (blocks::STONE, blocks::STONE);
                let mut solid_above = false;
                // The biome is decided at the first (highest) surface and
                // shared by any lower surfaces under overhangs.
                let mut col_biome: Option<Biome> = None;
                // Current noise cell, its corners and whether the band part
                // of it is all solid, all air or mixed (see `span_state`).
                let mut cell: Option<(usize, [f64; 8], Option<bool>)> = None;
                for y in (stop_y..=band.top.max(shape.water)).rev() {
                    let solid = if y > band.top {
                        false
                    } else if y < band.bottom {
                        true
                    } else {
                        let ly = y - min_y;
                        let gy = (ly / cell_y) as usize;
                        if cell.is_none_or(|(g, _, _)| g != gy) {
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
                            let cell_lo = (min_y + gy as i32 * cell_y).max(band.bottom);
                            cell = Some((gy, c, self.span_state(shape, &c, fx, fz, gy, cell_lo, y)));
                        }
                        let (_, c, state) = cell.as_ref().expect("set above");
                        match state {
                            Some(b) => *b,
                            None => Self::solid(shape, y, c, fx, (ly % cell_y) as f64 / cell_y as f64, fz),
                        }
                    };

                    if !solid {
                        if y <= shape.water {
                            put(lx, y, lz, blocks::WATER);
                            highest = highest.max(y);
                        }
                        solid_above = false;
                        continue;
                    }
                    let biome = *col_biome.get_or_insert_with(|| {
                        surface[i] = y;
                        self.biome_at(shape, y, x, z)
                    });
                    if !solid_above {
                        // A newly exposed surface (the top, or under an overhang).
                        depth = 0;
                        (top, filler) = self.surface_for(x, y, z, shape, biome);
                        if surface[i] == y {
                            tops[i] = top;
                        }
                    }
                    solid_above = true;
                    highest = highest.max(y);

                    let b = if y == min_y || (y < min_y + 5 && self.is_bedrock(x, y, z)) {
                        blocks::BEDROCK
                    } else if depth == 0 {
                        top
                    } else {
                        self.below_surface(x, y, z, depth, filler_depth, top, filler, biome)
                    };
                    put(lx, y, lz, b);
                    depth = depth.saturating_add(1);
                }
                if s_lo > 0 {
                    // The floor section, far below anything interesting.
                    for y in min_y..min_y + SECTION_SIZE as i32 {
                        let b = if y == min_y || (y < min_y + 5 && self.is_bedrock(x, y, z)) {
                            blocks::BEDROCK
                        } else {
                            blocks::STONE
                        };
                        put(lx, y, lz, b);
                    }
                }
                let biome = col_biome.unwrap_or_default();
                biomes[i] = biome;
                tints[i] = Tint::of(biome, &shapes[i], surface[i]);
                let water = shapes[i].water;
                if surface[i] < water && self.frozen_surface(x, z, biome) {
                    put(lx, water, lz, blocks::ICE);
                }
                heightmap[i] = highest as i16;
            }
        }

        if self.settings.features {
            let local = |lx: usize, lz: usize| (surface[lx + lz * 16], tops[lx + lz * 16], shapes[lx + lz * 16]);
            for tree in self.trees_for_chunk(pos, Some(&local)) {
                trees::build(&tree, &mut |wx, wy, wz, b, is_log| {
                    let (lx, lz) = (wx - bx0, wz - bz0);
                    if !(0..16).contains(&lx) || !(0..16).contains(&lz) || !(min_y..=max_y).contains(&wy) {
                        return;
                    }
                    if !self.settings.size.contains_column(wx, wz) {
                        return;
                    }
                    let (lx, lz) = (lx as usize, lz as usize);
                    let si = section_of(wy);
                    if si < s_lo || si > s_hi {
                        return; // can't happen: the window includes TREE_HEADROOM
                    }
                    let ly = (wy - min_y) as usize;
                    let cell = &mut dense[si - s_lo][section_index(lx, ly % SECTION_SIZE, lz)];
                    // Logs grow through air, water and leaves; leaves only
                    // fill air. See `trees` for why this makes overlaps
                    // independent of chunk order.
                    let free = cell.is_air() || (is_log && (*cell == blocks::WATER || cell.is_leaves()));
                    if free {
                        *cell = b;
                        let h = &mut heightmap[lx + lz * 16];
                        *h = (*h).max(wy as i16);
                    }
                });
            }
        }

        let mut sections = dense.iter().map(PalettedSection::from_dense);
        let (floor, window_start) = if s_lo == 0 {
            (sections.next().expect("window has at least one section"), 1)
        } else {
            (PalettedSection::from_dense(&floor), s_lo)
        };
        let mut chunk = Chunk::assemble(pos, height, floor, window_start, sections.collect());
        chunk.heightmap = heightmap;
        chunk.biomes = biomes;
        chunk.tints = tints;
        chunk
    }

    /// The biome of a column (with the dither that makes borders into
    /// transition zones).
    #[inline]
    fn biome_at(&self, shape: &Shape, surface_y: i32, x: i32, z: i32) -> Biome {
        biome::select_with(shape, surface_y, &self.dither.ground(x, z))
    }

    /// Decides whether the tree candidate at (x, z) grows and what it is.
    /// The species comes from the biome under the tree's own dither (see
    /// `biome`), so trees of neighbouring biomes mix across their border.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_tree(
        &self,
        x: i32,
        z: i32,
        hash: u64,
        surface_y: i32,
        top: BlockId,
        shape: &Shape,
    ) -> Option<Tree> {
        let biome = if self.is_flat() {
            Biome::Plains
        } else {
            biome::select_with(shape, surface_y, &self.dither.tree(x, z, hash))
        };
        trees::resolve(x, z, hash, biome, surface_y, top, self.max_y)
    }

    /// All trees that put at least one block into chunk `pos`, in global
    /// cell order. `local` returns (surface y, top block, shape) for the
    /// chunk's own columns when they are already known; everything else
    /// comes from [`Self::sample_column`], which matches chunk generation
    /// exactly.
    #[allow(clippy::type_complexity)]
    pub fn trees_for_chunk(
        &self,
        pos: ChunkPos,
        local: Option<&dyn Fn(usize, usize) -> (i32, BlockId, Shape)>,
    ) -> Vec<Tree> {
        let mut out = Vec::new();
        if !self.settings.features || !self.chunk_touches_world(pos) {
            return out;
        }
        let (bx0, bz0) = (pos.min_block_x(), pos.min_block_z());
        let (x_lo, x_hi) = (bx0 - TREE_REACH, bx0 + 15 + TREE_REACH);
        let (z_lo, z_hi) = (bz0 - TREE_REACH, bz0 + 15 + TREE_REACH);
        for cz in z_lo.div_euclid(TREE_CELL)..=z_hi.div_euclid(TREE_CELL) {
            for cx in x_lo.div_euclid(TREE_CELL)..=x_hi.div_euclid(TREE_CELL) {
                let (x, z, hash) = trees::cell_candidate(self.tree_seed, cx, cz);
                if x < x_lo || x > x_hi || z < z_lo || z > z_hi || !self.settings.size.contains_column(x, z) {
                    continue;
                }
                let (lx, lz) = (x - bx0, z - bz0);
                let (surface_y, top, shape) = match local {
                    Some(f) if (0..16).contains(&lx) && (0..16).contains(&lz) => f(lx as usize, lz as usize),
                    _ => {
                        let s = self.sample_column(x, z);
                        (s.surface_y, s.top_block, s.shape)
                    }
                };
                if let Some(t) = self.resolve_tree(x, z, hash, surface_y, top, &shape) {
                    out.push(t);
                }
            }
        }
        out
    }

    /// Bedrock floor: always at the lowest layer, then a ragged layer that
    /// thins out over the next 4 blocks.
    fn is_bedrock(&self, x: i32, y: i32, z: i32) -> bool {
        let layer = (y - self.min_y) as f64; // 1..=4
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
        // Continents are `scale` times wider in tall worlds; search as far.
        let half = self.settings.size.half_width().min(self.settings.size.half_depth());
        let mut max_radius = (half - 8).clamp(0, (6000.0 * self.vscale) as i32);
        let mut step = (16.0 * self.vscale) as i32;
        let (cx, cz) = match self.realistic() {
            Some(r) => {
                // Start on the land the planet simulation picked, and
                // search about one grid cell around it.
                max_radius = (r.cell_blocks() * 2.0) as i32;
                step = ((r.cell_blocks() / 24.0) as i32).clamp(4, 4096);
                r.spawn_hint()
            }
            None => (0, 0),
        };
        let mut r = 0;
        while r <= max_radius {
            let n = if r == 0 { 1 } else { (r * 8) / step };
            for i in 0..n {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                let (x, z) = (cx + (a.cos() * r as f64) as i32, cz + (a.sin() * r as f64) as i32);
                let c = self.sample_column(x, z);
                if c.inside && !c.underwater && c.shape.normalized_y(c.surface_y as f64) < 140.0 && c.shape.river < 0.1
                {
                    return (x, c.surface_y + 1, z);
                }
            }
            r += step;
        }
        let c = self.sample_column(cx, cz);
        (cx, c.surface_y.max(SEA_LEVEL) + 1, cz)
    }
}

const _: () = assert!(SECTION_SIZE.is_multiple_of(CELL_Y as usize));
const _: () = assert!(CHUNK_SIZE % CELL_XZ == 0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Generator, Planet, SimQuality, WorldHeight, WorldSize, MAX_Y, MIN_Y};

    fn generator(seed: u64, world_type: WorldType) -> WorldGenerator {
        WorldGenerator::new(WorldSettings { seed, world_type, size: WorldSize::from_km(64.0), ..Default::default() })
    }

    fn chunks_equal(a: &Chunk, b: &Chunk) -> bool {
        a == b
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

    /// The map tool, menu preview, HUD and tree placement rely on
    /// `sample_column`; it must describe the generated chunk exactly,
    /// including in rugged mountains where 3D noise creates overhangs.
    /// Without features nothing may stand above the surface but water and
    /// sea ice; with features, also tree parts.
    #[test]
    fn column_sample_matches_generated_chunk() {
        let default = WorldHeight::default();
        let mut cases = Vec::new();
        for features in [false, true] {
            for t in
                [WorldType::Normal, WorldType::Amplified, WorldType::Islands, WorldType::LargeBiomes, WorldType::Flat]
            {
                cases.push((t, features, default));
            }
        }
        // Tall and short worlds, where most sections are implied, not stored.
        for h in [256, 1000, 5000, 17_696] {
            cases.push((WorldType::Normal, true, WorldHeight::from_blocks(h)));
        }
        cases.push((WorldType::Amplified, true, WorldHeight::from_blocks(17_696)));
        for (t, features, height) in cases {
            let g = WorldGenerator::new(WorldSettings {
                seed: 77,
                world_type: t,
                size: WorldSize::from_km(64.0),
                features,
                height,
                ..Default::default()
            });
            for pos in [ChunkPos::new(-3, 12), ChunkPos::new(40, -7), ChunkPos::new(-90, -60), ChunkPos::new(150, 151)]
            {
                let ctx = format!("{t:?} features={features} height={}", height.blocks());
                assert_chunk_matches_samples(&g, pos, &ctx);
            }
        }
    }

    /// Checks one chunk against `sample_column` for each of its columns.
    fn assert_chunk_matches_samples(g: &WorldGenerator, pos: ChunkPos, ctx: &str) {
        let (min_y, max_y) = (g.min_y(), g.max_y());
        let features = g.settings().features;
        let c = g.generate_chunk(pos);
        for lz in 0..16 {
            for lx in 0..16 {
                let s = g.sample_column(pos.min_block_x() + lx as i32, pos.min_block_z() + lz as i32);
                let ctx = format!("{ctx} {pos:?} {lx},{lz}");
                assert_eq!(c.get(lx, s.surface_y, lz), s.top_block, "{ctx}");
                assert_eq!(c.biome_at(lx, lz), s.biome, "{ctx}");
                if s.underwater {
                    assert_eq!(c.get(lx, s.shape.water, lz) == blocks::ICE, s.ice, "{ctx}");
                } else {
                    assert!(!s.ice, "{ctx}");
                }
                for y in s.surface_y + 1..=max_y {
                    let b = c.get(lx, y, lz);
                    let ok =
                        b == blocks::AIR || b == blocks::WATER || b == blocks::ICE || (features && b.is_tree_part());
                    assert!(ok, "{ctx}: {} above surface at y={y}", b.info().name);
                    // Water exactly fills the column up to its water level.
                    if !s.underwater {
                        assert!(b != blocks::WATER && b != blocks::ICE, "{ctx}: water on dry land at y={y}");
                    } else if y < s.shape.water {
                        assert_eq!(b, blocks::WATER, "{ctx}: y={y} below water level {}", s.shape.water);
                    } else if y == s.shape.water {
                        assert!(b == blocks::WATER || b == blocks::ICE, "{ctx}: no water at the water level");
                    }
                }
                assert_eq!(c.get(lx, min_y, lz), blocks::BEDROCK, "{ctx}");
                // Deep underground is stone (whether stored or implied).
                if g.settings().world_type != WorldType::Flat || g.realistic().is_some() {
                    assert_eq!(c.get(lx, min_y + 8, lz), blocks::STONE, "{ctx}");
                }
                let water_top = s.surface_y.max(if s.underwater { s.shape.water } else { min_y });
                if features {
                    assert!(c.height_at(lx, lz) >= water_top, "{ctx}");
                } else {
                    assert_eq!(c.height_at(lx, lz), water_top, "{ctx}");
                }
            }
        }
    }

    fn realistic(planet: Planet, km: f64, height: u32) -> WorldGenerator {
        WorldGenerator::new(WorldSettings {
            seed: 12,
            generator: Generator::Realistic,
            planet,
            quality: SimQuality::Fast,
            size: WorldSize::from_km(km),
            height: WorldHeight::from_blocks(height),
            ..Default::default()
        })
    }

    /// Finds columns of a realistic world on a coarse grid: (river above
    /// sea level, lake, high mountain, sea coast).
    fn realistic_landmarks(g: &WorldGenerator) -> Vec<(&'static str, i32, i32)> {
        let half = g.settings().size.half_width().min(g.settings().size.half_depth()) - 64;
        let n = 400;
        let mut found: Vec<(&'static str, i32, i32)> = Vec::new();
        let mut want = |name: &'static str, x: i32, z: i32| {
            if !found.iter().any(|f| f.0 == name) {
                found.push((name, x, z));
            }
        };
        for j in 0..n {
            for i in 0..n {
                let x = -half + (2 * half as i64 * i / n) as i32;
                let z = -half + (2 * half as i64 * j / n) as i32;
                let s = g.sample_column(x, z);
                if s.shape.river >= 1.0 && s.shape.water > SEA_LEVEL + 20 {
                    want("river", x, z);
                }
                if s.underwater && s.shape.water > SEA_LEVEL + 20 && s.shape.river < 1.0 {
                    want("lake", x, z);
                }
                if s.shape.normalized_y(s.surface_y as f64) > 200.0 {
                    want("mountain", x, z);
                }
                if s.underwater && s.shape.water == SEA_LEVEL && s.surface_y > SEA_LEVEL - 4 {
                    want("coast", x, z);
                }
            }
        }
        found
    }

    /// Realistic worlds: rivers and lakes above the sea, mountains and
    /// coasts all generate exactly as `sample_column` describes them.
    #[test]
    fn realistic_column_sample_matches_generated_chunk() {
        for (planet, km, height) in [(Planet::Continents, 4000.0, 2048), (Planet::Earth, 8000.0, 4096)] {
            let g = realistic(planet, km, height);
            let marks = realistic_landmarks(&g);
            let names: Vec<_> = marks.iter().map(|m| m.0).collect();
            for want in ["river", "mountain", "coast"] {
                assert!(names.contains(&want), "{planet:?}: no {want} found ({names:?})");
            }
            let (sx, _, sz) = g.find_spawn();
            for (name, x, z) in marks.into_iter().chain([("spawn", sx, sz)]) {
                // The landmark and its neighbour, in case it sits on an edge.
                for (dx, dz) in [(0, 0), (16, 16)] {
                    let pos = ChunkPos::from_block(x + dx, z + dz);
                    assert_chunk_matches_samples(&g, pos, &format!("{planet:?} {name}"));
                }
            }
        }
    }

    /// Same settings, same planet; another seed, another planet.
    #[test]
    fn realistic_worlds_are_deterministic() {
        let a = realistic(Planet::Islands, 2000.0, 1024);
        let b = realistic(Planet::Islands, 2000.0, 1024);
        let pos = ChunkPos::from_block(a.find_spawn().0, a.find_spawn().2);
        assert!(chunks_equal(&a.generate_chunk(pos), &b.generate_chunk(pos)));
        let probe = |g: &WorldGenerator| -> Vec<i32> {
            (0..64).map(|k| g.sample_column(k * 15_000 - 480_000, k * 7_000 - 220_000).surface_y).collect()
        };
        assert_eq!(probe(&a), probe(&b));
        let c = WorldGenerator::new(WorldSettings { seed: 13, ..a.settings().clone() });
        assert_ne!(probe(&a), probe(&c));
    }

    /// Each planet has its intended share of land, and spawn is on land.
    #[test]
    fn realistic_planets_have_their_land_share() {
        for (planet, lo, hi) in [
            (Planet::Continents, 0.25, 0.45),
            (Planet::Pangea, 0.22, 0.42),
            (Planet::Islands, 0.04, 0.2),
            (Planet::Earth, 0.25, 0.35),
        ] {
            let g = realistic(planet, 10_000.0, 2048);
            let (hw, hd) = (g.settings().size.half_width(), g.settings().size.half_depth());
            let n = 120;
            let mut land = 0;
            for j in 0..n {
                for i in 0..n {
                    let x = -hw + (2 * hw as i64 * i / n + hw as i64 / n) as i32;
                    let z = -hd + (2 * hd as i64 * j / n + hd as i64 / n) as i32;
                    land += !g.sample_column(x, z).underwater as i32;
                }
            }
            let share = land as f64 / (n * n) as f64;
            assert!((lo..hi).contains(&share), "{planet:?}: land share {share}");
            let (x, y, z) = g.find_spawn();
            let s = g.sample_column(x, z);
            assert!(!s.underwater && y > SEA_LEVEL, "{planet:?}: spawn at {x},{y},{z} is in water");
        }
    }

    /// A tall world is the default world magnified: same land/ocean
    /// pattern, heights stretched around sea level, and chunks that store
    /// only the sections around the surface.
    #[test]
    fn tall_worlds_scale_the_terrain() {
        let tall = WorldHeight::from_blocks(17_696);
        let s = tall.terrain_scale();
        let g1 = generator(5, WorldType::Normal);
        let gt =
            WorldGenerator::new(WorldSettings { seed: 5, height: tall, size: WorldSize::full(), ..Default::default() });
        let (mut same_water, mut n, mut max_t) = (0, 0, MIN_Y);
        for iz in 0..40 {
            for ix in 0..40 {
                let (x, z) = (ix * 400 - 8000, iz * 400 - 8000);
                let a = g1.sample_column(x, z);
                let b = gt.sample_column((x as f64 * s) as i32, (z as f64 * s) as i32);
                n += 1;
                same_water += (a.underwater == b.underwater) as i32;
                max_t = max_t.max(b.surface_y);
                // Base heights match after un-scaling (the 3D noise differs a
                // little because its sample grid lands on other points).
                let unscaled = SEA_LEVEL as f64 + (b.shape.height - SEA_LEVEL as f64) / s;
                assert!((unscaled - a.shape.height).abs() < 3.0, "{x},{z}: {} vs {}", a.shape.height, unscaled);
            }
        }
        assert!(same_water as f64 / n as f64 > 0.9, "{same_water}/{n}");
        assert!(max_t > SEA_LEVEL + 2000, "tall world peaks only reach {max_t}");
        assert!(max_t < tall.max_y());

        // Storage: a tall chunk is a few hundred KB at most, not 1,106 sections.
        let (x, y, z) = gt.find_spawn();
        let c = gt.generate_chunk(ChunkPos::from_block(x, z));
        assert!(c.window().len() < 120, "window {:?}", c.window());
        assert!(c.memory_bytes() < 400_000, "{} bytes", c.memory_bytes());
        assert!(!gt.sample_column(x, z).underwater && y > SEA_LEVEL);
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
        let g = WorldGenerator::new(WorldSettings {
            seed,
            world_type: t,
            size: WorldSize::from_km(64.0),
            ..Default::default()
        });
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

    /// Biomes must agree with the land under them, and every biome must
    /// actually occur somewhere. Sampled over 48 × 48 km on two seeds.
    #[test]
    fn biomes_fit_their_terrain_and_all_occur() {
        let mut seen = vec![false; Biome::ALL.len()];
        for seed in [42, 7] {
            let g = generator(seed, WorldType::Normal);
            for iz in 0..240 {
                for ix in 0..240 {
                    let s = g.sample_column(ix * 200 - 24_000, iz * 200 - 24_000);
                    let b = s.biome;
                    seen[b as usize] = true;
                    let ctx = format!(
                        "seed {seed} at {},{}: {} y={}",
                        ix * 200 - 24_000,
                        iz * 200 - 24_000,
                        b.name(),
                        s.surface_y
                    );
                    // Water biomes exactly where the ground is under water
                    // (mangroves may stand in either).
                    if b != Biome::MangroveSwamp {
                        assert_eq!(b.is_water(), s.underwater, "{ctx}");
                    }
                    if matches!(b, Biome::FrozenPeaks | Biome::JaggedPeaks | Biome::StonyPeaks) {
                        assert!(s.surface_y > SEA_LEVEL + 85 - biome::DITHER_ALT as i32 - 1, "{ctx}");
                    }
                    if matches!(b, Biome::Beach | Biome::SnowyBeach) {
                        assert!(s.surface_y <= SEA_LEVEL + 3, "{ctx}");
                    }
                    // Climate bands hold, give or take the transition zones
                    // (`biome::DITHER_T` and the slant of the climate-space
                    // borders).
                    let t = biome::effective_temperature(&s.shape, s.surface_y);
                    if matches!(b, Biome::Desert | Biome::Badlands | Biome::Jungle | Biome::Savanna) {
                        assert!(t > 0.18 - 0.1, "hot biome in the cold: {ctx} t={t}");
                    }
                    if matches!(b, Biome::SnowyPlains | Biome::Tundra | Biome::SnowyTaiga | Biome::FrozenOcean) {
                        assert!(t < -0.45 + 0.1, "frozen biome in the warm: {ctx} t={t}");
                    }
                    if matches!(b, Biome::River | Biome::FrozenRiver) {
                        assert!(s.shape.river > 0.3, "{ctx}");
                    }
                    if matches!(b, Biome::Volcanic | Biome::MushroomFields) {
                        assert!(s.shape.island > 0.0, "{ctx}");
                    }
                    if s.ice {
                        assert!(b.freezes() && s.underwater, "{ctx}");
                    }
                }
            }
        }
        let missing: Vec<_> = Biome::ALL.iter().filter(|b| !seen[**b as usize]).map(|b| b.name()).collect();
        assert!(missing.is_empty(), "biomes never generated: {missing:?}");
    }

    /// Grass colour follows the climate, not the biome: where the biome
    /// changes between neighbouring grass columns, the colour barely does
    /// (it used to jump to the other biome's colour).
    #[test]
    fn grass_colour_blends_across_biome_borders() {
        let g = generator(42, WorldType::Normal);
        let diff = |a: [u8; 3], b: [u8; 3]| a.iter().zip(&b).map(|(&x, &y)| (x as i32 - y as i32).abs()).max().unwrap();
        let (mut borders, mut tint_jump, mut biome_jump) = (0, 0i64, 0i64);
        for row in 0..40 {
            let z = row * 600 - 12_000;
            let mut prev = g.sample_column(-12_000, z);
            for x in -11_999..12_000 {
                let s = g.sample_column(x, z);
                if s.biome != prev.biome && s.top_block == blocks::GRASS && prev.top_block == blocks::GRASS {
                    borders += 1;
                    tint_jump += diff(s.tint.color(), prev.tint.color()) as i64;
                    biome_jump += diff(s.biome.grass_color(), prev.biome.grass_color()) as i64;
                }
                prev = s;
            }
        }
        assert!(borders > 50, "only {borders} grass borders crossed");
        let (t, b) = (tint_jump as f64 / borders as f64, biome_jump as f64 / borders as f64);
        assert!(t < 8.0 && t < b / 3.0, "mean colour jump at borders: {t:.1} (by biome it would be {b:.1})");
    }

    /// Trees are generated per chunk, but a canopy may reach into the next
    /// chunk. Every block of every tree must appear in whichever chunk holds
    /// it, unless that spot was already taken by terrain or another tree.
    /// An AIR block where a tree should be means a seam.
    #[test]
    fn trees_are_seamless_across_chunk_borders() {
        let g = generator(42, WorldType::Normal);
        // A forested area found from the biome map of seed 42.
        let mut chunks = std::collections::HashMap::new();
        let mut found = 0;
        for (ox, oz) in [(0, 0), (-150, 200), (300, -120), (-40, -260)] {
            for cz in oz..oz + 4 {
                for cx in ox..ox + 4 {
                    chunks.insert(ChunkPos::new(cx, cz), g.generate_chunk(ChunkPos::new(cx, cz)));
                }
            }
        }
        let mut crossing = 0;
        for &pos in chunks.keys() {
            for t in g.trees_for_chunk(pos, None) {
                if ChunkPos::from_block(t.x, t.z) != pos {
                    continue; // check each tree once, from its home chunk
                }
                found += 1;
                let mut crosses = false;
                trees::build(&t, &mut |x, y, z, _b, _| {
                    let p = ChunkPos::from_block(x, z);
                    let Some(c) = chunks.get(&p) else { return };
                    crosses |= p != pos;
                    let got = c.get((x - p.min_block_x()) as usize, y, (z - p.min_block_z()) as usize);
                    assert!(!got.is_air(), "tree {t:?}: missing block at {x},{y},{z} in chunk {p:?}");
                });
                crossing += crosses as i32;
            }
        }
        assert!(found > 50, "only {found} trees in the test area");
        assert!(crossing > 5, "only {crossing} trees cross a chunk border");
    }

    #[test]
    fn forests_have_trees_and_deserts_have_few() {
        let g = generator(42, WorldType::Normal);
        let (mut forest, mut forest_trees, mut desert, mut desert_trees) = (0, 0, 0, 0);
        for cz in -60..60 {
            for cx in -60..60 {
                let pos = ChunkPos::new(cx * 3, cz * 3);
                let s = g.sample_column(pos.min_block_x() + 8, pos.min_block_z() + 8);
                let n = g
                    .trees_for_chunk(pos, None)
                    .iter()
                    .filter(|t| ChunkPos::from_block(t.x, t.z) == pos && !matches!(t.kind, trees::TreeKind::Bush))
                    .count();
                match s.biome {
                    Biome::Forest | Biome::BirchForest | Biome::DarkForest | Biome::Taiga | Biome::Jungle => {
                        forest += 1;
                        forest_trees += n;
                    }
                    Biome::Desert => {
                        desert += 1;
                        desert_trees += n;
                    }
                    _ => {}
                }
            }
        }
        // 256 columns per chunk / 25 per cell ≈ 10 candidates per chunk.
        let (f, d) = (forest_trees as f64 / forest as f64, desert_trees as f64 / desert.max(1) as f64);
        assert!(forest > 20 && f > 3.0, "forest chunks {forest}, trees/chunk {f}");
        assert!(d < 1.0, "desert trees/chunk {d}");
    }

    #[test]
    fn no_trees_without_features() {
        let g = WorldGenerator::new(WorldSettings { seed: 42, features: false, ..Default::default() });
        for cz in -5..5 {
            for cx in -5..5 {
                let c = g.generate_chunk(ChunkPos::new(cx, cz));
                for si in 0..c.section_count() {
                    assert!(c.section(si).palette().iter().all(|b| !b.is_tree_part()));
                }
            }
        }
    }
}
