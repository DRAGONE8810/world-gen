//! Grass and foliage colour that follows the climate smoothly.
//!
//! A biome is a *discrete* choice made from continuous climate values
//! (temperature, humidity): crossing a threshold switches the biome from
//! one column to the next. Colouring grass by biome therefore draws a hard
//! line wherever the biome changes, which nothing in nature does. Here the
//! grass colour is instead a continuous function of the same climate (a
//! "colour map" over temperature × humidity, the idea behind Minecraft's
//! grass colour map): from orbit, forests fade into steppe and steppe into
//! desert gradually, while the biome border is still visible where the
//! ground or the trees change.
//!
//! The colour is quantized into a small palette (`Tint`, one byte): the
//! mesher can only merge faces of equal colour, so a few hundred shades
//! keep large merged faces while the steps stay too small to see. A few
//! biomes with a colour of their own that no climate explains (swamps,
//! mushroom islands, badlands, volcanic ash, heather moors) keep it.

use crate::biome::{effective_temperature, Biome};
use crate::spline::lerp;
use crate::terrain::Shape;

/// Temperature steps of the palette.
const T_STEPS: usize = 13;
/// Humidity steps of the palette.
const H_STEPS: usize = 19;
/// Palette entries used by the climate colour map.
const CLIMATE: usize = T_STEPS * H_STEPS;
/// Temperature range of the map (effective temperature, after cooling by
/// altitude); colder or hotter columns use the edge colour.
const T_RANGE: (f64, f64) = (-0.75, 0.8);
/// Humidity range of the map.
const H_RANGE: (f64, f64) = (-0.6, 0.6);

/// Biomes whose grass keeps a colour of its own.
const SPECIAL: [Biome; 6] =
    [Biome::Swamp, Biome::MangroveSwamp, Biome::MushroomFields, Biome::Badlands, Biome::Volcanic, Biome::Moor];

/// Control colours of the climate map, rows from frozen to hot, columns
/// from arid to very wet. They are the grass colours of the biome that
/// typically grows at each point, so a column deep inside a biome looks
/// as before and only the borders blend.
const MAP: [[[u8; 3]; 5]; 5] = [
    // Frozen: tundra → snowy taiga.
    [[150, 155, 100], [140, 165, 120], [128, 180, 151], [122, 178, 138], [112, 174, 120]],
    // Cold: cold steppe → taiga → temperate rainforest.
    [[165, 165, 100], [150, 175, 115], [134, 183, 131], [124, 170, 110], [90, 160, 85]],
    // Temperate: steppe → plains → forest.
    [[175, 170, 95], [140, 185, 85], [121, 192, 90], [89, 174, 48], [80, 160, 50]],
    // Warm: savanna → chaparral → karst forest.
    [[191, 183, 85], [160, 160, 85], [130, 185, 70], [100, 180, 60], [90, 175, 60]],
    // Hot: desert → savanna → jungle.
    [[191, 183, 85], [180, 178, 85], [100, 185, 50], [70, 190, 30], [70, 190, 30]],
];

/// Grass colour index: an entry of the climate map or a special biome.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash, PartialOrd, Ord)]
pub struct Tint(pub u8);

const _: () = assert!(CLIMATE + SPECIAL.len() <= 256);

impl Tint {
    /// Number of distinct tints.
    pub const COUNT: usize = CLIMATE + SPECIAL.len();

    /// The tint of a column: its biome, climate and surface height (the
    /// temperature is cooled by altitude, like the biome choice, so
    /// mountain meadows get mountain colours).
    pub fn of(biome: Biome, shape: &Shape, surface_y: i32) -> Tint {
        if let Some(i) = SPECIAL.iter().position(|&b| b == biome) {
            return Tint((CLIMATE + i) as u8);
        }
        // Measured at the foot of a regional pillar, like the biome.
        let t = effective_temperature(shape, surface_y - shape.pillar as i32);
        let h = shape.climate.humidity;
        let step = |v: f64, (lo, hi): (f64, f64), n: usize| {
            (((v - lo) / (hi - lo)).clamp(0.0, 1.0) * (n - 1) as f64).round() as usize
        };
        Tint((step(t, T_RANGE, T_STEPS) * H_STEPS + step(h, H_RANGE, H_STEPS)) as u8)
    }

    /// Grass colour.
    pub fn color(self) -> [u8; 3] {
        let i = self.0 as usize;
        if i >= CLIMATE {
            return SPECIAL[(i - CLIMATE).min(SPECIAL.len() - 1)].grass_color();
        }
        // Bilinear interpolation of the 5 × 5 control colours.
        let (ti, hi) = (i / H_STEPS, i % H_STEPS);
        let tf = ti as f64 / (T_STEPS - 1) as f64 * 4.0;
        let hf = hi as f64 / (H_STEPS - 1) as f64 * 4.0;
        let (t0, h0) = ((tf as usize).min(3), (hf as usize).min(3));
        let (ft, fh) = (tf - t0 as f64, hf - h0 as f64);
        std::array::from_fn(|c| {
            let at = |t: usize, h: usize| MAP[t][h][c] as f64;
            let v = lerp(ft, lerp(fh, at(t0, h0), at(t0, h0 + 1)), lerp(fh, at(t0 + 1, h0), at(t0 + 1, h0 + 1)));
            v.round() as u8
        })
    }

    /// Colour of tinted (oak-type) leaves: a darker shade of the grass.
    pub fn foliage_color(self) -> [u8; 3] {
        self.color().map(|c| (c as f32 * 0.72) as u8)
    }
}

/// The tree canopy of a biome as seen from above: the share of the ground
/// it covers and its colour. Maps sample columns without trees, so this
/// is what makes a forest look like a forest from orbit (dark green, not
/// the brown of the podzol under it).
pub fn canopy(biome: Biome, tint: Tint) -> (f64, [u8; 3]) {
    use crate::block::blocks;
    use crate::trees::{biome_trees, TreeKind as T};
    let (chance, mix) = biome_trees(biome);
    let (mut cover, mut weight, mut rgb) = (0.0, 0.0, [0.0f64; 3]);
    for &(kind, w) in mix {
        // Crown area relative to the 5 × 5 tree cell, and leaf colour.
        let (crown, color) = match kind {
            T::Oak | T::Bush => (if kind == T::Bush { 0.35 } else { 0.9 }, tint.foliage_color()),
            T::Birch => (0.7, blocks::BIRCH_LEAVES.info().color),
            T::Spruce | T::TallSpruce => (0.6, blocks::SPRUCE_LEAVES.info().color),
            T::Jungle => (1.0, blocks::JUNGLE_LEAVES.info().color),
            T::Acacia => (0.9, blocks::ACACIA_LEAVES.info().color),
            T::DarkOak => (1.0, blocks::DARK_OAK_LEAVES.info().color),
            T::Mangrove => (0.9, blocks::MANGROVE_LEAVES.info().color),
            T::Cactus => (0.04, blocks::CACTUS.info().color),
            T::RedMushroom => (0.6, blocks::RED_MUSHROOM_BLOCK.info().color),
            T::BrownMushroom => (0.6, blocks::BROWN_MUSHROOM_BLOCK.info().color),
            // Autumn maples: red, two in four orange, yellow.
            T::Maple => (0.95, [204, 114, 40]),
            T::AutumnBirch => (0.7, blocks::YELLOW_LEAVES.info().color),
            T::Cherry => (0.95, blocks::CHERRY_LEAVES.info().color),
            T::Palm => (0.45, blocks::PALM_LEAVES.info().color),
            T::Baobab => (0.8, blocks::ACACIA_LEAVES.info().color),
            T::Redwood => (0.7, blocks::SPRUCE_LEAVES.info().color),
            T::Bamboo => (0.95, blocks::BAMBOO_LEAVES.info().color),
        };
        let w = w as f64;
        cover += w * crown;
        weight += w;
        for (c, v) in rgb.iter_mut().zip(color) {
            *c += w * crown * v as f64;
        }
    }
    if weight == 0.0 || cover == 0.0 {
        return (0.0, [0, 0, 0]);
    }
    let color = rgb.map(|c| (c / cover).round() as u8);
    ((chance * cover / weight * 1.3).min(0.95), color)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neighbouring palette entries differ by a few levels at most, so the
    /// quantization steps can't be seen; the map spans the old biome
    /// colours (straw-yellow deserts to deep-green jungles).
    #[test]
    fn palette_steps_are_small_and_span_the_biomes() {
        let dist = |a: [u8; 3], b: [u8; 3]| a.iter().zip(&b).map(|(&x, &y)| (x as i32 - y as i32).abs()).max().unwrap();
        for t in 0..T_STEPS {
            for h in 0..H_STEPS {
                let c = Tint((t * H_STEPS + h) as u8).color();
                if h + 1 < H_STEPS {
                    let n = Tint((t * H_STEPS + h + 1) as u8).color();
                    assert!(dist(c, n) <= 20, "humidity step {t},{h}: {c:?} → {n:?}");
                }
                if t + 1 < T_STEPS {
                    let n = Tint(((t + 1) * H_STEPS + h) as u8).color();
                    assert!(dist(c, n) <= 20, "temperature step {t},{h}: {c:?} → {n:?}");
                }
            }
        }
        // Hot and arid → desert straw; hot and wet → jungle green.
        assert_eq!(Tint(((T_STEPS - 1) * H_STEPS) as u8).color(), [191, 183, 85]);
        assert_eq!(Tint((T_STEPS * H_STEPS - 1) as u8).color(), [70, 190, 30]);
        for (i, b) in SPECIAL.iter().enumerate() {
            assert_eq!(Tint((CLIMATE + i) as u8).color(), b.grass_color());
        }
    }
}
