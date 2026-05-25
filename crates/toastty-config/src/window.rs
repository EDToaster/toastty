//! `[window]` table.

use serde::{Deserialize, Serialize};

/// Window-presentation config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Initial window width in physical pixels.
    pub width: u32,
    /// Initial window height in physical pixels.
    pub height: u32,
    /// Sync presentation to the display refresh. `true` (default) uses
    /// the platform's vsync (wgpu `AutoVsync` / `Fifo`) — smooth, power-
    /// friendly, may add ~1 frame of latency. `false` selects
    /// `AutoNoVsync` (Immediate / Mailbox where available) — minimum
    /// latency at the cost of possible tearing and higher GPU usage.
    pub vsync: bool,
}

impl WindowConfig {
    /// Schema defaults — 1280 × 800 with vsync on.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            width: 1280,
            height: 800,
            vsync: true,
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_defaults() {
        let d = WindowConfig::defaults();
        assert_eq!(d.width, 1280);
        assert_eq!(d.height, 800);
        assert!(d.vsync);
    }

    #[test]
    fn window_default_trait() {
        assert_eq!(WindowConfig::default(), WindowConfig::defaults());
    }

    #[test]
    fn window_round_trip() {
        let w = WindowConfig {
            width: 1920,
            height: 1080,
            vsync: false,
        };
        let t = toml::to_string(&w).unwrap();
        let p: WindowConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, w);
    }

    #[test]
    fn vsync_can_be_disabled() {
        let cfg: WindowConfig = toml::from_str("vsync = false\n").unwrap();
        assert!(!cfg.vsync);
        // Other fields default.
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 800);
    }

    #[test]
    fn window_unknown_key_rejected() {
        let r: Result<WindowConfig, _> = toml::from_str("width = 800\nfoo = 1\n");
        assert!(r.is_err());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: WindowConfig = toml::from_str("width = 1600\n").unwrap();
        assert_eq!(cfg.width, 1600);
        assert_eq!(cfg.height, 800);
    }
}
