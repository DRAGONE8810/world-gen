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
use std::collections::HashSet;
use worldgen::{BlockId, ChunkPos, WorldGenerator, CHUNK_SIZE};

/// Width of each ring, in tiles of that ring's level.
pub const RING_TILES: i32 = 8;
/// Coarsest level: 8,192-chunk tiles with 8,192-block cells, enough for
/// the LOD distance of the tallest worlds (256 chunks × 46).
pub const MAX_LEVEL: u8 = 13;
/// Cells per tile edge (same as blocks per chunk edge).
pub const TILE_CELLS: usize = CHUNK_SIZE as usize;

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

    fn splits(&self, n: NodeKey, center: ChunkPos) -> bool {
        n.level > 0 && n.dist(center) < self.split_dist(n.level)
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
        out.sort_by_key(|n| (n.dist(center), n.level, n.z, n.x));
        out
    }

    /// What to draw: the desired leaves where their meshes are `ready`,
    /// and otherwise whatever ready meshes cover the same area completely
    /// (a parent, or the children of a node that is merging). Never two
    /// meshes over the same spot.
    pub fn draw_set(
        &self,
        center: ChunkPos,
        in_world: &dyn Fn(NodeKey) -> bool,
        ready: &HashSet<NodeKey>,
    ) -> Vec<NodeKey> {
        let mut out = Vec::new();
        for root in self.roots(center) {
            let (keys, _) = self.cover(root, center, in_world, ready);
            out.extend(keys);
        }
        out
    }

    /// Returns the meshes that draw node `n` and whether they cover it
    /// completely.
    fn cover(
        &self,
        n: NodeKey,
        center: ChunkPos,
        in_world: &dyn Fn(NodeKey) -> bool,
        ready: &HashSet<NodeKey>,
    ) -> (Vec<NodeKey>, bool) {
        if !self.wanted(n, center) || !in_world(n) {
            return (Vec::new(), true); // nothing to draw here
        }
        if !self.splits(n, center) {
            if ready.contains(&n) {
                return (vec![n], true);
            }
            // Merging: the four finer meshes may still be there.
            if n.level > 0 {
                let kids = n.children();
                let present: Vec<NodeKey> =
                    kids.iter().copied().filter(|k| in_world(*k) && ready.contains(k)).collect();
                if present.len() == kids.iter().filter(|k| in_world(**k)).count() && !present.is_empty() {
                    return (present, true);
                }
            }
            return (Vec::new(), false);
        }
        let mut keys = Vec::new();
        let mut complete = true;
        for k in n.children() {
            let (k_keys, k_complete) = self.cover(k, center, in_world, ready);
            keys.extend(k_keys);
            complete &= k_complete;
        }
        if !complete && ready.contains(&n) {
            // Splitting: keep the coarse mesh until all the finer ones exist.
            return (vec![n], true);
        }
        (keys, complete)
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
    let sf = s as f32;

    // Tops, greedy-merged by (height, block, biome tint).
    let key_of = |c: &worldgen::lod::LodColumn| -> Option<(i32, BlockId, [u8; 3])> {
        c.inside.then(|| (c.top_y, c.block, block_color(c.block, c.biome)))
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
            let base = [i as f32 * sf, (k.0 + 1) as f32, j as f32 * sf];
            emit_quad(&mut out, base, 2, 0, h as f32 * sf, w as f32 * sf, true, k.2, 2);
        }
    }

    // Walls down to each lower neighbour; skirts on the tile edge.
    for j in 0..TILE_CELLS {
        for i in 0..TILE_CELLS {
            let c = at(i + 1, j + 1);
            if !c.inside {
                continue;
            }
            let color = block_color(c.block, c.biome);
            let top = (c.top_y + 1) as f32;
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
                let (x0, z0) = (i as f32 * sf, j as f32 * sf);
                match face {
                    // ±x faces: u = y, v = z.
                    0 => emit_quad(&mut out, [x0 + sf, bottom, z0], 1, 2, top - bottom, sf, true, color, 0),
                    1 => emit_quad(&mut out, [x0, bottom, z0], 1, 2, top - bottom, sf, false, color, 1),
                    // ±z faces: u = x, v = y.
                    4 => emit_quad(&mut out, [x0, bottom, z0 + sf], 0, 1, sf, top - bottom, true, color, 4),
                    _ => emit_quad(&mut out, [x0, bottom, z0], 0, 1, sf, top - bottom, false, color, 5),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
            let layout = Layout { detail, lod };
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
        let layout = Layout { detail: 12, lod: 256 };
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
        let layout = Layout { detail: 16, lod: 256 };
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
        let tall = Layout { detail: 16, lod: 256 * 46 };
        let tall_tiles = tall.leaves(ChunkPos::new(0, 0), &everywhere).len() - chunks;
        eprintln!("tall world: {tall_tiles} tiles, top level {}", tall.top_level());
        assert!(tall_tiles < 3 * tiles, "{tall_tiles} tiles");
    }

    #[test]
    fn world_border_prunes_nodes() {
        // Only nodes with x ≥ 0 exist in this fake world.
        let inside = |k: NodeKey| k.min_chunk().0 + k.chunks() > 0;
        let layout = Layout { detail: 8, lod: 64 };
        let leaves = layout.leaves(ChunkPos::new(0, 0), &inside);
        assert!(leaves.iter().all(|k| inside(*k)));
        assert!(!leaves.is_empty());
    }

    #[test]
    fn draw_set_never_overlaps_and_falls_back_while_streaming() {
        let layout = Layout { detail: 6, lod: 48 };
        let a = ChunkPos::new(0, 0);
        let leaves_a = layout.leaves(a, &everywhere);
        let mut ready: HashSet<NodeKey> = leaves_a.iter().copied().collect();
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
            let size = (16 << level) as f32;
            for v in &m.vertices {
                assert!((0.0..=size).contains(&v.pos[0]) && (0.0..=size).contains(&v.pos[2]), "{:?}", v.pos);
            }
            assert!(m.min_y < m.max_y);
            // Greedy tops: far fewer than one quad per cell plus 4 walls.
            assert!(m.quad_count() < 16 * 16 * 5, "{} quads", m.quad_count());
        }
    }
}
