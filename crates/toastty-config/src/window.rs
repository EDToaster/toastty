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

/// Global gate for edge-cell background extension (overscan/bleed): when
/// the feature is active *at all*. The per-axis [`ExtendBackground`] rule
/// then decides which edge rows/columns actually bleed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExtendBackgroundWhen {
    /// Never bleed — the padding always shows the base theme background.
    #[default]
    Never,
    /// Bleed whenever the per-axis rule allows.
    Always,
    /// Bleed only while the alternate screen is active (serializes as
    /// `"alt-screen"`) — full-page TUIs.
    AltScreen,
}

/// Per-axis rule for *whether* a given edge row/column bleeds, once
/// [`ExtendBackgroundWhen`] has gated the feature on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExtendCondition {
    /// Never extend along this axis.
    Never,
    /// Extend only when the whole edge row (horizontal axis) or column
    /// (vertical axis) is painted with a non-default background — the
    /// "solid band" case (powerlines, full-width prompt/status lines)
    /// where the band color is a good padding color. Serializes as
    /// `"solid-line"`.
    SolidLine,
    /// Always extend along this axis.
    #[default]
    Always,
}

/// Per-axis edge-background extension rule. Combined with
/// [`ExtendBackgroundWhen`]: an edge cell bleeds along an axis iff the
/// `when` gate is active **and** that axis's condition is met.
///
/// - `horizontal` controls the **left/right** gutters, decided per-row
///   (a row's edge cells bleed left/right).
/// - `vertical` controls the **top/bottom** gutters, decided per-column
///   (a column's edge cells bleed up/down).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtendBackground {
    /// Left/right gutters (per-row).
    pub horizontal: ExtendCondition,
    /// Top/bottom gutters (per-column).
    pub vertical: ExtendCondition,
}

impl ExtendBackground {
    /// Schema defaults — bleed along both axes whenever the `when` gate
    /// is active (so flipping only `extend_background_when` reproduces the
    /// old "bleed every edge" behavior).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            horizontal: ExtendCondition::Always,
            vertical: ExtendCondition::Always,
        }
    }
}

impl Default for ExtendBackground {
    fn default() -> Self {
        Self::defaults()
    }
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
    /// Global gate for edge-cell background extension (overscan/bleed).
    pub extend_background_when: ExtendBackgroundWhen,
    /// How to align the cell grid when it doesn't exactly fill the
    /// content area (partial trailing row/column).
    pub grid_align: GridAlign,
    /// Per-axis edge-background extension rule. A table — declared before
    /// `padding` but after every scalar so the `[window.extend_background]`
    /// / `[window.padding]` sub-tables and TOML field ordering round-trip.
    pub extend_background: ExtendBackground,
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
            extend_background_when: ExtendBackgroundWhen::Never,
            grid_align: GridAlign::TopLeft,
            extend_background: ExtendBackground::defaults(),
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
            extend_background_when: ExtendBackgroundWhen::AltScreen,
            grid_align: GridAlign::Centered,
            extend_background: ExtendBackground {
                horizontal: ExtendCondition::SolidLine,
                vertical: ExtendCondition::Never,
            },
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
    fn extend_background_when_defaults_to_never() {
        assert_eq!(
            WindowConfig::defaults().extend_background_when,
            ExtendBackgroundWhen::Never
        );
    }

    #[test]
    fn extend_background_defaults_to_both_always() {
        let d = WindowConfig::defaults().extend_background;
        assert_eq!(d.horizontal, ExtendCondition::Always);
        assert_eq!(d.vertical, ExtendCondition::Always);
    }

    #[test]
    fn extend_background_when_round_trips_each_variant() {
        for variant in [
            ExtendBackgroundWhen::Never,
            ExtendBackgroundWhen::Always,
            ExtendBackgroundWhen::AltScreen,
        ] {
            let w = WindowConfig {
                extend_background_when: variant,
                ..WindowConfig::defaults()
            };
            let t = toml::to_string(&w).unwrap();
            let p: WindowConfig = toml::from_str(&t).unwrap();
            assert_eq!(p, w);
        }
    }

    #[test]
    fn extend_background_when_serializes_alt_screen_kebab() {
        let t = toml::to_string(&WindowConfig {
            extend_background_when: ExtendBackgroundWhen::AltScreen,
            ..WindowConfig::defaults()
        })
        .unwrap();
        assert!(t.contains("extend_background_when = \"alt-screen\""), "{t}");
    }

    #[test]
    fn extend_background_subtable_parses_and_partial_fills_default() {
        // Only `horizontal` given → `vertical` keeps its `Always` default.
        let cfg: WindowConfig =
            toml::from_str("[extend_background]\nhorizontal = \"solid-line\"\n").unwrap();
        assert_eq!(cfg.extend_background.horizontal, ExtendCondition::SolidLine);
        assert_eq!(cfg.extend_background.vertical, ExtendCondition::Always);
        // Untouched scalar fields keep their defaults.
        assert_eq!(cfg.width, 1280);
    }

    #[test]
    fn extend_condition_serializes_solid_line_kebab() {
        let t = toml::to_string(&WindowConfig {
            extend_background: ExtendBackground {
                horizontal: ExtendCondition::SolidLine,
                vertical: ExtendCondition::Never,
            },
            ..WindowConfig::defaults()
        })
        .unwrap();
        assert!(t.contains("horizontal = \"solid-line\""), "{t}");
        assert!(t.contains("vertical = \"never\""), "{t}");
    }

    #[test]
    fn extend_condition_invalid_value_rejected() {
        let r: Result<WindowConfig, _> =
            toml::from_str("[extend_background]\nhorizontal = \"sometimes\"\n");
        assert!(r.is_err());
    }

    #[test]
    fn extend_background_unknown_key_rejected() {
        let r: Result<WindowConfig, _> =
            toml::from_str("[extend_background]\ndiagonal = \"always\"\n");
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
