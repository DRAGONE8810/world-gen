//! Continent count and coastline complexity of generated planets, next to
//! the real Earth's at the same resolution.
//!
//! cargo run --release -p worldgen --example continent_stats -- [planet] [first seed] [count]

use worldgen::realistic::{earth, grid::Grid, tectonics};
use worldgen::settings::Planet;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let planet = match args.get(1).map(String::as_str) {
        Some("pangea") => Planet::Pangea,
        Some("islands") => Planet::Islands,
        Some("mushoku") => Planet::Mushoku,
        _ => Planet::Continents,
    };
    let first: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let count: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12);
    // ~52 km cells, like the Balanced simulation.
    let (w, km) = (768, 40_000.0 / 768.0);

    // Earth: average the 0.1° relief into 52 km cells (5 × 5 of 11 km).
    let r = earth::relief();
    let (ew, eh) = (r.w / 5, r.h / 5);
    let mut e = Grid::new(ew, eh, 0.0f32);
    for j in 0..eh {
        for i in 0..ew {
            let mut s = 0.0;
            for b in 0..5 {
                for a in 0..5 {
                    s += r.get(i * 5 + a, j * 5 + b) as f64;
                }
            }
            e.set(i, j, (s / 25.0) as f32);
        }
    }
    let s = tectonics::land_shape(&e);
    let show = |name: &str, s: &tectonics::LandShape| {
        println!(
            "{name}: {} continents (≥2% of land), coast index {:.1}, islands {:.0}% of land, largest {:.0}%, land {:.0}%",
            s.continents(0.02),
            s.coast_index(0.02),
            100.0 * s.island_share(0.02),
            100.0 * s.masses[0].0 as f64 / s.masses.iter().map(|m| m.0).sum::<usize>() as f64,
            100.0 * s.land_share()
        )
    };
    show("Earth", &s);

    let mut total = 0.0;
    for seed in first..first + count {
        let t = tectonics::build(seed, planet, w, w, km);
        let s = tectonics::land_shape(&t.elevation);
        total += s.coast_index(0.02);
        show(&format!("seed {seed}"), &s);
        // WG_DUMP=dir writes each planet's land/sea as a PGM image.
        if let Ok(dir) = std::env::var("WG_DUMP") {
            let mut out = format!("P5 {w} {w} 255\n").into_bytes();
            out.extend(t.elevation.data.iter().map(|&e| {
                if e >= 0.0 {
                    255u8
                } else if e > -1000.0 {
                    128
                } else {
                    0
                }
            }));
            std::fs::write(format!("{dir}/seed{seed}.pgm"), out).unwrap();
            let mut out = format!("P5 {w} {w} 255\n").into_bytes();
            out.extend(t.crust.data.iter().map(|&c| (c * 255.0) as u8));
            std::fs::write(format!("{dir}/crust{seed}.pgm"), out).unwrap();
        }
    }
    println!("mean coast index {:.1}", total / count as f64);
}
