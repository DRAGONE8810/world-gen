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
| 4 | Scale and speed: tall worlds (cubic chunks), LODs to 256 chunks, faster meshing, GPU-driven drawing, world map | **Done** (4a, 4b, 4c), see [devlog 04](devlog/04-scale.md); RTX 2080 frame-rate check pending on real hardware |
| 5 | Realistic generator: whole-planet simulation (plate tectonics, climate, rivers, erosion, lakes), planets Continents / Pangea / Earth / Islands, simulation quality, graphics presets | **Done**, see [devlog 05](devlog/05-realistic.md) |
| 6 | Whole-world far view: LOD to the world border and orbit, detail by height above ground, GPU mesh cache, faster tiles and bookkeeping | **Done**, see [devlog 06](devlog/06-far-view.md); RTX 2080 frame-rate check pending |
| 7 | Coasts, continents and colours: no sea walls, LOD to 256k chunks, planned continents, beach types and sub-biomes, climate-blended grass, satellite map with clouds, random seed | **Done**, see [devlog 07](devlog/07-coasts-and-colours.md) |

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
- ✔ (4b) **Two view distances:** full detail up to 64 chunks of radius,
  and an **LOD distance** up to 256 chunks of radius (512 × 512 = 262,144
  chunks), in the style of Voxy and Distant Horizons. Tall worlds multiply
  the LOD distance by their terrain scale.
- ✔ (4b) **LODs:** a quadtree of tiles in rings of 2, 4, 8, … block cells
  out to the LOD distance, sampled straight from the generator (ground,
  water, ice, trees exact at small cells and by density at large ones),
  swapped without holes as the camera moves. Reverse-Z depth.
- ✔ (4c) **Binary greedy meshing** (bitmask face culling, 12× faster:
  0.11 ms per chunk) and packed 8-byte vertices with a shared quad index
  buffer (4.7 KiB of GPU memory per chunk, was 13).
- ✔ (4c) **GPU-driven drawing:** meshes suballocated from 128 MiB vertex
  pools, one indirect draw per pool (1 draw call for the whole default
  view), direct-draw fallback.
- ✔ (4b) Unloading for the larger distances: chunks kept 2 rings past
  the view, meshes evicted once neither a leaf nor drawn. A cache for LOD
  tiles that come back into view is left for later.
- ✔ (4c) **In-game world map** (M): top-down, from 1/8 block per pixel
  to the whole world, drag, zoom, double-click to teleport.
- Performance budget on an RTX 2080: 60 FPS at render distance 16 plus
  the LOD rings. Not measurable in the cloud container (software
  renderer); to be confirmed on the owner's machine.

## Step 5: Realistic generator ✔

Requested by the owner: keep the Minecraft-style generator (now
**Classic**) and add a second one based on the real Earth.

- ✔ Generator choice in the create-world menu; Realistic planets:
  **Continents** (random plate layout), **Pangea** (one supercontinent),
  **Earth** (NOAA ETOPO 2022 relief, real lakes), **Islands**.
- ✔ Plate tectonics (kinematic, after Cortial et al. 2019): collision
  ranges and plateaus, Andean margins with trenches, island arcs, rifts,
  mid-ocean ridges, ocean depth by crustal age (GDH1), old orogens,
  hotspot chains.
- ✔ Climate from latitude, ocean currents, wind belts, rain shadows and
  monsoons; aridity index drives the biomes.
- ✔ Rivers and fluvial erosion (Priority-Flood + implicit stream power),
  sediment-filled basins, water-balance lakes; rivers flow downhill with
  their own water level.
- ✔ Per-block detail: ridged, strike-aligned noise in ranges, smooth
  plains, river channels, lake shores; 1 block = 1 m in a 40,000 km ×
  17,696-block world.
- ✔ Simulation quality Optimized / Balanced / Quality (0.3 s to 8 s) and
  graphics presets Optimized / Balanced / Quality in Settings.
- ✔ Loading screen, latitude in the HUD, whole-planet preview and maps.

Result: 0.6 ms per chunk (6× faster than Classic in tall worlds), 107
tests. Known limits: hyper-arid deserts too rare, Earth's rain is
modelled, not measured.

## Step 6: The whole world at once ✔

- ✔ LOD levels up to 22 (4,194,304-block cells): a **Whole world** option
  (on in the Quality preset) draws everything out to the world border.
- ✔ Detail by `max(horizontal distance, height above ground)`: flying up
  removes chunks and fine tiles; the camera can climb to "orbit" (as high
  as the world is wide). The whole Earth from orbit is 200 tiles.
- ✔ 256 MiB GPU cache of out-of-view meshes, reused instantly.
- ✔ Quadtree bookkeeping 4–6× faster (fast hashing, allocation-free draw
  set, packed sort keys); more jobs in flight; uploads capped by bytes.
- ✔ Far tiles 1.5–2.5× faster in Classic worlds (bit-identical sampling
  shortcuts).

Result: the whole 60,000 km world from the ground is ~13,600 tiles and
~200 MB of vertices, built in 6–13 s on 3 threads, nearest first.
Pending: frame rate on an RTX 2080; a screen-space error metric.

## Step 7: Coasts, continents and colours ✔

- ✔ Dry depressions get a rim, so the sea never stops in a straight "wall"
  (tested by marching from every dry column below sea level).
- ✔ LOD distance up to 262,144 chunks (logarithmic slider); Random seed
  button.
- ✔ Continents planet planned from ellipse lobes, peninsulas, gulfs and
  islands in four Earth-like characters, kept apart by Voronoi straits:
  4–8 continents per seed (was mostly 2), largest ~⅓ of the land.
- ✔ Five beach types (sand, shingle, tropical white sand, tidal flats,
  coastal dunes) and sub-biomes (rocky desert, bog, moor, flooded
  jungle): 56 biomes.
- ✔ Grass and oak-leaf colour follows the climate (`Tint`), so biome
  borders fade instead of jumping; +0.3 % quads.
- ✔ World map like a satellite photo: canopy colours, soft borders, hill
  shading, shelf colours, optional clouds with opacity.

Next: clouds in the world; RTX 2080 frame-rate check.
