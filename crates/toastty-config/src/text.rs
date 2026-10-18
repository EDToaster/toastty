//! `[text]` table — how SGR text renditions are drawn.
//!
//! ```toml
//! [text]
//! # Animate SGR 5 / 6 (blink). When false, blinking text is drawn
//! # steadily (always visible), like xterm's `-bc`-less default or
//! # kitty's `blink_interval 0`.
//! blink = true
//! ```
//!
//! Blinking text toggles on the same 530 ms half-cycle as the cursor.
//! The renderer only schedules wake-ups while at least one blinking
//! cell is on screen, so an idle terminal with no blinking text still
//! sleeps.

use serde::{Deserialize, Serialize};

/// Text-rendition config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextConfig {
    /// Animate SGR blink. `false` renders blinking cells steadily.
    pub blink: bool,
}

impl TextConfig {
    /// Schema defaults: blink on (xterm, kitty, VTE all animate SGR 5).
    #[must_use]
    pub fn defaults() -> Self {
        Self { blink: true }
    }
}

impl Default for TextConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_blink_on() {
        assert!(TextConfig::defaults().blink);
        assert_eq!(TextConfig::default(), TextConfig::defaults());
    }

    #[test]
    fn parses_blink_false_and_rejects_unknown_keys() {
        let c: TextConfig = toml::from_str("blink = false\n").unwrap();
        assert!(!c.blink);
        let empty: TextConfig = toml::from_str("").unwrap();
        assert!(empty.blink);
        assert!(toml::from_str::<TextConfig>("blinky = 1\n").is_err());
    }
}
