//! Toastty's own input/window event types.
//!
//! These mirror the parts of `winit::event::*` that the rest of the codebase
//! actually consumes. Defining our own enums keeps `winit` an implementation
//! detail of this crate so the renderer / dispatcher don't need to be churned
//! every time winit ships a breaking minor release.

use std::time::Duration;

/// Whether a key or mouse button is pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

impl KeyState {
    #[must_use]
    pub fn is_pressed(self) -> bool {
        matches!(self, KeyState::Pressed)
    }
}

bitflags::bitflags! {
    /// Keyboard modifier state, expressed as a bitfield.
    ///
    /// `CAPS_LOCK` and `NUM_LOCK` are not exposed by winit's `ModifiersState`
    /// but are part of the kitty keyboard modifier mask (bits 6 and 7).
    /// The wrapper reserves the bits now; reading platform LED state
    /// is `TODO(kitty-keyboard)` and lives in this file.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Modifiers: u32 {
        const SHIFT     = 1 << 0;
        const CONTROL   = 1 << 1;
        const ALT       = 1 << 2;
        const SUPER     = 1 << 3;
        // TODO(kitty-keyboard): populate these by reading LED state per
        // platform. See `docs/decisions/window-input.md` §1.
        const CAPS_LOCK = 1 << 4;
        const NUM_LOCK  = 1 << 5;
    }
}

/// Mouse buttons we surface. Matches `winit::event::MouseButton` 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u16),
}

/// Logical key — what character / named key the user effectively pressed,
/// taking into account the active keyboard layout (but **not** dead-key
/// composition; that arrives via IME, not here).
///
/// We only surface enough to drive the kitty keyboard handler that comes
/// later; we deliberately avoid leaking `winit::keyboard::Key` so the
/// downstream code doesn't depend on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalKey {
    /// A typeable character (already cooked for layout). For terminals,
    /// `text` is usually the useful field; this is here for keybinds.
    Character(String),
    /// A named non-character key (Enter, Escape, F1, arrow keys, …).
    Named(NamedKey),
    /// A key we don't classify (uncommon).
    Unidentified,
}

/// Subset of named keys terminals care about. Add to this as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Escape,
    Backspace,
    Tab,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    Other,
}

/// Physical key — a layout-independent position on the keyboard. Useful
/// for game-style bindings and for the kitty protocol's report-alternate-key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalKey {
    /// W3C `KeyCode` string, e.g. `"KeyA"`, `"Digit1"`, `"Enter"`.
    Code(String),
    Unidentified,
}

/// Whether a scroll event originated from a discrete wheel notch
/// (`Lines`) or a continuous pixel stream (`Pixels`). The terminal
/// uses this to choose between animated jumps (for notches) and
/// direct pixel-accurate scrolling (for trackpad inertia).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollKind {
    /// Notched wheel — the delta is in "lines" (typically ±1 per
    /// notch). The terminal multiplies this by the configured
    /// `lines_per_notch` and animates between positions.
    Lines,
    /// Trackpad / high-resolution wheel — the delta is in physical
    /// pixels (often fractional, including inertial decay frames
    /// from macOS). The terminal feeds these straight into the
    /// viewport target without further animation.
    Pixels,
}

/// One window event consumable by the binary / renderer.
#[derive(Debug, Clone)]
pub enum Event {
    /// A key was pressed or released. `text` is what should be written to
    /// the PTY — comes from `KeyEventExtModifierSupplement::text_with_all_modifiers`
    /// per decision #2, so `Ctrl+A` arrives as `"\x01"`, not `"a"`.
    Key {
        logical: LogicalKey,
        physical: PhysicalKey,
        text: Option<String>,
        modifiers: Modifiers,
        state: KeyState,
        repeat: bool,
        is_synthetic: bool,
    },
    /// A mouse button was pressed or released.
    Mouse {
        button: MouseButton,
        state: KeyState,
        position: (f64, f64),
        modifiers: Modifiers,
    },
    /// The cursor moved over the window. Dispatched on every winit
    /// `CursorMoved` — the app is expected to filter at cell granularity
    /// and against the active DECSET 1000/1002/1003 mouse mode.
    MouseMotion {
        position: (f64, f64),
        modifiers: Modifiers,
    },
    /// Scroll wheel / trackpad scroll. `kind` distinguishes a discrete
    /// notch (`Lines`) from a continuous pixel stream (`Pixels`, the
    /// usual macOS trackpad path including inertial momentum frames).
    /// Sign convention matches winit: positive y == content moves down.
    Scroll {
        kind: ScrollKind,
        delta_x: f64,
        delta_y: f64,
    },
    /// Window resized — `width`/`height` are physical pixels.
    Resize {
        width: u32,
        height: u32,
        scale_factor: f64,
    },
    /// Focus gained (`true`) or lost (`false`).
    Focus(bool),
    /// The window should redraw.
    Redraw,
    /// Window close requested (user clicked the close button, etc).
    Close,
    /// A user-event posted via `WindowHandle::wake`. Used by the mio PTY
    /// thread to nudge the event loop.
    User,
    /// PTY bytes arrived on the master fd (delivered via
    /// `toastty_io::spawn_pty_reader`). The binary feeds these to the
    /// parser and requests a redraw.
    PtyBytes(Vec<u8>),
    /// PTY master closed (child exited / EIO). The binary should reap
    /// and exit.
    PtyClosed,
}

/// What `run` should do after the app callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlSignal {
    /// Wait for the next input/IO event indefinitely (power-friendly).
    Continue,
    /// Exit the event loop cleanly.
    Exit,
    /// Schedule a redraw at most `Duration` from now.
    RedrawIn(Duration),
}
