//! Ctrl/Cmd+click link opening: modifier policy and the system opener.
//!
//! Hit-testing lives in `toastty_term` ([`toastty_term::Term::link_at`]),
//! URL recognition in [`toastty_protocols::url_detect`]; this module holds
//! the small pieces the binary glues together.

use toastty_protocols::url_detect;
use toastty_window::Modifiers;

/// True when `modifiers` is exactly the platform's open-link modifier:
/// Cmd on macOS, Ctrl elsewhere (iTerm2 / Alacritty / kitty convention).
///
/// The set must match exactly so `Ctrl+Shift`/`Ctrl+Alt` drags stay free
/// for selection and window-manager bindings; the lock bits (Caps Lock /
/// Num Lock) are ignored because they're latched state, not chords.
#[must_use]
pub fn is_open_link_modifiers(modifiers: Modifiers) -> bool {
    let chord = modifiers - Modifiers::CAPS_LOCK - Modifiers::NUM_LOCK;
    if cfg!(target_os = "macos") {
        chord == Modifiers::SUPER
    } else {
        chord == Modifiers::CONTROL
    }
}

/// Why [`open_link`] didn't open a URL.
#[derive(Debug)]
pub enum OpenLinkError {
    /// The URL's scheme is refused (see [`url_detect::is_safe_to_open`]).
    Refused,
    /// The system opener failed.
    Opener(std::io::Error),
}

impl std::fmt::Display for OpenLinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused => f.write_str("refusing to open URL with an unsafe scheme"),
            Self::Opener(e) => write!(f, "{e}"),
        }
    }
}

/// Hand `url` to the system opener (the `webbrowser` crate: `$BROWSER`
/// first, then `xdg-open` & co. on Unix, `open` on macOS), unless its
/// scheme is unsafe. Shared by OSC 8 hyperlinks and detected URLs.
///
/// # Errors
/// [`OpenLinkError::Refused`] for unsafe schemes, [`OpenLinkError::Opener`]
/// when no opener could be launched.
pub fn open_link(url: &str) -> Result<(), OpenLinkError> {
    if !url_detect::is_safe_to_open(url) {
        return Err(OpenLinkError::Refused);
    }
    webbrowser::open(url).map_err(OpenLinkError::Opener)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> Modifiers {
        if cfg!(target_os = "macos") {
            Modifiers::SUPER
        } else {
            Modifiers::CONTROL
        }
    }

    #[test]
    fn bare_primary_modifier_opens_links() {
        assert!(is_open_link_modifiers(primary()));
    }

    #[test]
    fn lock_bits_are_ignored() {
        assert!(is_open_link_modifiers(primary() | Modifiers::CAPS_LOCK));
        assert!(is_open_link_modifiers(
            primary() | Modifiers::NUM_LOCK | Modifiers::CAPS_LOCK
        ));
        assert!(!is_open_link_modifiers(Modifiers::NUM_LOCK));
    }

    #[test]
    fn extra_or_missing_modifiers_do_not_open_links() {
        assert!(!is_open_link_modifiers(Modifiers::empty()));
        assert!(!is_open_link_modifiers(primary() | Modifiers::SHIFT));
        assert!(!is_open_link_modifiers(primary() | Modifiers::ALT));
        assert!(!is_open_link_modifiers(
            Modifiers::CONTROL | Modifiers::SUPER
        ));
        assert!(!is_open_link_modifiers(Modifiers::ALT));
    }

    #[test]
    fn unsafe_urls_are_refused_without_spawning() {
        assert!(matches!(
            open_link("javascript:alert(1)"),
            Err(OpenLinkError::Refused)
        ));
        assert!(matches!(
            open_link("data:text/html,x"),
            Err(OpenLinkError::Refused)
        ));
        assert_eq!(
            OpenLinkError::Refused.to_string(),
            "refusing to open URL with an unsafe scheme"
        );
        let e = OpenLinkError::Opener(std::io::Error::other("boom"));
        assert_eq!(e.to_string(), "boom");
    }
}
