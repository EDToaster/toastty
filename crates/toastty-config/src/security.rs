//! `[security]` table.
//!
//! Gates for sensitive PTY surfaces: OSC 52 clipboard read and write,
//! and OSC 7700 Markdown rich blocks (rendering, local image files). Both are on by default (so tmux, neovim
//! and remote shells can use the clipboard out of the box); users who
//! don't trust what runs in the terminal can turn them off — see the
//! iTerm2 advisory history for the risks.

use serde::{Deserialize, Serialize};

/// Security knobs. Both clipboard surfaces default to `true`; set
/// either to `false` to stop programs running on the PTY from reading
/// or writing the user's clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct SecurityConfig {
    /// Allow OSC 52 reads (`OSC 52 ; c ; ? ST`). On by default. Risk
    /// when on: any PTY program can read (and exfiltrate) the
    /// clipboard contents.
    pub osc_52_read: bool,
    /// Allow OSC 52 writes (`OSC 52 ; c ; <base64> ST`). On by
    /// default. Risk when on: a malicious program can replace the
    /// user's clipboard with attacker-controlled text right before
    /// they paste into another window.
    pub osc_52_write: bool,
    /// Render Markdown rich blocks sent with `OSC 7700` (what
    /// `toastty md` emits). On by default. When off the sequence is
    /// ignored. Rendering is bounded (row cap, image and diagram size
    /// limits; see `docs/markdown.md`) and never touches the network.
    pub markdown: bool,
    /// Let Markdown blocks load local image files
    /// (`![](diagram.png)`), resolved against the sender's directory.
    /// Off by default: a block can come from any program, including one
    /// on a remote host, and must not make the terminal read your
    /// files. `data:` images always work; remote URLs are never fetched.
    pub markdown_local_files: bool,
}

impl SecurityConfig {
    /// Schema defaults — OSC 52 read and write both allowed.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            osc_52_read: true,
            osc_52_write: true,
            markdown: true,
            markdown_local_files: false,
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
    fn defaults_allow_osc_52() {
        // Deliberate since c6dbfda: clipboard integration works out of
        // the box; `[security]` is the opt-out.
        let s = SecurityConfig::defaults();
        assert!(s.osc_52_read);
        assert!(s.osc_52_write);
    }

    #[test]
    fn empty_table_uses_defaults_and_keys_can_opt_out() {
        let empty: SecurityConfig = toml::from_str("").unwrap();
        assert_eq!(empty, SecurityConfig::defaults());
        let off: SecurityConfig =
            toml::from_str("osc_52_read = false\nosc_52_write = false\n").unwrap();
        assert!(!off.osc_52_read);
        assert!(!off.osc_52_write);
    }

    #[test]
    fn markdown_defaults_on_local_files_off() {
        let s = SecurityConfig::defaults();
        assert!(s.markdown);
        assert!(!s.markdown_local_files);
    }

    #[test]
    fn markdown_keys_can_be_toggled() {
        let s: SecurityConfig = toml::from_str("markdown = false\n").unwrap();
        assert!(!s.markdown);
        assert!(!s.markdown_local_files);
        assert!(s.osc_52_read, "other keys keep their defaults");
        let s: SecurityConfig = toml::from_str("markdown_local_files = true\n").unwrap();
        assert!(s.markdown);
        assert!(s.markdown_local_files);
    }

    #[test]
    fn default_trait_matches_defaults() {
        assert_eq!(SecurityConfig::default(), SecurityConfig::defaults());
    }

    #[test]
    fn round_trip_via_toml() {
        let s = SecurityConfig {
            osc_52_read: false,
            osc_52_write: true,
            markdown: false,
            markdown_local_files: true,
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
