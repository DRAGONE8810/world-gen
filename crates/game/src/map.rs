//! In-game world map (press M).
//!
//! A top-down view drawn straight from the generator, like `wg-map` and the
//! create-world preview: one column sample per pixel, so it shows any part
//! of the world at any zoom, from single blocks to the whole 60,000 km
//! world, without the world being loaded or stored anywhere.
//!
//! ## Tiles
//!
//! The map is cut into square **tiles** of 256 × 256 pixels on a pyramid of
//! zoom levels, like a web map: at level `L` one pixel is `2^L` blocks. The
//! view always draws the level whose pixels are closest to the screen's own
//! pixels, so the picture is as sharp as the screen at every zoom (a single
//! stretched image, as before, was blurry on large screens however far you
//! zoomed in). Finished tiles are kept (up to `CACHE_TILES`), so panning
//! only samples the strip that comes into view, and zooming back is
//! instant.
//!
//! One background thread samples tiles in order of priority, each spread
//! over all cores: first one coarse tile three levels up wherever nothing
//! is on screen yet (a quick preview), then the tiles of the current level
//! from the centre of the view outwards. A tile that isn't ready yet is
//! drawn from the nearest coarser tile that is.
//!
//! The picture imitates a satellite photo rather than a block map:
//!
//! * each land pixel shows the ground *with the biome's average tree
//!   canopy over it* (`ColumnSample::orbit_color`), so forests are dark
//!   green, and grass follows the climate smoothly (`worldgen::tint`);
//!   biome borders are transition zones in the generator itself, so the
//!   map needs no blur of its own;
//! * relief is hill-shaded from the sampled heights (light from the
//!   north-west), exaggerated in proportion to the zoom so mountain ranges
//!   still show on a whole planet;
//! * water goes from turquoise shallows to dark deep sea;
//! * an optional cloud layer: fractal noise, denser where the climate is
//!   humid, drawn as a separate texture so its opacity can change without
//!   resampling. It fades out as you zoom in, so close views stay sharp.

use egui::{Color32, Pos2, Rect, Sense, TextureFilter, TextureOptions, Vec2};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use worldgen::noise::{OctaveConfig, OctaveNoise};
use worldgen::rng::derive_seed;
use worldgen::spline::{lerp, smoothstep};
use worldgen::{ColumnSample, WorldGenerator, SEA_LEVEL};

use crate::config::MapClouds;

/// Pixels per side of a tile.
const TILE: usize = 256;
/// Finished tiles kept for reuse (two 256 KB textures each).
const CACHE_TILES: usize = 160;
/// Levels above the current one used for the quick preview.
const PREVIEW_LEVELS: u32 = 3;

/// What the map wants the game to do.
pub enum MapAction {
    None,
    /// Move the camera to this block column.
    Teleport(f64, f64),
}

/// A tile of the pyramid: `2^level` blocks per pixel, covering blocks
/// `[tx·span, (tx + 1)·span)` × `[tz·span, (tz + 1)·span)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TileKey {
    level: u32,
    tx: i64,
    tz: i64,
}

impl TileKey {
    fn bpp(self) -> f64 {
        (1u64 << self.level) as f64
    }
    /// Blocks across the tile.
    fn span(self) -> f64 {
        self.bpp() * TILE as f64
    }
    fn origin(self) -> (f64, f64) {
        (self.tx as f64 * self.span(), self.tz as f64 * self.span())
    }
    /// The tile `up` levels coarser that contains this one.
    fn ancestor(self, up: u32) -> TileKey {
        TileKey { level: self.level + up, tx: self.tx.div_euclid(1 << up), tz: self.tz.div_euclid(1 << up) }
    }
}

struct Tile {
    map: egui::TextureHandle,
    clouds: egui::TextureHandle,
    last_used: u64,
}

/// State shared with the sampling thread.
struct Shared {
    /// Tiles to sample, most urgent first; replaced every frame.
    wanted: Mutex<Vec<TileKey>>,
    /// The tile being sampled right now.
    busy: Mutex<Option<TileKey>>,
    wake: Condvar,
    quit: AtomicBool,
}

type Finished = (TileKey, egui::ColorImage, egui::ColorImage);

pub struct WorldMap {
    /// Centre of the view in block coordinates.
    center: (f64, f64),
    /// Zoom: blocks per screen point.
    blocks_per_point: f64,
    tiles: HashMap<TileKey, Tile>,
    frame: u64,
    shared: Arc<Shared>,
    rx: Receiver<Finished>,
    /// The generator and sender, until the worker starts on first use.
    pending_worker: Option<Sender<Finished>>,
}

impl Drop for WorldMap {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

impl WorldMap {
    /// Opens the map centred on the player, zoomed so the view spans ~4 km
    /// of a normal world (scaled for tall worlds).
    /// `WG_MAP_ZOOM=<blocks per point>` overrides the zoom (screenshots).
    pub fn new(player: (f64, f64), terrain_scale: f64) -> Self {
        let zoom = std::env::var("WG_MAP_ZOOM").ok().and_then(|v| v.parse().ok()).unwrap_or(8.0 * terrain_scale);
        let (tx, rx) = channel();
        Self {
            center: player,
            blocks_per_point: zoom,
            tiles: HashMap::new(),
            frame: 0,
            shared: Arc::new(Shared {
                wanted: Mutex::new(Vec::new()),
                busy: Mutex::new(None),
                wake: Condvar::new(),
                quit: AtomicBool::new(false),
            }),
            rx,
            pending_worker: Some(tx),
        }
    }

    /// Draws the map full-screen. `player` is the camera position (x, z)
    /// and `yaw` its heading.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        gen: &Arc<WorldGenerator>,
        player: (f64, f64),
        yaw: f32,
        clouds: &mut MapClouds,
    ) -> MapAction {
        let mut action = MapAction::None;
        if let Some(tx) = self.pending_worker.take() {
            spawn_worker(self.shared.clone(), gen.clone(), tx);
        }
        self.frame += 1;
        let half_world = gen.settings().size.max_extent() as f64 / 2.0;
        let ppp = ui.ctx().pixels_per_point() as f64;
        // Coarsest level: one tile holds the whole world.
        let max_level = ((2.0 * half_world / TILE as f64).log2().ceil().max(0.0)) as u32;

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_rgb(14, 16, 22))).show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let (rect, response) = ui.allocate_exact_size(full.size(), Sense::click_and_drag());
            // Zoomed all the way out, the world fills most of the panel.
            let max_bpp = (2.0 * half_world / (0.9 * rect.width().min(rect.height()) as f64)).max(1.0);

            // ---- Input: drag to pan, wheel to zoom about the cursor ----
            if response.dragged() {
                let d = response.drag_delta();
                self.center.0 -= d.x as f64 * self.blocks_per_point;
                self.center.1 -= d.y as f64 * self.blocks_per_point;
            }
            let to_world = |p: Pos2, c: (f64, f64), bpp: f64| {
                let v = p - rect.center();
                (c.0 + v.x as f64 * bpp, c.1 + v.y as f64 * bpp)
            };
            if let Some(hover) = response.hover_pos() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let before = to_world(hover, self.center, self.blocks_per_point);
                    let factor = (-(scroll as f64) / 120.0).exp2();
                    self.blocks_per_point = (self.blocks_per_point * factor).clamp(0.125, max_bpp);
                    let after = to_world(hover, self.center, self.blocks_per_point);
                    self.center.0 += before.0 - after.0;
                    self.center.1 += before.1 - after.1;
                }
            }
            self.blocks_per_point = self.blocks_per_point.min(max_bpp);
            self.center.0 = self.center.0.clamp(-half_world, half_world);
            self.center.1 = self.center.1.clamp(-half_world, half_world);

            // ---- Tiles ----
            self.receive(ui.ctx());
            // The level whose pixels are closest to the screen's pixels.
            let level = ((self.blocks_per_point / ppp).log2().round().max(0.0) as u32).min(max_level);
            let (vx0, vz0) = to_world(rect.min, self.center, self.blocks_per_point);
            let (vx1, vz1) = to_world(rect.max, self.center, self.blocks_per_point);
            let (vx0, vz0) = (vx0.max(-half_world - 1.0), vz0.max(-half_world - 1.0));
            let (vx1, vz1) = (vx1.min(half_world + 1.0), vz1.min(half_world + 1.0));
            let span = TileKey { level, tx: 0, tz: 0 }.span();
            let mut visible = Vec::new();
            if vx0 < vx1 && vz0 < vz1 {
                for tz in (vz0 / span).floor() as i64..=(vz1 / span).floor() as i64 {
                    for tx in (vx0 / span).floor() as i64..=(vx1 / span).floor() as i64 {
                        visible.push(TileKey { level, tx, tz });
                    }
                }
            }
            // Requests: a coarse preview wherever nothing covers the view
            // yet, then this level from the centre outwards.
            let busy = *self.shared.busy.lock().expect("map lock");
            let covered = |k: TileKey, tiles: &HashMap<TileKey, Tile>| {
                (0..=max_level - level).any(|up| tiles.contains_key(&k.ancestor(up)))
            };
            let mut wanted: Vec<TileKey> = Vec::new();
            for &k in &visible {
                if !covered(k, &self.tiles) && level < max_level {
                    let p = k.ancestor(PREVIEW_LEVELS.min(max_level - level));
                    if !wanted.contains(&p) && Some(p) != busy {
                        wanted.push(p);
                    }
                }
            }
            let mut fine: Vec<TileKey> =
                visible.iter().copied().filter(|k| !self.tiles.contains_key(k) && Some(*k) != busy).collect();
            let dist = |k: &TileKey| {
                let (ox, oz) = k.origin();
                let (dx, dz) = (ox + span / 2.0 - self.center.0, oz + span / 2.0 - self.center.1);
                dx * dx + dz * dz
            };
            fine.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
            wanted.extend(fine);
            let drawing = !wanted.is_empty() || busy.is_some();
            *self.shared.wanted.lock().expect("map lock") = wanted;
            self.shared.wake.notify_all();
            if drawing {
                ui.ctx().request_repaint_after(Duration::from_millis(30));
            }

            // ---- Drawing ----
            let painter = ui.painter_at(rect);
            let to_screen = |x: f64, z: f64| {
                rect.center()
                    + Vec2::new(
                        ((x - self.center.0) / self.blocks_per_point) as f32,
                        ((z - self.center.1) / self.blocks_per_point) as f32,
                    )
            };
            // Clouds are weather systems hundreds of km wide: zoomed in
            // they would only veil the whole view in haze, so they fade out
            // below ~1,000 blocks per point (gone at 128), as if the camera
            // had dropped under them.
            let zoom_fade = smoothstep(128f64.ln(), 1024f64.ln(), self.blocks_per_point.ln()) as f32;
            let cloud_alpha =
                if clouds.show { (clouds.opacity.clamp(0.0, 1.0) * zoom_fade * 255.0).round() as u8 } else { 0 };
            for &k in &visible {
                // This tile, or the part of the nearest coarser one over it.
                let Some((src, up)) =
                    (0..=max_level - level).map(|up| (k.ancestor(up), up)).find(|(a, _)| self.tiles.contains_key(a))
                else {
                    continue;
                };
                let tile = self.tiles.get_mut(&src).expect("found above");
                tile.last_used = self.frame;
                let n = (1u64 << up) as f32;
                let (fx, fz) = (k.tx.rem_euclid(1 << up) as f32 / n, k.tz.rem_euclid(1 << up) as f32 / n);
                let uv = Rect::from_min_size(Pos2::new(fx, fz), Vec2::splat(1.0 / n));
                let (ox, oz) = k.origin();
                let at = Rect::from_two_pos(to_screen(ox, oz), to_screen(ox + span, oz + span));
                painter.image(tile.map.id(), at, uv, Color32::WHITE);
                if cloud_alpha > 0 {
                    painter.image(tile.clouds.id(), at, uv, Color32::from_white_alpha(cloud_alpha));
                }
            }
            // Player marker: a triangle pointing along the view direction.
            let p = to_screen(player.0, player.1);
            let (s, c) = yaw.sin_cos();
            let fwd = Vec2::new(s, -c);
            let right = Vec2::new(c, s);
            let pts = vec![p + fwd * 11.0, p - fwd * 6.0 + right * 6.0, p - fwd * 6.0 - right * 6.0];
            painter.add(egui::Shape::convex_polygon(pts, Color32::from_rgb(255, 70, 50), (1.5, Color32::WHITE)));

            // ---- Text ----
            let mut lines = vec![format!(
                "{} per pixel   ·   view {}   ·   drag to move · wheel to zoom · double-click to teleport · M or Esc to close",
                format_blocks(self.blocks_per_point / ppp),
                format_blocks(rect.width().max(rect.height()) as f64 * self.blocks_per_point)
            )];
            if let Some(h) = response.hover_pos() {
                let (x, z) = to_world(h, self.center, self.blocks_per_point);
                let s = gen.sample_column(x.floor() as i32, z.floor() as i32);
                lines.push(if s.inside {
                    format!(
                        "x {:.0}  z {:.0}   {}   ground y {}{}",
                        x,
                        z,
                        s.biome.display_name(),
                        s.surface_y,
                        gen.realistic().map(|r| format!("   {}", r.describe_climate(&s.shape, s.surface_y))).unwrap_or_default()
                    )
                } else {
                    format!("x {x:.0}  z {z:.0}   outside the world border")
                });
                if response.double_clicked() && s.inside {
                    action = MapAction::Teleport(x, z);
                }
            }
            if drawing {
                lines.push("drawing…".into());
            }
            let text = lines.join("\n");
            let galley = painter.layout_no_wrap(text, egui::FontId::monospace(13.0), Color32::WHITE);
            let at = rect.left_bottom() + Vec2::new(12.0, -12.0 - galley.size().y);
            painter.rect_filled(
                Rect::from_min_size(at - Vec2::splat(6.0), galley.size() + Vec2::splat(12.0)),
                4.0,
                Color32::from_black_alpha(170),
            );
            painter.galley(at, galley, Color32::WHITE);
            painter.text(
                rect.center_top() + Vec2::new(0.0, 14.0),
                egui::Align2::CENTER_TOP,
                "World map",
                egui::FontId::proportional(24.0),
                Color32::WHITE,
            );

            // ---- Cloud controls (top right) ----
            egui::Area::new(egui::Id::new("map_clouds"))
                .anchor(egui::Align2::RIGHT_TOP, Vec2::new(-14.0, 14.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.checkbox(&mut clouds.show, "Clouds");
                        ui.add_enabled_ui(clouds.show, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Opacity");
                                let mut pct = clouds.opacity * 100.0;
                                if ui.add(egui::Slider::new(&mut pct, 0.0..=100.0).suffix(" %").integer()).changed() {
                                    clouds.opacity = pct / 100.0;
                                }
                            });
                        });
                    });
                });
        });
        action
    }

    /// Uploads finished tiles and evicts the least recently drawn ones.
    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok((key, map, clouds)) = self.rx.try_recv() {
            // Single blocks stay crisp when zoomed in; shrunk tiles are
            // filtered.
            let opts = TextureOptions {
                magnification: TextureFilter::Nearest,
                minification: TextureFilter::Linear,
                ..TextureOptions::LINEAR
            };
            let name = format!("map_{}_{}_{}", key.level, key.tx, key.tz);
            let map = ctx.load_texture(name.clone(), map, opts);
            let clouds = ctx.load_texture(name + "_clouds", clouds, TextureOptions::LINEAR);
            self.tiles.insert(key, Tile { map, clouds, last_used: self.frame });
        }
        while self.tiles.len() > CACHE_TILES {
            let oldest = *self.tiles.iter().min_by_key(|(_, t)| t.last_used).expect("not empty").0;
            self.tiles.remove(&oldest);
        }
    }
}

/// Starts the thread that samples requested tiles, most urgent first,
/// until the map closes.
fn spawn_worker(shared: Arc<Shared>, gen: Arc<WorldGenerator>, tx: Sender<Finished>) {
    std::thread::spawn(move || loop {
        let key = {
            let mut wanted = shared.wanted.lock().expect("map lock");
            loop {
                if shared.quit.load(Ordering::Relaxed) {
                    return;
                }
                if !wanted.is_empty() {
                    let k = wanted.remove(0);
                    *shared.busy.lock().expect("map lock") = Some(k);
                    break k;
                }
                wanted = shared.wake.wait_timeout(wanted, Duration::from_millis(200)).expect("map lock").0;
            }
        };
        let samples = sample_tile(&gen, key);
        let vscale = gen.settings().height.terrain_scale();
        let map = paint(&samples, key.bpp(), vscale);
        let clouds = paint_clouds(&samples, key, gen.settings().seed);
        let sent = tx.send((key, map, clouds));
        *shared.busy.lock().expect("map lock") = None;
        if sent.is_err() {
            return;
        }
    });
}

/// Side of the sampled grid: the tile plus a one-pixel border all round,
/// so hill shading needs no neighbouring tile.
const SAMPLED: usize = TILE + 2;

/// Samples a tile and its border (`None` beyond the representable
/// coordinates).
fn sample_tile(gen: &WorldGenerator, key: TileKey) -> Vec<Option<ColumnSample>> {
    use rayon::prelude::*;
    let (x0, z0) = key.origin();
    let bpp = key.bpp();
    (0..SAMPLED * SAMPLED)
        .into_par_iter()
        .map(|i| {
            let x = (x0 + ((i % SAMPLED) as f64 - 0.5) * bpp).floor();
            let z = (z0 + ((i / SAMPLED) as f64 - 0.5) * bpp).floor();
            (x.abs() <= i32::MAX as f64 && z.abs() <= i32::MAX as f64).then(|| gen.sample_column(x as i32, z as i32))
        })
        .collect()
}

/// Colour outside the world.
const OUTSIDE: [u8; 3] = [10, 10, 14];

/// Paints a tile from its samples (with border): satellite colours,
/// hill-shaded on land.
fn paint(samples: &[Option<ColumnSample>], bpp: f64, vscale: f64) -> egui::ColorImage {
    let at = |x: usize, z: usize| samples[x + z * SAMPLED].as_ref();
    // Hill shading needs heights in default-world blocks per pixel; on
    // coarse views real slopes vanish, so they're exaggerated with the zoom.
    let height = |x: usize, z: usize| at(x, z).map_or(0.0, |s| s.water_top() as f64);
    let exaggerate = 0.35 * (bpp / 16.0).max(1.0).sqrt() * 2.5 / vscale;
    let pixels = (0..TILE * TILE)
        .map(|i| {
            // Position in the sampled grid (one-pixel border).
            let (x, z) = (i % TILE + 1, i / TILE + 1);
            let Some(s) = at(x, z).filter(|s| s.inside) else {
                let [r, g, b] = OUTSIDE;
                return Color32::from_rgb(r, g, b);
            };
            let base = map_color(s, vscale);
            let land = !(s.underwater || s.ice);
            // Light from the north-west (top left): ground rising towards
            // the south-east faces it.
            let slope = (height(x + 1, z + 1) - height(x - 1, z - 1)) / (2.83 * bpp.max(1.0));
            let shade = if land { (1.0 + slope * exaggerate).clamp(0.6, 1.4) } else { 1.0 };
            let [r, g, b] = base.map(|v| (v as f64 * shade).clamp(0.0, 255.0) as u8);
            Color32::from_rgb(r, g, b)
        })
        .collect();
    egui::ColorImage::new([TILE, TILE], pixels)
}

/// Map colour of a column before blending and shading: the column seen
/// from orbit on land (canopy included), sea ice, or water from turquoise
/// shallows to dark deep sea.
pub fn map_color(c: &ColumnSample, vscale: f64) -> [u8; 3] {
    if !c.inside {
        OUTSIDE
    } else if c.ice {
        worldgen::blocks::ICE.info().color
    } else if c.underwater {
        let d = ((c.shape.water - c.surface_y) as f64 / (40.0 * vscale)).clamp(0.0, 1.0).sqrt();
        let (shallow, deep) = ([62.0, 140.0, 178.0], [14.0, 38.0, 96.0]);
        std::array::from_fn(|k| lerp(d, shallow[k], deep[k]) as u8)
    } else {
        // Snow and bare rock high up read a little brighter, like sunlit
        // peaks.
        let alt = ((c.surface_y - SEA_LEVEL) as f64 / (150.0 * vscale)).clamp(0.0, 1.0);
        c.orbit_color().map(|v| (v as f64 * (0.95 + 0.12 * alt)).min(255.0) as u8)
    }
}

/// The cloud layer: white with alpha. Fractal noise whose finest octave is
/// one pixel wide (finer ones would only alias), warped for swirls, and
/// thresholded by a cloud cover that grows with humidity: rainforests and
/// oceans are cloudy, deserts mostly clear.
fn paint_clouds(samples: &[Option<ColumnSample>], key: TileKey, seed: u64) -> egui::ColorImage {
    let bpp = key.bpp();
    // Largest systems ~2,000 km (2^21 blocks), down to one pixel.
    let first = -21;
    let octaves = (1..=16).take_while(|&k| 2f64.powi(-(first + k - 1)) >= bpp).count().max(1);
    let amps: Vec<f64> = (0..octaves).map(|k| 0.7f64.powi(k as i32)).collect();
    let noise = OctaveNoise::new(derive_seed(seed, "map/clouds"), &OctaveConfig::new(first, &amps));
    let warp_x = OctaveNoise::new(derive_seed(seed, "map/clouds_wx"), &OctaveConfig::new(-21, &[1.0, 0.6, 0.35]));
    let warp_z = OctaveNoise::new(derive_seed(seed, "map/clouds_wz"), &OctaveConfig::new(-21, &[1.0, 0.6, 0.35]));
    let (x0, z0) = key.origin();
    let pixels = (0..TILE * TILE)
        .map(|i| {
            let (px, pz) = (i % TILE, i / TILE);
            let Some(s) = samples[px + 1 + (pz + 1) * SAMPLED].as_ref().filter(|s| s.inside) else {
                return Color32::TRANSPARENT;
            };
            let x = x0 + (px as f64 + 0.5) * bpp;
            let z = z0 + (pz as f64 + 0.5) * bpp;
            let (wx, wz) = (x + 8.0e5 * warp_x.sample_2d(x, z), z + 8.0e5 * warp_z.sample_2d(x, z));
            let n = noise.sample_2d(wx, wz);
            let cover = (0.36 + 0.4 * s.shape.climate.humidity).clamp(0.06, 0.8);
            // n is roughly normal around 0 with spread ~0.3: cut it so
            // about `cover` of the sky is cloudy.
            let cut = 0.3 * (1.0 - 2.0 * cover);
            let a = smoothstep(cut - 0.03, cut + 0.16, n);
            Color32::from_white_alpha((a * 255.0) as u8)
        })
        .collect();
    egui::ColorImage::new([TILE, TILE], pixels)
}

/// `1234.5` → "1.2 km", `3.0` → "3 blocks".
fn format_blocks(b: f64) -> String {
    if b >= 1000.0 {
        format!("{:.1} km", b / 1000.0)
    } else if b >= 10.0 {
        format!("{b:.0} blocks")
    } else {
        format!("{b:.2} blocks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use worldgen::settings::WorldSize;
    use worldgen::WorldSettings;

    #[test]
    fn map_image_shows_the_world() {
        let g =
            WorldGenerator::new(WorldSettings { seed: 42, size: WorldSize::from_blocks(16_000), ..Default::default() });
        // A tile of 64 blocks per pixel holds the whole 16 km world plus a
        // margin: both water and land, and the dark area outside the border.
        let key = TileKey { level: 6, tx: -1, tz: -1 };
        let samples = sample_tile(&g, key);
        let img = paint(&samples, key.bpp(), 1.0);
        let clouds = paint_clouds(&samples, key, 42);
        assert!(clouds.pixels.iter().any(|p| p.a() > 0) && clouds.pixels.iter().any(|p| p.a() == 0));
        let px: std::collections::HashSet<_> = img.pixels.iter().copied().collect();
        assert!(px.contains(&Color32::from_rgb(10, 10, 14)), "border not shown");
        assert!(img.pixels.iter().any(|p| p.b() > 90 && p.r() < 70), "no water");
        assert!(img.pixels.iter().any(|p| p.g() > p.b()), "no land");
    }

    #[test]
    fn tiles_nest_in_their_ancestors() {
        let k = TileKey { level: 3, tx: -5, tz: 6 };
        let a = k.ancestor(2);
        assert_eq!((a.level, a.tx, a.tz), (5, -2, 1));
        let (ox, oz) = k.origin();
        let (ax, az) = a.origin();
        assert!(ox >= ax && ox + k.span() <= ax + a.span());
        assert!(oz >= az && oz + k.span() <= az + a.span());
    }

    #[test]
    fn block_formatting() {
        assert_eq!(format_blocks(0.5), "0.50 blocks");
        assert_eq!(format_blocks(64.0), "64 blocks");
        assert_eq!(format_blocks(117_187.4), "117.2 km");
    }
}
