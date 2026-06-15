//! Convert a `toastty_config::ThemeConfig` into the renderer's `Theme`.
//!
//! Lives in the binary so neither `toastty-config` nor `toastty-render`
//! has to depend on the other — keeping `toastty-config` a leaf crate
//! and `toastty-render` GPU-only. The `hello_text` demo has its own
//! copy of the same function; the binary's copy is the canonical one.

use toastty_config::{ScrollButtonConfig, ScrollButtonPosition, ThemeConfig};
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
