//! `[font]` table.

use serde::{Deserialize, Serialize};

/// Font config — what the renderer plugs into cosmic-text + the
/// `Metrics::new(size, size * line_height)` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Family name forwarded to cosmic-text. The bundled fallback is
    /// `"Fira Mono"`.
    pub family: String,
    /// Font size in pixels.
    pub size_px: f32,
    /// Line-height multiplier (× `size_px`).
    pub line_height: f32,
}

impl FontConfig {
    /// Schema defaults: Fira Mono, 16 px, 1.20× line height.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            family: "Fira Mono".to_string(),
            size_px: 16.0,
            line_height: 1.20,
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_defaults_match_documented_schema() {
        let f = FontConfig::defaults();
        assert_eq!(f.family, "Fira Mono");
        assert!((f.size_px - 16.0).abs() < 1e-6);
        assert!((f.line_height - 1.20).abs() < 1e-6);
    }

    #[test]
    fn font_default_trait() {
        assert_eq!(FontConfig::default(), FontConfig::defaults());
    }

    #[test]
    fn font_partial_parse_keeps_defaults() {
        let f: FontConfig = toml::from_str("size_px = 24.0\n").expect("parse");
        assert!((f.size_px - 24.0).abs() < 1e-6);
        assert_eq!(f.family, "Fira Mono");
        assert!((f.line_height - 1.20).abs() < 1e-6);
    }

    #[test]
    fn font_round_trip() {
        let f = FontConfig {
            family: "Iosevka".into(),
            size_px: 14.5,
            line_height: 1.3,
        };
        let s = toml::to_string(&f).unwrap();
        let p: FontConfig = toml::from_str(&s).unwrap();
        assert_eq!(p, f);
    }

    #[test]
    fn font_unknown_key_rejected() {
        let r: Result<FontConfig, _> = toml::from_str("bogus = 1\n");
        assert!(r.is_err());
    }
}
