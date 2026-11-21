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

use bytemuck::{Pod, Zeroable};
use worldgen::block::Opacity;
use worldgen::chunk::SECTIONS_PER_CHUNK;
use worldgen::palette::{SECTION_SIZE, SECTION_VOLUME};
use worldgen::{blocks, Biome, BlockId, Chunk, MIN_Y};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct Vertex {
    /// Position relative to the chunk's (min x, 0, min z) corner.
    pub pos: [f32; 3],
    /// sRGB colour with face shading baked in.
    pub color: [u8; 4],
}

#[derive(Default, Debug)]
pub struct MeshData {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    /// Vertical extent of the geometry, for frustum culling.
    pub min_y: f32,
    pub max_y: f32,
}

impl MeshData {
    #[cfg(test)]
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

/// Should the face of `b` that touches `n` be drawn?
#[inline]
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

pub fn mesh_chunk(chunk: &Chunk, nb: &Neighbours) -> MeshData {
    let mut out = MeshData { min_y: f32::MAX, max_y: f32::MIN, ..Default::default() };
    let mut padded = vec![blocks::AIR; P * P * P];
    let mut dense = [blocks::AIR; SECTION_VOLUME];

    for s in 0..SECTIONS_PER_CHUNK {
        let section = &chunk.sections[s];
        if section.uniform() == Some(blocks::AIR) {
            continue;
        }
        let y0 = MIN_Y + (s * SECTION_SIZE) as i32;

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
                chunk.sections[s - 1].get(x, SECTION_SIZE - 1, z)
            }
        };
        for z in 0..16 {
            for x in 0..16 {
                padded[pidx(x + 1, 0, z + 1)] = below(x, z);
                padded[pidx(x + 1, P - 1, z + 1)] =
                    if s + 1 < SECTIONS_PER_CHUNK { chunk.sections[s + 1].get(x, 0, z) } else { blocks::AIR };
            }
        }
        let side = |c: Option<&Chunk>, x: usize, y: usize, z: usize| -> BlockId {
            c.map_or(blocks::AIR, |c| c.sections[s].get(x, y, z))
        };
        for y in 0..16 {
            for i in 0..16 {
                padded[pidx(0, y + 1, i + 1)] = side(nb.neg_x, 15, y, i);
                padded[pidx(P - 1, y + 1, i + 1)] = side(nb.pos_x, 0, y, i);
                padded[pidx(i + 1, y + 1, 0)] = side(nb.neg_z, i, y, 15);
                padded[pidx(i + 1, y + 1, P - 1)] = side(nb.pos_z, i, y, 0);
            }
        }

        greedy_section(&padded, &chunk.biomes, y0, &mut out);
    }

    if out.vertices.is_empty() {
        out.min_y = 0.0;
        out.max_y = 0.0;
    }
    out
}

fn is_buried(chunk: &Chunk, nb: &Neighbours, s: usize) -> bool {
    let opaque = |c: Option<&Chunk>, s: usize| -> bool {
        c.and_then(|c| c.sections.get(s)).and_then(|sec| sec.uniform()).is_some_and(|b| b.opacity() == Opacity::Opaque)
    };
    opaque(Some(chunk), s)
        && (s == 0 || opaque(Some(chunk), s - 1))
        && opaque(Some(chunk), s + 1)
        && opaque(nb.neg_x, s)
        && opaque(nb.pos_x, s)
        && opaque(nb.neg_z, s)
        && opaque(nb.pos_z, s)
}

/// Blocks whose colour depends on the biome (like Minecraft's grass and
/// foliage tint): grass takes the biome's grass colour, oak leaves a darker
/// shade of it.
#[inline]
fn tinted(b: BlockId) -> bool {
    b == blocks::GRASS || b == blocks::OAK_LEAVES
}

/// Face colour of block `b` in `biome`, before face shading.
pub fn block_color(b: BlockId, biome: Biome) -> [u8; 3] {
    match b {
        blocks::GRASS => biome.grass_color(),
        blocks::OAK_LEAVES => biome.grass_color().map(|c| (c as f32 * 0.72) as u8),
        _ => b.info().color,
    }
}

// The mask packs block id and biome into 8 bits each.
const _: () = assert!(Biome::ALL.len() < 255);

/// Greedy-meshes one padded section into `out`.
///
/// The merge key is the block id, plus the biome for tinted blocks, so two
/// grass faces only merge when they have the same colour.
fn greedy_section(padded: &[BlockId], biomes: &[Biome; 256], y0: i32, out: &mut MeshData) {
    // 0 = no face; otherwise (block id + 1) | (biome + 1) << 8 for tinted
    // blocks. Block ids and biomes both fit in 8 bits.
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
                            let biome = biomes[(p[0] - 1) + (p[2] - 1) * 16];
                            (b.0 + 1) | ((biome as u16 + 1) << 8)
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
                        let biome = match m >> 8 {
                            0 => Biome::default(),
                            k => Biome::ALL[k as usize - 1],
                        };
                        emit_quad(out, base, u, v, w as f32, h as f32, positive, block_color(block, biome), face);
                        i += w;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_quad(
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
    let color = [(c[0] as f32 * shade) as u8, (c[1] as f32 * shade) as u8, (c[2] as f32 * shade) as u8, 255];
    let start = out.vertices.len() as u32;
    // Counter-clockwise when seen from the side the face points to.
    let order: [usize; 4] = if positive { [0, 1, 2, 3] } else { [0, 3, 2, 1] };
    for k in order {
        let p = corners[k];
        out.min_y = out.min_y.min(p[1]);
        out.max_y = out.max_y.max(p[1]);
        out.vertices.push(Vertex { pos: p, color });
    }
    out.indices.extend_from_slice(&[start, start + 1, start + 2, start, start + 2, start + 3]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldgen::ChunkPos;

    const NONE: Neighbours<'static> = Neighbours { neg_x: None, pos_x: None, neg_z: None, pos_z: None };

    #[test]
    fn single_block_has_six_faces() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
        c.set(5, 70, 5, blocks::STONE);
        let m = mesh_chunk(&c, &NONE);
        assert_eq!(m.quad_count(), 6);
        assert_eq!(m.indices.len(), 36);
        assert_eq!((m.min_y, m.max_y), (70.0, 71.0));
    }

    #[test]
    fn flat_layer_merges_into_six_quads() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
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
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
        c.set(1, 70, 1, blocks::STONE);
        c.set(2, 70, 1, blocks::DIRT); // different type: can't merge
        let m = mesh_chunk(&c, &NONE);
        // 12 faces minus the 2 touching ones; same-plane faces of different
        // blocks stay separate quads.
        assert_eq!(m.quad_count(), 10);
    }

    #[test]
    fn neighbour_chunk_hides_border_faces() {
        let mut a = Chunk::empty(ChunkPos::new(0, 0));
        let mut b = Chunk::empty(ChunkPos::new(1, 0));
        a.set(15, 70, 0, blocks::STONE);
        b.set(0, 70, 0, blocks::STONE);
        let m = mesh_chunk(&a, &Neighbours { pos_x: Some(&b), ..NONE });
        assert_eq!(m.quad_count(), 5);
    }

    #[test]
    fn water_has_no_internal_faces_and_does_not_hide_ground() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
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
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
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
    fn grass_merges_only_within_one_biome() {
        let mut c = Chunk::empty(ChunkPos::new(0, 0));
        for z in 0..16 {
            for x in 0..16 {
                c.set(x, 64, z, blocks::GRASS);
                c.biomes[x + z * 16] = if x < 8 { Biome::Plains } else { Biome::Jungle };
            }
        }
        let m = mesh_chunk(&c, &NONE);
        // Top and bottom split into two quads each, the ±x sides are one
        // biome each, the ±z sides split in two.
        assert_eq!(m.quad_count(), 2 + 2 + 1 + 1 + 2 + 2);
        // Top faces are unshaded, so they carry the exact biome grass colours.
        let colors: std::collections::HashSet<_> = m.vertices.iter().map(|v| v.color).collect();
        for b in [Biome::Plains, Biome::Jungle] {
            let [r, g, bl] = b.grass_color();
            assert!(colors.contains(&[r, g, bl, 255]), "{b:?}");
        }
    }
}
