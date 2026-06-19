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

/// When to paint each edge cell's background outward into the window
/// padding (overscan/bleed), so a full-page TUI's colors reach the
/// physical window edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExtendBackground {
    /// Never bleed — the padding always shows the base theme background.
    #[default]
    Never,
    /// Always bleed the edge cells into the padding.
    Always,
    /// Bleed only while the alternate screen is active (serializes as
    /// `"alt-screen"`).
    AltScreen,
}

/// How to align the cell grid within the content area when the window
/// isn't an exact multiple of the cell size (so the floor-divided grid
/// leaves a sub-cell sliver on the right/bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GridAlign {
    /// Pin the grid to the top-left; the leftover sliver sits on the
    /// right and bottom edges (serializes as `"top-left"`).
    #[default]
    TopLeft,
    /// Center the grid; the leftover is split evenly across opposite
    /// edges.
    Centered,
}

/// Window padding (cell-grid inset) in logical pixels, per side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaddingConfig {
    /// Top inset (logical px).
    pub top: u16,
    /// Right inset (logical px).
    pub right: u16,
    /// Bottom inset (logical px).
    pub bottom: u16,
    /// Left inset (logical px).
    pub left: u16,
}

impl PaddingConfig {
    /// Schema defaults — zero padding on every side.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            top: 0,
            right: 0,
            bottom: 0,
            left: 0,
        }
    }
}

impl Default for PaddingConfig {
    fn default() -> Self {
        Self::defaults()
    }
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
    /// When to bleed each edge cell's background into the padding.
    pub extend_background: ExtendBackground,
    /// How to align the cell grid when it doesn't exactly fill the
    /// content area (partial trailing row/column).
    pub grid_align: GridAlign,
    /// Cell-grid inset (logical px) per side. Declared LAST so a partial
    /// `[window.padding]` sub-table and TOML field ordering round-trip.
    pub padding: PaddingConfig,
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
            extend_background: ExtendBackground::Never,
            grid_align: GridAlign::TopLeft,
            padding: PaddingConfig::defaults(),
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
            extend_background: ExtendBackground::Always,
            grid_align: GridAlign::Centered,
            padding: PaddingConfig {
                top: 1,
                right: 2,
                bottom: 3,
                left: 4,
            },
        };
        let t = toml::to_string(&w).unwrap();
        let p: WindowConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, w);
    }

    #[test]
    fn grid_align_defaults_to_top_left() {
        assert_eq!(WindowConfig::defaults().grid_align, GridAlign::TopLeft);
    }

    #[test]
    fn grid_align_parses_and_serializes_kebab_case() {
        let cfg: WindowConfig = toml::from_str("grid_align = \"centered\"\n").unwrap();
        assert_eq!(cfg.grid_align, GridAlign::Centered);
        // Untouched fields keep their defaults.
        assert_eq!(cfg.width, 1280);

        let t = toml::to_string(&WindowConfig {
            grid_align: GridAlign::TopLeft,
            ..WindowConfig::defaults()
        })
        .unwrap();
        assert!(t.contains("grid_align = \"top-left\""), "{t}");
    }

    #[test]
    fn grid_align_invalid_value_rejected() {
        let r: Result<WindowConfig, _> = toml::from_str("grid_align = \"middle\"\n");
        assert!(r.is_err());
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
