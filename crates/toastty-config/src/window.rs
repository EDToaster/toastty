//! `[window]` table.

use serde::{Deserialize, Serialize};

/// When to prompt for confirmation before closing the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConfirmClose {
    /// Never prompt; close immediately.
    Never,
    /// Prompt only when a non-shell program is running in the foreground
    /// (the kitty default).
    #[default]
    IfRunningProgram,
    /// Always prompt, even at a bare shell prompt.
    Always,
}

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
    /// When to prompt for confirmation before closing the window
    /// (mirrors kitty's `confirm_os_window_close`).
    pub confirm_close: ConfirmClose,
}

impl WindowConfig {
    /// Schema defaults — 1280 × 800 with vsync on, prompting on close
    /// only when a program is running.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            width: 1280,
            height: 800,
            vsync: true,
            confirm_close: ConfirmClose::IfRunningProgram,
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
        assert_eq!(d.confirm_close, ConfirmClose::IfRunningProgram);
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
            confirm_close: ConfirmClose::Always,
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

    #[test]
    fn confirm_close_defaults_to_if_running_program() {
        assert_eq!(
            WindowConfig::defaults().confirm_close,
            ConfirmClose::IfRunningProgram
        );
    }

    #[test]
    fn confirm_close_round_trips_each_variant() {
        for variant in [
            ConfirmClose::Never,
            ConfirmClose::IfRunningProgram,
            ConfirmClose::Always,
        ] {
            let w = WindowConfig {
                confirm_close: variant,
                ..WindowConfig::defaults()
            };
            let t = toml::to_string(&w).unwrap();
            let p: WindowConfig = toml::from_str(&t).unwrap();
            assert_eq!(p, w);
        }
    }

    #[test]
    fn confirm_close_serializes_to_kebab_case() {
        let t = toml::to_string(&WindowConfig {
            confirm_close: ConfirmClose::IfRunningProgram,
            ..WindowConfig::defaults()
        })
        .unwrap();
        assert!(t.contains("confirm_close = \"if-running-program\""), "{t}");
    }

    #[test]
    fn confirm_close_never_parses_and_leaves_others_default() {
        let cfg: WindowConfig = toml::from_str("confirm_close = \"never\"\n").unwrap();
        assert_eq!(cfg.confirm_close, ConfirmClose::Never);
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 800);
        assert!(cfg.vsync);
    }

    #[test]
    fn confirm_close_always_parses_and_leaves_others_default() {
        let cfg: WindowConfig = toml::from_str("confirm_close = \"always\"\n").unwrap();
        assert_eq!(cfg.confirm_close, ConfirmClose::Always);
        assert_eq!(cfg.width, 1280);
        assert_eq!(cfg.height, 800);
        assert!(cfg.vsync);
    }

    #[test]
    fn confirm_close_invalid_value_rejected() {
        let r: Result<WindowConfig, _> = toml::from_str("confirm_close = \"sometimes\"\n");
        assert!(r.is_err());
    }
}
