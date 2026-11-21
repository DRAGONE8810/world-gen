# CLAUDE.md

Guidance for Claude Code (and humans) working in this repository.

## What this is

A Minecraft-like voxel **world generator** in Rust with a Vulkan viewer and a
game-style menu. World size is configurable from 1 km up to the full
Minecraft world (59,999,968 blocks ≈ 60,000 km across), and world height
from 256 to 17,696 blocks.

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
                  rng, noise, spline, terrain, biome, trees, block,
                  palette, chunk, settings, generator.
                  examples/noise_stats calibrates noise;
                  examples/find_biome finds the nearest column of a biome.
crates/wg-map     CLI: renders top-down PNG maps straight from the generator,
                  and benchmarks chunk generation. Use it to check accuracy.
crates/game       Binary `world-gen`: winit window, egui menus, ash/Vulkan
                  renderer, greedy mesher, chunk streaming.
docs/PLAN.md      The 4-step roadmap and its status.
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
cargo run --release -p wg-map -- --seed 42 --bench 32   # ms per chunk
cargo run --release -p worldgen --example noise_stats   # climate noise percentiles
cargo run --release -p worldgen --example find_biome -- 42 jungle   # where to put WG_CAMERA
cargo run --release -p worldgen --example find_biome -- 42 snowy_slopes 17696   # in a tall world
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
(with autostart: place the camera), `WG_SCREEN=create|settings` (open a menu),
`WG_EXIT_AFTER_FRAMES=N`, `WG_HEIGHT=<blocks>` (with autostart: world height). A `settings.ron` in the working directory sets
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
- **Biomes** are a pure function of the climate (+ temperature, humidity)
  and the final surface height (`biome::select`). Surface blocks come from
  `surface_for(.., biome)`. Every biome must stay reachable: a test fails
  if one never appears over 48 km.
- **Trees** use a jittered grid (`trees.rs`). A chunk places every tree
  within `TREE_REACH` of it, using `sample_column` for outside columns.
  Logs may replace air/water/leaves and leaves only air, in global cell
  order, so overlaps don't depend on chunk order. Keep every tree shape
  within `TREE_REACH` (tested).
- **Column sampling and chunks must agree:** `sample_column` is used by maps,
  the menu preview, the HUD and tree placement; tests assert it matches
  `generate_chunk` (surface, top block, biome, sea ice).
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
