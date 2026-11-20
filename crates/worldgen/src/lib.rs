//! # worldgen
//!
//! A deterministic Minecraft-like world generator with no graphics code.
//! Everything here can run headless (tests, the `wg-map` tool, servers).
//!
//! Module map:
//! * [`rng`] – seeded random numbers and positional hashing
//! * [`noise`] – Perlin noise and fractal (octave) noise
//! * [`block`] – block ids and their properties
//! * [`palette`] – compressed 16³ block storage
//! * [`chunk`] – 16×384×16 chunk columns
//! * [`settings`] – world size, type and seed
//! * [`spline`] – monotone cubic curves used to shape terrain
//! * [`terrain`] – climate parameters → height, roughness, rivers
//! * [`generator`] – turns settings + chunk position into blocks

pub mod block;
pub mod chunk;
pub mod generator;
pub mod noise;
pub mod palette;
pub mod rng;
pub mod settings;
pub mod spline;
pub mod terrain;

pub use block::{blocks, BlockId};
pub use chunk::{Chunk, ChunkPos, CHUNK_SIZE};
pub use generator::{ColumnSample, WorldGenerator};
pub use settings::{WorldSettings, WorldSize, WorldType, MAX_Y, MIN_Y, SEA_LEVEL, WORLD_HEIGHT};
