//! `wg-map`: draws a top-down map of a world as a PNG.
//!
//! It calls exactly the same `WorldGenerator::sample_column` the game uses,
//! so the map is a faithful picture of the generated world. Colours come
//! from the top block, water is tinted by depth, and land is hill-shaded
//! (lit from the north-west) so relief is visible.
//!
//! ```text
//! cargo run --release -p wg-map -- --seed 42 --extent 8000 --px 1024 --out map.png
//! ```

use rayon::prelude::*;
use worldgen::{blocks, WorldGenerator, WorldSettings, WorldSize, WorldType, SEA_LEVEL};

struct Args {
    seed: u64,
    world_type: WorldType,
    size_km: f64,
    center: (i32, i32),
    extent: u32,
    px: u32,
    out: String,
    bench: Option<u32>,
}

fn usage() -> ! {
    eprintln!(
        "usage: wg-map [--seed N|TEXT] [--type normal|amplified|large|islands|flat] [--size-km KM]\n\
         \x20             [--center X,Z] [--extent BLOCKS] [--px PIXELS] [--out FILE.png]\n\
         \x20      wg-map --bench N   (time generating an N×N square of chunks on one thread)"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        seed: 0,
        world_type: WorldType::Normal,
        size_km: 16.0,
        center: (0, 0),
        extent: 4096,
        px: 1024,
        out: "map.png".into(),
        bench: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--seed" => a.seed = worldgen::settings::seed_from_text(&val()).unwrap_or(0),
            "--type" => {
                a.world_type = match val().as_str() {
                    "normal" => WorldType::Normal,
                    "amplified" => WorldType::Amplified,
                    "large" => WorldType::LargeBiomes,
                    "islands" => WorldType::Islands,
                    "flat" => WorldType::Flat,
                    _ => usage(),
                }
            }
            "--size-km" => a.size_km = val().parse().unwrap_or_else(|_| usage()),
            "--center" => {
                let v = val();
                let (x, z) = v.split_once(',').unwrap_or_else(|| usage());
                a.center = (x.trim().parse().unwrap_or_else(|_| usage()), z.trim().parse().unwrap_or_else(|_| usage()));
            }
            "--extent" => a.extent = val().parse().unwrap_or_else(|_| usage()),
            "--px" => a.px = val().parse().unwrap_or_else(|_| usage()),
            "--out" => a.out = val(),
            "--bench" => a.bench = Some(val().parse().unwrap_or_else(|_| usage())),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let gen = WorldGenerator::new(WorldSettings {
        seed: args.seed,
        size: WorldSize::from_km(args.size_km),
        world_type: args.world_type,
        features: true,
    });
    if let Some(n) = args.bench {
        bench(&gen, n as i32);
        return;
    }
    let px = args.px.max(16);
    let blocks_per_px = args.extent as f64 / px as f64;
    let x0 = args.center.0 as f64 - args.extent as f64 / 2.0;
    let z0 = args.center.1 as f64 - args.extent as f64 / 2.0;

    let start = std::time::Instant::now();
    // One extra row/column so every pixel has a south-east neighbour for shading.
    let samples: Vec<_> = (0..(px + 1) * (px + 1))
        .into_par_iter()
        .map(|i| {
            let (ix, iz) = (i % (px + 1), i / (px + 1));
            let x = (x0 + ix as f64 * blocks_per_px).floor() as i32;
            let z = (z0 + iz as f64 * blocks_per_px).floor() as i32;
            gen.sample_column(x, z)
        })
        .collect();

    let mut img = image::RgbImage::new(px, px);
    let mut land = 0u64;
    let mut inside = 0u64;
    for iz in 0..px {
        for ix in 0..px {
            let s = samples[(iz * (px + 1) + ix) as usize];
            let color = if !s.inside {
                [12, 12, 16]
            } else if s.underwater {
                inside += 1;
                // Darker blue for deeper water.
                let depth = (SEA_LEVEL - s.surface_y).clamp(0, 60) as f64 / 60.0;
                let w = blocks::WATER.info().color;
                let f = 1.15 - depth * 0.6;
                [(w[0] as f64 * f) as u8, (w[1] as f64 * f) as u8, (w[2] as f64 * f).min(255.0) as u8]
            } else {
                inside += 1;
                land += 1;
                let se = samples[((iz + 1) * (px + 1) + ix + 1) as usize];
                let slope = (s.surface_y - se.surface_y) as f64 / blocks_per_px.max(1.0);
                let shade = (1.0 + slope * 0.35).clamp(0.55, 1.35);
                let c = s.top_block.info().color;
                c.map(|v| (v as f64 * shade).clamp(0.0, 255.0) as u8)
            };
            img.put_pixel(ix, iz, image::Rgb(color));
        }
    }
    img.save(&args.out).expect("failed to write image");
    eprintln!(
        "wrote {} ({}x{} px, {:.1} blocks/px, {} columns in {:.2?}); land {:.1}% of world area shown",
        args.out,
        px,
        px,
        blocks_per_px,
        samples.len(),
        start.elapsed(),
        if inside > 0 { land as f64 * 100.0 / inside as f64 } else { 0.0 }
    );
}

/// Single-threaded chunk generation benchmark.
fn bench(gen: &WorldGenerator, n: i32) {
    let start = std::time::Instant::now();
    let mut bytes = 0usize;
    for cz in 0..n {
        for cx in 0..n {
            bytes += gen.generate_chunk(worldgen::ChunkPos::new(cx - n / 2, cz - n / 2)).memory_bytes();
        }
    }
    let total = (n * n) as f64;
    let t = start.elapsed();
    eprintln!(
        "{} chunks in {:.2?}: {:.3} ms/chunk on one thread, {:.1} KiB/chunk stored",
        n * n,
        t,
        t.as_secs_f64() * 1000.0 / total,
        bytes as f64 / total / 1024.0
    );
}
