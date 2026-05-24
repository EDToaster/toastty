//! `[scrollback]` table.

use serde::{Deserialize, Serialize};

/// Easing function selector for smooth-scroll animation. Mirrors
/// `toastty_term::Smoothing` but as a wire-side enum (no tuning params)
/// so users only have to pick a curve. The binary supplies sensible
/// defaults for the per-curve params.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SmoothingFunction {
    /// No animation — snap immediately to target on every input.
    Instant,
    /// Constant velocity (~600 px/sec). Predictable but unnatural.
    Linear,
    /// Cubic ease-out, ~250 ms total — quick, settles softly.
    EaseOut,
    /// Exponential decay with 80 ms half-life. Default — feels most
    /// natural under both wheel notches and trackpad inertia.
    #[default]
    ExpDecay,
}

/// Scrollback config — how many lines to retain in the ring buffer and
/// how scrolling animates between positions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollbackConfig {
    /// Maximum scrollback lines retained above the visible region. The
    /// term crate allocates `visible_rows + lines` ring slots.
    pub lines: u32,
    /// Enable sub-row pixel-accurate scrolling and animated wheel
    /// notches. When `false`, scrolling snaps to whole rows and wheel
    /// notches jump instantly; trackpad inertia still scrolls but the
    /// fractional offset is dropped each frame.
    pub smooth_scrolling: bool,
    /// Easing function for the animation between current and target
    /// positions. Ignored when `smooth_scrolling = false`.
    pub smoothing_function: SmoothingFunction,
    /// Terminal rows scrolled per discrete wheel notch (LineDelta=1).
    /// Trackpad `PixelDelta` events are accumulated as raw pixels and
    /// don't go through this knob.
    pub lines_per_notch: u32,
}

impl ScrollbackConfig {
    /// Schema defaults. 10 000 lines of history, smooth-scrolling on
    /// with an 80 ms exponential decay, 3 lines per wheel notch.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            lines: 10_000,
            smooth_scrolling: true,
            smoothing_function: SmoothingFunction::ExpDecay,
            lines_per_notch: 3,
        }
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
        let d = ScrollbackConfig::defaults();
        assert_eq!(d.lines, 10_000);
        assert!(d.smooth_scrolling);
        assert_eq!(d.smoothing_function, SmoothingFunction::ExpDecay);
        assert_eq!(d.lines_per_notch, 3);
    }

    #[test]
    fn scrollback_default_trait() {
        assert_eq!(ScrollbackConfig::default(), ScrollbackConfig::defaults());
    }

    #[test]
    fn scrollback_round_trip() {
        let s = ScrollbackConfig {
            lines: 42,
            smooth_scrolling: false,
            smoothing_function: SmoothingFunction::Linear,
            lines_per_notch: 1,
        };
        let t = toml::to_string(&s).unwrap();
        let p: ScrollbackConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, s);
    }

    #[test]
    fn scrollback_unknown_key_rejected() {
        let r: Result<ScrollbackConfig, _> = toml::from_str("lines = 100\nfoo = 1\n");
        assert!(r.is_err());
    }

    #[test]
    fn smoothing_function_parses_snake_case_variants() {
        let cases = [
            ("instant", SmoothingFunction::Instant),
            ("linear", SmoothingFunction::Linear),
            ("ease_out", SmoothingFunction::EaseOut),
            ("exp_decay", SmoothingFunction::ExpDecay),
        ];
        for (s, want) in cases {
            let toml = format!("smoothing_function = \"{s}\"\n");
            let cfg: ScrollbackConfig = toml::from_str(&toml).unwrap();
            assert_eq!(cfg.smoothing_function, want);
        }
    }

    #[test]
    fn smoothing_function_rejects_unknown() {
        let r: Result<ScrollbackConfig, _> =
            toml::from_str("smoothing_function = \"bogus\"\n");
        assert!(r.is_err());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // Only `lines` set — the rest should pick up defaults.
        let cfg: ScrollbackConfig = toml::from_str("lines = 5000\n").unwrap();
        assert_eq!(cfg.lines, 5000);
        assert!(cfg.smooth_scrolling);
        assert_eq!(cfg.smoothing_function, SmoothingFunction::ExpDecay);
        assert_eq!(cfg.lines_per_notch, 3);
    }
}
