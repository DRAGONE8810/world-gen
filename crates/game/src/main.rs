//! World Gen: game menu + Vulkan viewer for the `worldgen` crate.
//!
//! Frame loop (one `RedrawRequested` per frame):
//! 1. move the camera from keyboard input,
//! 2. advance chunk streaming and hand finished meshes to the renderer,
//! 3. build the UI for the current screen,
//! 4. render terrain + UI.

mod camera;
mod config;
mod lod;
mod map;
mod mesh;
mod render;
mod ui;
mod world;

use camera::Camera;
use config::GameSettings;
use glam::DVec3;
use render::{Renderer, UiFrame};
use std::collections::HashSet;
use std::sync::{mpsc, Arc};
use std::time::Instant;
use ui::{Action, Screen, WorldForm};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};
use world::{ChunkManager, MeshEvent};
use worldgen::{ChunkPos, WorldGenerator, WorldSettings};

const SKY: [f32; 3] = [0.62, 0.78, 0.98];
const MENU_BG: [f32; 3] = [0.10, 0.12, 0.16];

/// A world being played.
struct Session {
    settings: WorldSettings,
    chunks: ChunkManager,
    camera: Camera,
    /// The world map while it is open.
    map: Option<map::WorldMap>,
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    settings: GameSettings,
    screen: Screen,
    form: WorldForm,
    session: Option<Session>,
    keys: HashSet<KeyCode>,
    /// Touchpads scroll in pixels; collect them until they add up to a notch.
    wheel_pixels: f64,
    last_frame: Instant,
    fps: f32,
    frame_ms: f32,
    last_draw: (usize, u64, u64, usize),
    /// Screenshot-test mode: quit after this many frames.
    exit_after_frames: Option<u64>,
    frames: u64,
    fatal: Option<String>,
    /// A world being created on a worker thread: its settings, the
    /// generator and spawn point when ready, and when it started.
    loading: Option<(WorldSettings, mpsc::Receiver<NewWorld>, Instant)>,
}

/// A created world's generator and spawn point.
type NewWorld = (WorldGenerator, (i32, i32, i32));

impl App {
    fn new() -> Self {
        let settings = GameSettings::load();
        let form = WorldForm::from_settings(&settings.last_world);
        let exit_after_frames = std::env::var("WG_EXIT_AFTER_FRAMES").ok().and_then(|v| v.parse().ok());
        Self {
            window: None,
            renderer: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            settings,
            screen: Screen::MainMenu,
            form,
            session: None,
            keys: HashSet::new(),
            wheel_pixels: 0.0,
            last_frame: Instant::now(),
            fps: 0.0,
            frame_ms: 0.0,
            last_draw: (0, 0, 0, 0),
            exit_after_frames,
            frames: 0,
            fatal: None,
            loading: None,
        }
    }

    fn start_world(&mut self, settings: WorldSettings) {
        let gen = WorldGenerator::new(settings.clone());
        let spawn = gen.find_spawn();
        self.enter_world(settings, gen, spawn);
    }

    /// Creates the world on a worker thread (a realistic planet takes
    /// seconds to simulate) and shows the loading screen meanwhile.
    fn start_world_in_background(&mut self, settings: WorldSettings) {
        let (tx, rx) = mpsc::channel();
        let job = settings.clone();
        std::thread::spawn(move || {
            let gen = WorldGenerator::new(job);
            let spawn = gen.find_spawn();
            let _ = tx.send((gen, spawn));
        });
        self.loading = Some((settings, rx, Instant::now()));
        self.set_screen(Screen::Loading);
    }

    fn enter_world(&mut self, settings: WorldSettings, gen: WorldGenerator, (x, y, z): (i32, i32, i32)) {
        log::info!("Starting world {settings:?}");
        self.settings.last_world = settings.clone();
        self.settings.save();
        let mut camera = Camera::new(DVec3::new(x as f64 + 0.5, y as f64 + 24.0, z as f64 + 0.5));
        camera.fov_y_deg = self.settings.fov_deg;
        if let Some(r) = &mut self.renderer {
            r.clear_meshes();
        }
        self.session = Some(Session { settings, chunks: ChunkManager::new(gen), camera, map: None });
        self.set_screen(Screen::Playing);
    }

    fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        self.keys.clear();
        if let Some(s) = &mut self.session {
            // The map opens centred on the player each time.
            s.map = (screen == Screen::Map)
                .then(|| map::WorldMap::new((s.camera.pos.x, s.camera.pos.z), s.settings.height.terrain_scale()));
        }
        if let Some(w) = &self.window {
            let capture = screen == Screen::Playing;
            if capture {
                let _ =
                    w.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
            } else {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
            }
            w.set_cursor_visible(!capture);
        }
    }

    fn handle_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::StartWorld(s) if s.generator == worldgen::Generator::Realistic => self.start_world_in_background(s),
            Action::StartWorld(s) => self.start_world(s),
            Action::Goto(s) => {
                if self.screen == Screen::Loading {
                    // Cancelled: the worker finishes on its own, unheard.
                    self.loading = None;
                }
                self.set_screen(s)
            }
            Action::Resume => self.set_screen(Screen::Playing),
            Action::QuitToMenu => {
                self.session = None;
                if let Some(r) = &mut self.renderer {
                    r.clear_meshes();
                }
                self.set_screen(Screen::MainMenu);
            }
            Action::Exit => event_loop.exit(),
        }
    }

    fn update_game(&mut self, dt: f32) {
        if let Some((settings, rx, _)) = &self.loading {
            if let Ok((gen, spawn)) = rx.try_recv() {
                let settings = settings.clone();
                self.loading = None;
                self.enter_world(settings, gen, spawn);
            }
        }
        let Some(s) = &mut self.session else { return };
        s.camera.fov_y_deg = self.settings.fov_deg;

        if self.screen == Screen::Playing {
            let k = |c| self.keys.contains(&c);
            let mut dir = glam::Vec3::ZERO;
            if k(KeyCode::KeyW) {
                dir += s.camera.forward_flat();
            }
            if k(KeyCode::KeyS) {
                dir -= s.camera.forward_flat();
            }
            if k(KeyCode::KeyD) {
                dir += s.camera.right();
            }
            if k(KeyCode::KeyA) {
                dir -= s.camera.right();
            }
            if k(KeyCode::Space) {
                dir.y += 1.0;
            }
            if k(KeyCode::ShiftLeft) || k(KeyCode::ShiftRight) {
                dir.y -= 1.0;
            }
            let fast = if k(KeyCode::ControlLeft) || k(KeyCode::ControlRight) { 5.0 } else { 1.0 };
            if dir != glam::Vec3::ZERO {
                let v = dir.normalize() * self.settings.fly_speed * fast * dt;
                s.camera.pos += v.as_dvec3();
            }
            // Stay inside the world border and the vertical build range.
            let (hx, hz) = (s.settings.size.half_width() as f64, s.settings.size.half_depth() as f64);
            s.camera.pos.x = s.camera.pos.x.clamp(-hx + 0.5, hx - 0.5);
            s.camera.pos.z = s.camera.pos.z.clamp(-hz + 0.5, hz - 0.5);
            // Up to "orbit": high enough to see the whole world at once.
            let (lo, hi) = (s.settings.height.min_y() as f64, s.settings.height.max_y() as f64);
            let ceiling = (hi + 200.0).max(s.settings.size.width_blocks() as f64);
            s.camera.pos.y = s.camera.pos.y.clamp(lo, ceiling);
        }

        let (bx, bz) = (s.camera.pos.x.floor() as i32, s.camera.pos.z.floor() as i32);
        let center = ChunkPos::from_block(bx, bz);
        let ground = s.chunks.generator().sample_column(bx, bz).water_top() as f64;
        let altitude = quantize_altitude(((s.camera.pos.y - ground) / 16.0).floor() as i64);
        let layout = self.settings.layout(s.settings.height.terrain_scale(), world_chunks(&s.settings), altitude);
        let events = s.chunks.update(center, layout);
        if let Some(r) = &mut self.renderer {
            for e in events {
                match e {
                    MeshEvent::Upload(p, m) => r.upload_mesh(p, m),
                    MeshEvent::Remove(p) => r.remove_mesh(p),
                }
            }
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if dt > 0.0 {
            self.fps = self.fps * 0.95 + (1.0 / dt) * 0.05;
            self.frame_ms = self.frame_ms * 0.95 + dt * 1000.0 * 0.05;
        }

        self.update_game(dt);

        let (Some(window), Some(egui_state)) = (self.window.clone(), self.egui_state.as_mut()) else { return };
        let raw = egui_state.take_egui_input(&window);
        let mut actions = Vec::new();
        let ctx = self.egui_ctx.clone();
        let gpu = self.renderer.as_ref().map(|r| r.gpu_name.clone()).unwrap_or_default();
        let mut settings_done = false;
        let mut teleport = None;
        let output = ctx.run_ui(raw, |ui| match self.screen {
            Screen::MainMenu => ui::main_menu(ui, &mut actions),
            Screen::CreateWorld => ui::create_world(ui, &mut self.form, &mut actions),
            Screen::Loading => match &self.loading {
                Some((s, _, t0)) => ui::loading(ui, s, t0.elapsed().as_secs_f32(), &mut actions),
                None => actions.push(Action::Goto(Screen::CreateWorld)),
            },
            Screen::Settings { .. } => settings_done = ui::settings(ui, &mut self.settings),
            Screen::Playing | Screen::Paused => {
                if let Some(s) = &self.session {
                    ui::hud(
                        ui,
                        &ui::HudInfo {
                            fps: self.fps,
                            frame_ms: self.frame_ms,
                            pos: s.camera.pos,
                            world: &s.settings,
                            latitude: s
                                .chunks
                                .generator()
                                .realistic()
                                .map(|r| r.latitude(s.camera.pos.z.floor() as i32)),
                            column: s
                                .chunks
                                .generator()
                                .sample_column(s.camera.pos.x.floor() as i32, s.camera.pos.z.floor() as i32),
                            stats: s.chunks.stats,
                            drawn_chunks: self.last_draw.0,
                            drawn_triangles: self.last_draw.1,
                            gpu_vertex_bytes: self.last_draw.2,
                            draw_calls: self.last_draw.3,
                            gpu: &gpu,
                            pending: s.chunks.stats.pending_chunks,
                            show_debug: self.settings.show_debug,
                            fly_speed: self.settings.fly_speed,
                        },
                    );
                }
                if self.screen == Screen::Paused {
                    ui::pause_menu(ui, &mut actions);
                }
            }
            Screen::Map => {
                if let Some(s) = &mut self.session {
                    let gen = s.chunks.generator_arc().clone();
                    let player = (s.camera.pos.x, s.camera.pos.z);
                    if let Some(m) = &mut s.map {
                        if let map::MapAction::Teleport(x, z) = m.ui(ui, &gen, player, s.camera.yaw) {
                            teleport = Some((x, z));
                        }
                    }
                }
            }
        });
        if let Some((x, z)) = teleport {
            if let Some(s) = &mut self.session {
                let ground = s.chunks.generator().sample_column(x.floor() as i32, z.floor() as i32).water_top();
                s.camera.pos = DVec3::new(x, ground as f64 + 40.0, z);
            }
            actions.push(Action::Resume);
        }
        egui_state.handle_platform_output(&window, output.platform_output);

        if settings_done {
            self.settings.save();
            if let Some(r) = &mut self.renderer {
                r.set_vsync(self.settings.vsync);
            }
            let back =
                if self.screen == (Screen::Settings { from_pause: true }) { Screen::Paused } else { Screen::MainMenu };
            self.set_screen(back);
        }

        let primitives = self.egui_ctx.tessellate(output.shapes, output.pixels_per_point);
        let size = window.inner_size();
        if let Some(r) = &mut self.renderer {
            let in_world = matches!(self.screen, Screen::Playing | Screen::Paused)
                || self.screen == (Screen::Settings { from_pause: true });
            let cam = self.session.as_ref().filter(|_| in_world).map(|s| {
                let fog =
                    self.settings.view_distance_blocks(s.settings.height.terrain_scale(), world_chunks(&s.settings));
                (&s.camera, fog, s.chunks.draw_list())
            });
            let stats = r.draw(
                (size.width, size.height),
                cam,
                if cam.is_some() { SKY } else { MENU_BG },
                UiFrame {
                    primitives: &primitives,
                    textures_delta: &output.textures_delta,
                    pixels_per_point: output.pixels_per_point,
                },
            );
            self.last_draw = (stats.drawn_chunks, stats.drawn_triangles, stats.gpu_vertex_bytes, stats.draw_calls);
        }

        for a in actions {
            self.handle_action(a, event_loop);
        }

        self.frames += 1;
        if self.exit_after_frames.is_some_and(|n| self.frames >= n) {
            event_loop.exit();
        }
    }

    /// Scripted start used for automated screenshots: `WG_AUTOSTART=seed`
    /// jumps straight into a world, `WG_SCREEN=create|settings` opens a menu
    /// (`WG_SCREEN=map` with autostart opens the world map).
    fn autostart(&mut self) {
        if let Ok(seed) = std::env::var("WG_AUTOSTART") {
            let mut s = self.settings.last_world.clone();
            s.seed = worldgen::settings::seed_from_text(&seed).unwrap_or(0);
            // WG_HEIGHT=blocks picks the world height without the menu.
            if let Some(h) = std::env::var("WG_HEIGHT").ok().and_then(|h| h.trim().parse().ok()) {
                s.height = worldgen::settings::WorldHeight::from_blocks(h);
            }
            // WG_PLANET=continents|pangea|earth|islands: a realistic world;
            // WG_QUALITY=optimized|balanced|quality; WG_SIZE_KM=km.
            let pick = |var: &str, label: fn(usize) -> Option<&'static str>| {
                let v = std::env::var(var).ok()?.trim().to_lowercase();
                (0..).map_while(label).position(|l| l.to_lowercase() == v)
            };
            if let Some(i) = pick("WG_PLANET", |i| worldgen::Planet::ALL.get(i).map(|p| p.label())) {
                s.generator = worldgen::Generator::Realistic;
                s.planet = worldgen::Planet::ALL[i];
            }
            if let Some(i) = pick("WG_QUALITY", |i| worldgen::SimQuality::ALL.get(i).map(|q| q.label())) {
                s.quality = worldgen::SimQuality::ALL[i];
            }
            if let Some(km) = std::env::var("WG_SIZE_KM").ok().and_then(|v| v.trim().parse().ok()) {
                s.size = worldgen::WorldSize::from_km(km);
            }
            self.start_world(s.normalized());
            // Optional camera placement for screenshots: WG_CAMERA=x,z[,yaw_deg,pitch_deg,height]
            if let (Ok(spec), Some(sess)) = (std::env::var("WG_CAMERA"), self.session.as_mut()) {
                let v: Vec<f64> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                if v.len() >= 2 {
                    let ground = sess.chunks.generator().sample_column(v[0] as i32, v[1] as i32).water_top();
                    let above = v.get(4).copied().unwrap_or(40.0);
                    sess.camera.pos = DVec3::new(v[0], ground as f64 + above, v[1]);
                    sess.camera.yaw = v.get(2).copied().unwrap_or(0.0).to_radians() as f32;
                    sess.camera.pitch = v.get(3).copied().unwrap_or(-20.0).to_radians() as f32;
                }
            }
            if std::env::var("WG_SCREEN").is_ok_and(|v| v == "map") {
                self.set_screen(Screen::Map);
            }
        } else if let Ok(screen) = std::env::var("WG_SCREEN") {
            match screen.as_str() {
                "create" => self.set_screen(Screen::CreateWorld),
                "settings" => self.set_screen(Screen::Settings { from_pause: false }),
                _ => {}
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("World Gen")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 760.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.fatal = Some(format!("Could not open a window: {e}"));
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(&window, self.settings.vsync) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                self.fatal = Some(format!("Could not start Vulkan: {e}"));
                event_loop.exit();
                return;
            }
        }
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        ));
        self.window = Some(window);
        self.autostart();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let (Some(state), Some(window)) = (self.egui_state.as_mut(), self.window.as_ref()) {
            let _ = state.on_window_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(r) = &mut self.renderer {
                    r.resized();
                }
            }
            WindowEvent::Focused(false)
                if self.screen == Screen::Playing && std::env::var_os("WG_AUTOSTART").is_none() =>
            {
                self.set_screen(Screen::Paused)
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key: PhysicalKey::Code(code), state, repeat, .. },
                ..
            } => match state {
                ElementState::Pressed => {
                    if !repeat {
                        match (code, self.screen) {
                            (KeyCode::Escape, Screen::Playing) => self.set_screen(Screen::Paused),
                            (KeyCode::Escape, Screen::Paused) => self.set_screen(Screen::Playing),
                            (KeyCode::KeyM, Screen::Playing) => self.set_screen(Screen::Map),
                            (KeyCode::KeyM | KeyCode::Escape, Screen::Map) => self.set_screen(Screen::Playing),
                            (KeyCode::F3, _) => {
                                self.settings.show_debug = !self.settings.show_debug;
                            }
                            _ => {}
                        }
                    }
                    if self.screen == Screen::Playing {
                        self.keys.insert(code);
                    }
                }
                ElementState::Released => {
                    self.keys.remove(&code);
                }
            },
            WindowEvent::MouseWheel { delta, .. } if self.screen == Screen::Playing => {
                // Each notch doubles (wheel up) or halves (wheel down) the fly
                // speed, so going from walking pace to crossing a continent
                // takes a few flicks instead of a settings menu.
                let notches = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(p) => {
                        const PIXELS_PER_NOTCH: f64 = 60.0;
                        self.wheel_pixels += p.y;
                        let n = (self.wheel_pixels / PIXELS_PER_NOTCH).trunc();
                        self.wheel_pixels -= n * PIXELS_PER_NOTCH;
                        n as i32
                    }
                };
                if notches != 0 {
                    self.settings.fly_speed = config::wheel_speed(self.settings.fly_speed, notches);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.screen == Screen::Playing {
                if let Some(s) = &mut self.session {
                    let k = 0.0025 * self.settings.mouse_sensitivity;
                    s.camera.rotate(delta.0 as f32 * k, delta.1 as f32 * k);
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.settings.save();
        // Destroy GPU resources before the window goes away.
        self.renderer = None;
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("Could not start the windowing system: {e}");
            std::process::exit(1);
        }
    };
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new();
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("Event loop error: {e}");
    }
    if let Some(msg) = app.fatal {
        eprintln!("{msg}");
        std::process::exit(1);
    }
}

/// The world's longer side, in chunks.
fn world_chunks(w: &WorldSettings) -> i32 {
    (w.size.width_blocks().max(w.size.depth_blocks()) / 16) as i32
}

/// Height above the ground in chunks, rounded down to its 4 leading bits
/// (exact up to 15, then steps of 1/16 or less), so climbing or falling
/// changes the LOD layout every few percent of the height, not every 16
/// blocks.
fn quantize_altitude(chunks: i64) -> i32 {
    let a = chunks.clamp(0, i32::MAX as i64) as u32;
    if a < 16 {
        return a as i32;
    }
    let shift = (32 - a.leading_zeros()) - 4;
    ((a >> shift) << shift) as i32
}
