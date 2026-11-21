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

/// Size of the world in blocks (1 block = 1 metre), centred on 0,0. Square
/// unless a depth is set: the realistic Earth is twice as wide as it is
/// deep, like a map of the whole planet (360° of longitude by 180° of
/// latitude).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldSize {
    width: u32,
    /// North–south extent; 0 means "same as the width" (and keeps older
    /// settings files loading).
    #[serde(default, skip_serializing_if = "is_zero")]
    depth: u32,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl WorldSize {
    pub fn from_blocks(width: u32) -> Self {
        Self { width: width.clamp(MIN_WORLD_WIDTH, MAX_WORLD_WIDTH), depth: 0 }
    }
    /// A rectangular world, `width` east–west by `depth` north–south.
    pub fn rect(width: u32, depth: u32) -> Self {
        let width = width.clamp(MIN_WORLD_WIDTH, MAX_WORLD_WIDTH);
        let depth = depth.clamp(MIN_WORLD_WIDTH, MAX_WORLD_WIDTH);
        Self { width, depth: if depth == width { 0 } else { depth } }
    }
    /// The same width, made square.
    pub fn square(self) -> Self {
        Self { depth: 0, ..self }
    }
    pub fn is_square(self) -> bool {
        self.depth == 0
    }
    pub fn from_km(km: f64) -> Self {
        Self::from_blocks((km * 1000.0).round().clamp(0.0, u32::MAX as f64) as u32)
    }
    pub const fn full() -> Self {
        Self { width: MAX_WORLD_WIDTH, depth: 0 }
    }
    pub fn width_blocks(self) -> u32 {
        self.width
    }
    pub fn width_km(self) -> f64 {
        self.width as f64 / 1000.0
    }
    /// North–south extent in blocks.
    pub fn depth_blocks(self) -> u32 {
        if self.depth == 0 {
            self.width
        } else {
            self.depth
        }
    }
    /// The larger of width and depth.
    pub fn max_extent(self) -> u32 {
        self.width.max(self.depth_blocks())
    }
    /// Blocks with `-half_width <= x < half_width` are inside.
    pub fn half_width(self) -> i32 {
        (self.width / 2) as i32
    }
    /// Blocks with `-half_depth <= z < half_depth` are inside.
    pub fn half_depth(self) -> i32 {
        (self.depth_blocks() / 2) as i32
    }
    #[inline]
    pub fn contains_column(self, x: i32, z: i32) -> bool {
        let (hx, hz) = (self.half_width(), self.half_depth());
        (-hx..hx).contains(&x) && (-hz..hz).contains(&z)
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

/// Which generator builds the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Generator {
    /// Minecraft-style: climate noise shapes the land (steps 1–3).
    #[default]
    Classic,
    /// Earth-like: a whole planet from plate tectonics, erosion, rivers and
    /// a climate driven by latitude and winds (step 5, `realistic`).
    Realistic,
}

impl Generator {
    pub const ALL: [Generator; 2] = [Generator::Classic, Generator::Realistic];
    pub fn label(self) -> &'static str {
        match self {
            Generator::Classic => "Classic (Minecraft-like)",
            Generator::Realistic => "Realistic (plate tectonics)",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Generator::Classic => "Endless Minecraft-style terrain from climate noise.",
            Generator::Realistic => {
                "A whole planet: continents from moving plates, mountain ranges where they collide, \
                 rivers, lakes and climate zones. Best at 40,000 km wide."
            }
        }
    }
}

/// The kind of planet a realistic world is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Planet {
    /// Several continents from a random plate layout, like today's Earth.
    #[default]
    Continents,
    /// One supercontinent surrounded by a world ocean.
    Pangea,
    /// The real Earth: today's continents and ocean floor (NOAA ETOPO
    /// 2022 relief), refined with the same detail as the others.
    Earth,
    /// A water world of archipelagos, island arcs and volcanic chains.
    Islands,
    /// Continents like `Continents`, each ruled by one biome (a desert
    /// continent, a jungle continent…) with a few kindred companions.
    Mushoku,
}

impl Planet {
    pub const ALL: [Planet; 5] = [Planet::Continents, Planet::Pangea, Planet::Earth, Planet::Islands, Planet::Mushoku];
    pub fn label(self) -> &'static str {
        match self {
            Planet::Continents => "Continents",
            Planet::Pangea => "Pangea",
            Planet::Earth => "Earth",
            Planet::Islands => "Islands",
            Planet::Mushoku => "Mushoku",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Planet::Continents => "Procedural continents and oceans shaped by moving plates.",
            Planet::Pangea => "One supercontinent in a world ocean, ringed by mountains.",
            Planet::Earth => "Today's Earth: real continents, mountains and sea floor.",
            Planet::Islands => "Mostly ocean: archipelagos, island arcs and volcanoes.",
            Planet::Mushoku => "Each continent ruled by one biome: a desert, a jungle, a taiga…",
        }
    }
}

/// How finely a realistic planet is simulated when the world is created.
/// Part of the world's identity, like the seed: the same seed at another
/// quality is a (similar but) different world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SimQuality {
    /// Coarse simulation grid, fewest erosion steps: fastest to create.
    Fast,
    #[default]
    Balanced,
    /// Finest grid and most erosion: more and smaller rivers, sharper
    /// coasts and ranges. Takes several seconds to create.
    Quality,
}

impl SimQuality {
    pub const ALL: [SimQuality; 3] = [SimQuality::Fast, SimQuality::Balanced, SimQuality::Quality];
    pub fn label(self) -> &'static str {
        match self {
            SimQuality::Fast => "Optimized",
            SimQuality::Balanced => "Balanced",
            SimQuality::Quality => "Quality",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            SimQuality::Fast => "Coarse planet simulation: ready in well under a second.",
            SimQuality::Balanced => "Finer plates, rivers and coasts: about a second to create.",
            SimQuality::Quality => "Finest simulation, three times the rivers: several seconds to create.",
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
    #[serde(default)]
    pub generator: Generator,
    /// Realistic worlds only.
    #[serde(default)]
    pub planet: Planet,
    /// Realistic worlds only.
    #[serde(default)]
    pub quality: SimQuality,
}

impl WorldSettings {
    /// Settings as the generator uses them: the Earth is a 2:1 rectangle,
    /// every other world a square.
    pub fn normalized(mut self) -> Self {
        let w = self.size.width_blocks();
        self.size = if self.generator == Generator::Realistic && self.planet == Planet::Earth {
            WorldSize::rect(w, w / 2)
        } else {
            self.size.square()
        };
        self
    }
}

impl Default for WorldSettings {
    fn default() -> Self {
        Self {
            seed: 0,
            size: WorldSize::default(),
            world_type: WorldType::Normal,
            features: true,
            height: WorldHeight::default(),
            generator: Generator::Classic,
            planet: Planet::Continents,
            quality: SimQuality::Balanced,
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
    fn earth_is_twice_as_wide_as_deep() {
        let s = WorldSettings {
            generator: Generator::Realistic,
            planet: Planet::Earth,
            size: WorldSize::from_blocks(40_000_000),
            ..Default::default()
        }
        .normalized();
        assert_eq!((s.size.width_blocks(), s.size.depth_blocks()), (40_000_000, 20_000_000));
        assert!(s.size.contains_column(-20_000_000, -10_000_000));
        assert!(!s.size.contains_column(0, 10_000_000));
        assert!(s.size.contains_column(19_999_999, 0));
        let back = WorldSettings { planet: Planet::Pangea, ..s }.normalized();
        assert!(back.size.is_square());
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
            generator: Generator::Realistic,
            planet: Planet::Earth,
            quality: SimQuality::Quality,
        };
        let text = ron::to_string(&s).unwrap();
        assert_eq!(ron::from_str::<WorldSettings>(&text).unwrap(), s);
        // Settings saved before the height option existed still load.
        let old = "(seed: 1, size: (width: 4000), world_type: Normal, features: true)";
        let old = ron::from_str::<WorldSettings>(old).unwrap();
        assert_eq!(old.height, WorldHeight::default());
        assert_eq!(old.generator, Generator::Classic);
        assert!(old.size.is_square());
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
