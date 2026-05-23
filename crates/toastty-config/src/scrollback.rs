//! `[scrollback]` table.
//!
//! TODO(M5): wire `lines` into the term/PTY grid allocation.

use serde::{Deserialize, Serialize};

/// Scrollback config — how many lines to retain in the ring buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollbackConfig {
    pub lines: u32,
}

impl ScrollbackConfig {
    /// Schema default: 10 000 lines.
    #[must_use]
    pub fn defaults() -> Self {
        Self { lines: 10_000 }
    }
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_defaults() {
        assert_eq!(ScrollbackConfig::defaults().lines, 10_000);
    }

    #[test]
    fn scrollback_default_trait() {
        assert_eq!(ScrollbackConfig::default(), ScrollbackConfig::defaults());
    }

    #[test]
    fn scrollback_round_trip() {
        let s = ScrollbackConfig { lines: 42 };
        let t = toml::to_string(&s).unwrap();
        let p: ScrollbackConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, s);
    }

    #[test]
    fn scrollback_unknown_key_rejected() {
        let r: Result<ScrollbackConfig, _> = toml::from_str("lines = 100\nfoo = 1\n");
        assert!(r.is_err());
    }
}
