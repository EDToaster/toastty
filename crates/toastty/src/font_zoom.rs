//! Runtime font zoom: shortcut classification and size math.
//!
//! `Ctrl+=` / `Ctrl+-` / `Ctrl+0` (`Cmd` on macOS) grow, shrink and
//! reset the font size, the way kitty (`ctrl+shift+equal` /
//! `ctrl+shift+minus`), Ghostty (`ctrl+=` / `ctrl+-` / `ctrl+0`) and
//! `WezTerm` (`ctrl+=` / `ctrl+-` / `ctrl+0`) do. The binary owns the side
//! effects (re-rasterising, regridding, PTY winsize, in-band resize); this
//! module holds the pure parts so they can be unit-tested.
//!
//! # Size model
//!
//! The zoomed size is a *logical* pixel size, the same unit as the
//! config's `font.size_px`. It replaces `font.size_px` as the input to
//! [`crate::geometry::effective_font_size_px`], so zoom composes with the
//! monitor scale factor exactly the way the configured size does: 17 px
//! zoomed on a 2× monitor rasterises at 34 physical px.
//!
//! # Step and clamp
//!
//! One step is [`ZOOM_STEP_PX`] = 1 logical px. The config already speaks
//! pixels, and at the 16 px default one pixel is ~6 %, close to Ghostty's
//! 1 pt (≈1.33 px) step and finer than kitty's default 2 pt. Cell metrics
//! are snapped to whole pixels by the rasterizer, so a sub-pixel step would
//! often produce no visible change; a larger step makes it hard to land on
//! a comfortable size. Additive (rather than `WezTerm`'s ×1.1) steps also
//! make `n` zoom-ins followed by `n` zoom-outs return exactly to the start.
//!
//! The size is clamped to [`MIN_FONT_PX`]..=[`MAX_FONT_PX`] (6..=72 px).
//! Below ~6 px glyphs are unreadable blobs and cells degenerate; above
//! ~72 px an 800 px wide window holds fewer than ~20 columns, and each
//! glyph's atlas footprint grows quadratically. A configured size outside
//! that range is honoured as-is: the bounds widen to include it, so zoom
//! never jumps away from the user's base size, and reset always returns
//! to it.
//!
//! # Config hot-reload
//!
//! [`FontZoom::rebase`] is called with the reloaded `font.size_px`. If it
//! changed, the zoom resets to the new base (the user just asked for a
//! specific size, so honour it). Any other reload keeps the current zoom.

use toastty_window::{LogicalKey, Modifiers, PhysicalKey};

/// Size change per zoom-in / zoom-out press, in logical px.
pub const ZOOM_STEP_PX: f32 = 1.0;
/// Smallest size zoom-out reaches (unless the configured base is smaller).
pub const MIN_FONT_PX: f32 = 6.0;
/// Largest size zoom-in reaches (unless the configured base is larger).
pub const MAX_FONT_PX: f32 = 72.0;

/// Restart point when the current size is not finite (a `nan` / `inf`
/// `font.size_px` in TOML): the config schema's default size.
const FALLBACK_PX: f32 = 16.0;

/// What a zoom shortcut asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomAction {
    /// Grow by one step.
    In,
    /// Shrink by one step.
    Out,
    /// Return to the configured `font.size_px`.
    Reset,
}

/// Classify a key press as a zoom shortcut.
///
/// The primary modifier is `Cmd` (`SUPER`) on macOS and `Ctrl` elsewhere,
/// chosen the same way as the Ctrl/Cmd+click link binding: by host
/// platform (`macos` is `cfg!(target_os = "macos")` at the call site,
/// passed in so both branches are testable). The primary modifier must be
/// held, Alt and the *other* platform modifier must not; Shift is
/// ignored and so are the Caps/Num Lock bits.
///
/// Shift is ignored because on US layouts `+` is `Shift+=` and `_` is
/// `Shift+-`, so `Ctrl+Shift+=` (kitty's default) and `Ctrl++` both reach
/// us as logical `+`. winit's logical key applies every modifier except
/// Ctrl, which is why matching on the logical character works with Ctrl
/// held.
///
/// Keypad `+` / `-` are matched on their physical codes (`NumpadAdd`,
/// `NumpadSubtract`) so they work regardless of layout. `Ctrl+keypad 0`
/// resets via its logical `0` when Num Lock is on (with it off the key is
/// `Insert`, which is left alone).
#[must_use]
pub fn zoom_action(
    logical: &LogicalKey,
    physical: &PhysicalKey,
    modifiers: Modifiers,
    macos: bool,
) -> Option<ZoomAction> {
    let primary = if macos {
        Modifiers::SUPER
    } else {
        Modifiers::CONTROL
    };
    let relevant = modifiers - (Modifiers::SHIFT | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
    if relevant != primary {
        return None;
    }
    if let PhysicalKey::Code(code) = physical {
        match code.as_str() {
            "NumpadAdd" => return Some(ZoomAction::In),
            "NumpadSubtract" => return Some(ZoomAction::Out),
            _ => {}
        }
    }
    let LogicalKey::Character(s) = logical else {
        return None;
    };
    match s.as_str() {
        "=" | "+" => Some(ZoomAction::In),
        "-" | "_" => Some(ZoomAction::Out),
        "0" => Some(ZoomAction::Reset),
        _ => None,
    }
}

/// Inclusive `(min, max)` bounds for zooming around `base_px`: the fixed
/// [`MIN_FONT_PX`]..=[`MAX_FONT_PX`] window, widened to include a
/// configured base outside it. A non-finite base (TOML allows `nan` /
/// `inf`) gets the fixed window.
#[must_use]
pub fn zoom_bounds(base_px: f32) -> (f32, f32) {
    if base_px.is_finite() {
        (MIN_FONT_PX.min(base_px), MAX_FONT_PX.max(base_px))
    } else {
        (MIN_FONT_PX, MAX_FONT_PX)
    }
}

/// Size after applying `action` to `current_px`, given the configured
/// `base_px`. Steps by [`ZOOM_STEP_PX`] and clamps to [`zoom_bounds`].
/// A non-finite `current_px` restarts from the base, or from
/// [`FALLBACK_PX`] when the base isn't finite either.
#[must_use]
pub fn zoomed_size(current_px: f32, base_px: f32, action: ZoomAction) -> f32 {
    let (lo, hi) = zoom_bounds(base_px);
    if action == ZoomAction::Reset {
        return base_px;
    }
    let start = if current_px.is_finite() {
        current_px
    } else if base_px.is_finite() {
        base_px
    } else {
        FALLBACK_PX
    };
    let delta = match action {
        ZoomAction::In => ZOOM_STEP_PX,
        ZoomAction::Out => -ZOOM_STEP_PX,
        ZoomAction::Reset => unreachable!("handled above"),
    };
    (start + delta).clamp(lo, hi)
}

/// Runtime zoom state: the configured base size and the current
/// (possibly zoomed) logical size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontZoom {
    base_px: f32,
    current_px: f32,
}

impl FontZoom {
    /// Unzoomed state for a configured `font.size_px`.
    #[must_use]
    pub fn new(base_px: f32) -> Self {
        Self {
            base_px,
            current_px: base_px,
        }
    }

    /// Current logical size: what to feed `effective_font_size_px`.
    #[must_use]
    pub fn current_px(&self) -> f32 {
        self.current_px
    }

    /// Configured base size.
    #[must_use]
    pub fn base_px(&self) -> f32 {
        self.base_px
    }

    /// True when the current size differs from the configured base.
    #[must_use]
    pub fn is_zoomed(&self) -> bool {
        !same_size(self.current_px, self.base_px)
    }

    /// Apply a shortcut. Returns `true` when the size actually changed,
    /// so the caller can skip the re-rasterise + regrid at a clamp limit
    /// or on a reset while unzoomed.
    pub fn apply(&mut self, action: ZoomAction) -> bool {
        let next = zoomed_size(self.current_px, self.base_px, action);
        if same_size(next, self.current_px) {
            return false;
        }
        self.current_px = next;
        true
    }

    /// Adopt a (re)loaded `font.size_px`. When it differs from the old
    /// base the zoom resets to it and this returns `true`; an unchanged
    /// base keeps the current zoom and returns `false`.
    pub fn rebase(&mut self, base_px: f32) -> bool {
        if same_size(base_px, self.base_px) {
            return false;
        }
        *self = Self::new(base_px);
        true
    }
}

/// Size equality with a tolerance far below one zoom step, so float
/// noise never counts as a change. NaN equals NaN here (bitwise), so a
/// NaN base doesn't register as "changed" on every reload.
fn same_size(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4 || a.to_bits() == b.to_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> LogicalKey {
        LogicalKey::Character(s.to_string())
    }

    fn code(s: &str) -> PhysicalKey {
        PhysicalKey::Code(s.to_string())
    }

    const CTRL: Modifiers = Modifiers::CONTROL;
    const CMD: Modifiers = Modifiers::SUPER;

    #[test]
    fn ctrl_equals_minus_zero_on_linux() {
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), CTRL, false),
            Some(ZoomAction::In)
        );
        assert_eq!(
            zoom_action(&ch("-"), &code("Minus"), CTRL, false),
            Some(ZoomAction::Out)
        );
        assert_eq!(
            zoom_action(&ch("0"), &code("Digit0"), CTRL, false),
            Some(ZoomAction::Reset)
        );
    }

    #[test]
    fn shifted_forms_are_accepted() {
        let cs = CTRL | Modifiers::SHIFT;
        // Ctrl+Shift+= (kitty default) and Ctrl++ both arrive as "+".
        assert_eq!(
            zoom_action(&ch("+"), &code("Equal"), cs, false),
            Some(ZoomAction::In)
        );
        assert_eq!(
            zoom_action(&ch("+"), &code("Equal"), CTRL, false),
            Some(ZoomAction::In)
        );
        // Ctrl+Shift+= on a backend that reports the unshifted char.
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), cs, false),
            Some(ZoomAction::In)
        );
        // Ctrl+Shift+- (kitty default) arrives as "_".
        assert_eq!(
            zoom_action(&ch("_"), &code("Minus"), cs, false),
            Some(ZoomAction::Out)
        );
    }

    #[test]
    fn keypad_plus_minus_by_physical_code() {
        assert_eq!(
            zoom_action(&ch("+"), &code("NumpadAdd"), CTRL, false),
            Some(ZoomAction::In)
        );
        assert_eq!(
            zoom_action(&ch("-"), &code("NumpadSubtract"), CTRL, false),
            Some(ZoomAction::Out)
        );
        // Layout-independent: the physical code wins over an odd logical.
        assert_eq!(
            zoom_action(&LogicalKey::Unidentified, &code("NumpadAdd"), CTRL, false),
            Some(ZoomAction::In)
        );
        // Keypad 0 with Num Lock on resets via logical "0".
        assert_eq!(
            zoom_action(
                &ch("0"),
                &code("Numpad0"),
                CTRL | Modifiers::NUM_LOCK,
                false
            ),
            Some(ZoomAction::Reset)
        );
    }

    #[test]
    fn lock_bits_are_ignored() {
        let m = CTRL | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK;
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), m, false),
            Some(ZoomAction::In)
        );
    }

    #[test]
    fn cmd_on_macos_ctrl_elsewhere() {
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), CMD, true),
            Some(ZoomAction::In)
        );
        assert_eq!(
            zoom_action(&ch("+"), &code("Equal"), CMD | Modifiers::SHIFT, true),
            Some(ZoomAction::In)
        );
        assert_eq!(
            zoom_action(&ch("0"), &code("Digit0"), CMD, true),
            Some(ZoomAction::Reset)
        );
        // Ctrl on macOS stays the app's (e.g. Ctrl+_ is readline / emacs undo).
        assert_eq!(zoom_action(&ch("="), &code("Equal"), CTRL, true), None);
        // Super on Linux is the window manager's / app's.
        assert_eq!(zoom_action(&ch("="), &code("Equal"), CMD, false), None);
        // Both primary modifiers held is not a zoom on either platform.
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), CTRL | CMD, false),
            None
        );
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), CTRL | CMD, true),
            None
        );
    }

    #[test]
    fn non_bindings() {
        // Bare keys type normally.
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), Modifiers::empty(), false),
            None
        );
        assert_eq!(
            zoom_action(&ch("+"), &code("NumpadAdd"), Modifiers::empty(), false),
            None
        );
        assert_eq!(
            zoom_action(&ch("+"), &code("Equal"), Modifiers::SHIFT, false),
            None
        );
        // Alt combos are left for the app / IME.
        assert_eq!(
            zoom_action(&ch("="), &code("Equal"), CTRL | Modifiers::ALT, false),
            None
        );
        // Other keys with Ctrl.
        assert_eq!(zoom_action(&ch("c"), &code("KeyC"), CTRL, false), None);
        assert_eq!(zoom_action(&ch(")"), &code("Digit0"), CTRL, false), None);
        assert_eq!(
            zoom_action(
                &LogicalKey::Named(toastty_window::NamedKey::Insert),
                &code("Numpad0"),
                CTRL,
                false
            ),
            None
        );
        assert_eq!(
            zoom_action(
                &LogicalKey::Unidentified,
                &PhysicalKey::Unidentified,
                CTRL,
                false
            ),
            None
        );
        // Unknown physical code falls through to the logical check.
        assert_eq!(zoom_action(&ch("x"), &code("KeyX"), CTRL, false), None);
        assert_eq!(
            zoom_action(&ch("="), &PhysicalKey::Unidentified, CTRL, false),
            Some(ZoomAction::In)
        );
    }

    #[test]
    fn bounds_widen_for_out_of_range_base() {
        assert_eq!(zoom_bounds(16.0), (MIN_FONT_PX, MAX_FONT_PX));
        assert_eq!(zoom_bounds(4.0), (4.0, MAX_FONT_PX));
        assert_eq!(zoom_bounds(100.0), (MIN_FONT_PX, 100.0));
        assert_eq!(zoom_bounds(f32::NAN), (MIN_FONT_PX, MAX_FONT_PX));
        assert_eq!(zoom_bounds(f32::INFINITY), (MIN_FONT_PX, MAX_FONT_PX));
    }

    #[test]
    fn step_and_clamp() {
        assert!((zoomed_size(16.0, 16.0, ZoomAction::In) - 17.0).abs() < 1e-6);
        assert!((zoomed_size(16.0, 16.0, ZoomAction::Out) - 15.0).abs() < 1e-6);
        assert!((zoomed_size(20.0, 16.0, ZoomAction::Reset) - 16.0).abs() < 1e-6);
        assert!((zoomed_size(72.0, 16.0, ZoomAction::In) - 72.0).abs() < 1e-6);
        assert!((zoomed_size(6.0, 16.0, ZoomAction::Out) - 6.0).abs() < 1e-6);
        assert!((zoomed_size(71.5, 16.0, ZoomAction::In) - 72.0).abs() < 1e-6);
        // Fractional base keeps its fraction while stepping.
        assert!((zoomed_size(14.5, 14.5, ZoomAction::In) - 15.5).abs() < 1e-6);
    }

    #[test]
    fn out_of_range_base_never_jumps() {
        // Base 100: zoom-in is a no-op, zoom-out moves down by one.
        assert!((zoomed_size(100.0, 100.0, ZoomAction::In) - 100.0).abs() < 1e-6);
        assert!((zoomed_size(100.0, 100.0, ZoomAction::Out) - 99.0).abs() < 1e-6);
        // Base 4: zoom-out is a no-op, zoom-in moves up by one.
        assert!((zoomed_size(4.0, 4.0, ZoomAction::Out) - 4.0).abs() < 1e-6);
        assert!((zoomed_size(4.0, 4.0, ZoomAction::In) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn non_finite_current_restarts_from_base() {
        assert!((zoomed_size(f32::NAN, 16.0, ZoomAction::In) - 17.0).abs() < 1e-6);
        assert!((zoomed_size(f32::NAN, f32::NAN, ZoomAction::In) - 17.0).abs() < 1e-6);
    }

    #[test]
    fn font_zoom_apply_round_trips() {
        let mut z = FontZoom::new(16.0);
        assert!(!z.is_zoomed());
        assert!((z.base_px() - 16.0).abs() < 1e-6);
        for _ in 0..5 {
            assert!(z.apply(ZoomAction::In));
        }
        assert!((z.current_px() - 21.0).abs() < 1e-6);
        assert!(z.is_zoomed());
        for _ in 0..5 {
            assert!(z.apply(ZoomAction::Out));
        }
        assert!(!z.is_zoomed());
        assert!(z.apply(ZoomAction::Out));
        assert!(z.apply(ZoomAction::Reset));
        assert!((z.current_px() - 16.0).abs() < 1e-6);
        // Reset while unzoomed: no change, caller skips the rebuild.
        assert!(!z.apply(ZoomAction::Reset));
    }

    #[test]
    fn font_zoom_apply_reports_no_change_at_limits() {
        let mut z = FontZoom::new(16.0);
        let mut changes = 0;
        for _ in 0..200 {
            if z.apply(ZoomAction::In) {
                changes += 1;
            }
        }
        assert_eq!(changes, 56); // 16 → 72
        assert!((z.current_px() - MAX_FONT_PX).abs() < 1e-6);
        // One press back down leaves the limit immediately (no
        // accumulated overshoot).
        assert!(z.apply(ZoomAction::Out));
        assert!((z.current_px() - 71.0).abs() < 1e-6);
        let mut z = FontZoom::new(16.0);
        for _ in 0..200 {
            z.apply(ZoomAction::Out);
        }
        assert!((z.current_px() - MIN_FONT_PX).abs() < 1e-6);
        assert!(!z.apply(ZoomAction::Out));
    }

    #[test]
    fn rebase_resets_only_on_size_change() {
        let mut z = FontZoom::new(16.0);
        z.apply(ZoomAction::In);
        z.apply(ZoomAction::In);
        // Unrelated reload (same font.size_px): zoom kept.
        assert!(!z.rebase(16.0));
        assert!((z.current_px() - 18.0).abs() < 1e-6);
        // font.size_px edited: zoom resets to the new base.
        assert!(z.rebase(20.0));
        assert!((z.current_px() - 20.0).abs() < 1e-6);
        assert!((z.base_px() - 20.0).abs() < 1e-6);
        assert!(!z.is_zoomed());
    }

    #[test]
    fn nan_base_is_stable_across_reloads() {
        let mut z = FontZoom::new(f32::NAN);
        assert!(!z.is_zoomed());
        assert!(!z.rebase(f32::NAN));
        assert!(z.rebase(16.0));
    }
}
