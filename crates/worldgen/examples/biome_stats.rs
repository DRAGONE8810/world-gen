//! Share of land covered by each biome, and how wide the transition zones
//! between biomes are.
//!
//! cargo run --release -p worldgen --example biome_stats -- 42            # classic, 48 km square
//! cargo run --release -p worldgen --example biome_stats -- 42 continents # realistic planet
//!
//! "Mixing" is measured along rows of columns 1 block apart: where the
//! biome changes, how often it changes back within the next 64 blocks.
//! Hard borders give ~0 %, transition zones (interleaved patches) much more.
use std::collections::HashMap;
use worldgen::settings::{Planet, SimQuality, WorldHeight};
use worldgen::{Biome, Generator, WorldGenerator, WorldSettings, WorldSize};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seed = args.get(1).and_then(|s| worldgen::settings::seed_from_text(s)).unwrap_or(42);
    let planet = args.get(2).and_then(|p| match p.as_str() {
        "continents" => Some(Planet::Continents),
        "pangea" => Some(Planet::Pangea),
        "earth" => Some(Planet::Earth),
        "islands" => Some(Planet::Islands),
        _ => None,
    });
    let settings = match planet {
        Some(planet) => WorldSettings {
            seed,
            size: WorldSize::full(),
            height: WorldHeight::from_blocks(17_696),
            generator: Generator::Realistic,
            planet,
            quality: SimQuality::Fast,
            ..Default::default()
        },
        None => WorldSettings { seed, size: WorldSize::full(), ..Default::default() },
    };
    let g = WorldGenerator::new(settings);
    // A grid over the area: 48 km for classic worlds, the whole planet for
    // realistic ones (avoiding the outermost tenth, near the poles).
    let (half, n) = if planet.is_some() { (27_000_000, 400) } else { (24_000, 400) };
    let step = 2 * half / n;
    let mut count: HashMap<Biome, u64> = HashMap::new();
    let mut land = 0u64;
    for iz in 0..n {
        for ix in 0..n {
            let s = g.sample_column(-half + ix * step, -half + iz * step);
            if s.inside && !s.underwater {
                *count.entry(s.biome).or_default() += 1;
                land += 1;
            }
        }
    }
    let mut v: Vec<_> = count.into_iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    println!("{} land samples", land);
    for (b, c) in &v {
        println!("{:>24} {:5.2} %", b.name(), 100.0 * *c as f64 / land as f64);
    }
    let missing: Vec<_> = Biome::ALL.iter().filter(|b| !v.iter().any(|(x, _)| x == *b)).map(|b| b.name()).collect();
    println!("not seen: {missing:?}");

    // Transition zones: along rows, at each change of land biome, does the
    // previous biome come back within 64 blocks?
    let (mut changes, mut returns) = (0u64, 0u64);
    for row in 0..60 {
        let z = -half + row * (2 * half / 60);
        let x0 = -half / 2 + row * 997;
        let col: Vec<_> = (0..20_000).map(|i| g.sample_column(x0 + i, z)).collect();
        for i in 1..col.len() - 64 {
            let (a, b) = (&col[i - 1], &col[i]);
            if a.underwater || b.underwater || a.biome == b.biome {
                continue;
            }
            changes += 1;
            returns += col[i + 1..i + 64].iter().any(|c| c.biome == a.biome) as u64;
        }
    }
    println!(
        "biome changes along rows: {changes}, of which {:.0} % switch back within 64 blocks (interleaving)",
        100.0 * returns as f64 / changes.max(1) as f64
    );
}
