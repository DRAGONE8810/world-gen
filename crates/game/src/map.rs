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

use egui::{Color32, Pos2, Rect, Sense, Vec2};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use worldgen::{ColumnSample, WorldGenerator, SEA_LEVEL};

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
}

struct Job {
    center: (f64, f64),
    blocks_per_px: f64,
    px: usize,
    rx: Receiver<egui::ColorImage>,
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
    pub fn new(player: (f64, f64), terrain_scale: f64) -> Self {
        Self { center: player, blocks_per_point: 8.0 * terrain_scale, image: None, job: None, needs_full: true }
    }

    /// Draws the map full-screen. `player` is the camera position (x, z)
    /// and `yaw` its heading.
    pub fn ui(&mut self, ui: &mut egui::Ui, gen: &Arc<WorldGenerator>, player: (f64, f64), yaw: f32) -> MapAction {
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
                painter.image(
                    im.texture.id(),
                    Rect::from_min_size(min, Vec2::splat(size)),
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
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
        });
        action
    }

    fn start(&mut self, gen: &Arc<WorldGenerator>, px: usize, span: f64, vscale: f64) {
        let (tx, rx) = channel();
        let (gen, center, bpp) = (gen.clone(), self.center, span / px as f64);
        std::thread::spawn(move || {
            let _ = tx.send(render(&gen, center, bpp, px, vscale));
        });
        self.job = Some(Job { center, blocks_per_px: bpp, px, rx });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        match job.rx.try_recv() {
            Ok(img) => {
                let texture = ctx.load_texture("world_map", img, egui::TextureOptions::NEAREST);
                self.image =
                    Some(MapImage { center: job.center, blocks_per_px: job.blocks_per_px, px: job.px, texture });
                self.job = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.job = None,
        }
    }
}

/// Samples a `px × px` image centred on `center`, `bpp` blocks per pixel.
fn render(gen: &WorldGenerator, center: (f64, f64), bpp: f64, px: usize, vscale: f64) -> egui::ColorImage {
    use rayon::prelude::*;
    let x0 = center.0 - bpp * px as f64 / 2.0;
    let z0 = center.1 - bpp * px as f64 / 2.0;
    let pixels: Vec<Color32> = (0..px * px)
        .into_par_iter()
        .map(|i| {
            let x = (x0 + ((i % px) as f64 + 0.5) * bpp).floor();
            let z = (z0 + ((i / px) as f64 + 0.5) * bpp).floor();
            if x.abs() > i32::MAX as f64 || z.abs() > i32::MAX as f64 {
                return Color32::from_rgb(10, 10, 14);
            }
            let [r, g, b] = map_color(&gen.sample_column(x as i32, z as i32), vscale);
            Color32::from_rgb(r, g, b)
        })
        .collect();
    egui::ColorImage::new([px, px], pixels)
}

/// Map colour of a column: the top block, water darker with depth, land
/// brighter with altitude so mountains read without hill shading.
pub fn map_color(c: &ColumnSample, vscale: f64) -> [u8; 3] {
    if !c.inside {
        [10, 10, 14]
    } else if c.ice {
        worldgen::blocks::ICE.info().color
    } else if c.underwater {
        let d = ((c.shape.water - c.surface_y) as f64 / (50.0 * vscale)).clamp(0.0, 1.0) as f32;
        [(40.0 - d * 20.0) as u8, (90.0 - d * 40.0) as u8, (200.0 - d * 60.0) as u8]
    } else {
        let alt = ((c.surface_y - SEA_LEVEL) as f64 / (150.0 * vscale)).clamp(0.0, 1.0);
        let k = 0.85 + 0.3 * alt;
        c.top_color().map(|v| (v as f64 * k).min(255.0) as u8)
    }
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
        let img = render(&g, (0.0, 0.0), 20_000.0 / 64.0, 64, 1.0);
        let px: std::collections::HashSet<_> = img.pixels.iter().copied().collect();
        assert!(px.contains(&Color32::from_rgb(10, 10, 14)), "border not shown");
        assert!(img.pixels.iter().any(|p| p.b() > 150 && p.r() < 80), "no water");
        assert!(img.pixels.iter().any(|p| p.g() > p.b()), "no land");
    }

    #[test]
    fn block_formatting() {
        assert_eq!(format_blocks(0.5), "0.50 blocks");
        assert_eq!(format_blocks(64.0), "64 blocks");
        assert_eq!(format_blocks(117_187.4), "117.2 km");
    }
}
