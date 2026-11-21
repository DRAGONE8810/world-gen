# Roadmap

Goal: a Minecraft-like world generator, accurate first, fast second, pretty
third. Rust + Vulkan, Windows and Linux (macOS a plus), smooth on an RTX 2080.
World size from 1 km to the full 60,000 km Minecraft world; world height
up to 17,696 blocks (twice Mount Everest) from step 4.

The core idea: the world is never stored. Every block is computed from the
seed when it is needed, so a 60,000 km world costs the same as a 1 km one.

| Step | What | Status |
|------|------|--------|
| 1 | Foundation: project layout, world settings, chunk storage, generator skeleton, game menu, Vulkan renderer, map tool | **Done**, see [devlog 01](devlog/01-foundation.md) |
| 2 | Terrain: continents, oceans, erosion, mountains, rivers, 3D overhangs | **Done**, see [devlog 02](devlog/02-terrain.md) |
| 3 | Biomes and trees | **Done**, see [devlog 03](devlog/03-biomes.md) |
| 4 | Scale and speed: tall worlds (cubic chunks), LODs to 256 chunks, faster meshing, GPU-driven drawing, world map | **In progress**: 4a tall worlds done, see [devlog 04](devlog/04-scale.md) |

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

## Step 3: Biomes and trees ✔

- Temperature and humidity climate maps; temperature cools with altitude.
- 48 biomes from a decision tree on the climate parameters and surface
  height: 9 oceans, rivers, lakes, coasts, peaks and slopes, and a
  Whittaker temperature × rainfall grid for the lowlands. Includes the
  Minecraft family plus real-world tundra, steppe, chaparral, salt flats,
  fjords, karst, temperate rainforest, cloud forest and volcanic islands.
- Per-biome surface rules (snow, podzol, red sand + terracotta strata,
  mud, salt, basalt, limestone, …) and sea ice.
- Trees on a jittered grid, seamless across chunks: oak, birch, spruce,
  giant spruce, jungle, acacia, dark oak, mangrove, cactus, giant
  mushrooms, bushes.
- Biome map in `wg-map`, biome name in the HUD, grass tinted by biome.

Result: all 48 biomes occur within 24 km of seed 42; 0.90 ms per chunk.

## Step 4: Scale and speed

Scope extended after owner feedback (tall worlds, very far view). Lands
in three PRs: 4a tall worlds, 4b far view and LODs, 4c speed and world map.

- ✔ (4a) **Configurable world height**, chosen in the create-world menu
  like the world size: 256 to 17,696 blocks (2 × Everest's 8,849 m),
  presets 256, 384, 1,000, 2,000, 5,000, 10,000, 17,696. Terrain grows
  with it in all three directions; altitude rules of biomes (lapse rate,
  snow line, peaks) scale with it, sea-level rules don't.
- ✔ (4a) **Cubic-chunk storage:** a column stores only a window of 16³
  sections around the surface; stone below and air above are implied, so
  a 17,696-block chunk costs 3.3 KiB and 3.5 ms. 3D streaming of single
  sections waits for caves, when there is something to see underground.
- **Two view distances:** full detail up to 64 chunks of radius, and an
  **LOD distance** up to 256 chunks of radius (512 × 512 = 262,144 chunks),
  in the style of Voxy and Distant Horizons.
- **LODs:** rings of coarser chunks (2×, 4×, 8×, … block size) out to the
  LOD distance, generated straight from the column sampler (heights, top
  blocks, trees as coarse blobs), cached and rebuilt as the camera moves.
- **Binary greedy meshing** (bitmask face culling) and packed 8-byte vertices.
- **GPU-driven drawing:** one big vertex/index pool, indirect draws.
- Chunk cache and unloading tuned for the larger distances.
- In-game world map (top-down, zoomable to the whole world).
- Performance budget verified against an RTX 2080: 60 FPS at render
  distance 16 plus the LOD rings.
