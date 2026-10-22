//! `[shader]` table — user post-process shaders (M13).
//!
//! ```toml
//! [shader]
//! # Post-process shaders, run in order. Empty turns post-processing off.
//! # `~` expands; relative paths are relative to the config file's directory.
//! # .glsl / .frag = GLSL (ShaderToy / Ghostty dialect), .wgsl = WGSL.
//! paths = []
//! # "auto" (detect from the shader), "always" (~60 Hz), "never" (events only).
//! animation = "auto"
//! ```
//!
//! This crate only stores the raw strings; path resolution and file
//! loading live in the binary (`toastty::shader_files`). See
//! `docs/milestones/m13-user-shaders.md`.

use serde::{Deserialize, Serialize};

/// When post-process shaders get time-driven redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShaderAnimation {
    /// Detect from the shader which inputs it reads (the default).
    #[default]
    Auto,
    /// Redraw at ~60 Hz all the time, even unfocused.
    Always,
    /// Redraw only on terminal updates and shader-input events.
    Never,
}

/// Post-process shader config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShaderConfig {
    /// Shader files, run in order. Empty disables post-processing.
    pub paths: Vec<String>,
    /// Animation mode.
    pub animation: ShaderAnimation,
}

impl ShaderConfig {
    /// Schema defaults — no shaders, `auto` animation.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            paths: Vec::new(),
            animation: ShaderAnimation::Auto,
        }
    }
}

impl Default for ShaderConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_auto() {
        let d = ShaderConfig::defaults();
        assert_eq!(d, ShaderConfig::default());
        assert!(d.paths.is_empty());
        assert_eq!(d.animation, ShaderAnimation::Auto);
    }

    #[test]
    fn round_trip_and_partial_table() {
        let c = ShaderConfig {
            paths: vec!["~/a.glsl".into(), "b.wgsl".into()],
            animation: ShaderAnimation::Never,
        };
        let t = toml::to_string(&c).unwrap();
        let p: ShaderConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, c);
        let only_paths: ShaderConfig = toml::from_str("paths = [\"x.glsl\"]\n").unwrap();
        assert_eq!(only_paths.paths, vec!["x.glsl".to_string()]);
        assert_eq!(only_paths.animation, ShaderAnimation::Auto);
    }

    #[test]
    fn animation_parses_each_variant() {
        for (s, want) in [
            ("auto", ShaderAnimation::Auto),
            ("always", ShaderAnimation::Always),
            ("never", ShaderAnimation::Never),
        ] {
            let c: ShaderConfig = toml::from_str(&format!("animation = \"{s}\"\n")).unwrap();
            assert_eq!(c.animation, want);
        }
    }

    #[test]
    fn invalid_values_rejected() {
        assert!(toml::from_str::<ShaderConfig>("animation = \"sometimes\"\n").is_err());
        assert!(toml::from_str::<ShaderConfig>("paths = \"a.glsl\"\n").is_err());
        assert!(toml::from_str::<ShaderConfig>("bogus = 1\n").is_err());
    }
}
