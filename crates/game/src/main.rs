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
mod mesh;
mod render;
mod ui;
mod world;

use camera::Camera;
use config::GameSettings;
use glam::DVec3;
use render::{Renderer, UiFrame};
use std::collections::HashSet;
use std::sync::Arc;
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
    last_draw: (usize, u64),
    /// Screenshot-test mode: quit after this many frames.
    exit_after_frames: Option<u64>,
    frames: u64,
    fatal: Option<String>,
}

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
            last_draw: (0, 0),
            exit_after_frames,
            frames: 0,
            fatal: None,
        }
    }

    fn start_world(&mut self, settings: WorldSettings) {
        log::info!("Starting world {settings:?}");
        self.settings.last_world = settings.clone();
        self.settings.save();
        let gen = WorldGenerator::new(settings.clone());
        let (x, y, z) = gen.find_spawn();
        let mut camera = Camera::new(DVec3::new(x as f64 + 0.5, y as f64 + 24.0, z as f64 + 0.5));
        camera.fov_y_deg = self.settings.fov_deg;
        if let Some(r) = &mut self.renderer {
            r.clear_meshes();
        }
        self.session = Some(Session { settings, chunks: ChunkManager::new(gen), camera });
        self.set_screen(Screen::Playing);
    }

    fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
        self.keys.clear();
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
            Action::StartWorld(s) => self.start_world(s),
            Action::Goto(s) => self.set_screen(s),
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
            let h = s.settings.size.half_width() as f64;
            s.camera.pos.x = s.camera.pos.x.clamp(-h + 0.5, h - 0.5);
            s.camera.pos.z = s.camera.pos.z.clamp(-h + 0.5, h - 0.5);
            let (lo, hi) = (s.settings.height.min_y() as f64, s.settings.height.max_y() as f64);
            s.camera.pos.y = s.camera.pos.y.clamp(lo, hi + 200.0);
        }

        let center = ChunkPos::from_block(s.camera.pos.x.floor() as i32, s.camera.pos.z.floor() as i32);
        let events = s.chunks.update(center, self.settings.layout(s.settings.height.terrain_scale()));
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
        let output = ctx.run_ui(raw, |ui| match self.screen {
            Screen::MainMenu => ui::main_menu(ui, &mut actions),
            Screen::CreateWorld => ui::create_world(ui, &mut self.form, &mut actions),
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
                            column: s
                                .chunks
                                .generator()
                                .sample_column(s.camera.pos.x.floor() as i32, s.camera.pos.z.floor() as i32),
                            stats: s.chunks.stats,
                            drawn_chunks: self.last_draw.0,
                            drawn_triangles: self.last_draw.1,
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
        });
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
                (&s.camera, self.settings.view_distance_blocks(s.settings.height.terrain_scale()), s.chunks.draw_list())
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
            self.last_draw = (stats.drawn_chunks, stats.drawn_triangles);
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
    /// jumps straight into a world, `WG_SCREEN=create|settings` opens a menu.
    fn autostart(&mut self) {
        if let Ok(seed) = std::env::var("WG_AUTOSTART") {
            let mut s = self.settings.last_world.clone();
            s.seed = worldgen::settings::seed_from_text(&seed).unwrap_or(0);
            // WG_HEIGHT=blocks picks the world height without the menu.
            if let Some(h) = std::env::var("WG_HEIGHT").ok().and_then(|h| h.trim().parse().ok()) {
                s.height = worldgen::settings::WorldHeight::from_blocks(h);
            }
            self.start_world(s);
            // Optional camera placement for screenshots: WG_CAMERA=x,z[,yaw_deg,pitch_deg,height]
            if let (Ok(spec), Some(sess)) = (std::env::var("WG_CAMERA"), self.session.as_mut()) {
                let v: Vec<f64> = spec.split(',').filter_map(|p| p.trim().parse().ok()).collect();
                if v.len() >= 2 {
                    let ground = sess.chunks.generator().sample_column(v[0] as i32, v[1] as i32).surface_y;
                    let above = v.get(4).copied().unwrap_or(40.0);
                    sess.camera.pos = DVec3::new(v[0], ground.max(worldgen::SEA_LEVEL) as f64 + above, v[1]);
                    sess.camera.yaw = v.get(2).copied().unwrap_or(0.0).to_radians() as f32;
                    sess.camera.pitch = v.get(3).copied().unwrap_or(-20.0).to_radians() as f32;
                }
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
