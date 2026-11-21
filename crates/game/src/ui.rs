//! Game menus (egui): main menu, world creation, settings, pause menu and
//! the in-game debug overlay.

use crate::config::GameSettings;
use egui::{Align2, Color32, RichText, Vec2};
use worldgen::settings::{seed_from_text, MAX_WORLD_WIDTH, MIN_WORLD_WIDTH};
use worldgen::{WorldGenerator, WorldSettings, WorldSize, WorldType, SEA_LEVEL};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    MainMenu,
    CreateWorld,
    /// Settings, remembering whether we came from the pause menu.
    Settings {
        from_pause: bool,
    },
    Playing,
    Paused,
}

pub enum Action {
    StartWorld(WorldSettings),
    Goto(Screen),
    Resume,
    QuitToMenu,
    Exit,
}

/// State of the "Create world" form.
pub struct WorldForm {
    pub seed_text: String,
    pub size_km: f64,
    pub world_type: WorldType,
    pub features: bool,
    preview: Option<(WorldSettings, egui::TextureHandle)>,
}

impl WorldForm {
    pub fn from_settings(s: &WorldSettings) -> Self {
        Self {
            seed_text: String::new(),
            size_km: s.size.width_km(),
            world_type: s.world_type,
            features: s.features,
            preview: None,
        }
    }

    fn to_settings(&self, seed: u64) -> WorldSettings {
        WorldSettings {
            seed,
            size: WorldSize::from_km(self.size_km),
            world_type: self.world_type,
            features: self.features,
        }
    }
}

fn big_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(20.0)).min_size(Vec2::new(260.0, 44.0)))
}

fn centered_column(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.18).max(20.0));
        add(ui)
    });
}

pub fn main_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    egui::CentralPanel::default().show(ui, |ui| {
        centered_column(ui, |ui| {
            ui.label(RichText::new("WORLD GEN").size(56.0).strong());
            ui.label(RichText::new("procedural voxel worlds, 1 km to 60,000 km").size(16.0).weak());
            ui.add_space(40.0);
            if big_button(ui, "Play").clicked() {
                actions.push(Action::Goto(Screen::CreateWorld));
            }
            ui.add_space(8.0);
            if big_button(ui, "Settings").clicked() {
                actions.push(Action::Goto(Screen::Settings { from_pause: false }));
            }
            ui.add_space(8.0);
            if big_button(ui, "Quit").clicked() {
                actions.push(Action::Exit);
            }
        });
    });
}

pub fn create_world(ui: &mut egui::Ui, form: &mut WorldForm, actions: &mut Vec<Action>) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(16.0);
            ui.label(RichText::new("Create world").size(32.0).strong());
        });
        ui.add_space(16.0);
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            egui::Grid::new("world_form").num_columns(2).spacing([16.0, 12.0]).show(ui, |ui| {
                ui.label("Seed");
                ui.add(
                    egui::TextEdit::singleline(&mut form.seed_text)
                        .hint_text("leave empty for random")
                        .desired_width(240.0),
                );
                ui.end_row();

                ui.label("World size");
                ui.vertical(|ui| {
                    egui::ComboBox::from_id_salt("size_preset").selected_text(format_km(form.size_km)).show_ui(
                        ui,
                        |ui| {
                            for (label, blocks) in WorldSize::PRESETS {
                                ui.selectable_value(&mut form.size_km, *blocks as f64 / 1000.0, *label);
                            }
                        },
                    );
                    ui.add(
                        egui::Slider::new(
                            &mut form.size_km,
                            MIN_WORLD_WIDTH as f64 / 1000.0..=MAX_WORLD_WIDTH as f64 / 1000.0,
                        )
                        .logarithmic(true)
                        .suffix(" km")
                        .max_decimals(0),
                    );
                    let w = WorldSize::from_km(form.size_km).width_blocks() as f64;
                    ui.label(
                        RichText::new(format!("{:.0} × {:.0} blocks · {:.0} chunks across", w, w, (w / 16.0).ceil()))
                            .weak(),
                    );
                });
                ui.end_row();

                ui.label("World type");
                ui.vertical(|ui| {
                    for t in WorldType::ALL {
                        ui.radio_value(&mut form.world_type, t, t.label()).on_hover_text(t.description());
                    }
                    ui.label(RichText::new(form.world_type.description()).weak());
                });
                ui.end_row();

                ui.label("Trees");
                ui.checkbox(&mut form.features, "Generate trees, cacti and giant mushrooms");
                ui.end_row();
            });

            let ui = &mut cols[1];
            ui.label(RichText::new("Preview").strong());
            let seed = seed_from_text(&form.seed_text).unwrap_or(0);
            let wanted = form.to_settings(seed);
            let needs_update = form.preview.as_ref().is_none_or(|(s, _)| *s != wanted);
            if needs_update {
                let image = preview_image(&wanted, 160);
                let tex = ui.ctx().load_texture("world_preview", image, egui::TextureOptions::NEAREST);
                form.preview = Some((wanted.clone(), tex));
            }
            if let Some((s, tex)) = &form.preview {
                let side = ui.available_width().min(360.0);
                ui.image((tex.id(), Vec2::splat(side)));
                let extent = preview_extent(s);
                ui.label(
                    RichText::new(if extent as u32 >= s.size.width_blocks() {
                        "Whole world, top-down. Water is blue.".to_string()
                    } else {
                        format!("Central {} of the world, top-down.", format_km(extent / 1000.0))
                    })
                    .weak(),
                );
                if form.seed_text.trim().is_empty() {
                    ui.label(RichText::new("(Previewing seed 0. An empty seed picks a random one.)").weak().small());
                }
            }
        });

        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            if big_button(ui, "Create world").clicked() {
                let seed = seed_from_text(&form.seed_text).unwrap_or_else(random_seed);
                actions.push(Action::StartWorld(form.to_settings(seed)));
            }
            ui.add_space(6.0);
            if big_button(ui, "Back").clicked() {
                actions.push(Action::Goto(Screen::MainMenu));
            }
        });
    });
}

fn preview_extent(s: &WorldSettings) -> f64 {
    (s.size.width_blocks() as f64).min(16_000.0)
}

/// Top-down colour map straight from the generator.
fn preview_image(s: &WorldSettings, px: usize) -> egui::ColorImage {
    let gen = WorldGenerator::new(s.clone());
    let extent = preview_extent(s);
    let step = extent / px as f64;
    use rayon::prelude::*;
    let samples: Vec<_> = (0..px * px)
        .into_par_iter()
        .map(|i| {
            let (ix, iz) = (i % px, i / px);
            let x = (-extent / 2.0 + (ix as f64 + 0.5) * step).floor() as i32;
            let z = (-extent / 2.0 + (iz as f64 + 0.5) * step).floor() as i32;
            gen.sample_column(x, z)
        })
        .collect();
    let mut pixels = Vec::with_capacity(px * px);
    {
        for c in samples {
            let rgb = if !c.inside {
                [10, 10, 14]
            } else if c.ice {
                worldgen::blocks::ICE.info().color
            } else if c.underwater {
                let d = ((SEA_LEVEL - c.surface_y) as f32 / 50.0).clamp(0.0, 1.0);
                [(40.0 - d * 20.0) as u8, (90.0 - d * 40.0) as u8, (200.0 - d * 60.0) as u8]
            } else {
                c.top_color()
            };
            pixels.push(Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        }
    }
    egui::ColorImage::new([px, px], pixels)
}

fn random_seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    worldgen::rng::mix64(t as u64 ^ (t >> 64) as u64)
}

pub fn format_km(km: f64) -> String {
    if km >= 59_999.0 {
        "60,000 km (full)".into()
    } else if km >= 1000.0 {
        let n = km.round() as u64;
        format!("{},{:03} km", n / 1000, n % 1000)
    } else if km >= 10.0 {
        format!("{km:.0} km")
    } else {
        format!("{km:.1} km")
    }
}

/// Returns true when the user pressed "Done".
pub fn settings(ui: &mut egui::Ui, s: &mut GameSettings) -> bool {
    let mut done = false;
    egui::CentralPanel::default().show(ui, |ui| {
        centered_column(ui, |ui| {
            ui.label(RichText::new("Settings").size(32.0).strong());
            ui.add_space(20.0);
            egui::Grid::new("settings").num_columns(2).spacing([16.0, 12.0]).show(ui, |ui| {
                ui.label("Render distance");
                ui.add(egui::Slider::new(&mut s.render_distance, 2..=32).suffix(" chunks"));
                ui.end_row();
                ui.label("Field of view");
                ui.add(egui::Slider::new(&mut s.fov_deg, 50.0..=110.0).suffix("°"));
                ui.end_row();
                ui.label("Mouse sensitivity");
                ui.add(egui::Slider::new(&mut s.mouse_sensitivity, 0.1..=3.0));
                ui.end_row();
                ui.label("Fly speed");
                ui.add(
                    egui::Slider::new(&mut s.fly_speed, crate::config::MIN_FLY_SPEED..=crate::config::MAX_FLY_SPEED)
                        .logarithmic(true)
                        .suffix(" blocks/s"),
                );
                ui.end_row();
                ui.label("VSync");
                ui.checkbox(&mut s.vsync, "Limit FPS to the monitor refresh rate");
                ui.end_row();
                ui.label("Debug overlay");
                ui.checkbox(&mut s.show_debug, "Show (toggle in game with F3)");
                ui.end_row();
            });
            ui.add_space(24.0);
            if big_button(ui, "Done").clicked() {
                done = true;
            }
        });
    });
    done
}

pub fn pause_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_black_alpha(140))).show(ui, |ui| {
        centered_column(ui, |ui| {
            ui.label(RichText::new("Paused").size(36.0).strong().color(Color32::WHITE));
            ui.add_space(24.0);
            if big_button(ui, "Resume").clicked() {
                actions.push(Action::Resume);
            }
            ui.add_space(8.0);
            if big_button(ui, "Settings").clicked() {
                actions.push(Action::Goto(Screen::Settings { from_pause: true }));
            }
            ui.add_space(8.0);
            if big_button(ui, "Quit to title").clicked() {
                actions.push(Action::QuitToMenu);
            }
        });
    });
}

pub struct HudInfo<'a> {
    pub fps: f32,
    pub frame_ms: f32,
    pub pos: glam::DVec3,
    pub world: &'a WorldSettings,
    pub column: worldgen::ColumnSample,
    pub stats: crate::world::Stats,
    pub drawn_chunks: usize,
    pub drawn_triangles: u64,
    pub gpu: &'a str,
    pub pending: usize,
    pub show_debug: bool,
    pub fly_speed: f32,
}

pub fn hud(ui: &mut egui::Ui, h: &HudInfo) {
    let ctx = ui.ctx().clone();
    if h.show_debug {
        egui::Area::new("debug".into()).anchor(Align2::LEFT_TOP, [8.0, 8.0]).interactable(false).show(&ctx, |ui| {
            egui::Frame::NONE.fill(Color32::from_black_alpha(150)).inner_margin(8.0).corner_radius(4.0).show(
                ui,
                |ui| {
                    let mono = |s: String| RichText::new(s).monospace().color(Color32::WHITE);
                    ui.label(mono(format!("{:.0} fps ({:.1} ms)   {}", h.fps, h.frame_ms, h.gpu)));
                    ui.label(mono(format!("XYZ  {:.1} / {:.1} / {:.1}", h.pos.x, h.pos.y, h.pos.z)));
                    ui.label(mono(format!("Speed {} blocks/s  (wheel: ×2 / ÷2, Ctrl ×5)", format_speed(h.fly_speed))));
                    let (cx, cz) = ((h.pos.x.floor() as i64) >> 4, (h.pos.z.floor() as i64) >> 4);
                    ui.label(mono(format!("Chunk {cx} {cz}")));
                    ui.label(mono(format!(
                        "Ground {}  top {}{}",
                        h.column.surface_y,
                        h.column.top_block.info().name,
                        if h.column.underwater { "  (under water)" } else { "" }
                    )));
                    let cl = h.column.shape.climate;
                    ui.label(mono(format!(
                        "Biome {}{}  (T {:+.2} at this height, H {:+.2})",
                        h.column.biome.display_name(),
                        if h.column.biome.origin() == worldgen::biome::Origin::RealWorld {
                            " (real world)"
                        } else {
                            ""
                        },
                        worldgen::biome::effective_temperature(&h.column.shape, h.column.surface_y),
                        cl.humidity
                    )));
                    ui.label(mono(format!(
                        "Climate  C {:+.2}  E {:+.2}  PV {:+.2}  rough {:.1}  river {:.2}",
                        cl.continentalness,
                        cl.erosion,
                        cl.peaks_valleys,
                        h.column.shape.roughness,
                        h.column.shape.river
                    )));
                    ui.label(mono(format!(
                        "Seed {}  {}  {}",
                        h.world.seed as i64,
                        h.world.world_type.label(),
                        format_km(h.world.size.width_km())
                    )));
                    ui.label(mono(format!(
                        "Chunks: {} loaded, {} meshed, {} drawn, {} jobs",
                        h.stats.loaded_chunks, h.stats.meshed_chunks, h.drawn_chunks, h.stats.jobs_in_flight
                    )));
                    ui.label(mono(format!(
                        "Triangles {:.2} M   block memory {:.1} MB",
                        h.drawn_triangles as f64 / 1e6,
                        h.stats.chunk_memory_bytes as f64 / 1e6
                    )));
                },
            );
        });
    }
    if h.pending > 0 {
        egui::Area::new("loading".into()).anchor(Align2::CENTER_TOP, [0.0, 12.0]).interactable(false).show(
            &ctx,
            |ui| {
                egui::Frame::NONE.fill(Color32::from_black_alpha(150)).inner_margin(8.0).corner_radius(4.0).show(
                    ui,
                    |ui| {
                        ui.label(
                            RichText::new(format!("Generating terrain… {} chunks to go", h.pending))
                                .color(Color32::WHITE),
                        );
                    },
                );
            },
        );
    }
    egui::Area::new("controls".into()).anchor(Align2::CENTER_BOTTOM, [0.0, -8.0]).interactable(false).show(
        &ctx,
        |ui| {
            ui.label(
                RichText::new(
                    "WASD move · Space / Shift up / down · Wheel speed ×2 / ÷2 · Ctrl faster · Esc pause · F3 debug",
                )
                .color(Color32::from_white_alpha(200)),
            );
        },
    );
}

/// "0.25", "20", "1,048,576".
fn format_speed(v: f32) -> String {
    if v < 10.0 {
        return format!("{}", (v * 100.0).round() / 100.0);
    }
    let digits = format!("{}", v.round() as u64);
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_formatting() {
        assert_eq!(format_speed(0.25), "0.25");
        assert_eq!(format_speed(20.0), "20");
        assert_eq!(format_speed(1_048_576.0), "1,048,576");
    }

    #[test]
    fn km_formatting() {
        assert_eq!(format_km(1.0), "1.0 km");
        assert_eq!(format_km(16.0), "16 km");
        assert_eq!(format_km(1000.0), "1,000 km");
        assert_eq!(format_km(59_999.968), "60,000 km (full)");
    }
}
