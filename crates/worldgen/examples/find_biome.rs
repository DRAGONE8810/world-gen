//! Finds the nearest column of a biome, searching outward from (0, 0).
//! Handy for screenshots and for checking a biome by eye in the game:
//!
//! cargo run --release -p worldgen --example find_biome -- 42 jungle [WORLD_HEIGHT] [PLANET]
//! WG_AUTOSTART=42 WG_CAMERA=<x>,<z> cargo run --release
//!
//! With a planet (continents, pangea, earth, islands) the search runs on a
//! full-size realistic world (Balanced quality), from the spawn point.
//!
//! Tall worlds stretch the terrain sideways too, so the search step and
//! radius grow with the world height.
use worldgen::settings::{Planet, SimQuality, WorldHeight};
use worldgen::{Biome, Generator, WorldGenerator, WorldSettings, WorldSize};

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
    let height = WorldHeight::from_blocks(args.get(3).and_then(|h| h.parse().ok()).unwrap_or(384));
    let planet = args.get(4).and_then(|p| match p.as_str() {
        "continents" => Some(Planet::Continents),
        "pangea" => Some(Planet::Pangea),
        "earth" => Some(Planet::Earth),
        "islands" => Some(Planet::Islands),
        "mushoku" => Some(Planet::Mushoku),
        _ => None,
    });
    let mut settings = WorldSettings { seed, size: WorldSize::full(), height, ..Default::default() };
    if let Some(planet) = planet {
        settings.generator = Generator::Realistic;
        settings.planet = planet;
        settings.quality = SimQuality::Balanced;
    }
    let g = WorldGenerator::new(settings);
    // Square rings out to 20 km (× terrain scale), 32 blocks (× scale)
    // apart; on a planet, out to 30,000 km, 20 km apart.
    let (step, rings) =
        if planet.is_some() { (20_000, 1500) } else { ((32.0 * height.terrain_scale()).round() as i32, 625) };
    let (cx, cz) = if planet.is_some() { (g.find_spawn().0, g.find_spawn().2) } else { (0, 0) };
    for r in 0..=rings {
        for i in -r..=r {
            for (x, z) in [(i, -r), (i, r), (-r, i), (r, i)] {
                let (x, z) = (cx + x * step, cz + z * step);
                let s = g.sample_column(x, z);
                if s.inside && s.biome == biome {
                    println!("{} at x={} z={} (surface y={})", biome.name(), x, z, s.surface_y);
                    return;
                }
            }
        }
    }
    println!("no {} within {} km", biome.name(), rings as i64 * step as i64 / 1000);
}

fn names() -> String {
    Biome::ALL.iter().map(|b| b.name()).collect::<Vec<_>>().join(", ")
}
