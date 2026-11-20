# Roadmap

Goal: a Minecraft-like world generator, accurate first, fast second, pretty
third. Rust + Vulkan, Windows and Linux (macOS a plus), smooth on an RTX 2080.
World size from 1 km to the full 60,000 km Minecraft world.

The core idea: the world is never stored. Every block is computed from the
seed when it is needed, so a 60,000 km world costs the same as a 1 km one.

| Step | What | Status |
|------|------|--------|
| 1 | Foundation: project layout, world settings, chunk storage, generator skeleton, game menu, Vulkan renderer, map tool | **Done**, see [devlog 01](devlog/01-foundation.md) |
| 2 | Terrain: continents, oceans, erosion, mountains, rivers, 3D overhangs | **Done**, see [devlog 02](devlog/02-terrain.md) |
| 3 | Biomes and trees | Next |
| 4 | Scale and speed: LODs, faster meshing, GPU-driven drawing, world map | Planned |

## Step 1: Foundation ✔

- Cargo workspace with three crates (`worldgen`, `wg-map`, `game`).
- Deterministic random numbers and Improved Perlin + fractal noise.
- Palette-compressed 16³ sections, 16×384×16 chunks.
- World settings: seed, size (1 km–60,000 km), type (Normal, Amplified,
  Large biomes, Islands, Flat). World border enforced everywhere.
- Placeholder heightmap terrain with beaches, sea floor, snow, bedrock.
- `wg-map` CLI for top-down maps and benchmarks.
- Game: main menu, create-world screen with live map preview, settings
  screen, pause menu, debug overlay; fly camera.
- Vulkan renderer: greedy-meshed chunks streamed on worker threads,
  staging uploads to GPU memory, frustum culling, camera-relative rendering.

## Step 2: Terrain ✔

Replace the placeholder height with the layered "climate parameter" model
modern Minecraft popularised:

- **Continentalness** (ocean ↔ coast ↔ inland), **erosion** (flat ↔ rugged),
  **peaks & valleys / weirdness** (ridges), each a large-scale `DoubleNoise`.
- Spline curves map those parameters to base height, squash factor and
  roughness, so the shape is tunable without touching noise.
- 3D density noise on top of that for cliffs, overhangs and arches.
- Deep oceans, continental shelves, beaches, river channels.
- World types re-expressed through the same model (Amplified = steeper
  splines, Islands = continentalness biased to ocean, Large biomes = ×4 scale).
- Accuracy checks: height histograms, land/ocean ratio per world type,
  slope statistics, and maps at several zoom levels in the devlog.

Result: continents several km wide (about 58% land in Normal, 13% in
Islands), oceans down to y ≈ 13, mountains to y ≈ 270–300, rivers to the
sea, overhangs in rugged areas. Known limitation: rivers sit at sea level
(no downhill flow from plateaus).

## Step 3: Biomes and trees

- Biome chosen by nearest point in a 6D parameter space (temperature,
  humidity, continentalness, erosion, weirdness, depth). Biome boundaries are
  smooth, and the same seed always gives the same biome map.
- Minecraft-style biomes (plains, forest, birch forest, dark forest, taiga,
  snowy plains, desert, savanna, jungle, swamp, badlands, mushroom fields,
  stony peaks, meadow, ocean variants, …) plus real-world biomes from the
  Whittaker climate diagram (steppe, tundra, mangrove, salt flats, fjords,
  karst, volcanic, temperate rainforest, cloud forest, …).
- Per-biome surface rules (grass, sand, snow, terracotta layers, …).
- Simple blocky trees per biome, placed with deterministic, evenly spaced
  sampling (jittered grid / Poisson-disk style) and generated across chunk
  borders without seams.
- Biome map output in `wg-map` and a biome name in the debug overlay.

## Step 4: Scale and speed

- **LODs:** rings of coarser chunks (2×, 4×, 8×, … block size) out to the
  horizon, generated from the same column sampler, in the style of the
  Distant Horizons mod.
- **Binary greedy meshing** (bitmask face culling) and packed 8-byte vertices.
- **GPU-driven drawing:** one big vertex/index pool, indirect draws.
- Chunk cache and unloading tuned for 32+ chunk render distance.
- In-game world map (top-down, zoomable to the whole world).
- Performance budget verified against an RTX 2080: 60 FPS at render distance 16.
