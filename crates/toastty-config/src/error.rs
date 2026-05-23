//! Typed config errors.

use std::path::PathBuf;
use thiserror::Error;

/// Errors produced by config loading / parsing.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the file failed (not found, permissions, etc).
    #[error("failed to read config file {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),

    /// TOML parse failed (syntax, unknown key, type mismatch, ...).
    #[error("invalid TOML: {0}")]
    Toml(#[from] toml::de::Error),

    /// Color hex string didn't validate (length, charset, `#` prefix).
    #[error("invalid color {input:?}: {reason}")]
    InvalidColor {
        input: String,
        reason: &'static str,
    },

    /// `theme.palette` must contain exactly 16 entries.
    #[error("theme.palette must contain exactly 16 colors, got {0}")]
    PaletteLength(usize),

    /// `cursor.shape` was not one of the documented values.
    #[error("unknown cursor shape {0:?}: expected block | bar | underline")]
    UnknownCursorShape(String),
}
