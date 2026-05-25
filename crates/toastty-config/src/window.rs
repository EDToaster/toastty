//! `[window]` table.

use serde::{Deserialize, Serialize};

/// Initial window size, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Initial window width in physical pixels.
    pub width: u32,
    /// Initial window height in physical pixels.
    pub height: u32,
}

impl WindowConfig {
    /// Schema defaults — 1280 × 800, roughly a comfortable shell window
    /// on a HiDPI display.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            width: 1280,
            height: 800,
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
        };
        let t = toml::to_string(&w).unwrap();
        let p: WindowConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, w);
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
