//! Finds the nearest column of a biome, searching outward from (0, 0).
//! Handy for screenshots and for checking a biome by eye in the game:
//!
//! cargo run --release -p worldgen --example find_biome -- 42 jungle
//! WG_AUTOSTART=42 WG_CAMERA=<x>,<z> cargo run --release
use worldgen::{Biome, WorldGenerator, WorldSettings, WorldSize};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(seed), Some(name)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: find_biome SEED BIOME_NAME   (biomes: {})", names());
        std::process::exit(2)
    };
    let seed = worldgen::settings::seed_from_text(seed).unwrap_or(0);
    let Some(biome) = Biome::ALL.iter().copied().find(|b| b.name() == name.as_str()) else {
        eprintln!("unknown biome {name}; one of: {}", names());
        std::process::exit(2)
    };
    let g = WorldGenerator::new(WorldSettings { seed, size: WorldSize::full(), ..Default::default() });
    // Square rings of 32-block steps out to 20 km.
    for r in 0..=625 {
        for i in -r..=r {
            for (x, z) in [(i, -r), (i, r), (-r, i), (r, i)] {
                let s = g.sample_column(x * 32, z * 32);
                if s.biome == biome {
                    println!("{} at x={} z={} (surface y={})", biome.name(), x * 32, z * 32, s.surface_y);
                    return;
                }
            }
        }
    }
    println!("no {} within 20 km", biome.name());
}

fn names() -> String {
    Biome::ALL.iter().map(|b| b.name()).collect::<Vec<_>>().join(", ")
}
