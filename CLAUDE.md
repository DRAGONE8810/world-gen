# CLAUDE.md

Guidance for Claude Code (and humans) working in this repository.

## What this is

A Minecraft-like voxel **world generator** in Rust with a Vulkan viewer and a
game-style menu. World size is configurable from 1 km up to the full
Minecraft world (59,999,968 blocks ≈ 60,000 km across), and world height
from 256 to 17,696 blocks. Two generators: **Classic** (Minecraft-style
noise terrain) and **Realistic** (a simulated Earth-like planet: plate
tectonics, climate, rivers; planets Continents, Pangea, Earth, Islands).

**Priorities, in order:**
1. **Accuracy of the generated world.** Deterministic, seed-stable, no
   precision glitches anywhere up to the ±30,000,000 border, believable
   terrain and biome layout. Every generator change needs tests.
2. **Performance.** Must run well on an RTX 2080 (target 60 FPS at default
   render distance); developed on an RTX 3060.
3. **Visuals are secondary.** Flat colours, no textures. Do not spend effort
   on eye candy.

Platforms: Windows and Linux required, macOS (MoltenVK) nice to have.

## Layout

```
crates/worldgen   Pure generator library. NO graphics, NO windowing, NO I/O.
                  rng, noise, spline, terrain, biome, trees, region (rare
                  regional landscapes), block, palette, chunk, settings,
                  generator, lod (far-view sampling),
                  realistic/ (second generator: tectonics + island rounds,
                  climate, erosion, earth relief + lakes, grid, theme =
                  Mushoku continent themes). data/earth_relief.bin is NOAA
                  ETOPO 2022 at 0.1°, compiled in (make_earth_relief.py).
                  examples/noise_stats calibrates noise;
                  examples/find_biome finds the nearest column of a biome;
                  examples/planet_stats prints realistic climate statistics;
                  examples/continent_stats compares continent shapes with Earth;
                  examples/biome_stats prints biome shares and how much
                  biome borders interleave.
                  tint: grass/foliage colour from the climate.
crates/wg-map     CLI: renders top-down PNG maps straight from the generator,
                  and benchmarks chunk generation. Use it to check accuracy.
crates/game       Binary `world-gen`: winit window, egui menus, ash/Vulkan
                  renderer (render/: GPU-driven, pool.rs range allocator),
                  binary greedy mesher (mesh.rs), LOD quadtree + tile mesher
                  (lod.rs), chunk/tile streaming (world.rs), world map (map.rs:
                  256-px tiles per zoom level, built on a worker, LRU cache).
docs/PLAN.md      The roadmap (steps 1–8) and its status.
docs/devlog/      One log per step: detailed technical write-up + a short
                  plain-language summary for non-programmers.
```

Dependency direction: `game` → `worldgen` ← `wg-map`. Never make `worldgen`
depend on graphics crates.

## Commands

```sh
cargo test                       # all tests (fast; run before every commit)
cargo clippy --all-targets       # must be warning-free
cargo fmt                        # rustfmt.toml: max_width 120
cargo run --release              # the game (default-run = world-gen)
cargo run --release -p wg-map -- --seed 42 --extent 16000 --px 1024 --out map.png
cargo run --release -p wg-map -- --seed 42 --layer biome    # also: height|continentalness|erosion|pv|roughness|temperature|humidity
cargo run --release -p wg-map -- --seed 42 --bench 32   # ms per chunk and per LOD tile
cargo run --release -p worldgen --example noise_stats   # climate noise percentiles
cargo run --release -p worldgen --example find_biome -- 42 jungle   # where to put WG_CAMERA
cargo run --release -p worldgen --example find_biome -- 42 snowy_slopes 17696   # in a tall world
cargo run --release -p worldgen --example find_biome -- 42 cherry_grove 17696 continents   # on a planet
cargo run --release -p worldgen --example biome_stats -- 42 [continents]   # biome shares, border interleaving
cargo run --release -p wg-map -- --seed 42 --realistic earth --size-km 40000 --height 17696 --whole
cargo run --release -p worldgen --example planet_stats -- 42 continents   # aridity classes vs Earth
cargo run --release -p worldgen --example continent_stats -- continents 1 12   # continents, coasts vs Earth (WG_DUMP=dir writes PGMs)
```

Shaders live in `crates/game/shaders/*.vert|frag` (Vulkan GLSL) and are
compiled to SPIR-V by `crates/game/build.rs` with naga. No Vulkan SDK is
needed to build. naga's `ADJUST_COORDINATE_SPACE` flag is disabled on
purpose: the projection matrix already flips Y for Vulkan.

### Running without a GPU (CI / cloud containers)

Mesa's software Vulkan driver (lavapipe) plus a virtual X display works:

```sh
apt-get install -y mesa-vulkan-drivers vulkan-validationlayers xvfb libxkbcommon-x11-0 imagemagick
Xvfb :99 -screen 0 1280x800x24 &
DISPLAY=:99 WG_VALIDATION=1 WG_AUTOSTART=42 cargo run --release &   # jump into seed 42
sleep 25; DISPLAY=:99 import -window root shot.png
```

Environment switches: `WG_VALIDATION=1` (Khronos validation layer, must
print no errors), `WG_AUTOSTART=<seed>` (skip menus), `WG_CAMERA=x,z[,yaw,pitch,height]`
(with autostart: place the camera), `WG_SCREEN=create|settings` (open a menu; `map` with autostart),
`WG_EXIT_AFTER_FRAMES=N`, `WG_HEIGHT=<blocks>` (with autostart: world height),
`WG_PLANET=continents|pangea|earth|islands|mushoku` (with autostart: a Realistic world),
`WG_QUALITY=fast|balanced|quality`, `WG_SIZE_KM=<km>`, `WG_MAP_ZOOM=<blocks per point>` (map zoom). A `settings.ron` in the working directory sets
render distance etc.

## Conventions

- **Determinism:** all randomness comes from `worldgen::rng` (SplitMix64,
  Xoroshiro128++, `hash_position`). Never use `rand`, `HashMap` iteration
  order, thread timing or `f32` in anything that decides block placement.
  New features get their own sub-seed via `derive_seed(seed, "feature/name")`
  so adding a feature never changes existing terrain.
- **Generation is pure:** `WorldGenerator` is `Send + Sync` with no mutable
  state; any chunk can be generated on any thread in any order.
- **Terrain model:** climate noise → `terrain::TerrainShaper` (spline
  curves) → base height `H` + roughness `A` → 3D density
  `(H − y) + A·N > 0` on a 4×8×4 interpolation grid. Tune the world by
  editing curve tables, then check with `wg-map` layers and the statistics
  tests in `generator.rs`.
- **Biomes** are a pure function of the climate (+ temperature, humidity),
  the final surface height and the column's region (`biome::select`).
  Lowland biomes are the nearest `LOWLAND` target point in temperature ×
  humidity. Surface blocks come from `surface_for(.., biome)`. Every biome
  must stay reachable: a test fails if one never appears over 48 km.
- **Transitions:** chunks, `sample_column` and LODs choose the biome with
  `select_with(.., &Jitter)` from `BiomeDither::ground` (and trees with
  `BiomeDither::tree` via `resolve_tree`), never from the raw climate, so
  borders are interleaved zones. Keep any new biome rule inside
  `select_with` so it gets dithered too.
- **Regions** (`region.rs`): a warped Voronoi of rare flavours (cherry,
  pillars, baobab, …) in `Shape.region`; `biome::regional` maps host biomes.
  Pillars also raise the terrain (`Shape.pillar`, subtracted again for
  climate-by-altitude).
- **Grass colour** is a `Tint` (one byte per column: a climate colour map,
  or a few special biomes), not the biome, so borders fade. Chunks,
  `ColumnSample` and `LodColumn` all carry it from `Tint::of`; the mesher
  merges tinted faces by tint. Maps use `ColumnSample::orbit_color`
  (ground + average canopy).
- **Trees** use a jittered grid (`trees.rs`). A chunk places every tree
  within `TREE_REACH` of it, using `sample_column` for outside columns.
  Logs may replace air/water/leaves and leaves only air, in global cell
  order, so overlaps don't depend on chunk order. Keep every tree shape
  within `TREE_REACH` (tested).
- **Column sampling and chunks must agree:** `sample_column` is used by maps,
  the menu preview, the HUD and tree placement; tests assert it matches
  `generate_chunk` (surface, top block, biome, sea ice).
- **LODs:** `WorldGenerator::lod_grid` is the only source of far terrain;
  at 1-block cells it must equal the generated chunk's top blocks (tested),
  so any generator change that breaks that test must be mirrored there.
  Canopy in LOD cells is shown with the probability that trees cover the
  cell (`cover_roll`), so forests look the same at every cell size.
  `LodColumn::side` (from `ColumnSample::side_block`) colours cliff walls.
  The game's quadtree (`lod.rs`) must keep its leaves gap- and overlap-free
  and `draw_set` must never draw a mesh that isn't uploaded (tested).
  Levels go to `MAX_LEVEL` = 22 so the whole 60,000 km world fits
  (`GameSettings.whole_world`); detail is chosen by `max(horizontal
  distance, camera height above ground)` (`Layout.altitude`), so flying
  up removes work. Meshes leaving the view stay on the GPU in an LRU
  cache (`world::CACHE_BYTES`) and are reused. Use `lod::FastSet` /
  `FastMap` for sets keyed by `NodeKey`/`ChunkPos` (per-frame code).
  Depth is reverse-Z with an infinite far plane.
- **Two generators:** `Generator::Classic` (`terrain::TerrainShaper`) and
  `Generator::Realistic` (`realistic::RealisticWorld`), behind the
  `Shaper` enum in `generator.rs`. Both produce a `terrain::Shape`, so
  biomes, chunks, trees, LODs and maps are shared. Never change Classic
  output when working on Realistic. A Realistic world simulates its
  planet once in `WorldGenerator::new` (0.3–8 s by `SimQuality`); the game
  does that on a background thread with a loading screen. Planet and
  quality are part of the world's identity, like the seed. The Continents
  planet's crust is *planned* (`tectonics::plan_continents`: lobes,
  peninsulas, gulfs, islands, Voronoi straits); check shape changes with
  `continent_stats` and the `continents_are_many_separate_and_intricate`
  test. Islands come in rounds: `tectonics::island_rounds` (large and
  medium, on the grid, before erosion) and `RealisticWorld::build_islets`
  (open-ocean volcanoes, hills, atolls below the grid's resolution,
  evaluated per column). Hills and dunes away from the ranges are
  `build_hills` grids. Mushoku (`realistic::theme`) pulls the simulated
  climate towards each continent's theme and hands its range to the
  biome rules (`Shape::theme_t`/`theme_h`, clamped in `select_with`).
- **Rivers** (realistic) are a drainage tree (`route_water`): every
  reach has one successor, levels never rise, rivers never cross (keep
  `rivers_form_networks_that_reach_the_sea_or_a_lake` passing) and end at
  the sea or in a lake.
- **Transition width:** `Shape::blend_t`/`blend_h` scale the biome dither
  (realistic: by the climate gradient), `Shape::purity` removes ground
  patches; LOD cells ≥ `COARSE_CELL` use the coarse dither (no patches).
- **Water level per column:** `Shape.water` is the water surface (a river
  or lake can sit above `SEA_LEVEL`). Fill water up to it, put sea ice at
  it, use `ColumnSample::water_top()` to stand on it; `underwater` means
  `surface_y < shape.water`.
- **Parallel simulation:** rayon is allowed in the planet simulation only
  as `par_iter().map().collect()` (keeps cell order, so results don't
  depend on thread count); transcendentals there go through `libm`.
- **Rendering:** vertices are 8 bytes (`mesh::Vertex`: x, z in cells, y +
  `Y_BIAS`, sRGB colour); quads share one index buffer, so meshes carry no
  indices. Meshes live in shared vertex pools (`render/pool.rs`); a range
  is freed only after the frames in flight that used it have finished.
  Each visible mesh is one draw record + one indirect command, drawn with
  one `vkCmdDrawIndexedIndirect` per pool (direct draws as a fallback).
  The binary mesher must match `greedy_section_reference` (tested).
- **Coordinates:** blocks are `i32`, noise inputs `f64` and wrapped
  (`noise::wrap`); camera position is `f64`; the GPU only sees
  camera-relative `f32`.
- **World height** is a setting (`WorldHeight`, 256..=17,696, whole
  sections). `terrain_scale = height / 384` stretches the terrain both
  vertically and horizontally; use `Shape::normalized_y` for rules about
  mountains (snow line, peaks, lapse rate) and absolute blocks for rules
  about the sea (beaches, shallow water). `SEA_LEVEL = 63` always. The
  `MIN_Y`/`MAX_Y` constants describe only the default 384 world: use
  `generator.min_y()/max_y()` or `chunk.min_y()/max_y()` instead.
- **Chunk storage:** floor section 0 plus a contiguous window of
  palette-compressed 16³ sections; stone below the window and air above it
  are implied. Read with `chunk.section(i)` (a `SectionRef`), never assume
  every section is stored.
- Code comments explain *why*; each module starts with a doc comment that
  explains the technique for a reader new to voxel engines.
- Original work only: we take ideas from published algorithms and open
  source projects, but don't copy Minecraft code, assets or textures.

## Workflow

- One branch + PR per roadmap step (`step-N-name`), merged by the owner.
- Every step adds `docs/devlog/NN-name.md` with: a plain-language summary
  (short, no jargon) first, then the detailed technical log, test results,
  measurements and screenshots/maps in `docs/images/`.
- Update the status table in `docs/PLAN.md` when a step lands.
