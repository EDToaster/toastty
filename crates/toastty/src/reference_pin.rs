//! Pure input routing for the nonmodal reference pin.
//!
//! The app owns snapshot, clipboard and redraw side effects. This module
//! classifies the shortcut and pairs pointer presses with their releases
//! so the terminal never receives half of an overlay click.

use toastty_render::reference_pin::PinButton;
use toastty_window::{KeyState, LogicalKey, Modifiers, MouseButton};

/// Match Ctrl+Shift+P, or Cmd+Shift+P on macOS.
///
/// Caps/Num Lock do not change the binding. Other modifiers must match
/// exactly: notably, plain Ctrl+P remains the terminal application's key.
/// The caller handles synthetic events, repeats and release suppression.
#[must_use]
pub fn is_pin_binding(logical: &LogicalKey, modifiers: Modifiers, macos: bool) -> bool {
    let primary = if macos {
        Modifiers::SUPER
    } else {
        Modifiers::CONTROL
    };
    let relevant = modifiers - (Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
    relevant == (primary | Modifiers::SHIFT)
        && matches!(logical, LogicalKey::Character(text) if text.eq_ignore_ascii_case("p"))
}

/// The current pointer position relative to the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceHit {
    Outside,
    Body,
    Button(PinButton),
}

/// What the app should do with a mouse-button event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerOutcome {
    /// Do not forward this event to selection, links or the PTY.
    pub consume: bool,
    /// Perform this action after a complete left click on its button.
    pub activate: Option<PinButton>,
}

#[derive(Debug)]
struct Capture {
    button: MouseButton,
    action: Option<PinButton>,
}

/// Tracks buttons whose presses belonged to the reference card.
///
/// Every button pressed over the card is consumed, including presses on
/// its noninteractive body. Only a left click can activate an action.
/// Each matching release is consumed wherever it lands, even after the
/// card disappears. Releases for uncaptured buttons always pass through,
/// including when a terminal drag ends over the card.
///
/// Keep this state alive when hiding or replacing the pin. Use
/// [`Self::cancel_actions`] to disarm clicks without losing their paired
/// releases; dropping captures while buttons are held would leak those
/// releases to the terminal.
#[derive(Debug, Default)]
pub struct ReferencePointer {
    captures: Vec<Capture>,
}

impl ReferencePointer {
    /// Whether a card-owned press is still awaiting its release.
    ///
    /// The app can suppress pointer motion and wheel events while this
    /// is true, including outside the card during a captured drag.
    #[must_use]
    pub fn is_captured(&self) -> bool {
        !self.captures.is_empty()
    }

    /// Route one button event using the same hit test as the renderer.
    ///
    /// Calling this while the card is hidden, with `Outside`, still
    /// consumes any releases of presses that began on the card.
    #[must_use]
    pub fn mouse(
        &mut self,
        button: MouseButton,
        state: KeyState,
        hit: ReferenceHit,
    ) -> PointerOutcome {
        let captured = self.captures.iter().position(|c| c.button == button);
        match state {
            KeyState::Pressed => {
                // A duplicate press cannot change ownership or rearm a
                // cancelled action before the matching release.
                if captured.is_none() {
                    if hit == ReferenceHit::Outside {
                        return PointerOutcome::default();
                    }
                    let action = match (button, hit) {
                        (MouseButton::Left, ReferenceHit::Button(action)) => Some(action),
                        _ => None,
                    };
                    self.captures.push(Capture { button, action });
                }
                PointerOutcome {
                    consume: true,
                    activate: None,
                }
            }
            KeyState::Released => {
                let Some(index) = captured else {
                    return PointerOutcome::default();
                };
                let capture = self.captures.swap_remove(index);
                PointerOutcome {
                    consume: true,
                    activate: capture
                        .action
                        .filter(|action| hit == ReferenceHit::Button(*action)),
                }
            }
        }
    }

    /// Cancel an action when a drag leaves the button it started on.
    ///
    /// Returning to the button does not rearm it. Ownership of the
    /// eventual release is retained, regardless of the new position.
    pub fn motion(&mut self, hit: ReferenceHit) {
        for capture in &mut self.captures {
            if capture
                .action
                .is_some_and(|action| hit != ReferenceHit::Button(action))
            {
                capture.action = None;
            }
        }
    }

    /// Disarm pending actions without giving their releases to the PTY.
    ///
    /// Useful when the pointer leaves the window, the pin is dismissed
    /// by a shortcut, or a modal dialog takes priority over the card.
    pub fn cancel_actions(&mut self) {
        for capture in &mut self.captures {
            capture.action = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_window::NamedKey;

    const CONSUMED: PointerOutcome = PointerOutcome {
        consume: true,
        activate: None,
    };

    #[test]
    fn pin_shortcut_requires_the_exact_platform_chord() {
        for (macos, primary, other) in [
            (false, Modifiers::CONTROL, Modifiers::SUPER),
            (true, Modifiers::SUPER, Modifiers::CONTROL),
        ] {
            let chord = primary | Modifiers::SHIFT;
            for text in ["p", "P"] {
                let key = LogicalKey::Character(text.into());
                for locks in [
                    Modifiers::empty(),
                    Modifiers::CAPS_LOCK,
                    Modifiers::NUM_LOCK,
                    Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK,
                ] {
                    assert!(is_pin_binding(&key, chord | locks, macos));
                }
                for wrong in [
                    Modifiers::empty(),
                    primary,
                    Modifiers::SHIFT,
                    other | Modifiers::SHIFT,
                    chord | other,
                    chord | Modifiers::ALT,
                ] {
                    assert!(!is_pin_binding(&key, wrong, macos));
                }
            }
            for key in [
                LogicalKey::Character("pp".into()),
                LogicalKey::Character("q".into()),
                LogicalKey::Character("\u{10}".into()),
                LogicalKey::Named(NamedKey::Escape),
                LogicalKey::Unidentified,
            ] {
                assert!(!is_pin_binding(&key, chord, macos));
            }
        }
    }

    #[test]
    fn matching_left_click_activates_only_on_release() {
        for action in [PinButton::Copy, PinButton::Dismiss] {
            let mut pointer = ReferencePointer::default();
            let hit = ReferenceHit::Button(action);
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Pressed, hit),
                CONSUMED
            );
            pointer.motion(hit);
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Released, hit),
                PointerOutcome {
                    consume: true,
                    activate: Some(action),
                }
            );
            assert!(!pointer.is_captured());
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Released, hit),
                PointerOutcome::default()
            );
        }
    }

    #[test]
    fn terminal_press_keeps_its_release_even_over_card() {
        for hit in [
            ReferenceHit::Body,
            ReferenceHit::Button(PinButton::Copy),
            ReferenceHit::Button(PinButton::Dismiss),
        ] {
            let mut pointer = ReferencePointer::default();
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Pressed, ReferenceHit::Outside),
                PointerOutcome::default()
            );
            pointer.motion(hit);
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Released, hit),
                PointerOutcome::default()
            );
            assert!(!pointer.is_captured());
        }
    }

    #[test]
    fn card_body_captures_every_button_without_activating() {
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
            MouseButton::Other(17),
            MouseButton::Other(u16::MAX),
        ] {
            let mut pointer = ReferencePointer::default();
            assert_eq!(
                pointer.mouse(button, KeyState::Pressed, ReferenceHit::Body),
                CONSUMED
            );
            assert!(pointer.is_captured());
            assert_eq!(
                pointer.mouse(
                    button,
                    KeyState::Released,
                    ReferenceHit::Button(PinButton::Dismiss),
                ),
                CONSUMED
            );
            assert!(!pointer.is_captured());
        }
    }

    #[test]
    fn non_left_clicks_on_actions_are_consumed_not_activated() {
        for button in [
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Other(8),
        ] {
            let mut pointer = ReferencePointer::default();
            let hit = ReferenceHit::Button(PinButton::Copy);
            assert_eq!(pointer.mouse(button, KeyState::Pressed, hit), CONSUMED);
            assert_eq!(pointer.mouse(button, KeyState::Released, hit), CONSUMED);
        }
    }

    #[test]
    fn releasing_off_original_button_cancels_but_still_consumes() {
        for hit in [
            ReferenceHit::Outside,
            ReferenceHit::Body,
            ReferenceHit::Button(PinButton::Dismiss),
        ] {
            let mut pointer = ReferencePointer::default();
            let _ = pointer.mouse(
                MouseButton::Left,
                KeyState::Pressed,
                ReferenceHit::Button(PinButton::Copy),
            );
            assert_eq!(
                pointer.mouse(MouseButton::Left, KeyState::Released, hit),
                CONSUMED
            );
            assert!(!pointer.is_captured());
        }
    }

    #[test]
    fn dragging_away_cannot_rearm_the_action_by_returning() {
        let mut pointer = ReferencePointer::default();
        let copy = ReferenceHit::Button(PinButton::Copy);
        let _ = pointer.mouse(MouseButton::Left, KeyState::Pressed, copy);
        pointer.motion(ReferenceHit::Outside);
        assert!(pointer.is_captured());
        pointer.motion(copy);
        // Even a duplicate press cannot turn the cancelled drag into
        // a fresh click or leave an extra capture behind.
        assert_eq!(
            pointer.mouse(MouseButton::Left, KeyState::Pressed, copy),
            CONSUMED
        );
        assert_eq!(
            pointer.mouse(MouseButton::Left, KeyState::Released, copy),
            CONSUMED
        );
        assert!(!pointer.is_captured());
    }

    #[test]
    fn simultaneous_buttons_keep_independent_release_ownership() {
        let mut pointer = ReferencePointer::default();
        let copy = ReferenceHit::Button(PinButton::Copy);
        let _ = pointer.mouse(MouseButton::Left, KeyState::Pressed, copy);
        let _ = pointer.mouse(MouseButton::Right, KeyState::Pressed, ReferenceHit::Body);
        assert_eq!(
            pointer.mouse(MouseButton::Middle, KeyState::Released, copy),
            PointerOutcome::default()
        );
        assert_eq!(
            pointer.mouse(
                MouseButton::Right,
                KeyState::Released,
                ReferenceHit::Outside
            ),
            CONSUMED
        );
        assert!(pointer.is_captured());
        assert_eq!(
            pointer.mouse(MouseButton::Left, KeyState::Released, copy),
            PointerOutcome {
                consume: true,
                activate: Some(PinButton::Copy),
            }
        );
        assert!(!pointer.is_captured());
    }

    #[test]
    fn cancelling_actions_keeps_captures_until_release() {
        let mut pointer = ReferencePointer::default();
        let dismiss = ReferenceHit::Button(PinButton::Dismiss);
        let _ = pointer.mouse(MouseButton::Left, KeyState::Pressed, dismiss);
        let _ = pointer.mouse(MouseButton::Other(7), KeyState::Pressed, ReferenceHit::Body);
        pointer.cancel_actions();
        assert!(pointer.is_captured());
        assert_eq!(
            pointer.mouse(MouseButton::Left, KeyState::Released, dismiss),
            CONSUMED
        );
        assert!(pointer.is_captured());
        assert_eq!(
            pointer.mouse(
                MouseButton::Other(7),
                KeyState::Released,
                ReferenceHit::Outside,
            ),
            CONSUMED
        );
        assert!(!pointer.is_captured());
    }
}
