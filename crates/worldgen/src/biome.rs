//! Biomes: which kind of landscape a column belongs to.
//!
//! ## How a biome is chosen
//!
//! Every column already has six smooth "climate" values from
//! [`crate::terrain`] (or the simulated planet of [`crate::realistic`]):
//! continentalness, erosion, weirdness (and its folded form, peaks &
//! valleys), temperature and humidity. The biome is a function of those
//! values, the column's surface height and its region
//! ([`crate::region`]). It works in steps, each reading like geography:
//!
//! 1. **Water.** If the surface is below the water the column is ocean,
//!    river or lake. Depth and temperature choose the ocean variant;
//!    mangroves stand in warm shallows.
//! 2. **Oceanic islands** (rare cones raised from the deep sea) are volcanic
//!    or mushroom fields.
//! 3. **Coast.** Low land next to the sea is one of the shores seen on
//!    Earth, by climate and lie of the land: mangrove forest on flat, warm,
//!    humid coasts and estuaries, coral sand in the humid tropics, salt
//!    flats (sabkha) on hot dry coasts, shingle on cool stormy ones, tidal
//!    mud flats, dune belts, fjords and stony shores.
//! 4. **Altitude.** Temperature drops with height, like the real lapse rate
//!    of about 6.5 °C per km. High summits become frozen, jagged or stony
//!    peaks; high slopes become groves, snowy slopes, meadows, cloud forest,
//!    páramo or windswept hills; wet uplands become moorland.
//! 5. **Everything else** is classified **in climate space**, the way
//!    ecologists' Whittaker diagram places biomes on a temperature ×
//!    rainfall chart. Each biome has one or more *target points* on that
//!    chart (`LOWLAND`), and a column gets the biome whose target is
//!    nearest to its climate: a Voronoi diagram of the chart. Several points
//!    per biome let a biome take an irregular area of the chart, and adding
//!    a biome is adding a point. Then sister biomes split by the
//!    weirdness noise (forest and birch forest, plains and flower plains),
//!    and flat, waterlogged ground turns into swamp, bog or flooded forest.
//! 6. **Regions.** Inside a rare region ([`crate::region`]) the matching
//!    biome becomes the regional one: cherry groves, pillar forests, baobab
//!    savannas, redwoods, bamboo, red outback, salt pans, lavender.
//!
//! ## Transitions
//!
//! A biome is a discrete choice from continuous values, so choosing it
//! from the exact climate draws a hard line wherever a threshold is
//! crossed. Instead, each column's climate is first offset by a small
//! **dither** ([`Jitter`], from [`BiomeDither`]): smooth noise in patches
//! of 16–32 blocks plus a little per-column randomness. Far from a border
//! the offset changes nothing. Within the dither width of a border, the
//! column lands on one side or the other with a probability that falls
//! linearly from 100 % to 0 % across the zone, 50/50 on the border itself.
//! The result is a wide transition zone where both biomes interleave in
//! patches, the nearer biome dominating, as forests and grasslands do in
//! nature. The width is set in climate units, so it scales with how fast
//! the climate changes: a few hundred blocks in a classic world, tens of
//! km on a realistic planet, where climate changes slowly.
//!
//! Trees choose their species the same way, with their own dither that is
//! half the column's patch and half per-tree randomness: in the middle of a
//! forest–taiga border half the trees are spruces and half are oaks, mixed
//! individually and in groves, and the share follows the distance to each
//! side. Grass colour follows the climate continuously (see
//! [`crate::tint`]), so it needs no dither.
//!
//! Real-world biomes that Minecraft doesn't have are marked
//! [`Origin::RealWorld`] in the table below.

use crate::noise::{OctaveConfig, OctaveNoise};
use crate::region::Flavour;
use crate::rng::{derive_seed, hash_position};
use crate::settings::SEA_LEVEL;
use crate::terrain::Shape;

/// Where a biome comes from: the Minecraft family, or added from real
/// geography (Whittaker diagram, coastal and volcanic landforms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Minecraft,
    RealWorld,
}

macro_rules! biomes {
    ($( $v:ident $name:literal map [$r:literal, $g:literal, $b:literal]
        grass [$gr:literal, $gg:literal, $gb:literal] $origin:ident ),* $(,)?) => {
        /// A biome. Stored as one byte per column in [`crate::Chunk`].
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum Biome {
            #[default]
            $( $v ),*
        }

        impl Biome {
            /// Every biome, in id order.
            pub const ALL: &'static [Biome] = &[$( Biome::$v ),*];

            /// Lowercase identifier, e.g. `"birch_forest"`.
            pub fn name(self) -> &'static str {
                match self { $( Biome::$v => $name ),* }
            }
            /// Distinct colour for the biome map layer.
            pub fn map_color(self) -> [u8; 3] {
                match self { $( Biome::$v => [$r, $g, $b] ),* }
            }
            /// Grass colour in this biome: lush in jungles, olive in swamps,
            /// straw-coloured in savannas, brownish in tundra.
            pub fn grass_color(self) -> [u8; 3] {
                match self { $( Biome::$v => [$gr, $gg, $gb] ),* }
            }
            pub fn origin(self) -> Origin {
                match self { $( Biome::$v => Origin::$origin ),* }
            }
        }
    };
}

// The `#[default]` attribute lands on the first entry, so Plains comes first.
biomes! {
    Plains              "plains"               map [141, 179,  96] grass [121, 192,  90] Minecraft,
    // --- Water ---------------------------------------------------------
    Ocean               "ocean"                map [  0,   0, 160] grass [142, 185, 113] Minecraft,
    DeepOcean           "deep_ocean"           map [  0,   0,  90] grass [142, 185, 113] Minecraft,
    WarmOcean           "warm_ocean"           map [  0,  90, 200] grass [142, 185, 113] Minecraft,
    LukewarmOcean       "lukewarm_ocean"       map [  0,  60, 180] grass [142, 185, 113] Minecraft,
    DeepLukewarmOcean   "deep_lukewarm_ocean"  map [  0,  40, 120] grass [142, 185, 113] Minecraft,
    ColdOcean           "cold_ocean"           map [ 32,  32, 150] grass [142, 185, 113] Minecraft,
    DeepColdOcean       "deep_cold_ocean"      map [ 32,  32,  90] grass [142, 185, 113] Minecraft,
    FrozenOcean         "frozen_ocean"         map [112, 112, 214] grass [128, 180, 151] Minecraft,
    DeepFrozenOcean     "deep_frozen_ocean"    map [ 64,  64, 150] grass [128, 180, 151] Minecraft,
    River               "river"                map [ 40,  90, 255] grass [142, 185, 113] Minecraft,
    FrozenRiver         "frozen_river"         map [160, 160, 255] grass [128, 180, 151] Minecraft,
    Lake                "lake"                 map [ 60, 120, 230] grass [142, 185, 113] RealWorld,
    // --- Coast ---------------------------------------------------------
    Beach               "beach"                map [250, 222,  85] grass [145, 189,  89] Minecraft,
    SnowyBeach          "snowy_beach"          map [250, 240, 192] grass [128, 180, 151] Minecraft,
    StonyShore          "stony_shore"          map [162, 162, 132] grass [138, 182, 137] Minecraft,
    Fjord               "fjord"                map [ 90, 110, 130] grass [128, 175, 140] RealWorld,
    SaltFlats           "salt_flats"           map [245, 245, 235] grass [191, 183,  85] RealWorld,
    CoastalDunes        "coastal_dunes"        map [238, 214, 140] grass [160, 180,  90] RealWorld,
    ShingleBeach        "shingle_beach"        map [150, 145, 135] grass [130, 175, 120] RealWorld,
    TropicalBeach       "tropical_beach"       map [255, 245, 210] grass [100, 185,  60] RealWorld,
    TidalFlats          "tidal_flats"          map [110, 105,  90] grass [120, 150,  80] RealWorld,
    MangroveSwamp       "mangrove_swamp"       map [ 50, 110,  60] grass [141, 177,  39] Minecraft,
    // --- Oceanic islands -----------------------------------------------
    MushroomFields      "mushroom_fields"      map [255,   0, 255] grass [ 85, 201,  63] Minecraft,
    Volcanic            "volcanic"             map [ 60,  40,  40] grass [150, 160,  80] RealWorld,
    // --- Mountains -----------------------------------------------------
    FrozenPeaks         "frozen_peaks"         map [160, 200, 255] grass [128, 180, 151] Minecraft,
    JaggedPeaks         "jagged_peaks"         map [220, 220, 245] grass [128, 180, 151] Minecraft,
    StonyPeaks          "stony_peaks"          map [123, 143, 116] grass [138, 182, 137] Minecraft,
    SnowySlopes         "snowy_slopes"         map [196, 196, 196] grass [128, 180, 151] Minecraft,
    Grove               "grove"                map [ 71, 114, 108] grass [128, 180, 151] Minecraft,
    Meadow              "meadow"               map [ 96, 164,  69] grass [131, 187, 109] Minecraft,
    CloudForest         "cloud_forest"         map [ 30, 120,  90] grass [ 60, 160,  70] RealWorld,
    Paramo              "paramo"               map [150, 160, 100] grass [140, 165, 100] RealWorld,
    WindsweptHills      "windswept_hills"      map [ 96,  96,  96] grass [138, 182, 137] Minecraft,
    Moor                "moor"                 map [120, 100, 110] grass [125, 120,  80] RealWorld,
    // --- Cold lowlands -------------------------------------------------
    IceSheet            "ice_sheet"            map [235, 245, 255] grass [128, 180, 151] RealWorld,
    Tundra              "tundra"               map [170, 160, 130] grass [150, 155, 100] RealWorld,
    SnowyPlains         "snowy_plains"         map [255, 255, 255] grass [128, 180, 151] Minecraft,
    SnowyTaiga          "snowy_taiga"          map [ 49,  85,  74] grass [128, 180, 151] Minecraft,
    Taiga               "taiga"                map [ 11, 102,  89] grass [134, 183, 131] Minecraft,
    OldGrowthTaiga      "old_growth_taiga"     map [ 89, 102,  81] grass [124, 170, 110] Minecraft,
    TemperateRainforest "temperate_rainforest" map [ 20,  80,  40] grass [ 70, 150,  60] RealWorld,
    Bog                 "bog"                  map [ 70,  90,  70] grass [110, 140,  90] RealWorld,
    MixedForest         "mixed_forest"         map [ 40, 100,  60] grass [110, 170,  90] RealWorld,
    // --- Temperate lowlands --------------------------------------------
    Steppe              "steppe"               map [200, 190, 120] grass [175, 170,  95] RealWorld,
    FlowerPlains        "flower_plains"        map [170, 200, 100] grass [121, 192,  90] Minecraft,
    Forest              "forest"               map [  5, 102,  33] grass [ 89, 174,  48] Minecraft,
    BirchForest         "birch_forest"         map [ 48, 116,  68] grass [116, 183,  72] Minecraft,
    AutumnForest        "autumn_forest"        map [200, 110,  40] grass [140, 165,  70] RealWorld,
    DarkForest          "dark_forest"          map [ 64,  81,  26] grass [ 80, 122,  50] Minecraft,
    FlowerForest        "flower_forest"        map [ 45, 142,  73] grass [100, 180,  60] Minecraft,
    Swamp               "swamp"                map [  7, 249, 178] grass [106, 112,  57] Minecraft,
    // --- Warm and hot lowlands -----------------------------------------
    Chaparral           "chaparral"            map [170, 140,  70] grass [160, 160,  85] RealWorld,
    Savanna             "savanna"              map [189, 178,  95] grass [191, 183,  85] Minecraft,
    ThornScrub          "thorn_scrub"          map [160, 140,  80] grass [175, 170,  85] RealWorld,
    Desert              "desert"               map [250, 148,  24] grass [191, 183,  85] Minecraft,
    RockyDesert         "rocky_desert"         map [180, 110,  70] grass [191, 183,  85] RealWorld,
    Badlands            "badlands"             map [217,  69,  21] grass [144, 129,  77] Minecraft,
    Karst               "karst"                map [140, 200, 160] grass [ 90, 175,  60] RealWorld,
    TropicalDryForest   "tropical_dry_forest"  map [120, 140,  50] grass [130, 170,  60] RealWorld,
    SparseJungle        "sparse_jungle"        map [ 98, 139,  23] grass [100, 185,  50] Minecraft,
    Jungle              "jungle"               map [ 83, 123,   9] grass [ 70, 190,  30] Minecraft,
    FloodedJungle       "flooded_jungle"       map [ 40, 100,  50] grass [ 80, 180,  40] RealWorld,
    // --- Regional (see `region`) ---------------------------------------
    CherryGrove         "cherry_grove"         map [240, 170, 200] grass [120, 190,  90] Minecraft,
    PillarKarst         "pillar_karst"         map [110, 190, 150] grass [ 90, 175,  60] RealWorld,
    BaobabSavanna       "baobab_savanna"       map [210, 170,  90] grass [191, 183,  85] RealWorld,
    RedwoodForest       "redwood_forest"       map [120,  50,  35] grass [ 90, 150,  70] RealWorld,
    BambooForest        "bamboo_forest"        map [ 90, 190,  60] grass [ 90, 185,  55] Minecraft,
    Outback             "outback"              map [200,  80,  30] grass [180, 150,  80] RealWorld,
    LavenderFields      "lavender_fields"      map [150, 120, 210] grass [150, 170,  90] RealWorld,
}

/// Temperature bands (after altitude cooling).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum TempBand {
    Frozen,
    Cold,
    Temperate,
    Warm,
    Hot,
}

/// Humidity bands. The cut points split the humidity noise into roughly
/// equal fifths (its standard deviation is ~0.39).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum HumidBand {
    Arid,
    Dry,
    Medium,
    Wet,
    VeryWet,
}

impl TempBand {
    pub fn of(t: f64) -> Self {
        match t {
            t if t < -0.45 => TempBand::Frozen,
            t if t < -0.15 => TempBand::Cold,
            t if t < 0.18 => TempBand::Temperate,
            t if t < 0.48 => TempBand::Warm,
            _ => TempBand::Hot,
        }
    }
}

impl HumidBand {
    pub fn of(h: f64) -> Self {
        match h {
            h if h < -0.33 => HumidBand::Arid,
            h if h < -0.1 => HumidBand::Dry,
            h if h < 0.1 => HumidBand::Medium,
            h if h < 0.33 => HumidBand::Wet,
            _ => HumidBand::VeryWet,
        }
    }
}

/// Altitude above which temperature starts to drop.
const LAPSE_START: f64 = 15.0;
/// Blocks of climb per 1.0 of temperature lost. At 160 blocks per unit, a
/// 125-block summit is about 0.7 colder than the valley below it, enough
/// to turn temperate into frozen.
const LAPSE_BLOCKS: f64 = 160.0;

/// Temperature at a surface height, cooled by altitude.
pub fn effective_temperature(shape: &Shape, surface_y: i32) -> f64 {
    // Altitude in default-world blocks, so a 9 km summit in a tall world
    // is as cold as a 200-block summit in a default one.
    let alt = shape.normalized_y(surface_y as f64) - SEA_LEVEL as f64;
    shape.climate.temperature - (alt - LAPSE_START).max(0.0) / LAPSE_BLOCKS
}

/// Offsets added to a column's climate before its biome is chosen (see
/// "Transitions" in the module docs). `region` is the threshold the
/// column's region strength must exceed for a regional biome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Jitter {
    pub t: f64,
    pub h: f64,
    pub w: f64,
    /// Default-world blocks added to the altitude used by mountain rules.
    pub alt: f64,
    pub region: f64,
}

impl Jitter {
    /// No dither: the biome of the exact climate.
    pub const NONE: Jitter = Jitter { t: 0.0, h: 0.0, w: 0.0, alt: 0.0, region: 0.5 };
}

/// Half-widths of the transition zones, in climate units (and blocks of
/// altitude): the largest offset the dither applies. Temperature bands are
/// 0.3 wide and humidity bands ~0.23, so a zone covers about a third of a
/// band on each side of a border.
pub const DITHER_T: f64 = 0.07;
pub const DITHER_H: f64 = 0.08;
pub const DITHER_W: f64 = 0.16;
pub const DITHER_ALT: f64 = 7.0;

/// The dither field of a world (see "Transitions" in the module docs).
pub struct BiomeDither {
    a: OctaveNoise,
    b: OctaveNoise,
    seed: u64,
}

/// A hash → two values in [-1, 1).
#[inline]
fn white(h: u64) -> (f64, f64) {
    let f = |v: u64| (v & 0xFFFF) as f64 / 32768.0 - 1.0;
    (f(h), f(h >> 16))
}

impl BiomeDither {
    pub fn new(seed: u64) -> Self {
        // Patches 32 and 16 blocks across; the per-column grain adds the
        // finest scale.
        let n = |salt| OctaveNoise::new(derive_seed(seed, salt), &OctaveConfig::new(-5, &[1.0, 0.6]));
        Self { a: n("biome/dither_a"), b: n("biome/dither_b"), seed: derive_seed(seed, "biome/dither_white") }
    }

    /// The two patch fields at a column, spread to roughly fill [-1, 1].
    #[inline]
    fn patches(&self, x: i32, z: i32) -> (f64, f64) {
        let (fx, fz) = (x as f64, z as f64);
        ((self.a.sample_2d(fx, fz) * 2.6).clamp(-1.0, 1.0), (self.b.sample_2d(fx, fz) * 2.6).clamp(-1.0, 1.0))
    }

    #[inline]
    fn jitter(a: f64, b: f64) -> Jitter {
        Jitter {
            t: a * DITHER_T,
            h: b * DITHER_H,
            // Weirdness and altitude reuse the two fields, rotated, so
            // their patches don't line up with the others'.
            w: (0.7 * (a - b)).clamp(-1.0, 1.0) * DITHER_W,
            alt: (0.7 * (a + b)).clamp(-1.0, 1.0) * DITHER_ALT,
            region: 0.5 + 0.45 * (0.6 * a - 0.8 * b).clamp(-1.0, 1.0),
        }
    }

    /// Dither of the ground at a column: mostly patches, with a little
    /// per-column grain so patch edges aren't smooth curves.
    pub fn ground(&self, x: i32, z: i32) -> Jitter {
        let (a, b) = self.patches(x, z);
        let (wa, wb) = white(hash_position(self.seed, x, 0, z));
        Self::jitter(0.8 * a + 0.2 * wa, 0.8 * b + 0.2 * wb)
    }

    /// Dither of a tree standing at a column (`hash` is the tree's): half
    /// the ground's patches, half its own randomness, so species mix both
    /// in groves and tree by tree.
    pub fn tree(&self, x: i32, z: i32, hash: u64) -> Jitter {
        let (a, b) = self.patches(x, z);
        let (wa, wb) = white(hash >> 24);
        Self::jitter(0.5 * a + 0.5 * wa, 0.5 * b + 0.5 * wb)
    }
}

/// Picks the biome of a column from its exact climate (no dither).
/// `surface_y` is the highest solid block.
pub fn select(shape: &Shape, surface_y: i32) -> Biome {
    select_with(shape, surface_y, &Jitter::NONE)
}

/// Lowland biomes in climate space: target points (temperature, humidity)
/// on the scales of `TempBand` and `HumidBand`. A column takes the biome
/// of the nearest target (temperature distances weighted by `T_WEIGHT`).
/// Real climates for orientation (realistic worlds): temperature −0.45 is
/// −2 °C, −0.15 is 6 °C, 0.18 is 15 °C, 0.48 is 22 °C; humidity −0.33 is an
/// aridity index of 0.2 (desert edge), −0.1 is 0.5, 0.1 is 0.8, 0.33 is 1.5.
const LOWLAND: &[(Biome, f64, f64)] = {
    use Biome::*;
    &[
        // Polar and subpolar. Tundra and snowy plains are as cold as
        // −6 to −15 °C a year (−0.57 … −0.75), however wet: too cold for
        // trees; the boreal forest starts near −5 °C.
        (IceSheet, -1.25, -0.4),
        (IceSheet, -1.25, 0.4),
        (Tundra, -0.8, -0.5),
        (Tundra, -0.78, 0.05),
        (SnowyPlains, -0.86, 0.5),
        (SnowyTaiga, -0.56, 0.0),
        (SnowyTaiga, -0.56, 0.45),
        // Boreal.
        (Steppe, -0.3, -0.62),
        (Taiga, -0.32, -0.18),
        (Taiga, -0.32, 0.08),
        (OldGrowthTaiga, -0.3, 0.4),
        (TemperateRainforest, -0.22, 0.82),
        (TemperateRainforest, 0.05, 0.85),
        // Boreal–temperate ecotone: conifers and broadleaves together.
        (MixedForest, -0.16, 0.02),
        (MixedForest, -0.16, 0.3),
        // Temperate.
        (Steppe, 0.0, -0.6),
        (Plains, 0.02, -0.24),
        (AutumnForest, -0.05, 0.08),
        (AutumnForest, -0.05, 0.3),
        (Forest, 0.1, 0.0),
        (DarkForest, 0.12, 0.28),
        // Warm (subtropical).
        (Desert, 0.4, -0.62),
        (Chaparral, 0.33, -0.24),
        (Savanna, 0.42, -0.06),
        (Forest, 0.28, 0.08),
        (SparseJungle, 0.36, 0.24),
        (Jungle, 0.4, 0.5),
        // Hot (tropical).
        (Desert, 0.75, -0.62),
        (ThornScrub, 0.72, -0.24),
        (Savanna, 0.72, -0.08),
        (TropicalDryForest, 0.72, 0.08),
        (SparseJungle, 0.72, 0.22),
        (Jungle, 0.72, 0.42),
    ]
};

/// Temperature distances count this much more than humidity ones: the
/// temperature bands are narrower than the humidity bands they sit
/// beside on the chart.
const T_WEIGHT: f64 = 1.6;

/// The lowland biome of a climate: nearest target in climate space.
pub fn classify(t: f64, h: f64) -> Biome {
    let mut best = (f64::INFINITY, Biome::Plains);
    for &(b, bt, bh) in LOWLAND {
        let d = (T_WEIGHT * (t - bt)).powi(2) + (h - bh).powi(2);
        if d < best.0 {
            best = (d, b);
        }
    }
    best.1
}

/// The regional biome a host biome turns into inside a region of the
/// given flavour, if it is one the region's landscape grows from.
fn regional(b: Biome, flavour: Flavour, t: f64, e: f64) -> Option<Biome> {
    use Biome::*;
    let warm = t > 0.18;
    match flavour {
        Flavour::None => None,
        Flavour::Sakura => matches!(b, Forest | BirchForest | FlowerForest | AutumnForest | MixedForest | Meadow)
            .then_some(CherryGrove),
        Flavour::Pillars => (matches!(b, Karst | SparseJungle | Jungle | CloudForest | TropicalDryForest)
            || (warm && b == Forest))
            .then_some(PillarKarst),
        Flavour::Baobab => matches!(b, Savanna | ThornScrub | TropicalDryForest).then_some(BaobabSavanna),
        Flavour::Redwood => matches!(b, TemperateRainforest | OldGrowthTaiga | MixedForest).then_some(RedwoodForest),
        Flavour::Bamboo => (matches!(b, Jungle | SparseJungle | Karst | TropicalDryForest) || (warm && b == Forest))
            .then_some(BambooForest),
        Flavour::Outback => {
            (matches!(b, Desert | RockyDesert | ThornScrub) || (warm && b == Chaparral)).then_some(Outback)
        }
        // Salt pans fill flat desert basins.
        Flavour::Salar => (matches!(b, Desert | RockyDesert | Steppe | Outback) && e > 0.2).then_some(SaltFlats),
        Flavour::Lavender => {
            (matches!(b, Plains | FlowerPlains | Chaparral) || (t > 0.0 && b == Steppe)).then_some(LavenderFields)
        }
    }
}

/// Picks the biome of a column whose climate is offset by `j` (see
/// "Transitions" in the module docs). `surface_y` is the highest solid
/// block.
pub fn select_with(shape: &Shape, surface_y: i32, j: &Jitter) -> Biome {
    use Biome::*;
    use HumidBand::*;
    use TempBand::*;

    let cl = &shape.climate;
    // `alt` is the true height above the water, used for things that keep
    // their real size in every world (beaches, shallow water). `alt_n` is
    // the height a default world would have here, for mountain rules.
    // Under water means under this column's water: the sea, or a river or
    // lake above sea level in realistic worlds. A regional pillar doesn't
    // count: its top belongs to the landscape at its foot.
    let ground_y = surface_y - shape.pillar as i32;
    let alt = ground_y - shape.water;
    let alt_n = shape.normalized_y(ground_y as f64) - SEA_LEVEL as f64 + j.alt;
    let t = effective_temperature(shape, ground_y) + j.t;
    let hum = cl.humidity + j.h;
    let tb = TempBand::of(t);
    let hb = HumidBand::of(hum);
    let (c, e, pv) = (cl.continentalness, cl.erosion, cl.peaks_valleys);
    let w = cl.weirdness + j.w;
    // Mangroves need a warm (about 20 °C or more), not dry climate and
    // sea water: not in freshwater lakes above the sea.
    let mangrove = t > 0.4 && hum > -0.12 && shape.water == SEA_LEVEL && shape.island == 0.0;

    // 1. Under water.
    if surface_y < shape.water {
        if shape.river > 0.3 {
            return if tb == Frozen { FrozenRiver } else { River };
        }
        if mangrove && alt >= -4 && c > -0.22 {
            return MangroveSwamp; // mangroves stand in shallow tropical water
        }
        if c > 0.05 {
            return Lake; // an inland basin below sea level
        }
        let deep = c < -0.45 && shape.island < 0.05;
        return match (tb, deep) {
            (Frozen, false) => FrozenOcean,
            (Frozen, true) => DeepFrozenOcean,
            (Cold, false) => ColdOcean,
            (Cold, true) => DeepColdOcean,
            (Temperate, false) => Ocean,
            (Temperate, true) => DeepOcean,
            (Warm, false) => LukewarmOcean,
            (Warm, true) | (Hot, true) => DeepLukewarmOcean,
            (Hot, false) => WarmOcean,
        };
    }

    // 2. Oceanic islands: lush mushroom fields where wet, bare volcanic
    //    rock where dry.
    if shape.island > 0.02 {
        return if cl.humidity > 0.0 { MushroomFields } else { Volcanic };
    }

    // A regional pillar's walls and top belong to its pillar forest.
    if shape.pillar > 0.5 {
        return PillarKarst;
    }

    // 3. Coast.
    let rugged = e < -0.3;
    if c < 0.0 && alt_n <= 14.0 && rugged {
        return if tb <= Cold { Fjord } else { StonyShore };
    }
    // Estuaries: mangroves line warm river mouths too.
    if mangrove && shape.river > 0.15 && alt <= 3 && c < 0.1 {
        return MangroveSwamp;
    }
    if c < -0.03 && alt <= 7 {
        let flat = e > 0.15;
        let beach = match (tb, hb) {
            (Frozen, _) => SnowyBeach,
            // Mangrove forest on flat, sheltered tropical coasts.
            _ if mangrove && e > 0.05 => MangroveSwamp,
            (Warm | Hot, Arid) => SaltFlats, // coastal sabkha
            // Mud flats and salt marsh where flat, humid coasts shelve
            // gently (Wadden Sea, Bay of Fundy).
            (Temperate, Medium | Wet | VeryWet) if e > 0.35 && w < -0.2 => TidalFlats,
            // Storm-sorted gravel on cool, wet coasts (Britain, Norway,
            // British Columbia).
            (Cold, _) | (Temperate, Wet | VeryWet) if w > -0.15 => ShingleBeach,
            // Coral sand and palms in the humid tropics.
            (Warm | Hot, Medium | Wet | VeryWet) if w > -0.35 => TropicalBeach,
            // Wide dune belts behind flat, dry-ish coasts (the Landes,
            // Namib coast, Outer Banks).
            (_, Arid | Dry | Medium) if flat && w < 0.05 => CoastalDunes,
            _ => Beach,
        };
        // Dunes pile up a few blocks higher than a beach reaches, and
        // mangroves stand on the low flats behind the shore.
        let reach = match beach {
            CoastalDunes => 7,
            MangroveSwamp => 4,
            _ => 3,
        };
        if alt <= reach {
            return beach;
        }
    }

    // 4. Mountains.
    let summit = alt_n > 125.0 || (alt_n > 85.0 && pv > 0.5 && e < -0.2);
    if summit {
        return match tb {
            Frozen | Cold if w > 0.0 => JaggedPeaks,
            Frozen | Cold => FrozenPeaks,
            _ => StonyPeaks,
        };
    }
    // Ice caps bury everything below the peaks where it is bitterly cold
    // (Antarctica, Greenland).
    if t < -1.0 {
        return IceSheet;
    }
    let base = if alt_n > 55.0 {
        match (tb, hb) {
            (Frozen | Cold, _) if t < -0.3 => Some(if hum > 0.0 { Grove } else { SnowySlopes }),
            // Tropical mountains: cloud forest on the wet slopes, páramo
            // grassland above it (the Andes, East Africa).
            (Warm | Hot, Medium | Wet | VeryWet) if alt_n > 80.0 => Some(Paramo),
            (Warm | Hot, Wet | VeryWet) => Some(CloudForest),
            (_, Arid | Dry) if shape.roughness > 2.5 => Some(WindsweptHills),
            (Temperate, Dry | Medium | Wet) if e > -0.35 => Some(Meadow),
            _ => None,
        }
    } else {
        None
    };
    // Heather moorland on wet, cool uplands (Pennines, Scottish Highlands).
    let base = base.or_else(|| {
        (alt_n > 30.0 && matches!(tb, Cold | Temperate) && hb >= Wet && w < -0.25 && e > -0.3).then_some(Moor)
    });

    // 5. Lowlands: nearest point in climate space, then sister biomes and
    //    waterlogged ground.
    let b = base.unwrap_or_else(|| {
        let swampy = e > 0.35 && alt_n < 6.0;
        let boggy = e > 0.3 && alt_n < 12.0 && w > 0.2;
        match classify(t, hum) {
            // Peat bogs where the boreal forest is flat and waterlogged.
            Taiga | OldGrowthTaiga | MixedForest if boggy && tb == Cold => Bog,
            Forest if tb == Temperate && w < 0.0 => BirchForest,
            DarkForest if w < 0.0 => FlowerForest,
            Plains if w > 0.4 => FlowerPlains,
            TemperateRainforest if swampy && tb >= Temperate => Swamp,
            // Stony desert (reg, hamada) beside the sand seas, badlands on
            // eroded hot uplands.
            Desert if tb == Hot && e < 0.0 => Badlands,
            Desert if w < -0.35 => RockyDesert,
            SparseJungle if pv > 0.1 && alt_n > 6.0 && tb == Warm => Karst,
            Savanna if tb == Hot && w < -0.3 && hb >= Medium => SparseJungle,
            // Seasonally flooded forest on flat, wet lowlands (várzea).
            Jungle | SparseJungle if swampy && tb == Hot && w > 0.15 => FloodedJungle,
            Jungle if swampy && tb == Warm => Swamp,
            other => other,
        }
    });

    // 6. Regions.
    if shape.region.flavour != Flavour::None && shape.region.strength as f64 > j.region {
        if let Some(r) = regional(b, shape.region.flavour, t, e) {
            return r;
        }
    }
    b
}

impl Biome {
    pub fn is_ocean(self) -> bool {
        use Biome::*;
        matches!(
            self,
            Ocean
                | DeepOcean
                | WarmOcean
                | LukewarmOcean
                | DeepLukewarmOcean
                | ColdOcean
                | DeepColdOcean
                | FrozenOcean
                | DeepFrozenOcean
        )
    }

    /// Biomes whose surface is (at least partly) water.
    pub fn is_water(self) -> bool {
        self.is_ocean() || matches!(self, Biome::River | Biome::FrozenRiver | Biome::Lake)
    }

    /// Water at sea level freezes over in these biomes.
    pub fn freezes(self) -> bool {
        matches!(self, Biome::FrozenOcean | Biome::DeepFrozenOcean | Biome::FrozenRiver)
    }

    /// Name with spaces and a capital letter, for menus: "Birch forest".
    pub fn display_name(self) -> String {
        let n = self.name().replace('_', " ");
        let mut c = n.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Climate;

    fn shape(c: f64, e: f64, w: f64, t: f64, h: f64) -> Shape {
        let pv = 1.0 - ((3.0 * w.abs()) - 2.0).abs();
        Shape {
            climate: Climate {
                continentalness: c,
                erosion: e,
                weirdness: w,
                peaks_valleys: pv,
                temperature: t,
                humidity: h,
            },
            height: 70.0,
            roughness: 1.0,
            river: 0.0,
            island: 0.0,
            vscale: 1.0,
            water: SEA_LEVEL,
            ..Shape::default()
        }
    }

    #[test]
    fn ids_and_names_are_unique() {
        for (i, b) in Biome::ALL.iter().enumerate() {
            assert_eq!(*b as usize, i);
            for other in &Biome::ALL[i + 1..] {
                assert_ne!(b.name(), other.name());
            }
        }
        assert_eq!(Biome::default(), Biome::Plains);
        assert_eq!(Biome::BirchForest.display_name(), "Birch forest");
    }

    #[test]
    fn whittaker_corners() {
        let at = |t, h| select(&shape(0.3, 0.5, 0.3, t, h), SEA_LEVEL + 10);
        assert_eq!(at(-0.8, -0.8), Biome::Tundra);
        assert_eq!(at(0.8, -0.8), Biome::Desert);
        assert_eq!(at(0.8, 0.8), Biome::Jungle);
        assert_eq!(at(-0.3, 0.8), Biome::TemperateRainforest);
        assert_eq!(at(0.0, -0.2), Biome::Plains);
    }

    #[test]
    fn water_and_altitude_rules() {
        let s = shape(-0.7, 0.0, 0.3, 0.0, 0.0);
        assert_eq!(select(&s, SEA_LEVEL - 40), Biome::DeepOcean);
        let s = shape(-0.7, 0.0, 0.3, -0.8, 0.0);
        assert_eq!(select(&s, SEA_LEVEL - 40), Biome::DeepFrozenOcean);
        // A temperate column becomes frozen high up because of the lapse rate.
        let s = shape(0.5, -0.8, 0.6, 0.1, 0.0);
        assert!(matches!(select(&s, SEA_LEVEL + 180), Biome::JaggedPeaks | Biome::FrozenPeaks));
        assert_eq!(select(&s, SEA_LEVEL + 180), Biome::JaggedPeaks);
    }

    #[test]
    fn altitude_rules_scale_with_tall_worlds() {
        // The same column in a 46× world: a summit 46× higher is still a
        // summit, and is as cold as the default one.
        let mut s = shape(0.5, -0.8, 0.6, 0.1, 0.0);
        let normal = select(&s, SEA_LEVEL + 180);
        let t = effective_temperature(&s, SEA_LEVEL + 180);
        s.vscale = 46.0;
        assert_eq!(select(&s, SEA_LEVEL + 180 * 46), normal);
        assert!((effective_temperature(&s, SEA_LEVEL + 180 * 46) - t).abs() < 1e-9);
        // Beaches keep their real size.
        let b = shape(-0.05, 0.3, 0.3, 0.0, 0.0);
        let mut tall = b;
        tall.vscale = 46.0;
        assert_eq!(select(&b, SEA_LEVEL + 2), Biome::Beach);
        assert_eq!(select(&tall, SEA_LEVEL + 2), Biome::Beach);
    }

    #[test]
    fn temperature_cools_with_altitude() {
        let s = shape(0.3, 0.0, 0.0, 0.2, 0.0);
        assert_eq!(effective_temperature(&s, SEA_LEVEL + 10), 0.2);
        assert!((effective_temperature(&s, SEA_LEVEL + 175) - (0.2 - 1.0)).abs() < 1e-9);
    }
}
