//! Chunk mesher: turns blocks into triangles.
//!
//! Two classic techniques, both standard in voxel engines:
//!
//! 1. **Hidden-face culling.** A face between two solid blocks can never be
//!    seen, so it is never emitted. Underground, almost every face is hidden.
//! 2. **Greedy meshing** (Mikola Lysenko, 2012). Visible faces of the same
//!    block type that lie in the same plane are merged into the largest
//!    possible rectangles. A flat 16×16 grass field becomes 1 quad instead of
//!    256, cutting vertex count by 10–50× on typical terrain.
//!
//! The mesher works one 16×16×16 section at a time on a padded 18³ copy of
//! the blocks (the section plus a 1-block border taken from neighbouring
//! sections and chunks), so face tests never need bounds checks.
//!
//! Face culling is **binary**: for every column of 18 blocks along an axis
//! the opaque and non-empty blocks become two bitmasks in a `u32`, and one
//! shift-and-mask finds every visible face of the column at once (a block
//! whose neighbour bit is clear). Only blocks that really have a face are
//! looked up again, which on typical terrain is a few percent of them.
//!
//! Vertices are packed into 8 bytes: x and z (0…16, in cells), y (16 bits)
//! and an sRGB colour with the face shading baked in. Quads always use the
//! same 6-index pattern, so meshes carry no index buffer: the renderer
//! shares one for all of them.

use bytemuck::{Pod, Zeroable};
use worldgen::block::Opacity;
use worldgen::palette::{SECTION_SIZE, SECTION_VOLUME};
use worldgen::{blocks, BlockId, Chunk, Tint};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq, Eq, Hash)]
pub struct Vertex {
    /// x (5 bits) | z (5 bits) << 5 | (y + Y_BIAS) (16 bits) << 10.
    /// x and z count cells from the mesh corner (a cell is one block in a
    /// chunk, `2^level` blocks in an LOD tile), y is in blocks.
    pub a: u32,
    /// sRGB colour: r | g << 8 | b << 16.
    pub b: u32,
}

/// Added to y so negative heights fit 16 unsigned bits: −32,768 … 32,767
/// covers the tallest world (−2,944 … 14,752) plus LOD skirts.
pub const Y_BIAS: i32 = 32_768;

impl Vertex {
    pub fn pack(x: u32, y: i32, z: u32, rgb: [u8; 3]) -> Self {
        debug_assert!(x <= 16 && z <= 16, "x {x} z {z}");
        debug_assert!((-Y_BIAS..Y_BIAS).contains(&y), "y {y}");
        let yb = (y + Y_BIAS) as u32;
        Self { a: x | (z << 5) | (yb << 10), b: rgb[0] as u32 | (rgb[1] as u32) << 8 | (rgb[2] as u32) << 16 }
    }
    #[cfg(test)]
    pub fn x(self) -> u32 {
        self.a & 31
    }
    #[cfg(test)]
    pub fn z(self) -> u32 {
        (self.a >> 5) & 31
    }
    #[cfg(test)]
    pub fn y(self) -> i32 {
        ((self.a >> 10) & 0xFFFF) as i32 - Y_BIAS
    }
    #[cfg(test)]
    pub fn color(self) -> [u8; 3] {
        [self.b as u8, (self.b >> 8) as u8, (self.b >> 16) as u8]
    }
}

#[derive(Default, Debug)]
pub struct MeshData {
    /// Four per quad, in the order the shared index pattern expects.
    pub vertices: Vec<Vertex>,
    /// Vertical extent of the geometry, for frustum culling.
    pub min_y: f32,
    pub max_y: f32,
}

impl MeshData {
    pub fn quad_count(&self) -> usize {
        self.vertices.len() / 4
    }
}

/// The four horizontal neighbours of a chunk. `None` means "no chunk there"
/// (outside the world border), which is treated as air.
pub struct Neighbours<'a> {
    pub neg_x: Option<&'a Chunk>,
    pub pos_x: Option<&'a Chunk>,
    pub neg_z: Option<&'a Chunk>,
    pub pos_z: Option<&'a Chunk>,
}

const P: usize = SECTION_SIZE + 2; // padded edge length
const P2: usize = P * P;

#[inline]
fn pidx(x: usize, y: usize, z: usize) -> usize {
    x + z * P + y * P2
}

/// Should the face of `b` that touches `n` be drawn? (The rule the
/// bitmasks in [`greedy_section`] implement.)
#[cfg(test)]
fn face_visible(b: BlockId, n: BlockId) -> bool {
    match b.opacity() {
        Opacity::Empty => false,
        Opacity::Opaque => n.opacity() != Opacity::Opaque,
        // Water/leaves/ice: only draw against air, so the inside of a lake
        // isn't full of faces.
        Opacity::Translucent => n.opacity() == Opacity::Empty,
    }
}

/// Simple directional light baked into vertex colours: top faces brightest,
/// bottoms darkest, the two side axes in between. Enough to read the shape
/// of terrain without any lighting system.
const FACE_SHADE: [f32; 6] = [0.80, 0.80, 1.00, 0.50, 0.65, 0.65]; // +x -x +y -y +z -z

type SectionMesher = fn(&[BlockId], &[Tint; 256], i32, &mut MeshData);

pub fn mesh_chunk(chunk: &Chunk, nb: &Neighbours) -> MeshData {
    mesh_chunk_with(chunk, nb, greedy_section)
}

fn mesh_chunk_with(chunk: &Chunk, nb: &Neighbours, section_mesher: SectionMesher) -> MeshData {
    let mut out = MeshData { min_y: f32::MAX, max_y: f32::MIN, ..Default::default() };
    let mut padded = vec![blocks::AIR; P * P * P];
    let mut dense = [blocks::AIR; SECTION_VOLUME];

    let sections = chunk.section_count();
    // Only the floor and the stored window can have visible faces: implied
    // stone below the window is buried, implied air above it is empty. In
    // a 17,696-block world this skips ~1,000 sections per chunk.
    let window = chunk.window();
    let to_mesh = std::iter::once(0).chain(window.start.saturating_sub(1).max(1)..(window.end + 1).min(sections));
    for s in to_mesh {
        let section = chunk.section(s);
        if section.uniform() == Some(blocks::AIR) {
            continue;
        }
        let y0 = chunk.min_y() + (s * SECTION_SIZE) as i32;

        // Fast reject: a solid opaque section fully enclosed by opaque
        // sections has no visible faces at all (most of the underground).
        if is_buried(chunk, nb, s) {
            continue;
        }

        // --- Fill the padded 18³ block cache --------------------------------
        section.to_dense(&mut dense);
        for y in 0..SECTION_SIZE {
            for z in 0..SECTION_SIZE {
                let src = y * 256 + z * 16;
                let dst = pidx(1, y + 1, z + 1);
                padded[dst..dst + 16].copy_from_slice(&dense[src..src + 16]);
            }
        }
        let below = |x: usize, z: usize| -> BlockId {
            if s == 0 {
                blocks::BEDROCK // never draw the underside of the world
            } else {
                chunk.section(s - 1).get(x, SECTION_SIZE - 1, z)
            }
        };
        for z in 0..16 {
            for x in 0..16 {
                padded[pidx(x + 1, 0, z + 1)] = below(x, z);
                padded[pidx(x + 1, P - 1, z + 1)] =
                    if s + 1 < sections { chunk.section(s + 1).get(x, 0, z) } else { blocks::AIR };
            }
        }
        let side = |c: Option<&Chunk>, x: usize, y: usize, z: usize| -> BlockId {
            c.map_or(blocks::AIR, |c| c.section(s).get(x, y, z))
        };
        for y in 0..16 {
            for i in 0..16 {
                padded[pidx(0, y + 1, i + 1)] = side(nb.neg_x, 15, y, i);
                padded[pidx(P - 1, y + 1, i + 1)] = side(nb.pos_x, 0, y, i);
                padded[pidx(i + 1, y + 1, 0)] = side(nb.neg_z, i, y, 15);
                padded[pidx(i + 1, y + 1, P - 1)] = side(nb.pos_z, i, y, 0);
            }
        }

        section_mesher(&padded, &chunk.tints, y0, &mut out);
    }

    if out.vertices.is_empty() {
        out.min_y = 0.0;
        out.max_y = 0.0;
    }
    out
}

fn is_buried(chunk: &Chunk, nb: &Neighbours, s: usize) -> bool {
    let opaque = |c: Option<&Chunk>, s: usize| -> bool {
        c.filter(|c| s < c.section_count())
            .and_then(|c| c.section(s).uniform())
            .is_some_and(|b| b.opacity() == Opacity::Opaque)
    };
    opaque(Some(chunk), s)
        && (s == 0 || opaque(Some(chunk), s - 1))
        && opaque(Some(chunk), s + 1)
        && opaque(nb.neg_x, s)
        && opaque(nb.pos_x, s)
        && opaque(nb.neg_z, s)
        && opaque(nb.pos_z, s)
}

/// Blocks whose colour depends on the column's tint (like Minecraft's grass
/// and foliage colours): grass takes the tint colour, oak leaves a darker
/// shade of it.
#[inline]
fn tinted(b: BlockId) -> bool {
    b == blocks::GRASS || b == blocks::OAK_LEAVES
}

/// Face colour of block `b` in a column of tint `tint`, before face
/// shading.
pub fn block_color(b: BlockId, tint: Tint) -> [u8; 3] {
    match b {
        blocks::GRASS => tint.color(),
        blocks::OAK_LEAVES => tint.foliage_color(),
        _ => b.info().color,
    }
}

// The mask packs block id and tint into 8 bits each.
const _: () = assert!(Tint::COUNT < 255);

/// Merge key of a visible face: 0 = none; otherwise (block id + 1), plus
/// (tint + 1) << 8 for tinted blocks. Block ids and tints both fit in 8
/// bits.
#[inline]
fn face_key(b: BlockId, tint: Tint) -> u16 {
    if tinted(b) {
        (b.0 + 1) | ((tint.0 as u16 + 1) << 8)
    } else {
        b.0 + 1
    }
}

/// Greedy-meshes one padded section into `out`, finding visible faces with
/// bitmasks (see the module docs).
///
/// The merge key is the block id, plus the tint for tinted blocks, so two
/// grass faces only merge when they have the same colour.
fn greedy_section(padded: &[BlockId], tints: &[Tint; 256], y0: i32, out: &mut MeshData) {
    // Per axis d, per interior column (i along u, j along v): bit k set when
    // the padded block at p[d] = k is opaque / not empty.
    let mut opaque = [[0u32; 256]; 3];
    let mut solid = [[0u32; 256]; 3];
    for y in 0..P {
        for z in 0..P {
            for x in 0..P {
                let class = padded[pidx(x, y, z)].opacity();
                if class == Opacity::Empty {
                    continue;
                }
                let p = [x, y, z];
                for d in 0..3 {
                    let (u, v) = ((d + 1) % 3, (d + 2) % 3);
                    if !(1..=16).contains(&p[u]) || !(1..=16).contains(&p[v]) {
                        continue;
                    }
                    let col = (p[u] - 1) + (p[v] - 1) * 16;
                    solid[d][col] |= 1 << p[d];
                    if class == Opacity::Opaque {
                        opaque[d][col] |= 1 << p[d];
                    }
                }
            }
        }
    }

    let mut masks = [[0u16; 256]; 16];
    for d in 0..3 {
        // u and v are the two axes spanning the face plane, chosen cyclically
        // so that u × v points along +d (this fixes triangle winding).
        let u = (d + 1) % 3;
        let v = (d + 2) % 3;
        for positive in [true, false] {
            let face = d * 2 + if positive { 0 } else { 1 }; // +x -x +y -y +z -z
            let mut slices = 0u32;
            for col in 0..256 {
                let (o, n) = (opaque[d][col], solid[d][col]);
                let t = n & !o; // translucent: water, leaves, ice
                                // Opaque faces show against anything not opaque, translucent
                                // ones only against air (so a lake has no inner faces).
                let (o_nb, n_nb) = if positive { (o >> 1, n >> 1) } else { (o << 1, n << 1) };
                let mut faces = ((o & !o_nb) | (t & !n_nb)) & 0x1FFFE; // interior bits 1..=16
                while faces != 0 {
                    let k = faces.trailing_zeros() as usize;
                    faces &= faces - 1;
                    let (i, j) = (col % 16, col / 16);
                    let mut p = [0usize; 3];
                    p[d] = k;
                    p[u] = i + 1;
                    p[v] = j + 1;
                    let b = padded[pidx(p[0], p[1], p[2])];
                    masks[k - 1][col] = face_key(b, tints[(p[0] - 1) + (p[2] - 1) * 16]);
                    slices |= 1 << (k - 1);
                }
            }
            while slices != 0 {
                let slice = slices.trailing_zeros() as usize;
                slices &= slices - 1;
                merge_slice(&mut masks[slice], d, u, v, slice, positive, face, y0, out);
            }
        }
    }
}

/// Greedy merge of one 16×16 face mask: grow each rectangle along u, then
/// along v. Clears the mask.
#[allow(clippy::too_many_arguments)]
fn merge_slice(
    mask: &mut [u16; 256],
    d: usize,
    u: usize,
    v: usize,
    slice: usize,
    positive: bool,
    face: usize,
    y0: i32,
    out: &mut MeshData,
) {
    for j in 0..16 {
        let mut i = 0;
        while i < 16 {
            let m = mask[i + j * 16];
            if m == 0 {
                i += 1;
                continue;
            }
            let mut w = 1;
            while i + w < 16 && mask[i + w + j * 16] == m {
                w += 1;
            }
            let mut h = 1;
            while j + h < 16 && mask[i + (j + h) * 16..i + w + (j + h) * 16].iter().all(|&x| x == m) {
                h += 1;
            }
            for jj in 0..h {
                mask[i + (j + jj) * 16..i + w + (j + jj) * 16].fill(0);
            }
            let mut base = [0f32; 3];
            base[d] = (slice + positive as usize) as f32;
            base[u] = i as f32;
            base[v] = j as f32;
            base[1] += y0 as f32;
            let block = BlockId((m & 0xFF) - 1);
            let tint = Tint((m >> 8).saturating_sub(1) as u8);
            emit_quad(out, base, u, v, w as f32, h as f32, positive, block_color(block, tint), face);
            i += w;
        }
    }
}

/// The original per-block greedy mesher, kept as the reference the binary
/// one is tested against.
#[cfg(test)]
fn greedy_section_reference(padded: &[BlockId], tints: &[Tint; 256], y0: i32, out: &mut MeshData) {
    // 0 = no face; otherwise (block id + 1) | (tint + 1) << 8 for tinted
    // blocks. Block ids and tints both fit in 8 bits.
    let mut mask = [0u16; 256];
    for d in 0..3 {
        // u and v are the two axes spanning the face plane, chosen cyclically
        // so that u × v points along +d (this fixes triangle winding).
        let u = (d + 1) % 3;
        let v = (d + 2) % 3;
        for positive in [true, false] {
            let face = d * 2 + if positive { 0 } else { 1 }; // +x -x +y -y +z -z
            for slice in 0..16 {
                // Build the visibility mask for this slice.
                let mut any = false;
                for j in 0..16 {
                    for i in 0..16 {
                        let mut p = [0usize; 3];
                        p[d] = slice + 1;
                        p[u] = i + 1;
                        p[v] = j + 1;
                        let b = padded[pidx(p[0], p[1], p[2])];
                        let mut q = p;
                        if positive {
                            q[d] += 1
                        } else {
                            q[d] -= 1
                        }
                        let n = padded[pidx(q[0], q[1], q[2])];
                        let m = if !face_visible(b, n) {
                            0
                        } else if tinted(b) {
                            let tint = tints[(p[0] - 1) + (p[2] - 1) * 16];
                            (b.0 + 1) | ((tint.0 as u16 + 1) << 8)
                        } else {
                            b.0 + 1
                        };
                        mask[i + j * 16] = m;
                        any |= m != 0;
                    }
                }
                if !any {
                    continue;
                }
                // Greedy merge: grow each rectangle along u, then along v.
                for j in 0..16 {
                    let mut i = 0;
                    while i < 16 {
                        let m = mask[i + j * 16];
                        if m == 0 {
                            i += 1;
                            continue;
                        }
                        let mut w = 1;
                        while i + w < 16 && mask[i + w + j * 16] == m {
                            w += 1;
                        }
                        let mut h = 1;
                        'grow: while j + h < 16 {
                            for k in 0..w {
                                if mask[i + k + (j + h) * 16] != m {
                                    break 'grow;
                                }
                            }
                            h += 1;
                        }
                        for jj in 0..h {
                            for k in 0..w {
                                mask[i + k + (j + jj) * 16] = 0;
                            }
                        }
                        let mut base = [0f32; 3];
                        base[d] = (slice + positive as usize) as f32;
                        base[u] = i as f32;
                        base[v] = j as f32;
                        base[1] += y0 as f32;
                        let block = BlockId((m & 0xFF) - 1);
                        let tint = Tint((m >> 8).saturating_sub(1) as u8);
                        emit_quad(out, base, u, v, w as f32, h as f32, positive, block_color(block, tint), face);
                        i += w;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn emit_quad(
    out: &mut MeshData,
    base: [f32; 3],
    u: usize,
    v: usize,
    w: f32,
    h: f32,
    positive: bool,
    c: [u8; 3],
    face: usize,
) {
    let mut du = [0f32; 3];
    du[u] = w;
    let mut dv = [0f32; 3];
    dv[v] = h;
    let add = |a: [f32; 3], b: [f32; 3]| [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
    let corners = [base, add(base, du), add(add(base, du), dv), add(base, dv)];
    let shade = FACE_SHADE[face];
    let color = [(c[0] as f32 * shade) as u8, (c[1] as f32 * shade) as u8, (c[2] as f32 * shade) as u8];
    // Counter-clockwise when seen from the side the face points to.
    let order: [usize; 4] = if positive { [0, 1, 2, 3] } else { [0, 3, 2, 1] };
    for k in order {
        let p = corners[k];
        out.min_y = out.min_y.min(p[1]);
        out.max_y = out.max_y.max(p[1]);
        out.vertices.push(Vertex::pack(p[0] as u32, p[1] as i32, p[2] as u32, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldgen::settings::WorldHeight;
    use worldgen::{ChunkPos, MIN_Y};

    const NONE: Neighbours<'static> = Neighbours { neg_x: None, pos_x: None, neg_z: None, pos_z: None };

    #[test]
    fn single_block_has_six_faces() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        c.set(5, 70, 5, blocks::STONE);
        let m = mesh_chunk(&c, &NONE);
        assert_eq!(m.quad_count(), 6);
        assert_eq!((m.min_y, m.max_y), (70.0, 71.0));
    }

    #[test]
    fn flat_layer_merges_into_six_quads() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        for z in 0..16 {
            for x in 0..16 {
                c.set(x, 64, z, blocks::GRASS);
            }
        }
        let m = mesh_chunk(&c, &NONE);
        // top, bottom, and one long strip per side.
        assert_eq!(m.quad_count(), 6);
    }

    #[test]
    fn hidden_faces_between_blocks_are_culled() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        c.set(1, 70, 1, blocks::STONE);
        c.set(2, 70, 1, blocks::DIRT); // different type: can't merge
        let m = mesh_chunk(&c, &NONE);
        // 12 faces minus the 2 touching ones; same-plane faces of different
        // blocks stay separate quads.
        assert_eq!(m.quad_count(), 10);
    }

    #[test]
    fn neighbour_chunk_hides_border_faces() {
        let mut a = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        let mut b = Chunk::empty(ChunkPos::new(1, 0), WorldHeight::default());
        a.set(15, 70, 0, blocks::STONE);
        b.set(0, 70, 0, blocks::STONE);
        let m = mesh_chunk(&a, &Neighbours { pos_x: Some(&b), ..NONE });
        assert_eq!(m.quad_count(), 5);
    }

    #[test]
    fn water_has_no_internal_faces_and_does_not_hide_ground() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        c.set(3, 60, 3, blocks::SAND);
        c.set(3, 61, 3, blocks::WATER);
        c.set(3, 62, 3, blocks::WATER);
        let m = mesh_chunk(&c, &NONE);
        // Sand: 6 faces (its top touches water, which is not opaque).
        // Water column: top + 4 sides each merged over 2 blocks = 5; its
        // bottom touches sand (not air) so it's skipped.
        assert_eq!(m.quad_count(), 6 + 5);
    }

    #[test]
    fn world_bottom_is_never_drawn() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        c.set(0, MIN_Y, 0, blocks::BEDROCK);
        assert_eq!(mesh_chunk(&c, &NONE).quad_count(), 5);
    }

    #[test]
    fn generated_terrain_is_mostly_culled() {
        let g = worldgen::WorldGenerator::new(worldgen::WorldSettings::default());
        let get = |x, z| g.generate_chunk(ChunkPos::new(x, z));
        let (c, nx, px, nz, pz) = (get(0, 0), get(-1, 0), get(1, 0), get(0, -1), get(0, 1));
        let m = mesh_chunk(&c, &Neighbours { neg_x: Some(&nx), pos_x: Some(&px), neg_z: Some(&nz), pos_z: Some(&pz) });
        // 98,304 blocks; a naive mesher would emit hundreds of thousands of
        // faces. Terrain should need only a few hundred quads.
        assert!(m.quad_count() > 10 && m.quad_count() < 6000, "{} quads", m.quad_count());
    }

    #[test]
    fn block_ids_fit_the_mask() {
        assert!(BlockId::count() < 255);
    }

    #[test]
    fn grass_merges_only_within_one_tint() {
        let (a, b) = (Tint(40), Tint(200));
        let mut c = Chunk::empty(ChunkPos::new(0, 0), WorldHeight::default());
        for z in 0..16 {
            for x in 0..16 {
                c.set(x, 64, z, blocks::GRASS);
                c.tints[x + z * 16] = if x < 8 { a } else { b };
            }
        }
        let m = mesh_chunk(&c, &NONE);
        // Top and bottom split into two quads each, the ±x sides are one
        // tint each, the ±z sides split in two.
        assert_eq!(m.quad_count(), 2 + 2 + 1 + 1 + 2 + 2);
        // Top faces are unshaded, so they carry the exact tint colours.
        let colors: std::collections::HashSet<_> = m.vertices.iter().map(|v| v.color()).collect();
        for t in [a, b] {
            assert!(colors.contains(&t.color()), "{t:?}");
        }
    }

    #[test]
    fn vertex_packing_round_trips() {
        for (x, y, z) in [(0, -2944, 0), (16, 14_752, 16), (7, 63, 9), (16, -11_136, 0)] {
            let v = Vertex::pack(x, y, z, [1, 2, 3]);
            assert_eq!((v.x(), v.y(), v.z(), v.color()), (x, y, z, [1, 2, 3]));
        }
        assert_eq!(std::mem::size_of::<Vertex>(), 8);
    }

    #[test]
    fn binary_mesher_matches_the_reference_mesher() {
        use worldgen::settings::{WorldSize, WorldType};
        use worldgen::{WorldGenerator, WorldSettings};
        let mut total = 0;
        for (seed, world_type, height) in
            [(42, WorldType::Normal, 384), (7, WorldType::Amplified, 384), (3, WorldType::Normal, 5000)]
        {
            let g = WorldGenerator::new(WorldSettings {
                seed,
                world_type,
                height: WorldHeight::from_blocks(height),
                size: WorldSize::full(),
                ..Default::default()
            });
            let (sx, _, sz) = g.find_spawn();
            for (dx, dz) in [(0, 0), (3, -2), (-9, 4)] {
                let p = ChunkPos::from_block(sx, sz).offset(dx, dz);
                let get = |x, z| g.generate_chunk(p.offset(x, z));
                let (c, nx, px, nz, pz) = (get(0, 0), get(-1, 0), get(1, 0), get(0, -1), get(0, 1));
                let nb = Neighbours { neg_x: Some(&nx), pos_x: Some(&px), neg_z: Some(&nz), pos_z: Some(&pz) };
                let fast = mesh_chunk(&c, &nb);
                let reference = mesh_chunk_with(&c, &nb, greedy_section_reference);
                assert_eq!(fast.vertices, reference.vertices, "seed {seed} chunk {p:?}");
                total += fast.quad_count();
            }
        }
        assert!(total > 500, "only {total} quads compared");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use worldgen::{ChunkPos, WorldGenerator, WorldSettings};

    /// `cargo test --release -p game mesh_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mesh_bench() {
        let g = WorldGenerator::new(WorldSettings { seed: 42, ..Default::default() });
        let n = 12;
        let chunks: Vec<Vec<Chunk>> =
            (0..n + 2).map(|z| (0..n + 2).map(|x| g.generate_chunk(ChunkPos::new(x - 1, z - 1))).collect()).collect();
        let t = std::time::Instant::now();
        let (mut quads, mut bytes) = (0, 0);
        for z in 1..=n as usize {
            for x in 1..=n as usize {
                let m = mesh_chunk(
                    &chunks[z][x],
                    &Neighbours {
                        neg_x: Some(&chunks[z][x - 1]),
                        pos_x: Some(&chunks[z][x + 1]),
                        neg_z: Some(&chunks[z - 1][x]),
                        pos_z: Some(&chunks[z + 1][x]),
                    },
                );
                quads += m.quad_count();
                bytes += m.vertices.len() * std::mem::size_of::<Vertex>();
            }
        }
        let c = (n * n) as f64;
        eprintln!(
            "{:.3} ms/chunk, {:.0} quads/chunk, {:.1} KiB GPU/chunk",
            t.elapsed().as_secs_f64() * 1000.0 / c,
            quads as f64 / c,
            bytes as f64 / c / 1024.0
        );
    }
}
