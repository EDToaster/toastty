//! Convert a `toastty_config::ThemeConfig` into the renderer's `Theme`.
//!
//! Lives in the binary so neither `toastty-config` nor `toastty-render`
//! has to depend on the other — keeping `toastty-config` a leaf crate
//! and `toastty-render` GPU-only. The `hello_text` demo has its own
//! copy of the same function; the binary's copy is the canonical one.

use toastty_config::ThemeConfig;
use toastty_render::text::instance::Theme;

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrs_eq(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-6)
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
}
