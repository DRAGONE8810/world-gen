//! In-game world map (press M).
//!
//! A top-down view drawn straight from the generator, like `wg-map` and the
//! create-world preview: one column sample per pixel, so it shows any part
//! of the world at any zoom, from 1 block per pixel to the whole 60,000 km
//! world, without the world being loaded or stored anywhere.
//!
//! Sampling runs on a background thread. While a new view is being sampled
//! the previous image stays on screen, stretched to the new view, so
//! zooming and panning feel immediate. A coarse 128-pixel pass comes first,
//! then the full-resolution one.
//!
//! The picture imitates a satellite photo rather than a block map:
//!
//! * each land pixel shows the ground *with the biome's average tree
//!   canopy over it* (`ColumnSample::orbit_color`), so forests are dark
//!   green, and grass follows the climate smoothly (`worldgen::tint`);
//! * land colours are averaged with their land neighbours, which turns
//!   pixel-sharp biome borders into narrow transitions but keeps coasts
//!   crisp;
//! * relief is hill-shaded from the sampled heights (light from the
//!   north-west), exaggerated in proportion to the zoom so mountain ranges
//!   still show on a whole planet;
//! * water goes from turquoise shallows to dark deep sea;
//! * an optional cloud layer: fractal noise, denser where the climate is
//!   humid, drawn as a separate texture so its opacity can change without
//!   resampling.

use egui::{Color32, Pos2, Rect, Sense, Vec2};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use worldgen::noise::{OctaveConfig, OctaveNoise};
use worldgen::rng::derive_seed;
use worldgen::spline::{lerp, smoothstep};
use worldgen::{ColumnSample, WorldGenerator, SEA_LEVEL};

use crate::config::MapClouds;

/// Pixels per side of the full-resolution map image.
const MAP_PX: usize = 512;
/// Pixels per side of the quick first pass.
const QUICK_PX: usize = 128;

/// What the map wants the game to do.
pub enum MapAction {
    None,
    /// Move the camera to this block column.
    Teleport(f64, f64),
}

/// A sampled image of a square area.
struct MapImage {
    center: (f64, f64),
    blocks_per_px: f64,
    px: usize,
    texture: egui::TextureHandle,
    clouds: egui::TextureHandle,
}

struct Job {
    center: (f64, f64),
    blocks_per_px: f64,
    px: usize,
    rx: Receiver<(egui::ColorImage, egui::ColorImage)>,
}

pub struct WorldMap {
    /// Centre of the view in block coordinates.
    center: (f64, f64),
    /// Zoom: blocks per screen point.
    blocks_per_point: f64,
    image: Option<MapImage>,
    job: Option<Job>,
    /// The full-resolution pass still has to run for the current view.
    needs_full: bool,
}

impl WorldMap {
    /// Opens the map centred on the player, zoomed so the view spans ~4 km
    /// of a normal world (scaled for tall worlds).
    /// `WG_MAP_ZOOM=<blocks per point>` overrides the zoom (screenshots).
    pub fn new(player: (f64, f64), terrain_scale: f64) -> Self {
        let zoom = std::env::var("WG_MAP_ZOOM").ok().and_then(|v| v.parse().ok()).unwrap_or(8.0 * terrain_scale);
        Self { center: player, blocks_per_point: zoom, image: None, job: None, needs_full: true }
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
        let half_world = gen.settings().size.max_extent() as f64 / 2.0;
        let max_bpp = (2.0 * half_world / 256.0).max(1.0);
        let vscale = gen.settings().height.terrain_scale();

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_rgb(14, 16, 22))).show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            let (rect, response) = ui.allocate_exact_size(full.size(), Sense::click_and_drag());
            // The sampled square covers the whole panel (the shorter side is clipped).
            let side = rect.width().max(rect.height()) as f64;

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
            self.center.0 = self.center.0.clamp(-half_world, half_world);
            self.center.1 = self.center.1.clamp(-half_world, half_world);

            // ---- Sampling ----
            let span = side * self.blocks_per_point; // blocks across the square
            self.poll(ui.ctx());
            let image_current = self.image.as_ref().is_some_and(|im| {
                im.center == self.center && (im.blocks_per_px * im.px as f64 - span).abs() < 1e-9
            });
            // One job at a time: while the view keeps moving, each finished
            // job starts the next one for wherever the view is by then.
            if self.job.is_none() {
                if !image_current {
                    self.start(gen, QUICK_PX, span, vscale);
                    self.needs_full = true;
                } else if self.needs_full {
                    self.start(gen, MAP_PX, span, vscale);
                    self.needs_full = false;
                }
            }

            // ---- Drawing ----
            let painter = ui.painter_at(rect);
            if let Some(im) = &self.image {
                // Place the image where its area lies in the current view.
                let img_span = im.blocks_per_px * im.px as f64;
                let c = rect.center();
                let min = Pos2::new(
                    c.x + ((im.center.0 - img_span / 2.0 - self.center.0) / self.blocks_per_point) as f32,
                    c.y + ((im.center.1 - img_span / 2.0 - self.center.1) / self.blocks_per_point) as f32,
                );
                let size = (img_span / self.blocks_per_point) as f32;
                let (at, uv) = (Rect::from_min_size(min, Vec2::splat(size)), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)));
                painter.image(im.texture.id(), at, uv, Color32::WHITE);
                if clouds.show && clouds.opacity > 0.0 {
                    let a = (clouds.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
                    painter.image(im.clouds.id(), at, uv, Color32::from_white_alpha(a));
                }
            }
            // Player marker: a triangle pointing along the view direction.
            let to_screen = |x: f64, z: f64| {
                rect.center()
                    + Vec2::new(
                        ((x - self.center.0) / self.blocks_per_point) as f32,
                        ((z - self.center.1) / self.blocks_per_point) as f32,
                    )
            };
            let p = to_screen(player.0, player.1);
            let (s, c) = yaw.sin_cos();
            let fwd = Vec2::new(s, -c);
            let right = Vec2::new(c, s);
            let pts = vec![p + fwd * 11.0, p - fwd * 6.0 + right * 6.0, p - fwd * 6.0 - right * 6.0];
            painter.add(egui::Shape::convex_polygon(pts, Color32::from_rgb(255, 70, 50), (1.5, Color32::WHITE)));

            // ---- Text ----
            let mut lines = vec![format!(
                "{} per pixel   ·   view {}   ·   drag to move · wheel to zoom · double-click to teleport · M or Esc to close",
                format_blocks(self.blocks_per_point),
                format_blocks(span)
            )];
            if let Some(h) = response.hover_pos() {
                let (x, z) = to_world(h, self.center, self.blocks_per_point);
                let s = gen.sample_column(x.floor() as i32, z.floor() as i32);
                lines.push(if s.inside {
                    format!(
                        "x {:.0}  z {:.0}   {}   ground y {}",
                        x,
                        z,
                        s.biome.display_name(),
                        s.surface_y
                    )
                } else {
                    format!("x {x:.0}  z {z:.0}   outside the world border")
                });
                if response.double_clicked() && s.inside {
                    action = MapAction::Teleport(x, z);
                }
            }
            if self.job.is_some() {
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

    fn start(&mut self, gen: &Arc<WorldGenerator>, px: usize, span: f64, vscale: f64) {
        let (tx, rx) = channel();
        let (gen, center, bpp) = (gen.clone(), self.center, span / px as f64);
        std::thread::spawn(move || {
            let samples = sample(&gen, center, bpp, px);
            let map = paint(&samples, px, bpp, vscale);
            let clouds = paint_clouds(&samples, px, center, bpp, gen.settings().seed);
            let _ = tx.send((map, clouds));
        });
        self.job = Some(Job { center, blocks_per_px: bpp, px, rx });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        match job.rx.try_recv() {
            Ok((img, clouds)) => {
                let texture = ctx.load_texture("world_map", img, egui::TextureOptions::NEAREST);
                let clouds = ctx.load_texture("world_map_clouds", clouds, egui::TextureOptions::LINEAR);
                self.image = Some(MapImage {
                    center: job.center,
                    blocks_per_px: job.blocks_per_px,
                    px: job.px,
                    texture,
                    clouds,
                });
                self.job = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.job = None,
        }
    }
}

/// Samples a `px × px` grid of columns centred on `center`, `bpp` blocks
/// per pixel (`None` beyond the representable coordinates).
fn sample(gen: &WorldGenerator, center: (f64, f64), bpp: f64, px: usize) -> Vec<Option<ColumnSample>> {
    use rayon::prelude::*;
    let x0 = center.0 - bpp * px as f64 / 2.0;
    let z0 = center.1 - bpp * px as f64 / 2.0;
    (0..px * px)
        .into_par_iter()
        .map(|i| {
            let x = (x0 + ((i % px) as f64 + 0.5) * bpp).floor();
            let z = (z0 + ((i / px) as f64 + 0.5) * bpp).floor();
            (x.abs() <= i32::MAX as f64 && z.abs() <= i32::MAX as f64).then(|| gen.sample_column(x as i32, z as i32))
        })
        .collect()
}

/// Colour outside the world.
const OUTSIDE: [u8; 3] = [10, 10, 14];

/// Paints the satellite-style map from the samples (see the module docs).
fn paint(samples: &[Option<ColumnSample>], px: usize, bpp: f64, vscale: f64) -> egui::ColorImage {
    // Pixels blend only with their own kind: land with land, sea (water
    // and pack ice) with sea, so coasts stay sharp.
    let kind = |i: usize| match samples[i].as_ref() {
        Some(s) if s.inside => 1 + (s.underwater || s.ice) as u8,
        _ => 0,
    };
    let land = |i: usize| kind(i) == 1;
    let base: Vec<[f32; 3]> =
        samples.iter().map(|s| s.as_ref().map_or(OUTSIDE, |s| map_color(s, vscale)).map(|v| v as f32)).collect();
    // Hill shading needs heights in default-world blocks per pixel; on
    // coarse views real slopes vanish, so they're exaggerated with the zoom.
    let height = |i: usize| samples[i].as_ref().map_or(0.0, |s| s.water_top() as f64);
    let exaggerate = 0.35 * (bpp / 16.0).max(1.0).sqrt() * 2.5 / vscale;
    let pixels = (0..px * px)
        .map(|i| {
            let (x, z) = (i % px, i / px);
            if kind(i) == 0 {
                let [r, g, b] = base[i].map(|v| v as u8);
                return Color32::from_rgb(r, g, b);
            }
            // 3 × 3 average over neighbours of the same kind (centre
            // weighted double).
            let (mut sum, mut w) = ([0f32; 3], 0f32);
            for dz in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, nz) = (x as i32 + dx, z as i32 + dz);
                    if nx < 0 || nz < 0 || nx >= px as i32 || nz >= px as i32 {
                        continue;
                    }
                    let j = nx as usize + nz as usize * px;
                    if kind(j) == kind(i) {
                        let k = if dx == 0 && dz == 0 { 2.0 } else { 1.0 };
                        for c in 0..3 {
                            sum[c] += k * base[j][c];
                        }
                        w += k;
                    }
                }
            }
            // Light from the north-west (top left): ground rising towards
            // the south-east faces it.
            let up_left = i - (x > 0) as usize - if z > 0 { px } else { 0 };
            let down_right = i + (x + 1 < px) as usize + if z + 1 < px { px } else { 0 };
            let slope = (height(down_right) - height(up_left)) / (2.83 * bpp.max(1.0));
            let shade = if land(i) { (1.0 + slope * exaggerate).clamp(0.6, 1.4) as f32 } else { 1.0 };
            let [r, g, b] = sum.map(|v| (v / w * shade).clamp(0.0, 255.0) as u8);
            Color32::from_rgb(r, g, b)
        })
        .collect();
    egui::ColorImage::new([px, px], pixels)
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
fn paint_clouds(
    samples: &[Option<ColumnSample>],
    px: usize,
    center: (f64, f64),
    bpp: f64,
    seed: u64,
) -> egui::ColorImage {
    // Largest systems ~2,000 km (2^21 blocks), down to one pixel.
    let first = -21;
    let octaves = (1..=16).take_while(|&k| 2f64.powi(-(first + k - 1)) >= bpp).count().max(1);
    let amps: Vec<f64> = (0..octaves).map(|k| 0.7f64.powi(k as i32)).collect();
    let noise = OctaveNoise::new(derive_seed(seed, "map/clouds"), &OctaveConfig::new(first, &amps));
    let warp_x = OctaveNoise::new(derive_seed(seed, "map/clouds_wx"), &OctaveConfig::new(-21, &[1.0, 0.6, 0.35]));
    let warp_z = OctaveNoise::new(derive_seed(seed, "map/clouds_wz"), &OctaveConfig::new(-21, &[1.0, 0.6, 0.35]));
    let x0 = center.0 - bpp * px as f64 / 2.0;
    let z0 = center.1 - bpp * px as f64 / 2.0;
    let pixels = (0..px * px)
        .map(|i| {
            let Some(s) = samples[i].as_ref().filter(|s| s.inside) else {
                return Color32::TRANSPARENT;
            };
            let x = x0 + ((i % px) as f64 + 0.5) * bpp;
            let z = z0 + ((i / px) as f64 + 0.5) * bpp;
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
    egui::ColorImage::new([px, px], pixels)
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
        // The whole 16 km world plus a margin: both water and land, and the
        // dark area outside the border.
        let img = paint(&sample(&g, (0.0, 0.0), 20_000.0 / 64.0, 64), 64, 20_000.0 / 64.0, 1.0);
        let px: std::collections::HashSet<_> = img.pixels.iter().copied().collect();
        assert!(px.contains(&Color32::from_rgb(10, 10, 14)), "border not shown");
        assert!(img.pixels.iter().any(|p| p.b() > 90 && p.r() < 70), "no water");
        assert!(img.pixels.iter().any(|p| p.g() > p.b()), "no land");
    }

    #[test]
    fn block_formatting() {
        assert_eq!(format_blocks(0.5), "0.50 blocks");
        assert_eq!(format_blocks(64.0), "64 blocks");
        assert_eq!(format_blocks(117_187.4), "117.2 km");
    }
}
