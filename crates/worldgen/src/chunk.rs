//! Chunks: the unit of generation, storage and meshing.
//!
//! A chunk is a 16×16 column of blocks spanning the full world height
//! (`MIN_Y..=MAX_Y`, 384 layers), split vertically into 24 sections of
//! 16×16×16. Each section is palette-compressed independently, so the
//! all-air sky and all-stone underground cost almost nothing.

use crate::block::{blocks, BlockId};
use crate::palette::{PalettedSection, SECTION_SIZE};
use crate::settings::{MAX_Y, MIN_Y, WORLD_HEIGHT};
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: i32 = 16;
pub const SECTIONS_PER_CHUNK: usize = (WORLD_HEIGHT as usize) / SECTION_SIZE;

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

#[derive(Clone, Debug)]
pub struct Chunk {
    pub pos: ChunkPos,
    pub sections: Vec<PalettedSection>,
    /// Y of the highest non-air block per column (`x + z*16`), or `MIN_Y - 1`
    /// for an empty column. Used for spawning, lighting and maps.
    pub heightmap: [i16; 256],
}

impl Chunk {
    pub fn empty(pos: ChunkPos) -> Self {
        Self {
            pos,
            sections: vec![PalettedSection::filled(blocks::AIR); SECTIONS_PER_CHUNK],
            heightmap: [(MIN_Y - 1) as i16; 256],
        }
    }

    /// Block at local x/z (0..16) and absolute world y.
    #[inline]
    pub fn get(&self, x: usize, y: i32, z: usize) -> BlockId {
        if !(MIN_Y..=MAX_Y).contains(&y) {
            return blocks::AIR;
        }
        let ly = (y - MIN_Y) as usize;
        self.sections[ly / SECTION_SIZE].get(x, ly % SECTION_SIZE, z)
    }

    pub fn set(&mut self, x: usize, y: i32, z: usize, block: BlockId) {
        if !(MIN_Y..=MAX_Y).contains(&y) {
            return;
        }
        let ly = (y - MIN_Y) as usize;
        self.sections[ly / SECTION_SIZE].set(x, ly % SECTION_SIZE, z, block);
        let h = self.heightmap[x + z * 16];
        if !block.is_air() && y as i16 > h {
            self.heightmap[x + z * 16] = y as i16;
        } else if block.is_air() && y as i16 == h {
            // Removed the top block: scan down for the new top.
            let mut ny = y - 1;
            while ny >= MIN_Y && self.get(x, ny, z).is_air() {
                ny -= 1;
            }
            self.heightmap[x + z * 16] = ny as i16;
        }
    }

    #[inline]
    pub fn height_at(&self, x: usize, z: usize) -> i32 {
        self.heightmap[x + z * 16] as i32
    }

    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|s| s.uniform() == Some(blocks::AIR))
    }

    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.sections.iter().map(|s| s.memory_bytes()).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_block_coords_map_to_correct_chunk() {
        assert_eq!(ChunkPos::from_block(0, 15), ChunkPos::new(0, 0));
        assert_eq!(ChunkPos::from_block(-1, -16), ChunkPos::new(-1, -1));
        assert_eq!(ChunkPos::from_block(-17, 16), ChunkPos::new(-2, 1));
    }

    #[test]
    fn set_get_and_heightmap() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
        assert_eq!(SECTIONS_PER_CHUNK, 24);
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
}
