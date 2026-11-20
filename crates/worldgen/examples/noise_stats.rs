//! Prints the value distribution of the climate noises. Used to calibrate
//! the GAIN_* constants in `terrain.rs` so each parameter spans about [-1, 1].
//!
//! cargo run --release -p worldgen --example noise_stats
use worldgen::noise::{DoubleNoise, OctaveConfig};
fn main() {
    let cfgs: &[(&str, i32, &[f64])] = &[
        ("cont", -12, &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]),
        ("eros", -11, &[1.0, 1.0, 1.0, 1.0, 1.0]),
        ("weird", -9, &[1.0, 1.0, 1.0, 1.0]),
        ("river", -11, &[1.0, 1.0, 1.0, 1.0]),
    ];
    for (name, fo, amps) in cfgs {
        let n = DoubleNoise::new(7, &OctaveConfig::new(*fo, amps));
        let mut v: Vec<f64> =
            (0..200_000).map(|i| n.sample_2d((i % 500) as f64 * 397.3, (i / 500) as f64 * 401.7)).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        let std = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "{name}: std {std:.3} p1 {:.3} p10 {:.3} p50 {:.3} p90 {:.3} p99 {:.3} min {:.3} max {:.3}",
            p(0.01),
            p(0.1),
            p(0.5),
            p(0.9),
            p(0.99),
            v[0],
            v[v.len() - 1]
        );
    }
}
