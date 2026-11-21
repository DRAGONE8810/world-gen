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
    // Step 3: biome ground blocks.
    14 RED_SAND           "red_sand"            [190, 102, 33]   Opaque,
    15 TERRACOTTA         "terracotta"          [152, 94, 67]    Opaque,
    16 ORANGE_TERRACOTTA  "orange_terracotta"   [161, 83, 37]    Opaque,
    17 YELLOW_TERRACOTTA  "yellow_terracotta"   [186, 133, 35]   Opaque,
    18 RED_TERRACOTTA     "red_terracotta"      [143, 61, 46]    Opaque,
    19 WHITE_TERRACOTTA   "white_terracotta"    [209, 178, 161]  Opaque,
    20 BROWN_TERRACOTTA   "brown_terracotta"    [77, 51, 35]     Opaque,
    21 PODZOL             "podzol"              [91, 63, 24]     Opaque,
    22 MYCELIUM           "mycelium"            [111, 99, 105]   Opaque,
    23 COARSE_DIRT        "coarse_dirt"         [119, 85, 59]    Opaque,
    24 MUD                "mud"                 [60, 57, 60]     Opaque,
    25 PACKED_ICE         "packed_ice"          [141, 180, 250]  Opaque,
    26 BASALT             "basalt"              [72, 72, 78]     Opaque,
    27 SALT               "salt_crust"          [236, 232, 222]  Opaque,
    28 LIMESTONE          "limestone"           [196, 190, 170]  Opaque,
    // Step 3: trees and plants.
    29 BIRCH_LOG          "birch_log"           [216, 215, 210]  Opaque,
    30 BIRCH_LEAVES       "birch_leaves"        [100, 150, 60]   Translucent,
    31 SPRUCE_LOG         "spruce_log"          [58, 37, 16]     Opaque,
    32 SPRUCE_LEAVES      "spruce_leaves"       [50, 90, 55]     Translucent,
    33 JUNGLE_LOG         "jungle_log"          [85, 67, 25]     Opaque,
    34 JUNGLE_LEAVES      "jungle_leaves"       [40, 140, 20]    Translucent,
    35 ACACIA_LOG         "acacia_log"          [103, 96, 86]    Opaque,
    36 ACACIA_LEAVES      "acacia_leaves"       [95, 135, 30]    Translucent,
    37 DARK_OAK_LOG       "dark_oak_log"        [60, 46, 26]     Opaque,
    38 DARK_OAK_LEAVES    "dark_oak_leaves"     [40, 85, 20]     Translucent,
    39 MANGROVE_LOG       "mangrove_log"        [84, 56, 40]     Opaque,
    40 MANGROVE_LEAVES    "mangrove_leaves"     [70, 125, 25]    Translucent,
    41 MANGROVE_ROOTS     "mangrove_roots"      [74, 59, 38]     Opaque,
    42 CACTUS             "cactus"              [85, 127, 43]    Opaque,
    43 MUSHROOM_STEM      "mushroom_stem"       [203, 196, 185]  Opaque,
    44 RED_MUSHROOM_BLOCK "red_mushroom_block"  [200, 46, 45]    Opaque,
    45 BROWN_MUSHROOM_BLOCK "brown_mushroom_block" [149, 111, 81] Opaque,
    // Step 7: coral sand of tropical beaches.
    46 WHITE_SAND         "white_sand"          [240, 233, 208]  Opaque,
    // Step 8: trees of the new biomes.
    47 CHERRY_LOG         "cherry_log"          [82, 50, 52]     Opaque,
    48 CHERRY_LEAVES      "cherry_leaves"       [238, 168, 198]  Translucent,
    49 RED_LEAVES         "red_maple_leaves"    [186, 52, 36]    Translucent,
    50 ORANGE_LEAVES      "orange_maple_leaves" [218, 118, 34]   Translucent,
    51 YELLOW_LEAVES      "yellow_leaves"       [226, 186, 52]   Translucent,
    52 PALM_LEAVES        "palm_leaves"         [92, 162, 46]    Translucent,
    53 BAOBAB_LOG         "baobab_log"          [152, 128, 108]  Opaque,
    54 REDWOOD_LOG        "redwood_log"         [126, 56, 36]    Opaque,
    55 BAMBOO             "bamboo"              [112, 166, 56]   Opaque,
    56 BAMBOO_LEAVES      "bamboo_leaves"       [104, 178, 62]   Translucent,
    // Step 8: grass in flower (a flat colour, no plant models).
    57 POPPIES            "poppies"             [204, 46, 40]    Opaque,
    58 DANDELIONS         "dandelions"          [234, 204, 52]   Opaque,
    59 CORNFLOWERS        "cornflowers"         [82, 112, 218]   Opaque,
    60 DAISIES            "daisies"             [236, 236, 226]  Opaque,
    61 LAVENDER           "lavender"            [152, 112, 202]  Opaque,
    62 PINK_PETALS        "pink_petals"         [240, 166, 196]  Opaque,
    63 HEATHER            "heather"             [150, 88, 138]   Opaque,
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
    /// Logs, leaves, roots, cactus and mushroom blocks: everything trees
    /// are built from. Used by tests and the column/chunk agreement checks.
    pub fn is_tree_part(self) -> bool {
        (blocks::OAK_LOG.0..=blocks::OAK_LEAVES.0).contains(&self.0)
            || (blocks::BIRCH_LOG.0..=blocks::BROWN_MUSHROOM_BLOCK.0).contains(&self.0)
            || (blocks::CHERRY_LOG.0..=blocks::BAMBOO_LEAVES.0).contains(&self.0)
    }
    /// Grass in flower: a grass block covered by one colour of flowers.
    pub fn is_flowers(self) -> bool {
        (blocks::POPPIES.0..=blocks::HEATHER.0).contains(&self.0)
    }
    /// Grass, flowering or not: ground that plants grow on.
    pub fn is_grassy(self) -> bool {
        self == blocks::GRASS || self.is_flowers()
    }
    /// Leaves of any species (trees may grow their logs through these).
    pub fn is_leaves(self) -> bool {
        matches!(
            self,
            blocks::OAK_LEAVES
                | blocks::BIRCH_LEAVES
                | blocks::SPRUCE_LEAVES
                | blocks::JUNGLE_LEAVES
                | blocks::ACACIA_LEAVES
                | blocks::DARK_OAK_LEAVES
                | blocks::MANGROVE_LEAVES
                | blocks::CHERRY_LEAVES
                | blocks::RED_LEAVES
                | blocks::ORANGE_LEAVES
                | blocks::YELLOW_LEAVES
                | blocks::PALM_LEAVES
                | blocks::BAMBOO_LEAVES
        )
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
