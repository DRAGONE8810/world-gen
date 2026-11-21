//! Chunks: the unit of generation, storage and meshing.
//!
//! A chunk is a 16×16 column of blocks spanning the full world height,
//! split vertically into 16×16×16 sections. Each section is
//! palette-compressed on its own, so all-air sky and all-stone underground
//! cost almost nothing.
//!
//! ## Tall worlds
//!
//! A 17,696-block world has 1,106 sections per column, but almost all of
//! them are solid stone deep down or empty air high up. Storing all of
//! them, even compressed, would cost ~60 KB per chunk just in bookkeeping.
//! So a chunk stores only:
//!
//! * the **floor** section (bedrock) at the bottom,
//! * a contiguous **window** of sections around the surface,
//! * and the rule that everything between the floor and the window is
//!   stone and everything above the window is air.
//!
//! Like cubic-chunk mods, this only spends memory where the terrain is
//! interesting, but it keeps the simple "one chunk = one column" model
//! for generation and streaming.

use crate::biome::Biome;
use crate::block::{blocks, BlockId};
use crate::palette::{PalettedSection, SECTION_SIZE, SECTION_VOLUME};
use crate::settings::WorldHeight;
use crate::tint::Tint;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: i32 = 16;
/// Sections per chunk in the default 384-block world.
pub const SECTIONS_PER_CHUNK: usize = crate::settings::WORLD_HEIGHT as usize / SECTION_SIZE;

/// Chunk coordinates: chunk (cx, cz) covers blocks `cx*16 .. cx*16+15`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }
    /// The chunk containing block column (bx, bz). Uses floor division so
    /// negative coordinates work: block -1 is in chunk -1, not chunk 0.
    #[inline]
    pub const fn from_block(bx: i32, bz: i32) -> Self {
        Self { x: bx >> 4, z: bz >> 4 }
    }
    #[inline]
    pub const fn min_block_x(self) -> i32 {
        self.x * CHUNK_SIZE
    }
    #[inline]
    pub const fn min_block_z(self) -> i32 {
        self.z * CHUNK_SIZE
    }
    pub const fn offset(self, dx: i32, dz: i32) -> Self {
        Self { x: self.x + dx, z: self.z + dz }
    }
    /// Chebyshev ("square ring") distance, used for render distance.
    pub fn chebyshev(self, other: ChunkPos) -> i32 {
        (self.x - other.x).abs().max((self.z - other.z).abs())
    }
}

/// A read-only view of one section, stored or implied.
#[derive(Clone, Copy)]
pub enum SectionRef<'a> {
    /// Not stored: every block is the same (stone below the window, air
    /// above it).
    Uniform(BlockId),
    Stored(&'a PalettedSection),
}

impl SectionRef<'_> {
    #[inline]
    pub fn get(&self, x: usize, y: usize, z: usize) -> BlockId {
        match self {
            SectionRef::Uniform(b) => *b,
            SectionRef::Stored(s) => s.get(x, y, z),
        }
    }
    #[inline]
    pub fn uniform(&self) -> Option<BlockId> {
        match self {
            SectionRef::Uniform(b) => Some(*b),
            SectionRef::Stored(s) => s.uniform(),
        }
    }
    pub fn to_dense(&self, out: &mut [BlockId; SECTION_VOLUME]) {
        match self {
            SectionRef::Uniform(b) => out.fill(*b),
            SectionRef::Stored(s) => s.to_dense(out),
        }
    }
    /// All distinct blocks in the section.
    pub fn palette(&self) -> &[BlockId] {
        match self {
            SectionRef::Uniform(b) => std::slice::from_ref(b),
            SectionRef::Stored(s) => s.palette(),
        }
    }
}

/// What fills the unstored sections between the floor and the window.
const BELOW_WINDOW: BlockId = blocks::STONE;

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub pos: ChunkPos,
    height: WorldHeight,
    /// Section 0 (holds the bedrock floor).
    floor: PalettedSection,
    /// Index of `window[0]`; always ≥ 1.
    window_start: usize,
    window: Vec<PalettedSection>,
    /// Y of the highest non-air block per column (`x + z*16`), or
    /// `min_y - 1` for an empty column. Used for spawning, maps and LODs.
    pub heightmap: [i16; 256],
    /// Biome of each column (`x + z*16`). One per column, like Minecraft
    /// before 1.18: biomes here are 2D, chosen at the surface.
    pub biomes: [Biome; 256],
    /// Grass and foliage colour of each column (`x + z*16`).
    pub tints: [Tint; 256],
}

impl Chunk {
    /// An all-air chunk.
    pub fn empty(pos: ChunkPos, height: WorldHeight) -> Self {
        Self {
            pos,
            height,
            floor: PalettedSection::filled(blocks::AIR),
            window_start: 1,
            // An empty window starting at 1: everything above the floor is air.
            window: Vec::new(),
            heightmap: [(height.min_y() - 1) as i16; 256],
            biomes: [Biome::default(); 256],
            tints: [Tint::default(); 256],
        }
    }

    /// Builds a chunk from its floor section and the stored sections
    /// starting at section index `window_start` (≥ 1). Sections between
    /// the floor and the window are stone. Uniform stone at the bottom of
    /// the window and uniform air at its top are dropped.
    pub fn assemble(
        pos: ChunkPos,
        height: WorldHeight,
        floor: PalettedSection,
        window_start: usize,
        window: Vec<PalettedSection>,
    ) -> Self {
        assert!(window_start >= 1 && window_start + window.len() <= height.sections());
        Self {
            pos,
            height,
            floor,
            window_start,
            window,
            heightmap: [(height.min_y() - 1) as i16; 256],
            biomes: [Biome::default(); 256],
            tints: [Tint::default(); 256],
        }
        .trimmed()
    }

    fn trimmed(mut self) -> Self {
        while self.window.last().is_some_and(|s| s.uniform() == Some(blocks::AIR)) {
            self.window.pop();
        }
        let lead = self.window.iter().take_while(|s| s.uniform() == Some(BELOW_WINDOW)).count();
        if lead > 0 {
            self.window.drain(..lead);
            self.window_start += lead;
        }
        // Sections below `window_start` read as stone and sections at or
        // above the window's end as air, so an empty window at index k means
        // "stone up to section k, air above".
        self
    }

    pub fn world_height(&self) -> WorldHeight {
        self.height
    }
    pub fn min_y(&self) -> i32 {
        self.height.min_y()
    }
    pub fn max_y(&self) -> i32 {
        self.height.max_y()
    }
    pub fn section_count(&self) -> usize {
        self.height.sections()
    }
    /// Section indices that are stored (floor excluded): `start..end`.
    pub fn window(&self) -> std::ops::Range<usize> {
        self.window_start..self.window_start + self.window.len()
    }

    /// Section `si` (0 = bottom of the world).
    #[inline]
    pub fn section(&self, si: usize) -> SectionRef<'_> {
        if si == 0 {
            SectionRef::Stored(&self.floor)
        } else if si < self.window_start {
            SectionRef::Uniform(BELOW_WINDOW)
        } else if si < self.window_start + self.window.len() {
            SectionRef::Stored(&self.window[si - self.window_start])
        } else {
            SectionRef::Uniform(blocks::AIR)
        }
    }

    /// Block at local x/z (0..16) and absolute world y. Air outside the
    /// world's height range.
    #[inline]
    pub fn get(&self, x: usize, y: i32, z: usize) -> BlockId {
        let min_y = self.min_y();
        if y < min_y || y > self.max_y() {
            return blocks::AIR;
        }
        let ly = (y - min_y) as usize;
        self.section(ly / SECTION_SIZE).get(x, ly % SECTION_SIZE, z)
    }

    /// Mutable access to section `si`, materialising it (and any implied
    /// sections between it and the window) if needed.
    fn section_mut(&mut self, si: usize) -> &mut PalettedSection {
        if si == 0 {
            return &mut self.floor;
        }
        while si < self.window_start {
            self.window.insert(0, PalettedSection::filled(BELOW_WINDOW));
            self.window_start -= 1;
        }
        while si >= self.window_start + self.window.len() {
            self.window.push(PalettedSection::filled(blocks::AIR));
        }
        &mut self.window[si - self.window_start]
    }

    pub fn set(&mut self, x: usize, y: i32, z: usize, block: BlockId) {
        let min_y = self.min_y();
        if y < min_y || y > self.max_y() {
            return;
        }
        let ly = (y - min_y) as usize;
        self.section_mut(ly / SECTION_SIZE).set(x, ly % SECTION_SIZE, z, block);
        let h = self.heightmap[x + z * 16];
        if !block.is_air() && y as i16 > h {
            self.heightmap[x + z * 16] = y as i16;
        } else if block.is_air() && y as i16 == h {
            // Removed the top block: scan down for the new top.
            let mut ny = y - 1;
            while ny >= min_y && self.get(x, ny, z).is_air() {
                ny -= 1;
            }
            self.heightmap[x + z * 16] = ny as i16;
        }
    }

    #[inline]
    pub fn height_at(&self, x: usize, z: usize) -> i32 {
        self.heightmap[x + z * 16] as i32
    }

    #[inline]
    pub fn biome_at(&self, x: usize, z: usize) -> Biome {
        self.biomes[x + z * 16]
    }

    pub fn is_empty(&self) -> bool {
        self.floor.uniform() == Some(blocks::AIR)
            && self.window_start == 1
            && self.window.iter().all(|s| s.uniform() == Some(blocks::AIR))
    }

    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.floor.memory_bytes()
            + self.window.iter().map(|s| s.memory_bytes()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{MAX_Y, MIN_Y};

    #[test]
    fn negative_block_coords_map_to_correct_chunk() {
        assert_eq!(ChunkPos::from_block(0, 15), ChunkPos::new(0, 0));
        assert_eq!(ChunkPos::from_block(-1, -16), ChunkPos::new(-1, -1));
        assert_eq!(ChunkPos::from_block(-17, 16), ChunkPos::new(-2, 1));
    }

    #[test]
    fn set_get_and_heightmap() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        assert_eq!(SECTIONS_PER_CHUNK, 24);
        assert!(c.is_empty());
        c.set(3, MIN_Y, 4, blocks::BEDROCK);
        c.set(3, 100, 4, blocks::STONE);
        c.set(3, MAX_Y, 4, blocks::GRASS);
        c.set(3, MAX_Y + 1, 4, blocks::GRASS); // ignored
        assert_eq!(c.get(3, 100, 4), blocks::STONE);
        assert_eq!(c.get(3, MAX_Y, 4), blocks::GRASS);
        assert_eq!(c.height_at(3, 4), MAX_Y);
        c.set(3, MAX_Y, 4, blocks::AIR);
        assert_eq!(c.height_at(3, 4), 100);
        assert_eq!(c.height_at(0, 0), MIN_Y - 1);
    }

    #[test]
    fn window_storage() {
        let h = WorldHeight::from_blocks(17_696);
        let mut c = Chunk::empty(ChunkPos::new(0, 0), h);
        assert!(c.is_empty());
        assert_eq!(c.get(0, h.max_y(), 0), blocks::AIR);
        // Writing far up materialises only the sections it needs.
        c.set(1, 5000, 1, blocks::STONE);
        assert_eq!(c.get(1, 5000, 1), blocks::STONE);
        assert_eq!(c.get(1, 4999, 1), blocks::AIR);
        assert_eq!(c.get(1, 0, 1), blocks::AIR);
        assert_eq!(c.height_at(1, 1), 5000);
        assert!(c.memory_bytes() < 100_000);

        // Assemble: stone sections below the window are implied.
        let floor = PalettedSection::filled(blocks::BEDROCK);
        let win = vec![
            PalettedSection::filled(blocks::STONE),
            PalettedSection::filled(blocks::DIRT),
            PalettedSection::filled(blocks::AIR),
        ];
        let c = Chunk::assemble(ChunkPos::new(0, 0), h, floor, 10, win);
        assert_eq!(c.window(), 11..12);
        let y = |si: usize| h.min_y() + (si * 16) as i32;
        assert_eq!(c.get(0, y(0), 0), blocks::BEDROCK);
        assert_eq!(c.get(0, y(5), 0), blocks::STONE);
        assert_eq!(c.get(0, y(10), 0), blocks::STONE);
        assert_eq!(c.get(0, y(11), 0), blocks::DIRT);
        assert_eq!(c.get(0, y(12), 0), blocks::AIR);
        assert_eq!(c.get(0, h.max_y(), 0), blocks::AIR);
        assert!(c.memory_bytes() < 2_000);
    }
}
