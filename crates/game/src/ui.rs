//! Game menus (egui): main menu, world creation, settings, pause menu and
//! the in-game debug overlay.

use crate::config::GameSettings;
use egui::{Align2, Color32, RichText, Vec2};
use std::sync::mpsc;
use worldgen::settings::{
    seed_from_text, WorldHeight, MAX_WORLD_HEIGHT, MAX_WORLD_WIDTH, MIN_WORLD_HEIGHT, MIN_WORLD_WIDTH,
};
use worldgen::{Generator, Planet, SimQuality, WorldGenerator, WorldSettings, WorldSize, WorldType, SEA_LEVEL};

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
    /// The world map, opened from the game with M.
    Map,
    /// A realistic planet is being simulated before the world starts.
    Loading,
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
    /// World height in blocks (rounded to whole 16-block sections on use).
    pub height: u32,
    pub world_type: WorldType,
    pub features: bool,
    pub generator: Generator,
    pub planet: Planet,
    pub quality: SimQuality,
    preview: Option<(WorldSettings, egui::TextureHandle)>,
    /// Preview being drawn on a worker thread (a realistic planet takes
    /// seconds to simulate, and the menu must stay responsive).
    preview_job: Option<(WorldSettings, mpsc::Receiver<egui::ColorImage>)>,
}

impl WorldForm {
    pub fn from_settings(s: &WorldSettings) -> Self {
        Self {
            seed_text: String::new(),
            size_km: s.size.width_km(),
            height: s.height.blocks(),
            world_type: s.world_type,
            features: s.features,
            generator: s.generator,
            planet: s.planet,
            quality: s.quality,
            preview: None,
            preview_job: None,
        }
    }

    fn to_settings(&self, seed: u64) -> WorldSettings {
        WorldSettings {
            seed,
            size: WorldSize::from_km(self.size_km),
            height: WorldHeight::from_blocks(self.height),
            world_type: self.world_type,
            features: self.features,
            generator: self.generator,
            planet: self.planet,
            quality: self.quality,
        }
        .normalized()
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
                    let size = form.to_settings(0).size;
                    let (w, d) = (size.width_blocks() as f64, size.depth_blocks() as f64);
                    ui.label(
                        RichText::new(format!("{:.0} × {:.0} blocks · {:.0} chunks across", w, d, (w / 16.0).ceil()))
                            .weak(),
                    );
                });
                ui.end_row();

                ui.label("World height");
                ui.vertical(|ui| {
                    egui::ComboBox::from_id_salt("height_preset")
                        .selected_text(format!("{} blocks", group_thousands(form.height)))
                        .show_ui(ui, |ui| {
                            for (label, blocks) in WorldHeight::PRESETS {
                                ui.selectable_value(&mut form.height, *blocks, *label);
                            }
                        });
                    ui.add(
                        egui::Slider::new(&mut form.height, MIN_WORLD_HEIGHT..=MAX_WORLD_HEIGHT)
                            .logarithmic(true)
                            .suffix(" blocks"),
                    );
                    let h = WorldHeight::from_blocks(form.height);
                    ui.label(
                        RichText::new(format!(
                            "{}y {} to {} · sea at {} · terrain ×{:.2}",
                            if h.blocks() != form.height {
                                format!("rounded to {} · ", group_thousands(h.blocks()))
                            } else {
                                String::new()
                            },
                            h.min_y(),
                            h.max_y(),
                            SEA_LEVEL,
                            h.terrain_scale()
                        ))
                        .weak(),
                    );
                });
                ui.end_row();

                ui.label("Generator");
                ui.vertical(|ui| {
                    let before = form.generator;
                    for g in Generator::ALL {
                        ui.radio_value(&mut form.generator, g, g.label()).on_hover_text(g.description());
                    }
                    ui.label(RichText::new(form.generator.description()).weak());
                    if before != form.generator && form.generator == Generator::Realistic {
                        // A planet reads best at its real size, with the
                        // tallest world for Everest-sized mountains.
                        form.size_km = form.size_km.max(40_000.0);
                        form.height = form.height.max(MAX_WORLD_HEIGHT);
                    }
                });
                ui.end_row();

                match form.generator {
                    Generator::Classic => {
                        ui.label("World type");
                        ui.vertical(|ui| {
                            for t in WorldType::ALL {
                                ui.radio_value(&mut form.world_type, t, t.label()).on_hover_text(t.description());
                            }
                            ui.label(RichText::new(form.world_type.description()).weak());
                        });
                        ui.end_row();
                    }
                    Generator::Realistic => {
                        ui.label("Planet");
                        ui.vertical(|ui| {
                            for p in Planet::ALL {
                                ui.radio_value(&mut form.planet, p, p.label()).on_hover_text(p.description());
                            }
                            ui.label(RichText::new(form.planet.description()).weak());
                        });
                        ui.end_row();

                        ui.label("Simulation");
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                for q in SimQuality::ALL {
                                    ui.selectable_value(&mut form.quality, q, q.label()).on_hover_text(q.description());
                                }
                            });
                            ui.label(RichText::new(form.quality.description()).weak());
                        });
                        ui.end_row();
                    }
                }

                ui.label("Trees");
                ui.checkbox(&mut form.features, "Generate trees, cacti and giant mushrooms");
                ui.end_row();
            });

            let ui = &mut cols[1];
            ui.label(RichText::new("Preview").strong());
            let seed = seed_from_text(&form.seed_text).unwrap_or(0);
            let wanted = form.to_settings(seed);
            // Collect a finished preview; start a new one when the settings
            // changed (an outdated job is simply dropped).
            if let Some((s, rx)) = &form.preview_job {
                match rx.try_recv() {
                    Ok(image) => {
                        let tex = ui.ctx().load_texture("world_preview", image, egui::TextureOptions::NEAREST);
                        form.preview = Some((s.clone(), tex));
                        form.preview_job = None;
                    }
                    Err(mpsc::TryRecvError::Empty) => ui.ctx().request_repaint(),
                    Err(mpsc::TryRecvError::Disconnected) => form.preview_job = None,
                }
            }
            let shown = form.preview.as_ref().is_some_and(|(s, _)| *s == wanted);
            let running = form.preview_job.as_ref().is_some_and(|(s, _)| *s == wanted);
            if !shown && !running {
                let (tx, rx) = mpsc::channel();
                let job = wanted.clone();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send(preview_image(&job, 200));
                    ctx.request_repaint();
                });
                form.preview_job = Some((wanted.clone(), rx));
            }
            if let Some((s, tex)) = &form.preview {
                let side = ui.available_width().min(360.0);
                let [w, h] = tex.size();
                ui.image((tex.id(), Vec2::new(side, side * h as f32 / w as f32)));
                let extent = preview_extent(s);
                ui.label(
                    RichText::new(if extent as u32 >= s.size.max_extent() {
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
            if !shown {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if wanted.generator == Generator::Realistic {
                        "Simulating the planet…"
                    } else {
                        "Drawing…"
                    });
                });
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

/// Width of the area shown by the preview. Tall worlds stretch the terrain
/// sideways by the same factor as upwards, so the preview zooms out with
/// them; a realistic planet is shown whole.
fn preview_extent(s: &WorldSettings) -> f64 {
    if s.generator == Generator::Realistic {
        return s.size.max_extent() as f64;
    }
    (s.size.width_blocks() as f64).min(16_000.0 * s.height.terrain_scale())
}

/// Top-down colour map straight from the generator, `px` wide (half as
/// tall for a 2:1 world shown whole).
fn preview_image(s: &WorldSettings, px: usize) -> egui::ColorImage {
    let gen = WorldGenerator::new(s.clone());
    let extent = preview_extent(s);
    let pz = if extent >= s.size.max_extent() as f64 {
        (px as f64 * s.size.depth_blocks() as f64 / s.size.width_blocks() as f64).round().max(1.0) as usize
    } else {
        px
    };
    let step = extent / px as f64;
    let vscale = s.height.terrain_scale();
    use rayon::prelude::*;
    let samples: Vec<_> = (0..px * pz)
        .into_par_iter()
        .map(|i| {
            let (ix, iz) = (i % px, i / px);
            let x = (-extent / 2.0 + (ix as f64 + 0.5) * step).floor() as i32;
            let z = (-(pz as f64) * step / 2.0 + (iz as f64 + 0.5) * step).floor() as i32;
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
                let d = ((c.shape.water - c.surface_y) as f64 / (50.0 * vscale)).clamp(0.0, 1.0) as f32;
                [(40.0 - d * 20.0) as u8, (90.0 - d * 40.0) as u8, (200.0 - d * 60.0) as u8]
            } else {
                c.top_color()
            };
            pixels.push(Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        }
    }
    egui::ColorImage::new([px, pz], pixels)
}

/// "Simulating the planet" while a realistic world is created.
pub fn loading(ui: &mut egui::Ui, s: &WorldSettings, seconds: f32, actions: &mut Vec<Action>) {
    egui::CentralPanel::default().show(ui, |ui| {
        centered_column(ui, |ui| {
            ui.label(RichText::new("Creating the planet").size(32.0).strong());
            ui.add_space(16.0);
            ui.label(
                RichText::new(format!(
                    "{} · {} · {} simulation",
                    s.planet.label(),
                    format_km(s.size.width_km()),
                    s.quality.label()
                ))
                .weak(),
            );
            ui.add_space(24.0);
            ui.spinner();
            ui.add_space(8.0);
            ui.label(format!("Moving plates, raising mountains, eroding valleys… {seconds:.0} s"));
            ui.add_space(32.0);
            if big_button(ui, "Cancel").clicked() {
                actions.push(Action::Goto(Screen::CreateWorld));
            }
        });
    });
    ui.ctx().request_repaint();
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

/// `17696` → `"17,696"`.
pub fn group_thousands(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Returns true when the user pressed "Done".
pub fn settings(ui: &mut egui::Ui, s: &mut GameSettings) -> bool {
    let mut done = false;
    egui::CentralPanel::default().show(ui, |ui| {
        centered_column(ui, |ui| {
            ui.label(RichText::new("Settings").size(32.0).strong());
            ui.add_space(20.0);
            egui::Grid::new("settings").num_columns(2).spacing([16.0, 12.0]).show(ui, |ui| {
                ui.label("Graphics");
                ui.vertical(|ui| {
                    let current = crate::config::GraphicsPreset::of(s);
                    ui.horizontal(|ui| {
                        for p in crate::config::GraphicsPreset::ALL {
                            if ui.selectable_label(current == Some(p), p.label()).on_hover_text(p.description()).clicked() {
                                p.apply(s);
                            }
                        }
                    });
                    ui.label(
                        RichText::new(match current {
                            Some(p) => p.description(),
                            None => "Custom view distances.",
                        })
                        .weak(),
                    );
                });
                ui.end_row();
                ui.label("Render distance");
                ui.vertical(|ui| {
                    ui.add(
                        egui::Slider::new(&mut s.render_distance, 2..=crate::config::MAX_RENDER_DISTANCE)
                            .suffix(" chunks"),
                    );
                    ui.label(RichText::new("Full detail, every block.").weak());
                });
                ui.end_row();
                ui.label("LOD distance");
                ui.vertical(|ui| {
                    ui.add(egui::Slider::new(&mut s.lod_distance, 0..=crate::config::MAX_LOD_DISTANCE).suffix(" chunks"));
                    let side = 2 * s.lod_distance.max(s.render_distance) as u64;
                    ui.label(
                        RichText::new(if s.lod_distance > s.render_distance {
                            format!(
                                "Simplified terrain out to {} blocks ({} × {} = {} chunks of area). Tall worlds see further, by their terrain scale.",
                                group_thousands(s.lod_distance * 16),
                                side,
                                side,
                                group_thousands((side * side) as u32)
                            )
                        } else {
                            "Off: set it above the render distance.".to_string()
                        })
                        .weak(),
                    );
                });
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
            if big_button(ui, "World map").clicked() {
                actions.push(Action::Goto(Screen::Map));
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
    /// Latitude of the camera on a realistic planet.
    pub latitude: Option<f64>,
    pub stats: crate::world::Stats,
    pub drawn_chunks: usize,
    pub drawn_triangles: u64,
    pub gpu_vertex_bytes: u64,
    pub draw_calls: usize,
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
                    match h.latitude {
                        Some(lat) => ui.label(mono(format!(
                            "Chunk {cx} {cz}   latitude {:.1}°{}",
                            lat.abs(),
                            if lat >= 0.0 { "N" } else { "S" }
                        ))),
                        None => ui.label(mono(format!("Chunk {cx} {cz}"))),
                    };
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
                        "Seed {}  {}  {} wide  {} tall",
                        h.world.seed as i64,
                        h.world.world_type.label(),
                        format_km(h.world.size.width_km()),
                        group_thousands(h.world.height.blocks())
                    )));
                    ui.label(mono(format!(
                        "Chunks: {} loaded, {} meshed   LOD tiles: {}",
                        h.stats.loaded_chunks, h.stats.meshed_chunks, h.stats.lod_tiles
                    )));
                    ui.label(mono(format!(
                        "Meshes drawn {} in {} draw calls   {} to build, {} jobs",
                        h.drawn_chunks, h.draw_calls, h.stats.pending, h.stats.jobs_in_flight
                    )));
                    ui.label(mono(format!(
                        "Triangles {:.2} M   blocks {:.1} MB   GPU meshes {:.1} MB",
                        h.drawn_triangles as f64 / 1e6,
                        h.stats.chunk_memory_bytes as f64 / 1e6,
                        h.gpu_vertex_bytes as f64 / 1e6
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
    group_thousands(v.round() as u32)
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
        assert_eq!(group_thousands(256), "256");
        assert_eq!(group_thousands(17_696), "17,696");
        assert_eq!(group_thousands(1_048_576), "1,048,576");
    }
}
