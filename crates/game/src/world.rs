//! Streaming: decides which chunks and LOD tiles to build, draw and drop
//! as the camera moves, and runs that work on background threads.
//!
//! What to show comes from the LOD quadtree ([`crate::lod`]): real chunks
//! up to the render distance, coarser and coarser tiles beyond it, out to
//! the LOD distance. Pipelines:
//!
//! ```text
//!   chunk: not loaded ──generate (worker)──▶ generated ──mesh (worker)──▶ ready ──upload──▶ drawn
//!   tile:  ──────────────sample + mesh (worker, straight from the generator)──▶ ready ──upload──▶ drawn
//! ```
//!
//! A chunk can only be meshed once its 4 horizontal neighbours exist,
//! because faces on the chunk edge depend on the blocks next door. So we
//! generate one ring further out than we mesh. Tiles need no neighbours:
//! they sample the generator's columns directly, one border cell included.
//!
//! Work is ordered nearest-first and the number of jobs in flight is
//! capped, so the area around the player fills in first and the main thread
//! never stalls. Meshes that are no longer wanted stay until the meshes
//! replacing them are ready ([`Layout::draw_set`]), so moving never opens
//! holes in the distance.

use crate::lod::{mesh_tile, Layout, NodeKey};
use crate::mesh::{mesh_chunk, MeshData, Neighbours};
use crate::render::MAX_UPLOADS_PER_FRAME;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use worldgen::{Chunk, ChunkPos, WorldGenerator};

enum JobResult {
    Generated(ChunkPos, Arc<Chunk>),
    Meshed(NodeKey, MeshData),
}

/// What the renderer should do this frame.
pub enum MeshEvent {
    Upload(NodeKey, MeshData),
    Remove(NodeKey),
}

pub struct ChunkManager {
    gen: Arc<WorldGenerator>,
    pool: rayon::ThreadPool,
    tx: Sender<JobResult>,
    rx: Receiver<JobResult>,
    /// Block data of chunks near the camera (needed to mesh them).
    chunks: HashMap<ChunkPos, Arc<Chunk>>,
    generating: HashSet<ChunkPos>,
    meshing: HashSet<NodeKey>,
    /// Meshes built but not handed to the renderer yet.
    finished: VecDeque<(NodeKey, MeshData)>,
    /// Meshes handed to the renderer (including empty ones it never sees).
    ready: HashSet<NodeKey>,
    /// The quadtree leaves for the current view, nearest first.
    leaves: Vec<NodeKey>,
    leaf_set: HashSet<NodeKey>,
    view: Option<(ChunkPos, Layout)>,
    /// Leaves before this index are all ready (scan shortcut).
    scan_from: usize,
    draw: Vec<NodeKey>,
    draw_dirty: bool,
    max_in_flight: usize,
    /// When the world opened, to log how long the first full view took.
    started: std::time::Instant,
    first_view_logged: bool,
    pub stats: Stats,
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub loaded_chunks: usize,
    /// Chunk meshes ready (level 0).
    pub meshed_chunks: usize,
    /// LOD tile meshes ready (levels ≥ 1).
    pub lod_tiles: usize,
    pub jobs_in_flight: usize,
    pub generated_total: u64,
    pub chunk_memory_bytes: usize,
    /// Leaves of the current view that are not ready yet.
    pub pending: usize,
    /// Of which real chunks.
    pub pending_chunks: usize,
}

impl ChunkManager {
    pub fn new(gen: WorldGenerator) -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(1).max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("chunk-worker-{i}"))
            .build()
            .expect("thread pool");
        let (tx, rx) = channel();
        Self {
            gen: Arc::new(gen),
            pool,
            tx,
            rx,
            chunks: HashMap::new(),
            generating: HashSet::new(),
            meshing: HashSet::new(),
            finished: VecDeque::new(),
            ready: HashSet::new(),
            leaves: Vec::new(),
            leaf_set: HashSet::new(),
            view: None,
            scan_from: 0,
            draw: Vec::new(),
            draw_dirty: true,
            max_in_flight: threads * 3,
            started: std::time::Instant::now(),
            first_view_logged: false,
            stats: Stats::default(),
        }
    }

    pub fn generator(&self) -> &WorldGenerator {
        &self.gen
    }

    /// Shared handle, for work on other threads (the world map).
    pub fn generator_arc(&self) -> &Arc<WorldGenerator> {
        &self.gen
    }

    /// The meshes to draw this frame.
    pub fn draw_list(&self) -> &[NodeKey] {
        &self.draw
    }

    fn in_flight(&self) -> usize {
        self.generating.len() + self.meshing.len()
    }

    /// Does node `k` overlap the world (inside the border)?
    fn in_world(gen: &WorldGenerator, k: NodeKey) -> bool {
        let (hx, hz) = (gen.settings().size.half_width() as i64, gen.settings().size.half_depth() as i64);
        let (x0, z0) = k.origin_block();
        let (x0, z0, n) = (x0 as i64, z0 as i64, k.size_blocks() as i64);
        x0 < hx && x0 + n > -hx && z0 < hz && z0 + n > -hz
    }

    /// Advances streaming. `center` is the camera's chunk.
    pub fn update(&mut self, center: ChunkPos, layout: Layout) -> Vec<MeshEvent> {
        let mut events = Vec::new();
        let gen = self.gen.clone();
        let in_world = |k: NodeKey| Self::in_world(&gen, k);
        let keep_radius = layout.chunk_radius() + 2;

        // 1. New view: recompute the quadtree leaves.
        if self.view != Some((center, layout)) {
            self.view = Some((center, layout));
            self.leaves = layout.leaves(center, &in_world);
            self.leaf_set = self.leaves.iter().copied().collect();
            self.scan_from = 0;
            self.draw_dirty = true;
        }

        // 2. Collect finished work.
        while let Ok(res) = self.rx.try_recv() {
            match res {
                JobResult::Generated(pos, chunk) => {
                    self.generating.remove(&pos);
                    self.stats.generated_total += 1;
                    if pos.chebyshev(center) <= keep_radius {
                        self.stats.chunk_memory_bytes += chunk.memory_bytes();
                        self.chunks.insert(pos, chunk);
                    }
                }
                JobResult::Meshed(key, mesh) => {
                    // Stays in `meshing` until handed over, so it isn't
                    // scheduled twice.
                    self.finished.push_back((key, mesh));
                }
            }
        }
        // Hand at most what the renderer uploads in one frame, so "ready"
        // really means "on the GPU" and swaps never show a hole.
        let mut uploads = 0;
        while uploads < MAX_UPLOADS_PER_FRAME {
            let Some((key, mesh)) = self.finished.pop_front() else { break };
            self.meshing.remove(&key);
            if !self.leaf_set.contains(&key) || !self.ready.insert(key) {
                continue;
            }
            if !mesh.vertices.is_empty() {
                events.push(MeshEvent::Upload(key, mesh));
                uploads += 1;
            }
            self.draw_dirty = true;
        }

        // 3. Decide what to draw, and drop meshes nothing needs any more.
        if self.draw_dirty {
            self.draw = layout.draw_set(center, &in_world, &self.ready);
            let drawn: HashSet<NodeKey> = self.draw.iter().copied().collect();
            let leaf_set = &self.leaf_set;
            self.ready.retain(|k| {
                let keep = leaf_set.contains(k) || drawn.contains(k);
                if !keep {
                    events.push(MeshEvent::Remove(*k));
                }
                keep
            });
            self.draw_dirty = false;
        }

        // 4. Drop block data that is now too far away.
        let mut freed = 0;
        self.chunks.retain(|p, c| {
            let keep = p.chebyshev(center) <= keep_radius;
            if !keep {
                freed += c.memory_bytes();
            }
            keep
        });
        self.stats.chunk_memory_bytes = self.stats.chunk_memory_bytes.saturating_sub(freed);

        // 5. Schedule new work, nearest first.
        while self.scan_from < self.leaves.len() && self.ready.contains(&self.leaves[self.scan_from]) {
            self.scan_from += 1;
        }
        let (mut pending, mut pending_chunks) = (0, 0);
        for i in self.scan_from..self.leaves.len() {
            let key = self.leaves[i];
            if self.ready.contains(&key) {
                continue;
            }
            pending += 1;
            pending_chunks += (key.level == 0) as usize;
            if self.meshing.contains(&key) || self.in_flight() >= self.max_in_flight {
                continue;
            }
            if key.level > 0 {
                self.meshing.insert(key);
                let (gen, tx) = (self.gen.clone(), self.tx.clone());
                self.pool.spawn(move || {
                    let _ = tx.send(JobResult::Meshed(key, mesh_tile(&gen, key)));
                });
                continue;
            }

            // A chunk: generate it and its neighbours first.
            let pos = key.chunk_pos();
            let mut missing = false;
            for (dx, dz) in [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)] {
                let p = pos.offset(dx, dz);
                if self.chunks.contains_key(&p) || !self.gen.chunk_touches_world(p) {
                    continue;
                }
                missing = true;
                if !self.generating.contains(&p) && self.in_flight() < self.max_in_flight {
                    self.generating.insert(p);
                    let (gen, tx) = (self.gen.clone(), self.tx.clone());
                    self.pool.spawn(move || {
                        let chunk = gen.generate_chunk(p);
                        let _ = tx.send(JobResult::Generated(p, Arc::new(chunk)));
                    });
                }
            }
            if missing {
                continue;
            }
            let get = |dx, dz| self.chunks.get(&pos.offset(dx, dz)).cloned();
            let (chunk, nx, px, nz, pz) = (get(0, 0), get(-1, 0), get(1, 0), get(0, -1), get(0, 1));
            self.meshing.insert(key);
            let tx = self.tx.clone();
            self.pool.spawn(move || {
                let mesh = match &chunk {
                    Some(c) => mesh_chunk(
                        c,
                        &Neighbours {
                            neg_x: nx.as_deref(),
                            pos_x: px.as_deref(),
                            neg_z: nz.as_deref(),
                            pos_z: pz.as_deref(),
                        },
                    ),
                    None => MeshData::default(), // outside the world border
                };
                let _ = tx.send(JobResult::Meshed(key, mesh));
            });
        }

        self.stats.loaded_chunks = self.chunks.len();
        self.stats.meshed_chunks = self.ready.iter().filter(|k| k.level == 0).count();
        self.stats.lod_tiles = self.ready.len() - self.stats.meshed_chunks;
        self.stats.jobs_in_flight = self.in_flight();
        self.stats.pending = pending;
        self.stats.pending_chunks = pending_chunks;
        if pending == 0 && !self.first_view_logged {
            self.first_view_logged = true;
            log::info!(
                "view complete after {:.1} s: {} chunks + {} LOD tiles ({:?})",
                self.started.elapsed().as_secs_f64(),
                self.stats.meshed_chunks,
                self.stats.lod_tiles,
                layout
            );
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldgen::{WorldSettings, WorldSize};

    #[test]
    fn streams_every_chunk_in_range_exactly_once() {
        let gen = WorldGenerator::new(WorldSettings { size: WorldSize::from_blocks(1000), ..Default::default() });
        let mut m = ChunkManager::new(gen);
        let mut uploaded = HashSet::new();
        let center = ChunkPos::new(0, 0);
        for _ in 0..5000 {
            for e in m.update(center, Layout { detail: 3, lod: 0 }) {
                if let MeshEvent::Upload(p, _) = e {
                    assert!(uploaded.insert(p), "{p:?} meshed twice");
                }
            }
            if m.stats.pending == 0 && m.stats.jobs_in_flight == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(uploaded.len(), 49);
    }

    #[test]
    fn world_border_limits_streaming() {
        // 1 km world = chunks -32..=31. Standing at the edge, only chunks
        // inside the border may be generated.
        let gen = WorldGenerator::new(WorldSettings { size: WorldSize::from_blocks(1000), ..Default::default() });
        let mut m = ChunkManager::new(gen);
        let center = ChunkPos::new(31, 0);
        let mut uploaded = HashSet::new();
        for _ in 0..5000 {
            for e in m.update(center, Layout { detail: 2, lod: 0 }) {
                if let MeshEvent::Upload(p, _) = e {
                    uploaded.insert(p);
                }
            }
            if m.stats.pending == 0 && m.stats.jobs_in_flight == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(uploaded.len(), 3 * 5);
        assert!(uploaded.iter().all(|p| p.x <= 31 && p.level == 0));
    }

    /// Runs the streamer until everything for `layout` around `center` is
    /// ready; returns the keys uploaded.
    fn settle(m: &mut ChunkManager, center: ChunkPos, layout: Layout, live: &mut HashSet<NodeKey>) {
        for _ in 0..20_000 {
            for e in m.update(center, layout) {
                match e {
                    MeshEvent::Upload(k, _) => assert!(live.insert(k), "{k:?} uploaded twice"),
                    MeshEvent::Remove(k) => assert!(live.remove(&k), "{k:?} removed but never uploaded"),
                }
            }
            if m.stats.pending == 0 && m.stats.jobs_in_flight == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("streaming did not settle");
    }

    #[test]
    fn lod_rings_stream_swap_and_evict() {
        let gen = WorldGenerator::new(WorldSettings { seed: 42, size: WorldSize::full(), ..Default::default() });
        let mut m = ChunkManager::new(gen);
        let layout = Layout { detail: 2, lod: 40 };
        let mut live = HashSet::new();
        let a = ChunkPos::new(0, 0);
        settle(&mut m, a, layout, &mut live);
        assert!(live.iter().any(|k| k.level == 0) && live.iter().any(|k| k.level >= 2));
        // Everything drawn is on the GPU (or empty), and nothing overlaps.
        let drawn: HashSet<NodeKey> = m.draw_list().iter().copied().collect();
        assert_eq!(drawn.len(), m.draw_list().len());
        assert!(drawn.iter().all(|k| m.ready.contains(k)));

        // Move far enough that most of the quadtree changes. While streaming,
        // every frame's draw list only has uploaded meshes and never two
        // meshes over the same chunk.
        let b = ChunkPos::new(30, -11);
        for _ in 0..20_000 {
            for e in m.update(b, layout) {
                match e {
                    MeshEvent::Upload(k, _) => assert!(live.insert(k)),
                    MeshEvent::Remove(k) => assert!(live.remove(&k)),
                }
            }
            let mut seen = HashSet::new();
            for k in m.draw_list() {
                assert!(m.ready.contains(k), "{k:?} drawn before it is ready");
                let (x0, z0) = k.min_chunk();
                for z in z0..z0 + k.chunks() {
                    for x in x0..x0 + k.chunks() {
                        assert!(seen.insert((x, z)), "two meshes over chunk {x},{z}");
                    }
                }
            }
            if m.stats.pending == 0 && m.stats.jobs_in_flight == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Settled: the GPU holds exactly the new leaves (old ones evicted).
        let leaves: HashSet<NodeKey> =
            layout.leaves(b, &|k| ChunkManager::in_world(m.generator(), k)).into_iter().collect();
        assert!(live.iter().all(|k| leaves.contains(k)), "stale meshes left on the GPU");
        assert!(m.ready.iter().all(|k| leaves.contains(k)));
    }
}
