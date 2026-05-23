//! `[shell]` table.
//!
//! Stored but not wired through to PTY spawn yet — that lands in
//! `toastty/src/main.rs` during M5.

use serde::{Deserialize, Serialize};

/// Shell launch config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Program path / name. `"auto"` means "honor `$SHELL`, fall back to
    /// `/bin/sh`" — the binary does the resolution at spawn time so this
    /// crate stays env-free.
    pub program: String,
    /// Args passed to the program. Empty by default.
    pub args: Vec<String>,
}

impl ShellConfig {
    /// Schema defaults: program = "auto", no args.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            program: "auto".to_string(),
            args: Vec::new(),
        }
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_defaults() {
        let s = ShellConfig::defaults();
        assert_eq!(s.program, "auto");
        assert!(s.args.is_empty());
    }

    #[test]
    fn shell_default_trait() {
        assert_eq!(ShellConfig::default(), ShellConfig::defaults());
    }

    #[test]
    fn shell_round_trip() {
        let s = ShellConfig {
            program: "/bin/zsh".into(),
            args: vec!["-l".into(), "-i".into()],
        };
        let t = toml::to_string(&s).unwrap();
        let p: ShellConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, s);
    }

    #[test]
    fn shell_unknown_key_rejected() {
        let r: Result<ShellConfig, _> = toml::from_str("program = \"bash\"\nfoo = 1\n");
        assert!(r.is_err());
    }
}
