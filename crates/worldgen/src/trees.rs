//! Trees and other tall plants (cactus, giant mushrooms).
//!
//! ## Where trees go
//!
//! Scattering trees with plain per-block randomness gives clumps and gaps
//! and lets trunks touch. We use a **jittered grid** instead, a cheap
//! relative of Poisson-disk sampling: the world is cut into 5×5-column
//! cells, and each cell holds at most one tree candidate at a hashed
//! position inside it. Candidates are therefore spread evenly but not on a
//! visible grid. The biome then decides whether the candidate grows (its
//! density) and which species it is.
//!
//! Everything is a hash of the cell coordinates and the seed, with no state.
//! That is what makes trees seamless across chunk borders. A chunk looks at
//! every cell within `TREE_REACH` blocks of its edges, so the canopy of a
//! tree rooted in the next chunk is still drawn here. The neighbour's
//! ground height comes from `WorldGenerator::sample_column`, which tests
//! prove equals what that chunk itself generates.
//!
//! ## Overlaps
//!
//! When two trees overlap, the result must not depend on which chunk is
//! being built. Trees are therefore always processed in global cell order,
//! logs may replace leaves, and leaves only fill air. Each block's final
//! value then depends only on the ordered list of trees that touch it, and
//! that list is the same from every chunk.

use crate::biome::Biome;
use crate::block::{blocks, BlockId};
use crate::rng::{hash_position, Xoroshiro128PlusPlus};
use crate::settings::SEA_LEVEL;

/// Edge length of a jittered-grid cell, in blocks.
pub const TREE_CELL: i32 = 5;
/// The farthest any tree block reaches horizontally from its trunk origin.
pub const TREE_REACH: i32 = 4;
/// The highest any tree block reaches above the ground it stands on.
pub const TREE_HEADROOM: i32 = 30;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum TreeKind {
    Oak,
    Birch,
    Spruce,
    TallSpruce,
    Jungle,
    Acacia,
    DarkOak,
    Mangrove,
    Cactus,
    RedMushroom,
    BrownMushroom,
    Bush,
}

/// A resolved tree: species, trunk base (the first log above the ground)
/// and the hash that drives its random size and shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tree {
    pub kind: TreeKind,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub hash: u64,
}

/// Probability that a candidate grows, and the species mix (relative
/// weights), per biome.
pub fn biome_trees(biome: Biome) -> (f64, &'static [(TreeKind, u32)]) {
    use Biome::*;
    use TreeKind as T;
    match biome {
        Plains => (0.03, &[(T::Oak, 9), (T::Bush, 1)]),
        Meadow => (0.02, &[(T::Oak, 1), (T::Birch, 1)]),
        Forest => (0.6, &[(T::Oak, 8), (T::Birch, 2)]),
        FlowerForest => (0.35, &[(T::Oak, 1), (T::Birch, 1)]),
        BirchForest => (0.6, &[(T::Birch, 1)]),
        DarkForest => (0.9, &[(T::DarkOak, 16), (T::Oak, 3), (T::RedMushroom, 1), (T::BrownMushroom, 1)]),
        Swamp => (0.2, &[(T::Oak, 1)]),
        Taiga => (0.55, &[(T::Spruce, 1)]),
        OldGrowthTaiga => (0.65, &[(T::TallSpruce, 2), (T::Spruce, 1)]),
        SnowyTaiga => (0.4, &[(T::Spruce, 1)]),
        Grove => (0.45, &[(T::Spruce, 1)]),
        SnowyPlains => (0.015, &[(T::Spruce, 1)]),
        TemperateRainforest => (0.8, &[(T::TallSpruce, 5), (T::Spruce, 2), (T::Oak, 2)]),
        WindsweptHills => (0.06, &[(T::Spruce, 2), (T::Oak, 1)]),
        Tundra => (0.01, &[(T::Bush, 1)]),
        Steppe => (0.015, &[(T::Bush, 1)]),
        Chaparral => (0.2, &[(T::Bush, 4), (T::Oak, 1)]),
        Savanna => (0.07, &[(T::Acacia, 4), (T::Bush, 1)]),
        Desert => (0.035, &[(T::Cactus, 1)]),
        Badlands => (0.02, &[(T::Cactus, 1)]),
        Karst => (0.35, &[(T::Jungle, 1), (T::Oak, 1), (T::Bush, 1)]),
        SparseJungle => (0.3, &[(T::Jungle, 2), (T::Oak, 1), (T::Bush, 2)]),
        Jungle => (0.85, &[(T::Jungle, 7), (T::Bush, 3)]),
        CloudForest => (0.75, &[(T::Jungle, 3), (T::Oak, 2), (T::Bush, 1)]),
        MangroveSwamp => (0.6, &[(T::Mangrove, 1)]),
        MushroomFields => (0.05, &[(T::RedMushroom, 1), (T::BrownMushroom, 1)]),
        _ => (0.0, &[]),
    }
}

/// The candidate position in cell (cx, cz) and its hash. Positions stay one
/// block away from the high edges of the cell, so neighbouring trunks are
/// always at least two blocks apart.
pub fn cell_candidate(seed: u64, cx: i32, cz: i32) -> (i32, i32, u64) {
    let h = hash_position(seed, cx, 0, cz);
    let span = (TREE_CELL - 1) as u64;
    let x = cx * TREE_CELL + (h % span) as i32;
    let z = cz * TREE_CELL + ((h >> 8) % span) as i32;
    (x, z, h)
}

/// Decides whether the candidate grows, given the column it stands on.
/// `top` is the block at `surface_y`.
pub fn resolve(x: i32, z: i32, hash: u64, biome: Biome, surface_y: i32, top: BlockId, max_y: i32) -> Option<Tree> {
    let (density, kinds) = biome_trees(biome);
    if density <= 0.0 {
        return None;
    }
    let roll = ((hash >> 16) & 0xFFFF) as f64 / 65536.0;
    if roll >= density {
        return None;
    }
    let total: u32 = kinds.iter().map(|k| k.1).sum();
    let mut pick = ((hash >> 32) % total as u64) as u32;
    let mut kind = kinds[0].0;
    for &(k, wgt) in kinds {
        if pick < wgt {
            kind = k;
            break;
        }
        pick -= wgt;
    }

    let ground_ok = match kind {
        TreeKind::Cactus => top == blocks::SAND || top == blocks::RED_SAND,
        TreeKind::Mangrove => top == blocks::MUD || top == blocks::GRASS,
        TreeKind::RedMushroom | TreeKind::BrownMushroom => {
            matches!(top, blocks::MYCELIUM | blocks::GRASS | blocks::PODZOL)
        }
        _ => matches!(
            top,
            blocks::GRASS | blocks::DIRT | blocks::PODZOL | blocks::COARSE_DIRT | blocks::SNOW | blocks::MUD
        ),
    };
    if !ground_ok {
        return None;
    }
    // Only mangroves stand in water (up to 4 blocks deep).
    let min_ground = if kind == TreeKind::Mangrove { SEA_LEVEL - 5 } else { SEA_LEVEL };
    if surface_y < min_ground {
        return None;
    }
    let y = surface_y + 1;
    if y + TREE_HEADROOM > max_y {
        return None;
    }
    Some(Tree { kind, x, y, z, hash })
}

/// Calls `emit(x, y, z, block, is_log)` for every block of the tree, in
/// world coordinates. Pure: depends only on `tree`.
pub fn build(tree: &Tree, emit: &mut dyn FnMut(i32, i32, i32, BlockId, bool)) {
    let mut rng = Xoroshiro128PlusPlus::from_seed(tree.hash ^ 0x7265_6573);
    let (x0, y0, z0) = (tree.x, tree.y, tree.z);
    let mut log = |dx: i32, dy: i32, dz: i32, b: BlockId| emit(x0 + dx, y0 + dy, z0 + dz, b, true);
    let mut logs: Vec<(i32, i32, i32, BlockId)> = Vec::new();
    let mut leaves: Vec<(i32, i32, i32, BlockId)> = Vec::new();

    // Disc of leaves at height dy around (cx, cz), radius r (Euclidean),
    // optionally trimming the corners at random for a rounder silhouette.
    let disc = |leaves: &mut Vec<_>, rng: &mut Xoroshiro128PlusPlus, cx: f64, cz: f64, dy: i32, r: f64, b| {
        let ri = r.ceil() as i32 + 1;
        for dz in -ri..=ri {
            for dx in -ri..=ri {
                let (fx, fz) = (dx as f64 - cx, dz as f64 - cz);
                let d2 = fx * fx + fz * fz;
                if d2 > r * r + 0.5 {
                    continue;
                }
                // Outermost ring: drop some leaves so canopies aren't perfect.
                if d2 > (r - 0.8).max(0.0).powi(2) + 0.5 && rng.next_bounded(4) == 0 {
                    continue;
                }
                leaves.push((dx, dy, dz, b));
            }
        }
    };

    match tree.kind {
        TreeKind::Oak | TreeKind::Birch => {
            let (lb, fb) = if tree.kind == TreeKind::Oak {
                (blocks::OAK_LOG, blocks::OAK_LEAVES)
            } else {
                (blocks::BIRCH_LOG, blocks::BIRCH_LEAVES)
            };
            let h = if tree.kind == TreeKind::Oak { 4 + rng.next_bounded(3) } else { 5 + rng.next_bounded(3) } as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, lb));
            }
            let top = h - 1;
            disc(&mut leaves, &mut rng, 0.0, 0.0, top - 2, 2.3, fb);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top - 1, 2.3, fb);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top, 1.5, fb);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top + 1, 1.0, fb);
        }
        TreeKind::Spruce | TreeKind::TallSpruce => {
            let tall = tree.kind == TreeKind::TallSpruce;
            let h = if tall { 13 + rng.next_bounded(8) } else { 6 + rng.next_bounded(5) } as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::SPRUCE_LOG));
            }
            // Cone: radius grows downwards in a 1-2-1-2-3 rhythm, which reads
            // as layered branches.
            let crown_start = if tall { h / 2 } else { 2 };
            leaves.push((0, h, 0, blocks::SPRUCE_LEAVES));
            for (k, dy) in (crown_start..h).rev().enumerate() {
                let base = (k as f64 * 0.28).min(if tall { 2.3 } else { 2.6 });
                let r = if k % 2 == 0 { base + 0.3 } else { (base - 0.6).max(0.8) };
                disc(&mut leaves, &mut rng, 0.0, 0.0, dy, r.max(1.0), blocks::SPRUCE_LEAVES);
            }
        }
        TreeKind::Jungle => {
            let h = 8 + rng.next_bounded(7) as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::JUNGLE_LOG));
            }
            let top = h - 1;
            disc(&mut leaves, &mut rng, 0.0, 0.0, top - 1, 3.0, blocks::JUNGLE_LEAVES);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top, 2.6, blocks::JUNGLE_LEAVES);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top + 1, 1.6, blocks::JUNGLE_LEAVES);
            // A small side branch with its own tuft halfway up.
            if h > 10 {
                let (bx, bz) = [(1, 0), (-1, 0), (0, 1), (0, -1)][rng.next_bounded(4) as usize];
                let by = h / 2 + 1;
                logs.push((bx, by, bz, blocks::JUNGLE_LOG));
                disc(&mut leaves, &mut rng, bx as f64 * 1.5, bz as f64 * 1.5, by + 1, 1.5, blocks::JUNGLE_LEAVES);
            }
        }
        TreeKind::Acacia => {
            // Straight lower trunk, then one diagonal step, then a flat,
            // wide canopy: the umbrella shape of savanna trees.
            let h = 4 + rng.next_bounded(2) as i32;
            let (sx, sz) = [(1, 0), (-1, 0), (0, 1), (0, -1)][rng.next_bounded(4) as usize];
            for dy in 0..h - 2 {
                logs.push((0, dy, 0, blocks::ACACIA_LOG));
            }
            logs.push((sx, h - 2, sz, blocks::ACACIA_LOG));
            logs.push((sx, h - 1, sz, blocks::ACACIA_LOG));
            let (cx, cz) = (sx as f64, sz as f64);
            disc(&mut leaves, &mut rng, cx, cz, h, 2.8, blocks::ACACIA_LEAVES);
            disc(&mut leaves, &mut rng, cx, cz, h + 1, 1.5, blocks::ACACIA_LEAVES);
        }
        TreeKind::DarkOak => {
            // 2×2 trunk, broad low canopy.
            let h = 6 + rng.next_bounded(3) as i32;
            for dy in 0..h {
                for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    logs.push((dx, dy, dz, blocks::DARK_OAK_LOG));
                }
            }
            let top = h - 1;
            disc(&mut leaves, &mut rng, 0.5, 0.5, top - 1, 3.0, blocks::DARK_OAK_LEAVES);
            disc(&mut leaves, &mut rng, 0.5, 0.5, top, 3.0, blocks::DARK_OAK_LEAVES);
            disc(&mut leaves, &mut rng, 0.5, 0.5, top + 1, 2.0, blocks::DARK_OAK_LEAVES);
        }
        TreeKind::Mangrove => {
            // Trunk rises from the mud through the water; prop roots on all
            // four sides reach from the floor to just above the surface.
            let above = (SEA_LEVEL + 1 - y0).max(0);
            let h = above + 4 + rng.next_bounded(3) as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::MANGROVE_LOG));
            }
            let root_top = above.max(1);
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                for dy in -1..=root_top {
                    logs.push((dx, dy, dz, blocks::MANGROVE_ROOTS));
                }
            }
            let top = h - 1;
            disc(&mut leaves, &mut rng, 0.0, 0.0, top - 1, 2.5, blocks::MANGROVE_LEAVES);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top, 2.5, blocks::MANGROVE_LEAVES);
            disc(&mut leaves, &mut rng, 0.0, 0.0, top + 1, 1.5, blocks::MANGROVE_LEAVES);
        }
        TreeKind::Cactus => {
            let h = 2 + rng.next_bounded(3) as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::CACTUS));
            }
        }
        TreeKind::RedMushroom => {
            let h = 5 + rng.next_bounded(3) as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::MUSHROOM_STEM));
            }
            // Dome: a 3×3 roof and a ring hanging down three blocks.
            for dz in -1..=1 {
                for dx in -1..=1 {
                    logs.push((dx, h, dz, blocks::RED_MUSHROOM_BLOCK));
                }
            }
            for dy in h - 3..h {
                for dz in -2..=2i32 {
                    for dx in -2..=2i32 {
                        let ring = dx.abs() == 2 || dz.abs() == 2;
                        if ring && !(dx.abs() == 2 && dz.abs() == 2) {
                            logs.push((dx, dy, dz, blocks::RED_MUSHROOM_BLOCK));
                        }
                    }
                }
            }
        }
        TreeKind::BrownMushroom => {
            let h = 5 + rng.next_bounded(3) as i32;
            for dy in 0..h {
                logs.push((0, dy, 0, blocks::MUSHROOM_STEM));
            }
            for dz in -3..=3i32 {
                for dx in -3..=3i32 {
                    if !(dx.abs() == 3 && dz.abs() == 3) {
                        logs.push((dx, h, dz, blocks::BROWN_MUSHROOM_BLOCK));
                    }
                }
            }
        }
        TreeKind::Bush => {
            logs.push((0, 0, 0, blocks::OAK_LOG));
            disc(&mut leaves, &mut rng, 0.0, 0.0, 0, 1.5, blocks::OAK_LEAVES);
            disc(&mut leaves, &mut rng, 0.0, 0.0, 1, 1.0, blocks::OAK_LEAVES);
        }
    }

    // Logs first so that, within one tree, leaves never cover the trunk.
    for (dx, dy, dz, b) in logs {
        log(dx, dy, dz, b);
    }
    for (dx, dy, dz, b) in leaves {
        emit(x0 + dx, y0 + dy, z0 + dz, b, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [TreeKind; 12] = [
        TreeKind::Oak,
        TreeKind::Birch,
        TreeKind::Spruce,
        TreeKind::TallSpruce,
        TreeKind::Jungle,
        TreeKind::Acacia,
        TreeKind::DarkOak,
        TreeKind::Mangrove,
        TreeKind::Cactus,
        TreeKind::RedMushroom,
        TreeKind::BrownMushroom,
        TreeKind::Bush,
    ];

    #[test]
    fn every_tree_stays_within_reach_and_is_deterministic() {
        for kind in KINDS {
            for i in 0..200u64 {
                let t = Tree { kind, x: 0, y: SEA_LEVEL - 3, z: 0, hash: i.wrapping_mul(0x9E37_79B9_7F4A_7C15) };
                let mut a = Vec::new();
                build(&t, &mut |x, y, z, b, l| a.push((x, y, z, b, l)));
                let mut b = Vec::new();
                build(&t, &mut |x, y, z, bl, l| b.push((x, y, z, bl, l)));
                assert_eq!(a, b);
                assert!(!a.is_empty());
                for &(x, y, z, _, _) in &a {
                    assert!(x.abs() <= TREE_REACH && z.abs() <= TREE_REACH, "{kind:?} reaches {x},{z}");
                    assert!(y >= t.y - 1 && y < t.y + TREE_HEADROOM - 1, "{kind:?} y {y}");
                }
                // The trunk base is always a log-type block.
                assert!(a.iter().any(|&(x, y, z, _, l)| l && (x, y, z) == (0, t.y, 0)));
            }
        }
    }

    #[test]
    fn candidates_are_spaced_out() {
        for cz in -20..20 {
            for cx in -20..20 {
                let (x, z, _) = cell_candidate(9, cx, cz);
                assert!(x >= cx * TREE_CELL && x < (cx + 1) * TREE_CELL - 1);
                assert!(z >= cz * TREE_CELL && z < (cz + 1) * TREE_CELL - 1);
                let (nx, _, _) = cell_candidate(9, cx + 1, cz);
                let (_, nz, _) = cell_candidate(9, cx, cz + 1);
                assert!(nx - x >= 2 && nz - z >= 2);
            }
        }
    }
}
