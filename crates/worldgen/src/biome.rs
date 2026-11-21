//! Biomes: which kind of landscape a column belongs to.
//!
//! ## How a biome is chosen
//!
//! Every column already has six smooth "climate" values from
//! [`crate::terrain`]: continentalness, erosion, weirdness (and its folded
//! form, peaks & valleys), temperature and humidity. The biome is a pure
//! function of those values, plus the column's final surface height. The
//! same seed therefore always gives the same biome map, and neighbouring
//! columns almost always share a biome because their inputs are almost
//! equal.
//!
//! Minecraft chooses the biome with a nearest-neighbour search in that
//! parameter space. We use an explicit decision tree instead. It gives the
//! same smooth, deterministic result, and each rule reads like geography:
//!
//! 1. **Water.** If the surface is below sea level the column is ocean,
//!    river or lake. Depth and temperature choose the ocean variant.
//! 2. **Oceanic islands** (rare cones raised from the deep sea) are volcanic
//!    or mushroom fields.
//! 3. **Coast.** Low land next to the sea is beach, snowy beach, mangrove,
//!    salt flats (a sabkha), stony shore or fjord.
//! 4. **Altitude.** Temperature drops with height, like the real lapse rate
//!    of about 6.5 °C per km. High summits become frozen, jagged or stony
//!    peaks. High slopes become groves, snowy slopes, meadows, cloud forest
//!    or windswept hills.
//! 5. **Everything else** follows a Whittaker diagram, the classic ecology
//!    chart that places biomes on a temperature × rainfall grid. Tundra and
//!    snowy plains sit at the cold end, deserts and badlands at hot and dry,
//!    jungles at hot and wet, and the temperate forests in between. The sign
//!    of weirdness picks between sister biomes (forest or birch forest, for
//!    example), so variants come in natural patches.
//!
//! Real-world biomes that Minecraft doesn't have are marked
//! [`Origin::RealWorld`] in the table below.

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
    WindsweptHills      "windswept_hills"      map [ 96,  96,  96] grass [138, 182, 137] Minecraft,
    // --- Cold lowlands -------------------------------------------------
    Tundra              "tundra"               map [170, 160, 130] grass [150, 155, 100] RealWorld,
    SnowyPlains         "snowy_plains"         map [255, 255, 255] grass [128, 180, 151] Minecraft,
    SnowyTaiga          "snowy_taiga"          map [ 49,  85,  74] grass [128, 180, 151] Minecraft,
    Taiga               "taiga"                map [ 11, 102,  89] grass [134, 183, 131] Minecraft,
    OldGrowthTaiga      "old_growth_taiga"     map [ 89, 102,  81] grass [124, 170, 110] Minecraft,
    TemperateRainforest "temperate_rainforest" map [ 20,  80,  40] grass [ 70, 150,  60] RealWorld,
    // --- Temperate lowlands --------------------------------------------
    Steppe              "steppe"               map [200, 190, 120] grass [175, 170,  95] RealWorld,
    Forest              "forest"               map [  5, 102,  33] grass [ 89, 174,  48] Minecraft,
    BirchForest         "birch_forest"         map [ 48, 116,  68] grass [116, 183,  72] Minecraft,
    DarkForest          "dark_forest"          map [ 64,  81,  26] grass [ 80, 122,  50] Minecraft,
    FlowerForest        "flower_forest"        map [ 45, 142,  73] grass [100, 180,  60] Minecraft,
    Swamp               "swamp"                map [  7, 249, 178] grass [106, 112,  57] Minecraft,
    // --- Warm and hot lowlands -----------------------------------------
    Chaparral           "chaparral"            map [170, 140,  70] grass [160, 160,  85] RealWorld,
    Savanna             "savanna"              map [189, 178,  95] grass [191, 183,  85] Minecraft,
    Desert              "desert"               map [250, 148,  24] grass [191, 183,  85] Minecraft,
    Badlands            "badlands"             map [217,  69,  21] grass [144, 129,  77] Minecraft,
    Karst               "karst"                map [140, 200, 160] grass [ 90, 175,  60] RealWorld,
    SparseJungle        "sparse_jungle"        map [ 98, 139,  23] grass [100, 185,  50] Minecraft,
    Jungle              "jungle"               map [ 83, 123,   9] grass [ 70, 190,  30] Minecraft,
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

/// Picks the biome of a column. `surface_y` is the highest solid block.
pub fn select(shape: &Shape, surface_y: i32) -> Biome {
    use Biome::*;
    use HumidBand::*;
    use TempBand::*;

    let cl = &shape.climate;
    // `alt` is the true height above the sea, used for things that keep
    // their real size in every world (beaches, shallow water). `alt_n` is
    // the height a default world would have here, for mountain rules.
    // Under water means under this column's water: the sea, or a river or
    // lake above sea level in realistic worlds.
    let alt = surface_y - shape.water;
    let alt_n = shape.normalized_y(surface_y as f64) - SEA_LEVEL as f64;
    let t = effective_temperature(shape, surface_y);
    let tb = TempBand::of(t);
    let hb = HumidBand::of(cl.humidity);
    let (c, e, w, pv) = (cl.continentalness, cl.erosion, cl.weirdness, cl.peaks_valleys);

    // 1. Under water.
    if alt < 0 {
        if shape.river > 0.3 {
            return if tb == Frozen { FrozenRiver } else { River };
        }
        if tb == Hot && hb >= Wet && alt >= -4 && c > -0.22 && shape.island == 0.0 {
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

    // 3. Coast.
    let rugged = e < -0.3;
    if c < 0.0 && alt_n <= 14.0 && rugged {
        return if tb <= Cold { Fjord } else { StonyShore };
    }
    if c < -0.03 && alt <= 3 {
        return match (tb, hb) {
            (Frozen, _) => SnowyBeach,
            (Hot, Wet | VeryWet) => MangroveSwamp,
            (Warm | Hot, Arid) => SaltFlats, // coastal sabkha
            _ => Beach,
        };
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
    if alt_n > 55.0 {
        match (tb, hb) {
            (Frozen | Cold, _) if t < -0.3 => return if cl.humidity > 0.0 { Grove } else { SnowySlopes },
            (Warm | Hot, Wet | VeryWet) => return CloudForest,
            (_, Arid | Dry) if shape.roughness > 2.5 => return WindsweptHills,
            (Temperate, Dry | Medium | Wet) if e > -0.35 => return Meadow,
            _ => {}
        }
    }

    // 5. Lowlands: Whittaker grid.
    let swampy = e > 0.35 && alt_n < 6.0;
    match (tb, hb) {
        (Frozen, Arid | Dry) => Tundra,
        (Frozen, Medium) => SnowyPlains,
        (Frozen, _) => SnowyTaiga,

        (Cold, Arid) => Steppe,
        (Cold, Dry | Medium) => Taiga,
        (Cold, Wet) => OldGrowthTaiga,
        (Cold, VeryWet) => TemperateRainforest,

        (Temperate, Arid) => Steppe,
        (Temperate, Dry) => Plains,
        (Temperate, Medium) => {
            if w > 0.0 {
                Forest
            } else {
                BirchForest
            }
        }
        (Temperate, Wet) => {
            if w > 0.0 {
                DarkForest
            } else {
                FlowerForest
            }
        }
        (Temperate, VeryWet) => {
            if swampy {
                Swamp
            } else {
                TemperateRainforest
            }
        }

        (Warm, Arid) => Desert,
        (Warm, Dry) => Chaparral,
        (Warm, Medium) => {
            if w > 0.0 {
                Savanna
            } else {
                Forest
            }
        }
        (Warm, Wet) => {
            if pv > 0.1 && alt_n > 6.0 {
                Karst
            } else {
                SparseJungle
            }
        }
        (Warm, VeryWet) => {
            if swampy {
                Swamp
            } else {
                Jungle
            }
        }

        (Hot, Arid) => {
            if e < 0.0 {
                Badlands
            } else {
                Desert
            }
        }
        (Hot, Dry) => Savanna,
        (Hot, Medium) => {
            if w > 0.0 {
                Savanna
            } else {
                SparseJungle
            }
        }
        (Hot, Wet | VeryWet) => Jungle,
    }
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
