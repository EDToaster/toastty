//! Color parsing + sRGB → linear conversion.
//!
//! The TOML representation is an `#rrggbb` or `#rrggbbaa` string.
//! In-memory we store linear-light `[f32; 4]` so the value can flow
//! straight to wgpu / shader uniforms (which expect linear, since the
//! swapchain encodes back to sRGB at write time).
//!
//! The sRGB → linear gamma curve mirrors the inverse of
//! `toastty-render::color::linear_to_srgb_u8`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// A linear-light RGBA color in `[0, 1]`.
///
/// Use [`Color::from_hex`] to parse a `#rrggbb[aa]` string. Serializes as
/// the same hex string so config files round-trip readably.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color(pub [f32; 4]);

impl Color {
    /// Parse a `#rrggbb` or `#rrggbbaa` (case-insensitive) sRGB string
    /// into a linear-light RGBA value.
    #[allow(clippy::many_single_char_names)] // r/g/b/a are the standard names
    pub fn from_hex(s: &str) -> Result<Self, ConfigError> {
        let Some(rest) = s.strip_prefix('#') else {
            return Err(ConfigError::InvalidColor {
                input: s.to_string(),
                reason: "missing leading '#'",
            });
        };
        let (r, g, b, a) = match rest.len() {
            6 => (
                parse_byte(s, &rest[0..2])?,
                parse_byte(s, &rest[2..4])?,
                parse_byte(s, &rest[4..6])?,
                255u8,
            ),
            8 => (
                parse_byte(s, &rest[0..2])?,
                parse_byte(s, &rest[2..4])?,
                parse_byte(s, &rest[4..6])?,
                parse_byte(s, &rest[6..8])?,
            ),
            _ => {
                return Err(ConfigError::InvalidColor {
                    input: s.to_string(),
                    reason: "expected #rrggbb or #rrggbbaa",
                });
            }
        };
        Ok(Self([
            srgb_u8_to_linear(r),
            srgb_u8_to_linear(g),
            srgb_u8_to_linear(b),
            // Alpha is linear (no gamma curve), matching wgpu's behavior.
            f32::from(a) / 255.0,
        ]))
    }

    /// Render to `#rrggbb` (alpha = 1) or `#rrggbbaa` (alpha < 1).
    /// Round-trips through `from_hex`.
    #[must_use]
    #[allow(clippy::many_single_char_names)] // r/g/b/a are the standard names
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        let r = linear_to_srgb_u8(r);
        let g = linear_to_srgb_u8(g);
        let b = linear_to_srgb_u8(b);
        // Alpha is linear → byte; the round-trip tolerates ±1 due to the
        // quantization, which is fine for a config file.
        let a8 = ((a.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
        if a8 == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a8:02x}")
        }
    }

    /// Convenience: the raw linear `[f32; 4]` array.
    #[must_use]
    pub fn as_array(self) -> [f32; 4] {
        self.0
    }
}

fn parse_byte(orig: &str, hex: &str) -> Result<u8, ConfigError> {
    u8::from_str_radix(hex, 16).map_err(|_| ConfigError::InvalidColor {
        input: orig.to_string(),
        reason: "non-hex digit in color",
    })
}

/// sRGB byte (0..=255) → linear-light `[0, 1]`. Inverse of
/// `toastty-render::color::linear_to_srgb_u8`.
#[must_use]
pub(crate) fn srgb_u8_to_linear(byte: u8) -> f32 {
    let s = f32::from(byte) / 255.0;
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear-light `[0, 1]` → sRGB byte. Same curve as the renderer's
/// helper; duplicated here so this crate stays leaf-pure.
#[must_use]
pub(crate) fn linear_to_srgb_u8(linear: f32) -> u8 {
    let l = linear.clamp(0.0, 1.0);
    let enc = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (enc * 255.0 + 0.5) as u8
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::items_after_statements)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn pure_red_round_trips_to_linear_red() {
        let c = Color::from_hex("#ff0000").expect("parse");
        assert!(approx(c.0[0], 1.0));
        assert!(approx(c.0[1], 0.0));
        assert!(approx(c.0[2], 0.0));
        assert_eq!(c.0[3], 1.0);
    }

    #[test]
    fn pure_white_round_trips() {
        let c = Color::from_hex("#ffffff").expect("parse");
        assert!(approx(c.0[0], 1.0));
        assert!(approx(c.0[1], 1.0));
        assert!(approx(c.0[2], 1.0));
    }

    #[test]
    fn pure_black_round_trips() {
        let c = Color::from_hex("#000000").expect("parse");
        assert_eq!(c.0[0], 0.0);
        assert_eq!(c.0[1], 0.0);
        assert_eq!(c.0[2], 0.0);
    }

    #[test]
    fn srgb_mid_gray_to_linear_matches_curve() {
        // 0x80 = 128 → linear ~0.2159
        let c = Color::from_hex("#808080").expect("parse");
        assert!((c.0[0] - 0.2159).abs() < 1e-3);
    }

    #[test]
    fn alpha_is_parsed_as_linear() {
        // #ffffff80 → alpha 0x80 = 128 → linear 0.5019..
        let c = Color::from_hex("#ffffff80").expect("parse");
        assert!((c.0[3] - 0.5019).abs() < 1e-3);
    }

    #[test]
    fn case_insensitive_hex() {
        let lower = Color::from_hex("#aabbcc").expect("lower");
        let upper = Color::from_hex("#AABBCC").expect("upper");
        assert_eq!(lower, upper);
    }

    #[test]
    fn missing_hash_is_rejected() {
        let err = Color::from_hex("ff0000").expect_err("must fail");
        match err {
            ConfigError::InvalidColor { reason, .. } => {
                assert!(reason.contains("missing"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn wrong_length_is_rejected() {
        let err = Color::from_hex("#12345").expect_err("too short");
        assert!(matches!(err, ConfigError::InvalidColor { .. }));
        let err = Color::from_hex("#1234567").expect_err("seven");
        assert!(matches!(err, ConfigError::InvalidColor { .. }));
        let err = Color::from_hex("#").expect_err("empty");
        assert!(matches!(err, ConfigError::InvalidColor { .. }));
    }

    #[test]
    fn non_hex_chars_rejected() {
        let err = Color::from_hex("#zzzzzz").expect_err("zzz not hex");
        match err {
            ConfigError::InvalidColor { reason, .. } => {
                assert!(reason.contains("non-hex"));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn to_hex_round_trip() {
        for input in ["#000000", "#ffffff", "#123456", "#aabbcc"] {
            let c = Color::from_hex(input).unwrap();
            let s = c.to_hex();
            let c2 = Color::from_hex(&s).unwrap();
            // sRGB byte round-trip is exact; linear round-trip via the
            // gamma curve loses < 1 byte of precision — assert byte
            // equality.
            assert_eq!(s, input, "{input} -> {s}");
            let _ = c2;
        }
    }

    #[test]
    fn to_hex_emits_8_chars_when_alpha_lt_1() {
        let c = Color([0.0, 0.0, 0.0, 0.5]);
        let s = c.to_hex();
        assert_eq!(s.len(), 9, "{s}");
        assert!(s.starts_with('#'));
    }

    #[test]
    fn to_hex_emits_6_chars_when_alpha_is_1() {
        let c = Color([0.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.to_hex(), "#000000");
    }

    #[test]
    fn linear_to_srgb_clamps() {
        assert_eq!(linear_to_srgb_u8(-1.0), 0);
        assert_eq!(linear_to_srgb_u8(10.0), 255);
    }

    #[test]
    fn srgb_to_linear_zero_one() {
        assert!(approx(srgb_u8_to_linear(0), 0.0));
        assert!(approx(srgb_u8_to_linear(255), 1.0));
    }

    #[test]
    fn serde_round_trip_via_toml() {
        let c = Color::from_hex("#abcdef").unwrap();
        let wrapped = toml::Value::String(c.to_hex());
        let s = toml::to_string(&toml::Value::Table(
            [("c".to_string(), wrapped)].into_iter().collect(),
        ))
        .unwrap();
        #[derive(serde::Deserialize)]
        struct Holder {
            c: Color,
        }
        let parsed: Holder = toml::from_str(&s).unwrap();
        assert_eq!(parsed.c, c);
    }

    #[test]
    fn serde_rejects_bad_hex_via_deserialize() {
        #[derive(serde::Deserialize)]
        struct Holder {
            #[allow(dead_code)]
            c: Color,
        }
        let res: Result<Holder, _> = toml::from_str(r##"c = "#zzzzzz""##);
        assert!(res.is_err());
    }

    #[test]
    fn as_array_returns_inner() {
        let c = Color([0.1, 0.2, 0.3, 0.4]);
        assert_eq!(c.as_array(), [0.1, 0.2, 0.3, 0.4]);
    }
}
