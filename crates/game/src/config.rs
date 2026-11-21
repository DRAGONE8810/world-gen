//! Player settings (the "Settings" menu), saved to `settings.ron` next to
//! where the game is started.

use serde::{Deserialize, Serialize};
use worldgen::WorldSettings;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameSettings {
    /// Radius of loaded chunks around the camera.
    pub render_distance: u32,
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
            fov_deg: 70.0,
            mouse_sensitivity: 1.0,
            fly_speed: 20.0,
            vsync: true,
            show_debug: true,
            last_world: WorldSettings::default(),
        }
    }
}

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
        std::fs::read_to_string(PATH).ok().and_then(|s| ron::from_str(&s).ok()).unwrap_or_default()
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
