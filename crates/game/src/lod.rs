//! Level of detail: which pieces of the world to draw, and at what detail.
//!
//! Near the camera the world is drawn from real chunks (every block). Past
//! the *render distance* it is drawn from **LOD tiles**: a tile always has
//! 16 × 16 cells, but a level-`L` tile's cells are `2^L` blocks wide, so it
//! covers `2^L × 2^L` chunks. This is the idea behind Distant Horizons and
//! Voxy: detail halves every time the distance doubles, so the number of
//! cells drawn grows with the *log* of the view distance, not its square.
//!
//! ## The quadtree
//!
//! Chunks (level 0) and tiles (levels 1…) form a quadtree: a level-`L`
//! node splits into its four level-`L−1` children. Starting from the
//! coarsest level, a node splits while it is closer to the camera than
//! `split_dist(L)`, which grows so that every ring of a level is
//! [`RING_TILES`] tiles wide:
//!
//! ```text
//!   split_dist(1) = render distance + 1          (chunks inside, tiles outside)
//!   split_dist(L+1) = split_dist(L) + RING_TILES · 2^L
//! ```
//!
//! The leaves of that tree cover the view square exactly once, with no
//! overlaps and no gaps, and neighbouring leaves differ by at most one
//! level.
//!
//! ## Height above the ground
//!
//! The distance that decides splitting is the horizontal distance or the
//! camera's height above the ground, whichever is larger. Seen from 50 km
//! up, the ground right below is 50 km away too, so it gets the detail of
//! anything 50 km away: no chunks, no fine tiles. Flying up therefore
//! *removes* work instead of adding it, and the whole planet seen from
//! orbit is a few hundred coarse tiles.
//!
//! ## The whole world
//!
//! Levels go up to [`MAX_LEVEL`] = 22, whose cells are 4,194,304 blocks
//! wide, so one ring of tiles spans more than the full 60,000 km world. With
//! the LOD distance set to the world's width every point of the world is
//! drawn from anywhere, at a cost that grows with the logarithm of the
//! distance: about [`RING_TILES`]² × 12 tiles per level.
//!
//! ## Swapping without holes
//!
//! When the camera moves, a tile may need to split into four finer ones
//! (or four into one). Until the new meshes exist, [`draw_set`] keeps
//! drawing whatever already covers that area completely, so the view
//! never shows holes while detail streams in.
//!
//! ## Tile meshes
//!
//! Each LOD cell is a box: a flat top at the height of what you'd see from
//! the sky (terrain, canopy, water or ice, from
//! [`worldgen::WorldGenerator::lod_grid`]) and walls down to each lower
//! neighbour. On the tile edge the walls reach one cell further down
//! ("skirts"), hiding cracks against a neighbour tile of another level.

use crate::mesh::{block_color, emit_quad, MeshData};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use worldgen::{BlockId, ChunkPos, WorldGenerator, CHUNK_SIZE};

/// FxHash-style hasher for quadtree keys and chunk positions. The default
/// SipHash guards against keys chosen by an attacker, which can't happen
/// here, and was most of the cost of the per-frame quadtree bookkeeping.
#[derive(Default, Clone, Copy)]
pub struct FastHasher(u64);

impl FastHasher {
    #[inline]
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(b as u64);
        }
    }
    fn write_u8(&mut self, v: u8) {
        self.add(v as u64);
    }
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    fn write_i32(&mut self, v: i32) {
        self.add(v as u32 as u64);
    }
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FastSet<T> = HashSet<T, BuildHasherDefault<FastHasher>>;
pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// Width of each ring, in tiles of that ring's level.
pub const RING_TILES: i32 = 8;
/// Coarsest level: tiles of 2^22 chunks (67 million blocks, more than the
/// whole 60,000 km world) with 4,194,304-block cells.
pub const MAX_LEVEL: u8 = 22;
/// Cells per tile edge (same as blocks per chunk edge).
pub const TILE_CELLS: usize = CHUNK_SIZE as usize;
/// Depth (blocks) of a tree crown on a far-view wall; below it the wall
/// shows the ground.
const CROWN: f32 = 6.0;

/// A node of the LOD quadtree. Level 0 is a chunk at chunk coordinates
/// (`x`, `z`); level `L` covers chunks `x·2^L ..< (x+1)·2^L` (same for z).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeKey {
    pub level: u8,
    pub x: i32,
    pub z: i32,
}

impl NodeKey {
    pub fn chunk_pos(self) -> ChunkPos {
        ChunkPos::new(self.x, self.z)
    }
    /// Chunks per tile edge.
    pub fn chunks(self) -> i32 {
        1 << self.level
    }
    pub fn min_chunk(self) -> (i32, i32) {
        (self.x * self.chunks(), self.z * self.chunks())
    }
    /// Block coordinates of the tile's (min x, min z) corner.
    pub fn origin_block(self) -> (i32, i32) {
        let (cx, cz) = self.min_chunk();
        (cx * CHUNK_SIZE, cz * CHUNK_SIZE)
    }
    pub fn size_blocks(self) -> i32 {
        CHUNK_SIZE * self.chunks()
    }
    /// Width of one LOD cell in blocks.
    pub fn cell_blocks(self) -> i32 {
        self.chunks()
    }
    pub fn children(self) -> [Self; 4] {
        debug_assert!(self.level > 0);
        let (l, x, z) = (self.level - 1, self.x * 2, self.z * 2);
        [
            Self { level: l, x, z },
            Self { level: l, x: x + 1, z },
            Self { level: l, x, z: z + 1 },
            Self { level: l, x: x + 1, z: z + 1 },
        ]
    }
    /// Chebyshev distance in chunks from `c` to the nearest chunk of the
    /// node (0 when `c` is inside it).
    pub fn dist(self, c: ChunkPos) -> i32 {
        let (x0, z0) = self.min_chunk();
        let n = self.chunks();
        let d = |v: i32, lo: i32| {
            if v < lo {
                lo - v
            } else if v >= lo + n {
                v - (lo + n - 1)
            } else {
                0
            }
        };
        d(c.x, x0).max(d(c.z, z0))
    }
}

/// The two view distances, in chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Full detail (real chunks) up to here.
    pub detail: i32,
    /// LOD tiles up to here. At or below `detail` there are no tiles.
    pub lod: i32,
    /// Camera height above the ground, in chunks (0 on the ground). Nodes
    /// closer than this are treated as this far away.
    pub altitude: i32,
}

impl Layout {
    pub fn lod_enabled(&self) -> bool {
        self.lod > self.detail
    }
    /// Nodes of `level` (≥ 1) split when nearer than this.
    pub fn split_dist(&self, level: u8) -> i32 {
        self.detail + 1 + RING_TILES * ((1 << level) - 2)
    }
    /// Coarsest level needed to reach the LOD distance.
    pub fn top_level(&self) -> u8 {
        if !self.lod_enabled() {
            return 0;
        }
        let mut l = 1;
        while l < MAX_LEVEL && self.split_dist(l + 1) <= self.lod {
            l += 1;
        }
        l
    }
    /// Radius of the chunks that may become leaves (and need meshing).
    pub fn chunk_radius(&self) -> i32 {
        if self.lod_enabled() {
            self.detail + 1
        } else {
            self.detail
        }
    }

    /// Distance that decides the detail: horizontal or height above the
    /// ground, whichever is larger.
    fn detail_dist(&self, n: NodeKey, center: ChunkPos) -> i32 {
        n.dist(center).max(self.altitude)
    }
    fn splits(&self, n: NodeKey, center: ChunkPos) -> bool {
        n.level > 0 && self.detail_dist(n, center) < self.split_dist(n.level)
    }
    fn wanted(&self, n: NodeKey, center: ChunkPos) -> bool {
        n.dist(center) <= if n.level == 0 { self.chunk_radius() } else { self.lod }
    }

    fn roots(&self, center: ChunkPos) -> impl Iterator<Item = NodeKey> + '_ {
        let top = self.top_level();
        let reach = if top == 0 { self.detail } else { self.lod };
        let n = 1i32 << top;
        let (lo_x, hi_x) = ((center.x - reach).div_euclid(n), (center.x + reach).div_euclid(n));
        let (lo_z, hi_z) = ((center.z - reach).div_euclid(n), (center.z + reach).div_euclid(n));
        (lo_z..=hi_z).flat_map(move |z| (lo_x..=hi_x).map(move |x| NodeKey { level: top, x, z }))
    }

    /// The leaves of the quadtree around `center`, nearest first. Nodes for
    /// which `in_world` is false (entirely outside the world border) are
    /// left out.
    pub fn leaves(&self, center: ChunkPos, in_world: &dyn Fn(NodeKey) -> bool) -> Vec<NodeKey> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeKey> = self.roots(center).collect();
        while let Some(n) = stack.pop() {
            if !self.wanted(n, center) || !in_world(n) {
                continue;
            }
            if self.splits(n, center) {
                stack.extend(n.children());
            } else {
                out.push(n);
            }
        }
        // Nearest first, then by level, z, x: packed into one integer
        // (flipping the sign bit keeps the order of signed values) and
        // sorted unstably, which is fine as the keys are unique. Several
        // times faster than sorting by a computed tuple.
        let flip = |v: i32| (v as u32 ^ 0x8000_0000) as u128;
        let mut keyed: Vec<(u128, NodeKey)> = out
            .into_iter()
            .map(|n| {
                let k = (n.dist(center) as u128) << 96 | (n.level as u128) << 64 | flip(n.z) << 32 | flip(n.x);
                (k, n)
            })
            .collect();
        keyed.sort_unstable_by_key(|e| e.0);
        keyed.into_iter().map(|e| e.1).collect()
    }

    /// What to draw: the desired leaves where their meshes are `ready`,
    /// and otherwise whatever ready meshes cover the same area completely
    /// (a parent, or the children of a node that is merging). Never two
    /// meshes over the same spot.
    pub fn draw_set(
        &self,
        center: ChunkPos,
        in_world: &dyn Fn(NodeKey) -> bool,
        ready: &FastSet<NodeKey>,
    ) -> Vec<NodeKey> {
        let mut out = Vec::new();
        for root in self.roots(center) {
            self.cover(root, center, in_world, ready, &mut out);
        }
        out
    }

    /// Appends the meshes that draw node `n` to `out` and returns whether
    /// they cover it completely.
    fn cover(
        &self,
        n: NodeKey,
        center: ChunkPos,
        in_world: &dyn Fn(NodeKey) -> bool,
        ready: &FastSet<NodeKey>,
        out: &mut Vec<NodeKey>,
    ) -> bool {
        if !self.wanted(n, center) || !in_world(n) {
            return true; // nothing to draw here
        }
        if !self.splits(n, center) {
            if ready.contains(&n) {
                out.push(n);
                return true;
            }
            // Merging: the four finer meshes may still be there.
            if n.level > 0 {
                let kids = n.children();
                let (mut inside, mut present) = (0, 0);
                for k in kids {
                    if in_world(k) {
                        inside += 1;
                        present += ready.contains(&k) as usize;
                    }
                }
                if present == inside && present > 0 {
                    out.extend(kids.into_iter().filter(|k| in_world(*k)));
                    return true;
                }
            }
            return false;
        }
        let start = out.len();
        let mut complete = true;
        for k in n.children() {
            complete &= self.cover(k, center, in_world, ready, out);
        }
        if !complete && ready.contains(&n) {
            // Splitting: keep the coarse mesh until all the finer ones exist.
            out.truncate(start);
            out.push(n);
            return true;
        }
        complete
    }
}

/// Builds the mesh of LOD tile `key` (level ≥ 1). Vertex positions are
/// relative to the tile's (min x, 0, min z) corner, like chunk meshes.
pub fn mesh_tile(gen: &WorldGenerator, key: NodeKey) -> MeshData {
    const N: usize = TILE_CELLS + 2; // one border cell on each side for the walls
    let s = key.cell_blocks();
    let (bx, bz) = key.origin_block();
    let grid = gen.lod_grid(bx - s, bz - s, s, N);
    let at = |i: usize, j: usize| &grid[i + j * N];
    let mut out = MeshData { min_y: f32::MAX, max_y: f32::MIN, ..Default::default() };
    // x and z are in cells (the renderer scales them by the cell size), y
    // in blocks.

    // Tops, greedy-merged by (height, block, biome tint).
    let key_of = |c: &worldgen::lod::LodColumn| -> Option<(i32, BlockId, [u8; 3])> {
        c.inside.then(|| (c.top_y, c.block, block_color(c.block, c.tint)))
    };
    let mut done = [[false; TILE_CELLS]; TILE_CELLS];
    for j in 0..TILE_CELLS {
        for i in 0..TILE_CELLS {
            if done[j][i] {
                continue;
            }
            let Some(k) = key_of(at(i + 1, j + 1)) else { continue };
            let mut w = 1;
            while i + w < TILE_CELLS && !done[j][i + w] && key_of(at(i + w + 1, j + 1)) == Some(k) {
                w += 1;
            }
            let mut h = 1;
            'grow: while j + h < TILE_CELLS {
                let row_matches = (i..i + w).all(|ii| !done[j + h][ii] && key_of(at(ii + 1, j + h + 1)) == Some(k));
                if !row_matches {
                    break 'grow;
                }
                h += 1;
            }
            for row in done.iter_mut().skip(j).take(h) {
                row[i..i + w].fill(true);
            }
            // +y face: u = z, v = x (see `emit_quad`).
            let base = [i as f32, (k.0 + 1) as f32, j as f32];
            emit_quad(&mut out, base, 2, 0, h as f32, w as f32, true, k.2, 2);
        }
    }

    // Walls down to each lower neighbour; skirts on the tile edge.
    for j in 0..TILE_CELLS {
        for i in 0..TILE_CELLS {
            let c = at(i + 1, j + 1);
            if !c.inside {
                continue;
            }
            let color = block_color(c.block, c.tint);
            let side = block_color(c.side, c.tint);
            let under = block_color(c.ground, c.tint);
            let top = (c.top_y + 1) as f32;
            // A wall more than two cells tall is a cliff (a pillar, a
            // gorge, a mountain face): its top cell's height shows the
            // top block, the rest the rock under it.
            let cliff = 2.0 * s as f32;
            let band = s as f32;
            // (di, dj, face, edge of tile?)
            for (di, dj, face) in [(1i32, 0i32, 0usize), (-1, 0, 1), (0, 1, 4), (0, -1, 5)] {
                let (ni, nj) = ((i as i32 + 1 + di) as usize, (j as i32 + 1 + dj) as usize);
                let nb = at(ni, nj);
                let edge = !(1..=TILE_CELLS).contains(&ni) || !(1..=TILE_CELLS).contains(&nj);
                let nb_top = if nb.inside { nb.top_y + 1 } else { c.ground_y.min(c.top_y) + 1 - s };
                let bottom = if edge { nb_top.min(c.top_y + 1) - s } else { nb_top } as f32;
                if bottom >= top {
                    continue;
                }
                let (x0, z0) = (i as f32, j as f32);
                // A canopy shows its colour only as deep as a crown: below
                // it, between the trunks, you see the ground (a wall of
                // leaves down to the ground would make far forests look
                // much denser from the side than the real trees). A
                // ground cliff is rock below a top band of its top block.
                let ground = ((c.ground_y + 1) as f32).clamp(bottom, top);
                let crown = (top - CROWN).max(ground);
                let ground_col = if crown > ground { under } else { color };
                let parts = if ground - bottom > cliff && side != ground_col {
                    [
                        (crown, top, color),
                        (ground, crown, under),
                        (ground - band, ground, ground_col),
                        (bottom, ground - band, side),
                    ]
                } else {
                    [
                        (crown.max(bottom), top, color),
                        (bottom, crown.max(bottom), under),
                        (0.0, 0.0, color),
                        (0.0, 0.0, color),
                    ]
                };
                for (lo, hi, col) in parts {
                    if hi <= lo {
                        continue;
                    }
                    match face {
                        // ±x faces: u = y, v = z.
                        0 => emit_quad(&mut out, [x0 + 1.0, lo, z0], 1, 2, hi - lo, 1.0, true, col, 0),
                        1 => emit_quad(&mut out, [x0, lo, z0], 1, 2, hi - lo, 1.0, false, col, 1),
                        // ±z faces: u = x, v = y.
                        4 => emit_quad(&mut out, [x0, lo, z0 + 1.0], 0, 1, 1.0, hi - lo, true, col, 4),
                        _ => emit_quad(&mut out, [x0, lo, z0], 0, 1, 1.0, hi - lo, false, col, 5),
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn everywhere(_: NodeKey) -> bool {
        true
    }

    /// Marks every chunk covered by `keys`; fails on overlap.
    fn coverage(keys: &[NodeKey]) -> HashMap<(i32, i32), u8> {
        let mut m = HashMap::new();
        for k in keys {
            let (x0, z0) = k.min_chunk();
            for z in z0..z0 + k.chunks() {
                for x in x0..x0 + k.chunks() {
                    assert!(m.insert((x, z), k.level).is_none(), "{k:?} overlaps at chunk {x},{z}");
                }
            }
        }
        m
    }

    #[test]
    fn leaves_tile_the_view_without_gaps_or_overlaps() {
        for (detail, lod) in [(8, 0), (12, 64), (16, 256), (32, 256), (64, 256), (5, 100)] {
            let layout = Layout { detail, lod, altitude: 0 };
            for center in [ChunkPos::new(0, 0), ChunkPos::new(-37, 1001), ChunkPos::new(123_457, -99_999)] {
                let leaves = layout.leaves(center, &everywhere);
                let cov = coverage(&leaves);
                // Everything within the view square is covered…
                let reach = lod.max(detail);
                for z in center.z - reach..=center.z + reach {
                    for x in center.x - reach..=center.x + reach {
                        assert!(cov.contains_key(&(x, z)), "{detail}/{lod} at {center:?}: gap at {x},{z}");
                    }
                }
                // …with chunks exactly up to the render distance (+1 with LODs).
                for (&(x, z), &level) in &cov {
                    let d = (x - center.x).abs().max((z - center.z).abs());
                    if d <= detail {
                        assert_eq!(level, 0, "{detail}/{lod}: chunk {x},{z} at distance {d} is level {level}");
                    }
                }
                assert!(leaves.iter().filter(|k| k.level == 0).all(|k| k.dist(center) <= layout.chunk_radius()));
                if !layout.lod_enabled() {
                    assert_eq!(leaves.len(), ((2 * detail + 1) * (2 * detail + 1)) as usize);
                }
            }
        }
    }

    #[test]
    fn neighbouring_leaves_differ_by_at_most_one_level() {
        let layout = Layout { detail: 12, lod: 256, altitude: 0 };
        let center = ChunkPos::new(3, -7);
        let cov = coverage(&layout.leaves(center, &everywhere));
        for (&(x, z), &l) in &cov {
            for (dx, dz) in [(1, 0), (0, 1)] {
                if let Some(&m) = cov.get(&(x + dx, z + dz)) {
                    assert!(l.abs_diff(m) <= 1, "levels {l} and {m} touch at chunk {x},{z}");
                }
            }
        }
    }

    #[test]
    fn detail_halves_as_distance_doubles() {
        // Cells (blocks) per tile stay constant, so the tile count of each
        // ring is about the same: total grows with log(distance).
        let layout = Layout { detail: 16, lod: 256, altitude: 0 };
        let leaves = layout.leaves(ChunkPos::new(0, 0), &everywhere);
        let tiles = leaves.iter().filter(|k| k.level > 0).count();
        let chunks = leaves.len() - tiles;
        // Chunks out to the render distance, rounded out to whole level-1 tiles.
        assert!((33 * 33..=35 * 35).contains(&chunks), "{chunks} chunks");
        eprintln!("{chunks} chunks + {tiles} tiles, top level {}", layout.top_level());
        // 513² = 263,169 chunks of area drawn with under 4,000 tiles.
        assert!(tiles < 4000, "{tiles} tiles");
        assert!(layout.top_level() >= 4);

        // A 17,696-block world stretches the LOD distance 46 ×: about five
        // more rings, not 46² times the tiles.
        let tall = Layout { detail: 16, lod: 256 * 46, altitude: 0 };
        let tall_tiles = tall.leaves(ChunkPos::new(0, 0), &everywhere).len() - chunks;
        eprintln!("tall world: {tall_tiles} tiles, top level {}", tall.top_level());
        assert!(tall_tiles < 3 * tiles, "{tall_tiles} tiles");
    }

    #[test]
    fn altitude_coarsens_the_view_without_gaps() {
        // Same checks as above, from a camera high above the ground.
        let center = ChunkPos::new(5, -3);
        let mut last = usize::MAX;
        for altitude in [0, 10, 40, 200, 1000] {
            let layout = Layout { detail: 12, lod: 256, altitude };
            let leaves = layout.leaves(center, &everywhere);
            let cov = coverage(&leaves);
            for z in center.z - 256..=center.z + 256 {
                for x in center.x - 256..=center.x + 256 {
                    assert!(cov.contains_key(&(x, z)), "altitude {altitude}: gap at {x},{z}");
                }
            }
            for (&(x, z), &l) in &cov {
                for (dx, dz) in [(1, 0), (0, 1)] {
                    if let Some(&m) = cov.get(&(x + dx, z + dz)) {
                        assert!(l.abs_diff(m) <= 1, "altitude {altitude}: levels {l} and {m} touch at {x},{z}");
                    }
                }
            }
            // Higher up: never more nodes, and no chunks once above the render distance.
            assert!(leaves.len() <= last, "altitude {altitude}: {} leaves, was {last}", leaves.len());
            last = leaves.len();
            if altitude > layout.detail {
                assert!(leaves.iter().all(|k| k.level > 0), "altitude {altitude}: chunks drawn");
            }
        }
    }

    /// The whole 60,000 km world (3.75 million chunks across) from the
    /// ground and from higher and higher up: every chunk of the world is
    /// covered exactly once, with a few thousand nodes at most.
    #[test]
    fn whole_world_fits_in_a_few_thousand_tiles() {
        let world = 3_749_998; // 59,999,968 blocks
        let half = world / 2;
        let clip = |k: NodeKey| -> i64 {
            let (x0, z0) = k.min_chunk();
            let n = k.chunks();
            let w = (x0 + n).min(half) - x0.max(-half);
            let d = (z0 + n).min(half) - z0.max(-half);
            w.max(0) as i64 * d.max(0) as i64
        };
        let inside = |k: NodeKey| clip(k) > 0;
        let mut counts = Vec::new();
        for (center, altitude) in [
            (ChunkPos::new(0, 0), 0),
            (ChunkPos::new(half - 1, -half), 0),
            (ChunkPos::new(0, 0), 1_000),
            (ChunkPos::new(0, 0), 100_000),
            (ChunkPos::new(0, 0), 3_000_000),
        ] {
            let layout = Layout { detail: 12, lod: world, altitude };
            assert!(layout.top_level() < MAX_LEVEL);
            let leaves = layout.leaves(center, &inside);
            let area: i64 = leaves.iter().map(|k| clip(*k)).sum();
            assert_eq!(area, world as i64 * world as i64, "{center:?} at {altitude}: holes or overlaps");
            let tiles = leaves.iter().filter(|k| k.level > 0).count();
            eprintln!("{center:?}, altitude {altitude} chunks: {} leaves ({tiles} tiles)", leaves.len());
            counts.push(leaves.len());
        }
        assert!(counts[0] < 20_000 && counts[1] < 20_000, "{counts:?}");
        assert!(counts[2] < counts[0] && counts[3] < counts[2] && counts[4] < 300, "{counts:?}");
    }

    #[test]
    fn world_border_prunes_nodes() {
        // Only nodes with x ≥ 0 exist in this fake world.
        let inside = |k: NodeKey| k.min_chunk().0 + k.chunks() > 0;
        let layout = Layout { detail: 8, lod: 64, altitude: 0 };
        let leaves = layout.leaves(ChunkPos::new(0, 0), &inside);
        assert!(leaves.iter().all(|k| inside(*k)));
        assert!(!leaves.is_empty());
    }

    #[test]
    fn draw_set_never_overlaps_and_falls_back_while_streaming() {
        let layout = Layout { detail: 6, lod: 48, altitude: 0 };
        let a = ChunkPos::new(0, 0);
        let leaves_a = layout.leaves(a, &everywhere);
        let mut ready: FastSet<NodeKey> = leaves_a.iter().copied().collect();
        // Everything ready: the draw set is exactly the leaves.
        let mut d = layout.draw_set(a, &everywhere, &ready);
        d.sort();
        let mut l = leaves_a.clone();
        l.sort();
        assert_eq!(d, l);

        // Move 20 chunks east: nothing new is ready yet. The old meshes
        // still cover most of the view, without overlaps.
        let b = ChunkPos::new(20, 0);
        let d = layout.draw_set(b, &everywhere, &ready);
        coverage(&d);
        assert!(d.len() > 100);

        // Once the new leaves are ready too, the draw set is the new leaves.
        let leaves_b = layout.leaves(b, &everywhere);
        ready.extend(leaves_b.iter().copied());
        let mut d = layout.draw_set(b, &everywhere, &ready);
        d.sort();
        let mut l = leaves_b.clone();
        l.sort();
        assert_eq!(d, l);
    }

    #[test]
    fn tile_mesh_has_tops_and_walls() {
        use worldgen::settings::WorldSize;
        let g =
            WorldGenerator::new(worldgen::WorldSettings { seed: 42, size: WorldSize::full(), ..Default::default() });
        for level in [1u8, 3, 6] {
            let m = mesh_tile(&g, NodeKey { level, x: 0, z: 0 });
            assert!(!m.vertices.is_empty());
            for v in &m.vertices {
                assert!(v.x() <= 16 && v.z() <= 16, "{v:?}");
            }
            assert!(m.min_y < m.max_y);
            // Greedy tops: far fewer than one quad per cell plus 4 walls.
            assert!(m.quad_count() < 16 * 16 * 5, "{} quads", m.quad_count());
        }
    }
}
