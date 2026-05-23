//! XDG-style config path resolution.
//!
//! Order (Unix):
//! 1. `$XDG_CONFIG_HOME/toastty/config.toml` if `XDG_CONFIG_HOME` is set.
//! 2. `$HOME/.config/toastty/config.toml` if `HOME` is set.
//!
//! Returns `None` if neither env var is set. Caller decides what to do
//! (the toplevel falls back to `Config::defaults()`).
//!
//! TODO(windows): on Windows we'd use `dirs::config_dir()` (typically
//! `%APPDATA%/toastty/config.toml`). Deferred to a v2 task; this crate
//! stays leaf-pure for M4.5.

use std::path::{Path, PathBuf};

const APP_DIR: &str = "toastty";
const FILE_NAME: &str = "config.toml";

/// Default config path using the process environment.
#[must_use]
pub(crate) fn default_config_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_with_env(xdg.as_deref(), home.as_deref())
}

/// Same resolution rule but with explicit env values — keeps tests
/// from poisoning the real process environment (which is racy under
/// the multi-threaded test harness).
#[must_use]
pub fn resolve_with_env(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(x) = xdg_config_home
        && !x.as_os_str().is_empty()
    {
        return Some(x.join(APP_DIR).join(FILE_NAME));
    }
    if let Some(h) = home
        && !h.as_os_str().is_empty()
    {
        return Some(h.join(".config").join(APP_DIR).join(FILE_NAME));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_wins_over_home() {
        let xdg = PathBuf::from("/x");
        let home = PathBuf::from("/h");
        let p = resolve_with_env(Some(&xdg), Some(&home)).unwrap();
        assert_eq!(p, PathBuf::from("/x/toastty/config.toml"));
    }

    #[test]
    fn home_used_when_xdg_missing() {
        let home = PathBuf::from("/h");
        let p = resolve_with_env(None, Some(&home)).unwrap();
        assert_eq!(p, PathBuf::from("/h/.config/toastty/config.toml"));
    }

    #[test]
    fn nothing_when_both_missing() {
        assert!(resolve_with_env(None, None).is_none());
    }

    #[test]
    fn empty_xdg_falls_through_to_home() {
        let xdg = PathBuf::from("");
        let home = PathBuf::from("/h");
        let p = resolve_with_env(Some(&xdg), Some(&home)).unwrap();
        assert_eq!(p, PathBuf::from("/h/.config/toastty/config.toml"));
    }

    #[test]
    fn empty_home_returns_none_if_no_xdg() {
        let home = PathBuf::from("");
        assert!(resolve_with_env(None, Some(&home)).is_none());
    }

    #[test]
    fn default_config_path_is_callable() {
        // Just exercise the env-reading branch — content depends on the
        // test runner's environment so we don't assert a specific path.
        let _ = default_config_path();
    }
}
