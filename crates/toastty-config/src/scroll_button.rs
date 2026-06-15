//! `[scroll_button]` table.
//!
//! A small clickable "scroll to bottom" button shown in a corner of the
//! window whenever the view is scrolled up into the scrollback; clicking
//! it jumps the view back to the live bottom. Set `enabled = false` to
//! turn the button off entirely.

use serde::{Deserialize, Serialize};

/// Which corner the scroll-to-bottom button is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollButtonPosition {
    /// Bottom-right corner (the default — out of the way of most prompts).
    #[default]
    BottomRight,
    /// Bottom-left corner.
    BottomLeft,
}

/// Scroll-to-bottom button config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollButtonConfig {
    /// Master on/off switch. When `false` the button never renders and
    /// clicks in its corner fall through to normal handling.
    pub enabled: bool,
    /// Which corner to anchor the button to.
    pub position: ScrollButtonPosition,
}

impl ScrollButtonConfig {
    /// Schema defaults — enabled, bottom-right.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            enabled: true,
            position: ScrollButtonPosition::BottomRight,
        }
    }
}

impl Default for ScrollButtonConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_button_defaults() {
        let d = ScrollButtonConfig::defaults();
        assert!(d.enabled);
        assert_eq!(d.position, ScrollButtonPosition::BottomRight);
    }

    #[test]
    fn scroll_button_default_trait() {
        assert_eq!(
            ScrollButtonConfig::default(),
            ScrollButtonConfig::defaults()
        );
    }

    #[test]
    fn scroll_button_round_trip() {
        let c = ScrollButtonConfig {
            enabled: false,
            position: ScrollButtonPosition::BottomLeft,
        };
        let t = toml::to_string(&c).unwrap();
        let p: ScrollButtonConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, c);
    }

    #[test]
    fn can_be_disabled() {
        let c: ScrollButtonConfig = toml::from_str("enabled = false\n").unwrap();
        assert!(!c.enabled);
        // Other fields still default.
        assert_eq!(c.position, ScrollButtonPosition::BottomRight);
    }

    #[test]
    fn position_parses_each_variant() {
        for (s, want) in [
            ("bottom-right", ScrollButtonPosition::BottomRight),
            ("bottom-left", ScrollButtonPosition::BottomLeft),
        ] {
            let c: ScrollButtonConfig = toml::from_str(&format!("position = \"{s}\"\n")).unwrap();
            assert_eq!(c.position, want);
            // enabled defaults to true.
            assert!(c.enabled);
        }
    }

    #[test]
    fn position_serializes_to_kebab_case() {
        let t = toml::to_string(&ScrollButtonConfig {
            position: ScrollButtonPosition::BottomLeft,
            ..ScrollButtonConfig::defaults()
        })
        .unwrap();
        assert!(t.contains("position = \"bottom-left\""), "{t}");
    }

    #[test]
    fn unknown_key_rejected() {
        let r: Result<ScrollButtonConfig, _> = toml::from_str("enabled = true\nfoo = 1\n");
        assert!(r.is_err());
    }

    #[test]
    fn invalid_position_rejected() {
        let r: Result<ScrollButtonConfig, _> = toml::from_str("position = \"top-left\"\n");
        assert!(r.is_err());
    }
}
