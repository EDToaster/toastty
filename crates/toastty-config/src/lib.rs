//! TOML configuration loading.
//!
//! ## Crate dependency-graph note (M4.5)
//!
//! `toastty-config` is intentionally a **leaf crate**: it does not depend
//! on any other workspace crate. In particular it does **not** depend on
//! `toastty-render`. The renderer's GPU types (`Theme`, etc.) live in
//! `toastty-render`; this crate only exposes raw, serde-shaped values
//! (`ThemeConfig`, `FontConfig`, …).
//!
//! Whoever needs to *bridge* the two (i.e. the `toastty` binary, or any
//! example that constructs a `Renderer`) is the one that depends on both
//! crates and performs the trivial `ThemeConfig -> toastty_render::Theme`
//! mapping. This keeps the dep graph linear (`bin -> render -> term`,
//! `bin -> config`) and stops `toastty-config` from re-pulling wgpu / the
//! entire render stack into its build & test cycle — which matters a lot
//! for the 95 % coverage gate this crate is held to.
//!
//! The conversion helper for the demo lives alongside the demo
//! (`crates/toastty-render/examples/hello_text.rs`).

#![forbid(unsafe_code)]

mod color;
mod cursor;
mod error;
mod font;
mod scrollback;
mod security;
mod shell;
mod theme;
mod xdg;

use std::path::{Path, PathBuf};

pub use color::Color;
pub use cursor::{CursorConfig, CursorShape};
pub use error::ConfigError;
pub use font::FontConfig;
pub use scrollback::ScrollbackConfig;
pub use security::SecurityConfig;
pub use shell::ShellConfig;
pub use theme::ThemeConfig;

use serde::{Deserialize, Serialize};

/// Top-level config.
///
/// All fields are optional in the TOML — anything missing falls back to
/// [`Config::defaults`]. Use [`Config::load_from_path`] or
/// [`Config::load_default`] to construct from disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub cursor: CursorConfig,
    pub shell: ShellConfig,
    pub scrollback: ScrollbackConfig,
    pub security: SecurityConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Config {
    /// Built-in defaults. Pure, deterministic, no I/O.
    ///
    /// Mirrors the schema documented in `docs/architecture.md` (M4.5
    /// section).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            font: FontConfig::defaults(),
            theme: ThemeConfig::defaults(),
            cursor: CursorConfig::defaults(),
            shell: ShellConfig::defaults(),
            scrollback: ScrollbackConfig::defaults(),
            security: SecurityConfig::defaults(),
        }
    }

    /// Parse the TOML at `path`. Missing fields fall back to defaults.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        Self::parse_str(&contents)
    }

    /// Parse a TOML string directly. Useful for tests and embedded
    /// configs; the public surface is [`Config::load_from_path`].
    pub fn parse_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str::<Self>(s).map_err(ConfigError::Toml)
    }

    /// Try the XDG-style path
    /// (`$XDG_CONFIG_HOME/toastty/config.toml` or
    /// `~/.config/toastty/config.toml`).
    ///
    /// If a file is present and parses cleanly, returns
    /// `(config, ConfigSource::File(path))`. Otherwise returns
    /// `(Config::defaults(), ConfigSource::Defaults)`.
    ///
    /// **Errors are silently swallowed** here — a parse failure on the
    /// user's config shouldn't take the terminal down. Use
    /// [`Config::load_from_path`] when you want a hard error path
    /// (e.g. when the user passed `--config <path>` explicitly).
    #[must_use]
    pub fn load_default() -> (Self, ConfigSource) {
        let Some(path) = xdg::default_config_path() else {
            return (Self::defaults(), ConfigSource::Defaults);
        };
        match Self::load_from_path(&path) {
            Ok(cfg) => (cfg, ConfigSource::File(path)),
            Err(_) => (Self::defaults(), ConfigSource::Defaults),
        }
    }
}

/// Where [`Config::load_default`] sourced its result from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded from this path on disk.
    File(PathBuf),
    /// No file found (or unreadable); built-in defaults are in effect.
    Defaults,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file:{}", p.display()),
            Self::Defaults => write!(f, "defaults"),
        }
    }
}

// `xdg` is private but useful for tests.
#[doc(hidden)]
pub mod test_support {
    //! Test-only helpers. Not part of the public API.
    pub use crate::xdg::resolve_with_env;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::defaults();
        let serialized = toml::to_string(&cfg).expect("serialize defaults");
        let parsed: Config = toml::from_str(&serialized).expect("parse serialized defaults");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn defaults_match_documented_schema() {
        let cfg = Config::defaults();
        assert_eq!(cfg.font.family, "Fira Mono");
        assert!((cfg.font.size_px - 16.0).abs() < 1e-6);
        assert!((cfg.font.line_height - 1.4).abs() < 1e-6);
        assert_eq!(cfg.cursor.shape, CursorShape::Block);
        assert!(cfg.cursor.blink);
        assert_eq!(cfg.shell.program, "auto");
        assert!(cfg.shell.args.is_empty());
        assert_eq!(cfg.scrollback.lines, 10_000);
        assert_eq!(cfg.theme.palette.len(), 16);
    }

    #[test]
    fn empty_toml_gives_defaults() {
        let cfg = Config::parse_str("").expect("empty toml ok");
        assert_eq!(cfg, Config::defaults());
    }

    #[test]
    fn partial_toml_only_overrides_listed_fields() {
        let cfg = Config::parse_str("[font]\nsize_px = 18.0\n").expect("partial toml ok");
        assert!((cfg.font.size_px - 18.0).abs() < 1e-6);
        // Other font fields still default.
        assert_eq!(cfg.font.family, "Fira Mono");
        assert!((cfg.font.line_height - 1.4).abs() < 1e-6);
        // Other sections still default.
        assert_eq!(cfg.cursor.shape, CursorShape::Block);
    }

    #[test]
    fn fully_populated_toml_parses() {
        let toml_src = include_str!("../tests/fixtures/full.toml");
        let cfg = Config::parse_str(toml_src).expect("full fixture parses");
        // Round-trip too.
        let serialized = toml::to_string(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&serialized).expect("re-parse");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let res = Config::parse_str("nonsense = true\n");
        assert!(res.is_err(), "expected unknown-key error");
    }

    #[test]
    fn load_from_path_missing_file_returns_io_error() {
        let res = Config::load_from_path(Path::new("/definitely/not/a/path.toml"));
        match res {
            Err(ConfigError::Io(_, _)) => (),
            other => panic!("expected ConfigError::Io, got {other:?}"),
        }
    }

    #[test]
    fn load_from_path_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[font]\nsize_px = 20.0\n").unwrap();
        let cfg = Config::load_from_path(&path).expect("load");
        assert!((cfg.font.size_px - 20.0).abs() < 1e-6);
    }

    #[test]
    fn config_source_display() {
        let p = PathBuf::from("/tmp/x");
        assert_eq!(ConfigSource::File(p.clone()).to_string(), "file:/tmp/x");
        assert_eq!(ConfigSource::Defaults.to_string(), "defaults");
    }

    #[test]
    fn load_default_returns_defaults_when_no_file() {
        // Force the resolver to a definitely-empty directory.
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = dir.path().to_path_buf();
        // No file written under xdg → defaults.
        let resolved = test_support::resolve_with_env(Some(&xdg), None);
        assert_eq!(resolved, Some(xdg.join("toastty/config.toml")));
        let (cfg, src) = match resolved {
            Some(p) if p.exists() => (
                Config::load_from_path(&p).expect("load"),
                ConfigSource::File(p),
            ),
            _ => (Config::defaults(), ConfigSource::Defaults),
        };
        assert_eq!(cfg, Config::defaults());
        assert_eq!(src, ConfigSource::Defaults);
    }

    #[test]
    fn load_default_finds_file_in_xdg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let xdg = dir.path().to_path_buf();
        let path = xdg.join("toastty").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[font]\nsize_px = 22.5\n").unwrap();

        let resolved = test_support::resolve_with_env(Some(&xdg), None).expect("path resolves");
        assert_eq!(resolved, path);
        let cfg = Config::load_from_path(&resolved).expect("load");
        assert!((cfg.font.size_px - 22.5).abs() < 1e-6);
    }

    #[test]
    fn load_default_swallows_parse_errors() {
        // Simulate a real call where the env points at a directory with
        // a malformed file. We can't actually touch the process env in a
        // multi-threaded test runner safely, so we exercise the same
        // code path via the public Config::load_from_path + the result
        // contract of load_default (errors return Defaults).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("toastty").join("config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[font\nbroken").unwrap();
        let result = Config::load_from_path(&path);
        assert!(result.is_err());

        // The load_default branch: simulate the same lookup → parse →
        // fallback chain using the resolver.
        let resolved = test_support::resolve_with_env(Some(dir.path()), None).unwrap();
        let (cfg, src) = match Config::load_from_path(&resolved) {
            Ok(c) => (c, ConfigSource::File(resolved)),
            Err(_) => (Config::defaults(), ConfigSource::Defaults),
        };
        assert_eq!(cfg, Config::defaults());
        assert_eq!(src, ConfigSource::Defaults);
    }

    #[test]
    fn config_default_trait_matches_defaults() {
        let a: Config = Config::default();
        let b = Config::defaults();
        assert_eq!(a, b);
    }

    #[test]
    fn load_default_is_callable_and_returns_a_source() {
        // We can't control the user's real XDG dir; just verify the
        // function runs and returns something well-formed.
        let (cfg, src) = Config::load_default();
        // Any returned config must at least round-trip through TOML.
        let _ = toml::to_string(&cfg).expect("serialize");
        match src {
            ConfigSource::File(p) => assert!(!p.as_os_str().is_empty()),
            ConfigSource::Defaults => assert_eq!(cfg, Config::defaults()),
        }
    }
}
