//! Noise functions: the raw material of procedural terrain.

mod octave;
mod perlin;

pub use octave::{wrap, DoubleNoise, OctaveConfig, OctaveNoise};
pub use perlin::ImprovedPerlin;
