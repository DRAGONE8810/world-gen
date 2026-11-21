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
use worldgen::settings::WorldHeight;
use worldgen::{
    blocks, Biome, ColumnSample, Generator, Planet, SimQuality, WorldGenerator, WorldSettings, WorldSize, WorldType,
    SEA_LEVEL,
};

struct Args {
    seed: u64,
    world_type: WorldType,
    size_km: f64,
    height: u32,
    center: (i32, i32),
    extent: u32,
    px: u32,
    out: String,
    bench: Option<u32>,
    layer: Layer,
    features: bool,
    generator: Generator,
    planet: Planet,
    quality: SimQuality,
    whole: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Layer {
    Surface,
    Height,
    Continentalness,
    Erosion,
    PeaksValleys,
    Roughness,
    Biome,
    Temperature,
    Humidity,
}

fn usage() -> ! {
    eprintln!(
        "usage: wg-map [--seed N|TEXT] [--type normal|amplified|large|islands|flat] [--size-km KM] [--height BLOCKS]\n\
         \x20             [--center X,Z] [--extent BLOCKS] [--px PIXELS] [--out FILE.png]\n\
         \x20             [--layer surface|height|biome|continentalness|erosion|pv|roughness|temperature|humidity]\n\
         \x20             [--no-trees] [--legend]\n\
         \x20             [--realistic continents|pangea|earth|islands] [--quality fast|balanced|quality] [--whole]\n\
         \x20      wg-map --bench N   (time generating an N×N square of chunks, and N LOD tiles per level, on one thread)"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        seed: 0,
        world_type: WorldType::Normal,
        size_km: 16.0,
        height: 384,
        center: (0, 0),
        extent: 4096,
        px: 1024,
        out: "map.png".into(),
        bench: None,
        layer: Layer::Surface,
        features: true,
        generator: Generator::Classic,
        planet: Planet::Continents,
        quality: SimQuality::Balanced,
        whole: false,
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
            "--height" => a.height = val().parse().unwrap_or_else(|_| usage()),
            "--center" => {
                let v = val();
                let (x, z) = v.split_once(',').unwrap_or_else(|| usage());
                a.center = (x.trim().parse().unwrap_or_else(|_| usage()), z.trim().parse().unwrap_or_else(|_| usage()));
            }
            "--extent" => a.extent = val().parse().unwrap_or_else(|_| usage()),
            "--px" => a.px = val().parse().unwrap_or_else(|_| usage()),
            "--out" => a.out = val(),
            "--layer" => {
                a.layer = match val().as_str() {
                    "surface" => Layer::Surface,
                    "height" => Layer::Height,
                    "continentalness" | "c" => Layer::Continentalness,
                    "erosion" | "e" => Layer::Erosion,
                    "pv" | "peaks" => Layer::PeaksValleys,
                    "roughness" => Layer::Roughness,
                    "biome" | "biomes" => Layer::Biome,
                    "temperature" | "t" => Layer::Temperature,
                    "humidity" | "h" => Layer::Humidity,
                    _ => usage(),
                }
            }
            "--no-trees" => a.features = false,
            "--realistic" => {
                a.generator = Generator::Realistic;
                a.planet = match val().as_str() {
                    "continents" => Planet::Continents,
                    "pangea" => Planet::Pangea,
                    "earth" => Planet::Earth,
                    "islands" => Planet::Islands,
                    _ => usage(),
                };
            }
            "--quality" => {
                a.quality = match val().as_str() {
                    "fast" => SimQuality::Fast,
                    "balanced" => SimQuality::Balanced,
                    "quality" => SimQuality::Quality,
                    _ => usage(),
                };
            }
            "--whole" => a.whole = true,
            "--bench" => a.bench = Some(val().parse().unwrap_or_else(|_| usage())),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    a
}

fn main() {
    let mut args = parse_args();
    let created = std::time::Instant::now();
    let gen = WorldGenerator::new(WorldSettings {
        seed: args.seed,
        size: WorldSize::from_km(args.size_km),
        world_type: args.world_type,
        features: args.features,
        height: WorldHeight::from_blocks(args.height),
        generator: args.generator,
        planet: args.planet,
        quality: args.quality,
    });
    if let Some(r) = gen.realistic() {
        let (w, h) = r.grid_size();
        eprintln!(
            "planet simulated in {:.2?}: {w}x{h} grid, {:.1} km per cell, {} river reaches",
            created.elapsed(),
            r.cell_blocks() / 1000.0,
            r.river_count()
        );
    }
    if args.whole {
        let size = gen.settings().size;
        args.extent = size.max_extent();
        args.center = (0, 0);
    }
    if let Some(n) = args.bench {
        bench(&gen, n as i32);
        bench_lod(&gen, n as i32);
        return;
    }
    let px = args.px.max(16);
    // A whole 2:1 world (Earth) is drawn 2:1; everything else square.
    let pz = if args.whole {
        let size = gen.settings().size;
        ((px as f64 * size.depth_blocks() as f64 / size.width_blocks() as f64).round() as u32).max(16)
    } else {
        px
    };
    let blocks_per_px = args.extent as f64 / px as f64;
    let x0 = args.center.0 as f64 - args.extent as f64 / 2.0;
    let z0 = args.center.1 as f64 - pz as f64 * blocks_per_px / 2.0;

    let start = std::time::Instant::now();
    // One extra row/column so every pixel has a south-east neighbour for shading.
    let samples: Vec<_> = (0..(px + 1) * (pz + 1))
        .into_par_iter()
        .map(|i| {
            let (ix, iz) = (i % (px + 1), i / (px + 1));
            let x = (x0 + ix as f64 * blocks_per_px).floor() as i32;
            let z = (z0 + iz as f64 * blocks_per_px).floor() as i32;
            gen.sample_column(x, z)
        })
        .collect();

    let mut img = image::RgbImage::new(px, pz);
    let mut land = 0u64;
    let mut inside = 0u64;
    let mut heights = Vec::with_capacity((px * pz) as usize);
    let mut biome_count = vec![0u64; Biome::ALL.len()];
    for iz in 0..pz {
        for ix in 0..px {
            let s = samples[(iz * (px + 1) + ix) as usize];
            if s.inside {
                inside += 1;
                land += !s.underwater as u64;
                heights.push(s.surface_y);
                biome_count[s.biome as usize] += 1;
            }
            let se = samples[((iz + 1) * (px + 1) + ix + 1) as usize];
            let color = pixel_color(args.layer, &s, &se, blocks_per_px, gen.realistic().is_some());
            img.put_pixel(ix, iz, image::Rgb(color));
        }
    }
    img.save(&args.out).expect("failed to write image");
    eprintln!(
        "wrote {} ({}x{} px, {:.1} blocks/px, {} columns in {:.2?}); land {:.1}% of world area shown",
        args.out,
        px,
        pz,
        blocks_per_px,
        samples.len(),
        start.elapsed(),
        if inside > 0 { land as f64 * 100.0 / inside as f64 } else { 0.0 }
    );
    if !heights.is_empty() {
        heights.sort_unstable();
        let p = |q: f64| heights[((heights.len() - 1) as f64 * q) as usize];
        eprintln!(
            "surface height: min {} p10 {} p50 {} p90 {} p99 {} max {}",
            heights[0],
            p(0.1),
            p(0.5),
            p(0.9),
            p(0.99),
            heights[heights.len() - 1]
        );
    }
    if args.layer == Layer::Biome && inside > 0 {
        // Share of the map area per biome, largest first.
        let mut v: Vec<_> = Biome::ALL.iter().map(|b| (biome_count[*b as usize], *b)).collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.0));
        let line: Vec<String> = v
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, b)| format!("{} {:.2}%", b.name(), *n as f64 * 100.0 / inside as f64))
            .collect();
        eprintln!("biomes: {}", line.join(", "));
        let missing: Vec<_> = v.iter().filter(|(n, _)| *n == 0).map(|(_, b)| b.name()).collect();
        if !missing.is_empty() {
            eprintln!("not in this area: {}", missing.join(", "));
        }
    }
}

/// Single-threaded chunk generation benchmark, around the spawn point
/// (on land: a realistic world's origin is often open ocean).
fn bench(gen: &WorldGenerator, n: i32) {
    let (sx, _, sz) = gen.find_spawn();
    let c = worldgen::ChunkPos::from_block(sx, sz);
    let start = std::time::Instant::now();
    let mut bytes = 0usize;
    for cz in 0..n {
        for cx in 0..n {
            bytes += gen.generate_chunk(worldgen::ChunkPos::new(c.x + cx - n / 2, c.z + cz - n / 2)).memory_bytes();
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

/// Single-threaded LOD sampling benchmark: one tile = an 18 × 18 grid of
/// cells (16 × 16 plus a border), as the game samples it.
fn bench_lod(gen: &WorldGenerator, n: i32) {
    let (sx, _, sz) = gen.find_spawn();
    for level in [1u32, 2, 3, 4, 6, 8] {
        let cell = 1i32 << level;
        let tile = 16 * cell;
        let start = std::time::Instant::now();
        let mut canopy = 0;
        for i in 0..n {
            // Tiles along a line through the world, like a ring of the view.
            let grid = gen.lod_grid(sx + i * tile - cell, sz - cell, cell, 18);
            canopy += grid.iter().filter(|c| c.block.is_tree_part()).count();
        }
        let t = start.elapsed();
        eprintln!(
            "LOD level {level} ({cell}-block cells): {:.2} ms/tile on one thread ({canopy} canopy cells)",
            t.as_secs_f64() * 1000.0 / n as f64
        );
    }
}

/// Colour of one map pixel for the chosen layer.
fn pixel_color(layer: Layer, s: &ColumnSample, se: &ColumnSample, blocks_per_px: f64, realistic: bool) -> [u8; 3] {
    if !s.inside {
        return [12, 12, 16];
    }
    // Hill shading: brighter on slopes facing the north-west light. Maps
    // of whole realistic planets need it exaggerated to show anything.
    let slope = if realistic {
        (s.surface_y - se.surface_y) as f64 / blocks_per_px.max(1.0) * 3.0 * (blocks_per_px / 16.0).max(1.0).sqrt()
    } else {
        (s.surface_y - se.surface_y) as f64 / blocks_per_px.max(1.0) / s.shape.scale() * 0.35
    };
    let shade = (1.0 + slope).clamp(0.55, 1.4);
    let shaded = |c: [u8; 3]| c.map(|v| (v as f64 * shade).clamp(0.0, 255.0) as u8);
    let water = |depth: i32| {
        let d = (depth.clamp(0, 60) as f64) / 60.0;
        let w = blocks::WATER.info().color;
        let f = 1.15 - d * 0.6;
        [(w[0] as f64 * f) as u8, (w[1] as f64 * f) as u8, (w[2] as f64 * f).min(255.0) as u8]
    };
    // Diverging blue → white → red ramp for parameters in [-1, 1].
    let ramp = |v: f64| {
        let t = v.clamp(-1.0, 1.0);
        if t < 0.0 {
            let k = 1.0 + t;
            [(40.0 + 215.0 * k) as u8, (80.0 + 175.0 * k) as u8, 255]
        } else {
            [255, (255.0 - 190.0 * t) as u8, (255.0 - 215.0 * t) as u8]
        }
    };
    match layer {
        Layer::Surface => {
            if s.ice {
                shaded(blocks::ICE.info().color)
            } else if s.underwater {
                water(((s.shape.water - s.surface_y) as f64 / s.shape.scale()) as i32)
            } else {
                shaded(s.top_color())
            }
        }
        Layer::Biome => {
            // Flat biome colours, lightly hill-shaded so relief stays readable.
            let c = s.biome.map_color();
            let k = (1.0 + slope * 0.43).clamp(0.8, 1.15);
            c.map(|v| (v as f64 * k).clamp(0.0, 255.0) as u8)
        }
        Layer::Temperature => ramp(s.shape.climate.temperature),
        Layer::Humidity => ramp(s.shape.climate.humidity),
        Layer::Height => {
            if s.underwater {
                water(((s.shape.water - s.surface_y) as f64 / s.shape.scale()) as i32)
            } else {
                // Hypsometric tint: green lowlands, yellow/brown hills, grey
                // mountains, white peaks.
                // In default-world blocks, so tall worlds use the same tints.
                let h = s.shape.normalized_y(s.surface_y as f64) - SEA_LEVEL as f64;
                let stops: [(f64, [f64; 3]); 6] = [
                    (0.0, [70.0, 140.0, 60.0]),
                    (25.0, [140.0, 170.0, 80.0]),
                    (60.0, [190.0, 160.0, 100.0]),
                    (110.0, [140.0, 110.0, 90.0]),
                    (160.0, [150.0, 150.0, 150.0]),
                    (220.0, [250.0, 250.0, 250.0]),
                ];
                let mut c = stops[stops.len() - 1].1;
                for w in stops.windows(2) {
                    if h <= w[1].0 {
                        let t = ((h - w[0].0) / (w[1].0 - w[0].0)).clamp(0.0, 1.0);
                        c = [0, 1, 2].map(|i| w[0].1[i] + t * (w[1].1[i] - w[0].1[i]));
                        break;
                    }
                }
                shaded(c.map(|v| v as u8))
            }
        }
        Layer::Continentalness => ramp(s.shape.climate.continentalness),
        Layer::Erosion => ramp(s.shape.climate.erosion),
        Layer::PeaksValleys => ramp(s.shape.climate.peaks_valleys),
        Layer::Roughness => ramp(s.shape.roughness / 12.0 - 1.0),
    }
}
