//! `[theme]` table.

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::color::Color;
use crate::error::ConfigError;

/// Theme: foreground, background, cursor + the 16-entry ANSI palette
/// (8 normal + 8 bright).
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeConfig {
    pub fg: Color,
    pub bg: Color,
    pub cursor: Color,
    /// Exactly 16 entries: indexes 0..=7 are the normal colors, 8..=15
    /// the bright variants.
    pub palette: [Color; 16],
}

impl ThemeConfig {
    /// Schema defaults. Linear-light values via the `Color::from_hex`
    /// sRGB → linear path so this matches the renderer's expected blend.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            fg: hex("#d9d9d9"),
            bg: hex("#121214"),
            cursor: hex("#f2d94c"),
            palette: default_palette(),
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Internal helper: panic on bad hex. Only ever called with constant
/// strings under our control — bad hex here is a compile-time bug.
fn hex(s: &str) -> Color {
    Color::from_hex(s).expect("default theme hex is valid")
}

fn default_palette() -> [Color; 16] {
    [
        hex("#000000"), hex("#cc3030"), hex("#30cc30"), hex("#cccc30"),
        hex("#3030cc"), hex("#cc30cc"), hex("#30cccc"), hex("#cccccc"),
        hex("#666666"), hex("#ff5050"), hex("#50ff50"), hex("#ffff50"),
        hex("#5050ff"), hex("#ff50ff"), hex("#50ffff"), hex("#ffffff"),
    ]
}

// ---- serde ------------------------------------------------------------

/// Schema mirror used purely for serde. `palette` is a `Vec<Color>` here
/// so we can return a typed length error from the conversion (rather
/// than serde's stock "expected 16 elements" with no path).
#[derive(Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeSchema {
    fg: Color,
    bg: Color,
    cursor: Color,
    palette: Vec<Color>,
}

impl Default for ThemeSchema {
    fn default() -> Self {
        let t = ThemeConfig::defaults();
        Self {
            fg: t.fg,
            bg: t.bg,
            cursor: t.cursor,
            palette: t.palette.to_vec(),
        }
    }
}

impl Serialize for ThemeConfig {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let s = ThemeSchema {
            fg: self.fg,
            bg: self.bg,
            cursor: self.cursor,
            palette: self.palette.to_vec(),
        };
        s.serialize(ser)
    }
}

impl<'de> Deserialize<'de> for ThemeConfig {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = ThemeSchema::deserialize(de)?;
        ThemeConfig::from_schema(&s).map_err(DeError::custom)
    }
}

impl ThemeConfig {
    fn from_schema(s: &ThemeSchema) -> Result<Self, ConfigError> {
        let palette: [Color; 16] = s
            .palette
            .clone()
            .try_into()
            .map_err(|_| ConfigError::PaletteLength(s.palette.len()))?;
        Ok(Self {
            fg: s.fg,
            bg: s.bg,
            cursor: s.cursor,
            palette,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_defaults_have_16_palette_entries() {
        let t = ThemeConfig::defaults();
        assert_eq!(t.palette.len(), 16);
    }

    #[test]
    fn theme_defaults_via_default_trait_match() {
        assert_eq!(ThemeConfig::default(), ThemeConfig::defaults());
    }

    #[test]
    fn theme_round_trip() {
        let t = ThemeConfig::defaults();
        let s = toml::to_string(&t).unwrap();
        let p: ThemeConfig = toml::from_str(&s).unwrap();
        assert_eq!(p, t);
    }

    #[test]
    fn theme_palette_wrong_length_rejected() {
        let bad = r##"
fg = "#ffffff"
bg = "#000000"
cursor = "#ff0000"
palette = ["#000000", "#111111"]
"##;
        let res: Result<ThemeConfig, _> = toml::from_str(bad);
        let err = res.expect_err("expected length error");
        assert!(err.to_string().contains("16"), "{err}");
    }

    #[test]
    fn theme_palette_too_long_rejected() {
        let palette = std::iter::repeat_n("\"#000000\"", 17)
            .collect::<Vec<_>>()
            .join(", ");
        let bad = format!(
            r##"
fg = "#ffffff"
bg = "#000000"
cursor = "#ff0000"
palette = [{palette}]
"##
        );
        let res: Result<ThemeConfig, _> = toml::from_str(&bad);
        assert!(res.is_err());
    }

    #[test]
    fn theme_bad_color_rejected() {
        let bad = r##"
fg = "not-a-color"
bg = "#000000"
cursor = "#ff0000"
palette = []
"##;
        let res: Result<ThemeConfig, _> = toml::from_str(bad);
        assert!(res.is_err());
    }

    #[test]
    fn theme_unknown_key_rejected() {
        let bad = r##"
fg = "#ffffff"
bg = "#000000"
cursor = "#ff0000"
palette = ["#000000","#000000","#000000","#000000",
           "#000000","#000000","#000000","#000000",
           "#000000","#000000","#000000","#000000",
           "#000000","#000000","#000000","#000000"]
extra = 42
"##;
        let res: Result<ThemeConfig, _> = toml::from_str(bad);
        assert!(res.is_err());
    }
}
