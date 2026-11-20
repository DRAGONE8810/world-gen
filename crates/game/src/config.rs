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
