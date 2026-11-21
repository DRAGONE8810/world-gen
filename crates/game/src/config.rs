//! Player settings (the "Settings" menu), saved to `settings.ron` next to
//! where the game is started.

use crate::lod::Layout;
use serde::{Deserialize, Serialize};
use worldgen::WorldSettings;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// Radius of full-detail chunks around the camera.
    pub render_distance: u32,
    /// Radius, in chunks, of the LOD rings drawn beyond the render
    /// distance (0 or ≤ render distance: off).
    pub lod_distance: u32,
    /// Draw the whole world: the LOD rings reach every point of it from
    /// anywhere (overrides `lod_distance`).
    pub whole_world: bool,
    pub fov_deg: f32,
    pub mouse_sensitivity: f32,
    /// Blocks per second. The mouse wheel doubles or halves it in game.
    pub fly_speed: f32,
    pub vsync: bool,
    pub show_debug: bool,
    /// Last used world settings, so "Create world" remembers them.
    pub last_world: WorldSettings,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            render_distance: 12,
            lod_distance: 128,
            whole_world: false,
            fov_deg: 70.0,
            mouse_sensitivity: 1.0,
            fly_speed: 20.0,
            vsync: true,
            show_debug: true,
            last_world: WorldSettings::default(),
        }
    }
}

/// One-click view settings in the Settings menu. Balanced is the default
/// and holds 60 FPS on an RTX 2080; Optimized suits weaker GPUs and laptops;
/// Quality draws four times the area of full-detail chunks and the full LOD
/// range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsPreset {
    Optimized,
    Balanced,
    Quality,
}

impl GraphicsPreset {
    pub const ALL: [GraphicsPreset; 3] = [GraphicsPreset::Optimized, GraphicsPreset::Balanced, GraphicsPreset::Quality];

    pub fn label(self) -> &'static str {
        match self {
            GraphicsPreset::Optimized => "Optimized",
            GraphicsPreset::Balanced => "Balanced",
            GraphicsPreset::Quality => "Quality",
        }
    }

    /// (render distance, LOD distance in chunks, whole world).
    pub fn distances(self) -> (u32, u32, bool) {
        match self {
            GraphicsPreset::Optimized => (8, 64, false),
            GraphicsPreset::Balanced => (12, 128, false),
            GraphicsPreset::Quality => (24, 256, true),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            GraphicsPreset::Optimized => "Shorter view for weaker GPUs and laptops.",
            GraphicsPreset::Balanced => "The default, aimed at 60 FPS on an RTX 2080.",
            GraphicsPreset::Quality => "Twice the full-detail radius, and the far view reaches the whole world.",
        }
    }

    /// The preset these settings match, if any.
    pub fn of(s: &GameSettings) -> Option<GraphicsPreset> {
        Self::ALL.into_iter().find(|p| p.distances() == (s.render_distance, s.lod_distance, s.whole_world))
    }

    pub fn apply(self, s: &mut GameSettings) {
        (s.render_distance, s.lod_distance, s.whole_world) = self.distances();
    }
}

/// Full-detail radius range, in chunks.
pub const MAX_RENDER_DISTANCE: u32 = 64;
/// LOD radius range: up to 262,144 chunks (4,194,304 blocks, a 8,400 km
/// wide view). Detail halves each time the distance doubles, so this is
/// only ~10 more rings of tiles than 256 chunks (see `lod.rs`).
pub const MAX_LOD_DISTANCE: u32 = 262_144;

/// Fly speed range. The top end (2^20 ≈ 1 million blocks/s) crosses the
/// full 60,000 km world in about a minute; chunk generation can't keep up at
/// that speed, but it's handy to jump far away.
pub const MIN_FLY_SPEED: f32 = 0.25;
pub const MAX_FLY_SPEED: f32 = 1_048_576.0;

/// One mouse-wheel notch doubles (up) or halves (down) the speed.
pub fn wheel_speed(speed: f32, notches: i32) -> f32 {
    (speed * 2f32.powi(notches)).clamp(MIN_FLY_SPEED, MAX_FLY_SPEED)
}

const PATH: &str = "settings.ron";

impl GameSettings {
    pub fn load() -> Self {
        let mut s: Self = std::fs::read_to_string(PATH).ok().and_then(|s| ron::from_str(&s).ok()).unwrap_or_default();
        s.render_distance = s.render_distance.clamp(2, MAX_RENDER_DISTANCE);
        s.lod_distance = s.lod_distance.min(MAX_LOD_DISTANCE);
        s
    }

    /// The view distances for the streamer in a world whose terrain is
    /// magnified `terrain_scale` times (tall worlds) and is `world_chunks`
    /// chunks across (its longer side), with the camera `altitude` chunks
    /// above the ground. The LOD distance grows with the terrain, so a
    /// 17,696-block world shows the same mountains as a normal one, 46 ×
    /// further away. That costs only a few more rings (one per doubling of
    /// the distance), not 46² times the tiles; so does the whole world.
    pub fn layout(&self, terrain_scale: f64, world_chunks: i32, altitude: i32) -> Layout {
        let detail = self.render_distance as i32;
        let lod = if self.whole_world {
            // From a corner, the opposite corner is one world width away.
            world_chunks.max(detail + 1)
        } else if self.lod_distance > self.render_distance {
            // Never further than the whole world needs.
            let scaled = (self.lod_distance as f64 * terrain_scale.max(1.0)).round() as i32;
            scaled.min(world_chunks.max(detail + 1))
        } else {
            0
        };
        Layout { detail, lod, altitude: altitude.max(0) }
    }

    /// How far the view reaches, in blocks (where the fog ends).
    pub fn view_distance_blocks(&self, terrain_scale: f64, world_chunks: i32) -> f32 {
        (self.layout(terrain_scale, world_chunks, 0).lod.max(self.render_distance as i32) as f32) * 16.0
    }

    pub fn save(&self) {
        let pretty = ron::ser::PrettyConfig::default();
        if let Ok(text) = ron::ser::to_string_pretty(self, pretty) {
            if let Err(e) = std::fs::write(PATH, text) {
                log::warn!("could not save settings: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_the_balanced_preset() {
        let mut s = GameSettings::default();
        assert_eq!(GraphicsPreset::of(&s), Some(GraphicsPreset::Balanced));
        for p in GraphicsPreset::ALL {
            p.apply(&mut s);
            assert_eq!(GraphicsPreset::of(&s), Some(p));
            assert!(s.render_distance <= MAX_RENDER_DISTANCE && s.lod_distance <= MAX_LOD_DISTANCE);
        }
    }

    #[test]
    fn whole_world_reaches_across_the_world() {
        let mut s = GameSettings::default();
        assert_eq!(s.layout(1.0, 3_749_998, 0).lod, 128);
        assert_eq!(s.layout(46.0, 3_749_998, 0).lod, 128 * 46);
        s.whole_world = true;
        let l = s.layout(1.0, 3_749_998, 7);
        assert_eq!((l.lod, l.altitude), (3_749_998, 7));
        // The longest LOD distance, magnified by a tall world, stops at the world's width.
        s.whole_world = false;
        s.lod_distance = MAX_LOD_DISTANCE;
        assert_eq!(s.layout(46.0, 3_749_998, 0).lod, 3_749_998);
        assert_eq!(s.layout(1.0, 3_749_998, 0).lod, 262_144);
        s.whole_world = true;
        // A 1 km world still gets LOD rings past the render distance.
        assert!(s.layout(1.0, 63, 0).lod_enabled());
        // Old settings files without the field load with it off.
        let old: GameSettings = ron::from_str("(render_distance: 16, lod_distance: 64)").unwrap();
        assert!(!old.whole_world);
    }

    #[test]
    fn wheel_doubles_and_halves_within_limits() {
        assert_eq!(wheel_speed(20.0, 1), 40.0);
        assert_eq!(wheel_speed(20.0, -1), 10.0);
        assert_eq!(wheel_speed(20.0, 3), 160.0);
        assert_eq!(wheel_speed(20.0, 40), MAX_FLY_SPEED);
        assert_eq!(wheel_speed(20.0, -40), MIN_FLY_SPEED);
        // Far above the old 500 blocks/s cap.
        assert!(wheel_speed(500.0, 5) > 10_000.0);
    }
}
