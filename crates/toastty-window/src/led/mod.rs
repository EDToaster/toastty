//! Platform-specific lock-key LED state.
//!
//! Caps Lock and Num Lock are *not* exposed through `winit::ModifiersState`
//! (decision §1 in `docs/decisions/window-input.md`), but the kitty
//! keyboard protocol's modifier mask reserves bits 64 and 128 for them.
//! We read the actual platform state and OR the bits in at the wrapper
//! seam in [`crate::translate_modifiers`].
//!
//! Implementation status:
//!
//! - **macOS**: `CGEventSource::flagsState` via direct FFI.
//!   `kCGEventFlagMaskAlphaShift` = caps lock; no public num-lock API on
//!   macOS (Mac hardware rarely ships a num-lock key — return false).
//! - **Linux**: `TODO(linux-leds)`. The plan is `/sys/class/leds/input*::capslock/brightness`
//!   for X11/Wayland-agnostic reading, or `XkbGetIndicatorState` on X11.
//!   Left unwired for M7; Linux users will see kitty modifiers without
//!   caps/num bits.
//! - **Other**: returns `false` for both.

/// Snapshot of the current lock-key LED state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedState {
    pub caps_lock: bool,
    pub num_lock: bool,
}

/// Read the current LED state from the OS. Cheap enough to call on every
/// key event on macOS (the underlying call is a single Mach IPC roundtrip
/// to `WindowServer`; tens of microseconds).
#[must_use]
pub fn read_led_state() -> LedState {
    platform::read_led_state()
}

// ============================================================================
// macOS
// ============================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::LedState;

    // From CoreGraphics/CGEventTypes.h:
    //   kCGEventFlagMaskAlphaShift = 0x00010000
    const FLAG_ALPHA_SHIFT: u64 = 0x0001_0000;

    // CGEventSourceStateID:
    //   kCGEventSourceStateCombinedSessionState = 0
    const COMBINED_SESSION_STATE: i32 = 0;

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
    }

    pub(super) fn read_led_state() -> LedState {
        // Safety: `CGEventSourceFlagsState` is a thread-safe Apple system
        // function. It takes a single integer (`state_id`) and returns
        // a 64-bit flags bitmask. No allocations, no callbacks, no
        // borrowed references. Calling with `kCGEventSourceStateCombinedSessionState`
        // (= 0) reads the per-session caps-lock state.
        #[allow(unsafe_code)]
        let flags = unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) };
        LedState {
            caps_lock: (flags & FLAG_ALPHA_SHIFT) != 0,
            // macOS has no public num-lock API. Mac keyboards rarely ship
            // a num-lock key. Report false.
            num_lock: false,
        }
    }
}

// ============================================================================
// Linux (and everything else — stubbed)
// ============================================================================

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::LedState;

    pub(super) fn read_led_state() -> LedState {
        // TODO(linux-leds): read /sys/class/leds/input*::capslock/brightness
        // (works on X11 + Wayland without an XKB roundtrip), or query
        // XkbGetIndicatorState on X11. Unimplemented for M7; returns
        // a zero state so kitty modifier reporting still works for the
        // standard SHIFT/CTRL/ALT/SUPER bits.
        LedState::default()
    }
}

// ============================================================================
// Tests — pure, no platform calls.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_both_off() {
        let s = LedState::default();
        assert!(!s.caps_lock);
        assert!(!s.num_lock);
    }

    #[test]
    fn read_led_state_does_not_panic() {
        // Real call on every platform. We don't assert a value because
        // the host's actual caps-lock state could be either way; we just
        // confirm the call returns without panicking.
        let _ = read_led_state();
    }

    #[test]
    fn equality_and_clone() {
        let a = LedState {
            caps_lock: true,
            num_lock: false,
        };
        let b = a;
        assert_eq!(a, b);
        let c = LedState::default();
        assert_ne!(a, c);
    }
}
