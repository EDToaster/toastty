//! Convert a `toastty_config::ThemeConfig` into the renderer's `Theme`.
//!
//! Lives in the binary so neither `toastty-config` nor `toastty-render`
//! has to depend on the other — keeping `toastty-config` a leaf crate
//! and `toastty-render` GPU-only. The `hello_text` demo has its own
//! copy of the same function; the binary's copy is the canonical one.

use toastty_config::ExtendBackground as CfgExtend;
use toastty_config::ExtendBackgroundWhen as CfgWhen;
use toastty_config::ExtendCondition as CfgCond;
use toastty_config::GridAlign as CfgGridAlign;
use toastty_config::{ScrollButtonConfig, ScrollButtonPosition, ThemeConfig};
use toastty_render::ExtendBackground as RExtend;
use toastty_render::ExtendBackgroundWhen as RWhen;
use toastty_render::ExtendCondition as RCond;
use toastty_render::GridAlign as RGridAlign;
use toastty_render::ScrollButtonCorner;
use toastty_render::text::instance::Theme;

/// Map the `[scroll_button]` config to the renderer's corner enum.
/// `None` when the button is disabled (`enabled = false`) — the renderer
/// treats `None` as "never paint and never hit-test".
#[must_use]
pub fn scroll_button_corner(cfg: &ScrollButtonConfig) -> Option<ScrollButtonCorner> {
    if !cfg.enabled {
        return None;
    }
    Some(match cfg.position {
        ScrollButtonPosition::BottomRight => ScrollButtonCorner::BottomRight,
        ScrollButtonPosition::BottomLeft => ScrollButtonCorner::BottomLeft,
    })
}

/// Map the `[window]` `extend_background_when` config knob (the global
/// bleed gate) to the renderer's enum. Mirrors [`scroll_button_corner`]:
/// the binary bridges the two enums so `toastty-config` stays a leaf crate
/// (no `toastty-render` dep) and `toastty-render` defines its own copy.
#[must_use]
pub fn extend_background_when(when: CfgWhen) -> RWhen {
    match when {
        CfgWhen::Never => RWhen::Never,
        CfgWhen::Always => RWhen::Always,
        CfgWhen::AltScreen => RWhen::AltScreen,
    }
}

/// Map a single per-axis [`CfgCond`] to the renderer's [`RCond`].
#[must_use]
fn extend_condition(cond: CfgCond) -> RCond {
    match cond {
        CfgCond::Never => RCond::Never,
        CfgCond::SolidLine => RCond::SolidLine,
        CfgCond::Always => RCond::Always,
    }
}

/// Map the `[window.extend_background]` per-axis config table to the
/// renderer's struct. Same leaf-crate bridging as
/// [`extend_background_when`].
#[must_use]
pub fn extend_background(ext: CfgExtend) -> RExtend {
    RExtend {
        horizontal: extend_condition(ext.horizontal),
        vertical: extend_condition(ext.vertical),
    }
}

/// Map the `[window]` `grid_align` config knob to the renderer's
/// alignment enum. Same leaf-crate bridging as [`extend_background`].
#[must_use]
pub fn grid_align(mode: CfgGridAlign) -> RGridAlign {
    match mode {
        CfgGridAlign::TopLeft => RGridAlign::TopLeft,
        CfgGridAlign::Centered => RGridAlign::Centered,
    }
}

#[must_use]
pub fn theme_from_config(cfg: &ThemeConfig) -> Theme {
    let mut palette = [[0.0; 4]; 16];
    for (i, c) in cfg.palette.iter().enumerate() {
        palette[i] = c.as_array();
    }
    Theme {
        fg: cfg.fg.as_array(),
        bg: cfg.bg.as_array(),
        cursor: cfg.cursor.as_array(),
        palette,
        // Placeholder — overwritten by `with_default_selection_bg`
        // immediately below so the tint always matches the resolved
        // fg/bg pair.
        selection_bg: [0.0; 4],
    }
    .with_default_selection_bg()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_config::Color;

    fn arrs_eq(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn scroll_button_corner_maps_position_and_disable() {
        // Disabled → None regardless of position.
        let off = ScrollButtonConfig {
            enabled: false,
            position: ScrollButtonPosition::BottomLeft,
        };
        assert_eq!(scroll_button_corner(&off), None);

        // Enabled → the matching corner.
        let right = ScrollButtonConfig {
            enabled: true,
            position: ScrollButtonPosition::BottomRight,
        };
        assert_eq!(
            scroll_button_corner(&right),
            Some(ScrollButtonCorner::BottomRight)
        );
        let left = ScrollButtonConfig {
            enabled: true,
            position: ScrollButtonPosition::BottomLeft,
        };
        assert_eq!(
            scroll_button_corner(&left),
            Some(ScrollButtonCorner::BottomLeft)
        );
    }

    #[test]
    fn extend_background_when_maps_each_variant() {
        assert_eq!(extend_background_when(CfgWhen::Never), RWhen::Never);
        assert_eq!(extend_background_when(CfgWhen::Always), RWhen::Always);
        assert_eq!(extend_background_when(CfgWhen::AltScreen), RWhen::AltScreen);
    }

    #[test]
    fn extend_condition_maps_each_variant() {
        assert_eq!(extend_condition(CfgCond::Never), RCond::Never);
        assert_eq!(extend_condition(CfgCond::SolidLine), RCond::SolidLine);
        assert_eq!(extend_condition(CfgCond::Always), RCond::Always);
    }

    #[test]
    fn extend_background_maps_both_axes() {
        let mapped = extend_background(CfgExtend {
            horizontal: CfgCond::SolidLine,
            vertical: CfgCond::Never,
        });
        assert_eq!(
            mapped,
            RExtend {
                horizontal: RCond::SolidLine,
                vertical: RCond::Never,
            }
        );
    }

    #[test]
    fn grid_align_maps_each_variant() {
        assert_eq!(grid_align(CfgGridAlign::TopLeft), RGridAlign::TopLeft);
        assert_eq!(grid_align(CfgGridAlign::Centered), RGridAlign::Centered);
    }

    #[test]
    fn defaults_round_trip_to_theme() {
        let cfg = ThemeConfig::defaults();
        let theme = theme_from_config(&cfg);
        assert!(arrs_eq(theme.fg, cfg.fg.as_array()));
        assert!(arrs_eq(theme.bg, cfg.bg.as_array()));
        assert!(arrs_eq(theme.cursor, cfg.cursor.as_array()));
        for (i, c) in cfg.palette.iter().enumerate() {
            assert!(arrs_eq(theme.palette[i], c.as_array()), "palette[{i}]");
        }
    }

    #[test]
    fn sub_one_bg_alpha_is_preserved_in_theme() {
        // A config bg with alpha < 1.0 must flow through to the Theme with
        // its alpha intact — this is what drives transparent-window mode.
        let mut cfg = ThemeConfig::defaults();
        cfg.bg = Color::from_hex("#12121480").expect("valid hex");
        assert!(cfg.bg.as_array()[3] < 1.0, "fixture bg should be sub-1.0");

        let theme = theme_from_config(&cfg);
        assert!(
            theme.bg[3] < 1.0,
            "theme bg alpha must stay < 1.0, got {}",
            theme.bg[3]
        );
        assert!(arrs_eq(theme.bg, cfg.bg.as_array()));
    }
}
