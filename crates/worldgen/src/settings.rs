//! Everything that defines a world. Two worlds with equal `WorldSettings`
//! are identical, block for block.

use serde::{Deserialize, Serialize};

/// Lowest block layer (inclusive) of the default 384-block world. Same
/// vertical range as modern Minecraft. Taller or shorter worlds use
/// [`WorldHeight::min_y`] instead.
pub const MIN_Y: i32 = -64;
/// Number of block layers in the default world.
pub const WORLD_HEIGHT: i32 = 384;
/// Highest block layer (inclusive) of the default world.
pub const MAX_Y: i32 = MIN_Y + WORLD_HEIGHT - 1;
/// Water fills every air block at or below this height in oceans. The same
/// in every world height.
pub const SEA_LEVEL: i32 = 63;

/// Smallest selectable world height.
pub const MIN_WORLD_HEIGHT: u32 = 256;
/// Tallest selectable world height: twice Mount Everest (2 × 8,849 m),
/// rounded down to whole 16-block sections.
pub const MAX_WORLD_HEIGHT: u32 = 17_696;

/// Total height of the world in blocks (1 block = 1 m).
///
/// The whole terrain is scaled with it, vertically *and* horizontally, by
/// [`WorldHeight::terrain_scale`] = height / 384. A 17,696-block world is
/// the default world magnified 46×: mountains ~9.5 km above the sea (like
/// the Himalaya), ocean trenches ~2.3 km deep, continents ~180 km across.
/// Scaling both directions keeps slopes realistic; scaling only the height
/// would turn every hill into a needle. Trees, rivers' banks, beaches and
/// surface layers keep their real size in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldHeight {
    height: u32,
}

impl WorldHeight {
    /// Rounds to whole 16-block sections and clamps to the allowed range.
    pub fn from_blocks(height: u32) -> Self {
        let h = height.clamp(MIN_WORLD_HEIGHT, MAX_WORLD_HEIGHT);
        Self { height: (h + 8) / 16 * 16 }
    }
    pub const fn default_height() -> Self {
        Self { height: WORLD_HEIGHT as u32 }
    }
    pub fn blocks(self) -> u32 {
        self.height
    }
    /// Vertical and horizontal magnification of the terrain (1.0 for 384).
    pub fn terrain_scale(self) -> f64 {
        self.height as f64 / WORLD_HEIGHT as f64
    }
    /// Lowest layer. The part below sea level grows with the terrain scale
    /// (deeper oceans need more room), rounded to whole sections.
    pub fn min_y(self) -> i32 {
        let below = (64.0 * self.terrain_scale() / 16.0).round().max(1.0) as i32 * 16;
        -below
    }
    /// Highest layer (inclusive).
    pub fn max_y(self) -> i32 {
        self.min_y() + self.height as i32 - 1
    }
    /// Number of 16-block sections in a column.
    pub fn sections(self) -> usize {
        self.height as usize / 16
    }

    /// Presets shown in the menu.
    pub const PRESETS: &'static [(&'static str, u32)] = &[
        ("256", 256),
        ("384 (Minecraft)", 384),
        ("1,000", 1_000),
        ("2,000", 2_000),
        ("5,000", 5_000),
        ("10,000", 10_000),
        ("17,696 (2× Everest)", MAX_WORLD_HEIGHT),
    ];
}

impl Default for WorldHeight {
    fn default() -> Self {
        Self::default_height()
    }
}

/// The largest world we support: the vanilla Minecraft world border sits at
/// ±29,999,984 blocks, i.e. just under 60,000 km across.
pub const MAX_WORLD_WIDTH: u32 = 59_999_968;
/// Smallest world: 1 km across.
pub const MIN_WORLD_WIDTH: u32 = 1_000;

/// Width of the square world in blocks (1 block = 1 metre), centred on 0,0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSize {
    width: u32,
}

impl WorldSize {
    pub fn from_blocks(width: u32) -> Self {
        Self { width: width.clamp(MIN_WORLD_WIDTH, MAX_WORLD_WIDTH) }
    }
    pub fn from_km(km: f64) -> Self {
        Self::from_blocks((km * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32)
    }
    pub const fn full() -> Self {
        Self { width: MAX_WORLD_WIDTH }
    }
    pub fn width_blocks(self) -> u32 {
        self.width
    }
    pub fn width_km(self) -> f64 {
        self.width as f64 / 1000.0
    }
    /// Blocks with `-half <= x < half` (and the same for z) are inside.
    pub fn half_width(self) -> i32 {
        (self.width / 2) as i32
    }
    #[inline]
    pub fn contains_column(self, x: i32, z: i32) -> bool {
        let h = self.half_width();
        (-h..h).contains(&x) && (-h..h).contains(&z)
    }

    /// Presets shown in the menu.
    pub const PRESETS: &'static [(&'static str, u32)] = &[
        ("1 km", 1_000),
        ("4 km", 4_000),
        ("16 km", 16_000),
        ("64 km", 64_000),
        ("1,000 km", 1_000_000),
        ("10,000 km", 10_000_000),
        ("Full (60,000 km)", MAX_WORLD_WIDTH),
    ];
}

impl Default for WorldSize {
    fn default() -> Self {
        Self::from_blocks(16_000)
    }
}

/// Generation style, picked in the "World settings" menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorldType {
    /// Continents, oceans, mountains and biomes.
    #[default]
    Normal,
    /// Same as normal but terrain height variation is greatly exaggerated.
    Amplified,
    /// Biomes and climate zones are four times larger.
    LargeBiomes,
    /// Mostly ocean with scattered islands and archipelagos.
    Islands,
    /// Flat layers of bedrock, stone, dirt and grass. Useful for testing.
    Flat,
}

impl WorldType {
    pub const ALL: [WorldType; 5] =
        [WorldType::Normal, WorldType::Amplified, WorldType::LargeBiomes, WorldType::Islands, WorldType::Flat];

    pub fn label(self) -> &'static str {
        match self {
            WorldType::Normal => "Normal",
            WorldType::Amplified => "Amplified",
            WorldType::LargeBiomes => "Large biomes",
            WorldType::Islands => "Islands",
            WorldType::Flat => "Flat",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            WorldType::Normal => "Continents, oceans, mountains and biomes.",
            WorldType::Amplified => "Extreme mountains and deep valleys.",
            WorldType::LargeBiomes => "Everything is four times wider.",
            WorldType::Islands => "Mostly ocean with scattered islands.",
            WorldType::Flat => "Flat layers, for testing.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldSettings {
    pub seed: u64,
    pub size: WorldSize,
    pub world_type: WorldType,
    /// Generate trees and other surface features.
    pub features: bool,
    /// Total world height. `serde(default)` keeps settings files from
    /// before this option loading as 384-block worlds.
    #[serde(default)]
    pub height: WorldHeight,
}

impl Default for WorldSettings {
    fn default() -> Self {
        Self {
            seed: 0,
            size: WorldSize::default(),
            world_type: WorldType::Normal,
            features: true,
            height: WorldHeight::default(),
        }
    }
}

/// Turns the text typed into the "Seed" box into a numeric seed. Numbers are
/// used as-is; any other text is hashed, so "hello" is always the same world.
pub fn seed_from_text(text: &str) -> Option<u64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(n) = t.parse::<i64>() {
        return Some(n as u64);
    }
    if let Ok(n) = t.parse::<u64>() {
        return Some(n);
    }
    Some(crate::rng::derive_seed(0, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_clamped() {
        assert_eq!(WorldSize::from_blocks(10).width_blocks(), MIN_WORLD_WIDTH);
        assert_eq!(WorldSize::from_blocks(u32::MAX).width_blocks(), MAX_WORLD_WIDTH);
        assert_eq!(WorldSize::from_km(4.0).width_blocks(), 4000);
    }

    #[test]
    fn border_contains_exactly_width_columns() {
        let s = WorldSize::from_blocks(1000);
        assert!(s.contains_column(-500, 499));
        assert!(!s.contains_column(500, 0));
        assert!(!s.contains_column(0, -501));
        let full = WorldSize::full();
        assert!(full.contains_column(-29_999_984, 29_999_983));
        assert!(!full.contains_column(29_999_984, 0));
    }

    #[test]
    fn seeds_from_text() {
        assert_eq!(seed_from_text("123"), Some(123));
        assert_eq!(seed_from_text("-1"), Some(u64::MAX));
        assert_eq!(seed_from_text("hello"), seed_from_text(" hello "));
        assert_ne!(seed_from_text("hello"), seed_from_text("world"));
        assert_eq!(seed_from_text("   "), None);
    }

    #[test]
    fn settings_roundtrip_through_ron() {
        let s = WorldSettings {
            seed: 99,
            size: WorldSize::full(),
            world_type: WorldType::Islands,
            features: false,
            height: WorldHeight::from_blocks(5000),
        };
        let text = ron::to_string(&s).unwrap();
        assert_eq!(ron::from_str::<WorldSettings>(&text).unwrap(), s);
        // Settings saved before the height option existed still load.
        let old = "(seed: 1, size: (width: 4000), world_type: Normal, features: true)";
        assert_eq!(ron::from_str::<WorldSettings>(old).unwrap().height, WorldHeight::default());
    }

    #[test]
    fn world_heights() {
        let d = WorldHeight::default();
        assert_eq!((d.min_y(), d.max_y(), d.sections()), (MIN_Y, MAX_Y, 24));
        assert_eq!(d.terrain_scale(), 1.0);
        let max = WorldHeight::from_blocks(u32::MAX);
        assert_eq!(max.blocks(), MAX_WORLD_HEIGHT);
        assert!(max.blocks() as f64 <= 2.0 * 8_849.0);
        assert_eq!(WorldHeight::from_blocks(10).blocks(), MIN_WORLD_HEIGHT);
        for h in [256, 300, 1000, 5000, 10_000, 17_696] {
            let w = WorldHeight::from_blocks(h);
            assert_eq!(w.blocks() % 16, 0);
            assert_eq!(w.min_y() % 16, 0);
            assert!(w.min_y() < SEA_LEVEL - 40 && w.max_y() > SEA_LEVEL + 100, "{h}");
            assert_eq!((w.max_y() - w.min_y() + 1) as u32, w.blocks());
        }
    }
}
