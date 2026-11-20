# world-gen

A Minecraft-like voxel world generator written in Rust, with a Vulkan viewer
and a game-style menu. Worlds range from **1 km to 60,000 km** across (the
full Minecraft world), and are generated on the fly from a seed, so a huge
world costs no more than a small one.

Accuracy of the generated world comes first, then performance. Visuals are
deliberately plain.

![In game](docs/images/step1-ingame.png)

## Status

Step 1 of 4 (foundation) is done. See [docs/PLAN.md](docs/PLAN.md) for the
roadmap and [docs/devlog/](docs/devlog/) for a detailed log of each step,
each with a short plain-language summary.

## Run it

1. Install Rust from <https://rustup.rs> (Windows: accept the Visual Studio
   C++ build tools prompt).
2. Make sure your graphics driver is up to date (any Vulkan GPU works; on
   macOS install the [Vulkan SDK](https://vulkan.lunarg.com/) for MoltenVK).
3. In this folder:

```sh
cargo run --release
```

The first build takes a few minutes. No Vulkan SDK is needed on Windows or
Linux; shaders are compiled by the build itself.

### Controls

| Key | Action |
|---|---|
| Mouse | look |
| W A S D | move |
| Space / Shift | up / down |
| Ctrl | 5× speed |
| Esc | pause menu |
| F3 | debug overlay |

## Maps without the game

`wg-map` draws a top-down map straight from the generator. It's the fastest
way to check what a seed looks like:

```sh
cargo run --release -p wg-map -- --seed 42 --extent 16000 --px 1024 --out map.png
cargo run --release -p wg-map -- --seed 42 --bench 32     # generation speed
```

## Layout

| Crate | Purpose |
|---|---|
| `crates/worldgen` | the generator: noise, chunks, settings, terrain. No graphics. |
| `crates/wg-map` | map and benchmark tool |
| `crates/game` | window, menus, Vulkan renderer, mesher, chunk streaming |

## Tests

```sh
cargo test
```
