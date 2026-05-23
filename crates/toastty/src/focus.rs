//! Focus-event reporting (DECSET 1004).
//!
//! When the app has enabled focus reporting via `\x1b[?1004h`, we emit
//! `\x1b[I` on focus-in and `\x1b[O` on focus-out. Prompt themes (tmux,
//! starship, p10k) dim their styling when the terminal loses focus.

/// Bytes for focus-in (`ESC [ I`).
pub const FOCUS_IN: &[u8] = b"\x1b[I";
/// Bytes for focus-out (`ESC [ O`).
pub const FOCUS_OUT: &[u8] = b"\x1b[O";

/// Pick the byte sequence to send for a focus transition, or `None` if
/// reporting is disabled.
#[must_use]
pub fn encode_focus(focused: bool, report_enabled: bool) -> Option<&'static [u8]> {
    if !report_enabled {
        return None;
    }
    Some(if focused { FOCUS_IN } else { FOCUS_OUT })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_in_when_enabled() {
        assert_eq!(encode_focus(true, true), Some(FOCUS_IN));
    }

    #[test]
    fn focus_out_when_enabled() {
        assert_eq!(encode_focus(false, true), Some(FOCUS_OUT));
    }

    #[test]
    fn no_emission_when_disabled() {
        assert_eq!(encode_focus(true, false), None);
        assert_eq!(encode_focus(false, false), None);
    }

    #[test]
    fn byte_sequences_are_canonical() {
        assert_eq!(FOCUS_IN, b"\x1b[I");
        assert_eq!(FOCUS_OUT, b"\x1b[O");
    }
}
