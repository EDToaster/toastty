//! `[cursor]` table.
//!
//! M4.5 stores the cursor shape + blink flag but does not yet wire them
//! through to the renderer — see TODO in the crate root for the M5+
//! plumbing pass.

use std::str::FromStr;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// Cursor shape.
///
/// Serializes as the lowercase string used in TOML
/// (`"block"`, `"bar"`, `"underline"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

impl CursorShape {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Bar => "bar",
            Self::Underline => "underline",
        }
    }
}

impl FromStr for CursorShape {
    type Err = ConfigError;

    /// Parse the canonical lowercase form.
    fn from_str(s: &str) -> Result<Self, ConfigError> {
        match s {
            "block" => Ok(Self::Block),
            "bar" => Ok(Self::Bar),
            "underline" => Ok(Self::Underline),
            other => Err(ConfigError::UnknownCursorShape(other.to_string())),
        }
    }
}

impl Serialize for CursorShape {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CursorShape {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_str(&s).map_err(DeError::custom)
    }
}

/// Cursor block shape + blink flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    pub shape: CursorShape,
    pub blink: bool,
}

impl CursorConfig {
    /// Schema defaults: block, blinking.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            shape: CursorShape::Block,
            blink: true,
        }
    }
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn cursor_defaults() {
        let c = CursorConfig::defaults();
        assert_eq!(c.shape, CursorShape::Block);
        assert!(c.blink);
    }

    #[test]
    fn cursor_default_trait() {
        assert_eq!(CursorConfig::default(), CursorConfig::defaults());
    }

    #[test]
    fn parse_each_shape() {
        for (s, want) in [
            ("block", CursorShape::Block),
            ("bar", CursorShape::Bar),
            ("underline", CursorShape::Underline),
        ] {
            assert_eq!(CursorShape::from_str(s).unwrap(), want);
        }
    }

    #[test]
    fn unknown_shape_rejected() {
        let err = CursorShape::from_str("triangle").expect_err("err");
        match err {
            ConfigError::UnknownCursorShape(s) => assert_eq!(s, "triangle"),
            other => panic!("wrong err: {other:?}"),
        }
    }

    #[test]
    fn shape_round_trip_via_toml() {
        for shape in [CursorShape::Block, CursorShape::Bar, CursorShape::Underline] {
            let c = CursorConfig {
                shape,
                blink: false,
            };
            let s = toml::to_string(&c).unwrap();
            let p: CursorConfig = toml::from_str(&s).unwrap();
            assert_eq!(p, c);
        }
    }

    #[test]
    fn unknown_shape_in_toml_rejected() {
        let res: Result<CursorConfig, _> = toml::from_str(r#"shape = "wibble""#);
        assert!(res.is_err());
    }

    #[test]
    fn unknown_key_rejected() {
        let res: Result<CursorConfig, _> = toml::from_str(
            r#"shape = "bar"
extra = 1"#,
        );
        assert!(res.is_err());
    }

    #[test]
    fn shape_serializes_as_string() {
        let v = toml::Value::try_from(CursorShape::Bar).unwrap();
        assert_eq!(v.as_str(), Some("bar"));
    }
}
