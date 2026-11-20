//! Block types.
//!
//! A block is stored as a 16-bit [`BlockId`]. Everything else about a block
//! (name, colour, whether it is solid) lives once in a static table, so a
//! chunk only stores small integers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct BlockId(pub u16);

/// How a block interacts with its neighbours when building meshes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opacity {
    /// Nothing to draw (air).
    Empty,
    /// Fully hides whatever is behind it.
    Opaque,
    /// Drawn, but neighbouring faces behind it stay visible (water, leaves, ice).
    Translucent,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub name: &'static str,
    /// sRGB colour used by the renderer and the map tool.
    pub color: [u8; 3],
    pub opacity: Opacity,
}

macro_rules! blocks {
    ($( $id:literal $konst:ident $name:literal [$r:literal, $g:literal, $b:literal] $op:ident ),* $(,)?) => {
        /// Named constants for every block.
        pub mod blocks {
            use super::BlockId;
            $( pub const $konst: BlockId = BlockId($id); )*
        }
        static BLOCK_TABLE: &[BlockInfo] = &[
            $( BlockInfo { name: $name, color: [$r, $g, $b], opacity: Opacity::$op } ),*
        ];
        #[cfg(test)]
        const BLOCK_IDS: &[u16] = &[$($id),*];
    };
}

// Colours are our own flat palette (no textures); they only need to make
// terrain readable in the viewer and on maps.
blocks! {
    0  AIR          "air"           [0, 0, 0]        Empty,
    1  STONE        "stone"         [125, 125, 125]  Opaque,
    2  DIRT         "dirt"          [134, 96, 67]    Opaque,
    3  GRASS        "grass_block"   [95, 159, 53]    Opaque,
    4  SAND         "sand"          [219, 207, 163]  Opaque,
    5  WATER        "water"         [48, 89, 196]    Translucent,
    6  BEDROCK      "bedrock"       [40, 40, 40]     Opaque,
    7  GRAVEL       "gravel"        [136, 126, 126]  Opaque,
    8  SNOW         "snow_block"    [240, 248, 250]  Opaque,
    9  ICE          "ice"           [160, 195, 245]  Translucent,
    10 SANDSTONE    "sandstone"     [216, 202, 155]  Opaque,
    11 OAK_LOG      "oak_log"       [102, 81, 50]    Opaque,
    12 OAK_LEAVES   "oak_leaves"    [58, 120, 35]    Translucent,
    13 CLAY         "clay"          [160, 166, 179]  Opaque,
}

impl BlockId {
    #[inline]
    pub fn info(self) -> &'static BlockInfo {
        &BLOCK_TABLE[self.0 as usize]
    }
    #[inline]
    pub fn is_air(self) -> bool {
        self == blocks::AIR
    }
    #[inline]
    pub fn opacity(self) -> Opacity {
        self.info().opacity
    }
    pub fn count() -> usize {
        BLOCK_TABLE.len()
    }
    pub fn by_name(name: &str) -> Option<BlockId> {
        BLOCK_TABLE.iter().position(|b| b.name == name).map(|i| BlockId(i as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_dense_and_match_table() {
        for (i, id) in BLOCK_IDS.iter().enumerate() {
            assert_eq!(*id as usize, i, "block ids must be 0..N in order");
        }
        assert_eq!(BLOCK_IDS.len(), BlockId::count());
        assert_eq!(BlockId::by_name("water"), Some(blocks::WATER));
        assert!(blocks::AIR.is_air());
    }
}
