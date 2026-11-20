//! Chunk streaming: decides which chunks to generate, mesh and drop as the
//! camera moves, and runs that work on background threads.
//!
//! Pipeline for one chunk:
//!
//! ```text
//!   not loaded ──generate (worker)──▶ generated ──mesh (worker)──▶ meshed ──upload (main)──▶ drawn
//! ```
//!
//! A chunk can only be meshed once its 4 horizontal neighbours exist,
//! because faces on the chunk edge depend on the blocks next door. So we
//! generate one ring further out than we mesh.
//!
//! Work is ordered nearest-first and the number of jobs in flight is capped,
//! so the area around the player fills in first and the main thread never
//! stalls.

use crate::mesh::{mesh_chunk, MeshData, Neighbours};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use worldgen::{Chunk, ChunkPos, WorldGenerator};

enum JobResult {
    Generated(ChunkPos, Arc<Chunk>),
    Meshed(ChunkPos, MeshData),
}

/// What the renderer should do this frame.
pub enum MeshEvent {
    Upload(ChunkPos, MeshData),
    Remove(ChunkPos),
}

pub struct ChunkManager {
    gen: Arc<WorldGenerator>,
    pool: rayon::ThreadPool,
    tx: Sender<JobResult>,
    rx: Receiver<JobResult>,
    chunks: HashMap<ChunkPos, Arc<Chunk>>,
    generating: HashSet<ChunkPos>,
    meshing: HashSet<ChunkPos>,
    meshed: HashSet<ChunkPos>,
    /// Offsets within the current radius, sorted nearest first.
    spiral: Vec<(i32, i32)>,
    spiral_radius: i32,
    max_in_flight: usize,
    pub stats: Stats,
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub loaded_chunks: usize,
    pub meshed_chunks: usize,
    pub jobs_in_flight: usize,
    pub generated_total: u64,
    pub chunk_memory_bytes: usize,
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
            meshed: HashSet::new(),
            spiral: Vec::new(),
            spiral_radius: -1,
            max_in_flight: threads * 3,
            stats: Stats::default(),
        }
    }

    pub fn generator(&self) -> &WorldGenerator {
        &self.gen
    }

    fn ensure_spiral(&mut self, radius: i32) {
        if self.spiral_radius == radius {
            return;
        }
        let mut v = Vec::new();
        for z in -radius..=radius {
            for x in -radius..=radius {
                v.push((x, z));
            }
        }
        v.sort_by_key(|&(x, z)| x * x + z * z);
        self.spiral = v;
        self.spiral_radius = radius;
    }

    fn in_flight(&self) -> usize {
        self.generating.len() + self.meshing.len()
    }

    /// Advances streaming. `center` is the camera's chunk, `radius` the
    /// render distance in chunks.
    pub fn update(&mut self, center: ChunkPos, radius: i32) -> Vec<MeshEvent> {
        let mut events = Vec::new();
        let gen_radius = radius + 1;

        // 1. Collect finished work.
        while let Ok(res) = self.rx.try_recv() {
            match res {
                JobResult::Generated(pos, chunk) => {
                    self.generating.remove(&pos);
                    self.stats.generated_total += 1;
                    if pos.chebyshev(center) <= gen_radius + 1 {
                        self.stats.chunk_memory_bytes += chunk.memory_bytes();
                        self.chunks.insert(pos, chunk);
                    }
                }
                JobResult::Meshed(pos, mesh) => {
                    self.meshing.remove(&pos);
                    if pos.chebyshev(center) <= radius + 1 && self.chunks.contains_key(&pos) {
                        self.meshed.insert(pos);
                        events.push(MeshEvent::Upload(pos, mesh));
                    }
                }
            }
        }

        // 2. Drop what is now too far away (with one chunk of hysteresis so
        //    walking back and forth over a chunk border doesn't thrash).
        let mut freed = 0;
        self.chunks.retain(|p, c| {
            let keep = p.chebyshev(center) <= gen_radius + 1;
            if !keep {
                freed += c.memory_bytes();
            }
            keep
        });
        self.stats.chunk_memory_bytes = self.stats.chunk_memory_bytes.saturating_sub(freed);
        let far: Vec<ChunkPos> = self.meshed.iter().copied().filter(|p| p.chebyshev(center) > radius + 1).collect();
        for p in far {
            self.meshed.remove(&p);
            events.push(MeshEvent::Remove(p));
        }

        // 3. Schedule new work, nearest first.
        self.ensure_spiral(gen_radius);
        for i in 0..self.spiral.len() {
            if self.in_flight() >= self.max_in_flight {
                break;
            }
            let (dx, dz) = self.spiral[i];
            let pos = center.offset(dx, dz);
            let in_mesh_range = dx.abs() <= radius && dz.abs() <= radius;

            if !self.chunks.contains_key(&pos) {
                if !self.generating.contains(&pos) && self.gen.chunk_touches_world(pos) {
                    self.generating.insert(pos);
                    let (gen, tx) = (self.gen.clone(), self.tx.clone());
                    self.pool.spawn(move || {
                        let chunk = gen.generate_chunk(pos);
                        let _ = tx.send(JobResult::Generated(pos, Arc::new(chunk)));
                    });
                }
                continue;
            }

            if in_mesh_range && !self.meshed.contains(&pos) && !self.meshing.contains(&pos) {
                let nb = |dx, dz| -> Result<Option<Arc<Chunk>>, ()> {
                    let p = pos.offset(dx, dz);
                    match self.chunks.get(&p) {
                        Some(c) => Ok(Some(c.clone())),
                        None if !self.gen.chunk_touches_world(p) => Ok(None), // border: air
                        None => Err(()),                                      // not generated yet
                    }
                };
                let (Ok(nx), Ok(px), Ok(nz), Ok(pz)) = (nb(-1, 0), nb(1, 0), nb(0, -1), nb(0, 1)) else {
                    continue;
                };
                let chunk = self.chunks[&pos].clone();
                self.meshing.insert(pos);
                let tx = self.tx.clone();
                self.pool.spawn(move || {
                    let mesh = mesh_chunk(
                        &chunk,
                        &Neighbours {
                            neg_x: nx.as_deref(),
                            pos_x: px.as_deref(),
                            neg_z: nz.as_deref(),
                            pos_z: pz.as_deref(),
                        },
                    );
                    let _ = tx.send(JobResult::Meshed(pos, mesh));
                });
            }
        }

        self.stats.loaded_chunks = self.chunks.len();
        self.stats.meshed_chunks = self.meshed.len();
        self.stats.jobs_in_flight = self.in_flight();
        events
    }

    /// Chunks within `radius` that still need generating or meshing.
    pub fn pending_near(&self, center: ChunkPos, radius: i32) -> usize {
        let mut n = 0;
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                let p = center.offset(dx, dz);
                if self.gen.chunk_touches_world(p) && !self.meshed.contains(&p) {
                    n += 1;
                }
            }
        }
        n
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
            for e in m.update(center, 3) {
                if let MeshEvent::Upload(p, _) = e {
                    assert!(uploaded.insert(p), "{p:?} meshed twice");
                }
            }
            if m.pending_near(center, 3) == 0 {
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
            for e in m.update(center, 2) {
                if let MeshEvent::Upload(p, _) = e {
                    uploaded.insert(p);
                }
            }
            if m.pending_near(center, 2) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(uploaded.len(), 3 * 5);
        assert!(uploaded.iter().all(|p| p.x <= 31));
    }
}
