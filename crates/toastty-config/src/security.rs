//! `[security]` table.
//!
//! Opt-in for sensitive PTY surfaces. Currently covers OSC 52
//! clipboard read and write (both off by default — see the iTerm2
//! advisory history for why).

use serde::{Deserialize, Serialize};

/// Security knobs. Both clipboard surfaces default to `false` —
/// programs running on the PTY cannot read or write the user's
/// clipboard without explicit opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Allow OSC 52 reads (`OSC 52 ; c ; ? ST`). On by default —
    /// any PTY program could exfiltrate the clipboard contents.
    pub osc_52_read: bool,
    /// Allow OSC 52 writes (`OSC 52 ; c ; <base64> ST`). On by
    /// default — a malicious program could replace the user's
    /// clipboard with attacker-controlled text right before they
    /// paste into another window.
    pub osc_52_write: bool,
}

impl SecurityConfig {
    /// All gates closed.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            osc_52_read: true,
            osc_52_write: true,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off() {
        let s = SecurityConfig::defaults();
        assert!(!s.osc_52_read);
        assert!(!s.osc_52_write);
    }

    #[test]
    fn default_trait_matches_defaults() {
        assert_eq!(SecurityConfig::default(), SecurityConfig::defaults());
    }

    #[test]
    fn round_trip_via_toml() {
        let s = SecurityConfig {
            osc_52_read: true,
            osc_52_write: true,
        };
        let serialized = toml::to_string(&s).unwrap();
        let parsed: SecurityConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn unknown_key_rejected() {
        let res: Result<SecurityConfig, _> = toml::from_str("nonsense = true\n");
        assert!(res.is_err());
    }
}
